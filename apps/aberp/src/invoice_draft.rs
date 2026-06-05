//! S236 / PR-230b — pre-allocation invoice-draft staging.
//!
//! New `invoice_draft` table that holds invoices in a "staged" state
//! BEFORE the gap-free sequence allocator (ADR-0009 §3 / §169) runs.
//! Solves the §169 vs. Stage 3 hand-off problem PR-230 deliberately
//! left open: a dispatch can now materialise a visible draft on
//! Mark-Shipped without burning a sequence slot — the operator's
//! Issue click in the SPA is the sole allocator gate.
//!
//! # Why a separate table (not nullable `invoice.sequence_number`)
//!
//! Mirrors the [[aberp-ap-module-backend-s177]] / [[nav-as-dr-restore-s180]]
//! / [[quote-intake-crate-s210]] precedent — stage-not-burn in a
//! sibling table, never touching the regulated `invoice` table. Three
//! reasons over the alternative of widening `invoice.sequence_number`
//! to NULL:
//!
//! 1. The `invoice` table's `NOT NULL` + `UNIQUE (series_id,
//!    fiscal_year, sequence_number)` invariants are load-bearing for
//!    ADR-0009 §169 (gap-free). NULL widening would force every
//!    allocator-walk and audit-replay query to filter on
//!    `sequence_number IS NOT NULL` — a maintenance trap.
//! 2. DuckDB's `ALTER COLUMN` is blocked by foreign / CHECK references
//!    per [[duckdb-alter-column-type-check-constraint]]. A schema
//!    migration through `add/backfill/drop/rename` on a regulated
//!    table mid-PR would be risky against existing prod databases.
//! 3. The "row may be edited or deleted freely" affordance only
//!    applies before allocation. Keeping it in a sibling table means
//!    the regulated `invoice` table stays append-only-after-allocation
//!    by construction.
//!
//! # Row lifecycle
//!
//! ```text
//! create_draft_in_tx       (no seq allocated, emits InvoiceStaged)
//!         │
//!         ▼
//! ┌──────────────────┐
//! │ invoice_draft    │── delete_draft_in_tx ──▶ row gone, no seq burn
//! └──────────────────┘
//!         │
//!         │  promote_draft_to_invoice
//!         ▼
//! issue_invoice path runs (allocates seq, inserts `invoice`,
//! emits InvoiceSequenceReserved + InvoiceDraftCreated)
//! ```
//!
//! # Audit chain
//!
//! - `InvoiceStaged` (`invoice.staged`) fires once at draft creation.
//!   Carries `draft_id`, `partner_id`, `source_dispatch_id`, the
//!   actor, and the F8 idempotency key. The prefix is `invoice.*` per
//!   the rationale in [`aberp_audit_ledger::EventKind::InvoiceStaged`]
//!   docs.
//! - On promotion the existing `InvoiceSequenceReserved` +
//!   `InvoiceDraftCreated` pair fires from `issue_invoice::run`. The
//!   chain back to the draft rides the idempotency-key suffix
//!   convention.
//! - On operator delete `InvoiceDraftDeleted` fires once (S239 / PR-233
//!   per the S237 §🟡 #13 finding). Carries `draft_id`,
//!   `source_dispatch_id` (Some(_) when the draft was spawned by a
//!   dispatch — the matching `dispatches.spawned_invoice_id` is NULLed
//!   in the SAME transaction per the S237 §🔴 #1 fix), `partner_id`,
//!   `actor`, and the F8 idempotency key. The pre-S239 posture
//!   (`InvoiceStaged`-without-downstream as the "deleted" signal) was
//!   ambiguous: it could not distinguish "draft still pending operator
//!   issue" from "draft deleted." `InvoiceDraftDeleted` makes the
//!   deletion explicit so forensic "who deleted which draft when"
//!   queries are answerable from the chain alone.

use anyhow::{anyhow, Context, Result};
use duckdb::{params, Connection, Transaction};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use ulid::Ulid;

use aberp_audit_ledger::{append_in_tx, Actor, EventKind, LedgerMeta};
use aberp_dispatch::{Dispatch, InvoiceSpawner};

// ──────────────────────────────────────────────────────────────────────
// Schema.
// ──────────────────────────────────────────────────────────────────────

const INVOICE_DRAFT_SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS invoice_draft (
    drf_id              VARCHAR NOT NULL PRIMARY KEY,
    tenant_id           VARCHAR NOT NULL,
    partner_id          VARCHAR NOT NULL,
    source_dispatch_id  VARCHAR,
    source_wo_id        VARCHAR,
    product_id          VARCHAR NOT NULL,
    qty                 VARCHAR NOT NULL,
    notes               VARCHAR,
    created_at          VARCHAR NOT NULL,
    UNIQUE (tenant_id, source_dispatch_id)
);
CREATE INDEX IF NOT EXISTS invoice_draft_tenant_partner_idx
    ON invoice_draft (tenant_id, partner_id);
CREATE INDEX IF NOT EXISTS invoice_draft_tenant_created_idx
    ON invoice_draft (tenant_id, created_at);
";

/// S255 / PR-244 — additive migration adding `source_quote_id` for the
/// new quote-pickup path. Same `ADD COLUMN IF NOT EXISTS` posture as
/// the PR-97 partners migration: idempotent on a post-S255 boot, fills
/// pre-S255 rows with `NULL` (which already matches their semantic —
/// they were dispatch-spawned, not quote-picked-up).
///
/// Deliberately NOT a UNIQUE constraint per [[no-sql-specific]] — the
/// idempotency gate lives at the quote-pickup route (audit-ledger F8
/// + `quote_intake_log.picked_up_drf_id` writeback), so the DB does
/// not need to enforce single-pickup-per-quote at the row level. A
/// re-pickup after S239 delete must mint a fresh draft for the same
/// quote_id, which a UNIQUE constraint would block.
const INVOICE_DRAFT_S255_MIGRATION_SQL: &str = "
ALTER TABLE invoice_draft
    ADD COLUMN IF NOT EXISTS source_quote_id VARCHAR;
";

/// Idempotent `CREATE TABLE IF NOT EXISTS` for the `invoice_draft`
/// table. Same boot-time posture as `incoming_invoices::ensure_schema`
/// / `restore_from_nav_outgoing::ensure_schema`.
pub fn ensure_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(INVOICE_DRAFT_SCHEMA_SQL)
        .context("ensure invoice_draft schema")?;
    conn.execute_batch(INVOICE_DRAFT_S255_MIGRATION_SQL)
        .context("apply S255 invoice_draft migration (source_quote_id)")
}

