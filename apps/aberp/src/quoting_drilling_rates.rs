//! ADR-0112 Part C (wiring) — `quoting_drilling_rates` catalogue.
//!
//! The engine half (D-19 slice C1) shipped [`aberp_quote_engine::DrillingRate`]
//! and the `CatalogueSnapshot.drilling_rates` slice: given located holes and a
//! matching, non-inert rate for the part's material group, the engine prices a
//! per-hole drilling cycle time (cut + peck + rapid + tool-change) per §C.2.
//! This module is the missing data layer: the operator-managed, material-group-
//! keyed rate table the pricing pipeline snapshots into the engine.
//!
//! ## A direct structural clone of [`crate::quoting_machine_rates`]
//!
//! Prefixed-ULID id (`qdr_<ULID>`), lazy `CREATE TABLE IF NOT EXISTS`,
//! invariants enforced **in code** not via SQL CHECK/trigger
//! ([[no-sql-specific]]). One rate per `material_group` per tenant — the group
//! is the natural unique key (enforced in code). Unlike machine rates, the
//! `material_group` is a FREE string: it keys against
//! [`aberp_quote_engine::Material::grade`], the operator-typed material grade,
//! so there is no closed vocab to round-trip — the check is only non-empty.
//!
//! ## The empty / inert slice IS the edition gate
//!
//! Two independent reasons the table moves nothing on day one, both by design
//! (ADR-0112 §C.1):
//!
//!   1. **Edition gate.** Only a Defense build seeds this table
//!      ([`crate::build_profile::machining_cost_model_allowed`]). A Portable
//!      tenant gets zero rows, the snapshot slice is empty, and the engine
//!      never enters the drilling path — `drilling_minutes = 0.0`, no reasoning
//!      line, breakdown byte-identical to pre-ADR-0112.
//!   2. **Zero-contribution seed.** Even on Defense the seeded rows carry
//!      `feed_mm_per_min_per_mm_dia = 0.0`. The engine's `drilling_active`
//!      predicate requires a matching rate with `feed > 0.0`, so a feed-zero
//!      row is INERT: Defense operators have rows to edit, but nothing moves
//!      until they tune a real feed per material (ADR-0097 Q6 / ADR-0112 Q7).
//!      The real feeds, peck policy and tool-change times must come from
//!      Ervin's actual machines — until then the feature ships inert.
//!
//! ## Audit
//!
//! CRUD emits via the audit ledger, **reusing** [`EventKind::ParametersChanged`]
//! (the quoting-tunables-changed kind) rather than a dedicated
//! `DrillingRatesChanged` variant — `EventKind` is not `#[non_exhaustive]` and
//! has ~186 variants matched across crates the sandbox cannot compile-verify,
//! so a new variant is an unacceptable blast radius (ADR-0094 toolchain-honesty
//! clause). The payload is self-describing (`"catalogue":"quoting_drilling_rates"`)
//! so a future migration to a dedicated kind is a pure relabel.

use anyhow::{Context, Result};
use duckdb::{params, Connection};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use ulid::Ulid;

use aberp_audit_ledger::{append_in_tx, Actor, EventKind, LedgerMeta};

// Reuse the tunables write-error + validation-error vocab so the serve
// layer's `tunable_write_response` maps drilling-rate failures identically.
use crate::quoting_tunables::{TunableWriteError, ValidationError};

/// Wire + storage shape of a `quoting_drilling_rates` row.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct DrillingRateRow {
    /// `qdr_<26-char-ULID>`.
    pub id: String,
    /// The material group this rate keys against — matched against
    /// [`aberp_quote_engine::Material::grade`]. Free string, operator-typed.
    pub material_group: String,
    /// Cutting feed, mm/min per mm of drill diameter. **0.0 ⇒ INERT**: the
    /// engine never enters the drilling path for this group until it is > 0.
    pub feed_mm_per_min_per_mm_dia: f64,
    /// Peck depth as a multiple of diameter (a full-depth peck cycle).
    pub peck_depth_dia_multiple: f64,
    /// Seconds lost per peck retract-and-return.
    pub peck_retract_sec: f64,
    /// Rapid approach + retract seconds, once per hole.
    pub rapid_per_hole_sec: f64,
    /// Tool change seconds, once per DISTINCT diameter on the part.
    pub tool_change_sec: f64,
    /// Multiplier for a blind flat-bottom hole (a flat-bottom drill / mill is
    /// slower than a point drill). >= 1.0.
    pub flat_bottom_factor: f64,
    /// Multiplier when the hole's end condition is Unknown — the conservative
    /// branch, never a discount. >= 1.0.
    pub unknown_end_condition_factor: f64,
    pub notes: Option<String>,
    pub updated_at: String,
    pub updated_by_actor: String,
}

