//! S438 (ADR-0089) — part-UID shipment gate + Part UID Lookup e2e.
//!
//! Walks the customer journey the brief names ([[customer-journey-e2e-gate]]):
//! defense quote → WO → heat lot (S432) → part UID marked → traceability lookup
//! returns the full chain, plus the refuse-Shipment invariant
//! ([[trust-code-not-operator]]): a defense/aerospace dispatch whose WO has any
//! unmarked unit is BLOCKED; the non-defense path and the same dispatch once
//! every unit is marked PASS.

use duckdb::{params, Connection};

use aberp::material_inventory::{assign_heat_lot, ensure_schema as ensure_inventory_schema};
use aberp::part_marking::{
    data_matrix_payload, ensure_schema as ensure_part_schema, generate_part_uid, record_part_marks,
    trace_customer, trace_part_uid, PartMark, PartMarkError,
};
use aberp::partners::{create_partner, CustomerType, PartnerInputs, PartnerKind};
use aberp::serve::{resolve_part_uid_gate, PartUidGate};

use aberp_audit_ledger::ensure_schema as audit_ensure_schema;

const T: &str = "part_uid_gate_test";

fn setup() -> Connection {
    let conn = Connection::open_in_memory().expect("open in-memory duckdb");
    audit_ensure_schema(&conn).unwrap();
    ensure_inventory_schema(&conn).unwrap();
    ensure_part_schema(&conn).unwrap();
    aberp_work_orders::ensure_schema(&conn).unwrap();
    aberp_dispatch::ensure_schema(&conn).unwrap();
    aberp::quote_pricing_jobs::ensure_schema(&conn).unwrap();
    conn
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

fn seed_balance_with_heat(conn: &Connection, grade: &str, heat: &str) {
    conn.execute(
        "INSERT INTO inventory_balances (
            tenant_id, material_grade, on_hand_qty, reserved_qty,
            committed_qty, consumed_qty, unit_of_measure, last_updated
         ) VALUES (?1, ?2, 100.0, 0, 0, 0, 'kg', '2026-06-06T00:00:00Z')",
        params![T, grade],
    )
    .unwrap();
    assign_heat_lot(conn, T, grade, heat, "file:///c.pdf", "op").unwrap();
}

fn seed_quote(conn: &Connection, quote_id: &str, grade: &str, buyer_partner_id: &str) {
    conn.execute(
        "INSERT INTO quote_pricing_jobs (
            quote_id, tenant_id, state, fetched_at, updated_at,
            customer_email, customer_name, customer_company, material_grade, quantity,
            cad_filename, cad_local_path, attempt_n, buyer_partner_id
         ) VALUES (?1, ?2, 'posted', '2026-06-06T00:00:00Z', '2026-06-06T00:00:00Z',
                   'a@b.c', 'N', 'Co', ?3, 2, 'p.stl', '/tmp/p.stl', 1, ?4)",
        params![quote_id, T, grade, buyer_partner_id],
    )
    .unwrap();
}

/// Seed a Completed WO with `qty` target.
fn seed_wo(conn: &Connection, wo_id: &str, source_quote_id: &str, qty: &str) {
    conn.execute(
        "INSERT INTO work_orders (
            wo_id, tenant_id, wo_number, product_id, qty_target, state,
            created_at, source_quote_id
         ) VALUES (?1, ?2, ?3, 'prd_1', ?4, 'completed', '2026-06-06T00:00:00Z', ?5)",
        params![wo_id, T, wo_id, qty, source_quote_id],
    )
    .unwrap();
}

fn seed_dispatch(conn: &Connection, dsp_id: &str, wo_id: &str, partner_id: &str, state: &str) {
    conn.execute(
        "INSERT INTO dispatches (dsp_id, tenant_id, wo_id, partner_id, state, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, '2026-06-06T00:00:00Z')",
        params![dsp_id, T, wo_id, partner_id, state],
    )
    .unwrap();
}

fn get_dispatch(conn: &Connection, dsp_id: &str) -> aberp_dispatch::Dispatch {
    aberp_dispatch::get_dispatch(conn, T, dsp_id)
        .unwrap()
        .expect("read seeded dispatch")
}

