//! S439 (ADR-0090) — defense-grade quality management: NCR + CAPA.
//!
//! The aerospace-pivot strand that adds the AS9100 §10.2 / IATF 16949 §10.2
//! Non-Conformance-Report (NCR) + Corrective-And-Preventive-Action (CAPA)
//! workflow. Builds on S438 per-unit part-UID marking + S428 `customer_type` +
//! ADR-0064 dispatch. Closes the quality loop: every marked part has a
//! traceable history of quality events.
//!
//! ## What this is
//!
//! When a part/process fails inspection, the operator opens an **NCR** (what
//! failed, severity, category, affected part UIDs, photos). The fix is tracked
//! as a separate linked **CAPA** record (containment + corrective + preventive,
//! approval, effectiveness review). An NCR closes only when its CAPA is approved
//! AND verified-effective.
//!
//! ## Trust the code, not the operator ([[trust-code-not-operator]])
//!
//! Three invariants live in code, never in operator discipline:
//!   1. **State transitions** — only the allowed edges of the lifecycle graph
//!      ([`allowed_transition`]); closing requires a verified CAPA.
//!   2. **Escalation timer** — a `Critical` NCR not closed within
//!      [`CRITICAL_ESCALATION_HOURS`] auto-escalates on the boot scan.
//!   3. **Refuse-Shipment gate** — a defense/aerospace WO with any unit whose
//!      `part_uid` is referenced by an `Open`/`Contained` NCR cannot ship
//!      (extends the S438 part-UID gate; the resolver lives in `serve.rs`).
//!
//! ## Sparse by design (CLAUDE.md rule 12)
//!
//! No CHECK / no DEFAULT / no surrogate ids ([[no-sql-specific]] + the DuckDB
//! replay-clobber trap). A non-defense tenant simply has zero rows here.

use anyhow::{Context, Result};
use duckdb::{params, Connection};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use ulid::Ulid;

use aberp_audit_ledger::{Actor, BinaryHash, EventKind, Ledger, TenantId};

// ── Closed-vocab enums ──────────────────────────────────────────────

/// Non-conformance severity tier. Drives the escalation timer ([`Critical`]
/// only) and the operator's triage priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NcrSeverity {
    Critical,
    Major,
    Minor,
}

impl NcrSeverity {
    pub fn as_db_str(&self) -> &'static str {
        match self {
            NcrSeverity::Critical => "critical",
            NcrSeverity::Major => "major",
            NcrSeverity::Minor => "minor",
        }
    }
    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "critical" => Some(NcrSeverity::Critical),
            "major" => Some(NcrSeverity::Major),
            "minor" => Some(NcrSeverity::Minor),
            _ => None,
        }
    }
}

/// What kind of non-conformance this is. Closed vocabulary — `Other` is the
/// explicit escape hatch, never a silent fallback (CLAUDE.md rule 12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NcrCategory {
    Material,
    Workmanship,
    Documentation,
    EquipmentFailure,
    OperatorError,
    SupplierIssue,
    Other,
}

impl NcrCategory {
    pub fn as_db_str(&self) -> &'static str {
        match self {
            NcrCategory::Material => "material",
            NcrCategory::Workmanship => "workmanship",
            NcrCategory::Documentation => "documentation",
            NcrCategory::EquipmentFailure => "equipment_failure",
            NcrCategory::OperatorError => "operator_error",
            NcrCategory::SupplierIssue => "supplier_issue",
            NcrCategory::Other => "other",
        }
    }
    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "material" => Some(NcrCategory::Material),
            "workmanship" => Some(NcrCategory::Workmanship),
            "documentation" => Some(NcrCategory::Documentation),
            "equipment_failure" => Some(NcrCategory::EquipmentFailure),
            "operator_error" => Some(NcrCategory::OperatorError),
            "supplier_issue" => Some(NcrCategory::SupplierIssue),
            "other" => Some(NcrCategory::Other),
            _ => None,
        }
    }
}

/// NCR lifecycle state. The happy path is
/// `Open → Contained → UnderInvestigation → CorrectionApplied → Closed`;
/// `Escalated` is the timer-driven branch reachable from any non-terminal state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NcrState {
    Open,
    Contained,
    UnderInvestigation,
    CorrectionApplied,
    Closed,
    Escalated,
}

impl NcrState {
    pub fn as_db_str(&self) -> &'static str {
        match self {
            NcrState::Open => "open",
            NcrState::Contained => "contained",
            NcrState::UnderInvestigation => "under_investigation",
            NcrState::CorrectionApplied => "correction_applied",
            NcrState::Closed => "closed",
            NcrState::Escalated => "escalated",
        }
    }
    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "open" => Some(NcrState::Open),
            "contained" => Some(NcrState::Contained),
            "under_investigation" => Some(NcrState::UnderInvestigation),
            "correction_applied" => Some(NcrState::CorrectionApplied),
            "closed" => Some(NcrState::Closed),
            "escalated" => Some(NcrState::Escalated),
            _ => None,
        }
    }
    /// Terminal — no further transitions allowed.
    pub fn is_terminal(&self) -> bool {
        matches!(self, NcrState::Closed)
    }
    /// "Open" in the shipment-gate sense: the part still has an unresolved
    /// quality issue. `Open` + `Contained` block shipment (brief §4).
    pub fn blocks_shipment(&self) -> bool {
        matches!(self, NcrState::Open | NcrState::Contained)
    }
}

/// Whether a CAPA's corrective action was effective. `Pending` until reviewed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapaVerdict {
    Verified,
    NotEffective,
    Pending,
}

impl CapaVerdict {
    pub fn as_db_str(&self) -> &'static str {
        match self {
            CapaVerdict::Verified => "verified",
            CapaVerdict::NotEffective => "not_effective",
            CapaVerdict::Pending => "pending",
        }
    }
    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "verified" => Some(CapaVerdict::Verified),
            "not_effective" => Some(CapaVerdict::NotEffective),
            "pending" => Some(CapaVerdict::Pending),
            _ => None,
        }
    }
}

// ── Pure: transition rules + escalation timer ───────────────────────

/// A `Critical` NCR not closed within this many hours auto-escalates.
pub const CRITICAL_ESCALATION_HOURS: i64 = 24;

/// The allowed NCR state-transition edges ([[trust-code-not-operator]]). Pure —
/// the lifecycle graph is unit-testable without a DB. `Closed` is terminal.
pub fn allowed_transition(from: NcrState, to: NcrState) -> bool {
    use NcrState::*;
    matches!(
        (from, to),
        (Open, Contained)
            | (Open, UnderInvestigation)
            | (Open, Escalated)
            | (Contained, UnderInvestigation)
            | (Contained, Escalated)
            | (UnderInvestigation, CorrectionApplied)
            | (UnderInvestigation, Escalated)
            | (CorrectionApplied, Closed)
            | (CorrectionApplied, Escalated)
            | (Escalated, UnderInvestigation)
            | (Escalated, CorrectionApplied)
            | (Escalated, Closed)
    )
}

