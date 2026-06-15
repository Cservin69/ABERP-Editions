//! S428 — integration pins for customer-type margin profiles:
//!   - profile CRUD via the serve `*_request` helpers + the three
//!     `quote.margin_profile_*` audit kinds,
//!   - the unique-active-per-type 409,
//!   - the partner `customer_type` change → `PartnerCustomerTypeChanged`,
//!   - the customer-journey e2e: partner → profile → quote → re-price →
//!     margin-floor → DEAL refused (`below_margin_floor`),
//!   - the operator margin-override confirm flow + its two audit kinds.
//!
//! Library-helper boundary (mirrors `quoting_machines_route.rs`): the
//! HTTPS listener is not spun; the `*_request` helpers carry the full
//! validate → DB-write → audit-emit path the route handlers call.

use std::path::PathBuf;
use std::sync::Arc;

use aberp_audit_ledger::{Actor, BinaryHash, EventKind, Ledger, LedgerMeta, TenantId};
use aberp_quote_engine::FeatureGraph;
use ulid::Ulid;

use aberp::margin_profiles::MarginProfileInputs;
use aberp::nav_xml::CustomerVatStatus;
use aberp::partners::{CustomerType, PartnerInputs, PartnerKind};
use aberp::quote_deal::{run_deal_saga, DealSagaError, DealSagaInputs};
use aberp::serve::{
    self, AppState, BuyerPartnerBody, MarginOverrideBody, MarginProfileRouteError, QuoteMarginError,
};
use aberp_quote_intake::log_table;

const TEST_TENANT: &str = "s428_margin_profiles_test";
const TEST_HASH: BinaryHash = BinaryHash::from_bytes([0xC4; 32]);

fn test_dir(label: &str) -> PathBuf {
    let dir =
        std::env::temp_dir()
            .join("aberp-s428-margin")
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

fn profile_inputs(ct: &str, gross: f64, min: f64) -> MarginProfileInputs {
    MarginProfileInputs {
        name: format!("{ct} profile"),
        customer_type: ct.to_string(),
        gross_margin_pct: gross,
        min_margin_pct: min,
        notes: None,
        enabled: true,
    }
}

fn partner_inputs(name: &str, ct: CustomerType) -> PartnerInputs {
    PartnerInputs {
        display_name: name.to_string(),
        legal_name: format!("{name} Kft."),
        kind: PartnerKind::Customer,
        customer_vat_status: CustomerVatStatus::Domestic,
        customer_type: ct,
        tax_number: Some("12345678-1-42".to_string()),
        eu_vat_number: None,
        address_street: None,
        address_postal_code: None,
        address_city: None,
        address_country: None,
        bank_account: None,
        contact_email: None,
        contact_phone: None,
    }
}

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

// ── CRUD + the three margin_profile audit kinds + duplicate 409 ──────

#[test]
fn profile_crud_emits_three_event_kinds_and_blocks_duplicate() {
    let dir = test_dir("crud");
    let db = dir.join("aberp.duckdb");
    let state = build_state(db.clone());

    let p = serve::create_margin_profile_request(
        &state,
        &profile_inputs("defense", 0.4, 0.1),
        "op",
        TEST_HASH,
    )
    .expect("create");
    assert!(p.id.starts_with("mp_"), "server-minted id: {}", p.id);

    // unique-active-per-type → 409.
    let dup = serve::create_margin_profile_request(
        &state,
        &profile_inputs("defense", 0.5, 0.2),
        "op",
        TEST_HASH,
    );
    assert!(matches!(
        dup,
        Err(MarginProfileRouteError::DuplicateActiveType)
    ));

    // update + archive.
    serve::update_margin_profile_request(
        &state,
        &p.id,
        &profile_inputs("defense", 0.45, 0.12),
        "op",
        TEST_HASH,
    )
    .expect("update");
    serve::archive_margin_profile_request(&state, &p.id, "op", TEST_HASH).expect("archive");

    // archive-not-delete: gone from active list, still readable by id.
    assert!(serve::list_margin_profiles_request(&state)
        .expect("list")
        .is_empty());

    let kinds = ledger_kinds(&db);
    assert!(
        kinds.contains(&EventKind::MarginProfileCreated),
        "{kinds:?}"
    );
    assert!(kinds.contains(&EventKind::MarginProfileEdited), "{kinds:?}");
    assert!(
        kinds.contains(&EventKind::MarginProfileArchived),
        "{kinds:?}"
    );
}

// ── Partner customer_type change fires PartnerCustomerTypeChanged ─────

#[test]
fn partner_customer_type_change_emits_audit() {
    let dir = test_dir("partner-audit");
    let db = dir.join("aberp.duckdb");
    let state = build_state(db.clone());

    let created =
        serve::create_partner_request(&state, &partner_inputs("Acme", CustomerType::Unset))
            .expect("create partner");

    // No-op edit (same customer_type) must NOT fire the audit.
    serve::update_partner_request(
        &state,
        &created.id,
        &partner_inputs("Acme", CustomerType::Unset),
        "op",
        TEST_HASH,
    )
    .expect("noop update");
    assert!(
        !ledger_kinds(&db).contains(&EventKind::PartnerCustomerTypeChanged),
        "no-op edit must not fire the customer-type audit"
    );

    // Changing the customer_type fires it.
    serve::update_partner_request(
        &state,
        &created.id,
        &partner_inputs("Acme", CustomerType::Defense),
        "op",
        TEST_HASH,
    )
    .expect("type change");
    assert!(
        ledger_kinds(&db).contains(&EventKind::PartnerCustomerTypeChanged),
        "customer-type change must fire the audit"
    );
}

// ── E2e: partner → profile → quote → re-price → DEAL refused ─────────

fn fixed_ts() -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp(1_780_000_000).unwrap()
}

