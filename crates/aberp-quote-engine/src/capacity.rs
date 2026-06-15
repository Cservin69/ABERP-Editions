//! S427 — capacity-aware lead-time model.
//!
//! A second pure function on the engine surface ([`lead_time_days`]).
//! Given the operator's `quoting_machines` master data (as
//! [`MachineCapacity`] rows), the current shop load in machining hours
//! grouped by [`MachineFamily`], and the new quote's projected hours by
//! family, it returns a [`LeadTimeEstimate`] — the calendar days the
//! shop needs to clear the most-loaded family the new quote touches.
//!
//! ## Why this lives in the (pure) engine crate
//!
//! It obeys the same five invariants as [`crate::quote`] — no I/O, no
//! clock, no RNG, no async, no global state. The wiring layer
//! (`apps/aberp`) does all the I/O: it loads the enabled machines, sums
//! the load from `quote_pricing_jobs`, and feeds the two maps in. The
//! division itself is deterministic arithmetic, so it belongs here next
//! to the pricing math rather than smeared into the daemon.
//!
//! ## The math (per the S427 brief)
//!
//! For each family the new quote touches:
//!
//! ```text
//! days_f = ceil( (existing_load_h[f] + new_quote_h[f]) / family_daily_capacity_h[f] )
//! ```
//!
//! where `family_daily_capacity_h[f] = Σ over enabled machines of family f
//! of `daily_hours_avail × (1 − buffer_pct/100)``. The reported
//! lead-time is the **max** across the touched families — the shop is
//! only as fast as its most-loaded touched resource.
//!
//! ## Empty-machine fallback ([[trust-code-not-operator]])
//!
//! If the operator has not entered any machines yet, the model falls
//! back to a single virtual "machine-shop" of [`FALLBACK_DAILY_HOURS`]
//! hours/day at [`FALLBACK_BUFFER_PCT`] buffer. With one shop every
//! family collapses onto it, so the estimate is
//! `ceil(total_existing + total_new / fallback_capacity)`. The caller
//! observes [`LeadTimeEstimate::used_fallback`] and emits
//! `QuotingMachinesEmptyFallback` once per server start so the operator
//! knows to fill in real machines — the estimate is never *withheld*,
//! only flagged.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Virtual-shop hours/day used when `quoting_machines` is empty.
pub const FALLBACK_DAILY_HOURS: f64 = 16.0;
/// Virtual-shop buffer percentage used when `quoting_machines` is empty.
pub const FALLBACK_BUFFER_PCT: f64 = 20.0;

/// Closed-vocab machine family. The auto-quote engine currently only
/// distinguishes 3-axis vs 5-axis mills (the sole signal the CAD
/// extractor produces — [`crate::FeatureGraph::requires_5_axis`]); the
/// remaining families exist so the operator can enter the real shop and
/// so a future extractor classification has a home without a schema
/// change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MachineFamily {
    /// 3-axis milling. Default family for a non-5-axis auto-quote.
    ThreeAxisMill,
    /// 5-axis milling. Chosen when the quote routes to 5-axis.
    FiveAxisMill,
    /// Wire EDM.
    WireEdm,
    /// Sinker / ram EDM.
    SinkerEdm,
    /// Turning / lathe.
    Lathe,
    /// Surface / cylindrical grinding.
    Grinder,
    /// Additive (metal or polymer).
    Additive,
    /// Anything not covered above.
    Other,
}

impl MachineFamily {
    /// Every variant, in declaration order — for SPA dropdowns and the
    /// CRUD validator's closed-vocab check.
    pub const ALL: [MachineFamily; 8] = [
        Self::ThreeAxisMill,
        Self::FiveAxisMill,
        Self::WireEdm,
        Self::SinkerEdm,
        Self::Lathe,
        Self::Grinder,
        Self::Additive,
        Self::Other,
    ];

    /// DB / wire storage string. Stable — `quoting_machines.family`
    /// round-trips through this.
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::ThreeAxisMill => "3-axis-mill",
            Self::FiveAxisMill => "5-axis-mill",
            Self::WireEdm => "wire-EDM",
            Self::SinkerEdm => "sinker-EDM",
            Self::Lathe => "lathe",
            Self::Grinder => "grinder",
            Self::Additive => "additive",
            Self::Other => "other",
        }
    }

    /// Round-trip parse. `None` on an unknown string — the caller errors
    /// loud rather than silently bucketing into `Other` (a silent
    /// fallback would mask schema drift per CLAUDE.md rule 12).
    pub fn from_db_str(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.as_db_str() == s)
    }

    /// The family an auto-quote routes to given the engine's only
    /// geometry signal. Documented as the single mapping point so when
    /// the extractor learns to classify lathes/EDM the change lands here.
    pub fn for_route(route_to_5_axis: bool) -> Self {
        if route_to_5_axis {
            Self::FiveAxisMill
        } else {
            Self::ThreeAxisMill
        }
    }
}

