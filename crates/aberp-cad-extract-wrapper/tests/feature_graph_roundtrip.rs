//! Pin the wrapper-side parse of the same canonical Python-produced
//! JSON fixture the quote-engine's compat test exercises
//! (`crates/aberp-quote-engine/tests/fixtures/feature_graph_python_v2.json`,
//! pinned by `feature_graph_compat.rs`).
//!
//! Both crates parse the SAME bytes into the SAME struct; this test
//! is the cross-crate guarantee that the wrapper's parse path agrees
//! with the engine's pin. If the engine fixture changes shape, this
//! test will surface the drift on the wrapper side as well.

use aberp_cad_extract_wrapper::{FeatureGraph, EXPECTED_SCHEMA_VERSION, MIN_SCHEMA_VERSION};
use aberp_quote_engine::FeatureType;

/// The canonical Python fixture, shared verbatim with the quote-
/// engine compat test. Inlining via `include_str!` against the
/// engine crate's `tests/fixtures/` keeps a single on-disk source
/// of truth.
const PYTHON_FIXTURE: &str =
    include_str!("../../aberp-quote-engine/tests/fixtures/feature_graph_python_v2.json");

#[test]
fn engine_fixture_deserializes_byte_identical_via_wrapper_re_export() {
    let graph: FeatureGraph = serde_json::from_str(PYTHON_FIXTURE)
        .expect("Python-produced canonical fixture must deserialize");

    // The fixture is, and stays, a **v2** graph — that is its whole point:
    // it is the oldest shape still on disk, and the wrapper must keep
    // accepting it. Asserting `== EXPECTED_SCHEMA_VERSION` (as this line
    // did pre-ADR-0112) only passed because the wrapper's constant had
    // drifted DOWN to 2 while the engine struct was already at 5; it
    // encoded the drift rather than the contract.
    assert_eq!(graph.schema_version, 2, "fixture is the canonical v2 graph");
    assert!(
        (MIN_SCHEMA_VERSION..=EXPECTED_SCHEMA_VERSION).contains(&graph.schema_version),
        "a v2 graph must stay inside the accepted range {MIN_SCHEMA_VERSION}..={EXPECTED_SCHEMA_VERSION}"
    );
    assert_eq!(graph.bounding_box_mm, [50.0, 30.0, 20.0]);
    assert_eq!(graph.volume_mm3, 25_000.0);
    assert_eq!(graph.surface_area_mm2, 6200.0);
    assert_eq!(graph.material_grade, "6061-T6");
    assert_eq!(graph.features.len(), 2);
    assert_eq!(graph.features[0].feature_type, FeatureType::Hole);
    assert_eq!(graph.features[0].count, 4);
    assert_eq!(graph.features[0].representative_size_mm, 6.0);
    assert_eq!(graph.features[1].feature_type, FeatureType::Pocket);

    // Addendum 1 booleans — same pin as the engine compat test.
    assert!(!graph.requires_5_axis);
    assert!(!graph.thin_wall_present);
}
