//! Leg B, from the Mac's side: **ask, never listen** (ADR-0115 §G1,
//! §2.2).
//!
//! > The Mac only ever dials out. The frozen prod invoice box gains
//! > **zero** new listening ports.
//!
//! There is no `TcpListener` anywhere in this crate — that absence *is*
//! the top-ranked security goal, and a reader checking G1 can grep for
//! `bind(` and find nothing.
//!
//! # Why this replaced the tunnel
//!
//! Phase 0 originally held a framed connection open to the relay.
//! Ervin's transport decision: *"no existing tunnels, just a Mac
//! querying."* So this module is now a poll loop, the same pattern
//! `crates/aberp-quote-intake` already uses against the storefront:
//!
//! 1. long-poll `POST /agent/v3/poll`, asking the relay to hold the
//!    request for up to [`proto::MAX_POLL_WAIT`];
//! 2. run whatever work comes back, locally, on the Mac;
//! 3. post each answer to `POST /agent/v3/deliver`.
//!
//! Both legs are outbound HTTPS over the same mutually-pinned TLS
//! config the tunnel used, so the §2.3 posture is unchanged: the public
//! WebPKI is not consulted, and an unpinned peer fails inside the
//! handshake.
//!
//! # The epoch, and what it is for
//!
//! Sessions used to be bound to a tunnel id, and a tunnel drop revoked
//! them. With nothing held open there is no drop to observe, so
//! sessions are bound to an **epoch** instead — a generation id the
//! agent mints and the relay echoes.
//!
//! The agent rotates it whenever the relay reports
//! [`PollResponse::known_epoch`]` == false`, which means the relay had
//! no live presence for this epoch when the poll arrived: it restarted,
//! or the Mac was away long enough for the lease to lapse. Either way
//! the relay's memory of the Mac was discontinuous, so every session
//! minted under the old epoch is revoked. That is the same guarantee
//! the tunnel drop gave — "a cookie that transited relay memory dies no
//! later than the relay's own memory of the Mac" (§2.4) — obtained
//! without holding a socket.
//!
//! # Silence is the failure this loop watches for
//!
//! A relay that has crashed, been firewalled, or been taken over and
//! told to drop canary batches produces the same observable as a quiet
//! internet: nothing. So every poll response carries a
//! [`proto::Heartbeat`], and a gap in them is reported to the canary
//! watch on the Mac — the side that owns the alert path. The design
//! does not depend on the heartbeat's *contents* being true; a hostile
//! relay can lie about every counter in it. It depends only on it
//! arriving.

use std::sync::Arc;
use std::time::{Duration, Instant};

