//! S279 / PR-265 — auto-quoting producer pipeline.
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
//! ## What this module is
//!
//! Orchestration glue between four moving pieces:
//!
//! 1. Storefront polling — `/api/quotes?status=received` to discover
//!    new submissions, `/api/quotes/{id}/files/{name}` to pull the
//!    CAD blob, `/api/quotes/{id}/status` to flip the customer-facing
//!    state to `quoting` on extract start.
//! 2. [`aberp_cad_extract_wrapper`] — Python subprocess that emits a
//!    `FeatureGraph` from the CAD file.
//! 3. [`aberp_quote_engine`] — pure scoring function that turns
//!    FeatureGraph + catalogue snapshot into a `QuoteBreakdown`.
//! 4. [`aberp_quote_pdf`] — pure renderer that turns FeatureGraph +
//!    QuoteBreakdown + customer info into PDF bytes.
//!
//! Then the priced-writeback POST to `/api/quotes/{id}/priced` per
//! ADR-0004's multipart contract, with the `feature_graph_hash` as
//! the idempotency key.
//!
//! ## State machine
//!
//! See [`crate::quote_pricing_jobs`] — `Fetched → Extracting →
//! Pricing → Rendering → PostingBack → Posted | Failed`. The pipeline
//! advances one row per `poll_once` cycle to keep memory pressure
//! bounded (the Python extractor can be 100 MB+ resident).
//!
//! ## Why a separate daemon from quote-intake
//!
//! The existing [`aberp_quote_intake`] daemon polls `status=approved`
//! and stages rows into `quote_intake_log` for operator pickup. The
//! pipeline polls `status=received` and drives them through pricing
//! into the storefront's `quoted` state. The two never overlap: the
//! storefront's accept-click on a `quoted` quote later transitions
//! it to `approved`, at which point the existing intake daemon picks
//! it up. Single-responsibility per daemon — easier to reason about
//! cadences, audit emit, and shutdown semantics.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::task::spawn_blocking;
use tokio_util::sync::CancellationToken;
use ulid::Ulid;
use zeroize::Zeroizing;

use crate::cad_blob::{CadBlobCtx, DecryptedTempFile, ReadPurpose};
use aberp_audit_ledger::{
    append_in_tx, ensure_schema as audit_ensure_schema, Actor, BinaryHash, EventKind, LedgerMeta,
    TenantId,
};
use aberp_cad_extract_wrapper::{CadExtractor, ExtractRequest};
use aberp_quote_engine::{
    self as engine, ComplexityRule as EngineComplexityRule, FeatureGraph, GearOp, GearProcess,
    GearProcessRate as EngineGearProcessRate, MachineFamily, MachineRate as EngineMachineRate,
    Material as EngineMaterial, QuoteBreakdown, QuotingParameters, StockAdjustment, StockForm,
    StockStatus, ToleranceMultiplier, ToleranceRange,
};
use aberp_quote_pdf::QuoteInputs;

use crate::catalogue_push::response_excerpt;
use crate::quote_pricing_jobs::{self as jobs, FailureKind, JobState, PricingJobRow};

/// S2 / ADR-0094 Gap 1 (wiring) — reconstruct the operator-chosen
/// [`StockForm`] from the persisted quote/part columns. `None` means the
/// operator left it unset (NULL or `rectangular_block`, or any unknown
/// discriminant the SPA cannot emit), so the caller keeps whatever form
/// the graph already carries (the extractor hint, or the
/// `RectangularBlock` serde-default). Pure + total: unit-tested for each
/// precedence path with neither a DB nor a live engine.
fn operator_stock_form(
    kind: Option<&str>,
    od_mm: Option<f64>,
    id_mm: Option<f64>,
    length_mm: Option<f64>,
) -> Option<StockForm> {
    match kind {
        Some("round_bar") => Some(StockForm::RoundBar {
            diameter_mm: od_mm.unwrap_or(0.0),
            length_mm: length_mm.unwrap_or(0.0),
        }),
        Some("tube") => Some(StockForm::Tube {
            od_mm: od_mm.unwrap_or(0.0),
            id_mm: id_mm.unwrap_or(0.0),
            length_mm: length_mm.unwrap_or(0.0),
        }),
        // NULL, "rectangular_block", or anything unrecognised ⇒ no operator
        // override (inert): the graph keeps its extractor hint / default.
        _ => None,
    }
}

/// S2 / ADR-0094 Gap 1 (wiring) — stamp the chosen stock form onto `graph`
/// with precedence **operator field > extractor hint > RectangularBlock**,
/// returning the chosen form + its provenance for the pricing audit.
///
/// The graph already carries the extractor's hint (or, until S269 lands,
/// the `RectangularBlock` serde-default), so `operator = None` leaves it
/// untouched — that IS the "extractor hint > RectangularBlock" tier, for
/// free. An explicit operator form overwrites it. Provenance is best-
/// effort: an extractor that explicitly emitted `RectangularBlock` is
/// indistinguishable from the default and reads as `"default"` — harmless,
/// since the math and the price are identical either way.
fn stamp_stock_form(
    graph: &mut FeatureGraph,
    operator: Option<StockForm>,
) -> (StockForm, &'static str) {
    match operator {
        Some(form) => {
            graph.stock_form = form;
            (form, "operator")
        }
        None if graph.stock_form != StockForm::RectangularBlock => (graph.stock_form, "extractor"),
        None => (graph.stock_form, "default"),
    }
}

/// S6 / ADR-0094 Gap 3 (wiring) — reconstruct the operator-entered gear ops
/// from the persisted `gear_ops_json` column (the engine's `Vec<GearOp>` wire
/// shape). `None` / empty / `"[]"` ⇒ `None` (no operator override: the graph
/// keeps its empty serde-default, or a future S269 extractor hint). A
/// present-but-malformed payload is a hard error — fail loud (CLAUDE.md rule
/// 12); the route layer validates the closed vocab before persisting, so a
/// malformed blob here is real corruption, not operator input. Pure + total.
fn operator_gear_ops(json: Option<&str>) -> Result<Option<Vec<GearOp>>> {
    let Some(raw) = json else { return Ok(None) };
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "[]" {
        return Ok(None);
    }
    let ops: Vec<GearOp> =
        serde_json::from_str(trimmed).context("decode operator gear_ops_json")?;
    if ops.is_empty() {
        Ok(None)
    } else {
        Ok(Some(ops))
    }
}

/// S6 / ADR-0094 Gap 3 (wiring) — stamp the operator gear ops onto
/// `graph.gears` with precedence **operator > extractor hint (already on the
/// graph) > empty default**, returning the provenance for the pricing audit.
/// Mirrors `stamp_stock_form`: `operator = None` leaves the graph's gears
/// untouched, so a quote with no operator gears keeps its empty default ⇒ no
/// gear cost ⇒ byte-identical to today. The gear count for the audit is read
/// back off `graph.gears` after stamping.
fn stamp_gear_ops(graph: &mut FeatureGraph, operator: Option<Vec<GearOp>>) -> &'static str {
    match operator {
        Some(ops) => {
            graph.gears = ops;
            "operator"
        }
        None if !graph.gears.is_empty() => "extractor",
        None => "default",
    }
}

/// What every pricing-pipeline cycle reports back. Used for the SPA
/// "last cycle" indicator + the daemon's log line.
#[derive(Debug, Clone, Default)]
pub struct PipelineCycleSummary {
    /// `received` quotes polled this cycle.
    pub fetched_from_storefront: u32,
    /// New `quote_pricing_jobs` rows inserted (idempotent — a quote
    /// already in the table doesn't bump this).
    pub enqueued: u32,
    /// Number of jobs advanced one step this cycle (extract / price
    /// / render / post). Caps at the per-cycle batch limit.
    pub advanced: u32,
    /// Number of jobs that hit `Failed` this cycle. The audit row is
    /// the durable record.
    pub failed: u32,
    /// Number of jobs that reached the terminal `Posted` state this
    /// cycle. UI badges key on this to surface "quote sent" toasts.
    pub posted: u32,
    /// Wall-clock elapsed_ms for the cycle.
    pub elapsed_ms: u64,
    /// Top-level cycle error if any (transport flake, audit-ledger
    /// failure). Per-job errors are NOT surfaced here — those go on
    /// the job row's `error_reason`.
    pub error: Option<String>,
}

/// Per-cycle batch cap. Honest v1 latency: a slow Python extractor
/// at 30s × 5 jobs = 150s, well under the 60s storefront poll cadence
/// so the cycle still completes within roughly the cadence period.
const MAX_JOBS_PER_CYCLE: u32 = 5;

/// Default valid_until window for an indicative quote, in days. Per
/// ADR-0004 the storefront requires `YYYY-MM-DD` in the future; 30
/// days is the "reasonable indicative window" the design doc names.
const DEFAULT_VALID_UNTIL_DAYS: i64 = 30;

/// Pipeline config — the daemon needs the storefront URL + bearer +
/// the local artifact-dir to land downloaded CADs and rendered PDFs.
#[derive(Debug, Clone)]
pub struct PricingPipelineConfig {
    pub base_url: String,
    pub bearer_token: Zeroizing<String>,
    /// How often the daemon loops. The Stage 2 baseline is 60s
    /// (matches the storefront design doc §3 customer-facing copy).
    pub poll_interval: Duration,
    /// Local filesystem dir where `<quote_id>/{cad,priced.pdf}` land.
    /// Created on demand.
    pub artifact_dir: PathBuf,
    /// Python interpreter for `aberp-cad-extract`. Absolute path is
    /// expected (the wrapper's venv gotcha; see S270 memory).
    pub python_bin: PathBuf,
    /// `ToleranceRange` default for v1. The storefront submission
    /// form does not yet collect a tolerance band, so the daemon
    /// quotes everything at `Standard` until the storefront's
    /// `/quote` page surfaces a picker.
    pub default_tolerance: ToleranceRange,
}

/// Shared daemon dependencies — same shape as
/// [`aberp_quote_intake::service::QuoteIntakeDeps`]. The audit
/// context lets every per-job append carry the same tenant + binary
/// hash + operator login the rest of ABERP's audit ledger uses.
#[derive(Debug, Clone)]
pub struct PricingPipelineDeps {
    pub db_path: PathBuf,
    pub tenant: TenantId,
    pub binary_hash: BinaryHash,
    pub operator_login: String,
}

/// The daemon struct. Holds the HTTP client, the audit context, and
/// the local config. Cheap to clone (`Arc`-style internal shape).
pub struct PricingPipelineService {
    config: PricingPipelineConfig,
    deps: PricingPipelineDeps,
    client: reqwest::Client,
    /// S430 / ADR-0083 — CAD-blob key + read-audit debounce. The write
    /// path encrypts downloaded CADs with `cad_blob.key`; the extract
    /// read path decrypts + emits `CadBlobRead` (debounced).
    cad_blob: CadBlobCtx,
}

impl std::fmt::Debug for PricingPipelineService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PricingPipelineService")
            .field("base_url", &self.config.base_url)
            .field("bearer", &"<redacted>")
            .field("poll_interval", &self.config.poll_interval)
            .finish()
    }
}

