//! D-09 (S362) — DFARS 252.204-7012 cyber-incident intake.
//!
//! Fires [`EventKind::IncidentCyberDetected`] (`incident.cyber_detected`) with
//! the payload pinned in `aberp_audit_ledger`'s
//! `s362_incident_cyber_detected_payload_serializes`. The 72-hour DoD reporting
//! deadline is computed by [`aberp_compliance::incident::dod_72h_report_due_at_ms`]
//! and is present in the payload **only** when CDI or OCS is affected (the two
//! conditions that start the 252.204-7012(c) clock).
//!
//! No PII / no controlled content at rest (mirrors the S362 kind doc): the
//! `scope_description` is a summary — never raw log dumps — `operator_user_id`
//! is an opaque accountability handle, and `affected_systems` are identifiers,
//! not their contents. The append rides the caller's transaction on the shared
//! `aberp_db::Handle` (ADR-0099 — no independent opener).

use anyhow::{Context, Result};
use duckdb::Transaction;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use ulid::Ulid;

use aberp_audit_ledger::{append_in_tx, Actor, EventKind, LedgerMeta};
use aberp_compliance::incident::{dod_72h_report_due_at_ms, DetectionSource, IncidentSeverity};

/// Operator-supplied intake for a cyber incident. `detected_at_ms` defaults to
/// "now" when omitted (the operator is logging a live discovery);
/// `operator_user_id` is NOT taken from the body — the serve layer fills it
/// from the authenticated session so the accountability handle cannot be
/// spoofed.
#[derive(Debug, Clone, Deserialize)]
pub struct CyberIncidentInput {
    pub severity: String,
    pub scope_description: String,
    pub detection_source: String,
    #[serde(default)]
    pub detected_at_ms: Option<i64>,
    #[serde(default)]
    pub cdi_affected: bool,
    #[serde(default)]
    pub cui_affected: bool,
    #[serde(default)]
    pub ocs_affected: bool,
    #[serde(default)]
    pub exfiltration_suspected: bool,
    #[serde(default)]
    pub affected_systems: Vec<String>,
    #[serde(default)]
    pub mitigation_notes: Option<String>,
}

/// The recorded incident, echoed back to the operator. `incident_id` is the
/// server-minted reference (also the audit entry's business key); it is NOT
/// part of the pinned audit payload. `dod_72h_report_due_at_ms` is `Some` iff
/// CDI or OCS is affected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CyberIncidentRecord {
    pub incident_id: String,
    pub detected_at_ms: i64,
    pub operator_user_id: String,
    pub severity: String,
    pub scope_description: String,
    pub cdi_affected: bool,
    pub cui_affected: bool,
    pub ocs_affected: bool,
    pub exfiltration_suspected: bool,
    pub affected_systems: Vec<String>,
    pub detection_source: String,
    pub mitigation_notes: Option<String>,
    pub dod_72h_report_due_at_ms: Option<i64>,
}

/// Intake rejections — all operator-fixable, so the serve layer maps every
/// variant to 400.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum CyberIncidentError {
    #[error("unknown severity {value:?} (expected informational/low/medium/high/critical)")]
    InvalidSeverity { value: String },
    #[error(
        "unknown detection_source {value:?} \
         (expected siem/user_report/vendor_notification/audit/other)"
    )]
    InvalidDetectionSource { value: String },
    #[error("scope_description must not be empty")]
    EmptyScope,
}

/// The pinned `incident.cyber_detected` payload shape (no `incident_id` — that
/// is a serve-side reference, not part of the audit contract). The two
/// optionals are omitted when absent, matching the S362 doc ("an optional
/// `mitigation_notes`, and an optional `dod_72h_report_due_at_ms`").
#[derive(Serialize)]
struct CyberIncidentPayload<'a> {
    detected_at_ms: i64,
    operator_user_id: &'a str,
    severity: &'a str,
    scope_description: &'a str,
    cdi_affected: bool,
    ocs_affected: bool,
    cui_affected: bool,
    exfiltration_suspected: bool,
    affected_systems: &'a [String],
    detection_source: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    mitigation_notes: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dod_72h_report_due_at_ms: Option<i64>,
}

