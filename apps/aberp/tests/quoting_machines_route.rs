//! S427 — integration pins for `quoting_machines` master-data CRUD,
//! the machine/lead-time audit emission, and the capacity-aware
//! lead-time wiring end-to-end (quote shop-load → engine → lead_time
//! written → effective value the PDF banner reads).
//!
//! Library-helper boundary (mirrors `serve_partners_route.rs`): the
//! HTTPS listener is not spun; the `*_request` helpers carry the full
//! validate → DB-write → audit-emit path the route handlers call.

use std::path::PathBuf;
use std::sync::Arc;

use aberp_audit_ledger::{BinaryHash, EventKind, Ledger, TenantId};
use ulid::Ulid;

use aberp::quoting_machines::MachineInputs;
use aberp::serve::{self, AppState, MachineRouteError};

const TEST_TENANT: &str = "quoting_machines_route_test";
const TEST_HASH: BinaryHash = BinaryHash::from_bytes([0xAB; 32]);

fn test_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("aberp-machines-route")
        .join(format!("{}-{}", label, Ulid::new()));
    std::fs::create_dir_all(&dir).expect("create test dir");
    dir
}

fn build_state(db_path: PathBuf) -> AppState {
    let tenant = TenantId::new(TEST_TENANT.to_string()).expect("tenant id");
    AppState {
        db_path: Arc::new(db_path),
        tenant,
        binary_hash: aberp::binary_hash::BinaryHashHandle::from_ready(TEST_HASH),
        session_token: Arc::new("test-token".to_string()),
        secrets_cache: aberp::secrets_cache::SecretsCache::empty(),
        nav_poll_semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(
            aberp::serve::NAV_POLL_DAEMON_CONCURRENCY,
        )),
        boot_state: Arc::new(std::sync::RwLock::new(
            aberp::serve::ServeBootState::Ready {
                operator_login: "test-operator".to_string(),
            },
        )),
        shutdown_token: tokio_util::sync::CancellationToken::new(),
        adapter_registry: Arc::new(std::sync::RwLock::new(aberp_mes::AdapterRegistry::new())),
        adapter_manager: Arc::new(aberp::mes_manager::AdapterManager::new(
            Arc::new(std::sync::RwLock::new(aberp_mes::AdapterRegistry::new())),
            tokio_util::sync::CancellationToken::new(),
        )),
        adapter_health_baseline: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        restore_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        catalogue_push: aberp::catalogue_push::CataloguePushHandle::dormant(),
        email_relay_rate_limiter: std::sync::Arc::new(aberp::email_relay::RateLimiter::new()),
        pipeline_python_resolution: aberp::quote_pricing_pipeline::PythonResolutionHandle::dormant(
        ),
        storefront_credential: aberp::storefront_credential::StorefrontCredentialHandle::dormant(),
        email_outbox_daemon: aberp::email_outbox_poll_daemon::EmailOutboxDaemonHandle::dormant(),
        quote_pdf_rerender_queue: aberp::quote_pdf_rerender_queue::QuotePdfRerenderQueue::new(),
        digital_id: std::sync::Arc::new(aberp_digital_id::MockProvider::new()),
    }
}

fn inputs(name: &str, family: &str, daily: f64, buffer: f64, enabled: bool) -> MachineInputs {
    MachineInputs {
        name: name.to_string(),
        family: family.to_string(),
        max_envelope_xyz_mm: [500.0, 400.0, 300.0],
        daily_hours_avail: daily,
        buffer_pct: buffer,
        enabled,
    }
}

/// Every entry kind currently in the ledger (read-back for audit pins).
fn ledger_kinds(db_path: &PathBuf) -> Vec<EventKind> {
    let tenant = TenantId::new(TEST_TENANT.to_string()).expect("tenant id");
    let ledger = Ledger::open(db_path, tenant, TEST_HASH).expect("open ledger");
    ledger
        .entries()
        .expect("read entries")
        .into_iter()
        .map(|e| e.kind)
        .collect()
}

// ── CRUD smoke + archive-not-delete ──────────────────────────────────

#[test]
fn crud_smoke_create_list_update_archive() {
    let dir = test_dir("crud");
    let db = dir.join("aberp.duckdb");
    let state = build_state(db.clone());

    // create
    let m = serve::create_machine_request(
        &state,
        &inputs("DMG MORI 1", "3-axis-mill", 16.0, 20.0, true),
        "op",
        TEST_HASH,
    )
    .expect("create");
    assert!(m.id.starts_with("qcm_"), "server-minted id: {}", m.id);
    assert_eq!(m.family, "3-axis-mill");
    assert!(m.archived_at.is_none());

    // list sees it
    let list = serve::list_machines_request(&state).expect("list");
    assert_eq!(list.len(), 1);

    // update flips enabled + family
    let updated = serve::update_machine_request(
        &state,
        &m.id,
        &inputs("DMG MORI 1", "5-axis-mill", 20.0, 10.0, false),
        "op",
        TEST_HASH,
    )
    .expect("update");
    assert_eq!(updated.family, "5-axis-mill");
    assert!(!updated.enabled);

    // archive (soft) — NOT hard delete
    serve::archive_machine_request(&state, &m.id, "op", TEST_HASH).expect("archive");

    // archive-not-delete invariant: row gone from the active list, but
    // still readable by id with archived_at stamped.
    let after = serve::list_machines_request(&state).expect("list after archive");
    assert!(after.is_empty(), "archived row excluded from active list");
    let got = serve::get_machine_request(&state, &m.id).expect("get archived");
    assert!(
        got.archived_at.is_some(),
        "archived_at stamped, row preserved"
    );

    // re-archiving an already-archived row is a NotFound (idempotent-safe).
    assert!(matches!(
        serve::archive_machine_request(&state, &m.id, "op", TEST_HASH),
        Err(MachineRouteError::NotFound)
    ));
}

