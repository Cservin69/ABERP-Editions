//! Library face of the ABERP Tauri shell (PR-9-2).
//!
//! # What this PR lands
//!
//! - Launches `aberp serve` as a child subprocess on Tauri startup
//!   (`backend::spawn`) per F17 resolution = option 1: parse the
//!   handshake line on stdout, not a persisted port file. PR-45a /
//!   session-61 switched the handshake to a dedicated machine-
//!   parseable line:
//!
//!     `READY 127.0.0.1:<port> sha256:<hex>`
//!
//!   The parser in `handshake` rejects anything else loudly; a silent
//!   drift in the format is exactly the CLAUDE.md rule 12 failure
//!   mode.
//!
//! - Builds a `reqwest::Client` with a pin-only TLS trust store: a
//!   custom `rustls::client::danger::ServerCertVerifier` that accepts
//!   the connection iff `SHA-256(leaf cert DER)` equals the
//!   fingerprint parsed off stdout. Per `feedback_reqwest_trust_store`,
//!   the bare `rustls::ClientConfig` is handed to reqwest via
//!   `use_preconfigured_tls`; no `add_root_certificate` builder helper
//!   (those merge with webpki defaults).
//!
//! - Reads the bearer session token from the OS keychain (service
//!   `aberp.nav.<tenant>`, account `session_token`). The Tauri shell
//!   does NOT mint tokens; minting is owned by `aberp serve`'s
//!   `load_or_create_session_token` per A28.
//!
//! - Exposes the `#[tauri::command]` surface to the Svelte SPA — the
//!   read-only routes, the PDF download, the issue/submit/poll-ack
//!   mutations, PLUS the PR-45a boot-status surface (`get_boot_status`
//!   / `retry_boot`).
//!
//! # PR-45a / session-61 — boot-status surface (extended in PR-46α)
//!
//! Pre-PR-45a, a backend boot failure called `handle.exit(1)` and the
//! Tauri window flashed blank before vanishing. The SPA's only
//! signal was "is the backend reachable" (via /health), which left
//! the operator staring at a blank window during the 5-10s cold
//! boot. PR-45a wires a four-state lifecycle (extended in PR-46α)
//! the SPA can render against:
//!
//!   - `Starting` — `aberp serve` subprocess is mid-spawn / mid-
//!     handshake. The SPA renders a loading indicator + the recent
//!     backend log lines (forwarded from stderr).
//!   - `NeedsSetup` (PR-46α) — handshake parsed with
//!     `state=needs-setup`; the keychain is empty for this tenant.
//!     The SPA renders the first-run setup wizard (four fields →
//!     POST /api/setup-nav-credentials → flip to Ready).
//!   - `Ready` — handshake parsed with `state=ready` (or the legacy
//!     no-state-suffix shape), BackendHandle stored. The SPA mounts
//!     its normal screen.
//!   - `Failed(message)` — boot errored out. The SPA renders an
//!     error pane with the message + the recent log lines + a Retry
//!     button (re-invokes `boot_backend`).
//!
//! Boot failures no longer exit the Tauri process; the operator
//! sees the failure in-window and can act on it.

#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use tauri::Manager;
use tokio::sync::Mutex;

pub mod backend;
pub mod commands;
pub mod handshake;
pub mod pinned_client;

use backend::BackendHandle;

/// Bound on the recent-log ring buffer surfaced to the SPA. Twenty
/// lines is enough to give the operator a vertical-scroll-free
/// snapshot of the cold-boot stream (cert ready + binary-hash ready +
/// listener bound = 3-4 lines at info level; 20 covers any debug-
/// level drift).
pub const RECENT_LOGS_CAP: usize = 20;

/// PR-45a / session-61 — three-state boot lifecycle exposed to the
/// SPA via `get_boot_status`. PR-46α / session-62 added the fourth
/// variant `NeedsSetup` for the first-run NAV-credentials wizard.
/// PR-51 / session-71 added the fifth variant `NeedsSellerConfig`
/// for the seller-identity wizard (NAV creds present, but
/// `~/.aberp/<tenant>/seller.toml` missing or identity-incomplete).
/// The variants are JSON-serialised as lower-case strings
/// (`"starting"`, `"needs-setup"`, `"needs-seller-config"`,
/// `"ready"`, `"failed"`) in the `commands::get_boot_status` handler;
/// the SPA's typed mirror lives in `ui/src/lib/api.ts`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootStatus {
    Starting,
    /// PR-46α / session-62 — backend handshake parsed with
    /// `state=needs-setup`; the SPA renders the first-run wizard
    /// against the loopback. The transition to `Ready` (or
    /// `NeedsSellerConfig` per PR-51) happens after the wizard's
    /// POST to `/api/setup-nav-credentials` succeeds.
    NeedsSetup,
    /// PR-51 / session-71 — backend handshake parsed with
    /// `state=needs-seller-config`; the SPA renders the
    /// `SellerConfigWizard`. Transition to `Ready` after the
    /// wizard's POST to `/api/setup-seller-info` succeeds.
    NeedsSellerConfig,
    Ready,
    Failed,
}

