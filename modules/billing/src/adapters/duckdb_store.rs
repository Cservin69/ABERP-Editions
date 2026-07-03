//! DuckDB-backed [`BillingStore`] adapter.
//!
//! Five tables, no foreign keys (ADR-0019). The allocator is the heart:
//! [`allocate_in_tx`] runs the entire ADR-0009 §3 "Allocate (atomic)"
//! sequence against a borrowed [`duckdb::Transaction`]. Crash mid-flight
//! rolls back the whole thing — no burned number without an invoice, no
//! invoice without a reservation.
//!
//! # Two entry points
//!
//! - [`allocate_in_tx`] (free function) — for the binary path where one
//!   `Connection`/`Transaction` is shared across `aberp-billing` and
//!   `aberp-audit-ledger` so that ADR-0009 §3 step 6 (audit-ledger
//!   entries in the same transaction) holds. PR-6 added this; the
//!   binary in `apps/aberp` is the production caller.
//! - [`DuckDbBillingStore::allocate_and_insert`] (trait method) — opens
//!   its own transaction and calls [`allocate_in_tx`]. Retained for
//!   in-process tests where the caller is not coordinating an audit
//!   write in the same tx.
//!
//! The two paths share one body of SQL — the trait method is a five-line
//! wrapper around the free function so there is exactly one place that
//! knows how to allocate.

use duckdb::{params, Connection};
use rust_decimal::Decimal;
use std::str::FromStr;
use time::format_description::well_known::Rfc3339;
use time::macros::format_description;
use time::OffsetDateTime;
use ulid::Ulid;

use crate::app::error::BillingError;
use crate::domain::ids::{CustomerId, InvoiceId, ReservationId, SeriesId};
use crate::domain::invoice::{LineItem, ReadyInvoice};
use crate::domain::money::{Currency, Huf};
use crate::domain::reservation::{ReservationStatus, SequenceReservation};
use crate::domain::series::{InvoiceSeries, ResetPolicy, SeriesCode};
use crate::ports::storage::{AllocateArgs, AllocateOutcome, BillingStore};

// PR-73 / ADR-0040 §addendum added the per-invoice bank-account-snapshot
// columns; the SQL-comment rationale alongside them quotes operator-
// facing phrases like "no bank account on file" with embedded `"`
// characters. A plain double-quoted Rust string literal would terminate
// at those quotes, so this const switched to a raw string. The two
// migration consts below stay as plain strings (no embedded quotes).
// S410 / [[no-sql-specific]] — no DB-level CHECK constraints. Every
// closed-vocab and range invariant these tables once encoded as `CHECK`
// lives in Rust instead, where it is engine-portable and unit-tested:
//   - `reset_policy`  → `reset_policy_from_str` (rejects out-of-vocab on read);
//                        only `reset_policy_to_str` ever writes it.
//   - `status`        → the read-side match in `row_to_reservation`
//                        (`decode_err` on out-of-vocab); only the enum writes it.
//   - `next_number≥1` → the allocator floor (`max(next_number, start_value,
//                        sequence_floor)`, S394); `start_value ≥ 1`.
//   - `quantity≥0`    → the issuance preflight `LineItemQuantityZero` gate
//                        (`issue_preflight.rs`, rejects `quantity <= 0`).
// Dropping the CHECKs also unblocks DuckDB `ALTER COLUMN TYPE` (the S157
// quantity widen needed an add/backfill/drop/rename ladder *because* of
// the CHECK — see `MIGRATE_S157_SQL`).
const CREATE_TABLES_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS invoice_series (
    id           VARCHAR NOT NULL PRIMARY KEY,
    code         VARCHAR NOT NULL UNIQUE,
    reset_policy VARCHAR NOT NULL,
    fiscal_year  INTEGER,
    created_at   VARCHAR NOT NULL
);

CREATE TABLE IF NOT EXISTS invoice_sequence_state (
    series_id   VARCHAR NOT NULL,
    fiscal_year INTEGER NOT NULL,
    next_number BIGINT  NOT NULL,
    updated_at  VARCHAR NOT NULL,
    PRIMARY KEY (series_id, fiscal_year)
);

CREATE TABLE IF NOT EXISTS invoice_sequence_reservation (
    id          VARCHAR NOT NULL PRIMARY KEY,
    series_id   VARCHAR NOT NULL,
    fiscal_year INTEGER NOT NULL,
    number      BIGINT  NOT NULL,
    invoice_id  VARCHAR NOT NULL UNIQUE,
    status      VARCHAR NOT NULL,
    void_reason VARCHAR,
    reserved_at VARCHAR NOT NULL,
    used_at     VARCHAR,
    voided_at   VARCHAR,
    UNIQUE (series_id, fiscal_year, number)
);

CREATE TABLE IF NOT EXISTS invoice (
    id              VARCHAR NOT NULL PRIMARY KEY,
    series_id       VARCHAR NOT NULL,
    customer_id     VARCHAR NOT NULL,
    issue_date      VARCHAR NOT NULL,
    sequence_number BIGINT  NOT NULL,
    fiscal_year     INTEGER NOT NULL,
    idempotency_key VARCHAR NOT NULL UNIQUE,
    -- PR-44γ / ADR-0037 §3 + §1.a + §1.b additions. Fresh-DB callers
    -- pick up the columns via this CREATE; pre-PR-44γ databases pick
    -- them up via `MIGRATE_PR_44C_SQL` below (the idempotent
    -- `ALTER TABLE ADD COLUMN IF NOT EXISTS` ladder).
    --
    -- Migration backfill posture: existing rows are HUF (the only
    -- currency before PR-44γ); the migration ADDs the column as
    -- nullable, then UPDATEs NULL rows to `'HUF'`. The CREATE form
    -- mirrors that nullable shape so a fresh DB has the SAME column
    -- definitions as a migrated DB — DuckDB v1 does not support
    -- `ALTER TABLE ADD COLUMN ... NOT NULL DEFAULT ...`, so the two
    -- code paths converge on nullable + explicit-INSERT-value. NULL
    -- in `currency` is treated as HUF at read time (the only value
    -- pre-PR-44γ rows could possibly carry).
    --
    -- The four exchange-rate columns are nullable on both fresh +
    -- migrated DBs since there IS no equivalent for a HUF invoice
    -- (the regulatory record is the HUF amount itself). This is the
    -- C10 byte-identical invariant prerequisite: no existing HUF row
    -- gains a non-trivial rate stamp.
    currency             VARCHAR,
    exchange_rate        DECIMAL(18, 6),
    exchange_rate_source VARCHAR,
    exchange_rate_date   DATE,
    huf_equivalent_total DECIMAL(18, 0),
    -- PR-73 / ADR-0040 §addendum — denormalized per-invoice bank-account
    -- snapshot. Mirrors the `[[seller.banks]]` entry shape (per
    -- ADR-0040 §1) the operator selected (or defaulted to) at issuance
    -- time. NULL across all five columns iff the invoice was issued
    -- before PR-73's wire shape OR by a non-SPA caller that does not
    -- exercise the bank picker. The read path
    -- (`invoice_bank_snapshot.rs`) treats a NULL row as "no bank
    -- account on file" — never fabricates one from current
    -- `seller.toml` state, since the regulatory record is "the bank
    -- account the invoice was issued with."
    --
    -- Migration backfill posture: pre-PR-73 rows pick up the columns
    -- via `MIGRATE_PR_73_SQL` below; existing HUF + EUR rows stay NULL
    -- across all five (no retroactive rewrite — the C10 byte-identical
    -- invariant carries through). Fresh-DB callers populate the
    -- columns iff `AllocateArgs.bank_snapshot` is `Some(_)`.
    bank_account_id        VARCHAR,
    bank_account_currency  VARCHAR,
    bank_account_number    VARCHAR,
    bank_account_bank_name VARCHAR,
    bank_account_swift_bic VARCHAR,
    -- PR-82 — buyer-facing invoice-level note ("Megjegyzés"). Recipient-
    -- facing only; never emitted into the NAV InvoiceData XML. Nullable
    -- by design (most invoices carry no global note); pre-PR-82 rows
    -- gain the column via MIGRATE_PR_82_SQL and stay NULL.
    invoice_note           VARCHAR,
    -- PR-84 — operator-supplied payment deadline (Fizetési határidő)
    -- and delivery / fulfillment date (Teljesítési dátum). Calendar
    -- dates in canonical YYYY-MM-DD form (DuckDB DATE type accepts the
    -- ISO string). Pre-PR-84 rows pick these up via MIGRATE_PR_84_SQL
    -- and stay NULL until backfilled; the read path treats NULL as
    -- "fall back to issue_date" so old rows keep their pre-PR-84
    -- behaviour (delivery + payment mirror issue, per the current
    -- nav_xml emit). Fresh issuances post-PR-84 write both values
    -- non-NULL.
    payment_deadline       DATE,
    delivery_date          DATE,
    -- PR-203 / S203 — operator-typed per-invoice email recipient override
    -- ("Email-címzett(ek)"). Comma-separated address list (the canonical
    -- shape `partners::join_emails_canonical` already emits for
    -- `partners.contact_email`); NULL when the operator left it blank.
    -- The send-path resolver consults this column FIRST (override-then-
    -- partner-fallback-then-skip ladder); editing it here NEVER writes
    -- back to the partner master record — it is a one-off per-invoice
    -- override for one-off buyers, ad-hoc CC requests, and operators who
    -- want a different address on THIS invoice without churning the
    -- partner row. Pre-PR-203 rows pick up the column NULL via
    -- MIGRATE_PR_203_SQL; the resolver falls back to partner.email there.
    email_recipient_override VARCHAR,
    UNIQUE (series_id, fiscal_year, sequence_number)
);

