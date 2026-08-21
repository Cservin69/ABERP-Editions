//! The shapes that cross Leg B (ADR-0115 §2.1).
//!
//! # Leg B is a POLL, not a tunnel
//!
//! Ervin's transport decision: *"no existing tunnels, just a Mac
//! querying."* Leg B is therefore the pattern
//! `crates/aberp-quote-intake` already uses against the ABERP-site
//! storefront — the Mac **pulls work**:
//!
//! 1. a browser request passes the knock at the front, and the relay
//!    **parks** it in a bounded in-memory queue;
//! 2. the Mac long-polls [`POLL_PATH`] outbound and pulls the parked
//!    request (plus any canary batches waiting for it);
//! 3. the Mac runs the read-only query locally and **posts** the answer
//!    back to [`DELIVER_PATH`], again outbound.
//!
//! Every leg is Mac-initiated. The Mac opens no inbound port (§G1), and
//! the relay never holds a socket it can push down: a poll that is not
//! answered inside [`MAX_POLL_WAIT`] returns empty, and the Mac decides
//! whether to ask again.
//!
//! # What the relay learns, and for exactly how long
//!
//! Every poll carries an [`AgentIdentity`]. That is how the relay learns
//! the current knock token — the token is minted and rotated at the
//! agent like everything else (§3.3) — and it is why the relay's
//! knowledge **expires**: a Mac that stops polling stops refreshing the
//! presence record, the record lapses, and the whole host collapses to
//! the uniform 404 for everyone (§5.3). The old tunnel got that property
//! from a socket closing; the poll model gets it from a TTL, which also
//! covers the case a socket close does not — a Mac that is wedged rather
//! than gone.
//!
//! # Why the relay forwards the method verbatim
//!
//! ADR-0115 §6.3 puts the read-only refusal *at the agent, on the Mac,
//! inside the trust boundary* — not in cloud code an attacker could
//! alter. That only works if the relay is method-transparent: it must
//! park `POST /api/invoices` and let the agent refuse it, rather than
//! answering `405` itself. [`PortalRequest::method`] is therefore an
//! unvalidated free string on the relay side; the agent is the one that
//! has an opinion about it.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The wire version every [`AgentIdentity`] declares. The relay refuses
/// anything else.
///
/// `3` replaced the persistent framed tunnel with this poll protocol.
/// There is no back-compatibility shim: both ends of Leg B ship as one
/// unit, and a version skew must fail loudly rather than negotiate.
pub const PROTOCOL_VERSION: u32 = 3;

/// Where the Mac long-polls for work.
pub const POLL_PATH: &str = "/agent/v3/poll";
/// Where the Mac posts an answer back.
pub const DELIVER_PATH: &str = "/agent/v3/deliver";

/// Longest a poll may be parked before the relay answers it empty.
///
/// Short enough that a knock-token rotation or an agent restart is
/// visible within a minute; long enough that an idle portal costs one
/// request per half-minute rather than a busy loop.
pub const MAX_POLL_WAIT: Duration = Duration::from_secs(25);

/// How long the relay treats a polling agent as present after its last
/// poll *started*.
///
/// Deliberately more than [`MAX_POLL_WAIT`] plus a reconnect: a Mac that
/// is merely between polls must not blink the portal out. Deliberately
/// finite: it is the whole of §5.3's "Mac down → the host is simply not
/// there", and the agent rotates its epoch when a gap this long passes,
/// so a session cookie can never outlive the relay's memory of the Mac.
pub const PRESENCE_TTL: Duration = Duration::from_secs(75);

/// Largest poll response or delivery body either side will accept.
///
/// 8 MiB is the invoice-PDF ceiling with room to spare (the
/// `crates/invoice-pdf` renderer emits single-page documents measured in
/// tens of kilobytes). **Residual, named:** Phase 0 buffers a whole
/// response in relay memory rather than streaming it, which ADR-0115 §7
/// flags as the Phase-1 posture to fix. Buffered-but-capped is bounded
/// and transient; the no-at-rest rule is not weakened.
///
/// The cap is a security control, not a tidiness rule: the relay must
/// hold nothing at rest (§2.4) and buffers whole bodies in memory, so an
/// unbounded body from a compromised peer is a trivial OOM. It is
/// enforced on **both** sides — the relay bounds what the agent posts,
/// and the agent bounds what a hostile relay may return.
pub const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Who is polling, and everything the relay needs from them.
///
/// Repeated on every poll rather than established once: there is no
/// session on this leg to go stale, and a relay restart therefore costs
/// one poll rather than a reconnect dance.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentIdentity {
    /// Wire-compat guard. A relay that does not recognise the version
    /// refuses the poll rather than guessing.
    pub protocol_version: u32,
    /// The current knock token, base64url, no padding.
    pub knock_token: String,
    /// The portal's hostname, so the front can tell a probe that *named
    /// the label* from one that merely reached the IP — the difference
    /// between a HIGH and a LOW canary.
    ///
    /// It arrives with each poll and lapses with the presence record,
    /// exactly like the knock token: never on the relay's disk, never in
    /// this repository (see `aberp-portal-agent::config`). A hostile
    /// relay learns the label from the first legitimate request's `Host`
    /// header anyway, so publishing it here costs nothing that was not
    /// already spent — and buys the canary its most important signal.
    ///
    /// `None` when the agent has no hostname to publish; the canary then
    /// simply never raises `NamedTheHost`.
    pub expected_host: Option<String>,
    /// The decoy path whose every hit is a high-severity canary.
    /// Published by the agent so rotating it needs no relay redeploy.
    pub tripwire_path: String,
    /// The agent's current generation id. Sessions are bound to it
    /// (§4.4), and the agent mints a fresh one whenever the relay could
    /// have forgotten it — so a cookie that transited relay memory dies
    /// no later than the relay's own memory of the Mac.
    pub epoch: String,
}

