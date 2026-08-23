//! `aberp-portal-relay` — the VPS binary.
//!
//! Two listeners, both configured entirely from the command line so the
//! deployment unit is one binary plus a systemd unit file:
//!
//! - the **agent listener**, mutually-authenticated, which the Mac
//!   polls (ADR-0115 §2.3);
//! - the **front listener**, public HTTPS with the wildcard
//!   `*.abenerp.com` certificate (§3.2).
//!
//! Both are served by [`aberp_portal_relay::http1`] rather than a web
//! framework, because the front's whole job is to be byte-identical to
//! a parked nginx and a framework answers malformed requests before any
//! of our code runs. See that module for the full argument.
//!
//! # Nothing here knows the portal's hostname
//!
//! There is no hostname argument and no hostname default. The front
//! answers whatever `Host` arrives; the wildcard certificate covers the
//! label; the WebAuthn RP ID lives on the Mac. So the deploy-time
//! secret — which label of `abenerp.com` this is — never appears in
//! this repository, in this binary, or in a CT log.
//!
//! `anyhow` appears here and nowhere else in the crate, per ADR-0021
//! Part A item 2.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use aberp_portal_core::PinnedFingerprint;
use aberp_portal_relay::{canary, http1, AgentLeg, Broker, Canary, Front};
use anyhow::{bail, Context, Result};
use clap::Parser;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio::net::TcpListener;

#[derive(Parser)]
#[command(
    name = "aberp-portal-relay",
    about = "ADR-0115 portal relay — a blind, mutually-authenticated parking lot with nothing at rest",
    long_about = None
)]
struct Cli {
    /// Where browsers connect (Leg A).
    #[arg(long, default_value = "0.0.0.0:443")]
    front_addr: SocketAddr,

    /// Where the Mac agent polls (Leg B).
    #[arg(long, default_value = "0.0.0.0:8443")]
    agent_addr: SocketAddr,

    /// The relay's own Leg-B certificate — the one the agent pins.
    #[arg(long)]
    agent_leg_cert_pem: PathBuf,

    /// The matching private key.
    #[arg(long)]
    agent_leg_key_pem: PathBuf,

    /// SHA-256 (hex) of an agent leaf certificate to accept. Repeatable
    /// — §2.3 already anticipates a short allowlist for a second Mac.
    /// At least one is required: an unpinned Leg B is not this design.
    #[arg(long = "pin-agent", required = true)]
    pin_agent: Vec<String>,

    /// Wildcard certificate chain for the front (Leg A).
    #[arg(long, required_unless_present = "front_plaintext")]
    front_cert_pem: Option<PathBuf>,

    /// Its private key.
    #[arg(long, required_unless_present = "front_plaintext")]
    front_key_pem: Option<PathBuf>,

    /// Serve the front over plaintext HTTP.
    ///
    /// For a loopback end-to-end test only. It is refused unless the
    /// front address is a loopback address, because a plaintext portal
    /// on a public interface would put the session cookie and every
    /// ceremony on the wire in the clear.
    #[arg(long)]
    front_plaintext: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    aberp_portal_core::pin::install_default_crypto_provider();

    let cli = Cli::parse();
    if cli.front_plaintext && !cli.front_addr.ip().is_loopback() {
        bail!(
            "--front-plaintext is refused on {} — it is a loopback test affordance, not a deployment mode",
            cli.front_addr
        );
    }

    let pinned: Vec<PinnedFingerprint> = cli
        .pin_agent
        .iter()
        .map(|h| PinnedFingerprint::from_hex(h))
        .collect::<Result<_, _>>()
        .context(
            "--pin-agent expects the 64-hex-character SHA-256 of the agent's leaf certificate",
        )?;

    let leg_b_tls = aberp_portal_core::pin::relay_server_config(
        pinned,
        load_chain(&cli.agent_leg_cert_pem)?,
        load_key(&cli.agent_leg_key_pem)?,
    )
    .context("building the Leg B (agent) TLS config")?;

    let broker = Arc::new(Broker::new());

    // Leg B. The TLS config demands and pins a client certificate, so
    // an unpinned peer is dropped inside the handshake — "before any
    // application byte, indistinguishable from a closed service" (§2.3).
    let agent_listener = TcpListener::bind(cli.agent_addr)
        .await
        .with_context(|| format!("binding the agent listener on {}", cli.agent_addr))?;
    tracing::info!(addr = %cli.agent_addr, "agent leg listening (mutually pinned)");
    tokio::spawn(serve_tls(
        agent_listener,
        Arc::new(leg_b_tls),
        Arc::new(AgentLeg {
            broker: Arc::clone(&broker),
        }),
    ));

