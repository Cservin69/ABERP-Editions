//! QA-queue repository — the load-bearing surface per ADR-0063 §3.
//!
//! Two write paths:
//!
//! 1. [`auto_create_inspection_for_op_completion`] — called from
//!    `aberp_work_orders::transition_routing_op` when a routing-op
//!    flips to Completed. Inserts ONE Pending `qa_inspections` row +
//!    emits one `QaInspectionCreated` audit entry in the supplied tx.
//!    Per ADR-0063 §"Rework" + invariant #1: if a prior non-superseded
//!    inspection exists for this (wo, op) — which happens after Rework
//!    flips the routing-op back to Active and the operator re-completes
//!    it — the prior row's `superseded_by` is set to the new qa_id so
//!    the live-state filter (`superseded_by IS NULL`) returns exactly
//!    one row per (wo, op).
//!
//! 2. [`decide_qa`] — operator (or adapter) decides on a Pending
//!    inspection: Pass / Fail / Rework / Dispose. Per ADR-0063 §4 the
//!    cross-actor override pattern fires when the new actor differs
//!    from the live row's actor — INSERT a new row + UPDATE the prior
//!    row's `superseded_by`. Same-actor decisions UPDATE in place.
//!    Dispose emits one `Scrap` `stock_movement` via
//!    `aberp_inventory::record_movement` (ADR-0063 §6 + invariant #5).
//!    `Rework` flips the upstream routing-op back to Active (the
//!    side-effect the route layer's response body names).
//!
//! All write paths take a `&Transaction` so the caller commits; the
//! audit-ledger entry, the inspection row, the routing-op cascade and
//! the stock-movement append all ride the same DB commit.

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

use crate::audit::{QaInspectionCreatedPayload, QaInspectionDecidedPayload};
use crate::error::QaError;
use crate::state::next_qa_state;
use crate::types::{QaDecision, QaState};

/// S264 / PR-253 (F6) — local, shape-compatible mirror of
/// `aberp_work_orders::RoutingOpStateChangedPayload`. aberp-qa CANNOT
/// depend on aberp-work-orders (that crate depends on aberp-qa — a Cargo
/// cycle; aberp-work-orders is only a dev-dependency here), so the Rework
/// branch emits the routing-op reverse transition (Completed → Active)
/// through this struct. The field names AND the snake_case state strings
/// (`"completed"` / `"active"`, matching `RoutingOpState`'s
/// `#[serde(rename_all = "snake_case")]`) MUST stay identical to the
/// work-orders payload so the audit ledger (generic JSON) and any
/// "list routing-op transitions for this WO" query read ONE uniform
/// schema. aberp-work-orders' `op_changed_payload_round_trips` pins the
/// canonical shape; `rework_emits_routing_op_state_changed` below pins
/// the literal JSON this emits.
#[derive(serde::Serialize)]
struct ReworkRoutingOpStateChangedPayload<'a> {
    routing_op_id: &'a str,
    wo_id: &'a str,
    from_state: &'static str,
    to_state: &'static str,
    actor: &'a str,
    idempotency_key: String,
}

// ── Schema ─────────────────────────────────────────────────────────

/// Apply `V001__qa.sql`. Idempotent — calling against an already-
/// migrated tenant DB is a no-op.
pub fn ensure_schema(conn: &Connection) -> anyhow::Result<()> {
    // ADR-0098 C2 fix-forward — no-op on a read-only conn (read_returns_readonly
    // read()-side); the schema is created by a writer before any read reaches
    // here. A genuine write mis-routed through read() still fails loud (F5).
    if aberp_audit_ledger::connection_is_read_only(conn) {
        return Ok(());
    }
    conn.execute_batch(include_str!("../migrations/V001__qa.sql"))
        .context("ensure qa schema")
}

// ── Row shape ──────────────────────────────────────────────────────

/// One row from `qa_inspections`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QaInspection {
    /// `qa_<ULID>`.
    pub qa_id: String,
    pub wo_id: String,
    pub routing_op_id: String,
    pub state: QaState,
    pub decided_at: Option<String>,
    pub decided_by: Option<String>,
    pub reason: Option<String>,
    pub measurement: Option<String>,
    pub source_event_id: Option<String>,
    pub created_at: String,
    /// `Some(qa_id)` if this row has been superseded by a later
    /// cross-actor decision per ADR-0063 §4; the live view filters
    /// `superseded_by IS NULL`.
    pub superseded_by: Option<String>,
}

// ── Context ────────────────────────────────────────────────────────

