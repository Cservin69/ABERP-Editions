"""End-to-end CLI test: subprocess → JSON on stdout → schema-valid.

The wrapper (S270) parses both stdout JSON and stderr error JSON;
the contract surface here is what the wrapper depends on.
"""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

import pytest

from aberp_cad_extract.feature_graph import SCHEMA_VERSION


def _run(args: list[str]) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, "-m", "aberp_cad_extract", *args],
        capture_output=True,
        text=True,
        check=False,
    )


def test_cli_emits_valid_feature_graph_json(step_cube_path: Path):
    pytest.importorskip("OCP", reason="requires `pip install -e '.[step]'`")
    result = _run([str(step_cube_path), "--material-grade", "6061-T6"])
    assert result.returncode == 0, result.stderr
    payload = json.loads(result.stdout)
    # Addendum 1: both booleans present in the wire output, typed bool.
    assert payload["_schema_version"] == SCHEMA_VERSION
    assert "surface_area_mm2" in payload
    assert "requires_5_axis" in payload
    assert "thin_wall_present" in payload
    assert isinstance(payload["requires_5_axis"], bool)
    assert isinstance(payload["thin_wall_present"], bool)
    assert payload["material_grade"] == "6061-T6"
    assert payload["bounding_box_mm"] == [20.0, 20.0, 20.0]


def test_cli_missing_file_returns_2(tmp_path: Path):
    missing = tmp_path / "ghost.step"
    result = _run([str(missing), "--material-grade", "6061-T6"])
    assert result.returncode == 2
    err = json.loads(result.stderr)
    assert err["error"]["stage"] == "input"
    assert "not found" in err["error"]["message"]


def test_cli_step_extension_routes_to_step_extractor():
    """A real STEP fixture either succeeds (OCP installed → JSON on stdout)
    OR surfaces the "not yet implemented in this build" message on stderr
    (OCP missing). ADR-0112 Part A note: the second branch is now a
    HARD-DOWN extractor, not a graceful degradation to another format —
    there is no other format. The test still pins the dispatch.
    """
    try:
        import OCP  # noqa: F401
        ocp_available = True
    except ImportError:
        ocp_available = False

    fixture = Path(__file__).parent / "fixtures" / "unit_cube.step"
    assert fixture.exists(), "test fixture missing; regenerate via PR-273 helper"

    result = _run([str(fixture), "--material-grade", "6061-T6"])
    if ocp_available:
        assert result.returncode == 0, result.stderr
        payload = json.loads(result.stdout)
        assert payload["_schema_version"] == SCHEMA_VERSION
        assert payload["bounding_box_mm"] == [20.0, 20.0, 20.0]
        assert payload["volume_mm3"] == pytest.approx(8000.0, abs=1e-3)
        assert payload["surface_area_mm2"] == pytest.approx(2400.0, abs=1e-2)
    else:
        assert result.returncode == 2
        err = json.loads(result.stderr)
        assert err["error"]["stage"] == "extractor"
        # Classifier matches "not yet implemented" → Permanent.
        assert "not yet implemented" in err["error"]["message"]


def test_cli_assembly_step_returns_2_with_step_file_message():
    """Multi-solid STEP must error out with a classifier-friendly message.
    Skips when OCP isn't installed (the assembly path is only reachable
    when the OCCT loader actually runs).
    """
    try:
        import OCP  # noqa: F401
    except ImportError:
        pytest.skip("requires `pip install -e '.[step]'`")

    fixture = Path(__file__).parent / "fixtures" / "assembly_two_solids.step"
    result = _run([str(fixture), "--material-grade", "6061-T6"])
    assert result.returncode == 2
    err = json.loads(result.stderr)
    assert err["error"]["stage"] == "input"
    # Rust-side classifier requires "step file" substring → Permanent.
    assert "STEP file" in err["error"]["message"]
    assert "assembly" in err["error"]["message"].lower()