CREATE TABLE IF NOT EXISTS invoice_line (
    invoice_id            VARCHAR NOT NULL,
    ordinal               INTEGER NOT NULL,
    description           VARCHAR NOT NULL,
    -- S157 — DECIMAL (not INTEGER) so fractional quantities (1.5 days,
    -- 0.25 hours) persist exactly. (18,6) mirrors the `exchange_rate`
    -- precision precedent and NAV's 6-decimal quantity ceiling. Pre-S157
    -- DBs created this column as INTEGER; `MIGRATE_S157_SQL` widens them
    -- (DuckDB forbids ALTER COLUMN TYPE on a CHECK-constrained column, so
    -- that path is add/backfill/drop/rename — see the constant below).
    quantity              DECIMAL(18, 6) NOT NULL,
    unit_price            BIGINT  NOT NULL,
    vat_rate_basis_points INTEGER NOT NULL,
    -- PR-82 — buyer-facing per-line note ("Megjegyzés"). Recipient-
    -- facing only; never emitted into the NAV InvoiceData XML. Nullable;
    -- pre-PR-82 rows gain the column via MIGRATE_PR_82_SQL and stay NULL.
    note                  VARCHAR,
    PRIMARY KEY (invoice_id, ordinal)
);
"#;

/// Migration ladder for PR-44γ — additive columns on the pre-existing
/// `invoice` table per ADR-0037 §3 + §1.a + §1.b.
///
/// # Backfill posture (per the session-51 brief task #5)
///
/// The five columns are added with `ADD COLUMN IF NOT EXISTS` so this
/// SQL is safe to run against:
///   - Fresh DBs (where `CREATE_TABLES_SQL` above already defined the
///     columns; the `IF NOT EXISTS` makes the ALTER a no-op).
///   - Pre-PR-44γ DBs (where the columns are missing; the ALTER adds
///     each with its DEFAULT / nullable shape).
///
/// The five new columns are added without `NOT NULL` or `DEFAULT`
/// constraints because DuckDB v1's `ALTER TABLE ADD COLUMN` rejects
/// inline constraints ("Adding columns with constraints not yet
/// supported" — verified empirically by the rollback conformance
/// tests). The follow-up `UPDATE` statement backfills `currency` to
/// `'HUF'` on pre-PR-44γ rows; the four exchange-rate columns stay
/// NULL on those rows because HUF invoices have no equivalent —
/// the regulatory record IS the HUF amount itself per ADR-0009 §1.
/// This is the C10 byte-identical invariant prerequisite: no
/// existing HUF row gains a non-trivial rate stamp.
///
/// EUR (and future non-HUF) rows MUST populate all four exchange-rate
/// columns at INSERT time — `allocate_in_tx` writes them in one step
/// per the `AllocateArgs.rate_metadata` field; a missing rate triggers
/// the typed loud-fail at the CLI boundary per ADR-0037 §4 invariant
/// C1 BEFORE the tx opens.
///
/// Read posture: a NULL `currency` is treated as HUF at read time
/// (the only value pre-PR-44γ rows could carry). Fresh DBs INSERT the
/// explicit `'HUF'` string; the two code paths converge structurally.
const MIGRATE_PR_44C_SQL: &str = "
ALTER TABLE invoice ADD COLUMN IF NOT EXISTS currency             VARCHAR;
ALTER TABLE invoice ADD COLUMN IF NOT EXISTS exchange_rate        DECIMAL(18, 6);
ALTER TABLE invoice ADD COLUMN IF NOT EXISTS exchange_rate_source VARCHAR;
ALTER TABLE invoice ADD COLUMN IF NOT EXISTS exchange_rate_date   DATE;
ALTER TABLE invoice ADD COLUMN IF NOT EXISTS huf_equivalent_total DECIMAL(18, 0);
UPDATE invoice SET currency = 'HUF' WHERE currency IS NULL;
";

/// PR-73 / ADR-0040 §addendum — additive migration for the five
/// denormalized bank-account snapshot columns. Mirrors the PR-44γ
/// posture: idempotent `ADD COLUMN IF NOT EXISTS`, no `NOT NULL` /
/// `DEFAULT` constraints (DuckDB v1's ALTER TABLE rejects inline
/// constraints — same trap PR-44γ documented). Pre-PR-73 rows stay
/// NULL across all five columns; the read path treats NULL as "no
/// bank account on file" and renders an em-dash placeholder rather
/// than fabricating a snapshot from current `seller.toml` state.
///
/// No backfill `UPDATE` runs here. Unlike PR-44γ's `currency = 'HUF'`
/// backfill (which preserved a regulatorily meaningful default for the
/// HUF-only legacy posture), there is no per-PR-73-row default that
/// would carry regulatory weight — a pre-PR-73 invoice was issued
/// against `seller.toml`'s flat-root bank slot, which PR-D will
/// re-source for those rows at render time. Backfilling a fabricated
/// snapshot here would corrupt the regulatory record per
/// CLAUDE.md rule 12 (fail loud) — silent fabrication is the trap.
const MIGRATE_PR_73_SQL: &str = "
ALTER TABLE invoice ADD COLUMN IF NOT EXISTS bank_account_id        VARCHAR;
ALTER TABLE invoice ADD COLUMN IF NOT EXISTS bank_account_currency  VARCHAR;
ALTER TABLE invoice ADD COLUMN IF NOT EXISTS bank_account_number    VARCHAR;
ALTER TABLE invoice ADD COLUMN IF NOT EXISTS bank_account_bank_name VARCHAR;
ALTER TABLE invoice ADD COLUMN IF NOT EXISTS bank_account_swift_bic VARCHAR;
";

