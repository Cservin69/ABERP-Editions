//! Cross-language compat pin — S269/PR-258.
//!
//! The Python extractor `aberp-cad-extract` (S269) produces a
//! FeatureGraph JSON. This test loads a fixture that file generated
//! and deserializes it through the Rust struct WITHOUT data loss.
//!
//! The fixture lives at `tests/fixtures/feature_graph_python_v2.json`
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

use aberp_quote_engine::{
    Feature, FeatureGraph, FeatureType, HoleEndCondition, LocatedHole, StockForm, ToleranceSpec,
};

const PYTHON_FIXTURE: &str = include_str!("fixtures/feature_graph_python_v2.json");

#[test]
fn python_v2_fixture_deserializes_into_rust_feature_graph() {
    let parsed: FeatureGraph = serde_json::from_str(PYTHON_FIXTURE)
        .expect("Python-produced fixture must deserialize into Rust FeatureGraph");

    // The Python extractor (S269) still emits v2; its lockstep bump to v3
    // lands with S269 (ADR-0094 Q3). The v3 engine must still accept a v2
    // graph: version <= current passes the guard, and the absent
    // `stock_form` defaults to RectangularBlock (today's block math).
    assert_eq!(parsed.schema_version, 2);
    assert!(parsed.schema_version <= FeatureGraph::SCHEMA_VERSION);
    assert_eq!(
        parsed.stock_form,
        StockForm::RectangularBlock,
        "a v2 graph omits stock_form -> must default to RectangularBlock"
    );
    // ADR-0097 v5: a ≤v4 graph omits `tolerance` -> must default to
    // Unspecified (defers to the resolved target_tolerance -> today's price)
    // and the per-feature callouts default empty. Inert-by-default load.
    assert_eq!(
        parsed.tolerance,
        ToleranceSpec::Unspecified,
        "a graph omitting tolerance -> must default to Unspecified"
    );
    assert!(
        parsed.critical_feature_tolerances.is_empty(),
        "a graph omitting critical_feature_tolerances -> must default empty"
    );
    assert_eq!(parsed.bounding_box_mm, [50.0, 30.0, 20.0]);
    assert_eq!(parsed.volume_mm3, 25_000.0);
    assert_eq!(parsed.surface_area_mm2, 6200.0);
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

#[test]
fn graph_omitting_tolerance_loads_unspecified_and_is_skipped_on_serialize() {
    // A v5-shaped graph that carries no `tolerance`/`critical_feature_tolerances`
    // keys must (a) deserialize with the inert defaults, and (b) re-serialize
    // WITHOUT introducing either key — the skip_serializing_if wire contract
    // that keeps a no-tolerance blob byte-identical to the pre-ADR-0097 shape.
    let src = r#"{
        "_schema_version": 5,
        "bounding_box_mm": [10.0, 10.0, 10.0],
        "volume_mm3": 1000.0,
        "material_grade": "6061-T6",
        "features": [],
        "requires_5_axis": false,
        "thin_wall_present": false
    }"#;
    let fg: FeatureGraph = serde_json::from_str(src).expect("v5 graph without tolerance must load");
    assert_eq!(fg.tolerance, ToleranceSpec::Unspecified);
    assert!(fg.critical_feature_tolerances.is_empty());

    let out = serde_json::to_string(&fg).expect("serialize");
    assert!(
        !out.contains("tolerance"),
        "unspecified tolerance + empty callouts must add no key: {out}"
    );
}

#[test]
fn graph_with_tolerance_spec_round_trips() {
    // When a tolerance IS supplied it round-trips through the wire unchanged.
    let src = r#"{
        "_schema_version": 5,
        "bounding_box_mm": [10.0, 10.0, 10.0],
        "volume_mm3": 1000.0,
        "material_grade": "6061-T6",
        "features": [],
        "requires_5_axis": false,
        "thin_wall_present": false,
        "tolerance": {"kind": "it_grade", "grade": 7},
        "critical_feature_tolerances": [
            {"feature_index": 0, "spec": {"kind": "plus_minus", "value_mm": 0.01}}
        ]
    }"#;
    let fg: FeatureGraph = serde_json::from_str(src).expect("graph with tolerance must load");
    assert_eq!(fg.tolerance, ToleranceSpec::ItGrade { grade: 7 });
    assert_eq!(fg.critical_feature_tolerances.len(), 1);
    assert_eq!(fg.critical_feature_tolerances[0].feature_index, 0);
    assert_eq!(
        fg.critical_feature_tolerances[0].spec,
        ToleranceSpec::PlusMinus { value_mm: 0.01 }
    );
    let out = serde_json::to_string(&fg).expect("serialize");
    assert!(out.contains("it_grade"), "tolerance must serialize: {out}");
}

