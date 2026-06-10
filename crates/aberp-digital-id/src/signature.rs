//! An algorithm-tagged signature produced by a [`crate::DigitalIdProvider`].

use serde::{Deserialize, Serialize};

/// A signature over arbitrary payload bytes.
///
/// The `algorithm` tag is load-bearing: a verifier checks it before
/// recomputing, so a `mock-hmac-sha256` signature can never be silently
/// accepted by an `ecdsa-p256` verifier (or vice versa). The `signer_id`
/// links the signature back to the [`crate::DigitalId::id`] that produced
/// it, so a downstream audit consumer can correlate signature → identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signature {
    /// Algorithm tag, e.g. `"mock-hmac-sha256"`, `"ecdsa-p256"`. A verifier
    /// rejects a signature whose tag it does not implement.
    pub algorithm: String,
    /// Raw signature bytes. For the mock this is the 32-byte HMAC-SHA256
    /// output; for a real backend it is the DER/raw signature.
    pub bytes: Vec<u8>,
    /// The [`crate::DigitalId::id`] of the signer.
    pub signer_id: String,
    /// Unix-epoch milliseconds at which the signature was produced. The
    /// mock pins a fixed constant for determinism; real backends stamp the
    /// wall clock. Not an input to verification — it is provenance metadata.
    pub signed_at_ms: u64,
}
