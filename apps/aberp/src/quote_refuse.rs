//! S403 — operator REFUSE-with-reason saga.
//!
//! The DEAL step (`quote_deal`) is where an operator confirms stock +
//! production capacity for an accepted auto-quote. When they CAN'T fulfil
//! it, they need the negative counterpart: refuse the order, tell the
//! customer why, and stage NOTHING. This module is that saga.
//!
//! Posture mirrors [`crate::quote_deal`]: a pure function over a
//! `Connection` + the audit ledger so the integration test drives it
//! without spinning AppState. On refuse the saga, in a single tx:
//!   1. CAS-flips `quote_intake_log.intake_state` to `refused`
//!      (`log_table::mark_refused_in_tx`, single-use guard),
//!   2. appends [`EventKind::QuoteOperatorRefused`] (reason + operator),
//!      and
//!   3. queues the bilingual customer notification e-mail into the
//!      ADR-0009 outbox (`email_relay_queue`) — atomic with the refusal
//!      per [[hulye-biztos]] so a committed refusal always carries a
//!      queued e-mail when a recipient exists.
//!
//! NO draft invoice is staged or issued. The route layer (`serve.rs`)
//! validates the reason (≥5 chars, per [[trust-code-not-operator]]) and
//! best-effort writes the storefront `rejected` status back so the
//! customer portal reflects the refusal.

use anyhow::{anyhow, Context, Result};
use duckdb::Connection;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::OffsetDateTime;
use ulid::Ulid;

use aberp_audit_ledger::{append_in_tx, Actor, EventKind, LedgerMeta};
use aberp_quote_intake::log_table;

use crate::email_relay::hash_recipient_list;
use crate::email_relay_queue;

/// Minimum reason length (chars, after trim). The route enforces this
/// before the saga ever runs; the constant lives here so the SPA copy
/// and the server gate cite the same number.
pub const REASON_MIN_CHARS: usize = 5;

/// `submitter` tag stamped on the `outbound_email_queue` row for refusal
/// notifications. Distinct from the storefront relay submitter so a
/// forensic walk / the SPA queue inspector can tell operator-refusal
/// e-mails apart from storefront-composed ones.
pub const REFUSE_EMAIL_SUBMITTER: &str = "quote-refuse";

/// Closed-vocab refuse-saga failure modes. Each maps to a distinct HTTP
/// 4xx the route renders as a red operator toast. Codes deliberately
/// overlap the DEAL saga's where the meaning is identical (`not_staged`,
/// `not_actionable`, `deal_already_issued`) so the SPA's existing typed
/// error copy is reused.
#[derive(Debug, Error, PartialEq)]
pub enum RefuseError {
    /// The quote intake row does not exist for `(tenant, quote_id)`.
    #[error("quote {quote_id} not staged in quote_intake_log (tenant {tenant})")]
    NotStaged { tenant: String, quote_id: String },
    /// The row exists but its `intake_state` is not `staged` (already
    /// refused, dismissed, or an error row).
    #[error("quote {quote_id} intake_state is {state:?}; only staged rows can be refused")]
    NotActionable { quote_id: String, state: String },
    /// The row was already DEAL'd — a confirmed order cannot be refused.
    #[error("quote {quote_id} already DEAL'd; a dealt order cannot be refused")]
    AlreadyDealt { quote_id: String },
}

impl RefuseError {
    pub fn machine_code(&self) -> &'static str {
        match self {
            RefuseError::NotStaged { .. } => "not_staged",
            RefuseError::NotActionable { .. } => "not_actionable",
            RefuseError::AlreadyDealt { .. } => "deal_already_issued",
        }
    }
}

/// Saga inputs. `reason` is already trimmed + length-validated by the
/// route (defense-in-depth: the saga trusts it but the audit + e-mail
/// carry it verbatim).
#[derive(Debug, Clone)]
pub struct RefuseSagaInputs {
    pub tenant: String,
    pub quote_id: String,
    pub reason: String,
    /// Operator login string for the audit `Actor` + payload.
    pub actor: String,
}

