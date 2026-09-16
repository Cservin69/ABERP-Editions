//! ADR-0123 slice 2 — the promote route, end to end.
//!
//! The seam F1 recorded as cut: an outgoing invoice could not be joined back
//! to the shipment it bills, in the ledger or in the tables. These tests drive
//! the real issuance pipeline through `issue_invoice_request` and assert the
//! link is recorded in BOTH places an auditor would look — the
//! `invoice_shipment_provenance` row and the `InvoiceDraftCreated` chain entry
//! — and that it survives everything that used to erase it.
//!
//! Defense-only: the promote ROUTE is edition-gated (§D4). These tests exercise
//! the pipeline helper directly, so they pin the behaviour on both arms; the
//! route's Portable refusal is pinned separately.

use std::path::PathBuf;
use std::sync::Arc;

use aberp_audit_ledger::{Actor, BinaryHash, EventKind, Ledger, TenantId};
use aberp_billing::Currency;
use aberp_mnb_rates::{MnbError, MnbRate};
use duckdb::params;
use time::Date;
use ulid::Ulid;

use aberp::audit_payloads::InvoiceDraftCreatedPayload;
use aberp::invoice_draft::DraftState;
use aberp::issue_invoice::{AddressJson, CustomerJson, LineJson, SupplierJson};
use aberp::mnb_rates_provider::MnbRatesProvider;
use aberp::nav_xml::CustomerVatStatus;
use aberp::serve::{self, AppState, IssueInvoiceRequest};

const TEST_TENANT: &str = "prov-route-tenant";
const DSP: &str = "dsp_01ARZ3NDEKTSV4RRFFQ69G5FAV";
const WO: &str = "wo-prov";

/// The pipeline never reaches MNB on the HUF path; any call is a bug.
struct UnreachableProvider;

#[async_trait::async_trait]
impl MnbRatesProvider for UnreachableProvider {
    async fn fetch_official_rate(
        &self,
        _currency: Currency,
        _date: Date,
    ) -> Result<MnbRate, MnbError> {
        unreachable!("the HUF path is rate-free; MNB must never be consulted")
    }
}

fn test_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("aberp-invoice-provenance")
        .join(format!("{label}-{}", Ulid::new()));
    std::fs::create_dir_all(&dir).expect("create test dir");
    dir
}

fn build_state(db_path: PathBuf) -> AppState {
    let tenant = TenantId::new(TEST_TENANT.to_string()).expect("tenant id");
    AppState {
        db: serve::open_tenant_handle(&db_path, tenant.clone()).expect("open shared handle"),
        db_path: Arc::new(db_path),
        tenant,
        nav_enabled: true,
        binary_hash: aberp::binary_hash::BinaryHashHandle::from_ready(BinaryHash::from_bytes(
            [0u8; 32],
        )),
        session_token: Arc::new("test-token".to_string()),
        secrets_cache: aberp::secrets_cache::SecretsCache::empty(),
        nav_poll_semaphore: Arc::new(tokio::sync::Semaphore::new(
            serve::NAV_POLL_DAEMON_CONCURRENCY,
        )),
        boot_state: Arc::new(std::sync::RwLock::new(serve::ServeBootState::Ready {
            operator_login: "test-operator".to_string(),
        })),
        shutdown_token: tokio_util::sync::CancellationToken::new(),
        adapter_registry: Arc::new(std::sync::RwLock::new(aberp_mes::AdapterRegistry::new())),
        adapter_manager: Arc::new(aberp::mes_manager::AdapterManager::new(
            Arc::new(std::sync::RwLock::new(aberp_mes::AdapterRegistry::new())),
            tokio_util::sync::CancellationToken::new(),
        )),
        adapter_health_baseline: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        restore_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        catalogue_push: aberp::catalogue_push::CataloguePushHandle::dormant(),
        email_relay_rate_limiter: Arc::new(aberp::email_relay::RateLimiter::new()),
        pipeline_python_resolution: aberp::quote_pricing_pipeline::PythonResolutionHandle::dormant(
        ),
        storefront_credential: aberp::storefront_credential::StorefrontCredentialHandle::dormant(),
        email_outbox_daemon: aberp::email_outbox_poll_daemon::EmailOutboxDaemonHandle::dormant(),
        quote_pdf_rerender_queue: aberp::quote_pdf_rerender_queue::QuotePdfRerenderQueue::new(),
        digital_id: Arc::new(aberp_digital_id::MockProvider::new()),
    }
}

