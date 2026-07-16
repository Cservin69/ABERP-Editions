//! Per-line VAT rate-kind discriminant — `VatRateKind` (ADR-0101).
//!
//! # Why this exists
//!
//! Before ADR-0101 a line could only carry a **numeric** VAT rate
//! (`vat_rate_basis_points`), emitted as `<lineVatRate><vatPercentage>…`.
//! A "0% line" was therefore a literal `vatPercentage 0.00`. That is
//! **wrong** for the Hungarian zero-VAT situations, which are not a
//! numeric zero rate but distinct NAV VAT-category *choices* — AAM
//! (alanyi adómentesség), belföldi fordított adózás (domestic
//! reverse-charge), and the EU / intra-Community exempt / out-of-scope
//! cases. This enum is the closed-vocab discriminant that says WHICH
//! `lineVatRate` choice element a line emits; the `(element, case,
//! reason)` triple each kind maps to is DERIVED in code
//! (`apps/aberp/src/nav_xml.rs`), never stored (ADR-0101 §3.2 /
//! CLAUDE.md rule 5).
//!
//! # Closed vocab, complete-in-intent (ADR-0048 pattern)
//!
//! `Percent` is the default at every layer (model `#[serde(default)]`,
//! DB column default, migration backfill) so a pre-0101 body / side-store
//! `input.json` / migrated DB row all resolve to `Percent` and round-trip
//! **byte-identically** (ADR-0101 §5).
//!
//! v1 (ADR-0101 Sessions 1–2) fully wires the four non-`Percent` kinds
//! Ervin named: [`VatRateKind::AamExempt`],
//! [`VatRateKind::DomesticReverseCharge`],
//! [`VatRateKind::IntraCommunityGoods`],
//! [`VatRateKind::IntraCommunityServiceReverse`]. The remainder of the
//! NAV `LineVatRateType` choice group (§2.2 / §2.5) is **named-deferred**
//! exactly like ADR-0048 deferred `Other`: the enum knows the names, but
//! preflight rejects them and NAV emit `anyhow!`s them, as explicit
//! "not-yet" markers (CLAUDE.md rule 12). This keeps v1 minimal while the
//! vocab is *closed* and complete-in-intent.
//!
//! # Session-1 shut door
//!
//! ADR-0101 §9 lands the NAV machinery (this enum, the emit branch, the
//! validator, the persisted column) in Session 1 but leaves preflight
//! **rejecting every non-`Percent` kind**, so no invoice can actually be
//! issued in a new shape. Session 2 opens preflight behind the mandatory
//! NAV-category adversarial review. Until then the only kind that reaches
//! a real submission is `Percent`.

use serde::{Deserialize, Serialize};

/// The closed vocabulary of per-line VAT rate-kinds (ADR-0101 §3.1).
///
/// Serde serialises each unit variant as its Rust name (`"Percent"`,
/// `"AamExempt"`, …); [`VatRateKind::as_str`] returns the identical
/// canonical string, and that same string is what the `invoice_line`
/// DuckDB column stores. `serde(default)` on a carrier field therefore
/// resolves an absent value to [`VatRateKind::Percent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum VatRateKind {
    /// Numeric VAT rate (27 / 18 / 5 / 0 %). The UNCHANGED pre-0101 path:
    /// emits `<lineVatRate><vatPercentage>{rate}</vatPercentage></…>`.
    /// The default at every layer (backward-compat, ADR-0101 §5).
    #[default]
    Percent,

    // ── v1 fully-wired kinds (Ervin-confirmed codes, 2026-07-15) ──────
    /// Alanyi adómentesség — `vatExemption` / case `AAM`. Áfa tv. §187–196.
    AamExempt,
    /// Belföldi fordított adózás — `vatDomesticReverseCharge` = `true`
    /// (boolean element, no case code). Áfa tv. §142. Buyer self-assesses;
    /// the line VAT amount is 0.
    DomesticReverseCharge,
    /// Közösségen belüli adómentes termékértékesítés (intra-Community
    /// exempt supply of GOODS) — `vatExemption` / case `KBAET`. Áfa tv. §89.
    IntraCommunityGoods,
    /// Közösségen belüli, fordított adózású SZOLGÁLTATÁS (cross-border
    /// service reverse-charged at the customer's member state) —
    /// `vatOutOfScope` / case `EUFAD37` (out of HU scope, NOT an
    /// exemption). Áfa tv. §37.
    IntraCommunityServiceReverse,

    // ── named-deferred remainder (ADR-0101 §3.1 / §10.4) ──────────────
    // Enum-known so the vocab is closed + complete-in-intent, but preflight
    // rejects them and NAV emit `anyhow!`s them (CLAUDE.md rule 12 explicit
    // not-yet markers). Wiring any of these is a separate, later PR — do NOT
    // add an emit/preflight branch for one without its own confirmed code.
    /// Tárgyi adómentesség — `vatExemption` / case `TAM`. NOT YET WIRED.
    TamExempt,
    /// Export termékértékesítés (3rd-country) — `vatExemption` / case `EAM`.
    /// NOT YET WIRED.
    ExportGoods,
    /// Egyéb nemzetközi ügylethez kapcsolódó adómentesség — `vatExemption`
    /// / case `NAM`. NOT YET WIRED.
    OtherInternational,
    /// Új közlekedési eszköz Közösségen belüli értékesítése — `vatExemption`
    /// / case `KBAUK`. Áfa tv. §89(2). NOT YET WIRED.
    NewTransportIntraCommunity,
    /// ÁFA területi hatályán kívüli, 3rd-country teljesítési hely —
    /// `vatOutOfScope` / case `HO`. NOT YET WIRED.
    OutOfScopeThirdCountry,
    /// Különbözet szerinti szabályozás — `marginSchemeIndicator`.
    /// NOT YET WIRED.
    MarginScheme,
    /// Nincs felszámított áfa (§17 / OSA v3.0) — `noVatCharge` = `true`.
    /// NOT YET WIRED.
    NoVatCharge,
    /// Áfatartalom (gross-inclusive, simplified/retail) — `vatContent`.
    /// NOT YET WIRED.
    VatContent,
}