/// Request body for create/update.
#[derive(Deserialize, Debug, Clone)]
pub struct DrillingRateInputs {
    #[serde(default)]
    pub material_group: String,
    #[serde(default)]
    pub feed_mm_per_min_per_mm_dia: f64,
    #[serde(default = "default_peck_depth_dia_multiple")]
    pub peck_depth_dia_multiple: f64,
    #[serde(default)]
    pub peck_retract_sec: f64,
    #[serde(default)]
    pub rapid_per_hole_sec: f64,
    #[serde(default)]
    pub tool_change_sec: f64,
    #[serde(default = "default_unit_factor")]
    pub flat_bottom_factor: f64,
    #[serde(default = "default_unit_factor")]
    pub unknown_end_condition_factor: f64,
    #[serde(default)]
    pub notes: Option<String>,
}

/// Neutral default peck depth (3×D — a common full-depth peck) so a minimal
/// body need only supply the group + feed.
fn default_peck_depth_dia_multiple() -> f64 {
    3.0
}

/// Neutral default for the two end-condition multipliers (1.0 ⇒ no penalty).
fn default_unit_factor() -> f64 {
    1.0
}

/// One seed row: material group + its day-1 (INERT) coefficients.
struct Seed {
    material_group: &'static str,
    peck_depth_dia_multiple: f64,
    peck_retract_sec: f64,
    rapid_per_hole_sec: f64,
    tool_change_sec: f64,
    flat_bottom_factor: f64,
    unknown_end_condition_factor: f64,
}

/// The unmistakable provenance label stamped into every seeded row's `notes`
/// column, so an operator never mistakes an INERT seed default for a tuned,
/// shop-measured drilling feed. The SPA's rate list renders `notes` verbatim.
pub const SEED_NOTE: &str =
    "SEED — INERT (feed = 0). Drilling is NOT priced for this material until you \
     set a real cutting feed measured on your machines. / ALAPÉRTÉK — INAKTÍV \
     (előtolás = 0). A fúrás nincs beárazva, amíg meg nem adja a saját gépén mért \
     előtolást.";

