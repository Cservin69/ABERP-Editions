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

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use time::Date;

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
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

/// MNB-rate metadata stamped onto an issued non-HUF invoice per ADR-0037
/// §1.a + §1.b + §2. Composed at issuance time from
/// `aberp-mnb-rates`'s response (the rate value + publication date) plus
/// the per-Áfa-convention round-half-even HUF-equivalent total (per
/// ADR-0037 §1.c + §4 invariant C11).
///
/// The struct is intentionally inline-stamped onto each EUR (or future
/// non-HUF) invoice row rather than externalised into a separate table.
/// Two forces drove the inline-stamp posture:
///
/// 1. **Regulatory record fidelity.** ADR-0037 §1.a requires the rate
///    value, source name, and rate-publication date to appear on the
///    printed invoice; the per-issued-invoice row is the canonical
///    record those three printed fields read back from.
/// 2. **No retroactive rewrite.** Existing HUF-only rows pre-PR-44γ
///    carry `currency = "HUF"` and `exchange_rate = NULL` per the
///    migration backfill — a NULL rate means "no conversion needed",
///    NOT "rate missing"; the C10 byte-identical invariant holds at
///    PR-44γ because HUF rows do not gain a non-trivial rate stamp.
///
/// The `source` field is intentionally a `String`, not a typed
/// `RateSource` enum, because ADR-0037 §2.a confirms the literal `"MNB"`
/// as the only source at PR-44γ time; a future operator-walk that
/// surfaces a second source (e.g., ECB) lifts this into an enum at that
/// PR's surface, not speculatively here (CLAUDE.md rule 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateMetadata {
    /// MNB-published rate value at the precision MNB returns (typically
    /// 4 decimal places — e.g., `405.2300`). Stored as
    /// `rust_decimal::Decimal` so the per-invoice round-half-even
    /// arithmetic is exact (per A135's deferred-to-arithmetic-consumer
    /// trigger lifted at THIS PR).
    pub rate: Decimal,
    /// Exchange-rate source identifier — the literal string `"MNB"`
    /// per ADR-0037 §2.a (printed-invoice surface) + the
    /// `aberp_mnb_rates::SOURCE` constant the network-side fetcher
    /// emits. Pinned by the `rate_metadata_source_matches_mnb_constant`
    /// test in the binary's issuance path.
    pub source: String,
    /// Publication date of the rate that was applied. May differ from
    /// the supply-fulfillment date if MNB walked back to the most-recent
    /// prior publication date per ADR-0037 §2.b (weekend, holiday).
    /// Consumers MUST read this when populating the printed-invoice
    /// `Exchange-rate date` field per ADR-0037 §1.a.
    pub date: Date,
    /// The round-half-even HUF equivalent of the invoice's gross total,
    /// in whole forints, per ADR-0037 §1.c + §4 invariant C11. Computed
    /// at the per-invoice level using [`huf_equivalent_round_half_even`].
    /// Per-VAT-rate HUF amounts on the wire body (C5 / PR-44δ) decompose
    /// this total; PR-44γ stamps the invoice-level figure only.
    pub huf_equivalent_total: i64,
}

