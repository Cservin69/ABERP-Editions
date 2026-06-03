//! Audit-ledger payloads for the two Dispatch kinds per ADR-0064 §6.
//!
//! - [`DispatchCreatedPayload`] → `mes.dispatch_created`
//! - [`DispatchShippedPayload`] → `mes.dispatch_shipped`
//!
//! Both round-trip through `serde_json`; the closed-vocab `CarrierKind`
//! enum re-uses the `rename_all = "snake_case"` from [`crate::types`].

use serde::{Deserialize, Serialize};

use crate::types::CarrierKind;

/// `mes.dispatch_created` payload — emitted once per Drafted dispatch
/// row inserted by `create_dispatch`. Per ADR-0064 §6 carries the
/// load-bearing trace-back fields so a future audit walk can
/// reconstruct who created the dispatch + against which WO + for which
/// recipient.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DispatchCreatedPayload {
    /// `dsp_<ULID>`.
    pub dsp_id: String,
    /// Parent WO (`wo_<ULID>`). One dispatch per WO in v1 per ADR-0064 §2.
    pub wo_id: String,
    /// Recipient partner (`ptr_<ULID>` or whatever prefix partners use).
    pub partner_id: String,
    /// Human-readable operator attribution string per
    /// [`aberp_inventory::ActorKind::as_operator_string`].
    pub actor: String,
    /// F8 idempotency key — caller-provided; pinned by
    /// `aberp_audit_ledger::append_in_tx`.
    pub idempotency_key: String,
}

impl DispatchCreatedPayload {
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("JSON serialization of DispatchCreatedPayload cannot fail")
    }
}

/// `mes.dispatch_shipped` payload — emitted once per successful
/// `mark_shipped` call. Per ADR-0064 §6 + invariant #1 this lands in
/// the SAME transaction as the dispatch state flip, the
/// `stock_movement` row, and the `spawned_invoice_id` UPDATE. The
/// audit-trail walks both ways: from dispatch to invoice via this
/// payload's `spawned_invoice_id`, and from the invoice draft's own
/// `InvoiceDraftCreated` audit entry back to the dispatch via the
/// invoice idempotency-key suffix (`derive_from(dispatch.dsp_id,
/// "spawn_invoice")`).
///
/// `spawned_invoice_id` is `Option<String>` so the v1 deferred-spawner
/// posture (the production `InvoiceSpawner` is a no-op in PR-230;
/// PR-230b lands the real billing extraction) can record a faithful
/// `None` instead of fabricating a fake id. Tests that exercise the
/// real spawner pin `Some(_)`; the v1 production-noop pins `None` so
/// the audit-walker can distinguish "spawn deferred" from "spawn
/// fired."
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DispatchShippedPayload {
    pub dsp_id: String,
    pub wo_id: String,
    pub partner_id: String,
    pub carrier_kind: CarrierKind,
    /// Operator-typed or pasted tracking number; `None` when the
    /// carrier is `SelfDelivery` / `CustomerPickup` and the operator
    /// has nothing to record.
    pub tracking_number: Option<String>,
    /// RFC3339 timestamp the operator named (or `now()` when the form
    /// did not surface the picker).
    pub shipped_at: String,
    /// `Some(invoice_id)` when the injected `InvoiceSpawner` produced
    /// a draft in the same tx; `None` when the spawner was the v1
    /// `NoopInvoiceSpawner` (PR-230b lands the real spawner — see
    /// open question in the PR-230 body).
    pub spawned_invoice_id: Option<String>,
    pub actor: String,
    pub idempotency_key: String,
}

impl DispatchShippedPayload {
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("JSON serialization of DispatchShippedPayload cannot fail")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_created() -> DispatchCreatedPayload {
        DispatchCreatedPayload {
            dsp_id: "dsp_01H8DSP000000000000000000".to_string(),
            wo_id: "wo_01H8WORK0000000000000000".to_string(),
            partner_id: "ptr_01H8PTR000000000000000000".to_string(),
            actor: "ervin".to_string(),
            idempotency_key: "01H8IDEM00000000000000000".to_string(),
        }
    }

    fn sample_shipped() -> DispatchShippedPayload {
        DispatchShippedPayload {
            dsp_id: "dsp_01H8DSP000000000000000000".to_string(),
            wo_id: "wo_01H8WORK0000000000000000".to_string(),
            partner_id: "ptr_01H8PTR000000000000000000".to_string(),
            carrier_kind: CarrierKind::MagyarPosta,
            tracking_number: Some("MPL-XYZ-123".to_string()),
            shipped_at: "2026-06-03T10:00:00Z".to_string(),
            spawned_invoice_id: None,
            actor: "ervin".to_string(),
            idempotency_key: "01H8IDEM00000000000000001".to_string(),
        }
    }

    #[test]
    fn created_payload_round_trips() {
        let p = sample_created();
        let back: DispatchCreatedPayload = serde_json::from_slice(&p.to_bytes()).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn shipped_payload_round_trips() {
        let p = sample_shipped();
        let back: DispatchShippedPayload = serde_json::from_slice(&p.to_bytes()).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn shipped_payload_uses_snake_case_carrier_tokens() {
        let mut p = sample_shipped();
        p.carrier_kind = CarrierKind::CustomerPickup;
        let v: serde_json::Value = serde_json::from_slice(&p.to_bytes()).unwrap();
        assert_eq!(v["carrier_kind"].as_str(), Some("customer_pickup"));
    }

    /// `spawned_invoice_id: None` MUST serialize as a JSON null, not
    /// be omitted — same posture as the QA-decided pin
    /// `superseded_qa_id_none_serializes_as_null_not_omitted`. A
    /// future audit-walker relies on the field being present so it
    /// can distinguish "spawn deferred (PR-230 v1 noop)" from
    /// "writer forgot the field."
    #[test]
    fn shipped_payload_spawned_invoice_id_none_serializes_as_null_not_omitted() {
        let p = sample_shipped();
        assert!(p.spawned_invoice_id.is_none());
        let v: serde_json::Value = serde_json::from_slice(&p.to_bytes()).unwrap();
        assert!(v["spawned_invoice_id"].is_null());
    }

    /// `tracking_number: None` MUST serialize as JSON null too —
    /// SelfDelivery + CustomerPickup carriers don't have a tracking
    /// number; the audit-walker distinguishes "no tracking" from
    /// "writer forgot."
    #[test]
    fn shipped_payload_tracking_number_none_serializes_as_null_not_omitted() {
        let mut p = sample_shipped();
        p.carrier_kind = CarrierKind::SelfDelivery;
        p.tracking_number = None;
        let v: serde_json::Value = serde_json::from_slice(&p.to_bytes()).unwrap();
        assert!(v["tracking_number"].is_null());
    }
}
