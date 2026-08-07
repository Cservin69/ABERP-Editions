//! ADR-0105 — the audit chain has TWO DISJOINT SERIALIZATION DOMAINS.
//!
//! `aberp_db::Handle::write()` parks on the handle's writer mutex; the audit
//! append it then runs (`append_in_tx`) takes **no** lock of its own. The other
//! audit path — `Ledger::append` / `Ledger::append_signed` / `append_reopen` —
//! takes the audit-ledger's process-wide `AUDIT_APPEND_LOCK` and never touches
//! the handle mutex. Neither lock excludes the other, so one writer in EACH
//! domain can read the same committed chain head and both self-assign
//! `seq = head + 1`. The `UNIQUE(seq)` ART was dropped (see
//! `storage/schema.rs`), so both rows commit and the tamper-evident chain
//! FORKS — `verify_chain` → `Chain(OutOfOrder { expected, found })`, the exact
//! recurring prod-incident signature (seq 369 / 416 / 428 / 515).
//!
//! This is PRE-EXISTING, not a PR-33 regression: PR-33 moved the MES adapter
//! writer onto the shared handle, which removed the last *in-domain* fork but
//! could not close the cross-domain one. The live trigger today is
//! `audit_dap_boot::run_heartbeat_supervised` (+ `serve::spawn_dap_audit_chain`),
//! gated on the tenant's `dap_enabled` flag, which DEFAULTS OFF — latent, not
//! firing, but loaded.
//!
//! [`concurrent_handle_and_ledger_writers_keep_one_chain`] is the failing-first
//! proof. It deliberately builds the `Ledger` from `Handle::read()` — a
//! `try_clone` of the SAME DuckDB instance — so the only variable under test is
//! the LOCK DOMAIN. (A separate `Connection::open`, which is what the live
//! heartbeat actually does, would additionally drag in the ADR-0098 Gap-1a
//! two-instance tear and confound the result.) That framing is also the reason
//! `Handle::with_ledger` cannot just hand out a clone and call it a day: a
//! shared instance is necessary but NOT sufficient — the caller must hold the
//! writer mutex too, which is precisely what `with_ledger` does.
//!
//! Same build gate as `handle_concurrency_e2e.rs`: real DuckDB, so Mac/CI only.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use aberp_audit_ledger::{
    append_in_tx, ensure_schema, Actor, BinaryHash, EventKind, Ledger, LedgerMeta, TenantId,
};
use aberp_db::Handle;
use duckdb::Connection;

const TENANT: &str = "defense";
/// Rounds per writer. The fork is a race, so this needs enough interleavings to
/// be reliable rather than flaky-green; 150 × 2 threads reproduces on every run
/// observed on the pre-fix tree.
const ROUNDS: usize = 150;

struct Tmp(PathBuf);
impl Tmp {
    fn new(label: &str) -> Self {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p =
            std::env::temp_dir().join(format!("aberp-db-lock-{label}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        Tmp(p)
    }
    fn db(&self) -> PathBuf {
        self.0.join("aberp.duckdb")
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tenant() -> TenantId {
    TenantId::new(TENANT.to_string()).unwrap()
}

fn meta() -> LedgerMeta {
    LedgerMeta::new(tenant(), BinaryHash::from_bytes([7u8; 32]))
}

fn seed(db: &Path) {
    let conn = Connection::open(db).unwrap();
    ensure_schema(&conn).unwrap();
    conn.execute_batch("CHECKPOINT;").unwrap();
}

fn payload(tag: &str) -> Vec<u8> {
    format!("{{\"probe\":\"{tag}\"}}").into_bytes()
}

/// DOMAIN 1 — the shared-handle writer. `write()` holds the handle's writer
/// mutex; `append_in_tx` itself takes no lock.
fn handle_domain_append(handle: &Handle, tag: &str) {
    let mut guard = handle.write().unwrap();
    let conn = guard.conn();
    let tx = conn.transaction().unwrap();
    append_in_tx(
        &tx,
        &meta(),
        EventKind::DbAutoRecovered,
        payload(tag),
        Actor::from_local_cli(format!("ulid-{tag}"), "tester"),
        None,
    )
    .unwrap();
    tx.commit().unwrap();
}

/// DOMAIN 2 — the audit-ledger writer. `Ledger::append` holds
/// `AUDIT_APPEND_LOCK` and nothing else. Built on a `try_clone` of the shared
/// instance so ONLY the lock domain differs from `handle_domain_append`.
fn ledger_domain_append(handle: &Handle, tag: &str) {
    let conn = handle.read().unwrap();
    let mut ledger = Ledger::from_connection(conn, tenant(), BinaryHash::from_bytes([7u8; 32]));
    ledger
        .append(
            EventKind::DbAutoRecovered,
            payload(tag),
            Actor::from_local_cli(format!("ulid-{tag}"), "tester"),
            None,
        )
        .unwrap();
}

/// FAILING-FIRST. Two writers, one in each lock domain, on ONE DuckDB instance.
/// Pre-fix this forks the chain; post-fix (both routed through the handle's
/// writer mutex via `Handle::with_ledger`) it must hold one contiguous chain.
#[test]
fn concurrent_handle_and_ledger_writers_keep_one_chain() {
    let tmp = Tmp::new("domains");
    let db = tmp.db();
    seed(&db);
    let handle = Handle::open_default(&db, tenant()).unwrap();

    let a = {
        let h: Arc<Handle> = handle.clone();
        thread::spawn(move || {
            for i in 0..ROUNDS {
                handle_domain_append(&h, &format!("A{i}"));
            }
        })
    };
    let b = {
        let h: Arc<Handle> = handle.clone();
        thread::spawn(move || {
            for i in 0..ROUNDS {
                ledger_domain_append(&h, &format!("B{i}"));
            }
        })
    };
    a.join().unwrap();
    b.join().unwrap();

    // Integrity is proven with the hash chain, NOT a row count: a fork keeps the
    // row count correct while breaking the links, which is exactly why the prod
    // incidents were only ever caught by `verify_chain`.
    let ledger = Ledger::open(&db, tenant(), BinaryHash::from_bytes([7u8; 32])).unwrap();
    let verified = ledger.verify_chain().unwrap_or_else(|e| {
        panic!(
            "audit chain FORKED across the two lock domains: {e:?}\n\
             A handle-routed writer (Handle::write + append_in_tx, no audit lock) \
             and a Ledger writer (AUDIT_APPEND_LOCK, no handle mutex) read the same \
             head and both took the next seq."
        )
    });
    assert_eq!(
        verified,
        (ROUNDS * 2) as u64,
        "every append must be present exactly once in one contiguous chain"
    );
}
