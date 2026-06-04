//! Dispatch-board repository — the load-bearing surface per ADR-0064 §3.
//!
//! Two write paths:
//!
//! 1. [`create_dispatch`] — operator (or future adapter) creates a
//!    Drafted dispatch row against a Completed WO. Enforces the
//!    ADR-0064 §2 eligibility gate (WO state = Completed AND no prior
//!    dispatch row) in the same transaction. Emits one
//!    `DispatchCreated` audit entry.
//!
//! 2. [`mark_shipped`] — operator (or future adapter) flips a Drafted
//!    dispatch to Shipped. Per ADR-0064 §4 + §5 + §"Invariants pinned"
//!    #1 this writes — IN ONE TRANSACTION — the state flip, exactly
//!    one `Dispatch` `stock_movements` row (via
//!    [`aberp_inventory::record_movement`]), the spawned invoice id
//!    (via the injected [`InvoiceSpawner`] — see §"Invoice spawner
//!    injection" below), and one `DispatchShipped` audit entry. A
//!    failure of any sub-call rolls back ALL of them.
//!
//! [`cancel_dispatch`] is a third write path, but cancellation has no
//! side-effects (no inventory, no invoice spawn, no dedicated audit
//! kind per ADR-0064 §6). Implemented inline in this module for
//! symmetry; the route layer surfaces it as `POST /dispatches/:id/cancel`.
//!
//! ## Invoice spawner injection
//!
//! ADR-0064 §5 demands the Stage 1 invoice draft be created in the
//! SAME transaction as the dispatch state flip + stock movement.
//! The existing Stage 1 issuance pipeline (`apps/aberp/src/issue_invoice.rs`)
//! is async, opens its own transaction, and atomically allocates a
//! gap-free sequence number — bringing it in-tx would require a major
//! refactor that is OUT OF SCOPE for PR-230.
//!
//! The pragmatic v1 cut per [[pushback-as-method]] (flagged in the
//! PR-230 body as an open ADR-0064 revision):
//!
//! - This crate defines the [`InvoiceSpawner`] trait whose
//!   `spawn(&self, &Transaction, &Dispatch, &str /* idempotency_key */)
//!   -> Result<Option<String>, anyhow::Error>` method runs INSIDE the
//!   mark_shipped tx.
//! - PR-230 ships [`NoopInvoiceSpawner`] which returns `Ok(None)` —
//!   the v1 production wiring. The SPA's dispatch detail surfaces a
//!   "Create invoice draft from this dispatch" click-through that
//!   pre-fills the existing IssueInvoice form with the dispatch's
//!   partner + the WO's BOM finished good × qty. Operator clicks
//!   Issue → existing gap-free pipeline runs → invoice draft created.
//!   `dispatches.spawned_invoice_id` is updated post-issue via the
//!   existing audit-ledger walker (S189-style read-time derivation).
//! - PR-230b (named-deferred in the open questions) will land the
//!   sync billing extraction + the real `InvoiceSpawner` impl.
//! - Tests in `tests/dispatch_round_trip.rs` swap in a
//!   [`MockInvoiceSpawner`] (test-support gated) that can be
//!   configured to return `Ok(Some(id))` (pin invariants #4 #5) or
//!   `Err(_)` (pin invariant #6 — failed spawn rolls back the entire
//!   mark_shipped tx).
//!
//! Same invariants hold for both spawner impls: the trait method runs
//! inside the supplied tx, so a returned `Err(_)` propagates out of
//! `mark_shipped` and the caller's `tx.commit()` is never reached —
//! every write rolls back.

use anyhow::{anyhow, Context};
use duckdb::{params, Connection, Transaction};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use ulid::Ulid;

use aberp_audit_ledger::{append_in_tx, Actor, EventKind, LedgerMeta};
use aberp_inventory::{
    record_movement, ActorKind, MovementReason, MovementRefKind, RecordMovementContext,
    RecordMovementInputs,
};

use crate::audit::{DispatchCreatedPayload, DispatchShippedPayload};
use crate::error::DispatchError;
use crate::state::{next_dispatch_state, DispatchAction};
use crate::types::{CarrierKind, DispatchState};

// ── DoS bound ──────────────────────────────────────────────────────

/// Per-request maximum result rows the list endpoint returns. Mirrors
/// `aberp_qa`'s `QA_LIST_MAX_LIMIT` posture and pins the
/// [[trust-code-not-operator]] discipline at this surface.
pub const MAX_DISPATCH_LIST_LIMIT: u32 = 500;

/// Per-request maximum eligible-WO rows the eligibility endpoint
/// returns. Same posture.
pub const MAX_ELIGIBLE_WO_LIMIT: u32 = 500;

