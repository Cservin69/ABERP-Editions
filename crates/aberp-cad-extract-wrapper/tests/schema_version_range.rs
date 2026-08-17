//! ADR-0112 B.1 — pin the schema-version guard as a RANGE, not equality.
//!
//! This is the ADR's flagged highest-risk item. Before this cut the
//! wrapper's only guard was `graph.schema_version != EXPECTED_SCHEMA_VERSION`
//! (exact equality, against a hand-kept `2`), while both this crate's docs
//! and `feature_graph.rs` claimed four times over that it accepted any
//! `schema_version <= N`. Consequence: bumping the Python `SCHEMA_VERSION`
//! without editing the Rust constant in the SAME diff fails every extraction
//! with `SchemaVersionMismatch`, which `classify_failure` marks **Permanent**
//! — no auto-retry, every in-flight Defense quote parked until an operator
//! clicks Retry on each one, after a rebuild. Silent until deploy.
//!
//! The pin below is the thing that makes that non-recurrable: it asserts
//! **both** ends of the range and both rejection sides, driven by a stub
//! Python module that emits an arbitrary `_schema_version`. It reds under
//! the plausible mutation (restoring `!=`) because `MIN_SCHEMA_VERSION` and
//! `EXPECTED_SCHEMA_VERSION` differ, so at most one of the two accept-cases
//! can pass under equality.
//!
//! Uses a stub-python script — cheap, deterministic, no geometry deps.

use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use aberp_cad_extract_wrapper::{
    CadExtractor, ExtractError, ExtractRequest, EXPECTED_SCHEMA_VERSION, MIN_SCHEMA_VERSION,
};

mod common;
use common::{copy_step_fixture, test_python_bin};

/// Run the wrapper against a stub module that emits `version` as the
/// `_schema_version`, and return the result.
fn extract_with_version(
    version: u32,
) -> Result<aberp_cad_extract_wrapper::FeatureGraph, ExtractError> {
    let tmp = tempfile::tempdir().unwrap();
    // Carrier file only — the stub module never opens it. STEP suffix
    // because ADR-0112 Part A made `.step`/`.stp` the only accepted
    // suffixes and a test carrier should not advertise a dead format.
    let carrier = tmp.path().join("carrier.step");
    copy_step_fixture(&carrier).unwrap();

    let pkg_name = format!("ver{version}_pkg");
    let pkg_dir = tmp.path().join(&pkg_name);
    fs::create_dir(&pkg_dir).unwrap();
    File::create(pkg_dir.join("__init__.py")).unwrap();
    let mut main = File::create(pkg_dir.join("__main__.py")).unwrap();
    writeln!(main, "import sys, json").unwrap();
    writeln!(main, "payload = {{").unwrap();
    writeln!(main, "    '_schema_version': {version},").unwrap();
    writeln!(main, "    'bounding_box_mm': [20.0, 20.0, 20.0],").unwrap();
    writeln!(main, "    'volume_mm3': 8000.0,").unwrap();
    writeln!(main, "    'surface_area_mm2': 2400.0,").unwrap();
    writeln!(main, "    'material_grade': '6061-T6',").unwrap();
    writeln!(main, "    'features': [],").unwrap();
    writeln!(main, "    'requires_5_axis': False,").unwrap();
    writeln!(main, "    'thin_wall_present': False,").unwrap();
    writeln!(main, "}}").unwrap();
    writeln!(main, "json.dump(payload, sys.stdout)").unwrap();
    drop(main);

    let shim = python_with_pythonpath(tmp.path(), &test_python_bin());

    CadExtractor::new()
        .with_python_bin(&shim)
        .with_module(&pkg_name)
        .with_timeout(Duration::from_secs(5))
        .extract(&ExtractRequest {
            input_path: carrier,
            material_grade: "6061-T6".into(),
        })
}

#[test]
fn the_range_is_actually_a_range() {
    // The whole pin is worthless if the two ends coincide — an equality
    // guard would then pass every case below. Assert the premise.
    assert!(
        MIN_SCHEMA_VERSION < EXPECTED_SCHEMA_VERSION,
        "ADR-0112 B.1 pin requires a NON-DEGENERATE range; got {MIN_SCHEMA_VERSION}..={EXPECTED_SCHEMA_VERSION}"
    );
}

