//! The pre-auth gate token (ADR-0113 §3.3, Ervin's §9.3 decision (b)).
//!
//! > The token is **not** an authenticator — it only decides *whether
//! > the door is even visible*; WebAuthn remains the lock.
//!
//! It is minted, persisted and rotated **here, on the Mac**, and pushed
//! to the relay in the [`aberp_portal_core::Frame::Hello`] of every
//! connection. Two consequences fall out of that placement, both
//! wanted:
//!
//! - the relay never has a knock token at rest (§2.4), only for the
//!   life of a connection it did not initiate;
//! - when the tunnel is down the relay has no token at all, so *every*
//!   request — including a correctly-bookmarked one from Ervin — gets
//!   the uniform 404. That is §5.3's "Mac down → nothing", achieved by
//!   construction rather than by a check someone could forget.
//!
//! Rotation is `aberp-portal-agent rotate-knock`: mint, persist,
//! reconnect. The old bookmark stops working the moment the new
//! `Hello` lands, which is the §7 answer to "knock token leaked via
//! bookmark sync".

use std::path::{Path, PathBuf};

use crate::rand;

#[derive(Debug, thiserror::Error)]
pub enum KnockError {
    #[error("minting the knock token: {0}")]
    Mint(#[from] rand::RandError),
    #[error("knock token store {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// The on-disk knock token, owned by the agent.
#[derive(Debug, Clone)]
pub struct KnockStore {
    path: PathBuf,
}

impl KnockStore {
    #[must_use]
    pub fn in_dir(state_dir: &Path) -> Self {
        Self {
            path: state_dir.join("knock.token"),
        }
    }

    /// Read the current token, minting one on first run.
    pub fn load_or_mint(&self) -> Result<String, KnockError> {
        match std::fs::read_to_string(&self.path) {
            Ok(s) if !s.trim().is_empty() => Ok(s.trim().to_string()),
            Ok(_) => self.rotate(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => self.rotate(),
            Err(source) => Err(KnockError::Io {
                path: self.path.clone(),
                source,
            }),
        }
    }

    /// Mint a fresh token and persist it, replacing any predecessor.
    pub fn rotate(&self) -> Result<String, KnockError> {
        let token = rand::token()?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| KnockError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        std::fs::write(&self.path, format!("{token}\n")).map_err(|source| KnockError::Io {
            path: self.path.clone(),
            source,
        })?;
        restrict_permissions(&self.path);
        Ok(token)
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// `chmod 0600` on Unix. Belt to the braces: the file is inside the
/// operator's home directory on a single-user Mac, and the token is a
/// visibility gate rather than an authenticator — but a world-readable
/// secret-shaped file is still the wrong default.
fn restrict_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
            tracing::warn!(error = %e, path = %path.display(), "could not restrict permissions on the knock token file");
        }
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
        let d = std::env::temp_dir().join(format!("portal-knock-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).expect("temp dir");
        d
    }

    #[test]
    fn first_run_mints_and_second_run_reuses() {
        let dir = tmpdir("mint");
        let store = KnockStore::in_dir(&dir);
        let a = store.load_or_mint().expect("mint");
        let b = store.load_or_mint().expect("load");
        assert_eq!(a, b, "a restart must not invalidate the bookmark");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rotation_replaces_the_token() {
        let dir = tmpdir("rotate");
        let store = KnockStore::in_dir(&dir);
        let a = store.load_or_mint().expect("mint");
        let b = store.rotate().expect("rotate");
        assert_ne!(a, b);
        assert_eq!(store.load_or_mint().expect("load"), b);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_empty_file_is_treated_as_no_token_and_re_minted() {
        // A truncated write (disk full, crash) must not leave the portal
        // permanently gated on the empty string.
        let dir = tmpdir("empty");
        let store = KnockStore::in_dir(&dir);
        std::fs::write(store.path(), "   \n").expect("write");
        let t = store.load_or_mint().expect("re-mint");
        assert!(!t.trim().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn minted_token_file_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tmpdir("perm");
        let store = KnockStore::in_dir(&dir);
        store.load_or_mint().expect("mint");
        let mode = std::fs::metadata(store.path())
            .expect("stat")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o077,
            0,
            "knock token file is group/world accessible"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
