//! S279 / PR-265 — `quote_pricing_jobs` table + state machine for the
//! ABERP-side auto-quoting producer pipeline.
//!
//! ## Posture
//!
//! Distinct from [`crate::quote_intake_query`]'s `quote_intake_log` —
//! that table is "approved quotes awaiting DEAL", this table is
//! "pending quotes needing pricing." The two never overlap: a
//! storefront quote starts in `received` state, the pricing pipeline
//! drives it through this table until the storefront flips it to
//! `quoted`. Customer accept then later promotes it to `approved`,
//! which the existing intake daemon picks up into `quote_intake_log`.
//!
//! ## State machine
//!
//! ```text
//!   Fetched ──► Extracting ──► Pricing ──► Rendering ──► PostingBack ──► Posted
//!      │            │              │            │              │            (terminal)
//!      ▼            ▼              ▼            ▼              ▼
//!   Failed       Failed         Failed       Failed        Failed
//!  (operator retry re-enqueues at Fetched)
//! ```
//!
//! Per [[no-sql-specific]] the state is **app-layer enforced**, not a
//! DuckDB CHECK. Closed-vocab strings ([`STATE_FETCHED`] etc.) are the
//! single source of truth; any `from_storage_str` failure is loud.
//!
//! ## No CHECK constraints, no DEFAULTs
//!
//! Same DuckDB gotcha as [[material-reservation-s273]] and S271's
//! storefront projection columns: `ALTER TABLE ... ADD COLUMN IF NOT
//! EXISTS col TYPE DEFAULT V` silently re-applies the default on every
//! replay. Every column is nullable; the app layer enforces required-
//! vs-optional on insert/update.

use anyhow::{anyhow, Context, Result};
use duckdb::{params, Connection};
use time::OffsetDateTime;

/// State name as stored in the table's `state` VARCHAR column.
///
/// Closed-vocab — any string parsed as state goes through
/// [`JobState::parse_str`] which errors on unknown values (CLAUDE.md
/// rule 12).
pub const STATE_FETCHED: &str = "fetched";
/// Daemon has begun the CAD subprocess via `aberp-cad-extract-wrapper`.
pub const STATE_EXTRACTING: &str = "extracting";
/// Subprocess succeeded; daemon is now calling `aberp_quote_engine::quote`.
pub const STATE_PRICING: &str = "pricing";
/// Engine returned `Ok(_)`; daemon is now rendering the PDF.
pub const STATE_RENDERING: &str = "rendering";
/// PDF rendered; daemon is POSTing the priced multipart to the
/// storefront's `/api/quotes/{id}/priced` endpoint per ADR-0004.
pub const STATE_POSTING_BACK: &str = "posting_back";
/// Storefront returned 200 (incl. idempotent replay). Terminal.
pub const STATE_POSTED: &str = "posted";
/// Any stage failure lands here. Operator can retry from the SPA,
/// which re-enqueues at `Fetched`.
pub const STATE_FAILED: &str = "failed";
/// S414/S414b — terminal "operator dispositioned this row out of the
/// queue" state. The operator's Delete click flips a `Failed` row here
/// (it does NOT hard-delete — see [`archive_failed_job_in_tx`]). Keeping
/// the row (same `quote_id` PK) is what stops the daemon re-pulling the
/// quote from the storefront `?status=received` listing and re-inserting
/// it every poll cycle: `insert_fetched_job` / `insert_failed_enqueue_job`
/// both `ON CONFLICT (quote_id) DO NOTHING`, so an archived row makes the
/// re-enqueue a silent no-op (closes the S414b "permanent-Failed re-WARN
/// every cycle" loop). Filtered out of the operator panel by [`list_jobs`]
/// and never picked up by [`next_actionable_job`].
pub const STATE_ARCHIVED: &str = "archived";

/// Closed-vocab `state` value the pricing pipeline tracks for each
/// row. Wire shape on insert + read is the lowercase string above.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    /// Fetched from storefront; awaiting extract.
    Fetched,
    /// Extract subprocess running.
    Extracting,
    /// Engine running.
    Pricing,
    /// PDF rendering.
    Rendering,
    /// POSTing the multipart writeback.
    PostingBack,
    /// Terminal success.
    Posted,
    /// Terminal failure (operator can retry).
    Failed,
    /// S414/S414b — terminal, operator-dispositioned (Delete click). The
    /// row stays in the table so the daemon's `ON CONFLICT` re-enqueue
    /// guard keeps skipping the quote_id; hidden from the operator panel.
    Archived,
}

/// S290 / PR-271 — closed-vocab classifier verdict that rides alongside
/// every `Failed` row. Lets the daemon decide whether to auto-re-enqueue
/// (Transient) or wait for an operator Retry click (Permanent / Unknown
/// past the auto-retry cap).
pub const FAILURE_KIND_TRANSIENT: &str = "transient";
/// S290 / PR-271 — wait-for-operator failure (extractor stub, schema
/// version mismatch, MarginFloor, missing material, 4xx).
pub const FAILURE_KIND_PERMANENT: &str = "permanent";
/// S290 / PR-271 — default when the classifier didn't recognise the
/// error shape. Treated as Transient up to [`UNKNOWN_AUTO_RETRY_CAP`]
/// auto-retries, then frozen pending operator action — defence against
/// silent permanent loops on a future error we haven't classified yet.
pub const FAILURE_KIND_UNKNOWN: &str = "unknown";

/// Auto-retry cap for `Unknown`-classified Failed rows. Three matches
/// the storefront's "retry once or twice before giving up" intuition;
/// the cap lives in code (not config) because changing it without an
/// audit-row-emit change would silently shift behavior on prod —
/// CLAUDE.md rule 12.
pub const UNKNOWN_AUTO_RETRY_CAP: u32 = 3;

/// Closed-vocab failure-kind verdict. Rides on a Failed row's
/// `failure_kind` column. NULL ↔ legacy pre-PR-271 row → treated as
/// `Unknown` for daemon scheduling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// Retry on next cycle (network blip, 5xx, timeout).
    Transient,
    /// Wait for operator Retry click (extractor stub / bad input /
    /// schema-version mismatch / 4xx).
    Permanent,
    /// Classifier didn't recognise the error. Capped auto-retry, then
    /// surfaces operator-action-required.
    Unknown,
}

impl FailureKind {
    /// DB storage string.
    pub fn as_str(self) -> &'static str {
        match self {
            FailureKind::Transient => FAILURE_KIND_TRANSIENT,
            FailureKind::Permanent => FAILURE_KIND_PERMANENT,
            FailureKind::Unknown => FAILURE_KIND_UNKNOWN,
        }
    }

    /// Round-trip parse. Errors loud on unknown — silent fallback would
    /// mask schema drift.
    pub fn parse_str(s: &str) -> Result<Self> {
        match s {
            FAILURE_KIND_TRANSIENT => Ok(FailureKind::Transient),
            FAILURE_KIND_PERMANENT => Ok(FailureKind::Permanent),
            FAILURE_KIND_UNKNOWN => Ok(FailureKind::Unknown),
            other => Err(anyhow!(
                "unknown quote_pricing_jobs.failure_kind: {other:?}"
            )),
        }
    }
}

impl JobState {
    /// DB storage string.
    pub fn as_str(self) -> &'static str {
        match self {
            JobState::Fetched => STATE_FETCHED,
            JobState::Extracting => STATE_EXTRACTING,
            JobState::Pricing => STATE_PRICING,
            JobState::Rendering => STATE_RENDERING,
            JobState::PostingBack => STATE_POSTING_BACK,
            JobState::Posted => STATE_POSTED,
            JobState::Failed => STATE_FAILED,
            JobState::Archived => STATE_ARCHIVED,
        }
    }

    /// Round-trip parse. Errors loud on unknown — silent-fallback
    /// would mask schema drift. Named `parse_str` rather than
    /// `from_str` so we don't accidentally shadow the std
    /// `FromStr::from_str` signature (clippy warns about the trait
    /// confusion; we don't want or need the `FromStr` trait here
    /// because the error type carries `anyhow` context).
    pub fn parse_str(s: &str) -> Result<Self> {
        match s {
            STATE_FETCHED => Ok(JobState::Fetched),
            STATE_EXTRACTING => Ok(JobState::Extracting),
            STATE_PRICING => Ok(JobState::Pricing),
            STATE_RENDERING => Ok(JobState::Rendering),
            STATE_POSTING_BACK => Ok(JobState::PostingBack),
            STATE_POSTED => Ok(JobState::Posted),
            STATE_FAILED => Ok(JobState::Failed),
            STATE_ARCHIVED => Ok(JobState::Archived),
            other => Err(anyhow!("unknown quote_pricing_jobs.state: {other:?}")),
        }
    }

    /// S350 / PR-39 (U5) — may an operator inline-edit this row's
    /// `material_grade`? Editable while `Fetched` / `PostingBack` /
    /// `Failed`; refused mid-pipeline (`Extracting` / `Pricing` /
    /// `Rendering` — editing would race the daemon's in-flight stage
    /// transition) and once `Posted` (terminal: the storefront already
    /// holds the priced result, so a re-priced row would 409 on the new
    /// `feature_graph_hash`, F12). The serve-layer handler maps a
    /// non-editable row to 409.
    pub fn material_editable(self) -> bool {
        matches!(
            self,
            JobState::Fetched | JobState::PostingBack | JobState::Failed
        )
    }
}

/// One row of `quote_pricing_jobs`. Read-only projection used by the
/// SPA list + the daemon's state machine.
#[derive(Debug, Clone)]
pub struct PricingJobRow {
    /// Storefront UUID — the durable identity.
    pub quote_id: String,
    pub tenant_id: String,
    pub state: JobState,
    /// ISO-8601 timestamps for the operator-visible "since" column.
    pub fetched_at: String,
    pub updated_at: String,
    pub customer_email: String,
    pub customer_name: String,
    /// S401 — buyer's company, carried from the storefront quote's
    /// `contact.company`. `None` on legacy PROD_v2.27.[0-55] rows that
    /// pre-date this column (rendered as a placeholder by the operator
    /// SPA); `Some("")` when the buyer left the company field blank.
    pub customer_company: Option<String>,
    pub material_grade: String,
    pub quantity: u32,
    /// `Some(_)` once Extracting succeeded; carries the blake3 hash
    /// the storefront uses as the priced-writeback idempotency key.
    pub feature_graph_hash: Option<String>,
    /// `Some(_)` once Pricing succeeded; total in EUR, surfaced in
    /// the SPA list column.
    pub total_price_eur: Option<f64>,
    /// `Some(_)` on Failed rows; operator-readable stage + reason.
    pub error_stage: Option<String>,
    pub error_reason: Option<String>,
    /// Increments on every retry — disambiguates the audit-failure
    /// idempotency key suffix so a re-failure doesn't UNIQUE-collide.
    pub attempt_n: u32,
    /// S290 / PR-271 — `Some(_)` on Failed rows that the classifier
    /// tagged with a verdict. `None` on legacy PROD_v2.27.[0-5] rows
    /// AND on non-Failed rows. Treated as `Unknown` by the daemon's
    /// auto-retry decision for backwards compatibility.
    pub failure_kind: Option<FailureKind>,
}

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS quote_pricing_jobs (
    quote_id              VARCHAR NOT NULL PRIMARY KEY,
    tenant_id             VARCHAR NOT NULL,
    state                 VARCHAR NOT NULL,
    fetched_at            VARCHAR NOT NULL,
    updated_at            VARCHAR NOT NULL,
    customer_email        VARCHAR NOT NULL,
    customer_name         VARCHAR NOT NULL,
    customer_company      VARCHAR,
    material_grade        VARCHAR NOT NULL,
    quantity              INTEGER NOT NULL,
    cad_filename          VARCHAR NOT NULL,
    cad_local_path        VARCHAR NOT NULL,
    feature_graph_hash    VARCHAR,
    feature_graph_json    VARCHAR,
    breakdown_json        VARCHAR,
    pdf_path              VARCHAR,
    total_price_eur       DOUBLE,
    valid_until_iso       VARCHAR,
    error_stage           VARCHAR,
    error_reason          VARCHAR,
    attempt_n             INTEGER NOT NULL,
    failure_kind          VARCHAR
);
-- S286 / PR-268 — explicitly drop the (tenant_id, state) secondary
-- index that S279 ship-cut introduced. The PROD_v2.27.2 crash stack
-- (`RowGroupCollection::RemoveFromIndexes` inside
-- `UndoBuffer::RevertCommit` after a PK \"duplicate key\" inside the
-- INSERT-side of UPDATE's MVCC-lowering) matches a known class of
-- DuckDB issue: UPDATE on a column that participates in a SECONDARY
-- index, on a table that also has a PRIMARY KEY, can fire spurious PK
-- constraint violations during commit. Since `state` IS part of the
-- index AND is what every daemon `set_state` UPDATE touches, the
-- index is the most likely trigger. Dropping it is purely subtractive:
-- `next_actionable_job` falls back to a full-tenant scan on a sub-1k-
-- row table, which costs microseconds in v1. The index can be added
-- back via a future ADR if the row population ever justifies it.
DROP INDEX IF EXISTS quote_pricing_jobs_tenant_state_idx;
-- S290 / PR-271 — additive migration for installs already carrying the
-- pre-PR-271 schema (PROD_v2.27.[0-5]). New nullable column, no DEFAULT
-- per [[no-sql-specific]]; the app layer treats NULL as 'Unknown' for
-- the daemon scheduler so existing Failed rows keep their auto-retry
-- behavior until they're operator-Retry'd (which writes a fresh row at
-- Fetched + clears the column).
ALTER TABLE quote_pricing_jobs ADD COLUMN IF NOT EXISTS failure_kind VARCHAR;
-- S401 — additive migration for installs carrying the pre-S401 schema
-- (PROD_v2.27.[0-55]). New nullable column, no DEFAULT per
-- [[no-sql-specific]]; existing rows read back NULL (the operator SPA
-- renders a placeholder) until they're re-fetched/retried, which writes
-- the buyer's company from the storefront listing.
ALTER TABLE quote_pricing_jobs ADD COLUMN IF NOT EXISTS customer_company VARCHAR;
-- S427 — capacity-aware lead-time. `lead_time_days` is the engine-
-- computed estimate stamped at pricing; `lead_time_override_days` is the
-- operator's manual override (NULL = use the computed value). Both
-- nullable, no DEFAULT per [[no-sql-specific]]; existing rows read back
-- NULL until re-priced / overridden.
ALTER TABLE quote_pricing_jobs ADD COLUMN IF NOT EXISTS lead_time_days INTEGER;
ALTER TABLE quote_pricing_jobs ADD COLUMN IF NOT EXISTS lead_time_override_days INTEGER;
";

/// Idempotent — call at every writer entry.
///
/// **S286 / PR-268 caveat**: existing prod DBs from PROD_v2.27.[012]
/// carry the orphan `quote_pricing_jobs_tenant_state_idx` index. The
/// embedded `DROP INDEX IF EXISTS` in `SCHEMA_SQL` cleans it up on the
/// first call after upgrade — the daemon's startup is the first call.
pub fn ensure_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA_SQL)
        .with_context(|| "ensure quote_pricing_jobs schema")
}

