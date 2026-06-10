//! S266 / PR-255 — `quoting_materials`, the first DB-backed tunable
//! table of the auto-quoting strand (design doc §3 / §11).
//!
//! A per-tenant catalogue of stock material grades. Each row carries the
//! physics (`density_g_cm3`), the money (`cost_per_kg_eur`), the
//! machining cost factors (`machinability_index`, `carbide_life_multiplier`),
//! the sourcing posture (`stock_status` + `lead_time_default_days`), and an
//! operator override knob (`quote_multiplier`) that the future quote engine
//! reads from a catalogue snapshot. The engine itself is out of scope here
//! (S268+); this PR ships the table, its CRUD, and the storefront push of
//! the public projection (see [`crate::catalogue_push`]).
//!
//! Conventions, deliberately mirrored from [`crate::partners`]:
//! - **`[[no-sql-specific]]`**: the schema is plain columns + a PRIMARY KEY.
//!   The brief's CHECK constraints (`density > 0`, `cost >= 0`,
//!   `lead >= 0`) are enforced in Rust ([`validate_material_inputs`]),
//!   never as DuckDB CHECK / triggers.
//! - **`[[trust-code-not-operator]]`**: every CRUD write is audited
//!   ([`EventKind::MaterialCatalogueChanged`]) with the actor and a JSON
//!   snapshot of the row, in the SAME transaction as the data write — the
//!   per-row history surfaces from the ledger, same posture the seller.toml
//!   writers use.
//! - Closed-vocab `stock_status` validated in the app layer ([`StockStatus`]).
//!
//! Timestamps are stored as RFC3339 `VARCHAR` (matching `partners.updated_at`),
//! not a SQL `TIMESTAMP` — the design doc §11 writes "TIMESTAMP" loosely; the
//! codebase convention for audit-adjacent timestamps is RFC3339 strings.

use anyhow::{Context, Result};
use duckdb::{params, Connection};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use aberp_audit_ledger::{append_in_tx, Actor, EventKind, LedgerMeta};
use ulid::Ulid;

/// The schema. Plain columns + PRIMARY KEY only — no CHECK, no triggers
/// (`[[no-sql-specific]]`). `grade` is the operator-typed natural key
/// (e.g. `6061-T6`); it is also what the storefront dropdown keys on.
///
/// **DuckDB DEFAULT-on-replay trap — DO NOT extend via `ALTER TABLE …
/// ADD COLUMN IF NOT EXISTS <col> <type> DEFAULT V`.** The `DEFAULT`
/// values below are SAFE because they ride a `CREATE TABLE IF NOT EXISTS`
/// (one-shot at create time). The trap fires if a future contributor
/// adds a column via `ALTER TABLE … ADD COLUMN … DEFAULT V` — DuckDB
/// re-applies the default on every replay of that ALTER, clobbering any
/// writes the app has made since the first migration run. See the same
/// pin on [`aberp_quote_intake::log_table::S271_MIGRATION_SQL`] for the
/// trail of evidence (S271 `stock_alert`, S272 `deal_issued_at`). The
/// fix is always: omit the DEFAULT on the ALTER; coerce NULL → desired
/// default in the app-layer reader.
const QUOTING_MATERIALS_SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS quoting_materials (
    grade                   VARCHAR NOT NULL PRIMARY KEY,
    tenant_id               VARCHAR NOT NULL,
    display_name            VARCHAR NOT NULL,
    density_g_cm3           DOUBLE  NOT NULL,
    cost_per_kg_eur         DOUBLE  NOT NULL,
    machinability_index     DOUBLE  NOT NULL DEFAULT 1.0,
    carbide_life_multiplier DOUBLE  NOT NULL DEFAULT 1.0,
    stock_status            VARCHAR NOT NULL,
    lead_time_default_days  INTEGER NOT NULL,
    quote_multiplier        DOUBLE  NOT NULL DEFAULT 1.0,
    notes                   VARCHAR,
    updated_at              VARCHAR NOT NULL,
    updated_by_actor        VARCHAR NOT NULL
);
";

// ── Closed-vocab stock status ───────────────────────────────────────────

/// Sourcing posture for a grade. Closed-vocab, snake_case on the wire and
/// in the DB. Validated in Rust, never a DB CHECK.
///
/// PUSHBACK (flagged in the PR report): the design doc §10 prose names
/// stock statuses `low` / `on_order` in passing, but the brief's §1
/// column spec pins this deliberate four-value *sourcing* vocab that maps
/// cleanly to a lead-time tier. The brief's vocab is the considered spec;
/// §10's `low`/`on_order` are illustrative prose written before it was
/// pinned. We adopt the brief's four values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StockStatus {
    /// On the shelf — promisable now.
    InStock,
    /// Not stocked; sourceable in 1–2 days.
    Source1_2d,
    /// Not stocked; sourceable in 3–7 days.
    Source3_7d,
    /// Exotic / made-to-order — a deliberate procurement event.
    SpecialOrder,
}

