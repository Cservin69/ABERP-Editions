//! Unit tests for the S362 cyber-incident-reporting types (ADR-0079).

use super::{
    dod_72h_report_due_at_ms, DetectionSource, IncidentSeverity, DFARS_72H_REPORT_WINDOW_MS,
};

#[test]
fn s362_incident_severity_defaults_to_informational() {
    assert_eq!(IncidentSeverity::default(), IncidentSeverity::Informational);
}

#[test]
fn s362_detection_source_defaults_to_siem() {
    assert_eq!(DetectionSource::default(), DetectionSource::Siem);
}

// ── Storage-string round-trips ──
//
// The `incident.cyber_detected` audit payload stores the canonical `as_str`
// form; the firing site (later session) validates an inbound string through
// `from_storage_str` before it reaches the ledger. These pin that the `as_str`
// ⇄ `from_storage_str` pair round-trips every variant and rejects garbage, so a
// malformed value can never reach storage.

#[test]
fn s362_incident_severity_storage_str_round_trips_every_variant() {
    for sev in [
        IncidentSeverity::Informational,
        IncidentSeverity::Low,
        IncidentSeverity::Medium,
        IncidentSeverity::High,
        IncidentSeverity::Critical,
    ] {
        let s = sev.as_str();
        assert_eq!(
            IncidentSeverity::from_storage_str(s).expect("round-trip"),
            sev,
            "round-trip mismatch for {s}"
        );
    }
}

#[test]
fn s362_incident_severity_tokens_are_pinned_and_reject_unknown() {
    // The exact severity vocab the brief pins for the payload.
    assert_eq!(IncidentSeverity::Informational.as_str(), "informational");
    assert_eq!(IncidentSeverity::Low.as_str(), "low");
    assert_eq!(IncidentSeverity::Medium.as_str(), "medium");
    assert_eq!(IncidentSeverity::High.as_str(), "high");
    assert_eq!(IncidentSeverity::Critical.as_str(), "critical");
    // A mis-parse to Informational would silently downgrade a reportable
    // incident below the 72-hour threshold. Must fail loud.
    assert!(IncidentSeverity::from_storage_str("HIGH").is_err());
    assert!(IncidentSeverity::from_storage_str("severe").is_err());
    assert!(IncidentSeverity::from_storage_str("").is_err());
}

#[test]
fn s362_detection_source_storage_str_round_trips_every_variant() {
    for src in [
        DetectionSource::Siem,
        DetectionSource::UserReport,
        DetectionSource::VendorNotification,
        DetectionSource::Audit,
        DetectionSource::Other,
    ] {
        let s = src.as_str();
        assert_eq!(
            DetectionSource::from_storage_str(s).expect("round-trip"),
            src,
            "round-trip mismatch for {s}"
        );
    }
}

#[test]
fn s362_detection_source_tokens_match_brief_vocab_and_reject_unknown() {
    assert_eq!(DetectionSource::Siem.as_str(), "siem");
    assert_eq!(DetectionSource::UserReport.as_str(), "user_report");
    assert_eq!(
        DetectionSource::VendorNotification.as_str(),
        "vendor_notification"
    );
    assert_eq!(DetectionSource::Audit.as_str(), "audit");
    assert_eq!(DetectionSource::Other.as_str(), "other");
    assert!(DetectionSource::from_storage_str("SIEM").is_err());
    assert!(DetectionSource::from_storage_str("ids").is_err());
    assert!(DetectionSource::from_storage_str("").is_err());
}

// ── The DFARS 72-hour deadline arithmetic ──

#[test]
fn s362_dod_72h_report_due_is_exactly_72h_after_detection() {
    let detected_at_ms = 1_750_000_000_000_i64;
    // A CDI-affecting incident triggers the deadline.
    let due = dod_72h_report_due_at_ms(detected_at_ms, true, false).expect("CDI triggers deadline");
    assert_eq!(due - detected_at_ms, DFARS_72H_REPORT_WINDOW_MS);
    // 72 h in ms, spelled out independently of the constant.
    assert_eq!(due - detected_at_ms, 259_200_000);
}

#[test]
fn s367_dod_72h_trigger_covers_both_cdi_and_ocs() {
    // F16: 252.204-7012(c)(1)(i) triggers on CDI-affecting OR operationally-
    // critical-support-affecting. Either alone arms the 72-hour clock; both
    // together do; neither leaves no deadline.
    let t = 1_750_000_000_000_i64;
    let due = t + DFARS_72H_REPORT_WINDOW_MS;
    assert_eq!(dod_72h_report_due_at_ms(t, true, false), Some(due)); // CDI only
    assert_eq!(dod_72h_report_due_at_ms(t, false, true), Some(due)); // OCS only
    assert_eq!(dod_72h_report_due_at_ms(t, true, true), Some(due)); // both
    assert_eq!(dod_72h_report_due_at_ms(t, false, false), None); // neither
}

#[test]
fn s362_dod_72h_report_window_constant_is_72_hours() {
    assert_eq!(DFARS_72H_REPORT_WINDOW_MS, 72 * 60 * 60 * 1000);
}