impl PricingPipelineService {
    pub fn new(
        config: PricingPipelineConfig,
        deps: PricingPipelineDeps,
        cad_blob: CadBlobCtx,
    ) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("build pricing-pipeline HTTP client")?;
        Ok(Self {
            config,
            deps,
            client,
            cad_blob,
        })
    }

    pub fn poll_interval(&self) -> Duration {
        self.config.poll_interval
    }

    /// One full sweep: storefront list → enqueue new rows → advance
    /// up to `MAX_JOBS_PER_CYCLE` non-terminal rows by one state.
    pub async fn poll_once(&self) -> PipelineCycleSummary {
        let started = Instant::now();
        let mut summary = PipelineCycleSummary::default();

        match self.list_and_enqueue_received().await {
            Ok((fetched, enqueued)) => {
                summary.fetched_from_storefront = fetched;
                summary.enqueued = enqueued;
            }
            Err(e) => {
                tracing::warn!(error = %e, "pricing-pipeline storefront list failed");
                summary.error = Some(format!("storefront list: {e:#}"));
                summary.elapsed_ms = started.elapsed().as_millis() as u64;
                return summary;
            }
        }

        for _ in 0..MAX_JOBS_PER_CYCLE {
            let row = match self.next_actionable_blocking().await {
                Ok(Some(r)) => r,
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!(error = %e, "pricing-pipeline next-job lookup failed");
                    summary.error = Some(format!("next_actionable: {e:#}"));
                    break;
                }
            };
            match self.advance_one_step(row).await {
                Ok(StepOutcome::Advanced) => summary.advanced += 1,
                Ok(StepOutcome::Posted) => {
                    summary.advanced += 1;
                    summary.posted += 1;
                }
                Ok(StepOutcome::Failed) => {
                    summary.advanced += 1;
                    summary.failed += 1;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "pricing-pipeline advance error");
                    summary.error = Some(format!("advance: {e:#}"));
                    break;
                }
            }
        }

        summary.elapsed_ms = started.elapsed().as_millis() as u64;
        summary
    }

    async fn list_and_enqueue_received(&self) -> Result<(u32, u32)> {
        let url = format!("{}/api/quotes?status=received", self.config.base_url);
        // S348 / PR-39 (F1) — classify the response by status + Content-Type
        // BEFORE parsing. A send/body-read failure (no HTTP response) or a
        // 200 `text/html` (CDN misroute serving the SPA shell) used to crash
        // `resp.json()` and abort the cycle with an opaque reason; now every
        // such case is a typed `WritebackOutcome`, audited as a
        // `quote.poll_outcome` row, and surfaced with a granular reason.
        let resp = match self
            .client
            .get(&url)
            .header("Authorization", self.bearer_header()?)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                let outcome = classify_send_error(&e);
                self.emit_poll_outcome_audit(&outcome).await;
                return Err(anyhow!("poll {url}: {}", outcome.failure_reason()));
            }
        };
        let status = resp.status().as_u16();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let body_text = match resp.text().await {
            Ok(t) => t,
            Err(e) => {
                let outcome = classify_send_error(&e);
                self.emit_poll_outcome_audit(&outcome).await;
                return Err(anyhow!("poll {url}: {}", outcome.failure_reason()));
            }
        };
        let parsed = match classify_poll_response(status, content_type.as_deref(), &body_text) {
            Ok(list) => list,
            Err(outcome) => {
                self.emit_poll_outcome_audit(&outcome).await;
                return Err(anyhow!("poll {url}: {}", outcome.failure_reason()));
            }
        };
        let fetched = parsed.quotes.len() as u32;
        let mut enqueued = 0u32;
        for quote in parsed.quotes {
            match self.enqueue_one(quote).await {
                Ok(true) => enqueued += 1,
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(error = %e, "pricing-pipeline enqueue failed");
                }
            }
        }
        Ok((fetched, enqueued))
    }

    /// Pull metadata + the first CAD file, save to artifact_dir,
    /// insert a `Fetched` job, and emit `QuotePricingFetched`. Returns
    /// `Ok(true)` when a fresh row was inserted, `Ok(false)` when the
    /// quote already had a pricing-jobs row (idempotent).
    async fn enqueue_one(&self, quote: StorefrontQuote) -> Result<bool> {
        let qid = &quote.id;
        // Pick the first .stl/.step file — v1 is single-CAD-per-quote
        // per the design doc; a multi-CAD quote will need explicit
        // operator routing.
        let cad = match quote.files.iter().find(|f| {
            let lower = f.filename.to_lowercase();
            lower.ends_with(".stl") || lower.ends_with(".step") || lower.ends_with(".stp")
        }) {
            Some(c) => c,
            None => {
                // S379 — the storefront listing returned this quote with no
                // CAD file (the pre-S368 redeploy wiped those files and they
                // are not coming back). The old code `warn!`-ed and bailed
                // here WITHOUT inserting a row, so the same quote_id re-fired
                // every 60s cycle forever (S376 phantom-retry loop). Classify
                // it as a PERMANENT enqueue failure: insert a `Failed` row +
                // audit pair so the row trips `insert_*`'s ON CONFLICT
                // idempotency guard and the next cycle SKIPS this quote_id.
                // Operator must explicitly Retry once the storefront supplies
                // a CAD again — we never auto-reset (conservative path).
                return self.enqueue_failed_no_cad(quote).await;
            }
        };

        // Download to artifact_dir/<id>/<filename>.
        let dest_dir = self.config.artifact_dir.join(qid);
        std::fs::create_dir_all(&dest_dir)
            .with_context(|| format!("mkdir {}", dest_dir.display()))?;
        let dest_path = dest_dir.join(&cad.filename);
        let file_url = format!(
            "{}/api/quotes/{}/files/{}",
            self.config.base_url, qid, cad.filename
        );
        let resp = self
            .client
            .get(&file_url)
            .header("Authorization", self.bearer_header()?)
            .send()
            .await
            .with_context(|| format!("GET {file_url}"))?;
        let s = resp.status();
        if !s.is_success() {
            return Err(anyhow!("CAD download {file_url} returned HTTP {s}"));
        }
        let body = resp.bytes().await.context("read CAD body")?;
        // S430 / ADR-0083 — encrypt at rest. New downloads never land in
        // the clear; the on-disk file is `MAGIC || nonce || ciphertext`.
        let encrypted = self
            .cad_blob
            .key
            .encrypt(&body)
            .context("encrypt downloaded CAD blob")?;
        std::fs::write(&dest_path, &encrypted)
            .with_context(|| format!("write {}", dest_path.display()))?;

        let material_grade = quote.request.material_preference.clone();
        let quantity = quote.request.quantity.unwrap_or(1).max(1) as u32;
        let customer_email = quote.contact.email.clone();
        let customer_name = quote.contact.name.clone();
        // S401 — carry the buyer's company through to the pricing-jobs row
        // so the operator panel can show who they're quoting at a glance.
        let customer_company = quote.contact.company.trim().to_string();

        let db_path = self.deps.db_path.clone();
        let tenant_id = self.deps.tenant.as_str().to_string();
        let quote_id = qid.clone();
        let filename = cad.filename.clone();
        let dest_path_str = dest_path.to_string_lossy().into_owned();
        let binary_hash = self.deps.binary_hash;
        let login = self.deps.operator_login.clone();

        let inserted = spawn_blocking(move || -> Result<bool> {
            let mut conn = duckdb::Connection::open(&db_path).context("open DB for enqueue")?;
            audit_ensure_schema(&conn).context("ensure audit-ledger schema")?;
            jobs::ensure_schema(&conn).context("ensure quote_pricing_jobs schema")?;
            let now = OffsetDateTime::now_utc();
            let inserted = jobs::insert_fetched_job(
                &conn,
                &quote_id,
                &tenant_id,
                &customer_email,
                &customer_name,
                &customer_company,
                &material_grade,
                quantity,
                &filename,
                &dest_path_str,
                now,
            )?;
            if !inserted {
                return Ok(false);
            }
            // Emit QuotePricingFetched in its own short tx — same posture as
            // the existing intake-daemon audit emits.
            let tx = conn.transaction().context("open fetched-audit tx")?;
            let meta =
                LedgerMeta::new(TenantId::new(&tenant_id).context("tenant_id")?, binary_hash);
            let actor = Actor::from_local_cli(Ulid::new().to_string(), &login);
            let payload = QuotePricingFetchedPayload {
                quote_id: quote_id.clone(),
                tenant_id: tenant_id.clone(),
                customer_email,
                material_grade,
                quantity,
                cad_filename: filename,
                cad_local_path: dest_path_str,
                actor: "system".to_string(),
                idempotency_key: format!("quote_pricing_fetched:{quote_id}"),
                fetched_at: now
                    .format(&Rfc3339)
                    .unwrap_or_else(|_| "unknown".to_string()),
            };
            let bytes = serde_json::to_vec(&payload).context("encode fetched payload")?;
            append_in_tx(
                &tx,
                &meta,
                EventKind::QuotePricingFetched,
                bytes,
                actor,
                Some(payload.idempotency_key.clone()),
            )
            .context("append QuotePricingFetched")?;
            tx.commit().context("commit fetched-audit")?;
            Ok(true)
        })
        .await
        .context("enqueue spawn_blocking join")??;

        if inserted {
            // Storefront customer-facing chip flips to `quoting` per
            // ADR-0004. Best-effort — a failure here logs but doesn't
            // abort the enqueue; the row is already in the local table
            // and the next cycle's POST-back will still drive the
            // customer-facing state to `quoted` directly.
            if let Err(e) = self
                .writeback_quoting_status(qid, "ABERP pricing pipeline picked up CAD")
                .await
            {
                tracing::warn!(quote_id = qid, error = %e, "writeback quoting status failed");
            }
        }
        Ok(inserted)
    }

    /// S379 — record a quote whose storefront listing carries no CAD file
    /// as a PERMANENT enqueue failure. Inserts ONE `Failed` row (idempotent
    /// via ON CONFLICT) + the `QuotePricingFailed` / `FailureClassified`
    /// audit pair on the first cycle; later cycles no-op (the row already
    /// exists), which is exactly what stops the S376 phantom-retry loop.
    ///
    /// Always returns `Ok(false)` — a no-CAD quote is NOT an enqueued
    /// pricing job, so it never bumps the cycle's `enqueued` count and never
    /// drives the customer-facing `quoting` writeback. The inner `inserted`
    /// flag only gates the one-time `warn!` so the log isn't spammed every
    /// cycle.
    async fn enqueue_failed_no_cad(&self, quote: StorefrontQuote) -> Result<bool> {
        let qid = quote.id.clone();
        let material_grade = quote.request.material_preference.clone();
        let quantity = quote.request.quantity.unwrap_or(1).max(1) as u32;
        let customer_email = quote.contact.email.clone();
        let customer_name = quote.contact.name.clone();
        // S401 — carry company onto the Failed row too, so the operator
        // panel identifies the buyer even when the listing had no CAD.
        let customer_company = quote.contact.company.trim().to_string();

        let db_path = self.deps.db_path.clone();
        let tenant_id = self.deps.tenant.as_str().to_string();
        let quote_id = qid.clone();
        let binary_hash = self.deps.binary_hash;
        let login = self.deps.operator_login.clone();
        // Stable strings the SPA's Failed-row renderer surfaces verbatim.
        let stage = "enqueue";
        let reason = "no CAD file on listing";

        let inserted = spawn_blocking(move || -> Result<bool> {
            let conn = duckdb::Connection::open(&db_path).context("open DB for enqueue-fail")?;
            audit_ensure_schema(&conn).context("ensure audit-ledger schema")?;
            jobs::ensure_schema(&conn).context("ensure quote_pricing_jobs schema")?;
            let now = OffsetDateTime::now_utc();
            let inserted = jobs::insert_failed_enqueue_job(
                &conn,
                &quote_id,
                &tenant_id,
                &customer_email,
                &customer_name,
                &customer_company,
                &material_grade,
                quantity,
                stage,
                reason,
                FailureKind::Permanent,
                now,
            )?;
            if !inserted {
                // Row already present from an earlier cycle — idempotent
                // skip, no second audit pair.
                return Ok(false);
            }
            let mut conn = conn;
            append_failure_audit_pair(
                &mut conn,
                &tenant_id,
                binary_hash,
                &login,
                &quote_id,
                stage,
                reason,
                FailureKind::Permanent,
                0,
            )?;
            Ok(true)
        })
        .await
        .context("enqueue-fail spawn_blocking join")??;

        if inserted {
            tracing::warn!(
                quote_id = %qid,
                "storefront listing has no CAD file; recorded permanent enqueue failure (operator Retry required)"
            );
        }
        Ok(false)
    }

    async fn writeback_quoting_status(&self, quote_id: &str, notes: &str) -> Result<()> {
        let url = resolved_writeback_url(&self.config.base_url, quote_id, "status");
        let body = serde_json::json!({ "status": "quoting", "notes": notes });
        let resp = self
            .client
            .post(&url)
            .header("Authorization", self.bearer_header()?)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        if !resp.status().is_success() {
            return Err(anyhow!("status writeback HTTP {}", resp.status()));
        }
        Ok(())
    }

    /// S348 / PR-39 (F1) — write ONE `quote.poll_outcome` audit row for a
    /// FAILED list-poll cycle, carrying the granular transport verdict
    /// (variant tag + http_status + content_type + body_excerpt + retryable).
    /// Fires ONLY on failure — a healthy cycle's success is already implied
    /// by the per-quote `QuotePricingFetched` rows, and emitting a row every
    /// idle cadence would reproduce exactly the audit-spam S335 throttled for
    /// `EmailOutboxFetched`. Best-effort: an audit-write failure is logged
    /// but never changes the cycle's already-decided failure outcome. Own DB
    /// connection + tx (the poll path has no surrounding `spawn_blocking`).
    async fn emit_poll_outcome_audit(&self, outcome: &WritebackOutcome) {
        let db_path = self.deps.db_path.clone();
        let tenant_id = self.deps.tenant.as_str().to_string();
        let binary_hash = self.deps.binary_hash;
        let login = self.deps.operator_login.clone();
        let tag = outcome.tag().to_string();
        let http_status = outcome.http_status();
        let content_type = outcome.content_type();
        let body_excerpt = outcome.body_excerpt().map(|s| s.to_string());
        let retryable = outcome.retryable();
        let res = spawn_blocking(move || -> Result<()> {
            let mut conn =
                duckdb::Connection::open(&db_path).context("open DB for poll-outcome audit")?;
            audit_ensure_schema(&conn).context("ensure audit-ledger schema")?;
            let tx = conn.transaction().context("open poll-outcome tx")?;
            let meta =
                LedgerMeta::new(TenantId::new(&tenant_id).context("tenant id")?, binary_hash);
            let actor = Actor::from_local_cli(Ulid::new().to_string(), &login);
            let idempotency_key = format!("quote_poll_outcome:{}", Ulid::new());
            let payload = QuotePollOutcomePayload {
                tenant_id: tenant_id.clone(),
                outcome: tag,
                http_status,
                content_type,
                body_excerpt,
                retryable,
                actor: "system".to_string(),
                idempotency_key: idempotency_key.clone(),
            };
            let bytes = serde_json::to_vec(&payload).context("encode poll-outcome payload")?;
            append_in_tx(
                &tx,
                &meta,
                EventKind::QuotePollOutcome,
                bytes,
                actor,
                Some(idempotency_key),
            )
            .context("append QuotePollOutcome")?;
            tx.commit().context("commit poll-outcome")?;
            Ok(())
        })
        .await;
        match res {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!(error = %e, "poll-outcome audit write failed"),
            Err(e) => tracing::warn!(error = %e, "poll-outcome audit task panicked"),
        }
    }

    async fn next_actionable_blocking(&self) -> Result<Option<PricingJobRow>> {
        let db_path = self.deps.db_path.clone();
        let tenant_id = self.deps.tenant.as_str().to_string();
        spawn_blocking(move || -> Result<Option<PricingJobRow>> {
            let conn = duckdb::Connection::open(&db_path).context("open DB for next-job")?;
            jobs::next_actionable_job(&conn, &tenant_id)
        })
        .await
        .context("next-actionable spawn_blocking join")?
    }

    /// Advance one job by one state. Returns whether the job reached
    /// terminal `Posted`, terminal `Failed`, or simply advanced.
    async fn advance_one_step(&self, row: PricingJobRow) -> Result<StepOutcome> {
        match row.state {
            JobState::Fetched | JobState::Extracting => self.advance_extract(row).await,
            JobState::Pricing => self.advance_price(row).await,
            JobState::Rendering => self.advance_render(row).await,
            JobState::PostingBack => self.advance_post(row).await,
            // Terminal states should never appear from
            // next_actionable_job (it filters to the in-flight states, and
            // S414's `archived` is excluded there too); if one does,
            // treat it as a no-op advance rather than panicking.
            JobState::Posted | JobState::Failed | JobState::Archived => Ok(StepOutcome::Advanced),
        }
    }

    async fn advance_extract(&self, row: PricingJobRow) -> Result<StepOutcome> {
        let db_path = self.deps.db_path.clone();
        let tenant_id_string = self.deps.tenant.as_str().to_string();
        let binary_hash = self.deps.binary_hash;
        let login = self.deps.operator_login.clone();
        let python_bin = self.config.python_bin.clone();
        let quote_id = row.quote_id.clone();
        let material_grade = row.material_grade.clone();
        let attempt_n = row.attempt_n;
        // S430 — clone the CAD-blob ctx (key + debounce) into the blocking
        // task so the read can decrypt + emit the read-audit.
        let cad_blob = self.cad_blob.clone();

        let outcome = spawn_blocking(move || -> Result<StepOutcome> {
            let mut conn = duckdb::Connection::open(&db_path).context("open DB for extract")?;
            audit_ensure_schema(&conn).context("audit schema")?;
            jobs::ensure_schema(&conn).context("jobs schema")?;
            // S286 hotfix: NotFound aborts this step, AlreadyInState
            // continues (the prior cycle marked us Extracting; safe to
            // re-run the extract because the audit emit is keyed on the
            // post-extract feature_graph_hash, not the pre-extract mark).
            match jobs::set_state(
                &conn,
                &quote_id,
                &tenant_id_string,
                JobState::Extracting,
                OffsetDateTime::now_utc(),
            )? {
                jobs::TransitionOutcome::NotFound => {
                    tracing::warn!(
                        quote_id = %quote_id,
                        "pricing-pipeline row vanished before extract; skipping"
                    );
                    return Ok(StepOutcome::Advanced);
                }
                jobs::TransitionOutcome::AlreadyInState | jobs::TransitionOutcome::Applied => {}
            }
            let arts = jobs::get_job_artifacts(&conn, &quote_id, &tenant_id_string)?;
            // S430 / ADR-0083 — the on-disk CAD is encrypted at rest, but
            // the Python extractor reads a file PATH (not bytes), so we
            // decrypt to a short-lived sibling temp file deleted on drop.
            let on_disk = std::fs::read(&arts.cad_local_path)
                .with_context(|| format!("read CAD blob {}", arts.cad_local_path))?;
            let opened = match cad_blob.key.open(&on_disk) {
                Ok(o) => o,
                Err(e) => {
                    // Tampered blob / wrong key: a customer-visible failure,
                    // not a daemon crash. Mark the job Failed (the SPA shows
                    // a red error chip with the reason) and move on.
                    emit_failure(
                        &mut conn,
                        &tenant_id_string,
                        binary_hash,
                        &login,
                        &quote_id,
                        "decrypt",
                        &format!("CAD blob decryption failed: {e}"),
                        attempt_n,
                    )?;
                    return Ok(StepOutcome::Failed);
                }
            };
            // The daemon reads on behalf of the quote engine to (re-)price.
            let requester = login.clone();
            if opened.was_legacy_plaintext {
                crate::cad_blob::emit_legacy_plaintext_read(
                    &mut conn,
                    &tenant_id_string,
                    binary_hash,
                    &login,
                    &quote_id,
                    &requester,
                )?;
            }
            if cad_blob
                .debounce
                .should_emit(&requester, &quote_id, Instant::now())
            {
                crate::cad_blob::emit_blob_read(
                    &mut conn,
                    &tenant_id_string,
                    binary_hash,
                    &login,
                    &quote_id,
                    &requester,
                    ReadPurpose::Reprice,
                )?;
            }
            let temp = DecryptedTempFile::write_beside(
                Path::new(&arts.cad_local_path),
                &opened.plaintext,
            )?;
            let extractor = CadExtractor::new().with_python_bin(python_bin);
            let req = ExtractRequest {
                input_path: temp.path().to_path_buf(),
                material_grade: material_grade.clone(),
            };
            match extractor.extract(&req) {
                Ok(graph) => {
                    let canonical =
                        serde_json::to_vec(&graph).context("encode FeatureGraph for hash")?;
                    let hash = blake3::hash(&canonical);
                    let hash_str = format!("blake3:{}", hash.to_hex());
                    let json =
                        String::from_utf8(canonical).context("FeatureGraph json was not utf8")?;
                    let set_outcome = jobs::set_extracted(
                        &mut conn,
                        &quote_id,
                        &tenant_id_string,
                        &hash_str,
                        &json,
                        OffsetDateTime::now_utc(),
                    )?;
                    if !matches!(set_outcome, jobs::TransitionOutcome::Applied) {
                        // Already-at-Pricing (prior cycle won the race) or
                        // NotFound — skip audit emit so we don't double-fire.
                        tracing::info!(
                            quote_id = %quote_id,
                            outcome = ?set_outcome,
                            "set_extracted no-op; skipping audit emit"
                        );
                        return Ok(StepOutcome::Advanced);
                    }
                    let tx = conn.transaction().context("open extract-audit tx")?;
                    let meta = LedgerMeta::new(
                        TenantId::new(&tenant_id_string).context("tenant id")?,
                        binary_hash,
                    );
                    let actor = Actor::from_local_cli(Ulid::new().to_string(), &login);
                    let payload = QuotePricingExtractedPayload {
                        quote_id: quote_id.clone(),
                        tenant_id: tenant_id_string.clone(),
                        feature_graph_hash: hash_str,
                        extractor_version: aberp_cad_extract_wrapper::WRAPPER_VERSION.to_string(),
                        volume_mm3: graph.volume_mm3,
                        bounding_box_mm: graph.bounding_box_mm,
                        feature_count: graph.features.len() as u32,
                        requires_5_axis: graph.requires_5_axis,
                        thin_wall_present: graph.thin_wall_present,
                        actor: "system".to_string(),
                        idempotency_key: format!("quote_pricing_extracted:{quote_id}"),
                    };
                    let bytes = serde_json::to_vec(&payload).context("encode extracted")?;
                    append_in_tx(
                        &tx,
                        &meta,
                        EventKind::QuotePricingExtracted,
                        bytes,
                        actor,
                        Some(payload.idempotency_key.clone()),
                    )
                    .context("append extracted")?;
                    tx.commit().context("commit extract-audit")?;
                    Ok(StepOutcome::Advanced)
                }
                Err(e) => {
                    let reason = e.to_string();
                    emit_failure(
                        &mut conn,
                        &tenant_id_string,
                        binary_hash,
                        &login,
                        &quote_id,
                        "extract",
                        &reason,
                        row.attempt_n,
                    )?;
                    Ok(StepOutcome::Failed)
                }
            }
        })
        .await
        .context("extract spawn_blocking join")??;
        Ok(outcome)
    }

    async fn advance_price(&self, row: PricingJobRow) -> Result<StepOutcome> {
        let db_path = self.deps.db_path.clone();
        let tenant_id_string = self.deps.tenant.as_str().to_string();
        let binary_hash = self.deps.binary_hash;
        let login = self.deps.operator_login.clone();
        let quote_id = row.quote_id.clone();
        let qty = row.quantity.max(1);
        let target_tol = self.config.default_tolerance;

        let outcome = spawn_blocking(move || -> Result<StepOutcome> {
            let mut conn = duckdb::Connection::open(&db_path).context("open DB for price")?;
            audit_ensure_schema(&conn).context("audit schema")?;
            jobs::ensure_schema(&conn).context("jobs schema")?;
            let arts = jobs::get_job_artifacts(&conn, &quote_id, &tenant_id_string)?;
            let graph_json = arts
                .feature_graph_json
                .ok_or_else(|| anyhow!("Pricing state but no feature_graph_json"))?;
            let mut graph: FeatureGraph =
                serde_json::from_str(&graph_json).context("decode FeatureGraph")?;

            // Catalogue snapshot — read all four tables synchronously
            // inside the blocking task. Per-pricing-pass to honor live
            // tunables edits; the snapshot is cheap (under 100 rows
            // across all four tables in v1).
            let materials = crate::quoting_materials::list_materials(&conn, &tenant_id_string)
                .context("list_materials")?;
            let complexity =
                crate::quoting_tunables::list_complexity_rules(&conn, &tenant_id_string)
                    .context("list_complexity_rules")?;
            let tolerance =
                crate::quoting_tunables::list_tolerance_multipliers(&conn, &tenant_id_string)
                    .context("list_tolerance_multipliers")?;
            let stock_adjustments =
                crate::quoting_tunables::list_stock_adjustments(&conn, &tenant_id_string)
                    .context("list_stock_adjustments")?;
            let params = crate::quoting_tunables::get_parameters(&conn, &tenant_id_string)
                .context("get_parameters")?;

            let engine_materials = convert_materials(&materials)?;
            let engine_complexity = convert_complexity(&complexity);
            let engine_tolerance = convert_tolerance(&tolerance);
            let engine_stock_adjustments = convert_stock_adjustments(&stock_adjustments);
            let mut engine_params = convert_parameters(&params);

            // S428 — resolve the customer-type margin policy: the buyer
            // partner's customer_type → active profile (or operator
            // override) overrides the global markup + floor. The engine
            // stays pure; we only swap the knobs we feed it.
            let (
                buyer_partner_id,
                margin_override_pct,
                operator_form,
                machine_family_override,
                gear_ops_json,
            ) = match jobs::get_job_detail(&conn, &quote_id, &tenant_id_string)? {
                Some(d) => (
                    d.buyer_partner_id,
                    d.margin_override_pct,
                    operator_stock_form(
                        d.stock_form.as_deref(),
                        d.stock_od_mm,
                        d.stock_id_mm,
                        d.stock_length_mm,
                    ),
                    d.machine_family_override,
                    d.gear_ops_json,
                ),
                None => (None, None, None, None, None),
            };
            // ADR-0094 Gap 1 (S2 wiring) — stamp the chosen stock form onto
            // the graph BEFORE the engine call (now `quote_with_shop_model`,
            // S4 Gap 2). Precedence:
            // operator field > extractor hint (already on the graph) >
            // RectangularBlock (the serde default). An unset operator field
            // leaves the graph's form untouched, so an existing quote with
            // no stock form prices byte-identically to today.
            let (chosen_stock_form, stock_form_source) =
                stamp_stock_form(&mut graph, operator_form);
            let customer_type = crate::quote_margin::customer_type_for_partner(
                &conn,
                &tenant_id_string,
                buyer_partner_id.as_deref(),
            )?;
            let profile = if customer_type == crate::partners::CustomerType::Unset {
                None
            } else {
                crate::margin_profiles::active_profile_for_customer_type(
                    &conn,
                    &tenant_id_string,
                    customer_type,
                )?
            };
            let margin_policy = crate::quote_margin::MarginPolicy::resolve(
                profile.as_ref(),
                margin_override_pct,
                engine_params.profit_margin_base,
                engine_params.min_margin,
            );
            margin_policy.apply(&mut engine_params);

            // S429 — materialize the closed-loop calibration table fresh on
            // every quote-create (cheap query over the samples) and feed it to
            // the engine so the routed family's machining estimate is scaled by
            // what past jobs actually took.
            let cal_table = crate::quote_calibration::materialize_table(&conn, &tenant_id_string)
                .context("materialize calibration table")?;

            // ── ADR-0094 Gap 2 (S4 wiring) — machine-family rates ────────
            // Load the operator's rate table, route the part to a family by
            // geometry (the same `route_family` the engine uses internally),
            // and apply any per-quote operator override by re-labelling the
            // chosen rate row (see `apply_family_override`). An empty/absent
            // table ⇒ the engine keeps the global flat rate (price unchanged).
            let machine_rate_rows =
                crate::quoting_machine_rates::list_machine_rates(&conn, &tenant_id_string)
                    .context("load machine rates")?;
            let base_machine_rates = convert_machine_rates(&machine_rate_rows);
            let geometry_family = engine::route_family(
                chosen_stock_form,
                graph.requires_5_axis,
                stock_od_mm(chosen_stock_form),
                &engine_params,
            );
            let override_family = machine_family_override
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .and_then(MachineFamily::from_db_str);
            let effective_family = override_family.unwrap_or(geometry_family);
            let engine_machine_rates =
                apply_family_override(&base_machine_rates, geometry_family, effective_family);

            // ── ADR-0094 Gap 3 (S6 wiring) — gear-generation ops ─────────
            // Stamp the operator's gear ops onto the graph (precedence
            // operator > extractor hint > empty default; an unset column
            // leaves the empty serde-default ⇒ no gear cost) and snapshot the
            // operator's gear-process coefficient table into the engine's
            // CatalogueSnapshot. Empty either way ⇒ the engine never enters the
            // gear path ⇒ price byte-identical to pre-Gap-3.
            let gear_ops_source = stamp_gear_ops(
                &mut graph,
                operator_gear_ops(gear_ops_json.as_deref()).context("decode operator gear ops")?,
            );
            let gear_process_rows =
                crate::quoting_gear_processes::list_gear_processes(&conn, &tenant_id_string)
                    .context("load gear processes")?;
            let engine_gear_process_rates = convert_gear_process_rates(&gear_process_rows);

            match engine::quote_with_catalogue(
                &graph,
                &engine::CatalogueSnapshot {
                    materials: &engine_materials,
                    complexity_rules: &engine_complexity,
                    tolerance_multipliers: &engine_tolerance,
                    stock_adjustments: &engine_stock_adjustments,
                    machine_rates: &engine_machine_rates,
                    gear_process_rates: &engine_gear_process_rates,
                },
                &engine_params,
                qty,
                target_tol,
                &cal_table,
            ) {
                Ok(breakdown) => {
                    let json = serde_json::to_string(&breakdown).context("encode breakdown")?;
                    let set_outcome = jobs::set_priced(
                        &mut conn,
                        &quote_id,
                        &tenant_id_string,
                        &json,
                        breakdown.total_price,
                        OffsetDateTime::now_utc(),
                    )?;
                    if !matches!(set_outcome, jobs::TransitionOutcome::Applied) {
                        tracing::info!(
                            quote_id = %quote_id,
                            outcome = ?set_outcome,
                            "set_priced no-op; skipping audit emit"
                        );
                        return Ok(StepOutcome::Advanced);
                    }

                    // S428 — record the margin-floor verdict alongside the
                    // priced breakdown so the operator banner + the DEAL
                    // saga's hard block read one source of truth.
                    let below_floor =
                        margin_policy.is_below_floor(breakdown.margin, breakdown.total_price);
                    let stored_floor_pct = if margin_policy.pipeline_enforced {
                        Some(margin_policy.floor_pct)
                    } else {
                        None
                    };
                    jobs::set_margin_result(
                        &conn,
                        &quote_id,
                        &tenant_id_string,
                        &json,
                        breakdown.total_price,
                        below_floor,
                        stored_floor_pct,
                        OffsetDateTime::now_utc(),
                    )?;

                    // S427 — capacity-aware lead-time. Computed after the
                    // price commits; the row is now `Rendering`, so the
                    // 30-day `Posted` shop-load sum correctly excludes it.
                    {
                        use std::sync::atomic::{AtomicBool, Ordering};
                        // Once-per-server-start guard for the empty-machine
                        // fallback notice (re-arms on each process restart).
                        static EMPTY_FALLBACK_EMITTED: AtomicBool = AtomicBool::new(false);

                        let now = OffsetDateTime::now_utc();
                        let since = (now - time::Duration::days(30))
                            .format(&Rfc3339)
                            .context("format lead-time window")?;
                        let machines = crate::quoting_machines::list_enabled_capacities(
                            &conn,
                            &tenant_id_string,
                        )
                        .context("load machine capacities")?;
                        let existing = jobs::sum_posted_machining_hours_by_family(
                            &conn,
                            &tenant_id_string,
                            &since,
                            &quote_id,
                        )?;
                        let mut new_hours = std::collections::BTreeMap::new();
                        // machining_minutes is per part; the batch occupies
                        // the machine for all `qty` parts.
                        let proj_h = (breakdown.machining_minutes / 60.0 * (qty as f64)).max(0.0);
                        if proj_h > 0.0 {
                            new_hours.insert(
                                aberp_quote_engine::MachineFamily::for_route(
                                    breakdown.route_to_5_axis,
                                ),
                                proj_h,
                            );
                        }
                        let est =
                            aberp_quote_engine::lead_time_days(&machines, &existing, &new_hours);
                        jobs::set_computed_lead_time(
                            &conn,
                            &quote_id,
                            &tenant_id_string,
                            est.days,
                        )?;

                        if est.used_fallback
                            && EMPTY_FALLBACK_EMITTED
                                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                                .is_ok()
                        {
                            let tx = conn.transaction().context("open empty-fallback audit tx")?;
                            let meta = LedgerMeta::new(
                                TenantId::new(&tenant_id_string).context("tenant id")?,
                                binary_hash,
                            );
                            let actor = Actor::from_local_cli(Ulid::new().to_string(), &login);
                            let payload =
                                crate::audit_payloads::QuotingMachinesEmptyFallbackPayload {
                                    fallback_daily_hours: aberp_quote_engine::FALLBACK_DAILY_HOURS,
                                    fallback_buffer_pct: aberp_quote_engine::FALLBACK_BUFFER_PCT,
                                    observed_at: now
                                        .format(&Rfc3339)
                                        .context("format observed_at")?,
                                };
                            // No idempotency key: we WANT it to re-emit on
                            // each fresh server start (the atomic gates
                            // within a run).
                            append_in_tx(
                                &tx,
                                &meta,
                                EventKind::QuotingMachinesEmptyFallback,
                                payload.to_bytes(),
                                actor,
                                None,
                            )
                            .context("append empty-fallback")?;
                            tx.commit().context("commit empty-fallback")?;
                        }
                    }

                    let tx = conn.transaction().context("open priced-audit tx")?;
                    let meta = LedgerMeta::new(
                        TenantId::new(&tenant_id_string).context("tenant id")?,
                        binary_hash,
                    );
                    let actor = Actor::from_local_cli(Ulid::new().to_string(), &login);
                    let payload = QuotePricingPricedPayload {
                        quote_id: quote_id.clone(),
                        tenant_id: tenant_id_string.clone(),
                        engine_version: breakdown.engine_version.clone(),
                        total_price_eur: breakdown.total_price,
                        material_cost_eur: breakdown.material_cost,
                        machining_cost_eur: breakdown.machining_cost,
                        cad_cam_cost_eur: breakdown.cad_cam_cost,
                        setup_cost_eur: breakdown.setup_cost,
                        overhead_eur: breakdown.overhead,
                        margin_eur: breakdown.margin,
                        stock_form: chosen_stock_form,
                        stock_form_source: stock_form_source.to_string(),
                        routed_machine_family: Some(geometry_family.as_db_str().to_string()),
                        machine_family_override: override_family.map(|f| f.as_db_str().to_string()),
                        effective_machine_family: Some(effective_family.as_db_str().to_string()),
                        machine_rate_snapshot: engine_machine_rates
                            .iter()
                            .map(|r| MachineRateAudit {
                                family: r.family.clone(),
                                attended_rate_eur_per_min: r.attended_rate_eur_per_min,
                                lights_out_factor: r.lights_out_factor,
                                unattended_capable: r.unattended_capable,
                            })
                            .collect(),
                        gear_cost_eur: breakdown.gear_cost,
                        gear_ops_source: gear_ops_source.to_string(),
                        gear_ops_snapshot: graph
                            .gears
                            .iter()
                            .map(|g| {
                                // Record the engine's resolved process (Auto →
                                // concrete) using the SAME public selector the
                                // engine uses, keyed on the effective family.
                                let resolved = match g.process {
                                    GearProcess::Auto => engine::select_gear_process(
                                        g.kind,
                                        effective_family,
                                        g.quality_agma,
                                    ),
                                    other => other,
                                };
                                GearOpAudit {
                                    kind: g.kind.as_db_str().to_string(),
                                    module_mm: g.module_mm,
                                    teeth: g.teeth,
                                    face_width_mm: g.face_width_mm,
                                    quality_agma: g.quality_agma,
                                    requested_process: g.process.as_db_str().to_string(),
                                    resolved_process: resolved.as_db_str().to_string(),
                                }
                            })
                            .collect(),
                        gear_process_rate_snapshot: engine_gear_process_rates
                            .iter()
                            .map(|r| GearProcessAudit {
                                process: r.process.clone(),
                                setup_min: r.setup_min,
                                min_per_tooth: r.min_per_tooth,
                                module_exponent: r.module_exponent,
                                agma_quality_factor_base: r.agma_quality_factor_base,
                                in_cycle_factor: r.in_cycle_factor,
                            })
                            .collect(),
                        actor: "system".to_string(),
                        idempotency_key: format!("quote_pricing_priced:{quote_id}"),
                    };
                    let bytes = serde_json::to_vec(&payload).context("encode priced")?;
                    append_in_tx(
                        &tx,
                        &meta,
                        EventKind::QuotePricingPriced,
                        bytes,
                        actor,
                        Some(payload.idempotency_key.clone()),
                    )
                    .context("append priced")?;

                    // S429 — record which calibration coefficient was applied,
                    // with the set hash, for reproducibility. Emitted only when
                    // a coefficient actually moved the price (non-neutral); a
                    // neutral set is the identity and carries no provenance.
                    if (breakdown.calibration_coefficient - 1.0).abs() > f64::EPSILON {
                        let cal_family =
                            aberp_quote_engine::MachineFamily::for_route(breakdown.route_to_5_axis);
                        let cal_actor = Actor::from_local_cli(Ulid::new().to_string(), &login);
                        let cal_payload = crate::audit_payloads::QuoteCalibrationAppliedPayload {
                            quote_id: quote_id.clone(),
                            machine_family: cal_family.as_db_str().to_string(),
                            coefficient: breakdown.calibration_coefficient,
                            coefficient_set_hash: cal_table.set_hash(),
                        };
                        append_in_tx(
                            &tx,
                            &meta,
                            EventKind::QuoteCalibrationApplied,
                            cal_payload.to_bytes(),
                            cal_actor,
                            Some(format!("quote_calibration_applied:{quote_id}")),
                        )
                        .context("append calibration applied")?;
                    }

                    // S428 — margin-policy provenance + floor breach.
                    if matches!(
                        margin_policy.source,
                        crate::quote_margin::MarginSource::Global
                    ) {
                        let payload = crate::audit_payloads::QuoteUsingGlobalMarginPayload {
                            quote_id: quote_id.clone(),
                            global_margin_base: margin_policy.applied_margin_base,
                        };
                        append_in_tx(
                            &tx,
                            &meta,
                            EventKind::QuoteUsingGlobalMargin,
                            payload.to_bytes(),
                            Actor::from_local_cli(Ulid::new().to_string(), &login),
                            Some(format!("quote_using_global_margin:{quote_id}")),
                        )
                        .context("append using-global-margin")?;
                    }
                    if below_floor {
                        let payload = crate::audit_payloads::QuoteMarginBelowFloorPayload {
                            quote_id: quote_id.clone(),
                            realized_margin_pct:
                                crate::quote_margin::MarginPolicy::realized_margin_pct(
                                    breakdown.margin,
                                    breakdown.total_price,
                                ),
                            floor_pct: margin_policy.floor_pct,
                        };
                        append_in_tx(
                            &tx,
                            &meta,
                            EventKind::QuoteMarginBelowFloor,
                            payload.to_bytes(),
                            Actor::from_local_cli(Ulid::new().to_string(), &login),
                            Some(format!("quote_margin_below_floor:{quote_id}")),
                        )
                        .context("append margin-below-floor")?;
                    }

                    tx.commit().context("commit priced-audit")?;
                    Ok(StepOutcome::Advanced)
                }
                Err(e) => {
                    let reason = e.to_string();
                    emit_failure(
                        &mut conn,
                        &tenant_id_string,
                        binary_hash,
                        &login,
                        &quote_id,
                        "price",
                        &reason,
                        row.attempt_n,
                    )?;
                    Ok(StepOutcome::Failed)
                }
            }
        })
        .await
        .context("price spawn_blocking join")??;
        Ok(outcome)
    }

    async fn advance_render(&self, row: PricingJobRow) -> Result<StepOutcome> {
        let db_path = self.deps.db_path.clone();
        let tenant_id_string = self.deps.tenant.as_str().to_string();
        let binary_hash = self.deps.binary_hash;
        let login = self.deps.operator_login.clone();
        let artifact_dir = self.config.artifact_dir.clone();
        let quote_id = row.quote_id.clone();
        let customer_email = row.customer_email.clone();
        let customer_name = row.customer_name.clone();
        // S401 — the row now carries the buyer's company; feed it into the
        // quote PDF (was hard-coded "" before this column existed).
        let customer_company = row.customer_company.clone().unwrap_or_default();
        let qty = row.quantity.max(1);
        let target_tol = self.config.default_tolerance;

        let outcome = spawn_blocking(move || -> Result<StepOutcome> {
            let mut conn = duckdb::Connection::open(&db_path).context("open DB for render")?;
            audit_ensure_schema(&conn).context("audit schema")?;
            jobs::ensure_schema(&conn).context("jobs schema")?;
            let arts = jobs::get_job_artifacts(&conn, &quote_id, &tenant_id_string)?;
            let graph: FeatureGraph = serde_json::from_str(
                arts.feature_graph_json
                    .as_deref()
                    .ok_or_else(|| anyhow!("Rendering state but no feature_graph_json"))?,
            )
            .context("decode FeatureGraph for render")?;
            let breakdown: QuoteBreakdown = serde_json::from_str(
                arts.breakdown_json
                    .as_deref()
                    .ok_or_else(|| anyhow!("Rendering state but no breakdown_json"))?,
            )
            .context("decode QuoteBreakdown for render")?;
            let valid_until = (OffsetDateTime::now_utc().date()
                + time::Duration::days(DEFAULT_VALID_UNTIL_DAYS))
            .format(&time::format_description::parse("[year]-[month]-[day]").expect("valid"))
            .context("format valid_until")?;
            let inputs = QuoteInputs {
                quote_id: &quote_id,
                customer_email: &customer_email,
                customer_name: &customer_name,
                customer_company: &customer_company,
                quantity: qty,
                notes: "",
                valid_until_iso: &valid_until,
                extractor_version: aberp_cad_extract_wrapper::WRAPPER_VERSION,
                engine_version: &breakdown.engine_version,
                feature_graph: &graph,
                breakdown: &breakdown,
                target_tolerance: target_tol,
                // First-render is always pre-acceptance, so the EVE
                // addendum-2 stock downgrade cannot have happened yet —
                // `stock_alert` is necessarily false here. The banner is
                // only reached via a post-acceptance re-render+re-post,
                // which is BLOCKED storefront-side today (the `/priced`
                // endpoint no-ops a same-hash re-post). Tracked in
                // docs/findings/s318-customer-pdf-stock-banner.md.
                stock_alert: false,
                // S427 — effective lead-time (override ?? computed).
                lead_time_days: jobs::get_effective_lead_time_days(
                    &conn,
                    &quote_id,
                    &tenant_id_string,
                )?,
            };
            match aberp_quote_pdf::render(&inputs) {
                Ok(bytes) => {
                    let dest_dir = artifact_dir.join(&quote_id);
                    std::fs::create_dir_all(&dest_dir).context("mkdir pdf dest")?;
                    let pdf_path = dest_dir.join("priced.pdf");
                    std::fs::write(&pdf_path, &bytes).context("write priced.pdf")?;
                    let pdf_path_str = pdf_path.to_string_lossy().into_owned();
                    let pdf_size = bytes.len() as u64;
                    let set_outcome = jobs::set_rendered(
                        &mut conn,
                        &quote_id,
                        &tenant_id_string,
                        &pdf_path_str,
                        &valid_until,
                        OffsetDateTime::now_utc(),
                    )?;
                    if !matches!(set_outcome, jobs::TransitionOutcome::Applied) {
                        tracing::info!(
                            quote_id = %quote_id,
                            outcome = ?set_outcome,
                            "set_rendered no-op; skipping audit emit"
                        );
                        return Ok(StepOutcome::Advanced);
                    }
                    let tx = conn.transaction().context("open rendered-audit tx")?;
                    let meta = LedgerMeta::new(
                        TenantId::new(&tenant_id_string).context("tenant id")?,
                        binary_hash,
                    );
                    let actor = Actor::from_local_cli(Ulid::new().to_string(), &login);
                    let payload = QuotePricingRenderedPayload {
                        quote_id: quote_id.clone(),
                        tenant_id: tenant_id_string.clone(),
                        pdf_path: pdf_path_str,
                        pdf_size_bytes: pdf_size,
                        pdf_renderer_version: aberp_quote_pdf::QUOTE_PDF_RENDERER_VERSION
                            .to_string(),
                        actor: "system".to_string(),
                        idempotency_key: format!("quote_pricing_rendered:{quote_id}"),
                    };
                    let bytes_payload = serde_json::to_vec(&payload).context("encode rendered")?;
                    append_in_tx(
                        &tx,
                        &meta,
                        EventKind::QuotePricingRendered,
                        bytes_payload,
                        actor,
                        Some(payload.idempotency_key.clone()),
                    )
                    .context("append rendered")?;
                    tx.commit().context("commit rendered-audit")?;
                    Ok(StepOutcome::Advanced)
                }
                Err(e) => {
                    let reason = e.to_string();
                    emit_failure(
                        &mut conn,
                        &tenant_id_string,
                        binary_hash,
                        &login,
                        &quote_id,
                        "render",
                        &reason,
                        row.attempt_n,
                    )?;
                    Ok(StepOutcome::Failed)
                }
            }
        })
        .await
        .context("render spawn_blocking join")??;
        Ok(outcome)
    }

    async fn advance_post(&self, row: PricingJobRow) -> Result<StepOutcome> {
        // Read row artifacts off-thread, then do the async HTTP POST
        // on the main runtime.
        let db_path = self.deps.db_path.clone();
        let tenant_id_string = self.deps.tenant.as_str().to_string();
        let quote_id = row.quote_id.clone();
        let read_arts = {
            let qid = quote_id.clone();
            let tid = tenant_id_string.clone();
            let db = db_path.clone();
            spawn_blocking(move || -> Result<(jobs::JobArtifacts, String, String)> {
                let conn = duckdb::Connection::open(&db).context("open DB for post")?;
                let arts = jobs::get_job_artifacts(&conn, &qid, &tid)?;
                // Re-fetch the persisted breakdown + hash from the row.
                let mut stmt = conn.prepare(
                    "SELECT feature_graph_hash, valid_until_iso
                        FROM quote_pricing_jobs WHERE quote_id = ? AND tenant_id = ?",
                )?;
                let mut rows = stmt.query(duckdb::params![qid, tid])?;
                let r = rows.next()?.ok_or_else(|| anyhow!("no row for post"))?;
                let hash: String = r.get(0)?;
                let valid_until: String = r.get(1)?;
                Ok((arts, hash, valid_until))
            })
            .await
            .context("post-read spawn_blocking join")??
        };
        let (arts, hash, valid_until) = read_arts;
        let pdf_path = arts
            .pdf_path
            .ok_or_else(|| anyhow!("PostingBack state but no pdf_path"))?;
        let breakdown_json = arts
            .breakdown_json
            .ok_or_else(|| anyhow!("PostingBack state but no breakdown_json"))?;
        let pdf_bytes = std::fs::read(&pdf_path).with_context(|| format!("read pdf {pdf_path}"))?;

        let post_result = self
            .post_priced_writeback(
                &quote_id,
                &hash,
                &valid_until,
                &breakdown_json,
                pdf_bytes.as_slice(),
            )
            .await;

        let db_path = self.deps.db_path.clone();
        let tenant_id_string = self.deps.tenant.as_str().to_string();
        let binary_hash = self.deps.binary_hash;
        let login = self.deps.operator_login.clone();
        let quote_id_persist = quote_id.clone();
        let attempt_n = row.attempt_n;

        let final_outcome = spawn_blocking(move || -> Result<StepOutcome> {
            let mut conn = duckdb::Connection::open(&db_path).context("open DB for post-finish")?;
            audit_ensure_schema(&conn).context("audit schema")?;
            jobs::ensure_schema(&conn).context("jobs schema")?;

            // An internal pre-send error (multipart build / bearer header)
            // never reached the wire — there's no transport verdict to
            // record. Fail the job through the generic path and stop.
            let outcome = match post_result {
                Ok(o) => o,
                Err(e) => {
                    let reason = format!("post pre-send error: {e:#}");
                    emit_failure(
                        &mut conn,
                        &tenant_id_string,
                        binary_hash,
                        &login,
                        &quote_id_persist,
                        "post",
                        &reason,
                        attempt_n,
                    )?;
                    return Ok(StepOutcome::Failed);
                }
            };

            // S347 / PR-39 — record the granular transport verdict for EVERY
            // attempt (success + failure), before mutating job state, so the
            // forensic walker sees the full priced-writeback delivery trail.
            emit_priced_writeback_outcome(
                &mut conn,
                &tenant_id_string,
                binary_hash,
                &login,
                &quote_id_persist,
                &outcome,
                attempt_n,
            )?;

            if let WritebackOutcome::Success { idempotent } = outcome {
                let set_outcome = jobs::set_state(
                    &conn,
                    &quote_id_persist,
                    &tenant_id_string,
                    JobState::Posted,
                    OffsetDateTime::now_utc(),
                )?;
                if !matches!(set_outcome, jobs::TransitionOutcome::Applied) {
                    // Already-Posted (prior cycle's storefront-replay
                    // landed) or NotFound. The storefront has the PDF;
                    // skip the posted-audit emit to avoid double-firing.
                    tracing::info!(
                        quote_id = %quote_id_persist,
                        outcome = ?set_outcome,
                        "set_state(Posted) no-op; skipping audit emit"
                    );
                    return Ok(StepOutcome::Posted);
                }
                let tx = conn.transaction().context("open posted-audit tx")?;
                let meta = LedgerMeta::new(
                    TenantId::new(&tenant_id_string).context("tenant id")?,
                    binary_hash,
                );
                let actor = Actor::from_local_cli(Ulid::new().to_string(), &login);
                let payload = QuotePricingPostedPayload {
                    quote_id: quote_id_persist.clone(),
                    tenant_id: tenant_id_string.clone(),
                    feature_graph_hash: hash.clone(),
                    idempotent,
                    valid_until_iso: valid_until.clone(),
                    actor: "system".to_string(),
                    idempotency_key: format!("quote_pricing_posted:{quote_id_persist}"),
                };
                let bytes_payload = serde_json::to_vec(&payload).context("encode posted")?;
                append_in_tx(
                    &tx,
                    &meta,
                    EventKind::QuotePricingPosted,
                    bytes_payload,
                    actor,
                    Some(payload.idempotency_key.clone()),
                )
                .context("append posted")?;
                tx.commit().context("commit posted-audit")?;
                Ok(StepOutcome::Posted)
            } else {
                // Typed failure — the reason embeds the stable `writeback:`
                // tag so `classify_failure` maps it by-variant (never the
                // old `? Ismeretlen / Unknown` catch-all).
                let reason = outcome.failure_reason();
                emit_failure(
                    &mut conn,
                    &tenant_id_string,
                    binary_hash,
                    &login,
                    &quote_id_persist,
                    "post",
                    &reason,
                    attempt_n,
                )?;
                Ok(StepOutcome::Failed)
            }
        })
        .await
        .context("post-finish spawn_blocking join")??;
        Ok(final_outcome)
    }

    /// Hand-rolled multipart POST against `/api/quotes/{id}/priced`
    /// per ADR-0004's contract. Hand-rolled rather than feature-
    /// flagging reqwest's `multipart` to avoid bloating the global
    /// HTTP-client feature surface for this one caller.
    /// S347 / PR-39 (F1+F2) — returns a typed [`WritebackOutcome`] instead
    /// of a stringly `Result`. The old code parsed `resp.text()` as JSON
    /// unconditionally, so a 200 `text/html` (the CDN serving the SPA shell
    /// instead of the API — the 2026-06-11 incident) logged
    /// `parse priced-writeback ok JSON: <!doctype html>…` and fell through
    /// to the `? Ismeretlen / Unknown` bucket. Now every response is
    /// classified by status + Content-Type at the source; a transport-vs-app
    /// verdict travels back to the caller. `Err` is reserved for the
    /// internal pre-send failures (multipart build / bearer header) that are
    /// config bugs, not a writeback verdict.
    async fn post_priced_writeback(
        &self,
        quote_id: &str,
        feature_graph_hash: &str,
        valid_until_iso: &str,
        breakdown_json: &str,
        pdf_bytes: &[u8],
    ) -> Result<WritebackOutcome> {
        let url = resolved_writeback_url(&self.config.base_url, quote_id, "priced");
        let boundary = format!("aberp-mp-{}", Ulid::new());
        let body = build_priced_multipart(
            &boundary,
            feature_graph_hash,
            valid_until_iso,
            breakdown_json,
            pdf_bytes,
            // First priced-writeback is always pre-acceptance — the
            // stock-alert overlay only flips on the S325 re-render path.
            false,
        )?;
        let resp = match self
            .client
            .post(&url)
            .header("Authorization", self.bearer_header()?)
            // S377 — required by SvelteKit's CSRF gate for multipart POSTs;
            // without it the storefront 403s before any handler runs.
            .header("Origin", origin_from_base_url(&self.config.base_url))
            .header(
                "Content-Type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(body)
            .send()
            .await
        {
            Ok(r) => r,
            // No HTTP response at all — connection refused / DNS / TLS /
            // timeout. Classify the reqwest error, never bubble it as an
            // opaque `?`.
            Err(e) => return Ok(classify_send_error(&e)),
        };
        let status = resp.status().as_u16();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        // F8 — a mid-body connection drop is itself a transport failure, not
        // an empty-body app response. Surface it as such rather than
        // `unwrap_or_default()`-ing into a misleading parse error.
        let body_text = match resp.text().await {
            Ok(t) => t,
            Err(e) => return Ok(classify_send_error(&e)),
        };
        Ok(classify_writeback_response(
            status,
            content_type.as_deref(),
            &body_text,
        ))
    }

    fn bearer_header(&self) -> Result<reqwest::header::HeaderValue> {
        let s = format!("Bearer {}", &*self.config.bearer_token);
        let mut hv = reqwest::header::HeaderValue::from_str(&s)
            .context("invalid bearer header (control chars)")?;
        hv.set_sensitive(true);
        Ok(hv)
    }

    /// Loop forever until cancelled. Boots with a 30s delay to let
    /// other ABERP daemons settle (matches the intake-daemon posture).
    /// On a cycle error, backs off 5s → 15s → 60s → cadence (matching
    /// the intake-daemon's S256 backoff). Exit on cancel via tokio::select.
    ///
    /// **S286 / PR-268**: prefer [`Self::run_daemon_supervised`] over this
    /// at the boot site. This entry point stays for cancellation
    /// integration tests (where panic-recovery would confuse the
    /// assertion); production spawns the supervised wrapper.
    pub async fn run_daemon_forever(self, cancel: CancellationToken) {
        let service = Arc::new(self);
        service.poll_loop(cancel).await;
    }

    /// Inner loop body. Shared by [`Self::run_daemon_forever`] (cancellation
    /// tests) and [`Self::run_daemon_supervised`] (production). Takes
    /// `Arc<Self>` so the supervisor can re-await a fresh tokio spawn after
    /// a panic without re-constructing the service.
    async fn poll_loop(self: Arc<Self>, cancel: CancellationToken) {
        let cadence = self.config.poll_interval;
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(Duration::from_secs(30)) => {}
        }
        let mut backoff_idx: usize = 0;
        loop {
            if cancel.is_cancelled() {
                return;
            }
            let summary = self.poll_once().await;
            tracing::info!(
                fetched = summary.fetched_from_storefront,
                enqueued = summary.enqueued,
                advanced = summary.advanced,
                posted = summary.posted,
                failed = summary.failed,
                elapsed_ms = summary.elapsed_ms,
                error = ?summary.error,
                "pricing-pipeline cycle complete"
            );
            let sleep_dur = if summary.error.is_some() {
                let dur = backoff_duration(backoff_idx, cadence);
                backoff_idx = backoff_idx.saturating_add(1);
                dur
            } else {
                backoff_idx = 0;
                cadence
            };
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(sleep_dur) => {}
            }
        }
    }

    /// S286 / PR-268 hotfix supervisor. Runs [`Self::poll_loop`] inside a
    /// tokio task whose `JoinHandle` we await; if the task panics
    /// (Rust-side panic — NOT a C++ `libc++abi` termination, which kills
    /// the process before this code runs), we:
    ///
    /// 1. Extract the panic message from the payload.
    /// 2. Emit a `quote.pricing_daemon_panicked` audit row (best-effort —
    ///    if the audit emit itself errors, we log and continue rather
    ///    than infinite-looping).
    /// 3. Sleep 30s (or 5min if 5+ panics within the last 10 minutes —
    ///    runaway-safety).
    /// 4. Re-spawn the loop.
    ///
    /// The PROD_v2.27.2 panic that motivated this hotfix was a *C++*
    /// FATAL inside DuckDB; this supervisor can't catch it directly. The
    /// SELECT-first defence in [`crate::quote_pricing_jobs::set_state`]
    /// avoids triggering that FATAL path. This supervisor is the
    /// defence-in-depth: any future Rust-side panic is recovered here
    /// rather than aborting the host process.
    pub async fn run_daemon_supervised(self, cancel: CancellationToken) {
        let service = Arc::new(self);
        let mut panic_window: std::collections::VecDeque<Instant> =
            std::collections::VecDeque::new();
        let mut restart_count: u32 = 0;
        loop {
            if cancel.is_cancelled() {
                return;
            }
            let s = service.clone();
            let inner_cancel = cancel.clone();
            let handle = tokio::spawn(async move {
                s.poll_loop(inner_cancel).await;
            });
            match handle.await {
                Ok(()) => return,
                Err(join_err) if join_err.is_cancelled() => return,
                Err(join_err) => {
                    restart_count = restart_count.saturating_add(1);
                    let panic_msg = if join_err.is_panic() {
                        panic_payload_to_string(join_err.into_panic())
                    } else {
                        // Shouldn't happen — JoinError with neither
                        // is_cancelled nor is_panic is currently
                        // unreachable in stable tokio. Defensive: render
                        // it as a string.
                        format!("daemon join failed (non-panic): {join_err}")
                    };
                    tracing::error!(
                        panic_msg = %panic_msg,
                        restart_count = restart_count,
                        "pricing-pipeline daemon panicked; supervisor recovering"
                    );
                    // Best-effort audit emit. A failure here logs but does
                    // NOT block the restart — the daemon staying up is more
                    // important than the durable forensic row in this case.
                    let svc = service.clone();
                    let panic_msg_for_audit = panic_msg.clone();
                    if let Err(e) = spawn_blocking(move || {
                        emit_daemon_panicked_audit(
                            &svc.deps,
                            &panic_msg_for_audit,
                            restart_count,
                            None,
                        )
                    })
                    .await
                    .unwrap_or_else(|join| Err(anyhow!("audit spawn join: {join}")))
                    {
                        tracing::warn!(
                            error = %e,
                            "quote.pricing_daemon_panicked audit emit failed (non-fatal)"
                        );
                    }

                    // Trim the panic-window to the last 10 minutes.
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
                        "pricing-pipeline supervisor sleeping before restart"
                    );
                    tokio::select! {
                        _ = cancel.cancelled() => return,
                        _ = tokio::time::sleep(sleep_dur) => {}
                    }
                }
            }
        }
    }
}

