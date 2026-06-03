//! Closed-vocab enums for the inventory module per ADR-0061 §2.
//!
//! Two enums:
//!
//! - [`MovementReason`] — WHY the movement happened. Closed vocab; the
//!   set is pinned because the reason-sign matrix in
//!   [`crate::repository::record_movement`] gates writes. Adding a
//!   reason is a coordinated edit across the storage-string round-trip
//!   pin, the reason-sign matrix, and any downstream UI dropdown.
//!
//! - [`MovementRefKind`] — WHAT entity caused the movement (so the
//!   operator can trace stock back to its root cause: which work
//!   order, which dispatch, which invoice). NULL for operator-typed
//!   manual adjustments, where `ref_kind = Manual` is the explicit
//!   sentinel that paired with `ref_id = NULL` per ADR-0061 §1.
//!
//! Both enums round-trip through the storage strings named in
//! ADR-0061 §2. The unit tests in this file pin every variant.
//!
//! ## Why these are not built from serde derives
//!
//! The DB layer stores movement reasons as plain VARCHAR (no DB CHECK
//! per `[[no-sql-specific]]`). The round-trip pair is exercised at
//! every read site; pairing `as_str` + `from_storage_str` mirrors the
//! `aberp_audit_ledger::EventKind` F12 ritual so a future contributor
//! adding a variant has to touch all three sites at once. A naked
//! `#[derive(Serialize, Deserialize)]` over a `rename_all` would
//! collapse the as_str surface into a single source of truth — but
//! then the SQL `WHERE reason = 'bom_consumption'` queries scattered
//! across the binary would silently break on a rename. The two-surface
//! pin is the load-bearing posture.

use serde::{Deserialize, Serialize};

/// Why a stock movement happened. Per ADR-0061 §2 + the reason-sign
/// matrix in §5: every variant except [`MovementReason::Adjustment`]
/// has a required sign on its `qty_delta`. The matrix is enforced at
/// the route boundary by [`MovementReason::required_sign`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MovementReason {
    /// Inbound stock (manual GRN today; future PO-receive). Required
    /// sign: positive.
    Receipt,
    /// ADR-0062 Work Order Release consumes a BOM line. Required
    /// sign: negative.
    BomConsumption,
    /// ADR-0062 Work Order Complete produces a finished good.
    /// Required sign: positive.
    WoCompletion,
    /// Manual operator stock-take correction (loud + reason
    /// required). Any sign — Adjustment is the ONLY path that can
    /// drive `stock_qty` negative, and only with explicit operator
    /// intent per ADR-0061 §5.
    Adjustment,
    /// ADR-0064 Dispatch shipping. Required sign: negative.
    Dispatch,
    /// ADR-0063 QA Fail-Dispose. Required sign: negative.
    Scrap,
}

/// Required sign of `qty_delta` for a given reason per the matrix.
/// `None` means any sign is acceptable (Adjustment).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequiredSign {
    /// `qty_delta > 0` — refuse zero or negative at the route boundary.
    Positive,
    /// `qty_delta < 0` — refuse zero or positive at the route
    /// boundary.
    Negative,
    /// Any non-zero sign accepted; zero is still refused (a zero
    /// movement is structurally meaningless — an audit-trail row that
    /// describes no state change).
    Any,
}

impl MovementReason {
    /// Render to the on-disk / wire-shape string per ADR-0061 §2.
    /// Paired with [`MovementReason::from_storage_str`].
    pub fn as_str(&self) -> &'static str {
        match self {
            MovementReason::Receipt => "receipt",
            MovementReason::BomConsumption => "bom_consumption",
            MovementReason::WoCompletion => "wo_completion",
            MovementReason::Adjustment => "adjustment",
            MovementReason::Dispatch => "dispatch",
            MovementReason::Scrap => "scrap",
        }
    }

    /// Parse from the on-disk / wire-shape string. Errors loud on
    /// anything outside the closed vocab — silent fallback would
    /// mask schema drift per CLAUDE.md rule 12.
    pub fn from_storage_str(s: &str) -> Result<Self, &'static str> {
        match s {
            "receipt" => Ok(MovementReason::Receipt),
            "bom_consumption" => Ok(MovementReason::BomConsumption),
            "wo_completion" => Ok(MovementReason::WoCompletion),
            "adjustment" => Ok(MovementReason::Adjustment),
            "dispatch" => Ok(MovementReason::Dispatch),
            "scrap" => Ok(MovementReason::Scrap),
            _ => Err("unknown MovementReason storage string"),
        }
    }

    /// The reason-sign matrix per ADR-0061 §5.
    pub fn required_sign(&self) -> RequiredSign {
        match self {
            MovementReason::Receipt | MovementReason::WoCompletion => RequiredSign::Positive,
            MovementReason::BomConsumption | MovementReason::Dispatch | MovementReason::Scrap => {
                RequiredSign::Negative
            }
            MovementReason::Adjustment => RequiredSign::Any,
        }
    }
}

