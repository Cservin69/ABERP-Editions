//! S432 (ADR-0085) — heat-lot chain-of-custody gate + traceability e2e.
//!
//! Walks the customer journey the brief names ([[customer-journey-e2e-gate]]):
//! material-add → heat-lot-assigned → defense-quote-WO → trace-view shows the
//! chain, plus the refuse-WO-start invariant ([[trust-code-not-operator]]):
//! a defense/aerospace WO with no heat lot is BLOCKED; the non-defense path and
//! the same WO once a heat lot is assigned PASS.

use duckdb::{params, Connection};

use aberp::material_inventory::{assign_heat_lot, ensure_schema as ensure_inventory_schema};
use aberp::material_traceability::{trace, TraceQueryKind};
use aberp::partners::{create_partner, CustomerType, PartnerInputs, PartnerKind};
use aberp::serve::{resolve_heat_lot_gate, HeatLotGate};

use aberp_audit_ledger::ensure_schema as audit_ensure_schema;

const T: &str = "heat_lot_gate_test";

fn setup() -> Connection {
    let conn = Connection::open_in_memory().expect("open in-memory duckdb");
    audit_ensure_schema(&conn).unwrap();
    ensure_inventory_schema(&conn).unwrap();
    aberp_work_orders::ensure_schema(&conn).unwrap();
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

fn seed_balance(conn: &Connection, grade: &str) {
    conn.execute(
        "INSERT INTO inventory_balances (
            tenant_id, material_grade, on_hand_qty, reserved_qty,
            committed_qty, consumed_qty, unit_of_measure, last_updated
         ) VALUES (?1, ?2, 100.0, 0, 0, 0, 'kg', '2026-06-06T00:00:00Z')",
        params![T, grade],
    )
    .unwrap();
}

fn seed_quote(conn: &Connection, quote_id: &str, grade: &str, buyer_partner_id: &str) {
    conn.execute(
        "INSERT INTO quote_pricing_jobs (
            quote_id, tenant_id, state, fetched_at, updated_at,
            customer_email, customer_name, customer_company, material_grade, quantity,
            cad_filename, cad_local_path, attempt_n, buyer_partner_id
         ) VALUES (?1, ?2, 'posted', '2026-06-06T00:00:00Z', '2026-06-06T00:00:00Z',
                   'a@b.c', 'N', 'Co', ?3, 1, 'p.stl', '/tmp/p.stl', 1, ?4)",
        params![quote_id, T, grade, buyer_partner_id],
    )
    .unwrap();
}

fn seed_wo(conn: &Connection, wo_id: &str, source_quote_id: &str) -> aberp_work_orders::WorkOrder {
    conn.execute(
        "INSERT INTO work_orders (
            wo_id, tenant_id, wo_number, product_id, qty_target, state,
            created_at, source_quote_id
         ) VALUES (?1, ?2, ?3, 'prd_1', '1', 'released', '2026-06-06T00:00:00Z', ?4)",
        params![wo_id, T, wo_id, source_quote_id],
    )
    .unwrap();
    aberp_work_orders::read_work_order(conn, T, wo_id)
        .unwrap()
        .expect("read seeded WO")
}

/// Defense WO with NO heat lot → BLOCKED; assigning a heat lot → PASS.
/// The same flow exercises material-add → heat-lot-assigned → defense WO.
#[test]
fn defense_wo_blocked_until_heat_lot_assigned() {
    let conn = setup();
    let buyer = create_partner(&conn, T, &partner_inputs("Def Co", CustomerType::Defense)).unwrap();
    seed_balance(&conn, "Ti-6Al-4V");
    seed_quote(&conn, "q-def", "Ti-6Al-4V", &buyer.id);
    let wo = seed_wo(&conn, "wo-def", "q-def");

    // No heat lot yet → Blocked, naming the grade + customer_type.
    match resolve_heat_lot_gate(&conn, T, &wo).unwrap() {
        HeatLotGate::Blocked {
            material_grade,
            customer_type,
            source_quote_id,
        } => {
            assert_eq!(material_grade, "Ti-6Al-4V");
            assert_eq!(customer_type, "defense");
            assert_eq!(source_quote_id, "q-def");
        }
        other => panic!("expected Blocked, got {other:?}"),
    }

    // Assign the heat lot → gate now passes.
    assign_heat_lot(&conn, T, "Ti-6Al-4V", "HEAT-9F3A", "file:///c.pdf", "op").unwrap();
    assert_eq!(
        resolve_heat_lot_gate(&conn, T, &wo).unwrap(),
        HeatLotGate::Pass
    );
}