impl VatRateKind {
    /// Canonical string — the serde token AND the `invoice_line`
    /// persisted-column value. Kept in lock-step with the serde
    /// representation by `serde_token_matches_as_str` (test).
    pub fn as_str(&self) -> &'static str {
        match self {
            VatRateKind::Percent => "Percent",
            VatRateKind::AamExempt => "AamExempt",
            VatRateKind::DomesticReverseCharge => "DomesticReverseCharge",
            VatRateKind::IntraCommunityGoods => "IntraCommunityGoods",
            VatRateKind::IntraCommunityServiceReverse => "IntraCommunityServiceReverse",
            VatRateKind::TamExempt => "TamExempt",
            VatRateKind::ExportGoods => "ExportGoods",
            VatRateKind::OtherInternational => "OtherInternational",
            VatRateKind::NewTransportIntraCommunity => "NewTransportIntraCommunity",
            VatRateKind::OutOfScopeThirdCountry => "OutOfScopeThirdCountry",
            VatRateKind::MarginScheme => "MarginScheme",
            VatRateKind::NoVatCharge => "NoVatCharge",
            VatRateKind::VatContent => "VatContent",
        }
    }

    /// Hydrate a persisted-column string back to a kind. `None` on an
    /// unknown token — the caller (the DuckDB adapter) maps that to a
    /// loud `BillingError::Invalid` rather than silently defaulting a
    /// corrupt row to `Percent` (CLAUDE.md rule 11, fail loud). A NULL
    /// column (pre-0101 row that predates the additive migration's
    /// backfill) is resolved to `Percent` by the caller BEFORE reaching
    /// here, so `from_db_str` never sees an empty string as "default".
    pub fn from_db_str(s: &str) -> Option<Self> {
        Some(match s {
            "Percent" => VatRateKind::Percent,
            "AamExempt" => VatRateKind::AamExempt,
            "DomesticReverseCharge" => VatRateKind::DomesticReverseCharge,
            "IntraCommunityGoods" => VatRateKind::IntraCommunityGoods,
            "IntraCommunityServiceReverse" => VatRateKind::IntraCommunityServiceReverse,
            "TamExempt" => VatRateKind::TamExempt,
            "ExportGoods" => VatRateKind::ExportGoods,
            "OtherInternational" => VatRateKind::OtherInternational,
            "NewTransportIntraCommunity" => VatRateKind::NewTransportIntraCommunity,
            "OutOfScopeThirdCountry" => VatRateKind::OutOfScopeThirdCountry,
            "MarginScheme" => VatRateKind::MarginScheme,
            "NoVatCharge" => VatRateKind::NoVatCharge,
            "VatContent" => VatRateKind::VatContent,
            _ => return None,
        })
    }

    /// `true` for the default numeric-rate kind. The Session-1 preflight
    /// shut door rejected every kind for which this is `false`.
    pub fn is_percent(&self) -> bool {
        matches!(self, VatRateKind::Percent)
    }

    /// `true` for the four non-`Percent` kinds ADR-0101 v1 FULLY WIRES
    /// (Ervin-confirmed NAV codes: AAM / KBAET / EUFAD37 / the
    /// `vatDomesticReverseCharge` boolean). These are the kinds Session 2
    /// preflight ACCEPTS (each requiring a 0% line — ADR-0101 §4). The
    /// remaining non-`Percent` kinds are named-deferred: enum-known so the
    /// vocab is closed + complete-in-intent, but preflight rejects them
    /// (`VatRateKindNotSupportedYet`) and NAV emit `anyhow!`s them
    /// (`vat_rate_choice`), as explicit not-yet markers (CLAUDE.md rule 12).
    ///
    /// The membership here is the SINGLE source of truth for "which kinds
    /// preflight opens"; it is kept in lock-step with the four `vat_rate_choice`
    /// arms in `nav_xml.rs` that resolve to a concrete NAV choice (the
    /// storno-fold inverse `vat_rate_kind_from_choice` iterates this same
    /// four-kind set). A named-deferred kind added to `vat_rate_choice`
    /// later MUST also be added here in the same PR.
    pub fn is_wired(&self) -> bool {
        matches!(
            self,
            VatRateKind::AamExempt
                | VatRateKind::DomesticReverseCharge
                | VatRateKind::IntraCommunityGoods
                | VatRateKind::IntraCommunityServiceReverse
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every variant the compiler knows must round-trip through
    /// `as_str` → `from_db_str`. A future variant added without a
    /// `from_db_str` arm fails here (the `_ => None` arm returns `None`),
    /// not silently at a prod hydrate. The list is the breadth ritual —
    /// keep it exhaustive.
    #[test]
    fn as_str_from_db_str_round_trip_every_variant() {
        let all = [
            VatRateKind::Percent,
            VatRateKind::AamExempt,
            VatRateKind::DomesticReverseCharge,
            VatRateKind::IntraCommunityGoods,
            VatRateKind::IntraCommunityServiceReverse,
            VatRateKind::TamExempt,
            VatRateKind::ExportGoods,
            VatRateKind::OtherInternational,
            VatRateKind::NewTransportIntraCommunity,
            VatRateKind::OutOfScopeThirdCountry,
            VatRateKind::MarginScheme,
            VatRateKind::NoVatCharge,
            VatRateKind::VatContent,
        ];
        for k in all {
            assert_eq!(
                VatRateKind::from_db_str(k.as_str()),
                Some(k),
                "round-trip failed for {k:?}"
            );
        }
    }

    /// The serde JSON token MUST equal `as_str` for every variant — the
    /// DB column and the wire body share one canonical spelling. A drift
    /// (e.g. a `#[serde(rename)]` added to one but not the other) would
    /// silently split the two representations; this pin catches it.
    #[test]
    fn serde_token_matches_as_str() {
        let all = [
            VatRateKind::Percent,
            VatRateKind::AamExempt,
            VatRateKind::DomesticReverseCharge,
            VatRateKind::IntraCommunityGoods,
            VatRateKind::IntraCommunityServiceReverse,
            VatRateKind::TamExempt,
            VatRateKind::ExportGoods,
            VatRateKind::OtherInternational,
            VatRateKind::NewTransportIntraCommunity,
            VatRateKind::OutOfScopeThirdCountry,
            VatRateKind::MarginScheme,
            VatRateKind::NoVatCharge,
            VatRateKind::VatContent,
        ];
        for k in all {
            let json = serde_json::to_string(&k).unwrap();
            assert_eq!(json, format!("\"{}\"", k.as_str()), "serde token for {k:?}");
            let back: VatRateKind = serde_json::from_str(&json).unwrap();
            assert_eq!(back, k);
        }
    }

    /// `Default` is `Percent` — the backward-compat keystone. If this
    /// ever changes, every `#[serde(default)]` carrier and the DB
    /// `DEFAULT 'Percent'` diverge and pre-0101 bodies stop round-tripping.
    #[test]
    fn default_is_percent() {
        assert_eq!(VatRateKind::default(), VatRateKind::Percent);
        assert!(VatRateKind::default().is_percent());
    }

    #[test]
    fn unknown_db_token_is_none_not_default() {
        assert_eq!(VatRateKind::from_db_str("NotAKind"), None);
        assert_eq!(VatRateKind::from_db_str(""), None);
    }

    /// ADR-0101 §4 — exactly the four Ervin-confirmed kinds are `is_wired`
    /// (Session-2 preflight accepts them at 0%); `Percent` is NOT wired (it
    /// is the numeric path, gated separately) and every named-deferred kind
    /// is NOT wired (preflight rejects them `VatRateKindNotSupportedYet`).
    /// Pinning the exact set here means a future variant slipped into the
    /// wired set without a confirmed NAV code trips this test, not a live
    /// ÁFA submission.
    #[test]
    fn is_wired_is_exactly_the_four_confirmed_kinds() {
        use VatRateKind::*;
        let wired = [
            AamExempt,
            DomesticReverseCharge,
            IntraCommunityGoods,
            IntraCommunityServiceReverse,
        ];
        for k in wired {
            assert!(k.is_wired(), "{k:?} must be wired");
            assert!(!k.is_percent(), "{k:?} must not be Percent");
        }
        // Percent + every named-deferred kind must NOT be wired.
        let not_wired = [
            Percent,
            TamExempt,
            ExportGoods,
            OtherInternational,
            NewTransportIntraCommunity,
            OutOfScopeThirdCountry,
            MarginScheme,
            NoVatCharge,
            VatContent,
        ];
        for k in not_wired {
            assert!(!k.is_wired(), "{k:?} must NOT be wired");
        }
    }
}