    // The scanner trap. Its aggregator is a background task, so the
    // response path only ever does a non-blocking hand-off.
    let (canary_handle, canary_rx) = Canary::new();
    tokio::spawn(canary::run_aggregator(
        Arc::clone(&canary_handle),
        Arc::clone(&broker),
        canary_rx,
    ));

    let front = Arc::new(Front {
        broker,
        canary: canary_handle,
    });
    let front_listener = TcpListener::bind(cli.front_addr)
        .await
        .with_context(|| format!("binding the front on {}", cli.front_addr))?;
    tracing::info!(addr = %cli.front_addr, "front listening");

    if cli.front_plaintext {
        serve_plain(front_listener, front).await;
    } else {
        let cert = cli
            .front_cert_pem
            .expect("clap requires it without --front-plaintext");
        let key = cli
            .front_key_pem
            .expect("clap requires it without --front-plaintext");
        let tls = aberp_portal_core::pin::front_server_config(load_chain(&cert)?, load_key(&key)?)
            .context("loading the front (wildcard) certificate")?;
        serve_tls(front_listener, Arc::new(tls), front).await;
    }
    Ok(())
}

/// Accept forever, terminate TLS, hand each connection to `http1`.
///
/// # The permit is taken before `accept`
///
/// Both loops here used to be `loop { accept().await; tokio::spawn(…) }`
/// with no bound of any kind — an unbounded task, buffer and file
/// descriptor allocator handed to anyone able to open a socket, on a
/// box whose entire claim is that it holds nothing. The bound is a
/// [`http1::ConnectionLimit`], and it is acquired **before** the accept
/// rather than after, which is the part that matters twice over:
///
/// - surplus connections wait in the kernel's listen backlog instead of
///   being accepted and immediately dropped, which is what nginx does
///   when `worker_connections` is exhausted;
/// - a prober therefore cannot tell a loaded relay from a loaded nginx.
///   "Accepted, then closed without a byte" is a signature; "not
///   accepted yet" is what every busy server on the internet looks like.
async fn serve_tls<H: http1::Handler>(
    listener: TcpListener,
    tls: Arc<rustls::ServerConfig>,
    handler: Arc<H>,
) {
    let acceptor = tokio_rustls::TlsAcceptor::from(tls);
    let limit = http1::ConnectionLimit::default();
    loop {
        let permit = limit.acquire().await;
        let (tcp, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "accept failed");
                continue;
            }
        };
        let acceptor = acceptor.clone();
        let handler = Arc::clone(&handler);
        tokio::spawn(async move {
            // Held for exactly as long as the connection lives.
            let _permit = permit;
            // Metadata-only logging, Ervin's §9.5 decision: peer address
            // and timestamps, no paths, no tokens, no bodies.
            //
            // The handshake is bounded because none of `http1`'s
            // timeouts have started yet: a peer that opens a socket and
            // sends nothing would otherwise hold this slot forever,
            // inside a future that never completes.
            match tokio::time::timeout(http1::HANDSHAKE_TIMEOUT, acceptor.accept(tcp)).await {
                Ok(Ok(stream)) => http1::serve(stream, Some(peer), handler).await,
                // A failed handshake — including an unpinned client
                // certificate on Leg B — is answered with silence.
                // Nothing was served, and nothing is said about why.
                Ok(Err(e)) => tracing::debug!(%peer, error = %e, "handshake refused"),
                Err(_) => tracing::debug!(%peer, "handshake timed out"),
            }
        });
    }
}

/// The loopback-only plaintext front. Bounded identically — see
/// [`serve_tls`] on why the permit is taken before the accept.
async fn serve_plain<H: http1::Handler>(listener: TcpListener, handler: Arc<H>) {
    let limit = http1::ConnectionLimit::default();
    loop {
        let permit = limit.acquire().await;
        let (tcp, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "accept failed");
                continue;
            }
        };
        let handler = Arc::clone(&handler);
        tokio::spawn(async move {
            let _permit = permit;
            http1::serve(tcp, Some(peer), handler).await;
        });
    }
}

fn load_chain(path: &PathBuf) -> Result<Vec<CertificateDer<'static>>> {
    let pem = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let chain: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut pem.as_slice())
        .collect::<Result<_, _>>()
        .with_context(|| format!("parsing certificates from {}", path.display()))?;
    if chain.is_empty() {
        bail!("{} contains no certificate", path.display());
    }
    Ok(chain)
}

fn load_key(path: &PathBuf) -> Result<PrivateKeyDer<'static>> {
    let pem = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    rustls_pemfile::private_key(&mut pem.as_slice())
        .with_context(|| format!("parsing a private key from {}", path.display()))?
        .with_context(|| format!("{} contains no private key", path.display()))
}
