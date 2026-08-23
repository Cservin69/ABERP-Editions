//! ADR-0199 §D1/§D2/§D3/§D4 — the QC-report closed vocabularies.
//!
//! Every token a report can carry is an enum with an `as_str` /
//! `from_storage_str` round-trip pair, exactly like [`super::Verdict`]
//! and `aberp_dispatch::CarrierKind`. Nothing here is free text, and
//! that is load-bearing rather than tidy:
//!
//! - **A report is a compliance document.** ADR-0199 §D2 rejected a real
//!   template engine (Tera / Handlebars / operator-uploaded layouts)
//!   because *an operator who can edit the template can hide a failing
//!   characteristic, and nothing in the audit chain would show it*. A
//!   closed [`QcReportTemplate`] vocabulary means a new customer form is
//!   a new variant and a new render function **in a reviewed PR**, the
//!   same posture `CarrierKind` takes on Hungarian carriers.
//! - **`from_storage_str` never falls back.** An unknown token is an
//!   error, not a default — a silently-coerced token would let a
//!   corrupted or hand-edited row render as something it is not
//!   (CLAUDE.md rule 12).

use serde::{Deserialize, Serialize};

/// Which document shape a [`super::reports::QcReport`] renders as.
///
/// All three project from the SAME frozen `qc_reports` +
/// `qc_report_lines` snapshot; they differ only in layout and in which
/// blocks they print (ADR-0199 §D1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QcReportKind {
    /// The per-shipment dimensional-inspection report — the default, and
    /// the document Ervin asked for. One row per characteristic per
    /// serialised unit, plus the traceability block and the disposition.
    DimensionalInspection,
    /// Certificate of Conformance. One page, NO characteristic table:
    /// the conformance statement, part + drawing rev, qty, serial range,
    /// heat/lot + mill cert, the QC report number it certifies against,
    /// disposition, signature block.
    CertificateOfConformance,
    /// AS9102 First Article Inspection Report, Forms 1/2/3 at **Rev C**
    /// (Ervin confirmed Rev C explicitly — ADR-0199 §Open Q1). A FAIR is
    /// a FIRST-ARTICLE event, not a per-shipment one: it is generated on
    /// demand from the same characteristics, never automatically per
    /// delivery.
    As9102Fair,
}

impl QcReportKind {
    /// On-disk / wire token.
    pub fn as_str(&self) -> &'static str {
        match self {
            QcReportKind::DimensionalInspection => "dimensional_inspection",
            QcReportKind::CertificateOfConformance => "coc",
            QcReportKind::As9102Fair => "as9102_fair",
        }
    }

    pub fn from_storage_str(s: &str) -> Result<Self, &'static str> {
        match s {
            "dimensional_inspection" => Ok(QcReportKind::DimensionalInspection),
            "coc" => Ok(QcReportKind::CertificateOfConformance),
            "as9102_fair" => Ok(QcReportKind::As9102Fair),
            _ => Err("unknown QcReportKind storage string"),
        }
    }
}

/// The customer-resolved layout family (ADR-0199 §D2).
///
/// Resolution order, most specific first: `partners.qc_report_template`
/// → tenant default → [`QcReportTemplate::AbenStandard`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum QcReportTemplate {
    /// House dimensional-inspection layout. The default.
    #[default]
    AbenStandard,
    /// AS9102 Rev C Forms 1/2/3 layout.
    As9102RevC,
    /// Certificate of conformance only — no characteristic table.
    CocOnly,
}

impl QcReportTemplate {
    pub fn as_str(&self) -> &'static str {
        match self {
            QcReportTemplate::AbenStandard => "aben_standard",
            QcReportTemplate::As9102RevC => "as9102_rev_c",
            QcReportTemplate::CocOnly => "coc_only",
        }
    }

    pub fn from_storage_str(s: &str) -> Result<Self, &'static str> {
        match s {
            "aben_standard" => Ok(QcReportTemplate::AbenStandard),
            "as9102_rev_c" => Ok(QcReportTemplate::As9102RevC),
            "coc_only" => Ok(QcReportTemplate::CocOnly),
            _ => Err("unknown QcReportTemplate storage string"),
        }
    }

    /// Whether this template can produce `kind`.
    ///
    /// The pairing is checked in code at the draft boundary so a
    /// nonsensical combination (a `CocOnly` customer being handed an
    /// AS9102 FAIR, or an `AbenStandard` customer being handed one) can
    /// never reach a frozen row. `CocOnly` is the restrictive case: a
    /// customer configured for certificate-only explicitly does NOT
    /// receive characteristic tables, and silently upgrading them to one
    /// would leak measurements the customer's contract does not cover.
    pub fn permits(&self, kind: QcReportKind) -> bool {
        match self {
            // The house template covers the per-shipment pair. A FAIR is
            // an AS9102 artefact and needs the AS9102 template.
            QcReportTemplate::AbenStandard => matches!(
                kind,
                QcReportKind::DimensionalInspection | QcReportKind::CertificateOfConformance
            ),
            // The AS9102 template covers everything: a shop on AS9102 still
            // ships the per-delivery dimensional report + CoC.
            QcReportTemplate::As9102RevC => true,
            QcReportTemplate::CocOnly => matches!(kind, QcReportKind::CertificateOfConformance),
        }
    }
}