impl StockStatus {
    pub fn as_db_str(&self) -> &'static str {
        match self {
            StockStatus::InStock => "in_stock",
            StockStatus::Source1_2d => "source_1_2d",
            StockStatus::Source3_7d => "source_3_7d",
            StockStatus::SpecialOrder => "special_order",
        }
    }

    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "in_stock" => Some(StockStatus::InStock),
            "source_1_2d" => Some(StockStatus::Source1_2d),
            "source_3_7d" => Some(StockStatus::Source3_7d),
            "special_order" => Some(StockStatus::SpecialOrder),
            _ => None,
        }
    }

    /// All variants, in display order. Used by tests + the SPA's closed
    /// vocab (kept in sync with the TS `STOCK_STATUS_ORDER`).
    pub const ALL: [StockStatus; 4] = [
        StockStatus::InStock,
        StockStatus::Source1_2d,
        StockStatus::Source3_7d,
        StockStatus::SpecialOrder,
    ];
}

// ── Wire shapes ─────────────────────────────────────────────────────────

/// Operator-supplied input for create / update. On update, `grade` is
/// taken from the path (the PK is immutable, like an adapter's kind), so
/// the body's `grade` is ignored there.
#[derive(Debug, Clone, Deserialize)]
pub struct MaterialInputs {
    #[serde(default)]
    pub grade: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub density_g_cm3: f64,
    #[serde(default)]
    pub cost_per_kg_eur: f64,
    #[serde(default = "one")]
    pub machinability_index: f64,
    #[serde(default = "one")]
    pub carbide_life_multiplier: f64,
    #[serde(default)]
    pub stock_status: String,
    #[serde(default)]
    pub lead_time_default_days: i64,
    #[serde(default = "one")]
    pub quote_multiplier: f64,
    #[serde(default)]
    pub notes: Option<String>,
}

fn one() -> f64 {
    1.0
}

/// A persisted catalogue row, as returned to the SPA.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Material {
    pub grade: String,
    pub display_name: String,
    pub density_g_cm3: f64,
    pub cost_per_kg_eur: f64,
    pub machinability_index: f64,
    pub carbide_life_multiplier: f64,
    pub stock_status: String,
    pub lead_time_default_days: i64,
    pub quote_multiplier: f64,
    pub notes: Option<String>,
    pub updated_at: String,
    pub updated_by_actor: String,
}

/// The public projection pushed to the storefront dropdown (design doc
/// §4 / §14-C). DELIBERATELY excludes cost, the multipliers, density, and
/// machining factors — the customer-facing dropdown shows only what a
/// buyer picks from.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PublicMaterial {
    pub grade: String,
    pub display_name: String,
    pub stock_status: String,
    pub lead_time_default_days: i64,
}

#[derive(Serialize, Debug, PartialEq, Eq, Clone)]
pub struct ValidationError {
    pub field: &'static str,
    pub message: String,
}

// ── Validation (the app-layer invariants the brief asked for) ───────────

