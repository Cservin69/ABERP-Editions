//! Enrolment — physical presence at the Mac is the credential
//! (ADR-0113 §4.3).
//!
//! > **Registration (enrolment):** disabled remotely, always. Enrolment
//! > runs only via a one-time, 10-minute, single-use URL minted **at the
//! > Mac's own console**.
//!
//! So there is no remote enrolment endpoint anywhere in this crate. The
//! browser can *complete* a ceremony, but only against a token that a
//! human already caused to exist by running `aberp-portal-agent enrol`
//! while sitting at the machine. That is what makes §4.5's recovery
//! story ("the Mac is the recovery") true rather than aspirational: no
//! email link, no recovery code, no remote fallback that would become
//! the weakest door.
//!
//! **Deviation from §4.3, flagged:** the ADR says the CLI "prints it as
//! a QR code". This prints the URL as text. The security properties —
//! console-only minting, 10-minute TTL, single use — are all
//! implemented; the QR is a convenience for pointing a phone camera at
//! the screen, and rendering one would mean taking a QR-encoder
//! dependency that `deny.toml`'s `unmaintained = "all"` scope would
//! have to be argued past. Worth doing; not worth doing silently as
//! part of this build.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::rand;

/// §4.3's window.
pub const ENROL_TTL_SECONDS: u64 = 10 * 60;

#[derive(Debug, thiserror::Error)]
pub enum EnrolError {
    #[error("minting the enrolment token: {0}")]
    Mint(#[from] rand::RandError),
    #[error("enrolment store {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("enrolment store {path} is not valid JSON: {source}")]
    Malformed {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("no enrolment is pending — run `aberp-portal-agent enrol` at the Mac")]
    NonePending,
    #[error("the enrolment window has closed — mint a new one at the Mac")]
    Expired,
    #[error("enrolment token does not match")]
    BadToken,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Pending {
    token: String,
    /// Unix seconds. Wall-clock rather than a monotonic instant because
    /// the token outlives the process that minted it.
    expires_at: u64,
    label: String,
}

/// The single pending enrolment, if any.
#[derive(Debug, Clone)]
pub struct EnrolStore {
    path: PathBuf,
}

impl EnrolStore {
    #[must_use]
    pub fn in_dir(state_dir: &Path) -> Self {
        Self {
            path: state_dir.join("enrol.pending.json"),
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Mint a one-time enrolment token for a device labelled `label`.
    ///
    /// Replaces any predecessor: there is at most one enrolment window
    /// open at a time, so a mistyped `enrol` invocation cannot leave a
    /// second live token behind.
    pub fn mint(&self, label: &str) -> Result<String, EnrolError> {
        let token = rand::token()?;
        let pending = Pending {
            token: token.clone(),
            expires_at: now_unix() + ENROL_TTL_SECONDS,
            label: label.to_string(),
        };
        let io = |source| EnrolError::Io {
            path: self.path.clone(),
            source,
        };
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(io)?;
        }
        let body = serde_json::to_string(&pending).map_err(|e| EnrolError::Io {
            path: self.path.clone(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidData, e),
        })?;
        std::fs::write(&self.path, body).map_err(io)?;
        restrict_permissions(&self.path);
        Ok(token)
    }

    /// Is an unexpired enrolment window open? Used by the shell to
    /// decide whether to offer the registration ceremony at all.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.read().is_ok_and(|p| p.expires_at > now_unix())
    }

    /// Validate and **consume** `token`. Single-use: the pending record
    /// is deleted before the ceremony result is returned, so a replay
    /// of the same URL — including one captured inside the 10-minute
    /// window — finds nothing to use.
    pub fn consume(&self, token: &str) -> Result<String, EnrolError> {
        let pending = self.read()?;
        if pending.expires_at <= now_unix() {
            self.clear();
            return Err(EnrolError::Expired);
        }
        if !aberp_portal_core::ct::eq(pending.token.as_bytes(), token.as_bytes()) {
            // Deliberately NOT cleared: a wrong guess must not let an
            // attacker cancel Ervin's legitimate open window.
            return Err(EnrolError::BadToken);
        }
        self.clear();
        Ok(pending.label)
    }

    /// Close any open window (the `enrol --cancel` path).
    pub fn clear(&self) {
        let _ = std::fs::remove_file(&self.path);
    }

    fn read(&self) -> Result<Pending, EnrolError> {
        match std::fs::read_to_string(&self.path) {
            Ok(raw) => serde_json::from_str(&raw).map_err(|source| EnrolError::Malformed {
                path: self.path.clone(),
                source,
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(EnrolError::NonePending),
            Err(source) => Err(EnrolError::Io {
                path: self.path.clone(),
                source,
            }),
        }
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
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
        let d = std::env::temp_dir().join(format!("portal-enrol-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).expect("temp dir");
        d
    }

    #[test]
    fn a_minted_token_is_consumable_exactly_once() {
        let dir = tmpdir("once");
        let s = EnrolStore::in_dir(&dir);
        let t = s.mint("iPhone").expect("mint");
        assert!(s.is_open());
        assert_eq!(s.consume(&t).expect("consume"), "iPhone");
        assert!(!s.is_open());
        assert!(matches!(s.consume(&t), Err(EnrolError::NonePending)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn with_no_pending_enrolment_nothing_can_register() {
        // The property that makes remote enrolment impossible.
        let dir = tmpdir("none");
        let s = EnrolStore::in_dir(&dir);
        assert!(!s.is_open());
        assert!(matches!(
            s.consume("anything"),
            Err(EnrolError::NonePending)
        ));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_wrong_token_is_refused_without_cancelling_the_open_window() {
        let dir = tmpdir("wrong");
        let s = EnrolStore::in_dir(&dir);
        let t = s.mint("Mac").expect("mint");
        assert!(matches!(s.consume("guess"), Err(EnrolError::BadToken)));
        assert!(s.is_open(), "a guess must not close Ervin's window");
        assert_eq!(s.consume(&t).expect("consume"), "Mac");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_expired_window_is_refused_and_cleared() {
        let dir = tmpdir("expired");
        let s = EnrolStore::in_dir(&dir);
        let t = s.mint("iPhone").expect("mint");
        // Rewrite the record with an expiry in the past — the same
        // state a token captured and replayed 11 minutes later meets.
        let expired = Pending {
            token: t.clone(),
            expires_at: now_unix() - 1,
            label: "iPhone".into(),
        };
        std::fs::write(s.path(), serde_json::to_string(&expired).expect("json")).expect("write");
        assert!(!s.is_open());
        assert!(matches!(s.consume(&t), Err(EnrolError::Expired)));
        assert!(matches!(s.consume(&t), Err(EnrolError::NonePending)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn minting_twice_leaves_only_the_newer_window_open() {
        let dir = tmpdir("twice");
        let s = EnrolStore::in_dir(&dir);
        let first = s.mint("iPhone").expect("mint");
        let second = s.mint("Mac").expect("mint");
        assert!(matches!(s.consume(&first), Err(EnrolError::BadToken)));
        assert_eq!(s.consume(&second).expect("consume"), "Mac");
        std::fs::remove_dir_all(&dir).ok();
    }
}
