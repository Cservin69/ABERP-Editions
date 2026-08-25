//! ADR-0099 R3 — a DIVERGENCE between the audit mirror and the DB is TERMINAL.
//!
//! R2 got the detection right and the handling wrong. Having correctly named the
//! seq-2508 shape (`MirrorDivergedFromDb`), it routed that shape to
//! `attempt_db_auto_recovery` — the snapshot+replay engine — on the reasoning
//! that `replay_mirror_delta` refuses a colliding replay with
//! `SequenceConflict`, so a diverged mirror could only reach `Recovered` if the
//! replay was genuinely conflict-free.
//!
//! That guarantee does not exist. `replay_mirror_delta` is only ever asked to
//! replay seqs ABOVE the imported snapshot's head, into a staging DB that by
//! construction holds only `[1..=snapshot_head]` — it never targets an occupied
//! seq, so it never collides. And even if it did, `audit_ledger` has no
//! `UNIQUE(seq)` (S341 dropped that ART index, duckdb#23046 / S332), so a
//! colliding INSERT still reports `rows_changed = 1` and returns `Ok`.
//!
//! The loss underneath is structural, not incidental, and
//! [`recovery_on_a_diverged_mirror_discards_the_dbs_own_rows`] measures it:
//! recovery rebuilds from a SNAPSHOT and replays the MIRROR's delta, so the DB's
//! rows at the re-used seqs — present in NEITHER input — are silently gone. Four
//! committed audit rows, a `Recovered` log line, and boot continuing on a WARN,
//! where the pre-R2 tree returned `MirrorCorruptPreserved` and went boot-fatal
//! so a human looked at it.
//!
//! Real DuckDB, so Mac/CI only.

use std::path::{Path, PathBuf};

use aberp_audit_ledger::{
    append_in_tx, ensure_consistent_with_db, ensure_schema, mirror_path_for, read_mirror_entries,
    sync_mirror, sync_mirror_lockstep, Actor, AppendError, BinaryHash, EventKind, LedgerMeta,
    LockstepSync, TenantId,
};
use aberp_db::{Handle, HandleConfig};
use fs2::FileExt;

const TENANT: &str = "defense";

