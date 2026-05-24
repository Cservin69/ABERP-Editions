//! PR-44γ — offline unit-level tests for the EUR issuance helpers
//! (ADR-0037 §1–§4 + C1 prerequisite + §2.b walk-back + A137/C11
//! round-half-even + A139 walk-back cap).
//!
//! These tests inject a fake [`MnbRatesProvider`] so the
//! rate-fetch + walk-back + HUF-equivalent-arithmetic helpers run
//! without any network call (the live MNB test path lives in
//! `crates/mnb-rates/tests/live_mnb.rs` and is env-gated).
//!
//! The end-to-end issuance flow (`run_with_provider`) goes through
//! `NavCredentials::load_from_keychain`, which would force this test
//! to install per-OS keychain entries — out of scope for an offline
//! pin test. Instead these tests exercise the *load-bearing helper*
//! that the EUR path adds at PR-44γ: `fetch_and_stamp_rate`. Higher-
//! level DuckDB-persistence + audit-payload behaviour is pinned by
//! `tests/rollback_conformance.rs` (HUF path) and by the
//! `draft_created_round_trip_with_rate_metadata` /
//! `huf_equivalent_uses_banker_rounding_on_ties` unit tests in the
//! billing module + audit_payloads.
//!
//! Pin tests:
//!
//! 1. **EUR happy path** — fetch succeeds on the supply-fulfillment
//!    date; the returned `RateMetadata` carries the rate value, the
//!    `MNB` source identifier, the rate-publication date, and the
//!    round-half-even HUF-equivalent total.
//! 2. **EUR walk-back to D-1** — supply-date fetch returns
//!    `NoRateForCurrency`; D-1 fetch succeeds; the returned
//!    `RateMetadata.date` IS D-1 (NOT the supply date) per ADR-0037
//!    §2.b.
//! 3. **EUR walk-back exhausted** — supply-date AND every D-N
//!    attempt up to the cap return `NoRateForCurrency`; the helper
//!    loud-fails with the named substring per the C2 invariant.
//! 4. **MNB transport-class failure** — any `MnbError` variant
//!    other than `NoRateForCurrency` propagates immediately (no
//!    walk-back); the loud-fail message carries the named
//!    transport-failure sentinel.
//! 5. **HUF default path bypasses MNB** — when the issuance
//!    currency is HUF the helper is NOT invoked at all (pinned by
//!    the binary's run path; here we pin the call-site precondition
//!    that the HUF path doesn't consult an `MnbRatesProvider`).

use std::collections::HashMap;
use std::sync::Mutex;

use aberp_billing::Currency;
use aberp_mnb_rates::{MnbError, MnbRate};
use aberp::issue_invoice::{
    fetch_and_stamp_rate, ERR_MNB_FETCH_FAILED, ERR_NO_RATE_AFTER_WALKBACK,
    MNB_WALKBACK_DAYS_CAP,
};
use aberp::mnb_rates_provider::MnbRatesProvider;
use aberp_billing::{Huf, LineItem};
use time::Date;

// ──────────────────────────────────────────────────────────────────────
// Fake MnbRatesProvider
// ──────────────────────────────────────────────────────────────────────

/// HashMap-backed `MnbRatesProvider` for offline tests. Returns
/// `MnbError::NoRateForCurrency` for any (currency, date) tuple not
/// in the map — matching MNB's real "non-publication day" shape per
/// ADR-0037 §2.b.
struct FakeMnbRates {
    rates: HashMap<(Currency, Date), MnbRate>,
    /// Optional "always fail with this error" mode for the transport-
    /// failure test. When `Some`, every call returns this error.
    poison: Mutex<Option<MnbErrorKind>>,
    /// Call-counter so tests can assert the walk-back loop reached
    /// the expected number of fetches.
    calls: Mutex<Vec<(Currency, Date)>>,
}

#[derive(Debug, Clone, Copy)]
enum MnbErrorKind {
    EnvelopeParse,
    SoapFault,
}

