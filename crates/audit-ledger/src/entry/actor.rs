//! [`Actor`] — who produced this entry, per ADR-0008 §"Entry shape":
//! "session ID + user ID + capability set used".

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// Real authn lands in a later PR. PR-3 ships [`Actor::test_only`] for the
/// conformance test; producing entries from a real session is the job of
/// the billing module (PR-4) and the binary (PR-5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Actor {
    pub session_id: String,
    pub user_id: String,
    pub capabilities: BTreeSet<String>,
}

impl Actor {
    /// Fixed test actor for PR-3's conformance test. Not for use outside tests.
    pub fn test_only() -> Self {
        Self {
            session_id: "test-session".to_string(),
            user_id: "test-user".to_string(),
            capabilities: ["audit.append".to_string()].into_iter().collect(),
        }
    }

    /// Serialize to a stable string for DuckDB storage. The canonical CBOR
    /// encoder ([`crate::canonical`]) does not consult this — it walks the
    /// fields directly — so this is purely a storage convenience.
    pub(crate) fn to_storage_json(&self) -> String {
        serde_json::to_string(self).expect("Actor is always JSON-serializable")
    }

    pub(crate) fn from_storage_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }
}
