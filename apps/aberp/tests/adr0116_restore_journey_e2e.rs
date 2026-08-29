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
    ///
    /// The edition segment is taken from the BUILD, not hardcoded: `aberp
    /// serve`'s ADR-0093 guard (`guard_db_matches_edition`) refuses a `--db`
    /// outside this edition's own root, so the boot journey below would
    /// `exit(1)` on a Portable build if this said `.aberp-defense` always.
    fn home(&self) -> PathBuf {
        let h = self
            .0
            .join(aberp::build_profile::edition_data_dirname())
            .join(TENANT);
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
        r.stdout.contains("PRE-RESTORE") && r.stdout.contains(".audit.log mirror"),
        "ADR-0116 D3.4 — the dry-run must state that the DB+WAL+marker+mirror move as a unit: \
         {}",
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
            .find(|n| {
                n.contains("PRE-RESTORE-")
                    && !n.ends_with(".wal")
                    && !n.contains("ckpt-ok")
                    && !n.ends_with(".audit.log")
            })
            .expect("the preserved DB itself must exist"),
    );
    assert_eq!(
        invoice_count(&preserved_db),
        5,
        "ADR-0116 AC-9 — the preserved unit must open and hold the state that was replaced"
    );

    // ── the mirror moved INTO the unit, and a fresh one replaced it ──────
    //
    // **This assertion was inverted before rev 2, and the inversion is the
    // blocker it hid.** The old contract was "the mirror does NOT move — it is
    // the durable record and stays at the live path". After a backwards
    // rollback that leaves the discarded audit tail beside the restored
    // database, the step-7 `SnapshotRestored` row lands at a seq the mirror
    // already holds with a different entry, and the NEXT `aberp serve` boot
    // refuses with `MirrorDivergedFromDb`. Nothing in this file walked that
    // boot, so a green suite meant a product that could not start.
    //
    // The discarded tail is not lost: it is preserved inside the
    // `.PRE-RESTORE-` unit, where the ADR-0116 D2 evidence guard protects it.
    let preserved_mirror_name = preserved
        .iter()
        .find(|n| n.contains("PRE-RESTORE-") && n.ends_with(".audit.log"))
        .unwrap_or_else(|| {
            panic!(
                "ADR-0116 D3.4 rev 2 — the pre-rollback .audit.log mirror must move INTO the \
                 PRE-RESTORE unit, named so `mirror_path_for(preserved_db)` finds it. Leaving \
                 it at the live path is what made `aberp serve` refuse to boot after a \
                 backwards restore. Found: {preserved:?}"
            )
        });
    let preserved_mirror = t.home().join(preserved_mirror_name);
    assert_eq!(
        aberp_audit_ledger::mirror_path_for(&preserved_db),
        preserved_mirror,
        "the preserved mirror must PAIR with the preserved DB by name — the same rule the \
         preserved WAL follows"
    );
    // Byte-identity would be the WRONG assertion, and getting it wrong is
    // instructive: the mandatory pre-restore snapshot (step 2) legitimately
    // appends its own `snapshot.created` row and reconciles the mirror BEFORE
    // the preserve moves it. So the invariant is APPEND-ONLY — the preserved
    // mirror EXTENDS what was there, it was never truncated or rewritten. The
    // mirror is half the audit ledger; a rewritten mirror is a forked chain.
    let preserved_mirror_bytes = std::fs::read(&preserved_mirror).unwrap();
    assert!(
        preserved_mirror_bytes.starts_with(&mirror_before),
        "the preserved mirror must EXTEND the pre-rollback bytes — it is the only surviving \
         record of the audit tail this rollback discarded, and the preserve MOVES it, never \
         rewrites it"
    );
    assert!(
        preserved_mirror_bytes.len() >= mirror_before.len(),
        "the preserved mirror was truncated by the restore"
    );
    assert!(
        aberp_snapshot::is_protected_evidence(&preserved_mirror),
        "ADR-0116 D2 — the preserved mirror is recovery evidence and must be protected from \
         every future cleanup helper"
    );

    // …and a FRESH mirror now describes the RESTORED chain.
    assert!(
        mirror.exists(),
        "ADR-0116 D3.4 rev 2 — a fresh .audit.log must be written for the restored chain \
         inside the same operation. Leaving the live path with no mirror would boot (the boot \
         path creates one), but it would leave a window with a database and no durable record \
         of its chain."
    );
    let mirror_after = std::fs::read(&mirror).unwrap();
    assert!(
        mirror_after.len() < preserved_mirror_bytes.len(),
        "the fresh mirror must describe the RESTORED (shorter) chain, not carry the discarded \
         tail: fresh={} bytes, preserved={} bytes",
        mirror_after.len(),
        preserved_mirror_bytes.len()
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
    // The fresh mirror was written BEFORE the step-7 `SnapshotRestored`
    // append, so it does not carry that row — the DB is one entry ahead of the
    // mirror, which is the `RecoveryAction::Extended` case the boot path
    // resolves without a murmur. (`Ledger::append` commits without syncing the
    // mirror; that is the pre-existing D-22 shape, not something this restore
    // introduces.) What matters is that the mirror AGREES with the restored
    // chain over their shared prefix, which the boot journey below proves.
    assert!(
        !String::from_utf8_lossy(&mirror_after).contains("snapshot.restored"),
        "ADR-0116 D3.5 — the fresh mirror is written from the restored chain BEFORE the \
         restore row is appended; a mirror carrying it would mean a reconcile ran after the \
         install."
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

/// **THE BLOCKER GATE — ADR-0116 D3.4 rev 2.**
///
/// A backwards in-place restore must produce a database `aberp serve` can
/// BOOT. This is the step the rest of this file stopped short of, and the step
/// an operator cannot skip: they roll back at 02:00 and then start the
/// product.
///
/// Before rev 2 the journey below failed at the last assertion. `restore
/// --in-place --accept-data-loss` rolled the live database back exactly as
/// designed and left the `.audit.log` mirror at the live path holding the
/// discarded tail; the step-7 `SnapshotRestored` row then landed at a seq the
/// mirror already held with a different entry;
/// `ensure_consistent_with_db` reported `MirrorDivergedFromDb`;
/// `serve.rs::boot_mirror_route` classified that `RefuseFatal`; and **`aberp
/// serve` did not start**. The only way out was the hand-reconciliation D3.4
/// exists to eliminate — and nothing in the command's output said so.
///
/// The journey is walked twice over, deliberately:
///
///   1. through the shipped binary (`aberp serve --boot-check`), which runs
///      the real `serve::run` boot up to and including the mirror reconcile
///      and its routing decision, and
///   2. directly against `ensure_consistent_with_db` — the exact call
///      `serve.rs` makes at every boot — so the assertion does not rest on one
///      flag's wiring being right.
#[test]
fn journey_backwards_in_place_restore_leaves_a_database_serve_can_boot() {
    let t = Tmp::new("boot-after-rollback");
    let db = t.db();
    let store = t.store();
    let (db_s, store_s) = (db.to_str().unwrap(), store.to_str().unwrap());

    // ── the rollback point ──────────────────────────────────────────────
    seed(&db, 2, 3);
    assert!(
        aberp(
            &t,
            &["snapshot", "now", "--db", db_s, "--tenant", TENANT, "--store", store_s]
        )
        .ok,
        "fixture: the snapshot must succeed"
    );

    // ── the live DB moves AHEAD of it — this is what makes the restore
    //    BACKWARDS, and it is the only case --accept-data-loss exists for ──
    seed(&db, 4, 7);
    let audit_before = audit_count(&db);
    assert!(
        audit_before > 3,
        "fixture: the live chain must be ahead of the snapshot ({audit_before} rows)"
    );

    // A boot BEFORE the restore must be clean, so a failure after it cannot be
    // blamed on the fixture.
    let r = aberp(
        &t,
        &["serve", "--db", db_s, "--tenant", TENANT, "--boot-check"],
    );
    assert!(
        r.ok,
        "fixture: `aberp serve --boot-check` must pass BEFORE the restore.\nstdout: {}\nstderr: {}",
        r.stdout, r.stderr
    );

    let r = aberp(
        &t,
        &[
            "snapshot", "list", "--tenant", TENANT, "--store", store_s, "--json",
        ],
    );
    let listed: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    let id = listed["snapshots"][0]["id"].as_str().unwrap().to_string();

    // ── the acknowledged backwards rollback ─────────────────────────────
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
        "the backwards in-place restore failed.\nstdout: {}\nstderr: {}",
        r.stdout, r.stderr
    );
    assert_eq!(
        invoice_count(&db),
        2,
        "the rollback must actually roll back — otherwise the boot below proves nothing"
    );

    // ADR-0116 D3.3 — the discarded count is on STDOUT, not only in a log
    // line. The durable record of "I threw away N committed audit entries"
    // must not be shell history.
    assert!(
        r.stdout.contains("discarded") && r.stdout.contains("committed audit entries DISCARDED"),
        "ADR-0116 D3.3 — the completion message must name how many committed audit entries \
         this restore threw away: {}",
        r.stdout
    );
    // …and the D3.3 acknowledgement WARN still fires. It is what makes
    // `--accept-data-loss` a decision rather than a switch, and it is the
    // condition Ervin attached to keeping the flag.
    assert!(
        r.stderr.contains("--accept-data-loss was passed")
            && r.stderr.contains("DISCARD committed audit entries"),
        "ADR-0116 D3.3 — passing --accept-data-loss must emit the loud acknowledgement \
         warning naming the live head, the snapshot's count and the difference: {}",
        r.stderr
    );

    // ══ THE GATE ══════════════════════════════════════════════════════════
    //
    // (1) through the shipped binary.
    let r = aberp(
        &t,
        &["serve", "--db", db_s, "--tenant", TENANT, "--boot-check"],
    );
    assert!(
        r.ok,
        "ADR-0116 D3.4 — **`aberp serve` cannot boot after a backwards in-place restore.** A \
         restore that produces a database the server refuses to start on is a total failure of \
         the feature: the operator is left with the product down and the hand-reconciliation \
         this command exists to eliminate.\nstdout: {}\nstderr: {}",
        r.stdout, r.stderr
    );
    assert!(
        r.stdout.contains("boot-check: PASSED"),
        "the boot-check must state its verdict plainly: {}",
        r.stdout
    );

    // (2) directly against the call `serve.rs` makes at boot, so the gate does
    //     not rest on one flag's wiring.
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch("PRAGMA disable_checkpoint_on_shutdown;")
            .unwrap();
        aberp_audit_ledger::ensure_schema(&conn).unwrap();
        let action = aberp_audit_ledger::ensure_consistent_with_db(
            &conn,
            &aberp_audit_ledger::mirror_path_for(&db),
        )
        .expect(
            "ADR-0116 D3.4 — the boot mirror reconcile must SUCCEED after a backwards restore. \
             A MirrorDivergedFromDb here is routed to RefuseFatal and boot stops.",
        );
        // Extended (the mirror was written before the restore row) or
        // Unchanged are both healthy; a divergence is an `Err` and would have
        // panicked above.
        assert!(
            matches!(
                action,
                aberp_audit_ledger::RecoveryAction::Extended { .. }
                    | aberp_audit_ledger::RecoveryAction::Unchanged
                    | aberp_audit_ledger::RecoveryAction::Created { .. }
            ),
            "unexpected boot reconcile action after a backwards restore: {action:?}"
        );
    }

    // ── and the restored chain still verifies FROM GENESIS ──────────────
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch("PRAGMA disable_checkpoint_on_shutdown;")
            .unwrap();
        let ledger = aberp_audit_ledger::Ledger::from_connection(
            conn,
            TenantId::new(TENANT.to_string()).unwrap(),
            BinaryHash::from_bytes([0u8; 32]),
        );
        let len = ledger
            .verify_chain()
            .expect("the restored chain must verify from genesis after the rollback");
        assert!(len >= 4, "the restored chain is implausibly short: {len}");
    }

    // ── the rollback SURVIVED the boot ──────────────────────────────────
    //
    // The other half of the fork the review found: if the restored chain is a
    // clean PREFIX of a mirror left at the live path, boot routes to
    // AutoRecover instead, rebuilds from the mandatory pre-restore snapshot —
    // which is by construction the state the operator rolled back FROM — and
    // silently UNDOES the rollback while filing the restored database away as
    // `.CORRUPT-`. Refusing to boot and silently reverting were the only two
    // outcomes available; neither is the contract.
    assert_eq!(
        invoice_count(&db),
        2,
        "ADR-0116 D3.4 — the boot UNDID the rollback. A restore the next boot reverts is not a \
         restore."
    );
    assert!(
        !std::fs::read_dir(t.home())
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().contains("CORRUPT")),
        "ADR-0116 D3.4 — the boot filed the deliberately-restored database away as CORRUPT \
         evidence and rebuilt over it"
    );

    // ── D4 / the drift condition — the restored chain carries the anchor
    //    verdict it was restored UNDER ─────────────────────────────────────
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch("PRAGMA disable_checkpoint_on_shutdown;")
            .unwrap();
        let raw: Vec<u8> = conn
            .query_row(
                "SELECT payload FROM audit_ledger WHERE kind = 'snapshot.restored' ORDER BY \
                 seq DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .expect("the restore row must be on the restored chain");
        let payload = String::from_utf8_lossy(&raw).into_owned();
        let v: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert!(
            v["anchor_verdict"].is_string(),
            "ADR-0116 D4 — the SnapshotRestored payload must record the anchor verdict, so the \
             restored database itself carries what coverage it was restored under. Today that \
             fact lives only in a stderr warning nobody keeps, and it is the fact a court \
             would ask about. Payload: {payload}"
        );
        assert_eq!(
            v["anchor_verdict"], "no-anchors-at-all",
            "this fixture has no anchors at all; the verdict must say so, and must never say \
             'not-recorded' (checked-and-none is not the same statement as never-checked)"
        );
        assert!(
            v["discarded_audit_rows"].as_u64().unwrap_or(0) > 0,
            "ADR-0116 D3.3 — the restored chain must record how much committed audit history \
             this restore discarded: {payload}"
        );
    }
}