/// PR-82 — additive migration for the two buyer-facing note columns
/// ("Megjegyzés"): `invoice.invoice_note` (per-invoice global note) and
/// `invoice_line.note` (per-line note). Idempotent via
/// `ADD COLUMN IF NOT EXISTS`; safe on fresh + pre-PR-82 DBs (the
/// `CREATE TABLE IF NOT EXISTS` posture means fresh DBs already
/// have the columns; the ALTER is a no-op there).
///
/// # Why notes live in their own migration block
///
/// The "notes never reach the NAV InvoiceData XML" invariant is
/// load-bearing (see `adr/0042-invoice-notes-never-in-nav-xml.md`).
/// Keeping the migration block named for PR-82 makes the regulatory
/// boundary visible in `git blame`: any future code change that
/// would emit notes onto the wire surfaces here as the place where
/// "notes were stored" sits beside the emitter that never reads them.
///
/// No backfill `UPDATE` runs here — pre-PR-82 rows have no note value
/// to recover, and NULL is the natural representation of "no note."
const MIGRATE_PR_82_SQL: &str = "
ALTER TABLE invoice      ADD COLUMN IF NOT EXISTS invoice_note VARCHAR;
ALTER TABLE invoice_line ADD COLUMN IF NOT EXISTS note         VARCHAR;
";

/// PR-84 — additive migration for the two invoice-date columns
/// (`payment_deadline`, `delivery_date`). Idempotent via
/// `ADD COLUMN IF NOT EXISTS`; safe on fresh + pre-PR-84 DBs.
///
/// No backfill `UPDATE` runs here — pre-PR-84 rows have no operator-
/// chosen delivery or payment date to recover (the NAV wire previously
/// mirrored issue_date for both), and NULL is the natural
/// representation of "not stored." The read path's `load_invoice`
/// treats NULL columns as "fall back to `issue_date.date()`" so
/// pre-PR-84 rows continue to render the pre-PR-84 wire/PDF behaviour
/// — identical bytes-on-disk, by design.
const MIGRATE_PR_84_SQL: &str = "
ALTER TABLE invoice ADD COLUMN IF NOT EXISTS payment_deadline DATE;
ALTER TABLE invoice ADD COLUMN IF NOT EXISTS delivery_date    DATE;
";

/// PR-203 / S203 — additive migration for the per-invoice
/// `email_recipient_override` column. Idempotent via
/// `ADD COLUMN IF NOT EXISTS`; safe on fresh + pre-PR-203 DBs (the
/// `CREATE TABLE IF NOT EXISTS` posture means fresh DBs already have
/// the column; the ALTER is a no-op there).
///
/// No backfill `UPDATE` runs here — pre-PR-203 rows had no operator-
/// chosen override to recover, and NULL is the natural representation of
/// "use the partner.email fallback" (the send-path resolver's
/// override-then-partner-fallback ladder treats NULL the same as a
/// pre-PR-203 row).
const MIGRATE_PR_203_SQL: &str = "
ALTER TABLE invoice ADD COLUMN IF NOT EXISTS email_recipient_override VARCHAR;
";

/// S157 — widen `invoice_line.quantity` from `INTEGER` to
/// `DECIMAL(18, 6)` so decimal quantities (1.5 days, 0.25 hours) persist
/// exactly rather than being rejected/truncated to whole units.
///
/// # Why this is not a one-line `ALTER COLUMN ... TYPE`
///
/// DuckDB refuses to change a column's type while a CHECK constraint
/// references it ("Binder Error: Cannot change the type of a column that
/// has a CHECK constraint specified" — verified empirically). The
/// `quantity >= 0` CHECK on the pre-S157 column blocks the direct path,
/// so we add a fresh DECIMAL column, backfill it from the legacy INTEGER
/// values, drop the old column (which carries its CHECK away), and rename
/// the new column into its place. The new column has no CHECK; the
/// issuance preflight's `LineItemQuantityZero` gate (now "must be greater
/// than zero") is the surviving positive-quantity guard.
///
/// # One-shot, not idempotent-cheap
///
/// Unlike the `ADD COLUMN IF NOT EXISTS` migrations above, this batch is
/// NOT safe to re-run every boot — a second run would drop and rebuild
/// the (now-DECIMAL) column on each launch. `ensure_schema` guards it on
/// the current column type (`quantity_column_is_integer`) so it executes
/// exactly once, on the first boot after this change against a pre-S157
/// DB. Fresh DBs create the column as DECIMAL directly (CREATE_TABLES_SQL
/// above) and skip the migration entirely.
const MIGRATE_S157_SQL: &str = "
ALTER TABLE invoice_line ADD COLUMN IF NOT EXISTS quantity_dec DECIMAL(18, 6);
UPDATE invoice_line SET quantity_dec = quantity WHERE quantity_dec IS NULL;
ALTER TABLE invoice_line DROP COLUMN IF EXISTS quantity;
ALTER TABLE invoice_line RENAME COLUMN quantity_dec TO quantity;
";

#[derive(Debug)]
pub struct DuckDbBillingStore {
    conn: Connection,
}

impl DuckDbBillingStore {
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self, BillingError> {
        let conn = Connection::open(path)?;
        // ADR-0098 R3 (finding C) — suppress DuckDB's implicit close-checkpoint
        // (in-place WAL fold, duckdb#23046) on the billing-store connection so
        // its ~10 callers inherit the guard. Exact pragma string from
        // take.rs:208 / aberp-db open_runtime_connection.
        conn.execute_batch("PRAGMA disable_checkpoint_on_shutdown;")?;
        Ok(Self { conn })
    }

    pub fn open_in_memory() -> Result<Self, BillingError> {
        Ok(Self {
            conn: Connection::open_in_memory()?,
        })
    }

    /// Take a borrowed Connection — used by PR-5's binary when it owns
    /// the tenant DB connection and wants the billing module to share it.
    pub fn from_connection(conn: Connection) -> Self {
        Self { conn }
    }

    /// S157 — `true` iff `invoice_line.quantity` still has the pre-S157
    /// `INTEGER` declared type (i.e. this DB predates the DECIMAL widening
    /// and `MIGRATE_S157_SQL` must run). DuckDB reports the type via
    /// `information_schema.columns.data_type` (e.g. `"INTEGER"` vs
    /// `"DECIMAL(18,6)"`).
    fn quantity_column_is_integer(&self) -> Result<bool, BillingError> {
        let data_type: Option<String> = self
            .conn
            .query_row(
                "SELECT data_type FROM information_schema.columns
                 WHERE table_name = 'invoice_line' AND column_name = 'quantity';",
                [],
                |r| r.get::<_, String>(0),
            )
            .ok();
        Ok(matches!(data_type, Some(t) if t.to_uppercase().contains("INT")))
    }

    /// Hand the wrapped Connection back to the caller. PR-6 uses this:
    /// the binary creates the store, drives idempotent pre-tx setup
    /// (`ensure_schema`, `ensure_series`) through the trait, then takes
    /// the Connection back so it can `Connection::transaction()` and
    /// drive [`allocate_in_tx`] + audit-ledger appends inside the same
    /// tx. The store cannot be reused after this call.
    pub fn into_connection(self) -> Connection {
        self.conn
    }
}

/// S392 — resolve the `(series_id_str, fiscal_year)` bucket key for an
/// invoice issued in `issue_year`. Reads `reset_policy` from the series
/// row so the counter buckets identically whether reached via
/// [`allocate_in_tx`] (which then advances the counter) or
/// [`peek_next_number`] (read-only). Loud-fails when the series is absent
/// (callers ensure it upstream). Takes `&Connection` so both a borrowed
/// `Transaction` (via `Deref`) and a bare `Connection` can call it.
fn resolve_bucket(
    conn: &Connection,
    series_id: SeriesId,
    issue_year: i32,
) -> Result<(String, i32), BillingError> {
    let series = {
        let mut stmt = conn.prepare(
            "SELECT id, code, reset_policy, fiscal_year, created_at
             FROM invoice_series WHERE id = ?;",
        )?;
        let mut rows = stmt.query_map([series_id.to_prefixed_string()], row_to_series)?;
        match rows.next() {
            Some(r) => r?,
            None => return Err(BillingError::SeriesNotFound(series_id.to_prefixed_string())),
        }
    };
    // PR-90 / ADR-0045 §2: `AnnualOnFiscalYear` keys the bucket on the
    // invoice's immutable issue-date year (NOT wall-clock); `Never` keeps
    // fiscal_year=0 (continuous bucket, byte-identical for legacy rows).
    let fiscal_year = match series.reset_policy {
        ResetPolicy::Never => 0,
        ResetPolicy::AnnualOnFiscalYear => issue_year,
    };
    Ok((series.id.to_prefixed_string(), fiscal_year))
}

