//! S266 / PR-255 — outbound storefront push of the material catalogue
//!
//! ## S2 / ADR-0093 — Defense-only storefront reach
//!
//! This module reaches the customer storefront (`abenerp.com`). That reach is
//! a COMPILE-TIME Defense-only capability
//! ([`crate::build_profile::storefront_polling_allowed`]): in a Portable build
//! the daemon is never spawned — the boot guard / spawn gate in
//! [`crate::serve`] refuses — so this code physically never runs there. The
//! local quote engine + manual quoting stay available in BOTH editions; only
//! the abenerp.com reach is gated.
//! (design doc §4 / §14-C).
//!
//! ABERP has **no public inbound surface** (ADR-0057: local Tauri app,
//! loopback HTTPS, no webhook). So the storefront's material dropdown is
//! not *pulled* from ABERP — it is **pushed** out: ABERP `PUT`s the public
//! projection of `quoting_materials` to `{storefront}/api/catalogue/materials`
//! on a cadence and on every operator write. The storefront caches it and
//! serves its `/quote` dropdown from that cache; the customer's browser
//! never reaches ABERP.
//!
//! ## Design choices (flagged in the PR report)
//!
//! - **Location: an app module, not a new crate.** The push reads
//!   [`crate::quoting_materials::list_public`] (which lives in the app) and
//!   needs the [`CataloguePushHandle`] in `AppState` for the on-write
//!   trigger. A crate (the quote-intake shape) would have to re-implement
//!   the table read and could not see `AppState`. When the full
//!   `crates/aberp-quoting` daemon lands (design doc §2, S271+), this
//!   module migrates into it.
//! - **Surface secret reuse (SPOC).** The brief names `ABERP_SITE_ADMIN_TOKEN`,
//!   but no such env var exists — the storefront surface's actual secret is
//!   the quote-intake bearer (`ABERP_QUOTE_INTAKE_TOKEN` / keychain
//!   `quote_intake_token` / `[quote_intake].base_url`). Per `[[aberp-smtp-spoc]]`
//!   ("one secret per surface"), the push REUSES the already-resolved
//!   quote-intake `base_url` + bearer rather than minting a second token for
//!   the same storefront. Consequence: catalogue-push is active iff
//!   quote-intake is configured (same storefront). If Ervin ever wants them
//!   decoupled, that is a follow-up introducing a shared `[storefront]`
//!   config slot — out of scope for one PR (surgical-change discipline).
//! - **Cadence** is a fixed 15 minutes ([`PUSH_CADENCE_SECS`]) per the brief;
//!   not operator-tunable in v1.
//!
//! Failure handling mirrors the S256 quote-intake daemon: exponential
//! backoff (5s → 15s → 60s → cadence) on transient errors; a **401 pauses**
//! the daemon (a rotated bearer) and the `quote.material_catalogue_pushed`
//! audit entry + the in-memory status drive the Settings "re-paste bearer"
//! prompt. Resumption is on the next `aberp serve` boot.
//!
//! ## S289 / PR-270 — hot-reload via [`crate::storefront_credential`]
//!
//! Pre-S289 the daemon cached `base_url` + `bearer` at boot, so an
//! operator changing the SPA URL only took effect after a restart. The
//! WARN-spam Ervin saw against `https://abenerp.com` while quote-intake
//! polled localhost was that gap. The service now holds an
//! `Arc<StorefrontCredentialHandle>` instead and re-resolves at the
//! top of every push cycle. The PUT `/api/quote-intake/config` route
//! calls `handle.set(...)` after a successful save, so the very next
//! cycle (operator-triggered or cadence) sees the new value. A dormant
//! handle ⇒ skip the cycle (record a `dormant` outcome and back off).
//!
//! ## S339 / PR-24 — the storefront origin shared secret
//!
//! The storefront's global guard (`hooks.server.ts`) rejects any request
//! lacking an `X-CloudFront-Secret` header that matches its
//! `CLOUDFRONT_SHARED_SECRET` with `403 "forbidden: missing origin
//! signature"`. The name says "signature" but it is a **static
//! shared-secret header compare**, NOT an HMAC (no signing / canonical
//! string / timestamp — verified S339 cross-repo). CloudFront injects
//! the header on origin requests, but its behaviours are per-path, so a
//! direct/origin hit to `/api/catalogue/materials` can arrive without it
//! and 403 → the daemon's `unexpected_status` outcome (403 ≠ 401).
//!
//! When the optional origin secret is provisioned
//! ([`crate::storefront_origin_secret`]) the push attaches the header
//! itself; when it is absent (the common case) the push is byte-for-byte
//! what it was pre-S339 and relies on CloudFront injecting it. The
//! bearer is unchanged — it is the SAME shared [`StorefrontCredentialHandle`]
//! the (working) email-outbox daemon uses, so the bearer half of the
//! storefront's dual-gate (`requireAdminAuth`, 401) was never the gap.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde::Serialize;
use tokio_util::sync::CancellationToken;
use ulid::Ulid;
use zeroize::Zeroizing;

