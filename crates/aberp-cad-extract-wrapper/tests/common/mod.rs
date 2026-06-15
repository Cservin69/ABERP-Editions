//! Shared test helpers.
//!
//! Each `tests/*.rs` is its own compilation unit, so this module
//! gets `#[allow(dead_code)]` — helpers used only by one of the
//! files would otherwise warn from the unused-import lint in the
//! others.

#![allow(dead_code)]

use std::fs::File;
use std::io::Write;
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

/// Write a 20 mm cube as a binary STL to `path`. Matches the
/// fixture geometry exercised by the Python-side CLI test
/// (`test_cli_emits_valid_feature_graph_json`) — 20×20×20 axis-
/// aligned cube centered on the origin, so the wrapper's smoke
/// test asserts the same bounding box [20, 20, 20].
///
/// Binary STL layout (Wikipedia: STL format):
///   80-byte header (any content — convention is "" padded with NUL)
///   uint32 little-endian triangle count
///   per triangle:
///     3 × float32 LE  normal (x,y,z) — we leave (0,0,0); STL viewers
///                     don't require valid normals for solid models
///     9 × float32 LE  three vertices (x,y,z each)
///     uint16 LE       attribute byte count (0)
pub fn write_cube_stl(path: &Path, side_mm: f32) -> std::io::Result<()> {
    let h = side_mm / 2.0;
    // Eight cube corners.
    let v = [
        [-h, -h, -h],
        [h, -h, -h],
        [h, h, -h],
        [-h, h, -h],
        [-h, -h, h],
        [h, -h, h],
        [h, h, h],
        [-h, h, h],
    ];
    // 12 triangles (2 per face). Winding doesn't affect the
    // signed-tetrahedra volume's absolute value (the extractor
    // takes `abs`), so we don't bother enforcing outward normals.
    let tris: [[usize; 3]; 12] = [
        [0, 3, 1],
        [1, 3, 2], // bottom (-z)
        [4, 5, 7],
        [5, 6, 7], // top (+z)
        [0, 1, 5],
        [0, 5, 4], // front (-y)
        [2, 3, 7],
        [2, 7, 6], // back (+y)
        [1, 2, 6],
        [1, 6, 5], // right (+x)
        [0, 4, 7],
        [0, 7, 3], // left (-x)
    ];

    let mut f = File::create(path)?;
    f.write_all(&[0u8; 80])?;
    f.write_all(&(tris.len() as u32).to_le_bytes())?;
    for t in tris.iter() {
        // zero normal
        for _ in 0..3 {
            f.write_all(&0f32.to_le_bytes())?;
        }
        for vi in t {
            for coord in &v[*vi] {
                f.write_all(&coord.to_le_bytes())?;
            }
        }
        f.write_all(&0u16.to_le_bytes())?;
    }
    Ok(())
}