// ──────────────────────────────────────────────────────────────────────
// Row shape.
// ──────────────────────────────────────────────────────────────────────

/// One row from `invoice_draft`. Surfaced in the SPA invoice-list as
/// `InvoiceState::Draft` per the third row source (alongside `invoice`
/// → Ready+ states and `restored_invoice` → ExtNav rows).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InvoiceDraft {
    pub drf_id: String,
    pub tenant_id: String,
    pub partner_id: String,
    pub source_dispatch_id: Option<String>,
    pub source_wo_id: Option<String>,
    /// S255 / PR-244 — quote-pickup bridge field. `Some(<quote_id>)`
    /// for a draft minted by the quote-intake pickup route; `None`
    /// for the dispatch-spawn path. Mirrors the `source_dispatch_id`
    /// / `source_wo_id` discriminator pattern (no generic
    /// `source_kind` enum — explicit columns scale to two sources,
    /// and a parallel column makes the SPA's audit-walk pin direct).
    pub source_quote_id: Option<String>,
    pub product_id: String,
    /// Decimal-as-string for the wire form; matches the
    /// `aberp_inventory` / `aberp_work_orders` convention.
    pub qty: String,
    pub notes: Option<String>,
    pub created_at: String,
}

// ──────────────────────────────────────────────────────────────────────
// Audit payload.
// ──────────────────────────────────────────────────────────────────────

/// `invoice.staged` payload. Carries the draft id + bridge fields so
/// a future audit-walk can reconstruct "this draft was spawned by
/// dispatch dsp_X against partner ptr_Y on behalf of WO wo_Z".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InvoiceStagedPayload {
    pub draft_id: String,
    pub tenant_id: String,
    pub partner_id: String,
    pub source_dispatch_id: Option<String>,
    pub source_wo_id: Option<String>,
    /// S255 / PR-244 — `Some(<quote_id>)` when minted by the
    /// quote-pickup route. `serde` emits JSON `null` for `None` so
    /// the audit-walker can tell apart "pre-S255 row, never had the
    /// field" from "S255-aware row, source is dispatch not quote"
    /// (the latter serialises explicitly as `null`).
    pub source_quote_id: Option<String>,
    pub product_id: String,
    /// Decimal-as-string.
    pub qty: String,
    pub actor: String,
    pub idempotency_key: String,
}

impl InvoiceStagedPayload {
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("JSON serialization of InvoiceStagedPayload cannot fail")
    }
}

/// S239 / PR-233 — `invoice.draft_deleted` payload. Mirrors
/// [`InvoiceStagedPayload`]'s shape so a future audit-walker can
/// pair the create + delete entries on `draft_id` and the chain
/// to source dispatch / WO survives the deletion.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InvoiceDraftDeletedPayload {
    pub draft_id: String,
    pub tenant_id: String,
    pub partner_id: String,
    pub source_dispatch_id: Option<String>,
    pub source_wo_id: Option<String>,
    /// S255 / PR-244 — surfaces the quote-pickup chain across the
    /// delete event so a forensic walker can pair "this quote was
    /// picked up, then the operator deleted the draft" without
    /// joining on `quote_intake_log`.
    pub source_quote_id: Option<String>,
    pub actor: String,
    pub idempotency_key: String,
    pub deleted_at: String,
}

impl InvoiceDraftDeletedPayload {
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self)
            .expect("JSON serialization of InvoiceDraftDeletedPayload cannot fail")
    }
}

// ──────────────────────────────────────────────────────────────────────
// Create / read / delete.
// ──────────────────────────────────────────────────────────────────────

/// Inputs to [`create_draft_in_tx`]. `actor` and `idempotency_key` are
/// the audit attribution + F8 dedup pair; `partner_id` is the
/// recipient resolved by the caller (the dispatch's
/// `partner_id` in the spawner path).
#[derive(Debug, Clone)]
pub struct CreateDraftInputs {
    pub tenant: String,
    pub partner_id: String,
    pub source_dispatch_id: Option<String>,
    pub source_wo_id: Option<String>,
    /// S255 / PR-244 — `Some(<quote_id>)` from the quote-pickup
    /// route; the dispatch-spawn path leaves this `None`. Adding it
    /// to the existing `CreateDraftInputs` (rather than introducing a
    /// second constructor) keeps the audit-emit branch single per
    /// CLAUDE.md rule 7 (surface conflicts, don't average them).
    pub source_quote_id: Option<String>,
    pub product_id: String,
    pub qty: Decimal,
    pub notes: Option<String>,
    pub actor: String,
    pub idempotency_key: String,
}

/// Insert one `invoice_draft` row + emit one `InvoiceStaged` audit
/// entry, all in the supplied transaction. Defence-in-depth:
///   - `idempotency_key` non-empty (the ledger pin is the actual gate;
///     this is the early-validate)
///   - `partner_id` non-empty
///   - `product_id` non-empty
pub fn create_draft_in_tx(
    tx: &Transaction<'_>,
    ledger_meta: &LedgerMeta,
    ledger_actor: Actor,
    inputs: CreateDraftInputs,
) -> Result<InvoiceDraft> {
    if inputs.idempotency_key.trim().is_empty() {
        return Err(anyhow!("idempotency_key must be non-empty"));
    }
    if inputs.partner_id.trim().is_empty() {
        return Err(anyhow!("partner_id must be non-empty"));
    }
    if inputs.product_id.trim().is_empty() {
        return Err(anyhow!("product_id must be non-empty"));
    }

    let drf_id = format!("drf_{}", Ulid::new());
    let now = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .context("format invoice_draft.created_at as RFC3339")?;
    let qty_str = inputs.qty.to_string();

    tx.execute(
        "INSERT INTO invoice_draft (
            drf_id, tenant_id, partner_id,
            source_dispatch_id, source_wo_id, source_quote_id,
            product_id, qty, notes, created_at
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?);",
        params![
            &drf_id,
            &inputs.tenant,
            &inputs.partner_id,
            inputs.source_dispatch_id.as_deref(),
            inputs.source_wo_id.as_deref(),
            inputs.source_quote_id.as_deref(),
            &inputs.product_id,
            &qty_str,
            inputs.notes.as_deref(),
            &now,
        ],
    )
    .context("INSERT invoice_draft")?;

    let payload = InvoiceStagedPayload {
        draft_id: drf_id.clone(),
        tenant_id: inputs.tenant.clone(),
        partner_id: inputs.partner_id.clone(),
        source_dispatch_id: inputs.source_dispatch_id.clone(),
        source_wo_id: inputs.source_wo_id.clone(),
        source_quote_id: inputs.source_quote_id.clone(),
        product_id: inputs.product_id.clone(),
        qty: qty_str.clone(),
        actor: inputs.actor.clone(),
        idempotency_key: inputs.idempotency_key.clone(),
    };
    append_in_tx(
        tx,
        ledger_meta,
        EventKind::InvoiceStaged,
        payload.to_bytes(),
        ledger_actor,
        Some(inputs.idempotency_key.clone()),
    )
    .context("audit append InvoiceStaged")?;

    Ok(InvoiceDraft {
        drf_id,
        tenant_id: inputs.tenant,
        partner_id: inputs.partner_id,
        source_dispatch_id: inputs.source_dispatch_id,
        source_wo_id: inputs.source_wo_id,
        source_quote_id: inputs.source_quote_id,
        product_id: inputs.product_id,
        qty: qty_str,
        notes: inputs.notes,
        created_at: now,
    })
}