/// S288 / PR-269 — does the orphan `quote_pricing_jobs_tenant_state_idx`
/// secondary index currently exist on this DB? Queries DuckDB's
/// `duckdb_indexes()` system table function; safe to call on a DB that
/// hasn't yet seen `ensure_schema` (returns `Ok(false)` because the table
/// itself doesn't exist).
///
/// Caller is `serve.rs` boot, BEFORE the first `ensure_schema` call —
/// that's how the caller can observe the index's presence (`true`) and
/// thus know whether to emit the one-shot
/// `quote.pricing_jobs_index_migrated` audit row.
pub fn detect_secondary_index_present(conn: &Connection) -> Result<bool> {
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM duckdb_indexes() \
             WHERE index_name = 'quote_pricing_jobs_tenant_state_idx'",
            [],
            |r| r.get(0),
        )
        .context("query duckdb_indexes for orphan secondary index presence")?;
    Ok(n > 0)
}

/// S288 / PR-269 — boot-time migration step. Detects whether the orphan
/// `quote_pricing_jobs_tenant_state_idx` secondary index is present and,
/// if so, drops it. Returns the *prior* presence (`true` = "we just
/// migrated", `false` = "already migrated / fresh DB / no-op").
///
/// Idempotent: a `false` return on the second boot is the steady-state.
/// On `Ok(true)` the caller (boot) emits the one-shot
/// `quote.pricing_jobs_index_migrated` audit row.
///
/// **Ordering matters**: detection MUST run before `ensure_schema`,
/// because `SCHEMA_SQL` itself includes a `DROP INDEX IF EXISTS` that
/// would otherwise erase the evidence the audit row is recording.
/// `duckdb_indexes()` is a system table function that returns zero rows
/// on a fresh DB regardless of whether the user table exists, so
/// pre-`ensure_schema` calls are safe.
pub fn migrate_secondary_index_with_report(conn: &Connection) -> Result<bool> {
    let was_present = detect_secondary_index_present(conn)?;
    ensure_schema(conn)?;
    if was_present {
        // Belt + braces: ensure_schema dropped it via SCHEMA_SQL's
        // `DROP INDEX IF EXISTS`. This explicit drop survives a future
        // refactor that removes the SCHEMA_SQL line — the migration's
        // correctness shouldn't depend on the schema-bootstrap path.
        conn.execute_batch("DROP INDEX IF EXISTS quote_pricing_jobs_tenant_state_idx;")
            .context("drop orphan secondary index (S288 migration)")?;
    }
    Ok(was_present)
}

/// Insert a new `Fetched` job. Idempotent via `quote_id` PK: a re-
/// fetch on an existing row returns `Ok(false)`. The caller emits
/// the `QuotePricingFetched` audit row ONLY when this returns
/// `Ok(true)`.
#[allow(clippy::too_many_arguments)]
pub fn insert_fetched_job(
    conn: &Connection,
    quote_id: &str,
    tenant_id: &str,
    customer_email: &str,
    customer_name: &str,
    customer_company: &str,
    material_grade: &str,
    quantity: u32,
    cad_filename: &str,
    cad_local_path: &str,
    now: OffsetDateTime,
) -> Result<bool> {
    ensure_schema(conn)?;
    let ts = now
        .format(&time::format_description::well_known::Rfc3339)
        .context("format fetched_at")?;
    let rows = conn
        .execute(
            "INSERT INTO quote_pricing_jobs (
                quote_id, tenant_id, state, fetched_at, updated_at,
                customer_email, customer_name, customer_company, material_grade, quantity,
                cad_filename, cad_local_path, attempt_n
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0)
            ON CONFLICT (quote_id) DO NOTHING",
            params![
                quote_id,
                tenant_id,
                STATE_FETCHED,
                ts,
                ts,
                customer_email,
                customer_name,
                customer_company,
                material_grade,
                quantity as i64,
                cad_filename,
                cad_local_path,
            ],
        )
        .context("insert quote_pricing_jobs row")?;
    Ok(rows > 0)
}

/// S379 / PR-379 — insert a terminal `Failed` row for a quote whose
/// storefront listing came back WITHOUT any CAD file. There is no Fetched
/// row to transition (the no-CAD check in
/// [`crate::quote_pricing_pipeline::PricingPipelineService::enqueue_one`]
/// runs *before* [`insert_fetched_job`]), so we INSERT directly at
/// `Failed` with the enqueue-stage failure columns + placeholder CAD
/// fields (`(missing)`, the legacy no-CAD shape).
///
/// `ON CONFLICT (quote_id) DO NOTHING` is the idempotency guard that
/// closes the S376 phantom-retry loop: once the row exists, every
/// subsequent 60s enqueue cycle's insert is a no-op, so the daemon SKIPS
/// the quote_id instead of re-`warn!`-ing it forever. Returns `true` when
/// a fresh row was inserted, `false` when one already existed.
#[allow(clippy::too_many_arguments)]
pub fn insert_failed_enqueue_job(
    conn: &Connection,
    quote_id: &str,
    tenant_id: &str,
    customer_email: &str,
    customer_name: &str,
    customer_company: &str,
    material_grade: &str,
    quantity: u32,
    stage: &str,
    reason: &str,
    failure_kind: FailureKind,
    now: OffsetDateTime,
) -> Result<bool> {
    ensure_schema(conn)?;
    let ts = now
        .format(&time::format_description::well_known::Rfc3339)
        .context("format fetched_at")?;
    let safe = sanitize_reason(reason);
    let rows = conn
        .execute(
            "INSERT INTO quote_pricing_jobs (
                quote_id, tenant_id, state, fetched_at, updated_at,
                customer_email, customer_name, customer_company, material_grade, quantity,
                cad_filename, cad_local_path, attempt_n,
                error_stage, error_reason, failure_kind
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, '(missing)', '(missing)', 0, ?, ?, ?)
            ON CONFLICT (quote_id) DO NOTHING",
            params![
                quote_id,
                tenant_id,
                STATE_FAILED,
                ts,
                ts,
                customer_email,
                customer_name,
                customer_company,
                material_grade,
                quantity as i64,
                stage,
                safe,
                failure_kind.as_str(),
            ],
        )
        .context("insert failed enqueue row")?;
    Ok(rows > 0)
}

/// Outcome of a state-machine transition. Distinguishes the three cases
/// the daemon needs to react to without panicking:
///
/// - `Applied` — the UPDATE matched 1 row and committed.
/// - `AlreadyInState` — the row was already at the target state; no UPDATE
///   was issued. This is the [[trust-code-not-operator]] defence against
///   the S286 PROD_v2.27.2 panic: a UPDATE-to-the-same-value on certain
///   DuckDB index states FATAL'd via `libc++abi: terminating due to
///   uncaught exception` (DuckDB INTERNAL Error class), bringing the entire
///   process down. SELECT-first → skip-write avoids triggering the FATAL
///   path entirely.
/// - `NotFound` — no row matched. Daemon callers treat this as a skip
///   (warn-log, continue with next job).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionOutcome {
    Applied,
    AlreadyInState,
    NotFound,
}

/// Flip state to `to`. Caller is responsible for ordering — this helper
/// is unconditional, the state machine lives in the daemon pipeline
/// module.
///
/// **S286 hotfix**: SELECT-first. If the row already carries `to` as its
/// state, skip the UPDATE entirely and return `AlreadyInState` — the
/// PROD_v2.27.2 panic stack was a DuckDB FATAL inside `RowGroupCollection::
/// RemoveFromIndexes` during the commit of an UPDATE that internally lowers
/// to a DELETE+INSERT. The defence is to never issue the UPDATE when it
/// would be a no-op anyway; combined with the supervisor in
/// [`crate::quote_pricing_pipeline::PricingPipelineService::run_daemon_supervised`]
/// the daemon recovers instead of taking the host process down.
pub fn set_state(
    conn: &Connection,
    quote_id: &str,
    tenant_id: &str,
    to: JobState,
    now: OffsetDateTime,
) -> Result<TransitionOutcome> {
    ensure_schema(conn)?;
    match read_state(conn, quote_id, tenant_id)? {
        None => Ok(TransitionOutcome::NotFound),
        Some(cur) if cur == to => Ok(TransitionOutcome::AlreadyInState),
        Some(_) => {
            let ts = now
                .format(&time::format_description::well_known::Rfc3339)
                .context("format updated_at")?;
            let rows = conn
                .execute(
                    "UPDATE quote_pricing_jobs
                        SET state = ?, updated_at = ?
                        WHERE quote_id = ? AND tenant_id = ?",
                    params![to.as_str(), ts, quote_id, tenant_id],
                )
                .context("set_state UPDATE")?;
            if rows == 0 {
                // Race: the row was deleted between read_state and UPDATE.
                // Treat as NotFound rather than panicking on inconsistency.
                Ok(TransitionOutcome::NotFound)
            } else {
                Ok(TransitionOutcome::Applied)
            }
        }
    }
}

/// Cheap helper — read just the `state` column for a single row. Returns
/// `None` when no row matches. Used by `set_state` (and tests) for the
/// SELECT-first defensive pattern. Tenant-scoped (matches every writer's
/// `WHERE tenant_id = ?` posture).
fn read_state(conn: &Connection, quote_id: &str, tenant_id: &str) -> Result<Option<JobState>> {
    let mut stmt = conn
        .prepare(
            "SELECT state FROM quote_pricing_jobs
                WHERE quote_id = ? AND tenant_id = ?",
        )
        .context("prepare read_state")?;
    let mut rows = stmt
        .query(params![quote_id, tenant_id])
        .context("execute read_state")?;
    match rows.next().context("step read_state")? {
        Some(r) => {
            let s: String = r.get(0).context("get state col")?;
            Ok(Some(JobState::parse_str(&s)?))
        }
        None => Ok(None),
    }
}

/// Read-only count of rows in `quote_pricing_jobs`. Emitted to tracing at
/// daemon boot so the operator can see the population on the next launch
/// without surprises (S286 hotfix posture: existing rows that pre-date this
/// PR may be the trigger for the C++ FATAL path; this lets us see them).
pub fn count_jobs(conn: &Connection, tenant_id: &str) -> Result<u64> {
    ensure_schema(conn)?;
    let mut stmt = conn
        .prepare(
            "SELECT COUNT(*) FROM quote_pricing_jobs
                WHERE tenant_id = ?",
        )
        .context("prepare count_jobs")?;
    let n: i64 = stmt
        .query_row(params![tenant_id], |r| r.get(0))
        .context("execute count_jobs")?;
    Ok(n.max(0) as u64)
}

/// Stamp the extracted FeatureGraph + its hash, then flip state to
/// `Pricing`. One transaction so a crash mid-update can't leave a row
/// with `feature_graph_hash` set but `state` still `Extracting`.
///
/// **S286 hotfix**: SELECT-first defensive pattern. If the row is already
/// at `Pricing`, skip the UPDATE entirely (`AlreadyInState`). See
/// [`set_state`] for the FATAL-path rationale.
pub fn set_extracted(
    conn: &mut Connection,
    quote_id: &str,
    tenant_id: &str,
    feature_graph_hash: &str,
    feature_graph_json: &str,
    now: OffsetDateTime,
) -> Result<TransitionOutcome> {
    ensure_schema(conn)?;
    match read_state(conn, quote_id, tenant_id)? {
        None => return Ok(TransitionOutcome::NotFound),
        Some(JobState::Pricing) => return Ok(TransitionOutcome::AlreadyInState),
        Some(_) => {}
    }
    let ts = now
        .format(&time::format_description::well_known::Rfc3339)
        .context("format updated_at")?;
    let tx = conn.transaction().context("open set_extracted tx")?;
    let rows = tx
        .execute(
            "UPDATE quote_pricing_jobs
                SET state = ?, updated_at = ?, feature_graph_hash = ?, feature_graph_json = ?
                WHERE quote_id = ? AND tenant_id = ?",
            params![
                STATE_PRICING,
                ts,
                feature_graph_hash,
                feature_graph_json,
                quote_id,
                tenant_id,
            ],
        )
        .context("set_extracted UPDATE")?;
    tx.commit().context("commit set_extracted")?;
    Ok(if rows == 0 {
        TransitionOutcome::NotFound
    } else {
        TransitionOutcome::Applied
    })
}

/// Stamp the priced breakdown + total, flip to `Rendering`.
///
/// **S286 hotfix**: SELECT-first; skip the UPDATE if already at `Rendering`.
pub fn set_priced(
    conn: &mut Connection,
    quote_id: &str,
    tenant_id: &str,
    breakdown_json: &str,
    total_price_eur: f64,
    now: OffsetDateTime,
) -> Result<TransitionOutcome> {
    ensure_schema(conn)?;
    match read_state(conn, quote_id, tenant_id)? {
        None => return Ok(TransitionOutcome::NotFound),
        Some(JobState::Rendering) => return Ok(TransitionOutcome::AlreadyInState),
        Some(_) => {}
    }
    let ts = now
        .format(&time::format_description::well_known::Rfc3339)
        .context("format updated_at")?;
    let tx = conn.transaction().context("open set_priced tx")?;
    let rows = tx
        .execute(
            "UPDATE quote_pricing_jobs
                SET state = ?, updated_at = ?, breakdown_json = ?, total_price_eur = ?
                WHERE quote_id = ? AND tenant_id = ?",
            params![
                STATE_RENDERING,
                ts,
                breakdown_json,
                total_price_eur,
                quote_id,
                tenant_id,
            ],
        )
        .context("set_priced UPDATE")?;
    tx.commit().context("commit set_priced")?;
    Ok(if rows == 0 {
        TransitionOutcome::NotFound
    } else {
        TransitionOutcome::Applied
    })
}

