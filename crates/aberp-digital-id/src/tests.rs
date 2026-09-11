//! Unit tests for the S344 DigitalIdProvider trait + MockProvider.

use crate::mock::{constant_time_eq, MOCK_ALGORITHM};
use crate::{DigitalIdProvider, MockProvider, MOCK_OPERATOR_ID};

#[test]
fn s344_mock_provider_returns_stable_identity() {
    let p = MockProvider::new();
    let a = p.current_operator().expect("current operator");
    let b = p.current_operator().expect("current operator");
    // Stable across calls — byte-identical, including the pinned timestamp.
    assert_eq!(a, b);
    assert_eq!(a.id, MOCK_OPERATOR_ID);
    assert_eq!(a.issuer, "mock");
    assert_eq!(a.display_name, "Mock Operator");
    assert_eq!(a.scope, vec!["operator".to_string()]);
}

#[test]
fn s344_mock_provider_signs_and_verifies_round_trip() {
    let p = MockProvider::new();
    let payload = b"audit-entry-canonical-bytes";
    let sig = p.sign(payload).expect("sign");
    assert_eq!(sig.algorithm, MOCK_ALGORITHM);
    assert_eq!(sig.signer_id, MOCK_OPERATOR_ID);
    assert_eq!(sig.bytes.len(), 32, "HMAC-SHA256 is 32 bytes");
    assert!(p.verify(payload, &sig).expect("verify"));
}

#[test]
fn s344_mock_provider_verify_fails_on_tampered_payload() {
    let p = MockProvider::new();
    let sig = p.sign(b"original payload").expect("sign");
    assert!(
        !p.verify(b"tampered payload", &sig).expect("verify"),
        "a signature must not validate a different payload"
    );
}

#[test]
fn s344_mock_provider_verify_fails_on_tampered_signature() {
    let p = MockProvider::new();
    let payload = b"original payload";
    let mut sig = p.sign(payload).expect("sign");
    // Flip one bit of the MAC.
    sig.bytes[0] ^= 0x01;
    assert!(
        !p.verify(payload, &sig).expect("verify"),
        "a mutated signature must not validate"
    );

    // A foreign algorithm tag is rejected even if the bytes happen to be a
    // valid mock MAC — the verifier checks the tag first.
    let mut wrong_algo = p.sign(payload).expect("sign");
    wrong_algo.algorithm = "ecdsa-p256".to_string();
    assert!(
        !p.verify(payload, &wrong_algo).expect("verify"),
        "a foreign algorithm tag must be rejected"
    );
}

#[test]
fn s344_mock_provider_uses_constant_time_compare() {
    // The mock's verify() routes through `constant_time_eq`. We can't time
    // a unit test reliably, so this asserts *correctness* across N variations
    // — equal, first-byte differ, last-byte differ, length differ — which is
    // the property `constant_time_eq` must hold while remaining branchless
    // over equal-length inputs. (The constant-time property itself is
    // documented at the function; this pins behaviour so a refactor to a
    // short-circuiting `==` would fail here.)
    assert!(constant_time_eq(b"", b""));
    assert!(constant_time_eq(b"abcdef", b"abcdef"));
    assert!(!constant_time_eq(b"abcdef", b"Xbcdef")); // differ at start
    assert!(!constant_time_eq(b"abcdef", b"abcdeX")); // differ at end
    assert!(!constant_time_eq(b"abcdef", b"abcde")); // differ in length
    assert!(!constant_time_eq(b"abcde", b"abcdef")); // differ in length (other way)

    // Sweep every single-byte mismatch position across a 32-byte buffer
    // (the MAC width): all must report unequal.
    let base = [0xAAu8; 32];
    for i in 0..base.len() {
        let mut other = base;
        other[i] ^= 0xFF;
        assert!(
            !constant_time_eq(&base, &other),
            "mismatch at byte {i} must be detected"
        );
    }
}

#[test]
fn s344_mock_provider_name_is_mock() {
    assert_eq!(MockProvider::new().name(), "mock");
}

