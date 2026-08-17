"""Command-line entry point.

Usage:
    ``python -m aberp_cad_extract <input.step|input.stp> --material-grade <grade>``

Stdout: FeatureGraph JSON on success. Stderr: structured error JSON
on failure. Exit 0 success, 2 user-input error, 1 internal error.
The Rust subprocess wrapper parses both stdout JSON and stderr
error-JSON; matching the contract here means the wrapper can ship
without re-tuning.

**ADR-0112 Part A — STEP-only.** ``.stl`` is a REJECTED input, not a
degraded one, and the STL parser is gone rather than deprecated in
place. A half-live STL path is exactly the configuration that lets an
STL quote slip through into stages that assume located geometry.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Sequence

from aberp_cad_extract.extractors.step import extract_step

#: The complete set of accepted input suffixes (ADR-0112 A.1).
SUPPORTED_SUFFIXES: tuple[str, ...] = (".step", ".stp")


def _error(stage: str, message: str) -> dict:
    return {"error": {"stage": stage, "message": message}}


def _route(path: Path, material_grade: str):
    """Dispatch on suffix. STEP extracts; everything else raises.

    Both rejection branches keep the literal substring ``Unsupported
    file extension``, which the daemon's ``classify_failure``
    (``apps/aberp/src/quote_pricing_pipeline.rs``) already maps to
    ``FailureKind::Permanent``. So STL rejection inherits correct
    no-auto-retry classification with ZERO Rust-side change — the
    customer gets one clear failure instead of a retry storm.
    """
    suffix = path.suffix.lower()
    if suffix in SUPPORTED_SUFFIXES:
        return extract_step(path, material_grade)
    if suffix == ".stl":
        # STL gets its OWN branch and its own message. It is the format
        # customers are most likely to have, so a generic "unsupported"
        # here is a conversion-killer — say WHY, and say what to do.
        raise ValueError(
            "Unsupported file extension '.stl'. STL is a triangle mesh with no "
            "topology: it cannot carry hole axes, depths or diameters, so it "
            "cannot be priced for drilling or programmed for toolpaths. "
            "Re-export the part as STEP (AP203/AP214/AP242). "
            "Supported: .step, .stp"
        )
    raise ValueError(
        f"Unsupported file extension '{suffix}'. Supported: .step, .stp"
    )


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="aberp-cad-extract",
        description=(
            "Extract a FeatureGraph JSON from a STEP CAD file "
            "(.step / .stp; AP203/AP214/AP242)."
        ),
    )
    parser.add_argument(
        "input",
        type=Path,
        help="Path to the input STEP file (.step or .stp).",
    )
    parser.add_argument(
        "--material-grade",
        required=True,
        help="Material grade as it appears in quoting_materials.grade (e.g. '6061-T6').",
    )
    args = parser.parse_args(argv)

    if not args.input.exists():
        json.dump(_error("input", f"file not found: {args.input}"), sys.stderr)
        sys.stderr.write("\n")
        return 2

    try:
        feature_graph = _route(args.input, args.material_grade)
    except NotImplementedError as exc:
        json.dump(_error("extractor", str(exc)), sys.stderr)
        sys.stderr.write("\n")
        return 2
    except ValueError as exc:
        json.dump(_error("input", str(exc)), sys.stderr)
        sys.stderr.write("\n")
        return 2
    except Exception as exc:  # noqa: BLE001 — boundary, structured to stderr
        json.dump(_error("internal", f"{type(exc).__name__}: {exc}"), sys.stderr)
        sys.stderr.write("\n")
        return 1

    json.dump(feature_graph.to_canonical_dict(), sys.stdout)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
