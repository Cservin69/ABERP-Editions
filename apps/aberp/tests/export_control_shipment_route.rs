//! S440 — route-layer pins for the `export.*` export-control firing sites.
//!
//! The three `export.*` kinds themselves fire inside
//! `aberp_dispatch::mark_shipped` and are pinned there (see
//! `crates/aberp-dispatch/tests/dispatch_round_trip.rs`, the `s440_*` block).
//! What lives in `serve.rs` — and is pinned HERE — is the wiring around them:
//!
//! 1. [`serve::resolve_export_recipient`] — the consignee the gate screens and
//!    the shipment row records, resolved from the app-owned `partners` table.
//! 2. [`serve::append_export_access_denied`] — the standalone denial append. A
//!    refused screening rolls the ship transaction back, taking any in-tx
//!    denial row with it, so the denial has to be recorded outside that tx.
//! 3. [`serve::export_control_provider`] — production is honestly the mock.
//!
//! Library-helper boundary (mirrors `avl_vendors_route.rs`): no HTTPS listener.

use std::path::PathBuf;
use std::sync::Arc;

use aberp_audit_ledger::{BinaryHash, EventKind, Ledger, TenantId};
use ulid::Ulid;

use aberp::serve::{self, AppState};

const TEST_TENANT: &str = "export_control_route_test";
const TEST_HASH: BinaryHash = BinaryHash::from_bytes([0xE7; 32]);

fn test_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("aberp-export-route")
        .join(format!("{}-{}", label, Ulid::new()));
    std::fs::create_dir_all(&dir).expect("create test dir");
    dir
}

