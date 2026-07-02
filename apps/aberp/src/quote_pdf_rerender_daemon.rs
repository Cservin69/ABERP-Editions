//! S325 / PR-25 — customer-facing PDF re-render daemon (EVE addendum-2).
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
//!
//! Drains the in-memory [`crate::quote_pdf_rerender_queue`], and for each
//! quote-id whose `quote_intake_log.stock_alert` just flipped FALSE →
//! TRUE:
//! 1. loads the quote's pricing-job artifacts (`feature_graph_json` +
//!    `breakdown_json` + `feature_graph_hash` + `valid_until_iso` +
//!    `pdf_path`) from `quote_pricing_jobs`;
//! 2. re-renders `priced.pdf` via [`aberp_quote_pdf::render`] with
//!    `QuoteInputs.stock_alert = true` (the S318 PDF capability draws the
//!    red customer-facing stock-status band);
//! 3. re-POSTs to the storefront `POST /api/quotes/{id}/priced` with the
//!    SAME `feature_graph_hash` and `stock_alert:true` meta.
//!
//! The S323 storefront relax accepts a same-hash, `stock_alert:true`
//! re-post: it overwrites the stored PDF and flips the customer-side flag
//! (`{rerendered:true}`), or returns `{idempotent:true}` if it was
//! already flipped. Both are success. Before S323 a same-hash re-post was
//! swallowed as an idempotent no-op (see
//! `docs/findings/s318-customer-pdf-stock-banner.md`), which is why this
//! producer could not ship in S318.
//!
//! ## Ordering guarantee (why artifacts are always present)
//!
//! `stock_alert` can only transition once `stock_status_at_accept` is set
//! — which happens when the customer ACCEPTS the quote, strictly AFTER
//! the pricing pipeline has Fetched → … → Posted the PDF. So by the time
//! a transition is enqueued, the `quote_pricing_jobs` row has reached
//! `Posted` and carries all artifacts. A missing/NULL artifact is
//! therefore a genuine anomaly: classified **Permanent** (fail-loud audit
//! row, no hot-loop re-queue) rather than silently dropped.
//!
//! ## Failure classification (mirrors `quote_pricing_jobs::FailureKind`)
//!
//! - storefront `5xx` / transport (timeout, connection) → **Transient**:
//!   re-enqueued for the next cycle.
//! - storefront `4xx` (except `409`) → **Permanent**: dropped from the
//!   queue, audit row emitted.
//! - storefront `409` with an `idempotent:true` / `rerendered:true` body →
//!   **success** (already flipped / just flipped). Any other `409` reason
//!   (different-hash conflict, terminal state) or an unparseable body →
//!   **Permanent** (S329 🔴3 — never record a non-delivery as success).
//! - artifacts missing / render failure → **Permanent**.
//! - unexpected status (`1xx`/`3xx`) → **Unknown**: dropped + audited
//!   (no hot-loop), per the conservative no-silent-retry posture.
//!
//! ## Single-flight per cycle
//!
//! One `drain()` per cycle, entries processed sequentially. The customer
//! flow is single-digit quotes/day; ABERP-side parallelism buys nothing
//! and keeps the audit trail linear (mirrors the S307 email-outbox
//! daemon).
//!
//! ## Supervisor
//!
//! Wraps the inner loop in [`run_supervised`] (S286/S307 pattern): a
//! Rust-side panic is caught, logged, and the daemon re-spawns after a
//! 30s back-off (5min if 5+ panics in a 10-minute window). Cancellation
//! is honoured via the shared [`CancellationToken`].

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use duckdb::params;
#[allow(unused_imports)]
use duckdb::Connection;
use tokio::task::spawn_blocking;
use tokio_util::sync::CancellationToken;
use ulid::Ulid;

use aberp_audit_ledger::{append_in_tx, Actor, BinaryHash, EventKind, LedgerMeta, TenantId};
use aberp_quote_engine::{FeatureGraph, QuoteBreakdown, ToleranceRange};
use aberp_quote_pdf::QuoteInputs;

use crate::email_relay_daemon::scrub_for_audit;
use crate::quote_pdf_rerender_queue::QuotePdfRerenderQueue;
use crate::quote_pricing_pipeline::build_priced_multipart;
use crate::storefront_credential::{StorefrontCredentialHandle, StorefrontCredentialSnapshot};

/// Default poll cadence (seconds). Matches the email-outbox daemon: fast
/// enough that a stock downgrade reaches the customer PDF within seconds.
pub const POLL_TICK_SECS_DEFAULT: u64 = 5;

/// Operator-facing cadence override (seconds). Clamped to `[1, 3600]`.
pub const POLL_INTERVAL_ENV: &str = "ABERP_PDF_RERENDER_POLL_SECS";

/// Kill switch — set to `1`/`true` to suppress the daemon spawn at boot.
pub const POLL_DISABLE_ENV: &str = "ABERP_PDF_RERENDER_DISABLED";

/// HTTP timeout for the storefront re-post. Matches the S279 pipeline +
/// S307 outbox client bound.
const HTTP_TIMEOUT_SECS: u64 = 30;

const PANIC_WINDOW: Duration = Duration::from_secs(10 * 60);
const PANIC_BURST_THRESHOLD: usize = 5;
const PANIC_SHORT_BACKOFF: Duration = Duration::from_secs(30);
const PANIC_LONG_BACKOFF: Duration = Duration::from_secs(5 * 60);

/// Deps threaded from `AppState`. `Clone` for the supervisor's re-spawn.
#[derive(Clone)]
pub struct QuotePdfRerenderDaemonDeps {
    pub db_path: PathBuf,
    /// ADR-0098 Session B (Gap 1a) — shared DuckDB handle.
    pub db: aberp_db::HandleArc,
    pub tenant: TenantId,
    pub binary_hash: BinaryHash,
    pub operator_login: String,
    pub storefront_credential: Arc<StorefrontCredentialHandle>,
    pub queue: Arc<QuotePdfRerenderQueue>,
    pub poll_interval: Duration,
}

/// Resolve the poll interval from the env (default + clamp). Public so the
/// boot site can log the resolved value.
pub fn resolve_poll_interval() -> Duration {
    let secs = std::env::var(POLL_INTERVAL_ENV)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&n| (1..=3600).contains(&n))
        .unwrap_or(POLL_TICK_SECS_DEFAULT);
    Duration::from_secs(secs)
}

/// Kill-switch check. Returns true iff the daemon should NOT be spawned.
pub fn is_disabled() -> bool {
    std::env::var(POLL_DISABLE_ENV)
        .ok()
        .map(|v| {
            let t = v.trim();
            t == "1" || t.eq_ignore_ascii_case("true")
        })
        .unwrap_or(false)
}

// ── Boot recovery ─────────────────────────────────────────────────────

