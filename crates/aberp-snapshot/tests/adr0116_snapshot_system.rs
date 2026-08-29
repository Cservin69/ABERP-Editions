//! ADR-0116 acceptance criteria — the crate-layer half.
//!
//! One test (or one pair) per acceptance criterion, named so a reviewer can
//! map a red test to the criterion it violates. The app-layer criteria (the
//! daemon cadence, the CLI pre-flight, the customer journey) live in
//! `apps/aberp/tests/adr0116_restore_journey_e2e.rs`.

use std::path::{Path, PathBuf};

use aberp_audit_ledger::{Actor, BinaryHash, EventKind, Ledger, TenantId};
use aberp_snapshot::{
    guarded_remove, is_protected_evidence, list_evidence, list_snapshots, plan_evidence_release,
    plan_retention, prune, restore_in_place, restore_into, snapshot_identity, take_snapshot,
    validate_export, EvidencePolicy, RetainReason, RetentionPolicy, SnapshotMeta, SnapshotRecord,
};
use duckdb::Connection;
use time::macros::datetime;
use time::OffsetDateTime;

// ──────────────────────────────────────────────────────────────────────
// Scaffolding (mirrors snapshot_tests.rs — no tempfile dev-dep)
// ──────────────────────────────────────────────────────────────────────

struct ScopedTempDir(PathBuf);