// ── Schema ─────────────────────────────────────────────────────────

/// Apply `V001__dispatch.sql`. Idempotent — calling against an already-
/// migrated tenant DB is a no-op.
pub fn ensure_schema(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(include_str!("../migrations/V001__dispatch.sql"))
        .context("ensure dispatch schema")
}

// ── Row shape ──────────────────────────────────────────────────────

/// One row from `dispatches`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Dispatch {
    /// `dsp_<ULID>`.
    pub dsp_id: String,
    pub wo_id: String,
    pub partner_id: String,
    pub state: DispatchState,
    pub created_at: String,
    pub shipped_at: Option<String>,
    pub cancelled_at: Option<String>,
    pub carrier_kind: Option<CarrierKind>,
    pub tracking_number: Option<String>,
    pub spawned_invoice_id: Option<String>,
    pub notes: Option<String>,
}

/// One row from the eligible-WO read view per ADR-0064 §2. The view
/// itself is a SELECT not a separate table per the brief's "Source
/// eligibility (read-only view, no separate table)" instruction.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EligibleWorkOrder {
    pub wo_id: String,
    pub wo_number: String,
    pub product_id: String,
    pub qty_target: String, // Decimal-as-string for the wire form
    pub completed_at: String,
}

// ── Context ────────────────────────────────────────────────────────

/// Context for write paths. Mirrors
/// [`aberp_qa::QaWriteContext`] / [`aberp_work_orders::WoWriteContext`].
#[derive(Debug)]
pub struct DispatchWriteContext<'a> {
    pub tenant: &'a str,
    pub actor: ActorKind,
    pub ledger_meta: &'a LedgerMeta,
    pub ledger_actor: Actor,
}

// ── Invoice spawner trait ──────────────────────────────────────────

/// Stage 1 invoice-draft spawner injected into [`mark_shipped`] per
/// ADR-0064 §5 + the [[pushback-as-method]] divergence flagged in the
/// PR-230 body.
///
/// Implementations MUST run inside the supplied transaction so a
/// failed spawn rolls back the entire mark_shipped tx (invariant #6).
/// Returning `Ok(None)` is the v1 production posture (defer the spawn
/// to an operator-driven Issue click); returning `Ok(Some(invoice_id))`
/// records the spawn outcome on the dispatch row.
///
/// The trait method is sync; the existing async issuance pipeline
/// cannot be called from here. Implementations either:
///   - return `Ok(None)` (the v1 [`NoopInvoiceSpawner`] default);
///   - perform a sync `aberp_billing::allocate_in_tx` + invoice INSERT
///     directly inside the tx (the PR-230b extraction path);
///   - mock the return for tests (see [`MockInvoiceSpawner`]).
pub trait InvoiceSpawner {
    /// Spawn an invoice draft and return its id, or `None` to record
    /// that no draft was created. Errors propagate out of
    /// [`mark_shipped`] and roll back the supplied transaction.
    fn spawn(
        &self,
        tx: &Transaction<'_>,
        dispatch: &Dispatch,
        wo_product_id: &str,
        wo_qty_target: Decimal,
        idempotency_key: &str,
    ) -> anyhow::Result<Option<String>>;
}

/// No-op [`InvoiceSpawner`] — returns `Ok(None)`. PR-230 wired this
/// as the production default; PR-230b / S236 moved production to the
/// real `BillingInvoiceSpawner` in `apps/aberp/src/invoice_draft.rs`
/// (writes one `invoice_draft` row + one `InvoiceStaged` audit entry
/// per dispatch ship). The type is retained because the dispatch
/// crate's own round-trip tests use it for the "no-spawn-recorded"
/// arm of the trait (a downstream consumer wanting deferred-spawn
/// semantics can also still reach for it).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopInvoiceSpawner;

impl InvoiceSpawner for NoopInvoiceSpawner {
    fn spawn(
        &self,
        _tx: &Transaction<'_>,
        _dispatch: &Dispatch,
        _wo_product_id: &str,
        _wo_qty_target: Decimal,
        _idempotency_key: &str,
    ) -> anyhow::Result<Option<String>> {
        Ok(None)
    }
}

// ── Create dispatch ───────────────────────────────────────────────

/// Inputs to [`create_dispatch`].
#[derive(Debug, Clone)]
pub struct CreateDispatchInputs {
    pub wo_id: String,
    pub partner_id: String,
    pub notes: Option<String>,
    pub idempotency_key: String,
}

