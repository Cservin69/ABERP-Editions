"""STEP extractor — STUB for S269.

OCCT/CadQuery STEP parsing is reserved for the S270 line alongside
the Rust subprocess wrapper. Calling ``extract_step`` raises
``NotImplementedError`` with a clear "use STL" message so the wrapper
can surface a typed error rather than a crash.

The stub is intentional: pulling OCCT into the dev-loop today
(heavy, platform-finicky, multi-hundred-MB wheels) buys nothing
until the wrapper exists. CLAUDE.md rule 13 — delete-the-part is
not deferred-the-part, but here the part exists in scope as a named
function so callers can route on file extension without an
``ImportError`` at module load.
"""

from __future__ import annotations

from pathlib import Path

from aberp_cad_extract.feature_graph import FeatureGraph


def extract_step(path: Path | str, material_grade: str) -> FeatureGraph:  # noqa: ARG001
    """Always raises ``NotImplementedError``.

    Slated for S270 (OCCT bindings + Rust subprocess wrapper). Until
    then the storefront only accepts STL — the content-sniff
    validation backlog noted in [[aberp-site-cad-validation]] will
    enforce that.
    """
    raise NotImplementedError(
        "STEP extraction not yet implemented in v1; please supply STL. "
        "Slated for S270 alongside the Rust subprocess wrapper."
    )
