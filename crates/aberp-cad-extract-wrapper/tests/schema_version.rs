//! Pin the schema-version gate: feed the wrapper a hand-crafted JSON
//! with `_schema_version` ≠ [`EXPECTED_SCHEMA_VERSION`] and confirm
//! [`ExtractError::SchemaVersionMismatch`].
//!
//! Uses a stub-python script that prints a fixed JSON to stdout —
//! cheap, deterministic, no Python deps required beyond `python3`.

use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use aberp_cad_extract_wrapper::{
    CadExtractor, ExtractError, ExtractRequest, EXPECTED_SCHEMA_VERSION,
};

mod common;
use common::{test_python_bin, write_cube_stl};

#[test]
fn schema_version_mismatch_returns_typed_error() {
    let tmp = tempfile::tempdir().unwrap();
    let stl = tmp.path().join("cube.stl");
    write_cube_stl(&stl, 20.0).unwrap();

    // Synthetic package emits a valid-shaped FeatureGraph with
    // _schema_version=99. All other fields are well-typed so the
    // serde parse SUCCEEDS, and the version check kicks in.
    let pkg_dir = tmp.path().join("ver99_pkg");
    fs::create_dir(&pkg_dir).unwrap();
    File::create(pkg_dir.join("__init__.py")).unwrap();
    let mut main = File::create(pkg_dir.join("__main__.py")).unwrap();
    writeln!(main, "import sys, json").unwrap();
    writeln!(main, "payload = {{").unwrap();
    writeln!(main, "    '_schema_version': 99,").unwrap();
    writeln!(main, "    'bounding_box_mm': [20.0, 20.0, 20.0],").unwrap();
    writeln!(main, "    'volume_mm3': 8000.0,").unwrap();
    writeln!(main, "    'material_grade': '6061-T6',").unwrap();
    writeln!(main, "    'features': [],").unwrap();
    writeln!(main, "    'requires_5_axis': False,").unwrap();
    writeln!(main, "    'thin_wall_present': False,").unwrap();
    writeln!(main, "}}").unwrap();
    writeln!(main, "json.dump(payload, sys.stdout)").unwrap();
    drop(main);

    let shim = python_with_pythonpath(tmp.path(), &test_python_bin());

    let extractor = CadExtractor::new()
        .with_python_bin(&shim)
        .with_module("ver99_pkg")
        .with_timeout(Duration::from_secs(5));

    let req = ExtractRequest {
        input_path: stl,
        material_grade: "6061-T6".into(),
    };

    match extractor.extract(&req).unwrap_err() {
        ExtractError::SchemaVersionMismatch { expected, got } => {
            assert_eq!(expected, EXPECTED_SCHEMA_VERSION);
            assert_eq!(got, 99);
        }
        other => panic!("expected SchemaVersionMismatch, got {other:?}"),
    }
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
    // Linux ETXTBSY race — flushing + closing the writer fd before
    // exec is necessary but not sufficient (rust-lang/rust#114554); a
    // short sleep between chmod and exec makes the test deterministic
    // on GitHub Actions. The same pattern appears in
    // `tests/error_paths.rs`; keep both copies in sync if the shim
    // shape ever changes.
    s.sync_all().unwrap();
    drop(s);
    let mut perm = fs::metadata(&shim).unwrap().permissions();
    perm.set_mode(0o755);
    fs::set_permissions(&shim, perm).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    shim
}
