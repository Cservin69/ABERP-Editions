//! S428 — `margin_profiles` master data.
//!
//! Operator-managed margin policy keyed by customer segment
//! ([`crate::partners::CustomerType`]). When a quote's buyer partner has a
//! customer type that matches an *active* (non-archived, enabled) profile,
//! the pricing pipeline applies the profile's `gross_margin_pct` as the
//! markup (overriding the global default) and enforces `min_margin_pct` as
//! the refuse-DEAL floor (see [`crate::quote_margin`]).
//!
//! ## Conventions mirrored from [`crate::quoting_machines`]
//!
//! Prefixed-ULID id (`mp_<ULID>`), lazy `CREATE TABLE IF NOT EXISTS`,
//! invariants in **code** not SQL CHECK ([[no-sql-specific]]),
//! archive-not-delete (`archived_at`), and audit emission via
//! [`crate::quoting_machines::append_machine_event`] (the generic ledger
//! append helper) called by the serve request wrappers after the DB write.
//!
//! ## Unique-active-per-type invariant
//!
//! At most ONE non-archived profile may exist per customer type. Enforced
//! in [`create_profile`] / [`update_profile`] (a duplicate returns the
//! `DuplicateActiveType` outcome, mapped to 409 at the route) rather than a
//! DuckDB UNIQUE index ([[no-sql-specific]]).

use anyhow::{Context, Result};
use duckdb::{params, Connection};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use ulid::Ulid;

use crate::partners::CustomerType;

fn default_enabled() -> bool {
    true
}

/// A margin-profile row, wire + storage shape.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct MarginProfile {
    /// `mp_<26-char-ULID>`.
    pub id: String,
    pub name: String,
    /// [`CustomerType`] db-string, e.g. `defense`.
    pub customer_type: String,
    /// Target gross margin applied as the engine's `profit_margin_base`
    /// markup (fraction, e.g. 0.35 = 35%).
    pub gross_margin_pct: f64,
    /// Refuse-DEAL floor: the realized margin (`margin / total_price`)
    /// must stay at or above this (fraction, e.g. 0.10 = 10%).
    pub min_margin_pct: f64,
    pub notes: Option<String>,
    pub enabled: bool,
    pub created_at: String,
    pub updated_at: String,
    /// `None` while active; Rfc3339 once archived.
    pub archived_at: Option<String>,
}

/// Request body for create/update.
#[derive(Deserialize, Debug, Clone)]
pub struct MarginProfileInputs {
    pub name: String,
    pub customer_type: String,
    pub gross_margin_pct: f64,
    pub min_margin_pct: f64,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

/// Field-level validation error (wire shape for 400 responses).
#[derive(Serialize, Debug, PartialEq, Eq, Clone)]
pub struct ValidationError {
    pub field: &'static str,
    pub message: String,
}

/// Outcome of [`create_profile`]. `DuplicateActiveType` ⇒ 409.
#[derive(Debug, PartialEq)]
pub enum CreateOutcome {
    Created(MarginProfile),
    DuplicateActiveType,
}

/// Outcome of [`update_profile`]. `NotFound` ⇒ 404, `DuplicateActiveType`
/// ⇒ 409 (the edit would collide with another active profile of the new
/// customer type).
#[derive(Debug, PartialEq)]
pub enum UpdateOutcome {
    Updated(MarginProfile),
    NotFound,
    DuplicateActiveType,
}

/// Validate inputs in code (no SQL CHECK). Surfaces every error at once
/// (CLAUDE.md rule 9 / 12) rather than failing on the first.
pub fn validate_profile_inputs(inputs: &MarginProfileInputs) -> Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();

    if inputs.name.trim().is_empty() {
        errors.push(ValidationError {
            field: "name",
            message: "A profil neve kötelező. / Profile name is required.".to_string(),
        });
    } else if inputs.name.trim().len() > 120 {
        errors.push(ValidationError {
            field: "name",
            message: "A profil neve legfeljebb 120 karakter. / Profile name max 120 chars."
                .to_string(),
        });
    }

    if CustomerType::from_db_str(inputs.customer_type.trim()).is_none() {
        errors.push(ValidationError {
            field: "customer_type",
            message: format!(
                "Ismeretlen vevőtípus: {:?}. / Unknown customer type.",
                inputs.customer_type
            ),
        });
    }

