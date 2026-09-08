//! D-15 (S355 / ADR-0073) — the electronic-signature ceremony.
//!
//! Fires [`EventKind::PersonnelSignatureApplied`] — the 21 CFR Part 11 §11.50
//! signature-manifestation anchor: a durable, hash-chained record that operator
//! X signed record Y with algorithm Z at time T, under a registered digital
//! identity ([`aberp_digital_id::DigitalIdProvider`]). The provider does the
//! actual signing (mock HMAC today, a real CAC/eID backend later); this module
//! validates the operator's target, shapes the ceremony record, and appends the
//! audit landmark on the caller's tx on the shared `aberp_db::Handle`
//! (ADR-0099).
//!
//! The `operator_user_id` is the SIGNER's provider-issued identity id (bound
//! into the signature), never a request-body value — the whole point of the
//! ceremony is that the identity layer, not the client, attests who signed.

use anyhow::{Context, Result};
use duckdb::Transaction;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use aberp_audit_ledger::{append_in_tx, Actor, EventKind, LedgerMeta};

/// Operator intake: WHAT is being signed. `signed_record_kind` is a
/// discriminator (e.g. `"invoice"` / `"work_order"` / `"inspection"`);
/// `signed_record_id` the target record's id.
#[derive(Debug, Clone, Deserialize)]
pub struct SignatureCeremonyInput {
    pub signed_record_kind: String,
    pub signed_record_id: String,
}

/// The completed ceremony, echoed to the operator. `operator_user_id` +
/// `signature_algorithm` + `signed_at_ms` come from the signature the provider
/// produced, not the request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignatureCeremonyRecord {
    pub operator_user_id: String,
    pub operator_display_name: String,
    pub signed_record_kind: String,
    pub signed_record_id: String,
    pub signature_algorithm: String,
    pub signed_at_ms: u64,
}

/// Ceremony rejections — operator-fixable, so the serve layer maps them to 400.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ESignatureError {
    #[error("signed_record_kind must not be empty")]
    EmptyRecordKind,
    #[error("signed_record_id must not be empty")]
    EmptyRecordId,
}

/// The canonical bytes the signature covers. Binding the kind + id (with a
/// domain-separating prefix) means a signature over one record can never be
/// replayed as a signature over another.
pub fn payload_to_sign(signed_record_kind: &str, signed_record_id: &str) -> Vec<u8> {
    format!("personnel.signature_applied\nkind={signed_record_kind}\nid={signed_record_id}")
        .into_bytes()
}

impl SignatureCeremonyRecord {
    /// Assemble a ceremony record from a validated intake + the signature the
    /// provider produced.
    pub fn assemble(
        input: &SignatureCeremonyInput,
        signer_id: &str,
        display_name: &str,
        signature_algorithm: &str,
        signed_at_ms: u64,
    ) -> std::result::Result<Self, ESignatureError> {
        let kind = input.signed_record_kind.trim();
        if kind.is_empty() {
            return Err(ESignatureError::EmptyRecordKind);
        }
        let id = input.signed_record_id.trim();
        if id.is_empty() {
            return Err(ESignatureError::EmptyRecordId);
        }
        Ok(Self {
            operator_user_id: signer_id.to_string(),
            operator_display_name: display_name.to_string(),
            signed_record_kind: kind.to_string(),
            signed_record_id: id.to_string(),
            signature_algorithm: signature_algorithm.to_string(),
            signed_at_ms,
        })
    }
}

/// Append `personnel.signature_applied` for `record` inside the caller's tx.
pub fn append_signature_applied_in_tx(
    tx: &Transaction<'_>,
    ledger_meta: &LedgerMeta,
    ledger_actor: Actor,
    record: &SignatureCeremonyRecord,
) -> Result<()> {
    let payload = serde_json::json!({
        "operator_user_id": record.operator_user_id,
        "signed_record_kind": record.signed_record_kind,
        "signed_record_id": record.signed_record_id,
        "signature_algorithm": record.signature_algorithm,
        "signed_at_ms": record.signed_at_ms,
    });
    append_in_tx(
        tx,
        ledger_meta,
        EventKind::PersonnelSignatureApplied,
        serde_json::to_vec(&payload).expect("serialize personnel.signature_applied"),
        ledger_actor,
        Some(format!(
            "personnel_signature:{}:{}:{}",
            record.signed_record_kind, record.signed_record_id, record.signed_at_ms
        )),
    )
    .context("audit append PersonnelSignatureApplied")?;
    Ok(())
}

