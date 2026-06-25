//! S267 / PR-256 — the four quoting-engine tunable tables, second
//! cut of the auto-quoting strand (design doc §11 / Appendix).
//!
//! Each table is operator-tunable from the SPA; every CRUD write is
//! audited with a row-snapshot payload so per-row history can be
//! reconstructed from the ledger (same posture S266's
//! [`crate::quoting_materials`] uses).
//!
//! The four tables sit on top of the materials catalogue (S266) and
//! feed the future `aberp-quote-engine` (S268+). They are
//! quoting-engine internals — **NONE** of them push to the storefront.
//! Only [`crate::quoting_materials`]'s public projection does (design
//! §14-C).
//!
//! Tables shipped here:
//!
//! | Table | PK | Audit kind |
//! |---|---|---|
//! | `quoting_complexity_rules` | `id` (sequence) — composite `(feature_type, size_bucket, count_min)` enforced in app | [`EventKind::ComplexityRulesChanged`] |
//! | `quoting_tolerance_multipliers` | `tolerance_range` (closed-vocab enum) | [`EventKind::ToleranceMultipliersChanged`] |
//! | `quoting_parameters` | `id = 1` (singleton; insert-blocked beyond row 1) | [`EventKind::ParametersChanged`] |
//! | `quoting_stock_adjustments` | `id` (sequence) — composite `(grade, stock_status)` enforced in app | [`EventKind::StockAdjustmentsChanged`] |
//!
//! Conventions inherited from `quoting_materials` (deliberately, for
//! reviewability):
//! - **`[[no-sql-specific]]`**: plain columns + PRIMARY KEY only; no
//!   CHECK, no triggers, no FK declarations. Invariants in Rust.
//! - **`[[trust-code-not-operator]]`**: every CRUD write appends an
//!   audit entry in the same transaction as the data write.
//! - **Hard delete** (no soft-delete) — the future engine reads a
//!   fresh snapshot per quote, so deleted rules disappear.
//! - **Timestamps** as RFC3339 `VARCHAR` (matches `quoting_materials`).
//!
//! Pushbacks applied (full list in the PR body):
//! 1. **Stock adjustments key on `grade` VARCHAR, not `material_id BIGINT`.**
//!    [`crate::quoting_materials`] uses `grade VARCHAR` as its PRIMARY
//!    KEY — there's no surrogate `id BIGINT`. Routing the FK through
//!    `grade` keeps the existing PK contract intact instead of
//!    bolting on a surrogate that nothing else uses.
//! 2. **`quoting_parameters` is a singleton** (one row, fixed `id = 1`).
//!    Justified: only one set of global knobs ever exists. A
//!    key-value-pairs table would lose type safety on the `DOUBLE`
//!    columns; a singleton with named columns is clearer.
//! 3. **Size-bucket boundaries are hardcoded** in [`SizeBucket::range_mm`],
//!    not tunable. The buckets define the *shape* of the rules table;
//!    tuning is done per-row via `base_time_minutes` / `multiplier` /
//!    `setup_penalty_minutes`.
//! 4. **No new material `Exotic` enum variant.** S266's
//!    `quoting_materials` schema is `grade VARCHAR` (no `category`
//!    column, no closed-vocab category enum). The
//!    `exotic_material_tax` parameter is exposed here for the future
//!    engine, but **how** the engine identifies a row as exotic
//!    (likely via an explicit `is_exotic` boolean column when S268
//!    needs it) is deferred. S266's enum is untouched.

use anyhow::{Context, Result};
use duckdb::{params, Connection};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use aberp_audit_ledger::{append_in_tx, Actor, EventKind, LedgerMeta};
use ulid::Ulid;

// ── Closed-vocab enums (validated in Rust per `no-sql-specific`) ────────

/// The feature-type axis of [`ComplexityRule`]. Closed-vocab; the v1
/// list is the first eight feature categories the future
/// `aberp-quote-engine` will weight. Extensible by adding a variant
/// here + the `as_db_str`/`from_db_str` arms — adding a variant
/// without updating both arms is a compile error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeatureType {
    Pocket,
    Hole,
    Slot,
    Thread,
    Undercut5Axis,
    ThinWall,
    Surface,
    Engraving,
}

impl FeatureType {
    pub fn as_db_str(&self) -> &'static str {
        match self {
            FeatureType::Pocket => "pocket",
            FeatureType::Hole => "hole",
            FeatureType::Slot => "slot",
            FeatureType::Thread => "thread",
            FeatureType::Undercut5Axis => "undercut_5axis",
            FeatureType::ThinWall => "thin_wall",
            FeatureType::Surface => "surface",
            FeatureType::Engraving => "engraving",
        }
    }
    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "pocket" => Some(FeatureType::Pocket),
            "hole" => Some(FeatureType::Hole),
            "slot" => Some(FeatureType::Slot),
            "thread" => Some(FeatureType::Thread),
            "undercut_5axis" => Some(FeatureType::Undercut5Axis),
            "thin_wall" => Some(FeatureType::ThinWall),
            "surface" => Some(FeatureType::Surface),
            "engraving" => Some(FeatureType::Engraving),
            _ => None,
        }
    }
    pub const ALL: [FeatureType; 8] = [
        FeatureType::Pocket,
        FeatureType::Hole,
        FeatureType::Slot,
        FeatureType::Thread,
        FeatureType::Undercut5Axis,
        FeatureType::ThinWall,
        FeatureType::Surface,
        FeatureType::Engraving,
    ];
}

/// The size-bucket axis of [`ComplexityRule`]. Closed-vocab; the
/// boundaries are hardcoded in [`SizeBucket::range_mm`] (pushback in
/// module docs — buckets define the rules-table *shape*, not a
/// tunable parameter).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeBucket {
    Xs,
    S,
    M,
    L,
    Xl,
}

impl SizeBucket {
    pub fn as_db_str(&self) -> &'static str {
        match self {
            SizeBucket::Xs => "XS",
            SizeBucket::S => "S",
            SizeBucket::M => "M",
            SizeBucket::L => "L",
            SizeBucket::Xl => "XL",
        }
    }
    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "XS" => Some(SizeBucket::Xs),
            "S" => Some(SizeBucket::S),
            "M" => Some(SizeBucket::M),
            "L" => Some(SizeBucket::L),
            "XL" => Some(SizeBucket::Xl),
            _ => None,
        }
    }
    pub const ALL: [SizeBucket; 5] = [
        SizeBucket::Xs,
        SizeBucket::S,
        SizeBucket::M,
        SizeBucket::L,
        SizeBucket::Xl,
    ];

    /// v1 bucket → millimetre half-open range `[min, max)`. `None`
    /// upper bound means open-ended (XL ≥ 200mm). The future quote
    /// engine reads this map to bucket an extracted feature's size.
    pub fn range_mm(&self) -> (f64, Option<f64>) {
        match self {
            SizeBucket::Xs => (0.0, Some(10.0)),
            SizeBucket::S => (10.0, Some(30.0)),
            SizeBucket::M => (30.0, Some(80.0)),
            SizeBucket::L => (80.0, Some(200.0)),
            SizeBucket::Xl => (200.0, None),
        }
    }
}

/// Tolerance bands feeding [`ToleranceMultiplier`]. Closed-vocab; the
/// mapping from a numeric CAD tolerance to a band is deferred to the
/// Python/Rust CAD extractor (S269/S270). This module only exposes the
/// enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToleranceRange {
    Loose,
    Standard,
    Tight,
    Precision,
    UltraPrecision,
}

impl ToleranceRange {
    pub fn as_db_str(&self) -> &'static str {
        match self {
            ToleranceRange::Loose => "loose",
            ToleranceRange::Standard => "standard",
            ToleranceRange::Tight => "tight",
            ToleranceRange::Precision => "precision",
            ToleranceRange::UltraPrecision => "ultra_precision",
        }
    }
    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "loose" => Some(ToleranceRange::Loose),
            "standard" => Some(ToleranceRange::Standard),
            "tight" => Some(ToleranceRange::Tight),
            "precision" => Some(ToleranceRange::Precision),
            "ultra_precision" => Some(ToleranceRange::UltraPrecision),
            _ => None,
        }
    }
    pub const ALL: [ToleranceRange; 5] = [
        ToleranceRange::Loose,
        ToleranceRange::Standard,
        ToleranceRange::Tight,
        ToleranceRange::Precision,
        ToleranceRange::UltraPrecision,
    ];
}