/// Whether a `Critical` NCR has breached its escalation window. Pure — the timer
/// is code, not operator memory. A non-`Critical` or already-closed/escalated
/// NCR never auto-escalates.
pub fn escalation_overdue(
    severity: NcrSeverity,
    state: NcrState,
    discovered_at: OffsetDateTime,
    now: OffsetDateTime,
) -> bool {
    if severity != NcrSeverity::Critical {
        return false;
    }
    if matches!(state, NcrState::Closed | NcrState::Escalated) {
        return false;
    }
    (now - discovered_at).whole_hours() >= CRITICAL_ESCALATION_HOURS
}

/// Mint `ncr_<26-char-ULID>`.
pub fn generate_ncr_id() -> String {
    format!("ncr_{}", Ulid::new())
}

/// Mint `capa_<26-char-ULID>`.
pub fn generate_capa_id() -> String {
    format!("capa_{}", Ulid::new())
}

/// Trim + validate an operator-typed NCR description. Loud-rejects blank
/// (CLAUDE.md rule 12) — an NCR with no description is not an NCR.
pub fn validate_description(s: &str) -> std::result::Result<(), &'static str> {
    let t = s.trim();
    if t.is_empty() {
        return Err("description must not be blank");
    }
    if t.len() > 4000 {
        return Err("description must be at most 4000 characters");
    }
    Ok(())
}

// ── Schema ──────────────────────────────────────────────────────────

/// Additive quality tables. NO surrogate id (natural prefixed-ULID PK), NO
/// CHECK / NO DEFAULT ([[no-sql-specific]] + DuckDB replay-clobber trap), NO
/// index (S341/S410 — scan/filter in Rust). Array columns are JSON text.
const QUALITY_SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS ncrs (
    ncr_id                  VARCHAR NOT NULL,
    tenant_id               VARCHAR NOT NULL,
    discovered_at_utc       VARCHAR NOT NULL,
    discovered_by_operator  VARCHAR NOT NULL,
    severity                VARCHAR NOT NULL,
    category                VARCHAR NOT NULL,
    description             VARCHAR NOT NULL,
    affected_part_uids      VARCHAR NOT NULL,
    affected_wo_ids         VARCHAR NOT NULL,
    affected_heat_lots      VARCHAR NOT NULL,
    photos                  VARCHAR NOT NULL,
    state                   VARCHAR NOT NULL,
    closed_at_utc           VARCHAR,
    closed_by_operator      VARCHAR
);
CREATE TABLE IF NOT EXISTS ncr_transitions (
    tenant_id     VARCHAR NOT NULL,
    ncr_id        VARCHAR NOT NULL,
    seq           INTEGER NOT NULL,
    from_state    VARCHAR NOT NULL,
    to_state      VARCHAR NOT NULL,
    operator      VARCHAR NOT NULL,
    at_utc        VARCHAR NOT NULL,
    note          VARCHAR NOT NULL
);
CREATE TABLE IF NOT EXISTS capas (
    capa_id                    VARCHAR NOT NULL,
    ncr_id                     VARCHAR NOT NULL,
    tenant_id                  VARCHAR NOT NULL,
    corrective_action_text     VARCHAR NOT NULL,
    preventive_action_text     VARCHAR NOT NULL,
    responsible_operator       VARCHAR NOT NULL,
    target_close_date          VARCHAR NOT NULL,
    actual_close_date          VARCHAR,
    effectiveness_review_at_utc VARCHAR,
    effectiveness_verdict      VARCHAR NOT NULL,
    effectiveness_comment      VARCHAR,
    approved_by_operator       VARCHAR,
    approved_at_utc            VARCHAR,
    created_at_utc             VARCHAR NOT NULL,
    created_by_operator        VARCHAR NOT NULL
);
";

/// Idempotent `CREATE TABLE IF NOT EXISTS` for all three quality tables.
pub fn ensure_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(QUALITY_SCHEMA_SQL)
        .context("ensure quality (ncr/capa) schema")
}

// ── Row shapes ──────────────────────────────────────────────────────

/// One NCR record. Array fields are decoded from their JSON columns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ncr {
    pub ncr_id: String,
    pub discovered_at_utc: String,
    pub discovered_by_operator: String,
    pub severity: NcrSeverity,
    pub category: NcrCategory,
    pub description: String,
    pub affected_part_uids: Vec<String>,
    pub affected_wo_ids: Vec<String>,
    pub affected_heat_lots: Vec<String>,
    pub photos: Vec<String>,
    pub state: NcrState,
    pub closed_at_utc: Option<String>,
    pub closed_by_operator: Option<String>,
}

/// One append-only state-transition log row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NcrTransition {
    pub seq: u32,
    pub from_state: String,
    pub to_state: String,
    pub operator: String,
    pub at_utc: String,
    pub note: String,
}

/// One CAPA record linked to a parent NCR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capa {
    pub capa_id: String,
    pub ncr_id: String,
    pub corrective_action_text: String,
    pub preventive_action_text: String,
    pub responsible_operator: String,
    pub target_close_date: String,
    pub actual_close_date: Option<String>,
    pub effectiveness_review_at_utc: Option<String>,
    pub effectiveness_verdict: CapaVerdict,
    pub effectiveness_comment: Option<String>,
    pub approved_by_operator: Option<String>,
    pub approved_at_utc: Option<String>,
    pub created_at_utc: String,
    pub created_by_operator: String,
}

impl Capa {
    /// A CAPA gates an NCR close only when it is approved AND verified-effective.
    pub fn permits_ncr_close(&self) -> bool {
        self.approved_at_utc.is_some() && self.effectiveness_verdict == CapaVerdict::Verified
    }
}

/// NCR + its transition log + its linked CAPAs (the detail-page payload).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NcrDetail {
    #[serde(flatten)]
    pub ncr: Ncr,
    pub transitions: Vec<NcrTransition>,
    pub capas: Vec<Capa>,
}

fn json_array(s: &str) -> Vec<String> {
    serde_json::from_str(s).unwrap_or_default()
}

fn encode_array(v: &[String]) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "[]".to_string())
}

fn row_to_ncr(r: &duckdb::Row) -> duckdb::Result<Ncr> {
    Ok(Ncr {
        ncr_id: r.get::<_, String>(0)?,
        discovered_at_utc: r.get::<_, String>(1)?,
        discovered_by_operator: r.get::<_, String>(2)?,
        severity: NcrSeverity::from_db_str(&r.get::<_, String>(3)?).unwrap_or(NcrSeverity::Minor),
        category: NcrCategory::from_db_str(&r.get::<_, String>(4)?).unwrap_or(NcrCategory::Other),
        description: r.get::<_, String>(5)?,
        affected_part_uids: json_array(&r.get::<_, String>(6)?),
        affected_wo_ids: json_array(&r.get::<_, String>(7)?),
        affected_heat_lots: json_array(&r.get::<_, String>(8)?),
        photos: json_array(&r.get::<_, String>(9)?),
        state: NcrState::from_db_str(&r.get::<_, String>(10)?).unwrap_or(NcrState::Open),
        closed_at_utc: r.get::<_, Option<String>>(11)?,
        closed_by_operator: r.get::<_, Option<String>>(12)?,
    })
}

const NCR_COLS: &str = "ncr_id, discovered_at_utc, discovered_by_operator, severity, category, \
     description, affected_part_uids, affected_wo_ids, affected_heat_lots, photos, state, \
     closed_at_utc, closed_by_operator";

// ── NCR reads ───────────────────────────────────────────────────────