/// The `granted_by` sentinel for a self-service authorisation (ADR-0117 §7 —
/// the identity system authorised it; no distinct second human).
pub const SELF_SERVICE: &str = "self-service";

/// Append `personnel.access_granted` — the operator was authorised to sign the
/// named record (ADR-0073 payload).
pub fn append_access_granted_in_tx(
    tx: &Transaction<'_>,
    ledger_meta: &LedgerMeta,
    ledger_actor: Actor,
    operator_user_id: &str,
    resource_kind: &str,
    resource_id: &str,
    reason: &str,
) -> Result<()> {
    let payload = serde_json::json!({
        "operator_user_id": operator_user_id,
        "resource_kind": resource_kind,
        "resource_id": resource_id,
        "granted_by": SELF_SERVICE,
        "reason": reason,
    });
    append_in_tx(
        tx,
        ledger_meta,
        EventKind::PersonnelAccessGranted,
        serde_json::to_vec(&payload).expect("serialize personnel.access_granted"),
        ledger_actor,
        Some(format!(
            "personnel_access_granted:{resource_kind}:{resource_id}"
        )),
    )
    .context("audit append PersonnelAccessGranted")?;
    Ok(())
}

/// Append `personnel.access_denied` — the operator was refused access to sign
/// the named record (ADR-0073 payload).
pub fn append_access_denied_in_tx(
    tx: &Transaction<'_>,
    ledger_meta: &LedgerMeta,
    ledger_actor: Actor,
    operator_user_id: &str,
    resource_kind: &str,
    resource_id: &str,
    denied_reason: &str,
) -> Result<()> {
    let payload = serde_json::json!({
        "operator_user_id": operator_user_id,
        "resource_kind": resource_kind,
        "resource_id": resource_id,
        "denied_reason": denied_reason,
    });
    append_in_tx(
        tx,
        ledger_meta,
        EventKind::PersonnelAccessDenied,
        serde_json::to_vec(&payload).expect("serialize personnel.access_denied"),
        ledger_actor,
        Some(format!(
            "personnel_access_denied:{resource_kind}:{resource_id}"
        )),
    )
    .context("audit append PersonnelAccessDenied")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(kind: &str, id: &str) -> SignatureCeremonyInput {
        SignatureCeremonyInput {
            signed_record_kind: kind.to_string(),
            signed_record_id: id.to_string(),
        }
    }

    #[test]
    fn assemble_trims_and_carries_the_signature_fields() {
        let rec = SignatureCeremonyRecord::assemble(
            &input("  work_order ", " wo_123 "),
            "mock-op-001",
            "Mock Operator",
            "mock-hmac-sha256",
            1_750_000_000_000,
        )
        .unwrap();
        assert_eq!(rec.operator_user_id, "mock-op-001");
        assert_eq!(rec.signed_record_kind, "work_order");
        assert_eq!(rec.signed_record_id, "wo_123");
        assert_eq!(rec.signature_algorithm, "mock-hmac-sha256");
        assert_eq!(rec.signed_at_ms, 1_750_000_000_000);
    }

    #[test]
    fn empty_kind_or_id_is_rejected() {
        assert_eq!(
            SignatureCeremonyRecord::assemble(&input("  ", "wo_1"), "s", "d", "a", 0),
            Err(ESignatureError::EmptyRecordKind)
        );
        assert_eq!(
            SignatureCeremonyRecord::assemble(&input("invoice", "   "), "s", "d", "a", 0),
            Err(ESignatureError::EmptyRecordId)
        );
    }

    #[test]
    fn payload_binds_kind_and_id_distinctly() {
        // Different records → different signed bytes (no cross-record replay).
        assert_ne!(
            payload_to_sign("invoice", "x"),
            payload_to_sign("work_order", "x")
        );
        assert_ne!(
            payload_to_sign("invoice", "a"),
            payload_to_sign("invoice", "b")
        );
        // Stable for the same input.
        assert_eq!(
            payload_to_sign("invoice", "x"),
            payload_to_sign("invoice", "x")
        );
    }
}
