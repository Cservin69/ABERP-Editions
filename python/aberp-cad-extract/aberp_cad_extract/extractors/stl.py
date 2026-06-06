"""STL extractor — numpy-stl-backed FeatureGraph populator.

STL is a triangle-soup format with no semantic feature data, so the
v1 extractor ships the geometric quantities STL CAN provide (bounding
box, signed-tetrahedra volume) plus the addendum-1 booleans from the
heuristics module. The ``features`` list is empty in v1: feature
extraction needs B-rep topology that arrives only with the OCCT
upgrade (S270+).

The empty ``features`` list is honest, not a bug — the engine will
still produce a material-cost-driven baseline quote, and the
operator can override the breakdown in the SPA. The reasoning_log
will show "0 features matched" so the operator sees the limitation.
"""

from __future__ import annotations

from pathlib import Path
from typing import List

import numpy as np
from stl import mesh as stl_mesh

from aberp_cad_extract.feature_graph import Feature, FeatureGraph, SCHEMA_VERSION
from aberp_cad_extract.heuristics import (
    MeshSummary,
    infer_requires_5_axis,
    infer_thin_wall_present,
)


def _bounding_box_mm(triangles: np.ndarray) -> tuple[float, float, float]:
    """X/Y/Z extent of the mesh's bounding box, in millimetres.

    numpy-stl stores triangles as a (N, 3, 3) array; we flatten and
    take min/max per axis.
    """
    pts = triangles.reshape(-1, 3)
    mins = pts.min(axis=0)
    maxs = pts.max(axis=0)
    extent = maxs - mins
    return float(extent[0]), float(extent[1]), float(extent[2])


def _signed_tetrahedra_volume_mm3(triangles: np.ndarray) -> float:
    """Closed-mesh volume via signed-tetrahedra summation.

    Each triangle and the origin form a tetrahedron with signed
    volume ``dot(v0, cross(v1, v2)) / 6``. Summed over a closed mesh
    these signed volumes give the enclosed volume. Open or non-
    manifold meshes return garbage — STL is supposed to be closed.

    Returns absolute value so a mesh with reversed normals does not
    surface a negative quote.
    """
    v0 = triangles[:, 0, :]
    v1 = triangles[:, 1, :]
    v2 = triangles[:, 2, :]
    cross = np.cross(v1, v2)
    dots = np.einsum("ij,ij->i", v0, cross)
    return float(abs(dots.sum()) / 6.0)


def extract_stl(
    path: Path | str,
    material_grade: str,
    *,
    features: List[Feature] | None = None,
) -> FeatureGraph:
    """Parse an STL file into a FeatureGraph.

    ``material_grade`` is operator-supplied at quote time — not
    extracted from the CAD (STL has no material metadata; even STEP
    rarely does in customer-uploaded files). The engine validates it
    against ``quoting_materials.grade`` (S266).

    ``features`` is exposed for tests that want to inject synthetic
    feature lists. Production callers leave it ``None``; the v1 STL
    path returns an empty feature list (see module docstring).

    Both addendum-1 booleans are populated unconditionally via the
    heuristics module — they are never missing or ``None``.
    """
    stl = stl_mesh.Mesh.from_file(str(path))
    triangles = stl.vectors

    bbox = _bounding_box_mm(triangles)
    volume = _signed_tetrahedra_volume_mm3(triangles)
    summary = MeshSummary(bounding_box_mm=bbox, volume_mm3=volume)

    return FeatureGraph(
        schema_version=SCHEMA_VERSION,
        bounding_box_mm=list(bbox),
        volume_mm3=volume,
        material_grade=material_grade,
        features=features or [],
        requires_5_axis=infer_requires_5_axis(summary),
        thin_wall_present=infer_thin_wall_present(summary),
    )
