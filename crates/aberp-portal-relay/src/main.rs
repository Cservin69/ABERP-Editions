//! `aberp-portal-relay` — the VPS binary.
//!
//! Two listeners, both configured entirely from the command line so the
//! deployment unit is one binary plus a systemd unit file:
//!
//! - the **agent listener**, mutually-authenticated, which the Mac
//!   dials out to (ADR-0113 §2.3);
//! - the **front listener**, public HTTPS with the wildcard
//!   `*.abenerp.com` certificate (§3.2).
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
use aberp_portal_relay::{broker, front, Broker, Front};
use anyhow::{bail, Context, Result};
use clap::Parser;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

#[derive(Parser)]
#[command(
    name = "aberp-portal-relay",
    about = "ADR-0113 portal relay — a blind, mutually-authenticated pipe with nothing at rest",
    long_about = None
)]
struct Cli {
    /// Where browsers connect (Leg A).
    #[arg(long, default_value = "0.0.0.0:443")]
    front_addr: SocketAddr,

    /// Where the Mac agent dials in (Leg B).
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

    let agent_listener = tokio::net::TcpListener::bind(cli.agent_addr)
        .await
        .with_context(|| format!("binding the agent listener on {}", cli.agent_addr))?;
    tracing::info!(addr = %cli.agent_addr, "agent leg listening (mutually pinned)");
    let acceptor_broker = Arc::clone(&broker);
    tokio::spawn(broker::accept_forever(
        acceptor_broker,
        agent_listener,
        Arc::new(leg_b_tls),
    ));

    let app = front::router(Arc::new(Front { broker }));
    tracing::info!(addr = %cli.front_addr, "front listening");

    if cli.front_plaintext {
        let listener = tokio::net::TcpListener::bind(cli.front_addr)
            .await
            .with_context(|| format!("binding the front on {}", cli.front_addr))?;
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .context("serving the front")?;
    } else {
        let cert = cli
            .front_cert_pem
            .expect("clap requires it without --front-plaintext");
        let key = cli
            .front_key_pem
            .expect("clap requires it without --front-plaintext");
        let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert, &key)
            .await
            .context("loading the front (wildcard) certificate")?;
        axum_server::bind_rustls(cli.front_addr, tls)
            .serve(app.into_make_service_with_connect_info::<SocketAddr>())
            .await
            .context("serving the front")?;
    }
    Ok(())
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
