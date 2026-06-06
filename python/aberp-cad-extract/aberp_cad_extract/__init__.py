"""S269 / PR-258 — Python CAD extractor for the auto-quoting strand.

Produces the FeatureGraph JSON consumed by ``crates/aberp-quote-engine``
(S268). v1 ships STL-only via ``numpy-stl``; STEP/IGES extraction is
stubbed and reserved for the OCCT/CadQuery upgrade (slated alongside
the Rust subprocess wrapper in S270).

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
__version__ = "0.0.0"
