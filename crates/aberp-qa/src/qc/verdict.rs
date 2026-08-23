//! S443 / ADR-0092 — the pure verdict computation.
//!
//! Base MTConnect (and a manual gauge) carries a *value*, not a
//! *verdict*. The pass/minor/major/critical tier is computed HERE, in
//! ABERP code, against an ABERP-held inspection-plan nominal + tolerance
//! ([[trust-code-not-operator]]). No I/O — property/table-testable.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use super::plans::InspectionPlan;

/// The computed verdict for one measurement. `calibration_stale` is a
/// distinct outcome: the row is still recorded, but NO NCR is spawned (a
/// probe that may be lying must not manufacture a false defect — ISO 9001
/// §7.1.5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Pass,
    Minor,
    Major,
    Critical,
    CalibrationStale,
}

impl Verdict {
    /// On-disk / wire token. Round-trips with [`Verdict::from_storage_str`].
    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::Pass => "pass",
            Verdict::Minor => "minor",
            Verdict::Major => "major",
            Verdict::Critical => "critical",
            Verdict::CalibrationStale => "calibration_stale",
        }
    }

    pub fn from_storage_str(s: &str) -> Result<Self, &'static str> {
        match s {
            "pass" => Ok(Verdict::Pass),
            "minor" => Ok(Verdict::Minor),
            "major" => Ok(Verdict::Major),
            "critical" => Ok(Verdict::Critical),
            "calibration_stale" => Ok(Verdict::CalibrationStale),
            _ => Err("unknown Verdict storage string"),
        }
    }

    /// An out-of-tolerance verdict that must auto-spawn an NCR. `Pass`
    /// and `CalibrationStale` do NOT.
    pub fn is_failing(&self) -> bool {
        matches!(self, Verdict::Minor | Verdict::Major | Verdict::Critical)
    }
}