/// Snapshot of the boot lifecycle the SPA reads via `get_boot_status`.
/// `error` is `Some(msg)` iff `status == Failed`.
#[derive(Debug, Clone)]
pub struct BootState {
    pub status: BootStatus,
    pub error: Option<String>,
}

impl BootState {
    pub fn starting() -> Self {
        BootState {
            status: BootStatus::Starting,
            error: None,
        }
    }
}

/// Process-wide state passed to every `#[tauri::command]`.
///
/// `Arc<Mutex<Option<...>>>` shape for the backend handle because the
/// backend is launched asynchronously in `setup` — commands invoked
/// before `setup` completes loud-fail (per rule 12) rather than block.
///
/// PR-45a / session-61 added the boot-status + recent-logs surface
/// so the SPA can render the cold-boot stream instead of sitting
/// blank.
pub struct AppState {
    pub backend: Arc<Mutex<Option<BackendHandle>>>,
    pub boot_state: Arc<std::sync::Mutex<BootState>>,
    pub recent_logs: Arc<std::sync::Mutex<VecDeque<String>>>,
}

/// Push one stderr line onto the bounded recent-logs ring buffer.
/// The buffer is shared between the stderr-pump task in
/// `backend::spawn` and the `get_boot_status` Tauri command surface;
/// the operator sees the latest backend output while the SPA waits
/// on the handshake.
pub fn push_recent_log(buffer: &std::sync::Mutex<VecDeque<String>>, line: String) {
    let mut guard = buffer.lock().expect("recent_logs mutex poisoned");
    if guard.len() >= RECENT_LOGS_CAP {
        guard.pop_front();
    }
    guard.push_back(line);
}