struct Tmp(PathBuf);
impl Tmp {
    fn new(label: &str) -> Self {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p =
            std::env::temp_dir().join(format!("aberp-r3term-{label}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        Tmp(p)
    }
    fn db(&self) -> PathBuf {
        self.0.join("aberp.duckdb")
    }
    fn store(&self) -> PathBuf {
        self.0.join("snapshots")
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

/// One intake-poll heartbeat through the shared writer, exactly as the daemon
/// emits it (the same helper `adr0099r2_lost_commit_divergence.rs` uses).
fn beat(h: &Handle, at: &str) {
    let mut g = h.write().unwrap();
    ensure_schema(&g).unwrap();
    let c = g.conn();
    let tx = c.transaction().unwrap();
    append_in_tx(
        &tx,
        &LedgerMeta::new(tenant(), BinaryHash::from_bytes([7u8; 32])),
        EventKind::QuoteIntakePollAttempted,
        format!("{{\"at\":\"{at}\"}}").into_bytes(),
        Actor::from_local_cli(format!("ulid-{at}"), "daemon"),
        None,
    )
    .unwrap();
    tx.commit().unwrap();
}

/// Drop the DB's tail from `seq` on — the lost commits. The mirror is untouched,
/// which is the point: it was `fsync`ed and the DB was not.
fn lose_db_commits_from(h: &Handle, seq: u64) {
    let g = h.write().unwrap();
    g.execute_batch(&format!("DELETE FROM audit_ledger WHERE seq >= {seq};"))
        .unwrap();
}

fn mirror_seqs(mirror: &Path) -> Vec<u64> {
    read_mirror_entries(mirror)
        .expect("mirror stays readable")
        .into_iter()
        .map(|e| e.seq)
        .collect()
}

/// `(seq, entry_hash)` for every row in the DB, read through a plain connection
/// so it works with or without a live Handle.
fn db_rows(db: &Path) -> Vec<(u64, String)> {
    let conn = duckdb::Connection::open(db).unwrap();
    let mut st = conn
        .prepare("SELECT seq, hex(entry_hash) FROM audit_ledger ORDER BY seq")
        .unwrap();
    let rows = st
        .query_map([], |r| {
            Ok((r.get::<_, i64>(0)? as u64, r.get::<_, String>(1)?))
        })
        .unwrap();
    rows.map(|r| r.unwrap()).collect()
}

/// The seq-2508 fork shape, built for real: three heartbeats, a snapshot, two
/// more heartbeats, the DB loses those two, then FOUR more land — the first two
/// re-using the freed seqs 4 and 5, the next two advancing to 6 and 7.
///
/// Leaves the DB closed. Returns `(the DB's rows after the fork, mirror path)`.
fn build_fork_shape(t: &Tmp) -> (Vec<(u64, String)>, PathBuf) {
    let db = t.db();
    {
        let h = handle(&db);
        for at in ["19:16:44", "19:17:44", "19:18:44"] {
            beat(&h, at);
        }
    }
    // A valid snapshot at audit head 3 — the recovery engine's rebuild source.
    aberp_snapshot::take_snapshot(&db, &t.store(), TENANT, time::OffsetDateTime::now_utc())
        .expect("snapshot the healthy DB");
    {
        let h = handle(&db);
        for at in ["19:19:44", "19:20:44"] {
            beat(&h, at); // durable in the mirror…
        }
        lose_db_commits_from(&h, 4); // …and lost from the DB
        for at in ["19:21:44", "19:22:44", "19:23:44", "19:24:44"] {
            beat(&h, at); // re-uses 4,5 then advances to 6,7
        }
    }
    (db_rows(&db), mirror_path_for(&db))
}

// ── (a) DIVERGED IS TERMINAL ────────────────────────────────────────────────

/// THE REGRESSION. The fork shape must never be handed to the auto-recovery
/// engine, and [`aberp::serve::boot_mirror_route`] is where that is decided —
/// so the decision is asserted directly rather than inferred from a 30k-line
/// `run`. See `apps/aberp/src/serve.rs`'s own unit test for the classifier; this
/// one pins the input side: the fork shape really does produce
/// `MirrorDivergedFromDb`, with the evidence preserved and nothing mutated.
#[test]
fn the_fork_shape_refuses_and_preserves_without_touching_either_store() {
    let t = Tmp::new("terminal");
    let (db_before, mirror) = build_fork_shape(&t);
    assert_eq!(mirror_seqs(&mirror), vec![1, 2, 3, 4, 5]);
    assert_eq!(
        db_before.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5, 6, 7],
        "the DB re-used 4,5 and advanced to 6,7"
    );
    let mirror_before = std::fs::read(&mirror).unwrap();

    let conn = duckdb::Connection::open(t.db()).unwrap();
    match ensure_consistent_with_db(&conn, &mirror) {
        Err(AppendError::MirrorDivergedFromDb {
            first_divergent_seq,
            preserved,
            ..
        }) => {
            assert_eq!(first_divergent_seq, 4);
            assert!(
                Path::new(&preserved).exists(),
                "the mirror's copy of what the DB lost is preserved BEFORE refusing"
            );
        }
        other => panic!("the fork shape must refuse, got {other:?}"),
    }
    drop(conn);

    assert_eq!(
        std::fs::read(&mirror).unwrap(),
        mirror_before,
        "a refusal leaves the live mirror byte-identical — it holds the ONLY copy of the \
         entries the DB lost"
    );
    assert_eq!(
        db_rows(&t.db()),
        db_before,
        "and it leaves every DB row in place — the DB's entries at the re-used seqs are \
         the other half of the evidence"
    );
}

/// WHY the route is banned, measured rather than argued. This drives the REAL
/// `recover_or_refuse` over the fork shape and shows it reporting `Recovered`
/// while four committed DB rows cease to exist.
///
/// This is a CHARACTERISATION test of the recovery engine, not a specification
/// of it: `recover_or_refuse` is a snapshot+mirror rebuild and dropping rows
/// that are in neither input is inherent to what it does — which is exactly why
/// nothing may route a divergence into it. If a later change teaches the engine
/// to detect divergence and refuse, this test goes red: delete it and pin the
/// stronger property (recovery never returns `Recovered` while losing a row) in
/// its place.
#[test]
fn recovery_on_a_diverged_mirror_discards_the_dbs_own_rows() {
    let t = Tmp::new("measure");
    let (db_before, mirror) = build_fork_shape(&t);

    let outcome = aberp_snapshot::recover_or_refuse(&t.db(), &t.store(), &mirror, TENANT)
        .expect("the recovery engine runs to a decision");
    assert!(
        matches!(outcome, aberp_snapshot::RecoveryOutcome::Recovered { .. }),
        "the engine reports success on a shape it cannot actually resolve: {outcome:?}"
    );

    let after = db_rows(&t.db());
    let kept: std::collections::HashSet<&String> = after.iter().map(|(_, h)| h).collect();
    let dropped: Vec<u64> = db_before
        .iter()
        .filter(|(_, h)| !kept.contains(h))
        .map(|(s, _)| *s)
        .collect();
    assert_eq!(
        dropped,
        vec![4, 5, 6, 7],
        "FOUR committed DB rows are gone and the engine still said Recovered. Rows 4 and 5 \
         are replaced by the mirror's originals; rows 6 and 7 are in neither the snapshot \
         nor the mirror, so nothing rebuilt them. This is the silent loss that makes \
         `MirrorDivergedFromDb` terminal — a WARN in the log where the pre-R2 tree went \
         boot-fatal."
    );
}

// ── (b) AHEAD *AND* DIVERGED ────────────────────────────────────────────────

/// A mirror can be ahead on COUNT and still disagree over the shared prefix.
/// R2 proved the prefix only on the BEHIND branch, so this shape reported plain
/// `MirrorAheadOfDb` — the one condition boot auto-recovers — and recovery then
/// discarded the DB's divergent row. Divergence is a property of the SHARED
/// PREFIX, not of which store is longer, so it must be decided before the
/// length branch.
#[test]
fn an_ahead_but_diverged_mirror_refuses_instead_of_reporting_a_recoverable_ahead() {
    let t = Tmp::new("aheaddiv");
    let db = t.db();
    let h = handle(&db);
    for at in ["19:16:44", "19:17:44", "19:18:44", "19:19:44", "19:20:44"] {
        beat(&h, at);
    }
    let mirror = mirror_path_for(&db);
    lose_db_commits_from(&h, 4);
    beat(&h, "19:21:44"); // ONE replacement: the DB re-uses seq 4 only
    drop(h);

    assert_eq!(mirror_seqs(&mirror), vec![1, 2, 3, 4, 5]);
    let db_before = db_rows(&db);
    assert_eq!(
        db_before.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
        vec![1, 2, 3, 4],
        "the mirror is AHEAD on count (5 > 4) and yet they disagree at seq 4"
    );

    let conn = duckdb::Connection::open(&db).unwrap();
    match ensure_consistent_with_db(&conn, &mirror) {
        Err(AppendError::MirrorDivergedFromDb {
            first_divergent_seq,
            mirror_max_seq,
            db_max_seq,
            preserved,
        }) => {
            assert_eq!(first_divergent_seq, 4);
            assert_eq!((mirror_max_seq, db_max_seq), (5, 4));
            assert!(Path::new(&preserved).exists());
        }
        other => panic!(
            "an ahead-AND-diverged mirror reported as {other:?}. Reported as a plain AHEAD it \
             is routed to auto-recovery, which rebuilds from the snapshot + the mirror and \
             DISCARDS the DB's own row at seq 4."
        ),
    }
    drop(conn);
    assert_eq!(
        db_rows(&db),
        db_before,
        "the refusal leaves the DB's divergent row in place"
    );
}

// ── (c) A GENUINE CLEAN AHEAD STILL RECOVERS, LOSING NOTHING ────────────────

/// The case that MUST stay automatable. The mirror strictly EXTENDS the DB's
/// chain — every DB row is a prefix of the mirror — so snapshot+replay puts back
/// exactly what was lost and drops nothing. The prefix proof runs over
/// `[1..=db_max_seq]` only; comparing the whole mirror would read the mirror's
/// legitimately DB-absent tail as a divergence and turn every honest lost-tail
/// (and every intentional dev DB-nuke) into a boot-fatal refusal.
#[test]
fn a_clean_ahead_mirror_still_recovers_with_zero_row_loss() {
    let t = Tmp::new("cleanahead");
    let db = t.db();
    {
        let h = handle(&db);
        for at in ["19:16:44", "19:17:44", "19:18:44"] {
            beat(&h, at);
        }
    }
    aberp_snapshot::take_snapshot(&db, &t.store(), TENANT, time::OffsetDateTime::now_utc())
        .expect("snapshot the healthy DB");
    let mirror = mirror_path_for(&db);
    {
        let h = handle(&db);
        for at in ["19:19:44", "19:20:44"] {
            beat(&h, at);
        }
        lose_db_commits_from(&h, 4); // a CLEAN tail loss: nothing re-used
    }
    assert_eq!(mirror_seqs(&mirror), vec![1, 2, 3, 4, 5]);
    let db_before = db_rows(&db);
    assert_eq!(
        db_before.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );

    // Detection: AHEAD, not DIVERGED — the shared prefix [1..=3] agrees.
    let conn = duckdb::Connection::open(&db).unwrap();
    match ensure_consistent_with_db(&conn, &mirror) {
        Err(AppendError::MirrorAheadOfDb {
            mirror_max_seq,
            db_max_seq,
            ..
        }) => assert_eq!((mirror_max_seq, db_max_seq), (5, 3)),
        other => panic!(
            "a clean lost TAIL must stay AHEAD and stay recoverable, got {other:?}. Reported \
             as a divergence it becomes boot-fatal and the one condition this system can fix \
             by itself stops being fixed."
        ),
    }
    drop(conn);

    // Recovery: every row that existed in either store comes back.
    let outcome = aberp_snapshot::recover_or_refuse(&db, &t.store(), &mirror, TENANT).unwrap();
    assert!(
        matches!(outcome, aberp_snapshot::RecoveryOutcome::Recovered { .. }),
        "expected a clean recovery, got {outcome:?}"
    );
    let after = db_rows(&db);
    assert_eq!(
        after.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5],
        "the DB catches up to the mirror head"
    );
    let kept: std::collections::HashSet<&String> = after.iter().map(|(_, h)| h).collect();
    for (seq, hash) in &db_before {
        assert!(
            kept.contains(hash),
            "recovery dropped the DB's own row at seq {seq} — a clean AHEAD must lose nothing"
        );
    }
    let mirror_hashes: Vec<String> = read_mirror_entries(&mirror)
        .unwrap()
        .into_iter()
        .map(|e| e.entry_hash.to_uppercase())
        .collect();
    for (i, h) in mirror_hashes.iter().enumerate() {
        assert!(
            kept.contains(h),
            "recovery dropped the mirror's row at seq {} — the mirror-only entries are the \
             ONLY copy of what the DB lost",
            i + 1
        );
    }
}

// ── (d) THE CROSS-PROCESS LOCK IS BOUNDED ───────────────────────────────────

/// `reconcile_mirror_for` calls `ensure_consistent_with_db` while holding
/// `aberp_db`'s single writer mutex, and the reconciler blocks on a
/// CROSS-PROCESS `flock`. Untimed, that let any stuck peer — a hung `aberp` CLI,
/// a crashed-but-not-reaped process still owning the fd — freeze every DB write
/// in the serve process behind it, with no diagnostic at all.
///
/// The bound fails LOUD and never proceeds unsynchronised, so the R2 TOCTOU
/// stays closed. Takes ~10 s by construction (that IS the bound). The holder
/// keeps the lock for 25 s and is deliberately not joined: with an untimed
/// acquire the reconcile would park until the holder let go and then return
/// `Ok`, so the mutation fails this assertion rather than hanging the suite.
#[test]
fn a_stuck_peer_cannot_wedge_the_reconciler_forever() {
    let t = Tmp::new("lockbound");
    let db = t.db();
    {
        let h = handle(&db);
        beat(&h, "19:16:44");
    }
    let mirror = mirror_path_for(&db);

    let held = mirror.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&held)
            .unwrap();
        f.lock_exclusive().unwrap();
        tx.send(()).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(25));
        let _ = FileExt::unlock(&f);
    });
    rx.recv().expect("the peer took the mirror lock");

    let conn = duckdb::Connection::open(&db).unwrap();
    let started = std::time::Instant::now();
    let got = ensure_consistent_with_db(&conn, &mirror);
    let waited = started.elapsed();

    match got {
        Err(AppendError::MirrorLockTimeout { waited_ms, .. }) => {
            assert!(waited_ms >= 1_000, "the bound must be generous, not a poll");
        }
        other => panic!(
            "a peer holding the mirror lock must produce a bounded, loud MirrorLockTimeout \
             after ~{waited_ms} ms, got {other:?} after {waited:?}. Untimed, this call holds \
             aberp_db's writer mutex for as long as the peer lives.",
            waited_ms = waited.as_millis()
        ),
    }
    assert!(
        waited < std::time::Duration::from_secs(24),
        "the reconciler waited {waited:?} — that is the peer's lifetime, not a bound"
    );
}

