//! End-to-end pin for the Defense demo seed.
//!
//! The seed's value is that a demo can walk ONE job from screen to screen, so
//! the thing worth testing is not "did rows appear" but "do the rows still
//! join". These tests therefore assert the *narrative*: the heat lot on the
//! stock row resolves to the grade that names the quote that spawned the work
//! order whose units carry that heat lot in their UIDs. A seed that wrote all
//! the right row COUNTS with the ids drifted apart would pass a count check
//! and fail the demo.
//!
//! Everything runs against a scratch DB under `TempDir`; nothing touches
//! `$HOME`, the tenant registry, or any edition data root.

use aberp::{demo_seed, material_traceability, part_marking, purchasing, quality, serve};
use aberp_audit_ledger::TenantId;

const TENANT: &str = "demo";

struct Fixture {
    db_path: std::path::PathBuf,
    db: aberp_db::HandleArc,
    tenant: TenantId,
}

fn seed_fixture() -> (Fixture, demo_seed::DemoSeedSummary) {
    // Same scratch-dir idiom as the other file-backed integration tests
    // (`purchase_order_e2e`): a run-unique dir under `$TMPDIR`, no new dev-dep.
    let dir = std::env::temp_dir()
        .join("aberp-defense-demo-seed")
        .join(ulid::Ulid::new().to_string());
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let db_path = dir.join("aberp.duckdb");
    let tenant = TenantId::new(TENANT.to_string()).expect("tenant id");
    let db = serve::open_tenant_handle(&db_path, tenant.clone()).expect("open tenant handle");
    let binary_hash = aberp_audit_ledger::BinaryHash::from_bytes([7u8; 32]);
    let summary = demo_seed::seed(&db_path, &db, &tenant, binary_hash).expect("seed");
    (
        Fixture {
            db_path,
            db,
            tenant,
        },
        summary,
    )
}

#[test]
fn seed_populates_every_defense_surface() {
    let (fx, s) = seed_fixture();
    assert!(!s.already_seeded);

    // Master data + the three AVL statuses the screen has to be able to show.
    assert_eq!(s.partners, 4, "one customer + three suppliers");
    assert_eq!(s.products, 4, "two finished goods + two raw stock items");
    assert_eq!(s.avl_vendors, 3);
    assert_eq!(s.purchase_orders, 2);
    assert_eq!(s.material_balances, 3);
    assert_eq!(s.pricing_jobs, 3, "two priced + one failed");
    assert_eq!(s.intake_rows, 2);
    assert_eq!(s.inspection_plans, 6, "four bracket + two manifold");
    assert_eq!(s.work_orders, 3);
    assert_eq!(s.part_marks, 18, "12 + 6 units, one UID each");
    assert_eq!(s.dispatches, 2);
    assert_eq!(s.invoice_drafts, 2);
    assert_eq!(
        s.ncrs, 2,
        "one auto-raised by the failed incoming inspection, one by the failed CMM measurement"
    );

    let conn = fx.db.read().expect("read");

    // The wall-TV tiles read counts off exactly these three queries.
    let wo_counts =
        aberp_work_orders::count_work_orders_by_state(&conn, TENANT).expect("WO state counts");
    assert_eq!(wo_counts.completed, 2, "both bracket batches finished");
    assert_eq!(
        wo_counts.in_progress, 1,
        "the manifold batch is on the floor"
    );

    let qa = aberp_qa::count_qa_inspections_by_state(&conn, TENANT).expect("QA counts");
    assert!(
        qa.pending >= 1,
        "the QA queue must have a live Pending row or the QA screen is empty"
    );
    assert!(qa.passed >= 8, "both completed WOs passed four ops each");

    let dispatch =
        aberp_dispatch::count_dispatches_by_state(&conn, TENANT).expect("dispatch counts");
    assert_eq!(
        dispatch.drafted, 2,
        "both dispatches wait for the ship click"
    );
}

