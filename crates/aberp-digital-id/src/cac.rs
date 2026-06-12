//! [`UsDodCacProvider`] — a deterministic, **non-production** US-DoD-CAC
//! stub backend (S363 / PR-50, ADR-0080).
//!
//! This is the SECOND [`DigitalIdProvider`] implementation. Its job is not to
//! issue real certificates — it is to *prove the trait abstracts*. Where the
//! S344 [`crate::MockProvider`] is HMAC-shaped (a single static operator, a
//! keyed-MAC recompute), this stub models a US DoD Common Access Card with
//! three deliberately-different semantics, so a future real backend slots in
//! behind the same trait without the trait having silently calcified around
//! HMAC:
//!
//! 1. **Signing persona** — a certificate-bound `stub-ecdsa-p256-cac` digest,
//!    NOT a keyed HMAC. (Still a stub: a plain SHA-256 over card-bound bytes,
//!    obviously not real ECDSA.)
//! 2. **`current_operator()` is session-based, not static** — the operator
//!    comes from an *inserted card session*. Eject the card ([`Self::ejected`])
//!    and there is no current operator, no signing: the trait's
//!    [`ProviderError::NoCurrentOperator`] arm finally has a real producer.
//! 3. **Verification is cert-chain membership, not MAC equality** — `verify`
//!    first checks the claimed signer is present in the reader's trusted chain
//!    (a real CAC reader trusts the DoD PKI root, not the payload), THEN
//!    recomputes the stub digest. A signature whose digest is internally
//!    consistent but whose signer is *not in the chain* is rejected — a case
//!    the mock's pure-HMAC verify cannot even express.
//!
//! ⚠️ Like the mock, it logs a WARN on every construction and must never back
//! a production operator identity. The "ECDSA" here is a SHA-256 stand-in.

use sha2::{Digest, Sha256};

use crate::mock::constant_time_eq;
use crate::{DigitalId, DigitalIdProvider, ProviderError, Signature};

/// Algorithm tag this stub stamps onto every [`Signature`]. Distinct from the
/// mock's `mock-hmac-sha256`, so a signature minted by one backend can never
/// be recomputed under the other's verifier.
pub const CAC_ALGORITHM: &str = "stub-ecdsa-p256-cac";

/// Issuer tag for identities vended by this backend.
pub const CAC_ISSUER: &str = "us-dod-cac";

/// EDIPI (the CAC subject identifier) of the default stub card [`Self::new`]
/// inserts — an obviously-fake 10-digit value, never a real person.
pub const CAC_DEFAULT_EDIPI: &str = "0000000363";

/// Fixed Unix-epoch-ms pinned into the stub identity / signatures so they are
/// byte-deterministic across calls and process restarts (2023-11-14T22:13:20Z
/// — an arbitrary fixed constant, NOT a wall-clock read). Real backends stamp
/// the live authentication / signing clock.
const CAC_TIMESTAMP_MS: u64 = 1_700_000_000_000;

/// One inserted-card session: the identity the card asserts plus the reader's
/// trusted chain. In a real backend the chain is the DoD PKI path validated up
/// to a trusted root; here it is just the set of EDIPIs the reader trusts
/// (always, and only, the inserted card itself).
#[derive(Debug, Clone)]
struct CacSession {
    identity: DigitalId,
    /// Signer ids ([`DigitalId::id`] / EDIPIs) this reader trusts. Verification
    /// rejects any signature whose `signer_id` is absent from this list.
    trusted_chain: Vec<String>,
}

/// A deterministic, non-production US-DoD-CAC [`DigitalIdProvider`] stub.
///
/// Holds an `Option<CacSession>`: `Some` when a card is inserted, `None` once
/// ejected. This `Option` IS the point — it makes `current_operator()` /
/// `sign()` genuinely fallible in a way the always-present mock never is.
#[derive(Debug, Clone)]
pub struct UsDodCacProvider {
    session: Option<CacSession>,
}