/// Filter spec for [`list_ncrs`]. Empty fields match all. Resolved in Rust —
/// no index, no SQL-specific WHERE building ([[no-sql-specific]]).
#[derive(Debug, Clone, Default)]
pub struct NcrFilter {
    pub state: Option<NcrState>,
    pub severity: Option<NcrSeverity>,
    /// Inclusive ISO date/datetime lower bound on `discovered_at_utc`.
    pub from: Option<String>,
    /// Inclusive ISO date/datetime upper bound on `discovered_at_utc`.
    pub to: Option<String>,
    /// Match NCRs whose `affected_part_uids` contains this UID.
    pub part_uid: Option<String>,
}

/// List NCRs (newest first), filtered in Rust over a full scan.
pub fn list_ncrs(conn: &Connection, tenant: &str, filter: &NcrFilter) -> Result<Vec<Ncr>> {
    ensure_schema(conn)?;
    let sql = format!(
        "SELECT {NCR_COLS} FROM ncrs WHERE tenant_id = ?1 ORDER BY discovered_at_utc DESC, ncr_id DESC"
    );
    let mut stmt = conn.prepare(&sql).context("prepare list_ncrs")?;
    let rows = stmt
        .query_map(params![tenant], row_to_ncr)
        .context("query list_ncrs")?;
    let mut out = Vec::new();
    for r in rows {
        let ncr = r.context("read ncr row")?;
        if let Some(s) = filter.state {
            if ncr.state != s {
                continue;
            }
        }
        if let Some(sv) = filter.severity {
            if ncr.severity != sv {
                continue;
            }
        }
        if let Some(from) = filter
            .from
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if ncr.discovered_at_utc.as_str() < from {
                continue;
            }
        }
        if let Some(to) = filter
            .to
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            // Inclusive upper bound: keep rows discovered on/before `to`. A
            // date-only `to` (e.g. "2026-06-16") matches its whole day via prefix.
            let keep =
                ncr.discovered_at_utc.as_str() <= to || ncr.discovered_at_utc.starts_with(to);
            if !keep {
                continue;
            }
        }
        if let Some(uid) = filter
            .part_uid
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if !ncr.affected_part_uids.iter().any(|u| u == uid) {
                continue;
            }
        }
        out.push(ncr);
    }
    Ok(out)
}

/// Read one NCR by id (no transitions/CAPAs).
pub fn get_ncr(conn: &Connection, tenant: &str, ncr_id: &str) -> Result<Option<Ncr>> {
    ensure_schema(conn)?;
    let sql = format!("SELECT {NCR_COLS} FROM ncrs WHERE tenant_id = ?1 AND ncr_id = ?2 LIMIT 1");
    let mut stmt = conn.prepare(&sql).context("prepare get_ncr")?;
    let mut rows = stmt
        .query(params![tenant, ncr_id])
        .context("query get_ncr")?;
    match rows.next().context("read get_ncr row")? {
        Some(r) => Ok(Some(row_to_ncr(r)?)),
        None => Ok(None),
    }
}

/// Read the append-only transition log for an NCR, oldest first.
pub fn list_transitions(
    conn: &Connection,
    tenant: &str,
    ncr_id: &str,
) -> Result<Vec<NcrTransition>> {
    ensure_schema(conn)?;
    let mut stmt = conn
        .prepare(
            "SELECT seq, from_state, to_state, operator, at_utc, note
               FROM ncr_transitions WHERE tenant_id = ?1 AND ncr_id = ?2 ORDER BY seq",
        )
        .context("prepare list_transitions")?;
    let rows = stmt
        .query_map(params![tenant, ncr_id], |r| {
            Ok(NcrTransition {
                seq: r.get::<_, i64>(0)?.max(0) as u32,
                from_state: r.get::<_, String>(1)?,
                to_state: r.get::<_, String>(2)?,
                operator: r.get::<_, String>(3)?,
                at_utc: r.get::<_, String>(4)?,
                note: r.get::<_, String>(5)?,
            })
        })
        .context("query list_transitions")?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.context("read transition row")?);
    }
    Ok(out)
}

const CAPA_COLS: &str = "capa_id, ncr_id, corrective_action_text, preventive_action_text, \
     responsible_operator, target_close_date, actual_close_date, effectiveness_review_at_utc, \
     effectiveness_verdict, effectiveness_comment, approved_by_operator, approved_at_utc, \
     created_at_utc, created_by_operator";

fn row_to_capa(r: &duckdb::Row) -> duckdb::Result<Capa> {
    Ok(Capa {
        capa_id: r.get::<_, String>(0)?,
        ncr_id: r.get::<_, String>(1)?,
        corrective_action_text: r.get::<_, String>(2)?,
        preventive_action_text: r.get::<_, String>(3)?,
        responsible_operator: r.get::<_, String>(4)?,
        target_close_date: r.get::<_, String>(5)?,
        actual_close_date: r.get::<_, Option<String>>(6)?,
        effectiveness_review_at_utc: r.get::<_, Option<String>>(7)?,
        effectiveness_verdict: CapaVerdict::from_db_str(&r.get::<_, String>(8)?)
            .unwrap_or(CapaVerdict::Pending),
        effectiveness_comment: r.get::<_, Option<String>>(9)?,
        approved_by_operator: r.get::<_, Option<String>>(10)?,
        approved_at_utc: r.get::<_, Option<String>>(11)?,
        created_at_utc: r.get::<_, String>(12)?,
        created_by_operator: r.get::<_, String>(13)?,
    })
}

/// List the CAPAs linked to an NCR, oldest first.
pub fn list_capas(conn: &Connection, tenant: &str, ncr_id: &str) -> Result<Vec<Capa>> {
    ensure_schema(conn)?;
    let sql = format!(
        "SELECT {CAPA_COLS} FROM capas WHERE tenant_id = ?1 AND ncr_id = ?2 ORDER BY created_at_utc, capa_id"
    );
    let mut stmt = conn.prepare(&sql).context("prepare list_capas")?;
    let rows = stmt
        .query_map(params![tenant, ncr_id], row_to_capa)
        .context("query list_capas")?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.context("read capa row")?);
    }
    Ok(out)
}

/// Read one CAPA by id.
pub fn get_capa(conn: &Connection, tenant: &str, capa_id: &str) -> Result<Option<Capa>> {
    ensure_schema(conn)?;
    let sql =
        format!("SELECT {CAPA_COLS} FROM capas WHERE tenant_id = ?1 AND capa_id = ?2 LIMIT 1");
    let mut stmt = conn.prepare(&sql).context("prepare get_capa")?;
    let mut rows = stmt
        .query(params![tenant, capa_id])
        .context("query get_capa")?;
    match rows.next().context("read get_capa row")? {
        Some(r) => Ok(Some(row_to_capa(r)?)),
        None => Ok(None),
    }
}

/// Full detail payload: NCR + transitions + CAPAs.
pub fn get_ncr_detail(conn: &Connection, tenant: &str, ncr_id: &str) -> Result<Option<NcrDetail>> {
    let Some(ncr) = get_ncr(conn, tenant, ncr_id)? else {
        return Ok(None);
    };
    let transitions = list_transitions(conn, tenant, ncr_id)?;
    let capas = list_capas(conn, tenant, ncr_id)?;
    Ok(Some(NcrDetail {
        ncr,
        transitions,
        capas,
    }))
}