fn fixture_supplier() -> SupplierJson {
    SupplierJson {
        tax_number: "12345678-1-42".to_string(),
        name: "ABERP Supplier Kft.".to_string(),
        address: AddressJson {
            country_code: "HU".to_string(),
            postal_code: "1011".to_string(),
            city: "Budapest".to_string(),
            street: "Fo utca 1.".to_string(),
        },
    }
}

fn fixture_request() -> IssueInvoiceRequest {
    IssueInvoiceRequest {
        customer: CustomerJson {
            community_vat_number: None,
            vat_status: CustomerVatStatus::Domestic,
            partner_id: None,
            tax_number: "87654321-2-13".to_string(),
            name: "Vevo Kft.".to_string(),
            address: Some(AddressJson {
                country_code: "HU".to_string(),
                postal_code: "1052".to_string(),
                city: "Budapest".to_string(),
                street: "Vaci utca 19.".to_string(),
            }),
        },
        lines: vec![LineJson {
            description: "Machined bracket".to_string(),
            quantity: rust_decimal::Decimal::from(1),
            unit_price: 1000,
            vat_rate_percent: 27,
            vat_rate_kind: aberp_billing::VatRateKind::Percent,
            note: None,
            unit: None,
        }],
        currency: Currency::Huf,
        series: None,
        bank_account_id: None,
        invoice_note: None,
        payment_deadline: None,
        delivery_date: None,
        delivery_date_override: None,
        email_buyer_on_issue: Some(false),
        submit_to_nav_on_issue: Some(false),
        payment_method: aberp_billing::PaymentMethod::default(),
        email_recipient_override: None,
    }
}

fn write_fixture_seller_toml(dir: &std::path::Path) {
    let seller = dir.join("seller.toml");
    std::fs::write(
        &seller,
        r#"
[seller]
tax_number = "12345678-1-42"
name = "ABERP Supplier Kft."
country_code = "HU"
postal_code = "1011"
city = "Budapest"
street = "Fo utca 1."
"#,
    )
    .expect("write seller.toml");
}

/// Seed a shipped dispatch with a staged draft, exactly as the spawner does
/// inside `mark_shipped`. Returns the draft id.
fn seed_staged_draft(state: &AppState) -> String {
    seed_staged_draft_for(state, DSP)
}

/// One draft per dispatch is enforced by the schema
/// (`UNIQUE (tenant_id, source_dispatch_id)`), so a test seeding two drafts
/// must give them distinct dispatches.
fn seed_staged_draft_for(state: &AppState, dsp: &str) -> String {
    let drf_id = format!("drf_{}", Ulid::new());
    let guard = state.db.write().expect("writer");
    guard
        .execute(
            "INSERT INTO invoice_draft (
                drf_id, tenant_id, partner_id, source_dispatch_id, source_wo_id,
                product_id, qty, notes, created_at, state
             ) VALUES (?1, ?2, 'ptr_x', ?3, ?4, 'prd_bracket', '1', NULL,
                       '2026-09-11T00:00:00Z', 'staged');",
            params![&drf_id, TEST_TENANT, dsp, WO],
        )
        .expect("seed invoice_draft");
    drop(guard);
    drf_id
}

fn draft_state(state: &AppState, drf_id: &str) -> DraftState {
    let conn = state.db.read().expect("read");
    let raw: Option<String> = conn
        .query_row(
            "SELECT state FROM invoice_draft WHERE tenant_id = ?1 AND drf_id = ?2;",
            params![TEST_TENANT, drf_id],
            |r| r.get(0),
        )
        .expect("read draft state");
    DraftState::parse(raw.as_deref())
}

