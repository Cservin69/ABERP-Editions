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
    Actor, AppendError, BinaryHash, EventKind, LedgerMeta, TenantId,
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
