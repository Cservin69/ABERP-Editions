//! The parking lot (ADR-0115 §2.4).
//!
//! > The relay is a dumb authenticated pipe: no business data at rest,
//! > no WebAuthn credential store, no session issuance.
//!
//! # The transport, and why it changed
//!
//! Phase 0 originally held a framed tunnel open from the Mac to the
//! VPS. Ervin's transport decision replaced it: *"no existing tunnels,
//! just a Mac querying."* So the relay no longer holds a socket it can
//! push down. It **parks** an authenticated front request in a bounded
//! in-memory queue, and the Mac's long-poll comes and takes it. Every
//! leg is Mac-initiated; the relay initiates nothing, ever.
//!
//! Concretely, one browser request is:
//!
//! 1. the front calls [`Broker::park`], which enqueues the request and
//!    blocks that one front task on a `oneshot`;
//! 2. the agent's `POST /agent/v3/poll` arrives, [`Broker::poll`] drains
//!    the queue into the response, and the agent runs the query on the
//!    Mac;
//! 3. the agent's `POST /agent/v3/deliver` arrives, [`Broker::deliver`]
//!    matches it to the parked `oneshot`, and the front task wakes with
//!    an answer.
//!
//! # "Mac down -> the host is not there", without a socket to close
//!
//! The tunnel model got §5.3 for free: the socket closed, the knock
//! token left with it, and the host collapsed to the parked nginx. A
//! poll model has no socket to close, so the same property comes from
//! [`Presence`] and its TTL — the relay's knowledge of the knock token
//! is a *lease* the Mac renews by polling, and a Mac that stops polling
//! stops renewing it.
//!
//! That is strictly better than the socket close, because it also
//! covers the case a close does not: a Mac that is **wedged rather than
//! gone** holds a TCP connection open indefinitely while answering
//! nothing. Under the tunnel model that Mac kept the portal advertised
//! and every request timing out; under the lease it simply lapses.
//!
//! # What this module still cannot do
//!
//! - it cannot verify a WebAuthn assertion — there is no crypto here
//!   beyond TLS, and `aberp-portal-agent` is not a dependency;
//! - it cannot mint a session — it only copies a `Set-Cookie` the agent
//!   produced;
//! - it cannot decide what is readable — it parks the method verbatim
//!   and lets the agent refuse (§6.3);
//! - it cannot answer at all without a live lease (§5.3).
//!
//! # What it *can* see — the named residual
//!
//! Until hardening H1 (browser<->agent HPKE, Ervin's §9.4 decision:
//! Phase 2), Leg A's TLS terminates in front of this code, so
//! everything passing through — ceremony messages, session cookies, and
//! from Phase 1 the invoice payloads themselves — is in this process's
//! memory in plaintext. A root-level compromise of the VPS can read a
//! session while it is happening. It cannot mint one, cannot reach the
//! Mac for anything outside the allowlist, and cannot recover anything
//! after the fact. That is the §2.4 residual, stated where the code
//! that carries it lives.
//!
//! Enrolment deserves its own sentence, because the original wording
//! here — "cannot enrol" — was overstated. A compromised relay CAN see
//! a live, console-minted enrolment token in this process's memory. It
//! is stopped from turning that into a credential by two controls on
//! the Mac (§4.3a Apple attestation and §4.3b console confirmation),
//! not by the token being hidden from it. See
//! `aberp_portal_agent::webauthn` for why that distinction matters.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};

use aberp_portal_core::canary::CanaryBatch;
use aberp_portal_core::proto::{
    AgentIdentity, Delivery, DeliveryAck, Heartbeat, PollRequest, PollResponse, PortalRequest,
    PortalResponse, Work, MAX_POLL_WAIT, PRESENCE_TTL, PROTOCOL_VERSION,
};
use tokio::sync::{oneshot, Notify};

