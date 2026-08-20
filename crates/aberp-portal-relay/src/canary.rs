//! The canary trap, front half.
//!
//! Reads `aberp_portal_core::canary` first — it carries the premise
//! (this host has no legitimate unauthenticated traffic, so every
//! un-knocked request is a probe) and the one rule (the trap must never
//! change the response).
//!
//! # How the rule is kept
//!
//! [`Canary::observe`] does exactly three things: read a monotonic
//! clock, build a small struct, and `try_send` it into a bounded
//! channel. No I/O, no lock held across an await, no allocation that
//! depends on severity, and — critically — **no branch whose cost
//! depends on what was observed**. Classification, deduplication,
//! logging and forwarding all happen on [`run_aggregator`], a separate
//! task. A scanner cannot tell a tripwire hit from a stray crawl by
//! timing the response, because the response path did the same work
//! either way.
//!
//! `try_send` and not `send`: an await on a full queue would couple
//! response latency to the aggregator's health, which is the same leak
//! wearing a different hat. A full queue drops the observation and
//! bumps a counter that the next batch reports — a scan large enough to
//! overrun the buffer is itself a finding, so it is stated rather than
//! silently absorbed.
//!
//! # Nothing at rest
//!
//! Per ADR-0113 §2.4 and Ervin's §9.5 decision, the relay keeps no
//! probe log on disk. Observations live in a bounded in-memory window,
//! are coalesced into a [`CanaryBatch`], and are pushed down the tunnel
//! to the Mac, which owns the durable log and the alert. If the tunnel
//! is down the batches wait in a bounded buffer and flush on reconnect;
//! if the relay restarts they are gone, which is the correct trade for
//! a box that is supposed to hold nothing.
//!
//! # Residual: no SNI, no TLS fingerprint
//!
//! [`ProbeSample::sni`] is always `None`. `axum-server`'s rustls
//! acceptor does not surface the handshake's SNI or a JA3-style client
//! fingerprint to the handler, and recovering them means running a
//! custom acceptor and threading per-connection state into the request
//! extensions. That is real work with real regression surface on the
//! listener that must never behave distinguishably, so it is named as
//! Phase-2 rather than half-done here. `named_the_host` covers most of
//! the signal, with the honest caveat that `Host` is client-controlled
//! where SNI is observed — a scanner can send any `Host` it likes, so
//! `NamedTheHost` proves someone *typed* the label, not that TLS was
//! negotiated for it.

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aberp_portal_core::canary::{classify, CanaryBatch, ProbeInput, ProbeSample, Severity};
use aberp_portal_core::proto::Frame;
use tokio::sync::mpsc;

use crate::broker::Broker;

/// How long after passing the knock a source is treated as the
/// operator rather than a probe.
///
/// This exists because of one very concrete false positive: a browser
/// that has just loaded the portal will, entirely on its own, ask the
/// **bare host** for `/favicon.ico` and (on iOS "Add to Home Screen")
/// `/apple-touch-icon.png`. Those requests carry no knock and DO carry
/// the portal's hostname, so without a grace window every legitimate
/// visit would page Ervin at HIGH severity — and an alert that fires on
/// normal use is an alert that gets ignored, which is worse than none.
///
/// The shell also declares inline `data:` icons to stop most of those
/// requests being made at all; this is the belt to that's braces,
/// because browser behaviour here is not something to bet a pager on.
pub const AUTHORISED_GRACE: Duration = Duration::from_secs(5 * 60);

/// Bound on the observation queue. Beyond this, observations are
/// dropped and counted.
const QUEUE_DEPTH: usize = 1024;

/// Ordinary flush cadence.
pub const FLUSH_INTERVAL: Duration = Duration::from_secs(30);

/// Minimum gap between two batches carrying HIGH probes. A burst
/// inside this window is coalesced into one batch — the "a scan is one
/// alert, not a flood" requirement, enforced at the source rather than
/// only at the mailer.
pub const HIGH_COALESCE_WINDOW: Duration = Duration::from_secs(60);

