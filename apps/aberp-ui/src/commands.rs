//! `#[tauri::command]` surface — the four read-only routes the SPA
//! consumes. Each forwards to the loopback `aberp serve` listener
//! with the bearer header attached and the response body relayed as
//! `serde_json::Value` (so the SPA can render arbitrary JSON without
//! a separate DTO layer per ADR-0021 §Part B).
//!
//! Errors are stringified at the boundary: Tauri commands serialise
//! to JSON, and `anyhow::Error` is not Serialize. Loud-fail wording
//! is preserved verbatim — the SPA renders the message in a banner
//! per rule 12.

use anyhow::Context;
use serde_json::{json, Value};
use tauri::{AppHandle, Manager, State};

use crate::{boot_backend, mark_post_setup_state, AppState, BootStatus};

/// `GET /health` — unauthenticated on the backend, but we still
/// route it through the same pinned client so the SPA never bypasses
/// the trust boundary.
#[tauri::command]
pub async fn health(state: State<'_, AppState>) -> Result<Value, String> {
    forward_get(&state, "/health", false).await
}

/// `GET /invoices` — authenticated; returns the list shape derived
/// per ADR-0009 §2.
#[tauri::command]
pub async fn list_invoices(state: State<'_, AppState>) -> Result<Value, String> {
    forward_get(&state, "/invoices", true).await
}

/// `GET /invoices/<id>` — authenticated; returns the single-invoice
/// detail plus its full audit-ledger trail.
#[tauri::command]
pub async fn get_invoice(state: State<'_, AppState>, invoice_id: String) -> Result<Value, String> {
    validate_invoice_id(&invoice_id).map_err(|e| format!("{e:#}"))?;
    let path = format!("/invoices/{invoice_id}");
    forward_get(&state, &path, true).await
}

/// `GET /audit/<invoice_id>` — authenticated; the evidence-bundle
/// drill-down per ADR-0009 §8.
#[tauri::command]
pub async fn get_audit(state: State<'_, AppState>, invoice_id: String) -> Result<Value, String> {
    validate_invoice_id(&invoice_id).map_err(|e| format!("{e:#}"))?;
    let path = format!("/audit/{invoice_id}");
    forward_get(&state, &path, true).await
}

/// PR-44ε.UI / session-58 — `GET /invoices/<id>/pdf`; returns the
/// raw PDF bytes for the SPA's "Download PDF" button on the invoice
/// detail modal. Unlike the other commands here, the response body
/// is binary (`application/pdf`), not JSON; the bytes are relayed to
/// the SPA as a `Vec<u8>` and the SPA re-wraps them in a `Blob` for
/// the browser-side download trigger.
#[tauri::command]
pub async fn download_invoice_pdf(
    state: State<'_, AppState>,
    invoice_id: String,
) -> Result<Vec<u8>, String> {
    validate_invoice_id(&invoice_id).map_err(|e| format!("{e:#}"))?;
    let path = format!("/invoices/{invoice_id}/pdf");
    forward_get_bytes(&state, &path).await
}

/// PR-44ζ / session-59 — `POST /invoices/issue`; the SPA's "+ New
/// Invoice" form posts the composed body here. The body is forwarded
/// verbatim — the typed shape lives on the backend's
/// `IssueInvoiceRequest` and on the SPA's `composeIssueInvoiceBody`
/// composer (`issue-invoice.ts`); this command is the pass-through
/// seam.
///
/// Returns the backend's typed response body
/// (`{invoice_id, invoice_number, state}`); the SPA navigates the
/// detail modal open on the returned `invoice_id`.
#[tauri::command]
pub async fn issue_invoice(state: State<'_, AppState>, body: Value) -> Result<Value, String> {
    forward_post(&state, "/invoices/issue", body).await
}

/// PR-44η / session-60 — `POST /invoices/<id>/submit`; the SPA's
/// "Submit to NAV" button on the invoice-detail modal posts here.
/// No body — the backend resolves the on-disk NAV XML + supplier
/// tax number from the audit ledger server-side per A162.
///
/// Returns the backend's typed response body (`{invoice_id,
/// transaction_id, state, entries_verified}`). On precondition
/// mismatch (invoice not in `Ready`) the backend returns 409; the
/// SPA renders the typed error body inline per A157.
#[tauri::command]
pub async fn submit_invoice_to_nav(
    state: State<'_, AppState>,
    invoice_id: String,
) -> Result<Value, String> {
    validate_invoice_id(&invoice_id).map_err(|e| format!("{e:#}"))?;
    let path = format!("/invoices/{invoice_id}/submit");
    forward_post(&state, &path, Value::Null).await
}

