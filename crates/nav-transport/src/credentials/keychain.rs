//! OS-keychain reader for the NAV credential artifacts.
//!
//! # PR-57 / session-77 — consolidated blob (one keychain item)
//!
//! The four NAV credential artifacts (login + password + xmlSignKey +
//! xmlChangeKey) used to live as four separate keychain items, which
//! meant a freshly-rebuilt binary paid four ACL prompts on first boot
//! (the macOS keychain re-prompts on a changed binary signature; the
//! prompt count scales with the number of items touched). PR-57
//! consolidates them into a single JSON-encoded item,
//! [`ITEM_NAV_CREDENTIALS_BLOB`], so the boot path costs ONE prompt
//! for the four artifacts (plus one for the session token, which has
//! a different lifecycle and stays a separate item).
//!
//! The legacy per-artifact item constants ([`ITEM_LOGIN`],
//! [`ITEM_PASSWORD`], [`ITEM_SIGN_KEY`], [`ITEM_CHANGE_KEY`]) remain
//! public so the migration path can read existing entries from
//! installations populated under the pre-PR-57 model. The
//! [`load_blob_with_legacy_migration`] helper handles both paths
//! transparently: blob present → one read; blob absent + legacy present
//! → migrate then return; everything absent → typed
//! `KeychainItemMissing` (NeedsSetup boot state).
//!
//! Service-and-account naming convention (stable across platforms via
//! the `keyring` crate's abstraction; per ADR-0007 §Secrets and
//! ADR-0020 §3):
//!
//!   service:  `aberp.nav.<tenant_id>`
//!   account:  [`ITEM_NAV_CREDENTIALS_BLOB`] (post-PR-57), or one of
//!             the four legacy items during the migration window.
//!
//! On macOS this maps to the system keychain "Where" + "Account"
//! fields, viewable via `security find-generic-password -s
//! "aberp.nav.<tenant>"`. On Linux/SecretService and Windows
//! Credential Manager the mapping is analogous and handled by
//! `keyring`.

use keyring::Entry;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::error::NavTransportError;

// ----- item name constants -----------------------------------------
//
// Named here (not inlined as string literals at call sites) so a
// future rename is a single point-of-edit and a grep across the repo
// finds every reference. The values themselves are part of the
// on-disk contract with the operator's keychain and must NOT change
// silently — a rename effectively orphans the operator's existing
// keychain entries, which is a tooling-affecting change.

/// PR-57 / session-77 — consolidated keychain item that holds all
/// four NAV credential artifacts as a single JSON blob. One item =
/// one ACL prompt per fresh-build boot.
pub const ITEM_NAV_CREDENTIALS_BLOB: &str = "nav_credentials_blob";

/// Account name for the technical-user login (operator-visible string).
///
/// **Legacy item** — pre-PR-57 per-artifact storage. Still read at
/// boot time IFF [`ITEM_NAV_CREDENTIALS_BLOB`] is absent (migration
/// path). New installations write the blob directly and never touch
/// this item.
pub const ITEM_LOGIN: &str = "technical_user.login";

/// Account name for the technical-user password (plaintext at rest).
///
/// **Legacy item** — same posture as [`ITEM_LOGIN`].
pub const ITEM_PASSWORD: &str = "technical_user.password";

/// Account name for the `xmlSignKey` per ADR-0009 §4.
///
/// **Legacy item** — same posture as [`ITEM_LOGIN`].
pub const ITEM_SIGN_KEY: &str = "xml_sign_key";

/// Account name for the `xmlChangeKey` per ADR-0009 §4.
///
/// **Legacy item** — same posture as [`ITEM_LOGIN`].
pub const ITEM_CHANGE_KEY: &str = "xml_change_key";

/// PR-57 / session-77 — the four legacy per-artifact item names in a
/// single slice. Used by [`delete_legacy_items`] and the migration
/// path so a future contributor adding a fifth artifact doesn't have
/// to remember to update three places.
pub const LEGACY_ITEMS: [&str; 4] = [ITEM_LOGIN, ITEM_PASSWORD, ITEM_SIGN_KEY, ITEM_CHANGE_KEY];

/// Compose the keychain `service` field for a tenant. Public so the
/// operator-tooling (a future PR) and the unit test can agree on the
/// naming without duplicating the format string.
pub fn service_name(tenant_id: &str) -> String {
    format!("aberp.nav.{tenant_id}")
}

/// PR-57 / session-77 — the JSON shape that lives inside the
/// consolidated [`ITEM_NAV_CREDENTIALS_BLOB`] keychain entry. Field
/// names match the legacy per-artifact account names verbatim so a
/// grep across the repo for either the JSON key or the legacy item
/// constant finds the same set of references.
///
/// The struct is `pub(crate)` because callers should go through
/// [`load_blob_with_legacy_migration`] or [`write_blob`] rather than
/// touching the JSON encoding directly — the encoding is an
/// implementation detail of this module.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct NavCredentialsBlob {
    #[serde(rename = "technical_user.login")]
    pub login: String,
    #[serde(rename = "technical_user.password")]
    pub password: String,
    pub xml_sign_key: String,
    pub xml_change_key: String,
}

