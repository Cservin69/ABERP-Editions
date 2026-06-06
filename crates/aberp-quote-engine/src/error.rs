//! Typed engine failures. Every variant carries enough context that
//! a wiring-layer log message can name *exactly* what is unsatisfiable
//! about the inputs — operators reading the SPA never see "engine
//! errored", they see "complexity rule missing for hole/XS/count=42".

use thiserror::Error;

/// Every reason the engine refuses to produce a quote.
#[derive(Debug, Error)]
pub enum QuoteError {
    /// `FeatureGraph::material_grade` is not present in the input
    /// material slice.
    #[error("material grade `{grade}` is not in the catalogue snapshot")]
    MaterialNotInCatalogue {
        /// The grade requested by the feature graph.
        grade: String,
    },

    /// No row of `quoting_tolerance_multipliers` matches the requested
    /// target tolerance.
    #[error("no tolerance multiplier row for `{tolerance}`")]
    ToleranceNotInTable {
        /// The target tolerance the caller asked for, as DB string.
        tolerance: String,
    },

    /// No `quoting_complexity_rules` row matches this feature triple.
    /// The wiring layer should ensure a catch-all open-ended rule
    /// exists for every `(feature_type, size_bucket)` it expects to
    /// see; loud failure is better than silently dropping a feature.
    #[error("no complexity rule for ({feature_type}/{size_bucket}/count={count})")]
    NoComplexityRuleForFeature {
        /// Feature type DB string.
        feature_type: String,
        /// Size bucket DB string.
        size_bucket: String,
        /// The actual count on the part.
        count: u32,
    },

    /// A `quoting_complexity_rules` row has `count_min > count_max`.
    /// Refuse the snapshot loud rather than ignore the broken row.
    #[error(
        "complexity rule id={rule_id} has inverted bounds: count_min={count_min} > count_max={count_max:?}"
    )]
    InvalidComplexityRule {
        /// Snapshot rule id.
        rule_id: i64,
        /// The row's `count_min`.
        count_min: u32,
        /// The row's `count_max` (Option for completeness, though
        /// `None` cannot be inverted).
        count_max: Option<u32>,
    },

    /// Quantity must be ≥ 1.
    #[error("quantity must be at least 1; received 0")]
    QuantityZero,

    /// The computed margin / total price ratio is below
    /// [`crate::QuotingParameters::min_margin`]. Refuse the quote —
    /// quoting below floor is worse than not quoting.
    #[error(
        "computed margin {actual_pct:.4} below configured floor {floor_pct:.4} (total_price={total_price:.4})"
    )]
    MarginFloorViolation {
        /// Actual margin / total_price.
        actual_pct: f64,
        /// `quoting_parameters.min_margin`.
        floor_pct: f64,
        /// The total price computed before the floor check.
        total_price: f64,
    },

    /// The feature-graph schema version is newer than this engine
    /// build understands. The wrapper (S270) should catch this
    /// before we ever get here; defence-in-depth.
    #[error(
        "feature-graph schema version {got} is not understood by engine (supports {supported})"
    )]
    UnsupportedSchemaVersion {
        /// What the graph declared.
        got: u32,
        /// What this build understands (always
        /// [`crate::FeatureGraph::SCHEMA_VERSION`]).
        supported: u32,
    },
}
