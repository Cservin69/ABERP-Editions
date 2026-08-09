//! Audit-ledger payloads for the Dispatch kinds per ADR-0064 §6, plus the
//! S440 `export.*` export-control family fired at the same shipment boundary.
//!
//! - [`DispatchCreatedPayload`] → `mes.dispatch_created`
//! - [`DispatchShippedPayload`] → `mes.dispatch_shipped`
//! - [`ExportClassificationSetPayload`] → `export.classification_set`
//! - [`ExportAccessCheckPayload`] → `export.access_check`
//! - [`ExportShipmentLoggedPayload`] → `export.shipment_logged`
//!
//! All round-trip through `serde_json`; the closed-vocab `CarrierKind`
//! enum re-uses the `rename_all = "snake_case"` from [`crate::types`].
//!
//! ## Why the `export.*` payloads live here
//!
//! S440 wires the first firing sites for the `export.*` family, which shipped
//! as kinds-only in S359 ("firing sites land in a later session"). Their one
//! firing boundary is [`crate::mark_shipped`] — the moment goods actually
//! leave — so the payload structs live beside the dispatch payloads they are
//! appended alongside, inside the SAME transaction. Every field name is
//! transcribed verbatim from the `EventKind` doc comments in
//! `aberp_audit_ledger`; the doc comment IS the schema contract and
//! [`export_payload_field_names_match_the_eventkind_docs`] pins it.

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

// ── S440 — the `export.*` family (ADR-0076 kinds, first firing sites) ──

/// `export.classification_set` payload — emitted once per `mark_shipped`
/// call, recording the export classification the injected
/// [`aberp_compliance::export_control::ExportControlProvider`] returned for
/// the commodity about to leave.
///
/// The determination is the provider's, never this crate's: mis-classification
/// is a felony, so nothing here infers a code from the item. With the
/// production-default `MockExportControlProvider` the answer is
/// `NotClassified`, which renders as `jurisdiction = "UNKNOWN"` with both code
/// fields `null` — a faithful "no determination has been made", NOT a silent
/// `NOT_CONTROLLED` (which is a *positive* determination that would read as
/// "cleared for export").
///
/// Field names are verbatim from the `EventKind::ExportClassificationSet` doc.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExportClassificationSetPayload {
    /// What was classified. `"product"` at this firing site — the commodity
    /// referenced by the shipped WO.
    pub entity_kind: String,
    /// The artifact key — the WO's `product_id`.
    pub entity_id: String,
    /// The EAR Commerce-Control-List number when EAR-listed (or the literal
    /// `EAR99`); `null` otherwise. Shape-validated through
    /// `aberp_compliance::export_control::validate_eccn` at the write boundary
    /// so a malformed code can never reach the ledger.
    pub eccn: Option<String>,
    /// The USML category when ITAR-controlled; `null` otherwise.
    pub usml_category: Option<String>,
    /// The regime string, rendered through
    /// `aberp_compliance::export_control::Jurisdiction::as_str` — one of
    /// `ITAR` / `EAR` / `EAR99` / `NOT_CONTROLLED` / `UNKNOWN`.
    pub jurisdiction: String,
    /// Who the determination is recorded against — the accountability anchor.
    pub operator_user_id: String,
    /// Epoch-ms stamp of the determination.
    pub classified_at_ms: i64,
}

impl ExportClassificationSetPayload {
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self)
            .expect("JSON serialization of ExportClassificationSetPayload cannot fail")
    }
}