/// PR-57 / session-77 — opaque wrapper around a freshly-loaded
/// `NavCredentialsBlob` that zeroizes the four field bytes on drop.
/// Exposed to the credentials module so it can hoist the four values
/// into the existing `Zeroizing<String>` slots on `NavCredentials`.
pub(crate) struct LoadedBlob {
    pub login: Zeroizing<String>,
    pub password: Zeroizing<String>,
    pub sign_key: Zeroizing<String>,
    pub change_key: Zeroizing<String>,
}

/// Read one secret from the keychain. The secret is wrapped in
/// `Zeroizing<String>` so the buffer is overwritten on drop.
///
/// Two distinct failure modes are returned as distinct typed errors:
///
///   1. `NavTransportError::KeychainItemMissing` — the keychain backend
///      reports the entry doesn't exist. Operator action: populate.
///   2. `NavTransportError::KeychainBackend`     — the backend itself
///      errored (locked keychain, permission denied, unsupported
///      platform). Operator action: triage the underlying error.
///
/// CLAUDE.md rule 12 (fail loud): there is NO third path that returns
/// an empty string or a default. Missing means missing.
pub fn read_secret(
    tenant_id: &str,
    item: &'static str,
) -> Result<Zeroizing<String>, NavTransportError> {
    let service = service_name(tenant_id);
    let entry = Entry::new(&service, item)
        .map_err(|e| NavTransportError::KeychainBackend { item, source: e })?;
    match entry.get_password() {
        Ok(secret) => Ok(Zeroizing::new(secret)),
        Err(keyring::Error::NoEntry) => Err(NavTransportError::KeychainItemMissing {
            tenant_id: tenant_id.to_string(),
            item,
        }),
        Err(other) => Err(NavTransportError::KeychainBackend {
            item,
            source: other,
        }),
    }
}

/// PR-57 / session-77 — write the consolidated JSON blob.
///
/// Public so `apps/aberp/src/setup_nav_credentials.rs` and
/// `apps/aberp/src/serve.rs::rotate_nav_credential_request` (the two
/// write surfaces) can both share the one serialization path. Returns
/// the typed `KeychainBackend` error on any backend failure (CLAUDE.md
/// rule 12 — loud).
pub fn write_blob(
    tenant_id: &str,
    login: &str,
    password: &str,
    xml_sign_key: &str,
    xml_change_key: &str,
) -> Result<(), NavTransportError> {
    let blob = NavCredentialsBlob {
        login: login.to_string(),
        password: password.to_string(),
        xml_sign_key: xml_sign_key.to_string(),
        xml_change_key: xml_change_key.to_string(),
    };
    let json =
        serde_json::to_string(&blob).map_err(|e| NavTransportError::KeychainBlobMalformed {
            tenant_id: tenant_id.to_string(),
            detail: format!("serialize NAV credentials blob to JSON: {e}"),
        })?;
    let service = service_name(tenant_id);
    let entry = Entry::new(&service, ITEM_NAV_CREDENTIALS_BLOB).map_err(|e| {
        NavTransportError::KeychainBackend {
            item: ITEM_NAV_CREDENTIALS_BLOB,
            source: e,
        }
    })?;
    entry
        .set_password(&json)
        .map_err(|e| NavTransportError::KeychainBackend {
            item: ITEM_NAV_CREDENTIALS_BLOB,
            source: e,
        })
}

/// PR-57 / session-77 — read the consolidated JSON blob and split it
/// into the four `Zeroizing<String>` slots `NavCredentials` consumes.
/// `Ok(None)` means the blob entry is absent (the caller falls through
/// to the legacy-migration path); `Err` means the backend failed OR
/// the blob is malformed (both loud per rule 12).
pub(crate) fn read_blob(tenant_id: &str) -> Result<Option<LoadedBlob>, NavTransportError> {
    let service = service_name(tenant_id);
    let entry = Entry::new(&service, ITEM_NAV_CREDENTIALS_BLOB).map_err(|e| {
        NavTransportError::KeychainBackend {
            item: ITEM_NAV_CREDENTIALS_BLOB,
            source: e,
        }
    })?;
    let raw = match entry.get_password() {
        Ok(s) => Zeroizing::new(s),
        Err(keyring::Error::NoEntry) => return Ok(None),
        Err(other) => {
            return Err(NavTransportError::KeychainBackend {
                item: ITEM_NAV_CREDENTIALS_BLOB,
                source: other,
            });
        }
    };
    let parsed: NavCredentialsBlob = serde_json::from_str(raw.as_str()).map_err(|e| {
        NavTransportError::KeychainBlobMalformed {
            tenant_id: tenant_id.to_string(),
            detail: format!("parse NAV credentials blob JSON: {e}"),
        }
    })?;
    Ok(Some(LoadedBlob {
        login: Zeroizing::new(parsed.login),
        password: Zeroizing::new(parsed.password),
        sign_key: Zeroizing::new(parsed.xml_sign_key),
        change_key: Zeroizing::new(parsed.xml_change_key),
    }))
}