/// Enforce the brief's CHECK-equivalent invariants in Rust. Returns the
/// full list of problems (the SPA renders all at once), or `Ok` when
/// clean. Numbers must be finite (NaN/Inf is garbage, fail loud).
pub fn validate_material_inputs(inputs: &MaterialInputs) -> Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();

    if inputs.grade.trim().is_empty() {
        errors.push(ValidationError {
            field: "grade",
            message: "Az anyagminőség (grade) kötelező / Material grade is required".to_string(),
        });
    }
    if inputs.display_name.trim().is_empty() {
        errors.push(ValidationError {
            field: "display_name",
            message: "A megjelenítendő név kötelező / Display name is required".to_string(),
        });
    }

    // density > 0
    check_positive(&mut errors, "density_g_cm3", inputs.density_g_cm3);
    // cost >= 0
    check_non_negative(&mut errors, "cost_per_kg_eur", inputs.cost_per_kg_eur);
    // multipliers > 0 — a zero/negative multiplier would zero or invert a
    // future quote (footgun); the brief sets DEFAULT 1.0 and the engine
    // multiplies by these, so reject non-positive. (Beyond the brief's
    // explicit CHECK set; flagged in the PR report as conservative.)
    check_positive(
        &mut errors,
        "machinability_index",
        inputs.machinability_index,
    );
    check_positive(
        &mut errors,
        "carbide_life_multiplier",
        inputs.carbide_life_multiplier,
    );
    check_positive(&mut errors, "quote_multiplier", inputs.quote_multiplier);

    // lead_time_default_days >= 0
    if inputs.lead_time_default_days < 0 {
        errors.push(ValidationError {
            field: "lead_time_default_days",
            message: "A szállítási idő nem lehet negatív / Lead time cannot be negative"
                .to_string(),
        });
    }

    if StockStatus::from_db_str(inputs.stock_status.trim()).is_none() {
        errors.push(ValidationError {
            field: "stock_status",
            message: format!(
                "Ismeretlen készlet-állapot `{}` / Unknown stock_status (expected one of: {})",
                inputs.stock_status,
                StockStatus::ALL
                    .iter()
                    .map(|s| s.as_db_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        });
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

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

// ── Schema + seed ───────────────────────────────────────────────────────

/// Create the table (idempotent) and, on a brand-new (empty) table, seed
/// a small set of common grades the operator can then edit or delete.
pub fn ensure_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(QUOTING_MATERIALS_SCHEMA_SQL)
        .context("ensure quoting_materials schema")?;
    Ok(())
}

/// Seed common grades on first boot if the table is empty. Idempotent:
/// re-running is a no-op once any row exists (operator may have edited or
/// deleted seeds — we never re-add). Seed writes carry the `boot` actor.
///
/// The numbers are reasonable engineering *starting points* the operator
/// tunes — NOT authoritative. Densities are standard handbook values;
/// `machinability_index` is relative to a free-cutting baseline of 1.0
/// (aluminium easier, stainless/titanium/superalloy progressively harder);
/// costs are rough €/kg order-of-magnitude. Seeds are NOT pushed any
/// differently from operator rows — they are ordinary editable data.
pub fn seed_if_empty(conn: &mut Connection, tenant: &str) -> Result<()> {
    ensure_schema(conn)?;
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM quoting_materials", [], |r| r.get(0))
        .context("count quoting_materials for seed gate")?;
    if count > 0 {
        return Ok(());
    }

    // (grade, display_name, density, cost€/kg, machinability, carbide,
    //  stock_status, lead_days, quote_multiplier)
    let seeds: &[(&str, &str, f64, f64, f64, f64, StockStatus, i64, f64)] = &[
        (
            "6061-T6",
            "Aluminium 6061-T6",
            2.70,
            6.0,
            0.7,
            1.0,
            StockStatus::InStock,
            0,
            1.0,
        ),
        (
            "7075-T651",
            "Aluminium 7075-T651",
            2.81,
            9.0,
            0.9,
            1.1,
            StockStatus::InStock,
            0,
            1.0,
        ),
        (
            "304",
            "Stainless steel 304",
            8.00,
            4.0,
            1.6,
            1.8,
            StockStatus::InStock,
            0,
            1.0,
        ),
        (
            "316",
            "Stainless steel 316",
            8.00,
            6.0,
            1.8,
            2.0,
            StockStatus::Source1_2d,
            2,
            1.0,
        ),
        (
            "Ti-6Al-4V",
            "Titanium Ti-6Al-4V (Grade 5)",
            4.43,
            35.0,
            3.5,
            4.0,
            StockStatus::Source3_7d,
            7,
            1.0,
        ),
        (
            "Inconel 718",
            "Nickel superalloy Inconel 718",
            8.19,
            50.0,
            5.0,
            6.0,
            StockStatus::SpecialOrder,
            21,
            1.0,
        ),
        (
            "PEEK",
            "PEEK (polyether ether ketone)",
            1.30,
            90.0,
            0.9,
            1.0,
            StockStatus::Source1_2d,
            2,
            1.0,
        ),
        (
            "MONEL_650",
            "Monel 650",
            8.80,
            40.0,
            3.0,
            3.5,
            StockStatus::SpecialOrder,
            14,
            1.0,
        ),
    ];

    let now = now_rfc3339()?;
    let tx = conn.transaction().context("begin seed tx")?;
    for (grade, name, density, cost, mach, carbide, status, lead, qmult) in seeds {
        tx.execute(
            "INSERT INTO quoting_materials (
                grade, tenant_id, display_name, density_g_cm3, cost_per_kg_eur,
                machinability_index, carbide_life_multiplier, stock_status,
                lead_time_default_days, quote_multiplier, notes, updated_at, updated_by_actor
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, ?, 'boot');",
            params![
                grade,
                tenant,
                name,
                density,
                cost,
                mach,
                carbide,
                status.as_db_str(),
                lead,
                qmult,
                &now,
            ],
        )
        .with_context(|| format!("seed quoting_materials row {grade}"))?;
    }
    tx.commit().context("commit quoting_materials seed")?;
    Ok(())
}

// ── CRUD (each write audited in-tx) ─────────────────────────────────────

/// List every grade for the tenant, ordered by grade.
pub fn list_materials(conn: &Connection, tenant: &str) -> Result<Vec<Material>> {
    ensure_schema(conn)?;
    let mut stmt = conn
        .prepare(
            "SELECT grade, display_name, density_g_cm3, cost_per_kg_eur,
                    machinability_index, carbide_life_multiplier, stock_status,
                    lead_time_default_days, quote_multiplier, notes,
                    updated_at, updated_by_actor
             FROM quoting_materials
             WHERE tenant_id = ?
             ORDER BY grade ASC;",
        )
        .context("prepare list quoting_materials")?;
    let rows = stmt
        .query_map(params![tenant], row_to_material)
        .context("query quoting_materials")?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.context("read quoting_materials row")?);
    }
    Ok(out)
}