// ── ROUND 4 — what the R3 adversarial found ─────────────────────────────────

/// R3 bounded the WRONG lock. `ensure_consistent_with_db` runs at boot and
/// before a snapshot; `sync_mirror` runs on EVERY COMMIT, from
/// `aberp_db`'s `WriteGuard::drop`, **while the single writer mutex is still
/// held**. Leaving that one untimed left the actual wedge fully intact: a stuck
/// peer still froze every DB write in the serve process for its whole lifetime,
/// with no diagnostic — verbatim the failure ADR §R3.5 claimed to have removed.
///
/// Measured before the fix: one ordinary `write()` + guard drop took 30.03 s
/// against a peer holding the mirror lock for 30 s.
///
/// The peer here holds for 20 s and is not joined, so restoring the untimed
/// `lock_exclusive()` fails this assertion rather than hanging the suite.
#[test]
fn a_stuck_peer_cannot_wedge_the_per_commit_write_path() {
    let t = Tmp::new("wedgecommit");
    let db = t.db();
    let h = handle(&db);
    beat(&h, "warm-up"); // establish the mirror before contending for it
    let mirror = mirror_path_for(&db);

    let held = mirror.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&held)
            .unwrap();
        f.lock_exclusive().unwrap();
        tx.send(()).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(20));
        let _ = FileExt::unlock(&f);
    });
    rx.recv().expect("the peer took the mirror lock");

    // One ordinary commit through the shared writer. The guard drop runs the
    // lockstep sync_mirror, which must NOT park on the peer indefinitely.
    let started = std::time::Instant::now();
    beat(&h, "under-contention");
    let waited = started.elapsed();

    assert!(
        waited < std::time::Duration::from_secs(10),
        "a commit took {waited:?} while an unrelated process held the mirror lock. That is \
         the peer's lifetime, not a bound — and it is taken with aberp_db's writer mutex \
         held, so EVERY DB write in this process is frozen behind a stuck peer."
    );
    // The commit itself still succeeded; only the mirror sync was skipped, which
    // leaves the mirror BEHIND (the safe direction — ADR-0110 D3).
    assert_eq!(db_rows(&db).len(), 2, "the committed write is not lost");
}