impl CyberIncidentRecord {
    /// Validate + normalise an operator intake into a recordable incident.
    /// `now_ms` is the fallback discovery stamp; `operator_user_id` comes from
    /// the authenticated session, never the request body.
    pub fn from_input(
        input: &CyberIncidentInput,
        operator_user_id: &str,
        now_ms: i64,
    ) -> std::result::Result<Self, CyberIncidentError> {
        // Validate through the compliance enums (exact storage strings) and
        // store the canonical `as_str()` form so a malformed value can never
        // reach the ledger.
        let severity = IncidentSeverity::from_storage_str(input.severity.trim()).map_err(|_| {
            CyberIncidentError::InvalidSeverity {
                value: input.severity.clone(),
            }
        })?;
        let detection_source = DetectionSource::from_storage_str(input.detection_source.trim())
            .map_err(|_| CyberIncidentError::InvalidDetectionSource {
                value: input.detection_source.clone(),
            })?;
        let scope = input.scope_description.trim();
        if scope.is_empty() {
            return Err(CyberIncidentError::EmptyScope);
        }

        let detected_at_ms = input.detected_at_ms.unwrap_or(now_ms);
        let dod_72h_report_due_at_ms =
            dod_72h_report_due_at_ms(detected_at_ms, input.cdi_affected, input.ocs_affected);

        let affected_systems: Vec<String> = input
            .affected_systems
            .iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        let mitigation_notes = input
            .mitigation_notes
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        Ok(Self {
            incident_id: format!("inc_{}", Ulid::new()),
            detected_at_ms,
            operator_user_id: operator_user_id.to_string(),
            severity: severity.as_str().to_string(),
            scope_description: scope.to_string(),
            cdi_affected: input.cdi_affected,
            cui_affected: input.cui_affected,
            ocs_affected: input.ocs_affected,
            exfiltration_suspected: input.exfiltration_suspected,
            affected_systems,
            detection_source: detection_source.as_str().to_string(),
            mitigation_notes,
            dod_72h_report_due_at_ms,
        })
    }

    /// The pinned audit-payload bytes (no `incident_id`).
    pub fn payload_bytes(&self) -> Vec<u8> {
        let payload = CyberIncidentPayload {
            detected_at_ms: self.detected_at_ms,
            operator_user_id: &self.operator_user_id,
            severity: &self.severity,
            scope_description: &self.scope_description,
            cdi_affected: self.cdi_affected,
            ocs_affected: self.ocs_affected,
            cui_affected: self.cui_affected,
            exfiltration_suspected: self.exfiltration_suspected,
            affected_systems: &self.affected_systems,
            detection_source: &self.detection_source,
            mitigation_notes: self.mitigation_notes.as_deref(),
            dod_72h_report_due_at_ms: self.dod_72h_report_due_at_ms,
        };
        serde_json::to_vec(&payload).expect("JSON serialize CyberIncidentPayload")
    }
}

