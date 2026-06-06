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
    /// EUR. (machining_minutes + inspection_minutes) ×
    /// machining_rate × tolerance_multiplier × material multipliers,
    /// with the thin-wall + tight-tolerance bump (see
    /// [`crate::THIN_WALL_TIGHT_TOL_BUMP`]) applied at step 7.
    pub labor_cost: f64,
    /// EUR. The amortised / per-part share of the rules' setup
    /// penalty (step 9).
    pub setup_cost: f64,
    /// EUR. `subtotal × overhead_factor` (step 13).
    pub overhead: f64,
    /// EUR. `(subtotal + overhead) × profit_margin_base` (step 14).
    pub margin: f64,
    /// EUR. subtotal + overhead + margin (step 15).
    pub total_price: f64,
    /// Minutes — the unamortised total of (base_time × count × multiplier)
    /// across every matched complexity rule, before material
    /// machinability and tolerance multiplier are folded into the
    /// labour cost.
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
    /// Stamp the engine version that produced this breakdown — lets
    /// future "re-quoted by engine vX.Y vs persisted by vA.B" audits
    /// be cleanly forensic.
    pub engine_version: String,
    /// One line per algorithm step, in execution order. The log is
    /// the trust signal per `[[trust-code-not-operator]]` — same
    /// inputs ⇒ byte-identical log.
    pub reasoning_log: Vec<String>,
}
