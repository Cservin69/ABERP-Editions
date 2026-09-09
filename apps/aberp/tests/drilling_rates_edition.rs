//! ADR-0112 Part C (D-19 slice C2) — pin the drilling-cost-model EDITION
//! binding, the app-layer half of "Portable never moves".
//!
//! Mirrors `edition_qc_reporting.rs`: call the library decision functions
//! directly, assert BOTH edition arms where the `_for(edition)`
//! parameterisation allows it, and assert this build's own arm under `#[cfg]`.
//!
//! ## Division of labour with the engine crate
//!
//! The ENGINE math — that an empty slice / a no-match / an inert `feed <= 0`
//! row prices `drilling_minutes = 0.0` with no `[drilling]` reasoning line, and
//! that a tuned `feed > 0` row moves the price per §C.2 — is pinned
//! exhaustively by `aberp-quote-engine/tests/drilling_cost.rs` (9 tests),
//! edition-agnostically because the engine has no notion of edition. This file
//! does NOT re-prove that. It pins the thing the ENGINE cannot see: that only a
//! Defense build ever seeds a row, that every seeded row is inert (`feed = 0`)
//! so even Defense stays byte-identical until an operator tunes a real feed,
//! and that the CRUD surface refuses on a non-Defense build.
//!
//! "Portable is unaffected" therefore means four things, each pinned below:
//!
//! 1. The capability predicate is `false` for Portable (and the never-built
//!    `Prod` arm).
//! 2. The route-mount / handler-guard predicate tracks the edition, not a
//!    separate flag.
//! 3. The boot seed is edition-gated: a Portable tenant gets ZERO drilling
//!    rows, so its snapshot slice is empty and the engine never enters the
//!    drilling path.
//! 4. Even a Defense tenant's seed is INERT (every row `feed = 0`), so the
//!    feature ships doing nothing until Ervin sets real feeds.

use aberp::build_profile::{
    assert_machining_cost_model_allowed, machining_cost_model_allowed,
    machining_cost_model_allowed_for, Edition, EDITION,
};
use aberp::quoting_drilling_rates as qdr;

const T: &str = "t_drilling_edition";

// ── 1. Both arms of the gate, provable in ONE compile ────────────────────

#[test]
fn the_drilling_cost_model_is_defense_only_on_every_arm() {
    assert!(
        machining_cost_model_allowed_for(Edition::Defense),
        "Defense is the edition that seeds and tunes the drilling cost model"
    );
    assert!(
        !machining_cost_model_allowed_for(Edition::Portable),
        "a Portable build must never own a Defense-only drilling cost model"
    );
    assert!(
        !machining_cost_model_allowed_for(Edition::Prod),
        "the frozen prod line is never a build target from this tree"
    );
}

/// The compile-time constant and the parameterised predicate agree — the
/// binding is derived, not duplicated.
#[test]
fn the_build_constant_agrees_with_the_predicate() {
    assert_eq!(
        machining_cost_model_allowed(),
        machining_cost_model_allowed_for(EDITION)
    );
    assert_ne!(EDITION, Edition::Prod);
}

// ── 2. Route-mount / handler-guard predicate ─────────────────────────────

/// The drilling-rate ROUTES are mounted in both editions (so a Portable caller
/// gets a clear 403, not a 404), but the HANDLERS refuse on a non-Defense
/// build. Both facts hinge on exactly this predicate — a future handler added
/// without the guard would drift from this pin.
#[test]
fn the_handler_guard_predicate_tracks_the_edition() {
    assert_eq!(
        machining_cost_model_allowed(),
        EDITION == Edition::Defense,
        "the drilling-rate handler guard must track the edition, not a separate flag"
    );
}

// ── 3. THIS build's arm — seed gating + CRUD refusal ─────────────────────

/// The Portable arm — what `cargo test --workspace` (feature OFF) runs.
///
/// A Portable tenant, run through the SAME boot gate the server uses
/// (`if machining_cost_model_allowed() { seed }`), ends up with ZERO drilling
/// rows — so the pricing pipeline snapshots an empty slice and the engine can
/// never enter the drilling path. And the CRUD backstop refuses loudly.
#[cfg(not(feature = "production"))]
#[test]
fn portable_seeds_nothing_and_refuses_crud() {
    assert!(!machining_cost_model_allowed());

    let conn = duckdb::Connection::open_in_memory().unwrap();
    qdr::ensure_schema(&conn).unwrap();
    // The boot gate, verbatim: a Portable build never reaches the seed call.
    if machining_cost_model_allowed() {
        qdr::seed_drilling_rates_if_absent(&conn, T).unwrap();
    }
    let rows = qdr::engine_rates(&conn, T).unwrap();
    assert!(
        rows.is_empty(),
        "a Portable tenant must have ZERO drilling rows — the empty slice is the edition gate"
    );

    // The runtime CRUD backstop refuses.
    let err = assert_machining_cost_model_allowed("edit quoting_drilling_rates")
        .expect_err("a Portable build must refuse the drilling cost model");
    let msg = err.to_string();
    assert!(
        msg.contains("ADR-0112"),
        "the refusal must cite its ADR: {msg}"
    );
    assert!(
        msg.contains("Defense-only"),
        "the refusal must name the capability boundary: {msg}"
    );
    assert!(
        msg.contains("manual quoting"),
        "the refusal must say what STAYS available: {msg}"
    );
}

