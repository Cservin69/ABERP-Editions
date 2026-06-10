//! S307 / PR-276 — Email-outbox poll daemon (ADR-0009).
//!
//! Polls the storefront's `/api/internal/email-queue` endpoint, claims
//! each entry the daemon decides to handle, sends it via ABERP's
//! local SMTP (reusing the S281 lettre transport helpers from
//! [`crate::email_relay_daemon`] per [[aberp-smtp-spoc]]), then POSTs
//! `.../sent` or `.../failed` back to the storefront. Replaces the
//! deprecated push-based [`crate::email_relay`] endpoint that ADR-0007
//! shipped.
//!
//! ## Why polling
//!
//! ABERP runs on Ervin's MacBook behind a loopback-only listener; the
//! storefront is on Lightsail with a public TLS terminus. There is no
//! inbound path to ABERP without a third-party tunnel, and Ervin's
//! 2026-06-09 threat model rejects every vendor (`Cloudflare` /
//! `Tailscale`) and the recurring-maintenance cost of self-hosted
//! WireGuard ([[trust-code-not-operator]]). ADR-0009 §Decision: ABERP
//! polls outbound only.
//!
//! ## State machine
//!
//! The daemon does NOT keep its own persistent state — the storefront's
//! `queued/ → claimed/ → sent/ | failed/` directory layout IS the
//! state machine. ABERP's view per cycle:
//!
//! ```text
//!   GET /api/internal/email-queue?since=<iso>
//!     └─→ for each entry returned:
//!           POST .../claim                      (200 → ours; 409 → skip)
//!             └─→ compose lettre Message
//!                  └─→ transport.send()
//!                       ├─ Ok  → POST .../sent       + audit EmailOutboxSent
//!                       └─ Err → POST .../failed     + audit EmailOutboxFailed
//! ```
//!
//! ## Single-flight, not parallel
//!
//! Per the brief: process one entry at a time per cycle. The atomic-
//! rename on the storefront side already serialises concurrent claims;
//! we don't need ABERP-side parallelism on top. Throughput is bounded
//! by `MAX_RELAY_PER_MINUTE` (S281 = 30/min) AND by Ervin's customer
//! flow (single-digit quotes/day). Single-flight keeps the audit trail
//! linear and the recovery story trivial.
//!
//! ## Hot-reload of credential
//!
//! Reads the storefront base_url + bearer from the shared
//! [`StorefrontCredentialHandle`] (S289) at the top of every cycle. An
//! operator URL change in SPA → Settings → Quote Intake takes effect on
//! the very next cycle, no restart. Mirrors the catalogue-push pattern.
//!
//! ## Supervisor
//!
//! Wraps the inner loop in [`run_supervised`] (mirrors the S286 pricing-
//! pipeline supervisor): a Rust-side panic is caught, an audit row is
//! emitted (best-effort), and the daemon re-spawns after a 30s back-off
//! (escalates to 5min if 5+ panics fire inside a 10-minute window). The
//! supervisor cannot catch a C++-level `libc++abi` termination — defence
//! against that path lives in the storefront's atomic-rename semantics,
//! not here.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use duckdb::Connection;
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use ulid::Ulid;
use zeroize::Zeroizing;

use aberp_audit_ledger::{append_in_tx, Actor, BinaryHash, EventKind, LedgerMeta, TenantId};
use lettre::{
    message::{header::ContentType, Attachment, MultiPart, SinglePart},
    AsyncTransport, Message,
};

use crate::audit_payloads::{EmailOutboxEntryAuditPayload, EmailOutboxFetchedPayload};
use crate::email_relay::hash_recipient_list;
use crate::email_relay_daemon::{build_mailbox, build_transport, scrub_for_audit};
use crate::secrets_cache::SecretsCache;
use crate::smtp_config;
use crate::storefront_credential::{StorefrontCredentialHandle, StorefrontCredentialSnapshot};

/// Default poll cadence. Matches ADR-0009 §Decision (5s for email,
/// fast enough that the customer-facing acknowledgement email feels
/// instant).
pub const POLL_TICK_SECS_DEFAULT: u64 = 5;

/// Operator-facing kill switch. Set `ABERP_EMAIL_OUTBOX_POLL_DISABLED=1`
/// to suppress the daemon spawn at boot. Used for local-dev (when the
/// developer is hitting the deprecated push endpoint directly) AND for
/// emergency rollback (an operator can disable the daemon without a
/// downgrade if the polling surface regresses in some way).
pub const POLL_DISABLE_ENV: &str = "ABERP_EMAIL_OUTBOX_POLL_DISABLED";

/// Operator-facing cadence override (seconds). Defaults to
/// [`POLL_TICK_SECS_DEFAULT`] if unset / invalid. Clamped to
/// `[1, 3600]` so a typo can't degrade the daemon to a heartbeat or to
/// a hot-loop.
pub const POLL_INTERVAL_ENV: &str = "ABERP_EMAIL_OUTBOX_POLL_SECS";

/// HTTP timeout for storefront calls. Matches the S279 pricing-pipeline
/// client constant — same network surface, same bound.
const HTTP_TIMEOUT_SECS: u64 = 30;

/// S335 — idle-liveness heartbeat cadence.
///
/// Pre-S335 the daemon emitted one `EmailOutboxFetched` audit row on
/// **every** poll cycle, including idle (zero-row) cycles — ~17k rows/day
/// at the 5s cadence, by far the highest-frequency producer in the
/// tamper-evident ledger and the volume driver behind the DuckDB ART
/// crash Ervin saw (see `docs/findings/s335-email-outbox-throttle.md`).
///
/// S335 throttles idle cycles to silence (a `tracing::debug!` line
/// instead of a ledger row), but still emits **one** idle
/// `EmailOutboxFetched{fetched_count:0}` at most once per this interval
/// so an operator can still confirm "daemon alive and idle" from the
/// durable ledger without the flood. 5 minutes keeps the idle footprint
/// at ~288 rows/day (a ~98% cut) while staying well inside a
/// human-noticeable liveness window.
///
/// Conservative call (cadence is a judgement, not a derived value) —
/// flagged in the findings doc. NOT operator-overridable in v1; if a
/// future need arises, plumb it through `EmailOutboxPollDaemonDeps`
/// rather than an env knob (avoids a second hot-reload surface).
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Hard cap on `?limit=` we ask the storefront for. v1 ships no
/// pagination loop within a cycle; one page of up to 50 entries is
/// fine for the customer-flow volume Ervin sees. Storefront clamps to
/// 200 anyway (see `email-outbox.ts listQueued`).
const QUEUE_LIST_LIMIT: u32 = 50;

/// Rolling panic-window for the supervisor. Mirrors the S286 pricing-
/// pipeline supervisor's [`PANIC_WINDOW`].
const PANIC_WINDOW: Duration = Duration::from_secs(10 * 60);

/// Five panics inside [`PANIC_WINDOW`] tips the supervisor into the
/// long-backoff branch.
const PANIC_BURST_THRESHOLD: usize = 5;

const PANIC_SHORT_BACKOFF: Duration = Duration::from_secs(30);

const PANIC_LONG_BACKOFF: Duration = Duration::from_secs(5 * 60);

