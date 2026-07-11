//! ADR-0095 §1 — the **`aberp serve` BOOT PATH** silently auto-heals a
//! mirror-ahead-of-DB tear, with ZERO operator / CLI action.
//!
//! # The gap this closes
//!
//! `boot_crash_recovery_e2e.rs` already proves the shared recovery engine
//! heals an ahead-mirror tear — but it drives the engine through the
//! `aberp recover` CLI (an explicit operator step). What it does NOT prove
//! is that the **`aberp serve` boot path itself** reaches the identical
//! engine with no operator action: that a server started against a torn
//! tenant comes up serving on its own. That is the operator-facing promise
//! of the live Defense line (`serve.rs` mirror-reconcile → `MirrorAheadOfDb`
//! → `attempt_db_auto_recovery(.., "mirror_ahead")` →
//! `aberp_snapshot::recover_or_refuse_with_audit`), and this file is its
//! end-to-end pin.
//!
//! We seed a tenant into the exact ahead-mirror tear
//! (`boot_crash_recovery_e2e.rs`'s `recover_cli_replays_ahead_mirror…`
//! fingerprint: a valid snapshot, committed audit entries past it, then the
//! live DB replaced by a fresh empty one while the append-only mirror still
//! leads it), spawn the REAL `aberp serve` binary, and assert:
//!
//! 1. **No operator step.** `serve` reaches the `READY …` handshake on its
//!    own — the recovery runs inside boot, before the listener binds; the
//!    `READY` line is printed only *after* the TCP listener is bound
//!    (`serve.rs` prints `resolved_addr` from `listener.local_addr()`), so
//!    reaching it proves the server is serving with no `aberp recover` in
//!    between.
//! 2. **Chain verifies genesis→head** on the post-boot DB
//!    (`Ledger::verify_chain`).
//! 3. **No fork, no loss.** Every committed audit entry from before the tear
//!    is present post-boot (the mirror tail was REPLAYED, not truncated),
//!    the invoices are restored from the snapshot, a verified-good checkpoint
//!    marker covers the rebuilt DB, and the heal is audited as
//!    `db.auto_recovered` exactly once.
//!
//! # Fully hermetic — synthetic `$HOME`, no real keychain (the closed gap)
//!
//! Unlike the `aberp recover` CLI, the `aberp serve` boot path always
//! mints/reads the SPA session token from the OS keychain (the Bearer secret,
//! independent of NAV — see `serve::load_or_create_session_token`). On macOS
//! the keychain is resolved from the real login `HOME`; a synthesised `HOME`
//! fails with "a default keychain could not be found" — which is exactly why
//! a prior iteration of this pin had to run under the real `HOME` behind an
//! opt-in gate (the same posture as `portable_demo_boot_e2e.rs`'s subprocess
//! pin and `serve_boot_budget_live.rs`).
//!
//! That keychain dependency is now removed by the TEST-ONLY, non-production,
//! debug-build-only session-token bypass (`serve::SESSION_TOKEN_TEST_BYPASS_ENV`
//! = `ABERP_KEYCHAIN_TEST_BYPASS=1`): with it set, boot supplies a dummy
//! in-memory token and never touches the keychain. Combined with a NAV-off
//! tenant (which already skips the NAV-credentials keychain read and the §169
//! seller gate), boot makes **zero** keychain calls — so a synthetic `HOME`
//! now works. This test therefore runs **fully hermetic and unattended**:
//!   * a per-process temp `HOME`, so the tenant registry
//!     (`~/.aberp-portable/tenants.toml`) and the edition snapshot store
//!     (`~/Documents/ABERP-snapshots-portable/<tenant>/`) live entirely under
//!     that temp dir and are removed on drop — a real Portable install is
//!     never read or written;
//!   * the DB + audit mirror in a second temp dir (`--db`);
//!   * `ABERP_KEYCHAIN_TEST_BYPASS=1` (no keychain) + a NAV-off registry (no
//!     NAV, deterministic `state=ready`); background snapshotting disabled.
//! No env-gate skip: this runs in a normal `cargo test --workspace`.
//!
//! The whole file is gated to `all(not(feature = "production"), debug_assertions)`
//! — the EXACT compile config in which the session-token bypass exists in the
//! spawned `aberp` binary. That keeps the test and the bypass in lockstep: a
//! `--release` or `--features production` build (where the bypass is compiled
//! out) simply skips this test instead of hanging on the real keychain.
//!
//! # Portable arm only
//!
//! The mirror-ahead → `attempt_db_auto_recovery` boot block in `serve.rs` is
//! edition-agnostic (NOT `#[cfg(feature = "production")]`-gated) — the Defense
//! and Portable binaries run byte-identical recovery instructions — so
//! exercising it on the Portable arm proves the code the Defense line runs.
//! (The session-token bypass is orthogonal to the recovery logic: it only
//! removes the keychain dependency; it is itself compiled out of the Defense
//! `--features production` binary.) We deliberately do NOT drive the Defense
//! (`--features production`) binary: its edition data root (`~/.aberp-defense/`)
//! is the pilot's LIVE operational store, and `serve` boot self-heals the
//! registry with the running tenant — a test must not write a throwaway tenant
//! into live operational state. Scoping to the Portable arm mirrors
//! `portable_demo_boot_e2e.rs`, which is Portable-scoped for the same
//! subprocess-boot reasons.
#![cfg(all(not(feature = "production"), debug_assertions))]

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use aberp_audit_ledger::{
    mirror_path_for, read_mirror_entries, Actor, BinaryHash, EventKind, Ledger, TenantId,
};
use aberp_snapshot::checkpoint_is_current;
use duckdb::Connection;

