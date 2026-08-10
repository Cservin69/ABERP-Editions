//! ADR-0110 D3 — the power-loss durability spec for Defense's money paths.
//!
//! Adapted from Portable's `adr0110_d6b_ondisk_durability.rs` (PROD_v2.33.6).
//!
//! # What this proves
//!
//! `durable_ack` promises that once the operator is told a money-path write
//! succeeded, the bytes survive a **power loss** — not merely a process kill.
//! This models a power loss the only way a filesystem-level harness can: it
//! drives a real money-path function to its ack, then copies **only the files
//! the write path certified as durable** into a fresh directory, boots that
//! copy, and demands the write back out of it. Everything not in the durable
//! set is simply absent from the copy — exactly what an un-`fsync`'d page-cache
//! page is after the power goes.
//!
//! # The durable set is DERIVED, never hard-coded
//!
//! [`power_loss_durable_set`] builds the copy manifest from
//! [`aberp_db::Handle::fsynced_paths`] — the durability journal, appended to
//! only on a SUCCESSFUL `sync_all`. That is what keeps this honest in both
//! directions:
//!
//! * Before D3 nothing in `crates/aberp-db/src/` called `sync_all`, the journal
//!   is empty, the set collapses to the two always-durable constants, the WAL
//!   is absent from the copy — and this test is RED.
//! * With D3 the WAL earns its place in the set and this is GREEN.
//! * **Delete the `fsync` and the WAL falls straight back out of the set: the
//!   derivation IS the mutation test.**
//!
//! The two constants are the files Defense made durable BEFORE D3, so they are
//! seeded unconditionally rather than derived: the audit mirror
//! (`sync_mirror` `fsync`s it on every `WriteGuard` drop) and the main DB file
//! (the debounced D2 checkpoint installs a fully-`fsync`'d replacement via
//! `atomic_install`). Seeding them is what makes the WAL the ONLY thing this
//! spec can be measuring.
//!
//! # Divergence from Portable's D6b — what is NOT ported here
//!
//! Portable's file carries three further tiers that reproduce the ISSUANCE
//! sequence by hand (the real `issue_from_parsed` needs the OS keychain, an
//! `MnbRatesProvider` and a seller profile, so an unattended test cannot drive
//! it). Those tiers call `aberp::issue_invoice::with_durable_high_water`, a
//! **Portable-only API from a later session (S444)** that does not exist in
//! Defense. Porting them would mean porting S444 as well, which is outside D3.
//!
//! They are also the *weaker* half: they model the write path rather than run
//! it. What is ported is the tier that models nothing —
//! [`real_money_path_mark_paid_survives_the_power_loss_durable_set`] drives the
//! actual shipped `mark_invoice_paid::mark_paid`, which carries the identical
//! `drop(guard); db.durable_ack()?` pair as the other four censused sites.
//! Flagged in the PR body as a named follow-up.
//!
//! # Honest scope
//!
//! This takes the write path's word that `sync_all` reached stable storage. A
//! harness that can only read the filesystem cannot tell a page-cache write
//! from an `fsync`'d one; that needs fault injection below the FS, so R1's
//! machine-restart clause is not proven by this file.
//!
//! What the word is worth is not in doubt — on macOS `sync_all` is
//! `fcntl(F_FULLFSYNC)`, a real device flush, not `fsync(2)` — and it is not
//! taken entirely on trust either: `crates/aberp-db/tests/durable_ack_fault_injection.rs`
//! breaks the filesystem reach and requires `durable_ack` to notice, which is
//! the one mutation this derivation cannot see (a `durable_ack` that journals a
//! path without ever syncing it would keep this green).
//!
//! # Scope
//!
//! `$TMPDIR` only. Nothing here touches `~/.aberp/**`.

use std::path::{Path, PathBuf};

use aberp_audit_ledger::{BinaryHash, TenantId};
use duckdb::Connection;

const TEST_BINARY_HASH: BinaryHash = BinaryHash::from_bytes([0xD3; 32]);
const DB_FILE: &str = "aberp.duckdb";
const MIRROR_FILE: &str = "aberp.duckdb.audit.log";

fn tenant_id() -> TenantId {
    TenantId::new("tenant-adr0110-d3").expect("test tenant id is valid")
}

fn tenant_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "aberp-adr0110-d3-{}-{}-{:?}",
        tag,
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir tenant dir");
    dir
}

/// Open the copied tenant the way boot does — a plain `Connection::open`, which
/// is what replays a WAL sitting next to the main file.
fn boot_shaped_open(db: &Path) -> Connection {
    Connection::open(db).expect("open tenant DuckDB (boot-shaped)")
}

