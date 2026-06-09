//! Failure-mode tests for [`CadExtractor::extract`].
//!
//! Every [`ExtractError`] variant has its own test below — except
//! [`ExtractError::SchemaVersionMismatch`] (covered by
//! `schema_version.rs`) and [`ExtractError::MalformedJson`] (covered
//! by the stub-python "emit garbage" case below, which exercises both
//! the same code path).
//!
//! These tests do not require the real Python extractor; they use
//! synthetic stub-python scripts that the test writes to a tempdir
//! and points the wrapper at via `with_python_bin`. The only
//! external dep is *some* Python 3 on PATH — for the stub scripts.

use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use aberp_cad_extract_wrapper::{CadExtractor, ExtractError, ExtractRequest};

mod common;
use common::{test_python_bin, write_cube_stl};

#[test]
fn missing_input_file_returns_input_file_not_found() {
    let tmp = tempfile::tempdir().unwrap();
    let req = ExtractRequest {
        input_path: tmp.path().join("ghost.stl"),
        material_grade: "6061-T6".into(),
    };
    let extractor = CadExtractor::new().with_python_bin(test_python_bin());
    let err = extractor.extract(&req).unwrap_err();
    assert!(
        matches!(err, ExtractError::InputFileNotFound(_)),
        "expected InputFileNotFound, got {err:?}"
    );
}

#[test]
fn missing_python_binary_returns_python_not_found() {
    let tmp = tempfile::tempdir().unwrap();
    let stl = tmp.path().join("cube.stl");
    write_cube_stl(&stl, 20.0).unwrap();

    let extractor = CadExtractor::new()
        .with_python_bin("/does/not/exist/python-ghost-42")
        .with_timeout(Duration::from_secs(5));

    let req = ExtractRequest {
        input_path: stl,
        material_grade: "6061-T6".into(),
    };

    let err = extractor.extract(&req).unwrap_err();
    assert!(
        matches!(err, ExtractError::PythonNotFound),
        "expected PythonNotFound, got {err:?}"
    );
}

#[test]
fn unimportable_module_returns_module_not_found() {
    let tmp = tempfile::tempdir().unwrap();
    let stl = tmp.path().join("cube.stl");
    write_cube_stl(&stl, 20.0).unwrap();

    // Use the real Python interpreter, but ask for an obviously-
    // bogus module — Python will exit 1 with
    //   ModuleNotFoundError: No module named 'aberp_no_such_module'
    // on stderr, which the wrapper maps to ModuleNotFound (the
    // contains("No module named") branch).
    let extractor = CadExtractor::new()
        .with_python_bin(test_python_bin())
        .with_module("aberp_no_such_module_42")
        .with_timeout(Duration::from_secs(5));

    let req = ExtractRequest {
        input_path: stl,
        material_grade: "6061-T6".into(),
    };

    match extractor.extract(&req).unwrap_err() {
        ExtractError::ModuleNotFound { module, stderr } => {
            assert_eq!(module, "aberp_no_such_module_42");
            assert!(
                stderr.contains("No module named"),
                "stderr should carry Python's error verbatim: {stderr}"
            );
        }
        other => panic!("expected ModuleNotFound, got {other:?}"),
    }
}

#[test]
fn timeout_kills_child_and_returns_timeout() {
    let tmp = tempfile::tempdir().unwrap();
    let stl = tmp.path().join("cube.stl");
    write_cube_stl(&stl, 20.0).unwrap();

    // Write a synthetic Python module that sleeps longer than the
    // wrapper's timeout. Layout under the tempdir:
    //   <tmp>/slow_pkg/__init__.py     (empty — marks it as a package)
    //   <tmp>/slow_pkg/__main__.py     (the sleep)
    // We invoke with `-m slow_pkg`; Python finds it because we
    // prepend the tempdir to PYTHONPATH via a wrapper script.
    let pkg_dir = tmp.path().join("slow_pkg");
    fs::create_dir(&pkg_dir).unwrap();
    File::create(pkg_dir.join("__init__.py")).unwrap();
    let mut main = File::create(pkg_dir.join("__main__.py")).unwrap();
    writeln!(main, "import time, sys").unwrap();
    writeln!(main, "time.sleep(30)").unwrap();
    writeln!(main, "sys.exit(0)").unwrap();
    drop(main);

    // Wrapper shell script that prepends the tempdir to PYTHONPATH,
    // then execs the real Python. Cross-platform-ish: we are
    // implicitly Unix here (the test depends on PermissionsExt
    // anyway).
    let shim = tmp.path().join("python-with-tmp-on-path");
    let mut s = File::create(&shim).unwrap();
    writeln!(s, "#!/bin/sh").unwrap();
    writeln!(
        s,
        "PYTHONPATH=\"{}:$PYTHONPATH\" exec \"{}\" \"$@\"",
        tmp.path().display(),
        test_python_bin().display(),
    )
    .unwrap();
    // `sync_all` + drop before exec: Linux returns ETXTBSY (os error 26)
    // when a freshly-written file is exec'd while the writer fd is still
    // open. macOS never trips this. S303 found the race flakes on the
    // GitHub Actions ubuntu runner. fsync + close clears it deterministically.
    s.sync_all().unwrap();
    drop(s);
    let mut perm = fs::metadata(&shim).unwrap().permissions();
    perm.set_mode(0o755);
    fs::set_permissions(&shim, perm).unwrap();

    let extractor = CadExtractor::new()
        .with_python_bin(&shim)
        .with_module("slow_pkg")
        .with_timeout(Duration::from_millis(400));

    let req = ExtractRequest {
        input_path: stl,
        material_grade: "6061-T6".into(),
    };

    let start = std::time::Instant::now();
    let err = extractor.extract(&req).unwrap_err();
    let elapsed = start.elapsed();
    assert!(
        matches!(err, ExtractError::Timeout(_)),
        "expected Timeout, got {err:?}"
    );
    // We asked for 400 ms; we should be back inside 2 s even with
    // wait-loop jitter + the kill round-trip.
    assert!(
        elapsed < Duration::from_secs(2),
        "timeout should return promptly after the deadline; took {elapsed:?}"
    );
}