#[test]
fn the_quotes_are_priced_by_the_real_engine() {
    let (fx, _) = seed_fixture();
    let conn = fx.db.read().expect("read");
    let jobs = aberp::quote_pricing_jobs::list_jobs(&conn, TENANT).expect("list pricing jobs");
    assert_eq!(jobs.len(), 3);

    let posted: Vec<_> = jobs
        .iter()
        .filter(|j| j.state == aberp::quote_pricing_jobs::JobState::Posted)
        .collect();
    assert_eq!(posted.len(), 2, "two quotes reached Posted");

    for job in &posted {
        let total = job
            .total_price_eur
            .expect("a Posted job carries a total price");
        assert!(
            total > 0.0 && total.is_finite(),
            "quote {} priced at {total}",
            job.quote_id
        );
        let detail = aberp::quote_pricing_jobs::get_job_detail(&conn, &job.quote_id, TENANT)
            .expect("job detail")
            .expect("job exists");

        // The breakdown is the engine's own output, not a literal: it has to
        // decode as a QuoteBreakdown and carry the reasoning log the PDF and
        // the operator panel both render.
        let breakdown: aberp_quote_engine::QuoteBreakdown =
            serde_json::from_str(detail.breakdown_json.as_deref().expect("breakdown present"))
                .expect("breakdown decodes as the engine's own type");
        assert!(
            !breakdown.reasoning_log.is_empty(),
            "a priced quote with no reasoning log cannot be taken apart on stage"
        );
        assert!(breakdown.machining_minutes > 0.0);
        assert!(
            (breakdown.total_price - total).abs() < 0.005,
            "the stored total must be the breakdown's own total"
        );

        // The operator inputs that move the price are all present, so the
        // demo can show WHY the number is what it is.
        assert!(detail.buyer_partner_id.is_some(), "buyer partner assigned");
        assert!(detail.tolerance_class.is_some(), "tolerance band recorded");
        assert!(detail.lead_time_days.is_some(), "lead time computed");

        // And the customer PDF actually exists on disk.
        let pdf = detail.pdf_path.as_deref().expect("pdf path");
        let bytes = std::fs::read(pdf).expect("the seeded quote PDF is on disk");
        assert!(
            bytes.starts_with(b"%PDF"),
            "the seeded quote artifact must be a real PDF"
        );
    }

    let failed: Vec<_> = jobs
        .iter()
        .filter(|j| j.state == aberp::quote_pricing_jobs::JobState::Failed)
        .collect();
    assert_eq!(failed.len(), 1, "one failed job, so the queue looks real");
    assert_eq!(
        failed[0].failure_kind,
        Some(aberp::quote_pricing_jobs::FailureKind::Permanent),
        "a permanent failure is the one an operator deletes rather than retries"
    );
}

/// The whole point of the seed: one chain, not ten unrelated tables.
#[test]
fn the_narrative_joins_end_to_end() {
    let (fx, _) = seed_fixture();
    let conn = fx.db.read().expect("read");

    // Heat lot → grade → the quotes that priced it → the WOs those quotes
    // spawned. This is the Material Traceability screen's own query.
    let report = material_traceability::trace(
        &conn,
        TENANT,
        material_traceability::TraceQueryKind::HeatLot,
        "HT-2026-TI-88431",
    )
    .expect("trace by heat lot");
    let material = report
        .material
        .expect("the heat lot resolves to a stock row");
    assert_eq!(material.material_grade, "Ti-6Al-4V");
    assert!(
        material
            .mill_test_report_url
            .as_deref()
            .is_some_and(|u| u.starts_with("file://")),
        "the traced grade must carry an MTR"
    );
    // …and that MTR must be a document, not a dead link.
    let mtr_path = material
        .mill_test_report_url
        .as_deref()
        .expect("mtr url")
        .trim_start_matches("file://")
        .to_string();
    assert!(
        std::path::Path::new(&mtr_path).is_file(),
        "the seeded MTR link must resolve to a real file: {mtr_path}"
    );
    assert!(
        !report.quotes.is_empty(),
        "the traced grade must name the quote that priced it"
    );
    assert!(
        report.work_orders.len() >= 2,
        "the traced grade must reach the work orders its quote spawned, got {}",
        report.work_orders.len()
    );

    // Every unit of a completed batch carries a UID whose data-matrix payload
    // embeds that same heat lot.
    for wo in &report.work_orders {
        let marks = part_marking::list_part_marks(&conn, TENANT, &wo.wo_id).expect("part marks");
        if marks.is_empty() {
            continue;
        }
        for m in &marks {
            assert_eq!(m.heat_lot_reference.as_deref(), Some("HT-2026-TI-88431"));
            assert!(
                m.data_matrix_payload.contains(&m.part_uid),
                "the mark's data-matrix payload must carry its own UID"
            );
        }
        // And the UID resolves back through Part UID Lookup.
        let traced = part_marking::trace_part_uid(&conn, TENANT, &marks[0].part_uid)
            .expect("trace the part UID");
        assert!(
            traced.found && !traced.parts.is_empty(),
            "a minted part UID must be resolvable"
        );
    }
}

