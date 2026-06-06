//! The geometry-side input contract — what the Python extractor (S269)
//! produces and the engine consumes.
//!
//! **Stub for v1.** S269 brings the canonical Python-produced JSON
//! schema; this struct mirrors the agreed shape. Don't expand it
//! without coordinating with S269 — the schema is the wire contract
//! between the two crates. Versioned via [`FeatureGraph::SCHEMA_VERSION`].

use serde::{Deserialize, Serialize};

/// One feature on the part — pocket, hole, slot, …
///
/// Closed-vocab; mirrors [`apps/aberp/src/quoting_tunables.rs`'s
/// `FeatureType` enum (S267)] verbatim so a stored
/// `quoting_complexity_rules.feature_type` string round-trips through
/// the engine without translation. The serde rename to lowercase +
/// `snake_case` matches the DB storage strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeatureType {
    /// Closed cavity / boss pocket.
    Pocket,
    /// Drilled or bored hole.
    Hole,
    /// Linear or radial slot.
    Slot,
    /// Threaded hole or external thread.
    Thread,
    /// 5-axis-only undercut (B-axis or C-axis access).
    ///
    /// Explicit rename: serde's `snake_case` rule breaks the camel-
    /// case around the digit and produces `undercut5_axis`, which
    /// disagrees with the DB string `undercut_5axis` (see
    /// [`FeatureType::as_db_str`]) AND with the S269 Python
    /// extractor's emitted JSON. Aligned here; surfaced by the
    /// S269 cross-language compat test.
    #[serde(rename = "undercut_5axis")]
    Undercut5Axis,
    /// Wall thinner than the operator-tunable thin-wall threshold.
    ThinWall,
    /// Finishing surface that needs its own toolpath.
    Surface,
    /// Engraved / etched feature.
    Engraving,
}

impl FeatureType {
    /// The DB storage string used by `quoting_complexity_rules.feature_type`
    /// (S267). Kept in sync with the `apps/aberp` enum's `as_db_str`.
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::Pocket => "pocket",
            Self::Hole => "hole",
            Self::Slot => "slot",
            Self::Thread => "thread",
            Self::Undercut5Axis => "undercut_5axis",
            Self::ThinWall => "thin_wall",
            Self::Surface => "surface",
            Self::Engraving => "engraving",
        }
    }
}

/// The size-bucket axis. Mirrors the S267 enum verbatim, including
/// the v1 boundaries returned by [`SizeBucket::range_mm`] — engine
/// and tunables MUST agree on the buckets or a feature could bucket
/// into a bucket no rule exists for. Kept duplicated here (not via
/// dependency on `apps/aberp`) so the engine stays application-
/// independent per design doc §2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum SizeBucket {
    /// 0 ≤ size < 10 mm.
    Xs,
    /// 10 ≤ size < 30 mm.
    S,
    /// 30 ≤ size < 80 mm.
    M,
    /// 80 ≤ size < 200 mm.
    L,
    /// 200 mm ≤ size (open-ended).
    Xl,
}

impl SizeBucket {
    /// DB storage string — matches `quoting_complexity_rules.size_bucket`.
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::Xs => "XS",
            Self::S => "S",
            Self::M => "M",
            Self::L => "L",
            Self::Xl => "XL",
        }
    }

    /// Half-open millimetre range `[min, max)`. `None` upper bound is
    /// open-ended (XL ≥ 200mm). Identical to S267's `range_mm`.
    pub fn range_mm(self) -> (f64, Option<f64>) {
        match self {
            Self::Xs => (0.0, Some(10.0)),
            Self::S => (10.0, Some(30.0)),
            Self::M => (30.0, Some(80.0)),
            Self::L => (80.0, Some(200.0)),
            Self::Xl => (200.0, None),
        }
    }

    /// Bucket a representative size (mm) into the appropriate bucket.
    /// Negative input is clamped to [`SizeBucket::Xs`] (the extractor
    /// should never produce negatives; defence-in-depth).
    pub fn bucket(size_mm: f64) -> Self {
        // Iterate buckets in ascending order; first whose upper bound
        // exceeds the input wins.
        for b in [Self::Xs, Self::S, Self::M, Self::L] {
            if let (_, Some(upper)) = b.range_mm() {
                if size_mm < upper {
                    return b;
                }
            }
        }
        Self::Xl
    }
}