def test_cli_unknown_extension_returns_2(tmp_path: Path):
    weird = tmp_path / "part.xyz"
    weird.write_bytes(b"")
    result = _run([str(weird), "--material-grade", "6061-T6"])
    assert result.returncode == 2
    err = json.loads(result.stderr)
    assert err["error"]["stage"] == "input"
    assert "Unsupported" in err["error"]["message"]


def test_cli_requires_material_grade(step_cube_path: Path):
    result = _run([str(step_cube_path)])
    # argparse exit code 2 for missing required argument
    assert result.returncode == 2


# ── ADR-0112 Part A — STL is a REJECTED input ────────────────────────────


def test_cli_stl_is_rejected_with_structured_error(tmp_path: Path):
    """PIN (ADR-0112 A.2). An STL input must be REJECTED, not parsed.

    Four properties are load-bearing and each is asserted separately:

    1. exit **2** (user-input error) — not 0, and not 1 (internal).
    2. the structured stderr envelope `{"error":{"stage","message"}}`
       with `stage == "input"`, which the Rust wrapper parses.
    3. the literal substring **"Unsupported file extension"**, which the
       daemon's `classify_failure` maps to `FailureKind::Permanent`. This
       is what buys STL rejection correct no-auto-retry classification
       with zero Rust-side change; if the wording drifts off that
       substring, STL failures silently become `Unknown` and enter the
       capped auto-retry loop.
    4. the word **STEP** and the reason — the customer has to learn what
       to do, not merely that they failed.

    The file contains REAL binary-STL bytes (an 80-byte header + a
    triangle count) so the test proves the *dispatcher* refuses on
    suffix, rather than something downstream failing to parse garbage.
    """
    stl = tmp_path / "part.stl"
    stl.write_bytes(b"\x00" * 80 + (0).to_bytes(4, "little"))

    result = _run([str(stl), "--material-grade", "6061-T6"])

    assert result.returncode == 2, f"stdout={result.stdout!r} stderr={result.stderr!r}"
    assert result.stdout.strip() == "", "a rejected input must emit NO graph on stdout"
    err = json.loads(result.stderr)
    assert err["error"]["stage"] == "input"
    message = err["error"]["message"]
    # (3) classifier substring — case-insensitive, as classify_failure is.
    assert "unsupported file extension" in message.lower()
    assert ".stl" in message
    # (4) actionable: names STEP and says why STL cannot work.
    assert "STEP" in message
    assert "topology" in message.lower()


def test_cli_stl_rejection_does_not_import_a_parser():
    """The STL parser is DELETED, not merely unrouted (ADR-0112 A.3).

    A rejection branch that still had a live parser behind it is exactly
    the half-live configuration the ADR refuses. Assert the module is
    gone from the package, so re-adding a route would fail loudly rather
    than quietly resurrect STL.
    """
    import importlib

    with pytest.raises(ModuleNotFoundError):
        importlib.import_module("aberp_cad_extract.extractors.stl")

    from aberp_cad_extract import extractors

    assert extractors.__all__ == ["extract_step"]
    assert not hasattr(extractors, "extract_stl")


def test_cli_supported_suffixes_are_exactly_step_and_stp():
    """The accepted set is closed and small (ADR-0112 A.1)."""
    from aberp_cad_extract.cli import SUPPORTED_SUFFIXES

    assert set(SUPPORTED_SUFFIXES) == {".step", ".stp"}


def test_cli_other_cad_formats_keep_the_generic_message(tmp_path: Path):
    """`.iges`/`.dxf`/… keep the GENERIC branch — STL's message is its own.

    C6: the storefront's allow-list is wider than the extractor's, and
    the generic branch is what covers the difference. It must not have
    been collapsed into the STL message.
    """
    for suffix in (".iges", ".dxf", ".sldprt"):
        weird = tmp_path / f"part{suffix}"
        weird.write_bytes(b"")
        result = _run([str(weird), "--material-grade", "6061-T6"])
        assert result.returncode == 2
        err = json.loads(result.stderr)
        message = err["error"]["message"]
        assert "unsupported file extension" in message.lower()
        assert suffix in message
        # The generic branch says nothing about triangle meshes.
        assert "topology" not in message.lower()
        assert "Supported: .step, .stp" in message