/// The `InvoiceDraftCreated` payload for one invoice, read back off the chain.
fn chain_payload(state: &AppState, invoice_id: &str) -> InvoiceDraftCreatedPayload {
    let ledger = Ledger::open(
        state.db_path.as_ref(),
        TenantId::new(TEST_TENANT.to_string()).unwrap(),
        BinaryHash::from_bytes([0u8; 32]),
    )
    .expect("open ledger");
    for entry in ledger.entries().expect("entries") {
        if entry.kind != EventKind::InvoiceDraftCreated {
            continue;
        }
        let p: InvoiceDraftCreatedPayload =
            serde_json::from_slice(&entry.payload).expect("decode payload");
        if p.invoice_id == invoice_id {
            return p;
        }
    }
    panic!("no InvoiceDraftCreated entry for {invoice_id}");
}

/// Issue (or promote) with an explicit saved-partner id on the buyer, which is
/// what the partner-mismatch guard compares against.
async fn promote_as_partner(
    state: &AppState,
    drf_id: Option<String>,
    partner_id: &str,
) -> anyhow::Result<aberp::issue_invoice::IssuedInvoiceSummary> {
    let mut req = fixture_request();
    req.customer.partner_id = Some(partner_id.to_string());
    serve::issue_invoice_request(
        state,
        req,
        fixture_supplier(),
        &UnreachableProvider,
        Actor::from_local_cli(Ulid::new().to_string(), "test-user"),
        None,
        drf_id,
    )
    .await
}

/// Promote (or issue) naming the seeded draft's own buyer, `ptr_x`.
///
/// Every promote here names a partner because ADR-0123's guard REQUIRES it: a
/// shipment-linked invoice must bill the party the goods went to, and a one-off
/// buyer cannot be checked against that. The unnamed-buyer case is its own test
/// below.
async fn promote(
    state: &AppState,
    drf_id: Option<String>,
) -> aberp::issue_invoice::IssuedInvoiceSummary {
    let mut req = fixture_request();
    if drf_id.is_some() {
        req.customer.partner_id = Some("ptr_x".to_string());
    }
    serve::issue_invoice_request(
        state,
        req,
        fixture_supplier(),
        &UnreachableProvider,
        Actor::from_local_cli(Ulid::new().to_string(), "test-user"),
        None,
        drf_id,
    )
    .await
    .expect("issuance must succeed")
}

/// **The seam, closed.** An invoice promoted from a draft records the shipment
/// in both places an auditor looks, and the draft is flipped rather than left
/// as outstanding work.
#[tokio::test(flavor = "current_thread")]
async fn a_promoted_invoice_records_its_shipment_in_the_table_and_the_chain() {
    let dir = test_dir("promote");
    std::env::set_var("HOME", &dir);
    write_fixture_seller_toml(&dir);
    let state = build_state(dir.join("aberp.duckdb"));
    let drf_id = seed_staged_draft(&state);

    let summary = promote(&state, Some(drf_id.clone())).await;

    // 1. The table.
    let conn = state.db.read().expect("read");
    let row = aberp::invoice_provenance::get_for_invoice(&conn, TEST_TENANT, &summary.invoice_id)
        .expect("read provenance")
        .expect("a promoted invoice MUST have provenance");
    assert_eq!(row.source_dispatch_id.as_deref(), Some(DSP));
    assert_eq!(row.source_wo_id.as_deref(), Some(WO));
    assert_eq!(row.source_draft_id, drf_id);
    drop(conn);

    // 2. The hash-chained ledger — the copy that cannot be edited afterwards.
    let payload = chain_payload(&state, &summary.invoice_id);
    assert_eq!(payload.source_dispatch_id.as_deref(), Some(DSP));
    assert_eq!(payload.source_wo_id.as_deref(), Some(WO));
    assert_eq!(payload.source_draft_id.as_deref(), Some(drf_id.as_str()));

    // 3. The draft is no longer outstanding work (§D5).
    assert_eq!(draft_state(&state, &drf_id), DraftState::Promoted);
}

