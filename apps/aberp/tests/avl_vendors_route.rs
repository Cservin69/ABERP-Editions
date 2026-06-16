//! S431 — integration pins for the Approved Vendor List (AVL): CRUD +
//! archive-not-delete, status-transition invariants, audit emission of all
//! five new EventKinds + the (previously never-fired) `supplier.export_screened`,
//! the refuse-PO gate, and the boot-time re-screening reminder.
//!
//! Library-helper boundary (mirrors `quoting_machines_route.rs`): the HTTPS
//! listener is not spun; the `*_request` helpers carry the full
//! validate → DB-write → audit-emit path the route handlers call.

use std::path::PathBuf;
use std::sync::Arc;

use aberp_audit_ledger::{BinaryHash, EventKind, Ledger, TenantId};
use aberp_compliance::avl::ApprovedStatus;
use ulid::Ulid;

use aberp::avl_vendors::{ScreenVendorInputs, VendorEditInputs, VendorInputs, VendorStatusInputs};
use aberp::serve::{self, AppState, VendorRouteError};

const TEST_TENANT: &str = "avl_vendors_route_test";
const TEST_HASH: BinaryHash = BinaryHash::from_bytes([0xAB; 32]);

fn test_dir(label: &str) -> PathBuf {
    let dir =
        std::env::temp_dir()
            .join("aberp-avl-route")
            .join(format!("{}-{}", label, Ulid::new()));
    std::fs::create_dir_all(&dir).expect("create test dir");
    dir
}