/// How long the front waits for the Mac to answer before giving up.
///
/// Must exceed one full poll cycle plus the agent's own upstream read
/// timeout, or a request parked one millisecond after a poll returned
/// empty would expire before the next poll could even collect it.
pub const DISPATCH_TIMEOUT: Duration = Duration::from_secs(60);

/// Most requests that may sit parked at once.
///
/// A hard bound, not a tuning knob: the queue is the one place a relay
/// that "holds nothing" does hold something, and an unbounded one is a
/// remote OOM for anybody who can pass the knock. Past it, the front
/// answers its ordinary parked 404 — the same answer overload produces
/// as everything else, so load is not an oracle either.
pub const MAX_PARKED: usize = 64;

/// Most canary batches held for redelivery.
///
/// Bounded for the same reason, and dropped **oldest-first** so a flood
/// of fresh probes cannot push the newest — most interesting —
/// observations out of the queue.
pub const MAX_CANARY_PENDING: usize = 256;

/// What the relay knows about the Mac, and for exactly how long.
///
/// Every field arrives with a poll and lapses with the lease. Nothing
/// here is ever written to disk (§2.4).
#[derive(Debug, Clone)]
struct Presence {
    knock_token: String,
    expected_host: Option<String>,
    tripwire_path: String,
    epoch: String,
    /// When the most recent poll *started*. The lease is measured from
    /// here, not from when a poll returns, so a long-poll parked for
    /// its full 25 seconds does not look like 25 seconds of silence.
    last_seen: Instant,
}

impl Presence {
    fn live(&self) -> bool {
        self.last_seen.elapsed() < PRESENCE_TTL
    }
}

/// One request waiting for the Mac to come and take it.
#[derive(Debug)]
struct Parked {
    id: u64,
    req: PortalRequest,
}

/// The relay's only state. All of it in memory, all of it perishable.
#[derive(Debug)]
pub struct Broker {
    presence: RwLock<Option<Presence>>,
    parked: Mutex<VecDeque<Parked>>,
    /// Front tasks waiting for an answer, by request id.
    pending: Mutex<HashMap<u64, oneshot::Sender<PortalResponse>>>,
    /// Canary batches awaiting acknowledgement. Held, not dropped, so a
    /// poll response lost to a broken connection does not take the
    /// probes with it (§3.4's at-least-once rule).
    canary: Mutex<VecDeque<(u64, CanaryBatch)>>,
    next_id: AtomicU64,
    next_canary_seq: AtomicU64,
    heartbeat_seq: AtomicU64,
    observed_total: AtomicU64,
    started: Instant,
    /// Woken whenever work is enqueued, so a parked long-poll returns
    /// immediately rather than at the end of its wait.
    work_ready: Notify,
}

impl Default for Broker {
    fn default() -> Self {
        Self::new()
    }
}

/// Why a parked request did not produce an answer. All of them collapse
/// to the parked nginx 404 at the front — the browser must not learn
/// which.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchError {
    /// No live lease: the Mac is down, wedged, or has never polled.
    NoAgent,
    /// The parking queue is full.
    Overloaded,
    /// The Mac took the request and never delivered an answer.
    Timeout,
    /// The relay dropped the waiter — a restart, or an epoch change.
    Lost,
}

/// Why a poll was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PollError {
    #[error("agent speaks protocol {got}, relay speaks {PROTOCOL_VERSION}")]
    ProtocolMismatch { got: u32 },
}

impl Broker {
    #[must_use]
    pub fn new() -> Self {
        Self {
            presence: RwLock::new(None),
            parked: Mutex::new(VecDeque::new()),
            pending: Mutex::new(HashMap::new()),
            canary: Mutex::new(VecDeque::new()),
            next_id: AtomicU64::new(1),
            next_canary_seq: AtomicU64::new(1),
            heartbeat_seq: AtomicU64::new(0),
            observed_total: AtomicU64::new(0),
            started: Instant::now(),
            work_ready: Notify::new(),
        }
    }

