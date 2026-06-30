//! `QuoteIntakeService` — orchestrates one poll cycle end-to-end.
//!
//! Snapshot pending-writebacks BEFORE the per-quote loop so rows
//! inserted this cycle aren't double-attempted (brief §5 honors
//! "next cycle retries").

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use time::OffsetDateTime;
use tokio::task::spawn_blocking;
use tokio_util::sync::CancellationToken;
use ulid::Ulid;

use aberp_audit_ledger::{Actor, BinaryHash, LedgerMeta, TenantId};
use aberp_billing::Currency;

use crate::audit::{
    write_poll_audit_entry, write_poll_failed_entry, write_row_added_entry, PollFailureReason,
    PollTrigger, QuoteIntakePollFailedPayload, QuoteIntakePollPayload, QuoteIntakeRowAddedPayload,
};
use crate::config::QuoteIntakeConfig;
use crate::error::QuoteIntakeError;
use crate::log_table;
use crate::mapping::{quote_to_draft_invoice, QUOTE_PROCESSING_STATUS};
use crate::payload::Quote;
use crate::transport::QuoteIntakeTransport;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PollSummary {
    pub trigger: PollTrigger,
    pub fetched: u32,
    pub created: u32,
    pub skipped_duplicate: u32,
    pub writeback_retried: u32,
    pub writeback_failed: u32,
    /// S256 — quotes that failed mapping and were staged as `error`
    /// rows (brief §A.4) rather than silently dropped.
    pub errored: u32,
    pub failed: Vec<(String, String)>,
    pub elapsed_ms: u64,
    pub error: Option<String>,
    /// S256 — structured class of a cycle-aborting transport failure.
    /// `Some` ⇒ the cycle emitted a `QuoteIntakePollFailed` entry and
    /// the daemon loop should back off (or PAUSE on `Unauthorized`).
    pub error_reason: Option<PollFailureReason>,
}

#[derive(Debug, Clone)]
pub struct QuoteIntakeDeps {
    pub db_path: PathBuf,
    /// ADR-0098 Session B (Gap 1a) — shared DuckDB handle.
    pub db: aberp_db::HandleArc,
    pub tenant: TenantId,
    pub binary_hash: BinaryHash,
    pub operator_login: String,
    pub default_currency: Currency,
}

pub struct QuoteIntakeService {
    config: QuoteIntakeConfig,
    transport: QuoteIntakeTransport,
    deps: QuoteIntakeDeps,
}

/// Best-effort extraction of the HTTP status from an
/// `UnexpectedStatus` error's Display string ("…unexpected HTTP status
/// 500"). Returns `None` if no trailing integer is present. Used only
/// to populate the optional `status` field on `QuoteIntakePollFailed`;
/// the structured `reason` is the load-bearing field.
fn parse_status_from_detail(detail: &Option<String>) -> Option<u16> {
    detail
        .as_deref()?
        .rsplit(|c: char| !c.is_ascii_digit())
        .find(|s| !s.is_empty())
        .and_then(|s| s.parse::<u16>().ok())
}

impl std::fmt::Debug for QuoteIntakeService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuoteIntakeService")
            .field("config", &self.config)
            .field("transport", &self.transport)
            .field("deps", &self.deps)
            .finish()
    }
}

impl QuoteIntakeService {
    pub fn new(config: QuoteIntakeConfig, deps: QuoteIntakeDeps) -> Result<Self, QuoteIntakeError> {
        let transport = QuoteIntakeTransport::new(&config)?;
        Ok(Self {
            config,
            transport,
            deps,
        })
    }

