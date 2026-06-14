//! S281 / PR-266 — `outbound_email_queue` table + state machine.
//!
//! Persists each accepted `POST /api/internal/send-email` request as a
//! row in `Queued` state; the background daemon
//! ([`crate::email_relay_daemon`]) walks `Queued → Sending → Sent` (or
//! `→ Failed` after exhausting retries).
//!
//! ## State machine
//!
//! ```text
//!   Queued ──► Sending ──► Sent
//!                  │
//!                  ▼
//!               Failed   (after retry-cap exhaustion; operator can
//!                         hand-retry from the SPA in a future PR)
//! ```
//!
//! Per [[no-sql-specific]] the state is **app-layer enforced** — no
//! DuckDB `CHECK` constraints, no `DEFAULT` clauses (the
//! `ADD COLUMN IF NOT EXISTS ... DEFAULT` gotcha from S271 / S273 /
//! S279 still bites here). Every column is nullable at the DDL layer;
//! the writer paths enforce required-vs-optional.
//!
//! ## Why DuckDB + on-disk attachment files
//!
//! The brief's pushback #2: DuckDB BLOB columns degrade perf with
//! large binary data. Attachments live under
//! `~/.aberp/<tenant>/email-relay-attachments/<row_id>/<n>_<safe_name>`
//! and the row stores a single `attachments_dir` rel-path. Body text +
//! HTML stay in VARCHAR columns (small; the 25 MB cap is overall body
//! + attachments; the text/html share is typically <100 KB).

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use duckdb::{params, Connection};
use time::OffsetDateTime;

/// Wire storage strings for the closed-vocab `state` column.
///
/// Round-trip-proven against [`QueueState::parse_str`].
pub const STATE_QUEUED: &str = "queued";
pub const STATE_SENDING: &str = "sending";
pub const STATE_SENT: &str = "sent";
pub const STATE_FAILED: &str = "failed";

/// Closed-vocab queue state. Wire / on-disk form is the lowercase
/// token above; `parse_str` errors loud on unknown values per CLAUDE.md
/// rule 12.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueState {
    /// Just accepted — body persisted, attachments on disk, awaiting
    /// the daemon.
    Queued,
    /// Daemon has CAS-claimed the row and is mid-SMTP-send.
    Sending,
    /// Terminal success.
    Sent,
    /// Terminal failure after retry-cap exhaustion.
    Failed,
}

impl QueueState {
    pub fn as_str(self) -> &'static str {
        match self {
            QueueState::Queued => STATE_QUEUED,
            QueueState::Sending => STATE_SENDING,
            QueueState::Sent => STATE_SENT,
            QueueState::Failed => STATE_FAILED,
        }
    }

    /// Round-trip parse. Errors loud on unknown — silent-fallback
    /// would mask schema drift.
    pub fn parse_str(s: &str) -> Result<Self> {
        match s {
            STATE_QUEUED => Ok(QueueState::Queued),
            STATE_SENDING => Ok(QueueState::Sending),
            STATE_SENT => Ok(QueueState::Sent),
            STATE_FAILED => Ok(QueueState::Failed),
            other => Err(anyhow!("unknown outbound_email_queue.state: {other:?}")),
        }
    }
}

