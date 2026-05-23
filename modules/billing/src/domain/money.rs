//! Money types per ADR-0009 §1 (HUF-only command boundary) — **extended**
//! by ADR-0037 §3 to a closed `Currency` vocab `{Huf, Eur}` with the
//! widening trigger "operator signs a customer needing that currency"
//! inherited from ADR-0009 §1. The original HUF-only rationale is
//! preserved verbatim below for the `Huf` type; the new `Currency` + `Money`
//! shapes generalise it for PR-44β-through-ε per ADR-0037 §5.
//!
//! HUF amounts are stored as **whole forints** in an `i64`. HUF has no
//! sub-unit in practice — Hungarian invoices round to the forint, and NAV
//! accepts integer HUF values.
//!
//! EUR amounts are stored as **cents** in an `i64` (two-decimal precision).
//!
//! Negative amounts are permitted at the type level because credit notes
//! and storno invoices carry negative line totals. The command-handler
//! layer (`app/issue_invoice.rs`) enforces per-line invariants.

use std::fmt;

use serde::Serialize;

/// Whole forints. Hungarian invoicing rounds to the forint at the line
/// level; sub-forint precision is not used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Huf(pub i64);

impl Huf {
    pub const ZERO: Self = Self(0);

    pub fn as_i64(self) -> i64 {
        self.0
    }

    pub fn checked_add(self, other: Self) -> Option<Self> {
        self.0.checked_add(other.0).map(Self)
    }

    pub fn checked_mul_u32(self, n: u32) -> Option<Self> {
        self.0.checked_mul(n as i64).map(Self)
    }
}

impl fmt::Display for Huf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} HUF", self.0)
    }
}



/// EUR implemented by Ervin
/// Internal representation: cents (i64) → exact two decimal places, no rounding issues
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Eur(pub i64);

impl Eur {
    pub const ZERO: Self = Self(0);

    /// Create from cents (e.g. 123 → 1.23 EUR)
    pub fn from_cents(cents: i64) -> Self {
        Self(cents)
    }

    /// Create from whole euros (e.g. 5 → 5.00 EUR)
    pub fn from_euros(euros: i64) -> Self {
        Self(euros * 100)
    }

    pub fn as_i64(self) -> i64 {
        self.0
    }

    /// Returns the value in cents (same as as_i64)
    pub fn as_cents(self) -> i64 {
        self.0
    }

    pub fn checked_add(self, other: Self) -> Option<Self> {
        self.0.checked_add(other.0).map(Self)
    }

    pub fn checked_sub(self, other: Self) -> Option<Self> {
        self.0.checked_sub(other.0).map(Self)
    }

    pub fn checked_mul_u32(self, n: u32) -> Option<Self> {
        self.0.checked_mul(n as i64).map(Self)
    }
}

impl fmt::Display for Eur {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 == 0 {
            return write!(f, "0.00 EUR");
        }

        let sign = if self.0 < 0 { "-" } else { "" };
        let abs_cents = self.0.abs();
        let euros = abs_cents / 100;
        let cents = abs_cents % 100;

        write!(f, "{}{}.{:02} EUR", sign, euros, cents)
    }
}

/// Closed currency vocab per ADR-0037 §3. Initial variant set `{Huf, Eur}`;
/// adding a third variant (`Chf`, `Usd`) is an additive enum-variant change
/// + a test row + an ADR-0037 amendment per ADR-0037 §3 widening trigger.
///
/// Variant names match the existing money-type names (`Huf`, `Eur`) rather
/// than the ISO 4217 codes — the ISO 4217 string surfaces via
/// `Currency::iso_code` AND via serde with `rename_all = "UPPERCASE"`. The
/// dual surface mirrors `AckStatus` in `apps/aberp/src/serve.rs` (A109) so
/// the wire emit (JSON for the SPA / NAV body field text) and the
/// programmatic accessor (for non-serde callers — e.g., the future
/// `nav_xml.rs` `currencyCode` element body) agree by construction.
///
/// Derives mirror the typed-enum precedent set by `InvoiceState` and
/// `AckStatus` in `apps/aberp/src/serve.rs` (A108 / A109): `Debug` +
/// `PartialEq` + `Eq` + `Clone` + `Copy` keep test assertions ergonomic;
/// `Hash` is added so a future per-(currency, date) MNB-rate cache (named
/// open in ADR-0037 §2.b) can key on `Currency` without re-deriving.
#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "UPPERCASE")]
pub enum Currency {
    Huf,
    Eur,
}

impl Currency {
    /// ISO 4217 three-letter code. Non-serde callers (NAV XML body text,
    /// printed-invoice render) read this directly; serde callers receive
    /// the same string via `Serialize` per `rename_all = "UPPERCASE"`. Both
    /// surfaces are pinned by `currency_wire_shape_pins_iso_4217_strings`.
    pub fn iso_code(self) -> &'static str {
        match self {
            Currency::Huf => "HUF",
            Currency::Eur => "EUR",
        }
    }
}

/// Currency-aware money shape per ADR-0037 §3 + §Open question 1.
///
/// Enum-sum unification chosen per session-48 brief option (c) — each
/// variant carries its existing per-currency typed amount (`Huf` in whole
/// forints, `Eur` in cents). The two underlying types keep their unit
/// discipline (no re-encoding of HUF-vs-EUR precision at the `Money`
/// layer); consumers of `Money` get exhaustive variant matching at every
/// site (CLAUDE.md rule 11 — adding a third currency surfaces every
/// non-exhaustive `match Money { ... }` as a compile error).
///
/// PR-44α adds the type ONLY. PR-44γ wires it into the issuance-command
/// boundary; PR-44δ wires `currency()` into the NAV XML body's
/// `currencyCode` element; PR-44ε wires the SPA surface. The existing
/// `Huf` / `Eur` tuple structs are preserved — every pre-PR-44α
/// `Huf(N)` call site continues to compile and behave identically (the
/// C10 byte-identical invariant prerequisite at the type level).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Money {
    Huf(Huf),
    Eur(Eur),
}