fn sample_feature_graph_json() -> String {
    let g = FeatureGraph {
        schema_version: FeatureGraph::SCHEMA_VERSION,
        bounding_box_mm: [50.0, 30.0, 20.0],
        volume_mm3: 12_345.6,
        surface_area_mm2: 6_200.0,
        material_grade: "6061-T6".to_string(),
        // Empty feature list: geometry-driven machining (roughing /
        // finishing) needs no per-feature complexity rules, so the engine
        // prices without a seeded `quoting_complexity_rules` table.
        features: vec![],
        requires_5_axis: false,
        thin_wall_present: false,
    };
    serde_json::to_string(&g).expect("encode FG")
}

/// Seed the catalogue + one extracted job (with a feature graph) so the
/// re-price engine call succeeds.
fn seed_job(db: &PathBuf, quote_id: &str) {
    let mut conn = duckdb::Connection::open(db).expect("open");
    aberp::quoting_tunables::ensure_schema(&mut conn, TEST_TENANT).expect("tunables");
    aberp::quoting_materials::seed_if_empty(&mut conn, TEST_TENANT).expect("materials");
    aberp::quote_pricing_jobs::insert_fetched_job(
        &conn,
        quote_id,
        TEST_TENANT,
        "buyer@example.com",
        "Buyer Kft.",
        "Buyer Kft.",
        "6061-T6",
        1,
        "p.step",
        "/tmp/p.step",
        fixed_ts(),
    )
    .expect("insert job");
    conn.execute(
        "UPDATE quote_pricing_jobs SET state = 'rendering', feature_graph_json = ? \
         WHERE quote_id = ? AND tenant_id = ?",
        duckdb::params![sample_feature_graph_json(), quote_id, TEST_TENANT],
    )
    .expect("set feature graph");
}

fn stage_intake(db: &PathBuf, quote_id: &str) {
    let conn = duckdb::Connection::open(db).expect("open");
    log_table::insert_intake(
        &conn,
        TEST_TENANT,
        quote_id,
        "inv_x",
        "2026-06-10T08:00:00Z",
        fixed_ts(),
        "{}",
        "{}",
    )
    .expect("stage intake");
}

