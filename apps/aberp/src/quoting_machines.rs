//! S427 — `quoting_machines` master data.
//!
//! Operator-managed catalogue of the shop's machines. The capacity-
//! aware lead-time model ([`aberp_quote_engine::lead_time_days`]) loads
//! the enabled rows at quote-start; the operator edits a machine in one
//! form and the engine always reloads, so there is never a decision
//! about whether a change "takes" ([[hulye-biztos]]).
//!
//! ## Conventions mirrored from [`crate::partners`]
//!
//! Prefixed-ULID id (`qcm_<ULID>`), lazy `CREATE TABLE IF NOT EXISTS`,
//! invariants enforced **in code** not via SQL CHECK/triggers
//! ([[no-sql-specific]]), archive-not-delete soft lifecycle
//! (`archived_at`). The `family` column stores the stable db-string
//! ([`aberp_quote_engine::MachineFamily::as_db_str`]); the SPA and wire
//! use the same string so there is exactly one family encoding.
//!
//! ## Divergence from partners: this module DOES fire audit
//!
//! Partners deliberately stays off the audit ledger (its history is the
//! row timestamps). Machine CRUD, by contrast, emits
//! `MachineCreated`/`MachineEdited`/`MachineArchived` — the brief
//! requires it, and a machine's capacity is a pricing input whose
//! changes the operator needs an audit trail for. Emission lives in
//! [`append_machine_event`], called by the serve request wrappers after
//! the DB write (same split as `record_numbering_change_audit`).

use anyhow::{Context, Result};
use duckdb::{params, Connection};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use ulid::Ulid;

use aberp_audit_ledger::{Actor, BinaryHash, EventKind, LedgerMeta, TenantId};
use aberp_db::HandleArc;
use aberp_quote_engine::{MachineCapacity, MachineFamily};

/// Default schedulable hours per day for a new machine (brief default).
pub const DEFAULT_DAILY_HOURS: f64 = 16.0;
/// Default planning buffer percentage for a new machine (brief default).
pub const DEFAULT_BUFFER_PCT: f64 = 20.0;

fn default_daily_hours() -> f64 {
    DEFAULT_DAILY_HOURS
}
fn default_buffer_pct() -> f64 {
    DEFAULT_BUFFER_PCT
}
fn default_enabled() -> bool {
    true
}

/// A machine row, wire + storage shape.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct QuotingMachine {
    /// `qcm_<26-char-ULID>`.
    pub id: String,
    pub name: String,
    /// `MachineFamily` db-string, e.g. `3-axis-mill`.
    pub family: String,
    /// Max working envelope `[x, y, z]` in millimetres (0 = unspecified).
    pub max_envelope_xyz_mm: [f64; 3],
    pub daily_hours_avail: f64,
    pub buffer_pct: f64,
    pub enabled: bool,
    pub created_at: String,
    pub updated_at: String,
    /// `None` while active; Rfc3339 once archived.
    pub archived_at: Option<String>,
}

/// Request body for create/update.
#[derive(Deserialize, Debug, Clone)]
pub struct MachineInputs {
    pub name: String,
    pub family: String,
    #[serde(default)]
    pub max_envelope_xyz_mm: [f64; 3],
    #[serde(default = "default_daily_hours")]
    pub daily_hours_avail: f64,
    #[serde(default = "default_buffer_pct")]
    pub buffer_pct: f64,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

/// Field-level validation error (wire shape for 400 responses).
#[derive(Serialize, Debug, PartialEq, Eq, Clone)]
pub struct ValidationError {
    pub field: &'static str,
    pub message: String,
}

/// Validate inputs in code (no SQL CHECK). Surfaces every error at once
/// (CLAUDE.md rule 9 / 12) rather than failing on the first.
pub fn validate_machine_inputs(inputs: &MachineInputs) -> Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();

    if inputs.name.trim().is_empty() {
        errors.push(ValidationError {
            field: "name",
            message: "A gép neve kötelező. / Machine name is required.".to_string(),
        });
    } else if inputs.name.trim().len() > 120 {
        errors.push(ValidationError {
            field: "name",
            message: "A gép neve legfeljebb 120 karakter. / Machine name max 120 chars."
                .to_string(),
        });
    }

    if MachineFamily::from_db_str(inputs.family.trim()).is_none() {
        errors.push(ValidationError {
            field: "family",
            message: format!(
                "Ismeretlen gépcsalád: {:?}. / Unknown machine family.",
                inputs.family
            ),
        });
    }

    if !(inputs.daily_hours_avail.is_finite()
        && inputs.daily_hours_avail > 0.0
        && inputs.daily_hours_avail <= 24.0)
    {
        errors.push(ValidationError {
            field: "daily_hours_avail",
            message: "A napi óraszám 0 és 24 között legyen. / Daily hours must be in (0, 24]."
                .to_string(),
        });
    }

    if !(inputs.buffer_pct.is_finite() && (0.0..100.0).contains(&inputs.buffer_pct)) {
        errors.push(ValidationError {
            field: "buffer_pct",
            message: "A puffer 0 és 100% között legyen. / Buffer must be in [0, 100).".to_string(),
        });
    }

