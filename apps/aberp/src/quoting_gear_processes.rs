//! S6 / ADR-0094 Gap 3 (wiring) — `quoting_gear_processes` catalogue.
//!
//! The engine half (S5) shipped [`aberp_quote_engine::GearProcessRate`] and
//! grew [`aberp_quote_engine::quote_with_shop_model`]'s 11th argument
//! (`gear_process_rates`), which costs a part's tooth-generation operations
//! at the routed family's effective EUR/min (a power-skived external gear
//! cut in-cycle on a turn-mill is far cheaper than a hobbed one; an internal
//! ring shaped/broached/wire-EDM'd is the premium end). This module is the
//! missing data layer: the operator-managed, process-keyed coefficient table
//! the pricing pipeline snapshots into the engine.
//!
//! ## Conventions mirrored from [`crate::quoting_machine_rates`] (S4)
//!
//! Prefixed-ULID id (`qgp_<ULID>`), lazy `CREATE TABLE IF NOT EXISTS`,
//! invariants enforced **in code** not via SQL CHECK/trigger
//! ([[no-sql-specific]]). The `process` column stores the stable db-string
//! ([`aberp_quote_engine::GearProcess::as_db_str`]); the closed-vocab check
//! round-trips through that `as_db_str` over the **five concrete** processes
//! (`Auto` is a per-part selection directive, never a persisted catalogue
//! key — see [`concrete_process_from_db_str`]). The engine exposes only
//! `as_db_str` for `GearProcess` (no `from_db_str`/`ALL`), so unlike the
//! machine-family vocab — which reads `MachineFamily::ALL` — the concrete set
//! is enumerated here; it is still a single source of truth (the engine enum)
//! round-tripped, not a hand-copied string list. There is exactly **one row
//! per process** per tenant — the process is the natural unique key (enforced
//! in code like `quoting_machine_rates`' one-rate-per-family guard).
//!
//! ## Seed values (FLAG — day-1, calibration-bound)
//!
//! The five concrete processes are seeded with the S5 engine-test
//! magnitudes (`tests/gear_ops.rs::gear_rates`) carried forward verbatim as
//! conservative day-1 values: external hob/skive cheap (skive cheaper, and
//! `in_cycle_factor 0.5` when power-skived in-cycle), internal shape/broach/
//! wire-EDM premium. They are **illustrative, not gospel** — ADR-0094 Q5
//! defers them to operator tuning / S429-style calibration. `module_exponent`
//! is seeded at `1.0` (module-linear) as the conservative day-1 choice and is
//! a prime calibration target. FLAGGED to S7/Ervin: confirm production seeds.
//!
//! ## Audit
//!
//! CRUD emits via the audit ledger. S6 **reuses** [`EventKind::ParametersChanged`]
//! (the quoting-tunables-changed kind) rather than introducing a dedicated
//! `GearProcessesChanged` variant — identical reasoning to S4's machine-rate
//! audit: `EventKind` is not `#[non_exhaustive]` and has ~186 variants matched
//! across crates the 45s/4 GB sandbox cannot compile-verify, so a new variant
//! is an unacceptable blast radius here (ADR-0094 toolchain-honesty clause).
//! The payload is self-describing (`"catalogue":"quoting_gear_processes"`) so
//! a future migration to a dedicated kind is a pure relabel. FLAGGED for CI
//! follow-up.

use anyhow::{Context, Result};
use duckdb::{params, Connection};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use ulid::Ulid;

use aberp_audit_ledger::{append_in_tx, Actor, EventKind, LedgerMeta};
use aberp_quote_engine::GearProcess;

// Reuse the tunables write-error + validation-error vocab so the serve
// layer's `tunable_write_response` maps gear-process failures identically.
use crate::quoting_tunables::{TunableWriteError, ValidationError};

