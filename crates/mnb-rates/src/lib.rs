//! MNB exchange-rate fetcher per ADR-0037 §2 (PR-44β).
//!
//! Hand-rolled SOAP client over the Magyar Nemzeti Bank's published
//! `GetExchangeRates` web service. The crate exposes one async entry
//! point — [`MnbClient::fetch_official_rate`] — and one parsed value
//! type — [`MnbRate`]. The closed `Currency` vocab the fetcher keys
//! on is reused from `aberp-billing` per ADR-0037 §3 / A133 (the
//! type-domain lift that landed in PR-44α).
//!
//! # Posture per ADR-0037 §4
//!
//! - **C2 (refuse on MNB unavailable).** Every failure path through
//!   this crate is loud per CLAUDE.md rule 12 — there is no silent
//!   fallback rate, no cached-but-stale fallback, no alternate
//!   source. A consumer-side error here MUST transition into a
//!   loud-fail at the issuance command boundary (PR-44γ).
//! - **C3 (date alignment).** The fetcher returns the date MNB
//!   actually answered with. If the caller asked for a
//!   non-publication day and MNB walked back to the most-recent
//!   prior publication date per ADR-0037 §2.b, [`MnbRate::date`]
//!   reflects that walked-back date — the differential is visible
//!   to the consumer.
//!
//! # Posture deliberately deferred at PR-44β
//!
//! - **No (currency, date) cache.** ADR-0037 §2.b + §Open question 2
//!   name caching as optional (`MAY`), with "no cache" as the
//!   default lean until operational evidence surfaces a need. PR-44β
//!   ships without a cache; a future operational case (rate-fetch
//!   latency surveyed by an operator, throttling pressure from MNB)
//!   is the named trigger for adding one. Skipping it now also
//!   means PR-44β does NOT take a dependency on `duckdb` or on the
//!   tenant DuckDB file — the crate is a pure-network library.
//! - **No `ExchangeRateFetched` audit event.** ADR-0037 §2.c +
//!   §Open question 3 name this as named-open with default "no" —
//!   the applied rate already lives inside the NAV-submitted wire
//!   bytes (PR-44δ) that `apps/aberp`'s audit-ledger writer captures
//!   verbatim per ADR-0008 §"Storage". PR-44β surfaces zero new
//!   EventKind variants (F12 four-edit ritual NOT fired).

pub mod error;
pub mod parse;
pub mod soap;

pub use crate::error::MnbError;
pub use crate::parse::{MnbRate, SOURCE};

use std::time::Duration;

use time::macros::format_description;
use time::Date;
use tracing::{debug, error, info, warn};

use aberp_billing::Currency;

/// MNB GetExchangeRates SOAP endpoint. HTTPS form per ADR-0007
/// §Transport hygiene; MNB also serves the plain `http://` URL but
/// TLS is the right default when both work.
pub const MNB_ENDPOINT_URL: &str = "https://www.mnb.hu/arfolyamok.asmx";

/// Default per-request timeout. MNB's GetExchangeRates is fast
/// (single-digit-second p99 historically); 10 seconds gives generous
/// headroom without letting a hung connection block the issuance
/// command indefinitely.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Thin wrapper around `reqwest::Client` plus the configured MNB
/// endpoint. Constructed once per process; `fetch_official_rate`
/// is the only method.
pub struct MnbClient {
    client: reqwest::Client,
    endpoint_url: String,
}

impl MnbClient {
    /// Build a client targeting the public MNB endpoint with the
    /// default request timeout. Public-WebPKI trust is correct for
    /// MNB (the endpoint cert chains to standard Hungarian /
    /// public CAs); a per-anchor pin like nav-transport's NAV pin
    /// would be over-fitting (MNB rotates issuers more often and
    /// has no published anchor list).
    pub fn new() -> Result<Self, MnbError> {
        let client = reqwest::Client::builder()
            .timeout(DEFAULT_REQUEST_TIMEOUT)
            .build()
            .map_err(MnbError::ClientBuild)?;
        Ok(Self {
            client,
            endpoint_url: MNB_ENDPOINT_URL.to_string(),
        })
    }

    /// Override the endpoint URL — used by `tests/live_mnb.rs`
    /// only in case an operator wants to point at MNB's HTTP form
    /// or a local mock during incident triage. Production callers
    /// use [`MnbClient::new`].
    pub fn with_endpoint(mut self, endpoint_url: impl Into<String>) -> Self {
        self.endpoint_url = endpoint_url.into();
        self
    }

    /// Fetch the official MNB rate for `currency` on `date`. The
    /// returned [`MnbRate::date`] may walk back to MNB's
    /// most-recent prior publication date if `date` was a
    /// non-publication day per ADR-0037 §2.b; consumers MUST read
    /// the returned date when populating the printed-invoice
    /// `Exchange-rate date` field per ADR-0037 §1.a.
    pub async fn fetch_official_rate(
        &self,
        currency: Currency,
        date: Date,
    ) -> Result<MnbRate, MnbError> {
        let date_str = format_date(date);
        let currency_iso = currency.iso_code();

        let envelope = soap::render_get_exchange_rates_request(&date_str, currency_iso)?;

        debug!(
            target: "mnb_rates",
            currency = %currency_iso,
            date = %date_str,
            endpoint = %self.endpoint_url,
            "MNB fetch_official_rate request",
        );

        let response = self
            .client
            .post(&self.endpoint_url)
            .header("Content-Type", "text/xml; charset=utf-8")
            .header(
                "SOAPAction",
                format!("\"{}\"", soap::SOAP_ACTION_GET_RATES),
            )
            .body(envelope)
            .send()
            .await
            .map_err(|e| {
                warn!(
                    target: "mnb_rates",
                    currency = %currency_iso,
                    date = %date_str,
                    error = %e,
                    "MNB fetch_official_rate transport failure",
                );
                MnbError::Http(e)
            })?;

        let status = response.status();
        let body = response.bytes().await.map_err(MnbError::Http)?;

        if !status.is_success() {
            error!(
                target: "mnb_rates",
                currency = %currency_iso,
                date = %date_str,
                status = status.as_u16(),
                "MNB fetch_official_rate non-success HTTP status",
            );
            return Err(MnbError::HttpStatus {
                status: status.as_u16(),
            });
        }

        let rate = parse::parse_get_exchange_rates_response(&body, currency)?;
        info!(
            target: "mnb_rates",
            currency = %currency_iso,
            requested_date = %date_str,
            returned_date = %format_date(rate.date),
            value = %rate.value,
            unit = rate.unit,
            "MNB fetch_official_rate ok",
        );
        Ok(rate)
    }
}

fn format_date(d: Date) -> String {
    let fmt = format_description!("[year]-[month]-[day]");
    d.format(&fmt)
        .unwrap_or_else(|_| "INVALID-DATE".to_string())
}

#[cfg(test)]
mod lib_tests {
    use super::*;

    #[test]
    fn format_date_emits_iso_8601() {
        let d = time::macros::date!(2026 - 05 - 22);
        assert_eq!(format_date(d), "2026-05-22");
    }

    #[test]
    fn source_constant_pins_adr_0037_section_1_a_literal() {
        // Pin per ADR-0037 §1.a: the printed-invoice
        // `Exchange-rate source name` is the literal string "MNB".
        // PR-44ε's SPA render reads this constant; a regression
        // here drifts the regulatory record.
        assert_eq!(SOURCE, "MNB");
    }
}
