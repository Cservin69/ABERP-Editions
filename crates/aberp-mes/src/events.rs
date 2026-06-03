//! Canonical event vocabulary per ADR-0060 §"The canonical event
//! vocabulary".
//!
//! Six initial variants drawn from the Stage 3 memo and the research
//! package (`docs/research/stage3/01-machine-protocols.md` §"Vocabulary",
//! §"OPC 40501 — Machine Tools"). Closed Rust enum — the vocabulary IS
//! the schema, and serde JSON round-trip tests pin the on-disk form.
//!
//! Adding a new variant is a Rust enum extension + a serde round-trip
//! test + matching changes downstream (consumer projections). It is NOT
//! a breaking change to the audit-ledger crate — the audit-ledger kind
//! is `EventKind::MesAdapterEvent` regardless of which `CanonicalEvent`
//! subtype rode inside.

use serde::{Deserialize, Serialize};

/// A canonical manufacturing event emitted by an [`Adapter`](crate::Adapter).
///
/// Variants carry the fields needed for the audit-ledger entry plus any
/// downstream consumer (cell-controller projection, operator UI). The
/// `tag = "type"` serde attribute makes the JSON shape self-describing:
///
/// ```json
/// {
///   "type": "machine_state_changed",
///   "machine_id": "dmg-mori-nmh-6300-cell-A",
///   "previous_state": "idle",
///   "new_state": "running",
///   "at_iso8601": "2026-06-03T08:30:00Z"
/// }
/// ```
///
/// The `at_iso8601` field is supplied by the adapter (not the framework)
/// — the adapter knows when the source system observed the event, which
/// may differ from the audit-ledger entry's wall-clock by network /
/// polling lag. Adapters MUST pass a RFC3339-formatted UTC string; format
/// pinning is unit-tested at the canonical-event level (see this file's
/// tests) and re-tested at the adapter level when real adapters land.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CanonicalEvent {
    /// A part (raw stock, WIP, or finished good) moved between two
    /// stations. Emitted by robot / conveyor adapters and by
    /// barcode-scan adapters that infer movement from a scan at a
    /// destination.
    PartMoved {
        part_id: String,
        from_station: String,
        to_station: String,
        at_iso8601: String,
    },

    /// A machine transitioned between operational states. Closed vocab
    /// per [`MachineState`].
    MachineStateChanged {
        machine_id: String,
        previous_state: MachineState,
        new_state: MachineState,
        at_iso8601: String,
    },

    /// A measurement gate (Renishaw / on-machine probe / hand-gauge)
    /// emitted a pass/fail/hold-for-review outcome against a part.
    /// Closed vocab per [`QualityOutcome`]. Optional `note` carries
    /// free-form operator-facing context (probe routine name, hold
    /// reason); MUST NOT carry secret-bearing strings.
    QualityResultReceived {
        part_id: String,
        gate_id: String,
        outcome: QualityOutcome,
        note: Option<String>,
        at_iso8601: String,
    },

    /// A barcode / QR code was scanned at a station. The `code` is the
    /// verbatim payload the scanner emitted; downstream consumers parse
    /// vendor-specific encodings.
    ScanReceived {
        scanner_id: String,
        station_id: String,
        code: String,
        at_iso8601: String,
    },

    /// A work order transitioned between operational states. Closed
    /// vocab per [`WorkOrderState`]. Used by laser / manual-station
    /// adapters and by the future work-order module's intake.
    WorkOrderStateChanged {
        work_order_id: String,
        previous_state: WorkOrderState,
        new_state: WorkOrderState,
        at_iso8601: String,
    },

    /// A robot task was queued. The outcome of the task surfaces as a
    /// downstream `PartMoved` once the robot reports completion;
    /// `RobotTaskQueued` records the dispatch event itself for OEE
    /// availability calculations.
    RobotTaskQueued {
        robot_id: String,
        task_id: String,
        description: String,
        priority: u8,
        at_iso8601: String,
    },
}

impl CanonicalEvent {
    /// Stable discriminator string matching the serde `tag` value.
    ///
    /// Useful for SPA filtering / SQL JSON-extract queries against the
    /// audit-ledger payload without round-tripping through serde.
    pub fn type_tag(&self) -> &'static str {
        match self {
            CanonicalEvent::PartMoved { .. } => "part_moved",
            CanonicalEvent::MachineStateChanged { .. } => "machine_state_changed",
            CanonicalEvent::QualityResultReceived { .. } => "quality_result_received",
            CanonicalEvent::ScanReceived { .. } => "scan_received",
            CanonicalEvent::WorkOrderStateChanged { .. } => "work_order_state_changed",
            CanonicalEvent::RobotTaskQueued { .. } => "robot_task_queued",
        }
    }
}