/// **Ervin's "NOT NULLed on delete", as a test.**
///
/// Everything that used to erase the link is run against a promoted invoice:
/// the dispatch's own pointer is NULLed and the draft row is deleted outright.
/// The invoice's provenance must survive both, because it lives on the invoice
/// side and `delete_draft_in_tx` has no path to it.
#[tokio::test(flavor = "current_thread")]
async fn the_provenance_survives_everything_that_used_to_erase_it() {
    let dir = test_dir("survives");
    std::env::set_var("HOME", &dir);
    write_fixture_seller_toml(&dir);
    let state = build_state(dir.join("aberp.duckdb"));
    let drf_id = seed_staged_draft(&state);
    let summary = promote(&state, Some(drf_id.clone())).await;

    {
        let guard = state.db.write().expect("writer");
        // The dispatch pointer — this is what `delete_draft_in_tx` clears.
        guard
            .execute(
                "UPDATE dispatches SET spawned_invoice_id = NULL
                  WHERE tenant_id = ?1 AND spawned_invoice_id = ?2;",
                params![TEST_TENANT, &drf_id],
            )
            .ok();
        // And the draft row itself, gone.
        guard
            .execute(
                "DELETE FROM invoice_draft WHERE tenant_id = ?1 AND drf_id = ?2;",
                params![TEST_TENANT, &drf_id],
            )
            .expect("delete the draft");
        drop(guard);
    }

    let conn = state.db.read().expect("read");
    let row = aberp::invoice_provenance::get_for_invoice(&conn, TEST_TENANT, &summary.invoice_id)
        .expect("read provenance")
        .expect("the link must survive the draft's deletion");
    assert_eq!(row.source_dispatch_id.as_deref(), Some(DSP));
    drop(conn);

    // The chain copy is immutable by construction and still reads.
    assert_eq!(
        chain_payload(&state, &summary.invoice_id)
            .source_dispatch_id
            .as_deref(),
        Some(DSP)
    );
}

/// **§D5 revert-proof — storno then re-issue keeps the shipment linked.**
///
/// This is the case that killed the design pass's delete-on-promote: correct an
/// invoice and the replacement must still be joinable to the shipment. With the
/// draft state-flipped rather than deleted, the row is still there — but
/// promoting it twice is refused, because an already-promoted draft has already
/// answered for an invoice. The honest correction path is a NEW draft for the
/// re-issue, and what this pins is that the refusal is loud rather than a
/// silently provenance-less invoice.
#[tokio::test(flavor = "current_thread")]
async fn promoting_an_already_promoted_draft_is_refused_loudly() {
    let dir = test_dir("twice");
    std::env::set_var("HOME", &dir);
    write_fixture_seller_toml(&dir);
    let state = build_state(dir.join("aberp.duckdb"));
    let drf_id = seed_staged_draft(&state);
    let first = promote(&state, Some(drf_id.clone())).await;

    let err = serve::issue_invoice_request(
        &state,
        fixture_request(),
        fixture_supplier(),
        &UnreachableProvider,
        Actor::from_local_cli(Ulid::new().to_string(), "test-user"),
        None,
        Some(drf_id.clone()),
    )
    .await
    .expect_err("a second promote of the same draft must refuse");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("already promoted"),
        "the refusal must say why: {msg}"
    );

    // And the refusal rolled back cleanly: the first invoice's provenance is
    // untouched, and no second row was written against the draft.
    let conn = state.db.read().expect("read");
    let all = aberp::invoice_provenance::list_for_dispatch(&conn, TEST_TENANT, DSP)
        .expect("list for dispatch");
    assert_eq!(all.len(), 1, "the refused issuance wrote nothing");
    assert_eq!(all[0].invoice_id, first.invoice_id);
}

/// A missing draft refuses rather than issuing an invoice with no origin.
///
/// The direction matters: the alternative — mint the invoice and skip the
/// provenance — would turn an operator's typo into exactly the silent absence
/// ADR-0123 exists to remove.
#[tokio::test(flavor = "current_thread")]
async fn promoting_a_draft_that_does_not_exist_refuses() {
    let dir = test_dir("missing");
    std::env::set_var("HOME", &dir);
    write_fixture_seller_toml(&dir);
    let state = build_state(dir.join("aberp.duckdb"));

    let err = serve::issue_invoice_request(
        &state,
        fixture_request(),
        fixture_supplier(),
        &UnreachableProvider,
        Actor::from_local_cli(Ulid::new().to_string(), "test-user"),
        None,
        Some("drf_01ARZ3NDEKTSV4RRFFQ69G5FAV".to_string()),
    )
    .await
    .expect_err("a nonexistent draft must refuse");
    assert!(format!("{err:#}").contains("does not exist"));
}