impl ScopedTempDir {
    fn new(label: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "aberp-adr0116-{label}-{}-{nanos}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create scoped tempdir");
        Self(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScopedTempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn seed_db(path: &Path, tenant: &str, n_invoice: usize, n_audit: usize) {
    {
        let conn = Connection::open(path).expect("open db for invoice seed");
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS invoice (id BIGINT, amount DOUBLE, note VARCHAR);",
        )
        .expect("create invoice");
        for i in 0..n_invoice {
            conn.execute(
                "INSERT INTO invoice VALUES (?, ?, ?)",
                duckdb::params![i as i64, (i as f64) * 10.0, format!("inv-{i}")],
            )
            .expect("insert invoice");
        }
    }
    let tid = TenantId::new(tenant.to_string()).expect("tenant");
    let mut ledger =
        Ledger::open(path, tid, BinaryHash::from_bytes([1u8; 32])).expect("open ledger");
    for i in 0..n_audit {
        ledger
            .append(
                EventKind::Test,
                format!("{{\"i\":{i}}}").into_bytes(),
                Actor::test_only(),
                None,
            )
            .expect("append audit entry");
    }
}

fn record(seq: u64, created_at: OffsetDateTime, valid: bool) -> SnapshotRecord {
    SnapshotRecord {
        dir: PathBuf::from(format!("/nonexistent/snap-{seq}-20260101-000000")),
        meta: SnapshotMeta {
            meta_version: 2,
            seq,
            created_at,
            source_db_sha256: format!("{seq:064x}"),
            byte_size: 100,
            valid,
            invoice_count: 1,
            audit_count: 1,
            chain_len: 1,
            validation_error: None,
            anchor_count: -1,
            anchored_through_seq: None,
        },
    }
}

fn wal_of(db: &Path) -> PathBuf {
    let mut os = db.as_os_str().to_owned();
    os.push(".wal");
    PathBuf::from(os)
}

// ══════════════════════════════════════════════════════════════════════
// AC-1 (F2) — the restore install path is DURABLE, asserted by MUTATION
// ══════════════════════════════════════════════════════════════════════

/// **ADR-0116 AC-1, and read the reasoning before changing this test.**
///
/// The ADR's ORIGINAL acceptance criterion — "kill the process between import
/// and rename, assert the target is old-or-new, never torn" — is **vacuous**.
/// `rename(2)` is atomic in the page cache, so `kill -9` / `panic!` /
/// `abort()` cannot lose it. That assertion passed BEFORE the fix, with
/// **zero** fsyncs, and would pass again after it: it tests rename atomicity
/// (never in doubt) instead of the fsyncs (the entire point). This project has
/// been burned by that exact vacuity twice already — the debounce-shadow
/// power-loss test, and the `durable_ack` deletion no test could see.
///
/// So AC-1 is restated as a **mutation assertion**: deleting `fsync_file` or
/// `fsync_dir` from the restore install path must turn a named test red. Two
/// halves, both required:
///
///  1. a CODE-LEVEL assertion that `restore_into` commits through
///     `crash_safe::atomic_install` — the primitive that carries the fsyncs —
///     rather than a bare `std::fs::rename`; and
///  2. a BEHAVIOURAL assertion of the ordering guarantee that the fsyncs
///     exist to protect (AC-2 below).
///
/// **Honest scope, stated because the ADR demands it:** genuine crash
/// injection needs a filesystem-level harness this project does not have
/// (`powercut`-style block-device fault injection). What is asserted here is
/// that the durable primitive is on the path and that the ordering around it
/// is correct. Do not substitute a `kill -9` test a reviewer will tick while
/// the defect is still present.
#[test]
fn ac1_restore_install_goes_through_the_durable_primitive_not_a_bare_rename() {
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/take.rs"))
        .expect("read take.rs");
    // Isolate `restore_into`'s body: from its signature to the next
    // top-level item.
    let start = src
        .find("pub fn restore_into(")
        .expect("restore_into must exist");
    let rest = &src[start..];
    let end = rest
        .find("\n/// What [`restore_in_place`]")
        .unwrap_or(rest.len());
    let body = &rest[..end];

    assert!(
        body.contains("crash_safe::atomic_install"),
        "ADR-0116 AC-1 — restore_into must commit through crash_safe::atomic_install, the \
         primitive that carries fsync_file -> rename -> fsync_dir. Before ADR-0116 this was \
         the ONE file-install path in the tree with no fsync at all (grep -c sync_all take.rs \
         -> 0), and it had already been used for two real incident recoveries."
    );
    assert!(
        !body.contains("std::fs::rename(&staging, target)"),
        "ADR-0116 AC-1 — restore_into must NOT commit with a bare rename; that is exactly the \
         pre-ADR-0116 defect."
    );

    // The primitive itself must still carry both fsyncs. Deleting either one
    // from crash_safe.rs turns THIS test red — which is the mutation
    // assertion AC-1 asks for, since a deleted fsync is otherwise invisible.
    let cs = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/crash_safe.rs"))
        .expect("read crash_safe.rs");
    let ai_start = cs
        .find("pub fn atomic_install(")
        .expect("atomic_install must exist");
    let ai = &cs[ai_start..];
    let ai_end = ai.find("\n/// Write (and `fsync`)").unwrap_or(ai.len());
    let ai_body = &ai[..ai_end];
    assert!(
        ai_body.contains("fsync_file(staged)?"),
        "ADR-0116 AC-1 (MUTATION) — atomic_install lost its fsync of the staged file. The \
         restored bytes would then be in the page cache only, and a power cut just after the \
         rename leaves a target that names blocks never written."
    );
    assert!(
        ai_body.contains("fsync_dir(parent)?"),
        "ADR-0116 AC-1 (MUTATION) — atomic_install lost its fsync of the PARENT DIRECTORY. The \
         rename itself would then be unpersisted, so a power cut can leave the OLD target \
         (or, on some filesystems, no entry at all)."
    );
}

// ══════════════════════════════════════════════════════════════════════
// AC-2 (D3.1 / F1) — the target's WAL is unlinked BEFORE the rename
// ══════════════════════════════════════════════════════════════════════

/// **ADR-0116 AC-2** — the window F1 found, closed without a journal.
///
/// `atomic_install` drops the target's stale WAL *after* the rename:
///
/// ```text
///   fsync(staged) -> rename(staged -> target) -> remove target.wal -> fsync(dir)
///                                             ^ crash HERE: new file + OLD WAL
/// ```
///
/// and `restore_into`'s own comment says a surviving old WAL "would corrupt it
/// on next open". The ADR's first draft claimed the `install-intent` journal
/// closed that window. **It does not**: `write_install_intent` has exactly one
/// non-test caller (inside `durable_checkpoint`) and `resume_pending_install`
/// is keyed on the LIVE db path, so nothing would ever resume an intent left
/// beside a side-path restore target.
///
/// The fix is to delete the WAL FIRST, which makes `atomic_install`'s own step
/// a proven no-op. Asserted two ways: the installed file has no WAL beside it,
/// and the source ordering is pinned so a refactor cannot silently move the
/// unlink back after the rename.
#[test]
fn ac2_target_wal_is_unlinked_before_the_rename() {
    let tmp = ScopedTempDir::new("ac2-wal-first");
    let src_db = tmp.path().join("source.duckdb");
    seed_db(&src_db, "t", 3, 3);
    let store = tmp.path().join("store");
    let rec = take_snapshot(&src_db, &store, "t", OffsetDateTime::now_utc()).expect("snapshot");

    // A target that already exists AND carries a stale WAL — prod's live DB
    // carries one right now, so this is the real case, not a contrived one.
    let target = tmp.path().join("target.duckdb");
    seed_db(&target, "t", 1, 1);
    let target_wal = wal_of(&target);
    std::fs::write(
        &target_wal,
        b"stale WAL bytes that would corrupt the restored file",
    )
    .expect("plant stale WAL");
    assert!(
        target_wal.exists(),
        "fixture: the stale WAL must be present"
    );

    restore_into(&rec.dir, &target, "t").expect("restore");

    assert!(
        !target_wal.exists(),
        "ADR-0116 AC-2 — a stale WAL survived beside the restored file. On next open DuckDB \
         would replay it into a database it has nothing to do with: the exact corruption \
         vector restore_into's own comment warns about."
    );
    assert!(target.exists(), "the restored file must be installed");

    // Ordering, pinned in source: the unlink must precede the install call.
    let s = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/take.rs"))
        .expect("read take.rs");
    let body_start = s.find("pub fn restore_into(").expect("restore_into");
    let body = &s[body_start..];
    let unlink = body
        .find("std::fs::remove_file(&target_wal)")
        .expect("ADR-0116 AC-2 — restore_into no longer unlinks the target's WAL at all");
    let install = body
        .find("crash_safe::atomic_install(&staging, target)")
        .expect("ADR-0116 AC-2 — restore_into no longer installs via atomic_install");
    assert!(
        unlink < install,
        "ADR-0116 AC-2 — the target's WAL is unlinked AFTER the install. That reopens the \
         crash window F1 found (new file visible beside the old WAL), and no journal closes \
         it here: resume_pending_install is keyed on the LIVE db path and would never resume \
         an intent left beside a side-path restore target."
    );
}

// ══════════════════════════════════════════════════════════════════════
// AC-6 (D2 / F3) — is_protected_evidence against the REAL on-disk names
// ══════════════════════════════════════════════════════════════════════

/// Every evidence-shaped name observed in the live tenant homes, fixtured
/// **verbatim**. The ISO-tagged families and the `_evidence-`/`_recovery-`
/// directories were enumerated from `~/.aberp-defense/*/` (read-only listing,
/// names only). The `healed-*.bak` and `INDEXDESYNC-BACKUP` families are
/// quoted from ADR-0116 G5, which measured them in `~/.aberp/prod/` — the
/// FROZEN prod line this tree must never touch.
///
/// **These 14 are the ones that escaped even a case-INSENSITIVE deny-list**
/// and are the reason the guard is inverted to an allow-list: `healed-` and
/// `INDEXDESYNC-` match none of the original five patterns in any case.
const REAL_EVIDENCE_NAMES: &[&str] = &[
    // ── the 14 that escaped even case-insensitively (ADR-0116 F3) ──
    "healed-20260803T120000Z.bak",
    "aberp.duckdb.audit.log.healed-1783315209649645000.bak",
    "aberp.duckdb.INDEXDESYNC-BACKUP-20260803",
    "aberp.duckdb.INDEXDESYNC-BACKUP-20260803.wal",
    "_evidence-20260627",
    "aberp.duckdb.wal.SPURIOUS-POST-DEDUP-20260706T062631Z",
    "aberp.duckdb.wal.SPURIOUS-POST-DEFORK-20260706T083842Z",
    // ── ISO-tagged .CORRUPT- family ──
    "aberp.duckdb.CORRUPT-20260705T184449Z",
    "aberp.duckdb.CORRUPT-20260706T061940Z",
    "aberp.duckdb.CORRUPT-99234-1787209326689433000",
    "aberp.duckdb.CORRUPT-BACKUP-20260629T140040Z",
    "aberp.duckdb.wal.CORRUPT-20260705T184449Z",
    // ── the 22 lowercase nanosecond-tagged mirror backups ──
    "aberp.duckdb.audit.log.corrupt-1783315209649645000.bak",
    "aberp.duckdb.audit.log.corrupt-1787552871196106000.bak",
    // ── PRE-* families ──
    "aberp.duckdb.PRE-DEDUP-20260705T085404Z",
    "aberp.duckdb.audit.log.PRE-DEFORK-20260824T074043Z.bak",
    "aberp.duckdb.audit.log.PRE-MIRROR-REBUILD-20260820T070244Z",
    "aberp.duckdb.audit.log.PRE-RECONCILE-20260708T235509Z",
    "aberp.duckdb.audit.log.PRE-TOPUP-20260708T151430Z",
    "aberp.duckdb.audit.log.pre-recovery-20260629T140040Z",
    // ── RECOVERY / DEFORK / AHEAD / DEEPCORRUPT ──
    "RECOVERY-EVIDENCE-20260705T184449Z",
    "_RECOVERY-DEFORK-20260705T184449Z.md",
    "_RECOVERY-STOP-20260707T233718Z.md",
    "_recovery-20260629",
    "aberp.duckdb.audit.log.AHEAD-manual-20260810T172538Z",
    "aberp.duckdb.audit.log.DEEPCORRUPT-manual-20260810T165806Z",
    // ── this ADR's own artefact ──
    "aberp.duckdb.PRE-RESTORE-20260829T101500Z",
    "aberp.duckdb.wal.PRE-RESTORE-20260829T101500Z",
    "aberp.duckdb.ckpt-ok.PRE-RESTORE-20260829T101500Z",
    // ── the credential dumps outside the tenant homes ──
    "prod-20260811-keychain.zip",
];

/// The LIVE set — these must NOT be protected, or the guard freezes the
/// working tenant home and every cleanup helper in the tree starts refusing.
const REAL_LIVE_NAMES: &[&str] = &[
    "aberp.duckdb",
    "aberp.duckdb.wal",
    "aberp.duckdb.audit.log",
    "aberp.duckdb.ckpt-ok",
    "seller.toml",
    "logo.png",
    "runtime.json",
];

#[test]
fn ac6_is_protected_evidence_covers_every_real_on_disk_name() {
    let home = Path::new("/Users/someone/.aberp-defense/defense");
    for name in REAL_EVIDENCE_NAMES {
        assert!(
            is_protected_evidence(&home.join(name)),
            "ADR-0116 AC-6 — {name:?} is recovery evidence and the guard did not protect it. \
             The first-draft deny-list missed 58 of 101 such names case-sensitively and 14 \
             even case-insensitively; this is permanent data loss, in the one place where it \
             is unrecoverable."
        );
    }
    // Case-insensitivity is the property, not an accident of these fixtures.
    for name in REAL_EVIDENCE_NAMES {
        let upper = name.to_uppercase();
        let lower = name.to_lowercase();
        assert!(
            is_protected_evidence(&home.join(&upper)) && is_protected_evidence(&home.join(&lower)),
            "ADR-0116 AC-6 — {name:?} escapes the guard in some CASE. This exact bug class was \
             closed once already in this repo's edition DB-guard, which needed both walks made \
             case-insensitive."
        );
    }
}

#[test]
fn ac6_live_tenant_files_are_not_treated_as_evidence() {
    let home = Path::new("/Users/someone/.aberp-defense/defense");
    for name in REAL_LIVE_NAMES {
        assert!(
            !is_protected_evidence(&home.join(name)),
            "ADR-0116 AC-6 — the live file {name:?} was classified as protected evidence. The \
             guard's safe direction is to over-protect, but freezing the LIVE set breaks every \
             cleanup helper in the tree (including the WAL unlink that makes a restore \
             crash-safe) and the guard would be switched off."
        );
    }
}

#[test]
fn ac6_allow_list_inversion_protects_an_unknown_name_under_a_tenant_home() {
    // The point of the inversion: a name nobody anticipated, matching NO
    // evidence family, is still protected because it is not on the live list.
    let unknown = Path::new("/Users/someone/.aberp-defense/defense/whatever-2027-forensics");
    assert!(
        is_protected_evidence(unknown),
        "ADR-0116 D2.2 — the allow-list inversion is the PRIMARY predicate: under a tenant \
         home, anything that is not a known-live filename is evidence. The evidence set is not \
         enumerable; the live set is."
    );
    // …and the same name OUTSIDE a tenant home is not frozen, so the guard
    // cannot accidentally seize unrelated temp files.
    let elsewhere = Path::new("/tmp/whatever-2027-forensics");
    assert!(
        !is_protected_evidence(elsewhere),
        "the inversion is scoped to the homes/roots where evidence actually lives; outside \
         them the family predicate is the whole rule"
    );
}

#[test]
fn ac6_named_families_are_protected_everywhere_not_only_under_a_tenant_home() {
    for p in [
        "/tmp/scratch/aberp.duckdb.CORRUPT-20260705T184449Z",
        "/Users/x/Documents/ABERP-recovery-20260808/rebuilt.duckdb",
        "/Users/x/aberp-snapshots/prod-20260811-keychain.zip",
    ] {
        assert!(
            is_protected_evidence(Path::new(p)),
            "ADR-0116 D2.4 — {p} must be protected. Governing one third of the footprint while \
             claiming to govern all of it is worse than governing none, because it reads as \
             done: ~271 MB sits OUTSIDE the tenant homes, including the encrypted keychain \
             dumps."
        );
    }
}

/// **The allow-list inversion is scoped to IMMEDIATE children of a tenant
/// directory — and the contents of evidence directories are still protected.**
///
/// Evidence is written as a *sibling of the live DB*; it is never written
/// inside a live working directory. A first cut applied the inversion at any
/// depth, which made every file inside `ap-artifacts/`, `ncr-photos/`,
/// `email-relay-attachments/` and `issued/` "protected evidence" — those
/// directories are allow-listed but their CONTENTS are per-invoice/per-NCR
/// files no list can enumerate. It would have frozen, among others, the
/// incoming-invoice ingest's rollback cleanup of an orphaned artifact file.
/// **A guard that blocks legitimate cleanup is a guard that gets switched
/// off**, which is the worst outcome available here.
///
/// Depth is therefore governed by the family predicate alone — which now also
/// matches on ANCESTOR components, so an evidence directory's contents stay
/// protected. Both halves are asserted, because either one alone is a defect:
/// too broad breaks working code, too narrow loses evidence.
#[test]
fn the_inversion_is_scoped_to_siblings_of_the_live_db() {
    let home = Path::new("/Users/someone/.aberp-defense/defense");

    // ── still protected: an unknown name SIBLING of the live DB ──
    assert!(
        is_protected_evidence(&home.join("whatever-2027-forensics")),
        "the inversion must still cover immediate children — that is where every real \
         evidence artefact on disk lives"
    );

    // ── NOT protected: legitimate contents of live working directories ──
    for p in [
        "ap-artifacts/inv-2026-001.xml",
        "ncr-photos/NCR-17/front.jpg",
        "email-relay-attachments/quote.pdf",
        "issued/INV-001/input.json",
    ] {
        assert!(
            !is_protected_evidence(&home.join(p)),
            "ADR-0116 D2 — {p} is ordinary working data inside an allow-listed live \
             directory, not recovery evidence. Freezing it blocks legitimate cleanup (the \
             incoming-invoice ingest's rollback of an orphaned artifact, for one) and a guard \
             that does that gets switched off."
        );
    }

    // ── still protected: the CONTENTS of an evidence directory ──
    //
    // These file names carry no family token of their own; they are protected
    // by their PARENT. Without the ancestor walk the guard would refuse a
    // `remove_dir_all` of the directory while permitting an equivalent
    // file-by-file walk — a difference no attacker or careless helper should
    // be able to exploit.
    for p in [
        "_evidence-20260627/notes.md",
        "_recovery-20260629/aberp.duckdb",
        "RECOVERY-EVIDENCE-20260705T184449Z/mirror.jsonl",
    ] {
        assert!(
            is_protected_evidence(&home.join(p)),
            "ADR-0116 D2 — {p} sits inside a recovery-evidence DIRECTORY and must be protected \
             by its ancestor, or the guard refuses the directory removal while permitting the \
             file-by-file equivalent"
        );
    }
}

/// **The `.backup-` allow-list entry must not become an evidence escape.**
///
/// `seller_toml_backup::prune_old_backups` enumerates a tenant home and
/// unlinks by prefix — the ADR's exact hazard shape, and the SECOND instance
/// of it in this tree. Routing it through `guarded_remove` required putting
/// `.backup-` on the live allow-list, or the rotation would refuse forever and
/// backups would accumulate in the tenant home.
///
/// That entry is the risky half, so it is pinned from both sides: the seller
/// backup must be removable, and the real `-BACKUP-` evidence families must
/// still be protected. The LEADING DOT is what separates them, and the family
/// predicate runs first regardless.
#[test]
fn seller_toml_backup_is_removable_but_backup_shaped_evidence_is_not() {
    let home = Path::new("/Users/someone/.aberp-defense/defense");
    assert!(
        !is_protected_evidence(&home.join(".seller.toml.backup-1787209326")),
        "ADR-0116 D2 — the seller.toml backup rotation must keep working. A guard that freezes \
         it is a guard an operator switches off, and backups would grow without bound in the \
         tenant home."
    );
    for evidence in [
        "aberp.duckdb.CORRUPT-BACKUP-20260629T140040Z",
        "aberp.duckdb.CORRUPT-BACKUP-20260704T043734Z",
        "aberp.duckdb.INDEXDESYNC-BACKUP-20260803",
        "aberp.duckdb.INDEXDESYNC-BACKUP-20260803.wal",
    ] {
        assert!(
            is_protected_evidence(&home.join(evidence)),
            "ADR-0116 D2 — {evidence} is real recovery evidence and the `.backup-` allow-list \
             entry must not reach it. These spell it `-BACKUP-`, not `.backup-`, AND carry a \
             family token that is matched FIRST — both separations must hold."
        );
        // Case-insensitively too, which is where the first-draft guard failed.
        assert!(is_protected_evidence(&home.join(evidence.to_lowercase())));
        assert!(is_protected_evidence(&home.join(evidence.to_uppercase())));
    }
}

#[test]
fn ac6_guarded_remove_refuses_evidence_and_permits_a_live_transient() {
    let tmp = ScopedTempDir::new("ac6-guarded-remove");
    // Fabricate a tenant-home-shaped path so the inversion applies.
    let home = tmp.path().join(".aberp-defense").join("defense");
    std::fs::create_dir_all(&home).expect("mkdir");

    let evidence = home.join("aberp.duckdb.CORRUPT-20260705T184449Z");
    std::fs::write(&evidence, b"the only record of an incident").expect("write");
    let err = guarded_remove(&evidence).expect_err(
        "ADR-0116 D2 — guarded_remove MUST refuse recovery evidence; a helper that unlinks it \
         destroys the only record of a durability incident",
    );
    assert!(
        format!("{err}").contains("refusing to delete recovery evidence"),
        "the refusal must say what it is refusing and why: {err}"
    );
    assert!(evidence.exists(), "the artefact must still be on disk");

    // A code-owned transient IS removable, or the orphan sweeper the guard now
    // routes through would refuse forever and the tenant home would grow
    // without bound.
    let transient = home.join("aberp.duckdb.creating-1787209326689433000");
    std::fs::write(&transient, b"half-built rebuild").expect("write");
    assert!(
        guarded_remove(&transient).expect("a live transient must be removable"),
        "guarded_remove reported nothing removed for an existing transient"
    );
    assert!(!transient.exists());
}

// ══════════════════════════════════════════════════════════════════════
// AC-7 (G8) — a failed snapshot survives the cycle that created it
// ══════════════════════════════════════════════════════════════════════

/// **ADR-0116 AC-7** — the forensic-retention rule, end to end on disk.
///
/// `run_cycle` takes a snapshot then applies retention in the SAME cycle, and
/// every pre-G8 keep rule considered only valid snapshots — so a snapshot that
/// failed validation was written to disk and deleted milliseconds later. Prod
/// lost real forensic evidence to this twice (2026-07-08 and 07-09), each time
/// destroying the complete logical export of a live audit-chain fork and
/// leaving only an error string in the audit payload.
#[test]
fn ac7_a_failed_snapshot_is_left_on_disk_by_the_cycle_that_created_it() {
    let tmp = ScopedTempDir::new("ac7-forensic");
    let db = tmp.path().join("aberp.duckdb");
    seed_db(&db, "t", 2, 4);
    let store = tmp.path().join("store");

    // A good snapshot first, so the newest-valid floor is not what saves the
    // failed one — otherwise this test would pass for the wrong reason.
    let good = take_snapshot(&db, &store, "t", OffsetDateTime::now_utc()).expect("good snapshot");
    assert!(good.meta.valid);

    // Now break the chain so the next snapshot fails validation, exactly as
    // the 2026-07-08 "out of order: expected seq=7995, found seq=7994" case.
    {
        let conn = Connection::open(&db).expect("open to tamper");
        conn.execute_batch("PRAGMA disable_checkpoint_on_shutdown;")
            .expect("pragma");
        conn.execute("DELETE FROM audit_ledger WHERE seq = 2", [])
            .expect("punch a hole in the chain");
    }
    let bad = take_snapshot(&db, &store, "t", OffsetDateTime::now_utc()).expect("bad snapshot");
    assert!(
        !bad.meta.valid,
        "fixture: the tampered DB must fail validation, or this tests nothing"
    );

    // Retention, in the same cycle.
    let records = list_snapshots(&store).expect("list");
    let plan = plan_retention(
        &records,
        &RetentionPolicy::default(),
        OffsetDateTime::now_utc(),
    );
    prune(&records, &plan).expect("prune");

    assert!(
        bad.dir.exists(),
        "ADR-0116 AC-7 / G8 — the failed snapshot was destroyed by the cycle that created it. \
         An invalid snapshot has no RESTORE value and the highest FORENSIC value the subsystem \
         produces: a complete logical export taken at the instant a defect was detected. Prod \
         has already lost two of these."
    );
    assert!(good.dir.exists(), "the good snapshot must survive too");

    // `list` must be able to report it distinctly — a rollback store whose
    // newest entries are all invalid is an incident, not an inventory.
    let after = list_snapshots(&store).expect("list again");
    assert!(
        after.iter().any(|r| !r.meta.valid),
        "the retained failed snapshot must still be visible to list_snapshots"
    );
}

// ══════════════════════════════════════════════════════════════════════
// AC-8 (D3.3 / G6) — a recycled seq is AMBIGUOUS and must be refused
// ══════════════════════════════════════════════════════════════════════

/// **ADR-0116 AC-8** — `seq` is not a stable identity.
///
/// `next_seq` is `max(surviving seq) + 1`, so pruning recycles a seq: seq 24
/// names three different snapshots in prod's ledger, two of them the
/// `validation_failed` pair. The pre-ADR-0116 `find_snapshot` returned the
/// FIRST seq match, so a `seq`-addressed restore silently chose between a good
/// snapshot and a broken one — and the ambiguity is worst exactly where it
/// hurts most.
#[test]
fn ac8_a_recycled_seq_refuses_and_the_stable_identity_resolves() {
    let base = datetime!(2026-06-15 00:00:00 UTC);
    // Two records that have held the SAME seq — constructible in reality by
    // create, prune, create.
    let mut first = record(24, base, false);
    first.dir = PathBuf::from("/store/snap-24-20260709-142400");
    first.meta.source_db_sha256 =
        "aaaaaaaa1111111111111111111111111111111111111111111111111111".into();
    let mut second = record(24, base + time::Duration::days(1), true);
    second.dir = PathBuf::from("/store/snap-24-20260710-081500");
    second.meta.source_db_sha256 =
        "bbbbbbbb2222222222222222222222222222222222222222222222222222".into();
    let records = vec![first.clone(), second.clone()];

    let err = aberp_snapshot::resolve_selector_in(&records, "24").expect_err(
        "ADR-0116 AC-8 — a bare seq that names two snapshots MUST refuse. Guessing here means \
         silently picking between a validation_failed export and a good rollback point.",
    );
    let msg = format!("{err}");
    assert!(
        msg.contains("AMBIGUOUS"),
        "the refusal must name the ambiguity: {msg}"
    );
    assert!(
        msg.contains(&snapshot_identity(&first.meta))
            && msg.contains(&snapshot_identity(&second.meta)),
        "the refusal must list every candidate by its STABLE identity so the operator can retry \
         with one: {msg}"
    );

    // The stable identity resolves exactly one.
    let got = aberp_snapshot::resolve_selector_in(&records, &snapshot_identity(&second.meta))
        .expect("ADR-0116 AC-8 — the (seq, created_at, source_db_sha256) identity must resolve");
    assert_eq!(got.dir, second.dir);

    // …and so does the full directory name, which is unique on a filesystem.
    let got = aberp_snapshot::resolve_selector_in(&records, "snap-24-20260709-142400")
        .expect("a full directory name must resolve");
    assert_eq!(got.dir, first.dir);
}

// ══════════════════════════════════════════════════════════════════════
// AC-9 (D3.4 / F4) — the .PRE-RESTORE- unit, and the mirror that stays
// ══════════════════════════════════════════════════════════════════════

/// **ADR-0116 AC-9** — the preserved unit is DB + `.wal` + `.ckpt-ok`, the
/// mirror does not move, and no orphan WAL is left beside the restored file.
///
/// The ADR's first draft said only "move the current DB aside". That would
/// have been a real defect twice over: a DB moved without its WAL is stripped
/// of its un-checkpointed commits — not a recoverable original, so this very
/// criterion would have been *unsatisfiable* — and the orphaned
/// `aberp.duckdb.wal` would stay at the live path and pair with the freshly
/// restored file, which is the exact corruption vector `restore_into`'s own
/// comment warns about, reintroduced by the command written to eliminate the
/// hand-swap.
///
/// The recoverability half is asserted the strong way the ADR asks for: the
/// preserved unit is opened and a row that was **in the WAL and not in the DB
/// file** is read back.
#[test]
fn ac9_in_place_restore_preserves_db_wal_and_marker_and_leaves_the_mirror() {
    let tmp = ScopedTempDir::new("ac9-in-place");
    let home = tmp.path().join(".aberp-defense").join("defense");
    std::fs::create_dir_all(&home).expect("mkdir");
    let db = home.join("aberp.duckdb");

    // A snapshot to restore FROM, taken while the DB holds 2 invoices.
    seed_db(&db, "t", 2, 3);
    let store = tmp.path().join("store");
    let rec = take_snapshot(&db, &store, "t", OffsetDateTime::now_utc()).expect("snapshot");
    assert!(rec.meta.valid);

    // Now write a row that lives ONLY in the WAL — no checkpoint after it —
    // so "was the WAL preserved" has a factual answer rather than a
    // file-existence answer.
    {
        let conn = Connection::open(&db).expect("open live");
        conn.execute_batch("PRAGMA disable_checkpoint_on_shutdown;")
            .expect("pragma");
        conn.execute(
            "INSERT INTO invoice VALUES (?, ?, ?)",
            duckdb::params![999i64, 1234.0f64, "only-in-the-wal"],
        )
        .expect("insert");
    }
    let live_wal = wal_of(&db);
    let had_wal = live_wal.exists();

    // A checkpoint marker, as the live path carries.
    aberp_snapshot::write_marker(&db).expect("write marker");
    let marker = aberp_snapshot::marker_path(&db);
    assert!(marker.exists(), "fixture: the marker must exist");

    // The audit mirror at the live path.
    let mirror = aberp_audit_ledger::mirror_path_for(&db);
    std::fs::write(&mirror, b"{\"seq\":1}\n").expect("write mirror");
    let mirror_before = std::fs::read(&mirror).expect("read mirror");

    let report =
        restore_in_place(&rec.dir, &db, "t", "20260829T120000Z").expect("in-place restore");

    // ── the unit ──
    assert!(
        report.preserved.db.exists(),
        "ADR-0116 AC-9 — the previous database was not preserved"
    );
    assert!(
        report
            .preserved
            .db
            .to_string_lossy()
            .contains("PRE-RESTORE-"),
        "the preserved DB must be tagged PRE-RESTORE so the evidence guard and the incident \
         grouping both recognise it: {}",
        report.preserved.db.display()
    );
    if had_wal {
        let wal = report.preserved.wal.as_ref().expect(
            "ADR-0116 AC-9/F4 — the live DB had a WAL and it was NOT preserved. Moving \
                     the DB without it strips its un-checkpointed commits, so the preserved \
                     unit is not a recoverable original.",
        );
        assert!(wal.exists());
    }
    assert!(
        report
            .preserved
            .ckpt_ok
            .as_ref()
            .is_some_and(|p| p.exists()),
        "ADR-0116 AC-9 — the .ckpt-ok marker must move WITH the DB it describes"
    );

    // ── no orphan WAL beside the restored file ──
    assert!(
        !live_wal.exists(),
        "ADR-0116 AC-9/F4 — an orphaned WAL was left at the live path beside the freshly \
         restored file. That is the corruption vector restore_into's own comment warns about."
    );

    // ── the mirror stays, byte-identical ──
    assert!(
        mirror.exists(),
        "ADR-0116 AC-9/D3.4 step 4 — the .audit.log mirror must NOT move. It is the durable \
         record and stays at the live path; an implementer moving 'the tenant's DB artefacts' \
         would naturally take it too."
    );
    assert_eq!(
        std::fs::read(&mirror).expect("read mirror"),
        mirror_before,
        "the mirror must be untouched by the restore"
    );

    // ── the restored file is the snapshot's content ──
    let restored: i64 = {
        let conn = Connection::open(&db).expect("open restored");
        conn.query_row("SELECT count(*) FROM invoice", [], |r| r.get(0))
            .expect("count")
    };
    assert_eq!(
        restored, 2,
        "the restored DB must hold the snapshot's 2 invoices, not the live 3"
    );

    // ── the RECOVERABILITY assertion the ADR asks for: read back a row that
    //    was in the WAL and not in the DB file. ──
    if had_wal {
        let preserved_count: i64 = {
            let conn = Connection::open(&report.preserved.db).expect("open preserved unit");
            conn.query_row("SELECT count(*) FROM invoice WHERE id = 999", [], |r| {
                r.get(0)
            })
            .expect("count preserved")
        };
        assert_eq!(
            preserved_count, 1,
            "ADR-0116 AC-9 — the row that existed ONLY in the WAL is missing from the preserved \
             unit. The unit is therefore not a recoverable original, which is precisely the \
             defect F4 caught in the first draft."
        );
    }

    // ── a fresh marker describes the file that is actually there ──
    assert!(
        aberp_snapshot::checkpoint_is_current(&db),
        "ADR-0116 AC-9 / D3.4 step 6 — a fresh .ckpt-ok must be written for the INSTALLED file. \
         Otherwise the restored file sits beside a marker describing the old one, and its \
         provenance record is a lie."
    );
}

/// The install refuses BEFORE moving anything aside when the snapshot cannot
/// restore — an operator must never pay a swapped-out live database for a
/// snapshot that was never going to work.
#[test]
fn ac9_in_place_restore_refuses_an_invalid_snapshot_without_touching_the_live_db() {
    let tmp = ScopedTempDir::new("ac9-refuse");
    let db = tmp.path().join("aberp.duckdb");
    seed_db(&db, "t", 2, 3);
    let before = std::fs::read(&db).expect("read live");

    let bogus = tmp.path().join("not-a-snapshot");
    std::fs::create_dir_all(&bogus).expect("mkdir");

    let err = restore_in_place(&bogus, &db, "t", "20260829T120000Z")
        .expect_err("an unrestorable export must refuse");
    assert!(
        format!("{err}").contains("refusing to restore")
            || format!("{err}").contains("failed validation"),
        "the refusal must be about the snapshot, not an incidental IO error: {err}"
    );
    assert_eq!(
        std::fs::read(&db).expect("read live after"),
        before,
        "ADR-0116 AC-9 — the live database was modified by a restore that refused"
    );
}

/// **The preserve step must not be able to strip the LIVE database of its WAL.**
///
/// Ordering within the `.PRE-RESTORE-` unit is load-bearing and the obvious
/// order is wrong. Moving the WAL first and the DB second means a failed DB
/// rename leaves the live database in place **without its WAL** — stripped of
/// every un-checkpointed commit, which is exactly the F4 failure the unit
/// exists to prevent, caused by the preserve step itself. Every `Handle`
/// commit is WAL-only until a checkpoint (ADR-0098 R5), so that is not a
/// narrow window; it is the most recent rows.
///
/// Asserted two ways, because neither alone is enough: the source order is
/// pinned (a refactor cannot silently swap them back), and a real injected
/// failure of the WAL move is shown to roll the DB back to the original state.
#[test]
fn preserve_moves_the_db_first_and_rolls_back_if_the_wal_move_fails() {
    // ── source ordering ──
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/take.rs"))
        .expect("read take.rs");
    let body_start = src
        .find("pub fn restore_in_place(")
        .expect("restore_in_place must exist");
    let body = &src[body_start..];
    let db_rename = body
        .find("std::fs::rename(db_path, &preserved_db)")
        .expect("the DB rename must exist");
    let wal_move = body
        .find("move_aside_to(&db_wal,")
        .expect("the WAL move must exist");
    assert!(
        db_rename < wal_move,
        "the DB must be moved aside BEFORE its WAL. With the WAL first, a failed DB rename \
         leaves the LIVE database without its un-checkpointed commits — F4's failure, caused \
         by the preserve step itself."
    );
    assert!(
        body[db_rename..wal_move + 2000].contains("rename(&preserved_db, db_path)"),
        "a failed WAL move must ROLL THE DB BACK; otherwise the live path is left with a WAL \
         and no database — the torn preserve in the other direction"
    );

    // ── injected failure: the WAL destination cannot be created ──
    let tmp = ScopedTempDir::new("preserve-order");
    let home = tmp.path().join(".aberp-defense").join("defense");
    std::fs::create_dir_all(&home).expect("mkdir");
    let db = home.join("aberp.duckdb");
    seed_db(&db, "t", 2, 3);
    let store = tmp.path().join("store");
    let rec = take_snapshot(&db, &store, "t", OffsetDateTime::now_utc()).expect("snapshot");

    // Give the live DB a WAL, and make its destination un-creatable by
    // planting a DIRECTORY where the preserved WAL file must go.
    {
        let conn = Connection::open(&db).expect("open");
        conn.execute_batch("PRAGMA disable_checkpoint_on_shutdown;")
            .expect("pragma");
        conn.execute(
            "INSERT INTO invoice VALUES (?, ?, ?)",
            duckdb::params![42i64, 1.0f64, "wal-only"],
        )
        .expect("insert");
    }
    if !wal_of(&db).exists() {
        // No WAL on this build/platform — the injected failure cannot be
        // staged, and asserting anything here would be vacuous. The source
        // ordering assertion above still stands.
        return;
    }
    let tag = "20260829T130000Z";
    let blocker = home.join(format!("aberp.duckdb.PRE-RESTORE-{tag}.wal"));
    std::fs::create_dir_all(&blocker).expect("plant a directory in the WAL's place");
    // rename(file -> existing non-empty dir) fails on macOS and Linux alike.
    std::fs::write(blocker.join("occupied"), b"x").expect("make the dir non-empty");

    let before_db = std::fs::read(&db).expect("read live db");
    let before_wal = std::fs::read(wal_of(&db)).expect("read live wal");

    let err = restore_in_place(&rec.dir, &db, "t", tag)
        .expect_err("the blocked WAL move must fail the restore");
    let _ = err;

    assert!(
        db.exists(),
        "the live database must be ROLLED BACK to its original path when the WAL move fails; \
         leaving it moved strands the live path with a WAL and no database"
    );
    assert_eq!(
        std::fs::read(&db).expect("read db after"),
        before_db,
        "the rolled-back database must be byte-identical to the original"
    );
    assert!(
        wal_of(&db).exists() && std::fs::read(wal_of(&db)).unwrap() == before_wal,
        "the live WAL must still be beside the live DB — the whole point of the rollback is \
         that the pair is intact"
    );
}

// ══════════════════════════════════════════════════════════════════════
// AC-11 (D4 / F7) — anchors recorded, never gating, never 0-for-unknown
// ══════════════════════════════════════════════════════════════════════

#[test]
fn ac11_zero_anchors_still_validates_and_is_recorded_as_checked_not_unknown() {
    let tmp = ScopedTempDir::new("ac11-anchors");
    let db = tmp.path().join("aberp.duckdb");
    seed_db(&db, "t", 2, 3);
    let store = tmp.path().join("store");
    let rec = take_snapshot(&db, &store, "t", OffsetDateTime::now_utc()).expect("snapshot");

    assert!(
        rec.meta.valid,
        "ADR-0116 AC-11 / D4 — a snapshot with ZERO anchors must still validate. Every \
         audit_ledger_anchors.parquet in both live stores is exactly 300 bytes, consistent \
         with zero anchor rows everywhere: a hard gate would mark EVERY existing snapshot \
         invalid, and plan_retention prunes invalid snapshots — so it would delete the entire \
         rollback store on the next cycle. That is a durability regression in service of a \
         legal property."
    );
    assert_eq!(
        rec.meta.anchor_count, 0,
        "the anchors table WAS readable and held nothing, so the count is a recorded 0"
    );
    assert_eq!(
        rec.meta.anchored_through_seq,
        Some(0),
        "ADR-0116 D4 — Some(0) means 'checked, nothing anchored'. None would mean 'never \
         checked', and the two must stay distinguishable for an operator deciding whether a \
         restored DB can be relied on in court."
    );
}

/// **ADR-0116 AC-11 / F7** — a `meta.json` written before D4 must read back
/// the NOT-RECORDED sentinels, never `0`/`Some(0)`.
///
/// The first draft used a bare `#[serde(default)]`, which for `i64`/`u64`
/// yields `0` — so every existing snapshot would read back "0 anchors",
/// indistinguishable from one *verified* to carry none. For a field whose only
/// purpose is telling a restoring operator what a database can prove in court,
/// defaulting to the worst-case-looking value while meaning "unknown" is
/// exactly backwards. The precedent is already in the tree: prod's
/// `secondary_index_count` carries a `-1` sentinel doc'd as "not recorded,
/// never zero indexes", added after the 2026-08-03 incident.
#[test]
fn ac11_a_pre_d4_meta_json_reads_back_not_recorded_never_zero() {
    // A verbatim pre-D4 meta.json: no meta_version, no anchor fields.
    let pre_d4 = r#"{
      "seq": 79,
      "created_at": "2026-08-26T10:19:21Z",
      "source_db_sha256": "3f2a1c",
      "byte_size": 1884160,
      "valid": true,
      "invoice_count": 12,
      "audit_count": 8412,
      "chain_len": 8412,
      "validation_error": null
    }"#;
    let meta: SnapshotMeta = serde_json::from_str(pre_d4).expect(
        "ADR-0116 D4 — a pre-D4 meta.json MUST keep parsing. meta.json is serde-parsed per \
         directory, and a snapshot whose sidecar no longer parses is skipped by \
         list_snapshots — i.e. invisible to retention AND to restore.",
    );
    assert_eq!(
        meta.anchor_count, -1,
        "ADR-0116 AC-11/F7 — a pre-D4 snapshot must read back anchor_count == -1 (NOT \
         RECORDED). A bare #[serde(default)] yields 0, which claims the snapshot was checked \
         and found to carry no anchors."
    );
    assert_eq!(
        meta.anchored_through_seq, None,
        "ADR-0116 AC-11/F7 — anchored_through_seq must be None (not recorded), never Some(0)"
    );
    assert_eq!(meta.meta_version, 1, "a pre-D4 sidecar is meta_version 1");
    // The pre-existing fields must be unchanged — this is an ADDITIVE format
    // change, and a regression here silently rewrites history.
    assert_eq!(meta.seq, 79);
    assert_eq!(meta.audit_count, 8412);
    assert!(meta.valid);
}

