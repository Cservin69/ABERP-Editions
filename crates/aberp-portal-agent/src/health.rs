//! "Running or not running" — the Phase-0 health surface
//! (ADR-0113 §5).
//!
//! This is the whole reason the agent is a **separate process** from
//! `aberp serve` (§2.2). If health lived inside ABERP, the one state
//! Ervin most wants to see remotely — ABERP is down — would be the one
//! state that could not be reported. Here, the agent's liveness is the
//! portal's liveness and ABERP's liveness is merely a status it
//! observes.
//!
//! # Re-discovery is part of the probe
//!
//! `serve.rs` picks a kernel-assigned port unless `ABERP_HTTPS_PORT`
//! pins one, and writes `runtime.json` at boot (deleting it on graceful
//! shutdown). So the probe re-reads discovery on every tick rather than
//! caching a URL from startup: an ABERP that restarted on a new port is
//! *up*, and an agent that cached the old port would report it down.
//! The absence of the file is itself a down signal — no ABERP boot has
//! completed — and needs no network round trip.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::config::{AgentConfig, UpstreamConfig, UpstreamDiscovery};
use crate::upstream::Upstream;

/// §5.1: "Poll cadence ~10 s, cached; a browser session never triggers
/// a probe storm."
pub const POLL_INTERVAL: Duration = Duration::from_secs(10);

/// What the portal renders (§5.2): up/down, since-when,
/// last-known-good, agent uptime.
#[derive(Debug, Clone, serde::Serialize)]
pub struct HealthView {
    /// The headline.
    pub aberp_up: bool,
    /// RFC-3339 instant at which the current up/down state began.
    pub since: String,
    /// RFC-3339 instant of the last successful probe, if there has ever
    /// been one in this daemon's lifetime.
    pub last_good: Option<String>,
    /// Seconds the agent has been running — the "the portal itself is
    /// fine" signal.
    pub agent_uptime_seconds: u64,
    /// Short, operator-facing explanation when down. Never carries a
    /// response body or a URL with a token in it.
    pub detail: Option<String>,
}

#[derive(Debug)]
struct Inner {
    up: bool,
    since: time::OffsetDateTime,
    last_good: Option<time::OffsetDateTime>,
    detail: Option<String>,
    /// Cached loopback client, keyed by the discovery values it was
    /// built from, so a stable ABERP is probed without rebuilding TLS
    /// state every 10 seconds.
    client: Option<(String, Upstream)>,
}

/// The agent's health observations.
#[derive(Debug)]
pub struct HealthMonitor {
    started: Instant,
    started_at: time::OffsetDateTime,
    inner: Mutex<Inner>,
}

impl HealthMonitor {
    #[must_use]
    pub fn new() -> Self {
        let now = time::OffsetDateTime::now_utc();
        Self {
            started: Instant::now(),
            started_at: now,
            inner: Mutex::new(Inner {
                // Boot state is DOWN, not "unknown": the portal must
                // never claim ABERP is up before it has seen a probe
                // succeed.
                up: false,
                since: now,
                last_good: None,
                detail: Some("no probe has completed yet".to_string()),
                client: None,
            }),
        }
    }

    /// The current view, for the status card and for the proxy gate.
    #[must_use]
    pub fn view(&self) -> HealthView {
        let g = self.lock();
        HealthView {
            aberp_up: g.up,
            since: rfc3339(g.since),
            last_good: g.last_good.map(rfc3339),
            agent_uptime_seconds: self.started.elapsed().as_secs(),
            detail: g.detail.clone(),
        }
    }

    /// `true` iff the last probe found ABERP up. §5.2: the UI hiding a
    /// button is never the enforcement — the proxy consults this too.
    #[must_use]
    pub fn is_up(&self) -> bool {
        self.lock().up
    }