/// Wire-shape of one queue entry the storefront returns. Mirrors the
/// `EmailQueueEntry` interface in `email-outbox.ts` exactly — any field
/// the storefront serialises must round-trip serde.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmailOutboxEntry {
    pub id: String,
    pub queued_at: String,
    pub to: Vec<String>,
    #[serde(default)]
    pub cc: Vec<String>,
    pub subject: String,
    pub body_text: String,
    #[serde(default)]
    pub body_html: Option<String>,
    #[serde(default)]
    pub attachments: Option<Vec<EmailOutboxAttachment>>,
    pub submitter: String,
    pub state: String,
    #[serde(default)]
    pub attempt_n: u32,
    /// Storefront-side last_error (object with class+detail). The
    /// daemon doesn't act on this; preserved for the wire round-trip.
    #[serde(default)]
    pub last_error: Option<serde_json::Value>,
    #[serde(default)]
    pub sent_at: Option<String>,
    #[serde(default)]
    pub audit_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmailOutboxAttachment {
    pub filename: String,
    pub content_type: String,
    pub data_b64: String,
}

/// Wire-shape of the storefront's `GET /api/internal/email-queue`
/// response.
#[derive(Debug, Clone, Deserialize)]
struct EmailOutboxListResponse {
    entries: Vec<EmailOutboxEntry>,
}

/// Pluggable SMTP-send strategy. Default impl is [`LettreSender`]
/// which goes through the same lettre transport the S281 relay daemon
/// uses (one source of truth per [[aberp-smtp-spoc]]). The integration
/// test in `tests/email_outbox_poll_full_cycle.rs` injects a capture-
/// only fake so the cycle's audit / writeback paths can be exercised
/// without a real SMTP server.
#[async_trait::async_trait]
pub trait OutboxSender: Send + Sync {
    async fn send(&self, entry: &EmailOutboxEntry) -> Result<()>;
}

/// Default production sender. Reads `[seller.smtp]` from disk on each
/// send (the SMTP config is in seller.toml, not a hot-path; reading is
/// fine), composes the MIME message, and goes via the same
/// [`crate::email_relay_daemon`] transport builder.
pub struct LettreSender {
    pub seller_toml_path: std::path::PathBuf,
    pub secrets_cache: SecretsCache,
}

#[async_trait::async_trait]
impl OutboxSender for LettreSender {
    async fn send(&self, entry: &EmailOutboxEntry) -> Result<()> {
        send_via_smtp(&self.seller_toml_path, &self.secrets_cache, entry).await
    }
}

/// Deps threaded from `AppState`. `Clone` for `Arc<Self>`-style sharing
/// across the supervisor's re-spawn loop.
#[derive(Clone)]
pub struct EmailOutboxPollDaemonDeps {
    pub db_path: std::path::PathBuf,
    pub tenant: TenantId,
    pub binary_hash: BinaryHash,
    pub operator_login: String,
    pub storefront_credential: Arc<StorefrontCredentialHandle>,
    pub status: Arc<EmailOutboxDaemonHandle>,
    pub poll_interval: Duration,
    pub sender: Arc<dyn OutboxSender>,
}

/// Resolve the configured poll interval from the env. Falls back to
/// [`POLL_TICK_SECS_DEFAULT`] for any of: unset / blank / unparseable /
/// out-of-range (clamp to `[1, 3600]`). Public so the boot site can log
/// the resolved value.
pub fn resolve_poll_interval() -> Duration {
    let secs = std::env::var(POLL_INTERVAL_ENV)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&n| (1..=3600).contains(&n))
        .unwrap_or(POLL_TICK_SECS_DEFAULT);
    Duration::from_secs(secs)
}

/// Check the kill switch. Returns true iff the daemon should NOT be
/// spawned this boot.
pub fn is_disabled() -> bool {
    std::env::var(POLL_DISABLE_ENV)
        .ok()
        .map(|v| {
            let t = v.trim();
            t == "1" || t.eq_ignore_ascii_case("true")
        })
        .unwrap_or(false)
}

/// Operator-visible status surface. Mirrors the S282 pricing-pipeline
/// `PythonResolutionHandle` posture: a single `Mutex` around a
/// `Default`-derivable snapshot struct, read by the SPA status route.
#[derive(Debug, Default)]
pub struct EmailOutboxDaemonHandle {
    inner: Mutex<EmailOutboxDaemonStatus>,
    /// S335 — wall-clock of the last `EmailOutboxFetched` row this daemon
    /// emitted (real fetch, errored cycle, OR idle heartbeat). Drives the
    /// idle-heartbeat cadence in [`Self::heartbeat_due_and_stamp`]. Kept
    /// OUT of the serialized [`EmailOutboxDaemonStatus`] snapshot — it's
    /// internal cadence state, not an operator-facing field, and
    /// `OffsetDateTime` would bloat the SPA payload for no consumer.
    last_fetched_emit: Mutex<Option<OffsetDateTime>>,
}

impl EmailOutboxDaemonHandle {
    pub fn dormant() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Returns a snapshot the SPA / tests can consume.
    pub fn snapshot(&self) -> EmailOutboxDaemonStatus {
        self.inner.lock().map(|g| g.clone()).unwrap_or_default()
    }

    fn record_cycle(&self, cycle: CycleOutcome) {
        if let Ok(mut g) = self.inner.lock() {
            g.last_poll_ts = Some(cycle.cycle_at.clone());
            if let Some(c) = &cycle.last_seen_iso {
                g.last_seen_iso = Some(c.clone());
            }
            g.total_cycles_since_boot = g.total_cycles_since_boot.saturating_add(1);
            g.total_fetched_since_boot = g
                .total_fetched_since_boot
                .saturating_add(cycle.fetched_count as u64);
            g.total_sent_since_boot = g
                .total_sent_since_boot
                .saturating_add(cycle.sent_count as u64);
            g.total_failed_since_boot = g
                .total_failed_since_boot
                .saturating_add(cycle.failed_count as u64);
            g.entries_in_progress = 0;
            if let Some(err) = cycle.last_error_detail {
                g.last_error_ts = Some(cycle.cycle_at);
                g.last_error_detail = Some(err);
            }
        }
    }

    fn record_in_progress(&self, n: u32) {
        if let Ok(mut g) = self.inner.lock() {
            g.entries_in_progress = n;
        }
    }

    fn record_dormant_cycle(&self, cycle_at: String, reason: &str) {
        if let Ok(mut g) = self.inner.lock() {
            g.last_poll_ts = Some(cycle_at.clone());
            g.last_error_ts = Some(cycle_at);
            g.last_error_detail = Some(format!("dormant: {reason}"));
            g.entries_in_progress = 0;
        }
    }

    /// S335 — decide whether an idle-liveness heartbeat `EmailOutboxFetched`
    /// is due, stamping the emit clock when it returns `true`. Returns
    /// `true` at most once per `interval`. `now` is passed in (not read
    /// from the clock here) so the cadence is deterministically testable.
    fn heartbeat_due_and_stamp(&self, now: OffsetDateTime, interval: Duration) -> bool {
        if let Ok(mut g) = self.last_fetched_emit.lock() {
            let due = heartbeat_due(*g, now, interval);
            if due {
                *g = Some(now);
            }
            due
        } else {
            false
        }
    }

    /// S335 — record that an `EmailOutboxFetched` row was just emitted on a
    /// non-heartbeat path (a real fetch or an errored cycle). Resets the
    /// idle-heartbeat clock so a busy or continuously-erroring daemon — both
    /// of which already write rows that prove liveness — does not also emit
    /// a redundant idle heartbeat.
    fn stamp_fetched_emit(&self, now: OffsetDateTime) {
        if let Ok(mut g) = self.last_fetched_emit.lock() {
            *g = Some(now);
        }
    }