// ── S286 / PR-268 supervisor knobs ────────────────────────────────────

/// Rolling window for the runaway-loop guard. Five panics inside this
/// span tips the supervisor into the long-backoff branch.
const PANIC_WINDOW: Duration = Duration::from_secs(10 * 60);

/// Five consecutive panics inside [`PANIC_WINDOW`] → switch to the long
/// backoff. Picked so a transient-but-recurring bug (one panic per
/// poll-cycle) still gets multiple-minutes between restart attempts
/// rather than tight-looping.
const PANIC_BURST_THRESHOLD: usize = 5;

/// Default supervisor sleep before re-spawning the daemon loop.
const PANIC_SHORT_BACKOFF: Duration = Duration::from_secs(30);

/// Escalated supervisor sleep when [`PANIC_BURST_THRESHOLD`] panics
/// have fired inside [`PANIC_WINDOW`]. The brief: "if the daemon panics,
/// something's seriously wrong; don't tight-loop." Five minutes gives
/// the operator a chance to see the AMBER SPA banner.
const PANIC_LONG_BACKOFF: Duration = Duration::from_secs(5 * 60);

/// Extract a human-readable string from a `Box<dyn Any + Send>` panic
/// payload. Standard idiom — strings/Strings are the two payloads stdlib
/// emits; anything else we render as a placeholder. Sanitized before the
/// audit emit (CR/LF/NUL stripped, truncated to 1000 chars).
fn panic_payload_to_string(payload: Box<dyn std::any::Any + Send>) -> String {
    let raw = if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    };
    sanitize_panic_msg(&raw)
}

/// Sanitization rules match `quote_pricing_jobs::sanitize_reason` — strip
/// CR/LF/NUL so the panic message can't leak into a log line as a forged
/// extra line, and truncate to 1000 chars so a 100MB-allocated-vec panic
/// payload doesn't bloat the audit ledger.
fn sanitize_panic_msg(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .filter(|c| !matches!(*c, '\r' | '\n' | '\0'))
        .collect();
    if cleaned.chars().count() <= 1000 {
        cleaned
    } else {
        cleaned.chars().take(1000).collect()
    }
}

/// S286 / PR-268 — emit `quote.pricing_daemon_panicked`. Opens a fresh
/// DuckDB connection (the supervisor runs after a panic that may have
/// poisoned the inner task's connections), ensures the audit schema, and
/// appends one row with a fresh ULID-keyed idempotency. Returns Err on
/// any failure so the supervisor can log it without crashing.
pub(crate) fn emit_daemon_panicked_audit(
    deps: &PricingPipelineDeps,
    panic_msg: &str,
    restart_count_since_boot: u32,
    last_known_quote_id: Option<String>,
) -> Result<()> {
    let mut conn =
        duckdb::Connection::open(&deps.db_path).context("open DB for daemon-panic audit")?;
    audit_ensure_schema(&conn).context("ensure audit-ledger schema")?;
    let tx = conn.transaction().context("open daemon-panic tx")?;
    let meta = LedgerMeta::new(
        TenantId::new(deps.tenant.as_str()).context("tenant id")?,
        deps.binary_hash,
    );
    let actor = Actor::from_local_cli(Ulid::new().to_string(), &deps.operator_login);
    // Every panic gets a fresh ULID-keyed idempotency suffix — restart_count
    // would collide on the rare two-process race during a quick relaunch.
    let idempotency_key = format!("quote_pricing_daemon_panicked:{}", Ulid::new());
    let payload = QuotePricingDaemonPanickedPayload {
        tenant_id: deps.tenant.as_str().to_string(),
        panic_msg: panic_msg.to_string(),
        restart_count_since_boot,
        last_known_quote_id,
        actor: "system".to_string(),
        idempotency_key: idempotency_key.clone(),
    };
    let bytes = serde_json::to_vec(&payload).context("encode daemon-panic payload")?;
    append_in_tx(
        &tx,
        &meta,
        EventKind::QuotePricingDaemonPanicked,
        bytes,
        actor,
        Some(idempotency_key),
    )
    .context("append QuotePricingDaemonPanicked")?;
    tx.commit().context("commit daemon-panic")?;
    Ok(())
}

#[derive(Debug, Serialize)]
struct QuotePricingDaemonPanickedPayload {
    tenant_id: String,
    panic_msg: String,
    restart_count_since_boot: u32,
    last_known_quote_id: Option<String>,
    actor: String,
    idempotency_key: String,
}

/// Result of one state-machine step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepOutcome {
    Advanced,
    Posted,
    Failed,
}

/// The contract shape of a 200 priced-writeback response per ADR-0004:
/// `{status:"quoted"}` (fresh) or `{status:"quoted", idempotent:true}`
/// (replay). `status` is REQUIRED — its absence is how
/// [`classify_writeback_response`] tells a malformed app response (e.g.
/// `{"unexpected":"shape"}`) from a real one.
#[derive(Debug, Deserialize)]
struct PricedWritebackOk {
    status: Option<String>,
    idempotent: Option<bool>,
}

/// S347 / PR-39 (F1 + F2) — typed transport-vs-app verdict for ONE
/// priced-writeback POST. Replaces the old stringly `Result` that parsed
/// HTML bodies as JSON and dumped every unrecognised shape into the
/// `? Ismeretlen / Unknown` bucket. Each variant maps to a non-`Unknown`
/// [`FailureKind`] and a bilingual operator label, so a misroute can never
/// masquerade as anything else (audit F1/F2, 2026-06-11 incident).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WritebackOutcome {
    /// 2xx + `application/json` + the required `{status}` field present.
    Success { idempotent: bool },
    /// 200 + `text/html` — the CDN routed the API path to the SPA origin
    /// and returned the index shell. THE incident shape.
    RoutingMisconfigured {
        http_status: u16,
        content_type: String,
        body_excerpt: String,
    },
    /// 401 — bearer / origin-secret mismatch.
    Unauthorized {
        http_status: u16,
        body_excerpt: String,
    },
    /// 403 — SvelteKit CSRF gate (missing/wrong `Origin` header) or the
    /// storefront's origin secret rejected. (Bearer failures are 401.)
    Forbidden {
        http_status: u16,
        body_excerpt: String,
    },
    /// Any other status whose Content-Type is not `application/json`.
    NonJsonResponse {
        http_status: u16,
        content_type: String,
        body_excerpt: String,
    },
    /// 2xx + `application/json` but missing the required `{status}` field.
    MalformedAppResponse {
        http_status: u16,
        body_excerpt: String,
    },
    /// 4xx (non-401/403) with a JSON body — the app rejected the writeback.
    AppRejected {
        http_status: u16,
        body_excerpt: String,
    },
    /// 5xx with a JSON body — storefront server error. Retryable.
    AppErrored {
        http_status: u16,
        body_excerpt: String,
    },
    /// Request timed out before a response. Retryable.
    Timeout,
    /// Connection refused / DNS / TLS / body-read drop — no HTTP response.
    /// Retryable.
    TransportError { kind: String },
}

impl WritebackOutcome {
    /// Stable closed-vocab storage token (audit payload + the
    /// `writeback:<tag>` reason prefix [`classify_failure`] matches on).
    pub fn tag(&self) -> &'static str {
        match self {
            WritebackOutcome::Success { .. } => "success",
            WritebackOutcome::RoutingMisconfigured { .. } => "routing_misconfigured",
            WritebackOutcome::Unauthorized { .. } => "unauthorized",
            WritebackOutcome::Forbidden { .. } => "forbidden",
            WritebackOutcome::NonJsonResponse { .. } => "non_json_response",
            WritebackOutcome::MalformedAppResponse { .. } => "malformed_app_response",
            WritebackOutcome::AppRejected { .. } => "app_rejected",
            WritebackOutcome::AppErrored { .. } => "app_errored",
            WritebackOutcome::Timeout => "timeout",
            WritebackOutcome::TransportError { .. } => "transport_error",
        }
    }

    /// THE single source of truth mapping a tag → coarse [`FailureKind`].
    /// `None` for `success` and any unrecognised tag. Both the instance
    /// methods AND [`classify_failure`]'s post-stage arm consult this, so
    /// the by-variant classification can't drift from the by-reason one.
    pub fn failure_kind_for_tag(tag: &str) -> Option<FailureKind> {
        match tag {
            // Server-side / network blips — the next cycle has a real shot.
            "app_errored" | "timeout" | "transport_error" => Some(FailureKind::Transient),
            // Config / routing / contract — retry alone never helps; an
            // operator must fix routing, rotate a secret, or chase the
            // storefront contract.
            "routing_misconfigured"
            | "unauthorized"
            | "forbidden"
            | "non_json_response"
            | "malformed_app_response"
            | "app_rejected" => Some(FailureKind::Permanent),
            _ => None,
        }
    }

    /// Coarse verdict for the `failure_kind` column. Only meaningful for
    /// non-`Success` outcomes (Success is not a failure).
    pub fn failure_kind(&self) -> FailureKind {
        Self::failure_kind_for_tag(self.tag()).unwrap_or(FailureKind::Permanent)
    }

    pub fn is_success(&self) -> bool {
        matches!(self, WritebackOutcome::Success { .. })
    }

    /// Whether the next daemon cycle should bother retrying.
    pub fn retryable(&self) -> bool {
        matches!(
            Self::failure_kind_for_tag(self.tag()),
            Some(FailureKind::Transient)
        )
    }

    pub fn http_status(&self) -> Option<u16> {
        match self {
            WritebackOutcome::RoutingMisconfigured { http_status, .. }
            | WritebackOutcome::Unauthorized { http_status, .. }
            | WritebackOutcome::Forbidden { http_status, .. }
            | WritebackOutcome::NonJsonResponse { http_status, .. }
            | WritebackOutcome::MalformedAppResponse { http_status, .. }
            | WritebackOutcome::AppRejected { http_status, .. }
            | WritebackOutcome::AppErrored { http_status, .. } => Some(*http_status),
            WritebackOutcome::Success { .. }
            | WritebackOutcome::Timeout
            | WritebackOutcome::TransportError { .. } => None,
        }
    }

    /// The response Content-Type when it is part of the verdict. The two
    /// non-JSON variants carry the actual header; the JSON-path variants
    /// are `application/json` by construction; auth + transport verdicts
    /// don't constrain it.
    pub fn content_type(&self) -> Option<String> {
        match self {
            WritebackOutcome::RoutingMisconfigured { content_type, .. }
            | WritebackOutcome::NonJsonResponse { content_type, .. } => Some(content_type.clone()),
            WritebackOutcome::Success { .. }
            | WritebackOutcome::MalformedAppResponse { .. }
            | WritebackOutcome::AppRejected { .. }
            | WritebackOutcome::AppErrored { .. } => Some("application/json".to_string()),
            WritebackOutcome::Unauthorized { .. }
            | WritebackOutcome::Forbidden { .. }
            | WritebackOutcome::Timeout
            | WritebackOutcome::TransportError { .. } => None,
        }
    }

    pub fn body_excerpt(&self) -> Option<&str> {
        match self {
            WritebackOutcome::RoutingMisconfigured { body_excerpt, .. }
            | WritebackOutcome::Unauthorized { body_excerpt, .. }
            | WritebackOutcome::Forbidden { body_excerpt, .. }
            | WritebackOutcome::NonJsonResponse { body_excerpt, .. }
            | WritebackOutcome::MalformedAppResponse { body_excerpt, .. }
            | WritebackOutcome::AppRejected { body_excerpt, .. }
            | WritebackOutcome::AppErrored { body_excerpt, .. } => Some(body_excerpt.as_str()),
            WritebackOutcome::TransportError { kind } => Some(kind.as_str()),
            WritebackOutcome::Success { .. } | WritebackOutcome::Timeout => None,
        }
    }

    /// Bilingual operator label (HU) — mirrors the SPA chip vocab in
    /// `pricing-failure-kind.ts`.
    pub fn label_hu(&self) -> &'static str {
        match self {
            WritebackOutcome::Success { .. } => "✓ Sikeres",
            WritebackOutcome::RoutingMisconfigured { .. } => {
                "🛑 Útvonal vagy 404 — CloudFront elfedte"
            }
            WritebackOutcome::Unauthorized { .. } => "🛑 Hitelesítési hiba",
            WritebackOutcome::Forbidden { .. } => "🛑 Hozzáférés megtagadva",
            WritebackOutcome::NonJsonResponse { .. } => "🛑 Nem-JSON válasz",
            WritebackOutcome::MalformedAppResponse { .. } => "🛑 Hibás válasz-szerkezet",
            WritebackOutcome::AppRejected { .. } => "🛑 Storefront elutasította",
            WritebackOutcome::AppErrored { .. } => "↻ Storefront szerverhiba",
            WritebackOutcome::Timeout => "↻ Időtúllépés",
            WritebackOutcome::TransportError { .. } => "↻ Hálózati hiba",
        }
    }

    /// Bilingual operator label (EN).
    pub fn label_en(&self) -> &'static str {
        match self {
            WritebackOutcome::Success { .. } => "Success",
            WritebackOutcome::RoutingMisconfigured { .. } => {
                "Routing or 404 — masked by CloudFront"
            }
            WritebackOutcome::Unauthorized { .. } => "Unauthorized",
            WritebackOutcome::Forbidden { .. } => "Forbidden",
            WritebackOutcome::NonJsonResponse { .. } => "Non-JSON response",
            WritebackOutcome::MalformedAppResponse { .. } => "Malformed app response",
            WritebackOutcome::AppRejected { .. } => "Storefront rejected",
            WritebackOutcome::AppErrored { .. } => "Storefront server error",
            WritebackOutcome::Timeout => "Timeout",
            WritebackOutcome::TransportError { .. } => "Transport error",
        }
    }

    /// The actual next action the operator should take.
    pub fn operator_hint(&self) -> &'static str {
        match self {
            WritebackOutcome::Success { .. } => "",
            WritebackOutcome::RoutingMisconfigured { .. } => {
                "Az origin HTML-t adott vissza JSON helyett. Két ismert oka lehet / Origin returned \
                 HTML where JSON expected. Two known causes: (1) hiányzó CloudFront-útvonal — az `/api/*` \
                 behavior nem illeszkedik erre az URL-re / CloudFront route missing — `/api/*` \
                 behavior not matching this URL path; (2) a storefront 404-et adott (pl. ajánlat nem \
                 található, rossz ABERP_SITE_QUOTE_DIR), és a CloudFront `404→/index.html` szabálya \
                 200-ra írta felül / storefront returned 404 (e.g. quote not found, \
                 ABERP_SITE_QUOTE_DIR misconfigured) and CloudFront's `404→/index.html` rule \
                 overrode the code to 200. Nézd a storefront logokat és a CloudFront \
                 CustomErrorResponses panelt / check storefront logs and the CloudFront \
                 CustomErrorResponses panel."
            }
            WritebackOutcome::Unauthorized { .. } => {
                "401 — X-CloudFront-Secret vagy Bearer eltérés; ellenőrizd az ADR-0009 \
                 titok-rotációt / X-CloudFront-Secret or Bearer mismatch; check ADR-0009 secret \
                 rotation"
            }
            WritebackOutcome::Forbidden { .. } => {
                "403 — két ok / two causes: (1) SvelteKit CSRF — hiányzó vagy rossz Origin fejléc / \
                 missing or wrong Origin header; (2) a storefront origin-titok elutasítva / origin \
                 secret rejected (ADR-0009 rotation order). Bearer-hiba 401, nem ide tartozik / \
                 Bearer failures are 401, not this."
            }
            WritebackOutcome::NonJsonResponse { .. } => {
                "A storefront nem-JSON választ adott; rossz útvonal vagy middleware / storefront \
                 returned non-JSON; routing or middleware misconfigured"
            }
            WritebackOutcome::MalformedAppResponse { .. } => {
                "200-as JSON a várt {status} szerkezet nélkül; storefront szerződés-eltérés / 200 \
                 JSON without the expected {status} shape; storefront contract drift"
            }
            WritebackOutcome::AppRejected { .. } => {
                "A storefront elutasította a visszaírást (4xx); nézd a törzs-kivonatot / storefront \
                 rejected the writeback (4xx); inspect the body excerpt"
            }
            WritebackOutcome::AppErrored { .. } => {
                "Storefront 5xx — a következő ciklusban újrapróbálható / storefront 5xx — retryable \
                 on the next cycle"
            }
            WritebackOutcome::Timeout => {
                "A visszaírás időtúllépett — a következő ciklusban újrapróbálható / writeback timed \
                 out — retryable on the next cycle"
            }
            WritebackOutcome::TransportError { .. } => {
                "Kapcsolat / DNS / TLS hiba — a következő ciklusban újrapróbálható / connection / \
                 DNS / TLS failure — retryable on the next cycle"
            }
        }
    }

    /// The `error_reason` string persisted on the Failed row. Prefixed with
    /// the stable `writeback:<tag>` token so [`classify_failure`] maps it
    /// by-variant and the SPA can swap in the granular bilingual chip.
    pub fn failure_reason(&self) -> String {
        let mut s = format!(
            "writeback:{} {} / {} — {}",
            self.tag(),
            self.label_hu(),
            self.label_en(),
            self.operator_hint()
        );
        if let Some(code) = self.http_status() {
            s.push_str(&format!("; http_status={code}"));
        }
        if let Some(ct) = self.content_type() {
            s.push_str(&format!("; content_type={ct}"));
        }
        if let Some(body) = self.body_excerpt() {
            if !body.is_empty() {
                s.push_str(&format!("; body={body}"));
            }
        }
        s
    }
}

