//! S443 / ADR-0092 — `qc_inspection_plans` master data.
//!
//! The nominal/tolerance source of truth, keyed by (tenant, product,
//! feature). This is what makes the verdict ABERP's code, not the
//! machine's. Standard CRUD + archive-not-delete. Unique (product,
//! feature) among non-archived plans is enforced HERE, not by a SQL
//! UNIQUE constraint ([[no-sql-specific]]).
//!
//! Plan CRUD emits no audit EventKind — the auditable events (ADR-0092)
//! are the *inspections*, not the reference plans (the six `qc.*` kinds
//! are all inspection-scoped). A plan edit is master-data maintenance.

use duckdb::{params, Connection};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use ulid::Ulid;

use super::error::QcError;
use super::vocab::{CharacteristicDesignator, CharacteristicType, InspectionMethod};

/// One `qc_inspection_plans` row.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InspectionPlan {
    /// `qcp_<ULID>`.
    pub plan_id: String,
    pub product_id: String,
    pub feature_name: String,
    pub nominal_value: f64,
    pub upper_tol: f64,
    pub lower_tol: f64,
    pub units: String,
    pub optional_probe_cycle_id: Option<String>,
    pub enabled: bool,
    pub created_at: String,
    pub archived_at: Option<String>,

    // ── ADR-0199 §D3(a) — AS9102 characteristic IDENTITY metadata ──
    //
    // Six ADDITIVE nullable columns. They are the plan's missing
    // *identity*, not a different concept: the plan is already the
    // nominal/tolerance source of truth and already what the verdict is
    // computed against, so forking a second "characteristic" table would
    // mean two places to state a tolerance and a silent divergence
    // between what a part was measured against and what the report said
    // it was measured against.
    //
    // All six are `Option`, so every pre-ADR-0199 plan row and every
    // existing construction site is unchanged.
    /// Balloon number as it appears on the drawing ("14", "7.2").
    pub characteristic_number: Option<String>,
    /// Key / critical / major / minor / none.
    pub characteristic_designator: Option<CharacteristicDesignator>,
    /// Dimensional / material / process / note / functional.
    pub characteristic_type: Option<CharacteristicType>,
    /// How it is inspected (on-machine probe, CMM, gauge, …).
    pub inspection_method: Option<InspectionMethod>,
    /// Drawing sheet + zone, e.g. `"2/B4"`.
    pub sheet_zone: Option<String>,
    /// Whether this characteristic COUNTS TOWARD ACCOUNTABILITY
    /// (ADR-0199 §D4). `None` is read as `true` by
    /// [`InspectionPlan::counts_toward_accountability`] — see there for
    /// why the pre-ADR-0199 default is "required".
    pub is_required: Option<bool>,
}

impl InspectionPlan {
    /// Whether a missing measurement for this characteristic makes the
    /// report `incomplete` (ADR-0199 §D4).
    ///
    /// **`None` means required.** Every plan row that existed before
    /// ADR-0199 has `is_required = NULL`, and those rows are precisely
    /// the dimensional characteristics an operator set up to be
    /// inspected. Reading NULL as "optional" would silently drop every
    /// legacy characteristic out of the accountability count — a report
    /// that looks complete because the rows that would have failed are
    /// simply absent, which is the exact failure this feature exists to
    /// prevent. The conservative reading is the safe one: unless someone
    /// deliberately marked a characteristic optional, it is required.
    pub fn counts_toward_accountability(&self) -> bool {
        self.is_required.unwrap_or(true)
    }
}

/// Create/edit inputs (operator-supplied; `plan_id`/timestamps are minted).
#[derive(Debug, Clone, Deserialize)]
pub struct NewInspectionPlan {
    pub product_id: String,
    pub feature_name: String,
    pub nominal_value: f64,
    pub upper_tol: f64,
    pub lower_tol: f64,
    pub units: String,
    pub optional_probe_cycle_id: Option<String>,
    pub enabled: bool,