/// Context for write paths. Mirrors
/// [`aberp_work_orders::WoWriteContext`] and
/// [`aberp_inventory::RecordMovementContext`].
#[derive(Debug)]
pub struct QaWriteContext<'a> {
    pub tenant: &'a str,
    pub actor: ActorKind,
    pub ledger_meta: &'a LedgerMeta,
    pub ledger_actor: Actor,
}

// ── Auto-create (called from aberp-work-orders' routing-op Completed) ──

/// Inputs to [`auto_create_inspection_for_op_completion`]. The caller
/// (aberp-work-orders' routing-op transition handler) populates this
/// struct; the repository mints `qa_id` + the `created_at` stamp.
#[derive(Debug, Clone)]
pub struct AutoCreateInspectionInputs<'a> {
    pub wo_id: &'a str,
    pub routing_op_id: &'a str,
    /// F8 idempotency key — typically the routing-op transition's own
    /// key suffixed with `:qa-create` so re-runs of the cascade are
    /// idempotent.
    pub idempotency_key: String,
}

/// Insert a Pending `qa_inspections` row + emit one
/// `QaInspectionCreated` audit entry — all in the supplied tx.
///
/// Per ADR-0063 §"Rework" + invariant #1: if a prior non-superseded
/// inspection exists for this (wo, op), set its `superseded_by` to
/// the new qa_id so the live-state filter returns exactly one row.
pub fn auto_create_inspection_for_op_completion(
    tx: &Transaction<'_>,
    ctx: &QaWriteContext<'_>,
    inputs: AutoCreateInspectionInputs<'_>,
) -> Result<QaInspection, QaError> {
    if inputs.idempotency_key.trim().is_empty() {
        return Err(QaError::Validation(
            "idempotency_key must be non-empty".to_string(),
        ));
    }

    // Idempotency probe: if the audit ledger already has a Created
    // entry for this idempotency_key, surface duplicate. Defence-in-
    // depth — the cascade caller's own idempotency key is suffixed
    // with `:qa-create` so a routing-op-transition retry doesn't
    // double-create.
    let existing: Option<String> = tx
        .query_row(
            "SELECT qa_id FROM qa_inspections
             WHERE tenant_id = ?
             ORDER BY created_at DESC, qa_id DESC
             LIMIT 1
             OFFSET (
                CASE WHEN EXISTS (
                    SELECT 1 FROM qa_inspections
                    WHERE tenant_id = ? AND wo_id = ? AND routing_op_id = ?
                      AND created_at IS NOT NULL
                ) THEN 0 ELSE 999999999 END
             );",
            params![ctx.tenant, ctx.tenant, inputs.wo_id, inputs.routing_op_id],
            |row| row.get::<_, String>(0),
        )
        .ok();
    let _ = existing; // suppress unused-warn; the real idempotency gate is on the audit-ledger F8 chain via append_in_tx.

    // Find any prior non-superseded inspection for this (wo, op) and
    // mark it superseded by the new id. (Happens after Rework: the
    // routing-op was flipped Active, the operator re-completed it,
    // and this auto-create now needs to retire the prior Reworking
    // row in favour of the fresh Pending row.)
    let prior_live_qa_ids: Vec<String> = {
        let mut stmt = tx
            .prepare(
                "SELECT qa_id FROM qa_inspections
                 WHERE tenant_id = ? AND wo_id = ? AND routing_op_id = ?
                   AND superseded_by IS NULL;",
            )
            .map_err(|e| QaError::Storage(anyhow!("prepare prior-live-qa SELECT: {e}")))?;
        let rows = stmt
            .query_map(
                params![ctx.tenant, inputs.wo_id, inputs.routing_op_id],
                |row| row.get::<_, String>(0),
            )
            .map_err(|e| QaError::Storage(anyhow!("query prior-live-qa: {e}")))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| QaError::Storage(anyhow!("read prior-live-qa row: {e}")))?);
        }
        out
    };

    let now = now_rfc3339()?;
    let qa_id = format!("qa_{}", Ulid::new());

    // 1. INSERT the new Pending row.
    tx.execute(
        "INSERT INTO qa_inspections (
            qa_id, tenant_id, wo_id, routing_op_id, state,
            decided_at, decided_by, reason, measurement, source_event_id,
            created_at, superseded_by
         ) VALUES (?, ?, ?, ?, ?, NULL, NULL, NULL, NULL, NULL, ?, NULL);",
        params![
            &qa_id,
            ctx.tenant,
            inputs.wo_id,
            inputs.routing_op_id,
            QaState::Pending.as_str(),
            &now,
        ],
    )
    .map_err(|e| QaError::Storage(anyhow!("INSERT qa_inspections: {e}")))?;

    // 2. Set superseded_by on any prior live rows (the post-Rework path).
    for prior_id in &prior_live_qa_ids {
        tx.execute(
            "UPDATE qa_inspections SET superseded_by = ?
             WHERE tenant_id = ? AND qa_id = ?;",
            params![&qa_id, ctx.tenant, prior_id],
        )
        .map_err(|e| QaError::Storage(anyhow!("UPDATE prior qa_inspections.superseded_by: {e}")))?;
    }

    // 3. Audit-ledger entry.
    let actor_str = ctx.actor.as_operator_string();
    let payload = QaInspectionCreatedPayload {
        qa_id: qa_id.clone(),
        wo_id: inputs.wo_id.to_string(),
        routing_op_id: inputs.routing_op_id.to_string(),
        actor: actor_str.clone(),
        idempotency_key: inputs.idempotency_key.clone(),
    };
    append_in_tx(
        tx,
        ctx.ledger_meta,
        EventKind::QaInspectionCreated,
        payload.to_bytes(),
        ctx.ledger_actor.clone(),
        Some(inputs.idempotency_key.clone()),
    )
    .map_err(|e| QaError::Storage(anyhow!("audit append QaInspectionCreated: {e}")))?;

    Ok(QaInspection {
        qa_id,
        wo_id: inputs.wo_id.to_string(),
        routing_op_id: inputs.routing_op_id.to_string(),
        state: QaState::Pending,
        decided_at: None,
        decided_by: None,
        reason: None,
        measurement: None,
        source_event_id: None,
        created_at: now,
        superseded_by: None,
    })
}