/// R3's prose claimed the head `entry_hash` "commits to its ENTIRE prefix", so
/// one row read proves the whole prefix agrees. A hash chain commits to its
/// HISTORY, not to its own continued existence in the table: deleting an
/// interior row rewrites nothing, head included. So an interior hole — the DB
/// losing a committed audit entry, which is the entire subject of this ADR —
/// read as agreement and reconciled to `Unchanged`.
#[test]
fn an_interior_row_the_db_lost_is_not_reported_as_agreement() {
    let t = Tmp::new("midhole");
    let db = t.db();
    let h = handle(&db);
    for at in ["a", "b", "c", "d", "e"] {
        beat(&h, at);
    }
    let mirror = mirror_path_for(&db);
    assert_eq!(mirror_seqs(&mirror), vec![1, 2, 3, 4, 5]);

    // A HOLE, not a lost tail: the head and every other row survive untouched,
    // so every surviving `entry_hash` still matches the mirror's.
    {
        let g = h.write().unwrap();
        g.execute_batch("DELETE FROM audit_ledger WHERE seq = 3;")
            .unwrap();
    }
    drop(h);
    assert_eq!(
        db_rows(&db).iter().map(|(s, _)| *s).collect::<Vec<_>>(),
        vec![1, 2, 4, 5],
        "the DB lost seq 3 and kept its head"
    );

    let conn = duckdb::Connection::open(&db).unwrap();
    match ensure_consistent_with_db(&conn, &mirror) {
        Err(AppendError::MirrorDivergedFromDb {
            first_divergent_seq,
            ..
        }) => assert_eq!(
            first_divergent_seq, 3,
            "the refusal must name the hole, not the head"
        ),
        other => panic!(
            "an interior row the DB lost was reported as {other:?}. The mirror holds a \
             committed entry the DB does not, and the reconciler called it healthy — a head \
             hash cannot see a hole behind it."
        ),
    }
}