/// PR-44η / session-60 — `POST /invoices/<id>/poll-ack`; the SPA's
/// "Poll ack now" button on the invoice-detail modal posts here.
/// No body — the backend resolves the NAV transactionId from the
/// audit ledger server-side per the same posture as the CLI's
/// `aberp poll-ack`.
///
/// Returns the backend's typed response body (`{invoice_id, state,
/// attempts_made, transaction_id, diagnostic, entries_verified}`).
/// On precondition mismatch (invoice not in `Submitted` or
/// `PendingNavExists`) the backend returns 409; the SPA renders the
/// typed error body inline per A157.
#[tauri::command]
pub async fn poll_ack(state: State<'_, AppState>, invoice_id: String) -> Result<Value, String> {
    validate_invoice_id(&invoice_id).map_err(|e| format!("{e:#}"))?;
    let path = format!("/invoices/{invoice_id}/poll-ack");
    forward_post(&state, &path, Value::Null).await
}

/// PR-47α / session-64 — `POST /api/invoices/<id>/storno`; the SPA's
/// "Cancel invoice (storno)" button on the invoice-detail modal posts
/// here. The backend resolves the operator's original invoice-content
/// JSON from the side-stored `<ULID>.input.json` (per A174) and the
/// base's NAV XML path from the audit ledger.
///
/// PR-83 — the body now carries an optional buyer-facing storno
/// reason ("Sztornó indoka / Storno reason"). Wire shape:
/// `{ "stornoReason": <string> | null }`. The body is forwarded
/// verbatim — the typed shape lives on the backend's
/// `StornoInvoiceRequest`; this command is the pass-through seam.
/// A `null` reason matches the pre-PR-83 wire (no buyer-facing
/// reason); CLI fallback unaffected.
///
/// Returns the backend's typed response body (`{invoice_id,
/// invoice_number, state, modification_index, entries_verified}`).
/// On precondition mismatch (base not in `Finalized`) the backend
/// returns 409; the SPA renders the typed error body inline per A157.
#[tauri::command]
pub async fn cancel_invoice_storno(
    state: State<'_, AppState>,
    invoice_id: String,
    body: Value,
) -> Result<Value, String> {
    validate_invoice_id(&invoice_id).map_err(|e| format!("{e:#}"))?;
    let path = format!("/api/invoices/{invoice_id}/storno");
    forward_post(&state, &path, body).await
}

/// PR-47β / session-65 — `POST /api/invoices/<id>/modification`; the
/// SPA's "Amend invoice (modification)" button on the invoice-detail
/// modal posts here with the operator-edited body (full corrected
/// invoice content + operator-supplied `modificationDate`). Unlike
/// storno, modification IS operator-edited — the new wire content can
/// differ from the base's, but the currency must match (ADR-0037 §4
/// invariant C6, enforced 400 at the route layer).
///
/// Returns the backend's typed response body (`{invoice_id,
/// invoice_number, state, modification_index, entries_verified}`).
/// On precondition mismatch (base not in `Finalized` or `Amended`)
/// the backend returns 409; on C6 mismatch the backend returns 400.
/// The SPA renders both inline per A157.
#[tauri::command]
pub async fn amend_invoice_modification(
    state: State<'_, AppState>,
    invoice_id: String,
    body: Value,
) -> Result<Value, String> {
    validate_invoice_id(&invoice_id).map_err(|e| format!("{e:#}"))?;
    let path = format!("/api/invoices/{invoice_id}/modification");
    forward_post(&state, &path, body).await
}

/// PR-70 / ADR-0039 — `POST /api/invoices/<id>/mark-paid`; the SPA's
/// "Mark as paid" button on the invoice-detail modal posts here with
/// the operator-supplied payment metadata (paid_at + amount_minor +
/// currency + method + optional reference). Records the payment as
/// operational audit metadata WITHOUT changing the NAV regulatory
/// state ladder.
///
/// Returns the backend's typed response body (`{invoice_id, payment,
/// entries_verified}`). Failure modes:
///   - 400 — invalid paid_at format OR currency mismatch with invoice.
///   - 409 — invoice not in `Finalized` state, OR already paid (the
///     409 body carries the existing payment record so the SPA can
///     render the duplicate gracefully).
///   - 500 — propagated audit-write / chain-verify error.
#[tauri::command]
pub async fn mark_invoice_paid(
    state: State<'_, AppState>,
    invoice_id: String,
    body: Value,
) -> Result<Value, String> {
    validate_invoice_id(&invoice_id).map_err(|e| format!("{e:#}"))?;
    let path = format!("/api/invoices/{invoice_id}/mark-paid");
    forward_post(&state, &path, body).await
}

