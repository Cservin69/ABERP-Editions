//! Export-control classification + denied-party screening (ITAR / EAR).
//!
//! Two distinct compliance questions live here:
//!
//! 1. **Classification** — *what is this item?* An exported part / technical
//!    drawing / software carries an EAR ECCN, a USML category (ITAR), or the
//!    catch-all EAR99. Mis-classification is a felony, so the real answer
//!    comes from a licensed classification service / commodity-jurisdiction
//!    determination — never inferred here.
//! 2. **Screening** — *who is the party?* Every consignee / end-user is
//!    screened against the consolidated denied-party lists (BIS Entity List,
//!    OFAC SDN, State DDTC debarred, …). A hit blocks the shipment.
//!
//! S345 ships the [`ExportControlProvider`] trait (the swap-point) and one
//! implementation, [`MockExportControlProvider`], which answers
//! [`ExportClassification::NotClassified`] + [`ScreeningResult::Clear`] for
//! everything. The real backends slot in behind the same trait later.

mod mock;

pub use mock::MockExportControlProvider;

use serde::{Deserialize, Serialize};

/// The export-control classification of an item.
///
/// `ECCN` / `USMLCategory` carry the determined code string; `EAR99` is the
/// EAR catch-all (commercial items subject to the EAR but not on the Commerce
/// Control List); `NotClassified` means no determination has been made yet
/// (the mock's answer); `Pending` means a determination is in flight.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExportClassification {
    /// Export Control Classification Number (EAR / Commerce Control List),
    /// e.g. `"7A994"`. The string is the determined ECCN.
    #[allow(clippy::upper_case_acronyms)]
    ECCN(String),
    /// United States Munitions List category (ITAR / USML), e.g. `"VIII(h)"`.
    #[allow(clippy::upper_case_acronyms)]
    USMLCategory(String),
    /// EAR99 — subject to the EAR but not listed on the CCL.
    EAR99,
    /// No determination has been made.
    NotClassified,
    /// A determination is in progress.
    Pending,
}

/// The export-control **jurisdiction** (regulatory regime) an item falls
/// under — a distinct axis from [`ExportClassification`].
///
/// `ExportClassification` answers *"what is the code?"* (an ECCN string, a USML
/// category string, or the bare EAR99 catch-all). `Jurisdiction` answers *"which
/// body of law governs it?"* — the question the `export.classification_set`
/// audit event's `jurisdiction` field records. The two overlap only at EAR99
/// (which is both a classification and, trivially, an EAR-jurisdiction item), so
/// they are modelled separately rather than crammed into one enum: an
/// `ExportClassification::ITAR` variant would be a category error (ITAR is the
/// regime; the USML category is its classification).
///
/// S359 adds this typed enum so the audit firing site (later session) renders
/// the `jurisdiction` payload string through [`Jurisdiction::as_str`] — a
/// free-text regime can never reach the ledger. The storage strings are the
/// UPPER_SNAKE tokens the brief / ADR-0076 pin: `ITAR` / `EAR` / `EAR99` /
/// `NOT_CONTROLLED` / `UNKNOWN`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Jurisdiction {
    /// International Traffic in Arms Regulations (22 CFR §§ 120-130) — the item
    /// is on the United States Munitions List, controlled by the State
    /// Department's DDTC.
    Itar,
    /// Export Administration Regulations (15 CFR §§ 730-774) — the item is on
    /// the Commerce Control List, controlled by Commerce's BIS, and carries an
    /// ECCN.
    Ear,
    /// EAR99 — subject to the EAR but **not** listed on the CCL. The catch-all
    /// for most commercial items; usually exportable without a licence (subject
    /// to embargo / denied-party screening).
    Ear99,
    /// Determined to be neither ITAR- nor EAR-controlled (e.g. published / public-
    /// domain information, EAR § 734.7). A *positive* determination, distinct
    /// from [`Self::Unknown`].
    NotControlled,
    /// No determination has been made yet — the conservative default the mock
    /// boundary surfaces until a real classification service answers.
    Unknown,
}

impl Jurisdiction {
    /// Render in the on-disk / audit-payload form. Paired with
    /// [`Jurisdiction::from_storage_str`] as a round-trip-proven pair (the unit
    /// test below checks `from_storage_str(V.as_str()) == Ok(V)` for every
    /// variant), mirroring the audit-ledger `EventKind` round-trip discipline.
    pub fn as_str(&self) -> &'static str {
        match self {
            Jurisdiction::Itar => "ITAR",
            Jurisdiction::Ear => "EAR",
            Jurisdiction::Ear99 => "EAR99",
            Jurisdiction::NotControlled => "NOT_CONTROLLED",
            Jurisdiction::Unknown => "UNKNOWN",
        }
    }

    /// Parse the on-disk / audit-payload form back into a `Jurisdiction`.
    /// Errors on unknown strings — silent fallback would mask schema drift
    /// (CLAUDE.md rule 12, "fail loud").
    pub fn from_storage_str(s: &str) -> Result<Self, &'static str> {
        match s {
            "ITAR" => Ok(Jurisdiction::Itar),
            "EAR" => Ok(Jurisdiction::Ear),
            "EAR99" => Ok(Jurisdiction::Ear99),
            "NOT_CONTROLLED" => Ok(Jurisdiction::NotControlled),
            "UNKNOWN" => Ok(Jurisdiction::Unknown),
            _ => Err("unknown Jurisdiction storage string"),
        }
    }
}

