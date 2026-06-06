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


SCHEMA_VERSION: int = 1


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
    material_grade: str = Field(min_length=1)
    features: List[Feature]

    requires_5_axis: bool
    thin_wall_present: bool

    def to_canonical_dict(self) -> dict:
        """Plain-JSON dict using the wire field names (Rust-compatible).

        The Rust deserializer reads ``_schema_version``; this is the
        single point that turns the Python field name into the wire
        name before ``json.dumps`` is called by the CLI.
        """
        return self.model_dump(by_alias=True, mode="json")
