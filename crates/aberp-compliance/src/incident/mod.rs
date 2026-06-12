//! Cyber-incident reporting types (DFARS 252.204-7012).
//!
//! A defense contractor handling Controlled Defense Information (CDI) must, per
//! **DFARS 252.204-7012(c)(1)**, report a discovered cyber incident that
//! affects CDI — or its ability to perform operationally critical requirements
//! — to the DoD **within 72 hours of discovery** (the report is filed through
//! the DIBNet / SPRS portal). The audit-ledger `incident.cyber_detected` kind
//! (S362, ADR-0079) records the *detection* that starts that 72-hour clock;
//! this module supplies the small closed vocabularies that detection metadata
//! speaks — incident severity and detection source — plus the deadline
//! arithmetic, so a free-text severity / source can never reach the ledger and
//! the 72-hour deadline is computed one way in one place.
//!
//! S362 ships the enums + the [`dod_72h_report_due_at_ms`] helper only. The
//! real report submission (SPRS), the SIEM detection backend
//! ([`IncidentDetectionProvider`] trait + mock), the incident-entry UI, and the
//! automated deadline alerting are out of scope (later sessions, mock-first per
//! `[[mock-everything-principle]]`).

/// The 72-hour DFARS reporting window, expressed in epoch-milliseconds.
///
/// `72 h × 60 min × 60 s × 1000 ms`. Named so the one place the deadline is
/// computed reads self-evidently and a future reader can see *which* clause's
/// window this is.
pub const DFARS_72H_REPORT_WINDOW_MS: i64 = 72 * 60 * 60 * 1000;

/// Severity of a detected cyber incident.
///
/// A closed five-level scale (informational → critical). The
/// `incident.cyber_detected` payload `severity` field and any future
/// incident-record column store the [`IncidentSeverity::as_str`] form; the
/// firing site (later session) validates an inbound string through
/// [`IncidentSeverity::from_storage_str`] before it reaches the ledger, so a
/// free-text severity can never be persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IncidentSeverity {
    /// Informational — noted for the record, no impact assessed.
    #[default]
    Informational,
    /// Low — minor / contained, no CDI or operational impact.
    Low,
    /// Medium — limited impact, monitoring / response underway.
    Medium,
    /// High — significant impact, likely CDI / operational implications.
    High,
    /// Critical — severe impact, confirmed or strongly suspected CDI breach.
    Critical,
}

impl IncidentSeverity {
    /// Render in the on-disk / audit-payload form — the canonical string the
    /// S362 `incident.cyber_detected` firing site writes. Paired with
    /// [`IncidentSeverity::from_storage_str`] as a round-trip-proven pair,
    /// mirroring the export-control [`crate::export_control::Jurisdiction`] and
    /// AVL [`crate::avl::ExportScreeningStatus`] discipline. A free-text severity
    /// can never reach the ledger — it must round-trip through this typed pair
    /// first.
    pub fn as_str(&self) -> &'static str {
        match self {
            IncidentSeverity::Informational => "informational",
            IncidentSeverity::Low => "low",
            IncidentSeverity::Medium => "medium",
            IncidentSeverity::High => "high",
            IncidentSeverity::Critical => "critical",
        }
    }

    /// Parse the on-disk / audit-payload form back into an `IncidentSeverity`.
    /// Errors on unknown strings — silent fallback would mask schema drift
    /// (CLAUDE.md rule 12, "fail loud"); a mis-parse of an unrecognised severity
    /// to [`IncidentSeverity::Informational`] would silently downgrade a
    /// reportable incident below the 72-hour threshold.
    pub fn from_storage_str(s: &str) -> Result<Self, &'static str> {
        match s {
            "informational" => Ok(IncidentSeverity::Informational),
            "low" => Ok(IncidentSeverity::Low),
            "medium" => Ok(IncidentSeverity::Medium),
            "high" => Ok(IncidentSeverity::High),
            "critical" => Ok(IncidentSeverity::Critical),
            _ => Err("unknown IncidentSeverity storage string"),
        }
    }
}

/// How a cyber incident was detected.
///
/// A closed source vocabulary. The `incident.cyber_detected` payload
/// `detection_source` field stores the [`DetectionSource::as_str`] form,
/// validated at the (future) firing site through
/// [`DetectionSource::from_storage_str`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DetectionSource {
    /// A SIEM / IDS / monitoring system raised the alert.
    #[default]
    Siem,
    /// A user / operator reported it.
    UserReport,
    /// A vendor / supplier / third party notified the contractor.
    VendorNotification,
    /// Surfaced by an internal audit / log review.
    Audit,
    /// Any other source not covered above.
    Other,
}

impl DetectionSource {
    /// Render in the on-disk / audit-payload form — the canonical string the
    /// S362 `incident.cyber_detected` firing site writes. Round-trip-proven with
    /// [`DetectionSource::from_storage_str`].
    pub fn as_str(&self) -> &'static str {
        match self {
            DetectionSource::Siem => "siem",
            DetectionSource::UserReport => "user_report",
            DetectionSource::VendorNotification => "vendor_notification",
            DetectionSource::Audit => "audit",
            DetectionSource::Other => "other",
        }
    }

    /// Parse the on-disk / audit-payload form back into a `DetectionSource`.
    /// Errors on unknown strings — fail loud (CLAUDE.md rule 12) rather than
    /// silently bucketing an unrecognised source into [`DetectionSource::Other`].
    pub fn from_storage_str(s: &str) -> Result<Self, &'static str> {
        match s {
            "siem" => Ok(DetectionSource::Siem),
            "user_report" => Ok(DetectionSource::UserReport),
            "vendor_notification" => Ok(DetectionSource::VendorNotification),
            "audit" => Ok(DetectionSource::Audit),
            "other" => Ok(DetectionSource::Other),
            _ => Err("unknown DetectionSource storage string"),
        }
    }
}

/// Compute the DFARS 252.204-7012(c)(1) 72-hour reporting deadline, if the
/// incident triggers one.
///
/// Per **252.204-7012(c)(1)(i)**, a rapid report is required when a discovered
/// cyber incident affects **(A)** covered defense information (CDI) on a covered
/// contractor information system, **OR (B)** the contractor's ability to
/// perform requirements designated as **operationally critical support (OCS)**.
/// Returns `Some(detected_at_ms + [`DFARS_72H_REPORT_WINDOW_MS`])` — the instant
/// by which the report is due — when *either* trigger fires, and `None` when
/// neither does (no reporting deadline attaches). The firing site (later
/// session) stamps the `incident.cyber_detected` payload's optional
/// `dod_72h_report_due_at_ms` with this value; this function is the single place
/// the window is added, so the deadline is computed one way everywhere.
///
/// S366 review F16 widened the trigger: the prior helper keyed only on
/// `cdi_affected`, so an OCS-only incident — half of the clause's trigger — got
/// no deadline stamp.
///
/// Pure arithmetic — no `Date::now()`. The caller supplies the detection stamp;
/// the deadline derives deterministically from it.
pub fn dod_72h_report_due_at_ms(
    detected_at_ms: i64,
    cdi_affected: bool,
    ocs_affected: bool,
) -> Option<i64> {
    if cdi_affected || ocs_affected {
        Some(detected_at_ms + DFARS_72H_REPORT_WINDOW_MS)
    } else {
        None
    }
}

#[cfg(test)]
mod tests;