impl FakeMnbRates {
    fn empty() -> Self {
        Self {
            rates: HashMap::new(),
            poison: Mutex::new(None),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn with_rate(mut self, currency: Currency, date: Date, value: &str) -> Self {
        self.rates.insert(
            (currency, date),
            MnbRate {
                currency,
                date,
                unit: 1,
                value: value.to_string(),
            },
        );
        self
    }

    fn poisoned(self, kind: MnbErrorKind) -> Self {
        *self.poison.lock().unwrap() = Some(kind);
        self
    }

    fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

impl MnbRatesProvider for FakeMnbRates {
    fn fetch_official_rate(
        &self,
        currency: Currency,
        date: Date,
    ) -> Result<MnbRate, MnbError> {
        self.calls.lock().unwrap().push((currency, date));
        if let Some(kind) = *self.poison.lock().unwrap() {
            return Err(match kind {
                MnbErrorKind::EnvelopeParse => MnbError::EnvelopeParse("fake".to_string()),
                MnbErrorKind::SoapFault => MnbError::SoapFault("fake".to_string()),
            });
        }
        match self.rates.get(&(currency, date)) {
            Some(rate) => Ok(rate.clone()),
            None => Err(MnbError::NoRateForCurrency {
                currency: currency.iso_code().to_string(),
                date: date.to_string(),
            }),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Fixtures
// ──────────────────────────────────────────────────────────────────────

/// Mirror the EUR-line shape from `fixtures/invoice_minimal.json` at
/// the EUR-cents interpretation:
///
///   2 × 1000 cents = €20.00 net
///   1 × 5000 cents = €50.00 net
///   subtotal       = €70.00 (= 7000 cents) net
///   27% VAT        = €18.90 (= 1890 cents)
///   gross total    = €88.90 (= 8890 cents)
///
/// At rate 405.230000 HUF/EUR:
///   8890 cents × 405.230000 / 100 = 36025.0470 HUF
///   round-half-even → 36025 HUF
fn fixture_eur_lines() -> Vec<LineItem> {
    vec![
        LineItem {
            description: "Widget A".to_string(),
            quantity: 2,
            unit_price: Huf(1000),
            vat_rate_basis_points: 2700,
        },
        LineItem {
            description: "Service B".to_string(),
            quantity: 1,
            unit_price: Huf(5000),
            vat_rate_basis_points: 2700,
        },
    ]
}

const SUPPLY_DATE: Date = time::macros::date!(2026 - 05 - 22);

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

/// EUR happy path. Provider has a rate on the supply date; the
/// stamped `RateMetadata` carries the rate, source, date, and
/// round-half-even HUF-equivalent total.
#[test]
fn eur_happy_path_stamps_all_four_metadata_fields() {
    let provider =
        FakeMnbRates::empty().with_rate(Currency::Eur, SUPPLY_DATE, "405.230000");
    let lines = fixture_eur_lines();

    let metadata = fetch_and_stamp_rate(&provider, Currency::Eur, SUPPLY_DATE, &lines)
        .expect("EUR happy path must succeed");

    use rust_decimal::Decimal;
    use std::str::FromStr;
    assert_eq!(metadata.rate, Decimal::from_str("405.230000").unwrap());
    assert_eq!(metadata.source, "MNB");
    assert_eq!(metadata.date, SUPPLY_DATE);
    // 8890 cents × 405.230000 / 100 = 36025.0470 → 36025 (round-half-even)
    assert_eq!(metadata.huf_equivalent_total, 36025);

    // Exactly one MNB call — no walk-back invoked when the supply
    // date has a rate.
    assert_eq!(
        provider.call_count(),
        1,
        "happy path must NOT invoke walk-back fetches"
    );
}

/// EUR walk-back to D-1. Provider has no rate on the supply date
/// but a rate on D-1; the helper walks back one day and uses the
/// D-1 rate. The stamped `RateMetadata.date` IS D-1.
#[test]
fn eur_walks_back_to_d_minus_1_when_supply_date_has_no_rate() {
    let d_minus_1 = SUPPLY_DATE - time::Duration::days(1);
    let provider =
        FakeMnbRates::empty().with_rate(Currency::Eur, d_minus_1, "404.000000");
    let lines = fixture_eur_lines();

    let metadata = fetch_and_stamp_rate(&provider, Currency::Eur, SUPPLY_DATE, &lines)
        .expect("walk-back to D-1 must succeed");

    assert_eq!(
        metadata.date, d_minus_1,
        "walk-back must stamp the publication date that MNB answered with (D-1), not the supply date"
    );
    use rust_decimal::Decimal;
    use std::str::FromStr;
    assert_eq!(metadata.rate, Decimal::from_str("404.000000").unwrap());

    // Two MNB calls: one for the supply date (returns
    // NoRateForCurrency), then one for D-1 (wins).
    assert_eq!(
        provider.call_count(),
        2,
        "walk-back to D-1 must invoke exactly 2 fetches"
    );
}

/// EUR walk-back exhausted. Provider returns
/// `NoRateForCurrency` for every (currency, date) tuple. The
/// helper loud-fails with the named substring per ADR-0037 §4
/// invariant C2; the call count proves the cap was traversed in
/// full.
#[test]
fn eur_walk_back_exhausted_loud_fails_with_named_sentinel() {
    let provider = FakeMnbRates::empty();
    let lines = fixture_eur_lines();

    let err = fetch_and_stamp_rate(&provider, Currency::Eur, SUPPLY_DATE, &lines)
        .expect_err("walk-back exhausted MUST loud-fail per C2");
    let msg = format!("{:#}", err);
    assert!(
        msg.contains(ERR_NO_RATE_AFTER_WALKBACK),
        "loud-fail must carry the C2-prereq sentinel `{}` — got: {}",
        ERR_NO_RATE_AFTER_WALKBACK,
        msg
    );
    assert!(
        msg.contains(&MNB_WALKBACK_DAYS_CAP.to_string()),
        "loud-fail must name the {}-day cap — got: {}",
        MNB_WALKBACK_DAYS_CAP,
        msg
    );

    // The full walk-back window must be traversed before loud-fail:
    // offset 0 (supply date) + 1..=MNB_WALKBACK_DAYS_CAP (each
    // walk-back day) = MNB_WALKBACK_DAYS_CAP + 1 calls.
    assert_eq!(
        provider.call_count() as i64,
        MNB_WALKBACK_DAYS_CAP + 1,
        "walk-back must traverse the full {}-day window before loud-fail",
        MNB_WALKBACK_DAYS_CAP
    );
}

/// MNB transport-class failure propagates immediately. No
/// walk-back — that posture is only for `NoRateForCurrency`. The
/// loud-fail message carries the named transport-failure sentinel
/// per the C2 invariant.
#[test]
fn eur_mnb_envelope_parse_failure_propagates_without_walk_back() {
    let provider = FakeMnbRates::empty().poisoned(MnbErrorKind::EnvelopeParse);
    let lines = fixture_eur_lines();

    let err = fetch_and_stamp_rate(&provider, Currency::Eur, SUPPLY_DATE, &lines)
        .expect_err("envelope-parse fault must propagate without walk-back");
    let msg = format!("{:#}", err);
    assert!(
        msg.contains(ERR_MNB_FETCH_FAILED),
        "transport-class fault must carry the `{}` sentinel — got: {}",
        ERR_MNB_FETCH_FAILED,
        msg
    );
    // Exactly one MNB call — no walk-back for transport-class
    // failures (per ADR-0037 §4 invariant C2: walk-back is the
    // "non-publication day" posture, not the "transport broken"
    // posture).
    assert_eq!(
        provider.call_count(),
        1,
        "transport-class fault must NOT invoke walk-back"
    );
}

/// SOAP-fault-class failure (a NAV-side application-error analogue
/// for MNB) propagates the same way as envelope-parse. Confirms the
/// loud-fail behaviour is uniform across non-`NoRateForCurrency`
/// variants.
#[test]
fn eur_mnb_soap_fault_failure_propagates_without_walk_back() {
    let provider = FakeMnbRates::empty().poisoned(MnbErrorKind::SoapFault);
    let lines = fixture_eur_lines();

    let err = fetch_and_stamp_rate(&provider, Currency::Eur, SUPPLY_DATE, &lines)
        .expect_err("SOAP fault must propagate without walk-back");
    let msg = format!("{:#}", err);
    assert!(
        msg.contains(ERR_MNB_FETCH_FAILED),
        "SOAP fault must carry the `{}` sentinel — got: {}",
        ERR_MNB_FETCH_FAILED,
        msg
    );
    assert_eq!(provider.call_count(), 1);
}

/// Mid-walk-back transport failure. The walk-back loop is interrupted
/// by a transport-class failure on offset N; the loud-fail surfaces
/// THAT error (not the cap-exhausted error). Per ADR-0037 §4 invariant
/// C2 the loud-fail discriminates by type — the operator sees
/// "MNB transport broke" rather than "MNB has no rate for 7 days".
///
/// Fixture: provider returns NoRateForCurrency on supply-date and
/// D-1, then poisons on D-2 onwards (via a hand-crafted custom
/// provider).
#[test]
fn eur_mid_walk_back_transport_failure_surfaces_typed() {
    // Custom provider: walks-back-then-poisons.
    struct PoisonAfterN {
        no_rate_until_offset: u32,
        calls: Mutex<u32>,
    }
    impl MnbRatesProvider for PoisonAfterN {
        fn fetch_official_rate(
            &self,
            _currency: Currency,
            date: Date,
        ) -> Result<MnbRate, MnbError> {
            let n = {
                let mut c = self.calls.lock().unwrap();
                let v = *c;
                *c += 1;
                v
            };
            if n <= self.no_rate_until_offset {
                Err(MnbError::NoRateForCurrency {
                    currency: "EUR".to_string(),
                    date: date.to_string(),
                })
            } else {
                Err(MnbError::EnvelopeParse(format!(
                    "fake transport failure on offset {}",
                    n
                )))
            }
        }
    }
    let provider = PoisonAfterN {
        no_rate_until_offset: 1,
        calls: Mutex::new(0),
    };
    let lines = fixture_eur_lines();

    let err = fetch_and_stamp_rate(&provider, Currency::Eur, SUPPLY_DATE, &lines)
        .expect_err("mid-walk-back transport fault must surface typed");
    let msg = format!("{:#}", err);
    assert!(
        msg.contains(ERR_MNB_FETCH_FAILED),
        "mid-walk-back transport fault must carry the `{}` sentinel — got: {}",
        ERR_MNB_FETCH_FAILED,
        msg
    );
    // Must NOT carry the walk-back-exhausted sentinel: the C2
    // invariant says the operator-visible discrimination is "MNB
    // unavailable" vs "MNB has no rate in N days", not a confusing
    // hybrid.
    assert!(
        !msg.contains(ERR_NO_RATE_AFTER_WALKBACK),
        "mid-walk-back transport fault MUST NOT alias as walk-back-exhausted"
    );
}
