//! S341 — concurrent append safety after dropping `UNIQUE(seq)`.
//!
//! The legacy `UNIQUE(seq)` ART index was removed (duckdb#23046 / S332).
//! In-process fork PREVENTION now comes from `AUDIT_APPEND_LOCK` inside
//! [`append_reopen`], and DETECTION from the hash chain. This test pins
//! the prevention layer: many threads appending concurrently through the
//! serialized reopen-per-write path must still produce a dense, monotonic
//! `seq` and a chain that verifies — i.e. NOT a single fork.
//!
//! Without the lock, concurrent reopen-per-write writers read the same
//! committed head and both append `seq = head + 1`, forking the chain
//! (which `verify_chain` would then reject) — this test would fail.

use std::path::PathBuf;
use std::sync::Arc;
use std::thread;

use aberp_audit_ledger::{
    append_reopen, Actor, BinaryHash, EventKind, Ledger, LedgerMeta, TenantId,
};
use ulid::Ulid;

fn tenant() -> TenantId {
    TenantId::new("s341-concurrent".to_string()).unwrap()
}

#[test]
fn s341_concurrent_appends_stay_dense_and_verify() {
    let mut path: PathBuf = std::env::temp_dir();
    path.push(format!("aberp-s341-concurrent-{}.duckdb", Ulid::new()));
    let _ = std::fs::remove_file(&path);

    let binary_hash = BinaryHash::from_bytes([0x33u8; 32]);

    // Create the file + schema once up front (fresh = no-UNIQUE schema).
    {
        let _l = Ledger::open(&path, tenant(), binary_hash).expect("create ledger");
    }

    // 16 threads contend hard for AUDIT_APPEND_LOCK while each does a
    // batch of serialized reopen-per-write appends. The lock contention
    // (not the absolute count) is what proves no fork; the count is kept
    // modest because each reopen-per-write `Connection::open` on a
    // file-backed DuckDB is ~tens of ms (the inherent daemon-pattern
    // cost), so 16 × 8 = 128 keeps the gate fast while still racing every
    // worker through the critical section many times over.
    const THREADS: usize = 16;
    const PER_THREAD: usize = 8;
    let total = THREADS * PER_THREAD;

    let path = Arc::new(path);
    let mut handles = Vec::new();
    for t in 0..THREADS {
        let path = Arc::clone(&path);
        handles.push(thread::spawn(move || {
            let meta = LedgerMeta::new(tenant(), binary_hash);
            for i in 0..PER_THREAD {
                let actor = Actor::from_local_cli(Ulid::new().to_string(), "concurrent-tester");
                append_reopen(
                    &path,
                    &meta,
                    EventKind::Test,
                    format!("{{\"t\":{t},\"i\":{i}}}").into_bytes(),
                    actor,
                    None,
                )
                .expect("serialized concurrent append must succeed");
            }
        }));
    }
    for h in handles {
        h.join().expect("worker thread panicked");
    }

    // Read back: dense + monotonic seq, and the chain verifies — proof no
    // two writers forked on a shared head.
    let ledger = Ledger::open(path.as_ref(), tenant(), binary_hash).expect("reopen ledger");
    let entries = ledger.entries().expect("read entries");
    assert_eq!(
        entries.len(),
        total,
        "every concurrent append landed exactly once"
    );
    let seqs: Vec<u64> = entries.iter().map(|e| e.seq.as_u64()).collect();
    let expected: Vec<u64> = (1..=total as u64).collect();
    assert_eq!(
        seqs, expected,
        "seq must be dense + monotonic 1..=N (no fork, no gap, no dup)"
    );
    assert_eq!(
        ledger.verify_chain().expect("chain verifies"),
        total as u64,
        "hash chain verifies across all concurrent appends"
    );

    let _ = std::fs::remove_file(path.as_ref());
}