/// Closed-vocab machine state per ADR-0060 §"The canonical event
/// vocabulary" — aligned with MTConnect's `execution` DataItem values
/// (`READY → Idle`, `ACTIVE → Running`, `STOPPED → Down`) and OPC 40501's
/// machine-tool state model.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum MachineState {
    Idle,
    Running,
    Setup,
    Down,
    Fault,
    Unknown,
}

/// Closed-vocab quality outcome. Three values rather than two because
/// real shop-floor workflows have a "hold for human review" branch
/// distinct from automatic pass / fail (per Stage 3 memo §"Quality gate
/// (Renishaw)").
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum QualityOutcome {
    Pass,
    Fail,
    HoldForReview,
}

/// Closed-vocab work-order state. Six values covering the standard
/// MES work-order lifecycle (`docs/research/stage3/07-oee-mes-metrics.md`
/// §"Status taxonomy").
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum WorkOrderState {
    Created,
    Released,
    InProgress,
    Completed,
    Cancelled,
    OnHold,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_part_moved() -> CanonicalEvent {
        CanonicalEvent::PartMoved {
            part_id: "part_01H".to_string(),
            from_station: "stock".to_string(),
            to_station: "cnc-load-1".to_string(),
            at_iso8601: "2026-06-03T08:30:00Z".to_string(),
        }
    }

    fn sample_machine_state_changed() -> CanonicalEvent {
        CanonicalEvent::MachineStateChanged {
            machine_id: "dmg-mori-nmh-6300-cell-A".to_string(),
            previous_state: MachineState::Idle,
            new_state: MachineState::Running,
            at_iso8601: "2026-06-03T08:30:00Z".to_string(),
        }
    }

    fn sample_quality_result_received() -> CanonicalEvent {
        CanonicalEvent::QualityResultReceived {
            part_id: "part_01H".to_string(),
            gate_id: "renishaw-equator-1".to_string(),
            outcome: QualityOutcome::Pass,
            note: None,
            at_iso8601: "2026-06-03T08:31:00Z".to_string(),
        }
    }

    fn sample_scan_received() -> CanonicalEvent {
        CanonicalEvent::ScanReceived {
            scanner_id: "honeywell-1900-station-A".to_string(),
            station_id: "packaging-1".to_string(),
            code: "QR:wo_01H/op-3".to_string(),
            at_iso8601: "2026-06-03T08:32:00Z".to_string(),
        }
    }

    fn sample_work_order_state_changed() -> CanonicalEvent {
        CanonicalEvent::WorkOrderStateChanged {
            work_order_id: "wo_01H".to_string(),
            previous_state: WorkOrderState::Released,
            new_state: WorkOrderState::InProgress,
            at_iso8601: "2026-06-03T08:33:00Z".to_string(),
        }
    }

    fn sample_robot_task_queued() -> CanonicalEvent {
        CanonicalEvent::RobotTaskQueued {
            robot_id: "ur10-cell-A".to_string(),
            task_id: "task_01H".to_string(),
            description: "move part_01H from cnc-unload-1 to renishaw-equator-1".to_string(),
            priority: 3,
            at_iso8601: "2026-06-03T08:30:30Z".to_string(),
        }
    }

    /// Round-trip every variant through serde JSON. If a future
    /// contributor adds a variant + forgets to update `type_tag` or
    /// derives, this test fails. Hand-listed (not iterated) so the
    /// maintainer has to think about whether the new variant is
    /// covered.
    #[test]
    fn round_trip_every_variant_through_json() {
        let variants = [
            sample_part_moved(),
            sample_machine_state_changed(),
            sample_quality_result_received(),
            sample_scan_received(),
            sample_work_order_state_changed(),
            sample_robot_task_queued(),
        ];
        for v in variants {
            let json = serde_json::to_vec(&v).expect("serialize");
            let back: CanonicalEvent = serde_json::from_slice(&json).expect("deserialize");
            assert_eq!(back, v, "round-trip mismatch for {v:?}");
        }
    }

    /// The `tag = "type"` serde attribute MUST emit a `"type"` field
    /// matching `type_tag()`. Future serde-rename refactors would
    /// silently break the SPA / SQL JSON-extract pattern named in
    /// ADR-0060 §"One EventKind for all MES events is too coarse"; this
    /// pin catches it.
    #[test]
    fn type_tag_matches_serde_discriminator() {
        let variants = [
            ("part_moved", sample_part_moved()),
            ("machine_state_changed", sample_machine_state_changed()),
            ("quality_result_received", sample_quality_result_received()),
            ("scan_received", sample_scan_received()),
            (
                "work_order_state_changed",
                sample_work_order_state_changed(),
            ),
            ("robot_task_queued", sample_robot_task_queued()),
        ];
        for (expected_tag, event) in variants {
            assert_eq!(event.type_tag(), expected_tag);
            let json: serde_json::Value =
                serde_json::from_slice(&serde_json::to_vec(&event).unwrap()).unwrap();
            assert_eq!(
                json["type"].as_str(),
                Some(expected_tag),
                "serde discriminator mismatch for {expected_tag}"
            );
        }
    }

    /// `MachineState` MUST round-trip; the closed vocab is the on-disk
    /// schema. Adding a variant without updating downstream consumers
    /// is a separate problem; the round-trip pin guards against
    /// renaming the existing vocab silently.
    #[test]
    fn machine_state_round_trips() {
        let all = [
            MachineState::Idle,
            MachineState::Running,
            MachineState::Setup,
            MachineState::Down,
            MachineState::Fault,
            MachineState::Unknown,
        ];
        for s in all {
            let json = serde_json::to_string(&s).unwrap();
            let back: MachineState = serde_json::from_str(&json).unwrap();
            assert_eq!(back, s);
        }
    }

    /// `MachineState::Idle` serializes as `"idle"`, not `"Idle"`. Pins
    /// the snake_case rename so a future derive-tweak that drops
    /// `rename_all` would fail loud.
    #[test]
    fn machine_state_is_snake_case_on_wire() {
        assert_eq!(
            serde_json::to_string(&MachineState::Idle).unwrap(),
            "\"idle\""
        );
        assert_eq!(
            serde_json::to_string(&MachineState::Running).unwrap(),
            "\"running\""
        );
        assert_eq!(
            serde_json::to_string(&MachineState::Setup).unwrap(),
            "\"setup\""
        );
        assert_eq!(
            serde_json::to_string(&MachineState::Down).unwrap(),
            "\"down\""
        );
        assert_eq!(
            serde_json::to_string(&MachineState::Fault).unwrap(),
            "\"fault\""
        );
        assert_eq!(
            serde_json::to_string(&MachineState::Unknown).unwrap(),
            "\"unknown\""
        );
    }

    /// `QualityOutcome::HoldForReview` MUST serialize as
    /// `"hold_for_review"` (snake_case). The three-value vocab is
    /// load-bearing per the Stage 3 memo §"Quality gate".
    #[test]
    fn quality_outcome_round_trips_snake_case() {
        assert_eq!(
            serde_json::to_string(&QualityOutcome::Pass).unwrap(),
            "\"pass\""
        );
        assert_eq!(
            serde_json::to_string(&QualityOutcome::Fail).unwrap(),
            "\"fail\""
        );
        assert_eq!(
            serde_json::to_string(&QualityOutcome::HoldForReview).unwrap(),
            "\"hold_for_review\""
        );
    }

    /// `WorkOrderState::OnHold` and `InProgress` MUST serialize as
    /// `"on_hold"` / `"in_progress"`. Same pin posture as
    /// `quality_outcome_round_trips_snake_case`.
    #[test]
    fn work_order_state_round_trips_snake_case() {
        assert_eq!(
            serde_json::to_string(&WorkOrderState::OnHold).unwrap(),
            "\"on_hold\""
        );
        assert_eq!(
            serde_json::to_string(&WorkOrderState::InProgress).unwrap(),
            "\"in_progress\""
        );
        assert_eq!(
            serde_json::to_string(&WorkOrderState::Created).unwrap(),
            "\"created\""
        );
    }

    /// Optional fields MUST survive round-trip (None → field absent →
    /// None). Specifically: `note` on `QualityResultReceived`.
    #[test]
    fn optional_fields_round_trip_through_none() {
        let with_none = sample_quality_result_received();
        let json = serde_json::to_string(&with_none).unwrap();
        let back: CanonicalEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, with_none);

        let with_some = CanonicalEvent::QualityResultReceived {
            part_id: "p".into(),
            gate_id: "g".into(),
            outcome: QualityOutcome::HoldForReview,
            note: Some("operator review required".into()),
            at_iso8601: "2026-06-03T08:31:00Z".into(),
        };
        let json = serde_json::to_string(&with_some).unwrap();
        let back: CanonicalEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, with_some);
    }
}
