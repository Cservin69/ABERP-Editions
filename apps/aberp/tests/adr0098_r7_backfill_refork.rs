//! ADR-0098 R7 — regression guard for the boot buyer-backfill RE-FORK
//! (`system.restore_buyer_backfill_cycle_completed`, the 415/416 divergence).
//!
//! ## What forked, and why the earlier gates missed it
//!
//! `restore_from_nav_outgoing::append_backfill_cycle_entry` used to write the
//! cycle-completion audit entry through a RAW `duckdb::Connection::open` +
//! a second `Ledger::open`/`sync_mirror` on the tenant DB **path** — and it was
//! spawned at boot (`serve.rs`) with `db_path`, NOT the shared
//! `aberp_db::Handle`. A separate `Connection::open` is an independent DuckDB
//! `Database` instance with no shared buffer cache (documented at
//! `incoming_invoices.rs:54-74`): it does not observe rows the live Handle's
//! instance committed, so on its append it read a **stale** chain head,
//! re-assigned an already-taken `seq` (the seq-415 collision), and then rewrote
//! the audit mirror **from its own stale view** — the exact DB↔mirror divergence
//! the ADR-0098 R1 boot guard refuses on. It even carried
//! `PRAGMA disable_checkpoint_on_shutdown` — but that fence only stops an
//! implicit close-checkpoint; it does NOT stop the stale-head `seq` collision or
//! the rogue `sync_mirror`. It was FENCED, not migrated.
//!
//! ## This test
//!
//!   * **Arm A (the RED reproduction)** replays the OLD shape: a separate
//!     `Connection::open` appends the `RestoreBuyerBackfillCycleCompleted`
//!     entry while a persistent instance already holds a newer head. It forks —
//!     colliding `seq`, a lost row — reproducing the 415/416 divergence. This is
//!     the behaviour `append_backfill_cycle_entry` had BEFORE ADR-0098 R7.
//!   * **Arm B (the GREEN pin)** routes the SAME cycle-audit append through the
//!     ONE shared `aberp_db::Handle` writer (the R7 fix), interleaved with a
//!     boot burst of other Handle writes. It stays coherent: dense + monotonic
//!     `seq`, no duplicate `seq`, `verify_chain` clean, and DB head == mirror
//!     head. The `WriteGuard` drop is the sole post-commit `sync_mirror`.
//!
//! CHECK 10L in `tools/cut_gate_db_isolation.sh` statically guarantees the
//! shipped `append_backfill_cycle_entry` takes Arm B's shape (no independent
//! opener, no rogue `sync_mirror`, routed through `.write()`); this test pins
//! that Arm B's shape is coherent and Arm A's shape is the fork it replaced.
//! Edition-agnostic (aberp-db + aberp-audit-ledger are edition-independent).

use aberp_audit_ledger::{
    append_in_tx, ensure_schema, mirror_path_for, read_mirror_entries, recent_entries, Actor,
    BinaryHash, EventKind, Ledger, LedgerMeta, TenantId,
};
use aberp_db::{Handle, HandleConfig};
use duckdb::Connection;
use ulid::Ulid;

fn tenant() -> TenantId {
    TenantId::new("adr0098-r7-refork").expect("tenant")
}

fn meta() -> LedgerMeta {
    LedgerMeta::new(tenant(), BinaryHash::from_bytes([0u8; 32]))
}

fn scratch(name: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("adr0098_r7_{name}_{}.duckdb", Ulid::new()));
    let _ = std::fs::remove_file(&p);
    p
}

/// Append one `RestoreBuyerBackfillCycleCompleted` cycle entry on the given
/// connection — the exact event `append_backfill_cycle_entry` writes.
fn append_cycle(conn: &mut Connection, tag: &str) {
    let m = meta();
    let tx = conn.transaction().expect("tx");
    append_in_tx(
        &tx,
        &m,
        EventKind::RestoreBuyerBackfillCycleCompleted,
        format!("{{\"trigger\":\"boot\",\"tag\":\"{tag}\"}}").into_bytes(),
        Actor::from_local_cli(Ulid::new().to_string(), tag),
        None,
    )
    .expect("append cycle entry");
    tx.commit().expect("commit cycle entry");
}

fn head_seq(conn: &Connection) -> u64 {
    conn.query_row("SELECT COALESCE(MAX(seq), 0) FROM audit_ledger", [], |r| {
        r.get::<_, i64>(0)
    })
    .expect("head") as u64
}