/// S392 — read the next sequence number [`allocate_in_tx`] would assign
/// for the `(series, issue_year)` bucket WITHOUT advancing it. Returns
/// `max(stored_next_number, start_value)`, or `start_value.max(1)` when
/// the bucket has no state row yet — mirroring `allocate_in_tx`, which
/// (S394) floors every allocation by `start_value`, not just the seed.
///
/// The binary's NAV pre-flight (`apps/aberp` `issue_from_parsed`) calls
/// this to learn which candidate numbers to existence-check against NAV
/// (`queryInvoiceCheck`) BEFORE the allocator transaction opens; the
/// first NAV-clear number is then threaded back as
/// [`AllocateArgs::sequence_floor`]. Read-only: takes `&Connection` and
/// issues a single `SELECT`, so it composes with the pre-tx Connection
/// the caller already holds.
pub fn peek_next_number(
    conn: &Connection,
    series_id: SeriesId,
    issue_year: i32,
    start_value: u64,
) -> Result<u64, BillingError> {
    let (series_id_str, fiscal_year) = resolve_bucket(conn, series_id, issue_year)?;
    let mut stmt = conn.prepare(
        "SELECT next_number FROM invoice_sequence_state
         WHERE series_id = ? AND fiscal_year = ?;",
    )?;
    let mut rows = stmt.query_map(params![&series_id_str, fiscal_year], |r| r.get::<_, i64>(0))?;
    match rows.next() {
        // S394 — floor the stored counter by `start_value` so the NAV
        // pre-flight probes the number `allocate_in_tx` will actually
        // reserve (which also maxes against `start_value`). Without this
        // floor the probe could clear, say, 41 while the allocator jumps
        // to the operator's `start_value` of 56 — a number NAV never
        // checked. `start_value <= next_number` (default-1, steady state)
        // leaves the stored value untouched, byte-identical to pre-S394.
        Some(r) => Ok((r? as u64).max(start_value)),
        None => Ok(start_value.max(1)),
    }
}