/// PR-73 / ADR-0040 §addendum — denormalized per-invoice snapshot of the
/// `[[seller.banks]]` entry the operator selected (or defaulted to) at
/// issuance time.
///
/// Inline-stamped onto each issued invoice row rather than resolved live
/// from `seller.toml` at read time. Two forces drove the inline-stamp
/// posture (mirroring [`RateMetadata`]'s rationale):
///
/// 1. **Operator-twin survivor.** If the operator later edits or deletes
///    the `SellerBank` entry, the historical invoice continues to render
///    the bank account it was issued with. The `id` field is preserved
///    so a future audit can pivot back to the still-present entry when
///    one exists; the other four fields are the regulatory-record copy.
/// 2. **No retroactive rewrite.** Pre-PR-73 invoices carry the snapshot
///    as NULL across all five columns — they were issued before the
///    multi-bank-account schema existed, and the read path falls back
///    to "(no bank account on file)" rendering rather than silently
///    fabricating one from current state.
///
/// Chain children (storno + modification) inherit the snapshot verbatim
/// from the base invoice rather than re-resolving against the operator's
/// current `seller.toml` — the regulatory record is "the bank account
/// the base invoice asked to be paid to", and a fresh resolution at chain
/// time could surface a different account if the operator rotated the
/// default between issuance and storno/modification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BankAccountSnapshot {
    /// `bnk_<26-char>` deterministic id from `aberp::seller_banks` per
    /// ADR-0040 §1.
    pub id: String,
    /// ISO 4217 string (`"HUF"` / `"EUR"`) of the bank account at
    /// issuance time. Pinned to the invoice's currency by the route
    /// resolver (currency mismatch surfaces as
    /// `InvoicePreflightError::SellerBankCurrencyMismatch`).
    pub currency: String,
    /// Bank account number string verbatim — IBAN form for EUR,
    /// domestic form for HUF.
    pub account_number: String,
    /// Operator-typed bank name (e.g., `"Erste Bank"`).
    pub bank_name: String,
    /// SWIFT/BIC code (8 or 11 chars).
    pub swift_bic: String,
}