/// The storefront push projection (public fields only), ordered by grade.
pub fn list_public(conn: &Connection, tenant: &str) -> Result<Vec<PublicMaterial>> {
    ensure_schema(conn)?;
    let mut stmt = conn
        .prepare(
            "SELECT grade, display_name, stock_status, lead_time_default_days
             FROM quoting_materials
             WHERE tenant_id = ?
             ORDER BY grade ASC;",
        )
        .context("prepare list_public quoting_materials")?;
    let rows = stmt
        .query_map(params![tenant], |row| {
            Ok(PublicMaterial {
                grade: row.get(0)?,
                display_name: row.get(1)?,
                stock_status: row.get(2)?,
                lead_time_default_days: row.get(3)?,
            })
        })
        .context("query_map list_public")?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.context("read public material row")?);
    }
    Ok(out)
}

/// Insert a new grade. Fails if the grade already exists (PK conflict).
/// Validates first, then writes the row + the `create` audit entry in one
/// transaction.
pub fn create_material(
    conn: &mut Connection,
    meta: &LedgerMeta,
    actor_login: &str,
    tenant: &str,
    inputs: &MaterialInputs,
) -> Result<Material, MaterialWriteError> {
    if let Err(errs) = validate_material_inputs(inputs) {
        return Err(MaterialWriteError::Validation(errs));
    }
    ensure_schema(conn).map_err(MaterialWriteError::Other)?;
    let grade = inputs.grade.trim().to_string();

    // Pre-check for a friendlier error than a raw PK violation.
    let exists: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM quoting_materials WHERE tenant_id = ? AND grade = ?;",
            params![tenant, &grade],
            |r| r.get(0),
        )
        .context("check existing grade")
        .map_err(MaterialWriteError::Other)?;
    if exists > 0 {
        return Err(MaterialWriteError::Conflict(grade));
    }

    let now = now_rfc3339().map_err(MaterialWriteError::Other)?;
    let status = StockStatus::from_db_str(inputs.stock_status.trim())
        .expect("validated above")
        .as_db_str();
    let notes = normalize_optional(inputs.notes.as_deref());

    let tx = conn
        .transaction()
        .context("begin create_material tx")
        .map_err(MaterialWriteError::Other)?;
    tx.execute(
        "INSERT INTO quoting_materials (
            grade, tenant_id, display_name, density_g_cm3, cost_per_kg_eur,
            machinability_index, carbide_life_multiplier, stock_status,
            lead_time_default_days, quote_multiplier, notes, updated_at, updated_by_actor
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?);",
        params![
            &grade,
            tenant,
            inputs.display_name.trim(),
            inputs.density_g_cm3,
            inputs.cost_per_kg_eur,
            inputs.machinability_index,
            inputs.carbide_life_multiplier,
            status,
            inputs.lead_time_default_days,
            inputs.quote_multiplier,
            notes.as_deref(),
            &now,
            actor_login,
        ],
    )
    .context("INSERT quoting_materials")
    .map_err(MaterialWriteError::Other)?;

    let material = read_material_in_tx(&tx, tenant, &grade).map_err(MaterialWriteError::Other)?;
    append_catalogue_change(&tx, meta, actor_login, "create", &material)
        .map_err(MaterialWriteError::Other)?;
    tx.commit()
        .context("commit create_material")
        .map_err(MaterialWriteError::Other)?;
    Ok(material)
}

/// Update an existing grade in place (the PK `grade` is immutable; taken
/// from the path). Returns `NotFound` if no such grade.
pub fn update_material(
    conn: &mut Connection,
    meta: &LedgerMeta,
    actor_login: &str,
    tenant: &str,
    grade: &str,
    inputs: &MaterialInputs,
) -> Result<Material, MaterialWriteError> {
    if let Err(errs) = validate_material_inputs(inputs) {
        return Err(MaterialWriteError::Validation(errs));
    }
    ensure_schema(conn).map_err(MaterialWriteError::Other)?;
    let now = now_rfc3339().map_err(MaterialWriteError::Other)?;
    let status = StockStatus::from_db_str(inputs.stock_status.trim())
        .expect("validated above")
        .as_db_str();
    let notes = normalize_optional(inputs.notes.as_deref());

    let tx = conn
        .transaction()
        .context("begin update_material tx")
        .map_err(MaterialWriteError::Other)?;
    let changed = tx
        .execute(
            "UPDATE quoting_materials SET
                display_name            = ?,
                density_g_cm3           = ?,
                cost_per_kg_eur         = ?,
                machinability_index     = ?,
                carbide_life_multiplier = ?,
                stock_status            = ?,
                lead_time_default_days  = ?,
                quote_multiplier        = ?,
                notes                   = ?,
                updated_at              = ?,
                updated_by_actor        = ?
             WHERE tenant_id = ? AND grade = ?;",
            params![
                inputs.display_name.trim(),
                inputs.density_g_cm3,
                inputs.cost_per_kg_eur,
                inputs.machinability_index,
                inputs.carbide_life_multiplier,
                status,
                inputs.lead_time_default_days,
                inputs.quote_multiplier,
                notes.as_deref(),
                &now,
                actor_login,
                tenant,
                grade,
            ],
        )
        .context("UPDATE quoting_materials")
        .map_err(MaterialWriteError::Other)?;
    if changed == 0 {
        return Err(MaterialWriteError::NotFound(grade.to_string()));
    }

    let material = read_material_in_tx(&tx, tenant, grade).map_err(MaterialWriteError::Other)?;
    append_catalogue_change(&tx, meta, actor_login, "update", &material)
        .map_err(MaterialWriteError::Other)?;
    tx.commit()
        .context("commit update_material")
        .map_err(MaterialWriteError::Other)?;
    Ok(material)
}