// ── Wire shapes ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct ComplexityRuleInputs {
    #[serde(default)]
    pub feature_type: String,
    #[serde(default)]
    pub size_bucket: String,
    #[serde(default)]
    pub count_min: i64,
    #[serde(default)]
    pub count_max: Option<i64>,
    #[serde(default)]
    pub base_time_minutes: f64,
    #[serde(default = "one")]
    pub multiplier: f64,
    #[serde(default)]
    pub setup_penalty_minutes: f64,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ComplexityRule {
    /// App-minted prefixed ULID (`qcr_…`). S410 / [[no-sql-specific]] —
    /// was a DB-sequence `BIGINT`; the PK is now minted in Rust.
    pub id: String,
    pub feature_type: String,
    pub size_bucket: String,
    pub count_min: i64,
    pub count_max: Option<i64>,
    pub base_time_minutes: f64,
    pub multiplier: f64,
    pub setup_penalty_minutes: f64,
    pub notes: Option<String>,
    pub updated_at: String,
    pub updated_by_actor: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ToleranceMultiplierInputs {
    #[serde(default)]
    pub tolerance_range: String,
    #[serde(default = "one")]
    pub multiplier: f64,
    #[serde(default)]
    pub inspection_minutes_per_feature: f64,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToleranceMultiplier {
    pub tolerance_range: String,
    pub multiplier: f64,
    pub inspection_minutes_per_feature: f64,
    pub notes: Option<String>,
    pub updated_at: String,
    pub updated_by_actor: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct QuotingParametersInputs {
    #[serde(default = "default_scrap_factor")]
    pub scrap_factor: f64,
    #[serde(default = "default_profit_margin_base")]
    pub profit_margin_base: f64,
    #[serde(default = "default_overhead_factor")]
    pub overhead_factor: f64,
    #[serde(default = "default_setup_amortization_threshold")]
    pub setup_amortization_threshold: i64,
    #[serde(default = "default_min_margin")]
    pub min_margin: f64,
    #[serde(default = "default_exotic_material_tax")]
    pub exotic_material_tax: f64,
    // S418 geometry-model knobs (report §8.1).
    #[serde(default = "default_machining_rate")]
    pub machining_rate_eur_per_minute: f64,
    #[serde(default = "default_cad_cam_rate")]
    pub cad_cam_rate_eur_per_hour: f64,
    #[serde(default = "default_cad_cam_base_hours")]
    pub cad_cam_base_hours: f64,
    #[serde(default = "default_mrr_rough_ref")]
    pub mrr_rough_ref_cm3_per_min: f64,
    #[serde(default = "default_t_finish")]
    pub t_finish_min_per_cm2: f64,
    #[serde(default = "default_setup_base_min")]
    pub setup_base_min: f64,
    #[serde(default = "default_setup_5axis_min")]
    pub setup_5axis_min: f64,
    // ADR-0094 Gap 2 (S4) — bar-feeder capacity routing tunable.
    #[serde(default = "default_bar_capacity_mm")]
    pub bar_capacity_mm: f64,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QuotingParameters {
    pub scrap_factor: f64,
    pub profit_margin_base: f64,
    pub overhead_factor: f64,
    pub setup_amortization_threshold: i64,
    pub min_margin: f64,
    pub exotic_material_tax: f64,
    /// S418 — geometry-model knobs (report §8.1).
    pub machining_rate_eur_per_minute: f64,
    pub cad_cam_rate_eur_per_hour: f64,
    pub cad_cam_base_hours: f64,
    pub mrr_rough_ref_cm3_per_min: f64,
    pub t_finish_min_per_cm2: f64,
    pub setup_base_min: f64,
    pub setup_5axis_min: f64,
    /// ADR-0094 Gap 2 (S4) — bar-feeder capacity (mm) for engine routing.
    pub bar_capacity_mm: f64,
    pub notes: Option<String>,
    pub updated_at: String,
    pub updated_by_actor: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StockAdjustmentInputs {
    #[serde(default)]
    pub grade: String,
    #[serde(default)]
    pub stock_status: String,
    #[serde(default)]
    pub price_adjustment_pct: f64,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StockAdjustment {
    /// App-minted prefixed ULID (`qsa_…`). S410 / [[no-sql-specific]] —
    /// was a DB-sequence `BIGINT`; the PK is now minted in Rust.
    pub id: String,
    pub grade: String,
    pub stock_status: String,
    pub price_adjustment_pct: f64,
    pub notes: Option<String>,
    pub updated_at: String,
    pub updated_by_actor: String,
}

fn one() -> f64 {
    1.0
}
fn default_scrap_factor() -> f64 {
    // S418 — repurposed as the stock-oversize fraction (report §6.4);
    // day-1 0.15 (was 0.08 material-scrap).
    0.15
}
fn default_profit_margin_base() -> f64 {
    0.35
}
fn default_overhead_factor() -> f64 {
    0.20
}
fn default_setup_amortization_threshold() -> i64 {
    5
}
fn default_min_margin() -> f64 {
    0.10
}
fn default_exotic_material_tax() -> f64 {
    0.05
}
// S418 geometry-model knob defaults (report §8.1).
fn default_machining_rate() -> f64 {
    1.6667 // 100 EUR/machine-hour
}
fn default_cad_cam_rate() -> f64 {
    100.0
}
fn default_cad_cam_base_hours() -> f64 {
    1.0
}
fn default_mrr_rough_ref() -> f64 {
    8.0
}
fn default_t_finish() -> f64 {
    0.08
}
fn default_setup_base_min() -> f64 {
    20.0
}
fn default_setup_5axis_min() -> f64 {
    25.0
}

/// ADR-0094 Gap 2 (S4) — serde/seed default for `bar_capacity_mm`: the
/// largest bar-stock diameter the shop's bar-fed Swiss/turn-mill accepts.
/// 32 mm is a common bar-feeder capacity (engine default mirrored).
fn default_bar_capacity_mm() -> f64 {
    32.0
}

#[derive(Serialize, Debug, PartialEq, Eq, Clone)]
pub struct ValidationError {
    pub field: &'static str,
    pub message: String,
}

#[derive(Debug)]
pub enum TunableWriteError {
    Validation(Vec<ValidationError>),
    /// Composite-unique conflict for `quoting_complexity_rules` /
    /// `quoting_stock_adjustments`. Carries a human-readable description
    /// of the colliding key.
    Conflict(String),
    /// Update/delete of a row that does not exist.
    NotFound(String),
    Other(anyhow::Error),
}

impl From<anyhow::Error> for TunableWriteError {
    fn from(e: anyhow::Error) -> Self {
        TunableWriteError::Other(e)
    }
}

// ── Schema ──────────────────────────────────────────────────────────────

// S410 / [[no-sql-specific]] — no `CREATE SEQUENCE` / `DEFAULT nextval()`.
// `CREATE SEQUENCE` is a DuckDB/Postgres-only object with no portable
// equivalent (SQLite / Elasticsearch have none); it was the only
// engine-minted identity in the codebase. The PK is now an app-minted
// prefixed ULID (`qcr_…`), like every other table (`inv_…`, `mvt_…`,
// `dsp_…`), generated in `create_complexity_rule`.
const COMPLEXITY_RULES_SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS quoting_complexity_rules (
    id                    VARCHAR NOT NULL PRIMARY KEY,
    tenant_id             VARCHAR NOT NULL,
    feature_type          VARCHAR NOT NULL,
    size_bucket           VARCHAR NOT NULL,
    count_min             INTEGER NOT NULL,
    count_max             INTEGER,
    base_time_minutes     DOUBLE  NOT NULL,
    multiplier            DOUBLE  NOT NULL DEFAULT 1.0,
    setup_penalty_minutes DOUBLE  NOT NULL DEFAULT 0.0,
    notes                 VARCHAR,
    updated_at            VARCHAR NOT NULL,
    updated_by_actor      VARCHAR NOT NULL
);
";

const TOLERANCE_MULTIPLIERS_SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS quoting_tolerance_multipliers (
    tolerance_range                VARCHAR NOT NULL PRIMARY KEY,
    tenant_id                      VARCHAR NOT NULL,
    multiplier                     DOUBLE  NOT NULL DEFAULT 1.0,
    inspection_minutes_per_feature DOUBLE  NOT NULL DEFAULT 0.0,
    notes                          VARCHAR,
    updated_at                     VARCHAR NOT NULL,
    updated_by_actor               VARCHAR NOT NULL
);
";

const PARAMETERS_SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS quoting_parameters (
    id                            INTEGER NOT NULL PRIMARY KEY,
    tenant_id                     VARCHAR NOT NULL,
    scrap_factor                  DOUBLE  NOT NULL DEFAULT 0.15,
    profit_margin_base            DOUBLE  NOT NULL DEFAULT 0.35,
    overhead_factor               DOUBLE  NOT NULL DEFAULT 0.20,
    setup_amortization_threshold  INTEGER NOT NULL DEFAULT 5,
    min_margin                    DOUBLE  NOT NULL DEFAULT 0.10,
    exotic_material_tax           DOUBLE  NOT NULL DEFAULT 0.05,
    machining_rate_eur_per_minute DOUBLE  NOT NULL DEFAULT 1.6667,
    cad_cam_rate_eur_per_hour     DOUBLE  NOT NULL DEFAULT 100.0,
    cad_cam_base_hours            DOUBLE  NOT NULL DEFAULT 1.0,
    mrr_rough_ref_cm3_per_min     DOUBLE  NOT NULL DEFAULT 8.0,
    t_finish_min_per_cm2          DOUBLE  NOT NULL DEFAULT 0.08,
    setup_base_min                DOUBLE  NOT NULL DEFAULT 20.0,
    setup_5axis_min               DOUBLE  NOT NULL DEFAULT 25.0,
    bar_capacity_mm               DOUBLE  NOT NULL DEFAULT 32.0,
    notes                         VARCHAR,
    updated_at                    VARCHAR NOT NULL,
    updated_by_actor              VARCHAR NOT NULL
);
";

/// S418 — promote the geometry-model knobs onto the parameters
/// singleton. The pre-S418 pipeline hardcoded the machining rate (1.0)
/// and had no CAD-CAM / MRR / finishing / setup knobs at all; the
/// geometry model (report §5) needs them operator-tunable.
///
/// **Replay-safe per the DEFAULT-on-replay trap** (see
/// `quoting_materials::QUOTING_MATERIALS_SCHEMA_SQL`): the ADDs carry NO
/// `DEFAULT` (a replay can never clobber a tuned value); the backfill
/// ([`backfill_s418_parameters`]) writes only columns still NULL. The
/// fresh-install `CREATE TABLE` already has these columns + defaults, so
/// every ALTER here no-ops on a new tenant.
const S418_PARAMETERS_ADD_SQL: &str = "
ALTER TABLE quoting_parameters ADD COLUMN IF NOT EXISTS machining_rate_eur_per_minute DOUBLE;
ALTER TABLE quoting_parameters ADD COLUMN IF NOT EXISTS cad_cam_rate_eur_per_hour DOUBLE;
ALTER TABLE quoting_parameters ADD COLUMN IF NOT EXISTS cad_cam_base_hours DOUBLE;
ALTER TABLE quoting_parameters ADD COLUMN IF NOT EXISTS mrr_rough_ref_cm3_per_min DOUBLE;
ALTER TABLE quoting_parameters ADD COLUMN IF NOT EXISTS t_finish_min_per_cm2 DOUBLE;
ALTER TABLE quoting_parameters ADD COLUMN IF NOT EXISTS setup_base_min DOUBLE;
ALTER TABLE quoting_parameters ADD COLUMN IF NOT EXISTS setup_5axis_min DOUBLE;
";

/// ADR-0094 Gap 2 (S4) — add the `bar_capacity_mm` routing tunable to any
/// pre-ADR-0094 parameters row. Replay-safe (DEFAULT-on-replay trap): the
/// ADD carries NO `DEFAULT`, and `ensure_schema` backfills only the still-
/// NULL column to 32.0 — a tuned value is never clobbered. Fresh installs
/// already have the column + default from `CREATE TABLE`.
const ADR0094_PARAMETERS_ADD_SQL: &str = "
ALTER TABLE quoting_parameters ADD COLUMN IF NOT EXISTS bar_capacity_mm DOUBLE;
";

// S410 / [[no-sql-specific]] — see `COMPLEXITY_RULES_SCHEMA_SQL` above.
// App-minted prefixed ULID (`qsa_…`), no `CREATE SEQUENCE` / `nextval`.
const STOCK_ADJUSTMENTS_SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS quoting_stock_adjustments (
    id                    VARCHAR NOT NULL PRIMARY KEY,
    tenant_id             VARCHAR NOT NULL,
    grade                 VARCHAR NOT NULL,
    stock_status          VARCHAR NOT NULL,
    price_adjustment_pct  DOUBLE  NOT NULL,
    notes                 VARCHAR,
    updated_at            VARCHAR NOT NULL,
    updated_by_actor      VARCHAR NOT NULL
);
";

/// Boot-time schema migration for the four tables + the parameters
/// singleton seed. Idempotent: re-runs against a migrated DB cost
/// nothing.
///
/// The tolerance-multipliers and parameters tables are seeded if
/// empty so a fresh tenant has reasonable defaults to tune. The
/// complexity-rules and stock-adjustments tables stay empty — the
/// operator builds them up over time, there is no defensible seed.
pub fn ensure_schema(conn: &mut Connection, tenant: &str) -> Result<()> {
    conn.execute_batch(COMPLEXITY_RULES_SCHEMA_SQL)
        .context("ensure quoting_complexity_rules schema")?;
    conn.execute_batch(TOLERANCE_MULTIPLIERS_SCHEMA_SQL)
        .context("ensure quoting_tolerance_multipliers schema")?;
    conn.execute_batch(PARAMETERS_SCHEMA_SQL)
        .context("ensure quoting_parameters schema")?;
    conn.execute_batch(S418_PARAMETERS_ADD_SQL)
        .context("apply S418 quoting_parameters knob columns")?;
    backfill_s418_parameters(conn).context("backfill S418 quoting_parameters knobs")?;
    conn.execute_batch(ADR0094_PARAMETERS_ADD_SQL)
        .context("apply ADR-0094 bar_capacity_mm column")?;
    conn.execute(
        "UPDATE quoting_parameters SET bar_capacity_mm = ? WHERE bar_capacity_mm IS NULL;",
        params![default_bar_capacity_mm()],
    )
    .context("backfill quoting_parameters.bar_capacity_mm")?;
    conn.execute_batch(STOCK_ADJUSTMENTS_SCHEMA_SQL)
        .context("ensure quoting_stock_adjustments schema")?;
    seed_tolerance_if_empty(conn, tenant)?;
    seed_parameters_if_empty(conn, tenant)?;
    Ok(())
}

/// Backfill the S418 geometry-model knobs onto any pre-S418 parameters
/// row (the migrated-but-NULL state). Replay-safe: each `IS NULL` guard
/// means an operator-tuned value is never overwritten.
///
/// `scrap_factor` is special: it pre-existed (default 0.08, material
/// scrap) and is repurposed as the stock-oversize fraction (day-1
/// 0.15). The guarded bump `WHERE scrap_factor = 0.08` migrates the
/// untouched old default to the new one. KNOWN CAVEAT (flagged in the
/// S418 report): an operator who *deliberately* re-enters exactly 0.08
/// would be re-bumped to 0.15 on the next boot — acceptable because
/// nobody sets a stock-oversize margin to the old material-scrap value.
fn backfill_s418_parameters(conn: &Connection) -> Result<()> {
    let knobs: &[(&str, f64)] = &[
        ("machining_rate_eur_per_minute", default_machining_rate()),
        ("cad_cam_rate_eur_per_hour", default_cad_cam_rate()),
        ("cad_cam_base_hours", default_cad_cam_base_hours()),
        ("mrr_rough_ref_cm3_per_min", default_mrr_rough_ref()),
        ("t_finish_min_per_cm2", default_t_finish()),
        ("setup_base_min", default_setup_base_min()),
        ("setup_5axis_min", default_setup_5axis_min()),
    ];
    for (col, val) in knobs {
        conn.execute(
            &format!("UPDATE quoting_parameters SET {col} = ? WHERE {col} IS NULL;"),
            params![val],
        )
        .with_context(|| format!("backfill quoting_parameters.{col}"))?;
    }
    // Repurpose the old material-scrap default to the new stock-oversize
    // default (one-time; see fn doc for the caveat).
    conn.execute(
        "UPDATE quoting_parameters SET scrap_factor = ? WHERE scrap_factor = 0.08;",
        params![default_scrap_factor()],
    )
    .context("migrate quoting_parameters.scrap_factor 0.08 → stock-oversize default")?;
    Ok(())
}

fn seed_tolerance_if_empty(conn: &mut Connection, tenant: &str) -> Result<()> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM quoting_tolerance_multipliers",
            [],
            |r| r.get(0),
        )
        .context("count quoting_tolerance_multipliers for seed gate")?;
    if count > 0 {
        return Ok(());
    }
    // (range, multiplier, inspection_minutes_per_feature)
    let seeds: &[(ToleranceRange, f64, f64)] = &[
        (ToleranceRange::Loose, 0.9, 0.0),
        (ToleranceRange::Standard, 1.0, 0.0),
        (ToleranceRange::Tight, 1.4, 0.5),
        (ToleranceRange::Precision, 1.9, 1.5),
        (ToleranceRange::UltraPrecision, 2.8, 3.0),
    ];
    let now = now_rfc3339()?;
    let tx = conn.transaction().context("begin tolerance seed tx")?;
    for (range, mult, insp) in seeds {
        tx.execute(
            "INSERT INTO quoting_tolerance_multipliers (
                tolerance_range, tenant_id, multiplier,
                inspection_minutes_per_feature, notes,
                updated_at, updated_by_actor
             ) VALUES (?, ?, ?, ?, NULL, ?, 'boot');",
            params![range.as_db_str(), tenant, mult, insp, &now],
        )
        .with_context(|| {
            format!(
                "seed quoting_tolerance_multipliers row {}",
                range.as_db_str()
            )
        })?;
    }
    tx.commit().context("commit tolerance seed")?;
    Ok(())
}

fn seed_parameters_if_empty(conn: &mut Connection, tenant: &str) -> Result<()> {
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM quoting_parameters", [], |r| r.get(0))
        .context("count quoting_parameters for seed gate")?;
    if count > 0 {
        return Ok(());
    }
    let now = now_rfc3339()?;
    conn.execute(
        "INSERT INTO quoting_parameters (
            id, tenant_id, scrap_factor, profit_margin_base, overhead_factor,
            setup_amortization_threshold, min_margin, exotic_material_tax,
            machining_rate_eur_per_minute, cad_cam_rate_eur_per_hour, cad_cam_base_hours,
            mrr_rough_ref_cm3_per_min, t_finish_min_per_cm2, setup_base_min, setup_5axis_min,
            bar_capacity_mm, notes, updated_at, updated_by_actor
         ) VALUES (1, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, ?, 'boot');",
        params![
            tenant,
            default_scrap_factor(),
            default_profit_margin_base(),
            default_overhead_factor(),
            default_setup_amortization_threshold(),
            default_min_margin(),
            default_exotic_material_tax(),
            default_machining_rate(),
            default_cad_cam_rate(),
            default_cad_cam_base_hours(),
            default_mrr_rough_ref(),
            default_t_finish(),
            default_setup_base_min(),
            default_setup_5axis_min(),
            default_bar_capacity_mm(),
            &now,
        ],
    )
    .context("seed quoting_parameters singleton")?;
    Ok(())
}

// ── Validation helpers ──────────────────────────────────────────────────

fn check_positive(errors: &mut Vec<ValidationError>, field: &'static str, v: f64) {
    if !v.is_finite() || v <= 0.0 {
        errors.push(ValidationError {
            field,
            message: format!("`{field}` must be a finite number > 0 (got {v})"),
        });
    }
}

fn check_non_negative(errors: &mut Vec<ValidationError>, field: &'static str, v: f64) {
    if !v.is_finite() || v < 0.0 {
        errors.push(ValidationError {
            field,
            message: format!("`{field}` must be a finite number >= 0 (got {v})"),
        });
    }
}

fn check_finite(errors: &mut Vec<ValidationError>, field: &'static str, v: f64) {
    if !v.is_finite() {
        errors.push(ValidationError {
            field,
            message: format!("`{field}` must be a finite number (got {v})"),
        });
    }
}

// ── Complexity rules: validation ────────────────────────────────────────

pub fn validate_complexity_rule(inputs: &ComplexityRuleInputs) -> Result<(), Vec<ValidationError>> {
    let mut errs = Vec::new();
    if FeatureType::from_db_str(inputs.feature_type.trim()).is_none() {
        errs.push(ValidationError {
            field: "feature_type",
            message: format!(
                "Ismeretlen jellemző-típus `{}` / Unknown feature_type (expected one of: {})",
                inputs.feature_type,
                FeatureType::ALL
                    .iter()
                    .map(|s| s.as_db_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        });
    }
    if SizeBucket::from_db_str(inputs.size_bucket.trim()).is_none() {
        errs.push(ValidationError {
            field: "size_bucket",
            message: format!(
                "Ismeretlen mérettartomány `{}` / Unknown size_bucket (expected one of: {})",
                inputs.size_bucket,
                SizeBucket::ALL
                    .iter()
                    .map(|s| s.as_db_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        });
    }
    if inputs.count_min < 0 {
        errs.push(ValidationError {
            field: "count_min",
            message: "count_min must be >= 0".to_string(),
        });
    }
    if let Some(max) = inputs.count_max {
        if max <= inputs.count_min {
            errs.push(ValidationError {
                field: "count_max",
                message: format!(
                    "count_max ({max}) must be > count_min ({}); use NULL for unbounded",
                    inputs.count_min
                ),
            });
        }
    }
    check_non_negative(&mut errs, "base_time_minutes", inputs.base_time_minutes);
    check_positive(&mut errs, "multiplier", inputs.multiplier);
    check_non_negative(
        &mut errs,
        "setup_penalty_minutes",
        inputs.setup_penalty_minutes,
    );
    if errs.is_empty() {
        Ok(())
    } else {
        Err(errs)
    }
}

// ── Complexity rules: CRUD ──────────────────────────────────────────────

pub fn list_complexity_rules(conn: &Connection, tenant: &str) -> Result<Vec<ComplexityRule>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, feature_type, size_bucket, count_min, count_max,
                    base_time_minutes, multiplier, setup_penalty_minutes,
                    notes, updated_at, updated_by_actor
             FROM quoting_complexity_rules
             WHERE tenant_id = ?
             ORDER BY feature_type, size_bucket, count_min;",
        )
        .context("prepare list_complexity_rules")?;
    let rows = stmt
        .query_map(params![tenant], row_to_complexity_rule)
        .context("query complexity_rules")?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.context("read complexity_rule row")?);
    }
    Ok(out)
}

pub fn create_complexity_rule(
    conn: &mut Connection,
    meta: &LedgerMeta,
    actor_login: &str,
    tenant: &str,
    inputs: &ComplexityRuleInputs,
) -> Result<ComplexityRule, TunableWriteError> {
    if let Err(e) = validate_complexity_rule(inputs) {
        return Err(TunableWriteError::Validation(e));
    }
    let ft = inputs.feature_type.trim().to_string();
    let sb = inputs.size_bucket.trim().to_string();
    // Composite-unique check in app layer.
    let existing: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM quoting_complexity_rules
             WHERE tenant_id = ? AND feature_type = ? AND size_bucket = ? AND count_min = ?;",
            params![tenant, &ft, &sb, inputs.count_min],
            |r| r.get(0),
        )
        .context("check complexity_rule composite uniqueness")
        .map_err(TunableWriteError::Other)?;
    if existing > 0 {
        return Err(TunableWriteError::Conflict(format!(
            "({ft}, {sb}, count_min={}) already exists",
            inputs.count_min
        )));
    }
    let now = now_rfc3339().map_err(TunableWriteError::Other)?;
    let notes = normalize_optional(inputs.notes.as_deref());
    let tx = conn
        .transaction()
        .context("begin create_complexity_rule tx")
        .map_err(TunableWriteError::Other)?;
    // S410 / [[no-sql-specific]] — app-mint the PK (prefixed ULID), no
    // DB sequence. ULID is globally unique, so no read-back-by-natural-key
    // round-trip is needed to learn the id.
    let id = format!("qcr_{}", Ulid::new());
    tx.execute(
        "INSERT INTO quoting_complexity_rules (
            id, tenant_id, feature_type, size_bucket, count_min, count_max,
            base_time_minutes, multiplier, setup_penalty_minutes,
            notes, updated_at, updated_by_actor
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?);",
        params![
            &id,
            tenant,
            &ft,
            &sb,
            inputs.count_min,
            inputs.count_max,
            inputs.base_time_minutes,
            inputs.multiplier,
            inputs.setup_penalty_minutes,
            notes.as_deref(),
            &now,
            actor_login,
        ],
    )
    .context("INSERT quoting_complexity_rules")
    .map_err(TunableWriteError::Other)?;
    let row = read_complexity_rule_in_tx(&tx, tenant, &id).map_err(TunableWriteError::Other)?;
    append_tunable_change(
        &tx,
        meta,
        actor_login,
        EventKind::ComplexityRulesChanged,
        "create",
        &serde_json::json!({
            "id": row.id,
            "feature_type": row.feature_type,
            "size_bucket": row.size_bucket,
            "count_min": row.count_min,
            "row": row,
        }),
    )
    .map_err(TunableWriteError::Other)?;
    tx.commit()
        .context("commit create_complexity_rule")
        .map_err(TunableWriteError::Other)?;
    Ok(row)
}