    for (axis, v) in ["x", "y", "z"]
        .iter()
        .zip(inputs.max_envelope_xyz_mm.iter())
    {
        if !(v.is_finite() && *v >= 0.0) {
            errors.push(ValidationError {
                field: "max_envelope_xyz_mm",
                message: format!(
                    "Az envelop {axis} tengely nem lehet negatív. / Envelope {axis} must be ≥ 0."
                ),
            });
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS quoting_machines (
    id                VARCHAR NOT NULL PRIMARY KEY,
    tenant_id         VARCHAR NOT NULL,
    name              VARCHAR NOT NULL,
    family            VARCHAR NOT NULL,
    max_envelope_x_mm DOUBLE  NOT NULL,
    max_envelope_y_mm DOUBLE  NOT NULL,
    max_envelope_z_mm DOUBLE  NOT NULL,
    daily_hours_avail DOUBLE  NOT NULL,
    buffer_pct        DOUBLE  NOT NULL,
    enabled           BOOLEAN NOT NULL,
    created_at        VARCHAR NOT NULL,
    updated_at        VARCHAR NOT NULL,
    archived_at       VARCHAR
);
";

/// Idempotent table creation. Called at serve boot + defensively on
/// each request entry point. No SQL CHECK/index ([[no-sql-specific]] —
/// the table is small master data, scanned in full).
pub fn ensure_schema(conn: &Connection) -> Result<()> {
    // ADR-0098 C2 fix-forward — no-op on a read-only conn (read_returns_readonly
    // read()-side); the schema is created by a writer before any read reaches
    // here. A genuine write mis-routed through read() still fails loud (F5).
    if aberp_audit_ledger::connection_is_read_only(conn) {
        return Ok(());
    }
    conn.execute_batch(SCHEMA_SQL)
        .context("ensure quoting_machines schema")
}

fn now_rfc3339() -> Result<String> {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .context("format quoting_machines timestamp")
}

const COLS: &str = "id, name, family, max_envelope_x_mm, max_envelope_y_mm, max_envelope_z_mm, \
                    daily_hours_avail, buffer_pct, enabled, created_at, updated_at, archived_at";

fn row_to_machine(row: &duckdb::Row<'_>) -> duckdb::Result<QuotingMachine> {
    Ok(QuotingMachine {
        id: row.get(0)?,
        name: row.get(1)?,
        family: row.get(2)?,
        max_envelope_xyz_mm: [row.get(3)?, row.get(4)?, row.get(5)?],
        daily_hours_avail: row.get(6)?,
        buffer_pct: row.get(7)?,
        enabled: row.get(8)?,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
        archived_at: row.get(11)?,
    })
}

/// Insert a new machine. Inputs MUST be pre-validated by the caller
/// (the serve wrapper calls [`validate_machine_inputs`] first).
pub fn create_machine(
    conn: &Connection,
    tenant: &str,
    inputs: &MachineInputs,
) -> Result<QuotingMachine> {
    ensure_schema(conn)?;
    let id = format!("qcm_{}", Ulid::new());
    let now = now_rfc3339()?;
    let family = MachineFamily::from_db_str(inputs.family.trim())
        .context("family validated before create")?
        .as_db_str();
    let env = inputs.max_envelope_xyz_mm;
    conn.execute(
        "INSERT INTO quoting_machines (id, tenant_id, name, family, max_envelope_x_mm, \
         max_envelope_y_mm, max_envelope_z_mm, daily_hours_avail, buffer_pct, enabled, \
         created_at, updated_at, archived_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL);",
        params![
            &id,
            tenant,
            inputs.name.trim(),
            family,
            env[0],
            env[1],
            env[2],
            inputs.daily_hours_avail,
            inputs.buffer_pct,
            inputs.enabled,
            &now,
            &now,
        ],
    )
    .context("INSERT into quoting_machines")?;
    Ok(QuotingMachine {
        id,
        name: inputs.name.trim().to_string(),
        family: family.to_string(),
        max_envelope_xyz_mm: env,
        daily_hours_avail: inputs.daily_hours_avail,
        buffer_pct: inputs.buffer_pct,
        enabled: inputs.enabled,
        created_at: now.clone(),
        updated_at: now,
        archived_at: None,
    })
}

/// Fetch a single machine (archived or not) by id.
pub fn get_machine(conn: &Connection, tenant: &str, id: &str) -> Result<Option<QuotingMachine>> {
    ensure_schema(conn)?;
    let sql = format!("SELECT {COLS} FROM quoting_machines WHERE tenant_id = ? AND id = ?;");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query_map(params![tenant, id], row_to_machine)?;
    match rows.next() {
        Some(r) => Ok(Some(r?)),
        None => Ok(None),
    }
}

/// List active (non-archived) machines, newest-edited first.
pub fn list_machines(conn: &Connection, tenant: &str) -> Result<Vec<QuotingMachine>> {
    ensure_schema(conn)?;
    let sql = format!(
        "SELECT {COLS} FROM quoting_machines WHERE tenant_id = ? AND archived_at IS NULL \
         ORDER BY name ASC;"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![tenant], row_to_machine)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Update an existing, non-archived machine. Returns `None` if no such
/// active row exists (404 at the route).
pub fn update_machine(
    conn: &Connection,
    tenant: &str,
    id: &str,
    inputs: &MachineInputs,
) -> Result<Option<QuotingMachine>> {
    ensure_schema(conn)?;
    let family = MachineFamily::from_db_str(inputs.family.trim())
        .context("family validated before update")?
        .as_db_str();
    let env = inputs.max_envelope_xyz_mm;
    let now = now_rfc3339()?;
    let changed = conn
        .execute(
            "UPDATE quoting_machines SET name = ?, family = ?, max_envelope_x_mm = ?, \
             max_envelope_y_mm = ?, max_envelope_z_mm = ?, daily_hours_avail = ?, \
             buffer_pct = ?, enabled = ?, updated_at = ? \
             WHERE tenant_id = ? AND id = ? AND archived_at IS NULL;",
            params![
                inputs.name.trim(),
                family,
                env[0],
                env[1],
                env[2],
                inputs.daily_hours_avail,
                inputs.buffer_pct,
                inputs.enabled,
                &now,
                tenant,
                id,
            ],
        )
        .context("UPDATE quoting_machines")?;
    if changed == 0 {
        return Ok(None);
    }
    get_machine(conn, tenant, id)
}

/// Archive (soft-delete) a machine. Returns the `archived_at` timestamp
/// on success, `None` if the row was absent or already archived. No
/// hard delete — the row stays for historical lead-time forensics.
pub fn archive_machine(conn: &Connection, tenant: &str, id: &str) -> Result<Option<String>> {
    ensure_schema(conn)?;
    let now = now_rfc3339()?;
    let changed = conn
        .execute(
            "UPDATE quoting_machines SET archived_at = ?, updated_at = ? \
             WHERE tenant_id = ? AND id = ? AND archived_at IS NULL;",
            params![&now, &now, tenant, id],
        )
        .context("UPDATE quoting_machines SET archived_at")?;
    Ok(if changed > 0 { Some(now) } else { None })
}

/// The enabled, non-archived machines reduced to the engine's capacity
/// input. A row with an unparseable family is skipped loud (logged by
/// the caller via the returned skipped count) rather than silently
/// dropped — defence against schema drift (CLAUDE.md rule 12).
pub fn list_enabled_capacities(conn: &Connection, tenant: &str) -> Result<Vec<MachineCapacity>> {
    ensure_schema(conn)?;
    let mut out = Vec::new();
    for m in list_machines(conn, tenant)? {
        if !m.enabled {
            continue;
        }
        let Some(family) = MachineFamily::from_db_str(&m.family) else {
            // Unparseable family on an enabled row — skip, but loudly:
            // this means the table holds a string no engine variant
            // knows. The boot/CRUD validators prevent it; if it appears
            // it is corruption worth not crashing pricing over.
            eprintln!(
                "WARN quoting_machines: enabled machine {} has unknown family {:?}; skipped from capacity",
                m.id, m.family
            );
            continue;
        };
        out.push(MachineCapacity {
            family,
            daily_hours_avail: m.daily_hours_avail,
            buffer_pct: m.buffer_pct,
        });
    }
    Ok(out)
}

/// Append a machine-lifecycle audit entry. Split from the DB write so
/// the write half stays unit-testable without a ledger, mirroring
/// `record_numbering_change_audit`.
pub fn append_machine_event(
    db: &HandleArc,
    tenant: TenantId,
    binary_hash: BinaryHash,
    operator_login: &str,
    kind: EventKind,
    payload: Vec<u8>,
) -> Result<()> {
    // ADR-0099 — route the machine-lifecycle audit append through the ONE
    // shared `aberp_db::Handle` writer (`db.write()`) instead of an
    // independent `Ledger::open` on the live tenant DB. These wrappers run
    // in-process under `aberp serve` (the machine / margin / lead-time CRUD
    // handlers); an independent opener off a stale chain head self-assigns an
    // already-used seq (the 369→515 fork class) while a daemon writes the same
    // chain. The serialized writer re-reads the head under its mutex and the
    // WriteGuard drop runs the lockstep mirror sync (no separate opener).
    let mut guard = db
        .write()
        .map_err(|e| anyhow::anyhow!("shared writer for machine audit event (ADR-0099): {e}"))?;
    aberp_audit_ledger::ensure_schema(&guard)
        .context("ensure audit-ledger schema for machine event")?;
    let tx = guard
        .transaction()
        .context("begin DuckDB transaction for machine event")?;
    let meta = LedgerMeta::new(tenant, binary_hash);
    let actor = Actor::from_local_cli(Ulid::new().to_string(), operator_login);
    aberp_audit_ledger::append_in_tx(&tx, &meta, kind, payload, actor, None)
        .map_err(|e| anyhow::anyhow!("append machine audit entry via shared Handle: {e}"))?;
    tx.commit()
        .context("commit DuckDB transaction for machine event")?;
    Ok(())
}
