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

impl ExportClassification {
    /// The regulatory regime this determination implies.
    ///
    /// This is a *rendering* of the provider's own answer, NOT an inference
    /// about the item: an `ECCN` determination is by definition an EAR/CCL
    /// listing, a `USMLCategory` determination is by definition ITAR, and both
    /// `NotClassified` and `Pending` mean *no determination exists yet* →
    /// [`Jurisdiction::Unknown`], the conservative default the mock boundary
    /// surfaces. Mis-classification is a felony, so nothing here guesses; the
    /// only input is what a licensed classification service already said.
    ///
    /// S440 — the `export.classification_set` firing site renders the payload's
    /// `jurisdiction` field through this, so a free-text regime can never reach
    /// the ledger (the discipline [`Jurisdiction`]'s doc pins).
    pub fn jurisdiction(&self) -> Jurisdiction {
        match self {
            ExportClassification::ECCN(_) => Jurisdiction::Ear,
            ExportClassification::USMLCategory(_) => Jurisdiction::Itar,
            ExportClassification::EAR99 => Jurisdiction::Ear99,
            ExportClassification::NotClassified | ExportClassification::Pending => {
                Jurisdiction::Unknown
            }
        }
    }

    /// The determined ECCN, when this determination carries one. `EAR99` is an
    /// ECCN-shaped value in the audit payload's `eccn` field (it is the EAR
    /// catch-all code an exporter cites), so it is surfaced here too;
    /// `USMLCategory` / `NotClassified` / `Pending` carry none.
    pub fn eccn(&self) -> Option<&str> {
        match self {
            ExportClassification::ECCN(code) => Some(code.as_str()),
            ExportClassification::EAR99 => Some("EAR99"),
            _ => None,
        }
    }

    /// The determined USML category, when ITAR-controlled.
    pub fn usml_category(&self) -> Option<&str> {
        match self {
            ExportClassification::USMLCategory(cat) => Some(cat.as_str()),
            _ => None,
        }
    }
}

/// The verdict of an export-control access decision — the closed vocabulary the
/// `export.access_check` audit payload's `decision` field carries.
///
/// S440 — a typed enum rather than a free-text string for the same reason
/// [`Jurisdiction`] is typed: the decision token is the field an auditor greps
/// for, so a typo (`"grant"`, `"GRANTED"`) must be impossible at the write
/// boundary. Round-trip-proven via [`AccessDecision::as_str`] /
/// [`AccessDecision::from_storage_str`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccessDecision {
    /// The access / export may proceed.
    Granted,
    /// The access / export is refused. The accompanying `reason` field names
    /// the rule that drove the refusal.
    Denied,
}

impl AccessDecision {
    /// Canonical storage / audit-payload token.
    pub fn as_str(self) -> &'static str {
        match self {
            AccessDecision::Granted => "granted",
            AccessDecision::Denied => "denied",
        }
    }

    /// Parse the storage token. Fail loud on unknown (CLAUDE.md rule 12) — a
    /// silent fallback to `Granted` would be the worst-class export-control bug
    /// (it would read a denial back out of the ledger as an approval).
    pub fn from_storage_str(s: &str) -> Result<Self, &'static str> {
        match s {
            "granted" => Ok(AccessDecision::Granted),
            "denied" => Ok(AccessDecision::Denied),
            _ => Err("unknown AccessDecision storage string"),
        }
    }
}

impl ScreeningResult {
    /// The access decision this screening outcome implies.
    ///
    /// `Clear` → [`AccessDecision::Granted`]. BOTH `Restricted` and `Denied` →
    /// [`AccessDecision::Denied`]: a `Restricted` match means the transaction
    /// needs a licence that this install has no surface to record, so the
    /// conservative posture is to refuse and let a human adjudicate
    /// ([[trust-code-not-operator]]) rather than let a licensable-but-unlicensed
    /// export through.
    pub fn access_decision(&self) -> AccessDecision {
        match self {
            ScreeningResult::Clear => AccessDecision::Granted,
            ScreeningResult::Restricted(_) | ScreeningResult::Denied(_) => AccessDecision::Denied,
        }
    }

    /// The rule / list that drove the verdict — the `reason` field of the
    /// `export.access_check` payload. `Clear` has no list hit, so it renders the
    /// positive statement rather than an empty string (an empty `reason` reads
    /// as "the writer forgot the field").
    pub fn reason(&self) -> String {
        match self {
            ScreeningResult::Clear => "denied-party screening: clear".to_string(),
            ScreeningResult::Restricted(why) => format!("denied-party screening: restricted ({why})"),
            ScreeningResult::Denied(why) => format!("denied-party screening: denied ({why})"),
        }
    }
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

/// Failure mode of [`validate_eccn`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error(
    "ECCN must be a 5-char Commerce Control List code [0-9][A-E][0-9][0-9][0-9] \
     (e.g. \"7A994\") or the literal \"EAR99\", got {0:?}"
)]
pub struct EccnError(pub String);