/// One row of `outbound_email_queue` — read-only projection used by
/// the SPA list route and the daemon's drain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundEmailRow {
    /// UUID, minted at insert time; surfaces as the 200 `audit_id`.
    pub id: String,
    /// RFC-3339 timestamp of the queueing call.
    pub created_at: String,
    pub submitter: String,
    /// JSON array of `to`-addresses (canonical-form `["a@b","c@d"]`).
    pub to_recipients_json: String,
    /// Optional JSON array of `cc`-addresses. `None` when the request
    /// carried no `cc` field; `Some("[]")` when the field was present
    /// but empty (the daemon treats both as no-CC).
    pub cc_recipients_json: Option<String>,
    pub subject: String,
    pub body_text: String,
    pub body_html: Option<String>,
    /// Rel-path under the tenant attachment root; `None` when no
    /// attachments rode the request.
    pub attachments_dir: Option<String>,
    pub state: QueueState,
    /// 0 before the first send attempt; incremented at the start of
    /// each daemon-claimed send.
    pub attempt_n: u32,
    /// Operator-readable last-error string (already scrubbed of
    /// secrets at the writer). `None` for rows that never failed.
    pub last_error: Option<String>,
    /// RFC-3339 timestamp of the successful `Sent` transition.
    pub sent_at: Option<String>,
    /// SHA-256 (hex) of the canonicalised recipient list (lower-case
    /// comma-joined, byte-sort order). Stable across retries; threads
    /// the operational row to its audit lineage without exposing
    /// plaintext recipients in the chain.
    pub recipient_hash: String,
    /// Rendered byte size (text + html + attachments after b64 decode)
    /// — surfaced in the SPA list + the audit payload.
    pub byte_size: u64,
}

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS outbound_email_queue (
    id                     VARCHAR NOT NULL PRIMARY KEY,
    created_at             VARCHAR NOT NULL,
    submitter              VARCHAR NOT NULL,
    to_recipients_json     VARCHAR NOT NULL,
    cc_recipients_json     VARCHAR,
    subject                VARCHAR NOT NULL,
    body_text              VARCHAR NOT NULL,
    body_html              VARCHAR,
    attachments_dir        VARCHAR,
    state                  VARCHAR NOT NULL,
    attempt_n              INTEGER NOT NULL,
    last_error             VARCHAR,
    sent_at                VARCHAR,
    recipient_hash         VARCHAR NOT NULL,
    byte_size              BIGINT NOT NULL
);
-- S409 — drop the two secondary indexes. `outbound_email_queue_state_idx`
-- ON (state) is the PROD bug trigger: `state` is UPDATEd on EVERY
-- transition (Queued -> Sending -> Sent/Failed) and DuckDB's ART
-- secondary-index maintenance on an UPDATEd column, over a table that
-- also carries a PRIMARY KEY, can fire `Invalid Input Error: Failed to
-- delete all rows from index. Only deleted 0 out of 1 rows.` mid-
-- `mark_sent` (observed in PROD 2026-06-14 after the S403 refuse feature
-- raised relay volume). Same DuckDB issue class S286/PR-268 fixed on
-- `quote_pricing_jobs`. `outbound_email_queue_submitter_idx` ON
-- (submitter) is unused — nothing filters / sorts by submitter — so it
-- is pure dead weight (rule 13). Both drops are subtractive: the queue
-- drains to near-empty continuously and every read is a tiny result set,
-- so a full scan of a sub-1k-row table costs microseconds. One-time
-- idempotent migration for installs carrying the indexes from
-- PROD_v2.27.* (<= .61); `DROP INDEX IF EXISTS` no-ops on fresh DBs and
-- on re-runs.
DROP INDEX IF EXISTS outbound_email_queue_state_idx;
DROP INDEX IF EXISTS outbound_email_queue_submitter_idx;
";

/// Idempotent — call at every writer / reader entry.
///
/// **S409 caveat**: existing prod DBs from PROD_v2.27.* (<= .61) carry the
/// orphan `outbound_email_queue_state_idx` (and dead
/// `outbound_email_queue_submitter_idx`) secondary indexes. The embedded
/// `DROP INDEX IF EXISTS` in `SCHEMA_SQL` removes them on the first call
/// after upgrade — the daemon's startup is the first call.
pub fn ensure_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA_SQL)
        .with_context(|| "ensure outbound_email_queue schema")
}

/// Insert a freshly-accepted relay request. `state` is always
/// [`QueueState::Queued`] at insert time. `attempt_n` is 0 (the daemon
/// increments to 1 on the first attempt).
#[allow(clippy::too_many_arguments)]
pub fn insert_queued(
    conn: &Connection,
    id: &str,
    submitter: &str,
    to_recipients_json: &str,
    cc_recipients_json: Option<&str>,
    subject: &str,
    body_text: &str,
    body_html: Option<&str>,
    attachments_dir: Option<&str>,
    recipient_hash: &str,
    byte_size: u64,
    now: OffsetDateTime,
) -> Result<()> {
    ensure_schema(conn)?;
    let created_at = now
        .format(&time::format_description::well_known::Rfc3339)
        .context("format created_at")?;
    conn.execute(
        "INSERT INTO outbound_email_queue (
            id, created_at, submitter,
            to_recipients_json, cc_recipients_json,
            subject, body_text, body_html,
            attachments_dir,
            state, attempt_n, last_error, sent_at,
            recipient_hash, byte_size
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, NULL, ?, ?)",
        params![
            id,
            created_at,
            submitter,
            to_recipients_json,
            cc_recipients_json,
            subject,
            body_text,
            body_html,
            attachments_dir,
            STATE_QUEUED,
            0_i64,
            recipient_hash,
            byte_size as i64,
        ],
    )
    .context("insert outbound_email_queue row")?;
    Ok(())
}

/// Atomic CAS claim: move a `Queued` (or retryable `Sending` that's
/// stale — though we don't expect any in a single-process deployment)
/// row to `Sending` and bump `attempt_n`. Returns `Ok(Some(row))` on
/// claim, `Ok(None)` when the row was already claimed / completed.
///
/// In v1 we claim the OLDEST Queued row per call (FIFO drain). Per
/// [[no-sql-specific]] no `FOR UPDATE` / DB locking — the single-
/// daemon-process deployment makes the read-then-write race trivially
/// impossible (the daemon awaits the previous tick before issuing the
/// next claim).
pub fn claim_next_queued(
    conn: &Connection,
    now: OffsetDateTime,
) -> Result<Option<OutboundEmailRow>> {
    ensure_schema(conn)?;
    let row = match read_oldest_queued(conn)? {
        Some(r) => r,
        None => return Ok(None),
    };
    let _ = now; // reserved for future "claimed_at" surface
    let new_attempt = row.attempt_n + 1;
    let n = conn
        .execute(
            "UPDATE outbound_email_queue
             SET state = ?, attempt_n = ?
             WHERE id = ? AND state = ?",
            params![STATE_SENDING, new_attempt as i64, row.id, STATE_QUEUED,],
        )
        .context("CAS claim Queued -> Sending")?;
    if n == 0 {
        return Ok(None);
    }
    Ok(Some(OutboundEmailRow {
        state: QueueState::Sending,
        attempt_n: new_attempt,
        ..row
    }))
}