/// Lay down an audit-ledger schema and fold it into the main file, so the
/// baseline is durable before the measured write happens. Without the
/// `CHECKPOINT` the schema itself would live in the WAL and the test could pass
/// for the wrong reason.
fn provision(db: &Path) {
    let conn = boot_shaped_open(db);
    aberp_audit_ledger::ensure_schema(&conn).expect("audit schema");
    conn.execute_batch("CHECKPOINT;")
        .expect("fold the provisioned baseline into the main file");
}

/// Drive one real `mark_paid` and return its invoice id.
fn mark_one_paid(handle: &aberp_db::HandleArc, amount_minor: i64, reference: &str) -> String {
    let invoice_id = format!("inv_{}", ulid::Ulid::new());
    aberp::mark_invoice_paid::mark_paid(
        handle,
        tenant_id(),
        TEST_BINARY_HASH,
        "operator@test",
        aberp::mark_invoice_paid::MarkPaidInput {
            invoice_id: invoice_id.clone(),
            paid_at: "2026-08-08".to_string(),
            amount_minor,
            currency: "HUF".to_string(),
            method: aberp::audit_payloads::PaymentMethod::BankTransfer,
            reference: Some(reference.to_string()),
        },
    )
    .expect("mark_paid must succeed on a fresh invoice");
    invoice_id
}

/// **Enter the debounce shadow — the window D3 actually protects.**
///
/// `CheckpointDebouncer::should_checkpoint_now` returns `true` unconditionally
/// when `last_checkpoint` is `None`, so the FIRST committed write in a fresh
/// process always fires an immediate D2 checkpoint. That checkpoint
/// `EXPORT`s + `atomic_install`s a fully-`fsync`'d main file and deletes the
/// WAL, so after write #1 there is no WAL to lose and a durability spec
/// measured there is VACUOUS — it would pass with `durable_ack` deleted.
///
/// Writes #2..N inside `DEFAULT_MIN_CHECKPOINT_INTERVAL` (60 s) are debounced:
/// no checkpoint runs, the rows stay in a live WAL, and before D3 nothing
/// `fsync`'d it. That is the exposure, so that is where these tests measure.
///
/// This burns write #1 and returns with the handle sitting in the shadow.
fn enter_debounce_shadow(handle: &aberp_db::HandleArc) {
    mark_one_paid(handle, 1, "D3-WARMUP-absorbs-the-immediate-checkpoint");
}

/// The file names a power loss would leave behind: the two Defense already made
/// durable before D3, plus every path the write path actually `fsync`'d.
///
/// See the module docs — this derivation is the mutation test.
fn power_loss_durable_set(handle: &aberp_db::Handle) -> Vec<String> {
    let mut set = vec![DB_FILE.to_string(), MIRROR_FILE.to_string()];
    for path in handle.fsynced_paths() {
        let name = path
            .file_name()
            .expect("a journalled fsync path always names a file")
            .to_string_lossy()
            .into_owned();
        if !set.contains(&name) {
            set.push(name);
        }
    }
    set
}

/// Copy `from` → `to`, keeping only the names in `only`. Everything else is
/// dropped on the floor, which is the power loss.
fn copy_on_disk_bytes(from: &Path, to: &Path, only: Option<&[&str]>) -> Vec<(String, u64)> {
    std::fs::create_dir_all(to).expect("mkdir copy tenant dir");
    let mut manifest = Vec::new();
    for entry in std::fs::read_dir(from).expect("read tenant dir") {
        let entry = entry.expect("dir entry");
        let src = entry.path();
        // DuckDB's spill directory (`<db>.tmp/`) is scratch space, never
        // replayed at boot. Nothing else directory-shaped is expected here.
        if !src.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(allow) = only {
            if !allow.contains(&name.as_str()) {
                continue;
            }
        }
        let bytes = std::fs::copy(&src, to.join(&name)).expect("copy on-disk bytes");
        manifest.push((name, bytes));
    }
    manifest.sort();
    manifest
}

