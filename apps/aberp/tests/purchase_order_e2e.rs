//! S440 (ADR-0068) — purchase-order procurement e2e + AVL gate + receiving→NCR.
//!
//! Walks the procurement journey the brief names ([[customer-journey-e2e-gate]]):
//! Approved vendor → create PO → issue → receive partial → receive remaining →
//! Closed. Plus the AVL-gate invariants ([[trust-code-not-operator]]) for every
//! vendor status, the auto-NCR on failed incoming inspection (S439 integration),
//! and the heat-lot-required capture.
//!
//! The purchasing lifecycle functions open their own connections (audit lives on
//! the same file), so these tests use a file-backed DuckDB, not in-memory.

use duckdb::{params, Connection};

use aberp::avl_vendors::{create_vendor, ensure_schema as ensure_avl_schema, VendorInputs};
use aberp::purchasing::{
    self, create_po, ensure_schema as ensure_po_schema, record_receipt, transition_po, NewPo,
    NewPoLine, NewReceipt, PoError, PoState, ReceiptLineInput,
};

use aberp_audit_ledger::{ensure_schema as audit_ensure_schema, BinaryHash, TenantId};

const T: &str = "po_e2e_test";

struct Fixture {
    db_path: std::path::PathBuf,
    // ADR-0099 — the shared Handle the purchasing (+ auto-NCR) audit appends
    // route through.
    handle: aberp_db::HandleArc,
    tenant: TenantId,
    hash: BinaryHash,
}

fn setup() -> Fixture {
    let dir = std::env::temp_dir()
        .join("aberp-po-e2e-test")
        .join(ulid::Ulid::new().to_string());
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("aberp.duckdb");
    {
        let conn = Connection::open(&db_path).unwrap();
        audit_ensure_schema(&conn).unwrap();
        ensure_po_schema(&conn).unwrap();
        ensure_avl_schema(&conn).unwrap();
        aberp::quality::ensure_schema(&conn).unwrap();
    }
    let tenant = TenantId::new(T).unwrap();
    let handle = aberp::serve::open_tenant_handle(&db_path, tenant.clone())
        .expect("open shared test Handle");
    Fixture {
        db_path,
        handle,
        tenant,
        hash: BinaryHash::from_bytes([0u8; 32]),
    }
}

/// Seed an AVL vendor at a given status for a partner id (the string the PO
/// references — no partner row is needed, the gate matches by partner_id).
fn seed_vendor(fx: &Fixture, partner_id: &str, status: &str) {
    let conn = Connection::open(&fx.db_path).unwrap();
    create_vendor(
        &conn,
        T,
        &VendorInputs {
            partner_id: partner_id.to_string(),
            approved_status: status.to_string(),
            approval_categories: vec![],
            approved_until_utc: None,
            screening_notes: String::new(),
        },
        "qa",
    )
    .unwrap();
}

fn sample_po(vendor: &str, heat_lot_line: bool) -> NewPo {
    NewPo {
        vendor_partner_id: vendor.to_string(),
        currency: "EUR".to_string(),
        vat_rate_pct: 27,
        expected_delivery_utc: None,
        notes: "qa".to_string(),
        lines: vec![
            NewPoLine {
                product_id: None,
                description: "316L bar stock".into(),
                quantity: 10,
                unit_price_minor: 5000,
                expected_heat_lot_required: heat_lot_line,
            },
            NewPoLine {
                product_id: None,
                description: "fasteners".into(),
                quantity: 4,
                unit_price_minor: 250,
                expected_heat_lot_required: false,
            },
        ],
    }
}

// ADR-0099 — read audit through the SAME shared Handle the migrated purchasing
// fns wrote through (a fresh Connection::open is a separate DuckDB instance that
// does not see the Handle's uncheckpointed WAL; production reads audit via
// state.db.read() too).
fn count_kind(handle: &aberp_db::HandleArc, kind: &str) -> i64 {
    let conn = handle.read().unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM audit_ledger WHERE kind = ?1",
        params![kind],
        |r| r.get(0),
    )
    .unwrap()
}