/// PR-57 / session-77 — delete one of the four legacy per-artifact
/// keychain entries. Returns `Ok(true)` if a delete happened, `Ok(false)`
/// if the entry was absent (idempotent migration), `Err` for any other
/// backend failure.
pub(crate) fn delete_legacy_item(
    tenant_id: &str,
    item: &'static str,
) -> Result<bool, NavTransportError> {
    let service = service_name(tenant_id);
    let entry = Entry::new(&service, item)
        .map_err(|e| NavTransportError::KeychainBackend { item, source: e })?;
    match entry.delete_password() {
        Ok(()) => Ok(true),
        Err(keyring::Error::NoEntry) => Ok(false),
        Err(other) => Err(NavTransportError::KeychainBackend {
            item,
            source: other,
        }),
    }
}

/// PR-57 / session-77 — read all four legacy per-artifact entries and
/// return them packaged in [`LoadedBlob`] form. Used only by the
/// migration path. Returns `KeychainItemMissing` on the first missing
/// artifact (NeedsSetup semantics — partial loading is refused per
/// CLAUDE.md rule 12).
pub(crate) fn read_legacy_artifacts(tenant_id: &str) -> Result<LoadedBlob, NavTransportError> {
    let login = read_secret(tenant_id, ITEM_LOGIN)?;
    let password = read_secret(tenant_id, ITEM_PASSWORD)?;
    let sign_key = read_secret(tenant_id, ITEM_SIGN_KEY)?;
    let change_key = read_secret(tenant_id, ITEM_CHANGE_KEY)?;
    Ok(LoadedBlob {
        login,
        password,
        sign_key,
        change_key,
    })
}

/// PR-57 / session-77 — best-effort cleanup of the four legacy
/// per-artifact entries. The blob is the authoritative source; lingering
/// legacy entries are dormant but waste a keychain-ACL re-prompt slot
/// on the next rebuild, so we delete them after a successful blob write
/// or migration. Backend failures are logged via `tracing` rather than
/// propagated — a delete failure does NOT undo a successful blob write,
/// and the operator can clear stragglers manually via `security delete-
/// generic-password -s "aberp.nav.<tenant>" -a "<legacy-item>"`.
pub fn delete_legacy_items_best_effort(tenant_id: &str) {
    for item in LEGACY_ITEMS {
        match delete_legacy_item(tenant_id, item) {
            Ok(true) => {
                tracing::info!(
                    tenant = tenant_id,
                    item,
                    "deleted legacy NAV-credentials keychain entry (consolidated into nav_credentials_blob)"
                );
            }
            Ok(false) => {
                // Already absent — no-op, no log.
            }
            Err(e) => {
                tracing::warn!(
                    tenant = tenant_id,
                    item,
                    error = %e,
                    "failed to delete legacy NAV-credentials keychain entry; \
                     blob is authoritative, operator may clear via `security delete-generic-password`"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Naming-convention guard. If a future contributor changes the
    /// `service_name` format, this test fails — and the rename is a
    /// breaking change for any tenant whose keychain is already
    /// populated, so the failure forces the discussion.
    #[test]
    fn service_name_format_is_stable() {
        assert_eq!(service_name("acme"), "aberp.nav.acme");
        assert_eq!(service_name("t-uuid-1234"), "aberp.nav.t-uuid-1234");
    }

    /// Item-name guard. Same reasoning as `service_name_format_is_stable`
    /// — the strings are part of the on-disk operator contract.
    #[test]
    fn item_names_are_stable() {
        assert_eq!(ITEM_NAV_CREDENTIALS_BLOB, "nav_credentials_blob");
        assert_eq!(ITEM_LOGIN, "technical_user.login");
        assert_eq!(ITEM_PASSWORD, "technical_user.password");
        assert_eq!(ITEM_SIGN_KEY, "xml_sign_key");
        assert_eq!(ITEM_CHANGE_KEY, "xml_change_key");
    }

    /// PR-57 / session-77 — round-trip the JSON shape so a future
    /// contributor renaming a field of `NavCredentialsBlob` without
    /// updating the `#[serde(rename)]` attributes breaks the test
    /// before it breaks an operator's keychain.
    #[test]
    fn blob_json_field_names_match_legacy_items() {
        let blob = NavCredentialsBlob {
            login: "lg".to_string(),
            password: "pw".to_string(),
            xml_sign_key: "sk".to_string(),
            xml_change_key: "ck".to_string(),
        };
        let json = serde_json::to_string(&blob).unwrap();
        // Field names match legacy item constants verbatim.
        assert!(
            json.contains(r#""technical_user.login":"lg""#),
            "json = {json}"
        );
        assert!(
            json.contains(r#""technical_user.password":"pw""#),
            "json = {json}"
        );
        assert!(json.contains(r#""xml_sign_key":"sk""#), "json = {json}");
        assert!(json.contains(r#""xml_change_key":"ck""#), "json = {json}");

        let parsed: NavCredentialsBlob = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.login, "lg");
        assert_eq!(parsed.password, "pw");
        assert_eq!(parsed.xml_sign_key, "sk");
        assert_eq!(parsed.xml_change_key, "ck");
    }
}
