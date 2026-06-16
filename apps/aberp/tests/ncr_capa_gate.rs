//! S439 (ADR-0090) — NCR / CAPA quality workflow + open-NCR shipment gate e2e.
//!
//! Walks the defense quality journey the brief names
//! ([[customer-journey-e2e-gate]]): defense WO → part marked → NCR opened →
//! shipment BLOCKED → CAPA created + approved + verified → NCR closed → shipment
//! gate PASSES. Plus the refuse-Shipment invariants ([[trust-code-not-operator]]):
//! a defense dispatch whose WO has an Open/Contained NCR on a unit is blocked;
//! the non-defense path is unaffected; a closed NCR unblocks.
//!
//! The quality lifecycle functions open their own connections (audit lives on
//! the same file), so these tests use a file-backed DuckDB, not in-memory.

use duckdb::{params, Connection};

use aberp::part_marking::{
    data_matrix_payload, ensure_schema as ensure_part_schema, generate_part_uid, record_part_marks,
    PartMark,
};
use aberp::partners::{create_partner, CustomerType, PartnerInputs, PartnerKind};
use aberp::quality::{
    self, ensure_schema as ensure_quality_schema, CapaVerdict, NcrCategory, NcrSeverity, NcrState,
    NewCapa, NewNcr,
};
use aberp::serve::{resolve_open_ncr_gate, OpenNcrGate};

use aberp_audit_ledger::{ensure_schema as audit_ensure_schema, BinaryHash, TenantId};

const T: &str = "ncr_capa_gate_test";

struct Fixture {
    db_path: std::path::PathBuf,
    tenant: TenantId,
    hash: BinaryHash,
}

fn setup() -> Fixture {
    let dir = std::env::temp_dir()
        .join("aberp-ncr-gate-test")
        .join(ulid::Ulid::new().to_string());
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("aberp.duckdb");
    {
        let conn = Connection::open(&db_path).unwrap();
        audit_ensure_schema(&conn).unwrap();
        ensure_part_schema(&conn).unwrap();
        ensure_quality_schema(&conn).unwrap();
        aberp_work_orders::ensure_schema(&conn).unwrap();
        aberp_dispatch::ensure_schema(&conn).unwrap();
        aberp::quote_pricing_jobs::ensure_schema(&conn).unwrap();
    }
    Fixture {
        db_path,
        tenant: TenantId::new(T).unwrap(),
        hash: BinaryHash::from_bytes([0u8; 32]),
    }
}