/// The full happy journey: Approved vendor → create → issue → receive partial →
/// receive remaining → Closed.
#[test]
fn full_procurement_journey_approved_vendor() {
    let fx = setup();
    seed_vendor(&fx, "ptn_approved", "approved");

    let po = create_po(
        &fx.db_path,
        &fx.handle,
        fx.tenant.clone(),
        fx.hash,
        "buyer",
        sample_po("ptn_approved", true),
    )
    .unwrap();
    assert_eq!(po.state, PoState::Draft);
    assert!(po.po_number.starts_with("PO-"));
    assert!(po.po_number.ends_with("-0001"), "first PO of the year");
    // 27% of (10*5000 + 4*250) = 27% of 51000 = 13770.
    assert_eq!(po.subtotal_minor, 51000);
    assert_eq!(po.vat_minor, 13770);
    assert_eq!(po.total_minor, 64770);
    assert_eq!(po.vendor_avl_status.as_deref(), Some("approved"));
    assert_eq!(count_kind(&fx.handle, "po.created"), 1);
    assert_eq!(count_kind(&fx.handle, "po.line_added"), 2);

    // Issue requires an approver.
    let err = transition_po(
        &fx.db_path,
        &fx.handle,
        fx.tenant.clone(),
        fx.hash,
        "buyer",
        &po.po_id,
        PoState::IssuedToVendor,
        None,
    )
    .unwrap_err();
    assert!(matches!(err, PoError::Invalid(_)), "{err:?}");

    let issued = transition_po(
        &fx.db_path,
        &fx.handle,
        fx.tenant.clone(),
        fx.hash,
        "buyer",
        &po.po_id,
        PoState::IssuedToVendor,
        Some("manager"),
    )
    .unwrap();
    assert_eq!(issued.state, PoState::IssuedToVendor);
    assert_eq!(issued.approved_by_operator.as_deref(), Some("manager"));
    assert_eq!(count_kind(&fx.handle, "po.issued"), 1);

    // Find the line ids.
    let conn = Connection::open(&fx.db_path).unwrap();
    let lines = purchasing::list_po_lines(&conn, T, &po.po_id).unwrap();
    drop(conn);
    let bar = &lines[0]; // heat-lot required
    let fasteners = &lines[1];

    // Receive PART of the bar (4 of 10), passing inspection, with a heat lot.
    let after_partial = record_receipt(
        &fx.db_path,
        &fx.handle,
        fx.tenant.clone(),
        fx.hash,
        "receiver",
        &po.po_id,
        NewReceipt {
            delivery_note_number: "DN-1".into(),
            lines: vec![ReceiptLineInput {
                pol_id: bar.pol_id.clone(),
                received_quantity: 4,
                inspection_pass: true,
                inspection_notes: String::new(),
                heat_lot: Some("HL-77".into()),
            }],
        },
    )
    .unwrap();
    assert_eq!(after_partial.state, PoState::PartiallyReceived);
    assert_eq!(count_kind(&fx.handle, "po.receipt_recorded"), 1);
    assert_eq!(count_kind(&fx.handle, "po.partially_received"), 1);

    // Receive the REMAINING bar (6) + all fasteners (4) → fully received.
    let after_full = record_receipt(
        &fx.db_path,
        &fx.handle,
        fx.tenant.clone(),
        fx.hash,
        "receiver",
        &po.po_id,
        NewReceipt {
            delivery_note_number: "DN-2".into(),
            lines: vec![
                ReceiptLineInput {
                    pol_id: bar.pol_id.clone(),
                    received_quantity: 6,
                    inspection_pass: true,
                    inspection_notes: String::new(),
                    heat_lot: Some("HL-78".into()),
                },
                ReceiptLineInput {
                    pol_id: fasteners.pol_id.clone(),
                    received_quantity: 4,
                    inspection_pass: true,
                    inspection_notes: String::new(),
                    heat_lot: None,
                },
            ],
        },
    )
    .unwrap();
    assert_eq!(after_full.state, PoState::Received);
    assert_eq!(count_kind(&fx.handle, "po.received"), 1);

    // Close.
    let closed = transition_po(
        &fx.db_path,
        &fx.handle,
        fx.tenant.clone(),
        fx.hash,
        "buyer",
        &po.po_id,
        PoState::Closed,
        None,
    )
    .unwrap();
    assert_eq!(closed.state, PoState::Closed);
    assert_eq!(count_kind(&fx.handle, "po.closed"), 1);
}