/// What entity caused the movement. Per ADR-0061 §2: trace-back
/// labels that pair with `ref_id` (the entity's `<prefix>_<ULID>`).
/// [`MovementRefKind::Manual`] is the sentinel for operator-typed
/// movements, which carry `ref_id = NULL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MovementRefKind {
    /// ADR-0062 work order — `ref_id` is `wo_<ULID>`.
    WorkOrder,
    /// ADR-0063 QA inspection — `ref_id` is `qa_<ULID>`. Set by
    /// QA Dispose movements that emit one Scrap row.
    QaInspection,
    /// ADR-0064 dispatch — `ref_id` is `dsp_<ULID>`.
    Dispatch,
    /// Reserved for future inbound-stock-from-AP-invoice — not emitted
    /// today (the AP module records the invoice; no auto-Receipt
    /// movement yet). Future bridge would set `ref_id` to the
    /// `apinv_<ULID>` of the source AP row.
    Invoice,
    /// Operator-typed manual movement; `ref_id` is NULL. The SPA form
    /// never exposes ref_kind / ref_id to the operator — see
    /// ADR-0061 §6.
    Manual,
}

impl MovementRefKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            MovementRefKind::WorkOrder => "work_order",
            MovementRefKind::QaInspection => "qa_inspection",
            MovementRefKind::Dispatch => "dispatch",
            MovementRefKind::Invoice => "invoice",
            MovementRefKind::Manual => "manual",
        }
    }

    pub fn from_storage_str(s: &str) -> Result<Self, &'static str> {
        match s {
            "work_order" => Ok(MovementRefKind::WorkOrder),
            "qa_inspection" => Ok(MovementRefKind::QaInspection),
            "dispatch" => Ok(MovementRefKind::Dispatch),
            "invoice" => Ok(MovementRefKind::Invoice),
            "manual" => Ok(MovementRefKind::Manual),
            _ => Err("unknown MovementRefKind storage string"),
        }
    }
}

/// Mock-friendly actor enum per ADR-0061 §"Cross-cutting decisions" #1
/// and ADR-0060's framing. One [`crate::repository::record_movement`]
/// signature for the SPA route today AND the future adapter consumer
/// (a barcode scan that triggers a Receipt). The audit-ledger entry
/// records WHO drove the mutation; the handler does not care.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActorKind {
    /// A logged-in operator (the SPA route's `operator_login`).
    SpaOperator {
        /// Operator login string (matches the `Actor::from_local_cli`
        /// shape every other ABERP write surface uses).
        operator_login: String,
    },
    /// An adapter triggered the movement (e.g. a barcode scan that
    /// caused a Receipt). Carries the adapter's `name` so the
    /// audit-evidence trail can disambiguate per-adapter writes.
    Adapter {
        /// Adapter name (e.g. `barcode-scanner-cell-A`).
        adapter_name: String,
    },
    /// System-initiated movement (e.g. a future BomConsumption emitted
    /// by the Work Order Release handler). No human in the loop.
    System,
}

