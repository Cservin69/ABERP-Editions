//! Enrolment — physical presence at the Mac is the credential
//! (ADR-0115 §4.3).
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

use crate::credstore::Credential;
use crate::rand;

/// §4.3's window.
pub const ENROL_TTL_SECONDS: u64 = 10 * 60;

/// How long a verified-but-unconfirmed credential waits at the console.
///
/// Long enough for Ervin to walk to the Mac, short enough that a
/// staged credential is not left sitting there overnight for someone
/// else to confirm.
pub const CONFIRM_TTL_SECONDS: u64 = 10 * 60;

/// Length of the confirmation code the operator types.
///
/// Eight hex characters — 32 bits — is not a secret and is not doing
/// cryptographic work: an attacker who could reach this code could
/// already reach the console it is typed at. It is there so the
/// operator confirms *the enrolment in front of them* rather than
/// blindly approving whatever happens to be staged, and so two
/// enrolments cannot be confused for one another.
pub const CONFIRM_CODE_CHARS: usize = 8;

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
    #[error("no credential is waiting for confirmation at this Mac")]
    NoneStaged,
    #[error("the confirmation window has closed — start the enrolment again")]
    ConfirmExpired,
    #[error("that confirmation code does not match the credential waiting at this Mac")]
    BadConfirmCode,
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

/// A credential that has passed every cryptographic check and is
/// waiting for a human at the Mac to say yes (ADR-0115 §4.3b).
///
/// # Why this step exists
///
/// Everything upstream of it is a check a *machine* performs, and each
/// one can be satisfied by an attacker who has what it asks for. The
/// enrolment token is single-use and console-minted — but until
/// hardening H1 it crosses relay memory in plaintext, so a compromised
/// relay can see a live one. Apple attestation proves the key lives in
/// a Secure Enclave — but it does not prove it is *Ervin's* Secure
/// Enclave; an attacker with a stolen token and any iPhone satisfies
/// it.
///
/// This step asks a different question, and it is the one no remote
/// attacker can answer: **is a human standing at the Mac right now who
/// meant to do this?** Enrolment is the only operation in the whole
/// design that grants standing access, so it is the one worth spending
/// a walk to the console on.
///
/// The confirmation code is not a secret and does not need to be one.
/// Its job is to make the operator confirm the specific enrolment in
/// front of them: a second, silent enrolment staged by an attacker has
/// a different code, so approving one does not approve the other.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StagedCredential {
    /// The code the operator types. Derived from the credential id, so
    /// it is reproducible and can be shown on both screens.
    pub code: String,
    pub credential: Credential,
    /// Unix seconds.
    pub expires_at: u64,
}

/// The single credential awaiting console confirmation, if any.
#[derive(Debug, Clone)]
pub struct StagingStore {
    path: PathBuf,
}