impl UsDodCacProvider {
    /// Insert the default stub card ([`CAC_DEFAULT_EDIPI`]). Emits a WARN — by
    /// design — so a misconfigured production boot that lands on this stub is
    /// loud, never silent.
    pub fn new() -> Self {
        Self::with_edipi(CAC_DEFAULT_EDIPI)
    }

    /// Insert a stub card for a specific EDIPI. Lets a caller (and the S363
    /// tests) model two different cards in the same reader — distinct
    /// operators producing distinct, card-bound signatures.
    pub fn with_edipi(edipi: impl Into<String>) -> Self {
        tracing::warn!("DigitalIdProvider: US-DoD-CAC STUB — NOT FOR PRODUCTION USE");
        let edipi = edipi.into();
        let identity = DigitalId {
            id: edipi.clone(),
            display_name: format!("CAC Stub Operator {edipi}"),
            issuer: CAC_ISSUER.to_string(),
            // A CAC operator carries clearance scopes a bare mock operator
            // does not — proving `scope` is provider-shaped, not fixed.
            scope: vec!["operator".to_string(), "cui-cleared".to_string()],
            issued_at_ms: CAC_TIMESTAMP_MS,
        };
        Self {
            session: Some(CacSession {
                trusted_chain: vec![edipi],
                identity,
            }),
        }
    }

    /// A reader with NO card inserted. `current_operator()` / `sign()` /
    /// `verify()` all surface [`ProviderError::NoCurrentOperator`] — the
    /// session-based semantics the mock cannot express. Still WARNs: a stub
    /// reader, card or not, is never production.
    pub fn ejected() -> Self {
        tracing::warn!("DigitalIdProvider: US-DoD-CAC STUB (no card) — NOT FOR PRODUCTION USE");
        Self { session: None }
    }

    fn session(&self) -> Result<&CacSession, ProviderError> {
        self.session
            .as_ref()
            .ok_or(ProviderError::NoCurrentOperator)
    }
}

impl Default for UsDodCacProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl DigitalIdProvider for UsDodCacProvider {
    fn name(&self) -> &str {
        CAC_ISSUER
    }

    fn current_operator(&self) -> Result<DigitalId, ProviderError> {
        Ok(self.session()?.identity.clone())
    }

    fn sign(&self, payload: &[u8]) -> Result<Signature, ProviderError> {
        let session = self.session()?;
        let signer_id = session.identity.id.clone();
        Ok(Signature {
            algorithm: CAC_ALGORITHM.to_string(),
            bytes: stub_cac_digest(&signer_id, payload).to_vec(),
            signer_id,
            signed_at_ms: CAC_TIMESTAMP_MS,
        })
    }

    fn verify(&self, payload: &[u8], sig: &Signature) -> Result<bool, ProviderError> {
        let session = self.session()?;
        // Algorithm tag first: a foreign-backend signature must never be
        // recomputed under this stub's digest.
        if sig.algorithm != CAC_ALGORITHM {
            return Ok(false);
        }
        // Cert-chain membership — the genuinely different verification shape.
        // A real reader trusts the DoD PKI path, not the payload: a signature
        // whose signer is absent from the trusted chain is rejected before any
        // digest math, even if the digest is internally self-consistent.
        if !session.trusted_chain.iter().any(|id| id == &sig.signer_id) {
            return Ok(false);
        }
        let expected = stub_cac_digest(&sig.signer_id, payload);
        Ok(constant_time_eq(&expected, &sig.bytes))
    }
}

/// The stub "ECDSA" signature: `SHA-256(signer_id ‖ 0x00 ‖ payload)`.
///
/// Deliberately a plain, UN-keyed digest (NOT an HMAC) so the construction is
/// visibly different from the mock's and visibly fake. The signer_id is folded
/// in so the signature is *card-bound*: a different EDIPI yields a different
/// signature over the same payload, modelling a certificate-bound signature
/// without any real key material. The `0x00` separator domain-separates the
/// two inputs so `("ab", "c")` and `("a", "bc")` cannot collide.
fn stub_cac_digest(signer_id: &str, payload: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(signer_id.as_bytes());
    h.update([0x00]);
    h.update(payload);
    let mut out = [0u8; 32];
    out.copy_from_slice(&h.finalize());
    out
}