/// Divergence is TERMINAL now, so the reconcile re-runs every boot (supervisor
/// restart loop) and every snapshot cycle (`reconcile_mirror_for` is
/// best-effort). Copying the mirror aside unconditionally on each attempt meant
/// one full copy per attempt, forever, on the same filesystem as the DB — the
/// audit mirror is usually the largest file in the tenant directory. Filling
/// that disk while refusing to boot turns a recoverable incident into an
/// unrecoverable one.
#[test]
fn repeated_refusals_do_not_grow_the_evidence_without_bound() {
    let t = Tmp::new("evidence");
    let (_db_before, mirror) = build_fork_shape(&t);
    let dir = mirror.parent().unwrap().to_path_buf();
    let baks = || {
        std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".diverged-"))
            .count()
    };

    let conn = duckdb::Connection::open(t.db()).unwrap();
    let mut seqs = Vec::new();
    for _ in 0..4 {
        match ensure_consistent_with_db(&conn, &mirror) {
            Err(AppendError::MirrorDivergedFromDb {
                first_divergent_seq,
                ..
            }) => seqs.push(first_divergent_seq),
            other => panic!("expected a repeated refusal, got {other:?}"),
        }
    }
    assert_eq!(seqs, vec![4, 4, 4, 4], "the verdict must not decay");
    assert_eq!(
        baks(),
        1,
        "four refusals produced {} copies of an UNCHANGED mirror. A terminal condition is \
         re-evaluated on every boot and every snapshot cycle, so an unconditional copy is \
         unbounded growth on the filesystem holding the DB.",
        baks()
    );
}

// ── ROUND 5 — what the round-4 re-adversarial found ─────────────────────────

/// Hold the mirror's `flock` from another thread for `secs`, returning once the
/// lock is definitely taken. Deliberately not joined: a test whose mutation
/// would *block* is a test that hangs the suite instead of failing it, so every
/// holder here releases on its own schedule and the assertions are on elapsed
/// time and outcome.
fn peer_holds_mirror_for(mirror: &Path, secs: u64) {
    let held = mirror.to_path_buf();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&held)
            .unwrap();
        f.lock_exclusive().unwrap();
        tx.send(()).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(secs));
        let _ = FileExt::unlock(&f);
    });
    rx.recv().expect("the peer took the mirror lock");
}

