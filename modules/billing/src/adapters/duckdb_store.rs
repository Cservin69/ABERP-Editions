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

const CREATE_TABLES_SQL: &str = "
CREATE TABLE IF NOT EXISTS invoice_series (
    id           VARCHAR NOT NULL PRIMARY KEY,
    code         VARCHAR NOT NULL UNIQUE,
    reset_policy VARCHAR NOT NULL CHECK (reset_policy IN ('never','annual_on_fiscal_year')),
    fiscal_year  INTEGER,
    created_at   VARCHAR NOT NULL
);

CREATE TABLE IF NOT EXISTS invoice_sequence_state (
    series_id   VARCHAR NOT NULL,
    fiscal_year INTEGER NOT NULL,
    next_number BIGINT  NOT NULL CHECK (next_number >= 1),
    updated_at  VARCHAR NOT NULL,
    PRIMARY KEY (series_id, fiscal_year)
);

CREATE TABLE IF NOT EXISTS invoice_sequence_reservation (
    id          VARCHAR NOT NULL PRIMARY KEY,
    series_id   VARCHAR NOT NULL,
    fiscal_year INTEGER NOT NULL,
    number      BIGINT  NOT NULL,
    invoice_id  VARCHAR NOT NULL UNIQUE,
    status      VARCHAR NOT NULL CHECK (status IN ('reserved','used','voided')),
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
    UNIQUE (series_id, fiscal_year, sequence_number)
);

CREATE TABLE IF NOT EXISTS invoice_line (
    invoice_id            VARCHAR NOT NULL,
    ordinal               INTEGER NOT NULL,
    description           VARCHAR NOT NULL,
    quantity              INTEGER NOT NULL CHECK (quantity >= 0),
    unit_price            BIGINT  NOT NULL,
    vat_rate_basis_points INTEGER NOT NULL,
    PRIMARY KEY (invoice_id, ordinal)
);
";

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

#[derive(Debug)]
pub struct DuckDbBillingStore {
    conn: Connection,
}

impl DuckDbBillingStore {
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self, BillingError> {
        Ok(Self {
            conn: Connection::open(path)?,
        })
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
    let idem_str = args.idempotency_key.0.to_string();
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

    // ── Resolve series + fiscal_year. PR-4 supports `Never` only;
    //    the handler rejected Annual before reaching us.
    let series = {
        let mut stmt = tx.prepare(
            "SELECT id, code, reset_policy, fiscal_year, created_at
             FROM invoice_series WHERE id = ?;",
        )?;
        let mut rows = stmt.query_map([args.series_id.to_prefixed_string()], row_to_series)?;
        match rows.next() {
            Some(r) => r?,
            None => {
                return Err(BillingError::SeriesNotFound(
                    args.series_id.to_prefixed_string(),
                ))
            }
        }
    };
    let fiscal_year: i32 = match series.reset_policy {
        ResetPolicy::Never => 0,
        ResetPolicy::AnnualOnFiscalYear => {
            return Err(BillingError::AnnualResetUnimplemented);
        }
    };

    // ── ADR-0009 §3 step 1+3: read next_number (creating the row at
    //    1 if absent), then UPDATE to advance.
    let series_id_str = series.id.to_prefixed_string();
    let now_str = now.format(&Rfc3339)?;
    let allocated: u64 = {
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
                // the state row at next_number = 1 (and we are about
                // to burn 1 and advance to 2 below).
                tx.execute(
                    "INSERT INTO invoice_sequence_state
                     (series_id, fiscal_year, next_number, updated_at)
                     VALUES (?, ?, 1, ?);",
                    params![&series_id_str, fiscal_year, &now_str],
                )?;
                1
            }
        }
    };

    tx.execute(
        "UPDATE invoice_sequence_state
         SET next_number = next_number + 1, updated_at = ?
         WHERE series_id = ? AND fiscal_year = ?;",
        params![&now_str, &series_id_str, fiscal_year],
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
    let rate_value_str: Option<String> = args
        .rate_metadata
        .as_ref()
        .map(|r| r.rate.to_string());
    let rate_source: Option<String> = args
        .rate_metadata
        .as_ref()
        .map(|r| r.source.clone());
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
    tx.execute(
        "INSERT INTO invoice
         (id, series_id, customer_id, issue_date, sequence_number,
          fiscal_year, idempotency_key,
          currency, exchange_rate, exchange_rate_source,
          exchange_rate_date, huf_equivalent_total)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?);",
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
        ],
    )?;

    for (ordinal, line) in draft.lines.iter().enumerate() {
        tx.execute(
            "INSERT INTO invoice_line
             (invoice_id, ordinal, description, quantity,
              unit_price, vat_rate_basis_points)
             VALUES (?, ?, ?, ?, ?, ?);",
            params![
                draft.id.to_prefixed_string(),
                ordinal as i64,
                &line.description,
                line.quantity as i64,
                line.unit_price.as_i64(),
                line.vat_rate_basis_points as i64,
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
        sequence_number: allocated,
        fiscal_year,
    };
    let reservation = SequenceReservation {
        id: reservation_id,
        series_id: series.id,
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

    let status = match status_str.as_str() {
        "reserved" => ReservationStatus::Reserved,
        "used" => ReservationStatus::Used,
        "voided" => ReservationStatus::Voided,
        _ => return Err(decode_err("unknown reservation.status")),
    };
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
    let (series_id_str, customer_id_str, issue_date_str, seq_number, fiscal_year) = {
        let mut stmt = tx.prepare(
            "SELECT series_id, customer_id, issue_date, sequence_number, fiscal_year
             FROM invoice WHERE id = ?;",
        )?;
        let mut rows = stmt.query_map([invoice_id_str], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i32>(4)?,
            ))
        })?;
        match rows.next() {
            Some(r) => r?,
            None => return Err(BillingError::Invalid("invoice missing for idempotency hit")),
        }
    };

    // Load lines in ordinal order.
    let mut stmt = tx.prepare(
        "SELECT description, quantity, unit_price, vat_rate_basis_points
         FROM invoice_line WHERE invoice_id = ? ORDER BY ordinal ASC;",
    )?;
    let rows = stmt.query_map([invoice_id_str], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, i64>(3)?,
        ))
    })?;
    let mut lines = Vec::new();
    for r in rows {
        let (description, quantity, unit_price, vat) = r?;
        lines.push(LineItem {
            description,
            quantity: quantity as u32,
            unit_price: Huf(unit_price),
            vat_rate_basis_points: vat as u16,
        });
    }

    let issue_date = OffsetDateTime::parse(&issue_date_str, &Rfc3339)?;
    Ok(ReadyInvoice {
        id: InvoiceId(parse_prefixed_ulid(invoice_id_str, "inv")?),
        series_id: SeriesId(parse_prefixed_ulid(&series_id_str, "srs")?),
        customer_id: CustomerId(parse_prefixed_ulid(&customer_id_str, "cus")?),
        lines,
        issue_date,
        sequence_number: seq_number as u64,
        fiscal_year,
    })
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
