//! The blind pipe (ADR-0113 §2.4).
//!
//! > The relay is a dumb authenticated pipe: no business data at rest,
//! > no WebAuthn credential store, no session issuance.
//!
//! This module is the whole relay-side of Leg B, and what it *cannot*
//! do is the specification:
//!
//! - it cannot verify a WebAuthn assertion (no crypto here beyond TLS,
//!   and `aberp-portal-agent` is not a dependency of this crate);
//! - it cannot mint a session (it only copies a `Set-Cookie` the agent
//!   produced);
//! - it cannot decide what is readable (it forwards the method verbatim
//!   and lets the agent refuse — §6.3);
//! - it cannot answer anything at all when the Mac is not connected,
//!   because the knock token arrives with the agent and leaves with it
//!   (§5.3).
//!
//! Nothing here writes to disk. The only state is one in-memory link
//! and a map of in-flight request ids.
//!
//! # What it *can* see — the named residual
//!
//! Until hardening H1 (browser↔agent HPKE, Ervin's §9.4 decision:
//! Phase 2), Leg A's TLS terminates in front of this code, so every
//! frame passing through — ceremony messages, session cookies, and from
//! Phase 1 the invoice payloads themselves — is in this process's
//! memory in plaintext. A root-level compromise of the VPS can read a
//! session while it is happening. It cannot mint one, cannot enrol,
//! cannot reach the Mac for anything outside the allowlist, and cannot
//! recover anything after the fact (nothing is at rest). That is the
//! §2.4 residual, stated where the code that carries it lives.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use aberp_portal_core::frame::{FrameError, FrameReader, FrameWriter};
use aberp_portal_core::proto::{Frame, PortalRequest, PortalResponse, PROTOCOL_VERSION};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};

/// How long the front waits for the Mac to answer before giving up.
/// Slightly longer than the agent's own upstream read timeout so a slow
/// ABERP produces the agent's typed error rather than this timeout.
pub const DISPATCH_TIMEOUT: Duration = Duration::from_secs(35);
const WRITER_QUEUE: usize = 64;

/// The live connection to the one Mac, if there is one.
#[derive(Debug)]
struct Link {
    knock_token: String,
    /// The portal hostname the agent published for this tunnel, if it
    /// has one. In memory only, for the life of the connection — the
    /// same posture as the knock token (§2.4). The canary needs it to
    /// tell "named the label" from "hit the IP".
    expected_host: Option<String>,
    /// The decoy path the agent published for this tunnel.
    tripwire_path: String,
    tunnel_id: String,
    tx: mpsc::Sender<Frame>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<PortalResponse>>>,
}

/// The relay's only state.
#[derive(Debug, Default)]
pub struct Broker {
    link: RwLock<Option<Arc<Link>>>,
}

/// Why a dispatch did not produce an answer. All three collapse to the
/// uniform 404 at the front — the browser must not learn which.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchError {
    /// No agent is connected: the Mac is down, or the tunnel is
    /// reconnecting.
    NoAgent,
    /// The agent accepted the request and never answered.
    Timeout,
    /// The tunnel dropped mid-request.
    LinkLost,
}

impl Broker {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// `true` iff an agent is connected and `candidate` is its current
    /// knock token.
    ///
    /// Constant-time (`aberp_portal_core::ct`) because §3.2 forbids a
    /// timing cliff here: a bytewise early return would turn the
    /// uniform 404 into an oracle for guessing the token prefix.
    #[must_use]
    pub fn knock_matches(&self, candidate: &str) -> bool {
        let g = self.read();
        match g.as_ref() {
            Some(link) => {
                aberp_portal_core::ct::eq(link.knock_token.as_bytes(), candidate.as_bytes())
            }
            // No agent → nothing matches → the host looks dead (§5.3).
            None => false,
        }
    }

    /// `true` iff a Mac is currently connected.
    #[must_use]
    pub fn agent_connected(&self) -> bool {
        self.read().is_some()
    }

