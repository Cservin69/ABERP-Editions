//! ABERP — modular multi-tenant ERP backend (binary entry point).
//!
//! The actual orchestration modules live in `lib.rs`; `main.rs` is a
//! thin shim that wires clap parsing to the per-subcommand `run`
//! functions. The split lets integration tests under
//! `apps/aberp/tests/` exercise the same orchestration via the library
//! face (Cargo does not expose `src/main.rs`'s sibling modules to
//! integration tests).
//!
//! See `lib.rs` for the per-module commentary.

#![forbid(unsafe_code)]

use anyhow::Result;
use clap::Parser;

use aberp::{cli, issue_invoice, setup_nav_credentials, submit_invoice};

fn main() -> Result<()> {
    init_tracing();
    let args = cli::Cli::parse();
    match args.command {
        cli::Command::IssueInvoice(a) => issue_invoice::run(&a),
        cli::Command::SubmitInvoice(a) => submit_invoice::run(&a),
        cli::Command::SetupNavCredentials(a) => setup_nav_credentials::run(&a),
    }
}

fn init_tracing() {
    // Human-readable logs to stderr by default; production deployments
    // can flip to JSON via RUST_LOG / a config flag in a later PR.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}
