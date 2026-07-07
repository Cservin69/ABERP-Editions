//! S431 — Approved Vendor List (AVL) master data + screening + PO-gate.
//!
//! The defense pivot shipped the AVL *foundation* (S345/S361:
//! [`aberp_compliance::avl`] enums + the `supplier.*` EventKinds) but no firing
//! site. This module is the first firing site: an operator-managed list of
//! vendors with an approval lifecycle ([`aberp_compliance::avl::ApprovedStatus`]),
//! a multi-select category set, a re-screening window (`approved_until_utc`),
//! and a "Screen vendor" action that fires `supplier.export_screened`.
//!
//! ## Conventions mirrored from [`crate::quoting_machines`]
//!
//! Prefixed-ULID id (`avl_<ULID>`), lazy `CREATE TABLE IF NOT EXISTS`,
//! invariants enforced **in code** not via SQL CHECK/triggers
//! ([[no-sql-specific]]), archive-not-delete soft lifecycle (the `Revoked`
//! status + `revoked_reason`, NOT a hard delete). CRUD fires audit via
//! [`append_vendor_event`] called by the serve request wrappers after the DB
//! write (the write conn is scoped/dropped first — DuckDB rejects a second
//! writer; same split as `create_machine_request`).
//!
//! ## The PO gate ([[trust-code-not-operator]])
//!
//! [`po_eligibility`] is the refuse-at-point-of-use guard: a `Suspended` /
//! `Revoked` vendor blocks any new PO referencing them. ADR-0068's full PO
//! surface is still Proposed, so the gate is exercised today through the
//! partner/intake paths via [`crate::serve`]; the rule lives in code so an
//! operator can never "just issue the PO anyway".

use anyhow::{Context, Result};
use duckdb::{params, Connection};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use ulid::Ulid;

use aberp_audit_ledger::{Actor, BinaryHash, EventKind, TenantId};
use aberp_compliance::avl::{
    render_categories, ApprovalCategory, ApprovedStatus, AvlScreeningResult,
};

/// An AVL vendor row, wire + storage shape.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct AvlVendor {
    /// `avl_<26-char-ULID>`.
    pub id: String,
    /// The referenced partner (vendor) master-data id.
    pub partner_id: String,
    /// [`ApprovedStatus`] storage token.
    pub approved_status: String,
    /// [`ApprovalCategory`] storage tokens (multi-select).
    pub approval_categories: Vec<String>,
    /// RFC-3339 re-screening deadline; `None` = no expiry set.
    pub approved_until_utc: Option<String>,
    pub screening_notes: String,
    pub reviewer_login: String,
    /// RFC-3339 of the last review/edit/screen; `None` until first set.
    pub reviewed_at_utc: Option<String>,
    /// Set only when `approved_status == "revoked"`.
    pub revoked_reason: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Request body for create.
#[derive(Deserialize, Debug, Clone)]
pub struct VendorInputs {
    pub partner_id: String,
    /// Initial [`ApprovedStatus`] token (default `pending`).
    #[serde(default = "default_status")]
    pub approved_status: String,
    #[serde(default)]
    pub approval_categories: Vec<String>,
    #[serde(default)]
    pub approved_until_utc: Option<String>,
    #[serde(default)]
    pub screening_notes: String,
}

fn default_status() -> String {
    ApprovedStatus::Pending.as_str().to_string()
}

/// Request body for update (edit-only fields; status changes go through the
/// dedicated status route so the transition invariant is enforced).
#[derive(Deserialize, Debug, Clone)]
pub struct VendorEditInputs {
    #[serde(default)]
    pub approval_categories: Vec<String>,
    #[serde(default)]
    pub approved_until_utc: Option<String>,
    #[serde(default)]
    pub screening_notes: String,
}

/// Request body for a status change.
#[derive(Deserialize, Debug, Clone)]
pub struct VendorStatusInputs {
    /// Target [`ApprovedStatus`] token.
    pub new_status: String,
    /// Required (non-empty) when `new_status == "revoked"`; the revocation
    /// reason. Carried through to the `revoked_reason` column.
    #[serde(default)]
    pub reason: Option<String>,
    /// Manual-override flag: bypasses the normal transition invariant (e.g.
    /// reactivating a `Revoked` vendor). The serve layer only sets this on an
    /// explicit operator confirm.
    #[serde(default)]
    pub force: bool,
}

