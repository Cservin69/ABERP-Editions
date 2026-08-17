"""Modular extractor backends — one per CAD file format.

**ADR-0112 Part A — there is exactly one.** ``step`` (OCCT/OCP) is the
whole set. The ``stl`` backend (numpy-stl) was deleted, not deprecated:
STL is a triangle mesh with no topology, so it cannot carry the hole
axes / depths / diameters the located-hole schema (Part B) and any
future toolpath work require. A pipeline whose downstream stages assume
located geometry cannot have an upstream stage that structurally cannot
supply it — keeping STL would mean an "except for STL" branch in every
stage, forever.

``.stl`` is now REJECTED by ``cli._route`` with its own explanatory
message. (The stale "step is a stub" docstring this replaces had been
wrong since PR-273 landed the real OCCT extractor.)
"""

from aberp_cad_extract.extractors.step import extract_step

__all__ = ["extract_step"]