    // ── ADR-0199 §D3(a) — characteristic identity, all optional ──
    //
    // `#[serde(default)]` on every one so a pre-ADR-0199 request body
    // (and every existing test fixture) deserialises unchanged.
    #[serde(default)]
    pub characteristic_number: Option<String>,
    #[serde(default)]
    pub characteristic_designator: Option<CharacteristicDesignator>,
    #[serde(default)]
    pub characteristic_type: Option<CharacteristicType>,
    #[serde(default)]
    pub inspection_method: Option<InspectionMethod>,
    #[serde(default)]
    pub sheet_zone: Option<String>,
    #[serde(default)]
    pub is_required: Option<bool>,
}

fn now_rfc3339() -> Result<String, QcError> {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|e| QcError::Storage(anyhow::anyhow!("format now: {e}")))
}

/// Validate operator inputs. Tolerance band must be non-degenerate
/// (`upper_tol > lower_tol`) so the half-width tier denominator is > 0
/// (CLAUDE.md rule 12 — a malformed plan must fail loud at create, never
/// silently pass an out-of-band part later).
fn validate(input: &NewInspectionPlan) -> Result<(), QcError> {
    if input.product_id.trim().is_empty() {
        return Err(QcError::Validation("product_id is required".into()));
    }
    if input.feature_name.trim().is_empty() {
        return Err(QcError::Validation("feature_name is required".into()));
    }
    if input.units.trim().is_empty() {
        return Err(QcError::Validation("units is required".into()));
    }
    if !input.nominal_value.is_finite()
        || !input.upper_tol.is_finite()
        || !input.lower_tol.is_finite()
    {
        return Err(QcError::Validation(
            "nominal/tolerances must be finite".into(),
        ));
    }
    if input.upper_tol <= input.lower_tol {
        return Err(QcError::Validation(
            "upper_tol must be greater than lower_tol (a non-degenerate band)".into(),
        ));
    }
    Ok(())
}

/// Reject a duplicate (product, feature) among NON-archived plans (the
/// in-code unique invariant). `exclude_plan_id` skips the row being
/// edited.
fn ensure_unique(
    conn: &Connection,
    tenant: &str,
    product_id: &str,
    feature_name: &str,
    exclude_plan_id: Option<&str>,
) -> Result<(), QcError> {
    let mut stmt = conn
        .prepare(
            "SELECT plan_id FROM qc_inspection_plans
             WHERE tenant_id = ? AND product_id = ? AND feature_name = ?
               AND archived_at IS NULL;",
        )
        .map_err(|e| QcError::Storage(anyhow::anyhow!("prepare unique-check: {e}")))?;
    let rows = stmt
        .query_map(params![tenant, product_id, feature_name], |row| {
            row.get::<_, String>(0)
        })
        .map_err(|e| QcError::Storage(anyhow::anyhow!("query unique-check: {e}")))?;
    for r in rows {
        let existing =
            r.map_err(|e| QcError::Storage(anyhow::anyhow!("read unique-check: {e}")))?;
        if Some(existing.as_str()) != exclude_plan_id {
            return Err(QcError::Validation(format!(
                "an active inspection plan already exists for product {product_id} feature {feature_name:?}"
            )));
        }
    }
    Ok(())
}

/// Create a new plan. Returns the persisted row.
pub fn create_plan(
    conn: &Connection,
    tenant: &str,
    input: NewInspectionPlan,
) -> Result<InspectionPlan, QcError> {
    validate(&input)?;
    ensure_unique(conn, tenant, &input.product_id, &input.feature_name, None)?;
    let plan_id = format!("qcp_{}", Ulid::new());
    let now = now_rfc3339()?;
    conn.execute(
        "INSERT INTO qc_inspection_plans (
            plan_id, tenant_id, product_id, feature_name, nominal_value,
            upper_tol, lower_tol, units, optional_probe_cycle_id, enabled,
            created_at, archived_at,
            characteristic_number, characteristic_designator, characteristic_type,
            inspection_method, sheet_zone, is_required
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, ?, ?, ?, ?, ?, ?);",
        params![
            &plan_id,
            tenant,
            input.product_id.trim(),
            input.feature_name.trim(),
            input.nominal_value,
            input.upper_tol,
            input.lower_tol,
            input.units.trim(),
            input.optional_probe_cycle_id.as_deref(),
            input.enabled,
            &now,
            trimmed_opt(input.characteristic_number.as_deref()),
            input.characteristic_designator.map(|v| v.as_str()),
            input.characteristic_type.map(|v| v.as_str()),
            input.inspection_method.map(|v| v.as_str()),
            trimmed_opt(input.sheet_zone.as_deref()),
            input.is_required,
        ],
    )
    .map_err(|e| QcError::Storage(anyhow::anyhow!("INSERT qc_inspection_plans: {e}")))?;
    get_plan(conn, tenant, &plan_id)?
        .ok_or_else(|| QcError::Storage(anyhow::anyhow!("plan vanished after insert")))
}

