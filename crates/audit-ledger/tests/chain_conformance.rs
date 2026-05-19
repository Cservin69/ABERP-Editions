//! Chain-verification conformance test for ADR-0008.
//!
//! Verifies the audit ledger's three load-bearing invariants:
//!
//! 1. **Hash chain is correct on append.** A freshly-built chain of N
//!    entries verifies cleanly: every `entry[k].prev_hash` matches the
//!    previous `entry_hash`, every `entry_hash` equals the SHA-256 of
//!    the canonical-encoded entry minus `entry_hash`.
//! 2. **Tamper of any field is detected.** Mutating the `payload` column
//!    of any historical row causes verification to fail at that row
//!    with `VerifyError::TamperedAt { seq }`.
//! 3. **Chain-link break is detected.** Mutating the `prev_hash` of a
//!    historical row causes verification to fail at that row with
//!    `VerifyError::ChainBroken { seq }`.
//!
//! Per CLAUDE.md rule 9 ("tests verify intent, not just behaviour"),
//! each invariant has its own test and the test asserts the precise
//! failure variant — not just `is_err()`. A passing-but-meaningless
//! version of these tests would be one that only checks that
//! verification returns something; we check that it returns the right
//! something at the right seq.

use aberp_audit_ledger::{
    Actor, BinaryHash, EventKind, Ledger, LedgerVerifyError, TenantId, VerifyError,
};

/// Stable test binary hash. Real builds compute this from the on-disk
/// binary in PR-5; the conformance test treats it as a fixed constant.
const TEST_BINARY_HASH: BinaryHash = BinaryHash::from_bytes([0xAB; 32]);

fn tenant() -> TenantId {
    TenantId::new("tenant-conformance-test").expect("test tenant id is valid")
}

fn fresh_ledger() -> Ledger {
    Ledger::open_in_memory(tenant(), TEST_BINARY_HASH).expect("open in-memory ledger")
}

fn append_n(ledger: &mut Ledger, n: u64) {
    for i in 1..=n {
        ledger
            .append(
                EventKind::Test,
                format!("payload-{i}").into_bytes(),
                Actor::test_only(),
                Some(format!("idem-{i}")),
            )
            .unwrap_or_else(|e| panic!("append {i} failed: {e}"));
    }
}

#[test]
fn empty_ledger_verifies() {
    let ledger = fresh_ledger();
    let count = ledger.verify_chain().expect("empty chain verifies");
    assert_eq!(count, 0, "empty ledger should report 0 entries verified");
}

#[test]
fn three_entries_verify_cleanly() {
    let mut ledger = fresh_ledger();
    append_n(&mut ledger, 3);

    let count = ledger.verify_chain().expect("clean chain verifies");
    assert_eq!(count, 3, "verifier should report 3 entries verified");
}

#[test]
fn round_trip_preserves_all_fields() {
    let mut ledger = fresh_ledger();
    append_n(&mut ledger, 3);
    let entries = ledger.entries().expect("read entries");
    assert_eq!(entries.len(), 3);

    // Each entry's id, seq, kind, and payload should be exactly what we
    // stored. We don't pin time_wall/time_mono because those are clock-
    // dependent, but the chain check already covers their stability.
    for (i, entry) in entries.iter().enumerate() {
        let expected_seq = (i + 1) as u64;
        assert_eq!(entry.seq.as_u64(), expected_seq);
        assert_eq!(entry.kind, EventKind::Test);
        assert_eq!(
            entry.payload,
            format!("payload-{expected_seq}").into_bytes(),
        );
        assert_eq!(
            entry.idempotency_key.as_deref(),
            Some(format!("idem-{expected_seq}").as_str()),
        );
        assert_eq!(entry.tenant_id, tenant());
        assert_eq!(entry.binary_hash, TEST_BINARY_HASH);
    }
}

// ──────────────────────────────────────────────────────────────────────
// Tamper tests
//
// We open a fresh ledger, append entries, then re-open the underlying
// DuckDB connection via a second `Connection::open_in_memory` would be
// wrong (in-memory is per-connection). Instead the tamper tests use a
// file-backed DB so we can mutate it from a side connection.
// ──────────────────────────────────────────────────────────────────────

/// Per-test temp path. `tag` lets parallel tests pick unique files even
/// when they share a pid; the thread id adds another layer of uniqueness
/// in case cargo's parallel runner reuses thread names across tests.
fn temp_db_path(tag: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "aberp-audit-conformance-{}-{}-{:?}.duckdb",
        std::process::id(),
        tag,
        std::thread::current().id(),
    ));
    // Best-effort cleanup of any prior run's file.
    let _ = std::fs::remove_file(&p);
    p
}

fn append_three_to_file(path: &std::path::Path) {
    let mut ledger = Ledger::open(path, tenant(), TEST_BINARY_HASH).expect("open file ledger");
    append_n(&mut ledger, 3);
}

#[test]
fn tamper_of_payload_detected_at_correct_seq() {
    let path = temp_db_path("tamper-payload");
    append_three_to_file(&path);

    // Mutate seq=2's payload behind the ledger's back.
    {
        let conn = duckdb::Connection::open(&path).expect("side-connection open");
        let changed = conn
            .execute(
                "UPDATE audit_ledger SET payload = ? WHERE seq = 2;",
                duckdb::params![&b"tampered"[..]],
            )
            .expect("tamper update");
        assert_eq!(changed, 1, "exactly one row should be tampered");
    }

    let ledger = Ledger::open(&path, tenant(), TEST_BINARY_HASH).expect("re-open file ledger");
    let result = ledger.verify_chain();

    match result {
        Err(LedgerVerifyError::Chain(VerifyError::TamperedAt { seq })) => {
            assert_eq!(seq, 2, "tamper should be reported at the mutated seq");
        }
        other => {
            panic!("expected TamperedAt {{ seq: 2 }}, got {other:?} — fail-loud invariant broken")
        }
    }

    let _ = std::fs::remove_file(&path);
}

#[test]
fn chain_break_via_prev_hash_mutation_detected_at_correct_seq() {
    let path = temp_db_path("chain-break");
    append_three_to_file(&path);

    // Replace seq=2's prev_hash with 32 zero bytes (which the chain rule
    // says must equal seq=1's entry_hash, so this is a guaranteed break).
    {
        let conn = duckdb::Connection::open(&path).expect("side-connection open");
        let zeros = [0u8; 32];
        let changed = conn
            .execute(
                "UPDATE audit_ledger SET prev_hash = ? WHERE seq = 2;",
                duckdb::params![&zeros[..]],
            )
            .expect("chain-break update");
        assert_eq!(changed, 1);
    }

    let ledger = Ledger::open(&path, tenant(), TEST_BINARY_HASH).expect("re-open file ledger");
    let result = ledger.verify_chain();

    match result {
        Err(LedgerVerifyError::Chain(VerifyError::ChainBroken { seq })) => {
            assert_eq!(seq, 2, "chain break should be reported at the mutated seq");
        }
        other => {
            panic!("expected ChainBroken {{ seq: 2 }}, got {other:?} — fail-loud invariant broken")
        }
    }

    let _ = std::fs::remove_file(&path);
}
