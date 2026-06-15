//! S426 / ADR-0082 — app-level snapshot integration tests.
//!
//! The `aberp-snapshot` crate's own suite covers export/import/validate/
//! retention math. This suite covers what only the app layer can: the
//! **audit-event emission** for each operation and the full operator
//! journey create → list → restore → validate ([[customer-journey-e2e-gate]],
//! here operator-internal but high-stakes).

use std::path::{Path, PathBuf};

use aberp::snapshot::{restore_and_emit, retention_and_emit, take_and_emit};
use aberp_audit_ledger::{Actor, BinaryHash, EventKind, Ledger, TenantId};
use aberp_snapshot::{list_snapshots, RetentionPolicy};
use duckdb::Connection;

// ── scaffolding ────────────────────────────────────────────────────────

struct ScopedTempDir(PathBuf);
impl ScopedTempDir {
    fn new(label: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static C: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let seq = C.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "aberp-s426-e2e-{label}-{}-{nanos}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
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

const TENANT: &str = "prod";

fn tid() -> TenantId {
    TenantId::new(TENANT.to_string()).unwrap()
}
fn bh() -> BinaryHash {
    BinaryHash::from_bytes([2u8; 32])
}
fn actor() -> Actor {
    Actor::test_only()
}

/// Seed `db` with an invoice table (+rows) and `n_audit` valid audit
/// entries (a well-formed chain).
fn seed(db: &Path, n_invoice: usize, n_audit: usize) {
    {
        let conn = Connection::open(db).unwrap();
        conn.execute_batch("CREATE TABLE IF NOT EXISTS invoice (id BIGINT, amount DOUBLE);")
            .unwrap();
        for i in 0..n_invoice {
            conn.execute(
                "INSERT INTO invoice VALUES (?, ?)",
                duckdb::params![i as i64, (i as f64) * 5.0],
            )
            .unwrap();
        }
    }
    let mut l = Ledger::open(db, tid(), bh()).unwrap();
    for i in 0..n_audit {
        l.append(
            EventKind::Test,
            format!("{{\"i\":{i}}}").into_bytes(),
            actor(),
            None,
        )
        .unwrap();
    }
}

/// All event kinds currently in the DB's ledger, in seq order.
fn ledger_kinds(db: &Path) -> Vec<EventKind> {
    let l = Ledger::open(db, tid(), bh()).unwrap();
    l.entries()
        .unwrap()
        .iter()
        .map(|e| e.kind.clone())
        .collect()
}

fn count_kind(db: &Path, kind: EventKind) -> usize {
    ledger_kinds(db).into_iter().filter(|k| *k == kind).count()
}

// ── tests ──────────────────────────────────────────────────────────────

#[test]
fn create_list_restore_journey_emits_events() {
    let dir = ScopedTempDir::new("journey");
    let db = dir.path().join("aberp.duckdb");
    seed(&db, 4, 3);
    let store = dir.path().join("store");

    // CREATE — emits SnapshotCreated against the live ledger.
    let before = ledger_kinds(&db).len();
    let rec = take_and_emit(&db, &store, &tid(), bh(), actor()).expect("take");
    assert!(rec.meta.valid, "fresh snapshot valid: {:?}", rec.meta);
    assert_eq!(rec.meta.audit_count, 3);
    assert_eq!(rec.meta.invoice_count, 4);
    assert_eq!(count_kind(&db, EventKind::SnapshotCreated), 1);
    assert_eq!(
        ledger_kinds(&db).last().cloned(),
        Some(EventKind::SnapshotCreated),
        "SnapshotCreated is the newest ledger entry"
    );
    assert!(ledger_kinds(&db).len() > before);

    // LIST — the snapshot is discoverable.
    let listed = list_snapshots(&store).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].meta.seq, rec.meta.seq);

    // RESTORE — to a side path; emits SnapshotRestored. The restored DB
    // carries the same invoice rows (validate the round-trip end-to-end).
    let target = dir.path().join("recovery").join("aberp.duckdb");
    let selector = rec.meta.seq.to_string();
    restore_and_emit(&db, &store, &selector, &target, &tid(), bh(), actor()).expect("restore");
    assert!(target.exists());
    assert_eq!(count_kind(&db, EventKind::SnapshotRestored), 1);

    let conn = Connection::open(&target).unwrap();
    let n: i64 = conn
        .query_row("SELECT count(*) FROM invoice", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 4, "restored DB has the original invoice rows");
}

#[test]
fn validation_failure_emits_validation_failed_event() {
    let dir = ScopedTempDir::new("valfail");
    let db = dir.path().join("aberp.duckdb");
    seed(&db, 1, 3);
    // Tamper the chain so validation must fail.
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch("UPDATE audit_ledger SET payload = 'x'::BLOB WHERE seq = 1;")
            .unwrap();
    }

    let store = dir.path().join("store");
    let rec = take_and_emit(&db, &store, &tid(), bh(), actor()).expect("take produces a record");
    assert!(!rec.meta.valid, "tampered chain must fail validation");
    assert_eq!(count_kind(&db, EventKind::SnapshotValidationFailed), 1);
    assert_eq!(count_kind(&db, EventKind::SnapshotCreated), 0);
}

#[test]
fn retention_emits_pruned_event_and_removes_dirs() {
    let dir = ScopedTempDir::new("retain");
    let db = dir.path().join("aberp.duckdb");
    seed(&db, 1, 1);
    let store = dir.path().join("store");

    // Take three snapshots.
    for _ in 0..3 {
        take_and_emit(&db, &store, &tid(), bh(), actor()).unwrap();
    }
    assert_eq!(list_snapshots(&store).unwrap().len(), 3);

    // Retain only the newest valid (keep_last=1, no day/week windows).
    let policy = RetentionPolicy {
        keep_last: 1,
        daily_days: 0,
        weekly_weeks: 0,
    };
    let removed = retention_and_emit(&db, &store, &tid(), bh(), actor(), &policy).unwrap();
    assert_eq!(removed.len(), 2, "two older snapshots pruned");
    assert_eq!(list_snapshots(&store).unwrap().len(), 1);
    assert_eq!(count_kind(&db, EventKind::SnapshotPruned), 1);
}
