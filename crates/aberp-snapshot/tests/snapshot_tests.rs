//! Integration tests for the snapshot subsystem (ADR-0081).
//!
//! Covers the brief's required cases: retention math, validation
//! success/fail flow, restore refuses-prod-overwrite, and snapshot
//! round-trip equivalence (export → import → rows match). Audit-event
//! emission is tested in `apps/aberp` where the events are emitted.

use std::path::{Path, PathBuf};

use aberp_audit_ledger::{Actor, BinaryHash, EventKind, Ledger, TenantId};
use aberp_snapshot::{
    ensure_restore_allowed, list_snapshots, plan_retention, prune, restore_into, take_snapshot,
    validate_export, RetentionPolicy, SnapshotMeta, SnapshotRecord,
};
use duckdb::Connection;
use time::macros::datetime;
use time::OffsetDateTime;

// ──────────────────────────────────────────────────────────────────────
// Test scaffolding (no tempfile dev-dep — mirrors the S393 pattern)
// ──────────────────────────────────────────────────────────────────────

struct ScopedTempDir(PathBuf);

impl ScopedTempDir {
    fn new(label: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "aberp-s426-snap-{label}-{}-{nanos}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create scoped tempdir");
        Self(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScopedTempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Seed a DuckDB at `path` with `n_invoice` invoice rows and `n_audit`
/// valid audit entries (a well-formed hash chain).
fn seed_db(path: &Path, tenant: &str, n_invoice: usize, n_audit: usize) {
    {
        let conn = Connection::open(path).expect("open db for invoice seed");
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS invoice (id BIGINT, amount DOUBLE, note VARCHAR);",
        )
        .expect("create invoice");
        for i in 0..n_invoice {
            conn.execute(
                "INSERT INTO invoice VALUES (?, ?, ?)",
                duckdb::params![i as i64, (i as f64) * 10.0, format!("inv-{i}")],
            )
            .expect("insert invoice");
        }
    }
    let tid = TenantId::new(tenant.to_string()).expect("tenant");
    let mut ledger =
        Ledger::open(path, tid, BinaryHash::from_bytes([1u8; 32])).expect("open ledger");
    for i in 0..n_audit {
        ledger
            .append(
                EventKind::Test,
                format!("{{\"i\":{i}}}").into_bytes(),
                Actor::test_only(),
                None,
            )
            .expect("append audit entry");
    }
}

fn read_invoice_ids(path: &Path) -> Vec<i64> {
    let conn = Connection::open(path).expect("reopen for invoice read");
    let mut stmt = conn
        .prepare("SELECT id FROM invoice ORDER BY id")
        .expect("prepare");
    let ids = stmt
        .query_map([], |r| r.get::<_, i64>(0))
        .expect("query")
        .map(|r| r.unwrap())
        .collect();
    ids
}

fn record(seq: u64, created_at: OffsetDateTime, valid: bool) -> SnapshotRecord {
    SnapshotRecord {
        dir: PathBuf::from(format!("/nonexistent/snap-{seq}")),
        meta: SnapshotMeta {
            seq,
            created_at,
            source_db_sha256: "deadbeef".into(),
            byte_size: 100,
            valid,
            invoice_count: 1,
            audit_count: 1,
            chain_len: 1,
            validation_error: None,
        },
    }
}

// ──────────────────────────────────────────────────────────────────────
// Snapshot round-trip equivalence (export → import → rows match)
// ──────────────────────────────────────────────────────────────────────

#[test]
fn take_snapshot_validates_and_round_trips() {
    let dir = ScopedTempDir::new("roundtrip");
    let db = dir.path().join("aberp.duckdb");
    seed_db(&db, "prod", 3, 5);

    let store = dir.path().join("store");
    let now = datetime!(2026-06-15 14:30:00 UTC);
    let rec = take_snapshot(&db, &store, "prod", now).expect("snapshot ok");

    assert!(rec.meta.valid, "fresh snapshot must validate: {:?}", rec.meta);
    assert_eq!(rec.meta.invoice_count, 3);
    assert_eq!(rec.meta.audit_count, 5);
    assert_eq!(rec.meta.chain_len, 5);
    assert_eq!(rec.meta.seq, 1);
    assert!(rec.dir.exists(), "finalized snapshot dir must exist");
    assert!(rec.dir.join("meta.json").exists());
    assert!(rec.meta.byte_size > 0);
    assert_eq!(rec.meta.source_db_sha256.len(), 64, "hex sha256");

    // Restore into a fresh side path → rows survive identically.
    let target = dir.path().join("restored").join("aberp.duckdb");
    restore_into(&rec.dir, &target, "prod").expect("restore ok");
    assert_eq!(read_invoice_ids(&target), vec![0, 1, 2]);
}

#[test]
fn second_snapshot_gets_next_seq() {
    let dir = ScopedTempDir::new("seq");
    let db = dir.path().join("aberp.duckdb");
    seed_db(&db, "prod", 1, 1);
    let store = dir.path().join("store");

    let r1 = take_snapshot(&db, &store, "prod", datetime!(2026-06-15 10:00:00 UTC)).unwrap();
    let r2 = take_snapshot(&db, &store, "prod", datetime!(2026-06-15 14:00:00 UTC)).unwrap();
    assert_eq!(r1.meta.seq, 1);
    assert_eq!(r2.meta.seq, 2);

    let listed = list_snapshots(&store).unwrap();
    assert_eq!(listed.len(), 2);
    // Newest first.
    assert_eq!(listed[0].meta.seq, 2);
    assert_eq!(listed[1].meta.seq, 1);
}

// ──────────────────────────────────────────────────────────────────────
// Validation success / fail flow
// ──────────────────────────────────────────────────────────────────────

#[test]
fn validation_fails_on_tampered_chain() {
    let dir = ScopedTempDir::new("tamper");
    let db = dir.path().join("aberp.duckdb");
    seed_db(&db, "prod", 1, 3);

    // Tamper with a payload so its stored entry_hash no longer matches the
    // recomputed hash → the chain must fail to verify.
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch("UPDATE audit_ledger SET payload = 'tampered'::BLOB WHERE seq = 1;")
            .expect("tamper");
    }