pub fn update_complexity_rule(
    conn: &mut Connection,
    meta: &LedgerMeta,
    actor_login: &str,
    tenant: &str,
    id: &str,
    inputs: &ComplexityRuleInputs,
) -> Result<ComplexityRule, TunableWriteError> {
    if let Err(e) = validate_complexity_rule(inputs) {
        return Err(TunableWriteError::Validation(e));
    }
    let ft = inputs.feature_type.trim().to_string();
    let sb = inputs.size_bucket.trim().to_string();
    // Composite-unique check against OTHER rows.
    let clash: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM quoting_complexity_rules
             WHERE tenant_id = ? AND feature_type = ? AND size_bucket = ? AND count_min = ?
               AND id <> ?;",
            params![tenant, &ft, &sb, inputs.count_min, id],
            |r| r.get(0),
        )
        .context("check complexity_rule composite uniqueness (update)")
        .map_err(TunableWriteError::Other)?;
    if clash > 0 {
        return Err(TunableWriteError::Conflict(format!(
            "({ft}, {sb}, count_min={}) already exists on another row",
            inputs.count_min
        )));
    }
    let now = now_rfc3339().map_err(TunableWriteError::Other)?;
    let notes = normalize_optional(inputs.notes.as_deref());
    let tx = conn
        .transaction()
        .context("begin update_complexity_rule tx")
        .map_err(TunableWriteError::Other)?;
    let changed = tx
        .execute(
            "UPDATE quoting_complexity_rules SET
                feature_type          = ?,
                size_bucket           = ?,
                count_min             = ?,
                count_max             = ?,
                base_time_minutes     = ?,
                multiplier            = ?,
                setup_penalty_minutes = ?,
                notes                 = ?,
                updated_at            = ?,
                updated_by_actor      = ?
             WHERE tenant_id = ? AND id = ?;",
            params![
                &ft,
                &sb,
                inputs.count_min,
                inputs.count_max,
                inputs.base_time_minutes,
                inputs.multiplier,
                inputs.setup_penalty_minutes,
                notes.as_deref(),
                &now,
                actor_login,
                tenant,
                id,
            ],
        )
        .context("UPDATE quoting_complexity_rules")
        .map_err(TunableWriteError::Other)?;
    if changed == 0 {
        return Err(TunableWriteError::NotFound(format!("id {id}")));
    }
    let row = read_complexity_rule_in_tx(&tx, tenant, id).map_err(TunableWriteError::Other)?;
    append_tunable_change(
        &tx,
        meta,
        actor_login,
        EventKind::ComplexityRulesChanged,
        "update",
        &serde_json::json!({
            "id": row.id,
            "feature_type": row.feature_type,
            "size_bucket": row.size_bucket,
            "count_min": row.count_min,
            "row": row,
        }),
    )
    .map_err(TunableWriteError::Other)?;
    tx.commit()
        .context("commit update_complexity_rule")
        .map_err(TunableWriteError::Other)?;
    Ok(row)
}