impl StagingStore {
    #[must_use]
    pub fn in_dir(state_dir: &Path) -> Self {
        Self {
            path: state_dir.join("enrol.staged.json"),
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The confirmation code for a credential id. Deterministic, so the
    /// daemon, the console and the browser all derive the same string
    /// from the same credential without passing it around.
    #[must_use]
    pub fn code_for(credential_id: &str) -> String {
        use sha2::Digest as _;
        let digest = sha2::Sha256::digest(credential_id.as_bytes());
        hex::encode(digest)[..CONFIRM_CODE_CHARS].to_string()
    }

    /// Stage a verified credential and return the code to display.
    ///
    /// Replaces any predecessor: at most one credential is ever waiting,
    /// so an attacker cannot queue one behind a legitimate enrolment
    /// and have the operator's single confirmation commit both.
    pub fn stage(&self, credential: Credential) -> Result<String, EnrolError> {
        let code = Self::code_for(&credential.id);
        let staged = StagedCredential {
            code: code.clone(),
            credential,
            expires_at: now_unix() + CONFIRM_TTL_SECONDS,
        };
        let io = |source| EnrolError::Io {
            path: self.path.clone(),
            source,
        };
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(io)?;
        }
        let body = serde_json::to_string(&staged).map_err(|e| EnrolError::Io {
            path: self.path.clone(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidData, e),
        })?;
        std::fs::write(&self.path, body).map_err(io)?;
        restrict_permissions(&self.path);
        Ok(code)
    }

    /// What is waiting, for the console to display.
    pub fn peek(&self) -> Result<StagedCredential, EnrolError> {
        let staged = self.read()?;
        if staged.expires_at <= now_unix() {
            self.clear();
            return Err(EnrolError::ConfirmExpired);
        }
        Ok(staged)
    }

    /// Validate `code` and hand back the credential to commit.
    ///
    /// Single-use, and constant-time on the code so a mistyped digit
    /// leaks nothing about the rest of it — cheap, and the alternative
    /// is arguing about whether it matters.
    pub fn confirm(&self, code: &str) -> Result<Credential, EnrolError> {
        let staged = self.peek()?;
        if !aberp_portal_core::ct::eq(
            staged.code.as_bytes(),
            code.trim().to_ascii_lowercase().as_bytes(),
        ) {
            // Deliberately NOT cleared: a wrong guess must not let an
            // attacker cancel a legitimate staged enrolment.
            return Err(EnrolError::BadConfirmCode);
        }
        self.clear();
        Ok(staged.credential)
    }

    /// Discard whatever is staged (the `--reject` path).
    pub fn clear(&self) {
        let _ = std::fs::remove_file(&self.path);
    }

    fn read(&self) -> Result<StagedCredential, EnrolError> {
        match std::fs::read_to_string(&self.path) {
            Ok(raw) => serde_json::from_str(&raw).map_err(|source| EnrolError::Malformed {
                path: self.path.clone(),
                source,
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(EnrolError::NoneStaged),
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

    fn credential(id: &str) -> Credential {
        Credential {
            id: id.to_string(),
            x: "00".into(),
            y: "00".into(),
            sign_count: 0,
            label: "iPhone".into(),
            created_at: "2026-08-21T00:00:00Z".into(),
        }
    }

    #[test]
    fn a_staged_credential_is_committed_only_by_its_own_code() {
        let dir = tmpdir("stage");
        let s = StagingStore::in_dir(&dir);
        let code = s.stage(credential("cred-a")).expect("stage");
        assert_eq!(code.len(), CONFIRM_CODE_CHARS);
        assert_eq!(code, StagingStore::code_for("cred-a"), "deterministic");

        assert!(matches!(
            s.confirm("deadbeef"),
            Err(EnrolError::BadConfirmCode)
        ));
        assert!(
            s.peek().is_ok(),
            "a wrong code must not discard the staging"
        );
        assert_eq!(s.confirm(&code).expect("confirm").id, "cred-a");
        assert!(
            matches!(s.peek(), Err(EnrolError::NoneStaged)),
            "single use"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn with_nothing_staged_nothing_can_be_confirmed() {
        // The property that makes silent remote enrolment impossible:
        // no console step, no credential.
        let dir = tmpdir("nostage");
        let s = StagingStore::in_dir(&dir);
        assert!(matches!(s.confirm("anything"), Err(EnrolError::NoneStaged)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_second_staging_replaces_the_first_rather_than_queueing() {
        // Otherwise an attacker could stage one behind a legitimate
        // enrolment and have a single confirmation commit both.
        let dir = tmpdir("restage");
        let s = StagingStore::in_dir(&dir);
        let first = s.stage(credential("cred-a")).expect("stage");
        let second = s.stage(credential("cred-b")).expect("stage");
        assert_ne!(first, second);
        assert!(matches!(s.confirm(&first), Err(EnrolError::BadConfirmCode)));
        assert_eq!(s.confirm(&second).expect("confirm").id, "cred-b");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_expired_staging_is_refused_and_cleared() {
        let dir = tmpdir("stale-stage");
        let s = StagingStore::in_dir(&dir);
        let code = s.stage(credential("cred-a")).expect("stage");
        let expired = StagedCredential {
            code: code.clone(),
            credential: credential("cred-a"),
            expires_at: now_unix() - 1,
        };
        std::fs::write(s.path(), serde_json::to_string(&expired).expect("json")).expect("write");
        assert!(matches!(s.confirm(&code), Err(EnrolError::ConfirmExpired)));
        assert!(matches!(s.peek(), Err(EnrolError::NoneStaged)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_confirmation_code_is_typed_case_insensitively() {
        // It is read off one screen and typed at another; a case
        // mismatch is a support call, not a security boundary.
        let dir = tmpdir("case");
        let s = StagingStore::in_dir(&dir);
        let code = s.stage(credential("cred-a")).expect("stage");
        assert!(s
            .confirm(&format!("  {}  ", code.to_ascii_uppercase()))
            .is_ok());
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