/// **Arm A — the RED reproduction.** A separate-instance backfill write forks
/// the ledger head off a stale view (the pre-R7 boot behaviour): a persistent
/// instance holds a newer head, a transient instance (the old raw opener) reads
/// a stale head, re-assigns an already-taken `seq`, and a row is lost — the
/// 415/416 divergence.
#[test]
fn r7_separate_instance_backfill_write_forks_the_ledger_head() {
    let path = scratch("refork");

    // Persistent instance P — the live writer holding an advanced head.
    let mut p = Connection::open(&path).expect("open P");
    ensure_schema(&p).expect("schema P");
    append_cycle(&mut p, "P-boot-1"); // seq=1 in P's instance
    assert_eq!(head_seq(&p), 1, "P holds its own head=1");

    // Transient instance S — the OLD `append_backfill_cycle_entry` shape: a
    // separate `Connection::open` on the same path. It does NOT observe P's
    // committed row (no shared buffer cache) and appends off a stale head.
    {
        let mut s = Connection::open(&path).expect("open S (old raw opener)");
        ensure_schema(&s).expect("schema S");
        append_cycle(&mut s, "S-backfill-cycle");
        drop(s);
    }

    // P is stale — it never saw S's row (the fork window).
    assert_eq!(
        head_seq(&p),
        1,
        "documented hazard: persistent P does not observe the separate backfill instance's row"
    );

    // P appends again, reusing a seq the separate instance already burned.
    append_cycle(&mut p, "P-boot-2");
    drop(p);

    // Three writes were issued (P-boot-1, S-backfill-cycle, P-boot-2) but the
    // fork collided on `seq`, so the ledger holds fewer distinct rows than
    // writes — the seq-415 corruption the shared Handle prevents.
    let fresh = Connection::open(&path).expect("reopen fresh");
    let total: i64 = fresh
        .query_row("SELECT COUNT(*) FROM audit_ledger", [], |r| r.get(0))
        .expect("count");
    let distinct: i64 = fresh
        .query_row("SELECT COUNT(DISTINCT seq) FROM audit_ledger", [], |r| {
            r.get(0)
        })
        .expect("distinct");
    assert!(
        total < 3,
        "separate-instance backfill fork must lose a row (issued 3 writes, ledger has {total})"
    );
    assert_eq!(
        total, distinct,
        "surviving rows collide on seq — the re-fork the shared Handle eliminates"
    );
    let _ = std::fs::remove_file(&path);
}

/// **Arm B — the GREEN pin.** The SAME cycle-audit append routed through the one
/// shared `aberp_db::Handle`, interleaved with a boot burst of other Handle
/// writes, is coherent: dense + monotonic `seq`, no duplicate `seq`,
/// `verify_chain` clean, and DB head == mirror head (the lockstep `sync_mirror`
/// fires on the `WriteGuard` drop — never a separate opener). This is the shape
/// `append_backfill_cycle_entry` takes AFTER ADR-0098 R7.
#[test]
fn r7_backfill_cycle_via_shared_handle_is_coherent_no_refork() {
    let path = scratch("handle");

    // Seed the schema + a first checkpoint the way a boot does.
    {
        let conn = Connection::open(&path).expect("seed open");
        ensure_schema(&conn).expect("seed schema");
        conn.execute_batch("CHECKPOINT;").expect("seed checkpoint");
    }

    // checkpoint disabled so the assertion isolates the mirror lockstep.
    let cfg = HandleConfig {
        checkpoint_enabled: false,
        ..Default::default()
    };
    let handle = Handle::open(&path, tenant(), cfg).expect("open shared Handle");

    // A boot burst: the backfill cycle entry interleaved with other Handle
    // writes, all through the ONE instance (the single-writer discipline).
    let burst = 8u64;
    for i in 0..burst {
        let mut g = handle.write().expect("shared writer");
        append_cycle(&mut g, &format!("handle-cycle-{i}"));
        // guard drop -> post-commit lockstep sync_mirror on the shared instance
    }

    // DB coherence: dense monotonic seqs, no dup, verify_chain clean.
    let conn = handle.read().expect("shared read clone");
    let db_entries = recent_entries(&conn, u32::MAX).expect("recent entries");
    assert_eq!(
        db_entries.len() as u64,
        burst,
        "every Handle-routed cycle append is durable (no lost row)"
    );
    let total: i64 = conn
        .query_row("SELECT COUNT(*) FROM audit_ledger", [], |r| r.get(0))
        .expect("count");
    let distinct: i64 = conn
        .query_row("SELECT COUNT(DISTINCT seq) FROM audit_ledger", [], |r| {
            r.get(0)
        })
        .expect("distinct");
    assert_eq!(
        total, distinct,
        "no duplicate seq across the boot burst (the fork is gone)"
    );
    assert_eq!(
        head_seq(&conn),
        burst,
        "head is dense + monotonic == burst size (no stale-head re-assignment)"
    );

    let ledger = Ledger::open(&path, tenant(), BinaryHash::from_bytes([0u8; 32]))
        .expect("reopen ledger for verify");
    let verified = ledger.verify_chain().expect("chain verifies");
    assert_eq!(
        verified as u64, burst,
        "verify_chain clean across the whole Handle-routed burst"
    );

    // DB head == mirror head — the R1 boot guard's coherence invariant.
    let mirror = mirror_path_for(&path);
    let mirror_entries = read_mirror_entries(&mirror).expect("read mirror");
    let mirror_head = mirror_entries.last().map(|e| e.seq).unwrap_or(0);
    assert_eq!(
        mirror_head, burst,
        "mirror head ({mirror_head}) == DB head ({burst}) — lockstep sync_mirror held (DB==mirror)"
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&mirror);
}
