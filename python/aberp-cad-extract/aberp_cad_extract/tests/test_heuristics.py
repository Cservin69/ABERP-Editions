"""Heuristic boolean inference — synthetic ``MeshSummary`` inputs."""

from __future__ import annotations

from aberp_cad_extract.heuristics import (
    DEFAULT_THIN_WALL_THRESHOLD_MM,
    MeshSummary,
    infer_requires_5_axis,
    infer_thin_wall_present,
)


# ---------------- thin_wall_present ----------------


def test_cube_well_above_threshold_is_not_thin_wall():
    cube = MeshSummary(bounding_box_mm=(20.0, 20.0, 20.0), volume_mm3=8_000.0)
    assert infer_thin_wall_present(cube) is False


def test_plate_below_threshold_is_thin_wall():
    plate = MeshSummary(bounding_box_mm=(100.0, 50.0, 0.8), volume_mm3=4_000.0)
    assert infer_thin_wall_present(plate) is True


def test_exact_threshold_is_not_thin_wall():
    """The threshold is strict: < threshold ⇒ thin-wall. Equality
    is NOT thin-wall — gives operator a tunable boundary that can
    be exactly the design intent."""
    edge = MeshSummary(
        bounding_box_mm=(50.0, 50.0, DEFAULT_THIN_WALL_THRESHOLD_MM),
        volume_mm3=3_750.0,
    )
    assert infer_thin_wall_present(edge) is False


def test_zero_threshold_disables_detection():
    plate = MeshSummary(bounding_box_mm=(100.0, 50.0, 0.1), volume_mm3=500.0)
    assert infer_thin_wall_present(plate, threshold_mm=0.0) is False


def test_smallest_axis_decides_not_max():
    """Two axes large, one tiny ⇒ thin-wall."""
    flake = MeshSummary(bounding_box_mm=(200.0, 200.0, 0.5), volume_mm3=20_000.0)
    assert infer_thin_wall_present(flake) is True


# ---------------- requires_5_axis ----------------


def test_compact_cube_is_not_5_axis():
    cube = MeshSummary(bounding_box_mm=(20.0, 20.0, 20.0), volume_mm3=8_000.0)
    assert infer_requires_5_axis(cube) is False


def test_long_solid_bar_is_not_5_axis():
    """Aspect ratio 10:1 but solid (fill_ratio=1.0) ⇒ NOT 5-axis.

    Demonstrates the conservative posture: a long solid bar does
    not need 5-axis; only an extreme-aspect concave/hollow part does.
    """
    bar = MeshSummary(bounding_box_mm=(1_000.0, 100.0, 100.0), volume_mm3=10_000_000.0)
    assert infer_requires_5_axis(bar) is False


def test_extreme_aspect_with_low_fill_is_5_axis():
    """Extreme aspect ratio (>6) AND low fill ratio (<0.15) ⇒ 5-axis."""
    concave = MeshSummary(
        bounding_box_mm=(600.0, 100.0, 100.0),  # aspect 6:1
        volume_mm3=600_000.0,  # fill 0.10 (deep cavities / undercuts)
    )
    assert infer_requires_5_axis(concave) is True


def test_low_fill_alone_not_enough():
    """A box with low fill but compact aspect ratio is NOT 5-axis."""
    blocky = MeshSummary(
        bounding_box_mm=(100.0, 100.0, 100.0),  # aspect 1:1
        volume_mm3=50_000.0,  # fill 0.05
    )
    assert infer_requires_5_axis(blocky) is False


def test_high_aspect_alone_not_enough():
    """Aspect 8:1 but solid ⇒ NOT 5-axis."""
    rod = MeshSummary(
        bounding_box_mm=(800.0, 100.0, 100.0),  # aspect 8:1
        volume_mm3=8_000_000.0,  # fill 1.0
    )
    assert infer_requires_5_axis(rod) is False


def test_zero_dim_does_not_crash():
    """Degenerate mesh (zero axis) returns False without div-by-zero."""
    degenerate = MeshSummary(bounding_box_mm=(0.0, 100.0, 100.0), volume_mm3=0.0)
    assert infer_requires_5_axis(degenerate) is False
    assert infer_thin_wall_present(degenerate) is True  # 0 < 1.5