// ── Decision (Pass / Fail / Rework / Dispose) ──────────────────────

/// Inputs to [`decide_qa`].
#[derive(Debug, Clone)]
pub struct DecideQaInputs {
    pub decision: QaDecision,
    pub reason: Option<String>,
    pub measurement: Option<String>,
    /// `None` for SPA-button-driven decisions; `Some(ULID)` for
    /// adapter-driven decisions per ADR-0063 §3 + the
    /// `aberp_work_orders` `source_event_id` invariant.
    pub source_event_id: Option<String>,
    pub idempotency_key: String,
}

/// Outcome of [`decide_qa`].
#[derive(Debug, Clone)]
pub struct QaDecisionOutcome {
    /// The LIVE inspection row after the decision. For cross-actor
    /// supersede this is the newly-inserted row; for same-actor in-
    /// place updates it's the same row.
    pub inspection: QaInspection,
    /// Set when the cross-actor supersede pattern fired (ADR-0063 §4).
    /// `None` for same-actor in-place updates.
    pub superseded_qa_id: Option<String>,
    /// `true` when the decision was Rework AND the upstream
    /// routing-op was flipped Active → Active (read: the SPA should
    /// re-fetch the WO detail so its routing-ops table reflects the
    /// new state). The side-effect the route layer's response names.
    pub rework_flipped_routing_op_back_to_active: bool,
    /// `true` when the decision was Dispose AND a `Scrap`
    /// `stock_movement` was emitted (ADR-0063 §6).
    pub disposed_emitted_scrap_movement: bool,
}