pub fn delete_complexity_rule(
    conn: &mut Connection,
    meta: &LedgerMeta,
    actor_login: &str,
    tenant: &str,
    id: &str,
) -> Result<(), TunableWriteError> {
    let tx = conn
        .transaction()
        .context("begin delete_complexity_rule tx")
        .map_err(TunableWriteError::Other)?;
    let row =
        match read_complexity_rule_in_tx_opt(&tx, tenant, id).map_err(TunableWriteError::Other)? {
            Some(r) => r,
            None => return Err(TunableWriteError::NotFound(format!("id {id}"))),
        };
    tx.execute(
        "DELETE FROM quoting_complexity_rules WHERE tenant_id = ? AND id = ?;",
        params![tenant, id],
    )
    .context("DELETE quoting_complexity_rules")
    .map_err(TunableWriteError::Other)?;
    append_tunable_change(
        &tx,
        meta,
        actor_login,
        EventKind::ComplexityRulesChanged,
        "delete",
        &serde_json::json!({
            "id": row.id,
            "feature_type": row.feature_type,
            "size_bucket": row.size_bucket,
            "count_min": row.count_min,
            "row": row,
        }),
    )
    .map_err(TunableWriteError::Other)?;
    tx.commit()
        .context("commit delete_complexity_rule")
        .map_err(TunableWriteError::Other)?;
    Ok(())
}

// ── Tolerance multipliers: validation + CRUD ────────────────────────────

pub fn validate_tolerance_multiplier(
    inputs: &ToleranceMultiplierInputs,
) -> Result<(), Vec<ValidationError>> {
    let mut errs = Vec::new();
    if ToleranceRange::from_db_str(inputs.tolerance_range.trim()).is_none() {
        errs.push(ValidationError {
            field: "tolerance_range",
            message: format!(
                "Ismeretlen tűréstartomány `{}` / Unknown tolerance_range (expected one of: {})",
                inputs.tolerance_range,
                ToleranceRange::ALL
                    .iter()
                    .map(|s| s.as_db_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        });
    }
    check_positive(&mut errs, "multiplier", inputs.multiplier);
    check_non_negative(
        &mut errs,
        "inspection_minutes_per_feature",
        inputs.inspection_minutes_per_feature,
    );
    if errs.is_empty() {
        Ok(())
    } else {
        Err(errs)
    }
}

pub fn list_tolerance_multipliers(
    conn: &Connection,
    tenant: &str,
) -> Result<Vec<ToleranceMultiplier>> {
    let mut stmt = conn
        .prepare(
            "SELECT tolerance_range, multiplier, inspection_minutes_per_feature,
                    notes, updated_at, updated_by_actor
             FROM quoting_tolerance_multipliers
             WHERE tenant_id = ?
             ORDER BY tolerance_range;",
        )
        .context("prepare list_tolerance_multipliers")?;
    let rows = stmt
        .query_map(params![tenant], row_to_tolerance_multiplier)
        .context("query tolerance_multipliers")?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.context("read tolerance_multiplier row")?);
    }
    Ok(out)
}

/// Update-in-place (the closed-vocab `tolerance_range` PK is the
/// natural key). Returns `NotFound` if the row does not exist.
///
/// There is no CREATE / DELETE for this table: the closed-vocab enum
/// is exhaustive and pre-seeded at boot. Operator deletes the row
/// would leave a hole the engine would fail to find at quote time.
pub fn update_tolerance_multiplier(
    conn: &mut Connection,
    meta: &LedgerMeta,
    actor_login: &str,
    tenant: &str,
    tolerance_range: &str,
    inputs: &ToleranceMultiplierInputs,
) -> Result<ToleranceMultiplier, TunableWriteError> {
    if let Err(e) = validate_tolerance_multiplier(inputs) {
        return Err(TunableWriteError::Validation(e));
    }
    let range = ToleranceRange::from_db_str(tolerance_range.trim())
        .ok_or_else(|| TunableWriteError::NotFound(format!("tolerance_range `{tolerance_range}`")))?
        .as_db_str();
    // Path PK and body PK must match — the body's `tolerance_range`
    // is otherwise authoritative only on create paths (which don't
    // exist here).
    let body_range = ToleranceRange::from_db_str(inputs.tolerance_range.trim());
    if let Some(b) = body_range {
        if b.as_db_str() != range {
            return Err(TunableWriteError::Validation(vec![ValidationError {
                field: "tolerance_range",
                message: format!(
                    "URL `tolerance_range` ({range}) does not match body ({})",
                    b.as_db_str()
                ),
            }]));
        }
    }
    let now = now_rfc3339().map_err(TunableWriteError::Other)?;
    let notes = normalize_optional(inputs.notes.as_deref());
    let tx = conn
        .transaction()
        .context("begin update_tolerance_multiplier tx")
        .map_err(TunableWriteError::Other)?;
    let changed = tx
        .execute(
            "UPDATE quoting_tolerance_multipliers SET
                multiplier                     = ?,
                inspection_minutes_per_feature = ?,
                notes                          = ?,
                updated_at                     = ?,
                updated_by_actor               = ?
             WHERE tenant_id = ? AND tolerance_range = ?;",
            params![
                inputs.multiplier,
                inputs.inspection_minutes_per_feature,
                notes.as_deref(),
                &now,
                actor_login,
                tenant,
                range,
            ],
        )
        .context("UPDATE quoting_tolerance_multipliers")
        .map_err(TunableWriteError::Other)?;
    if changed == 0 {
        return Err(TunableWriteError::NotFound(format!(
            "tolerance_range `{range}`"
        )));
    }
    let row =
        read_tolerance_multiplier_in_tx(&tx, tenant, range).map_err(TunableWriteError::Other)?;
    append_tunable_change(
        &tx,
        meta,
        actor_login,
        EventKind::ToleranceMultipliersChanged,
        "update",
        &serde_json::json!({
            "tolerance_range": row.tolerance_range,
            "row": row,
        }),
    )
    .map_err(TunableWriteError::Other)?;
    tx.commit()
        .context("commit update_tolerance_multiplier")
        .map_err(TunableWriteError::Other)?;
    Ok(row)
}