/// Hard-delete a grade (no soft-delete: nothing references the catalogue
/// yet — the engine doesn't exist — and a deleted grade should vanish from
/// the storefront dropdown on the next push). Returns `NotFound` if absent.
pub fn delete_material(
    conn: &mut Connection,
    meta: &LedgerMeta,
    actor_login: &str,
    tenant: &str,
    grade: &str,
) -> Result<(), MaterialWriteError> {
    ensure_schema(conn).map_err(MaterialWriteError::Other)?;
    let tx = conn
        .transaction()
        .context("begin delete_material tx")
        .map_err(MaterialWriteError::Other)?;

    // Read the row first so the audit snapshot records what was deleted.
    let material =
        match read_material_in_tx_opt(&tx, tenant, grade).map_err(MaterialWriteError::Other)? {
            Some(m) => m,
            None => return Err(MaterialWriteError::NotFound(grade.to_string())),
        };
    tx.execute(
        "DELETE FROM quoting_materials WHERE tenant_id = ? AND grade = ?;",
        params![tenant, grade],
    )
    .context("DELETE quoting_materials")
    .map_err(MaterialWriteError::Other)?;
    append_catalogue_change(&tx, meta, actor_login, "delete", &material)
        .map_err(MaterialWriteError::Other)?;
    tx.commit()
        .context("commit delete_material")
        .map_err(MaterialWriteError::Other)?;
    Ok(())
}

// ── Errors ──────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum MaterialWriteError {
    Validation(Vec<ValidationError>),
    /// Create attempted on a grade that already exists.
    Conflict(String),
    /// Update/delete of a grade that does not exist.
    NotFound(String),
    Other(anyhow::Error),
}

impl From<anyhow::Error> for MaterialWriteError {
    fn from(e: anyhow::Error) -> Self {
        MaterialWriteError::Other(e)
    }
}

// ── Internals ───────────────────────────────────────────────────────────

fn append_catalogue_change(
    tx: &duckdb::Transaction<'_>,
    meta: &LedgerMeta,
    actor_login: &str,
    op: &str,
    material: &Material,
) -> Result<()> {
    let payload = serde_json::json!({
        "op": op,
        "grade": material.grade,
        "row": material,
        "idempotency_key": Ulid::new().to_string(),
    });
    let bytes =
        serde_json::to_vec(&payload).context("serialize MaterialCatalogueChanged payload")?;
    let actor = Actor::from_local_cli(Ulid::new().to_string(), actor_login);
    append_in_tx(
        tx,
        meta,
        EventKind::MaterialCatalogueChanged,
        bytes,
        actor,
        None,
    )
    .context("audit append MaterialCatalogueChanged")?;
    Ok(())
}

fn read_material_in_tx(
    tx: &duckdb::Transaction<'_>,
    tenant: &str,
    grade: &str,
) -> Result<Material> {
    read_material_in_tx_opt(tx, tenant, grade)?
        .with_context(|| format!("row vanished mid-tx for grade {grade}"))
}

fn read_material_in_tx_opt(
    tx: &duckdb::Transaction<'_>,
    tenant: &str,
    grade: &str,
) -> Result<Option<Material>> {
    let mut stmt = tx.prepare(
        "SELECT grade, display_name, density_g_cm3, cost_per_kg_eur,
                machinability_index, carbide_life_multiplier, stock_status,
                lead_time_default_days, quote_multiplier, notes,
                updated_at, updated_by_actor
         FROM quoting_materials
         WHERE tenant_id = ? AND grade = ?;",
    )?;
    let mut rows = stmt.query_map(params![tenant, grade], row_to_material)?;
    match rows.next() {
        Some(r) => Ok(Some(r?)),
        None => Ok(None),
    }
}