/// **The production function, not a model of it.**
///
/// `mark_invoice_paid::mark_paid` is a real money-path ack — the operator is
/// told "marked paid" — its only inputs are the shared `Handle`, the tenant and
/// the payload, and it carries the same `drop(guard); db.durable_ack()?` pair
/// as the other four censused sites. So this drives the actual shipped
/// function, cuts the power at its ack, and demands the payment back out of the
/// durable set with nothing modelled anywhere in the chain.
///
/// **Measured in the debounce shadow** (see [`enter_debounce_shadow`]): the
/// warm-up write absorbs the unconditional first-write D2 checkpoint, so the
/// MEASURED payment lives in a live WAL that only `durable_ack` flushes. Run
/// against write #1 instead, this test passes with D3 deleted — the checkpoint
/// would have folded the row into the main file, and the spec would be proving
/// the checkpoint, not the ack.
///
/// It reads back via `audit_query::payment_record_for` against the **copied
/// DB**, not against the mirror — deliberately. Defense already `fsync`'d the
/// mirror before D3, so a mirror-only check would have passed pre-D3 and proven
/// nothing. The DB's own `audit_ledger` table is also what `mark_paid`'s
/// no-double-payment gate reads, so a payment durable only in the mirror
/// silently re-opens exactly the double-payment window that gate exists to
/// close.
#[test]
fn real_money_path_mark_paid_survives_the_power_loss_durable_set() {
    let live = tenant_dir("markpaid");
    let db = live.join(DB_FILE);
    provision(&db);

    let handle = aberp_db::Handle::open_default(&db, tenant_id()).expect("shared Handle");
    enter_debounce_shadow(&handle);

    // THE PRODUCTION FUNCTION, inside the shadow.
    let amount_minor = 3_480_653;
    let invoice_id = mark_one_paid(&handle, amount_minor, "D3-REAL-PATH");
    // `mark_paid` has returned: in `serve.rs` this is the 200. Cut the power.

    let copy = tenant_dir("markpaid-copy");
    let durable = power_loss_durable_set(&handle);
    let only: Vec<&str> = durable.iter().map(String::as_str).collect();
    let manifest = copy_on_disk_bytes(&live, &copy, Some(&only));

    let conn = boot_shaped_open(&copy.join(DB_FILE));
    let ledger = aberp_audit_ledger::Ledger::from_connection(conn, tenant_id(), TEST_BINARY_HASH);
    let recovered = aberp::audit_query::payment_record_for(&ledger, &invoice_id)
        .expect("read payment record back from the durable set");

    assert_eq!(
        recovered.as_ref().map(|p| p.amount_minor),
        Some(amount_minor),
        "ADR-0110 R1 VIOLATED on the REAL mark-paid path — the operator was told \
         invoice {invoice_id} was marked paid ({amount_minor} minor units), and \
         the power-loss durable set does not have it back: {recovered:?}.\n\n\
         Durable set: {manifest:?}"
    );
}

/// The companion pin: the WAL is in the durable set *because the write path put
/// it there*, not because this file hard-codes it — and it says so out loud, so
/// a regression reports its cause rather than a confusing "payment not
/// recovered".
///
/// Also pins the debounce-shadow premise itself. If a future change made the
/// first write NOT checkpoint (or made every write checkpoint), the shadow
/// would move and the spec above would quietly stop measuring D3.
#[test]
fn the_wal_is_in_the_durable_set_only_because_the_write_path_fsynced_it() {
    let live = tenant_dir("derivation");
    let db = live.join(DB_FILE);
    provision(&db);

    let handle = aberp_db::Handle::open_default(&db, tenant_id()).expect("shared Handle");

    // Before any ack the journal is empty, so the set is exactly the two files
    // Defense already made durable before D3 — the RED baseline.
    assert_eq!(
        power_loss_durable_set(&handle),
        vec![DB_FILE.to_string(), MIRROR_FILE.to_string()],
        "with no ack yet the durable set must be only the two pre-D3 files"
    );

    // Write #1 fires the unconditional D2 checkpoint, which folds and DELETES
    // the WAL. Pin that, because it is why the measured write must be #2.
    mark_one_paid(&handle, 1, "D3-WARMUP");
    let wal = live.join("aberp.duckdb.wal");
    assert!(
        !wal.exists(),
        "premise changed: the first write no longer folds the WAL away, so the \
         debounce shadow this spec measures in has moved — re-derive it"
    );

    // Write #2 is debounced: no checkpoint, so the rows stay in a live WAL.
    mark_one_paid(&handle, 2, "D3-MEASURED");
    assert!(
        wal.exists(),
        "premise changed: a debounced write left no WAL, so there is nothing \
         for durable_ack to flush and this spec proves nothing"
    );

    let set = power_loss_durable_set(&handle);
    assert!(
        set.iter().any(|n| n.ends_with(".wal")),
        "ADR-0110 D3 REGRESSION: after a debounced money-path ack the WAL is \
         still not in the derived durable set ({set:?}). The acked rows live in \
         that WAL until the next D2 checkpoint (up to 60s later), so this is the \
         un-fsynced posture with a durable_ack call in front of it."
    );
}

// ══════════════════════════════════════════════════════════════════════
// PR #37 ADVERSARIAL — B2. The mirror-ahead fork on the ack's ERROR path.
//
// Test body adapted from the adversarial's own `attack_d3_residuals.rs::f1`,
// pulled in as the regression pin for the fix.
// ══════════════════════════════════════════════════════════════════════