// ── Parameters (singleton): validation + CRUD ───────────────────────────

pub fn validate_parameters(inputs: &QuotingParametersInputs) -> Result<(), Vec<ValidationError>> {
    let mut errs = Vec::new();
    check_non_negative(&mut errs, "scrap_factor", inputs.scrap_factor);
    check_non_negative(&mut errs, "profit_margin_base", inputs.profit_margin_base);
    check_non_negative(&mut errs, "overhead_factor", inputs.overhead_factor);
    if inputs.setup_amortization_threshold < 1 {
        errs.push(ValidationError {
            field: "setup_amortization_threshold",
            message: "setup_amortization_threshold must be >= 1".to_string(),
        });
    }
    check_non_negative(&mut errs, "min_margin", inputs.min_margin);
    check_non_negative(&mut errs, "exotic_material_tax", inputs.exotic_material_tax);
    // S418 geometry-model knobs. Rates/divisors must be strictly
    // positive (a zero rate zeroes the line; a zero MRR divides by
    // zero); base hours / setup minutes only non-negative (0 = "no
    // floor / no setup", a legitimate operator choice). `check_positive`
    // also rejects NaN/Inf (fail loud).
    check_positive(
        &mut errs,
        "machining_rate_eur_per_minute",
        inputs.machining_rate_eur_per_minute,
    );
    check_positive(
        &mut errs,
        "cad_cam_rate_eur_per_hour",
        inputs.cad_cam_rate_eur_per_hour,
    );
    check_positive(
        &mut errs,
        "mrr_rough_ref_cm3_per_min",
        inputs.mrr_rough_ref_cm3_per_min,
    );
    check_non_negative(&mut errs, "cad_cam_base_hours", inputs.cad_cam_base_hours);
    check_non_negative(
        &mut errs,
        "t_finish_min_per_cm2",
        inputs.t_finish_min_per_cm2,
    );
    check_non_negative(&mut errs, "setup_base_min", inputs.setup_base_min);
    check_non_negative(&mut errs, "setup_5axis_min", inputs.setup_5axis_min);
    // ADR-0094 Gap 2 (S4) — bar capacity must be a finite, positive mm.
    check_positive(&mut errs, "bar_capacity_mm", inputs.bar_capacity_mm);
    // min_margin must not exceed profit_margin_base — otherwise the
    // floor is above the base and every quote would be rejected.
    if inputs.min_margin.is_finite()
        && inputs.profit_margin_base.is_finite()
        && inputs.min_margin > inputs.profit_margin_base
    {
        errs.push(ValidationError {
            field: "min_margin",
            message: format!(
                "min_margin ({}) cannot exceed profit_margin_base ({})",
                inputs.min_margin, inputs.profit_margin_base
            ),
        });
    }
    if errs.is_empty() {
        Ok(())
    } else {
        Err(errs)
    }
}

pub fn get_parameters(conn: &Connection, tenant: &str) -> Result<QuotingParameters> {
    let mut stmt = conn
        .prepare(
            "SELECT scrap_factor, profit_margin_base, overhead_factor,
                    setup_amortization_threshold, min_margin, exotic_material_tax,
                    machining_rate_eur_per_minute, cad_cam_rate_eur_per_hour, cad_cam_base_hours,
                    mrr_rough_ref_cm3_per_min, t_finish_min_per_cm2, setup_base_min, setup_5axis_min,
                    notes, updated_at, updated_by_actor, bar_capacity_mm
             FROM quoting_parameters
             WHERE tenant_id = ? AND id = 1;",
        )
        .context("prepare get_parameters")?;
    let mut rows = stmt
        .query_map(params![tenant], row_to_parameters)
        .context("query quoting_parameters")?;
    match rows.next() {
        Some(r) => Ok(r.context("read parameters row")?),
        None => Err(anyhow::anyhow!(
            "quoting_parameters singleton missing for tenant {tenant} — should have been seeded at boot"
        )),
    }
}

pub fn update_parameters(
    conn: &mut Connection,
    meta: &LedgerMeta,
    actor_login: &str,
    tenant: &str,
    inputs: &QuotingParametersInputs,
) -> Result<QuotingParameters, TunableWriteError> {
    if let Err(e) = validate_parameters(inputs) {
        return Err(TunableWriteError::Validation(e));
    }
    let now = now_rfc3339().map_err(TunableWriteError::Other)?;
    let notes = normalize_optional(inputs.notes.as_deref());
    let tx = conn
        .transaction()
        .context("begin update_parameters tx")
        .map_err(TunableWriteError::Other)?;
    let changed = tx
        .execute(
            "UPDATE quoting_parameters SET
                scrap_factor                  = ?,
                profit_margin_base            = ?,
                overhead_factor               = ?,
                setup_amortization_threshold  = ?,
                min_margin                    = ?,
                exotic_material_tax           = ?,
                machining_rate_eur_per_minute = ?,
                cad_cam_rate_eur_per_hour     = ?,
                cad_cam_base_hours            = ?,
                mrr_rough_ref_cm3_per_min     = ?,
                t_finish_min_per_cm2          = ?,
                setup_base_min                = ?,
                setup_5axis_min               = ?,
                bar_capacity_mm               = ?,
                notes                         = ?,
                updated_at                    = ?,
                updated_by_actor              = ?
             WHERE tenant_id = ? AND id = 1;",
            params![
                inputs.scrap_factor,
                inputs.profit_margin_base,
                inputs.overhead_factor,
                inputs.setup_amortization_threshold,
                inputs.min_margin,
                inputs.exotic_material_tax,
                inputs.machining_rate_eur_per_minute,
                inputs.cad_cam_rate_eur_per_hour,
                inputs.cad_cam_base_hours,
                inputs.mrr_rough_ref_cm3_per_min,
                inputs.t_finish_min_per_cm2,
                inputs.setup_base_min,
                inputs.setup_5axis_min,
                inputs.bar_capacity_mm,
                notes.as_deref(),
                &now,
                actor_login,
                tenant,
            ],
        )
        .context("UPDATE quoting_parameters")
        .map_err(TunableWriteError::Other)?;
    if changed == 0 {
        return Err(TunableWriteError::NotFound(
            "quoting_parameters singleton (id=1) missing — boot seed required".to_string(),
        ));
    }
    let row = read_parameters_in_tx(&tx, tenant).map_err(TunableWriteError::Other)?;
    append_tunable_change(
        &tx,
        meta,
        actor_login,
        EventKind::ParametersChanged,
        "update",
        &serde_json::json!({ "row": row }),
    )
    .map_err(TunableWriteError::Other)?;
    tx.commit()
        .context("commit update_parameters")
        .map_err(TunableWriteError::Other)?;
    Ok(row)
}

// ── Stock adjustments: validation + CRUD ────────────────────────────────

/// Validate a stock-adjustment row's free fields. The `grade` value is
/// validated against the live `quoting_materials` PK by the caller
/// (the route layer) because that requires a connection, not the
/// input struct alone.
pub fn validate_stock_adjustment(
    inputs: &StockAdjustmentInputs,
) -> Result<(), Vec<ValidationError>> {
    let mut errs = Vec::new();
    if inputs.grade.trim().is_empty() {
        errs.push(ValidationError {
            field: "grade",
            message: "Az anyagminőség kötelező / Material grade is required".to_string(),
        });
    }
    if crate::quoting_materials::StockStatus::from_db_str(inputs.stock_status.trim()).is_none() {
        errs.push(ValidationError {
            field: "stock_status",
            message: format!(
                "Ismeretlen készlet-állapot `{}` / Unknown stock_status (expected one of: {})",
                inputs.stock_status,
                crate::quoting_materials::StockStatus::ALL
                    .iter()
                    .map(|s| s.as_db_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        });
    }
    // The pct can be negative (discount) OR positive (surcharge); just
    // require finite, and clamp the absolute sanity to <= 100% to
    // catch fat-finger input.
    check_finite(
        &mut errs,
        "price_adjustment_pct",
        inputs.price_adjustment_pct,
    );
    if inputs.price_adjustment_pct.is_finite() && inputs.price_adjustment_pct.abs() > 1.0 {
        errs.push(ValidationError {
            field: "price_adjustment_pct",
            message: format!(
                "price_adjustment_pct ({}) outside the ±100% sanity range — expected a fraction like 0.05 for +5%, NOT a percentage",
                inputs.price_adjustment_pct
            ),
        });
    }
    if errs.is_empty() {
        Ok(())
    } else {
        Err(errs)
    }
}

fn check_grade_exists(conn: &Connection, tenant: &str, grade: &str) -> Result<bool> {
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM quoting_materials WHERE tenant_id = ? AND grade = ?;",
            params![tenant, grade],
            |r| r.get(0),
        )
        .context("check quoting_materials grade existence")?;
    Ok(n > 0)
}

pub fn list_stock_adjustments(conn: &Connection, tenant: &str) -> Result<Vec<StockAdjustment>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, grade, stock_status, price_adjustment_pct,
                    notes, updated_at, updated_by_actor
             FROM quoting_stock_adjustments
             WHERE tenant_id = ?
             ORDER BY grade, stock_status;",
        )
        .context("prepare list_stock_adjustments")?;
    let rows = stmt
        .query_map(params![tenant], row_to_stock_adjustment)
        .context("query stock_adjustments")?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.context("read stock_adjustment row")?);
    }
    Ok(out)
}

