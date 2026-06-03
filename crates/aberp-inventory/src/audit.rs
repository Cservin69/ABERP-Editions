//! Audit-ledger payload for [`aberp_audit_ledger::EventKind::StockMovementRecorded`].
//!
//! Per ADR-0061 §4 every stock_movements INSERT emits exactly one
//! ledger entry of kind `mes.stock_movement_recorded`. The payload
//! carries the structured movement so the audit-evidence trail can be
//! consumed without joining back into `stock_movements` (the ledger is
//! the load-bearing artifact; the table is the queryable cache).
//!
//! ## Why a distinct EventKind from `mes.adapter_event`
//!
//! `mes.adapter_event` is broadcast telemetry — losing one is
//! acceptable per ADR-0060 §"Consequences" #4. A stock movement is
//! regulated quantity — losing one means the cache drifts and
//! inventory is wrong. The two surfaces must not share an audit kind
//! even though both live under the `mes.*` prefix family.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::types::{MovementReason, MovementRefKind};

/// Audit-ledger payload for [`aberp_audit_ledger::EventKind::StockMovementRecorded`].
///
/// JSON-serialised into the audit-ledger row's `payload` BLOB. The
/// closed-vocab enums round-trip through the rename-all-snake-case
/// serde form (matching the on-disk storage strings — see the
/// `movement_reason_serde_matches_storage_string` pin in `types.rs`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StockMovementRecordedPayload {
    /// `mvt_<ULID>` per ADR-0061 §1.
    pub movement_id: String,
    /// `prd_<ULID>` per ADR-0061 §1.
    pub product_id: String,
    /// Signed delta. Positive = inbound (Receipt / WoCompletion);
    /// negative = outbound (BomConsumption / Dispatch / Scrap);
    /// any-non-zero for Adjustment per the reason-sign matrix.
    /// Serialised as a JSON string by rust_decimal's default serde
    /// path so JS clients do not lose precision.
    pub qty_delta: Decimal,
    /// Why the movement happened.
    pub reason: MovementReason,
    /// Trace-back label. `None` for operator-typed manual movements
    /// (Adjustment originated from the SPA), where the matching
    /// `ref_id` is also `None`.
    pub ref_kind: Option<MovementRefKind>,
    /// `<prefix>_<ULID>` of the entity that caused the movement, when
    /// `ref_kind` is non-`None`. `None` paired with
    /// `Some(MovementRefKind::Manual)` — the Manual variant is the
    /// explicit "no upstream ref" sentinel; the `ref_id` is still
    /// `None`. The repository documents this pairing.
    pub ref_id: Option<String>,
    /// Human-readable operator attribution (see
    /// [`crate::types::ActorKind::as_operator_string`]).
    pub operator: String,
    /// F8 idempotency key — the client-supplied retry token.
    pub idempotency_key: String,
}

impl StockMovementRecordedPayload {
    /// Encode to the audit-ledger's byte form (JSON). Mirrors every
    /// other `mes.*` and `system.*` payload's `to_bytes` helper.
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self)
            .expect("JSON serialization of StockMovementRecordedPayload cannot fail")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    use std::str::FromStr;

    fn sample() -> StockMovementRecordedPayload {
        StockMovementRecordedPayload {
            movement_id: "mvt_01H8MES1234567890ABCDEFGHJ".to_string(),
            product_id: "prd_01H8PROD234567890ABCDEFGH".to_string(),
            qty_delta: Decimal::from_str("-3.500000").unwrap(),
            reason: MovementReason::BomConsumption,
            ref_kind: Some(MovementRefKind::WorkOrder),
            ref_id: Some("wo_01H8WORK1234567890ABCDEFG".to_string()),
            operator: "ervin".to_string(),
            idempotency_key: "01H8IDEMPOTENT0000000000000".to_string(),
        }
    }

    #[test]
    fn payload_round_trips_through_bytes() {
        let p = sample();
        let bytes = p.to_bytes();
        let back: StockMovementRecordedPayload = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back, p);
    }

    /// Closed-vocab enum tokens MUST be snake_case on the wire so the
    /// SPA's audit-timeline reader and the SQL `WHERE
    /// payload->>'reason' = …` queries see the same surface.
    #[test]
    fn payload_uses_snake_case_enum_tokens() {
        let p = sample();
        let v: serde_json::Value = serde_json::from_slice(&p.to_bytes()).unwrap();
        assert_eq!(v["reason"].as_str(), Some("bom_consumption"));
        assert_eq!(v["ref_kind"].as_str(), Some("work_order"));
    }

    /// Manual adjustment posture per ADR-0061 §6: `ref_kind = Manual`
    /// and `ref_id = None`. The SPA form never populates ref_id; the
    /// payload's `Option<String>` carries the `None` faithfully.
    #[test]
    fn payload_manual_adjustment_carries_null_ref_id() {
        let mut p = sample();
        p.reason = MovementReason::Adjustment;
        p.ref_kind = Some(MovementRefKind::Manual);
        p.ref_id = None;
        let v: serde_json::Value = serde_json::from_slice(&p.to_bytes()).unwrap();
        assert_eq!(v["ref_kind"].as_str(), Some("manual"));
        assert!(v["ref_id"].is_null());
    }
}