/// The two refusal surfaces a Defense demo exists to show.
#[test]
fn the_gates_have_something_to_refuse() {
    let (fx, _) = seed_fixture();
    // Read through a FRESH tenant Handle, exactly as the running app does:
    // `aberp demo-seed` and `serve` are separate processes, so the gate reads
    // happen on serve's boot-time Handle, opened AFTER every seed row is
    // committed to the file. The seed writes several tables (purchase orders,
    // NCRs, material movements) through residual `Connection::open` openers,
    // and a DuckDB connection opened BEFORE those writes — like the one
    // `seed_fixture` held while seeding — is a separate instance that never
    // sees them (aberp_db's read() is a try_clone of that persistent instance,
    // not a re-open). Reusing the seeding-time Handle here would test an
    // instance no real caller ever reads through. A fresh Handle sees the full
    // committed state, which is what serve's ship-gate resolves against.
    let read_handle = aberp::serve::open_tenant_handle(&fx.db_path, fx.tenant.clone())
        .expect("fresh boot-time tenant handle");
    let conn = read_handle.read().expect("read");

    // 1 — a SUSPENDED vendor on the AVL, so an operator can try to raise a PO
    //     against it live and watch `create_po` refuse.
    let vendors = aberp::avl_vendors::list_vendors(&conn, TENANT).expect("list AVL vendors");
    let suspended = vendors
        .iter()
        .find(|v| v.approved_status == "suspended")
        .expect("the AVL must carry a suspended vendor");
    assert_eq!(
        aberp::avl_vendors::po_eligibility(&conn, TENANT, &suspended.partner_id)
            .expect("eligibility"),
        aberp::avl_vendors::PoEligibility::Blocked {
            vendor: Box::new(suspended.clone()),
            status: aberp_compliance::avl::ApprovedStatus::Suspended,
        },
        "the seeded suspended vendor must actually block a PO"
    );
    // …and one whose re-screening window has lapsed, for the overdue chip.
    assert!(
        vendors
            .iter()
            .any(|v| aberp::avl_vendors::vendor_is_overdue(v, time::OffsetDateTime::now_utc())),
        "the AVL must carry an overdue-re-screening vendor"
    );

    // 2 — an OPEN non-conformance against a drafted dispatch's part UIDs, so
    //     pressing Ship on that dispatch is refused.
    let ncrs = quality::list_ncrs(&conn, TENANT, &quality::NcrFilter::default()).expect("list");
    let open: Vec<_> = ncrs
        .iter()
        .filter(|n| !matches!(n.state, quality::NcrState::Closed))
        .collect();
    assert!(
        open.len() >= 2,
        "both seeded NCRs stay non-terminal so the gates bite, got {}",
        open.len()
    );
    let dispatches =
        aberp_dispatch::list_dispatches(&conn, TENANT, None, 50, 0).expect("dispatches");
    assert_eq!(dispatches.len(), 2);
    let blocked: Vec<_> = dispatches
        .iter()
        .filter(|d| {
            let uids = part_marking::list_part_marks(&conn, TENANT, &d.wo_id)
                .expect("marks")
                .into_iter()
                .map(|m| m.part_uid)
                .collect::<Vec<_>>();
            !quality::open_ncr_ids_blocking_part_uids(&ncrs, &uids).is_empty()
        })
        .collect();
    assert_eq!(
        blocked.len(),
        1,
        "exactly one drafted dispatch must be ship-blocked — the other is the clean one \
         the demo actually ships"
    );

    // 3 — the failed incoming inspection left the PO partially received, with
    //     an NCR bound to the receipt line.
    let pos = purchasing::list_pos(&conn, TENANT, &purchasing::PoFilter::default()).expect("POs");
    assert!(
        pos.iter()
            .any(|p| p.state == purchasing::PoState::PartiallyReceived),
        "the failed delivery must leave its PO partially received"
    );
    assert!(
        pos.iter().any(|p| p.state == purchasing::PoState::Received),
        "the clean delivery must leave its PO received"
    );
}

/// Re-running the seed is free. An operator who runs the launcher twice must
/// not get two of everything.
#[test]
fn seeding_twice_is_a_no_op() {
    let (fx, first) = seed_fixture();
    assert!(!first.already_seeded);

    let counts_before = row_census(&fx);
    let binary_hash = aberp_audit_ledger::BinaryHash::from_bytes([7u8; 32]);
    let second =
        demo_seed::seed(&fx.db_path, &fx.db, &fx.tenant, binary_hash).expect("second seed");
    assert!(second.already_seeded, "the second run must short-circuit");
    assert_eq!(second.partners, 0, "and write nothing");
    assert_eq!(
        counts_before,
        row_census(&fx),
        "no row was added or changed"
    );
}

fn row_census(fx: &Fixture) -> Vec<(String, i64)> {
    let conn = fx.db.read().expect("read");
    [
        "partners",
        "products",
        "avl_vendors",
        "purchase_orders",
        "inventory_balances",
        "quote_pricing_jobs",
        "quote_intake_log",
        "qc_inspection_plans",
        "qc_inspections",
        "work_orders",
        "wo_part_marks",
        "ncrs",
        "dispatches",
        "invoice_draft",
    ]
    .iter()
    .map(|table| {
        let n: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap_or_else(|e| panic!("count {table}: {e}"));
        (table.to_string(), n)
    })
    .collect()
}