    let store = dir.path().join("store");
    let now = datetime!(2026-06-15 14:30:00 UTC);
    let rec = take_snapshot(&db, &store, "prod", now).expect("snapshot still produced");

    assert!(!rec.meta.valid, "tampered chain must fail validation");
    let err = rec.meta.validation_error.unwrap_or_default();
    assert!(
        err.contains("hash-chain"),
        "validation error should name the chain failure: {err}"
    );
    // The invalid snapshot is still kept on disk (operator can inspect).
    assert!(rec.dir.exists());
}

#[test]
fn validate_export_rejects_garbage_dir() {
    let dir = ScopedTempDir::new("garbage");
    let bogus = dir.path().join("not-an-export");
    std::fs::create_dir_all(&bogus).unwrap();
    std::fs::write(bogus.join("schema.sql"), b"this is not a valid export").unwrap();

    let report = validate_export(&bogus, "prod");
    assert!(!report.ok, "a non-export dir must not validate");
    assert!(report.error.is_some());
}

#[test]
fn restore_refuses_from_invalid_snapshot() {
    let dir = ScopedTempDir::new("restore-invalid");
    let db = dir.path().join("aberp.duckdb");
    seed_db(&db, "prod", 1, 2);
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch("UPDATE audit_ledger SET payload = 'x'::BLOB WHERE seq = 1;")
            .unwrap();
    }
    let store = dir.path().join("store");
    let rec = take_snapshot(&db, &store, "prod", datetime!(2026-06-15 14:30:00 UTC)).unwrap();
    assert!(!rec.meta.valid);

    let target = dir.path().join("out").join("aberp.duckdb");
    let err = restore_into(&rec.dir, &target, "prod")
        .expect_err("restore from invalid snapshot must refuse");
    assert!(err.to_string().contains("failed validation"));
}

// ──────────────────────────────────────────────────────────────────────
// Restore refuses prod overwrite (safety in the binary)
// ──────────────────────────────────────────────────────────────────────

#[test]
fn ensure_restore_allowed_refuses_without_confirm() {
    let target = PathBuf::from("/tmp/some/side/aberp.duckdb");
    let err = ensure_restore_allowed(&target, false).expect_err("no --confirm → refuse");
    assert!(err.to_string().contains("--confirm"));
}

#[test]
fn ensure_restore_allowed_refuses_aberp_home_even_with_confirm() {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    let prod = PathBuf::from(home).join(".aberp").join("prod").join("aberp.duckdb");
    let err = ensure_restore_allowed(&prod, true).expect_err("prod path → refuse");
    assert!(err.to_string().contains(".aberp"));

    // Any tenant home, not just prod.
    let dev = PathBuf::from("/Users/x/.aberp/dev/aberp.duckdb");
    assert!(ensure_restore_allowed(&dev, true).is_err());
}

#[test]
fn ensure_restore_allowed_permits_side_path_with_confirm() {
    let side = PathBuf::from("/tmp/aberp-recovery/aberp.duckdb");
    ensure_restore_allowed(&side, true).expect("side path + confirm → allowed");
}

// ──────────────────────────────────────────────────────────────────────
// Retention math (pure)
// ──────────────────────────────────────────────────────────────────────

#[test]
fn retention_keeps_last_n_and_prunes_older() {
    // 30 snapshots, one every 4h, all valid. keep_last=24, no daily/weekly.
    let policy = RetentionPolicy {
        keep_last: 24,
        daily_days: 0,
        weekly_weeks: 0,
    };
    let base = datetime!(2026-05-01 00:00:00 UTC);
    let records: Vec<SnapshotRecord> = (1..=30)
        .map(|i| record(i, base + time::Duration::hours(4 * i as i64), true))
        .collect();
    let now = base + time::Duration::hours(4 * 31);

    let plan = plan_retention(&records, &policy, now);
    assert_eq!(plan.keep.len(), 24, "exactly keep_last kept");
    assert_eq!(plan.prune.len(), 6);
    // The 6 oldest seqs are pruned.
    assert_eq!(plan.prune, vec![1, 2, 3, 4, 5, 6]);
    // keep+prune partition the input.
    assert_eq!(plan.keep.len() + plan.prune.len(), 30);
}

