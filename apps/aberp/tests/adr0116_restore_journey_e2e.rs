//! **ADR-0116 — the customer-journey e2e gate.**
//!
//! `[[customer-journey-e2e-gate]]`: snapshot → simulated loss → restore via
//! the REAL `aberp` binary → verify. Operator-internal, but the highest-stakes
//! journey in the product — it is the one an operator walks at 02:00 after a
//! durability incident, and it has been walked in anger twice (2026-08-03 and
//! 2026-08-08, both through a restore with zero fsyncs).
//!
//! Every step drives the **built binary through its real CLI**, not the
//! library functions. A library-level test would miss exactly the class of
//! defect that matters here: an argument that does not parse, a guard wired to
//! the wrong flag, a subcommand that silently no-ops. The one thing this
//! cannot cover is genuine crash injection (see the AC-1 note in
//! `crates/aberp-snapshot/tests/adr0116_snapshot_system.rs`).

use std::path::{Path, PathBuf};
use std::process::Command;

use aberp_audit_ledger::{Actor, BinaryHash, EventKind, Ledger, TenantId};
use duckdb::Connection;

// ── scaffolding ────────────────────────────────────────────────────────

struct Tmp(PathBuf);
impl Tmp {
    fn new(label: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static C: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let seq = C.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "aberp-adr0116-journey-{label}-{}-{nanos}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    fn path(&self) -> &Path {
        &self.0
    }
    /// A tenant-home-SHAPED directory, so the ADR-0116 D2 evidence guard's
    /// allow-list inversion applies exactly as it does in production.
    fn home(&self) -> PathBuf {
        let h = self.0.join(".aberp-defense").join("defense");
        std::fs::create_dir_all(&h).unwrap();
        h
    }
    fn db(&self) -> PathBuf {
        self.home().join("aberp.duckdb")
    }
    fn store(&self) -> PathBuf {
        self.0.join("store")
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const TENANT: &str = "defense";

fn seed(db: &Path, n_invoice: usize, n_audit: usize) {
    {
        let conn = Connection::open(db).unwrap();
        conn.execute_batch("PRAGMA disable_checkpoint_on_shutdown;")
            .unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS invoice (id BIGINT, amount DOUBLE, note VARCHAR);",
        )
        .unwrap();
        for i in 0..n_invoice {
            conn.execute(
                "INSERT INTO invoice VALUES (?, ?, ?)",
                duckdb::params![i as i64, (i as f64) * 10.0, format!("inv-{i}")],
            )
            .unwrap();
        }
    }
    let tid = TenantId::new(TENANT.to_string()).unwrap();
    let mut ledger = Ledger::open(db, tid, BinaryHash::from_bytes([7u8; 32])).unwrap();
    for i in 0..n_audit {
        ledger
            .append(
                EventKind::Test,
                format!("{{\"i\":{i}}}").into_bytes(),
                Actor::test_only(),
                None,
            )
            .unwrap();
    }
}

fn invoice_count(db: &Path) -> i64 {
    let conn = Connection::open(db).unwrap();
    conn.query_row("SELECT count(*) FROM invoice", [], |r| r.get(0))
        .unwrap()
}

fn audit_count(db: &Path) -> i64 {
    let conn = Connection::open(db).unwrap();
    conn.query_row("SELECT count(*) FROM audit_ledger", [], |r| r.get(0))
        .unwrap()
}

struct Run {
    ok: bool,
    stdout: String,
    stderr: String,
}

/// Run the REAL built `aberp` binary. `HOME` is redirected into the scratch
/// dir so nothing can reach the operator's actual tenant homes, snapshot
/// stores, or recovery evidence — the whole point of the sandbox.
fn aberp(tmp: &Tmp, args: &[&str]) -> Run {
    let out = Command::new(env!("CARGO_BIN_EXE_aberp"))
        .args(args)
        .env("HOME", tmp.path())
        .env("ABERP_SNAPSHOT_DISABLE", "")
        .output()
        .expect("spawn aberp binary");
    Run {
        ok: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn wal_of(db: &Path) -> PathBuf {
    let mut os = db.as_os_str().to_owned();
    os.push(".wal");
    PathBuf::from(os)
}

// ══════════════════════════════════════════════════════════════════════
// THE JOURNEY — snapshot → loss → restore → verify, all through the CLI
// ══════════════════════════════════════════════════════════════════════

/// **The gate.** An operator's full recovery, driven end to end through the
/// shipped CLI:
///
///  1. `aberp snapshot now` — take a validated rollback point;
///  2. `aberp snapshot list --json` — the store is machine-readable and the
///     snapshot carries a STABLE identity, not a bare seq;
///  3. simulate the loss — the live DB is destroyed;
///  4. `aberp snapshot restore --dry-run` — the operator SEES the restore
///     before committing to it, and nothing is written;
///  5. `aberp snapshot restore … --confirm` — rebuild to a side path;
///  6. verify the rebuilt DB has the rows and a verifiable chain;
///  7. `aberp snapshot restore` records the event on the LIVE ledger.
#[test]
fn journey_snapshot_then_loss_then_restore_cli_then_verify() {
    let t = Tmp::new("journey");
    let db = t.db();
    let store = t.store();
    let (db_s, store_s) = (db.to_str().unwrap(), store.to_str().unwrap());

    // ── 1. truth before the loss ────────────────────────────────────────
    seed(&db, 3, 4);
    let invoices_before = invoice_count(&db);
    let audit_before = audit_count(&db);
    assert_eq!(invoices_before, 3);

    let r = aberp(
        &t,
        &[
            "snapshot", "now", "--db", db_s, "--tenant", TENANT, "--store", store_s,
        ],
    );
    assert!(
        r.ok,
        "step 1 — `aberp snapshot now` failed.\nstdout: {}\nstderr: {}",
        r.stdout, r.stderr
    );
    assert!(
        r.stdout.contains("written and validated"),
        "the snapshot must be VALIDATED, not merely written: {}",
        r.stdout
    );

    // ── 2. the store is listable, and by a STABLE identity ──────────────
    let r = aberp(
        &t,
        &[
            "snapshot", "list", "--tenant", TENANT, "--store", store_s, "--json",
        ],
    );
    assert!(r.ok, "step 2 — `snapshot list --json` failed: {}", r.stderr);
    let listed: serde_json::Value =
        serde_json::from_str(&r.stdout).expect("`--json` must emit parseable JSON");
    assert_eq!(listed["count"], 1);
    let id = listed["snapshots"][0]["id"]
        .as_str()
        .expect("every snapshot must carry a stable id")
        .to_string();
    assert!(
        id.contains('@') && id.contains('#'),
        "ADR-0116 D3.3/G6 — the identity must be (seq, created_at, source_db_sha256), not a \
         bare seq: seq is RECYCLED after a prune and names more than one snapshot in prod's \
         own ledger. Got: {id}"
    );
    assert_eq!(
        listed["snapshots"][0]["retained_as"], "rollback-point",
        "a valid snapshot is a rollback point; a retained INVALID one is forensic evidence, \
         and a consumer must be able to tell them apart"
    );

    // ── 3. simulate the loss ────────────────────────────────────────────
    // The live database is destroyed. This is the 2026-08-08 shape: the file
    // is gone or unusable and the operator has only the snapshot store.
    let live_before_loss = std::fs::read(&db).unwrap();
    std::fs::remove_file(&db).unwrap();
    let _ = std::fs::remove_file(wal_of(&db));
    assert!(!db.exists(), "the loss must actually happen");

    // ── 4. dry-run: SEE the restore, write nothing ──────────────────────
    let target = t.path().join("recovery").join("rebuilt.duckdb");
    let target_s = target.to_str().unwrap();
    let store_mtime_before = std::fs::metadata(&store).unwrap().modified().unwrap();

    let r = aberp(
        &t,
        &[
            "snapshot",
            "restore",
            &id,
            "--to",
            target_s,
            "--tenant",
            TENANT,
            "--db",
            db_s,
            "--store",
            store_s,
            "--dry-run",
        ],
    );
    assert!(
        r.ok,
        "step 4 — the dry-run must EXIT 0 when the restore would proceed (the exit code is how \
         a script distinguishes 'would proceed' from 'would refuse').\nstdout: {}\nstderr: {}",
        r.stdout, r.stderr
    );
    assert!(
        r.stdout.contains("NOTHING is written") && r.stdout.contains("VERDICT: would PROCEED"),
        "the dry-run must state its verdict plainly: {}",
        r.stdout
    );
    assert!(
        r.stdout.contains("anchors"),
        "ADR-0116 D4 — anchor coverage must be reported prominently, so the operator always \
         sees what a restored DB will and will not be able to prove: {}",
        r.stdout
    );
    assert!(
        !target.exists(),
        "ADR-0116 AC-5 — `--dry-run` wrote the target. It must write NOTHING."
    );
    assert_eq!(
        std::fs::metadata(&store).unwrap().modified().unwrap(),
        store_mtime_before,
        "ADR-0116 AC-5 — `--dry-run` modified the snapshot store"
    );

    // ── 5. the real restore ─────────────────────────────────────────────
    let r = aberp(
        &t,
        &[
            "snapshot",
            "restore",
            &id,
            "--to",
            target_s,
            "--tenant",
            TENANT,
            "--db",
            db_s,
            "--store",
            store_s,
            "--confirm",
        ],
    );
    assert!(
        r.ok,
        "step 5 — the restore failed.\nstdout: {}\nstderr: {}",
        r.stdout, r.stderr
    );

    // ── 6. verify the rebuilt database ──────────────────────────────────
    assert!(target.exists(), "the restore produced no file");
    assert_eq!(
        invoice_count(&target),
        invoices_before,
        "step 6 — the rebuilt DB lost invoices"
    );
    assert_eq!(
        audit_count(&target),
        audit_before,
        "step 6 — the rebuilt DB lost audit entries"
    );
    // The chain must re-verify end to end against the tenant genesis — a
    // restored database whose chain does not verify is not a recovery.
    {
        let conn = Connection::open(&target).unwrap();
        conn.execute_batch("PRAGMA disable_checkpoint_on_shutdown;")
            .unwrap();
        let ledger = aberp_audit_ledger::Ledger::from_connection(
            conn,
            TenantId::new(TENANT.to_string()).unwrap(),
            BinaryHash::from_bytes([0u8; 32]),
        );
        let len = ledger
            .verify_chain()
            .expect("step 6 — the restored chain must verify end to end");
        assert_eq!(len as i64, audit_before);
    }
    // ADR-0116 D3.1 — no orphan WAL beside the installed file.
    assert!(
        !wal_of(&target).exists(),
        "ADR-0116 D3.1 — a WAL was left beside the restored file; on next open DuckDB would \
         replay it into a database it has nothing to do with"
    );

    // The pre-loss live file is genuinely gone — this journey rebuilt from the
    // snapshot, it did not merely find the old file.
    assert_ne!(
        std::fs::read(&target).unwrap(),
        live_before_loss,
        "the rebuilt file must be a fresh logical IMPORT, not a copy of the pre-loss file"
    );
}

/// **ADR-0116 AC-5 + D3.3** — the pre-flight refuses a backwards rollback the
/// operator has not acknowledged, its exit code says so, and it writes nothing.
///
/// Driven through `restore --in-place`, because that is the path where the
/// live counts are EXACT: it refuses unless serve is stopped, so it holds the
/// file exclusively and can read the true delta. The side-path command
/// deliberately does not open a possibly-live database (ADR-0098) and reports
/// a lower bound instead — pinned separately below.
#[test]
fn journey_preflight_refuses_an_unacknowledged_backwards_rollback() {
    let t = Tmp::new("rollback-ack");
    let db = t.db();
    let store = t.store();
    let (db_s, store_s) = (db.to_str().unwrap(), store.to_str().unwrap());
    seed(&db, 2, 3);
    assert!(
        aberp(
            &t,
            &["snapshot", "now", "--db", db_s, "--tenant", TENANT, "--store", store_s]
        )
        .ok,
        "fixture: the snapshot must succeed"
    );

    // Commit more entries AFTER the snapshot: the live DB is now ahead, and
    // restoring would silently discard them. This is the "am I about to roll
    // back 5 days of invoices?" question the pre-flight exists to answer with
    // a machine-checked verdict rather than the operator's memory.
    seed(&db, 0, 5);
    let live_audit_before = audit_count(&db);

    let args = |extra: &[&str]| -> Vec<String> {
        let mut v: Vec<String> = [
            "restore",
            "--in-place",
            "--tenant",
            TENANT,
            "--snapshot",
            "1",
            "--db",
            db_s,
            "--store",
            store_s,
            "--dry-run",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        v.extend(extra.iter().map(|s| s.to_string()));
        v
    };
    fn as_refs(v: &[String]) -> Vec<&str> {
        v.iter().map(String::as_str).collect()
    }

    let a = args(&[]);
    let r = aberp(&t, &as_refs(&a));
    assert!(
        !r.ok,
        "ADR-0116 AC-5 — the dry-run must exit NON-ZERO when the pre-flight would refuse; the \
         exit code is how a script distinguishes 'would proceed' from 'would refuse'.\nstdout: {}",
        r.stdout
    );
    assert!(
        r.stdout.contains("VERDICT: would REFUSE") && r.stdout.contains("AHEAD"),
        "the refusal must name the live-DB delta, not merely fail: {}",
        r.stdout
    );
    assert!(
        r.stdout.contains("EXACT (serve is stopped)"),
        "ADR-0116 D3.2 — with serve stopped the delta must be EXACT, not the mirror's lower \
         bound. The mirror lags whenever writes come through Ledger::append (the 15 CLI \
         money-submission sites, D-22), which is precisely the serve-down window an operator \
         reaches for a restore in: {}",
        r.stdout
    );
    assert_eq!(
        audit_count(&db),
        live_audit_before,
        "a refusing dry-run must not modify the live DB"
    );

    // With the acknowledgement, the same pre-flight PROCEEDS. Rolling
    // backwards past committed rows is what a rollback IS — the gate makes it
    // a decision, it does not ban it.
    let a = args(&["--accept-data-loss"]);
    let r = aberp(&t, &as_refs(&a));
    assert!(
        r.ok,
        "ADR-0116 D3.3 — --accept-data-loss must let an ACKNOWLEDGED rollback proceed. A gate \
         that cannot be passed is not a gate, it is a ban, and an operator would work around \
         it with `cp`.\nstdout: {}\nstderr: {}",
        r.stdout, r.stderr
    );
    assert!(r.stdout.contains("VERDICT: would PROCEED"), "{}", r.stdout);
}

/// **ADR-0116 D3.2 — the side-path dry-run must be HONEST about its bound.**
///
/// It does not open the live database, because `aberp serve` may be running
/// and a second DuckDB instance on a live file is the ADR-0098 two-instance
/// hazard. So its delta comes from the audit mirror, which is a LOWER BOUND:
/// `Ledger::append` commits without syncing the mirror, so with serve down the
/// mirror can lag the DB. A report that presented that as "nothing would be
/// lost" would be worse than no report, because an operator would act on it.
#[test]
fn journey_side_path_dry_run_states_its_lower_bound_rather_than_claiming_certainty() {
    let t = Tmp::new("lower-bound");
    let db = t.db();
    let store = t.store();
    let (db_s, store_s) = (db.to_str().unwrap(), store.to_str().unwrap());
    seed(&db, 2, 3);
    assert!(
        aberp(
            &t,
            &["snapshot", "now", "--db", db_s, "--tenant", TENANT, "--store", store_s]
        )
        .ok
    );
    // Entries that DO NOT reach the mirror — the exact lag this warning is
    // about.
    seed(&db, 0, 5);

    let target = t.path().join("side.duckdb");
    let r = aberp(
        &t,
        &[
            "snapshot",
            "restore",
            "1",
            "--to",
            target.to_str().unwrap(),
            "--tenant",
            TENANT,
            "--db",
            db_s,
            "--store",
            store_s,
            "--dry-run",
        ],
    );
    assert!(
        r.stdout.contains("LOWER BOUND"),
        "the side-path delta must be labelled a LOWER BOUND: {}",
        r.stdout
    );
    assert!(
        r.stdout.contains("commits WITHOUT syncing the mirror")
            && r.stdout.contains("--in-place --dry-run"),
        "the report must say WHY it is only a bound and where to get an exact answer, or an \
         operator will read 'nothing newer' as proof: {}",
        r.stdout
    );
    assert!(!target.exists(), "a dry-run must write nothing");
}

/// **ADR-0116 D3.4 / AC-9, end to end through the CLI** — `aberp restore
/// --in-place` replaces the hand-swap: it snapshots first, preserves the
/// DB+WAL+marker unit, installs crash-safely, and leaves the mirror alone.
#[test]
fn journey_in_place_restore_replaces_the_hand_swap() {
    let t = Tmp::new("in-place");
    let db = t.db();
    let store = t.store();
    let (db_s, store_s) = (db.to_str().unwrap(), store.to_str().unwrap());

    seed(&db, 2, 3);
    assert!(
        aberp(
            &t,
            &["snapshot", "now", "--db", db_s, "--tenant", TENANT, "--store", store_s]
        )
        .ok,
        "fixture: the snapshot must succeed"
    );

    // Diverge the live DB from the snapshot, so a successful restore is
    // observable rather than a no-op.
    seed(&db, 3, 0);
    assert_eq!(invoice_count(&db), 5);

    let mirror = aberp_audit_ledger::mirror_path_for(&db);
    let mirror_before = std::fs::read(&mirror).expect("the mirror must exist after seeding");

    let r = aberp(
        &t,
        &[
            "snapshot", "list", "--tenant", TENANT, "--store", store_s, "--json",
        ],
    );
    let listed: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    let id = listed["snapshots"][0]["id"].as_str().unwrap().to_string();

    // Dry-run first: nothing written, and the operator is told what will move.
    let r = aberp(
        &t,
        &[
            "restore",
            "--in-place",
            "--tenant",
            TENANT,
            "--snapshot",
            &id,
            "--db",
            db_s,
            "--store",
            store_s,
            "--dry-run",
            "--accept-data-loss",
        ],
    );
    assert!(
        r.ok,
        "the in-place dry-run failed.\nstdout: {}\nstderr: {}",
        r.stdout, r.stderr
    );
    assert!(
        r.stdout.contains("PRE-RESTORE") && r.stdout.contains(".audit.log mirror would NOT move"),
        "ADR-0116 D3.4 — the dry-run must state that the DB+WAL+marker move as a unit and that \
         the mirror does not: {}",
        r.stdout
    );
    assert_eq!(invoice_count(&db), 5, "the dry-run modified the live DB");

    // The real thing.
    let r = aberp(
        &t,
        &[
            "restore",
            "--in-place",
            "--tenant",
            TENANT,
            "--snapshot",
            &id,
            "--db",
            db_s,
            "--store",
            store_s,
            "--confirm",
            "--accept-data-loss",
        ],
    );
    assert!(
        r.ok,
        "the in-place restore failed.\nstdout: {}\nstderr: {}",
        r.stdout, r.stderr
    );
    assert!(
        r.stdout
            .contains("Pre-restore snapshot of the CURRENT database"),
        "ADR-0116 D3.4 step 2 / G7 — the restore must preserve what it is about to overwrite \
         BEFORE overwriting it. Restoring is the single most destructive operation the system \
         offers and it did not previously do this: {}",
        r.stdout
    );

    // ── the restored state ──
    assert_eq!(
        invoice_count(&db),
        2,
        "the live DB must now hold the snapshot's 2 invoices"
    );
    assert!(
        !wal_of(&db).exists(),
        "ADR-0116 D3.1/F4 — no orphan WAL may remain beside the restored file"
    );

    // ── the preserved unit, as ONE unit ──
    let preserved: Vec<String> = std::fs::read_dir(t.home())
        .unwrap()
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .filter(|n| n.contains("PRE-RESTORE-"))
        .collect();
    assert!(
        preserved.iter().any(|n| n.ends_with(".wal")),
        "ADR-0116 AC-9/F4 — the preserved unit must include the WAL, named so it PAIRS with \
         the preserved DB (`<db>.PRE-RESTORE-<tag>.wal`). A DB moved without its WAL is \
         stripped of its un-checkpointed commits and is not a recoverable original. Found: \
         {preserved:?}"
    );
    // The preserved unit must be openable and hold the pre-restore rows.
    let preserved_db = t.home().join(
        preserved
            .iter()
            .find(|n| n.contains("PRE-RESTORE-") && !n.ends_with(".wal") && !n.contains("ckpt-ok"))
            .expect("the preserved DB itself must exist"),
    );
    assert_eq!(
        invoice_count(&preserved_db),
        5,
        "ADR-0116 AC-9 — the preserved unit must open and hold the state that was replaced"
    );

    // ── the mirror stayed, and was only ever APPENDED to ──
    //
    // Byte-identity would be the WRONG assertion here, and getting it wrong is
    // instructive: the mandatory pre-restore snapshot (step 2) legitimately
    // appends its own `snapshot.created` row and reconciles the mirror. F5
    // resolves exactly this — D3.5's "do not reconcile the mirror" rule is
    // scoped to AFTER the install; the pre-restore snapshot runs before
    // anything is overwritten, on the DB that is about to be replaced.
    //
    // So the invariants are: the mirror did not MOVE, and it was only extended
    // — never truncated, never rewritten.
    assert!(
        mirror.exists(),
        "ADR-0116 D3.4 step 4 — the .audit.log mirror must NOT move. It is the durable record \
         and stays at the live path; an implementer moving 'the tenant's DB artefacts' would \
         naturally take it too."
    );
    assert!(
        !preserved.iter().any(|n| n.contains("audit.log")),
        "ADR-0116 D3.4 step 4 — the mirror was moved into the .PRE-RESTORE- unit: {preserved:?}"
    );
    let mirror_after = std::fs::read(&mirror).unwrap();
    assert!(
        mirror_after.starts_with(&mirror_before),
        "ADR-0116 — the audit mirror is APPEND-ONLY and was rewritten or truncated by the \
         restore. The mirror is half the audit ledger; a rewritten mirror is a forked chain."
    );

    // ── D3.5/F8 — the restore event landed on the RESTORED chain ────────
    //
    // For --in-place the live DB IS the restored DB, so the two collapse and
    // the row is the next seq on the restored chain — no pre-seeded seq, no
    // out-of-band edit of the restored file (the 2026-08-03 heal-path lesson).
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch("PRAGMA disable_checkpoint_on_shutdown;")
            .unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM audit_ledger WHERE kind = 'snapshot.restored'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            n, 1,
            "ADR-0116 D3.5/F8 — `restore --in-place` must record snapshot.restored on the \
             RESTORED chain"
        );
        // …and the chain must still verify with that row on it.
        let ledger = aberp_audit_ledger::Ledger::from_connection(
            conn,
            TenantId::new(TENANT.to_string()).unwrap(),
            BinaryHash::from_bytes([0u8; 32]),
        );
        ledger
            .verify_chain()
            .expect("ADR-0116 D3.5 — the restored chain must still verify with the restore row");
    }
    // The mirror must NOT have received that row: no reconcile happens after
    // the install, by design. If the mirror disagrees with the restored DB,
    // that is recover_or_refuse's decision at the next boot.
    assert!(
        !String::from_utf8_lossy(&mirror_after).contains("snapshot.restored"),
        "ADR-0116 D3.5 — the mirror was reconciled AFTER the install. The restore must leave \
         any mirror/DB disagreement for the next boot's recover_or_refuse to own."
    );

    // ── the evidence guard protects what was just created ──
    for n in &preserved {
        assert!(
            aberp_snapshot::is_protected_evidence(&t.home().join(n)),
            "ADR-0116 D2 — the .PRE-RESTORE- unit is recovery evidence and must be protected \
             from every future cleanup helper: {n}"
        );
    }
}

/// **ADR-0116 D2** — `aberp evidence list` makes the previously-invisible
/// footprint visible, and `archive` never deletes without a verified copy.
#[test]
fn journey_evidence_is_visible_and_release_is_never_deletion() {
    let t = Tmp::new("evidence");
    let home = t.home();
    let db = t.db();
    seed(&db, 1, 2);

    // Two artefacts of one OLD incident (releasable), and one that is not.
    for f in [
        "aberp.duckdb.CORRUPT-20260101T000000Z",
        "aberp.duckdb.CORRUPT-20260101T000000Z.wal",
    ] {
        std::fs::write(home.join(f), b"forensic bytes").unwrap();
    }
    std::fs::write(home.join("prod-20260102-keychain.zip"), b"secrets").unwrap();

    let r = aberp(
        &t,
        &[
            "evidence",
            "list",
            "--tenant",
            TENANT,
            "--home",
            home.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(r.ok, "`evidence list` failed: {}", r.stderr);
    let v: serde_json::Value = serde_json::from_str(&r.stdout).expect("parseable JSON");
    let names: Vec<&str> = v["artefacts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"aberp.duckdb.CORRUPT-20260101T000000Z"),
        "ADR-0116 D2 — evidence must be VISIBLE. ~330 MB accumulated in the tenant homes with \
         no way for an operator to see it: {names:?}"
    );
    assert!(
        !names.contains(&"aberp.duckdb"),
        "the live DB is not evidence: {names:?}"
    );
    let keychain = v["artefacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"].as_str().unwrap().contains("keychain"))
        .expect("the credential dump must be inventoried");
    assert_eq!(
        keychain["releasable"], false,
        "ADR-0116 D2.4 — encrypted NAV credentials and an SMTP password are never archived to \
         a second location"
    );

    // Archive without --confirm writes nothing.
    let archive_root = t.path().join("Documents").join("ABERP-evidence");
    let r = aberp(
        &t,
        &[
            "evidence",
            "archive",
            "--tenant",
            TENANT,
            "--home",
            home.to_str().unwrap(),
            "--db",
            db.to_str().unwrap(),
            "--archive-root",
            archive_root.to_str().unwrap(),
            "--dry-run",
        ],
    );
    assert!(r.ok, "`evidence archive --dry-run` failed: {}", r.stderr);
    assert!(
        !archive_root.exists(),
        "a dry-run must not create the archive store"
    );
    assert!(
        home.join("aberp.duckdb.CORRUPT-20260101T000000Z").exists(),
        "a dry-run must not unlink anything"
    );
}

/// **ADR-0116 G8, through the CLI** — a snapshot that FAILS validation is
/// retained and reported distinctly, not deleted by the cycle that made it.
#[test]
fn journey_a_failed_snapshot_is_retained_and_reported_as_forensic() {
    let t = Tmp::new("g8");
    let db = t.db();
    let store = t.store();
    let (db_s, store_s) = (db.to_str().unwrap(), store.to_str().unwrap());

    seed(&db, 2, 4);
    assert!(
        aberp(
            &t,
            &["snapshot", "now", "--db", db_s, "--tenant", TENANT, "--store", store_s]
        )
        .ok
    );

    // Break the chain — the 2026-07-08 shape ("out of order: expected
    // seq=7995, found seq=7994").
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch("PRAGMA disable_checkpoint_on_shutdown;")
            .unwrap();
        conn.execute("DELETE FROM audit_ledger WHERE seq = 2", [])
            .unwrap();
    }
    let r = aberp(
        &t,
        &[
            "snapshot", "now", "--db", db_s, "--tenant", TENANT, "--store", store_s,
        ],
    );
    assert!(
        r.ok,
        "a validation failure is a normal outcome, not a command failure: {}",
        r.stderr
    );
    assert!(
        r.stdout.contains("FAILED validation") && r.stdout.contains("forensic evidence"),
        "the operator must be told the snapshot is kept AS EVIDENCE, not silently: {}",
        r.stdout
    );

    let r = aberp(
        &t,
        &[
            "snapshot", "list", "--tenant", TENANT, "--store", store_s, "--json",
        ],
    );
    let v: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    let forensic: Vec<&serde_json::Value> = v["snapshots"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["retained_as"] == "forensic-evidence")
        .collect();
    assert_eq!(
        forensic.len(),
        1,
        "ADR-0116 G8 — the failed snapshot must SURVIVE the cycle that created it and be \
         reported distinctly. Prod destroyed the only artefact of a live audit-chain fork \
         twice, on a timer, with no operator involved: {}",
        r.stdout
    );
}