/// Read one draft by id within the tenant; `None` if absent.
pub fn read_draft(conn: &Connection, tenant: &str, drf_id: &str) -> Result<Option<InvoiceDraft>> {
    let mut stmt = conn
        .prepare(
            "SELECT drf_id, tenant_id, partner_id, source_dispatch_id, source_wo_id,
                    source_quote_id, product_id, qty, notes, created_at
             FROM invoice_draft
             WHERE tenant_id = ? AND drf_id = ?
             LIMIT 1;",
        )
        .context("prepare read_draft")?;
    let mut rows = stmt
        .query_map(params![tenant, drf_id], |row| {
            Ok(InvoiceDraft {
                drf_id: row.get(0)?,
                tenant_id: row.get(1)?,
                partner_id: row.get(2)?,
                source_dispatch_id: row.get(3)?,
                source_wo_id: row.get(4)?,
                source_quote_id: row.get(5)?,
                product_id: row.get(6)?,
                qty: row.get(7)?,
                notes: row.get(8)?,
                created_at: row.get(9)?,
            })
        })
        .context("query read_draft")?;
    match rows.next() {
        Some(r) => Ok(Some(r.context("decode invoice_draft row")?)),
        None => Ok(None),
    }
}

/// List all drafts in a tenant; ordered by `created_at DESC`.
pub fn list_drafts(conn: &Connection, tenant: &str) -> Result<Vec<InvoiceDraft>> {
    let mut stmt = conn
        .prepare(
            "SELECT drf_id, tenant_id, partner_id, source_dispatch_id, source_wo_id,
                    source_quote_id, product_id, qty, notes, created_at
             FROM invoice_draft
             WHERE tenant_id = ?
             ORDER BY created_at DESC;",
        )
        .context("prepare list_drafts")?;
    let rows = stmt
        .query_map(params![tenant], |row| {
            Ok(InvoiceDraft {
                drf_id: row.get(0)?,
                tenant_id: row.get(1)?,
                partner_id: row.get(2)?,
                source_dispatch_id: row.get(3)?,
                source_wo_id: row.get(4)?,
                source_quote_id: row.get(5)?,
                product_id: row.get(6)?,
                qty: row.get(7)?,
                notes: row.get(8)?,
                created_at: row.get(9)?,
            })
        })
        .context("query list_drafts")?;
    rows.collect::<duckdb::Result<Vec<_>>>()
        .context("collect list_drafts")
}

/// S255 / PR-244 — find a draft by `source_quote_id` for the
/// quote-pickup idempotency walk. Returns `Ok(None)` if no draft
/// references the quote (operator never picked it up, or a prior
/// pickup's draft was deleted via S239). At most one row matches by
/// construction: the pickup route checks-and-writes inside the same
/// audit-ledger F8 gate, so a successful retry under the same
/// `quote_pickup:<quote_id>` key returns the existing draft rather
/// than minting a second one.
pub fn find_draft_by_source_quote_id(
    conn: &Connection,
    tenant: &str,
    quote_id: &str,
) -> Result<Option<InvoiceDraft>> {
    let mut stmt = conn
        .prepare(
            "SELECT drf_id, tenant_id, partner_id, source_dispatch_id, source_wo_id,
                    source_quote_id, product_id, qty, notes, created_at
             FROM invoice_draft
             WHERE tenant_id = ? AND source_quote_id = ?
             ORDER BY created_at DESC
             LIMIT 1;",
        )
        .context("prepare find_draft_by_source_quote_id")?;
    let mut rows = stmt
        .query_map(params![tenant, quote_id], |row| {
            Ok(InvoiceDraft {
                drf_id: row.get(0)?,
                tenant_id: row.get(1)?,
                partner_id: row.get(2)?,
                source_dispatch_id: row.get(3)?,
                source_wo_id: row.get(4)?,
                source_quote_id: row.get(5)?,
                product_id: row.get(6)?,
                qty: row.get(7)?,
                notes: row.get(8)?,
                created_at: row.get(9)?,
            })
        })
        .context("query find_draft_by_source_quote_id")?;
    match rows.next() {
        Some(r) => Ok(Some(r.context("decode invoice_draft row")?)),
        None => Ok(None),
    }
}

/// S239 / PR-233 inputs to [`delete_draft_in_tx`].
///
/// The caller supplies the audit attribution + ledger context so the
/// delete can emit `InvoiceDraftDeleted` (S237 §🟡 #13 audit gap fix)
/// in the same transaction as the row removal + dispatch-pointer
/// NULL (S237 §🔴 #1 orphan-pointer fix).
#[derive(Debug, Clone)]
pub struct DeleteDraftInputs {
    pub tenant: String,
    pub drf_id: String,
    /// Free-text operator/adapter identity persisted in the audit
    /// payload's `actor` field — same posture as
    /// [`CreateDraftInputs::actor`].
    pub actor: String,
}

/// Outcome of [`delete_draft_in_tx`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteDraftOutcome {
    /// `false` iff the row was not present at all (idempotent
    /// concurrent-delete safety per the S239 test
    /// `concurrent_delete_idempotent`). Caller maps to 404.
    pub deleted: bool,
    /// Number of `dispatches` rows whose `spawned_invoice_id`
    /// pointer was cleared. `0` for a standalone draft (no source
    /// dispatch) or a dispatch row whose pointer was already NULL;
    /// `> 0` otherwise. Advisory — the invariant "no dispatch row
    /// points at `drf_id` after this call" holds for any non-Err
    /// outcome regardless of the count.
    pub dispatch_pointers_cleared: usize,
}

