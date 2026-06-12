//! MIL-STD-130N Item Unique Identifier (IUID / UID) format types.
//!
//! DoD 5000.64 and MIL-STD-130N require that serially-managed items carry a
//! globally-unique, machine-readable Item Unique Identifier (the "UII" / UID)
//! marked on the item, so a part's pedigree can be resolved for the life of the
//! item. The UII is built from a small set of data elements and rendered as a
//! concatenated reference string (an "IRI" — item reference identifier in the
//! IUID registry sense). MIL-STD-130N defines two valid UII *constructs*:
//!
//! - **Construct 1** — Issuing Agency Code + Enterprise Identifier + Serial
//!   Number (NO part number). The serial is unique across *all* items that
//!   enterprise produces.
//! - **Construct 2** — Issuing Agency Code + Enterprise Identifier + Original
//!   Part Number + Serial Number. The serial is unique *within the part number*
//!   for that enterprise.
//!
//! Source: MIL-STD-130N and the DoD *Guide to Uniquely Identifying Items* (the
//! IUID "UII constructs" appendix) — Construct #1 is the serial-only form (used
//! when the enterprise guarantees serial uniqueness across all of its items);
//! Construct #2 adds the original part number (used when the serial is unique
//! only within a part number). S366 review F5 corrected the S358 swap: the two
//! constructs had been defined in reverse against the standard.
//!
//! The Issuing Agency Code (IAC, per ISO/IEC 15459) names the registration
//! authority that guarantees the Enterprise Identifier's uniqueness — for a
//! DoD contractor the EID is typically a CAGE code or a DUNS number, both
//! reachable through their registered IACs.
//!
//! S358 ships the validated UID model only. The firing site that constructs an
//! [`Iuid`] from captured shop-floor data and records a `part.uid_marked` audit
//! event (ADR-0075) lands in a later session, so an invalid IAC / enterprise
//! id / serial can never reach the ledger.
//!
//! The validation follows the same defensive newtype pattern as the S345
//! [`crate::lot_heat`] `LotId` / `HeatId` types: a value of one of these types
//! cannot exist in an invalid state through its constructor path.

use serde::{Deserialize, Serialize};

/// Maximum length of an Issuing Agency Code, in characters.
///
/// The ISO/IEC 15459 issuing agency codes are short — one or two characters
/// (e.g. `D` for the Dun & Bradstreet / DUNS authority, `UN`, `LH`). Two is
/// the ceiling that admits every registered IAC while rejecting a pasted blob.
pub const MAX_IAC_LEN: usize = 2;

/// Maximum length of a UID data field (enterprise id, part number, serial), in
/// characters.
///
/// CAGE codes are 5 chars, DUNS numbers 9–13; part numbers and serials run
/// longer. 50 is a generous ceiling that rejects accidental free-text while
/// admitting realistic part/serial strings.
pub const MAX_UID_FIELD_LEN: usize = 50;

/// Validation failure for a UID component or an [`Iuid`] construct.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum UidError {
    /// A required field was empty.
    #[error("UID field {field} must not be empty")]
    Empty {
        /// Which field was empty (e.g. `"iac"`, `"serial"`).
        field: &'static str,
    },
    /// A field exceeded its length ceiling.
    #[error("UID field {field} is {len} chars, exceeds the {max}-char limit")]
    TooLong {
        /// Which field was too long.
        field: &'static str,
        /// The offending length.
        len: usize,
        /// The ceiling that was exceeded.
        max: usize,
    },
    /// A field contained a character outside its allowed set.
    #[error("UID field {field} contains invalid character {ch:?}")]
    InvalidChar {
        /// Which field held the invalid character.
        field: &'static str,
        /// The first offending character.
        ch: char,
    },
}

/// Validate a UID data field: non-empty, ≤ [`MAX_UID_FIELD_LEN`] chars,
/// `[A-Za-z0-9-]` only (no whitespace, underscores, or symbols) — the same
/// permitted set as [`crate::lot_heat`]'s lot/heat ids.
fn validate_field(field: &'static str, raw: &str) -> Result<(), UidError> {
    if raw.is_empty() {
        return Err(UidError::Empty { field });
    }
    let len = raw.chars().count();
    if len > MAX_UID_FIELD_LEN {
        return Err(UidError::TooLong {
            field,
            len,
            max: MAX_UID_FIELD_LEN,
        });
    }
    if let Some(ch) = raw
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || *c == '-'))
    {
        return Err(UidError::InvalidChar { field, ch });
    }
    Ok(())
}

/// Validate an Issuing Agency Code (ISO/IEC 15459): non-empty, ≤
/// [`MAX_IAC_LEN`] chars, uppercase ASCII alphanumeric (`A-Z0-9`) only.
///
/// MIL-STD-130N admits several issuing agencies behind their registered IACs —
/// CAGE codes, DUNS numbers, and manufacturer-assigned identifiers each resolve
/// through an IAC. This gate checks the IAC's *format* (it is the short
/// registration-authority code, not the enterprise id itself); whether a given
/// code is currently registered is an external-registry question out of scope
/// here. Uppercase is enforced because IACs are canonically uppercase.
pub fn validate_iac(s: &str) -> Result<(), UidError> {
    if s.is_empty() {
        return Err(UidError::Empty { field: "iac" });
    }
    let len = s.chars().count();
    if len > MAX_IAC_LEN {
        return Err(UidError::TooLong {
            field: "iac",
            len,
            max: MAX_IAC_LEN,
        });
    }
    if let Some(ch) = s
        .chars()
        .find(|c| !(c.is_ascii_uppercase() || c.is_ascii_digit()))
    {
        return Err(UidError::InvalidChar { field: "iac", ch });
    }
    Ok(())
}