    fn record_panic(&self, panic_msg: &str, ts: String) {
        if let Ok(mut g) = self.inner.lock() {
            g.recent_panic_count = g.recent_panic_count.saturating_add(1);
            g.last_panic_ts = Some(ts);
            g.last_panic_msg = Some(panic_msg.to_string());
        }
    }

    fn record_spawned(&self, poll_interval: Duration) {
        if let Ok(mut g) = self.inner.lock() {
            g.spawned = true;
            g.poll_interval_secs = poll_interval.as_secs();
        }
    }

    /// Operator-facing reset of the panic counter — exposed for the
    /// integration tests but not wired to the SPA in v1.
    pub fn reset_panic_count(&self) {
        if let Ok(mut g) = self.inner.lock() {
            g.recent_panic_count = 0;
            g.last_panic_ts = None;
            g.last_panic_msg = None;
        }
    }

    /// Test seam — drop the `last_seen_iso` cursor so the next cycle
    /// behaves as if booted fresh. The S311 stale-recovery integration
    /// test uses this between cycles so the recovered wedge is
    /// re-fetched even though its `queued_at` predates cycle 1's
    /// cursor (in production a real recovered entry would carry its
    /// original `queued_at`, so this seam pins test-only state, not a
    /// runtime knob). NOT exposed on the SPA or on any HTTP route.
    pub fn reset_last_seen_for_test(&self) {
        if let Ok(mut g) = self.inner.lock() {
            g.last_seen_iso = None;
        }
    }
}

/// Snapshot the SPA + tests consume. `Serialize` so the status route
/// can return it verbatim.
#[derive(Debug, Default, Clone, Serialize)]
pub struct EmailOutboxDaemonStatus {
    /// True iff the daemon was spawned (kill switch + dev-mode both
    /// off, credential resolved at boot or hot-reloaded since). Lets
    /// the SPA distinguish "polling, nothing in queue" (GREEN) from
    /// "daemon disabled by operator" (AMBER).
    pub spawned: bool,
    /// Cadence the daemon is using this boot.
    pub poll_interval_secs: u64,
    /// ISO-8601 UTC of the last completed cycle (success OR error path).
    /// `None` until the first cycle completes.
    pub last_poll_ts: Option<String>,
    /// The `?since=<iso>` cursor the daemon currently sends. `None`
    /// before the first non-empty fetch.
    pub last_seen_iso: Option<String>,
    /// Number of entries the daemon is currently mid-claim/send on.
    /// Single-flight per cycle so this is 0 or 1; the field stays for
    /// forward-compat if a future cycle becomes parallel.
    pub entries_in_progress: u32,
    /// Lifetime cycle counter since boot.
    pub total_cycles_since_boot: u64,
    /// Lifetime entries-fetched counter since boot.
    pub total_fetched_since_boot: u64,
    /// Lifetime entries-sent counter since boot.
    pub total_sent_since_boot: u64,
    /// Lifetime entries-failed counter since boot.
    pub total_failed_since_boot: u64,
    /// ISO-8601 UTC of the last error (HTTP / SMTP / writeback).
    pub last_error_ts: Option<String>,
    /// Scrubbed-of-secrets last error string (≤ 1000 chars).
    pub last_error_detail: Option<String>,
    /// Supervisor-counted Rust-side panics (per [[s286-supervisor]]).
    pub recent_panic_count: u32,
    pub last_panic_ts: Option<String>,
    pub last_panic_msg: Option<String>,
}

/// What one cycle returned. Used to update the handle status after
/// the cycle completes (success or error).
#[derive(Debug, Default, Clone)]
struct CycleOutcome {
    cycle_at: String,
    fetched_count: u32,
    sent_count: u32,
    failed_count: u32,
    last_seen_iso: Option<String>,
    last_error_detail: Option<String>,
}

/// Entry point per [[post-issue-async]] — never blocks on SMTP.
///
/// Supervised wrapper. The inner loop is spawned, its `JoinHandle`
/// awaited; on a Rust-side panic the supervisor:
/// 1. Records the panic in the status handle.
/// 2. Best-effort emits a `quote.pricing_daemon_panicked`-shaped audit
///    row (we reuse that EventKind since the outbox daemon doesn't have
///    its own panic kind — the SPA panel surfaces the panic regardless).
///    Actually, no — see footnote: the brief did not request a panic
///    EventKind. We log + status-record only.
/// 3. Sleeps 30s (or 5min if 5+ panics in the last 10min).
/// 4. Re-spawns.
///
/// Sibling shape to [`crate::quote_pricing_pipeline::PricingPipelineService::run_daemon_supervised`].
pub async fn run_supervised(deps: EmailOutboxPollDaemonDeps, cancel: CancellationToken) {
    let status_handle = deps.status.clone();
    status_handle.record_spawned(deps.poll_interval);
    tracing::info!(
        poll_interval_secs = deps.poll_interval.as_secs(),
        "email-outbox poll daemon spawned (S307 / PR-276)"
    );
    let deps = Arc::new(deps);
    let mut panic_window: std::collections::VecDeque<Instant> = std::collections::VecDeque::new();
    loop {
        if cancel.is_cancelled() {
            return;
        }
        let d = deps.clone();
        let inner_cancel = cancel.clone();
        let handle = tokio::spawn(async move {
            run_loop(d, inner_cancel).await;
        });
        match handle.await {
            Ok(()) => return,
            Err(join_err) if join_err.is_cancelled() => return,
            Err(join_err) => {
                let panic_msg = if join_err.is_panic() {
                    panic_payload_to_string(join_err.into_panic())
                } else {
                    format!("non-panic JoinError: {join_err}")
                };
                let ts = iso8601_now();
                status_handle.record_panic(&panic_msg, ts);
                tracing::error!(
                    panic_msg = %panic_msg,
                    "email-outbox poll daemon panicked; supervisor recovering"
                );
                let now = Instant::now();
                while let Some(front) = panic_window.front().copied() {
                    if now.duration_since(front) > PANIC_WINDOW {
                        panic_window.pop_front();
                    } else {
                        break;
                    }
                }
                panic_window.push_back(now);
                let sleep_dur = if panic_window.len() >= PANIC_BURST_THRESHOLD {
                    PANIC_LONG_BACKOFF
                } else {
                    PANIC_SHORT_BACKOFF
                };
                tracing::warn!(
                    sleep_secs = sleep_dur.as_secs(),
                    recent_panic_count = panic_window.len(),
                    "email-outbox supervisor sleeping before restart"
                );
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep(sleep_dur) => {}
                }
            }
        }
    }
}

/// Inner loop. Polls every `deps.poll_interval`; cancellation honoured
/// via `tokio::select`.
async fn run_loop(deps: Arc<EmailOutboxPollDaemonDeps>, cancel: CancellationToken) {
    let cadence = deps.poll_interval;
    // Match the S279 boot delay so other ABERP boot daemons settle
    // before we start hitting the storefront.
    tokio::select! {
        _ = cancel.cancelled() => return,
        _ = tokio::time::sleep(Duration::from_secs(30)) => {}
    }
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "failed to build email-outbox HTTP client");
            return;
        }
    };
    loop {
        if cancel.is_cancelled() {
            return;
        }
        poll_once(&deps, &client).await;
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(cadence) => {}
        }
    }
}