/// Build + record `n` marks for a WO, returning the minted part UIDs.
fn mark_units(conn: &Connection, wo_id: &str, heat: Option<&str>, n: u32) -> Vec<String> {
    let mut marks = Vec::new();
    for i in 1..=n {
        let part_uid = generate_part_uid();
        let serial = format!("SN-{i}");
        let payload = data_matrix_payload(&part_uid, &serial, heat);
        marks.push(PartMark {
            wo_id: wo_id.to_string(),
            unit_index: i,
            part_uid,
            serial_number: serial,
            data_matrix_payload: payload,
            heat_lot_reference: heat.map(str::to_string),
            marked_at_utc: "2026-06-16T00:00:00Z".to_string(),
            marked_by_operator: "op".to_string(),
        });
    }
    record_part_marks(conn, T, wo_id, &marks).unwrap();
    marks.into_iter().map(|m| m.part_uid).collect()
}

/// Defense dispatch BLOCKED until every unit is marked → then PASS.
#[test]
fn defense_dispatch_blocked_until_all_units_marked() {
    let conn = setup();
    let buyer = create_partner(&conn, T, &partner_inputs("Def Co", CustomerType::Defense)).unwrap();
    seed_balance_with_heat(&conn, "Ti-6Al-4V", "HEAT-9F3A");
    seed_quote(&conn, "q-def", "Ti-6Al-4V", &buyer.id);
    seed_wo(&conn, "wo-def", "q-def", "2");
    seed_dispatch(&conn, "dsp-def", "wo-def", &buyer.id, "drafted");
    let dispatch = get_dispatch(&conn, "dsp-def");

    // No marks → Blocked, naming the WO + expected/marked counts.
    match resolve_part_uid_gate(&conn, T, &dispatch).unwrap() {
        PartUidGate::Blocked {
            work_order_id,
            qty_target,
            marked_count,
            customer_type,
        } => {
            assert_eq!(work_order_id, "wo-def");
            assert_eq!(qty_target, 2);
            assert_eq!(marked_count, 0);
            assert_eq!(customer_type, "defense");
        }
        other => panic!("expected Blocked, got {other:?}"),
    }

    // Mark ONE of two → still Blocked (partial coverage is not enough).
    mark_units(&conn, "wo-def", Some("HEAT-9F3A"), 1);
    assert!(matches!(
        resolve_part_uid_gate(&conn, T, &dispatch).unwrap(),
        PartUidGate::Blocked {
            marked_count: 1,
            ..
        }
    ));
}

/// All units marked → PASS.
#[test]
fn defense_dispatch_passes_when_fully_marked() {
    let conn = setup();
    let buyer = create_partner(&conn, T, &partner_inputs("Def Co", CustomerType::Defense)).unwrap();
    seed_balance_with_heat(&conn, "Ti-6Al-4V", "HEAT-9F3A");
    seed_quote(&conn, "q-def", "Ti-6Al-4V", &buyer.id);
    seed_wo(&conn, "wo-def", "q-def", "2");
    seed_dispatch(&conn, "dsp-def", "wo-def", &buyer.id, "drafted");
    mark_units(&conn, "wo-def", Some("HEAT-9F3A"), 2);
    let dispatch = get_dispatch(&conn, "dsp-def");
    assert_eq!(
        resolve_part_uid_gate(&conn, T, &dispatch).unwrap(),
        PartUidGate::Pass
    );
}

/// The COMMERCIAL path is unaffected: an Industrial buyer's dispatch ships even
/// with zero marked units.
#[test]
fn non_defense_dispatch_passes_unmarked() {
    let conn = setup();
    let buyer = create_partner(
        &conn,
        T,
        &partner_inputs("Ind Co", CustomerType::Industrial),
    )
    .unwrap();
    seed_balance_with_heat(&conn, "6061-T6", "HEAT-X");
    seed_quote(&conn, "q-ind", "6061-T6", &buyer.id);
    seed_wo(&conn, "wo-ind", "q-ind", "5");
    seed_dispatch(&conn, "dsp-ind", "wo-ind", &buyer.id, "drafted");
    let dispatch = get_dispatch(&conn, "dsp-ind");
    assert_eq!(
        resolve_part_uid_gate(&conn, T, &dispatch).unwrap(),
        PartUidGate::Pass
    );
}

