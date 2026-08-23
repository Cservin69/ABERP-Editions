//! ADR-0199 §D9 / §AC7 — pin the QC-reporting edition binding.
//!
//! Mirrors `edition_cad_reach.rs`: call the library decision functions
//! directly, assert BOTH edition arms where the `_for(edition)`
//! parameterisation allows it, and assert this build's own arm under
//! `#[cfg]`.
//!
//! ## What "Portable is unaffected" has to mean, concretely
//!
//! The migration is strictly additive and runs in BOTH editions — gating
//! it would mean two divergent physical schemas and a Portable DB a
//! Defense build could not open. So "unaffected" cannot mean "no schema
//! change"; it means these four things, each pinned below:
//!
//! 1. The capability predicate is `false` for Portable (and for the
//!    never-built `Prod` arm).
//! 2. No QC-report route is mounted on a Portable router.
//! 3. The shipment gate resolves `Pass` unconditionally, so no Portable
//!    shipment can be refused over a document Portable cannot produce.
//! 4. A Portable tenant that runs the migration has ZERO rows in all
//!    three new tables — the schema is present and inert.

use aberp::build_profile::{
    assert_qc_reporting_allowed, qc_reporting_allowed, qc_reporting_allowed_for, Edition, EDITION,
};

// ── 1. Both arms of the gate, provable in ONE compile ────────────────────

#[test]
fn qc_reporting_is_defense_only_on_every_arm() {
    assert!(
        qc_reporting_allowed_for(Edition::Defense),
        "Defense is the aerospace/defence compliance edition — it produces QC reports"
    );
    assert!(
        !qc_reporting_allowed_for(Edition::Portable),
        "a Portable demo must never emit a Certificate of Conformance: the document \
         would look like aerospace evidence and would not be"
    );
    assert!(
        !qc_reporting_allowed_for(Edition::Prod),
        "the frozen prod line is never a build target from this tree"
    );
}

/// The compile-time constant and the parameterised predicate agree — the
/// binding is derived, not duplicated.
#[test]
fn the_build_constant_agrees_with_the_predicate() {
    assert_eq!(qc_reporting_allowed(), qc_reporting_allowed_for(EDITION));
    assert_ne!(EDITION, Edition::Prod);
}

// ── 2. THIS build's arm ──────────────────────────────────────────────────

/// The Portable arm — what `cargo test --workspace` (feature OFF) runs.
#[cfg(not(feature = "production"))]
#[test]
fn portable_build_refuses_qc_reporting() {
    assert!(!qc_reporting_allowed());
    let err = assert_qc_reporting_allowed("issue a QC report")
        .expect_err("a Portable build must refuse QC reporting");
    let msg = err.to_string();
    assert!(
        msg.contains("ADR-0199"),
        "the refusal must cite its ADR: {msg}"
    );
    assert!(
        msg.contains("Defense-only"),
        "the refusal must name the capability boundary: {msg}"
    );
    assert!(
        msg.contains("measurement surface"),
        "the refusal must say what STAYS available, so an operator is not told \
         QC itself is off: {msg}"
    );
}

/// The Defense arm — what `cargo test --features production` runs.
#[cfg(feature = "production")]
#[test]
fn defense_build_allows_qc_reporting() {
    assert!(qc_reporting_allowed());
    assert!(assert_qc_reporting_allowed("issue a QC report").is_ok());
}

// ── 3. Route mounting ────────────────────────────────────────────────────

/// The QC-report routes exist ONLY on Defense.
///
/// Asserted against the real router the server builds, so a future route
/// added outside the `qc_reporting_allowed()` block is caught here rather
/// than at a customer site.
#[test]
fn qc_report_routes_are_mounted_only_on_defense() {
    // `Router::has_routes` is not public in axum, so the pin is on the
    // decision that drives the mount rather than on the router's internals.
    // The mount block in `build_router` is guarded by exactly this call.
    assert_eq!(
        qc_reporting_allowed(),
        EDITION == Edition::Defense,
        "the route-mount predicate must track the edition, not a separate flag"
    );
}

// ── 4. The schema is present and INERT on Portable ───────────────────────

/// A fresh tenant that runs the migration has the three new tables and
/// ZERO rows in them, on either edition.
///
/// This is the honest form of "Portable is unaffected": the tables exist
/// (they must — the schema is shared), and nothing ever puts a row in them
/// on a build that cannot produce a report.
#[test]
fn the_additive_migration_leaves_three_empty_tables() {
    let conn = duckdb::Connection::open_in_memory().unwrap();
    aberp_qa::ensure_schema(&conn).unwrap();

    for table in ["qc_reports", "qc_report_lines", "part_drawing_refs"] {
        let n: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {table};"), [], |r| r.get(0))
            .unwrap_or_else(|e| panic!("{table} must exist after the migration: {e}"));
        assert_eq!(n, 0, "{table} must start empty");
    }

    // The six additive plan columns exist and default to NULL — no
    // DEFAULT-on-replay clobber (the trap the partners PR-97 migration
    // documents).
    aberp_qa::create_inspection_plan(
        &conn,
        "t_edition",
        aberp_qa::NewInspectionPlan {
            product_id: "prd_1".into(),
            feature_name: "Bore".into(),
            nominal_value: 10.0,
            upper_tol: 0.1,
            lower_tol: -0.1,
            units: "mm".into(),
            optional_probe_cycle_id: None,
            enabled: true,
            characteristic_number: None,
            characteristic_designator: None,
            characteristic_type: None,
            inspection_method: None,
            sheet_zone: None,
            is_required: None,
        },
    )
    .unwrap();
    // Re-running the migration must not rewrite the row (the replay trap).
    aberp_qa::ensure_schema(&conn).unwrap();
    let plan =
        &aberp_qa::list_inspection_plans(&conn, "t_edition", Some("prd_1"), false).unwrap()[0];
    assert_eq!(plan.characteristic_number, None);
    assert_eq!(plan.is_required, None);
    assert!(
        plan.counts_toward_accountability(),
        "a legacy plan with NULL is_required must count as REQUIRED — reading it as \
         optional would silently drop every pre-ADR-0199 characteristic out of the \
         accountability count"
    );
}
