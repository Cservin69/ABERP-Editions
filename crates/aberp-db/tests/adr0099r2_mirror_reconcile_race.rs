//! ADR-0099 R2 — the MIRROR-side seq fork (prod recurrence #5, seq 2508).
//!
//! # The defect
//!
//! The audit ledger has two halves: the DuckDB `audit_ledger` table and the
//! `<db>.audit.log` MIRROR. Four prod forks (seq 369/416/428/515) were
//! table-side and were closed by routing every in-process audit append through
//! the ONE shared [`Handle`]. The fifth was MIRROR-side, and the shared Handle
//! did not cover it, because the mirror had a SECOND writer:
//! [`ensure_consistent_with_db`], run by the snapshot daemon on its own
//! `Connection::open`.
//!
//! Every branch of that reconciler acts on a SAMPLE of two heads — the DB's and
//! the mirror's. Before R2 the sample was taken with NO lock held, and only the
//! append helper locked. So a lockstep `sync_mirror` (fired by any other
//! writer's `WriteGuard::drop`) could extend the mirror between the sample and
//! the act, and the reconciler would then re-append the very seqs that were
//! already there. The mirror ends up with duplicated, non-ascending seqs — and
//! the next Defense boot reads that as a forked mirror and REFUSES to start.
//! The rows that got duplicated were poll heartbeats, for the boring reason
//! that the highest-frequency writer is the one most likely to land in the
//! window.
//!
//! # What proves what (read this before changing a test here)
//!
//! Following `audit_lock_domain_e2e.rs`: a thread race proves a hazard EXISTS,
//! never that one is GONE. So the load-bearing test here is the DETERMINISTIC
//! one:
//!
//! * [`reconcile_parks_on_the_mirror_lock_before_it_samples_any_head`] — the
//!   fix. The test holds the mirror's advisory lock and asserts the reconciler
//!   cannot even RETURN, on a tree state (mirror == DB) whose branch never
//!   touches the append helper at all. Pre-R2 that branch took no lock ever, so
//!   it returned immediately with the lock held by someone else; that is the
//!   discriminator, and it does not depend on any interleaving.
//! * [`a_reconcile_holding_the_lock_cannot_duplicate_a_lockstep_append`] — the
//!   outcome. Corroboration that the property above actually prevents the
//!   duplicate seqs, asserted on the mirror's CONTENT rather than on timing.
//!
//! Same build gate as the other files here: real DuckDB, so Mac/CI only.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use aberp_audit_ledger::{
    append_in_tx, ensure_consistent_with_db, ensure_schema, mirror_path_for, read_mirror_entries,
    Actor, BinaryHash, EventKind, LedgerMeta, RecoveryAction, TenantId,
};
use aberp_db::{Handle, HandleConfig};
use fs2::FileExt;

const TENANT: &str = "defense";
/// Long enough that a reconciler which does NOT lock up front has finished its
/// head sample many times over, short enough to keep the suite quick. The
/// assertions never depend on this being "just right": the parked side is
/// provably parked (it holds no lock it could make progress with) and the
/// unparked side returns in single-digit ms.
const WINDOW: Duration = Duration::from_millis(400);