pub fn create_stock_adjustment(
    conn: &mut Connection,
    meta: &LedgerMeta,
    actor_login: &str,
    tenant: &str,
    inputs: &StockAdjustmentInputs,
) -> Result<StockAdjustment, TunableWriteError> {
    if let Err(e) = validate_stock_adjustment(inputs) {
        return Err(TunableWriteError::Validation(e));
    }
    let grade = inputs.grade.trim().to_string();
    let status = crate::quoting_materials::StockStatus::from_db_str(inputs.stock_status.trim())
        .expect("validated above")
        .as_db_str();
    if !check_grade_exists(conn, tenant, &grade).map_err(TunableWriteError::Other)? {
        return Err(TunableWriteError::Validation(vec![ValidationError {
            field: "grade",
            message: format!(
                "`{grade}` not found in quoting_materials — create the material first"
            ),
        }]));
    }
    let existing: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM quoting_stock_adjustments
             WHERE tenant_id = ? AND grade = ? AND stock_status = ?;",
            params![tenant, &grade, status],
            |r| r.get(0),
        )
        .context("check stock_adjustment composite uniqueness")
        .map_err(TunableWriteError::Other)?;
    if existing > 0 {
        return Err(TunableWriteError::Conflict(format!(
            "({grade}, {status}) already exists"
        )));
    }
    let now = now_rfc3339().map_err(TunableWriteError::Other)?;
    let notes = normalize_optional(inputs.notes.as_deref());
    let tx = conn
        .transaction()
        .context("begin create_stock_adjustment tx")
        .map_err(TunableWriteError::Other)?;
    // S410 / [[no-sql-specific]] — app-mint the PK (prefixed ULID), no
    // DB sequence; ULID uniqueness removes the read-back-by-natural-key.
    let id = format!("qsa_{}", Ulid::new());
    tx.execute(
        "INSERT INTO quoting_stock_adjustments (
            id, tenant_id, grade, stock_status, price_adjustment_pct,
            notes, updated_at, updated_by_actor
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?);",
        params![
            &id,
            tenant,
            &grade,
            status,
            inputs.price_adjustment_pct,
            notes.as_deref(),
            &now,
            actor_login,
        ],
    )
    .context("INSERT quoting_stock_adjustments")
    .map_err(TunableWriteError::Other)?;
    let row = read_stock_adjustment_in_tx(&tx, tenant, &id).map_err(TunableWriteError::Other)?;
    append_tunable_change(
        &tx,
        meta,
        actor_login,
        EventKind::StockAdjustmentsChanged,
        "create",
        &serde_json::json!({
            "id": row.id,
            "grade": row.grade,
            "stock_status": row.stock_status,
            "row": row,
        }),
    )
    .map_err(TunableWriteError::Other)?;
    tx.commit()
        .context("commit create_stock_adjustment")
        .map_err(TunableWriteError::Other)?;
    Ok(row)
}

pub fn update_stock_adjustment(
    conn: &mut Connection,
    meta: &LedgerMeta,
    actor_login: &str,
    tenant: &str,
    id: &str,
    inputs: &StockAdjustmentInputs,
) -> Result<StockAdjustment, TunableWriteError> {
    if let Err(e) = validate_stock_adjustment(inputs) {
        return Err(TunableWriteError::Validation(e));
    }
    let grade = inputs.grade.trim().to_string();
    let status = crate::quoting_materials::StockStatus::from_db_str(inputs.stock_status.trim())
        .expect("validated above")
        .as_db_str();
    if !check_grade_exists(conn, tenant, &grade).map_err(TunableWriteError::Other)? {
        return Err(TunableWriteError::Validation(vec![ValidationError {
            field: "grade",
            message: format!("`{grade}` not found in quoting_materials"),
        }]));
    }
    let clash: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM quoting_stock_adjustments
             WHERE tenant_id = ? AND grade = ? AND stock_status = ? AND id <> ?;",
            params![tenant, &grade, status, id],
            |r| r.get(0),
        )
        .context("check stock_adjustment composite uniqueness (update)")
        .map_err(TunableWriteError::Other)?;
    if clash > 0 {
        return Err(TunableWriteError::Conflict(format!(
            "({grade}, {status}) already exists on another row"
        )));
    }
    let now = now_rfc3339().map_err(TunableWriteError::Other)?;
    let notes = normalize_optional(inputs.notes.as_deref());
    let tx = conn
        .transaction()
        .context("begin update_stock_adjustment tx")
        .map_err(TunableWriteError::Other)?;
    let changed = tx
        .execute(
            "UPDATE quoting_stock_adjustments SET
                grade                = ?,
                stock_status         = ?,
                price_adjustment_pct = ?,
                notes                = ?,
                updated_at           = ?,
                updated_by_actor     = ?
             WHERE tenant_id = ? AND id = ?;",
            params![
                &grade,
                status,
                inputs.price_adjustment_pct,
                notes.as_deref(),
                &now,
                actor_login,
                tenant,
                id,
            ],
        )
        .context("UPDATE quoting_stock_adjustments")
        .map_err(TunableWriteError::Other)?;
    if changed == 0 {
        return Err(TunableWriteError::NotFound(format!("id {id}")));
    }
    let row = read_stock_adjustment_in_tx(&tx, tenant, id).map_err(TunableWriteError::Other)?;
    append_tunable_change(
        &tx,
        meta,
        actor_login,
        EventKind::StockAdjustmentsChanged,
        "update",
        &serde_json::json!({
            "id": row.id,
            "grade": row.grade,
            "stock_status": row.stock_status,
            "row": row,
        }),
    )
    .map_err(TunableWriteError::Other)?;
    tx.commit()
        .context("commit update_stock_adjustment")
        .map_err(TunableWriteError::Other)?;
    Ok(row)
}

pub fn delete_stock_adjustment(
    conn: &mut Connection,
    meta: &LedgerMeta,
    actor_login: &str,
    tenant: &str,
    id: &str,
) -> Result<(), TunableWriteError> {
    let tx = conn
        .transaction()
        .context("begin delete_stock_adjustment tx")
        .map_err(TunableWriteError::Other)?;
    let row =
        match read_stock_adjustment_in_tx_opt(&tx, tenant, id).map_err(TunableWriteError::Other)? {
            Some(r) => r,
            None => return Err(TunableWriteError::NotFound(format!("id {id}"))),
        };
    tx.execute(
        "DELETE FROM quoting_stock_adjustments WHERE tenant_id = ? AND id = ?;",
        params![tenant, id],
    )
    .context("DELETE quoting_stock_adjustments")
    .map_err(TunableWriteError::Other)?;
    append_tunable_change(
        &tx,
        meta,
        actor_login,
        EventKind::StockAdjustmentsChanged,
        "delete",
        &serde_json::json!({
            "id": row.id,
            "grade": row.grade,
            "stock_status": row.stock_status,
            "row": row,
        }),
    )
    .map_err(TunableWriteError::Other)?;
    tx.commit()
        .context("commit delete_stock_adjustment")
        .map_err(TunableWriteError::Other)?;
    Ok(())
}

// ── Internals ───────────────────────────────────────────────────────────

fn append_tunable_change(
    tx: &duckdb::Transaction<'_>,
    meta: &LedgerMeta,
    actor_login: &str,
    kind: EventKind,
    op: &str,
    snapshot: &serde_json::Value,
) -> Result<()> {
    let payload = serde_json::json!({
        "op": op,
        "snapshot": snapshot,
        "idempotency_key": Ulid::new().to_string(),
    });
    let bytes = serde_json::to_vec(&payload).context("serialize tunable change audit payload")?;
    let actor = Actor::from_local_cli(Ulid::new().to_string(), actor_login);
    append_in_tx(tx, meta, kind, bytes, actor, None).context("audit append tunable change")?;
    Ok(())
}

fn row_to_complexity_rule(row: &duckdb::Row<'_>) -> duckdb::Result<ComplexityRule> {
    Ok(ComplexityRule {
        id: row.get(0)?,
        feature_type: row.get(1)?,
        size_bucket: row.get(2)?,
        count_min: row.get(3)?,
        count_max: row.get(4)?,
        base_time_minutes: row.get(5)?,
        multiplier: row.get(6)?,
        setup_penalty_minutes: row.get(7)?,
        notes: row.get(8)?,
        updated_at: row.get(9)?,
        updated_by_actor: row.get(10)?,
    })
}

fn read_complexity_rule_in_tx(
    tx: &duckdb::Transaction<'_>,
    tenant: &str,
    id: &str,
) -> Result<ComplexityRule> {
    read_complexity_rule_in_tx_opt(tx, tenant, id)?
        .with_context(|| format!("complexity_rule row vanished mid-tx for id {id}"))
}

fn read_complexity_rule_in_tx_opt(
    tx: &duckdb::Transaction<'_>,
    tenant: &str,
    id: &str,
) -> Result<Option<ComplexityRule>> {
    let mut stmt = tx.prepare(
        "SELECT id, feature_type, size_bucket, count_min, count_max,
                base_time_minutes, multiplier, setup_penalty_minutes,
                notes, updated_at, updated_by_actor
         FROM quoting_complexity_rules
         WHERE tenant_id = ? AND id = ?;",
    )?;
    let mut rows = stmt.query_map(params![tenant, id], row_to_complexity_rule)?;
    match rows.next() {
        Some(r) => Ok(Some(r?)),
        None => Ok(None),
    }
}

fn row_to_tolerance_multiplier(row: &duckdb::Row<'_>) -> duckdb::Result<ToleranceMultiplier> {
    Ok(ToleranceMultiplier {
        tolerance_range: row.get(0)?,
        multiplier: row.get(1)?,
        inspection_minutes_per_feature: row.get(2)?,
        notes: row.get(3)?,
        updated_at: row.get(4)?,
        updated_by_actor: row.get(5)?,
    })
}

fn read_tolerance_multiplier_in_tx(
    tx: &duckdb::Transaction<'_>,
    tenant: &str,
    range: &str,
) -> Result<ToleranceMultiplier> {
    let mut stmt = tx.prepare(
        "SELECT tolerance_range, multiplier, inspection_minutes_per_feature,
                notes, updated_at, updated_by_actor
         FROM quoting_tolerance_multipliers
         WHERE tenant_id = ? AND tolerance_range = ?;",
    )?;
    let mut rows = stmt.query_map(params![tenant, range], row_to_tolerance_multiplier)?;
    match rows.next() {
        Some(r) => Ok(r?),
        None => Err(anyhow::anyhow!(
            "tolerance_multiplier row vanished mid-tx for range {range}"
        )),
    }
}