// ══════════════════════════════════════════════════════════════════════
// D2 — the tiered evidence release policy (pure, exhaustively testable)
// ══════════════════════════════════════════════════════════════════════

/// **ADR-0116 D2 — the tiered release policy, with each floor ISOLATED.**
///
/// The fixture deliberately carries FIVE distinct incidents so the
/// `recent_incidents = 3` floor does not swallow everything: a first cut of
/// this test used two incidents and every artefact came back `RecentIncident`,
/// which would have "passed" while proving nothing about the other floors.
/// Each assertion below names the specific floor it is pinning.
#[test]
fn evidence_policy_isolates_each_retention_floor() {
    let tmp = ScopedTempDir::new("evidence-policy");
    let home = tmp.path().join(".aberp-defense").join("defense");
    std::fs::create_dir_all(&home).expect("mkdir");

    // Live files must not appear in the inventory at all.
    for live in ["aberp.duckdb", "seller.toml", "aberp.duckdb.audit.log"] {
        std::fs::write(home.join(live), b"live").expect("write live");
    }

    // The three MOST RECENT incidents (2026-07-05, 07-06, 07-07), each with
    // two artefacts so the "only artefact" floor is not what retains them.
    for tag in ["20260705T184449Z", "20260706T061940Z", "20260707T233718Z"] {
        std::fs::write(home.join(format!("aberp.duckdb.CORRUPT-{tag}")), b"db").expect("write");
        std::fs::write(home.join(format!("aberp.duckdb.wal.CORRUPT-{tag}")), b"wal")
            .expect("write");
    }
    // An OLDER incident with two artefacts — outside the 3 most recent, so it
    // is the one thing the policy may actually release.
    for f in [
        "aberp.duckdb.PRE-DEDUP-20260101T000000Z",
        "aberp.duckdb.wal.PRE-DEDUP-20260101T000000Z",
    ] {
        std::fs::write(home.join(f), b"x").expect("write");
    }
    // An older SINGLETON — outside the 3 most recent, so only the
    // "only artefact of an incident" floor can be what retains it.
    std::fs::write(home.join("aberp.duckdb.PRE-TOPUP-20260102T000000Z"), b"x").expect("write");
    // Credential material, old enough that no other floor applies.
    std::fs::write(home.join("prod-20260103-keychain.zip"), b"secrets").expect("write");
    // An UNGROUPABLE artefact: evidence-shaped, but no parseable incident tag.
    std::fs::write(home.join("_recovery-notes.bak"), b"x").expect("write");

    let artefacts = list_evidence(&home).expect("list evidence");
    let names: Vec<&str> = artefacts.iter().map(|a| a.name.as_str()).collect();
    for live in ["aberp.duckdb", "seller.toml", "aberp.duckdb.audit.log"] {
        assert!(
            !names.contains(&live),
            "live files must not be inventoried as evidence: {names:?}"
        );
    }
    assert_eq!(artefacts.len(), 11, "eleven evidence artefacts: {names:?}");

    // `now` far in the future so the 90-day age floor cannot be what retains
    // anything — each remaining retention must be attributable to its own rule.
    let now = datetime!(2030-01-01 00:00:00 UTC);
    let plan = plan_evidence_release(&artefacts, &EvidencePolicy::default(), now);
    let by = |n: &str| {
        plan.iter()
            .find(|d| d.artefact.name == n)
            .unwrap_or_else(|| panic!("missing {n} in the plan"))
    };

    // ── floor: credential material ──
    assert_eq!(
        by("prod-20260103-keychain.zip").retained_because,
        Some(RetainReason::CredentialMaterial),
        "ADR-0116 D2.4 — encrypted NAV credentials and an SMTP password are never archived to \
         a second, less-protected location. For them release means delete-in-place or nothing, \
         and this command never deletes in place."
    );

    // ── floor: ungroupable => protected ──
    assert_eq!(
        by("_recovery-notes.bak").retained_because,
        Some(RetainReason::Ungroupable),
        "ADR-0116 D2 — 'ungroupable => protected' is the stated safe default. An artefact whose \
         incident cannot be established is kept, never guessed at."
    );

    // ── floor: the N most recent distinct incidents ──
    assert_eq!(
        by("aberp.duckdb.CORRUPT-20260707T233718Z").retained_because,
        Some(RetainReason::RecentIncident),
        "the 3 most recent distinct incidents are never released"
    );

    // ── floor: never the ONLY artefact of an incident ──
    assert_eq!(
        by("aberp.duckdb.PRE-TOPUP-20260102T000000Z").retained_because,
        Some(RetainReason::OnlyArtefactOfIncident),
        "ADR-0116 D2 — the only artefact of an incident is never released, even when it is old \
         and outside every other window"
    );

    // ── and the one thing that IS releasable ──
    assert_eq!(
        by("aberp.duckdb.PRE-DEDUP-20260101T000000Z").retained_because,
        None,
        "an OLD incident with more than one artefact, outside the 3 most recent, is the case \
         the policy exists to release. If nothing is ever releasable the policy is \
         operationally inert — technically correct and useless, which is exactly the trap \
         ADR-0116 D2 names for tag-only incident keying."
    );
}