    pub async fn poll_once(&self, trigger: PollTrigger) -> PollSummary {
        let started = Instant::now();
        let mut summary = PollSummary {
            trigger,
            fetched: 0,
            created: 0,
            skipped_duplicate: 0,
            writeback_retried: 0,
            writeback_failed: 0,
            errored: 0,
            failed: Vec::new(),
            elapsed_ms: 0,
            error: None,
            error_reason: None,
        };

        let quotes = match self.transport.list_approved_quotes().await {
            Ok(qs) => qs,
            Err(e) => {
                summary.error = Some(e.to_string());
                // S256 — classify the failure for the structured
                // `QuoteIntakePollFailed` entry + the daemon's
                // backoff/pause decision.
                summary.error_reason = PollFailureReason::from_error(&e);
                summary.elapsed_ms = started.elapsed().as_millis() as u64;
                self.write_cycle_audit(&summary).await;
                return summary;
            }
        };
        summary.fetched = quotes.len() as u32;

        let pending_before_cycle = match self.snapshot_pending_writebacks().await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "snapshot pending writebacks failed; skipping retry");
                Vec::new()
            }
        };

        for quote in quotes {
            self.process_one_quote(&quote, &mut summary).await;
        }

        self.retry_pending_writebacks(&pending_before_cycle, &mut summary)
            .await;

        summary.elapsed_ms = started.elapsed().as_millis() as u64;
        self.write_cycle_audit(&summary).await;
        summary
    }

    async fn process_one_quote(&self, quote: &Quote, summary: &mut PollSummary) {
        let tenant_id = self.deps.tenant.as_str().to_string();
        let db_path = self.deps.db_path.clone();
        let db_handle = self.deps.db.clone();
        let quote_id = quote.id.clone();

        let quote_id_for_check = quote_id.clone();
        let tenant_for_check = tenant_id.clone();
        let db_for_check = db_handle.clone();
        let precheck = spawn_blocking(move || {
            let conn = db_for_check.read().map_err(|e| {
                QuoteIntakeError::Storage(format!("open tenant DB for precheck: {e}"))
            })?;
            log_table::already_intook(&conn, &tenant_for_check, &quote_id_for_check)
        })
        .await;

        match precheck {
            Ok(Ok(Some(_))) => {
                summary.skipped_duplicate += 1;
                return;
            }
            Ok(Ok(None)) => {}
            Ok(Err(e)) => {
                summary.failed.push((quote_id, e.to_string()));
                return;
            }
            Err(e) => {
                summary
                    .failed
                    .push((quote_id, format!("precheck task join: {e}")));
                return;
            }
        }

        let now = OffsetDateTime::now_utc();
        let mapping_outcome = match quote_to_draft_invoice(quote, now, self.deps.default_currency) {
            Ok(m) => m,
            Err(e) => {
                // S256 / PR-245 (brief §A.4) — a quote whose payload
                // fails mapping (missing contact email/name, bad status,
                // date overflow) is no longer silently dropped. Stage it
                // as an `error`-state row carrying the verbatim payload +
                // the message, so it surfaces in the Quotes tab and the
                // operator can retry-parse or mark-irrelevant. The
                // `quote_id` PRIMARY KEY makes this idempotent across
                // cycles (the precheck above skips an existing error row).
                let msg = e.to_string();
                let raw_payload_json =
                    serde_json::to_string(quote).unwrap_or_else(|_| "{}".to_string());
                let tenant_for_err = tenant_id.clone();
                let quote_id_for_err = quote.id.clone();
                let received_at_for_err = quote.received_at.clone();
                let db_for_err = db_handle.clone();
                let msg_for_err = msg.clone();
                let insert_err = spawn_blocking(move || {
                    let conn = db_for_err.write().map_err(|e| {
                        QuoteIntakeError::Storage(format!("open DB for error-row insert: {e}"))
                    })?;
                    log_table::insert_error_intake(
                        &conn,
                        &tenant_for_err,
                        &quote_id_for_err,
                        &received_at_for_err,
                        now,
                        &raw_payload_json,
                        &msg_for_err,
                    )
                })
                .await;
                match insert_err {
                    Ok(Ok(())) => summary.errored += 1,
                    Ok(Err(e)) => summary.failed.push((quote.id.clone(), e.to_string())),
                    Err(e) => summary
                        .failed
                        .push((quote.id.clone(), format!("error-row insert join: {e}"))),
                }
                tracing::warn!(quote_id = %quote.id, error = %msg, "quote failed mapping; staged as error row");
                return;
            }
        };

        let raw_payload_json = match serde_json::to_string(quote) {
            Ok(s) => s,
            Err(e) => {
                summary
                    .failed
                    .push((quote.id.clone(), format!("serialize raw payload: {e}")));
                return;
            }
        };
        let prepared_draft_json = match serde_json::to_string(&mapping_outcome.prepared) {
            Ok(s) => s,
            Err(e) => {
                summary
                    .failed
                    .push((quote.id.clone(), format!("serialize prepared draft: {e}")));
                return;
            }
        };

        let invoice_id = mapping_outcome.invoice_id.to_prefixed_string();
        let received_at = quote.received_at.clone();
        let tenant_for_insert = tenant_id.clone();
        let quote_id_for_insert = quote_id.clone();
        let db_for_insert = db_handle.clone();
        let invoice_id_for_insert = invoice_id.clone();
        let insert_outcome = spawn_blocking(move || {
            let conn = db_for_insert.write().map_err(|e| {
                QuoteIntakeError::Storage(format!("open tenant DB for insert: {e}"))
            })?;
            log_table::insert_intake(
                &conn,
                &tenant_for_insert,
                &quote_id_for_insert,
                &invoice_id_for_insert,
                &received_at,
                now,
                &raw_payload_json,
                &prepared_draft_json,
            )
        })
        .await;

        match insert_outcome {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                summary.failed.push((quote_id, e.to_string()));
                return;
            }
            Err(e) => {
                summary
                    .failed
                    .push((quote_id, format!("insert task join: {e}")));
                return;
            }
        }
        summary.created += 1;

        // S256 / PR-245 (brief §A.2) — emit a per-row arrival entry
        // carrying the customer's source `quote_id` so the SPA badge +
        // arrival toast can key on it and the arrival is traceable
        // end-to-end. Best-effort: a failure here is logged, not fatal
        // (the row is already staged; the badge re-derives from DB).
        self.write_row_added(quote, &invoice_id, now).await;

        let writeback_note = format!(
            "ABERP draft invoice {} created at {}",
            mapping_outcome.invoice_id.to_prefixed_string(),
            now.format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_else(|_| "unknown".to_string())
        );
        match self
            .transport
            .writeback_status(&quote.id, QUOTE_PROCESSING_STATUS, &writeback_note)
            .await
        {
            Ok(()) => {
                let tenant_for_mark = tenant_id.clone();
                let quote_id_for_mark = quote.id.clone();
                let db_for_mark = db_handle.clone();
                let mark_outcome = spawn_blocking(move || {
                    let conn = db_for_mark.write().map_err(|e| {
                        QuoteIntakeError::Storage(format!("open DB for mark writeback: {e}"))
                    })?;
                    log_table::mark_writeback_complete(
                        &conn,
                        &tenant_for_mark,
                        &quote_id_for_mark,
                        now,
                    )
                })
                .await;
                if let Err(e) = mark_outcome {
                    tracing::warn!(
                        quote_id = %quote.id,
                        error = %e,
                        "mark_writeback_complete task join failed; will retry next cycle"
                    );
                } else if let Ok(Err(e)) = mark_outcome {
                    tracing::warn!(
                        quote_id = %quote.id,
                        error = %e,
                        "mark_writeback_complete DB write failed; will retry next cycle"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    quote_id = %quote.id,
                    error = %e,
                    "writeback failed; will retry on next cycle"
                );
                summary.writeback_failed += 1;
            }
        }
    }

    async fn snapshot_pending_writebacks(&self) -> Result<Vec<String>, QuoteIntakeError> {
        let tenant_for_list = self.deps.tenant.as_str().to_string();
        let db_for_list = self.deps.db.clone();
        match spawn_blocking(move || {
            let conn = db_for_list.read()
                .map_err(|e| QuoteIntakeError::Storage(format!("open DB for pending list: {e}")))?;
            log_table::list_pending_writebacks(&conn, &tenant_for_list)
        })
        .await
        {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(e),
            Err(e) => Err(QuoteIntakeError::Storage(format!(
                "list_pending_writebacks task join: {e}"
            ))),
        }
    }

    async fn retry_pending_writebacks(&self, pending: &[String], summary: &mut PollSummary) {
        let tenant_id = self.deps.tenant.as_str().to_string();
        let db_path = self.deps.db_path.clone();
        let db_handle = self.deps.db.clone();

        for quote_id in pending {
            let note = "ABERP writeback retry";
            match self
                .transport
                .writeback_status(quote_id, QUOTE_PROCESSING_STATUS, note)
                .await
            {
                Ok(()) => {
                    let now = OffsetDateTime::now_utc();
                    let tenant_for_mark = tenant_id.clone();
                    let qid = quote_id.clone();
                    let db_for_mark = db_handle.clone();
                    let _ = spawn_blocking(move || {
                        let conn = db_for_mark.write().map_err(|e| {
                            QuoteIntakeError::Storage(format!(
                                "open DB for retry-mark writeback: {e}"
                            ))
                        })?;
                        log_table::mark_writeback_complete(&conn, &tenant_for_mark, &qid, now)
                    })
                    .await;
                    summary.writeback_retried += 1;
                }
                Err(e) => {
                    tracing::warn!(
                        quote_id = %quote_id,
                        error = %e,
                        "writeback retry still failing; will try again next cycle"
                    );
                    summary.writeback_failed += 1;
                }
            }
        }
    }

    /// S256 / PR-245 — write the per-cycle audit. ALWAYS emits the
    /// `QuoteIntakePollAttempted` heartbeat (no `should_emit` gate — the
    /// Settings panel needs a "last cycle" even for idle no-op cycles).
    /// When the cycle aborted on a transport failure
    /// (`summary.error_reason.is_some()`) it ALSO emits a structured
    /// `QuoteIntakePollFailed` entry in the same transaction.
    async fn write_cycle_audit(&self, summary: &PollSummary) {
        let attempted = QuoteIntakePollPayload {
            idempotency_key: Ulid::new().to_string(),
            trigger: summary.trigger.as_audit_str().to_string(),
            fetched_count: summary.fetched,
            created_count: summary.created,
            skipped_duplicate_count: summary.skipped_duplicate,
            writeback_retried_count: summary.writeback_retried,
            writeback_failed_count: summary.writeback_failed,
            failed_count: summary.failed.len() as u32,
            errored_count: summary.errored,
            elapsed_ms: summary.elapsed_ms,
            error: summary.error.clone(),
        };
        let failed = summary.error_reason.map(|reason| {
            let status = match reason {
                PollFailureReason::UnexpectedStatus => parse_status_from_detail(&summary.error),
                _ => None,
            };
            QuoteIntakePollFailedPayload {
                idempotency_key: Ulid::new().to_string(),
                trigger: summary.trigger.as_audit_str().to_string(),
                reason: reason.as_str().to_string(),
                status,
                detail: summary.error.clone(),
                elapsed_ms: summary.elapsed_ms,
            }
        });
        let db_handle = self.deps.db.clone();
        let tenant = self.deps.tenant.clone();
        let binary_hash = self.deps.binary_hash;
        let login = self.deps.operator_login.clone();
        let outcome = spawn_blocking(move || -> Result<(), QuoteIntakeError> {
            let mut conn = db_handle.write()
                .map_err(|e| QuoteIntakeError::Storage(format!("open DB for audit append: {e}")))?;
            aberp_audit_ledger::ensure_schema(&conn).map_err(|e| {
                QuoteIntakeError::Storage(format!("ensure audit-ledger schema: {e}"))
            })?;
            let tx = conn
                .transaction()
                .map_err(|e| QuoteIntakeError::Storage(format!("open audit tx: {e}")))?;
            let meta = LedgerMeta::new(tenant.clone(), binary_hash);
            let actor = Actor::from_local_cli(Ulid::new().to_string(), &login);
            write_poll_audit_entry(&tx, &meta, actor, &attempted)?;
            if let Some(failed) = failed.as_ref() {
                let actor2 = Actor::from_local_cli(Ulid::new().to_string(), &login);
                write_poll_failed_entry(&tx, &meta, actor2, failed)?;
            }
            tx.commit()
                .map_err(|e| QuoteIntakeError::Storage(format!("commit audit tx: {e}")))?;
            Ok(())
        })
        .await;
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "quote-intake cycle audit write failed");
            }
            Err(e) => {
                tracing::warn!(error = %e, "quote-intake cycle audit task panicked");
            }
        }
    }

    /// S256 / PR-245 — emit one `QuoteIntakeRowAdded` entry. Best-effort:
    /// a failure is logged but never aborts the cycle (the row is already
    /// staged in `quote_intake_log`; the badge re-derives from DB).
    async fn write_row_added(&self, quote: &Quote, invoice_id: &str, now: OffsetDateTime) {
        let payload = QuoteIntakeRowAddedPayload {
            idempotency_key: format!("quote_intake_row_added:{}", quote.id),
            quote_id: quote.id.clone(),
            invoice_id: invoice_id.to_string(),
            intake_at: now
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_else(|_| "unknown".to_string()),
        };
        let db_handle = self.deps.db.clone();
        let tenant = self.deps.tenant.clone();
        let binary_hash = self.deps.binary_hash;
        let login = self.deps.operator_login.clone();
        let outcome = spawn_blocking(move || -> Result<(), QuoteIntakeError> {
            let mut conn = db_handle.write().map_err(|e| {
                QuoteIntakeError::Storage(format!("open DB for row-added append: {e}"))
            })?;
            aberp_audit_ledger::ensure_schema(&conn).map_err(|e| {
                QuoteIntakeError::Storage(format!("ensure audit-ledger schema: {e}"))
            })?;
            let tx = conn
                .transaction()
                .map_err(|e| QuoteIntakeError::Storage(format!("open row-added tx: {e}")))?;
            let meta = LedgerMeta::new(tenant.clone(), binary_hash);
            let actor = Actor::from_local_cli(Ulid::new().to_string(), &login);
            write_row_added_entry(&tx, &meta, actor, &payload)?;
            tx.commit()
                .map_err(|e| QuoteIntakeError::Storage(format!("commit row-added tx: {e}")))?;
            Ok(())
        })
        .await;
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!(quote_id = %quote.id, error = %e, "QuoteIntakeRowAdded write failed")
            }
            Err(e) => {
                tracing::warn!(quote_id = %quote.id, error = %e, "QuoteIntakeRowAdded task panicked")
            }
        }
    }

    pub fn poll_interval(&self) -> Duration {
        self.config.poll_interval
    }

    /// Run the quote-intake daemon until the process exits OR
    /// `cancel` fires.
    ///
    /// PR-209 / S213 — the boot-delay sleep + steady-cadence sleep
    /// both race against `cancel.cancelled()` so a Tauri-window
    /// close or a Ctrl-C in `run_prod.sh` actually stops the daemon
    /// instead of waiting out the cadence (the cadence is often
    /// minutes, so pre-PR-209 shutdown left the daemon spinning).
    /// Mid-cycle cancellation is observed at the NEXT iteration —
    /// the in-flight `poll_once` finishes its current HTTP call
    /// (reqwest's connect timeout caps it) before the loop checks
    /// the token again. The shutdown timeout (5s default) is sized
    /// for this; a daemon that times out shows up by name in the
    /// `DaemonShutdownCompleted` audit row.
    pub async fn run_daemon_forever(self, cancel: CancellationToken) {
        let cadence = self.config.poll_interval;
        let service = Arc::new(self);
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(Duration::from_secs(30)) => {}
        }
        // S256 / PR-245 (brief §A.3) — exponential backoff on a
        // cycle-aborting transport failure: 5s → 15s → 60s, then settle
        // at the normal cadence. `backoff_idx` advances on each
        // consecutive failure and RESETS to 0 on the next success.
        let mut backoff_idx: usize = 0;
        loop {
            if cancel.is_cancelled() {
                return;
            }
            let s = service.clone();
            let summary = s.poll_once(PollTrigger::Daemon).await;
            tracing::info!(
                fetched = summary.fetched,
                created = summary.created,
                skipped = summary.skipped_duplicate,
                writeback_retried = summary.writeback_retried,
                writeback_failed = summary.writeback_failed,
                errored = summary.errored,
                failed = summary.failed.len(),
                elapsed_ms = summary.elapsed_ms,
                error = ?summary.error,
                reason = ?summary.error_reason,
                "quote-intake cycle complete"
            );

            // S256 / PR-245 (brief §A.5) — a 401 means the bearer was
            // rotated. PAUSE rather than hammer a known-bad credential:
            // stop the daemon loop entirely. The cycle already wrote a
            // `QuoteIntakePollFailed { reason: unauthorized }` entry,
            // which the Settings → Quote Intake panel reads to surface
            // the "re-paste bearer" prompt. Resumption is on the next
            // `aberp serve` boot after the operator re-pastes the token
            // (consistent with the existing no-hot-reload posture).
            if summary.error_reason == Some(PollFailureReason::Unauthorized) {
                tracing::error!(
                    "quote-intake daemon PAUSED: storefront returned 401 \
                     (bearer rotated/invalid). Re-paste the bearer token in \
                     Settings → Quote Intake and restart ABERP to resume."
                );
                return;
            }

            let sleep_dur = match summary.error_reason {
                Some(_) => {
                    let dur = backoff_duration(backoff_idx, cadence);
                    backoff_idx = backoff_idx.saturating_add(1);
                    tracing::warn!(
                        backoff_secs = dur.as_secs(),
                        "quote-intake cycle failed; backing off before retry"
                    );
                    dur
                }
                None => {
                    backoff_idx = 0;
                    cadence
                }
            };
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(sleep_dur) => {}
            }
        }
    }
}