/// Delete one draft by id within the tenant + clear any
/// `dispatches.spawned_invoice_id` pointer at the draft + emit one
/// `InvoiceDraftDeleted` audit entry — all in the supplied
/// transaction.
///
/// # Why all three happen in one tx
///
/// S237 §🔴 #1 named the orphan-pointer hole: pre-S239 the DELETE
/// ran alone, so a Shipped dispatch's `spawned_invoice_id =
/// drf_<ULID>` survived past the row's removal and the SPA
/// click-through 404'd. The fix is a transactional cascade — either
/// all three writes commit (row gone, pointer NULL, audit
/// recorded) or none do (operator sees the error, no torn state).
///
/// # Idempotency
///
/// Returns `Ok(DeleteDraftOutcome { deleted: false, .. })` when the
/// `drf_id` row is absent. No audit entry fires in that case —
/// "delete a nothing" is a no-op, not a state-change worth
/// recording. The route layer maps `deleted: false` to 404 per the
/// S237 brief's `concurrent_delete_idempotent` expectation. The
/// audit-entry's idempotency key (`draft_delete:<drf_id>`) gates
/// double-emission on RETRIED deletes against the SAME drf_id
/// within a single transaction window (audit-ledger F8 invariant).
pub fn delete_draft_in_tx(
    tx: &Transaction<'_>,
    ledger_meta: &LedgerMeta,
    ledger_actor: Actor,
    inputs: DeleteDraftInputs,
) -> Result<DeleteDraftOutcome> {
    if inputs.drf_id.trim().is_empty() {
        return Err(anyhow!("drf_id must be non-empty"));
    }
    if inputs.tenant.trim().is_empty() {
        return Err(anyhow!("tenant must be non-empty"));
    }

    // Read the row first so the audit payload carries the durable
    // partner_id / source_dispatch_id / source_wo_id even after the
    // DELETE removes the row. A SELECT-then-DELETE inside the same
    // tx is race-free against any concurrent writer because the tx
    // holds a write lock on the rows it touches.
    let prior_row = tx
        .query_row(
            "SELECT partner_id, source_dispatch_id, source_wo_id, source_quote_id
             FROM invoice_draft
             WHERE tenant_id = ? AND drf_id = ?
             LIMIT 1;",
            params![&inputs.tenant, &inputs.drf_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .ok();

    let Some((partner_id, source_dispatch_id, source_wo_id, source_quote_id)) = prior_row else {
        return Ok(DeleteDraftOutcome {
            deleted: false,
            dispatch_pointers_cleared: 0,
        });
    };

    // NULL any dispatch row pointing at this draft BEFORE the DELETE
    // so a `FOREIGN KEY`-like reasoning chain inspector sees the
    // pointer clear precede the target removal. (DuckDB doesn't
    // enforce the FK by construction per [[no-sql-specific]]; the
    // ordering is purely readability.)
    let dispatch_pointers_cleared =
        aberp_dispatch::null_spawned_invoice_id_in_tx(tx, &inputs.tenant, &inputs.drf_id)
            .context("NULL dispatches.spawned_invoice_id in delete_draft_in_tx")?;

    let n = tx
        .execute(
            "DELETE FROM invoice_draft WHERE tenant_id = ? AND drf_id = ?;",
            params![&inputs.tenant, &inputs.drf_id],
        )
        .context("DELETE invoice_draft")?;
    debug_assert!(
        n > 0,
        "row vanished between SELECT and DELETE in the same tx — DuckDB invariant violated"
    );

    let now = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .context("format invoice_draft.deleted_at as RFC3339")?;
    let idempotency_key = format!("draft_delete:{}", inputs.drf_id);
    let payload = InvoiceDraftDeletedPayload {
        draft_id: inputs.drf_id.clone(),
        tenant_id: inputs.tenant.clone(),
        partner_id,
        source_dispatch_id,
        source_wo_id,
        source_quote_id,
        actor: inputs.actor.clone(),
        idempotency_key: idempotency_key.clone(),
        deleted_at: now,
    };
    append_in_tx(
        tx,
        ledger_meta,
        EventKind::InvoiceDraftDeleted,
        payload.to_bytes(),
        ledger_actor,
        Some(idempotency_key),
    )
    .context("audit append InvoiceDraftDeleted")?;

    Ok(DeleteDraftOutcome {
        deleted: true,
        dispatch_pointers_cleared,
    })
}

// ──────────────────────────────────────────────────────────────────────
// Dispatch-spawner adapter.
// ──────────────────────────────────────────────────────────────────────

/// Real `InvoiceSpawner` that PR-230b lands. Replaces
/// `aberp_dispatch::NoopInvoiceSpawner` in the serve boot path.
///
/// On every `dispatches → Shipped` flip, this inserts an
/// `invoice_draft` row + emits one `InvoiceStaged` audit entry in the
/// dispatch's own transaction per ADR-0064 §"Invariants pinned" #6.
/// Returns the `drf_<ULID>` id so the dispatch row's
/// `spawned_invoice_id` records the bridge.
///
/// # Why the spawner stores a `drf_*` id in `spawned_invoice_id`
///
/// PR-230 wired `Dispatch.spawned_invoice_id: Option<String>` as
/// "whatever the spawner returned." The brief named "spawned invoice
/// id" but the value is in fact "spawned-invoice-or-draft id" — the
/// dispatch tracks the bridge target regardless of allocation state.
/// The SPA's dispatch-detail click-through resolves which table to
/// open by prefix: `drf_*` → draft detail; `inv_*` → invoice detail
/// (post-promotion path, when the operator's Issue click flipped the
/// draft into a real invoice and the dispatch row was UPDATEd with
/// the new `inv_*` id — that re-link is the SPA's responsibility, not
/// the spawner's).
pub struct BillingInvoiceSpawner<'a> {
    pub tenant: String,
    pub actor: String,
    pub ledger_meta: &'a LedgerMeta,
    pub ledger_actor: Actor,
}

impl<'a> InvoiceSpawner for BillingInvoiceSpawner<'a> {
    fn spawn(
        &self,
        tx: &Transaction<'_>,
        dispatch: &Dispatch,
        wo_product_id: &str,
        wo_qty_target: Decimal,
        idempotency_key: &str,
    ) -> anyhow::Result<Option<String>> {
        let draft = create_draft_in_tx(
            tx,
            self.ledger_meta,
            self.ledger_actor.clone(),
            CreateDraftInputs {
                tenant: self.tenant.clone(),
                partner_id: dispatch.partner_id.clone(),
                source_dispatch_id: Some(dispatch.dsp_id.clone()),
                source_wo_id: Some(dispatch.wo_id.clone()),
                source_quote_id: None,
                product_id: wo_product_id.to_string(),
                qty: wo_qty_target,
                notes: dispatch.notes.clone(),
                actor: self.actor.clone(),
                idempotency_key: idempotency_key.to_string(),
            },
        )?;
        Ok(Some(draft.drf_id))
    }
}

// ──────────────────────────────────────────────────────────────────────
// Unit tests.
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use aberp_audit_ledger::{
        ensure_schema as audit_ensure_schema, BinaryHash, LedgerMeta, TenantId,
    };
    use rust_decimal::Decimal;
    use std::str::FromStr;

    fn open_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("open in-memory DuckDB");
        audit_ensure_schema(&conn).expect("audit-ledger schema");
        ensure_schema(&conn).expect("invoice_draft schema");
        conn
    }

    fn ledger_meta() -> LedgerMeta {
        LedgerMeta::new(
            TenantId::new("test-tenant").unwrap(),
            BinaryHash::from_bytes([0u8; 32]),
        )
    }

    fn actor() -> Actor {
        Actor::test_only()
    }

    #[test]
    fn create_draft_inserts_row_and_emits_audit() {
        let mut conn = open_conn();
        let tx = conn.transaction().unwrap();
        let draft = create_draft_in_tx(
            &tx,
            &ledger_meta(),
            actor(),
            CreateDraftInputs {
                tenant: "test-tenant".to_string(),
                partner_id: "ptr_TESTPARTNER000000000000".to_string(),
                source_dispatch_id: Some("dsp_TESTDISPATCH00000000000".to_string()),
                source_wo_id: Some("wo_TESTWORKORDER0000000000".to_string()),
                source_quote_id: None,
                product_id: "prd_TESTPRODUCT00000000000".to_string(),
                qty: Decimal::from_str("3.5").unwrap(),
                notes: Some("test note".to_string()),
                actor: "test-actor".to_string(),
                idempotency_key: "idem-key-1".to_string(),
            },
        )
        .expect("create_draft_in_tx");
        tx.commit().unwrap();

        assert!(draft.drf_id.starts_with("drf_"));
        assert_eq!(draft.qty, "3.5");

        let round_trip = read_draft(&conn, "test-tenant", &draft.drf_id)
            .unwrap()
            .expect("draft round-trip");
        assert_eq!(round_trip, draft);
    }

    #[test]
    fn delete_draft_removes_row_and_emits_audit() {
        let mut conn = open_conn();
        // S239 / PR-233 — even a standalone-draft delete touches the
        // `dispatches` table (NULL-pointer cascade with zero matches).
        // The real route handler defensively ensures this schema; the
        // unit test mirrors that posture.
        ensure_minimal_dispatch_schema(&conn);
        let tx = conn.transaction().unwrap();
        let draft = create_draft_in_tx(
            &tx,
            &ledger_meta(),
            actor(),
            CreateDraftInputs {
                tenant: "test-tenant".to_string(),
                partner_id: "ptr_X".to_string(),
                source_dispatch_id: None,
                source_wo_id: None,
                source_quote_id: None,
                product_id: "prd_X".to_string(),
                qty: Decimal::ONE,
                notes: None,
                actor: "a".to_string(),
                idempotency_key: "idem".to_string(),
            },
        )
        .unwrap();
        tx.commit().unwrap();

        let tx2 = conn.transaction().unwrap();
        let outcome = delete_draft_in_tx(
            &tx2,
            &ledger_meta(),
            actor(),
            DeleteDraftInputs {
                tenant: "test-tenant".to_string(),
                drf_id: draft.drf_id.clone(),
                actor: "operator-A".to_string(),
            },
        )
        .unwrap();
        tx2.commit().unwrap();
        assert!(outcome.deleted);
        // Standalone draft (no source_dispatch_id) — no dispatch
        // pointers to clear.
        assert_eq!(outcome.dispatch_pointers_cleared, 0);
        assert!(read_draft(&conn, "test-tenant", &draft.drf_id)
            .unwrap()
            .is_none());
    }

    #[test]
    fn create_draft_refuses_empty_idempotency_key() {
        let mut conn = open_conn();
        let tx = conn.transaction().unwrap();
        let err = create_draft_in_tx(
            &tx,
            &ledger_meta(),
            actor(),
            CreateDraftInputs {
                tenant: "test-tenant".to_string(),
                partner_id: "ptr_X".to_string(),
                source_dispatch_id: None,
                source_wo_id: None,
                source_quote_id: None,
                product_id: "prd_X".to_string(),
                qty: Decimal::ONE,
                notes: None,
                actor: "a".to_string(),
                idempotency_key: "  ".to_string(),
            },
        );
        assert!(err.is_err());
    }

    #[test]
    fn invoice_staged_payload_round_trip() {
        let p = InvoiceStagedPayload {
            draft_id: "drf_X".to_string(),
            tenant_id: "t".to_string(),
            partner_id: "ptr_X".to_string(),
            source_dispatch_id: Some("dsp_X".to_string()),
            source_wo_id: Some("wo_X".to_string()),
            source_quote_id: None,
            product_id: "prd_X".to_string(),
            qty: "1".to_string(),
            actor: "a".to_string(),
            idempotency_key: "i".to_string(),
        };
        let back: InvoiceStagedPayload = serde_json::from_slice(&p.to_bytes()).unwrap();
        assert_eq!(back, p);
    }

    /// `source_dispatch_id: None` MUST serialize as JSON null so the
    /// audit-walker can distinguish "dispatch-spawned" from "writer
    /// forgot." Same posture as the dispatch shipped pin
    /// `shipped_payload_spawned_invoice_id_none_serializes_as_null_not_omitted`.
    #[test]
    fn invoice_staged_payload_none_serializes_as_null_not_omitted() {
        let p = InvoiceStagedPayload {
            draft_id: "drf_X".to_string(),
            tenant_id: "t".to_string(),
            partner_id: "ptr_X".to_string(),
            source_dispatch_id: None,
            source_wo_id: None,
            source_quote_id: None,
            product_id: "prd_X".to_string(),
            qty: "1".to_string(),
            actor: "a".to_string(),
            idempotency_key: "i".to_string(),
        };
        let v: serde_json::Value = serde_json::from_slice(&p.to_bytes()).unwrap();
        assert!(v["source_dispatch_id"].is_null());
        assert!(v["source_wo_id"].is_null());
    }

    /// S236 / PR-230b — `BillingInvoiceSpawner` is the type that
    /// replaces `aberp_dispatch::NoopInvoiceSpawner` in the production
    /// `serve.rs` `mark_dispatch_shipped_request` path. Driving the
    /// trait directly pins the exact return shape `mark_shipped`
    /// consumes (`Ok(Some(drf_<ULID>))`); a future refactor that drops
    /// the dispatch bridge fields would surface here as well as in
    /// the dispatch crate's `mark_shipped_writes_movement_and_spawns_
    /// draft_in_same_tx` integration test.
    #[test]
    fn billing_spawner_returns_some_draft_id_and_persists_row() {
        use aberp_dispatch::{CarrierKind, Dispatch, DispatchState, InvoiceSpawner};
        let mut conn = open_conn();
        let dsp = Dispatch {
            dsp_id: "dsp_TEST".to_string(),
            wo_id: "wo_TEST".to_string(),
            partner_id: "ptr_TEST".to_string(),
            state: DispatchState::Drafted,
            created_at: "2026-06-03T00:00:00Z".to_string(),
            shipped_at: None,
            cancelled_at: None,
            carrier_kind: Some(CarrierKind::CustomerPickup),
            tracking_number: None,
            spawned_invoice_id: None,
            notes: Some("test note".to_string()),
        };
        let meta = ledger_meta();
        let spawner = BillingInvoiceSpawner {
            tenant: "test-tenant".to_string(),
            actor: "test-actor".to_string(),
            ledger_meta: &meta,
            ledger_actor: actor(),
        };
        let tx = conn.transaction().unwrap();
        let returned = spawner
            .spawn(
                &tx,
                &dsp,
                "prd_TEST",
                Decimal::from_str("5.0").unwrap(),
                "idem-spawn",
            )
            .expect("spawner");
        tx.commit().unwrap();
        let drf_id = returned.expect("spawner returns Some(drf_...)");
        assert!(drf_id.starts_with("drf_"));
        let row = read_draft(&conn, "test-tenant", &drf_id)
            .unwrap()
            .expect("draft row");
        assert_eq!(row.partner_id, "ptr_TEST");
        assert_eq!(row.product_id, "prd_TEST");
        assert_eq!(row.qty, "5.0");
        assert_eq!(row.source_dispatch_id, Some("dsp_TEST".to_string()));
        assert_eq!(row.source_wo_id, Some("wo_TEST".to_string()));
        assert_eq!(row.notes, Some("test note".to_string()));
    }

    /// S236 / PR-230b — second `BillingInvoiceSpawner.spawn` call with
    /// the same idempotency key MUST loud-fail (the audit ledger's F8
    /// pin is the gate). Without this pin, a retried dispatch ship
    /// could silently create two `invoice_draft` rows for the same
    /// dispatch — the [[trust-code-not-operator]] failure mode this
    /// PR is meant to close.
    #[test]
    fn billing_spawner_loud_fails_on_duplicate_idempotency_key() {
        use aberp_dispatch::{CarrierKind, Dispatch, DispatchState, InvoiceSpawner};
        let mut conn = open_conn();
        let dsp = Dispatch {
            dsp_id: "dsp_TEST".to_string(),
            wo_id: "wo_TEST".to_string(),
            partner_id: "ptr_TEST".to_string(),
            state: DispatchState::Drafted,
            created_at: "2026-06-03T00:00:00Z".to_string(),
            shipped_at: None,
            cancelled_at: None,
            carrier_kind: Some(CarrierKind::CustomerPickup),
            tracking_number: None,
            spawned_invoice_id: None,
            notes: None,
        };
        let meta = ledger_meta();
        let spawner = BillingInvoiceSpawner {
            tenant: "test-tenant".to_string(),
            actor: "test-actor".to_string(),
            ledger_meta: &meta,
            ledger_actor: actor(),
        };
        let tx = conn.transaction().unwrap();
        spawner
            .spawn(&tx, &dsp, "prd_TEST", Decimal::ONE, "idem-dup")
            .unwrap();
        tx.commit().unwrap();
        let tx2 = conn.transaction().unwrap();
        let err = spawner.spawn(&tx2, &dsp, "prd_TEST", Decimal::ONE, "idem-dup");
        assert!(
            err.is_err(),
            "BillingInvoiceSpawner must loud-fail on duplicate idempotency key (got {err:?})"
        );
    }

    // ── S239 / PR-233 — delete-cascade tests ─────────────────────────

    /// Minimal `dispatches` schema fixture for the cascade tests.
    /// Mirrors only the columns `null_spawned_invoice_id_in_tx` touches
    /// (`tenant_id`, `dsp_id`, `spawned_invoice_id`) so the test stays
    /// independent of the full aberp-dispatch migration; the test
    /// shape is pinned by `aberp_dispatch::ensure_schema` in the
    /// integration cross-crate tests on the dispatch crate.
    ///
    /// We deliberately use a fresh `CREATE TABLE` here rather than
    /// `aberp_dispatch::ensure_schema` so the unit-test crate does
    /// not depend on the runtime migration; the dispatch-crate's own
    /// schema test guards the column set.
    fn ensure_minimal_dispatch_schema(conn: &Connection) {
        // The full schema would carry many more columns; the cascade
        // touches only `spawned_invoice_id`. Aligning with the real
        // schema's column names + nullability so the helper's SQL
        // matches verbatim against either schema.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS dispatches (
                dsp_id              VARCHAR NOT NULL PRIMARY KEY,
                tenant_id           VARCHAR NOT NULL,
                spawned_invoice_id  VARCHAR
            );",
        )
        .expect("create minimal dispatches schema");
    }

    fn insert_dispatch_pointing_at(conn: &Connection, dsp_id: &str, drf_id: &str) {
        conn.execute(
            "INSERT INTO dispatches (dsp_id, tenant_id, spawned_invoice_id)
             VALUES (?, ?, ?);",
            params![dsp_id, "test-tenant", drf_id],
        )
        .expect("insert dispatch row");
    }

    fn read_dispatch_pointer(conn: &Connection, dsp_id: &str) -> Option<String> {
        conn.query_row(
            "SELECT spawned_invoice_id FROM dispatches
             WHERE tenant_id = ? AND dsp_id = ? LIMIT 1;",
            params!["test-tenant", dsp_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .expect("read dispatch row")
    }

    /// 🔴 S237 #1 — when a draft was spawned by a dispatch and the
    /// operator deletes the draft, the dispatch's `spawned_invoice_id`
    /// pointer MUST be NULLed in the same tx so the SPA's
    /// click-through never reaches a 404 arm.
    #[test]
    fn delete_draft_with_source_dispatch_nulls_pointer() {
        let mut conn = open_conn();
        ensure_minimal_dispatch_schema(&conn);

        let tx = conn.transaction().unwrap();
        let draft = create_draft_in_tx(
            &tx,
            &ledger_meta(),
            actor(),
            CreateDraftInputs {
                tenant: "test-tenant".to_string(),
                partner_id: "ptr_X".to_string(),
                source_dispatch_id: Some("dsp_X".to_string()),
                source_wo_id: Some("wo_X".to_string()),
                source_quote_id: None,
                product_id: "prd_X".to_string(),
                qty: Decimal::ONE,
                notes: None,
                actor: "a".to_string(),
                idempotency_key: "idem-create".to_string(),
            },
        )
        .unwrap();
        tx.commit().unwrap();
        insert_dispatch_pointing_at(&conn, "dsp_X", &draft.drf_id);

        let tx2 = conn.transaction().unwrap();
        let outcome = delete_draft_in_tx(
            &tx2,
            &ledger_meta(),
            actor(),
            DeleteDraftInputs {
                tenant: "test-tenant".to_string(),
                drf_id: draft.drf_id.clone(),
                actor: "operator-A".to_string(),
            },
        )
        .unwrap();
        tx2.commit().unwrap();

        assert!(outcome.deleted);
        assert_eq!(outcome.dispatch_pointers_cleared, 1);
        assert_eq!(read_dispatch_pointer(&conn, "dsp_X"), None);
        assert!(read_draft(&conn, "test-tenant", &draft.drf_id)
            .unwrap()
            .is_none());
    }

    /// 🟡 S237 #13 — the `InvoiceDraftDeleted` audit entry MUST fire
    /// in the same tx as the DELETE so a forensic walker can see
    /// "who deleted which draft when," and its payload MUST carry the
    /// `source_dispatch_id` so the chain back to the originating
    /// dispatch survives the row's removal.
    #[test]
    fn delete_draft_emits_audit_with_source_dispatch_id() {
        let mut conn = open_conn();
        ensure_minimal_dispatch_schema(&conn);

        let tx = conn.transaction().unwrap();
        let draft = create_draft_in_tx(
            &tx,
            &ledger_meta(),
            actor(),
            CreateDraftInputs {
                tenant: "test-tenant".to_string(),
                partner_id: "ptr_DELETE".to_string(),
                source_dispatch_id: Some("dsp_DELETE".to_string()),
                source_wo_id: Some("wo_DELETE".to_string()),
                source_quote_id: None,
                product_id: "prd_DELETE".to_string(),
                qty: Decimal::from_str("2.5").unwrap(),
                notes: None,
                actor: "a".to_string(),
                idempotency_key: "idem-c".to_string(),
            },
        )
        .unwrap();
        tx.commit().unwrap();
        insert_dispatch_pointing_at(&conn, "dsp_DELETE", &draft.drf_id);

        let tx2 = conn.transaction().unwrap();
        delete_draft_in_tx(
            &tx2,
            &ledger_meta(),
            actor(),
            DeleteDraftInputs {
                tenant: "test-tenant".to_string(),
                drf_id: draft.drf_id.clone(),
                actor: "operator-B".to_string(),
            },
        )
        .unwrap();
        tx2.commit().unwrap();

        // Query the audit-ledger storage table directly — same
        // posture as the verify-bundle integration tests that walk
        // chain.jsonl through `audit_ledger`'s row-shape.
        let mut stmt = conn
            .prepare(
                "SELECT payload FROM audit_ledger
                 WHERE kind = 'invoice.draft_deleted'
                 ORDER BY seq DESC LIMIT 1;",
            )
            .expect("prepare audit query");
        let payload_bytes: Vec<u8> = stmt
            .query_row([], |row| row.get::<_, Vec<u8>>(0))
            .expect("audit row exists");
        let payload: InvoiceDraftDeletedPayload =
            serde_json::from_slice(&payload_bytes).expect("audit payload decodes");
        assert_eq!(payload.draft_id, draft.drf_id);
        assert_eq!(payload.partner_id, "ptr_DELETE");
        assert_eq!(payload.source_dispatch_id, Some("dsp_DELETE".to_string()));
        assert_eq!(payload.source_wo_id, Some("wo_DELETE".to_string()));
        assert_eq!(payload.actor, "operator-B");
        assert_eq!(
            payload.idempotency_key,
            format!("draft_delete:{}", draft.drf_id)
        );
    }

    /// Round-trip for the new payload shape — same posture as
    /// `invoice_staged_payload_round_trip`. Pins the wire form so a
    /// future contributor adding a field surfaces the breakage at
    /// `cargo test` not at runtime.
    #[test]
    fn invoice_draft_deleted_payload_round_trip() {
        let p = InvoiceDraftDeletedPayload {
            draft_id: "drf_X".to_string(),
            tenant_id: "t".to_string(),
            partner_id: "ptr_X".to_string(),
            source_dispatch_id: Some("dsp_X".to_string()),
            source_wo_id: Some("wo_X".to_string()),
            source_quote_id: None,
            actor: "a".to_string(),
            idempotency_key: "draft_delete:drf_X".to_string(),
            deleted_at: "2026-06-04T10:00:00Z".to_string(),
        };
        let back: InvoiceDraftDeletedPayload = serde_json::from_slice(&p.to_bytes()).unwrap();
        assert_eq!(back, p);

        // `None` source ids serialise as JSON null (same posture as
        // `invoice_staged_payload_none_serializes_as_null_not_omitted`).
        let p_none = InvoiceDraftDeletedPayload {
            draft_id: "drf_Y".to_string(),
            tenant_id: "t".to_string(),
            partner_id: "ptr_Y".to_string(),
            source_dispatch_id: None,
            source_wo_id: None,
            source_quote_id: None,
            actor: "a".to_string(),
            idempotency_key: "draft_delete:drf_Y".to_string(),
            deleted_at: "2026-06-04T10:00:00Z".to_string(),
        };
        let v: serde_json::Value = serde_json::from_slice(&p_none.to_bytes()).unwrap();
        assert!(v["source_dispatch_id"].is_null());
        assert!(v["source_wo_id"].is_null());
    }

    /// Standalone draft (no `source_dispatch_id`) — the cascade MUST
    /// be a no-op against the `dispatches` table. Verifies the
    /// SELECT-on-pointer is correctly scoped so a freshly-restored
    /// dispatch row whose `spawned_invoice_id` happens to be the same
    /// ULID by accident does NOT get touched (drf_<ULID> collision
    /// is impossible by construction, but the test pins the SQL
    /// predicate's tenant scoping anyway).
    #[test]
    fn delete_draft_without_source_dispatch_no_dispatch_touch() {
        let mut conn = open_conn();
        ensure_minimal_dispatch_schema(&conn);

        // Insert a dispatch that points at SOMEONE ELSE's draft.
        // Cascade MUST leave it alone.
        insert_dispatch_pointing_at(&conn, "dsp_OTHER", "drf_someoneelse");

        let tx = conn.transaction().unwrap();
        let draft = create_draft_in_tx(
            &tx,
            &ledger_meta(),
            actor(),
            CreateDraftInputs {
                tenant: "test-tenant".to_string(),
                partner_id: "ptr_X".to_string(),
                source_dispatch_id: None,
                source_wo_id: None,
                source_quote_id: None,
                product_id: "prd_X".to_string(),
                qty: Decimal::ONE,
                notes: None,
                actor: "a".to_string(),
                idempotency_key: "idem-standalone".to_string(),
            },
        )
        .unwrap();
        tx.commit().unwrap();

        let tx2 = conn.transaction().unwrap();
        let outcome = delete_draft_in_tx(
            &tx2,
            &ledger_meta(),
            actor(),
            DeleteDraftInputs {
                tenant: "test-tenant".to_string(),
                drf_id: draft.drf_id.clone(),
                actor: "operator-A".to_string(),
            },
        )
        .unwrap();
        tx2.commit().unwrap();

        assert!(outcome.deleted);
        assert_eq!(outcome.dispatch_pointers_cleared, 0);
        // The dispatch pointing at someone else's draft is untouched.
        assert_eq!(
            read_dispatch_pointer(&conn, "dsp_OTHER"),
            Some("drf_someoneelse".to_string())
        );
    }

    /// Calling delete twice on the same draft → second call returns
    /// `deleted: false` (caller maps to 404), NOT a 500. Pins the
    /// S237 brief's `concurrent_delete_idempotent` expectation.
    #[test]
    fn concurrent_delete_idempotent() {
        let mut conn = open_conn();
        ensure_minimal_dispatch_schema(&conn);

        let tx = conn.transaction().unwrap();
        let draft = create_draft_in_tx(
            &tx,
            &ledger_meta(),
            actor(),
            CreateDraftInputs {
                tenant: "test-tenant".to_string(),
                partner_id: "ptr_X".to_string(),
                source_dispatch_id: None,
                source_wo_id: None,
                source_quote_id: None,
                product_id: "prd_X".to_string(),
                qty: Decimal::ONE,
                notes: None,
                actor: "a".to_string(),
                idempotency_key: "idem-concurrent".to_string(),
            },
        )
        .unwrap();
        tx.commit().unwrap();

        // First delete — succeeds.
        let tx2 = conn.transaction().unwrap();
        let first = delete_draft_in_tx(
            &tx2,
            &ledger_meta(),
            actor(),
            DeleteDraftInputs {
                tenant: "test-tenant".to_string(),
                drf_id: draft.drf_id.clone(),
                actor: "operator-A".to_string(),
            },
        )
        .unwrap();
        tx2.commit().unwrap();
        assert!(first.deleted);

        // Second delete — returns deleted: false, NOT an Err.
        let tx3 = conn.transaction().unwrap();
        let second = delete_draft_in_tx(
            &tx3,
            &ledger_meta(),
            actor(),
            DeleteDraftInputs {
                tenant: "test-tenant".to_string(),
                drf_id: draft.drf_id.clone(),
                actor: "operator-A".to_string(),
            },
        )
        .unwrap();
        tx3.commit().unwrap();
        assert!(!second.deleted);
        assert_eq!(second.dispatch_pointers_cleared, 0);
    }

    /// If the audit-emit step fails, the entire tx MUST roll back:
    /// the draft row remains AND the dispatch pointer is restored.
    /// Closes the "torn-write" failure mode named in the S239 brief's
    /// `delete_draft_rollback_on_audit_failure` expectation.
    ///
    /// We force the audit append to fail by DROPping the
    /// `audit_ledger` table after schema setup — the next
    /// `append_in_tx` call's `read_head` SELECT hits a "table does
    /// not exist" error. The DROP happens outside the test tx so
    /// it is visible to the test tx's read.
    #[test]
    fn delete_draft_rollback_on_audit_failure() {
        let mut conn = open_conn();
        ensure_minimal_dispatch_schema(&conn);

        let tx = conn.transaction().unwrap();
        let draft = create_draft_in_tx(
            &tx,
            &ledger_meta(),
            actor(),
            CreateDraftInputs {
                tenant: "test-tenant".to_string(),
                partner_id: "ptr_X".to_string(),
                source_dispatch_id: Some("dsp_X".to_string()),
                source_wo_id: Some("wo_X".to_string()),
                source_quote_id: None,
                product_id: "prd_X".to_string(),
                qty: Decimal::ONE,
                notes: None,
                actor: "a".to_string(),
                idempotency_key: "idem-create".to_string(),
            },
        )
        .unwrap();
        tx.commit().unwrap();
        insert_dispatch_pointing_at(&conn, "dsp_X", &draft.drf_id);

        // Force audit-emit failure — drop the audit_ledger table so
        // the next append_in_tx hits a missing-table error mid-cascade.
        conn.execute_batch("DROP TABLE audit_ledger;")
            .expect("drop audit_ledger");

        // Now the real delete_draft_in_tx must fail at audit append
        // AND the tx must roll back — row + pointer survive.
        let tx2 = conn.transaction().unwrap();
        let err = delete_draft_in_tx(
            &tx2,
            &ledger_meta(),
            actor(),
            DeleteDraftInputs {
                tenant: "test-tenant".to_string(),
                drf_id: draft.drf_id.clone(),
                actor: "operator-A".to_string(),
            },
        );
        assert!(
            err.is_err(),
            "audit-table-gone must surface as Err (got {err:?})"
        );
        // Caller drops the tx without committing — DuckDB rolls back
        // automatically on Drop.
        drop(tx2);

        // Draft row STILL present (rollback worked).
        assert!(read_draft(&conn, "test-tenant", &draft.drf_id)
            .unwrap()
            .is_some());
        // Dispatch pointer STILL pointing at the draft (rollback
        // worked).
        assert_eq!(
            read_dispatch_pointer(&conn, "dsp_X"),
            Some(draft.drf_id.clone())
        );
    }
}