/// Request body for the "Screen vendor" action.
#[derive(Deserialize, Debug, Clone)]
pub struct ScreenVendorInputs {
    #[serde(default)]
    pub categories_screened: Vec<String>,
    /// [`AvlScreeningResult`] token (default `skipped_no_integration` — the
    /// mock-screening result until a real OFAC/SDN integration lands).
    #[serde(default = "default_screening_result")]
    pub screening_result: String,
}

fn default_screening_result() -> String {
    AvlScreeningResult::SkippedNoIntegration
        .as_str()
        .to_string()
}

/// Field-level validation error (wire shape for 400 responses).
#[derive(Serialize, Debug, PartialEq, Eq, Clone)]
pub struct ValidationError {
    pub field: &'static str,
    pub message: String,
}

fn validate_categories(cats: &[String], errors: &mut Vec<ValidationError>) {
    for c in cats {
        if ApprovalCategory::from_storage_str(c.trim()).is_err() {
            errors.push(ValidationError {
                field: "approval_categories",
                message: format!("Ismeretlen kategória: {c:?}. / Unknown approval category."),
            });
        }
    }
}

fn validate_until(until: &Option<String>, errors: &mut Vec<ValidationError>) {
    if let Some(s) = until {
        if !s.trim().is_empty() && OffsetDateTime::parse(s.trim(), &Rfc3339).is_err() {
            errors.push(ValidationError {
                field: "approved_until_utc",
                message: "Az érvényességi dátum RFC-3339 formátumú legyen. / approved_until_utc \
                          must be RFC-3339."
                    .to_string(),
            });
        }
    }
}