fn row_to_parameters(row: &duckdb::Row<'_>) -> duckdb::Result<QuotingParameters> {
    // S418 knobs are NULL-safe: a row migrated but not yet backfilled
    // reads the day-1 default rather than erroring (defence-in-depth;
    // the backfill in `ensure_schema` fills them first in practice).
    Ok(QuotingParameters {
        scrap_factor: row.get(0)?,
        profit_margin_base: row.get(1)?,
        overhead_factor: row.get(2)?,
        setup_amortization_threshold: row.get(3)?,
        min_margin: row.get(4)?,
        exotic_material_tax: row.get(5)?,
        machining_rate_eur_per_minute: row
            .get::<_, Option<f64>>(6)?
            .unwrap_or_else(default_machining_rate),
        cad_cam_rate_eur_per_hour: row
            .get::<_, Option<f64>>(7)?
            .unwrap_or_else(default_cad_cam_rate),
        cad_cam_base_hours: row
            .get::<_, Option<f64>>(8)?
            .unwrap_or_else(default_cad_cam_base_hours),
        mrr_rough_ref_cm3_per_min: row
            .get::<_, Option<f64>>(9)?
            .unwrap_or_else(default_mrr_rough_ref),
        t_finish_min_per_cm2: row
            .get::<_, Option<f64>>(10)?
            .unwrap_or_else(default_t_finish),
        setup_base_min: row
            .get::<_, Option<f64>>(11)?
            .unwrap_or_else(default_setup_base_min),
        setup_5axis_min: row
            .get::<_, Option<f64>>(12)?
            .unwrap_or_else(default_setup_5axis_min),
        notes: row.get(13)?,
        updated_at: row.get(14)?,
        updated_by_actor: row.get(15)?,
        // ADR-0094 Gap 2 (S4) — NULL-safe: a migrated-but-not-yet-backfilled
        // row reads the 32.0 default rather than erroring.
        bar_capacity_mm: row
            .get::<_, Option<f64>>(16)?
            .unwrap_or_else(default_bar_capacity_mm),
    })
}

fn read_parameters_in_tx(tx: &duckdb::Transaction<'_>, tenant: &str) -> Result<QuotingParameters> {
    let mut stmt = tx.prepare(
        "SELECT scrap_factor, profit_margin_base, overhead_factor,
                setup_amortization_threshold, min_margin, exotic_material_tax,
                machining_rate_eur_per_minute, cad_cam_rate_eur_per_hour, cad_cam_base_hours,
                mrr_rough_ref_cm3_per_min, t_finish_min_per_cm2, setup_base_min, setup_5axis_min,
                notes, updated_at, updated_by_actor, bar_capacity_mm
         FROM quoting_parameters
         WHERE tenant_id = ? AND id = 1;",
    )?;
    let mut rows = stmt.query_map(params![tenant], row_to_parameters)?;
    match rows.next() {
        Some(r) => Ok(r?),
        None => Err(anyhow::anyhow!(
            "quoting_parameters singleton missing for tenant {tenant} mid-tx"
        )),
    }
}

fn row_to_stock_adjustment(row: &duckdb::Row<'_>) -> duckdb::Result<StockAdjustment> {
    Ok(StockAdjustment {
        id: row.get(0)?,
        grade: row.get(1)?,
        stock_status: row.get(2)?,
        price_adjustment_pct: row.get(3)?,
        notes: row.get(4)?,
        updated_at: row.get(5)?,
        updated_by_actor: row.get(6)?,
    })
}

fn read_stock_adjustment_in_tx(
    tx: &duckdb::Transaction<'_>,
    tenant: &str,
    id: &str,
) -> Result<StockAdjustment> {
    read_stock_adjustment_in_tx_opt(tx, tenant, id)?
        .with_context(|| format!("stock_adjustment row vanished mid-tx for id {id}"))
}

fn read_stock_adjustment_in_tx_opt(
    tx: &duckdb::Transaction<'_>,
    tenant: &str,
    id: &str,
) -> Result<Option<StockAdjustment>> {
    let mut stmt = tx.prepare(
        "SELECT id, grade, stock_status, price_adjustment_pct,
                notes, updated_at, updated_by_actor
         FROM quoting_stock_adjustments
         WHERE tenant_id = ? AND id = ?;",
    )?;
    let mut rows = stmt.query_map(params![tenant, id], row_to_stock_adjustment)?;
    match rows.next() {
        Some(r) => Ok(Some(r?)),
        None => Ok(None),
    }
}

fn normalize_optional(s: Option<&str>) -> Option<String> {
    match s {
        Some(v) if !v.trim().is_empty() => Some(v.trim().to_string()),
        _ => None,
    }
}

fn now_rfc3339() -> Result<String> {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .context("format updated_at as Rfc3339")
}

#[cfg(test)]
mod tests {
    use super::*;
    use aberp_audit_ledger::{
        ensure_schema as audit_ensure_schema, BinaryHash, LedgerMeta, TenantId,
    };

    const TENANT: &str = "tnt_test";

    fn meta() -> LedgerMeta {
        LedgerMeta::new(
            TenantId::new(TENANT).expect("tenant id"),
            BinaryHash::from_bytes([0u8; 32]),
        )
    }

    fn conn() -> Connection {
        let mut c = Connection::open_in_memory().expect("open in-memory");
        audit_ensure_schema(&c).expect("audit schema");
        // S266 materials schema needed for stock_adjustments FK check.
        crate::quoting_materials::ensure_schema(&c).expect("materials schema");
        ensure_schema(&mut c, TENANT).expect("tunables schema + seed");
        c
    }

    #[test]
    fn closed_vocabs_round_trip() {
        for v in FeatureType::ALL {
            assert_eq!(FeatureType::from_db_str(v.as_db_str()), Some(v));
        }
        for v in SizeBucket::ALL {
            assert_eq!(SizeBucket::from_db_str(v.as_db_str()), Some(v));
        }
        for v in ToleranceRange::ALL {
            assert_eq!(ToleranceRange::from_db_str(v.as_db_str()), Some(v));
        }
        assert_eq!(FeatureType::from_db_str("nope"), None);
        assert_eq!(SizeBucket::from_db_str("xs"), None); // case-sensitive
        assert_eq!(ToleranceRange::from_db_str("Loose"), None);
    }

    #[test]
    fn size_bucket_ranges_are_monotonic_and_xl_open_ended() {
        let mut prev_max = 0.0;
        for b in SizeBucket::ALL {
            let (lo, hi) = b.range_mm();
            assert_eq!(lo, prev_max, "buckets should be contiguous on the lo end");
            match hi {
                Some(h) => {
                    assert!(h > lo, "hi must exceed lo");
                    prev_max = h;
                }
                None => {
                    assert_eq!(b, SizeBucket::Xl, "only XL is open-ended");
                }
            }
        }
    }

    #[test]
    fn complexity_rule_validation_catches_bad_inputs() {
        let bad_ft = ComplexityRuleInputs {
            feature_type: "drilling".to_string(),
            size_bucket: "M".to_string(),
            count_min: 1,
            count_max: None,
            base_time_minutes: 1.0,
            multiplier: 1.0,
            setup_penalty_minutes: 0.0,
            notes: None,
        };
        assert!(validate_complexity_rule(&bad_ft).is_err());

        let bad_count = ComplexityRuleInputs {
            feature_type: "hole".to_string(),
            size_bucket: "M".to_string(),
            count_min: 5,
            count_max: Some(3),
            base_time_minutes: 1.0,
            multiplier: 1.0,
            setup_penalty_minutes: 0.0,
            notes: None,
        };
        assert!(validate_complexity_rule(&bad_count).is_err());

        let bad_mult = ComplexityRuleInputs {
            feature_type: "hole".to_string(),
            size_bucket: "M".to_string(),
            count_min: 1,
            count_max: None,
            base_time_minutes: 1.0,
            multiplier: 0.0,
            setup_penalty_minutes: 0.0,
            notes: None,
        };
        assert!(validate_complexity_rule(&bad_mult).is_err());
    }

    #[test]
    fn complexity_rule_crud_round_trip_with_audit() {
        let mut c = conn();
        let m = meta();
        let r = ComplexityRuleInputs {
            feature_type: "hole".to_string(),
            size_bucket: "S".to_string(),
            count_min: 1,
            count_max: Some(10),
            base_time_minutes: 0.5,
            multiplier: 1.0,
            setup_penalty_minutes: 0.0,
            notes: Some("baseline hole".to_string()),
        };
        let created = create_complexity_rule(&mut c, &m, "ervin", TENANT, &r).expect("create");
        assert_eq!(created.feature_type, "hole");
        // S410 — app-minted prefixed ULID, not a positive integer.
        assert!(created.id.starts_with("qcr_"));

        // Duplicate composite key → Conflict.
        let dup = create_complexity_rule(&mut c, &m, "ervin", TENANT, &r);
        assert!(matches!(dup, Err(TunableWriteError::Conflict(_))));

        // Update with new fields.
        let mut updated_inputs = r.clone();
        updated_inputs.base_time_minutes = 0.7;
        updated_inputs.multiplier = 1.2;
        let updated =
            update_complexity_rule(&mut c, &m, "ervin", TENANT, &created.id, &updated_inputs)
                .expect("update");
        assert!((updated.base_time_minutes - 0.7).abs() < 1e-9);

        // Delete.
        delete_complexity_rule(&mut c, &m, "ervin", TENANT, &created.id).expect("delete");
        assert!(list_complexity_rules(&c, TENANT).expect("list").is_empty());

        assert_eq!(count_audit(&c, "quote.complexity_rules_changed"), 3);
    }

    #[test]
    fn tolerance_multipliers_seeded_with_five_rows() {
        let c = conn();
        let rows = list_tolerance_multipliers(&c, TENANT).expect("list");
        assert_eq!(rows.len(), 5);
        let kinds: std::collections::HashSet<_> =
            rows.iter().map(|r| r.tolerance_range.as_str()).collect();
        assert!(kinds.contains("loose"));
        assert!(kinds.contains("ultra_precision"));
    }

