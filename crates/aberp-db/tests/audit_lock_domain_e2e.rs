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
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use aberp_audit_ledger::{
    append_in_tx, ensure_schema, Actor, BinaryHash, EventKind, Ledger, LedgerMeta, TenantId,
};
use aberp_db::{Handle, HandleConfig};
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

/// DOMAIN 2, PRE-FIX — the audit-ledger writer. `Ledger::append` holds
/// `AUDIT_APPEND_LOCK` and nothing else. Built on a `try_clone` of the shared
/// instance so ONLY the lock domain differs from `handle_domain_append`.
/// Retained (and exercised by [`unfixed_ledger_writer_still_forks`]) so the
/// test suite keeps PROVING the hazard is real rather than merely asserting the
/// fixed path is fine — a guard whose failure mode is never demonstrated is a
/// guard nobody can tell is still wired up.
fn ledger_domain_append_unguarded(handle: &Handle, tag: &str) {
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

/// DOMAIN 2, FIXED — the same `Ledger` work routed through
/// [`Handle::with_ledger`], which holds the writer mutex across it so the two
/// domains can no longer interleave. This is the shape the DÁP boot + heartbeat
/// paths now use.
fn ledger_domain_append_via_handle(handle: &Handle, tag: &str) {
    handle
        .with_ledger(BinaryHash::from_bytes([7u8; 32]), |ledger| {
            ledger.append(
                EventKind::DbAutoRecovered,
                payload(tag),
                Actor::from_local_cli(format!("ulid-{tag}"), "tester"),
                None,
            )
        })
        .unwrap()
        .unwrap();
}

/// Run the two-domain race and return the chain verdict.
fn race(label: &str, ledger_arm: fn(&Handle, &str)) -> (PathBuf, Tmp, Arc<Handle>) {
    let tmp = Tmp::new(label);
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
                ledger_arm(&h, &format!("B{i}"));
            }
        })
    };
    a.join().unwrap();
    b.join().unwrap();
    (db, tmp, handle)
}

/// **THE LOAD-BEARING GUARD — deterministic, not a race.**
///
/// The race-based tests below are probabilistic: a fork needs writer B to read
/// the head inside writer A's read→commit window, and whether that happens on a
/// given run is timing. That is fine for DEMONSTRATING the hazard (one fork is
/// proof) but useless for proving it GONE — a green race run is equally
/// explained by "the mutex works" and by "the interleaving didn't happen this
/// time". Confirmed the hard way: with the writer mutex mutated OUT of
/// `with_ledger`, the race test still passed.
///
/// So the fix is pinned on the property itself: `with_ledger` must hold the
/// writer mutex FOR THE WHOLE DURATION OF ITS CLOSURE. That is the only thing
/// that makes the two domains exclusive — the append happens inside the closure,
/// so a mutex released before it runs protects nothing.
///
/// The direction matters. Asserting "`with_ledger` blocks while a handle writer
/// holds the guard" does NOT work: `Handle::read()` also takes the same mutex
/// briefly to `try_clone`, so the mutated build blocks too and the test passes
/// vacuously (verified — it did). The discriminating direction is the opposite
/// one: park inside `with_ledger`'s closure and prove a handle-routed writer is
/// locked OUT. With the mutation (`read()`), the guard is already released by
/// the time the closure runs, the writer sails in, and this fails every time.
#[test]
fn with_ledger_holds_the_writer_mutex_across_its_closure() {
    const HOLD: Duration = Duration::from_millis(600);

    let tmp = Tmp::new("exclusion");
    let db = tmp.db();
    seed(&db);
    // Checkpoint OFF: the WriteGuard's post-commit durable checkpoint can itself
    // take longer than the threshold, which would satisfy the assertion for the
    // wrong reason. (It did, on the first draft — the mutated build "passed"
    // purely on checkpoint time.) This test is about lock WAIT and nothing else.
    let handle = Handle::open(
        &db,
        tenant(),
        HandleConfig {
            checkpoint_enabled: false,
            ..HandleConfig::default()
        },
    )
    .unwrap();

    let inside = Arc::new(Barrier::new(2));
    let b = {
        let h: Arc<Handle> = handle.clone();
        let inside = inside.clone();
        thread::spawn(move || {
            h.with_ledger(BinaryHash::from_bytes([7u8; 32]), |ledger| {
                inside.wait(); // we are INSIDE the closure now
                thread::sleep(HOLD);
                ledger
                    .append(
                        EventKind::DbAutoRecovered,
                        payload("inside"),
                        Actor::from_local_cli("ulid-inside".to_string(), "tester"),
                        None,
                    )
                    .unwrap();
            })
            .unwrap();
        })
    };

    inside.wait();
    // B is inside `with_ledger`'s closure. A handle-routed writer must WAIT.
    // Time ONLY the guard acquisition — not the append, and not the guard's
    // drop hooks (mirror sync + checkpoint), whose cost would mask the signal.
    let started = Instant::now();
    let g = handle.write().unwrap();
    let waited = started.elapsed();
    drop(g);
    b.join().unwrap();

    assert!(
        waited >= HOLD / 2,
        "a handle-routed writer acquired the guard in {waited:?} while another \
         thread was inside with_ledger's closure (which parked for {HOLD:?}) — \
         with_ledger is NOT holding the writer mutex across the append, so the \
         two audit lock domains are still disjoint and the chain can fork"
    );
}

/// MUTATION GUARD for the race harness. The UNGUARDED `Ledger` writer must
/// still be able to fork when raced against a handle-routed writer. If this
/// stops forking, the race harness has stopped interleaving and
/// [`concurrent_handle_and_ledger_writers_keep_one_chain`] is vacuous.
///
/// Retried because a single race is probabilistic; forking even once proves the
/// hazard, so ATTEMPTS independent races and needs only one to fork.
#[test]
fn unfixed_ledger_writer_still_forks() {
    const ATTEMPTS: usize = 6;
    for attempt in 0..ATTEMPTS {
        let (db, _tmp, _h) = race(&format!("unguarded{attempt}"), ledger_domain_append_unguarded);
        let ledger = Ledger::open(&db, tenant(), BinaryHash::from_bytes([7u8; 32])).unwrap();
        if ledger.verify_chain().is_err() {
            return; // hazard demonstrated
        }
    }
    panic!(
        "the UNGUARDED cross-domain race did not fork in {ATTEMPTS} attempts — the \
         race harness has stopped interleaving, so the fixed-path race test proves \
         nothing. Fix the harness (more rounds / tighter window)."
    );
}

/// End-to-end CORROBORATION (not the load-bearing guard — see
/// [`with_ledger_is_mutually_exclusive_with_a_handle_writer`] for that). Runs the
/// real workload through [`Handle::with_ledger`] and proves chain integrity with
/// `verify_chain` rather than a row count: a fork leaves the row COUNT correct
/// and only breaks the links, which is why the prod incidents were invisible
/// until someone verified the chain.
#[test]
fn concurrent_handle_and_ledger_writers_keep_one_chain() {
    let (db, _tmp, _h) = race("domains", ledger_domain_append_via_handle);

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