/// Target tolerance class for the part.
///
/// Mirrors S267's `ToleranceRange`. Ordered Loose < Standard < Tight
/// < Precision < UltraPrecision — derived `PartialOrd` works because
/// the variants are declared in that order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToleranceRange {
    /// Loosest band — fewer inspection passes, lower multiplier.
    Loose,
    /// Default — design doc baseline.
    Standard,
    /// Tighter tolerance; raises multiplier and inspection time.
    Tight,
    /// Precision band — multi-pass inspection, slower toolpath.
    Precision,
    /// Highest band — CMM time per feature, thermal-stable runs.
    UltraPrecision,
}

impl ToleranceRange {
    /// DB storage string — matches `quoting_tolerance_multipliers.tolerance_range`.
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::Loose => "loose",
            Self::Standard => "standard",
            Self::Tight => "tight",
            Self::Precision => "precision",
            Self::UltraPrecision => "ultra_precision",
        }
    }
}

/// One feature on the extracted part. The engine processes features
/// in input order — the extractor is responsible for any normalising
/// sort it wants applied.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Feature {
    /// What kind of feature it is.
    pub feature_type: FeatureType,
    /// How many of this feature appear on the part. Engine
    /// distributes the time as `base_time × count × multiplier`.
    pub count: u32,
    /// A characteristic length (mm) for the feature — bore diameter,
    /// pocket width, slot length, etc. Drives the [`SizeBucket`]
    /// lookup. The extractor picks the dimension; the engine does
    /// not second-guess.
    pub representative_size_mm: f64,
}

/// The extracted-geometry side of the engine input.
///
/// Produced by `aberp-cad-extract` (Python, S269) and validated +
/// version-stamped by `aberp-cad-extract-wrapper` (Rust, S270).
/// Plain serde — no clock, no I/O.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeatureGraph {
    /// `_schema_version` lets S269 evolve the schema without breaking
    /// the wire. The wrapper refuses graphs with an unknown version.
    #[serde(rename = "_schema_version")]
    pub schema_version: u32,
    /// XYZ bounding-box extent in millimetres. Used by downstream
    /// surfacing on the PDF, not in the v1 scoring math (volume
    /// drives material cost).
    pub bounding_box_mm: [f64; 3],
    /// Solid volume in mm³ (after extraction, before scrap).
    pub volume_mm3: f64,
    /// Material grade as it appears in `quoting_materials.grade`
    /// (S266) — e.g. `6061-T6`. The engine errors with
    /// [`crate::QuoteError::MaterialNotInCatalogue`] if no row matches.
    pub material_grade: String,
    /// The features themselves. Engine processes in input order.
    pub features: Vec<Feature>,
    /// **Addendum 1 (per [[aberp-quoting-design-addenda]]).** First-
    /// class boolean for the 5-axis routing decision. The extractor
    /// will set this when any feature requires a 5-axis machine
    /// (compound angle, undercut, 3+2 setup that can't be split).
    /// Engine reads it to set [`crate::QuoteBreakdown::route_to_5_axis`].
    ///
    /// Wired now per the addendum's "land first-class handling
    /// already" instruction; the value will be populated by S269.
    pub requires_5_axis: bool,
    /// **Addendum 1.** First-class boolean for thin-wall presence.
    /// Drives the tight-tolerance labor bump
    /// ([`crate::THIN_WALL_TIGHT_TOL_BUMP`]).
    pub thin_wall_present: bool,
}

impl FeatureGraph {
    /// The schema version this build of the engine understands. The
    /// wrapper (S270) compares this against the value in the JSON
    /// and refuses unknown versions loud.
    pub const SCHEMA_VERSION: u32 = 1;
}
