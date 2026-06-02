//! AP-side auto-sync daemon — S178 / PR-178.
//!
//! Pairs the S177 [`crate::incoming_invoices::ingest_incoming_invoice`]
//! foundation with NAV's `queryInvoiceDigest INBOUND` endpoint to
//! mirror supplier-issued invoices into the local `ap_invoice` table
//! without operator action. Ervin's posture: "low resource
//! utilization low priority database sync."
//!
//! # Cadence
//!
//!   - **Boot tick**: 30 seconds after `serve` start so the hot
//!     launch path is uncontested.
//!   - **Steady cadence**: every 30 minutes.
//!   - **Manual trigger**: `POST /api/incoming-invoices/sync-now`
//!     calls [`run_one_cycle`] synchronously and returns the
//!     ingest/skip counts in the JSON body.
//!
//! # Window
//!
//!   - **30-day rolling window** (`today - 30 .. today`). NAV's
//!     per-request cap is 35 days (per the v3.0 XSD); 30 leaves
//!     operator margin for clock skew + the "ingest the same
//!     invoice that came in last night" overlap. Flagged in the
//!     S178 brief — bump to 35 if operator-visible drops appear.
//!
//! # Pagination + safety
//!
//!   - The daemon walks pages until `current_page >= available_page`
//!     OR the [`MAX_PAGES_PER_CYCLE`] safety cap fires (10K
//!     invoices / 100 per page). A capped cycle logs a `warn!` and
//!     records the truncation on the cycle's audit entry so the
//!     operator sees the silent-omission risk loud per CLAUDE.md
//!     rule 12.
//!   - Concurrency is sequential (no per-digest fanout). The data
//!     volume is small and the daemon is deliberately gentle on
//!     NAV.
//!
//! # Idempotency
//!
//!   - `ingest_incoming_invoice` is idempotent on the UNIQUE
//!     `(tenant, supplier_tax_number, nav_invoice_number)` key per
//!     S177. The daemon does NOT pre-check existence — the helper
//!     returns `AlreadyExists { id }` for duplicates which the
//!     daemon counts as `skipped_count`.
//!
//! # Audit
//!
//!   - One [`audit_payloads::IncomingInvoiceSyncCycleCompletedPayload`]
//!     per cycle, written via
//!     `aberp_audit_ledger::EventKind::IncomingInvoiceSyncCycleCompleted`.
//!   - Per-digest ingestions emit their own `IncomingInvoiceIngested`
//!     entries via `ingest_incoming_invoice` (same path as the manual
//!     route).
//!
//! # What this module DELIBERATELY does NOT do
//!
//!   - (S197 update) The follow-on `queryInvoiceData` XML fetch IS
//!     wired now — see [`fetch_and_persist_xml_for_row`]. Per digest
//!     newly inserted (or backfill: previously ingested with
//!     `nav_xml_path` still NULL), the daemon issues one
//!     `queryInvoiceData INBOUND` call, base64-decodes the inner
//!     `<invoiceData>` blob via
//!     [`crate::restore_from_nav_extract::extract_inner_invoice_data_xml`]
//!     (the S196 helper — same NAV envelope shape), writes the bytes
//!     to `~/.aberp/<tenant>/ap-artifacts/<apinv_id>.xml`, and
//!     UPDATEs `ap_invoice.nav_xml_path`. Per-row failures (HTTP
//!     non-success, base64 / parse error, file IO) are CONTAINED —
//!     they `tracing::warn!` and leave the row's `nav_xml_path`
//!     NULL; the next cycle re-attempts. The XML fetch is
//!     idempotent: rows with `nav_xml_path` already set are skipped.
//!     Concurrency stays sequential (one queryInvoiceData at a time
//!     per cycle) per the daemon's gentle-on-NAV posture.
//!   - It does NOT short-circuit on `outcome != IngestOutcome::Created`.
//!     The daemon walks every page and counts both inserts + skips so
//!     the cycle entry is honest about the volume seen, not just the
//!     volume changed.
//!   - It does NOT trigger NAV setup or boot-state checks. The caller
//!     must be in `ServeBootState::Ready` (the spawn point in
//!     `serve.rs` checks; the manual route runs through
//!     `require_ready`).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use time::{format_description::FormatItem, macros, OffsetDateTime};
use tokio_util::sync::CancellationToken;
use ulid::Ulid;

use aberp_audit_ledger::{self as audit_ledger, Actor, BinaryHash, EventKind, Ledger, TenantId};
use aberp_billing::IdempotencyKey;
use aberp_nav_transport::operations::query_invoice_data;
use aberp_nav_transport::operations::query_invoice_digest::{
    self, InvoiceDigest, QueryInvoiceDigestPage,
};
use aberp_nav_transport::soap::InvoiceDirection;
use aberp_nav_transport::{NavCredentials, NavEndpoint, NavTransport};

use crate::audit_payloads::IncomingInvoiceSyncCycleCompletedPayload;
use crate::incoming_invoices::{self, IngestOutcome, IngestionInput};
use crate::restore_from_nav_extract;

/// Boot delay before the first daemon tick. 30s gives `serve`'s
/// other boot tasks (NAV poll daemon recovery, mirror reconciliation)
/// uncontested CPU.
pub const BOOT_DELAY_SECS: u64 = 30;

/// Steady-state cadence between daemon ticks. 30 minutes per the
/// session-178 brief — small data volume + low priority => no need
/// to hammer NAV.
pub const CADENCE_SECS: u64 = 30 * 60;

/// Date-window width in days. NAV's per-request cap is 35; the
/// 30-day choice leaves operator margin.
pub const WINDOW_DAYS: i64 = 30;

/// Per-cycle pagination cap. 100 pages × ~100 digests/page = 10K
/// invoices. A capped cycle records the truncation in the audit
/// entry so the operator can re-run /sync-now manually with the
/// next window slice.
pub const MAX_PAGES_PER_CYCLE: u32 = 100;

/// Closed-vocab trigger label persisted on the cycle audit entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CycleTrigger {
    /// Boot tick (30s after `serve` start) or steady-state cadence
    /// (every 30 min).
    Daemon,
    /// Operator-clicked `/api/incoming-invoices/sync-now`.
    Manual,
    /// PR-203 / S203 — one chunk of the year-to-date bootstrap sweep
    /// that fires on the FIRST process boot after PR-203 lands (or any
    /// boot whose audit ledger has no prior `bootstrap-year` cycle row,
    /// so the bootstrap re-runs after an audit-ledger wipe). One audit
    /// row PER MONTH CHUNK so the operator can see exactly which
    /// month-window pulled what.
    BootstrapYear,
}

impl CycleTrigger {
    pub fn as_audit_str(self) -> &'static str {
        match self {
            CycleTrigger::Daemon => "daemon",
            CycleTrigger::Manual => "manual",
            // PR-203 / S203 — closed-vocab token consumed by the SPA's
            // audit-row renderer + the bootstrap-already-ran detector.
            CycleTrigger::BootstrapYear => "bootstrap-year",
        }
    }
}

/// Result of one cycle. Surfaced to the manual route handler so the
/// SPA can echo a toast like "synced 3 new / 47 skipped in 412 ms."
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CycleSummary {
    pub trigger: CycleTrigger,
    pub date_from: String,
    pub date_to: String,
    pub ingested_count: u64,
    pub skipped_count: u64,
    pub pages_walked: u32,
    pub elapsed_ms: u64,
    pub error: Option<String>,
}

/// Inputs to [`run_one_cycle`]. The daemon's spawn site in
/// `serve.rs` builds one of these per tick; the manual route does
/// the same.
pub struct CycleInputs {
    pub db_path: PathBuf,
    pub tenant: TenantId,
    pub binary_hash: BinaryHash,
    pub operator_login: String,
    pub ap_artifacts_dir: PathBuf,
    pub tax_number_8: String,
    pub endpoint: NavEndpoint,
    pub credentials: NavCredentials,
}

/// Spawn the auto-sync daemon as a background task. Returns
/// immediately — the daemon ticks forever (or until the runtime
/// shuts down). Boot-recovery posture: a daemon panic / loud-failure
/// is logged at `warn!` and the daemon dies; the next process boot
/// re-spawns. The audit chain remains the source of truth for
/// `ingested_count` / `skipped_count` per cycle, so a missed cycle is
/// recoverable on the next tick.
///
/// PR-209 / S213 — `cancel` is the shutdown token. The boot-delay
/// sleep, the bootstrap-year sweep, and the steady-cadence sleep all
/// race against `cancel.cancelled()` so a Ctrl-C / window-close
/// during a 30-minute idle window exits within the shutdown timeout
/// instead of waiting out the cadence. Cancellation between cycles
/// is silent — the daemon has nothing in flight to flush.
pub async fn run_daemon_forever<F>(build_inputs: F, cancel: CancellationToken)
where
    F: Fn() -> Result<CycleInputs> + Send + Sync + 'static,
{
    let build_inputs = Arc::new(build_inputs);
    tokio::select! {
        _ = cancel.cancelled() => return,
        _ = tokio::time::sleep(Duration::from_secs(BOOT_DELAY_SECS)) => {}
    }
    // PR-203 / S203 — one-shot year-to-date bootstrap on the FIRST boot
    // tick. The sentinel scan inside `run_bootstrap_year_once` short-
    // circuits on subsequent boots whose audit ledger already records
    // a `bootstrap-year` cycle, so this is a guarded once-per-DB sweep
    // (re-runs after wipe/restore, idempotent at the digest UNIQUE on
    // overlap).
    //
    // PR-209 / S213 — bootstrap is itself a long-running sweep
    // (≤ 12 month-window queries). Race against shutdown so a
    // first-boot shutdown doesn't stall the coordinator for minutes.
    tokio::select! {
        _ = cancel.cancelled() => return,
        _ = run_bootstrap_year_once(&*build_inputs) => {}
    }
    loop {
        // PR-209 / S213 — check the token BEFORE each cycle.
        // run_one_cycle's NAV calls have their own timeouts; if a
        // cycle is in flight when shutdown fires it finishes (worst-
        // case ~30s) and the next loop iteration observes the
        // cancellation. Acceptable: the shutdown timeout (5s default)
        // would name `ap-sync` as a timeout_kill in that case, which
        // accurately reports the situation.
        if cancel.is_cancelled() {
            return;
        }
        match build_inputs() {
            Ok(inputs) => match run_one_cycle(inputs, CycleTrigger::Daemon).await {
                Ok(summary) => {
                    tracing::info!(
                        ingested = summary.ingested_count,
                        skipped = summary.skipped_count,
                        pages = summary.pages_walked,
                        elapsed_ms = summary.elapsed_ms,
                        error = ?summary.error,
                        "AP auto-sync cycle complete"
                    );
                }
                Err(e) => tracing::warn!(error = %format!("{e:#}"), "AP auto-sync cycle failed"),
            },
            Err(e) => tracing::warn!(
                error = %format!("{e:#}"),
                "AP auto-sync skipped (build_inputs failed; will retry on next tick)"
            ),
        }
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(Duration::from_secs(CADENCE_SECS)) => {}
        }
    }
}