/// **F4 — an unreadable live audit table must report UNKNOWN and REFUSE, never
/// print `EXACT … 0`.**
///
/// `ensure_serve_is_stopped` recorded `-1` for "could not read"
/// (`unwrap_or(-1)`) and `build_preflight` coerced it with `.max(0) as u64`, so
/// `-1` became a confident **zero** in the refusal arithmetic:
///
/// ```text
///   live delta  EXACT (serve is stopped): the live DB holds 0 audit entries
///               and -1 invoices; this snapshot carries 5 / 3
///               → nothing newer than the snapshot would be lost
///   VERDICT: would PROCEED.
/// ```
///
/// The `-1` leaked visibly for invoices while being silently swallowed for the
/// audit count — the one number the gate keys on. That is the same sentinel
/// mistake the ADR is careful to avoid for `anchor_count` (*"`-1` means not
/// recorded, NEVER zero"*), made in the place where it DISARMS the safety, and
/// in exactly the scenario a restore exists for: a database whose tables
/// cannot be read.
///
/// The mirror must not paper over it either. The mirror is a LOWER bound, so
/// falling back to it here would report "nothing newer" about a database whose
/// rows cannot be counted at all.
#[test]
fn journey_an_unreadable_live_audit_table_is_unknown_and_refuses() {
    let t = Tmp::new("f4-unknown");
    let db = t.db();
    let store = t.store();
    let (db_s, store_s) = (db.to_str().unwrap(), store.to_str().unwrap());

    seed(&db, 3, 5);
    assert!(
        aberp(
            &t,
            &["snapshot", "now", "--db", db_s, "--tenant", TENANT, "--store", store_s]
        )
        .ok,
        "fixture: the snapshot must succeed"
    );
    // The mirror is left in place holding 6 entries, deliberately: its LOWER
    // bound must not be used to answer a question it cannot answer.
    assert!(aberp_audit_ledger::mirror_path_for(&db).exists());

    // Now make the live audit table unreadable while the file still opens
    // exclusively — the shape a partially-corrupt database takes.
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch("PRAGMA disable_checkpoint_on_shutdown;")
            .unwrap();
        conn.execute_batch("DROP TABLE audit_ledger;").unwrap();
    }

    let r = aberp(
        &t,
        &[
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
        ],
    );
    assert!(
        r.stdout.contains("live delta       UNKNOWN"),
        "ADR-0116 F4 — the pre-flight must report UNKNOWN when the live audit table cannot be \
         read. Printing `EXACT … 0` is how the data-loss gate disarmed itself in the one \
         scenario a restore is for.\nstdout: {}\nstderr: {}",
        r.stdout,
        r.stderr
    );
    assert!(
        !r.stdout
            .contains("nothing newer than the snapshot would be lost"),
        "ADR-0116 F4 — the pre-flight claimed nothing would be lost about a database whose \
         rows it could not count: {}",
        r.stdout
    );
    assert!(
        r.stdout.contains("VERDICT: would REFUSE"),
        "ADR-0116 F4/D3.3 — an UNKNOWN delta must REFUSE without --accept-data-loss: {}",
        r.stdout
    );

    // …and the real command refuses too, not just the dry-run.
    let r = aberp(
        &t,
        &[
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
            "--confirm",
        ],
    );
    assert!(
        !r.ok,
        "ADR-0116 F4 — the restore PROCEEDED with an unmeasurable data loss.\nstdout: {}",
        r.stdout
    );
    assert!(
        r.stderr.contains("UNKNOWN") || r.stdout.contains("UNKNOWN"),
        "the refusal must say WHY: {}\n{}",
        r.stdout,
        r.stderr
    );
}