/// PR-47β / session-65 — `GET /api/invoices/<id>/issuance-input`;
/// returns the operator's original [`InvoiceInputJson`] side-stored
/// alongside the NAV XML at issuance time (per A174). The SPA's
/// modification modal calls this on open to pre-fill its form fields
/// so the operator edits in place rather than retyping.
///
/// On 404 (no side-stored input — CLI-issued or pre-PR-47α SPA-issued)
/// the forwarding helper surfaces the error string; the SPA detects
/// the "no side-stored issuance input" message and falls back to a
/// fresh empty form with an explanatory banner.
#[tauri::command]
pub async fn get_issuance_input(
    state: State<'_, AppState>,
    invoice_id: String,
) -> Result<Value, String> {
    validate_invoice_id(&invoice_id).map_err(|e| format!("{e:#}"))?;
    let path = format!("/api/invoices/{invoice_id}/issuance-input");
    forward_get(&state, &path, true).await
}

/// PR-45a / session-61 — boot lifecycle snapshot the SPA polls
/// while it's deciding which screen to render. Returns a JSON object
/// with three fields:
///
///   - `status`: one of `"starting"`, `"ready"`, `"failed"`.
///   - `error`: error message (`string` iff `status == "failed"`,
///     `null` otherwise).
///   - `recent_logs`: array of strings (oldest first) — the last N
///     backend stderr lines, so the SPA's loading pane shows what
///     the backend is doing during the cold-boot window. The buffer
///     is bounded by `RECENT_LOGS_CAP` (20 lines).
///
/// The SPA polls this every few hundred ms while the backend isn't
/// `ready`; once `ready` the SPA mounts the existing screens and
/// stops polling. A `failed` status renders an error pane with a
/// Retry button that calls `retry_boot`.
#[tauri::command]
pub async fn get_boot_status(state: State<'_, AppState>) -> Result<Value, String> {
    let boot_state = {
        let guard = state
            .boot_state
            .lock()
            .map_err(|e| format!("boot_state mutex poisoned: {e}"))?;
        guard.clone()
    };
    let recent_logs: Vec<String> = {
        let guard = state
            .recent_logs
            .lock()
            .map_err(|e| format!("recent_logs mutex poisoned: {e}"))?;
        guard.iter().cloned().collect()
    };
    let status_str = match boot_state.status {
        BootStatus::Starting => "starting",
        BootStatus::NeedsSetup => "needs-setup",
        BootStatus::NeedsSellerConfig => "needs-seller-config",
        BootStatus::Ready => "ready",
        BootStatus::Failed => "failed",
    };
    Ok(json!({
        "status": status_str,
        "error": boot_state.error,
        "recent_logs": recent_logs,
    }))
}

/// PR-46α / session-62 — relay the SPA's first-run setup wizard
/// payload to the backend's `POST /api/setup-nav-credentials` route.
/// In `NeedsSetup` boot state the backend bypasses Bearer-auth
/// (A170 chicken-and-egg posture); this command forwards the JSON
/// body verbatim and on a 200 response flips the Tauri-side boot
/// state to `Ready` via [`mark_ready_after_setup`] so the SPA's next
/// `get_boot_status` poll picks up the transition seamlessly.
///
/// The body is taken as a generic `Value` (matching the
/// `issue_invoice` posture per A156): the SPA composer owns the
/// snake_case shape; the backend's `SetupNavCredentialsRequest`
/// deserialiser is the type-level pin. Adding a strongly-typed
/// `#[derive(Deserialize)]` here would force a new workspace
/// dependency (`serde` derives) on the Tauri shell — out of scope
/// per CLAUDE.md rule 2 (minimum code, no speculative abstractions).
///
/// Failure modes propagate as the rejected promise (matching the
/// existing `issue_invoice` / `submit_invoice_to_nav` posture):
/// the typed `400` validation body and the `503` not-in-needs-setup
/// body both surface as `Err(String)` to the SPA's inline-error
/// renderer.
#[tauri::command]
pub async fn setup_nav_credentials(app: AppHandle, body: Value) -> Result<Value, String> {
    let state = app.state::<AppState>();
    let response = forward_post(&state, "/api/setup-nav-credentials", body).await?;
    // PR-51 / session-71 — the response body's `state` field carries
    // the backend's post-transition boot state, which is now either
    // `"ready"` (seller.toml already populated) OR
    // `"needs-seller-config"` (the seller-config wizard fires next).
    // Mirror that on the Tauri shell side so the SPA's next
    // `get_boot_status` poll returns the matching snapshot.
    if let Some(token) = response.get("state").and_then(Value::as_str) {
        mark_post_setup_state(&app, token);
    }
    Ok(response)
}

