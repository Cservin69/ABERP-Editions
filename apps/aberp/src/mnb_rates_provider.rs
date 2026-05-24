//! MNB-rate provider abstraction for the issuance path (PR-44γ /
//! ADR-0037 §2 + §4 invariants C1 + C2 + C3 prerequisites).
//!
//! # Why a trait
//!
//! The issuance command (apps/aberp/src/issue_invoice.rs) needs to
//! fetch the MNB official mid-rate at issue time for non-HUF invoices.
//! In production the fetch reaches the real MNB SOAP endpoint via the
//! sibling `aberp-mnb-rates` crate (PR-44β). In tests the issuance
//! path must run fully offline — the live test path (`MNB_LIVE_TEST=1`)
//! from PR-44β is env-gated and not the default test surface.
//!
//! Per the session-51 brief's
//! "Live MNB calls in tests" guidance, this module defines a small
//! sync trait the issuance path consumes; the production impl wraps
//! the async [`aberp_mnb_rates::MnbClient`] via a per-call `block_on`
//! on a current-thread tokio runtime (matching the binary's existing
//! pattern in submit_invoice / drain_submission_queue / etc.). The
//! test impl is a `HashMap<(Currency, Date), Result<MnbRate>>`-backed
//! fake — purely sync, no network, no runtime ceremony.
//!
//! # A140 — trait over direct injection
//!
//! The session-51 brief named two alternatives: a trait, or inject the
//! rate value directly into the command. The trait choice has smaller
//! blast radius for two reasons:
//!
//! 1. **Walk-back loop owns retries.** ADR-0037 §2.b's D-1 walk-back
//!    (PR-44γ task #2) calls the fetcher in a bounded loop. Direct
//!    injection of a pre-fetched rate would push the walk-back logic
//!    into every caller; the trait keeps the walk-back local to the
//!    issuance path.
//!
//! 2. **Production wiring stays narrow.** Only the binary's `run()`
//!    constructs the real `MnbClient`-backed provider; the trait's
//!    `dyn` boxing keeps the per-call site (`issue_invoice::run_with_provider`)
//!    free of `MnbClient`-specific type ceremony.
//!
//! The trait is sync to match the surrounding `issue_invoice::run`
//! pipeline (which today has no tokio runtime — the runtime is built
//! only inside the submit / poll-ack subcommands per the existing
//! pattern). The real-impl `block_on` is the standard binary-side
//! sync↔async bridge.

use aberp_billing::Currency;
use aberp_mnb_rates::{MnbClient, MnbError, MnbRate};
use time::Date;

/// Sync abstraction over MNB-rate fetching. The production impl wraps
/// the real [`aberp_mnb_rates::MnbClient`] via a per-call `block_on`
/// on a current-thread tokio runtime; the test impl returns canned
/// values from a `HashMap`.
///
/// The error type is the same [`MnbError`] the `aberp-mnb-rates` crate
/// emits — re-exported here without wrapping so the issuance path's
/// walk-back loop pattern-matches on the same variants
/// (`NoRateForCurrency` is the "walk back" signal; every other variant
/// is an immediate loud-fail per ADR-0037 §4 invariant C2).
pub trait MnbRatesProvider: Send {
    /// Fetch the MNB official mid-rate for `currency` on `date`. The
    /// returned [`MnbRate::date`] may walk back to MNB's most-recent
    /// prior publication date if `date` was a non-publication day per
    /// ADR-0037 §2.b — consumers MUST read the returned date when
    /// populating the printed-invoice `Exchange-rate date` field per
    /// ADR-0037 §1.a.
    fn fetch_official_rate(
        &self,
        currency: Currency,
        date: Date,
    ) -> Result<MnbRate, MnbError>;
}

/// Production impl backed by the real `aberp_mnb_rates::MnbClient`.
/// Owns a current-thread tokio runtime (built once at construction);
/// the per-call `block_on` runs the async `fetch_official_rate`
/// without per-call runtime ceremony. Matching the existing binary-
/// side pattern in `submit_invoice::run` etc. (each subcommand owns
/// its own runtime) — but lifted to construction-time here because
/// the issuance walk-back may call `fetch_official_rate` up to N
/// times in one issuance (per A139's 7-day cap).
pub struct LiveMnbRatesProvider {
    client: MnbClient,
    runtime: tokio::runtime::Runtime,
}

impl LiveMnbRatesProvider {
    /// Build a live provider targeting the public MNB endpoint with
    /// the default request timeout. Returns an `anyhow::Error` on
    /// runtime-build failure (lifted to anyhow at the binary boundary
    /// per ADR-0021 Part A item 2; runtime-build is rare OOM territory
    /// and surfaces as the binary's top-level loud-fail).
    pub fn new() -> anyhow::Result<Self> {
        use anyhow::Context;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("build tokio current-thread runtime for MNB rate fetch")?;
        let client = MnbClient::new().context("build MNB client")?;
        Ok(Self { client, runtime })
    }
}

impl MnbRatesProvider for LiveMnbRatesProvider {
    fn fetch_official_rate(
        &self,
        currency: Currency,
        date: Date,
    ) -> Result<MnbRate, MnbError> {
        self.runtime
            .block_on(self.client.fetch_official_rate(currency, date))
    }
}