/// **F5 — the data-loss gate must compare against the LIVE re-validation, not
/// the recorded `meta.audit_count`.**
///
/// `build_preflight` re-runs validation live and says why: *"never trust the
/// recorded verdict for a decision this destructive."* Two lines later the
/// comparison used `record.meta.audit_count` — the recorded number — while
/// `pf.live.audit_count` was sitting right there.
///
/// `meta.json` is a plain file beside the export with no integrity binding to
/// it. Inflating its `audit_count` (export bytes untouched, so the live
/// re-validation still reports the true count) let an UNACKNOWLEDGED rollback
/// proceed and discard committed audit rows.
#[test]
fn journey_a_tampered_meta_json_cannot_disarm_the_data_loss_gate() {
    let t = Tmp::new("f5-tampered");
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
    // Move the live DB AHEAD of the snapshot, so a restore is backwards and the
    // D3.3 gate is the only thing standing between the operator and the loss.
    seed(&db, 0, 7);
    let live_audit = audit_count(&db);
    assert!(live_audit > 4, "fixture: the live chain must be ahead");

    // Baseline: the gate refuses, naming the flag.
    let r = aberp(
        &t,
        &[
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
            "--confirm",
        ],
    );
    assert!(!r.ok, "fixture: the unacknowledged rollback must refuse");
    assert!(
        r.stderr.contains("--accept-data-loss"),
        "fixture: the refusal must name the flag: {}",
        r.stderr
    );

    // Inflate the RECORDED count. The export bytes are untouched, so the live
    // re-validation still sees the true 3 — the two numbers now disagree, and
    // which one the gate reads is the whole finding.
    let snap_dir = std::fs::read_dir(&store)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.is_dir())
        .expect("the snapshot directory must exist");
    let meta_path = snap_dir.join("meta.json");
    let mut meta: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&meta_path).unwrap()).unwrap();
    meta["audit_count"] = serde_json::json!(999_999i64);
    std::fs::write(&meta_path, serde_json::to_vec_pretty(&meta).unwrap()).unwrap();

    let r = aberp(
        &t,
        &[
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
            "--confirm",
        ],
    );
    assert!(
        !r.ok,
        "ADR-0116 F5 — a tampered/stale `meta.json` disarmed the data-loss gate and an \
         UNACKNOWLEDGED rollback discarded {} committed audit rows. `meta.json` is evidence, \
         not authority: the gate must key on the LIVE re-validation it already runs.\nstdout: \
         {}\nstderr: {}",
        live_audit - 3,
        r.stdout,
        r.stderr
    );
    assert!(
        r.stderr.contains("--accept-data-loss"),
        "the refusal must still name the acknowledgement flag: {}",
        r.stderr
    );
    assert_eq!(
        audit_count(&db),
        live_audit,
        "ADR-0116 F5 — the live database was rolled back by a restore that should have refused"
    );
}