/// Run the ADR-0009 §3 "Allocate (atomic)" sequence inside a borrowed
/// [`duckdb::Transaction`]. Caller owns commit/rollback.
///
/// This is the body the [`BillingStore::allocate_and_insert`] trait impl
/// delegates to. The binary path (PR-6) calls this directly so that the
/// audit-ledger appends for the issuance can ride the same transaction
/// per ADR-0008 §Storage: "Entries are written in the same transaction
/// as the state change they describe."
///
/// Crash/error semantics: the caller's `Transaction` rolls back on drop
/// when not committed, so a return of `Err(_)` or a panic between this
/// call and `tx.commit()` leaves the tenant DB unchanged. Conformance
/// tests in `apps/aberp/tests/rollback_conformance.rs` exercise both
/// paths.
pub fn allocate_in_tx(
    tx: &duckdb::Transaction<'_>,
    args: AllocateArgs,
    now: OffsetDateTime,
) -> Result<AllocateOutcome, BillingError> {
    // ── Pre-flight validation. `IssueInvoiceCommand` handler does these
    //    before delegating to the store; PR-6 callers that drive
    //    `allocate_in_tx` directly (the binary path) bypass the handler,
    //    so we re-assert here to fail loud at the lowest write surface
    //    rather than silently allowing a zero-line or overflowing
    //    invoice to be written.
    if args.draft.lines.is_empty() {
        return Err(BillingError::Invalid(
            "DraftInvoice.lines must contain at least one line item",
        ));
    }
    for (line_index, line) in args.draft.lines.iter().enumerate() {
        if line.gross_total().is_none() {
            return Err(BillingError::MoneyOverflow { line_index });
        }
    }

    // ── ADR-0009 §5 Layer 1 idempotency check. Same idempotency key
    //    => return the prior outcome unchanged. Hit before any number
    //    is burned.
    //
    // PR-49 — must be the canonical `idem_<ULID>` form so the column
    // round-trips through `load_ready_invoice_by_id`
    // (`IdempotencyKey::from_canonical_string`). PR-6's original write
    // used the bare `Ulid::to_string()`; PR-7-B-1 introduced the
    // canonical-form read on top without a round-trip pin, so the
    // mismatch lay latent until the SPA's list+detail handlers (PR-44ζ
    // session 59) first issued a row and immediately read it back.
    let idem_str = args.idempotency_key.to_canonical_string();
    let prior: Option<String> = {
        let mut stmt = tx.prepare("SELECT id FROM invoice WHERE idempotency_key = ?;")?;
        let mut rows = stmt.query_map([&idem_str], |r| r.get::<_, String>(0))?;
        match rows.next() {
            Some(r) => Some(r?),
            None => None,
        }
    };

    if let Some(prior_invoice_id) = prior {
        let invoice = load_invoice(tx, &prior_invoice_id)?;
        let reservation = load_reservation_by_invoice(tx, &prior_invoice_id)?;
        // No commit here — caller controls the tx boundary. Replay must
        // still close cleanly so the caller's tx can also commit any
        // sibling work (audit-ledger appends are skipped on replay per
        // `apps/aberp/src/issue_invoice.rs`).
        return Ok(AllocateOutcome::Replay {
            invoice,
            reservation,
        });
    }

    // ── Resolve series + fiscal_year (S392 — shared with `peek_next_number`).
    let (series_id_str, fiscal_year) =
        resolve_bucket(tx, args.series_id, args.draft.issue_date.year())?;

    // ── ADR-0009 §3 step 1+3: read next_number (creating the row at
    //    `args.start_value` if absent), then UPDATE to advance.
    //
    // PR-90 — `args.start_value` is the operator's template seed
    // (default 1). It is used ONLY on the first INSERT into the bucket;
    // subsequent allocations into the same bucket read + advance the
    // stored `next_number`, preserving the §169 gap-free invariant
    // within each `(series_id, fiscal_year)` bucket.
    let now_str = now.format(&Rfc3339)?;
    let next_number: u64 = {
        let mut stmt = tx.prepare(
            "SELECT next_number FROM invoice_sequence_state
             WHERE series_id = ? AND fiscal_year = ?;",
        )?;
        let mut rows =
            stmt.query_map(params![&series_id_str, fiscal_year], |r| r.get::<_, i64>(0))?;
        match rows.next() {
            Some(r) => r? as u64,
            None => {
                // First allocation for this series/fiscal_year — seed
                // the state row at `start_value` (we are about to burn
                // a number and advance below).
                let seed = args.start_value.max(1);
                tx.execute(
                    "INSERT INTO invoice_sequence_state
                     (series_id, fiscal_year, next_number, updated_at)
                     VALUES (?, ?, ?, ?);",
                    params![&series_id_str, fiscal_year, seed as i64, &now_str],
                )?;
                seed
            }
        }
    };

    // The number actually reserved is the largest of three floors:
    //
    //   * the stored `next_number` (gap-free steady-state advance),
    //   * S394 — the operator's `start_value`, honoured on EVERY
    //     allocation rather than only the first-INSERT seed. Raising
    //     `[seller.numbering].start_value` above the current counter now
    //     takes effect immediately (operator mental model: "I set 56 →
    //     next is 56"; the SPA preview already renders the next number AT
    //     start_value). When `start_value <= next_number` — the default-1
    //     case and all steady-state operation — this is a no-op, so the
    //     §169 gap-free invariant and the pre-S394 byte stream both hold.
    //   * S392 — the NAV pre-flight floor: the first `queryInvoiceCheck`-
    //     clear number, so a fresh local sequence skips past any number
    //     NAV's shared TEST endpoint already holds. `None` (production +
    //     the no-skip case) contributes 0.
    //
    // A jump past `next_number` (operator-raised start_value OR a NAV
    // skip) burns the skipped range as deliberate gaps — those numbers
    // were either issued upstream or intentionally vacated by the
    // operator, so the gap-free invariant is knowingly relaxed here.
    let allocated: u64 = next_number
        .max(args.start_value)
        .max(args.sequence_floor.unwrap_or(0));

    // Advance the stored counter to `allocated + 1`. Equivalent to the
    // pre-S392 `next_number + 1` when no floor jump occurred; when the
    // floor jumped past `next_number`, the skipped range is burned (left
    // as deliberate gaps — those numbers were issued in a prior cycle).
    tx.execute(
        "UPDATE invoice_sequence_state
         SET next_number = ?, updated_at = ?
         WHERE series_id = ? AND fiscal_year = ?;",
        params![
            (allocated + 1) as i64,
            &now_str,
            &series_id_str,
            fiscal_year
        ],
    )?;

    // ── ADR-0009 §3 step 4: insert the reservation.
    let reservation_id = ReservationId::new();
    let draft = args.draft;
    tx.execute(
        "INSERT INTO invoice_sequence_reservation
         (id, series_id, fiscal_year, number, invoice_id, status,
          void_reason, reserved_at, used_at, voided_at)
         VALUES (?, ?, ?, ?, ?, 'reserved', NULL, ?, NULL, NULL);",
        params![
            reservation_id.to_prefixed_string(),
            &series_id_str,
            fiscal_year,
            allocated as i64,
            draft.id.to_prefixed_string(),
            &now_str,
        ],
    )?;

    // ── ADR-0009 §3 step 5: insert the invoice row + line items.
    //
    // PR-44γ — the seven pre-PR-44γ columns are joined by the five
    // PR-44γ additive columns per ADR-0037 §3 + §1.a + §1.b. HUF rows
    // pass `currency = "HUF"` + four NULL rate columns (the C10
    // byte-identical invariant prerequisite: HUF rows continue to
    // carry NO rate metadata, mirroring their pre-PR-44γ shape).
    // Non-HUF rows pass the full rate stamp; `allocate_in_tx`'s
    // pre-flight refuses non-HUF rows lacking rate metadata.
    let issue_date_str = draft.issue_date.format(&Rfc3339)?;
    let currency_iso = args.currency.iso_code();
    // Pre-flight: ADR-0037 §4 invariant C1 — refuse non-HUF allocation
    // when rate metadata is missing. The CLI surfaces this loud per
    // CLAUDE.md rule 12; here it is the lowest write boundary defense.
    if !matches!(args.currency, Currency::Huf) && args.rate_metadata.is_none() {
        return Err(BillingError::Invalid(
            "non-HUF invoice requires AllocateArgs.rate_metadata (ADR-0037 §4 C1)",
        ));
    }
    let date_fmt = format_description!("[year]-[month]-[day]");
    // The rate-metadata fields are written as four nullable column
    // parameters. We bind them as `Option<String>` for the decimal
    // columns (DuckDB casts the string at column-write per the
    // declared `DECIMAL(...)` type) and `Option<String>` for the
    // DATE column (DuckDB casts ISO-8601 `YYYY-MM-DD` to DATE).
    // `rust_decimal::Decimal::to_string` emits a canonical decimal
    // form that DuckDB accepts.
    let rate_value_str: Option<String> = args.rate_metadata.as_ref().map(|r| r.rate.to_string());
    let rate_source: Option<String> = args.rate_metadata.as_ref().map(|r| r.source.clone());
    let rate_date_str: Option<String> = args
        .rate_metadata
        .as_ref()
        .map(|r| r.date.format(&date_fmt))
        .transpose()
        .map_err(|_| BillingError::Invalid("rate_metadata.date formatting failed"))?;
    let huf_eq_str: Option<String> = args
        .rate_metadata
        .as_ref()
        .map(|r| r.huf_equivalent_total.to_string());
    // PR-73 / ADR-0040 §addendum — bank-account snapshot columns. All
    // five columns are written together iff `bank_snapshot` is `Some(_)`;
    // a `None` snapshot writes all five as NULL (the pre-PR-73 row
    // shape). The route resolver (`serve::resolve_bank_snapshot`)
    // refuses to call into `allocate_in_tx` with a snapshot whose
    // currency mismatches `args.currency` — the typed preflight
    // surfaces `SellerBankCurrencyMismatch` BEFORE we reach this
    // boundary. As defence in depth, the read path's
    // `InvoiceBankSnapshot.currency` reads back the value verbatim;
    // a downstream renderer that wants invariant safety can re-check
    // against the invoice's `currency` column.
    let bank_account_id = args.bank_snapshot.as_ref().map(|b| b.id.clone());
    let bank_account_currency = args.bank_snapshot.as_ref().map(|b| b.currency.clone());
    let bank_account_number = args
        .bank_snapshot
        .as_ref()
        .map(|b| b.account_number.clone());
    let bank_account_bank_name = args.bank_snapshot.as_ref().map(|b| b.bank_name.clone());
    let bank_account_swift_bic = args.bank_snapshot.as_ref().map(|b| b.swift_bic.clone());
    // PR-82 — buyer-facing invoice-level note ("Megjegyzés"). Persisted
    // to the new `invoice.invoice_note` column. NEVER reaches the NAV
    // InvoiceData XML — the emitter in `apps/aberp/src/nav_xml.rs` does
    // not consume it, and the "never-leak" pin in
    // `apps/aberp/tests/nav_xml_notes_never_leak.rs` enforces the
    // byte-identical invariant on the wire output.
    let invoice_note = args.invoice_note.clone();
    // PR-203 / S203 — per-invoice email recipient override. Comma-
    // separated address list (canonical `", "` separator); NULL when
    // the operator left the field blank. Persisted verbatim so the
    // send-path resolver (`resolve_recipient_email` in `serve.rs`)
    // consults it as the first rung of the override-then-partner-
    // fallback-then-skip ladder. The wire validator at the route
    // boundary already rejected malformed shapes; this seam stores
    // operator-typed truth.
    let email_recipient_override = args.email_recipient_override.clone();
    // PR-84 — invoice-date columns rendered as canonical YYYY-MM-DD
    // strings (DuckDB's DATE type casts the ISO string at column-write
    // per the declared `DATE` type, same posture as `exchange_rate_date`).
    let payment_deadline_str = draft
        .payment_deadline
        .format(&date_fmt)
        .map_err(|_| BillingError::Invalid("draft.payment_deadline formatting failed"))?;
    let delivery_date_str = draft
        .delivery_date
        .format(&date_fmt)
        .map_err(|_| BillingError::Invalid("draft.delivery_date formatting failed"))?;
    tx.execute(
        "INSERT INTO invoice
         (id, series_id, customer_id, issue_date, sequence_number,
          fiscal_year, idempotency_key,
          currency, exchange_rate, exchange_rate_source,
          exchange_rate_date, huf_equivalent_total,
          bank_account_id, bank_account_currency, bank_account_number,
          bank_account_bank_name, bank_account_swift_bic,
          invoice_note,
          payment_deadline, delivery_date,
          email_recipient_override)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?);",
        params![
            draft.id.to_prefixed_string(),
            &series_id_str,
            draft.customer_id.to_prefixed_string(),
            &issue_date_str,
            allocated as i64,
            fiscal_year,
            &idem_str,
            currency_iso,
            &rate_value_str,
            &rate_source,
            &rate_date_str,
            &huf_eq_str,
            &bank_account_id,
            &bank_account_currency,
            &bank_account_number,
            &bank_account_bank_name,
            &bank_account_swift_bic,
            &invoice_note,
            &payment_deadline_str,
            &delivery_date_str,
            &email_recipient_override,
        ],
    )?;

    for (ordinal, line) in draft.lines.iter().enumerate() {
        // PR-82 — per-line note ("Megjegyzés") persisted alongside the
        // line content. Same "never on the wire" posture as
        // `invoice.invoice_note`; the per-line emitter in
        // `nav_xml::render_line` does not read this field.
        tx.execute(
            "INSERT INTO invoice_line
             (invoice_id, ordinal, description, quantity,
              unit_price, vat_rate_basis_points, note)
             VALUES (?, ?, ?, ?, ?, ?, ?);",
            params![
                draft.id.to_prefixed_string(),
                ordinal as i64,
                &line.description,
                // S157 — bind the quantity as its canonical decimal string;
                // DuckDB casts it to the column's DECIMAL type at write,
                // same posture as `exchange_rate` (Decimal-as-string bind).
                line.quantity.to_string(),
                line.unit_price.as_i64(),
                line.vat_rate_basis_points as i64,
                &line.note,
            ],
        )?;
    }

    // ── ADR-0009 §3 step 6: audit-ledger entries land via the caller
    //    using the same `tx`. Step 7 (commit) is also the caller's.
    let invoice = ReadyInvoice {
        id: draft.id,
        series_id: draft.series_id,
        customer_id: draft.customer_id,
        lines: draft.lines,
        issue_date: draft.issue_date,
        payment_deadline: draft.payment_deadline,
        delivery_date: draft.delivery_date,
        sequence_number: allocated,
        fiscal_year,
    };
    let reservation = SequenceReservation {
        id: reservation_id,
        series_id: args.series_id,
        fiscal_year,
        number: allocated,
        invoice_id: invoice.id,
        status: ReservationStatus::Reserved,
        void_reason: None,
        reserved_at: now,
        used_at: None,
        voided_at: None,
    };

    Ok(AllocateOutcome::Fresh {
        invoice,
        reservation,
    })
}