/// S329 / 🔴4 — replay unfinished re-render intents at boot.
///
/// The in-memory queue starts empty every boot, and `poll_once` drains
/// the whole set atomically into a local `Vec` before processing entries
/// sequentially (each with a 30s HTTP timeout). A panic / SIGTERM /
/// restart anywhere in that window loses every drained-but-unprocessed
/// id, and the read side cannot re-detect because `stock_alert` is now
/// sticky-TRUE. The durable signal already exists but was never replayed:
/// `persist_alerts_and_enqueue_rerender` writes a `QuotePdfRerenderEnqueued`
/// audit row in the same tx as the flip.
///
/// On boot, scan the ledger for every `quote.pdf_rerender_enqueued` whose
/// quote_id has NOT since reached a terminal — a `quote.pdf_rerendered`
/// (delivered) or a `quote.pdf_rerender_failed` the daemon classified
/// `permanent` (operator-retry-only) — and re-enqueue it. A `transient`
/// or `unknown` failed row is deliberately NOT terminal: the in-memory
/// re-enqueue it implies was also lost on the crash, so the banner is
/// still undelivered and must be recovered. Returns the count re-enqueued.
pub fn recover_unfinished_rerenders(
    db: &aberp_db::HandleArc,
    tenant: &TenantId,
    queue: &QuotePdfRerenderQueue,
) -> Result<usize> {
    let conn = db
        .read()
        .context("shared read: pdf-rerender boot recovery (ADR-0098 Gap 1a)")?;
    aberp_audit_ledger::ensure_schema(&conn)
        .context("ensure audit schema for pdf-rerender boot recovery")?;
    let mut stmt = conn.prepare(
        "SELECT kind, payload FROM audit_ledger
          WHERE tenant_id = ?1
            AND kind IN ('quote.pdf_rerender_enqueued',
                         'quote.pdf_rerendered',
                         'quote.pdf_rerender_failed')
          ORDER BY seq ASC",
    )?;
    let mut rows = stmt.query(params![tenant.as_str()])?;
    // Replay the per-quote event stream in seq order: an enqueue makes a
    // quote outstanding; a terminal clears it. The final residue is the
    // set of quotes whose last word was "enqueued" (or a non-permanent
    // failure) with no delivery after.
    let mut outstanding: std::collections::HashSet<String> = std::collections::HashSet::new();
    while let Some(row) = rows.next()? {
        let kind: String = row.get(0)?;
        let payload: Vec<u8> = row.get(1)?;
        let value: serde_json::Value = match serde_json::from_slice(&payload) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let quote_id = match value.get("quote_id").and_then(|q| q.as_str()) {
            Some(q) => q.to_string(),
            None => continue,
        };
        match kind.as_str() {
            "quote.pdf_rerender_enqueued" => {
                outstanding.insert(quote_id);
            }
            "quote.pdf_rerendered" => {
                outstanding.remove(&quote_id);
            }
            "quote.pdf_rerender_failed" => {
                let permanent =
                    value.get("failure_kind").and_then(|k| k.as_str()) == Some("permanent");
                if permanent {
                    outstanding.remove(&quote_id);
                }
            }
            _ => {}
        }
    }
    let mut recovered = 0usize;
    for quote_id in &outstanding {
        if queue.enqueue(quote_id) {
            recovered += 1;
        }
    }
    Ok(recovered)
}

// ── Re-post transport (pluggable for tests) ───────────────────────────

/// A completed HTTP response from the storefront `/priced` re-post.
#[derive(Debug, Clone)]
pub struct RepostResponse {
    pub status: u16,
    pub body: String,
}

/// Strategy for re-POSTing the re-rendered PDF. The default
/// [`HttpReposter`] goes over reqwest; the integration test injects a
/// canned-status fake so the classify / audit / re-queue paths run
/// without a live storefront.
#[async_trait::async_trait]
pub trait PricedReposter: Send + Sync {
    /// POST the re-rendered PDF to `{base_url}/api/quotes/{id}/priced`
    /// with `stock_alert:true` meta and the SAME `feature_graph_hash`.
    /// `Ok` for any completed HTTP response (the daemon classifies the
    /// status); `Err` for transport-level failure (timeout / connection).
    async fn repost(
        &self,
        snap: &StorefrontCredentialSnapshot,
        quote_id: &str,
        feature_graph_hash: &str,
        valid_until_iso: &str,
        breakdown_json: &str,
        pdf_bytes: &[u8],
    ) -> Result<RepostResponse>;
}

/// Production reposter — hand-rolled multipart via the pipeline's
/// [`build_priced_multipart`] (now `stock_alert`-parameterised) + reqwest.
pub struct HttpReposter {
    client: reqwest::Client,
}

impl HttpReposter {
    pub fn new() -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
            .build()
            .context("build pdf-rerender HTTP client")?;
        Ok(Self { client })
    }
}