// ── Errors ──────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum QualityError {
    #[error("NCR {0} not found")]
    NcrNotFound(String),
    #[error("CAPA {0} not found")]
    CapaNotFound(String),
    #[error("invalid input: {0}")]
    Invalid(String),
    /// A transition not permitted by [`allowed_transition`], or a close without
    /// a verified CAPA. Maps to HTTP 409.
    #[error("{0}")]
    IllegalTransition(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

// ── NCR writes (own Ledger after the conn drops — S432/S438 pattern) ─

/// Operator-supplied NCR creation input.
#[derive(Debug, Clone)]
pub struct NewNcr {
    pub severity: NcrSeverity,
    pub category: NcrCategory,
    pub description: String,
    pub affected_part_uids: Vec<String>,
    pub affected_wo_ids: Vec<String>,
    pub affected_heat_lots: Vec<String>,
    /// Operator-uploaded photos (base64). Saved under
    /// `~/.aberp/<tenant>/ncr-photos/<ncr_id>/` once the id is minted.
    pub photos: Vec<PhotoUpload>,
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default()
}

/// Create an NCR (state `Open`), seed its transition log (`"" → open`), and fire
/// `ncr.created`. Returns the persisted NCR.
pub fn create_ncr(
    db_path: &std::path::Path,
    tenant: TenantId,
    binary_hash: BinaryHash,
    operator: &str,
    input: NewNcr,
) -> std::result::Result<Ncr, QualityError> {
    validate_description(&input.description).map_err(|e| QualityError::Invalid(e.to_string()))?;
    let ncr_id = generate_ncr_id();
    let now = now_rfc3339();
    // Decode + persist photos to disk first, storing only their file names.
    let photo_names = save_photos(tenant.as_str(), &ncr_id, &input.photos)?;
    let ncr = Ncr {
        ncr_id: ncr_id.clone(),
        discovered_at_utc: now.clone(),
        discovered_by_operator: operator.to_string(),
        severity: input.severity,
        category: input.category,
        description: input.description.trim().to_string(),
        affected_part_uids: input.affected_part_uids,
        affected_wo_ids: input.affected_wo_ids,
        affected_heat_lots: input.affected_heat_lots,
        photos: photo_names,
        state: NcrState::Open,
        closed_at_utc: None,
        closed_by_operator: None,
    };
    {
        let conn = Connection::open(db_path)
            .map_err(|e| QualityError::Other(anyhow::anyhow!("open DuckDB for NCR create: {e}")))?;
        ensure_schema(&conn)?;
        conn.execute(
            "INSERT INTO ncrs (ncr_id, tenant_id, discovered_at_utc, discovered_by_operator, \
             severity, category, description, affected_part_uids, affected_wo_ids, \
             affected_heat_lots, photos, state, closed_at_utc, closed_by_operator) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,NULL,NULL)",
            params![
                ncr.ncr_id,
                tenant.as_str(),
                ncr.discovered_at_utc,
                ncr.discovered_by_operator,
                ncr.severity.as_db_str(),
                ncr.category.as_db_str(),
                ncr.description,
                encode_array(&ncr.affected_part_uids),
                encode_array(&ncr.affected_wo_ids),
                encode_array(&ncr.affected_heat_lots),
                encode_array(&ncr.photos),
                ncr.state.as_db_str(),
            ],
        )
        .context("insert ncr row")?;
        conn.execute(
            "INSERT INTO ncr_transitions (tenant_id, ncr_id, seq, from_state, to_state, operator, at_utc, note) \
             VALUES (?1,?2,0,'','open',?3,?4,'opened')",
            params![tenant.as_str(), ncr.ncr_id, operator, ncr.discovered_at_utc],
        )
        .context("insert ncr opening transition")?;
    }
    let payload = serde_json::json!({
        "ncr_id": ncr.ncr_id,
        "severity": ncr.severity.as_db_str(),
        "category": ncr.category.as_db_str(),
        "discovered_by_operator": ncr.discovered_by_operator,
        "discovered_at_utc": ncr.discovered_at_utc,
        "affected_part_uids": ncr.affected_part_uids,
        "affected_wo_ids": ncr.affected_wo_ids,
        "operator_user_id": operator,
    });
    append_event(
        db_path,
        tenant,
        binary_hash,
        operator,
        EventKind::NcrCreated,
        payload,
    )?;
    Ok(ncr)
}

/// Apply an NCR state transition (operator-driven). Validates the edge against
/// [`allowed_transition`]; a `→ Closed` additionally requires a CAPA that
/// [`Capa::permits_ncr_close`] ([[trust-code-not-operator]]). Appends the
/// transition log row and fires `ncr.state_changed` (+ `ncr.closed` on close).
pub fn transition_ncr(
    db_path: &std::path::Path,
    tenant: TenantId,
    binary_hash: BinaryHash,
    operator: &str,
    ncr_id: &str,
    to: NcrState,
    note: &str,
) -> std::result::Result<Ncr, QualityError> {
    let now = now_rfc3339();
    let (from, capa_id_for_close) = {
        let conn = Connection::open(db_path).map_err(|e| {
            QualityError::Other(anyhow::anyhow!("open DuckDB for NCR transition: {e}"))
        })?;
        ensure_schema(&conn)?;
        let Some(ncr) = get_ncr(&conn, tenant.as_str(), ncr_id)? else {
            return Err(QualityError::NcrNotFound(ncr_id.to_string()));
        };
        let from = ncr.state;
        if from == to {
            return Err(QualityError::IllegalTransition(format!(
                "NCR already in state {}",
                to.as_db_str()
            )));
        }
        if !allowed_transition(from, to) {
            return Err(QualityError::IllegalTransition(format!(
                "transition {} → {} is not allowed",
                from.as_db_str(),
                to.as_db_str()
            )));
        }
        // Close gate: a verified, approved CAPA must exist.
        let capa_id_for_close =
            if to == NcrState::Closed {
                let capas = list_capas(&conn, tenant.as_str(), ncr_id)?;
                match capas.iter().find(|c| c.permits_ncr_close()) {
                    Some(c) => Some(c.capa_id.clone()),
                    None => return Err(QualityError::IllegalTransition(
                        "cannot close: needs a CAPA that is approved and effectiveness-Verified"
                            .to_string(),
                    )),
                }
            } else {
                None
            };
        // Write the transition + new state.
        let seq: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(seq), -1) + 1 FROM ncr_transitions WHERE tenant_id = ?1 AND ncr_id = ?2",
                params![tenant.as_str(), ncr_id],
                |r| r.get(0),
            )
            .context("next transition seq")?;
        conn.execute(
            "INSERT INTO ncr_transitions (tenant_id, ncr_id, seq, from_state, to_state, operator, at_utc, note) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                tenant.as_str(),
                ncr_id,
                seq,
                from.as_db_str(),
                to.as_db_str(),
                operator,
                now,
                note.trim(),
            ],
        )
        .context("insert ncr transition")?;
        if to == NcrState::Closed {
            conn.execute(
                "UPDATE ncrs SET state = ?3, closed_at_utc = ?4, closed_by_operator = ?5 \
                 WHERE tenant_id = ?1 AND ncr_id = ?2",
                params![tenant.as_str(), ncr_id, to.as_db_str(), now, operator],
            )
            .context("update ncr state (closed)")?;
        } else {
            conn.execute(
                "UPDATE ncrs SET state = ?3 WHERE tenant_id = ?1 AND ncr_id = ?2",
                params![tenant.as_str(), ncr_id, to.as_db_str()],
            )
            .context("update ncr state")?;
        }
        (from, capa_id_for_close)
    };

    append_event(
        db_path,
        tenant.clone(),
        binary_hash,
        operator,
        EventKind::NcrStateChanged,
        serde_json::json!({
            "ncr_id": ncr_id,
            "from_state": from.as_db_str(),
            "to_state": to.as_db_str(),
            "operator_user_id": operator,
            "note": note.trim(),
            "changed_at": now,
        }),
    )?;
    if to == NcrState::Closed {
        append_event(
            db_path,
            tenant.clone(),
            binary_hash,
            operator,
            EventKind::NcrClosed,
            serde_json::json!({
                "ncr_id": ncr_id,
                "closed_by_operator": operator,
                "closed_at_utc": now,
                "capa_id": capa_id_for_close,
            }),
        )?;
    }
    let conn = Connection::open(db_path)
        .map_err(|e| QualityError::Other(anyhow::anyhow!("reopen DuckDB: {e}")))?;
    get_ncr(&conn, tenant.as_str(), ncr_id)?
        .ok_or_else(|| QualityError::NcrNotFound(ncr_id.to_string()))
}