#[test]
fn create_validation_rejects_bad_family_and_hours() {
    let dir = test_dir("validate");
    let state = build_state(dir.join("aberp.duckdb"));
    let err = serve::create_machine_request(
        &state,
        &inputs("bad", "spindle-of-doom", 99.0, 150.0, true),
        "op",
        TEST_HASH,
    )
    .expect_err("must reject");
    match err {
        MachineRouteError::Validation(fields) => {
            let names: Vec<&str> = fields.iter().map(|f| f.field).collect();
            assert!(names.contains(&"family"), "family error: {names:?}");
            assert!(
                names.contains(&"daily_hours_avail"),
                "hours error: {names:?}"
            );
            assert!(names.contains(&"buffer_pct"), "buffer error: {names:?}");
        }
        other => panic!("expected Validation, got {other:?}"),
    }
}

// ── Audit emission: the 3 machine kinds on the right operations ───────

#[test]
fn machine_crud_emits_the_three_event_kinds() {
    let dir = test_dir("audit");
    let db = dir.join("aberp.duckdb");
    let state = build_state(db.clone());

    let m = serve::create_machine_request(
        &state,
        &inputs("Hermle C42", "5-axis-mill", 16.0, 20.0, true),
        "op",
        TEST_HASH,
    )
    .expect("create");
    serve::update_machine_request(
        &state,
        &m.id,
        &inputs("Hermle C42", "5-axis-mill", 18.0, 25.0, true),
        "op",
        TEST_HASH,
    )
    .expect("update");
    serve::archive_machine_request(&state, &m.id, "op", TEST_HASH).expect("archive");

    let kinds = ledger_kinds(&db);
    assert!(
        kinds.contains(&EventKind::MachineCreated),
        "created: {kinds:?}"
    );
    assert!(
        kinds.contains(&EventKind::MachineEdited),
        "edited: {kinds:?}"
    );
    assert!(
        kinds.contains(&EventKind::MachineArchived),
        "archived: {kinds:?}"
    );
}

// ── list_enabled_capacities excludes disabled + archived ─────────────

#[test]
fn enabled_capacities_excludes_disabled_and_archived() {
    let dir = test_dir("caps");
    let db = dir.join("aberp.duckdb");
    let state = build_state(db.clone());

    let on = serve::create_machine_request(
        &state,
        &inputs("on", "3-axis-mill", 16.0, 20.0, true),
        "op",
        TEST_HASH,
    )
    .expect("on");
    serve::create_machine_request(
        &state,
        &inputs("off", "lathe", 16.0, 20.0, false),
        "op",
        TEST_HASH,
    )
    .expect("off");
    let arch = serve::create_machine_request(
        &state,
        &inputs("arch", "grinder", 16.0, 20.0, true),
        "op",
        TEST_HASH,
    )
    .expect("arch");
    serve::archive_machine_request(&state, &arch.id, "op", TEST_HASH).expect("archive");

    let conn = duckdb::Connection::open(&db).expect("open");
    let caps = aberp::quoting_machines::list_enabled_capacities(&conn, TEST_TENANT).expect("caps");
    assert_eq!(
        caps.len(),
        1,
        "only the enabled, non-archived machine: {caps:?}"
    );
    assert_eq!(
        caps[0].family,
        aberp_quote_engine::MachineFamily::ThreeAxisMill
    );
    let _ = on;
}

// ── E2e: shop-load → engine → lead_time written → effective value ────

fn fixed_ts() -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp(1_780_000_000).unwrap()
}

/// Insert a `Posted` priced job whose breakdown carries machining hours
/// (per part) + family, with `qty` parts, so it counts toward the shop
/// load as `machining_minutes × qty`.
fn seed_posted(db: &PathBuf, quote_id: &str, machining_minutes: f64, qty: u32, route_5axis: bool) {
    let conn = duckdb::Connection::open(db).expect("open");
    aberp::quote_pricing_jobs::insert_fetched_job(
        &conn,
        quote_id,
        TEST_TENANT,
        "c@example.com",
        "Cust Kft.",
        "Cust Kft.",
        "6061-T6",
        qty,
        "p.step",
        "/tmp/p.step",
        fixed_ts(),
    )
    .expect("insert");
    let breakdown =
        format!("{{\"machining_minutes\":{machining_minutes},\"route_to_5_axis\":{route_5axis}}}");
    conn.execute(
        "UPDATE quote_pricing_jobs SET state = 'posted', breakdown_json = ? WHERE quote_id = ? AND tenant_id = ?",
        duckdb::params![breakdown, quote_id, TEST_TENANT],
    )
    .expect("post");
}