#[async_trait::async_trait]
impl PricedReposter for HttpReposter {
    async fn repost(
        &self,
        snap: &StorefrontCredentialSnapshot,
        quote_id: &str,
        feature_graph_hash: &str,
        valid_until_iso: &str,
        breakdown_json: &str,
        pdf_bytes: &[u8],
    ) -> Result<RepostResponse> {
        // S351 — share the trailing-slash-safe builder with the pricing
        // pipeline so every storefront writeback site trims identically.
        let url = crate::quote_pricing_pipeline::resolved_writeback_url(
            &snap.base_url,
            quote_id,
            "priced",
        );
        let boundary = format!("aberp-rr-{}", Ulid::new());
        let body = build_priced_multipart(
            &boundary,
            feature_graph_hash,
            valid_until_iso,
            breakdown_json,
            pdf_bytes,
            // The re-render path carries the stock-alert overlay.
            true,
        )?;
        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", snap.bearer.as_str()))
            // S377 — same SvelteKit CSRF gate as the priced-writeback path;
            // share the pipeline's origin derivation so both sites match.
            .header(
                "Origin",
                crate::quote_pricing_pipeline::origin_from_base_url(&snap.base_url),
            )
            .header(
                "Content-Type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(body)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        Ok(RepostResponse { status, body })
    }
}

// ── Classification ────────────────────────────────────────────────────

/// Verdict on a completed storefront `/priced` re-post.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepostOutcome {
    /// Delivered. `label` is the audit `outcome` discriminator.
    Success { label: &'static str },
    /// `5xx` — re-enqueue for the next cycle.
    Transient,
    /// `4xx` (not `409`) — drop + audit; re-queue cannot help.
    Permanent,
    /// Unexpected `1xx`/`3xx` — drop + audit (no hot-loop).
    Unknown,
}

/// Pure status → verdict mapping. `body` is inspected only to discriminate
/// the success label (`rerendered` vs `idempotent`); the substring probe
/// mirrors the S307 outbox daemon's classification posture.
pub fn classify_repost(status: u16, body: &str) -> RepostOutcome {
    match status {
        200..=299 => {
            let idempotent =
                body.contains("\"idempotent\":true") || body.contains("\"idempotent\": true");
            if idempotent {
                RepostOutcome::Success {
                    label: "idempotent",
                }
            } else {
                RepostOutcome::Success {
                    label: "rerendered",
                }
            }
        }
        // S329 / 🔴3 — a 409 is NOT uniformly benign. The storefront
        // returns 409 for three distinct cases; only the already-flipped
        // ones are success. Treating all 409s as Success (the S325 bug)
        // recorded genuine non-delivery (`terminal_or_committed`,
        // `already_priced_with_different_hash`) as `quote.pdf_rerendered`
        // — a CLAUDE.md #12 "fail-loud" violation that silently dropped
        // the customer's banner. Now: a body asserting `idempotent:true`
        // or `rerendered:true` is a legitimate already-flipped/just-flipped
        // 409 → Success; ANY other reason (different-hash conflict,
        // terminal state) OR an unparseable body → Permanent (operator
        // must reconcile; never claim a delivery that did not happen).
        409 => {
            let benign = body.contains("\"idempotent\":true")
                || body.contains("\"idempotent\": true")
                || body.contains("\"rerendered\":true")
                || body.contains("\"rerendered\": true");
            if benign {
                RepostOutcome::Success {
                    label: "already_flipped_409",
                }
            } else {
                RepostOutcome::Permanent
            }
        }
        500..=599 => RepostOutcome::Transient,
        400..=499 => RepostOutcome::Permanent,
        _ => RepostOutcome::Unknown,
    }
}

// ── DB load + render ──────────────────────────────────────────────────

/// Everything the re-post needs, loaded from `quote_pricing_jobs` and the
/// freshly re-rendered PDF bytes.
struct PreparedRerender {
    feature_graph_hash: String,
    valid_until_iso: String,
    breakdown_json: String,
    pdf_bytes: Vec<u8>,
}

/// Why a prepare attempt could not produce re-postable bytes.
enum PrepareError {
    /// No pricing-job row, or a NULL artifact. Permanent (see ordering
    /// guarantee in the module doc).
    ArtifactsMissing,
    /// DB open / query failure. Transient — retry next cycle.
    Db(anyhow::Error),
    /// Corrupt artifact JSON or `lopdf` render failure. Permanent — a
    /// re-post would carry the same bad data.
    Render(anyhow::Error),
}

/// Blocking: load artifacts, re-render the PDF with the stock-alert band,
/// and best-effort overwrite the on-disk `priced.pdf`. Pure DB + CPU; the
/// caller runs it inside `spawn_blocking`.
fn prepare_rerender(
    db: &aberp_db::HandleArc,
    tenant_id: &str,
    quote_id: &str,
) -> std::result::Result<PreparedRerender, PrepareError> {
    let conn = db
        .read()
        .context("shared read: pdf-rerender prepare (ADR-0098 Gap 1a)")
        .map_err(PrepareError::Db)?;
    crate::quote_pricing_jobs::ensure_schema(&conn)
        .context("ensure quote_pricing_jobs schema")
        .map_err(PrepareError::Db)?;

    let loaded: Option<(
        String,         // customer_email
        String,         // customer_name
        i64,            // quantity
        Option<String>, // feature_graph_hash
        Option<String>, // feature_graph_json
        Option<String>, // breakdown_json
        Option<String>, // pdf_path
        Option<String>, // valid_until_iso
    )> = conn
        .query_row(
            "SELECT customer_email, customer_name, quantity,
                    feature_graph_hash, feature_graph_json, breakdown_json,
                    pdf_path, valid_until_iso
               FROM quote_pricing_jobs
              WHERE quote_id = ?1 AND tenant_id = ?2",
            params![quote_id, tenant_id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3).ok(),
                    r.get(4).ok(),
                    r.get(5).ok(),
                    r.get(6).ok(),
                    r.get(7).ok(),
                ))
            },
        )
        .map(Some)
        .or_else(|e| match e {
            duckdb::Error::QueryReturnedNoRows => Ok(None),
            other => Err(PrepareError::Db(anyhow!("load pricing job: {other}"))),
        })?;

    let (
        customer_email,
        customer_name,
        quantity,
        hash,
        graph_json,
        breakdown_json,
        pdf_path,
        valid_until,
    ) = loaded.ok_or(PrepareError::ArtifactsMissing)?;

    let feature_graph_hash = hash.ok_or(PrepareError::ArtifactsMissing)?;
    let graph_json = graph_json.ok_or(PrepareError::ArtifactsMissing)?;
    let breakdown_json = breakdown_json.ok_or(PrepareError::ArtifactsMissing)?;
    let pdf_path = pdf_path.ok_or(PrepareError::ArtifactsMissing)?;
    let valid_until_iso = valid_until.ok_or(PrepareError::ArtifactsMissing)?;

    let graph: FeatureGraph = serde_json::from_str(&graph_json)
        .context("decode FeatureGraph for re-render")
        .map_err(PrepareError::Render)?;
    let breakdown: QuoteBreakdown = serde_json::from_str(&breakdown_json)
        .context("decode QuoteBreakdown for re-render")
        .map_err(PrepareError::Render)?;

    let inputs = QuoteInputs {
        quote_id,
        customer_email: &customer_email,
        customer_name: &customer_name,
        // Mirror `advance_render`'s first-render literal exactly so the
        // re-rendered PDF differs ONLY by the stock-alert band.
        customer_company: "",
        quantity: u32::try_from(quantity.max(1)).unwrap_or(1),
        notes: "",
        valid_until_iso: &valid_until_iso,
        extractor_version: aberp_cad_extract_wrapper::WRAPPER_VERSION,
        engine_version: &breakdown.engine_version,
        feature_graph: &graph,
        breakdown: &breakdown,
        // v1 daemon quotes everything at `Standard` (the storefront form
        // collects no tolerance band — see `PricingPipelineConfig::
        // default_tolerance`); matching it keeps the re-render faithful.
        target_tolerance: ToleranceRange::Standard,
        // The whole point of the re-render.
        stock_alert: true,
        // S427 — effective lead-time (override ?? computed), same source
        // as `advance_render` so the re-render differs only by the band.
        lead_time_days: crate::quote_pricing_jobs::get_effective_lead_time_days(
            &conn, quote_id, tenant_id,
        )
        .map_err(PrepareError::Db)?,
    };
    let pdf_bytes = aberp_quote_pdf::render(&inputs)
        .map_err(|e| PrepareError::Render(anyhow!("render priced.pdf: {e}")))?;

    // Best-effort overwrite the stored artifact so the on-disk PDF matches
    // what the customer now sees. A write failure does NOT abort the
    // re-post — the bytes still travel in the multipart.
    //
    // S385 — write ATOMICALLY (temp + fsync + rename) via the shared
    // `crate::fs::write_atomic`. The naive `std::fs::write` truncated the
    // file in place, so the SPA's PDF-download reader
    // (`serve::read_pricing_job_pdf`, S352) could catch a torn prefix mid
    // re-render and hand the operator a corrupt PDF. The atomic rename
    // means a concurrent reader sees either the whole old PDF or the
    // whole new one.
    if let Err(e) = crate::fs::write_atomic(&pdf_path, &pdf_bytes) {
        tracing::warn!(quote_id, pdf_path, error = %e, "pdf-rerender: failed to overwrite on-disk priced.pdf (re-post still proceeds)");
    }

    Ok(PreparedRerender {
        feature_graph_hash,
        valid_until_iso,
        breakdown_json,
        pdf_bytes,
    })
}