/// Boot scan ([[trust-code-not-operator]]): auto-escalate every `Critical` NCR
/// whose escalation window has lapsed. Transitions to `Escalated`, appends the
/// transition log row, and fires `ncr.escalated`. Returns the escalated count.
/// Non-fatal at boot — the caller logs, never `?`-fails boot.
pub fn escalate_overdue_ncrs(
    db_path: &std::path::Path,
    tenant: TenantId,
    binary_hash: BinaryHash,
    operator: &str,
    now: OffsetDateTime,
) -> Result<usize> {
    let due: Vec<Ncr> = {
        let conn = Connection::open(db_path)
            .with_context(|| format!("open tenant DuckDB at {}", db_path.display()))?;
        list_ncrs(&conn, tenant.as_str(), &NcrFilter::default())?
            .into_iter()
            .filter(|n| {
                OffsetDateTime::parse(&n.discovered_at_utc, &Rfc3339)
                    .map(|d| escalation_overdue(n.severity, n.state, d, now))
                    .unwrap_or(false)
            })
            .collect()
    };
    let now_str = now.format(&Rfc3339).context("format escalation stamp")?;
    for n in &due {
        // Each escalation is its own short transaction; a failure on one NCR
        // must not abort the rest of the scan (fail-loud per row, not the batch).
        {
            let conn = Connection::open(db_path)
                .with_context(|| format!("open tenant DuckDB at {}", db_path.display()))?;
            let seq: i64 = conn
                .query_row(
                    "SELECT COALESCE(MAX(seq), -1) + 1 FROM ncr_transitions WHERE tenant_id = ?1 AND ncr_id = ?2",
                    params![tenant.as_str(), n.ncr_id],
                    |r| r.get(0),
                )
                .context("next escalation transition seq")?;
            conn.execute(
                "INSERT INTO ncr_transitions (tenant_id, ncr_id, seq, from_state, to_state, operator, at_utc, note) \
                 VALUES (?1,?2,?3,?4,'escalated',?5,?6,'auto-escalated: critical SLA breach')",
                params![tenant.as_str(), n.ncr_id, seq, n.state.as_db_str(), operator, now_str],
            )
            .context("insert escalation transition")?;
            conn.execute(
                "UPDATE ncrs SET state = 'escalated' WHERE tenant_id = ?1 AND ncr_id = ?2",
                params![tenant.as_str(), n.ncr_id],
            )
            .context("update ncr state (escalated)")?;
        }
        let hours = OffsetDateTime::parse(&n.discovered_at_utc, &Rfc3339)
            .map(|d| (now - d).whole_hours())
            .unwrap_or_default();
        append_event(
            db_path,
            tenant.clone(),
            binary_hash,
            operator,
            EventKind::NcrEscalated,
            serde_json::json!({
                "ncr_id": n.ncr_id,
                "severity": n.severity.as_db_str(),
                "discovered_at_utc": n.discovered_at_utc,
                "escalated_at": now_str,
                "hours_elapsed": hours,
                "operator_user_id": operator,
            }),
        )?;
    }
    Ok(due.len())
}

// ── CAPA writes ─────────────────────────────────────────────────────

/// Operator-supplied CAPA creation input.
#[derive(Debug, Clone)]
pub struct NewCapa {
    pub ncr_id: String,
    pub corrective_action_text: String,
    pub preventive_action_text: String,
    pub responsible_operator: String,
    pub target_close_date: String,
}

/// Create a CAPA for a parent NCR (verdict `Pending`); fire `capa.created`.
pub fn create_capa(
    db_path: &std::path::Path,
    tenant: TenantId,
    binary_hash: BinaryHash,
    operator: &str,
    input: NewCapa,
) -> std::result::Result<Capa, QualityError> {
    if input.corrective_action_text.trim().is_empty() {
        return Err(QualityError::Invalid(
            "corrective action must not be blank".to_string(),
        ));
    }
    if input.preventive_action_text.trim().is_empty() {
        return Err(QualityError::Invalid(
            "preventive action must not be blank".to_string(),
        ));
    }
    let now = now_rfc3339();
    let capa = Capa {
        capa_id: generate_capa_id(),
        ncr_id: input.ncr_id.clone(),
        corrective_action_text: input.corrective_action_text.trim().to_string(),
        preventive_action_text: input.preventive_action_text.trim().to_string(),
        responsible_operator: input.responsible_operator.trim().to_string(),
        target_close_date: input.target_close_date.trim().to_string(),
        actual_close_date: None,
        effectiveness_review_at_utc: None,
        effectiveness_verdict: CapaVerdict::Pending,
        effectiveness_comment: None,
        approved_by_operator: None,
        approved_at_utc: None,
        created_at_utc: now.clone(),
        created_by_operator: operator.to_string(),
    };
    {
        let conn = Connection::open(db_path).map_err(|e| {
            QualityError::Other(anyhow::anyhow!("open DuckDB for CAPA create: {e}"))
        })?;
        ensure_schema(&conn)?;
        if get_ncr(&conn, tenant.as_str(), &capa.ncr_id)?.is_none() {
            return Err(QualityError::NcrNotFound(capa.ncr_id.clone()));
        }
        conn.execute(
            "INSERT INTO capas (capa_id, ncr_id, tenant_id, corrective_action_text, \
             preventive_action_text, responsible_operator, target_close_date, actual_close_date, \
             effectiveness_review_at_utc, effectiveness_verdict, effectiveness_comment, \
             approved_by_operator, approved_at_utc, created_at_utc, created_by_operator) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,NULL,NULL,'pending',NULL,NULL,NULL,?8,?9)",
            params![
                capa.capa_id,
                capa.ncr_id,
                tenant.as_str(),
                capa.corrective_action_text,
                capa.preventive_action_text,
                capa.responsible_operator,
                capa.target_close_date,
                capa.created_at_utc,
                capa.created_by_operator,
            ],
        )
        .context("insert capa row")?;
    }
    append_event(
        db_path,
        tenant,
        binary_hash,
        operator,
        EventKind::CapaCreated,
        serde_json::json!({
            "capa_id": capa.capa_id,
            "ncr_id": capa.ncr_id,
            "responsible_operator": capa.responsible_operator,
            "target_close_date": capa.target_close_date,
            "operator_user_id": operator,
            "created_at_utc": capa.created_at_utc,
        }),
    )?;
    Ok(capa)
}