    /// Run one probe and fold the result in.
    pub async fn tick(&self, cfg: &AgentConfig) {
        match self.upstream_for(&resolve_upstream_config(cfg)) {
            Err(detail) => self.set(false, Some(detail)),
            Ok(client) => match client.probe_health().await {
                Ok(r) if r.status == 200 => self.set(true, None),
                Ok(r) => self.set(
                    false,
                    Some(format!("ABERP answered /health with {}", r.status)),
                ),
                Err(e) => {
                    // The error text is the agent's own words about a
                    // connection it made; it carries no request body and
                    // no token.
                    self.set(false, Some(format!("ABERP did not answer: {e}")));
                    self.drop_client();
                }
            },
        }
    }

    /// The loopback client for the current discovery values, rebuilt
    /// when they change.
    pub fn upstream_for(&self, cfg: &UpstreamConfig) -> Result<Upstream, String> {
        let key = format!("{}|{}", cfg.base_url, cfg.tls_fingerprint);
        let mut g = self.lock();
        if let Some((cached_key, client)) = &g.client {
            if cached_key == &key {
                return Ok(client.clone());
            }
        }
        drop(g);
        let client = Upstream::new(cfg).map_err(|e| e.to_string())?;
        g = self.lock();
        g.client = Some((key, client.clone()));
        Ok(client)
    }

    fn drop_client(&self) {
        self.lock().client = None;
    }

    fn set(&self, up: bool, detail: Option<String>) {
        let now = time::OffsetDateTime::now_utc();
        let mut g = self.lock();
        if g.up != up {
            g.since = now;
        }
        g.up = up;
        g.detail = detail;
        if up {
            g.last_good = Some(now);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// When the agent itself started — the §5.2 "agent uptime" figure's
    /// wall-clock anchor.
    #[must_use]
    pub fn started_at(&self) -> time::OffsetDateTime {
        self.started_at
    }
}

impl Default for HealthMonitor {
    fn default() -> Self {
        Self::new()
    }
}

/// Re-resolve Leg C's coordinates for this probe.
#[must_use]
pub fn resolve_upstream_config(cfg: &AgentConfig) -> UpstreamConfig {
    let mut out = cfg.upstream.clone();
    let UpstreamDiscovery::RuntimeJson { tenant } = &cfg.discovery else {
        return out;
    };
    match crate::config::runtime_discovery_path(tenant)
        .ok()
        .as_deref()
        .and_then(crate::config::read_runtime_discovery)
    {
        Some((base, fp)) => {
            out.base_url = base;
            out.tls_fingerprint = fp;
        }
        None => {
            // No discovery file: ABERP has not completed a boot. That
            // is a health signal on its own, with no round trip.
            out.base_url = String::new();
            out.tls_fingerprint = String::new();
        }
    }
    out
}

fn rfc3339(t: time::OffsetDateTime) -> String {
    t.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_monitor_reports_down_not_unknown() {
        let m = HealthMonitor::new();
        let v = m.view();
        assert!(!v.aberp_up);
        assert!(v.last_good.is_none());
        assert!(v.detail.is_some());
    }

    #[test]
    fn transitions_move_since_and_record_last_good() {
        let m = HealthMonitor::new();
        let down_since = m.view().since.clone();
        m.set(true, None);
        let up = m.view();
        assert!(up.aberp_up);
        assert!(up.last_good.is_some());
        assert_ne!(up.since, down_since, "`since` must move on a transition");

        let up_since = up.since.clone();
        // A second consecutive up must NOT reset `since` — the card
        // says "up since", not "up as of the last probe".
        m.set(true, None);
        assert_eq!(m.view().since, up_since);

        m.set(false, Some("stopped".into()));
        let down = m.view();
        assert!(!down.aberp_up);
        assert_ne!(down.since, up_since);
        // Last-known-good survives the outage — it is the whole point
        // of the field (§5.1).
        assert!(down.last_good.is_some());
        assert_eq!(down.detail.as_deref(), Some("stopped"));
    }

    #[test]
    fn an_undiscoverable_aberp_yields_a_down_client_error() {
        let m = HealthMonitor::new();
        let cfg = UpstreamConfig {
            base_url: String::new(),
            tls_fingerprint: String::new(),
            bearer: crate::config::SecretSource::Inline("x".into()),
        };
        assert!(m.upstream_for(&cfg).is_err());
    }
}