// ── Cycle ─────────────────────────────────────────────────────────────

/// One poll cycle. Public so the integration test can drive it without
/// sleeping. Drains the queue and processes each id; transient failures
/// are re-enqueued.
pub async fn poll_once(deps: &QuotePdfRerenderDaemonDeps, reposter: &dyn PricedReposter) {
    let ids = deps.queue.drain();
    if ids.is_empty() {
        return;
    }
    let snap = match deps.storefront_credential.snapshot() {
        Some(s) => s,
        None => {
            // Storefront not configured this boot — preserve the entries
            // for a later cycle (the read-side won't re-detect a
            // sticky-TRUE row). No HTTP, so the re-enqueue is cheap.
            for id in &ids {
                deps.queue.enqueue(id);
            }
            tracing::debug!(
                count = ids.len(),
                "pdf-rerender dormant: storefront credential not configured; re-queued"
            );
            return;
        }
    };
    for quote_id in ids {
        process_one(deps, reposter, &snap, &quote_id).await;
    }
}

async fn process_one(
    deps: &QuotePdfRerenderDaemonDeps,
    reposter: &dyn PricedReposter,
    snap: &StorefrontCredentialSnapshot,
    quote_id: &str,
) {
    let db = deps.db.clone();
    let tenant = deps.tenant.as_str().to_string();
    let qid = quote_id.to_string();
    let prepared = spawn_blocking(move || prepare_rerender(&db, &tenant, &qid)).await;

    let prep = match prepared {
        Ok(Ok(p)) => p,
        Ok(Err(PrepareError::ArtifactsMissing)) => {
            emit_failed(
                deps,
                quote_id,
                "permanent",
                "artifacts_missing",
                "no posted pricing-job artifacts for quote (expected Posted row)",
            )
            .await;
            return;
        }
        Ok(Err(PrepareError::Render(e))) => {
            emit_failed(
                deps,
                quote_id,
                "permanent",
                "render",
                &scrub_for_audit(&format!("{e:#}")),
            )
            .await;
            return;
        }
        Ok(Err(PrepareError::Db(e))) => {
            deps.queue.enqueue(quote_id);
            emit_failed(
                deps,
                quote_id,
                "transient",
                "other",
                &scrub_for_audit(&format!("{e:#}")),
            )
            .await;
            return;
        }
        Err(join) => {
            // The blocking task panicked — treat as transient, re-queue.
            deps.queue.enqueue(quote_id);
            emit_failed(
                deps,
                quote_id,
                "transient",
                "other",
                &scrub_for_audit(&format!("prepare task join: {join}")),
            )
            .await;
            return;
        }
    };

    match reposter
        .repost(
            snap,
            quote_id,
            &prep.feature_graph_hash,
            &prep.valid_until_iso,
            &prep.breakdown_json,
            &prep.pdf_bytes,
        )
        .await
    {
        Ok(resp) => match classify_repost(resp.status, &resp.body) {
            RepostOutcome::Success { label } => {
                emit_rerendered(
                    deps,
                    quote_id,
                    &prep.feature_graph_hash,
                    label,
                    prep.pdf_bytes.len() as u64,
                )
                .await;
            }
            RepostOutcome::Transient => {
                deps.queue.enqueue(quote_id);
                emit_failed(
                    deps,
                    quote_id,
                    "transient",
                    "http_5xx",
                    &scrub_for_audit(&format!("HTTP {}: {}", resp.status, resp.body)),
                )
                .await;
            }
            RepostOutcome::Permanent => {
                emit_failed(
                    deps,
                    quote_id,
                    "permanent",
                    "http_4xx",
                    &scrub_for_audit(&format!("HTTP {}: {}", resp.status, resp.body)),
                )
                .await;
            }
            RepostOutcome::Unknown => {
                emit_failed(
                    deps,
                    quote_id,
                    "unknown",
                    "other",
                    &scrub_for_audit(&format!("unexpected HTTP {}: {}", resp.status, resp.body)),
                )
                .await;
            }
        },
        Err(e) => {
            // Transport-level failure (timeout / connection) → transient.
            deps.queue.enqueue(quote_id);
            emit_failed(
                deps,
                quote_id,
                "transient",
                "transport",
                &scrub_for_audit(&format!("{e:#}")),
            )
            .await;
        }
    }
}

// ── Audit emit ────────────────────────────────────────────────────────

async fn emit_rerendered(
    deps: &QuotePdfRerenderDaemonDeps,
    quote_id: &str,
    feature_graph_hash: &str,
    outcome: &str,
    pdf_byte_size: u64,
) {
    let payload = serde_json::json!({
        "quote_id": quote_id,
        "feature_graph_hash": feature_graph_hash,
        "outcome": outcome,
        "pdf_byte_size": pdf_byte_size,
    });
    write_audit(deps, EventKind::QuotePdfRerendered, payload).await;
}

async fn emit_failed(
    deps: &QuotePdfRerenderDaemonDeps,
    quote_id: &str,
    failure_kind: &str,
    error_class: &str,
    error_detail: &str,
) {
    tracing::warn!(
        quote_id,
        failure_kind,
        error_class,
        error = %error_detail,
        "pdf-rerender failed"
    );
    let payload = serde_json::json!({
        "quote_id": quote_id,
        "failure_kind": failure_kind,
        "error_class": error_class,
        "error_detail": error_detail,
    });
    write_audit(deps, EventKind::QuotePdfRerenderFailed, payload).await;
}

async fn write_audit(
    deps: &QuotePdfRerenderDaemonDeps,
    kind: EventKind,
    payload: serde_json::Value,
) {
    let db = deps.db.clone();
    let tenant = deps.tenant.clone();
    let binary_hash = deps.binary_hash;
    let login = deps.operator_login.clone();
    let kind_label = kind.as_str();
    let res = spawn_blocking(move || -> Result<()> {
        let bytes = serde_json::to_vec(&payload).context("serialize pdf-rerender payload")?;
        let mut conn = db
            .write()
            .context("shared writer: pdf-rerender audit (ADR-0098 Gap 1a)")?;
        aberp_audit_ledger::ensure_schema(&conn).context("ensure audit schema")?;
        let tx = conn.transaction().context("begin pdf-rerender audit tx")?;
        let meta = LedgerMeta::new(tenant, binary_hash);
        let actor = Actor::from_local_cli(Ulid::new().to_string(), &login);
        append_in_tx(&tx, &meta, kind, bytes, actor, None).context("append pdf-rerender audit")?;
        tx.commit().context("commit pdf-rerender audit")?;
        Ok(())
    })
    .await;
    match res {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            tracing::error!(error = ?e, kind = %kind_label, "pdf-rerender audit write failed")
        }
        Err(join) => {
            tracing::error!(%join, kind = %kind_label, "pdf-rerender audit task panicked")
        }
    }
}