    // `gross_margin_pct` is the engine's `profit_margin_base` markup —
    // a non-negative fraction (1.0 = +100% markup). Cap at 10.0 as a
    // sanity ceiling; below the cap the operator owns the number.
    if !(inputs.gross_margin_pct.is_finite() && (0.0..=10.0).contains(&inputs.gross_margin_pct)) {
        errors.push(ValidationError {
            field: "gross_margin_pct",
            message:
                "A cél árrés 0 és 10 (1000%) között legyen. / Target margin must be in [0, 10]."
                    .to_string(),
        });
    }

    // `min_margin_pct` is a gross-margin floor (fraction of price), so it
    // must be strictly below 1.0 (you cannot floor margin at 100% of price).
    if !(inputs.min_margin_pct.is_finite() && (0.0..1.0).contains(&inputs.min_margin_pct)) {
        errors.push(ValidationError {
            field: "min_margin_pct",
            message: "A minimum árrés 0 és 1 (100%) között legyen. / Min margin must be in [0, 1)."
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
CREATE TABLE IF NOT EXISTS margin_profiles (
    id               VARCHAR NOT NULL PRIMARY KEY,
    tenant_id        VARCHAR NOT NULL,
    name             VARCHAR NOT NULL,
    customer_type    VARCHAR NOT NULL,
    gross_margin_pct DOUBLE  NOT NULL,
    min_margin_pct   DOUBLE  NOT NULL,
    notes            VARCHAR,
    enabled          BOOLEAN NOT NULL,
    created_at       VARCHAR NOT NULL,
    updated_at       VARCHAR NOT NULL,
    archived_at      VARCHAR
);
";

/// Idempotent table creation. Called at serve boot + defensively on each
/// request entry point. No SQL CHECK/index ([[no-sql-specific]] — small
/// master-data table, scanned in full).
pub fn ensure_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA_SQL)
        .context("ensure margin_profiles schema")
}

fn now_rfc3339() -> Result<String> {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .context("format margin_profiles timestamp")
}

const COLS: &str = "id, name, customer_type, gross_margin_pct, min_margin_pct, notes, enabled, \
                    created_at, updated_at, archived_at";

fn row_to_profile(row: &duckdb::Row<'_>) -> duckdb::Result<MarginProfile> {
    Ok(MarginProfile {
        id: row.get(0)?,
        name: row.get(1)?,
        customer_type: row.get(2)?,
        gross_margin_pct: row.get(3)?,
        min_margin_pct: row.get(4)?,
        notes: row.get(5)?,
        enabled: row.get(6)?,
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
        archived_at: row.get(9)?,
    })
}

/// Is there a non-archived profile for `customer_type` other than
/// `exclude_id`? Drives the unique-active-per-type invariant.
fn active_type_taken(
    conn: &Connection,
    tenant: &str,
    customer_type: &str,
    exclude_id: Option<&str>,
) -> Result<bool> {
    let sql = "SELECT COUNT(*) FROM margin_profiles \
               WHERE tenant_id = ? AND customer_type = ? AND archived_at IS NULL \
               AND id != ?;";
    let exclude = exclude_id.unwrap_or("");
    let count: i64 = conn
        .query_row(sql, params![tenant, customer_type, exclude], |r| r.get(0))
        .context("count active margin_profiles by type")?;
    Ok(count > 0)
}

/// Insert a new profile. Inputs MUST be pre-validated by the caller.
/// Returns [`CreateOutcome::DuplicateActiveType`] if an active profile
/// already exists for the same customer type (invariant in code).
pub fn create_profile(
    conn: &Connection,
    tenant: &str,
    inputs: &MarginProfileInputs,
) -> Result<CreateOutcome> {
    ensure_schema(conn)?;
    let customer_type = CustomerType::from_db_str(inputs.customer_type.trim())
        .context("customer_type validated before create")?
        .as_db_str();
    if active_type_taken(conn, tenant, customer_type, None)? {
        return Ok(CreateOutcome::DuplicateActiveType);
    }
    let id = format!("mp_{}", Ulid::new());
    let now = now_rfc3339()?;
    let notes = inputs
        .notes
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    conn.execute(
        "INSERT INTO margin_profiles (id, tenant_id, name, customer_type, gross_margin_pct, \
         min_margin_pct, notes, enabled, created_at, updated_at, archived_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL);",
        params![
            &id,
            tenant,
            inputs.name.trim(),
            customer_type,
            inputs.gross_margin_pct,
            inputs.min_margin_pct,
            notes,
            inputs.enabled,
            &now,
            &now,
        ],
    )
    .context("INSERT into margin_profiles")?;
    Ok(CreateOutcome::Created(MarginProfile {
        id,
        name: inputs.name.trim().to_string(),
        customer_type: customer_type.to_string(),
        gross_margin_pct: inputs.gross_margin_pct,
        min_margin_pct: inputs.min_margin_pct,
        notes: notes.map(str::to_string),
        enabled: inputs.enabled,
        created_at: now.clone(),
        updated_at: now,
        archived_at: None,
    }))
}

/// Fetch a single profile (archived or not) by id.
pub fn get_profile(conn: &Connection, tenant: &str, id: &str) -> Result<Option<MarginProfile>> {
    ensure_schema(conn)?;
    let sql = format!("SELECT {COLS} FROM margin_profiles WHERE tenant_id = ? AND id = ?;");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query_map(params![tenant, id], row_to_profile)?;
    match rows.next() {
        Some(r) => Ok(Some(r?)),
        None => Ok(None),
    }
}

/// List active (non-archived) profiles, name ascending.
pub fn list_profiles(conn: &Connection, tenant: &str) -> Result<Vec<MarginProfile>> {
    ensure_schema(conn)?;
    let sql = format!(
        "SELECT {COLS} FROM margin_profiles WHERE tenant_id = ? AND archived_at IS NULL \
         ORDER BY name ASC;"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![tenant], row_to_profile)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// The active (non-archived AND enabled) profile that the pricing engine
/// applies for a customer type, if any. The unique-active invariant means
/// there is at most one non-archived row per type; a disabled one is
/// treated as "no profile" (global default applies).
pub fn active_profile_for_customer_type(
    conn: &Connection,
    tenant: &str,
    customer_type: CustomerType,
) -> Result<Option<MarginProfile>> {
    ensure_schema(conn)?;
    let sql = format!(
        "SELECT {COLS} FROM margin_profiles \
         WHERE tenant_id = ? AND customer_type = ? AND archived_at IS NULL AND enabled = TRUE \
         LIMIT 1;"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query_map(params![tenant, customer_type.as_db_str()], row_to_profile)?;
    match rows.next() {
        Some(r) => Ok(Some(r?)),
        None => Ok(None),
    }
}

/// Update an existing, non-archived profile. Returns
/// [`UpdateOutcome::NotFound`] if no active row exists, or
/// [`UpdateOutcome::DuplicateActiveType`] if the new customer type
/// collides with another active profile.
pub fn update_profile(
    conn: &Connection,
    tenant: &str,
    id: &str,
    inputs: &MarginProfileInputs,
) -> Result<UpdateOutcome> {
    ensure_schema(conn)?;
    let customer_type = CustomerType::from_db_str(inputs.customer_type.trim())
        .context("customer_type validated before update")?
        .as_db_str();
    // The row must exist + be active first (so a NotFound beats a spurious
    // duplicate check against a non-existent edit target).
    match get_profile(conn, tenant, id)? {
        Some(p) if p.archived_at.is_none() => {}
        _ => return Ok(UpdateOutcome::NotFound),
    }
    if active_type_taken(conn, tenant, customer_type, Some(id))? {
        return Ok(UpdateOutcome::DuplicateActiveType);
    }
    let now = now_rfc3339()?;
    let notes = inputs
        .notes
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let changed = conn
        .execute(
            "UPDATE margin_profiles SET name = ?, customer_type = ?, gross_margin_pct = ?, \
             min_margin_pct = ?, notes = ?, enabled = ?, updated_at = ? \
             WHERE tenant_id = ? AND id = ? AND archived_at IS NULL;",
            params![
                inputs.name.trim(),
                customer_type,
                inputs.gross_margin_pct,
                inputs.min_margin_pct,
                notes,
                inputs.enabled,
                &now,
                tenant,
                id,
            ],
        )
        .context("UPDATE margin_profiles")?;
    if changed == 0 {
        return Ok(UpdateOutcome::NotFound);
    }
    match get_profile(conn, tenant, id)? {
        Some(p) => Ok(UpdateOutcome::Updated(p)),
        None => Ok(UpdateOutcome::NotFound),
    }
}

/// Archive (soft-delete) a profile. Returns the `archived_at` timestamp on
/// success, `None` if the row was absent or already archived. No hard
/// delete — the row stays for historical pricing forensics.
pub fn archive_profile(conn: &Connection, tenant: &str, id: &str) -> Result<Option<String>> {
    ensure_schema(conn)?;
    let now = now_rfc3339()?;
    let changed = conn
        .execute(
            "UPDATE margin_profiles SET archived_at = ?, updated_at = ? \
             WHERE tenant_id = ? AND id = ? AND archived_at IS NULL;",
            params![&now, &now, tenant, id],
        )
        .context("UPDATE margin_profiles SET archived_at")?;
    Ok(if changed > 0 { Some(now) } else { None })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        let c = Connection::open_in_memory().expect("in-memory duckdb");
        ensure_schema(&c).expect("schema");
        c
    }

    fn inputs(ct: &str, gross: f64, min: f64) -> MarginProfileInputs {
        MarginProfileInputs {
            name: format!("{ct} profile"),
            customer_type: ct.to_string(),
            gross_margin_pct: gross,
            min_margin_pct: min,
            notes: None,
            enabled: true,
        }
    }

    #[test]
    fn create_then_lookup_by_customer_type() {
        let c = conn();
        let made = create_profile(&c, "t", &inputs("defense", 0.4, 0.1)).unwrap();
        let CreateOutcome::Created(p) = made else {
            panic!("expected Created");
        };
        assert!(p.id.starts_with("mp_"));
        let found = active_profile_for_customer_type(&c, "t", CustomerType::Defense)
            .unwrap()
            .expect("profile present");
        assert_eq!(found.id, p.id);
        assert_eq!(found.gross_margin_pct, 0.4);
    }

    #[test]
    fn unset_customer_type_has_no_profile_unless_created() {
        let c = conn();
        create_profile(&c, "t", &inputs("defense", 0.4, 0.1)).unwrap();
        // A different type returns None → caller falls back to global.
        assert!(
            active_profile_for_customer_type(&c, "t", CustomerType::Industrial)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn refuses_duplicate_active_per_type() {
        let c = conn();
        create_profile(&c, "t", &inputs("defense", 0.4, 0.1)).unwrap();
        let dup = create_profile(&c, "t", &inputs("defense", 0.5, 0.2)).unwrap();
        assert_eq!(dup, CreateOutcome::DuplicateActiveType);
    }

    #[test]
    fn archive_frees_the_type_for_a_new_profile() {
        let c = conn();
        let CreateOutcome::Created(p) =
            create_profile(&c, "t", &inputs("defense", 0.4, 0.1)).unwrap()
        else {
            panic!("created");
        };
        // archive-not-delete: row stays, but type is free again.
        assert!(archive_profile(&c, "t", &p.id).unwrap().is_some());
        assert!(get_profile(&c, "t", &p.id)
            .unwrap()
            .unwrap()
            .archived_at
            .is_some());
        let again = create_profile(&c, "t", &inputs("defense", 0.5, 0.2)).unwrap();
        assert!(matches!(again, CreateOutcome::Created(_)));
        // archived profile no longer drives pricing.
        let active = active_profile_for_customer_type(&c, "t", CustomerType::Defense)
            .unwrap()
            .unwrap();
        assert_ne!(active.id, p.id);
    }

    #[test]
    fn disabled_profile_is_not_applied_but_still_blocks_duplicate() {
        let c = conn();
        let mut i = inputs("defense", 0.4, 0.1);
        i.enabled = false;
        create_profile(&c, "t", &i).unwrap();
        // disabled → no pricing profile
        assert!(
            active_profile_for_customer_type(&c, "t", CustomerType::Defense)
                .unwrap()
                .is_none()
        );
        // but the active (non-archived) slot is taken
        let dup = create_profile(&c, "t", &inputs("defense", 0.5, 0.2)).unwrap();
        assert_eq!(dup, CreateOutcome::DuplicateActiveType);
    }

    #[test]
    fn validation_surfaces_every_problem_at_once() {
        let bad = MarginProfileInputs {
            name: "  ".to_string(),
            customer_type: "nope".to_string(),
            gross_margin_pct: -1.0,
            min_margin_pct: 1.5,
            notes: None,
            enabled: true,
        };
        let errs = validate_profile_inputs(&bad).unwrap_err();
        let fields: Vec<_> = errs.iter().map(|e| e.field).collect();
        assert!(fields.contains(&"name"));
        assert!(fields.contains(&"customer_type"));
        assert!(fields.contains(&"gross_margin_pct"));
        assert!(fields.contains(&"min_margin_pct"));
    }
}