/// The material groups seeded, one per `quoting_materials` seed grade
/// (`quoting_materials.rs` `MACHINING_DIFFICULTY_SEED`) so a freshly-seeded
/// Defense tenant has an editable drilling row for every stock grade. Every
/// row is **feed-zero / inert**: the peck / rapid / tool-change / end-condition
/// coefficients are reasonable placeholders, but with feed = 0 the engine never
/// prices a hole, so the seed contributes nothing until Ervin sets real feeds
/// per material (ADR-0112 Q7). Keeping the group list in lockstep with the
/// material seed is what makes "seed then tune" the operator's whole workflow.
const SEEDS: &[Seed] = &[
    Seed {
        material_group: "PEEK",
        peck_depth_dia_multiple: 5.0,
        peck_retract_sec: 0.5,
        rapid_per_hole_sec: 2.0,
        tool_change_sec: 20.0,
        flat_bottom_factor: 1.2,
        unknown_end_condition_factor: 1.5,
    },
    Seed {
        material_group: "6061-T6",
        peck_depth_dia_multiple: 4.0,
        peck_retract_sec: 0.6,
        rapid_per_hole_sec: 2.0,
        tool_change_sec: 20.0,
        flat_bottom_factor: 1.2,
        unknown_end_condition_factor: 1.5,
    },
    Seed {
        material_group: "7075-T651",
        peck_depth_dia_multiple: 4.0,
        peck_retract_sec: 0.6,
        rapid_per_hole_sec: 2.0,
        tool_change_sec: 20.0,
        flat_bottom_factor: 1.2,
        unknown_end_condition_factor: 1.5,
    },
    Seed {
        material_group: "304",
        peck_depth_dia_multiple: 3.0,
        peck_retract_sec: 0.8,
        rapid_per_hole_sec: 2.0,
        tool_change_sec: 25.0,
        flat_bottom_factor: 1.3,
        unknown_end_condition_factor: 1.5,
    },
    Seed {
        material_group: "316",
        peck_depth_dia_multiple: 3.0,
        peck_retract_sec: 0.8,
        rapid_per_hole_sec: 2.0,
        tool_change_sec: 25.0,
        flat_bottom_factor: 1.3,
        unknown_end_condition_factor: 1.5,
    },
    Seed {
        material_group: "MONEL_650",
        peck_depth_dia_multiple: 2.5,
        peck_retract_sec: 1.0,
        rapid_per_hole_sec: 2.0,
        tool_change_sec: 30.0,
        flat_bottom_factor: 1.4,
        unknown_end_condition_factor: 1.6,
    },
    Seed {
        material_group: "Ti-6Al-4V",
        peck_depth_dia_multiple: 2.0,
        peck_retract_sec: 1.2,
        rapid_per_hole_sec: 2.5,
        tool_change_sec: 30.0,
        flat_bottom_factor: 1.5,
        unknown_end_condition_factor: 1.7,
    },
    Seed {
        material_group: "Inconel 718",
        peck_depth_dia_multiple: 1.5,
        peck_retract_sec: 1.5,
        rapid_per_hole_sec: 2.5,
        tool_change_sec: 35.0,
        flat_bottom_factor: 1.6,
        unknown_end_condition_factor: 1.8,
    },
];