/// Validate create inputs in code (no SQL CHECK). Surfaces every error at once
/// (CLAUDE.md rule 9 / 12).
pub fn validate_vendor_inputs(inputs: &VendorInputs) -> Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();
    if inputs.partner_id.trim().is_empty() {
        errors.push(ValidationError {
            field: "partner_id",
            message: "A beszállító (partner) kötelező. / Vendor (partner) is required.".to_string(),
        });
    } else if inputs.partner_id.trim().len() > 120 {
        errors.push(ValidationError {
            field: "partner_id",
            message: "A partner azonosító legfeljebb 120 karakter. / Partner id max 120 chars."
                .to_string(),
        });
    }
    if ApprovedStatus::from_storage_str(inputs.approved_status.trim()).is_err() {
        errors.push(ValidationError {
            field: "approved_status",
            message: format!(
                "Ismeretlen státusz: {:?}. / Unknown approval status.",
                inputs.approved_status
            ),
        });
    }
    validate_categories(&inputs.approval_categories, &mut errors);
    validate_until(&inputs.approved_until_utc, &mut errors);
    if inputs.screening_notes.len() > 2000 {
        errors.push(ValidationError {
            field: "screening_notes",
            message: "A megjegyzés legfeljebb 2000 karakter. / Notes max 2000 chars.".to_string(),
        });
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Validate edit inputs.
pub fn validate_vendor_edit(inputs: &VendorEditInputs) -> Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();
    validate_categories(&inputs.approval_categories, &mut errors);
    validate_until(&inputs.approved_until_utc, &mut errors);
    if inputs.screening_notes.len() > 2000 {
        errors.push(ValidationError {
            field: "screening_notes",
            message: "A megjegyzés legfeljebb 2000 karakter. / Notes max 2000 chars.".to_string(),
        });
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS avl_vendors (
    id                  VARCHAR NOT NULL PRIMARY KEY,
    tenant_id           VARCHAR NOT NULL,
    partner_id          VARCHAR NOT NULL,
    approved_status     VARCHAR NOT NULL,
    approval_categories VARCHAR NOT NULL,
    approved_until_utc  VARCHAR,
    screening_notes     VARCHAR NOT NULL,
    reviewer_login      VARCHAR NOT NULL,
    reviewed_at_utc     VARCHAR,
    revoked_reason      VARCHAR,
    created_at          VARCHAR NOT NULL,
    updated_at          VARCHAR NOT NULL
);
";

/// Idempotent table creation. No SQL CHECK/index ([[no-sql-specific]] — the
/// table is small master data, scanned in full).
pub fn ensure_schema(conn: &Connection) -> Result<()> {
    // ADR-0098 C2 fix-forward — no-op on a read-only conn (read_returns_readonly
    // read()-side); the schema is created by a writer before any read reaches
    // here. A genuine write mis-routed through read() still fails loud (F5).
    if aberp_audit_ledger::connection_is_read_only(conn) {
        return Ok(());
    }
    conn.execute_batch(SCHEMA_SQL)
        .context("ensure avl_vendors schema")
}

fn now_rfc3339() -> Result<String> {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .context("format avl_vendors timestamp")
}

const COLS: &str = "id, partner_id, approved_status, approval_categories, approved_until_utc, \
                    screening_notes, reviewer_login, reviewed_at_utc, revoked_reason, \
                    created_at, updated_at";

fn row_to_vendor(row: &duckdb::Row<'_>) -> duckdb::Result<AvlVendor> {
    let cats: String = row.get(3)?;
    Ok(AvlVendor {
        id: row.get(0)?,
        partner_id: row.get(1)?,
        approved_status: row.get(2)?,
        // Stored comma-joined; split back to the wire vec. Empty → empty vec.
        approval_categories: if cats.trim().is_empty() {
            Vec::new()
        } else {
            cats.split(',').map(|s| s.trim().to_string()).collect()
        },
        approved_until_utc: row.get(4)?,
        screening_notes: row.get(5)?,
        reviewer_login: row.get(6)?,
        reviewed_at_utc: row.get(7)?,
        revoked_reason: row.get(8)?,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
    })
}

/// Normalize a category list to validated, comma-joined storage. Inputs MUST be
/// pre-validated; an unknown token is a bug here (the validator runs first).
fn cats_to_storage(cats: &[String]) -> Result<String> {
    let parsed: Result<Vec<ApprovalCategory>, _> = cats
        .iter()
        .map(|c| ApprovalCategory::from_storage_str(c.trim()))
        .collect();
    let parsed = parsed.map_err(|e| anyhow::anyhow!("category validated before write: {e}"))?;
    Ok(render_categories(&parsed))
}

fn norm_until(until: &Option<String>) -> Option<String> {
    until
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Insert a new vendor. Inputs MUST be pre-validated by the caller.
pub fn create_vendor(
    conn: &Connection,
    tenant: &str,
    inputs: &VendorInputs,
    reviewer_login: &str,
) -> Result<AvlVendor> {
    ensure_schema(conn)?;
    let id = format!("avl_{}", Ulid::new());
    let now = now_rfc3339()?;
    let status = ApprovedStatus::from_storage_str(inputs.approved_status.trim())
        .map_err(|e| anyhow::anyhow!("status validated before create: {e}"))?;
    let cats = cats_to_storage(&inputs.approval_categories)?;
    let until = norm_until(&inputs.approved_until_utc);
    conn.execute(
        "INSERT INTO avl_vendors (id, tenant_id, partner_id, approved_status, \
         approval_categories, approved_until_utc, screening_notes, reviewer_login, \
         reviewed_at_utc, revoked_reason, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, ?, ?);",
        params![
            &id,
            tenant,
            inputs.partner_id.trim(),
            status.as_str(),
            &cats,
            &until,
            inputs.screening_notes.trim(),
            reviewer_login,
            &now, // reviewed_at_utc — set at creation (the first review)
            &now,
            &now,
        ],
    )
    .context("INSERT into avl_vendors")?;
    get_vendor(conn, tenant, &id)?.context("vendor vanished immediately after insert")
}

/// Fetch a single vendor by id.
pub fn get_vendor(conn: &Connection, tenant: &str, id: &str) -> Result<Option<AvlVendor>> {
    ensure_schema(conn)?;
    let sql = format!("SELECT {COLS} FROM avl_vendors WHERE tenant_id = ? AND id = ?;");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query_map(params![tenant, id], row_to_vendor)?;
    match rows.next() {
        Some(r) => Ok(Some(r?)),
        None => Ok(None),
    }
}

/// The AVL entry for a partner (the PO gate's lookup), newest first if more
/// than one exists. `None` = partner not on the AVL.
pub fn get_vendor_by_partner(
    conn: &Connection,
    tenant: &str,
    partner_id: &str,
) -> Result<Option<AvlVendor>> {
    ensure_schema(conn)?;
    let sql = format!(
        "SELECT {COLS} FROM avl_vendors WHERE tenant_id = ? AND partner_id = ? \
         ORDER BY created_at DESC;"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query_map(params![tenant, partner_id], row_to_vendor)?;
    match rows.next() {
        Some(r) => Ok(Some(r?)),
        None => Ok(None),
    }
}

/// List every vendor (the full AVL — `Revoked` rows stay visible, since the AVL
/// is the record of which vendors are and are not approved). Newest first.
pub fn list_vendors(conn: &Connection, tenant: &str) -> Result<Vec<AvlVendor>> {
    ensure_schema(conn)?;
    let sql =
        format!("SELECT {COLS} FROM avl_vendors WHERE tenant_id = ? ORDER BY created_at DESC;");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![tenant], row_to_vendor)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Update an existing vendor's editable fields (categories / until / notes) +
/// stamp `reviewed_at_utc` + `reviewer_login`. Returns `None` if no such row.
/// Status is NOT changed here ([`set_vendor_status`]).
pub fn update_vendor(
    conn: &Connection,
    tenant: &str,
    id: &str,
    inputs: &VendorEditInputs,
    reviewer_login: &str,
) -> Result<Option<AvlVendor>> {
    ensure_schema(conn)?;
    let cats = cats_to_storage(&inputs.approval_categories)?;
    let until = norm_until(&inputs.approved_until_utc);
    let now = now_rfc3339()?;
    let changed = conn
        .execute(
            "UPDATE avl_vendors SET approval_categories = ?, approved_until_utc = ?, \
             screening_notes = ?, reviewer_login = ?, reviewed_at_utc = ?, updated_at = ? \
             WHERE tenant_id = ? AND id = ?;",
            params![
                &cats,
                &until,
                inputs.screening_notes.trim(),
                reviewer_login,
                &now,
                &now,
                tenant,
                id,
            ],
        )
        .context("UPDATE avl_vendors")?;
    if changed == 0 {
        return Ok(None);
    }
    get_vendor(conn, tenant, id)
}

/// Outcome of a successful status change (the old status for the audit event).
#[derive(Debug, Clone)]
pub struct StatusChange {
    pub vendor: AvlVendor,
    pub old_status: ApprovedStatus,
}

/// Why a status change was refused.
#[derive(Debug)]
pub enum StatusChangeError {
    NotFound,
    /// The normal transition invariant rejected the move (and `force` was not
    /// set). E.g. `Revoked → Approved` without a manual override.
    InvalidTransition {
        from: ApprovedStatus,
        to: ApprovedStatus,
    },
    /// A revoke was requested with no (non-empty) reason.
    MissingRevokeReason,
    Other(anyhow::Error),
}

impl From<anyhow::Error> for StatusChangeError {
    fn from(e: anyhow::Error) -> Self {
        StatusChangeError::Other(e)
    }
}

/// Change a vendor's approval status, enforcing the transition invariant in
/// code ([`ApprovedStatus::can_transition_to`]) unless `force` (manual
/// override). A move to `Revoked` requires a non-empty reason and writes it to
/// `revoked_reason`; a move AWAY from `Revoked` (only via `force`) clears it.
pub fn set_vendor_status(
    conn: &Connection,
    tenant: &str,
    id: &str,
    new_status: ApprovedStatus,
    reason: Option<&str>,
    force: bool,
) -> std::result::Result<StatusChange, StatusChangeError> {
    ensure_schema(conn).map_err(StatusChangeError::Other)?;
    let current = get_vendor(conn, tenant, id)
        .map_err(StatusChangeError::Other)?
        .ok_or(StatusChangeError::NotFound)?;
    let old_status = ApprovedStatus::from_storage_str(&current.approved_status)
        .map_err(|e| StatusChangeError::Other(anyhow::anyhow!("corrupt stored status: {e}")))?;

    if !force && !old_status.can_transition_to(new_status) {
        return Err(StatusChangeError::InvalidTransition {
            from: old_status,
            to: new_status,
        });
    }

    let revoked_reason: Option<String> = if new_status == ApprovedStatus::Revoked {
        let r = reason.map(str::trim).filter(|s| !s.is_empty());
        match r {
            Some(r) => Some(r.to_string()),
            None => return Err(StatusChangeError::MissingRevokeReason),
        }
    } else {
        // Leaving (or not entering) revoked clears any stale reason.
        None
    };

    let now = now_rfc3339().map_err(StatusChangeError::Other)?;
    conn.execute(
        "UPDATE avl_vendors SET approved_status = ?, revoked_reason = ?, reviewed_at_utc = ?, \
         updated_at = ? WHERE tenant_id = ? AND id = ?;",
        params![new_status.as_str(), &revoked_reason, &now, &now, tenant, id],
    )
    .context("UPDATE avl_vendors SET approved_status")
    .map_err(StatusChangeError::Other)?;

    let vendor = get_vendor(conn, tenant, id)
        .map_err(StatusChangeError::Other)?
        .ok_or(StatusChangeError::NotFound)?;
    Ok(StatusChange { vendor, old_status })
}

/// Record a "Screen vendor" action — stamps `reviewed_at_utc` + `reviewer_login`
/// and (when notes are appended) returns the updated row. The audit emission
/// (`supplier.export_screened`) is the serve layer's job; this is the DB half.
/// Returns `None` if no such vendor.
pub fn record_screening(
    conn: &Connection,
    tenant: &str,
    id: &str,
    reviewer_login: &str,
) -> Result<Option<AvlVendor>> {
    ensure_schema(conn)?;
    let now = now_rfc3339()?;
    let changed = conn
        .execute(
            "UPDATE avl_vendors SET reviewer_login = ?, reviewed_at_utc = ?, updated_at = ? \
             WHERE tenant_id = ? AND id = ?;",
            params![reviewer_login, &now, &now, tenant, id],
        )
        .context("UPDATE avl_vendors SET reviewed_at_utc")?;
    if changed == 0 {
        return Ok(None);
    }
    get_vendor(conn, tenant, id)
}

/// The PO gate's verdict for a partner ([[trust-code-not-operator]]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PoEligibility {
    /// No AVL entry (partner not listed) — the gate does not block.
    NoEntry,
    /// On the AVL in a non-blocking status.
    Eligible,
    /// On the AVL in a `Suspended` / `Revoked` status — a new PO must be
    /// refused. Carries the vendor + the blocking status for the operator
    /// message + audit. The vendor is boxed so this variant does not bloat
    /// the (common) `NoEntry` / `Eligible` cases (clippy `large_enum_variant`).
    Blocked {
        vendor: Box<AvlVendor>,
        status: ApprovedStatus,
    },
}

/// Resolve PO eligibility for a partner. Only `Suspended` / `Revoked` block
/// ([`ApprovedStatus::blocks_po`]); an unlisted partner is `NoEntry` (this gate
/// does not require AVL membership, it only blocks the two refused statuses).
pub fn po_eligibility(conn: &Connection, tenant: &str, partner_id: &str) -> Result<PoEligibility> {
    let Some(vendor) = get_vendor_by_partner(conn, tenant, partner_id)? else {
        return Ok(PoEligibility::NoEntry);
    };
    let status = ApprovedStatus::from_storage_str(&vendor.approved_status)
        .map_err(|e| anyhow::anyhow!("corrupt stored status on {}: {e}", vendor.id))?;
    if status.blocks_po() {
        Ok(PoEligibility::Blocked {
            vendor: Box::new(vendor),
            status,
        })
    } else {
        Ok(PoEligibility::Eligible)
    }
}

/// `true` if a vendor's re-screening window has lapsed: it is NOT revoked and
/// its `approved_until_utc` parses to a time strictly before `now`. A vendor
/// with no deadline, an unparseable deadline (corruption — skipped, not
/// crashed), or a future deadline is not overdue.
pub fn vendor_is_overdue(vendor: &AvlVendor, now: OffsetDateTime) -> bool {
    if vendor.approved_status == ApprovedStatus::Revoked.as_str() {
        return false;
    }
    match &vendor.approved_until_utc {
        Some(s) => match OffsetDateTime::parse(s.trim(), &Rfc3339) {
            Ok(until) => until < now,
            Err(_) => false,
        },
        None => false,
    }
}

/// ADR-0098 C2 round-13 — route the AVL audit append through the ONE shared
/// `aberp_db::Handle` (single instance) instead of a separate `Ledger::open`.
/// A separate opener is a 2nd DuckDB instance that checkpoint-races the Handle's
/// WAL on drop and can drop a row just committed THROUGH the Handle from a
/// following `read()` (the `avl_paths_emit` create -> po_check loss). On the
/// shared instance the append is coherent, the writer mutex serialises it
/// (subsuming AUDIT_APPEND_LOCK for handle-routed writes), and the WriteGuard's
/// post-commit hook syncs the mirror. Both the runtime request wrappers AND the
/// boot overdue scan (ADR-0099) use this — the scan runs in-process under
/// `aberp serve` with the shared Handle in scope, so it must not fork.
pub fn append_vendor_event_via_handle(
    db: &aberp_db::HandleArc,
    tenant: TenantId,
    binary_hash: BinaryHash,
    operator_login: &str,
    kind: EventKind,
    payload: Vec<u8>,
) -> Result<()> {
    let mut guard = db
        .write()
        .context("open shared Handle writer for AVL audit")?;
    let conn = guard.conn();
    aberp_audit_ledger::ensure_schema(conn).context("ensure audit-ledger schema")?;
    let meta = aberp_audit_ledger::LedgerMeta::new(tenant, binary_hash);
    let actor = Actor::from_local_cli(Ulid::new().to_string(), operator_login);
    let tx = conn.transaction().context("open AVL audit tx")?;
    aberp_audit_ledger::append_in_tx(&tx, &meta, kind, payload, actor, None)
        .context("append AVL vendor audit entry")?;
    tx.commit().context("commit AVL audit tx")?;
    Ok(())
}

/// Boot-time re-screening reminder: scan every non-revoked vendor whose
/// `approved_until_utc` has lapsed and fire `supplier.avl_screening_overdue`
/// once per such vendor. Returns the count fired. Non-fatal at the call site —
/// a reminder scan must never block boot ([[hulye-biztos]]).
///
/// "Exactly once" = once per overdue vendor per boot scan (the natural reminder
/// cadence; a restart reminds again), pinned by `avl_vendors_route.rs`.
pub fn fire_overdue_screening_reminders(
    db: &aberp_db::HandleArc,
    tenant: TenantId,
    binary_hash: BinaryHash,
    operator_login: &str,
    now: OffsetDateTime,
) -> Result<usize> {
    // ADR-0099 — the boot scan runs in-process under `aberp serve`, so both the
    // read AND the per-vendor audit append route through the ONE shared Handle,
    // never an independent Connection::open / Ledger::open (the 369→515 fork
    // class). The read uses the sanctioned single-instance `db.read()`; the
    // append uses `append_vendor_event_via_handle` (the serialized writer whose
    // WriteGuard drop keeps the mirror in lockstep).
    let overdue: Vec<AvlVendor> = {
        let conn = db
            .read()
            .context("shared Handle read for AVL overdue scan")?;
        list_vendors(&conn, tenant.as_str())?
            .into_iter()
            .filter(|v| vendor_is_overdue(v, now))
            .collect()
    };
    let now_str = now.format(&Rfc3339).context("format overdue scan stamp")?;
    for v in &overdue {
        let payload = crate::audit_payloads::AvlScreeningOverduePayload {
            vendor_id: v.id.clone(),
            partner_id: v.partner_id.clone(),
            approved_until_utc: v.approved_until_utc.clone().unwrap_or_default(),
            decision_time_utc: now_str.clone(),
        };
        append_vendor_event_via_handle(
            db,
            tenant.clone(),
            binary_hash,
            operator_login,
            EventKind::AvlScreeningOverdue,
            payload.to_bytes(),
        )?;
    }
    Ok(overdue.len())
}