/// Run one sync cycle: query the digest by page, ingest each new
/// row via `ingest_incoming_invoice`, write the cycle audit entry.
/// The cycle audit entry fires UNCONDITIONALLY at the end (success
/// or loud-failure) so the audit trail has zero gaps.
pub async fn run_one_cycle(inputs: CycleInputs, trigger: CycleTrigger) -> Result<CycleSummary> {
    let (date_from, date_to) = compute_date_window(OffsetDateTime::now_utc())?;
    run_one_cycle_for_window(inputs, trigger, date_from, date_to).await
}

/// PR-203 / S203 — run one sync cycle for an EXPLICIT date window.
/// The 30-day rolling window helper [`run_one_cycle`] delegates to this
/// after computing its own window; the year-to-date bootstrap calls
/// this once per month chunk with the chunk's start/end.
///
/// Same audit + idempotency posture as [`run_one_cycle`]: one
/// `IncomingInvoiceSyncCycleCompleted` audit row per call (success OR
/// loud-failure); per-digest idempotency on the
/// `(tenant, supplier_tax_number, nav_invoice_number)` UNIQUE so
/// overlapping windows during bootstrap re-ingest as `AlreadyExists` /
/// skip rather than duplicating rows.
pub async fn run_one_cycle_for_window(
    inputs: CycleInputs,
    trigger: CycleTrigger,
    date_from: String,
    date_to: String,
) -> Result<CycleSummary> {
    let started = Instant::now();
    let result = run_cycle_inner(&inputs, &date_from, &date_to).await;

    let elapsed_ms = started.elapsed().as_millis() as u64;
    let (ingested_count, skipped_count, pages_walked, error) = match &result {
        Ok((i, s, p)) => (*i, *s, *p, None),
        Err(e) => (0, 0, 0, Some(format!("{e:#}"))),
    };

    let summary = CycleSummary {
        trigger,
        date_from: date_from.clone(),
        date_to: date_to.clone(),
        ingested_count,
        skipped_count,
        pages_walked,
        elapsed_ms,
        error: error.clone(),
    };

    // Best-effort audit-entry write. A write-failure here logs loud
    // but does NOT mask the caller's original error. S191 — the
    // sync DuckDB write is fenced inside `spawn_blocking` so the
    // tokio worker pool is not blocked for the duration of the
    // INSERT + chain-verify + mirror-sync. `JoinError` is unified
    // into the existing warn! surface.
    let audit_inputs_db = inputs.db_path.clone();
    let audit_inputs_tenant = inputs.tenant.clone();
    let audit_inputs_binary_hash = inputs.binary_hash;
    let audit_inputs_login = inputs.operator_login.clone();
    let audit_summary = summary.clone();
    let audit_outcome = tokio::task::spawn_blocking(move || {
        write_cycle_audit_entry_inner(
            &audit_inputs_db,
            audit_inputs_tenant,
            audit_inputs_binary_hash,
            &audit_inputs_login,
            &audit_summary,
        )
    })
    .await;
    match audit_outcome {
        Ok(Ok(())) => {}
        Ok(Err(audit_err)) => tracing::warn!(
            error = %format!("{audit_err:#}"),
            "failed to write IncomingInvoiceSyncCycleCompleted audit entry"
        ),
        Err(join_err) => tracing::warn!(
            error = %format!("{join_err}"),
            "IncomingInvoiceSyncCycleCompleted audit-write task panicked"
        ),
    }

    match result {
        Ok(_) => Ok(summary),
        Err(e) => Err(e),
    }
}

/// S197 — one row that the per-page ingest pass surfaced as needing
/// an XML follow-on fetch. `Created` rows always need fetch; an
/// `AlreadyExists` row needs fetch only if its `nav_xml_path` was
/// still NULL from a prior digest-only cycle (backfill posture).
struct XmlFetchTarget {
    id: String,
    invoice_number: String,
}

async fn run_cycle_inner(
    inputs: &CycleInputs,
    date_from: &str,
    date_to: &str,
) -> Result<(u64, u64, u32)> {
    let transport =
        NavTransport::new(inputs.endpoint).context("build NAV transport for AP sync cycle")?;

    let mut ingested_count: u64 = 0;
    let mut skipped_count: u64 = 0;
    let mut page: u32 = 1;

    loop {
        if page > MAX_PAGES_PER_CYCLE {
            tracing::warn!(
                cap = MAX_PAGES_PER_CYCLE,
                "AP auto-sync hit per-cycle page cap; truncating — \
                 operator should re-run /sync-now to walk the remainder"
            );
            return Ok((ingested_count, skipped_count, page - 1));
        }

        let page_result: QueryInvoiceDigestPage = query_invoice_digest::call(
            &transport,
            &inputs.credentials,
            &inputs.tax_number_8,
            page,
            InvoiceDirection::Inbound,
            date_from,
            date_to,
        )
        .await
        .with_context(|| format!("queryInvoiceDigest page {page}"))?;

        let available_page = page_result.available_page;

        // S191 — process the whole page's digests on the blocking
        // pool so the tokio worker is not held across N synchronous
        // DuckDB INSERT + chain-verify + mirror-sync calls. One
        // `spawn_blocking` per page keeps the boundary-cross count at
        // O(pages) instead of O(digests).
        //
        // S197 — the blocking pass ALSO classifies each row's XML-
        // fetch need: `Created` always needs fetch; `AlreadyExists`
        // needs fetch only when the row's existing `nav_xml_path` is
        // still NULL (backfill posture for digests previously ingested
        // pre-S197). The async XML fanout runs AFTER the spawn_blocking
        // returns so the queryInvoiceData HTTP calls are NOT held on
        // the blocking pool.
        let digests = page_result.digests;
        let db_path = inputs.db_path.clone();
        let tenant = inputs.tenant.clone();
        let binary_hash = inputs.binary_hash;
        let operator_login = inputs.operator_login.clone();
        let ap_artifacts_dir = inputs.ap_artifacts_dir.clone();
        let (page_ingested, page_skipped, xml_targets) = tokio::task::spawn_blocking(move || {
            let mut ingested: u64 = 0;
            let mut skipped: u64 = 0;
            let mut targets: Vec<XmlFetchTarget> = Vec::new();
            for digest in digests {
                match digest_to_ingestion_input(&digest) {
                    Ok(input) => {
                        match incoming_invoices::ingest_incoming_invoice(
                            &db_path,
                            tenant.clone(),
                            binary_hash,
                            &operator_login,
                            &ap_artifacts_dir,
                            input,
                        ) {
                            Ok(IngestOutcome::Created { id }) => {
                                ingested += 1;
                                targets.push(XmlFetchTarget {
                                    id,
                                    invoice_number: digest.invoice_number.clone(),
                                });
                            }
                            Ok(IngestOutcome::AlreadyExists { id }) => {
                                skipped += 1;
                                // S197 backfill — re-read the row's
                                // `nav_xml_path`; queue the fetch only
                                // when still NULL. A DB read failure
                                // here is non-fatal: surface as warn
                                // and skip the row this cycle.
                                match incoming_invoices::get_nav_xml_path(
                                    &db_path,
                                    tenant.as_str(),
                                    &id,
                                ) {
                                    Ok(None) => targets.push(XmlFetchTarget {
                                        id,
                                        invoice_number: digest.invoice_number.clone(),
                                    }),
                                    Ok(Some(_)) => {}
                                    Err(e) => {
                                        tracing::warn!(
                                            ap_invoice_id = %id,
                                            error = ?e,
                                            "get_nav_xml_path failed; XML backfill skipped this cycle"
                                        );
                                    }
                                }
                            }
                            Err(e) => {
                                // A single-digest ingest failure must NOT
                                // abort the whole cycle — the digest is
                                // logged loud and the daemon continues.
                                // Otherwise one malformed row from NAV
                                // would block every subsequent row.
                                tracing::warn!(
                                    invoice_number = %digest.invoice_number,
                                    supplier_tax = %digest.supplier_tax_number,
                                    error = ?e,
                                    "ingest_incoming_invoice failed for digest; continuing cycle"
                                );
                                skipped += 1;
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            invoice_number = %digest.invoice_number,
                            supplier_tax = %digest.supplier_tax_number,
                            error = ?e,
                            "digest → IngestionInput conversion failed; skipping"
                        );
                        skipped += 1;
                    }
                }
            }
            (ingested, skipped, targets)
        })
        .await
        .map_err(|join_err| anyhow!("AP sync per-page ingest task panicked: {join_err}"))?;
        ingested_count += page_ingested;
        skipped_count += page_skipped;

        // S197 — sequential queryInvoiceData fan-out per row needing
        // XML enrichment. Per-row failures are contained (warn + leave
        // nav_xml_path NULL); the next cycle re-attempts. Sequential
        // (one NAV call at a time) per the daemon's gentle-on-NAV
        // posture documented in the module header.
        let mut xml_fetch_ok: u64 = 0;
        let mut xml_fetch_err: u64 = 0;
        for target in xml_targets {
            match fetch_and_persist_xml_for_row(
                &transport,
                &inputs.credentials,
                &inputs.tax_number_8,
                &inputs.db_path,
                &inputs.tenant,
                &inputs.ap_artifacts_dir,
                &target,
            )
            .await
            {
                Ok(()) => xml_fetch_ok += 1,
                Err(e) => {
                    xml_fetch_err += 1;
                    tracing::warn!(
                        ap_invoice_id = %target.id,
                        invoice_number = %target.invoice_number,
                        error = %format!("{e:#}"),
                        "AP queryInvoiceData fetch failed; nav_xml_path stays NULL — next cycle re-attempts"
                    );
                }
            }
        }
        if xml_fetch_ok > 0 || xml_fetch_err > 0 {
            tracing::info!(
                page,
                xml_fetched = xml_fetch_ok,
                xml_failed = xml_fetch_err,
                "AP queryInvoiceData fetches complete for page"
            );
        }

        if page >= available_page {
            return Ok((ingested_count, skipped_count, page));
        }
        page += 1;
    }
}

/// S197 — fetch the full NAV InvoiceData XML for ONE just-ingested (or
/// previously-ingested but XML-less) `ap_invoice` row. Pipeline:
///
///   1. `queryInvoiceData INBOUND` for the row's NAV invoice number.
///   2. Base64-decode the inner `<invoiceData>` blob via the S196
///      [`restore_from_nav_extract::extract_inner_invoice_data_xml`]
///      helper (same NAV envelope shape; not duplicated here).
///   3. Persist the inner XML bytes to
///      `<ap_artifacts_dir>/<ap_invoice_id>.xml`.
///   4. `UPDATE ap_invoice SET nav_xml_path = ?` via
///      [`incoming_invoices::set_nav_xml_path`].
///
/// Every error path returns `Err(...)`; the caller (the cycle loop)
/// turns it into a `warn!` and continues — one row's XML fetch failure
/// must NOT abort the cycle. No audit entry is written for the success
/// path: the `IncomingInvoiceIngested` payload covering the row has
/// already landed; the XML fetch is operator-invisible enrichment.
async fn fetch_and_persist_xml_for_row(
    transport: &NavTransport,
    credentials: &NavCredentials,
    tax_number_8: &str,
    db_path: &std::path::Path,
    tenant: &TenantId,
    ap_artifacts_dir: &std::path::Path,
    target: &XmlFetchTarget,
) -> Result<()> {
    let outcome = query_invoice_data::call(
        transport,
        credentials,
        tax_number_8,
        &target.invoice_number,
        InvoiceDirection::Inbound,
    )
    .await
    .with_context(|| {
        format!(
            "queryInvoiceData INBOUND for {} (ap_invoice_id={})",
            target.invoice_number, target.id
        )
    })?;

    let response_xml = outcome.response_xml;
    let db_path_owned = db_path.to_path_buf();
    let tenant_owned = tenant.clone();
    let artifacts_dir_owned = ap_artifacts_dir.to_path_buf();
    let target_id = target.id.clone();
    let target_invoice_number = target.invoice_number.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        persist_xml_for_row(
            &response_xml,
            &db_path_owned,
            tenant_owned.as_str(),
            &artifacts_dir_owned,
            &target_id,
            &target_invoice_number,
        )
    })
    .await
    .map_err(|join_err| anyhow!("AP XML persist task panicked: {join_err}"))??;
    Ok(())
}