/// An item that can be submitted for export classification.
///
/// The provider keys on a short, stable descriptor (part number, commodity
/// description, material grade). The trait is intentionally minimal —
/// classification is the provider's job, not the caller's.
pub trait Classifiable {
    /// A short, stable descriptor of the item — the key a classification
    /// service would dereference (part number, commodity description, …).
    fn classification_descriptor(&self) -> String;
}

/// A party (consignee / end-user / intermediate consignee) to be screened
/// against the denied-party lists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartyRef {
    /// Legal name as it appears on the order.
    pub name: String,
    /// ISO 3166-1 alpha-2 country code, when known — embargo screening keys
    /// on destination country as well as name.
    pub country: Option<String>,
}

/// The outcome of screening a [`PartyRef`] against the denied-party lists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScreeningResult {
    /// No match — the party is clear to transact with.
    Clear,
    /// A match that restricts (but does not outright deny) the transaction —
    /// e.g. requires a license. The string names the list / reason.
    Restricted(String),
    /// A denied-party match — the transaction must not proceed. The string
    /// names the list / reason.
    Denied(String),
}

/// Failure modes a [`ExportControlProvider`] can surface.
///
/// Typed (not stringly) so the boot/audit layer can branch — a backend that
/// is unconfigured is a different posture from one that is configured but
/// unreachable.
#[derive(Debug, thiserror::Error)]
pub enum ExportControlError {
    /// The classification/screening backend is not configured.
    #[error("export-control backend not configured")]
    NotConfigured,
    /// The backend is configured but could not be reached / answered.
    #[error("export-control backend unavailable: {0}")]
    BackendUnavailable(String),
}

/// The abstraction every export-sensitive operation will consult for
/// classification + denied-party screening.
///
/// `Send + Sync` so a single `Arc<dyn ExportControlProvider>` can be shared
/// into `AppState` across every handler + daemon, the same way the S344
/// `DigitalIdProvider` is shared.
pub trait ExportControlProvider: Send + Sync {
    /// Short backend tag, e.g. `"mock"`, `"bis-api"`. Used in the boot log
    /// line and as a fast discriminator in tests.
    fn name(&self) -> &str;

    /// Determine the export classification of an item.
    fn classify(&self, item: &dyn Classifiable)
        -> Result<ExportClassification, ExportControlError>;

    /// Screen a party against the denied-party lists.
    fn screen_party(&self, party: &PartyRef) -> Result<ScreeningResult, ExportControlError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// S359 — round-trip every `Jurisdiction` variant through the storage form,
    /// mirroring the audit-ledger `EventKind` discipline. A future contributor
    /// who adds a variant + `as_str` arm but forgets `from_storage_str` fails
    /// here, not against a production audit row.
    #[test]
    fn s359_jurisdiction_round_trips_every_variant() {
        for j in [
            Jurisdiction::Itar,
            Jurisdiction::Ear,
            Jurisdiction::Ear99,
            Jurisdiction::NotControlled,
            Jurisdiction::Unknown,
        ] {
            let s = j.as_str();
            assert_eq!(
                Jurisdiction::from_storage_str(s).expect("round-trip"),
                j,
                "round-trip mismatch for {s}"
            );
        }
    }

    /// S359 — pin the exact UPPER_SNAKE tokens the brief / ADR-0076 / the
    /// `export.classification_set` payload `jurisdiction` field depend on.
    #[test]
    fn s359_jurisdiction_storage_tokens_are_pinned() {
        assert_eq!(Jurisdiction::Itar.as_str(), "ITAR");
        assert_eq!(Jurisdiction::Ear.as_str(), "EAR");
        assert_eq!(Jurisdiction::Ear99.as_str(), "EAR99");
        assert_eq!(Jurisdiction::NotControlled.as_str(), "NOT_CONTROLLED");
        assert_eq!(Jurisdiction::Unknown.as_str(), "UNKNOWN");
    }

    /// S359 — unknown strings must fail loud, never silently fall through to a
    /// default regime (a mis-parse to `NotControlled` would be the worst-class
    /// silent-omission bug for an export-control field).
    #[test]
    fn s359_jurisdiction_rejects_unknown() {
        assert!(Jurisdiction::from_storage_str("ear").is_err());
        assert!(Jurisdiction::from_storage_str("").is_err());
        assert!(Jurisdiction::from_storage_str("DUAL_USE").is_err());
    }

    /// S359 — `Jurisdiction` also survives a serde JSON round-trip (it derives
    /// `Serialize`/`Deserialize` for callers that embed it in typed structs;
    /// the audit payload uses the `as_str` form, but the derive must stay sound).
    #[test]
    fn s359_jurisdiction_serde_round_trips() {
        for j in [
            Jurisdiction::Itar,
            Jurisdiction::Ear,
            Jurisdiction::Ear99,
            Jurisdiction::NotControlled,
            Jurisdiction::Unknown,
        ] {
            let json = serde_json::to_string(&j).expect("serialize");
            let back: Jurisdiction = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(j, back);
        }
    }
}