/// S351 — PURE storefront writeback URL builder. The operator-stored
/// `base_url` may carry a trailing `/`; left raw it produces a `//api/…`
/// double-slash path that CloudFront's `/api/*` behavior does NOT match,
/// so the request hits the S3 SPA fallback and returns HTML (the S347
/// classifier then correctly labels it `routing_misconfigured`). Trimming
/// here makes that misroute impossible regardless of what the operator
/// typed. Trims only the formatted URL — never the stored config value.
pub(crate) fn resolved_writeback_url(base: &str, quote_id: &str, suffix: &str) -> String {
    let base = base.trim_end_matches('/');
    format!("{base}/api/quotes/{quote_id}/{suffix}")
}

/// S377 — PURE `Origin` header value for storefront writebacks. SvelteKit
/// 2.61.x's CSRF gate (`respond.js`) deterministically `403`s every
/// `multipart/form-data` POST whose `Origin` header is absent or does not
/// match the app's own origin; reqwest never sends `Origin` on its own, so
/// every priced writeback failed in prod (S376). The storefront's own
/// origin IS the `base_url` scheme+authority, so we derive it from there:
/// trim to `scheme://host[:port]`, dropping any path / query / trailing
/// slash (Origin is host-only by spec — a path would itself fail the gate).
/// Shared by the pricing pipeline and the S325 re-render daemon so both
/// writeback sites send an identical, correct header. PURE — unit-testable.
pub(crate) fn origin_from_base_url(base: &str) -> String {
    let base = base.trim();
    match base.find("://") {
        Some(scheme_end) => {
            let after_scheme = scheme_end + 3;
            let authority_end = base[after_scheme..]
                .find('/')
                .map(|i| after_scheme + i)
                .unwrap_or(base.len());
            base[..authority_end].to_string()
        }
        // No scheme — return the host-ish input minus any trailing slash;
        // a malformed origin still beats a missing one for the CSRF gate.
        None => base.trim_end_matches('/').to_string(),
    }
}

/// S347 / PR-39 (F1) — PURE response classifier. The F1 fix lives here:
/// the Content-Type gate runs BEFORE any JSON parse, so an HTML (or any
/// non-`application/json`) body is NEVER parsed as "ok". No I/O — directly
/// unit-testable. Auth verdicts (401/403) take precedence over the
/// Content-Type gate because they are actionable as auth regardless of the
/// body the CDN attached.
// `pub(crate)` so the operator accept-on-behalf POST in `serve.rs`
// reuses the SAME transport-vs-app classifier as the priced-writeback
// (CLAUDE.md #8/#13 — one gate, no drift).
pub(crate) fn classify_writeback_response(
    status: u16,
    content_type: Option<&str>,
    body: &str,
) -> WritebackOutcome {
    match classify_response_gate(status, content_type, body) {
        // 2xx + application/json — the only path where the caller's contract
        // shape is parsed. `status` REQUIRED: `{"unexpected":"shape"}` parses
        // into PricedWritebackOk{status:None,..} (both fields optional), so
        // presence of `status` is the real malformed-vs-ok signal.
        Ok(()) => match serde_json::from_str::<PricedWritebackOk>(body) {
            Ok(parsed) if parsed.status.is_some() => WritebackOutcome::Success {
                idempotent: parsed.idempotent.unwrap_or(false),
            },
            _ => WritebackOutcome::MalformedAppResponse {
                http_status: status,
                body_excerpt: response_excerpt(body),
            },
        },
        Err(outcome) => outcome,
    }
}

/// S348 / PR-39 (F1) — poll-site classifier. Same Content-Type + status
/// gate as the writeback, but the 2xx+JSON success path parses the
/// `StorefrontListResponse` envelope instead of the priced-writeback ok
/// shape. `Ok(list)` on a clean 2xx `application/json`; `Err(outcome)`
/// carries the typed transport-vs-app verdict — a 200 `text/html` (CDN
/// serving the SPA shell) is `RoutingMisconfigured`, NEVER fed to the JSON
/// parser as it was before this fix. Reuses [`WritebackOutcome`] (same
/// crate) rather than duplicating the variant set (CLAUDE.md #8/#13).
fn classify_poll_response(
    status: u16,
    content_type: Option<&str>,
    body: &str,
) -> Result<StorefrontListResponse, WritebackOutcome> {
    classify_response_gate(status, content_type, body)?;
    serde_json::from_str::<StorefrontListResponse>(body).map_err(|_| {
        WritebackOutcome::MalformedAppResponse {
            http_status: status,
            body_excerpt: response_excerpt(body),
        }
    })
}

/// S347 / PR-39 (F1) — the shared Content-Type + status gate. PURE, no I/O.
/// `Ok(())` ONLY for a 2xx `application/json` response (the caller then
/// parses its own contract shape); every transport/routing/auth/non-JSON/
/// 4xx/5xx response is an `Err(outcome)`. Auth verdicts (401/403) take
/// precedence over the Content-Type gate because they are actionable as
/// auth regardless of the body the CDN attached. Factored out of
/// [`classify_writeback_response`] so the writeback POST and the
/// list-poll GET share ONE gate (CLAUDE.md #13 — no duplicated skeleton).
fn classify_response_gate(
    status: u16,
    content_type: Option<&str>,
    body: &str,
) -> Result<(), WritebackOutcome> {
    let excerpt = || response_excerpt(body);
    // Normalise: drop the `; charset=…` parameter, trim, lowercase.
    let ct_norm = content_type.map(|c| {
        c.split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase()
    });
    let is_json = ct_norm.as_deref() == Some("application/json");

    match status {
        401 => {
            return Err(WritebackOutcome::Unauthorized {
                http_status: status,
                body_excerpt: excerpt(),
            })
        }
        403 => {
            return Err(WritebackOutcome::Forbidden {
                http_status: status,
                body_excerpt: excerpt(),
            })
        }
        _ => {}
    }

    if !is_json {
        let ct = ct_norm.unwrap_or_default();
        if status == 200 && ct.starts_with("text/html") {
            return Err(WritebackOutcome::RoutingMisconfigured {
                http_status: status,
                content_type: ct,
                body_excerpt: excerpt(),
            });
        }
        return Err(WritebackOutcome::NonJsonResponse {
            http_status: status,
            content_type: ct,
            body_excerpt: excerpt(),
        });
    }

    // application/json from here down.
    match status {
        200..=299 => Ok(()),
        400..=499 => Err(WritebackOutcome::AppRejected {
            http_status: status,
            body_excerpt: excerpt(),
        }),
        500..=599 => Err(WritebackOutcome::AppErrored {
            http_status: status,
            body_excerpt: excerpt(),
        }),
        // Genuinely weird 1xx/3xx with a JSON body — not a contract we
        // expect; malformed rather than silently "ok".
        _ => Err(WritebackOutcome::MalformedAppResponse {
            http_status: status,
            body_excerpt: excerpt(),
        }),
    }
}

/// S347 / PR-39 — map a reqwest send/body-read error (no usable HTTP
/// response) to a typed verdict. `is_timeout()` → [`WritebackOutcome::
/// Timeout`]; everything else (connect / DNS / TLS / body drop) →
/// [`WritebackOutcome::TransportError`]. The Display is run through
/// `response_excerpt` to bearer-scrub + bound it.
pub(crate) fn classify_send_error(e: &reqwest::Error) -> WritebackOutcome {
    if e.is_timeout() {
        return WritebackOutcome::Timeout;
    }
    WritebackOutcome::TransportError {
        kind: response_excerpt(&e.to_string()),
    }
}

/// S347 / PR-39 — extract the `writeback:<tag>` token a typed failure
/// reason is prefixed with, and resolve it to a [`FailureKind`]. `None`
/// for legacy pre-S347 reasons (no prefix) — the caller falls back to the
/// historical string matching. `reason_lower` is already lowercased by
/// [`classify_failure`].
fn writeback_failure_kind_from_reason(reason_lower: &str) -> Option<FailureKind> {
    let tag = reason_lower
        .strip_prefix("writeback:")?
        .split(|c: char| c.is_whitespace())
        .next()?;
    WritebackOutcome::failure_kind_for_tag(tag)
}

/// 5s/15s/60s/cadence backoff schedule — mirrors the intake-daemon
/// shape from S256.
fn backoff_duration(idx: usize, cadence: Duration) -> Duration {
    match idx {
        0 => Duration::from_secs(5),
        1 => Duration::from_secs(15),
        2 => Duration::from_secs(60),
        _ => cadence,
    }
}

/// S290 / PR-271 — classify a per-stage failure into a daemon-actionable
/// verdict. Pure function — no I/O, no DB, no clock. Runs ONCE per
/// failure transition just before `set_failed`.
///
/// Rules (in evaluation order):
///
/// - reason contains `"not yet implemented"` → `Permanent`. PR-273
///   wired the OCCT-backed STEP extractor, but the Python `[step]`
///   extra is opt-in: in an environment without cadquery-ocp the
///   extractor still raises NotImplementedError. Operator must run
///   `pip install -e '.[step]'` in the aberp-cad-extract venv before
///   retry. Retains the prior c1cf32-forensic classification for the
///   historical S269 stub message verbatim (same substring).
/// - reason contains `"is not in the catalogue"` →
///   `Permanent`. Matches [`aberp_quote_engine::QuoteError::
///   MaterialNotInCatalogue`]'s `Display`. Operator must add the grade
///   to the catalogue (Material Catalogue SPA) before retry.
/// - reason contains `"_schema_version mismatch"` OR
///   `"feature-graph schema version"` → `Permanent`. Matches both
///   [`aberp_cad_extract_wrapper::ExtractError::SchemaVersionMismatch`]
///   and [`aberp_quote_engine::QuoteError::UnsupportedSchemaVersion`]'s
///   `Display`. A code-build mismatch — retry won't help until the
///   binary is upgraded.
/// - reason contains `"below configured floor"` →
///   `Permanent`. Matches [`aberp_quote_engine::QuoteError::
///   MarginFloorViolation`]. Documented pushback in the brief —
///   technically retryable if the operator adjusts the margin profile,
///   but that IS an operator action, so the Retry-click is the right
///   trigger.
/// - **stage="extract"** + reason contains `"cad file missing"` OR
///   `"CAD file not found"` → `Permanent`. Storefront data-quality miss
///   (S289 caught one variant; the storefront persistence still has
///   gaps in the corner-case path).
/// - **stage="extract"** + reason contains `"step file"` → `Permanent`.
///   PR-273 added the OCCT-backed STEP extractor; v1 only handles
///   single-part STEP so multi-solid assemblies and unreadable STEP
///   files both surface with "STEP file ..." in the message. Operator
///   must trim the assembly to a single part (or re-export) — retry
///   without that change won't help.
/// - **stage="extract"** + reason contains `"unsupported file extension"`
///   → `Permanent`. PR-274 (S297 F1) closes the storefront-vs-extractor
///   whitelist mismatch: the storefront accepts 11 CAD formats but the
///   Python dispatcher (`cli.py::_route`) only routes `.stl`/`.step`/`.stp`.
///   Anything else (`.iges`, `.dxf`, `.sldprt`, …) raises `ValueError`
///   with the literal "Unsupported file extension". Retry can never help
///   — the customer must re-upload in a supported format.
/// - **stage="post"** + reason contains `"HTTP 401"` OR `"HTTP 403"` →
///   `Permanent`. Auth — operator must rotate the storefront token via
///   Settings → Storefront credentials.
/// - **stage="post"** + reason contains `"HTTP 4"` (any other 4xx) →
///   `Permanent`. Storefront-side validation rejected the writeback;
///   retry without change won't help.
/// - **stage="post"** + reason contains `"HTTP 5"` (any 5xx) OR
///   `"timeout"` OR `"connection"` OR `"dns"` → `Transient`. Storefront
///   blip; the next cycle's retry has a real chance of succeeding.
/// - Default → `Unknown`. Surfaces with a capped auto-retry policy
///   per the daemon scheduler; better than silent permanent-loop on a
///   future error shape we haven't classified yet.
///
/// Stays case-INSENSITIVE on the reason match by lowercasing both
/// sides (matchings use lowercase literals); the engine + wrapper
/// `Display` strings already use lowercase but defence-in-depth saves
/// a future regression.
pub fn classify_failure(stage: &str, reason: &str) -> FailureKind {
    let r = reason.to_ascii_lowercase();
    // Permanent: extractor stubs / not-yet-implemented / bad input.
    if r.contains("not yet implemented") {
        return FailureKind::Permanent;
    }
    if r.contains("is not in the catalogue") {
        return FailureKind::Permanent;
    }
    if r.contains("_schema_version mismatch") || r.contains("feature-graph schema version") {
        return FailureKind::Permanent;
    }
    if r.contains("below configured floor") {
        return FailureKind::Permanent;
    }
    if stage == "extract" && (r.contains("cad file missing") || r.contains("cad file not found")) {
        return FailureKind::Permanent;
    }
    if stage == "extract" && r.contains("input cad file not found") {
        return FailureKind::Permanent;
    }
    // PR-273 — STEP extractor surfaces shape/parse errors with "STEP
    // file ..." in the message (assembly with N solids, no solid
    // body, could not be parsed). All are data-quality issues the
    // operator must fix at upload time; auto-retry is wasted cycles.
    if stage == "extract" && r.contains("step file") {
        return FailureKind::Permanent;
    }
    // PR-274 / S297 F1 — storefront whitelist (11 extensions) is wider
    // than the dispatcher's route table (3 extensions). For any
    // unsupported format the CLI raises `ValueError("Unsupported file
    // extension '.iges'. Supported: .stl, .step, .stp")` and the
    // wrapper bubbles that verbatim into the reason. Retry can never
    // help — the customer must re-upload in a supported format.
    if stage == "extract" && r.contains("unsupported file extension") {
        return FailureKind::Permanent;
    }
    // POST-back classification — split on HTTP status family + transport
    // failure shape.
    if stage == "post" {
        // S347 / PR-39 (F2) — typed writeback verdicts embed a stable
        // `writeback:<tag>` token. Classify on the token, NOT on incidental
        // reqwest Display wording (the old `"connection"`/`"dns"` substrings
        // were one reqwest upgrade away from misclassifying). The legacy
        // string fallback below stays for pre-S347 Failed rows on disk.
        if let Some(kind) = writeback_failure_kind_from_reason(&r) {
            return kind;
        }
        if r.contains("http 401") || r.contains("http 403") {
            return FailureKind::Permanent;
        }
        // The post_priced_writeback `Display` shape is `priced-writeback
        // HTTP <code> body=...`. Match "http 4" then disambiguate from
        // 5xx below.
        if r.contains("http 4") {
            return FailureKind::Permanent;
        }
        if r.contains("http 5")
            || r.contains("timeout")
            || r.contains("timed out")
            || r.contains("connection")
            || r.contains("dns")
        {
            return FailureKind::Transient;
        }
    }
    FailureKind::Unknown
}

/// Best-effort failure path used by every state-machine arm. Writes
/// the `Failed` row + emits `QuotePricingFailed` and (S290) the
/// `QuotePricingFailureClassified` companion. Tracing is downstream of
/// the audit row (the audit is the durable record).
#[allow(clippy::too_many_arguments)]
fn emit_failure(
    conn: &mut duckdb::Connection,
    tenant_id: &str,
    binary_hash: BinaryHash,
    login: &str,
    quote_id: &str,
    stage: &str,
    reason: &str,
    attempt_n: u32,
) -> Result<()> {
    let failure_kind = classify_failure(stage, reason);
    let set_outcome = jobs::set_failed(
        conn,
        quote_id,
        tenant_id,
        stage,
        reason,
        failure_kind,
        OffsetDateTime::now_utc(),
    )?;
    if !matches!(set_outcome, jobs::TransitionOutcome::Applied) {
        // Already-Failed (prior cycle landed) or NotFound. Skip audit emit
        // to keep one row per terminal-failure transition.
        tracing::info!(
            quote_id = %quote_id,
            outcome = ?set_outcome,
            "set_failed no-op; skipping audit emit"
        );
        return Ok(());
    }
    append_failure_audit_pair(
        conn,
        tenant_id,
        binary_hash,
        login,
        quote_id,
        stage,
        reason,
        failure_kind,
        attempt_n,
    )
}

/// Append the `QuotePricingFailed` + `QuotePricingFailureClassified` audit
/// pair in one tx so a forensic walker never sees one without the other.
/// Shared by the state-machine failure path ([`emit_failure`], which
/// runs `classify_failure` then `set_failed` first) and the enqueue-time
/// no-CAD failure path
/// ([`PricingPipelineService::enqueue_failed_no_cad`], S379, which
/// classifies the listing-level no-CAD as `Permanent` up front and inserts
/// the Failed row directly). `failure_kind` is passed in explicitly rather
/// than re-derived so each caller's verdict is authoritative.
#[allow(clippy::too_many_arguments)]
fn append_failure_audit_pair(
    conn: &mut duckdb::Connection,
    tenant_id: &str,
    binary_hash: BinaryHash,
    login: &str,
    quote_id: &str,
    stage: &str,
    reason: &str,
    failure_kind: FailureKind,
    attempt_n: u32,
) -> Result<()> {
    let tx = conn.transaction().context("open failed-audit tx")?;
    let meta = LedgerMeta::new(TenantId::new(tenant_id).context("tenant id")?, binary_hash);
    let actor = Actor::from_local_cli(Ulid::new().to_string(), login);
    let payload = QuotePricingFailedPayload {
        quote_id: quote_id.to_string(),
        tenant_id: tenant_id.to_string(),
        stage: stage.to_string(),
        reason: reason.chars().take(1000).collect(),
        attempt_n,
        actor: "system".to_string(),
        idempotency_key: format!("quote_pricing_failed:{quote_id}:{attempt_n}"),
    };
    let bytes = serde_json::to_vec(&payload).context("encode failed payload")?;
    append_in_tx(
        &tx,
        &meta,
        EventKind::QuotePricingFailed,
        bytes,
        actor,
        Some(payload.idempotency_key.clone()),
    )
    .context("append QuotePricingFailed")?;
    // S290 / PR-271 — companion classifier-verdict row. Inside the same
    // tx so the (failed + classified) pair commit atomically: a forensic
    // walker never sees one without the other. Fresh ULID actor so the
    // pair is distinguishable in the audit list (same suffix shape as
    // the failed row).
    let classified_actor = Actor::from_local_cli(Ulid::new().to_string(), login);
    let classified_payload = QuotePricingFailureClassifiedPayload {
        quote_id: quote_id.to_string(),
        tenant_id: tenant_id.to_string(),
        failure_kind: failure_kind.as_str().to_string(),
        last_error: reason.chars().take(1000).collect(),
        attempt_n,
        actor: "system".to_string(),
        idempotency_key: format!("quote_pricing_failure_classified:{quote_id}:{attempt_n}"),
    };
    let classified_bytes =
        serde_json::to_vec(&classified_payload).context("encode classified payload")?;
    append_in_tx(
        &tx,
        &meta,
        EventKind::QuotePricingFailureClassified,
        classified_bytes,
        classified_actor,
        Some(classified_payload.idempotency_key.clone()),
    )
    .context("append QuotePricingFailureClassified")?;
    tx.commit().context("commit failed-audit")?;
    Ok(())
}

/// S347 / PR-39 (F1+F2) — emit the per-attempt `quote.priced_writeback_outcome`
/// audit row carrying the granular transport-vs-app verdict
/// (variant + http_status + content_type + body_excerpt + retryable). Fires
/// on EVERY writeback attempt (success too); the idempotency key
/// (`quote_priced_writeback_outcome:<quote_id>:<attempt_n>`) keeps it to one
/// row per (quote, attempt). Own tx — independent of the success/failure
/// job-state transition that follows.
fn emit_priced_writeback_outcome(
    conn: &mut duckdb::Connection,
    tenant_id: &str,
    binary_hash: BinaryHash,
    login: &str,
    quote_id: &str,
    outcome: &WritebackOutcome,
    attempt_n: u32,
) -> Result<()> {
    let tx = conn.transaction().context("open writeback-outcome tx")?;
    let meta = LedgerMeta::new(TenantId::new(tenant_id).context("tenant id")?, binary_hash);
    let actor = Actor::from_local_cli(Ulid::new().to_string(), login);
    let payload = QuotePricedWritebackOutcomePayload {
        quote_id: quote_id.to_string(),
        tenant_id: tenant_id.to_string(),
        outcome: outcome.tag().to_string(),
        http_status: outcome.http_status(),
        content_type: outcome.content_type(),
        body_excerpt: outcome.body_excerpt().map(|s| s.to_string()),
        retryable: outcome.retryable(),
        attempt_n,
        actor: "system".to_string(),
        idempotency_key: format!("quote_priced_writeback_outcome:{quote_id}:{attempt_n}"),
    };
    let bytes = serde_json::to_vec(&payload).context("encode writeback-outcome payload")?;
    append_in_tx(
        &tx,
        &meta,
        EventKind::QuotePricedWritebackOutcome,
        bytes,
        actor,
        Some(payload.idempotency_key.clone()),
    )
    .context("append QuotePricedWritebackOutcome")?;
    tx.commit().context("commit writeback-outcome")?;
    Ok(())
}

/// Convert local catalogue rows into engine input shape. The two
/// share field names but the local row has more columns (audit
/// metadata, display_name); the engine only takes what scoring needs.
fn convert_materials(locals: &[crate::quoting_materials::Material]) -> Result<Vec<EngineMaterial>> {
    locals
        .iter()
        .map(|m| {
            Ok(EngineMaterial {
                grade: m.grade.clone(),
                density_g_cm3: m.density_g_cm3,
                cost_per_kg_eur: m.cost_per_kg_eur,
                machining_difficulty: m.machining_difficulty,
                quote_multiplier: m.quote_multiplier,
                stock_status: parse_stock_status(&m.stock_status)?,
            })
        })
        .collect()
}

fn parse_stock_status(s: &str) -> Result<StockStatus> {
    match s {
        "in_stock" => Ok(StockStatus::InStock),
        "source_1_2d" => Ok(StockStatus::Source1_2d),
        "source_3_7d" => Ok(StockStatus::Source3_7d),
        "special_order" => Ok(StockStatus::SpecialOrder),
        other => Err(anyhow!("unknown stock_status: {other:?}")),
    }
}

fn convert_complexity(
    locals: &[crate::quoting_tunables::ComplexityRule],
) -> Vec<EngineComplexityRule> {
    locals
        .iter()
        .enumerate()
        .map(|(i, r)| EngineComplexityRule {
            // S410 / [[no-sql-specific]] — the storage PK is now an
            // app-minted ULID string (`r.id`), but the engine uses `id`
            // only as an internal within-run dedup / precedence /
            // reasoning-log key (`crates/aberp-quote-engine/src/engine.rs`).
            // The engine domain stays decoupled from storage identity: we
            // hand it a per-run ordinal. The operator reasoning log line
            // ("rule#N") is a within-explanation index, not a DB id.
            id: i as i64,
            feature_type: r.feature_type.clone(),
            size_bucket: r.size_bucket.clone(),
            // Local table uses i64 for SPA-friendly form-handling; engine
            // expects u32. Clamp negative (forbidden by app-layer validation
            // anyway, but defence-in-depth) and saturate above u32::MAX.
            count_min: r.count_min.clamp(0, u32::MAX as i64) as u32,
            count_max: r.count_max.map(|c| c.clamp(0, u32::MAX as i64) as u32),
            base_time_minutes: r.base_time_minutes,
            multiplier: r.multiplier,
            setup_penalty_minutes: r.setup_penalty_minutes,
        })
        .collect()
}

fn convert_tolerance(
    locals: &[crate::quoting_tunables::ToleranceMultiplier],
) -> Vec<ToleranceMultiplier> {
    locals
        .iter()
        .map(|t| ToleranceMultiplier {
            tolerance_range: t.tolerance_range.clone(),
            multiplier: t.multiplier,
            inspection_minutes_per_feature: t.inspection_minutes_per_feature,
        })
        .collect()
}

fn convert_stock_adjustments(
    locals: &[crate::quoting_tunables::StockAdjustment],
) -> Vec<StockAdjustment> {
    locals
        .iter()
        .map(|a| StockAdjustment {
            grade: a.grade.clone(),
            stock_status: a.stock_status.clone(),
            price_adjustment_pct: a.price_adjustment_pct,
        })
        .collect()
}

/// S4 / ADR-0094 Gap 2 — snapshot the local machine-rate rows into the
/// engine's `MachineRate` slice. Mirrors `convert_stock_adjustments`. An
/// empty result ⇒ the engine uses the global flat rate (byte-identical to
/// pre-ADR-0094). The loader (`quoting_machine_rates::list_machine_rates`)
/// returns rows already validated on write.
fn convert_machine_rates(
    locals: &[crate::quoting_machine_rates::MachineRateRow],
) -> Vec<EngineMachineRate> {
    locals
        .iter()
        .map(|r| EngineMachineRate {
            family: r.family.clone(),
            attended_rate_eur_per_min: r.attended_rate_eur_per_min,
            lights_out_factor: r.lights_out_factor,
            unattended_capable: r.unattended_capable,
        })
        .collect()
}

/// S6 / ADR-0094 Gap 3 — snapshot the local gear-process rows into the
/// engine's `GearProcessRate` slice. Mirrors `convert_machine_rates`. An
/// empty result (no rows) ⇒ a gear whose resolved process has no row
/// contributes 0.0 + a loud engine reasoning line; with no gears at all the
/// slice is never consulted (byte-identical to pre-Gap-3). Rows are validated
/// on write (`quoting_gear_processes::validate_gear_process_inputs`).
fn convert_gear_process_rates(
    locals: &[crate::quoting_gear_processes::GearProcessRow],
) -> Vec<EngineGearProcessRate> {
    locals
        .iter()
        .map(|r| EngineGearProcessRate {
            process: r.process.clone(),
            setup_min: r.setup_min,
            min_per_tooth: r.min_per_tooth,
            module_exponent: r.module_exponent,
            agma_quality_factor_base: r.agma_quality_factor_base,
            in_cycle_factor: r.in_cycle_factor,
        })
        .collect()
}

/// S4 / ADR-0094 Gap 2 — outer diameter (mm) of a turned/round stock form,
/// for the wiring's `route_family` call (mirrors the engine's private
/// `stock_od_mm`). A prismatic block has no turning OD ⇒ 0.0.
fn stock_od_mm(form: StockForm) -> f64 {
    match form {
        StockForm::RoundBar { diameter_mm, .. } => diameter_mm,
        StockForm::Tube { od_mm, .. } => od_mm,
        StockForm::RectangularBlock => 0.0,
    }
}

