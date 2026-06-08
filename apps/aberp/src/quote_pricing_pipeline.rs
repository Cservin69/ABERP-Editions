//! S279 / PR-265 — auto-quoting producer pipeline.
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

use aberp_audit_ledger::{
    append_in_tx, ensure_schema as audit_ensure_schema, Actor, BinaryHash, EventKind, LedgerMeta,
    TenantId,
};
use aberp_cad_extract_wrapper::{CadExtractor, ExtractRequest};
use aberp_quote_engine::{
    self as engine, ComplexityRule as EngineComplexityRule, FeatureGraph,
    Material as EngineMaterial, QuoteBreakdown, QuotingParameters, StockAdjustment, StockStatus,
    ToleranceMultiplier, ToleranceRange,
};
use aberp_quote_pdf::QuoteInputs;

use crate::quote_pricing_jobs::{self as jobs, FailureKind, JobState, PricingJobRow};

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
    pub fn new(config: PricingPipelineConfig, deps: PricingPipelineDeps) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("build pricing-pipeline HTTP client")?;
        Ok(Self {
            config,
            deps,
            client,
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
        let resp = self
            .client
            .get(&url)
            .header("Authorization", self.bearer_header()?)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow!("storefront list returned HTTP {status}"));
        }
        let parsed: StorefrontListResponse = resp
            .json()
            .await
            .context("parse storefront /api/quotes JSON")?;
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
        let cad = quote
            .files
            .iter()
            .find(|f| {
                let lower = f.filename.to_lowercase();
                lower.ends_with(".stl") || lower.ends_with(".step") || lower.ends_with(".stp")
            })
            .ok_or_else(|| anyhow!("no CAD file on quote {qid}"))?;

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
        std::fs::write(&dest_path, &body)
            .with_context(|| format!("write {}", dest_path.display()))?;

        let material_grade = quote.request.material_preference.clone();
        let quantity = quote.request.quantity.unwrap_or(1).max(1) as u32;
        let customer_email = quote.contact.email.clone();
        let customer_name = quote.contact.name.clone();

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

    async fn writeback_quoting_status(&self, quote_id: &str, notes: &str) -> Result<()> {
        let url = format!("{}/api/quotes/{}/status", self.config.base_url, quote_id);
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
            // next_actionable_job; if they do something is wrong.
            JobState::Posted | JobState::Failed => Ok(StepOutcome::Advanced),
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
            let extractor = CadExtractor::new().with_python_bin(python_bin);
            let req = ExtractRequest {
                input_path: PathBuf::from(&arts.cad_local_path),
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
            let graph: FeatureGraph =
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
            let engine_params = convert_parameters(&params);

            match engine::quote(
                &graph,
                &engine_materials,
                &engine_complexity,
                &engine_tolerance,
                &engine_stock_adjustments,
                &engine_params,
                qty,
                target_tol,
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
                        labor_cost_eur: breakdown.labor_cost,
                        setup_cost_eur: breakdown.setup_cost,
                        overhead_eur: breakdown.overhead,
                        margin_eur: breakdown.margin,
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
                customer_company: "",
                quantity: qty,
                notes: "",
                valid_until_iso: &valid_until,
                extractor_version: aberp_cad_extract_wrapper::WRAPPER_VERSION,
                engine_version: &breakdown.engine_version,
                feature_graph: &graph,
                breakdown: &breakdown,
                target_tolerance: target_tol,
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

        let post_outcome_state = post_result.as_ref().map(|r| r.outcome).ok();
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
            match post_result {
                Ok(PostResult { outcome, .. }) => {
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
                        // skip audit emit to avoid double-firing.
                        tracing::info!(
                            quote_id = %quote_id_persist,
                            outcome = ?set_outcome,
                            "set_state(Posted) no-op; skipping audit emit"
                        );
                        let _ = post_outcome_state;
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
                        idempotent: matches!(outcome, PostOutcome::Idempotent),
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
                    let _ = post_outcome_state;
                    Ok(StepOutcome::Posted)
                }
                Err(e) => {
                    let reason = e.to_string();
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
    async fn post_priced_writeback(
        &self,
        quote_id: &str,
        feature_graph_hash: &str,
        valid_until_iso: &str,
        breakdown_json: &str,
        pdf_bytes: &[u8],
    ) -> Result<PostResult> {
        let url = format!("{}/api/quotes/{}/priced", self.config.base_url, quote_id);
        let boundary = format!("aberp-mp-{}", Ulid::new());
        let body = build_priced_multipart(
            &boundary,
            feature_graph_hash,
            valid_until_iso,
            breakdown_json,
            pdf_bytes,
        )?;
        let resp = self
            .client
            .post(&url)
            .header("Authorization", self.bearer_header()?)
            .header(
                "Content-Type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(body)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        let status = resp.status();
        let body_text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!("priced-writeback HTTP {status} body={body_text}"));
        }
        // 200 path: distinguish idempotent replay via JSON shape per
        // ADR-0004 — `{status:"quoted", idempotent:true}` vs the
        // fresh `{status:"quoted"}` shape.
        let parsed: PricedWritebackOk = serde_json::from_str(&body_text)
            .with_context(|| format!("parse priced-writeback ok JSON: {body_text}"))?;
        let outcome = if parsed.idempotent.unwrap_or(false) {
            PostOutcome::Idempotent
        } else {
            PostOutcome::Fresh
        };
        Ok(PostResult { outcome })
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PostOutcome {
    Fresh,
    Idempotent,
}

#[derive(Debug, Clone)]
struct PostResult {
    outcome: PostOutcome,
}

#[derive(Debug, Deserialize)]
struct PricedWritebackOk {
    #[allow(dead_code)]
    status: Option<String>,
    idempotent: Option<bool>,
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
/// - **stage="extract"** + reason contains `"not yet implemented"` →
///   `Permanent`. The STEP-extractor stub (S269) returns this verbatim
///   for any non-STL submission; nothing the daemon can do without an
///   OCCT lift (S270+).
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
    // POST-back classification — split on HTTP status family + transport
    // failure shape.
    if stage == "post" {
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
                machinability_index: m.machinability_index,
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
        .map(|r| EngineComplexityRule {
            id: r.id,
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

/// Hard-coded `machining_rate_eur_per_minute` until the local
/// `quoting_parameters` table grows the column.
///
/// **Gap:** the S267 `quoting_parameters` singleton (CRUD'd from
/// Settings → Quoting Parameters) does NOT yet carry a `machining_rate`
/// field, but the engine's S268 [`QuotingParameters`] requires one.
/// Documented pushback: this PR ships the producer-side bridge with a
/// flat 1.0 EUR/min default — pricing is wrong-but-monotonic until the
/// column lands. The right follow-up is a one-row migration on
/// `quoting_parameters` + the matching SPA form field; out of scope here
/// because the schema-edit + SPA edit + audit kind broaden the PR
/// beyond producer-pipeline plumbing.
const DEFAULT_MACHINING_RATE_EUR_PER_MIN: f64 = 1.0;

fn convert_parameters(local: &crate::quoting_tunables::QuotingParameters) -> QuotingParameters {
    QuotingParameters {
        scrap_factor: local.scrap_factor,
        machining_rate_eur_per_minute: DEFAULT_MACHINING_RATE_EUR_PER_MIN,
        // Local table uses i64; engine expects u32. Clamp negative +
        // saturate; the SPA enforces ≥ 1 already (per S267 validation).
        setup_amortization_threshold: local.setup_amortization_threshold.clamp(0, u32::MAX as i64)
            as u32,
        overhead_factor: local.overhead_factor,
        profit_margin_base: local.profit_margin_base,
        min_margin: local.min_margin,
        exotic_material_tax: local.exotic_material_tax,
    }
}

/// Build the multipart body per ADR-0004 §"Wire shape". Two parts:
/// JSON `meta` + binary `pdf`. Bytes are CRLF-delimited per RFC 7578.
pub(crate) fn build_priced_multipart(
    boundary: &str,
    feature_graph_hash: &str,
    valid_until_iso: &str,
    breakdown_json: &str,
    pdf_bytes: &[u8],
) -> Result<Vec<u8>> {
    let breakdown_value: serde_json::Value = serde_json::from_str(breakdown_json)
        .with_context(|| format!("re-parse breakdown_json: {breakdown_json}"))?;
    let meta = serde_json::json!({
        "breakdown_json": breakdown_value,
        "valid_until": valid_until_iso,
        "feature_graph_hash": feature_graph_hash,
        "extractor_version": aberp_cad_extract_wrapper::WRAPPER_VERSION,
        "engine_version": engine::ENGINE_VERSION,
        "stock_alert": false,
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

#[derive(Debug, Serialize)]
struct QuotePricingPricedPayload {
    quote_id: String,
    tenant_id: String,
    engine_version: String,
    total_price_eur: f64,
    material_cost_eur: f64,
    labor_cost_eur: f64,
    setup_cost_eur: f64,
    overhead_eur: f64,
    margin_eur: f64,
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
        let body =
            build_priced_multipart("b", "blake3:1", "2026-07-06", "{}", b"x").expect("build");
        let s = String::from_utf8_lossy(&body).to_string();
        assert!(
            s.contains("\"extractor_version\":"),
            "missing extractor_version"
        );
        assert!(s.contains("\"engine_version\":"), "missing engine_version");
        assert!(s.contains("\"stock_alert\":false"), "missing stock_alert");
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
}