impl Money {
    /// Construct a HUF `Money` value directly from whole forints, mirroring
    /// `Huf(N)`'s tuple-constructor unit convention.
    pub fn huf(forints: i64) -> Self {
        Money::Huf(Huf(forints))
    }

    /// Construct an EUR `Money` value directly from cents, mirroring
    /// `Eur(N)`'s tuple-constructor unit convention.
    pub fn eur(cents: i64) -> Self {
        Money::Eur(Eur(cents))
    }

    /// The currency tag for this value. Used by callers that need to
    /// route on currency without destructuring the amount (e.g., a future
    /// `ReadyInvoice.currency` accessor at PR-44γ time, or the NAV XML
    /// body's `currencyCode` element body at PR-44δ time).
    pub fn currency(self) -> Currency {
        match self {
            Money::Huf(_) => Currency::Huf,
            Money::Eur(_) => Currency::Eur,
        }
    }
}

impl fmt::Display for Money {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Money::Huf(h) => h.fmt(f),
            Money::Eur(e) => e.fmt(f),
        }
    }
}

#[cfg(test)]
mod currency_tests {
    //! PR-44α / ADR-0037 §3 pin tests. The compile-time invariant (closed
    //! vocab, exhaustive variant matching) is enforced by Rust's type
    //! system; these tests pin the runtime invariants the type system
    //! cannot reach: the wire form (JSON strings for SPA / NAV body
    //! consumption per ADR-0037 §1.a/§1.b), the `iso_code` accessor
    //! agreeing with the serde rename, the variant count matching the
    //! ADR's pinned vocab, and the cross-currency non-interchange of
    //! `Money` values (a HUF amount and an EUR amount of the same
    //! numeric value MUST NOT compare equal — the prerequisite at the
    //! type level for ADR-0037 §4 invariant C8).
    //!
    //! Mirrors the `ack_status_wire_shape_pins_uppercase_strings` /
    //! `invoice_state_wire_shape_pins_pascalcase_strings` precedents
    //! (A109 / A108) in `apps/aberp/src/serve.rs`.
    use super::*;

    /// Currency wire-shape pin: ISO 4217 three-letter strings emitted by
    /// both `Serialize` and `iso_code`. A regression that re-cases the
    /// serde rename (or drops it) drifts the wire form; a regression
    /// that re-cases `iso_code` drifts the non-serde surface. Pinning
    /// both at the same call site closes the drift gap per CLAUDE.md
    /// rule 12 (fail loud).
    #[test]
    fn currency_wire_shape_pins_iso_4217_strings() {
        let cases: [(Currency, &str); 2] = [
            (Currency::Huf, "HUF"),
            (Currency::Eur, "EUR"),
        ];
        for (variant, expected) in cases {
            let value = serde_json::to_value(variant)
                .expect("Currency variants must always serialise");
            assert_eq!(
                value,
                serde_json::Value::String(expected.to_string()),
                "Currency::{variant:?} serde wire form drifted from `{expected}`",
            );
            assert_eq!(
                variant.iso_code(),
                expected,
                "Currency::{variant:?}::iso_code drifted from `{expected}`",
            );
        }
    }

    /// Closed-vocab guard: the variant count matches ADR-0037 §3's pinned
    /// `{Huf, Eur}` set, and the exhaustive `match` below is a compile-
    /// time forcing function — adding a third variant without updating
    /// every consumer is a compile error per Rust's normal exhaustiveness
    /// check, which is the §3 widening-trigger enforcement mechanism.
    /// The runtime count assertion catches the case where a variant is
    /// added AND this test's iteration array is updated AND the match is
    /// updated, but the ADR amendment is forgotten — the test still
    /// surfaces the change as a loud diff.
    #[test]
    fn currency_closed_vocab_pins_huf_and_eur_only() {
        let variants = [Currency::Huf, Currency::Eur];
        for v in variants {
            let _: &'static str = match v {
                Currency::Huf => "HUF",
                Currency::Eur => "EUR",
            };
        }
        assert_eq!(
            variants.len(),
            2,
            "Currency variant count drifted from ADR-0037 §3's pinned `{{Huf, Eur}}` vocab",
        );
    }

    /// Money cross-currency non-interchange: a HUF amount and an EUR
    /// amount of the same integer value are distinct `Money` values.
    /// Prerequisite at the type level for ADR-0037 §4 invariant C8 (the
    /// command boundary refuses any currency not in the closed vocab and
    /// MUST NOT silently coerce between currencies). Per CLAUDE.md rule 9
    /// the test pins the intent: a regression that defines `PartialEq`
    /// to compare only the inner amount (ignoring the variant tag) would
    /// fail this assertion, which is exactly the silent coercion the
    /// invariant forbids.
    #[test]
    fn money_huf_and_eur_do_not_interchange() {
        let huf = Money::huf(1_000);
        let eur = Money::eur(1_000);
        assert_ne!(
            huf, eur,
            "Money::huf(1_000) and Money::eur(1_000) MUST NOT compare equal",
        );
        assert_eq!(huf.currency(), Currency::Huf);
        assert_eq!(eur.currency(), Currency::Eur);
        // Currency-tagged constructor agrees with the underlying typed
        // amount — the lift from a pre-PR-44α `Huf(N)` call site to a
        // `Money::Huf(Huf(N))` is structurally identical.
        assert_eq!(Money::huf(1_000), Money::Huf(Huf(1_000)));
        assert_eq!(Money::eur(1_000), Money::Eur(Eur(1_000)));
    }
}