/// Stamp the PDF on-disk path + valid_until, flip to `PostingBack`.
///
/// **S286 hotfix**: SELECT-first; skip the UPDATE if already at `PostingBack`.
pub fn set_rendered(
    conn: &mut Connection,
    quote_id: &str,
    tenant_id: &str,
    pdf_path: &str,
    valid_until_iso: &str,
    now: OffsetDateTime,
) -> Result<TransitionOutcome> {
    ensure_schema(conn)?;
    match read_state(conn, quote_id, tenant_id)? {
        None => return Ok(TransitionOutcome::NotFound),
        Some(JobState::PostingBack) => return Ok(TransitionOutcome::AlreadyInState),
        Some(_) => {}
    }
    let ts = now
        .format(&time::format_description::well_known::Rfc3339)
        .context("format updated_at")?;
    let tx = conn.transaction().context("open set_rendered tx")?;
    let rows = tx
        .execute(
            "UPDATE quote_pricing_jobs
                SET state = ?, updated_at = ?, pdf_path = ?, valid_until_iso = ?
                WHERE quote_id = ? AND tenant_id = ?",
            params![
                STATE_POSTING_BACK,
                ts,
                pdf_path,
                valid_until_iso,
                quote_id,
                tenant_id,
            ],
        )
        .context("set_rendered UPDATE")?;
    tx.commit().context("commit set_rendered")?;
    Ok(if rows == 0 {
        TransitionOutcome::NotFound
    } else {
        TransitionOutcome::Applied
    })
}

/// Mark `Failed` with the stage + reason that broke the pipeline.
/// Truncates `reason` to 1000 chars (CR/LF/NUL stripped).
///
/// **S286 hotfix**: SELECT-first; skip the UPDATE if already at `Failed`.
/// (Daemon-side: a re-failure of an already-failed row is plausible during
/// supervisor recovery; treat as a no-op rather than re-emitting.)
///
/// **S290 / PR-271**: also stamps `failure_kind` (Transient / Permanent /
/// Unknown) so the daemon's `next_actionable_job` can skip Permanent rows
/// and cap Unknown auto-retries. The classifier runs in
/// [`crate::quote_pricing_pipeline::classify_failure`] before calling this.
pub fn set_failed(
    conn: &mut Connection,
    quote_id: &str,
    tenant_id: &str,
    stage: &str,
    reason: &str,
    failure_kind: FailureKind,
    now: OffsetDateTime,
) -> Result<TransitionOutcome> {
    ensure_schema(conn)?;
    match read_state(conn, quote_id, tenant_id)? {
        None => return Ok(TransitionOutcome::NotFound),
        Some(JobState::Failed) => return Ok(TransitionOutcome::AlreadyInState),
        Some(_) => {}
    }
    let ts = now
        .format(&time::format_description::well_known::Rfc3339)
        .context("format updated_at")?;
    let safe = sanitize_reason(reason);
    let tx = conn.transaction().context("open set_failed tx")?;
    let rows = tx
        .execute(
            "UPDATE quote_pricing_jobs
                SET state = ?, updated_at = ?, error_stage = ?, error_reason = ?,
                    failure_kind = ?
                WHERE quote_id = ? AND tenant_id = ?",
            params![
                STATE_FAILED,
                ts,
                stage,
                safe,
                failure_kind.as_str(),
                quote_id,
                tenant_id,
            ],
        )
        .context("set_failed UPDATE")?;
    tx.commit().context("commit set_failed")?;
    Ok(if rows == 0 {
        TransitionOutcome::NotFound
    } else {
        TransitionOutcome::Applied
    })
}

/// Operator retry — bump `attempt_n`, clear error fields, reset state
/// to `Fetched`. The daemon's next sweep picks it up again. Returns
/// the new `attempt_n` so the caller's audit-key suffix stays unique.
pub fn retry_job(
    conn: &mut Connection,
    quote_id: &str,
    tenant_id: &str,
    now: OffsetDateTime,
) -> Result<u32> {
    ensure_schema(conn)?;
    let ts = now
        .format(&time::format_description::well_known::Rfc3339)
        .context("format updated_at")?;
    let tx = conn.transaction().context("open retry_job tx")?;
    tx.execute(
        "UPDATE quote_pricing_jobs
            SET state = ?, updated_at = ?, error_stage = NULL, error_reason = NULL,
                failure_kind = NULL, attempt_n = attempt_n + 1
            WHERE quote_id = ? AND tenant_id = ? AND state = ?",
        params![STATE_FETCHED, ts, quote_id, tenant_id, STATE_FAILED],
    )
    .context("retry_job UPDATE")?;
    let new_n: i64 = tx
        .query_row(
            "SELECT attempt_n FROM quote_pricing_jobs
                WHERE quote_id = ? AND tenant_id = ?",
            params![quote_id, tenant_id],
            |r| r.get(0),
        )
        .context("read attempt_n")?;
    tx.commit().context("commit retry_job")?;
    Ok(new_n as u32)
}

/// S350 / PR-39 (U5) — outcome of an operator material-grade override.
/// The serve-layer handler maps each variant to an HTTP status: `Applied`
/// → 200, `NotFound` → 404, `NotEditable` → 409.
#[derive(Debug, Clone)]
pub enum MaterialEditOutcome {
    /// Grade rewritten, row reset to `Fetched`, `attempt_n` bumped.
    /// Carries the prior grade + state for the audit payload.
    Applied {
        old_grade: String,
        previous_state: JobState,
        new_attempt_n: u32,
    },
    /// No row for (tenant, quote_id) — 404.
    NotFound,
    /// Row exists but its current state forbids an edit — 409. Carries
    /// the offending state so the operator copy can name it.
    NotEditable { state: JobState },
}

/// S350 / PR-39 (U5) — operator material-grade override, the tx-owned
/// core. State-guarded: reads the row's current state + grade inside the
/// caller's transaction (one consistent snapshot — no TOCTOU against a
/// concurrent daemon transition), refuses (without mutating) when the
/// state is not [`JobState::material_editable`], otherwise rewrites
/// `material_grade`, resets the row to `Fetched` (re-enters the pricing
/// pipeline — the daemon's next sweep re-extracts/prices/renders/posts
/// with the new grade; S347/S348 own the writeback-retry path), bumps
/// `attempt_n`, and clears the error columns (the prior failure is
/// preserved in the audit ledger, not the row).
///
/// **Does NOT commit** — the serve-layer caller appends the
/// `quote.material_grade_edited` audit row in the SAME transaction and
/// commits both together (mutation + its audit-of-record are atomic).
/// **Catalogue membership is NOT validated here** — the caller validates
/// `new_grade` against `quoting_materials` first (400 + `available_count`
/// on a miss) so this fn owns only the DB transition + the editable-state
/// invariant. Tenant-scoped like every other writer. Caller guarantees
/// [`ensure_schema`] has run.
pub fn amend_material_grade_in_tx(
    tx: &duckdb::Transaction<'_>,
    quote_id: &str,
    tenant_id: &str,
    new_grade: &str,
    now: OffsetDateTime,
) -> Result<MaterialEditOutcome> {
    let current: Option<(String, String)> = {
        let mut stmt = tx
            .prepare(
                "SELECT state, material_grade FROM quote_pricing_jobs
                    WHERE quote_id = ? AND tenant_id = ?",
            )
            .context("prepare amend_material_grade read")?;
        let mut rows = stmt
            .query(params![quote_id, tenant_id])
            .context("execute amend_material_grade read")?;
        match rows.next().context("step amend_material_grade read")? {
            Some(r) => Some((
                r.get(0).context("get state")?,
                r.get(1).context("get material_grade")?,
            )),
            None => None,
        }
    };
    let Some((state_str, old_grade)) = current else {
        return Ok(MaterialEditOutcome::NotFound);
    };
    let state = JobState::parse_str(&state_str)?;
    if !state.material_editable() {
        return Ok(MaterialEditOutcome::NotEditable { state });
    }
    let ts = now
        .format(&time::format_description::well_known::Rfc3339)
        .context("format updated_at")?;
    tx.execute(
        "UPDATE quote_pricing_jobs
            SET material_grade = ?, state = ?, updated_at = ?,
                error_stage = NULL, error_reason = NULL, failure_kind = NULL,
                attempt_n = attempt_n + 1
            WHERE quote_id = ? AND tenant_id = ?",
        params![new_grade, STATE_FETCHED, ts, quote_id, tenant_id],
    )
    .context("amend_material_grade UPDATE")?;
    let new_n: i64 = tx
        .query_row(
            "SELECT attempt_n FROM quote_pricing_jobs
                WHERE quote_id = ? AND tenant_id = ?",
            params![quote_id, tenant_id],
            |r| r.get(0),
        )
        .context("read attempt_n after amend")?;
    Ok(MaterialEditOutcome::Applied {
        old_grade,
        previous_state: state,
        new_attempt_n: new_n.max(0) as u32,
    })
}

/// S391/F + S414 — outcome of an operator Delete of a Failed pricing-job
/// row. The serve-layer handler maps each variant to an HTTP status:
/// `Archived` → 200, `NotFound` → 404, `NotDeletable` → 409.
#[derive(Debug, Clone)]
pub enum DeleteJobOutcome {
    /// S414 — row flipped to `archived` (NOT hard-deleted). Carries the
    /// terminal failure context for the audit payload. Archiving rather
    /// than DELETEing keeps the `quote_id` row present so the daemon's
    /// `ON CONFLICT` re-enqueue guard keeps skipping the quote_id instead
    /// of re-pulling + re-inserting it (and re-`warn!`-ing) every poll
    /// cycle — see [`STATE_ARCHIVED`].
    Archived {
        attempt_n: u32,
        error_stage: Option<String>,
        error_reason: Option<String>,
        failure_kind: Option<String>,
    },
    /// No row for (tenant, quote_id) — 404.
    NotFound,
    /// Row exists but its current state is not `Failed` — 409. Carries the
    /// offending state so the operator copy can name it. Conservative: an
    /// in-flight (or Posted) row is never archived out from under the
    /// daemon.
    NotDeletable { state: JobState },
}

/// S391/F + S414 — operator Delete of a permanently-Failed pricing-job
/// row, the tx-owned core. State-guarded: reads the row's current state +
/// terminal failure context inside the caller's transaction (one
/// consistent snapshot — no TOCTOU against a concurrent daemon
/// transition), refuses (without mutating) when the state is not
/// [`JobState::Failed`], otherwise flips the row to [`STATE_ARCHIVED`]
/// (S414 — soft-delete, NOT a hard DELETE) and returns its `attempt_n` +
/// error columns for the audit payload.
///
/// **Why archive, not DELETE (S414/S414b):** a hard DELETE removed the
/// `quote_id` row, so on the next 60-90s poll the daemon re-pulled the
/// quote from the storefront `?status=received` listing, re-inserted it
/// (no PK conflict — the row was gone), and `warn!`-ed the
/// "no CAD file ... operator Retry required" line again. The phantom row
/// "crept back" on refresh and re-WARNed every cycle. Keeping the row as
/// `archived` makes `insert_fetched_job` / `insert_failed_enqueue_job`'s
/// `ON CONFLICT (quote_id) DO NOTHING` a no-op, so the re-enqueue is
/// silent and the panel stays clear — one-click final disposition.
///
/// **Does NOT commit** — the serve-layer caller appends the
/// `quote.pricing_failure_deleted` audit row in the SAME transaction and
/// commits both together (the archive and its audit-of-record are
/// atomic). Tenant-scoped like every other writer. Caller guarantees
/// [`ensure_schema`] has run.
pub fn delete_failed_job_in_tx(
    tx: &duckdb::Transaction<'_>,
    quote_id: &str,
    tenant_id: &str,
    now: OffsetDateTime,
) -> Result<DeleteJobOutcome> {
    let current: Option<(String, i64, Option<String>, Option<String>, Option<String>)> = {
        let mut stmt = tx
            .prepare(
                "SELECT state, attempt_n, error_stage, error_reason, failure_kind
                    FROM quote_pricing_jobs
                    WHERE quote_id = ? AND tenant_id = ?",
            )
            .context("prepare delete_failed_job read")?;
        let mut rows = stmt
            .query(params![quote_id, tenant_id])
            .context("execute delete_failed_job read")?;
        match rows.next().context("step delete_failed_job read")? {
            Some(r) => Some((
                r.get(0).context("get state")?,
                r.get(1).context("get attempt_n")?,
                r.get(2).context("get error_stage")?,
                r.get(3).context("get error_reason")?,
                r.get(4).context("get failure_kind")?,
            )),
            None => None,
        }
    };
    let Some((state_str, attempt_n, error_stage, error_reason, failure_kind)) = current else {
        return Ok(DeleteJobOutcome::NotFound);
    };
    let state = JobState::parse_str(&state_str)?;
    if state != JobState::Failed {
        return Ok(DeleteJobOutcome::NotDeletable { state });
    }
    // S414 — soft-delete: flip `failed` → `archived` rather than DELETE, so
    // the `quote_id` row survives and the daemon's `ON CONFLICT` re-enqueue
    // guard keeps skipping it (no re-pull, no re-`warn!`). The `state =
    // 'failed'` predicate makes this a real value change (never a no-op
    // UPDATE-to-same-value — the S286 DuckDB-FATAL class is avoided), and
    // the (tenant_id, state) secondary index was dropped in S286 so the
    // UPDATE-on-`state` is index-safe.
    let ts = now
        .format(&time::format_description::well_known::Rfc3339)
        .context("format archived_at")?;
    tx.execute(
        "UPDATE quote_pricing_jobs SET state = ?, updated_at = ?
            WHERE quote_id = ? AND tenant_id = ? AND state = ?",
        params![STATE_ARCHIVED, ts, quote_id, tenant_id, STATE_FAILED],
    )
    .context("archive_failed_job UPDATE")?;
    Ok(DeleteJobOutcome::Archived {
        attempt_n: attempt_n.max(0) as u32,
        error_stage,
        error_reason,
        failure_kind,
    })
}