/// PR-51 / session-71 — relay the SPA's seller-config wizard payload
/// to the backend's `POST /api/setup-seller-info` route. Mirrors the
/// `setup_nav_credentials` command's shape exactly: forwards the
/// JSON body verbatim, flips the Tauri-side boot state mirror on
/// success, propagates the typed 400 / 500 error bodies as the
/// rejected promise.
#[tauri::command]
pub async fn setup_seller_info(app: AppHandle, body: Value) -> Result<Value, String> {
    let state = app.state::<AppState>();
    let response = forward_post(&state, "/api/setup-seller-info", body).await?;
    // The seller-config wizard is the last gate; the response's
    // `state` field is always `"ready"`. Flip the mirror so the SPA
    // re-renders the normal app without waiting for the next poll.
    if let Some(token) = response.get("state").and_then(Value::as_str) {
        mark_post_setup_state(&app, token);
    }
    Ok(response)
}

/// PR-53 / session-73 — `GET /api/seller-info`. Used by the SPA's
/// Tenant Settings page to read the saved seller identity (legal
/// name + tax number + EU VAT + address + bank) before the operator
/// decides to edit any of the fields.
#[tauri::command]
pub async fn get_seller_info(state: State<'_, AppState>) -> Result<Value, String> {
    forward_get(&state, "/api/seller-info", true).await
}

/// PR-53 / session-73 — `GET /api/nav-credentials-status`. Used by
/// the SPA's NAV Credentials settings page to show the four presence
/// rows + the operator-visible login value.
#[tauri::command]
pub async fn get_nav_credentials_status(state: State<'_, AppState>) -> Result<Value, String> {
    forward_get(&state, "/api/nav-credentials-status", true).await
}

/// PR-53 / session-73 — `POST /api/rotate-nav-credential`. The
/// Settings page POSTs one of these per "Rotate" button click so the
/// operator can update a single secret without re-entering the other
/// three. Errors propagate as the rejected promise.
#[tauri::command]
pub async fn rotate_nav_credential(
    state: State<'_, AppState>,
    body: Value,
) -> Result<Value, String> {
    forward_post(&state, "/api/rotate-nav-credential", body).await
}

/// PR-54 / session-74 — `GET /api/partners[?search=]`. The SPA's
/// PartnersList screen calls this on open + on every typeahead
/// keystroke (debounced 200ms client-side). `search` is appended as a
/// query-string only when non-empty so the route's `Option<String>`
/// deserialiser treats absence and empty-string identically.
#[tauri::command]
pub async fn list_partners(
    state: State<'_, AppState>,
    search: Option<String>,
) -> Result<Value, String> {
    let path = match search.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(needle) => format!("/api/partners?search={}", urlencode(needle)),
        None => "/api/partners".to_string(),
    };
    forward_get(&state, &path, true).await
}

/// PR-54 / session-74 — `GET /api/partners/:id`. Used by the SPA when
/// an operator clicks "Edit" on a list row OR when a deep-link wants
/// to surface a single partner by id. Returns the full Partner JSON.
#[tauri::command]
pub async fn get_partner(state: State<'_, AppState>, partner_id: String) -> Result<Value, String> {
    validate_partner_id(&partner_id).map_err(|e| format!("{e:#}"))?;
    let path = format!("/api/partners/{partner_id}");
    forward_get(&state, &path, true).await
}

/// PR-54 / session-74 — `POST /api/partners`. PartnerForm modal POSTs
/// the composed inputs body here. Backend returns 201 with the
/// freshly-created Partner (server-minted id + timestamps). Validation
/// failures surface as the typed `{ "error": "validation_failed",
/// "fields": [...] }` 400 body the SPA renders inline per A157.
#[tauri::command]
pub async fn create_partner(state: State<'_, AppState>, body: Value) -> Result<Value, String> {
    forward_post(&state, "/api/partners", body).await
}

/// PR-54 / session-74 — `PUT /api/partners/:id`. PartnerForm modal's
/// edit path PUTs the composed inputs body here. Backend returns 200
/// with the updated Partner (bumped `updated_at`). 404 on unknown id;
/// 400 on validation failure (same envelope as create).
#[tauri::command]
pub async fn update_partner(
    state: State<'_, AppState>,
    partner_id: String,
    body: Value,
) -> Result<Value, String> {
    validate_partner_id(&partner_id).map_err(|e| format!("{e:#}"))?;
    let path = format!("/api/partners/{partner_id}");
    forward_put(&state, &path, body).await
}

/// PR-54 / session-74 — `DELETE /api/partners/:id`. PartnerList's
/// per-row Delete button POSTs here (after the inline confirm). The
/// backend soft-deletes the row (kept in the DB for
/// historical-invoice resolution per A182); subsequent GETs surface
/// 404. Returns `null` on the happy path (HTTP 204 → no JSON body).
#[tauri::command]
pub async fn delete_partner(state: State<'_, AppState>, partner_id: String) -> Result<(), String> {
    validate_partner_id(&partner_id).map_err(|e| format!("{e:#}"))?;
    let path = format!("/api/partners/{partner_id}");
    forward_delete(&state, &path).await
}

