//! A virtual WebAuthn platform authenticator.
//!
//! It produces exactly the bytes an iPhone's Secure Enclave produces
//! for `navigator.credentials.create()` and `.get()`: a
//! `clientDataJSON`, an `attestationObject` with `fmt: "none"`, an
//! `authenticatorData` with the right RP-ID hash and flags, and an
//! ES256 signature over `authData || SHA-256(clientDataJSON)`.
//!
//! That is the whole point of testing the relying party this way: the
//! agent verifies *bytes*, and these are the same bytes. What a
//! software key cannot reproduce is the biometric gate that releases a
//! real key — that lives in the OS, is asserted by the `UV` flag, and
//! this authenticator can therefore also lie about it, which is exactly
//! what `refuses_an_assertion_without_user_verification` needs.

#![allow(dead_code)]

use ciborium::value::Value;
use p256::ecdsa::signature::Signer as _;
use sha2::{Digest, Sha256};

pub const FLAG_UP: u8 = 0x01;
pub const FLAG_UV: u8 = 0x04;
pub const FLAG_AT: u8 = 0x40;

pub fn b64url(raw: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw)
}

pub struct VirtualAuthenticator {
    key: p256::ecdsa::SigningKey,
    pub credential_id: Vec<u8>,
    pub sign_count: u32,
}

impl VirtualAuthenticator {
    /// A distinct authenticator per `seed` — one for the iPhone, one
    /// for the Mac, and a third for the "someone else's passkey" case.
    pub fn new(seed: u8) -> Self {
        Self {
            key: p256::ecdsa::SigningKey::from_bytes(&[seed.max(1); 32].into())
                .expect("test signing key"),
            credential_id: vec![seed; 16],
            sign_count: 0,
        }
    }

    fn cose_key(&self) -> Vec<u8> {
        let point = self.key.verifying_key().to_encoded_point(false);
        let map = Value::Map(vec![
            (Value::Integer(1.into()), Value::Integer(2.into())), // kty: EC2
            (Value::Integer(3.into()), Value::Integer((-7i64).into())), // alg: ES256
            (Value::Integer((-1i64).into()), Value::Integer(1.into())), // crv: P-256
            (
                Value::Integer((-2i64).into()),
                Value::Bytes(point.x().expect("x").to_vec()),
            ),
            (
                Value::Integer((-3i64).into()),
                Value::Bytes(point.y().expect("y").to_vec()),
            ),
        ]);
        let mut out = Vec::new();
        ciborium::into_writer(&map, &mut out).expect("cose key");
        out
    }

    fn auth_data(&self, rp_id: &str, flags: u8, attested: bool) -> Vec<u8> {
        let mut v = Sha256::digest(rp_id.as_bytes()).to_vec();
        v.push(flags);
        v.extend_from_slice(&self.sign_count.to_be_bytes());
        if attested {
            v.extend_from_slice(&[0u8; 16]); // aaguid
            v.extend_from_slice(&(self.credential_id.len() as u16).to_be_bytes());
            v.extend_from_slice(&self.credential_id);
            v.extend_from_slice(&self.cose_key());
        }
        v
    }

    fn client_data(&self, ceremony: &str, challenge: &str, origin: &str) -> Vec<u8> {
        serde_json::json!({
            "type": ceremony,
            "challenge": challenge,
            "origin": origin,
            "crossOrigin": false,
        })
        .to_string()
        .into_bytes()
    }

    /// `navigator.credentials.create()`.
    pub fn register(
        &self,
        rp_id: &str,
        origin: &str,
        challenge: &str,
        flags: u8,
    ) -> serde_json::Value {
        let client_data = self.client_data("webauthn.create", challenge, origin);
        let auth_data = self.auth_data(rp_id, flags, true);
        let attestation = Value::Map(vec![
            (Value::Text("fmt".into()), Value::Text("none".into())),
            (Value::Text("attStmt".into()), Value::Map(Vec::new())),
            (Value::Text("authData".into()), Value::Bytes(auth_data)),
        ]);
        let mut object = Vec::new();
        ciborium::into_writer(&attestation, &mut object).expect("attestation object");
        serde_json::json!({
            "client_data_json": b64url(&client_data),
            "attestation_object": b64url(&object),
        })
    }

    /// `navigator.credentials.get()`.
    pub fn assert(
        &self,
        rp_id: &str,
        origin: &str,
        challenge: &str,
        flags: u8,
    ) -> serde_json::Value {
        let client_data = self.client_data("webauthn.get", challenge, origin);
        let auth_data = self.auth_data(rp_id, flags, false);
        let mut signed = auth_data.clone();
        signed.extend_from_slice(Sha256::digest(&client_data).as_slice());
        let signature: p256::ecdsa::Signature = self.key.sign(&signed);
        serde_json::json!({
            "id": b64url(&self.credential_id),
            "client_data_json": b64url(&client_data),
            "authenticator_data": b64url(&auth_data),
            "signature": b64url(signature.to_der().as_bytes()),
        })
    }

    /// The stored form of this authenticator's credential, as the
    /// console-confirmation step would commit it (ADR-0115 §4.3b).
    ///
    /// This is the ONLY way a software authenticator gets into the
    /// credential store now: §4.3a refuses its ceremony outright, and
    /// `e2e_portal::a_software_credential_cannot_enrol` pins that. See
    /// `Portal::provision_credential`.
    pub fn as_stored_credential(&self, label: &str) -> aberp_portal_agent::credstore::Credential {
        let point = self.key.verifying_key().to_encoded_point(false);
        aberp_portal_agent::credstore::Credential {
            id: b64url(&self.credential_id),
            x: hex::encode(point.x().expect("x")),
            y: hex::encode(point.y().expect("y")),
            sign_count: self.sign_count,
            label: label.to_string(),
            created_at: "2026-08-21T00:00:00Z".to_string(),
        }
    }

    /// The normal case: user present and user verified.
    pub fn register_verified(
        &self,
        rp_id: &str,
        origin: &str,
        challenge: &str,
    ) -> serde_json::Value {
        self.register(rp_id, origin, challenge, FLAG_UP | FLAG_UV | FLAG_AT)
    }

    pub fn assert_verified(&self, rp_id: &str, origin: &str, challenge: &str) -> serde_json::Value {
        self.assert(rp_id, origin, challenge, FLAG_UP | FLAG_UV)
    }
}
