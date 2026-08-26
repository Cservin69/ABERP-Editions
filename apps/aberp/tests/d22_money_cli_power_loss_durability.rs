//! D-22 (ADR-0114) — the power-loss / fault-injection spec for the NAV
//! money-submission **CLI** paths.
//!
//! # The gap this pins closed
//!
//! ADR-0110 D3 closed the durability inversion on the serve/daemon path in
//! v0.4.x: `WriteGuard::drop` `fsync`s the DB + WAL + tenant directory BEFORE
//! it syncs the audit mirror, and `Handle::durable_ack` claims that flush's
//! outcome so a money-path ack propagates a failure instead of logging it.
//!
//! The **CLI** money paths were not on that mechanism. They did
//!
//! ```text
//! Ledger::open(db) -> append -> sync_mirror(<db>.audit.log)
//! ```
//!
//! on their own `Connection`, with `PRAGMA disable_checkpoint_on_shutdown` set.
//! Nothing ever `fsync`ed the DB or its WAL, and the connection's close
//! deliberately folded nothing — so the audit MIRROR was explicitly made
//! durable while the DB row it mirrors was left in an un-`fsync`'d WAL. That is
//! the durability ordering exactly inverted (ADR-0099 §R2.2), it was live at
//! `main` on real NAV ÁFA filings, and it is the shape of the 2026-08-08 loss.
//!
//! # What is measured here, and why it is not vacuous
//!
//! `mark_abandoned` is the one D-22 path an unattended test can drive
//! end-to-end: it writes a terminal money-path decision (the invoice's sequence
//! is burned for good) and makes **no NAV call**. D-22 split its library core
//! out of the CLI wrapper — [`aberp::mark_abandoned::mark_abandoned_from_inputs`]
//! — precisely so a test can own the `Handle` and control which write is
//! measured.
//!
//! Three mutations of `mark_abandoned_from_inputs` were RUN against this file.
//! All three are killed, and by different assertions — which is what says the
//! three tests are not the same test three times:
//!
//! | mutation | what it restores | killed by |
//! |---|---|---|
//! | **M1** — `db.durable_ack()?` rewritten as `if let Err(e) = … { warn!() }` | the ADR-0110 R3 downgrade | `…refuses_to_ack_when_the_durable_flush_fails` ONLY |
//! | **M2** — `db.write()` replaced by an independent pragma-fenced `Connection::open` + `Ledger::open` + `sync_mirror`, ack kept | the pre-D-22 opener | `…survives_the_power_loss_durable_set` |
//! | **M3** — both of the above together | the pre-D-22 posture verbatim | all three |
//!
//! Two results are worth writing down rather than assuming:
//!
//! * **M1 leaves the bytes durable.** The `fsync` happens in `WriteGuard::drop`
//!   (ADR-0110's B2 reorder), not in `durable_ack` — so deleting the ack costs
//!   the operator the FAILURE REPORT, not the data. A power-loss spec
//!   structurally cannot see that, which is exactly why the fault-injection
//!   test exists alongside it.
//! * **M2 keeps the row in the durable set**, because `durable_ack` on a handle
//!   with no parked outcome falls through to a direct `fsync_data_paths`. What
//!   it loses is COHERENCE: the row went to a second DuckDB instance, so the
//!   shared handle's own `verify_chain` reports a short chain. The power-loss
//!   test catches it there. Durability and single-instance coherence are two
//!   guarantees, and M2 is the one that separates them.
//!
//! Under **M3** the read-back reports exactly the loss D-22 exists to close:
//! `Durable set: [aberp.duckdb, aberp.duckdb.audit.log]` — no WAL, and the
//! abandonment is gone from the DB while the mirror still carries it.
//!
//! # Why the measured write is the ONLY write on the handle
//!
//! `CheckpointDebouncer::should_checkpoint_now` returns `true` unconditionally
//! on a fresh `Handle`, so write #1 also fires an immediate D2 checkpoint. That
//! is not a problem here — it is the production shape: `aberp mark-abandoned`
//! is a one-shot process that opens a `Handle`, makes ONE money write and
//! exits.
//!
//! It is also what keeps the spec honest, and this took a wrong turn first. The
//! obvious design — burn a warm-up write to enter the debounce shadow, then
//! measure write #2 — is VACUOUS for a per-site mutation, because
//! `fsync_data_paths` journals `<db>.wal` on the FIRST guard drop and
//! [`power_loss_durable_set`] then copies that file whole. Once the WAL is in
//! the set, ANY later bytes in it ride along, `fsync`'d or not; the pre-D-22
//! posture measured that way came back GREEN. So the fixture writes the
//! `InvoiceSubmissionAttempt` precondition row through a plain connection and
//! CHECKPOINTs it before the handle exists. The journal is then empty, the
//! durable set is exactly `{DB, mirror}` — the RED baseline pinned by
//! [`d22_the_durable_set_is_empty_until_the_money_path_fsyncs`] — and the WAL
//! earns its place in it only if the measured write really did flush.
//!
//! # The NAV-gated siblings
//!
//! `submit-invoice`, `retry-submission`, `drain-submission-queue`,
//! `drain-pending-retries`, `poll-ack`, `poll-annulment-ack`,
//! `submit-annulment`, `observe-receiver-confirmation` and `recover-from-nav`
//! cannot be driven unattended (each needs the OS keychain and a live NAV wire
//! call between its writes). They carry the IDENTICAL
//! `{ db.write() … } db.durable_ack()?` pair this file exercises, on the same
//! `Handle`, and their coverage is the static census gate
//! `tools/cut_gate_durable_ack.sh` (CHECK D3-A/B/C) — the same division of
//! labour ADR-0110 already documents for modification / storno / the AP status
//! change.
//!
//! # Scope
//!
//! `$TMPDIR` only. Nothing here touches `~/.aberp/**`.

