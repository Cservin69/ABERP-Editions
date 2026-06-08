"""STL extractor — known-mesh → known-FeatureGraph."""

from __future__ import annotations

from pathlib import Path

import pytest

from aberp_cad_extract.extractors.stl import extract_stl
from aberp_cad_extract.feature_graph import FeatureGraph


def test_cube_bounding_box_and_volume(cube_stl_path: Path):
    fg = extract_stl(cube_stl_path, material_grade="6061-T6")
    assert isinstance(fg, FeatureGraph)
    bx, by, bz = fg.bounding_box_mm
    assert bx == pytest.approx(20.0, abs=1e-3)
    assert by == pytest.approx(20.0, abs=1e-3)
    assert bz == pytest.approx(20.0, abs=1e-3)
    # 20**3 = 8000 mm^3
    assert fg.volume_mm3 == pytest.approx(8_000.0, abs=1e-3)
    assert fg.material_grade == "6061-T6"
    # v1 STL path emits empty feature list — see extractor docstring.
    assert fg.features == []


def test_addendum_1_booleans_always_populated(cube_stl_path: Path):
    fg = extract_stl(cube_stl_path, material_grade="6061-T6")
    assert isinstance(fg.requires_5_axis, bool)
    assert isinstance(fg.thin_wall_present, bool)


def test_thin_plate_flags_thin_wall(thin_plate_stl_path: Path):
    fg = extract_stl(thin_plate_stl_path, material_grade="6061-T6")
    assert fg.thin_wall_present is True
    # Conservative: a plate with mid aspect ratio does NOT trip 5-axis.
    assert fg.requires_5_axis is False


def test_long_solid_bar_does_not_force_5_axis(long_thin_concave_proxy_path: Path):
    fg = extract_stl(long_thin_concave_proxy_path, material_grade="6061-T6")
    assert fg.requires_5_axis is False  # solid fill ratio ⇒ no 5-axis
    assert fg.thin_wall_present is False  # all dims >> 1.5 mm


def test_canonical_dict_uses_wire_field_names(cube_stl_path: Path):
    fg = extract_stl(cube_stl_path, material_grade="6061-T6")
    out = fg.to_canonical_dict()
    assert "_schema_version" in out
    assert out["_schema_version"] == 1
    # Addendum 1: both booleans surfaced in the JSON, never optional.
    assert "requires_5_axis" in out
    assert "thin_wall_present" in out
    assert isinstance(out["requires_5_axis"], bool)
    assert isinstance(out["thin_wall_present"], bool)


