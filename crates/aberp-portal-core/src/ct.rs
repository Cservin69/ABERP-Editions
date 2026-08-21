//! Constant-time byte comparison.
//!
//! Every token the portal compares — the knock token (ADR-0115 §3.3),
//! the enrolment token (§4.3), the session token (§4.4), the WebAuthn
//! challenge (§4.3) — goes through here. §3.2 makes the timing
//! property explicit for the knock in particular: "no timing cliff (the
//! gate check is a constant-time token compare)", because a byte-at-a-
//! time early return would turn the uniform 404 into an oracle.
//!
//! Same shape as `apps/aberp/src/serve.rs::constant_time_eq` and
//! `apps/aberp-ui/src/pinned_client.rs::constant_time_eq` — the
//! codebase's existing convention, kept uniform rather than reinvented.

/// `true` iff `a` and `b` are byte-identical, in time that depends only
/// on the lengths.
///
/// Note the length check itself short-circuits: that leaks the length
/// of the candidate, which the attacker already chose, not the length
/// of the secret.
#[must_use]
pub fn eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::eq;

    #[test]
    fn equal_slices_compare_equal() {
        assert!(eq(b"knock", b"knock"));
        assert!(eq(b"", b""));
    }

    #[test]
    fn unequal_content_compares_unequal() {
        assert!(!eq(b"knock", b"knocl"));
        // Differing in the FIRST byte must be as unequal as differing in
        // the last — the whole point of the loop having no early exit.
        assert!(!eq(b"knock", b"Knock"));
    }

    #[test]
    fn unequal_length_compares_unequal() {
        assert!(!eq(b"knock", b"knocks"));
        assert!(!eq(b"", b"k"));
    }
}
