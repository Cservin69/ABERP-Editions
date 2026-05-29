//! Unit-of-measure domain types — `NavUnitOfMeasure` + `ProductUnit`.
//!
//! # Why these live in billing (S159)
//!
//! PR-91 first introduced these types in `apps/aberp/src/products.rs` for
//! the products master-data. S159 threads a line's unit-of-measure all the
//! way to the NAV `<unitOfMeasure>` emit, which reads off
//! [`crate::domain::invoice::LineItem`]. `LineItem` is a billing domain
//! type, so the unit type must be reachable from billing without a
//! backwards `billing → app` dependency. The types moved DOWN here; the
//! products module re-exports them (`pub use aberp_billing::{…}`) so its
//! existing API is unchanged.
//!
//! # The unit-of-measure model — load-bearing
//!
//! NAV's v3.0 InvoiceData schema requires every `<line>` to carry a
//! `<unitOfMeasure>` element whose body is one of a closed enum of tokens,
//! OR the literal `OWN` paired with a `<unitOfMeasureOwn>` free-text
//! element. The product's unit maps to that wire shape cleanly so the
//! "pick product → autofill line → NAV emit" path (PR-100 + S159) hands the
//! operator's catalog entry straight to the emitter.

use serde::{Deserialize, Serialize};

// ──────────────────────────────────────────────────────────────────────
// NavUnitOfMeasure — closed-vocab mirror of the NAV v3.0
// unitOfMeasureType enum (sans OWN — see ProductUnit).
// ──────────────────────────────────────────────────────────────────────

/// NAV v3.0 `unitOfMeasureType` enum mirror. Each variant serialises as
/// the NAV-defined token (`PIECE`, `KILOGRAM`, …) via serde's
/// `rename_all = "SCREAMING_SNAKE_CASE"` — wire body and NAV XML body
/// agree by construction.
///
/// `OWN` is intentionally NOT a variant here. The `OWN` /
/// `unitOfMeasureOwn` pairing on the NAV side is modelled at the
/// outer [`ProductUnit`] level so callers cannot accidentally emit
/// `OWN` without the paired free-text payload — a class of bug that a
/// flat `Nav(OWN)` shape would invite.
///
/// Adding a variant: confirm against NAV's v3.0 unitOfMeasureType
/// schema, extend the enum + the SCREAMING_SNAKE serde mapping, then
/// widen the SPA's dropdown. See ADR-0046.
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone, Copy)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum NavUnitOfMeasure {
    Piece,
    Kilogram,
    Ton,
    Kwh,
    Day,
    Hour,
    Minute,
    Month,
    Liter,
    Kilometer,
    CubicMeter,
    Meter,
    LinearMeter,
    Carton,
    Pack,
}

impl NavUnitOfMeasure {
    /// NAV v3.0 token body — what goes between `<unitOfMeasure>` and
    /// `</unitOfMeasure>` in InvoiceData XML. The NAV XML emitter
    /// (`apps/aberp/src/nav_xml.rs::write_lines`, S159) uses this
    /// directly; serde callers receive the same string via `Serialize`.
    pub fn nav_token(self) -> &'static str {
        match self {
            NavUnitOfMeasure::Piece => "PIECE",
            NavUnitOfMeasure::Kilogram => "KILOGRAM",
            NavUnitOfMeasure::Ton => "TON",
            NavUnitOfMeasure::Kwh => "KWH",
            NavUnitOfMeasure::Day => "DAY",
            NavUnitOfMeasure::Hour => "HOUR",
            NavUnitOfMeasure::Minute => "MINUTE",
            NavUnitOfMeasure::Month => "MONTH",
            NavUnitOfMeasure::Liter => "LITER",
            NavUnitOfMeasure::Kilometer => "KILOMETER",
            NavUnitOfMeasure::CubicMeter => "CUBIC_METER",
            NavUnitOfMeasure::Meter => "METER",
            NavUnitOfMeasure::LinearMeter => "LINEAR_METER",
            NavUnitOfMeasure::Carton => "CARTON",
            NavUnitOfMeasure::Pack => "PACK",
        }
    }

    /// Parse a NAV token string back to the enum. Used by the products
    /// module's DB read path (`unit_from_db_columns`). `None` for any
    /// string outside the closed vocab.
    pub fn from_nav_token(token: &str) -> Option<Self> {
        match token {
            "PIECE" => Some(NavUnitOfMeasure::Piece),
            "KILOGRAM" => Some(NavUnitOfMeasure::Kilogram),
            "TON" => Some(NavUnitOfMeasure::Ton),
            "KWH" => Some(NavUnitOfMeasure::Kwh),
            "DAY" => Some(NavUnitOfMeasure::Day),
            "HOUR" => Some(NavUnitOfMeasure::Hour),
            "MINUTE" => Some(NavUnitOfMeasure::Minute),
            "MONTH" => Some(NavUnitOfMeasure::Month),
            "LITER" => Some(NavUnitOfMeasure::Liter),
            "KILOMETER" => Some(NavUnitOfMeasure::Kilometer),
            "CUBIC_METER" => Some(NavUnitOfMeasure::CubicMeter),
            "METER" => Some(NavUnitOfMeasure::Meter),
            "LINEAR_METER" => Some(NavUnitOfMeasure::LinearMeter),
            "CARTON" => Some(NavUnitOfMeasure::Carton),
            "PACK" => Some(NavUnitOfMeasure::Pack),
            _ => None,
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// ProductUnit — `Nav(enum) | Own(String)` outer sum.
// ──────────────────────────────────────────────────────────────────────

/// A unit of measure: either one of the NAV-defined tokens
/// ([`NavUnitOfMeasure`]) or a free-text label that the NAV emitter
/// renders as `OWN` + `<unitOfMeasureOwn>{label}</...>`.
///
/// Wire shape (internally-tagged serde):
///
/// ```json
/// {"kind": "Nav", "value": "PIECE"}
/// {"kind": "Own", "value": "liter@15C"}
/// ```
///
/// The tagged shape keeps the JSON self-describing for SPA debugging
/// at minimal cost (one extra field name). Pinned by
/// `product_unit_serde_round_trip_pin`.
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone)]
#[serde(tag = "kind", content = "value")]
pub enum ProductUnit {
    /// One of the NAV v3.0 unitOfMeasure tokens.
    Nav(NavUnitOfMeasure),
    /// Operator-typed free-text label. The NAV emitter pairs this with
    /// the literal `OWN` token in the wire `<unitOfMeasure>` element.
    /// `liter@15C` (fuel measure) is the canonical example — no plain
    /// LITER variant covers the temperature correction.
    Own(String),
}