/// Run `f` with a scoped `tracing` subscriber installed and return everything
/// it logged at WARN or above.
///
/// # Why this is serialized, and why it rebuilds the interest cache
///
/// Both production-guard tests in this module (`MockProvider` and
/// `UsDodCacProvider`) capture a `tracing` WARN emitted during construction.
/// Run in parallel they were **flaky** — measured at 2 failures in 120 runs of
/// the test binary at `--test-threads=16`, each time as a captured buffer that
/// was simply `""`, and on either test indifferently. Alone, each passes
/// indefinitely; the crate's suite is fast enough that the pair almost always
/// interleaves.
///
/// The cause is `tracing`'s per-callsite **`Interest` cache**, which is
/// global while `tracing::subscriber::with_default` is only *thread-local*.
/// A callsite first reached while no subscriber has ever been set caches
/// `Interest::never()`, after which the `warn!` macro short-circuits without
/// ever consulting the current thread's subscriber. Setting a scoped default
/// rebuilds that cache — which is why a single-threaded reproduction of the
/// poisoning always passed — so the surviving window is two threads racing
/// their FIRST `set_default` against each other's callsite registration.
///
/// Two things close it, and both are needed:
///
/// 1. **One capture at a time** ([`CAPTURE_LOCK`]). The race needs two
///    concurrent first-time scoped defaults; there is now at most one.
/// 2. **`rebuild_interest_cache()` after the subscriber is installed.** This
///    re-asks every callsite, and with a scoped subscriber in place the answer
///    is `Interest::sometimes()` — "call `enabled()` each time" — which routes
///    the decision through the thread-local dispatcher instead of a stale
///    global verdict. It also clears any `never` cached by an earlier test.
///
/// What this deliberately does **not** do is weaken the assertion. These tests
/// exist to prove a stub identity provider announces itself loudly, which is a
/// production-safety guard; a "fix" that let them pass without the WARN
/// actually being emitted would be worse than the flake. Deleting the `warn!`
/// from either provider still reds them.
fn capture_warn_logs(f: impl FnOnce()) -> String {
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::fmt::MakeWriter;

    /// Serializes the WARN-capturing tests. See [`capture_warn_logs`].
    ///
    /// Poisoning is ignored on purpose: if one capture test panics, the other
    /// should still report its own verdict rather than fail with a lock error
    /// that hides which guard is actually missing.
    static CAPTURE_LOCK: Mutex<()> = Mutex::new(());
    let _serialized = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    /// An in-memory `MakeWriter` accumulating every log line into one buffer.
    #[derive(Clone)]
    struct BufMaker(Arc<Mutex<Vec<u8>>>);
    struct BufGuard(Arc<Mutex<Vec<u8>>>);
    impl Write for BufGuard {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> MakeWriter<'a> for BufMaker {
        type Writer = BufGuard;
        fn make_writer(&'a self) -> Self::Writer {
            BufGuard(self.0.clone())
        }
    }

    let buf = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(BufMaker(buf.clone()))
        .with_max_level(tracing::Level::WARN)
        .without_time()
        .finish();

    tracing::subscriber::with_default(subscriber, || {
        // Order matters: the subscriber must already be the thread's default
        // when the cache is rebuilt, or the rebuild re-derives the same stale
        // verdict it is meant to clear.
        tracing::callsite::rebuild_interest_cache();
        f();
    });

    let logged = buf.lock().unwrap().clone();
    String::from_utf8(logged).expect("utf8 log")
}

#[test]
fn s344_mock_provider_logs_warning_on_construction() {
    let logged = capture_warn_logs(|| {
        let _p = MockProvider::new();
    });
    assert!(
        logged.contains("MOCK — NOT FOR PRODUCTION USE"),
        "construction must emit the production-guard WARN; got: {logged:?}"
    );
    assert!(
        logged.contains("WARN"),
        "the guard line must be at WARN level"
    );
}

// ──────────────────────────────────────────────────────────────────────────
// S363 / PR-50 — UsDodCacProvider: the SECOND impl, validating the trait
// abstracts cleanly across different signing / session / verification shapes.
// ──────────────────────────────────────────────────────────────────────────

use crate::{UsDodCacProvider, CAC_ALGORITHM, CAC_DEFAULT_EDIPI, CAC_ISSUER};

#[test]
fn s363_cac_provider_returns_stable_session_identity() {
    let p = UsDodCacProvider::new();
    let a = p.current_operator().expect("inserted card has an operator");
    let b = p.current_operator().expect("inserted card has an operator");
    // Stable across calls — byte-identical, including the pinned timestamp.
    assert_eq!(a, b);
    // Identity is card-derived: id == EDIPI, issuer == us-dod-cac, and it
    // carries clearance scope a bare mock operator does not.
    assert_eq!(a.id, CAC_DEFAULT_EDIPI);
    assert_eq!(a.issuer, CAC_ISSUER);
    assert!(a.scope.contains(&"cui-cleared".to_string()));
}

#[test]
fn s363_cac_provider_signs_and_verifies_round_trip() {
    let p = UsDodCacProvider::new();
    let payload = b"audit-entry-canonical-bytes";
    let sig = p.sign(payload).expect("sign");
    // A different signing persona than the mock: distinct algorithm tag,
    // distinct (un-keyed) digest construction.
    assert_eq!(sig.algorithm, CAC_ALGORITHM);
    assert_ne!(sig.algorithm, MOCK_ALGORITHM);
    assert_eq!(sig.signer_id, CAC_DEFAULT_EDIPI);
    assert_eq!(sig.bytes.len(), 32, "SHA-256 stub digest is 32 bytes");
    assert!(p.verify(payload, &sig).expect("verify"));
}

#[test]
fn s363_cac_provider_verify_fails_on_tampered_payload() {
    let p = UsDodCacProvider::new();
    let sig = p.sign(b"original payload").expect("sign");
    assert!(
        !p.verify(b"tampered payload", &sig).expect("verify"),
        "a signature must not validate a different payload"
    );
}

