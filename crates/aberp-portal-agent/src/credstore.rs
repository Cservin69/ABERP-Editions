//! The WebAuthn credential store — public keys only, on the Mac.
//!
//! ADR-0113 §4.2:
//!
//! > Credential public keys + metadata live in the agent's own small
//! > store on the Mac (not in an ABERP tenant DB — the agent must work
//! > with ABERP stopped, and ADR-0002's tenant isolation is not for
//! > infrastructure state).
//!
//! Nothing here is secret: a credential record is a public key, a
//! credential id, a signature counter and a label. The private key
//! never leaves the Secure Enclave of the enrolled device, which is
//! precisely why "a relay compromise cannot read the credential store"
//! (§4.2) is worth stating — the store is not on the relay at all, and
//! even a stolen copy of this file authenticates nobody.
//!
//! The counter is the one mutable field: §4.3 requires a **sign-count
//! regression check**, so the store is read-modify-write on every
//! successful assertion.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::webauthn::cose::Es256PublicKey;

#[derive(Debug, thiserror::Error)]
pub enum CredStoreError {
    #[error("credential store {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("credential store {path} is not valid JSON: {source}")]
    Malformed {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("credential {id} has a malformed public key in the store")]
    BadKey { id: String },
}

/// One enrolled authenticator.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Credential {
    /// Credential id, base64url — as the browser reports `rawId`.
    pub id: String,
    /// P-256 affine X, hex.
    pub x: String,
    /// P-256 affine Y, hex.
    pub y: String,
    /// Last signature counter observed. `0` means "the authenticator
    /// does not maintain one" — Apple's platform authenticators
    /// famously always report 0, so the regression check has to treat
    /// that case as *no signal* rather than as a failure.
    pub sign_count: u32,
    /// Operator-facing label, set at enrolment (`iPhone`, `Mac`).
    pub label: String,
    /// RFC-3339 UTC.
    pub created_at: String,
}