#[test]
fn oldest_accepted_version_passes() {
    // The bottom of the range. Under the pre-ADR-0112 `!=` guard this
    // passed only by coincidence (EXPECTED was 2); it must pass BY RULE.
    let graph = extract_with_version(MIN_SCHEMA_VERSION)
        .unwrap_or_else(|e| panic!("v{MIN_SCHEMA_VERSION} must be accepted: {e}"));
    assert_eq!(graph.schema_version, MIN_SCHEMA_VERSION);
    // …and it loads with the post-v2 fields at their inert defaults, which
    // is what makes accepting it safe rather than merely permissive.
    assert!(graph.gears.is_empty());
    assert!(graph.critical_feature_tolerances.is_empty());
}

#[test]
fn newest_accepted_version_passes() {
    // The top of the range — what the current Python extractor emits.
    let graph = extract_with_version(EXPECTED_SCHEMA_VERSION)
        .unwrap_or_else(|e| panic!("v{EXPECTED_SCHEMA_VERSION} must be accepted: {e}"));
    assert_eq!(graph.schema_version, EXPECTED_SCHEMA_VERSION);
}

#[test]
fn every_version_inside_the_range_passes() {
    // Not just the endpoints: a graph stamped at any intermediate version
    // is a graph that exists on disk today, and must keep re-pricing.
    for v in MIN_SCHEMA_VERSION..=EXPECTED_SCHEMA_VERSION {
        let graph =
            extract_with_version(v).unwrap_or_else(|e| panic!("v{v} is in range, must pass: {e}"));
        assert_eq!(graph.schema_version, v);
    }
}

#[test]
fn below_the_range_is_rejected() {
    // v1 predates surface_area_mm2 and the addendum-1 booleans — refused
    // rather than silently defaulted.
    let too_old = MIN_SCHEMA_VERSION - 1;
    match extract_with_version(too_old).unwrap_err() {
        ExtractError::SchemaVersionMismatch { expected, got } => {
            assert_eq!(expected, EXPECTED_SCHEMA_VERSION);
            assert_eq!(got, too_old);
        }
        other => panic!("expected SchemaVersionMismatch for v{too_old}, got {other:?}"),
    }
}

#[test]
fn above_the_range_is_rejected() {
    let too_new = EXPECTED_SCHEMA_VERSION + 1;
    match extract_with_version(too_new).unwrap_err() {
        ExtractError::SchemaVersionMismatch { expected, got } => {
            assert_eq!(expected, EXPECTED_SCHEMA_VERSION);
            assert_eq!(got, too_new);
        }
        other => panic!("expected SchemaVersionMismatch for v{too_new}, got {other:?}"),
    }
}

#[test]
fn wrapper_ceiling_equals_the_engine_struct_version() {
    // The drift this cut designed out: three hand-kept version numbers in
    // three files. The wrapper's ceiling is now DERIVED from the engine
    // struct, and this asserts the derivation rather than trusting it.
    assert_eq!(
        EXPECTED_SCHEMA_VERSION,
        aberp_cad_extract_wrapper::FeatureGraph::SCHEMA_VERSION,
        "wrapper ceiling must track the engine's FeatureGraph::SCHEMA_VERSION"
    );
}

#[test]
fn mismatch_message_still_classifies_permanent() {
    // `classify_failure` (apps/aberp) keys Permanent off the literal
    // substring `_schema_version mismatch`. Reworking the guard must not
    // change the Display string — the classifier lives in another crate
    // and would not fail to compile if it did.
    let err = ExtractError::SchemaVersionMismatch {
        expected: EXPECTED_SCHEMA_VERSION,
        got: 99,
    };
    let msg = err.to_string().to_ascii_lowercase();
    assert!(
        msg.contains("_schema_version mismatch"),
        "classifier substring must survive: {msg}"
    );
}

fn python_with_pythonpath(tmp: &Path, real_python: &Path) -> PathBuf {
    let shim = tmp.join("python-with-tmp-on-path");
    let mut s = File::create(&shim).unwrap();
    writeln!(s, "#!/bin/sh").unwrap();
    writeln!(
        s,
        "PYTHONPATH=\"{}:$PYTHONPATH\" exec \"{}\" \"$@\"",
        tmp.display(),
        real_python.display(),
    )
    .unwrap();
    // Linux ETXTBSY race — see the twin helper in `tests/error_paths.rs`.
    s.sync_all().unwrap();
    drop(s);
    let mut perm = fs::metadata(&shim).unwrap().permissions();
    perm.set_mode(0o755);
    fs::set_permissions(&shim, perm).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    shim
}