/// Outcome of a successful refusal. Surfaced on the route's 200 body so
/// the SPA can confirm + the operator sees whether the customer e-mail
/// was queued.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RefuseOutcome {
    /// RFC-3339 timestamp of the refusal commit.
    pub refused_at: String,
    /// The address the notification e-mail was queued to. `None` when
    /// neither the `customer_email` column nor the raw payload carried an
    /// address — surfaced loud per CLAUDE.md #12 (the row is still
    /// refused, but no customer e-mail could be queued).
    pub customer_email: Option<String>,
    /// `true` when a notification e-mail row was queued in the saga tx.
    pub email_queued: bool,
}

/// Per-quote idempotency key for the audit entry. A refusal is
/// single-use (the CAS guarantees one), so the quote id alone keys it.
pub fn refuse_idempotency_key(quote_id: &str) -> String {
    format!("quote_operator_refused:{quote_id}")
}

/// Pure, bilingual (HU/EN) refusal e-mail body. Returns
/// `(subject, body_text)`. `customer_name` may be empty (neutral
/// greeting fallback). The reason is the operator's verbatim text.
pub fn refuse_email_content(quote_id: &str, customer_name: &str, reason: &str) -> (String, String) {
    let short = &quote_id[..quote_id.len().min(8)];
    let subject =
        format!("Ajánlatát nem tudjuk teljesíteni / Unable to fulfil your quote — {short}");
    let name = customer_name.trim();
    let greeting = if name.is_empty() {
        "Tisztelt Ügyfelünk".to_string()
    } else {
        format!("Kedves {name}")
    };
    let body = format!(
        "{greeting},\n\n\
Köszönjük megrendelési szándékát (ajánlat azonosító: {quote_id}). Sajnálattal \
értesítjük, hogy ezt a megrendelést jelenleg nem tudjuk teljesíteni az alábbi ok miatt:\n\n    \
{reason}\n\n\
Elnézését kérjük az okozott kellemetlenségért. Ha kérdése van, válaszoljon erre az \
e-mailre, vagy írjon a confirmation@abenerp.com címre.\n\n\
— Áben Consulting Kft.\n\n\
──────────────────────────────────────────────────────────────────────\n\n\
{greeting},\n\n\
Thank you for your order (quote id: {quote_id}). We are sorry to inform you that we are \
unable to fulfil this order at this time, for the following reason:\n\n    \
{reason}\n\n\
We apologise for the inconvenience. If you have any questions, reply to this e-mail or \
write to confirmation@abenerp.com.\n\n\
— Áben Consulting Kft.\n"
    );
    (subject, body)
}

/// Minimal forward-tolerant view of the stored submission, just enough
/// to resolve the notification recipient. We deliberately do NOT
/// deserialize the full [`aberp_quote_intake::payload::Quote`] (which
/// requires id / received_at / request / … to be present) — a refusal
/// must not be blocked by an unrelated payload-shape drift; only the
/// contact block is needed.
#[derive(Deserialize)]
struct ContactOnly {
    contact: ContactFields,
}

#[derive(Deserialize)]
struct ContactFields {
    #[serde(default)]
    name: String,
    #[serde(default)]
    email: String,
}

/// Resolve the notification recipient + greeting name. Prefers the
/// S271 typed `customer_email` column; falls back to the authoritative
/// raw-payload `contact.email` (non-empty by intake-mapping guarantee).
fn resolve_recipient(row: &log_table::RefuseSourceRow) -> (Option<String>, String) {
    let mut name = String::new();
    let mut email = row
        .customer_email
        .as_deref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if let Ok(parsed) = serde_json::from_str::<ContactOnly>(&row.raw_payload) {
        name = parsed.contact.name.trim().to_string();
        if email.is_none() {
            let e = parsed.contact.email.trim().to_string();
            if !e.is_empty() {
                email = Some(e);
            }
        }
    }
    (email, name)
}