use aberp_audit_ledger::{append_in_tx, Actor, BinaryHash, EventKind, LedgerMeta, TenantId};

use crate::storefront_credential::StorefrontCredentialHandle;

/// 15 minutes (design doc §4). Not operator-tunable in v1.
pub const PUSH_CADENCE_SECS: u64 = 900;
const REQUEST_TIMEOUT_SECS: u64 = 10;
const BOOT_DELAY_SECS: u64 = 30;
const CATALOGUE_PATH: &str = "/api/catalogue/materials";
/// S339 / PR-24 — the storefront's CloudFront→origin shared-secret
/// header. Sent only when the optional origin secret is provisioned.
const ORIGIN_SECRET_HEADER: &str = "X-CloudFront-Secret";

// ── Shared handle (lives in AppState; the on-write trigger + status) ─────

/// The status snapshot the Settings → Material Catalogue page reads to
/// show "last push" and the paused/re-paste-bearer banner.
#[derive(Debug, Clone, Serialize, Default)]
pub struct CataloguePushStatus {
    /// A push daemon is running this process (false = dormant, e.g. no
    /// storefront configured).
    pub running: bool,
    /// A 401 paused the daemon — the operator must re-paste the bearer and
    /// restart. Sticky until next boot.
    pub paused: bool,
    pub last_attempt_at: Option<String>,
    /// `ok` / `unauthorized` / `dormant` / `transport` /
    /// `rejected_<code>` (4xx) / `transient_<code>` (5xx) /
    /// `unexpected_status` (genuinely-weird 1xx/3xx).
    pub last_outcome: Option<String>,
    pub last_pushed_count: Option<i64>,
    pub last_detail: Option<String>,
}

/// Created once at `AppState` construction (so the SPA always has a status
/// to read, even dormant). When the storefront is configured, the boot
/// block clones this into the daemon `CataloguePushService` and spawns it.
#[derive(Debug)]
pub struct CataloguePushHandle {
    notify: tokio::sync::Notify,
    running: AtomicBool,
    status: Mutex<CataloguePushStatus>,
}

impl CataloguePushHandle {
    /// A dormant handle — no daemon yet. Stored in `AppState`.
    pub fn dormant() -> Arc<Self> {
        Arc::new(Self {
            notify: tokio::sync::Notify::new(),
            running: AtomicBool::new(false),
            status: Mutex::new(CataloguePushStatus::default()),
        })
    }

    /// Wake the daemon for an immediate push (operator saved a row). A
    /// no-op if no daemon is running (dormant / paused).
    pub fn trigger(&self) {
        self.notify.notify_one();
    }

    fn mark_running(&self) {
        self.running.store(true, Ordering::SeqCst);
        if let Ok(mut s) = self.status.lock() {
            s.running = true;
        }
    }

