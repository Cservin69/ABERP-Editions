//! ADR-0082 follow-up (chunk 3) — DuckDB-backed crash-injection tests for
//! the crash-safe durable checkpoint. These open real DuckDB files, so they
//! run under `cargo test -p aberp-snapshot` on the Mac gate (the bundled
//! DuckDB amalgamation cannot build in the saw-off sandbox). The PURE
//! crash-safe COMMIT semantics (crash-between-write-and-rename) are unit
//! tested in `crash_safe.rs` and run anywhere.

use std::path::Path;

use aberp_audit_ledger::{Actor, BinaryHash, EventKind, Ledger, TenantId};
use aberp_snapshot::{checkpoint_is_current, durable_checkpoint, marker_path};
use duckdb::Connection;

struct Tmp(std::path::PathBuf);
impl Tmp {
    fn new(label: &str) -> Self {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p =
            std::env::temp_dir().join(format!("aberp-ckpt-it-{label}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        Tmp(p)
    }
    fn db(&self) -> std::path::PathBuf {
        self.0.join("aberp.duckdb")
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn seed(path: &Path, tenant: &str, n_invoice: usize, n_audit: usize) {
    {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch("CREATE TABLE IF NOT EXISTS invoice (id BIGINT, amount DOUBLE);")
            .unwrap();
        for i in 0..n_invoice {
            conn.execute(
                "INSERT INTO invoice VALUES (?, ?)",
                duckdb::params![i as i64, i as f64],
            )
            .unwrap();
        }
    }
    let tid = TenantId::new(tenant.to_string()).unwrap();
    let mut ledger = Ledger::open(path, tid, BinaryHash::from_bytes([1u8; 32])).unwrap();
    for i in 0..n_audit {
        ledger
            .append(
                EventKind::Test,
                format!("{{\"i\":{i}}}").into_bytes(),
                Actor::test_only(),
                None,
            )
            .unwrap();
    }
}

fn invoice_ids(path: &Path) -> Vec<i64> {
    let conn = Connection::open(path).unwrap();
    let mut stmt = conn.prepare("SELECT id FROM invoice ORDER BY id").unwrap();
    let v = stmt
        .query_map([], |r| r.get::<_, i64>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    v
}

fn sha_of(path: &Path) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(std::fs::read(path).unwrap());
    h.finalize().to_vec()
}

#[test]
fn durable_checkpoint_round_trips_rows_and_marks_good() {
    let t = Tmp::new("rt");
    let db = t.db();
    seed(&db, "acme", 3, 4);
    // Before: no verified-good marker.
    assert!(!checkpoint_is_current(&db));

    let rep = durable_checkpoint(&db, "acme").expect("checkpoint ok on a healthy DB");
    assert!(rep.validated);

    // The live file is the freshly installed, self-contained checkpoint…
    assert_eq!(
        invoice_ids(&db),
        vec![0, 1, 2],
        "rows survive the checkpoint swap"
    );
    // …its marker is present and current…
    assert!(marker_path(&db).exists(), "verified-good marker written");
    assert!(
        checkpoint_is_current(&db),
        "marker matches the installed file"
    );
    // …and the WAL was folded away (self-contained file).
    let wal = {
        let mut o = db.as_os_str().to_owned();
        o.push(".wal");
        std::path::PathBuf::from(o)
    };
    assert!(!wal.exists(), "no WAL beside a freshly checkpointed file");
}

#[test]
fn durable_checkpoint_refuses_corrupt_db_and_leaves_file_byte_identical() {
    let t = Tmp::new("corrupt");
    let db = t.db();
    seed(&db, "acme", 2, 3);
    // Tamper a committed payload so the hash chain no longer verifies — the
    // logical export will fail validation.
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch("UPDATE audit_ledger SET payload = 'tampered'::BLOB WHERE seq = 1;")
            .unwrap();
    }
    let before = sha_of(&db);

    let err = durable_checkpoint(&db, "acme")
        .expect_err("a corrupt DB must NOT be checkpointed over itself");
    assert!(
        err.to_string().contains("did not validate") || err.to_string().contains("corrupt"),
        "error should name the validation refusal: {err}"
    );

    // The crucial property: the OLD (corrupt-but-original) live file is left
    // byte-for-byte intact — we never tore it; recovery proceeds from the
    // periodic logical snapshots, not from a half-swapped file.
    let after = sha_of(&db);
    assert_eq!(
        before, after,
        "live DB must be byte-identical after a refused checkpoint"
    );
    // And we did NOT write a verified-good marker for a DB we refused.
    assert!(!checkpoint_is_current(&db));
}

#[test]
fn durable_checkpoint_is_repeatable() {
    let t = Tmp::new("again");
    let db = t.db();
    seed(&db, "acme", 1, 2);
    durable_checkpoint(&db, "acme").expect("first checkpoint");
    assert!(checkpoint_is_current(&db));
    // A second checkpoint still succeeds and preserves the data.
    durable_checkpoint(&db, "acme").expect("second checkpoint");
    assert_eq!(invoice_ids(&db), vec![0]);
    assert!(checkpoint_is_current(&db));
}