    #[test]
    fn tolerance_multiplier_update_round_trip_with_audit() {
        let mut c = conn();
        let m = meta();
        let inputs = ToleranceMultiplierInputs {
            tolerance_range: "tight".to_string(),
            multiplier: 1.5,
            inspection_minutes_per_feature: 0.75,
            notes: None,
        };
        let out = update_tolerance_multiplier(&mut c, &m, "ervin", TENANT, "tight", &inputs)
            .expect("update");
        assert!((out.multiplier - 1.5).abs() < 1e-9);

        // Unknown range → NotFound.
        let miss = update_tolerance_multiplier(&mut c, &m, "ervin", TENANT, "lax", &inputs);
        assert!(matches!(miss, Err(TunableWriteError::NotFound(_))));

        assert_eq!(count_audit(&c, "quote.tolerance_multipliers_changed"), 1);
    }

    #[test]
    fn parameters_seeded_singleton_and_update_audit() {
        let mut c = conn();
        let m = meta();
        let p = get_parameters(&c, TENANT).expect("seeded");
        assert!((p.scrap_factor - 0.15).abs() < 1e-9); // S418 stock-oversize default
        assert!((p.profit_margin_base - 0.35).abs() < 1e-9);
        // S418 geometry-model knobs seeded at their day-1 defaults.
        assert!((p.machining_rate_eur_per_minute - 1.6667).abs() < 1e-9);
        assert!((p.cad_cam_rate_eur_per_hour - 100.0).abs() < 1e-9);
        assert!((p.mrr_rough_ref_cm3_per_min - 8.0).abs() < 1e-9);
        assert!((p.setup_base_min - 20.0).abs() < 1e-9);

        let inputs = QuotingParametersInputs {
            scrap_factor: 0.10,
            profit_margin_base: 0.40,
            overhead_factor: 0.25,
            setup_amortization_threshold: 8,
            min_margin: 0.12,
            exotic_material_tax: 0.06,
            machining_rate_eur_per_minute: 2.0,
            cad_cam_rate_eur_per_hour: 120.0,
            cad_cam_base_hours: 1.5,
            mrr_rough_ref_cm3_per_min: 10.0,
            t_finish_min_per_cm2: 0.1,
            setup_base_min: 25.0,
            setup_5axis_min: 30.0,
            bar_capacity_mm: 32.0,
            notes: Some("tuned 2026-06-06".to_string()),
        };
        let updated = update_parameters(&mut c, &m, "ervin", TENANT, &inputs).expect("update");
        assert!((updated.scrap_factor - 0.10).abs() < 1e-9);
        assert_eq!(updated.setup_amortization_threshold, 8);
        // S418 knobs round-trip through the write + read.
        assert!((updated.machining_rate_eur_per_minute - 2.0).abs() < 1e-9);
        assert!((updated.cad_cam_rate_eur_per_hour - 120.0).abs() < 1e-9);
        assert!((updated.setup_5axis_min - 30.0).abs() < 1e-9);

        // min_margin > profit_margin_base → Validation error.
        let bad = QuotingParametersInputs {
            scrap_factor: 0.08,
            profit_margin_base: 0.10,
            overhead_factor: 0.20,
            setup_amortization_threshold: 5,
            min_margin: 0.20,
            exotic_material_tax: 0.05,
            machining_rate_eur_per_minute: 1.6667,
            cad_cam_rate_eur_per_hour: 100.0,
            cad_cam_base_hours: 1.0,
            mrr_rough_ref_cm3_per_min: 8.0,
            t_finish_min_per_cm2: 0.08,
            setup_base_min: 20.0,
            setup_5axis_min: 25.0,
            bar_capacity_mm: 32.0,
            notes: None,
        };
        let err = update_parameters(&mut c, &m, "ervin", TENANT, &bad);
        assert!(matches!(err, Err(TunableWriteError::Validation(_))));

        assert_eq!(count_audit(&c, "quote.parameters_changed"), 1);
    }

    #[test]
    fn stock_adjustment_crud_and_grade_fk_check() {
        let mut c = conn();
        let m = meta();
        // Seed a single material so the FK check passes.
        crate::quoting_materials::create_material(
            &mut c,
            &m,
            "ervin",
            TENANT,
            &crate::quoting_materials::MaterialInputs {
                grade: "6061-T6".to_string(),
                display_name: "Aluminium".to_string(),
                density_g_cm3: 2.7,
                cost_per_kg_eur: 6.0,
                machining_difficulty: 1.0,
                carbide_life_multiplier: 1.0,
                stock_status: "in_stock".to_string(),
                lead_time_default_days: 0,
                quote_multiplier: 1.0,
                notes: None,
            },
        )
        .expect("seed material");

        let inputs = StockAdjustmentInputs {
            grade: "6061-T6".to_string(),
            stock_status: "in_stock".to_string(),
            price_adjustment_pct: -0.05,
            notes: None,
        };
        let created =
            create_stock_adjustment(&mut c, &m, "ervin", TENANT, &inputs).expect("create");
        assert_eq!(created.grade, "6061-T6");
        // S410 — app-minted prefixed ULID, not a positive integer.
        assert!(created.id.starts_with("qsa_"));

        // Unknown grade → Validation error.
        let ghost = StockAdjustmentInputs {
            grade: "MITHRIL".to_string(),
            stock_status: "in_stock".to_string(),
            price_adjustment_pct: 0.10,
            notes: None,
        };
        let miss = create_stock_adjustment(&mut c, &m, "ervin", TENANT, &ghost);
        assert!(matches!(miss, Err(TunableWriteError::Validation(_))));

        // Duplicate (grade, stock_status) → Conflict.
        let dup = create_stock_adjustment(&mut c, &m, "ervin", TENANT, &inputs);
        assert!(matches!(dup, Err(TunableWriteError::Conflict(_))));

        // Update path.
        let mut updated_inputs = inputs.clone();
        updated_inputs.price_adjustment_pct = -0.10;
        let updated =
            update_stock_adjustment(&mut c, &m, "ervin", TENANT, &created.id, &updated_inputs)
                .expect("update");
        assert!((updated.price_adjustment_pct + 0.10).abs() < 1e-9);

        delete_stock_adjustment(&mut c, &m, "ervin", TENANT, &created.id).expect("delete");
        assert!(list_stock_adjustments(&c, TENANT).expect("list").is_empty());

        assert_eq!(count_audit(&c, "quote.stock_adjustments_changed"), 3);
    }

    /// S410 / [[no-sql-specific]] — the PK used to be a DuckDB
    /// `CREATE SEQUENCE` + `nextval()` (engine-serialized identity). It
    /// is now an app-minted prefixed ULID. This pins the property that
    /// replaced the sequence: ids are globally unique even when inserts
    /// run concurrently. Each thread runs the real `create_*` insert
    /// path against its own in-memory DB (so the test is deterministic,
    /// not subject to DuckDB write-write timing); uniqueness across them
    /// is purely the app-layer ULID guarantee the sequence used to
    /// provide. If someone reintroduces a per-DB counter, two threads
    /// would mint `qcr_…1` and this fails.
    #[test]
    fn app_minted_ids_are_unique_under_concurrent_insert() {
        use std::collections::HashSet;

        let inputs = ComplexityRuleInputs {
            feature_type: "hole".to_string(),
            size_bucket: "S".to_string(),
            count_min: 1,
            count_max: Some(10),
            base_time_minutes: 0.5,
            multiplier: 1.0,
            setup_penalty_minutes: 0.0,
            notes: None,
        };
        let stock = StockAdjustmentInputs {
            grade: "6061-T6".to_string(),
            stock_status: "in_stock".to_string(),
            price_adjustment_pct: -0.05,
            notes: None,
        };

        let mut handles = Vec::new();
        for _ in 0..16 {
            let ci = inputs.clone();
            let si = stock.clone();
            handles.push(std::thread::spawn(move || {
                let mut c = conn();
                // 6061-T6 material must exist for the stock-adjustment FK gate.
                crate::quoting_materials::create_material(
                    &mut c,
                    &meta(),
                    "ervin",
                    TENANT,
                    &crate::quoting_materials::MaterialInputs {
                        grade: "6061-T6".to_string(),
                        display_name: "Aluminium".to_string(),
                        density_g_cm3: 2.7,
                        cost_per_kg_eur: 6.0,
                        machining_difficulty: 1.0,
                        carbide_life_multiplier: 1.0,
                        stock_status: "in_stock".to_string(),
                        lead_time_default_days: 0,
                        quote_multiplier: 1.0,
                        notes: None,
                    },
                )
                .expect("seed material");
                let cr = create_complexity_rule(&mut c, &meta(), "ervin", TENANT, &ci)
                    .expect("create complexity rule");
                let sa = create_stock_adjustment(&mut c, &meta(), "ervin", TENANT, &si)
                    .expect("create stock adjustment");
                (cr.id, sa.id)
            }));
        }

        let mut ids: HashSet<String> = HashSet::new();
        for h in handles {
            let (cr_id, sa_id) = h.join().expect("thread panicked");
            assert!(cr_id.starts_with("qcr_"), "complexity id prefix: {cr_id}");
            assert!(sa_id.starts_with("qsa_"), "stock id prefix: {sa_id}");
            assert!(ids.insert(cr_id), "duplicate complexity-rule id minted");
            assert!(ids.insert(sa_id), "duplicate stock-adjustment id minted");
        }
        // 16 threads × 2 rows, all distinct.
        assert_eq!(ids.len(), 32, "all app-minted ids must be unique");
    }

    #[test]
    fn stock_adjustment_pct_sanity_clamp_at_100pct() {
        let mut c = conn();
        let m = meta();
        // Seed material.
        crate::quoting_materials::create_material(
            &mut c,
            &m,
            "ervin",
            TENANT,
            &crate::quoting_materials::MaterialInputs {
                grade: "X".to_string(),
                display_name: "X".to_string(),
                density_g_cm3: 1.0,
                cost_per_kg_eur: 1.0,
                machining_difficulty: 1.0,
                carbide_life_multiplier: 1.0,
                stock_status: "in_stock".to_string(),
                lead_time_default_days: 0,
                quote_multiplier: 1.0,
                notes: None,
            },
        )
        .expect("seed");

        let too_big = StockAdjustmentInputs {
            grade: "X".to_string(),
            stock_status: "in_stock".to_string(),
            price_adjustment_pct: 5.0, // 500% — user typed a percentage not a fraction
            notes: None,
        };
        let err = create_stock_adjustment(&mut c, &m, "ervin", TENANT, &too_big);
        assert!(matches!(err, Err(TunableWriteError::Validation(_))));
    }

    fn count_audit(conn: &Connection, kind: &str) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM audit_ledger WHERE kind = ?;",
            params![kind],
            |r| r.get(0),
        )
        .expect("count audit")
    }
}