/// The ordinary issue path is unchanged: no draft named, no provenance
/// recorded, and nothing inferred.
#[tokio::test(flavor = "current_thread")]
async fn the_ordinary_issue_path_records_no_provenance_and_infers_nothing() {
    let dir = test_dir("ordinary");
    std::env::set_var("HOME", &dir);
    write_fixture_seller_toml(&dir);
    let state = build_state(dir.join("aberp.duckdb"));
    // A staged draft for this very partner + product exists and is IGNORED —
    // the inference ADR-0123 rejected would have matched it.
    let drf_id = seed_staged_draft(&state);

    let summary = promote(&state, None).await;

    let conn = state.db.read().expect("read");
    assert_eq!(
        aberp::invoice_provenance::get_for_invoice(&conn, TEST_TENANT, &summary.invoice_id)
            .expect("read provenance"),
        None,
        "an invoice issued through the ordinary form must claim no shipment origin"
    );
    drop(conn);
    assert_eq!(
        chain_payload(&state, &summary.invoice_id).source_dispatch_id,
        None
    );
    // And the untouched draft is still outstanding work.
    assert_eq!(draft_state(&state, &drf_id), DraftState::Staged);
}

// ── ADR-0123 §D5 — the two protections on the draft row ─────────────────

/// **A promoted draft is not deletable.**
///
/// The invoice's link survives a delete either way (proven above). What the
/// draft row carries is the ability to CHECK that link — §D2 keeps
/// `source_draft_id` so an inspector can inspect the row the server derived
/// the dispatch from instead of trusting the derivation. Deleting it
/// downgrades a checkable derivation to an assertion.
#[tokio::test(flavor = "current_thread")]
async fn a_promoted_draft_cannot_be_deleted_by_an_operator() {
    let dir = test_dir("nodelete");
    std::env::set_var("HOME", &dir);
    write_fixture_seller_toml(&dir);
    let state = build_state(dir.join("aberp.duckdb"));
    let drf_id = seed_staged_draft(&state);
    let summary = promote(&state, Some(drf_id.clone())).await;

    let mut guard = state.db.write().expect("writer");
    let tx = guard.transaction().expect("tx");
    let err = aberp::invoice_draft::delete_draft_in_tx(
        &tx,
        &aberp_audit_ledger::LedgerMeta::new(
            TenantId::new(TEST_TENANT.to_string()).unwrap(),
            BinaryHash::from_bytes([0u8; 32]),
        ),
        Actor::from_local_cli("s".to_string(), "op"),
        aberp::invoice_draft::DeleteDraftInputs {
            tenant: TEST_TENANT.to_string(),
            drf_id: drf_id.clone(),
            actor: "op".to_string(),
        },
    )
    .expect_err("a promoted draft must refuse deletion");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("has been promoted") && msg.contains(&drf_id),
        "the refusal must name the draft and say why: {msg}"
    );
    drop(tx);
    drop(guard);

    // The row is still there — and so is the invoice's link.
    assert_eq!(draft_state(&state, &drf_id), DraftState::Promoted);
    let conn = state.db.read().expect("read");
    assert!(
        aberp::invoice_provenance::get_for_invoice(&conn, TEST_TENANT, &summary.invoice_id)
            .expect("read")
            .is_some()
    );
}

/// A STAGED draft is still deletable — the protection is targeted, not a
/// blanket freeze on the delete route.
#[tokio::test(flavor = "current_thread")]
async fn a_staged_draft_is_still_deletable() {
    let dir = test_dir("stageddelete");
    std::env::set_var("HOME", &dir);
    write_fixture_seller_toml(&dir);
    let state = build_state(dir.join("aberp.duckdb"));
    let drf_id = seed_staged_draft(&state);

    let mut guard = state.db.write().expect("writer");
    let tx = guard.transaction().expect("tx");
    let outcome = aberp::invoice_draft::delete_draft_in_tx(
        &tx,
        &aberp_audit_ledger::LedgerMeta::new(
            TenantId::new(TEST_TENANT.to_string()).unwrap(),
            BinaryHash::from_bytes([0u8; 32]),
        ),
        Actor::from_local_cli("s".to_string(), "op"),
        aberp::invoice_draft::DeleteDraftInputs {
            tenant: TEST_TENANT.to_string(),
            drf_id: drf_id.clone(),
            actor: "op".to_string(),
        },
    )
    .expect("a staged draft deletes as before");
    assert!(outcome.deleted);
    tx.commit().expect("commit");
}