/// Most individual probes carried in one batch. The counts are exact;
/// the samples are evidence, and ten of them is enough to recognise a
/// pattern without turning an alert into a log dump.
pub const MAX_SAMPLES: usize = 10;

/// Most batches held while the tunnel is down.
const MAX_PENDING_BATCHES: usize = 32;

/// Most sources remembered as recently-authorised.
const MAX_AUTHORISED: usize = 256;

/// The aggregator's timing.
///
/// A struct rather than bare constants so a test can drive the real
/// aggregator at a real cadence instead of waiting half a minute, and
/// so the two windows are visibly one decision rather than two scattered
/// constants. [`Default`] is the deployed behaviour.
#[derive(Debug, Clone, Copy)]
pub struct AggregatorConfig {
    /// Ordinary flush cadence.
    pub flush_interval: Duration,
    /// Minimum gap between two batches carrying HIGH probes.
    pub high_coalesce_window: Duration,
}

impl Default for AggregatorConfig {
    fn default() -> Self {
        Self {
            flush_interval: FLUSH_INTERVAL,
            high_coalesce_window: HIGH_COALESCE_WINDOW,
        }
    }
}

/// One observation, as the response path hands it over.
///
/// The timestamp is wall-clock because it is reported to a human and
/// correlated with other logs. Windowing uses the aggregator's own
/// monotonic clock rather than a stamp carried per observation, so
/// there is no second clock here to disagree with it.
#[derive(Debug)]
pub struct Observation {
    pub wall: time::OffsetDateTime,
    pub source: Option<IpAddr>,
    pub method: String,
    pub path: String,
    pub user_agent: Option<String>,
    pub host: Option<String>,
}

/// The front's handle to the trap.
#[derive(Debug)]
pub struct Canary {
    tx: mpsc::Sender<Observation>,
    dropped: AtomicU64,
    /// Sources that passed the knock recently. Bounded; see
    /// [`AUTHORISED_GRACE`].
    authorised: Mutex<HashMap<IpAddr, Instant>>,
}

impl Canary {
    /// Build the trap and its aggregator task's receiver.
    #[must_use]
    pub fn new() -> (Arc<Self>, mpsc::Receiver<Observation>) {
        let (tx, rx) = mpsc::channel(QUEUE_DEPTH);
        (
            Arc::new(Self {
                tx,
                dropped: AtomicU64::new(0),
                authorised: Mutex::new(HashMap::new()),
            }),
            rx,
        )
    }

    /// Record that this source presented a valid knock.
    ///
    /// Called on the *authorised* path, so it is the one place the two
    /// paths differ — and it differs in the direction that is safe: a
    /// scanner never reaches it.
    pub fn note_authorised(&self, source: Option<IpAddr>) {
        let Some(ip) = source else { return };
        let now = Instant::now();
        let mut g = self.lock();
        g.retain(|_, seen| now.duration_since(*seen) < AUTHORISED_GRACE);
        if g.len() >= MAX_AUTHORISED && !g.contains_key(&ip) {
            return;
        }
        g.insert(ip, now);
    }

    /// `true` iff this source passed the knock inside the grace window.
    #[must_use]
    pub fn recently_authorised(&self, source: Option<IpAddr>) -> bool {
        let Some(ip) = source else { return false };
        let g = self.lock();
        g.get(&ip)
            .is_some_and(|seen| Instant::now().duration_since(*seen) < AUTHORISED_GRACE)
    }