/// Create a Drafted dispatch row + emit one `DispatchCreated` audit
/// entry, all in the supplied transaction.
///
/// Enforces per ADR-0064 §2:
///   - WO exists in the tenant
///   - WO state = Completed
///   - No prior `dispatches` row for this WO (any state — Drafted,
///     Shipped, Cancelled all block; one-dispatch-per-WO is v1 scope
///     per ADR-0064 §"Out of scope")
///   - partner exists in the tenant (defence-in-depth so a typo'd
///     partner_id loud-fails at create time, not at ship time)
///   - idempotency_key non-empty (the F8 ledger pin is the actual
///     gate; this is the early-validate)
pub fn create_dispatch(
    tx: &Transaction<'_>,
    ctx: &DispatchWriteContext<'_>,
    inputs: CreateDispatchInputs,
) -> Result<Dispatch, DispatchError> {
    if inputs.idempotency_key.trim().is_empty() {
        return Err(DispatchError::Validation(
            "idempotency_key must be non-empty".to_string(),
        ));
    }
    if inputs.wo_id.trim().is_empty() {
        return Err(DispatchError::Validation(
            "wo_id must be non-empty".to_string(),
        ));
    }
    if inputs.partner_id.trim().is_empty() {
        return Err(DispatchError::Validation(
            "partner_id must be non-empty".to_string(),
        ));
    }

    // 1. WO must exist + be Completed.
    let wo_state: Option<String> = tx
        .query_row(
            "SELECT state FROM work_orders
             WHERE tenant_id = ? AND wo_id = ? LIMIT 1;",
            params![ctx.tenant, &inputs.wo_id],
            |row| row.get::<_, String>(0),
        )
        .ok();
    let wo_state =
        wo_state.ok_or_else(|| DispatchError::WorkOrderNotFound(inputs.wo_id.clone()))?;
    if wo_state != "completed" {
        return Err(DispatchError::WorkOrderNotEligible {
            wo_id: inputs.wo_id.clone(),
            state: wo_state,
        });
    }

    // 2. No prior dispatch row for this WO.
    let prior_dsp_id: Option<String> = tx
        .query_row(
            "SELECT dsp_id FROM dispatches
             WHERE tenant_id = ? AND wo_id = ? LIMIT 1;",
            params![ctx.tenant, &inputs.wo_id],
            |row| row.get::<_, String>(0),
        )
        .ok();
    if let Some(dsp_id) = prior_dsp_id {
        return Err(DispatchError::WorkOrderAlreadyDispatched {
            wo_id: inputs.wo_id.clone(),
            dsp_id,
        });
    }

    // 3. Partner must exist.
    let partner_exists: bool = tx
        .query_row(
            "SELECT 1 FROM partners
             WHERE tenant_id = ? AND id = ? AND deleted_at IS NULL
             LIMIT 1;",
            params![ctx.tenant, &inputs.partner_id],
            |row| row.get::<_, i64>(0),
        )
        .map(|_| true)
        .unwrap_or(false);
    if !partner_exists {
        return Err(DispatchError::PartnerNotFound(inputs.partner_id.clone()));
    }

    let now = now_rfc3339()?;
    let dsp_id = format!("dsp_{}", Ulid::new());

    // 4. INSERT the Drafted row.
    tx.execute(
        "INSERT INTO dispatches (
            dsp_id, tenant_id, wo_id, partner_id, state,
            created_at, shipped_at, cancelled_at,
            carrier_kind, tracking_number, spawned_invoice_id, notes
         ) VALUES (?, ?, ?, ?, ?, ?, NULL, NULL, NULL, NULL, NULL, ?);",
        params![
            &dsp_id,
            ctx.tenant,
            &inputs.wo_id,
            &inputs.partner_id,
            DispatchState::Drafted.as_str(),
            &now,
            inputs.notes.as_deref(),
        ],
    )
    .map_err(|e| DispatchError::Storage(anyhow!("INSERT dispatches: {e}")))?;

    // 5. Audit-ledger entry.
    let actor_str = ctx.actor.as_operator_string();
    let payload = DispatchCreatedPayload {
        dsp_id: dsp_id.clone(),
        wo_id: inputs.wo_id.clone(),
        partner_id: inputs.partner_id.clone(),
        actor: actor_str.clone(),
        idempotency_key: inputs.idempotency_key.clone(),
    };
    append_in_tx(
        tx,
        ctx.ledger_meta,
        EventKind::DispatchCreated,
        payload.to_bytes(),
        ctx.ledger_actor.clone(),
        Some(inputs.idempotency_key.clone()),
    )
    .map_err(|e| DispatchError::Storage(anyhow!("audit append DispatchCreated: {e}")))?;

    Ok(Dispatch {
        dsp_id,
        wo_id: inputs.wo_id,
        partner_id: inputs.partner_id,
        state: DispatchState::Drafted,
        created_at: now,
        shipped_at: None,
        cancelled_at: None,
        carrier_kind: None,
        tracking_number: None,
        spawned_invoice_id: None,
        notes: inputs.notes,
    })
}