/// S4 / ADR-0094 Gap 2 — honour a per-quote operator family override.
///
/// The engine (`quote_with_shop_model`) routes by geometry and keys the
/// machine-rate row on the geometry-routed family. The engine crate is
/// frozen for S4, so to make the override actually change the price the
/// wiring re-labels the override family's rate row to the geometry family's
/// db-string — the engine then matches it and charges the override family's
/// EUR/min. Lights-out eligibility still keys on the real stock form inside
/// the engine, so forcing a prismatic part onto an unattended family does
/// NOT spuriously trigger the lights-out discount.
///
/// FLAG (handed to S5): the engine's own `[machining] routed_family=…`
/// reasoning line names the GEOMETRY family, not the override — the
/// authoritative override record is the pricing-audit payload
/// (`routed/override/effective_machine_family`). A first-class engine-level
/// family-override parameter is deferred to a future engine ADR.
fn apply_family_override(
    rates: &[EngineMachineRate],
    geometry: MachineFamily,
    effective: MachineFamily,
) -> Vec<EngineMachineRate> {
    if geometry == effective {
        return rates.to_vec();
    }
    let Some(src) = rates.iter().find(|r| r.family == effective.as_db_str()) else {
        // No rate row for the override family ⇒ nothing to substitute; leave
        // the slice as-is (the engine falls back to the global rate for the
        // geometry family if it has no row either).
        return rates.to_vec();
    };
    let geo = geometry.as_db_str();
    let mut out: Vec<EngineMachineRate> =
        rates.iter().filter(|r| r.family != geo).cloned().collect();
    out.push(EngineMachineRate {
        family: geo.to_string(),
        attended_rate_eur_per_min: src.attended_rate_eur_per_min,
        lights_out_factor: src.lights_out_factor,
        unattended_capable: src.unattended_capable,
    });
    out
}

/// S418 — the pre-S418 hardcoded `machining_rate` (1.0) is gone; the
/// rate and all six geometry-model knobs now ride the local
/// `quoting_parameters` singleton (operator-tunable from Settings →
/// Quoting Parameters) and convert straight through.
fn convert_parameters(local: &crate::quoting_tunables::QuotingParameters) -> QuotingParameters {
    QuotingParameters {
        scrap_factor: local.scrap_factor,
        machining_rate_eur_per_minute: local.machining_rate_eur_per_minute,
        // Local table uses i64; engine expects u32. Clamp negative +
        // saturate; the SPA enforces ≥ 1 already (per S267 validation).
        setup_amortization_threshold: local.setup_amortization_threshold.clamp(0, u32::MAX as i64)
            as u32,
        overhead_factor: local.overhead_factor,
        profit_margin_base: local.profit_margin_base,
        min_margin: local.min_margin,
        exotic_material_tax: local.exotic_material_tax,
        cad_cam_rate_eur_per_hour: local.cad_cam_rate_eur_per_hour,
        cad_cam_base_hours: local.cad_cam_base_hours,
        mrr_rough_ref_cm3_per_min: local.mrr_rough_ref_cm3_per_min,
        t_finish_min_per_cm2: local.t_finish_min_per_cm2,
        setup_base_min: local.setup_base_min,
        setup_5axis_min: local.setup_5axis_min,
        // ADR-0094 Gap 2 (S4) — bar-feeder capacity drives engine routing
        // (round/tube within this OD ⇒ lights-out Swiss). Was the missing
        // field that left apps/aberp non-compiling after the S3 engine bump.
        bar_capacity_mm: local.bar_capacity_mm,
    }
}

/// S428 — outcome of an operator-triggered re-price (buyer-partner assign
/// or margin override). Pure data; the caller persists + audits.
#[derive(Debug, Clone)]
pub struct RepriceOutcome {
    pub breakdown_json: String,
    pub total_price: f64,
    pub below_floor: bool,
    /// The effective floor (profile or global) when the pipeline enforces
    /// it; `None` on the global path.
    pub floor_pct: Option<f64>,
    pub realized_margin_pct: f64,
    pub source: crate::quote_margin::MarginSource,
    pub applied_margin_base: f64,
    pub profile_id: Option<String>,
}

/// S428 — re-run the pricing engine for an already-extracted job with a
/// (possibly new) operator margin override, applying the buyer partner's
/// margin profile. Returns `Ok(None)` when the job is missing or has no
/// feature graph yet (nothing to re-price). Does NOT persist or audit —
/// the serve handler decides whether to commit (e.g. a below-floor
/// override needs explicit confirmation first).
///
/// Re-prices at [`ToleranceRange::Standard`] — the same default the daemon
/// config carries for first pricing; the tolerance is not stored per-job.
pub fn reprice_quote(
    conn: &duckdb::Connection,
    tenant: &str,
    quote_id: &str,
    override_pct: Option<f64>,
) -> Result<Option<RepriceOutcome>> {
    jobs::ensure_schema(conn)?;
    let Some(detail) = jobs::get_job_detail(conn, quote_id, tenant)? else {
        return Ok(None);
    };
    let Some(graph_json) = detail.feature_graph_json else {
        return Ok(None);
    };
    let mut graph: FeatureGraph =
        serde_json::from_str(&graph_json).context("decode FeatureGraph for reprice")?;
    // ADR-0094 Gap 1 (S2) — apply the operator stock form here too, so an
    // in-place re-price (a margin or buyer-partner change) on a part that
    // already carries a stock-form override re-prices on that form rather
    // than silently reverting to the block model. Same precedence as the
    // daemon path; an unset override is inert.
    let _ = stamp_stock_form(
        &mut graph,
        operator_stock_form(
            detail.stock_form.as_deref(),
            detail.stock_od_mm,
            detail.stock_id_mm,
            detail.stock_length_mm,
        ),
    );
    let qty = detail.row.quantity.max(1);

    let materials = crate::quoting_materials::list_materials(conn, tenant)?;
    let complexity = crate::quoting_tunables::list_complexity_rules(conn, tenant)?;
    let tolerance = crate::quoting_tunables::list_tolerance_multipliers(conn, tenant)?;
    let stock_adjustments = crate::quoting_tunables::list_stock_adjustments(conn, tenant)?;
    let params = crate::quoting_tunables::get_parameters(conn, tenant)?;

    let engine_materials = convert_materials(&materials)?;
    let engine_complexity = convert_complexity(&complexity);
    let engine_tolerance = convert_tolerance(&tolerance);
    let engine_stock_adjustments = convert_stock_adjustments(&stock_adjustments);
    let mut engine_params = convert_parameters(&params);

    let customer_type = crate::quote_margin::customer_type_for_partner(
        conn,
        tenant,
        detail.buyer_partner_id.as_deref(),
    )?;
    let profile = if customer_type == crate::partners::CustomerType::Unset {
        None
    } else {
        crate::margin_profiles::active_profile_for_customer_type(conn, tenant, customer_type)?
    };
    let policy = crate::quote_margin::MarginPolicy::resolve(
        profile.as_ref(),
        override_pct,
        engine_params.profit_margin_base,
        engine_params.min_margin,
    );
    policy.apply(&mut engine_params);

    // ★ ADR-0094 Gap 2 (S4) + Gap 3 (S6) — route the in-place re-price through
    // the shop-model entry so it respects BOTH machine-family rates AND gear
    // cost. It previously called plain `engine::quote`, which ignored both
    // (flagged by S4 + S5). Mirrors the daemon price path's family routing +
    // operator override + gear stamp/snapshot.
    //
    // Calibration is intentionally kept NEUTRAL here — exactly as the prior
    // `engine::quote` was (it delegates to `quote_with_calibration` with a
    // neutral table). Aligning re-price with the daemon's LIVE calibration is
    // a separate, deferred decision (FLAGGED to S7), kept out to stay surgical.
    //
    // Inert by default: the seeded 3-axis rate equals the global flat rate, so
    // a prismatic part with no gears re-prices byte-identically to the old
    // `engine::quote`; only seeded turned/geared work moves (the intended fix).
    let machine_rate_rows = crate::quoting_machine_rates::list_machine_rates(conn, tenant)?;
    let base_machine_rates = convert_machine_rates(&machine_rate_rows);
    let geometry_family = engine::route_family(
        graph.stock_form,
        graph.requires_5_axis,
        stock_od_mm(graph.stock_form),
        &engine_params,
    );
    let effective_family = detail
        .machine_family_override
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(MachineFamily::from_db_str)
        .unwrap_or(geometry_family);
    let engine_machine_rates =
        apply_family_override(&base_machine_rates, geometry_family, effective_family);

    // Gap 3 — stamp operator gear ops + snapshot the gear-process table. Unset
    // gear ops leave the graph's empty default ⇒ no gear cost ⇒ inert.
    let _ = stamp_gear_ops(
        &mut graph,
        operator_gear_ops(detail.gear_ops_json.as_deref())
            .context("decode operator gear ops for reprice")?,
    );
    let gear_process_rows = crate::quoting_gear_processes::list_gear_processes(conn, tenant)?;
    let engine_gear_process_rates = convert_gear_process_rates(&gear_process_rows);

    let breakdown = engine::quote_with_catalogue(
        &graph,
        &engine::CatalogueSnapshot {
            materials: &engine_materials,
            complexity_rules: &engine_complexity,
            tolerance_multipliers: &engine_tolerance,
            stock_adjustments: &engine_stock_adjustments,
            machine_rates: &engine_machine_rates,
            gear_process_rates: &engine_gear_process_rates,
        },
        &engine_params,
        qty,
        ToleranceRange::Standard,
        &aberp_quote_engine::CalibrationTable::neutral(),
    )
    .map_err(|e| anyhow!("reprice engine error: {e:?}"))?;

    let below_floor = policy.is_below_floor(breakdown.margin, breakdown.total_price);
    let realized = crate::quote_margin::MarginPolicy::realized_margin_pct(
        breakdown.margin,
        breakdown.total_price,
    );
    let floor_pct = if policy.pipeline_enforced {
        Some(policy.floor_pct)
    } else {
        None
    };
    let breakdown_json = serde_json::to_string(&breakdown).context("encode reprice breakdown")?;
    Ok(Some(RepriceOutcome {
        breakdown_json,
        total_price: breakdown.total_price,
        below_floor,
        floor_pct,
        realized_margin_pct: realized,
        source: policy.source,
        applied_margin_base: policy.applied_margin_base,
        profile_id: policy.profile_id,
    }))
}

/// Build the multipart body per ADR-0004 §"Wire shape". Two parts:
/// JSON `meta` + binary `pdf`. Bytes are CRLF-delimited per RFC 7578.
pub(crate) fn build_priced_multipart(
    boundary: &str,
    feature_graph_hash: &str,
    valid_until_iso: &str,
    breakdown_json: &str,
    pdf_bytes: &[u8],
    stock_alert: bool,
) -> Result<Vec<u8>> {
    let breakdown_value: serde_json::Value = serde_json::from_str(breakdown_json)
        .with_context(|| format!("re-parse breakdown_json: {breakdown_json}"))?;
    let meta = serde_json::json!({
        "breakdown_json": breakdown_value,
        "valid_until": valid_until_iso,
        "feature_graph_hash": feature_graph_hash,
        "extractor_version": aberp_cad_extract_wrapper::WRAPPER_VERSION,
        "engine_version": engine::ENGINE_VERSION,
        // First priced-writeback (`advance_post`) passes `false`: the EVE
        // addendum-2 downgrade is detected post-acceptance, after this POST
        // has landed. The S325 re-render daemon passes `true` on a same-hash
        // re-post, which the S323 storefront `/priced` relax now overwrites +
        // flips (was an idempotent no-op pre-S323). See
        // docs/findings/s318-customer-pdf-stock-banner.md.
        "stock_alert": stock_alert,
    });
    let meta_bytes = serde_json::to_vec(&meta).context("encode meta JSON")?;

    let mut out: Vec<u8> = Vec::with_capacity(pdf_bytes.len() + meta_bytes.len() + 512);
    out.extend_from_slice(b"--");
    out.extend_from_slice(boundary.as_bytes());
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(b"Content-Disposition: form-data; name=\"meta\"\r\n");
    out.extend_from_slice(b"Content-Type: application/json\r\n\r\n");
    out.extend_from_slice(&meta_bytes);
    out.extend_from_slice(b"\r\n");

    out.extend_from_slice(b"--");
    out.extend_from_slice(boundary.as_bytes());
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(
        b"Content-Disposition: form-data; name=\"pdf\"; filename=\"quote.pdf\"\r\n",
    );
    out.extend_from_slice(b"Content-Type: application/pdf\r\n\r\n");
    out.extend_from_slice(pdf_bytes);
    out.extend_from_slice(b"\r\n");

    out.extend_from_slice(b"--");
    out.extend_from_slice(boundary.as_bytes());
    out.extend_from_slice(b"--\r\n");
    Ok(out)
}

/// Storefront `/api/quotes?status=received` response. Shape mirrors
/// the existing `aberp_quote_intake::payload::QuoteListResponse`.
#[derive(Debug, Deserialize)]
struct StorefrontListResponse {
    quotes: Vec<StorefrontQuote>,
}

#[derive(Debug, Deserialize)]
struct StorefrontQuote {
    id: String,
    contact: StorefrontContact,
    request: StorefrontRequest,
    #[serde(default)]
    files: Vec<StorefrontFile>,
}

#[derive(Debug, Deserialize)]
struct StorefrontContact {
    name: String,
    email: String,
    /// S401 — buyer's company. `#[serde(default)]` keeps the daemon
    /// fail-soft if a storefront build ever omits the field (it
    /// deserializes to "" rather than failing the whole listing parse);
    /// the operator panel then shows the company placeholder.
    #[serde(default)]
    company: String,
}

#[derive(Debug, Deserialize)]
struct StorefrontRequest {
    material_preference: String,
    #[serde(default)]
    quantity: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct StorefrontFile {
    filename: String,
}

// ── Audit payload structs ────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct QuotePricingFetchedPayload {
    quote_id: String,
    tenant_id: String,
    customer_email: String,
    material_grade: String,
    quantity: u32,
    cad_filename: String,
    cad_local_path: String,
    actor: String,
    idempotency_key: String,
    fetched_at: String,
}

#[derive(Debug, Serialize)]
struct QuotePricingExtractedPayload {
    quote_id: String,
    tenant_id: String,
    feature_graph_hash: String,
    extractor_version: String,
    volume_mm3: f64,
    bounding_box_mm: [f64; 3],
    feature_count: u32,
    requires_5_axis: bool,
    thin_wall_present: bool,
    actor: String,
    idempotency_key: String,
}

/// S4 / ADR-0094 Gap 2 — one machine-rate row as snapshotted into the
/// priced-audit payload (the exact slice handed to the engine).
#[derive(Debug, Serialize)]
struct MachineRateAudit {
    family: String,
    attended_rate_eur_per_min: f64,
    lights_out_factor: f64,
    unattended_capable: bool,
}

/// S6 / ADR-0094 Gap 3 — one operator gear op + the engine's resolved process,
/// as snapshotted into the priced-audit payload.
#[derive(Debug, Serialize)]
struct GearOpAudit {
    kind: String,
    module_mm: f64,
    teeth: u32,
    face_width_mm: f64,
    quality_agma: u8,
    requested_process: String,
    resolved_process: String,
}

/// S6 / ADR-0094 Gap 3 — one gear-process coefficient row as snapshotted into
/// the priced-audit payload (the exact slice handed to the engine).
#[derive(Debug, Serialize)]
struct GearProcessAudit {
    process: String,
    setup_min: f64,
    min_per_tooth: f64,
    module_exponent: f64,
    agma_quality_factor_base: f64,
    in_cycle_factor: f64,
}

#[derive(Debug, Serialize)]
struct QuotePricingPricedPayload {
    quote_id: String,
    tenant_id: String,
    engine_version: String,
    total_price_eur: f64,
    material_cost_eur: f64,
    /// S418 — renamed from `labor_cost_eur` (the line is now the
    /// geometry-driven machining cost). Pre-S418 audit rows keep the
    /// old key; the ledger is append-only history.
    machining_cost_eur: f64,
    /// S418 — new amortised CAD-CAM design cost line.
    cad_cam_cost_eur: f64,
    setup_cost_eur: f64,
    overhead_eur: f64,
    margin_eur: f64,
    /// S2 / ADR-0094 Gap 1 — the stock form the pipeline stamped onto the
    /// graph for this pricing pass (serialised as `{ "kind": …, dims… }`
    /// via the engine's serde), plus its provenance: "operator" (the
    /// operator field), "extractor" (a hint already on the graph), or
    /// "default" (the inert `RectangularBlock`). Inert quotes record
    /// `rectangular_block` / `default`.
    stock_form: StockForm,
    stock_form_source: String,
    /// S4 / ADR-0094 Gap 2 — machine-family routing + rate provenance.
    /// `routed_machine_family` = geometry route (engine `route_family`);
    /// `machine_family_override` = operator per-quote override (None=unset);
    /// `effective_machine_family` = what the rate was charged at
    /// (override ?? routed). `machine_rate_snapshot` = the exact
    /// `&[MachineRate]` slice the engine priced with ("the chosen rates").
    routed_machine_family: Option<String>,
    machine_family_override: Option<String>,
    effective_machine_family: Option<String>,
    machine_rate_snapshot: Vec<MachineRateAudit>,
    /// S6 / ADR-0094 Gap 3 — gear-generation provenance. `gear_cost_eur` is
    /// the engine's summed tooth-generation cost (0.0 on an inert quote — the
    /// wire `gear_cost` key is itself omitted when zero). `gear_ops_source` =
    /// "operator" | "extractor" | "default". `gear_ops_snapshot` records each
    /// costed gear + its resolved process (Auto → concrete via the engine's
    /// own selector); `gear_process_rate_snapshot` is the exact
    /// `&[GearProcessRate]` slice the engine priced with. All empty/zero on a
    /// no-gear quote.
    gear_cost_eur: f64,
    gear_ops_source: String,
    gear_ops_snapshot: Vec<GearOpAudit>,
    gear_process_rate_snapshot: Vec<GearProcessAudit>,
    actor: String,
    idempotency_key: String,
}

#[derive(Debug, Serialize)]
struct QuotePricingRenderedPayload {
    quote_id: String,
    tenant_id: String,
    pdf_path: String,
    pdf_size_bytes: u64,
    pdf_renderer_version: String,
    actor: String,
    idempotency_key: String,
}

#[derive(Debug, Serialize)]
struct QuotePricingPostedPayload {
    quote_id: String,
    tenant_id: String,
    feature_graph_hash: String,
    idempotent: bool,
    valid_until_iso: String,
    actor: String,
    idempotency_key: String,
}

#[derive(Debug, Serialize)]
struct QuotePricingFailedPayload {
    quote_id: String,
    tenant_id: String,
    stage: String,
    reason: String,
    attempt_n: u32,
    actor: String,
    idempotency_key: String,
}

/// S290 / PR-271 — payload for `quote.pricing_failure_classified`. Rides
/// alongside every `QuotePricingFailed` emit; the classifier verdict
/// (`transient`/`permanent`/`unknown`) drives the SPA badge + the
/// daemon's "should I auto-retry?" decision.
#[derive(Debug, Serialize)]
struct QuotePricingFailureClassifiedPayload {
    quote_id: String,
    tenant_id: String,
    /// Closed-vocab: `transient` / `permanent` / `unknown`.
    failure_kind: String,
    /// The free-text reason that fed the classifier — truncated to 1000
    /// chars, CR/LF/NUL already stripped by [`jobs::sanitize_reason`]
    /// before this payload is built. Carries the exact string the
    /// operator sees in the SPA's Error column for cross-referencing.
    last_error: String,
    attempt_n: u32,
    actor: String,
    idempotency_key: String,
}

/// S347 / PR-39 (F1+F2) — payload for `quote.priced_writeback_outcome`.
/// One row per writeback attempt; carries the granular transport-vs-app
/// verdict so a forensic walker can tell a CDN misroute from an auth
/// failure from a real 5xx without parsing the free-text `reason`.
#[derive(Debug, Serialize)]
struct QuotePricedWritebackOutcomePayload {
    quote_id: String,
    tenant_id: String,
    /// Closed-vocab tag from [`WritebackOutcome::tag`] — `success`,
    /// `routing_misconfigured`, `unauthorized`, `non_json_response`,
    /// `app_errored`, `timeout`, …
    outcome: String,
    /// HTTP status when a response was received; `null` for transport
    /// failures (timeout / connection) that never reached one.
    http_status: Option<u16>,
    /// Response Content-Type when it's part of the verdict; `null` for
    /// auth + transport outcomes.
    content_type: Option<String>,
    /// Bearer-scrubbed, ≤200-char body excerpt; `null` for success/timeout.
    body_excerpt: Option<String>,
    /// Whether the daemon should bother retrying (Transient-class verdicts).
    retryable: bool,
    attempt_n: u32,
    actor: String,
    idempotency_key: String,
}

/// S348 / PR-39 (F1) — payload for `quote.poll_outcome`. One row per FAILED
/// `GET /api/quotes?status=received` cycle; carries the same granular
/// transport-vs-app verdict as the writeback row but spans a list poll, so
/// there is no single `quote_id`. Reuses [`WritebackOutcome`]'s closed-vocab
/// `outcome` tag.
#[derive(Debug, Serialize)]
struct QuotePollOutcomePayload {
    tenant_id: String,
    /// Closed-vocab tag from [`WritebackOutcome::tag`] — `routing_misconfigured`,
    /// `unauthorized`, `non_json_response`, `app_errored`, `timeout`, …
    /// (`success` never reaches this payload — the poll only audits failures).
    outcome: String,
    /// HTTP status when a response was received; `null` for transport
    /// failures (timeout / connection) that never reached one.
    http_status: Option<u16>,
    /// Response Content-Type when it's part of the verdict; `null` for
    /// auth + transport outcomes.
    content_type: Option<String>,
    /// Bearer-scrubbed, ≤200-char body excerpt; `null` for timeout.
    body_excerpt: Option<String>,
    /// Whether the next cycle should bother retrying (Transient-class verdicts).
    retryable: bool,
    actor: String,
    idempotency_key: String,
}

// ── S282 / PR-267 — Python venv auto-discovery ────────────────────────
//
// The S279 cut shipped opt-in via the `ABERP_QUOTE_PIPELINE_PYTHON` env
// var. That violates [[trust-code-not-operator]] — an operator who
// hasn't memorised the env-var name has a silently-dormant daemon with
// no SPA signal of why. PR-267 inverts the default: code discovers the
// venv. The env var stays as an explicit override for devs/CI/debugging,
// but no operator ever needs to type it on a fresh install.
//
// The resolver runs ONCE per daemon spawn (at serve.rs boot). Its
// outcome lands in three places:
// 1. `PipelinePythonResolution` returned to the spawn site (which builds
//    `PricingPipelineConfig.python_bin` from `.resolved_path()`).
// 2. A `quote.pipeline_python_resolved` audit row written ONCE per spawn,
//    so a forensic walker can answer "what venv did this install
//    actually use?" without combing through stderr.
// 3. The shared `Arc<PythonResolutionHandle>` in `AppState`, which the
//    SPA `PricingJobsList` reads via the new
//    `GET /api/quote-pipeline/status` route to differentiate the
//    empty-state copy (dormant / active / errored).

/// Outcome of the layered Python-venv discovery. Closed-vocab — the
/// `as_kind_str` rendering is the audit-row storage form and is part
/// of the on-disk contract.
#[derive(Debug, Clone)]
pub enum PythonResolution {
    /// `ABERP_QUOTE_PIPELINE_PYTHON` env var set AND points at an existing
    /// file. Module-importable was NOT re-checked (env-var overrides are
    /// "the operator knows what they're doing"); a broken explicit
    /// override surfaces as a runtime extract failure, not at boot.
    EnvOverride { path: PathBuf },
    /// `<aberp_root>/python/aberp-cad-extract/.venv/bin/python` exists
    /// AND `aberp_cad_extract` is importable from it. The canonical
    /// post-`upgrade_prod.sh` layout.
    ProjectVenv { path: PathBuf },
    /// `<aberp_root>/.venv/bin/python` exists AND `aberp_cad_extract` is
    /// importable from it. Honors the "project-root venv" layout some
    /// devs prefer for IDE wiring.
    AltVenv { path: PathBuf },
    /// `python3` is on the system PATH AND has `aberp_cad_extract` in
    /// site-packages (e.g. pipx/global install). `path` is the absolute
    /// path resolved via `sys.executable`; never the literal `python3`.
    SystemPython { path: PathBuf },
    /// None of the four layers matched. `canonical_path` is the path
    /// the provisioning step in `upgrade_prod.sh` would create; the SPA
    /// surfaces it verbatim alongside the "run upgrade_prod.sh" hint
    /// so the operator can copy/paste.
    NotResolved { canonical_path: PathBuf },
}

impl PythonResolution {
    /// Closed-vocab storage string for the audit payload. Distinct from
    /// the human-readable `Display` impl — the audit form is the on-disk
    /// contract and must NOT drift (round-trip pinned in tests).
    pub fn as_kind_str(&self) -> &'static str {
        match self {
            PythonResolution::EnvOverride { .. } => "env_override",
            PythonResolution::ProjectVenv { .. } => "project_venv",
            PythonResolution::AltVenv { .. } => "alt_venv",
            PythonResolution::SystemPython { .. } => "system_python",
            PythonResolution::NotResolved { .. } => "not_resolved",
        }
    }

    /// The python binary the daemon should exec, or `None` on
    /// `NotResolved` (caller MUST NOT spawn the daemon).
    pub fn resolved_path(&self) -> Option<&Path> {
        match self {
            PythonResolution::EnvOverride { path }
            | PythonResolution::ProjectVenv { path }
            | PythonResolution::AltVenv { path }
            | PythonResolution::SystemPython { path } => Some(path),
            PythonResolution::NotResolved { .. } => None,
        }
    }

    /// True iff the daemon should spawn. The serve.rs boot block keys
    /// on this; on false, the SPA empty-state shows the RED card.
    pub fn is_resolved(&self) -> bool {
        self.resolved_path().is_some()
    }
}

/// Per-resolution metadata for the SPA + audit payload. Held in the
/// shared `PythonResolutionHandle`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct PipelinePythonStatus {
    /// Closed-vocab — matches [`PythonResolution::as_kind_str`].
    pub resolution_kind: String,
    /// Absolute path of the resolved interpreter, or `None` on
    /// `NotResolved`.
    pub resolved_path: Option<String>,
    /// Did `python -c "import aberp_cad_extract"` exit 0? Always true
    /// for the three auto-discovery variants (we re-check during
    /// resolution); meaningful-but-unverified for `env_override`
    /// (operator-asserted); false for `not_resolved`.
    pub module_importable: bool,
    /// On `NotResolved`, the canonical path the operator should aim
    /// for. Verbatim into the SPA RED card so they can copy/paste.
    pub canonical_path: Option<String>,
    /// On a resolved spawn, the daemon's configured poll cadence in
    /// seconds (60 in v1). The SPA folds it into the empty-state
    /// "polling every Ns" copy. `None` on `NotResolved`.
    pub poll_cadence_secs: Option<u64>,
    /// Did the daemon actually start? Set true after `tokio::spawn`
    /// in serve.rs; stays false when the resolver returned
    /// `NotResolved`. Lets the SPA distinguish "venv missing" (red)
    /// from "venv resolved but spawn errored" (amber).
    pub daemon_spawned: bool,
    /// **S286 / PR-268**: when the operator manually moved the canonical
    /// venv to a `.disabled-*`-suffixed sibling to stop the daemon (e.g.
    /// Ervin's `mv .venv .venv.disabled-pending-hotfix` after the
    /// PROD_v2.27.2 crash), the resolver detects the sibling and reports
    /// its absolute path here. The SPA renders a distinct copy
    /// ("disabled by operator pending hotfix") instead of the generic
    /// "venv missing" RED card — so the operator knows the dormant
    /// state is THEIR doing, not a missing install. `None` when no such
    /// sibling exists.
    pub operator_disabled_path: Option<String>,
}

/// Shared dormant handle. Mirrors [`crate::catalogue_push::CataloguePushHandle`]
/// — created at AppState construction with `dormant()`, written once at
/// daemon-spawn time by serve.rs, read by the new
/// `GET /api/quote-pipeline/status` route.
#[derive(Debug, Default)]
pub struct PythonResolutionHandle {
    status: Mutex<PipelinePythonStatus>,
}

impl PythonResolutionHandle {
    pub fn dormant() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Record the resolver outcome. Called ONCE per `aberp serve` boot.
    /// Subsequent calls are not expected; if one fires (e.g. a hot-
    /// reload that doesn't exist today), it overwrites — the latest
    /// is the truth.
    pub fn record(&self, status: PipelinePythonStatus) {
        if let Ok(mut s) = self.status.lock() {
            *s = status;
        }
    }

