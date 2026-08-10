//! T4 / ADR-0097 Part 2 (wiring) — `quoting_tolerance_cost_rates` catalogue.
//!
//! The engine half (T3) shipped [`aberp_quote_engine::ToleranceCostRate`] and
//! the additive, itemised `tolerance_cost` line in
//! [`aberp_quote_engine::quote_with_catalogue`], which prices the five
//! separable professional-tolerance cost drivers (in-process gauging, CMM,
//! extra slower-feed finishing passes, scrap/rework uplift, and — at the
//! tightest band — a grinding adder) at the routed effective EUR/min. This
//! module is the missing data layer: the operator-managed, band-keyed cost-rate
//! table the pricing pipeline snapshots into the engine's `CatalogueSnapshot`.
//!
//! ## Conventions mirrored from [`crate::quoting_machine_rates`] (S4) + [`crate::quoting_gear_processes`] (S6)
//!
//! Prefixed-ULID id (`qtcr_<ULID>`), lazy `CREATE TABLE IF NOT EXISTS`,
//! invariants enforced **in code** not via SQL CHECK/trigger
//! ([[no-sql-specific]]). The `tolerance_class` column stores the stable
//! db-string of the governing [`aberp_quote_engine::ToleranceRange`] band
//! ([`ToleranceRange::as_db_str`]); the closed-vocab check is the local
//! [`band_from_db_str`] round-trip (the engine exposes `as_db_str` but no
//! `from_db_str`, so the five-band list lives here — the single place that
//! parses the column). There is exactly **one rate per band** per tenant — the
//! band is the natural unique key (enforced in code like
//! `quoting_machine_rates`' one-rate-per-family key).
//!
//! ## Seed: researched EU/DE defaults on the tight bands, inert on the default band
//!
//! **Revised from the original T4 all-zero seed.** [`seed_tolerance_cost_rates_if_absent`]
//! now inserts the researched **default EU/DE machine-shop** values on the three
//! genuinely-tighter bands (`tight`, `precision`, `ultra_precision`) so
//! tolerance-driven pricing produces real numbers out of the box, and keeps
//! `loose` / `standard` at the engine's exact **no-op** — see [`SEEDS`] for the
//! per-value provenance and the R4 reasoning, and
//! `docs/findings/0097-tolerance-seed-rates-eu-de-2026-08-10.md` for the source
//! citations.
//!
//! The R4 (seed-inflation) guarantee is therefore **preserved where it matters**:
//! `standard` is the ISO 2768-m title-block default every un-toleranced quote
//! resolves to, so a part with no tolerance signal still prices byte-identically
//! to pre-ADR-0097. Money moves if and only if a genuinely tighter class or a
//! critical-feature callout is supplied — the cost driver ADR-0097 exists to
//! price. Every seeded row carries [`SEED_NOTE`] in `notes` so it is
//! unmistakably a seed default rather than a shop-measured value, and every row
//! stays operator-editable through the T5 CRUD (the seed is insert-if-absent per
//! band, so an operator edit is never re-clobbered on the next boot).
//!
//! (FLAG, unchanged from T4: because T3's `tolerance_op_cost` enters its
//! computation whenever the rate slice is **non-empty**, a seeded — therefore
//! non-empty — table makes a freshly priced quote's `reasoning_log` carry the
//! itemised tolerance lines even at a zero-cost band; the *price* at
//! `loose`/`standard` is unchanged and already-frozen quotes are untouched. The
//! truly empty table remains byte-identical including the log. See the T4
//! wiring note in `quote_pricing_pipeline`.)
//!
//! ## Audit
//!
//! CRUD emits via the audit ledger. T4 **reuses** [`EventKind::ParametersChanged`]
//! (the quoting-tunables-changed kind) rather than introducing a dedicated
//! `ToleranceCostRatesChanged` variant — exactly the S4 machine-rates
//! precedent: `EventKind` is not `#[non_exhaustive]` and has ~186 variants
//! matched across crates the 45s/4 GB sandbox cannot compile-verify, so a new
//! variant is an unacceptable blast radius here (ADR-0094 toolchain-honesty
//! clause). The payload is self-describing
//! (`"catalogue":"quoting_tolerance_cost_rates"`) so a future migration to a
//! dedicated kind is a pure relabel. FLAGGED for a later CI-backed follow-up.

use anyhow::{Context, Result};
use duckdb::{params, Connection};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use ulid::Ulid;

use aberp_audit_ledger::{append_in_tx, Actor, EventKind, LedgerMeta};
use aberp_quote_engine::ToleranceRange;

// Reuse the tunables write-error + validation-error vocab so the serve
// layer's `tunable_write_response` maps tolerance-cost-rate failures
// identically to the other catalogues.
use crate::quoting_tunables::{TunableWriteError, ValidationError};

/// The five governing [`ToleranceRange`] bands, in tightness order. The single
/// place the column's closed vocab is enumerated (the engine exposes
/// [`ToleranceRange::as_db_str`] but no inverse).
const BANDS: &[ToleranceRange] = &[
    ToleranceRange::Loose,
    ToleranceRange::Standard,
    ToleranceRange::Tight,
    ToleranceRange::Precision,
    ToleranceRange::UltraPrecision,
];

/// Parse a `tolerance_class` db-string back into its [`ToleranceRange`] — the
/// inverse of [`ToleranceRange::as_db_str`], kept local because the engine does
/// not ship a `from_db_str` (mirrors the one-list-per-module posture). `None`
/// for an unknown string (closed-vocab guard).
pub fn band_from_db_str(s: &str) -> Option<ToleranceRange> {
    BANDS.iter().copied().find(|b| b.as_db_str() == s)
}

/// Wire + storage shape of a `quoting_tolerance_cost_rates` row.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct ToleranceCostRateRow {
    /// `qtcr_<26-char-ULID>`.
    pub id: String,
    /// Governing [`ToleranceRange`] band db-string, e.g. `tight`.
    pub tolerance_class: String,
    /// Extra whole-part finishing passes contributed at this band.
    pub finish_passes_add: f64,
    /// In-process gauging minutes per critical feature.
    pub inproc_inspection_min: f64,
    /// Final / CMM-report minutes per critical feature.
    pub cmm_min_per_critical_feature: f64,
    /// Fractional scrap/rework uplift on `(material + machining)`.
    pub rework_scrap_pct: f64,
    /// `>= 1.0`; multiplies the extra-finishing-pass minutes (slower feeds hold
    /// a tight tolerance). `1.0` = no slowdown.
    pub feed_slowdown_factor: f64,
    /// Tightest-band grinding escalation (only fires at `ultra_precision`).
    pub grinding_escalation: bool,
    pub notes: Option<String>,
    pub updated_at: String,
    pub updated_by_actor: String,
}