/// Move a `Sending` row to `Sent`. Stamps `sent_at`.
pub fn mark_sent(conn: &Connection, id: &str, now: OffsetDateTime) -> Result<()> {
    ensure_schema(conn)?;
    let sent_at = now
        .format(&time::format_description::well_known::Rfc3339)
        .context("format sent_at")?;
    let n = conn
        .execute(
            "UPDATE outbound_email_queue
             SET state = ?, sent_at = ?, last_error = NULL
             WHERE id = ? AND state = ?",
            params![STATE_SENT, sent_at, id, STATE_SENDING],
        )
        .context("UPDATE Sending -> Sent")?;
    if n == 0 {
        return Err(anyhow!(
            "outbound_email_queue row {id} not in Sending state; refusing to flip to Sent"
        ));
    }
    Ok(())
}

/// Move a `Sending` row to `Failed` (terminal — retries exhausted).
/// Carries the scrubbed-of-secrets `last_error`.
pub fn mark_failed(conn: &Connection, id: &str, last_error: &str) -> Result<()> {
    ensure_schema(conn)?;
    let n = conn
        .execute(
            "UPDATE outbound_email_queue
             SET state = ?, last_error = ?
             WHERE id = ? AND state = ?",
            params![STATE_FAILED, last_error, id, STATE_SENDING],
        )
        .context("UPDATE Sending -> Failed")?;
    if n == 0 {
        return Err(anyhow!(
            "outbound_email_queue row {id} not in Sending state; refusing to flip to Failed"
        ));
    }
    Ok(())
}

/// Move a `Sending` row back to `Queued` for a retry. Stamps the
/// transient `last_error` so the operator can see the most recent
/// failure mid-flight (the chain still emits the terminal
/// `email.relay_failed` only after retry-cap exhaustion).
pub fn requeue_for_retry(conn: &Connection, id: &str, last_error: &str) -> Result<()> {
    ensure_schema(conn)?;
    let n = conn
        .execute(
            "UPDATE outbound_email_queue
             SET state = ?, last_error = ?
             WHERE id = ? AND state = ?",
            params![STATE_QUEUED, last_error, id, STATE_SENDING],
        )
        .context("UPDATE Sending -> Queued (retry)")?;
    if n == 0 {
        return Err(anyhow!(
            "outbound_email_queue row {id} not in Sending state; refusing to requeue"
        ));
    }
    Ok(())
}

/// S409 — operator-visible note stamped into `last_error` when a row is
/// reconciled out of an orphaned `Sending` state at daemon startup.
pub const RECONCILE_NOTE: &str =
    "reconciled at daemon startup: orphaned in Sending after an incomplete state \
     transition; SMTP send had already been attempted — marked Sent WITHOUT \
     re-sending to avoid duplicate delivery (S409)";

/// S409 — reconcile rows orphaned in `Sending`.
///
/// At daemon startup the drain loop has claimed nothing yet, so any row
/// still in `Sending` is the residue of a PRIOR process whose terminal
/// transition (`mark_sent` / `mark_failed`) did not complete — e.g. the
/// DuckDB secondary-index `UPDATE` failure this PR removes. The SMTP send
/// for such a row had ALREADY been attempted by the daemon's `send_one`,
/// so we walk it forward to terminal `Sent` and NEVER back to `Queued`:
/// re-queuing would re-send, and the customer must never receive a
/// duplicate ([[hulye-biztos]]). The reconcile note is stamped into
/// `last_error` so the state is operator-visible ([[fail-loud]]). Returns
/// the count of rows reconciled so the caller can log loudly.
///
/// Idempotent: a second call (no rows in `Sending`) returns `Ok(0)`.
pub fn reconcile_orphaned_sending(conn: &Connection, now: OffsetDateTime) -> Result<u64> {
    ensure_schema(conn)?;
    let sent_at = now
        .format(&time::format_description::well_known::Rfc3339)
        .context("format reconcile sent_at")?;
    let n = conn
        .execute(
            "UPDATE outbound_email_queue
             SET state = ?, sent_at = COALESCE(sent_at, ?), last_error = ?
             WHERE state = ?",
            params![STATE_SENT, sent_at, RECONCILE_NOTE, STATE_SENDING],
        )
        .context("reconcile orphaned Sending rows")?;
    Ok(n as u64)
}