impl BillingStore for DuckDbBillingStore {
    fn ensure_schema(&mut self) -> Result<(), BillingError> {
        self.conn.execute_batch(CREATE_TABLES_SQL)?;
        // PR-44γ — additive migration for the five non-HUF columns on
        // `invoice`. Idempotent via `ADD COLUMN IF NOT EXISTS`; safe on
        // fresh + pre-PR-44γ DBs. See `MIGRATE_PR_44C_SQL`'s comment
        // block for the backfill posture.
        self.conn.execute_batch(MIGRATE_PR_44C_SQL)?;
        // PR-73 — additive migration for the five denormalized
        // bank-account snapshot columns. Same idempotent posture; pre-
        // PR-73 rows stay NULL across all five (no backfill — the
        // regulatory record forbids fabricating a snapshot).
        self.conn.execute_batch(MIGRATE_PR_73_SQL)?;
        // PR-82 — additive migration for the two buyer-facing note
        // columns. Idempotent; old DBs gain `invoice.invoice_note` +
        // `invoice_line.note` on first boot.
        self.conn.execute_batch(MIGRATE_PR_82_SQL)?;
        // PR-84 — additive migration for the operator-supplied
        // payment_deadline + delivery_date columns. Old DBs gain the
        // columns NULL; the read path falls back to `issue_date` for
        // NULL rows so byte-on-wire behaviour is preserved for pre-PR-84
        // invoices.
        self.conn.execute_batch(MIGRATE_PR_84_SQL)?;
        // PR-203 / S203 — additive migration for the per-invoice
        // `email_recipient_override` column. Idempotent; old DBs gain it
        // NULL and the send-path resolver continues falling back to
        // partner.email for those rows.
        self.conn.execute_batch(MIGRATE_PR_203_SQL)?;
        // S157 — widen `invoice_line.quantity` to DECIMAL on pre-S157 DBs.
        // Guarded on the column type so the (non-idempotent-cheap)
        // drop/rename rebuild runs exactly once; fresh DBs already created
        // the column as DECIMAL and skip it.
        if self.quantity_column_is_integer()? {
            self.conn.execute_batch(MIGRATE_S157_SQL)?;
        }
        Ok(())
    }

    fn create_series(&mut self, series: &InvoiceSeries) -> Result<(), BillingError> {
        let reset_policy = reset_policy_to_str(series.reset_policy);
        self.conn.execute(
            "INSERT INTO invoice_series (id, code, reset_policy, fiscal_year, created_at)
             VALUES (?, ?, ?, ?, ?);",
            params![
                series.id.to_prefixed_string(),
                series.code.as_str(),
                reset_policy,
                series.fiscal_year,
                series.created_at.format(&Rfc3339)?,
            ],
        )?;
        Ok(())
    }

    fn find_series_by_code(
        &self,
        code: &SeriesCode,
    ) -> Result<Option<InvoiceSeries>, BillingError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, code, reset_policy, fiscal_year, created_at
             FROM invoice_series WHERE code = ?;",
        )?;
        let mut rows = stmt.query_map([code.as_str()], row_to_series)?;
        match rows.next() {
            Some(r) => Ok(Some(r?)),
            None => Ok(None),
        }
    }

    fn find_series_by_id(&self, id: SeriesId) -> Result<Option<InvoiceSeries>, BillingError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, code, reset_policy, fiscal_year, created_at
             FROM invoice_series WHERE id = ?;",
        )?;
        let mut rows = stmt.query_map([id.to_prefixed_string()], row_to_series)?;
        match rows.next() {
            Some(r) => Ok(Some(r?)),
            None => Ok(None),
        }
    }

    fn update_series_reset_policy(
        &mut self,
        id: SeriesId,
        policy: ResetPolicy,
    ) -> Result<(), BillingError> {
        let changed = self.conn.execute(
            "UPDATE invoice_series SET reset_policy = ? WHERE id = ?;",
            params![reset_policy_to_str(policy), id.to_prefixed_string()],
        )?;
        if changed != 1 {
            return Err(BillingError::SeriesNotFound(id.to_prefixed_string()));
        }
        Ok(())
    }

    fn allocate_and_insert(
        &mut self,
        args: AllocateArgs,
        now: OffsetDateTime,
    ) -> Result<AllocateOutcome, BillingError> {
        // Thin wrapper: open a tx, delegate to the free function, commit.
        // The body of the allocator lives in `allocate_in_tx` so the
        // binary can drive a single tx that also covers the audit-ledger
        // appends (ADR-0009 §3 step 6, closed in PR-6).
        let tx = self.conn.transaction()?;
        let outcome = allocate_in_tx(&tx, args, now)?;
        tx.commit()?;
        Ok(outcome)
    }

    fn void_reservation(
        &mut self,
        invoice_id: InvoiceId,
        void_reason: String,
        voided_at: OffsetDateTime,
    ) -> Result<(), BillingError> {
        let voided_at_str = voided_at.format(&Rfc3339)?;
        let changed = self.conn.execute(
            "UPDATE invoice_sequence_reservation
             SET status = 'voided', void_reason = ?, voided_at = ?
             WHERE invoice_id = ? AND status = 'reserved';",
            params![
                &void_reason,
                &voided_at_str,
                invoice_id.to_prefixed_string()
            ],
        )?;
        if changed != 1 {
            return Err(BillingError::Invalid(
                "no Reserved reservation found for that invoice_id",
            ));
        }
        Ok(())
    }

    fn list_reservations(
        &self,
        series_id: SeriesId,
    ) -> Result<Vec<SequenceReservation>, BillingError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, series_id, fiscal_year, number, invoice_id, status,
                    void_reason, reserved_at, used_at, voided_at
             FROM invoice_sequence_reservation
             WHERE series_id = ?
             ORDER BY fiscal_year ASC, number ASC;",
        )?;
        let rows = stmt.query_map([series_id.to_prefixed_string()], row_to_reservation)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }
}

// ── Decoding helpers ──────────────────────────────────────────────────

fn reset_policy_to_str(p: ResetPolicy) -> &'static str {
    match p {
        ResetPolicy::Never => "never",
        ResetPolicy::AnnualOnFiscalYear => "annual_on_fiscal_year",
    }
}

fn reset_policy_from_str(s: &str) -> Option<ResetPolicy> {
    match s {
        "never" => Some(ResetPolicy::Never),
        "annual_on_fiscal_year" => Some(ResetPolicy::AnnualOnFiscalYear),
        _ => None,
    }
}