    /// `true` iff the lease is live and `candidate` is the Mac's
    /// current knock token.
    ///
    /// Constant-time (`aberp_portal_core::ct`) because §3.2 forbids a
    /// timing cliff here: a bytewise early return would turn the parked
    /// 404 into an oracle for guessing the token prefix.
    ///
    /// The liveness check comes first and is not constant-time, which
    /// is correct — whether the Mac is up is not a secret, and §5.3
    /// makes it observable on purpose.
    #[must_use]
    pub fn knock_matches(&self, candidate: &str) -> bool {
        match self.live_presence() {
            Some(p) => aberp_portal_core::ct::eq(p.knock_token.as_bytes(), candidate.as_bytes()),
            None => false,
        }
    }

    /// `true` iff the Mac's lease is live.
    #[must_use]
    pub fn agent_present(&self) -> bool {
        self.live_presence().is_some()
    }

    /// The current epoch, for metadata logging only.
    #[must_use]
    pub fn epoch(&self) -> Option<String> {
        self.live_presence().map(|p| p.epoch)
    }

    /// The hostname the Mac published, if any.
    #[must_use]
    pub fn expected_host(&self) -> Option<String> {
        self.live_presence().and_then(|p| p.expected_host)
    }

    /// The decoy path the Mac published.
    ///
    /// Falls back to the compiled-in default while no lease is live, so
    /// the trap still recognises its own decoy during an outage — which
    /// is exactly when a scan is most interesting.
    #[must_use]
    pub fn tripwire_path(&self) -> String {
        self.live_presence().map_or_else(
            || aberp_portal_core::canary::DEFAULT_TRIPWIRE_PATH.to_string(),
            |p| p.tripwire_path,
        )
    }