/// One long-poll, agent → relay.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PollRequest {
    pub agent: AgentIdentity,
    /// How long the relay may park this poll, in milliseconds. Clamped
    /// to [`MAX_POLL_WAIT`] at the relay — the agent asks, it does not
    /// dictate, because an agent asking for an hour would pin a relay
    /// task for an hour.
    pub wait_ms: u32,
    /// Highest [`Work::Canary`] `seq` the Mac has durably recorded.
    /// Everything at or below it may be dropped by the relay; anything
    /// above is redelivered on the next poll. `0` acknowledges nothing.
    pub ack_canary_seq: u64,
}

/// One item of work the relay hands down on a poll.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Work {
    /// A browser request that passed the knock and was parked.
    Request { id: u64, req: PortalRequest },
    /// A coalesced report of probes the front saw.
    ///
    /// It rides the poll the Mac was making anyway, in the direction
    /// that already runs, so the alert can be *sent from the Mac* — the
    /// VPS never needs SMTP credentials, which §2.4 forbids it to hold.
    ///
    /// `seq` is monotonic per relay process and is what makes canary
    /// delivery **at-least-once** rather than fire-and-forget: the relay
    /// keeps a batch until a later poll acknowledges it
    /// ([`PollRequest::ack_canary_seq`]), so a poll response lost to a
    /// dropped connection does not lose the probes with it. The agent
    /// discards a `seq` it has already recorded, which is what makes
    /// at-least-once safe to alert on.
    Canary {
        seq: u64,
        batch: crate::canary::CanaryBatch,
    },
}

/// The relay's answer to a poll.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PollResponse {
    /// Work to run. Empty when the poll timed out with nothing parked —
    /// the ordinary idle case.
    pub work: Vec<Work>,
    /// Always present. See [`Heartbeat`].
    pub heartbeat: Heartbeat,
    /// `false` when the relay had **no live presence** for this epoch
    /// before this poll — it restarted, or the Mac was away long enough
    /// to lapse. The agent treats it as "every session minted under this
    /// epoch may have outlived the relay's memory" and rotates.
    pub known_epoch: bool,
}

/// Proof of life from the relay, on every single poll response.
///
/// The canary's weakest link is silence: a relay that has crashed, been
/// firewalled, or been taken over and told to drop canary frames
/// produces exactly the same observable as a quiet internet — nothing.
/// A monotonic sequence stamped on every answer turns that silence into
/// a *detectable* event at the Mac, which is the side that owns the
/// alert path (`aberp-portal-agent::canary`).
///
/// It carries counters, never contents: a hostile relay can lie about
/// these numbers, and the design does not depend on them being true. It
/// depends only on them *arriving*.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Heartbeat {
    /// Monotonic per relay process, from 1.
    pub seq: u64,
    /// RFC-3339 UTC, stamped by the relay.
    pub emitted_at: String,
    /// Seconds since this relay process started.
    pub relay_uptime_s: u64,
    /// Probes observed since the relay started. Coarse, and only ever a
    /// cross-check against the batches that actually arrived.
    pub observed_total: u64,
    /// Requests parked and not yet pulled.
    pub parked: u32,
    /// Canary batches waiting for a poll to carry them.
    pub canary_pending: u32,
}

/// One answer, agent → relay, posted on its own outbound request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Delivery {
    /// The epoch the agent believes it is serving under. A delivery
    /// stamped with a stale epoch is dropped: it belongs to a generation
    /// whose sessions are already revoked.
    pub epoch: String,
    /// The id the work carried.
    pub id: u64,
    pub res: PortalResponse,
}

/// The relay's acknowledgement of a delivery.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeliveryAck {
    /// `true` iff a waiting front request received it. `false` means the
    /// browser had already given up, or the epoch was stale — neither is
    /// an error the agent can act on, and both are worth counting.
    pub accepted: bool,
}