/// Re-marking a WO is REFUSED ([[hulye-biztos]] — mint once).
#[test]
fn double_marking_is_refused() {
    let conn = setup();
    seed_wo_no_quote(&conn, "wo-x", "3");
    mark_units(&conn, "wo-x", None, 3);
    let again = vec![PartMark {
        wo_id: "wo-x".to_string(),
        unit_index: 1,
        part_uid: generate_part_uid(),
        serial_number: "SN-1".to_string(),
        data_matrix_payload: "x".to_string(),
        heat_lot_reference: None,
        marked_at_utc: "2026-06-16T00:00:00Z".to_string(),
        marked_by_operator: "op".to_string(),
    }];
    let err = record_part_marks(&conn, T, "wo-x", &again).unwrap_err();
    assert!(matches!(err, PartMarkError::AlreadyMarked { n: 3, .. }));
}

fn seed_wo_no_quote(conn: &Connection, wo_id: &str, qty: &str) {
    conn.execute(
        "INSERT INTO work_orders (
            wo_id, tenant_id, wo_number, product_id, qty_target, state, created_at
         ) VALUES (?1, ?2, ?3, 'prd_1', ?4, 'completed', '2026-06-06T00:00:00Z')",
        params![wo_id, T, wo_id, qty],
    )
    .unwrap();
}

/// Full chain: defense quote → WO → heat lot → part UID → forward + reverse
/// traceability return the chain.
#[test]
fn traceability_forward_and_reverse() {
    let conn = setup();
    let buyer = create_partner(&conn, T, &partner_inputs("Def Co", CustomerType::Defense)).unwrap();
    seed_balance_with_heat(&conn, "Ti-6Al-4V", "HEAT-9F3A");
    seed_quote(&conn, "q-def", "Ti-6Al-4V", &buyer.id);
    seed_wo(&conn, "wo-def", "q-def", "2");
    // Shipped dispatch so the reverse (customer) trace resolves.
    seed_dispatch(&conn, "dsp-def", "wo-def", &buyer.id, "shipped");
    let uids = mark_units(&conn, "wo-def", Some("HEAT-9F3A"), 2);

    // Forward: part_uid → WO + heat lot + quote + customer.
    let fwd = trace_part_uid(&conn, T, &uids[0]).unwrap();
    assert!(fwd.found);
    assert_eq!(fwd.parts.len(), 1);
    let row = &fwd.parts[0];
    assert_eq!(row.part_uid, uids[0]);
    assert_eq!(row.wo_id, "wo-def");
    assert_eq!(row.heat_lot_reference.as_deref(), Some("HEAT-9F3A"));
    assert_eq!(row.source_quote_id.as_deref(), Some("q-def"));
    assert_eq!(row.customer_partner_id.as_deref(), Some(buyer.id.as_str()));
    assert_eq!(row.customer_name.as_deref(), Some("Def Co"));

    // An unknown part UID is a clean miss (no chain).
    let miss = trace_part_uid(&conn, T, "dp-00000000000000000000000000").unwrap();
    assert!(!miss.found);
    assert!(miss.parts.is_empty());

    // Reverse: customer → every part UID Shipped to them (both units).
    let rev = trace_customer(&conn, T, &buyer.id).unwrap();
    assert!(rev.found);
    assert_eq!(rev.parts.len(), 2);
    let mut got: Vec<&str> = rev.parts.iter().map(|p| p.part_uid.as_str()).collect();
    got.sort();
    let mut want: Vec<&str> = uids.iter().map(String::as_str).collect();
    want.sort();
    assert_eq!(got, want);
}

/// Reverse trace only counts SHIPPED dispatches — a Drafted dispatch's parts
/// are not yet "shipped to" the customer.
#[test]
fn reverse_trace_excludes_unshipped() {
    let conn = setup();
    let buyer = create_partner(&conn, T, &partner_inputs("Def Co", CustomerType::Defense)).unwrap();
    seed_wo_no_quote(&conn, "wo-draft", "1");
    seed_dispatch(&conn, "dsp-draft", "wo-draft", &buyer.id, "drafted");
    mark_units(&conn, "wo-draft", None, 1);
    let rev = trace_customer(&conn, T, &buyer.id).unwrap();
    assert!(!rev.found, "drafted dispatch parts are not yet shipped");
}
