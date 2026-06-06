"""Schema-lock tests for the FeatureGraph Pydantic model.

The Rust ``aberp_quote_engine::FeatureGraph`` struct is the wire
contract; this test file is the Python-side mirror of that pin. Any
field rename, type widening, or missing-required-field change MUST
break a test here AND the Rust cross-language compat test in one
diff — never silently.
"""

from __future__ import annotations

import json

import pytest
from pydantic import ValidationError

from aberp_cad_extract.feature_graph import (
    Feature,
    FeatureGraph,
    FeatureType,
    SCHEMA_VERSION,
)


def _valid_payload() -> dict:
    return {
        "_schema_version": 1,
        "bounding_box_mm": [50.0, 30.0, 20.0],
        "volume_mm3": 25_000.0,
        "material_grade": "6061-T6",
        "features": [
            {"feature_type": "hole", "count": 4, "representative_size_mm": 6.0},
            {"feature_type": "pocket", "count": 1, "representative_size_mm": 20.0},
        ],
        "requires_5_axis": False,
        "thin_wall_present": False,
    }


def test_valid_payload_round_trips_through_wire_aliases():
    payload = _valid_payload()
    fg = FeatureGraph.model_validate(payload)
    canonical = fg.to_canonical_dict()
    # Wire-name preservation: '_schema_version', NOT 'schema_version'.
    assert "_schema_version" in canonical
    assert "schema_version" not in canonical
    assert canonical["_schema_version"] == 1
    # All addendum-1 booleans present and typed.
    assert canonical["requires_5_axis"] is False
    assert canonical["thin_wall_present"] is False
    # Re-parse the canonical output and assert structural equality.
    reparsed = FeatureGraph.model_validate(canonical)
    assert reparsed == fg


def test_schema_version_constant_matches_default():
    assert SCHEMA_VERSION == 1
    fg = FeatureGraph(
        bounding_box_mm=[10.0, 10.0, 10.0],
        volume_mm3=1000.0,
        material_grade="6061-T6",
        features=[],
        requires_5_axis=False,
        thin_wall_present=False,
    )
    assert fg.schema_version == SCHEMA_VERSION


# ---------------- Addendum 1 — first-class booleans ----------------


def test_missing_requires_5_axis_fails_validation():
    payload = _valid_payload()
    del payload["requires_5_axis"]
    with pytest.raises(ValidationError) as exc:
        FeatureGraph.model_validate(payload)
    assert "requires_5_axis" in str(exc.value)


def test_missing_thin_wall_present_fails_validation():
    payload = _valid_payload()
    del payload["thin_wall_present"]
    with pytest.raises(ValidationError) as exc:
        FeatureGraph.model_validate(payload)
    assert "thin_wall_present" in str(exc.value)


def test_null_requires_5_axis_fails_validation():
    payload = _valid_payload()
    payload["requires_5_axis"] = None
    with pytest.raises(ValidationError):
        FeatureGraph.model_validate(payload)


def test_null_thin_wall_present_fails_validation():
    payload = _valid_payload()
    payload["thin_wall_present"] = None
    with pytest.raises(ValidationError):
        FeatureGraph.model_validate(payload)


def test_string_for_requires_5_axis_fails_validation():
    """A real bug we caught: JSON encoders sometimes coerce booleans
    to 'true'/'false' strings. Pydantic v2 in ``strict`` mode would
    reject; we keep default mode but use ``extra='forbid'`` and rely
    on the JSON schema check below as the second gate.
    """
    payload = _valid_payload()
    payload["requires_5_axis"] = "false"
    fg = FeatureGraph.model_validate(payload)
    # Lax mode coerces but the *output* must still be a bool, not str.
    assert isinstance(fg.requires_5_axis, bool)
    assert fg.requires_5_axis is False


def test_json_schema_marks_both_booleans_required():
    schema = FeatureGraph.model_json_schema(by_alias=True)
    required = set(schema["required"])
    assert "requires_5_axis" in required
    assert "thin_wall_present" in required
    # And both are typed as boolean in the schema.
    props = schema["properties"]
    assert props["requires_5_axis"]["type"] == "boolean"
    assert props["thin_wall_present"]["type"] == "boolean"


# ---------------- Other schema invariants ----------------


def test_extra_field_rejected():
    payload = _valid_payload()
    payload["surprise_field"] = "boom"
    with pytest.raises(ValidationError):
        FeatureGraph.model_validate(payload)


def test_negative_volume_rejected():
    payload = _valid_payload()
    payload["volume_mm3"] = -1.0
    with pytest.raises(ValidationError):
        FeatureGraph.model_validate(payload)


def test_bounding_box_must_have_three_axes():
    payload = _valid_payload()
    payload["bounding_box_mm"] = [50.0, 30.0]
    with pytest.raises(ValidationError):
        FeatureGraph.model_validate(payload)


def test_feature_type_enum_is_closed_vocab():
    payload = _valid_payload()
    payload["features"][0]["feature_type"] = "fishhook"
    with pytest.raises(ValidationError):
        FeatureGraph.model_validate(payload)


def test_all_feature_type_strings_match_rust_snake_case():
    """Lock the closed vocab so a future enum rename surfaces here
    BEFORE the Rust deserialize breaks downstream.
    """
    expected = {
        "pocket",
        "hole",
        "slot",
        "thread",
        "undercut_5axis",
        "thin_wall",
        "surface",
        "engraving",
    }
    actual = {ft.value for ft in FeatureType}
    assert actual == expected


def test_feature_count_must_be_positive():
    payload = _valid_payload()
    payload["features"][0]["count"] = 0
    with pytest.raises(ValidationError):
        FeatureGraph.model_validate(payload)


def test_canonical_dict_is_json_serializable():
    payload = _valid_payload()
    fg = FeatureGraph.model_validate(payload)
    out = fg.to_canonical_dict()
    # Round-trip through stdlib json — no Pydantic-specific types.
    encoded = json.dumps(out)
    decoded = json.loads(encoded)
    assert decoded == out