/// S256 / PR-245 — backoff schedule for consecutive cycle failures:
/// 5s, 15s, 60s, then the normal cadence for every further consecutive
/// failure. `idx` is the count of prior consecutive failures.
fn backoff_duration(idx: usize, cadence: Duration) -> Duration {
    match idx {
        0 => Duration::from_secs(5),
        1 => Duration::from_secs(15),
        2 => Duration::from_secs(60),
        _ => cadence,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_follows_5_15_60_then_cadence() {
        let cadence = Duration::from_secs(60 * 5);
        assert_eq!(backoff_duration(0, cadence), Duration::from_secs(5));
        assert_eq!(backoff_duration(1, cadence), Duration::from_secs(15));
        assert_eq!(backoff_duration(2, cadence), Duration::from_secs(60));
        // 4th+ consecutive failure settles at the configured cadence.
        assert_eq!(backoff_duration(3, cadence), cadence);
        assert_eq!(backoff_duration(99, cadence), cadence);
    }

    #[test]
    fn parse_status_extracts_trailing_integer() {
        assert_eq!(
            parse_status_from_detail(&Some("quote-intake unexpected HTTP status 500".to_string())),
            Some(500)
        );
        assert_eq!(
            parse_status_from_detail(&Some("no digits here".to_string())),
            None
        );
        assert_eq!(parse_status_from_detail(&None), None);
    }
}