/// S197 — synchronous persist half of [`fetch_and_persist_xml_for_row`].
/// Split out so the spawn_blocking closure is one call and the unit
/// tests can exercise the extract → write → UPDATE pipeline without
/// standing up a `NavTransport`. `response_xml` is the verbatim
/// `<QueryInvoiceDataResponse>` envelope NAV returned; the helper
/// base64-decodes the inner `<invoiceData>` blob and persists those
/// bytes (the supplier's original `<InvoiceData>` XML root).
///
/// PR-215 / S217 — three outcomes:
///   - `<invoiceData>` present + decodes  → write XML file + UPDATE
///                                          `nav_xml_path`, return `Ok(())`
///   - `<invoiceData>` ABSENT             → `info!` + return `Ok(())`,
///                                          leave `nav_xml_path` NULL,
///                                          NO `.failed/` capture (this is
///                                          the legitimate NAV "supplier
///                                          has not exposed XML to buyer"
///                                          case — every one of the 13/13
///                                          2026-06-01 prod cycle failures
///                                          falls under this branch).
///   - `<invoiceData>` present + malformed → `capture_failing_response()`
///                                           + return `Err(...)` (this is
///                                           a genuine contract violation
///                                           worth surfacing for triage).
fn persist_xml_for_row(
    response_xml: &[u8],
    db_path: &std::path::Path,
    tenant: &str,
    ap_artifacts_dir: &std::path::Path,
    ap_invoice_id: &str,
    invoice_number: &str,
) -> Result<()> {
    let inner = match restore_from_nav_extract::extract_inner_invoice_data_xml(response_xml) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            // PR-215 / S217 — per the NAV OSA 3.0 XSD,
            // `QueryInvoiceDataResponseType.invoiceDataResult` is
            // `minOccurs=0`; NAV legitimately returns funcCode=OK
            // without it whenever the supplier has not exposed the
            // original XML payload to the buyer (paper invoices,
            // partial-data submissions, supplier opted out of XML
            // republication). The buyer's entitlement ends at the
            // digest in those cases. Leave nav_xml_path NULL — the SPA
            // already hides the XML download button when null — and
            // do NOT write to `.failed/` (would mis-signal a real
            // failure to the next-session operator). The row stays
            // digest-only forever, which is the intended steady-state
            // for invoices the supplier has chosen not to expose.
            tracing::info!(
                ap_invoice_id = %ap_invoice_id,
                invoice_number = %invoice_number,
                "queryInvoiceData INBOUND: NAV returned funcCode=OK without \
                 <invoiceData> (supplier has not exposed the XML payload to \
                 the buyer — paper invoice, partial-data submission, or \
                 supplier opted out of republication). Row stays digest-only; \
                 nav_xml_path remains NULL."
            );
            return Ok(());
        }
        Err(extract_err) => {
            // PR-214 / S216 — diagnostic capture for genuinely
            // malformed `<invoiceData>` payloads (present but empty
            // text, base64 garbage, structurally malformed XML around
            // it). Pre-PR-215 the loud-fail also fired on the absent
            // case, which produced the 13/13 false-positive captures
            // on the 2026-06-01 prod cycle. Post-PR-215 the absent
            // case is handled in the `Ok(None)` arm above and this
            // arm only fires on genuine contract violations worth
            // surfacing for triage.
            //
            // Defence-in-depth: every capture side-effect is
            // best-effort — the original `extract_err` is the
            // contract-bearing return value and must NOT be masked by
            // a filesystem failure during capture.
            capture_failing_response(ap_artifacts_dir, ap_invoice_id, response_xml);
            return Err(extract_err).with_context(|| {
                format!(
                    "base64-decode <invoiceData> for {} (ap_invoice_id={})",
                    invoice_number, ap_invoice_id
                )
            });
        }
    };
    std::fs::create_dir_all(ap_artifacts_dir).with_context(|| {
        format!(
            "create AP artifacts directory at {}",
            ap_artifacts_dir.display()
        )
    })?;
    let file_path = ap_artifacts_dir.join(format!("{}.xml", ap_invoice_id));
    std::fs::write(&file_path, &inner)
        .with_context(|| format!("write AP NAV XML artifact to {}", file_path.display()))?;
    incoming_invoices::set_nav_xml_path(
        db_path,
        tenant,
        ap_invoice_id,
        &file_path.to_string_lossy(),
    )
    .with_context(|| {
        format!(
            "UPDATE ap_invoice.nav_xml_path for ap_invoice_id={}",
            ap_invoice_id
        )
    })?;
    Ok(())
}

/// PR-214 / S216 — best-effort diagnostic capture for a failing
/// `queryInvoiceData INBOUND` response. Saves the raw response bytes
/// to `<ap_artifacts_dir>/.failed/<ap_invoice_id>.xml` (overwrites any
/// prior capture for the same row — only the latest matters for
/// next-session triage) and logs a 500-byte preview with HU tax IDs
/// redacted. Every failure inside this helper is swallowed via
/// `tracing::warn!` — the caller's original extraction error is the
/// contract-bearing surface and must NOT be masked by a filesystem
/// failure during diagnostic capture.
///
/// Supplier names are NOT redacted: they're already surfaced in the
/// per-row warn! lines the daemon emits during the cycle and they're
/// what the operator needs to correlate the captured files with the
/// observed failure.
fn capture_failing_response(
    ap_artifacts_dir: &std::path::Path,
    ap_invoice_id: &str,
    response_xml: &[u8],
) {
    let failed_dir = ap_artifacts_dir.join(".failed");
    if let Err(e) = std::fs::create_dir_all(&failed_dir) {
        tracing::warn!(
            ap_invoice_id = %ap_invoice_id,
            error = %format!("{e:#}"),
            "diagnostic capture: failed to create .failed/ directory"
        );
        // Fall through — the preview log line below still gives the
        // operator something to read from, even without the file save.
    } else {
        let file_path = failed_dir.join(format!("{}.xml", ap_invoice_id));
        match std::fs::write(&file_path, response_xml) {
            Ok(()) => tracing::warn!(
                ap_invoice_id = %ap_invoice_id,
                path = %file_path.display(),
                bytes = response_xml.len(),
                "diagnostic capture: saved failing queryInvoiceData response — \
                 share with next-session triage to identify NAV-side shape change"
            ),
            Err(e) => tracing::warn!(
                ap_invoice_id = %ap_invoice_id,
                path = %file_path.display(),
                error = %format!("{e:#}"),
                "diagnostic capture: failed to write capture file"
            ),
        }
    }
    let preview = sanitise_response_preview(response_xml, 500);
    tracing::warn!(
        ap_invoice_id = %ap_invoice_id,
        preview = %preview,
        "diagnostic capture: queryInvoiceData response preview (first 500 bytes, HU tax IDs redacted)"
    );
}

/// PR-214 / S216 — produce a tax-ID-redacted preview of a NAV
/// response body suitable for a `tracing::warn!` field. Truncates to
/// at most `max_bytes` UTF-8 bytes (boundary-aware — never splits a
/// multi-byte code point) and then redacts any HU tax-ID pattern via
/// [`redact_hu_tax_ids`]. The truncation marker `…` is appended when
/// the input exceeds `max_bytes`.
fn sanitise_response_preview(response_xml: &[u8], max_bytes: usize) -> String {
    let s = String::from_utf8_lossy(response_xml);
    let truncated: String = if s.len() <= max_bytes {
        s.into_owned()
    } else {
        let mut idx = max_bytes;
        while idx > 0 && !s.is_char_boundary(idx) {
            idx -= 1;
        }
        format!("{}…", &s[..idx])
    };
    redact_hu_tax_ids(&truncated)
}

