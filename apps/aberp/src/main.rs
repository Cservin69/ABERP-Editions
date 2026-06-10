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
    audit_rebuild, cli, drain_pending_retries, drain_submission_queue, export_invoice_bundle,
    issue_invoice, issue_modification, issue_storno, mark_abandoned, observe_receiver_confirmation,
    poll_ack, poll_annulment_ack, print_invoice, recover_from_nav, request_technical_annulment,
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
        cli::Command::DrainSubmissionQueue(a) => drain_submission_queue::run(&a),
        cli::Command::DrainPendingRetries(a) => drain_pending_retries::run(&a),
        cli::Command::RecoverFromNav(a) => recover_from_nav::run(&a),
        cli::Command::PrintInvoice(a) => print_invoice::run(&a),
        cli::Command::AuditRebuild(a) => audit_rebuild::run(&a),
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
    // PR-46α.1 / session-62-fix — explicitly route tracing emits to
    // STDERR. `tracing_subscriber::fmt()` defaults to STDOUT, which
    // mixes structured-log output with the `aberp serve` handshake
    // line on the same byte-stream the Tauri shell's
    // `wait_for_handshake_line` reads. Two consequences pre-fix:
    //
    //   1. The Tauri shell's stderr-pump task (which populates the
    //      SPA's `recent_logs` ring buffer per session-61's
    //      loading-pane work) saw an empty stream. `latestLogLine`
    //      stayed null on every poll, so the boot-pane only showed
    //      the static fallback string — there was no live progress
    //      surface during a slow boot, which is exactly when the
    //      operator needs it most.
    //   2. The `printlnly`-written `READY ...` handshake line
    //      shared a Mutex<BufWriter<Stdout>> with every tracing
    //      emit. A drift that re-introduced a synchronous
    //      heavyweight write to stdout could in principle interleave
    //      with the handshake println (rust's `io::stdout()` is
    //      LineWriter, so newline-flushing keeps lines atomic in
    //      practice — but routing observability to a different stream
    //      removes the hazard entirely).
    //
    // The fix routes tracing to stderr. The Tauri shell's stderr
    // pump then forwards every backend log line into recent_logs;
    // the SPA's loading pane shows real-time progress; stdout
    // carries only the handshake line + any intentional
    // operator-facing `println!` (currently just the READY emit).
    //
    // Operators running `aberp` subcommands directly in a terminal
    // see the same logs in stderr that they previously saw in
    // stdout. Terminal-attached stderr is line-buffered and visible
    // by default; pipelines that captured stdout for the handshake
    // line continue to work (the line stays where they expect).
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .with_target(false)
        .init();
}
