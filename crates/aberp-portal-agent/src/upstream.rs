//! Leg C — the agent as a client of `aberp serve` over loopback
//! (ADR-0115 §2.1).
//!
//! > Leg C — agent to the local ABERP process over loopback, exactly as
//! > the Tauri shell talks to it today (`Authorization: Bearer`).
//!
//! Three properties, all inherited rather than invented:
//!
//! - **TLS pinned by fingerprint.** `serve.rs` binds `127.0.0.1` with a
//!   self-signed `rcgen` certificate; the fingerprint comes from the
//!   same `runtime.json` discovery file the storefront's dev launcher
//!   reads. Pinning is done the way `apps/aberp-ui/src/pinned_client.rs`
//!   does it, for the reason recorded there.
//! - **The existing bearer, read from the keychain, never leaving the
//!   Mac** (§2.2). ADR-0115 §6.4 names the liability honestly: this is
//!   an all-routes token today. The agent's allowlist confines what can
//!   be *asked*; hardening H2 (a read-only-scoped bearer minted by
//!   `serve.rs`) is what would confine what can be *held*.
//! - **`GET` only, at the type level.** [`Upstream`] exposes exactly one
//!   verb. There is no `post`, no `request(method, …)`, nothing a later
//!   edit could reach for without adding it deliberately — the §6.3
//!   refusal is upstream of this module, and this module has no way to
//!   contradict it.

use std::time::Duration;

use aberp_portal_core::PinnedFingerprint;

use crate::config::{SecretError, UpstreamConfig};

/// How long the agent waits on the local ABERP before calling it down.
/// Short on purpose: §5.1 wants "a short timeout" so a hung backend
/// reads as down rather than hanging the portal.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
/// A read may legitimately take longer than a health probe (the PDF
/// render is synchronous upstream), but never long enough to hold a
/// phone-side request open indefinitely.
pub const READ_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum UpstreamError {
    #[error("ABERP is not discoverable — no base URL or TLS fingerprint (has `aberp serve` ever booted?)")]
    NotConfigured,
    #[error("loopback TLS pin: {0}")]
    Pin(#[from] aberp_portal_core::PinError),
    #[error("building the loopback client: {0}")]
    Client(#[source] reqwest::Error),
    #[error("reading the ABERP bearer: {0}")]
    Bearer(#[from] SecretError),
    #[error("ABERP did not answer: {0}")]
    Unreachable(#[source] reqwest::Error),
}

/// One upstream response, already read into memory.
#[derive(Debug, Clone)]
pub struct UpstreamResponse {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
}

/// The pinned, bearer-carrying loopback client.
#[derive(Debug, Clone)]
pub struct Upstream {
    client: reqwest::Client,
    base_url: String,
    bearer: String,
}

impl Upstream {
    /// Build from config. Returns [`UpstreamError::NotConfigured`] when
    /// ABERP has never booted — the caller turns that into "ABERP:
    /// down" rather than into a daemon failure (§2.2: the agent's
    /// liveness is the portal's liveness).
    pub fn new(cfg: &UpstreamConfig) -> Result<Self, UpstreamError> {
        if cfg.base_url.is_empty() || cfg.tls_fingerprint.is_empty() {
            return Err(UpstreamError::NotConfigured);
        }
        let pinned = PinnedFingerprint::from_hex(&cfg.tls_fingerprint)?;
        let tls = aberp_portal_core::pin::loopback_client_config(pinned)?;
        let client = reqwest::ClientBuilder::new()
            .use_preconfigured_tls(tls)
            .timeout(READ_TIMEOUT)
            .build()
            .map_err(UpstreamError::Client)?;
        Ok(Self {
            client,
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            bearer: cfg.bearer.read()?,
        })
    }

    /// The ONLY verb this type offers. See the module docs.
    pub async fn get(&self, path: &str) -> Result<UpstreamResponse, UpstreamError> {
        self.get_with_timeout(path, READ_TIMEOUT).await
    }

    /// `GET` with an explicit timeout — the health probe uses a short
    /// one so a hung backend does not hold the portal open.
    pub async fn get_with_timeout(
        &self,
        path: &str,
        timeout: Duration,
    ) -> Result<UpstreamResponse, UpstreamError> {
        let url = format!("{}{}", self.base_url, path);
        let resp = self
            .client
            .get(&url)
            .bearer_auth(&self.bearer)
            .timeout(timeout)
            .send()
            .await
            .map_err(UpstreamError::Unreachable)?;
        let status = resp.status().as_u16();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();
        let body = resp
            .bytes()
            .await
            .map_err(UpstreamError::Unreachable)?
            .to_vec();
        Ok(UpstreamResponse {
            status,
            content_type,
            body,
        })
    }

    /// `GET /health` — `serve.rs`'s one deliberately unauthenticated
    /// route (§5.1). The bearer is sent anyway; it costs nothing and
    /// keeps one code path.
    pub async fn probe_health(&self) -> Result<UpstreamResponse, UpstreamError> {
        self.get_with_timeout("/health", PROBE_TIMEOUT).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SecretSource;

    fn cfg(base: &str, fp: &str) -> UpstreamConfig {
        UpstreamConfig {
            base_url: base.into(),
            tls_fingerprint: fp.into(),
            bearer: SecretSource::Inline("test-bearer".into()),
        }
    }

    #[test]
    fn an_undiscovered_aberp_is_not_configured_rather_than_an_error_to_crash_on() {
        assert!(matches!(
            Upstream::new(&cfg("", "")),
            Err(UpstreamError::NotConfigured)
        ));
        assert!(matches!(
            Upstream::new(&cfg("https://127.0.0.1:1", "")),
            Err(UpstreamError::NotConfigured)
        ));
    }

    #[test]
    fn a_malformed_fingerprint_refuses_to_build_a_client() {
        // Never fall back to WebPKI: an unpinned loopback client would
        // silently accept any public CA if the URL ever moved off
        // 127.0.0.1.
        assert!(matches!(
            Upstream::new(&cfg("https://127.0.0.1:1", "not-hex")),
            Err(UpstreamError::Pin(_))
        ));
    }

    #[test]
    fn a_well_formed_config_builds() {
        aberp_portal_core::pin::install_default_crypto_provider();
        let u = Upstream::new(&cfg("https://127.0.0.1:18443/", &hex::encode([7u8; 32])))
            .expect("builds");
        // The trailing slash is normalised away so paths concatenate
        // without producing `//invoices`.
        assert_eq!(u.base_url, "https://127.0.0.1:18443");
    }
}