#[test]
fn capacity_wiring_writes_lead_time_and_effective_reads_it() {
    let dir = test_dir("e2e");
    let db = dir.join("aberp.duckdb");
    let state = build_state(db.clone());

    // One 3-axis mill: 16h × 80% = 12.8 h/day schedulable.
    serve::create_machine_request(
        &state,
        &inputs("mill", "3-axis-mill", 16.0, 20.0, true),
        "op",
        TEST_HASH,
    )
    .expect("machine");

    // Existing shop load: 360 min/part on 3-axis. Job A is a batch of 2
    // (6h × 2 = 12h), job B a single part (6h) → 18h total. The qty
    // multiplication is what makes A count double.
    seed_posted(&db, "11111111-1111-1111-1111-111111111111", 360.0, 2, false);
    seed_posted(&db, "22222222-2222-2222-2222-222222222222", 360.0, 1, false);
    // The job being priced now (not Posted, excluded from the load sum).
    let target = "33333333-3333-3333-3333-333333333333";
    seed_posted(&db, target, 0.0, 1, false); // exists so set_computed can stamp it
    let conn = duckdb::Connection::open(&db).expect("open");
    conn.execute(
        "UPDATE quote_pricing_jobs SET state = 'rendering' WHERE quote_id = ? AND tenant_id = ?",
        duckdb::params![target, TEST_TENANT],
    )
    .expect("flip target");

    // Gather existing load (a wide window so both posted rows count).
    let existing = aberp::quote_pricing_jobs::sum_posted_machining_hours_by_family(
        &conn,
        TEST_TENANT,
        "2000-01-01T00:00:00Z",
        target,
    )
    .expect("sum");
    let three = aberp_quote_engine::MachineFamily::ThreeAxisMill;
    assert!(
        (existing.get(&three).copied().unwrap_or(0.0) - 18.0).abs() < 1e-9,
        "existing load must be 18h (12h batch-of-2 + 6h single), got {existing:?}"
    );

    // New quote: 14h of 3-axis machining. (18 existing + 14) / 12.8 = ceil(2.5) = 3.
    let machines =
        aberp::quoting_machines::list_enabled_capacities(&conn, TEST_TENANT).expect("caps");
    let mut new_hours = std::collections::BTreeMap::new();
    new_hours.insert(three, 14.0);
    let est = aberp_quote_engine::lead_time_days(&machines, &existing, &new_hours);
    assert_eq!(est.days, 3, "ceil((12+14)/12.8) = 3");
    assert!(!est.used_fallback);

    aberp::quote_pricing_jobs::set_computed_lead_time(&conn, target, TEST_TENANT, est.days)
        .expect("stamp");
    let eff = aberp::quote_pricing_jobs::get_effective_lead_time_days(&conn, target, TEST_TENANT)
        .expect("effective");
    assert_eq!(eff, Some(3), "computed value surfaces as effective");
}

// ── Override: persists + QuoteLeadTimeOverridden emitted ─────────────

#[test]
fn override_persists_and_emits_event() {
    let dir = test_dir("override");
    let db = dir.join("aberp.duckdb");
    let state = build_state(db.clone());
    let qid = "44444444-4444-4444-4444-444444444444";
    seed_posted(&db, qid, 600.0, 1, false);
    {
        // Give it a computed value first so the override payload's
        // computed_days is populated.
        let conn = duckdb::Connection::open(&db).expect("open");
        aberp::quote_pricing_jobs::set_computed_lead_time(&conn, qid, TEST_TENANT, 4)
            .expect("comp");
    }

    // Operator overrides to 10 days.
    serve::override_lead_time_request(&state, qid, Some(10), "op", TEST_HASH).expect("override");

    let conn = duckdb::Connection::open(&db).expect("open");
    let eff = aberp::quote_pricing_jobs::get_effective_lead_time_days(&conn, qid, TEST_TENANT)
        .expect("effective");
    assert_eq!(eff, Some(10), "override wins over computed");

    assert!(
        ledger_kinds(&db).contains(&EventKind::QuoteLeadTimeOverridden),
        "override event emitted"
    );

    // Clearing the override reverts to the computed value.
    serve::override_lead_time_request(&state, qid, None, "op", TEST_HASH).expect("clear");
    let conn2 = duckdb::Connection::open(&db).expect("open");
    let eff2 = aberp::quote_pricing_jobs::get_effective_lead_time_days(&conn2, qid, TEST_TENANT)
        .expect("effective2");
    assert_eq!(eff2, Some(4), "cleared override → computed value");
}

#[test]
fn override_on_missing_job_is_not_found() {
    let dir = test_dir("override-404");
    let state = build_state(dir.join("aberp.duckdb"));
    assert!(matches!(
        serve::override_lead_time_request(&state, "no-such-quote", Some(5), "op", TEST_HASH),
        Err(MachineRouteError::NotFound)
    ));
}