/// `export.access_check` payload — the export-eligibility *decision* on a
/// shipment: the consignee was screened against the denied-party lists and the
/// export was granted or refused.
///
/// ITAR's deemed-export rule (22 CFR § 120.62) makes the decision trail
/// load-bearing, so EVERY check is recorded — not just the denials. The
/// granted row is appended inside the `mark_shipped` transaction (atomic with
/// the state flip it authorises); the denied row is appended by the route
/// layer, because a denial rolls the ship transaction back and there is no
/// business write left for it to be atomic with.
///
/// Field names are verbatim from the `EventKind::ExportAccessCheck` doc.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExportAccessCheckPayload {
    /// Artifact kind. `"dispatch"` at this firing site — the shipment whose
    /// release was being decided.
    pub entity_kind: String,
    /// Artifact key — the `dsp_id`.
    pub entity_id: String,
    /// Who asked (the operator driving the ship).
    pub operator_user_id: String,
    /// `"granted"` / `"denied"`, rendered through
    /// `aberp_compliance::export_control::AccessDecision::as_str`.
    pub decision: String,
    /// The rule that drove the verdict. Never empty — the clear path renders
    /// the positive statement rather than `""`.
    pub reason: String,
    /// Epoch-ms stamp of the check.
    pub checked_at_ms: i64,
}

impl ExportAccessCheckPayload {
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("JSON serialization of ExportAccessCheckPayload cannot fail")
    }
}

/// `export.shipment_logged` payload — the physical-export record: controlled
/// goods crossed to a recipient party / country under a stated authorization.
///
/// Appended inside the `mark_shipped` transaction, immediately after the
/// `mes.dispatch_shipped` entry, so an export row can never exist for a
/// shipment that rolled back (nor a shipment exist without its export row).
///
/// Field names are verbatim from the `EventKind::ExportShipmentLogged` doc.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExportShipmentLoggedPayload {
    /// The shipment key — the `dsp_id`.
    pub shipment_id: String,
    /// The exporter of record. The caller supplies it; today the route passes
    /// the tenant id, because ABERP has no separate exporter-of-record party
    /// (flagged in the S440 PR body).
    pub exporter_party_id: String,
    /// The consignee — the dispatch's `partner_id`.
    pub recipient_party_id: String,
    /// ISO 3166-1 alpha-2 destination, upper-cased. Empty string when the
    /// partner record carries no country — recorded faithfully rather than
    /// guessed (an invented `"HU"` would be a false export record).
    pub recipient_country: String,
    /// The licence / licence-exception / ECCN cited. Populated from the same
    /// determination the `export.classification_set` row carries; `null` when
    /// no determination exists.
    pub ecn_or_authorization: Option<String>,
    /// Epoch-ms stamp of the shipment.
    pub shipped_at_ms: i64,
    /// Who released the shipment.
    pub operator_user_id: String,
}