/// One cycle. Public so the integration test can drive cycles
/// deterministically without sleeping. Errors are caught and recorded
/// on the status handle; this function does not propagate them.
pub async fn poll_once(deps: &EmailOutboxPollDaemonDeps, client: &reqwest::Client) {
    // S335 — capture the cycle clock ONCE: reused for the audit `cycle_at`
    // string AND the idle-heartbeat cadence decision below.
    let now_dt = OffsetDateTime::now_utc();
    let cycle_at = iso8601(now_dt);
    let snap = match deps.storefront_credential.snapshot() {
        Some(s) => s,
        None => {
            deps.status
                .record_dormant_cycle(cycle_at, "storefront credential not configured");
            return;
        }
    };
    let since = deps.status.snapshot().last_seen_iso.clone();
    let entries = match fetch_queue(client, &snap, since.as_deref()).await {
        Ok(es) => es,
        Err(e) => {
            let detail = scrub_for_audit(&format!("storefront list: {e:#}"));
            let error_class = classify_fetch_error(&e);
            // S311 / F13 + F18 — emit an audit row on errored cycles too.
            // Pre-S311 only success cycles fired EmailOutboxFetched, which
            // left silent-401 token-rotation gaps invisible in the audit
            // ledger (SPA `last_error_detail` is volatile; the ledger is
            // durable). Now: every cycle attempt fires the same EventKind
            // with `fetched_count: 0` and the error fields populated.
            tracing::warn!(error = %detail, error_class, "email-outbox fetch failed");
            // Errored cycles ALWAYS emit (S311 F13/F18 — the silent-401
            // observability path is the whole point of erroring loudly into
            // the ledger). S335 — stamp the heartbeat clock so this row
            // counts as liveness and we don't also emit a redundant idle
            // heartbeat on the next quiet cycle.
            deps.status.stamp_fetched_emit(now_dt);
            emit_fetched_error_audit(deps, since.clone(), &cycle_at, error_class, &detail).await;
            let mut outcome = CycleOutcome {
                cycle_at,
                ..Default::default()
            };
            outcome.last_error_detail = Some(detail);
            deps.status.record_cycle(outcome);
            return;
        }
    };
    let fetched_count = entries.len() as u32;
    let max_queued_at: Option<String> = entries.iter().map(|e| e.queued_at.clone()).max();
    // S335 — throttle idle (zero-row) emits. Pre-S335 this `emit_fetched_audit`
    // fired on every cycle including idle ones — ~17k rows/day into the
    // monotonic-`seq` ART, the volume driver behind the DuckDB ART crash
    // (docs/findings/s335-email-outbox-throttle.md). Now:
    //   * a real batch (`fetched_count > 0`) always emits — full work
    //     observability is unchanged;
    //   * an idle cycle emits at most one liveness heartbeat per
    //     `HEARTBEAT_INTERVAL` and is otherwise a `tracing::debug!` line.
    // The audit event-schema and wire format are unchanged — only the
    // emit FREQUENCY drops (~98% on the idle path).
    if fetched_count > 0 {
        deps.status.stamp_fetched_emit(now_dt);
        emit_fetched_audit(deps, fetched_count, since.clone(), &cycle_at).await;
    } else if deps
        .status
        .heartbeat_due_and_stamp(now_dt, HEARTBEAT_INTERVAL)
    {
        emit_fetched_audit(deps, 0, since.clone(), &cycle_at).await;
    } else {
        tracing::debug!(
            since = ?since,
            "email-outbox idle cycle (0 fetched); EmailOutboxFetched emit throttled (S335)"
        );
    }

    let mut sent_count: u32 = 0;
    let mut failed_count: u32 = 0;
    let mut last_error: Option<String> = None;

    for entry in entries {
        match handle_one_entry(deps, client, &snap, &entry).await {
            Ok(EntryOutcome::Sent) => sent_count = sent_count.saturating_add(1),
            Ok(EntryOutcome::SmtpFailed) => failed_count = failed_count.saturating_add(1),
            Ok(EntryOutcome::AlreadyClaimed) => {}
            Ok(EntryOutcome::WritebackSentFailed) | Ok(EntryOutcome::WritebackFailedErrored) => {
                last_error = Some(format!("entry {}: storefront writeback failed", entry.id));
            }
            Err(e) => {
                last_error = Some(scrub_for_audit(&format!("entry {}: {e:#}", entry.id)));
            }
        }
    }

    let outcome = CycleOutcome {
        cycle_at,
        fetched_count,
        sent_count,
        failed_count,
        last_seen_iso: max_queued_at.or(since),
        last_error_detail: last_error,
    };
    deps.status.record_cycle(outcome);
}

/// What `handle_one_entry` returned. Used by the cycle loop to maintain
/// per-cycle sent/failed counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryOutcome {
    /// Storefront returned `409` — already claimed by someone else (or
    /// us on a prior cycle); skip.
    AlreadyClaimed,
    /// SMTP + writeback `.../sent` both succeeded.
    Sent,
    /// SMTP failed; writeback `.../failed` succeeded.
    SmtpFailed,
    /// SMTP succeeded BUT writeback `.../sent` failed — the row stays in
    /// `claimed/` storefront-side; we'll retry the writeback next cycle.
    /// NOT a terminal outcome but the audit `EmailOutboxSent` does NOT
    /// fire (per ADR-0009: writeback is the terminal signal).
    WritebackSentFailed,
    /// Writeback `.../failed` itself errored after SMTP failed — same
    /// shape as the prior; no terminal audit fires.
    WritebackFailedErrored,
}

async fn handle_one_entry(
    deps: &EmailOutboxPollDaemonDeps,
    client: &reqwest::Client,
    snap: &StorefrontCredentialSnapshot,
    entry: &EmailOutboxEntry,
) -> Result<EntryOutcome> {
    deps.status.record_in_progress(1);
    let outcome = handle_one_entry_inner(deps, client, snap, entry).await;
    deps.status.record_in_progress(0);
    outcome
}