/// The 90-day age floor is a floor, not a knob: it retains an artefact that
/// every other rule would release.
#[test]
fn evidence_age_floor_retains_a_recent_artefact_no_other_rule_would() {
    let tmp = ScopedTempDir::new("evidence-age");
    let home = tmp.path().join(".aberp-defense").join("defense");
    std::fs::create_dir_all(&home).expect("mkdir");
    // Four incidents, so the 3-most-recent floor does not cover the fourth.
    for tag in [
        "20260101T000000Z",
        "20260102T000000Z",
        "20260103T000000Z",
        "20260104T000000Z",
    ] {
        std::fs::write(home.join(format!("aberp.duckdb.CORRUPT-{tag}")), b"db").expect("write");
        std::fs::write(home.join(format!("aberp.duckdb.wal.CORRUPT-{tag}")), b"wal")
            .expect("write");
    }
    let artefacts = list_evidence(&home).expect("list");
    // `now` is the artefacts' own mtime (they were just written), so EVERY
    // artefact is inside the 90-day floor.
    let now = OffsetDateTime::now_utc();
    let plan = plan_evidence_release(&artefacts, &EvidencePolicy::default(), now);
    assert!(
        plan.iter().all(|d| d.retained_because.is_some()),
        "ADR-0116 D2 — nothing younger than the 90-day floor may be released, whatever the \
         other rules say"
    );
    assert!(
        plan.iter()
            .any(|d| d.retained_because == Some(RetainReason::WithinAgeFloor)),
        "at least one artefact must be attributed to the AGE floor specifically, or this test \
         is passing on the strength of a different rule"
    );
}

