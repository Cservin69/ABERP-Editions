//! `aberp-portal-agent` — the launchd daemon and its console CLI.
//!
//! ADR-0115 §2.2 installs this as a launchd daemon with `KeepAlive`, so
//! it runs from boot and survives ABERP being stopped, crashed or
//! upgraded. The subcommands other than `run` are the **console**
//! surface (§4.3): they exist to be typed by someone sitting at the
//! Mac, which is the enrolment credential this design uses instead of
//! any remote recovery path.
//!
//! `anyhow` appears here and nowhere else in the crate, per ADR-0021
//! Part A item 2 (typed errors in libraries, `anyhow` at the binary
//! boundary).

use std::sync::Arc;

use aberp_portal_agent::{config::AgentConfig, poll, Agent};
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "aberp-portal-agent",
    about = "ADR-0115 portal agent — outbound-only remote access to this Mac",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon: dial the relay and serve the portal.
    Run,
    /// Mint a one-time, 10-minute enrolment URL at this console.
    ///
    /// This is the ONLY way a passkey is ever registered (§4.3).
    Enrol {
        /// Label for the device being enrolled, e.g. `iPhone` or `Mac`.
        #[arg(long, default_value = "iPhone")]
        label: String,
        /// Close any open enrolment window instead of opening one.
        #[arg(long)]
        cancel: bool,
    },
    /// Confirm — or reject — a passkey enrolment waiting at this Mac.
    ///
    /// ADR-0115 §4.3b. A ceremony that passed every cryptographic check
    /// is STAGED, not stored: nothing is granted until someone standing
    /// at this Mac types the code. That is the one check no remote
    /// attacker can satisfy, and enrolment is the only operation in the
    /// design that grants standing access.
    Confirm {
        /// The code shown on the enrolling device and in the alert mail.
        #[arg(long, conflicts_with = "reject")]
        code: Option<String>,
        /// Discard whatever is staged. Use this if you did not start it.
        #[arg(long)]
        reject: bool,
    },
    /// List enrolled credentials.
    Credentials,
    /// Revoke one credential by id, or every credential.
    Revoke {
        /// Credential id (base64url) as shown by `credentials`.
        #[arg(long, conflicts_with = "all")]
        id: Option<String>,
        /// Revoke every enrolled credential (§4.4 / §4.5).
        #[arg(long)]
        all: bool,
    },
    /// Mint a fresh knock token, invalidating the old bookmark (§3.3).
    RotateKnock,
    /// Print the portal's current knock URL for bookmarking.
    ///
    /// Prints the path only unless `PORTAL_HOST` is set, so the
    /// hostname is never required to be present just to read a token.
    Knock,
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
    let cfg = AgentConfig::from_env().context(
        "portal agent configuration (PORTAL_HOST, PORTAL_RELAY_ADDR, PORTAL_RELAY_CERT_SHA256, PORTAL_AGENT_CERT_PEM)",
    )?;
    let agent = Agent::new(cfg).context("initialising the portal agent state directory")?;

    match cli.command {
        Command::Run => run(agent).await,
        Command::Enrol { label, cancel } => enrol(&agent, &label, cancel),
        Command::Confirm { code, reject } => confirm(&agent, code.as_deref(), reject),
        Command::Credentials => credentials(&agent),
        Command::Revoke { id, all } => revoke(&agent, id.as_deref(), all),
        Command::RotateKnock => rotate_knock(&agent),
        Command::Knock => print_knock(&agent),
    }
}

async fn run(agent: Arc<Agent>) -> Result<()> {
    // The health poller is independent of Leg B: ABERP's state is
    // observed whether or not anyone is looking (§5.1), so the status
    // card is already accurate the moment a session opens.
    let poller = Arc::clone(&agent);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(aberp_portal_agent::health::POLL_INTERVAL);
        loop {
            ticker.tick().await;
            poller.health.tick(&poller.cfg).await;
        }
    });

    tracing::info!(
        rp_id = %agent.cfg.rp_id,
        relay = %agent.cfg.relay_addr,
        state_dir = %agent.cfg.state_dir.display(),
        "portal agent starting — outbound only, no listening sockets"
    );
    poll::run_forever(agent).await;
    Ok(())
}

