//! Payment-method domain type — `PaymentMethod` (S160 / ADR-0050).
//!
//! # Why a single closed enum, NOT the `Nav(enum) | Own(String)` shape
//!
//! [`crate::domain::unit_of_measure::ProductUnit`] (S159) is an
//! `Nav(NavUnitOfMeasure) | Own(String)` sum because NAV's `LineType`
//! genuinely carries a paired `<unitOfMeasure>OWN</...>` +
//! `<unitOfMeasureOwn>{free-text}</...>` escape hatch. Payment method
//! looks superficially similar — both are per-emit NAV enums — but
//! NAV's `paymentMethodType` is a **closed** enumeration
//! (`TRANSFER` / `CASH` / `CARD` / `VOUCHER` / `OTHER`) with **no
//! free-text companion element**. There is no `<paymentMethodOwn>` in
//! NAV's v3.0 InvoiceData schema (confirmed against the validator's
//! `walk_invoice_detail` allowlist, which admits `paymentMethod` but
//! has no `paymentMethodOwn` slot). Emitting one would be rejected by
//! both ABERP's own `nav-xsd-validator` (`UnexpectedElement`) and by
//! NAV (`SCHEMA_VIOLATION`).
//!
//! So `OTHER` IS the escape hatch NAV provides — a catch-all that
//! renders as "Egyéb" on the printed invoice. A free-text variant
//! would be a wrapper around a payload the wire cannot carry
//! (CLAUDE.md rule 13: delete the part that should not exist). The
//! type is therefore a single closed-vocab enum, mirroring
//! [`NavUnitOfMeasure`]'s SCREAMING_SNAKE serde + `nav_token` shape but
//! without the outer `Own` sum.
//!
//! # The payment-method model — load-bearing
//!
//! Payment method is a property of the **transaction**, not the party:
//! the same buyer may pay by transfer one month and cash the next, so
//! it is snapshotted per invoice (rides the side-store `input.json` +
//! the on-disk NAV XML, audit-immutable — see ADR-0050). The operator
//! picks it on the Issue form; the default is `Transfer` (Átutalás),
//! which preserves the pre-S160 hardcoded behaviour byte-for-byte.

use serde::{Deserialize, Serialize};

/// NAV v3.0 `paymentMethodType` enum mirror. Each variant serialises as
/// the NAV-defined token (`TRANSFER`, `CASH`, …) via serde's
/// `rename_all = "SCREAMING_SNAKE_CASE"` — wire body and NAV XML body
/// agree by construction, exactly as [`NavUnitOfMeasure`] does.
///
/// `Other` is `OTHER` — NAV's catch-all. There is intentionally no
/// free-text companion (see the module doc); `Other` renders as the
/// Hungarian "Egyéb" on the printed PDF.
///
/// Adding a variant: confirm against NAV's v3.0 paymentMethodType
/// schema, extend the enum + the `nav_token` / `from_nav_token` /
/// label mappings, then widen the SPA's dropdown
/// (`apps/aberp-ui/ui/src/lib/payment-method.ts`). See ADR-0050.
///
/// [`NavUnitOfMeasure`]: crate::domain::unit_of_measure::NavUnitOfMeasure
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone, Copy, Default)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PaymentMethod {
    /// Átutalás / Bank transfer — the default (pre-S160 hardcoded value).
    #[default]
    Transfer,
    /// Készpénz / Cash.
    Cash,
    /// Bankkártya / Card.
    Card,
    /// Utalvány / Voucher.
    Voucher,
    /// Egyéb / Other — NAV's closed-enum catch-all (no free-text on the
    /// wire; renders as "Egyéb" on the PDF).
    Other,
}