#[test]
fn customer_journey_partner_profile_quote_margin_floor_refuse() {
    let dir = test_dir("e2e");
    let db = dir.join("aberp.duckdb");
    let state = build_state(db.clone());

    // 1. partner-create (defense).
    let partner =
        serve::create_partner_request(&state, &partner_inputs("Aegis", CustomerType::Defense))
            .expect("partner");

    // 2. margin-profile-create — target BELOW the floor so any priced
    //    quote for this segment trips the floor.
    serve::create_margin_profile_request(
        &state,
        &profile_inputs("defense", 0.04, 0.10),
        "op",
        TEST_HASH,
    )
    .expect("profile");

    // 3. quote-create (an extracted job) + assign the buyer → re-price.
    let quote_id = "5a5a5a5a-5a5a-5a5a-5a5a-5a5a5a5a5a5a";
    seed_job(&db, quote_id);
    serve::set_quote_buyer_partner_request(
        &state,
        quote_id,
        BuyerPartnerBody {
            partner_id: Some(partner.id.clone()),
        },
        "op",
        TEST_HASH,
    )
    .expect("assign buyer + reprice");

    // The re-price flagged the job below the floor + emitted the event.
    let conn = duckdb::Connection::open(&db).expect("open");
    assert!(
        aberp::quote_pricing_jobs::margin_below_floor(&conn, quote_id, TEST_TENANT).expect("flag"),
        "defense profile (gross 4% < floor 10%) must trip the floor"
    );
    assert!(
        ledger_kinds(&db).contains(&EventKind::QuoteMarginBelowFloor),
        "below-floor event must fire on the re-price"
    );

    // 4. margin-floor-refuse: the DEAL saga is hard-blocked.
    stage_intake(&db, quote_id);
    let mut saga_conn = duckdb::Connection::open(&db).expect("open saga conn");
    let meta = LedgerMeta::new(TenantId::new(TEST_TENANT).unwrap(), TEST_HASH);
    let err = run_deal_saga(
        &mut saga_conn,
        &meta,
        Actor::from_local_cli(Ulid::new().to_string(), "op"),
        DealSagaInputs {
            tenant: TEST_TENANT.to_string(),
            quote_id: quote_id.to_string(),
            actor: "op".to_string(),
            deal_token: quote_id[..8].to_string(),
            refresh_ack: None,
        },
    )
    .expect_err("DEAL must be refused below floor");
    let saga = err.downcast::<DealSagaError>().expect("typed saga error");
    assert_eq!(saga.machine_code(), "below_margin_floor");
}

// ── Operator margin override: below-floor needs confirm + 2 audit kinds ─

#[test]
fn margin_override_below_floor_needs_confirmation() {
    let dir = test_dir("override");
    let db = dir.join("aberp.duckdb");
    let state = build_state(db.clone());

    // A job with NO buyer (global default ~26% realized) prices fine.
    let quote_id = "6b6b6b6b-6b6b-6b6b-6b6b-6b6b6b6b6b6b";
    seed_job(&db, quote_id);

    // An aggressive 1% markup override → realized ≈ 1% < global floor 10%.
    let unconfirmed = serve::override_quote_margin_request(
        &state,
        quote_id,
        MarginOverrideBody {
            margin_pct: Some(0.01),
            confirm_below_floor: false,
            reason: None,
        },
        "op",
        TEST_HASH,
    );
    assert!(
        matches!(
            unconfirmed,
            Err(QuoteMarginError::BelowFloorNeedsConfirm { .. })
        ),
        "below-floor override without confirm must be refused"
    );
    // ...and nothing was persisted.
    let conn = duckdb::Connection::open(&db).expect("open");
    assert!(
        !aberp::quote_pricing_jobs::margin_below_floor(&conn, quote_id, TEST_TENANT).expect("flag"),
        "rejected override must not flag the job"
    );

    // Confirming proceeds: persists + flags + fires BOTH audit kinds.
    serve::override_quote_margin_request(
        &state,
        quote_id,
        MarginOverrideBody {
            margin_pct: Some(0.01),
            confirm_below_floor: true,
            reason: Some("strategic loss-leader".to_string()),
        },
        "op",
        TEST_HASH,
    )
    .expect("confirmed override");
    let conn2 = duckdb::Connection::open(&db).expect("open");
    assert!(
        aberp::quote_pricing_jobs::margin_below_floor(&conn2, quote_id, TEST_TENANT).expect("flag"),
        "confirmed below-floor override flags the job"
    );
    let kinds = ledger_kinds(&db);
    assert!(
        kinds.contains(&EventKind::QuoteMarginOverridden),
        "{kinds:?}"
    );
    assert!(
        kinds.contains(&EventKind::QuoteMarginFloorOverridden),
        "{kinds:?}"
    );
}