fn meta() -> LedgerMeta {
    LedgerMeta::new(tenant(), BinaryHash::from_bytes([7u8; 32]))
}

/// ROUND 5 — round 4 put the 2 s budget on `sync_mirror` ITSELF, which put it on
/// all fifteen `?`-propagating money-CLI callers as well as on the one
/// per-commit caller it was aimed at. Those fifteen (`mark_abandoned::run`,
/// `submit_annulment::run`, `submit_invoice`'s post-commit helper, …) call
/// `Ledger::sync_mirror(&mirror)?` as the TAIL of a command whose DB commit —
/// and, on the submission paths, whose NAV submission — has ALREADY LANDED.
/// A budget there converts a lock wait into a post-commit HARD ERROR: the
/// command reports failure for work that is durable and, in NAV's ledger, done.
/// The re-adversarial measured it against a 5 s peer: round 4 returned
/// `Err(MirrorLockTimeout { waited_ms: 2000 })` at 2.05 s where the pre-round-4
/// tree returned `Ok(1)` at 4.99 s.
///
/// So the money path WAITS. Waiting is its benign outcome — it holds no writer
/// mutex (the guard is dropped or consumed before the sync) and it has a real
/// answer to give once the peer lets go. The bound belongs on the per-commit
/// caller, which has the opposite profile; `the_bounded_budget_is_fatal_only_
/// where_a_timeout_is_safe` pins that half.
///
/// Mutation that kills this: put the budget back on `sync_mirror`
/// (`lock_exclusive_bounded(&file, mirror_path, SYNC_MIRROR_LOCK_TIMEOUT)?`
/// instead of the blocking `lock_exclusive()`). The call then returns
/// `Err(MirrorLockTimeout)` at ~2 s and this test says exactly what that means.
#[test]
fn a_committed_money_command_does_not_fail_because_a_peer_held_the_mirror() {
    let t = Tmp::new("moneywait");
    let db = t.db();
    let mirror = mirror_path_for(&db);

    {
        let h = handle(&db);
        beat(&h, "already-synced");
        // The peer takes the lock BEFORE the second commit, so that commit's
        // lockstep sync is skipped and the mirror is genuinely BEHIND — the
        // state the money path's explicit sync exists to repair.
        // 8 s, not 4: the second commit's own lockstep sync burns the first
        // 2 s of that, so the peer must still be holding well past the point
        // where a 2 s budget on THIS path would have fired.
        peer_holds_mirror_for(&mirror, 8);
        beat(&h, "the-commit-that-landed");
    }
    assert_eq!(
        mirror_seqs(&mirror),
        vec![1],
        "the lockstep sync was skipped, so the mirror is behind at seq 1"
    );
    assert_eq!(db_rows(&db).len(), 2, "both commits are durable in the DB");

    // The money-CLI tail, verbatim in shape: the command has committed, the
    // guard is gone, and this is the last thing it does before reporting
    // success. `Ledger::sync_mirror` is a one-line delegate to this fn.
    let conn = duckdb::Connection::open(&db).unwrap();
    let started = std::time::Instant::now();
    let got = sync_mirror(&conn, &meta(), &mirror);
    let waited = started.elapsed();

    match got {
        Ok(head) => assert_eq!(head, 2, "the mirror caught up to the DB head after waiting"),
        Err(e) => panic!(
            "a money command whose DB commit ALREADY LANDED reported {e:?} after {waited:?}, \
             because an unrelated process was holding the mirror lock. The commit is durable \
             and (on the submission paths) NAV has already accepted it; the operator is told \
             it failed. A lock wait on this path must not become a post-commit hard error."
        ),
    }
    assert!(
        waited >= std::time::Duration::from_secs(3),
        "the call returned in {waited:?} — that is inside the 2 s lockstep budget, so it never \
         outwaited a peer and this test proves nothing about waiting"
    );
    assert_eq!(
        mirror_seqs(&mirror),
        vec![1, 2],
        "the mirror really caught up"
    );
}