use std::path::{Path, PathBuf};

use aberp::mark_abandoned::{mark_abandoned_from_inputs, MarkAbandonedInputs};
use aberp_audit_ledger::{
    self as audit_ledger, Actor, BinaryHash, EventKind, Ledger, LedgerMeta, TenantId,
};
use aberp_billing::{
    self as billing, AllocateArgs, BillingStore, CustomerId, DraftInvoice, DuckDbBillingStore, Huf,
    IdempotencyKey, InvoiceId, InvoiceSeries, LineItem, ResetPolicy, SeriesCode, SeriesId,
};
use duckdb::Connection;
use time::OffsetDateTime;

const DB_FILE: &str = "aberp.duckdb";
const WAL_FILE: &str = "aberp.duckdb.wal";
const MIRROR_FILE: &str = "aberp.duckdb.audit.log";

fn tenant_id() -> TenantId {
    TenantId::new("tenant-d22-money-cli").expect("test tenant id is valid")
}

/// The binary hash the fixture seeds with is the one the production code will
/// compute for itself, so the seeded rows and the measured row agree.
fn binary_hash() -> BinaryHash {
    aberp::binary_hash::compute().expect("compute this test binary's hash")
}

fn tenant_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "aberp-d22-{}-{}-{:?}",
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

/// Seed one issued invoice — billing row, its `InvoiceSequenceReserved`
/// issuance entry, and the `InvoiceSubmissionAttempt` row that puts it in the
/// state-2 `Pending` stuck posture `mark-abandoned` refuses to run outside of —
/// and FOLD all of it into the main file with an explicit `CHECKPOINT`.
///
/// Seeded through a PLAIN connection, before any `Handle` exists, on purpose:
/// it leaves the durability journal empty and no WAL on disk, so the durable
/// set starts at exactly `{DB, mirror}` and the measured write is the only
/// thing that can put anything else in it. See the module docs.
///
/// Returns the prefixed invoice id — `mark-abandoned`'s F8 cross-check requires
/// the audit chain to carry the same idempotency key the billing row does, so
/// the seeded Attempt row reuses the allocation's key.
fn seed_stuck_invoice(db: &Path) -> String {
    let mut store = DuckDbBillingStore::open(db).expect("open billing store");
    store.ensure_schema().expect("ensure billing schema");
    let series = InvoiceSeries {
        id: SeriesId::new(),
        code: SeriesCode::new("D22".to_string()).expect("series code"),
        reset_policy: ResetPolicy::Never,
        fiscal_year: None,
        created_at: OffsetDateTime::now_utc(),
    };
    store.create_series(&series).expect("create series");
    let mut conn = store.into_connection();
    audit_ledger::ensure_schema(&conn).expect("ensure audit-ledger schema");

    let invoice_id = InvoiceId::new();
    let idempotency_key = IdempotencyKey::new();
    let meta = LedgerMeta::new(tenant_id(), binary_hash());
    {
        let tx = conn.transaction().expect("begin seed tx");
        billing::allocate_in_tx(
            &tx,
            AllocateArgs {
                series_id: series.id,
                draft: DraftInvoice {
                    id: invoice_id,
                    series_id: series.id,
                    customer_id: CustomerId::new(),
                    lines: vec![LineItem {
                        description: "D-22 durability fixture line".to_string(),
                        quantity: rust_decimal::Decimal::from(1),
                        unit_price: Huf(1_000),
                        vat_rate_basis_points: 2700,
                        vat_rate_kind: aberp_billing::VatRateKind::Percent,
                        note: None,
                        unit: None,
                    }],
                    issue_date: OffsetDateTime::now_utc(),
                    payment_deadline: OffsetDateTime::now_utc().date(),
                    delivery_date: OffsetDateTime::now_utc().date(),
                },
                idempotency_key,
                currency: aberp_billing::Currency::Huf,
                rate_metadata: None,
                bank_snapshot: None,
                invoice_note: None,
                email_recipient_override: None,
                start_value: 1,
                sequence_floor: None,
            },
            OffsetDateTime::now_utc(),
        )
        .expect("allocate_in_tx");
        audit_ledger::append_in_tx(
            &tx,
            &meta,
            EventKind::InvoiceSequenceReserved,
            br#"{"fixture":"d22-issuance"}"#.to_vec(),
            Actor::test_only(),
            Some(idempotency_key.to_canonical_string()),
        )
        .expect("append issuance entry");
        // The Attempt row is not filler: `audit_query::stuck_precondition`
        // reads it to classify the invoice as state-2 `Pending`, which is the
        // precondition `mark-abandoned` loud-fails without.
        let attempt = aberp::audit_payloads::InvoiceSubmissionAttemptPayload::new(
            &invoice_id.to_prefixed_string(),
            idempotency_key,
            "test",
            b"<fixture-request/>".to_vec(),
        );
        audit_ledger::append_in_tx(
            &tx,
            &meta,
            EventKind::InvoiceSubmissionAttempt,
            attempt.to_bytes(),
            Actor::test_only(),
            Some(idempotency_key.to_canonical_string()),
        )
        .expect("append InvoiceSubmissionAttempt");
        tx.commit().expect("commit seed tx");
    }
    conn.execute_batch("CHECKPOINT;")
        .expect("fold the seeded baseline into the main file");
    drop(conn);
    assert!(
        !db.with_extension("duckdb.wal").exists(),
        "fixture premise: the seed must leave NO WAL behind, or the durable set \
         does not start at {{DB, mirror}} and the spec measures nothing"
    );

    invoice_id.to_prefixed_string()
}

