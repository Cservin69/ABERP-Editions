//! The engine's output — a transparent, operator-readable breakdown.
//!
//! Every monetary line and every minute count is named here. The
//! `reasoning_log` carries the *step-by-step* justification — every
//! contribution that influenced any number above it. The PDF surface
//! (S271) renders the numbers; the SPA detail view renders the log.

use serde::{Deserialize, Serialize};

/// What the engine returns to the wiring layer.
///
/// **Persistence note.** The wiring layer (S271) persists this as
/// the `quotes.calculated_breakdown_json`. Once a quote is `accepted`
/// the persisted value is the frozen contract (design §10) — the
/// engine never re-runs against an accepted quote.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuoteBreakdown {
    /// EUR. Base material × density × volume × scrap, with the stock
    /// adjustment and exotic-material tax folded in. Step 1–2 + 10
    /// + 11 of the engine algorithm.
    pub material_cost: f64,
    /// EUR. The machining line — (machining_minutes +
    /// inspection_minutes) × machining_rate × tolerance_multiplier ×
    /// material multipliers, with the thin-wall + tight-tolerance bump
    /// (see [`crate::THIN_WALL_TIGHT_TOL_BUMP`]) applied. Where
    /// `machining_minutes` is the geometry-driven roughing + finishing
    /// total (report §5).
    ///
    /// **Wire key stays `labor_cost`** via `#[serde(rename)]`: the
    /// persisted `breakdown_json` and the audit payload are an
    /// immutable history, and the storefront stores the blob opaquely
    /// (`quote-store.ts` "never inspects breakdown_json"). The Rust
    /// field renamed to `machining_cost` (S418 vocabulary), the bytes
    /// on the wire did not — no migration, no storefront break.
    #[serde(rename = "labor_cost")]
    pub machining_cost: f64,
    /// EUR. The amortised per-part share of the one-time CAD-CAM
    /// programming / fixturing cost — `cad_cam_hours × rate ÷ qty`
    /// (report §4). Always amortised: programming is done once for the
    /// batch. New in S418 (no prior wire key).
    pub cad_cam_cost: f64,
    /// EUR. The amortised / per-part share of the setup minutes
    /// (`setup_base + 5-axis extra + fired-rule penalties`), report
    /// §5.5.
    pub setup_cost: f64,
    /// EUR. **ADR-0094 Gap 3.** Sum of every gear's tooth-generation op cost
    /// (`Σ gear_min × routed-family effective €/min`), folded into the
    /// subtotal alongside material/machining/setup/cad_cam. Per part — each
    /// part's teeth are cut. `#[serde(default)]` so pre-Gap-3 persisted blobs
    /// deserialize as `0.0`; `skip_serializing_if` omits it from the wire when
    /// it is exactly zero (no gears), keeping a no-gear `breakdown_json`
    /// byte-identical to pre-Gap-3 — the inert-by-default wire contract.
    #[serde(default, skip_serializing_if = "is_zero_eur")]
    pub gear_cost: f64,
    /// EUR. `subtotal × overhead_factor` (step 13).
    pub overhead: f64,
    /// EUR. `(subtotal + overhead) × profit_margin_base` (step 14).
    pub margin: f64,
    /// EUR. subtotal + overhead + margin (step 15).
    pub total_price: f64,
    /// Minutes of machining per part — the geometry-driven roughing +
    /// finishing total (report §5.2), plus any feature-graph rule time
    /// (0 today: STL/STEP v1 emit no features). Difficulty is folded
    /// into the roughing/finishing terms; tolerance + thin-wall
    /// multipliers are applied to the cost, not these minutes.
    pub machining_minutes: f64,
    /// Minutes — `inspection_minutes_per_feature × feature_count`,
    /// where feature_count is the total number of `Feature` entries
    /// (not the sum of their `count` field — one row per drill
    /// operation, not per hole).
    pub inspection_minutes: f64,
    /// True iff [`crate::FeatureGraph::requires_5_axis`] was set. The
    /// SPA uses this to label the quote with the chosen machine
    /// class; the v1 engine does not bill differently for 5-axis
    /// (design doc note — machine-rate split is S270+ work).
    pub route_to_5_axis: bool,
    /// S429 — the closed-loop calibration coefficient applied to the routed
    /// family's `machining_minutes` (and therefore to the machining cost). A
    /// neutral price has `1.0`. The sample emitter recovers the engine's
    /// pre-coefficient base via `machining_minutes / calibration_coefficient`.
    ///
    /// `#[serde(default = ...)]` so `breakdown_json` blobs persisted before
    /// S429 deserialize as a neutral `1.0` rather than failing.
    #[serde(default = "default_calibration_coefficient")]
    pub calibration_coefficient: f64,
    /// Stamp the engine version that produced this breakdown — lets
    /// future "re-quoted by engine vX.Y vs persisted by vA.B" audits
    /// be cleanly forensic.
    pub engine_version: String,
    /// One line per algorithm step, in execution order. The log is
    /// the trust signal per `[[trust-code-not-operator]]` — same
    /// inputs ⇒ byte-identical log.
    pub reasoning_log: Vec<String>,
}

/// Serde default for [`QuoteBreakdown::calibration_coefficient`] — pre-S429
/// persisted breakdowns predate the field and price as neutral.
fn default_calibration_coefficient() -> f64 {
    1.0
}

/// Serde skip predicate for [`QuoteBreakdown::gear_cost`] — omit the field
/// from the wire when it is exactly zero (the no-gear path), keeping a
/// no-gear `breakdown_json` byte-identical to pre-ADR-0094-Gap-3.
#[allow(clippy::float_cmp)]
fn is_zero_eur(v: &f64) -> bool {
    *v == 0.0
}