/// ROUND 5, the other half — the bound is not removed, it is MOVED to the taker
/// for which a timeout is both fatal and safe.
///
/// One stuck peer, two takers, two deliberately different outcomes:
///
/// * The LOCKSTEP sync (`WriteGuard::drop`, writer mutex held, transaction
///   already committed) gives up after 2 s and REPORTS it. It cannot fail its
///   caller, so it does not: the mirror is left behind and the commit stands.
///   This is the round-4 win, preserved — a stuck peer cannot freeze a
///   committing writer for the peer's whole lifetime.
/// * The RECONCILER (`ensure_consistent_with_db`, boot / pre-snapshot) waits
///   longer and then fails LOUD and FATAL at its own 10 s budget. It has
///   committed nothing, so refusing loses nothing, and refusing is what keeps
///   R2's TOCTOU closed.
///
/// Takes ~12 s by construction: 2 s + 10 s is the pair of budgets being pinned.
///
/// Mutations that kill this: route the reconciler through the benign
/// `try_lock_exclusive_within` (the fatal refusal disappears), or route the
/// lockstep through `lock_exclusive_bounded` (the benign skip becomes an `Err`).
#[test]
fn the_bounded_budget_is_fatal_only_where_a_timeout_is_safe() {
    let t = Tmp::new("budgetsplit");
    let db = t.db();
    {
        let h = handle(&db);
        beat(&h, "20:31:09");
    }
    let mirror = mirror_path_for(&db);
    let conn = duckdb::Connection::open(&db).unwrap();

    peer_holds_mirror_for(&mirror, 14);

    let started = std::time::Instant::now();
    let lockstep = sync_mirror_lockstep(&conn, &meta(), &mirror);
    let lockstep_waited = started.elapsed();
    match lockstep {
        Ok(LockstepSync::SkippedLockContended { waited_ms }) => assert_eq!(
            waited_ms, 2_000,
            "the lockstep budget must be the 2 s one, not the reconciler's"
        ),
        other => panic!(
            "the per-commit sync returned {other:?} after {lockstep_waited:?}. It runs with \
             aberp_db's writer mutex held and with its transaction already committed, so a \
             contended lock must be a bounded, BENIGN report — not an Err its caller cannot \
             act on, and not an unbounded park."
        ),
    }
    assert!(
        lockstep_waited < std::time::Duration::from_secs(6),
        "the lockstep sync waited {lockstep_waited:?} — that is not a 2 s bound, and every \
         commit in the process is queued behind it"
    );

    let started = std::time::Instant::now();
    let reconcile = ensure_consistent_with_db(&conn, &mirror);
    let reconcile_waited = started.elapsed();
    match reconcile {
        Err(AppendError::MirrorLockTimeout { waited_ms, .. }) => assert_eq!(
            waited_ms, 10_000,
            "the reconciler must keep its own, generous, FATAL budget"
        ),
        other => panic!(
            "the booting reconciler returned {other:?} after {reconcile_waited:?}. Moving the \
             money path off the bounded-fatal helper must not take the reconciler with it: a \
             boot that cannot take the mirror lock has to refuse, loudly, rather than proceed \
             unsynchronised."
        ),
    }
    assert!(
        reconcile_waited < std::time::Duration::from_secs(13),
        "the reconciler waited {reconcile_waited:?} — that is the peer's lifetime, not a bound"
    );
}

/// ROUND 5 — `COUNT(*)` is not the cardinality half of the prefix proof.
///
/// Round 4 closed the interior-hole hole by requiring the DB to hold
/// `head.seq` ROWS over `[1..=head.seq]`. But `audit_ledger` carries no
/// `UNIQUE(seq)` — S341 dropped that ART index over duckdb#23046 — so a
/// duplicate seq is representable, and one duplicate OFFSETS one hole exactly.
/// `db = [1, 2, 2, 4, 5]` against `mirror = [1..=5]` has a matching head hash
/// and `COUNT(*) = 5`, so round 4's proof passed it: `Ok(Unchanged)` on a DB
/// that had lost its committed audit entry at seq 3. Same class as the bug
/// round 4 fixed, one layer down.
///
/// `COUNT(DISTINCT seq)` is what actually says "each of `[1..=head.seq]`,
/// exactly once".
///
/// Mutation that kills this: drop the `COUNT(DISTINCT seq)` half of
/// `read_db_seq_counts_up_to`'s comparison. The pair reconciles `Unchanged` and
/// the lost row is reported as healthy.
#[test]
fn a_duplicate_seq_cannot_offset_a_hole_into_agreement() {
    let t = Tmp::new("dupoffset");
    let db = t.db();
    let h = handle(&db);
    for at in ["a", "b", "c", "d", "e"] {
        beat(&h, at);
    }
    let mirror = mirror_path_for(&db);
    assert_eq!(mirror_seqs(&mirror), vec![1, 2, 3, 4, 5]);

    // The DB loses its committed row at seq 3, and an exact duplicate of seq 2
    // takes its place in the count. The head and every surviving entry_hash are
    // untouched, so the hash half of the proof still passes.
    {
        let g = h.write().unwrap();
        g.execute_batch(
            "INSERT INTO audit_ledger SELECT * FROM audit_ledger WHERE seq = 2; \
             DELETE FROM audit_ledger WHERE seq = 3;",
        )
        .unwrap();
    }
    drop(h);

    let conn = duckdb::Connection::open(&db).unwrap();
    let rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM audit_ledger WHERE seq <= 5",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        rows, 5,
        "the shape only matters because COUNT(*) still reads as 5 — that is why round 4's \
         proof passed a DB that had lost a committed row"
    );
    assert_eq!(
        db_rows(&db).iter().map(|(s, _)| *s).collect::<Vec<_>>(),
        vec![1, 2, 2, 4, 5],
        "one duplicate, one hole"
    );

    match ensure_consistent_with_db(&conn, &mirror) {
        Err(AppendError::MirrorDivergedFromDb {
            first_divergent_seq,
            ..
        }) => assert_eq!(
            first_divergent_seq, 3,
            "the refusal must name the hole, not the duplicate"
        ),
        other => panic!(
            "a DB that lost seq 3 while carrying a duplicate of seq 2 was reported as \
             {other:?}. Without a UNIQUE(seq), a duplicate offsets a hole one-for-one and \
             COUNT(*) alone cannot tell the difference — the mirror holds a committed entry \
             the DB does not, and the reconciler called the pair healthy."
        ),
    }
}