/// Drive the real D-22 library core with a fixed reason.
fn run_mark_abandoned(
    handle: &aberp_db::HandleArc,
    invoice_id: &str,
) -> anyhow::Result<aberp::mark_abandoned::MarkAbandonedOutcome> {
    mark_abandoned_from_inputs(MarkAbandonedInputs {
        db: handle,
        tenant: tenant_id(),
        actor: Actor::test_only(),
        invoice_id,
        reason: "D-22 power-loss durability spec",
        force_despite_nav_exists: false,
    })
}

/// The file names a power loss would leave behind: the two Defense already made
/// durable before D3, plus every path the write path actually `fsync`'d.
///
/// Derived from [`aberp_db::Handle::fsynced_paths`], never hard-coded — the
/// derivation IS the mutation test. Same construction as
/// `adr0110_d3_power_loss_durability.rs`, deliberately, so the two specs cannot
/// drift into measuring different things.
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
fn copy_on_disk_bytes(from: &Path, to: &Path, only: &[&str]) -> Vec<(String, u64)> {
    std::fs::create_dir_all(to).expect("mkdir copy tenant dir");
    let mut manifest = Vec::new();
    for entry in std::fs::read_dir(from).expect("read tenant dir") {
        let entry = entry.expect("dir entry");
        let src = entry.path();
        if !src.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if !only.contains(&name.as_str()) {
            continue;
        }
        let bytes = std::fs::copy(&src, to.join(&name)).expect("copy on-disk bytes");
        manifest.push((name, bytes));
    }
    manifest.sort();
    manifest
}