fn enrol(agent: &Arc<Agent>, label: &str, cancel: bool) -> Result<()> {
    if cancel {
        agent.enrolment.clear();
        println!("Enrolment window closed.");
        return Ok(());
    }
    let knock = agent.knock.load_or_mint().context("knock token")?;
    let token = agent.enrolment.mint(label).context("minting enrolment")?;
    let host = std::env::var(aberp_portal_agent::config::PORTAL_HOST_ENV).unwrap_or_default();
    let base = if host.is_empty() {
        "https://<PORTAL_HOST>".to_string()
    } else {
        format!("https://{host}")
    };
    println!(
        "Enrolment window open for {} minutes, single use, device label `{label}`.",
        aberp_portal_agent::enrol::ENROL_TTL_SECONDS / 60
    );
    println!();
    println!("  {base}/{knock}/#enrol={token}");
    println!();
    println!("Open it on the device being enrolled and approve with Face ID / Touch ID.");
    println!("(ADR-0115 §4.3 also asks for a QR rendering of this URL — not yet built.)");
    Ok(())
}

fn confirm(agent: &Arc<Agent>, code: Option<&str>, reject: bool) -> Result<()> {
    use aberp_portal_agent::audit::Event;

    if reject {
        agent.staging.clear();
        println!("Discarded. Nothing was enrolled.");
        println!();
        println!("If you did not start this enrolment, rotate the knock token as well:");
        println!("  aberp-portal-agent rotate-knock");
        agent.audit.append(
            &Event::new("portal.enrol.rejected").reason("operator rejected at the console"),
        );
        return Ok(());
    }

    let staged = agent
        .staging
        .peek()
        .context("no passkey enrolment is waiting at this Mac")?;

    let Some(code) = code else {
        // Shown, not auto-confirmed: the operator must still type it,
        // so that reading this output is not the same act as approving.
        println!("A passkey enrolment is waiting for confirmation.");
        println!();
        println!("  device:        {}", staged.credential.label);
        println!("  credential id: {}", staged.credential.id);
        println!("  code:          {}", staged.code);
        println!();
        println!("If this is the enrolment you just started, and the code above matches");
        println!("the one on that device, run:");
        println!();
        println!("  aberp-portal-agent confirm --code {}", staged.code);
        println!();
        println!("If you did NOT start it, run `aberp-portal-agent confirm --reject`.");
        return Ok(());
    };

    let credential = agent
        .staging
        .confirm(code)
        .context("confirming the staged enrolment")?;
    let id = credential.id.clone();
    let label = credential.label.clone();
    agent
        .credentials
        .add(credential)
        .context("writing the credential store")?;
    agent
        .audit
        .append(&Event::new("portal.enrol.confirmed").credential(id.clone()));
    println!("Enrolled `{label}` ({id}).");
    println!("Sign in from that device now — no session was minted by the ceremony itself.");
    Ok(())
}

fn credentials(agent: &Arc<Agent>) -> Result<()> {
    let all = agent.credentials.load().context("credential store")?;
    if all.is_empty() {
        println!("No passkeys enrolled. Run `aberp-portal-agent enrol` at this Mac.");
        return Ok(());
    }
    for c in all {
        println!(
            "{}\t{}\tsign_count={}\tenrolled={}",
            c.id, c.label, c.sign_count, c.created_at
        );
    }
    Ok(())
}

fn revoke(agent: &Arc<Agent>, id: Option<&str>, all: bool) -> Result<()> {
    if all {
        let n = agent.credentials.revoke_all().context("revoke all")?;
        let s = agent.sessions.revoke_all();
        println!("Revoked {n} credential(s) and {s} live session(s).");
        return Ok(());
    }
    let id = id.context("pass --id <credential-id> or --all")?;
    if agent.credentials.revoke(id).context("revoke")? {
        println!("Revoked {id}.");
    } else {
        println!("No credential with id {id}.");
    }
    Ok(())
}

fn rotate_knock(agent: &Arc<Agent>) -> Result<()> {
    agent.knock.rotate().context("rotating the knock token")?;
    println!("Knock token rotated. The old bookmark now gets the uniform 404.");
    println!("Restart the agent (or wait for the next reconnect) to publish it to the relay.");
    Ok(())
}

fn print_knock(agent: &Arc<Agent>) -> Result<()> {
    let knock = agent.knock.load_or_mint().context("knock token")?;
    match std::env::var(aberp_portal_agent::config::PORTAL_HOST_ENV) {
        Ok(host) if !host.is_empty() => println!("https://{host}/{knock}/"),
        _ => println!("/{knock}/"),
    }
    Ok(())
}
