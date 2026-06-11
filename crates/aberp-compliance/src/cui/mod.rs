//! Controlled Unclassified Information (CUI) marking + classification levels.
//!
//! A [`CuiMarking`] tags a record / document / drawing with its sensitivity
//! so downstream handling (storage, transmission, marking on PDFs, access
//! control) can enforce the right safeguards. The vocabulary spans the
//! unclassified-but-controlled band (CUI, with a category from the DoD CUI
//! Registry) and the national-security classification levels.
//!
//! S345 ships the enums + marking helpers only. Wiring a marking onto a
//! record and enforcing handling rules lands later.
//!
//! S360 (ADR-0077) extends the marking helpers with [`DisseminationControl`]
//! and [`CuiMarking::to_banner_str`] so the `cui.marking_applied` audit payload
//! can carry the full DoD banner — base marking plus the limited-dissemination
//! segment (`CUI//CTI//NOFORN`).

use serde::{Deserialize, Serialize};

/// The sensitivity marking of a record.
///
/// Ordered least → most sensitive. `Cui` carries the specific category from
/// the DoD CUI Registry; the three classification variants are the national
/// security levels.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CuiMarking {
    /// No control required.
    Unclassified,
    /// Controlled Unclassified Information, with its registry category.
    Cui(CuiCategory),
    /// Classified — Confidential.
    Confidential,
    /// Classified — Secret.
    Secret,
    /// Classified — Top Secret.
    TopSecret,
}

/// A CUI category from the DoD CUI Registry (the most common organizational
/// index groupings). The banner marking renders as `CUI//<abbrev>`.
///
/// This is a deliberate starter subset, not the full registry — S346+ extend
/// it as real flowdowns demand specific categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CuiCategory {
    /// Controlled Technical Information.
    Cti,
    /// Privacy.
    Prvcy,
    /// Export Control.
    Expt,
    /// Critical Infrastructure.
    Crit,
    /// Law Enforcement.
    Lei,
    /// Intelligence.
    Ifg,
    /// Information Systems Vulnerability Information (general infrastructure).
    Inf,
    /// Information Systems Vulnerability Information.
    Isvi,
    /// Procurement and Acquisition.
    Proc,
    /// Proprietary Business Information.
    Prop,
}

impl CuiCategory {
    /// The banner abbreviation as it appears after the `CUI//` control marking,
    /// e.g. `CUI//SP-EXPT`-style index groupings collapse here to the registry
    /// abbreviation (`CTI`, `PRVCY`, `EXPT`, …).
    pub fn abbreviation(self) -> &'static str {
        match self {
            CuiCategory::Cti => "CTI",
            CuiCategory::Prvcy => "PRVCY",
            CuiCategory::Expt => "EXPT",
            CuiCategory::Crit => "CRIT",
            CuiCategory::Lei => "LEI",
            CuiCategory::Ifg => "IFG",
            CuiCategory::Inf => "INF",
            CuiCategory::Isvi => "ISVI",
            CuiCategory::Proc => "PROC",
            CuiCategory::Prop => "PROP",
        }
    }
}

/// A limited-dissemination control marking — the trailing `//<DISSEM>` segment
/// of a DoD banner (`CUI//CTI//NOFORN`). These constrain *who* may receive the
/// artifact, orthogonal to the category that says *what kind* of CUI it is.
///
/// A deliberate starter subset of the registry's limited-dissemination
/// controls, not the full set — S360+ extend it as real flowdowns demand
/// specific controls (mirrors the [`CuiCategory`] starter-subset posture).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DisseminationControl {
    /// No foreign nationals (no dissemination to non-U.S. persons).
    NoForn,
    /// Federal employees and contractors.
    FedCon,
    /// No dissemination to contractors.
    NoCon,
    /// Dissemination list controlled — to those on an explicit list only.
    DlOnly,
}

impl DisseminationControl {
    /// The banner abbreviation as it appears in the trailing `//` segment, e.g.
    /// `NOFORN`, `FEDCON`, `NOCON`, `DL ONLY`.
    pub fn abbreviation(self) -> &'static str {
        match self {
            DisseminationControl::NoForn => "NOFORN",
            DisseminationControl::FedCon => "FEDCON",
            DisseminationControl::NoCon => "NOCON",
            DisseminationControl::DlOnly => "DL ONLY",
        }
    }
}

impl CuiMarking {
    /// `true` only for the [`CuiMarking::Cui`] band — controlled but
    /// unclassified.
    pub fn is_cui(&self) -> bool {
        matches!(self, CuiMarking::Cui(_))
    }

    /// `true` for the national-security classification levels (Confidential
    /// and above). `Unclassified` and `Cui` are NOT classified.
    pub fn is_classified(&self) -> bool {
        matches!(
            self,
            CuiMarking::Confidential | CuiMarking::Secret | CuiMarking::TopSecret
        )
    }

    /// The banner marking string per DoD marking conventions:
    /// `UNCLASSIFIED`, `CUI//<ABBREV>`, `CONFIDENTIAL`, `SECRET`,
    /// `TOP SECRET`.
    pub fn display_marking(&self) -> String {
        match self {
            CuiMarking::Unclassified => "UNCLASSIFIED".to_string(),
            CuiMarking::Cui(cat) => format!("CUI//{}", cat.abbreviation()),
            CuiMarking::Confidential => "CONFIDENTIAL".to_string(),
            CuiMarking::Secret => "SECRET".to_string(),
            CuiMarking::TopSecret => "TOP SECRET".to_string(),
        }
    }

    /// The full DoD banner per 32 CFR Part 2002: the base marking from
    /// [`Self::display_marking`] followed by a trailing limited-dissemination
    /// segment when any controls are supplied —
    /// `<MARKING>//<DISSEM1>/<DISSEM2>` (e.g. `CUI//CTI//NOFORN`,
    /// `CUI//PRVCY//FEDCON/NOFORN`). With no dissemination controls this is
    /// exactly [`Self::display_marking`], so an empty slice is the honest
    /// "category banner, no further limits" form.
    ///
    /// This is the string the `cui.marking_applied` audit payload's
    /// `cui_marking_str` carries — rendered from typed values so a free-text
    /// banner can never reach the ledger.
    pub fn to_banner_str(&self, dissemination: &[DisseminationControl]) -> String {
        let base = self.display_marking();
        if dissemination.is_empty() {
            return base;
        }
        let dissem = dissemination
            .iter()
            .map(|d| d.abbreviation())
            .collect::<Vec<_>>()
            .join("/");
        format!("{base}//{dissem}")
    }
}

#[cfg(test)]
mod tests;
