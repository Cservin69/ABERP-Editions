//! Approved Vendor List (AVL) + DPAS priority rating types.
//!
//! Aerospace / defense procurement constrains *who* you may buy from: a
//! supplier must be qualified (AS9100D §8.4 supplier control), and a defense
//! order may carry a DPAS priority rating (FAR 11.6 / DPAS regulation 15 CFR
//! 700) that the supplier must acknowledge and prioritize. ABERP's commercial
//! core models a supplier as a 3-value `PartnerKind` flag — this module
//! introduces the qualification + rating + screening-status fields the AVL
//! (S347) attaches to a partner.
//!
//! S345 ships the enums + the [`ApprovedSupplierEntry`] record only.

use serde::{Deserialize, Serialize};

/// Reference to a partner in ABERP's partner master data.
///
/// A local newtype rather than a dependency on `apps/aberp` — this crate is
/// the leaf, the wiring layer (S347) maps it to the real partner id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartnerRef(pub String);

/// The rating symbol (priority level) of a DPAS-rated order — 15 CFR 700.12.
///
/// `Dx` outranks `Do`; both outrank an unrated commercial order (represented by
/// the *absence* of a [`DpasRating`], i.e. `Option::None`, not a variant here —
/// a DPAS rating that exists always carries a priority).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DpasPriority {
    /// DO — the lower of the two defense priorities.
    Do,
    /// DX — the higher defense priority; takes precedence over DO.
    Dx,
}

impl DpasPriority {
    /// The two-letter rating symbol as it appears in a rating string (`DO` /
    /// `DX`).
    pub fn as_str(self) -> &'static str {
        match self {
            DpasPriority::Do => "DO",
            DpasPriority::Dx => "DX",
        }
    }
}

impl std::fmt::Display for DpasPriority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Validation failure for a DPAS rating string or program-identification symbol.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProgramSymbolError {
    /// The program identification symbol was not the Schedule I `[A-F][1-9]`
    /// shape.
    #[error(
        "DPAS program symbol must be a letter A-F followed by a digit 1-9 \
         (15 CFR 700 Schedule I), got {0:?}"
    )]
    BadSymbol(String),
    /// The rating string was not `<DO|DX>-<program symbol>`.
    #[error("DPAS rating must be <DO|DX>-<program symbol> (e.g. \"DO-A1\"), got {0:?}")]
    BadFormat(String),
    /// The rating symbol was neither `DO` nor `DX`.
    #[error("DPAS rating symbol must be DO or DX, got {0:?}")]
    BadPriority(String),
}

/// A DPAS priority rating carried by a defense order (FAR 11.604 / 15 CFR
/// 700.12).
///
/// A rating is a *rating symbol* (`DO` or `DX`, [`DpasPriority`]) joined to a
/// *program identification symbol* from 15 CFR 700 Schedule I (e.g. `A1`
/// aircraft, `A7` radar / electronics, `C1`…) by a hyphen — `DO-A1`, `DX-A7`.
/// The program-symbol space is the whole of Schedule I, which a closed enum
/// cannot enumerate; this is a validated newtype over the `[A-F][1-9]` symbol
/// shape instead.
///
/// S366 review F13 replaced the prior closed `{None, DoC1, DxC1}` enum, which
/// could not represent `DO-A1` — the canonical aircraft-program rating, and 15
/// CFR 700's own worked example. An *unrated* commercial order is the absence
/// of a rating (`Option<DpasRating>::None`), not a variant here.
///
/// Build via [`DpasRating::new`] / [`DpasRating::parse`] (both validate the
/// program symbol) — the fields are public for ergonomics, but write boundaries
/// must route through the constructors so a malformed symbol never reaches the
/// ledger or the `dpas_rating` column (S366 review F14).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DpasRating {
    /// The rating symbol — `DO` or `DX`.
    pub priority: DpasPriority,
    /// The 15 CFR 700 Schedule I program identification symbol (validated
    /// `[A-F][1-9]`, e.g. `A1`).
    pub program_symbol: String,
}

impl DpasRating {
    /// Construct from a priority + program symbol, validating the symbol shape.
    pub fn new(
        priority: DpasPriority,
        program_symbol: impl Into<String>,
    ) -> Result<Self, ProgramSymbolError> {
        let program_symbol = program_symbol.into();
        Self::validate_program_symbol(&program_symbol)?;
        Ok(Self {
            priority,
            program_symbol,
        })
    }

    /// Render in the on-disk / audit-payload form — the canonical string the
    /// S361 `supplier.dpas_priority_set` firing site writes and the partners
    /// `dpas_rating` column stores (`DO-A1`, `DX-A7`). Paired with
    /// [`DpasRating::parse`] as a round-trip-proven pair, mirroring the
    /// export-control [`crate::export_control::Jurisdiction`] discipline (S359).
    pub fn as_str(&self) -> String {
        format!("{}-{}", self.priority, self.program_symbol)
    }

    /// Parse the on-disk / audit-payload form (`DO-A1`) back into a
    /// `DpasRating`. Errors on anything that is not `<DO|DX>-<[A-F][1-9]>` —
    /// fail loud (CLAUDE.md rule 12); a silent fallback would strip or corrupt
    /// a defense order's priority.
    pub fn parse(s: &str) -> Result<Self, ProgramSymbolError> {
        let (sym, prog) = s
            .split_once('-')
            .ok_or_else(|| ProgramSymbolError::BadFormat(s.to_string()))?;
        let priority = match sym {
            "DO" => DpasPriority::Do,
            "DX" => DpasPriority::Dx,
            _ => return Err(ProgramSymbolError::BadPriority(sym.to_string())),
        };
        Self::validate_program_symbol(prog)?;
        Ok(Self {
            priority,
            program_symbol: prog.to_string(),
        })
    }

