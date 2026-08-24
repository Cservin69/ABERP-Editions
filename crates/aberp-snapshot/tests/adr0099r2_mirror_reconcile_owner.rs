//! ADR-0099 R2 — the snapshot EXPORT connection must not be a MIRROR writer.
//!
//! `take_snapshot`'s export connection is an INDEPENDENT `Connection::open` of
//! the live DB, retained as a sanctioned residual on the stated grounds that it
//! is "a LOGICAL, READ-ONLY operation ... it never writes the live file". That
//! is true, and it was never the question: the step immediately before the
//! EXPORT was `ensure_consistent_with_db`, which WRITES the audit mirror — the
//! ledger's other half — from a second DuckDB instance that does not replay the
//! shared writer's WAL and therefore reads a stale `db_max_seq`.
//!
//! R2 makes the owner explicit. In `aberp serve` the reconcile is hoisted to the
//! caller and runs under the ONE shared `aberp_db::Handle` writer; the CLI keeps
//! reconciling on the export connection, which is the only opener in its
//! process and so cannot race anything.
//!
//! These tests pin BOTH arms, because the failure mode of the fix is silence:
//! an arm that quietly still reconciles here would look identical in every
//! passing snapshot test.

use std::path::{Path, PathBuf};

use aberp_audit_ledger::{mirror_path_for, Actor, BinaryHash, EventKind, Ledger, TenantId};
use aberp_snapshot::{take_snapshot_with, MirrorReconcile};
use duckdb::Connection;
use time::OffsetDateTime;

struct ScopedTempDir(PathBuf);
impl ScopedTempDir {
    fn new(label: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "aberp-r2-owner-{label}-{}-{nanos}",
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

/// A DB with a short, valid audit chain and NO mirror file — so "did this
/// module write the mirror?" is answerable by the file's mere existence.
fn seed(path: &Path, tenant: &str) {
    {
        let conn = Connection::open(path).expect("open db");
        conn.execute_batch("CREATE TABLE IF NOT EXISTS invoice (id BIGINT);")
            .expect("create invoice");
    }
    let tid = TenantId::new(tenant.to_string()).expect("tenant");
    let mut ledger =
        Ledger::open(path, tid, BinaryHash::from_bytes([1u8; 32])).expect("open ledger");
    for i in 0..3 {
        ledger
            .append(
                EventKind::Test,
                format!("{{\"i\":{i}}}").into_bytes(),
                Actor::test_only(),
                None,
            )
            .expect("append");
    }
    drop(ledger);
    let mirror = mirror_path_for(path);
    let _ = std::fs::remove_file(&mirror);
    assert!(!mirror.exists(), "fixture must start with no mirror file");
}

#[test]
fn already_done_by_caller_leaves_the_mirror_untouched() {
    let t = ScopedTempDir::new("caller");
    let db = t.path().join("aberp.duckdb");
    let store = t.path().join("store");
    seed(&db, "defense");

    let rec = take_snapshot_with(
        &db,
        &store,
        "defense",
        OffsetDateTime::now_utc(),
        MirrorReconcile::AlreadyDoneByCaller,
    )
    .expect("snapshot must still be taken");
    assert!(rec.dir.exists(), "the EXPORT itself is unaffected");

    assert!(
        !mirror_path_for(&db).exists(),
        "the export connection WROTE the audit mirror. In `aberp serve` that is a \
         second mirror writer on a stale non-shared instance — the seq-2508 fork \
         (ADR-0099 R2). The caller owns this reconcile and already ran it under the \
         shared Handle writer."
    );
}

#[test]
fn on_export_connection_still_reconciles_for_the_cli() {
    let t = ScopedTempDir::new("cli");
    let db = t.path().join("aberp.duckdb");
    let store = t.path().join("store");
    seed(&db, "defense");

    take_snapshot_with(
        &db,
        &store,
        "defense",
        OffsetDateTime::now_utc(),
        MirrorReconcile::OnExportConnection,
    )
    .expect("snapshot");

    assert!(
        mirror_path_for(&db).exists(),
        "the CLI arm must keep its pre-R2 behaviour: with no shared Handle in the \
         process, the export connection is the only opener and reconciling on it is \
         both coherent and race-free"
    );
}
