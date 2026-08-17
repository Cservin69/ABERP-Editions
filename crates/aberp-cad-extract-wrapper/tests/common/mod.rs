//! Shared test helpers.
//!
//! Each `tests/*.rs` is its own compilation unit, so this module
//! gets `#[allow(dead_code)]` — helpers used only by one of the
//! files would otherwise warn from the unused-import lint in the
//! others.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Path to the Python interpreter the test suite uses. Resolved in the
/// same order as the daemon's `resolve_pipeline_python`
/// (`apps/aberp/src/quote_pricing_pipeline.rs`), so a developer with a
/// normally set-up checkout gets passing CAD-smoke tests WITHOUT having
/// to export anything:
///
/// 1. `ABERP_TEST_PYTHON` if set — explicit override (CI uses this, set
///    to `sys.executable` of the venv it `pip install -e`s). Trusted but
///    unverified, exactly like the daemon's `ABERP_QUOTE_PIPELINE_PYTHON`
///    env arm — the operator who sets it owns its correctness.
/// 2. canonical venv `<repo>/python/aberp-cad-extract/.venv/bin/python`
///    — the documented per-checkout dev venv (gitignored, so each
///    worktree/checkout has its own). Selected only if it EXISTS **and**
///    can `import aberp_cad_extract` — see [`module_importable`].
/// 3. alt project-root venv `<repo>/.venv/bin/python` — same exists +
///    importable gate.
/// 4. `python3` on PATH — last resort. If the module isn't installed
///    there the test fails downstream with a clear ImportError
///    (CLAUDE.md rule 12: fail loud, never silently skip).
///
/// The exists **AND** importable gate at steps 2/3 is the parity fix
/// (S421, from the S420 review): a canonical venv that exists but lacks
/// the module — a partial/stale `pip install`, or a symlink to a broken
/// venv — must NOT win over a working alt/system python the way a
/// file-exists-only check let it. The daemon gates each candidate on
/// `is_file() && check_module_importable(..)`; the harness now does too,
/// so a broken-but-present canonical falls through here exactly as it
/// does in prod, instead of producing a false test failure prod would
/// never hit.
///
/// We do NOT `#[ignore]` these tests — de-gating is forbidden
/// ([[all-gates-must-pass]]). Auto-discovery makes them pass when a
/// venv exists; they still fail loud when no python has the module.
pub fn test_python_bin() -> PathBuf {
    if let Ok(p) = std::env::var("ABERP_TEST_PYTHON") {
        return PathBuf::from(p);
    }
    resolve_test_python(&repo_root())
}

/// Steps 2–4 of [`test_python_bin`], factored out with `repo_root` as a
/// parameter so the broken-canonical fallthrough is unit-testable
/// without touching the real checkout (mirrors the daemon's
/// `resolve_pipeline_python(aberp_root: &Path)` shape). The `python3`
/// last resort is returned unconditionally — if it lacks the module the
/// caller fails loud with an ImportError (rule 12), which is the
/// intended "no venv anywhere" signal.
pub fn resolve_test_python(repo_root: &Path) -> PathBuf {
    let canonical = repo_root
        .join("python")
        .join("aberp-cad-extract")
        .join(".venv")
        .join("bin")
        .join("python");
    if canonical.is_file() && module_importable(&canonical) {
        return canonical;
    }
    let alt = repo_root.join(".venv").join("bin").join("python");
    if alt.is_file() && module_importable(&alt) {
        return alt;
    }
    PathBuf::from("python3")
}

/// Mirror of the daemon's `check_module_importable`
/// (`quote_pricing_pipeline.rs`): spawn `python -c "import
/// aberp_cad_extract"` and treat a zero exit as importable. A candidate
/// that cannot launch (not executable / not a real python) or whose
/// import fails yields `false`, so the resolver falls through to the
/// next candidate rather than selecting a dead interpreter.
fn module_importable(python: &Path) -> bool {
    Command::new(python)
        .args(["-c", "import aberp_cad_extract"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Repo root = two levels above this crate's manifest dir
/// (`<repo>/crates/aberp-cad-extract-wrapper`). Used only to locate the
/// dev venv; falls back to `.` if the layout is ever unexpected.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Absolute path to a committed Python-side STEP fixture.
///
/// `CARGO_MANIFEST_DIR` points at `crates/aberp-cad-extract-wrapper`;
/// [`repo_root`] walks up two levels, then into the Python package's
/// `tests/fixtures/`. Same resolution `step_extract_smoke.rs` uses.
pub fn step_fixture_source(name: &str) -> PathBuf {
    repo_root()
        .join("python/aberp-cad-extract/aberp_cad_extract/tests/fixtures")
        .join(name)
}

/// Copy the 20 mm-cube STEP fixture to `dest`.
///
/// ADR-0112 replacement for `write_cube_stl`. The tests using this need
/// a **carrier file** — something that exists on disk so the wrapper's
/// pre-flight `input_path.exists()` check passes and the stub Python
/// module (which never opens it) gets to run. A format synthesiser is
/// overkill for that; copying the committed fixture is simpler and
/// honest about which formats are live.
pub fn copy_step_fixture(dest: &Path) -> std::io::Result<()> {
    let src = step_fixture_source("unit_cube.step");
    std::fs::copy(&src, dest).map(|_| ())
}
