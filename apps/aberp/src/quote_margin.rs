//! S428 — customer-type margin policy resolution for the pricing engine.
//!
//! The engine ([`aberp_quote_engine`]) is a pure crate with a fixed
//! two-knob margin model: `profit_margin_base` is the markup applied to
//! `(subtotal + overhead)`, and `min_margin` is the realized-margin floor
//! (`margin / total_price`) below which it refuses to quote. This module
//! is the **pipeline-side** glue that decides which knobs to feed it for a
//! given quote, without touching the engine:
//!
//! 1. If the buyer partner's [`CustomerType`] matches an active
//!    [`MarginProfile`], the profile's `gross_margin_pct` replaces
//!    `profit_margin_base` and its `min_margin_pct` becomes the floor.
//! 2. An operator margin override (set on the quote detail) replaces the
//!    applied markup outright (keeping the same floor).
//! 3. Otherwise the engine's own global parameters apply unchanged.
//!
//! For paths (1) and (2) the engine's `min_margin` is relaxed to `0.0` so
//! a below-floor quote still **prices** (the floor is then surfaced as an
//! operator-facing banner + a hard DEAL block — [[trust-code-not-operator]])
//! rather than failing to price. The global path keeps the engine's
//! existing hard refusal so its behaviour is unchanged ([[hulye-biztos]]).

use aberp_quote_engine::QuotingParameters;

use crate::margin_profiles::MarginProfile;
use crate::partners::CustomerType;

/// Where the applied margin came from. Drives which audit event fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarginSource {
    /// A matching active profile drove the markup.
    Profile,
    /// The operator overrode the margin manually.
    Override,
    /// No profile matched (or `unset`) and no override — global default.
    Global,
}

/// Resolved margin policy for one pricing pass.
#[derive(Debug, Clone, PartialEq)]
pub struct MarginPolicy {
    /// The markup to feed the engine as `profit_margin_base`.
    pub applied_margin_base: f64,
    /// The realized-margin floor (`margin / total_price`) to enforce.
    pub floor_pct: f64,
    /// `true` when the pipeline (not the engine) enforces the floor: the
    /// engine's `min_margin` is relaxed to 0 so the quote prices, and
    /// [`evaluate_floor`](MarginPolicy::evaluate_floor) decides the breach.
    pub pipeline_enforced: bool,
    pub source: MarginSource,
    /// The driving profile id, when `source == Profile`.
    pub profile_id: Option<String>,
}

impl MarginPolicy {
    /// Decide the policy from the (optional) matched profile, the
    /// (optional) operator override, and the engine's global knobs.
    ///
    /// Precedence for the applied markup: override → profile → global.
    /// The floor is the profile's `min_margin_pct` when a profile applies,
    /// else the engine's global `min_margin`.
    pub fn resolve(
        profile: Option<&MarginProfile>,
        override_pct: Option<f64>,
        global_margin_base: f64,
        global_min_margin: f64,
    ) -> Self {
        let floor_pct = profile
            .map(|p| p.min_margin_pct)
            .unwrap_or(global_min_margin);
        match (override_pct, profile) {
            (Some(ov), prof) => MarginPolicy {
                applied_margin_base: ov,
                floor_pct,
                pipeline_enforced: true,
                source: MarginSource::Override,
                profile_id: prof.map(|p| p.id.clone()),
            },
            (None, Some(p)) => MarginPolicy {
                applied_margin_base: p.gross_margin_pct,
                floor_pct,
                pipeline_enforced: true,
                source: MarginSource::Profile,
                profile_id: Some(p.id.clone()),
            },
            (None, None) => MarginPolicy {
                applied_margin_base: global_margin_base,
                floor_pct,
                pipeline_enforced: false,
                source: MarginSource::Global,
                profile_id: None,
            },
        }
    }

    /// Mutate the engine parameters to enact this policy. Sets the markup,
    /// and (for pipeline-enforced paths) relaxes the engine floor to 0 so
    /// a below-floor quote still produces a breakdown.
    pub fn apply(&self, params: &mut QuotingParameters) {
        params.profit_margin_base = self.applied_margin_base;
        if self.pipeline_enforced {
            params.min_margin = 0.0;
        }
    }

