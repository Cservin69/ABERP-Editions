"""Command-line entry point.

Usage:
    ``python -m aberp_cad_extract <input.stl|input.step> --material-grade <grade>``

Stdout: FeatureGraph JSON on success. Stderr: structured error JSON
on failure. Exit 0 success, 2 user-input error, 1 internal error.
The Rust subprocess wrapper parses both stdout JSON and stderr
error-JSON; matching the contract here means the wrapper can ship
without re-tuning.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Sequence

from aberp_cad_extract.extractors.step import extract_step
from aberp_cad_extract.extractors.stl import extract_stl


def _error(stage: str, message: str) -> dict:
    return {"error": {"stage": stage, "message": message}}


def _route(path: Path, material_grade: str):
    suffix = path.suffix.lower()
    if suffix == ".stl":
        return extract_stl(path, material_grade)
    if suffix in (".step", ".stp"):
        return extract_step(path, material_grade)
    raise ValueError(
        f"Unsupported file extension '{suffix}'. Supported: .stl, .step, .stp"
    )


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="aberp-cad-extract",
        description="Extract a FeatureGraph JSON from a CAD file (STL v1).",
    )
    parser.add_argument("input", type=Path, help="Path to the input CAD file.")
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