/// Round-half-even (banker's rounding) HUF-equivalent of an EUR cent
/// amount converted at `rate` (HUF per 1 EUR). Per ADR-0037 §1.c +
/// §4 invariant C11 / A137 — the Áfa-convention rounding mode that
/// supersedes the pre-cleanup half-up posture at the 2026-05-23 legal
/// walk.
///
/// HUF amounts are whole forints (`Huf(pub i64)`) per ADR-0009 §1;
/// the rounding mode reaches for `rust_decimal::Decimal::ROUND_HALF_EVEN`
/// after multiplying-and-dividing-by-100 (cents → euros) at full
/// `Decimal` precision so neither input units nor the rate's published
/// precision lose information.
///
/// `eur_cents` is the EUR amount in cents (e.g., `1234` for `€12.34`),
/// the same shape `Eur(pub i64)` carries internally per the session-46
/// pre-PR-44α `Eur` introduction.
///
/// # Pin-test handle
///
/// The half-even tie-break is pinned by
/// `huf_equivalent_uses_banker_rounding_on_ties` in this file's test
/// module — `1230 cents × 1.0 HUF/EUR = 12.30 HUF` rounds to `12` (the
/// even forint), `1250 cents × 1.0 HUF/EUR = 12.50 HUF` rounds to `12`
/// (the even forint), `1350 cents × 1.0 HUF/EUR = 13.50 HUF` rounds to
/// `14` (the even forint). Half-up would produce `13` / `13` / `14`
/// — the differential is the load-bearing pin per CLAUDE.md rule 9.
///
/// Returns `None` only when the intermediate
/// `Decimal * Decimal` overflows the 96-bit mantissa OR the final
/// rounded value doesn't fit in `i64`. Both surface as loud-fail at the
/// CLI boundary per CLAUDE.md rule 12.
pub fn huf_equivalent_round_half_even(eur_cents: i64, rate: &Decimal) -> Option<i64> {
    use rust_decimal::prelude::ToPrimitive;
    use rust_decimal::RoundingStrategy;

    let cents = Decimal::from(eur_cents);
    let hundred = Decimal::from(100);
    // huf = (eur_cents / 100) * rate, kept at full Decimal precision
    // through the multiply so neither operand truncates.
    let huf_full = cents.checked_mul(*rate)?.checked_div(hundred)?;
    let huf_rounded = huf_full.round_dp_with_strategy(0, RoundingStrategy::MidpointNearestEven);
    huf_rounded.to_i64()
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
        let cases: [(Currency, &str); 2] = [(Currency::Huf, "HUF"), (Currency::Eur, "EUR")];
        for (variant, expected) in cases {
            let value =
                serde_json::to_value(variant).expect("Currency variants must always serialise");
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

    /// PR-44γ — `huf_equivalent_round_half_even` ties round to the even
    /// forint per ADR-0037 §1.c + A137 (the 2026-05-23 legal-cleanup
    /// correction superseding the pre-cleanup half-up posture). The
    /// load-bearing pin is the differential against half-up: this test
    /// is constructed so its assertions WOULD FAIL if anyone reverted
    /// the rounding mode to half-up. Per CLAUDE.md rule 9 (tests verify
    /// intent, not just behaviour) — a test that asserted only
    /// `eur * rate = huf` regardless of fractional part would pass
    /// under either rounding mode and would NOT catch the regression
    /// the C11 invariant exists to prevent.
    ///
    /// Cases:
    /// - `12.50 EUR × 1.0 = 12.50 HUF` → round-half-even = `12`,
    ///   half-up = `13`. Differential pin.
    /// - `13.50 EUR × 1.0 = 13.50 HUF` → round-half-even = `14`,
    ///   half-up = `14`. (Both agree; included to show the rule does
    ///   round AWAY when the next-even direction is up.)
    /// - `12.30 EUR × 1.0 = 12.30 HUF` → both modes = `12` (no tie;
    ///   sanity check that non-tied values agree.)
    /// - `12.70 EUR × 1.0 = 12.70 HUF` → both modes = `13` (no tie;
    ///   sanity check that round-up applies to the > 0.5 half too.)
    #[test]
    fn huf_equivalent_uses_banker_rounding_on_ties() {
        use rust_decimal::Decimal;
        use std::str::FromStr;

        let rate_one = Decimal::from(1);

        // Half-even tie-break case 1: `12.50 → 12` (even forint).
        // Half-up would produce `13` — the regression this test catches.
        assert_eq!(
            huf_equivalent_round_half_even(1250, &rate_one),
            Some(12),
            "12.50 HUF must round half-even to 12 (the even forint); half-up would give 13"
        );

        // Half-even tie-break case 2: `13.50 → 14` (even forint).
        // Half-up also gives `14`; this case confirms the rule rounds AWAY
        // on the half whose next-even neighbour is the higher integer.
        assert_eq!(
            huf_equivalent_round_half_even(1350, &rate_one),
            Some(14),
            "13.50 HUF must round half-even to 14 (the even forint)"
        );

        // Non-tied below-half: `12.30 → 12` under either rule.
        assert_eq!(
            huf_equivalent_round_half_even(1230, &rate_one),
            Some(12),
            "12.30 HUF rounds down to 12 (below-half; non-tied)"
        );

        // Non-tied above-half: `12.70 → 13` under either rule.
        assert_eq!(
            huf_equivalent_round_half_even(1270, &rate_one),
            Some(13),
            "12.70 HUF rounds up to 13 (above-half; non-tied)"
        );

        // Realistic MNB EUR/HUF rate: `12.50 EUR × 405.230000 HUF/EUR`
        // = `5065.375 HUF`. The fractional part is `.375` — non-tied,
        // rounds down to `5065` under either rule. Included as a sanity
        // check that the helper composes correctly with a typical MNB
        // precision value.
        let rate_mnb = Decimal::from_str("405.230000").expect("rate parses");
        assert_eq!(
            huf_equivalent_round_half_even(1250, &rate_mnb),
            Some(5065),
            "12.50 EUR × 405.230000 HUF/EUR = 5065.375 HUF; rounds to 5065 under either rule"
        );
    }

    /// PR-44γ — `huf_equivalent_round_half_even` returns `None` on
    /// arithmetic overflow rather than silently producing a wrong
    /// figure. Per CLAUDE.md rule 12 (fail loud); the CLI boundary
    /// surfaces the `None` as a typed loud-fail error.
    #[test]
    fn huf_equivalent_returns_none_on_overflow() {
        use rust_decimal::Decimal;
        use std::str::FromStr;

        // `i64::MAX cents × 1.0 = i64::MAX / 100` HUF. That fits in i64
        // (it's smaller than i64::MAX). Pick a value that doesn't.
        // `i64::MAX cents × 1000.0 = 10× too big after /100`. The
        // intermediate Decimal multiply succeeds but the final
        // `to_i64` returns None.
        let huge_rate = Decimal::from_str("1000.0").expect("rate parses");
        assert_eq!(
            huf_equivalent_round_half_even(i64::MAX, &huge_rate),
            None,
            "extreme cents × rate must loud-fail (None) rather than wrap silently"
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