    /// Hand one probe to the aggregator. Never blocks, never fails
    /// visibly, never touches the response.
    pub fn observe(&self, observation: Observation) {
        if self.tx.try_send(observation).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Observations dropped because the queue was full, and reset.
    fn take_dropped(&self) -> u64 {
        self.dropped.swap(0, Ordering::Relaxed)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<IpAddr, Instant>> {
        self.authorised
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Accumulates observations between flushes.
#[derive(Debug, Default)]
struct Window {
    total: u64,
    high: u64,
    low: u64,
    suppressed: u64,
    sources: HashSet<String>,
    samples: Vec<ProbeSample>,
    first: Option<time::OffsetDateTime>,
    last: Option<time::OffsetDateTime>,
    /// Set when a HIGH arrives, so the aggregator knows to flush early.
    saw_high: bool,
}

impl Window {
    fn is_empty(&self) -> bool {
        self.total == 0
    }

    fn push(&mut self, sample: ProbeSample) {
        self.total += 1;
        match sample.severity {
            Severity::High => self.high += 1,
            Severity::Low => self.low += 1,
            Severity::Suppressed => self.suppressed += 1,
        }
        self.sources.insert(sample.source_ip.clone());
        if self.first.is_none() {
            self.first = parse_stamp(&sample.at);
        }
        self.last = parse_stamp(&sample.at);
        if sample.severity == Severity::High {
            self.saw_high = true;
        }
        // Keep the worst evidence, not merely the first: a burst of
        // background noise must not crowd out the one tripwire hit.
        if self.samples.len() < MAX_SAMPLES {
            self.samples.push(sample);
        } else if let Some(pos) = self
            .samples
            .iter()
            .position(|existing| existing.severity < sample.severity)
        {
            self.samples[pos] = sample;
        }
    }

    fn drain(&mut self, dropped: u64) -> CanaryBatch {
        let mut samples = std::mem::take(&mut self.samples);
        // Worst first, so a truncated alert still leads with the
        // tripwire hit rather than the background noise around it.
        samples.sort_by_key(|s| std::cmp::Reverse(s.severity));
        let batch = CanaryBatch {
            window_start: self.first.map(stamp).unwrap_or_default(),
            window_end: self.last.map(stamp).unwrap_or_default(),
            total: self.total,
            high: self.high,
            low: self.low,
            suppressed: self.suppressed,
            distinct_sources: self.sources.len() as u64,
            dropped,
            samples,
        };
        *self = Self::default();
        batch
    }
}

/// Turn one observation into a classified sample.
///
/// Public so the classification the front actually performs is
/// testable without standing up a tunnel.
#[must_use]
pub fn to_sample(
    observation: &Observation,
    expected_host: Option<&str>,
    tripwire_path: &str,
    recently_authorised: bool,
) -> ProbeSample {
    let named_the_host = match (expected_host, observation.host.as_deref()) {
        (Some(expected), Some(got)) => {
            // Compare without the port: browsers send `host:443` in
            // some configurations and `host` in others.
            let got = got.split(':').next().unwrap_or(got);
            got.eq_ignore_ascii_case(expected)
        }
        _ => false,
    };
    let reason = classify(&ProbeInput {
        path: &observation.path,
        matched_expected_host: named_the_host,
        tripwire: observation.path == tripwire_path,
        recently_authorised,
    });
    ProbeSample {
        at: stamp(observation.wall),
        severity: reason.severity(),
        reason,
        source_ip: observation
            .source
            .map_or_else(|| "unknown".to_string(), |ip| ip.to_string()),
        method: aberp_portal_core::canary::sanitise(&observation.method, 16),
        path: aberp_portal_core::canary::sanitise(&observation.path, 120),
        user_agent: observation
            .user_agent
            .as_deref()
            .map(|ua| aberp_portal_core::canary::sanitise(ua, 160)),
        named_the_host,
        // Phase-2 — see the module docs.
        sni: None,
    }
}

/// The aggregator task at the deployed cadence.
pub async fn run_aggregator(
    canary: Arc<Canary>,
    broker: Arc<Broker>,
    rx: mpsc::Receiver<Observation>,
) {
    run_aggregator_with(canary, broker, rx, AggregatorConfig::default()).await;
}

/// The aggregator task: classify, coalesce, forward.
pub async fn run_aggregator_with(
    canary: Arc<Canary>,
    broker: Arc<Broker>,
    mut rx: mpsc::Receiver<Observation>,
    cfg: AggregatorConfig,
) {
    let mut window = Window::default();
    let mut pending: Vec<CanaryBatch> = Vec::new();
    let mut last_high_flush = Instant::now()
        .checked_sub(cfg.high_coalesce_window)
        .unwrap_or_else(Instant::now);
    let mut ticker = tokio::time::interval(cfg.flush_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        let flush_now = tokio::select! {
            maybe = rx.recv() => {
                let Some(observation) = maybe else { return };
                let recently_authorised = canary.recently_authorised(observation.source);
                let sample = to_sample(
                    &observation,
                    broker.expected_host().as_deref(),
                    &broker.tripwire_path(),
                    recently_authorised,
                );
                window.push(sample);
                // A HIGH is worth telling the Mac about promptly — but
                // no more often than once per coalesce window, so a
                // sweep is one batch rather than one per packet.
                window.saw_high
                    && Instant::now().duration_since(last_high_flush) >= cfg.high_coalesce_window
            }
            _ = ticker.tick() => true,
        };

        if !flush_now || window.is_empty() {
            continue;
        }
        if window.saw_high {
            last_high_flush = Instant::now();
        }
        let batch = window.drain(canary.take_dropped());

        // Metadata-only, per BATCH not per probe: a per-probe log line
        // would let a scan flood the VPS's journal, which is a disk
        // exhaustion primitive on a box that is supposed to hold
        // nothing.
        tracing::info!(
            total = batch.total,
            high = batch.high,
            low = batch.low,
            suppressed = batch.suppressed,
            sources = batch.distinct_sources,
            dropped = batch.dropped,
            "canary window"
        );

        // Forwarded whether or not it is *reportable*: the Mac owns the
        // durable probe log, and a record of what the operator's own
        // browser did is worth having when reading that log later. The
        // decision not to *alert* on a suppressed-only window is the
        // agent's, and it is made there — one place decides, and it is
        // the trusted one.
        pending.push(batch);
        // Oldest-first drop: the newest evidence is the most useful,
        // and the count is preserved in the surviving batches' own
        // totals only for their own windows — so log the loss.
        while pending.len() > MAX_PENDING_BATCHES {
            pending.remove(0);
            tracing::warn!("canary batch dropped — the tunnel has been down too long");
        }
        pending.retain(|batch| !forward(&broker, batch));
    }
}

/// Try to push one batch down the tunnel. `true` iff it went.
fn forward(broker: &Arc<Broker>, batch: &CanaryBatch) -> bool {
    broker.try_send_now(Frame::Canary {
        batch: batch.clone(),
    })
}

fn stamp(t: time::OffsetDateTime) -> String {
    t.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

fn parse_stamp(s: &str) -> Option<time::OffsetDateTime> {
    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aberp_portal_core::canary::Reason;

    fn observation(path: &str, host: Option<&str>) -> Observation {
        Observation {
            wall: time::OffsetDateTime::now_utc(),
            source: Some("203.0.113.7".parse().expect("ip")),
            method: "GET".into(),
            path: path.into(),
            user_agent: Some("curl/8.0".into()),
            host: host.map(str::to_string),
        }
    }

    #[test]
    fn a_full_queue_drops_and_counts_rather_than_blocking() {
        // The response path must never wait on the aggregator.
        let (canary, _rx) = Canary::new();
        for _ in 0..(QUEUE_DEPTH + 50) {
            canary.observe(observation("/", None));
        }
        assert_eq!(canary.take_dropped(), 50);
        // …and the counter resets, so the next batch reports only its
        // own losses.
        assert_eq!(canary.take_dropped(), 0);
    }

    #[test]
    fn the_grace_window_tracks_only_authorised_sources() {
        let (canary, _rx) = Canary::new();
        let ip: IpAddr = "198.51.100.4".parse().expect("ip");
        assert!(!canary.recently_authorised(Some(ip)));
        canary.note_authorised(Some(ip));
        assert!(canary.recently_authorised(Some(ip)));
        assert!(!canary.recently_authorised(Some("198.51.100.5".parse().expect("ip"))));
        // An unknown source is never graced.
        assert!(!canary.recently_authorised(None));
    }

    #[test]
    fn the_authorised_map_is_bounded() {
        let (canary, _rx) = Canary::new();
        for i in 0..(MAX_AUTHORISED + 100) {
            let ip: IpAddr = format!("10.0.{}.{}", i / 256, i % 256).parse().expect("ip");
            canary.note_authorised(Some(ip));
        }
        assert!(canary.lock().len() <= MAX_AUTHORISED);
    }

    #[test]
    fn host_match_ignores_the_port_and_case() {
        let s = to_sample(
            &observation("/", Some("Portal.Example:443")),
            Some("portal.example"),
            "/decoy",
            false,
        );
        assert!(s.named_the_host);
        assert_eq!(s.reason, Reason::NamedTheHost);
        assert_eq!(s.severity, Severity::High);
    }

    #[test]
    fn a_sample_never_carries_the_hostname_itself() {
        // The alert this feeds travels by email. `named_the_host` is a
        // boolean precisely so the label Ervin kept out of CT logs does
        // not end up in a mailbox.
        let s = to_sample(
            &observation("/", Some("portal.example")),
            Some("portal.example"),
            "/decoy",
            false,
        );
        let json = serde_json::to_string(&s).expect("serialise");
        assert!(
            !json.contains("portal.example"),
            "the sample leaked the label: {json}"
        );
    }

    #[test]
    fn the_tripwire_is_recognised_by_exact_path() {
        let s = to_sample(&observation("/decoy", None), None, "/decoy", false);
        assert_eq!(s.reason, Reason::Tripwire);
        // A near miss is not the tripwire.
        let s = to_sample(&observation("/decoy/x", None), None, "/decoy", false);
        assert_ne!(s.reason, Reason::Tripwire);
    }

    #[test]
    fn attacker_controlled_strings_are_sanitised_into_the_sample() {
        let mut o = observation("/a\r\nInjected: yes", None);
        o.user_agent = Some("x\ny".repeat(200));
        let s = to_sample(&o, None, "/decoy", false);
        assert!(!s.path.contains('\r') && !s.path.contains('\n'));
        let ua = s.user_agent.expect("ua");
        assert!(!ua.contains('\n'));
        assert!(ua.chars().count() <= 161);
    }

    #[test]
    fn the_window_keeps_the_worst_samples_not_the_first() {
        let mut w = Window::default();
        for _ in 0..MAX_SAMPLES {
            w.push(to_sample(
                &observation("/noise", None),
                None,
                "/decoy",
                false,
            ));
        }
        w.push(to_sample(
            &observation("/decoy", None),
            None,
            "/decoy",
            false,
        ));
        let batch = w.drain(0);
        assert_eq!(batch.total, MAX_SAMPLES as u64 + 1);
        assert_eq!(batch.high, 1);
        assert_eq!(batch.samples.len(), MAX_SAMPLES);
        assert_eq!(
            batch.samples[0].reason,
            Reason::Tripwire,
            "the tripwire hit was crowded out by background noise"
        );
    }

    #[test]
    fn a_window_of_only_suppressed_probes_is_not_reportable() {
        let mut w = Window::default();
        w.push(to_sample(
            &observation("/favicon.ico", None),
            None,
            "/decoy",
            true,
        ));
        let batch = w.drain(0);
        assert_eq!(batch.suppressed, 1);
        assert!(
            !batch.is_reportable(),
            "the operator's own browser must not page anyone"
        );
    }
}