/// Request body for create/update.
#[derive(Deserialize, Debug, Clone)]
pub struct ToleranceCostRateInputs {
    #[serde(default)]
    pub tolerance_class: String,
    #[serde(default)]
    pub finish_passes_add: f64,
    #[serde(default)]
    pub inproc_inspection_min: f64,
    #[serde(default)]
    pub cmm_min_per_critical_feature: f64,
    #[serde(default)]
    pub rework_scrap_pct: f64,
    #[serde(default = "default_feed_slowdown_factor")]
    pub feed_slowdown_factor: f64,
    #[serde(default)]
    pub grinding_escalation: bool,
    #[serde(default)]
    pub notes: Option<String>,
}

/// Neutral default for `feed_slowdown_factor` (1.0 ⇒ no slowdown), so a row
/// that only sets gauging minutes needs no finishing knob.
fn default_feed_slowdown_factor() -> f64 {
    1.0
}

/// The unmistakable provenance label stamped into every seeded row's `notes`
/// column, so no operator (and no auditor reading a priced-quote snapshot)
/// mistakes a researched **seed default** for their own measured shop numbers.
/// The SPA's rate list renders `notes` verbatim.
pub const SEED_NOTE: &str =
    "SEED — default EU/DE machine-shop rates, NOT your shop's measured values. \
     Tune to your shop. / ALAPÉRTÉK — EU/DE gépipari átlag, hangolja a saját műhelyére.";

/// One seed row: band + its day-1 numbers (ADR-0097 Q6, revised — see [`SEEDS`]).
struct Seed {
    band: ToleranceRange,
    finish_passes_add: f64,
    inproc_inspection_min: f64,
    cmm_min_per_critical_feature: f64,
    rework_scrap_pct: f64,
    feed_slowdown_factor: f64,
    grinding_escalation: bool,
}

/// Day-1 cost-rate seed — **researched default EU/DE machine-shop values, not
/// this shop's measured numbers**. Every row is stamped with [`SEED_NOTE`] in
/// its `notes` column and stays fully operator-editable through the T5 CRUD
/// (these are seed *rows*, not config constants — the operator's edit wins and
/// is never re-clobbered, because the seed is insert-if-absent per band).
///
/// ## Why `loose` and `standard` stay at exactly zero (the conservative fork)
///
/// `standard` is the ISO 2768-**m** title-block default — the band nearly every
/// un-toleranced quote resolves to. A non-zero `standard` seed would silently
/// raise **every** quote, including in-flight ones: precisely risk **R4**
/// (seed inflation) that ADR-0097 Q6 chose the all-zero seed to avoid. Keeping
/// `loose`/`standard` at the engine's no-op preserves that guarantee exactly —
/// a part with no tolerance signal still prices byte-identically — while the
/// three genuinely-tighter bands now produce **real numbers out of the box**.
/// Money moves if and only if a tighter spec or a critical-feature callout is
/// actually supplied, which is the cost driver ADR-0097 set out to price.
///
/// ## Provenance of the numbers (researched 2026-08-10; full citations in
/// `docs/findings/0097-tolerance-seed-rates-eu-de-2026-08-10.md`)
///
/// The three tightness-driven quantities the literature actually pins:
///
/// * **Scrap / rework.** A published aerospace-aluminium case relaxing twenty
///   ±0.01 mm dimensions to ±0.03 mm cut scrap from **12 % to 2 %** — read
///   directly as ≈2 % at the `tight` (±0.03-class) band and ≈12 % at
///   `ultra_precision` (±0.01-class), with `precision` interpolated at 5 %.
/// * **Inspection.** A metrology guide puts a simple part's CMM run at
///   **15–30 min** (complex components "an hour or more"), and tight tolerances
///   are reported to **double inspection effort**. The split to a *per-feature*
///   minute figure is ours: ≈2 min/feature at `precision` on a programmed
///   repeat run, doubled to 4 at the tightest band, with in-process gauging
///   charged at half the CMM minutes.
/// * **Slower feeds + extra finishing passes.** Going from ±0.005″ to ±0.0005″
///   raises machining cost **30–50 %** through slower feeds and additional
///   finishing passes ⇒ **half** an extra whole-part pass at a 1.25 feed factor
///   at `precision` and a full one at 1.5 at `ultra_precision`, landing the
///   line mid-range rather than at the 50 % ceiling.
///
/// Every value is deliberately at the **low end** of its researched range: a
/// seed that under-states is corrected by the operator's first real quote
/// review, whereas one that over-states silently wins no work.
const SEEDS: &[Seed] = &[
    // ── Inert bands: the engine's exact no-op (see the R4 note above). ──
    Seed {
        band: ToleranceRange::Loose,
        finish_passes_add: 0.0,
        inproc_inspection_min: 0.0,
        cmm_min_per_critical_feature: 0.0,
        rework_scrap_pct: 0.0,
        feed_slowdown_factor: 1.0,
        grinding_escalation: false,
    },
    Seed {
        band: ToleranceRange::Standard,
        finish_passes_add: 0.0,
        inproc_inspection_min: 0.0,
        cmm_min_per_critical_feature: 0.0,
        rework_scrap_pct: 0.0,
        feed_slowdown_factor: 1.0,
        grinding_escalation: false,
    },
    // ── Tight (ISO 2768-f / ≈IT9–IT10, ±0.03-class): the part is still cut on
    //    the routed machine at normal feeds. What changes is that the critical
    //    features now get gauged and a measurable scrap rate appears.
    Seed {
        band: ToleranceRange::Tight,
        finish_passes_add: 0.0,
        inproc_inspection_min: 0.5,
        cmm_min_per_critical_feature: 1.0,
        rework_scrap_pct: 0.02,
        feed_slowdown_factor: 1.0,
        grinding_escalation: false,
    },
    // ── Precision (≈IT6–IT7, H7-fit class): half an extra finishing pass at a
    //    slower feed, full CMM documentation per critical feature, 5 % scrap.
    //    `finish_passes_add` is a **whole-part** multiplier on the geometry
    //    finishing minutes, so a full 1.0 would re-finish every surface on the
    //    part to hold one toleranced bore — and lands the line at ~50 % of
    //    machining, the very top of the cited 30–50 % range, stacked on the
    //    1.9× `quoting_tolerance_multipliers` row that already fires here
    //    (risk R1). Half a pass ≈ re-finishing the toleranced regions only,
    //    and puts the line mid-range at ~30 %.
    Seed {
        band: ToleranceRange::Precision,
        finish_passes_add: 0.5,
        inproc_inspection_min: 1.0,
        cmm_min_per_critical_feature: 2.0,
        rework_scrap_pct: 0.05,
        feed_slowdown_factor: 1.25,
        grinding_escalation: false,
    },
    // ── UltraPrecision (≈IT5 and tighter, ±0.01-class): a full extra finishing
    //    pass at a 1.5 feed factor, doubled inspection, the published 12 % scrap, and
    //    the grinding escalation — this band is where a milled surface stops
    //    holding size and the feature goes to the grinder. That adder is priced
    //    at the `Grinder` machine-rate row seeded by `quoting_machine_rates`.
    Seed {
        band: ToleranceRange::UltraPrecision,
        finish_passes_add: 1.0,
        inproc_inspection_min: 2.0,
        cmm_min_per_critical_feature: 4.0,
        rework_scrap_pct: 0.12,
        feed_slowdown_factor: 1.5,
        grinding_escalation: true,
    },
];