async fn handle_one_entry_inner(
    deps: &EmailOutboxPollDaemonDeps,
    client: &reqwest::Client,
    snap: &StorefrontCredentialSnapshot,
    entry: &EmailOutboxEntry,
) -> Result<EntryOutcome> {
    // Claim — atomic queued → claimed on the storefront side.
    let claimed = claim(client, snap, &entry.id).await?;
    if !claimed {
        // 409 → another claimer won; skip silently. No audit row
        // (only successful claims are durable).
        return Ok(EntryOutcome::AlreadyClaimed);
    }
    let recipients = recipients_combined(entry);
    let recipient_hash = hash_recipient_list(&recipients);
    let byte_size = entry_byte_size(entry);
    emit_claimed_audit(deps, entry, &recipient_hash, byte_size).await;

    let send_outcome = deps.sender.send(entry).await;
    let audit_id = Ulid::new().to_string();
    match send_outcome {
        Ok(()) => match mark_sent(client, snap, &entry.id, &audit_id).await {
            Ok(()) => {
                emit_sent_audit(deps, entry, &recipient_hash, byte_size, &audit_id).await;
                Ok(EntryOutcome::Sent)
            }
            Err(e) => {
                let detail = scrub_for_audit(&format!("writeback /sent: {e:#}"));
                tracing::warn!(
                    entry_id = %entry.id,
                    error = %detail,
                    "email-outbox SMTP succeeded but writeback /sent failed; storefront stale-claim sweep will recover after CLAIM_TTL"
                );
                // NO terminal audit fires. The storefront's S311 / PR-12
                // stale-claim sweep (`recoverStaleClaimed` inside
                // `listQueued`, ABERP-site/src/lib/server/email-outbox.ts)
                // atomically renames any claimed/<id>.json whose
                // claimed_at is older than CLAIM_TTL (default 600s) back
                // to queued/, so a daemon cycle past that TTL window
                // sees the entry as queued and re-claims+re-sends.
                // Duplicate-send risk is acceptable per ADR-0009
                // Consequences §3 — the storefront sweep is the single
                // recovery surface, not an operator runbook step
                // (closes S309 F1).
                Ok(EntryOutcome::WritebackSentFailed)
            }
        },
        Err(send_err) => {
            let error_detail = scrub_for_audit(&send_err.to_string());
            let error_class = classify_send_error(&send_err);
            match mark_failed(client, snap, &entry.id, error_class, &error_detail).await {
                Ok(()) => {
                    emit_failed_audit(
                        deps,
                        entry,
                        &recipient_hash,
                        byte_size,
                        error_class,
                        &error_detail,
                    )
                    .await;
                    Ok(EntryOutcome::SmtpFailed)
                }
                Err(wb_err) => {
                    let wb_detail = scrub_for_audit(&format!("writeback /failed: {wb_err:#}"));
                    tracing::warn!(
                        entry_id = %entry.id,
                        smtp_error = %error_detail,
                        writeback_error = %wb_detail,
                        "email-outbox SMTP failed AND writeback /failed errored; storefront stale-claim sweep will recover after CLAIM_TTL"
                    );
                    // Same recovery surface as the WritebackSentFailed
                    // arm above — storefront sweep auto-recovers the
                    // wedged claim past CLAIM_TTL (S311 F1). NO terminal
                    // audit fires; the next cycle past TTL gets a clean
                    // re-claim and either reaches a terminal Sent or
                    // a terminal Failed with the same `error_class`.
                    Ok(EntryOutcome::WritebackFailedErrored)
                }
            }
        }
    }
}

fn recipients_combined(entry: &EmailOutboxEntry) -> Vec<String> {
    let mut v: Vec<String> = Vec::with_capacity(entry.to.len() + entry.cc.len());
    v.extend(entry.to.iter().cloned());
    v.extend(entry.cc.iter().cloned());
    v
}

fn entry_byte_size(entry: &EmailOutboxEntry) -> u64 {
    let text = entry.body_text.len() as u64;
    let html = entry
        .body_html
        .as_ref()
        .map(|s| s.len() as u64)
        .unwrap_or(0);
    let atts: u64 = entry
        .attachments
        .as_ref()
        .map(|a| {
            a.iter()
                .map(|att| {
                    // base64 → ~3/4 the encoded length; approximation
                    // is fine for the audit byte_size field.
                    (att.data_b64.len() as u64) * 3 / 4
                })
                .sum()
        })
        .unwrap_or(0);
    text.saturating_add(html).saturating_add(atts)
}

fn classify_send_error(e: &anyhow::Error) -> &'static str {
    let s = e.to_string();
    if s.starts_with("compose:") {
        "compose"
    } else if s.starts_with("writeback:") {
        "writeback"
    } else {
        "smtp_transport"
    }
}

/// S311 / F18 — classify the GET error so the audit row's `error_class`
/// names the failure family the operator must act on. The detection is
/// substring-based because `fetch_queue` wraps the underlying reqwest
/// error in `anyhow::Context` and formats with `:#}` (full cause chain);
/// the storefront's `HTTP 401:` prefix is unambiguous.
fn classify_fetch_error(e: &anyhow::Error) -> &'static str {
    let s = e.to_string();
    if s.contains("HTTP 401") {
        // Most likely: ABERP_SITE_ADMIN_TOKEN rotated storefront-side
        // without updating the keychain entry that feeds the SPOC. The
        // operator sees a cycle-by-cycle audit row instead of silence.
        "auth_failed"
    } else if s.contains("decode response") {
        "decode"
    } else if s.contains("HTTP ") {
        // Any other non-2xx (403/404/5xx).
        "other"
    } else {
        // Network / DNS / TLS / timeout — reqwest's underlying error.
        "network"
    }
}

// ── HTTP shape ────────────────────────────────────────────────────────

async fn fetch_queue(
    client: &reqwest::Client,
    snap: &StorefrontCredentialSnapshot,
    since: Option<&str>,
) -> Result<Vec<EmailOutboxEntry>> {
    let base = snap.base_url.trim_end_matches('/');
    let mut url = format!("{base}/api/internal/email-queue?limit={QUEUE_LIST_LIMIT}");
    if let Some(s) = since {
        if !s.is_empty() {
            // URL-encode the ISO timestamp's `:` chars — reqwest handles
            // the rest. We just append.
            let encoded: String = url_encode(s);
            url.push_str("&since=");
            url.push_str(&encoded);
        }
    }
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", snap.bearer.as_str()))
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("storefront GET email-queue: HTTP {status}: {body}"));
    }
    let parsed: EmailOutboxListResponse = resp
        .json()
        .await
        .with_context(|| format!("decode response from {url}"))?;
    Ok(parsed.entries)
}

async fn claim(
    client: &reqwest::Client,
    snap: &StorefrontCredentialSnapshot,
    id: &str,
) -> Result<bool> {
    let base = snap.base_url.trim_end_matches('/');
    let url = format!("{base}/api/internal/email-queue/{id}/claim");
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", snap.bearer.as_str()))
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    if status.is_success() {
        Ok(true)
    } else if status.as_u16() == 409 {
        // already claimed — not our row this cycle
        Ok(false)
    } else if status.as_u16() == 404 {
        // entry vanished between GET and claim — race against
        // operator delete or test reset; treat as already-claimed
        Ok(false)
    } else {
        let body = resp.text().await.unwrap_or_default();
        Err(anyhow!("claim: HTTP {status}: {body}"))
    }
}

async fn mark_sent(
    client: &reqwest::Client,
    snap: &StorefrontCredentialSnapshot,
    id: &str,
    audit_id: &str,
) -> Result<()> {
    let base = snap.base_url.trim_end_matches('/');
    let url = format!("{base}/api/internal/email-queue/{id}/sent");
    let body = serde_json::json!({ "audit_id": audit_id });
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", snap.bearer.as_str()))
        .header("Content-Type", "application/json")
        .body(serde_json::to_vec(&body)?)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    if status.is_success() {
        Ok(())
    } else {
        let body = resp.text().await.unwrap_or_default();
        Err(anyhow!("mark_sent: HTTP {status}: {body}"))
    }
}

async fn mark_failed(
    client: &reqwest::Client,
    snap: &StorefrontCredentialSnapshot,
    id: &str,
    error_class: &str,
    error_detail: &str,
) -> Result<()> {
    let base = snap.base_url.trim_end_matches('/');
    let url = format!("{base}/api/internal/email-queue/{id}/failed");
    let body = serde_json::json!({
        "error_class": error_class,
        "error_detail": error_detail,
    });
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", snap.bearer.as_str()))
        .header("Content-Type", "application/json")
        .body(serde_json::to_vec(&body)?)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    if status.is_success() {
        Ok(())
    } else {
        let body = resp.text().await.unwrap_or_default();
        Err(anyhow!("mark_failed: HTTP {status}: {body}"))
    }
}

// ── SMTP send ─────────────────────────────────────────────────────────

