//! S271 / PR-260 — `stock_alert` recompute (EVE addendum 2 data half).
//!
//! Pure-function core: given (snapshot_status_at_accept, current_status,
//! stored_alert), decide whether the quote needs a stock_alert flag.
//!
//! **Sticky semantics.** Once `stored_alert == true`, the function
//! returns `true` regardless of the snapshot / current comparison. EVE's
//! brief is explicit: a stock-status downgrade between quote acceptance
//! and DEAL surfaces a banner the operator must consciously acknowledge
//! (the typed REFRESH token UI lives in S272/PR-261); recovery of the
//! material stock_status BACK to the snapshot tier does NOT un-trigger
//! the alert. The operator's REFRESH is the only path back.
//!
//! **Ordinal ladder.** The current [`crate::quoting_materials::StockStatus`]
//! vocabulary is four-valued: `in_stock < source_1_2d < source_3_7d <
//! special_order`. We assign the same monotone ordinal in
//! [`stock_status_ordinal`]; a downgrade is `current > snapshot`. An
//! upgrade or equal tier is not an alert. A snapshot of NULL (the quote
//! has not yet transitioned `priced → accepted` storefront-side) skips
//! the recompute entirely. A current status that has been removed from
//! the catalogue (the operator deleted the material since acceptance)
//! also skips — we cannot reason about "downgrade vs disappeared" without
//! more storefront-side state, so the conservative call is no-op rather
//! than firing a false alarm.
//!
//! The persistence + audit-emit layer lives in
//! [`crate::quote_intake_query::list_quote_intake_rows`] (read-side
//! recompute pass) and the SPA list route (handler emits one
//! `QuoteStockAlertTriggered` audit entry per row whose stored value
//! transitions FALSE → TRUE this call).

use crate::quoting_materials::StockStatus;

/// Monotone ordinal for downgrade detection. **Higher == worse stock
/// posture**. Pinned by [`tests::stock_status_ordinal_is_monotone`] so
/// a future re-ordering of the [`StockStatus`] variants doesn't silently
/// reverse the comparison.
pub fn stock_status_ordinal(s: StockStatus) -> u8 {
    match s {
        StockStatus::InStock => 0,
        StockStatus::Source1_2d => 1,
        StockStatus::Source3_7d => 2,
        StockStatus::SpecialOrder => 3,
    }
}

/// Coerce a nullable DB-stored `stock_alert` value to its boolean form.
/// NULL (pre-S271 row OR a row that the recompute pass has never visited)
/// maps to `false`. See `crates/aberp-quote-intake/src/log_table.rs` for
/// why the column carries no SQL `DEFAULT FALSE` (DuckDB clobber trap).
pub fn coerce_stock_alert(stored: Option<bool>) -> bool {
    stored.unwrap_or(false)
}