/// AVL gate ([[trust-code-not-operator]]): each status produces the expected
/// create/issue behaviour.
#[test]
fn avl_gate_per_status() {
    let fx = setup();
    seed_vendor(&fx, "ptn_suspended", "suspended");
    seed_vendor(&fx, "ptn_revoked", "revoked");
    seed_vendor(&fx, "ptn_pending", "pending");
    seed_vendor(&fx, "ptn_conditional", "conditional");
    // "ptn_unlisted" intentionally has no AVL row.

    // Suspended → refused at create; PoBlockedByVendorStatus fires.
    let err = create_po(
        &fx.db_path,
        &fx.handle,
        fx.tenant.clone(),
        fx.hash,
        "buyer",
        sample_po("ptn_suspended", false),
    )
    .unwrap_err();
    assert!(
        matches!(err, PoError::BlockedByVendorStatus { .. }),
        "{err:?}"
    );

    // Revoked → refused at create.
    let err = create_po(
        &fx.db_path,
        &fx.handle,
        fx.tenant.clone(),
        fx.hash,
        "buyer",
        sample_po("ptn_revoked", false),
    )
    .unwrap_err();
    assert!(
        matches!(err, PoError::BlockedByVendorStatus { .. }),
        "{err:?}"
    );
    assert_eq!(
        count_kind(&fx.handle, "supplier.po_blocked_by_vendor_status"),
        2,
        "both suspended + revoked fired the S431 gate kind"
    );

    // Pending → create OK (Draft) but issue refused (needs approval first).
    let pending = create_po(
        &fx.db_path,
        &fx.handle,
        fx.tenant.clone(),
        fx.hash,
        "buyer",
        sample_po("ptn_pending", false),
    )
    .unwrap();
    assert_eq!(pending.vendor_avl_status.as_deref(), Some("pending"));
    let err = transition_po(
        &fx.db_path,
        &fx.handle,
        fx.tenant.clone(),
        fx.hash,
        "buyer",
        &pending.po_id,
        PoState::IssuedToVendor,
        Some("manager"),
    )
    .unwrap_err();
    assert!(matches!(err, PoError::IllegalTransition(_)), "{err:?}");

    // Conditional → create OK with snapshot, issue OK (flagged yellow in SPA).
    let cond = create_po(
        &fx.db_path,
        &fx.handle,
        fx.tenant.clone(),
        fx.hash,
        "buyer",
        sample_po("ptn_conditional", false),
    )
    .unwrap();
    assert_eq!(cond.vendor_avl_status.as_deref(), Some("conditional"));
    let issued = transition_po(
        &fx.db_path,
        &fx.handle,
        fx.tenant.clone(),
        fx.hash,
        "buyer",
        &cond.po_id,
        PoState::IssuedToVendor,
        Some("manager"),
    )
    .unwrap();
    assert_eq!(issued.state, PoState::IssuedToVendor);

    // Unlisted partner → no AVL row, no friction; snapshot is None.
    let unlisted = create_po(
        &fx.db_path,
        &fx.handle,
        fx.tenant.clone(),
        fx.hash,
        "buyer",
        sample_po("ptn_unlisted", false),
    )
    .unwrap();
    assert_eq!(unlisted.vendor_avl_status, None);
    transition_po(
        &fx.db_path,
        &fx.handle,
        fx.tenant.clone(),
        fx.hash,
        "buyer",
        &unlisted.po_id,
        PoState::IssuedToVendor,
        Some("manager"),
    )
    .unwrap();
}

