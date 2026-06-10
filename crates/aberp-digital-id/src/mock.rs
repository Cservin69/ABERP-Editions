//! [`MockProvider`] — a deterministic, **non-production** test backend.
//!
//! ⚠️ This signs with a hand-rolled HMAC-SHA256 keyed on a publicly-known
//! constant ([`MOCK_TEST_KEY`]). It is NOT cryptographically meaningful. It
//! exists to prove the sign/verify *shape* and to give downstream sessions
//! (audit wiring, SPA surfaces) a stable identity to develop against. It
//! logs a WARN on every construction so it can never silently ship.

use sha2::{Digest, Sha256};

use crate::{DigitalId, DigitalIdProvider, ProviderError, Signature};

/// The publicly-known test key the mock HMACs with. The name is the warning.
pub const MOCK_TEST_KEY: &[u8] = b"MOCK_TEST_KEY_NEVER_USE_IN_PROD";

/// Algorithm tag the mock stamps onto every [`Signature`] it produces.
pub const MOCK_ALGORITHM: &str = "mock-hmac-sha256";

/// Stable opaque id of the single operator the mock vends.
pub const MOCK_OPERATOR_ID: &str = "mock-op-001";

/// Fixed Unix-epoch-ms the mock pins into `issued_at_ms` / `signed_at_ms`
/// so its identity and signatures are byte-deterministic across calls and
/// across process restarts (2023-11-14T22:13:20Z — an arbitrary fixed
/// constant, NOT a wall-clock read). Real backends stamp the live clock.
const MOCK_TIMESTAMP_MS: u64 = 1_700_000_000_000;

/// SHA-256 block size in bytes (HMAC pad width).
const SHA256_BLOCK_SIZE: usize = 64;

/// A deterministic, non-production [`DigitalIdProvider`].
#[derive(Debug, Clone)]
pub struct MockProvider {
    identity: DigitalId,
}

impl MockProvider {
    /// Construct the mock. Emits a WARN — by design — so a misconfigured
    /// production boot that falls through to the mock is loud, not silent.
    pub fn new() -> Self {
        tracing::warn!("DigitalIdProvider: MOCK — NOT FOR PRODUCTION USE");
        Self {
            identity: DigitalId {
                id: MOCK_OPERATOR_ID.to_string(),
                display_name: "Mock Operator".to_string(),
                issuer: "mock".to_string(),
                scope: vec!["operator".to_string()],
                issued_at_ms: MOCK_TIMESTAMP_MS,
            },
        }
    }
}

impl Default for MockProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl DigitalIdProvider for MockProvider {
    fn name(&self) -> &str {
        "mock"
    }

    fn current_operator(&self) -> Result<DigitalId, ProviderError> {
        Ok(self.identity.clone())
    }

    fn sign(&self, payload: &[u8]) -> Result<Signature, ProviderError> {
        let mac = hmac_sha256(MOCK_TEST_KEY, payload);
        Ok(Signature {
            algorithm: MOCK_ALGORITHM.to_string(),
            bytes: mac.to_vec(),
            signer_id: self.identity.id.clone(),
            signed_at_ms: MOCK_TIMESTAMP_MS,
        })
    }

    fn verify(&self, payload: &[u8], sig: &Signature) -> Result<bool, ProviderError> {
        // Algorithm tag is checked first: a signature minted by some other
        // backend must never be recomputed under the mock's HMAC.
        if sig.algorithm != MOCK_ALGORITHM {
            return Ok(false);
        }
        let expected = hmac_sha256(MOCK_TEST_KEY, payload);
        Ok(constant_time_eq(&expected, &sig.bytes))
    }
}

/// Hand-rolled HMAC-SHA256 (RFC 2104) over `sha2`.
///
/// Deliberately in-tree rather than the `hmac` crate — see the crate
/// `Cargo.toml` rationale. Mock-only; do not lift this into a real backend.
pub(crate) fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    // Derive the block-sized key K': hash if longer than the block, else
    // right-pad with zeros.
    let mut block_key = [0u8; SHA256_BLOCK_SIZE];
    if key.len() > SHA256_BLOCK_SIZE {
        let digest = Sha256::digest(key);
        block_key[..digest.len()].copy_from_slice(&digest);
    } else {
        block_key[..key.len()].copy_from_slice(key);
    }

    let mut ipad = [0x36u8; SHA256_BLOCK_SIZE];
    let mut opad = [0x5cu8; SHA256_BLOCK_SIZE];
    for i in 0..SHA256_BLOCK_SIZE {
        ipad[i] ^= block_key[i];
        opad[i] ^= block_key[i];
    }

    let inner = {
        let mut h = Sha256::new();
        h.update(ipad);
        h.update(message);
        h.finalize()
    };
    let outer = {
        let mut h = Sha256::new();
        h.update(opad);
        h.update(inner);
        h.finalize()
    };

    let mut out = [0u8; 32];
    out.copy_from_slice(&outer);
    out
}

/// Constant-time byte-slice equality.
///
/// Compares every byte regardless of where the first difference is, so the
/// time taken does not leak the position of a mismatch. The length check
/// short-circuits (lengths are not secret here — the mock's MAC is always
/// 32 bytes), but for equal-length inputs the loop is mismatch-position
/// independent.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}