struct Tmp(PathBuf);
impl Tmp {
    fn new(label: &str) -> Self {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p = std::env::temp_dir().join(format!("aberp-r2-{label}-{}-{n}", std::process::id()));
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

fn handle(db: &Path) -> std::sync::Arc<Handle> {
    Handle::open(
        db,
        tenant(),
        HandleConfig {
            checkpoint_enabled: false,
            ..Default::default()
        },
    )
    .unwrap()
}

/// One daemon-heartbeat-shaped append through the shared writer. The
/// `WriteGuard` drop runs the lockstep `sync_mirror`, exactly as in `serve`.
fn heartbeat(h: &Handle, tag: &str) {
    let mut guard = h.write().unwrap();
    ensure_schema(&guard).unwrap();
    let conn = guard.conn();
    let tx = conn.transaction().unwrap();
    append_in_tx(
        &tx,
        &LedgerMeta::new(tenant(), BinaryHash::from_bytes([7u8; 32])),
        EventKind::QuoteIntakePollAttempted,
        format!("{{\"probe\":\"{tag}\"}}").into_bytes(),
        Actor::from_local_cli(format!("ulid-{tag}"), "tester"),
        None,
    )
    .unwrap();
    tx.commit().unwrap();
}

/// Take the mirror's exclusive advisory lock the same way every mirror writer
/// does. Holding this is what a concurrent `sync_mirror` holds.
fn lock_mirror(mirror: &Path) -> std::fs::File {
    let f = OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(mirror)
        .unwrap();
    f.lock_exclusive().unwrap();
    f
}

fn mirror_seqs(mirror: &Path) -> Vec<u64> {
    read_mirror_entries(mirror)
        .expect("mirror must stay readable: duplicated / non-ascending seqs make this Err")
        .into_iter()
        .map(|e| e.seq)
        .collect()
}

/// THE FIX. `ensure_consistent_with_db` must hold the mirror's exclusive lock
/// across its WHOLE decide→act window, starting BEFORE it samples either head.
///
/// The tree state here is mirror == DB, whose branch (`Unchanged`) returns
/// without ever calling the append helper. Pre-R2 the append helper was the ONLY
/// thing that locked, so this branch completed happily while another writer held
/// the lock. Post-R2 it cannot even start. Deterministic: no interleaving is
/// required for either verdict.
#[test]
fn reconcile_parks_on_the_mirror_lock_before_it_samples_any_head() {
    let t = Tmp::new("park");
    let db = t.db();
    let h = handle(&db);
    for i in 0..3 {
        heartbeat(&h, &format!("seed-{i}"));
    }
    let mirror = mirror_path_for(&db);
    assert_eq!(
        mirror_seqs(&mirror),
        vec![1, 2, 3],
        "the lockstep sync should have mirrored all three seed heartbeats"
    );

    // A concurrent mirror writer holds the lock.
    let held = lock_mirror(&mirror);

    let (tx, rx) = mpsc::channel();
    let conn = h.read().unwrap();
    let m = mirror.clone();
    let worker = thread::spawn(move || {
        let out = ensure_consistent_with_db(&conn, &m);
        let _ = tx.send(out);
    });

    match rx.recv_timeout(WINDOW) {
        Err(mpsc::RecvTimeoutError::Timeout) => { /* parked — the invariant */ }
        other => panic!(
            "ensure_consistent_with_db RETURNED while another writer held the mirror lock \
             ({other:?}). It sampled the DB head and the mirror head outside the lock, which is \
             exactly how the seq-2508 duplicate was produced — a lockstep append landing in that \
             window is re-appended verbatim."
        ),
    }

    // Release; the reconciler must now complete and find nothing to do.
    FileExt::unlock(&held).unwrap();
    drop(held);
    let out = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("reconciler must proceed once the mirror lock is free")
        .expect("reconcile of a mirror that equals the DB must succeed");
    assert_eq!(
        out,
        RecoveryAction::Unchanged,
        "mirror == DB, so the reconcile is a no-op"
    );
    assert_eq!(mirror_seqs(&mirror), vec![1, 2, 3]);
    worker.join().unwrap();
}

/// THE OUTCOME the lock buys, asserted on the mirror's CONTENT.
///
/// Setup is the shape the snapshot daemon hit in prod: the mirror is BEHIND the
/// DB, so the reconciler takes the `Extended` branch — the one that appends. A
/// concurrent mirror writer then lands the very same entries while the
/// reconciler is in flight. Post-R2 the reconciler is still waiting for the lock
/// at that point and re-samples afterwards, so it appends nothing. Pre-R2 it had
/// already decided `after_seq = 3` and appended seqs 4 and 5 a SECOND time,
/// leaving a mirror whose seqs do not ascend — which `read_mirror_entries`
/// rejects and Defense's boot refuses.
#[test]
fn a_reconcile_holding_the_lock_cannot_duplicate_a_lockstep_append() {
    let t = Tmp::new("dup");
    let db = t.db();
    let h = handle(&db);
    for i in 0..3 {
        heartbeat(&h, &format!("seed-{i}"));
    }
    let mirror = mirror_path_for(&db);

    // Two more rows that the mirror has NOT yet seen: write them with the
    // lockstep sync suppressed by holding the mirror lock across the commit, so
    // the guard's post-commit `sync_mirror`… would block. Simpler and equally
    // faithful: append them to the DB directly on the shared writer and then
    // TRIM the mirror back to 3 lines, which is the state a crashed sync leaves.
    for i in 3..5 {
        heartbeat(&h, &format!("late-{i}"));
    }
    let full = std::fs::read(&mirror).unwrap();
    let keep = nth_line_end(&full, 3);
    std::fs::write(&mirror, &full[..keep]).unwrap();
    assert_eq!(
        mirror_seqs(&mirror),
        vec![1, 2, 3],
        "mirror trimmed behind DB"
    );

    // A concurrent mirror writer takes the lock, then — while the reconciler is
    // in flight — appends exactly the delta the reconciler would have appended,
    // then releases.
    let held = lock_mirror(&mirror);
    let conn = h.read().unwrap();
    let m = mirror.clone();
    let worker = thread::spawn(move || ensure_consistent_with_db(&conn, &m));

    thread::sleep(WINDOW); // a non-locking reconciler has long since sampled
    std::fs::OpenOptions::new()
        .append(true)
        .open(&mirror)
        .unwrap();
    {
        use std::io::Write;
        let mut sink = &held;
        sink.write_all(&full[keep..]).unwrap();
        sink.flush().unwrap();
    }
    FileExt::unlock(&held).unwrap();
    drop(held);

    let action = worker
        .join()
        .unwrap()
        .expect("reconcile must succeed, not report divergence");
    assert_eq!(
        mirror_seqs(&mirror),
        vec![1, 2, 3, 4, 5],
        "the reconciler re-appended entries the concurrent writer had already \
         landed: duplicated seqs in the mirror ({action:?}). This is the seq-2508 \
         fork — the next boot reads the mirror as corrupt and refuses."
    );
}

/// Byte offset just past the `n`-th newline (the mirror is JSON-Lines).
fn nth_line_end(bytes: &[u8], n: usize) -> usize {
    let mut seen = 0;
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'\n' {
            seen += 1;
            if seen == n {
                return i + 1;
            }
        }
    }
    bytes.len()
}

/// The boot reconcile must stay a clean no-op while daemon-shaped heartbeats and
/// foreground writes run concurrently through the shared writer — one dense,
/// monotonic chain, and a mirror that equals it.
///
/// Corroboration only (see the module docs): a green run does not by itself
/// prove the lock works. What it does prove is that the R2 lock introduces no
/// deadlock between the mirror flock and the handle's writer mutex, and that
/// contention leaves neither half of the ledger forked.
#[test]
fn boot_reconcile_stays_clean_under_concurrent_daemon_and_foreground_writes() {
    let t = Tmp::new("concurrent");
    let db = t.db();
    let h = handle(&db);
    heartbeat(&h, "boot");
    let mirror = mirror_path_for(&db);

    const WRITERS: usize = 4;
    const ROUNDS: usize = 25;
    let mut threads = Vec::new();
    for w in 0..WRITERS {
        let hc = h.clone();
        threads.push(thread::spawn(move || {
            for r in 0..ROUNDS {
                heartbeat(&hc, &format!("w{w}-r{r}"));
            }
        }));
    }
    // The boot-shaped reconcile, running against the same live instance while
    // the writers hammer it.
    let hr = h.clone();
    let m = mirror.clone();
    let reconciler = thread::spawn(move || {
        let mut actions = Vec::new();
        for _ in 0..10 {
            let conn = hr.read().unwrap();
            actions.push(ensure_consistent_with_db(&conn, &m));
            thread::sleep(Duration::from_millis(5));
        }
        actions
    });

    for th in threads {
        th.join().unwrap();
    }
    for a in reconciler.join().unwrap() {
        a.expect("a concurrent reconcile must never report divergence or corruption");
    }

    let expected: u64 = 1 + (WRITERS * ROUNDS) as u64;
    let seqs = mirror_seqs(&mirror);
    assert_eq!(
        seqs,
        (1..=seqs.len() as u64).collect::<Vec<_>>(),
        "mirror seqs must be dense and strictly ascending — a duplicate or a gap IS the fork"
    );

    // Final reconcile brings the mirror level with the DB; then both halves agree.
    let conn = h.read().unwrap();
    ensure_consistent_with_db(&conn, &mirror).unwrap();
    assert_eq!(
        mirror_seqs(&mirror).len() as u64,
        expected,
        "every committed heartbeat must be in the mirror exactly once"
    );
    let n: i64 = conn
        .prepare("SELECT count(*) FROM audit_ledger")
        .unwrap()
        .query_row([], |r| r.get(0))
        .unwrap();
    assert_eq!(n as u64, expected, "DB row count must match the mirror");
}