async fn send_via_smtp(
    seller_toml_path: &std::path::Path,
    secrets_cache: &SecretsCache,
    entry: &EmailOutboxEntry,
) -> Result<()> {
    use base64::{engine::general_purpose::STANDARD as B64, Engine as _};

    let cfg = smtp_config::read_smtp_config(seller_toml_path)
        .context("compose: read [seller.smtp]")?
        .ok_or_else(|| {
            anyhow!(
                "compose: no [seller.smtp] section in {}",
                seller_toml_path.display()
            )
        })?;
    let password: Zeroizing<String> = secrets_cache
        .smtp_password()
        .ok_or_else(|| anyhow!("compose: SMTP password not in secrets cache"))?;

    let from_mbox = build_mailbox(&cfg.from_address, cfg.from_display_name.as_deref(), "from")
        .map_err(|e| anyhow!("compose: {e}"))?;

    let mut multipart = MultiPart::mixed().singlepart(
        SinglePart::builder()
            .header(ContentType::TEXT_PLAIN)
            .body(entry.body_text.clone()),
    );
    if let Some(html) = &entry.body_html {
        multipart = multipart.singlepart(
            SinglePart::builder()
                .header(ContentType::TEXT_HTML)
                .body(html.clone()),
        );
    }
    if let Some(atts) = &entry.attachments {
        for att in atts {
            let bytes = B64
                .decode(att.data_b64.as_bytes())
                .map_err(|e| anyhow!("compose: attachment {} base64: {e}", att.filename))?;
            let ct = ContentType::parse(&att.content_type)
                .unwrap_or_else(|_| ContentType::parse("application/octet-stream").unwrap());
            let part = Attachment::new(att.filename.clone()).body(bytes, ct);
            multipart = multipart.singlepart(part);
        }
    }

    let mut builder = Message::builder()
        .from(from_mbox)
        .subject(entry.subject.clone());
    for addr in &entry.to {
        let mbox = build_mailbox(addr, None, "to").map_err(|e| anyhow!("compose: {e}"))?;
        builder = builder.to(mbox);
    }
    for addr in &entry.cc {
        let mbox = build_mailbox(addr, None, "cc").map_err(|e| anyhow!("compose: {e}"))?;
        builder = builder.cc(mbox);
    }
    let message = builder
        .multipart(multipart)
        .map_err(|e| anyhow!("compose: build MIME: {e}"))?;

    let transport = build_transport(&cfg, &password).map_err(|e| anyhow!("compose: {e}"))?;
    transport
        .send(message)
        .await
        .map_err(|e| anyhow!("SMTP transport: {e}"))?;
    Ok(())
}

// ── Audit emit ────────────────────────────────────────────────────────

async fn emit_fetched_audit(
    deps: &EmailOutboxPollDaemonDeps,
    fetched_count: u32,
    since_cursor: Option<String>,
    cycle_at: &str,
) {
    let payload = EmailOutboxFetchedPayload::new(fetched_count, since_cursor, cycle_at);
    write_audit(deps, EventKind::EmailOutboxFetched, payload.to_bytes()).await;
}

/// S311 / F13 + F18 — emit `EmailOutboxFetched` on an errored cycle with
/// `fetched_count: 0` and the error fields populated. Closes the silent-
/// 401 gap (operator rotating `ABERP_SITE_ADMIN_TOKEN` storefront-side
/// without updating the ABERP keychain entry used to see zero ledger
/// rows during the gap — indistinguishable from "daemon was never
/// spawned" or "no quotes in flight"). Now the gap shows up as a row
/// every cycle with `error_class: "auth_failed"`.
async fn emit_fetched_error_audit(
    deps: &EmailOutboxPollDaemonDeps,
    since_cursor: Option<String>,
    cycle_at: &str,
    error_class: &str,
    error_detail: &str,
) {
    let payload =
        EmailOutboxFetchedPayload::errored(since_cursor, cycle_at, error_class, error_detail);
    write_audit(deps, EventKind::EmailOutboxFetched, payload.to_bytes()).await;
}

async fn emit_claimed_audit(
    deps: &EmailOutboxPollDaemonDeps,
    entry: &EmailOutboxEntry,
    recipient_hash: &str,
    byte_size: u64,
) {
    let p = EmailOutboxEntryAuditPayload::claimed(
        &entry.submitter,
        &entry.id,
        recipient_hash,
        &entry.subject,
        byte_size,
    );
    write_audit(deps, EventKind::EmailOutboxClaimed, p.to_bytes()).await;
}

async fn emit_sent_audit(
    deps: &EmailOutboxPollDaemonDeps,
    entry: &EmailOutboxEntry,
    recipient_hash: &str,
    byte_size: u64,
    _audit_id: &str,
) {
    let p = EmailOutboxEntryAuditPayload::sent(
        &entry.submitter,
        &entry.id,
        recipient_hash,
        &entry.subject,
        byte_size,
        1,
    );
    write_audit(deps, EventKind::EmailOutboxSent, p.to_bytes()).await;
}

async fn emit_failed_audit(
    deps: &EmailOutboxPollDaemonDeps,
    entry: &EmailOutboxEntry,
    recipient_hash: &str,
    byte_size: u64,
    error_class: &str,
    error_detail: &str,
) {
    let p = EmailOutboxEntryAuditPayload::failed(
        &entry.submitter,
        &entry.id,
        recipient_hash,
        &entry.subject,
        byte_size,
        1,
        error_class,
        error_detail,
    );
    write_audit(deps, EventKind::EmailOutboxFailed, p.to_bytes()).await;
}

async fn write_audit(deps: &EmailOutboxPollDaemonDeps, kind: EventKind, bytes: Vec<u8>) {
    // S335 — this DELIBERATELY opens a fresh `Connection` per write rather
    // than holding a persistent one. DuckDB `Connection::open` creates an
    // independent `Database` instance with no shared buffer cache across
    // handles (incoming_invoices.rs:54-74). A persistent connection does
    // NOT observe rows another daemon's connection committed, so on its
    // next append it reads a STALE chain head, recomputes a `seq` already
    // taken, and FORKS the tamper-evident hash chain (silently losing a
    // row) — proven by the S335 coherence probe and pinned by
    // `tests/s335_email_outbox_audit_write_coherence.rs`. Reopen-per-write
    // is the coherence mechanism EVERY ABERP daemon relies on: each fresh
    // open reads current disk state and computes the correct next seq.
    //
    // The ART-pressure fix is therefore to write LESS OFTEN (the S335 idle
    // throttle in `poll_once`), NOT to persist the connection. Do not
    // "optimize" this into a held connection without first making ALL
    // audit writers share a single serialized connection.
    let db_path = deps.db_path.clone();
    let tenant = deps.tenant.clone();
    let binary_hash = deps.binary_hash;
    let login = deps.operator_login.clone();
    let kind_label = kind.as_str();
    let res = tokio::task::spawn_blocking(move || -> Result<()> {
        let mut conn = Connection::open(&db_path).context("open DB for email-outbox audit")?;
        aberp_audit_ledger::ensure_schema(&conn).context("ensure audit schema")?;
        let tx = conn.transaction().context("begin email-outbox audit tx")?;
        let meta = LedgerMeta::new(tenant, binary_hash);
        let actor = Actor::from_local_cli(Ulid::new().to_string(), &login);
        append_in_tx(&tx, &meta, kind, bytes, actor, None).context("append email-outbox audit")?;
        tx.commit().context("commit email-outbox audit")?;
        Ok(())
    })
    .await;
    match res {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            tracing::error!(error = ?e, kind = %kind_label, "email-outbox audit write failed")
        }
        Err(join) => tracing::error!(%join, kind = %kind_label, "email-outbox audit task panicked"),
    }
}