/// One enabled machine, reduced to exactly what the capacity math needs.
/// The wiring layer maps a `quoting_machines` row to this; archived /
/// disabled machines are filtered out before they reach here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MachineCapacity {
    /// Which family this machine serves.
    pub family: MachineFamily,
    /// Spindle hours available per calendar day before buffer.
    pub daily_hours_avail: f64,
    /// Planning buffer, percent. 20 ⇒ only 80% of `daily_hours_avail`
    /// is treated as schedulable.
    pub buffer_pct: f64,
}

impl MachineCapacity {
    /// Schedulable hours/day after the buffer. Defends against operator
    /// data that slipped past the CRUD validator: negative availability
    /// floors at 0, buffer clamps to `[0, 100)`.
    fn effective_daily_hours(&self) -> f64 {
        let buffer = self.buffer_pct.clamp(0.0, 100.0);
        self.daily_hours_avail.max(0.0) * (1.0 - buffer / 100.0)
    }
}

/// The result of [`lead_time_days`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LeadTimeEstimate {
    /// Calendar days the shop needs to clear the binding family.
    pub days: u32,
    /// The family that produced `days` (the most-loaded touched
    /// family). `None` only when the fallback virtual shop was used or
    /// the new quote carried no machining hours at all.
    pub binding_family: Option<MachineFamily>,
    /// True iff `machines` was empty and the virtual shop was used. The
    /// caller emits `QuotingMachinesEmptyFallback` once per server start
    /// when it sees this.
    pub used_fallback: bool,
}

/// Calendar days of lead-time for a new quote, capacity-aware.
///
/// * `machines` — the enabled `quoting_machines` rows. Empty ⇒ virtual
///   single-shop fallback.
/// * `existing_load_hours` — machining hours already committed/pending
///   in the shop, by family (the wiring layer sums `Posted` priced jobs
///   from the last 30 days).
/// * `new_quote_hours` — this quote's projected machining hours, by
///   family. The "touched" families are the keys with a positive value.
///
/// Deterministic: `BTreeMap` iteration order + a stable argmax tie-break
/// (first family in `MachineFamily` declaration order wins a tie) keep
/// the result byte-identical for identical inputs.
pub fn lead_time_days(
    machines: &[MachineCapacity],
    existing_load_hours: &BTreeMap<MachineFamily, f64>,
    new_quote_hours: &BTreeMap<MachineFamily, f64>,
) -> LeadTimeEstimate {
    let fallback_capacity = FALLBACK_DAILY_HOURS * (1.0 - FALLBACK_BUFFER_PCT / 100.0);

    // --- Empty-machine fallback: one virtual shop carries everything. ---
    if machines.is_empty() {
        let total: f64 = existing_load_hours
            .values()
            .chain(new_quote_hours.values())
            .sum();
        return LeadTimeEstimate {
            days: ceil_days(total.max(0.0), fallback_capacity),
            binding_family: None,
            used_fallback: true,
        };
    }

    // --- Per-family schedulable capacity from the real machines. ---
    let mut capacity: BTreeMap<MachineFamily, f64> = BTreeMap::new();
    for m in machines {
        *capacity.entry(m.family).or_insert(0.0) += m.effective_daily_hours();
    }

    // --- Days for each family the new quote touches; keep the max. ---
    let mut binding: Option<(MachineFamily, u32)> = None;
    for (&family, &new_h) in new_quote_hours {
        if new_h <= 0.0 {
            continue;
        }
        let load = existing_load_hours
            .get(&family)
            .copied()
            .unwrap_or(0.0)
            .max(0.0)
            + new_h;
        // A touched family with no enabled machine (or zeroed capacity)
        // is an operator data gap: route it through the fallback rate
        // rather than dividing by zero. Reported family stays accurate.
        let cap = capacity
            .get(&family)
            .copied()
            .filter(|c| *c > 0.0)
            .unwrap_or(fallback_capacity);
        let days = ceil_days(load, cap);
        if binding.is_none_or(|(_, d)| days > d) {
            binding = Some((family, days));
        }
    }

    match binding {
        Some((family, days)) => LeadTimeEstimate {
            days,
            binding_family: Some(family),
            used_fallback: false,
        },
        // New quote carried no machining hours at all.
        None => LeadTimeEstimate {
            days: 0,
            binding_family: None,
            used_fallback: false,
        },
    }
}

/// `ceil(load / capacity)` as days, saturating into `u32`. `capacity`
/// is guaranteed positive by the call sites; a non-positive `load`
/// yields 0.
fn ceil_days(load_hours: f64, capacity_hours_per_day: f64) -> u32 {
    if load_hours <= 0.0 || capacity_hours_per_day <= 0.0 {
        return 0;
    }
    (load_hours / capacity_hours_per_day).ceil() as u32
}