/// Read the oldest `Queued` row by `created_at` ascending. The daemon
/// uses this to pick the FIFO next-to-send.
fn read_oldest_queued(conn: &Connection) -> Result<Option<OutboundEmailRow>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, created_at, submitter,
                    to_recipients_json, cc_recipients_json,
                    subject, body_text, body_html,
                    attachments_dir,
                    state, attempt_n, last_error, sent_at,
                    recipient_hash, byte_size
             FROM outbound_email_queue
             WHERE state = ?
             ORDER BY created_at ASC
             LIMIT 1",
        )
        .context("prepare read_oldest_queued")?;
    let mut rows = stmt
        .query(params![STATE_QUEUED])
        .context("query read_oldest_queued")?;
    if let Some(r) = rows.next().context("step read_oldest_queued")? {
        Ok(Some(decode_row(r)?))
    } else {
        Ok(None)
    }
}

/// List queue rows by optional state filter, capped at `limit`. Newest
/// first (the SPA list inspector sorts by `created_at` DESC). When
/// `state` is `None` all states are returned.
pub fn list_rows(
    conn: &Connection,
    state: Option<QueueState>,
    limit: usize,
) -> Result<Vec<OutboundEmailRow>> {
    ensure_schema(conn)?;
    let limit_i64 = limit as i64;
    let mut out = Vec::new();
    if let Some(s) = state {
        let mut stmt = conn
            .prepare(
                "SELECT id, created_at, submitter,
                        to_recipients_json, cc_recipients_json,
                        subject, body_text, body_html,
                        attachments_dir,
                        state, attempt_n, last_error, sent_at,
                        recipient_hash, byte_size
                 FROM outbound_email_queue
                 WHERE state = ?
                 ORDER BY created_at DESC
                 LIMIT ?",
            )
            .context("prepare list_rows (filtered)")?;
        let mut rows = stmt
            .query(params![s.as_str(), limit_i64])
            .context("query list_rows (filtered)")?;
        while let Some(r) = rows.next().context("step list_rows (filtered)")? {
            out.push(decode_row(r)?);
        }
    } else {
        let mut stmt = conn
            .prepare(
                "SELECT id, created_at, submitter,
                        to_recipients_json, cc_recipients_json,
                        subject, body_text, body_html,
                        attachments_dir,
                        state, attempt_n, last_error, sent_at,
                        recipient_hash, byte_size
                 FROM outbound_email_queue
                 ORDER BY created_at DESC
                 LIMIT ?",
            )
            .context("prepare list_rows (all)")?;
        let mut rows = stmt
            .query(params![limit_i64])
            .context("query list_rows (all)")?;
        while let Some(r) = rows.next().context("step list_rows (all)")? {
            out.push(decode_row(r)?);
        }
    }
    Ok(out)
}

/// Read one row by id. Returns `None` when the row doesn't exist.
pub fn read_row(conn: &Connection, id: &str) -> Result<Option<OutboundEmailRow>> {
    ensure_schema(conn)?;
    let mut stmt = conn
        .prepare(
            "SELECT id, created_at, submitter,
                    to_recipients_json, cc_recipients_json,
                    subject, body_text, body_html,
                    attachments_dir,
                    state, attempt_n, last_error, sent_at,
                    recipient_hash, byte_size
             FROM outbound_email_queue
             WHERE id = ?
             LIMIT 1",
        )
        .context("prepare read_row")?;
    let mut rows = stmt.query(params![id]).context("query read_row")?;
    if let Some(r) = rows.next().context("step read_row")? {
        Ok(Some(decode_row(r)?))
    } else {
        Ok(None)
    }
}

/// Decode a row from a DuckDB cursor.
fn decode_row(row: &duckdb::Row<'_>) -> Result<OutboundEmailRow> {
    let id: String = row.get(0).context("col id")?;
    let created_at: String = row.get(1).context("col created_at")?;
    let submitter: String = row.get(2).context("col submitter")?;
    let to_recipients_json: String = row.get(3).context("col to_recipients_json")?;
    let cc_recipients_json: Option<String> = row.get(4).context("col cc_recipients_json")?;
    let subject: String = row.get(5).context("col subject")?;
    let body_text: String = row.get(6).context("col body_text")?;
    let body_html: Option<String> = row.get(7).context("col body_html")?;
    let attachments_dir: Option<String> = row.get(8).context("col attachments_dir")?;
    let state_str: String = row.get(9).context("col state")?;
    let state = QueueState::parse_str(&state_str)?;
    let attempt_n_i64: i64 = row.get(10).context("col attempt_n")?;
    let attempt_n = if attempt_n_i64 < 0 {
        0
    } else {
        attempt_n_i64 as u32
    };
    let last_error: Option<String> = row.get(11).context("col last_error")?;
    let sent_at: Option<String> = row.get(12).context("col sent_at")?;
    let recipient_hash: String = row.get(13).context("col recipient_hash")?;
    let byte_size_i64: i64 = row.get(14).context("col byte_size")?;
    let byte_size = if byte_size_i64 < 0 {
        0
    } else {
        byte_size_i64 as u64
    };
    Ok(OutboundEmailRow {
        id,
        created_at,
        submitter,
        to_recipients_json,
        cc_recipients_json,
        subject,
        body_text,
        body_html,
        attachments_dir,
        state,
        attempt_n,
        last_error,
        sent_at,
        recipient_hash,
        byte_size,
    })
}

