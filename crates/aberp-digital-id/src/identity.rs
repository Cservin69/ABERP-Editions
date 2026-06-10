//! The resolved operator identity returned by a [`crate::DigitalIdProvider`].

use serde::{Deserialize, Serialize};

/// A resolved, authenticated operator identity.
///
/// Provider-defined: the `mock` backend mints a fixed stub; a real backend
/// (HU eID, US DoD CAC, …) populates this from a verified certificate /
/// assertion at sign-in time. Downstream (S346) an [`Option<DigitalIdRef>`]
/// derived from this rides inside future audit payloads.
///
/// [`Option<DigitalIdRef>`]: aberp-audit-ledger's `signer` field (S346).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DigitalId {
    /// Stable opaque identifier, provider-defined. For `mock` this is
    /// `"mock-op-001"`; for a CAC backend it would be the certificate's
    /// EDIPI / subject DN.
    pub id: String,
    /// Human-readable name, e.g. `"Ervin Áben"`. Display-only; never the
    /// authorisation key.
    pub display_name: String,
    /// Issuing authority tag, e.g. `"mock"`, `"hu-eid"`, `"us-dod-cac"`.
    pub issuer: String,
    /// Authorisation scopes carried by this identity, e.g.
    /// `["operator", "cui-cleared"]`. Empty for the bare mock operator.
    pub scope: Vec<String>,
    /// Unix-epoch milliseconds at which this identity was authenticated.
    /// The mock pins a fixed constant so the identity is deterministic;
    /// real backends stamp the wall-clock authentication time.
    pub issued_at_ms: u64,
}