    /// Validate a program identification symbol against the 15 CFR 700 Schedule
    /// I shape: exactly two characters, a letter `A-F` then a digit `1-9`.
    pub fn validate_program_symbol(s: &str) -> Result<(), ProgramSymbolError> {
        let bytes = s.as_bytes();
        if bytes.len() == 2
            && (b'A'..=b'F').contains(&bytes[0])
            && (b'1'..=b'9').contains(&bytes[1])
        {
            Ok(())
        } else {
            Err(ProgramSymbolError::BadSymbol(s.to_string()))
        }
    }
}

/// The outcome of screening a supplier against the export-control denied-party
/// lists — the stored status on the AVL entry (S361, ADR-0078).
///
/// Distinct from [`crate::export_control::ScreeningResult`]: that is the typed
/// *adjudication* of a single screening call (clear / restricted-with-reason /
/// denied-with-reason); this is the *stored screening-outcome status* on the
/// AVL entry, which can be `NotScreened` before the first screen runs.
///
/// S361 reshapes the S345 scaffold vocabulary onto the denial-list-screening
/// outcome the BIS Consolidated Screening List / OFAC / State DDTC actually
/// return — `Clear` (no match), `Hit` (a denied-party match), `Inconclusive`
/// (a partial / common-name match needing manual review). The placeholder
/// `Restricted` / `Denied` variants the scaffold guessed are dropped: the
/// restricted-vs-denied *adjudication* is the job of
/// [`crate::export_control::ScreeningResult`], not the stored AVL status. These
/// are the exact tokens the `supplier.export_screened` payload `screening_result`
/// field and the partners `export_screening_status` column carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ExportScreeningStatus {
    /// No screening has been performed yet.
    #[default]
    NotScreened,
    /// Screened clear — no denied-party match.
    Clear,
    /// Screened to a denied-party match (e.g. BIS Entity List / OFAC SDN) —
    /// must not transact until adjudicated.
    Hit,
    /// Screened with an inconclusive result — a partial / common-name match
    /// that needs manual review before the supplier may transact.
    Inconclusive,
}

impl ExportScreeningStatus {
    /// Render in the on-disk / audit-payload form — the canonical string the
    /// S361 `supplier.export_screened` firing site writes and the partners
    /// `export_screening_status` column stores. Round-trip-proven with
    /// [`ExportScreeningStatus::from_storage_str`].
    pub fn as_str(&self) -> &'static str {
        match self {
            ExportScreeningStatus::NotScreened => "not_screened",
            ExportScreeningStatus::Clear => "clear",
            ExportScreeningStatus::Hit => "hit",
            ExportScreeningStatus::Inconclusive => "inconclusive",
        }
    }

    /// Parse the on-disk / audit-payload form back into an
    /// `ExportScreeningStatus`. Errors on unknown strings — a silent fallback
    /// to `Clear` would be the worst-class export-control bug (it would mark an
    /// unscreened / hit supplier as clear to transact). Fail loud (CLAUDE.md
    /// rule 12).
    pub fn from_storage_str(s: &str) -> Result<Self, &'static str> {
        match s {
            "not_screened" => Ok(ExportScreeningStatus::NotScreened),
            "clear" => Ok(ExportScreeningStatus::Clear),
            "hit" => Ok(ExportScreeningStatus::Hit),
            "inconclusive" => Ok(ExportScreeningStatus::Inconclusive),
            _ => Err("unknown ExportScreeningStatus storage string"),
        }
    }
}

/// The qualification level of a supplier on the AVL (AS9100D §8.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum QualLevel {
    /// May be invited to bid, but not yet cleared to deliver.
    Bid,
    /// Fully qualified — may bid and deliver.
    Approved,
    /// Disapproved — do not use.
    Disapproved,
}

impl QualLevel {
    /// `true` if the supplier may be invited to bid. Both `Bid` and
    /// `Approved` may bid; `Disapproved` may not.
    pub fn can_bid(self) -> bool {
        matches!(self, QualLevel::Bid | QualLevel::Approved)
    }

    /// `true` only if the supplier is cleared to deliver. ONLY `Approved`
    /// suppliers may deliver.
    pub fn can_deliver(self) -> bool {
        matches!(self, QualLevel::Approved)
    }
}

/// An entry on the Approved Vendor List.
///
/// The qualification + DPAS + screening fields are the compliance overlay on
/// top of the commercial partner record; `last_audit_at_ms` is the supplier's
/// most recent qualification audit (AS9100D §8.4 re-evaluation cadence),
/// `None` until the first audit is recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovedSupplierEntry {
    /// The partner this AVL entry qualifies.
    pub partner_id: PartnerRef,
    /// Current qualification level.
    pub qualification_level: QualLevel,
    /// DPAS rating the supplier is approved to service; `None` = unrated
    /// commercial supplier.
    pub dpas: Option<DpasRating>,
    /// Stored export-screening status.
    pub screening: ExportScreeningStatus,
    /// Unix-epoch milliseconds of the last qualification audit, if any.
    pub last_audit_at_ms: Option<u64>,
}

#[cfg(test)]
mod tests;
