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

/// Path to the Python interpreter the test suite uses. Resolved in the
/// same order as the daemon's `resolve_pipeline_python`
/// (`apps/aberp/src/quote_pricing_pipeline.rs`), so a developer with a
/// normally set-up checkout gets passing CAD-smoke tests WITHOUT having
/// to export anything:
///
/// 1. `ABERP_TEST_PYTHON` if set — explicit override (CI uses this, set
///    to `sys.executable` of the venv it `pip install -e`s).
/// 2. canonical venv `<repo>/python/aberp-cad-extract/.venv/bin/python`
///    — the documented per-checkout dev venv (gitignored, so each
///    worktree/checkout has its own).
/// 3. alt project-root venv `<repo>/.venv/bin/python`.
/// 4. `python3` on PATH — last resort. If the module isn't installed
///    there the test fails downstream with a clear ImportError
///    (CLAUDE.md rule 12: fail loud, never silently skip).
///
/// We do NOT `#[ignore]` these tests — de-gating is forbidden
/// ([[all-gates-must-pass]]). Auto-discovery makes them pass when a
/// venv exists; they still fail loud when no python has the module.
pub fn test_python_bin() -> PathBuf {
    if let Ok(p) = std::env::var("ABERP_TEST_PYTHON") {
        return PathBuf::from(p);
    }
    let repo_root = repo_root();
    let canonical = repo_root
        .join("python")
        .join("aberp-cad-extract")
        .join(".venv")
        .join("bin")
        .join("python");
    if canonical.is_file() {
        return canonical;
    }
    let alt = repo_root.join(".venv").join("bin").join("python");
    if alt.is_file() {
        return alt;
    }
    PathBuf::from("python3")
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