#[test]
fn nanosecond_tags_normalise_to_iso_so_the_policy_is_not_operationally_inert() {
    // ADR-0116 D2 — under tag-only keying every nanosecond-tagged artefact is
    // a singleton incident, permanently protected by the "only artefact" rule.
    // Safe, but the policy would never release the 22 `corrupt-*.bak` + 9
    // `healed-*.bak` that are MOST of the growth: technically correct and
    // operationally inert.
    let iso = aberp_snapshot::normalise_incident_tag(
        "aberp.duckdb.CORRUPT-20260705T184449Z",
        OffsetDateTime::UNIX_EPOCH,
    );
    assert_eq!(iso.as_deref(), Some("20260705T184449Z"));

    let nanos = aberp_snapshot::normalise_incident_tag(
        "aberp.duckdb.audit.log.corrupt-1783315209649645000.bak",
        OffsetDateTime::UNIX_EPOCH,
    )
    .expect("a nanosecond tag must normalise, not fall through to untagged");
    assert!(
        nanos.len() == 16 && nanos.ends_with('Z') && nanos.contains('T'),
        "a nanosecond tag must normalise to the SAME fixed-width ISO shape the ISO tags use, \
         or the two formats can never group into one incident: {nanos}"
    );

    // Two artefacts of the same instant, one in each format, must key alike.
    let ts = 1_783_315_209_649_645_000u64;
    let dt = OffsetDateTime::from_unix_timestamp_nanos(ts as i128).expect("dt");
    let iso_twin = format!(
        "aberp.duckdb.CORRUPT-{}",
        dt.format(&time::macros::format_description!(
            "[year][month][day]T[hour][minute][second]Z"
        ))
        .expect("fmt")
    );
    assert_eq!(
        aberp_snapshot::normalise_incident_tag(&iso_twin, OffsetDateTime::UNIX_EPOCH),
        Some(nanos),
        "the ISO artefact and the nanosecond artefact of the SAME incident must share a key"
    );
}