/// Validate inputs in code (no SQL CHECK). Surfaces every error at once
/// (CLAUDE.md rule 9 / 12). The `tolerance_class` closed-vocab check
/// round-trips through [`band_from_db_str`].
pub fn validate_tolerance_cost_rate_inputs(
    inputs: &ToleranceCostRateInputs,
) -> Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();

    if band_from_db_str(inputs.tolerance_class.trim()).is_none() {
        errors.push(ValidationError {
            field: "tolerance_class",
            message: format!(
                "Ismeretlen tűrési sáv: {:?}. / Unknown tolerance band.",
                inputs.tolerance_class
            ),
        });
    }

    for (value, field) in [
        (inputs.finish_passes_add, "finish_passes_add"),
        (inputs.inproc_inspection_min, "inproc_inspection_min"),
        (
            inputs.cmm_min_per_critical_feature,
            "cmm_min_per_critical_feature",
        ),
        (inputs.rework_scrap_pct, "rework_scrap_pct"),
    ] {
        if !(value.is_finite() && value >= 0.0) {
            errors.push(ValidationError {
                field,
                message: "Az érték legyen véges és >= 0. / Value must be finite and >= 0."
                    .to_string(),
            });
        }
    }

    if !(inputs.feed_slowdown_factor.is_finite() && inputs.feed_slowdown_factor >= 1.0) {
        errors.push(ValidationError {
            field: "feed_slowdown_factor",
            message:
                "A lassítási szorzó legyen véges és >= 1.0. / Feed-slowdown factor must be finite and >= 1.0."
                    .to_string(),
        });
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS quoting_tolerance_cost_rates (
    id                           VARCHAR NOT NULL PRIMARY KEY,
    tenant_id                    VARCHAR NOT NULL,
    tolerance_class              VARCHAR NOT NULL,
    finish_passes_add            DOUBLE  NOT NULL,
    inproc_inspection_min        DOUBLE  NOT NULL,
    cmm_min_per_critical_feature DOUBLE  NOT NULL,
    rework_scrap_pct             DOUBLE  NOT NULL,
    feed_slowdown_factor         DOUBLE  NOT NULL,
    grinding_escalation          BOOLEAN NOT NULL,
    notes                        VARCHAR,
    updated_at                   VARCHAR NOT NULL,
    updated_by_actor             VARCHAR NOT NULL
);
";

const COLS: &str = "id, tolerance_class, finish_passes_add, inproc_inspection_min, \
                    cmm_min_per_critical_feature, rework_scrap_pct, feed_slowdown_factor, \
                    grinding_escalation, notes, updated_at, updated_by_actor";

/// Idempotent table creation. Called at serve boot + defensively on each
/// request entry point ([[hulye-biztos]]). No SQL CHECK/index — small
/// master data scanned in full ([[no-sql-specific]]).
pub fn ensure_schema(conn: &Connection) -> Result<()> {
    // ADR-0098 C2 fix-forward — no-op on a read-only conn (read_returns_readonly
    // read()-side); the schema is created by a writer before any read reaches
    // here. A genuine write mis-routed through read() still fails loud (F5).
    if aberp_audit_ledger::connection_is_read_only(conn) {
        return Ok(());
    }
    conn.execute_batch(SCHEMA_SQL)
        .context("ensure quoting_tolerance_cost_rates schema")
}

/// Seed the five ADR-0097 bands, **insert-if-absent** per band — so a re-run
/// (or a partially-seeded table) never duplicates and never clobbers an
/// operator-tuned value. Idempotent: gated per `(tenant, tolerance_class)`.
/// The `loose`/`standard` rows are zero-contribution ⇒ an un-toleranced part
/// prices byte-identically (R4); the three tighter bands carry the researched
/// EU/DE defaults, each stamped with [`SEED_NOTE`] so the row is visibly a
/// seed and not a shop-measured value ([`SEEDS`]).
pub fn seed_tolerance_cost_rates_if_absent(conn: &Connection, tenant: &str) -> Result<()> {
    ensure_schema(conn)?;
    let now = now_rfc3339()?;
    for seed in SEEDS {
        let band = seed.band.as_db_str();
        let existing: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM quoting_tolerance_cost_rates \
                 WHERE tenant_id = ? AND tolerance_class = ?;",
                params![tenant, band],
                |r| r.get(0),
            )
            .context("count quoting_tolerance_cost_rates for seed gate")?;
        if existing > 0 {
            upgrade_pristine_zero_seed(conn, tenant, seed, &now)?;
            continue;
        }
        let id = format!("qtcr_{}", Ulid::new());
        conn.execute(
            "INSERT INTO quoting_tolerance_cost_rates (id, tenant_id, tolerance_class, \
             finish_passes_add, inproc_inspection_min, cmm_min_per_critical_feature, \
             rework_scrap_pct, feed_slowdown_factor, grinding_escalation, \
             notes, updated_at, updated_by_actor) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'boot');",
            params![
                &id,
                tenant,
                band,
                seed.finish_passes_add,
                seed.inproc_inspection_min,
                seed.cmm_min_per_critical_feature,
                seed.rework_scrap_pct,
                seed.feed_slowdown_factor,
                seed.grinding_escalation,
                SEED_NOTE,
                &now,
            ],
        )
        .with_context(|| format!("seed quoting_tolerance_cost_rates row for {band}"))?;
    }
    Ok(())
}

