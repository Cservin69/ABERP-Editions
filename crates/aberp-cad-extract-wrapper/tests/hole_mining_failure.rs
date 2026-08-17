//! ADR-0112 adversarial S2 — a hole-mining crash must NOT arrive
//! looking like a part with no holes.
//!
//! The Python extractor deliberately survives a hole-miner exception: it
//! still builds a complete, valid `FeatureGraph` and still exits 0, so a
//! miner bug cannot cost the pipeline the geometry it does have. The
//! defect was that it survived SILENTLY. `located_holes` is omitted when
//! empty, and empty is also the honest encoding of "this part has no
//! holes" — so a 200-hole part whose mining blew up was byte-for-byte
//! indistinguishable from a blank billet: exit 0, same schema version,
//! no key. The wrapper only ever read stderr inside its
//! `!status.success()` arm, so the warning went nowhere.
//!
//! Once ADR-0112 Part C prices drilling off `located_holes`, that
//! ambiguity stops being cosmetic: "could not measure" silently prices
//! as "nothing to charge for". Under-quotes are the invisible direction
//! — CLAUDE.md rule 12.
//!
//! These tests use stub Python modules rather than the real extractor,
//! because the point under test is the WRAPPER's stderr handling, and a
//! stub can produce the exact success-plus-sentinel combination on
//! demand. The cross-language half — that the literal the stub writes is
//! the literal the real extractor writes — is pinned separately, against
//! the real Python module, at the bottom of this file.

use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use aberp_cad_extract_wrapper::{
    CadExtractor, ExtractError, ExtractRequest, HOLE_MINING_FAILED_SENTINEL,
};

mod common;
use common::{copy_step_fixture, test_python_bin};

/// A complete, VALID v6 graph — the thing the extractor really does emit
/// alongside the warning. Using a valid one is the whole point: the
/// wrapper must reject on the stderr marker, not because the payload was
/// somehow broken.
const GOOD_GRAPH: &str = concat!(
    r#"{"_schema_version": 6, "bounding_box_mm": [100.0, 60.0, 12.0], "#,
    r#""volume_mm3": 69587.6, "surface_area_mm2": 15000.0, "#,
    r#""material_grade": "6061-T6", "features": [], "#,
    r#""requires_5_axis": false, "thin_wall_present": false}"#,
);

/// Build a stub `aberp_cad_extract`-shaped package that exits 0, writes
/// `stdout_json` to stdout, and writes `stderr_text` to stderr.
fn stub_extractor(tmp: &Path, pkg: &str, stdout_json: &str, stderr_text: &str) -> PathBuf {
    let pkg_dir = tmp.join(pkg);
    fs::create_dir(&pkg_dir).unwrap();
    File::create(pkg_dir.join("__init__.py")).unwrap();
    let mut main = File::create(pkg_dir.join("__main__.py")).unwrap();
    writeln!(main, "import sys").unwrap();
    writeln!(main, "sys.stderr.write({stderr_text:?})").unwrap();
    writeln!(main, "sys.stdout.write({stdout_json:?})").unwrap();
    drop(main);

    let shim = tmp.join(format!("python-shim-{pkg}"));
    let mut s = File::create(&shim).unwrap();
    writeln!(s, "#!/bin/sh").unwrap();
    writeln!(
        s,
        "PYTHONPATH=\"{}:$PYTHONPATH\" exec \"{}\" \"$@\"",
        tmp.display(),
        test_python_bin().display(),
    )
    .unwrap();
    s.sync_all().unwrap();
    drop(s);
    let mut perm = fs::metadata(&shim).unwrap().permissions();
    perm.set_mode(0o755);
    fs::set_permissions(&shim, perm).unwrap();
    // Linux ETXTBSY race — same pattern as `error_paths.rs`.
    std::thread::sleep(Duration::from_millis(100));
    shim
}

fn run(tmp: &Path, pkg: &str, stdout_json: &str, stderr_text: &str) -> Result<(), ExtractError> {
    let carrier = tmp.join(format!("{pkg}.step"));
    copy_step_fixture(&carrier).unwrap();
    let shim = stub_extractor(tmp, pkg, stdout_json, stderr_text);
    CadExtractor::new()
        .with_python_bin(&shim)
        .with_module(pkg)
        .with_timeout(Duration::from_secs(10))
        .extract(&ExtractRequest {
            input_path: carrier,
            material_grade: "6061-T6".into(),
        })
        .map(|_| ())
}

