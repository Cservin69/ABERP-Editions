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
        let body: QuoteListResponse = self.get_json(&url, "list response").await?;
        Ok(body.quotes)
    }

    pub async fn fetch_quote(&self, quote_id: &str) -> Result<Quote, QuoteIntakeError> {
        let url = format!(
            "{}/api/quotes/{}",
            self.base_url,
            url_encode_segment(quote_id)
        );
        self.get_json(&url, "single-quote body").await
    }

    /// S348 / PR-39 (F1) — the shared authed-GET-then-parse path with the
    /// Content-Type gate that the S347 sweep added to the priced-writeback
    /// POST, now applied to the intake daemon's two list/single parses.
    ///
    /// Order of verdicts mirrors `classify_response_gate` in the pricing
    /// pipeline: auth (401/503) takes precedence, THEN the Content-Type gate
    /// runs BEFORE `serde_json` ever sees the body — so a 200 `text/html`
    /// (CDN serving the SPA shell, the 2026-06-11 misroute) is refused as
    /// [`QuoteIntakeError::RoutingMisconfigured`] instead of producing a
    /// confusing `Parse` error indistinguishable from a real contract drift.
    /// A non-2xx HTML/text body is carried as
    /// [`QuoteIntakeError::NonJsonResponse`] (closing the F11 body-drop on
    /// the non-JSON path); a non-2xx JSON body keeps the historical
    /// [`map_status`] classification.
    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        ctx: &str,
    ) -> Result<T, QuoteIntakeError> {
        let response = self.send_authed_get(url).await?;
        let status = response.status();
        // Auth verdicts are actionable as auth regardless of the body the CDN
        // attached — keep them ahead of the Content-Type gate (S347 posture).
        match status.as_u16() {
            401 => return Err(QuoteIntakeError::Unauthorized),
            503 => return Err(QuoteIntakeError::ServiceUnavailable),
            _ => {}
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let body = response
            .text()
            .await
            .map_err(|e| QuoteIntakeError::Transport(format!("{ctx} read body: {}", scrub(e))))?;
        // Normalise: drop the `; charset=…` parameter, trim, lowercase.
        let ct_norm = content_type.as_deref().map(|c| {
            c.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        });
        if ct_norm.as_deref() != Some("application/json") {
            let ct = ct_norm.unwrap_or_default();
            let excerpt = body_excerpt(&body);
            if status.as_u16() == 200 && ct.starts_with("text/html") {
                return Err(QuoteIntakeError::RoutingMisconfigured {
                    status: status.as_u16(),
                    content_type: ct,
                    body_excerpt: excerpt,
                });
            }
            return Err(QuoteIntakeError::NonJsonResponse {
                status: status.as_u16(),
                content_type: ct,
                body_excerpt: excerpt,
            });
        }
        // application/json — a non-2xx error body keeps the historical
        // status-based classification (no Content-Type ambiguity here).
        if !status.is_success() {
            return Err(map_status(status));
        }
        serde_json::from_str::<T>(&body).map_err(|e| QuoteIntakeError::Parse(format!("{ctx}: {e}")))
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
        let value = format!("Bearer {}", *self.bearer_token);
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

/// S348 / PR-39 (F1) — first [`BODY_EXCERPT_MAX`] chars of a trimmed,
/// bearer-scrubbed response body. Mirrors the pricing pipeline's
/// `response_excerpt` (200-char bound) so a verbose HTML error page can't
/// bloat the audit detail. Defensive `Bearer ` scrub even though the body
/// is the storefront's response, not our request.
const BODY_EXCERPT_MAX: usize = 200;

fn body_excerpt(body: &str) -> String {
    let mut s = body.trim().to_string();
    if let Some(pos) = s.find("Bearer ") {
        s.replace_range(pos.., "Bearer <redacted>");
    }
    s.chars().take(BODY_EXCERPT_MAX).collect()
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

    // ── S348 / PR-39 (F1) — Content-Type gate on the intake parses ────────

    #[test]
    fn s348_body_excerpt_scrubs_bearer_and_bounds_length() {
        let raw = format!("  leak Bearer SECRET-TOKEN {}", "x".repeat(500));
        let e = body_excerpt(&raw);
        assert!(!e.contains("SECRET-TOKEN"), "{e}");
        assert!(e.contains("Bearer <redacted>"), "{e}");
        assert!(e.chars().count() <= BODY_EXCERPT_MAX, "len {}", e.len());
    }

    fn s348_http_canned(status_line: &str, content_type: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status_line}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    async fn s348_spawn_intake_mock(response: String) -> std::net::SocketAddr {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(x) => x,
                    Err(_) => break,
                };
                let response = response.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 16 * 1024];
                    let _ = sock.read(&mut buf).await;
                    let _ = sock.write_all(response.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        addr
    }

    fn s348_transport(addr: &std::net::SocketAddr) -> QuoteIntakeTransport {
        let cfg = QuoteIntakeConfig {
            base_url: format!("http://{addr}"),
            bearer_token: Zeroizing::new("t0k3n".to_string()),
            poll_interval: std::time::Duration::from_secs(60),
            enabled: true,
        };
        QuoteIntakeTransport::new(&cfg).expect("transport")
    }

    #[tokio::test]
    async fn s348_list_html_200_is_routing_misconfigured_not_parse() {
        // THE incident: the CDN serves the SPA shell as 200 text/html. It
        // must be refused at the gate, never reach serde_json as a `Parse`.
        let addr = s348_spawn_intake_mock(s348_http_canned(
            "200 OK",
            "text/html; charset=utf-8",
            "<!doctype html><html>spa</html>",
        ))
        .await;
        match s348_transport(&addr).list_approved_quotes().await {
            Err(QuoteIntakeError::RoutingMisconfigured {
                status,
                content_type,
                ..
            }) => {
                assert_eq!(status, 200);
                assert_eq!(content_type, "text/html");
            }
            other => panic!("expected RoutingMisconfigured, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn s348_list_401_is_unauthorized() {
        let addr = s348_spawn_intake_mock(s348_http_canned(
            "401 Unauthorized",
            "application/json",
            r#"{"message":"no"}"#,
        ))
        .await;
        assert!(matches!(
            s348_transport(&addr).list_approved_quotes().await,
            Err(QuoteIntakeError::Unauthorized)
        ));
    }

    #[tokio::test]
    async fn s348_list_500_html_is_non_json_with_body() {
        // A 5xx HTML error page: F11 — the body is carried, not dropped.
        let addr = s348_spawn_intake_mock(s348_http_canned(
            "500 Internal Server Error",
            "text/html",
            "<html>upstream boom</html>",
        ))
        .await;
        match s348_transport(&addr).list_approved_quotes().await {
            Err(QuoteIntakeError::NonJsonResponse {
                status,
                body_excerpt,
                ..
            }) => {
                assert_eq!(status, 500);
                assert!(body_excerpt.contains("upstream boom"), "{body_excerpt}");
            }
            other => panic!("expected NonJsonResponse, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn s348_list_500_json_keeps_unexpected_status() {
        // A 5xx with a genuine JSON body still classifies by status (no
        // Content-Type ambiguity) — the historical UnexpectedStatus path.
        let addr = s348_spawn_intake_mock(s348_http_canned(
            "500 Internal Server Error",
            "application/json",
            r#"{"error":"db down"}"#,
        ))
        .await;
        match s348_transport(&addr).list_approved_quotes().await {
            Err(QuoteIntakeError::UnexpectedStatus { status }) => assert_eq!(status, 500),
            other => panic!("expected UnexpectedStatus, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn s348_list_200_malformed_json_is_parse() {
        // 200 + application/json but not the {quotes:[...]} envelope → Parse
        // (a genuine contract drift, correctly distinct from a routing miss).
        let addr = s348_spawn_intake_mock(s348_http_canned(
            "200 OK",
            "application/json",
            r#"{"unexpected":"shape"}"#,
        ))
        .await;
        assert!(matches!(
            s348_transport(&addr).list_approved_quotes().await,
            Err(QuoteIntakeError::Parse(_))
        ));
    }

    #[tokio::test]
    async fn s348_fetch_quote_html_200_is_routing_misconfigured() {
        // Site 3 — the single-quote parse gets the same gate.
        let addr = s348_spawn_intake_mock(s348_http_canned(
            "200 OK",
            "text/html",
            "<!doctype html><html>spa</html>",
        ))
        .await;
        assert!(matches!(
            s348_transport(&addr).fetch_quote("q-1").await,
            Err(QuoteIntakeError::RoutingMisconfigured { .. })
        ));
    }

    #[tokio::test]
    async fn s348_list_connection_refused_is_transport() {
        // No server (bind, capture addr, drop) → a transport-class failure,
        // never a parse error. Stands in for the timeout/no-response case
        // (the intake crate folds both into `Transport`).
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        drop(listener);
        assert!(matches!(
            s348_transport(&addr).list_approved_quotes().await,
            Err(QuoteIntakeError::Transport(_))
        ));
    }
}