/// Run the refuse saga end-to-end. On any precondition failure returns
/// `Err(anyhow::Error)` wrapping a [`RefuseError`] the route downcasts to
/// the right 4xx machine code.
pub fn run_refuse_saga(
    conn: &mut Connection,
    ledger_meta: &LedgerMeta,
    ledger_actor: Actor,
    inputs: RefuseSagaInputs,
) -> Result<RefuseOutcome> {
    log_table::ensure_schema(conn).map_err(|e| anyhow!("ensure quote_intake_log schema: {e}"))?;
    email_relay_queue::ensure_schema(conn).context("ensure outbound_email_queue schema")?;

    // ── Precondition read (outside the tx) — same actionable gate as DEAL.
    let row = log_table::read_for_refuse(conn, &inputs.tenant, &inputs.quote_id)
        .map_err(|e| anyhow!("read quote_intake_log for refuse: {e}"))?
        .ok_or_else(|| {
            anyhow!(RefuseError::NotStaged {
                tenant: inputs.tenant.clone(),
                quote_id: inputs.quote_id.clone(),
            })
        })?;

    // Dealt takes precedence over state so a refuse attempt on a dealt
    // row gets the more specific 409 (cannot un-deal an order).
    if row.deal_issued_at.is_some() {
        return Err(anyhow!(RefuseError::AlreadyDealt {
            quote_id: inputs.quote_id.clone(),
        }));
    }
    if row.intake_state != log_table::STATE_STAGED {
        return Err(anyhow!(RefuseError::NotActionable {
            quote_id: inputs.quote_id.clone(),
            state: row.intake_state,
        }));
    }

    let (recipient, customer_name) = resolve_recipient(&row);

    let now_utc = OffsetDateTime::now_utc();
    let now_iso = now_utc
        .format(&time::format_description::well_known::Rfc3339)
        .context("format refuse timestamp")?;
    let idempotency_key = refuse_idempotency_key(&inputs.quote_id);

    let tx = conn.transaction().context("begin refuse saga tx")?;

    // CAS flip — single-use. Zero rows means the row was DEAL'd /
    // dismissed / already-refused between the pre-flight read and here.
    let claimed = log_table::mark_refused_in_tx(&tx, &inputs.tenant, &inputs.quote_id)
        .map_err(|e| anyhow!("CAS mark_refused_in_tx: {e}"))?;
    if claimed != 1 {
        // Re-read inside the tx to discriminate gone / dealt / non-staged.
        let row_now = log_table::read_for_deal_in_tx(&tx, &inputs.tenant, &inputs.quote_id)
            .map_err(|e| anyhow!("re-read after CAS rejection: {e}"))?;
        drop(tx);
        return Err(anyhow!(match row_now {
            None => RefuseError::NotStaged {
                tenant: inputs.tenant.clone(),
                quote_id: inputs.quote_id.clone(),
            },
            Some(r) if r.deal_issued_at.is_some() => RefuseError::AlreadyDealt {
                quote_id: inputs.quote_id.clone(),
            },
            Some(r) => RefuseError::NotActionable {
                quote_id: inputs.quote_id.clone(),
                state: r.intake_state,
            },
        }));
    }

    // Audit-of-record. `customer_email_present` is logged loud so a
    // missing address (no e-mail queued) surfaces on the forensic walk.
    let payload = serde_json::json!({
        "quote_id": inputs.quote_id,
        "tenant_id": inputs.tenant,
        "reason": inputs.reason,
        "operator_user_id": inputs.actor,
        "refused_at": now_iso,
        "customer_email_present": recipient.is_some(),
        "actor": inputs.actor,
        "idempotency_key": idempotency_key,
    });
    let bytes = serde_json::to_vec(&payload).context("encode QuoteOperatorRefused payload")?;
    append_in_tx(
        &tx,
        ledger_meta,
        EventKind::QuoteOperatorRefused,
        bytes,
        ledger_actor,
        Some(idempotency_key.clone()),
    )
    .context("audit append QuoteOperatorRefused")?;

    // Queue the customer notification e-mail in the SAME tx. A row with
    // no resolvable recipient still refuses + audits (loud), but no
    // e-mail can be queued.
    let email_queued = if let Some(ref to) = recipient {
        let (subject, body_text) =
            refuse_email_content(&inputs.quote_id, &customer_name, &inputs.reason);
        let to_json = serde_json::to_string(std::slice::from_ref(to))
            .context("serialize refuse e-mail recipient")?;
        let recipient_hash = hash_recipient_list(std::slice::from_ref(to));
        let byte_size = (subject.len() + body_text.len()) as u64;
        let email_id = format!("eml_{}", Ulid::new());
        email_relay_queue::insert_queued(
            &tx,
            &email_id,
            REFUSE_EMAIL_SUBMITTER,
            &to_json,
            None,
            &subject,
            &body_text,
            None,
            None,
            &recipient_hash,
            byte_size,
            now_utc,
        )
        .context("queue refuse notification e-mail")?;
        true
    } else {
        false
    };

    tx.commit().context("commit refuse saga tx")?;

    Ok(RefuseOutcome {
        refused_at: now_iso,
        customer_email: recipient,
        email_queued,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aberp_audit_ledger::{
        ensure_schema as audit_ensure_schema, BinaryHash, LedgerMeta, TenantId,
    };
    use aberp_quote_intake::log_table as quote_log;
    use duckdb::Connection;
    use time::OffsetDateTime;

    fn open_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("open in-memory DuckDB");
        audit_ensure_schema(&conn).expect("audit-ledger schema");
        quote_log::ensure_schema(&conn).expect("quote_intake_log schema");
        conn
    }

    fn ledger_meta() -> LedgerMeta {
        LedgerMeta::new(
            TenantId::new("test-tenant").unwrap(),
            BinaryHash::from_bytes([0u8; 32]),
        )
    }

    fn seed_staged(conn: &Connection, quote_id: &str, raw: &str) {
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        quote_log::insert_intake(conn, "t", quote_id, "inv_A", "r", now, raw, "{}").unwrap();
    }

    fn inputs(quote_id: &str, reason: &str) -> RefuseSagaInputs {
        RefuseSagaInputs {
            tenant: "t".to_string(),
            quote_id: quote_id.to_string(),
            reason: reason.to_string(),
            actor: "op@abenerp.com".to_string(),
        }
    }

    fn count_outbox(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM outbound_email_queue", [], |r| {
            r.get(0)
        })
        .unwrap()
    }

    fn count_audit_refused(conn: &Connection) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM audit_ledger WHERE kind = 'quote.operator_refused'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn happy_path_flips_refused_audits_and_queues_email_no_invoice() {
        let mut conn = open_conn();
        let raw = "{\"contact\":{\"name\":\"Anна\",\"email\":\"buyer@example.com\"}}";
        seed_staged(&conn, "q-1", raw);

        let meta = ledger_meta();
        let actor = Actor::from_local_cli(Ulid::new().to_string(), "op@abenerp.com");
        let out = run_refuse_saga(
            &mut conn,
            &meta,
            actor,
            inputs(
                "q-1",
                "Anyaghiány — a kért ötvözet jelenleg nincs raktáron.",
            ),
        )
        .expect("refuse saga succeeds");

        assert!(out.email_queued, "a recipient resolves → e-mail queued");
        assert_eq!(out.customer_email.as_deref(), Some("buyer@example.com"));

        // State flipped to refused (drops out of the actionable queue).
        let row = quote_log::read_for_refuse(&conn, "t", "q-1")
            .unwrap()
            .unwrap();
        assert_eq!(row.intake_state, quote_log::STATE_REFUSED);

        // Exactly one audit + one outbox row; NO sales-order / deal mint.
        assert_eq!(count_audit_refused(&conn), 1);
        assert_eq!(count_outbox(&conn), 1);
        assert!(
            row.deal_issued_at.is_none(),
            "refusing never DEAL's the row (no invoice staged)"
        );

        // The queued e-mail carries the reason verbatim, bilingual.
        let body: String = conn
            .query_row("SELECT body_text FROM outbound_email_queue", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(body.contains("nincs raktáron"), "reason in HU section");
        assert!(body.contains("unable to fulfil"), "EN section present");
    }

    #[test]
    fn missing_recipient_still_refuses_and_audits_but_no_email() {
        let mut conn = open_conn();
        // Raw payload with an empty contact email + no typed column.
        seed_staged(&conn, "q-2", "{\"contact\":{\"name\":\"\",\"email\":\"\"}}");

        let meta = ledger_meta();
        let actor = Actor::from_local_cli(Ulid::new().to_string(), "op");
        let out = run_refuse_saga(
            &mut conn,
            &meta,
            actor,
            inputs("q-2", "Túl rövid határidő."),
        )
        .expect("refuse saga succeeds even without a recipient");

        assert!(!out.email_queued);
        assert!(out.customer_email.is_none());
        assert_eq!(count_audit_refused(&conn), 1, "still audited");
        assert_eq!(count_outbox(&conn), 0, "no e-mail without a recipient");
        assert_eq!(
            quote_log::read_for_refuse(&conn, "t", "q-2")
                .unwrap()
                .unwrap()
                .intake_state,
            quote_log::STATE_REFUSED
        );
    }

    #[test]
    fn missing_row_is_not_staged() {
        let mut conn = open_conn();
        let meta = ledger_meta();
        let actor = Actor::from_local_cli(Ulid::new().to_string(), "op");
        let err = run_refuse_saga(&mut conn, &meta, actor, inputs("ghost", "reason here"))
            .expect_err("missing row");
        assert_eq!(
            err.downcast::<RefuseError>().unwrap().machine_code(),
            "not_staged"
        );
    }

    #[test]
    fn replay_against_refused_row_is_not_actionable() {
        let mut conn = open_conn();
        seed_staged(
            &conn,
            "q-3",
            "{\"contact\":{\"name\":\"X\",\"email\":\"x@e.com\"}}",
        );
        let meta = ledger_meta();
        run_refuse_saga(
            &mut conn,
            &meta,
            Actor::from_local_cli(Ulid::new().to_string(), "op"),
            inputs("q-3", "first refusal reason"),
        )
        .unwrap();
        let err = run_refuse_saga(
            &mut conn,
            &meta,
            Actor::from_local_cli(Ulid::new().to_string(), "op"),
            inputs("q-3", "second refusal reason"),
        )
        .expect_err("second refuse loses the CAS");
        assert_eq!(
            err.downcast::<RefuseError>().unwrap().machine_code(),
            "not_actionable"
        );
        // Idempotent: still exactly one audit + one outbox row.
        assert_eq!(count_audit_refused(&conn), 1);
        assert_eq!(count_outbox(&conn), 1);
    }

    #[test]
    fn dealt_row_cannot_be_refused() {
        let mut conn = open_conn();
        seed_staged(
            &conn,
            "q-4",
            "{\"contact\":{\"name\":\"X\",\"email\":\"x@e.com\"}}",
        );
        // DEAL the row first.
        let tx = conn.transaction().unwrap();
        quote_log::mark_deal_issued_in_tx(
            &tx,
            "t",
            "q-4",
            "so_A",
            "wo_B",
            "2026-06-14T00:00:00Z",
            None,
        )
        .unwrap();
        tx.commit().unwrap();

        let meta = ledger_meta();
        let err = run_refuse_saga(
            &mut conn,
            &meta,
            Actor::from_local_cli(Ulid::new().to_string(), "op"),
            inputs("q-4", "cannot refuse a dealt order"),
        )
        .expect_err("dealt row");
        assert_eq!(
            err.downcast::<RefuseError>().unwrap().machine_code(),
            "deal_already_issued"
        );
        assert_eq!(count_outbox(&conn), 0, "no e-mail on a refused refusal");
    }

    #[test]
    fn email_content_is_bilingual_and_carries_reason() {
        let (subject, body) = refuse_email_content(
            "0226e154-aaaa-bbbb-cccc-dddddddddddd",
            "Béla",
            "stock shortfall",
        );
        assert!(subject.contains("0226e154"), "subject carries short id");
        assert!(body.contains("Kedves Béla"), "HU greeting with name");
        assert!(body.contains("stock shortfall"), "reason embedded");
        assert!(body.contains("unable to fulfil"), "EN half present");
    }
}