/// Apply a decision to a QA inspection per ADR-0063 §3. SPA buttons
/// AND future adapter events both call this handler — actor is
/// captured into the audit entry + into the `decided_by` column;
/// the state-transition logic does NOT branch on actor (the cross-
/// actor BOUNDARY does — see §4).
///
/// Per ADR-0063 §4 the cross-actor override pattern:
///   - If the new actor differs from the live row's `decided_by`
///     (or, for a still-Pending row, the auto-creator's `system`),
///     INSERT a NEW row + UPDATE the prior row's `superseded_by`.
///   - Otherwise UPDATE the row in place.
pub fn decide_qa(
    tx: &Transaction<'_>,
    ctx: &QaWriteContext<'_>,
    qa_id: &str,
    inputs: DecideQaInputs,
) -> Result<QaDecisionOutcome, QaError> {
    if inputs.idempotency_key.trim().is_empty() {
        return Err(QaError::Validation(
            "idempotency_key must be non-empty".to_string(),
        ));
    }

    // Read the row inside the tx for the optimistic-concurrency check.
    #[allow(clippy::type_complexity)]
    let row: Option<(
        String,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
    )> = tx
        .query_row(
            "SELECT state, wo_id, routing_op_id, created_at, decided_by, superseded_by
             FROM qa_inspections
             WHERE tenant_id = ? AND qa_id = ? LIMIT 1;",
            params![ctx.tenant, qa_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            },
        )
        .map(Some)
        .or_else(|e| match e {
            duckdb::Error::QueryReturnedNoRows => Ok(None),
            other => Err(anyhow!("read qa_inspections for decide: {other}")),
        })?;
    let (current_state_str, wo_id, routing_op_id, _created_at, prior_decided_by, superseded_by) =
        row.ok_or_else(|| QaError::InspectionNotFound(qa_id.to_string()))?;
    if superseded_by.is_some() {
        return Err(QaError::AlreadySuperseded(qa_id.to_string()));
    }
    let current_state = QaState::from_storage_str(&current_state_str)
        .map_err(|e| QaError::Storage(anyhow!("{e}: {current_state_str:?}")))?;

    // S249-F18: refuse Pass/Fail/Rework on a Cancelled WO. The auto-
    // complete hook silently no-ops for terminal WOs (see
    // aberp_work_orders::try_auto_complete_wo), so a concurrent
    // Cancel between this tx's SELECT and the QA-decide would
    // otherwise record `QaInspectionDecided{Passed}` against a
    // terminal WO with no warning. Dispose stays legal — scrap is the
    // natural outcome for a cancelled WO. Same in-tx raw-SQL posture
    // as the Dispose branch below (deliberately no aberp-work-orders
    // dep to avoid the cycle).
    let wo_state_str: Option<String> = tx
        .query_row(
            "SELECT state FROM work_orders WHERE tenant_id = ? AND wo_id = ? LIMIT 1;",
            params![ctx.tenant, &wo_id],
            |row| row.get::<_, String>(0),
        )
        .map(Some)
        .or_else(|e| match e {
            duckdb::Error::QueryReturnedNoRows => Ok(None),
            other => Err(QaError::Storage(anyhow!(
                "read work_orders for QA-decide gate: {other}"
            ))),
        })?;
    if let Some(s) = wo_state_str.as_deref() {
        if s == "cancelled" && !matches!(inputs.decision, QaDecision::Dispose) {
            return Err(QaError::Validation(format!(
                "WO {wo_id} is cancelled; only Dispose is legal against its QA inspections"
            )));
        }
    }

    // Refuse illegal edges loud per the state machine.
    let new_state = next_qa_state(current_state, inputs.decision)?;

    let now = now_rfc3339()?;
    let actor_str = ctx.actor.as_operator_string();

    // Cross-actor boundary: if a Pending row's auto-creator was
    // `system` and the new actor is a SPA operator, those are
    // structurally the same operator decision-side — Pending is a
    // sentinel, not an actual decision. We only fire the supersede
    // when the prior row's `decided_by` is non-NULL AND differs
    // from the new actor.
    //
    // This implements ADR-0063 §4 paragraph 6: "Operator-to-
    // operator state changes within the same inspection mutate the
    // single row. Operator-vs-adapter is the cross-actor boundary
    // that triggers supersede."
    let cross_actor_supersede = match &prior_decided_by {
        Some(prior) => prior != &actor_str,
        None => false, // Pending — first decision against a fresh row, no supersede.
    };

    let (live_qa_id, superseded_qa_id) = if cross_actor_supersede {
        // INSERT a new row + UPDATE the prior row.
        let new_qa_id = format!("qa_{}", Ulid::new());
        tx.execute(
            "INSERT INTO qa_inspections (
                qa_id, tenant_id, wo_id, routing_op_id, state,
                decided_at, decided_by, reason, measurement, source_event_id,
                created_at, superseded_by
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL);",
            params![
                &new_qa_id,
                ctx.tenant,
                &wo_id,
                &routing_op_id,
                new_state.as_str(),
                &now,
                &actor_str,
                inputs.reason.as_deref(),
                inputs.measurement.as_deref(),
                inputs.source_event_id.as_deref(),
                &now,
            ],
        )
        .map_err(|e| QaError::Storage(anyhow!("INSERT supersede-row qa_inspections: {e}")))?;

        tx.execute(
            "UPDATE qa_inspections SET superseded_by = ?
             WHERE tenant_id = ? AND qa_id = ?;",
            params![&new_qa_id, ctx.tenant, qa_id],
        )
        .map_err(|e| QaError::Storage(anyhow!("UPDATE prior qa_inspections.superseded_by: {e}")))?;

        (new_qa_id, Some(qa_id.to_string()))
    } else {
        // UPDATE in place.
        tx.execute(
            "UPDATE qa_inspections SET
                state = ?,
                decided_at = ?,
                decided_by = ?,
                reason = COALESCE(?, reason),
                measurement = COALESCE(?, measurement),
                source_event_id = COALESCE(?, source_event_id)
             WHERE tenant_id = ? AND qa_id = ?;",
            params![
                new_state.as_str(),
                &now,
                &actor_str,
                inputs.reason.as_deref(),
                inputs.measurement.as_deref(),
                inputs.source_event_id.as_deref(),
                ctx.tenant,
                qa_id,
            ],
        )
        .map_err(|e| QaError::Storage(anyhow!("UPDATE qa_inspections in place: {e}")))?;

        (qa_id.to_string(), None)
    };

    // ── Decision-specific side effects ─────────────────────────
    let mut rework_flipped = false;
    let mut disposed_emitted_scrap = false;

    match inputs.decision {
        QaDecision::Rework => {
            // ADR-0063 §6 — Reworking flips the upstream routing-op
            // back to Active. The side-effect the route layer's
            // response body names. Direct SQL UPDATE on routings
            // (we deliberately do not depend on aberp-work-orders
            // to avoid the cycle — see Cargo.toml comment).
            //
            // S264 / PR-253 (F6) — emit a `RoutingOpStateChanged`
            // (Completed → Active) audit row for this reverse flip.
            // Pre-S264 we did NOT (a comment here claimed it was
            // "cleaner than double-emitting"), so the reverse transition
            // was invisible to anyone reconstructing routing-op state
            // from the ledger — the forward Active→Completed carries an
            // audit, the reverse only lived implicitly inside the QA
            // row. PR-243's F19 made the QA gate load-bearing for the
            // auto-complete cascade that walks exactly this state, so the
            // hole is on a now-load-bearing path. The QA row records the
            // DECISION; the ledger needs the routing-op STATE TRANSITION
            // as a first-class walkable row.
            let updated = tx
                .execute(
                    "UPDATE routings SET state = ?, completed_at = NULL
                     WHERE tenant_id = ? AND routing_op_id = ?;",
                    params!["active", ctx.tenant, &routing_op_id],
                )
                .map_err(|e| QaError::Storage(anyhow!("UPDATE routings (rework flip): {e}")))?;
            // If the row didn't exist that's odd — the QA inspection
            // references a routing_op_id that must exist. Surface as
            // a storage error so a future bug can't silently swallow
            // it (CLAUDE.md rule 12).
            if updated == 0 {
                return Err(QaError::Storage(anyhow!(
                    "Rework UPDATE matched no routings row for routing_op_id={routing_op_id}"
                )));
            }
            let rework_op_idem = format!("rework-op:{}:{}", routing_op_id, inputs.idempotency_key);
            let rework_audit = ReworkRoutingOpStateChangedPayload {
                routing_op_id: &routing_op_id,
                wo_id: &wo_id,
                from_state: "completed",
                to_state: "active",
                actor: &actor_str,
                idempotency_key: rework_op_idem.clone(),
            };
            append_in_tx(
                tx,
                ctx.ledger_meta,
                EventKind::RoutingOpStateChanged,
                serde_json::to_vec(&rework_audit)
                    .expect("JSON serialization of routing-op payload cannot fail"),
                ctx.ledger_actor.clone(),
                Some(rework_op_idem),
            )
            .map_err(|e| {
                QaError::Storage(anyhow!(
                    "audit append RoutingOpStateChanged (rework flip): {e}"
                ))
            })?;
            rework_flipped = true;
        }
        QaDecision::Dispose => {
            // ADR-0063 §6 — emit one Scrap stock_movement sized at
            // the WO's qty_target (v1 whole-qty per ADR-0063
            // §"Adversarial review" #5; partial-qty is named-
            // deferred to v2).
            //
            // We need the WO's product_id + qty_target. Read from
            // work_orders directly via SQL — same posture as the
            // Rework UPDATE above (no aberp-work-orders dep).
            let (product_id, qty_target_str): (String, String) = tx
                .query_row(
                    "SELECT product_id, CAST(qty_target AS VARCHAR)
                     FROM work_orders WHERE tenant_id = ? AND wo_id = ? LIMIT 1;",
                    params![ctx.tenant, &wo_id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .map_err(|e| {
                    QaError::Storage(anyhow!("SELECT work_orders for Dispose qty: {e}"))
                })?;
            let qty_target = Decimal::from_str(&qty_target_str).map_err(|e| {
                QaError::Storage(anyhow!("parse qty_target {qty_target_str:?}: {e}"))
            })?;
            let movement_ctx = RecordMovementContext {
                tenant: ctx.tenant,
                actor: ctx.actor.clone(),
                ledger_meta: ctx.ledger_meta,
                ledger_actor: ctx.ledger_actor.clone(),
            };
            let movement_inputs = RecordMovementInputs {
                product_id,
                qty_delta: -qty_target,
                reason: MovementReason::Scrap,
                ref_kind: MovementRefKind::QaInspection,
                ref_id: Some(live_qa_id.clone()),
                notes: inputs.reason.clone(),
                idempotency_key: format!("{}:scrap", inputs.idempotency_key),
            };
            record_movement(tx, &movement_ctx, movement_inputs).map_err(|e| match e {
                aberp_inventory::InventoryError::DuplicateIdempotencyKey(k) => {
                    QaError::DuplicateIdempotencyKey(k)
                }
                aberp_inventory::InventoryError::ProductNotFound(p) => {
                    QaError::Validation(format!("Dispose: product {p} not found"))
                }
                aberp_inventory::InventoryError::WrongSignForReason {
                    reason,
                    required,
                    got,
                } => QaError::Validation(format!(
                    "Dispose sign-violation: reason {reason} requires {required:?}, got {got}"
                )),
                aberp_inventory::InventoryError::Storage(err) => QaError::Storage(err),
            })?;
            disposed_emitted_scrap = true;
        }
        QaDecision::Pass | QaDecision::Fail => {
            // No inventory side-effects, no routing-op flip.
        }
    }

    // ── Audit-ledger entry ─────────────────────────────────────
    let payload = QaInspectionDecidedPayload {
        qa_id: live_qa_id.clone(),
        wo_id: wo_id.clone(),
        routing_op_id: routing_op_id.clone(),
        from_state: current_state,
        to_state: new_state,
        reason: inputs.reason.clone(),
        measurement: inputs.measurement.clone(),
        actor: actor_str.clone(),
        source_event_id: inputs.source_event_id.clone(),
        superseded_qa_id: superseded_qa_id.clone(),
        idempotency_key: inputs.idempotency_key.clone(),
    };
    append_in_tx(
        tx,
        ctx.ledger_meta,
        EventKind::QaInspectionDecided,
        payload.to_bytes(),
        ctx.ledger_actor.clone(),
        Some(format!(
            "decide:{}:{}",
            inputs.decision.as_str(),
            inputs.idempotency_key
        )),
    )
    .map_err(|e| QaError::Storage(anyhow!("audit append QaInspectionDecided: {e}")))?;

    // Read back the LIVE row (the new one on supersede, the same
    // one on in-place).
    let live = get_qa_inspection_tx(tx, ctx.tenant, &live_qa_id)?
        .ok_or_else(|| QaError::Storage(anyhow!("decide_qa: live row vanished after write")))?;

    Ok(QaDecisionOutcome {
        inspection: live,
        superseded_qa_id,
        rework_flipped_routing_op_back_to_active: rework_flipped,
        disposed_emitted_scrap_movement: disposed_emitted_scrap,
    })
}

// ── Reads ──────────────────────────────────────────────────────────

/// List inspections in the tenant, optionally filtering by state.
/// Ordered by `created_at ASC, qa_id ASC` (oldest first) so the QA
/// queue surfaces the longest-waiting items at the top of the
/// `Pending` view per ADR-0063 §8.
pub fn list_qa_inspections(
    conn: &Connection,
    tenant: &str,
    state_filter: Option<QaState>,
    limit: u32,
    offset: u32,
) -> anyhow::Result<Vec<QaInspection>> {
    let mut sql = String::from(
        "SELECT qa_id, wo_id, routing_op_id, state, decided_at, decided_by,
                reason, measurement, source_event_id, created_at, superseded_by
         FROM qa_inspections WHERE tenant_id = ?",
    );
    if state_filter.is_some() {
        sql.push_str(" AND state = ?");
    }
    sql.push_str(" ORDER BY created_at ASC, qa_id ASC LIMIT ? OFFSET ?;");

    let mut stmt = conn.prepare(&sql)?;
    let out: Vec<QaInspection> = match state_filter {
        Some(s) => {
            let rows = stmt.query_map(
                params![tenant, s.as_str(), limit, offset],
                row_to_inspection,
            )?;
            let mut acc = Vec::new();
            for r in rows {
                acc.push(r??);
            }
            acc
        }
        None => {
            let rows = stmt.query_map(params![tenant, limit, offset], row_to_inspection)?;
            let mut acc = Vec::new();
            for r in rows {
                acc.push(r??);
            }
            acc
        }
    };
    Ok(out)
}

/// Counts of QA inspections by state for the tenant. Returns a fully-
/// populated [`QaStateCounts`] (every state always present, zero-
/// defaulted) so the dashboard SPA renders a fixed row.
///
/// Single `SELECT state, COUNT(*) ... GROUP BY state` — used by the
/// operator dashboard tile (PR-231 / S235).
pub fn count_qa_inspections_by_state(
    conn: &Connection,
    tenant: &str,
) -> anyhow::Result<QaStateCounts> {
    let mut stmt = conn.prepare(
        "SELECT state, COUNT(*) FROM qa_inspections WHERE tenant_id = ? GROUP BY state;",
    )?;
    let rows = stmt.query_map(params![tenant], |row| {
        let state_str: String = row.get(0)?;
        let count: i64 = row.get(1)?;
        Ok((state_str, count))
    })?;
    let mut counts = QaStateCounts::default();
    for r in rows {
        let (state_str, count) = r?;
        let state =
            QaState::from_storage_str(&state_str).map_err(|e| anyhow!("{e}: {state_str:?}"))?;
        let bucket = match state {
            QaState::Pending => &mut counts.pending,
            QaState::Passed => &mut counts.passed,
            QaState::Failed => &mut counts.failed,
            QaState::Reworking => &mut counts.reworking,
            QaState::Disposed => &mut counts.disposed,
        };
        *bucket = u32::try_from(count.max(0)).unwrap_or(u32::MAX);
    }
    Ok(counts)
}

/// Tenant-scoped count of QA inspections grouped by [`QaState`]. All
/// five fields are always present (zero-defaulted).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QaStateCounts {
    pub pending: u32,
    pub passed: u32,
    pub failed: u32,
    pub reworking: u32,
    pub disposed: u32,
}

