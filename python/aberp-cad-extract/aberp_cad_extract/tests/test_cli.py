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


def _run(args: list[str]) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, "-m", "aberp_cad_extract", *args],
        capture_output=True,
        text=True,
        check=False,
    )


def test_cli_emits_valid_feature_graph_json(cube_stl_path: Path):
    result = _run([str(cube_stl_path), "--material-grade", "6061-T6"])
    assert result.returncode == 0, result.stderr
    payload = json.loads(result.stdout)
    # Addendum 1: both booleans present in the wire output, typed bool.
    assert payload["_schema_version"] == 1
    assert "requires_5_axis" in payload
    assert "thin_wall_present" in payload
    assert isinstance(payload["requires_5_axis"], bool)
    assert isinstance(payload["thin_wall_present"], bool)
    assert payload["material_grade"] == "6061-T6"
    assert payload["bounding_box_mm"] == [20.0, 20.0, 20.0]


def test_cli_missing_file_returns_2(tmp_path: Path):
    missing = tmp_path / "ghost.stl"
    result = _run([str(missing), "--material-grade", "6061-T6"])
    assert result.returncode == 2
    err = json.loads(result.stderr)
    assert err["error"]["stage"] == "input"
    assert "not found" in err["error"]["message"]


def test_cli_step_extension_routes_to_step_extractor():
    """A real STEP fixture either succeeds (OCP installed → JSON on stdout)
    OR surfaces the "not yet implemented in this build" stub message
    on stderr (OCP missing — CI/dev hasn't run `pip install -e '.[step]'`).
    Both paths are valid v0.1 contracts; the test pins the dispatch.
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
        assert payload["_schema_version"] == 1
        assert payload["bounding_box_mm"] == [20.0, 20.0, 20.0]
        assert payload["volume_mm3"] == pytest.approx(8000.0, abs=1e-3)
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


def test_cli_requires_material_grade(cube_stl_path: Path):
    result = _run([str(cube_stl_path)])
    # argparse exit code 2 for missing required argument
    assert result.returncode == 2