/// Pure decision function. Returns the **next** stored value of
/// `stock_alert` after one recompute pass. Caller persists only on a
/// FALSE → TRUE transition.
///
/// - `snapshot_status_at_accept`: the storefront-snapped material
///   stock_status at the moment the quote transitioned
///   `priced → accepted`. `None` when the quote has not yet been
///   accepted; the recompute skips (no alert possible without a
///   snapshot to compare against).
/// - `current_status`: the live `quoting_materials.stock_status` for
///   the quote's material grade. `None` when the material is no longer
///   in the catalogue (operator deleted it); the recompute skips
///   conservatively.
/// - `stored_alert`: the current value of `quote_intake_log.stock_alert`
///   (NULL coerced to `false` via [`coerce_stock_alert`] BEFORE this
///   call).
pub fn recompute_stock_alert(
    snapshot_status_at_accept: Option<&str>,
    current_status: Option<&str>,
    stored_alert: bool,
) -> bool {
    // Sticky: once stored_alert is TRUE, only the operator's REFRESH
    // (S272+) can untrigger it. The recompute pass NEVER resets the bit.
    if stored_alert {
        return true;
    }
    let Some(snapshot) = snapshot_status_at_accept.and_then(StockStatus::from_db_str) else {
        return false;
    };
    let Some(current) = current_status.and_then(StockStatus::from_db_str) else {
        return false;
    };
    stock_status_ordinal(current) > stock_status_ordinal(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four-variant vocab `in_stock < source_1_2d < source_3_7d <
    /// special_order` MUST stay monotone in
    /// [`stock_status_ordinal`]. A future re-ordering of the
    /// [`StockStatus`] enum (e.g. inserting a new tier in the middle)
    /// MUST update [`stock_status_ordinal`] in tandem; this pin fires
    /// otherwise.
    #[test]
    fn stock_status_ordinal_is_monotone() {
        assert!(
            stock_status_ordinal(StockStatus::InStock)
                < stock_status_ordinal(StockStatus::Source1_2d)
        );
        assert!(
            stock_status_ordinal(StockStatus::Source1_2d)
                < stock_status_ordinal(StockStatus::Source3_7d)
        );
        assert!(
            stock_status_ordinal(StockStatus::Source3_7d)
                < stock_status_ordinal(StockStatus::SpecialOrder)
        );
    }

    #[test]
    fn coerce_null_to_false_otherwise_passthrough() {
        assert!(!coerce_stock_alert(None));
        assert!(!coerce_stock_alert(Some(false)));
        assert!(coerce_stock_alert(Some(true)));
    }

    #[test]
    fn no_snapshot_means_no_alert() {
        // Quote not yet accepted storefront-side → no comparison to make.
        assert!(!recompute_stock_alert(None, Some("source_1_2d"), false));
    }

    #[test]
    fn material_gone_from_catalogue_means_no_alert() {
        // Operator deleted the material since acceptance. We cannot
        // distinguish "downgrade" from "disappeared"; conservative no-op.
        assert!(!recompute_stock_alert(Some("in_stock"), None, false));
        assert!(!recompute_stock_alert(
            Some("in_stock"),
            Some("not_a_known_status"),
            false
        ));
    }

    #[test]
    fn invalid_snapshot_string_means_no_alert() {
        // A snapshot value the closed-vocab parser doesn't recognise —
        // same conservative no-op.
        assert!(!recompute_stock_alert(
            Some("garbage"),
            Some("in_stock"),
            false
        ));
    }

    #[test]
    fn equal_tier_is_no_alert() {
        for s in StockStatus::ALL {
            let str = s.as_db_str();
            assert!(
                !recompute_stock_alert(Some(str), Some(str), false),
                "equal-tier on {str} must not trigger"
            );
        }
    }

    #[test]
    fn upgrade_is_no_alert() {
        // Stock improved since acceptance — definitely no alert.
        assert!(!recompute_stock_alert(
            Some("source_1_2d"),
            Some("in_stock"),
            false
        ));
        assert!(!recompute_stock_alert(
            Some("special_order"),
            Some("source_3_7d"),
            false
        ));
    }

    #[test]
    fn downgrade_triggers_alert() {
        // The EVE addendum 2 canonical case: accepted at InStock, now at
        // Source_1_2d → alert.
        assert!(recompute_stock_alert(
            Some("in_stock"),
            Some("source_1_2d"),
            false
        ));
        // Bigger downgrade also triggers.
        assert!(recompute_stock_alert(
            Some("in_stock"),
            Some("special_order"),
            false
        ));
        // Mid-ladder downgrade also triggers.
        assert!(recompute_stock_alert(
            Some("source_1_2d"),
            Some("source_3_7d"),
            false
        ));
    }

    /// Sticky: once `stored_alert == true`, the function NEVER returns
    /// false again — even if `current_status` recovers all the way back
    /// to the snapshot tier or BELOW it. The operator's REFRESH (S272+)
    /// is the only path back. This is the load-bearing EVE addendum 2
    /// invariant and the cut report's "sticky downgrade test passes
    /// both directions" guarantee.
    #[test]
    fn sticky_alert_survives_recovery_in_both_directions() {
        // Downgrade triggers; stored becomes TRUE.
        let after_downgrade = recompute_stock_alert(Some("in_stock"), Some("source_1_2d"), false);
        assert!(after_downgrade, "downgrade must trigger");

        // Now recovery (current == snapshot, equal tier). Sticky.
        let after_recovery =
            recompute_stock_alert(Some("in_stock"), Some("in_stock"), after_downgrade);
        assert!(
            after_recovery,
            "sticky: equal-tier recovery does NOT untrigger"
        );

        // Bigger recovery: catalogue removed entirely. Sticky.
        let after_purge = recompute_stock_alert(Some("in_stock"), None, after_recovery);
        assert!(after_purge, "sticky: catalogue purge does NOT untrigger");

        // Even an absurd "snapshot disappeared" run is sticky.
        let after_no_snap = recompute_stock_alert(None, Some("in_stock"), after_purge);
        assert!(after_no_snap, "sticky: missing snapshot does NOT untrigger");
    }
}