/// Map the stored `status` string to [`ReservationStatus`], rejecting any
/// out-of-vocab value. S410 / [[no-sql-specific]] — this is the
/// authoritative closed-vocab gate now that the DB-level
/// `CHECK (status IN ('reserved','used','voided'))` is gone. Writes use
/// the SQL literals `'reserved'` / `'voided'` directly, so the only way a
/// bad value reaches here is engine/import drift, which this rejects loudly.
fn reservation_status_from_str(s: &str) -> Option<ReservationStatus> {
    match s {
        "reserved" => Some(ReservationStatus::Reserved),
        "used" => Some(ReservationStatus::Used),
        "voided" => Some(ReservationStatus::Voided),
        _ => None,
    }
}

fn row_to_series(row: &duckdb::Row<'_>) -> duckdb::Result<InvoiceSeries> {
    let id_str: String = row.get(0)?;
    let code_str: String = row.get(1)?;
    let rp_str: String = row.get(2)?;
    let fy: Option<i32> = row.get(3)?;
    let created_str: String = row.get(4)?;

    let id_ulid = parse_prefixed_ulid(&id_str, "srs")?;
    let code =
        SeriesCode::new(code_str).ok_or_else(|| decode_err("series.code failed validation"))?;
    let reset_policy =
        reset_policy_from_str(&rp_str).ok_or_else(|| decode_err("unknown reset_policy"))?;
    let created_at = OffsetDateTime::parse(&created_str, &Rfc3339)
        .map_err(|_| decode_err("series.created_at not RFC3339"))?;

    Ok(InvoiceSeries {
        id: SeriesId(id_ulid),
        code,
        reset_policy,
        fiscal_year: fy,
        created_at,
    })
}

fn row_to_reservation(row: &duckdb::Row<'_>) -> duckdb::Result<SequenceReservation> {
    let id_str: String = row.get(0)?;
    let series_id_str: String = row.get(1)?;
    let fiscal_year: i32 = row.get(2)?;
    let number: i64 = row.get(3)?;
    let invoice_id_str: String = row.get(4)?;
    let status_str: String = row.get(5)?;
    let void_reason: Option<String> = row.get(6)?;
    let reserved_at_str: String = row.get(7)?;
    let used_at_str: Option<String> = row.get(8)?;
    let voided_at_str: Option<String> = row.get(9)?;

    let status = reservation_status_from_str(&status_str)
        .ok_or_else(|| decode_err("unknown reservation.status"))?;
    let reserved_at = OffsetDateTime::parse(&reserved_at_str, &Rfc3339)
        .map_err(|_| decode_err("reservation.reserved_at not RFC3339"))?;
    let used_at = match used_at_str {
        Some(s) => Some(
            OffsetDateTime::parse(&s, &Rfc3339)
                .map_err(|_| decode_err("reservation.used_at not RFC3339"))?,
        ),
        None => None,
    };
    let voided_at = match voided_at_str {
        Some(s) => Some(
            OffsetDateTime::parse(&s, &Rfc3339)
                .map_err(|_| decode_err("reservation.voided_at not RFC3339"))?,
        ),
        None => None,
    };

    Ok(SequenceReservation {
        id: ReservationId(parse_prefixed_ulid(&id_str, "rsv")?),
        series_id: SeriesId(parse_prefixed_ulid(&series_id_str, "srs")?),
        fiscal_year,
        number: number as u64,
        invoice_id: InvoiceId(parse_prefixed_ulid(&invoice_id_str, "inv")?),
        status,
        void_reason,
        reserved_at,
        used_at,
        voided_at,
    })
}

/// Load a previously-issued `ReadyInvoice` plus the
/// [`crate::app::issue_invoice::IdempotencyKey`] that was burned with
/// it. Used by `apps/aberp/src/submit_invoice.rs` (PR-7-B-3) to feed
/// `manageInvoice` and to thread the same idempotency key into the new
/// audit-ledger entries so the F8 contract (every NAV-related entry
/// for an invoice carries the same idempotency key) holds across
/// issue + submit.
///
/// Returns `Ok(None)` if no invoice with that id exists — the caller
/// surfaces that as a loud operator error, not as a silent fallback
/// to "issue a new one" (CLAUDE.md rule 12).
///
/// `invoice_id` is the ULID-prefixed form (`inv_XXXX...`). Free
/// function (not a trait method) for the same reason
/// [`allocate_in_tx`] is: the binary owns the `Transaction` lifecycle
/// and calls this inside its own tx that also drives audit-ledger
/// appends per ADR-0008 §Storage.
pub fn load_ready_invoice_by_id(
    tx: &duckdb::Transaction<'_>,
    invoice_id: &str,
) -> Result<Option<(ReadyInvoice, crate::app::issue_invoice::IdempotencyKey)>, BillingError> {
    // Two reads: the invoice row (for the idempotency key) + the lines
    // via the existing `load_invoice`. Doing both in one method keeps
    // the binary's submit_invoice.rs from having to compose two billing
    // calls in the same tx (and reach for `idempotency_key` parsing
    // logic that already lives in this crate).
    let idem_str: Option<String> = {
        let mut stmt = tx.prepare("SELECT idempotency_key FROM invoice WHERE id = ?;")?;
        let mut rows = stmt.query_map([invoice_id], |r| r.get::<_, String>(0))?;
        match rows.next() {
            Some(r) => Some(r?),
            None => None,
        }
    };
    let idem_str = match idem_str {
        Some(s) => s,
        None => return Ok(None),
    };
    let idempotency_key = crate::app::issue_invoice::IdempotencyKey::from_canonical_string(
        &idem_str,
    )
    .ok_or(BillingError::Invalid(
        "stored idempotency_key failed to parse — DB has been hand-edited",
    ))?;
    let invoice = load_invoice(tx, invoice_id)?;
    Ok(Some((invoice, idempotency_key)))
}

