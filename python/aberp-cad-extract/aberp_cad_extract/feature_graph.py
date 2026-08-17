"""FeatureGraph wire schema — mirrors ``aberp_quote_engine::FeatureGraph``.

The contract: a Python-produced FeatureGraph JSON MUST deserialize
cleanly into the Rust struct without data loss. Field names, types,
and enum string-forms are pinned by the Rust side; this module is the
Python shadow of that pin.

Per [[aberp-quoting-design-addenda]] addendum 1, both
``requires_5_axis`` and ``thin_wall_present`` are first-class
booleans, not derived counters. They are required on every output —
``Optional[bool]`` is intentionally not used. Missing or wrong-type
fails Pydantic validation and the cross-language compat test in the
Rust crate.
"""

from __future__ import annotations

from enum import Enum
from typing import List

from pydantic import BaseModel, ConfigDict, Field


#: Wire schema version. Must land inside the Rust wrapper's accepted
#: range ``MIN_SCHEMA_VERSION..=EXPECTED_SCHEMA_VERSION`` (2..=6), where
#: the ceiling is ``aberp_quote_engine::FeatureGraph::SCHEMA_VERSION``.
#:
#: **ADR-0112 B.1 note.** Before that cut the wrapper's guard was exact
#: equality against 2, so bumping this constant without editing the Rust
#: side in the same diff would have failed EVERY extraction Permanent.
#: The guard is now a range, which is what makes this 2 -> 6 bump safe:
#: it lands inside the range, and a stored graph stamped at any older
#: version keeps loading with the newer fields defaulted.
#:
#: Jumped 2 -> 6, not 2 -> 3: v3/v4/v5 fields (``stock_form``, ``gears``,
#: ``tolerance``) already exist on the Rust struct and are supplied by
#: the operator paths, not by this extractor. Catching the extractor up
#: to them is a SEPARATE workstream (ADR-0112 Part B0) because it moves
#: prices; v6 does not.
#:
#: **This bump CHANGES ``feature_graph_hash`` for every freshly extracted
#: part — including parts with no holes.** Stated plainly here because an
#: earlier revision of these docs claimed the opposite. The daemon hashes
#: ``blake3(serde_json::to_vec(&graph))``
#: (``apps/aberp/src/quote_pricing_pipeline.rs``), that encoding contains
#: ``_schema_version``, and this constant is inside it. Omitting an empty
#: ``located_holes`` keeps the REST of the encoding identical, which is
#: worth doing and is pinned — but it does not and cannot hold the
#: version field still.
#:
#: Why the jump is nevertheless the right call rather than something to
#: minimise:
#:
#: * The number has to move at all, because v6 is the version at which
#:   ``located_holes`` may be present and the wrapper's accepted range is
#:   what tells the two sides apart. Stamping 2 while emitting v6 content
#:   would be a lie in the field whose entire job is to describe the
#:   content.
#: * Stamping 3, 4 or 5 instead would be a DIFFERENT lie — those versions
#:   mean "carries ``stock_form`` / ``gears`` / ``tolerance``", which this
#:   extractor does not produce. There is no smaller honest step; the
#:   version line is a ladder, not a bitmask.
#: * The blast radius is bounded and was checked, not assumed: nothing in
#:   the tree COMPARES a ``feature_graph_hash`` against a stored one. It
#:   is written into the ``quote.pricing_extracted`` audit payload and
#:   posted to the storefront as a content tag; no cache key, no
#:   idempotency short-circuit, no golden asserts a literal value. So the
#:   effect is that parts re-extracted after this deploy get a new tag —
#:   once — and no price, no PDF and no stored breakdown moves.
#:
#: What IS byte-identical, and is pinned rather than asserted in prose:
#: a stored ≤v5 graph re-serialises to exactly its old bytes (it keeps
#: its own stamped version), and the frozen Portable edition never runs
#: this extractor at all, so its encoding cannot move. See
#: ``crates/aberp-quote-engine/tests/feature_graph_compat.rs``.
SCHEMA_VERSION: int = 6


class FeatureType(str, Enum):
    """Closed-vocab feature kinds.

    Strings match the Rust ``FeatureType`` serde rename (snake_case)
    AND the ``quoting_complexity_rules.feature_type`` DB column (S267).
    """

    POCKET = "pocket"
    HOLE = "hole"
    SLOT = "slot"
    THREAD = "thread"
    UNDERCUT_5_AXIS = "undercut_5axis"
    THIN_WALL = "thin_wall"
    SURFACE = "surface"
    ENGRAVING = "engraving"


class Feature(BaseModel):
    """One feature on the extracted part.

    Mirrors the Rust ``Feature`` struct field-for-field.
    """

    model_config = ConfigDict(extra="forbid")

    feature_type: FeatureType
    count: int = Field(ge=1)
    representative_size_mm: float = Field(ge=0.0)