/// PR-72 / session-94 — `GET /api/seller/banks`. Used by the SPA's
/// Tenant Settings page (bank-accounts subsection) + the
/// SellerConfigWizard's multi-row block to render the current
/// per-tenant bank-account collection. Body is `{banks: [...]}`.
#[tauri::command]
pub async fn list_seller_banks(state: State<'_, AppState>) -> Result<Value, String> {
    forward_get(&state, "/api/seller/banks", true).await
}

/// PR-72 / session-94 — `POST /api/seller/banks`. The Tenant Settings
/// "Add bank account" modal + the SetupWizard's "+ Add another bank
/// account" affordance POST here. Body shape mirrors the backend
/// `SellerBankInputs` (snake_case `currency`, `account_number`,
/// `bank_name`, `swift_bic`, `set_as_default`).
#[tauri::command]
pub async fn create_seller_bank(state: State<'_, AppState>, body: Value) -> Result<Value, String> {
    forward_post(&state, "/api/seller/banks", body).await
}

/// PR-72 / session-94 — `PUT /api/seller/banks/:id`. The "Edit"
/// affordance on the bank-accounts list PUTs here.
#[tauri::command]
pub async fn update_seller_bank(
    state: State<'_, AppState>,
    bank_id: String,
    body: Value,
) -> Result<Value, String> {
    validate_bank_id(&bank_id).map_err(|e| format!("{e:#}"))?;
    let path = format!("/api/seller/banks/{bank_id}");
    forward_put(&state, &path, body).await
}

/// PR-72 / session-94 — `POST /api/seller/banks/:id/set-default`.
/// The "Set as default" per-row button POSTs here (no body).
#[tauri::command]
pub async fn set_default_seller_bank(
    state: State<'_, AppState>,
    bank_id: String,
) -> Result<Value, String> {
    validate_bank_id(&bank_id).map_err(|e| format!("{e:#}"))?;
    let path = format!("/api/seller/banks/{bank_id}/set-default");
    forward_post(&state, &path, Value::Null).await
}

/// PR-72 / session-94 — `DELETE /api/seller/banks/:id`. The "Delete"
/// per-row button POSTs here. 409 Conflict surfaces if the delete
/// would leave a currency unrepresented while others still have
/// entries (the brief's explicit refusal rule).
#[tauri::command]
pub async fn delete_seller_bank(
    state: State<'_, AppState>,
    bank_id: String,
) -> Result<Value, String> {
    validate_bank_id(&bank_id).map_err(|e| format!("{e:#}"))?;
    let path = format!("/api/seller/banks/{bank_id}");
    forward_delete_returning_json(&state, &path).await
}

/// PR-45a / session-61 — the SPA's Retry button calls this command
/// from the "backend boot failed" error pane. Spawns a fresh
/// `boot_backend` attempt; the SPA continues polling
/// `get_boot_status` and re-renders based on the lifecycle that
/// follows. On a successful retry the SPA flips from the error pane
/// to the normal screen with no further operator action.
///
/// Note the boot_state reset happens inside `boot_backend` itself
/// (idempotent), so the SPA sees `starting` immediately on the next
/// poll.
#[tauri::command]
pub async fn retry_boot(app: AppHandle) -> Result<(), String> {
    // Spawn on the Tauri-owned async runtime so the await on this
    // command returns immediately — the SPA does not want a Retry
    // click to block on the full boot timeline.
    tauri::async_runtime::spawn(async move {
        if let Err(e) = boot_backend(&app).await {
            let message = format!("{e:#}");
            tracing::error!(error = %message, "backend boot failed (retry)");
            let state = app.state::<AppState>();
            let mut guard = match state.boot_state.lock() {
                Ok(g) => g,
                Err(e) => {
                    tracing::error!(error = %e, "boot_state mutex poisoned on retry");
                    return;
                }
            };
            *guard = crate::BootState {
                status: BootStatus::Failed,
                error: Some(message),
            };
        }
    });
    Ok(())
}

