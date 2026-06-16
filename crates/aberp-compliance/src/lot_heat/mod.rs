//! Lot / heat material traceability seed types (AS9100D §8.5.2).
//!
//! Aerospace material traceability requires that every piece of stock be
//! traceable back to its mill heat / lot, with the mill certificate, country
//! of origin, and melt date retained for the life of the part. ABERP's
//! commercial core treats material as a fungible per-grade scalar
//! (`material_inventory.rs`) — this module introduces the *identity* types
//! that the traceability spine (S348) attaches to receipts and consumption.
//!
//! S345 ships the validated [`LotId`] / [`HeatId`] newtypes and the
//! [`MaterialTraceabilitySeed`] record. The actual capture wiring (receiving,
//! mill-cert upload, consumption linkage) lands in S348.

use serde::{Deserialize, Serialize};

/// Maximum length of a lot / heat identifier, in characters.
///
/// Mill heat numbers and lot codes are short alphanumeric strings; 32 is a
/// generous ceiling that rejects accidental free-text / pasted blobs.
pub const MAX_ID_LEN: usize = 32;

/// Validation failure for a [`LotId`] / [`HeatId`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TraceabilityError {
    /// The identifier was empty.
    #[error("traceability id must not be empty")]
    Empty,
    /// The identifier exceeded [`MAX_ID_LEN`] characters.
    #[error("traceability id is {len} chars, exceeds the {MAX_ID_LEN}-char limit")]
    TooLong {
        /// The offending length.
        len: usize,
    },
    /// The identifier contained a character outside `[A-Za-z0-9-]`.
    #[error("traceability id contains invalid character {ch:?} (allowed: alphanumeric and '-')")]
    InvalidChar {
        /// The first offending character.
        ch: char,
    },
}

/// Validate a candidate lot/heat id: non-empty, ≤ [`MAX_ID_LEN`] chars,
/// `[A-Za-z0-9-]` only (no whitespace, no underscores, no symbols).
fn validate(raw: &str) -> Result<(), TraceabilityError> {
    if raw.is_empty() {
        return Err(TraceabilityError::Empty);
    }
    // Length in characters (not bytes) — ids are ASCII in practice but we do
    // not assume it.
    let len = raw.chars().count();
    if len > MAX_ID_LEN {
        return Err(TraceabilityError::TooLong { len });
    }
    if let Some(ch) = raw
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || *c == '-'))
    {
        return Err(TraceabilityError::InvalidChar { ch });
    }
    Ok(())
}

/// Maximum length of a Mill Test Report (MTR) URL, in characters. Generous —
/// `file://` paths can be long, but a multi-KB blob is a paste accident.
pub const MAX_MTR_URL_LEN: usize = 512;

/// Validation failure for a Mill Test Report URL.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MtrUrlError {
    /// Exceeded [`MAX_MTR_URL_LEN`] characters.
    #[error("MTR url is {len} chars, exceeds the {MAX_MTR_URL_LEN}-char limit")]
    TooLong {
        /// The offending length.
        len: usize,
    },
    /// A non-empty value that did not start with `file://`. S432 v1 retains MTR
    /// documents on the local filesystem only; remote URLs are a later slice.
    #[error("MTR url must start with file:// (got {got:?})")]
    NotFileScheme {
        /// The first 32 chars of the offending value, for the operator toast.
        got: String,
    },
}

/// Validate an optional Mill Test Report URL per the S432 firing-site rule:
/// empty/whitespace is allowed (the MTR may lag the heat-lot binding), but a
/// non-empty value MUST start with `file://`. Returns the trimmed value (or
/// `None` when empty) so the caller stores a canonical form.
///
/// Pure — no I/O. The path is NOT checked for existence (the file may be on an
/// operator workstation the server cannot see); this gate only constrains the
/// scheme + length so a forged remote URL can never reach the ledger.
pub fn validate_mtr_url(raw: &str) -> Result<Option<String>, MtrUrlError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let len = trimmed.chars().count();
    if len > MAX_MTR_URL_LEN {
        return Err(MtrUrlError::TooLong { len });
    }
    if !trimmed.starts_with("file://") {
        return Err(MtrUrlError::NotFileScheme {
            got: trimmed.chars().take(32).collect(),
        });
    }
    Ok(Some(trimmed.to_string()))
}

/// A validated material lot identifier.
///
/// Construct via [`LotId::new`]; the inner string is private so a `LotId`
/// cannot exist in an invalid state through the constructor path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LotId(String);

impl LotId {
    /// Validate and wrap a lot id. See [`validate`] for the rules.
    pub fn new(raw: impl Into<String>) -> Result<Self, TraceabilityError> {
        let raw = raw.into();
        validate(&raw)?;
        Ok(Self(raw))
    }

    /// The validated id string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A validated mill heat identifier.
///
/// Same validation rules as [`LotId`]; a distinct type so a heat number can
/// never be passed where a lot id is expected, and vice versa.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeatId(String);

impl HeatId {
    /// Validate and wrap a heat id. See [`validate`] for the rules.
    pub fn new(raw: impl Into<String>) -> Result<Self, TraceabilityError> {
        let raw = raw.into();
        validate(&raw)?;
        Ok(Self(raw))
    }

    /// The validated id string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The traceability metadata captured when material enters inventory.
///
/// `mill_cert_id`, `country_of_origin`, and `melt_date` are optional because
/// they arrive at different points (the cert may lag the physical receipt).
/// The lot + heat ids are mandatory — they are the traceability anchor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaterialTraceabilitySeed {
    /// The material lot.
    pub lot: LotId,
    /// The mill heat the lot was poured from.
    pub heat: HeatId,
    /// Opaque id of the mill test certificate (CoC / 3.1 cert), once captured.
    pub mill_cert_id: Option<String>,
    /// ISO 3166-1 alpha-2 country of origin, once known.
    pub country_of_origin: Option<String>,
    /// Melt date as Unix-epoch milliseconds, once known.
    pub melt_date: Option<u64>,
}

#[cfg(test)]
mod tests;
