//! Thin HTTPS client around the four ABERP-site endpoints.
//!
//! Bearer token rides in a `Zeroizing<String>` and is `set_sensitive`
//! on the `HeaderValue` — never logged. `Debug` impl is hand-rolled
//! to mask the token bytes.

use std::fmt;

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use reqwest::StatusCode;
use zeroize::Zeroizing;

use crate::config::QuoteIntakeConfig;
use crate::error::QuoteIntakeError;
use crate::payload::{Quote, QuoteListResponse, StatusWritebackBody};

const REQUEST_TIMEOUT_SECS: u64 = 10;

pub struct QuoteIntakeTransport {
    base_url: String,
    bearer_token: Zeroizing<String>,
    client: reqwest::Client,
}

impl fmt::Debug for QuoteIntakeTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QuoteIntakeTransport")
            .field("base_url", &self.base_url)
            .field("bearer_token", &"<redacted>")
            .field("client", &"<reqwest::Client>")
            .finish()
    }
}

impl QuoteIntakeTransport {
    pub fn new(config: &QuoteIntakeConfig) -> Result<Self, QuoteIntakeError> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()
            .map_err(|e| QuoteIntakeError::Transport(format!("client build: {e}")))?;
        Ok(Self {
            base_url: config.base_url.clone(),
            bearer_token: config.bearer_token.clone(),
            client,
        })
    }

    pub async fn list_approved_quotes(&self) -> Result<Vec<Quote>, QuoteIntakeError> {
        let url = format!("{}/api/quotes?status=approved", self.base_url);
        let response = self.send_authed_get(&url).await?;
        let status = response.status();
        if !status.is_success() {
            return Err(map_status(status));
        }
        let body: QuoteListResponse = response
            .json()
            .await
            .map_err(|e| QuoteIntakeError::Parse(format!("list response body: {e}")))?;
        Ok(body.quotes)
    }

    pub async fn fetch_quote(&self, quote_id: &str) -> Result<Quote, QuoteIntakeError> {
        let url = format!(
            "{}/api/quotes/{}",
            self.base_url,
            url_encode_segment(quote_id)
        );
        let response = self.send_authed_get(&url).await?;
        let status = response.status();
        if !status.is_success() {
            return Err(map_status(status));
        }
        response
            .json::<Quote>()
            .await
            .map_err(|e| QuoteIntakeError::Parse(format!("single-quote body: {e}")))
    }

    pub async fn writeback_status(
        &self,
        quote_id: &str,
        status: &str,
        notes: &str,
    ) -> Result<(), QuoteIntakeError> {
        let url = format!(
            "{}/api/quotes/{}/status",
            self.base_url,
            url_encode_segment(quote_id)
        );
        let body = StatusWritebackBody {
            status: status.to_string(),
            notes: notes.to_string(),
        };
        let response = self
            .client
            .post(&url)
            .header(AUTHORIZATION, self.bearer_header_value()?)
            .header(CONTENT_TYPE, "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| QuoteIntakeError::Transport(format!("status writeback: {}", scrub(e))))?;
        let status_code = response.status();
        if !status_code.is_success() {
            return Err(map_status(status_code));
        }
        Ok(())
    }

    async fn send_authed_get(&self, url: &str) -> Result<reqwest::Response, QuoteIntakeError> {
        self.client
            .get(url)
            .header(AUTHORIZATION, self.bearer_header_value()?)
            .send()
            .await
            .map_err(|e| QuoteIntakeError::Transport(format!("GET {url}: {}", scrub(e))))
    }

    fn bearer_header_value(&self) -> Result<reqwest::header::HeaderValue, QuoteIntakeError> {
        let value = format!("Bearer {}", &*self.bearer_token);
        let mut hv = reqwest::header::HeaderValue::from_str(&value).map_err(|_| {
            QuoteIntakeError::Config(
                "ABERP_QUOTE_INTAKE_TOKEN contains chars invalid for an HTTP header".to_string(),
            )
        })?;
        hv.set_sensitive(true);
        Ok(hv)
    }
}

fn map_status(status: StatusCode) -> QuoteIntakeError {
    match status.as_u16() {
        401 => QuoteIntakeError::Unauthorized,
        503 => QuoteIntakeError::ServiceUnavailable,
        other => QuoteIntakeError::UnexpectedStatus { status: other },
    }
}

fn scrub(e: reqwest::Error) -> String {
    let mut s = e.to_string();
    if let Some(pos) = s.find("Bearer ") {
        s.replace_range(pos.., "Bearer <redacted>");
    }
    s
}

fn url_encode_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for b in segment.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_does_not_leak_token() {
        std::env::set_var("ABERP_QUOTE_INTAKE_ENABLED", "true");
        std::env::set_var("ABERP_QUOTE_INTAKE_URL", "http://localhost:3000");
        std::env::set_var("ABERP_QUOTE_INTAKE_TOKEN", "S3CRET-VALUE");
        let cfg = QuoteIntakeConfig::from_env().expect("cfg");
        let tx = QuoteIntakeTransport::new(&cfg).expect("tx");
        let dbg = format!("{tx:?}");
        assert!(!dbg.contains("S3CRET-VALUE"), "{dbg}");
        assert!(dbg.contains("<redacted>"), "{dbg}");
        std::env::remove_var("ABERP_QUOTE_INTAKE_ENABLED");
        std::env::remove_var("ABERP_QUOTE_INTAKE_URL");
        std::env::remove_var("ABERP_QUOTE_INTAKE_TOKEN");
    }

    #[test]
    fn map_status_401_to_unauthorized() {
        assert!(matches!(
            map_status(StatusCode::UNAUTHORIZED),
            QuoteIntakeError::Unauthorized
        ));
    }

    #[test]
    fn map_status_503_to_service_unavailable() {
        assert!(matches!(
            map_status(StatusCode::SERVICE_UNAVAILABLE),
            QuoteIntakeError::ServiceUnavailable
        ));
    }

    #[test]
    fn map_status_other_to_unexpected() {
        match map_status(StatusCode::INTERNAL_SERVER_ERROR) {
            QuoteIntakeError::UnexpectedStatus { status } => assert_eq!(status, 500),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn url_encode_segment_passes_uuid() {
        assert_eq!(
            url_encode_segment("11111111-2222-3333-4444-555555555555"),
            "11111111-2222-3333-4444-555555555555"
        );
    }

    #[test]
    fn url_encode_segment_encodes_slash_and_dotdot() {
        assert_eq!(url_encode_segment("a/b"), "a%2Fb");
        assert_eq!(url_encode_segment(".."), "..");
        assert_eq!(url_encode_segment("a b"), "a%20b");
    }
}