    /// Park one request and wait for the Mac to answer it.
    pub async fn park(&self, req: PortalRequest) -> Result<PortalResponse, DispatchError> {
        if !self.agent_present() {
            return Err(DispatchError::NoAgent);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();

        // The waiter is registered BEFORE the request becomes visible
        // to a poll, and the order is load-bearing.
        //
        // Registering it after left a window: a poll running between
        // the `push_back` and the `insert` takes the request, the Mac
        // answers it, and `deliver` looks the id up in `pending` — which
        // does not contain it yet. The delivery is dropped as
        // unclaimed, the front task then waits out the full
        // `DISPATCH_TIMEOUT` for an answer that already came and went,
        // and the browser gets a 60-second pause and then the parked
        // 404. Rare, silent, and indistinguishable from "the Mac is
        // down" — which is exactly the kind of thing that gets
        // diagnosed as flakiness for a year.
        //
        // Inserting first cannot have the mirror-image bug: an id in
        // `pending` that never reaches `parked` is simply never
        // delivered to, and the `Overloaded` path below removes it.
        lock(&self.pending).insert(id, tx);
        {
            let mut q = lock(&self.parked);
            if q.len() >= MAX_PARKED {
                drop(q);
                lock(&self.pending).remove(&id);
                return Err(DispatchError::Overloaded);
            }
            q.push_back(Parked { id, req });
        }
        self.work_ready.notify_waiters();

        match tokio::time::timeout(DISPATCH_TIMEOUT, rx).await {
            Ok(Ok(res)) => Ok(res),
            Ok(Err(_)) => Err(DispatchError::Lost),
            Err(_) => {
                // Drop both the waiter and the request if it is still
                // queued: the browser has gone, and handing a dead
                // request to the Mac wastes a round trip.
                lock(&self.pending).remove(&id);
                lock(&self.parked).retain(|p| p.id != id);
                Err(DispatchError::Timeout)
            }
        }
    }

    /// Serve one long-poll from the Mac.
    ///
    /// Renews the lease, acknowledges canary batches, and returns
    /// whatever work is waiting — blocking up to the requested wait if
    /// there is none.
    pub async fn poll(&self, req: &PollRequest) -> Result<PollResponse, PollError> {
        if req.agent.protocol_version != PROTOCOL_VERSION {
            return Err(PollError::ProtocolMismatch {
                got: req.agent.protocol_version,
            });
        }

        // Computed BEFORE the lease is renewed, because that is the
        // question the agent is actually asking: "did you still know me
        // when this poll arrived?" Renewing first would make the answer
        // trivially `true` every time and the epoch guard useless.
        let known_epoch = self
            .live_presence()
            .is_some_and(|p| p.epoch == req.agent.epoch);

        self.renew(&req.agent);
        self.ack_canary(req.ack_canary_seq);

        // The agent asks; it does not dictate. An agent requesting an
        // hour would otherwise pin a relay task for an hour.
        let wait = Duration::from_millis(u64::from(req.wait_ms)).min(MAX_POLL_WAIT);

        let mut work = self.take_work();
        if work.is_empty() && !wait.is_zero() {
            // `notified()` is created before the second drain so a
            // request parked in the gap cannot be missed.
            let notified = self.work_ready.notified();
            tokio::pin!(notified);
            let _ = tokio::time::timeout(wait, &mut notified).await;
            work = self.take_work();
        }

        Ok(PollResponse {
            work,
            heartbeat: self.heartbeat(),
            known_epoch,
        })
    }

    /// Accept one answer from the Mac.
    pub fn deliver(&self, delivery: Delivery) -> DeliveryAck {
        // A delivery stamped with a stale epoch belongs to a generation
        // whose sessions are already revoked; dropping it is what stops
        // an answer computed under an old epoch reaching a browser
        // whose cookie was minted under a newer one.
        let epoch_ok = self
            .live_presence()
            .is_some_and(|p| p.epoch == delivery.epoch);
        if !epoch_ok {
            return DeliveryAck { accepted: false };
        }
        let accepted = match lock(&self.pending).remove(&delivery.id) {
            // `send` fails when the front task already timed out and
            // dropped its receiver — the browser gave up. Not an error
            // the agent can act on, but worth distinguishing.
            Some(waiter) => waiter.send(delivery.res).is_ok(),
            None => false,
        };
        DeliveryAck { accepted }
    }

    /// Queue a canary batch for the next poll.
    ///
    /// Never blocks: it runs on the aggregator task that the response
    /// path feeds, and an aggregator stalled here would eventually
    /// stall the observations behind it.
    ///
    /// Returns `false` iff the queue was full and the oldest batch had
    /// to be dropped, which the caller logs — silently losing probes is
    /// the one failure this trap must not have.
    pub fn queue_canary(&self, batch: CanaryBatch) -> bool {
        self.observed_total
            .fetch_add(batch.samples.len() as u64, Ordering::Relaxed);
        let seq = self.next_canary_seq.fetch_add(1, Ordering::Relaxed);
        let mut q = lock(&self.canary);
        let dropped = if q.len() >= MAX_CANARY_PENDING {
            q.pop_front();
            true
        } else {
            false
        };
        q.push_back((seq, batch));
        drop(q);
        self.work_ready.notify_waiters();
        !dropped
    }

    /// Drop every canary batch at or below `seq`.
    fn ack_canary(&self, seq: u64) {
        if seq == 0 {
            return;
        }
        lock(&self.canary).retain(|(s, _)| *s > seq);
    }

    /// Everything waiting: parked requests, then unacknowledged canary
    /// batches.
    ///
    /// Requests are **removed** (one Mac, one taker); canary batches are
    /// **copied** and stay queued until acknowledged. That asymmetry is
    /// the at-least-once guarantee: a poll response lost in flight
    /// costs one redelivery, not a lost probe.
    fn take_work(&self) -> Vec<Work> {
        let mut work: Vec<Work> = Vec::new();
        {
            let mut q = lock(&self.parked);
            while let Some(p) = q.pop_front() {
                work.push(Work::Request {
                    id: p.id,
                    req: p.req,
                });
            }
        }
        for (seq, batch) in lock(&self.canary).iter() {
            work.push(Work::Canary {
                seq: *seq,
                batch: batch.clone(),
            });
        }
        work
    }

    /// Stamp a heartbeat. Counters only, never contents.
    fn heartbeat(&self) -> Heartbeat {
        Heartbeat {
            seq: self.heartbeat_seq.fetch_add(1, Ordering::Relaxed) + 1,
            emitted_at: time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default(),
            relay_uptime_s: self.started.elapsed().as_secs(),
            observed_total: self.observed_total.load(Ordering::Relaxed),
            parked: lock(&self.parked).len() as u32,
            canary_pending: lock(&self.canary).len() as u32,
        }
    }

    /// Renew the lease from a poll's identity.
    ///
    /// An epoch change fails every in-flight request rather than
    /// leaving front tasks hanging until their dispatch timeout: those
    /// requests belong to a generation the agent has just abandoned.
    fn renew(&self, id: &AgentIdentity) {
        let rotated = {
            let mut g = write(&self.presence);
            let rotated = g.as_ref().is_some_and(|p| p.epoch != id.epoch);
            *g = Some(Presence {
                knock_token: id.knock_token.clone(),
                expected_host: id.expected_host.clone(),
                tripwire_path: id.tripwire_path.clone(),
                epoch: id.epoch.clone(),
                last_seen: Instant::now(),
            });
            rotated
        };
        if rotated {
            lock(&self.pending).clear();
            lock(&self.parked).clear();
        }
    }

    /// The lease, if it has not lapsed.
    fn live_presence(&self) -> Option<Presence> {
        read(&self.presence).as_ref().filter(|p| p.live()).cloned()
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn read<T>(l: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn write<T>(l: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(knock: &str, epoch: &str) -> AgentIdentity {
        AgentIdentity {
            protocol_version: PROTOCOL_VERSION,
            knock_token: knock.into(),
            expected_host: Some("portal.test".into()),
            tripwire_path: "/decoy".into(),
            epoch: epoch.into(),
        }
    }

    fn poll_req(knock: &str, epoch: &str, wait_ms: u32, ack: u64) -> PollRequest {
        PollRequest {
            agent: identity(knock, epoch),
            wait_ms,
            ack_canary_seq: ack,
        }
    }

    fn request(path: &str) -> PortalRequest {
        PortalRequest {
            method: "GET".into(),
            path: path.into(),
            query: None,
            cookie: None,
            body_b64: None,
            peer: None,
        }
    }

    #[tokio::test]
    async fn with_no_lease_nothing_knocks_and_nothing_parks() {
        // §5.3's top-right cell: Mac down -> the host is simply not there.
        let b = Broker::new();
        assert!(!b.agent_present());
        assert!(!b.knock_matches("anything"));
        assert!(!b.knock_matches(""));
        assert_eq!(
            b.park(request("/api/status")).await,
            Err(DispatchError::NoAgent)
        );
    }

    #[tokio::test]
    async fn a_poll_publishes_the_knock_token_and_the_labels() {
        let b = Broker::new();
        b.poll(&poll_req("the-token", "e1", 0, 0))
            .await
            .expect("poll");
        assert!(b.agent_present());
        assert!(b.knock_matches("the-token"));
        assert!(!b.knock_matches("the-toke"));
        assert!(!b.knock_matches("the-tokenX"));
        assert_eq!(b.expected_host().as_deref(), Some("portal.test"));
        assert_eq!(b.tripwire_path(), "/decoy");
        assert_eq!(b.epoch().as_deref(), Some("e1"));
    }

    #[tokio::test]
    async fn with_no_lease_the_tripwire_falls_back_to_the_compiled_default() {
        let b = Broker::new();
        assert_eq!(
            b.tripwire_path(),
            aberp_portal_core::canary::DEFAULT_TRIPWIRE_PATH
        );
        assert!(b.expected_host().is_none());
    }

    #[tokio::test]
    async fn a_version_skew_is_refused_rather_than_negotiated() {
        let b = Broker::new();
        let mut p = poll_req("t", "e1", 0, 0);
        p.agent.protocol_version = PROTOCOL_VERSION + 1;
        assert_eq!(
            b.poll(&p).await,
            Err(PollError::ProtocolMismatch {
                got: PROTOCOL_VERSION + 1
            })
        );
        assert!(
            !b.agent_present(),
            "a refused poll must not renew the lease"
        );
    }

    #[tokio::test]
    async fn a_request_is_parked_pulled_verbatim_and_answered() {
        // Including a mutating verb: the relay MUST park it so the
        // agent is the one that refuses (§6.3).
        let b = std::sync::Arc::new(Broker::new());
        b.poll(&poll_req("t", "e1", 0, 0)).await.expect("lease");

        let bb = std::sync::Arc::clone(&b);
        let front = tokio::spawn(async move {
            let mut req = request("/api/invoices");
            req.method = "POST".into();
            bb.park(req).await
        });

        // The Mac's long-poll collects it.
        let res = b.poll(&poll_req("t", "e1", 5_000, 0)).await.expect("poll");
        let Some(Work::Request { id, req }) = res.work.first().cloned() else {
            panic!("expected a parked request, got {:?}", res.work);
        };
        assert_eq!(req.method, "POST", "the verb must reach the Mac unfiltered");
        assert_eq!(req.path, "/api/invoices");

        let ack = b.deliver(Delivery {
            epoch: "e1".into(),
            id,
            res: PortalResponse::json(200, r#"{"ok":true}"#),
        });
        assert!(ack.accepted);

        let got = front.await.expect("join").expect("answered");
        assert_eq!(got.body().expect("body"), br#"{"ok":true}"#);
    }

    #[tokio::test]
    async fn a_long_poll_returns_the_moment_a_request_is_parked() {
        // If it waited out its full window instead, every page load
        // would cost up to MAX_POLL_WAIT.
        let b = std::sync::Arc::new(Broker::new());
        b.poll(&poll_req("t", "e1", 0, 0)).await.expect("lease");

        let bb = std::sync::Arc::clone(&b);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            let _ = bb.park(request("/api/status")).await;
        });

        let started = Instant::now();
        let res = b.poll(&poll_req("t", "e1", 10_000, 0)).await.expect("poll");
        assert_eq!(res.work.len(), 1);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the poll waited out its window instead of being woken"
        );
    }

    #[tokio::test]
    async fn an_idle_poll_returns_empty_but_still_carries_a_heartbeat() {
        // The heartbeat is the silence detector; an idle poll is
        // exactly when it matters.
        let b = Broker::new();
        let res = b.poll(&poll_req("t", "e1", 1, 0)).await.expect("poll");
        assert!(res.work.is_empty());
        assert_eq!(res.heartbeat.seq, 1);
        let res = b.poll(&poll_req("t", "e1", 1, 0)).await.expect("poll");
        assert_eq!(res.heartbeat.seq, 2, "the sequence must be monotonic");
    }

    #[tokio::test]
    async fn the_first_poll_of_an_epoch_reports_that_the_relay_did_not_know_it() {
        // This is what makes the agent rotate: a relay that restarted,
        // or a Mac that was away long enough to lapse.
        let b = Broker::new();
        let res = b.poll(&poll_req("t", "e1", 0, 0)).await.expect("poll");
        assert!(!res.known_epoch, "a cold relay knows nobody");
        let res = b.poll(&poll_req("t", "e1", 0, 0)).await.expect("poll");
        assert!(res.known_epoch, "the lease is live and the epoch matches");
        let res = b.poll(&poll_req("t", "e2", 0, 0)).await.expect("poll");
        assert!(
            !res.known_epoch,
            "a new epoch was not known before this poll"
        );
    }

    #[tokio::test]
    async fn a_canary_batch_rides_every_poll_until_it_is_acknowledged() {
        // At-least-once. A poll response lost to a dropped connection
        // must not take the probes with it.
        let b = Broker::new();
        b.poll(&poll_req("t", "e1", 0, 0)).await.expect("lease");
        assert!(b.queue_canary(CanaryBatch::default()));

        let first = b.poll(&poll_req("t", "e1", 0, 0)).await.expect("poll");
        let Some(Work::Canary { seq, .. }) = first.work.first() else {
            panic!("expected a canary batch, got {:?}", first.work);
        };
        let seq = *seq;

        // Pretend that response never arrived: poll again without
        // acknowledging.
        let again = b.poll(&poll_req("t", "e1", 0, 0)).await.expect("poll");
        assert!(
            matches!(again.work.first(), Some(Work::Canary { seq: s, .. }) if *s == seq),
            "an unacknowledged batch must be redelivered"
        );

        // Now acknowledge it.
        let after = b.poll(&poll_req("t", "e1", 0, seq)).await.expect("poll");
        assert!(
            after.work.is_empty(),
            "an acknowledged batch must be dropped"
        );
    }

    #[tokio::test]
    async fn the_canary_queue_is_bounded_and_drops_the_oldest_first() {
        // Newest probes are the interesting ones; a flood must not be
        // able to push them out.
        let b = Broker::new();
        for _ in 0..MAX_CANARY_PENDING {
            assert!(b.queue_canary(CanaryBatch::default()));
        }
        assert!(
            !b.queue_canary(CanaryBatch::default()),
            "overflow must be reported, not silent"
        );
        let res = b.poll(&poll_req("t", "e1", 0, 0)).await.expect("poll");
        assert_eq!(res.work.len(), MAX_CANARY_PENDING);
        let seqs: Vec<u64> = res
            .work
            .iter()
            .filter_map(|w| match w {
                Work::Canary { seq, .. } => Some(*seq),
                Work::Request { .. } => None,
            })
            .collect();
        assert_eq!(seqs.first(), Some(&2), "the oldest was the one dropped");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_delivery_racing_the_park_is_not_dropped() {
        // The window the ordering fix closes: a poll that collects the
        // request between it being queued and its waiter being
        // registered. Reproduced deterministically by driving the poll
        // and the delivery from the moment the request appears in the
        // queue — with the old order, `deliver` finds no waiter, the
        // ack is `accepted: false`, and `park` sits out its full
        // DISPATCH_TIMEOUT.
        // Needs real threads: with the queue write and the waiter
        // registration adjacent and no `.await` between them, a
        // single-threaded runtime can never interleave, and a test on
        // one would pass against the bug. So the collector runs on its
        // own worker, spinning exactly as a poll arriving at the wrong
        // microsecond does, and the whole thing is repeated enough
        // times to hit a window measured in nanoseconds.
        const ROUNDS: usize = 60;
        /// Parked concurrently per round, so the collector thread has
        /// many chances per round to land inside the window rather than
        /// one.
        const CONCURRENT: usize = 48;

        let b = std::sync::Arc::new(Broker::new());
        b.poll(&poll_req("t", "e1", 0, 0)).await.expect("lease");

        let taker = std::sync::Arc::clone(&b);
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopper = std::sync::Arc::clone(&stop);
        let collector = std::thread::spawn(move || {
            let mut unclaimed = 0usize;
            while !stopper.load(Ordering::Relaxed) {
                // `take_work` drains the whole queue, so every item it
                // returns must be answered — dropping the tail would
                // strand requests and blame the broker for it.
                for w in taker.take_work() {
                    let Work::Request { id, .. } = w else {
                        continue;
                    };
                    let ack = taker.deliver(Delivery {
                        id,
                        epoch: "e1".into(),
                        res: PortalResponse::json(200, r#"{"ok":true}"#),
                    });
                    if !ack.accepted {
                        unclaimed += 1;
                    }
                }
                std::hint::spin_loop();
            }
            unclaimed
        });

        for round in 0..ROUNDS {
            let mut fleet = Vec::with_capacity(CONCURRENT);
            for _ in 0..CONCURRENT {
                let bb = std::sync::Arc::clone(&b);
                fleet.push(tokio::spawn(async move {
                    tokio::time::timeout(Duration::from_secs(10), bb.park(request("/api/status")))
                        .await
                }));
            }
            for (i, task) in fleet.into_iter().enumerate() {
                let parked = task.await.expect("park task").unwrap_or_else(|_| {
                    panic!(
                        "round {round} request {i}: park waited out its dispatch timeout \
                         for an answer that had already been delivered"
                    )
                });
                assert!(parked.is_ok(), "round {round} request {i}: {parked:?}");
            }
        }
        stop.store(true, Ordering::Relaxed);
        assert_eq!(
            collector.join().expect("collector"),
            0,
            "the Mac was told an answer was unclaimed — its waiter was not yet registered"
        );
    }

    #[tokio::test]
    async fn the_parking_queue_is_bounded() {
        let b = std::sync::Arc::new(Broker::new());
        b.poll(&poll_req("t", "e1", 0, 0)).await.expect("lease");
        let mut tasks = Vec::new();
        for _ in 0..MAX_PARKED {
            let bb = std::sync::Arc::clone(&b);
            tasks.push(tokio::spawn(
                async move { bb.park(request("/api/x")).await },
            ));
        }
        // Let them all enqueue, against a deadline rather than a fixed
        // nap: a loaded machine loses a one-second race for no reason.
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline && lock(&b.parked).len() < MAX_PARKED {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            b.park(request("/api/one-too-many")).await,
            Err(DispatchError::Overloaded)
        );
        for t in tasks {
            t.abort();
        }
    }

    #[tokio::test]
    async fn a_delivery_under_a_stale_epoch_is_refused() {
        let b = std::sync::Arc::new(Broker::new());
        b.poll(&poll_req("t", "e2", 0, 0)).await.expect("lease");
        let ack = b.deliver(Delivery {
            epoch: "e1".into(),
            id: 1,
            res: PortalResponse::json(200, "{}"),
        });
        assert!(!ack.accepted);
    }

    #[tokio::test]
    async fn a_delivery_nobody_is_waiting_for_is_reported_not_accepted() {
        let b = Broker::new();
        b.poll(&poll_req("t", "e1", 0, 0)).await.expect("lease");
        let ack = b.deliver(Delivery {
            epoch: "e1".into(),
            id: 999,
            res: PortalResponse::json(200, "{}"),
        });
        assert!(!ack.accepted);
    }

    #[tokio::test]
    async fn an_epoch_rotation_fails_everything_in_flight() {
        // Those requests belong to a generation the agent abandoned;
        // leaving them to hang would hold browsers open for the whole
        // dispatch timeout for nothing.
        let b = std::sync::Arc::new(Broker::new());
        b.poll(&poll_req("t", "e1", 0, 0)).await.expect("lease");
        let bb = std::sync::Arc::clone(&b);
        let front = tokio::spawn(async move { bb.park(request("/api/x")).await });

        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline && lock(&b.pending).is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        b.poll(&poll_req("t", "e2", 0, 0)).await.expect("rotate");
        assert_eq!(front.await.expect("join"), Err(DispatchError::Lost));
    }

    #[test]
    fn the_dispatch_timeout_outlives_a_whole_poll_cycle() {
        // A request parked one millisecond after a poll returned empty
        // must survive until the next poll can collect it.
        assert!(DISPATCH_TIMEOUT > MAX_POLL_WAIT * 2);
    }

    #[test]
    fn a_lapsed_lease_is_not_live() {
        let p = Presence {
            knock_token: "t".into(),
            expected_host: None,
            tripwire_path: "/d".into(),
            epoch: "e".into(),
            last_seen: Instant::now() - PRESENCE_TTL - Duration::from_secs(1),
        };
        assert!(!p.live());
    }
}
