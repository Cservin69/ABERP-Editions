//! Portal sessions — minted, held and expired **on the Mac**
//! (ADR-0113 §4.4).
//!
//! > Agent-minted, short-lived, scoped tokens: 15-minute idle timeout,
//! > 8-hour absolute cap, bound to the front connection that carried
//! > the ceremony (a stolen cookie replayed through a new connection
//! > fails), delivered as `Secure; HttpOnly; SameSite=Strict`,
//! > revocable at the agent […] No refresh tokens.
//!
//! The tunnel binding is the interesting one. Every session records the
//! `tunnel_id` of the Leg-B connection it was minted over
//! ([`aberp_portal_core::Frame::Hello`]). A reconnect mints a new
//! tunnel id, so every session dies with the tunnel — which also means
//! a cookie exfiltrated from the relay's memory (the §2.4 residual)
//! stops working the moment the tunnel flaps, and cannot be carried to
//! a different relay at all.
//!
//! Sessions live in memory only. A daemon restart logs Ervin out, which
//! §4.4 already accepts: "a lapsed session is one Face ID glance away
//! from a new one".

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::rand;

/// Cookie name. Deliberately generic — a cookie called
/// `aberp_portal_session` would be a fingerprint if it ever escaped
/// into a response the unauthenticated can see (§3.2).
pub const COOKIE_NAME: &str = "s";

/// §4.4's idle timeout.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// §4.4's absolute cap.
pub const ABSOLUTE_CAP: Duration = Duration::from_secs(8 * 60 * 60);

#[derive(Debug, Clone)]
struct Session {
    tunnel_id: String,
    created: Instant,
    last_seen: Instant,
}

/// The agent's session table.
#[derive(Debug, Default)]
pub struct SessionStore {
    sessions: Mutex<HashMap<String, Session>>,
}

impl SessionStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Mint a session bound to `tunnel_id`. Returns the raw token; the
    /// caller wraps it in a `Set-Cookie` via [`cookie_header`].
    pub fn mint(&self, tunnel_id: &str) -> Result<String, rand::RandError> {
        let token = rand::token()?;
        let now = Instant::now();
        let mut g = self.lock();
        g.retain(|_, s| !expired(s, now));
        g.insert(
            token.clone(),
            Session {
                tunnel_id: tunnel_id.to_string(),
                created: now,
                last_seen: now,
            },
        );
        Ok(token)
    }

    /// Validate the browser's `Cookie` header against `tunnel_id`,
    /// refreshing the idle clock on success.
    #[must_use]
    pub fn validate(&self, cookie_header: Option<&str>, tunnel_id: &str) -> bool {
        let Some(token) = cookie_header.and_then(extract_cookie) else {
            return false;
        };
        let now = Instant::now();
        let mut g = self.lock();
        // Constant-time lookup is not attempted here: the token is a
        // 256-bit random map key, and a hash lookup leaks nothing an
        // attacker can steer. The tokens that ARE compared bytewise
        // (knock, enrolment) go through `aberp_portal_core::ct`.
        let Some(s) = g.get_mut(&token) else {
            return false;
        };
        if expired(s, now) || s.tunnel_id != tunnel_id {
            g.remove(&token);
            return false;
        }
        s.last_seen = now;
        true
    }

    /// §4.4's `revoke --all`. Returns how many were dropped.
    pub fn revoke_all(&self) -> usize {
        let mut g = self.lock();
        let n = g.len();
        g.clear();
        n
    }

    /// Drop every session bound to a tunnel that has gone away.
    pub fn revoke_tunnel(&self, tunnel_id: &str) {
        self.lock().retain(|_, s| s.tunnel_id != tunnel_id);
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Session>> {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn expired(s: &Session, now: Instant) -> bool {
    now.duration_since(s.last_seen) > IDLE_TIMEOUT || now.duration_since(s.created) > ABSOLUTE_CAP
}

/// Pull our cookie out of a raw `Cookie:` header.
fn extract_cookie(header: &str) -> Option<String> {
    header.split(';').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k.trim() == COOKIE_NAME).then(|| v.trim().to_string())
    })
}

/// Build the `Set-Cookie` value. §4.4 fixes every attribute;
/// `secure` is a parameter only so the loopback end-to-end test can
/// run over `http://127.0.0.1` — production never clears it (see
/// `config::AgentConfig::cookie_secure`, which is opt-out).
#[must_use]
pub fn cookie_header(token: &str, secure: bool) -> String {
    let mut v = format!("{COOKIE_NAME}={token}; HttpOnly; SameSite=Strict; Path=/");
    if secure {
        v.push_str("; Secure");
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minted_session_validates_on_its_own_tunnel() {
        let s = SessionStore::new();
        let t = s.mint("tunnel-1").expect("mint");
        assert!(s.validate(Some(&format!("{COOKIE_NAME}={t}")), "tunnel-1"));
    }

    #[test]
    fn a_session_replayed_through_a_different_tunnel_fails() {
        // §4.4's headline property, and the thing that bounds the §2.4
        // plaintext-in-relay residual.
        let s = SessionStore::new();
        let t = s.mint("tunnel-1").expect("mint");
        assert!(!s.validate(Some(&format!("{COOKIE_NAME}={t}")), "tunnel-2"));
        // And it is dropped, not merely rejected once.
        assert!(!s.validate(Some(&format!("{COOKIE_NAME}={t}")), "tunnel-1"));
    }

    #[test]
    fn no_cookie_and_unknown_cookie_both_fail() {
        let s = SessionStore::new();
        assert!(!s.validate(None, "tunnel-1"));
        assert!(!s.validate(Some("s=made-up"), "tunnel-1"));
        assert!(!s.validate(Some("other=value"), "tunnel-1"));
    }

    #[test]
    fn revoking_a_tunnel_drops_only_its_sessions() {
        let s = SessionStore::new();
        let a = s.mint("tunnel-a").expect("mint");
        let b = s.mint("tunnel-b").expect("mint");
        s.revoke_tunnel("tunnel-a");
        assert!(!s.validate(Some(&format!("{COOKIE_NAME}={a}")), "tunnel-a"));
        assert!(s.validate(Some(&format!("{COOKIE_NAME}={b}")), "tunnel-b"));
    }

    #[test]
    fn revoke_all_empties_the_table() {
        let s = SessionStore::new();
        let t = s.mint("tunnel-1").expect("mint");
        assert_eq!(s.revoke_all(), 1);
        assert!(s.is_empty());
        assert!(!s.validate(Some(&format!("{COOKIE_NAME}={t}")), "tunnel-1"));
    }

    #[test]
    fn cookie_header_carries_every_required_attribute() {
        let h = cookie_header("tok", true);
        assert!(h.starts_with("s=tok;"));
        for attr in ["HttpOnly", "SameSite=Strict", "Secure", "Path=/"] {
            assert!(h.contains(attr), "cookie is missing {attr}: {h}");
        }
        // There is no Max-Age / Expires: §4.4 has no refresh tokens and
        // no persistent cookie — expiry is the agent's decision, not
        // the browser's.
        assert!(!h.contains("Max-Age") && !h.contains("Expires"));
    }

    #[test]
    fn insecure_cookie_is_only_reachable_deliberately() {
        assert!(!cookie_header("tok", false).contains("Secure"));
    }

    #[test]
    fn cookie_is_extracted_from_a_multi_pair_header() {
        assert_eq!(
            extract_cookie("foo=bar; s=the-token; baz=qux"),
            Some("the-token".to_string())
        );
        assert_eq!(extract_cookie("foo=bar"), None);
    }
}