    /// Read for the SPA status route.
    pub fn snapshot(&self) -> PipelinePythonStatus {
        self.status.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

/// Canonical post-provisioning venv layout. Centralized so the resolver
/// (runtime) and the `upgrade_prod.sh` provisioner (build-time) stay in
/// lock-step.
pub fn canonical_venv_python(aberp_root: &Path) -> PathBuf {
    aberp_root
        .join("python")
        .join("aberp-cad-extract")
        .join(".venv")
        .join("bin")
        .join("python")
}

/// **S286 / PR-268** — detect an operator-disabled venv sibling.
///
/// Ervin's manual mitigation for the PROD_v2.27.2 crash was
/// `mv .venv .venv.disabled-pending-hotfix`. The resolver still
/// classifies this as `NotResolved` (the canonical path doesn't exist).
/// Without a distinct signal, the SPA RED card says "venv missing" which
/// is misleading — the operator KNOWS the venv was disabled by them.
///
/// This helper scans the canonical venv's parent dir for a sibling whose
/// name starts with `.venv.disabled` and returns its absolute path. It
/// runs ONCE per daemon boot (boundary-level discovery, like
/// `resolve_pipeline_python`), so the disk-scan cost is irrelevant.
/// Returns `None` if no sibling matches or the parent dir is unreadable.
pub fn detect_operator_disabled_venv(aberp_root: &Path) -> Option<PathBuf> {
    let parent = aberp_root.join("python").join("aberp-cad-extract");
    let entries = std::fs::read_dir(&parent).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with(".venv.disabled") && entry.path().is_dir() {
            return Some(entry.path());
        }
    }
    None
}

/// Alt-layout (project-root venv). Tried after the canonical path.
fn alt_venv_python(aberp_root: &Path) -> PathBuf {
    aberp_root.join(".venv").join("bin").join("python")
}

/// Probe whether a python binary has `aberp_cad_extract` importable.
/// Costs one `python -c "import aberp_cad_extract"` subprocess (~50-100ms).
/// Runs at boot only — never on a hot path. Stderr is suppressed so a
/// missing-module ImportError doesn't leak into the operator's stderr.
pub fn check_module_importable(python: &Path) -> bool {
    Command::new(python)
        .args(["-c", "import aberp_cad_extract"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Ask `python3` (resolved via PATH) what its absolute path is. Used
/// only for the `SystemPython` arm so the audit row records the actual
/// interpreter, not the literal string `"python3"`. Returns `None` if
/// `python3` isn't on PATH or the probe errored.
fn system_python_absolute_path() -> Option<PathBuf> {
    let out = Command::new("python3")
        .args(["-c", "import sys; print(sys.executable)"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(PathBuf::from(trimmed))
}

/// Layered fallback that picks the python binary the daemon should
/// exec. Order:
///
/// 1. `ABERP_QUOTE_PIPELINE_PYTHON` env var (explicit override —
///    devs/CI/debugging). Module-importable is NOT re-checked; the
///    override is trusted.
/// 2. `<aberp_root>/python/aberp-cad-extract/.venv/bin/python` — the
///    canonical post-`upgrade_prod.sh` layout. Module-importable is
///    re-checked.
/// 3. `<aberp_root>/.venv/bin/python` — alt project-root layout.
///    Re-checked.
/// 4. System `python3` IFF `aberp_cad_extract` is importable from it
///    (pipx / global install). Re-checked.
/// 5. `NotResolved` — caller must NOT spawn; SPA shows RED.
///
/// Boundary-level discovery — runs ONCE per `aberp serve` boot. The
/// resolved path is then stable for the lifetime of the process. No
/// retry, no hot-reload.
pub fn resolve_pipeline_python(aberp_root: &Path) -> PythonResolution {
    // 1. Env-var override
    if let Ok(s) = std::env::var("ABERP_QUOTE_PIPELINE_PYTHON") {
        let p = PathBuf::from(&s);
        if p.is_file() {
            return PythonResolution::EnvOverride { path: p };
        }
        tracing::warn!(
            env_value = %s,
            "ABERP_QUOTE_PIPELINE_PYTHON points at a missing file; falling through to auto-discovery"
        );
    }

    // 2. Canonical project venv
    let canonical = canonical_venv_python(aberp_root);
    if canonical.is_file() && check_module_importable(&canonical) {
        return PythonResolution::ProjectVenv { path: canonical };
    }

    // 3. Alt project-root venv
    let alt = alt_venv_python(aberp_root);
    if alt.is_file() && check_module_importable(&alt) {
        return PythonResolution::AltVenv { path: alt };
    }

    // 4. System python3 IFF the module is already there
    if let Some(sys_py) = system_python_absolute_path() {
        if check_module_importable(&sys_py) {
            return PythonResolution::SystemPython { path: sys_py };
        }
    }

    PythonResolution::NotResolved {
        canonical_path: canonical,
    }
}

/// Render a `PipelinePythonStatus` snapshot from a `PythonResolution`
/// + the daemon's intended cadence + the operator-disabled-venv sibling
/// (`None` if not detected; see [`detect_operator_disabled_venv`]).
/// `daemon_spawned` is left false here; serve.rs flips it true on a
/// successful `tokio::spawn`.
pub fn status_from_resolution(
    resolution: &PythonResolution,
    poll_cadence_secs: Option<u64>,
    operator_disabled_path: Option<PathBuf>,
) -> PipelinePythonStatus {
    let (resolved_path, canonical_path) = match resolution {
        PythonResolution::EnvOverride { path }
        | PythonResolution::ProjectVenv { path }
        | PythonResolution::AltVenv { path }
        | PythonResolution::SystemPython { path } => {
            (Some(path.to_string_lossy().into_owned()), None)
        }
        PythonResolution::NotResolved { canonical_path } => {
            (None, Some(canonical_path.to_string_lossy().into_owned()))
        }
    };
    // env_override is trusted-but-unverified; the other three resolved
    // arms had their module-importable check confirmed in the resolver.
    let module_importable = !matches!(resolution, PythonResolution::NotResolved { .. });
    // S286 / PR-268 — only carry the disabled-sibling hint when the
    // resolver said `NotResolved`. A resolved daemon shouldn't surface a
    // "operator disabled" hint even if a stale `.venv.disabled-*` exists
    // alongside; the resolver already picked the working one.
    let operator_disabled_path = if matches!(resolution, PythonResolution::NotResolved { .. }) {
        operator_disabled_path.map(|p| p.to_string_lossy().into_owned())
    } else {
        None
    };
    PipelinePythonStatus {
        resolution_kind: resolution.as_kind_str().to_string(),
        resolved_path,
        module_importable,
        canonical_path,
        poll_cadence_secs: resolution
            .is_resolved()
            .then_some(poll_cadence_secs)
            .flatten(),
        daemon_spawned: false,
        operator_disabled_path,
    }
}

// Audit payload for `quote.pipeline_python_resolved`. Emitted ONCE per
// daemon-spawn by serve.rs (see `emit_python_resolved_audit`).
#[derive(Debug, Serialize)]
struct PipelinePythonResolvedPayload {
    tenant_id: String,
    resolution_kind: String,
    resolved_path: Option<String>,
    module_importable: bool,
    canonical_path: Option<String>,
    actor: String,
    idempotency_key: String,
}

/// Append the `quote.pipeline_python_resolved` audit row. Idempotency
/// key is `<tenant>:<resolution_kind>:<resolved_path>` so re-spawns
/// after a restart re-fire (a new attempt deserves a new row) but a
/// double-call inside the same spawn-window is a no-op via the audit-
/// ledger's UNIQUE defence.
pub fn emit_python_resolved_audit(
    db_path: &Path,
    tenant_id: &str,
    binary_hash: BinaryHash,
    login: &str,
    status: &PipelinePythonStatus,
) -> Result<()> {
    let mut conn =
        duckdb::Connection::open(db_path).context("open DB for python-resolved audit")?;
    audit_ensure_schema(&conn).context("ensure audit-ledger schema")?;
    let tx = conn.transaction().context("open python-resolved tx")?;
    let meta = LedgerMeta::new(TenantId::new(tenant_id).context("tenant id")?, binary_hash);
    let actor = Actor::from_local_cli(Ulid::new().to_string(), login);
    let key_path = status.resolved_path.clone().unwrap_or_default();
    let payload = PipelinePythonResolvedPayload {
        tenant_id: tenant_id.to_string(),
        resolution_kind: status.resolution_kind.clone(),
        resolved_path: status.resolved_path.clone(),
        module_importable: status.module_importable,
        canonical_path: status.canonical_path.clone(),
        actor: "system".to_string(),
        idempotency_key: format!(
            "quote_pipeline_python_resolved:{tenant_id}:{}:{key_path}",
            status.resolution_kind
        ),
    };
    let bytes = serde_json::to_vec(&payload).context("encode python-resolved payload")?;
    append_in_tx(
        &tx,
        &meta,
        EventKind::PipelinePythonResolved,
        bytes,
        actor,
        Some(payload.idempotency_key.clone()),
    )
    .context("append PipelinePythonResolved")?;
    tx.commit().context("commit python-resolved")?;
    Ok(())
}

// S288 / PR-269 — boot-time row recording that the orphan
// `quote_pricing_jobs_tenant_state_idx` was detected and dropped.
// Emitted at most once per install (`migrate_secondary_index_with_report`
// returns `false` on every boot after the first). The audit row's
// idempotency key is `quote_pricing_jobs_index_migrated:<tenant>` so a
// pathological replay still collides at the audit-ledger's UNIQUE
// defence — defence-in-depth.
#[derive(Debug, Serialize)]
struct QuotePricingJobsIndexMigratedPayload {
    tenant_id: String,
    index_name: String,
    dropped_at: String,
    actor: String,
    idempotency_key: String,
}

/// Append the one-shot `quote.pricing_jobs_index_migrated` audit row.
/// Call site is `serve.rs` boot, ONLY when
/// [`crate::quote_pricing_jobs::migrate_secondary_index_with_report`]
/// returned `Ok(true)`. Re-emit on a second boot is structurally
/// impossible (the migration helper returns `false` once the index is
/// gone), so the UNIQUE idempotency-key constraint is belt-and-braces.
pub fn emit_index_migrated_audit(
    db_path: &Path,
    tenant_id: &str,
    binary_hash: BinaryHash,
    login: &str,
) -> Result<()> {
    let mut conn = duckdb::Connection::open(db_path).context("open DB for index-migrated audit")?;
    audit_ensure_schema(&conn).context("ensure audit-ledger schema")?;
    let tx = conn.transaction().context("open index-migrated tx")?;
    let meta = LedgerMeta::new(TenantId::new(tenant_id).context("tenant id")?, binary_hash);
    let actor = Actor::from_local_cli(Ulid::new().to_string(), login);
    let dropped_at = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .context("format dropped_at")?;
    let payload = QuotePricingJobsIndexMigratedPayload {
        tenant_id: tenant_id.to_string(),
        index_name: "quote_pricing_jobs_tenant_state_idx".to_string(),
        dropped_at,
        actor: "system".to_string(),
        idempotency_key: format!("quote_pricing_jobs_index_migrated:{tenant_id}"),
    };
    let bytes = serde_json::to_vec(&payload).context("encode index-migrated payload")?;
    append_in_tx(
        &tx,
        &meta,
        EventKind::QuotePricingJobsIndexMigrated,
        bytes,
        actor,
        Some(payload.idempotency_key.clone()),
    )
    .context("append QuotePricingJobsIndexMigrated")?;
    tx.commit().context("commit index-migrated")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── S2 / ADR-0094 Gap 1 — stock-form stamp precedence (pure) ──────
    fn mk_graph(stock_form: StockForm) -> FeatureGraph {
        FeatureGraph {
            gears: Vec::new(),
            schema_version: FeatureGraph::SCHEMA_VERSION,
            bounding_box_mm: [10.0, 10.0, 10.0],
            volume_mm3: 1000.0,
            surface_area_mm2: 0.0,
            material_grade: "6061-T6".to_string(),
            features: Vec::new(),
            requires_5_axis: false,
            thin_wall_present: false,
            stock_form,
        }
    }

    #[test]
    fn operator_stock_form_reads_each_discriminant() {
        // round bar: stock_od_mm carries the diameter, stock_length_mm the length.
        assert_eq!(
            operator_stock_form(Some("round_bar"), Some(40.0), None, Some(30.0)),
            Some(StockForm::RoundBar {
                diameter_mm: 40.0,
                length_mm: 30.0,
            }),
        );
        // tube: od / id / length all carried through.
        assert_eq!(
            operator_stock_form(Some("tube"), Some(100.0), Some(80.0), Some(15.0)),
            Some(StockForm::Tube {
                od_mm: 100.0,
                id_mm: 80.0,
                length_mm: 15.0,
            }),
        );
        // Unset / inert / unrecognised discriminants ⇒ None: the caller keeps
        // whatever form the graph already carries (extractor hint or default).
        assert_eq!(operator_stock_form(None, None, None, None), None);
        assert_eq!(
            operator_stock_form(Some("rectangular_block"), None, None, None),
            None
        );
        assert_eq!(
            operator_stock_form(Some("garbage"), Some(1.0), None, None),
            None
        );
    }

    #[test]
    fn stamp_precedence_operator_beats_extractor_hint() {
        // Graph arrives with an extractor hint (round bar); operator forces a tube.
        let mut g = mk_graph(StockForm::RoundBar {
            diameter_mm: 50.0,
            length_mm: 20.0,
        });
        let op = Some(StockForm::Tube {
            od_mm: 100.0,
            id_mm: 80.0,
            length_mm: 15.0,
        });
        let (chosen, source) = stamp_stock_form(&mut g, op);
        assert_eq!(
            chosen,
            StockForm::Tube {
                od_mm: 100.0,
                id_mm: 80.0,
                length_mm: 15.0
            }
        );
        assert_eq!(source, "operator");
        assert_eq!(g.stock_form, chosen);
    }

    #[test]
    fn stamp_precedence_extractor_hint_beats_default() {
        // No operator field, but the extractor already wrote a round bar onto
        // the graph: keep it, provenance "extractor".
        let hint = StockForm::RoundBar {
            diameter_mm: 40.0,
            length_mm: 30.0,
        };
        let mut g = mk_graph(hint);
        let (chosen, source) = stamp_stock_form(&mut g, None);
        assert_eq!(chosen, hint);
        assert_eq!(source, "extractor");
        assert_eq!(g.stock_form, hint);
    }

    #[test]
    fn stamp_inert_when_no_operator_and_no_hint() {
        // The inert default: no operator field, graph carries RectangularBlock.
        // Result is RectangularBlock / "default" — today's exact pricing path,
        // so existing quotes are byte-identical.
        let mut g = mk_graph(StockForm::RectangularBlock);
        let (chosen, source) = stamp_stock_form(&mut g, None);
        assert_eq!(chosen, StockForm::RectangularBlock);
        assert_eq!(source, "default");
        assert_eq!(g.stock_form, StockForm::RectangularBlock);
    }

    #[test]
    fn backoff_follows_5_15_60_then_cadence() {
        let cadence = Duration::from_secs(60);
        assert_eq!(backoff_duration(0, cadence), Duration::from_secs(5));
        assert_eq!(backoff_duration(1, cadence), Duration::from_secs(15));
        assert_eq!(backoff_duration(2, cadence), Duration::from_secs(60));
        assert_eq!(backoff_duration(3, cadence), cadence);
        assert_eq!(backoff_duration(99, cadence), cadence);
    }

    #[test]
    fn parse_stock_status_round_trips_closed_vocab() {
        assert_eq!(
            parse_stock_status("in_stock").unwrap(),
            StockStatus::InStock
        );
        assert_eq!(
            parse_stock_status("source_1_2d").unwrap(),
            StockStatus::Source1_2d
        );
        assert_eq!(
            parse_stock_status("source_3_7d").unwrap(),
            StockStatus::Source3_7d
        );
        assert_eq!(
            parse_stock_status("special_order").unwrap(),
            StockStatus::SpecialOrder
        );
        assert!(parse_stock_status("garbage").is_err());
    }

    #[test]
    fn build_priced_multipart_carries_meta_and_pdf() {
        let boundary = "test-bdry";
        let body = build_priced_multipart(
            boundary,
            "blake3:abcd",
            "2026-07-06",
            "{\"k\":1}",
            b"%PDF-1.5 fakebody",
            false,
        )
        .expect("build");
        let s = String::from_utf8_lossy(&body).to_string();
        assert!(s.contains("--test-bdry"));
        assert!(s.contains("name=\"meta\""));
        assert!(s.contains("Content-Type: application/json"));
        assert!(s.contains("blake3:abcd"));
        assert!(s.contains("2026-07-06"));
        assert!(s.contains("name=\"pdf\""));
        assert!(s.contains("Content-Type: application/pdf"));
        assert!(s.contains("--test-bdry--"));
        assert!(s.contains("%PDF-1.5 fakebody"));
    }

    /// ADR-0004 mandates `breakdown_json` as a JSON OBJECT inside
    /// `meta` (not a string). The hand-rolled multipart must emit the
    /// breakdown as a JSON value, not the encoded string. Loud-fail
    /// pin so the storefront's `breakdown_json must be a JSON object`
    /// rejection (line 46 of `+server.ts`) doesn't bite us later.
    #[test]
    fn priced_multipart_breakdown_is_json_object_not_string() {
        let body = build_priced_multipart(
            "b",
            "blake3:deadbeef",
            "2026-07-06",
            "{\"total\":42.0}",
            b"x",
            false,
        )
        .expect("build");
        let s = String::from_utf8_lossy(&body).to_string();
        // The breakdown must appear as `"breakdown_json":{"total":42.0}`,
        // NOT `"breakdown_json":"{\"total\":42.0}"` (a string).
        assert!(
            s.contains("\"breakdown_json\":{"),
            "breakdown_json must be a nested object, body: {s}"
        );
        assert!(
            !s.contains("\"breakdown_json\":\""),
            "breakdown_json must NOT be a string"
        );
    }

    #[test]
    fn priced_multipart_stamps_extractor_and_engine_versions() {
        let body = build_priced_multipart("b", "blake3:1", "2026-07-06", "{}", b"x", false)
            .expect("build");
        let s = String::from_utf8_lossy(&body).to_string();
        assert!(
            s.contains("\"extractor_version\":"),
            "missing extractor_version"
        );
        assert!(s.contains("\"engine_version\":"), "missing engine_version");
        assert!(s.contains("\"stock_alert\":false"), "missing stock_alert");
    }

    /// S325 / PR-25 — the re-render path passes `stock_alert: true`; the
    /// meta must carry the flipped flag so the S323 storefront `/priced`
    /// relax overwrites the stored PDF + flips the customer-side flag.
    #[test]
    fn priced_multipart_carries_stock_alert_true_for_rerender() {
        let body =
            build_priced_multipart("b", "blake3:1", "2026-07-06", "{}", b"x", true).expect("build");
        let s = String::from_utf8_lossy(&body).to_string();
        assert!(
            s.contains("\"stock_alert\":true"),
            "re-render meta must carry stock_alert:true, body: {s}"
        );
    }

    #[test]
    fn parse_priced_writeback_ok_recognises_idempotent_flag() {
        let fresh: PricedWritebackOk =
            serde_json::from_str(r#"{"status":"quoted"}"#).expect("fresh");
        assert!(!fresh.idempotent.unwrap_or(false));
        let idem: PricedWritebackOk =
            serde_json::from_str(r#"{"status":"quoted","idempotent":true}"#).expect("idem");
        assert!(idem.idempotent.unwrap_or(false));
    }

    #[test]
    fn convert_materials_with_unknown_stock_status_is_loud() {
        // Need to construct a local Material; instead test the parse
        // helper directly to keep the surface narrow.
        assert!(parse_stock_status("not_a_status").is_err());
    }

    // ── S282 / PR-267 — Python venv auto-discovery ────────────────────

    #[test]
    fn python_resolution_kind_strings_are_closed_vocab() {
        // The audit-row + SPA contract: every variant has a stable,
        // distinct lowercase-snake string. A future contributor who
        // adds an arm without updating `as_kind_str` will be caught
        // by the empty-state classifier in the SPA (which keys on
        // exactly `"not_resolved"`).
        let env = PythonResolution::EnvOverride {
            path: PathBuf::from("/bin/sh"),
        };
        let proj = PythonResolution::ProjectVenv {
            path: PathBuf::from("/repo/python/aberp-cad-extract/.venv/bin/python"),
        };
        let alt = PythonResolution::AltVenv {
            path: PathBuf::from("/repo/.venv/bin/python"),
        };
        let sys = PythonResolution::SystemPython {
            path: PathBuf::from("/usr/bin/python3"),
        };
        let nope = PythonResolution::NotResolved {
            canonical_path: PathBuf::from("/repo/python/aberp-cad-extract/.venv/bin/python"),
        };
        assert_eq!(env.as_kind_str(), "env_override");
        assert_eq!(proj.as_kind_str(), "project_venv");
        assert_eq!(alt.as_kind_str(), "alt_venv");
        assert_eq!(sys.as_kind_str(), "system_python");
        assert_eq!(nope.as_kind_str(), "not_resolved");
        // Pairwise-distinct — a collision would mis-route the SPA
        // empty-state classifier (e.g. RED card on a healthy install).
        let all = [
            env.as_kind_str(),
            proj.as_kind_str(),
            alt.as_kind_str(),
            sys.as_kind_str(),
            nope.as_kind_str(),
        ];
        for i in 0..all.len() {
            for j in (i + 1)..all.len() {
                assert_ne!(all[i], all[j]);
            }
        }
    }

    #[test]
    fn python_resolution_resolved_path_is_some_for_resolved_arms() {
        let env = PythonResolution::EnvOverride {
            path: PathBuf::from("/a"),
        };
        assert_eq!(env.resolved_path(), Some(Path::new("/a")));
        assert!(env.is_resolved());
        let nope = PythonResolution::NotResolved {
            canonical_path: PathBuf::from("/b"),
        };
        assert_eq!(nope.resolved_path(), None);
        assert!(!nope.is_resolved());
    }

    #[test]
    fn canonical_venv_python_lands_under_project_layout() {
        let root = Path::new("/Users/op/ABERP");
        let p = canonical_venv_python(root);
        assert_eq!(
            p,
            PathBuf::from("/Users/op/ABERP/python/aberp-cad-extract/.venv/bin/python")
        );
    }

    #[test]
    fn status_from_resolution_renders_resolved_path_and_cadence() {
        let r = PythonResolution::ProjectVenv {
            path: PathBuf::from("/a/b/python"),
        };
        let s = status_from_resolution(&r, Some(60), None);
        assert_eq!(s.resolution_kind, "project_venv");
        assert_eq!(s.resolved_path.as_deref(), Some("/a/b/python"));
        assert_eq!(s.canonical_path, None);
        assert!(s.module_importable);
        assert_eq!(s.poll_cadence_secs, Some(60));
        // daemon_spawned is the BOOT-block's job, not the helper's.
        assert!(!s.daemon_spawned);
    }

    #[test]
    fn status_from_resolution_env_override_is_trusted_but_unverified() {
        // `env_override` is operator-asserted; the resolver does NOT
        // re-check `aberp_cad_extract` is importable. The audit row
        // reflects that posture by reporting `module_importable: true`
        // (operator's claim) — runtime extract failures then surface
        // the broken override as a pricing-job audit row, not at boot.
        let r = PythonResolution::EnvOverride {
            path: PathBuf::from("/explicit/python"),
        };
        let s = status_from_resolution(&r, Some(60), None);
        assert_eq!(s.resolution_kind, "env_override");
        assert!(s.module_importable);
    }

    #[test]
    fn status_from_resolution_not_resolved_clears_path_and_cadence() {
        let r = PythonResolution::NotResolved {
            canonical_path: PathBuf::from("/missing/canonical"),
        };
        let s = status_from_resolution(&r, Some(60), None);
        assert_eq!(s.resolution_kind, "not_resolved");
        assert_eq!(s.resolved_path, None);
        assert_eq!(s.canonical_path.as_deref(), Some("/missing/canonical"));
        assert!(!s.module_importable);
        // Cadence is meaningless on a dormant daemon — clear it so the
        // SPA's "polling every Ns" copy doesn't render.
        assert_eq!(s.poll_cadence_secs, None);
        assert!(!s.daemon_spawned);
    }

    #[test]
    fn python_resolution_handle_dormant_default_then_record_roundtrip() {
        let h = PythonResolutionHandle::dormant();
        // Dormant snapshot = default — no resolution recorded yet.
        let empty = h.snapshot();
        assert_eq!(empty.resolution_kind, "");
        assert_eq!(empty.resolved_path, None);
        assert!(!empty.daemon_spawned);

        let r = PythonResolution::SystemPython {
            path: PathBuf::from("/usr/local/bin/python3"),
        };
        let mut s = status_from_resolution(&r, Some(60), None);
        s.daemon_spawned = true;
        assert_eq!(s.operator_disabled_path, None);
        h.record(s.clone());
        let out = h.snapshot();
        assert_eq!(out.resolution_kind, "system_python");
        assert_eq!(out.resolved_path.as_deref(), Some("/usr/local/bin/python3"));
        assert!(out.daemon_spawned);
        assert_eq!(out.poll_cadence_secs, Some(60));
    }

    #[test]
    fn check_module_importable_rejects_a_non_python_binary() {
        // `/bin/sh` is universally present on macOS + Linux dev/test
        // hosts. It accepts the `-c <script>` flag (so the subprocess
        // launches) but `import aberp_cad_extract` is not a shell
        // statement; it should exit non-zero. This is the FAIL path
        // — guarantees we don't silently classify any-binary-with-
        // exec-bit as a working Python.
        let sh = PathBuf::from("/bin/sh");
        if !sh.exists() {
            return; // unsupported host — skip
        }
        assert!(!check_module_importable(&sh));
    }

    // ── S286 / PR-268 — supervisor pins ───────────────────────────────

    #[test]
    fn sanitize_panic_msg_strips_control_chars_and_truncates() {
        let s = "boom\r\nfollowup\0tail";
        assert_eq!(sanitize_panic_msg(s), "boomfollowuptail");
        let long: String = "a".repeat(2000);
        let cleaned = sanitize_panic_msg(&long);
        assert_eq!(cleaned.chars().count(), 1000);
    }

    #[test]
    fn panic_payload_to_string_extracts_static_str() {
        let payload: Box<dyn std::any::Any + Send> = Box::new("boom");
        // `Box<&'static str>` is the typical `panic!("boom")` shape.
        let s = panic_payload_to_string(payload);
        assert_eq!(s, "boom");
    }

    #[test]
    fn panic_payload_to_string_extracts_owned_string() {
        let payload: Box<dyn std::any::Any + Send> = Box::new(String::from("formatted"));
        // `Box<String>` is the typical `panic!("{}", x)` shape (after a
        // `format!`-style invocation; the std lib boxes a String).
        let s = panic_payload_to_string(payload);
        assert_eq!(s, "formatted");
    }

    #[test]
    fn panic_payload_to_string_handles_non_string_payload() {
        // `panic_any(42i32)` would box an i32; our extractor must NOT
        // panic on it. The exact placeholder text is part of the audit
        // payload contract, so this pin doubles as a no-drift guard.
        let payload: Box<dyn std::any::Any + Send> = Box::new(42i32);
        assert_eq!(
            panic_payload_to_string(payload),
            "<non-string panic payload>"
        );
    }

    #[test]
    fn panic_payload_sanitization_applies_to_both_branches() {
        // CR/LF/NUL stripping happens BEFORE the audit row is encoded;
        // a forged extra line in the panic message must not bleed
        // through into the ledger.
        let payload: Box<dyn std::any::Any + Send> = Box::new(String::from("oops\r\nfake\0bytes"));
        assert_eq!(panic_payload_to_string(payload), "oopsfakebytes");
    }

    /// S286 / PR-268 — operator-disabled venv detection. When the
    /// operator manually moves the canonical venv aside (Ervin's
    /// `mv .venv .venv.disabled-pending-hotfix` after PROD_v2.27.2), the
    /// resolver still returns `NotResolved` (the canonical path is
    /// missing) but the disabled-sibling is detected so the SPA can
    /// render a distinct "disabled by operator" hint.
    #[test]
    fn detect_operator_disabled_venv_finds_renamed_sibling() {
        let mut root = std::env::temp_dir();
        root.push(format!("aberp-s286-disabled-{}", Ulid::new()));
        let cad_dir = root.join("python").join("aberp-cad-extract");
        std::fs::create_dir_all(&cad_dir).expect("mkdir cad_dir");
        let disabled = cad_dir.join(".venv.disabled-pending-hotfix");
        std::fs::create_dir(&disabled).expect("create disabled sibling");
        // Detect should find the renamed sibling.
        let found = detect_operator_disabled_venv(&root);
        assert_eq!(found, Some(disabled));
        // No disabled sibling → returns None.
        std::fs::remove_dir_all(root.join("python")).expect("clean");
        let cad_dir = root.join("python").join("aberp-cad-extract");
        std::fs::create_dir_all(&cad_dir).expect("re-mkdir");
        assert_eq!(detect_operator_disabled_venv(&root), None);
        // Different `.disabled-*` suffix is also caught (the hotfix uses
        // a specific suffix but the detector accepts any).
        let other = cad_dir.join(".venv.disabled-by-operator");
        std::fs::create_dir(&other).expect("create alt sibling");
        assert_eq!(detect_operator_disabled_venv(&root), Some(other));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// S286 / PR-268 — only a `NotResolved` outcome carries the
    /// `operator_disabled_path` hint. A resolved daemon shouldn't surface
    /// the disabled sibling even if it exists (the resolver picked the
    /// working venv; the renamed one is irrelevant).
    #[test]
    fn status_from_resolution_clears_operator_disabled_for_resolved_outcomes() {
        let r = PythonResolution::ProjectVenv {
            path: PathBuf::from("/working/python"),
        };
        let s = status_from_resolution(
            &r,
            Some(60),
            Some(PathBuf::from("/stale/.venv.disabled-something")),
        );
        assert_eq!(s.operator_disabled_path, None);
        // NotResolved carries it through.
        let nr = PythonResolution::NotResolved {
            canonical_path: PathBuf::from("/missing"),
        };
        let s = status_from_resolution(
            &nr,
            Some(60),
            Some(PathBuf::from("/disabled/.venv.disabled-pending-hotfix")),
        );
        assert_eq!(
            s.operator_disabled_path.as_deref(),
            Some("/disabled/.venv.disabled-pending-hotfix")
        );
    }

    /// Round-trip the daemon-panic audit into a stdlib-tmp DuckDB. Pins
    /// the helper's schema-ensure + tx-commit shape so a future refactor
    /// of `emit_daemon_panicked_audit` doesn't silently swallow appends.
    /// Stdlib-tmp (no `tempfile` dep — see Cargo.toml) so this test stays
    /// self-contained for the S286 hotfix surface.
    #[test]
    fn emit_daemon_panicked_audit_writes_one_row() {
        use aberp_audit_ledger::{recent_entries, BinaryHash, TenantId};
        let mut db_path = std::env::temp_dir();
        db_path.push(format!("aberp-s286-panic-{}.duckdb", Ulid::new()));
        // Best-effort: clean up a prior leftover from a flaky run.
        let _ = std::fs::remove_file(&db_path);
        let deps = PricingPipelineDeps {
            db_path: db_path.clone(),
            tenant: TenantId::new("T").expect("tid"),
            binary_hash: BinaryHash::from_bytes([0u8; 32]),
            operator_login: "ervin".to_string(),
        };
        emit_daemon_panicked_audit(&deps, "boom", 1, Some("q-1".to_string())).expect("emit");
        emit_daemon_panicked_audit(&deps, "second", 2, None).expect("emit2");
        let conn = duckdb::Connection::open(&db_path).expect("reopen");
        let entries = recent_entries(&conn, 10).expect("recent");
        // Two panic rows in seq-DESC order: "second" is newest.
        assert_eq!(entries.len(), 2);
        for e in &entries {
            assert_eq!(e.kind.as_str(), "quote.pricing_daemon_panicked");
        }
        let newest_payload: serde_json::Value =
            serde_json::from_slice(&entries[0].payload).expect("decode");
        assert_eq!(newest_payload["panic_msg"], "second");
        assert_eq!(newest_payload["restart_count_since_boot"], 2);
        assert!(newest_payload["last_known_quote_id"].is_null());
        drop(conn);
        let _ = std::fs::remove_file(&db_path);
    }

    // ── S290 / PR-271 — failure classifier pins ───────────────────────

    /// The motivating prod row: storefront accepted a STEP file (legacy
    /// data, before the storefront content-sniff lands), extractor stubs
    /// out with NotImplementedError. EVERY retry hits the same error.
    /// Classifier must report Permanent so the SPA badge tells the
    /// operator not to bother clicking Retry.
    #[test]
    fn s290_classify_step_extractor_stub_is_permanent() {
        // The exact `Display` shape the wrapper emits when the Python
        // CLI exits 2 with the STEP stub message — verbatim from
        // `python/aberp-cad-extract/aberp_cad_extract/extractors/step.py`
        // + the wrapper's `ExtractError::NonZeroExit` Display.
        let reason = "subprocess exited with code Some(2): \
            NotImplementedError: STEP extraction not yet implemented in v1; \
            please supply STL. Slated for S270 alongside the Rust subprocess wrapper.";
        assert_eq!(
            classify_failure("extract", reason),
            FailureKind::Permanent,
            "STEP-stub failures must be Permanent — auto-retry is wasted cycles"
        );
    }

    #[test]
    fn s290_classify_material_not_in_catalogue_is_permanent() {
        let reason = "material grade `unknown` is not in the catalogue snapshot";
        assert_eq!(classify_failure("price", reason), FailureKind::Permanent);
    }

    // ── PR-273 — STEP extractor data-quality rules ────────────────────

    /// PR-273: STEP extractor rejects assemblies with the exact
    /// `Display` shape `ExtractError::NonZeroExit` produces around the
    /// Python-side ValueError. The "step file" substring must trip the
    /// new classifier rule → Permanent.
    #[test]
    fn pr273_classify_step_assembly_rejection_is_permanent() {
        let reason = "subprocess exited with code Some(2): \
            ValueError: STEP file contains an assembly with 3 solids; \
            only single-part STEP is supported in v1";
        assert_eq!(
            classify_failure("extract", reason),
            FailureKind::Permanent,
            "STEP assembly rejection must classify Permanent — operator must \
             trim the file before retry"
        );
    }

    /// PR-273: STEP file with no transferable solid body (rare but
    /// reachable when a customer uploads a STEP file that only contains
    /// surfaces / sheets / wireframes).
    #[test]
    fn pr273_classify_step_no_solid_body_is_permanent() {
        let reason = "subprocess exited with code Some(2): \
            ValueError: STEP file contains no solid body; only solid-part \
            STEP is supported in v1";
        assert_eq!(classify_failure("extract", reason), FailureKind::Permanent);
    }

    /// PR-273: malformed STEP file that OCCT cannot parse.
    #[test]
    fn pr273_classify_step_parse_failure_is_permanent() {
        let reason = "subprocess exited with code Some(2): \
            ValueError: STEP file could not be parsed (OCCT ReadFile status=3)";
        assert_eq!(classify_failure("extract", reason), FailureKind::Permanent);
    }

    /// PR-273: case-insensitive — `STEP` and `step` both match. The
    /// classifier lowercases the reason before matching, so the
    /// uppercase `STEP file` we emit from Python lands on the lowercase
    /// substring rule.
    #[test]
    fn pr273_classify_step_rule_is_case_insensitive() {
        let upper = "ValueError: STEP file contains an assembly with 2 solids";
        let lower = "valueerror: step file contains an assembly with 2 solids";
        assert_eq!(classify_failure("extract", upper), FailureKind::Permanent);
        assert_eq!(classify_failure("extract", lower), FailureKind::Permanent);
    }

    /// PR-273: the "step file" rule is scoped to the extract stage.
    /// Other stages defaulting to Unknown on the same substring is the
    /// honest verdict — a "step file" mention at the post stage would
    /// be surprising and we don't want to misclassify based on it.
    #[test]
    fn pr273_classify_step_rule_is_extract_stage_only() {
        let reason = "ValueError: STEP file contains an assembly with 2 solids";
        // extract → Permanent
        assert_eq!(classify_failure("extract", reason), FailureKind::Permanent);
        // other stages → Unknown
        assert_eq!(classify_failure("price", reason), FailureKind::Unknown);
        assert_eq!(classify_failure("render", reason), FailureKind::Unknown);
    }

    // ── PR-274 / S297 F1 — storefront-vs-extractor whitelist mismatch ─

    /// PR-274 / S297 F1: the storefront accepts `.iges`, `.dxf`,
    /// `.sldprt`, `.obj`, etc., but the Python CLI dispatcher only routes
    /// `.stl`/`.step`/`.stp`. Anything else raises `ValueError` with the
    /// literal "Unsupported file extension '.iges'…" string. Without
    /// this rule the classifier fell through to `Unknown`, so the SPA
    /// badge gave the operator no signal that Retry was futile.
    #[test]
    fn pr274_classify_unsupported_extension_is_permanent() {
        let reason = "subprocess exited with code Some(2): \
            ValueError: Unsupported file extension '.iges'. \
            Supported: .stl, .step, .stp";
        assert_eq!(
            classify_failure("extract", reason),
            FailureKind::Permanent,
            "unsupported-extension failures must classify Permanent — \
             customer must re-upload, retry can never help"
        );
    }

    /// PR-274 / S297 F1: case-insensitive — uppercase / mixed-case
    /// "Unsupported File Extension" must still hit the rule.
    #[test]
    fn pr274_classify_unsupported_extension_is_case_insensitive() {
        let upper = "ValueError: Unsupported File Extension '.DXF'. \
            Supported: .stl, .step, .stp";
        assert_eq!(classify_failure("extract", upper), FailureKind::Permanent);
    }

    /// PR-274 / S297 F1: the rule is scoped to the extract stage so a
    /// hypothetical future Display string mentioning "unsupported file
    /// extension" at another stage doesn't silently get reclassified.
    #[test]
    fn pr274_classify_unsupported_extension_rule_is_extract_stage_only() {
        let reason = "ValueError: Unsupported file extension '.iges'";
        assert_eq!(classify_failure("extract", reason), FailureKind::Permanent);
        assert_eq!(classify_failure("price", reason), FailureKind::Unknown);
        assert_eq!(classify_failure("post", reason), FailureKind::Unknown);
    }

    #[test]
    fn s290_classify_schema_version_mismatch_is_permanent() {
        // Wrapper-side mismatch.
        assert_eq!(
            classify_failure(
                "extract",
                "FeatureGraph _schema_version mismatch: expected 1, got 2"
            ),
            FailureKind::Permanent
        );
        // Engine-side mismatch (defence-in-depth — same verdict).
        assert_eq!(
            classify_failure(
                "price",
                "feature-graph schema version 2 is not understood by engine (supports 1)"
            ),
            FailureKind::Permanent
        );
    }

    #[test]
    fn s290_classify_margin_floor_violation_is_permanent() {
        let reason = "computed margin 0.0500 below configured floor 0.1500 \
            (total_price=10.0000)";
        assert_eq!(classify_failure("price", reason), FailureKind::Permanent);
    }

    #[test]
    fn s290_classify_post_4xx_is_permanent() {
        assert_eq!(
            classify_failure(
                "post",
                "priced-writeback HTTP 401 Unauthorized body=invalid token"
            ),
            FailureKind::Permanent
        );
        assert_eq!(
            classify_failure(
                "post",
                "priced-writeback HTTP 403 Forbidden body=missing scope"
            ),
            FailureKind::Permanent
        );
        // Validation 4xx — operator review needed.
        assert_eq!(
            classify_failure(
                "post",
                "priced-writeback HTTP 422 Unprocessable body=schema mismatch"
            ),
            FailureKind::Permanent
        );
    }

    #[test]
    fn s290_classify_post_5xx_and_transport_are_transient() {
        assert_eq!(
            classify_failure(
                "post",
                "priced-writeback HTTP 503 Service Unavailable body=down"
            ),
            FailureKind::Transient
        );
        assert_eq!(
            classify_failure("post", "operation timed out after 30s"),
            FailureKind::Transient
        );
        assert_eq!(
            classify_failure("post", "connection reset by peer"),
            FailureKind::Transient
        );
    }

    #[test]
    fn s290_classify_unknown_default_is_unknown() {
        // A future error shape the classifier doesn't recognise → Unknown.
        // The daemon's auto-retry cap then frees the operator from
        // forever-loops on unknown failures (CLAUDE.md rule 12 — fail loud).
        assert_eq!(
            classify_failure("render", "PDF font missing"),
            FailureKind::Unknown
        );
        assert_eq!(
            classify_failure("extract", "subprocess spawn failed: out of memory"),
            FailureKind::Unknown
        );
    }

    #[test]
    fn s290_classify_post_transient_keywords_dont_leak_to_other_stages() {
        // "timeout" appearing in a non-post stage shouldn't classify as
        // Transient — the post-stage transport rules are stage-scoped.
        // Default to Unknown so the operator's auto-retry cap kicks in.
        assert_eq!(
            classify_failure("render", "rendering timeout after 30s"),
            FailureKind::Unknown
        );
    }

    /// S290 / PR-271 — forensic c1cf32 end-to-end pin. Replays the exact
    /// shape of Ervin's 2026-06-08 evening test:
    ///
    ///   * Storefront submitted a STEP CAD (legacy data, before the
    ///     storefront content-sniff lands).
    ///   * Daemon enqueued `c1cf32ed-72b6-4708-8abb-6359d27f042b` at
    ///     `Fetched`, with `material_grade = "unknown"`.
    ///   * Extractor subprocess exited non-zero with the STEP-stub
    ///     `NotImplementedError` message.
    ///   * emit_failure ran: classify_failure returned `Permanent`;
    ///     `set_failed` wrote `state=Failed`, `failure_kind=permanent`.
    ///   * Daemon's next sweep MUST NOT re-enqueue the row.
    ///
    /// Pins three invariants in one test:
    /// 1. The classifier verdict for the literal STEP-stub error string.
    /// 2. The DB roundtrip: `set_failed` writes the verdict; `list_jobs`
    ///    reads it back.
    /// 3. `next_actionable_job` does NOT pick the row up (the daemon's
    ///    auto-retry skip — operator's Retry click is the only route).
    #[test]
    fn s290_c1cf32_step_stub_lands_permanent_and_is_not_reenqueued() {
        use crate::quote_pricing_jobs::{
            insert_fetched_job, list_jobs, next_actionable_job, set_failed, set_state, FailureKind,
            JobState,
        };

        let mut conn = duckdb::Connection::open_in_memory().expect("open mem");
        crate::quote_pricing_jobs::ensure_schema(&conn).expect("schema");

        let prod_qid = "c1cf32ed-72b6-4708-8abb-6359d27f042b";
        let tenant = "T";
        let now = OffsetDateTime::from_unix_timestamp(1_750_000_000).expect("ts");

        // Step 1 — enqueue the prod-shaped row at Fetched.
        let inserted = insert_fetched_job(
            &conn,
            prod_qid,
            tenant,
            "ervin@aben.ch",
            "ervin csenger",
            "Áben Consulting Kft.",
            "unknown",
            1,
            "submission.step",
            "/var/aberp/quote-artifacts/c1cf32ed-.../submission.step",
            now,
        )
        .expect("enqueue at Fetched");
        assert!(inserted);

        // Step 2 — daemon picks up the row → advance_extract sets
        // Extracting → extractor subprocess returns the STEP stub
        // `NotImplementedError`. The reason string is the exact `Display`
        // shape `ExtractError::NonZeroExit` produces.
        set_state(&conn, prod_qid, tenant, JobState::Extracting, now).expect("Extracting");
        let extract_err = "subprocess exited with code Some(2): \
            NotImplementedError: STEP extraction not yet implemented in v1; \
            please supply STL. Slated for S270 alongside the Rust subprocess wrapper.";

        // Step 3 — classifier verdict.
        let verdict = classify_failure("extract", extract_err);
        assert_eq!(
            verdict,
            FailureKind::Permanent,
            "STEP-stub must classify as Permanent"
        );

        // Step 4 — set_failed writes both Failed AND the verdict.
        let outcome = set_failed(
            &mut conn,
            prod_qid,
            tenant,
            "extract",
            extract_err,
            verdict,
            now,
        )
        .expect("set_failed");
        assert_eq!(
            outcome,
            crate::quote_pricing_jobs::TransitionOutcome::Applied
        );

        // Step 5 — DB roundtrip pins the persisted verdict.
        let rows = list_jobs(&conn, tenant).expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, JobState::Failed);
        assert_eq!(
            rows[0].failure_kind,
            Some(FailureKind::Permanent),
            "persisted verdict must match classifier"
        );

        // Step 6 — the load-bearing pin: daemon's next sweep does NOT
        // re-enqueue the row. The operator's audit-visible Retry click
        // is the only route back to Fetched.
        let next = next_actionable_job(&conn, tenant).expect("next");
        assert!(
            next.is_none(),
            "next_actionable_job must NOT pick up a Permanent Failed row \
             (got {next:?}). The auto-retry-5× symptom from 2026-06-08 \
             evening must not recur."
        );

        // Belt + braces — even after several cycles, the row stays Failed.
        for _ in 0..3 {
            assert!(next_actionable_job(&conn, tenant)
                .expect("next-loop")
                .is_none());
        }
        let rows = list_jobs(&conn, tenant).expect("list-after-loop");
        assert_eq!(rows[0].state, JobState::Failed);
        assert_eq!(rows[0].failure_kind, Some(FailureKind::Permanent));
    }

    // ── S347 / PR-39 (F1+F2) — priced-writeback transport verdict ─────
    //
    // The 2026-06-11 incident: the storefront priced-writeback POST got a
    // 200 `text/html` (CDN routing the API path to the SPA origin), the
    // client parsed it as JSON, logged `parse priced-writeback ok JSON:
    // <!doctype html>…`, and the failure landed in `? Ismeretlen / Unknown`.
    // These pins prove the Content-Type gate refuses HTML and every failure
    // mode resolves to a typed, non-`Unknown` verdict.

    /// PURE classifier — the F1 Content-Type gate. HTML 200 must be
    /// `RoutingMisconfigured`, never parsed-as-ok.
    #[test]
    fn s347_classify_html_200_is_routing_misconfigured_not_ok() {
        let o = classify_writeback_response(
            200,
            Some("text/html; charset=utf-8"),
            "<!doctype html><html>spa shell</html>",
        );
        assert!(
            matches!(o, WritebackOutcome::RoutingMisconfigured { .. }),
            "got {o:?}"
        );
        assert!(!o.is_success(), "HTML 200 must NEVER be treated as ok");
        assert_eq!(o.failure_kind(), FailureKind::Permanent);
    }

    #[test]
    fn s347_classify_content_type_case_insensitive_and_ignores_charset() {
        let o = classify_writeback_response(
            200,
            Some("APPLICATION/JSON; charset=UTF-8"),
            r#"{"status":"quoted"}"#,
        );
        assert!(matches!(o, WritebackOutcome::Success { .. }), "got {o:?}");
    }

    #[test]
    fn s347_classify_404_html_is_non_json_not_app_rejected() {
        let o = classify_writeback_response(404, Some("text/html"), "<html>not found</html>");
        assert!(
            matches!(o, WritebackOutcome::NonJsonResponse { .. }),
            "got {o:?}"
        );
    }

    #[test]
    fn s347_classify_missing_content_type_is_non_json() {
        // No explicit `application/json` → F1 refuses to trust the body.
        let o = classify_writeback_response(200, None, r#"{"status":"quoted"}"#);
        assert!(
            matches!(o, WritebackOutcome::NonJsonResponse { .. }),
            "got {o:?}"
        );
    }

    #[test]
    fn s347_classify_401_403_take_precedence_over_content_type() {
        let u = classify_writeback_response(401, Some("application/json"), r#"{"message":"no"}"#);
        assert!(
            matches!(u, WritebackOutcome::Unauthorized { .. }),
            "got {u:?}"
        );
        // Even with an HTML body (CloudFront 403 page), auth wins.
        let f = classify_writeback_response(403, Some("text/html"), "<html>forbidden</html>");
        assert!(matches!(f, WritebackOutcome::Forbidden { .. }), "got {f:?}");
    }

    #[test]
    fn s347_classify_json_status_families() {
        assert!(matches!(
            classify_writeback_response(400, Some("application/json"), r#"{"e":"bad"}"#),
            WritebackOutcome::AppRejected { .. }
        ));
        assert!(matches!(
            classify_writeback_response(500, Some("application/json"), r#"{"error":"db down"}"#),
            WritebackOutcome::AppErrored { .. }
        ));
        // 200 JSON without the required `status` field → malformed.
        assert!(matches!(
            classify_writeback_response(200, Some("application/json"), r#"{"unexpected":"shape"}"#),
            WritebackOutcome::MalformedAppResponse { .. }
        ));
        // 200 JSON with `status` → success; `idempotent` flag honoured.
        assert!(matches!(
            classify_writeback_response(200, Some("application/json"), r#"{"status":"quoted"}"#),
            WritebackOutcome::Success { idempotent: false }
        ));
        assert!(matches!(
            classify_writeback_response(
                200,
                Some("application/json"),
                r#"{"status":"quoted","idempotent":true}"#
            ),
            WritebackOutcome::Success { idempotent: true }
        ));
    }

    /// F2 — the core invariant: EVERY failure variant resolves to a
    /// non-`Unknown` FailureKind, and the by-reason classifier
    /// (`classify_failure`) agrees with the by-variant one. This is what
    /// kills the `? Ismeretlen / Unknown` catch-all for the post stage.
    #[test]
    fn s347_every_writeback_failure_maps_to_non_unknown_kind() {
        let variants = [
            WritebackOutcome::RoutingMisconfigured {
                http_status: 200,
                content_type: "text/html".to_string(),
                body_excerpt: "<!doctype html>".to_string(),
            },
            WritebackOutcome::Unauthorized {
                http_status: 401,
                body_excerpt: "no".to_string(),
            },
            WritebackOutcome::Forbidden {
                http_status: 403,
                body_excerpt: "no".to_string(),
            },
            WritebackOutcome::NonJsonResponse {
                http_status: 404,
                content_type: "text/plain".to_string(),
                body_excerpt: "x".to_string(),
            },
            WritebackOutcome::MalformedAppResponse {
                http_status: 200,
                body_excerpt: "{}".to_string(),
            },
            WritebackOutcome::AppRejected {
                http_status: 422,
                body_excerpt: "x".to_string(),
            },
            WritebackOutcome::AppErrored {
                http_status: 500,
                body_excerpt: "x".to_string(),
            },
            WritebackOutcome::Timeout,
            WritebackOutcome::TransportError {
                kind: "connection refused".to_string(),
            },
        ];
        for v in &variants {
            let reason = v.failure_reason();
            let kind = classify_failure("post", &reason);
            assert_ne!(
                kind,
                FailureKind::Unknown,
                "variant {} fell into Unknown via reason {reason:?}",
                v.tag()
            );
            assert_eq!(
                kind,
                v.failure_kind(),
                "by-reason classify must match by-variant for {}",
                v.tag()
            );
        }
    }

    /// S368 / Scope B — the RoutingMisconfigured operator hint must enumerate
    /// BOTH known causes (CDN route missing AND the 404-masked-as-200 case), so
    /// an operator chasing a 404 isn't sent only to the CloudFront routing
    /// panel. The bilingual chip labels carry the "or 404 / masked" wording.
    #[test]
    fn s368_routing_hint_names_both_causes() {
        let o = WritebackOutcome::RoutingMisconfigured {
            http_status: 200,
            content_type: "text/html".to_string(),
            body_excerpt: "<!doctype html>".to_string(),
        };
        let hint = o.operator_hint();
        assert!(hint.contains("CloudFront route missing"), "{hint}");
        assert!(hint.contains("404"), "{hint}");
        assert!(
            hint.contains("ABERP_SITE_QUOTE_DIR"),
            "hint must point at the storefront quote-dir cause: {hint}"
        );
        assert!(hint.contains("index.html"), "{hint}");
        assert!(o.label_hu().contains("404"), "{}", o.label_hu());
        assert!(o.label_en().contains("404"), "{}", o.label_en());
        assert!(
            o.label_en().to_lowercase().contains("masked"),
            "{}",
            o.label_en()
        );
    }

    #[test]
    fn s347_retryable_split_matches_transient_class() {
        assert!(WritebackOutcome::AppErrored {
            http_status: 503,
            body_excerpt: "x".to_string()
        }
        .retryable());
        assert!(WritebackOutcome::Timeout.retryable());
        assert!(WritebackOutcome::TransportError {
            kind: "dns".to_string()
        }
        .retryable());
        assert!(!WritebackOutcome::RoutingMisconfigured {
            http_status: 200,
            content_type: "text/html".to_string(),
            body_excerpt: "x".to_string()
        }
        .retryable());
        assert!(!WritebackOutcome::Unauthorized {
            http_status: 401,
            body_excerpt: "x".to_string()
        }
        .retryable());
    }

    #[test]
    fn s347_failure_reason_carries_tag_status_and_excerpt() {
        let o = WritebackOutcome::RoutingMisconfigured {
            http_status: 200,
            content_type: "text/html".to_string(),
            body_excerpt: "<!doctype html>".to_string(),
        };
        let r = o.failure_reason();
        assert!(r.starts_with("writeback:routing_misconfigured "), "{r}");
        assert!(r.contains("http_status=200"), "{r}");
        assert!(r.contains("content_type=text/html"), "{r}");
        assert!(r.contains("body=<!doctype html>"), "{r}");
        // Never the word "ok" for an HTML response (the incident log line).
        assert!(!r.to_lowercase().contains(" ok "), "{r}");
    }

    // ── e2e: drive a real reqwest client at a hand-rolled TCP mock ─────

    fn s347_http_canned(status_line: &str, content_type: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status_line}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// `Some(raw)` → write the canned response after draining the request;
    /// `None` → accept the socket and never reply (drives the client into
    /// its short timeout). The accept loop lives until the test exits.
    async fn s347_spawn_writeback_mock(response: Option<String>) -> std::net::SocketAddr {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(x) => x,
                    Err(_) => break,
                };
                let response = response.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 16 * 1024];
                    let _ = sock.read(&mut buf).await;
                    match response {
                        Some(r) => {
                            let _ = sock.write_all(r.as_bytes()).await;
                            let _ = sock.shutdown().await;
                        }
                        None => {
                            // Hold the connection past the client timeout.
                            tokio::time::sleep(Duration::from_secs(5)).await;
                        }
                    }
                });
            }
        });
        addr
    }

    fn s347_writeback_service(addr: &std::net::SocketAddr) -> PricingPipelineService {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(400))
            .build()
            .expect("client");
        PricingPipelineService {
            config: PricingPipelineConfig {
                base_url: format!("http://{addr}"),
                bearer_token: Zeroizing::new("t0k3n".to_string()),
                poll_interval: Duration::from_secs(60),
                artifact_dir: std::env::temp_dir(),
                python_bin: PathBuf::from("/usr/bin/python3"),
                default_tolerance: ToleranceRange::Standard,
            },
            deps: PricingPipelineDeps {
                db_path: std::env::temp_dir().join("aberp-s347-unused.duckdb"),
                tenant: TenantId::new("T").expect("tid"),
                binary_hash: BinaryHash::from_bytes([0u8; 32]),
                operator_login: "ervin".to_string(),
            },
            client,
            cad_blob: CadBlobCtx::with_test_key(),
        }
    }

    async fn s347_run_writeback(addr: &std::net::SocketAddr) -> WritebackOutcome {
        s347_writeback_service(addr)
            .post_priced_writeback(
                "00000000-0000-0000-0000-000000000001",
                "hash-abc",
                "2026-07-01",
                "{}",
                b"%PDF-1.4 fake",
            )
            .await
            .expect("post_priced_writeback must not internal-error")
    }

    #[tokio::test]
    async fn s347_e2e_html_200_is_routing_misconfigured_not_ok() {
        let addr = s347_spawn_writeback_mock(Some(s347_http_canned(
            "200 OK",
            "text/html; charset=utf-8",
            "<!doctype html><html>spa shell</html>",
        )))
        .await;
        let o = s347_run_writeback(&addr).await;
        assert!(
            matches!(o, WritebackOutcome::RoutingMisconfigured { .. }),
            "HTML 200 must classify as routing-misconfig, got {o:?}"
        );
        assert!(!o.is_success());
    }

    #[tokio::test]
    async fn s347_e2e_401_json_is_unauthorized() {
        let addr = s347_spawn_writeback_mock(Some(s347_http_canned(
            "401 Unauthorized",
            "application/json",
            r#"{"message":"Unauthorized"}"#,
        )))
        .await;
        let o = s347_run_writeback(&addr).await;
        assert!(
            matches!(o, WritebackOutcome::Unauthorized { .. }),
            "got {o:?}"
        );
    }

    #[tokio::test]
    async fn s347_e2e_500_json_is_app_errored_and_retryable() {
        let addr = s347_spawn_writeback_mock(Some(s347_http_canned(
            "500 Internal Server Error",
            "application/json",
            r#"{"error":"db down"}"#,
        )))
        .await;
        let o = s347_run_writeback(&addr).await;
        assert!(
            matches!(o, WritebackOutcome::AppErrored { .. }),
            "got {o:?}"
        );
        assert!(o.retryable());
        assert_eq!(o.failure_kind(), FailureKind::Transient);
    }

    #[tokio::test]
    async fn s347_e2e_200_json_missing_status_is_malformed() {
        let addr = s347_spawn_writeback_mock(Some(s347_http_canned(
            "200 OK",
            "application/json",
            r#"{"unexpected":"shape"}"#,
        )))
        .await;
        let o = s347_run_writeback(&addr).await;
        assert!(
            matches!(o, WritebackOutcome::MalformedAppResponse { .. }),
            "got {o:?}"
        );
    }

    #[tokio::test]
    async fn s347_e2e_timeout_is_timeout() {
        let addr = s347_spawn_writeback_mock(None).await;
        let o = s347_run_writeback(&addr).await;
        assert!(matches!(o, WritebackOutcome::Timeout), "got {o:?}");
        assert!(o.retryable());
    }

    #[tokio::test]
    async fn s347_e2e_200_valid_is_success() {
        let addr = s347_spawn_writeback_mock(Some(s347_http_canned(
            "200 OK",
            "application/json",
            r#"{"status":"quoted"}"#,
        )))
        .await;
        let o = s347_run_writeback(&addr).await;
        assert!(
            matches!(o, WritebackOutcome::Success { idempotent: false }),
            "got {o:?}"
        );
        assert!(o.is_success());
    }

    // ── S351 — trailing-slash-safe writeback URL builder ──────────────────

    #[test]
    fn s351_resolved_writeback_url_strips_single_trailing_slash() {
        assert_eq!(
            resolved_writeback_url("https://abenerp.com/", "X", "priced"),
            "https://abenerp.com/api/quotes/X/priced"
        );
    }

    #[test]
    fn s351_resolved_writeback_url_strips_multiple_trailing_slashes() {
        assert_eq!(
            resolved_writeback_url("https://abenerp.com///", "X", "priced"),
            "https://abenerp.com/api/quotes/X/priced"
        );
    }

    #[test]
    fn s351_resolved_writeback_url_handles_no_slash() {
        assert_eq!(
            resolved_writeback_url("https://abenerp.com", "X", "priced"),
            "https://abenerp.com/api/quotes/X/priced"
        );
    }

    #[test]
    fn s351_resolved_writeback_url_handles_paths() {
        // Subpath bases keep their interior slash — only the trailing one is
        // trimmed, so the `/api/*` segment still resolves under the subpath.
        assert_eq!(
            resolved_writeback_url("https://abenerp.com/sub/", "X", "priced"),
            "https://abenerp.com/sub/api/quotes/X/priced"
        );
    }

    // ── S377 — Origin header derivation (SvelteKit CSRF gate) ─────────────

    #[test]
    fn s377_origin_from_base_url_plain() {
        assert_eq!(
            origin_from_base_url("https://abenerp.com"),
            "https://abenerp.com"
        );
    }

    #[test]
    fn s377_origin_from_base_url_strips_trailing_slash() {
        // The S351 incident input — a trailing slash must NOT leak into Origin
        // (an Origin with a path/slash itself fails the CSRF gate).
        assert_eq!(
            origin_from_base_url("https://abenerp.com/"),
            "https://abenerp.com"
        );
    }

    #[test]
    fn s377_origin_from_base_url_drops_path_and_query() {
        assert_eq!(
            origin_from_base_url("https://abenerp.com/sub/path?x=1"),
            "https://abenerp.com"
        );
    }

    #[test]
    fn s377_origin_from_base_url_keeps_port() {
        assert_eq!(
            origin_from_base_url("http://127.0.0.1:54321/"),
            "http://127.0.0.1:54321"
        );
        assert_eq!(
            origin_from_base_url("http://127.0.0.1:54321"),
            "http://127.0.0.1:54321"
        );
    }

    /// Path-capturing mock: replies via a oneshot with the request-line path
    /// the client actually hit, then a clean 200 JSON so the writeback
    /// resolves to Success. This is the production-incident assertion — a
    /// trailing slash in the stored base_url must NOT yield a `//api/…` path.
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
                let _ = sock
                    .write_all(
                        s347_http_canned("200 OK", "application/json", r#"{"status":"quoted"}"#)
                            .as_bytes(),
                    )
                    .await;
                let _ = sock.shutdown().await;
            }
        });
        (addr, rx)
    }

    fn s351_service_with_trailing_slash(addr: &std::net::SocketAddr) -> PricingPipelineService {
        let mut svc = s347_writeback_service(addr);
        // Operator typed a trailing slash — the incident's root cause.
        svc.config.base_url = format!("http://{addr}/");
        svc
    }

    #[tokio::test]
    async fn s351_priced_pipeline_resolves_url_correctly() {
        let (addr, rx) = s351_spawn_path_capturing_mock().await;
        let svc = s351_service_with_trailing_slash(&addr);
        let o = svc
            .post_priced_writeback(
                "00000000-0000-0000-0000-000000000001",
                "hash-abc",
                "2026-07-01",
                "{}",
                b"%PDF-1.4 fake",
            )
            .await
            .expect("post_priced_writeback must not internal-error");
        assert!(o.is_success(), "got {o:?}");
        let path = rx.await.expect("mock captured a request path");
        assert_eq!(
            path,
            "/api/quotes/00000000-0000-0000-0000-000000000001/priced"
        );
    }

    #[tokio::test]
    async fn s351_status_writeback_resolves_url_correctly() {
        let (addr, rx) = s351_spawn_path_capturing_mock().await;
        let svc = s351_service_with_trailing_slash(&addr);
        svc.writeback_quoting_status("00000000-0000-0000-0000-000000000002", "note")
            .await
            .expect("status writeback must succeed");
        let path = rx.await.expect("mock captured a request path");
        assert_eq!(
            path,
            "/api/quotes/00000000-0000-0000-0000-000000000002/status"
        );
    }

    // ── S377 — Origin header on the priced writeback ──────────────────────

    /// Request-capturing mock: sends the FULL raw request (headers included)
    /// back over the oneshot, then a clean 200 JSON so the writeback resolves.
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
                let _ = sock
                    .write_all(
                        s347_http_canned("200 OK", "application/json", r#"{"status":"quoted"}"#)
                            .as_bytes(),
                    )
                    .await;
                let _ = sock.shutdown().await;
            }
        });
        (addr, rx)
    }