// ── Supervisor ────────────────────────────────────────────────────────

/// Supervised entry point. Mirrors the S286/S307 supervisor: catch a
/// Rust-side panic, log it, back off (30s / 5min after a burst), re-spawn.
pub async fn run_supervised(deps: QuotePdfRerenderDaemonDeps, cancel: CancellationToken) {
    tracing::info!(
        poll_interval_secs = deps.poll_interval.as_secs(),
        "pdf-rerender daemon spawned (S325 / PR-25)"
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
                tracing::error!(
                    panic_msg = %panic_msg,
                    "pdf-rerender daemon panicked; supervisor recovering"
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
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep(sleep_dur) => {}
                }
            }
        }
    }
}

async fn run_loop(deps: Arc<QuotePdfRerenderDaemonDeps>, cancel: CancellationToken) {
    let cadence = deps.poll_interval;
    // Match the sibling daemons' boot delay so other boot daemons settle
    // before we start hitting the storefront.
    tokio::select! {
        _ = cancel.cancelled() => return,
        _ = tokio::time::sleep(Duration::from_secs(30)) => {}
    }
    let reposter = match HttpReposter::new() {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "failed to build pdf-rerender HTTP client");
            return;
        }
    };
    loop {
        if cancel.is_cancelled() {
            return;
        }
        poll_once(&deps, &reposter).await;
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(cadence) => {}
        }
    }
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

    /// ADR-0098 Session B — test-only shim: the production
    /// `recover_unfinished_rerenders` now takes the shared `aberp_db::Handle`
    /// (Gap 1a); these boot-recovery unit tests still drive a real DB file, so
    /// build a transient handle from that path and delegate. (The `Connection`
    /// the handle opens is the single instance — same coherence as production;
    /// allow-listed by the D5 grep gate as a test path.)
    fn recover_unfinished_rerenders_from_path(
        db_path: &std::path::Path,
        tenant: &TenantId,
        queue: &QuotePdfRerenderQueue,
    ) -> Result<usize> {
        let handle = aberp_db::Handle::open_default(db_path, tenant.clone())
            .context("build test shared handle for boot-recovery shim")?;
        recover_unfinished_rerenders(&handle, tenant, queue)
    }
    use std::sync::Mutex;

    use aberp_quote_engine::{Feature, FeatureType};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    // ── env helpers ──────────────────────────────────────────────────

    #[test]
    fn resolve_poll_interval_defaults_and_clamps() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::remove_var(POLL_INTERVAL_ENV);
        assert_eq!(
            resolve_poll_interval(),
            Duration::from_secs(POLL_TICK_SECS_DEFAULT)
        );
        std::env::set_var(POLL_INTERVAL_ENV, "0");
        assert_eq!(
            resolve_poll_interval(),
            Duration::from_secs(POLL_TICK_SECS_DEFAULT)
        );
        std::env::set_var(POLL_INTERVAL_ENV, "999999");
        assert_eq!(
            resolve_poll_interval(),
            Duration::from_secs(POLL_TICK_SECS_DEFAULT)
        );
        std::env::set_var(POLL_INTERVAL_ENV, "12");
        assert_eq!(resolve_poll_interval(), Duration::from_secs(12));
        std::env::remove_var(POLL_INTERVAL_ENV);
    }

    #[test]
    fn is_disabled_reads_canonical_values() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::remove_var(POLL_DISABLE_ENV);
        assert!(!is_disabled());
        for v in ["1", "true", "TRUE"] {
            std::env::set_var(POLL_DISABLE_ENV, v);
            assert!(is_disabled(), "{v}");
        }
        std::env::set_var(POLL_DISABLE_ENV, "0");
        assert!(!is_disabled());
        std::env::remove_var(POLL_DISABLE_ENV);
    }

    // ── classify ─────────────────────────────────────────────────────

    #[test]
    fn classify_repost_maps_status_to_verdict() {
        assert_eq!(
            classify_repost(200, "{\"rerendered\":true}"),
            RepostOutcome::Success {
                label: "rerendered"
            }
        );
        assert_eq!(
            classify_repost(200, "{\"idempotent\":true}"),
            RepostOutcome::Success {
                label: "idempotent"
            }
        );
        // S329 / 🔴3 — a benign 409 (body asserts the flip already
        // happened) is success; every other 409 reason is Permanent.
        assert_eq!(
            classify_repost(409, "{\"status\":\"approved\",\"idempotent\":true}"),
            RepostOutcome::Success {
                label: "already_flipped_409"
            }
        );
        assert_eq!(
            classify_repost(409, "{\"rerendered\": true}"),
            RepostOutcome::Success {
                label: "already_flipped_409"
            }
        );
        assert_eq!(
            classify_repost(409, "{\"error\":\"already_priced_with_different_hash\"}"),
            RepostOutcome::Permanent
        );
        assert_eq!(
            classify_repost(409, "{\"error\":\"terminal_or_committed\"}"),
            RepostOutcome::Permanent
        );
        assert_eq!(
            classify_repost(409, "<garbage unparseable body>"),
            RepostOutcome::Permanent
        );
        assert_eq!(classify_repost(503, ""), RepostOutcome::Transient);
        assert_eq!(classify_repost(500, ""), RepostOutcome::Transient);
        assert_eq!(classify_repost(400, ""), RepostOutcome::Permanent);
        assert_eq!(classify_repost(404, ""), RepostOutcome::Permanent);
        assert_eq!(classify_repost(301, ""), RepostOutcome::Unknown);
    }

    // ── S351 — trailing-slash-safe writeback URL ─────────────────────

    /// Path-capturing TCP mock: replies via a oneshot with the request-line
    /// path the real `HttpReposter` actually hit, then a clean 200 so the
    /// repost resolves. A trailing slash in the stored base_url must NOT
    /// produce a `//api/…` double-slash path (the production incident).
    async fn s351_spawn_path_capturing_mock(
    ) -> (std::net::SocketAddr, tokio::sync::oneshot::Receiver<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (tx, rx) = tokio::sync::oneshot::channel::<String>();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0u8; 16 * 1024];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                let path = req
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("")
                    .to_string();
                let _ = tx.send(path);
                let body = r#"{"rerendered":true}"#;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        (addr, rx)
    }

    #[tokio::test]
    async fn s351_pdf_rerender_resolves_url_correctly() {
        let (addr, rx) = s351_spawn_path_capturing_mock().await;
        // Operator typed a trailing slash — the incident's root cause.
        let snap = StorefrontCredentialSnapshot {
            base_url: format!("http://{addr}/"),
            bearer: zeroize::Zeroizing::new("bearer-X".to_string()),
        };
        let reposter = HttpReposter::new().expect("client");
        let resp = reposter
            .repost(
                &snap,
                "00000000-0000-0000-0000-000000000003",
                "hash-abc",
                "2026-07-01",
                "{}",
                b"%PDF-1.4 fake",
            )
            .await
            .expect("repost must not transport-error");
        assert_eq!(resp.status, 200, "body {}", resp.body);
        let path = rx.await.expect("mock captured a request path");
        assert_eq!(
            path,
            "/api/quotes/00000000-0000-0000-0000-000000000003/priced"
        );
    }

    // ── S377 — Origin header on the re-render repost ─────────────────

    /// Request-capturing TCP mock: sends the FULL raw request back over the
    /// oneshot, then a clean 200 so the repost resolves.
    async fn s377_spawn_request_capturing_mock(
    ) -> (std::net::SocketAddr, tokio::sync::oneshot::Receiver<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (tx, rx) = tokio::sync::oneshot::channel::<String>();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0u8; 16 * 1024];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                let _ = tx.send(req);
                let body = r#"{"rerendered":true}"#;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        (addr, rx)
    }

    #[tokio::test]
    async fn s377_repost_sends_origin_header() {
        let (addr, rx) = s377_spawn_request_capturing_mock().await;
        // Trailing slash — Origin must still resolve slash-free, matching the
        // pipeline's shared derivation.
        let snap = StorefrontCredentialSnapshot {
            base_url: format!("http://{addr}/"),
            bearer: zeroize::Zeroizing::new("bearer-X".to_string()),
        };
        let reposter = HttpReposter::new().expect("client");
        let resp = reposter
            .repost(
                &snap,
                "00000000-0000-0000-0000-000000000004",
                "hash-abc",
                "2026-07-01",
                "{}",
                b"%PDF-1.4 fake",
            )
            .await
            .expect("repost must not transport-error");
        assert_eq!(resp.status, 200, "body {}", resp.body);
        let req = rx.await.expect("mock captured a request");
        let origin = req
            .lines()
            .find(|l| l.to_ascii_lowercase().starts_with("origin:"))
            .and_then(|l| l.split_once(':'))
            .map(|(_, v)| v.trim().to_string());
        assert_eq!(
            origin.as_deref(),
            Some(format!("http://{addr}").as_str()),
            "re-render repost must carry the same slash-free Origin header"
        );
    }

    // ── daemon round-trip ────────────────────────────────────────────

    /// Reposter fake that returns a canned (status, body) and records the
    /// PDF bytes + stock_alert hash it was handed.
    struct FakeReposter {
        status: u16,
        body: String,
        calls: Arc<Mutex<Vec<(String, usize)>>>, // (quote_id, pdf_len)
    }

    #[async_trait::async_trait]
    impl PricedReposter for FakeReposter {
        async fn repost(
            &self,
            _snap: &StorefrontCredentialSnapshot,
            quote_id: &str,
            _feature_graph_hash: &str,
            _valid_until_iso: &str,
            _breakdown_json: &str,
            pdf_bytes: &[u8],
        ) -> Result<RepostResponse> {
            self.calls
                .lock()
                .unwrap()
                .push((quote_id.to_string(), pdf_bytes.len()));
            Ok(RepostResponse {
                status: self.status,
                body: self.body.clone(),
            })
        }
    }

    /// Reposter that always errors at the transport layer.
    struct ErrReposter;
    #[async_trait::async_trait]
    impl PricedReposter for ErrReposter {
        async fn repost(
            &self,
            _snap: &StorefrontCredentialSnapshot,
            _quote_id: &str,
            _feature_graph_hash: &str,
            _valid_until_iso: &str,
            _breakdown_json: &str,
            _pdf_bytes: &[u8],
        ) -> Result<RepostResponse> {
            Err(anyhow!("connection refused"))
        }
    }

    fn sample_graph_json() -> String {
        let g = FeatureGraph {
            gears: Vec::new(),
            schema_version: 2,
            bounding_box_mm: [50.0, 30.0, 20.0],
            volume_mm3: 12345.6,
            surface_area_mm2: 6200.0,
            material_grade: "AL_6061_T6".to_string(),
            features: vec![Feature {
                feature_type: FeatureType::Hole,
                count: 4,
                representative_size_mm: 8.0,
            }],
            requires_5_axis: false,
            thin_wall_present: false,
            stock_form: aberp_quote_engine::StockForm::RectangularBlock,
            tolerance: aberp_quote_engine::ToleranceSpec::Unspecified,
            critical_feature_tolerances: Vec::new(),
        };
        serde_json::to_string(&g).unwrap()
    }

    fn sample_breakdown_json() -> String {
        let b = QuoteBreakdown {
            gear_cost: 0.0,
            tolerance_cost: 0.0,
            material_cost: 1.23,
            machining_cost: 9.87,
            cad_cam_cost: 2.10,
            setup_cost: 4.56,
            overhead: 1.50,
            margin: 3.84,
            total_price: 21.00,
            machining_minutes: 11.25,
            inspection_minutes: 2.0,
            route_to_5_axis: false,
            calibration_coefficient: 1.0,
            engine_version: "aberp-quote-engine@0.0.0".to_string(),
            reasoning_log: vec!["[totals] total_price = 21.00 EUR".to_string()],
        };
        serde_json::to_string(&b).unwrap()
    }

    /// Seed a Posted `quote_pricing_jobs` row with full artifacts + an
    /// on-disk `priced.pdf` placeholder the daemon will overwrite.
    fn seed_posted_job(db_path: &std::path::Path, tenant: &str, quote_id: &str, pdf_path: &str) {
        let conn = Connection::open(db_path).unwrap();
        crate::quote_pricing_jobs::ensure_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO quote_pricing_jobs (
                quote_id, tenant_id, state, fetched_at, updated_at,
                customer_email, customer_name, material_grade, quantity,
                cad_filename, cad_local_path, feature_graph_hash,
                feature_graph_json, breakdown_json, pdf_path,
                total_price_eur, valid_until_iso, attempt_n
             ) VALUES (?1, ?2, 'Posted', 'now', 'now',
                'c@example.com', 'Cust', 'AL_6061_T6', 5,
                'part.stl', '/tmp/part.stl', 'blake3:abc',
                ?3, ?4, ?5, 21.0, '2026-07-06', 0)",
            params![
                quote_id,
                tenant,
                sample_graph_json(),
                sample_breakdown_json(),
                pdf_path,
            ],
        )
        .unwrap();
        std::fs::write(pdf_path, b"%PDF-old-no-banner").unwrap();
    }

    fn test_deps(
        db_path: PathBuf,
        queue: Arc<QuotePdfRerenderQueue>,
    ) -> QuotePdfRerenderDaemonDeps {
        let cred = StorefrontCredentialHandle::dormant();
        cred.set(
            "https://abenerp.com".to_string(),
            zeroize::Zeroizing::new("bearer-X".to_string()),
        );
        QuotePdfRerenderDaemonDeps {
            db: aberp_db::Handle::open_default(&db_path, TenantId::new("t1").unwrap())
                .expect("test shared handle"),
            db_path,
            tenant: TenantId::new("t1").unwrap(),
            binary_hash: BinaryHash::from_bytes([0u8; 32]),
            operator_login: "op".to_string(),
            storefront_credential: cred,
            queue,
            poll_interval: Duration::from_secs(5),
        }
    }

    fn audit_count(db_path: &std::path::Path, kind: &str) -> i64 {
        let conn = Connection::open(db_path).unwrap();
        conn.query_row(
            "SELECT count(*) FROM audit_ledger WHERE kind = ?1",
            params![kind],
            |r| r.get(0),
        )
        .unwrap_or(0)
    }

    /// Per-test scratch dir under the system tmp (no `tempfile` dep —
    /// mirrors the S286 pipeline tests' convention). Caller cleans up.
    struct ScratchDir {
        root: PathBuf,
    }
    impl ScratchDir {
        fn new(tag: &str) -> Self {
            let mut root = std::env::temp_dir();
            root.push(format!("aberp-s325-{tag}-{}", Ulid::new()));
            std::fs::create_dir_all(&root).unwrap();
            Self { root }
        }
        fn db(&self) -> PathBuf {
            self.root.join("aberp.duckdb")
        }
        fn pdf(&self) -> PathBuf {
            self.root.join("priced.pdf")
        }
    }
    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[tokio::test]
    async fn s325_pdf_rerender_daemon_posts_with_stock_alert_true_on_drain() {
        let dir = ScratchDir::new("X");
        let db = dir.db();
        let pdf = dir.pdf();
        seed_posted_job(&db, "t1", "q-1", pdf.to_str().unwrap());

        let queue = QuotePdfRerenderQueue::new();
        queue.enqueue("q-1");
        let deps = test_deps(db.clone(), queue.clone());
        let calls = Arc::new(Mutex::new(Vec::new()));
        let reposter = FakeReposter {
            status: 200,
            body: "{\"rerendered\":true}".to_string(),
            calls: calls.clone(),
        };

        poll_once(&deps, &reposter).await;

        // One post, queue drained, success audit emitted.
        let recorded = calls.lock().unwrap().clone();
        assert_eq!(recorded.len(), 1, "exactly one re-post");
        assert_eq!(recorded[0].0, "q-1");
        assert!(recorded[0].1 > 500, "re-rendered PDF carries real bytes");
        assert!(queue.is_empty(), "queue drained on success");
        assert_eq!(audit_count(&db, "quote.pdf_rerendered"), 1);
        assert_eq!(audit_count(&db, "quote.pdf_rerender_failed"), 0);
        // On-disk PDF overwritten (no longer the placeholder).
        assert_ne!(std::fs::read(&pdf).unwrap(), b"%PDF-old-no-banner");
    }

    /// S329 / 🔴3 — a benign 409 (the storefront reports the flip already
    /// landed) IS success: the customer PDF carries the banner.
    #[tokio::test]
    async fn s329_pdf_rerender_benign_409_idempotent_is_success() {
        let dir = ScratchDir::new("X");
        let db = dir.db();
        let pdf = dir.pdf();
        seed_posted_job(&db, "t1", "q-1", pdf.to_str().unwrap());

        let queue = QuotePdfRerenderQueue::new();
        queue.enqueue("q-1");
        let deps = test_deps(db.clone(), queue.clone());
        let reposter = FakeReposter {
            status: 409,
            body: "{\"status\":\"approved\",\"idempotent\":true}".to_string(),
            calls: Arc::new(Mutex::new(Vec::new())),
        };

        poll_once(&deps, &reposter).await;

        assert!(queue.is_empty(), "benign 409 is success, queue drained");
        assert_eq!(audit_count(&db, "quote.pdf_rerendered"), 1);
        assert_eq!(audit_count(&db, "quote.pdf_rerender_failed"), 0);
    }

    /// S329 / 🔴3 — a non-benign 409 (`already_priced_with_different_hash`
    /// or `terminal_or_committed`) is a REAL non-delivery. The S325 bug
    /// classified every 409 as success and emitted `quote.pdf_rerendered`
    /// while the customer PDF was never overwritten. It must instead be
    /// Permanent: drop + a `quote.pdf_rerender_failed` audit row, NEVER a
    /// success row (CLAUDE.md #12 fail-loud).
    #[tokio::test]
    async fn s329_daemon_classifies_unknown_409_as_permanent_not_success() {
        let dir = ScratchDir::new("X");
        let db = dir.db();
        let pdf = dir.pdf();
        seed_posted_job(&db, "t1", "q-1", pdf.to_str().unwrap());

        let queue = QuotePdfRerenderQueue::new();
        queue.enqueue("q-1");
        let deps = test_deps(db.clone(), queue.clone());
        let reposter = FakeReposter {
            status: 409,
            body: "{\"error\":\"already_priced_with_different_hash\"}".to_string(),
            calls: Arc::new(Mutex::new(Vec::new())),
        };

        poll_once(&deps, &reposter).await;

        assert!(queue.is_empty(), "permanent 409 dropped, not re-queued");
        assert_eq!(
            audit_count(&db, "quote.pdf_rerendered"),
            0,
            "non-delivery must NOT be recorded as a success"
        );
        assert_eq!(audit_count(&db, "quote.pdf_rerender_failed"), 1);
    }

    #[tokio::test]
    async fn s325_pdf_rerender_daemon_classifies_5xx_as_transient_requeues() {
        let dir = ScratchDir::new("X");
        let db = dir.db();
        let pdf = dir.pdf();
        seed_posted_job(&db, "t1", "q-1", pdf.to_str().unwrap());

        let queue = QuotePdfRerenderQueue::new();
        queue.enqueue("q-1");
        let deps = test_deps(db.clone(), queue.clone());
        let reposter = FakeReposter {
            status: 503,
            body: "upstream down".to_string(),
            calls: Arc::new(Mutex::new(Vec::new())),
        };

        poll_once(&deps, &reposter).await;

        assert!(queue.contains("q-1"), "5xx re-enqueues for next cycle");
        assert_eq!(audit_count(&db, "quote.pdf_rerendered"), 0);
        assert_eq!(audit_count(&db, "quote.pdf_rerender_failed"), 1);
    }

    #[tokio::test]
    async fn s325_pdf_rerender_daemon_classifies_4xx_non_409_as_permanent_marks_failed() {
        let dir = ScratchDir::new("X");
        let db = dir.db();
        let pdf = dir.pdf();
        seed_posted_job(&db, "t1", "q-1", pdf.to_str().unwrap());

        let queue = QuotePdfRerenderQueue::new();
        queue.enqueue("q-1");
        let deps = test_deps(db.clone(), queue.clone());
        let reposter = FakeReposter {
            status: 400,
            body: "bad meta".to_string(),
            calls: Arc::new(Mutex::new(Vec::new())),
        };

        poll_once(&deps, &reposter).await;

        assert!(queue.is_empty(), "4xx (non-409) is permanent, not requeued");
        assert_eq!(audit_count(&db, "quote.pdf_rerendered"), 0);
        assert_eq!(audit_count(&db, "quote.pdf_rerender_failed"), 1);
    }

    #[tokio::test]
    async fn s325_pdf_rerender_transport_error_requeues_transient() {
        let dir = ScratchDir::new("X");
        let db = dir.db();
        let pdf = dir.pdf();
        seed_posted_job(&db, "t1", "q-1", pdf.to_str().unwrap());

        let queue = QuotePdfRerenderQueue::new();
        queue.enqueue("q-1");
        let deps = test_deps(db.clone(), queue.clone());

        poll_once(&deps, &ErrReposter).await;

        assert!(queue.contains("q-1"), "transport error re-enqueues");
        assert_eq!(audit_count(&db, "quote.pdf_rerender_failed"), 1);
    }

    #[tokio::test]
    async fn s325_pdf_rerender_missing_artifacts_is_permanent() {
        let dir = ScratchDir::new("ghost");
        let db = dir.db();
        // No pricing-job row seeded — the queue holds a phantom id.
        let conn = Connection::open(&db).unwrap();
        crate::quote_pricing_jobs::ensure_schema(&conn).unwrap();
        drop(conn);

        let queue = QuotePdfRerenderQueue::new();
        queue.enqueue("ghost");
        let deps = test_deps(db.clone(), queue.clone());
        let reposter = FakeReposter {
            status: 200,
            body: "{}".to_string(),
            calls: Arc::new(Mutex::new(Vec::new())),
        };

        poll_once(&deps, &reposter).await;

        assert!(
            queue.is_empty(),
            "missing artifacts is permanent (no hot-loop requeue)"
        );
        assert_eq!(audit_count(&db, "quote.pdf_rerender_failed"), 1);
    }

    /// Append one re-render audit row directly, mirroring what the
    /// enqueue seam / daemon emit. `failure_kind` is set only for
    /// `QuotePdfRerenderFailed`.
    fn append_rr_event(
        db_path: &std::path::Path,
        tenant: &str,
        kind: EventKind,
        quote_id: &str,
        failure_kind: Option<&str>,
    ) {
        let mut conn = Connection::open(db_path).unwrap();
        aberp_audit_ledger::ensure_schema(&conn).unwrap();
        let mut payload = serde_json::json!({ "quote_id": quote_id });
        if let Some(fk) = failure_kind {
            payload["failure_kind"] = serde_json::Value::String(fk.to_string());
        }
        let bytes = serde_json::to_vec(&payload).unwrap();
        let tx = conn.transaction().unwrap();
        let meta = LedgerMeta::new(
            TenantId::new(tenant).unwrap(),
            BinaryHash::from_bytes([0u8; 32]),
        );
        let actor = Actor::from_local_cli(Ulid::new().to_string(), "test");
        append_in_tx(&tx, &meta, kind, bytes, actor, None).unwrap();
        tx.commit().unwrap();
    }

    /// S329 / 🔴4 — boot recovery replays enqueued intents that never
    /// reached a terminal. q1 was delivered (`rerendered`), q2 failed
    /// `permanent` — both are done. q3 failed `transient` (the in-memory
    /// re-enqueue was lost on crash) and q4 was enqueued with nothing
    /// after — both are still undelivered and MUST be re-enqueued.
    #[tokio::test]
    async fn s329_boot_replays_unfinished_rerender_enqueued_audit_entries() {
        let dir = ScratchDir::new("recover");
        let db = dir.db();
        for qid in ["q1", "q2", "q3", "q4"] {
            append_rr_event(&db, "t1", EventKind::QuotePdfRerenderEnqueued, qid, None);
        }
        append_rr_event(&db, "t1", EventKind::QuotePdfRerendered, "q1", None);
        append_rr_event(
            &db,
            "t1",
            EventKind::QuotePdfRerenderFailed,
            "q2",
            Some("permanent"),
        );
        append_rr_event(
            &db,
            "t1",
            EventKind::QuotePdfRerenderFailed,
            "q3",
            Some("transient"),
        );

        let queue = QuotePdfRerenderQueue::new();
        let recovered =
            recover_unfinished_rerenders_from_path(&db, &TenantId::new("t1").unwrap(), &queue)
                .unwrap();

        assert_eq!(recovered, 2, "only q3 (transient) + q4 (no terminal)");
        assert!(!queue.contains("q1"), "delivered quote not replayed");
        assert!(
            !queue.contains("q2"),
            "permanently-failed quote not replayed"
        );
        assert!(queue.contains("q3"), "transient-failed quote replayed");
        assert!(queue.contains("q4"), "never-finished quote replayed");
    }

    /// A re-enqueue-after-delivery (downgrade → deliver → second downgrade
    /// → enqueue again) leaves the quote outstanding: the LAST event wins.
    #[tokio::test]
    async fn s329_boot_recovery_last_event_wins_per_quote() {
        let dir = ScratchDir::new("recover-seq");
        let db = dir.db();
        append_rr_event(&db, "t1", EventKind::QuotePdfRerenderEnqueued, "q5", None);
        append_rr_event(&db, "t1", EventKind::QuotePdfRerendered, "q5", None);
        // A later, distinct downgrade re-enqueues the same quote.
        append_rr_event(&db, "t1", EventKind::QuotePdfRerenderEnqueued, "q5", None);

        let queue = QuotePdfRerenderQueue::new();
        let recovered =
            recover_unfinished_rerenders_from_path(&db, &TenantId::new("t1").unwrap(), &queue)
                .unwrap();
        assert_eq!(recovered, 1);
        assert!(
            queue.contains("q5"),
            "re-enqueue after delivery is outstanding"
        );
    }

    #[tokio::test]
    async fn s325_pdf_rerender_dormant_credential_requeues() {
        let dir = ScratchDir::new("dormant");
        let db = dir.db();
        let queue = QuotePdfRerenderQueue::new();
        queue.enqueue("q-1");
        let mut deps = test_deps(db.clone(), queue.clone());
        // Wipe the credential → dormant.
        deps.storefront_credential = StorefrontCredentialHandle::dormant();
        let reposter = FakeReposter {
            status: 200,
            body: "{}".to_string(),
            calls: Arc::new(Mutex::new(Vec::new())),
        };

        poll_once(&deps, &reposter).await;

        assert!(
            queue.contains("q-1"),
            "dormant credential preserves the entry for a later cycle"
        );
    }
}