/// Compute the verdict for `actual` against `plan`.
///
/// **Calibration-stale is checked FIRST.** If a calibration timestamp is
/// supplied and `current_time − last_calibration_at > stale_window`, the
/// verdict is [`Verdict::CalibrationStale`] regardless of the measured
/// value — the probe is not trusted, so no tier is computed and no NCR is
/// raised. A `None` calibration timestamp means "not a calibrated probe"
/// (a manual gauge entry without one) → the stale check is skipped and the
/// tier is computed directly.
///
/// **Tier (Ervin's S443 policy — 1×/2× of the tolerance HALF-width):**
/// the pass band is the (possibly asymmetric) signed interval
/// `[lower_tol, upper_tol]` offset from nominal. `overage` is how far the
/// deviation lies *outside* the nearer band edge (0 if inside). The tier
/// denominator is the **half-width** `(upper_tol − lower_tol) / 2`:
///
/// ```text
/// ratio = overage / half_width
///   ratio == 0        → Pass     (within tolerance)
///   0 < ratio ≤ 1     → Minor
///   1 < ratio ≤ 2     → Major
///   ratio > 2         → Critical
/// ```
///
/// For a SYMMETRIC band (`upper_tol = +t`, `lower_tol = −t`) this is
/// exactly `overage = max(0, |deviation| − t)` over `t` — the S443 brief's
/// literal formula. For an ASYMMETRIC band it uses the true signed edges,
/// so a part inside its real (asymmetric) tolerance is never flagged as a
/// false failure (CLAUDE.md rule 12 — don't manufacture a false defect).
pub fn compute_verdict(
    plan: &InspectionPlan,
    actual: f64,
    last_calibration_at: Option<OffsetDateTime>,
    current_time: OffsetDateTime,
    stale_window_seconds: u64,
) -> Verdict {
    // 1. Calibration-stale check FIRST — a stale probe yields no trusted
    //    tier (and the caller raises no NCR, only a warning).
    if let Some(cal) = last_calibration_at {
        let age_seconds = (current_time - cal).whole_seconds();
        if age_seconds > stale_window_seconds as i64 {
            return Verdict::CalibrationStale;
        }
    }

    // 2. Tier from the overage past the nearer band edge.
    let deviation = actual - plan.nominal_value;
    let overage = if deviation > plan.upper_tol {
        deviation - plan.upper_tol
    } else if deviation < plan.lower_tol {
        plan.lower_tol - deviation
    } else {
        0.0
    };
    if overage <= 0.0 {
        return Verdict::Pass;
    }

    let half_width = (plan.upper_tol - plan.lower_tol) / 2.0;
    // A zero/negative half-width is rejected at plan-create time; guard
    // anyway so a malformed plan can never silently pass an out-of-band part.
    let ratio = if half_width > 0.0 {
        overage / half_width
    } else {
        f64::INFINITY
    };
    if ratio <= 1.0 {
        Verdict::Minor
    } else if ratio <= 2.0 {
        Verdict::Major
    } else {
        Verdict::Critical
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qc::plans::InspectionPlan;
    use time::Duration;

    fn plan(nominal: f64, upper: f64, lower: f64) -> InspectionPlan {
        InspectionPlan {
            plan_id: "qcp_test".into(),
            product_id: "prod_1".into(),
            feature_name: "Bore Ø".into(),
            nominal_value: nominal,
            upper_tol: upper,
            lower_tol: lower,
            units: "mm".into(),
            optional_probe_cycle_id: None,
            enabled: true,
            created_at: "2026-06-17T00:00:00Z".into(),
            archived_at: None,
            // ADR-0199 §D3(a) — identity metadata. The verdict does not
            // read any of it (the tier is a pure function of nominal +
            // band + calibration), so the fixture leaves it unset; that
            // absence is itself the pin that ADR-0199 did not change
            // ADR-0092's verdict.
            characteristic_number: None,
            characteristic_designator: None,
            characteristic_type: None,
            inspection_method: None,
            sheet_zone: None,
            is_required: None,
        }
    }

    fn t(s: &str) -> OffsetDateTime {
        OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).unwrap()
    }

    // Symmetric band ±0.010 around nominal 10.0 → half_width 0.010.
    #[test]
    fn within_band_passes() {
        let p = plan(10.0, 0.010, -0.010);
        let now = t("2026-06-17T12:00:00Z");
        assert_eq!(compute_verdict(&p, 10.0, None, now, 86400), Verdict::Pass);
        assert_eq!(compute_verdict(&p, 10.010, None, now, 86400), Verdict::Pass); // exact edge
        assert_eq!(compute_verdict(&p, 9.990, None, now, 86400), Verdict::Pass);
        // exact lower edge
    }

    #[test]
    fn just_past_one_half_width_is_minor() {
        let p = plan(10.0, 0.010, -0.010);
        let now = t("2026-06-17T12:00:00Z");
        // overage 0.010 == 1× half_width → Minor (boundary inclusive).
        assert_eq!(
            compute_verdict(&p, 10.020, None, now, 86400),
            Verdict::Minor
        );
        // overage 0.005 → ratio 0.5 → Minor.
        assert_eq!(
            compute_verdict(&p, 10.015, None, now, 86400),
            Verdict::Minor
        );
    }

    #[test]
    fn between_one_and_two_is_major() {
        let p = plan(10.0, 0.010, -0.010);
        let now = t("2026-06-17T12:00:00Z");
        // overage 0.015 → ratio 1.5 → Major.
        assert_eq!(
            compute_verdict(&p, 10.025, None, now, 86400),
            Verdict::Major
        );
        // overage 0.020 == 2× → Major (boundary inclusive).
        assert_eq!(
            compute_verdict(&p, 10.030, None, now, 86400),
            Verdict::Major
        );
    }

    #[test]
    fn past_two_is_critical() {
        let p = plan(10.0, 0.010, -0.010);
        let now = t("2026-06-17T12:00:00Z");
        // overage 0.025 → ratio 2.5 → Critical.
        assert_eq!(
            compute_verdict(&p, 10.035, None, now, 86400),
            Verdict::Critical
        );
    }

    #[test]
    fn negative_deviation_is_symmetric() {
        let p = plan(10.0, 0.010, -0.010);
        let now = t("2026-06-17T12:00:00Z");
        // 0.015 below lower edge → overage 0.005 → Minor.
        assert_eq!(compute_verdict(&p, 9.985, None, now, 86400), Verdict::Minor);
        // 0.035 below → overage 0.025 → ratio 2.5 → Critical.
        assert_eq!(
            compute_verdict(&p, 9.965, None, now, 86400),
            Verdict::Critical
        );
    }

    #[test]
    fn asymmetric_tolerance_uses_true_band_edges() {
        // Band [-0.005, +0.021] → half_width 0.013.
        let p = plan(25.0, 0.021, -0.005);
        let now = t("2026-06-17T12:00:00Z");
        // A part 0.020 above nominal is INSIDE the asymmetric band → Pass.
        // (A naive |deviation|−half_width would have false-failed this.)
        assert_eq!(compute_verdict(&p, 25.020, None, now, 86400), Verdict::Pass);
        // 0.038 above → overage 0.017 → ratio 0.017/0.013 ≈ 1.31 → Major.
        assert_eq!(
            compute_verdict(&p, 25.038, None, now, 86400),
            Verdict::Major
        );
        // 0.010 below nominal → 0.005 past lower edge → ratio 0.385 → Minor.
        assert_eq!(
            compute_verdict(&p, 24.990, None, now, 86400),
            Verdict::Minor
        );
    }

    #[test]
    fn calibration_stale_overrides_tier() {
        let p = plan(10.0, 0.010, -0.010);
        let now = t("2026-06-17T12:00:00Z");
        let stale_cal = now - Duration::days(2); // 2 days > 1 day window
                                                 // A wildly out-of-tolerance value would be Critical, but a stale
                                                 // probe yields CalibrationStale (no trusted tier, no NCR).
        assert_eq!(
            compute_verdict(&p, 10.500, Some(stale_cal), now, 86400),
            Verdict::CalibrationStale
        );
    }

    #[test]
    fn fresh_calibration_computes_tier() {
        let p = plan(10.0, 0.010, -0.010);
        let now = t("2026-06-17T12:00:00Z");
        let fresh_cal = now - Duration::hours(2); // 2h < 1 day window
        assert_eq!(
            compute_verdict(&p, 10.025, Some(fresh_cal), now, 86400),
            Verdict::Major
        );
    }

    #[test]
    fn calibration_exactly_at_window_is_not_stale() {
        let p = plan(10.0, 0.010, -0.010);
        let now = t("2026-06-17T12:00:00Z");
        // Exactly 86400s old → NOT stale (strictly greater is stale).
        let cal = now - Duration::seconds(86400);
        assert_eq!(
            compute_verdict(&p, 10.0, Some(cal), now, 86400),
            Verdict::Pass
        );
        // One second older → stale.
        let cal2 = now - Duration::seconds(86401);
        assert_eq!(
            compute_verdict(&p, 10.0, Some(cal2), now, 86400),
            Verdict::CalibrationStale
        );
    }

    #[test]
    fn tenant_window_override_8h() {
        let p = plan(10.0, 0.010, -0.010);
        let now = t("2026-06-17T12:00:00Z");
        let window_8h = 8 * 3600;
        // 4h-old calibration passes the tier under an 8h window.
        let cal_4h = now - Duration::hours(4);
        assert_eq!(
            compute_verdict(&p, 10.0, Some(cal_4h), now, window_8h),
            Verdict::Pass
        );
        // 12h-old calibration is stale under an 8h window.
        let cal_12h = now - Duration::hours(12);
        assert_eq!(
            compute_verdict(&p, 10.0, Some(cal_12h), now, window_8h),
            Verdict::CalibrationStale
        );
    }
}