    /// Current tunnel id, for metadata logging only.
    #[must_use]
    pub fn tunnel_id(&self) -> Option<String> {
        self.read().as_ref().map(|l| l.tunnel_id.clone())
    }

    /// The hostname the connected agent published, if any.
    #[must_use]
    pub fn expected_host(&self) -> Option<String> {
        self.read().as_ref().and_then(|l| l.expected_host.clone())
    }

    /// The decoy path the connected agent published. Falls back to the
    /// compiled-in default while no agent is connected, so the trap
    /// still recognises its own decoy during a tunnel outage.
    #[must_use]
    pub fn tripwire_path(&self) -> String {
        self.read().as_ref().map_or_else(
            || aberp_portal_core::canary::DEFAULT_TRIPWIRE_PATH.to_string(),
            |l| l.tripwire_path.clone(),
        )
    }

    /// Push a frame to the agent without waiting for room.
    ///
    /// `true` iff it was queued. Used by the canary, which must never
    /// block: it runs on a task the response path feeds, and an
    /// aggregator stalled on a full writer queue would eventually stall
    /// the observations behind it. A canary batch that cannot be sent
    /// now is retried on the next flush.
    #[must_use]
    pub fn try_send_now(&self, frame: Frame) -> bool {
        self.read()
            .as_ref()
            .is_some_and(|link| link.tx.try_send(frame).is_ok())
    }

    /// Forward one request to the Mac and wait for its answer.
    pub async fn dispatch(&self, req: PortalRequest) -> Result<PortalResponse, DispatchError> {
        let link = self
            .read()
            .as_ref()
            .map(Arc::clone)
            .ok_or(DispatchError::NoAgent)?;
        let id = link.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        link.lock_pending().insert(id, tx);

        if link.tx.send(Frame::Request { id, req }).await.is_err() {
            link.lock_pending().remove(&id);
            return Err(DispatchError::LinkLost);
        }

        match tokio::time::timeout(DISPATCH_TIMEOUT, rx).await {
            Ok(Ok(res)) => Ok(res),
            Ok(Err(_)) => Err(DispatchError::LinkLost),
            Err(_) => {
                link.lock_pending().remove(&id);
                Err(DispatchError::Timeout)
            }
        }
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Option<Arc<Link>>> {
        self.link
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn set(&self, link: Option<Arc<Link>>) {
        let mut g = self
            .link
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *g = link;
    }

    /// Drop the link iff it is still the one identified by `tunnel_id`.
    /// Guards against a reconnect racing its predecessor's teardown.
    fn clear_if(&self, tunnel_id: &str) {
        let mut g = self
            .link
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if g.as_ref().is_some_and(|l| l.tunnel_id == tunnel_id) {
            *g = None;
        }
    }
}

impl Link {
    fn lock_pending(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<u64, oneshot::Sender<PortalResponse>>> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Accept agent connections forever.
///
/// The TLS config demands and pins a client certificate
/// (`aberp_portal_core::pin::relay_server_config`), so an unpinned peer
/// is dropped inside the handshake — "before any application byte,
/// indistinguishable from a closed service" (§2.3).
pub async fn accept_forever(
    broker: Arc<Broker>,
    listener: TcpListener,
    tls: Arc<rustls::ServerConfig>,
) {
    let acceptor = tokio_rustls::TlsAcceptor::from(tls);
    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "relay accept failed");
                continue;
            }
        };
        let broker = Arc::clone(&broker);
        let acceptor = acceptor.clone();
        tokio::spawn(async move {
            // Metadata-only logging, Ervin's §9.5 decision. Peer address
            // and timestamps: no paths, no tokens, no bodies.
            match acceptor.accept(tcp).await {
                Ok(stream) => {
                    tracing::info!(%peer, "agent leg accepted");
                    if let Err(e) = serve_agent(&broker, stream).await {
                        tracing::info!(%peer, error = %e, "agent leg closed");
                    }
                }
                Err(e) => {
                    // An unpinned or absent client certificate lands
                    // here. Nothing was served.
                    tracing::info!(%peer, error = %e, "agent leg refused at the handshake");
                }
            }
        });
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AgentLegError {
    #[error("frame: {0}")]
    Frame(#[from] FrameError),
    #[error("first frame was not Hello")]
    NoHello,
    #[error("agent speaks protocol {got}, relay speaks {PROTOCOL_VERSION}")]
    ProtocolMismatch { got: u32 },
}

/// Serve one agent connection: read `Hello`, publish the link, pump
/// responses back to waiting front requests.
pub async fn serve_agent<S>(broker: &Arc<Broker>, stream: S) -> Result<(), AgentLegError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (read_half, write_half) = tokio::io::split(stream);
    let mut reader = FrameReader::new(read_half);
    let mut writer = FrameWriter::new(write_half);

    let (knock_token, expected_host, tripwire_path, tunnel_id) =
        match reader.read_frame::<Frame>().await? {
            Frame::Hello {
                protocol_version,
                knock_token,
                expected_host,
                tripwire_path,
                tunnel_id,
            } => {
                if protocol_version != PROTOCOL_VERSION {
                    return Err(AgentLegError::ProtocolMismatch {
                        got: protocol_version,
                    });
                }
                (knock_token, expected_host, tripwire_path, tunnel_id)
            }
            _ => return Err(AgentLegError::NoHello),
        };

    let (tx, mut rx) = mpsc::channel::<Frame>(WRITER_QUEUE);
    let writer_task = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            if writer.write_frame(&frame).await.is_err() {
                break;
            }
        }
        let _ = writer.shutdown().await;
    });

    let link = Arc::new(Link {
        knock_token,
        expected_host,
        tripwire_path,
        tunnel_id: tunnel_id.clone(),
        tx,
        next_id: AtomicU64::new(1),
        pending: Mutex::new(HashMap::new()),
    });
    // One Mac, one link. A second connection replaces the first — §7's
    // "a second Mac behind the same relay" is a growth seam, not Phase 0.
    broker.set(Some(Arc::clone(&link)));

    let result = pump(&link, &mut reader).await;

    broker.clear_if(&tunnel_id);
    // Fail every in-flight request rather than leaving the front's
    // futures hanging until the dispatch timeout.
    link.lock_pending().clear();
    drop(link);
    let _ = writer_task.await;
    result
}

