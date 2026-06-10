//! S341 / PR-36 — regression tests for the `aberp audit-rebuild` path.
//!
//! These pin the integrity invariants of the ART rebuild: row
//! preservation, dense+monotonic seq, and — the load-bearing one — the
//! hash chain verifies AFTER the rebuild. A rebuild that silently forked
//! the chain would reopen the exact `[[no-sql-specific]]` integrity hole
//! the S335 probe found; `s341_rebuild_preserves_hash_chain` is the gate
//! that catches it.
//!
//! The tests build a real file-backed ledger via the public crate API,
//! then drive `aberp::audit_rebuild::rebuild_at` directly (the testable
//! core behind the CLI subcommand).

use std::path::{Path, PathBuf};

use aberp::audit_rebuild::{self, ArtHealth};
use aberp_audit_ledger::{Actor, BinaryHash, EventKind, Ledger, TenantId};
use ulid::Ulid;

const TENANT: &str = "s341-test-tenant";

/// Per-test scratch dir under the OS temp dir, mirroring the codebase
/// convention (`std::env::temp_dir().join("aberp-…").join(label-ULID)`)
/// — the `tempfile` crate is not an apps/aberp dev-dependency.
fn test_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("aberp-s341")
        .join(format!("{}-{}", label, Ulid::new()));
    std::fs::create_dir_all(&dir).expect("create test dir");
    dir
}

fn tenant() -> TenantId {
    TenantId::new(TENANT.to_string()).expect("valid tenant")
}

fn binary_hash() -> BinaryHash {
    BinaryHash::from_bytes([0x5au8; 32])
}

fn actor() -> Actor {
    Actor::from_local_cli("01H0000000000000000000000Z".to_string(), "tester")
}

/// Seed a fresh file-backed ledger at `db_path` with `n` `Test` entries.
fn seed_ledger(db_path: &Path, n: usize) {
    let mut ledger =
        Ledger::open(db_path, tenant(), binary_hash()).expect("open ledger for seeding");
    for i in 0..n {
        ledger
            .append(
                EventKind::Test,
                format!("{{\"i\":{i}}}").into_bytes(),
                actor(),
                None,
            )
            .expect("append seed entry");
    }
    // Prove the seed chain verifies before we ever rebuild.
    assert_eq!(ledger.verify_chain().expect("seed verifies"), n as u64);
}

#[test]
fn s341_rebuild_preserves_row_count() {
    let dir = test_dir("row-count");
    let db = dir.join("aberp.duckdb");
    seed_ledger(&db, 12);

    // Snapshot the original ids so we can prove none were lost.
    let before: Vec<String> = {
        let l = Ledger::open_read_only(&db, tenant(), binary_hash()).unwrap();
        l.entries()
            .unwrap()
            .iter()
            .map(|e| e.id.to_prefixed_string())
            .collect()
    };

    let report = audit_rebuild::rebuild_at(&db, tenant(), binary_hash(), false, true)
        .expect("rebuild succeeds");

    // N originals preserved + exactly one AuditLedgerRebuilt marker.
    assert_eq!(report.rows_before, 12);
    assert_eq!(report.rows_after, 13, "12 originals + 1 marker");

    let after: Vec<(String, String)> = {
        let l = Ledger::open_read_only(&db, tenant(), binary_hash()).unwrap();
        l.entries()
            .unwrap()
            .iter()
            .map(|e| (e.id.to_prefixed_string(), e.kind.as_str().to_string()))
            .collect()
    };
    assert_eq!(after.len(), 13);
    // Every original id is still present, in the same order, as the
    // first 12 rows (verbatim re-insert).
    for (i, orig_id) in before.iter().enumerate() {
        assert_eq!(&after[i].0, orig_id, "original row {i} must be preserved");
    }
    // The last row is the rebuild marker.
    assert_eq!(after[12].1, "audit.ledger_rebuilt");
}

#[test]
fn s341_rebuild_preserves_seq_order() {
    let dir = test_dir("seq-order");
    let db = dir.join("aberp.duckdb");
    seed_ledger(&db, 8);

    let report = audit_rebuild::rebuild_at(&db, tenant(), binary_hash(), false, true)
        .expect("rebuild succeeds");
    assert_eq!(report.seq_max_before, 8);
    assert_eq!(report.seq_max_after, 9);

    // seq must be dense + monotonic 1..=9 after the rebuild.
    let l = Ledger::open_read_only(&db, tenant(), binary_hash()).unwrap();
    let seqs: Vec<u64> = l
        .entries()
        .unwrap()
        .iter()
        .map(|e| e.seq.as_u64())
        .collect();
    assert_eq!(seqs, (1..=9).collect::<Vec<_>>());
}

