//! ADR-0099 R2 — the fifth prod recurrence (seq 2508) was a LOST DB COMMIT that
//! the reconciler then MASKED. This file pins both halves.
//!
//! # What actually happened
//!
//! Four consecutive 60s intake-poll heartbeats. Two (19:19:44, 19:20:44) were
//! durable in the `<db>.audit.log` MIRROR and ABSENT from the DB; the next two
//! (19:21:44, 19:22:44) were in the DB at the SAME seqs. That is not two
//! writers racing — a concurrent-writer duplicate puts the SAME entry (same
//! `entry_hash`, same timestamp) twice in ONE store. It is the DB losing two
//! already-committed appends: the chain head fell back, so the later pair
//! legitimately took the freed seqs.
//!
//! # Why it was lost (and why it cannot be lost again on this tree)
//!
//! Pre-ADR-0110-D3, `WriteGuard::drop` `fsync`ed the MIRROR (`sync_mirror` ends
//! in `sync_all`) and never flushed the DB at all — the durability ordering
//! exactly inverted. Every Defense release up to and including v0.3.0 ships that
//! drop; D3's unconditional `fsync_data_paths()` first appears in v0.4.0.
//! [`daemon_heartbeats_are_power_loss_durable`] is the guard that keeps it that
//! way for the DAEMON path specifically — the existing D3 power-loss spec drives
//! a MONEY path, so a change that made the flush conditional on money paths
//! would leave that spec green while heartbeats silently lost durability again.
//!
//! # Why nobody saw it
//!
//! The reconciler's `Extended` branch appended DB rows after `mirror_max_seq`
//! without ever comparing the shared prefix. That erased BOTH signals: the
//! length asymmetry `MirrorAheadOfDb` keys on, and the head-hash equality the
//! equal-length branch keys on. What surfaced instead was the mirror's own
//! `prev_hash` link check failing one seq later — reported as "corrupt mirror",
//! pointing the operator at the wrong subsystem entirely.
//!
//! Real DuckDB, so Mac/CI only.

use std::path::{Path, PathBuf};

use aberp_audit_ledger::{
    append_in_tx, ensure_consistent_with_db, ensure_schema, mirror_path_for, read_mirror_entries,
    Actor, AppendError, BinaryHash, EventKind, LedgerMeta, RecoveryAction, TenantId,
};
use aberp_db::{Handle, HandleConfig};

const TENANT: &str = "defense";

