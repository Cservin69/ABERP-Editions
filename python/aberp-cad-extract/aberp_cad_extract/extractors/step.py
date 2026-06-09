"""STEP extractor — OCCT/OCP-backed.

Loads a STEP file via cadquery-ocp (the OpenCascade Python bindings),
extracts its tight bounding box + volume via OCCT, and populates the
addendum-1 booleans via the same heuristics module the STL extractor
uses. STL semantics still drive 5-axis / thin-wall inference in v1;
proper BREP feature mining is left to a follow-on cut.

OCP is an OPTIONAL dependency (``pip install -e '.[step]'``). When it
is missing — typical for a CI image that hasn't been seeded with the
~63 MB OCCT wheel — ``extract_step`` raises ``NotImplementedError``
with wording the FailureKind classifier in
``apps/aberp/src/quote_pricing_pipeline.rs`` recognises as Permanent.
That preserves the existing operator-retry contract: a daemon running
in an environment without OCP surfaces an actionable error rather
than auto-retrying a failure that can only be cleared by a one-time
install on the operator's box.

Single-part STEP only in v1. Assemblies (multi-solid STEP files) and
shapes that transfer to zero solids are explicit ``ValueError`` paths
— both classify as Permanent via the new "step file" rule in the
classifier. The operator gets a clear SPA badge instead of the
daemon silently quoting half the assembly.
"""

from __future__ import annotations

import contextlib
import os
import sys
from pathlib import Path
from typing import List

from aberp_cad_extract.feature_graph import Feature, FeatureGraph, SCHEMA_VERSION
from aberp_cad_extract.heuristics import (
    MeshSummary,
    infer_requires_5_axis,
    infer_thin_wall_present,
)

try:
    from OCP.Bnd import Bnd_Box
    from OCP.BRepBndLib import BRepBndLib
    from OCP.BRepGProp import BRepGProp
    from OCP.GProp import GProp_GProps
    from OCP.IFSelect import IFSelect_ReturnStatus
    from OCP.Interface import Interface_Static
    from OCP.STEPControl import STEPControl_Reader
    from OCP.TopAbs import TopAbs_SOLID
    from OCP.TopExp import TopExp_Explorer

    _OCP_AVAILABLE = True
    _OCP_IMPORT_ERROR: str | None = None
except ImportError as exc:  # noqa: BLE001 — boundary, structured into the message
    _OCP_AVAILABLE = False
    _OCP_IMPORT_ERROR = f"{type(exc).__name__}: {exc}"


@contextlib.contextmanager
def _silence_stdout_fd():
    """Route OS-level fd 1 to /dev/null inside the block.

    OCCT writes ANSI-coloured progress lines to the C stdout fd
    during ``ReadFile`` / ``Write``. Those bytes would interleave
    with the FeatureGraph JSON the CLI emits on stdout, so we have
    to silence them at the OS layer — ``contextlib.redirect_stdout``
    only catches Python writes, not C++ writes.
    """
    sys.stdout.flush()
    saved = os.dup(1)
    devnull = os.open(os.devnull, os.O_WRONLY)
    os.dup2(devnull, 1)
    os.close(devnull)
    try:
        yield
    finally:
        sys.stdout.flush()
        os.dup2(saved, 1)
        os.close(saved)


def _load_step_shape(path_str: str):
    """Read a STEP file via OCCT and return the transferred OneShape.

    Returns the consolidated ``TopoDS_Shape`` (a compound when the
    file contains multiple solids). Raises ``ValueError`` with a
    classifier-recognisable message on any OCCT-side failure.

    PR-274 / S297 F2: force the read-side unit conversion target to
    millimetres BEFORE ``ReadFile`` so that any STEP file declaring a
    non-MM ``LENGTH_UNIT`` (``METRE``, ``CENTI METRE``, ``INCH``, …)
    is normalised on import. OCCT's documented default IS ``"MM"``,
    but `OCP`'s Python bindings do not guarantee that default — a
    customer file declaring ``LENGTH_UNIT('METRE')`` could otherwise
    read a 20 mm cube back as a 0.020 mm cube, silently produce a
    near-zero priced quote, and ship to the customer (CLAUDE.md rule
    12 — fail loud, the silent-wrong-value class is the most
    expensive).
    """
    with _silence_stdout_fd():
        # Defensive normalisation — set every read; OCCT keeps the
        # value as global state and a future reader call could otherwise
        # inherit a different value from elsewhere in the process.
        Interface_Static.SetCVal_s("xstep.cascade.unit", "MM")
        reader = STEPControl_Reader()
        status = reader.ReadFile(path_str)
        if status != IFSelect_ReturnStatus.IFSelect_RetDone:
            raise ValueError(
                f"STEP file could not be parsed (OCCT ReadFile status={int(status)})"
            )
        reader.TransferRoots()
        shape = reader.OneShape()
    if shape.IsNull():
        raise ValueError("STEP file contained no transferable shape")
    return shape


def _count_solids(shape) -> int:
    """Walk the shape's SOLID-tier sub-shapes and count them."""
    explorer = TopExp_Explorer(shape, TopAbs_SOLID)
    n = 0
    while explorer.More():
        n += 1
        explorer.Next()
    return n


def extract_step(
    path: Path | str,
    material_grade: str,
    *,
    features: List[Feature] | None = None,
) -> FeatureGraph:
    """Parse a STEP file into a FeatureGraph via OCCT.

    Raises ``NotImplementedError`` if OCP is missing (classifier →
    Permanent; the install is a one-time operator action). Raises
    ``ValueError`` for any STEP-shape problem the v1 extractor can't
    handle — unreadable file, no solid body, or multi-solid assembly
    — also Permanent.

    ``material_grade`` is operator-supplied at quote time; STEP files
    rarely carry usable material metadata, so we treat the file as
    geometry-only and let the engine validate the grade against
    ``quoting_materials.grade``.
    """
    if not _OCP_AVAILABLE:
        raise NotImplementedError(
            "STEP extraction not yet implemented in this build — install the "
            "OCCT backend with `pip install -e '.[step]'` in the "
            "aberp-cad-extract venv. Underlying ImportError: "
            f"{_OCP_IMPORT_ERROR or 'OCP not importable'}"
        )

    shape = _load_step_shape(str(path))

    solid_count = _count_solids(shape)
    if solid_count == 0:
        raise ValueError(
            "STEP file contains no solid body; only solid-part STEP is supported in v1"
        )
    if solid_count > 1:
        raise ValueError(
            f"STEP file contains an assembly with {solid_count} solids; "
            "only single-part STEP is supported in v1"
        )

    bbox = Bnd_Box()
    BRepBndLib.AddOptimal_s(shape, bbox)
    xmin, ymin, zmin, xmax, ymax, zmax = bbox.Get()
    extent = (
        float(xmax - xmin),
        float(ymax - ymin),
        float(zmax - zmin),
    )

    props = GProp_GProps()
    BRepGProp.VolumeProperties_s(shape, props)
    volume = float(props.Mass())
    if volume < 0.0:
        # Mixed-orientation shapes can surface a negative volume; the
        # quote engine clamps non-positive volumes anyway, but mirroring
        # the absolute value here keeps the wire honest.
        volume = -volume

    summary = MeshSummary(bounding_box_mm=extent, volume_mm3=volume)

    return FeatureGraph(
        schema_version=SCHEMA_VERSION,
        bounding_box_mm=list(extent),
        volume_mm3=volume,
        material_grade=material_grade,
        features=features or [],
        requires_5_axis=infer_requires_5_axis(summary),
        thin_wall_present=infer_thin_wall_present(summary),
    )
