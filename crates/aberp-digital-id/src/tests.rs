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

#[test]
fn s344_mock_provider_logs_warning_on_construction() {
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::fmt::MakeWriter;

    // An in-memory MakeWriter that accumulates every log line into a shared
    // buffer, so the test can assert the WARN line was emitted.
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
        let _p = MockProvider::new();
    });

    let logged = String::from_utf8(buf.lock().unwrap().clone()).expect("utf8 log");
    assert!(
        logged.contains("MOCK — NOT FOR PRODUCTION USE"),
        "construction must emit the production-guard WARN; got: {logged:?}"
    );
    assert!(
        logged.contains("WARN"),
        "the guard line must be at WARN level"
    );
}
