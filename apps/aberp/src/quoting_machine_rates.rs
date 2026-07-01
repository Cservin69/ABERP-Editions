//! S4 / ADR-0094 Gap 2 (wiring) — `quoting_machine_rates` catalogue.
//!
//! The engine half (S3) shipped [`aberp_quote_engine::MachineRate`] and
//! [`aberp_quote_engine::quote_with_shop_model`], which prices the routed
//! machine family at its own EUR/min (a bar-fed Swiss running lights-out
//! prices a small turned part below an attended 3-axis mill). This module is
//! the missing data layer: the operator-managed, family-keyed rate table the
//! pricing pipeline snapshots into the engine.
//!
//! ## Conventions mirrored from [`crate::quoting_machines`] + the tunables
//!
//! Prefixed-ULID id (`qmr_<ULID>`), lazy `CREATE TABLE IF NOT EXISTS`,
//! invariants enforced **in code** not via SQL CHECK/trigger
//! ([[no-sql-specific]]). The `family` column stores the stable db-string
//! ([`aberp_quote_engine::MachineFamily::as_db_str`]); the closed-vocab
//! check round-trips through [`aberp_quote_engine::MachineFamily::from_db_str`]
//! (so the S3 enum extension — Swiss/turn-mill/4-axis — is covered for free,
//! no second list to keep in sync). There is exactly **one rate per family**
//! per tenant — the family is the natural unique key (enforced in code like
//! `quoting_stock_adjustments`' composite key).
//!
//! ## Audit
//!
//! CRUD emits via the audit ledger. S4 **reuses** [`EventKind::ParametersChanged`]
//! (the quoting-tunables-changed kind) rather than introducing a dedicated
//! `MachineRatesChanged` variant: `EventKind` is not `#[non_exhaustive]` and
//! has ~186 variants matched across crates the 45s/4 GB sandbox cannot
//! compile-verify, so a new variant is an unacceptable blast radius here
//! (ADR-0094 toolchain-honesty clause). The payload is self-describing
//! (`"catalogue":"quoting_machine_rates"`) so a future migration to a
//! dedicated kind is a pure relabel. FLAGGED for S5/CI follow-up.

use anyhow::{Context, Result};
use duckdb::{params, Connection};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use ulid::Ulid;

use aberp_audit_ledger::{append_in_tx, Actor, EventKind, LedgerMeta};
use aberp_quote_engine::MachineFamily;

// Reuse the tunables write-error + validation-error vocab so the serve
// layer's `tunable_write_response` maps machine-rate failures identically.
use crate::quoting_tunables::{TunableWriteError, ValidationError};

/// Wire + storage shape of a `quoting_machine_rates` row.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct MachineRateRow {
    /// `qmr_<26-char-ULID>`.
    pub id: String,
    /// `MachineFamily` db-string, e.g. `swiss-turn-mill`.
    pub family: String,
    /// The family's attended EUR/min (dedicated operator).
    pub attended_rate_eur_per_min: f64,
    /// Multiplier in (0, 1] applied to the attended rate when the job runs
    /// unattended (lights-out). Ignored by the engine unless
    /// `unattended_capable` AND the job qualifies.
    pub lights_out_factor: f64,
    /// Whether this family can run unattended.
    pub unattended_capable: bool,
    pub notes: Option<String>,
    pub updated_at: String,
    pub updated_by_actor: String,
}

/// Request body for create/update.
#[derive(Deserialize, Debug, Clone)]
pub struct MachineRateInputs {
    #[serde(default)]
    pub family: String,
    #[serde(default)]
    pub attended_rate_eur_per_min: f64,
    #[serde(default = "default_lights_out_factor")]
    pub lights_out_factor: f64,
    #[serde(default)]
    pub unattended_capable: bool,
    #[serde(default)]
    pub notes: Option<String>,
}

/// Neutral default for `lights_out_factor` (1.0 ⇒ no discount), so an
/// attended-only family's row needs only the rate + capability flag.
fn default_lights_out_factor() -> f64 {
    1.0
}

/// One seed row: family + its proposed day-1 numbers (ADR-0094 Gap 2).
struct Seed {
    family: MachineFamily,
    attended_rate_eur_per_min: f64,
    lights_out_factor: f64,
    unattended_capable: bool,
}