/// **The B2 pin.** When the data flush fails, the audit mirror must NOT record
/// the write the operator was just told did not happen.
///
/// The first cut of this port had `WriteGuard::drop` `fsync` the mirror and
/// `durable_ack` run strictly afterwards, so the mirror was unconditionally
/// made durable FIRST. On the happy path that merely shrank a window. On the
/// ack's failure path it froze a fork:
///
/// * `mark_paid` returns `Err` — the operator is told the payment did not go
///   through, and (presumably) takes it again;
/// * the mirror durably holds the `InvoicePaymentRecorded` entry anyway;
/// * boot's `attempt_db_auto_recovery(mirror_ahead)` → `replay_mirror_delta`
///   replays it into `audit_ledger` and the payment RESURRECTS.
///
/// Pre-D3 that state was unreachable, because `mark_paid` returned `Ok` here —
/// so the first cut of D3 *introduced* a fork on the very path it existed to
/// protect. The fix reorders the flush ahead of the mirror sync and SKIPS the
/// mirror sync when it fails, leaving the mirror behind (benign, self-healing)
/// rather than ahead.
///
/// The failure is injected the same way the crate's own fault-injection suite
/// does it: unlink the main DB path. The `Handle`'s open `Connection` keeps its
/// fd so the commit still succeeds; only the flush's fresh `File::open` sees
/// `ENOENT`.
#[test]
fn b2_failed_data_flush_must_not_leave_the_mirror_ahead_of_the_db() {
    let live = tenant_dir("mirror-ahead");
    let db = live.join(DB_FILE);
    provision(&db);

    let handle = aberp_db::Handle::open_default(&db, tenant_id()).expect("handle");
    enter_debounce_shadow(&handle);

    // Break ONLY the flush's reach.
    std::fs::remove_file(&db).expect("unlink the main DB path");

    let mirror = live.join(MIRROR_FILE);
    let before = aberp_audit_ledger::read_mirror_entries(&mirror)
        .expect("read mirror")
        .len();

    let invoice_id = format!("inv_{}", ulid::Ulid::new());
    let outcome = aberp::mark_invoice_paid::mark_paid(
        &handle,
        tenant_id(),
        TEST_BINARY_HASH,
        "operator@test",
        aberp::mark_invoice_paid::MarkPaidInput {
            invoice_id: invoice_id.clone(),
            paid_at: "2026-08-08".to_string(),
            amount_minor: 4_242_424,
            currency: "HUF".to_string(),
            method: aberp::audit_payloads::PaymentMethod::BankTransfer,
            reference: Some("ACK-FAILS".to_string()),
        },
    );

    assert!(
        outcome.is_err(),
        "premise: with the flush's reach broken, mark_paid must fail — otherwise \
         this test is not measuring the error path at all"
    );

    let after = aberp_audit_ledger::read_mirror_entries(&mirror)
        .expect("read mirror")
        .len();

    assert_eq!(
        after,
        before,
        "ADR-0110 MIRROR-AHEAD FORK. mark_paid returned Err — the operator was \
         told invoice {invoice_id} was NOT marked paid ({outcome:?}) — but the \
         audit MIRROR at {} grew from {before} to {after} entries: it durably \
         recorded the payment anyway.\n\n\
         Defense's boot auto-heal replays the mirror into `audit_ledger`, so \
         this payment RESURRECTS on the next boot, after the operator was told \
         it failed and has presumably taken it again.\n\n\
         The data flush must happen BEFORE the mirror sync, and the mirror sync \
         must be SKIPPED when it fails.",
        mirror.display()
    );
}

/// The other half of B2: the reorder must not have broken the happy path. On a
/// successful ack the mirror still tracks the DB in lockstep — mirror-behind is
/// tolerated, but it must not become the permanent resting state.
#[test]
fn b2_successful_ack_still_syncs_the_mirror_in_lockstep() {
    let live = tenant_dir("mirror-lockstep");
    let db = live.join(DB_FILE);
    provision(&db);

    let handle = aberp_db::Handle::open_default(&db, tenant_id()).expect("handle");
    enter_debounce_shadow(&handle);

    let mirror = live.join(MIRROR_FILE);
    let before = aberp_audit_ledger::read_mirror_entries(&mirror)
        .expect("read mirror")
        .len();

    mark_one_paid(&handle, 7_777, "B2-HAPPY-PATH");

    let after = aberp_audit_ledger::read_mirror_entries(&mirror)
        .expect("read mirror")
        .len();
    assert!(
        after > before,
        "the reorder must not have disabled the lockstep mirror sync on the \
         SUCCESS path: mirror stayed at {before} entries across a successful \
         money-path ack"
    );
}