/// Approve a CAPA's plan; fire `capa.approved`.
pub fn approve_capa(
    db_path: &std::path::Path,
    tenant: TenantId,
    binary_hash: BinaryHash,
    operator: &str,
    capa_id: &str,
) -> std::result::Result<Capa, QualityError> {
    let now = now_rfc3339();
    let ncr_id = {
        let conn = Connection::open(db_path).map_err(|e| {
            QualityError::Other(anyhow::anyhow!("open DuckDB for CAPA approve: {e}"))
        })?;
        ensure_schema(&conn)?;
        let Some(capa) = get_capa(&conn, tenant.as_str(), capa_id)? else {
            return Err(QualityError::CapaNotFound(capa_id.to_string()));
        };
        if capa.approved_at_utc.is_some() {
            return Err(QualityError::IllegalTransition(
                "CAPA already approved".to_string(),
            ));
        }
        conn.execute(
            "UPDATE capas SET approved_by_operator = ?3, approved_at_utc = ?4 \
             WHERE tenant_id = ?1 AND capa_id = ?2",
            params![tenant.as_str(), capa_id, operator, now],
        )
        .context("update capa (approved)")?;
        capa.ncr_id
    };
    append_event(
        db_path,
        tenant.clone(),
        binary_hash,
        operator,
        EventKind::CapaApproved,
        serde_json::json!({
            "capa_id": capa_id,
            "ncr_id": ncr_id,
            "approved_by_operator": operator,
            "approved_at_utc": now,
        }),
    )?;
    reread_capa(db_path, tenant, capa_id)
}

/// Record a CAPA effectiveness verdict + comment; fire
/// `capa.effectiveness_reviewed`.
pub fn review_capa_effectiveness(
    db_path: &std::path::Path,
    tenant: TenantId,
    binary_hash: BinaryHash,
    operator: &str,
    capa_id: &str,
    verdict: CapaVerdict,
    comment: &str,
) -> std::result::Result<Capa, QualityError> {
    let now = now_rfc3339();
    let ncr_id = {
        let conn = Connection::open(db_path).map_err(|e| {
            QualityError::Other(anyhow::anyhow!("open DuckDB for CAPA review: {e}"))
        })?;
        ensure_schema(&conn)?;
        let Some(capa) = get_capa(&conn, tenant.as_str(), capa_id)? else {
            return Err(QualityError::CapaNotFound(capa_id.to_string()));
        };
        conn.execute(
            "UPDATE capas SET effectiveness_verdict = ?3, effectiveness_comment = ?4, \
             effectiveness_review_at_utc = ?5 WHERE tenant_id = ?1 AND capa_id = ?2",
            params![
                tenant.as_str(),
                capa_id,
                verdict.as_db_str(),
                comment.trim(),
                now
            ],
        )
        .context("update capa (effectiveness)")?;
        capa.ncr_id
    };
    append_event(
        db_path,
        tenant.clone(),
        binary_hash,
        operator,
        EventKind::CapaEffectivenessReviewed,
        serde_json::json!({
            "capa_id": capa_id,
            "ncr_id": ncr_id,
            "verdict": verdict.as_db_str(),
            "comment": comment.trim(),
            "effectiveness_review_at_utc": now,
            "operator_user_id": operator,
        }),
    )?;
    reread_capa(db_path, tenant, capa_id)
}

/// Stamp a CAPA's actual close date; fire `capa.closed`.
pub fn close_capa(
    db_path: &std::path::Path,
    tenant: TenantId,
    binary_hash: BinaryHash,
    operator: &str,
    capa_id: &str,
) -> std::result::Result<Capa, QualityError> {
    let now = now_rfc3339();
    let ncr_id = {
        let conn = Connection::open(db_path)
            .map_err(|e| QualityError::Other(anyhow::anyhow!("open DuckDB for CAPA close: {e}")))?;
        ensure_schema(&conn)?;
        let Some(capa) = get_capa(&conn, tenant.as_str(), capa_id)? else {
            return Err(QualityError::CapaNotFound(capa_id.to_string()));
        };
        if capa.actual_close_date.is_some() {
            return Err(QualityError::IllegalTransition(
                "CAPA already closed".to_string(),
            ));
        }
        conn.execute(
            "UPDATE capas SET actual_close_date = ?3 WHERE tenant_id = ?1 AND capa_id = ?2",
            params![tenant.as_str(), capa_id, now],
        )
        .context("update capa (closed)")?;
        capa.ncr_id
    };
    append_event(
        db_path,
        tenant.clone(),
        binary_hash,
        operator,
        EventKind::CapaClosed,
        serde_json::json!({
            "capa_id": capa_id,
            "ncr_id": ncr_id,
            "actual_close_date": now,
            "operator_user_id": operator,
        }),
    )?;
    reread_capa(db_path, tenant, capa_id)
}

fn reread_capa(
    db_path: &std::path::Path,
    tenant: TenantId,
    capa_id: &str,
) -> std::result::Result<Capa, QualityError> {
    let conn = Connection::open(db_path)
        .map_err(|e| QualityError::Other(anyhow::anyhow!("reopen DuckDB: {e}")))?;
    get_capa(&conn, tenant.as_str(), capa_id)?
        .ok_or_else(|| QualityError::CapaNotFound(capa_id.to_string()))
}

/// Open a fresh `Ledger` (after the read/write conn drops — DuckDB single-writer
/// rule) and append one quality audit entry.
fn append_event(
    db_path: &std::path::Path,
    tenant: TenantId,
    binary_hash: BinaryHash,
    operator: &str,
    kind: EventKind,
    payload: serde_json::Value,
) -> Result<()> {
    let mut ledger = Ledger::open(db_path, tenant, binary_hash)
        .context("open audit ledger to record quality event")?;
    ledger
        .append(
            kind,
            serde_json::to_vec(&payload).expect("serialize quality payload"),
            Actor::from_local_cli(Ulid::new().to_string(), operator),
            None,
        )
        .context("append quality audit entry")?;
    Ok(())
}

// ── Refuse-Shipment gate helper (extends S438) ──────────────────────

/// The ids of the NCRs in an `Open`/`Contained` state whose `affected_part_uids`
/// intersect a WO's marked units. Pure helper for the shipment gate (the
/// dispatch-aware resolver lives in `serve.rs`, mirroring `resolve_part_uid_gate`).
pub fn open_ncr_ids_blocking_part_uids(ncrs: &[Ncr], wo_part_uids: &[String]) -> Vec<String> {
    ncrs.iter()
        .filter(|n| n.state.blocks_shipment())
        .filter(|n| {
            n.affected_part_uids
                .iter()
                .any(|u| wo_part_uids.iter().any(|w| w == u))
        })
        .map(|n| n.ncr_id.clone())
        .collect()
}

