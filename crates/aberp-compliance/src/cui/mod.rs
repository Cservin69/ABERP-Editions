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
//! segment (`CUI//SP-CTI//NOFORN` for a CUI Specified category like CTI;
//! `CUI//PRVCY//NOFORN` for a CUI Basic one).

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
    /// The bare registry abbreviation (`CTI`, `PRVCY`, `EXPT`, …) — the category
    /// code WITHOUT the `SP-` Specified prefix. The banner-rendering path
    /// ([`CuiMarking::display_marking`]) prepends `SP-` for [`Self::is_specified`]
    /// categories; this returns the code alone.
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

    /// `true` if this is a CUI **Specified** category (vs CUI Basic) per the DoD
    /// CUI Registry. A Specified category is governed by a law / regulation /
    /// government-wide policy that prescribes specific controls beyond CUI
    /// Basic, and its banner marking takes the `SP-` prefix — `CUI//SP-CTI`,
    /// not `CUI//CTI` (DoD CUI Marking Handbook; 32 CFR 2002.20).
    ///
    /// Conservative subset: only the categories confirmed Specified against the
    /// registry are flagged — `Cti` (Controlled Technical Information; DoDI
    /// 5230.24 / DFARS) and `Expt` (Export Controlled; ITAR / EAR). The
    /// remainder are treated as CUI Basic until a registry row demands
    /// otherwise (S360 starter-subset posture — extend as real flowdowns
    /// require). S366 review F10 corrected the prior behaviour, which dropped
    /// the `SP-` prefix for every category.
    pub fn is_specified(self) -> bool {
        matches!(self, CuiCategory::Cti | CuiCategory::Expt)
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
    /// `UNCLASSIFIED`, `CUI//<ABBREV>` (or `CUI//SP-<ABBREV>` for a CUI
    /// Specified category), `CONFIDENTIAL`, `SECRET`, `TOP SECRET`.
    ///
    /// The `SP-` prefix on Specified categories ([`CuiCategory::is_specified`])
    /// is load-bearing: `CUI//SP-CTI` and `CUI//CTI` are different markings to a
    /// DoD recipient, and this string can be printed verbatim onto a CDRL
    /// deliverable's banner.
    pub fn display_marking(&self) -> String {
        match self {
            CuiMarking::Unclassified => "UNCLASSIFIED".to_string(),
            CuiMarking::Cui(cat) => {
                let prefix = if cat.is_specified() { "SP-" } else { "" };
                format!("CUI//{prefix}{}", cat.abbreviation())
            }
            CuiMarking::Confidential => "CONFIDENTIAL".to_string(),
            CuiMarking::Secret => "SECRET".to_string(),
            CuiMarking::TopSecret => "TOP SECRET".to_string(),
        }
    }

    /// The full DoD banner per 32 CFR Part 2002: the base marking from
    /// [`Self::display_marking`] (which carries the `SP-` prefix for CUI
    /// Specified categories) followed by a trailing limited-dissemination
    /// segment when any controls are supplied —
    /// `<MARKING>//<DISSEM1>/<DISSEM2>` (e.g. `CUI//SP-CTI//NOFORN`,
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