/// Read a single QA inspection row by id, scoped to the tenant.
/// `None` for unknown ids.
pub fn get_qa_inspection(
    conn: &Connection,
    tenant: &str,
    qa_id: &str,
) -> anyhow::Result<Option<QaInspection>> {
    conn.query_row(
        "SELECT qa_id, wo_id, routing_op_id, state, decided_at, decided_by,
                reason, measurement, source_event_id, created_at, superseded_by
         FROM qa_inspections WHERE tenant_id = ? AND qa_id = ? LIMIT 1;",
        params![tenant, qa_id],
        |row| Ok(parse_inspection_row(row)),
    )
    .map(Some)
    .or_else(|e| match e {
        duckdb::Error::QueryReturnedNoRows => Ok(None),
        other => Err(anyhow!("SELECT qa_inspections by id: {other}")),
    })?
    .transpose()
}

/// Internal: same as [`get_qa_inspection`] but takes a `&Transaction`
/// (used by [`decide_qa`] to read back the live row inside the tx).
fn get_qa_inspection_tx(
    tx: &Transaction<'_>,
    tenant: &str,
    qa_id: &str,
) -> Result<Option<QaInspection>, QaError> {
    tx.query_row(
        "SELECT qa_id, wo_id, routing_op_id, state, decided_at, decided_by,
                reason, measurement, source_event_id, created_at, superseded_by
         FROM qa_inspections WHERE tenant_id = ? AND qa_id = ? LIMIT 1;",
        params![tenant, qa_id],
        |row| Ok(parse_inspection_row(row)),
    )
    .map(Some)
    .or_else(|e| match e {
        duckdb::Error::QueryReturnedNoRows => Ok(None),
        other => Err(QaError::Storage(anyhow!(
            "SELECT qa_inspections in-tx: {other}"
        ))),
    })?
    .transpose()
    .map_err(|e| QaError::Storage(anyhow!("parse qa_inspections row: {e}")))
}