// ── Mark shipped ──────────────────────────────────────────────────

/// Inputs to [`mark_shipped`].
#[derive(Debug, Clone)]
pub struct MarkShippedInputs {
    pub carrier_kind: CarrierKind,
    /// Operator-typed tracking number; `None` is valid for SelfDelivery /
    /// CustomerPickup. The handler does not enforce a "tracking required
    /// for postal carriers" rule in v1 — operators occasionally ship
    /// without a number (truckload pickup, courier-of-record etc.);
    /// the audit payload faithfully records `None` instead of
    /// fabricating one.
    pub tracking_number: Option<String>,
    /// RFC3339 timestamp; `None` → server stamps `now()`.
    pub shipped_at: Option<String>,
    pub idempotency_key: String,
}

/// Outcome of [`mark_shipped`].
#[derive(Debug, Clone)]
pub struct MarkShippedOutcome {
    /// The LIVE dispatch row after the flip (state = Shipped,
    /// carrier_kind + tracking_number + shipped_at + spawned_invoice_id
    /// all populated).
    pub dispatch: Dispatch,
    /// `Some(invoice_id)` when the injected [`InvoiceSpawner`]
    /// returned a draft id; `None` when the spawner deferred (the v1
    /// production [`NoopInvoiceSpawner`] posture).
    pub spawned_invoice_id: Option<String>,
    /// `mvt_<ULID>` of the `Dispatch` stock_movement that was written
    /// in the same tx. Surfaced for the route layer's success body so
    /// the SPA can render "Stock decremented by N — view ledger entry".
    pub stock_movement_id: String,
}

