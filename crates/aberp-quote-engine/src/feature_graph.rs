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

/// The raw-stock envelope the part is cut from — the shape the shop
/// actually buys and bills, and the volume roughing removes down to the
/// finished part (ADR-0094 Gap 1).
///
/// **Back-compat (load-bearing).** `#[serde(default)]` on
/// [`FeatureGraph::stock_form`] means a graph that omits the field —
/// every persisted blob and every v2-extractor output — loads as
/// [`StockForm::RectangularBlock`], whose volume is exactly today's
/// bounding-box block (`bx·by·bz`). So every existing golden,
/// determinism and property number stays byte-identical until the
/// extractor/operator supplies a round form (Gap 1 part B, S2). Mirrors
/// the `surface_area_mm2` (S418) / `calibration_coefficient` (S429)
/// serde-default precedent.
///
/// Each variant carries its own dimensions: the pure engine evaluates a
/// volume formula and never infers a spin axis from the bounding box —
/// axis inference is the extractor's job, consistent with
/// [`Feature::representative_size_mm`] ("the engine does not
/// second-guess").
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StockForm {
    /// A rectangular block sized to the bounding box — **today's
    /// model**, reproduced byte-for-byte. Billed/roughed volume is
    /// `bounding_box_mm[0] * [1] * [2]`. The default.
    #[default]
    RectangularBlock,
    /// A solid round bar (turned stock). Billed/roughed volume is the
    /// cylinder `π/4 · diameter_mm² · length_mm` — `0.7854×` the
    /// bounding-box block, so a turned part bills ~21.5 % less material
    /// (and roughs only what a near-net bar actually removes).
    RoundBar {
        /// Bar diameter in millimetres.
        diameter_mm: f64,
        /// Bar cut-off length in millimetres.
        length_mm: f64,
    },
    /// A hollow tube / ring blank. Billed/roughed volume is the annulus
    /// `π/4 · (od_mm² − id_mm²) · length_mm`. The bore is **never
    /// bought**, so it is neither billed as material nor "roughed away"
    /// — the correct model for a ring-gear blank.
    Tube {
        /// Outside diameter in millimetres.
        od_mm: f64,
        /// Inside (bore) diameter in millimetres.
        id_mm: f64,
        /// Tube length in millimetres.
        length_mm: f64,
    },
}

/// **ADR-0094 Gap 3.** The kind of gear teeth to generate. Sets the
/// admissible process family: external spur/helical teeth are hobbed or
/// (in-cycle on a turn-mill) power-skived; internal ring teeth have no
/// hob/skive access and are shaped, broached, or wire-EDM'd.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GearKind {
    /// External spur or helical teeth on the outside of the blank.
    ExternalSpurHelical,
    /// Internal ring-gear teeth (cut from the bore).
    InternalRing,
}

impl GearKind {
    /// DB / wire storage string — matches the future `quoting_gear_ops.kind`.
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::ExternalSpurHelical => "external_spur_helical",
            Self::InternalRing => "internal_ring",
        }
    }
}

/// **ADR-0094 Gap 3.** The tooth-generation process. [`GearProcess::Auto`]
/// lets the engine pick deterministically from `kind` + routed machine
/// family + AGMA quality (see [`crate::select_gear_process`]); the explicit
/// variants force a process (operator override, reasoning-logged).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GearProcess {
    /// Engine selects the process deterministically.
    Auto,
    /// Hobbing — external teeth on a dedicated hobber (standalone op).
    Hob,
    /// Power-skiving — external teeth in-cycle on a turn-mill (the part is
    /// already on the spindle: no second op, no refixture ⇒ cheap).
    PowerSkive,
    /// Gear shaping — internal ring teeth (and external where blocked).
    Shape,
    /// Broaching — internal teeth: high setup, fast per part at volume.
    Broach,
    /// Wire-EDM — internal teeth at the tightest AGMA classes; slow + dear.
    WireEdm,
}

impl GearProcess {
    /// DB / wire storage string — matches [`crate::GearProcessRate::process`].
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Hob => "hob",
            Self::PowerSkive => "power_skive",
            Self::Shape => "shape",
            Self::Broach => "broach",
            Self::WireEdm => "wire_edm",
        }
    }
}