/// One-shot migration of a tenant seeded under the **original T4 all-zero**
/// seed onto the researched [`SEEDS`] values.
///
/// `seed_tolerance_cost_rates_if_absent` is insert-if-absent per band, so an
/// already-seeded tenant (every Defense pilot DB) would otherwise keep the
/// all-zero rows forever and tolerance pricing would stay silently at 0.00 EUR
/// — the exact defect this change exists to close.
///
/// The upgrade is gated on the row being **provably untouched by a human**, and
/// declines on any doubt:
///
/// * `updated_by_actor = 'boot'` — the CRUD stamps the operator's login on
///   every write, so anything else means a person edited this row;
/// * `notes IS NULL` — the old seed wrote NULL; the new seed and every CRUD
///   write set a non-NULL value, so this also makes the upgrade run at most once;
/// * all six drivers still at the engine's exact zero-contribution no-op — an
///   operator who tuned a value and reverted it is indistinguishable from
///   pristine, and in that case writing the seed default is still the intended
///   day-1 state.
///
/// A row failing any gate is left exactly as-is. Deliberately **not** audited:
/// this is boot-time seeding on rows no actor has ever written, identical in
/// kind to the INSERT beside it, which is likewise unaudited.
fn upgrade_pristine_zero_seed(
    conn: &Connection,
    tenant: &str,
    seed: &Seed,
    now: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE quoting_tolerance_cost_rates SET finish_passes_add = ?, \
         inproc_inspection_min = ?, cmm_min_per_critical_feature = ?, \
         rework_scrap_pct = ?, feed_slowdown_factor = ?, grinding_escalation = ?, \
         notes = ?, updated_at = ? \
         WHERE tenant_id = ? AND tolerance_class = ? \
           AND updated_by_actor = 'boot' AND notes IS NULL \
           AND finish_passes_add = 0 AND inproc_inspection_min = 0 \
           AND cmm_min_per_critical_feature = 0 AND rework_scrap_pct = 0 \
           AND feed_slowdown_factor = 1 AND grinding_escalation = FALSE;",
        params![
            seed.finish_passes_add,
            seed.inproc_inspection_min,
            seed.cmm_min_per_critical_feature,
            seed.rework_scrap_pct,
            seed.feed_slowdown_factor,
            seed.grinding_escalation,
            SEED_NOTE,
            now,
            tenant,
            seed.band.as_db_str(),
        ],
    )
    .with_context(|| {
        format!(
            "upgrade pristine zero seed for band {}",
            seed.band.as_db_str()
        )
    })?;
    Ok(())
}

fn now_rfc3339() -> Result<String> {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .context("format quoting_tolerance_cost_rates timestamp")
}

fn row_to_tolerance_cost_rate(row: &duckdb::Row<'_>) -> duckdb::Result<ToleranceCostRateRow> {
    Ok(ToleranceCostRateRow {
        id: row.get(0)?,
        tolerance_class: row.get(1)?,
        finish_passes_add: row.get(2)?,
        inproc_inspection_min: row.get(3)?,
        cmm_min_per_critical_feature: row.get(4)?,
        rework_scrap_pct: row.get(5)?,
        feed_slowdown_factor: row.get(6)?,
        grinding_escalation: row.get(7)?,
        notes: row.get(8)?,
        updated_at: row.get(9)?,
        updated_by_actor: row.get(10)?,
    })
}

/// All rate rows for a tenant, band-ordered (stable list for the SPA).
pub fn list_tolerance_cost_rates(
    conn: &Connection,
    tenant: &str,
) -> Result<Vec<ToleranceCostRateRow>> {
    ensure_schema(conn)?;
    let sql = format!(
        "SELECT {COLS} FROM quoting_tolerance_cost_rates WHERE tenant_id = ? \
         ORDER BY tolerance_class ASC;"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![tenant], row_to_tolerance_cost_rate)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

fn get_tolerance_cost_rate(
    conn: &Connection,
    tenant: &str,
    id: &str,
) -> Result<Option<ToleranceCostRateRow>> {
    let sql =
        format!("SELECT {COLS} FROM quoting_tolerance_cost_rates WHERE tenant_id = ? AND id = ?;");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query_map(params![tenant, id], row_to_tolerance_cost_rate)?;
    match rows.next() {
        Some(r) => Ok(Some(r?)),
        None => Ok(None),
    }
}

/// Count rows holding `band` other than `except_id` — the in-code
/// one-rate-per-band uniqueness guard (no SQL UNIQUE, [[no-sql-specific]]).
fn band_taken_by_other(
    conn: &Connection,
    tenant: &str,
    band: &str,
    except_id: &str,
) -> Result<bool> {
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM quoting_tolerance_cost_rates \
             WHERE tenant_id = ? AND tolerance_class = ? AND id != ?;",
            params![tenant, band, except_id],
            |r| r.get(0),
        )
        .context("check quoting_tolerance_cost_rates band uniqueness")?;
    Ok(n > 0)
}

/// Create a rate for a band (one per band). `Conflict` if the band already has
/// a row — the operator edits the existing (seeded) one instead.
pub fn create_tolerance_cost_rate(
    conn: &mut Connection,
    meta: &LedgerMeta,
    actor_login: &str,
    tenant: &str,
    inputs: &ToleranceCostRateInputs,
) -> Result<ToleranceCostRateRow, TunableWriteError> {
    if let Err(e) = validate_tolerance_cost_rate_inputs(inputs) {
        return Err(TunableWriteError::Validation(e));
    }
    ensure_schema(conn).map_err(TunableWriteError::Other)?;
    let band = band_from_db_str(inputs.tolerance_class.trim())
        .context("tolerance_class validated before create")
        .map_err(TunableWriteError::Other)?
        .as_db_str();
    if band_taken_by_other(conn, tenant, band, "").map_err(TunableWriteError::Other)? {
        return Err(TunableWriteError::Conflict(format!(
            "a rate for band `{band}` already exists — edit it instead"
        )));
    }
    let now = now_rfc3339().map_err(TunableWriteError::Other)?;
    let notes = normalize_optional(inputs.notes.as_deref());
    let id = format!("qtcr_{}", Ulid::new());
    let tx = conn
        .transaction()
        .context("begin create_tolerance_cost_rate tx")
        .map_err(TunableWriteError::Other)?;
    tx.execute(
        "INSERT INTO quoting_tolerance_cost_rates (id, tenant_id, tolerance_class, \
         finish_passes_add, inproc_inspection_min, cmm_min_per_critical_feature, \
         rework_scrap_pct, feed_slowdown_factor, grinding_escalation, \
         notes, updated_at, updated_by_actor) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?);",
        params![
            &id,
            tenant,
            band,
            inputs.finish_passes_add,
            inputs.inproc_inspection_min,
            inputs.cmm_min_per_critical_feature,
            inputs.rework_scrap_pct,
            inputs.feed_slowdown_factor,
            inputs.grinding_escalation,
            notes.as_deref(),
            &now,
            actor_login,
        ],
    )
    .context("INSERT quoting_tolerance_cost_rates")
    .map_err(TunableWriteError::Other)?;
    let row = read_in_tx(&tx, tenant, &id).map_err(TunableWriteError::Other)?;
    append_tolerance_cost_rate_change(&tx, meta, actor_login, "tolerance_cost_rate_create", &row)
        .map_err(TunableWriteError::Other)?;
    tx.commit()
        .context("commit create_tolerance_cost_rate")
        .map_err(TunableWriteError::Other)?;
    Ok(row)
}