/// Flip a Drafted dispatch to Shipped per ADR-0064 §4 + §5 +
/// §"Invariants pinned" #1. All side-effects ride the supplied
/// transaction:
///
/// 1. Read the dispatch row + the WO (for product_id + qty_target).
/// 2. Refuse if the dispatch is already Shipped (idempotency per
///    invariant #2) — return the existing row unchanged.
/// 3. Refuse if the dispatch is Cancelled (invariant #2 by
///    extension — Cancelled is terminal).
/// 4. Emit one `Dispatch` stock_movement (qty_delta = -wo.qty_target,
///    reason = Dispatch, ref_kind = Dispatch, ref_id = dsp_id) via
///    [`aberp_inventory::record_movement`].
/// 5. Call the injected [`InvoiceSpawner`] (may return `None` per the
///    v1 noop posture). Failure → propagates as
///    [`DispatchError::InvoiceSpawnFailed`] → rolls back the entire tx.
/// 6. UPDATE dispatches SET state=Shipped, shipped_at, carrier_kind,
///    tracking_number, spawned_invoice_id.
/// 7. Append one `DispatchShipped` audit entry.
///
/// Per ADR-0064 §5 ("Failure handling") + invariant #6: if any of
/// steps 4–7 returns `Err(_)`, the caller's `tx.commit()` is never
/// reached and the entire transaction rolls back — no state flip, no
/// stock movement, no spawned invoice, no audit entry.
pub fn mark_shipped(
    tx: &Transaction<'_>,
    ctx: &DispatchWriteContext<'_>,
    dsp_id: &str,
    inputs: MarkShippedInputs,
    spawner: &dyn InvoiceSpawner,
) -> Result<MarkShippedOutcome, DispatchError> {
    if inputs.idempotency_key.trim().is_empty() {
        return Err(DispatchError::Validation(
            "idempotency_key must be non-empty".to_string(),
        ));
    }

    // 1. Read the dispatch row inside the tx for state + wo_id.
    let prior = read_dispatch_in_tx(tx, ctx.tenant, dsp_id)?
        .ok_or_else(|| DispatchError::DispatchNotFound(dsp_id.to_string()))?;

    // 2. Idempotency on already-Shipped per ADR-0064 invariant #2 —
    //    return the existing row unchanged. The caller can detect via
    //    the unchanged `spawned_invoice_id` + `stock_movement_id =
    //    "<idempotent-noop>"` sentinel; the route layer treats the
    //    pre-existing row as a 200 OK (the operator's second click
    //    against a stale UI succeeds silently).
    if prior.state == DispatchState::Shipped {
        return Ok(MarkShippedOutcome {
            dispatch: prior,
            spawned_invoice_id: None,
            stock_movement_id: "<idempotent-noop>".to_string(),
        });
    }

    // 3. Refuse non-Drafted edges (Cancelled → Shipped is illegal).
    let _new_state = next_dispatch_state(prior.state, DispatchAction::Ship)?;

    // 4. Look up the WO's product_id + qty_target for the stock
    //    movement. The dispatch row alone doesn't carry this — we
    //    deliberately don't denormalise (the WO is the source of
    //    truth per ADR-0061 §6).
    let (wo_product_id, wo_qty_target_str): (String, String) = tx
        .query_row(
            "SELECT product_id, CAST(qty_target AS VARCHAR)
             FROM work_orders WHERE tenant_id = ? AND wo_id = ? LIMIT 1;",
            params![ctx.tenant, &prior.wo_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .map_err(|e| DispatchError::Storage(anyhow!("SELECT work_orders for ship: {e}")))?;
    let wo_qty_target = Decimal::from_str(&wo_qty_target_str).map_err(|e| {
        DispatchError::Storage(anyhow!("parse qty_target {wo_qty_target_str:?}: {e}"))
    })?;

    let now = inputs
        .shipped_at
        .clone()
        .map(Ok)
        .unwrap_or_else(now_rfc3339)?;

    // 5. Emit the Dispatch stock_movement (qty_delta = -wo.qty_target).
    let movement_ctx = RecordMovementContext {
        tenant: ctx.tenant,
        actor: ctx.actor.clone(),
        ledger_meta: ctx.ledger_meta,
        ledger_actor: ctx.ledger_actor.clone(),
    };
    let movement_inputs = RecordMovementInputs {
        product_id: wo_product_id.clone(),
        qty_delta: -wo_qty_target,
        reason: MovementReason::Dispatch,
        ref_kind: MovementRefKind::Dispatch,
        ref_id: Some(prior.dsp_id.clone()),
        notes: inputs.tracking_number.clone(),
        idempotency_key: format!("{}:dispatch_movement", inputs.idempotency_key),
    };
    let movement = record_movement(tx, &movement_ctx, movement_inputs).map_err(|e| match e {
        aberp_inventory::InventoryError::DuplicateIdempotencyKey(k) => {
            DispatchError::DuplicateIdempotencyKey(k)
        }
        aberp_inventory::InventoryError::ProductNotFound(p) => {
            DispatchError::Validation(format!("Ship: product {p} not found"))
        }
        aberp_inventory::InventoryError::WrongSignForReason {
            reason,
            required,
            got,
        } => DispatchError::Validation(format!(
            "Ship sign-violation: reason {reason} requires {required:?}, got {got}"
        )),
        aberp_inventory::InventoryError::Storage(err) => DispatchError::Storage(err),
    })?;

    // 6. Call the injected invoice spawner. Failure rolls back the
    //    whole tx per ADR-0064 invariant #6.
    let spawn_idempotency_key = format!("{}:spawn_invoice", inputs.idempotency_key);
    let spawned_invoice_id = spawner
        .spawn(
            tx,
            &prior,
            &wo_product_id,
            wo_qty_target,
            &spawn_idempotency_key,
        )
        .map_err(|e| DispatchError::InvoiceSpawnFailed(format!("{e:#}")))?;

    // 7. UPDATE the dispatch row.
    tx.execute(
        "UPDATE dispatches SET
            state = ?,
            shipped_at = ?,
            carrier_kind = ?,
            tracking_number = ?,
            spawned_invoice_id = ?
         WHERE tenant_id = ? AND dsp_id = ?;",
        params![
            DispatchState::Shipped.as_str(),
            &now,
            inputs.carrier_kind.as_str(),
            inputs.tracking_number.as_deref(),
            spawned_invoice_id.as_deref(),
            ctx.tenant,
            dsp_id,
        ],
    )
    .map_err(|e| DispatchError::Storage(anyhow!("UPDATE dispatches SET state=Shipped: {e}")))?;

    // 8. Audit-ledger entry.
    let actor_str = ctx.actor.as_operator_string();
    let payload = DispatchShippedPayload {
        dsp_id: prior.dsp_id.clone(),
        wo_id: prior.wo_id.clone(),
        partner_id: prior.partner_id.clone(),
        carrier_kind: inputs.carrier_kind,
        tracking_number: inputs.tracking_number.clone(),
        shipped_at: now.clone(),
        spawned_invoice_id: spawned_invoice_id.clone(),
        actor: actor_str,
        idempotency_key: inputs.idempotency_key.clone(),
    };
    append_in_tx(
        tx,
        ctx.ledger_meta,
        EventKind::DispatchShipped,
        payload.to_bytes(),
        ctx.ledger_actor.clone(),
        Some(format!("ship:{}", inputs.idempotency_key)),
    )
    .map_err(|e| DispatchError::Storage(anyhow!("audit append DispatchShipped: {e}")))?;

    // 9. Read back the live row.
    let live = read_dispatch_in_tx(tx, ctx.tenant, dsp_id)?.ok_or_else(|| {
        DispatchError::Storage(anyhow!("mark_shipped: live row vanished after write"))
    })?;

    Ok(MarkShippedOutcome {
        dispatch: live,
        spawned_invoice_id,
        stock_movement_id: movement.movement_id,
    })
}

// ── Cancel dispatch ───────────────────────────────────────────────

/// Cancel a Drafted dispatch — no inventory impact, no invoice spawn,
/// no audit kind (ADR-0064 §6: "A Cancelled dispatch does NOT get a
/// dedicated EventKind in v1"). Refuses non-Drafted (Shipped is
/// terminal-success; Cancelled is already-cancelled).
///
/// Returns the cancelled row.
pub fn cancel_dispatch(
    tx: &Transaction<'_>,
    ctx: &DispatchWriteContext<'_>,
    dsp_id: &str,
) -> Result<Dispatch, DispatchError> {
    let prior = read_dispatch_in_tx(tx, ctx.tenant, dsp_id)?
        .ok_or_else(|| DispatchError::DispatchNotFound(dsp_id.to_string()))?;

    // Refuse non-Drafted per ADR-0064 §1 table.
    let _new_state = next_dispatch_state(prior.state, DispatchAction::Cancel)?;

    let now = now_rfc3339()?;
    tx.execute(
        "UPDATE dispatches SET
            state = ?,
            cancelled_at = ?
         WHERE tenant_id = ? AND dsp_id = ?;",
        params![DispatchState::Cancelled.as_str(), &now, ctx.tenant, dsp_id,],
    )
    .map_err(|e| DispatchError::Storage(anyhow!("UPDATE dispatches SET state=Cancelled: {e}")))?;

    let live = read_dispatch_in_tx(tx, ctx.tenant, dsp_id)?.ok_or_else(|| {
        DispatchError::Storage(anyhow!("cancel_dispatch: live row vanished after write"))
    })?;
    Ok(live)
}

// ── Spawned-invoice-id cleanup ─────────────────────────────────────

/// S239 / PR-233 — NULL the `spawned_invoice_id` column on every
/// dispatch row that currently points at the given `drf_<ULID>`.
///
/// Called from the invoice-draft delete path inside the same
/// transaction as the `DELETE FROM invoice_draft` so the orphan
/// pointer S237 §🔴 #1 named cannot exist by construction: either
/// both writes commit (draft gone, dispatch pointer NULL) or both
/// roll back (draft + pointer unchanged).
///
/// Returns the number of dispatch rows that had their pointer
/// cleared (`0` when the deleted draft was never pointed at — i.e.,
/// a standalone draft with no source dispatch, or a draft whose
/// dispatch was already cancelled / had its pointer manually
/// detached). The caller treats the count as advisory; the
/// invariant is "no dispatch row points at `drf_id` after this
/// call returns Ok," and that holds for any non-Err outcome
/// regardless of the count.
///
/// Tenant-scoped so a cross-tenant `drf_<ULID>` collision (which
/// the ULID space rules out by construction, but [[trust-code-not-operator]]
/// belt-and-braces) could not affect a sibling tenant's dispatch
/// rows.
pub fn null_spawned_invoice_id_in_tx(
    tx: &Transaction<'_>,
    tenant: &str,
    drf_id: &str,
) -> anyhow::Result<usize> {
    tx.execute(
        "UPDATE dispatches SET spawned_invoice_id = NULL
         WHERE tenant_id = ? AND spawned_invoice_id = ?;",
        params![tenant, drf_id],
    )
    .context("UPDATE dispatches SET spawned_invoice_id = NULL")
}

// ── Reads ──────────────────────────────────────────────────────────

/// List dispatches in the tenant, optionally filtering by state.
/// Ordered by `created_at DESC, dsp_id DESC` (newest first) per
/// ADR-0064 §7 default sort.
pub fn list_dispatches(
    conn: &Connection,
    tenant: &str,
    state_filter: Option<DispatchState>,
    limit: u32,
    offset: u32,
) -> anyhow::Result<Vec<Dispatch>> {
    let limit = limit.min(MAX_DISPATCH_LIST_LIMIT);
    let mut sql = String::from(
        "SELECT dsp_id, wo_id, partner_id, state, created_at, shipped_at, cancelled_at,
                carrier_kind, tracking_number, spawned_invoice_id, notes
         FROM dispatches WHERE tenant_id = ?",
    );
    if state_filter.is_some() {
        sql.push_str(" AND state = ?");
    }
    sql.push_str(" ORDER BY created_at DESC, dsp_id DESC LIMIT ? OFFSET ?;");

    let mut stmt = conn.prepare(&sql)?;
    let out: Vec<Dispatch> = match state_filter {
        Some(s) => {
            let rows =
                stmt.query_map(params![tenant, s.as_str(), limit, offset], row_to_dispatch)?;
            let mut acc = Vec::new();
            for r in rows {
                acc.push(r??);
            }
            acc
        }
        None => {
            let rows = stmt.query_map(params![tenant, limit, offset], row_to_dispatch)?;
            let mut acc = Vec::new();
            for r in rows {
                acc.push(r??);
            }
            acc
        }
    };
    Ok(out)
}

/// Read a single dispatch row by id, scoped to the tenant. `None` for
/// unknown ids.
pub fn get_dispatch(
    conn: &Connection,
    tenant: &str,
    dsp_id: &str,
) -> anyhow::Result<Option<Dispatch>> {
    conn.query_row(
        "SELECT dsp_id, wo_id, partner_id, state, created_at, shipped_at, cancelled_at,
                carrier_kind, tracking_number, spawned_invoice_id, notes
         FROM dispatches WHERE tenant_id = ? AND dsp_id = ? LIMIT 1;",
        params![tenant, dsp_id],
        |row| Ok(parse_dispatch_row(row)),
    )
    .map(Some)
    .or_else(|e| match e {
        duckdb::Error::QueryReturnedNoRows => Ok(None),
        other => Err(anyhow!("SELECT dispatches by id: {other}")),
    })?
    .transpose()
}

/// List WOs eligible for dispatch per ADR-0064 §2 + the brief's
/// "Source eligibility (read-only view, no separate table)"
/// instruction. WO is eligible IFF:
///   - state = 'completed' (the WO-Complete handler already gates on
///     all-QA-passed per ADR-0063, so this single column is the
///     canonical signal per ADR-0064 §2 rationale)
///   - no `dispatches` row exists for the WO (any state)
///
/// Ordered by `completed_at ASC, wo_id ASC` (oldest first) so the SPA
/// surfaces the longest-waiting WOs at the top.
pub fn list_eligible_work_orders(
    conn: &Connection,
    tenant: &str,
    limit: u32,
) -> anyhow::Result<Vec<EligibleWorkOrder>> {
    let limit = limit.min(MAX_ELIGIBLE_WO_LIMIT);
    let mut stmt = conn.prepare(
        "SELECT wo.wo_id, wo.wo_number, wo.product_id,
                CAST(wo.qty_target AS VARCHAR), wo.completed_at
         FROM work_orders wo
         WHERE wo.tenant_id = ?
           AND wo.state = 'completed'
           AND NOT EXISTS (
                SELECT 1 FROM dispatches d
                WHERE d.tenant_id = wo.tenant_id AND d.wo_id = wo.wo_id
           )
         ORDER BY wo.completed_at ASC, wo.wo_id ASC
         LIMIT ?;",
    )?;
    let rows = stmt.query_map(params![tenant, limit], |row| {
        Ok(EligibleWorkOrder {
            wo_id: row.get(0)?,
            wo_number: row.get(1)?,
            product_id: row.get(2)?,
            qty_target: row.get(3)?,
            completed_at: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Counts of dispatches by state for the tenant. Returns a fully-
/// populated [`DispatchStateCounts`] (every state always present,
/// zero-defaulted). Powers the operator dashboard tile (PR-231 / S235).
pub fn count_dispatches_by_state(
    conn: &Connection,
    tenant: &str,
) -> anyhow::Result<DispatchStateCounts> {
    let mut stmt =
        conn.prepare("SELECT state, COUNT(*) FROM dispatches WHERE tenant_id = ? GROUP BY state;")?;
    let rows = stmt.query_map(params![tenant], |row| {
        let state_str: String = row.get(0)?;
        let count: i64 = row.get(1)?;
        Ok((state_str, count))
    })?;
    let mut counts = DispatchStateCounts::default();
    for r in rows {
        let (state_str, count) = r?;
        let state = DispatchState::from_storage_str(&state_str)
            .map_err(|e| anyhow!("{e}: {state_str:?}"))?;
        let bucket = match state {
            DispatchState::Drafted => &mut counts.drafted,
            DispatchState::Shipped => &mut counts.shipped,
            DispatchState::Cancelled => &mut counts.cancelled,
        };
        *bucket = u32::try_from(count.max(0)).unwrap_or(u32::MAX);
    }
    Ok(counts)
}

/// Count of WOs eligible for dispatch (same predicate as
/// [`list_eligible_work_orders`] but COUNT-only). The dashboard tile
/// only needs the headline number; the full list lives behind a
/// click-through to the Dispatch board.
pub fn count_eligible_work_orders(conn: &Connection, tenant: &str) -> anyhow::Result<u32> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*)
         FROM work_orders wo
         WHERE wo.tenant_id = ?
           AND wo.state = 'completed'
           AND NOT EXISTS (
                SELECT 1 FROM dispatches d
                WHERE d.tenant_id = wo.tenant_id AND d.wo_id = wo.wo_id
           );",
        params![tenant],
        |row| row.get(0),
    )?;
    Ok(u32::try_from(count.max(0)).unwrap_or(u32::MAX))
}

/// Count of dispatches shipped (transitioned into `Shipped`) on a
/// given local-calendar date. `today_iso` is the `YYYY-MM-DD` date
/// string the caller has already resolved against its local TZ — we
/// match against the `shipped_at` column's leading 10 chars.
pub fn count_dispatches_shipped_today(
    conn: &Connection,
    tenant: &str,
    today_iso: &str,
) -> anyhow::Result<u32> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*)
         FROM dispatches
         WHERE tenant_id = ?
           AND state = 'shipped'
           AND shipped_at IS NOT NULL
           AND substr(shipped_at, 1, 10) = ?;",
        params![tenant, today_iso],
        |row| row.get(0),
    )?;
    Ok(u32::try_from(count.max(0)).unwrap_or(u32::MAX))
}

/// Tenant-scoped count of dispatches grouped by [`DispatchState`]. All
/// three fields are always present (zero-defaulted).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchStateCounts {
    pub drafted: u32,
    pub shipped: u32,
    pub cancelled: u32,
}

