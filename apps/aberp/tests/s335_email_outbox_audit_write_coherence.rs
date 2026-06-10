//! S335 / PR-32 — coherence regression pinning WHY the email-outbox
//! daemon's `write_audit` deliberately reopens a fresh DuckDB connection
//! per write instead of holding a persistent one.
//!
//! ## Background
//!
//! The S335 brief prescribed converting the daemon to a persistent audit
//! `Connection` (`Arc<Mutex<Connection>>`) to kill the O(n²) per-write
//! checkpoint cost S332 measured. A coherence probe run during S335
//! REFUTED that fix: DuckDB `Connection::open` creates an independent
//! `Database` instance with no shared buffer cache across handles
//! (documented at `apps/aberp/src/incoming_invoices.rs:54-74`). A
//! persistent connection therefore does NOT observe rows another
//! connection committed; on its next append it reads a STALE chain head,
//! recomputes an already-taken `seq`, and forks the tamper-evident hash
//! chain — silently losing a row. That is strictly WORSE than the
//! contained, caught-and-logged log-spam the brief set out to fix.
//!
//! This test makes the probe a permanent guard:
//!   * Part A pins that the SHIPPED pattern (reopen-per-write, even
//!     interleaved across two writers) stays coherent — dense monotonic
//!     seqs, `verify_chain` clean, no rows lost.
//!   * Part B pins the HAZARD (a persistent connection interleaved with a
//!     transient one DOES fork / lose rows), mirroring the documented-quirk
//!     test style in `incoming_invoices.rs`. If a future contributor
//!     "optimizes" `write_audit` into a held connection, Part B documents
//!     exactly the corruption that would follow.

use aberp_audit_ledger::{
    append_in_tx, ensure_schema, Actor, BinaryHash, EventKind, Ledger, LedgerMeta, TenantId,
};
use duckdb::Connection;
use ulid::Ulid;

fn meta() -> (LedgerMeta, TenantId, BinaryHash) {
    let tenant = TenantId::new("s335-coherence").expect("tenant");
    let binary_hash = BinaryHash::from_bytes([0u8; 32]);
    (
        LedgerMeta::new(tenant.clone(), binary_hash),
        tenant,
        binary_hash,
    )
}

fn scratch(name: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("s335_coherence_{name}_{}.duckdb", Ulid::new()));
    let _ = std::fs::remove_file(&p);
    p
}

/// One reopen-per-write append — byte-for-byte the daemon's `write_audit`
/// shape (fresh open, ensure_schema, single-row tx, commit, drop).
fn reopen_write(path: &std::path::Path, meta: &LedgerMeta, tag: &str) {
    let mut conn = Connection::open(path).expect("open");
    ensure_schema(&conn).expect("schema");
    let tx = conn.transaction().expect("tx");
    let actor = Actor::from_local_cli(Ulid::new().to_string(), tag);
    append_in_tx(
        &tx,
        meta,
        EventKind::EmailOutboxFetched,
        format!("{{\"fetched_count\":0,\"tag\":\"{tag}\"}}").into_bytes(),
        actor,
        None,
    )
    .expect("append");
    tx.commit().expect("commit");
}

fn head_seq(conn: &Connection) -> u64 {
    conn.query_row("SELECT COALESCE(MAX(seq),0) FROM audit_ledger", [], |r| {
        r.get::<_, i64>(0)
    })
    .expect("head") as u64
}