/// The five concrete gear processes (engine [`GearProcess`] minus `Auto`)
/// that can carry a catalogue rate row. `Auto` is a per-part selection
/// directive resolved by the engine ([`aberp_quote_engine::select_gear_process`]),
/// never a persisted process — so it is deliberately excluded here.
const CONCRETE_PROCESSES: [GearProcess; 5] = [
    GearProcess::Hob,
    GearProcess::PowerSkive,
    GearProcess::Shape,
    GearProcess::Broach,
    GearProcess::WireEdm,
];

/// Closed-vocab check: round-trip a db-string through the engine's
/// [`GearProcess::as_db_str`] over the five concrete processes, rejecting
/// `"auto"` and any unknown token. The single source of truth is the engine
/// enum; this is the `from_db_str` the engine does not (yet) expose for
/// `GearProcess`, scoped to persistable processes.
fn concrete_process_from_db_str(s: &str) -> Option<GearProcess> {
    CONCRETE_PROCESSES
        .iter()
        .copied()
        .find(|p| p.as_db_str() == s)
}

/// Wire + storage shape of a `quoting_gear_processes` row.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct GearProcessRow {
    /// `qgp_<26-char-ULID>`.
    pub id: String,
    /// `GearProcess` db-string, e.g. `power_skive` (concrete only — never
    /// `auto`).
    pub process: String,
    /// Indexing / tool-load minutes charged once per gear.
    pub setup_min: f64,
    /// Base generation minutes per tooth (before module / face-width /
    /// quality scaling).
    pub min_per_tooth: f64,
    /// Generation time scales with `module_mm^module_exponent`.
    pub module_exponent: f64,
    /// Quality-factor growth per AGMA class above the engine datum:
    /// `quality_factor = 1 + max(0, agma - datum) * this`.
    pub agma_quality_factor_base: f64,
    /// Multiplier in (0, 1] applied when the process runs in-cycle on the
    /// routed turning family (power-skiving on a Swiss/turn-mill). `1.0` for
    /// a standalone op.
    pub in_cycle_factor: f64,
    pub notes: Option<String>,
    pub updated_at: String,
    pub updated_by_actor: String,
}

/// Request body for create/update.
#[derive(Deserialize, Debug, Clone)]
pub struct GearProcessInputs {
    #[serde(default)]
    pub process: String,
    #[serde(default)]
    pub setup_min: f64,
    #[serde(default)]
    pub min_per_tooth: f64,
    #[serde(default = "default_module_exponent")]
    pub module_exponent: f64,
    #[serde(default)]
    pub agma_quality_factor_base: f64,
    #[serde(default = "default_in_cycle_factor")]
    pub in_cycle_factor: f64,
    #[serde(default)]
    pub notes: Option<String>,
}

/// Neutral default for `module_exponent` (1.0 ⇒ module-linear), so a row
/// can be created with just the process + per-tooth time.
fn default_module_exponent() -> f64 {
    1.0
}

/// Neutral default for `in_cycle_factor` (1.0 ⇒ no in-cycle discount), so a
/// standalone process needs only its time coefficients.
fn default_in_cycle_factor() -> f64 {
    1.0
}

/// One seed row: process + its proposed day-1 coefficients (ADR-0094 Gap 3).
struct Seed {
    process: GearProcess,
    setup_min: f64,
    min_per_tooth: f64,
    module_exponent: f64,
    agma_quality_factor_base: f64,
    in_cycle_factor: f64,
}