fn load_invoice(
    tx: &duckdb::Transaction<'_>,
    invoice_id_str: &str,
) -> Result<ReadyInvoice, BillingError> {
    // PR-84 — `payment_deadline` and `delivery_date` are NULLable
    // (pre-PR-84 rows). The read path treats NULL as "fall back to
    // `issue_date`" so a pre-PR-84 invoice continues to render the
    // pre-PR-84 wire behaviour (delivery + payment mirror issue) when
    // it round-trips through this loader — same posture the NAV emit
    // had before PR-84. Fresh rows post-PR-84 always carry both.
    let (
        series_id_str,
        customer_id_str,
        issue_date_str,
        seq_number,
        fiscal_year,
        payment_deadline_str,
        delivery_date_str,
    ) = {
        let mut stmt = tx.prepare(
            "SELECT series_id, customer_id, issue_date, sequence_number, fiscal_year,
                    CAST(payment_deadline AS VARCHAR), CAST(delivery_date AS VARCHAR)
             FROM invoice WHERE id = ?;",
        )?;
        let mut rows = stmt.query_map([invoice_id_str], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i32>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<String>>(6)?,
            ))
        })?;
        match rows.next() {
            Some(r) => r?,
            None => return Err(BillingError::Invalid("invoice missing for idempotency hit")),
        }
    };

    // Load lines in ordinal order.
    //
    // PR-82 — `note` column included in the SELECT so per-line buyer
    // notes round-trip through every load path (SPA detail, storno
    // base-read for chain content, PDF re-render). The column is
    // nullable; pre-PR-82 rows decode as `note: None`.
    // S157 — `CAST(quantity AS VARCHAR)` reads the DECIMAL column as its
    // canonical string (same posture as the DATE columns above), then
    // `Decimal::from_str` reconstructs it exactly. Pre-S157 rows widened
    // by `MIGRATE_S157_SQL` read back as e.g. `"3.000000"` (whole values
    // gain trailing zeros from the DECIMAL(18,6) scale); the emit/render
    // layers `.normalize()` those away.
    let mut stmt = tx.prepare(
        "SELECT description, CAST(quantity AS VARCHAR), unit_price, vat_rate_basis_points, note
         FROM invoice_line WHERE invoice_id = ? ORDER BY ordinal ASC;",
    )?;
    let rows = stmt.query_map([invoice_id_str], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, Option<String>>(4)?,
        ))
    })?;
    let mut lines = Vec::new();
    for r in rows {
        let (description, quantity_str, unit_price, vat, note) = r?;
        let quantity = Decimal::from_str(&quantity_str)
            .map_err(|_| BillingError::Invalid("stored invoice_line.quantity is not a decimal"))?;
        lines.push(LineItem {
            description,
            quantity,
            unit_price: Huf(unit_price),
            vat_rate_basis_points: vat as u16,
            note,
            // S159 — the unit is NOT persisted on `invoice_line` (no
            // column; it rides the side-store `input.json` per-line
            // payload). A DB-reconstructed line therefore carries
            // `None`, falling back to `<unitOfMeasure>PIECE</...>`. This
            // path serves the idempotent Replay branch; fresh issuance +
            // storno / modification re-emit from `input.json`, which DOES
            // carry the unit.
            unit: None,
        });
    }

    let issue_date = OffsetDateTime::parse(&issue_date_str, &Rfc3339)?;
    // PR-84 — NULL date columns fall back to `issue_date.date()` so
    // pre-PR-84 rows preserve their pre-PR-84 wire behaviour
    // byte-identically. Post-PR-84 rows always carry both columns
    // non-NULL.
    let date_fmt = format_description!("[year]-[month]-[day]");
    let payment_deadline = match payment_deadline_str {
        Some(s) => time::Date::parse(&s, &date_fmt)
            .map_err(|_| BillingError::Invalid("invoice.payment_deadline not YYYY-MM-DD"))?,
        None => issue_date.date(),
    };
    let delivery_date = match delivery_date_str {
        Some(s) => time::Date::parse(&s, &date_fmt)
            .map_err(|_| BillingError::Invalid("invoice.delivery_date not YYYY-MM-DD"))?,
        None => issue_date.date(),
    };
    Ok(ReadyInvoice {
        id: InvoiceId(parse_prefixed_ulid(invoice_id_str, "inv")?),
        series_id: SeriesId(parse_prefixed_ulid(&series_id_str, "srs")?),
        customer_id: CustomerId(parse_prefixed_ulid(&customer_id_str, "cus")?),
        lines,
        issue_date,
        payment_deadline,
        delivery_date,
        sequence_number: seq_number as u64,
        fiscal_year,
    })
}

/// PR-82 — read the per-invoice global note ("Megjegyzés") off the
/// `invoice.invoice_note` column. `None` when the column is NULL
/// (operator did not supply a note OR pre-PR-82 row) AND when the
/// invoice row does not exist (the caller's wider read path surfaces
/// the missing-invoice case as `None` from `load_ready_invoice_by_id`).
///
/// Free function (not a `BillingStore` trait method) for the same reason
/// `load_ready_invoice_by_id` is: the binary owns the `Transaction`
/// lifecycle and calls this inside its own tx that also drives audit-
/// ledger appends or sibling reads.
///
/// NEVER consulted by the NAV XML emitter — the note column is
/// recipient-facing storage only (see
/// `adr/0042-invoice-notes-never-in-nav-xml.md`).
pub fn load_invoice_note_in_tx(
    tx: &duckdb::Transaction<'_>,
    invoice_id_str: &str,
) -> Result<Option<String>, BillingError> {
    let mut stmt = tx.prepare("SELECT invoice_note FROM invoice WHERE id = ?;")?;
    let mut rows = stmt.query_map([invoice_id_str], |r| r.get::<_, Option<String>>(0))?;
    match rows.next() {
        Some(r) => Ok(r?),
        None => Ok(None),
    }
}

/// PR-203 / S203 — read the per-invoice email recipient override off the
/// `invoice.email_recipient_override` column. `None` when the column is
/// NULL (operator left the field blank OR pre-PR-203 row) AND when the
/// invoice row does not exist (caller path surfaces the missing-invoice
/// case as `None`).
///
/// Same posture as [`load_invoice_note_in_tx`]: free function so the
/// binary's send-path resolver can call it inside its own read tx
/// alongside other invoice-column reads (no `BillingStore` trait method).
///
/// Returned string is the OPERATOR-TYPED comma-separated address list;
/// the caller (`serve::resolve_recipient_email`) feeds it through
/// `partners::parse_emails` to split into individual addresses for the
/// `Mailbox` build. NEVER consulted by the NAV XML emitter — the column
/// is recipient-routing storage only.
pub fn load_email_recipient_override_in_tx(
    tx: &duckdb::Transaction<'_>,
    invoice_id_str: &str,
) -> Result<Option<String>, BillingError> {
    let mut stmt = tx.prepare("SELECT email_recipient_override FROM invoice WHERE id = ?;")?;
    let mut rows = stmt.query_map([invoice_id_str], |r| r.get::<_, Option<String>>(0))?;
    match rows.next() {
        Some(r) => Ok(r?),
        None => Ok(None),
    }
}

fn load_reservation_by_invoice(
    tx: &duckdb::Transaction<'_>,
    invoice_id_str: &str,
) -> Result<SequenceReservation, BillingError> {
    let mut stmt = tx.prepare(
        "SELECT id, series_id, fiscal_year, number, invoice_id, status,
                void_reason, reserved_at, used_at, voided_at
         FROM invoice_sequence_reservation WHERE invoice_id = ?;",
    )?;
    let mut rows = stmt.query_map([invoice_id_str], row_to_reservation)?;
    match rows.next() {
        Some(r) => Ok(r?),
        None => Err(BillingError::Invalid(
            "reservation missing for idempotency hit",
        )),
    }
}

fn parse_prefixed_ulid(s: &str, expected_prefix: &'static str) -> duckdb::Result<Ulid> {
    let prefix_with_under = format!("{}_", expected_prefix);
    let bare = s
        .strip_prefix(&prefix_with_under)
        .ok_or_else(|| decode_err("id missing expected prefix"))?;
    Ulid::from_string(bare).map_err(|_| decode_err("id is not a valid Crockford-base32 ULID"))
}

fn decode_err(msg: &'static str) -> duckdb::Error {
    duckdb::Error::FromSqlConversionFailure(
        0,
        duckdb::types::Type::Text,
        Box::<dyn std::error::Error + Send + Sync>::from(msg),
    )
}

#[cfg(test)]
mod no_sql_specific_tests {
    //! S410 / [[no-sql-specific]] — the `reset_policy` and `status`
    //! closed-vocab CHECK constraints were dropped from the DDL; these
    //! pins prove the invariant still lives in code (the read-side
    //! parsers reject out-of-vocab values). If someone widens or breaks
    //! the parser, these fail — the invariant cannot silently regress.
    use super::*;

    #[test]
    fn reset_policy_from_str_accepts_vocab_rejects_others() {
        assert_eq!(reset_policy_from_str("never"), Some(ResetPolicy::Never));
        assert_eq!(
            reset_policy_from_str("annual_on_fiscal_year"),
            Some(ResetPolicy::AnnualOnFiscalYear)
        );
        // The dropped CHECK's job, now in code:
        assert_eq!(reset_policy_from_str("ANNUAL"), None);
        assert_eq!(reset_policy_from_str("monthly"), None);
        assert_eq!(reset_policy_from_str(""), None);
    }

    #[test]
    fn reservation_status_from_str_accepts_vocab_rejects_others() {
        assert_eq!(
            reservation_status_from_str("reserved"),
            Some(ReservationStatus::Reserved)
        );
        assert_eq!(
            reservation_status_from_str("used"),
            Some(ReservationStatus::Used)
        );
        assert_eq!(
            reservation_status_from_str("voided"),
            Some(ReservationStatus::Voided)
        );
        // The dropped CHECK's job, now in code:
        assert_eq!(reservation_status_from_str("Reserved"), None);
        assert_eq!(reservation_status_from_str("cancelled"), None);
        assert_eq!(reservation_status_from_str(""), None);
    }
}