/// Append `incident.cyber_detected` for `record` inside the caller's tx.
pub fn append_cyber_incident_in_tx(
    tx: &Transaction<'_>,
    ledger_meta: &LedgerMeta,
    ledger_actor: Actor,
    record: &CyberIncidentRecord,
) -> Result<()> {
    append_in_tx(
        tx,
        ledger_meta,
        EventKind::IncidentCyberDetected,
        record.payload_bytes(),
        ledger_actor,
        Some(format!("cyber_incident:{}", record.incident_id)),
    )
    .context("audit append IncidentCyberDetected")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> CyberIncidentInput {
        CyberIncidentInput {
            severity: "high".to_string(),
            scope_description: "Anomalous outbound traffic from CAD workstation segment"
                .to_string(),
            detection_source: "siem".to_string(),
            detected_at_ms: Some(1_750_000_000_000),
            cdi_affected: true,
            cui_affected: true,
            ocs_affected: false,
            exfiltration_suspected: false,
            affected_systems: vec!["cad-ws-04".to_string(), "file-srv-02".to_string()],
            mitigation_notes: Some("Segment isolated; credentials rotated.".to_string()),
        }
    }

    #[test]
    fn from_input_normalises_and_computes_the_72h_deadline() {
        let rec = CyberIncidentRecord::from_input(&input(), "mock-op-007", 42).unwrap();
        assert!(rec.incident_id.starts_with("inc_"));
        assert_eq!(rec.detected_at_ms, 1_750_000_000_000);
        assert_eq!(rec.operator_user_id, "mock-op-007");
        assert_eq!(rec.severity, "high");
        assert_eq!(rec.detection_source, "siem");
        // cdi_affected → the DFARS clock starts.
        assert_eq!(
            rec.dod_72h_report_due_at_ms,
            Some(1_750_000_000_000 + 72 * 60 * 60 * 1000)
        );
    }

    #[test]
    fn deadline_absent_when_neither_cdi_nor_ocs_affected() {
        let mut inp = input();
        inp.cdi_affected = false;
        inp.ocs_affected = false;
        let rec = CyberIncidentRecord::from_input(&inp, "op", 0).unwrap();
        assert_eq!(rec.dod_72h_report_due_at_ms, None);
    }

    #[test]
    fn deadline_present_when_only_ocs_affected() {
        let mut inp = input();
        inp.cdi_affected = false;
        inp.ocs_affected = true;
        // Null the explicit stamp so the deadline is measured off `now_ms`.
        inp.detected_at_ms = None;
        let rec = CyberIncidentRecord::from_input(&inp, "op", 100).unwrap();
        assert_eq!(rec.detected_at_ms, 100);
        assert_eq!(
            rec.dod_72h_report_due_at_ms,
            Some(100 + 72 * 60 * 60 * 1000)
        );
    }

    #[test]
    fn detected_at_ms_defaults_to_now_when_omitted() {
        let mut inp = input();
        inp.detected_at_ms = None;
        let rec = CyberIncidentRecord::from_input(&inp, "op", 999).unwrap();
        assert_eq!(rec.detected_at_ms, 999);
    }

    #[test]
    fn bad_severity_is_rejected() {
        let mut inp = input();
        inp.severity = "catastrophic".to_string();
        assert_eq!(
            CyberIncidentRecord::from_input(&inp, "op", 0),
            Err(CyberIncidentError::InvalidSeverity {
                value: "catastrophic".to_string()
            })
        );
    }

    #[test]
    fn bad_detection_source_is_rejected() {
        let mut inp = input();
        inp.detection_source = "telepathy".to_string();
        assert_eq!(
            CyberIncidentRecord::from_input(&inp, "op", 0),
            Err(CyberIncidentError::InvalidDetectionSource {
                value: "telepathy".to_string()
            })
        );
    }

    #[test]
    fn empty_scope_is_rejected() {
        let mut inp = input();
        inp.scope_description = "   ".to_string();
        assert_eq!(
            CyberIncidentRecord::from_input(&inp, "op", 0),
            Err(CyberIncidentError::EmptyScope)
        );
    }

    /// Mirror the `event_kind.rs` S362 pin: the payload carries exactly the
    /// documented fields with the documented JSON types, and the deadline is
    /// exactly 72h after detection. `incident_id` must NOT leak into it.
    #[test]
    fn payload_matches_the_pinned_s362_shape() {
        let rec = CyberIncidentRecord::from_input(&input(), "mock-op-007", 0).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&rec.payload_bytes()).unwrap();

        assert!(
            parsed.get("incident_id").is_none(),
            "payload must not carry incident_id"
        );
        assert!(parsed["detected_at_ms"].is_i64());
        assert_eq!(parsed["operator_user_id"], "mock-op-007");
        assert_eq!(parsed["severity"], "high");
        assert!(parsed["scope_description"].is_string());
        assert_eq!(parsed["cdi_affected"], true);
        assert_eq!(parsed["ocs_affected"], false);
        assert_eq!(parsed["cui_affected"], true);
        assert_eq!(parsed["exfiltration_suspected"], false);
        assert!(parsed["affected_systems"].is_array());
        assert_eq!(parsed["affected_systems"][0], "cad-ws-04");
        assert_eq!(parsed["detection_source"], "siem");
        assert!(parsed["mitigation_notes"].is_string());
        assert!(parsed["dod_72h_report_due_at_ms"].is_i64());
        assert_eq!(
            parsed["dod_72h_report_due_at_ms"].as_i64().unwrap() - 1_750_000_000_000,
            72 * 60 * 60 * 1000
        );
    }

    /// The two optionals drop out of the JSON when absent (not serialized as
    /// `null`) — a no-CDI/no-OCS incident with no notes.
    #[test]
    fn optionals_are_omitted_when_absent() {
        let mut inp = input();
        inp.cdi_affected = false;
        inp.ocs_affected = false;
        inp.mitigation_notes = None;
        let rec = CyberIncidentRecord::from_input(&inp, "op", 0).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&rec.payload_bytes()).unwrap();
        assert!(parsed.get("dod_72h_report_due_at_ms").is_none());
        assert!(parsed.get("mitigation_notes").is_none());
    }
}