/// The six families ADR-0094 Gap 2 seeds. 3-axis carries today's global
/// rate (1.6667) so a seeded shop's prismatic parts price exactly as before;
/// Swiss + turn-mill are the unattended families that drive the Gap-2 win.
const SEEDS: &[Seed] = &[
    Seed {
        family: MachineFamily::SwissTurnMill,
        attended_rate_eur_per_min: 1.50,
        lights_out_factor: 0.35,
        unattended_capable: true,
    },
    Seed {
        family: MachineFamily::TurnMill,
        attended_rate_eur_per_min: 1.60,
        lights_out_factor: 0.45,
        unattended_capable: true,
    },
    Seed {
        family: MachineFamily::ThreeAxisMill,
        attended_rate_eur_per_min: 1.6667,
        lights_out_factor: 1.0,
        unattended_capable: false,
    },
    Seed {
        family: MachineFamily::FourAxisMill,
        attended_rate_eur_per_min: 1.90,
        lights_out_factor: 1.0,
        unattended_capable: false,
    },
    Seed {
        family: MachineFamily::FiveAxisMill,
        attended_rate_eur_per_min: 2.50,
        lights_out_factor: 1.0,
        unattended_capable: false,
    },
    Seed {
        family: MachineFamily::Lathe,
        attended_rate_eur_per_min: 1.50,
        lights_out_factor: 1.0,
        unattended_capable: false,
    },
];