impl ExportShipmentLoggedPayload {
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self)
            .expect("JSON serialization of ExportShipmentLoggedPayload cannot fail")
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

    // ── S440 — `export.*` payload shape pins ───────────────────────

    /// The three `export.*` payloads carry EXACTLY the field names the
    /// `EventKind` doc comments specify — no more, no fewer. The doc comment is
    /// the schema contract for a hash-chained, append-only ledger: once a row
    /// is written the shape cannot be migrated, so a renamed or dropped field
    /// silently breaks every future compliance query. A contributor who renames
    /// `usml_category` to `usml` fails here, not against a BIS auditor.
    #[test]
    fn export_payload_field_names_match_the_eventkind_docs() {
        let classification = ExportClassificationSetPayload {
            entity_kind: "product".into(),
            entity_id: "prd_1".into(),
            eccn: Some("7A994".into()),
            usml_category: None,
            jurisdiction: "EAR".into(),
            operator_user_id: "ervin".into(),
            classified_at_ms: 1_780_000_000_000,
        };
        let v: serde_json::Value = serde_json::from_slice(&classification.to_bytes()).unwrap();
        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "classified_at_ms",
                "eccn",
                "entity_id",
                "entity_kind",
                "jurisdiction",
                "operator_user_id",
                "usml_category",
            ]
        );

        let access = ExportAccessCheckPayload {
            entity_kind: "dispatch".into(),
            entity_id: "dsp_1".into(),
            operator_user_id: "ervin".into(),
            decision: "granted".into(),
            reason: "denied-party screening: clear".into(),
            checked_at_ms: 1_780_000_000_000,
        };
        let v: serde_json::Value = serde_json::from_slice(&access.to_bytes()).unwrap();
        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "checked_at_ms",
                "decision",
                "entity_id",
                "entity_kind",
                "operator_user_id",
                "reason",
            ]
        );

        let shipment = ExportShipmentLoggedPayload {
            shipment_id: "dsp_1".into(),
            exporter_party_id: "tenant-a".into(),
            recipient_party_id: "ptr_1".into(),
            recipient_country: "DE".into(),
            ecn_or_authorization: None,
            shipped_at_ms: 1_780_000_000_000,
            operator_user_id: "ervin".into(),
        };
        let v: serde_json::Value = serde_json::from_slice(&shipment.to_bytes()).unwrap();
        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "ecn_or_authorization",
                "exporter_party_id",
                "operator_user_id",
                "recipient_country",
                "recipient_party_id",
                "shipment_id",
                "shipped_at_ms",
            ]
        );
    }

    /// All three `export.*` payloads round-trip.
    #[test]
    fn export_payloads_round_trip() {
        let classification = ExportClassificationSetPayload {
            entity_kind: "product".into(),
            entity_id: "prd_1".into(),
            eccn: None,
            usml_category: Some("VIII(h)".into()),
            jurisdiction: "ITAR".into(),
            operator_user_id: "ervin".into(),
            classified_at_ms: 1,
        };
        let back: ExportClassificationSetPayload =
            serde_json::from_slice(&classification.to_bytes()).unwrap();
        assert_eq!(back, classification);

        let access = ExportAccessCheckPayload {
            entity_kind: "dispatch".into(),
            entity_id: "dsp_1".into(),
            operator_user_id: "ervin".into(),
            decision: "denied".into(),
            reason: "denied-party screening: denied (OFAC SDN)".into(),
            checked_at_ms: 2,
        };
        let back: ExportAccessCheckPayload = serde_json::from_slice(&access.to_bytes()).unwrap();
        assert_eq!(back, access);

        let shipment = ExportShipmentLoggedPayload {
            shipment_id: "dsp_1".into(),
            exporter_party_id: "tenant-a".into(),
            recipient_party_id: "ptr_1".into(),
            recipient_country: String::new(),
            ecn_or_authorization: Some("EAR99".into()),
            shipped_at_ms: 3,
            operator_user_id: "ervin".into(),
        };
        let back: ExportShipmentLoggedPayload =
            serde_json::from_slice(&shipment.to_bytes()).unwrap();
        assert_eq!(back, shipment);
    }

    /// The optional `export.*` fields MUST serialize as JSON null, never be
    /// omitted — same posture as `spawned_invoice_id` above. A compliance
    /// walker distinguishes "no determination" (`null`) from "the writer forgot
    /// the field" (absent); for an export-control row that distinction is the
    /// difference between a documented gap and an unexplained one.
    #[test]
    fn export_payload_optionals_serialize_as_null_not_omitted() {
        let classification = ExportClassificationSetPayload {
            entity_kind: "product".into(),
            entity_id: "prd_1".into(),
            eccn: None,
            usml_category: None,
            jurisdiction: "UNKNOWN".into(),
            operator_user_id: "ervin".into(),
            classified_at_ms: 1,
        };
        let v: serde_json::Value = serde_json::from_slice(&classification.to_bytes()).unwrap();
        assert!(v["eccn"].is_null());
        assert!(v["usml_category"].is_null());

        let shipment = ExportShipmentLoggedPayload {
            shipment_id: "dsp_1".into(),
            exporter_party_id: "tenant-a".into(),
            recipient_party_id: "ptr_1".into(),
            recipient_country: String::new(),
            ecn_or_authorization: None,
            shipped_at_ms: 1,
            operator_user_id: "ervin".into(),
        };
        let v: serde_json::Value = serde_json::from_slice(&shipment.to_bytes()).unwrap();
        assert!(v["ecn_or_authorization"].is_null());
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