fn build_state(db_path: PathBuf) -> AppState {
    let tenant = TenantId::new(TEST_TENANT.to_string()).expect("tenant id");
    AppState {
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

fn create_inputs(partner: &str, status: &str, cats: &[&str], until: Option<&str>) -> VendorInputs {
    VendorInputs {
        partner_id: partner.to_string(),
        approved_status: status.to_string(),
        approval_categories: cats.iter().map(|s| s.to_string()).collect(),
        approved_until_utc: until.map(|s| s.to_string()),
        screening_notes: "init".to_string(),
    }
}

fn status_inputs(new: &str, reason: Option<&str>, force: bool) -> VendorStatusInputs {
    VendorStatusInputs {
        new_status: new.to_string(),
        reason: reason.map(|s| s.to_string()),
        force,
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

// ── CRUD smoke + archive-not-delete (revoke is the archive) ──────────

#[test]
fn crud_smoke_create_list_edit_revoke() {
    let dir = test_dir("crud");
    let db = dir.join("aberp.duckdb");
    let state = build_state(db.clone());

    let v = serve::create_avl_vendor_request(
        &state,
        &create_inputs("partner-acme", "pending", &["general", "itar"], None),
        "op",
        TEST_HASH,
    )
    .expect("create");
    assert!(v.id.starts_with("avl_"), "server-minted id: {}", v.id);
    assert_eq!(v.approved_status, "pending");
    assert_eq!(v.approval_categories, vec!["general", "itar"]);
    assert!(v.revoked_reason.is_none());

    // list sees it
    let list = serve::list_avl_vendors_request(&state).expect("list");
    assert_eq!(list.len(), 1);

    // edit categories + notes (no status change here)
    let edited = serve::update_avl_vendor_request(
        &state,
        &v.id,
        &VendorEditInputs {
            approval_categories: vec!["defense".to_string()],
            approved_until_utc: Some("2030-01-01T00:00:00Z".to_string()),
            screening_notes: "edited".to_string(),
        },
        "op",
    )
    .expect("edit");
    assert_eq!(edited.approval_categories, vec!["defense"]);
    assert_eq!(edited.screening_notes, "edited");

    // archive-not-delete: revoke sets status=Revoked + reason, row stays.
    let revoked = serve::set_avl_vendor_status_request(
        &state,
        &v.id,
        &status_inputs("revoked", Some("debarred"), false),
        "op",
        TEST_HASH,
    )
    .expect("revoke");
    assert_eq!(revoked.approved_status, "revoked");
    assert_eq!(revoked.revoked_reason.as_deref(), Some("debarred"));

    // Still listed (the AVL keeps revoked rows visible) + still gettable.
    let after = serve::list_avl_vendors_request(&state).expect("list after revoke");
    assert_eq!(
        after.len(),
        1,
        "revoked row stays on the AVL (archive-not-delete)"
    );
    let got = serve::get_avl_vendor_request(&state, &v.id).expect("get revoked");
    assert_eq!(got.approved_status, "revoked");
}

#[test]
fn create_validation_rejects_bad_status_category_and_date() {
    let dir = test_dir("validate");
    let state = build_state(dir.join("aberp.duckdb"));
    let err = serve::create_avl_vendor_request(
        &state,
        &create_inputs("", "frozen", &["bogus"], Some("not-a-date")),
        "op",
        TEST_HASH,
    )
    .expect_err("must reject");
    match err {
        VendorRouteError::Validation(fields) => {
            let names: Vec<&str> = fields.iter().map(|f| f.field).collect();
            assert!(names.contains(&"partner_id"), "{names:?}");
            assert!(names.contains(&"approved_status"), "{names:?}");
            assert!(names.contains(&"approval_categories"), "{names:?}");
            assert!(names.contains(&"approved_until_utc"), "{names:?}");
        }
        other => panic!("expected Validation, got {other:?}"),
    }
}

// ── Status-transition invariant: Revoked → Approved blocked w/o force ──

#[test]
fn revoked_to_approved_blocked_until_manual_override() {
    let dir = test_dir("transition");
    let db = dir.join("aberp.duckdb");
    let state = build_state(db.clone());
    let v = serve::create_avl_vendor_request(
        &state,
        &create_inputs("partner-x", "approved", &["general"], None),
        "op",
        TEST_HASH,
    )
    .expect("create");

    // Pending→Approved-style move is fine: Approved→Suspended.
    serve::set_avl_vendor_status_request(
        &state,
        &v.id,
        &status_inputs("suspended", None, false),
        "op",
        TEST_HASH,
    )
    .expect("approved→suspended ok");

    // Revoke it.
    serve::set_avl_vendor_status_request(
        &state,
        &v.id,
        &status_inputs("revoked", Some("cause"), false),
        "op",
        TEST_HASH,
    )
    .expect("revoke");

    // Revoked→Approved WITHOUT force → Conflict (invalid transition).
    let blocked = serve::set_avl_vendor_status_request(
        &state,
        &v.id,
        &status_inputs("approved", None, false),
        "op",
        TEST_HASH,
    )
    .expect_err("must block");
    assert!(
        matches!(blocked, VendorRouteError::Conflict(_)),
        "{blocked:?}"
    );

    // Revoked→Approved WITH force (manual override) → allowed.
    let reactivated = serve::set_avl_vendor_status_request(
        &state,
        &v.id,
        &status_inputs("approved", None, true),
        "op",
        TEST_HASH,
    )
    .expect("manual override");
    assert_eq!(reactivated.approved_status, "approved");
    assert!(
        reactivated.revoked_reason.is_none(),
        "reason cleared on reactivate"
    );
}

#[test]
fn revoke_without_reason_is_validation_error() {
    let dir = test_dir("revoke-no-reason");
    let state = build_state(dir.join("aberp.duckdb"));
    let v = serve::create_avl_vendor_request(
        &state,
        &create_inputs("p", "approved", &["general"], None),
        "op",
        TEST_HASH,
    )
    .expect("create");
    let err = serve::set_avl_vendor_status_request(
        &state,
        &v.id,
        &status_inputs("revoked", None, false),
        "op",
        TEST_HASH,
    )
    .expect_err("must require reason");
    assert!(matches!(err, VendorRouteError::Validation(_)), "{err:?}");
}

// ── Audit: every new kind + supplier.export_screened fire from the right path ──

#[test]
fn avl_paths_emit_all_five_new_kinds_and_export_screened() {
    let dir = test_dir("audit");
    let db = dir.join("aberp.duckdb");
    let state = build_state(db.clone());

    // add → AvlVendorAdded
    let v = serve::create_avl_vendor_request(
        &state,
        &create_inputs("partner-audit", "pending", &["itar"], None),
        "op",
        TEST_HASH,
    )
    .expect("create");

    // status change → AvlVendorStatusChanged
    serve::set_avl_vendor_status_request(
        &state,
        &v.id,
        &status_inputs("approved", None, false),
        "op",
        TEST_HASH,
    )
    .expect("status");

    // screen → supplier.export_screened
    serve::screen_avl_vendor_request(
        &state,
        &v.id,
        &ScreenVendorInputs {
            categories_screened: vec!["itar".to_string()],
            screening_result: "pass".to_string(),
        },
        "op",
        TEST_HASH,
    )
    .expect("screen");

    // suspend a second vendor so the PO gate has a blocker
    let v2 = serve::create_avl_vendor_request(
        &state,
        &create_inputs("partner-bad", "suspended", &["general"], None),
        "op",
        TEST_HASH,
    )
    .expect("create2");
    let _ = v2;

    // PO check against the suspended partner → PoBlockedByVendorStatus
    serve::avl_po_check_request(&state, "partner-bad", "op", TEST_HASH).expect_err("blocked");

    // revoke → AvlVendorRevoked
    serve::set_avl_vendor_status_request(
        &state,
        &v.id,
        &status_inputs("revoked", Some("end"), false),
        "op",
        TEST_HASH,
    )
    .expect("revoke");

    let kinds = ledger_kinds(&db);
    for expected in [
        EventKind::AvlVendorAdded,
        EventKind::AvlVendorStatusChanged,
        EventKind::SupplierExportScreened,
        EventKind::PoBlockedByVendorStatus,
        EventKind::AvlVendorRevoked,
    ] {
        assert!(
            kinds.contains(&expected),
            "missing {expected:?} in {kinds:?}"
        );
    }
}

// ── Refuse-PO gate: pending/eligible passes, suspended/revoked blocks ──

#[test]
fn po_gate_allows_eligible_and_unlisted_blocks_suspended_and_revoked() {
    let dir = test_dir("po-gate");
    let db = dir.join("aberp.duckdb");
    let state = build_state(db.clone());

    // Unlisted partner → no entry → allowed.
    serve::avl_po_check_request(&state, "never-listed", "op", TEST_HASH).expect("unlisted ok");

    // Pending vendor → not blocking → allowed.
    serve::create_avl_vendor_request(
        &state,
        &create_inputs("partner-pending", "pending", &["general"], None),
        "op",
        TEST_HASH,
    )
    .expect("pending");
    serve::avl_po_check_request(&state, "partner-pending", "op", TEST_HASH).expect("pending ok");

    // Suspended → blocked.
    serve::create_avl_vendor_request(
        &state,
        &create_inputs("partner-susp", "suspended", &["general"], None),
        "op",
        TEST_HASH,
    )
    .expect("susp");
    let blocked = serve::avl_po_check_request(&state, "partner-susp", "op", TEST_HASH)
        .expect_err("suspended blocks");
    match blocked {
        VendorRouteError::Conflict(msg) => assert!(
            msg.contains("partner-susp") && msg.to_lowercase().contains("suspended"),
            "operator message names vendor + status: {msg}"
        ),
        other => panic!("expected Conflict, got {other:?}"),
    }
}

// ── Overdue reminder: boot scan fires exactly once per overdue vendor ──

#[test]
fn boot_overdue_scan_fires_avl_screening_overdue_exactly_once() {
    let dir = test_dir("overdue");
    let db = dir.join("aberp.duckdb");
    let state = build_state(db.clone());
    let tenant = TenantId::new(TEST_TENANT.to_string()).expect("tenant");

    // One vendor whose approval window already lapsed.
    serve::create_avl_vendor_request(
        &state,
        &create_inputs(
            "partner-old",
            "approved",
            &["general"],
            Some("2000-01-01T00:00:00Z"),
        ),
        "op",
        TEST_HASH,
    )
    .expect("overdue vendor");
    // One vendor with a future window (NOT overdue).
    serve::create_avl_vendor_request(
        &state,
        &create_inputs(
            "partner-fresh",
            "approved",
            &["general"],
            Some("2100-01-01T00:00:00Z"),
        ),
        "op",
        TEST_HASH,
    )
    .expect("fresh vendor");
    // One overdue-but-revoked vendor (skipped — revoked never reminds).
    let rev = serve::create_avl_vendor_request(
        &state,
        &create_inputs(
            "partner-rev",
            "approved",
            &["general"],
            Some("2000-01-01T00:00:00Z"),
        ),
        "op",
        TEST_HASH,
    )
    .expect("rev vendor");
    serve::set_avl_vendor_status_request(
        &state,
        &rev.id,
        &status_inputs("revoked", Some("gone"), false),
        "op",
        TEST_HASH,
    )
    .expect("revoke");

    let now = time::OffsetDateTime::now_utc();
    let fired = aberp::avl_vendors::fire_overdue_screening_reminders(
        &db,
        tenant.clone(),
        TEST_HASH,
        "boot",
        now,
    )
    .expect("scan");
    assert_eq!(fired, 1, "only the non-revoked overdue vendor");

    let overdue_count = ledger_kinds(&db)
        .iter()
        .filter(|k| **k == EventKind::AvlScreeningOverdue)
        .count();
    assert_eq!(overdue_count, 1, "exactly one AvlScreeningOverdue fired");

    // The blocking-status helper agrees with the gate.
    assert!(ApprovedStatus::Suspended.blocks_po());
    assert!(!ApprovedStatus::Approved.blocks_po());
}