/// A validated MIL-STD-130N **Construct 1** UII: Issuing Agency Code +
/// Enterprise Identifier + Serial Number (NO part number).
///
/// Construct via [`IuidConstruct1::new`]; the inner fields are private so the
/// type cannot exist in an invalid state through the constructor path. The
/// serial is unique across *all* items the enterprise produces (no part number
/// participates in the UII), per the DoD IUID Guide's Construct #1 definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IuidConstruct1 {
    iac: String,
    eid: String,
    serial: String,
}

impl IuidConstruct1 {
    /// Validate every component and wrap. The IAC is checked by
    /// [`validate_iac`]; the enterprise id and serial by the shared UID-field
    /// rules.
    pub fn new(
        iac: impl Into<String>,
        eid: impl Into<String>,
        serial: impl Into<String>,
    ) -> Result<Self, UidError> {
        let iac = iac.into();
        let eid = eid.into();
        let serial = serial.into();
        validate_iac(&iac)?;
        validate_field("eid", &eid)?;
        validate_field("serial", &serial)?;
        Ok(Self { iac, eid, serial })
    }

    /// The Issuing Agency Code.
    pub fn iac(&self) -> &str {
        &self.iac
    }

    /// The Enterprise Identifier (CAGE / DUNS / manufacturer id).
    pub fn eid(&self) -> &str {
        &self.eid
    }

    /// The serial number (unique across all items of the enterprise).
    pub fn serial(&self) -> &str {
        &self.serial
    }

    /// Render the UII as its concatenated reference string:
    /// `IAC + EID + Serial`.
    pub fn to_iri(&self) -> String {
        format!("{}{}{}", self.iac, self.eid, self.serial)
    }
}

/// A validated MIL-STD-130N **Construct 2** UII: Issuing Agency Code +
/// Enterprise Identifier + Original Part Number + Serial Number.
///
/// Same defensive pattern as [`IuidConstruct1`]. The serial is unique *within
/// the part number* for the enterprise (the part number participates in the
/// UII), per the DoD IUID Guide's Construct #2 definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IuidConstruct2 {
    iac: String,
    eid: String,
    original_part_number: String,
    serial: String,
}

impl IuidConstruct2 {
    /// Validate every component and wrap. See [`IuidConstruct1::new`] for the
    /// per-field rules; this construct additionally validates the original part
    /// number.
    pub fn new(
        iac: impl Into<String>,
        eid: impl Into<String>,
        original_part_number: impl Into<String>,
        serial: impl Into<String>,
    ) -> Result<Self, UidError> {
        let iac = iac.into();
        let eid = eid.into();
        let original_part_number = original_part_number.into();
        let serial = serial.into();
        validate_iac(&iac)?;
        validate_field("eid", &eid)?;
        validate_field("original_part_number", &original_part_number)?;
        validate_field("serial", &serial)?;
        Ok(Self {
            iac,
            eid,
            original_part_number,
            serial,
        })
    }

    /// The Issuing Agency Code.
    pub fn iac(&self) -> &str {
        &self.iac
    }

    /// The Enterprise Identifier (CAGE / DUNS / manufacturer id).
    pub fn eid(&self) -> &str {
        &self.eid
    }

    /// The original part number.
    pub fn original_part_number(&self) -> &str {
        &self.original_part_number
    }

    /// The serial number (unique within the part number).
    pub fn serial(&self) -> &str {
        &self.serial
    }

    /// Render the UII as its concatenated reference string:
    /// `IAC + EID + Original Part Number + Serial`.
    pub fn to_iri(&self) -> String {
        format!(
            "{}{}{}{}",
            self.iac, self.eid, self.original_part_number, self.serial
        )
    }
}

/// A validated MIL-STD-130N Item Unique Identifier in one of its two valid
/// constructs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Iuid {
    /// IAC + EID + Serial (no part number).
    Construct1(IuidConstruct1),
    /// IAC + EID + Original Part Number + Serial.
    Construct2(IuidConstruct2),
}

impl Iuid {
    /// Render the UII reference string for whichever construct this is.
    pub fn to_iri(&self) -> String {
        match self {
            Iuid::Construct1(c) => c.to_iri(),
            Iuid::Construct2(c) => c.to_iri(),
        }
    }

    /// The construct discriminator string used in the `part.uid_marked` audit
    /// payload's `uid_construct_code` field (ADR-0075).
    pub fn construct_code(&self) -> &'static str {
        match self {
            Iuid::Construct1(_) => "construct_1",
            Iuid::Construct2(_) => "construct_2",
        }
    }
}

#[cfg(test)]
mod tests;