struct Tmp(PathBuf);
impl Tmp {
    fn new(label: &str) -> Self {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p =
            std::env::temp_dir().join(format!("aberp-r2lost-{label}-{}-{n}", std::process::id()));
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

fn handle(db: &Path, checkpoint: bool) -> std::sync::Arc<Handle> {
    Handle::open(
        db,
        tenant(),
        HandleConfig {
            checkpoint_enabled: checkpoint,
            ..Default::default()
        },
    )
    .unwrap()
}

/// One intake-poll heartbeat through the shared writer, exactly as the daemon
/// emits it. The `WriteGuard` drop runs the D3 flush and then the lockstep
/// mirror sync.
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
/// which is the whole point: it was `fsync`ed and the DB was not.
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

fn db_seqs(h: &Handle) -> Vec<u64> {
    let conn = h.read().unwrap();
    let mut st = conn
        .prepare("SELECT seq FROM audit_ledger ORDER BY seq")
        .unwrap();
    let rows = st.query_map([], |r| r.get::<_, i64>(0)).unwrap();
    rows.map(|r| r.unwrap() as u64).collect()
}

// ── PREVENTION ──────────────────────────────────────────────────────────────

/// A committed DAEMON audit append must survive a power loss, not just a
/// process kill — the same promise `durable_ack` makes on the money path.
///
/// Modelled the way ADR-0110's D3 spec models it: drive the real write path,
/// then copy ONLY the files the write path certified durable (its `fsynced_paths`
/// journal, plus the two constants Defense made durable before D3 — the mirror
/// and the main DB file) into a fresh directory and boot that. Everything else
/// is simply absent, which is what an un-`fsync`ed page-cache page is after the
/// power goes.
///
/// **The derivation is the mutation test.** Restore the pre-D3 drop (no
/// `fsync_data_paths`) and the WAL falls out of the journal, out of the copy,
/// and this goes RED with the exact incident shape: mirror 6, DB 1.
#[test]
fn daemon_heartbeats_are_power_loss_durable() {
    let t = Tmp::new("durable");
    let live = t.0.join("live");
    std::fs::create_dir_all(&live).unwrap();
    let db = live.join("aberp.duckdb");

    // Production posture: checkpointing ON, as `Handle::open_default`.
    let h = handle(&db, true);
    for i in 0..6 {
        beat(&h, &format!("19:1{i}:44"));
    }
    let mirror_before = mirror_seqs(&mirror_path_for(&db)).len();
    assert_eq!(mirror_before, 6, "all six heartbeats reached the mirror");

    let mut durable = vec![
        "aberp.duckdb".to_string(),
        "aberp.duckdb.audit.log".to_string(),
    ];
    for p in h.fsynced_paths() {
        let n = p.file_name().unwrap().to_string_lossy().into_owned();
        if !durable.contains(&n) {
            durable.push(n);
        }
    }
    assert!(
        durable.iter().any(|n| n.ends_with(".wal")),
        "the WAL must be in the durability journal — without it a committed \
         heartbeat lives only in the page cache while its MIRROR line is already \
         fsynced, which is the seq-2508 inversion. Journal: {durable:?}"
    );

    // The power loss.
    let dead = t.0.join("after_power_loss");
    std::fs::create_dir_all(&dead).unwrap();
    for e in std::fs::read_dir(&live).unwrap() {
        let e = e.unwrap();
        let p = e.path();
        if !p.is_file() {
            continue;
        }
        let n = e.file_name().to_string_lossy().into_owned();
        if durable.contains(&n) {
            std::fs::copy(&p, dead.join(&n)).unwrap();
        }
    }
    drop(h);

    let db2 = dead.join("aberp.duckdb");
    let h2 = handle(&db2, true);
    let survived = db_seqs(&h2).len();
    let mirrored = mirror_seqs(&mirror_path_for(&db2)).len();
    assert_eq!(
        survived, mirrored,
        "MIRROR AHEAD OF DB AFTER POWER LOSS: the mirror kept {mirrored} heartbeats and \
         the DB kept {survived}. A committed daemon audit append is not durable, which \
         is exactly how seq 2508 was produced."
    );
    assert_eq!(survived, 6, "every committed heartbeat must survive");
}

// ── DETECTION ───────────────────────────────────────────────────────────────

/// STATE A — the moment after the loss: the mirror is genuinely AHEAD.
/// `MirrorAheadOfDb` must fire, because recovery for this case REPLAYS the
/// mirror-only entries back into the DB.
#[test]
fn state_a_mirror_ahead_is_reported_as_ahead_not_as_divergence() {
    let t = Tmp::new("ahead");
    let h = handle(&t.db(), false);
    for at in ["19:16:44", "19:17:44", "19:18:44", "19:19:44", "19:20:44"] {
        beat(&h, at);
    }
    let mirror = mirror_path_for(&t.db());
    assert_eq!(mirror_seqs(&mirror), vec![1, 2, 3, 4, 5]);

    lose_db_commits_from(&h, 4);

    match ensure_consistent_with_db(&h.read().unwrap(), &mirror) {
        Err(AppendError::MirrorAheadOfDb {
            mirror_max_seq,
            db_max_seq,
            preserved,
        }) => {
            assert_eq!((mirror_max_seq, db_max_seq), (5, 3));
            assert!(
                Path::new(&preserved).exists(),
                "the ahead mirror is preserved"
            );
        }
        other => panic!(
            "a mirror that is ahead by two entries must be reported as AHEAD (recovery \
             replays those entries into the DB), got {other:?}"
        ),
    }
}

/// STATE B — the DB has re-used the freed seqs. Counts are equal and only the
/// CONTENT disagrees. This must be `MirrorDivergedFromDb` naming the seq, not a
/// generic "corrupt mirror".
#[test]
fn state_b_reused_seqs_are_reported_as_divergence_naming_the_seq() {
    let t = Tmp::new("reused");
    let h = handle(&t.db(), false);
    for at in ["19:16:44", "19:17:44", "19:18:44", "19:19:44", "19:20:44"] {
        beat(&h, at);
    }
    let mirror = mirror_path_for(&t.db());
    lose_db_commits_from(&h, 4);
    for at in ["19:21:44", "19:22:44"] {
        beat(&h, at); // the DB legitimately re-uses seq 4 and 5
    }
    assert_eq!(mirror_seqs(&mirror), vec![1, 2, 3, 4, 5]);
    assert_eq!(
        db_seqs(&h),
        vec![1, 2, 3, 4, 5],
        "equal counts, equal max seq"
    );

    match ensure_consistent_with_db(&h.read().unwrap(), &mirror) {
        Err(AppendError::MirrorDivergedFromDb {
            first_divergent_seq,
            mirror_max_seq,
            db_max_seq,
            preserved,
        }) => {
            assert_eq!(
                first_divergent_seq, 4,
                "the refusal must name the EARLIEST divergent seq — that is the operator's \
                 entry point into the incident"
            );
            assert_eq!((mirror_max_seq, db_max_seq), (5, 5));
            assert!(Path::new(&preserved).exists());
        }
        other => panic!("expected MirrorDivergedFromDb, got {other:?}"),
    }
}

/// STATE C — the one that actually hid the incident. The DB has moved PAST the
/// mirror, so the reconciler sees "mirror is behind" and takes `Extended`.
///
/// Pre-R2 that branch appended DB rows onto the divergent mirror without ever
/// comparing the prefix — which is what made the counts equal, destroyed both
/// detection signals, and relabelled the whole thing "corrupt mirror". It must
/// now refuse, name the seq, and leave the mirror byte-identical.
#[test]
fn state_c_extended_refuses_instead_of_grafting_onto_a_divergent_mirror() {
    let t = Tmp::new("extended");
    let h = handle(&t.db(), false);
    for at in ["19:16:44", "19:17:44", "19:18:44", "19:19:44", "19:20:44"] {
        beat(&h, at);
    }
    let mirror = mirror_path_for(&t.db());
    lose_db_commits_from(&h, 4);
    for at in ["19:21:44", "19:22:44", "19:23:44", "19:24:44"] {
        beat(&h, at); // re-uses 4,5 then advances to 6,7
    }
    assert_eq!(mirror_seqs(&mirror), vec![1, 2, 3, 4, 5]);
    assert_eq!(
        db_seqs(&h),
        vec![1, 2, 3, 4, 5, 6, 7],
        "the DB is now AHEAD on count"
    );
    let before = std::fs::read(&mirror).unwrap();

    match ensure_consistent_with_db(&h.read().unwrap(), &mirror) {
        Err(AppendError::MirrorDivergedFromDb {
            first_divergent_seq,
            mirror_max_seq,
            db_max_seq,
            preserved,
        }) => {
            assert_eq!(first_divergent_seq, 4);
            assert_eq!((mirror_max_seq, db_max_seq), (5, 7));
            assert!(Path::new(&preserved).exists());
        }
        other => panic!(
            "the Extended branch grafted DB rows onto a mirror it never compared, got \
             {other:?}. That is the masking step: it makes the counts equal, so every \
             later reconcile reports Unchanged and the lost commits become invisible."
        ),
    }
    assert_eq!(
        std::fs::read(&mirror).unwrap(),
        before,
        "a refusal must leave the live mirror byte-identical — it holds the ONLY copy \
         of the entries the DB lost"
    );
}

/// The masking itself, stated as a property: after the refusal, a SECOND
/// reconcile must reach the same verdict. Pre-R2 the second call returned a
/// different (and wrong) answer because the first had already mutated the
/// mirror — that non-idempotence IS the bug.
#[test]
fn the_refusal_is_idempotent_so_the_evidence_cannot_decay() {
    let t = Tmp::new("idem");
    let h = handle(&t.db(), false);
    for at in ["a", "b", "c", "d", "e"] {
        beat(&h, at);
    }
    let mirror = mirror_path_for(&t.db());
    lose_db_commits_from(&h, 4);
    for at in ["f", "g", "h"] {
        beat(&h, at);
    }
    let first = ensure_consistent_with_db(&h.read().unwrap(), &mirror);
    let second = ensure_consistent_with_db(&h.read().unwrap(), &mirror);
    match (&first, &second) {
        (
            Err(AppendError::MirrorDivergedFromDb {
                first_divergent_seq: a,
                ..
            }),
            Err(AppendError::MirrorDivergedFromDb {
                first_divergent_seq: b,
                ..
            }),
        ) => assert_eq!(a, b, "both reconciles must name the same seq"),
        _ => panic!("expected two identical divergence refusals, got {first:?} then {second:?}"),
    }
}

/// The happy path must stay untouched and O(1): a mirror that agrees with the
/// DB and is merely behind still Extends, and an equal mirror is Unchanged.
#[test]
fn an_agreeing_mirror_still_extends_and_settles() {
    let t = Tmp::new("happy");
    let h = handle(&t.db(), false);
    for at in ["a", "b", "c"] {
        beat(&h, at);
    }
    let mirror = mirror_path_for(&t.db());
    // Trim the mirror to 1 entry: behind, but in agreement.
    let all = std::fs::read(&mirror).unwrap();
    let end = all.iter().position(|&b| b == b'\n').unwrap() + 1;
    std::fs::write(&mirror, &all[..end]).unwrap();
    assert_eq!(mirror_seqs(&mirror), vec![1]);

    assert_eq!(
        ensure_consistent_with_db(&h.read().unwrap(), &mirror).unwrap(),
        RecoveryAction::Extended { entries_added: 2 },
        "an agreeing-but-behind mirror must still be extended — the prefix proof is one \
         hash comparison, not a reason to refuse"
    );
    assert_eq!(mirror_seqs(&mirror), vec![1, 2, 3]);
    assert_eq!(
        ensure_consistent_with_db(&h.read().unwrap(), &mirror).unwrap(),
        RecoveryAction::Unchanged
    );
}
