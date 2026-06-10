//! S344 / PR-38 — OPTIONAL digital-identity attestation for audit payloads.
//!
//! # Foundation only
//!
//! Per the defense-aerospace pivot (`[[defense-aerospace-pivot]]`), future
//! audit events will carry the digital identity of the operator who
//! authorised them. This module defines the *shape* of that attestation so
//! later sessions (S346) can opt new `EventKind`s into it without a payload
//! migration. **No existing event uses it yet**, and no new `EventKind` is
//! added in S344 — so the F12 four-edit ritual does not fire.
//!
//! # Why a wrapper, not a field on `Entry`
//!
//! Audit payloads are opaque `Vec<u8>` on [`crate::Entry`] (the canonical
//! CBOR hash covers them as bytes). There is no shared payload base to hang
//! a field on. Instead, a future event whose payload type is `T` wraps it in
//! [`Signed<T>`]; legacy events simply never wrap, so they are byte-for-byte
//! unchanged and every existing hash-chain test keeps passing.
//!
//! # Why [`DigitalIdRef`] is a plain DTO
//!
//! It deliberately does **not** depend on the `aberp-digital-id` crate.
//! `audit-ledger` is a low-level crate; pulling the identity crate in would
//! invert the dependency direction (identity → audit, not audit → identity).
//! The binary layer that owns both crates is responsible for projecting an
//! `aberp_digital_id::DigitalId` + `Signature` down into this DTO when it
//! constructs a signed payload.

use serde::{Deserialize, Serialize};

/// A compact, audit-embeddable reference to the operator identity + the
/// signature that authorised an event.
///
/// Mirrors the fields of `aberp_digital_id::DigitalId` + `Signature` that an
/// auditor needs to correlate an entry back to a verified identity, without
/// embedding the full identity record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DigitalIdRef {
    /// The signer's stable opaque id (`DigitalId::id`).
    pub signer_id: String,
    /// The issuing authority (`DigitalId::issuer`), e.g. `"mock"`,
    /// `"us-dod-cac"`.
    pub issuer: String,
    /// The signature algorithm tag (`Signature::algorithm`).
    pub algorithm: String,
    /// The raw signature bytes (`Signature::bytes`).
    pub signature: Vec<u8>,
    /// Unix-epoch-ms the signature was produced (`Signature::signed_at_ms`).
    pub signed_at_ms: u64,
}

/// A payload `T` optionally accompanied by a digital-identity attestation.
///
/// Legacy / unsigned events set `signer = None` and serialize to the same
/// shape they always had plus a `null` `signer` — round-trip stable. Future
/// events populate `signer` with a [`DigitalIdRef`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signed<T> {
    /// The wrapped event payload.
    pub payload: T,
    /// The attestation, or `None` for an unsigned event.
    #[serde(default = "none")]
    pub signer: Option<DigitalIdRef>,
}

fn none() -> Option<DigitalIdRef> {
    None
}

impl<T> Signed<T> {
    /// Wrap a payload with no attestation (the legacy/default posture).
    pub fn unsigned(payload: T) -> Self {
        Self {
            payload,
            signer: None,
        }
    }

    /// Wrap a payload with a digital-identity attestation.
    pub fn with_signer(payload: T, signer: DigitalIdRef) -> Self {
        Self {
            payload,
            signer: Some(signer),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct DummyPayload {
        invoice_id: String,
        amount: u64,
    }

    fn sample() -> DummyPayload {
        DummyPayload {
            invoice_id: "INV-001".to_string(),
            amount: 42,
        }
    }

    #[test]
    fn s344_audit_payload_signer_round_trips_when_none() {
        let wrapped = Signed::unsigned(sample());
        let bytes = serde_json::to_vec(&wrapped).expect("serialize");
        let back: Signed<DummyPayload> = serde_json::from_slice(&bytes).expect("deserialize");
        assert_eq!(back, wrapped);
        assert!(back.signer.is_none());
        assert_eq!(back.payload, sample());
    }

    #[test]
    fn s344_audit_payload_signer_round_trips_when_some_digital_id() {
        let signer = DigitalIdRef {
            signer_id: "mock-op-001".to_string(),
            issuer: "mock".to_string(),
            algorithm: "mock-hmac-sha256".to_string(),
            signature: vec![0xDE, 0xAD, 0xBE, 0xEF],
            signed_at_ms: 1_700_000_000_000,
        };
        let wrapped = Signed::with_signer(sample(), signer.clone());
        let bytes = serde_json::to_vec(&wrapped).expect("serialize");
        let back: Signed<DummyPayload> = serde_json::from_slice(&bytes).expect("deserialize");
        assert_eq!(back, wrapped);
        assert_eq!(back.signer, Some(signer));
        assert_eq!(back.payload, sample());
    }

    #[test]
    fn s344_audit_payload_legacy_bytes_deserialize_with_absent_signer() {
        // A payload serialized BEFORE the signer field existed has no
        // `signer` key at all. `#[serde(default)]` must hydrate it to None
        // so legacy ledger bytes keep deserializing — the backward-compat
        // invariant this whole module is built to protect.
        let legacy = br#"{"payload":{"invoice_id":"INV-001","amount":42}}"#;
        let back: Signed<DummyPayload> = serde_json::from_slice(legacy).expect("deserialize");
        assert!(back.signer.is_none());
        assert_eq!(back.payload, sample());
    }
}