// ── Internals ──────────────────────────────────────────────────────

fn read_dispatch_in_tx(
    tx: &Transaction<'_>,
    tenant: &str,
    dsp_id: &str,
) -> Result<Option<Dispatch>, DispatchError> {
    tx.query_row(
        "SELECT dsp_id, wo_id, partner_id, state, created_at, shipped_at, cancelled_at,
                carrier_kind, tracking_number, spawned_invoice_id, notes
         FROM dispatches WHERE tenant_id = ? AND dsp_id = ? LIMIT 1;",
        params![tenant, dsp_id],
        |row| Ok(parse_dispatch_row(row)),
    )
    .map(Some)
    .or_else(|e| match e {
        duckdb::Error::QueryReturnedNoRows => Ok(None),
        other => Err(DispatchError::Storage(anyhow!(
            "SELECT dispatches in-tx: {other}"
        ))),
    })?
    .transpose()
    .map_err(|e| DispatchError::Storage(anyhow!("parse dispatches row: {e}")))
}

#[allow(clippy::type_complexity)]
fn row_to_dispatch(row: &duckdb::Row<'_>) -> duckdb::Result<anyhow::Result<Dispatch>> {
    Ok(parse_dispatch_row(row))
}

fn parse_dispatch_row(row: &duckdb::Row<'_>) -> anyhow::Result<Dispatch> {
    let dsp_id: String = row.get(0).context("get dsp_id")?;
    let wo_id: String = row.get(1).context("get wo_id")?;
    let partner_id: String = row.get(2).context("get partner_id")?;
    let state_str: String = row.get(3).context("get state")?;
    let created_at: String = row.get(4).context("get created_at")?;
    let shipped_at: Option<String> = row.get(5).context("get shipped_at")?;
    let cancelled_at: Option<String> = row.get(6).context("get cancelled_at")?;
    let carrier_kind_str: Option<String> = row.get(7).context("get carrier_kind")?;
    let tracking_number: Option<String> = row.get(8).context("get tracking_number")?;
    let spawned_invoice_id: Option<String> = row.get(9).context("get spawned_invoice_id")?;
    let notes: Option<String> = row.get(10).context("get notes")?;

    let state =
        DispatchState::from_storage_str(&state_str).map_err(|e| anyhow!("{e}: {state_str:?}"))?;
    let carrier_kind = match carrier_kind_str {
        Some(s) => Some(CarrierKind::from_storage_str(&s).map_err(|e| anyhow!("{e}: {s:?}"))?),
        None => None,
    };

    Ok(Dispatch {
        dsp_id,
        wo_id,
        partner_id,
        state,
        created_at,
        shipped_at,
        cancelled_at,
        carrier_kind,
        tracking_number,
        spawned_invoice_id,
        notes,
    })
}

fn now_rfc3339() -> Result<String, DispatchError> {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|e| DispatchError::Storage(anyhow!("format Rfc3339: {e}")))
}