    /// Current status, for the list route.
    pub fn snapshot(&self) -> CataloguePushStatus {
        let mut s = self.status.lock().map(|g| g.clone()).unwrap_or_default();
        s.running = self.running.load(Ordering::SeqCst);
        s
    }

    fn record(&self, attempt_at: String, outcome: &PushOutcome) {
        if let Ok(mut s) = self.status.lock() {
            s.last_attempt_at = Some(attempt_at);
            s.last_outcome = Some(outcome.label());
            s.last_pushed_count = outcome.pushed_count();
            s.last_detail = outcome.detail();
            if matches!(outcome, PushOutcome::Unauthorized) {
                s.paused = true;
            }
        }
    }

    /// S289 / PR-270 — clear the sticky paused flag on a fresh successful
    /// push. Mirrors the daemon's "operator re-pasted the bearer → next
    /// cycle picks it up → un-pause" arc.
    pub fn clear_paused(&self) {
        if let Ok(mut s) = self.status.lock() {
            s.paused = false;
        }
    }
}

// ── Outcome ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushOutcome {
    Ok {
        count: i64,
    },
    Unauthorized,
    /// S342 / PR-37 — a 4xx (other than 401): the storefront REJECTED the
    /// snapshot (e.g. a contract-drift `400 "display_name is required"`).
    /// Carries the HTTP code + a scrubbed body excerpt so the operator
    /// sees the reason in the audit row / Maintenance card without
    /// curl-debugging prod. Labelled `rejected_<code>`.
    Rejected {
        status: u16,
        body: String,
    },
    /// S342 / PR-37 — a 5xx: server-side error on the storefront/origin.
    /// The daemon will retry on backoff, so this is non-terminal.
    /// Labelled `transient_<code>`.
    ServerError {
        status: u16,
        body: String,
    },
    Transport(String),
    /// A status that is neither 2xx, 401, 4xx, nor 5xx — i.e. a
    /// genuinely-weird 1xx/3xx. Should essentially never happen against a
    /// JSON API; kept as an honest catch-all rather than silently folding
    /// it into a rejection class.
    UnexpectedStatus(u16),
    /// S289 / PR-270 — the storefront credential handle was empty when
    /// the cycle started (operator hadn't configured the storefront, or
    /// flipped `enabled=false`). Distinct from `Transport` so the SPA
    /// banner reads "dormant" instead of an alarming error.
    Dormant,
}

impl PushOutcome {
    /// Operator-facing + audit outcome string. Dynamic for the HTTP-status
    /// classes (`rejected_400`, `transient_503`) so the code travels into
    /// the audit row and the Maintenance card without a second field.
    pub fn label(&self) -> String {
        match self {
            PushOutcome::Ok { .. } => "ok".to_string(),
            PushOutcome::Unauthorized => "unauthorized".to_string(),
            PushOutcome::Rejected { status, .. } => format!("rejected_{status}"),
            PushOutcome::ServerError { status, .. } => format!("transient_{status}"),
            PushOutcome::Transport(_) => "transport".to_string(),
            PushOutcome::UnexpectedStatus(_) => "unexpected_status".to_string(),
            PushOutcome::Dormant => "dormant".to_string(),
        }
    }
    fn is_ok(&self) -> bool {
        matches!(self, PushOutcome::Ok { .. })
    }
    fn pushed_count(&self) -> Option<i64> {
        match self {
            PushOutcome::Ok { count } => Some(*count),
            _ => None,
        }
    }
    /// Human-readable one-liner for the SPA `last_detail`. Carries the
    /// rejection body excerpt so the catalogue page shows *why* (e.g.
    /// "HTTP 400: display_name is required").
    fn detail(&self) -> Option<String> {
        match self {
            PushOutcome::Ok { .. } | PushOutcome::Unauthorized | PushOutcome::Dormant => None,
            PushOutcome::Transport(s) => Some(s.clone()),
            PushOutcome::Rejected { status, body } | PushOutcome::ServerError { status, body } => {
                if body.is_empty() {
                    Some(format!("HTTP {status}"))
                } else {
                    Some(format!("HTTP {status}: {body}"))
                }
            }
            PushOutcome::UnexpectedStatus(c) => Some(format!("HTTP {c}")),
        }
    }
    /// Structured HTTP status for the audit payload (`None` for
    /// transport/dormant, which never reached an HTTP response).
    fn http_status(&self) -> Option<u16> {
        match self {
            PushOutcome::Unauthorized => Some(401),
            PushOutcome::Rejected { status, .. }
            | PushOutcome::ServerError { status, .. }
            | PushOutcome::UnexpectedStatus(status) => Some(*status),
            PushOutcome::Ok { .. } | PushOutcome::Transport(_) | PushOutcome::Dormant => None,
        }
    }
    /// Scrubbed response-body excerpt for the audit payload (only the
    /// rejection classes carry one).
    fn response_excerpt(&self) -> Option<String> {
        match self {
            PushOutcome::Rejected { body, .. } | PushOutcome::ServerError { body, .. }
                if !body.is_empty() =>
            {
                Some(body.clone())
            }
            _ => None,
        }
    }
}