/// Aerospace is gated identically to Defense.
#[test]
fn aerospace_wo_blocked_without_heat_lot() {
    let conn = setup();
    let buyer = create_partner(
        &conn,
        T,
        &partner_inputs("Aero Co", CustomerType::Aerospace),
    )
    .unwrap();
    seed_balance(&conn, "Inconel-718");
    seed_quote(&conn, "q-aero", "Inconel-718", &buyer.id);
    let wo = seed_wo(&conn, "wo-aero", "q-aero");
    assert!(matches!(
        resolve_heat_lot_gate(&conn, T, &wo).unwrap(),
        HeatLotGate::Blocked { .. }
    ));
}

/// The COMMERCIAL path is unaffected: an Industrial buyer's WO starts even with
/// no heat lot assigned.
#[test]
fn non_defense_wo_passes_without_heat_lot() {
    let conn = setup();
    let buyer = create_partner(
        &conn,
        T,
        &partner_inputs("Ind Co", CustomerType::Industrial),
    )
    .unwrap();
    seed_balance(&conn, "6061-T6");
    seed_quote(&conn, "q-ind", "6061-T6", &buyer.id);
    let wo = seed_wo(&conn, "wo-ind", "q-ind");
    assert_eq!(
        resolve_heat_lot_gate(&conn, T, &wo).unwrap(),
        HeatLotGate::Pass
    );
}

/// A WO with no originating quote (operator-authored) is never gated.
#[test]
fn operator_wo_without_quote_passes() {
    let conn = setup();
    let wo = seed_wo_no_quote(&conn, "wo-op");
    assert_eq!(
        resolve_heat_lot_gate(&conn, T, &wo).unwrap(),
        HeatLotGate::Pass
    );
}

fn seed_wo_no_quote(conn: &Connection, wo_id: &str) -> aberp_work_orders::WorkOrder {
    conn.execute(
        "INSERT INTO work_orders (
            wo_id, tenant_id, wo_number, product_id, qty_target, state, created_at
         ) VALUES (?1, ?2, ?3, 'prd_1', '1', 'released', '2026-06-06T00:00:00Z')",
        params![wo_id, T, wo_id],
    )
    .unwrap();
    aberp_work_orders::read_work_order(conn, T, wo_id)
        .unwrap()
        .unwrap()
}

/// trace-view shows the full chain once the journey has run: the material with
/// its heat lot, the originating quote, and the WO — with the un-tracked
/// invoice leg surfaced as a placeholder, not omitted.
#[test]
fn traceability_view_shows_the_chain() {
    let conn = setup();
    let buyer = create_partner(&conn, T, &partner_inputs("Def Co", CustomerType::Defense)).unwrap();
    seed_balance(&conn, "Ti-6Al-4V");
    assign_heat_lot(&conn, T, "Ti-6Al-4V", "HEAT-9F3A", "file:///c.pdf", "op").unwrap();
    seed_quote(&conn, "q-def", "Ti-6Al-4V", &buyer.id);
    seed_wo(&conn, "wo-def", "q-def");

    // By material id.
    let rep = trace(&conn, T, TraceQueryKind::MaterialId, "Ti-6Al-4V").unwrap();
    assert_eq!(rep.material_id.as_deref(), Some("Ti-6Al-4V"));
    assert_eq!(
        rep.material.unwrap().heat_lot_number.as_deref(),
        Some("HEAT-9F3A")
    );
    assert_eq!(rep.quotes.len(), 1);
    assert_eq!(rep.work_orders.len(), 1);
    assert_eq!(rep.work_orders[0].wo_id, "wo-def");
    assert!(rep.invoices.is_empty());
    assert_eq!(rep.invoices_note, "(not tracked yet)");

    // By heat lot resolves the same grade.
    let by_lot = trace(&conn, T, TraceQueryKind::HeatLot, "HEAT-9F3A").unwrap();
    assert_eq!(by_lot.material_id.as_deref(), Some("Ti-6Al-4V"));
    assert_eq!(by_lot.work_orders.len(), 1);
}
