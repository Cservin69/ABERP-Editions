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
//! stays operator-editable through the T5 CRUD. The band set is laid down
//! **once per tenant, ever** (a durable marker row — see
//! [`tenant_already_seeded`]), so neither an operator's edit nor an operator's
//! *deletion* is overruled on the next boot.
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

/// N7 — upper bound on `rework_scrap_pct`. It is a fraction (0.05 = 5 %), so a
/// value above 1.0 means the operator typed a percentage into a fraction field:
/// a scrap allowance exceeding the entire job is a typo, not a rate.
pub const MAX_REWORK_SCRAP_PCT: f64 = 1.0;

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
/// * **Slower feeds + extra finishing passes.** Tight tolerances raise machining
///   time **30–200 %**, and tightening ±0.05→±0.02 mm "adds 50–80 %" while
///   ±0.02→±0.01 mm "multiplies by 2–4×" ⇒ **half** an extra whole-part pass at
///   a 1.25 feed factor at `precision` and the same half-pass at 1.5 at
///   `ultra_precision` (the feed factor, not the pass count, carries the extra
///   tightness). Both sit at the **bottom** of those ranges.
///
///   *(Corrected: an earlier draft attributed a "±0.005″→±0.0005″ raises cost
///   30–50 %" figure to a source that does not contain it. The claim is gone;
///   the values above are re-anchored on sources quoted verbatim in the
///   findings note, and the ceiling below is now the binding check.)*
///
/// **Published ceiling (the binding check).** Against an IT8 base, IT7 costs
/// 1.5–2×, IT6 2–4×, IT5 4–6×, and IT9–IT11 0.6–0.9×. Our `standard` band *is*
/// IT10–11, so dividing by its 0.75 midpoint gives the ceiling relative to our
/// own baseline: `precision` ≤ ~5.3×, `ultra_precision` ≤ 8.0×, `tight` ≤ ~1.5×.
/// Measured, this seed moves a real quote at most **1.19× (tight), 1.63×
/// (precision), 3.13× (ultra)** — comfortably inside, and if anything
/// *under*-priced against the published multipliers on the callout-only path.
/// Pinned by `pin_seeded_bands_stay_under_the_published_it_grade_ceiling`.
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
    // ── UltraPrecision (≈IT5 and tighter, ±0.01-class): half an extra
    //    whole-part finishing pass at a 1.5 feed factor, doubled inspection, the published 12 % scrap, and
    //    the grinding escalation — this band is where a milled surface stops
    //    holding size and the feature goes to the grinder. That adder is priced
    //    at the `Grinder` machine-rate row seeded by `quoting_machine_rates`.
    Seed {
        band: ToleranceRange::UltraPrecision,
        finish_passes_add: 0.5,
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

    // N7 — `rework_scrap_pct` is a FRACTION (0.05 = 5 %), and the field now
    // carries real money. An operator typing `5` meaning "5 %" was previously
    // accepted silently as a **500 %** uplift on (material + machining). Reject
    // anything above 1.0 and say why: a scrap allowance that exceeds the whole
    // job is a typo, not a shop rate. (Exactly 1.0 stays legal — a 100 % uplift
    // is extreme but coherent for a part expected to be scrapped once.)
    if inputs.rework_scrap_pct.is_finite() && inputs.rework_scrap_pct > MAX_REWORK_SCRAP_PCT {
        errors.push(ValidationError {
            field: "rework_scrap_pct",
            message: format!(
                "A selejt-ráhagyás tört szám, nem százalék: {v} = {p:.0}%. Adjon meg <= {m:.1} értéket (pl. 0.05 = 5%). \
                 / Scrap/rework is a FRACTION, not a percentage: {v} means {p:.0}%. Enter <= {m:.1} (e.g. 0.05 = 5%).",
                v = inputs.rework_scrap_pct,
                p = inputs.rework_scrap_pct * 100.0,
                m = MAX_REWORK_SCRAP_PCT,
            ),
        });
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

const SEED_MARKER_SQL: &str = "
CREATE TABLE IF NOT EXISTS quoting_tolerance_cost_rates_seeded (
    tenant_id VARCHAR NOT NULL PRIMARY KEY,
    seeded_at VARCHAR NOT NULL
);
";

/// Has this tenant's band set ever been laid down? **Tenant-level**, and
/// deliberately *not* inferred from the rows themselves.
///
/// B1 (PR #38 adversarial): per-band absence is the wrong question. Deleting a
/// row is a live SPA button whose own contract is "this band falls back to a
/// zero contribution" — i.e. the operator turning the band **off**. Re-deriving
/// "absent ⇒ never seeded" made the next boot re-insert that band at 12 % scrap
/// with grinding on, re-arming real money against an explicit instruction. It
/// also made the module self-contradictory: a human-written *zero* row was
/// respected but a human *deletion* was not.
///
/// A durable marker row is the honest state. It survives deleting every band
/// (which an operator may legitimately do to opt out of tolerance costing
/// entirely — the empty table is the engine's byte-identical inert path), so
/// the seed lays a band set down exactly once per tenant, ever.
fn tenant_already_seeded(conn: &Connection, tenant: &str) -> Result<bool> {
    conn.execute_batch(SEED_MARKER_SQL)
        .context("ensure quoting_tolerance_cost_rates_seeded marker table")?;
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM quoting_tolerance_cost_rates_seeded WHERE tenant_id = ?;",
            params![tenant],
            |r| r.get(0),
        )
        .context("read tolerance cost-rate seed marker")?;
    Ok(n > 0)
}

/// Lay down the five ADR-0097 bands **once per tenant, ever** (gated on the
/// [`tenant_already_seeded`] marker — NOT on per-band absence, see B1 there),
/// and separately migrate any row still carrying the original T4 all-zero seed
/// ([`upgrade_pristine_zero_seed`], which runs on every boot but only ever
/// touches provably-untouched rows).
///
/// The `loose`/`standard` rows are zero-contribution ⇒ an un-toleranced part
/// prices byte-identically (R4); the three tighter bands carry the researched
/// EU/DE defaults, each stamped with [`SEED_NOTE`] so the row is visibly a seed
/// and not a shop-measured value ([`SEEDS`]).
///
/// Takes `&mut Connection` + a [`LedgerMeta`] because the migration arm moves
/// **priced-money data on an existing tenant** and is audited inside the same
/// transaction as the write (N1). The first-seed arm writes rows that are, by
/// construction, the day-1 state of a tenant that has never priced anything, and
/// is unaudited exactly like every sibling catalogue's boot seed.
pub fn seed_tolerance_cost_rates_if_absent(
    conn: &mut Connection,
    meta: &LedgerMeta,
    tenant: &str,
) -> Result<()> {
    ensure_schema(conn)?;
    let now = now_rfc3339()?;

    if !tenant_already_seeded(conn, tenant)? {
        for seed in SEEDS {
            let band = seed.band.as_db_str();
            // Defensive: a partially-hand-built table must not gain a duplicate.
            let existing: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM quoting_tolerance_cost_rates \
                     WHERE tenant_id = ? AND tolerance_class = ?;",
                    params![tenant, band],
                    |r| r.get(0),
                )
                .context("count quoting_tolerance_cost_rates for seed gate")?;
            if existing > 0 {
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
        conn.execute(
            "INSERT INTO quoting_tolerance_cost_rates_seeded (tenant_id, seeded_at) \
             VALUES (?, ?);",
            params![tenant, &now],
        )
        .context("stamp tolerance cost-rate seed marker")?;
    }

    for seed in SEEDS {
        upgrade_pristine_zero_seed(conn, meta, tenant, seed, &now)?;
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
/// A row failing any gate is left exactly as-is.
///
/// **Audited (N1).** Unlike the first-seed INSERT beside it, this arm rewrites
/// priced-money data on a tenant that has already been quoting — it re-prices
/// every subsequent tight/precision/ultra quote. That must leave a trail, and
/// it does: the UPDATE and its `ParametersChanged` entry share one transaction
/// (`append_in_tx` on the same connection, exactly the CRUD's pattern — no
/// second opener, so CHECK 10M/10N stay clean), with the same self-describing
/// payload shape plus `"op":"tolerance_cost_rate_seed_upgrade"`. The entry is
/// emitted only when the guard actually matched a row, so a boot that changes
/// nothing writes nothing.
fn upgrade_pristine_zero_seed(
    conn: &mut Connection,
    meta: &LedgerMeta,
    tenant: &str,
    seed: &Seed,
    now: &str,
) -> Result<()> {
    let tx = conn
        .transaction()
        .context("begin tolerance cost-rate seed-upgrade tx")?;
    let changed = tx
        .execute(
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
    if changed == 0 {
        // Nothing matched the guard — no write, so nothing to record.
        return Ok(());
    }
    let row = read_in_tx_by_band(&tx, tenant, seed.band.as_db_str())?;
    append_tolerance_cost_rate_change(&tx, meta, "boot", "tolerance_cost_rate_seed_upgrade", &row)?;
    tx.commit()
        .context("commit tolerance cost-rate seed upgrade")?;
    Ok(())
}

/// Read a band's row inside the seed-upgrade tx (the upgrade is keyed by band,
/// not id, so it cannot reuse [`read_in_tx`]).
fn read_in_tx_by_band(
    tx: &duckdb::Transaction<'_>,
    tenant: &str,
    band: &str,
) -> Result<ToleranceCostRateRow> {
    let sql = format!(
        "SELECT {COLS} FROM quoting_tolerance_cost_rates \
         WHERE tenant_id = ? AND tolerance_class = ?;"
    );
    let mut stmt = tx.prepare(&sql)?;
    let mut rows = stmt.query_map(params![tenant, band], row_to_tolerance_cost_rate)?;
    match rows.next() {
        Some(r) => Ok(r?),
        None => Err(anyhow::anyhow!(
            "quoting_tolerance_cost_rates row for band {band} vanished mid-upgrade"
        )),
    }
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
        let mut conn = mem();
        seed_tolerance_cost_rates_if_absent(&mut conn, &ledger_meta(), "t1").expect("seed");

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
        assert_eq!(ultra.finish_passes_add, 0.5);
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
        seed_tolerance_cost_rates_if_absent(&mut conn, &ledger_meta(), "t1").expect("seed");
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

        seed_tolerance_cost_rates_if_absent(&mut conn, &ledger_meta(), "t1").expect("reseed");

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
        let mut conn = mem();
        for band in ["loose", "standard", "tight", "precision", "ultra_precision"] {
            insert_legacy_zero_row(&conn, band, "boot");
        }
        seed_tolerance_cost_rates_if_absent(&mut conn, &ledger_meta(), "t1")
            .expect("seed over legacy rows");

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
        let mut conn = mem();
        insert_legacy_zero_row(&conn, "ultra_precision", "ervin");
        seed_tolerance_cost_rates_if_absent(&mut conn, &ledger_meta(), "t1").expect("seed");

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
        seed_tolerance_cost_rates_if_absent(&mut conn, &ledger_meta(), "t1")
            .expect("first upgrade");

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

        seed_tolerance_cost_rates_if_absent(&mut conn, &ledger_meta(), "t1").expect("second boot");
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
            // ADR-0112 v6 — inert: empty ⇒ no JSON key, no price change.
            located_holes: Vec::new(),
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
        // Magnitude is bounded by `pin_seeded_bands_stay_under_the_published_it_grade_ceiling`,
        // which is scale-free and holds across callout counts. Here we only
        // require that the line is real money rather than a rounding artefact.
        assert!(
            prec.tolerance_cost > 1.0,
            "tolerance_cost {} EUR is a rounding artefact, not a cost driver",
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
    // ══════════════════════════════════════════════════════════════════
    // ADVERSARIAL REVIEW PINS (PR #38) — imported from the review; these
    // are the regression pins for B1 / B2 / N1 and the migration-guard sweep.
    // ══════════════════════════════════════════════════════════════════

    /// The rate slice built from the **live table** (what
    /// `quote_pricing_pipeline::convert_tolerance_cost_rates` does), rather
    /// than from the `SEEDS` constant — so a deleted / edited row is visible
    /// to the engine exactly as production would see it.
    fn engine_rates_from_db(conn: &Connection) -> Vec<ToleranceCostRate> {
        list_tolerance_cost_rates(conn, "t1")
            .expect("list")
            .into_iter()
            .map(|r| ToleranceCostRate {
                tolerance_class: r.tolerance_class,
                finish_passes_add: r.finish_passes_add,
                inproc_inspection_min: r.inproc_inspection_min,
                cmm_min_per_critical_feature: r.cmm_min_per_critical_feature,
                rework_scrap_pct: r.rework_scrap_pct,
                feed_slowdown_factor: r.feed_slowdown_factor,
                grinding_escalation: r.grinding_escalation,
            })
            .collect()
    }

    /// Like the PR's `bracket`, but with an arbitrary number of critical
    /// features — a real drawing rarely carries exactly one callout.
    fn bracket_n(spec: ToleranceSpec, n: usize) -> FeatureGraph {
        let mut fg = bracket(None);
        fg.critical_feature_tolerances = (0..n)
            .map(|i| FeatureTolerance {
                feature_index: i % 2,
                spec,
            })
            .collect();
        fg
    }

    /// Fully parametrised price: caller supplies both rate slices.
    fn price_with(
        fg: &FeatureGraph,
        band: ToleranceRange,
        rates: &[ToleranceCostRate],
        machine: &[MachineRate],
    ) -> QuoteBreakdown {
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
        let gears = Vec::new();
        let snap = CatalogueSnapshot {
            materials: &materials,
            complexity_rules: &rules,
            tolerance_multipliers: &multipliers,
            stock_adjustments: &stock,
            machine_rates: machine,
            gear_process_rates: &gears,
            tolerance_cost_rates: rates,
        };
        quote_with_catalogue(fg, &snap, &params(), 10, band, &CalibrationTable::neutral())
            .expect("quote must price")
    }

    /// MEASUREMENT (always passes) — dumps the real EUR figures each band
    /// produces so the order-of-magnitude claims can be checked against
    /// numbers rather than adjectives.
    #[test]
    fn pin_measure_seeded_prices_across_bands() {
        let mut conn = mem();
        seed_tolerance_cost_rates_if_absent(&mut conn, &ledger_meta(), "t1").expect("seed");
        let rates = engine_rates_from_db(&conn);
        let machine = engine_machine_rates();
        let empty: Vec<ToleranceCostRate> = Vec::new();

        let pre = price_with(&bracket(None), ToleranceRange::Standard, &empty, &machine);
        println!(
            "PRE-SEED baseline (empty table, no callout): total={:.4} material={:.4} machining={:.4} tol={:.4}",
            pre.total_price, pre.material_cost, pre.machining_cost, pre.tolerance_cost
        );

        for (label, spec) in [
            (
                "tight (ISO2768-f = storefront `precision` token)",
                ToleranceSpec::GeneralClass {
                    class: GeneralClass::Iso2768Fine,
                },
            ),
            ("precision (IT7)", ToleranceSpec::ItGrade { grade: 7 }),
            ("ultra (IT4)", ToleranceSpec::ItGrade { grade: 4 }),
        ] {
            for n in [1_usize, 4, 8] {
                let fg = bracket_n(spec, n);
                let before = price_with(&fg, ToleranceRange::Standard, &empty, &machine);
                let after = price_with(&fg, ToleranceRange::Standard, &rates, &machine);
                println!(
                    "{label:<48} n_crit={n}: tol_cost={tc:9.4}  total {b:9.4} -> {a:9.4}  (+{d:.1}%)",
                    tc = after.tolerance_cost,
                    b = before.total_price,
                    a = after.total_price,
                    d = 100.0 * (after.total_price - before.total_price) / before.total_price,
                );
            }
        }

        println!("\n-- STACKED: overall band set (storefront token / operator), which ALSO fires the pre-existing quoting_tolerance_multipliers row --");
        for (label, band) in [
            (
                "tight  (storefront `precision` token)",
                ToleranceRange::Tight,
            ),
            ("precision", ToleranceRange::Precision),
            ("ultra_precision", ToleranceRange::UltraPrecision),
        ] {
            for n in [0_usize, 1, 4] {
                let fg = bracket_n(ToleranceSpec::Unspecified, n);
                let before = price_with(&fg, band, &empty, &machine);
                let after = price_with(&fg, band, &rates, &machine);
                println!(
                    "{label:<40} n_crit={n}: tol_cost={tc:9.4}  machining={mc:8.4}  tol/machining={r:6.1}%  total {b:9.4} -> {a:9.4} (+{d:.1}%)",
                    tc = after.tolerance_cost,
                    mc = after.machining_cost,
                    r = 100.0 * after.tolerance_cost / after.machining_cost,
                    b = before.total_price,
                    a = after.total_price,
                    d = 100.0 * (after.total_price - before.total_price) / before.total_price,
                );
            }
        }
    }

    /// PROBE 1 — the operator deletes a band's row (the SPA's delete button;
    /// the CRUD doc says the band then "falls back to a zero `tolerance_cost`
    /// contribution — no orphaned pricing"). The very next boot silently
    /// re-inserts the researched seed, re-arming 12 % scrap + the grinding
    /// escalation on a band the operator deliberately disabled.
    #[test]
    fn pin_delete_then_boot_resurrects_a_deliberately_disabled_band() {
        let mut conn = mem();
        seed_tolerance_cost_rates_if_absent(&mut conn, &ledger_meta(), "t1").expect("seed");
        let id = band_row(&conn, "ultra_precision").id;
        let meta = ledger_meta();
        delete_tolerance_cost_rate(&mut conn, &meta, "ervin", "t1", &id).expect("operator delete");
        assert!(
            list_tolerance_cost_rates(&conn, "t1")
                .expect("list")
                .iter()
                .all(|r| r.tolerance_class != "ultra_precision"),
            "precondition: the band is gone"
        );

        seed_tolerance_cost_rates_if_absent(&mut conn, &ledger_meta(), "t1").expect("next boot");

        // The band must STAY deleted. Re-inserting it would re-arm 12 % scrap
        // and the grinding escalation against an explicit operator instruction
        // (the SPA's delete button means "turn this band off").
        let resurrected = list_tolerance_cost_rates(&conn, "t1")
            .expect("list")
            .into_iter()
            .find(|r| r.tolerance_class == "ultra_precision");
        assert!(
            resurrected.is_none(),
            "boot resurrected a band the operator deleted: {resurrected:?}"
        );

        // ...and a THIRD boot must not either (the marker is durable, and does
        // not decay back to "never seeded" once every band is gone).
        seed_tolerance_cost_rates_if_absent(&mut conn, &ledger_meta(), "t1").expect("third boot");
        assert!(
            list_tolerance_cost_rates(&conn, "t1")
                .expect("list")
                .iter()
                .all(|r| r.tolerance_class != "ultra_precision"),
            "the seed marker is not durable across boots"
        );
    }

    /// PROBE 2 — the migration UPDATE changes priced-money data on an
    /// existing tenant and appends NOTHING to the audit ledger, while the
    /// identical change made by an operator does.
    #[test]
    fn pin_migration_of_money_data_is_unaudited() {
        fn ledger_len(conn: &Connection) -> i64 {
            conn.query_row("SELECT COUNT(*) FROM audit_ledger;", [], |r| r.get(0))
                .expect("count ledger")
        }

        let mut conn = mem();
        for band in ["loose", "standard", "tight", "precision", "ultra_precision"] {
            insert_legacy_zero_row(&conn, band, "boot");
        }
        let before = ledger_len(&conn);
        seed_tolerance_cost_rates_if_absent(&mut conn, &ledger_meta(), "t1")
            .expect("boot migration");
        let after = ledger_len(&conn);

        assert_eq!(
            band_row(&conn, "ultra_precision").rework_scrap_pct,
            0.12,
            "precondition: the migration did move money"
        );
        assert!(
            after > before,
            "a boot-time UPDATE that re-prices every tolerance quote on an \
             existing tenant left no audit trail: ledger {before} -> {after}"
        );
    }

    /// PROBE 0 — the hostile sweep at the migration guard. Every row here is
    /// one an operator could plausibly own; NONE may be rewritten by the
    /// upgrade. This test PASSES — the guard's predicate holds.
    #[test]
    fn pin_migration_guard_survives_every_evasion_i_could_build() {
        // Each case: (band, actor, notes-SQL, the six drivers) + what must survive.
        let cases: &[(&str, &str, &str, &str)] = &[
            // actor='boot', notes NULL, but ONE driver off the no-op — the
            // "operator tuned a single knob and left the rest" shape.
            ("tight", "boot", "NULL", "0, 0.25, 0, 0, 1.0, FALSE"),
            ("tight", "boot", "NULL", "0.75, 0, 0, 0, 1.0, FALSE"),
            ("tight", "boot", "NULL", "0, 0, 3.0, 0, 1.0, FALSE"),
            ("tight", "boot", "NULL", "0, 0, 0, 0.01, 1.0, FALSE"),
            // feed factor: the validator's floor is 1.0, so 1.0000001 is the
            // smallest legal operator edit — must still block.
            ("tight", "boot", "NULL", "0, 0, 0, 0, 1.0000001, FALSE"),
            // grinding switched on with everything else at the no-op.
            ("tight", "boot", "NULL", "0, 0, 0, 0, 1.0, TRUE"),
            // pristine numbers but a human actor.
            ("tight", "ervin", "NULL", "0, 0, 0, 0, 1.0, FALSE"),
            // pristine numbers, boot actor, but a note exists (incl. the
            // empty string, which is NOT NULL in SQL).
            ("tight", "boot", "''", "0, 0, 0, 0, 1.0, FALSE"),
            (
                "tight",
                "boot",
                "'we run this band at zero'",
                "0, 0, 0, 0, 1.0, FALSE",
            ),
        ];

        for (i, (band, actor, notes, drivers)) in cases.iter().enumerate() {
            let mut conn = mem();
            conn.execute_batch(&format!(
                "INSERT INTO quoting_tolerance_cost_rates (id, tenant_id, tolerance_class, \
                 finish_passes_add, inproc_inspection_min, cmm_min_per_critical_feature, \
                 rework_scrap_pct, feed_slowdown_factor, grinding_escalation, \
                 notes, updated_at, updated_by_actor) \
                 VALUES ('qtcr_case{i}', 't1', '{band}', {drivers}, {notes}, \
                 '2026-01-01T00:00:00Z', '{actor}');"
            ))
            .expect("insert case row");

            let before = band_row(&conn, band);
            seed_tolerance_cost_rates_if_absent(&mut conn, &ledger_meta(), "t1").expect("boot");
            let after = band_row(&conn, band);

            assert_eq!(
                before, after,
                "case {i} ({band}, actor={actor}, notes={notes}, drivers={drivers}) \
                 was rewritten by the seed migration"
            );
        }
    }

    /// B2 / B3 ceiling pin — replaces the PR's original `(0.50..50.0)` EUR
    /// window, which was an absolute figure fitted to ONE fixture at ONE
    /// critical callout and said nothing true about any other shape (it passed
    /// at n=1 and failed at n=4 on the same part).
    ///
    /// The honest invariant is **scale-free and citable**: how far a band may
    /// move the whole quote relative to the same part at the default band,
    /// bounded by published IT-grade cost multipliers.
    ///
    /// Metalworks Plus (see the findings note) puts, against an IT8 base:
    /// IT7 = 1.5–2×, IT6 = 2–4×, IT5 = 4–6×, and IT9–IT11 = 0.6–0.9×. Our
    /// `standard` band IS IT10–11, so dividing through by its 0.75 midpoint
    /// gives the ceiling **relative to our own baseline**: `precision`
    /// (IT6–IT7) ≤ 4/0.75 ≈ 5.3×, `ultra_precision` (IT5 and tighter) ≤
    /// 6/0.75 = 8.0×. `tight` (IT8–IT9) is at most the IT8 base itself,
    /// 1/0.75 ≈ 1.33×, rounded to 1.5× for headroom.
    ///
    /// Checked across a realistic callout count, because the grinding adder is
    /// per-feature: a drawing carries several GD&T boxes, not one.
    #[test]
    fn pin_seeded_bands_stay_under_the_published_it_grade_ceiling() {
        let mut conn = mem();
        seed_tolerance_cost_rates_if_absent(&mut conn, &ledger_meta(), "t1").expect("seed");
        let rates = engine_rates_from_db(&conn);
        let machine = engine_machine_rates();
        let empty: Vec<ToleranceCostRate> = Vec::new();

        let baseline = price_with(&bracket(None), ToleranceRange::Standard, &empty, &machine);

        // (label, spec, ceiling relative to the standard-band baseline)
        let cases: &[(&str, ToleranceSpec, f64)] = &[
            (
                "tight",
                ToleranceSpec::GeneralClass {
                    class: GeneralClass::Iso2768Fine,
                },
                1.5,
            ),
            ("precision", ToleranceSpec::ItGrade { grade: 7 }, 5.3),
            ("ultra_precision", ToleranceSpec::ItGrade { grade: 4 }, 8.0),
        ];

        for (label, spec, ceiling) in cases {
            for n in [1_usize, 4, 8] {
                let q = price_with(
                    &bracket_n(*spec, n),
                    ToleranceRange::Standard,
                    &rates,
                    &machine,
                );
                let ratio = q.total_price / baseline.total_price;
                assert!(
                    q.tolerance_cost > 0.0,
                    "{label} n={n}: a tighter callout must cost something"
                );
                assert!(
                    ratio <= *ceiling,
                    "{label} n={n}: quote moved {ratio:.2}x the default-band baseline, \
                     above the published IT-grade ceiling of {ceiling:.2}x \
                     (tolerance_cost={tc:.4} of total={t:.4})",
                    tc = q.tolerance_cost,
                    t = q.total_price,
                );
                assert!(
                    ratio > 1.0,
                    "{label} n={n}: ratio {ratio:.2} — the seed moved no money at all"
                );
            }
        }

        // The grinding cap must actually bind before the ceiling does: 8
        // callouts is 96 min uncapped, and the cap holds it at 48.
        let ultra8 = price_with(
            &bracket_n(ToleranceSpec::ItGrade { grade: 4 }, 8),
            ToleranceRange::Standard,
            &rates,
            &machine,
        );
        let log = ultra8.reasoning_log.join("\n");
        assert!(
            log.contains("grinding escalation CAPPED"),
            "8 ground callouts must trip the per-part grinding cap:\n{log}"
        );
        // ...and the cap must be a real reduction, not cosmetic.
        let ultra4 = price_with(
            &bracket_n(ToleranceSpec::ItGrade { grade: 4 }, 4),
            ToleranceRange::Standard,
            &rates,
            &machine,
        );
        assert!(
            !ultra4.reasoning_log.join("\n").contains("CAPPED"),
            "4 callouts is exactly the cap and must NOT be reported as capped"
        );
    }

    /// N7 — `rework_scrap_pct` is a fraction. An operator typing `5` meaning
    /// "5 %" must be rejected, not silently accepted as a 500 % uplift on
    /// (material + machining).
    #[test]
    fn pin_percentage_typed_into_the_scrap_fraction_is_rejected() {
        let inputs = |scrap: f64| ToleranceCostRateInputs {
            tolerance_class: "precision".to_string(),
            finish_passes_add: 0.0,
            inproc_inspection_min: 0.0,
            cmm_min_per_critical_feature: 0.0,
            rework_scrap_pct: scrap,
            feed_slowdown_factor: 1.0,
            grinding_escalation: false,
            notes: None,
        };

        // The typo the field invites.
        let errs = validate_tolerance_cost_rate_inputs(&inputs(5.0))
            .expect_err("5.0 (i.e. 500 %) must be rejected");
        assert!(
            errs.iter().any(|e| e.field == "rework_scrap_pct"),
            "the error must name the offending field: {errs:?}"
        );

        // Real values still pass, including the extreme-but-coherent 100 %.
        validate_tolerance_cost_rate_inputs(&inputs(0.05)).expect("5 % is legal");
        validate_tolerance_cost_rate_inputs(&inputs(MAX_REWORK_SCRAP_PCT))
            .expect("exactly 100 % stays legal");
        // ...and the existing lower bound is untouched.
        validate_tolerance_cost_rate_inputs(&inputs(-0.01)).expect_err("negative is rejected");
    }
}
