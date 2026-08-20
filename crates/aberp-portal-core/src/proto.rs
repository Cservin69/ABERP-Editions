//! The shapes that cross Leg B (ADR-0113 §2.1).
//!
//! Everything here is JSON-serialised into a [`crate::frame`] payload.
//! Bodies are base64 so one shape carries both the ceremony JSON and a
//! PDF without a second encoding path.
//!
//! # Why the relay forwards the method verbatim
//!
//! ADR-0113 §6.3 puts the read-only refusal *at the agent, on the Mac,
//! inside the trust boundary* — not in cloud code an attacker could
//! alter. That only works if the relay is method-transparent: it must
//! hand `POST /api/invoices` to the agent and let the agent refuse it,
//! rather than answering `405` itself. [`PortalRequest::method`] is
//! therefore an unvalidated free string on the relay side; the agent is
//! the one that has an opinion about it.

use serde::{Deserialize, Serialize};

/// One message on Leg B. Both directions use the same enum; the
/// variants are directional by convention, documented per-variant.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Frame {
    /// **Agent → relay**, first frame after the mTLS handshake.
    ///
    /// Carries the knock token because the token is minted, rotated and
    /// owned by the agent (ADR-0113 §3.3 — "it is minted and verified
    /// there, like everything else"). The relay holds it in memory for
    /// the life of the connection and never writes it down (§2.4), so
    /// the tunnel dropping takes the portal's visibility with it —
    /// which is exactly §5.3's "Mac down → uniform 404 even to a
    /// knocked, enrolled user".
    Hello {
        /// Wire-compat guard. A relay that does not recognise the
        /// version closes the connection rather than guessing.
        protocol_version: u32,
        /// The current knock token, base64url, no padding.
        knock_token: String,
        /// The portal's hostname, so the front can tell a probe that
        /// *named the label* from one that merely reached the IP —
        /// the difference between a HIGH and a LOW canary.
        ///
        /// It arrives with the tunnel and leaves with it, exactly like
        /// the knock token: never on the relay's disk, never in this
        /// repository (see `aberp-portal-agent::config`). A hostile
        /// relay learns the label from the first legitimate request's
        /// `Host` header anyway, so publishing it here costs nothing
        /// that was not already spent — and buys the canary its most
        /// important signal.
        ///
        /// `None` when the agent has no hostname to publish; the
        /// canary then simply never raises `NamedTheHost`.
        expected_host: Option<String>,
        /// The decoy path whose every hit is a high-severity canary.
        /// Published by the agent so rotating it needs no relay
        /// redeploy.
        tripwire_path: String,
        /// Fresh random id per connection. Sessions are bound to it
        /// (§4.4 "bound to the front connection that carried the
        /// ceremony"), so a reconnect invalidates every session.
        tunnel_id: String,
    },
    /// **Relay → agent.** A browser request that passed the knock gate.
    Request { id: u64, req: PortalRequest },
    /// **Agent → relay.** The answer to `id`.
    Response { id: u64, res: PortalResponse },
    /// **Either direction.** Keepalive; the peer answers [`Frame::Pong`].
    Ping { nonce: u64 },
    /// **Either direction.** Keepalive answer.
    Pong { nonce: u64 },
    /// **Relay → agent.** A coalesced report of probes the front saw.
    ///
    /// Travels the tunnel that already exists, in the direction it
    /// already runs, so the alert can be *sent from the Mac* — the VPS
    /// never needs SMTP credentials, which §2.4 forbids it to hold.
    Canary { batch: crate::canary::CanaryBatch },
}

/// The wire version [`Frame::Hello`] declares. Bumped when the shapes
/// in this module change incompatibly; the relay refuses anything else.
///
/// `2` added the canary: `Hello` gained `expected_host` and
/// `tripwire_path`, and [`Frame::Canary`] appeared.
pub const PROTOCOL_VERSION: u32 = 2;

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
    /// (§6.5). Metadata only — never a trust input.
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

    #[test]
    fn frame_roundtrips_through_json() {
        let f = Frame::Hello {
            protocol_version: PROTOCOL_VERSION,
            knock_token: "abc".into(),
            expected_host: Some("host.invalid".into()),
            tripwire_path: crate::canary::DEFAULT_TRIPWIRE_PATH.into(),
            tunnel_id: "t1".into(),
        };
        let s = serde_json::to_string(&f).expect("serialise");
        let back: Frame = serde_json::from_str(&s).expect("deserialise");
        assert_eq!(f, back);
    }

    #[test]
    fn request_roundtrips_with_every_optional_absent() {
        let r = Frame::Request {
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
        let s = serde_json::to_string(&r).expect("serialise");
        assert_eq!(r, serde_json::from_str::<Frame>(&s).expect("deserialise"));
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
}
