//! Leg B, from the Mac's side: dial **out**, hold the connection, never
//! listen (ADR-0113 §G1, §2.2).
//!
//! > The Mac only ever dials out (WSS + mTLS, both ends pinned). The
//! > frozen prod invoice box gains **zero** new listening ports.
//!
//! There is no `TcpListener` anywhere in this crate — that absence *is*
//! the top-ranked security goal. A reader checking G1 can grep for
//! `bind(` and find nothing, which is the point.
//!
//! # Reconnect posture
//!
//! §7 names tunnel flap as a Phase-0 failure mode: "jittered reconnect;
//! meanwhile the portal is invisibly down — G1-consistent". So the loop
//! never gives up and never escalates: exponential backoff with jitter,
//! capped, and while it is down the relay has no knock token and the
//! whole host answers 404 (§5.3). Failing invisible is the designed
//! behaviour, not a degraded one.
//!
//! Every reconnect mints a **new tunnel id**, and sessions are bound to
//! it (§4.4), so a flap logs Ervin out. That is deliberate: it is the
//! cheapest possible upper bound on the lifetime of a cookie that
//! transited relay memory (§2.4).

use std::sync::Arc;
use std::time::Duration;

use aberp_portal_core::frame::{FrameError, FrameReader, FrameWriter};
use aberp_portal_core::proto::{Frame, PROTOCOL_VERSION};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_rustls::TlsConnector;

use crate::agent::Agent;
use crate::audit::Event;
use crate::rand;

/// First reconnect delay.
pub const BACKOFF_MIN: Duration = Duration::from_secs(1);
/// Ceiling. A minute is short enough that Ervin's next attempt after a
/// relay reboot succeeds, long enough not to hammer a dead VPS.
pub const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// How many responses may queue before the writer applies backpressure.
const WRITER_QUEUE: usize = 64;