// ── ADR-0112 Part B (v6) — located holes ─────────────────────────────────

#[test]
fn v5_graph_without_located_holes_loads_empty() {
    // PIN (ADR-0112 B.3). A stored v5 graph — every blob written before
    // this cut — must keep deserialising, with `located_holes` defaulting
    // to EMPTY. That is what makes it keep re-pricing at its historical
    // number: empty is the value the engine has always effectively had.
    let src = r#"{
        "_schema_version": 5,
        "bounding_box_mm": [50.0, 30.0, 20.0],
        "volume_mm3": 25000.0,
        "surface_area_mm2": 6200.0,
        "material_grade": "6061-T6",
        "features": [{"feature_type": "hole", "count": 4, "representative_size_mm": 6.0}],
        "requires_5_axis": false,
        "thin_wall_present": false
    }"#;

    let fg: FeatureGraph =
        serde_json::from_str(src).expect("a stored v5 graph must still deserialize under v6");
    assert_eq!(fg.schema_version, 5);
    assert!(
        fg.located_holes.is_empty(),
        "absent located_holes must default to EMPTY, never to a guess"
    );
    assert!(fg.schema_version <= FeatureGraph::SCHEMA_VERSION);
}

#[test]
fn v2_graph_without_located_holes_loads_empty() {
    // The oldest shape on disk gets the same guarantee, not just v5.
    let parsed: FeatureGraph =
        serde_json::from_str(PYTHON_FIXTURE).expect("the canonical v2 fixture must load");
    assert_eq!(parsed.schema_version, 2);
    assert!(parsed.located_holes.is_empty());
}

#[test]
fn empty_located_holes_adds_no_json_key() {
    // PIN: the byte-identity guarantee for a STORED graph.
    // `skip_serializing_if` means an empty vector emits NO key, so
    // re-serialising a pre-v6 graph off disk is byte-for-byte what it was
    // before this cut — it keeps its own stamped `_schema_version`, and
    // this field adds nothing beside it.
    //
    // Load-bearing well past aesthetics: the daemon blake3-hashes this
    // exact encoding into `feature_graph_hash` (`quote_pricing_pipeline`).
    // An extra `"located_holes":[]` would silently change the hash of
    // every stored hole-less graph on every re-price.
    //
    // It is also what makes the frozen Portable edition provably unmoved:
    // nothing produces located holes there (the extractor is unreachable),
    // so the field is always empty, always absent, always inert.
    //
    // SCOPE — corrected in the ADR-0112 adversarial round (S1). This says
    // nothing about a part extracted FRESH. The Python extractor's stamp
    // moved 2 -> 6 in this same cut, and that field is inside the very
    // encoding hashed here, so newly extracted parts DO get a new
    // `feature_graph_hash` — holes or no holes. Bounded (nothing compares
    // the hash to a stored one) but real. Do not read this test as
    // covering it; the extractor-path accounting lives on the Python
    // `SCHEMA_VERSION` constant and is pinned by
    // `test_hole_free_extraction_differs_from_pre_v6_only_in_the_version`.
    let src = r#"{
        "_schema_version": 5,
        "bounding_box_mm": [10.0, 10.0, 10.0],
        "volume_mm3": 1000.0,
        "material_grade": "6061-T6",
        "features": [],
        "requires_5_axis": false,
        "thin_wall_present": false
    }"#;
    let fg: FeatureGraph = serde_json::from_str(src).expect("load");
    let out = serde_json::to_string(&fg).expect("serialize");
    assert!(
        !out.contains("located_holes"),
        "an empty located_holes must add NO key: {out}"
    );
}