#[test]
fn s363_cac_provider_verify_fails_on_tampered_signature_and_foreign_algo() {
    let p = UsDodCacProvider::new();
    let payload = b"original payload";
    let mut sig = p.sign(payload).expect("sign");
    sig.bytes[0] ^= 0x01;
    assert!(
        !p.verify(payload, &sig).expect("verify"),
        "a mutated signature must not validate"
    );

    // A foreign algorithm tag (e.g. the mock's) is rejected before any digest
    // recompute — the tag is checked first.
    let mut wrong_algo = p.sign(payload).expect("sign");
    wrong_algo.algorithm = MOCK_ALGORITHM.to_string();
    assert!(
        !p.verify(payload, &wrong_algo).expect("verify"),
        "a foreign algorithm tag must be rejected"
    );
}

#[test]
fn s363_cac_provider_different_operator_yields_different_signature() {
    // Two different cards in two readers, same payload → card-bound, distinct
    // signatures. Proves the signature is identity-bound, not a fixed MAC.
    let a = UsDodCacProvider::with_edipi("1111111111");
    let b = UsDodCacProvider::with_edipi("2222222222");
    let payload = b"same canonical bytes";
    let sig_a = a.sign(payload).expect("sign a");
    let sig_b = b.sign(payload).expect("sign b");
    assert_ne!(sig_a.signer_id, sig_b.signer_id);
    assert_ne!(
        sig_a.bytes, sig_b.bytes,
        "different operators must produce different signatures over the same payload"
    );
    // Each reader validates its own card's signature…
    assert!(a.verify(payload, &sig_a).expect("verify a/a"));
    assert!(b.verify(payload, &sig_b).expect("verify b/b"));
}

#[test]
fn s363_cac_provider_rejects_signer_absent_from_trusted_chain() {
    // THE differentiating verification semantic: card B's reader rejects card
    // A's signature because A's EDIPI is not in B's trusted chain — even
    // though A's signature is internally self-consistent. The mock's pure-HMAC
    // verify cannot even express this rejection.
    let a = UsDodCacProvider::with_edipi("1111111111");
    let b = UsDodCacProvider::with_edipi("2222222222");
    let payload = b"cross-reader payload";
    let sig_a = a.sign(payload).expect("sign a");
    assert!(
        a.verify(payload, &sig_a).expect("a trusts its own card"),
        "a reader must trust its own card's signature"
    );
    assert!(
        !b.verify(payload, &sig_a)
            .expect("verify runs, just rejects"),
        "a reader must reject a signer absent from its trusted chain"
    );
}

#[test]
fn s363_cac_provider_ejected_card_has_no_operator() {
    // Session-based semantics the static mock cannot express: no card → the
    // NoCurrentOperator arm finally has a real producer, on every method.
    let p = UsDodCacProvider::ejected();
    assert!(
        matches!(
            p.current_operator(),
            Err(crate::ProviderError::NoCurrentOperator)
        ),
        "an ejected reader has no current operator"
    );
    assert!(
        matches!(p.sign(b"x"), Err(crate::ProviderError::NoCurrentOperator)),
        "an ejected reader cannot sign"
    );
    // verify() also can't run without a trusted chain → Err, not Ok(false).
    let other = UsDodCacProvider::new();
    let sig = other.sign(b"x").expect("sign");
    assert!(
        matches!(
            p.verify(b"x", &sig),
            Err(crate::ProviderError::NoCurrentOperator)
        ),
        "an ejected reader cannot verify"
    );
}

#[test]
fn s363_cac_provider_uses_constant_time_compare() {
    // The CAC verify routes its digest comparison through the same
    // `constant_time_eq` the mock uses (shared, not duplicated). Pin that the
    // digest equality is branchless-correct across positions: flip any single
    // byte of a valid signature and verification must fail.
    let p = UsDodCacProvider::new();
    let payload = b"constant-time check";
    let sig = p.sign(payload).expect("sign");
    assert!(p.verify(payload, &sig).expect("verify baseline"));
    for i in 0..sig.bytes.len() {
        let mut tampered = sig.clone();
        tampered.bytes[i] ^= 0xFF;
        assert!(
            !p.verify(payload, &tampered).expect("verify"),
            "a flip at digest byte {i} must be detected"
        );
    }
    // And the shared comparator itself still holds for equal inputs.
    assert!(constant_time_eq(&sig.bytes, &sig.bytes));
}

#[test]
fn s363_cac_provider_name_is_us_dod_cac() {
    assert_eq!(UsDodCacProvider::new().name(), CAC_ISSUER);
    assert_eq!(UsDodCacProvider::new().name(), "us-dod-cac");
}

#[test]
fn s363_cac_provider_logs_warning_on_construction() {
    let logged = capture_warn_logs(|| {
        let _p = UsDodCacProvider::new();
    });
    assert!(
        logged.contains("US-DoD-CAC STUB — NOT FOR PRODUCTION USE"),
        "construction must emit the production-guard WARN; got: {logged:?}"
    );
    assert!(
        logged.contains("WARN"),
        "the guard line must be at WARN level"
    );
}