/// Report lifecycle. There is deliberately **no delete path** — a
/// mistake is corrected by a new document, never by editing the old one,
/// matching the invoice posture (ADR-0199 §D7 "Retention").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QcReportState {
    /// Created, not yet issued. Mutable: re-freezing recomputes the lines.
    Drafted,
    /// Frozen and hashed. `rendered_sha256` + `issued_at_utc` are set;
    /// the lines can never change again.
    Issued,
    /// Replaced by a later report (`superseded_by_qcr_id`).
    Superseded,
    /// Withdrawn. Never deleted, never edited.
    Voided,
}

impl QcReportState {
    pub fn as_str(&self) -> &'static str {
        match self {
            QcReportState::Drafted => "drafted",
            QcReportState::Issued => "issued",
            QcReportState::Superseded => "superseded",
            QcReportState::Voided => "voided",
        }
    }

    pub fn from_storage_str(s: &str) -> Result<Self, &'static str> {
        match s {
            "drafted" => Ok(QcReportState::Drafted),
            "issued" => Ok(QcReportState::Issued),
            "superseded" => Ok(QcReportState::Superseded),
            "voided" => Ok(QcReportState::Voided),
            _ => Err("unknown QcReportState storage string"),
        }
    }
}

/// AS9102 Form 3 characteristic classification (`key` / `critical` /
/// `major` / `minor` / `none`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CharacteristicDesignator {
    /// A Key Characteristic — variation materially affects fit, form,
    /// function or safety.
    Key,
    Critical,
    Major,
    Minor,
    #[default]
    None,
}

impl CharacteristicDesignator {
    pub fn as_str(&self) -> &'static str {
        match self {
            CharacteristicDesignator::Key => "key",
            CharacteristicDesignator::Critical => "critical",
            CharacteristicDesignator::Major => "major",
            CharacteristicDesignator::Minor => "minor",
            CharacteristicDesignator::None => "none",
        }
    }

    pub fn from_storage_str(s: &str) -> Result<Self, &'static str> {
        match s {
            "key" => Ok(CharacteristicDesignator::Key),
            "critical" => Ok(CharacteristicDesignator::Critical),
            "major" => Ok(CharacteristicDesignator::Major),
            "minor" => Ok(CharacteristicDesignator::Minor),
            "none" => Ok(CharacteristicDesignator::None),
            _ => Err("unknown CharacteristicDesignator storage string"),
        }
    }
}

/// What KIND of requirement the characteristic is (AS9102 Form 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CharacteristicType {
    /// A measured dimension — the only type that carries a nominal + band.
    #[default]
    Dimensional,
    /// Material conformity (grade, heat/lot, mill cert).
    Material,
    /// A special process (heat treat, coating, NDT).
    Process,
    /// A drawing note requiring accountability but no measurement.
    Note,
    /// Functional / performance test.
    Functional,
}

impl CharacteristicType {
    pub fn as_str(&self) -> &'static str {
        match self {
            CharacteristicType::Dimensional => "dimensional",
            CharacteristicType::Material => "material",
            CharacteristicType::Process => "process",
            CharacteristicType::Note => "note",
            CharacteristicType::Functional => "functional",
        }
    }

    pub fn from_storage_str(s: &str) -> Result<Self, &'static str> {
        match s {
            "dimensional" => Ok(CharacteristicType::Dimensional),
            "material" => Ok(CharacteristicType::Material),
            "process" => Ok(CharacteristicType::Process),
            "note" => Ok(CharacteristicType::Note),
            "functional" => Ok(CharacteristicType::Functional),
            _ => Err("unknown CharacteristicType storage string"),
        }
    }
}