// ── small helpers ─────────────────────────────────────────────────────

fn iso8601_now() -> String {
    iso8601(OffsetDateTime::now_utc())
}

/// Format an [`OffsetDateTime`] as RFC-3339, with the same infallible
/// fallback `iso8601_now` has always used. Split out (S335) so `poll_once`
/// can capture the cycle's `OffsetDateTime` ONCE — both for the audit
/// `cycle_at` string and for the idle-heartbeat cadence decision — without
/// re-reading the clock or re-parsing the formatted string.
fn iso8601(dt: OffsetDateTime) -> String {
    dt.format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

/// S335 — is an idle-liveness heartbeat `EmailOutboxFetched` due?
///
/// Pure (no clock, no I/O) so the cadence is unit-testable. `None` — no
/// prior emit this boot — is always due, so a freshly-booted idle daemon
/// leaves exactly one liveness row immediately rather than going dark for
/// the first [`HEARTBEAT_INTERVAL`].
fn heartbeat_due(
    last_emit: Option<OffsetDateTime>,
    now: OffsetDateTime,
    interval: Duration,
) -> bool {
    match last_emit {
        None => true,
        // Compare in whole milliseconds: `now - prev` is a `time::Duration`
        // and `interval` a `std::time::Duration`; the two don't compare
        // directly, and a non-monotonic wall clock could make `now < prev`
        // (returns false → no spurious heartbeat, which is the safe arm).
        Some(prev) => (now - prev).whole_milliseconds() >= interval.as_millis() as i128,
    }
}

/// Minimal URL-encoder for the `?since=<iso>` query parameter.
/// We only need to escape `:` and `+` which are the chars actually
/// present in an ISO-8601 timestamp; anything else passes through. Not
/// a general-purpose encoder.
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '0'..='9' | 'A'..='Z' | 'a'..='z' | '-' | '_' | '.' | '~' => out.push(c),
            _ => {
                let mut buf = [0u8; 4];
                let bytes = c.encode_utf8(&mut buf).as_bytes();
                for b in bytes {
                    out.push_str(&format!("%{b:02X}"));
                }
            }
        }
    }
    out
}

