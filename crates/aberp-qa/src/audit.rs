//! Audit-ledger payloads for the two QA-queue kinds per ADR-0063 §5.
//!
//! - [`QaInspectionCreatedPayload`] → `mes.qa_inspection_created`
//! - [`QaInspectionDecidedPayload`]  → `mes.qa_inspection_decided`
//!
//! Both round-trip through `serde_json`; the closed-vocab `QaState`
//! enum re-uses the `rename_all = "snake_case"` from [`crate::types`].

use serde::{Deserialize, Serialize};

use crate::types::QaState;

/// `mes.qa_inspection_created` payload — emitted once per Pending
/// row inserted by the auto-create path (every routing-op Completed
/// creates exactly one inspection per ADR-0063 §2 + invariant #1).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QaInspectionCreatedPayload {
    /// `qa_<ULID>`.
    pub qa_id: String,
    /// Parent WO (`wo_<ULID>`).
    pub wo_id: String,
    /// The routing-op (`rop_<ULID>`) whose Completed transition
    /// triggered the auto-create.
    pub routing_op_id: String,
    /// Human-readable operator attribution string per
    /// [`aberp_inventory::ActorKind::as_operator_string`].
    pub actor: String,
    /// F8 idempotency key — typically the routing-op transition's own
    /// key suffixed with `:qa-create` so re-runs of the cascade are
    /// idempotent.
    pub idempotency_key: String,
}

impl QaInspectionCreatedPayload {
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self)
            .expect("JSON serialization of QaInspectionCreatedPayload cannot fail")
    }
}

/// `mes.qa_inspection_decided` payload — emitted once per decide_qa
/// call (the F8 idempotency key gates retries).
///
/// `superseded_qa_id` is **load-bearing** per ADR-0063 §4: when the
/// new actor differs from the live row's actor we INSERT a NEW row +
/// UPDATE the prior row's `superseded_by`. This field carries the
/// prior `qa_id` so a future audit walk can reconstruct the chain
/// without re-querying the table.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QaInspectionDecidedPayload {
    pub qa_id: String,
    pub wo_id: String,
    pub routing_op_id: String,
    pub from_state: QaState,
    pub to_state: QaState,
    pub reason: Option<String>,
    pub measurement: Option<String>,
    pub actor: String,
    /// `Some(ULID)` when an adapter event drove the decision; `None`
    /// for SPA-button-driven decisions per ADR-0063 §3.
    pub source_event_id: Option<String>,
    /// `Some(qa_id)` when this decision superseded a prior cross-actor
    /// row per ADR-0063 §4; `None` for same-actor in-place updates and
    /// for the first decision against a Pending row.
    pub superseded_qa_id: Option<String>,
    pub idempotency_key: String,
}

impl QaInspectionDecidedPayload {
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self)
            .expect("JSON serialization of QaInspectionDecidedPayload cannot fail")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_created() -> QaInspectionCreatedPayload {
        QaInspectionCreatedPayload {
            qa_id: "qa_01H8QA0000000000000000000".to_string(),
            wo_id: "wo_01H8WORK0000000000000000".to_string(),
            routing_op_id: "rop_01H8OP000000000000000000".to_string(),
            actor: "system".to_string(),
            idempotency_key: "01H8IDEM00000000000000000".to_string(),
        }
    }

    fn sample_decided() -> QaInspectionDecidedPayload {
        QaInspectionDecidedPayload {
            qa_id: "qa_01H8QA0000000000000000000".to_string(),
            wo_id: "wo_01H8WORK0000000000000000".to_string(),
            routing_op_id: "rop_01H8OP000000000000000000".to_string(),
            from_state: QaState::Pending,
            to_state: QaState::Passed,
            reason: None,
            measurement: None,
            actor: "ervin".to_string(),
            source_event_id: None,
            superseded_qa_id: None,
            idempotency_key: "01H8IDEM00000000000000001".to_string(),
        }
    }

    #[test]
    fn created_payload_round_trips() {
        let p = sample_created();
        let back: QaInspectionCreatedPayload = serde_json::from_slice(&p.to_bytes()).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn decided_payload_round_trips() {
        let p = sample_decided();
        let back: QaInspectionDecidedPayload = serde_json::from_slice(&p.to_bytes()).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn decided_payload_uses_snake_case_state_tokens() {
        let mut p = sample_decided();
        p.from_state = QaState::Reworking;
        p.to_state = QaState::Disposed;
        let v: serde_json::Value = serde_json::from_slice(&p.to_bytes()).unwrap();
        assert_eq!(v["from_state"].as_str(), Some("reworking"));
        assert_eq!(v["to_state"].as_str(), Some("disposed"));
    }

    /// `source_event_id: None` MUST serialize as a JSON null, not
    /// be omitted — same posture as
    /// [[s232-work-order-cascade]]'s pin
    /// `source_event_id_none_serializes_as_null_not_omitted`.
    #[test]
    fn source_event_id_none_serializes_as_null_not_omitted() {
        let p = sample_decided();
        assert!(p.source_event_id.is_none());
        let v: serde_json::Value = serde_json::from_slice(&p.to_bytes()).unwrap();
        assert!(v["source_event_id"].is_null());
    }

    /// `superseded_qa_id: None` also serializes as JSON null — same
    /// rationale: a future audit-walker relies on the field being
    /// present so it can distinguish "no supersede" from "writer
    /// forgot the field."
    #[test]
    fn superseded_qa_id_none_serializes_as_null_not_omitted() {
        let p = sample_decided();
        assert!(p.superseded_qa_id.is_none());
        let v: serde_json::Value = serde_json::from_slice(&p.to_bytes()).unwrap();
        assert!(v["superseded_qa_id"].is_null());
    }
}