/// **A promoted draft leaves the operator's work list; a staged one stays.**
///
/// Pre-ADR-0123 rows carry `state IS NULL` and MUST still list — matching only
/// `= 'staged'` would silently empty the queue on the first boot after the
/// migration, which is the failure mode worth pinning.
#[tokio::test(flavor = "current_thread")]
async fn the_draft_list_shows_staged_and_legacy_rows_but_not_promoted_ones() {
    let dir = test_dir("listfilter");
    std::env::set_var("HOME", &dir);
    write_fixture_seller_toml(&dir);
    let state = build_state(dir.join("aberp.duckdb"));

    let staged = seed_staged_draft_for(&state, DSP);
    let promoted = seed_staged_draft_for(&state, "dsp_01BRZ3NDEKTSV4RRFFQ69G5FAV");
    let legacy = format!("drf_{}", Ulid::new());
    {
        let guard = state.db.write().expect("writer");
        // A pre-migration row: state IS NULL.
        guard
            .execute(
                "INSERT INTO invoice_draft (
                    drf_id, tenant_id, partner_id, source_dispatch_id, source_wo_id,
                    product_id, qty, notes, created_at, state
                 ) VALUES (?1, ?2, 'ptr_x', NULL, NULL, 'prd_bracket', '1', NULL,
                           '2026-09-01T00:00:00Z', NULL);",
                params![&legacy, TEST_TENANT],
            )
            .expect("seed legacy draft");
        guard
            .execute(
                "UPDATE invoice_draft SET state = 'promoted'
                  WHERE tenant_id = ?1 AND drf_id = ?2;",
                params![TEST_TENANT, &promoted],
            )
            .expect("flip");
        drop(guard);
    }

    let conn = state.db.read().expect("read");
    let ids: Vec<String> = aberp::invoice_draft::list_drafts(&conn, TEST_TENANT)
        .expect("list drafts")
        .into_iter()
        .map(|d| d.drf_id)
        .collect();
    assert!(ids.contains(&staged), "a staged draft is outstanding work");
    assert!(
        ids.contains(&legacy),
        "a pre-migration NULL-state row is still outstanding work — matching \
         only `= 'staged'` would empty the queue on the first boot after upgrade"
    );
    assert!(
        !ids.contains(&promoted),
        "a promoted draft is not outstanding work"
    );
}

/// **The sharpest hazard in the promote flow: the operator edits the buyer.**
///
/// The form opens prefilled from the draft; nothing stops the operator
/// changing the customer before submitting. If that were allowed to proceed,
/// the invoice would bill partner B while carrying partner A's dispatch as its
/// recorded origin — an actively WRONG evidence link. ADR-0123 rejected
/// inference because "a guess in an evidence trail is worse than an honest
/// absence"; a wrong link is worse still, so promote refuses.
#[tokio::test(flavor = "current_thread")]
async fn promoting_a_draft_for_a_different_buyer_is_refused() {
    let dir = test_dir("mismatch");
    std::env::set_var("HOME", &dir);
    write_fixture_seller_toml(&dir);
    let state = build_state(dir.join("aberp.duckdb"));
    // The seeded draft belongs to `ptr_x`.
    let drf_id = seed_staged_draft(&state);

    let err = promote_as_partner(&state, Some(drf_id.clone()), "ptr_SOMEONE_ELSE")
        .await
        .expect_err("billing a different buyer under this shipment must refuse");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("ptr_x") && msg.contains("ptr_SOMEONE_ELSE"),
        "the refusal must name BOTH partners so the operator can see the swap: {msg}"
    );

    // Refused cleanly: the draft is untouched and nothing was recorded.
    assert_eq!(draft_state(&state, &drf_id), DraftState::Staged);
    let conn = state.db.read().expect("read");
    assert!(
        aberp::invoice_provenance::list_for_dispatch(&conn, TEST_TENANT, DSP)
            .expect("list")
            .is_empty()
    );
}