/// ROUND 6 — and `COUNT(DISTINCT seq)` is not the WHOLE cardinality half either.
///
/// Its sibling above pins one direction of the pair: a duplicate that offsets a
/// hole, which only `COUNT(DISTINCT seq)` can see. This pins the OTHER: a
/// duplicate with NO hole behind it — `db = [1, 2, 2, 3, 4, 5]` against
/// `mirror = [1..=5]`. The head hash matches, every seq in `[1..=5]` is present,
/// and `COUNT(DISTINCT seq)` is exactly 5. Only `COUNT(*)` (6) dissents.
///
/// That shape is not hypothetical bookkeeping. `audit_ledger` carries no
/// `UNIQUE(seq)` (S341 dropped that ART index over duckdb#23046 / S332), so a
/// second writer that samples a stale head and re-assigns a sequence already
/// committed by the first lands EXACTLY here: two rows at one seq, no gap. It is
/// the two-writer seq fork this whole ADR exists for, with the S186 signature.
/// A reconciler that reports `Unchanged` on it certifies a forked ledger as
/// healthy — and the divergence is durable in the DB while the mirror shows only
/// one of the two entries.
///
/// Mutation that kills this (MUT-F): drop the `COUNT(*)` half of
/// `read_db_seq_counts_up_to`'s comparison, keeping `COUNT(DISTINCT seq)`. All
/// 11 other tests on this branch stay green and the pair reconciles `Unchanged`.
/// The two aggregates are load-bearing in BOTH directions; neither alone is the
/// proof.
#[test]
fn a_duplicate_seq_with_no_hole_behind_it_is_still_a_fork() {
    let t = Tmp::new("dupnohole");
    let db = t.db();
    let h = handle(&db);
    for at in ["a", "b", "c", "d", "e"] {
        beat(&h, at);
    }
    let mirror = mirror_path_for(&db);
    assert_eq!(mirror_seqs(&mirror), vec![1, 2, 3, 4, 5]);

    // A second writer re-assigns a seq the first already committed: an exact
    // duplicate row at seq 2, and NOTHING deleted. Every entry_hash — head
    // included — is untouched, and no seq in [1..=5] is missing.
    {
        let g = h.write().unwrap();
        g.execute_batch("INSERT INTO audit_ledger SELECT * FROM audit_ledger WHERE seq = 2;")
            .unwrap();
    }
    drop(h);

    let conn = duckdb::Connection::open(&db).unwrap();
    let (rows, distinct): (i64, i64) = conn
        .query_row(
            "SELECT COUNT(*), COUNT(DISTINCT seq) FROM audit_ledger WHERE seq <= 5",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        (rows, distinct),
        (6, 5),
        "the shape only matters because COUNT(DISTINCT seq) still reads as 5 — that is why \
         dropping the COUNT(*) half passes this fork"
    );
    assert_eq!(
        db_rows(&db).iter().map(|(s, _)| *s).collect::<Vec<_>>(),
        vec![1, 2, 2, 3, 4, 5],
        "one duplicate, no hole"
    );

    match ensure_consistent_with_db(&conn, &mirror) {
        Err(AppendError::MirrorDivergedFromDb {
            first_divergent_seq,
            ..
        }) => assert_eq!(
            first_divergent_seq, 5,
            "with no hole, every mirror entry finds a hash-matching DB row, so the earliest \
             disagreement the locator can name is the head itself — the refusal is what \
             matters, and the operator still gets a seq to start from"
        ),
        other => panic!(
            "a DB carrying two committed rows at seq 2 was reported as {other:?}. Without a \
             UNIQUE(seq) that is a two-writer sequence fork sitting durably in the ledger, and \
             COUNT(DISTINCT seq) alone cannot see it — it counts 5 distinct seqs over a \
             6-row table and calls the pair healthy."
        ),
    }
}
