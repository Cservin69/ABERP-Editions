//! S268 / PR-257 — `aberp-quote-engine`, the pure-function scoring
//! crate for the auto-quoting strand (design doc §10, ADR-0066).
//!
//! ## What this crate is
//!
//! A single deterministic, idempotent function: [`quote`]. Given
//! a [`FeatureGraph`] (extracted CAD geometry — produced by the future
//! Python extractor S269 and wrapped by S270), a catalogue snapshot
//! (materials, complexity rules, tolerance multipliers, stock
//! adjustments — populated by S266/S267's tables), a
//! [`QuotingParameters`] singleton, a `quantity`, and a target
//! [`ToleranceRange`], it returns a [`QuoteBreakdown`] — or a typed
//! [`QuoteError`] when the inputs are unsatisfiable.
//!
//! ## Invariants (NON-NEGOTIABLE)
//!
//! Per design doc §2 the crate is **pure**:
//!
//! - **No I/O** — no file reads, no HTTP, no DB.
//! - **No clock** — no `SystemTime::now`, no `Instant::now`.
//! - **No RNG** — no `rand`, no `thread_rng`.
//! - **No async**.
//! - **No global state**.
//!
//! These invariants are what make `feature_graph_hash` a meaningful
//! idempotency key (design §10): same inputs ⇒ byte-identical
//! [`QuoteBreakdown`] AND byte-identical [`QuoteBreakdown::reasoning_log`].
//! The reasoning log IS the trust signal per `[[trust-code-not-operator]]`
//! — an operator can read the log and see *every* multiplicative or
//! additive contribution that produced the final price. There is no
//! hidden state; the function is its own audit.
//!
//! ## What this crate is NOT (yet)
//!
//! - **No DB integration.** Catalogue snapshots are plain owned
//!   structs the caller (S271) reads from `quoting_materials`,
//!   `quoting_complexity_rules`, etc. and hands in.
//! - **No subprocess wrapper.** S270 ships `aberp-cad-extract-wrapper`;
//!   this crate receives the already-validated [`FeatureGraph`].
//! - **No SPA / HTTP / route surface.** Pure library.
//! - **No audit emission.** Audit happens at the wiring layer (S271)
//!   where the engine result is persisted as the frozen breakdown on
//!   the `quotes` row.
//!
//! ## Architecture: where this sits
//!
//! ```text
//!  storefront CAD upload
//!       │
//!       ▼
//!  aberp-cad-extract (Python, S269)
//!       │   stdout: feature-graph JSON
//!       ▼
//!  aberp-cad-extract-wrapper (Rust subprocess shim, S270)
//!       │   validated FeatureGraph
//!       ▼
//!  aberp-quote-engine ◄── catalogue snapshot (S266/S267)
//!       │   QuoteBreakdown
//!       ▼
//!  apps/aberp daemon (S271) — persist, audit, email indicative
//! ```
//!
//! ## Pushbacks applied vs the brief
//!
//! - **No `proptest` dep.** The brief named proptest as optional. We
//!   skipped it: no other workspace crate uses it, and the
//!   panic-resistance property is satisfied by a deterministic
//!   parameterised sweep test (see `tests/property.rs`). Pulling
//!   proptest in for one test adds ~20 transitive crates per CLAUDE.md
//!   rule 13.
//! - **Exotic-material detection is currently a hardcoded substring
//!   set** (`"inconel"`, `"titanium"`, case-insensitive). The
//!   `quoting_materials` schema (S266) deliberately did NOT ship an
//!   `is_exotic` column or `category` enum (S267 pushback #4 — exotic
//!   detection was named-deferred to a future cut). When that lands,
//!   replace [`is_exotic_material`] with a column read; the engine
//!   contract does not change. Constant lives at module level and is
//!   flagged in code with a `TODO(S271+)`.
//! - **Machining rate is on [`QuotingParameters`], not on the
//!   `quoting_machines` table.** v1 has no machine catalogue (design
//!   doc §7 corrected pushback B — the dispatch board the brief named
//!   is the shipping board, not a capacity board). One global
//!   `machining_rate_eur_per_minute` parameter is the honest v1
//!   posture; ADR-0066 names the eventual split into per-machine rates.
//! - **`FeatureGraph` is a v1 stub.** Schema versioned via
//!   [`FeatureGraph::SCHEMA_VERSION`]. S269 brings the canonical
//!   Python-produced JSON shape; the struct here mirrors the agreed
//!   field list (`requires_5_axis` + `thin_wall_present` first-class
//!   per [[aberp-quoting-design-addenda]] addendum 1, even though
//!   the extractor itself does not land until S269). The engine's
//!   handling of these two booleans is wired NOW so S269 only needs
//!   to populate them.
//!
//! ## Worked example (from the golden test)
//!
//! See `tests/golden.rs` — a fixed (FeatureGraph, snapshot, params,
//! qty=10, tolerance=Standard) is locked to a 4-decimal numeric
//! output. ANY algorithm change breaks that test and forces a
//! conscious update. This is the contract the wiring layer (S271)
//! and the SPA breakdown view (S272+) will be built against.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod breakdown;
mod calibration;
mod capacity;
mod catalogue;
mod engine;
mod error;
mod feature_graph;

pub use breakdown::QuoteBreakdown;
pub use calibration::{
    coefficient, CalibrationSample, CalibrationTable, CALIBRATION_DEFAULT_COEFFICIENT,
    CALIBRATION_MAX_COEFFICIENT, CALIBRATION_MIN_COEFFICIENT, CALIBRATION_MIN_SAMPLES,
    CALIBRATION_WINDOW,
};
pub use capacity::{
    lead_time_days, LeadTimeEstimate, MachineCapacity, MachineFamily, FALLBACK_BUFFER_PCT,
    FALLBACK_DAILY_HOURS,
};
pub use catalogue::{
    ComplexityRule, Material, QuotingParameters, StockAdjustment, StockStatus, ToleranceMultiplier,
};
pub use engine::{is_exotic_material, quote, quote_with_calibration, THIN_WALL_TIGHT_TOL_BUMP};
pub use error::QuoteError;
pub use feature_graph::{
    Feature, FeatureGraph, FeatureType, SizeBucket, StockForm, ToleranceRange,
};

/// Crate version stamp emitted on every breakdown so a quote PDF can
/// surface "priced by engine v0.0.0" (per the design doc's mention of
/// extractor-version stamping; same posture for the engine).
pub const ENGINE_VERSION: &str = env!("CARGO_PKG_VERSION");