/// The matching case proceeds — the guard is targeted, not a blanket refusal
/// of every promote that names a partner.
#[tokio::test(flavor = "current_thread")]
async fn promoting_a_draft_for_the_same_buyer_proceeds() {
    let dir = test_dir("match");
    std::env::set_var("HOME", &dir);
    write_fixture_seller_toml(&dir);
    let state = build_state(dir.join("aberp.duckdb"));
    let drf_id = seed_staged_draft(&state);

    let summary = promote_as_partner(&state, Some(drf_id.clone()), "ptr_x")
        .await
        .expect("the draft's own buyer promotes normally");
    let conn = state.db.read().expect("read");
    assert_eq!(
        aberp::invoice_provenance::get_for_invoice(&conn, TEST_TENANT, &summary.invoice_id)
            .expect("read")
            .expect("recorded")
            .source_dispatch_id
            .as_deref(),
        Some(DSP)
    );
}

/// **The one-off-buyer hole, shut.**
///
/// The earlier guard compared `partner_id` to `partner_id`, so an invoice that
/// named NO saved buyer slipped past it entirely: an operator could bill an
/// ad-hoc buyer while recording this shipment as the invoice's origin. That is
/// the wrong-link case the guard exists for, reached by leaving a field blank
/// rather than by changing it.
///
/// Absent now refuses on the same footing as different, and the message names
/// the buyer the shipment actually went to so the operator can fix it.
#[tokio::test(flavor = "current_thread")]
async fn promoting_a_shipment_linked_draft_to_a_one_off_buyer_is_refused() {
    let dir = test_dir("oneoff");
    std::env::set_var("HOME", &dir);
    write_fixture_seller_toml(&dir);
    let state = build_state(dir.join("aberp.duckdb"));
    let drf_id = seed_staged_draft(&state);

    // `fixture_request()` carries `partner_id: None` — a one-off buyer.
    let err = serve::issue_invoice_request(
        &state,
        fixture_request(),
        fixture_supplier(),
        &UnreachableProvider,
        Actor::from_local_cli(Ulid::new().to_string(), "test-user"),
        None,
        Some(drf_id.clone()),
    )
    .await
    .expect_err("an unsaved buyer cannot be checked against the shipment's buyer");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("ptr_x"),
        "the refusal must name the buyer the goods went to: {msg}"
    );
    assert!(
        msg.contains("one-off") || msg.contains("no saved buyer"),
        "the refusal must say WHICH condition tripped, not just that it failed: {msg}"
    );

    // Refused cleanly — nothing recorded, draft untouched.
    assert_eq!(draft_state(&state, &drf_id), DraftState::Staged);
    let conn = state.db.read().expect("read");
    assert!(
        aberp::invoice_provenance::list_for_dispatch(&conn, TEST_TENANT, DSP)
            .expect("list")
            .is_empty(),
        "a refused promote records no provenance"
    );
}

/// The ORDINARY form still accepts a one-off buyer — the refusal is scoped to
/// promotions, not a new restriction on ad-hoc invoicing.
#[tokio::test(flavor = "current_thread")]
async fn the_ordinary_form_still_accepts_a_one_off_buyer() {
    let dir = test_dir("oneoffok");
    std::env::set_var("HOME", &dir);
    write_fixture_seller_toml(&dir);
    let state = build_state(dir.join("aberp.duckdb"));

    let summary = serve::issue_invoice_request(
        &state,
        fixture_request(),
        fixture_supplier(),
        &UnreachableProvider,
        Actor::from_local_cli(Ulid::new().to_string(), "test-user"),
        None,
        None,
    )
    .await
    .expect("an ad-hoc buyer is ordinary business on the plain form");
    let conn = state.db.read().expect("read");
    assert_eq!(
        aberp::invoice_provenance::get_for_invoice(&conn, TEST_TENANT, &summary.invoice_id)
            .expect("read"),
        None,
        "no draft named, so no shipment origin — an honest absence"
    );
}