/// The five concrete processes ADR-0094 Gap 3 seeds (NOT `auto`). Values are
/// the S5 engine-test magnitudes carried forward as conservative day-1 seeds
/// (FLAG: calibration-bound, ADR Q5). External hob/skive are cheap (skive
/// cheaper, with `in_cycle_factor 0.5`); internal shape/broach/wire-EDM are
/// premium. `in_cycle_factor < 1.0` only on `power_skive`; the rest are 1.0.
const SEEDS: &[Seed] = &[
    Seed {
        process: GearProcess::Hob,
        setup_min: 20.0,
        min_per_tooth: 0.30,
        module_exponent: 1.0,
        agma_quality_factor_base: 0.10,
        in_cycle_factor: 1.0,
    },
    Seed {
        process: GearProcess::PowerSkive,
        setup_min: 8.0,
        min_per_tooth: 0.10,
        module_exponent: 1.0,
        agma_quality_factor_base: 0.10,
        in_cycle_factor: 0.5,
    },
    Seed {
        process: GearProcess::Shape,
        setup_min: 30.0,
        min_per_tooth: 0.50,
        module_exponent: 1.0,
        agma_quality_factor_base: 0.15,
        in_cycle_factor: 1.0,
    },
    Seed {
        process: GearProcess::Broach,
        setup_min: 60.0,
        min_per_tooth: 0.05,
        module_exponent: 1.0,
        agma_quality_factor_base: 0.10,
        in_cycle_factor: 1.0,
    },
    Seed {
        process: GearProcess::WireEdm,
        setup_min: 15.0,
        min_per_tooth: 2.00,
        module_exponent: 1.0,
        agma_quality_factor_base: 0.20,
        in_cycle_factor: 1.0,
    },
];