/// How the characteristic is inspected (AS9102 Form 3 "inspection
/// method"). `OnMachineProbe` is the DMG MORI NTX path ADR-0092 built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InspectionMethod {
    OnMachineProbe,
    Cmm,
    Gauge,
    Visual,
    /// Conformity established by reviewing a supplier certificate
    /// (mill cert, heat-treat cert) rather than by measuring.
    CertReview,
}

impl InspectionMethod {
    pub fn as_str(&self) -> &'static str {
        match self {
            InspectionMethod::OnMachineProbe => "on_machine_probe",
            InspectionMethod::Cmm => "cmm",
            InspectionMethod::Gauge => "gauge",
            InspectionMethod::Visual => "visual",
            InspectionMethod::CertReview => "cert_review",
        }
    }

    pub fn from_storage_str(s: &str) -> Result<Self, &'static str> {
        match s {
            "on_machine_probe" => Ok(InspectionMethod::OnMachineProbe),
            "cmm" => Ok(InspectionMethod::Cmm),
            "gauge" => Ok(InspectionMethod::Gauge),
            "visual" => Ok(InspectionMethod::Visual),
            "cert_review" => Ok(InspectionMethod::CertReview),
            _ => Err("unknown InspectionMethod storage string"),
        }
    }
}

/// Per-line accountability (ADR-0199 §D4) — **the single most important
/// vocabulary in this module**.
///
/// A report that lists only what was measured is the selective-recording
/// failure mode ADR-0092 names in its Context (*"re-measure the marginal
/// feature until it passes"*), moved from the shop floor to the printer.
/// [`Accountability::NotMeasured`] is therefore a row that gets PRINTED,
/// with a blank actual — never an omission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Accountability {
    /// A measurement exists and was frozen onto this line.
    Measured,
    /// The characteristic is in scope but has NO measurement. Renders as
    /// an explicit row with a blank actual and forces the report
    /// `incomplete` when the characteristic is required.
    NotMeasured,
    /// The characteristic does not apply to this unit (e.g. a lot-level
    /// material characteristic when enumerating a serial). Does not count
    /// as unaccounted.
    NotApplicable,
}

impl Accountability {
    pub fn as_str(&self) -> &'static str {
        match self {
            Accountability::Measured => "measured",
            Accountability::NotMeasured => "not_measured",
            Accountability::NotApplicable => "not_applicable",
        }
    }

    pub fn from_storage_str(s: &str) -> Result<Self, &'static str> {
        match s {
            "measured" => Ok(Accountability::Measured),
            "not_measured" => Ok(Accountability::NotMeasured),
            "not_applicable" => Ok(Accountability::NotApplicable),
            _ => Err("unknown Accountability storage string"),
        }
    }
}

/// The report's overall disposition — **computed, never operator-typed**
/// (ADR-0199 §D4). See [`super::reports::compute_disposition`] for the
/// rule and why each arm exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    Accept,
    AcceptWithNcr,
    Reject,
    /// Not enough evidence to state conformity: a required characteristic
    /// is unaccounted-for, or a line is `CalibrationStale`. This is the
    /// disposition the shipment gate refuses on.
    Incomplete,
}