#[test]
fn exit_zero_plus_the_sentinel_is_a_hard_error() {
    // THE regression. Exit 0, a perfectly good graph on stdout, and the
    // marker on stderr — which is exactly what a real mining crash looks
    // like. Before this cut the wrapper returned Ok and the daemon
    // quoted the part as hole-free.
    let tmp = tempfile::tempdir().unwrap();
    let stderr = format!("{HOLE_MINING_FAILED_SENTINEL} RuntimeError: OCCT face-walk exploded\n");
    match run(tmp.path(), "stub_mining_failed", GOOD_GRAPH, &stderr) {
        Err(ExtractError::HoleMiningFailed { detail }) => {
            assert!(
                detail.contains("RuntimeError") && detail.contains("OCCT face-walk exploded"),
                "the Python diagnostic must survive into the error for the \
                 audit entry; got {detail:?}"
            );
        }
        other => panic!(
            "a hole-mining crash must be a hard error, not a hole-free \
             quote; got {other:?}"
        ),
    }
}

#[test]
fn the_same_graph_without_the_sentinel_still_succeeds() {
    // Mutation guard for the test above: prove it is the SENTINEL doing
    // the work, not the stub being malformed in some other way. Same
    // stdout, same exit code, unrelated stderr chatter — must be Ok.
    // Without this, the test above would still pass if `extract` had
    // simply started rejecting every graph.
    let tmp = tempfile::tempdir().unwrap();
    let stderr = "OCCT: reading STEP ...\nsome harmless progress noise\n";
    assert!(
        run(tmp.path(), "stub_mining_ok", GOOD_GRAPH, stderr).is_ok(),
        "ordinary stderr chatter must not fail an extraction"
    );
}

#[test]
fn the_sentinel_is_detected_even_when_buried_in_occt_noise() {
    // OCCT writes a lot to stderr. The marker is found by line scan, so
    // it must survive being surrounded — and only the marker's OWN line
    // may end up in `detail`.
    let tmp = tempfile::tempdir().unwrap();
    let stderr = format!(
        "OCCT: transferring roots\n\
         {HOLE_MINING_FAILED_SENTINEL} ValueError: bad axis\n\
         OCCT: done, 412 entities\n"
    );
    match run(tmp.path(), "stub_mining_buried", GOOD_GRAPH, &stderr) {
        Err(ExtractError::HoleMiningFailed { detail }) => {
            assert_eq!(
                detail, "ValueError: bad axis",
                "detail must be the marker's own line, not the rest of stderr"
            );
        }
        other => panic!("expected HoleMiningFailed, got {other:?}"),
    }
}

#[test]
fn the_error_message_is_what_the_daemon_classifies_permanent() {
    // The daemon's `classify_failure` matches on the lowercased Display
    // string (`apps/aberp/src/quote_pricing_pipeline.rs`), not on the
    // variant — this crate cannot see that function, so pin the token it
    // greps for. If this wording drifts, a mining crash silently falls
    // back to `FailureKind::Unknown` and gets auto-retried forever.
    let err = ExtractError::HoleMiningFailed {
        detail: "RuntimeError: boom".into(),
    };
    let rendered = err.to_string().to_ascii_lowercase();
    assert!(
        rendered.contains("hole mining failed"),
        "classify_failure greps for `hole mining failed`; got {rendered:?}"
    );
}

#[test]
fn the_sentinel_literal_matches_the_python_side() {
    // The cross-language pin. The marker is a contract between two
    // processes in two languages with no shared constant, so each side
    // hard-codes it and this test asserts they still agree — against the
    // REAL Python module, not a stub. Editing one half alone breaks here
    // rather than in production, where the symptom would be the silent
    // under-quote this whole file exists to prevent.
    let out = Command::new(test_python_bin())
        .args([
            "-c",
            "from aberp_cad_extract.extractors.step import \
             HOLE_MINING_FAILED_SENTINEL as s; print(s, end='')",
        ])
        .output()
        .expect("spawn python to read the Python-side sentinel");
    assert!(
        out.status.success(),
        "could not read the Python-side sentinel: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let python_side = String::from_utf8(out.stdout).expect("sentinel is utf-8");
    assert_eq!(
        python_side, HOLE_MINING_FAILED_SENTINEL,
        "the Rust and Python halves of the hole-mining sentinel have drifted; \
         a mining crash would go undetected"
    );
}