/// PR-214 / S216 — strip HU tax-ID patterns from a string for safe
/// log emission. Two patterns are redacted:
///
///   1. Full HU community tax number: `NNNNNNNN-N-NN` (8 digits + `-`
///      + 1 digit + `-` + 2 digits). Replaced with `[REDACTED-TAX]`.
///   2. Bare 8-digit taxpayer ID (the first segment alone, which NAV
///      embeds inside `<taxpayerId>` elements). Replaced with
///      `[REDACTED-ID]`.
///
/// The two patterns share their first 8 digits — pattern (1) is
/// matched FIRST so a full tax number isn't redacted to
/// `[REDACTED-ID]-1-23`. Match-positions are computed by a single
/// left-to-right scan; no regex dependency is pulled in.
///
/// Supplier names, dates, currency codes, and the XML element
/// structure stay verbatim — the operator needs them to correlate
/// the redacted preview with the observed failure.
fn redact_hu_tax_ids(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while i < bytes.len() {
        // Find the start of any 8-digit run anchored at byte `i`.
        if bytes[i].is_ascii_digit() {
            let mut j = i;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            let digits_len = j - i;
            // Require exactly 8 digits AND the preceding char (if any)
            // not be a digit, so we don't redact the first 8 digits of
            // a longer numeric token like `123456789012` (NAV invoice
            // numbers can carry long digit runs).
            let preceding_is_digit = i > 0 && bytes[i - 1].is_ascii_digit();
            if digits_len == 8 && !preceding_is_digit {
                // Look for `-N-NN` continuation for pattern (1).
                let tail_len = if j + 5 <= bytes.len()
                    && bytes[j] == b'-'
                    && bytes[j + 1].is_ascii_digit()
                    && bytes[j + 2] == b'-'
                    && bytes[j + 3].is_ascii_digit()
                    && bytes[j + 4].is_ascii_digit()
                    // The 13th byte must NOT be a digit (else we'd be
                    // mid-way through `NNNNNNNN-N-NNN` which isn't a
                    // HU tax ID; leave it alone).
                    && (j + 5 == bytes.len() || !bytes[j + 5].is_ascii_digit())
                {
                    Some(5usize)
                } else {
                    None
                };
                match tail_len {
                    Some(t) => {
                        out.push_str("[REDACTED-TAX]");
                        i = j + t;
                        continue;
                    }
                    None => {
                        out.push_str("[REDACTED-ID]");
                        i = j;
                        continue;
                    }
                }
            }
            // Otherwise emit the digit run verbatim and resume.
            out.push_str(&s[i..j]);
            i = j;
            continue;
        }
        // Non-digit: emit a single UTF-8 char boundary slice.
        let next_boundary = (i + 1..=s.len())
            .find(|k| s.is_char_boundary(*k))
            .unwrap_or(s.len());
        out.push_str(&s[i..next_boundary]);
        i = next_boundary;
    }
    out
}

/// Convert a NAV digest row into an [`IngestionInput`] suitable for
/// the S177 [`incoming_invoices::ingest_incoming_invoice`] helper.
///
/// Loud-fails on:
///   - Missing or empty `supplier_name` (NAV always populates;
///     absence is schema drift per CLAUDE.md rule 12).
///   - Missing `issue_date`.
///   - Currency outside the `ap_invoice` closed vocab
///     (HUF / EUR) — the daemon does NOT silently coerce, even
///     for digests whose `currency` field is absent.
///   - Net/VAT amounts that fail to parse as `Decimal` or land
///     outside i64 minor-unit range.
fn digest_to_ingestion_input(digest: &InvoiceDigest) -> Result<IngestionInput> {
    let supplier_name = digest
        .supplier_name
        .clone()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            anyhow!(
                "digest for supplier_tax={} invoice_number={} missing <supplierName>",
                digest.supplier_tax_number,
                digest.invoice_number,
            )
        })?;
    let issue_date = digest.issue_date.clone().ok_or_else(|| {
        anyhow!(
            "digest for supplier_tax={} invoice_number={} missing <invoiceIssueDate>",
            digest.supplier_tax_number,
            digest.invoice_number,
        )
    })?;
    let currency = match digest.currency.as_deref() {
        Some("HUF") => "HUF".to_string(),
        Some("EUR") => "EUR".to_string(),
        Some(other) => {
            return Err(anyhow!(
                "digest for invoice_number={} carries currency `{}` outside ap_invoice closed vocab (HUF | EUR)",
                digest.invoice_number,
                other,
            ));
        }
        None => {
            return Err(anyhow!(
                "digest for invoice_number={} missing <currency>",
                digest.invoice_number,
            ));
        }
    };

    let net_minor = decimal_to_minor(
        digest.invoice_net_amount.as_deref().unwrap_or("0"),
        &currency,
    )
    .with_context(|| format!("parse invoice_net_amount for {}", digest.invoice_number))?;
    let vat_minor = decimal_to_minor(
        digest.invoice_vat_amount.as_deref().unwrap_or("0"),
        &currency,
    )
    .with_context(|| format!("parse invoice_vat_amount for {}", digest.invoice_number))?;
    let gross_minor = net_minor
        .checked_add(vat_minor)
        .ok_or_else(|| anyhow!("gross overflow for {}", digest.invoice_number))?;

    Ok(IngestionInput {
        supplier_tax_number: digest.supplier_tax_number.clone(),
        supplier_name,
        supplier_address: None,
        nav_invoice_number: digest.invoice_number.clone(),
        issue_date,
        delivery_date: None,
        payment_deadline: None,
        total_net_minor: net_minor,
        total_vat_minor: vat_minor,
        total_gross_minor: gross_minor,
        currency,
        nav_xml: None,
    })
}

/// Convert a NAV-string amount into minor units for the closed-vocab
/// currency. HUF has 0 decimals (forint is the minor unit); EUR has 2
/// (cents). Loud-fails on parse / overflow per CLAUDE.md rule 12.
fn decimal_to_minor(value: &str, currency: &str) -> Result<i64> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(0);
    }
    let parsed: Decimal = trimmed
        .parse()
        .map_err(|e| anyhow!("amount `{trimmed}` is not a valid Decimal: {e}"))?;
    let scale: u32 = match currency {
        "HUF" => 0,
        "EUR" => 2,
        other => {
            return Err(anyhow!(
                "decimal_to_minor called with currency `{other}` outside closed vocab"
            ));
        }
    };
    let scaled = parsed * Decimal::from(10i64.pow(scale));
    let rounded = scaled.round();
    rounded
        .to_i64()
        .ok_or_else(|| anyhow!("amount `{trimmed}` (scaled) exceeds i64 range"))
}