#[test]
fn portable_arm_feature_graph_encoding_is_byte_identical_to_pre_branch() {
    // PIN: the exact bytes, captured from the tree at the branch point
    // (pre-ADR-0112, engine SCHEMA_VERSION = 5) and frozen here.
    //
    // The Portable edition never runs the extractor, so every FeatureGraph
    // it can ever hold is one with no located holes. This asserts that
    // such a graph encodes to precisely the bytes it did before the v6
    // field existed — a stronger statement than "no located_holes key",
    // because it also catches field REORDERING (serde emits declaration
    // order) and any incidental change to the other fields' encoding.
    let fg = FeatureGraph {
        schema_version: 5, // the pre-branch value, deliberately literal
        bounding_box_mm: [50.0, 30.0, 20.0],
        volume_mm3: 25_000.0,
        surface_area_mm2: 6200.0,
        material_grade: "6061-T6".to_string(),
        features: vec![Feature {
            feature_type: FeatureType::Hole,
            count: 4,
            representative_size_mm: 6.0,
        }],
        requires_5_axis: false,
        thin_wall_present: false,
        stock_form: StockForm::RectangularBlock,
        gears: Vec::new(),
        tolerance: ToleranceSpec::Unspecified,
        critical_feature_tolerances: Vec::new(),
        located_holes: Vec::new(),
    };

    const PRE_BRANCH_ENCODING: &str = concat!(
        r#"{"_schema_version":5,"bounding_box_mm":[50.0,30.0,20.0],"#,
        r#""volume_mm3":25000.0,"surface_area_mm2":6200.0,"#,
        r#""material_grade":"6061-T6","features":[{"feature_type":"hole","#,
        r#""count":4,"representative_size_mm":6.0}],"requires_5_axis":false,"#,
        r#""thin_wall_present":false,"stock_form":{"kind":"rectangular_block"},"#,
        r#""gears":[]}"#
    );

    assert_eq!(
        serde_json::to_string(&fg).expect("serialize"),
        PRE_BRANCH_ENCODING,
        "the Portable-arm FeatureGraph encoding MUST be byte-identical to pre-ADR-0112"
    );
}

#[test]
fn v6_graph_with_located_holes_round_trips() {
    let src = r#"{
        "_schema_version": 6,
        "bounding_box_mm": [100.0, 60.0, 12.0],
        "volume_mm3": 69587.6,
        "surface_area_mm2": 15000.0,
        "material_grade": "6061-T6",
        "features": [],
        "requires_5_axis": false,
        "thin_wall_present": false,
        "located_holes": [
            {
                "diameter_mm": 8.0,
                "depth_mm": 12.0,
                "axis_unit": [0.0, 0.0, 1.0],
                "entry_point_mm": [20.0, 20.0, 0.0],
                "end_condition": "through",
                "flat_bottom": false
            },
            {
                "diameter_mm": 10.0,
                "depth_mm": 18.0,
                "axis_unit": [0.0, 0.0, -1.0],
                "entry_point_mm": [20.0, 20.0, 30.0],
                "end_condition": "blind",
                "flat_bottom": true
            }
        ]
    }"#;

    let fg: FeatureGraph = serde_json::from_str(src).expect("a v6 graph must load");
    assert_eq!(fg.schema_version, 6);
    assert_eq!(fg.schema_version, FeatureGraph::SCHEMA_VERSION);
    assert_eq!(fg.located_holes.len(), 2);

    let through = fg.located_holes[0];
    assert_eq!(through.diameter_mm, 8.0);
    assert_eq!(through.depth_mm, 12.0);
    assert_eq!(through.axis_unit, [0.0, 0.0, 1.0]);
    assert_eq!(through.entry_point_mm, [20.0, 20.0, 0.0]);
    assert_eq!(through.end_condition, HoleEndCondition::Through);
    assert!(!through.flat_bottom);

    let blind = fg.located_holes[1];
    assert_eq!(blind.end_condition, HoleEndCondition::Blind);
    assert!(blind.flat_bottom);

    // Non-empty round-trips WITH the key, and without loss.
    let out = serde_json::to_string(&fg).expect("serialize");
    assert!(out.contains("located_holes"));
    let back: FeatureGraph = serde_json::from_str(&out).expect("re-load");
    assert_eq!(back, fg);
}

#[test]
fn hole_end_condition_strings_round_trip_through_serde() {
    // Lock the closed-vocab strings the Python side emits.
    for (s, expected) in [
        ("through", HoleEndCondition::Through),
        ("blind", HoleEndCondition::Blind),
        ("unknown", HoleEndCondition::Unknown),
    ] {
        let got: HoleEndCondition = serde_json::from_str(&format!("\"{s}\""))
            .unwrap_or_else(|e| panic!("Python emits '{s}'; Rust must accept it: {e}"));
        assert_eq!(got, expected);
        assert_eq!(expected.as_db_str(), s);
    }
}

#[test]
fn omitted_end_condition_defaults_to_unknown_not_through() {
    // A producer that omits the key must be read CONSERVATIVELY. Reading
    // it as Through would under-count peck cycles and under-price; the
    // conservative branch has to be the default, not the optimistic one.
    let hole = r#"{
        "diameter_mm": 6.0,
        "depth_mm": 10.0,
        "axis_unit": [0.0, 0.0, 1.0],
        "entry_point_mm": [0.0, 0.0, 0.0]
    }"#;
    let h: LocatedHole = serde_json::from_str(hole).expect("defaults must apply");
    assert_eq!(
        h.end_condition,
        HoleEndCondition::Unknown,
        "an omitted end_condition must be Unknown — never a silent Through"
    );
    assert!(!h.flat_bottom);
}