/// First `RESPONSE_EXCERPT_MAX` chars of a (bearer-scrubbed, trimmed)
/// response body. Bounds the audit payload + status struct so a verbose
/// HTML error page can't bloat the ledger.
const RESPONSE_EXCERPT_MAX: usize = 200;

// S347 / PR-39 — `pub(crate)` so the priced-writeback classifier
// (`quote_pricing_pipeline`) reuses the same bearer-scrubbed, 200-char
// bound rather than duplicating the helper (CLAUDE.md #8).
pub(crate) fn response_excerpt(body: &str) -> String {
    scrub(body.trim())
        .chars()
        .take(RESPONSE_EXCERPT_MAX)
        .collect()
}

/// Pure status classifier (S342 / PR-37). Maps a non-2xx HTTP code +
/// response-body excerpt to the right [`PushOutcome`]. Pure so it is
/// unit-testable without a mock server; `push_once` only adds the actual
/// network I/O (reading the body) around it.
///
/// - `401` → [`PushOutcome::Unauthorized`] (pauses the daemon; body dropped)
/// - other `4xx` → [`PushOutcome::Rejected`] (`rejected_<code>`)
/// - `5xx` → [`PushOutcome::ServerError`] (`transient_<code>`, retried)
/// - anything else (`1xx`/`3xx`) → [`PushOutcome::UnexpectedStatus`]
fn classify_status(status: u16, body: String) -> PushOutcome {
    match status {
        401 => PushOutcome::Unauthorized,
        400..=499 => PushOutcome::Rejected { status, body },
        500..=599 => PushOutcome::ServerError { status, body },
        _ => PushOutcome::UnexpectedStatus(status),
    }
}

// ── The wire body ───────────────────────────────────────────────────────

#[derive(Serialize)]
struct CatalogueBody {
    materials: Vec<crate::quoting_materials::PublicMaterial>,
}

// ── Service / daemon ─────────────────────────────────────────────────────

/// Dependencies for the audit write + table read (mirrors `QuoteIntakeDeps`).
pub struct CataloguePushDeps {
    pub db_path: PathBuf,
    /// ADR-0098 Session B (Gap 1a) — shared DuckDB handle.
    pub db: aberp_db::HandleArc,
    pub tenant: TenantId,
    pub binary_hash: BinaryHash,
    pub operator_login: String,
    /// S339 / PR-24 — optional storefront origin shared secret
    /// (`X-CloudFront-Secret`). Resolved ONCE at boot
    /// ([`crate::storefront_origin_secret::resolve`]). `None` (the
    /// default) ⇒ no header, pre-S339 behaviour. Deploy-infra credential,
    /// not operator-SPA-editable, so it does NOT live in the
    /// hot-reloadable [`StorefrontCredentialHandle`].
    pub origin_secret: Option<Zeroizing<String>>,
}