class HoleEndCondition(str, Enum):
    """How a located hole terminates along its axis.

    Mirrors the Rust ``HoleEndCondition`` serde strings (snake_case).

    ``UNKNOWN`` is FIRST-CLASS, never a silent default to ``THROUGH``: a
    blind hole mis-read as through under-counts peck cycles and therefore
    under-prices. An extractor that cannot tell must say so.
    """

    THROUGH = "through"
    BLIND = "blind"
    UNKNOWN = "unknown"


class LocatedHole(BaseModel):
    """One cylindrical hole with a known axis, depth, diameter, position.

    **ADR-0112 Part B (schema v6).** Mirrors the Rust ``LocatedHole``
    struct field-for-field. Millimetres, in the part's own coordinate
    system (the STEP reader normalises any declared ``LENGTH_UNIT`` to MM
    on import). No WCS, no fixture offset.

    ``axis_unit`` is normalised HERE, by the extractor — the engine "does
    not second-guess the extractor". It points from ``entry_point_mm``
    into the material.
    """

    model_config = ConfigDict(extra="forbid")

    diameter_mm: float = Field(gt=0.0)
    depth_mm: float = Field(gt=0.0)
    axis_unit: List[float] = Field(min_length=3, max_length=3)
    entry_point_mm: List[float] = Field(min_length=3, max_length=3)
    end_condition: HoleEndCondition = HoleEndCondition.UNKNOWN
    flat_bottom: bool = False


class FeatureGraph(BaseModel):
    """Extracted-geometry side of the quote-engine input.

    Mirrors the Rust ``FeatureGraph`` struct. The Rust side renames
    the schema-version field to ``_schema_version``; Pydantic alias
    + ``populate_by_name`` lets us round-trip with both names.

    Addendum 1 booleans are required fields: omitting or nulling
    either is a validation error, not a runtime fallback.
    """

    model_config = ConfigDict(
        extra="forbid",
        populate_by_name=True,
    )

    schema_version: int = Field(
        default=SCHEMA_VERSION,
        alias="_schema_version",
    )
    bounding_box_mm: List[float] = Field(min_length=3, max_length=3)
    volume_mm3: float = Field(ge=0.0)
    # Schema v2 (S418): total surface area in mm², drives finishing-pass
    # machining time in the engine (report §5.2). STL = Σ ½‖(v1−v0)×
    # (v2−v0)‖ over triangles; STEP = ``BRepGProp.SurfaceProperties``.
    surface_area_mm2: float = Field(ge=0.0)
    material_grade: str = Field(min_length=1)
    features: List[Feature]

    requires_5_axis: bool
    thin_wall_present: bool

    #: **ADR-0112 Part B (v6).** Located holes mined from the STEP B-rep.
    #: Defaults to empty, and ``to_canonical_dict`` OMITS the key when it
    #: is empty (see there), so a hole-less part adds no bytes here.
    #:
    #: CORRECTED (ADR-0112 adversarial S1): that does NOT make a freshly
    #: extracted hole-less part's ``feature_graph_hash`` unchanged, which
    #: is what this comment used to claim. ``_schema_version`` moved 2 ->
    #: 6 in the same cut and is inside the hashed encoding, so the hash
    #: moves for every extracted part regardless of this field. See
    #: :data:`SCHEMA_VERSION` for the full accounting and for what byte
    #: identity genuinely does still hold.
    located_holes: List[LocatedHole] = Field(default_factory=list)

    def to_canonical_dict(self) -> dict:
        """Plain-JSON dict using the wire field names (Rust-compatible).

        The Rust deserializer reads ``_schema_version``; this is the
        single point that turns the Python field name into the wire
        name before ``json.dumps`` is called by the CLI.

        **ADR-0112 v6.** An EMPTY ``located_holes`` emits no key at all,
        mirroring the Rust side's ``skip_serializing_if = "Vec::is_empty"``.
        Both halves of the wire contract have to agree on this, because
        the daemon blake3-hashes this exact encoding into
        ``feature_graph_hash`` and a graph that round-trips through Rust
        must come back as the same bytes it went in as.

        Scoped honestly (ADR-0112 adversarial S1): this keeps the two
        LANGUAGES' encodings identical to each other. It does not keep a
        freshly extracted part's hash identical to its pre-v6 value —
        ``_schema_version`` moved 2 -> 6 in the same cut and is inside the
        same hashed encoding. See :data:`SCHEMA_VERSION`.
        """
        out = self.model_dump(by_alias=True, mode="json")
        if not out.get("located_holes"):
            out.pop("located_holes", None)
        return out