impl Disposition {
    pub fn as_str(&self) -> &'static str {
        match self {
            Disposition::Accept => "accept",
            Disposition::AcceptWithNcr => "accept_with_ncr",
            Disposition::Reject => "reject",
            Disposition::Incomplete => "incomplete",
        }
    }

    pub fn from_storage_str(s: &str) -> Result<Self, &'static str> {
        match s {
            "accept" => Ok(Disposition::Accept),
            "accept_with_ncr" => Ok(Disposition::AcceptWithNcr),
            "reject" => Ok(Disposition::Reject),
            "incomplete" => Ok(Disposition::Incomplete),
            _ => Err("unknown Disposition storage string"),
        }
    }

    /// Whether a shipment may leave carrying a report in this
    /// disposition. `Accept` and `AcceptWithNcr` ship (an open NCR is
    /// already the ADR-0090 gate's business, not this one's);
    /// `Reject` and `Incomplete` do not.
    ///
    /// This is the predicate the D6 shipment gate turns on. It lives here,
    /// on the vocabulary, so it is pure and unit-testable without a
    /// database, and so a future "warn-only" policy change is one
    /// function to edit.
    pub fn permits_shipment(&self) -> bool {
        match self {
            Disposition::Accept | Disposition::AcceptWithNcr => true,
            Disposition::Reject | Disposition::Incomplete => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every vocabulary round-trips through storage. A token that does not
    /// survive a write/read cycle would silently change a frozen
    /// compliance record's meaning.
    #[test]
    fn every_vocabulary_round_trips() {
        for v in [
            QcReportKind::DimensionalInspection,
            QcReportKind::CertificateOfConformance,
            QcReportKind::As9102Fair,
        ] {
            assert_eq!(QcReportKind::from_storage_str(v.as_str()), Ok(v));
        }
        for v in [
            QcReportTemplate::AbenStandard,
            QcReportTemplate::As9102RevC,
            QcReportTemplate::CocOnly,
        ] {
            assert_eq!(QcReportTemplate::from_storage_str(v.as_str()), Ok(v));
        }
        for v in [
            QcReportState::Drafted,
            QcReportState::Issued,
            QcReportState::Superseded,
            QcReportState::Voided,
        ] {
            assert_eq!(QcReportState::from_storage_str(v.as_str()), Ok(v));
        }
        for v in [
            CharacteristicDesignator::Key,
            CharacteristicDesignator::Critical,
            CharacteristicDesignator::Major,
            CharacteristicDesignator::Minor,
            CharacteristicDesignator::None,
        ] {
            assert_eq!(
                CharacteristicDesignator::from_storage_str(v.as_str()),
                Ok(v)
            );
        }
        for v in [
            CharacteristicType::Dimensional,
            CharacteristicType::Material,
            CharacteristicType::Process,
            CharacteristicType::Note,
            CharacteristicType::Functional,
        ] {
            assert_eq!(CharacteristicType::from_storage_str(v.as_str()), Ok(v));
        }
        for v in [
            InspectionMethod::OnMachineProbe,
            InspectionMethod::Cmm,
            InspectionMethod::Gauge,
            InspectionMethod::Visual,
            InspectionMethod::CertReview,
        ] {
            assert_eq!(InspectionMethod::from_storage_str(v.as_str()), Ok(v));
        }
        for v in [
            Accountability::Measured,
            Accountability::NotMeasured,
            Accountability::NotApplicable,
        ] {
            assert_eq!(Accountability::from_storage_str(v.as_str()), Ok(v));
        }
        for v in [
            Disposition::Accept,
            Disposition::AcceptWithNcr,
            Disposition::Reject,
            Disposition::Incomplete,
        ] {
            assert_eq!(Disposition::from_storage_str(v.as_str()), Ok(v));
        }
    }

    /// No vocabulary silently accepts an unknown token. A fallback would
    /// let a hand-edited row render as something it is not.
    #[test]
    fn unknown_tokens_are_refused_not_defaulted() {
        assert!(QcReportKind::from_storage_str("fair").is_err());
        assert!(QcReportTemplate::from_storage_str("boeing").is_err());
        assert!(QcReportState::from_storage_str("deleted").is_err());
        assert!(CharacteristicDesignator::from_storage_str("KEY").is_err());
        assert!(CharacteristicType::from_storage_str("").is_err());
        assert!(InspectionMethod::from_storage_str("probe").is_err());
        assert!(Accountability::from_storage_str("missing").is_err());
        assert!(Disposition::from_storage_str("ok").is_err());
    }

    /// The safety-critical half of the vocabulary: exactly two
    /// dispositions may ship. `Incomplete` — the one a missing required
    /// characteristic produces — must NOT.
    #[test]
    fn only_accept_dispositions_permit_a_shipment() {
        assert!(Disposition::Accept.permits_shipment());
        assert!(Disposition::AcceptWithNcr.permits_shipment());
        assert!(!Disposition::Reject.permits_shipment());
        assert!(
            !Disposition::Incomplete.permits_shipment(),
            "an incomplete report MUST refuse the shipment — ADR-0199 §D6, \
             Ervin confirmed 'block' explicitly"
        );
    }

    /// A certificate-only customer never receives a characteristic table,
    /// and a FAIR needs the AS9102 template.
    #[test]
    fn template_kind_pairing_is_closed() {
        assert!(QcReportTemplate::CocOnly.permits(QcReportKind::CertificateOfConformance));
        assert!(!QcReportTemplate::CocOnly.permits(QcReportKind::DimensionalInspection));
        assert!(!QcReportTemplate::CocOnly.permits(QcReportKind::As9102Fair));
        assert!(!QcReportTemplate::AbenStandard.permits(QcReportKind::As9102Fair));
        assert!(QcReportTemplate::As9102RevC.permits(QcReportKind::As9102Fair));
        assert!(QcReportTemplate::As9102RevC.permits(QcReportKind::DimensionalInspection));
    }
}