impl Credential {
    /// Rebuild the parsed public key.
    pub fn public_key(&self) -> Result<Es256PublicKey, CredStoreError> {
        let bad = || CredStoreError::BadKey {
            id: self.id.clone(),
        };
        let x: [u8; 32] = hex::decode(&self.x)
            .map_err(|_| bad())?
            .try_into()
            .map_err(|_| bad())?;
        let y: [u8; 32] = hex::decode(&self.y)
            .map_err(|_| bad())?
            .try_into()
            .map_err(|_| bad())?;
        Ok(Es256PublicKey { x, y })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct StoreFile {
    #[serde(default)]
    credentials: Vec<Credential>,
}

/// The credential store, backed by one JSON file.
#[derive(Debug, Clone)]
pub struct CredentialStore {
    path: PathBuf,
}

impl CredentialStore {
    #[must_use]
    pub fn in_dir(state_dir: &Path) -> Self {
        Self {
            path: state_dir.join("credentials.json"),
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// All enrolled credentials. A missing file is an empty store — the
    /// day-zero state before the §4.3 console enrolment has run.
    ///
    /// A *malformed* file is an error, never an empty store: silently
    /// treating corruption as "nobody is enrolled" would re-open
    /// enrolment to whoever caused the corruption. Same posture as
    /// ADR-0088's "loud error on corruption, never silently re-minted".
    pub fn load(&self) -> Result<Vec<Credential>, CredStoreError> {
        match std::fs::read_to_string(&self.path) {
            Ok(raw) => {
                let f: StoreFile =
                    serde_json::from_str(&raw).map_err(|source| CredStoreError::Malformed {
                        path: self.path.clone(),
                        source,
                    })?;
                Ok(f.credentials)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(source) => Err(CredStoreError::Io {
                path: self.path.clone(),
                source,
            }),
        }
    }

    fn save(&self, credentials: &[Credential]) -> Result<(), CredStoreError> {
        let io = |source| CredStoreError::Io {
            path: self.path.clone(),
            source,
        };
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(io)?;
        }
        let body = serde_json::to_string_pretty(&StoreFile {
            credentials: credentials.to_vec(),
        })
        .map_err(|e| CredStoreError::Io {
            path: self.path.clone(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidData, e),
        })?;
        // Write-then-rename so a crash mid-write cannot leave a
        // half-written store, which `load` would (correctly, but
        // uselessly) refuse to parse.
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, body).map_err(io)?;
        restrict_permissions(&tmp);
        std::fs::rename(&tmp, &self.path).map_err(io)?;
        Ok(())
    }

    /// Add a credential. Replaces any record with the same id — a
    /// re-registration of the same authenticator is an update, not a
    /// duplicate.
    pub fn add(&self, credential: Credential) -> Result<(), CredStoreError> {
        let mut all = self.load()?;
        all.retain(|c| c.id != credential.id);
        all.push(credential);
        self.save(&all)
    }

    /// Look one up by base64url id.
    pub fn get(&self, id: &str) -> Result<Option<Credential>, CredStoreError> {
        Ok(self.load()?.into_iter().find(|c| c.id == id))
    }

    /// Record a new signature counter for `id`.
    pub fn update_sign_count(&self, id: &str, sign_count: u32) -> Result<(), CredStoreError> {
        let mut all = self.load()?;
        for c in &mut all {
            if c.id == id {
                c.sign_count = sign_count;
            }
        }
        self.save(&all)
    }

    /// Revoke one credential (§4.5 "revoke the phone's credential").
    /// Returns whether anything was removed.
    pub fn revoke(&self, id: &str) -> Result<bool, CredStoreError> {
        let mut all = self.load()?;
        let before = all.len();
        all.retain(|c| c.id != id);
        let removed = all.len() != before;
        self.save(&all)?;
        Ok(removed)
    }

    /// Revoke everything (§4.4's `revoke --all`, and the panic button
    /// after a device loss).
    pub fn revoke_all(&self) -> Result<usize, CredStoreError> {
        let n = self.load()?.len();
        self.save(&[])?;
        Ok(n)
    }
}

fn restrict_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("portal-cred-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).expect("temp dir");
        d
    }

    fn cred(id: &str) -> Credential {
        Credential {
            id: id.to_string(),
            x: hex::encode([1u8; 32]),
            y: hex::encode([2u8; 32]),
            sign_count: 0,
            label: "iPhone".into(),
            created_at: "2026-08-20T00:00:00Z".into(),
        }
    }

    #[test]
    fn missing_file_is_an_empty_store() {
        let dir = tmpdir("missing");
        assert!(CredentialStore::in_dir(&dir)
            .load()
            .expect("load")
            .is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn malformed_file_is_an_error_not_an_empty_store() {
        let dir = tmpdir("malformed");
        let s = CredentialStore::in_dir(&dir);
        std::fs::write(s.path(), "{ this is not json").expect("write");
        assert!(matches!(s.load(), Err(CredStoreError::Malformed { .. })));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn add_get_update_and_revoke_roundtrip() {
        let dir = tmpdir("roundtrip");
        let s = CredentialStore::in_dir(&dir);
        s.add(cred("cred-a")).expect("add");
        s.add(cred("cred-b")).expect("add");
        assert_eq!(s.load().expect("load").len(), 2);

        s.update_sign_count("cred-a", 42).expect("update");
        assert_eq!(
            s.get("cred-a").expect("get").expect("present").sign_count,
            42
        );
        assert_eq!(
            s.get("cred-b").expect("get").expect("present").sign_count,
            0
        );

        assert!(s.revoke("cred-a").expect("revoke"));
        assert!(!s.revoke("cred-a").expect("revoke again"));
        assert_eq!(s.load().expect("load").len(), 1);

        assert_eq!(s.revoke_all().expect("revoke all"), 1);
        assert!(s.load().expect("load").is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn re_registering_the_same_id_replaces_rather_than_duplicates() {
        let dir = tmpdir("replace");
        let s = CredentialStore::in_dir(&dir);
        s.add(cred("same")).expect("add");
        let mut second = cred("same");
        second.label = "Mac".into();
        s.add(second).expect("add again");
        let all = s.load().expect("load");
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].label, "Mac");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn public_key_rejects_a_corrupt_stored_coordinate() {
        let mut c = cred("bad");
        c.x = "not-hex".into();
        assert!(matches!(c.public_key(), Err(CredStoreError::BadKey { .. })));
    }
}