impl ActorKind {
    /// Render to the attribution string stored on
    /// `stock_movements.operator`. The audit-ledger's `Actor` JSON
    /// shape carries the structured form; this rendering is the
    /// human-readable label the SPA shows in the ledger view.
    pub fn as_operator_string(&self) -> String {
        match self {
            ActorKind::SpaOperator { operator_login } => operator_login.clone(),
            ActorKind::Adapter { adapter_name } => format!("adapter:{}", adapter_name),
            ActorKind::System => "system".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip every MovementReason variant per the
    /// `aberp_audit_ledger::EventKind` F12-ritual posture. Hand-listed
    /// (not strum-iterated) so a new variant added without an arm
    /// fails this test loudly.
    #[test]
    fn movement_reason_round_trip_for_every_variant() {
        let variants = [
            MovementReason::Receipt,
            MovementReason::BomConsumption,
            MovementReason::WoCompletion,
            MovementReason::Adjustment,
            MovementReason::Dispatch,
            MovementReason::Scrap,
        ];
        for v in variants {
            let s = v.as_str();
            let back = MovementReason::from_storage_str(s).unwrap_or_else(|e| panic!("{s:?}: {e}"));
            assert_eq!(back, v);
        }
    }

    #[test]
    fn movement_reason_rejects_unknown_string() {
        assert!(MovementReason::from_storage_str("not_a_real_reason").is_err());
        assert!(MovementReason::from_storage_str("").is_err());
    }

    /// Pin the wire shape too — the SPA reads MovementReason as JSON
    /// and the lower-case snake-case tokens must match the storage
    /// strings byte-for-byte. A future contributor swapping
    /// `rename_all` would silently desync the two surfaces; this pin
    /// fires before any production reader gets confused.
    #[test]
    fn movement_reason_serde_matches_storage_string() {
        for v in [
            MovementReason::Receipt,
            MovementReason::BomConsumption,
            MovementReason::WoCompletion,
            MovementReason::Adjustment,
            MovementReason::Dispatch,
            MovementReason::Scrap,
        ] {
            let json = serde_json::to_string(&v).unwrap();
            // Strip the surrounding quotes for the comparison.
            let inside = json.trim_matches('"');
            assert_eq!(inside, v.as_str(), "wire JSON must match storage string");
        }
    }

    /// Reason-sign matrix per ADR-0061 §5 — the load-bearing pin.
    /// If a future contributor relaxes this matrix without updating
    /// the route boundary, this test surfaces the divergence.
    #[test]
    fn reason_sign_matrix_matches_adr_0061_section_5() {
        assert_eq!(
            MovementReason::Receipt.required_sign(),
            RequiredSign::Positive
        );
        assert_eq!(
            MovementReason::WoCompletion.required_sign(),
            RequiredSign::Positive
        );
        assert_eq!(
            MovementReason::BomConsumption.required_sign(),
            RequiredSign::Negative
        );
        assert_eq!(
            MovementReason::Dispatch.required_sign(),
            RequiredSign::Negative
        );
        assert_eq!(
            MovementReason::Scrap.required_sign(),
            RequiredSign::Negative
        );
        assert_eq!(
            MovementReason::Adjustment.required_sign(),
            RequiredSign::Any
        );
    }

    /// Round-trip every MovementRefKind variant.
    #[test]
    fn movement_ref_kind_round_trip_for_every_variant() {
        let variants = [
            MovementRefKind::WorkOrder,
            MovementRefKind::QaInspection,
            MovementRefKind::Dispatch,
            MovementRefKind::Invoice,
            MovementRefKind::Manual,
        ];
        for v in variants {
            let s = v.as_str();
            let back =
                MovementRefKind::from_storage_str(s).unwrap_or_else(|e| panic!("{s:?}: {e}"));
            assert_eq!(back, v);
        }
    }

    #[test]
    fn movement_ref_kind_rejects_unknown_string() {
        assert!(MovementRefKind::from_storage_str("not_a_real_ref").is_err());
        assert!(MovementRefKind::from_storage_str("").is_err());
    }

    #[test]
    fn actor_kind_as_operator_string_disambiguates_sources() {
        let spa = ActorKind::SpaOperator {
            operator_login: "ervin".to_string(),
        };
        let adapter = ActorKind::Adapter {
            adapter_name: "barcode-scanner-cell-A".to_string(),
        };
        let system = ActorKind::System;
        assert_eq!(spa.as_operator_string(), "ervin");
        assert_eq!(
            adapter.as_operator_string(),
            "adapter:barcode-scanner-cell-A"
        );
        assert_eq!(system.as_operator_string(), "system");
    }
}
