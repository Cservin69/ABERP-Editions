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

use aberp::{
    cli, export_invoice_bundle, issue_invoice, issue_modification, issue_storno, mark_abandoned,
    observe_receiver_confirmation, poll_ack, poll_annulment_ack, request_technical_annulment,
    retry_submission, serve, setup_nav_credentials, submit_annulment, submit_invoice,
};

fn main() -> Result<()> {
    init_tracing();
    install_rustls_crypto_provider();
    let args = cli::Cli::parse();
    match args.command {
        cli::Command::IssueInvoice(a) => issue_invoice::run(&a),
        cli::Command::SubmitInvoice(a) => submit_invoice::run(&a),
        cli::Command::SetupNavCredentials(a) => setup_nav_credentials::run(&a),
        cli::Command::PollAck(a) => poll_ack::run(&a),
        cli::Command::RetrySubmission(a) => retry_submission::run(&a),
        cli::Command::MarkAbandoned(a) => mark_abandoned::run(&a),
        cli::Command::Serve(a) => serve::run(&a),
        cli::Command::IssueStorno(a) => issue_storno::run(&a),
        cli::Command::IssueModification(a) => issue_modification::run(&a),
        cli::Command::RequestTechnicalAnnulment(a) => request_technical_annulment::run(&a),
        cli::Command::SubmitAnnulment(a) => submit_annulment::run(&a),
        cli::Command::PollAnnulmentAck(a) => poll_annulment_ack::run(&a),
        cli::Command::ObserveReceiverConfirmation(a) => observe_receiver_confirmation::run(&a),
        cli::Command::ExportInvoiceBundle(a) => export_invoice_bundle::run(&a),
    }
}

/// rustls 0.23 requires a process-wide crypto provider be installed
/// before any TLS work happens. The nav-transport crate installs its
/// own provider at first `NavTransport::new` call; the `aberp serve`
/// path also builds a `rustls::ServerConfig`-shaped surface via
/// `axum-server`. Installing once up-front at binary start covers both
/// flows. `try_install` (vs `install`) is loud-fail-safe: if a future
/// PR adds a second install site, the second one is a no-op rather
/// than a panic.
fn install_rustls_crypto_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
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