#[test]
fn retention_never_prunes_newest_valid() {
    // Even with keep_last=0 and no windows, the newest valid survives.
    let policy = RetentionPolicy {
        keep_last: 0,
        daily_days: 0,
        weekly_weeks: 0,
    };
    let base = datetime!(2026-05-01 00:00:00 UTC);
    let records: Vec<SnapshotRecord> = (1..=5)
        .map(|i| record(i, base + time::Duration::hours(i as i64), true))
        .collect();
    let now = base + time::Duration::days(1);

    let plan = plan_retention(&records, &policy, now);
    assert_eq!(plan.keep, vec![5], "only newest valid kept");
    assert_eq!(plan.prune, vec![1, 2, 3, 4]);
}

#[test]
fn retention_keeps_one_per_day_within_window() {
    // 3 snapshots/day for 5 days; keep_last=0; daily_days=10. Expect one
    // (the newest) per day kept, plus newest-valid (already day-5's).
    let policy = RetentionPolicy {
        keep_last: 0,
        daily_days: 10,
        weekly_weeks: 0,
    };
    let mut records = Vec::new();
    let mut seq = 0;
    for day in 0..5 {
        for hour in [6, 12, 18] {
            seq += 1;
            let ts = datetime!(2026-06-01 00:00:00 UTC)
                + time::Duration::days(day)
                + time::Duration::hours(hour);
            records.push(record(seq, ts, true));
        }
    }
    let now = datetime!(2026-06-06 00:00:00 UTC);
    let plan = plan_retention(&records, &policy, now);
    // One per day × 5 days = 5 kept (the 18:00 snapshot of each day).
    assert_eq!(plan.keep.len(), 5, "one per day: {:?}", plan.keep);
    // Day 1's kept seq is the third (18:00) = seq 3; day 5's = seq 15.
    assert!(plan.keep.contains(&3));
    assert!(plan.keep.contains(&15));
    assert!(plan.prune.contains(&1));
    assert!(plan.prune.contains(&2));
}

#[test]
fn retention_prunes_invalid_but_keeps_newest_valid() {
    let policy = RetentionPolicy::default();
    let base = datetime!(2026-06-15 00:00:00 UTC);
    // seq 3 (newest) is INVALID; seq 2 valid; seq 1 valid.
    let records = vec![
        record(1, base + time::Duration::hours(1), true),
        record(2, base + time::Duration::hours(2), true),
        record(3, base + time::Duration::hours(3), false),
    ];
    let now = base + time::Duration::hours(4);
    let plan = plan_retention(&records, &policy, now);
    // The invalid newest (seq 3) is pruned; newest VALID (seq 2) kept.
    assert!(plan.keep.contains(&2));
    assert!(plan.prune.contains(&3), "invalid snapshot pruned");
}

#[test]
fn retention_keeps_newest_overall_when_none_valid() {
    let policy = RetentionPolicy::default();
    let base = datetime!(2026-06-15 00:00:00 UTC);
    let records = vec![
        record(1, base + time::Duration::hours(1), false),
        record(2, base + time::Duration::hours(2), false),
    ];
    let now = base + time::Duration::hours(3);
    let plan = plan_retention(&records, &policy, now);
    assert_eq!(plan.keep, vec![2], "last-resort: keep newest overall");
    assert_eq!(plan.prune, vec![1]);
}

// ──────────────────────────────────────────────────────────────────────
// Prune (IO) actually removes directories
// ──────────────────────────────────────────────────────────────────────

#[test]
fn prune_removes_condemned_dirs_only() {
    let dir = ScopedTempDir::new("prune");
    let db = dir.path().join("aberp.duckdb");
    seed_db(&db, "prod", 1, 1);
    let store = dir.path().join("store");

    let mut dirs = Vec::new();
    for h in 0..3 {
        let r = take_snapshot(
            &db,
            &store,
            "prod",
            datetime!(2026-06-15 00:00:00 UTC) + time::Duration::hours(h),
        )
        .unwrap();
        dirs.push(r.dir);
    }
    let records = list_snapshots(&store).unwrap();
    assert_eq!(records.len(), 3);

    // Prune seq 1 only.
    let plan = aberp_snapshot::RetentionPlan {
        keep: vec![2, 3],
        prune: vec![1],
    };
    let removed = prune(&records, &plan).unwrap();
    assert_eq!(removed, vec![1]);

    let after = list_snapshots(&store).unwrap();
    assert_eq!(after.len(), 2);
    assert!(after.iter().all(|r| r.meta.seq != 1));
}