pub struct CataloguePushService {
    handle: Arc<CataloguePushHandle>,
    /// S289 / PR-270 — shared storefront credential. Read on every push
    /// so an operator URL/bearer change in Settings → Quote Intake takes
    /// effect on the next cycle (no restart needed).
    credential: Arc<StorefrontCredentialHandle>,
    cadence: Duration,
    client: reqwest::Client,
    deps: CataloguePushDeps,
}

impl CataloguePushService {
    /// `credential` is the same hot-reloadable handle that the boot
    /// resolver populates and the PUT `/api/quote-intake/config` route
    /// updates. The daemon snapshots it at the top of every cycle.
    pub fn new(
        handle: Arc<CataloguePushHandle>,
        credential: Arc<StorefrontCredentialHandle>,
        deps: CataloguePushDeps,
    ) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()
            .context("build catalogue-push reqwest client")?;
        Ok(Self {
            handle,
            credential,
            cadence: Duration::from_secs(PUSH_CADENCE_SECS),
            client,
            deps,
        })
    }

    /// The boot-spawned loop. 30s settle, then push on cadence OR when the
    /// operator triggers a write. Backoff on transient failure; 401 marks
    /// the handle paused.
    ///
    /// ## S289 / PR-270 — no-exit on 401
    ///
    /// Pre-S289 a 401 returned (killing the daemon for the rest of the
    /// process) on the theory "don't hammer a rotated bearer until the
    /// operator restarts". With hot-reload that decision is wrong: the
    /// operator's re-paste-bearer in Settings → Quote Intake calls
    /// `handle.set(new_url, new_bearer)`, and the very next cycle picks
    /// it up — that's the whole point of S289. We therefore stay in the
    /// loop on 401 (one audit row per cadence = 4/hour at the 15-min
    /// cadence, well below spam threshold), set the sticky `paused` flag
    /// for the SPA banner, and let the next cycle clear it on success.
    pub async fn run_daemon_forever(self, cancel: CancellationToken) {
        self.handle.mark_running();
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(Duration::from_secs(BOOT_DELAY_SECS)) => {}
        }

        let mut backoff_idx: usize = 0;
        loop {
            if cancel.is_cancelled() {
                return;
            }
            let outcome = self.push_once("daemon").await;

            let sleep_dur = if outcome.is_ok() {
                backoff_idx = 0;
                // S289 — a successful cycle clears the sticky pause:
                // re-pasting the bearer was the fix.
                self.handle.clear_paused();
                self.cadence
            } else if matches!(outcome, PushOutcome::Unauthorized) {
                // Stay in the loop, but at cadence (NOT exponential) so
                // the operator's re-paste-bearer is picked up at most
                // ~15 min later without restart.
                tracing::error!(
                    "catalogue-push daemon: storefront returned 401 \
                     (bearer rotated/invalid). Re-paste the bearer token \
                     in Settings → Quote Intake — the next cycle will \
                     pick it up automatically (S289 hot-reload). Paused \
                     status banner is sticky until the next success."
                );
                self.cadence
            } else {
                let d = backoff_duration(backoff_idx, self.cadence);
                backoff_idx = backoff_idx.saturating_add(1);
                tracing::warn!(
                    backoff_secs = d.as_secs(),
                    outcome = outcome.label().as_str(),
                    "catalogue push failed; backing off"
                );
                d
            };

            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = self.handle.notify.notified() => {
                    // operator write — push immediately
                }
                _ = tokio::time::sleep(sleep_dur) => {}
            }
        }
    }

    /// One push attempt: snapshot the credential, read the public
    /// projection, PUT it, classify, audit, and record the status. Used
    /// by the daemon and (via the trigger) on operator write.
    pub async fn push_once(&self, trigger: &str) -> PushOutcome {
        let attempt_at = now_rfc3339();

        // S289 / PR-270 — re-resolve the storefront credential at the
        // top of every cycle so an SPA URL/bearer save takes effect on
        // the very next cycle (no restart). A dormant snapshot ⇒ the
        // storefront isn't configured (operator set enabled=false or
        // hasn't filled the form). Record a `dormant` outcome and back
        // off — the operator-visible status banner reads "dormant"
        // rather than an alarming network error.
        let credential = match self.credential.snapshot() {
            Some(c) => c,
            None => {
                let outcome = PushOutcome::Dormant;
                self.finish(trigger, attempt_at, outcome.clone()).await;
                return outcome;
            }
        };

        // Read the public catalogue off the DB (sync duckdb on a blocking
        // thread).
        let db = self.deps.db.clone();
        let tenant_str = self.deps.tenant.as_str().to_string();
        let rows = match tokio::task::spawn_blocking(move || {
            let conn = db
                .read()
                .context("shared read: catalogue push (ADR-0098 Gap 1a)")?;
            crate::quoting_materials::list_public(&conn, &tenant_str)
        })
        .await
        {
            Ok(Ok(rows)) => rows,
            Ok(Err(e)) => {
                let outcome = PushOutcome::Transport(format!("read catalogue: {e:#}"));
                self.finish(trigger, attempt_at, outcome.clone()).await;
                return outcome;
            }
            Err(join) => {
                let outcome = PushOutcome::Transport(format!("read task panicked: {join}"));
                self.finish(trigger, attempt_at, outcome.clone()).await;
                return outcome;
            }
        };

        let count = rows.len() as i64;
        let body = CatalogueBody { materials: rows };
        let url = format!("{}{CATALOGUE_PATH}", credential.base_url);
        let auth = format!("Bearer {}", &*credential.bearer);

        // S339 / PR-24 — satisfy the storefront's CloudFront→origin
        // shared-secret gate when provisioned. Additive: a `None`
        // secret sends exactly the pre-S339 headers.
        let mut request = self
            .client
            .put(&url)
            .header(AUTHORIZATION, auth)
            .header(CONTENT_TYPE, "application/json");
        if let Some(secret) = &self.deps.origin_secret {
            request = request.header(ORIGIN_SECRET_HEADER, secret.as_str());
        }

        let outcome = match request.json(&body).send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                if (200..300).contains(&status) {
                    PushOutcome::Ok { count }
                } else {
                    // S342 / PR-37 — read the body on any non-2xx so the
                    // operator sees the storefront's rejection reason
                    // (e.g. "display_name is required") in the audit row
                    // and Maintenance card without curl-debugging prod.
                    let resp_body = resp.text().await.unwrap_or_default();
                    classify_status(status, response_excerpt(&resp_body))
                }
            }
            Err(e) => PushOutcome::Transport(scrub(&e.to_string())),
        };

        self.finish(trigger, attempt_at, outcome.clone()).await;
        outcome
    }

    async fn finish(&self, trigger: &str, attempt_at: String, outcome: PushOutcome) {
        self.handle.record(attempt_at.clone(), &outcome);
        self.write_audit(trigger, &outcome).await;
    }

    async fn write_audit(&self, trigger: &str, outcome: &PushOutcome) {
        let db = self.deps.db.clone();
        let tenant = self.deps.tenant.clone();
        let binary_hash = self.deps.binary_hash;
        let login = self.deps.operator_login.clone();
        let trigger = trigger.to_string();
        let outcome = outcome.clone();

        let res = tokio::task::spawn_blocking(move || -> Result<()> {
            let mut conn = db
                .write()
                .context("shared writer: MaterialCataloguePushed audit (ADR-0098 Gap 1a)")?;
            aberp_audit_ledger::ensure_schema(&conn).context("ensure audit schema")?;
            let payload = serde_json::json!({
                "trigger": trigger,
                "outcome": outcome.label(),
                "pushed_count": outcome.pushed_count(),
                "detail": outcome.detail(),
                // S342 / PR-37 — structured rejection diagnostics so the
                // audit row carries the HTTP code + body excerpt, not just
                // an opaque "unexpected_status".
                "http_status": outcome.http_status(),
                "response_excerpt": outcome.response_excerpt(),
                "idempotency_key": Ulid::new().to_string(),
            });
            let bytes = serde_json::to_vec(&payload).context("serialize push payload")?;
            let tx = conn.transaction().context("begin push audit tx")?;
            let meta = LedgerMeta::new(tenant, binary_hash);
            let actor = Actor::from_local_cli(Ulid::new().to_string(), &login);
            append_in_tx(
                &tx,
                &meta,
                EventKind::MaterialCataloguePushed,
                bytes,
                actor,
                None,
            )
            .context("append MaterialCataloguePushed")?;
            tx.commit().context("commit push audit")?;
            Ok(())
        })
        .await;

        match res {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::error!(error = ?e, "catalogue-push audit write failed"),
            Err(join) => tracing::error!(%join, "catalogue-push audit task panicked"),
        }
    }
}