// ── Photos ──────────────────────────────────────────────────────────

/// `~/.aberp/<tenant>/ncr-photos/<ncr_id>/`. Mirrors the `email-relay-attachments`
/// + `ap-artifacts` per-tenant layout — no new top-level dir.
pub fn photos_root_for_ncr(tenant: &str, ncr_id: &str) -> Result<std::path::PathBuf> {
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("HOME env var not set"))?;
    Ok(home
        .join(crate::build_profile::edition_data_dirname())
        .join(tenant)
        .join("ncr-photos")
        .join(ncr_id))
}

/// One operator-uploaded photo (base64 over the JSON bridge — the SPA has no
/// multipart path).
#[derive(Debug, Clone, Deserialize)]
pub struct PhotoUpload {
    pub filename: String,
    pub data_base64: String,
}

/// Decode + write the photos under `ncr-photos/<ncr_id>/`, returning the stored
/// relative file names. Filenames are sanitized (no traversal). An NCR with no
/// photos writes nothing and returns empty.
pub fn save_photos(
    tenant: &str,
    ncr_id: &str,
    uploads: &[PhotoUpload],
) -> std::result::Result<Vec<String>, QualityError> {
    use base64::Engine as _;
    if uploads.is_empty() {
        return Ok(Vec::new());
    }
    let dir = photos_root_for_ncr(tenant, ncr_id).map_err(QualityError::Other)?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create ncr-photos dir {}", dir.display()))?;
    let mut names = Vec::with_capacity(uploads.len());
    for (i, up) in uploads.iter().enumerate() {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(up.data_base64.trim())
            .map_err(|e| QualityError::Invalid(format!("photo {i}: invalid base64: {e}")))?;
        if bytes.len() > 8 * 1024 * 1024 {
            return Err(QualityError::Invalid(format!("photo {i} exceeds 8 MiB")));
        }
        let safe = crate::email_relay_queue::sanitize_attachment_filename(&up.filename);
        let name = format!("{}_{}", i, safe);
        std::fs::write(dir.join(&name), &bytes).with_context(|| format!("write photo {name}"))?;
        names.push(name);
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::Duration;

    #[test]
    fn ncr_id_and_capa_id_are_prefixed_ulids() {
        let n = generate_ncr_id();
        assert!(n.starts_with("ncr_"), "{n}");
        assert_eq!(n.len(), 4 + 26);
        let c = generate_capa_id();
        assert!(c.starts_with("capa_"), "{c}");
        assert_eq!(c.len(), 5 + 26);
    }

    #[test]
    fn happy_path_transitions_are_allowed() {
        use NcrState::*;
        assert!(allowed_transition(Open, Contained));
        assert!(allowed_transition(Contained, UnderInvestigation));
        assert!(allowed_transition(UnderInvestigation, CorrectionApplied));
        assert!(allowed_transition(CorrectionApplied, Closed));
    }

    #[test]
    fn illegal_transitions_are_refused() {
        use NcrState::*;
        // Cannot skip straight to Closed.
        assert!(!allowed_transition(Open, Closed));
        // Closed is terminal.
        assert!(!allowed_transition(Closed, UnderInvestigation));
        assert!(Closed.is_terminal());
        // Cannot un-contain backwards.
        assert!(!allowed_transition(UnderInvestigation, Open));
    }

    #[test]
    fn any_open_state_can_escalate_and_recover() {
        use NcrState::*;
        for s in [Open, Contained, UnderInvestigation, CorrectionApplied] {
            assert!(allowed_transition(s, Escalated), "{s:?} → Escalated");
        }
        assert!(allowed_transition(Escalated, UnderInvestigation));
        assert!(allowed_transition(Escalated, Closed));
        // A closed NCR cannot escalate.
        assert!(!allowed_transition(Closed, Escalated));
    }

    #[test]
    fn critical_escalates_only_after_window() {
        let t0 = OffsetDateTime::UNIX_EPOCH;
        // 23h59m — not yet.
        assert!(!escalation_overdue(
            NcrSeverity::Critical,
            NcrState::Open,
            t0,
            t0 + Duration::hours(23) + Duration::minutes(59),
        ));
        // 24h — breach.
        assert!(escalation_overdue(
            NcrSeverity::Critical,
            NcrState::Open,
            t0,
            t0 + Duration::hours(24),
        ));
    }

    #[test]
    fn non_critical_and_closed_never_escalate() {
        let t0 = OffsetDateTime::UNIX_EPOCH;
        let way_late = t0 + Duration::days(30);
        assert!(!escalation_overdue(
            NcrSeverity::Major,
            NcrState::Open,
            t0,
            way_late
        ));
        assert!(!escalation_overdue(
            NcrSeverity::Minor,
            NcrState::Open,
            t0,
            way_late
        ));
        assert!(!escalation_overdue(
            NcrSeverity::Critical,
            NcrState::Closed,
            t0,
            way_late
        ));
        assert!(!escalation_overdue(
            NcrSeverity::Critical,
            NcrState::Escalated,
            t0,
            way_late
        ));
    }

    #[test]
    fn capa_permits_close_only_when_approved_and_verified() {
        let mut c = Capa {
            capa_id: "capa_x".into(),
            ncr_id: "ncr_x".into(),
            corrective_action_text: "fix".into(),
            preventive_action_text: "prevent".into(),
            responsible_operator: "op".into(),
            target_close_date: "2026-07-01".into(),
            actual_close_date: None,
            effectiveness_review_at_utc: None,
            effectiveness_verdict: CapaVerdict::Pending,
            effectiveness_comment: None,
            approved_by_operator: None,
            approved_at_utc: None,
            created_at_utc: "2026-06-16T00:00:00Z".into(),
            created_by_operator: "op".into(),
        };
        assert!(!c.permits_ncr_close(), "pending + unapproved");
        c.approved_at_utc = Some("2026-06-16T01:00:00Z".into());
        assert!(!c.permits_ncr_close(), "approved but not verified");
        c.effectiveness_verdict = CapaVerdict::Verified;
        assert!(c.permits_ncr_close(), "approved + verified");
        c.effectiveness_verdict = CapaVerdict::NotEffective;
        assert!(!c.permits_ncr_close(), "approved but not-effective");
    }

    #[test]
    fn open_ncr_gate_blocks_only_on_intersecting_open_states() {
        let mk = |id: &str, state: NcrState, uids: &[&str]| Ncr {
            ncr_id: id.into(),
            discovered_at_utc: "2026-06-16T00:00:00Z".into(),
            discovered_by_operator: "op".into(),
            severity: NcrSeverity::Major,
            category: NcrCategory::Workmanship,
            description: "d".into(),
            affected_part_uids: uids.iter().map(|s| s.to_string()).collect(),
            affected_wo_ids: vec![],
            affected_heat_lots: vec![],
            photos: vec![],
            state,
            closed_at_utc: None,
            closed_by_operator: None,
        };
        let ncrs = vec![
            mk("ncr_open", NcrState::Open, &["dp-A"]),
            mk("ncr_contained", NcrState::Contained, &["dp-B"]),
            mk("ncr_closed", NcrState::Closed, &["dp-C"]),
            mk("ncr_other", NcrState::Open, &["dp-Z"]),
        ];
        let wo_uids = vec!["dp-A".to_string(), "dp-B".to_string(), "dp-C".to_string()];
        let blocking = open_ncr_ids_blocking_part_uids(&ncrs, &wo_uids);
        // dp-A (open) + dp-B (contained) block; dp-C is closed → unblocked.
        assert_eq!(
            blocking,
            vec!["ncr_open".to_string(), "ncr_contained".to_string()]
        );
    }

    fn temp_db() -> (std::path::PathBuf, TenantId, BinaryHash) {
        let dir = std::env::temp_dir()
            .join("aberp-quality-test")
            .join(Ulid::new().to_string());
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("aberp.duckdb");
        {
            let conn = Connection::open(&db_path).unwrap();
            aberp_audit_ledger::ensure_schema(&conn).unwrap();
            ensure_schema(&conn).unwrap();
        }
        (
            db_path,
            TenantId::new("t").unwrap(),
            BinaryHash::from_bytes([0u8; 32]),
        )
    }

    fn count_kind(db_path: &std::path::Path, kind: &str) -> i64 {
        let conn = Connection::open(db_path).unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM audit_ledger WHERE kind = ?1",
            params![kind],
            |r| r.get(0),
        )
        .unwrap()
    }

    fn sample_new_ncr(severity: NcrSeverity, uids: &[&str]) -> NewNcr {
        NewNcr {
            severity,
            category: NcrCategory::Material,
            description: "bore out of tolerance".into(),
            affected_part_uids: uids.iter().map(|s| s.to_string()).collect(),
            affected_wo_ids: vec!["wo-1".into()],
            affected_heat_lots: vec![],
            photos: vec![],
        }
    }

    #[test]
    fn create_ncr_fires_event_and_seeds_transition() {
        let (db, tenant, hash) = temp_db();
        let ncr = create_ncr(
            &db,
            tenant.clone(),
            hash,
            "op",
            sample_new_ncr(NcrSeverity::Major, &["dp-A"]),
        )
        .unwrap();
        assert_eq!(ncr.state, NcrState::Open);
        assert_eq!(count_kind(&db, "ncr.created"), 1);
        let conn = Connection::open(&db).unwrap();
        let t = list_transitions(&conn, tenant.as_str(), &ncr.ncr_id).unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].to_state, "open");
    }

    #[test]
    fn close_is_refused_without_verified_capa_then_succeeds() {
        let (db, tenant, hash) = temp_db();
        let ncr = create_ncr(
            &db,
            tenant.clone(),
            hash,
            "op",
            sample_new_ncr(NcrSeverity::Major, &["dp-A"]),
        )
        .unwrap();
        let id = &ncr.ncr_id;
        // Walk to CorrectionApplied.
        transition_ncr(&db, tenant.clone(), hash, "op", id, NcrState::Contained, "").unwrap();
        transition_ncr(
            &db,
            tenant.clone(),
            hash,
            "op",
            id,
            NcrState::UnderInvestigation,
            "",
        )
        .unwrap();
        transition_ncr(
            &db,
            tenant.clone(),
            hash,
            "op",
            id,
            NcrState::CorrectionApplied,
            "",
        )
        .unwrap();
        // Close refused — no verified CAPA.
        let err =
            transition_ncr(&db, tenant.clone(), hash, "op", id, NcrState::Closed, "").unwrap_err();
        assert!(matches!(err, QualityError::IllegalTransition(_)));
        // Add + approve + verify a CAPA.
        let capa = create_capa(
            &db,
            tenant.clone(),
            hash,
            "op",
            NewCapa {
                ncr_id: id.clone(),
                corrective_action_text: "rework".into(),
                preventive_action_text: "calibrate".into(),
                responsible_operator: "qa".into(),
                target_close_date: "2026-07-01".into(),
            },
        )
        .unwrap();
        approve_capa(&db, tenant.clone(), hash, "op", &capa.capa_id).unwrap();
        review_capa_effectiveness(
            &db,
            tenant.clone(),
            hash,
            "op",
            &capa.capa_id,
            CapaVerdict::Verified,
            "holds",
        )
        .unwrap();
        // Now close succeeds.
        let closed = transition_ncr(
            &db,
            tenant.clone(),
            hash,
            "op",
            id,
            NcrState::Closed,
            "done",
        )
        .unwrap();
        assert_eq!(closed.state, NcrState::Closed);
        assert!(closed.closed_at_utc.is_some());
        assert_eq!(count_kind(&db, "ncr.closed"), 1);
        assert_eq!(count_kind(&db, "capa.created"), 1);
        assert_eq!(count_kind(&db, "capa.approved"), 1);
        assert_eq!(count_kind(&db, "capa.effectiveness_reviewed"), 1);
    }

    #[test]
    fn illegal_transition_is_refused_at_db_layer() {
        let (db, tenant, hash) = temp_db();
        let ncr = create_ncr(
            &db,
            tenant.clone(),
            hash,
            "op",
            sample_new_ncr(NcrSeverity::Minor, &[]),
        )
        .unwrap();
        let err = transition_ncr(
            &db,
            tenant.clone(),
            hash,
            "op",
            &ncr.ncr_id,
            NcrState::Closed,
            "",
        )
        .unwrap_err();
        assert!(matches!(err, QualityError::IllegalTransition(_)));
    }

    #[test]
    fn escalate_overdue_flips_only_late_critical() {
        let (db, tenant, hash) = temp_db();
        // A critical NCR, then back-date its discovered_at to 2 days ago.
        let crit = create_ncr(
            &db,
            tenant.clone(),
            hash,
            "op",
            sample_new_ncr(NcrSeverity::Critical, &["dp-A"]),
        )
        .unwrap();
        let major = create_ncr(
            &db,
            tenant.clone(),
            hash,
            "op",
            sample_new_ncr(NcrSeverity::Major, &["dp-B"]),
        )
        .unwrap();
        let old = (OffsetDateTime::UNIX_EPOCH + Duration::days(100))
            .format(&Rfc3339)
            .unwrap();
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute(
                "UPDATE ncrs SET discovered_at_utc = ?2 WHERE ncr_id = ?1",
                params![crit.ncr_id, old],
            )
            .unwrap();
            conn.execute(
                "UPDATE ncrs SET discovered_at_utc = ?2 WHERE ncr_id = ?1",
                params![major.ncr_id, old],
            )
            .unwrap();
        }
        let now = OffsetDateTime::UNIX_EPOCH + Duration::days(200);
        let n = escalate_overdue_ncrs(&db, tenant.clone(), hash, "boot", now).unwrap();
        assert_eq!(n, 1, "only the critical escalates");
        assert_eq!(count_kind(&db, "ncr.escalated"), 1);
        let conn = Connection::open(&db).unwrap();
        assert_eq!(
            get_ncr(&conn, tenant.as_str(), &crit.ncr_id)
                .unwrap()
                .unwrap()
                .state,
            NcrState::Escalated
        );
        assert_eq!(
            get_ncr(&conn, tenant.as_str(), &major.ncr_id)
                .unwrap()
                .unwrap()
                .state,
            NcrState::Open
        );
        // Idempotent-ish: a second scan re-finds nothing (already escalated).
        assert_eq!(
            escalate_overdue_ncrs(&db, tenant.clone(), hash, "boot", now).unwrap(),
            0
        );
    }
}