    /// Extract the `Origin` header value from a captured raw request.
    fn s377_origin_header(req: &str) -> Option<String> {
        req.lines()
            .find(|l| l.to_ascii_lowercase().starts_with("origin:"))
            .and_then(|l| l.split_once(':'))
            .map(|(_, v)| v.trim().to_string())
    }

    #[tokio::test]
    async fn s377_priced_writeback_sends_origin_header() {
        let (addr, rx) = s377_spawn_request_capturing_mock().await;
        // No trailing slash — Origin must equal the base scheme://authority.
        let svc = s347_writeback_service(&addr);
        let o = svc
            .post_priced_writeback(
                "00000000-0000-0000-0000-000000000010",
                "hash-abc",
                "2026-07-01",
                "{}",
                b"%PDF-1.4 fake",
            )
            .await
            .expect("post_priced_writeback must not internal-error");
        assert!(o.is_success(), "got {o:?}");
        let req = rx.await.expect("mock captured a request");
        assert_eq!(
            s377_origin_header(&req).as_deref(),
            Some(format!("http://{addr}").as_str()),
            "priced writeback must carry an Origin matching the storefront"
        );
    }

    #[tokio::test]
    async fn s377_trailing_slash_base_url_origin_has_no_slash() {
        let (addr, rx) = s377_spawn_request_capturing_mock().await;
        // Operator typed a trailing slash — Origin must still be slash-free.
        let svc = s351_service_with_trailing_slash(&addr);
        let o = svc
            .post_priced_writeback(
                "00000000-0000-0000-0000-000000000011",
                "hash-abc",
                "2026-07-01",
                "{}",
                b"%PDF-1.4 fake",
            )
            .await
            .expect("post_priced_writeback must not internal-error");
        assert!(o.is_success(), "got {o:?}");
        let req = rx.await.expect("mock captured a request");
        assert_eq!(
            s377_origin_header(&req).as_deref(),
            Some(format!("http://{addr}").as_str()),
            "trailing slash in base_url must NOT leak into Origin"
        );
    }