#[test]
fn validate_export_reports_anchor_coverage_on_a_broken_chain_too() {
    // A broken chain is exactly when an operator wants to know what the
    // snapshot could still prove, so coverage is recorded before the verdict.
    let tmp = ScopedTempDir::new("anchors-broken");
    let db = tmp.path().join("aberp.duckdb");
    seed_db(&db, "t", 1, 4);
    let store = tmp.path().join("store");
    let good = take_snapshot(&db, &store, "t", OffsetDateTime::now_utc()).expect("snapshot");
    {
        let conn = Connection::open(&db).expect("open");
        conn.execute_batch("PRAGMA disable_checkpoint_on_shutdown;")
            .expect("pragma");
        conn.execute("DELETE FROM audit_ledger WHERE seq = 2", [])
            .expect("break the chain");
    }
    let bad = take_snapshot(&db, &store, "t", OffsetDateTime::now_utc()).expect("snapshot");
    assert!(!bad.meta.valid, "fixture: the chain must be broken");
    assert_eq!(
        bad.meta.anchor_count, 0,
        "anchor coverage must be RECORDED even when the chain verdict is a failure — a `-1` \
         here would mean the anchors were never read, which is a different (and worse) answer"
    );
    let report = validate_export(&good.dir, "t");
    assert!(report.ok);
    assert_eq!(report.anchor_count, 0);
}
