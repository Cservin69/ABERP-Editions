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
//! # What proves what (read this before changing a test here)
//!
//! The original failing-first repro was a thread race asserted with
//! `verify_chain`. It reproduced the fork immediately on a dev box — and it is
//! NOT kept as a guard, because a race proves a hazard EXISTS (one fork is
//! proof) but can never prove one is GONE: a green run is equally explained by
//! "the mutex works" and "the interleaving didn't happen". Both halves of that
//! were observed for real — the race passed with the mutex mutated OUT on an
//! 8-core box, and its unguarded counterpart failed to fork at all in 6
//! attempts on a 2-core CI runner. So the two load-bearing tests are
//! DETERMINISTIC:
//!
//! * [`two_unserialized_appenders_fork_the_chain`] — the hazard. Two appenders
//!   with no shared serialization point, transactions ordered by hand, no
//!   threads. Both take `seq = head + 1`; the chain forks, every time.
//! * [`with_ledger_holds_the_writer_mutex_across_its_closure`] — the fix. Parks
//!   inside the closure and proves a handle-routed writer is locked out.
//!
//! [`concurrent_handle_and_ledger_writers_keep_one_chain`] remains only as
//! corroboration that the fixed path survives real contention (no deadlock
//! between the two locks, no corruption).
//!
//! Note both use `Handle::read()` — a `try_clone` of the SAME DuckDB instance —
//! so the variable under test is the LOCK DOMAIN alone. A separate
//! `Connection::open` (what the live heartbeat did) would additionally drag in
//! the ADR-0098 Gap-1a two-instance tear and confound the result. That is also
//! why `Handle::with_ledger` cannot just hand out a clone: a shared instance is
//! necessary but NOT sufficient — the caller must hold the writer mutex too.
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
/// Rounds per writer for the corroboration race. Sized for real contention, NOT
/// as a fork detector — see the module docs on why no assertion here may depend
/// on the threads actually interleaving.
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
/// A race cannot pin this: with the writer mutex mutated OUT of `with_ledger`,
/// the race test still passed.
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

/// **THE HAZARD, DEMONSTRATED DETERMINISTICALLY** — no threads, no timing, no
/// retries.
///
/// This replaced a retried race (`ATTEMPTS` independent races, needing one to
/// fork). That version was green on an 8-core dev box and **failed on a 2-core
/// CI runner, which never interleaved in 6 attempts** — a test whose verdict
/// depends on the scheduler is worthless as a guard in either direction.
///
/// The fork needs no concurrency at all, only two appenders with **no shared
/// serialization point**. Each opens its own transaction, so each reads the same
/// committed head inside its own snapshot, and each self-assigns
/// `seq = head + 1`. `UNIQUE(seq)` is gone, so both commit and the chain forks.
/// Ordering the two transactions by hand makes that inevitable rather than
/// likely — which is exactly the property the two disjoint lock domains fail to
/// provide, and exactly what [`Handle::with_ledger`] restores.
#[test]
fn two_unserialized_appenders_fork_the_chain() {
    let tmp = Tmp::new("deterministic");
    let db = tmp.db();
    seed(&db);
    let handle = Handle::open(
        &db,
        tenant(),
        HandleConfig {
            checkpoint_enabled: false,
            ..HandleConfig::default()
        },
    )
    .unwrap();

    // One committed row first, so "the head both writers read" is a real row
    // rather than the genesis special case.
    handle_domain_append(&handle, "seed");

    // Two connections on the SAME instance (`read()` is a try_clone), so this is
    // NOT the ADR-0098 two-instance hazard — only the missing serialization.
    let mut c1 = handle.read().unwrap();
    let mut c2 = handle.read().unwrap();

    // Both transactions open BEFORE either commits: each sees the same head.
    let tx1 = c1.transaction().unwrap();
    let tx2 = c2.transaction().unwrap();
    append_in_tx(
        &tx1,
        &meta(),
        EventKind::DbAutoRecovered,
        payload("A"),
        Actor::from_local_cli("ulid-A".to_string(), "tester"),
        None,
    )
    .unwrap();
    append_in_tx(
        &tx2,
        &meta(),
        EventKind::DbAutoRecovered,
        payload("B"),
        Actor::from_local_cli("ulid-B".to_string(), "tester"),
        None,
    )
    .unwrap();
    tx1.commit().unwrap();
    tx2.commit().unwrap();

    let ledger = Ledger::open(&db, tenant(), BinaryHash::from_bytes([7u8; 32])).unwrap();
    let verdict = ledger.verify_chain();
    assert!(
        verdict.is_err(),
        "two appenders with no shared serialization point BOTH took seq = head + 1, \
         yet the chain verified clean ({verdict:?}). Either DuckDB now rejects the \
         second commit or a UNIQUE(seq) constraint is back — in which case the \
         premise behind Handle::with_ledger has changed and ADR-0105 must be \
         revisited, not this assertion relaxed."
    );
}

/// End-to-end CORROBORATION — explicitly NOT a fork detector.
///
/// 300 real appends through [`Handle::with_ledger`] against a concurrent
/// handle-routed writer, verified with `verify_chain` (not a row count — a fork
/// leaves the COUNT correct and only breaks the links, which is why the prod
/// incidents stayed invisible until someone verified the chain).
///
/// What this DOES prove: the fixed path survives real contention — no deadlock
/// between the writer mutex and `AUDIT_APPEND_LOCK`, no panic, no corruption,
/// and the chain is contiguous afterwards. What it does NOT prove: that the
/// mutex is doing the work. A green run here is equally explained by "the
/// interleaving never happened", which is precisely what a 2-core CI runner
/// does. The load-bearing guards are
/// [`with_ledger_holds_the_writer_mutex_across_its_closure`] (deterministic
/// exclusion) and [`two_unserialized_appenders_fork_the_chain`] (deterministic
/// hazard).
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