/// Update a rate by id. `NotFound` if the row is absent; `Conflict` if the
/// edited `tolerance_class` collides with another row.
pub fn update_tolerance_cost_rate(
    conn: &mut Connection,
    meta: &LedgerMeta,
    actor_login: &str,
    tenant: &str,
    id: &str,
    inputs: &ToleranceCostRateInputs,
) -> Result<ToleranceCostRateRow, TunableWriteError> {
    if let Err(e) = validate_tolerance_cost_rate_inputs(inputs) {
        return Err(TunableWriteError::Validation(e));
    }
    ensure_schema(conn).map_err(TunableWriteError::Other)?;
    let band = band_from_db_str(inputs.tolerance_class.trim())
        .context("tolerance_class validated before update")
        .map_err(TunableWriteError::Other)?
        .as_db_str();
    if get_tolerance_cost_rate(conn, tenant, id)
        .map_err(TunableWriteError::Other)?
        .is_none()
    {
        return Err(TunableWriteError::NotFound(format!(
            "quoting_tolerance_cost_rates row {id} not found"
        )));
    }
    if band_taken_by_other(conn, tenant, band, id).map_err(TunableWriteError::Other)? {
        return Err(TunableWriteError::Conflict(format!(
            "another rate for band `{band}` already exists"
        )));
    }
    let now = now_rfc3339().map_err(TunableWriteError::Other)?;
    let notes = normalize_optional(inputs.notes.as_deref());
    let tx = conn
        .transaction()
        .context("begin update_tolerance_cost_rate tx")
        .map_err(TunableWriteError::Other)?;
    tx.execute(
        "UPDATE quoting_tolerance_cost_rates SET tolerance_class = ?, finish_passes_add = ?, \
         inproc_inspection_min = ?, cmm_min_per_critical_feature = ?, rework_scrap_pct = ?, \
         feed_slowdown_factor = ?, grinding_escalation = ?, notes = ?, updated_at = ?, \
         updated_by_actor = ? WHERE tenant_id = ? AND id = ?;",
        params![
            band,
            inputs.finish_passes_add,
            inputs.inproc_inspection_min,
            inputs.cmm_min_per_critical_feature,
            inputs.rework_scrap_pct,
            inputs.feed_slowdown_factor,
            inputs.grinding_escalation,
            notes.as_deref(),
            &now,
            actor_login,
            tenant,
            id,
        ],
    )
    .context("UPDATE quoting_tolerance_cost_rates")
    .map_err(TunableWriteError::Other)?;
    let row = read_in_tx(&tx, tenant, id).map_err(TunableWriteError::Other)?;
    append_tolerance_cost_rate_change(&tx, meta, actor_login, "tolerance_cost_rate_update", &row)
        .map_err(TunableWriteError::Other)?;
    tx.commit()
        .context("commit update_tolerance_cost_rate")
        .map_err(TunableWriteError::Other)?;
    Ok(row)
}

/// Hard-delete a rate by id (the band falls back to a zero `tolerance_cost`
/// contribution in the engine — no orphaned pricing). `NotFound` if absent.
pub fn delete_tolerance_cost_rate(
    conn: &mut Connection,
    meta: &LedgerMeta,
    actor_login: &str,
    tenant: &str,
    id: &str,
) -> Result<(), TunableWriteError> {
    ensure_schema(conn).map_err(TunableWriteError::Other)?;
    let Some(row) = get_tolerance_cost_rate(conn, tenant, id).map_err(TunableWriteError::Other)?
    else {
        return Err(TunableWriteError::NotFound(format!(
            "quoting_tolerance_cost_rates row {id} not found"
        )));
    };
    let tx = conn
        .transaction()
        .context("begin delete_tolerance_cost_rate tx")
        .map_err(TunableWriteError::Other)?;
    tx.execute(
        "DELETE FROM quoting_tolerance_cost_rates WHERE tenant_id = ? AND id = ?;",
        params![tenant, id],
    )
    .context("DELETE quoting_tolerance_cost_rates")
    .map_err(TunableWriteError::Other)?;
    append_tolerance_cost_rate_change(&tx, meta, actor_login, "tolerance_cost_rate_delete", &row)
        .map_err(TunableWriteError::Other)?;
    tx.commit()
        .context("commit delete_tolerance_cost_rate")
        .map_err(TunableWriteError::Other)?;
    Ok(())
}

// ── Internals ───────────────────────────────────────────────────────────

