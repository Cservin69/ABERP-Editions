"""Heuristic populators for the addendum-1 booleans.

Both functions are pure: ``(mesh) -> bool``. Defaults are deliberately
conservative — ``infer_requires_5_axis`` returns ``False`` unless a
strong signal is present, because forcing 5-axis routing on a part
that doesn't need it raises the quote unnecessarily.

When the OCCT-backed extractor lands (S270+), these functions are
replaced by proper geometric analysis (slope/normal sampling for
5-axis, principal-axis wall thickness for thin walls). The wire
contract — both fields are populated, always — does not change.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Tuple


DEFAULT_THIN_WALL_THRESHOLD_MM: float = 1.5
DEFAULT_UNDERCUT_THRESHOLD_DEG: float = 45.0


@dataclass(frozen=True)
class MeshSummary:
    """The minimal mesh signal the heuristics consume.

    Decoupled from numpy-stl so the same heuristics drive both the
    STL path (today) and the future OCCT path (S270+) — the OCCT
    extractor will compute proper wall-thickness rather than
    bounding-box proxy, but the consumer signature is the same.
    """

    bounding_box_mm: Tuple[float, float, float]
    volume_mm3: float


def infer_thin_wall_present(
    summary: MeshSummary,
    threshold_mm: float = DEFAULT_THIN_WALL_THRESHOLD_MM,
) -> bool:
    """Smallest principal bounding-box axis under threshold ⇒ thin-wall.

    Proxy for "any wall in the part is thinner than the threshold."
    OCCT replacement (S270+) will inspect actual face-pair distance;
    bounding-box minimum is a deliberately loose substitute that
    avoids false negatives on sheet-metal-shaped parts at the cost
    of false positives on small-but-solid widgets. Operator can
    tune the threshold via the engine's QuotingParameters singleton
    in a follow-on slice.
    """
    if threshold_mm <= 0.0:
        return False
    smallest = min(summary.bounding_box_mm)
    return smallest < threshold_mm


def infer_requires_5_axis(
    summary: MeshSummary,
    undercut_threshold_deg: float = DEFAULT_UNDERCUT_THRESHOLD_DEG,
) -> bool:
    """Conservative 5-axis-routing heuristic.

    The honest STL-only signal for "needs 5-axis" is weak — STL has
    no face/normal metadata that tells us "this hole goes in at a
    compound angle." Until OCCT is wired, we err FALSE and only
    flip TRUE on a combined strong signal:

    1. Bounding-box aspect ratio is extreme (max/min ≥ 6.0).
    2. Solid-volume / bounding-box-volume is low (< 0.15), which is
       a proxy for a part with deep pockets, undercuts, or
       conformal-surface concavity that 3-axis can't reach
       in one setup.

    The ``undercut_threshold_deg`` argument is reserved for the
    OCCT replacement; it is accepted now so callers don't have to
    refactor when geometric undercut detection lands.
    """
    del undercut_threshold_deg  # reserved for OCCT extractor (S270+)

    bx, by, bz = summary.bounding_box_mm
    if bx <= 0.0 or by <= 0.0 or bz <= 0.0:
        return False

    sizes = sorted((bx, by, bz))
    aspect_ratio = sizes[-1] / sizes[0]
    bbox_volume = bx * by * bz
    if bbox_volume <= 0.0:
        return False
    fill_ratio = summary.volume_mm3 / bbox_volume

    return aspect_ratio >= 6.0 and fill_ratio < 0.15