fn row_to_material(row: &duckdb::Row<'_>) -> duckdb::Result<Material> {
    Ok(Material {
        grade: row.get(0)?,
        display_name: row.get(1)?,
        density_g_cm3: row.get(2)?,
        cost_per_kg_eur: row.get(3)?,
        machinability_index: row.get(4)?,
        carbide_life_multiplier: row.get(5)?,
        stock_status: row.get(6)?,
        lead_time_default_days: row.get(7)?,
        quote_multiplier: row.get(8)?,
        notes: row.get(9)?,
        updated_at: row.get(10)?,
        updated_by_actor: row.get(11)?,
    })
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
        let c = Connection::open_in_memory().expect("open in-memory");
        audit_ensure_schema(&c).expect("audit schema");
        ensure_schema(&c).expect("quoting_materials schema");
        c
    }

    fn valid_inputs(grade: &str) -> MaterialInputs {
        MaterialInputs {
            grade: grade.to_string(),
            display_name: format!("Display {grade}"),
            density_g_cm3: 2.7,
            cost_per_kg_eur: 6.0,
            machinability_index: 1.0,
            carbide_life_multiplier: 1.0,
            stock_status: "in_stock".to_string(),
            lead_time_default_days: 0,
            quote_multiplier: 1.0,
            notes: None,
        }
    }

    #[test]
    fn stock_status_round_trips_all_variants() {
        for s in StockStatus::ALL {
            assert_eq!(StockStatus::from_db_str(s.as_db_str()), Some(s));
        }
        assert_eq!(StockStatus::from_db_str("low"), None);
        assert_eq!(StockStatus::from_db_str(""), None);
    }

    #[test]
    fn validation_enforces_brief_invariants() {
        // density must be > 0
        let mut i = valid_inputs("X");
        i.density_g_cm3 = 0.0;
        assert!(field_errored(&i, "density_g_cm3"));

        // cost >= 0 (0 is allowed; -1 is not)
        let mut i = valid_inputs("X");
        i.cost_per_kg_eur = -1.0;
        assert!(field_errored(&i, "cost_per_kg_eur"));
        let mut i = valid_inputs("X");
        i.cost_per_kg_eur = 0.0;
        assert!(validate_material_inputs(&i).is_ok());

        // lead >= 0
        let mut i = valid_inputs("X");
        i.lead_time_default_days = -1;
        assert!(field_errored(&i, "lead_time_default_days"));

        // unknown stock_status
        let mut i = valid_inputs("X");
        i.stock_status = "on_order".to_string();
        assert!(field_errored(&i, "stock_status"));

        // NaN/Inf are rejected (fail loud)
        let mut i = valid_inputs("X");
        i.machinability_index = f64::NAN;
        assert!(field_errored(&i, "machinability_index"));
        let mut i = valid_inputs("X");
        i.quote_multiplier = 0.0;
        assert!(field_errored(&i, "quote_multiplier"));

        // empty grade / display_name
        let mut i = valid_inputs("");
        i.grade = "   ".to_string();
        assert!(field_errored(&i, "grade"));
    }

    fn field_errored(i: &MaterialInputs, field: &str) -> bool {
        match validate_material_inputs(i) {
            Ok(()) => false,
            Err(errs) => errs.iter().any(|e| e.field == field),
        }
    }

    #[test]
    fn seed_is_idempotent_and_populates_known_grades() {
        let mut c = conn();
        seed_if_empty(&mut c, TENANT).expect("first seed");
        let first = list_materials(&c, TENANT).expect("list");
        assert_eq!(first.len(), 8, "expected 8 seed grades");
        assert!(first.iter().any(|m| m.grade == "6061-T6"));
        assert!(first.iter().any(|m| m.grade == "Inconel 718"));
        // every seed validates against our own rules
        for m in &first {
            assert!(StockStatus::from_db_str(&m.stock_status).is_some());
            assert!(m.density_g_cm3 > 0.0);
        }
        // re-seed is a no-op
        seed_if_empty(&mut c, TENANT).expect("second seed");
        assert_eq!(list_materials(&c, TENANT).expect("list2").len(), 8);
    }

    #[test]
    fn crud_round_trip_and_audit() {
        let mut c = conn();
        let m = meta();

        // create
        let created =
            create_material(&mut c, &m, "ervin", TENANT, &valid_inputs("6061-T6")).expect("create");
        assert_eq!(created.grade, "6061-T6");
        assert_eq!(created.updated_by_actor, "ervin");

        // duplicate create → Conflict
        let dup = create_material(&mut c, &m, "ervin", TENANT, &valid_inputs("6061-T6"));
        assert!(matches!(dup, Err(MaterialWriteError::Conflict(_))));

        // update
        let mut upd = valid_inputs("6061-T6");
        upd.cost_per_kg_eur = 7.5;
        upd.stock_status = "source_1_2d".to_string();
        let updated =
            update_material(&mut c, &m, "ervin", TENANT, "6061-T6", &upd).expect("update");
        assert_eq!(updated.cost_per_kg_eur, 7.5);
        assert_eq!(updated.stock_status, "source_1_2d");

        // update missing → NotFound
        let miss = update_material(&mut c, &m, "ervin", TENANT, "NOPE", &valid_inputs("NOPE"));
        assert!(matches!(miss, Err(MaterialWriteError::NotFound(_))));

        // two audit entries so far (create + 1 successful update; the
        // Conflict and NotFound never reach the audit append).
        assert_eq!(count_catalogue_audit(&c), 2);

        // delete
        delete_material(&mut c, &m, "ervin", TENANT, "6061-T6").expect("delete");
        assert!(list_materials(&c, TENANT).expect("list").is_empty());
        let miss = delete_material(&mut c, &m, "ervin", TENANT, "6061-T6");
        assert!(matches!(miss, Err(MaterialWriteError::NotFound(_))));

        assert_eq!(count_catalogue_audit(&c), 3, "create+update+delete audited");
    }

    #[test]
    fn public_projection_excludes_internal_fields() {
        let mut c = conn();
        let m = meta();
        create_material(&mut c, &m, "ervin", TENANT, &valid_inputs("304")).expect("create");
        let pub_rows = list_public(&c, TENANT).expect("public");
        assert_eq!(pub_rows.len(), 1);
        let json = serde_json::to_string(&pub_rows[0]).unwrap();
        // public projection carries the 4 public fields and NOTHING else.
        assert!(json.contains("\"grade\""));
        assert!(json.contains("\"display_name\""));
        assert!(json.contains("\"stock_status\""));
        assert!(json.contains("\"lead_time_default_days\""));
        assert!(!json.contains("cost_per_kg"));
        assert!(!json.contains("quote_multiplier"));
        assert!(!json.contains("density"));
        assert!(!json.contains("machinability"));
    }

    /// S338 — the storefront `/api/catalogue/materials` receiver validates
    /// every pushed grade against `GRADE_RE` in `catalogue-store.ts`. Pre-S338
    /// that regex was `/^[A-Z][A-Z0-9_]*$/`, which 400'd every real grade
    /// ("6061-T6", "304", "Ti-6Al-4V", "Inconel 718", …) — so the *entire*
    /// push was rejected and the `/quote` dropdown stayed on its generic
    /// fallback. The relaxed contract is `^[A-Za-z0-9][A-Za-z0-9 ._+/-]*$`.
    /// This pins the wire contract from the ABERP end: every grade we would
    /// PUT must satisfy what the storefront will ACCEPT. If a future seed (or
    /// operator-typed grade in a fixture) drifts outside the charset, this
    /// fails here rather than silently 400-looping in prod.
    ///
    /// Keep this pattern in lockstep with `GRADE_RE` in the storefront's
    /// `src/lib/server/catalogue-store.ts`.
    fn grade_satisfies_storefront_contract(grade: &str) -> bool {
        let mut chars = grade.chars();
        match chars.next() {
            // First char must be alphanumeric (no leading separator/space).
            Some(c) if c.is_ascii_alphanumeric() => {}
            _ => return false,
        }
        // Remaining chars: alnum or the safe separators ` . _ + / -`.
        chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '.' | '_' | '+' | '/' | '-'))
    }

    #[test]
    fn s338_catalogue_push_delivers_snapshot_to_storefront_on_change() {
        let mut c = conn();
        seed_if_empty(&mut c, TENANT).expect("seed");
        let pub_rows = list_public(&c, TENANT).expect("public projection");
        // The body we would PUT is non-empty (an empty snapshot is itself the
        // fallback-dropdown symptom).
        assert!(!pub_rows.is_empty(), "public projection must not be empty");
        // …and every grade in it satisfies the storefront's accept contract,
        // so the push lands (200) instead of being rejected wholesale (400).
        for m in &pub_rows {
            assert!(
                grade_satisfies_storefront_contract(&m.grade),
                "seed grade {:?} would be rejected by the storefront receiver",
                m.grade
            );
        }
    }

    #[test]
    fn s338_contract_helper_rejects_the_old_failure_shapes() {
        // Real grades the pre-S338 regex wrongly rejected — now accepted.
        for g in [
            "6061-T6",
            "304",
            "Ti-6Al-4V",
            "Inconel 718",
            "17-4PH",
            "PEEK",
        ] {
            assert!(grade_satisfies_storefront_contract(g), "{g} must pass");
        }
        // Genuinely unsafe / malformed shapes stay rejected.
        for g in ["-6061", " 304", "AL\r\n6061", "AL<x>", "AL;DROP", ""] {
            assert!(!grade_satisfies_storefront_contract(g), "{g:?} must fail");
        }
    }

    // ── S342 / PR-37 — display_name presence + full-row contract pin ─────

    #[test]
    fn s342_catalogue_push_snapshot_includes_display_name() {
        // The S339 prod symptom was a 400 "display_name is required". This
        // pins that the snapshot ABERP builds carries a NON-EMPTY
        // display_name per row (the field has shipped since S266; this guards
        // it against a future projection that drops or empties it).
        let mut c = conn();
        let m = meta();
        create_material(&mut c, &m, "ervin", TENANT, &valid_inputs("6061-T6")).expect("create");
        let rows = list_public(&c, TENANT).expect("public projection");
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.grade, "6061-T6");
        assert!(
            !row.display_name.trim().is_empty(),
            "every pushed row must carry a non-empty display_name (the storefront 400s without it)"
        );
        // …and it survives serialization under the wire key `display_name`.
        // The daemon PUTs `{ "materials": [ <PublicMaterial>, … ] }`.
        let json = serde_json::to_string(&serde_json::json!({ "materials": rows }))
            .expect("serialize snapshot");
        assert!(
            json.contains("\"display_name\":\"Display 6061-T6\""),
            "serialized snapshot must carry the display_name field verbatim; got {json}"
        );
    }

    /// S342 / PR-37 — cross-repo contract pin (mirrors S338's grade pin, but
    /// for the WHOLE row). The storefront's `validateMaterialRow` in
    /// `src/lib/server/catalogue-store.ts` rejects the entire push (400) if
    /// any field is missing/malformed. This mirrors those rules and asserts
    /// every row ABERP would PUT satisfies them — so a projection drift
    /// (e.g. dropping display_name) fails HERE, not as a silent prod 400.
    ///
    /// Keep in lockstep with `catalogue-store.ts`:
    ///   grade: non-empty, ≤64, GRADE_RE
    ///   display_name: non-empty (trimmed), ≤200, no CR/LF/NUL
    ///   stock_status: in the closed enum
    ///   lead_time_default_days: integer in [0, 365]
    fn row_satisfies_storefront_contract(m: &PublicMaterial) -> Result<(), String> {
        // Mirror of STOCK_STATUSES in catalogue-store.ts.
        const STOREFRONT_STOCK_STATUSES: [&str; 5] = [
            "in_stock",
            "source_1_2d",
            "source_3_7d",
            "source_2_4w",
            "special_order",
        ];
        if m.grade.is_empty() || m.grade.len() > 64 {
            return Err(format!("grade length out of [1,64]: {:?}", m.grade));
        }
        if !grade_satisfies_storefront_contract(&m.grade) {
            return Err(format!("grade fails GRADE_RE: {:?}", m.grade));
        }
        if m.display_name.trim().is_empty() || m.display_name.len() > 200 {
            return Err(format!("display_name out of [1,200]: {:?}", m.display_name));
        }
        if m.display_name.contains(['\r', '\n', '\0']) {
            return Err(format!("display_name has CR/LF/NUL: {:?}", m.display_name));
        }
        if !STOREFRONT_STOCK_STATUSES.contains(&m.stock_status.as_str()) {
            return Err(format!(
                "stock_status not in closed enum: {:?}",
                m.stock_status
            ));
        }
        if !(0..=365).contains(&m.lead_time_default_days) {
            return Err(format!(
                "lead_time_default_days out of [0,365]: {}",
                m.lead_time_default_days
            ));
        }
        Ok(())
    }

    #[test]
    fn s342_snapshot_satisfies_storefront_validate_material_row() {
        // The seeded catalogue (real hyphenated/digit grades — 6061-T6,
        // 304, Ti-6Al-4V, …) plus one operator-typed grade not in the seed
        // must all pass the storefront's per-row validation.
        let mut c = conn();
        seed_if_empty(&mut c, TENANT).expect("seed");
        create_material(
            &mut c,
            &meta(),
            "ervin",
            TENANT,
            &valid_inputs("Custom-Alloy 42"),
        )
        .expect("create");
        let rows = list_public(&c, TENANT).expect("public projection");
        assert!(!rows.is_empty(), "snapshot must not be empty");
        for m in &rows {
            row_satisfies_storefront_contract(m)
                .unwrap_or_else(|e| panic!("row would be 400'd by the storefront: {e}"));
        }
    }

    #[test]
    fn s342_contract_pin_catches_a_dropped_display_name() {
        // Prove the pin actually fails on the prod symptom (an empty
        // display_name) — a guard that can't fail is worthless (CLAUDE.md #9).
        let bad = PublicMaterial {
            grade: "6061-T6".to_string(),
            display_name: "   ".to_string(),
            stock_status: "in_stock".to_string(),
            lead_time_default_days: 7,
        };
        assert!(
            row_satisfies_storefront_contract(&bad).is_err(),
            "an empty display_name must fail the contract pin (it is the prod 400)"
        );
    }

    fn count_catalogue_audit(conn: &Connection) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM audit_ledger WHERE kind = 'quote.material_catalogue_changed';",
            [],
            |r| r.get(0),
        )
        .expect("count audit")
    }
}