    /// Realized margin (`margin / total_price`) of a priced breakdown.
    pub fn realized_margin_pct(margin: f64, total_price: f64) -> f64 {
        if total_price > 0.0 {
            margin / total_price
        } else {
            0.0
        }
    }

    /// Does this priced result breach the floor? Only pipeline-enforced
    /// paths can breach here (the global path's floor is enforced inside
    /// the engine). A tiny epsilon avoids float-equality false positives.
    pub fn is_below_floor(&self, margin: f64, total_price: f64) -> bool {
        if !self.pipeline_enforced {
            return false;
        }
        Self::realized_margin_pct(margin, total_price) < self.floor_pct - 1e-9
    }
}

/// Resolve the [`CustomerType`] of a buyer partner id, returning
/// [`CustomerType::Unset`] when the id is `None` or the partner is gone.
/// Kept here so both the pipeline and the re-price endpoints share one
/// definition of "the buyer's segment".
pub fn customer_type_for_partner(
    conn: &duckdb::Connection,
    tenant: &str,
    buyer_partner_id: Option<&str>,
) -> anyhow::Result<CustomerType> {
    let Some(id) = buyer_partner_id else {
        return Ok(CustomerType::Unset);
    };
    match crate::partners::get_partner(conn, tenant, id)? {
        Some(p) => Ok(p.customer_type),
        None => Ok(CustomerType::Unset),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(gross: f64, min: f64) -> MarginProfile {
        MarginProfile {
            id: "mp_x".to_string(),
            name: "p".to_string(),
            customer_type: "defense".to_string(),
            gross_margin_pct: gross,
            min_margin_pct: min,
            notes: None,
            enabled: true,
            created_at: "t".to_string(),
            updated_at: "t".to_string(),
            archived_at: None,
        }
    }

    #[test]
    fn no_profile_no_override_uses_global_and_keeps_engine_floor() {
        let p = MarginPolicy::resolve(None, None, 0.35, 0.10);
        assert_eq!(p.source, MarginSource::Global);
        assert_eq!(p.applied_margin_base, 0.35);
        assert!(!p.pipeline_enforced);
        // global path never breaches in the pipeline (engine enforces).
        assert!(!p.is_below_floor(0.0, 100.0));
    }

    #[test]
    fn profile_drives_markup_and_floor() {
        let prof = profile(0.40, 0.10);
        let p = MarginPolicy::resolve(Some(&prof), None, 0.35, 0.05);
        assert_eq!(p.source, MarginSource::Profile);
        assert_eq!(p.applied_margin_base, 0.40);
        assert_eq!(p.floor_pct, 0.10);
        assert!(p.pipeline_enforced);
        assert_eq!(p.profile_id.as_deref(), Some("mp_x"));
    }

    #[test]
    fn override_wins_over_profile_keeps_profile_floor() {
        let prof = profile(0.40, 0.20);
        let p = MarginPolicy::resolve(Some(&prof), Some(0.05), 0.35, 0.10);
        assert_eq!(p.source, MarginSource::Override);
        assert_eq!(p.applied_margin_base, 0.05);
        assert_eq!(p.floor_pct, 0.20);
        assert!(p.pipeline_enforced);
    }

    #[test]
    fn below_floor_detected_on_low_realized_margin() {
        let prof = profile(0.04, 0.10); // misconfigured: target below floor
        let p = MarginPolicy::resolve(Some(&prof), None, 0.35, 0.05);
        // realized 4/104 ≈ 0.0385 < 0.10 → breach
        assert!(p.is_below_floor(4.0, 104.0));
        // a healthy margin does not breach
        let prof2 = profile(0.40, 0.10);
        let p2 = MarginPolicy::resolve(Some(&prof2), None, 0.35, 0.05);
        assert!(!p2.is_below_floor(40.0, 140.0)); // 0.286 ≥ 0.10
    }
}