/// Count `InvoiceMarkedAbandoned` entries naming `invoice_id` in a ledger.
fn abandoned_entries_for(ledger: &Ledger, invoice_id: &str) -> usize {
    ledger
        .entries()
        .expect("read entries back")
        .iter()
        .filter(|e| e.kind == EventKind::InvoiceMarkedAbandoned)
        .filter(|e| {
            serde_json::from_slice::<aberp::audit_payloads::InvoiceMarkedAbandonedPayload>(
                &e.payload,
            )
            .map(|p| p.invoice_id == invoice_id)
            .unwrap_or(false)
        })
        .count()
}

// ══════════════════════════════════════════════════════════════════════
// 1 — the power loss
// ══════════════════════════════════════════════════════════════════════

/// **The D-22 spec.** The operator was told the invoice is ABANDONED. Cut the
/// power at that ack; the decision must come back out of what the filesystem
/// actually kept.
///
/// Pre-D-22 this is RED: `mark_abandoned::run` opened its own pragma-fenced
/// `Connection`, so no checkpoint ran and nothing `fsync`ed the DB or the WAL.
/// The `InvoiceMarkedAbandoned` row lived only in an un-`fsync`'d WAL, which is
/// absent from the durable set — while the MIRROR, explicitly `fsync`ed by the
/// old `sync_mirror` tail, carried it. That is mirror-ahead-of-DB: the exact
/// direction Defense's boot auto-heal resurrects from, and the reason the
/// read-back below deliberately goes through the copied **DB**, not the mirror.
#[test]
fn d22_mark_abandoned_survives_the_power_loss_durable_set() {
    let live = tenant_dir("markabandoned");
    let db = live.join(DB_FILE);
    let invoice_id = seed_stuck_invoice(&db);

    let handle = aberp_db::Handle::open_default(&db, tenant_id()).expect("shared Handle");

    // THE PRODUCTION PATH, inside the debounce shadow.
    let outcome = run_mark_abandoned(&handle, &invoice_id)
        .expect("mark_abandoned_from_inputs must succeed on a state-2 Pending invoice");
    // The core has returned: in the CLI this is the "mark-abandoned OK" line.
    // Cut the power.

    let copy = tenant_dir("markabandoned-copy");
    let durable = power_loss_durable_set(&handle);
    let only: Vec<&str> = durable.iter().map(String::as_str).collect();
    let manifest = copy_on_disk_bytes(&live, &copy, &only);

    let conn = boot_shaped_open(&copy.join(DB_FILE));
    let ledger = Ledger::from_connection(conn, tenant_id(), binary_hash());
    let recovered = abandoned_entries_for(&ledger, &invoice_id);

    assert_eq!(
        recovered, 1,
        "D-22 / ADR-0110 R1 VIOLATED on the mark-abandoned money path — the \
         operator was told invoice {invoice_id} is ABANDONED (a terminal decision \
         that burns the invoice sequence for good), and the power-loss durable \
         set does not have it back.\n\n\
         Durable set: {manifest:?}"
    );

    // Asserted AFTER the read-back on purpose. It is a sanity check on the
    // FIXTURE (three entries: issuance, Attempt, abandonment), and a fixture
    // sanity must never fire before the property under test — otherwise a
    // mutation that breaks durability could be reported as a broken fixture and
    // the read-back above would never run.
    assert!(
        outcome.entries_verified >= 3,
        "fixture sanity: the chain should carry the issuance, the Attempt and \
         the abandonment; got {}",
        outcome.entries_verified
    );
}

/// **The non-vacuity pin.** Before the money path runs, the derived durable set
/// is exactly the two files Defense already made durable before D3 — the RED
/// baseline — and afterwards it carries the WAL, *because the write path put it
/// there*.
///
/// This is what makes [`d22_mark_abandoned_survives_the_power_loss_durable_set`]
/// mean something. If the fixture ever leaked a `Handle` write before the
/// measured one (a warm-up, a schema touch, a stray `durable_ack`), the WAL
/// would already be in the set, the copy would carry it whole, and the
/// pre-D-22 posture would pass. That is not hypothetical: it is how the first
/// cut of this file was written, and it was green against a full revert.
#[test]
fn d22_the_durable_set_is_empty_until_the_money_path_fsyncs() {
    let live = tenant_dir("derivation");
    let db = live.join(DB_FILE);
    let invoice_id = seed_stuck_invoice(&db);

    let handle = aberp_db::Handle::open_default(&db, tenant_id()).expect("shared Handle");
    assert_eq!(
        power_loss_durable_set(&handle),
        vec![DB_FILE.to_string(), MIRROR_FILE.to_string()],
        "RED baseline broken: something flushed before the measured write, so \
         the power-loss spec is no longer measuring the money path"
    );

    run_mark_abandoned(&handle, &invoice_id).expect("mark_abandoned_from_inputs");

    let set = power_loss_durable_set(&handle);
    assert!(
        set.iter().any(|n| n == WAL_FILE),
        "D-22 REGRESSION: after the mark-abandoned ack the WAL is still not in \
         the derived durable set ({set:?}). The acked row lives in that WAL \
         until the next D2 checkpoint, so this is the un-fsynced posture with a \
         durable_ack call in front of it."
    );
    assert!(
        set.iter().any(|n| n == DB_FILE),
        "D-22 REGRESSION: the main DB file is not in the durability journal \
         ({set:?}) — the money path never flushed it"
    );
}

