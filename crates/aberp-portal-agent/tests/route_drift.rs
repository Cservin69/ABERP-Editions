//! ADR-0113 §7: "serve.rs route drift breaking the allowlist (the
//! allowlist is exact — drift fails *closed*, portal shows an error,
//! nothing silently widens)".
//!
//! Failing closed at runtime is correct but late. This test makes the
//! drift fail at **build** time instead: it reads
//! `apps/aberp/src/serve.rs` and asserts that each of the four
//! allowlisted routes is still registered there, verbatim.
//!
//! The check is deliberately source-level rather than a compile-time
//! dependency: the portal agent must never link `apps/aberp` (that
//! would drag DuckDB, NAV and Tauri into a daemon whose whole job is
//! to keep running when those are stopped — §2.2). A grep over a file
//! in the same repository is the cheapest honest coupling available,
//! and it is the same technique `tools/cut_gate_*.sh` already uses.

use aberp_portal_agent::allowlist::UPSTREAM_ROUTES;

fn serve_rs() -> String {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/aberp/src/serve.rs");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading {} for the route-drift check: {e}", path.display()))
}

#[test]
fn every_allowlisted_route_still_exists_in_serve_rs() {
    let src = serve_rs();
    for route in UPSTREAM_ROUTES {
        let needle = format!(".route(\"{route}\", get(");
        assert!(
            src.contains(&needle),
            "ADR-0113 §6.2 row `{route}` is no longer registered as a GET in serve.rs.\n\
             The portal's allowlist would fail closed at runtime; fix the allowlist \
             (crates/aberp-portal-agent/src/allowlist.rs) to match the new route shape \
             rather than deleting this test."
        );
    }
}

#[test]
fn the_allowlist_has_exactly_the_four_adr_rows() {
    // Widening the portal's read surface is a decision ADR-0113 §6.2
    // reserves ("each needing its own justification"), not an edit.
    assert_eq!(
        UPSTREAM_ROUTES,
        ["/health", "/invoices", "/invoices/:id", "/invoices/:id/pdf"],
        "the portal's upstream surface changed — ADR-0113 §6.2 lists exactly four rows"
    );
}

#[test]
fn the_mutating_invoice_routes_still_exist_and_are_still_not_get() {
    // The reason `allowlist::RESERVED_INVOICE_SEGMENTS` contains
    // `issue`, and the reason the read-only claim is worth testing: the
    // mutating neighbours are real and adjacent.
    let src = serve_rs();
    for (route, verb) in [
        ("/invoices/issue", "post("),
        ("/invoices/:id/submit", "post("),
    ] {
        let needle = format!(".route(\"{route}\", {verb}");
        assert!(
            src.contains(&needle),
            "expected `{route}` to still be a {verb} route in serve.rs"
        );
    }
}
