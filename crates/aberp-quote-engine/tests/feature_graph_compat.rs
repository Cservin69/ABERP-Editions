//! Cross-language compat pin — S269/PR-258.
//!
//! The Python extractor `aberp-cad-extract` (S269) produces a
//! FeatureGraph JSON. This test loads a fixture that file generated
//! and deserializes it through the Rust struct WITHOUT data loss.
//!
//! The fixture lives at `tests/fixtures/feature_graph_python_v1.json`
//! and was emitted by the Python `FeatureGraph.to_canonical_dict()`
//! + `json.dumps`. If the Python or Rust side renames a field, this
//! test (and the Python-side schema-lock test) MUST be updated in
//! the same diff. That is the contract.
//!
//! Per [[aberp-quoting-design-addenda]] addendum 1, the test also
//! asserts both `requires_5_axis` and `thin_wall_present` are
//! present and typed bool — never absent, never null.
//!
//! `serde_json` is a dev-dependency for this test only; the engine
//! crate itself has no JSON dep (per lib.rs: "parsing is the
//! wrapper's job").

use aberp_quote_engine::{FeatureGraph, FeatureType};

const PYTHON_FIXTURE: &str = include_str!("fixtures/feature_graph_python_v1.json");

#[test]
fn python_v1_fixture_deserializes_into_rust_feature_graph() {
    let parsed: FeatureGraph = serde_json::from_str(PYTHON_FIXTURE)
        .expect("Python-produced fixture must deserialize into Rust FeatureGraph");

    assert_eq!(parsed.schema_version, FeatureGraph::SCHEMA_VERSION);
    assert_eq!(parsed.bounding_box_mm, [50.0, 30.0, 20.0]);
    assert_eq!(parsed.volume_mm3, 25_000.0);
    assert_eq!(parsed.material_grade, "6061-T6");
    assert_eq!(parsed.features.len(), 2);
    assert_eq!(parsed.features[0].feature_type, FeatureType::Hole);
    assert_eq!(parsed.features[0].count, 4);
    assert_eq!(parsed.features[0].representative_size_mm, 6.0);
    assert_eq!(parsed.features[1].feature_type, FeatureType::Pocket);

    // Addendum 1 — booleans MUST be present (deserialization would
    // have failed above on a missing field; assert the values too).
    assert!(!parsed.requires_5_axis);
    assert!(!parsed.thin_wall_present);
}

#[test]
fn python_fixture_missing_addendum_1_boolean_fails_deserialize() {
    // Hand-craft a payload that drops `requires_5_axis` and confirm
    // serde refuses it. This is the Rust mirror of the Python
    // schema-lock test `test_missing_requires_5_axis_fails_validation`.
    let bad = r#"{
        "_schema_version": 1,
        "bounding_box_mm": [10.0, 10.0, 10.0],
        "volume_mm3": 1000.0,
        "material_grade": "6061-T6",
        "features": [],
        "thin_wall_present": false
    }"#;
    let r: Result<FeatureGraph, _> = serde_json::from_str(bad);
    assert!(r.is_err(), "missing requires_5_axis must fail");
    let msg = r.unwrap_err().to_string();
    assert!(
        msg.contains("requires_5_axis"),
        "error should name the missing field: {msg}"
    );
}

#[test]
fn python_fixture_missing_thin_wall_present_fails_deserialize() {
    let bad = r#"{
        "_schema_version": 1,
        "bounding_box_mm": [10.0, 10.0, 10.0],
        "volume_mm3": 1000.0,
        "material_grade": "6061-T6",
        "features": [],
        "requires_5_axis": false
    }"#;
    let r: Result<FeatureGraph, _> = serde_json::from_str(bad);
    assert!(r.is_err(), "missing thin_wall_present must fail");
    let msg = r.unwrap_err().to_string();
    assert!(
        msg.contains("thin_wall_present"),
        "error should name the missing field: {msg}"
    );
}

#[test]
fn feature_type_strings_round_trip_through_serde() {
    // Lock the closed-vocab strings the Python side emits.
    let cases = [
        ("pocket", FeatureType::Pocket),
        ("hole", FeatureType::Hole),
        ("slot", FeatureType::Slot),
        ("thread", FeatureType::Thread),
        ("undercut_5axis", FeatureType::Undercut5Axis),
        ("thin_wall", FeatureType::ThinWall),
        ("surface", FeatureType::Surface),
        ("engraving", FeatureType::Engraving),
    ];
    for (s, expected) in cases {
        let json = format!("\"{s}\"");
        let got: FeatureType = serde_json::from_str(&json)
            .unwrap_or_else(|e| panic!("Python emits '{s}'; Rust must accept it: {e}"));
        assert_eq!(got, expected);
    }
}
