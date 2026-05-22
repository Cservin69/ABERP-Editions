//! `aberp-verify` — bundle verifier binary entry point (PR-22, ADR-0035).
//!
//! Two-arg CLI per ADR-0035 §1: `--bundle <path>` (required) and
//! `--quiet` (optional). The orchestration lives in `lib::verify_bundle`;
//! `main` is responsible for: CLI parse, tracing init, report printing,
//! and process exit code per ADR-0035 §7 (0 on all-OK, 1 on any FAIL).

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::Parser;

/// Bundle verifier for ABERP per-invoice export bundles (ADR-0035).
///
/// Re-verifies the bundle's claims (hash chain, per-entry integrity,
/// payload-vs-NAV-XML byte equality, root-element pin per EventKind,
/// bundle membership, manifest invariants) from the bundle's own
/// bytes alone — no DB, no network, no keychain. Exit code 0 on
/// all-OK, 1 on any FAIL.
#[derive(Debug, Parser)]
#[command(
    name = "aberp-verify",
    about = "Verify an ABERP per-invoice export bundle from its own bytes alone (ADR-0035)."
)]
struct Args {
    /// Path to the `.tar.zst` bundle file produced by
    /// `aberp export-invoice-bundle`.
    #[arg(long)]
    bundle: PathBuf,

    /// Suppress per-check OK lines; print only NOTE / FAIL lines and
    /// the summary. Default: verbose so an inspector reading the
    /// output sees every check that ran.
    #[arg(long, default_value_t = false)]
    quiet: bool,
}

fn main() -> ExitCode {
    if let Err(e) = init_tracing() {
        eprintln!("aberp-verify: failed to initialise tracing: {e:#}");
        return ExitCode::from(2);
    }
    let args = Args::parse();
    match aberp_verify::verify_bundle(&args.bundle) {
        Ok(report) => {
            report.print(&args.bundle, args.quiet);
            if report.is_ok() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(e) => {
            // Structural failure (file-not-found / malformed archive /
            // manifest parse error) — print the error chain and exit
            // with code 2 to distinguish from semantic FAILs (code 1).
            eprintln!("aberp-verify: structural failure reading bundle: {e:#}");
            ExitCode::from(2)
        }
    }
}

fn init_tracing() -> Result<()> {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};

    // Default to WARN; operator sets RUST_LOG=info for the span/event
    // diagnostics. Output goes to stderr so stdout stays usable for
    // pipe-friendly report-line consumption.
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new("warn"))
        .context("build tracing EnvFilter")?;
    let layer = fmt::layer().with_writer(std::io::stderr).compact();
    tracing_subscriber::registry()
        .with(filter)
        .with(layer)
        .try_init()
        .context("install tracing subscriber")?;
    Ok(())
}