#[test]
fn s341_rebuild_preserves_hash_chain() {
    // THE critical test. If the rebuild forked the chain, verify_chain
    // would fail here (and the rebuild itself would have aborted at its
    // post-commit gate).
    let dir = test_dir("hash-chain");
    let db = dir.join("aberp.duckdb");
    seed_ledger(&db, 20);

    let report = audit_rebuild::rebuild_at(&db, tenant(), binary_hash(), false, true)
        .expect("rebuild succeeds");
    assert!(report.chain_verified_before, "rows intact going in");
    assert!(report.chain_verified_after, "chain verifies coming out");

    // Independent re-verify against a fresh handle.
    let l = Ledger::open_read_only(&db, tenant(), binary_hash()).unwrap();
    assert_eq!(
        l.verify_chain().expect("post-rebuild chain verifies"),
        21,
        "20 originals + 1 marker all chain-verify"
    );
}

#[test]
fn s341_rebuild_dry_run_is_no_op() {
    let dir = test_dir("dry-run");
    let db = dir.join("aberp.duckdb");
    seed_ledger(&db, 6);

    let mtime_before = std::fs::metadata(&db).unwrap().modified().unwrap();
    let size_before = std::fs::metadata(&db).unwrap().len();

    // Sleep a hair so a write WOULD bump mtime detectably.
    std::thread::sleep(std::time::Duration::from_millis(20));

    let report = audit_rebuild::rebuild_at(&db, tenant(), binary_hash(), true, true)
        .expect("dry-run succeeds");
    assert!(report.dry_run);
    assert_eq!(report.rows_before, 6);
    assert_eq!(report.rows_after, 6, "dry-run adds no rows");
    // A healthy seed must probe Healthy (the probe runs against a copy).
    assert_eq!(report.art_health, Some(ArtHealth::Healthy));

    let mtime_after = std::fs::metadata(&db).unwrap().modified().unwrap();
    let size_after = std::fs::metadata(&db).unwrap().len();
    assert_eq!(
        mtime_before, mtime_after,
        "dry-run must not touch the DB file"
    );
    assert_eq!(
        size_before, size_after,
        "dry-run must not resize the DB file"
    );

    // No backup, no probe-copy, no marker row leaked.
    let l = Ledger::open_read_only(&db, tenant(), binary_hash()).unwrap();
    assert_eq!(l.entries().unwrap().len(), 6, "dry-run left rows untouched");
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("art-probe") || n.contains("pre-rebuild"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "no probe/backup files left: {leftovers:?}"
    );
}

#[test]
fn s341_rebuild_refuses_if_serve_alive() {
    // Simulate a live serve by holding a write handle on the DB while the
    // rebuild is attempted. The `lsof`-based guard must detect the holder
    // and loud-fail rather than race the live writer.
    //
    // The guard's pure decision core is pinned deterministically by the
    // module unit test `guard_refuses_when_serve_holds_db`. This
    // integration test exercises the real `lsof` invocation end-to-end;
    // where `lsof` is unavailable (rare CI images) it defers to that
    // unit test rather than asserting on a platform-specific lock race.
    if std::process::Command::new("lsof")
        .arg("-v")
        .output()
        .is_err()
    {
        eprintln!(
            "S341: lsof unavailable — skipping live-serve refusal integration assertion \
             (covered deterministically by the audit_rebuild::tests guard unit test)"
        );
        return;
    }

    let dir = test_dir("serve-alive");
    let db = dir.join("aberp.duckdb");
    seed_ledger(&db, 5);

    let held = Ledger::open(&db, tenant(), binary_hash()).expect("hold a live write handle");

    let result = audit_rebuild::rebuild_at(&db, tenant(), binary_hash(), false, true);
    assert!(
        result.is_err(),
        "rebuild must refuse (lsof saw the held handle) while the DB is open elsewhere"
    );

    drop(held);

    // The DB must be untouched by the refused attempt — chain still
    // verifies the original 5 rows, no marker leaked.
    let l = Ledger::open_read_only(&db, tenant(), binary_hash()).unwrap();
    assert_eq!(
        l.verify_chain().expect("untouched chain verifies"),
        5,
        "refused rebuild must not have mutated the ledger"
    );
}

#[test]
fn s341_rebuild_marker_payload_records_counts() {
    // Pin the marker payload so a future change to the count semantics
    // surfaces here (CLAUDE.md rule 9 — test intent, not just shape).
    let dir = test_dir("marker");
    let db = dir.join("aberp.duckdb");
    seed_ledger(&db, 4);

    audit_rebuild::rebuild_at(&db, tenant(), binary_hash(), false, true).expect("rebuild");

    let l = Ledger::open_read_only(&db, tenant(), binary_hash()).unwrap();
    let entries = l.entries().unwrap();
    let marker = entries.last().expect("has a last row");
    assert_eq!(marker.kind.as_str(), "audit.ledger_rebuilt");
    let payload: serde_json::Value = serde_json::from_slice(&marker.payload).expect("json payload");
    assert_eq!(payload["rows_before"], 4);
    assert_eq!(payload["rows_after"], 5);
    assert_eq!(payload["seq_max_before"], 4);
    assert_eq!(payload["seq_max_after"], 5);
    assert_eq!(payload["chain_verified"], true);
}