fn backoff_duration(idx: usize, cadence: Duration) -> Duration {
    match idx {
        0 => Duration::from_secs(5),
        1 => Duration::from_secs(15),
        2 => Duration::from_secs(60),
        _ => cadence,
    }
}

/// Strip any bearer token that might appear in a reqwest error string.
fn scrub(s: &str) -> String {
    let mut out = s.to_string();
    if let Some(pos) = out.find("Bearer ") {
        out.replace_range(pos.., "Bearer <redacted>");
    }
    out
}

fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_schedule_matches_quote_intake() {
        let cad = Duration::from_secs(PUSH_CADENCE_SECS);
        assert_eq!(backoff_duration(0, cad), Duration::from_secs(5));
        assert_eq!(backoff_duration(1, cad), Duration::from_secs(15));
        assert_eq!(backoff_duration(2, cad), Duration::from_secs(60));
        assert_eq!(backoff_duration(3, cad), cad);
        assert_eq!(backoff_duration(99, cad), cad);
    }

    #[test]
    fn outcome_labels_and_pause_flag() {
        assert_eq!(PushOutcome::Ok { count: 3 }.label(), "ok");
        assert_eq!(PushOutcome::Ok { count: 3 }.pushed_count(), Some(3));
        assert_eq!(PushOutcome::Unauthorized.label(), "unauthorized");
        assert_eq!(PushOutcome::Transport("dns".into()).label(), "transport");
        // S342 / PR-37 — the HTTP-status classes carry the code in the label.
        assert_eq!(
            PushOutcome::Rejected {
                status: 400,
                body: String::new()
            }
            .label(),
            "rejected_400"
        );
        assert_eq!(
            PushOutcome::ServerError {
                status: 503,
                body: String::new()
            }
            .label(),
            "transient_503"
        );
        // UnexpectedStatus is now reserved for genuinely-weird 1xx/3xx.
        assert_eq!(
            PushOutcome::UnexpectedStatus(302).label(),
            "unexpected_status"
        );
        assert_eq!(
            PushOutcome::UnexpectedStatus(302).detail(),
            Some("HTTP 302".to_string())
        );
        // S289 / PR-270 — dormant is a distinct, non-alarming outcome.
        assert_eq!(PushOutcome::Dormant.label(), "dormant");
        assert!(PushOutcome::Dormant.detail().is_none());
        assert!(PushOutcome::Dormant.pushed_count().is_none());
    }

    #[test]
    fn classify_status_maps_each_http_class() {
        // 401 stays the special pause case (body dropped).
        assert_eq!(
            classify_status(401, "ignored".to_string()),
            PushOutcome::Unauthorized
        );
        // Other 4xx → Rejected, carrying the code + body excerpt.
        assert_eq!(
            classify_status(400, "display_name is required".to_string()),
            PushOutcome::Rejected {
                status: 400,
                body: "display_name is required".to_string()
            }
        );
        assert_eq!(
            classify_status(403, "forbidden".to_string()),
            PushOutcome::Rejected {
                status: 403,
                body: "forbidden".to_string()
            }
        );
        // 5xx → ServerError (transient, will retry).
        assert_eq!(
            classify_status(503, "upstream down".to_string()),
            PushOutcome::ServerError {
                status: 503,
                body: "upstream down".to_string()
            }
        );
        // Genuinely weird (3xx) → UnexpectedStatus.
        assert_eq!(
            classify_status(302, String::new()),
            PushOutcome::UnexpectedStatus(302)
        );
    }

    #[test]
    fn rejected_outcome_surfaces_status_and_excerpt() {
        let o = PushOutcome::Rejected {
            status: 400,
            body: "materials[0]: display_name is required".to_string(),
        };
        assert_eq!(o.http_status(), Some(400));
        assert_eq!(
            o.response_excerpt(),
            Some("materials[0]: display_name is required".to_string())
        );
        assert_eq!(
            o.detail(),
            Some("HTTP 400: materials[0]: display_name is required".to_string())
        );
        // The transient (5xx) class carries the same structured fields.
        let s = PushOutcome::ServerError {
            status: 503,
            body: "upstream".to_string(),
        };
        assert_eq!(s.http_status(), Some(503));
        assert_eq!(s.response_excerpt(), Some("upstream".to_string()));
    }

    #[test]
    fn response_excerpt_scrubs_bearer_and_bounds_length() {
        let bearer = response_excerpt("leak Bearer abc.def.ghi");
        assert!(bearer.contains("Bearer <redacted>"));
        assert!(!bearer.contains("abc.def.ghi"));
        let long = "x".repeat(500);
        assert_eq!(
            response_excerpt(&long).chars().count(),
            RESPONSE_EXCERPT_MAX
        );
    }

    #[test]
    fn handle_records_outcome_and_sets_paused_on_401() {
        let h = CataloguePushHandle::dormant();
        assert!(!h.snapshot().running);
        h.mark_running();
        assert!(h.snapshot().running);

        h.record(
            "2026-06-06T00:00:00Z".to_string(),
            &PushOutcome::Ok { count: 5 },
        );
        let s = h.snapshot();
        assert_eq!(s.last_outcome.as_deref(), Some("ok"));
        assert_eq!(s.last_pushed_count, Some(5));
        assert!(!s.paused);

        h.record(
            "2026-06-06T00:15:00Z".to_string(),
            &PushOutcome::Unauthorized,
        );
        assert!(h.snapshot().paused, "401 must set the sticky paused flag");
    }

    #[test]
    fn clear_paused_clears_only_the_pause_flag() {
        let h = CataloguePushHandle::dormant();
        h.record(
            "2026-06-06T00:15:00Z".to_string(),
            &PushOutcome::Unauthorized,
        );
        assert!(h.snapshot().paused);
        h.clear_paused();
        let s = h.snapshot();
        assert!(!s.paused);
        // The last_outcome string is preserved — `clear_paused` is the
        // un-pause hook, not a full status wipe.
        assert_eq!(s.last_outcome.as_deref(), Some("unauthorized"));
    }

    #[test]
    fn scrub_redacts_bearer() {
        let s = scrub("error sending request with Bearer abc.def.ghi");
        assert!(s.contains("Bearer <redacted>"));
        assert!(!s.contains("abc.def.ghi"));
    }

    #[test]
    fn origin_secret_header_name_is_the_storefront_contract() {
        // Pin the header name verbatim — the storefront's
        // `hooks.server.ts` reads `x-cloudfront-secret` (case-insensitive
        // HTTP header match). A silent rename here re-opens the 403.
        assert_eq!(ORIGIN_SECRET_HEADER, "X-CloudFront-Secret");
    }
}
