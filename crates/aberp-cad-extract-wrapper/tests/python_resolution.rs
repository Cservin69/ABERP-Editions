//! S421 — parity guard for the test-harness Python resolver.
//!
//! The S420 review of S419 found the harness mirrored the daemon's venv
//! *paths* but dropped the daemon's `&& check_module_importable` gate, so
//! a canonical venv that EXISTS but lacks the module would be selected
//! here while the daemon falls through to a working alt/system python —
//! a false test failure prod never hits. This test pins the fixed
//! behaviour: a broken-but-present canonical venv is NOT returned; the
//! resolver falls through.

mod common;

use std::fs;

use common::resolve_test_python;

/// A canonical venv that exists on disk but is not a working python (an
/// empty, non-executable file standing in for a partial/stale install or
/// a symlink to a broken venv) must be skipped — `module_importable`
/// returns false, so the resolver falls through to the next candidate.
/// With no alt venv and no working python in the fake root, it lands on
/// the `python3` last resort.
///
/// Rule-9 teeth: drop the `&& module_importable(..)` gate in
/// `resolve_test_python` and this returns the broken canonical path
/// instead of `python3`, failing the assertion.
#[test]
fn broken_canonical_venv_is_skipped_not_selected() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let root = tmp.path();

    // Materialize a present-but-broken canonical venv interpreter.
    let canonical = root
        .join("python")
        .join("aberp-cad-extract")
        .join(".venv")
        .join("bin")
        .join("python");
    fs::create_dir_all(canonical.parent().unwrap()).expect("mkdir canonical");
    fs::write(&canonical, b"not a python interpreter\n").expect("write canonical");
    assert!(
        canonical.is_file(),
        "canonical must exist for the test premise"
    );

    let resolved = resolve_test_python(root);

    // It must NOT pick the broken canonical...
    assert_ne!(
        resolved, canonical,
        "broken-but-present canonical venv was selected — the importability gate is missing"
    );
    // ...and with nothing else present it falls all the way through to
    // the `python3` last resort (fail-loud handoff).
    assert_eq!(
        resolved,
        std::path::PathBuf::from("python3"),
        "expected fallthrough to the python3 last resort"
    );
}