    #[test]
    fn s377_forbidden_hint_names_csrf() {
        // The reworded 403 hint must name the CSRF/Origin cause, not just
        // "origin secret or Bearer" (Bearer failures are 401, not 403).
        let o = WritebackOutcome::Forbidden {
            http_status: 403,
            body_excerpt: "forbidden".to_string(),
        };
        let hint = o.operator_hint();
        assert!(
            hint.contains("CSRF") && hint.contains("Origin"),
            "403 hint must name SvelteKit CSRF + Origin; got {hint:?}"
        );
    }

    // ── S348 / PR-39 (F1) — list-poll Content-Type gate ───────────────────

    #[test]
    fn s348_classify_poll_html_200_is_routing_misconfigured_not_parsed() {
        // THE incident shape on the poll site: a 200 text/html body must be
        // refused at the gate, never fed to the StorefrontListResponse parser.
        let r = classify_poll_response(
            200,
            Some("text/html; charset=utf-8"),
            "<!doctype html><html>spa shell</html>",
        );
        match r {
            Err(WritebackOutcome::RoutingMisconfigured { content_type, .. }) => {
                assert_eq!(content_type, "text/html");
            }
            other => panic!("expected RoutingMisconfigured, got {other:?}"),
        }
    }

    #[test]
    fn s348_classify_poll_401_403_take_precedence() {
        assert!(matches!(
            classify_poll_response(401, Some("text/html"), "<html>no</html>"),
            Err(WritebackOutcome::Unauthorized { .. })
        ));
        assert!(matches!(
            classify_poll_response(403, Some("application/json"), r#"{"e":"no"}"#),
            Err(WritebackOutcome::Forbidden { .. })
        ));
    }

    #[test]
    fn s348_classify_poll_500_json_is_app_errored_and_retryable() {
        match classify_poll_response(500, Some("application/json"), r#"{"error":"db"}"#) {
            Err(o @ WritebackOutcome::AppErrored { .. }) => {
                assert!(o.retryable());
                assert_eq!(o.failure_kind(), FailureKind::Transient);
            }
            other => panic!("expected AppErrored, got {other:?}"),
        }
    }

    #[test]
    fn s348_classify_poll_missing_content_type_is_non_json() {
        // A valid-looking JSON body with NO Content-Type is still refused —
        // the gate requires an explicit application/json, never sniffs.
        assert!(matches!(
            classify_poll_response(200, None, r#"{"quotes":[]}"#),
            Err(WritebackOutcome::NonJsonResponse { .. })
        ));
    }

    #[test]
    fn s348_classify_poll_200_json_wrong_shape_is_malformed() {
        // 200 + application/json but not the {quotes:[...]} envelope.
        assert!(matches!(
            classify_poll_response(200, Some("application/json"), r#"{"unexpected":"shape"}"#),
            Err(WritebackOutcome::MalformedAppResponse { .. })
        ));
    }

    #[test]
    fn s348_classify_poll_200_valid_envelope_is_ok() {
        let r = classify_poll_response(
            200,
            Some("application/json"),
            r#"{"quotes":[{"id":"q1","contact":{"email":"a@b.c","name":"A"},"request":{"material_preference":"6061-T6","quantity":2},"files":[]}]}"#,
        );
        let list = r.expect("valid envelope must parse");
        assert_eq!(list.quotes.len(), 1);
    }

    /// S401 — the storefront listing carries `contact.company`; the
    /// daemon's `StorefrontContact` must deserialize it (it was dropped
    /// before S401). Fails if the `company` field is removed from the
    /// struct.
    #[test]
    fn s401_storefront_contact_deserializes_company() {
        let r = classify_poll_response(
            200,
            Some("application/json"),
            r#"{"quotes":[{"id":"q1","contact":{"email":"a@b.c","name":"A","company":"Acme Manufacturing Kft."},"request":{"material_preference":"6061-T6","quantity":2},"files":[]}]}"#,
        );
        let list = r.expect("valid envelope must parse");
        assert_eq!(list.quotes[0].contact.company, "Acme Manufacturing Kft.");
    }

    /// S401 — a listing whose contact omits `company` (older storefront
    /// build) deserializes to "" via `#[serde(default)]` rather than
    /// failing the whole parse — the daemon stays fail-soft.
    #[test]
    fn s401_storefront_contact_missing_company_defaults_empty() {
        let r = classify_poll_response(
            200,
            Some("application/json"),
            r#"{"quotes":[{"id":"q1","contact":{"email":"a@b.c","name":"A"},"request":{"material_preference":"6061-T6","quantity":2},"files":[]}]}"#,
        );
        let list = r.expect("valid envelope must parse");
        assert_eq!(list.quotes[0].contact.company, "");
    }

    fn s348_poll_service(
        addr: &std::net::SocketAddr,
        db_path: std::path::PathBuf,
    ) -> PricingPipelineService {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(400))
            .build()
            .expect("client");
        PricingPipelineService {
            config: PricingPipelineConfig {
                base_url: format!("http://{addr}"),
                bearer_token: Zeroizing::new("t0k3n".to_string()),
                poll_interval: Duration::from_secs(60),
                artifact_dir: std::env::temp_dir(),
                python_bin: PathBuf::from("/usr/bin/python3"),
                default_tolerance: ToleranceRange::Standard,
            },
            deps: PricingPipelineDeps {
                db_path,
                tenant: TenantId::new("T").expect("tid"),
                binary_hash: BinaryHash::from_bytes([0u8; 32]),
                operator_login: "ervin".to_string(),
            },
            client,
            cad_blob: CadBlobCtx::with_test_key(),
        }
    }

    /// Drive `list_and_enqueue_received` against a canned response and return
    /// the Err reason (the failure cases never reach enqueue, so no CAD
    /// download / job insert happens). `db_path` is real so the
    /// `quote.poll_outcome` audit emit actually writes.
    async fn s348_run_poll(addr: &std::net::SocketAddr, db_path: std::path::PathBuf) -> String {
        let svc = s348_poll_service(addr, db_path);
        let err = svc
            .list_and_enqueue_received()
            .await
            .expect_err("poll must fail on a non-JSON / error response");
        format!("{err:#}")
    }

    fn s348_temp_db() -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("aberp-s348-poll-{}.duckdb", Ulid::new()));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[tokio::test]
    async fn s348_e2e_poll_html_200_is_routing_misconfig_and_audited() {
        use aberp_audit_ledger::recent_entries;
        let addr = s347_spawn_writeback_mock(Some(s347_http_canned(
            "200 OK",
            "text/html; charset=utf-8",
            "<!doctype html><html>spa shell</html>",
        )))
        .await;
        let db = s348_temp_db();
        let reason = s348_run_poll(&addr, db.clone()).await;
        assert!(
            reason.contains("routing_misconfigured"),
            "reason must name the routing misconfig, got: {reason}"
        );
        // The failure is durably audited as exactly ONE quote.poll_outcome row.
        let conn = duckdb::Connection::open(&db).expect("reopen");
        let entries = recent_entries(&conn, 10).expect("recent");
        let poll_rows: Vec<_> = entries
            .iter()
            .filter(|e| e.kind.as_str() == "quote.poll_outcome")
            .collect();
        assert_eq!(poll_rows.len(), 1, "exactly one poll-outcome row");
        let payload: serde_json::Value =
            serde_json::from_slice(&poll_rows[0].payload).expect("decode");
        assert_eq!(payload["outcome"], "routing_misconfigured");
        assert_eq!(payload["http_status"], 200);
        assert_eq!(payload["content_type"], "text/html");
        assert_eq!(payload["retryable"], false);
        drop(conn);
        let _ = std::fs::remove_file(&db);
    }

    #[tokio::test]
    async fn s348_e2e_poll_401_is_unauthorized() {
        let addr = s347_spawn_writeback_mock(Some(s347_http_canned(
            "401 Unauthorized",
            "application/json",
            r#"{"message":"Unauthorized"}"#,
        )))
        .await;
        let reason = s348_run_poll(&addr, s348_temp_db()).await;
        assert!(reason.contains("unauthorized"), "got: {reason}");
    }

    #[tokio::test]
    async fn s348_e2e_poll_500_is_app_errored() {
        let addr = s347_spawn_writeback_mock(Some(s347_http_canned(
            "500 Internal Server Error",
            "application/json",
            r#"{"error":"db down"}"#,
        )))
        .await;
        let reason = s348_run_poll(&addr, s348_temp_db()).await;
        assert!(reason.contains("app_errored"), "got: {reason}");
    }

    #[tokio::test]
    async fn s348_e2e_poll_200_malformed_json_is_malformed() {
        let addr = s347_spawn_writeback_mock(Some(s347_http_canned(
            "200 OK",
            "application/json",
            r#"{"unexpected":"shape"}"#,
        )))
        .await;
        let reason = s348_run_poll(&addr, s348_temp_db()).await;
        assert!(reason.contains("malformed_app_response"), "got: {reason}");
    }

    #[tokio::test]
    async fn s348_e2e_poll_timeout_is_timeout() {
        let addr = s347_spawn_writeback_mock(None).await;
        let reason = s348_run_poll(&addr, s348_temp_db()).await;
        assert!(reason.contains("timeout"), "got: {reason}");
    }

    // ── S379 / PR-379 — listing-level no-CAD = permanent enqueue failure ──

    /// Build a service whose `base_url` is never contacted — the no-CAD
    /// branch returns before any HTTP — pointed at a real temp DB so the
    /// Failed row + audit pair actually persist.
    fn s379_no_cad_service(db_path: std::path::PathBuf) -> PricingPipelineService {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(400))
            .build()
            .expect("client");
        PricingPipelineService {
            config: PricingPipelineConfig {
                base_url: "http://127.0.0.1:1".to_string(),
                bearer_token: Zeroizing::new("t0k3n".to_string()),
                poll_interval: Duration::from_secs(60),
                artifact_dir: std::env::temp_dir(),
                python_bin: PathBuf::from("/usr/bin/python3"),
                default_tolerance: ToleranceRange::Standard,
            },
            deps: PricingPipelineDeps {
                db_path,
                tenant: TenantId::new("T").expect("tid"),
                binary_hash: BinaryHash::from_bytes([0u8; 32]),
                operator_login: "ervin".to_string(),
            },
            client,
            cad_blob: CadBlobCtx::with_test_key(),
        }
    }

    fn s379_temp_db() -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("aberp-s379-nocad-{}.duckdb", Ulid::new()));
        let _ = std::fs::remove_file(&p);
        p
    }

    /// A storefront listing entry that carries no CAD file (the pre-S368
    /// wiped-files shape that drove the S376 phantom-retry loop).
    fn s379_quote_no_cad(id: &str) -> StorefrontQuote {
        StorefrontQuote {
            id: id.to_string(),
            contact: StorefrontContact {
                name: "Phantom Customer".to_string(),
                email: "phantom@example.com".to_string(),
                company: "Phantom Holdings Kft.".to_string(),
            },
            request: StorefrontRequest {
                material_preference: "6061-T6".to_string(),
                quantity: Some(2),
            },
            files: vec![],
        }
    }

    /// S379 regression: a no-CAD quote enqueues exactly ONE terminal
    /// `Failed` row + ONE `QuotePricingFailed`/`FailureClassified` audit
    /// pair on the first cycle, and the SECOND cycle is a complete no-op
    /// (no new row, no new audit, no re-warn) — closing the S376 loop where
    /// the same quote_id re-fired every 60s forever.
    #[tokio::test]
    async fn s379_no_cad_enqueues_one_failed_row_then_skips() {
        use aberp_audit_ledger::recent_entries;
        let db = s379_temp_db();
        let svc = s379_no_cad_service(db.clone());

        // ── Cycle 1 ──
        let first = svc
            .enqueue_one(s379_quote_no_cad("phantom-1"))
            .await
            .expect("no-CAD enqueue must not error");
        assert!(!first, "a no-CAD quote is not an enqueued pricing job");

        let conn = duckdb::Connection::open(&db).expect("reopen");
        let rows = jobs::list_jobs(&conn, "T").expect("list");
        assert_eq!(rows.len(), 1, "exactly one Failed row after cycle 1");
        let row = &rows[0];
        assert_eq!(row.quote_id, "phantom-1");
        assert_eq!(row.state, JobState::Failed);
        assert_eq!(row.error_stage.as_deref(), Some("enqueue"));
        assert_eq!(row.error_reason.as_deref(), Some("no CAD file on listing"));
        assert_eq!(row.failure_kind, Some(FailureKind::Permanent));

        let entries = recent_entries(&conn, 20).expect("recent");
        assert_eq!(
            entries
                .iter()
                .filter(|e| e.kind.as_str() == "quote.pricing_failed")
                .count(),
            1,
            "exactly one QuotePricingFailed"
        );
        let classified: Vec<_> = entries
            .iter()
            .filter(|e| e.kind.as_str() == "quote.pricing_failure_classified")
            .collect();
        assert_eq!(classified.len(), 1, "exactly one FailureClassified");
        let payload: serde_json::Value =
            serde_json::from_slice(&classified[0].payload).expect("decode");
        assert_eq!(payload["failure_kind"], "permanent");
        assert_eq!(payload["last_error"], "no CAD file on listing");
        drop(conn);

        // ── Cycle 2 — same quote_id, must be a total no-op ──
        let second = svc
            .enqueue_one(s379_quote_no_cad("phantom-1"))
            .await
            .expect("second no-CAD enqueue must not error");
        assert!(!second);

        let conn = duckdb::Connection::open(&db).expect("reopen2");
        assert_eq!(
            jobs::list_jobs(&conn, "T").expect("list2").len(),
            1,
            "still exactly one row — cycle 2 did not re-insert"
        );
        let total_audit = recent_entries(&conn, 50).expect("recent2").len();
        drop(conn);
        assert_eq!(
            total_audit, 2,
            "cycle 2 must not append another audit pair (only cycle 1's failed+classified)"
        );

        let _ = std::fs::remove_file(&db);
    }

    // ── S430 / ADR-0083 — CAD-blob encryption-at-rest + read-audit ──
    //
    // These drive the REAL pipeline write (`enqueue_one`) + read
    // (`advance_one_step` → `advance_extract`) paths. They are
    // python-free: decryption is byte-agnostic (we decrypt our own
    // ciphertext), and a garbage "CAD" fails at the `extract` stage —
    // never the `decrypt` stage — so the assertions hold whether or not
    // a venv python is present.

    /// Loop mock that serves the CAD file download (200 + the supplied
    /// bytes for any `/files/` GET) and 200 for the status writeback POST.
    async fn s430_spawn_cad_mock(cad_bytes: Vec<u8>) -> std::net::SocketAddr {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(s) => s,
                    Err(_) => break,
                };
                let cad = cad_bytes.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 16 * 1024];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let resp: Vec<u8> = if req.contains("/files/") {
                        let mut r = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\
                             Content-Length: {}\r\n\r\n",
                            cad.len()
                        )
                        .into_bytes();
                        r.extend_from_slice(&cad);
                        r
                    } else {
                        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                          Content-Length: 2\r\n\r\n{}"
                            .to_vec()
                    };
                    let _ = sock.write_all(&resp).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        addr
    }

    fn s430_service(
        addr: &std::net::SocketAddr,
        db_path: std::path::PathBuf,
        artifact_dir: std::path::PathBuf,
    ) -> PricingPipelineService {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(800))
            .build()
            .expect("client");
        PricingPipelineService {
            config: PricingPipelineConfig {
                base_url: format!("http://{addr}"),
                bearer_token: Zeroizing::new("t0k3n".to_string()),
                poll_interval: Duration::from_secs(60),
                artifact_dir,
                python_bin: PathBuf::from("/usr/bin/python3"),
                default_tolerance: ToleranceRange::Standard,
            },
            deps: PricingPipelineDeps {
                db_path,
                tenant: TenantId::new("T").expect("tid"),
                binary_hash: BinaryHash::from_bytes([0u8; 32]),
                operator_login: "ervin".to_string(),
            },
            client,
            cad_blob: CadBlobCtx::with_test_key(),
        }
    }

    fn s430_quote(qid: &str, filename: &str) -> StorefrontQuote {
        StorefrontQuote {
            id: qid.to_string(),
            contact: StorefrontContact {
                name: "Buyer".to_string(),
                email: "buyer@example.com".to_string(),
                company: "ACME".to_string(),
            },
            request: StorefrontRequest {
                material_preference: "AL_6061_T6".to_string(),
                quantity: Some(3),
            },
            files: vec![StorefrontFile {
                filename: filename.to_string(),
            }],
        }
    }

    fn s430_temp(prefix: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("aberp-s430-{prefix}-{}", Ulid::new()));
        p
    }

    fn s430_read_state(db: &std::path::Path, qid: &str) -> (String, Option<String>) {
        let conn = duckdb::Connection::open(db).expect("reopen db");
        conn.query_row(
            "SELECT state, error_stage FROM quote_pricing_jobs WHERE quote_id = ?",
            [qid],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
        )
        .expect("job row")
    }

    fn s430_count_kind(db: &std::path::Path, kind: &str) -> usize {
        let conn = duckdb::Connection::open(db).expect("reopen db");
        aberp_audit_ledger::recent_entries(&conn, 200)
            .expect("recent")
            .iter()
            .filter(|e| e.kind.as_str() == kind)
            .count()
    }

    /// Brief §2 — a NEW write lands ENCRYPTED at rest: the on-disk blob
    /// carries the magic header and decrypts back to the exact bytes the
    /// storefront served (length + content preserved).
    #[tokio::test]
    async fn s430_enqueue_writes_encrypted_blob_at_rest() {
        let plaintext = b"solid part\n  the customer's proprietary geometry\nendsolid".to_vec();
        let addr = s430_spawn_cad_mock(plaintext.clone()).await;
        let db = s430_temp("enc.duckdb");
        let artifacts = s430_temp("art");
        let qid = "00000000-0000-0000-0000-0000000000aa";
        let svc = s430_service(&addr, db.clone(), artifacts.clone());

        let inserted = svc.enqueue_one(s430_quote(qid, "part.stl")).await.unwrap();
        assert!(inserted, "first enqueue inserts a Fetched row");

        let on_disk = std::fs::read(artifacts.join(qid).join("part.stl")).expect("blob on disk");
        // Encrypted at rest: magic header present, NOT the plaintext.
        assert_eq!(
            &on_disk[..crate::cad_blob::MAGIC.len()],
            crate::cad_blob::MAGIC,
            "on-disk CAD must start with the encryption magic header"
        );
        assert_ne!(
            on_disk, plaintext,
            "on-disk bytes must not be the plaintext"
        );
        // Decrypts back to the original (the [7u8;32] test key).
        let key = crate::cad_blob::CadBlobKey::from_bytes([7u8; 32]);
        let opened = key.open(&on_disk).expect("decrypt");
        assert!(!opened.was_legacy_plaintext);
        assert_eq!(opened.plaintext, plaintext, "decrypted bytes round-trip");

        let _ = std::fs::remove_dir_all(&artifacts);
        let _ = std::fs::remove_file(&db);
    }

    /// Brief §5 — tamper-and-detect: corrupting the stored ciphertext
    /// drives the extract step to FAIL with `error_stage = "decrypt"`,
    /// which the operator SPA renders as a red error chip (the panel
    /// already renders Failed rows' stage + reason — no new SPA surface).
    #[tokio::test]
    async fn s430_tampered_blob_fails_extract_with_decrypt_stage() {
        let addr = s430_spawn_cad_mock(b"solid x endsolid".to_vec()).await;
        let db = s430_temp("tamper.duckdb");
        let artifacts = s430_temp("art");
        let qid = "00000000-0000-0000-0000-0000000000bb";
        let svc = s430_service(&addr, db.clone(), artifacts.clone());
        svc.enqueue_one(s430_quote(qid, "part.stl")).await.unwrap();

        // Flip a byte in the stored ciphertext body (past magic + nonce).
        let blob_path = artifacts.join(qid).join("part.stl");
        let mut bytes = std::fs::read(&blob_path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        std::fs::write(&blob_path, &bytes).unwrap();

        let row = svc
            .next_actionable_blocking()
            .await
            .unwrap()
            .expect("a Fetched row is actionable");
        let outcome = svc.advance_one_step(row).await.unwrap();
        assert!(
            matches!(outcome, StepOutcome::Failed),
            "tampered blob must Fail the step, got {outcome:?}"
        );
        let (state, stage) = s430_read_state(&db, qid);
        assert_eq!(state, "failed");
        assert_eq!(
            stage.as_deref(),
            Some("decrypt"),
            "tamper must be attributed to the decrypt stage"
        );

        let _ = std::fs::remove_dir_all(&artifacts);
        let _ = std::fs::remove_file(&db);
    }

    /// Brief §3/§5 (customer-journey) — a VALID encrypted blob decrypts
    /// successfully (the read gets PAST the decrypt stage) and the read
    /// is audited exactly once with `cad.blob_read`. We assert the read
    /// is NOT attributed to `decrypt` (decryption worked); whether the
    /// downstream extract then succeeds or fails is python-dependent and
    /// out of scope here.
    #[tokio::test]
    async fn s430_valid_encrypted_blob_decrypts_and_read_is_audited() {
        let addr = s430_spawn_cad_mock(b"solid x\nendsolid".to_vec()).await;
        let db = s430_temp("read.duckdb");
        let artifacts = s430_temp("art");
        let qid = "00000000-0000-0000-0000-0000000000cc";
        let svc = s430_service(&addr, db.clone(), artifacts.clone());
        svc.enqueue_one(s430_quote(qid, "part.stl")).await.unwrap();

        let row = svc.next_actionable_blocking().await.unwrap().unwrap();
        let _ = svc.advance_one_step(row).await.unwrap();

        // The read fired exactly once (debounce is single-read here).
        assert_eq!(
            s430_count_kind(&db, "cad.blob_read"),
            1,
            "exactly one CadBlobRead per fetch"
        );
        // Decrypt succeeded → the failure (if any, no python) is NOT decrypt.
        let (_state, stage) = s430_read_state(&db, qid);
        assert_ne!(
            stage.as_deref(),
            Some("decrypt"),
            "a valid encrypted blob must decrypt cleanly"
        );

        let _ = std::fs::remove_dir_all(&artifacts);
        let _ = std::fs::remove_file(&db);
    }
}