fn normalize_optional(s: Option<&str>) -> Option<String> {
    s.map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

fn read_in_tx(
    tx: &duckdb::Transaction<'_>,
    tenant: &str,
    id: &str,
) -> Result<ToleranceCostRateRow> {
    let sql =
        format!("SELECT {COLS} FROM quoting_tolerance_cost_rates WHERE tenant_id = ? AND id = ?;");
    let mut stmt = tx.prepare(&sql)?;
    let mut rows = stmt.query_map(params![tenant, id], row_to_tolerance_cost_rate)?;
    match rows.next() {
        Some(r) => Ok(r?),
        None => Err(anyhow::anyhow!(
            "quoting_tolerance_cost_rates row {id} vanished mid-tx"
        )),
    }
}

/// Append a tolerance-cost-rate-change audit entry inside the write tx. Reuses
/// [`EventKind::ParametersChanged`] (see module docs / FLAG) with a
/// self-describing payload so a future dedicated kind is a pure relabel.
fn append_tolerance_cost_rate_change(
    tx: &duckdb::Transaction<'_>,
    meta: &LedgerMeta,
    actor_login: &str,
    op: &str,
    row: &ToleranceCostRateRow,
) -> Result<()> {
    let payload = serde_json::json!({
        "catalogue": "quoting_tolerance_cost_rates",
        "op": op,
        "snapshot": { "row": row },
        "idempotency_key": Ulid::new().to_string(),
    });
    let bytes = serde_json::to_vec(&payload)
        .context("serialize tolerance-cost-rate change audit payload")?;
    let actor = Actor::from_local_cli(Ulid::new().to_string(), actor_login);
    append_in_tx(tx, meta, EventKind::ParametersChanged, bytes, actor, None)
        .context("audit append tolerance-cost-rate change")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aberp_quote_engine::{
        quote_with_catalogue, CalibrationTable, CatalogueSnapshot, ComplexityRule, Feature,
        FeatureGraph, FeatureTolerance, FeatureType, GeneralClass, MachineRate, Material,
        QuoteBreakdown, QuotingParameters, StockForm, StockStatus, ToleranceCostRate,
        ToleranceMultiplier, ToleranceSpec,
    };

    // ── Seed-table behaviour (DuckDB-backed) ─────────────────────────

    fn mem() -> Connection {
        let conn = Connection::open_in_memory().expect("open in-memory DuckDB");
        aberp_audit_ledger::ensure_schema(&conn).expect("audit-ledger schema");
        ensure_schema(&conn).expect("ensure schema");
        conn
    }

    fn ledger_meta() -> LedgerMeta {
        LedgerMeta::new(
            aberp_audit_ledger::TenantId::new("t1").expect("tenant id"),
            aberp_audit_ledger::BinaryHash::from_bytes([0u8; 32]),
        )
    }

    fn band_row(conn: &Connection, band: &str) -> ToleranceCostRateRow {
        list_tolerance_cost_rates(conn, "t1")
            .expect("list")
            .into_iter()
            .find(|r| r.tolerance_class == band)
            .unwrap_or_else(|| panic!("no seeded row for band {band}"))
    }

    /// The seed writes the researched EU/DE values on the three tighter bands,
    /// keeps `loose`/`standard` at the engine's exact no-op (R4), and labels
    /// every row unmistakably as a seed default.
    #[test]
    fn seed_writes_researched_values_and_labels_every_row() {
        let conn = mem();
        seed_tolerance_cost_rates_if_absent(&conn, "t1").expect("seed");

        for band in ["loose", "standard"] {
            let r = band_row(&conn, band);
            assert_eq!(r.finish_passes_add, 0.0, "{band} finish_passes_add");
            assert_eq!(r.inproc_inspection_min, 0.0, "{band} inproc");
            assert_eq!(r.cmm_min_per_critical_feature, 0.0, "{band} cmm");
            assert_eq!(r.rework_scrap_pct, 0.0, "{band} scrap");
            assert_eq!(r.feed_slowdown_factor, 1.0, "{band} feed");
            assert!(!r.grinding_escalation, "{band} grinding");
        }

        let tight = band_row(&conn, "tight");
        assert_eq!(tight.inproc_inspection_min, 0.5);
        assert_eq!(tight.cmm_min_per_critical_feature, 1.0);
        assert_eq!(tight.rework_scrap_pct, 0.02);
        assert_eq!(tight.finish_passes_add, 0.0);
        assert!(!tight.grinding_escalation);

        let prec = band_row(&conn, "precision");
        assert_eq!(prec.finish_passes_add, 0.5);
        assert_eq!(prec.cmm_min_per_critical_feature, 2.0);
        assert_eq!(prec.rework_scrap_pct, 0.05);
        assert_eq!(prec.feed_slowdown_factor, 1.25);
        assert!(
            !prec.grinding_escalation,
            "grinding fires only at the tightest band"
        );

        let ultra = band_row(&conn, "ultra_precision");
        assert_eq!(ultra.finish_passes_add, 1.0);
        assert_eq!(ultra.cmm_min_per_critical_feature, 4.0);
        assert_eq!(ultra.rework_scrap_pct, 0.12);
        assert_eq!(ultra.feed_slowdown_factor, 1.5);
        assert!(ultra.grinding_escalation);

        // Every row is visibly a seed, never mistakable for a measured rate.
        for r in list_tolerance_cost_rates(&conn, "t1").expect("list") {
            assert_eq!(r.notes.as_deref(), Some(SEED_NOTE), "{}", r.tolerance_class);
        }
        // Monotone in tightness — the invariant an operator tuning the table
        // must not accidentally invert.
        assert!(tight.rework_scrap_pct < prec.rework_scrap_pct);
        assert!(prec.rework_scrap_pct < ultra.rework_scrap_pct);
    }

    /// Re-running the seed neither duplicates rows nor overwrites a value the
    /// operator has tuned (the row is no longer `boot`-stamped).
    #[test]
    fn reseed_is_idempotent_and_never_clobbers_an_operator_edit() {
        let mut conn = mem();
        seed_tolerance_cost_rates_if_absent(&conn, "t1").expect("seed");
        let id = band_row(&conn, "precision").id;

        let meta = ledger_meta();
        update_tolerance_cost_rate(
            &mut conn,
            &meta,
            "ervin",
            "t1",
            &id,
            &ToleranceCostRateInputs {
                tolerance_class: "precision".to_string(),
                finish_passes_add: 3.0,
                inproc_inspection_min: 7.0,
                cmm_min_per_critical_feature: 9.0,
                rework_scrap_pct: 0.33,
                feed_slowdown_factor: 2.5,
                grinding_escalation: true,
                notes: Some("our measured cell".to_string()),
            },
        )
        .expect("operator update");

        seed_tolerance_cost_rates_if_absent(&conn, "t1").expect("reseed");

        assert_eq!(
            list_tolerance_cost_rates(&conn, "t1").expect("list").len(),
            5
        );
        let after = band_row(&conn, "precision");
        assert_eq!(
            after.inproc_inspection_min, 7.0,
            "operator value survives reseed"
        );
        assert_eq!(after.rework_scrap_pct, 0.33);
        assert_eq!(after.notes.as_deref(), Some("our measured cell"));
    }

    /// Insert a row exactly as the ORIGINAL T4 all-zero seed wrote it
    /// (`updated_by_actor = 'boot'`, `notes` NULL, every driver at the no-op).
    fn insert_legacy_zero_row(conn: &Connection, band: &str, actor: &str) {
        conn.execute(
            "INSERT INTO quoting_tolerance_cost_rates (id, tenant_id, tolerance_class, \
             finish_passes_add, inproc_inspection_min, cmm_min_per_critical_feature, \
             rework_scrap_pct, feed_slowdown_factor, grinding_escalation, \
             notes, updated_at, updated_by_actor) \
             VALUES (?, 't1', ?, 0, 0, 0, 0, 1.0, FALSE, NULL, '2026-01-01T00:00:00Z', ?);",
            params![format!("qtcr_{}", Ulid::new()), band, actor],
        )
        .expect("insert legacy zero row");
    }

    /// A tenant seeded under the old all-zero seed is migrated onto the
    /// researched values — otherwise insert-if-absent would leave every
    /// existing pilot DB pricing tolerance at 0.00 EUR forever.
    #[test]
    fn pristine_legacy_zero_seed_is_upgraded() {
        let conn = mem();
        for band in ["loose", "standard", "tight", "precision", "ultra_precision"] {
            insert_legacy_zero_row(&conn, band, "boot");
        }
        seed_tolerance_cost_rates_if_absent(&conn, "t1").expect("seed over legacy rows");

        assert_eq!(
            list_tolerance_cost_rates(&conn, "t1").expect("list").len(),
            5,
            "upgrade must not duplicate rows"
        );
        let ultra = band_row(&conn, "ultra_precision");
        assert_eq!(ultra.rework_scrap_pct, 0.12);
        assert!(ultra.grinding_escalation);
        assert_eq!(ultra.notes.as_deref(), Some(SEED_NOTE));
        // The inert bands stay inert through the upgrade.
        assert_eq!(band_row(&conn, "standard").rework_scrap_pct, 0.0);
    }

    /// A zero row a HUMAN wrote is deliberately left alone — the operator may
    /// have zeroed the band on purpose, and the upgrade must not second-guess.
    #[test]
    fn human_written_zero_row_is_not_upgraded() {
        let conn = mem();
        insert_legacy_zero_row(&conn, "ultra_precision", "ervin");
        seed_tolerance_cost_rates_if_absent(&conn, "t1").expect("seed");

        let ultra = band_row(&conn, "ultra_precision");
        assert_eq!(ultra.rework_scrap_pct, 0.0, "human zero survives");
        assert!(!ultra.grinding_escalation);
        assert_eq!(ultra.notes, None);
    }

    /// The upgrade runs at most once: after it has stamped `notes`, a second
    /// boot finds no pristine row and rewrites nothing.
    #[test]
    fn upgrade_does_not_re_run_over_a_tuned_back_to_zero_row() {
        let mut conn = mem();
        for band in ["loose", "standard", "tight", "precision", "ultra_precision"] {
            insert_legacy_zero_row(&conn, band, "boot");
        }
        seed_tolerance_cost_rates_if_absent(&conn, "t1").expect("first upgrade");

        // Operator decides this shop grinds nothing and zeroes the band back.
        let id = band_row(&conn, "ultra_precision").id;
        let meta = ledger_meta();
        update_tolerance_cost_rate(
            &mut conn,
            &meta,
            "ervin",
            "t1",
            &id,
            &ToleranceCostRateInputs {
                tolerance_class: "ultra_precision".to_string(),
                finish_passes_add: 0.0,
                inproc_inspection_min: 0.0,
                cmm_min_per_critical_feature: 0.0,
                rework_scrap_pct: 0.0,
                feed_slowdown_factor: 1.0,
                grinding_escalation: false,
                notes: None,
            },
        )
        .expect("operator zeroes the band");

        seed_tolerance_cost_rates_if_absent(&conn, "t1").expect("second boot");
        assert_eq!(
            band_row(&conn, "ultra_precision").rework_scrap_pct,
            0.0,
            "the seed must not resurrect a band the operator deliberately zeroed"
        );
    }

    // ── The seeded rates actually price a tolerance quote ─────────────

    /// The seed constants as the engine consumes them — the exact slice the
    /// pricing pipeline builds from the seeded table.
    fn engine_rates_from_seed() -> Vec<ToleranceCostRate> {
        SEEDS
            .iter()
            .map(|s| ToleranceCostRate {
                tolerance_class: s.band.as_db_str().to_string(),
                finish_passes_add: s.finish_passes_add,
                inproc_inspection_min: s.inproc_inspection_min,
                cmm_min_per_critical_feature: s.cmm_min_per_critical_feature,
                rework_scrap_pct: s.rework_scrap_pct,
                feed_slowdown_factor: s.feed_slowdown_factor,
                grinding_escalation: s.grinding_escalation,
            })
            .collect()
    }

    /// The `quoting_machine_rates` seed as the engine consumes it — including
    /// the `Grinder` row this change adds.
    fn engine_machine_rates() -> Vec<MachineRate> {
        crate::quoting_machine_rates::seed_rates_for_tests()
    }

    fn params() -> QuotingParameters {
        QuotingParameters {
            scrap_factor: 0.15,
            profit_margin_base: 0.35,
            overhead_factor: 0.20,
            setup_amortization_threshold: 5,
            min_margin: 0.10,
            exotic_material_tax: 0.05,
            machining_rate_eur_per_minute: 1.6667,
            cad_cam_rate_eur_per_hour: 100.0,
            cad_cam_base_hours: 1.0,
            mrr_rough_ref_cm3_per_min: 8.0,
            t_finish_min_per_cm2: 0.08,
            setup_base_min: 20.0,
            setup_5axis_min: 25.0,
            bar_capacity_mm: 32.0,
        }
    }

    /// A 120 × 80 × 30 aluminium bracket with a bored/pocketed face — a
    /// bread-and-butter prismatic job that routes 3-axis.
    fn bracket(callout: Option<ToleranceSpec>) -> FeatureGraph {
        FeatureGraph {
            gears: Vec::new(),
            schema_version: FeatureGraph::SCHEMA_VERSION,
            bounding_box_mm: [120.0, 80.0, 30.0],
            volume_mm3: 180_000.0,
            surface_area_mm2: 32_000.0,
            material_grade: "6061-T6".to_string(),
            features: vec![
                Feature {
                    feature_type: FeatureType::Hole,
                    count: 4,
                    representative_size_mm: 12.0,
                },
                Feature {
                    feature_type: FeatureType::Pocket,
                    count: 2,
                    representative_size_mm: 40.0,
                },
            ],
            requires_5_axis: false,
            thin_wall_present: false,
            stock_form: StockForm::RectangularBlock,
            tolerance: ToleranceSpec::Unspecified,
            critical_feature_tolerances: callout
                .map(|spec| {
                    vec![FeatureTolerance {
                        feature_index: 0,
                        spec,
                    }]
                })
                .unwrap_or_default(),
        }
    }

    fn price(fg: &FeatureGraph, band: ToleranceRange) -> QuoteBreakdown {
        let materials = vec![Material {
            grade: "6061-T6".to_string(),
            density_g_cm3: 2.7,
            cost_per_kg_eur: 6.0,
            machining_difficulty: 1.0,
            quote_multiplier: 1.0,
            stock_status: StockStatus::InStock,
        }];
        let mut rules = Vec::new();
        let mut id = 1_i64;
        for ft in [
            "pocket",
            "hole",
            "slot",
            "thread",
            "undercut_5axis",
            "thin_wall",
            "surface",
            "engraving",
        ] {
            for sb in ["XS", "S", "M", "L", "XL"] {
                rules.push(ComplexityRule {
                    id,
                    feature_type: ft.to_string(),
                    size_bucket: sb.to_string(),
                    count_min: 1,
                    count_max: None,
                    base_time_minutes: 2.0,
                    multiplier: 1.0,
                    setup_penalty_minutes: 5.0,
                });
                id += 1;
            }
        }
        let multipliers = vec![
            ToleranceMultiplier {
                tolerance_range: "loose".into(),
                multiplier: 0.9,
                inspection_minutes_per_feature: 0.0,
            },
            ToleranceMultiplier {
                tolerance_range: "standard".into(),
                multiplier: 1.0,
                inspection_minutes_per_feature: 0.0,
            },
            ToleranceMultiplier {
                tolerance_range: "tight".into(),
                multiplier: 1.4,
                inspection_minutes_per_feature: 0.5,
            },
            ToleranceMultiplier {
                tolerance_range: "precision".into(),
                multiplier: 1.9,
                inspection_minutes_per_feature: 1.5,
            },
            ToleranceMultiplier {
                tolerance_range: "ultra_precision".into(),
                multiplier: 2.8,
                inspection_minutes_per_feature: 3.0,
            },
        ];
        let stock = Vec::new();
        let machine = engine_machine_rates();
        let gears = Vec::new();
        let rates = engine_rates_from_seed();
        let snap = CatalogueSnapshot {
            materials: &materials,
            complexity_rules: &rules,
            tolerance_multipliers: &multipliers,
            stock_adjustments: &stock,
            machine_rates: &machine,
            gear_process_rates: &gears,
            tolerance_cost_rates: &rates,
        };
        quote_with_catalogue(fg, &snap, &params(), 10, band, &CalibrationTable::neutral())
            .expect("seeded tolerance quote must price")
    }

    /// THE last-mile assertion: with the table seeded, a part carrying a real
    /// tight callout now prices a **non-zero** tolerance line — and one with no
    /// tolerance signal still prices at exactly zero (R4 inert proof).
    #[test]
    fn seeded_rates_price_a_tolerance_quote_at_a_sane_order_of_magnitude() {
        // No callout, default band ⇒ the seeded table moves no money.
        let inert = price(&bracket(None), ToleranceRange::Standard);
        assert_eq!(
            inert.tolerance_cost, 0.0,
            "an un-toleranced part must stay byte-identical under the seeded table (R4)"
        );

        // A Ø12 H7 critical bore ⇒ Precision band ⇒ real money.
        let prec = price(
            &bracket(Some(ToleranceSpec::ItGrade { grade: 7 })),
            ToleranceRange::Standard,
        );
        assert!(
            prec.tolerance_cost > 0.0,
            "the whole point of the seed: a tight callout must cost something, got {}",
            prec.tolerance_cost
        );
        // Order of magnitude: a few EUR of gauging/CMM/finishing/scrap on a
        // ~10-EUR-class bracket — not cents, and not hundreds.
        assert!(
            (0.50..50.0).contains(&prec.tolerance_cost),
            "tolerance_cost {} EUR is outside the sane band for this part",
            prec.tolerance_cost
        );
        assert!(
            prec.total_price > inert.total_price,
            "a tighter part must not price below the same part at the default band"
        );

        // Every term is itemised in the log (the ADR-0097 auditability contract).
        let log = prec.reasoning_log.join("\n");
        for term in [
            "[tolerance] inspection =",
            "[tolerance] finishing =",
            "[tolerance] scrap/rework =",
            "[tolerance] total tolerance_cost=",
        ] {
            assert!(
                log.contains(term),
                "missing reasoning line {term} in:\n{log}"
            );
        }
    }

    /// Tightness is monotone in price, and the tightest band fires the grinding
    /// adder **at the seeded `Grinder` machine rate** — the row this change adds.
    /// Without it the adder silently falls back to the routed effective rate.
    #[test]
    fn tightest_band_grinds_at_the_seeded_grinder_rate() {
        let tight = price(
            &bracket(Some(ToleranceSpec::GeneralClass {
                class: GeneralClass::Iso2768Fine,
            })),
            ToleranceRange::Standard,
        );
        let prec = price(
            &bracket(Some(ToleranceSpec::ItGrade { grade: 7 })),
            ToleranceRange::Standard,
        );
        let ultra = price(
            &bracket(Some(ToleranceSpec::ItGrade { grade: 4 })),
            ToleranceRange::Standard,
        );

        assert!(tight.tolerance_cost > 0.0, "tight must cost something");
        assert!(
            tight.tolerance_cost < prec.tolerance_cost,
            "tight {} !< precision {}",
            tight.tolerance_cost,
            prec.tolerance_cost
        );
        assert!(
            prec.tolerance_cost < ultra.tolerance_cost,
            "precision {} !< ultra {}",
            prec.tolerance_cost,
            ultra.tolerance_cost
        );

        let log = ultra.reasoning_log.join("\n");
        assert!(
            log.contains("grinding escalation"),
            "tightest band must fire the grinding adder:\n{log}"
        );
        assert!(
            log.contains("grinder_rate=2.5000"),
            "grinding must price at the seeded Grinder machine rate, not the routed fallback:\n{log}"
        );
        assert!(
            !prec
                .reasoning_log
                .join("\n")
                .contains("grinding escalation"),
            "grinding must NOT fire below the tightest band"
        );
    }
}