/// Validate inputs in code (no SQL CHECK). Surfaces every error at once
/// (CLAUDE.md rule 9 / 12). The `family` closed-vocab check round-trips
/// through `MachineFamily::from_db_str`, so the S3 enum extension is
/// covered without a second list.
pub fn validate_machine_rate_inputs(
    inputs: &MachineRateInputs,
) -> Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();

    if MachineFamily::from_db_str(inputs.family.trim()).is_none() {
        errors.push(ValidationError {
            field: "family",
            message: format!(
                "Ismeretlen gépcsalád: {:?}. / Unknown machine family.",
                inputs.family
            ),
        });
    }

    if !(inputs.attended_rate_eur_per_min.is_finite() && inputs.attended_rate_eur_per_min > 0.0) {
        errors.push(ValidationError {
            field: "attended_rate_eur_per_min",
            message: "Az óradíj/perc legyen véges és > 0. / Attended rate must be finite and > 0."
                .to_string(),
        });
    }

    if !(inputs.lights_out_factor.is_finite()
        && inputs.lights_out_factor > 0.0
        && inputs.lights_out_factor <= 1.0)
    {
        errors.push(ValidationError {
            field: "lights_out_factor",
            message: "A lights-out szorzó (0, 1] tartományban legyen. / Lights-out factor must be in (0, 1]."
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
CREATE TABLE IF NOT EXISTS quoting_machine_rates (
    id                        VARCHAR NOT NULL PRIMARY KEY,
    tenant_id                 VARCHAR NOT NULL,
    family                    VARCHAR NOT NULL,
    attended_rate_eur_per_min DOUBLE  NOT NULL,
    lights_out_factor         DOUBLE  NOT NULL,
    unattended_capable        BOOLEAN NOT NULL,
    notes                     VARCHAR,
    updated_at                VARCHAR NOT NULL,
    updated_by_actor          VARCHAR NOT NULL
);
";

const COLS: &str = "id, family, attended_rate_eur_per_min, lights_out_factor, \
                    unattended_capable, notes, updated_at, updated_by_actor";

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
        .context("ensure quoting_machine_rates schema")
}

/// Seed the six ADR-0094 families, **insert-if-absent** per family — so a
/// re-run (or a partially-seeded table) never duplicates and never clobbers
/// an operator-tuned value. Idempotent: gated per `(tenant, family)`.
pub fn seed_machine_rates_if_absent(conn: &Connection, tenant: &str) -> Result<()> {
    ensure_schema(conn)?;
    let now = now_rfc3339()?;
    for seed in SEEDS {
        let family = seed.family.as_db_str();
        let existing: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM quoting_machine_rates WHERE tenant_id = ? AND family = ?;",
                params![tenant, family],
                |r| r.get(0),
            )
            .context("count quoting_machine_rates for seed gate")?;
        if existing > 0 {
            continue;
        }
        let id = format!("qmr_{}", Ulid::new());
        conn.execute(
            "INSERT INTO quoting_machine_rates (id, tenant_id, family, \
             attended_rate_eur_per_min, lights_out_factor, unattended_capable, \
             notes, updated_at, updated_by_actor) \
             VALUES (?, ?, ?, ?, ?, ?, NULL, ?, 'boot');",
            params![
                &id,
                tenant,
                family,
                seed.attended_rate_eur_per_min,
                seed.lights_out_factor,
                seed.unattended_capable,
                &now,
            ],
        )
        .with_context(|| format!("seed quoting_machine_rates row for {family}"))?;
    }
    Ok(())
}

fn now_rfc3339() -> Result<String> {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .context("format quoting_machine_rates timestamp")
}

fn row_to_machine_rate(row: &duckdb::Row<'_>) -> duckdb::Result<MachineRateRow> {
    Ok(MachineRateRow {
        id: row.get(0)?,
        family: row.get(1)?,
        attended_rate_eur_per_min: row.get(2)?,
        lights_out_factor: row.get(3)?,
        unattended_capable: row.get(4)?,
        notes: row.get(5)?,
        updated_at: row.get(6)?,
        updated_by_actor: row.get(7)?,
    })
}

/// All rate rows for a tenant, family-ordered (stable list for the SPA).
pub fn list_machine_rates(conn: &Connection, tenant: &str) -> Result<Vec<MachineRateRow>> {
    ensure_schema(conn)?;
    let sql = format!(
        "SELECT {COLS} FROM quoting_machine_rates WHERE tenant_id = ? ORDER BY family ASC;"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![tenant], row_to_machine_rate)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

fn get_machine_rate(conn: &Connection, tenant: &str, id: &str) -> Result<Option<MachineRateRow>> {
    let sql = format!("SELECT {COLS} FROM quoting_machine_rates WHERE tenant_id = ? AND id = ?;");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query_map(params![tenant, id], row_to_machine_rate)?;
    match rows.next() {
        Some(r) => Ok(Some(r?)),
        None => Ok(None),
    }
}

/// Count rows holding `family` other than `except_id` — the in-code
/// one-rate-per-family uniqueness guard (no SQL UNIQUE, [[no-sql-specific]]).
fn family_taken_by_other(
    conn: &Connection,
    tenant: &str,
    family: &str,
    except_id: &str,
) -> Result<bool> {
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM quoting_machine_rates \
             WHERE tenant_id = ? AND family = ? AND id != ?;",
            params![tenant, family, except_id],
            |r| r.get(0),
        )
        .context("check quoting_machine_rates family uniqueness")?;
    Ok(n > 0)
}

/// Create a rate for a family (one per family). `Conflict` if the family
/// already has a row — the operator edits the existing one instead.
pub fn create_machine_rate(
    conn: &mut Connection,
    meta: &LedgerMeta,
    actor_login: &str,
    tenant: &str,
    inputs: &MachineRateInputs,
) -> Result<MachineRateRow, TunableWriteError> {
    if let Err(e) = validate_machine_rate_inputs(inputs) {
        return Err(TunableWriteError::Validation(e));
    }
    ensure_schema(conn).map_err(TunableWriteError::Other)?;
    let family = MachineFamily::from_db_str(inputs.family.trim())
        .context("family validated before create")
        .map_err(TunableWriteError::Other)?
        .as_db_str();
    if family_taken_by_other(conn, tenant, family, "").map_err(TunableWriteError::Other)? {
        return Err(TunableWriteError::Conflict(format!(
            "a rate for family `{family}` already exists — edit it instead"
        )));
    }
    let now = now_rfc3339().map_err(TunableWriteError::Other)?;
    let notes = normalize_optional(inputs.notes.as_deref());
    let id = format!("qmr_{}", Ulid::new());
    let tx = conn
        .transaction()
        .context("begin create_machine_rate tx")
        .map_err(TunableWriteError::Other)?;
    tx.execute(
        "INSERT INTO quoting_machine_rates (id, tenant_id, family, \
         attended_rate_eur_per_min, lights_out_factor, unattended_capable, \
         notes, updated_at, updated_by_actor) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?);",
        params![
            &id,
            tenant,
            family,
            inputs.attended_rate_eur_per_min,
            inputs.lights_out_factor,
            inputs.unattended_capable,
            notes.as_deref(),
            &now,
            actor_login,
        ],
    )
    .context("INSERT quoting_machine_rates")
    .map_err(TunableWriteError::Other)?;
    let row = read_in_tx(&tx, tenant, &id).map_err(TunableWriteError::Other)?;
    append_machine_rate_change(&tx, meta, actor_login, "machine_rate_create", &row)
        .map_err(TunableWriteError::Other)?;
    tx.commit()
        .context("commit create_machine_rate")
        .map_err(TunableWriteError::Other)?;
    Ok(row)
}

/// Update a rate by id. `NotFound` if the row is absent; `Conflict` if the
/// edited `family` collides with another row.
pub fn update_machine_rate(
    conn: &mut Connection,
    meta: &LedgerMeta,
    actor_login: &str,
    tenant: &str,
    id: &str,
    inputs: &MachineRateInputs,
) -> Result<MachineRateRow, TunableWriteError> {
    if let Err(e) = validate_machine_rate_inputs(inputs) {
        return Err(TunableWriteError::Validation(e));
    }
    ensure_schema(conn).map_err(TunableWriteError::Other)?;
    let family = MachineFamily::from_db_str(inputs.family.trim())
        .context("family validated before update")
        .map_err(TunableWriteError::Other)?
        .as_db_str();
    if get_machine_rate(conn, tenant, id)
        .map_err(TunableWriteError::Other)?
        .is_none()
    {
        return Err(TunableWriteError::NotFound(format!(
            "quoting_machine_rates row {id} not found"
        )));
    }
    if family_taken_by_other(conn, tenant, family, id).map_err(TunableWriteError::Other)? {
        return Err(TunableWriteError::Conflict(format!(
            "another rate for family `{family}` already exists"
        )));
    }
    let now = now_rfc3339().map_err(TunableWriteError::Other)?;
    let notes = normalize_optional(inputs.notes.as_deref());
    let tx = conn
        .transaction()
        .context("begin update_machine_rate tx")
        .map_err(TunableWriteError::Other)?;
    tx.execute(
        "UPDATE quoting_machine_rates SET family = ?, attended_rate_eur_per_min = ?, \
         lights_out_factor = ?, unattended_capable = ?, notes = ?, updated_at = ?, \
         updated_by_actor = ? WHERE tenant_id = ? AND id = ?;",
        params![
            family,
            inputs.attended_rate_eur_per_min,
            inputs.lights_out_factor,
            inputs.unattended_capable,
            notes.as_deref(),
            &now,
            actor_login,
            tenant,
            id,
        ],
    )
    .context("UPDATE quoting_machine_rates")
    .map_err(TunableWriteError::Other)?;
    let row = read_in_tx(&tx, tenant, id).map_err(TunableWriteError::Other)?;
    append_machine_rate_change(&tx, meta, actor_login, "machine_rate_update", &row)
        .map_err(TunableWriteError::Other)?;
    tx.commit()
        .context("commit update_machine_rate")
        .map_err(TunableWriteError::Other)?;
    Ok(row)
}

/// Hard-delete a rate by id (the family falls back to the global rate in the
/// engine — no orphaned pricing). `NotFound` if absent.
pub fn delete_machine_rate(
    conn: &mut Connection,
    meta: &LedgerMeta,
    actor_login: &str,
    tenant: &str,
    id: &str,
) -> Result<(), TunableWriteError> {
    ensure_schema(conn).map_err(TunableWriteError::Other)?;
    let Some(row) = get_machine_rate(conn, tenant, id).map_err(TunableWriteError::Other)? else {
        return Err(TunableWriteError::NotFound(format!(
            "quoting_machine_rates row {id} not found"
        )));
    };
    let tx = conn
        .transaction()
        .context("begin delete_machine_rate tx")
        .map_err(TunableWriteError::Other)?;
    tx.execute(
        "DELETE FROM quoting_machine_rates WHERE tenant_id = ? AND id = ?;",
        params![tenant, id],
    )
    .context("DELETE quoting_machine_rates")
    .map_err(TunableWriteError::Other)?;
    append_machine_rate_change(&tx, meta, actor_login, "machine_rate_delete", &row)
        .map_err(TunableWriteError::Other)?;
    tx.commit()
        .context("commit delete_machine_rate")
        .map_err(TunableWriteError::Other)?;
    Ok(())
}

// ── Internals ───────────────────────────────────────────────────────────

fn normalize_optional(s: Option<&str>) -> Option<String> {
    s.map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

fn read_in_tx(tx: &duckdb::Transaction<'_>, tenant: &str, id: &str) -> Result<MachineRateRow> {
    let sql = format!("SELECT {COLS} FROM quoting_machine_rates WHERE tenant_id = ? AND id = ?;");
    let mut stmt = tx.prepare(&sql)?;
    let mut rows = stmt.query_map(params![tenant, id], row_to_machine_rate)?;
    match rows.next() {
        Some(r) => Ok(r?),
        None => Err(anyhow::anyhow!(
            "quoting_machine_rates row {id} vanished mid-tx"
        )),
    }
}

/// Append a machine-rate-change audit entry inside the write tx. Reuses
/// [`EventKind::ParametersChanged`] (see module docs / FLAG) with a
/// self-describing payload so a future dedicated kind is a pure relabel.
fn append_machine_rate_change(
    tx: &duckdb::Transaction<'_>,
    meta: &LedgerMeta,
    actor_login: &str,
    op: &str,
    row: &MachineRateRow,
) -> Result<()> {
    let payload = serde_json::json!({
        "catalogue": "quoting_machine_rates",
        "op": op,
        "snapshot": { "row": row },
        "idempotency_key": Ulid::new().to_string(),
    });
    let bytes =
        serde_json::to_vec(&payload).context("serialize machine-rate change audit payload")?;
    let actor = Actor::from_local_cli(Ulid::new().to_string(), actor_login);
    append_in_tx(tx, meta, EventKind::ParametersChanged, bytes, actor, None)
        .context("audit append machine-rate change")?;
    Ok(())
}