/// Edit an existing (non-archived) plan in place.
pub fn update_plan(
    conn: &Connection,
    tenant: &str,
    plan_id: &str,
    input: NewInspectionPlan,
) -> Result<InspectionPlan, QcError> {
    validate(&input)?;
    let existing = get_plan(conn, tenant, plan_id)?.ok_or(QcError::NotFound)?;
    if existing.archived_at.is_some() {
        return Err(QcError::Validation("cannot edit an archived plan".into()));
    }
    ensure_unique(
        conn,
        tenant,
        &input.product_id,
        &input.feature_name,
        Some(plan_id),
    )?;
    conn.execute(
        "UPDATE qc_inspection_plans SET
            product_id = ?, feature_name = ?, nominal_value = ?, upper_tol = ?,
            lower_tol = ?, units = ?, optional_probe_cycle_id = ?, enabled = ?,
            characteristic_number = ?, characteristic_designator = ?,
            characteristic_type = ?, inspection_method = ?, sheet_zone = ?,
            is_required = ?
         WHERE tenant_id = ? AND plan_id = ?;",
        params![
            input.product_id.trim(),
            input.feature_name.trim(),
            input.nominal_value,
            input.upper_tol,
            input.lower_tol,
            input.units.trim(),
            input.optional_probe_cycle_id.as_deref(),
            input.enabled,
            trimmed_opt(input.characteristic_number.as_deref()),
            input.characteristic_designator.map(|v| v.as_str()),
            input.characteristic_type.map(|v| v.as_str()),
            input.inspection_method.map(|v| v.as_str()),
            trimmed_opt(input.sheet_zone.as_deref()),
            input.is_required,
            tenant,
            plan_id,
        ],
    )
    .map_err(|e| QcError::Storage(anyhow::anyhow!("UPDATE qc_inspection_plans: {e}")))?;
    get_plan(conn, tenant, plan_id)?.ok_or(QcError::NotFound)
}

/// Soft-delete (archive-not-delete). Idempotent.
pub fn archive_plan(conn: &Connection, tenant: &str, plan_id: &str) -> Result<(), QcError> {
    let existing = get_plan(conn, tenant, plan_id)?.ok_or(QcError::NotFound)?;
    if existing.archived_at.is_some() {
        return Ok(());
    }
    let now = now_rfc3339()?;
    conn.execute(
        "UPDATE qc_inspection_plans SET archived_at = ?, enabled = false
         WHERE tenant_id = ? AND plan_id = ?;",
        params![&now, tenant, plan_id],
    )
    .map_err(|e| QcError::Storage(anyhow::anyhow!("archive qc_inspection_plans: {e}")))?;
    Ok(())
}