/// **F5's class, in the ADR-0116 D4 anchor sanction** — found while applying
/// Ervin's KEEP ruling on drift (b), not named by the review.
///
/// The Defense sanction read `meta.anchor_count` / `meta.anchored_through_seq`
/// off `meta.json`, which is a plain file beside the export with no integrity
/// binding to it. That is F5's exact shape one function over: the recorded
/// number is EVIDENCE, not authority.
///
/// The verdict now comes from the LIVE re-validation, and the pre-flight prints
/// BOTH when they disagree — because a disagreement between what a snapshot
/// records about itself and what its bytes actually say is a finding in its own
/// right, not a detail to reconcile silently.
#[test]
fn journey_the_anchor_verdict_comes_from_the_live_revalidation_not_meta_json() {
    let t = Tmp::new("d4-live-anchors");
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

    // Claim, in `meta.json` only, that this snapshot is fully anchored. The
    // export bytes are untouched, so the live re-validation still finds the
    // true zero anchors.
    let snap_dir = std::fs::read_dir(&store)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.is_dir())
        .expect("the snapshot directory must exist");
    let meta_path = snap_dir.join("meta.json");
    let mut meta: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&meta_path).unwrap()).unwrap();
    meta["anchor_count"] = serde_json::json!(42i64);
    meta["anchored_through_seq"] = serde_json::json!(9_999u64);
    std::fs::write(&meta_path, serde_json::to_vec_pretty(&meta).unwrap()).unwrap();

    let r = aberp(
        &t,
        &[
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
        ],
    );
    assert!(
        r.stdout.contains("they DISAGREE"),
        "ADR-0116 D4 — the pre-flight must show the LIVE coverage and say so when `meta.json` \
         records something else. A snapshot that misdescribes itself is a finding, not a \
         detail to reconcile silently.\nstdout: {}\nstderr: {}",
        r.stdout,
        r.stderr
    );
    assert!(
        r.stdout.contains("0 rows, NONE verified"),
        "the LIVE coverage (zero anchors) must be what is shown and what gates: {}",
        r.stdout
    );

    // …and the row on the restored chain records the LIVE verdict, so the
    // database cannot end up claiming a coverage its own bytes never had.
    let r = aberp(
        &t,
        &[
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
            "--confirm",
            // The `snapshot now` above appended its own `snapshot.created`
            // row, so the live chain is one entry ahead of the snapshot and
            // the D3.3 gate correctly demands the acknowledgement.
            "--accept-data-loss",
        ],
    );
    assert!(r.ok, "the restore must proceed: {}\n{}", r.stdout, r.stderr);
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch("PRAGMA disable_checkpoint_on_shutdown;")
        .unwrap();
    let raw: Vec<u8> = conn
        .query_row(
            "SELECT payload FROM audit_ledger WHERE kind = 'snapshot.restored' ORDER BY seq \
             DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(
        v["anchor_verdict"], "no-anchors-at-all",
        "ADR-0116 D4 — the restored chain recorded a coverage claim taken from `meta.json`. \
         That is the fact a court would read, and it must come from the bytes: {v}"
    );
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