#[derive(Debug, thiserror::Error)]
pub enum TunnelError {
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
    #[error("relay server name `{name}` is not a valid TLS name")]
    BadServerName { name: String },
    #[error("dialling the relay at {addr}: {source}")]
    Dial {
        addr: String,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "Leg B handshake with {addr} failed — is the agent certificate pinned there? ({source})"
    )]
    Handshake {
        addr: String,
        #[source]
        source: std::io::Error,
    },
    #[error("Leg B frame: {0}")]
    Frame(#[from] FrameError),
    #[error("minting the tunnel id: {0}")]
    Rand(#[from] rand::RandError),
    #[error("knock token: {0}")]
    Knock(#[from] crate::knock::KnockError),
}

/// Dial, serve, reconnect — forever.
pub async fn run_forever(agent: Arc<Agent>) {
    let mut backoff = BACKOFF_MIN;
    loop {
        match connect_once(&agent).await {
            Ok(()) => {
                tracing::info!("portal tunnel closed cleanly; reconnecting");
                backoff = BACKOFF_MIN;
            }
            Err(e) => {
                tracing::warn!(error = %e, "portal tunnel down; will retry");
                agent
                    .audit
                    .append(&Event::new("portal.tunnel.down").reason(e.to_string()));
            }
        }
        let delay = jittered(backoff);
        tokio::time::sleep(delay).await;
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

/// One connection, from dial to close.
pub async fn connect_once(agent: &Arc<Agent>) -> Result<(), TunnelError> {
    let cfg = &agent.cfg;

    let chain = load_cert_chain(&cfg.client_cert_pem)?;
    let key = load_private_key(&cfg.client_key)?;
    let tls =
        aberp_portal_core::pin::agent_client_config(cfg.relay_fingerprint.clone(), chain, key)?;

    let server_name = ServerName::try_from(cfg.relay_server_name.clone()).map_err(|_| {
        TunnelError::BadServerName {
            name: cfg.relay_server_name.clone(),
        }
    })?;

    // The one and only outbound dial. Nothing in this crate listens.
    let tcp = TcpStream::connect(&cfg.relay_addr)
        .await
        .map_err(|source| TunnelError::Dial {
            addr: cfg.relay_addr.clone(),
            source,
        })?;
    let stream = TlsConnector::from(Arc::new(tls))
        .connect(server_name, tcp)
        .await
        .map_err(|source| TunnelError::Handshake {
            addr: cfg.relay_addr.clone(),
            source,
        })?;

    let tunnel_id = rand::token()?;
    let knock_token = agent.knock.load_or_mint()?;

    let (read_half, write_half) = tokio::io::split(stream);
    let mut reader = FrameReader::new(read_half);
    let mut writer = FrameWriter::new(write_half);

    // The knock token reaches the relay here and only here — pushed by
    // the Mac, held in the relay's memory, gone when this connection is
    // (§3.3, §2.4).
    writer
        .write_frame(&Frame::Hello {
            protocol_version: PROTOCOL_VERSION,
            knock_token,
            // The canary needs the label to tell "someone typed the
            // hostname" from "someone hit the IP" — the whole
            // HIGH-versus-LOW distinction. It lives in the relay's
            // memory for the life of this connection and nowhere else,
            // the same posture as the knock token above.
            expected_host: Some(cfg.rp_id.clone()),
            tripwire_path: cfg.tripwire_path.clone(),
            tunnel_id: tunnel_id.clone(),
        })
        .await?;

    agent
        .audit
        .append(&Event::new("portal.tunnel.up").reason(cfg.relay_addr.clone()));
    tracing::info!(relay = %cfg.relay_addr, "portal tunnel up");

    // One task owns the writer; everything else sends frames to it.
    let (tx, mut rx) = mpsc::channel::<Frame>(WRITER_QUEUE);
    let writer_task = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            if let Err(e) = writer.write_frame(&frame).await {
                tracing::warn!(error = %e, "portal tunnel write failed");
                break;
            }
        }
        let _ = writer.shutdown().await;
    });

    let result = serve_frames(agent, &mut reader, &tx, &tunnel_id).await;

    drop(tx);
    let _ = writer_task.await;
    // Every session minted over this tunnel dies with it (§4.4).
    agent.sessions.revoke_tunnel(&tunnel_id);
    agent
        .audit
        .append(&Event::new("portal.tunnel.closed").reason(tunnel_id));
    result
}

async fn serve_frames<R>(
    agent: &Arc<Agent>,
    reader: &mut FrameReader<R>,
    tx: &mpsc::Sender<Frame>,
    tunnel_id: &str,
) -> Result<(), TunnelError>
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
            Frame::Request { id, req } => {
                // Each request is served on its own task so a slow PDF
                // render upstream does not stall the health card.
                let agent = Arc::clone(agent);
                let tx = tx.clone();
                let tunnel_id = tunnel_id.to_string();
                tokio::spawn(async move {
                    let res = agent.handle(&req, &tunnel_id).await;
                    let _ = tx.send(Frame::Response { id, res }).await;
                });
            }
            Frame::Ping { nonce } => {
                let _ = tx.send(Frame::Pong { nonce }).await;
            }
            Frame::Pong { .. } => {}
            Frame::Canary { batch } => {
                // Off the frame loop: the alert path can block on SMTP
                // for as long as its timeout allows, and the tunnel must
                // keep answering requests while it does.
                let agent = Arc::clone(agent);
                tokio::spawn(async move { agent.canary.record(&batch).await });
            }
            // The agent is the only party that sends these; a relay
            // that sent one is not speaking this protocol.
            Frame::Hello { .. } | Frame::Response { .. } => {
                tracing::warn!("relay sent an agent-only frame; closing the tunnel");
                return Ok(());
            }
        }
    }
}

fn load_cert_chain(path: &std::path::Path) -> Result<Vec<CertificateDer<'static>>, TunnelError> {
    let pem = std::fs::read(path).map_err(|source| TunnelError::CertRead {
        path: path.display().to_string(),
        source,
    })?;
    let chain: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut pem.as_slice())
        .collect::<Result<_, _>>()
        .map_err(|source| TunnelError::CertRead {
            path: path.display().to_string(),
            source,
        })?;
    if chain.is_empty() {
        return Err(TunnelError::CertEmpty {
            path: path.display().to_string(),
        });
    }
    Ok(chain)
}

fn load_private_key(
    source: &crate::config::SecretSource,
) -> Result<PrivateKeyDer<'static>, TunnelError> {
    let pem = source.read()?;
    rustls_pemfile::private_key(&mut pem.as_bytes())
        .ok()
        .flatten()
        .ok_or(TunnelError::KeyMalformed)
}

/// Full jitter over `[base/2, base]`. Two agents (a future second Mac,
/// §7) reconnecting after the same relay reboot must not synchronise.
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

    #[test]
    fn a_pem_with_no_certificate_is_refused() {
        let dir = std::env::temp_dir().join(format!("portal-tunnel-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let p = dir.join("empty.pem");
        std::fs::write(&p, "# nothing here\n").expect("write");
        assert!(matches!(
            load_cert_chain(&p),
            Err(TunnelError::CertEmpty { .. })
        ));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_malformed_key_is_refused_rather_than_silently_unused() {
        let src = crate::config::SecretSource::Inline("not a pem".into());
        assert!(matches!(
            load_private_key(&src),
            Err(TunnelError::KeyMalformed)
        ));
    }
}