async fn pump<R>(link: &Arc<Link>, reader: &mut FrameReader<R>) -> Result<(), AgentLegError>
where
    R: tokio::io::AsyncRead + Unpin,
{
    loop {
        let frame: Frame = match reader.read_frame().await {
            Ok(f) => f,
            Err(FrameError::Closed) => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        match frame {
            Frame::Response { id, res } => {
                if let Some(waiter) = link.lock_pending().remove(&id) {
                    let _ = waiter.send(res);
                }
            }
            Frame::Ping { nonce } => {
                let _ = link.tx.send(Frame::Pong { nonce }).await;
            }
            Frame::Pong { .. } => {}
            // Relay-only frames from the agent are a protocol error.
            // `Canary` included: it travels relay → agent only, and an
            // agent that sent one is not speaking this protocol.
            Frame::Hello { .. } | Frame::Request { .. } | Frame::Canary { .. } => {
                return Err(AgentLegError::NoHello)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn with_no_agent_nothing_knocks_and_nothing_dispatches() {
        // §5.3's top-right cell: Mac down → the host is simply not there.
        let b = Broker::new();
        assert!(!b.agent_connected());
        assert!(!b.knock_matches("anything"));
        assert!(!b.knock_matches(""));
        let req = PortalRequest {
            method: "GET".into(),
            path: "/api/status".into(),
            query: None,
            cookie: None,
            body_b64: None,
            peer: None,
        };
        assert_eq!(b.dispatch(req).await, Err(DispatchError::NoAgent));
    }

    /// Drive a whole agent leg over an in-memory duplex: the relay side
    /// is the real `serve_agent`; the far end is a minimal stand-in for
    /// the agent's framing.
    async fn with_fake_agent<F>(knock: &str, responder: F) -> Arc<Broker>
    where
        F: Fn(PortalRequest) -> PortalResponse + Send + 'static,
    {
        let (relay_side, agent_side) = tokio::io::duplex(64 * 1024);
        let broker = Arc::new(Broker::new());
        let b = Arc::clone(&broker);
        tokio::spawn(async move {
            let _ = serve_agent(&b, relay_side).await;
        });

        let knock = knock.to_string();
        tokio::spawn(async move {
            let (r, w) = tokio::io::split(agent_side);
            let mut reader = FrameReader::new(r);
            let mut writer = FrameWriter::new(w);
            writer
                .write_frame(&Frame::Hello {
                    protocol_version: PROTOCOL_VERSION,
                    knock_token: knock,
                    expected_host: Some("portal.test".into()),
                    tripwire_path: "/decoy".into(),
                    tunnel_id: "tunnel-test".into(),
                })
                .await
                .expect("hello");
            while let Ok(frame) = reader.read_frame::<Frame>().await {
                if let Frame::Request { id, req } = frame {
                    let res = responder(req);
                    writer
                        .write_frame(&Frame::Response { id, res })
                        .await
                        .expect("response");
                }
            }
        });

        // Wait for the Hello to land.
        for _ in 0..200 {
            if broker.agent_connected() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        broker
    }

    #[tokio::test]
    async fn a_connected_agent_publishes_its_knock_token() {
        let b = with_fake_agent("the-token", |_| PortalResponse::json(200, "{}")).await;
        assert!(b.agent_connected());
        assert!(b.knock_matches("the-token"));
        assert!(!b.knock_matches("the-toke"));
        assert!(!b.knock_matches("the-tokenX"));
        assert_eq!(b.tunnel_id().as_deref(), Some("tunnel-test"));
        assert_eq!(b.expected_host().as_deref(), Some("portal.test"));
        assert_eq!(b.tripwire_path(), "/decoy");
    }

    #[tokio::test]
    async fn with_no_agent_the_tripwire_falls_back_to_the_compiled_default() {
        // The trap must still recognise its own decoy during a tunnel
        // outage — that is exactly when a scan is most interesting.
        let b = Broker::new();
        assert_eq!(
            b.tripwire_path(),
            aberp_portal_core::canary::DEFAULT_TRIPWIRE_PATH
        );
        assert!(b.expected_host().is_none());
        assert!(!b.try_send_now(Frame::Ping { nonce: 1 }));
    }

    #[tokio::test]
    async fn a_request_reaches_the_agent_verbatim_and_the_answer_comes_back() {
        // Including a mutating verb: the relay MUST forward it so the
        // agent is the one that refuses (§6.3).
        let b = with_fake_agent("t", |req| {
            PortalResponse::json(200, &format!(r#"{{"saw":"{} {}"}}"#, req.method, req.path))
        })
        .await;
        let req = PortalRequest {
            method: "POST".into(),
            path: "/api/invoices".into(),
            query: None,
            cookie: None,
            body_b64: None,
            peer: None,
        };
        let res = b.dispatch(req).await.expect("dispatched");
        let body = String::from_utf8(res.body().expect("body")).expect("utf8");
        assert_eq!(body, r#"{"saw":"POST /api/invoices"}"#);
    }

    #[tokio::test]
    async fn the_relay_copies_a_cookie_it_could_not_have_minted() {
        let b = with_fake_agent("t", |_| {
            let mut r = PortalResponse::json(200, "{}");
            r.set_cookie = Some("s=agent-minted; HttpOnly".into());
            r
        })
        .await;
        let req = PortalRequest {
            method: "POST".into(),
            path: "/api/auth/finish".into(),
            query: None,
            cookie: None,
            body_b64: None,
            peer: None,
        };
        let res = b.dispatch(req).await.expect("dispatched");
        assert_eq!(res.set_cookie.as_deref(), Some("s=agent-minted; HttpOnly"));
    }
}