/// **ADR-0094 Gap 3.** One gear's teeth to cut, modelled as a costed
/// operation — the volume/feature model cannot see teeth. Carries the
/// parameters that drive generation cost (module, tooth count, face width,
/// AGMA quality) and the process selection. Defaulted-empty on
/// [`FeatureGraph::gears`], so a part with no gears prices exactly as today.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GearOp {
    /// External spur/helical vs internal ring — sets the admissible process.
    pub kind: GearKind,
    /// Module `m` (mm) — the tooth-size unit. Bigger module ⇒ more metal per
    /// tooth ⇒ slower generation (time scales by `module_mm^module_exponent`).
    pub module_mm: f64,
    /// Tooth count `z`. Generation time scales linearly with teeth.
    pub teeth: u32,
    /// Face width `b` (mm) — the axial length of the teeth.
    pub face_width_mm: f64,
    /// Target AGMA quality class (higher = tighter = slower). Drives the
    /// quality factor and, for internal rings under `Auto`, the
    /// shape→wire-EDM escalation above [`crate::GEAR_INTERNAL_WIRE_EDM_AGMA`].
    pub quality_agma: u8,
    /// The process to use; [`GearProcess::Auto`] ⇒ engine selects.
    pub process: GearProcess,
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
    /// XYZ bounding-box extent in millimetres. S418: now a first-class
    /// scoring input — material is billed on the stock block
    /// `bbox × (1 + scrap_factor)` (report §6.4) and roughing time is
    /// driven by `stock − part` removed volume (report §5.1).
    pub bounding_box_mm: [f64; 3],
    /// Solid volume in mm³ (after extraction, before scrap).
    pub volume_mm3: f64,
    /// Total surface area in mm² (schema v2, S418). Drives the
    /// finishing-pass machining time (report §5.2). STL: Σ ½‖(v1−v0)×
    /// (v2−v0)‖ over triangles; STEP: `BRepGProp::SurfaceProperties`.
    /// `#[serde(default)]` keeps the engine fail-soft on a v1 graph or
    /// a corrupt extractor: a non-positive value falls back to the
    /// bounding-box surface area `2(xy+yz+zx)` inside the engine
    /// (report §5.4) rather than zeroing finishing time.
    #[serde(default)]
    pub surface_area_mm2: f64,
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
    /// **ADR-0094 Gap 1.** The raw-stock envelope this part is cut from.
    /// Drives BOTH the billed material volume AND the roughing
    /// removed-volume `(stock − part)`. `#[serde(default)]` (mirroring
    /// `surface_area_mm2` / `calibration_coefficient`) makes a graph that
    /// omits it load as [`StockForm::RectangularBlock`] = today's
    /// bounding-box block, so existing goldens stay byte-identical. The
    /// extractor (S269) / operator (S2 wiring) populates it for turned
    /// and tubular parts.
    #[serde(default)]
    pub stock_form: StockForm,
    /// **ADR-0094 Gap 3.** Per-gear tooth-generation operations. Defaulted
    /// empty (`#[serde(default)]`, mirroring `stock_form` / `surface_area_mm2`)
    /// so a graph that omits it — every persisted blob, every pre-S5 extractor
    /// output — prices with NO gear cost and byte-identical output. The
    /// extractor (S269) / operator (S6 wiring) populates it for geared parts.
    #[serde(default)]
    pub gears: Vec<GearOp>,
}

impl FeatureGraph {
    /// The schema version this build of the engine understands. The
    /// wrapper (S270) compares this against the value in the JSON
    /// and refuses unknown versions loud. **v2 (S418)** added
    /// `surface_area_mm2`; the Python `SCHEMA_VERSION` and the
    /// wrapper's `EXPECTED_SCHEMA_VERSION` bump in lockstep. **v3
    /// (ADR-0094 Gap 1)** adds the defaulted `stock_form`; a v2 graph
    /// (no `stock_form`) still loads — it defaults to `RectangularBlock`
    /// — and the version guard accepts any `schema_version ≤ 3`. The
    /// extractor's lockstep bump to v3 lands with S269 (ADR-0094 Q3).
    /// **v4 (ADR-0094 Gap 3)** adds the defaulted `gears` vector; a v2/v3
    /// graph (no `gears`) still loads — it defaults to empty ⇒ zero gear cost
    /// ⇒ today's price — and the guard accepts any `schema_version <= 4`. The
    /// extractor's lockstep bump to v4 lands with S269 (ADR-0094 Q3).
    pub const SCHEMA_VERSION: u32 = 4;
}