const ISO_DATE: &[FormatItem<'_>] = macros::format_description!("[year]-[month]-[day]");

fn compute_date_window(now_utc: OffsetDateTime) -> Result<(String, String)> {
    let today = now_utc.date();
    let from = today
        .checked_sub(time::Duration::days(WINDOW_DAYS))
        .ok_or_else(|| anyhow!("date underflow computing AP sync window"))?;
    Ok((from.format(&ISO_DATE)?, today.format(&ISO_DATE)?))
}

/// PR-203 / S203 — pause between bootstrap month chunks. Two seconds
/// matches the daemon's "gentle on NAV" posture: ~6 NAV calls per
/// minute for the digest sweep, well under any plausible rate-limit
/// ceiling AND polite enough that an operator-clocked boot sweep
/// finishes in well under a minute for a typical year-to-date that
/// has only one or two months of inbound invoices.
pub const BOOTSTRAP_CHUNK_THROTTLE_SECS: u64 = 2;

/// PR-203 / S203 — split a year-to-date window into calendar-month
/// chunks. Each chunk is `(date_from, date_to)` as canonical
/// `YYYY-MM-DD` strings. The last chunk is clamped to `now_utc.date()`
/// — running this mid-month produces a final chunk ending on today,
/// not the last day of the month.
///
/// Returns an empty vector when `now_utc` falls before Jan 1 of its
/// own year (structurally impossible but defended for completeness)
/// or when calendar arithmetic underflows. Output ordering is
/// chronological: chunk 0 is January; chunk N is the current month.
///
/// Example: `now_utc = 2026-05-31` produces 5 chunks:
///   (2026-01-01, 2026-01-31), (2026-02-01, 2026-02-28),
///   (2026-03-01, 2026-03-31), (2026-04-01, 2026-04-30),
///   (2026-05-01, 2026-05-31).
///
/// Example mid-month: `now_utc = 2026-03-15` produces 3 chunks ending
/// with `(2026-03-01, 2026-03-15)` — the final chunk does not run
/// past today.
///
/// `pub` because the unit tests below pin the chunker independently
/// of the daemon's wiring; the bootstrap runner is the only
/// production consumer.
pub fn year_to_date_month_chunks(now_utc: OffsetDateTime) -> Result<Vec<(String, String)>> {
    let today = now_utc.date();
    let year = today.year();
    let mut chunks = Vec::new();
    // Iterate months 1..=today.month(); the last chunk's end is
    // clamped to `today` rather than the month's last day.
    let mut month_num: u8 = 1;
    let today_month: u8 = today.month() as u8;
    while month_num <= today_month {
        let month = time::Month::try_from(month_num)
            .map_err(|e| anyhow!("invalid month number {month_num}: {e}"))?;
        let first = time::Date::from_calendar_date(year, month, 1)
            .map_err(|e| anyhow!("YYYY-MM-01 calendar build failed: {e}"))?;
        // Last day of this calendar month: compute the first day of
        // the next month, subtract one day. December (12) wraps to
        // January of year+1 — handled via the `checked_add` ladder so
        // a future Date::MAX-edge contributor sees the loud-fail.
        let last_of_month = if month_num == 12 {
            time::Date::from_calendar_date(year, time::Month::December, 31)
                .map_err(|e| anyhow!("Dec-31 calendar build failed: {e}"))?
        } else {
            let next_month = time::Month::try_from(month_num + 1)
                .map_err(|e| anyhow!("invalid next-month number: {e}"))?;
            time::Date::from_calendar_date(year, next_month, 1)
                .map_err(|e| anyhow!("next-month-01 calendar build failed: {e}"))?
                .checked_sub(time::Duration::days(1))
                .ok_or_else(|| anyhow!("date underflow clamping month end"))?
        };
        // Final-chunk clamp: don't sweep beyond today.
        let chunk_end = if last_of_month > today {
            today
        } else {
            last_of_month
        };
        chunks.push((first.format(&ISO_DATE)?, chunk_end.format(&ISO_DATE)?));
        month_num += 1;
    }
    Ok(chunks)
}

/// PR-203 / S203 — read every audit-ledger entry; return `true` iff at
/// least one `IncomingInvoiceSyncCycleCompleted` payload's `trigger`
/// field equals `"bootstrap-year"`. The bootstrap pass is one-shot
/// detected through THIS audit sentinel (not a file marker) so a
/// dev-DB nuke / restore re-runs the bootstrap automatically — which
/// is the operator-correct behaviour (the local DuckDB is the
/// authoritative AP mirror and a fresh DB has nothing to mirror).
///
/// Returns `Err` only on ledger read / payload decode failure. A
/// pre-PR-203 audit trail (no bootstrap-year rows by definition)
/// returns `Ok(false)` so the bootstrap fires once on first launch
/// after this PR lands.
fn bootstrap_year_already_recorded(
    db_path: &std::path::Path,
    tenant: TenantId,
    binary_hash: BinaryHash,
) -> Result<bool> {
    let ledger = Ledger::open(db_path, tenant, binary_hash).with_context(|| {
        format!(
            "open audit ledger at {} for bootstrap-year sentinel scan",
            db_path.display()
        )
    })?;
    let entries = ledger
        .entries()
        .context("read audit-ledger entries to look for prior bootstrap-year row")?;
    for entry in entries.iter() {
        if entry.kind == EventKind::IncomingInvoiceSyncCycleCompleted {
            // PR-203 / S203 — decode the payload only enough to read
            // the `trigger` token. Tolerate decode failure on this
            // narrow read by treating as "not bootstrap" so a malformed
            // legacy row does not block the sweep.
            if let Ok(payload) =
                serde_json::from_slice::<IncomingInvoiceSyncCycleCompletedPayload>(&entry.payload)
            {
                if payload.trigger == CycleTrigger::BootstrapYear.as_audit_str() {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

/// PR-203 / S203 — one-shot year-to-date bootstrap sweep. Walks
/// [`year_to_date_month_chunks`] in chronological order, runs
/// [`run_one_cycle_for_window`] per chunk with
/// [`CycleTrigger::BootstrapYear`], sleeps
/// [`BOOTSTRAP_CHUNK_THROTTLE_SECS`] between chunks. Per-chunk
/// failures are logged and the sweep continues (a single bad month
/// must not block the rest of the year). The audit-row count
/// downstream consumers see is one per chunk attempted, not just
/// per success.
///
/// Conservative choice flag: **bootstrap-only**. The brief's
/// alternative was "always iterate the full year on every tick" which
/// would amortise the cost across many cycles but multiply NAV traffic
/// by ~12. Bootstrap is state-dependent (relies on the audit-ledger
/// sentinel above) but cheaper at steady state. Documented in the PR
/// body so the operator can request a re-run via a future
/// `bootstrap-sync-now` route if a wipe-and-restore loses the
/// sentinel — which is also the design's natural recovery posture
/// (the sentinel scan returns `false` after wipe → bootstrap re-runs).
async fn run_bootstrap_year_once(build_inputs: &(dyn Fn() -> Result<CycleInputs> + Send + Sync)) {
    let inputs = match build_inputs() {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(
                error = %format!("{e:#}"),
                "AP bootstrap-year skipped (build_inputs failed); next daemon tick will retry"
            );
            return;
        }
    };

    // Sentinel check — already done? Skip silently.
    match bootstrap_year_already_recorded(
        &inputs.db_path,
        inputs.tenant.clone(),
        inputs.binary_hash,
    ) {
        Ok(true) => {
            tracing::info!("AP bootstrap-year sweep already recorded in audit ledger; skipping");
            return;
        }
        Ok(false) => {}
        Err(e) => {
            tracing::warn!(
                error = %format!("{e:#}"),
                "AP bootstrap-year sentinel scan failed; running bootstrap as defence in depth \
                 (idempotent at the digest UNIQUE so re-running is safe)"
            );
        }
    }

    let chunks = match year_to_date_month_chunks(OffsetDateTime::now_utc()) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                error = %format!("{e:#}"),
                "AP bootstrap-year chunk computation failed; skipping bootstrap"
            );
            return;
        }
    };
    let chunk_count = chunks.len();
    tracing::info!(
        chunk_count,
        "AP bootstrap-year sweep starting; one cycle per calendar month chunk"
    );

    for (idx, (date_from, date_to)) in chunks.into_iter().enumerate() {
        // Rebuild inputs per chunk so a mid-sweep NAV-credentials
        // rotation is picked up (mirrors the daemon's per-tick rebuild
        // posture).
        let chunk_inputs = match build_inputs() {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!(
                    error = %format!("{e:#}"),
                    chunk_index = idx,
                    "AP bootstrap-year chunk skipped (build_inputs failed)"
                );
                continue;
            }
        };
        match run_one_cycle_for_window(
            chunk_inputs,
            CycleTrigger::BootstrapYear,
            date_from.clone(),
            date_to.clone(),
        )
        .await
        {
            Ok(summary) => tracing::info!(
                chunk_index = idx,
                date_from = %date_from,
                date_to = %date_to,
                ingested = summary.ingested_count,
                skipped = summary.skipped_count,
                "AP bootstrap-year chunk complete"
            ),
            Err(e) => tracing::warn!(
                chunk_index = idx,
                date_from = %date_from,
                date_to = %date_to,
                error = %format!("{e:#}"),
                "AP bootstrap-year chunk failed; sweep continues with next month"
            ),
        }
        if idx + 1 < chunk_count {
            tokio::time::sleep(Duration::from_secs(BOOTSTRAP_CHUNK_THROTTLE_SECS)).await;
        }
    }
    tracing::info!("AP bootstrap-year sweep complete");
    let _ = inputs; // silence the unused-binding lint on the first build_inputs call.
}

/// S191 — owned-arg variant called from inside `spawn_blocking`. The
/// pre-S191 `write_cycle_audit_entry(&CycleInputs, &CycleSummary)`
/// borrowed `inputs`, which the move-closure boundary forbids;
/// splitting the owned fields out keeps the move ergonomics clean
/// without a wrapping `Arc<CycleInputs>` clone.
fn write_cycle_audit_entry_inner(
    db_path: &std::path::Path,
    tenant: TenantId,
    binary_hash: BinaryHash,
    operator_login: &str,
    summary: &CycleSummary,
) -> Result<()> {
    let payload = IncomingInvoiceSyncCycleCompletedPayload {
        idempotency_key: IdempotencyKey::new().to_canonical_string(),
        trigger: summary.trigger.as_audit_str().to_string(),
        date_from: summary.date_from.clone(),
        date_to: summary.date_to.clone(),
        ingested_count: summary.ingested_count,
        skipped_count: summary.skipped_count,
        pages_walked: summary.pages_walked,
        elapsed_ms: summary.elapsed_ms,
        error: summary.error.clone(),
    };
    let session_id = Ulid::new().to_string();
    let actor = Actor::from_local_cli(session_id, operator_login);
    let ledger_meta = audit_ledger::LedgerMeta::new(tenant.clone(), binary_hash);

    let mut conn = duckdb::Connection::open(db_path).with_context(|| {
        format!(
            "open tenant DuckDB at {} for AP sync cycle audit entry",
            db_path.display()
        )
    })?;
    audit_ledger::ensure_schema(&conn)
        .context("ensure audit-ledger schema for AP sync cycle audit entry")?;
    let tx = conn
        .transaction()
        .context("begin DuckDB transaction (AP sync cycle audit entry)")?;
    audit_ledger::append_in_tx(
        &tx,
        &ledger_meta,
        EventKind::IncomingInvoiceSyncCycleCompleted,
        payload.to_bytes(),
        actor,
        Some(payload.idempotency_key.clone()),
    )
    .map_err(|e| anyhow!("audit_ledger::append_in_tx IncomingInvoiceSyncCycleCompleted: {e}"))?;
    tx.commit()
        .context("commit DuckDB transaction (AP sync cycle audit entry)")?;
    drop(conn);

    let ledger = Ledger::open(db_path, tenant, binary_hash)
        .context("open audit ledger to verify chain after AP sync cycle entry")?;
    ledger
        .verify_chain()
        .context("audit-ledger chain verification failed AFTER AP sync cycle entry")?;
    let mirror_path = audit_ledger::mirror_path_for(db_path);
    ledger
        .sync_mirror(&mirror_path)
        .context("sync audit-ledger mirror file after AP sync cycle entry")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn fixture_digest_huf() -> InvoiceDigest {
        InvoiceDigest {
            invoice_number: "SUP-2026/0001".to_string(),
            supplier_tax_number: "12345678".to_string(),
            supplier_name: Some("Példa Kft.".to_string()),
            issue_date: Some("2026-05-10".to_string()),
            transaction_id: Some("TXN-001".to_string()),
            currency: Some("HUF".to_string()),
            invoice_net_amount: Some("100000.00".to_string()),
            invoice_vat_amount: Some("27000.00".to_string()),
        }
    }

    fn fixture_digest_eur() -> InvoiceDigest {
        InvoiceDigest {
            invoice_number: "SUP-EU-001".to_string(),
            supplier_tax_number: "87654321".to_string(),
            supplier_name: Some("Other GmbH".to_string()),
            issue_date: Some("2026-05-11".to_string()),
            transaction_id: Some("TXN-002".to_string()),
            currency: Some("EUR".to_string()),
            invoice_net_amount: Some("50.00".to_string()),
            invoice_vat_amount: Some("13.50".to_string()),
        }
    }

    #[test]
    fn digest_to_ingestion_input_handles_huf() {
        let input = digest_to_ingestion_input(&fixture_digest_huf()).expect("HUF digest");
        assert_eq!(input.currency, "HUF");
        assert_eq!(input.total_net_minor, 100_000);
        assert_eq!(input.total_vat_minor, 27_000);
        assert_eq!(input.total_gross_minor, 127_000);
        assert_eq!(input.supplier_name, "Példa Kft.");
        assert_eq!(input.nav_invoice_number, "SUP-2026/0001");
        assert!(input.nav_xml.is_none());
    }

    #[test]
    fn digest_to_ingestion_input_handles_eur_scales_to_cents() {
        let input = digest_to_ingestion_input(&fixture_digest_eur()).expect("EUR digest");
        assert_eq!(input.currency, "EUR");
        // 50.00 EUR -> 5000 cents; 13.50 EUR -> 1350 cents.
        assert_eq!(input.total_net_minor, 5_000);
        assert_eq!(input.total_vat_minor, 1_350);
        assert_eq!(input.total_gross_minor, 6_350);
    }

    #[test]
    fn digest_to_ingestion_input_rejects_unknown_currency() {
        let mut d = fixture_digest_huf();
        d.currency = Some("USD".to_string());
        let err = digest_to_ingestion_input(&d).expect_err("USD outside closed vocab");
        assert!(format!("{err:#}").contains("USD"), "{err:#}");
    }

    #[test]
    fn digest_to_ingestion_input_rejects_missing_currency() {
        let mut d = fixture_digest_huf();
        d.currency = None;
        let err = digest_to_ingestion_input(&d).expect_err("missing currency");
        assert!(format!("{err:#}").contains("missing <currency>"));
    }

    #[test]
    fn digest_to_ingestion_input_rejects_missing_issue_date() {
        let mut d = fixture_digest_huf();
        d.issue_date = None;
        let err = digest_to_ingestion_input(&d).expect_err("missing issue_date");
        assert!(format!("{err:#}").contains("invoiceIssueDate"));
    }

    #[test]
    fn digest_to_ingestion_input_rejects_missing_supplier_name() {
        let mut d = fixture_digest_huf();
        d.supplier_name = None;
        let err = digest_to_ingestion_input(&d).expect_err("missing supplier_name");
        assert!(format!("{err:#}").contains("supplierName"));
    }

    #[test]
    fn digest_to_ingestion_input_treats_absent_amounts_as_zero() {
        let mut d = fixture_digest_huf();
        d.invoice_net_amount = None;
        d.invoice_vat_amount = None;
        let input = digest_to_ingestion_input(&d).expect("zero amounts ok");
        assert_eq!(input.total_net_minor, 0);
        assert_eq!(input.total_vat_minor, 0);
        assert_eq!(input.total_gross_minor, 0);
    }

    #[test]
    fn decimal_to_minor_rounds_half_even_for_eur() {
        // Decimal::round defaults to half-even (banker's rounding).
        assert_eq!(decimal_to_minor("12.34", "EUR").unwrap(), 1234);
        assert_eq!(decimal_to_minor("12.345", "EUR").unwrap(), 1234);
        assert_eq!(decimal_to_minor("12.355", "EUR").unwrap(), 1236);
    }

    #[test]
    fn decimal_to_minor_truncates_decimals_for_huf() {
        // HUF has 0 decimal scale; fractional inputs round to whole forints.
        assert_eq!(decimal_to_minor("100", "HUF").unwrap(), 100);
        assert_eq!(decimal_to_minor("100.49", "HUF").unwrap(), 100);
        assert_eq!(decimal_to_minor("100.50", "HUF").unwrap(), 100); // half-even
        assert_eq!(decimal_to_minor("101.50", "HUF").unwrap(), 102); // half-even
    }

    #[test]
    fn decimal_to_minor_loud_fails_on_malformed_input() {
        let err = decimal_to_minor("not-a-number", "HUF").expect_err("must loud-fail");
        assert!(format!("{err:#}").contains("not a valid Decimal"));
    }

    #[test]
    fn compute_date_window_is_thirty_days_back() {
        let now = datetime!(2026-05-30 12:00:00 UTC);
        let (from, to) = compute_date_window(now).unwrap();
        assert_eq!(to, "2026-05-30");
        assert_eq!(from, "2026-04-30");
    }

    /// S192 — `checked_sub` underflow surfaces as a typed loud-fail.
    /// PR-182 review's S178 🟢 flagged the `?` on
    /// `today.checked_sub(time::Duration::days(WINDOW_DAYS))` as a
    /// possible error path unreachable in practice but undocumented.
    /// At `time::Date::MIN`, subtracting 30 days underflows the
    /// representable date range, so the helper MUST surface the
    /// `"date underflow computing AP sync window"` anyhow error
    /// rather than silently clamping or panicking.
    ///
    /// CLAUDE.md rule 12 — loud-fail on unreachable-in-practice paths
    /// is the load-bearing contract: a future calendar-math refactor
    /// that swaps `checked_sub` for plain `-` would panic, and pinning
    /// the typed-error path forces the regressor to look at this test.
    #[test]
    fn compute_date_window_loud_fails_on_underflow_at_date_min() {
        // Build an OffsetDateTime whose `.date()` is exactly Date::MIN
        // (the lower bound of the `time` crate's representable range).
        // The 30-day subtraction is guaranteed to underflow.
        let now =
            time::PrimitiveDateTime::new(time::Date::MIN, time::Time::from_hms(0, 0, 0).unwrap())
                .assume_utc();
        let err = compute_date_window(now).expect_err("Date::MIN - 30 days must underflow");
        assert!(
            format!("{err:#}").contains("date underflow"),
            "underflow must surface as the documented loud-fail message; got: {err:#}"
        );
    }

    #[test]
    fn cycle_trigger_audit_strings_are_closed_vocab() {
        assert_eq!(CycleTrigger::Daemon.as_audit_str(), "daemon");
        assert_eq!(CycleTrigger::Manual.as_audit_str(), "manual");
        // PR-203 / S203 — closed-vocab triple. Adding a fourth variant
        // means updating: the SPA audit-row renderer (the SPA renders
        // the bare token today since no per-row label exists yet) AND
        // `bootstrap_year_already_recorded`'s string comparison.
        assert_eq!(CycleTrigger::BootstrapYear.as_audit_str(), "bootstrap-year");
    }

    /// PR-203 / S203 — end-of-May bootstrap. With `now_utc = 2026-05-31`
    /// the chunker MUST return exactly 5 chunks covering Jan, Feb, Mar,
    /// Apr, and May 1 .. May 31. Pins the `(date_from, date_to)` strings
    /// against the canonical YYYY-MM-DD format so a future format-string
    /// drift would surface here.
    #[test]
    fn year_to_date_month_chunks_for_end_of_may_returns_five() {
        let now = datetime!(2026-05-31 12:00:00 UTC);
        let chunks = year_to_date_month_chunks(now).expect("chunks ok");
        assert_eq!(
            chunks,
            vec![
                ("2026-01-01".to_string(), "2026-01-31".to_string()),
                ("2026-02-01".to_string(), "2026-02-28".to_string()),
                ("2026-03-01".to_string(), "2026-03-31".to_string()),
                ("2026-04-01".to_string(), "2026-04-30".to_string()),
                ("2026-05-01".to_string(), "2026-05-31".to_string()),
            ],
            "year-to-date chunks for 2026-05-31 must cover Jan..May in calendar-month rows"
        );
    }

    /// PR-203 / S203 — mid-month clamp. With `now_utc = 2026-03-15` the
    /// final chunk must end on 2026-03-15, NOT 2026-03-31. The clamp is
    /// the invariant that lets the bootstrap run mid-day without
    /// querying digests dated after `now`.
    #[test]
    fn year_to_date_month_chunks_clamps_final_chunk_to_today() {
        let now = datetime!(2026-03-15 09:00:00 UTC);
        let chunks = year_to_date_month_chunks(now).expect("chunks ok");
        assert_eq!(chunks.len(), 3, "Jan, Feb, Mar (clamped)");
        assert_eq!(
            chunks.last(),
            Some(&("2026-03-01".to_string(), "2026-03-15".to_string())),
            "final chunk must clamp to today, not month-end"
        );
    }

    /// PR-203 / S203 — January-only edge. Running the bootstrap on
    /// 2026-01-05 must yield exactly one chunk `(Jan 1, Jan 5)` rather
    /// than zero chunks (which would silently no-op the bootstrap on
    /// fresh January DBs) or a confused Jan 1..31 (which would sweep
    /// dates that haven't happened yet).
    #[test]
    fn year_to_date_month_chunks_january_only() {
        let now = datetime!(2026-01-05 23:59:59 UTC);
        let chunks = year_to_date_month_chunks(now).expect("chunks ok");
        assert_eq!(
            chunks,
            vec![("2026-01-01".to_string(), "2026-01-05".to_string())],
            "January-only bootstrap must produce exactly one clamped chunk"
        );
    }

    /// PR-203 / S203 — December rollover. Running the bootstrap on
    /// 2026-12-31 must yield exactly 12 chunks ending with
    /// `(Dec 1, Dec 31)`. Pins the `month_num == 12` branch which
    /// would otherwise underflow the "first day of next month minus
    /// one" pattern.
    #[test]
    fn year_to_date_month_chunks_full_year_dec_31() {
        let now = datetime!(2026-12-31 12:00:00 UTC);
        let chunks = year_to_date_month_chunks(now).expect("chunks ok");
        assert_eq!(chunks.len(), 12, "full-year sweep");
        assert_eq!(
            chunks.last(),
            Some(&("2026-12-01".to_string(), "2026-12-31".to_string())),
            "December branch must end on Dec 31, not roll into next year"
        );
        assert_eq!(
            chunks.first(),
            Some(&("2026-01-01".to_string(), "2026-01-31".to_string())),
            "January chunk must start on Jan 1"
        );
    }

    /// PR-203 / S203 — leap-day handling. 2024 had Feb 29; the chunker
    /// must produce `(Feb 1, Feb 29)` for a 2024 sweep, not `Feb 28`.
    /// Pins the `time::Date::from_calendar_date(next_month, 1) - 1 day`
    /// pattern that's the canonical "last day of this month" idiom.
    #[test]
    fn year_to_date_month_chunks_handles_leap_day_february() {
        let now = datetime!(2024-03-15 00:00:00 UTC);
        let chunks = year_to_date_month_chunks(now).expect("chunks ok");
        // chunks[1] is February in a 0-indexed list.
        assert_eq!(
            chunks[1],
            ("2024-02-01".to_string(), "2024-02-29".to_string()),
            "Feb 2024 leap-year chunk must end on Feb 29"
        );
    }

    /// S197 — `persist_xml_for_row` happy path: decodes the NAV
    /// envelope's `<invoiceData>` base64 blob, writes the inner bytes
    /// to `<artifacts_dir>/<ap_invoice_id>.xml`, and UPDATEs the row's
    /// `nav_xml_path` column. Defends the extract → write → UPDATE
    /// pipeline against a future refactor that splits any leg of it
    /// from the others.
    #[test]
    fn persist_xml_for_row_writes_file_and_updates_column() {
        use base64::Engine;
        use incoming_invoices::{IngestOutcome, IngestionInput};

        let tmp = std::env::temp_dir().join(format!(
            "aberp-s197-persist-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let db_path = tmp.join("tenant.duckdb");
        let artifacts_dir = tmp.join("ap-artifacts");

        let tenant = aberp_audit_ledger::TenantId::new("t1".to_string()).expect("fixture tenant");
        let binary_hash = aberp_audit_ledger::BinaryHash::from_bytes([0u8; 32]);
        let input = IngestionInput {
            supplier_tax_number: "12345678".into(),
            supplier_name: "Supplier Kft.".into(),
            supplier_address: None,
            nav_invoice_number: "SUP-2026/000001".into(),
            issue_date: "2026-05-30".into(),
            delivery_date: None,
            payment_deadline: None,
            total_net_minor: 100_000,
            total_vat_minor: 27_000,
            total_gross_minor: 127_000,
            currency: "HUF".into(),
            nav_xml: None,
        };
        let outcome = incoming_invoices::ingest_incoming_invoice(
            &db_path,
            tenant.clone(),
            binary_hash,
            "operator",
            &artifacts_dir,
            input,
        )
        .expect("fixture ingest");
        let id = match outcome {
            IngestOutcome::Created { id } => id,
            other => panic!("expected Created, got {other:?}"),
        };
        // Pre-condition — fresh ingest has no XML path.
        assert_eq!(
            incoming_invoices::get_nav_xml_path(&db_path, tenant.as_str(), &id).unwrap(),
            None
        );

        // Build a NAV-envelope fixture carrying a base64'd inner blob —
        // same shape S196's restore extract exercises.
        let inner = b"<InvoiceData><supplierInfo/><customerInfo/></InvoiceData>";
        let b64 = base64::engine::general_purpose::STANDARD.encode(inner);
        let response_xml = format!(
            "<QueryInvoiceDataResponse><invoiceDataResult><invoiceData>{b64}</invoiceData></invoiceDataResult></QueryInvoiceDataResponse>"
        );

        persist_xml_for_row(
            response_xml.as_bytes(),
            &db_path,
            tenant.as_str(),
            &artifacts_dir,
            &id,
            "SUP-2026/000001",
        )
        .expect("persist must succeed");

        // The on-disk artifact carries the decoded inner bytes.
        let file_path = artifacts_dir.join(format!("{}.xml", id));
        let bytes = std::fs::read(&file_path).expect("artifact must exist");
        assert_eq!(bytes, inner);

        // The row's nav_xml_path now points at the file.
        let path = incoming_invoices::get_nav_xml_path(&db_path, tenant.as_str(), &id)
            .unwrap()
            .expect("nav_xml_path must be populated");
        assert_eq!(path, file_path.to_string_lossy());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// PR-215 / S217 — RENAMED + FLIPPED from S197's
    /// `persist_xml_for_row_loud_fails_on_missing_invoice_data_element`.
    /// Per the NAV OSA 3.0 XSD `invoiceDataResult` is `minOccurs=0` and
    /// NAV legitimately returns `funcCode=OK` without `<invoiceData>`
    /// whenever the supplier has not exposed XML to the buyer. That is
    /// the EXACT shape Ervin observed on 13/13 INBOUND rows in the
    /// 2026-06-01 prod cycle — they are NOT failures and must NOT
    /// loud-fail. The S197 test was contract-wrong; this is the
    /// replacement.
    ///
    /// Pinned shape — `<invoiceData>` absent, anything else present:
    ///   - persist_xml_for_row returns `Ok(())`
    ///   - `nav_xml_path` stays NULL (no UPDATE)
    ///   - NO `.failed/` capture is written (would mis-signal a real
    ///     failure to the next-session operator)
    #[test]
    fn persist_xml_for_row_treats_absent_invoice_data_as_no_op_per_pr215() {
        let tmp = std::env::temp_dir().join(format!(
            "aberp-pr215-no-op-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let db_path = tmp.join("tenant.duckdb");
        let artifacts_dir = tmp.join("ap-artifacts");

        // No row ingest needed — the `Ok(None)` branch returns BEFORE
        // touching the UPDATE leg, so the row's absence is irrelevant.
        let response_xml = b"<QueryInvoiceDataResponse>\
            <result><funcCode>OK</funcCode></result>\
        </QueryInvoiceDataResponse>";
        persist_xml_for_row(
            response_xml,
            &db_path,
            "t1",
            &artifacts_dir,
            "apinv_01HRQXYZABCDEFGHJKMNPQRST",
            "SUP-2026/000001",
        )
        .expect("absent <invoiceData> must NOT loud-fail per PR-215");

        let capture_path = artifacts_dir
            .join(".failed")
            .join("apinv_01HRQXYZABCDEFGHJKMNPQRST.xml");
        assert!(
            !capture_path.exists(),
            "absent <invoiceData> is expected NAV behavior; .failed/ MUST stay empty \
             so a real failure stands out; found {}",
            capture_path.display()
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// PR-215 / S217 — same as the prior test but with the
    /// `<invoiceDataResult/>` self-closing wrapper present and no
    /// `<invoiceData>` inside. NAV's published XSD allows
    /// `invoiceDataResult` to be present-but-empty in principle (the
    /// real wire shape we've observed omits it entirely; this guards
    /// against a future NAV-side shape change adding the wrapper while
    /// still withholding the data). Same contract: `Ok(())`, no
    /// `.failed/` capture.
    #[test]
    fn persist_xml_for_row_treats_empty_invoice_data_result_as_no_op_per_pr215() {
        let tmp = std::env::temp_dir().join(format!(
            "aberp-pr215-empty-result-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let db_path = tmp.join("tenant.duckdb");
        let artifacts_dir = tmp.join("ap-artifacts");
        let id = "apinv_01HRQXYZABCDEFGHJKMNPQRST";

        let response_xml = b"<QueryInvoiceDataResponse>\
            <result><funcCode>OK</funcCode></result>\
            <invoiceDataResult/>\
        </QueryInvoiceDataResponse>";
        persist_xml_for_row(
            response_xml,
            &db_path,
            "t1",
            &artifacts_dir,
            id,
            "SUP-2026/000001",
        )
        .expect("empty <invoiceDataResult/> must NOT loud-fail per PR-215");

        let capture_path = artifacts_dir.join(".failed").join(format!("{}.xml", id));
        assert!(
            !capture_path.exists(),
            "empty <invoiceDataResult/> is expected NAV behavior; .failed/ must stay empty"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// PR-215 / S217 — golden fixture for NAV response Shape A
    /// (`xmlns="…/NTCA/…/common", xmlns:ns2="…/OSA/3.0/api"`). Redacted
    /// from a real 2026-06-01 prod fixture — supplier-side request and
    /// timestamps preserved verbatim because they're already public NAV
    /// transaction surface, but everything that could carry PII is
    /// constant. Pins the parser tolerance against the majority shape
    /// (10/13 of the prod cycle).
    #[test]
    fn persist_xml_for_row_tolerates_shape_a_ntca_default_ns_per_pr215() {
        let tmp = std::env::temp_dir().join(format!(
            "aberp-pr215-shape-a-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let db_path = tmp.join("tenant.duckdb");
        let artifacts_dir = tmp.join("ap-artifacts");
        let id = "apinv_01HRQXYZABCDEFGHJKMNPQRST";

        // Verbatim 2026-06-01 prod Shape A response, with the requestId
        // and timestamp masked. Note: the response is funcCode=OK with
        // NO <invoiceDataResult> and NO <invoiceData>. Real NAV bytes.
        let response_xml = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><ns2:QueryInvoiceDataResponse xmlns="http://schemas.nav.gov.hu/NTCA/1.0/common" xmlns:ns2="http://schemas.nav.gov.hu/OSA/3.0/api" xmlns:ns3="http://schemas.nav.gov.hu/OSA/3.0/base" xmlns:ns4="http://schemas.nav.gov.hu/OSA/3.0/data"><header><requestId>REQ00000000000000000000000000</requestId><timestamp>2026-06-01T00:00:00Z</timestamp><requestVersion>3.0</requestVersion><headerVersion>1.0</headerVersion></header><result><funcCode>OK</funcCode></result><ns2:software><ns2:softwareId>ABERP-000000000001</ns2:softwareId><ns2:softwareName>ABERP</ns2:softwareName><ns2:softwareOperation>LOCAL_SOFTWARE</ns2:softwareOperation><ns2:softwareMainVersion>0.0.0</ns2:softwareMainVersion><ns2:softwareDevName>Ervin Aben</ns2:softwareDevName><ns2:softwareDevContact>ervin@aben.ch</ns2:softwareDevContact></ns2:software></ns2:QueryInvoiceDataResponse>"#;

        persist_xml_for_row(
            response_xml,
            &db_path,
            "t1",
            &artifacts_dir,
            id,
            "SUP-2026/000001",
        )
        .expect("Shape A (NTCA-default) with no <invoiceData> must NOT loud-fail per PR-215");

        let capture_path = artifacts_dir.join(".failed").join(format!("{}.xml", id));
        assert!(
            !capture_path.exists(),
            "no .failed/ capture for Shape A no-data response"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// PR-215 / S217 — golden fixture for NAV response Shape B
    /// (`xmlns="…/OSA/3.0/api", xmlns:ns2="…/NTCA/…/common"`). The
    /// SAME 2026-06-01 prod cycle returned this shape on the minority
    /// of rows (3/13). NAV swaps which namespace URI is bound to the
    /// default vs the `ns2` prefix per response — perfectly legal XML.
    /// Pins that the namespace-blind `find_first_text` tolerates both
    /// shapes uniformly. (Brief hypothesised the parser was prefix-
    /// matching `ns2:invoiceData` literally; it was always
    /// namespace-blind via local_name_matches — the brief
    /// misdiagnosed; see PR body.)
    #[test]
    fn persist_xml_for_row_tolerates_shape_b_osa_api_default_ns_per_pr215() {
        let tmp = std::env::temp_dir().join(format!(
            "aberp-pr215-shape-b-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let db_path = tmp.join("tenant.duckdb");
        let artifacts_dir = tmp.join("ap-artifacts");
        let id = "apinv_01HRQXYZABCDEFGHJKMNPQRST";

        // Verbatim 2026-06-01 prod Shape B response, requestId +
        // timestamp masked. Default ns is OSA/api this time; the NTCA
        // namespace gets the ns2 prefix. Header + result + software
        // ALL carry the `ns2:` prefix here (whereas Shape A bound the
        // prefix to OSA so they were unprefixed there).
        let response_xml = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><QueryInvoiceDataResponse xmlns="http://schemas.nav.gov.hu/OSA/3.0/api" xmlns:ns2="http://schemas.nav.gov.hu/NTCA/1.0/common" xmlns:ns3="http://schemas.nav.gov.hu/OSA/3.0/base" xmlns:ns4="http://schemas.nav.gov.hu/OSA/3.0/data"><ns2:header><ns2:requestId>REQ00000000000000000000000000</ns2:requestId><ns2:timestamp>2026-06-01T00:00:00Z</ns2:timestamp><ns2:requestVersion>3.0</ns2:requestVersion><ns2:headerVersion>1.0</ns2:headerVersion></ns2:header><ns2:result><ns2:funcCode>OK</ns2:funcCode></ns2:result><software><softwareId>ABERP-000000000001</softwareId><softwareName>ABERP</softwareName><softwareOperation>LOCAL_SOFTWARE</softwareOperation><softwareMainVersion>0.0.0</softwareMainVersion><softwareDevName>Ervin Aben</softwareDevName><softwareDevContact>ervin@aben.ch</softwareDevContact></software></QueryInvoiceDataResponse>"#;

        persist_xml_for_row(
            response_xml,
            &db_path,
            "t1",
            &artifacts_dir,
            id,
            "SUP-2026/000001",
        )
        .expect("Shape B (OSA/api-default) with no <invoiceData> must NOT loud-fail per PR-215");

        let capture_path = artifacts_dir.join(".failed").join(format!("{}.xml", id));
        assert!(
            !capture_path.exists(),
            "no .failed/ capture for Shape B no-data response"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// PR-214 / S216 (REFINED by PR-215) — `.failed/` diagnostic
    /// capture survives, but the trigger surface is narrower now. Only
    /// GENUINELY malformed `<invoiceData>` payloads (present but
    /// non-base64 / empty text / structurally malformed surrounding
    /// XML) still capture. Absent `<invoiceData>` does NOT capture
    /// post-PR-215 (covered by the `treats_absent_..._as_no_op` tests
    /// above).
    ///
    /// This fixture has `<invoiceData>` PRESENT but carrying garbage
    /// (not valid base64), so the parser's loud-fail leg fires and the
    /// capture lands.
    #[test]
    fn persist_xml_for_row_writes_diagnostic_capture_on_genuine_malformed_invoice_data() {
        let tmp = std::env::temp_dir().join(format!(
            "aberp-pr215-capture-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let db_path = tmp.join("tenant.duckdb");
        let artifacts_dir = tmp.join("ap-artifacts");
        let id = "apinv_01HRQXYZABCDEFGHJKMNPQRST";
        // <invoiceData> PRESENT but carries clearly-invalid base64
        // characters (`!!!` is not in the base64 alphabet). Per the
        // PR-215 contract this stays a loud-fail and captures.
        let response_xml = b"<QueryInvoiceDataResponse>\
            <result><funcCode>OK</funcCode></result>\
            <invoiceDataResult><invoiceData>!!!not-base64!!!</invoiceData></invoiceDataResult>\
        </QueryInvoiceDataResponse>";
        let err = persist_xml_for_row(
            response_xml,
            &db_path,
            "t1",
            &artifacts_dir,
            id,
            "SUP-2026/000001",
        )
        .expect_err("genuinely malformed <invoiceData> must loud-fail");
        assert!(
            format!("{err:#}").contains("base64-decode <invoiceData>"),
            "loud-fail must name the base64 decode failure; got: {err:#}"
        );
        let capture_path = artifacts_dir.join(".failed").join(format!("{}.xml", id));
        let captured = std::fs::read(&capture_path).expect("capture file must exist");
        assert_eq!(
            captured, response_xml,
            "capture must carry the original response bytes verbatim"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// PR-214 / S216 — `extract_inner_invoice_data_xml` success path
    /// must NOT trigger diagnostic capture. Pins that a healthy
    /// response keeps the `.failed/` directory absent (the operator
    /// would mis-read its existence as "the daemon hit the failure
    /// mode again" otherwise).
    #[test]
    fn persist_xml_for_row_does_not_capture_on_success() {
        use base64::Engine;
        use incoming_invoices::IngestOutcome;

        let tmp = std::env::temp_dir().join(format!(
            "aberp-s216-no-capture-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let db_path = tmp.join("tenant.duckdb");
        let artifacts_dir = tmp.join("ap-artifacts");

        let tenant = aberp_audit_ledger::TenantId::new("t1".to_string()).expect("fixture tenant");
        let binary_hash = aberp_audit_ledger::BinaryHash::from_bytes([0u8; 32]);
        let input = IngestionInput {
            supplier_tax_number: "12345678".into(),
            supplier_name: "Supplier Kft.".into(),
            supplier_address: None,
            nav_invoice_number: "SUP-2026/000001".into(),
            issue_date: "2026-05-30".into(),
            delivery_date: None,
            payment_deadline: None,
            total_net_minor: 100_000,
            total_vat_minor: 27_000,
            total_gross_minor: 127_000,
            currency: "HUF".into(),
            nav_xml: None,
        };
        let outcome = incoming_invoices::ingest_incoming_invoice(
            &db_path,
            tenant.clone(),
            binary_hash,
            "operator",
            &artifacts_dir,
            input,
        )
        .expect("fixture ingest");
        let id = match outcome {
            IngestOutcome::Created { id } => id,
            other => panic!("expected Created, got {other:?}"),
        };

        let inner = b"<InvoiceData><supplierInfo/></InvoiceData>";
        let b64 = base64::engine::general_purpose::STANDARD.encode(inner);
        let response_xml = format!(
            "<QueryInvoiceDataResponse><invoiceDataResult><invoiceData>{b64}</invoiceData></invoiceDataResult></QueryInvoiceDataResponse>"
        );
        persist_xml_for_row(
            response_xml.as_bytes(),
            &db_path,
            tenant.as_str(),
            &artifacts_dir,
            &id,
            "SUP-2026/000001",
        )
        .expect("persist must succeed");

        let capture_path = artifacts_dir.join(".failed").join(format!("{}.xml", id));
        assert!(
            !capture_path.exists(),
            "success path must NOT create a .failed/ capture; found {}",
            capture_path.display()
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// PR-214 / S216 — full HU community tax number (`NNNNNNNN-N-NN`)
    /// is redacted with the `[REDACTED-TAX]` marker; a bare 8-digit
    /// taxpayer ID with no continuation is redacted with
    /// `[REDACTED-ID]`. Surrounding XML structure and supplier names
    /// stay verbatim — the operator needs them to correlate the
    /// preview with the observed failure.
    #[test]
    fn redact_hu_tax_ids_strips_both_patterns_and_keeps_context() {
        let input = "<supplierTaxNumber>12345678-1-23</supplierTaxNumber>\
                     <customerName>Áben Bt.</customerName>\
                     <taxpayerId>87654321</taxpayerId>\
                     <invoiceNumber>SUP-2026/000001</invoiceNumber>";
        let got = redact_hu_tax_ids(input);
        assert!(
            got.contains("[REDACTED-TAX]"),
            "full HU tax ID must be marked as TAX; got: {got}"
        );
        assert!(
            got.contains("[REDACTED-ID]"),
            "bare 8-digit taxpayer ID must be marked as ID; got: {got}"
        );
        assert!(
            !got.contains("12345678-1-23"),
            "raw full tax ID must not leak; got: {got}"
        );
        assert!(
            !got.contains(">87654321<"),
            "raw bare taxpayer ID must not leak; got: {got}"
        );
        assert!(
            got.contains("Áben Bt."),
            "supplier name must NOT be redacted (operator needs it for correlation); got: {got}"
        );
        assert!(
            got.contains("SUP-2026/000001"),
            "invoice number must NOT be redacted (NAV-issued, not a tax ID); got: {got}"
        );
    }

    /// PR-214 / S216 — the redactor must not redact arbitrary 8-digit
    /// runs that are part of a longer numeric token (NAV invoice
    /// numbers like `100351218526` carry long digit runs). The
    /// preceding-digit check is what defends against that drift.
    #[test]
    fn redact_hu_tax_ids_skips_digit_runs_inside_longer_tokens() {
        // `100351218526` is a real Yettel-Magyarország invoice number
        // shape from the prod DB; the first 8 digits form a "100..."
        // run that must NOT be mistaken for a taxpayer ID.
        let input = "<invoiceNumber>100351218526</invoiceNumber>";
        let got = redact_hu_tax_ids(input);
        assert_eq!(
            got, input,
            "12-digit invoice number must pass through unmodified; got: {got}"
        );
    }

    /// PR-214 / S216 — preview truncation is byte-bounded with a UTF-8
    /// boundary safeguard so a multi-byte HU character (e.g., `é`,
    /// `Ü`) split across `max_bytes` does not panic the format step.
    #[test]
    fn sanitise_response_preview_truncates_on_utf8_boundary() {
        // Pad with leading ASCII so the multi-byte char straddles the
        // 10-byte cap regardless of code-point width.
        let s = "abcdefghi".to_string() + "éééééééé";
        let bytes = s.as_bytes();
        let preview = sanitise_response_preview(bytes, 10);
        // The preview must NOT contain a half-character or panic.
        // We don't pin the exact length (it depends on where the
        // boundary lands) — just that it's <= the truncation
        // threshold + the `…` marker and ends cleanly.
        assert!(
            preview.ends_with('…'),
            "truncated preview must end with the truncation marker; got: {preview}"
        );
        // The leading ASCII prefix must survive.
        assert!(
            preview.starts_with("abcdefghi"),
            "leading ASCII prefix must survive; got: {preview}"
        );
    }

    /// PR-214 / S216 — preview pass-through when the input fits within
    /// `max_bytes` — no truncation marker, no length change.
    #[test]
    fn sanitise_response_preview_passes_short_input_unchanged() {
        let input = b"<r>OK</r>";
        let preview = sanitise_response_preview(input, 500);
        assert_eq!(preview, "<r>OK</r>");
        assert!(!preview.contains('…'));
    }
}
