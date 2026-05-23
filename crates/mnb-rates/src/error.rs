//! [`MnbError`] — the public failure surface of `aberp-mnb-rates`.
//!
//! Every variant is loud per CLAUDE.md rule 12 and ADR-0037 §2.a's
//! refusal posture: when MNB is unavailable, the consumer
//! (PR-44γ's issuance command) MUST loud-fail rather than fall back
//! to a stale rate or a non-MNB source. There is no "silent fallback"
//! path through this module.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum MnbError {
    /// `reqwest::ClientBuilder::build()` failed when constructing the
    /// MNB HTTPS client. Same shape as
    /// `NavTransportError::ClientBuild`; held distinct so the audit
    /// trail can distinguish a NAV-side client-build failure from an
    /// MNB-side one.
    #[error("failed to build reqwest::Client for MNB: {0}")]
    ClientBuild(#[source] reqwest::Error),

    /// HTTP-layer failure on the MNB GetExchangeRates call (DNS,
    /// connection reset, TLS handshake, timeout). Maps directly to
    /// ADR-0037 §4 invariant C2 — the consumer treats this as
    /// "MNB rate unavailable for <date>; refuse to issue".
    #[error("MNB GetExchangeRates HTTP call failed: {0}")]
    Http(#[source] reqwest::Error),

    /// MNB returned a non-success HTTP status. Body content is
    /// captured by the caller (the orchestration in PR-44γ) for the
    /// operator-visible error message; this variant carries only the
    /// status code.
    #[error("MNB GetExchangeRates returned non-success HTTP status: {status}")]
    HttpStatus { status: u16 },

    /// The outer SOAP envelope could not be parsed as XML, or the
    /// expected `<GetExchangeRatesResult>` element was absent. Loud
    /// per CLAUDE.md rule 12 — silent acceptance of a malformed
    /// envelope is exactly the failure mode the C2 invariant forbids.
    #[error("MNB SOAP envelope parse failed: {0}")]
    EnvelopeParse(String),

    /// MNB returned a SOAP Fault. The fault `faultstring` is included
    /// for operator triage; the underlying cause is typically a
    /// transient MNB-side issue (the operator re-runs after the
    /// transient cause resolves).
    #[error("MNB SOAP fault: {0}")]
    SoapFault(String),

    /// The inner `<MNBExchangeRates>` payload could not be parsed as
    /// XML, or its shape did not match the expected
    /// `<Day><Rate curr="..." unit="...">value</Rate></Day>` form.
    #[error("MNB inner exchange-rates payload parse failed: {0}")]
    PayloadParse(String),

    /// The request asked for a currency MNB did not return (e.g., a
    /// future `Currency::Usd` for which MNB's response was empty for
    /// the requested date). Loud per CLAUDE.md rule 12 — the consumer
    /// MUST surface "no rate available" rather than coerce to a
    /// neighbouring currency.
    #[error("MNB response carried no rate for currency {currency} on date {date}")]
    NoRateForCurrency { currency: String, date: String },

    /// The rate's decimal text was not a parseable decimal. MNB
    /// publishes rates as decimal strings (typically comma-separated
    /// in the Hungarian convention; normalized to dot at parse time).
    /// A surprise format here is loud per CLAUDE.md rule 12.
    #[error("MNB rate value `{value}` is not a parseable decimal")]
    MalformedDecimal { value: String },

    /// The requested currency is not present in the closed
    /// `Currency` vocab (ADR-0037 §3) — e.g., a caller passed a
    /// future enum variant the fetcher does not yet support. Today
    /// this fires only if a downstream additively widens `Currency`
    /// without updating [`super::iso_code_for_mnb`] in lockstep; the
    /// exhaustive `match` in that helper would surface the gap as a
    /// compile error first.
    #[error("currency {0:?} is not supported by the MNB fetcher")]
    UnsupportedCurrency(aberp_billing::Currency),
}
