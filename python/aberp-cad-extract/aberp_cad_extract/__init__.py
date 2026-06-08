"""Python CAD extractor for the auto-quoting strand.

Produces the FeatureGraph JSON consumed by ``crates/aberp-quote-engine``.
v0.1 (S292 / PR-273) supports both STL (via ``numpy-stl``) and STEP
(via the optional ``cadquery-ocp`` extra — install with
``pip install -e '.[step]'``). IGES remains unsupported.

The wire schema is locked at ``FeatureGraph.SCHEMA_VERSION = 1`` and
mirrors the Rust ``aberp_quote_engine::FeatureGraph`` struct exactly —
see ``aberp_cad_extract.feature_graph`` for the contract.
"""

from aberp_cad_extract.feature_graph import (
    Feature,
    FeatureGraph,
    FeatureType,
    SCHEMA_VERSION,
)

__all__ = ["Feature", "FeatureGraph", "FeatureType", "SCHEMA_VERSION"]
__version__ = "0.1.0"
