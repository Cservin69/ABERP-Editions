//! Closed-vocab enums for the Dispatch board per ADR-0064 §1.
//!
//! Two enums:
//!
//! - [`DispatchState`] — the dispatch lifecycle vocab. `Drafted` is the
//!   initial state; `Shipped` is the terminal-success state (carries
//!   carrier + tracking + spawned-invoice metadata); `Cancelled` is the
//!   terminal-not-shipped state (only valid FROM `Drafted` per ADR-0064
//!   §1 table).
//!
//! - [`CarrierKind`] — small Hungarian-market carrier set + the two
//!   "no carrier" sentinels per ADR-0064 §1. Closed vocab so a POST
//!   body parses unambiguously; new carriers go in by enum extension
//!   per the project's closed-vocab discipline (currency, payment
//!   method, NAV unit-of-measure all closed-vocab — see ADR-0064
//!   §"Consequences").

use serde::{Deserialize, Serialize};

/// Dispatch lifecycle vocab per ADR-0064 §1.
///
/// ```text
/// Drafted → Shipped     (mark_shipped — emits stock movement +
///                        spawns invoice draft in the SAME tx)
/// Drafted → Cancelled   (operator gives up before shipping; no
///                        inventory impact, no audit beyond DispatchCreated)
/// ```
///
/// `Shipped` and `Cancelled` are terminal — every transition out of
/// them is refused at the route boundary per [[trust-code-not-operator]].
/// Pinned by [`crate::state::next_dispatch_state`]; no DB CHECK per
/// [[no-sql-specific]].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchState {
    Drafted,
    Shipped,
    Cancelled,
}

impl DispatchState {
    /// On-disk / wire string per ADR-0064 §1 table.
    pub fn as_str(&self) -> &'static str {
        match self {
            DispatchState::Drafted => "drafted",
            DispatchState::Shipped => "shipped",
            DispatchState::Cancelled => "cancelled",
        }
    }

    /// Parse from the on-disk / wire string. Errors loud per
    /// CLAUDE.md rule 12.
    pub fn from_storage_str(s: &str) -> Result<Self, &'static str> {
        match s {
            "drafted" => Ok(DispatchState::Drafted),
            "shipped" => Ok(DispatchState::Shipped),
            "cancelled" => Ok(DispatchState::Cancelled),
            _ => Err("unknown DispatchState storage string"),
        }
    }

    /// `true` once the dispatch has reached a terminal state (Shipped
    /// or Cancelled). The DispatchList's `Drafted` filter is the SPA
    /// default per ADR-0064 §7.
    pub fn is_terminal(&self) -> bool {
        matches!(self, DispatchState::Shipped | DispatchState::Cancelled)
    }
}

/// Carrier-of-shipment vocab per ADR-0064 §1. HU-market-focused; two
/// non-carrier sentinels (`SelfDelivery`, `CustomerPickup`) cover the
/// in-person flows that need NO carrier picker. Free-text bypass is
/// deliberately refused — operators who use a sixth Hungarian carrier
/// (e.g. Sprinter) submit a PR to extend the enum, same posture as
/// every other ABERP closed-vocab surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CarrierKind {
    SelfDelivery,
    CustomerPickup,
    MagyarPosta,
    Gls,
    Dpd,
    Foxpost,
    Other,
}

