//! Financial-statistics aggregation backend for the SPA Statistics page.
//!
//! S225 / PR-221 — the FIRST read-only multi-table aggregator. Produces a
//! single JSON snapshot the SPA renders as a dashboard of revenue,
//! expenses, VAT, receivables, payables, DSO, aging, hygiene, and
//! period-over-period deltas. The data sources are the three invoice
//! tables (`invoice` + `invoice_line` for outgoing native, `ap_invoice`
//! for incoming, `restored_invoice` for NAV-as-DR rows) plus the audit
//! ledger (state derivation + payment records + storno chain links).
//!
//! # Architecture choices
//!
//!   - **No new audit kinds.** Reading the dashboard is a pure view; no
//!     state transitions, no audit-ledger writes. The financial figures
//!     are derivable from existing state and must remain so per
//!     CLAUDE.md rule 12 (fail loud — silent ledger writes from a read
//!     endpoint would corrupt the operator-twin model).
//!   - **One audit-ledger walk** per request, producing a `TraceMap`
//!     keyed by outgoing invoice id with the minimal fields the
//!     aggregator needs (state classification, storno-self flag,
//!     payment record, ack status). Reuses the same payload-typed-decode
//!     posture `serve::list_invoices` takes.
//!   - **SQL aggregation** for line-level VAT-rate breakdown. Per-line
//!     rounding is intentionally NOT applied at SUM time — the dashboard
//!     is a management view, not a regulatory document. NAV-byte-perfect
//!     totals live in the bevallás flow per ADR-0009 §1; the v2.2.0
//!     dashboard surfaces approximate aggregates within rounding tolerance
//!     of the per-line figures.
//!   - **Closed-vocab date basis** (`teljesites` | `issued`) per
//!     `[[aberp-invoice-dates]]`. The default `teljesites` (delivery date)
//!     is the regulatory anchor for monthly bevallás per HU VAT law;
//!     `issued` (issue date) is offered as a secondary cash-flow lens.
//!   - **Per-currency parallel totals** for HUF + EUR. No FX aggregation
//!     in v1 — flagged as v2.2.1 deferred in the meta block on the wire.
//!   - **Storno sign-flip in code.** Storno child rows have POSITIVE
//!     line amounts in `invoice_line` (negation lives in the NAV XML
//!     emit path per ADR-0049). Aggregation flips the sign at the
//!     trace lookup so the dashboard's revenue figure subtracts storno
//!     reversals, matching the SPA list view's display rule (S156).
//!
//! # Deferred for v2.2.1
//!
//!   - FX aggregation (`all-in-HUF-at-MNB-rate` tertiary column).
//!   - HIPA (Helyi Iparűzési Adó) base — needs operator categorization
//!     of which line items are "material/subcontractor"; ADR-pending.
//!   - KIVA / KATA threshold logic — the running YTD revenue is shown,
//!     but no threshold limits are encoded (regime-dependent).
//!   - AAM / reverse-charge / EU-0 VAT-rate sub-buckets — the schema
//!     does NOT distinguish them today (all are `0%` in `invoice_line`).
//!     Parsing the NAV XML to recover the tag is its own follow-on PR.
//!   - Per-VAT-rate breakdown for restored/incoming invoices —
//!     restored_invoice and ap_invoice are digest-only (no line items).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use aberp_audit_ledger::{BinaryHash, Entry, EventKind, Ledger, TenantId};
use anyhow::{anyhow, Context, Result};
use duckdb::{params, Connection};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use time::macros::format_description;
use time::{Date, Month, OffsetDateTime};

use crate::audit_payloads;

// ──────────────────────────────────────────────────────────────────────
// Public request / response shapes.
// ──────────────────────────────────────────────────────────────────────

/// Inputs to [`compute_financial_report`] after parsing the HTTP query
/// string. The route layer parses URL parameters into this typed shape
/// per CLAUDE.md rule 5.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportRequest {
    pub period: PeriodKind,
    pub date_basis: DateBasis,
    /// "Today" anchor for aging / cashflow / running-YTD calculations.
    /// Always callable with `today_local()`; tests pass a fixed date.
    pub today: Date,
    /// S262 / PR-251 — number of rows the top-customers / top-vendors
    /// lists return. Operator-configurable from the SPA (`?top_n=`),
    /// defaulting to 10. Clamped at the route layer to a sane range so a
    /// hand-typed `?top_n=100000` cannot force an unbounded sort-and-emit.
    pub top_n: usize,
}

/// Closed-vocab period selector. Default `Month(YYYY, MM)` per the
/// monthly bevallás cadence. The `Custom { from, to }` arm carries
/// inclusive ISO dates; `All` skips the date filter entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeriodKind {
    Month(i32, u8),
    Quarter(i32, u8),
    Year(i32),
    Custom { from: Date, to: Date },
    All,
}

/// Date axis for period filtering. `Teljesites` (delivery date with
/// fallback to issue date) is the regulatory anchor for VAT-month
/// assignment per `[[aberp-invoice-dates]]`. `Issued` (issue date) is
/// the cash-flow lens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateBasis {
    Teljesites,
    Issued,
}

impl DateBasis {
    pub fn as_wire_str(self) -> &'static str {
        match self {
            DateBasis::Teljesites => "teljesites",
            DateBasis::Issued => "issued",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "teljesites" => Some(DateBasis::Teljesites),
            "issued" => Some(DateBasis::Issued),
            _ => None,
        }
    }
}

/// Single JSON snapshot returned by `GET /api/reports/financial`. Every
/// field is deterministic from the inputs (period + date_basis + db
/// state + audit ledger) so two requests against the same state produce
/// identical bytes (modulo `today` floating each invocation, which the
/// SPA disclosures via the `period.today` echo).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct FinancialReport {
    pub period: PeriodMeta,
    pub revenue: CurrencyAggregate,
    pub expenses: CurrencyAggregate,
    pub gross_profit: CurrencyPair,
    pub vat_collected: CurrencyAggregate,
    pub vat_paid: CurrencyAggregate,
    pub vat_to_pay: CurrencyPair,
    pub receivables: CurrencyAggregate,
    pub payables: CurrencyAggregate,
    /// S262 / PR-251 — currency split of NATIVE outgoing revenue,
    /// expressed in a common HUF unit so HUF and EUR are comparable on
    /// one stacked bar.
    pub currency_split: CurrencySplitPanel,
    pub receivables_aging: AgingPanel,
    pub payables_aging: AgingPanel,
    pub dso_days: DsoPanel,
    pub cashflow_forward: CashflowPanel,
    pub vat_breakdown_outgoing: Vec<VatRateBreakdownEntry>,
    pub top_customers: Vec<TopEntry>,
    pub top_vendors: Vec<TopEntry>,
    pub hygiene: HygienePanel,
    pub deltas: PeriodDeltas,
    pub annual_running: AnnualRunningPanel,
    pub deferred_notes: Vec<String>,
    /// Integrity signal for THIS run of the aggregator — see
    /// [`LedgerDiagnostics`]. Non-zero means the figures above are
    /// possibly incomplete.
    #[serde(default)]
    pub ledger_diagnostics: LedgerDiagnostics,
}

/// Per-run integrity diagnostics for the financial report.
///
/// Two independent signals, both of the same family — a silent drop made
/// loud: entries the audit-ledger walk ([`walk_entries`]) could not decode,
/// and outstanding invoices whose payment deadline the aging pass could
/// not read ([`aging_placement`]).
///
/// The first, in detail:
///
/// The walk decodes every audit entry's JSON payload to derive per-invoice
/// state (ack status, payment, storno chain links). Before this fix, a
/// payload that failed to decode was dropped on the floor: the `if let
/// Ok(..)` / `.ok()?` arms in [`ReportTrace::merge`],
/// [`extract_chain_link_local`] and [`extract_invoice_id_local`] each fell
/// through to "nothing happened". A malformed `InvoicePaymentRecorded`
/// payload therefore made an invoice look UNPAID (inflating receivables,
/// dropping its DSO sample, and possibly counting it past-deadline); a
/// malformed `InvoiceAckStatus` payload could demote a SAVED invoice to
/// `PendingDraft` and take it out of revenue + VAT-collected entirely; a
/// malformed storno chain link left the base uncancelled. All of it
/// silently — the operator saw a clean number that was simply wrong.
///
/// The conservative posture is NOT to abort the whole dashboard on one bad
/// row: a single corrupt entry must not blank the operator's entire
/// financial screen, and every valid row's arithmetic is unchanged.
/// Instead the drop is made VISIBLE and ATTRIBUTABLE — a `tracing::error!`
/// per entry carrying its id, seq, kind and the decode error, plus this
/// machine-countable signal on the wire so a caller can flag the figures
/// as possibly-incomplete rather than present them as authoritative.
///
/// An entry is counted AT MOST ONCE even when several decode attempts fail
/// on it (a payload that is not JSON at all fails both the id extraction
/// and the chain-link decode): the count answers "how many entries could
/// not be read", not "how many decode attempts failed".
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct LedgerDiagnostics {
    /// Number of audit entries whose payload the report walk could not
    /// decode, and which are therefore NOT reflected in any figure above.
    /// Exact and uncapped.
    pub unparseable_entries: u64,
    /// Audit-entry ids (`aud_<ULID>`) of those entries, so the operator
    /// can go find them. Capped at [`MAX_UNPARSEABLE_ENTRY_IDS`] — the
    /// count above stays exact, so `unparseable_entries >
    /// unparseable_entry_ids.len()` simply means "and more".
    pub unparseable_entry_ids: Vec<String>,
    /// Outstanding invoices — receivable OR payable — whose
    /// `payment_deadline` was MISSING or UNPARSEABLE, and which are
    /// therefore aged conservatively into `days_90_plus` instead of being
    /// dropped. Exact and uncapped. See [`aging_placement`].
    ///
    /// Unlike [`Self::unparseable_entries`] this does NOT mean a figure is
    /// missing: every one of these invoices IS in the receivables /
    /// payables total AND in exactly one aging bucket. What it means is
    /// that the bucket is an imputation, not a measurement — the invoice's
    /// true age is unknown.
    #[serde(default)]
    pub aging_undated_invoices: u64,
    /// Ids of those invoices, sorted, so the operator can go fix the
    /// deadlines. Capped at [`MAX_UNPARSEABLE_ENTRY_IDS`] on the same
    /// "count exact, ids are a starting point" contract as
    /// [`Self::unparseable_entry_ids`].
    ///
    /// MACHINE-READABLE ONLY — the SPA deliberately does not render this
    /// list. NAV-synced payables carry no deadline at all, so on a real
    /// book it would be a permanent wall of ids on the dashboard. It stays
    /// on the wire for support and debugging; the operator-facing surface
    /// is the per-side count below.
    #[serde(default)]
    pub aging_undated_invoice_ids: Vec<String>,
    /// The [`Self::aging_undated_invoices`] total split by side, so each
    /// aging panel can footnote its OWN imputed rows. Rendering the
    /// combined figure under both panels would double-report it.
    ///
    /// `aging_undated_receivables + aging_undated_payables ==
    /// aging_undated_invoices`.
    #[serde(default)]
    pub aging_undated_receivables: u64,
    #[serde(default)]
    pub aging_undated_payables: u64,
}