/// A browser request, forwarded verbatim (minus the knock prefix).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PortalRequest {
    /// Uppercase HTTP method, forwarded **unfiltered** — see the module
    /// note. The agent's allowlist is the thing with an opinion.
    pub method: String,
    /// Path with the knock prefix already stripped, e.g. `/api/invoices`.
    pub path: String,
    /// Raw query string, without `?`. `None` when absent.
    pub query: Option<String>,
    /// The browser's `Cookie` header verbatim, if any. The agent parses
    /// its own session cookie out of it; the relay does not look.
    pub cookie: Option<String>,
    /// Request body, base64. `None` for bodyless requests.
    pub body_b64: Option<String>,
    /// Peer address as the front saw it, for the agent's audit log
    /// (§6.5). Metadata only — never a trust input, and re-sanitised on
    /// the Mac before it reaches any log (§6.5, hardening against a
    /// relay that decorates it).
    pub peer: Option<String>,
}

/// The agent's answer. The relay copies status/content-type/body onto
/// the browser response and forwards `set_cookie` if present.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PortalResponse {
    pub status: u16,
    pub content_type: String,
    /// Response body, base64.
    pub body_b64: String,
    /// A complete `Set-Cookie` value minted by the agent (§4.4:
    /// `Secure; HttpOnly; SameSite=Strict`). The relay does not
    /// construct cookies — it cannot mint a session (§4.2).
    pub set_cookie: Option<String>,
}

impl PortalResponse {
    /// A JSON response. `body` is the already-serialised JSON text.
    #[must_use]
    pub fn json(status: u16, body: &str) -> Self {
        use base64::Engine as _;
        Self {
            status,
            content_type: "application/json".to_string(),
            body_b64: base64::engine::general_purpose::STANDARD.encode(body.as_bytes()),
            set_cookie: None,
        }
    }

    /// A binary response of the given content type (the invoice PDF).
    #[must_use]
    pub fn bytes(status: u16, content_type: &str, body: &[u8]) -> Self {
        use base64::Engine as _;
        Self {
            status,
            content_type: content_type.to_string(),
            body_b64: base64::engine::general_purpose::STANDARD.encode(body),
            set_cookie: None,
        }
    }

    /// Decode the body back to bytes. Returns `None` on malformed
    /// base64 — the relay treats that as an agent it cannot render and
    /// falls back to its uniform 404 rather than emitting a
    /// distinguishing error (§3.2).
    #[must_use]
    pub fn body(&self) -> Option<Vec<u8>> {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .decode(self.body_b64.as_bytes())
            .ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> AgentIdentity {
        AgentIdentity {
            protocol_version: PROTOCOL_VERSION,
            knock_token: "abc".into(),
            expected_host: Some("host.invalid".into()),
            tripwire_path: crate::canary::DEFAULT_TRIPWIRE_PATH.into(),
            epoch: "e1".into(),
        }
    }

    #[test]
    fn a_poll_roundtrips_through_json() {
        let p = PollRequest {
            agent: identity(),
            wait_ms: 25_000,
            ack_canary_seq: 4,
        };
        let s = serde_json::to_string(&p).expect("serialise");
        assert_eq!(p, serde_json::from_str::<PollRequest>(&s).expect("back"));
    }

    #[test]
    fn work_roundtrips_with_every_optional_absent() {
        let w = Work::Request {
            id: 7,
            req: PortalRequest {
                method: "GET".into(),
                path: "/api/invoices".into(),
                query: None,
                cookie: None,
                body_b64: None,
                peer: None,
            },
        };
        let s = serde_json::to_string(&w).expect("serialise");
        assert_eq!(w, serde_json::from_str::<Work>(&s).expect("back"));
    }

    #[test]
    fn a_poll_response_always_carries_a_heartbeat() {
        // The silence detector is not optional: a response shape that
        // could omit it would let a hostile relay stay quiet AND look
        // well-formed. `serde` enforces this — the field has no default.
        let json = r#"{"work":[],"known_epoch":true}"#;
        assert!(serde_json::from_str::<PollResponse>(json).is_err());
    }

    #[test]
    fn response_body_roundtrips_binary() {
        // A PDF is not UTF-8; the base64 body must survive it intact.
        let raw = [0x25u8, 0x50, 0x44, 0x46, 0x00, 0xff, 0xfe];
        let r = PortalResponse::bytes(200, "application/pdf", &raw);
        assert_eq!(r.body().expect("decodes"), raw);
    }

    #[test]
    fn malformed_body_decodes_to_none_rather_than_panicking() {
        let r = PortalResponse {
            status: 200,
            content_type: "application/json".into(),
            body_b64: "!!!not base64!!!".into(),
            set_cookie: None,
        };
        assert!(r.body().is_none());
    }

    #[test]
    fn the_presence_ttl_outlives_a_poll_plus_a_retry() {
        // If it did not, an ordinary gap between polls would blink the
        // portal out for the operator.
        assert!(PRESENCE_TTL > MAX_POLL_WAIT * 2);
    }
}