/// The single Tauri entry point. Invoked from `main.rs` and from the
/// integration tests (`tests/handshake_parse.rs` does not invoke this
/// — it tests the parser directly; `run()` itself is exercised only
/// at the binary level).
pub fn run() {
    init_tracing();
    install_rustls_crypto_provider();

    let state = AppState {
        backend: Arc::new(Mutex::new(None)),
        boot_state: Arc::new(std::sync::Mutex::new(BootState::starting())),
        recent_logs: Arc::new(std::sync::Mutex::new(VecDeque::with_capacity(
            RECENT_LOGS_CAP,
        ))),
    };

    tauri::Builder::default()
        .manage(state)
        .setup(|app| {
            let handle = app.handle().clone();
            // Spawn the backend on the Tauri-owned tokio runtime. PR-
            // 45a / session-61 — a boot failure no longer terminates
            // the Tauri shell. The error message lands in
            // `AppState.boot_state` and the SPA renders an error
            // pane with a Retry button (re-invokes `boot_backend`
            // via the `retry_boot` Tauri command). Pre-PR-45a the
            // window briefly flashed blank then vanished — a worse
            // operator experience than the in-window error pane.
            tauri::async_runtime::spawn(async move {
                if let Err(e) = boot_backend(&handle).await {
                    let message = format!("{e:#}");
                    tracing::error!(error = %message, "backend boot failed");
                    let state = handle.state::<AppState>();
                    let mut guard = state.boot_state.lock().expect("boot_state mutex poisoned");
                    *guard = BootState {
                        status: BootStatus::Failed,
                        error: Some(message),
                    };
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::health,
            commands::acknowledge_first_prod_launch,
            commands::list_invoices,
            commands::get_invoice,
            commands::get_audit,
            commands::download_invoice_pdf,
            commands::issue_invoice,
            commands::submit_invoice_to_nav,
            commands::poll_ack,
            commands::cancel_invoice_storno,
            commands::amend_invoice_modification,
            commands::mark_invoice_paid,
            commands::get_issuance_input,
            commands::get_boot_status,
            commands::retry_boot,
            commands::setup_nav_credentials,
            commands::setup_seller_info,
            commands::get_seller_info,
            commands::get_nav_credentials_status,
            commands::rotate_nav_credential,
            commands::list_partners,
            commands::get_partner,
            commands::create_partner,
            commands::update_partner,
            commands::delete_partner,
            commands::list_notes_history,
            commands::list_products,
            commands::get_product,
            commands::create_product,
            commands::update_product,
            commands::delete_product,
            commands::list_seller_banks,
            commands::create_seller_bank,
            commands::update_seller_bank,
            commands::set_default_seller_bank,
            commands::delete_seller_bank,
            commands::get_seller_numbering,
            commands::put_seller_numbering,
            commands::get_smtp_config,
            commands::put_smtp_config,
            commands::test_smtp_connection,
            commands::email_invoice_to_buyer,
            // PR-179 / session-179 — AP module SPA surface (S177/S178
            // routes). Five thin pass-throughs to the backend's
            // incoming-invoice routes; consumed by the new
            // IncomingInvoiceList SPA screen.
            commands::list_incoming_invoices,
            commands::mark_incoming_paid,
            commands::mark_incoming_outstanding,
            commands::mark_incoming_irrelevant,
            commands::sync_incoming_invoices_now,
            // S197 / PR-197 — XML download per AP row.
            commands::download_incoming_xml,
            // S180 / PR-180 — NAV-as-DR restore wizard. Two
            // commands: trigger the wizard (POST { year }) and list
            // already-restored rows.
            commands::restore_from_nav_outgoing,
            commands::list_restored_invoices,
            // S211 / PR-210 — quote-intake config + queue surface.
            commands::get_quote_intake_config,
            commands::put_quote_intake_config,
            commands::test_quote_intake_connection,
            commands::list_quote_intake,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// Read the tenant identifier from `ABERP_TENANT`, defaulting to
/// `"default"` — matches every other CLI subcommand's default.
fn read_tenant_env() -> String {
    std::env::var("ABERP_TENANT").unwrap_or_else(|_| "default".to_string())
}

/// Resolve the `aberp` binary path. Three sources, in order:
///   1. `ABERP_BIN` environment variable (operator-explicit).
///   2. Sibling `aberp` (release) next to the running shell binary.
///   3. Sibling `aberp` (debug) — the dev `cargo run` workflow.
///
/// Loud-fails per rule 12 if none of those resolve to an existing
/// file; a Tauri shell that silently falls back to "type 'aberp' and
/// hope the user's PATH has it" is the exact failure mode CLAUDE.md
/// rule 12 names.
fn resolve_aberp_binary() -> Result<std::path::PathBuf> {
    if let Ok(explicit) = std::env::var("ABERP_BIN") {
        let p = std::path::PathBuf::from(explicit);
        if p.is_file() {
            return Ok(p);
        }
        return Err(anyhow!(
            "ABERP_BIN points at `{}` but no file exists there",
            p.display()
        ));
    }
    let shell_path = std::env::current_exe().context("read current_exe path")?;
    let shell_dir = shell_path
        .parent()
        .ok_or_else(|| anyhow!("current_exe has no parent dir"))?;
    let suffix = std::env::consts::EXE_SUFFIX;
    let candidate = shell_dir.join(format!("aberp{suffix}"));
    if candidate.is_file() {
        return Ok(candidate);
    }
    Err(anyhow!(
        "could not locate aberp binary — set ABERP_BIN or place it next to the shell at {}",
        shell_dir.display()
    ))
}

/// Boot the backend: spawn subprocess, parse handshake, load token,
/// build pinned client, store the handle in `AppState`. On success
/// the boot status flips to `Ready` (or `NeedsSetup` on a fresh-
/// keychain workstation per PR-46α); on failure the caller is
/// responsible for marking `Failed` (see `run`).
pub async fn boot_backend(handle: &tauri::AppHandle) -> Result<()> {
    let tenant = read_tenant_env();
    let aberp_bin = resolve_aberp_binary()?;
    let db_path = std::env::var("ABERP_DB").unwrap_or_else(|_| "./aberp.duckdb".to_string());

    let state = handle.state::<AppState>();
    // Reset the boot lifecycle for this attempt (covers the retry
    // path — a Failed state from a previous attempt should not
    // remain visible while the new spawn is in flight).
    *state.boot_state.lock().expect("boot_state mutex poisoned") = BootState::starting();
    state
        .recent_logs
        .lock()
        .expect("recent_logs mutex poisoned")
        .clear();

    let recent_logs = Arc::clone(&state.recent_logs);
    let started = backend::spawn(&aberp_bin, &tenant, &db_path, recent_logs)
        .await
        .context("spawn aberp serve subprocess")?;
    tracing::info!(
        url = %started.url,
        fingerprint = %started.fingerprint_hex,
        tenant = %tenant,
        backend_state = ?started.handshake_state,
        "aberp serve handshake parsed"
    );

    let token = load_session_token(&tenant).context("load session token from OS keychain")?;
    let client =
        pinned_client::build(&started.fingerprint_hex).context("build pinned reqwest client")?;
    let handshake_state = started.handshake_state;
    let backend = BackendHandle::new(started, token, client, tenant);

    *state.backend.lock().await = Some(backend);
    // PR-46α / session-62 — first-paint dispatch is driven by the
    // backend's handshake state. NeedsSetup routes the SPA's first
    // paint to the wizard; Ready mounts the normal app. The
    // wizard's successful POST flips the Tauri-side state to Ready
    // via [`mark_ready_after_setup`].
    let new_status = match handshake_state {
        handshake::ServeBootState::Ready => BootStatus::Ready,
        handshake::ServeBootState::NeedsSetup => BootStatus::NeedsSetup,
        handshake::ServeBootState::NeedsSellerConfig => BootStatus::NeedsSellerConfig,
    };
    *state.boot_state.lock().expect("boot_state mutex poisoned") = BootState {
        status: new_status.clone(),
        error: None,
    };
    tracing::info!(status = ?new_status, "backend reached its post-handshake lifecycle state");
    Ok(())
}

/// PR-46α / session-62 — flip the Tauri-side boot state mirror after
/// a setup-wizard POST succeeds. Called from the
/// `setup_nav_credentials` and `setup_seller_info` Tauri commands.
///
/// PR-51 / session-71 — the post-NAV-creds state is no longer always
/// Ready (the seller-config wizard may still be pending), so this
/// helper now takes the backend-reported `next_state` token verbatim
/// and flips to the matching variant. Recognised tokens:
///
///   - `"ready"` → `BootStatus::Ready`
///   - `"needs-seller-config"` → `BootStatus::NeedsSellerConfig`
///
/// An unknown token is treated as a no-op + WARN log; the SPA's
/// next `getBootStatus` poll picks up the real state from the
/// backend within ~300ms via the existing poll cadence (so a
/// missing-mirror-update is a small visual lag, not a stuck wizard).
pub fn mark_post_setup_state(handle: &tauri::AppHandle, next_state: &str) {
    let new_status = match next_state {
        "ready" => BootStatus::Ready,
        "needs-seller-config" => BootStatus::NeedsSellerConfig,
        other => {
            tracing::warn!(
                state = other,
                "unknown post-setup state token; deferring to backend boot-status poll"
            );
            return;
        }
    };
    let state = handle.state::<AppState>();
    let mut guard = state.boot_state.lock().expect("boot_state mutex poisoned");
    *guard = BootState {
        status: new_status.clone(),
        error: None,
    };
    tracing::info!(
        next_state = ?new_status,
        "Tauri-side boot state mirror flipped after setup-wizard success"
    );
}

/// Look up the session token in the OS keychain — mirrors
/// `apps/aberp/src/serve.rs::load_or_create_session_token` minus the
/// minting branch. The Tauri shell never mints; if the entry is
/// absent we loud-fail and ask the operator to run `aberp serve`
/// once first (which mints the entry as a side effect).
fn load_session_token(tenant: &str) -> Result<String> {
    let service = format!("aberp.nav.{tenant}");
    let entry = keyring::Entry::new(&service, "session_token")
        .context("build keyring::Entry for session_token")?;
    match entry.get_password() {
        Ok(t) if !t.is_empty() => Ok(t),
        Ok(_) => Err(anyhow!(
            "OS keychain entry `{service}` / `session_token` is empty — run `aberp serve --tenant {tenant}` once to mint it"
        )),
        Err(keyring::Error::NoEntry) => Err(anyhow!(
            "OS keychain has no `{service}` / `session_token` entry — run `aberp serve --tenant {tenant}` once to mint it"
        )),
        Err(e) => Err(anyhow!("OS keychain access failed: {e}")),
    }
}

/// rustls 0.23 requires a process-wide crypto provider before any TLS
/// work. Matches `apps/aberp/src/main.rs::install_rustls_crypto_provider`
/// — same try-install discipline (no panic if a transitive crate
/// already installed one).
fn install_rustls_crypto_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_recent_log_caps_at_cap() {
        let buf = std::sync::Mutex::new(VecDeque::with_capacity(RECENT_LOGS_CAP));
        // Push twice the cap; oldest entries must drop out.
        for i in 0..(RECENT_LOGS_CAP * 2) {
            push_recent_log(&buf, format!("line {i}"));
        }
        let snapshot: Vec<String> = buf.lock().unwrap().iter().cloned().collect();
        assert_eq!(snapshot.len(), RECENT_LOGS_CAP);
        // Newest line is the last one pushed; oldest is `RECENT_LOGS_CAP`.
        assert_eq!(
            snapshot.first().unwrap(),
            &format!("line {}", RECENT_LOGS_CAP)
        );
        assert_eq!(
            snapshot.last().unwrap(),
            &format!("line {}", RECENT_LOGS_CAP * 2 - 1)
        );
    }

    #[test]
    fn boot_state_starting_has_no_error() {
        let s = BootState::starting();
        assert_eq!(s.status, BootStatus::Starting);
        assert!(s.error.is_none());
    }
}