/// Validate inputs in code (no SQL CHECK). Surfaces every error at once
/// (CLAUDE.md rule 9 / 12). The `process` closed-vocab check round-trips
/// through the engine's `GearProcess::as_db_str` over the concrete processes,
/// so the engine enum is the single source of truth.
pub fn validate_gear_process_inputs(
    inputs: &GearProcessInputs,
) -> Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();

    if concrete_process_from_db_str(inputs.process.trim()).is_none() {
        errors.push(ValidationError {
            field: "process",
            message: format!(
                "Ismeretlen fogazási eljárás: {:?}. (hob|power_skive|shape|broach|wire_edm) \
                 / Unknown gear process.",
                inputs.process
            ),
        });
    }

    // setup_min, min_per_tooth, agma_quality_factor_base: finite and >= 0
    // (zero is a legitimate "this coefficient doesn't contribute" value).
    for (field, val) in [
        ("setup_min", inputs.setup_min),
        ("min_per_tooth", inputs.min_per_tooth),
        ("agma_quality_factor_base", inputs.agma_quality_factor_base),
    ] {
        if !(val.is_finite() && val >= 0.0) {
            errors.push(ValidationError {
                field,
                message: "Az érték legyen véges és >= 0. / Value must be finite and >= 0."
                    .to_string(),
            });
        }
    }

    // module_exponent: finite and >= 0 (0 ⇒ module-independent time).
    if !(inputs.module_exponent.is_finite() && inputs.module_exponent >= 0.0) {
        errors.push(ValidationError {
            field: "module_exponent",
            message:
                "A modul-kitevő legyen véges és >= 0. / Module exponent must be finite and >= 0."
                    .to_string(),
        });
    }

    // in_cycle_factor: finite and in (0, 1] (mirrors lights_out_factor — a
    // factor > 1 would make in-cycle dearer, which is nonsensical; 0 zeroes).
    if !(inputs.in_cycle_factor.is_finite()
        && inputs.in_cycle_factor > 0.0
        && inputs.in_cycle_factor <= 1.0)
    {
        errors.push(ValidationError {
            field: "in_cycle_factor",
            message:
                "A ciklus-szorzó (0, 1] tartományban legyen. / In-cycle factor must be in (0, 1]."
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
CREATE TABLE IF NOT EXISTS quoting_gear_processes (
    id                        VARCHAR NOT NULL PRIMARY KEY,
    tenant_id                 VARCHAR NOT NULL,
    process                   VARCHAR NOT NULL,
    setup_min                 DOUBLE  NOT NULL,
    min_per_tooth             DOUBLE  NOT NULL,
    module_exponent           DOUBLE  NOT NULL,
    agma_quality_factor_base  DOUBLE  NOT NULL,
    in_cycle_factor           DOUBLE  NOT NULL,
    notes                     VARCHAR,
    updated_at                VARCHAR NOT NULL,
    updated_by_actor          VARCHAR NOT NULL
);
";

const COLS: &str = "id, process, setup_min, min_per_tooth, module_exponent, \
                    agma_quality_factor_base, in_cycle_factor, notes, updated_at, \
                    updated_by_actor";

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
        .context("ensure quoting_gear_processes schema")
}

/// Seed the five ADR-0094 Gap 3 processes, **insert-if-absent** per process —
/// so a re-run (or a partially-seeded table) never duplicates and never
/// clobbers an operator-tuned value. Idempotent: gated per `(tenant, process)`.
pub fn seed_gear_processes_if_absent(conn: &Connection, tenant: &str) -> Result<()> {
    ensure_schema(conn)?;
    let now = now_rfc3339()?;
    for seed in SEEDS {
        let process = seed.process.as_db_str();
        let existing: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM quoting_gear_processes WHERE tenant_id = ? AND process = ?;",
                params![tenant, process],
                |r| r.get(0),
            )
            .context("count quoting_gear_processes for seed gate")?;
        if existing > 0 {
            continue;
        }
        let id = format!("qgp_{}", Ulid::new());
        conn.execute(
            "INSERT INTO quoting_gear_processes (id, tenant_id, process, setup_min, \
             min_per_tooth, module_exponent, agma_quality_factor_base, in_cycle_factor, \
             notes, updated_at, updated_by_actor) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, NULL, ?, 'boot');",
            params![
                &id,
                tenant,
                process,
                seed.setup_min,
                seed.min_per_tooth,
                seed.module_exponent,
                seed.agma_quality_factor_base,
                seed.in_cycle_factor,
                &now,
            ],
        )
        .with_context(|| format!("seed quoting_gear_processes row for {process}"))?;
    }
    Ok(())
}

fn now_rfc3339() -> Result<String> {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .context("format quoting_gear_processes timestamp")
}

fn row_to_gear_process(row: &duckdb::Row<'_>) -> duckdb::Result<GearProcessRow> {
    Ok(GearProcessRow {
        id: row.get(0)?,
        process: row.get(1)?,
        setup_min: row.get(2)?,
        min_per_tooth: row.get(3)?,
        module_exponent: row.get(4)?,
        agma_quality_factor_base: row.get(5)?,
        in_cycle_factor: row.get(6)?,
        notes: row.get(7)?,
        updated_at: row.get(8)?,
        updated_by_actor: row.get(9)?,
    })
}

/// All process rows for a tenant, process-ordered (stable list for the SPA).
pub fn list_gear_processes(conn: &Connection, tenant: &str) -> Result<Vec<GearProcessRow>> {
    ensure_schema(conn)?;
    let sql = format!(
        "SELECT {COLS} FROM quoting_gear_processes WHERE tenant_id = ? ORDER BY process ASC;"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![tenant], row_to_gear_process)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

fn get_gear_process(conn: &Connection, tenant: &str, id: &str) -> Result<Option<GearProcessRow>> {
    let sql = format!("SELECT {COLS} FROM quoting_gear_processes WHERE tenant_id = ? AND id = ?;");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query_map(params![tenant, id], row_to_gear_process)?;
    match rows.next() {
        Some(r) => Ok(Some(r?)),
        None => Ok(None),
    }
}

/// Count rows holding `process` other than `except_id` — the in-code
/// one-row-per-process uniqueness guard (no SQL UNIQUE, [[no-sql-specific]]).
fn process_taken_by_other(
    conn: &Connection,
    tenant: &str,
    process: &str,
    except_id: &str,
) -> Result<bool> {
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM quoting_gear_processes \
             WHERE tenant_id = ? AND process = ? AND id != ?;",
            params![tenant, process, except_id],
            |r| r.get(0),
        )
        .context("check quoting_gear_processes process uniqueness")?;
    Ok(n > 0)
}

/// Create a rate for a process (one per process). `Conflict` if the process
/// already has a row — the operator edits the existing one instead.
pub fn create_gear_process(
    conn: &mut Connection,
    meta: &LedgerMeta,
    actor_login: &str,
    tenant: &str,
    inputs: &GearProcessInputs,
) -> Result<GearProcessRow, TunableWriteError> {
    if let Err(e) = validate_gear_process_inputs(inputs) {
        return Err(TunableWriteError::Validation(e));
    }
    ensure_schema(conn).map_err(TunableWriteError::Other)?;
    let process = concrete_process_from_db_str(inputs.process.trim())
        .context("process validated before create")
        .map_err(TunableWriteError::Other)?
        .as_db_str();
    if process_taken_by_other(conn, tenant, process, "").map_err(TunableWriteError::Other)? {
        return Err(TunableWriteError::Conflict(format!(
            "a rate for process `{process}` already exists — edit it instead"
        )));
    }
    let now = now_rfc3339().map_err(TunableWriteError::Other)?;
    let notes = normalize_optional(inputs.notes.as_deref());
    let id = format!("qgp_{}", Ulid::new());
    let tx = conn
        .transaction()
        .context("begin create_gear_process tx")
        .map_err(TunableWriteError::Other)?;
    tx.execute(
        "INSERT INTO quoting_gear_processes (id, tenant_id, process, setup_min, \
         min_per_tooth, module_exponent, agma_quality_factor_base, in_cycle_factor, \
         notes, updated_at, updated_by_actor) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?);",
        params![
            &id,
            tenant,
            process,
            inputs.setup_min,
            inputs.min_per_tooth,
            inputs.module_exponent,
            inputs.agma_quality_factor_base,
            inputs.in_cycle_factor,
            notes.as_deref(),
            &now,
            actor_login,
        ],
    )
    .context("INSERT quoting_gear_processes")
    .map_err(TunableWriteError::Other)?;
    let row = read_in_tx(&tx, tenant, &id).map_err(TunableWriteError::Other)?;
    append_gear_process_change(&tx, meta, actor_login, "gear_process_create", &row)
        .map_err(TunableWriteError::Other)?;
    tx.commit()
        .context("commit create_gear_process")
        .map_err(TunableWriteError::Other)?;
    Ok(row)
}

/// Update a rate by id. `NotFound` if the row is absent; `Conflict` if the
/// edited `process` collides with another row.
pub fn update_gear_process(
    conn: &mut Connection,
    meta: &LedgerMeta,
    actor_login: &str,
    tenant: &str,
    id: &str,
    inputs: &GearProcessInputs,
) -> Result<GearProcessRow, TunableWriteError> {
    if let Err(e) = validate_gear_process_inputs(inputs) {
        return Err(TunableWriteError::Validation(e));
    }
    ensure_schema(conn).map_err(TunableWriteError::Other)?;
    let process = concrete_process_from_db_str(inputs.process.trim())
        .context("process validated before update")
        .map_err(TunableWriteError::Other)?
        .as_db_str();
    if get_gear_process(conn, tenant, id)
        .map_err(TunableWriteError::Other)?
        .is_none()
    {
        return Err(TunableWriteError::NotFound(format!(
            "quoting_gear_processes row {id} not found"
        )));
    }
    if process_taken_by_other(conn, tenant, process, id).map_err(TunableWriteError::Other)? {
        return Err(TunableWriteError::Conflict(format!(
            "another rate for process `{process}` already exists"
        )));
    }
    let now = now_rfc3339().map_err(TunableWriteError::Other)?;
    let notes = normalize_optional(inputs.notes.as_deref());
    let tx = conn
        .transaction()
        .context("begin update_gear_process tx")
        .map_err(TunableWriteError::Other)?;
    tx.execute(
        "UPDATE quoting_gear_processes SET process = ?, setup_min = ?, min_per_tooth = ?, \
         module_exponent = ?, agma_quality_factor_base = ?, in_cycle_factor = ?, notes = ?, \
         updated_at = ?, updated_by_actor = ? WHERE tenant_id = ? AND id = ?;",
        params![
            process,
            inputs.setup_min,
            inputs.min_per_tooth,
            inputs.module_exponent,
            inputs.agma_quality_factor_base,
            inputs.in_cycle_factor,
            notes.as_deref(),
            &now,
            actor_login,
            tenant,
            id,
        ],
    )
    .context("UPDATE quoting_gear_processes")
    .map_err(TunableWriteError::Other)?;
    let row = read_in_tx(&tx, tenant, id).map_err(TunableWriteError::Other)?;
    append_gear_process_change(&tx, meta, actor_login, "gear_process_update", &row)
        .map_err(TunableWriteError::Other)?;
    tx.commit()
        .context("commit update_gear_process")
        .map_err(TunableWriteError::Other)?;
    Ok(row)
}

/// Hard-delete a rate by id (the process then contributes 0.0 + a loud
/// reasoning line in the engine if a gear still resolves to it — fail-soft
/// per the S5 handoff). `NotFound` if absent.
pub fn delete_gear_process(
    conn: &mut Connection,
    meta: &LedgerMeta,
    actor_login: &str,
    tenant: &str,
    id: &str,
) -> Result<(), TunableWriteError> {
    ensure_schema(conn).map_err(TunableWriteError::Other)?;
    let Some(row) = get_gear_process(conn, tenant, id).map_err(TunableWriteError::Other)? else {
        return Err(TunableWriteError::NotFound(format!(
            "quoting_gear_processes row {id} not found"
        )));
    };
    let tx = conn
        .transaction()
        .context("begin delete_gear_process tx")
        .map_err(TunableWriteError::Other)?;
    tx.execute(
        "DELETE FROM quoting_gear_processes WHERE tenant_id = ? AND id = ?;",
        params![tenant, id],
    )
    .context("DELETE quoting_gear_processes")
    .map_err(TunableWriteError::Other)?;
    append_gear_process_change(&tx, meta, actor_login, "gear_process_delete", &row)
        .map_err(TunableWriteError::Other)?;
    tx.commit()
        .context("commit delete_gear_process")
        .map_err(TunableWriteError::Other)?;
    Ok(())
}

// ── Internals ───────────────────────────────────────────────────────────

fn normalize_optional(s: Option<&str>) -> Option<String> {
    s.map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

fn read_in_tx(tx: &duckdb::Transaction<'_>, tenant: &str, id: &str) -> Result<GearProcessRow> {
    let sql = format!("SELECT {COLS} FROM quoting_gear_processes WHERE tenant_id = ? AND id = ?;");
    let mut stmt = tx.prepare(&sql)?;
    let mut rows = stmt.query_map(params![tenant, id], row_to_gear_process)?;
    match rows.next() {
        Some(r) => Ok(r?),
        None => Err(anyhow::anyhow!(
            "quoting_gear_processes row {id} vanished mid-tx"
        )),
    }
}

/// Append a gear-process-change audit entry inside the write tx. Reuses
/// [`EventKind::ParametersChanged`] (see module docs / FLAG) with a
/// self-describing payload so a future dedicated kind is a pure relabel.
fn append_gear_process_change(
    tx: &duckdb::Transaction<'_>,
    meta: &LedgerMeta,
    actor_login: &str,
    op: &str,
    row: &GearProcessRow,
) -> Result<()> {
    let payload = serde_json::json!({
        "catalogue": "quoting_gear_processes",
        "op": op,
        "snapshot": { "row": row },
        "idempotency_key": Ulid::new().to_string(),
    });
    let bytes =
        serde_json::to_vec(&payload).context("serialize gear-process change audit payload")?;
    let actor = Actor::from_local_cli(Ulid::new().to_string(), actor_login);
    append_in_tx(tx, meta, EventKind::ParametersChanged, bytes, actor, None)
        .context("audit append gear-process change")?;
    Ok(())
}