/// List plans for the tenant. Filters: optional product, and whether to
/// include archived. Ordered product then feature.
pub fn list_plans(
    conn: &Connection,
    tenant: &str,
    product_id: Option<&str>,
    include_archived: bool,
) -> Result<Vec<InspectionPlan>, QcError> {
    let mut sql = String::from(
        "SELECT plan_id, product_id, feature_name, nominal_value, upper_tol,
                lower_tol, units, optional_probe_cycle_id, enabled, created_at, archived_at,
                characteristic_number, characteristic_designator, characteristic_type,
                inspection_method, sheet_zone, is_required
         FROM qc_inspection_plans WHERE tenant_id = ?",
    );
    if !include_archived {
        sql.push_str(" AND archived_at IS NULL");
    }
    if product_id.is_some() {
        sql.push_str(" AND product_id = ?");
    }
    sql.push_str(" ORDER BY product_id ASC, feature_name ASC, plan_id ASC;");

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| QcError::Storage(anyhow::anyhow!("prepare list_plans: {e}")))?;
    let collect = |rows: duckdb::MappedRows<'_, _>| -> Result<Vec<InspectionPlan>, QcError> {
        let mut acc = Vec::new();
        for r in rows {
            acc.push(r.map_err(|e| QcError::Storage(anyhow::anyhow!("read plan row: {e}")))?);
        }
        Ok(acc)
    };
    let out = match product_id {
        Some(p) => {
            let rows = stmt
                .query_map(params![tenant, p], parse_plan_row)
                .map_err(|e| QcError::Storage(anyhow::anyhow!("query list_plans: {e}")))?;
            collect(rows)?
        }
        None => {
            let rows = stmt
                .query_map(params![tenant], parse_plan_row)
                .map_err(|e| QcError::Storage(anyhow::anyhow!("query list_plans: {e}")))?;
            collect(rows)?
        }
    };
    Ok(out)
}

/// Fetch one plan by id (tenant-scoped). `None` if unknown.
pub fn get_plan(
    conn: &Connection,
    tenant: &str,
    plan_id: &str,
) -> Result<Option<InspectionPlan>, QcError> {
    let mut stmt = conn
        .prepare(
            "SELECT plan_id, product_id, feature_name, nominal_value, upper_tol,
                    lower_tol, units, optional_probe_cycle_id, enabled, created_at, archived_at,
                    characteristic_number, characteristic_designator, characteristic_type,
                    inspection_method, sheet_zone, is_required
             FROM qc_inspection_plans WHERE tenant_id = ? AND plan_id = ?;",
        )
        .map_err(|e| QcError::Storage(anyhow::anyhow!("prepare get_plan: {e}")))?;
    let mut rows = stmt
        .query_map(params![tenant, plan_id], parse_plan_row)
        .map_err(|e| QcError::Storage(anyhow::anyhow!("query get_plan: {e}")))?;
    match rows.next() {
        Some(r) => Ok(Some(r.map_err(|e| {
            QcError::Storage(anyhow::anyhow!("read get_plan: {e}"))
        })?)),
        None => Ok(None),
    }
}

/// Trim an optional operator string, collapsing a blank to `None` so a
/// stray space never becomes a "present" balloon number.
fn trimmed_opt(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|v| !v.is_empty())
}

/// Decode one of the ADR-0199 closed-vocab columns.
///
/// An unknown token is dropped to `None` rather than failing the whole
/// plan read. This is the ONE place in the report layer where a lenient
/// read is correct, and the reason is narrow: these six columns are
/// *identity metadata on a reference row*, not evidence. A plan whose
/// designator column was hand-edited to garbage must still be readable
/// so its tolerances keep gating measurements — refusing the read would
/// take the whole inspection pipeline down over a cosmetic field. The
/// frozen report line, by contrast, decodes its vocabularies strictly
/// (see `reports::parse_line_row`), because there the token IS evidence.
fn vocab_opt<T>(raw: Option<String>, parse: fn(&str) -> Result<T, &'static str>) -> Option<T> {
    raw.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| parse(s).ok())
}

fn parse_plan_row(row: &duckdb::Row<'_>) -> duckdb::Result<InspectionPlan> {
    Ok(InspectionPlan {
        plan_id: row.get(0)?,
        product_id: row.get(1)?,
        feature_name: row.get(2)?,
        nominal_value: row.get(3)?,
        upper_tol: row.get(4)?,
        lower_tol: row.get(5)?,
        units: row.get(6)?,
        optional_probe_cycle_id: row.get(7)?,
        enabled: row.get(8)?,
        created_at: row.get(9)?,
        archived_at: row.get(10)?,
        characteristic_number: row.get(11)?,
        characteristic_designator: vocab_opt(
            row.get(12)?,
            CharacteristicDesignator::from_storage_str,
        ),
        characteristic_type: vocab_opt(row.get(13)?, CharacteristicType::from_storage_str),
        inspection_method: vocab_opt(row.get(14)?, InspectionMethod::from_storage_str),
        sheet_zone: row.get(15)?,
        is_required: row.get(16)?,
    })
}