impl PaymentMethod {
    /// NAV v3.0 token body — what goes between `<paymentMethod>` and
    /// `</paymentMethod>` in InvoiceData XML. The NAV XML emitter
    /// (`apps/aberp/src/nav_xml.rs::write_invoice_detail`, S160) uses
    /// this directly; serde callers receive the same string via
    /// `Serialize`.
    pub fn nav_token(self) -> &'static str {
        match self {
            PaymentMethod::Transfer => "TRANSFER",
            PaymentMethod::Cash => "CASH",
            PaymentMethod::Card => "CARD",
            PaymentMethod::Voucher => "VOUCHER",
            PaymentMethod::Other => "OTHER",
        }
    }

    /// Parse a NAV token string back to the enum. `None` for any string
    /// outside the closed vocab. Mirror of
    /// [`NavUnitOfMeasure::from_nav_token`]; the PDF renderer keeps its
    /// own pass-through mapping (`print_invoice::payment_method_display`)
    /// so an unrecognised legacy token still prints readably rather than
    /// loud-failing the render.
    ///
    /// [`NavUnitOfMeasure::from_nav_token`]: crate::domain::unit_of_measure::NavUnitOfMeasure::from_nav_token
    pub fn from_nav_token(token: &str) -> Option<Self> {
        match token {
            "TRANSFER" => Some(PaymentMethod::Transfer),
            "CASH" => Some(PaymentMethod::Cash),
            "CARD" => Some(PaymentMethod::Card),
            "VOUCHER" => Some(PaymentMethod::Voucher),
            "OTHER" => Some(PaymentMethod::Other),
            _ => None,
        }
    }

    /// Hungarian operator-facing label (primary on the printed invoice).
    pub fn hu_label(self) -> &'static str {
        match self {
            PaymentMethod::Transfer => "Átutalás",
            PaymentMethod::Cash => "Készpénz",
            PaymentMethod::Card => "Bankkártya",
            PaymentMethod::Voucher => "Utalvány",
            PaymentMethod::Other => "Egyéb",
        }
    }

    /// English label (secondary; parenthesised in bilingual UI).
    pub fn en_label(self) -> &'static str {
        match self {
            PaymentMethod::Transfer => "Bank transfer",
            PaymentMethod::Cash => "Cash",
            PaymentMethod::Card => "Card",
            PaymentMethod::Voucher => "Voucher",
            PaymentMethod::Other => "Other",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// S160 — `nav_token` ↔ `from_nav_token` round-trip across the full
    /// closed vocab, and the serde wire form agrees with `nav_token`
    /// (both are the SCREAMING_SNAKE token). A future refactor that
    /// flipped the serde rename would surface here AND at the SPA mirror
    /// in `payment-method.ts`.
    #[test]
    fn nav_token_round_trips_and_matches_serde() {
        for m in [
            PaymentMethod::Transfer,
            PaymentMethod::Cash,
            PaymentMethod::Card,
            PaymentMethod::Voucher,
            PaymentMethod::Other,
        ] {
            assert_eq!(PaymentMethod::from_nav_token(m.nav_token()), Some(m));
            // serde wire form == nav_token (quoted JSON string).
            assert_eq!(
                serde_json::to_string(&m).unwrap(),
                format!("\"{}\"", m.nav_token())
            );
        }
        assert_eq!(PaymentMethod::from_nav_token("PIX_CRYPTO"), None);
    }

    /// S160 — `Default` is `Transfer` (the pre-S160 hardcoded value).
    /// This is what `#[serde(default)]` on `InvoiceInputJson` falls back
    /// to for pre-S160 side-stored bodies, preserving byte-identical
    /// `<paymentMethod>TRANSFER</...>` output.
    #[test]
    fn default_is_transfer() {
        assert_eq!(PaymentMethod::default(), PaymentMethod::Transfer);
        // A JSON object missing the field deserialises to Transfer.
        #[derive(Deserialize)]
        struct Holder {
            #[serde(default)]
            pm: PaymentMethod,
        }
        let h: Holder = serde_json::from_str("{}").unwrap();
        assert_eq!(h.pm, PaymentMethod::Transfer);
    }

    /// S160 — bilingual labels pinned (the SPA mirror in
    /// `payment-method.ts` must agree; the PDF renders `hu_label`).
    #[test]
    fn labels_pinned() {
        assert_eq!(PaymentMethod::Transfer.hu_label(), "Átutalás");
        assert_eq!(PaymentMethod::Cash.hu_label(), "Készpénz");
        assert_eq!(PaymentMethod::Other.hu_label(), "Egyéb");
        assert_eq!(PaymentMethod::Cash.en_label(), "Cash");
    }
}