/// Part A — the SHIPPED pattern stays coherent under interleaving.
///
/// Two independent writers (simulating the email-outbox daemon racing
/// another audit producer), each using reopen-per-write, interleave their
/// appends against the same file. The result is a single coherent chain:
/// dense + monotonic seqs, `verify_chain` clean, every write durable.
#[test]
fn s335_reopen_per_write_interleaved_stays_coherent() {
    let path = scratch("reopen");
    let (m, tenant, bh) = meta();

    // Interleave: writer-1, writer-2, writer-1, writer-2, ...
    let n_each = 25;
    for i in 0..n_each {
        reopen_write(&path, &m, &format!("w1-{i}"));
        reopen_write(&path, &m, &format!("w2-{i}"));
    }
    let total = n_each * 2;

    let ledger = Ledger::open(&path, tenant, bh).expect("reopen ledger");
    let verified = ledger.verify_chain().expect("chain verifies");
    assert_eq!(
        verified as usize, total,
        "interleaved reopen-per-write must verify all {total} entries"
    );
    let entries = ledger.entries().expect("entries");
    assert_eq!(entries.len(), total, "no rows lost under interleaving");
    for (idx, e) in entries.iter().enumerate() {
        assert_eq!(
            e.seq.as_u64(),
            idx as u64 + 1,
            "seq must be dense + monotonic (no fork)"
        );
    }
    let _ = std::fs::remove_file(&path);
}

/// Part B — the HAZARD a persistent connection would introduce.
///
/// A persistent connection A (held open) interleaved with a transient
/// connection B (separate `Database` instance) forks the chain: after B
/// commits + closes, A does NOT see B's row, recomputes B's `seq`, and the
/// ledger ends up with FEWER distinct rows than writes issued. This is the
/// documented-quirk pin — it proves the brief's persistent-connection fix
/// would corrupt the crown-jewel ledger, which is why S335 keeps
/// reopen-per-write.
#[test]
fn s335_persistent_connection_forks_chain_documented_hazard() {
    let path = scratch("persistent");
    let (m, _tenant, _bh) = meta();

    // Persistent connection A — opened once, held for the whole test.
    let mut a = Connection::open(&path).expect("open A");
    ensure_schema(&a).expect("schema A");

    // A writes seq=1.
    {
        let tx = a.transaction().expect("A tx");
        append_in_tx(
            &tx,
            &m,
            EventKind::EmailOutboxFetched,
            b"{\"fetched_count\":0,\"tag\":\"A-1\"}".to_vec(),
            Actor::from_local_cli(Ulid::new().to_string(), "A-1"),
            None,
        )
        .expect("A append 1");
        tx.commit().expect("A commit 1");
    }
    assert_eq!(head_seq(&a), 1, "A sees its own seq=1");

    // Transient B writes seq=2 and closes.
    {
        let mut b = Connection::open(&path).expect("open B");
        ensure_schema(&b).expect("schema B");
        let tx = b.transaction().expect("B tx");
        append_in_tx(
            &tx,
            &m,
            EventKind::EmailOutboxFetched,
            b"{\"fetched_count\":0,\"tag\":\"B-2\"}".to_vec(),
            Actor::from_local_cli(Ulid::new().to_string(), "B-2"),
            None,
        )
        .expect("B append 2");
        tx.commit().expect("B commit 2");
    }

    // The hazard: persistent A is STALE — it still sees head=1, not B's 2.
    assert_eq!(
        head_seq(&a),
        1,
        "documented hazard: persistent A does NOT observe B's committed row"
    );

    // A writes again — reusing seq=2 because it never saw B's row.
    {
        let tx = a.transaction().expect("A tx 2");
        append_in_tx(
            &tx,
            &m,
            EventKind::EmailOutboxFetched,
            b"{\"fetched_count\":0,\"tag\":\"A-next\"}".to_vec(),
            Actor::from_local_cli(Ulid::new().to_string(), "A-next"),
            None,
        )
        .expect("A append next");
        tx.commit().expect("A commit next");
    }
    drop(a);

    // Three writes were issued (A-1, B-2, A-next) but the fork lost one:
    // fewer rows than writes, and distinct-seq count is short. THIS is the
    // corruption reopen-per-write prevents.
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
        "persistent-connection fork must lose a row (issued 3 writes, ledger has {total})"
    );
    assert_eq!(
        total, distinct,
        "the surviving rows collide on seq — the fork the reopen pattern avoids"
    );
    let _ = std::fs::remove_file(&path);
}