/// Validate the *shape* of an Export Control Classification Number (ECCN).
///
/// A CCL ECCN is five characters: a CCL category digit `0-9`, a product-group
/// letter `A-E`, then a three-digit number (`7A994`, `3A001`, …). `EAR99` — the
/// EAR catch-all — is accepted as the one non-CCL literal. This checks format
/// only; whether a given code is *current* on the Commerce Control List is an
/// external-registry question out of scope here (the classification service
/// answers that — mis-classification is a felony, never inferred). Fail loud
/// (CLAUDE.md rule 12) so a malformed code never reaches the `eccn` column.
///
/// S366 review F14: the future AVL write boundary routes the `partners.eccn`
/// value through this gate (no production writer exists yet).
pub fn validate_eccn(s: &str) -> Result<(), EccnError> {
    if s == "EAR99" {
        return Ok(());
    }
    let b = s.as_bytes();
    if b.len() == 5
        && b[0].is_ascii_digit()
        && (b'A'..=b'E').contains(&b[1])
        && b[2].is_ascii_digit()
        && b[3].is_ascii_digit()
        && b[4].is_ascii_digit()
    {
        Ok(())
    } else {
        Err(EccnError(s.to_string()))
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

    /// S367 (review F14) — `validate_eccn` accepts the CCL shape + the `EAR99`
    /// literal and rejects everything else, so a malformed code never reaches
    /// the future `partners.eccn` write boundary.
    #[test]
    fn s367_validate_eccn_accepts_ccl_shape_and_ear99() {
        for ok in ["7A994", "3A001", "0A000", "9E991", "EAR99"] {
            assert!(validate_eccn(ok).is_ok(), "{ok} should be valid");
        }
        for bad in [
            "", "EAR99 ", "ear99", "7A99", "7A9941", "7F994", "AA994", "7A99X", "7a994",
        ] {
            assert_eq!(
                validate_eccn(bad),
                Err(EccnError(bad.to_string())),
                "{bad} should be rejected"
            );
        }
    }

    /// S440 — every `ExportClassification` renders exactly the regime the
    /// regulation assigns it, and the two "no determination yet" variants land
    /// on `UNKNOWN` (never a silent `NOT_CONTROLLED`, which is a *positive*
    /// determination and would read as "cleared for export").
    #[test]
    fn s440_classification_renders_its_jurisdiction() {
        assert_eq!(
            ExportClassification::ECCN("7A994".into()).jurisdiction(),
            Jurisdiction::Ear
        );
        assert_eq!(
            ExportClassification::USMLCategory("VIII(h)".into()).jurisdiction(),
            Jurisdiction::Itar
        );
        assert_eq!(ExportClassification::EAR99.jurisdiction(), Jurisdiction::Ear99);
        assert_eq!(
            ExportClassification::NotClassified.jurisdiction(),
            Jurisdiction::Unknown
        );
        assert_eq!(ExportClassification::Pending.jurisdiction(), Jurisdiction::Unknown);
    }

    /// S440 — the `eccn` / `usml_category` payload fields are populated from the
    /// determination and NEVER cross over (an ITAR determination must not leak
    /// into the `eccn` column and vice versa).
    #[test]
    fn s440_classification_code_accessors_do_not_cross_over() {
        let ear = ExportClassification::ECCN("3A001".into());
        assert_eq!(ear.eccn(), Some("3A001"));
        assert_eq!(ear.usml_category(), None);

        let itar = ExportClassification::USMLCategory("VIII(h)".into());
        assert_eq!(itar.eccn(), None);
        assert_eq!(itar.usml_category(), Some("VIII(h)"));

        // EAR99 IS the code an exporter cites, so it surfaces as the eccn.
        assert_eq!(ExportClassification::EAR99.eccn(), Some("EAR99"));
        assert_eq!(ExportClassification::EAR99.usml_category(), None);

        for undetermined in [
            ExportClassification::NotClassified,
            ExportClassification::Pending,
        ] {
            assert_eq!(undetermined.eccn(), None);
            assert_eq!(undetermined.usml_category(), None);
        }
    }

    /// S440 — `AccessDecision` round-trips and pins its two tokens.
    #[test]
    fn s440_access_decision_round_trips_and_pins_tokens() {
        for d in [AccessDecision::Granted, AccessDecision::Denied] {
            assert_eq!(AccessDecision::from_storage_str(d.as_str()), Ok(d));
        }
        assert_eq!(AccessDecision::Granted.as_str(), "granted");
        assert_eq!(AccessDecision::Denied.as_str(), "denied");
        assert!(AccessDecision::from_storage_str("GRANTED").is_err());
        assert!(AccessDecision::from_storage_str("grant").is_err());
        assert!(AccessDecision::from_storage_str("").is_err());
    }

    /// S440 — the load-bearing conservatism: a `Restricted` screening hit is a
    /// DENIAL at this boundary, not a pass. A restricted party needs a licence
    /// and ABERP has no licence surface to check one against, so letting it
    /// through would be the worst-class export bug.
    #[test]
    fn s440_restricted_screening_is_denied_not_granted() {
        assert_eq!(
            ScreeningResult::Clear.access_decision(),
            AccessDecision::Granted
        );
        assert_eq!(
            ScreeningResult::Restricted("BIS Entity List partial".into()).access_decision(),
            AccessDecision::Denied
        );
        assert_eq!(
            ScreeningResult::Denied("OFAC SDN".into()).access_decision(),
            AccessDecision::Denied
        );
    }

    /// S440 — the `reason` field is never empty, including on the clear path
    /// (an empty reason reads as "the writer forgot the field"), and it carries
    /// the list/why string through on a hit.
    #[test]
    fn s440_screening_reason_is_never_empty_and_carries_the_hit() {
        assert!(!ScreeningResult::Clear.reason().is_empty());
        assert!(ScreeningResult::Denied("OFAC SDN".into())
            .reason()
            .contains("OFAC SDN"));
        assert!(ScreeningResult::Restricted("licence required".into())
            .reason()
            .contains("licence required"));
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