/// Single point of contact with the backend. Locks the backend
/// mutex briefly to grab `url + token + client` and releases before
/// the HTTP roundtrip so command latency doesn't serialise across
/// the shell.
async fn forward_get(
    state: &State<'_, AppState>,
    path: &str,
    authenticated: bool,
) -> Result<Value, String> {
    let (url, token, client) = {
        let guard = state.backend.lock().await;
        let backend = guard
            .as_ref()
            .ok_or_else(|| "backend not ready yet — wait a moment and retry".to_string())?;
        (
            format!("{}{}", backend.url, path),
            backend.session_token.clone(),
            backend.client.clone(),
        )
    };

    let mut req = client.get(&url);
    if authenticated {
        req = req.bearer_auth(&token);
    }
    let resp = req
        .send()
        .await
        .with_context(|| format!("HTTPS GET {url}"))
        .map_err(|e| format!("{e:#}"))?;

    let status = resp.status();
    let body = resp
        .text()
        .await
        .with_context(|| format!("read body of {url}"))
        .map_err(|e| format!("{e:#}"))?;

    if !status.is_success() {
        return Err(format!("backend returned {status} for {path}: {body}"));
    }
    let value: Value = serde_json::from_str(&body)
        .with_context(|| format!("parse JSON body of {url}: `{body}`"))
        .map_err(|e| format!("{e:#}"))?;
    Ok(value)
}

/// PR-44ε.UI / session-58 — binary-body sibling of [`forward_get`].
///
/// The four pre-existing routes return JSON; the new
/// `/invoices/<id>/pdf` route returns `application/pdf` bytes. JSON
/// decoding is wrong for those bytes — a `serde_json::from_str` on a
/// PDF would always fail at the first non-JSON byte. This helper
/// reads the response as raw bytes and surfaces non-2xx as an error
/// string (matching the JSON path's posture).
async fn forward_get_bytes(state: &State<'_, AppState>, path: &str) -> Result<Vec<u8>, String> {
    let (url, token, client) = {
        let guard = state.backend.lock().await;
        let backend = guard
            .as_ref()
            .ok_or_else(|| "backend not ready yet — wait a moment and retry".to_string())?;
        (
            format!("{}{}", backend.url, path),
            backend.session_token.clone(),
            backend.client.clone(),
        )
    };

    let resp = client
        .get(&url)
        .bearer_auth(&token)
        .send()
        .await
        .with_context(|| format!("HTTPS GET {url}"))
        .map_err(|e| format!("{e:#}"))?;

    let status = resp.status();
    if !status.is_success() {
        // Try to surface the backend error JSON body if present so the
        // SPA renders the loud-fail message; falls back to "<no body>"
        // on a read failure rather than swallowing it silently.
        let body = resp
            .text()
            .await
            .unwrap_or_else(|_| "<no body>".to_string());
        return Err(format!("backend returned {status} for {path}: {body}"));
    }
    let bytes = resp
        .bytes()
        .await
        .with_context(|| format!("read bytes of {url}"))
        .map_err(|e| format!("{e:#}"))?;
    Ok(bytes.to_vec())
}

/// PR-44ζ / session-59 — POST sibling of [`forward_get`]. Sends
/// `body` as the request's JSON body; surfaces the backend's typed
/// 4xx error message verbatim to the SPA (so the inline-error pane
/// renders the actionable "customer name is required" rather than an
/// opaque "internal error"). The four pre-existing JSON routes are
/// all GETs; this is the first POST seam — kept narrow, no shared
/// helper with `forward_get` because the body + method differ at the
/// `RequestBuilder` layer.
async fn forward_post(
    state: &State<'_, AppState>,
    path: &str,
    body: Value,
) -> Result<Value, String> {
    let (url, token, client) = {
        let guard = state.backend.lock().await;
        let backend = guard
            .as_ref()
            .ok_or_else(|| "backend not ready yet — wait a moment and retry".to_string())?;
        (
            format!("{}{}", backend.url, path),
            backend.session_token.clone(),
            backend.client.clone(),
        )
    };

    let resp = client
        .post(&url)
        .bearer_auth(&token)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("HTTPS POST {url}"))
        .map_err(|e| format!("{e:#}"))?;

    let status = resp.status();
    let response_body = resp
        .text()
        .await
        .with_context(|| format!("read body of {url}"))
        .map_err(|e| format!("{e:#}"))?;

    if !status.is_success() {
        // Surface the backend's typed `{ "error": "..." }` body verbatim
        // so the SPA can render the operator-actionable message inline.
        // A non-JSON body (rare) falls through as the raw text.
        return Err(format!(
            "backend returned {status} for {path}: {response_body}"
        ));
    }
    let value: Value = serde_json::from_str(&response_body)
        .with_context(|| format!("parse JSON body of {url}: `{response_body}`"))
        .map_err(|e| format!("{e:#}"))?;
    Ok(value)
}