/// The Defense arm — what `cargo test --features production` runs.
///
/// A Defense tenant seeds a full set of per-material rows, but every one is
/// INERT (`feed = 0`) and stamped as a seed default — so the pricing pipeline
/// still snapshots a slice the engine treats as empty (no matching `feed > 0`
/// rate), and the quote is byte-identical until an operator tunes a real feed.
#[cfg(feature = "production")]
#[test]
fn defense_seeds_inert_rows_and_allows_crud() {
    assert!(machining_cost_model_allowed());
    assert!(assert_machining_cost_model_allowed("edit quoting_drilling_rates").is_ok());

    let conn = duckdb::Connection::open_in_memory().unwrap();
    qdr::ensure_schema(&conn).unwrap();
    // The boot gate, verbatim: a Defense build reaches the seed call.
    if machining_cost_model_allowed() {
        qdr::seed_drilling_rates_if_absent(&conn, T).unwrap();
    }

    let rows = qdr::list_drilling_rates(&conn, T).unwrap();
    assert!(
        !rows.is_empty(),
        "a Defense tenant must have editable drilling rows to tune"
    );
    for r in &rows {
        assert_eq!(
            r.feed_mm_per_min_per_mm_dia, 0.0,
            "every SEEDED drilling row must be INERT (feed = 0) — the feature ships doing \
             nothing until an operator sets a real feed: {} carried feed {}",
            r.material_group, r.feed_mm_per_min_per_mm_dia
        );
        assert!(
            r.notes.as_deref().unwrap_or_default().contains("SEED"),
            "every seeded row must read unmistakably as a seed default, not a measured feed"
        );
    }

    // The engine slice the pipeline would hand over is entirely inert — no
    // row a `drilling_active` predicate (`feed > 0`) could match.
    let engine_rows = qdr::engine_rates(&conn, T).unwrap();
    assert!(
        engine_rows
            .iter()
            .all(|r| r.feed_mm_per_min_per_mm_dia == 0.0),
        "the seeded engine slice must be entirely inert on day one"
    );

    // Re-running the seed is idempotent — no duplicate rows per material group.
    qdr::seed_drilling_rates_if_absent(&conn, T).unwrap();
    let again = qdr::list_drilling_rates(&conn, T).unwrap();
    assert_eq!(
        again.len(),
        rows.len(),
        "re-seeding must not duplicate rows"
    );
}

// ── 4. The schema is present and INERT on either edition ─────────────────

/// The `quoting_drilling_rates` table is created lazily in either edition (an
/// empty table is byte-identical to a lazily-created one, and gating the schema
/// would fork the physical schema). Proven here by creating it and reading zero
/// rows — the honest form of "the schema is shared, the rows are not".
#[test]
fn the_schema_is_present_and_empty_before_any_seed() {
    let conn = duckdb::Connection::open_in_memory().unwrap();
    qdr::ensure_schema(&conn).unwrap();
    let rows = qdr::list_drilling_rates(&conn, T).unwrap();
    assert_eq!(rows.len(), 0, "a freshly-created table must start empty");
}

/// Validation is edition-independent (it guards the shape of a row, not the
/// right to have one): `feed = 0` is a VALID, meaningful value (it disables
/// drilling for the group — the seed posture), but the conservative
/// end-condition multipliers may never be a discount (< 1.0), and the material
/// group may not be blank.
#[test]
fn validation_accepts_inert_feed_but_rejects_a_discount_factor() {
    // A zero-feed row is valid — that is exactly what the seed writes.
    let inert = qdr::DrillingRateInputs {
        material_group: "6061-T6".to_string(),
        feed_mm_per_min_per_mm_dia: 0.0,
        peck_depth_dia_multiple: 3.0,
        peck_retract_sec: 0.5,
        rapid_per_hole_sec: 2.0,
        tool_change_sec: 20.0,
        flat_bottom_factor: 1.2,
        unknown_end_condition_factor: 1.5,
        notes: None,
    };
    assert!(qdr::validate_drilling_rate_inputs(&inert).is_ok());

    // A blank group is rejected.
    let blank = qdr::DrillingRateInputs {
        material_group: "   ".to_string(),
        ..inert.clone()
    };
    assert!(qdr::validate_drilling_rate_inputs(&blank).is_err());

    // A discount end-condition multiplier is rejected — Unknown must never
    // under-quote (the conservative branch).
    let discount = qdr::DrillingRateInputs {
        unknown_end_condition_factor: 0.5,
        ..inert.clone()
    };
    assert!(qdr::validate_drilling_rate_inputs(&discount).is_err());
}