fn build_state(db_path: PathBuf) -> AppState {
    let tenant = TenantId::new(TEST_TENANT.to_string()).expect("tenant id");
    AppState {
        db: aberp::serve::open_tenant_handle(&db_path, tenant.clone())
            .expect("open shared test DuckDB handle (ADR-0098 Gap 1a)"),
        db_path: Arc::new(db_path),
        tenant,
        nav_enabled: true,
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

/// Insert a partner + a Drafted dispatch row directly. The point of these
/// tests is the serve-layer resolution/append, not the WO lifecycle (which the
/// dispatch crate's own fixture exercises end-to-end).
fn seed_partner_and_dispatch(
    state: &AppState,
    partner_id: &str,
    legal_name: &str,
    country: Option<&str>,
) -> String {
    let dsp_id = format!("dsp_{}", Ulid::new());
    let conn = state.db.write().expect("writer");
    aberp::partners::ensure_schema(&conn).expect("partners schema");
    aberp_dispatch::ensure_schema(&conn).expect("dispatch schema");
    conn.execute(
        "INSERT INTO partners (id, tenant_id, display_name, legal_name, kind, \
         address_country, customer_vat_status, issued_invoice_count, created_at, updated_at) \
         VALUES (?, ?, ?, ?, 'Customer', ?, 'Domestic', 0, ?, ?);",
        duckdb::params![
            partner_id,
            TEST_TENANT,
            legal_name,
            legal_name,
            country,
            "2026-08-09T00:00:00Z",
            "2026-08-09T00:00:00Z",
        ],
    )
    .expect("insert partner");
    conn.execute(
        "INSERT INTO dispatches (dsp_id, tenant_id, wo_id, partner_id, state, created_at) \
         VALUES (?, ?, 'wo_seed', ?, 'drafted', ?);",
        duckdb::params![&dsp_id, TEST_TENANT, partner_id, "2026-08-09T00:00:00Z",],
    )
    .expect("insert dispatch");
    dsp_id
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

/// The consignee handed to the export-control gate is the partner's LEGAL
/// name (the name a denied-party list is keyed on — not the display name) and
/// its country, normalised to upper-case alpha-2 at the boundary so `"de"` and
/// `"DE"` screen identically.
#[test]
fn resolve_export_recipient_uses_legal_name_and_upper_cases_the_country() {
    let dir = test_dir("recipient");
    let db = dir.join("aberp.duckdb");
    let state = build_state(db);
    let dsp_id = seed_partner_and_dispatch(&state, "ptr_acme", "ACME Aerospace GmbH", Some("de"));

    let party = serve::resolve_export_recipient(&state, &dsp_id).expect("resolve");
    assert_eq!(party.name, "ACME Aerospace GmbH");
    assert_eq!(party.country.as_deref(), Some("DE"));
}

/// A partner with no recorded country yields `None` — the shipment payload
/// records an empty destination rather than a guessed one. Fabricating a
/// destination on an export record is worse than a visibly incomplete one.
#[test]
fn resolve_export_recipient_leaves_an_unrecorded_country_unset() {
    let dir = test_dir("no-country");
    let db = dir.join("aberp.duckdb");
    let state = build_state(db);
    let dsp_id = seed_partner_and_dispatch(&state, "ptr_nocountry", "Nowhere Kft", None);

    let party = serve::resolve_export_recipient(&state, &dsp_id).expect("resolve");
    assert_eq!(party.country, None);

    // An empty-string country is the same "unknown", not a country named "".
    let dsp_id = seed_partner_and_dispatch(&state, "ptr_blank", "Blank Kft", Some("   "));
    let party = serve::resolve_export_recipient(&state, &dsp_id).expect("resolve");
    assert_eq!(party.country, None);
}

/// A dispatch whose partner row is missing still gets SCREENED — on the id we
/// do have. A silent skip of the screen is precisely the failure mode this
/// gate exists to prevent, so the degraded path must never return "no party".
#[test]
fn resolve_export_recipient_still_names_a_party_when_the_partner_row_is_missing() {
    let dir = test_dir("orphan");
    let db = dir.join("aberp.duckdb");
    let state = build_state(db);
    let dsp_id = seed_partner_and_dispatch(&state, "ptr_gone", "Gone Kft", Some("HU"));
    {
        let conn = state.db.write().expect("writer");
        conn.execute(
            "DELETE FROM partners WHERE tenant_id = ? AND id = ?;",
            duckdb::params![TEST_TENANT, "ptr_gone"],
        )
        .expect("delete partner");
    }

    let party = serve::resolve_export_recipient(&state, &dsp_id).expect("resolve");
    assert_eq!(
        party.name, "ptr_gone",
        "an orphaned dispatch is screened on its partner id, never skipped"
    );
    assert_eq!(party.country, None);
}

/// The denial append is the half of `export.access_check` that CANNOT live in
/// the ship transaction: a refused screening rolls that transaction back, so
/// an in-tx denial row would vanish with it. This pins that the standalone
/// append lands exactly one `export.access_check` row, on the shared Handle,
/// carrying `decision = "denied"` and the refusing reason.
#[test]
fn append_export_access_denied_records_the_refusal_outside_the_ship_tx() {
    let dir = test_dir("denial");
    let db = dir.join("aberp.duckdb");
    let state = build_state(db.clone());
    let dsp_id = seed_partner_and_dispatch(&state, "ptr_denied", "Sanctioned OOO", Some("RU"));

    assert!(
        ledger_kinds(&db).is_empty(),
        "fixture must start with an empty ledger"
    );

    serve::append_export_access_denied(
        &state,
        &dsp_id,
        "test-operator",
        "denied-party screening: denied (BIS Entity List)",
    );

    let kinds = ledger_kinds(&db);
    assert_eq!(
        kinds,
        vec![EventKind::ExportAccessCheck],
        "exactly one export.access_check row, nothing else"
    );

    let tenant = TenantId::new(TEST_TENANT.to_string()).expect("tenant id");
    let ledger = Ledger::open(&db, tenant, TEST_HASH).expect("open ledger");
    let entry = ledger.entries().expect("entries").pop().expect("one entry");
    let payload: aberp_dispatch::ExportAccessCheckPayload =
        serde_json::from_slice(&entry.payload).expect("denial payload parses");
    assert_eq!(payload.decision, "denied");
    assert_eq!(payload.entity_kind, "dispatch");
    assert_eq!(payload.entity_id, dsp_id);
    assert_eq!(payload.operator_user_id, "test-operator");
    assert!(payload.reason.contains("BIS Entity List"));
    assert!(
        payload.checked_at_ms > 1_700_000_000_000,
        "the stamp must be a real epoch-ms, not a zeroed placeholder"
    );
}

/// Production runs the MOCK export-control backend — it performs no real
/// classification and no real denied-party screening. This test exists so the
/// honesty is a pinned fact rather than a claim in a PR body: if someone wires
/// a real backend the assertion fails and they update the compliance docs
/// deliberately.
#[test]
fn production_export_control_backend_is_still_the_mock() {
    assert_eq!(
        serve::export_control_provider().name(),
        "mock",
        "production export-control backend changed — the export.* rows now carry \
         REAL determinations and the 'no real screening yet' caveat must be revisited"
    );
}