/// PR-54 / session-74 — PUT sibling of [`forward_post`]. Used by
/// `/api/partners/:id` updates. The body shape is identical to POST;
/// only the HTTP method differs (PUT for an existence-required update
/// vs POST for a create-or-validate).
async fn forward_put(
    state: &State<'_, AppState>,
    path: &str,
    body: Value,
) -> Result<Value, String> {
    let (url, token, client) = {
        let guard = state.backend.lock().await;
        let backend = guard
            .as_ref()
            .ok_or_else(|| "backend not ready yet — wait a moment and retry".to_string())?;
        (
            format!("{}{}", backend.url, path),
            backend.session_token.clone(),
            backend.client.clone(),
        )
    };

    let resp = client
        .put(&url)
        .bearer_auth(&token)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("HTTPS PUT {url}"))
        .map_err(|e| format!("{e:#}"))?;

    let status = resp.status();
    let response_body = resp
        .text()
        .await
        .with_context(|| format!("read body of {url}"))
        .map_err(|e| format!("{e:#}"))?;

    if !status.is_success() {
        return Err(format!(
            "backend returned {status} for {path}: {response_body}"
        ));
    }
    let value: Value = serde_json::from_str(&response_body)
        .with_context(|| format!("parse JSON body of {url}: `{response_body}`"))
        .map_err(|e| format!("{e:#}"))?;
    Ok(value)
}

/// PR-54 / session-74 — DELETE sibling of [`forward_get`]. Used by
/// `/api/partners/:id` soft-deletes. The backend returns 204 (empty
/// body) on the happy path — there's no JSON to parse, so the helper
/// returns `Ok(())` rather than `Ok(Value::Null)`. The SPA's typed
/// wrapper mirrors this as `Promise<void>`.
async fn forward_delete(state: &State<'_, AppState>, path: &str) -> Result<(), String> {
    let (url, token, client) = {
        let guard = state.backend.lock().await;
        let backend = guard
            .as_ref()
            .ok_or_else(|| "backend not ready yet — wait a moment and retry".to_string())?;
        (
            format!("{}{}", backend.url, path),
            backend.session_token.clone(),
            backend.client.clone(),
        )
    };

    let resp = client
        .delete(&url)
        .bearer_auth(&token)
        .send()
        .await
        .with_context(|| format!("HTTPS DELETE {url}"))
        .map_err(|e| format!("{e:#}"))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp
            .text()
            .await
            .unwrap_or_else(|_| "<no body>".to_string());
        return Err(format!("backend returned {status} for {path}: {body}"));
    }
    Ok(())
}

/// PR-72 / session-94 — JSON-returning DELETE sibling of
/// [`forward_delete`]. The bank-account DELETE route returns 200 +
/// the full updated collection (so the SPA re-renders the list view
/// from one source of truth without a second GET roundtrip). Pre-PR-72
/// the only DELETE seam was partner soft-delete (204 No Content), so
/// the existing helper's `Ok(())` return shape did not generalise.
async fn forward_delete_returning_json(
    state: &State<'_, AppState>,
    path: &str,
) -> Result<Value, String> {
    let (url, token, client) = {
        let guard = state.backend.lock().await;
        let backend = guard
            .as_ref()
            .ok_or_else(|| "backend not ready yet — wait a moment and retry".to_string())?;
        (
            format!("{}{}", backend.url, path),
            backend.session_token.clone(),
            backend.client.clone(),
        )
    };

    let resp = client
        .delete(&url)
        .bearer_auth(&token)
        .send()
        .await
        .with_context(|| format!("HTTPS DELETE {url}"))
        .map_err(|e| format!("{e:#}"))?;

    let status = resp.status();
    let body = resp
        .text()
        .await
        .with_context(|| format!("read body of {url}"))
        .map_err(|e| format!("{e:#}"))?;

    if !status.is_success() {
        return Err(format!("backend returned {status} for {path}: {body}"));
    }
    let value: Value = serde_json::from_str(&body)
        .with_context(|| format!("parse JSON body of {url}: `{body}`"))
        .map_err(|e| format!("{e:#}"))?;
    Ok(value)
}

/// PR-72 / session-94 — defence-in-depth path-parameter validator for
/// the `:id` segment on bank-account routes. The `bnk_<26-char-ULID>`
/// id is 30 chars total, alphanumeric + `_`; mirrors
/// [`validate_partner_id`].
fn validate_bank_id(s: &str) -> anyhow::Result<()> {
    if s.is_empty() {
        anyhow::bail!("bank_id is empty");
    }
    if s.len() > 64 {
        anyhow::bail!("bank_id length {} exceeds 64", s.len());
    }
    if !s
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        anyhow::bail!("bank_id `{s}` contains characters outside [A-Za-z0-9_-]");
    }
    Ok(())
}

