"""Modular extractor backends — one per CAD file format.

v1 ships ``stl`` (numpy-stl). ``step`` is a stub that raises with a
clear "not yet implemented; use STL" message — OCCT/CadQuery is
slated for the S270 line alongside the Rust subprocess wrapper.
"""

from aberp_cad_extract.extractors.stl import extract_stl

__all__ = ["extract_stl"]
