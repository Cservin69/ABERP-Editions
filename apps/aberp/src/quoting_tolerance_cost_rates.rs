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
//! ## Zero-contribution seed (ADR-0097 Q6 / R4)
//!
//! [`seed_tolerance_cost_rates_if_absent`] inserts one **all-zero** row per band
//! (`finish_passes_add = 0`, `inproc_inspection_min = 0`,
//! `cmm_min_per_critical_feature = 0`, `rework_scrap_pct = 0`,
//! `feed_slowdown_factor = 1.0`, `grinding_escalation = false`). The CRUD has
//! rows to edit, but **no money moves** until the operator tunes a value
//! (R4 seed-inflation mitigation): every seeded row contributes exactly
//! `0.0` EUR, so totals stay byte-identical to pre-ADR-0097. (FLAG: because
//! T3's `tolerance_op_cost` enters its computation whenever the rate slice is
//! **non-empty**, a seeded — therefore non-empty — table makes a freshly
//! priced quote's `reasoning_log` carry the itemised zero-cost tolerance lines;
//! the *price* is unchanged and already-frozen quotes are untouched. The truly
//! empty table remains byte-identical including the log. See the T4 wiring
//! note in `quote_pricing_pipeline`.)
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

/// One seed row: band + its **zero-contribution** day-1 numbers (ADR-0097 Q6).
struct Seed {
    band: ToleranceRange,
    finish_passes_add: f64,
    inproc_inspection_min: f64,
    cmm_min_per_critical_feature: f64,
    rework_scrap_pct: f64,
    feed_slowdown_factor: f64,
    grinding_escalation: bool,
}

/// One zero-contribution seed per band. Every value is the engine's no-op:
/// zero added minutes/passes/uplift and a neutral `1.0` feed factor, so a
/// seeded shop prices **exactly** as before until the operator tunes a row
/// (R4 seed-inflation mitigation). Grinding escalation off for every band
/// (it would only ever fire at `ultra_precision` anyway).
const SEEDS: &[Seed] = &[
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
    Seed {
        band: ToleranceRange::Tight,
        finish_passes_add: 0.0,
        inproc_inspection_min: 0.0,
        cmm_min_per_critical_feature: 0.0,
        rework_scrap_pct: 0.0,
        feed_slowdown_factor: 1.0,
        grinding_escalation: false,
    },
    Seed {
        band: ToleranceRange::Precision,
        finish_passes_add: 0.0,
        inproc_inspection_min: 0.0,
        cmm_min_per_critical_feature: 0.0,
        rework_scrap_pct: 0.0,
        feed_slowdown_factor: 1.0,
        grinding_escalation: false,
    },
    Seed {
        band: ToleranceRange::UltraPrecision,
        finish_passes_add: 0.0,
        inproc_inspection_min: 0.0,
        cmm_min_per_critical_feature: 0.0,
        rework_scrap_pct: 0.0,
        feed_slowdown_factor: 1.0,
        grinding_escalation: false,
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
    conn.execute_batch(SCHEMA_SQL)
        .context("ensure quoting_tolerance_cost_rates schema")
}

/// Seed the five ADR-0097 bands, **insert-if-absent** per band — so a re-run
/// (or a partially-seeded table) never duplicates and never clobbers an
/// operator-tuned value. Idempotent: gated per `(tenant, tolerance_class)`.
/// Every seed row is zero-contribution ⇒ pricing is byte-identical until
/// tuned (R4).
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
            continue;
        }
        let id = format!("qtcr_{}", Ulid::new());
        conn.execute(
            "INSERT INTO quoting_tolerance_cost_rates (id, tenant_id, tolerance_class, \
             finish_passes_add, inproc_inspection_min, cmm_min_per_critical_feature, \
             rework_scrap_pct, feed_slowdown_factor, grinding_escalation, \
             notes, updated_at, updated_by_actor) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, ?, 'boot');",
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
                &now,
            ],
        )
        .with_context(|| format!("seed quoting_tolerance_cost_rates row for {band}"))?;
    }
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