/// S350 / PR-39 (U5) — `&mut Connection` wrapper over
/// [`amend_material_grade_in_tx`] for unit tests + any non-audit caller:
/// runs [`ensure_schema`], opens a tx, applies the edit, and commits
/// (a no-op commit on the NotFound / NotEditable paths, which wrote
/// nothing). The serve handler does NOT use this — it needs the
/// in-tx variant so the audit row rides the same transaction.
pub fn amend_material_grade(
    conn: &mut Connection,
    quote_id: &str,
    tenant_id: &str,
    new_grade: &str,
    now: OffsetDateTime,
) -> Result<MaterialEditOutcome> {
    ensure_schema(conn)?;
    let tx = conn.transaction().context("open amend_material_grade tx")?;
    let outcome = amend_material_grade_in_tx(&tx, quote_id, tenant_id, new_grade, now)?;
    tx.commit().context("commit amend_material_grade")?;
    Ok(outcome)
}

/// SPA + daemon read path. Returns rows newest-first.
pub fn list_jobs(conn: &Connection, tenant_id: &str) -> Result<Vec<PricingJobRow>> {
    ensure_schema(conn)?;
    let mut stmt = conn
        .prepare(
            // S401 — customer_company appended LAST (ordinal 15) so every
            // existing column ordinal stays put; the read below picks it up
            // at .get(15).
            // S414 — `archived` rows are operator-dispositioned terminal
            // rows kept only to anchor the daemon's `ON CONFLICT`
            // re-enqueue guard; they are excluded from the operator panel.
            "SELECT quote_id, tenant_id, state, fetched_at, updated_at,
                    customer_email, customer_name, material_grade, quantity,
                    feature_graph_hash, total_price_eur, error_stage, error_reason,
                    attempt_n, failure_kind, customer_company
                FROM quote_pricing_jobs
                WHERE tenant_id = ?
                  AND state != 'archived'
                ORDER BY fetched_at DESC",
        )
        .context("prepare list_jobs")?;
    let mut rows = stmt
        .query(params![tenant_id])
        .context("execute list_jobs")?;
    let mut out: Vec<PricingJobRow> = Vec::new();
    while let Some(r) = rows.next().context("step list_jobs")? {
        let state_str: String = r.get(2).context("get state")?;
        let state = JobState::parse_str(&state_str)?;
        let qty: i64 = r.get(8).context("get quantity")?;
        let attempt_n: i64 = r.get(13).context("get attempt_n")?;
        let failure_kind = match r.get::<_, Option<String>>(14).ok().flatten() {
            Some(s) => Some(FailureKind::parse_str(&s)?),
            None => None,
        };
        out.push(PricingJobRow {
            quote_id: r.get(0).context("get quote_id")?,
            tenant_id: r.get(1).context("get tenant_id")?,
            state,
            fetched_at: r.get(3).context("get fetched_at")?,
            updated_at: r.get(4).context("get updated_at")?,
            customer_email: r.get(5).context("get customer_email")?,
            customer_name: r.get(6).context("get customer_name")?,
            customer_company: r.get(15).ok(),
            material_grade: r.get(7).context("get material_grade")?,
            quantity: qty.max(0) as u32,
            feature_graph_hash: r.get(9).ok(),
            total_price_eur: r.get(10).ok(),
            error_stage: r.get(11).ok(),
            error_reason: r.get(12).ok(),
            attempt_n: attempt_n.max(0) as u32,
            failure_kind,
        });
    }
    Ok(out)
}

/// Daemon read path — find the oldest non-terminal job to advance.
/// Returns `None` if the queue is empty.
///
/// **S290 / PR-271**: Failed rows are SKIPPED by the daemon entirely —
/// the operator's Retry click is the only way to re-enqueue. The brief's
/// failure-kind classifier already gates that decision at write time:
/// `Permanent` failures stay Failed pending operator action; `Transient`
/// and `Unknown` are also frozen at Failed here (the daemon's S279 design
/// already required operator retry — this query never returned Failed
/// rows). The classifier's value shows up in the SPA badge + audit row,
/// not in the daemon scheduler.
///
/// Auto-retry of Transient failures is deliberately NOT wired in this
/// PR: the operator's audit-visible Retry click is the durable record;
/// silent auto-retry of a network blip would hide the failure from the
/// SPA and lose the per-attempt audit chain.
pub fn next_actionable_job(conn: &Connection, tenant_id: &str) -> Result<Option<PricingJobRow>> {
    ensure_schema(conn)?;
    let mut stmt = conn
        .prepare(
            // S401 — customer_company appended LAST (ordinal 15); see list_jobs.
            "SELECT quote_id, tenant_id, state, fetched_at, updated_at,
                    customer_email, customer_name, material_grade, quantity,
                    feature_graph_hash, total_price_eur, error_stage, error_reason,
                    attempt_n, failure_kind, customer_company
                FROM quote_pricing_jobs
                WHERE tenant_id = ?
                  AND state IN ('fetched','extracting','pricing','rendering','posting_back')
                ORDER BY fetched_at ASC
                LIMIT 1",
        )
        .context("prepare next_actionable_job")?;
    let mut rows = stmt
        .query(params![tenant_id])
        .context("execute next_actionable_job")?;
    if let Some(r) = rows.next().context("step next_actionable_job")? {
        let state_str: String = r.get(2).context("get state")?;
        let state = JobState::parse_str(&state_str)?;
        let qty: i64 = r.get(8).context("get quantity")?;
        let attempt_n: i64 = r.get(13).context("get attempt_n")?;
        let failure_kind = match r.get::<_, Option<String>>(14).ok().flatten() {
            Some(s) => Some(FailureKind::parse_str(&s)?),
            None => None,
        };
        Ok(Some(PricingJobRow {
            quote_id: r.get(0).context("get quote_id")?,
            tenant_id: r.get(1).context("get tenant_id")?,
            state,
            fetched_at: r.get(3).context("get fetched_at")?,
            updated_at: r.get(4).context("get updated_at")?,
            customer_email: r.get(5).context("get customer_email")?,
            customer_name: r.get(6).context("get customer_name")?,
            customer_company: r.get(15).ok(),
            material_grade: r.get(7).context("get material_grade")?,
            quantity: qty.max(0) as u32,
            feature_graph_hash: r.get(9).ok(),
            total_price_eur: r.get(10).ok(),
            error_stage: r.get(11).ok(),
            error_reason: r.get(12).ok(),
            attempt_n: attempt_n.max(0) as u32,
            failure_kind,
        }))
    } else {
        Ok(None)
    }
}

/// Read FeatureGraph JSON + breakdown JSON + CAD path for a single
/// row. Used by the pipeline when advancing past `Extracting`.
pub fn get_job_artifacts(
    conn: &Connection,
    quote_id: &str,
    tenant_id: &str,
) -> Result<JobArtifacts> {
    let mut stmt = conn
        .prepare(
            "SELECT cad_local_path, feature_graph_json, breakdown_json, pdf_path
                FROM quote_pricing_jobs
                WHERE quote_id = ? AND tenant_id = ?",
        )
        .context("prepare get_job_artifacts")?;
    let mut rows = stmt
        .query(params![quote_id, tenant_id])
        .context("execute get_job_artifacts")?;
    if let Some(r) = rows.next().context("step get_job_artifacts")? {
        Ok(JobArtifacts {
            cad_local_path: r.get(0).context("get cad_local_path")?,
            feature_graph_json: r.get(1).ok(),
            breakdown_json: r.get(2).ok(),
            pdf_path: r.get(3).ok(),
        })
    } else {
        Err(anyhow!("no quote_pricing_jobs row for {quote_id}"))
    }
}

/// Artifacts attached to one job row. The fields go `Some(_)` as the
/// pipeline advances; reading from a `Failed` row gives whichever
/// state the failure happened at.
#[derive(Debug, Clone)]
pub struct JobArtifacts {
    pub cad_local_path: String,
    pub feature_graph_json: Option<String>,
    pub breakdown_json: Option<String>,
    pub pdf_path: Option<String>,
}

/// S349 / PR-40 (U1) — full single-row read for the operator detail
/// panel. One SELECT that carries everything the list row has
/// ([`PricingJobRow`]) plus the artifact columns the list deliberately
/// omits (FeatureGraph JSON, pricing breakdown JSON, CAD filename, PDF
/// path, valid-until). Returns `None` when no row matches so the route
/// handler can map it to a 404 (rather than `get_job_artifacts`'s
/// `Err`, which is the daemon's "this should exist mid-pipeline" path).
///
/// `cad_local_path` / `pdf_path` are server-filesystem paths and are
/// NOT surfaced to the SPA — the wire view exposes only the filename
/// and presence booleans (no fs-layout leak, no unauthenticated
/// download URL in v1). See `serve::PricingJobDetailView`.
#[derive(Debug, Clone)]
pub struct JobDetail {
    pub row: PricingJobRow,
    pub cad_filename: String,
    pub feature_graph_json: Option<String>,
    pub breakdown_json: Option<String>,
    pub pdf_path: Option<String>,
    pub valid_until_iso: Option<String>,
    /// S427 — engine-computed lead-time (calendar days), NULL pre-pricing.
    pub lead_time_days: Option<u32>,
    /// S427 — operator override (calendar days), NULL = use computed.
    pub lead_time_override_days: Option<u32>,
}

/// S349 / PR-40 (U1) — read one job row + its artifacts for the detail
/// panel. Tenant-scoped like every other reader. `Ok(None)` on a
/// missing row (→ 404 at the route).
pub fn get_job_detail(
    conn: &Connection,
    quote_id: &str,
    tenant_id: &str,
) -> Result<Option<JobDetail>> {
    ensure_schema(conn)?;
    let mut stmt = conn
        .prepare(
            // S401 — customer_company appended LAST (ordinal 20) so the
            // artifact ordinals 15-19 below stay put.
            "SELECT quote_id, tenant_id, state, fetched_at, updated_at,
                    customer_email, customer_name, material_grade, quantity,
                    feature_graph_hash, total_price_eur, error_stage, error_reason,
                    attempt_n, failure_kind,
                    cad_filename, feature_graph_json, breakdown_json, pdf_path,
                    valid_until_iso, customer_company,
                    lead_time_days, lead_time_override_days
                FROM quote_pricing_jobs
                WHERE quote_id = ? AND tenant_id = ?",
        )
        .context("prepare get_job_detail")?;
    let mut rows = stmt
        .query(params![quote_id, tenant_id])
        .context("execute get_job_detail")?;
    let Some(r) = rows.next().context("step get_job_detail")? else {
        return Ok(None);
    };
    let state_str: String = r.get(2).context("get state")?;
    let state = JobState::parse_str(&state_str)?;
    let qty: i64 = r.get(8).context("get quantity")?;
    let attempt_n: i64 = r.get(13).context("get attempt_n")?;
    let failure_kind = match r.get::<_, Option<String>>(14).ok().flatten() {
        Some(s) => Some(FailureKind::parse_str(&s)?),
        None => None,
    };
    let row = PricingJobRow {
        quote_id: r.get(0).context("get quote_id")?,
        tenant_id: r.get(1).context("get tenant_id")?,
        state,
        fetched_at: r.get(3).context("get fetched_at")?,
        updated_at: r.get(4).context("get updated_at")?,
        customer_email: r.get(5).context("get customer_email")?,
        customer_name: r.get(6).context("get customer_name")?,
        customer_company: r.get(20).ok(),
        material_grade: r.get(7).context("get material_grade")?,
        quantity: qty.max(0) as u32,
        feature_graph_hash: r.get(9).ok(),
        total_price_eur: r.get(10).ok(),
        error_stage: r.get(11).ok(),
        error_reason: r.get(12).ok(),
        attempt_n: attempt_n.max(0) as u32,
        failure_kind,
    };
    Ok(Some(JobDetail {
        row,
        cad_filename: r.get(15).context("get cad_filename")?,
        feature_graph_json: r.get(16).ok(),
        breakdown_json: r.get(17).ok(),
        pdf_path: r.get(18).ok(),
        valid_until_iso: r.get(19).ok(),
        lead_time_days: r
            .get::<_, Option<i64>>(21)
            .ok()
            .flatten()
            .map(|v| v.max(0) as u32),
        lead_time_override_days: r
            .get::<_, Option<i64>>(22)
            .ok()
            .flatten()
            .map(|v| v.max(0) as u32),
    }))
}

/// S427 — the effective lead-time for a job: the operator override if
/// set, else the engine-computed value, else `None` (not yet priced).
/// This is what the customer-facing PDF banner renders.
pub fn get_effective_lead_time_days(
    conn: &Connection,
    quote_id: &str,
    tenant_id: &str,
) -> Result<Option<u32>> {
    ensure_schema(conn)?;
    let mut stmt = conn
        .prepare(
            "SELECT lead_time_override_days, lead_time_days FROM quote_pricing_jobs \
             WHERE quote_id = ? AND tenant_id = ?",
        )
        .context("prepare get_effective_lead_time")?;
    let mut rows = stmt.query(params![quote_id, tenant_id])?;
    let Some(r) = rows.next()? else {
        return Ok(None);
    };
    let override_days: Option<i64> = r.get(0).ok();
    let computed: Option<i64> = r.get(1).ok();
    Ok(override_days.or(computed).map(|v| v.max(0) as u32))
}

/// S427 — stamp the engine-computed lead-time (calendar days) on a job
/// row. Separate from [`set_priced`] so the high-traffic state-flip path
/// keeps its signature and call sites; this is a small targeted UPDATE
/// run right after pricing succeeds.
pub fn set_computed_lead_time(
    conn: &Connection,
    quote_id: &str,
    tenant_id: &str,
    days: u32,
) -> Result<()> {
    ensure_schema(conn)?;
    conn.execute(
        "UPDATE quote_pricing_jobs SET lead_time_days = ? WHERE quote_id = ? AND tenant_id = ?",
        params![days as i64, quote_id, tenant_id],
    )
    .context("set_computed_lead_time UPDATE")?;
    Ok(())
}