/// A distinctive slug so nothing this test touches — the tenant registry row,
/// the edition snapshot store — can collide with a real operator tenant (it
/// cannot anyway, since everything lives under a synthetic temp `HOME`).
const TENANT: &str = "e2eserveboot";

// ── scaffolding (mirrors boot_crash_recovery_e2e.rs) ──────────────────────

/// A per-process temp dir, removed on drop. Used both for the DB dir and for
/// the synthetic `HOME`, so the whole test footprint is torn down
/// automatically — nothing under the real `HOME` is ever created.
struct Tmp(PathBuf);
impl Tmp {
    fn new(label: &str) -> Self {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p = std::env::temp_dir().join(format!(
            "aberp-serve-boot-recover-e2e-{label}-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&p).unwrap();
        Tmp(p)
    }
    fn dir(&self) -> &Path {
        &self.0
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

/// Kill-on-drop guard so a failed assertion never strands a bound listener.
struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn tid() -> TenantId {
    TenantId::new(TENANT.to_string()).unwrap()
}
fn bh() -> BinaryHash {
    BinaryHash::from_bytes([1u8; 32])
}

/// Seed `path` with `n_invoice` invoice rows and `n_audit` chained audit
/// entries — the same scaffold shape `boot_crash_recovery_e2e.rs` uses.
fn seed(path: &Path, n_invoice: usize, n_audit: usize) {
    {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch("CREATE TABLE IF NOT EXISTS invoice (id BIGINT, amount DOUBLE);")
            .unwrap();
        for i in 0..n_invoice {
            conn.execute(
                "INSERT INTO invoice VALUES (?, ?)",
                duckdb::params![i as i64, i as f64],
            )
            .unwrap();
        }
    }
    append_audit(path, n_audit);
}

fn append_audit(path: &Path, n: usize) {
    let mut ledger = Ledger::open(path, tid(), bh()).unwrap();
    for i in 0..n {
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

fn sync_mirror_of(path: &Path) -> u64 {
    let mirror = mirror_path_for(path);
    let ledger = Ledger::open(path, tid(), bh()).unwrap();
    ledger.sync_mirror(&mirror).unwrap()
}

fn invoice_ids(path: &Path) -> Vec<i64> {
    let conn = Connection::open(path).unwrap();
    let mut stmt = conn.prepare("SELECT id FROM invoice ORDER BY id").unwrap();
    let v = stmt
        .query_map([], |r| r.get::<_, i64>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    v
}

/// Every entry_hash (raw bytes) currently in the DB ledger, in seq order.
fn entry_hashes(path: &Path) -> Vec<Vec<u8>> {
    let l = Ledger::open(path, tid(), bh()).unwrap();
    l.entries()
        .unwrap()
        .iter()
        .map(|e| e.entry_hash.as_bytes().as_slice().to_vec())
        .collect()
}

fn count_kind(path: &Path, kind: EventKind) -> usize {
    let l = Ledger::open(path, tid(), bh()).unwrap();
    l.entries()
        .unwrap()
        .iter()
        .filter(|e| e.kind == kind)
        .count()
}

fn wal_of(path: &Path) -> PathBuf {
    let mut o = path.as_os_str().to_owned();
    o.push(".wal");
    PathBuf::from(o)
}

/// Replace the live DB with a FRESH EMPTY one (audit head 0) — the Defense
/// ahead-mirror trigger (boot rebuilt an empty DB; the mirror was ahead).
fn make_fresh_empty(path: &Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(wal_of(path));
    let _ = Ledger::open(path, tid(), bh()).unwrap();
}

/// Stand up a NAV-off registry row for `TENANT` at
/// `<home>/.aberp-portable/tenants.toml`. This pins the boot onto the
/// deterministic `state=ready` path (NAV synchron off ⇒ skip the keychain +
/// §169 seller gate) and marks the install non-fresh (no demo bootstrap). The
/// path is derived from the synthetic `home` + the compile-time edition data
/// dirname — never the process `$HOME` — so it lands under the temp home.
fn seed_nav_off_registry(home: &Path) {
    let mut reg = aberp::tenant_registry::TenantRegistry::default();
    reg.add(
        TENANT,
        "Serve-boot recovery e2e",
        time::OffsetDateTime::now_utc(),
    )
    .expect("add tenant row");
    reg.set_nav_enabled(TENANT, false)
        .expect("flip tenant NAV synchron off");
    let root = home.join(aberp::build_profile::edition_data_dirname());
    std::fs::create_dir_all(&root).expect("create Portable edition data root under synthetic HOME");
    reg.write_to(&root.join(aberp::tenant_registry::REGISTRY_FILENAME))
        .expect("write NAV-off tenants.toml");
}

/// Spawn the REAL built `aberp` binary with `args`, fully hermetic:
///   * `HOME` = the synthetic temp home, so every `$HOME`-derived path (the
///     registry, the edition snapshot store) lands under the temp dir;
///   * `ABERP_KEYCHAIN_TEST_BYPASS=1`, so the boot session-token read supplies
///     a dummy in-memory token and NEVER touches the OS keychain (the
///     non-production, debug-build-only bypass);
///   * `ABERP_SNAPSHOT_DISABLE=1`, so nothing mutates the DB or store after
///     boot, keeping the post-boot assertions deterministic.
/// `stdout` is captured; `stderr` is inherited so a boot refusal is visible.
fn spawn_aberp(home: &Path, args: &[&str]) -> Child {
    Command::new(env!("CARGO_BIN_EXE_aberp"))
        .args(args)
        .env("HOME", home)
        .env("ABERP_KEYCHAIN_TEST_BYPASS", "1")
        .env("ABERP_SNAPSHOT_DISABLE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn aberp binary")
}

/// Run a short-lived `aberp` subcommand (e.g. `snapshot now`) to completion
/// under the same hermetic environment and assert it exited 0.
fn run_aberp_to_completion(home: &Path, args: &[&str]) {
    let ok = spawn_aberp(home, args)
        .wait()
        .expect("wait for aberp subcommand")
        .success();
    assert!(ok, "aberp {args:?} must exit 0");
}

// ── the test ──────────────────────────────────────────────────────────────

/// The `aberp serve` boot path silently auto-heals a mirror-ahead tear.
#[test]
fn serve_boot_auto_recovers_ahead_mirror_with_no_operator_step() {
    // Two temp dirs: one is the synthetic HOME (registry + snapshot store),
    // one holds the DB (`--db`). Both are removed on drop, so the entire
    // footprint — including everything under the synthetic HOME — is
    // automatically cleaned; the real HOME is never touched.
    let home = Tmp::new("home");
    let t = Tmp::new("db");
    let db = t.db();
    let db_s = db.to_str().unwrap();
    let mirror = mirror_path_for(&db);

    // NAV-off registry under the synthetic HOME ⇒ boot is deterministic
    // (state=ready, no demo bootstrap, no keychain).
    seed_nav_off_registry(home.dir());

    // Truth before the tear: 3 invoices + 4 audit entries, a VALID snapshot
    // taken by the REAL binary into the edition store, then 2 more committed
    // audit entries the snapshot does not yet cover. (`snapshot now` is itself
    // an audited action, so it appends its own entry — the exact head is
    // derived, not hardcoded, so the pin never encodes an incidental count.)
    seed(&db, 3, 4);
    run_aberp_to_completion(
        home.dir(),
        &["snapshot", "now", "--db", db_s, "--tenant", TENANT],
    );
    append_audit(&db, 2);
    let committed = entry_hashes(&db);
    let head_before = committed.len() as u64;
    assert!(
        head_before >= 6,
        "at least the 4 seeded + 2 appended committed audit entries exist before the tear; \
         got {head_before}",
    );
    assert_eq!(sync_mirror_of(&db), head_before, "mirror at the DB head");

    // The Defense fingerprint: the live DB is lost → a fresh empty one
    // (head 0) takes its place while the append-only mirror still carries
    // every committed entry (ahead of the DB).
    make_fresh_empty(&db);

    // Spawn `aberp serve` against the torn tenant. NO `aberp recover` — the
    // heal must happen inside boot, unattended.
    let mut guard = ChildGuard(spawn_aberp(
        home.dir(),
        &["serve", "--tenant", TENANT, "--db", db_s, "--port", "0"],
    ));

    // Read stdout until the READY handshake, EOF (backend died), or timeout.
    let stdout = guard.0.stdout.take().expect("subprocess stdout pipe");
    let mut reader = BufReader::new(stdout);
    let started = Instant::now();
    let mut ready_line: Option<String> = None;
    let mut line_buf = String::new();
    while ready_line.is_none() {
        line_buf.clear();
        let n = reader
            .read_line(&mut line_buf)
            .expect("read subprocess stdout");
        if n == 0 {
            break; // EOF — backend died before READY
        }
        let trimmed = line_buf.trim();
        if trimmed.starts_with("READY ") {
            ready_line = Some(trimmed.to_string());
        }
        if started.elapsed() > Duration::from_secs(60) {
            break;
        }
    }

    let ready_line = ready_line.expect(
        "`aberp serve` must reach a `READY 127.0.0.1:<port> sha256:<hex> state=…` line on its \
         own — the ahead-mirror tear must be auto-healed inside boot with NO operator step. It \
         never did: boot either stalled or refused (see inherited stderr).",
    );

    // (1) No operator step: the READY line proves the listener bound (serve
    // prints the resolved addr from `listener.local_addr()`), so the server
    // is serving; the recovery ran earlier in boot to make that possible.
    let addr = ready_line
        .strip_prefix("READY ")
        .and_then(|rest| rest.split_whitespace().next())
        .expect("READY line carries a 127.0.0.1:<port> address");
    assert!(
        addr.parse::<std::net::SocketAddr>().is_ok(),
        "READY address `{addr}` must parse as a bound socket address",
    );
    // NAV-off boot reaches the strong `state=ready` (the listener bound and
    // the server is serving); recovery runs regardless of NAV state.
    assert!(
        ready_line.contains("state=ready"),
        "NAV-off boot must reach a serving state=ready; got `{ready_line}`",
    );

    // Tear the server down and wait so it releases the DuckDB file lock
    // before we re-open the DB for the post-boot assertions.
    let _ = guard.0.kill();
    let _ = guard.0.wait();
    drop(guard);

    // (2) The chain verifies genesis→head on the post-boot DB.
    {
        let l = Ledger::open(&db, tid(), bh()).unwrap();
        let verified = l
            .verify_chain()
            .expect("post-boot audit chain must verify genesis→head");
        assert!(
            verified >= head_before,
            "verified chain length {verified} must be >= the {head_before} committed pre-tear \
             entries (no fork, no truncation)",
        );
    }

    // (3) No fork, no loss: every committed pre-tear entry survives (the
    // mirror tail was REPLAYED, not truncated), the invoices are restored
    // from the snapshot, a verified-good marker covers the rebuilt DB, and
    // the heal is audited exactly once.
    let after = entry_hashes(&db);
    for h in &committed {
        assert!(
            after.contains(h),
            "a committed audit entry was lost by the serve-boot auto-recovery",
        );
    }
    assert_eq!(
        invoice_ids(&db),
        vec![0, 1, 2],
        "invoices restored from the snapshot",
    );
    assert!(
        checkpoint_is_current(&db),
        "a verified-good marker covers the rebuilt DB (serve-boot ahead-mirror path)",
    );
    assert_eq!(
        count_kind(&db, EventKind::DbAutoRecovered),
        1,
        "the serve-boot heal is audited as db.auto_recovered exactly once",
    );
    assert!(
        read_mirror_entries(&mirror).unwrap().len() as u64 >= head_before,
        "the ahead mirror was REPLAYED by the serve boot path, never truncated",
    );

    // Keep the temp dirs alive until here; Drop removes both (incl. the
    // synthetic HOME's registry + snapshot store).
    let _keep = (home.dir(), t.dir());
}