/// A failed incoming inspection auto-creates an NCR (S439) linked to the PO line
/// and fires `po.incoming_inspection_failed`.
#[test]
fn failed_inspection_auto_creates_ncr() {
    let fx = setup();
    seed_vendor(&fx, "ptn_approved", "approved");
    let po = create_po(
        &fx.db_path,
        &fx.handle,
        fx.tenant.clone(),
        fx.hash,
        "buyer",
        sample_po("ptn_approved", false),
    )
    .unwrap();
    transition_po(
        &fx.db_path,
        &fx.handle,
        fx.tenant.clone(),
        fx.hash,
        "buyer",
        &po.po_id,
        PoState::IssuedToVendor,
        Some("manager"),
    )
    .unwrap();
    let conn = Connection::open(&fx.db_path).unwrap();
    let lines = purchasing::list_po_lines(&conn, T, &po.po_id).unwrap();
    drop(conn);

    // Receive the bar but FAIL inspection.
    record_receipt(
        &fx.db_path,
        &fx.handle,
        fx.tenant.clone(),
        fx.hash,
        "receiver",
        &po.po_id,
        NewReceipt {
            delivery_note_number: "DN-bad".into(),
            lines: vec![ReceiptLineInput {
                pol_id: lines[0].pol_id.clone(),
                received_quantity: 10,
                inspection_pass: false,
                inspection_notes: "pitting on surface".into(),
                heat_lot: None,
            }],
        },
    )
    .unwrap();

    assert_eq!(count_kind(&fx.handle, "po.incoming_inspection_failed"), 1);
    assert_eq!(count_kind(&fx.handle, "ncr.created"), 1, "auto-NCR created");

    // The receipt row is linked to the NCR; the NCR is a SupplierIssue.
    let conn = Connection::open(&fx.db_path).unwrap();
    let receipts = purchasing::list_po_receipts(&conn, T, &po.po_id).unwrap();
    assert_eq!(receipts.len(), 1);
    let ncr_id = receipts[0].ncr_id.clone().expect("receipt linked to NCR");
    let ncr = aberp::quality::get_ncr(&conn, T, &ncr_id).unwrap().unwrap();
    assert_eq!(ncr.category, aberp::quality::NcrCategory::SupplierIssue);
    assert!(ncr.description.contains(&po.po_number));
}

/// Heat-lot capture: receiving a line with `expected_heat_lot_required` and no
/// heat lot is refused ([[trust-code-not-operator]]).
#[test]
fn heat_lot_required_on_receipt() {
    let fx = setup();
    seed_vendor(&fx, "ptn_approved", "approved");
    let po = create_po(
        &fx.db_path,
        &fx.handle,
        fx.tenant.clone(),
        fx.hash,
        "buyer",
        sample_po("ptn_approved", true), // line 0 requires heat lot
    )
    .unwrap();
    transition_po(
        &fx.db_path,
        &fx.handle,
        fx.tenant.clone(),
        fx.hash,
        "buyer",
        &po.po_id,
        PoState::IssuedToVendor,
        Some("manager"),
    )
    .unwrap();
    let conn = Connection::open(&fx.db_path).unwrap();
    let lines = purchasing::list_po_lines(&conn, T, &po.po_id).unwrap();
    drop(conn);

    // No heat lot on the required line → refused.
    let err = record_receipt(
        &fx.db_path,
        &fx.handle,
        fx.tenant.clone(),
        fx.hash,
        "receiver",
        &po.po_id,
        NewReceipt {
            delivery_note_number: "DN-1".into(),
            lines: vec![ReceiptLineInput {
                pol_id: lines[0].pol_id.clone(),
                received_quantity: 10,
                inspection_pass: true,
                inspection_notes: String::new(),
                heat_lot: None,
            }],
        },
    )
    .unwrap_err();
    assert!(matches!(err, PoError::Invalid(_)), "{err:?}");

    // With a heat lot → accepted.
    record_receipt(
        &fx.db_path,
        &fx.handle,
        fx.tenant.clone(),
        fx.hash,
        "receiver",
        &po.po_id,
        NewReceipt {
            delivery_note_number: "DN-1".into(),
            lines: vec![ReceiptLineInput {
                pol_id: lines[0].pol_id.clone(),
                received_quantity: 10,
                inspection_pass: true,
                inspection_notes: String::new(),
                heat_lot: Some("HL-99".into()),
            }],
        },
    )
    .unwrap();
}

/// PO numbers are monotonic + gap-free within a tenant-year (through the real
/// create path).
#[test]
fn po_numbers_are_sequential_through_create() {
    let fx = setup();
    seed_vendor(&fx, "ptn_approved", "approved");
    let mut numbers = Vec::new();
    for _ in 0..3 {
        let po = create_po(
            &fx.db_path,
            &fx.handle,
            fx.tenant.clone(),
            fx.hash,
            "buyer",
            sample_po("ptn_approved", false),
        )
        .unwrap();
        numbers.push(po.po_number);
    }
    assert!(numbers[0].ends_with("-0001"));
    assert!(numbers[1].ends_with("-0002"));
    assert!(numbers[2].ends_with("-0003"));
}