/// S427 — set or clear the operator's manual lead-time override. `None`
/// clears it (reverts to the computed value). Returns `false` if no such
/// row exists (→ 404 at the route).
pub fn set_lead_time_override(
    conn: &Connection,
    quote_id: &str,
    tenant_id: &str,
    override_days: Option<u32>,
    now: OffsetDateTime,
) -> Result<bool> {
    ensure_schema(conn)?;
    let ts = now
        .format(&time::format_description::well_known::Rfc3339)
        .context("format override updated_at")?;
    let changed = conn
        .execute(
            "UPDATE quote_pricing_jobs SET lead_time_override_days = ?, updated_at = ? \
             WHERE quote_id = ? AND tenant_id = ?",
            params![override_days.map(|d| d as i64), ts, quote_id, tenant_id],
        )
        .context("set_lead_time_override UPDATE")?;
    Ok(changed > 0)
}

/// S427 — sum committed/pending machining hours by machine family across
/// the shop, for the capacity-aware lead-time model. The authoritative
/// shop-load ledger is THIS table: a quote that the customer accepted
/// (now a work order) stays in `Posted` (the state machine has no
/// `accepted` state), so summing `Posted` priced jobs in the window
/// captures BOTH open-WO load and still-pending posted quotes in one
/// honest query — the `work_orders` table carries neither machining
/// hours nor a machine family, so it cannot supply this.
///
/// `since_rfc3339` is the 30-day cutoff (Rfc3339 string compares
/// chronologically because all timestamps are UTC `…Z`). `exclude` skips
/// the quote currently being priced. Family comes from each row's stored
/// `route_to_5_axis` (3-axis vs 5-axis — the only signal the extractor
/// produces today).
///
/// `machining_minutes` in the breakdown is **per part**, so each job's
/// shop-time footprint is `machining_minutes × quantity` — the batch
/// occupies the machine for every part, not just one.
pub fn sum_posted_machining_hours_by_family(
    conn: &Connection,
    tenant_id: &str,
    since_rfc3339: &str,
    exclude_quote_id: &str,
) -> Result<std::collections::BTreeMap<aberp_quote_engine::MachineFamily, f64>> {
    use aberp_quote_engine::MachineFamily;
    ensure_schema(conn)?;

    #[derive(serde::Deserialize)]
    struct LoadRow {
        #[serde(default)]
        machining_minutes: f64,
        #[serde(default)]
        route_to_5_axis: bool,
    }

    let mut stmt = conn
        .prepare(
            "SELECT breakdown_json, quantity FROM quote_pricing_jobs \
             WHERE tenant_id = ? AND state = ? AND breakdown_json IS NOT NULL \
               AND quote_id != ? AND fetched_at >= ?",
        )
        .context("prepare sum_posted_machining_hours")?;
    let rows = stmt
        .query_map(
            params![tenant_id, STATE_POSTED, exclude_quote_id, since_rfc3339],
            |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, i64>(1)?)),
        )
        .context("query sum_posted_machining_hours")?;

    let mut out: std::collections::BTreeMap<MachineFamily, f64> = std::collections::BTreeMap::new();
    for r in rows {
        let (json, qty) = r?;
        let Some(json) = json else { continue };
        let Ok(lr) = serde_json::from_str::<LoadRow>(&json) else {
            // A breakdown we can't parse contributes nothing rather than
            // crashing pricing; the row is still counted in nothing.
            continue;
        };
        let hours = (lr.machining_minutes / 60.0 * (qty.max(0) as f64)).max(0.0);
        *out.entry(MachineFamily::for_route(lr.route_to_5_axis))
            .or_insert(0.0) += hours;
    }
    Ok(out)
}