fn partner_inputs(name: &str, ct: CustomerType) -> PartnerInputs {
    PartnerInputs {
        display_name: name.to_string(),
        legal_name: name.to_string(),
        kind: PartnerKind::Customer,
        customer_vat_status: Default::default(),
        customer_type: ct,
        tax_number: None,
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

fn seed_wo(conn: &Connection, wo_id: &str, qty: &str) {
    conn.execute(
        "INSERT INTO work_orders (
            wo_id, tenant_id, wo_number, product_id, qty_target, state, created_at
         ) VALUES (?1, ?2, ?3, 'prd_1', ?4, 'completed', '2026-06-06T00:00:00Z')",
        params![wo_id, T, wo_id, qty],
    )
    .unwrap();
}

fn seed_dispatch(conn: &Connection, dsp_id: &str, wo_id: &str, partner_id: &str, st: &str) {
    conn.execute(
        "INSERT INTO dispatches (dsp_id, tenant_id, wo_id, partner_id, state, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, '2026-06-06T00:00:00Z')",
        params![dsp_id, T, wo_id, partner_id, st],
    )
    .unwrap();
}

fn mark_units(conn: &Connection, wo_id: &str, n: u32) -> Vec<String> {
    let mut marks = Vec::new();
    for i in 1..=n {
        let part_uid = generate_part_uid();
        let serial = format!("SN-{i}");
        let payload = data_matrix_payload(&part_uid, &serial, None);
        marks.push(PartMark {
            wo_id: wo_id.to_string(),
            unit_index: i,
            part_uid,
            serial_number: serial,
            data_matrix_payload: payload,
            heat_lot_reference: None,
            marked_at_utc: "2026-06-16T00:00:00Z".to_string(),
            marked_by_operator: "op".to_string(),
        });
    }
    record_part_marks(conn, T, wo_id, &marks).unwrap();
    marks.into_iter().map(|m| m.part_uid).collect()
}

fn get_dispatch(conn: &Connection, dsp_id: &str) -> aberp_dispatch::Dispatch {
    aberp_dispatch::get_dispatch(conn, T, dsp_id)
        .unwrap()
        .expect("read seeded dispatch")
}

fn open_ncr_on(fx: &Fixture, part_uids: &[String]) -> String {
    quality::create_ncr(
        &fx.db_path,
        fx.tenant.clone(),
        fx.hash,
        "qa",
        NewNcr {
            severity: NcrSeverity::Major,
            category: NcrCategory::Workmanship,
            description: "surface finish out of spec".into(),
            affected_part_uids: part_uids.to_vec(),
            affected_wo_ids: vec![],
            affected_heat_lots: vec![],
            photos: vec![],
        },
    )
    .unwrap()
    .ncr_id
}

/// A defense dispatch whose WO has a unit referenced by an Open NCR is BLOCKED;
/// resolving + closing that NCR unblocks it.
#[test]
fn defense_dispatch_blocked_by_open_ncr_then_unblocked_when_closed() {
    let fx = setup();
    let conn = Connection::open(&fx.db_path).unwrap();
    let buyer = create_partner(&conn, T, &partner_inputs("Def Co", CustomerType::Defense)).unwrap();
    seed_wo(&conn, "wo-def", "2");
    seed_dispatch(&conn, "dsp-def", "wo-def", &buyer.id, "drafted");
    let uids = mark_units(&conn, "wo-def", 2);
    drop(conn);

    // Open an NCR on unit 1 → defense gate BLOCKS, naming the NCR.
    let ncr_id = open_ncr_on(&fx, &[uids[0].clone()]);
    let conn = Connection::open(&fx.db_path).unwrap();
    let dispatch = get_dispatch(&conn, "dsp-def");
    match resolve_open_ncr_gate(&conn, T, &dispatch).unwrap() {
        OpenNcrGate::Blocked {
            work_order_id,
            customer_type,
            blocking_ncr_ids,
        } => {
            assert_eq!(work_order_id, "wo-def");
            assert_eq!(customer_type, "defense");
            assert_eq!(blocking_ncr_ids, vec![ncr_id.clone()]);
        }
        other => panic!("expected Blocked, got {other:?}"),
    }
    drop(conn);

    // Contained still blocks (brief §4: Open OR Contained).
    quality::transition_ncr(
        &fx.db_path,
        fx.tenant.clone(),
        fx.hash,
        "qa",
        &ncr_id,
        NcrState::Contained,
        "",
    )
    .unwrap();
    let conn = Connection::open(&fx.db_path).unwrap();
    assert!(matches!(
        resolve_open_ncr_gate(&conn, T, &get_dispatch(&conn, "dsp-def")).unwrap(),
        OpenNcrGate::Blocked { .. }
    ));
    drop(conn);

    // Drive to close with a verified CAPA → gate PASSES.
    quality::transition_ncr(
        &fx.db_path,
        fx.tenant.clone(),
        fx.hash,
        "qa",
        &ncr_id,
        NcrState::UnderInvestigation,
        "",
    )
    .unwrap();
    quality::transition_ncr(
        &fx.db_path,
        fx.tenant.clone(),
        fx.hash,
        "qa",
        &ncr_id,
        NcrState::CorrectionApplied,
        "",
    )
    .unwrap();
    let capa = quality::create_capa(
        &fx.db_path,
        fx.tenant.clone(),
        fx.hash,
        "qa",
        NewCapa {
            ncr_id: ncr_id.clone(),
            corrective_action_text: "re-polish".into(),
            preventive_action_text: "tighten op sheet".into(),
            responsible_operator: "qa".into(),
            target_close_date: "2026-07-01".into(),
        },
    )
    .unwrap();
    quality::approve_capa(&fx.db_path, fx.tenant.clone(), fx.hash, "qa", &capa.capa_id).unwrap();
    quality::review_capa_effectiveness(
        &fx.db_path,
        fx.tenant.clone(),
        fx.hash,
        "qa",
        &capa.capa_id,
        CapaVerdict::Verified,
        "holds",
    )
    .unwrap();
    let closed = quality::transition_ncr(
        &fx.db_path,
        fx.tenant.clone(),
        fx.hash,
        "qa",
        &ncr_id,
        NcrState::Closed,
        "done",
    )
    .unwrap();
    assert_eq!(closed.state, NcrState::Closed);

    let conn = Connection::open(&fx.db_path).unwrap();
    assert_eq!(
        resolve_open_ncr_gate(&conn, T, &get_dispatch(&conn, "dsp-def")).unwrap(),
        OpenNcrGate::Pass,
        "closed NCR no longer blocks shipment"
    );
}

/// The COMMERCIAL path is unaffected: an Industrial buyer's dispatch ships even
/// with an Open NCR on its part.
#[test]
fn non_defense_dispatch_unaffected_by_open_ncr() {
    let fx = setup();
    let conn = Connection::open(&fx.db_path).unwrap();
    let buyer = create_partner(
        &conn,
        T,
        &partner_inputs("Ind Co", CustomerType::Industrial),
    )
    .unwrap();
    seed_wo(&conn, "wo-ind", "1");
    seed_dispatch(&conn, "dsp-ind", "wo-ind", &buyer.id, "drafted");
    let uids = mark_units(&conn, "wo-ind", 1);
    drop(conn);

    open_ncr_on(&fx, &uids);
    let conn = Connection::open(&fx.db_path).unwrap();
    assert_eq!(
        resolve_open_ncr_gate(&conn, T, &get_dispatch(&conn, "dsp-ind")).unwrap(),
        OpenNcrGate::Pass,
        "non-defense path is never gated by NCRs"
    );
}

/// An NCR on a DIFFERENT part UID does not block a WO whose units are clean.
#[test]
fn open_ncr_on_other_part_does_not_block() {
    let fx = setup();
    let conn = Connection::open(&fx.db_path).unwrap();
    let buyer = create_partner(&conn, T, &partner_inputs("Def Co", CustomerType::Defense)).unwrap();
    seed_wo(&conn, "wo-def", "1");
    seed_dispatch(&conn, "dsp-def", "wo-def", &buyer.id, "drafted");
    mark_units(&conn, "wo-def", 1);
    drop(conn);

    // NCR references an unrelated UID.
    open_ncr_on(&fx, &["dp-0000000000000000000000000Z".to_string()]);
    let conn = Connection::open(&fx.db_path).unwrap();
    assert_eq!(
        resolve_open_ncr_gate(&conn, T, &get_dispatch(&conn, "dsp-def")).unwrap(),
        OpenNcrGate::Pass
    );
}

/// Full quality loop fires every NCR/CAPA EventKind exactly where expected.
#[test]
fn full_loop_fires_all_quality_events() {
    let fx = setup();
    let ncr_id = open_ncr_on(&fx, &["dp-AAAAAAAAAAAAAAAAAAAAAAAAAA".to_string()]);
    quality::transition_ncr(
        &fx.db_path,
        fx.tenant.clone(),
        fx.hash,
        "qa",
        &ncr_id,
        NcrState::Contained,
        "",
    )
    .unwrap();
    quality::transition_ncr(
        &fx.db_path,
        fx.tenant.clone(),
        fx.hash,
        "qa",
        &ncr_id,
        NcrState::UnderInvestigation,
        "",
    )
    .unwrap();
    quality::transition_ncr(
        &fx.db_path,
        fx.tenant.clone(),
        fx.hash,
        "qa",
        &ncr_id,
        NcrState::CorrectionApplied,
        "",
    )
    .unwrap();
    let capa = quality::create_capa(
        &fx.db_path,
        fx.tenant.clone(),
        fx.hash,
        "qa",
        NewCapa {
            ncr_id: ncr_id.clone(),
            corrective_action_text: "c".into(),
            preventive_action_text: "p".into(),
            responsible_operator: "qa".into(),
            target_close_date: "2026-07-01".into(),
        },
    )
    .unwrap();
    quality::approve_capa(&fx.db_path, fx.tenant.clone(), fx.hash, "qa", &capa.capa_id).unwrap();
    quality::review_capa_effectiveness(
        &fx.db_path,
        fx.tenant.clone(),
        fx.hash,
        "qa",
        &capa.capa_id,
        CapaVerdict::Verified,
        "ok",
    )
    .unwrap();
    quality::close_capa(&fx.db_path, fx.tenant.clone(), fx.hash, "qa", &capa.capa_id).unwrap();
    quality::transition_ncr(
        &fx.db_path,
        fx.tenant.clone(),
        fx.hash,
        "qa",
        &ncr_id,
        NcrState::Closed,
        "done",
    )
    .unwrap();

    let conn = Connection::open(&fx.db_path).unwrap();
    let count = |kind: &str| -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM audit_ledger WHERE kind = ?1",
            params![kind],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(count("ncr.created"), 1);
    assert_eq!(
        count("ncr.state_changed"),
        4,
        "contained, under_inv, corr_applied, closed"
    );
    assert_eq!(count("ncr.closed"), 1);
    assert_eq!(count("capa.created"), 1);
    assert_eq!(count("capa.approved"), 1);
    assert_eq!(count("capa.effectiveness_reviewed"), 1);
    assert_eq!(count("capa.closed"), 1);
}