#[test]
fn non_zero_exit_returns_non_zero_exit_with_stderr() {
    let tmp = tempfile::tempdir().unwrap();
    let stl = tmp.path().join("cube.stl");
    write_cube_stl(&stl, 20.0).unwrap();

    let pkg_dir = tmp.path().join("fail_pkg");
    fs::create_dir(&pkg_dir).unwrap();
    File::create(pkg_dir.join("__init__.py")).unwrap();
    let mut main = File::create(pkg_dir.join("__main__.py")).unwrap();
    writeln!(main, "import sys").unwrap();
    writeln!(
        main,
        "sys.stderr.write('deliberate test failure ABC123\\n')"
    )
    .unwrap();
    writeln!(main, "sys.exit(7)").unwrap();
    drop(main);

    let shim = python_with_pythonpath(tmp.path(), &test_python_bin());

    let extractor = CadExtractor::new()
        .with_python_bin(&shim)
        .with_module("fail_pkg")
        .with_timeout(Duration::from_secs(5));

    let req = ExtractRequest {
        input_path: stl,
        material_grade: "6061-T6".into(),
    };

    match extractor.extract(&req).unwrap_err() {
        ExtractError::NonZeroExit { code, stderr } => {
            assert_eq!(code, Some(7));
            assert!(
                stderr.contains("deliberate test failure ABC123"),
                "stderr should be carried verbatim: {stderr}"
            );
        }
        other => panic!("expected NonZeroExit, got {other:?}"),
    }
}

#[test]
fn zero_exit_with_garbage_stdout_returns_malformed_json() {
    let tmp = tempfile::tempdir().unwrap();
    let stl = tmp.path().join("cube.stl");
    write_cube_stl(&stl, 20.0).unwrap();

    let pkg_dir = tmp.path().join("garbage_pkg");
    fs::create_dir(&pkg_dir).unwrap();
    File::create(pkg_dir.join("__init__.py")).unwrap();
    let mut main = File::create(pkg_dir.join("__main__.py")).unwrap();
    writeln!(main, "import sys").unwrap();
    writeln!(main, "sys.stdout.write('not-json-at-all\\n')").unwrap();
    drop(main);

    let shim = python_with_pythonpath(tmp.path(), &test_python_bin());

    let extractor = CadExtractor::new()
        .with_python_bin(&shim)
        .with_module("garbage_pkg")
        .with_timeout(Duration::from_secs(5));

    let req = ExtractRequest {
        input_path: stl,
        material_grade: "6061-T6".into(),
    };

    match extractor.extract(&req).unwrap_err() {
        ExtractError::MalformedJson { stdout, error: _ } => {
            assert!(
                stdout.contains("not-json-at-all"),
                "stdout should be captured verbatim: {stdout}"
            );
        }
        other => panic!("expected MalformedJson, got {other:?}"),
    }
}

/// Helper — build the same `python-with-tmp-on-path` shim the timeout
/// test uses, so other tests can reuse the synthetic-package pattern.
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
    // Linux ETXTBSY race — see comment above the same pattern in the
    // timeout test.
    s.sync_all().unwrap();
    drop(s);
    let mut perm = fs::metadata(&shim).unwrap().permissions();
    perm.set_mode(0o755);
    fs::set_permissions(&shim, perm).unwrap();
    shim
}