/// Trim CR/LF/NUL out of a free-text error reason and truncate to
/// 1000 chars. The reason rides into the SPA error column +
/// audit-payload `reason` field; we never want header-injection chars
/// to leak into a log line.
fn sanitize_reason(reason: &str) -> String {
    let cleaned: String = reason
        .chars()
        .filter(|c| !matches!(*c, '\r' | '\n' | '\0'))
        .collect();
    if cleaned.chars().count() <= 1000 {
        cleaned
    } else {
        cleaned.chars().take(1000).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use duckdb::Connection;
    use time::OffsetDateTime;

    fn open_mem() -> Connection {
        let conn = Connection::open_in_memory().expect("open mem");
        ensure_schema(&conn).expect("schema");
        conn
    }

    /// S288 / PR-269 — open an in-memory DuckDB WITHOUT running
    /// `ensure_schema`. Tests that need to simulate the pre-PR-268
    /// prod schema (where the orphan secondary index existed) must
    /// avoid `ensure_schema`'s SCHEMA_SQL `DROP INDEX IF EXISTS`.
    fn open_bare_mem() -> Connection {
        Connection::open_in_memory().expect("open bare mem")
    }

    fn fixed_ts() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_750_000_000).unwrap()
    }

    #[test]
    fn ensure_schema_is_idempotent() {
        let conn = open_mem();
        ensure_schema(&conn).expect("re-apply");
        ensure_schema(&conn).expect("third");
    }

    #[test]
    fn insert_then_advance_through_states() {
        let mut conn = open_mem();
        let inserted = insert_fetched_job(
            &conn,
            "q1",
            "T",
            "alice@example.com",
            "Alice",
            "",
            "AL_6061_T6",
            10,
            "cube.stl",
            "/tmp/cube.stl",
            fixed_ts(),
        )
        .expect("insert");
        assert!(inserted);

        // Re-insert same quote_id → no-op (idempotent).
        let inserted_again = insert_fetched_job(
            &conn,
            "q1",
            "T",
            "alice@example.com",
            "Alice",
            "",
            "AL_6061_T6",
            10,
            "cube.stl",
            "/tmp/cube.stl",
            fixed_ts(),
        )
        .expect("re-insert");
        assert!(!inserted_again);

        assert_eq!(
            set_state(&conn, "q1", "T", JobState::Extracting, fixed_ts()).expect("set Extracting"),
            TransitionOutcome::Applied
        );
        assert_eq!(
            set_extracted(&mut conn, "q1", "T", "blake3:dead", "{}", fixed_ts())
                .expect("extracted"),
            TransitionOutcome::Applied
        );
        assert_eq!(
            set_priced(&mut conn, "q1", "T", "{\"k\":1}", 42.0, fixed_ts()).expect("priced"),
            TransitionOutcome::Applied
        );
        assert_eq!(
            set_rendered(
                &mut conn,
                "q1",
                "T",
                "/tmp/q1/priced.pdf",
                "2026-07-06",
                fixed_ts(),
            )
            .expect("rendered"),
            TransitionOutcome::Applied
        );
        assert_eq!(
            set_state(&conn, "q1", "T", JobState::Posted, fixed_ts()).expect("posted"),
            TransitionOutcome::Applied
        );

        let rows = list_jobs(&conn, "T").expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, JobState::Posted);
        assert_eq!(rows[0].feature_graph_hash.as_deref(), Some("blake3:dead"));
        assert_eq!(rows[0].total_price_eur, Some(42.0));
        assert_eq!(rows[0].quantity, 10);
    }

    /// S401 — the buyer's company round-trips insert → list_jobs →
    /// get_job_detail. This is the data-layer half of the operator-panel
    /// ask: if company is dropped at the staging layer (as it was before
    /// S401), this test fails because both reads return None / "".
    #[test]
    fn s401_company_round_trips_through_insert_and_reads() {
        let conn = open_mem();
        let inserted = insert_fetched_job(
            &conn,
            "qco",
            "T",
            "ervin@aben.ch",
            "Ervin Csengeri",
            "Acme Manufacturing Kft.",
            "AL_6061_T6",
            3,
            "bracket.step",
            "/tmp/bracket.step",
            fixed_ts(),
        )
        .expect("insert");
        assert!(inserted);

        let rows = list_jobs(&conn, "T").expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].customer_company.as_deref(),
            Some("Acme Manufacturing Kft."),
            "list_jobs must carry the staged company"
        );

        let detail = get_job_detail(&conn, "qco", "T")
            .expect("detail query")
            .expect("row present");
        assert_eq!(
            detail.row.customer_company.as_deref(),
            Some("Acme Manufacturing Kft."),
            "get_job_detail must carry the staged company"
        );
    }

    /// S401 — a buyer who left the company field blank stages `""` (not
    /// NULL); the reads surface `Some("")` so the SPA's placeholder branch
    /// (empty-after-trim) fires rather than the legacy-NULL branch.
    #[test]
    fn s401_blank_company_stages_empty_string_not_null() {
        let conn = open_mem();
        insert_fetched_job(
            &conn,
            "qblank",
            "T",
            "anon@example.com",
            "Anon Buyer",
            "",
            "SS_304",
            1,
            "part.step",
            "/tmp/part.step",
            fixed_ts(),
        )
        .expect("insert");
        let rows = list_jobs(&conn, "T").expect("list");
        assert_eq!(rows[0].customer_company.as_deref(), Some(""));
    }

    /// S401 — a legacy PROD_v2.27.[0-55] row that pre-dates the
    /// customer_company column reads back `None` (the column is NULL after
    /// the additive migration). The SPA renders the placeholder for it.
    /// Simulated by inserting a row WITHOUT the new column.
    #[test]
    fn s401_legacy_row_without_company_reads_none() {
        let conn = open_mem();
        let ts = fixed_ts()
            .format(&time::format_description::well_known::Rfc3339)
            .expect("ts");
        // Insert omitting customer_company → column defaults to NULL (the
        // post-migration state of a pre-S401 row).
        conn.execute(
            "INSERT INTO quote_pricing_jobs (
                quote_id, tenant_id, state, fetched_at, updated_at,
                customer_email, customer_name, material_grade, quantity,
                cad_filename, cad_local_path, attempt_n
            ) VALUES ('qleg', 'T', 'fetched', ?, ?, 'old@example.com',
                      'Legacy Buyer', 'AL_6061_T6', 5, 'old.step', '/tmp/old.step', 0)",
            params![ts, ts],
        )
        .expect("legacy insert");
        let rows = list_jobs(&conn, "T").expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].customer_company, None,
            "legacy NULL company must read back as None"
        );
    }

    // ── S379 / PR-379 — listing-level no-CAD permanent enqueue failure ──

    /// `insert_failed_enqueue_job` writes a terminal `Failed` row with the
    /// enqueue-stage failure columns + placeholder CAD fields, and is
    /// idempotent: the second call (same quote_id) is a no-op — exactly the
    /// ON CONFLICT guard that stops the S376 phantom-retry loop from
    /// re-inserting every 60s cycle.
    #[test]
    fn s379_insert_failed_enqueue_job_writes_shape_and_is_idempotent() {
        let conn = open_mem();
        let first = insert_failed_enqueue_job(
            &conn,
            "phantom-1",
            "T",
            "phantom@example.com",
            "Phantom Customer",
            "",
            "6061-T6",
            2,
            "enqueue",
            "no CAD file on listing",
            FailureKind::Permanent,
            fixed_ts(),
        )
        .expect("first insert");
        assert!(first, "first call inserts a fresh Failed row");

        let rows = list_jobs(&conn, "T").expect("list");
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.state, JobState::Failed);
        assert_eq!(row.error_stage.as_deref(), Some("enqueue"));
        assert_eq!(row.error_reason.as_deref(), Some("no CAD file on listing"));
        assert_eq!(row.failure_kind, Some(FailureKind::Permanent));
        assert_eq!(row.material_grade, "6061-T6");
        assert_eq!(row.quantity, 2);

        // Second cycle: same quote_id → no-op, still exactly one row.
        let second = insert_failed_enqueue_job(
            &conn,
            "phantom-1",
            "T",
            "phantom@example.com",
            "Phantom Customer",
            "",
            "6061-T6",
            2,
            "enqueue",
            "no CAD file on listing",
            FailureKind::Permanent,
            fixed_ts(),
        )
        .expect("second insert");
        assert!(!second, "second call is an idempotent no-op");
        assert_eq!(list_jobs(&conn, "T").expect("list2").len(), 1);
    }

    /// Conservative path: once a no-CAD `Failed` row exists, a later cycle
    /// where the storefront listing DOES carry a CAD must NOT auto-reset the
    /// row. `insert_fetched_job`'s ON CONFLICT guard returns `false` and
    /// leaves the row terminal — the operator must explicitly Retry.
    #[test]
    fn s379_existing_failed_row_is_not_auto_reset_by_later_cad() {
        let conn = open_mem();
        insert_failed_enqueue_job(
            &conn,
            "phantom-2",
            "T",
            "phantom@example.com",
            "Phantom Customer",
            "",
            "6061-T6",
            1,
            "enqueue",
            "no CAD file on listing",
            FailureKind::Permanent,
            fixed_ts(),
        )
        .expect("seed failed row");

        // Later cycle: storefront now returns a CAD → enqueue's DB write is
        // `insert_fetched_job`. It must be blocked by the existing row.
        let inserted = insert_fetched_job(
            &conn,
            "phantom-2",
            "T",
            "phantom@example.com",
            "Phantom Customer",
            "",
            "6061-T6",
            1,
            "now-present.stl",
            "/tmp/now-present.stl",
            fixed_ts(),
        )
        .expect("re-enqueue attempt");
        assert!(!inserted, "pre-existing row blocks re-enqueue");

        let rows = list_jobs(&conn, "T").expect("list");
        assert_eq!(rows.len(), 1);
        // Row is untouched — still terminal Failed with the no-CAD reason,
        // NOT silently reset to Fetched.
        assert_eq!(rows[0].state, JobState::Failed);
        assert_eq!(
            rows[0].error_reason.as_deref(),
            Some("no CAD file on listing")
        );
        assert_eq!(rows[0].failure_kind, Some(FailureKind::Permanent));
    }

    #[test]
    fn set_failed_marks_terminal_and_carries_reason() {
        let mut conn = open_mem();
        insert_fetched_job(
            &conn,
            "q2",
            "T",
            "b@x",
            "Bob",
            "",
            "AL",
            1,
            "p.stl",
            "/tmp/p.stl",
            fixed_ts(),
        )
        .expect("ins");
        set_state(&conn, "q2", "T", JobState::Extracting, fixed_ts()).expect("ex");
        assert_eq!(
            set_failed(
                &mut conn,
                "q2",
                "T",
                "extract",
                "OCCT crash",
                FailureKind::Transient,
                fixed_ts(),
            )
            .expect("fail"),
            TransitionOutcome::Applied
        );
        let rows = list_jobs(&conn, "T").expect("list");
        assert_eq!(rows[0].state, JobState::Failed);
        assert_eq!(rows[0].error_stage.as_deref(), Some("extract"));
        assert_eq!(rows[0].error_reason.as_deref(), Some("OCCT crash"));
        assert_eq!(rows[0].attempt_n, 0);
        assert_eq!(rows[0].failure_kind, Some(FailureKind::Transient));
    }

    #[test]
    fn retry_bumps_attempt_and_resets_state() {
        let mut conn = open_mem();
        insert_fetched_job(
            &conn,
            "q3",
            "T",
            "b@x",
            "Bob",
            "",
            "AL",
            1,
            "p.stl",
            "/tmp/p.stl",
            fixed_ts(),
        )
        .expect("ins");
        set_failed(
            &mut conn,
            "q3",
            "T",
            "post",
            "503",
            FailureKind::Transient,
            fixed_ts(),
        )
        .expect("fail");
        let n1 = retry_job(&mut conn, "q3", "T", fixed_ts()).expect("retry1");
        assert_eq!(n1, 1);
        let rows = list_jobs(&conn, "T").expect("list");
        assert_eq!(rows[0].state, JobState::Fetched);
        assert!(rows[0].error_stage.is_none());
        assert_eq!(rows[0].attempt_n, 1);
        // retry_job must also clear failure_kind so the daemon's next-cycle
        // pickup at Fetched doesn't carry stale verdict metadata.
        assert!(rows[0].failure_kind.is_none());
        // Re-fail + re-retry → attempt 2.
        set_failed(
            &mut conn,
            "q3",
            "T",
            "post",
            "again",
            FailureKind::Transient,
            fixed_ts(),
        )
        .expect("fail2");
        let n2 = retry_job(&mut conn, "q3", "T", fixed_ts()).expect("retry2");
        assert_eq!(n2, 2);
    }

    #[test]
    fn retry_is_a_noop_when_state_is_not_failed() {
        let mut conn = open_mem();
        insert_fetched_job(
            &conn,
            "q4",
            "T",
            "b@x",
            "Bob",
            "",
            "AL",
            1,
            "p.stl",
            "/tmp/p.stl",
            fixed_ts(),
        )
        .expect("ins");
        // No `Failed` state — retry must NOT advance.
        let n = retry_job(&mut conn, "q4", "T", fixed_ts()).expect("retry");
        // attempt_n stays 0 because the WHERE filter didn't match.
        assert_eq!(n, 0);
        let rows = list_jobs(&conn, "T").expect("list");
        assert_eq!(rows[0].state, JobState::Fetched);
    }

    #[test]
    fn next_actionable_skips_terminal_states() {
        let mut conn = open_mem();
        insert_fetched_job(
            &conn,
            "a",
            "T",
            "b@x",
            "Bob",
            "",
            "AL",
            1,
            "p.stl",
            "/tmp/p.stl",
            fixed_ts(),
        )
        .expect("a");
        insert_fetched_job(
            &conn,
            "b",
            "T",
            "b@x",
            "Bob",
            "",
            "AL",
            1,
            "p.stl",
            "/tmp/p.stl",
            fixed_ts() + time::Duration::seconds(1),
        )
        .expect("b");
        set_state(&conn, "a", "T", JobState::Posted, fixed_ts()).expect("posted");
        set_failed(
            &mut conn,
            "b",
            "T",
            "extract",
            "oops",
            FailureKind::Unknown,
            fixed_ts(),
        )
        .expect("fail");
        // a is Posted (terminal), b is Failed (terminal-for-daemon) →
        // nothing actionable.
        let nxt = next_actionable_job(&conn, "T").expect("next");
        assert!(nxt.is_none());
        // Inserting a new fresh row → that's the actionable one.
        insert_fetched_job(
            &conn,
            "c",
            "T",
            "c@x",
            "Carol",
            "",
            "AL",
            1,
            "p.stl",
            "/tmp/p.stl",
            fixed_ts() + time::Duration::seconds(2),
        )
        .expect("c");
        let nxt = next_actionable_job(&conn, "T").expect("next2");
        assert_eq!(nxt.expect("c is next").quote_id, "c");
    }

    #[test]
    fn job_state_round_trip() {
        let cases = [
            JobState::Fetched,
            JobState::Extracting,
            JobState::Pricing,
            JobState::Rendering,
            JobState::PostingBack,
            JobState::Posted,
            JobState::Failed,
        ];
        for s in cases {
            assert_eq!(JobState::parse_str(s.as_str()).unwrap(), s);
        }
        assert!(JobState::parse_str("not_a_state").is_err());
    }

    #[test]
    fn sanitize_reason_strips_control_chars_and_truncates() {
        let s = "boom\r\nfollowup\0tail";
        assert_eq!(sanitize_reason(s), "boomfollowuptail");
        let long: String = "a".repeat(2000);
        let cleaned = sanitize_reason(&long);
        assert_eq!(cleaned.chars().count(), 1000);
    }

    // ── S286 / PR-268 hotfix pins ─────────────────────────────────────

    /// The defensive SELECT-first pattern: if the row is already at the
    /// target state, the UPDATE is skipped and `AlreadyInState` is
    /// returned. The PROD_v2.27.2 panic stack was a DuckDB FATAL inside an
    /// UPDATE-to-same-value path; never issuing the UPDATE eliminates the
    /// trigger entirely.
    #[test]
    fn set_state_returns_already_in_state_when_no_op() {
        let conn = open_mem();
        insert_fetched_job(
            &conn,
            "q-noop",
            "T",
            "x@y",
            "X",
            "",
            "AL",
            1,
            "p.stl",
            "/tmp/p.stl",
            fixed_ts(),
        )
        .expect("ins");
        // Row was inserted at `Fetched`. Asking to set it to `Fetched`
        // again must skip the UPDATE and report `AlreadyInState`.
        assert_eq!(
            set_state(&conn, "q-noop", "T", JobState::Fetched, fixed_ts()).expect("set"),
            TransitionOutcome::AlreadyInState
        );
    }

    /// `NotFound` when the row doesn't exist — the daemon's safety net for
    /// when `next_actionable_job` returns a row that gets deleted out from
    /// under us (operator wipe / parallel cleanup). Treated as skip, not
    /// panic.
    #[test]
    fn set_state_returns_not_found_when_row_missing() {
        let conn = open_mem();
        assert_eq!(
            set_state(&conn, "ghost", "T", JobState::Pricing, fixed_ts()).expect("set"),
            TransitionOutcome::NotFound
        );
    }

    /// `set_failed` is idempotent: re-failing an already-failed row no-ops.
    /// Supervisor recovery can racy-re-emit; we don't want an extra
    /// audit-trail entry per re-poll.
    #[test]
    fn set_failed_is_idempotent_on_already_failed_row() {
        let mut conn = open_mem();
        insert_fetched_job(
            &conn,
            "q-idem",
            "T",
            "x@y",
            "X",
            "",
            "AL",
            1,
            "p.stl",
            "/tmp/p.stl",
            fixed_ts(),
        )
        .expect("ins");
        assert_eq!(
            set_failed(
                &mut conn,
                "q-idem",
                "T",
                "extract",
                "boom",
                FailureKind::Permanent,
                fixed_ts(),
            )
            .expect("fail1"),
            TransitionOutcome::Applied
        );
        assert_eq!(
            set_failed(
                &mut conn,
                "q-idem",
                "T",
                "extract",
                "boom-again",
                FailureKind::Transient,
                fixed_ts()
            )
            .expect("fail2"),
            TransitionOutcome::AlreadyInState
        );
        // The first failure's reason must NOT be overwritten by the second.
        // S290 / PR-271: same posture for failure_kind — a re-failure
        // reaching set_failed must NOT silently overwrite the first
        // classification (`Permanent` here). The SELECT-first guard ensures
        // this: the UPDATE is skipped entirely.
        let rows = list_jobs(&conn, "T").expect("list");
        assert_eq!(rows[0].error_reason.as_deref(), Some("boom"));
        assert_eq!(rows[0].failure_kind, Some(FailureKind::Permanent));
    }

    /// S286 / PR-268 — forensic regression. Reproduces the exact PROD_v2.27.2
    /// crash sequence Ervin captured at 2026-06-08T13:28:38.984Z:
    ///
    ///   * Row `c1cf32ed-72b6-4708-8abb-6359d27f042b` enqueued at `Fetched`
    ///     (storefront submission with `material_preference: "unknown"`).
    ///   * Daemon iteration picks the row up via `next_actionable_job`.
    ///   * `set_state(Extracting)` is called → DuckDB UPDATE → C++ FATAL
    ///     inside the INSERT-side of UPDATE's MVCC-lowering →
    ///     `libc++abi: terminating`.
    ///
    /// With PR-268's two-pronged fix (SELECT-first guard + secondary-index
    /// drop), this transition completes cleanly. The test is dual-purpose:
    /// it pins the API contract (`Applied` outcome on a real state change)
    /// AND names the specific prod ULID + the data-shape quirks that may
    /// have contributed (the `material_grade = "unknown"` storefront data-
    /// quality miss).
    ///
    /// **Test cannot reproduce the C++ FATAL on a fresh local DB** — the
    /// DuckDB bug requires accumulated index state from prior abnormal
    /// terminations to fire. This test pins the LOGIC + the schema change;
    /// confidence on prod comes from the index drop being purely
    /// subtractive.
    #[test]
    fn s286_regression_prod_row_c1cf32ed_fetched_to_extracting() {
        let conn = open_mem();
        let prod_quote_id = "c1cf32ed-72b6-4708-8abb-6359d27f042b";
        let prod_tenant = "T"; // single-tenant prod (per ADR-0002)
                               // Insert mirrors the storefront submission shape Ervin captured:
                               // ervin@aben.ch, material="unknown", qty=1.
        let inserted = insert_fetched_job(
            &conn,
            prod_quote_id,
            prod_tenant,
            "ervin@aben.ch",
            "ervin csenger",
            "",
            "unknown",
            1,
            "submission.stl",
            "/var/aberp/quote-artifacts/c1cf32ed-.../submission.stl",
            fixed_ts(),
        )
        .expect("enqueue at Fetched");
        assert!(inserted);
        // Verify pre-condition: row landed at Fetched (1st daemon call).
        let rows = list_jobs(&conn, prod_tenant).expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, JobState::Fetched);
        // The crashing call: 2nd daemon iteration's `advance_extract` → set_state(Extracting).
        // SELECT-first sees state=Fetched != Extracting → runs UPDATE.
        // With the secondary index dropped, this no longer FATALs.
        let outcome = set_state(
            &conn,
            prod_quote_id,
            prod_tenant,
            JobState::Extracting,
            fixed_ts(),
        )
        .expect("Fetched→Extracting transition");
        assert_eq!(
            outcome,
            TransitionOutcome::Applied,
            "PROD_v2.27.2 crashing transition must now Apply, not FATAL"
        );
        let rows = list_jobs(&conn, prod_tenant).expect("list after");
        assert_eq!(rows[0].state, JobState::Extracting);
    }

    /// S286 / PR-268 — schema migration pin: the secondary index drop is
    /// idempotent. Calling `ensure_schema` on a DB that already has the
    /// index dropped, AND on a fresh DB, must both succeed without error.
    #[test]
    fn s286_secondary_index_drop_is_idempotent() {
        let conn = open_mem();
        // Fresh DB — first ensure_schema runs DROP INDEX IF EXISTS (no-op
        // since the index was never created in this code revision).
        ensure_schema(&conn).expect("first ensure");
        ensure_schema(&conn).expect("second ensure");
        ensure_schema(&conn).expect("third ensure");
    }

    /// S286 / PR-268 — even on a DB that PRE-EXISTING-LY carries the
    /// (tenant_id, state) secondary index from PROD_v2.27.[012], the
    /// `ensure_schema` migration drops it cleanly. Manually re-create the
    /// index to simulate that prod state, then re-run ensure_schema, then
    /// confirm the index is gone.
    #[test]
    fn s286_secondary_index_drop_migrates_from_prior_schema() {
        let conn = open_mem();
        // Simulate the pre-PR-268 schema: create the index by hand.
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS quote_pricing_jobs_tenant_state_idx
                ON quote_pricing_jobs (tenant_id, state);",
        )
        .expect("pre-PR-268 index create");
        // Re-running ensure_schema must DROP it.
        ensure_schema(&conn).expect("post-migration ensure");
        // Confirm the index is absent via DuckDB's information_schema-
        // equivalent. The catalog name varies across DuckDB versions, but
        // `pragma_show('quote_pricing_jobs')` returns column metadata
        // (not index info). Instead, attempt to re-CREATE the index with
        // a contradictory column set — if the original index were still
        // there, this would error out; if it's gone, the second CREATE
        // succeeds. (Pragmatic — DuckDB does not expose a portable
        // `SHOW INDEXES` we can rely on across versions.)
        let res = conn.execute_batch(
            "CREATE INDEX quote_pricing_jobs_tenant_state_idx
                ON quote_pricing_jobs (state);",
        );
        // Either the index was dropped (this CREATE succeeds) or DuckDB
        // returns the pre-existing-index error. The first is the
        // happy path; the second would mean our DROP didn't take.
        assert!(
            res.is_ok(),
            "secondary index should be dropped before this CREATE; got {res:?}"
        );
    }

    /// S288 / PR-269 — `detect_secondary_index_present` returns `false`
    /// on a fresh DB (the `duckdb_indexes()` system function returns zero
    /// rows when no user index named `..._tenant_state_idx` exists).
    /// Pre-condition for the boot migration to behave idempotently after
    /// PROD_v2.27.4: a clean install must NOT emit the
    /// `quote.pricing_jobs_index_migrated` audit row.
    #[test]
    fn s288_detect_index_returns_false_on_fresh_db() {
        let conn = open_mem();
        // No ensure_schema yet — duckdb_indexes() is a system function
        // and should not require the user table to exist.
        let present = detect_secondary_index_present(&conn).expect("detect on fresh DB");
        assert!(!present, "fresh DB has no orphan secondary index");
    }

    /// S288 / PR-269 — `detect_secondary_index_present` returns `true`
    /// on a DB that still carries the orphan index from PROD_v2.27.[012].
    /// This is the pin that drives the one-shot audit emit at boot:
    /// without it, the operator-visible migration row would never fire.
    #[test]
    fn s288_detect_index_returns_true_when_present() {
        let conn = open_mem();
        ensure_schema(&conn).expect("schema for table");
        // Simulate the pre-PR-268 prod schema by hand.
        conn.execute_batch(
            "CREATE INDEX quote_pricing_jobs_tenant_state_idx
                ON quote_pricing_jobs (tenant_id, state);",
        )
        .expect("simulate prior-version index");
        let present = detect_secondary_index_present(&conn).expect("detect");
        assert!(present, "orphan index should be detected as present");
    }

    /// S288 / PR-269 — `migrate_secondary_index_with_report` returns
    /// `true` the first time it runs against a DB that carried the
    /// orphan index, and `false` on subsequent calls. This is the
    /// invariant that makes the audit emit one-shot per upgrade: only
    /// the `true` return fires the row.
    #[test]
    fn s288_migrate_reports_true_first_then_false() {
        let conn = open_bare_mem();
        // Pre-create the table + orphan index to simulate prod PROD_v2.27.2.
        // Bypass SCHEMA_SQL's DROP INDEX by hand-rolling the CREATE TABLE
        // with the EXACT column set from SCHEMA_SQL above (kept in sync
        // by inspection — a schema-drift smell test would catch it).
        conn.execute_batch(
            "CREATE TABLE quote_pricing_jobs (
                quote_id              VARCHAR NOT NULL PRIMARY KEY,
                tenant_id             VARCHAR NOT NULL,
                state                 VARCHAR NOT NULL,
                fetched_at            VARCHAR NOT NULL,
                updated_at            VARCHAR NOT NULL,
                customer_email        VARCHAR NOT NULL,
                customer_name         VARCHAR NOT NULL,
                material_grade        VARCHAR NOT NULL,
                quantity              INTEGER NOT NULL,
                cad_filename          VARCHAR NOT NULL,
                cad_local_path        VARCHAR NOT NULL,
                feature_graph_hash    VARCHAR,
                feature_graph_json    VARCHAR,
                breakdown_json        VARCHAR,
                pdf_path              VARCHAR,
                total_price_eur       DOUBLE,
                valid_until_iso       VARCHAR,
                error_stage           VARCHAR,
                error_reason          VARCHAR,
                attempt_n             INTEGER NOT NULL
            );
            CREATE INDEX quote_pricing_jobs_tenant_state_idx
                ON quote_pricing_jobs (tenant_id, state);",
        )
        .expect("pre-PR-268 schema seeding");
        // First migration: detects orphan + drops it.
        let first = migrate_secondary_index_with_report(&conn).expect("first migration");
        assert!(first, "first call must detect + report orphan as present");
        // Second migration: index gone, returns false.
        let second = migrate_secondary_index_with_report(&conn).expect("second migration");
        assert!(
            !second,
            "second call must report orphan as absent (no-op steady-state)"
        );
        // Fresh DBs also report false on their very first call.
        let fresh = open_bare_mem();
        let on_fresh = migrate_secondary_index_with_report(&fresh).expect("migrate on fresh");
        assert!(!on_fresh, "fresh DB must report no orphan index present");
    }

    /// S288 / PR-269 — the load-bearing end-to-end pin (Brief B).
    /// Simulate the exact PROD_v2.27.2/3 crash sequence in one test:
    ///
    /// 1. Pre-create the table with the orphan secondary index (the
    ///    state every existing prod install is in after upgrade).
    /// 2. Insert TWO rows: one mimicking the storefront-side legacy
    ///    "no CAD file" enqueue-failed shape (state=Failed,
    ///    last_reason=set), one being the actual c1cf32...042b row
    ///    captured at Fetched state.
    /// 3. Run the boot-time migration to drop the index.
    /// 4. Advance the c1cf32 row Fetched→Extracting via `set_state`
    ///    (the exact transition that FATALed at 13:28:39Z and
    ///    15:36:23Z).
    /// 5. Assert: the transition succeeds, no panic, the c1cf32 row
    ///    is now at Extracting, the failed row is unchanged.
    ///
    /// This is the "the crash from 13:28:39Z and 15:36:23Z cannot
    /// recur" pin — combines the migration prong + the SELECT-first
    /// idempotency guard from S286 in one forensic replay.
    #[test]
    fn s288_prod_crash_replay_does_not_panic_after_migration() {
        let conn = open_bare_mem();
        // Step 1: simulate a PROD_v2.27.2-shaped DB with the orphan
        // index intact. Hand-roll CREATE TABLE (matching SCHEMA_SQL)
        // so the orphan index is created before our migration sees it.
        conn.execute_batch(
            "CREATE TABLE quote_pricing_jobs (
                quote_id              VARCHAR NOT NULL PRIMARY KEY,
                tenant_id             VARCHAR NOT NULL,
                state                 VARCHAR NOT NULL,
                fetched_at            VARCHAR NOT NULL,
                updated_at            VARCHAR NOT NULL,
                customer_email        VARCHAR NOT NULL,
                customer_name         VARCHAR NOT NULL,
                material_grade        VARCHAR NOT NULL,
                quantity              INTEGER NOT NULL,
                cad_filename          VARCHAR NOT NULL,
                cad_local_path        VARCHAR NOT NULL,
                feature_graph_hash    VARCHAR,
                feature_graph_json    VARCHAR,
                breakdown_json        VARCHAR,
                pdf_path              VARCHAR,
                total_price_eur       DOUBLE,
                valid_until_iso       VARCHAR,
                error_stage           VARCHAR,
                error_reason          VARCHAR,
                attempt_n             INTEGER NOT NULL
            );
            CREATE INDEX quote_pricing_jobs_tenant_state_idx
                ON quote_pricing_jobs (tenant_id, state);",
        )
        .expect("pre-PR-268 schema seeding");
        // Step 2a: legacy "no CAD file" enqueue-failure row — these
        // populate the table from older storefront-side bugs. Uses the
        // real columns (`error_stage`, `error_reason`) — NOT a made-up
        // `last_reason` field.
        let legacy_qid = "legacy-no-cad-001";
        let tenant = "T";
        let ts = fixed_ts()
            .format(&time::format_description::well_known::Rfc3339)
            .expect("ts");
        conn.execute(
            "INSERT INTO quote_pricing_jobs (
                quote_id, tenant_id, state, fetched_at, updated_at,
                customer_email, customer_name, material_grade, quantity,
                cad_filename, cad_local_path, attempt_n,
                error_stage, error_reason
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1, ?, ?)",
            params![
                legacy_qid,
                tenant,
                STATE_FAILED,
                ts,
                ts,
                "legacy@example.com",
                "legacy customer",
                "AL",
                1,
                "(missing)",
                "(missing)",
                "extract",
                "no CAD file",
            ],
        )
        .expect("insert legacy failed row");
        // Step 2b: the c1cf32 prod row at Fetched, mirroring the
        // storefront submission Ervin captured.
        let prod_qid = "c1cf32ed-72b6-4708-8abb-6359d27f042b";
        conn.execute(
            "INSERT INTO quote_pricing_jobs (
                quote_id, tenant_id, state, fetched_at, updated_at,
                customer_email, customer_name, material_grade, quantity,
                cad_filename, cad_local_path, attempt_n
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0)",
            params![
                prod_qid,
                tenant,
                STATE_FETCHED,
                ts,
                ts,
                "ervin@aben.ch",
                "ervin csenger",
                "unknown",
                1,
                "submission.stl",
                "/var/aberp/quote-artifacts/c1cf32ed-.../submission.stl",
            ],
        )
        .expect("insert c1cf32 prod row at Fetched");
        // Step 3: boot-time migration runs. Reports `true` (orphan was
        // present); drops index.
        let migrated = migrate_secondary_index_with_report(&conn).expect("boot-time migration");
        assert!(
            migrated,
            "the c31b3c6 forensic premise — boot must report the orphan as present on a v2.27.2-shaped DB"
        );
        // Step 4: replay the crashing transition (Fetched → Extracting).
        // With the index dropped + SELECT-first guard, no FATAL.
        let outcome = set_state(&conn, prod_qid, tenant, JobState::Extracting, fixed_ts())
            .expect("Fetched→Extracting must not FATAL after migration");
        assert_eq!(outcome, TransitionOutcome::Applied);
        // Step 5: state invariants — c1cf32 advanced, legacy untouched.
        let rows = list_jobs(&conn, tenant).expect("list after replay");
        let by_qid: std::collections::HashMap<_, _> =
            rows.into_iter().map(|r| (r.quote_id.clone(), r)).collect();
        assert_eq!(
            by_qid[prod_qid].state,
            JobState::Extracting,
            "c1cf32 must have advanced past the formerly-crashing transition"
        );
        assert_eq!(
            by_qid[legacy_qid].state,
            JobState::Failed,
            "legacy enqueue-failure row must be untouched by the c1cf32 transition"
        );
        // Second migration call on the same DB now reports the steady-
        // state (the post-PROD_v2.27.4 reboot scenario).
        let again = migrate_secondary_index_with_report(&conn).expect("second migration call");
        assert!(
            !again,
            "post-migration boot must report no orphan — one-shot audit guarantee"
        );
    }

    /// Boot-time count helper used by `serve.rs` to log the existing-row
    /// population on daemon spawn. S286 posture: surface what's in the
    /// table so operator + forensic walker can compare against prod.
    #[test]
    fn count_jobs_returns_tenant_scoped_population() {
        let conn = open_mem();
        assert_eq!(count_jobs(&conn, "T").expect("count empty"), 0);
        for qid in ["a", "b", "c"] {
            insert_fetched_job(
                &conn,
                qid,
                "T",
                "x@y",
                "X",
                "",
                "AL",
                1,
                "p.stl",
                "/tmp/p.stl",
                fixed_ts(),
            )
            .expect("ins");
        }
        // A different tenant must not show up.
        insert_fetched_job(
            &conn,
            "alien",
            "OTHER",
            "x@y",
            "X",
            "",
            "AL",
            1,
            "p.stl",
            "/tmp/p.stl",
            fixed_ts(),
        )
        .expect("ins-other");
        assert_eq!(count_jobs(&conn, "T").expect("count after"), 3);
        assert_eq!(count_jobs(&conn, "OTHER").expect("count other"), 1);
    }

    // ── S290 / PR-271 — FailureKind classifier pins ──────────────────

    #[test]
    fn failure_kind_round_trip() {
        for k in [
            FailureKind::Transient,
            FailureKind::Permanent,
            FailureKind::Unknown,
        ] {
            assert_eq!(FailureKind::parse_str(k.as_str()).unwrap(), k);
        }
        // Loud-fail on unknown — silent fallback would mask schema drift
        // (CLAUDE.md rule 12).
        assert!(FailureKind::parse_str("garbage").is_err());
        assert!(FailureKind::parse_str("").is_err());
    }

    #[test]
    fn failure_kind_storage_strings_are_closed_vocab_lowercase() {
        // The on-disk contract: the strings ride into the SPA JSON and
        // the audit-row payload. Distinct + lowercase + snake.
        assert_eq!(FailureKind::Transient.as_str(), "transient");
        assert_eq!(FailureKind::Permanent.as_str(), "permanent");
        assert_eq!(FailureKind::Unknown.as_str(), "unknown");
        // Pairwise-distinct.
        let all = [
            FailureKind::Transient.as_str(),
            FailureKind::Permanent.as_str(),
            FailureKind::Unknown.as_str(),
        ];
        for i in 0..all.len() {
            for j in (i + 1)..all.len() {
                assert_ne!(all[i], all[j]);
            }
        }
    }

    /// S290 / PR-271 — `failure_kind` column lands via additive
    /// `ALTER TABLE ADD COLUMN IF NOT EXISTS`. Simulate a pre-PR-271
    /// schema (no `failure_kind` column), confirm `ensure_schema`
    /// idempotently adds it, and confirm a legacy `Failed` row reads
    /// back with `failure_kind = None` (treated as Unknown by the
    /// daemon scheduler).
    #[test]
    fn s290_failure_kind_column_added_idempotently_to_legacy_schema() {
        let conn = open_bare_mem();
        // Pre-PR-271 CREATE TABLE — same column set as SCHEMA_SQL but
        // without the trailing `failure_kind VARCHAR`. Hand-rolled to
        // simulate the prod schema before the additive migration.
        conn.execute_batch(
            "CREATE TABLE quote_pricing_jobs (
                quote_id              VARCHAR NOT NULL PRIMARY KEY,
                tenant_id             VARCHAR NOT NULL,
                state                 VARCHAR NOT NULL,
                fetched_at            VARCHAR NOT NULL,
                updated_at            VARCHAR NOT NULL,
                customer_email        VARCHAR NOT NULL,
                customer_name         VARCHAR NOT NULL,
                material_grade        VARCHAR NOT NULL,
                quantity              INTEGER NOT NULL,
                cad_filename          VARCHAR NOT NULL,
                cad_local_path        VARCHAR NOT NULL,
                feature_graph_hash    VARCHAR,
                feature_graph_json    VARCHAR,
                breakdown_json        VARCHAR,
                pdf_path              VARCHAR,
                total_price_eur       DOUBLE,
                valid_until_iso       VARCHAR,
                error_stage           VARCHAR,
                error_reason          VARCHAR,
                attempt_n             INTEGER NOT NULL
            );",
        )
        .expect("pre-PR-271 schema seeding");
        // Seed a legacy Failed row from before the column existed.
        let ts = fixed_ts()
            .format(&time::format_description::well_known::Rfc3339)
            .expect("ts");
        conn.execute(
            "INSERT INTO quote_pricing_jobs (
                quote_id, tenant_id, state, fetched_at, updated_at,
                customer_email, customer_name, material_grade, quantity,
                cad_filename, cad_local_path, attempt_n,
                error_stage, error_reason
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1, ?, ?)",
            params![
                "legacy-q",
                "T",
                STATE_FAILED,
                ts,
                ts,
                "legacy@x",
                "legacy",
                "AL",
                1,
                "(missing)",
                "(missing)",
                "extract",
                "before-PR-271",
            ],
        )
        .expect("seed legacy Failed row");
        // First ensure_schema: ALTER ADD COLUMN runs.
        ensure_schema(&conn).expect("first migration");
        // Second + third: idempotent — the IF NOT EXISTS guard makes a
        // re-call a no-op (no error, no double-add).
        ensure_schema(&conn).expect("second migration");
        ensure_schema(&conn).expect("third migration");
        // Legacy row reads back with failure_kind = None.
        let rows = list_jobs(&conn, "T").expect("list legacy");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, JobState::Failed);
        assert_eq!(rows[0].failure_kind, None);
    }

    // ── S349 / PR-40 (U1) — get_job_detail ──────────────────────────

    #[test]
    fn s349_get_job_detail_missing_row_is_none() {
        let conn = open_mem();
        let got = get_job_detail(&conn, "no-such-quote", "T").expect("query");
        assert!(got.is_none(), "missing row must be None (→ 404 at route)");
    }

    #[test]
    fn s349_get_job_detail_carries_row_plus_artifacts() {
        let mut conn = open_mem();
        insert_fetched_job(
            &conn,
            "qd1",
            "T",
            "alice@example.com",
            "Alice",
            "",
            "AL_6061_T6",
            7,
            "bracket.step",
            "/tmp/qd1/bracket.step",
            fixed_ts(),
        )
        .expect("insert");
        set_state(&conn, "qd1", "T", JobState::Extracting, fixed_ts()).expect("ex");
        set_extracted(
            &mut conn,
            "qd1",
            "T",
            "blake3:beef",
            "{\"_schema_version\":1,\"volume_mm3\":1000.0}",
            fixed_ts(),
        )
        .expect("extracted");
        set_priced(
            &mut conn,
            "qd1",
            "T",
            "{\"total_price\":42.5,\"material_cost\":10.0}",
            42.5,
            fixed_ts(),
        )
        .expect("priced");
        set_rendered(
            &mut conn,
            "qd1",
            "T",
            "/tmp/qd1/priced.pdf",
            "2026-07-06",
            fixed_ts(),
        )
        .expect("rendered");

        let d = get_job_detail(&conn, "qd1", "T")
            .expect("query")
            .expect("row present");
        // Row fields surface unchanged.
        assert_eq!(d.row.quote_id, "qd1");
        assert_eq!(d.row.customer_name, "Alice");
        assert_eq!(d.row.material_grade, "AL_6061_T6");
        assert_eq!(d.row.quantity, 7);
        assert_eq!(d.row.total_price_eur, Some(42.5));
        assert_eq!(d.row.state, JobState::PostingBack);
        // Artifact columns the list omits.
        assert_eq!(d.cad_filename, "bracket.step");
        assert_eq!(d.valid_until_iso.as_deref(), Some("2026-07-06"));
        assert!(d.pdf_path.is_some(), "pdf_path set after render");
        assert!(
            d.breakdown_json.as_deref().unwrap().contains("total_price"),
            "breakdown JSON carried"
        );
        assert!(
            d.feature_graph_json
                .as_deref()
                .unwrap()
                .contains("volume_mm3"),
            "FeatureGraph JSON carried"
        );
    }

    #[test]
    fn s349_get_job_detail_null_artifacts_on_early_row() {
        // Adversarial: a row that never reached Pricing has null
        // breakdown/featuregraph/pdf — get_job_detail must return Some
        // with those as None (the route renders a "not available"
        // placeholder, NOT a 500 or a blank section).
        let conn = open_mem();
        insert_fetched_job(
            &conn,
            "qd2",
            "T",
            "b@x",
            "Bob",
            "",
            "unknown",
            1,
            "part.iges",
            "/tmp/qd2/part.iges",
            fixed_ts(),
        )
        .expect("insert");
        let d = get_job_detail(&conn, "qd2", "T")
            .expect("query")
            .expect("row present");
        assert_eq!(d.row.state, JobState::Fetched);
        assert_eq!(d.cad_filename, "part.iges");
        assert!(d.breakdown_json.is_none(), "no breakdown before pricing");
        assert!(d.feature_graph_json.is_none(), "no graph before extract");
        assert!(d.pdf_path.is_none(), "no PDF before render");
        assert!(d.valid_until_iso.is_none());
    }

    #[test]
    fn s349_get_job_detail_is_tenant_scoped() {
        let conn = open_mem();
        insert_fetched_job(
            &conn,
            "qd3",
            "T",
            "c@x",
            "Cara",
            "",
            "AL",
            1,
            "p.stl",
            "/tmp/p.stl",
            fixed_ts(),
        )
        .expect("insert");
        // Same quote_id, different tenant → no leak.
        assert!(
            get_job_detail(&conn, "qd3", "OTHER")
                .expect("query")
                .is_none(),
            "tenant isolation: a foreign tenant sees no row"
        );
    }

    // ── S350 / PR-39 (U5) — operator material-grade override ─────────

    #[test]
    fn s350_material_editable_only_fetched_posting_back_failed() {
        assert!(JobState::Fetched.material_editable());
        assert!(JobState::PostingBack.material_editable());
        assert!(JobState::Failed.material_editable());
        assert!(!JobState::Extracting.material_editable());
        assert!(!JobState::Pricing.material_editable());
        assert!(!JobState::Rendering.material_editable());
        assert!(
            !JobState::Posted.material_editable(),
            "Posted is terminal — a re-priced grade would 409 on the new hash"
        );
    }

    /// Happy path: a Failed row gets its grade rewritten → reset to
    /// Fetched, attempt bumped, error fields cleared; the outcome
    /// carries the prior grade + state for the audit payload.
    #[test]
    fn s350_amend_resets_to_fetched_bumps_attempt_clears_error() {
        let mut conn = open_mem();
        insert_fetched_job(
            &conn,
            "qe1",
            "T",
            "e@x",
            "Eve",
            "",
            "unknown",
            4,
            "part.step",
            "/tmp/part.step",
            fixed_ts(),
        )
        .expect("insert");
        set_failed(
            &mut conn,
            "qe1",
            "T",
            "pricing",
            "material grade `unknown` is not in the catalogue snapshot",
            FailureKind::Permanent,
            fixed_ts(),
        )
        .expect("fail it");

        let outcome =
            amend_material_grade(&mut conn, "qe1", "T", "AL_6061_T6", fixed_ts()).expect("amend");
        match outcome {
            MaterialEditOutcome::Applied {
                old_grade,
                previous_state,
                new_attempt_n,
            } => {
                assert_eq!(old_grade, "unknown");
                assert_eq!(previous_state, JobState::Failed);
                assert_eq!(new_attempt_n, 1, "attempt bumped 0 → 1");
            }
            other => panic!("expected Applied, got {other:?}"),
        }

        let d = get_job_detail(&conn, "qe1", "T")
            .expect("query")
            .expect("row");
        assert_eq!(d.row.state, JobState::Fetched, "re-enters pricing");
        assert_eq!(d.row.material_grade, "AL_6061_T6");
        assert_eq!(d.row.attempt_n, 1);
        assert!(d.row.error_stage.is_none(), "error cleared on edit");
        assert!(d.row.error_reason.is_none());
        assert!(d.row.failure_kind.is_none());
    }

    #[test]
    fn s350_amend_missing_row_is_not_found() {
        let mut conn = open_mem();
        let outcome = amend_material_grade(&mut conn, "no-such", "T", "AL_6061_T6", fixed_ts())
            .expect("amend");
        assert!(matches!(outcome, MaterialEditOutcome::NotFound));
    }

    #[test]
    fn s350_amend_refuses_posted_row_without_mutating() {
        let mut conn = open_mem();
        insert_fetched_job(
            &conn,
            "qe2",
            "T",
            "e@x",
            "Eve",
            "",
            "AL_6061_T6",
            1,
            "p.stl",
            "/tmp/p.stl",
            fixed_ts(),
        )
        .expect("insert");
        // Drive to Posted.
        set_state(&conn, "qe2", "T", JobState::Extracting, fixed_ts()).expect("ex");
        set_extracted(&mut conn, "qe2", "T", "blake3:x", "{}", fixed_ts()).expect("extract");
        set_priced(&mut conn, "qe2", "T", "{}", 10.0, fixed_ts()).expect("price");
        set_rendered(
            &mut conn,
            "qe2",
            "T",
            "/tmp/x.pdf",
            "2026-07-06",
            fixed_ts(),
        )
        .expect("render");
        set_state(&conn, "qe2", "T", JobState::Posted, fixed_ts()).expect("post");

        let outcome =
            amend_material_grade(&mut conn, "qe2", "T", "SS_304", fixed_ts()).expect("amend");
        assert!(
            matches!(
                outcome,
                MaterialEditOutcome::NotEditable {
                    state: JobState::Posted
                }
            ),
            "Posted row must refuse the edit"
        );
        // Row unchanged — still Posted with the original grade.
        let d = get_job_detail(&conn, "qe2", "T")
            .expect("query")
            .expect("row");
        assert_eq!(d.row.state, JobState::Posted);
        assert_eq!(d.row.material_grade, "AL_6061_T6");
    }

    #[test]
    fn s350_amend_is_tenant_scoped() {
        let mut conn = open_mem();
        insert_fetched_job(
            &conn,
            "qe3",
            "T",
            "e@x",
            "Eve",
            "",
            "unknown",
            1,
            "p.stl",
            "/tmp/p.stl",
            fixed_ts(),
        )
        .expect("insert");
        // A foreign tenant sees no row → NotFound, never touches T's row.
        let outcome = amend_material_grade(&mut conn, "qe3", "OTHER", "AL_6061_T6", fixed_ts())
            .expect("amend");
        assert!(matches!(outcome, MaterialEditOutcome::NotFound));
        let d = get_job_detail(&conn, "qe3", "T")
            .expect("query")
            .expect("row");
        assert_eq!(d.row.material_grade, "unknown", "T's row untouched");
    }

    /// S414/S414b — operator Delete of a Failed row ARCHIVES it (does not
    /// hard-delete), so: (a) it drops out of the operator panel
    /// (`list_jobs`), (b) the row survives in the table as `archived`, and
    /// (c) a subsequent daemon re-enqueue of the SAME quote_id is a silent
    /// `ON CONFLICT` no-op — the regression test for the "phantom CAD-less
    /// quote re-WARNs every poll cycle" loop. A hard DELETE would have let
    /// `insert_failed_enqueue_job` return `true` again, re-`warn!`-ing.
    #[test]
    fn s414_delete_archives_failed_row_and_blocks_re_enqueue() {
        let mut conn = open_mem();
        // Permanent-Failed enqueue row (the S379 no-CAD shape).
        assert!(insert_failed_enqueue_job(
            &conn,
            "qzz",
            "T",
            "z@x",
            "Zoe",
            "",
            "AL_6061_T6",
            1,
            "enqueue",
            "no CAD file on listing",
            FailureKind::Permanent,
            fixed_ts(),
        )
        .expect("insert failed"));
        assert_eq!(list_jobs(&conn, "T").expect("list").len(), 1);

        // Operator Delete → archive (tx-owned core).
        let tx = conn.transaction().expect("tx");
        let outcome = delete_failed_job_in_tx(&tx, "qzz", "T", fixed_ts()).expect("archive");
        assert!(matches!(outcome, DeleteJobOutcome::Archived { .. }));
        tx.commit().expect("commit");

        // (a) gone from the operator panel.
        assert!(
            list_jobs(&conn, "T").expect("list2").is_empty(),
            "archived row must not appear in the operator panel"
        );
        // (b) row survives as `archived`.
        let state: String = conn
            .query_row(
                "SELECT state FROM quote_pricing_jobs WHERE quote_id = 'qzz'",
                [],
                |r| r.get(0),
            )
            .expect("row still present");
        assert_eq!(state, STATE_ARCHIVED);
        // (c) the daemon's next-cycle re-enqueue of the same quote_id is a
        // silent no-op — NO re-`warn!`, NO crept-back row.
        let re_enqueued = insert_failed_enqueue_job(
            &conn,
            "qzz",
            "T",
            "z@x",
            "Zoe",
            "",
            "AL_6061_T6",
            1,
            "enqueue",
            "no CAD file on listing",
            FailureKind::Permanent,
            fixed_ts(),
        )
        .expect("re-enqueue");
        assert!(
            !re_enqueued,
            "ON CONFLICT must skip the archived row (no re-WARN loop)"
        );
        // Still archived (not flipped back to failed by the no-op insert).
        let state2: String = conn
            .query_row(
                "SELECT state FROM quote_pricing_jobs WHERE quote_id = 'qzz'",
                [],
                |r| r.get(0),
            )
            .expect("row still present");
        assert_eq!(state2, STATE_ARCHIVED);
    }

    /// S414 — the archive is state-guarded exactly like the old delete:
    /// 409 (`NotDeletable`) for a non-Failed row, 404 (`NotFound`) for an
    /// absent one. An in-flight / Posted row is never archived out from
    /// under the daemon.
    #[test]
    fn s414_delete_refuses_non_failed_and_missing() {
        let mut conn = open_mem();
        insert_fetched_job(
            &conn,
            "qf",
            "T",
            "f@x",
            "Fred",
            "",
            "AL_6061_T6",
            1,
            "p.stl",
            "/tmp/p.stl",
            fixed_ts(),
        )
        .expect("insert");

        let tx = conn.transaction().expect("tx");
        // Fetched row → NotDeletable, no mutation.
        let outcome = delete_failed_job_in_tx(&tx, "qf", "T", fixed_ts()).expect("guarded");
        assert!(matches!(
            outcome,
            DeleteJobOutcome::NotDeletable {
                state: JobState::Fetched
            }
        ));
        // Absent quote → NotFound.
        let missing = delete_failed_job_in_tx(&tx, "nope", "T", fixed_ts()).expect("missing");
        assert!(matches!(missing, DeleteJobOutcome::NotFound));
        tx.commit().expect("commit");

        // The Fetched row is untouched (still visible + still Fetched).
        let rows = list_jobs(&conn, "T").expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, JobState::Fetched);
    }
}