fn panic_payload_to_string(payload: Box<dyn std::any::Any + Send>) -> String {
    let raw = if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    };
    let mut sanitized: String = raw
        .chars()
        .filter(|c| !matches!(c, '\r' | '\n' | '\u{0000}'))
        .collect();
    if sanitized.len() > 1000 {
        sanitized.truncate(1000);
    }
    sanitized
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serialise the four env-mutating tests in this module — cargo's
    /// default parallel runner would otherwise race two `set_var` /
    /// `remove_var` calls on the same key and the slower thread's read
    /// would see the other thread's write. S311 / PR-278's CI run hit
    /// this race on `resolve_poll_interval_defaults_when_unset` (env
    /// set by `_clamps_out_of_range`). A single process-wide Mutex is
    /// the smallest fix that does not pull in `serial_test`.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn resolve_poll_interval_defaults_when_unset() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::remove_var(POLL_INTERVAL_ENV);
        assert_eq!(
            resolve_poll_interval(),
            Duration::from_secs(POLL_TICK_SECS_DEFAULT)
        );
    }

    #[test]
    fn resolve_poll_interval_clamps_out_of_range() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var(POLL_INTERVAL_ENV, "0");
        assert_eq!(
            resolve_poll_interval(),
            Duration::from_secs(POLL_TICK_SECS_DEFAULT)
        );
        std::env::set_var(POLL_INTERVAL_ENV, "100000");
        assert_eq!(
            resolve_poll_interval(),
            Duration::from_secs(POLL_TICK_SECS_DEFAULT)
        );
        std::env::set_var(POLL_INTERVAL_ENV, "garbage");
        assert_eq!(
            resolve_poll_interval(),
            Duration::from_secs(POLL_TICK_SECS_DEFAULT)
        );
        std::env::set_var(POLL_INTERVAL_ENV, "10");
        assert_eq!(resolve_poll_interval(), Duration::from_secs(10));
        std::env::remove_var(POLL_INTERVAL_ENV);
    }

    #[test]
    fn is_disabled_false_by_default() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::remove_var(POLL_DISABLE_ENV);
        assert!(!is_disabled());
    }

    #[test]
    fn is_disabled_true_for_canonical_values() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        for v in ["1", "true", "TRUE", "True"] {
            std::env::set_var(POLL_DISABLE_ENV, v);
            assert!(is_disabled(), "expected disabled=true for {v}");
        }
        std::env::set_var(POLL_DISABLE_ENV, "false");
        assert!(!is_disabled());
        std::env::set_var(POLL_DISABLE_ENV, "0");
        assert!(!is_disabled());
        std::env::remove_var(POLL_DISABLE_ENV);
    }

    #[test]
    fn entry_byte_size_counts_text_html_attachments() {
        use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
        let entry = EmailOutboxEntry {
            id: "01H".to_string(),
            queued_at: "2026-06-09T00:00:00Z".to_string(),
            to: vec!["c@x.com".to_string()],
            cc: vec![],
            subject: "s".to_string(),
            body_text: "x".repeat(100),
            body_html: Some("y".repeat(50)),
            attachments: Some(vec![EmailOutboxAttachment {
                filename: "a.pdf".to_string(),
                content_type: "application/pdf".to_string(),
                data_b64: B64.encode(vec![0u8; 80]),
            }]),
            submitter: "priced_ready".to_string(),
            state: "queued".to_string(),
            attempt_n: 0,
            last_error: None,
            sent_at: None,
            audit_id: None,
        };
        let sz = entry_byte_size(&entry);
        // 100 + 50 + ~80 (base64-roughed). The base64 length is
        // ceil(80/3)*4 = 108; 108*3/4 = 81. We want it in a sane range,
        // not an exact pin.
        assert!((200..=240).contains(&sz), "byte_size {sz} out of range");
    }

    #[test]
    fn recipients_combined_preserves_order_and_cc() {
        let entry = EmailOutboxEntry {
            id: "x".into(),
            queued_at: "t".into(),
            to: vec!["a@x.com".into(), "b@x.com".into()],
            cc: vec!["c@x.com".into()],
            subject: "s".into(),
            body_text: "b".into(),
            body_html: None,
            attachments: None,
            submitter: "p".into(),
            state: "queued".into(),
            attempt_n: 0,
            last_error: None,
            sent_at: None,
            audit_id: None,
        };
        let v = recipients_combined(&entry);
        assert_eq!(
            v,
            vec![
                "a@x.com".to_string(),
                "b@x.com".to_string(),
                "c@x.com".to_string()
            ]
        );
    }

    #[test]
    fn classify_fetch_error_buckets_known_families() {
        // S311 / F18 — token-rotation 401 must surface as auth_failed so
        // the operator-facing audit row points at the right knob.
        assert_eq!(
            classify_fetch_error(&anyhow!(
                "storefront GET email-queue: HTTP 401: unauthorized"
            )),
            "auth_failed"
        );
        // 5xx and 4xx-other go to "other" — still louder than silence.
        assert_eq!(
            classify_fetch_error(&anyhow!(
                "storefront GET email-queue: HTTP 503: service unavailable"
            )),
            "other"
        );
        assert_eq!(
            classify_fetch_error(&anyhow!("storefront GET email-queue: HTTP 404: not found")),
            "other"
        );
        // Decode failures (JSON shape regression on the storefront side).
        assert_eq!(
            classify_fetch_error(&anyhow!("decode response from http://...: expected `]`")),
            "decode"
        );
        // Network family — anything else, including reqwest TLS errors.
        assert_eq!(
            classify_fetch_error(&anyhow!("connection refused after 30s")),
            "network"
        );
        assert_eq!(
            classify_fetch_error(&anyhow!("dns lookup failed")),
            "network"
        );
    }

    #[test]
    fn classify_send_error_routes_three_classes() {
        assert_eq!(
            classify_send_error(&anyhow!("compose: bad mailbox")),
            "compose"
        );
        assert_eq!(
            classify_send_error(&anyhow!("writeback: HTTP 500")),
            "writeback"
        );
        assert_eq!(
            classify_send_error(&anyhow!("SMTP transport: connection refused")),
            "smtp_transport"
        );
        // Default classification for "unknown" shapes is smtp_transport
        // — the dominant failure family.
        assert_eq!(classify_send_error(&anyhow!("weird")), "smtp_transport");
    }

    #[test]
    fn url_encode_escapes_colon_and_plus() {
        let s = "2026-06-09T12:00:00+02:00";
        let enc = url_encode(s);
        assert!(!enc.contains(':'), "got {enc}");
        assert!(!enc.contains('+'), "got {enc}");
        // Letters and digits pass through.
        assert!(enc.contains("2026-06-09T12"), "got {enc}");
    }

    #[test]
    fn url_encode_passes_through_unreserved() {
        let s = "abcXYZ0-9._~";
        assert_eq!(url_encode(s), s);
    }

    #[test]
    fn dormant_handle_snapshot_defaults() {
        let h = EmailOutboxDaemonHandle::dormant();
        let s = h.snapshot();
        assert!(!s.spawned);
        assert_eq!(s.poll_interval_secs, 0);
        assert_eq!(s.total_cycles_since_boot, 0);
    }

    #[test]
    fn record_spawned_sets_flag_and_cadence() {
        let h = EmailOutboxDaemonHandle::dormant();
        h.record_spawned(Duration::from_secs(7));
        let s = h.snapshot();
        assert!(s.spawned);
        assert_eq!(s.poll_interval_secs, 7);
    }

    #[test]
    fn record_dormant_cycle_writes_error_detail() {
        let h = EmailOutboxDaemonHandle::dormant();
        h.record_dormant_cycle("2026-06-09T00:00:00Z".into(), "no credential");
        let s = h.snapshot();
        assert_eq!(s.last_poll_ts.as_deref(), Some("2026-06-09T00:00:00Z"));
        assert!(s
            .last_error_detail
            .as_deref()
            .unwrap()
            .contains("no credential"));
    }

    #[test]
    fn record_cycle_accumulates_lifetime_counters() {
        let h = EmailOutboxDaemonHandle::dormant();
        for i in 0..3 {
            h.record_cycle(CycleOutcome {
                cycle_at: format!("2026-06-09T00:00:0{i}Z"),
                fetched_count: 2,
                sent_count: 2,
                failed_count: 0,
                last_seen_iso: None,
                last_error_detail: None,
            });
        }
        let s = h.snapshot();
        assert_eq!(s.total_cycles_since_boot, 3);
        assert_eq!(s.total_fetched_since_boot, 6);
        assert_eq!(s.total_sent_since_boot, 6);
        assert_eq!(s.total_failed_since_boot, 0);
    }

    #[test]
    fn record_panic_increments_counter() {
        let h = EmailOutboxDaemonHandle::dormant();
        h.record_panic("boom", "2026-06-09T00:00:00Z".into());
        h.record_panic("again", "2026-06-09T00:00:01Z".into());
        let s = h.snapshot();
        assert_eq!(s.recent_panic_count, 2);
        assert_eq!(s.last_panic_msg.as_deref(), Some("again"));
        h.reset_panic_count();
        let s2 = h.snapshot();
        assert_eq!(s2.recent_panic_count, 0);
        assert!(s2.last_panic_msg.is_none());
    }

    #[test]
    fn panic_payload_to_string_handles_string_and_static_str() {
        let p: Box<dyn std::any::Any + Send> = Box::new("static-str-panic");
        assert_eq!(panic_payload_to_string(p), "static-str-panic");
        let p2: Box<dyn std::any::Any + Send> = Box::new("dynamic-string".to_string());
        assert_eq!(panic_payload_to_string(p2), "dynamic-string");
    }

    #[test]
    fn panic_payload_to_string_strips_control_chars_and_truncates() {
        let raw = format!("hello\r\nworld\0{}", "x".repeat(2000));
        let p: Box<dyn std::any::Any + Send> = Box::new(raw);
        let out = panic_payload_to_string(p);
        assert!(!out.contains('\r'));
        assert!(!out.contains('\n'));
        assert!(!out.contains('\0'));
        assert!(out.len() <= 1000);
    }

    #[test]
    fn s335_email_outbox_heartbeat_emits_at_cadence() {
        let base = OffsetDateTime::now_utc();
        let interval = Duration::from_secs(5 * 60);
        // First-ever decision (no prior emit) is always due — a freshly
        // booted idle daemon leaves one liveness row immediately.
        assert!(heartbeat_due(None, base, interval), "None must be due");
        // Inside the interval: NOT due (this is the throttle).
        assert!(
            !heartbeat_due(Some(base), base + Duration::from_secs(60), interval),
            "60s < 5min must NOT be due"
        );
        assert!(
            !heartbeat_due(Some(base), base + Duration::from_secs(5 * 60 - 1), interval),
            "just under 5min must NOT be due"
        );
        // At/after the interval: due again.
        assert!(
            heartbeat_due(Some(base), base + Duration::from_secs(5 * 60), interval),
            "exactly 5min must be due"
        );
        assert!(
            heartbeat_due(Some(base), base + Duration::from_secs(600), interval),
            "10min must be due"
        );
        // Non-monotonic wall clock (now < prev) takes the safe arm: NOT due.
        assert!(
            !heartbeat_due(Some(base), base - Duration::from_secs(60), interval),
            "clock skew backwards must NOT spuriously fire"
        );
    }

    #[test]
    fn s335_heartbeat_due_and_stamp_fires_once_then_throttles() {
        let h = EmailOutboxDaemonHandle::dormant();
        let interval = Duration::from_secs(5 * 60);
        let t0 = OffsetDateTime::now_utc();
        // First idle decision: due, and stamps.
        assert!(h.heartbeat_due_and_stamp(t0, interval));
        // Immediately after: throttled.
        assert!(!h.heartbeat_due_and_stamp(t0 + Duration::from_secs(5), interval));
        assert!(!h.heartbeat_due_and_stamp(t0 + Duration::from_secs(120), interval));
        // Past the interval: due again.
        assert!(h.heartbeat_due_and_stamp(t0 + Duration::from_secs(5 * 60), interval));
        // A non-heartbeat emit (real fetch / errored cycle) resets the clock.
        let t1 = t0 + Duration::from_secs(20 * 60);
        h.stamp_fetched_emit(t1);
        assert!(
            !h.heartbeat_due_and_stamp(t1 + Duration::from_secs(60), interval),
            "stamp_fetched_emit must reset the heartbeat clock"
        );
    }

    #[test]
    fn iso8601_now_parses_back() {
        use time::format_description::well_known::Rfc3339;
        let s = iso8601_now();
        let parsed = time::OffsetDateTime::parse(&s, &Rfc3339);
        assert!(parsed.is_ok(), "iso8601_now returned non-RFC3339: {s}");
    }
}