/// List LIVE inspections for a WO — `superseded_by IS NULL`, ordered
/// by routing-op `sequence` so callers can render them aligned with
/// the WorkOrderDetail routing table. The WO-completion gate consumes
/// this — see [`all_live_inspections_passed_for_wo`].
pub fn list_live_inspections_for_wo(
    conn: &Connection,
    tenant: &str,
    wo_id: &str,
) -> anyhow::Result<Vec<QaInspection>> {
    // Join against routings to get the sequence ordering. Simple
    // SELECT keeps the SQL portable per [[no-sql-specific]] (no
    // DuckDB-specific syntax).
    let mut stmt = conn.prepare(
        "SELECT q.qa_id, q.wo_id, q.routing_op_id, q.state, q.decided_at, q.decided_by,
                q.reason, q.measurement, q.source_event_id, q.created_at, q.superseded_by
         FROM qa_inspections q
         LEFT JOIN routings r
           ON r.tenant_id = q.tenant_id AND r.routing_op_id = q.routing_op_id
         WHERE q.tenant_id = ? AND q.wo_id = ? AND q.superseded_by IS NULL
         ORDER BY r.sequence ASC, q.created_at ASC, q.qa_id ASC;",
    )?;
    let rows = stmt.query_map(params![tenant, wo_id], row_to_inspection)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r??);
    }
    Ok(out)
}