/// PR-54 / session-74 — minimal percent-encoder for the `?search=`
/// query-string value. Only operator-typed prefix needles flow through
/// here; the encoder covers the small set of bytes the operator might
/// realistically type (space, `&`, `=`, `#`, `?`, `+`, `%`) plus a
/// catch-all for any non-ASCII byte. Pulling in a dep just for this
/// would violate CLAUDE.md rule 2.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push('%');
                out.push_str(&format!("{:02X}", b));
            }
        }
    }
    out
}

/// PR-54 / session-74 — defence-in-depth path-parameter validator for
/// the `:id` segment on partner routes. Mirrors `validate_invoice_id`
/// (alphanumeric + `_` + `-`, capped at 64 chars) since the
/// `PartnerId` newtype is `prt_<26-char-ULID>` (30 chars total).
fn validate_partner_id(s: &str) -> anyhow::Result<()> {
    if s.is_empty() {
        anyhow::bail!("partner_id is empty");
    }
    if s.len() > 64 {
        anyhow::bail!("partner_id length {} exceeds 64", s.len());
    }
    if !s
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        anyhow::bail!("partner_id `{s}` contains characters outside [A-Za-z0-9_-]");
    }
    Ok(())
}

/// Reject obviously malformed invoice ids before they reach the
/// backend. The backend itself has its own (looser) parsing — this
/// is defence in depth against a path-injection attempt from a
/// compromised SPA build (per the ADR-0004 §Adversarial-review
/// "semi-trusted frontend" framing).
fn validate_invoice_id(s: &str) -> anyhow::Result<()> {
    if s.is_empty() {
        anyhow::bail!("invoice_id is empty");
    }
    if s.len() > 64 {
        anyhow::bail!("invoice_id length {} exceeds 64", s.len());
    }
    if !s
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        anyhow::bail!("invoice_id `{s}` contains characters outside [A-Za-z0-9_-]");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_invoice_id_accepts_typical_prefixed_ulid() {
        // inv_<26-char Crockford-base32 ULID> is the standard shape.
        assert!(validate_invoice_id("inv_01ARZ3NDEKTSV4RRFFQ69G5FAV").is_ok());
    }

    #[test]
    fn validate_invoice_id_rejects_empty() {
        assert!(validate_invoice_id("").is_err());
    }

    #[test]
    fn validate_invoice_id_rejects_path_traversal() {
        assert!(validate_invoice_id("../etc/passwd").is_err());
        assert!(validate_invoice_id("inv/foo").is_err());
    }

    #[test]
    fn validate_invoice_id_rejects_url_metacharacters() {
        assert!(validate_invoice_id("inv?id=1").is_err());
        assert!(validate_invoice_id("inv#frag").is_err());
        assert!(validate_invoice_id("inv 01").is_err());
    }

    #[test]
    fn validate_invoice_id_rejects_overlong() {
        let s = "a".repeat(65);
        assert!(validate_invoice_id(&s).is_err());
    }

    #[test]
    fn validate_partner_id_accepts_typical_prefixed_ulid() {
        assert!(validate_partner_id("prt_01ARZ3NDEKTSV4RRFFQ69G5FAV").is_ok());
    }

    #[test]
    fn validate_partner_id_rejects_path_traversal() {
        assert!(validate_partner_id("../etc/passwd").is_err());
        assert!(validate_partner_id("prt/foo").is_err());
    }

    #[test]
    fn validate_bank_id_accepts_typical_prefixed_ulid() {
        // `bnk_<26-char-ULID>` (Crockford-base32 over SHA-256[..16]).
        assert!(validate_bank_id("bnk_01ARZ3NDEKTSV4RRFFQ69G5FAV").is_ok());
    }

    #[test]
    fn validate_bank_id_rejects_path_traversal() {
        assert!(validate_bank_id("../etc/passwd").is_err());
        assert!(validate_bank_id("bnk/foo").is_err());
        assert!(validate_bank_id("").is_err());
    }

    #[test]
    fn urlencode_preserves_unreserved_and_escapes_reserved() {
        // PR-54 — the operator types arbitrary text into the
        // typeahead; the encoder must keep alphanumerics + `-._~`
        // verbatim AND escape the bytes that would otherwise break
        // the query string parse on the backend.
        assert_eq!(urlencode("Alpha"), "Alpha");
        assert_eq!(urlencode("Alpha-1.2_3~"), "Alpha-1.2_3~");
        assert_eq!(urlencode("Alpha Bravo"), "Alpha%20Bravo");
        assert_eq!(urlencode("a&b=c"), "a%26b%3Dc");
        // Non-ASCII bytes (Hungarian operator name) escape one byte at
        // a time per UTF-8 — the backend's `urlencoding` round-trip
        // re-parses them as the original characters.
        assert_eq!(urlencode("Ágnes"), "%C3%81gnes");
    }
}