// ══════════════════════════════════════════════════════════════════════
// 2 — the fault injection the power-loss spec structurally cannot see
// ══════════════════════════════════════════════════════════════════════

/// **The ack mutation test.** Break the filesystem reach so the D3 flush
/// FAILS, and the money path must refuse the ack.
///
/// Deleting the *path* (not the inode) is the same hermetic injection
/// `crates/aberp-db/tests/durable_ack_fault_injection.rs` uses: the `Handle`'s
/// open `Connection` keeps its file descriptor, so every read and the commit
/// itself still succeed, and only the flush's fresh `File::open` of the path
/// sees `ENOENT`. That isolates exactly one thing — whether the money path
/// CLAIMS the parked outcome.
///
/// Delete the `db.durable_ack()?` line from `mark_abandoned_from_inputs` and
/// this returns `Ok`: the operator is told a terminal, sequence-burning
/// decision was recorded while nothing could make it durable, and the only
/// trace is a `tracing::error!` nobody reads. That is the 2026-08-08 posture
/// with a log line, and it is the mutation
/// [`d22_mark_abandoned_survives_the_power_loss_durable_set`] cannot catch —
/// the flush happens in `WriteGuard::drop`, so deleting the ack leaves the
/// bytes durable and that spec green.
#[test]
fn d22_mark_abandoned_refuses_to_ack_when_the_durable_flush_fails() {
    let live = tenant_dir("ack-fault");
    let db = live.join(DB_FILE);
    let invoice_id = seed_stuck_invoice(&db);

    let handle = aberp_db::Handle::open_default(&db, tenant_id()).expect("shared Handle");

    // Sanity FIRST: on an intact tenant this path acks cleanly. Without it the
    // assertion below could pass because the fixture is broken in some entirely
    // unrelated way.
    handle
        .durable_ack()
        .expect("precondition: an intact tenant must ack cleanly");

    // ── Break the reach ────────────────────────────────────────────────────
    std::fs::remove_file(&db).expect("remove the main DB file out from under the handle");

    let err = run_mark_abandoned(&handle, &invoice_id).expect_err(
        "D-22 REGRESSION: mark_abandoned_from_inputs returned Ok with the main DB \
         file DELETED — nothing could have been made durable, yet the operator \
         would be told the invoice is ABANDONED. The durable-ack outcome is being \
         discarded (ADR-0110 R3 / CLAUDE.md rule 11).",
    );
    let rendered = format!("{err:#}");
    assert!(
        rendered.contains("durable ack") || rendered.contains("durable-ack"),
        "the failure must be greppable as a DURABILITY fault, not surface as some \
         incidental DuckDB error — an operator has to know WHICH guarantee broke. \
         Got: {rendered}"
    );

    // B2 (ADR-0110) — and the mirror must NOT have run ahead. The guard drop
    // skips the mirror sync when the data flush fails precisely so a refused
    // ack cannot leave a durable mirror row that Defense's boot auto-heal
    // (`attempt_db_auto_recovery(mirror_ahead)` -> `replay_mirror_delta`) would
    // then RESURRECT into the DB.
    let mirror = std::fs::read_to_string(live.join(MIRROR_FILE)).unwrap_or_default();
    assert!(
        !mirror.contains("InvoiceMarkedAbandoned"),
        "ADR-0110 B2 VIOLATED on a D-22 path: the ack FAILED, so the operator is \
         told the abandonment did not happen — but the mirror durably records \
         that it did, and boot's mirror-ahead auto-heal would replay it back in.\n\n\
         Mirror:\n{mirror}"
    );
}