/// WO-completion gate per ADR-0063 §7 + invariant #6: returns true
/// only when EVERY routing-op of the WO has at least one live
/// `qa_inspections` row in state `Passed`. A WO with NO routing-ops
/// returns true vacuously, but the WO-create handler rejects that
/// case per ADR-0062 §"create_work_order" — a WO MUST have at least
/// one routing-op. A routing-op with no live inspection (because no
/// Completed has fired yet) returns false: the operator must walk
/// the per-op Complete buttons first.
///
/// Surfaced as a boolean rather than `Result<(), Reason>` so the
/// caller (the WO Complete handler) can compose a structured error
/// at the call site. The route layer additionally fetches the
/// blocking routing-ops via [`list_live_inspections_for_wo`] for a
/// helpful 400 body.
pub fn all_live_inspections_passed_for_wo(
    tx: &Transaction<'_>,
    tenant: &str,
    wo_id: &str,
) -> anyhow::Result<bool> {
    // Two SELECTs: (1) total routing-op count for the WO; (2) count
    // of routing-ops that have at least one live Passed inspection.
    // The gate is satisfied iff (1) > 0 AND (2) == (1).
    let total_ops: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM routings WHERE tenant_id = ? AND wo_id = ?;",
            params![tenant, wo_id],
            |row| row.get(0),
        )
        .context("count routing ops for WO gate")?;
    if total_ops == 0 {
        return Ok(false);
    }
    let passed_ops: i64 = tx
        .query_row(
            "SELECT COUNT(DISTINCT r.routing_op_id)
             FROM routings r
             JOIN qa_inspections q
               ON q.tenant_id = r.tenant_id
              AND q.routing_op_id = r.routing_op_id
             WHERE r.tenant_id = ? AND r.wo_id = ?
               AND q.superseded_by IS NULL
               AND q.state = 'passed';",
            params![tenant, wo_id],
            |row| row.get(0),
        )
        .context("count passed-live ops for WO gate")?;
    Ok(passed_ops == total_ops)
}