/// Compose the per-tenant attachments root.
/// `~/.aberp/<tenant>/email-relay-attachments/`. Mirrors the
/// `ap-artifacts/` layout from S197.
pub fn attachments_root_for_tenant(tenant: &str) -> Result<std::path::PathBuf> {
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| anyhow!("HOME env var not set"))?;
    Ok(home
        .join(".aberp")
        .join(tenant)
        .join("email-relay-attachments"))
}

/// Sanitize an operator-typed attachment filename for safe on-disk
/// storage. Mirror of
/// [`crate::email_invoice::sanitize_invoice_number_for_filename`]: keep
/// ASCII alphanumeric + `-` + `_` + `.`, replace everything else with
/// `_`. Eliminates path-traversal (`../`), NUL, and Unicode-shenanigans
/// risk. Caps the output at 128 chars to bound disk usage.
pub fn sanitize_attachment_filename(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        out.push_str("attachment.bin");
    }
    if out.len() > 128 {
        out.truncate(128);
    }
    // Reject pure-dot strings (`.`, `..`, `...`) — they collide with
    // path traversal artefacts even after our char-class filter
    // (since `.` is allowed).
    if out.chars().all(|c| c == '.') {
        out = format!("attachment_{}.bin", out.len());
    }
    out
}

/// Helper used by the route handler to write one attachment to disk.
/// Returns the **basename** that was actually written under
/// `<row_dir>/<index>_<safe_name>`. The row's `attachments_dir` column
/// stores the relative directory only — the index disambiguates two
/// attachments with the same operator-supplied name.
pub fn write_attachment(
    row_dir: &Path,
    index: usize,
    operator_filename: &str,
    bytes: &[u8],
) -> Result<String> {
    std::fs::create_dir_all(row_dir)
        .with_context(|| format!("create_dir_all {}", row_dir.display()))?;
    let safe = sanitize_attachment_filename(operator_filename);
    let basename = format!("{index:02}_{safe}");
    let full_path = row_dir.join(&basename);
    std::fs::write(&full_path, bytes)
        .with_context(|| format!("write attachment to {}", full_path.display()))?;
    Ok(basename)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_in_memory() -> Connection {
        let conn = Connection::open_in_memory().expect("DuckDB in-memory open");
        ensure_schema(&conn).expect("schema");
        conn
    }

    #[test]
    fn queue_state_round_trips() {
        for s in [
            QueueState::Queued,
            QueueState::Sending,
            QueueState::Sent,
            QueueState::Failed,
        ] {
            let token = s.as_str();
            let parsed = QueueState::parse_str(token).expect("parse");
            assert_eq!(parsed, s);
        }
    }

    #[test]
    fn queue_state_rejects_unknown() {
        assert!(QueueState::parse_str("Queued").is_err()); // case-sensitive
        assert!(QueueState::parse_str("").is_err());
        assert!(QueueState::parse_str("retrying").is_err()); // not a vocab member
    }

    #[test]
    fn insert_then_read_round_trips() {
        let conn = open_in_memory();
        let now = OffsetDateTime::now_utc();
        insert_queued(
            &conn,
            "row-1",
            "storefront",
            "[\"a@b.c\"]",
            None,
            "Subject",
            "Body",
            Some("<p>html</p>"),
            Some("row-1"),
            "0123abc",
            123,
            now,
        )
        .expect("insert");
        let r = read_row(&conn, "row-1").expect("read").expect("Some");
        assert_eq!(r.id, "row-1");
        assert_eq!(r.submitter, "storefront");
        assert_eq!(r.subject, "Subject");
        assert_eq!(r.body_text, "Body");
        assert_eq!(r.body_html.as_deref(), Some("<p>html</p>"));
        assert_eq!(r.state, QueueState::Queued);
        assert_eq!(r.attempt_n, 0);
        assert_eq!(r.byte_size, 123);
        assert_eq!(r.recipient_hash, "0123abc");
    }

    #[test]
    fn claim_then_mark_sent_walks_state() {
        let conn = open_in_memory();
        let now = OffsetDateTime::now_utc();
        insert_queued(
            &conn,
            "row-1",
            "storefront",
            "[\"a@b.c\"]",
            None,
            "S",
            "B",
            None,
            None,
            "h",
            1,
            now,
        )
        .expect("insert");
        let claimed = claim_next_queued(&conn, now).expect("claim").expect("Some");
        assert_eq!(claimed.id, "row-1");
        assert_eq!(claimed.state, QueueState::Sending);
        assert_eq!(claimed.attempt_n, 1);
        mark_sent(&conn, "row-1", now).expect("sent");
        let r = read_row(&conn, "row-1").expect("read").expect("Some");
        assert_eq!(r.state, QueueState::Sent);
        assert!(r.sent_at.is_some());
        assert!(r.last_error.is_none());
    }

    #[test]
    fn claim_returns_none_when_no_queued() {
        let conn = open_in_memory();
        let now = OffsetDateTime::now_utc();
        assert!(claim_next_queued(&conn, now).expect("claim").is_none());
    }

    #[test]
    fn claim_does_not_reclaim_sending_row() {
        // CAS: claim moves Queued -> Sending; a second claim must NOT
        // re-flip the same row (single-process invariant, but pin so a
        // future writer can't accidentally re-grab).
        let conn = open_in_memory();
        let now = OffsetDateTime::now_utc();
        insert_queued(
            &conn,
            "r1",
            "storefront",
            "[\"a@b.c\"]",
            None,
            "S",
            "B",
            None,
            None,
            "h",
            1,
            now,
        )
        .unwrap();
        let _first = claim_next_queued(&conn, now).unwrap().unwrap();
        let second = claim_next_queued(&conn, now).unwrap();
        assert!(second.is_none(), "must not re-claim a Sending row");
    }

    #[test]
    fn mark_failed_requires_sending_state() {
        let conn = open_in_memory();
        let now = OffsetDateTime::now_utc();
        insert_queued(
            &conn,
            "r1",
            "storefront",
            "[\"a@b.c\"]",
            None,
            "S",
            "B",
            None,
            None,
            "h",
            1,
            now,
        )
        .unwrap();
        // Cannot fail a Queued row (must claim first).
        assert!(mark_failed(&conn, "r1", "boom").is_err());
        // Cannot fail a non-existent row.
        assert!(mark_failed(&conn, "ghost", "boom").is_err());
    }

    #[test]
    fn requeue_walks_back_to_queued() {
        let conn = open_in_memory();
        let now = OffsetDateTime::now_utc();
        insert_queued(
            &conn,
            "r1",
            "storefront",
            "[\"a@b.c\"]",
            None,
            "S",
            "B",
            None,
            None,
            "h",
            1,
            now,
        )
        .unwrap();
        let claimed = claim_next_queued(&conn, now).unwrap().unwrap();
        assert_eq!(claimed.state, QueueState::Sending);
        requeue_for_retry(&conn, "r1", "transient flake").unwrap();
        let r = read_row(&conn, "r1").unwrap().unwrap();
        assert_eq!(r.state, QueueState::Queued);
        assert_eq!(r.last_error.as_deref(), Some("transient flake"));
        // The attempt counter is preserved through requeue — it
        // increments on the NEXT claim, capping the retry budget.
        assert_eq!(r.attempt_n, 1);
    }

    #[test]
    fn list_rows_filter_by_state() {
        let conn = open_in_memory();
        let now = OffsetDateTime::now_utc();
        insert_queued(
            &conn,
            "a",
            "storefront",
            "[\"a@x\"]",
            None,
            "S",
            "B",
            None,
            None,
            "h",
            1,
            now,
        )
        .unwrap();
        insert_queued(
            &conn,
            "b",
            "storefront",
            "[\"b@x\"]",
            None,
            "S",
            "B",
            None,
            None,
            "h",
            1,
            now,
        )
        .unwrap();
        let _ = claim_next_queued(&conn, now).unwrap().unwrap(); // 'a' -> Sending
        mark_sent(&conn, "a", now).unwrap();
        let queued = list_rows(&conn, Some(QueueState::Queued), 100).unwrap();
        let sent = list_rows(&conn, Some(QueueState::Sent), 100).unwrap();
        let all = list_rows(&conn, None, 100).unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].id, "b");
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].id, "a");
        assert_eq!(all.len(), 2);
    }

    /// S409 — count user indexes by name via DuckDB's `duckdb_indexes()`
    /// system table function (same probe `quote_pricing_jobs` uses).
    fn index_count(conn: &Connection, name: &str) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM duckdb_indexes() WHERE index_name = ?",
            params![name],
            |r| r.get(0),
        )
        .expect("query duckdb_indexes")
    }

    /// S409 — a fresh DB carries NEITHER secondary index (the bug-trigger
    /// `state` index nor the dead `submitter` index): `SCHEMA_SQL` no
    /// longer creates them, and the embedded `DROP INDEX IF EXISTS` no-ops.
    #[test]
    fn s409_fresh_db_has_no_secondary_indexes() {
        let conn = open_in_memory();
        assert_eq!(index_count(&conn, "outbound_email_queue_state_idx"), 0);
        assert_eq!(index_count(&conn, "outbound_email_queue_submitter_idx"), 0);
    }

    /// S409 — an installed DB that PRE-EXISTINGLY carries the secondary
    /// indexes from PROD_v2.27.* (<= .61) is migrated clean: hand-create
    /// the indexes to simulate the prod schema, then `ensure_schema`
    /// drops them. This is the load-bearing regression — without the
    /// `DROP INDEX IF EXISTS`, the next `mark_sent` UPDATE on `state`
    /// would FATAL with "Failed to delete all rows from index".
    #[test]
    fn s409_drop_migrates_from_prior_schema() {
        let conn = open_in_memory();
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS outbound_email_queue_state_idx
                 ON outbound_email_queue (state);
             CREATE INDEX IF NOT EXISTS outbound_email_queue_submitter_idx
                 ON outbound_email_queue (submitter);",
        )
        .expect("simulate prior-version indexes");
        assert_eq!(index_count(&conn, "outbound_email_queue_state_idx"), 1);
        // Re-running ensure_schema must DROP both.
        ensure_schema(&conn).expect("post-migration ensure");
        assert_eq!(index_count(&conn, "outbound_email_queue_state_idx"), 0);
        assert_eq!(index_count(&conn, "outbound_email_queue_submitter_idx"), 0);
    }

    /// S409 — the schema migration is idempotent: repeated `ensure_schema`
    /// on a fresh DB and on an already-migrated DB both succeed.
    #[test]
    fn s409_ensure_schema_idempotent() {
        let conn = Connection::open_in_memory().expect("open");
        ensure_schema(&conn).expect("first");
        ensure_schema(&conn).expect("second");
        ensure_schema(&conn).expect("third");
    }

    /// S409 — the PROD scenario: a row claimed to `Sending` whose
    /// `mark_sent` never landed (orphaned by the index bug) is walked
    /// forward to `Sent` at startup — NOT re-queued. `sent_at` is stamped
    /// and the operator-visible reconcile note lands in `last_error`.
    #[test]
    fn s409_reconcile_walks_orphaned_sending_to_sent() {
        let conn = open_in_memory();
        let now = OffsetDateTime::now_utc();
        insert_queued(
            &conn,
            "r1",
            "storefront",
            "[\"a@b.c\"]",
            None,
            "S",
            "B",
            None,
            None,
            "h",
            1,
            now,
        )
        .unwrap();
        let claimed = claim_next_queued(&conn, now).unwrap().unwrap();
        assert_eq!(claimed.state, QueueState::Sending);
        // Simulate the bug: SMTP succeeded but mark_sent never ran.
        let n = reconcile_orphaned_sending(&conn, now).expect("reconcile");
        assert_eq!(n, 1, "the one orphaned Sending row is reconciled");
        let r = read_row(&conn, "r1").unwrap().unwrap();
        assert_eq!(r.state, QueueState::Sent, "walked forward, not back");
        assert!(r.sent_at.is_some(), "sent_at stamped");
        assert_eq!(r.last_error.as_deref(), Some(RECONCILE_NOTE));
    }

    /// S409 — [[hulye-biztos]] the load-bearing safety pin: after
    /// reconcile, the row is `Sent`, so `claim_next_queued` returns
    /// `None` — there is NO path that re-sends it. If a future refactor
    /// reconciled a Sending row back to `Queued`, this test fails (the
    /// claim would hand the row back to the SMTP send → duplicate email).
    #[test]
    fn s409_reconcile_never_resends() {
        let conn = open_in_memory();
        let now = OffsetDateTime::now_utc();
        insert_queued(
            &conn,
            "r1",
            "storefront",
            "[\"a@b.c\"]",
            None,
            "S",
            "B",
            None,
            None,
            "h",
            1,
            now,
        )
        .unwrap();
        claim_next_queued(&conn, now).unwrap().unwrap(); // -> Sending
        reconcile_orphaned_sending(&conn, now).unwrap();
        assert!(
            claim_next_queued(&conn, now).unwrap().is_none(),
            "a reconciled row must never be re-claimable for sending"
        );
    }

    /// S409 — reconcile is surgical: it touches ONLY `Sending` rows. A
    /// `Queued` row (not yet attempted) must be left for the drain, an
    /// already-`Sent` row untouched, a `Failed` row untouched. Intent
    /// pin (rule 9): a reconcile that grabbed `Queued` would re-send
    /// nothing yet but would skip the real SMTP attempt — silent loss.
    #[test]
    fn s409_reconcile_ignores_non_sending_states() {
        let conn = open_in_memory();
        let now = OffsetDateTime::now_utc();
        // q stays Queued. s -> Sent. f -> Failed. Drive s/f deterministically
        // by id (a raw setup UPDATE to Sending, then the public terminal
        // transition) rather than via claim, whose oldest-first order is
        // unspecified when created_at ties.
        for id in ["q", "s", "f"] {
            insert_queued(
                &conn,
                id,
                "storefront",
                "[\"a@x\"]",
                None,
                "S",
                "B",
                None,
                None,
                "h",
                1,
                now,
            )
            .unwrap();
        }
        for id in ["s", "f"] {
            conn.execute(
                "UPDATE outbound_email_queue SET state = ? WHERE id = ?",
                params![STATE_SENDING, id],
            )
            .unwrap();
        }
        mark_sent(&conn, "s", now).unwrap();
        mark_failed(&conn, "f", "boom").unwrap();

        let n = reconcile_orphaned_sending(&conn, now).expect("reconcile");
        assert_eq!(n, 0, "no Sending rows present -> nothing reconciled");
        assert_eq!(
            read_row(&conn, "q").unwrap().unwrap().state,
            QueueState::Queued
        );
        assert_eq!(
            read_row(&conn, "s").unwrap().unwrap().state,
            QueueState::Sent
        );
        assert_eq!(
            read_row(&conn, "f").unwrap().unwrap().state,
            QueueState::Failed
        );
    }

    /// S409 — regression for the index bug itself: drive many full
    /// Queued -> Sending -> Sent cycles. Pre-fix, the `state` secondary
    /// index made `mark_sent`'s UPDATE liable to FATAL; post-fix (index
    /// dropped) every cycle completes. A tight loop surfaces the failure
    /// the brief asked us to reproduce.
    #[test]
    fn s409_repeated_state_transitions_do_not_fail() {
        let conn = open_in_memory();
        let now = OffsetDateTime::now_utc();
        for i in 0..50 {
            let id = format!("row-{i}");
            insert_queued(
                &conn,
                &id,
                "storefront",
                "[\"a@b.c\"]",
                None,
                "S",
                "B",
                None,
                None,
                "h",
                1,
                now,
            )
            .unwrap();
            let claimed = claim_next_queued(&conn, now).unwrap().unwrap();
            assert_eq!(claimed.state, QueueState::Sending);
            mark_sent(&conn, &claimed.id, now).unwrap();
            assert_eq!(
                read_row(&conn, &id).unwrap().unwrap().state,
                QueueState::Sent
            );
        }
    }

    #[test]
    fn sanitize_attachment_filename_strips_traversal() {
        // Slashes become `_` (the actual traversal mechanism). Dots are
        // kept because `.pdf` is the most common extension — but the
        // `..` segment is harmless once the slash separator is gone
        // (path joins with an embedded `..` as a basename can't escape
        // the row directory because there are no path separators left).
        assert_eq!(
            sanitize_attachment_filename("../../etc/passwd"),
            ".._.._etc_passwd"
        );
        // Sanity — no `/` survives.
        assert!(!sanitize_attachment_filename("../../etc/passwd").contains('/'));
        // And nothing starts with `..` after the safe-byte cap is run
        // through write_attachment's `<index>_<safe_name>` prefix —
        // see `write_attachment_prefixes_with_index_so_dotdot_cannot_lead`.
    }

    /// S281 / PR-266 — defence-in-depth pin: even if the sanitiser
    /// permits `..` in a basename, the queue writer prepends a numeric
    /// index (`NN_`) so the on-disk filename can never start with `.`
    /// — eliminating dotfile shadowing and the residual concern that a
    /// `..` basename could be confused for a path segment.
    #[test]
    fn write_attachment_prefixes_with_index_so_dotdot_cannot_lead() {
        let tmp = std::env::temp_dir().join(format!(
            "aberp-relay-prefix-{}-{}",
            std::process::id(),
            ulid::Ulid::new()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let basename = write_attachment(&tmp, 3, "../../etc/passwd", b"x").unwrap();
        assert!(basename.starts_with("03_"));
        assert!(!basename.starts_with('.'));
        // Cleanup
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn sanitize_attachment_filename_keeps_safe_chars() {
        assert_eq!(sanitize_attachment_filename("quote.pdf"), "quote.pdf");
        assert_eq!(
            sanitize_attachment_filename("inv-2026-01.pdf"),
            "inv-2026-01.pdf"
        );
    }

    #[test]
    fn sanitize_attachment_filename_handles_empty() {
        assert_eq!(sanitize_attachment_filename(""), "attachment.bin");
    }

    #[test]
    fn sanitize_attachment_filename_rejects_dot_only() {
        assert_eq!(sanitize_attachment_filename("."), "attachment_1.bin");
        assert_eq!(sanitize_attachment_filename(".."), "attachment_2.bin");
    }

    #[test]
    fn sanitize_attachment_filename_caps_length() {
        let big = "a".repeat(500);
        let out = sanitize_attachment_filename(&big);
        assert!(out.len() <= 128);
    }
}
