//! ADR-0099 regression — the recurring audit-ledger seq fork (seq 369→416→428
//! →515), reproduced at the DuckDB level and shown to be closed by routing all
//! writes through the ONE shared [`aberp_db::Handle`].
//!
//! # The fork the forensic pinned
//!
//! The seq-515 fork was the periodic **snapshot daemon** (`snapshot.created`,
//! `apps/aberp/src/snapshot.rs::open_ledger`) and the **quote-intake daemon**
//! each opening an **INDEPENDENT** `Ledger` on the live DB and, reading the same
//! chain head, both self-assigning the next seq off it — inside the ONE `aberp
//! serve` process. It was NOT the narrow "opener + rogue `sync_mirror` in one
//! fn" class CHECK 10L froze (neither side ran a rogue `sync_mirror`; the
//! snapshot side appends through `Ledger`, whose mirror write is the sanctioned
//! WriteGuard drop). The TRUE fork primitive is simpler: **two independent
//! openers + an append, off the same head**. This test pins both halves.
//!
//! Real DuckDB → runs on the Mac/CI gate under `cargo test -p aberp-db` (the
//! bundled libduckdb amalgamation cannot build in the saw-off sandbox), same as
//! `handle_concurrency_e2e.rs`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aberp_audit_ledger::{
    append_in_tx, ensure_schema, mirror_path_for, read_mirror_entries, recent_entries, Actor,
    BinaryHash, EventKind, LedgerMeta, TenantId,
};
use aberp_db::{Handle, HandleConfig};
use duckdb::Connection;

const TENANT: &str = "defense";

struct Tmp(PathBuf);
impl Tmp {
    fn new(label: &str) -> Self {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p =
            std::env::temp_dir().join(format!("aberp-adr0099-{label}-{}-{n}", std::process::id()));
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

/// Append one audit row against a borrowed tx — the shape a daemon "created"
/// event takes (`snapshot.created` / a quote-intake row).
fn append_in(tx: &duckdb::Transaction<'_>, label: &str) {
    append_in_tx(
        tx,
        &meta(),
        EventKind::DbAutoRecovered,
        format!("{{\"daemon\":\"{label}\"}}").into_bytes(),
        Actor::from_local_cli(format!("ulid-{label}"), "tester"),
        None,
    )
    .unwrap();
}

/// Seed a valid empty tenant DB, then one committed row so the chain head is
/// seq 1 (both daemons will race to append seq 2 off it).
fn seed_with_head(db: &Path) {
    let mut conn = Connection::open(db).unwrap();
    ensure_schema(&conn).unwrap();
    let tx = conn.transaction().unwrap();
    append_in(&tx, "seed");
    tx.commit().unwrap();
    conn.execute_batch("CHECKPOINT;").unwrap();
}

/// Every seq currently in the on-disk ledger, via a fresh read.
fn seqs(db: &Path) -> Vec<u64> {
    let conn = Connection::open(db).unwrap();
    let mut s: Vec<u64> = recent_entries(&conn, 10_000)
        .unwrap()
        .iter()
        .map(|e| e.seq.as_u64())
        .collect();
    s.sort_unstable();
    s
}
fn has_duplicate(seqs: &[u64]) -> bool {
    seqs.windows(2).any(|w| w[0] == w[1])
}

/// **RED half — the fork primitive.** Two INDEPENDENT `Connection::open`
/// instances (the snapshot daemon + the quote-intake daemon), each reading the
/// chain head inside its own transaction, both compute the SAME next seq off
/// head 1 and both append it. With the `UNIQUE(seq)` ART gone (duckdb#23046),
/// nothing at the storage layer stops it: two rows land on seq 2 — the fork.
/// This is exactly what one independent opener racing another did in prod; the
/// only fix is to stop opening independent instances (the GREEN half).
#[test]
fn independent_openers_off_the_same_head_fork_the_seq() {
    let tmp = Tmp::new("red");
    let db = tmp.db();
    seed_with_head(&db);
    assert_eq!(seqs(&db), vec![1], "seeded head is seq 1");

    // Two independent openers — the snapshot daemon and the quote-intake daemon.
    let mut snap = Connection::open(&db).unwrap();
    let mut quote = Connection::open(&db).unwrap();

    // Each opens its tx and reads the head (seq 1) BEFORE either commits — the
    // stale-head interleave the shared buffer cache would have prevented.
    let tx_snap = snap.transaction().unwrap();
    let tx_quote = quote.transaction().unwrap();
    append_in(&tx_snap, "snapshot.created"); // computes seq = head(1) + 1 = 2
    append_in(&tx_quote, "quote-intake"); // ALSO reads head 1 -> seq = 2
    tx_snap.commit().unwrap();
    tx_quote.commit().unwrap();

    let s = seqs(&db);
    assert!(
        has_duplicate(&s),
        "two independent openers off the same head MUST fork the seq (the seq-515 \
         primitive ADR-0099 removes) — expected a duplicate seq, got {s:?}"
    );
}

/// **GREEN half — the fix.** Both daemons route their appends through the ONE
/// shared [`Handle`]'s serialized writer. Each `write()` re-reads the head on
/// the SAME coherent instance under the writer mutex, so a burst of interleaved
/// daemon ticks produces a DENSE, fork-free chain, and the WriteGuard drop keeps
/// the mirror in lockstep with the DB.
#[test]
fn all_writes_through_one_handle_never_fork_and_keep_the_mirror_in_step() {
    let tmp = Tmp::new("green");
    let db = tmp.db();
    seed_with_head(&db);

    // Isolate the single-instance / single-writer property from the debounced
    // checkpoint (same posture as handle_concurrency_e2e).
    let cfg = HandleConfig {
        checkpoint_enabled: false,
        ..Default::default()
    };
    let handle: Arc<Handle> = Handle::open(&db, tenant(), cfg).unwrap();

    // Interleave a burst of snapshot-daemon and quote-intake ticks, ALL through
    // the shared writer (what the ADR-0099 migration makes both daemons do).
    let ticks = 50usize;
    for i in 0..ticks {
        {
            let mut g = handle.write().unwrap();
            let tx = g.transaction().unwrap();
            append_in(&tx, &format!("snapshot.created-{i}"));
            tx.commit().unwrap();
        }
        {
            let mut g = handle.write().unwrap();
            let tx = g.transaction().unwrap();
            append_in(&tx, &format!("quote-intake-{i}"));
            tx.commit().unwrap();
        }
    }
    drop(handle); // final WriteGuard already dropped per write; flush any state.

    // 1 seed + 2*ticks appends, DENSE and fork-free.
    let s = seqs(&db);
    let expected: Vec<u64> = (1..=(1 + 2 * ticks as u64)).collect();
    assert!(
        !has_duplicate(&s),
        "shared-Handle chain must have no forked seq: {s:?}"
    );
    assert_eq!(
        s, expected,
        "shared-Handle chain must be dense + monotonic (no gap, no dup)"
    );

    // DB == mirror: the WriteGuard drop's lockstep sync_mirror tracked every row.
    let db_conn = Connection::open(&db).unwrap();
    let db_count = recent_entries(&db_conn, 10_000).unwrap().len();
    let mirror = mirror_path_for(&db);
    let mirror_count = read_mirror_entries(&mirror).unwrap().len();
    assert_eq!(
        db_count, mirror_count,
        "the lockstep mirror must hold exactly the DB's rows (DB={db_count}, mirror={mirror_count})"
    );
}
