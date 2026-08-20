//! Token minting.
//!
//! Every unguessable value the agent produces — knock token (ADR-0113
//! §3.3), enrolment token (§4.3), WebAuthn challenge (§4.3), session
//! token (§4.4), tunnel id (§2.1), WebAuthn user handle — comes from
//! here, so there is one place to audit the entropy source.
//!
//! `getrandom` is the OS CSPRNG (`getentropy` on macOS). It is already
//! a workspace pin; no `rand` crate enters the tree, matching the
//! posture in `crates/audit-ledger` and ADR-0087's session-key minting.

use base64::Engine as _;

/// Token width in bytes. 256 bits everywhere.
///
/// ADR-0113 §3.3 specifies 128 bits for the knock token; we mint 256
/// for every token uniformly because the cost is 22 more characters in
/// a bookmark and the benefit is that no reader has to ask which token
/// is which strength. §9.2's "memorable" requirement lives in the
/// *hostname*, not in these — see `config::PORTAL_HOST`.
pub const TOKEN_BYTES: usize = 32;

/// Minting failed because the OS CSPRNG failed. There is no fallback
/// by design: a weak knock token or a predictable challenge is worse
/// than a daemon that will not start (CLAUDE.md rule 12 — loud fail).
#[derive(Debug, thiserror::Error)]
#[error("OS CSPRNG unavailable — refusing to mint a portal token: {0}")]
pub struct RandError(#[from] getrandom::Error);

/// `n` random bytes from the OS CSPRNG.
pub fn bytes(n: usize) -> Result<Vec<u8>, RandError> {
    let mut buf = vec![0u8; n];
    getrandom::getrandom(&mut buf)?;
    Ok(buf)
}

/// A fresh [`TOKEN_BYTES`]-wide token, base64url without padding —
/// URL-safe so it can be a path segment (the knock is one) and
/// cookie-safe so it can be a session value.
pub fn token() -> Result<String, RandError> {
    Ok(b64url(&bytes(TOKEN_BYTES)?))
}

/// Base64url, no padding — the encoding WebAuthn uses throughout.
#[must_use]
pub fn b64url(raw: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw)
}

/// Decode base64url. Accepts both the padded and unpadded forms because
/// browser code in the wild emits either.
pub fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    let trimmed = s.trim_end_matches('=');
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(trimmed.as_bytes())
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_the_declared_width_and_distinct() {
        let a = token().expect("mint");
        let b = token().expect("mint");
        assert_ne!(a, b, "two mints must not collide");
        assert_eq!(
            b64url_decode(&a).expect("decodes").len(),
            TOKEN_BYTES,
            "a shortened token would silently weaken the knock gate"
        );
        assert!(!a.contains('='), "tokens must be padding-free");
        assert!(
            !a.contains('+') && !a.contains('/'),
            "a token is used as a URL path segment and a cookie value"
        );
    }

    #[test]
    fn b64url_roundtrips_including_padded_input() {
        let raw = [0u8, 1, 2, 250, 251, 252];
        let enc = b64url(&raw);
        assert_eq!(b64url_decode(&enc).expect("decodes"), raw);
        // A browser that emits the padded form must still be understood.
        assert_eq!(b64url_decode(&format!("{enc}==")).expect("decodes"), raw);
    }

    #[test]
    fn b64url_decode_rejects_garbage() {
        assert!(b64url_decode("###").is_none());
    }
}