use aberp_portal_core::proto::{
    self, AgentIdentity, Delivery, DeliveryAck, PollRequest, PollResponse, Work, DELIVER_PATH,
    MAX_BODY_BYTES, MAX_POLL_WAIT, POLL_PATH, PROTOCOL_VERSION,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

use crate::agent::Agent;
use crate::audit::Event;
use crate::rand;

/// First reconnect delay.
pub const BACKOFF_MIN: Duration = Duration::from_secs(1);
/// Ceiling. A minute is short enough that Ervin's next attempt after a
/// relay reboot succeeds, long enough not to hammer a dead VPS.
pub const BACKOFF_MAX: Duration = Duration::from_secs(60);

/// How long the agent gives one poll before treating it as failed.
///
/// Comfortably longer than [`MAX_POLL_WAIT`] so an ordinary empty
/// long-poll is never mistaken for a timeout.
pub const POLL_TIMEOUT: Duration = Duration::from_secs(40);

/// How long the agent gives one delivery.
pub const DELIVER_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the relay may be silent before the canary watch is told.
///
/// Two missed poll cycles plus slack. Shorter would page on an ordinary
/// network hiccup; much longer and a relay taken offline to suppress
/// alerts buys real quiet time.
pub const SILENCE_ALERT_AFTER: Duration = Duration::from_secs(180);

#[derive(Debug, thiserror::Error)]
pub enum PollLoopError {
    #[error("reading the agent client certificate {path}: {source}")]
    CertRead {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("agent client certificate {path} contains no certificate")]
    CertEmpty { path: String },
    #[error("agent client key is not a usable PEM private key")]
    KeyMalformed,
    #[error("reading the agent client key: {0}")]
    KeySource(#[from] crate::config::SecretError),
    #[error("Leg B TLS config: {0}")]
    Pin(#[from] aberp_portal_core::PinError),
    #[error("building the Leg B HTTP client: {0}")]
    Client(#[source] reqwest::Error),
    #[error("polling {url}: {source}")]
    Poll {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("the relay answered a poll with HTTP {status} — is this agent pinned there?")]
    PollStatus { status: u16 },
    #[error("the relay's poll response was not valid JSON: {0}")]
    PollDecode(String),
    #[error("the relay's poll response exceeded {MAX_BODY_BYTES} bytes")]
    PollTooLarge,
    #[error("minting the epoch: {0}")]
    Rand(#[from] rand::RandError),
    #[error("knock token: {0}")]
    Knock(#[from] crate::knock::KnockError),
}

/// Everything the loop carries across one relay connection's lifetime.
struct Loop {
    agent: Arc<Agent>,
    client: reqwest::Client,
    poll_url: String,
    deliver_url: String,
    /// The generation sessions are bound to.
    epoch: String,
    /// `true` once the relay has confirmed it knows the current epoch.
    ///
    /// Load-bearing, and the absence of it was a live bug: the FIRST
    /// poll of any epoch necessarily reports `known_epoch == false`,
    /// because the relay genuinely did not know it until that poll
    /// arrived. Treating that as "the relay forgot me" rotated the
    /// epoch on every single poll, which rotated it again on the next
    /// one — an infinite rotation that cleared the parked queue each
    /// time, so no browser request ever survived to be answered.
    ///
    /// The question that actually matters is *discontinuity*: did the
    /// relay forget an epoch it had already acknowledged? That needs
    /// this one bit of memory.
    epoch_established: bool,
    /// Highest canary sequence durably recorded, acknowledged on the
    /// next poll.
    ack_canary_seq: u64,
    /// Highest heartbeat sequence seen, so a relay that rewinds its
    /// counter is visible.
    last_heartbeat_seq: u64,
    /// When a heartbeat last arrived. The silence detector.
    last_heartbeat_at: Instant,
    /// Set once silence has been reported, so it is reported once per
    /// outage rather than once per poll.
    silence_reported: bool,
}

/// Poll, serve, retry — forever.
pub async fn run_forever(agent: Arc<Agent>) {
    let mut backoff = BACKOFF_MIN;
    loop {
        match connect(&agent).await {
            Ok(mut l) => {
                // A fresh client is a fresh epoch: the relay cannot
                // have known this generation yet.
                agent.sessions.revoke_all();
                let e = l.run().await;
                tracing::warn!(error = %e, "portal poll loop stopped; will retry");
                agent
                    .audit
                    .append(&Event::new("portal.leg_b.down").reason(e.to_string()));
            }
            Err(e) => {
                tracing::warn!(error = %e, "portal Leg B unavailable; will retry");
                agent
                    .audit
                    .append(&Event::new("portal.leg_b.down").reason(e.to_string()));
            }
        }
        tokio::time::sleep(jittered(backoff)).await;
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

/// Build the pinned client. Does not talk to the relay yet.
async fn connect(agent: &Arc<Agent>) -> Result<Loop, PollLoopError> {
    let cfg = &agent.cfg;

    let chain = load_cert_chain(&cfg.client_cert_pem)?;
    let key = load_private_key(&cfg.client_key)?;
    let tls =
        aberp_portal_core::pin::agent_client_config(cfg.relay_fingerprint.clone(), chain, key)?;

    // `use_preconfigured_tls` hands reqwest the exact rustls config
    // built above, so reqwest's own roots are never consulted — the
    // same pattern `upstream.rs` and `nav-transport` use, and the
    // reason a general-purpose HTTP client is safe on a leg whose whole
    // security property is leaf pinning.
    let mut builder = reqwest::ClientBuilder::new()
        .use_preconfigured_tls(tls)
        // No redirect following: a redirect on this leg could only ever
        // move the poll to somewhere unpinned.
        .redirect(reqwest::redirect::Policy::none());

    // When the operator configured a literal address, pin it rather
    // than letting DNS decide where the poll goes. Pinning the leaf
    // certificate already makes a hijacked name harmless, but not
    // resolving at all is one fewer moving part on the leg that is
    // supposed to have none.
    if let Ok(addr) = cfg.relay_addr.parse::<std::net::SocketAddr>() {
        builder = builder.resolve(&cfg.relay_server_name, addr);
    }
    let client = builder.build().map_err(PollLoopError::Client)?;

    let base = cfg.relay_base_url();
    Ok(Loop {
        agent: Arc::clone(agent),
        client,
        poll_url: format!("{base}{POLL_PATH}"),
        deliver_url: format!("{base}{DELIVER_PATH}"),
        epoch: rand::token()?,
        epoch_established: false,
        ack_canary_seq: 0,
        last_heartbeat_seq: 0,
        last_heartbeat_at: Instant::now(),
        silence_reported: false,
    })
}

impl Loop {
    async fn run(&mut self) -> PollLoopError {
        self.agent
            .audit
            .append(&Event::new("portal.leg_b.up").reason(self.poll_url.clone()));
        tracing::info!(relay = %self.poll_url, "portal Leg B up (polling)");

        loop {
            let res = match self.poll_once().await {
                Ok(r) => r,
                Err(e) => {
                    // A failed poll is not immediately an outage — the
                    // silence detector decides that, on its own clock.
                    self.check_silence().await;
                    return e;
                }
            };
            self.note_heartbeat(&res).await;

            if res.known_epoch {
                self.epoch_established = true;
            } else if self.epoch_established {
                // The relay forgot an epoch it had already
                // acknowledged: it restarted, or this Mac was away long
                // enough for the lease to lapse. Every session minted
                // under it may have outlived the relay's memory, so
                // rotate and revoke (§4.4).
                let revoked = self.agent.sessions.revoke_all();
                let previous = std::mem::replace(
                    &mut self.epoch,
                    match rand::token() {
                        Ok(t) => t,
                        Err(e) => return PollLoopError::Rand(e),
                    },
                );
                self.epoch_established = false;
                self.agent
                    .audit
                    .append(&Event::new("portal.epoch.rotated").reason(format!(
                        "relay lost presence for {previous}; {revoked} revoked"
                    )));
                tracing::info!(revoked, "relay reported no live presence; epoch rotated");
            } else {
                // The first poll of a freshly-minted epoch. The relay
                // did not know it a moment ago and knows it now, which
                // is not a discontinuity — it is how an epoch begins.
                // `run_forever` already revoked every session when it
                // minted this epoch.
                self.epoch_established = true;
            }

            for work in res.work {
                match work {
                    Work::Request { id, req } => {
                        // Each request on its own task so a slow PDF
                        // render does not stall the next poll.
                        let this = self.spawn_handle(id, req);
                        drop(this);
                    }
                    Work::Canary { seq, batch } => {
                        // At-least-once: the relay redelivers until
                        // acknowledged, so a sequence already recorded
                        // is dropped rather than alerted on twice.
                        if seq > self.ack_canary_seq {
                            self.ack_canary_seq = seq;
                            let agent = Arc::clone(&self.agent);
                            tokio::spawn(async move { agent.canary.record(&batch).await });
                        }
                    }
                }
            }
        }
    }

    /// Run one request and post its answer back.
    fn spawn_handle(&self, id: u64, req: proto::PortalRequest) -> tokio::task::JoinHandle<()> {
        let agent = Arc::clone(&self.agent);
        let client = self.client.clone();
        let url = self.deliver_url.clone();
        let epoch = self.epoch.clone();
        tokio::spawn(async move {
            // §6.5 hardening: every string the relay supplied is
            // re-sanitised HERE, on the Mac, before it can reach an
            // audit line. The relay is not trusted to have done it —
            // that is the whole point of the trust boundary — and
            // `peer` in particular is attacker-influenced metadata that
            // a hostile relay could decorate with newlines to forge
            // log entries.
            let req = sanitise_request(req);
            let res = agent.handle(&req, &epoch).await;
            let body = Delivery { epoch, id, res };
            match client
                .post(&url)
                .json(&body)
                .timeout(DELIVER_TIMEOUT)
                .send()
                .await
            {
                Ok(r) => {
                    let accepted = r.json::<DeliveryAck>().await.is_ok_and(|a| a.accepted);
                    if !accepted {
                        // The browser gave up, or the epoch rotated
                        // under us. Neither is actionable; both are
                        // worth counting.
                        tracing::debug!(id, "delivery was not accepted");
                    }
                }
                Err(e) => tracing::warn!(id, error = %e, "delivery failed"),
            }
        })
    }

    async fn poll_once(&mut self) -> Result<PollResponse, PollLoopError> {
        let knock_token = self.agent.knock.load_or_mint()?;
        let body = PollRequest {
            agent: AgentIdentity {
                protocol_version: PROTOCOL_VERSION,
                knock_token,
                // The canary needs the label to tell "someone typed the
                // hostname" from "someone hit the IP" — the whole
                // HIGH-versus-LOW distinction. It lives in the relay's
                // memory for the life of the lease and nowhere else,
                // the same posture as the knock token above.
                expected_host: Some(self.agent.cfg.rp_id.clone()),
                tripwire_path: self.agent.cfg.tripwire_path.clone(),
                epoch: self.epoch.clone(),
            },
            wait_ms: MAX_POLL_WAIT.as_millis() as u32,
            ack_canary_seq: self.ack_canary_seq,
        };

        let response = self
            .client
            .post(&self.poll_url)
            .json(&body)
            .timeout(POLL_TIMEOUT)
            .send()
            .await
            .map_err(|source| PollLoopError::Poll {
                url: self.poll_url.clone(),
                source,
            })?;

        let status = response.status();
        if !status.is_success() {
            // Notably includes the relay's own parked 404, which is
            // what a version skew or an unparseable poll looks like on
            // the wire. The agent says so in ITS log; the socket says
            // nothing.
            return Err(PollLoopError::PollStatus {
                status: status.as_u16(),
            });
        }

        // Bounded on this side too: a hostile relay must not be able to
        // OOM the Mac by answering a poll with an endless body. The
        // same constant bounds what the relay accepts from the agent.
        let raw = response
            .bytes()
            .await
            .map_err(|source| PollLoopError::Poll {
                url: self.poll_url.clone(),
                source,
            })?;
        if raw.len() > MAX_BODY_BYTES {
            return Err(PollLoopError::PollTooLarge);
        }
        serde_json::from_slice(&raw).map_err(|e| PollLoopError::PollDecode(e.to_string()))
    }

    /// Record that the relay is alive, and notice if it stops being.
    async fn note_heartbeat(&mut self, res: &PollResponse) {
        if res.heartbeat.seq < self.last_heartbeat_seq {
            // A relay that rewound its counter restarted — or is not
            // the relay we were talking to. Not fatal, but not silent.
            tracing::info!(
                was = self.last_heartbeat_seq,
                now = res.heartbeat.seq,
                "relay heartbeat sequence went backwards; relay restarted"
            );
        }
        self.last_heartbeat_seq = res.heartbeat.seq;
        self.last_heartbeat_at = Instant::now();
        if self.silence_reported {
            self.silence_reported = false;
            self.agent
                .audit
                .append(&Event::new("portal.relay.heartbeat_resumed"));
        }
    }

    /// Report a relay that has gone quiet — once per outage.
    async fn check_silence(&mut self) {
        if self.silence_reported || self.last_heartbeat_at.elapsed() < SILENCE_ALERT_AFTER {
            return;
        }
        self.silence_reported = true;
        let quiet_for = self.last_heartbeat_at.elapsed();
        self.agent
            .audit
            .append(&Event::new("portal.relay.silent").reason(format!("{}s", quiet_for.as_secs())));
        tracing::warn!(
            quiet_for_s = quiet_for.as_secs(),
            "the relay has stopped answering — the canary cannot report while this lasts"
        );
        self.agent.canary.report_silence(quiet_for).await;
    }
}

/// Re-sanitise every relay-supplied string on the Mac side (§6.5).
///
/// The relay is a blind pipe and is *supposed* to forward these
/// verbatim, but "supposed to" is not a control. A relay under an
/// attacker's control can put a newline in `peer` and forge audit
/// lines, or put megabytes in `path` and bloat the log. Both are cheap
/// to prevent here, at the trust boundary, and impossible to detect
/// afterwards.
fn sanitise_request(mut req: proto::PortalRequest) -> proto::PortalRequest {
    use aberp_portal_core::canary::sanitise;
    /// Generous for a real path, and a hard bound on what a hostile
    /// relay can push into an audit line.
    const MAX_FIELD: usize = 256;
    req.method = sanitise(&req.method, 16);
    req.path = sanitise(&req.path, MAX_FIELD);
    req.query = req.query.as_deref().map(|q| sanitise(q, MAX_FIELD));
    req.peer = req.peer.as_deref().map(|p| sanitise(p, 64));
    // `cookie` and `body_b64` are NOT sanitised: they are parsed by
    // code that already treats them as untrusted (a cookie is looked up
    // as a map key, a body is base64-decoded then JSON-parsed), and
    // truncating them would corrupt legitimate values. They never reach
    // a log.
    req
}

fn load_cert_chain(path: &std::path::Path) -> Result<Vec<CertificateDer<'static>>, PollLoopError> {
    let pem = std::fs::read(path).map_err(|source| PollLoopError::CertRead {
        path: path.display().to_string(),
        source,
    })?;
    let chain: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut pem.as_slice())
        .collect::<Result<_, _>>()
        .map_err(|source| PollLoopError::CertRead {
            path: path.display().to_string(),
            source,
        })?;
    if chain.is_empty() {
        return Err(PollLoopError::CertEmpty {
            path: path.display().to_string(),
        });
    }
    Ok(chain)
}

fn load_private_key(
    source: &crate::config::SecretSource,
) -> Result<PrivateKeyDer<'static>, PollLoopError> {
    let pem = source.read()?;
    rustls_pemfile::private_key(&mut pem.as_bytes())
        .ok()
        .flatten()
        .ok_or(PollLoopError::KeyMalformed)
}

/// Full jitter over `[base/2, base]`. Two agents (a future second Mac,
/// §7) retrying after the same relay reboot must not synchronise.
fn jittered(base: Duration) -> Duration {
    let half = base / 2;
    let span = base.saturating_sub(half).as_millis() as u64;
    if span == 0 {
        return base;
    }
    let mut b = [0u8; 8];
    match rand::bytes(8) {
        Ok(v) => b.copy_from_slice(&v),
        // If the CSPRNG is unavailable the daemon has bigger problems;
        // an unjittered retry is the safe degradation here (it delays,
        // it does not weaken a secret).
        Err(_) => return base,
    }
    half + Duration::from_millis(u64::from_be_bytes(b) % (span + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_stays_within_half_of_the_base() {
        for _ in 0..64 {
            let d = jittered(Duration::from_secs(8));
            assert!(d >= Duration::from_secs(4), "{d:?} below the floor");
            assert!(d <= Duration::from_secs(8), "{d:?} above the base");
        }
    }

    #[test]
    fn jitter_on_a_zero_span_is_the_base() {
        assert_eq!(jittered(Duration::from_millis(1)), Duration::from_millis(1));
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let mut b = BACKOFF_MIN;
        for _ in 0..16 {
            b = (b * 2).min(BACKOFF_MAX);
        }
        assert_eq!(b, BACKOFF_MAX);
    }

    /// The state machine `Loop::run` applies to `known_epoch`, lifted
    /// out so it can be tested without a relay.
    ///
    /// Returns `true` when the epoch must rotate.
    fn should_rotate(known_epoch: bool, established: &mut bool) -> bool {
        if known_epoch {
            *established = true;
            false
        } else if *established {
            *established = false;
            true
        } else {
            *established = true;
            false
        }
    }

    #[test]
    fn a_fresh_epoch_does_not_rotate_on_its_own_first_poll() {
        // The bug this pins: the first poll of any epoch necessarily
        // reports `known_epoch == false`, and treating that as "the
        // relay forgot me" rotated on every poll forever — clearing the
        // parked queue each time, so no browser request was ever
        // answered.
        let mut established = false;
        assert!(
            !should_rotate(false, &mut established),
            "rotated on the first poll"
        );
        assert!(established);
        // Steady state: the relay knows us, nothing rotates.
        for _ in 0..10 {
            assert!(!should_rotate(true, &mut established));
        }
    }

    #[test]
    fn a_relay_that_forgets_an_acknowledged_epoch_rotates_exactly_once() {
        let mut established = false;
        should_rotate(false, &mut established); // first poll
        should_rotate(true, &mut established); // acknowledged
                                               // The relay restarts.
        assert!(should_rotate(false, &mut established), "did not rotate");
        // …and the first poll of the NEW epoch must not rotate again.
        assert!(
            !should_rotate(false, &mut established),
            "rotated twice for one outage"
        );
    }

    #[test]
    fn the_poll_timeout_outlives_an_ordinary_empty_long_poll() {
        // Otherwise every idle cycle would look like a relay failure.
        assert!(POLL_TIMEOUT > MAX_POLL_WAIT);
    }

    #[test]
    fn silence_is_reported_only_after_more_than_two_poll_cycles() {
        // Shorter and an ordinary hiccup pages Ervin; the alert that
        // fires on normal operation is the alert that gets ignored.
        assert!(SILENCE_ALERT_AFTER > MAX_POLL_WAIT * 2);
    }

    #[test]
    fn a_pem_with_no_certificate_is_refused() {
        let dir = std::env::temp_dir().join(format!("portal-poll-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let p = dir.join("empty.pem");
        std::fs::write(&p, "# nothing here\n").expect("write");
        assert!(matches!(
            load_cert_chain(&p),
            Err(PollLoopError::CertEmpty { .. })
        ));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_malformed_key_is_refused_rather_than_silently_unused() {
        let src = crate::config::SecretSource::Inline("not a pem".into());
        assert!(matches!(
            load_private_key(&src),
            Err(PollLoopError::KeyMalformed)
        ));
    }

    #[test]
    fn relay_supplied_strings_are_re_sanitised_on_the_mac() {
        // A hostile relay must not be able to forge an audit line by
        // putting a newline in metadata it controls.
        let req = sanitise_request(proto::PortalRequest {
            method: "GET\nkind=portal.auth.verified".into(),
            path: "/api/x\r\ninjected".into(),
            query: Some("a=1\nb=2".into()),
            cookie: Some("s=keep\nthis".into()),
            body_b64: Some("Zm9v".into()),
            peer: Some("203.0.113.7\nkind=portal.session.minted".into()),
        });
        for field in [&req.method, &req.path] {
            assert!(!field.contains('\n') && !field.contains('\r'), "{field:?}");
        }
        assert!(!req.query.as_deref().expect("query").contains('\n'));
        assert!(!req.peer.as_deref().expect("peer").contains('\n'));
        // …and the two fields that must survive intact do.
        assert_eq!(req.cookie.as_deref(), Some("s=keep\nthis"));
        assert_eq!(req.body_b64.as_deref(), Some("Zm9v"));
    }
}