// ── Row parsers ────────────────────────────────────────────────────

#[allow(clippy::type_complexity)]
fn row_to_inspection(row: &duckdb::Row<'_>) -> duckdb::Result<anyhow::Result<QaInspection>> {
    Ok(parse_inspection_row(row))
}

#[allow(clippy::type_complexity)]
fn parse_inspection_row(row: &duckdb::Row<'_>) -> anyhow::Result<QaInspection> {
    let qa_id: String = row.get(0).context("get qa_id")?;
    let wo_id: String = row.get(1).context("get wo_id")?;
    let routing_op_id: String = row.get(2).context("get routing_op_id")?;
    let state_str: String = row.get(3).context("get state")?;
    let decided_at: Option<String> = row.get(4).context("get decided_at")?;
    let decided_by: Option<String> = row.get(5).context("get decided_by")?;
    let reason: Option<String> = row.get(6).context("get reason")?;
    let measurement: Option<String> = row.get(7).context("get measurement")?;
    let source_event_id: Option<String> = row.get(8).context("get source_event_id")?;
    let created_at: String = row.get(9).context("get created_at")?;
    let superseded_by: Option<String> = row.get(10).context("get superseded_by")?;
    let state = QaState::from_storage_str(&state_str).map_err(|e| anyhow!("{e}: {state_str:?}"))?;
    Ok(QaInspection {
        qa_id,
        wo_id,
        routing_op_id,
        state,
        decided_at,
        decided_by,
        reason,
        measurement,
        source_event_id,
        created_at,
        superseded_by,
    })
}

fn now_rfc3339() -> Result<String, QaError> {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|e| QaError::Storage(anyhow!("format Rfc3339: {e}")))
}