/// Cap on [`LedgerDiagnostics::unparseable_entry_ids`]. A systemically
/// corrupt ledger must not balloon the report's JSON payload; the count is
/// the machine-countable signal, the ids are the operator's starting
/// point.
const MAX_UNPARSEABLE_ENTRY_IDS: usize = 50;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct PeriodMeta {
    pub kind: String,
    pub label: String,
    pub from: Option<String>,
    pub to: Option<String>,
    pub date_basis: String,
    pub today: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct CurrencyAggregate {
    pub huf: AmountAggregate,
    pub eur: AmountAggregate,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct AmountAggregate {
    pub gross_minor: i64,
    pub net_minor: i64,
    pub vat_minor: i64,
    pub count: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct CurrencyPair {
    pub huf_minor: i64,
    pub eur_minor: i64,
}

/// S262 / PR-251 — currency split of native outgoing revenue.
///
/// HUF and EUR revenue live in different units (forints vs EUR cents), so
/// a raw side-by-side bar would be meaningless (HUF dwarfs EUR ~400×). To
/// make the split comparable, the EUR portion is converted to HUF at the
/// **snapshot MNB rate stamped on each invoice at issuance** (the
/// `huf_equivalent_total` column, ADR-0037 §1.c) — NOT today's rate. The
/// SPA renders `huf_minor` + `eur_as_huf_minor` as one stacked bar and
/// discloses the native EUR figure separately.
///
/// Basis: ISSUED native outgoing invoices (the `invoice` table only —
/// restored/AP digest rows have no snapshot rate). `huf_minor` reuses the
/// storno-adjusted native-revenue aggregate; `eur_as_huf_minor` sums
/// `huf_equivalent_total` on an issued basis (EUR storno reversals are not
/// sign-flipped here — a rare-case v1 approximation noted on the tile).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct CurrencySplitPanel {
    /// Native HUF revenue gross, in forints (storno-adjusted).
    pub huf_minor: i64,
    pub huf_count: u64,
    /// Native EUR revenue gross, in EUR cents (storno-adjusted) — shown
    /// for disclosure beside the converted figure.
    pub eur_native_minor: i64,
    pub eur_count: u64,
    /// EUR revenue converted to HUF at each invoice's snapshot rate, in
    /// forints (issued basis). The comparable EUR contribution to the bar.
    pub eur_as_huf_minor: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct AgingPanel {
    pub current: AmountAggregate,
    pub days_1_30: AmountAggregate,
    pub days_31_60: AmountAggregate,
    pub days_61_90: AmountAggregate,
    pub days_90_plus: AmountAggregate,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct DsoPanel {
    pub huf_days: Option<f64>,
    pub eur_days: Option<f64>,
    pub huf_sample_size: u64,
    pub eur_sample_size: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct CashflowPanel {
    pub next_30: CurrencyPair,
    pub next_60: CurrencyPair,
    pub next_90: CurrencyPair,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct VatRateBreakdownEntry {
    pub rate_basis_points: i32,
    pub currency: String,
    pub net_minor: i64,
    pub vat_minor: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct TopEntry {
    pub label: String,
    pub currency: String,
    pub gross_minor: i64,
    pub count: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct HygienePanel {
    /// Outgoing native invoices in terminal-bad NAV states (Rejected /
    /// Abandoned) — need attention from the operator.
    pub outgoing_rejected_count: u64,
    pub outgoing_abandoned_count: u64,
    /// Outgoing native invoices in pre-submission states (Ready, Pending,
    /// PendingNavExists) — drafts the operator may have forgotten.
    pub outgoing_pending_count: u64,
    /// Restored (ExtNav) rows without a partner_id (manual link missing).
    pub restored_no_partner_count: u64,
    /// Counted outgoing invoices whose payment_deadline has passed and
    /// no payment is recorded.
    ///
    /// EXCLUDES invoices with a missing or unreadable `payment_deadline`.
    /// Those are aged into `days_90_plus` on the receivables aging panel
    /// (an age ESTIMATE — see [`aging_placement`]), but this counter is a
    /// LATENESS ASSERTION and there is no defensible way to impute one:
    /// an unreadable deadline is unknown lateness, not lateness. They are
    /// disclosed instead via
    /// [`LedgerDiagnostics::aging_undated_invoices`].
    pub outstanding_past_deadline_count: u64,
    /// Outstanding ap_invoice rows whose payment_deadline has passed.
    ///
    /// Excludes missing / unreadable deadlines for the same reason as
    /// [`Self::outstanding_past_deadline_count`], and the exclusion is
    /// load-bearing here: `ap_sync` records `payment_deadline: None` on
    /// every NAV-synced payable, so a counter that took the imputed aging
    /// bucket would report essentially the entire payables book as past
    /// deadline.
    pub payable_past_deadline_count: u64,
    /// Number of `InvoiceStornoIssued` chain entries in the period.
    pub storno_chain_count: u64,
    /// Number of `InvoiceModificationIssued` chain entries in the period.
    pub modification_chain_count: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct PeriodDeltas {
    pub mom: Option<DeltaSet>,
    pub yoy: Option<DeltaSet>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct DeltaSet {
    pub period_label: String,
    pub revenue: CurrencyAggregate,
    pub expenses: CurrencyAggregate,
    pub revenue_pct_huf: Option<f64>,
    pub revenue_pct_eur: Option<f64>,
    pub expenses_pct_huf: Option<f64>,
    pub expenses_pct_eur: Option<f64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct AnnualRunningPanel {
    pub year: i32,
    pub revenue: CurrencyAggregate,
}

// ──────────────────────────────────────────────────────────────────────
// Period parsing.
// ──────────────────────────────────────────────────────────────────────

/// Resolved date window for SQL filtering. `None` on either side
/// represents an open bound (only used for `PeriodKind::All`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DateWindow {
    pub from: Option<Date>,
    pub to: Option<Date>,
}

impl DateWindow {
    fn unbounded() -> Self {
        Self {
            from: None,
            to: None,
        }
    }
}

/// Parse the `period` query-string parameter into a [`PeriodKind`].
///
/// Accepted forms:
///   - `2026-06` → `Month(2026, 6)`
///   - `2026-Q2` → `Quarter(2026, 2)`
///   - `2026` → `Year(2026)`
///   - `all` → `All`
///   - `2026-06-01..2026-06-30` → `Custom { from, to }`
///
/// Returns `Err` for malformed strings so the route layer can surface
/// a 400. Per CLAUDE.md rule 12 — silent coercion to a default would
/// hide an operator-typed typo in a URL parameter.
pub fn parse_period(s: &str) -> Result<PeriodKind> {
    let trimmed = s.trim();
    if trimmed.eq_ignore_ascii_case("all") {
        return Ok(PeriodKind::All);
    }
    if let Some((from_s, to_s)) = trimmed.split_once("..") {
        let from = parse_iso_date(from_s)?;
        let to = parse_iso_date(to_s)?;
        if to < from {
            return Err(anyhow!(
                "custom period `{}` has to-date before from-date",
                trimmed
            ));
        }
        return Ok(PeriodKind::Custom { from, to });
    }
    // Quarter form: `YYYY-Q[1-4]`.
    if let Some((y_s, q_s)) = trimmed.split_once("-Q") {
        let year: i32 = y_s
            .parse()
            .with_context(|| format!("quarter period `{}` has malformed year", trimmed))?;
        let q: u8 = q_s
            .parse()
            .with_context(|| format!("quarter period `{}` has malformed quarter", trimmed))?;
        if !(1..=4).contains(&q) {
            return Err(anyhow!(
                "quarter period `{}` has out-of-range quarter (1-4)",
                trimmed
            ));
        }
        return Ok(PeriodKind::Quarter(year, q));
    }
    // Month form: `YYYY-MM`.
    if let Some((y_s, m_s)) = trimmed.split_once('-') {
        let year: i32 = y_s
            .parse()
            .with_context(|| format!("month period `{}` has malformed year", trimmed))?;
        let m: u8 = m_s
            .parse()
            .with_context(|| format!("month period `{}` has malformed month", trimmed))?;
        if !(1..=12).contains(&m) {
            return Err(anyhow!(
                "month period `{}` has out-of-range month (1-12)",
                trimmed
            ));
        }
        return Ok(PeriodKind::Month(year, m));
    }
    // Bare year form: `YYYY`.
    if trimmed.len() == 4 && trimmed.chars().all(|c| c.is_ascii_digit()) {
        let year: i32 = trimmed.parse().with_context(|| "year parse")?;
        return Ok(PeriodKind::Year(year));
    }
    Err(anyhow!("unparseable period `{}`", trimmed))
}

fn parse_iso_date(s: &str) -> Result<Date> {
    let fmt = format_description!("[year]-[month]-[day]");
    Date::parse(s.trim(), fmt).with_context(|| format!("parse ISO date `{}`", s))
}

/// Resolve a [`PeriodKind`] to inclusive ISO-date bounds for SQL
/// filtering.
pub fn resolve_window(kind: PeriodKind) -> Result<DateWindow> {
    match kind {
        PeriodKind::All => Ok(DateWindow::unbounded()),
        PeriodKind::Month(y, m) => {
            let month = Month::try_from(m)
                .map_err(|_| anyhow!("month {} out of range when resolving period", m))?;
            let from = Date::from_calendar_date(y, month, 1)
                .with_context(|| format!("calendar date {}-{:02}-01", y, m))?;
            let next_first = next_month_first(y, m)?;
            let to = next_first.previous_day().expect("date arithmetic");
            Ok(DateWindow {
                from: Some(from),
                to: Some(to),
            })
        }
        PeriodKind::Quarter(y, q) => {
            let start_month = match q {
                1 => 1,
                2 => 4,
                3 => 7,
                4 => 10,
                _ => return Err(anyhow!("quarter {} out of range", q)),
            };
            let end_month_first_next = next_month_first(y, start_month + 2)?;
            let from = Date::from_calendar_date(y, Month::try_from(start_month).unwrap(), 1)
                .with_context(|| format!("calendar date {}-Q{}", y, q))?;
            let to = end_month_first_next
                .previous_day()
                .expect("date arithmetic");
            Ok(DateWindow {
                from: Some(from),
                to: Some(to),
            })
        }
        PeriodKind::Year(y) => {
            let from = Date::from_calendar_date(y, Month::January, 1)?;
            let to = Date::from_calendar_date(y, Month::December, 31)?;
            Ok(DateWindow {
                from: Some(from),
                to: Some(to),
            })
        }
        PeriodKind::Custom { from, to } => Ok(DateWindow {
            from: Some(from),
            to: Some(to),
        }),
    }
}

fn next_month_first(y: i32, m: u8) -> Result<Date> {
    let (ny, nm) = if m >= 12 { (y + 1, 1u8) } else { (y, m + 1) };
    let month = Month::try_from(nm).map_err(|_| anyhow!("month arithmetic failed"))?;
    Date::from_calendar_date(ny, month, 1).map_err(|e| anyhow!("date construction: {}", e))
}

fn period_label(kind: PeriodKind) -> String {
    match kind {
        PeriodKind::Month(y, m) => format!("{:04}-{:02}", y, m),
        PeriodKind::Quarter(y, q) => format!("{:04}-Q{}", y, q),
        PeriodKind::Year(y) => format!("{:04}", y),
        PeriodKind::All => "all".to_string(),
        PeriodKind::Custom { from, to } => format!("{}..{}", from, to),
    }
}

fn period_kind_label(kind: PeriodKind) -> &'static str {
    match kind {
        PeriodKind::Month(..) => "month",
        PeriodKind::Quarter(..) => "quarter",
        PeriodKind::Year(..) => "year",
        PeriodKind::All => "all",
        PeriodKind::Custom { .. } => "custom",
    }
}

/// Shift a period back to its comparable "previous month" / "previous
/// quarter" / "previous year" sibling for MoM delta computation. `None`
/// for `Custom` (would require shifting an arbitrary window — out of
/// scope for v2.2.0) and for `All`.
fn previous_period(kind: PeriodKind) -> Option<PeriodKind> {
    match kind {
        PeriodKind::Month(y, m) => {
            let (py, pm) = if m <= 1 { (y - 1, 12u8) } else { (y, m - 1) };
            Some(PeriodKind::Month(py, pm))
        }
        PeriodKind::Quarter(y, q) => {
            let (py, pq) = if q <= 1 { (y - 1, 4u8) } else { (y, q - 1) };
            Some(PeriodKind::Quarter(py, pq))
        }
        PeriodKind::Year(y) => Some(PeriodKind::Year(y - 1)),
        PeriodKind::All | PeriodKind::Custom { .. } => None,
    }
}

/// Shift a period back one year for YoY delta computation. `None` for
/// `All` (no sensible comparable) and for `Custom` (operator-defined).
fn yoy_period(kind: PeriodKind) -> Option<PeriodKind> {
    match kind {
        PeriodKind::Month(y, m) => Some(PeriodKind::Month(y - 1, m)),
        PeriodKind::Quarter(y, q) => Some(PeriodKind::Quarter(y - 1, q)),
        PeriodKind::Year(y) => Some(PeriodKind::Year(y - 1)),
        PeriodKind::All | PeriodKind::Custom { .. } => None,
    }
}

// ──────────────────────────────────────────────────────────────────────
// Audit-ledger trace walk.
// ──────────────────────────────────────────────────────────────────────

/// Minimal per-invoice trace produced by the single audit-ledger walk.
/// Mirrors the fields `serve::list_invoices` reads but trims to what
/// the aggregator needs (no chain-children, no NAV check-outcome).
#[derive(Debug, Default, Clone)]
struct ReportTrace {
    has_draft: bool,
    has_attempt: bool,
    has_submission_response: bool,
    has_marked_abandoned: bool,
    last_ack_status: Option<String>,
    /// This invoice is the BASE of a storno chain whose storno child has
    /// actually LANDED (its own trace classifies as [`CountedKind::Counted`],
    /// i.e. NAV accepted it). Resolved in a post-walk pass, never at
    /// storno-ISSUANCE time: a storno that was aborted / never submitted
    /// classifies `Rejected`, never negates the base in revenue, and so
    /// the base still stands as a real unpaid receivable.
    has_landed_storno: bool,
    is_amended_base: bool,
    is_storno_self: bool,
    payment_paid_at: Option<String>,
    payment_amount_minor: Option<i64>,
}

/// Classification of an outgoing invoice for aggregation purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CountedKind {
    /// Counts toward revenue/VAT-collected. May be storno-self → negated.
    Counted { is_storno_self: bool },
    /// Pre-submission state (Ready / Pending / PendingNavExists). Hygiene
    /// count only; not in revenue.
    PendingDraft,
    /// Terminal-bad state (Rejected / Aborted ack).
    Rejected,
    /// Operator-declared abandoned.
    Abandoned,
    /// No audit entries (orphan billing row).
    Unknown,
}

impl ReportTrace {
    fn classify(&self) -> CountedKind {
        if self.has_marked_abandoned {
            return CountedKind::Abandoned;
        }
        match self.last_ack_status.as_deref() {
            Some("SAVED") => {
                return CountedKind::Counted {
                    is_storno_self: self.is_storno_self,
                }
            }
            Some("ABORTED") => return CountedKind::Rejected,
            _ => {}
        }
        if self.has_submission_response {
            return CountedKind::Counted {
                is_storno_self: self.is_storno_self,
            };
        }
        // Storno-base / amended-base WITHOUT a SAVED ack — base rows
        // sit in earlier ledger entries; storno chain links don't
        // resurrect them. Fall through.
        if self.has_attempt || self.has_draft {
            return CountedKind::PendingDraft;
        }
        CountedKind::Unknown
    }
}

/// Result of the one-pass audit-ledger walk.
#[derive(Debug, Default, Clone)]
struct LedgerWalk {
    traces: HashMap<String, ReportTrace>,
    /// `InvoiceStornoIssued` chain entries whose audit `at` timestamp
    /// falls inside `(from, to)`. Counted for hygiene.
    storno_links_in_period: u64,
    /// `InvoiceModificationIssued` chain entries in the period.
    modification_links_in_period: u64,
    /// `(base_invoice_id, storno_child_invoice_id)` pairs seen during the
    /// walk. The base's `has_landed_storno` flag is resolved from these
    /// AFTER the walk completes — the child's own trace is only complete
    /// once every entry has been merged.
    storno_links: Vec<(String, String)>,
    /// Entries this walk could NOT decode — see [`LedgerDiagnostics`].
    diagnostics: LedgerDiagnostics,
}

fn walk_ledger(ledger: &Ledger, period_window: DateWindow) -> Result<LedgerWalk> {
    let entries = ledger
        .entries()
        .context("read audit ledger entries for financial report")?;
    Ok(walk_entries(&entries, period_window))
}

/// The pure core of [`walk_ledger`], split out so the storno-landing
/// resolution is testable without a file-backed ledger.
fn walk_entries(entries: &[Entry], period_window: DateWindow) -> LedgerWalk {
    let mut walk = LedgerWalk::default();
    for entry in entries {
        // A decode failure on THIS entry, if any. Recorded once at the end
        // of the iteration rather than at each failing call site: a payload
        // that is not JSON at all fails both decodes below, and the
        // operator-facing count is entries-not-read, not attempts-failed.
        let mut decode_error: Option<String> = None;

        match extract_invoice_id_local(entry) {
            Ok(Some(id)) => {
                // The trace row is created either way (`or_default()` runs
                // before `merge`), exactly as before — a failed typed decode
                // leaves it at its defaults instead of vanishing the id.
                if let Err(e) = walk.traces.entry(id.clone()).or_default().merge(entry, &id) {
                    decode_error = Some(e.to_string());
                }
            }
            // Valid JSON with no `invoice_id` key — the ordinary case for
            // every event kind that is not invoice-scoped. NOT a defect.
            Ok(None) => {}
            Err(e) => decode_error = Some(e.to_string()),
        }

        match extract_chain_link_local(entry) {
            Ok(Some(link)) => {
                if link.is_storno {
                    walk.traces
                        .entry(link.child_invoice_id.clone())
                        .or_default()
                        .is_storno_self = true;
                    // Record the link only — do NOT flag the base here. The
                    // flag means "a storno LANDED", which is not knowable at
                    // issuance time; it is resolved below.
                    walk.traces.entry(link.base_invoice_id.clone()).or_default();
                    walk.storno_links
                        .push((link.base_invoice_id.clone(), link.child_invoice_id.clone()));
                    if entry_in_window(entry, period_window) {
                        walk.storno_links_in_period = walk.storno_links_in_period.saturating_add(1);
                    }
                } else {
                    walk.traces
                        .entry(link.base_invoice_id.clone())
                        .or_default()
                        .is_amended_base = true;
                    if entry_in_window(entry, period_window) {
                        walk.modification_links_in_period =
                            walk.modification_links_in_period.saturating_add(1);
                    }
                }
            }
            // Not a chain-link kind. The overwhelming majority of entries.
            Ok(None) => {}
            Err(e) => {
                decode_error.get_or_insert_with(|| e.to_string());
            }
        }

        if let Some(err) = decode_error {
            record_unparseable_entry(&mut walk.diagnostics, entry, &err);
        }
    }
    resolve_landed_stornos(&mut walk);
    walk
}

/// Make one undecodable audit entry LOUD and ATTRIBUTABLE: a structured
/// error log naming the entry, and a bump on the machine-countable signal
/// that rides out on [`FinancialReport::ledger_diagnostics`].
///
/// Deliberately not a hard error. Aborting the walk would let a single
/// corrupt row blank the operator's whole financial screen, which trades a
/// wrong number for no number at all. See [`LedgerDiagnostics`].
fn record_unparseable_entry(diag: &mut LedgerDiagnostics, entry: &Entry, error: &str) {
    let id = entry.id.to_prefixed_string();
    tracing::error!(
        entry_id = %id,
        entry_seq = entry.seq.0,
        entry_kind = ?entry.kind,
        decode_error = %error,
        "financial report: audit entry payload could not be decoded — this entry is \
         NOT reflected in the reported figures"
    );
    diag.unparseable_entries = diag.unparseable_entries.saturating_add(1);
    if diag.unparseable_entry_ids.len() < MAX_UNPARSEABLE_ENTRY_IDS {
        diag.unparseable_entry_ids.push(id);
    }
}

/// Post-walk pass: flag a storno BASE as `has_landed_storno` only when its
/// storno child's own trace classifies as [`CountedKind::Counted`] — the
/// exact same `classify()` the revenue aggregation uses to net the pair.
///
/// This coupling is the point. Revenue nets base(+) against child(−) iff the
/// child is `Counted`; receivables drop the base iff the child is `Counted`.
/// Flagging at storno-ISSUANCE time instead would silently UNDER-report AR:
/// a storno that was ABORTED or never submitted classifies `Rejected`, never
/// negates the base in revenue, and leaves the original invoice standing as
/// a genuine unpaid receivable that must remain in AR.
fn resolve_landed_stornos(walk: &mut LedgerWalk) {
    let landed: Vec<String> = walk
        .storno_links
        .iter()
        .filter(|(_, child)| {
            walk.traces
                .get(child)
                .map(|t| matches!(t.classify(), CountedKind::Counted { .. }))
                .unwrap_or(false)
        })
        .map(|(base, _)| base.clone())
        .collect();
    for base in landed {
        walk.traces.entry(base).or_default().has_landed_storno = true;
    }
}

impl ReportTrace {
    /// Fold one audit entry into this invoice's trace.
    ///
    /// `Err` means the entry's typed payload did NOT decode, so nothing was
    /// folded and this entry is missing from every figure derived from the
    /// trace. The caller MUST surface it — see [`LedgerDiagnostics`]. The
    /// four decode arms used to be `if let Ok(..)` with no `else`, which is
    /// precisely the silent drop.
    ///
    /// The `parsed.invoice_id == invoice_id` guards are unchanged: a
    /// successfully-decoded payload naming a DIFFERENT invoice is not a
    /// defect, it just does not belong to this trace.
    fn merge(&mut self, entry: &Entry, invoice_id: &str) -> serde_json::Result<()> {
        match entry.kind {
            EventKind::InvoiceDraftCreated => self.has_draft = true,
            EventKind::InvoiceSubmissionAttempt => {
                let parsed = serde_json::from_slice::<
                    audit_payloads::InvoiceSubmissionAttemptPayload,
                >(&entry.payload)?;
                if parsed.invoice_id == invoice_id {
                    self.has_attempt = true;
                }
            }
            EventKind::InvoiceSubmissionResponse => {
                let parsed = serde_json::from_slice::<
                    audit_payloads::InvoiceSubmissionResponsePayload,
                >(&entry.payload)?;
                if parsed.invoice_id == invoice_id {
                    self.has_submission_response = true;
                }
            }
            EventKind::InvoiceAckStatus => {
                let parsed = serde_json::from_slice::<audit_payloads::InvoiceAckStatusPayload>(
                    &entry.payload,
                )?;
                if parsed.invoice_id == invoice_id {
                    self.last_ack_status = Some(parsed.ack_status);
                }
            }
            EventKind::InvoiceMarkedAbandoned => {
                self.has_marked_abandoned = true;
            }
            EventKind::InvoicePaymentRecorded => {
                let parsed = serde_json::from_slice::<audit_payloads::InvoicePaymentRecordedPayload>(
                    &entry.payload,
                )?;
                if parsed.invoice_id == invoice_id {
                    self.payment_paid_at = Some(parsed.paid_at);
                    self.payment_amount_minor = Some(parsed.amount_minor);
                }
            }
            _ => {}
        }
        Ok(())
    }
}

struct ChainLinkLocal {
    base_invoice_id: String,
    child_invoice_id: String,
    is_storno: bool,
}

/// `Ok(None)` = this entry is not a chain-link kind (the ordinary case).
/// `Err` = it IS one and its payload did not decode, so the chain link is
/// lost — a storno that never nets its base to zero, leaving revenue and
/// receivables overstated. Used to be `.ok()?`, indistinguishable from
/// "not a chain link" and therefore silent.
fn extract_chain_link_local(entry: &Entry) -> serde_json::Result<Option<ChainLinkLocal>> {
    match entry.kind {
        EventKind::InvoiceStornoIssued => {
            let parsed: audit_payloads::InvoiceStornoIssuedPayload =
                serde_json::from_slice(&entry.payload)?;
            Ok(Some(ChainLinkLocal {
                base_invoice_id: parsed.base_invoice_id,
                child_invoice_id: parsed.storno_invoice_id,
                is_storno: true,
            }))
        }
        EventKind::InvoiceModificationIssued => {
            let parsed: audit_payloads::InvoiceModificationIssuedPayload =
                serde_json::from_slice(&entry.payload)?;
            Ok(Some(ChainLinkLocal {
                base_invoice_id: parsed.base_invoice_id,
                child_invoice_id: parsed.modification_invoice_id,
                is_storno: false,
            }))
        }
        _ => Ok(None),
    }
}

/// `Ok(None)` = the payload is well-formed JSON that carries no
/// `invoice_id` (chain links, tenant-level events, …) — normal, and NOT a
/// diagnostic. `Err` = the payload is not JSON at all, so the entry never
/// reaches [`ReportTrace::merge`] and contributes to nothing. Used to be
/// `.ok()?`, which collapsed those two cases into one silent `None`.
fn extract_invoice_id_local(entry: &Entry) -> serde_json::Result<Option<String>> {
    let v: serde_json::Value = serde_json::from_slice(&entry.payload)?;
    Ok(v.as_object()
        .and_then(|m| m.get("invoice_id"))
        .and_then(|x| x.as_str())
        .map(|s| s.to_string()))
}

fn entry_in_window(entry: &Entry, window: DateWindow) -> bool {
    if window.from.is_none() && window.to.is_none() {
        return true;
    }
    let d = entry.time_wall.date();
    if let Some(from) = window.from {
        if d < from {
            return false;
        }
    }
    if let Some(to) = window.to {
        if d > to {
            return false;
        }
    }
    true
}

// ──────────────────────────────────────────────────────────────────────
// SQL aggregation rows.
// ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct OutgoingLineGroup {
    invoice_id: String,
    currency: String,
    /// The invoice's ISSUE date, normalised to `YYYY-MM-DD` — the DSO
    /// anchor. Deliberately NOT the basis/teljesites column
    /// (`COALESCE(delivery_date, issue_date)`): anchoring
    /// days-sales-outstanding on fulfillment makes a prepayment (paid
    /// before fulfillment) come out NEGATIVE. Credit-to-cash runs from
    /// the invoice being issued.
    ///
    /// Projected as `SUBSTR(CAST(i.issue_date AS VARCHAR), 1, 10)`, which
    /// is load-bearing in three ways:
    ///   * `invoice.issue_date` is a **VARCHAR holding RFC3339** in
    ///     production (`duckdb_store.rs` writes
    ///     `draft.issue_date.format(&Rfc3339)` into a `VARCHAR NOT NULL`
    ///     column), e.g. `2026-06-15T12:00:00Z`. [`parse_iso_date`] only
    ///     accepts `[year]-[month]-[day]`, so an unsliced value would
    ///     fail to parse and EVERY DSO sample would be silently dropped
    ///     at the `if let (Ok, Ok)` guard below — the tile would render
    ///     `— (n=0)`. The `SUBSTR` truncates the time component away.
    ///   * a date-only VARCHAR (`2026-06-15`) passes through unchanged.
    ///   * the `CAST` tolerates a legacy pre-PR-73 **DATE**-typed column:
    ///     without it, `row.get::<String>` on a DATE returns
    ///     `InvalidColumnType`, and the `?` would fail the WHOLE
    ///     financial report, not merely DSO.
    issue_date: String,
    payment_deadline: Option<String>,
    vat_rate_basis_points: i32,
    net_minor: i64,
}

#[derive(Debug, Clone)]
struct ApRow {
    /// `ap_invoice.id` — carried so an aging diagnostic can NAME the
    /// offending payable instead of just counting it.
    id: String,
    supplier_name: String,
    payment_deadline: Option<String>,
    net_minor: i64,
    vat_minor: i64,
    gross_minor: i64,
    currency: String,
    local_status: String,
}

#[derive(Debug, Clone)]
struct RestoredRow {
    customer_name: Option<String>,
    net_minor: i64,
    vat_minor: i64,
    gross_minor: i64,
    currency: String,
    partner_id: Option<String>,
}

/// The `basis_date` projection for the outgoing query. Shares
/// [`WINDOW_DATE_EXPR`] on the teljesites basis — same expression, so
/// the column the query groups by cannot drift from the column the
/// window filters on, and both tolerate a legacy DATE-typed
/// `issue_date` (see [`WINDOW_DATE_EXPR`] on why the CAST matters).
fn date_col_sql_invoice(basis: DateBasis) -> &'static str {
    match basis {
        DateBasis::Teljesites => WINDOW_DATE_EXPR,
        DateBasis::Issued => "SUBSTR(CAST(i.issue_date AS VARCHAR), 1, 10)",
    }
}

fn date_col_sql_ap(basis: DateBasis) -> &'static str {
    match basis {
        DateBasis::Teljesites => "COALESCE(a.delivery_date, a.issue_date)",
        DateBasis::Issued => "a.issue_date",
    }
}

fn date_col_sql_restored() -> &'static str {
    // `restored_invoice` has only `issue_date` — Teljesites falls back
    // to the same column.
    "r.issue_date"
}

fn date_str(d: Date) -> String {
    let fmt = format_description!("[year]-[month]-[day]");
    d.format(fmt).expect("ISO date format")
}

fn query_outgoing_groups(
    conn: &Connection,
    window: DateWindow,
    basis: DateBasis,
) -> Result<Vec<OutgoingLineGroup>> {
    let date_col = date_col_sql_invoice(basis);
    let (where_clause, has_from, has_to) = build_date_where(window);
    let sql = format!(
        "SELECT i.id,
                COALESCE(i.currency, 'HUF') AS currency,
                {date_col} AS basis_date,
                SUBSTR(CAST(i.issue_date AS VARCHAR), 1, 10) AS issue_date,
                CAST(i.payment_deadline AS VARCHAR) AS payment_deadline,
                il.vat_rate_basis_points,
                CAST(SUM(CAST(il.quantity AS DECIMAL(38,6)) * il.unit_price) AS VARCHAR) AS net_decimal
           FROM invoice i
           JOIN invoice_line il ON i.id = il.invoice_id
          {where_clause}
          GROUP BY i.id, currency, basis_date, SUBSTR(CAST(i.issue_date AS VARCHAR), 1, 10), payment_deadline,
                   il.vat_rate_basis_points",
    );
    let mut stmt = conn
        .prepare(&sql)
        .context("prepare outgoing aggregate SQL")?;
    let from_s = window.from.map(date_str);
    let to_s = window.to.map(date_str);
    let rows = match (has_from, has_to) {
        (true, true) => stmt.query_map(params![from_s.unwrap(), to_s.unwrap()], row_to_outgoing)?,
        (true, false) => stmt.query_map(params![from_s.unwrap()], row_to_outgoing)?,
        (false, true) => stmt.query_map(params![to_s.unwrap()], row_to_outgoing)?,
        (false, false) => stmt.query_map([], row_to_outgoing)?,
    };
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

fn row_to_outgoing(row: &duckdb::Row) -> duckdb::Result<OutgoingLineGroup> {
    let net_str: String = row.get(6)?;
    let net = decimal_str_to_i64(&net_str).unwrap_or(0);
    Ok(OutgoingLineGroup {
        invoice_id: row.get(0)?,
        currency: row.get(1)?,
        // col 2 is `basis_date` — selected + grouped so the date basis still
        // shapes the query, but the aggregator anchors DSO on issue_date.
        issue_date: row.get(3)?,
        payment_deadline: row.get(4)?,
        vat_rate_basis_points: row.get(5)?,
        net_minor: net,
    })
}

/// S262 / PR-251 — sum of `huf_equivalent_total` (snapshot-rate HUF
/// equivalent of gross, ADR-0037 §1.c) over EUR native invoices in the
/// window. HUF invoices store NULL there (their gross IS the HUF figure),
/// so the `= 'EUR'` predicate is what restricts the sum. Issued basis;
/// see [`CurrencySplitPanel`] for the storno caveat. The window predicate
/// mirrors `query_outgoing_groups` via [`build_date_where`].
fn query_eur_huf_equivalent(
    conn: &Connection,
    window: DateWindow,
    basis: DateBasis,
) -> Result<i64> {
    // `build_date_where` interpolates the teljesites date column; the
    // existing outgoing query relies on the same shape, so the currency
    // split stays consistent with the revenue figure it splits.
    let _ = basis;
    let (date_where, has_from, has_to) = build_date_where(window);
    let currency_pred = "COALESCE(i.currency, 'HUF') = 'EUR'";
    let where_clause = if date_where.is_empty() {
        format!("WHERE {currency_pred}")
    } else {
        format!("{date_where} AND {currency_pred}")
    };
    let sql = format!(
        "SELECT CAST(COALESCE(SUM(i.huf_equivalent_total), 0) AS VARCHAR) AS eur_huf
           FROM invoice i
          {where_clause}",
    );
    let mut stmt = conn
        .prepare(&sql)
        .context("prepare EUR huf-equivalent SQL")?;
    let from_s = window.from.map(date_str);
    let to_s = window.to.map(date_str);
    let read = |row: &duckdb::Row| -> duckdb::Result<i64> {
        let s: String = row.get(0)?;
        Ok(decimal_str_to_i64(&s).unwrap_or(0))
    };
    let mut rows = match (has_from, has_to) {
        (true, true) => stmt.query_map(params![from_s.unwrap(), to_s.unwrap()], read)?,
        (true, false) => stmt.query_map(params![from_s.unwrap()], read)?,
        (false, true) => stmt.query_map(params![to_s.unwrap()], read)?,
        (false, false) => stmt.query_map([], read)?,
    };
    match rows.next() {
        Some(r) => Ok(r?),
        None => Ok(0),
    }
}

fn query_ap_rows(
    conn: &Connection,
    tenant: &str,
    window: DateWindow,
    basis: DateBasis,
) -> Result<Vec<ApRow>> {
    let date_col = date_col_sql_ap(basis);
    let mut clauses = vec!["a.tenant_id = ?".to_string()];
    let mut binds: Vec<String> = vec![tenant.to_string()];
    if let Some(from) = window.from {
        clauses.push(format!("{date_col} >= ?"));
        binds.push(date_str(from));
    }
    if let Some(to) = window.to {
        clauses.push(format!("{date_col} <= ?"));
        binds.push(date_str(to));
    }
    let sql = format!(
        "SELECT a.id, a.supplier_name, a.payment_deadline,
                a.total_net_minor, a.total_vat_minor, a.total_gross_minor, a.currency,
                a.local_status
           FROM ap_invoice a
          WHERE {}",
        clauses.join(" AND ")
    );
    let mut stmt = conn.prepare(&sql).context("prepare ap_invoice SQL")?;
    let params_dyn: Vec<&dyn duckdb::ToSql> =
        binds.iter().map(|s| s as &dyn duckdb::ToSql).collect();
    let rows = stmt.query_map(params_dyn.as_slice(), |row| {
        Ok(ApRow {
            id: row.get(0)?,
            supplier_name: row.get(1)?,
            payment_deadline: row.get(2)?,
            net_minor: row.get(3)?,
            vat_minor: row.get(4)?,
            gross_minor: row.get(5)?,
            currency: row.get(6)?,
            local_status: row.get(7)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

fn query_restored_rows(
    conn: &Connection,
    tenant: &str,
    window: DateWindow,
) -> Result<Vec<RestoredRow>> {
    let date_col = date_col_sql_restored();
    let mut clauses = vec!["r.tenant_id = ?".to_string()];
    let mut binds: Vec<String> = vec![tenant.to_string()];
    if let Some(from) = window.from {
        clauses.push(format!("{date_col} >= ?"));
        binds.push(date_str(from));
    }
    if let Some(to) = window.to {
        clauses.push(format!("{date_col} <= ?"));
        binds.push(date_str(to));
    }
    let sql = format!(
        "SELECT r.customer_name,
                r.total_net_minor, r.total_vat_minor, r.total_gross_minor,
                r.currency, r.partner_id
           FROM restored_invoice r
          WHERE {}",
        clauses.join(" AND ")
    );
    let mut stmt = conn.prepare(&sql).context("prepare restored_invoice SQL")?;
    let params_dyn: Vec<&dyn duckdb::ToSql> =
        binds.iter().map(|s| s as &dyn duckdb::ToSql).collect();
    let rows = stmt.query_map(params_dyn.as_slice(), |row| {
        Ok(RestoredRow {
            customer_name: row.get(0)?,
            net_minor: row.get(1)?,
            vat_minor: row.get(2)?,
            gross_minor: row.get(3)?,
            currency: row.get(4)?,
            partner_id: row.get(5)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// The ONE invoice-side date-window expression — the teljesites basis
/// (`delivery_date`, falling back to `issue_date`) normalised to a
/// date-only `YYYY-MM-DD` string. Every window `WHERE` arm compares
/// this, so revenue, VAT, receivables, DSO and the currency split can
/// never disagree about what falls inside a period.
///
/// Both halves are load-bearing:
///   * the **`SUBSTR`** is the fix. `invoice.issue_date` is a VARCHAR
///     holding RFC3339 (`duckdb_store.rs` writes
///     `draft.issue_date.format(&Rfc3339)`), and `delivery_date` stays
///     NULL for pre-PR-84 rows and every non-SPA issuance — so the
///     COALESCE falls through to a value like `2026-06-30T21:15:00Z`.
///     Compared as a raw string against the date-only upper bound
///     `2026-06-30`, that sorts ABOVE it, and an invoice issued on the
///     period's LAST DAY silently dropped out of revenue, VAT and AR.
///     Truncating to 10 chars puts both sides in the same shape.
///   * the **`CAST(i.issue_date AS VARCHAR)`** tolerates a legacy
///     pre-PR-73 DATE-typed `issue_date`. Without it the COALESCE mixes
///     VARCHAR and DATE and DuckDB binder-errors, failing the WHOLE
///     financial report rather than one tile.
///
/// Recovering the boundary rows is the entire behaviour change: no
/// invoice that already counted toward a period leaves it.
const WINDOW_DATE_EXPR: &str =
    "SUBSTR(COALESCE(CAST(i.delivery_date AS VARCHAR), CAST(i.issue_date AS VARCHAR)), 1, 10)";

/// Emit the invoice-side date-window `WHERE` clause plus which bind slots
/// it uses. Every arm compares [`WINDOW_DATE_EXPR`], so revenue, VAT,
/// receivables, DSO and the currency split all decide period membership
/// with ONE predicate.
fn build_date_where(window: DateWindow) -> (String, bool, bool) {
    match (window.from, window.to) {
        (Some(_), Some(_)) => (
            format!("WHERE {WINDOW_DATE_EXPR} >= ? AND {WINDOW_DATE_EXPR} <= ?"),
            true,
            true,
        ),
        (Some(_), None) => (format!("WHERE {WINDOW_DATE_EXPR} >= ?"), true, false),
        (None, Some(_)) => (format!("WHERE {WINDOW_DATE_EXPR} <= ?"), false, true),
        (None, None) => (String::new(), false, false),
    }
}

fn decimal_str_to_i64(s: &str) -> Option<i64> {
    // The aggregate is `SUM(quantity * unit_price)` where quantity is
    // DECIMAL(18,6) and unit_price is BIGINT — the product fits a wide
    // DECIMAL. We round-half-even to whole minor units at the i64
    // boundary; per-line rounding would match NAV byte-perfect but the
    // dashboard is a management view (see module-level note).
    use rust_decimal::RoundingStrategy;
    let d: Decimal = s.parse().ok()?;
    d.round_dp_with_strategy(0, RoundingStrategy::MidpointNearestEven)
        .to_i64()
}

// ──────────────────────────────────────────────────────────────────────
// Aggregation.
// ──────────────────────────────────────────────────────────────────────

/// Per-(currency, vat_rate) accumulator key used while bucketing the
/// outgoing line groups.
type VatBucketKey = (String, i32);

#[derive(Default)]
struct OutgoingAggregate {
    revenue: CurrencyAggregate,
    vat_collected: CurrencyAggregate,
    receivables: CurrencyAggregate,
    receivables_aging: AgingPanel,
    cashflow_forward: CashflowPanel,
    vat_breakdown: BTreeMap<VatBucketKey, (i64, i64)>,
    top_customers: HashMap<(String, String), (i64, u64)>, // (label, currency) -> (gross, count)
    /// (paid_at - issue_date) sample for DSO calculation, by currency.
    dso_huf_samples: Vec<f64>,
    dso_eur_samples: Vec<f64>,
    counted_invoice_ids: HashSet<String>,
    outstanding_past_deadline_count: u64,
    /// Receivables aged without a usable deadline — see [`aging_placement`].
    aging_undated: AgingUndated,
    rejected_count: u64,
    abandoned_count: u64,
    pending_count: u64,
}

/// Aggregate outgoing native invoices over the windowed line groups,
/// classifying each invoice via the trace map and flipping the sign for
/// storno-self rows.
fn aggregate_outgoing(
    groups: Vec<OutgoingLineGroup>,
    traces: &HashMap<String, ReportTrace>,
    today: Date,
    buyer_names: &HashMap<String, String>,
) -> OutgoingAggregate {
    let mut agg = OutgoingAggregate::default();
    // Per-invoice aggregator: collapse multiple rows-per-invoice (one
    // per VAT rate) into one count + one gross.
    let mut per_invoice: HashMap<String, (String, i64, i64, Option<String>, String, bool)> =
        HashMap::new(); // id -> (currency, net, vat, payment_deadline, issue_date, is_storno_self)
    for group in groups {
        let trace = traces.get(&group.invoice_id).cloned().unwrap_or_default();
        let kind = trace.classify();
        match kind {
            CountedKind::Counted { is_storno_self } => {
                let sign: i64 = if is_storno_self { -1 } else { 1 };
                let net_signed = group.net_minor.saturating_mul(sign);
                // Round-half-even on VAT to whole minor units.
                let vat_signed = round_half_even_div(
                    net_signed.saturating_mul(group.vat_rate_basis_points as i64),
                    10_000,
                );
                let entry = agg
                    .vat_breakdown
                    .entry((group.currency.clone(), group.vat_rate_basis_points))
                    .or_insert((0, 0));
                entry.0 = entry.0.saturating_add(net_signed);
                entry.1 = entry.1.saturating_add(vat_signed);
                let inv_entry = per_invoice.entry(group.invoice_id.clone()).or_insert((
                    group.currency.clone(),
                    0,
                    0,
                    group.payment_deadline.clone(),
                    group.issue_date.clone(),
                    is_storno_self,
                ));
                inv_entry.1 = inv_entry.1.saturating_add(net_signed);
                inv_entry.2 = inv_entry.2.saturating_add(vat_signed);
                agg.counted_invoice_ids.insert(group.invoice_id.clone());
            }
            CountedKind::Rejected => {
                agg.rejected_count = agg.rejected_count.saturating_add(1);
                // Don't double-count per VAT row — collapse into a set.
            }
            CountedKind::Abandoned => {
                agg.abandoned_count = agg.abandoned_count.saturating_add(1);
            }
            CountedKind::PendingDraft => {
                agg.pending_count = agg.pending_count.saturating_add(1);
            }
            CountedKind::Unknown => {}
        }
    }
    // De-duplicate the hygiene counters (per VAT rate produces N rows
    // per invoice; the counter should count invoices, not rows).
    let mut seen_rejected: HashSet<String> = HashSet::new();
    let mut seen_abandoned: HashSet<String> = HashSet::new();
    let mut seen_pending: HashSet<String> = HashSet::new();
    agg.rejected_count = 0;
    agg.abandoned_count = 0;
    agg.pending_count = 0;
    for (id, trace) in traces {
        match trace.classify() {
            CountedKind::Rejected if seen_rejected.insert(id.clone()) => {
                agg.rejected_count += 1;
            }
            CountedKind::Abandoned if seen_abandoned.insert(id.clone()) => {
                agg.abandoned_count += 1;
            }
            CountedKind::PendingDraft if seen_pending.insert(id.clone()) => {
                agg.pending_count += 1;
            }
            _ => {}
        }
    }
    // Materialise per-invoice contributions into the currency aggregate
    // + receivables + DSO + top-customers.
    for (id, (currency, net, vat, deadline, issue_date, is_storno_self)) in &per_invoice {
        let gross = net.saturating_add(*vat);
        let target = match currency.as_str() {
            "EUR" => &mut agg.revenue.eur,
            _ => &mut agg.revenue.huf,
        };
        target.net_minor = target.net_minor.saturating_add(*net);
        target.vat_minor = target.vat_minor.saturating_add(*vat);
        target.gross_minor = target.gross_minor.saturating_add(gross);
        target.count = target.count.saturating_add(1);
        // VAT collected — same totals; in v1 VAT-collected mirrors the
        // VAT line of revenue. (When AAM / reverse-charge sub-bucketing
        // lands, those rows will be excluded from this aggregate.)
        let vat_target = match currency.as_str() {
            "EUR" => &mut agg.vat_collected.eur,
            _ => &mut agg.vat_collected.huf,
        };
        vat_target.gross_minor = vat_target.gross_minor.saturating_add(*vat);
        vat_target.vat_minor = vat_target.vat_minor.saturating_add(*vat);
        vat_target.count = vat_target.count.saturating_add(1);
        // Receivables: counted-but-not-paid. Two exclusions, and both are
        // needed for the pair to net:
        //   * storno-self rows — self-resolving (the negation IS the
        //     payment);
        //   * bases whose storno has LANDED — the cancelled original is no
        //     longer owed. Without this the base's + gross leaked into AR
        //     while its − credit note dropped out, so a cancelled invoice
        //     inflated receivables by its full amount.
        // `has_landed_storno` is deliberately NOT the issuance-time flag:
        // a base whose storno was aborted / never submitted still stands as
        // a real unpaid receivable and MUST remain here (see
        // [`resolve_landed_stornos`]).
        //
        // This one predicate also gates receivables_aging,
        // outstanding_past_deadline_count and cashflow_forward below.
        let trace = traces.get(id).cloned().unwrap_or_default();
        let paid = trace.payment_paid_at.is_some();
        if !paid && !*is_storno_self && !trace.has_landed_storno {
            let ar_target = match currency.as_str() {
                "EUR" => &mut agg.receivables.eur,
                _ => &mut agg.receivables.huf,
            };
            ar_target.net_minor = ar_target.net_minor.saturating_add(*net);
            ar_target.vat_minor = ar_target.vat_minor.saturating_add(*vat);
            ar_target.gross_minor = ar_target.gross_minor.saturating_add(gross);
            ar_target.count = ar_target.count.saturating_add(1);
            // Aging + cashflow forward bucketing. UNCONDITIONAL: this
            // receivable is already in the total three lines up, so it
            // gets a bucket no matter what its deadline says. See
            // `aging_placement` for the missing/unreadable case.
            let (bucket, deadline_d) =
                aging_placement(today, id, deadline.as_deref(), &mut agg.aging_undated);
            accrue_aging(&mut agg.receivables_aging, bucket, *net, *vat, gross);
            // NOT `!matches!(bucket, Current)` alone. The bucket above is
            // an age ESTIMATE and an undated invoice was given an imputed
            // one; this counter is an ASSERTION — "this invoice is late" —
            // and an unreadable deadline means unknown lateness, not
            // lateness. `deadline_d.is_some()` IS the not-imputed test.
            // See `aging_placement`.
            if deadline_d.is_some() && !matches!(bucket, AgingBucket::Current) {
                agg.outstanding_past_deadline_count =
                    agg.outstanding_past_deadline_count.saturating_add(1);
            }
            // Forward look only for not-yet-overdue receivables — which
            // is also why an undated one never lands here: `Current`
            // implies a deadline we could read.
            if let (AgingBucket::Current, Some(deadline_d)) = (bucket, deadline_d) {
                let days_out = (deadline_d - today).whole_days();
                let pair_target = match currency.as_str() {
                    "EUR" => {
                        |p: &mut CurrencyPair, v: i64| p.eur_minor = p.eur_minor.saturating_add(v)
                    }
                    _ => |p: &mut CurrencyPair, v: i64| p.huf_minor = p.huf_minor.saturating_add(v),
                };
                if days_out <= 30 {
                    pair_target(&mut agg.cashflow_forward.next_30, gross);
                }
                if days_out <= 60 {
                    pair_target(&mut agg.cashflow_forward.next_60, gross);
                }
                if days_out <= 90 {
                    pair_target(&mut agg.cashflow_forward.next_90, gross);
                }
            }
        }
        // DSO sample — paid invoice (not storno-self), days between
        // paid_at and the invoice's ISSUE date.
        //
        // The anchor must be issue_date, not the basis/teljesites date:
        // under the teljesites basis the anchor is
        // `COALESCE(delivery_date, issue_date)`, so a prepayment — paid
        // before fulfillment — produces a NEGATIVE days-sales-outstanding
        // sample and drags the mean below zero.
        if !*is_storno_self {
            if let (Some(paid_at), _) = (&trace.payment_paid_at, &trace.payment_amount_minor) {
                if let (Ok(paid_d), Ok(issued_d)) =
                    (parse_iso_date(paid_at), parse_iso_date(issue_date))
                {
                    let days = (paid_d - issued_d).whole_days() as f64;
                    if currency.as_str() == "EUR" {
                        agg.dso_eur_samples.push(days);
                    } else {
                        agg.dso_huf_samples.push(days);
                    }
                }
            }
        }
        // Top customers — keyed by buyer_name lookup (best-effort).
        if let Some(name) = buyer_names.get(id) {
            let key = (name.clone(), currency.clone());
            let entry = agg.top_customers.entry(key).or_insert((0, 0));
            entry.0 = entry.0.saturating_add(gross);
            entry.1 = entry.1.saturating_add(1);
        }
    }
    agg
}

#[derive(Debug, Clone, Copy)]
enum AgingBucket {
    Current,
    Days1To30,
    Days31To60,
    Days61To90,
    Days90Plus,
}

fn aging_bucket_for(today: Date, deadline: Date) -> AgingBucket {
    let overdue_days = (today - deadline).whole_days();
    if overdue_days <= 0 {
        AgingBucket::Current
    } else if overdue_days <= 30 {
        AgingBucket::Days1To30
    } else if overdue_days <= 60 {
        AgingBucket::Days31To60
    } else if overdue_days <= 90 {
        AgingBucket::Days61To90
    } else {
        AgingBucket::Days90Plus
    }
}

/// Per-side tally of invoices aged WITHOUT a usable deadline. Merged onto
/// [`LedgerDiagnostics`] when the report is assembled.
#[derive(Default)]
struct AgingUndated {
    count: u64,
    ids: Vec<String>,
}

impl AgingUndated {
    fn record(&mut self, invoice_id: &str) {
        self.count = self.count.saturating_add(1);
        if self.ids.len() < MAX_UNPARSEABLE_ENTRY_IDS {
            self.ids.push(invoice_id.to_string());
        }
    }
}

/// Place ONE outstanding invoice (receivable or payable) into an aging
/// bucket. Returns the bucket plus the parsed deadline — `None` when there
/// wasn't one to parse.
///
/// **The invariant this exists to hold: every invoice counted in the
/// receivables / payables TOTAL lands in exactly one aging bucket, so
/// `sum(buckets) == total`, always.**
///
/// Until now both aging sites read `if let Ok(d) = parse_iso_date(..)`
/// with no `else`, nested inside `if let Some(deadline)`. An invoice with
/// a missing or malformed `payment_deadline` therefore fell through BOTH
/// arms into nothing — while the lines just above had already added it to
/// the receivables/payables total. The panel's own breakdown summed to
/// less than the panel's own total, with no signal anywhere: the operator
/// reads "Receivables 4 500 000" over buckets adding to 3 200 000 and has
/// no way to learn which is the lie. Same silent-drop class as the audit
/// entries fixed alongside this, different code path.
///
/// Bucketing for an unusable deadline is `Days90Plus` — the MOST overdue
/// bucket. It is an imputation and it is deliberately the pessimistic one:
/// an invoice whose due date cannot be read cannot be assumed not-yet-due,
/// and understating overdue exposure is the error that costs money.
/// `Days90Plus` is used rather than a new "undated" bucket because the
/// aging vocabulary is closed and mirrored in three places (this fn, the
/// SPA's `aging.ts`, and the `?aging=` click-through deep links); widening
/// it is a UI change, not a bug fix. The imputation is disclosed via
/// [`LedgerDiagnostics::aging_undated_invoices`] and a log line per
/// invoice, so it is never mistaken for a measurement.
///
/// **An imputed bucket is an age ESTIMATE and travels only where an
/// estimate is admissible.** It does NOT reach the past-deadline hygiene
/// counters, which assert "this invoice is late" — a claim nothing here
/// supports; callers gate those on `deadline.is_some()`, which is exactly
/// what the returned `Option<Date>` is for. It also keeps the invoice out
/// of the cash-flow forward projection (only `Current` feeds it): an
/// undated invoice must not be promised as incoming cash. What it does
/// reach is the aging panel, where the alternative was vanishing, and the
/// diagnostics that say so.
///
/// Called once per outstanding invoice per aggregation run. The
/// comparative windows (MoM / YoY / annual-running) run the same
/// aggregation over their own windows; only the primary window's tally
/// rides out on the wire.
fn aging_placement(
    today: Date,
    invoice_id: &str,
    deadline: Option<&str>,
    undated: &mut AgingUndated,
) -> (AgingBucket, Option<Date>) {
    match deadline {
        Some(raw) => match parse_iso_date(raw) {
            Ok(parsed) => (aging_bucket_for(today, parsed), Some(parsed)),
            Err(error) => {
                // A deadline that is PRESENT but malformed is genuinely
                // wrong data — both writers validate the shape on the way
                // in (`incoming_invoices::validate`, the billing store),
                // so reaching here means something bypassed them. Rare,
                // and worth an error line each.
                tracing::error!(
                    invoice_id = %invoice_id,
                    payment_deadline = %raw,
                    parse_error = %error,
                    "financial report: outstanding invoice has an UNREADABLE payment_deadline \
                     — aged as 90+ (most overdue) so the buckets still sum to the total; its \
                     real age is unknown until the deadline is fixed"
                );
                undated.record(invoice_id);
                (AgingBucket::Days90Plus, None)
            }
        },
        None => {
            // An ABSENT deadline is not an error and must not be logged
            // like one: `ap_sync` records `payment_deadline: None` on
            // every NAV-synced payable, so at error level this would emit
            // a line per payable per dashboard load and drown the ledger
            // diagnostics that ARE errors. The honest signal is the
            // aggregate — one WARN summary at the end of the report plus
            // the machine-countable
            // `LedgerDiagnostics::aging_undated_invoices` — with the
            // per-invoice attribution kept at debug for whoever is
            // actually chasing one.
            tracing::debug!(
                invoice_id = %invoice_id,
                "financial report: outstanding invoice has NO payment_deadline — aged as 90+ \
                 (most overdue) so the buckets still sum to the total; its real age is unknown \
                 until a deadline is set"
            );
            undated.record(invoice_id);
            (AgingBucket::Days90Plus, None)
        }
    }
}

/// The bucket's slot on a panel — the one place the bucket→field mapping
/// lives, so the two aging sites cannot drift apart.
fn aging_slot(panel: &mut AgingPanel, bucket: AgingBucket) -> &mut AmountAggregate {
    match bucket {
        AgingBucket::Current => &mut panel.current,
        AgingBucket::Days1To30 => &mut panel.days_1_30,
        AgingBucket::Days31To60 => &mut panel.days_31_60,
        AgingBucket::Days61To90 => &mut panel.days_61_90,
        AgingBucket::Days90Plus => &mut panel.days_90_plus,
    }
}

/// Add one invoice's amounts to its aging bucket.
fn accrue_aging(panel: &mut AgingPanel, bucket: AgingBucket, net: i64, vat: i64, gross: i64) {
    let dest = aging_slot(panel, bucket);
    dest.net_minor = dest.net_minor.saturating_add(net);
    dest.vat_minor = dest.vat_minor.saturating_add(vat);
    dest.gross_minor = dest.gross_minor.saturating_add(gross);
    dest.count = dest.count.saturating_add(1);
}

/// Round-half-even integer division (banker's rounding) — matches the
/// `huf_equivalent_round_half_even` posture in `modules/billing` for
/// the VAT minor-unit rounding pass. `divisor` is assumed positive
/// (10000 for basis-points-to-fraction conversion).
fn round_half_even_div(numerator: i64, divisor: i64) -> i64 {
    if divisor == 0 {
        return 0;
    }
    let n = Decimal::from(numerator);
    let d = Decimal::from(divisor);
    let q = n.checked_div(d).unwrap_or(Decimal::ZERO);
    use rust_decimal::RoundingStrategy;
    q.round_dp_with_strategy(0, RoundingStrategy::MidpointNearestEven)
        .to_i64()
        .unwrap_or(0)
}

// ──────────────────────────────────────────────────────────────────────
// Top-level orchestration.
// ──────────────────────────────────────────────────────────────────────

/// Compute the financial report for the given period + date basis.
///
/// Reads the audit ledger (one walk) + three SQL aggregates against the
/// invoice + restored_invoice + ap_invoice tables; combines into a
/// single JSON snapshot. Computes MoM + YoY deltas by re-running the
/// SQL aggregates against the prior periods (the audit-ledger walk is
/// re-used).
pub fn compute_financial_report(
    db_path: &Path,
    tenant: TenantId,
    binary_hash: BinaryHash,
    req: ReportRequest,
) -> Result<FinancialReport> {
    let window = resolve_window(req.period)?;
    let conn = Connection::open(db_path)
        .with_context(|| format!("open DuckDB at {} for financial report", db_path.display()))?;
    conn.execute_batch("PRAGMA disable_checkpoint_on_shutdown;")
        .context("ADR-0098 R3 (finding C): disable implicit close-checkpoint on residual opener")?;

    // Ensure relevant schemas exist (idempotent; mirrors how the existing
    // list endpoints lazily ensure schema on first read). Billing-side
    // schema is bootstrapped via the typed store so the `invoice` +
    // `invoice_line` tables exist on a fresh DB; the AP and restored
    // mirrors carry their own idempotent CREATE.
    let _ = crate::incoming_invoices::ensure_schema(&conn);
    let _ = crate::restore_from_nav_outgoing::ensure_schema(&conn);
    drop(conn);
    {
        use aberp_billing::ports::storage::BillingStore;
        let mut store = aberp_billing::DuckDbBillingStore::open(db_path)
            .context("open billing store for schema bootstrap")?;
        store
            .ensure_schema()
            .context("ensure billing-side schema for financial report")?;
    }
    let conn = Connection::open(db_path).with_context(|| {
        format!(
            "re-open DuckDB at {} after billing-store schema bootstrap",
            db_path.display()
        )
    })?;
    conn.execute_batch("PRAGMA disable_checkpoint_on_shutdown;")
        .context("ADR-0098 R3 (finding C): disable implicit close-checkpoint on residual opener")?;

    let tenant_str = tenant.as_str().to_string();
    let ledger = Ledger::open(db_path, tenant.clone(), binary_hash)
        .context("open audit ledger for financial report")?;
    let walk = walk_ledger(&ledger, window)?;
    // The walk already error-logged each undecodable entry individually.
    // This is the one operator-facing line that says the SNAPSHOT as a
    // whole is suspect, and it rides out on the report so a caller/UI can
    // say so too.
    if walk.diagnostics.unparseable_entries > 0 {
        tracing::error!(
            unparseable_entries = walk.diagnostics.unparseable_entries,
            "financial report: figures may be INCOMPLETE — some audit entries could not be read"
        );
    }

    // Build a best-effort buyer-name map by reading side-store input.json
    // files for each `InvoiceDraftCreated` entry's `nav_xml_path`. Same
    // posture `serve::list_invoices` takes (S215). Best-effort: missing /
    // unreadable / blank → no entry.
    let buyer_names = build_buyer_names_map(&ledger)?;

    let outgoing_groups = query_outgoing_groups(&conn, window, req.date_basis)?;
    let ap_rows = query_ap_rows(&conn, &tenant_str, window, req.date_basis)?;
    let restored_rows = query_restored_rows(&conn, &tenant_str, window)?;

    let mut outgoing = aggregate_outgoing(outgoing_groups, &walk.traces, req.today, &buyer_names);

    // S262 / PR-251 — capture the NATIVE outgoing revenue (canonical
    // `invoice` table only, storno-adjusted) BEFORE the restored-mirror
    // loop folds digest rows into `outgoing.revenue`. The currency split
    // is snapshot-rate based and restored/AP rows carry no per-invoice
    // snapshot rate, so the split must exclude them.
    let native_revenue = outgoing.revenue.clone();
    let eur_as_huf_minor = query_eur_huf_equivalent(&conn, window, req.date_basis)?;
    let currency_split = CurrencySplitPanel {
        huf_minor: native_revenue.huf.gross_minor,
        huf_count: native_revenue.huf.count,
        eur_native_minor: native_revenue.eur.gross_minor,
        eur_count: native_revenue.eur.count,
        eur_as_huf_minor,
    };

    // Restored rows contribute to revenue + VAT-collected. No line-level
    // breakdown available (digest-only). No storno detection (the
    // restored mirror is read-only).
    for r in &restored_rows {
        let target = match r.currency.as_str() {
            "EUR" => &mut outgoing.revenue.eur,
            _ => &mut outgoing.revenue.huf,
        };
        target.net_minor = target.net_minor.saturating_add(r.net_minor);
        target.vat_minor = target.vat_minor.saturating_add(r.vat_minor);
        target.gross_minor = target.gross_minor.saturating_add(r.gross_minor);
        target.count = target.count.saturating_add(1);
        let vat_target = match r.currency.as_str() {
            "EUR" => &mut outgoing.vat_collected.eur,
            _ => &mut outgoing.vat_collected.huf,
        };
        vat_target.vat_minor = vat_target.vat_minor.saturating_add(r.vat_minor);
        vat_target.gross_minor = vat_target.gross_minor.saturating_add(r.vat_minor);
        vat_target.count = vat_target.count.saturating_add(1);
        // Top customers — restored rows carry buyer_name in-row (S218).
        if let Some(name) = &r.customer_name {
            let key = (name.clone(), r.currency.clone());
            let entry = outgoing.top_customers.entry(key).or_insert((0, 0));
            entry.0 = entry.0.saturating_add(r.gross_minor);
            entry.1 = entry.1.saturating_add(1);
        }
    }

    let ap = aggregate_ap(&ap_rows, req.today);
    let payable_past_deadline = ap.payable_past_deadline;

    let deferred_notes: Vec<String> = vec![
        "Revenue currency split now ships in snapshot-rate HUF (S262); FX-aggregated \
         expenses + a unified all-in-HUF P&L line remain deferred."
            .into(),
        "HIPA base + KATA/KIVA threshold logic deferred to v2.3 (separate ADR).".into(),
        "AAM / reverse-charge / EU-0 VAT sub-buckets deferred — schema does not tag them today."
            .into(),
        "Per-VAT-rate breakdown for incoming + restored deferred (digest-only ingestion in v1)."
            .into(),
    ];

    // Period-over-period deltas — re-run the aggregates over the prior
    // comparable window. Custom + All periods get None.
    let mom = compute_delta(
        &conn,
        &tenant_str,
        &walk.traces,
        &buyer_names,
        req.date_basis,
        req.today,
        previous_period(req.period),
        &outgoing.revenue,
        &ap.expenses,
    )?;
    let yoy = compute_delta(
        &conn,
        &tenant_str,
        &walk.traces,
        &buyer_names,
        req.date_basis,
        req.today,
        yoy_period(req.period),
        &outgoing.revenue,
        &ap.expenses,
    )?;

    // Annual running revenue — YTD up to today for the year that
    // contains `today`. Uses the same date basis as the current request.
    let annual_running = compute_annual_running(
        &conn,
        &tenant_str,
        &walk.traces,
        &buyer_names,
        req.date_basis,
        req.today,
    )?;

    // VAT breakdown → wire shape (sorted by rate DESC for the UI).
    let vat_breakdown_outgoing: Vec<VatRateBreakdownEntry> = outgoing
        .vat_breakdown
        .into_iter()
        .map(|((currency, rate_bp), (net, vat))| VatRateBreakdownEntry {
            rate_basis_points: rate_bp,
            currency,
            net_minor: net,
            vat_minor: vat,
        })
        .collect();

    // Top-N — sort by gross_minor DESC, take the operator-chosen N.
    let top_customers = top_n_from_map(outgoing.top_customers, req.top_n);
    let top_vendors = top_n_from_map(ap.top_vendors, req.top_n);

    // Hygiene panel — combine outgoing + ap + restored signals.
    let restored_no_partner_count = restored_rows
        .iter()
        .filter(|r| r.partner_id.is_none())
        .count() as u64;
    let hygiene = HygienePanel {
        outgoing_rejected_count: outgoing.rejected_count,
        outgoing_abandoned_count: outgoing.abandoned_count,
        outgoing_pending_count: outgoing.pending_count,
        restored_no_partner_count,
        outstanding_past_deadline_count: outgoing.outstanding_past_deadline_count,
        payable_past_deadline_count: payable_past_deadline,
        storno_chain_count: walk.storno_links_in_period,
        modification_chain_count: walk.modification_links_in_period,
    };

    // Gross profit + VAT-to-pay deltas.
    let gross_profit = CurrencyPair {
        huf_minor: outgoing
            .revenue
            .huf
            .gross_minor
            .saturating_sub(ap.expenses.huf.gross_minor),
        eur_minor: outgoing
            .revenue
            .eur
            .gross_minor
            .saturating_sub(ap.expenses.eur.gross_minor),
    };
    let vat_to_pay = CurrencyPair {
        huf_minor: outgoing
            .vat_collected
            .huf
            .vat_minor
            .saturating_sub(ap.vat_paid.huf.vat_minor),
        eur_minor: outgoing
            .vat_collected
            .eur
            .vat_minor
            .saturating_sub(ap.vat_paid.eur.vat_minor),
    };

    // Merge the two aging tallies onto the walk's diagnostics — one
    // integrity object goes out on the wire.
    //
    // The COUNT is exact. The id list is a sample, and honestly so: each
    // side already capped its own ids at `MAX_UNPARSEABLE_ENTRY_IDS` while
    // collecting, so the order here is cap → merge → sort → cap. The
    // published ids are therefore a sorted subset of "the first 50 each
    // side happened to see", NOT the 50 smallest ids overall — and on the
    // AR side "happened to see" is `HashMap` iteration order, which is not
    // stable between runs. The sort buys presentation determinism for a
    // given set, not a deterministic set. That is the same
    // count-exact/ids-are-a-starting-point contract
    // `unparseable_entry_ids` ships under; anyone needing the full list
    // reads the log.
    let mut ledger_diagnostics = walk.diagnostics;
    ledger_diagnostics.aging_undated_receivables = outgoing.aging_undated.count;
    ledger_diagnostics.aging_undated_payables = ap.aging_undated.count;
    ledger_diagnostics.aging_undated_invoices = outgoing
        .aging_undated
        .count
        .saturating_add(ap.aging_undated.count);
    let mut undated_ids = outgoing.aging_undated.ids;
    undated_ids.extend(ap.aging_undated.ids);
    undated_ids.sort();
    undated_ids.truncate(MAX_UNPARSEABLE_ENTRY_IDS);
    ledger_diagnostics.aging_undated_invoice_ids = undated_ids;
    // WARN, not ERROR, and once per report rather than once per invoice:
    // nothing is missing from the figures and a NAV-synced payables book
    // is legitimately all-undated, so this is a caveat on the aging
    // panels, not a fault. Contrast the `unparseable_entries` line above,
    // which says the figures themselves may be incomplete.
    if ledger_diagnostics.aging_undated_invoices > 0 {
        tracing::warn!(
            aging_undated_invoices = ledger_diagnostics.aging_undated_invoices,
            "financial report: some outstanding invoices have no readable payment deadline — \
             they are counted in full and aged as 90+ by imputation; their aging bucket is an \
             estimate and they are excluded from the past-deadline counters"
        );
    }

    let dso_days = DsoPanel {
        huf_days: mean(&outgoing.dso_huf_samples),
        eur_days: mean(&outgoing.dso_eur_samples),
        huf_sample_size: outgoing.dso_huf_samples.len() as u64,
        eur_sample_size: outgoing.dso_eur_samples.len() as u64,
    };

    Ok(FinancialReport {
        period: PeriodMeta {
            kind: period_kind_label(req.period).into(),
            label: period_label(req.period),
            from: window.from.map(date_str),
            to: window.to.map(date_str),
            date_basis: req.date_basis.as_wire_str().into(),
            today: date_str(req.today),
        },
        revenue: outgoing.revenue,
        expenses: ap.expenses,
        gross_profit,
        vat_collected: outgoing.vat_collected,
        vat_paid: ap.vat_paid,
        vat_to_pay,
        receivables: outgoing.receivables,
        payables: ap.payables,
        currency_split,
        receivables_aging: outgoing.receivables_aging,
        payables_aging: ap.payables_aging,
        dso_days,
        cashflow_forward: outgoing.cashflow_forward,
        vat_breakdown_outgoing,
        top_customers,
        top_vendors,
        hygiene,
        deltas: PeriodDeltas { mom, yoy },
        annual_running,
        deferred_notes,
        ledger_diagnostics,
    })
}

#[derive(Default)]
struct ApAggregate {
    expenses: CurrencyAggregate,
    vat_paid: CurrencyAggregate,
    payables: CurrencyAggregate,
    payables_aging: AgingPanel,
    top_vendors: HashMap<(String, String), (i64, u64)>,
    payable_past_deadline: u64,
    /// Payables aged without a usable deadline — see [`aging_placement`].
    aging_undated: AgingUndated,
}

/// AP-side aggregation: expenses + VAT-paid + payables + payable-aging +
/// top vendors. Irrelevant rows are excluded from every bucket per the
/// S177 closed-vocab semantics (operator declared not-our-problem).
///
/// Extracted from [`compute_financial_report`]'s body so the payables
/// aging invariant (`sum(buckets) == payables total`) is unit-testable
/// without standing up a DuckDB fixture — the AR side already was, and
/// only the AR side had pins.
fn aggregate_ap(rows: &[ApRow], today: Date) -> ApAggregate {
    let mut ap = ApAggregate::default();
    for r in rows {
        if r.local_status == "Irrelevant" {
            continue;
        }
        let exp_target = match r.currency.as_str() {
            "EUR" => &mut ap.expenses.eur,
            _ => &mut ap.expenses.huf,
        };
        exp_target.net_minor = exp_target.net_minor.saturating_add(r.net_minor);
        exp_target.vat_minor = exp_target.vat_minor.saturating_add(r.vat_minor);
        exp_target.gross_minor = exp_target.gross_minor.saturating_add(r.gross_minor);
        exp_target.count = exp_target.count.saturating_add(1);
        let vp_target = match r.currency.as_str() {
            "EUR" => &mut ap.vat_paid.eur,
            _ => &mut ap.vat_paid.huf,
        };
        vp_target.vat_minor = vp_target.vat_minor.saturating_add(r.vat_minor);
        vp_target.gross_minor = vp_target.gross_minor.saturating_add(r.vat_minor);
        vp_target.count = vp_target.count.saturating_add(1);
        // Top vendors
        let key = (r.supplier_name.clone(), r.currency.clone());
        let entry = ap.top_vendors.entry(key).or_insert((0, 0));
        entry.0 = entry.0.saturating_add(r.gross_minor);
        entry.1 = entry.1.saturating_add(1);
        // Payables + aging — outstanding only.
        if r.local_status == "Outstanding" {
            let p_target = match r.currency.as_str() {
                "EUR" => &mut ap.payables.eur,
                _ => &mut ap.payables.huf,
            };
            p_target.net_minor = p_target.net_minor.saturating_add(r.net_minor);
            p_target.vat_minor = p_target.vat_minor.saturating_add(r.vat_minor);
            p_target.gross_minor = p_target.gross_minor.saturating_add(r.gross_minor);
            p_target.count = p_target.count.saturating_add(1);
            // UNCONDITIONAL, same reason as the AR side: this payable is
            // already in the total above, so it gets a bucket whatever its
            // deadline says. See `aging_placement`.
            let (bucket, deadline_d) = aging_placement(
                today,
                &r.id,
                r.payment_deadline.as_deref(),
                &mut ap.aging_undated,
            );
            accrue_aging(
                &mut ap.payables_aging,
                bucket,
                r.net_minor,
                r.vat_minor,
                r.gross_minor,
            );
            // Same estimate-vs-assertion split as the AR side, and it
            // matters far more here: `ap_sync` stamps
            // `payment_deadline: None` on EVERY NAV-synced payable, which
            // is the dominant AP population. A counter that took the
            // imputed bucket would report the whole payables book as past
            // deadline.
            if deadline_d.is_some() && !matches!(bucket, AgingBucket::Current) {
                ap.payable_past_deadline = ap.payable_past_deadline.saturating_add(1);
            }
        }
    }
    ap
}

fn mean(xs: &[f64]) -> Option<f64> {
    if xs.is_empty() {
        return None;
    }
    let sum: f64 = xs.iter().copied().sum();
    Some(sum / xs.len() as f64)
}

fn top_n_from_map(map: HashMap<(String, String), (i64, u64)>, n: usize) -> Vec<TopEntry> {
    let mut v: Vec<TopEntry> = map
        .into_iter()
        .map(|((label, currency), (gross, count))| TopEntry {
            label,
            currency,
            gross_minor: gross,
            count,
        })
        .collect();
    // DESC sort by gross_minor (highest first).
    v.sort_by_key(|t| std::cmp::Reverse(t.gross_minor));
    v.truncate(n);
    v
}

#[allow(clippy::too_many_arguments)]
fn compute_delta(
    conn: &Connection,
    tenant: &str,
    traces: &HashMap<String, ReportTrace>,
    buyer_names: &HashMap<String, String>,
    basis: DateBasis,
    today: Date,
    prior_kind: Option<PeriodKind>,
    current_revenue: &CurrencyAggregate,
    current_expenses: &CurrencyAggregate,
) -> Result<Option<DeltaSet>> {
    let Some(prior_kind) = prior_kind else {
        return Ok(None);
    };
    let window = resolve_window(prior_kind)?;
    let groups = query_outgoing_groups(conn, window, basis)?;
    let restored = query_restored_rows(conn, tenant, window)?;
    let ap = query_ap_rows(conn, tenant, window, basis)?;
    let prior_outgoing = aggregate_outgoing(groups, traces, today, buyer_names);
    let mut prior_revenue = prior_outgoing.revenue;
    for r in &restored {
        let target = match r.currency.as_str() {
            "EUR" => &mut prior_revenue.eur,
            _ => &mut prior_revenue.huf,
        };
        target.gross_minor = target.gross_minor.saturating_add(r.gross_minor);
        target.net_minor = target.net_minor.saturating_add(r.net_minor);
        target.vat_minor = target.vat_minor.saturating_add(r.vat_minor);
        target.count = target.count.saturating_add(1);
    }
    let mut prior_expenses = CurrencyAggregate::default();
    for r in &ap {
        if r.local_status == "Irrelevant" {
            continue;
        }
        let target = match r.currency.as_str() {
            "EUR" => &mut prior_expenses.eur,
            _ => &mut prior_expenses.huf,
        };
        target.gross_minor = target.gross_minor.saturating_add(r.gross_minor);
        target.net_minor = target.net_minor.saturating_add(r.net_minor);
        target.vat_minor = target.vat_minor.saturating_add(r.vat_minor);
        target.count = target.count.saturating_add(1);
    }
    let revenue_pct_huf = pct_change(
        prior_revenue.huf.gross_minor,
        current_revenue.huf.gross_minor,
    );
    let revenue_pct_eur = pct_change(
        prior_revenue.eur.gross_minor,
        current_revenue.eur.gross_minor,
    );
    let expenses_pct_huf = pct_change(
        prior_expenses.huf.gross_minor,
        current_expenses.huf.gross_minor,
    );
    let expenses_pct_eur = pct_change(
        prior_expenses.eur.gross_minor,
        current_expenses.eur.gross_minor,
    );
    Ok(Some(DeltaSet {
        period_label: period_label(prior_kind),
        revenue: prior_revenue,
        expenses: prior_expenses,
        revenue_pct_huf,
        revenue_pct_eur,
        expenses_pct_huf,
        expenses_pct_eur,
    }))
}

fn pct_change(prior: i64, current: i64) -> Option<f64> {
    if prior == 0 {
        return None;
    }
    let delta = current as f64 - prior as f64;
    Some((delta / prior.unsigned_abs() as f64) * 100.0)
}

fn compute_annual_running(
    conn: &Connection,
    tenant: &str,
    traces: &HashMap<String, ReportTrace>,
    buyer_names: &HashMap<String, String>,
    basis: DateBasis,
    today: Date,
) -> Result<AnnualRunningPanel> {
    let year = today.year();
    let from = Date::from_calendar_date(year, Month::January, 1)?;
    let window = DateWindow {
        from: Some(from),
        to: Some(today),
    };
    let groups = query_outgoing_groups(conn, window, basis)?;
    let restored = query_restored_rows(conn, tenant, window)?;
    let outgoing = aggregate_outgoing(groups, traces, today, buyer_names);
    let mut revenue = outgoing.revenue;
    for r in &restored {
        let target = match r.currency.as_str() {
            "EUR" => &mut revenue.eur,
            _ => &mut revenue.huf,
        };
        target.gross_minor = target.gross_minor.saturating_add(r.gross_minor);
        target.net_minor = target.net_minor.saturating_add(r.net_minor);
        target.vat_minor = target.vat_minor.saturating_add(r.vat_minor);
        target.count = target.count.saturating_add(1);
    }
    Ok(AnnualRunningPanel { year, revenue })
}

/// Best-effort buyer-name map keyed by invoice id. Mirrors
/// `serve::list_invoices`'s side-store read posture (S215). Missing /
/// unreadable / blank side-store → no entry.
fn build_buyer_names_map(ledger: &Ledger) -> Result<HashMap<String, String>> {
    let entries = ledger
        .entries()
        .context("read audit ledger entries for buyer-name map")?;
    let mut out = HashMap::new();
    for entry in &entries {
        if entry.kind != EventKind::InvoiceDraftCreated {
            continue;
        }
        let Ok(parsed) =
            serde_json::from_slice::<audit_payloads::InvoiceDraftCreatedPayload>(&entry.payload)
        else {
            continue;
        };
        let Some(nav_xml_path) = parsed.nav_xml_path else {
            continue;
        };
        let xml_path = std::path::PathBuf::from(nav_xml_path);
        let input_path = crate::serve::sibling_input_json_path(&xml_path);
        let Ok(bytes) = std::fs::read(&input_path) else {
            continue;
        };
        let Ok(input_json) =
            serde_json::from_slice::<crate::issue_invoice::InvoiceInputJson>(&bytes)
        else {
            continue;
        };
        let trimmed = input_json.customer.name.trim();
        if !trimmed.is_empty() {
            out.insert(parsed.invoice_id, trimmed.to_string());
        }
    }
    Ok(out)
}

/// "Today" anchor for the report. Uses UTC date — the SPA renders the
/// raw ISO string back at the operator so a Budapest-vs-UTC mismatch
/// is visible rather than silently shifted.
pub fn today_local() -> Date {
    OffsetDateTime::now_utc().date()
}

// ──────────────────────────────────────────────────────────────────────
// Tests.
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use time::Duration;

    #[test]
    fn period_parse_month() {
        assert_eq!(parse_period("2026-06").unwrap(), PeriodKind::Month(2026, 6));
    }

    #[test]
    fn period_parse_quarter() {
        assert_eq!(
            parse_period("2026-Q2").unwrap(),
            PeriodKind::Quarter(2026, 2)
        );
    }

    #[test]
    fn period_parse_year() {
        assert_eq!(parse_period("2026").unwrap(), PeriodKind::Year(2026));
    }

    #[test]
    fn period_parse_all() {
        assert_eq!(parse_period("all").unwrap(), PeriodKind::All);
        assert_eq!(parse_period("All").unwrap(), PeriodKind::All);
    }

    #[test]
    fn period_parse_custom() {
        let kind = parse_period("2026-06-01..2026-06-30").unwrap();
        match kind {
            PeriodKind::Custom { from, to } => {
                assert_eq!(from.year(), 2026);
                assert_eq!(to.month() as u8, 6);
                assert_eq!(to.day(), 30);
            }
            _ => panic!("expected Custom"),
        }
    }

    #[test]
    fn period_parse_rejects_garbage() {
        assert!(parse_period("nope").is_err());
        assert!(parse_period("2026-13").is_err());
        assert!(parse_period("2026-Q5").is_err());
        assert!(parse_period("2026-06-30..2026-06-01").is_err());
    }

    #[test]
    fn resolve_month_window_inclusive() {
        let w = resolve_window(PeriodKind::Month(2026, 6)).unwrap();
        let fmt = format_description!("[year]-[month]-[day]");
        assert_eq!(w.from.unwrap().format(fmt).unwrap(), "2026-06-01");
        assert_eq!(w.to.unwrap().format(fmt).unwrap(), "2026-06-30");
    }

    #[test]
    fn resolve_year_window_full_year() {
        let w = resolve_window(PeriodKind::Year(2026)).unwrap();
        let fmt = format_description!("[year]-[month]-[day]");
        assert_eq!(w.from.unwrap().format(fmt).unwrap(), "2026-01-01");
        assert_eq!(w.to.unwrap().format(fmt).unwrap(), "2026-12-31");
    }

    #[test]
    fn previous_period_wraps_year() {
        assert_eq!(
            previous_period(PeriodKind::Month(2026, 1)).unwrap(),
            PeriodKind::Month(2025, 12)
        );
        assert_eq!(
            previous_period(PeriodKind::Quarter(2026, 1)).unwrap(),
            PeriodKind::Quarter(2025, 4)
        );
    }

    #[test]
    fn yoy_period_shifts_one_year() {
        assert_eq!(
            yoy_period(PeriodKind::Month(2026, 6)).unwrap(),
            PeriodKind::Month(2025, 6)
        );
        assert!(yoy_period(PeriodKind::All).is_none());
        let custom = PeriodKind::Custom {
            from: Date::from_calendar_date(2026, Month::June, 1).unwrap(),
            to: Date::from_calendar_date(2026, Month::June, 30).unwrap(),
        };
        assert!(yoy_period(custom).is_none());
    }

    #[test]
    fn aging_bucket_classification() {
        let today = Date::from_calendar_date(2026, Month::June, 1).unwrap();
        // Deadline 15 days in the future → Current.
        let future = Date::from_calendar_date(2026, Month::June, 16).unwrap();
        assert!(matches!(
            aging_bucket_for(today, future),
            AgingBucket::Current
        ));
        // Deadline 10 days ago → Days1To30.
        let recent = Date::from_calendar_date(2026, Month::May, 22).unwrap();
        assert!(matches!(
            aging_bucket_for(today, recent),
            AgingBucket::Days1To30
        ));
        // Deadline 100 days ago → Days90Plus.
        let stale = today.checked_sub(Duration::days(100)).unwrap();
        assert!(matches!(
            aging_bucket_for(today, stale),
            AgingBucket::Days90Plus
        ));
    }

    #[test]
    fn round_half_even_div_banker_rounding() {
        // 5 / 2 = 2.5 → round-half-even → 2 (even).
        assert_eq!(round_half_even_div(5, 2), 2);
        // 7 / 2 = 3.5 → round-half-even → 4 (even).
        assert_eq!(round_half_even_div(7, 2), 4);
        // Exact divisions don't round.
        assert_eq!(round_half_even_div(10, 2), 5);
        // Negative numerator preserves direction.
        assert_eq!(round_half_even_div(-7, 2), -4);
    }

    #[test]
    fn pct_change_handles_zero_prior() {
        assert_eq!(pct_change(0, 100), None);
        assert_eq!(pct_change(100, 200).unwrap(), 100.0);
        assert_eq!(pct_change(200, 100).unwrap(), -50.0);
    }

    #[test]
    fn date_basis_round_trip() {
        for basis in [DateBasis::Teljesites, DateBasis::Issued] {
            let s = basis.as_wire_str();
            assert_eq!(DateBasis::parse(s).unwrap(), basis);
        }
        assert!(DateBasis::parse("nope").is_none());
    }

    #[test]
    fn mean_empty_returns_none() {
        let none: Vec<f64> = vec![];
        assert!(mean(&none).is_none());
        assert_eq!(mean(&[1.0, 2.0, 3.0]).unwrap(), 2.0);
    }

    /// S262 / PR-251 — `query_eur_huf_equivalent` sums the snapshot-rate
    /// HUF equivalent ONLY over EUR invoices in the window. HUF invoices
    /// (NULL `huf_equivalent_total`) must contribute nothing — the
    /// currency split is the only consumer and double-counting HUF there
    /// would inflate the EUR bar segment. Also asserts the date window
    /// excludes out-of-period rows and the all-bounds (`All`) path works.
    #[test]
    fn eur_huf_equivalent_sums_only_eur_in_window() {
        let conn = Connection::open_in_memory().expect("in-memory duckdb");
        // Minimal `invoice` shape — only the columns the query reads.
        conn.execute_batch(
            "CREATE TABLE invoice (
                 id VARCHAR,
                 currency VARCHAR,
                 issue_date VARCHAR,
                 delivery_date DATE,
                 huf_equivalent_total DECIMAL(18,0)
             );
             INSERT INTO invoice VALUES
               ('eur-in',  'EUR', '2026-06-10', NULL, 190000),
               ('eur-in2', 'EUR', '2026-06-20', NULL, 10000),
               ('huf-in',  'HUF', '2026-06-12', NULL, NULL),
               ('eur-out', 'EUR', '2026-05-31', NULL, 999999);",
        )
        .expect("seed invoice rows");

        let window = DateWindow {
            from: Some(Date::from_calendar_date(2026, Month::June, 1).unwrap()),
            to: Some(Date::from_calendar_date(2026, Month::June, 30).unwrap()),
        };
        let got = query_eur_huf_equivalent(&conn, window, DateBasis::Teljesites).unwrap();
        assert_eq!(
            got, 200_000,
            "only the two in-window EUR rows (190000 + 10000) contribute; HUF NULL and the May EUR row are excluded"
        );

        // `All` (unbounded) window includes the May EUR row too.
        let got_all =
            query_eur_huf_equivalent(&conn, DateWindow::unbounded(), DateBasis::Teljesites)
                .unwrap();
        assert_eq!(got_all, 1_199_999, "unbounded window sums every EUR row");
    }

    // ──────────────────────────────────────────────────────────────
    // AR / DSO correctness pins (port of the Portable PROD_v2.33.7
    // financial-dashboard fix).
    //
    // These are permanent regression tests, and they are deliberately
    // BIDIRECTIONAL: one pins that a landed storno leaves NO receivable
    // (the leak), the other pins that an aborted / never-submitted
    // storno leaves the base receivable INTACT (the under-count). A fix
    // that only satisfies one of them is wrong.
    // ──────────────────────────────────────────────────────────────

    use aberp_audit_ledger::{Actor, EntryHash, EntryId, Sequence};

    /// Synthetic audit entry — the hash/actor fields are placeholders;
    /// the walk never reads them. Mirrors `audit_summary::tests::entry`.
    fn pin_entry(kind: EventKind, payload: serde_json::Value) -> Entry {
        pin_entry_raw(kind, serde_json::to_vec(&payload).unwrap())
    }

    /// Same, but with the payload bytes given VERBATIM — the only way to
    /// build an entry whose payload is malformed or not JSON at all, which
    /// is exactly what the unparseable-payload pins need.
    fn pin_entry_raw(kind: EventKind, payload: Vec<u8>) -> Entry {
        Entry {
            id: EntryId::new(),
            seq: Sequence::FIRST,
            prev_hash: EntryHash::from_bytes([0u8; 32]),
            time_wall: OffsetDateTime::UNIX_EPOCH,
            time_mono: 0,
            actor: Actor {
                session_id: "sess".to_string(),
                user_id: "ervin".to_string(),
                capabilities: Default::default(),
            },
            binary_hash: BinaryHash::from_bytes([0u8; 32]),
            tenant_id: TenantId::new("t").unwrap(),
            kind,
            payload,
            idempotency_key: None,
            entry_hash: EntryHash::from_bytes([0u8; 32]),
            session_id: None,
            session_pubkey: None,
            event_sig: None,
        }
    }

    fn pin_ack(invoice_id: &str, ack_status: &str) -> Entry {
        pin_entry(
            EventKind::InvoiceAckStatus,
            serde_json::json!({
                "invoice_id": invoice_id,
                "transaction_id": "tx",
                "ack_status": ack_status,
                "response_xml": [],
            }),
        )
    }

    fn pin_draft(invoice_id: &str) -> Entry {
        pin_entry(
            EventKind::InvoiceDraftCreated,
            serde_json::json!({ "invoice_id": invoice_id }),
        )
    }

    /// `InvoiceStornoIssued` — the chain link. Note this payload carries
    /// NO bare `invoice_id`, so it contributes only the chain link.
    fn pin_storno_link(base: &str, storno: &str) -> Entry {
        pin_entry(
            EventKind::InvoiceStornoIssued,
            serde_json::json!({
                "storno_invoice_id": storno,
                "storno_seq": 2u64,
                "storno_reservation_id": "res-2",
                "idempotency_key": "idem-2",
                "base_invoice_id": base,
                "base_sequence_number": 1u64,
                "modification_index": 1u32,
            }),
        )
    }

    fn pin_payment(invoice_id: &str, paid_at: &str, amount_minor: i64) -> Entry {
        pin_entry(
            EventKind::InvoicePaymentRecorded,
            serde_json::json!({
                "invoice_id": invoice_id,
                "idempotency_key": "idem-pay",
                "paid_at": paid_at,
                "amount_minor": amount_minor,
                "currency": "HUF",
                "method": "BankTransfer",
                "reference": null,
            }),
        )
    }

    fn pin_group(
        invoice_id: &str,
        issue_date: &str,
        deadline: &str,
        net: i64,
    ) -> OutgoingLineGroup {
        OutgoingLineGroup {
            invoice_id: invoice_id.to_string(),
            currency: "HUF".to_string(),
            issue_date: issue_date.to_string(),
            payment_deadline: Some(deadline.to_string()),
            vat_rate_basis_points: 2700,
            net_minor: net,
        }
    }

    /// PIN (a) — the storno LEAK. A cancelled invoice whose storno has
    /// landed (NAV-accepted) must contribute NOTHING to receivables: the
    /// base's `+` gross and the storno's `−` credit note have to net, not
    /// leave the `+` side standing. Receivables must equal the untouched
    /// control invoice alone.
    ///
    /// Mutation guard: drop `has_landed_storno` from the AR predicate and
    /// this reds (AR inflates by the cancelled invoice's full gross).
    #[test]
    fn landed_storno_pair_leaves_no_receivable() {
        let today = Date::from_calendar_date(2026, Month::June, 1).unwrap();
        let entries = vec![
            pin_ack("inv-base", "SAVED"),
            pin_ack("inv-ctrl", "SAVED"),
            pin_storno_link("inv-base", "inv-storno"),
            pin_ack("inv-storno", "SAVED"), // the storno LANDED
        ];
        let walk = walk_entries(&entries, DateWindow::unbounded());
        assert!(
            walk.traces["inv-base"].has_landed_storno,
            "a SAVED storno child must flag its base as landed"
        );
        // A ledger of exclusively well-formed payloads must produce an
        // EMPTY diagnostic. Without this, a diagnostic that fires on
        // everything would pass the malformed-payload pins below while
        // crying wolf on every real report.
        assert_eq!(
            walk.diagnostics,
            LedgerDiagnostics::default(),
            "no false positives: every payload here decodes, so nothing is unreadable"
        );

        let groups = vec![
            pin_group("inv-base", "2026-05-01", "2026-06-15", 100_000),
            pin_group("inv-storno", "2026-05-02", "2026-06-15", 100_000),
            pin_group("inv-ctrl", "2026-05-03", "2026-06-15", 50_000),
        ];
        let agg = aggregate_outgoing(groups, &walk.traces, today, &HashMap::new());

        // Revenue nets the pair to zero, leaving the control.
        assert_eq!(
            agg.revenue.huf.gross_minor, 63_500,
            "revenue nets base(+127000) against storno(-127000), leaving the control"
        );
        // Receivables MUST agree with revenue — this is the fix.
        assert_eq!(
            agg.receivables.huf.gross_minor, 63_500,
            "AR must equal the control alone; the cancelled invoice's gross must not leak"
        );
        assert_eq!(agg.receivables.huf.count, 1, "only the control is owed");
        // The same predicate gates aging + the forward cash-flow tiles.
        assert_eq!(agg.receivables_aging.current.gross_minor, 63_500);
        assert_eq!(agg.cashflow_forward.next_30.huf_minor, 63_500);
        assert_eq!(agg.cashflow_forward.next_60.huf_minor, 63_500);
        assert_eq!(agg.cashflow_forward.next_90.huf_minor, 63_500);
        assert_eq!(
            agg.outstanding_past_deadline_count, 0,
            "the control is not yet due"
        );
    }

    /// PIN (b) — the UNDER-COUNT guard. The storno flag must be resolved
    /// from whether the storno LANDED, never at storno-ISSUANCE time. A
    /// storno that was ABORTED by NAV — or drafted and never submitted —
    /// never negates the base in revenue, so the original invoice STILL
    /// STANDS as a real unpaid receivable and must remain in AR at its
    /// full amount.
    ///
    /// Mutation guard: flag the base at issuance time (i.e. set
    /// `has_landed_storno` inside the walk loop, as the naive fix does)
    /// and this reds — AR silently loses a genuinely owed invoice.
    #[test]
    fn aborted_or_unsubmitted_storno_leaves_base_in_receivables() {
        let today = Date::from_calendar_date(2026, Month::June, 1).unwrap();

        for (label, storno_entries) in [
            (
                "ABORTED by NAV",
                vec![pin_ack("inv-storno", "ABORTED")], // → Rejected
            ),
            (
                "drafted, never submitted",
                vec![pin_draft("inv-storno")], // → PendingDraft
            ),
        ] {
            let mut entries = vec![
                pin_ack("inv-base", "SAVED"),
                pin_storno_link("inv-base", "inv-storno"),
            ];
            entries.extend(storno_entries);
            let walk = walk_entries(&entries, DateWindow::unbounded());
            assert!(
                !walk.traces["inv-base"].has_landed_storno,
                "{label}: the storno never landed, so the base is not cancelled"
            );

            let groups = vec![
                pin_group("inv-base", "2026-05-01", "2026-06-15", 100_000),
                pin_group("inv-storno", "2026-05-02", "2026-06-15", 100_000),
            ];
            let agg = aggregate_outgoing(groups, &walk.traces, today, &HashMap::new());

            // Revenue keeps the base whole — the storno never negated it.
            assert_eq!(
                agg.revenue.huf.gross_minor, 127_000,
                "{label}: revenue keeps the base at full gross"
            );
            // AR must be COHERENT with that: the base is still owed.
            assert_eq!(
                agg.receivables.huf.gross_minor, 127_000,
                "{label}: the base is still a genuine unpaid receivable"
            );
            assert_eq!(agg.receivables.huf.count, 1, "{label}: one invoice owed");
        }
    }

    // ──────────────────────────────────────────────────────────────────
    // Unparseable-payload diagnostics.
    //
    // The walk used to swallow a payload that failed to decode (`if let
    // Ok(..)` with no else, `.ok()?`). The entry then contributed to
    // nothing and NOTHING said so — the operator read a clean number that
    // silently omitted a payment / an ack / a storno reversal. These pins
    // hold the two halves of the fix together: the drop is COUNTED and
    // ATTRIBUTED, and the valid rows' arithmetic is untouched.
    //
    // Mutation check: reverting any decode site to its silent form
    // (`if let Ok(parsed) = ..` in `merge`, `.ok()?` in the extractors)
    // drives `unparseable_entries` to 0 and reds every assertion below.
    // ──────────────────────────────────────────────────────────────────

    /// An invoice-scoped entry whose payload is well-formed JSON naming
    /// the invoice but MISSING the typed payload's required fields — so
    /// the entry reaches `merge` and the typed decode is what fails.
    fn pin_broken_payment(invoice_id: &str) -> Entry {
        pin_entry_raw(
            EventKind::InvoicePaymentRecorded,
            format!(r#"{{"invoice_id":"{invoice_id}","paid_at":"2026-07-20"}}"#).into_bytes(),
        )
    }

    /// `net == gross` groups (VAT basis points 0) so the aging arithmetic
    /// in these pins is legible at a glance, and an optional deadline so
    /// the NULL half of the aging hole is reachable.
    fn undated_group(
        invoice_id: &str,
        currency: &str,
        net: i64,
        deadline: Option<&str>,
    ) -> OutgoingLineGroup {
        OutgoingLineGroup {
            invoice_id: invoice_id.to_string(),
            currency: currency.to_string(),
            issue_date: "2026-07-01".to_string(),
            payment_deadline: deadline.map(|s| s.to_string()),
            vat_rate_basis_points: 0,
            net_minor: net,
        }
    }

    fn saved_trace() -> ReportTrace {
        ReportTrace {
            last_ack_status: Some("SAVED".to_string()),
            ..ReportTrace::default()
        }
    }

    /// The headline case. Two SAVED invoices, both with a recorded
    /// payment; one payment payload is malformed.
    ///
    /// Pre-fix the malformed payment simply evaporated: the invoice looked
    /// unpaid, sat in receivables, and no signal existed anywhere.
    /// Post-fix the NUMBERS ARE THE SAME — deliberately, this fix does not
    /// guess at an amount it could not read — but the run now says one
    /// entry was unreadable, so "receivables" can be presented as
    /// possibly-incomplete instead of authoritative.
    #[test]
    fn malformed_payment_payload_is_counted_not_swallowed() {
        let entries = vec![
            pin_ack("paid_ok", "SAVED"),
            pin_payment("paid_ok", "2026-07-20", 100_000),
            pin_ack("paid_broken", "SAVED"),
            pin_broken_payment("paid_broken"),
        ];
        let walk = walk_entries(&entries, DateWindow::unbounded());

        // (a) The drop is visible and attributable.
        assert_eq!(
            walk.diagnostics.unparseable_entries, 1,
            "the malformed payment must be counted; 0 means it vanished silently again"
        );
        assert_eq!(walk.diagnostics.unparseable_entry_ids.len(), 1);
        assert!(
            walk.diagnostics.unparseable_entry_ids[0].starts_with("aud_"),
            "the id must be the operator-facing prefixed form, got {:?}",
            walk.diagnostics.unparseable_entry_ids[0]
        );

        // (b) The valid row is untouched — same payment, same ack.
        assert_eq!(
            walk.traces["paid_ok"].payment_paid_at.as_deref(),
            Some("2026-07-20")
        );
        assert_eq!(walk.traces["paid_ok"].payment_amount_minor, Some(100_000));
        assert_eq!(
            walk.traces["paid_ok"].last_ack_status.as_deref(),
            Some("SAVED")
        );
        // The unreadable payment is NOT invented: the invoice stays as it
        // was before that entry — SAVED, no payment. Preserving current
        // numeric behaviour is the point; only the silence is fixed.
        assert_eq!(walk.traces["paid_broken"].payment_paid_at, None);
        assert_eq!(
            walk.traces["paid_broken"].last_ack_status.as_deref(),
            Some("SAVED"),
            "the malformed payment must not take the invoice's own valid ack down with it"
        );

        // (c) Aggregates over the valid rows are unchanged: `paid_ok` is
        // out of receivables because it is paid, `paid_broken` is in
        // because nothing readable said otherwise.
        let today = Date::from_calendar_date(2026, Month::August, 13).unwrap();
        let groups = vec![
            undated_group("paid_ok", "HUF", 100_000, Some("2026-07-14")),
            undated_group("paid_broken", "HUF", 250_000, Some("2026-07-14")),
        ];
        let agg = aggregate_outgoing(groups, &walk.traces, today, &HashMap::new());
        assert_eq!(
            agg.receivables.huf.gross_minor, 250_000,
            "only the unpaid one is owed; the valid payment still clears its invoice"
        );
        assert_eq!(agg.revenue.huf.gross_minor, 350_000);
    }

    /// The other two decode surfaces on the same walk, and the
    /// count-each-entry-once rule.
    ///
    /// A malformed ACK is the most damaging of the three: pre-fix it left
    /// `last_ack_status` at `None`, so a genuinely SAVED invoice
    /// classified as `PendingDraft` and dropped out of revenue AND
    /// VAT-collected entirely. A malformed STORNO chain link left the base
    /// uncancelled. A payload that is not JSON at all never even reached
    /// `merge`.
    #[test]
    fn malformed_ack_chain_link_and_non_json_payloads_are_each_counted_once() {
        let entries = vec![
            // One healthy invoice to prove the valid path survives all of it.
            pin_ack("healthy", "SAVED"),
            // Malformed ack: has `invoice_id`, missing `transaction_id` +
            // `response_xml`.
            pin_entry_raw(
                EventKind::InvoiceAckStatus,
                br#"{"invoice_id":"bad_ack","ack_status":"SAVED"}"#.to_vec(),
            ),
            // Malformed storno chain link: no `invoice_id` key at all
            // (normal for this kind — `Ok(None)` from the id extractor),
            // and the typed chain-link decode fails on the missing fields.
            pin_entry_raw(
                EventKind::InvoiceStornoIssued,
                br#"{"base_invoice_id":"some_base"}"#.to_vec(),
            ),
            // Not JSON at all, on a kind that BOTH decode paths look at.
            // Must still be one entry, not two: the count answers "how
            // many entries could not be read", not "how many decode
            // attempts failed".
            pin_entry_raw(EventKind::InvoiceStornoIssued, b"<<< not json >>>".to_vec()),
        ];
        let walk = walk_entries(&entries, DateWindow::unbounded());

        assert_eq!(
            walk.diagnostics.unparseable_entries, 3,
            "one per unreadable ENTRY — the non-JSON storno fails both decodes but counts once"
        );
        assert_eq!(walk.diagnostics.unparseable_entry_ids.len(), 3);

        // The malformed ack did not fabricate a trace state, and — the
        // point of the fix — did not fabricate silence either.
        assert!(
            walk.traces
                .get("bad_ack")
                .is_none_or(|t| t.last_ack_status.is_none()),
            "an unreadable ack must not be guessed at"
        );
        // The healthy invoice is completely unaffected by its corrupt
        // neighbours.
        assert_eq!(
            walk.traces["healthy"].last_ack_status.as_deref(),
            Some("SAVED")
        );
        assert!(matches!(
            walk.traces["healthy"].classify(),
            CountedKind::Counted {
                is_storno_self: false
            }
        ));
        // The unreadable chain link contributed no hygiene count — pre-fix
        // that was ALSO true, and equally silent. Now it is accounted for.
        assert_eq!(walk.storno_links_in_period, 0);
    }

    /// The id list is capped so a systemically corrupt ledger cannot
    /// balloon the report payload, but the COUNT stays exact — the
    /// difference is what tells a caller "and more".
    #[test]
    fn unparseable_entry_ids_are_capped_while_the_count_stays_exact() {
        let corrupt = MAX_UNPARSEABLE_ENTRY_IDS + 7;
        let entries: Vec<Entry> = (0..corrupt)
            .map(|_| pin_entry_raw(EventKind::InvoicePaymentRecorded, b"not json".to_vec()))
            .collect();

        let walk = walk_entries(&entries, DateWindow::unbounded());

        assert_eq!(walk.diagnostics.unparseable_entries, corrupt as u64);
        assert_eq!(
            walk.diagnostics.unparseable_entry_ids.len(),
            MAX_UNPARSEABLE_ENTRY_IDS
        );
    }

    // ──────────────────────────────────────────────────────────────────
    // Aging coherence — `sum(buckets) == total`.
    //
    // Both aging sites used to read `if let Ok(d) = parse_iso_date(..)`
    // with no `else`, nested in `if let Some(deadline)`. An invoice whose
    // `payment_deadline` was missing or malformed fell out of every bucket
    // — but the lines directly above had already added it to the
    // receivables / payables TOTAL. The panel's breakdown summed to less
    // than the panel's own headline and nothing said so. Same silent-drop
    // class as the audit entries above, different code path.
    //
    // MUTATION PROOF for the pins below: restore the old shape at either
    // site, e.g. in `aggregate_outgoing`
    //
    //     if let Some(s) = deadline {
    //         if let Ok(d) = parse_iso_date(s) { ..old body.. }
    //     }
    //
    // and the `sum == total` assertion is the first thing that reds
    // (buckets short by exactly the dropped invoice), followed by the
    // diagnostics assertions.
    // ──────────────────────────────────────────────────────────────────

    /// Sum an aging panel's five buckets — the figure that must equal the
    /// receivables / payables headline the panel sits under.
    fn aging_totals(panel: &AgingPanel) -> (i64, i64, i64, u64) {
        [
            &panel.current,
            &panel.days_1_30,
            &panel.days_31_60,
            &panel.days_61_90,
            &panel.days_90_plus,
        ]
        .into_iter()
        .fold((0, 0, 0, 0), |acc, b| {
            (
                acc.0 + b.net_minor,
                acc.1 + b.vat_minor,
                acc.2 + b.gross_minor,
                acc.3 + b.count,
            )
        })
    }

    /// **RECEIVABLES.** An unparseable `payment_deadline` must not fall
    /// out of the aging panel while still counting toward its total.
    ///
    /// (a) `sum(buckets) == receivables total` — the core invariant.
    /// (b) the invoice is counted AND named in `aging_undated`.
    /// (c) the two valid-deadline neighbours do not move: `2026-08-23` is
    ///     still `current` and `2026-07-14` still `days_1_30`, to the
    ///     minor unit. A fix that re-bucketed healthy invoices to make the
    ///     sum work would be a worse defect than the one it replaced.
    #[test]
    fn receivable_with_an_unparseable_deadline_stays_in_its_aging_panel() {
        let today = Date::from_calendar_date(2026, Month::August, 13).unwrap();
        let traces: HashMap<String, ReportTrace> = HashMap::from([
            ("future".into(), saved_trace()),
            ("overdue".into(), saved_trace()),
            ("garbled".into(), saved_trace()),
        ]);
        let groups = vec![
            undated_group("future", "HUF", 20_000, Some("2026-08-23")),
            undated_group("overdue", "HUF", 43_180, Some("2026-07-14")),
            // Real-world shape of a broken deadline: a swapped-format date
            // the ISO parser rejects.
            undated_group("garbled", "HUF", 777_000, Some("14/07/2026")),
        ];
        let agg = aggregate_outgoing(groups, &traces, today, &HashMap::new());

        // (a) The invariant. All three are receivable, so all three must
        //     be somewhere in the panel.
        assert_eq!(agg.receivables.huf.gross_minor, 840_180);
        assert_eq!(agg.receivables.huf.count, 3);
        let (net, vat, gross, count) = aging_totals(&agg.receivables_aging);
        assert_eq!(
            gross, agg.receivables.huf.gross_minor,
            "the aging buckets must sum to the receivables total the panel is showing; \
             short by 777 000 means the garbled invoice was silently dropped again"
        );
        assert_eq!(net, agg.receivables.huf.net_minor);
        assert_eq!(vat, agg.receivables.huf.vat_minor);
        assert_eq!(count, agg.receivables.huf.count);

        // (b) Surfaced, not just absorbed. The bucket is an imputation and
        //     the operator has to be able to learn that, by id.
        assert_eq!(agg.aging_undated.count, 1);
        assert_eq!(agg.aging_undated.ids, vec!["garbled".to_string()]);

        // (c) Valid deadlines are placed EXACTLY as before.
        assert_eq!(agg.receivables_aging.current.gross_minor, 20_000);
        assert_eq!(agg.receivables_aging.current.count, 1);
        assert_eq!(agg.receivables_aging.days_1_30.gross_minor, 43_180);
        assert_eq!(agg.receivables_aging.days_1_30.count, 1);
        assert_eq!(agg.receivables_aging.days_31_60, AmountAggregate::default());
        assert_eq!(agg.receivables_aging.days_61_90, AmountAggregate::default());
        // The undated one lands in the MOST-overdue bucket: a deadline
        // nobody can read cannot be assumed not-yet-due.
        assert_eq!(agg.receivables_aging.days_90_plus.gross_minor, 777_000);
        assert_eq!(agg.receivables_aging.days_90_plus.count, 1);

        // (d) The imputed bucket does NOT become a lateness claim. Only
        //     `overdue` — the one invoice whose deadline we could actually
        //     read and which has actually passed — is past deadline. 2
        //     here would mean the estimate leaked into an assertion: the
        //     hygiene tile would claim an invoice is late when nothing
        //     knows whether it is, and its click-through list (which
        //     excludes null deadlines) would come back one row short.
        assert_eq!(
            agg.outstanding_past_deadline_count, 1,
            "an unreadable deadline is UNKNOWN lateness, not lateness"
        );
        // Still not promised as incoming cash — only `current` feeds the
        // forward projection, and it is not current.
        assert_eq!(agg.cashflow_forward.next_90.huf_minor, 20_000);
    }

    /// The `None` half of the same hole: a receivable with NO deadline at
    /// all took the identical silent path out of the panel (the outer `if
    /// let Some(..)`), and is now placed and disclosed the same way.
    #[test]
    fn receivable_with_no_deadline_at_all_is_placed_and_disclosed() {
        let today = Date::from_calendar_date(2026, Month::August, 13).unwrap();
        let traces: HashMap<String, ReportTrace> =
            HashMap::from([("dateless".into(), saved_trace())]);
        let groups = vec![undated_group("dateless", "EUR", 51_500, None)];
        let agg = aggregate_outgoing(groups, &traces, today, &HashMap::new());

        let (_, _, gross, count) = aging_totals(&agg.receivables_aging);
        assert_eq!(
            gross, agg.receivables.eur.gross_minor,
            "a receivable with no deadline is still a receivable — it must be in a bucket"
        );
        assert_eq!(count, agg.receivables.eur.count);
        assert_eq!(agg.receivables_aging.days_90_plus.gross_minor, 51_500);
        assert_eq!(agg.aging_undated.count, 1);
        assert_eq!(agg.aging_undated.ids, vec!["dateless".to_string()]);
        assert_eq!(
            agg.outstanding_past_deadline_count, 0,
            "no deadline means nothing is known about lateness — the hygiene counter \
             must not inherit the imputed 90+ bucket"
        );
    }

    /// A PAID invoice with a garbled deadline is not a receivable and must
    /// stay out of BOTH the total and the panel — the fix must not turn
    /// "unreadable date" into a reason to resurrect settled invoices.
    #[test]
    fn paid_invoice_with_an_unparseable_deadline_enters_neither_total_nor_buckets() {
        let today = Date::from_calendar_date(2026, Month::August, 13).unwrap();
        let traces: HashMap<String, ReportTrace> = HashMap::from([(
            "paid".into(),
            ReportTrace {
                payment_paid_at: Some("2026-07-20".into()),
                ..saved_trace()
            },
        )]);
        let groups = vec![undated_group("paid", "HUF", 100_000, Some("garbage"))];
        let agg = aggregate_outgoing(groups, &traces, today, &HashMap::new());

        assert_eq!(agg.receivables.huf, AmountAggregate::default());
        assert_eq!(aging_totals(&agg.receivables_aging), (0, 0, 0, 0));
        assert_eq!(
            agg.aging_undated.count, 0,
            "a settled invoice's deadline is nobody's problem — do not cry wolf on it"
        );
    }

    fn ap_row(id: &str, deadline: Option<&str>, gross: i64, status: &str) -> ApRow {
        ApRow {
            id: id.into(),
            supplier_name: "Beszállító Kft.".into(),
            payment_deadline: deadline.map(|s| s.into()),
            net_minor: gross,
            vat_minor: 0,
            gross_minor: gross,
            currency: "HUF".into(),
            local_status: status.into(),
        }
    }

    /// **PAYABLES.** The mirror of the receivables pin, on the second
    /// site. Same invariant, same disclosure, same don't-move-the-healthy
    /// -rows requirement.
    #[test]
    fn payable_with_an_unparseable_deadline_stays_in_its_aging_panel() {
        let today = Date::from_calendar_date(2026, Month::August, 13).unwrap();
        let rows = vec![
            ap_row("ap_future", Some("2026-08-23"), 20_000, "Outstanding"),
            ap_row("ap_overdue", Some("2026-06-04"), 43_180, "Outstanding"),
            ap_row("ap_garbled", Some("2026-13-45"), 777_000, "Outstanding"),
            ap_row("ap_dateless", None, 5_000, "Outstanding"),
            // Settled + operator-declared-irrelevant rows are not payable
            // and must not be dragged in by the fix.
            ap_row("ap_settled", Some("nonsense"), 900_000, "Settled"),
            ap_row("ap_skip", None, 800_000, "Irrelevant"),
        ];
        let ap = aggregate_ap(&rows, today);

        // (a) The invariant.
        assert_eq!(ap.payables.huf.gross_minor, 845_180);
        assert_eq!(ap.payables.huf.count, 4);
        let (net, vat, gross, count) = aging_totals(&ap.payables_aging);
        assert_eq!(
            gross, ap.payables.huf.gross_minor,
            "the payables buckets must sum to the payables total the panel is showing"
        );
        assert_eq!(net, ap.payables.huf.net_minor);
        assert_eq!(vat, ap.payables.huf.vat_minor);
        assert_eq!(count, ap.payables.huf.count);

        // (b) Both undated rows disclosed, by id, and nothing else.
        assert_eq!(ap.aging_undated.count, 2);
        assert_eq!(
            ap.aging_undated.ids,
            vec!["ap_garbled".to_string(), "ap_dateless".to_string()]
        );

        // (c) Valid deadlines unmoved.
        assert_eq!(ap.payables_aging.current.gross_minor, 20_000);
        assert_eq!(ap.payables_aging.days_61_90.gross_minor, 43_180);
        assert_eq!(ap.payables_aging.days_1_30, AmountAggregate::default());
        assert_eq!(ap.payables_aging.days_31_60, AmountAggregate::default());
        assert_eq!(ap.payables_aging.days_90_plus.gross_minor, 782_000);
        assert_eq!(ap.payables_aging.days_90_plus.count, 2);

        // (d) Only `ap_overdue` is ASSERTED late. This is the load-bearing
        //     one on the AP side: `ap_sync` stamps `payment_deadline:
        //     None` on every NAV-synced payable, so a counter that
        //     inherited the imputed 90+ bucket would report essentially
        //     the whole payables book as past deadline — and the hygiene
        //     drill-down, which excludes null deadlines, would come back
        //     nearly empty against it.
        assert_eq!(
            ap.payable_past_deadline, 1,
            "undated payables are not late payables; NAV sync makes this the common case"
        );

        // The non-payable rows still reach expenses, and only there.
        assert_eq!(ap.expenses.huf.gross_minor, 1_745_180);
        assert_eq!(ap.expenses.huf.count, 5);
    }

    /// **The production shape.** `ap_sync::digest_to_ingestion_input`
    /// records `payment_deadline: None` on EVERY NAV-synced payable, and
    /// NAV sync is how the payables book is populated — so "all rows
    /// undated" is not an edge case, it is Tuesday.
    ///
    /// Two things must both hold on that book, and they pull in opposite
    /// directions:
    ///   * the aging panel still adds up (that is the whole fix), so all
    ///     of it lands in 90+ and is disclosed as imputed;
    ///   * NONE of it is asserted late. `payable_past_deadline_count`
    ///     drives a clickable hygiene tile whose drill-down filters on a
    ///     real deadline strictly before today; a counter of 3 over a list
    ///     of 0 is the tile-vs-list incoherence this branch exists to
    ///     remove, at full book scale.
    #[test]
    fn an_all_nav_synced_payables_book_is_fully_aged_but_never_asserted_late() {
        let today = Date::from_calendar_date(2026, Month::August, 13).unwrap();
        let rows = vec![
            ap_row("ap_nav_1", None, 120_000, "Outstanding"),
            ap_row("ap_nav_2", None, 340_000, "Outstanding"),
            ap_row("ap_nav_3", None, 55_000, "Outstanding"),
        ];
        let ap = aggregate_ap(&rows, today);

        assert_eq!(ap.payables.huf.gross_minor, 515_000);
        let (_, _, gross, count) = aging_totals(&ap.payables_aging);
        assert_eq!(gross, ap.payables.huf.gross_minor);
        assert_eq!(count, ap.payables.huf.count);
        assert_eq!(ap.payables_aging.days_90_plus.count, 3);
        assert_eq!(ap.aging_undated.count, 3);
        assert_eq!(
            ap.payable_past_deadline, 0,
            "NAV gives us no deadlines; reporting the entire payables book as past \
             deadline would be a fabrication, and the hygiene drill-down would show none \
             of it"
        );
    }

    /// PIN (c) — DSO must anchor on the invoice's ISSUE date, not the
    /// basis/teljesites date (`COALESCE(delivery_date, issue_date)`).
    /// A PREPAYMENT — paid before fulfillment — otherwise produces a
    /// NEGATIVE days-sales-outstanding sample and drags the mean below
    /// zero, which is nonsense for a credit-to-cash metric.
    ///
    /// Drives the real SQL so the anchor column itself is pinned:
    /// mutation guard — swap the DSO anchor back to the basis column and
    /// the prepayment sample becomes −10 and this reds.
    #[test]
    fn dso_anchors_on_issue_date_so_prepayment_is_never_negative() {
        let conn = Connection::open_in_memory().expect("in-memory duckdb");
        conn.execute_batch(
            "CREATE TABLE invoice (
                 id VARCHAR,
                 currency VARCHAR,
                 issue_date VARCHAR,
                 delivery_date DATE,
                 payment_deadline DATE
             );
             CREATE TABLE invoice_line (
                 invoice_id VARCHAR,
                 vat_rate_basis_points INTEGER,
                 quantity DECIMAL(18,6),
                 unit_price BIGINT
             );
             -- PRODUCTION-SHAPED issue_date: a VARCHAR holding RFC3339,
             -- exactly what duckdb_store.rs writes
             -- (`draft.issue_date.format(&Rfc3339)`). Seeding date-only
             -- here would be unfaithful to the write path and would let
             -- an anchor that cannot tolerate a time component pass.
             --
             -- Prepayment: fulfilled 2026-06-25, but issued 2026-06-10
             -- and paid 2026-06-15 — five days after ISSUE, ten days
             -- BEFORE fulfillment.
             INSERT INTO invoice VALUES
               ('inv-prepay',   'HUF', '2026-06-10T09:00:00Z', DATE '2026-06-25', DATE '2026-07-10'),
               ('inv-ordinary', 'HUF', '2026-06-01T17:45:31Z', NULL,              DATE '2026-07-01');
             INSERT INTO invoice_line VALUES
               ('inv-prepay',   2700, 1.0, 100000),
               ('inv-ordinary', 2700, 1.0, 100000);",
        )
        .expect("seed invoice + line rows");

        let window = DateWindow {
            from: Some(Date::from_calendar_date(2026, Month::June, 1).unwrap()),
            to: Some(Date::from_calendar_date(2026, Month::June, 30).unwrap()),
        };
        let groups =
            query_outgoing_groups(&conn, window, DateBasis::Teljesites).expect("outgoing groups");
        assert_eq!(groups.len(), 2, "both invoices fall in the window");
        // The anchor must arrive NORMALISED — an RFC3339 time component
        // would make `parse_iso_date` reject it and silently drop every
        // DSO sample (tile renders "— (n=0)").
        for g in &groups {
            assert_eq!(
                g.issue_date.len(),
                10,
                "issue_date must be normalised to YYYY-MM-DD, got `{}`",
                g.issue_date
            );
            assert!(
                parse_iso_date(&g.issue_date).is_ok(),
                "the DSO anchor must be parseable by parse_iso_date, got `{}`",
                g.issue_date
            );
        }

        let entries = vec![
            pin_ack("inv-prepay", "SAVED"),
            pin_payment("inv-prepay", "2026-06-15", 127_000),
            pin_ack("inv-ordinary", "SAVED"),
            pin_payment("inv-ordinary", "2026-06-21", 127_000),
        ];
        let walk = walk_entries(&entries, DateWindow::unbounded());
        let today = Date::from_calendar_date(2026, Month::June, 30).unwrap();
        let agg = aggregate_outgoing(groups, &walk.traces, today, &HashMap::new());

        let mut samples = agg.dso_huf_samples.clone();
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert!(
            samples.iter().all(|d| *d >= 0.0),
            "no DSO sample may be negative (prepayments must not go below zero): {samples:?}"
        );
        assert_eq!(
            samples,
            vec![5.0, 20.0],
            "prepayment = paid 2026-06-15 − issued 2026-06-10 = 5d (NOT −10d off the \
             2026-06-25 fulfillment date); ordinary = 2026-06-21 − 2026-06-01 = 20d"
        );
        assert_eq!(mean(&agg.dso_huf_samples).unwrap(), 12.5);
    }

    /// PIN (c2) — the anchor projection is TYPE-tolerant. Against a
    /// legacy pre-PR-73 DATE-typed `invoice.issue_date`, reading the
    /// column bare as a `String` returns `InvalidColumnType`; in
    /// `row_to_outgoing` that `?` would fail the WHOLE financial report,
    /// not merely DSO. The `CAST(... AS VARCHAR)` in the projection is
    /// what prevents that, and this pin holds both halves: bare read
    /// FAILS, projected read SUCCEEDS and normalises.
    ///
    /// Scope note: this pins the PROJECTION alone. The end-to-end gap it
    /// used to flag — the basis/window column binder-erroring on a mixed
    /// VARCHAR/DATE COALESCE before this projection was ever reached — is
    /// now closed by [`WINDOW_DATE_EXPR`], and pinned by
    /// [`window_binds_and_filters_both_legacy_date_and_date_only_varchar`].
    #[test]
    fn dso_anchor_projection_tolerates_a_legacy_date_typed_column() {
        let conn = Connection::open_in_memory().expect("in-memory duckdb");
        conn.execute_batch(
            "CREATE TABLE invoice (id VARCHAR, issue_date DATE);
             INSERT INTO invoice VALUES ('inv-legacy', DATE '2026-06-05');",
        )
        .expect("seed legacy DATE-typed invoice");

        // Bare read of a DATE column as String — the crash the CAST averts.
        let bare: duckdb::Result<String> = conn
            .prepare("SELECT i.issue_date FROM invoice i")
            .unwrap()
            .query_row([], |r| r.get(0));
        assert!(
            bare.is_err(),
            "a bare DATE column read as String must fail — this is the \
             InvalidColumnType that would error the whole report"
        );

        // The projection the DSO anchor actually uses.
        let projected: String = conn
            .prepare("SELECT SUBSTR(CAST(i.issue_date AS VARCHAR), 1, 10) FROM invoice i")
            .unwrap()
            .query_row([], |r| r.get(0))
            .expect("the CAST+SUBSTR projection must read a DATE column as a String");
        assert_eq!(projected, "2026-06-05");
        assert!(parse_iso_date(&projected).is_ok());
    }

    // ──────────────────────────────────────────────────────────────
    // Date-WINDOW normalisation pins (port of the Portable PR-66
    // financial-window fix).
    //
    // `invoice.issue_date` is a VARCHAR holding RFC3339 (duckdb_store.rs
    // writes `draft.issue_date.format(&Rfc3339)`), and `delivery_date`
    // is a nullable DATE that stays NULL for pre-PR-84 rows and for
    // every non-SPA issuance. When the basis COALESCE falls through to
    // the raw RFC3339 `issue_date`, a string compare against the
    // date-only upper bound (`2026-06-30`) sorts `2026-06-30T21:15:00Z`
    // ABOVE it — so an invoice issued on the period's LAST DAY silently
    // dropped out of revenue, VAT, receivables and the currency split.
    //
    // [`WINDOW_DATE_EXPR`] is the fix: ONE normalised `YYYY-MM-DD`
    // expression shared by every window WHERE arm. These four pins are
    // permanent, and each reds under the plausible mutation — reverting
    // the const to the raw `COALESCE(CAST(i.delivery_date AS VARCHAR),
    // i.issue_date)` comparison.
    // ──────────────────────────────────────────────────────────────

    /// Seed the production-shaped `invoice` + `invoice_line` pair used by
    /// the window pins: `issue_date` a VARCHAR holding RFC3339,
    /// `delivery_date` a nullable DATE. Every row is 100_000 net @ 27%.
    fn seed_window_fixture(conn: &Connection) {
        conn.execute_batch(
            "CREATE TABLE invoice (
                 id VARCHAR,
                 currency VARCHAR,
                 issue_date VARCHAR,
                 delivery_date DATE,
                 payment_deadline DATE,
                 huf_equivalent_total DECIMAL(18,0)
             );
             CREATE TABLE invoice_line (
                 invoice_id VARCHAR,
                 vat_rate_basis_points INTEGER,
                 quantity DECIMAL(18,6),
                 unit_price BIGINT
             );
             -- `inv-lastday` is the whole point: issued on the period's
             -- LAST DAY, late in the day, with NO delivery_date — so the
             -- basis COALESCE falls through to the raw RFC3339 string.
             INSERT INTO invoice VALUES
               ('inv-lastday', 'HUF', '2026-06-30T21:15:00Z', NULL, DATE '2026-07-31', NULL),
               ('inv-mid',     'HUF', '2026-06-10T09:00:00Z', NULL, DATE '2026-07-10', NULL),
               ('inv-next',    'HUF', '2026-07-01T08:00:00Z', NULL, DATE '2026-08-01', NULL);
             INSERT INTO invoice_line VALUES
               ('inv-lastday', 2700, 1.0, 100000),
               ('inv-mid',     2700, 1.0, 100000),
               ('inv-next',    2700, 1.0, 100000);",
        )
        .expect("seed window fixture");
    }

    fn june_2026() -> DateWindow {
        DateWindow {
            from: Some(Date::from_calendar_date(2026, Month::June, 1).unwrap()),
            to: Some(Date::from_calendar_date(2026, Month::June, 30).unwrap()),
        }
    }

    /// PIN (e) — an invoice issued on the period's LAST DAY, stamped
    /// RFC3339 and carrying no `delivery_date`, must stay inside the
    /// window: present in revenue, in VAT collected, and in receivables.
    /// The July invoice must still be excluded — the fix RECOVERS the
    /// boundary row, it does not widen the period.
    ///
    /// Mutation guard: revert [`WINDOW_DATE_EXPR`] to the raw
    /// `COALESCE(CAST(i.delivery_date AS VARCHAR), i.issue_date)` and
    /// `inv-lastday` vanishes from all three — this reds.
    #[test]
    fn last_day_rfc3339_invoice_stays_in_revenue_vat_and_receivables() {
        let conn = Connection::open_in_memory().expect("in-memory duckdb");
        seed_window_fixture(&conn);

        let groups = query_outgoing_groups(&conn, june_2026(), DateBasis::Teljesites)
            .expect("outgoing groups");
        let mut ids: Vec<&str> = groups.iter().map(|g| g.invoice_id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(
            ids,
            vec!["inv-lastday", "inv-mid"],
            "the last-day RFC3339 invoice belongs to June; the July invoice does not"
        );

        let entries = vec![pin_ack("inv-lastday", "SAVED"), pin_ack("inv-mid", "SAVED")];
        let walk = walk_entries(&entries, DateWindow::unbounded());
        let today = Date::from_calendar_date(2026, Month::June, 30).unwrap();
        let agg = aggregate_outgoing(groups, &walk.traces, today, &HashMap::new());

        // Two invoices × (100_000 net + 27_000 VAT).
        assert_eq!(agg.revenue.huf.count, 2, "both June invoices are revenue");
        assert_eq!(agg.revenue.huf.net_minor, 200_000, "revenue net");
        assert_eq!(agg.revenue.huf.gross_minor, 254_000, "revenue gross");
        assert_eq!(
            agg.vat_collected.huf.vat_minor, 54_000,
            "VAT collected must carry the last-day invoice's 27_000 too"
        );
        assert_eq!(
            agg.receivables.huf.count, 2,
            "neither June invoice is paid — both are receivables"
        );
        assert_eq!(
            agg.receivables.huf.gross_minor, 254_000,
            "receivables must include the last-day invoice's gross"
        );
    }

    /// PIN (f) — the currency split ([`query_eur_huf_equivalent`]) shares
    /// the SAME window predicate, so it must keep the last-day EUR
    /// invoice too. If it did not, the EUR bar segment would disagree
    /// with the revenue figure it is supposed to split.
    ///
    /// Mutation guard: revert [`WINDOW_DATE_EXPR`] to the raw comparison
    /// and the 500_000 last-day contribution disappears — this reds.
    #[test]
    fn currency_split_keeps_the_last_day_rfc3339_invoice() {
        let conn = Connection::open_in_memory().expect("in-memory duckdb");
        conn.execute_batch(
            "CREATE TABLE invoice (
                 id VARCHAR,
                 currency VARCHAR,
                 issue_date VARCHAR,
                 delivery_date DATE,
                 huf_equivalent_total DECIMAL(18,0)
             );
             INSERT INTO invoice VALUES
               ('eur-lastday', 'EUR', '2026-06-30T21:15:00Z', NULL, 500000),
               ('eur-mid',     'EUR', '2026-06-10T09:00:00Z', NULL, 190000),
               ('eur-next',    'EUR', '2026-07-01T08:00:00Z', NULL, 999999);",
        )
        .expect("seed EUR rows");

        let got = query_eur_huf_equivalent(&conn, june_2026(), DateBasis::Teljesites)
            .expect("eur huf equivalent");
        assert_eq!(
            got, 690_000,
            "the last-day EUR invoice (500_000) must be in the split alongside \
             the mid-month one (190_000); the July row (999_999) must not"
        );
    }

    /// PIN (g) — the window predicate BINDS and FILTERS against both
    /// column shapes the invoice table can present: a legacy pre-PR-73
    /// **DATE**-typed `issue_date`, and a date-only **VARCHAR**. The raw
    /// predicate handles neither cleanly — against a DATE-typed column
    /// `COALESCE(CAST(delivery_date AS VARCHAR), issue_date)` is a mixed
    /// VARCHAR/DATE COALESCE that binder-errors and fails the WHOLE
    /// financial report. The `CAST(i.issue_date AS VARCHAR)` inside
    /// [`WINDOW_DATE_EXPR`] is what makes both shapes servable, and this
    /// pin closes the gap PIN (c2)'s scope note used to flag.
    ///
    /// Mutation guard: revert [`WINDOW_DATE_EXPR`] to the raw comparison
    /// and the DATE-typed half errors outright — this reds.
    #[test]
    fn window_binds_and_filters_both_legacy_date_and_date_only_varchar() {
        // (label, issue_date column type, literal form)
        let cases: [(&str, &str, &str); 2] = [
            ("legacy DATE-typed issue_date", "DATE", "DATE '{}'"),
            ("date-only VARCHAR issue_date", "VARCHAR", "'{}'"),
        ];
        for (label, col_type, lit) in cases {
            let conn = Connection::open_in_memory().expect("in-memory duckdb");
            let render = |d: &str| lit.replace("{}", d);
            conn.execute_batch(&format!(
                "CREATE TABLE invoice (
                     id VARCHAR,
                     currency VARCHAR,
                     issue_date {col_type},
                     delivery_date DATE,
                     payment_deadline DATE
                 );
                 CREATE TABLE invoice_line (
                     invoice_id VARCHAR,
                     vat_rate_basis_points INTEGER,
                     quantity DECIMAL(18,6),
                     unit_price BIGINT
                 );
                 INSERT INTO invoice VALUES
                   ('inv-lastday', 'HUF', {lastday}, NULL, DATE '2026-07-31'),
                   ('inv-next',    'HUF', {next},    NULL, DATE '2026-08-01');
                 INSERT INTO invoice_line VALUES
                   ('inv-lastday', 2700, 1.0, 100000),
                   ('inv-next',    2700, 1.0, 100000);",
                lastday = render("2026-06-30"),
                next = render("2026-07-01"),
            ))
            .unwrap_or_else(|e| panic!("{label}: seed: {e}"));

            let groups = query_outgoing_groups(&conn, june_2026(), DateBasis::Teljesites)
                .unwrap_or_else(|e| {
                    panic!("{label}: the window query must bind against this column shape: {e}")
                });
            let ids: Vec<&str> = groups.iter().map(|g| g.invoice_id.as_str()).collect();
            assert_eq!(
                ids,
                vec!["inv-lastday"],
                "{label}: the last-day row is kept and the July row is filtered out"
            );
            assert!(
                parse_iso_date(&groups[0].issue_date).is_ok(),
                "{label}: the DSO anchor must still normalise, got `{}`",
                groups[0].issue_date
            );
        }
    }

    /// PIN (h) — the HALF-OPEN window arms share the one normalised
    /// predicate with the closed arm. Structural half: every arm
    /// [`build_date_where`] can emit references [`WINDOW_DATE_EXPR`] and
    /// never a bare `i.issue_date` comparison — that is what "one
    /// predicate for revenue, VAT, AR and DSO" MEANS, and it is what
    /// stops the two bounds from drifting apart again. Behavioural half:
    /// each arm still binds its slot and filters correctly, with the
    /// last-day RFC3339 row landing on the right side of both bounds.
    ///
    /// Two mutation guards, one per half. Inline a raw comparison into
    /// any single arm — the drift this pin exists to catch — and that
    /// arm's structural assertion reds. Revert [`WINDOW_DATE_EXPR`]
    /// itself to the raw comparison and the to-only behavioural half
    /// reds (the last-day row drops back out of the window).
    #[test]
    fn half_open_windows_share_the_normalised_predicate() {
        let jun_1 = Date::from_calendar_date(2026, Month::June, 1).unwrap();
        let jun_30 = Date::from_calendar_date(2026, Month::June, 30).unwrap();

        // Structural: all three emitting arms, one shared expression.
        let arms = [
            ("closed", june_2026(), true, true),
            (
                "from-only",
                DateWindow {
                    from: Some(jun_1),
                    to: None,
                },
                true,
                false,
            ),
            (
                "to-only",
                DateWindow {
                    from: None,
                    to: Some(jun_30),
                },
                false,
                true,
            ),
        ];
        for (label, window, want_from, want_to) in arms {
            let (sql, has_from, has_to) = build_date_where(window);
            assert_eq!(
                (has_from, has_to),
                (want_from, want_to),
                "{label}: bind slots"
            );
            let want_refs = usize::from(want_from) + usize::from(want_to);
            assert_eq!(
                sql.matches(WINDOW_DATE_EXPR).count(),
                want_refs,
                "{label}: every bound must compare the shared normalised \
                 expression, got `{sql}`"
            );
            assert!(
                !sql.replace(WINDOW_DATE_EXPR, "").contains("issue_date"),
                "{label}: no bound may compare a raw issue_date, got `{sql}`"
            );
        }
        // The unbounded arm emits nothing and binds nothing.
        assert_eq!(
            build_date_where(DateWindow::unbounded()),
            (String::new(), false, false),
            "the unbounded window must emit no WHERE and no bind slots"
        );

        // Behavioural: each half-open arm binds its slot and filters.
        let conn = Connection::open_in_memory().expect("in-memory duckdb");
        seed_window_fixture(&conn);
        let from_only = DateWindow {
            from: Some(jun_30),
            to: None,
        };
        let mut ids: Vec<String> = query_outgoing_groups(&conn, from_only, DateBasis::Teljesites)
            .expect("from-only window")
            .into_iter()
            .map(|g| g.invoice_id)
            .collect();
        ids.sort();
        assert_eq!(
            ids,
            vec!["inv-lastday", "inv-next"],
            "from-only: the last-day row is ON the lower bound and stays; \
             the mid-month row is below it and goes"
        );

        let to_only = DateWindow {
            from: None,
            to: Some(jun_30),
        };
        let mut ids: Vec<String> = query_outgoing_groups(&conn, to_only, DateBasis::Teljesites)
            .expect("to-only window")
            .into_iter()
            .map(|g| g.invoice_id)
            .collect();
        ids.sort();
        assert_eq!(
            ids,
            vec!["inv-lastday", "inv-mid"],
            "to-only: the last-day RFC3339 row is ON the upper bound and must \
             NOT sort above it; the July row stays out"
        );
    }

    /// PIN (d) — EXHAUSTIVE storno-child coverage. The base leaves AR if
    /// and only if its storno child classifies as `Counted`. This walks
    /// every [`CountedKind`] a storno child can reach, including the
    /// `Abandoned` and `Unknown` states the three headline pins don't
    /// touch, and asserts revenue and AR stay coherent in each.
    #[test]
    fn base_leaves_ar_iff_storno_child_is_counted_across_all_child_states() {
        let today = Date::from_calendar_date(2026, Month::June, 1).unwrap();
        // (label, child's audit entries, child lands?)
        let cases: Vec<(&str, Vec<Entry>, bool)> = vec![
            (
                "SAVED ack → Counted",
                vec![pin_ack("inv-storno", "SAVED")],
                true,
            ),
            (
                "submission response, no ack → Counted",
                vec![pin_entry(
                    EventKind::InvoiceSubmissionResponse,
                    serde_json::json!({
                        "invoice_id": "inv-storno",
                        "idempotency_key": "idem",
                        "transaction_id": "tx",
                        "response_xml": [],
                    }),
                )],
                true,
            ),
            (
                "ABORTED ack → Rejected",
                vec![pin_ack("inv-storno", "ABORTED")],
                false,
            ),
            (
                "draft only → PendingDraft",
                vec![pin_draft("inv-storno")],
                false,
            ),
            (
                "marked abandoned → Abandoned",
                vec![pin_entry(
                    EventKind::InvoiceMarkedAbandoned,
                    serde_json::json!({ "invoice_id": "inv-storno" }),
                )],
                false,
            ),
            ("no entries at all → Unknown", vec![], false),
        ];

        for (label, child_entries, lands) in cases {
            let mut entries = vec![
                pin_ack("inv-base", "SAVED"),
                pin_storno_link("inv-base", "inv-storno"),
            ];
            entries.extend(child_entries);
            let walk = walk_entries(&entries, DateWindow::unbounded());
            assert_eq!(
                walk.traces["inv-base"].has_landed_storno, lands,
                "{label}: has_landed_storno must track whether the child COUNTS"
            );

            let groups = vec![
                pin_group("inv-base", "2026-05-01", "2026-06-15", 100_000),
                pin_group("inv-storno", "2026-05-02", "2026-06-15", 100_000),
            ];
            let agg = aggregate_outgoing(groups, &walk.traces, today, &HashMap::new());

            let (want_revenue, want_ar) = if lands {
                // The pair nets in revenue; nothing is owed.
                (0, 0)
            } else {
                // The storno never counted: the base stands, in BOTH.
                (127_000, 127_000)
            };
            assert_eq!(
                agg.revenue.huf.gross_minor, want_revenue,
                "{label}: revenue"
            );
            assert_eq!(
                agg.receivables.huf.gross_minor, want_ar,
                "{label}: receivables must stay coherent with revenue"
            );
        }
    }
}