impl CarrierKind {
    /// On-disk / wire string per ADR-0064 §1 table.
    pub fn as_str(&self) -> &'static str {
        match self {
            CarrierKind::SelfDelivery => "self_delivery",
            CarrierKind::CustomerPickup => "customer_pickup",
            CarrierKind::MagyarPosta => "magyar_posta",
            CarrierKind::Gls => "gls",
            CarrierKind::Dpd => "dpd",
            CarrierKind::Foxpost => "foxpost",
            CarrierKind::Other => "other",
        }
    }

    /// Parse from the on-disk / wire string. Errors loud per
    /// CLAUDE.md rule 12 + ADR-0064 §"Invariants pinned" #8 —
    /// `CarrierKind` is closed-vocab; free text MUST be refused at the
    /// boundary.
    pub fn from_storage_str(s: &str) -> Result<Self, &'static str> {
        match s {
            "self_delivery" => Ok(CarrierKind::SelfDelivery),
            "customer_pickup" => Ok(CarrierKind::CustomerPickup),
            "magyar_posta" => Ok(CarrierKind::MagyarPosta),
            "gls" => Ok(CarrierKind::Gls),
            "dpd" => Ok(CarrierKind::Dpd),
            "foxpost" => Ok(CarrierKind::Foxpost),
            "other" => Ok(CarrierKind::Other),
            _ => Err("unknown CarrierKind storage string"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_state_round_trip_for_every_variant() {
        let variants = [
            DispatchState::Drafted,
            DispatchState::Shipped,
            DispatchState::Cancelled,
        ];
        for v in variants {
            let s = v.as_str();
            let back = DispatchState::from_storage_str(s).unwrap_or_else(|e| panic!("{s:?}: {e}"));
            assert_eq!(back, v);
        }
    }

    #[test]
    fn dispatch_state_rejects_unknown_string() {
        assert!(DispatchState::from_storage_str("not_a_state").is_err());
        assert!(DispatchState::from_storage_str("").is_err());
    }

    /// Pin the wire shape — snake_case tokens MUST match the storage
    /// strings byte-for-byte. Mirrors the
    /// `aberp_qa::types::tests::qa_state_serde_matches_storage_string`
    /// posture: a future contributor swapping `rename_all` would
    /// silently desync the two surfaces.
    #[test]
    fn dispatch_state_serde_matches_storage_string() {
        for v in [
            DispatchState::Drafted,
            DispatchState::Shipped,
            DispatchState::Cancelled,
        ] {
            let json = serde_json::to_string(&v).unwrap();
            let inside = json.trim_matches('"');
            assert_eq!(inside, v.as_str());
        }
    }

    #[test]
    fn is_terminal_returns_true_for_shipped_and_cancelled_only() {
        assert!(!DispatchState::Drafted.is_terminal());
        assert!(DispatchState::Shipped.is_terminal());
        assert!(DispatchState::Cancelled.is_terminal());
    }

    #[test]
    fn carrier_kind_round_trip_for_every_variant() {
        let variants = [
            CarrierKind::SelfDelivery,
            CarrierKind::CustomerPickup,
            CarrierKind::MagyarPosta,
            CarrierKind::Gls,
            CarrierKind::Dpd,
            CarrierKind::Foxpost,
            CarrierKind::Other,
        ];
        for v in variants {
            let s = v.as_str();
            let back = CarrierKind::from_storage_str(s).unwrap_or_else(|e| panic!("{s:?}: {e}"));
            assert_eq!(back, v);
        }
    }

    /// ADR-0064 §"Invariants pinned" #8 — `CarrierKind` is closed-vocab;
    /// free text MUST be refused at the boundary. Pinned here AND at
    /// the route layer (POST /dispatches/:id/ship parses `carrier_kind`
    /// via this exact function).
    #[test]
    fn carrier_kind_rejects_free_text_at_route_boundary() {
        assert!(CarrierKind::from_storage_str("sprinter").is_err());
        assert!(CarrierKind::from_storage_str("UPS").is_err());
        assert!(CarrierKind::from_storage_str("").is_err());
        assert!(CarrierKind::from_storage_str("FedEx").is_err());
    }

    #[test]
    fn carrier_kind_serde_matches_storage_string() {
        for v in [
            CarrierKind::SelfDelivery,
            CarrierKind::CustomerPickup,
            CarrierKind::MagyarPosta,
            CarrierKind::Gls,
            CarrierKind::Dpd,
            CarrierKind::Foxpost,
            CarrierKind::Other,
        ] {
            let json = serde_json::to_string(&v).unwrap();
            let inside = json.trim_matches('"');
            assert_eq!(inside, v.as_str());
        }
    }
}