/// Validate inputs in code (no SQL CHECK). Surfaces every error at once
/// (CLAUDE.md rule 9 / 12).
///
/// `feed = 0.0` is a VALID, meaningful value — it disables drilling pricing
/// for the group (the seed posture). So feed is required only to be finite and
/// **non-negative**; the engine's own `feed > 0.0` guard is what actually
/// switches pricing on. Every other coefficient must be finite and
/// non-negative, and the two end-condition multipliers must be >= 1.0 — they
/// are conservative penalties, never discounts.
pub fn validate_drilling_rate_inputs(
    inputs: &DrillingRateInputs,
) -> Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();

    if inputs.material_group.trim().is_empty() {
        errors.push(ValidationError {
            field: "material_group",
            message: "Az anyagcsoport kötelező. / Material group is required.".to_string(),
        });
    }

    let finite_nonneg = |v: f64| v.is_finite() && v >= 0.0;
    let finite_ge1 = |v: f64| v.is_finite() && v >= 1.0;

    if !finite_nonneg(inputs.feed_mm_per_min_per_mm_dia) {
        errors.push(ValidationError {
            field: "feed_mm_per_min_per_mm_dia",
            message: "Az előtolás legyen véges és >= 0 (0 = inaktív). / Feed must be finite and >= 0 (0 = inert)."
                .to_string(),
        });
    }
    if !(inputs.peck_depth_dia_multiple.is_finite() && inputs.peck_depth_dia_multiple > 0.0) {
        errors.push(ValidationError {
            field: "peck_depth_dia_multiple",
            message: "A csipegetési mélység legyen véges és > 0. / Peck depth multiple must be finite and > 0."
                .to_string(),
        });
    }
    if !finite_nonneg(inputs.peck_retract_sec) {
        errors.push(ValidationError {
            field: "peck_retract_sec",
            message: "A visszahúzási idő legyen véges és >= 0. / Peck retract seconds must be finite and >= 0."
                .to_string(),
        });
    }
    if !finite_nonneg(inputs.rapid_per_hole_sec) {
        errors.push(ValidationError {
            field: "rapid_per_hole_sec",
            message:
                "A gyorsjárati idő legyen véges és >= 0. / Rapid seconds must be finite and >= 0."
                    .to_string(),
        });
    }
    if !finite_nonneg(inputs.tool_change_sec) {
        errors.push(ValidationError {
            field: "tool_change_sec",
            message: "A szerszámcsere idő legyen véges és >= 0. / Tool-change seconds must be finite and >= 0."
                .to_string(),
        });
    }
    if !finite_ge1(inputs.flat_bottom_factor) {
        errors.push(ValidationError {
            field: "flat_bottom_factor",
            message: "A lapos fenekű szorzó legyen véges és >= 1.0. / Flat-bottom factor must be finite and >= 1.0."
                .to_string(),
        });
    }
    if !finite_ge1(inputs.unknown_end_condition_factor) {
        errors.push(ValidationError {
            field: "unknown_end_condition_factor",
            message: "Az ismeretlen véglezárás szorzó legyen véges és >= 1.0. / Unknown-end-condition factor must be finite and >= 1.0."
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
CREATE TABLE IF NOT EXISTS quoting_drilling_rates (
    id                           VARCHAR NOT NULL PRIMARY KEY,
    tenant_id                    VARCHAR NOT NULL,
    material_group               VARCHAR NOT NULL,
    feed_mm_per_min_per_mm_dia   DOUBLE  NOT NULL,
    peck_depth_dia_multiple      DOUBLE  NOT NULL,
    peck_retract_sec             DOUBLE  NOT NULL,
    rapid_per_hole_sec           DOUBLE  NOT NULL,
    tool_change_sec              DOUBLE  NOT NULL,
    flat_bottom_factor           DOUBLE  NOT NULL,
    unknown_end_condition_factor DOUBLE  NOT NULL,
    notes                        VARCHAR,
    updated_at                   VARCHAR NOT NULL,
    updated_by_actor             VARCHAR NOT NULL
);
";

const COLS: &str = "id, material_group, feed_mm_per_min_per_mm_dia, peck_depth_dia_multiple, \
                    peck_retract_sec, rapid_per_hole_sec, tool_change_sec, flat_bottom_factor, \
                    unknown_end_condition_factor, notes, updated_at, updated_by_actor";

/// Idempotent table creation. Called at serve boot + defensively on each
/// request entry point ([[hulye-biztos]]). No SQL CHECK/index — small
/// master data scanned in full ([[no-sql-specific]]).
pub fn ensure_schema(conn: &Connection) -> Result<()> {
    // ADR-0098 C2 fix-forward — no-op on a read-only conn; the schema is
    // created by a writer before any read reaches here.
    if aberp_audit_ledger::connection_is_read_only(conn) {
        return Ok(());
    }
    conn.execute_batch(SCHEMA_SQL)
        .context("ensure quoting_drilling_rates schema")
}

/// Seed the [`SEEDS`] material groups, **insert-if-absent** per group — so a
/// re-run (or a partially-seeded table) never duplicates and never clobbers an
/// operator-tuned value. Idempotent: gated per `(tenant, material_group)`. Each
/// seeded row is feed-zero / inert and stamped with [`SEED_NOTE`] so it reads
/// unmistakably as an inert default rather than a shop-measured feed.
///
/// **Defense-only.** The caller gates this behind
/// [`crate::build_profile::machining_cost_model_allowed`] — a Portable tenant
/// never seeds a row, which (with the empty-slice engine guard) is the edition
/// gate.
pub fn seed_drilling_rates_if_absent(conn: &Connection, tenant: &str) -> Result<()> {
    ensure_schema(conn)?;
    let now = now_rfc3339()?;
    for seed in SEEDS {
        let existing: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM quoting_drilling_rates WHERE tenant_id = ? AND material_group = ?;",
                params![tenant, seed.material_group],
                |r| r.get(0),
            )
            .context("count quoting_drilling_rates for seed gate")?;
        if existing > 0 {
            continue;
        }
        let id = format!("qdr_{}", Ulid::new());
        conn.execute(
            "INSERT INTO quoting_drilling_rates (id, tenant_id, material_group, \
             feed_mm_per_min_per_mm_dia, peck_depth_dia_multiple, peck_retract_sec, \
             rapid_per_hole_sec, tool_change_sec, flat_bottom_factor, \
             unknown_end_condition_factor, notes, updated_at, updated_by_actor) \
             VALUES (?, ?, ?, 0.0, ?, ?, ?, ?, ?, ?, ?, ?, 'boot');",
            params![
                &id,
                tenant,
                seed.material_group,
                seed.peck_depth_dia_multiple,
                seed.peck_retract_sec,
                seed.rapid_per_hole_sec,
                seed.tool_change_sec,
                seed.flat_bottom_factor,
                seed.unknown_end_condition_factor,
                SEED_NOTE,
                &now,
            ],
        )
        .with_context(|| {
            format!(
                "seed quoting_drilling_rates row for {}",
                seed.material_group
            )
        })?;
    }
    Ok(())
}

fn now_rfc3339() -> Result<String> {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .context("format quoting_drilling_rates timestamp")
}

fn row_to_drilling_rate(row: &duckdb::Row<'_>) -> duckdb::Result<DrillingRateRow> {
    Ok(DrillingRateRow {
        id: row.get(0)?,
        material_group: row.get(1)?,
        feed_mm_per_min_per_mm_dia: row.get(2)?,
        peck_depth_dia_multiple: row.get(3)?,
        peck_retract_sec: row.get(4)?,
        rapid_per_hole_sec: row.get(5)?,
        tool_change_sec: row.get(6)?,
        flat_bottom_factor: row.get(7)?,
        unknown_end_condition_factor: row.get(8)?,
        notes: row.get(9)?,
        updated_at: row.get(10)?,
        updated_by_actor: row.get(11)?,
    })
}

/// All rate rows for a tenant, group-ordered (stable list for the SPA).
pub fn list_drilling_rates(conn: &Connection, tenant: &str) -> Result<Vec<DrillingRateRow>> {
    ensure_schema(conn)?;
    let sql = format!(
        "SELECT {COLS} FROM quoting_drilling_rates WHERE tenant_id = ? ORDER BY material_group ASC;"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![tenant], row_to_drilling_rate)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// The rows as the engine consumes them — the exact `Vec<DrillingRate>` the
/// pricing pipeline snapshots into [`aberp_quote_engine::CatalogueSnapshot`].
pub fn engine_rates(
    conn: &Connection,
    tenant: &str,
) -> Result<Vec<aberp_quote_engine::DrillingRate>> {
    Ok(list_drilling_rates(conn, tenant)?
        .into_iter()
        .map(|r| aberp_quote_engine::DrillingRate {
            material_group: r.material_group,
            feed_mm_per_min_per_mm_dia: r.feed_mm_per_min_per_mm_dia,
            peck_depth_dia_multiple: r.peck_depth_dia_multiple,
            peck_retract_sec: r.peck_retract_sec,
            rapid_per_hole_sec: r.rapid_per_hole_sec,
            tool_change_sec: r.tool_change_sec,
            flat_bottom_factor: r.flat_bottom_factor,
            unknown_end_condition_factor: r.unknown_end_condition_factor,
        })
        .collect())
}

fn get_drilling_rate(conn: &Connection, tenant: &str, id: &str) -> Result<Option<DrillingRateRow>> {
    let sql = format!("SELECT {COLS} FROM quoting_drilling_rates WHERE tenant_id = ? AND id = ?;");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query_map(params![tenant, id], row_to_drilling_rate)?;
    match rows.next() {
        Some(r) => Ok(Some(r?)),
        None => Ok(None),
    }
}

/// Count rows holding `material_group` other than `except_id` — the in-code
/// one-rate-per-group uniqueness guard (no SQL UNIQUE, [[no-sql-specific]]).
fn group_taken_by_other(
    conn: &Connection,
    tenant: &str,
    material_group: &str,
    except_id: &str,
) -> Result<bool> {
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM quoting_drilling_rates \
             WHERE tenant_id = ? AND material_group = ? AND id != ?;",
            params![tenant, material_group, except_id],
            |r| r.get(0),
        )
        .context("check quoting_drilling_rates group uniqueness")?;
    Ok(n > 0)
}

/// Create a rate for a material group (one per group). `Conflict` if the group
/// already has a row — the operator edits the existing one instead.
pub fn create_drilling_rate(
    conn: &mut Connection,
    meta: &LedgerMeta,
    actor_login: &str,
    tenant: &str,
    inputs: &DrillingRateInputs,
) -> Result<DrillingRateRow, TunableWriteError> {
    if let Err(e) = validate_drilling_rate_inputs(inputs) {
        return Err(TunableWriteError::Validation(e));
    }
    ensure_schema(conn).map_err(TunableWriteError::Other)?;
    let material_group = inputs.material_group.trim();
    if group_taken_by_other(conn, tenant, material_group, "").map_err(TunableWriteError::Other)? {
        return Err(TunableWriteError::Conflict(format!(
            "a rate for material group `{material_group}` already exists — edit it instead"
        )));
    }
    let now = now_rfc3339().map_err(TunableWriteError::Other)?;
    let notes = normalize_optional(inputs.notes.as_deref());
    let id = format!("qdr_{}", Ulid::new());
    let tx = conn
        .transaction()
        .context("begin create_drilling_rate tx")
        .map_err(TunableWriteError::Other)?;
    tx.execute(
        "INSERT INTO quoting_drilling_rates (id, tenant_id, material_group, \
         feed_mm_per_min_per_mm_dia, peck_depth_dia_multiple, peck_retract_sec, \
         rapid_per_hole_sec, tool_change_sec, flat_bottom_factor, \
         unknown_end_condition_factor, notes, updated_at, updated_by_actor) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?);",
        params![
            &id,
            tenant,
            material_group,
            inputs.feed_mm_per_min_per_mm_dia,
            inputs.peck_depth_dia_multiple,
            inputs.peck_retract_sec,
            inputs.rapid_per_hole_sec,
            inputs.tool_change_sec,
            inputs.flat_bottom_factor,
            inputs.unknown_end_condition_factor,
            notes.as_deref(),
            &now,
            actor_login,
        ],
    )
    .context("INSERT quoting_drilling_rates")
    .map_err(TunableWriteError::Other)?;
    let row = read_in_tx(&tx, tenant, &id).map_err(TunableWriteError::Other)?;
    append_drilling_rate_change(&tx, meta, actor_login, "drilling_rate_create", &row)
        .map_err(TunableWriteError::Other)?;
    tx.commit()
        .context("commit create_drilling_rate")
        .map_err(TunableWriteError::Other)?;
    Ok(row)
}

/// Update a rate by id. `NotFound` if the row is absent; `Conflict` if the
/// edited `material_group` collides with another row.
pub fn update_drilling_rate(
    conn: &mut Connection,
    meta: &LedgerMeta,
    actor_login: &str,
    tenant: &str,
    id: &str,
    inputs: &DrillingRateInputs,
) -> Result<DrillingRateRow, TunableWriteError> {
    if let Err(e) = validate_drilling_rate_inputs(inputs) {
        return Err(TunableWriteError::Validation(e));
    }
    ensure_schema(conn).map_err(TunableWriteError::Other)?;
    let material_group = inputs.material_group.trim();
    if get_drilling_rate(conn, tenant, id)
        .map_err(TunableWriteError::Other)?
        .is_none()
    {
        return Err(TunableWriteError::NotFound(format!(
            "quoting_drilling_rates row {id} not found"
        )));
    }
    if group_taken_by_other(conn, tenant, material_group, id).map_err(TunableWriteError::Other)? {
        return Err(TunableWriteError::Conflict(format!(
            "another rate for material group `{material_group}` already exists"
        )));
    }
    let now = now_rfc3339().map_err(TunableWriteError::Other)?;
    let notes = normalize_optional(inputs.notes.as_deref());
    let tx = conn
        .transaction()
        .context("begin update_drilling_rate tx")
        .map_err(TunableWriteError::Other)?;
    tx.execute(
        "UPDATE quoting_drilling_rates SET material_group = ?, feed_mm_per_min_per_mm_dia = ?, \
         peck_depth_dia_multiple = ?, peck_retract_sec = ?, rapid_per_hole_sec = ?, \
         tool_change_sec = ?, flat_bottom_factor = ?, unknown_end_condition_factor = ?, \
         notes = ?, updated_at = ?, updated_by_actor = ? WHERE tenant_id = ? AND id = ?;",
        params![
            material_group,
            inputs.feed_mm_per_min_per_mm_dia,
            inputs.peck_depth_dia_multiple,
            inputs.peck_retract_sec,
            inputs.rapid_per_hole_sec,
            inputs.tool_change_sec,
            inputs.flat_bottom_factor,
            inputs.unknown_end_condition_factor,
            notes.as_deref(),
            &now,
            actor_login,
            tenant,
            id,
        ],
    )
    .context("UPDATE quoting_drilling_rates")
    .map_err(TunableWriteError::Other)?;
    let row = read_in_tx(&tx, tenant, id).map_err(TunableWriteError::Other)?;
    append_drilling_rate_change(&tx, meta, actor_login, "drilling_rate_update", &row)
        .map_err(TunableWriteError::Other)?;
    tx.commit()
        .context("commit update_drilling_rate")
        .map_err(TunableWriteError::Other)?;
    Ok(row)
}

/// Hard-delete a rate by id (the group falls back to inert — the engine simply
/// finds no matching rate and prices no drilling for it). `NotFound` if absent.
pub fn delete_drilling_rate(
    conn: &mut Connection,
    meta: &LedgerMeta,
    actor_login: &str,
    tenant: &str,
    id: &str,
) -> Result<(), TunableWriteError> {
    ensure_schema(conn).map_err(TunableWriteError::Other)?;
    let Some(row) = get_drilling_rate(conn, tenant, id).map_err(TunableWriteError::Other)? else {
        return Err(TunableWriteError::NotFound(format!(
            "quoting_drilling_rates row {id} not found"
        )));
    };
    let tx = conn
        .transaction()
        .context("begin delete_drilling_rate tx")
        .map_err(TunableWriteError::Other)?;
    tx.execute(
        "DELETE FROM quoting_drilling_rates WHERE tenant_id = ? AND id = ?;",
        params![tenant, id],
    )
    .context("DELETE quoting_drilling_rates")
    .map_err(TunableWriteError::Other)?;
    append_drilling_rate_change(&tx, meta, actor_login, "drilling_rate_delete", &row)
        .map_err(TunableWriteError::Other)?;
    tx.commit()
        .context("commit delete_drilling_rate")
        .map_err(TunableWriteError::Other)?;
    Ok(())
}

// ── Internals ───────────────────────────────────────────────────────────

fn normalize_optional(s: Option<&str>) -> Option<String> {
    s.map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

fn read_in_tx(tx: &duckdb::Transaction<'_>, tenant: &str, id: &str) -> Result<DrillingRateRow> {
    let sql = format!("SELECT {COLS} FROM quoting_drilling_rates WHERE tenant_id = ? AND id = ?;");
    let mut stmt = tx.prepare(&sql)?;
    let mut rows = stmt.query_map(params![tenant, id], row_to_drilling_rate)?;
    match rows.next() {
        Some(r) => Ok(r?),
        None => Err(anyhow::anyhow!(
            "quoting_drilling_rates row {id} vanished mid-tx"
        )),
    }
}

/// Append a drilling-rate-change audit entry inside the write tx. Reuses
/// [`EventKind::ParametersChanged`] (see module docs) with a self-describing
/// payload so a future dedicated kind is a pure relabel.
fn append_drilling_rate_change(
    tx: &duckdb::Transaction<'_>,
    meta: &LedgerMeta,
    actor_login: &str,
    op: &str,
    row: &DrillingRateRow,
) -> Result<()> {
    let payload = serde_json::json!({
        "catalogue": "quoting_drilling_rates",
        "op": op,
        "snapshot": { "row": row },
        "idempotency_key": Ulid::new().to_string(),
    });
    let bytes =
        serde_json::to_vec(&payload).context("serialize drilling-rate change audit payload")?;
    let actor = Actor::from_local_cli(Ulid::new().to_string(), actor_login);
    append_in_tx(tx, meta, EventKind::ParametersChanged, bytes, actor, None)
        .context("audit append drilling-rate change")?;
    Ok(())
}
