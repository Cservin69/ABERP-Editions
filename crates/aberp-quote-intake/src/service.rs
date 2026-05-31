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
use ulid::Ulid;

use aberp_audit_ledger::{Actor, BinaryHash, LedgerMeta, TenantId};
use aberp_billing::Currency;

use crate::audit::{should_emit, write_poll_audit_entry, PollTrigger, QuoteIntakePollPayload};
use crate::config::QuoteIntakeConfig;
use crate::error::QuoteIntakeError;
use crate::log_table;
use crate::mapping::{quote_to_draft_invoice, QUOTE_INVOICED_STATUS};
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
    pub failed: Vec<(String, String)>,
    pub elapsed_ms: u64,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct QuoteIntakeDeps {
    pub db_path: PathBuf,
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
            failed: Vec::new(),
            elapsed_ms: 0,
            error: None,
        };

        let quotes = match self.transport.list_approved_quotes().await {
            Ok(qs) => qs,
            Err(e) => {
                summary.error = Some(e.to_string());
                summary.elapsed_ms = started.elapsed().as_millis() as u64;
                self.write_audit_if_needed(&summary).await;
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
        self.write_audit_if_needed(&summary).await;
        summary
    }

    async fn process_one_quote(&self, quote: &Quote, summary: &mut PollSummary) {
        let tenant_id = self.deps.tenant.as_str().to_string();
        let db_path = self.deps.db_path.clone();
        let quote_id = quote.id.clone();

        let quote_id_for_check = quote_id.clone();
        let tenant_for_check = tenant_id.clone();
        let db_for_check = db_path.clone();
        let precheck = spawn_blocking(move || {
            let conn = duckdb::Connection::open(&db_for_check).map_err(|e| {
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
                summary.failed.push((quote.id.clone(), e.to_string()));
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
        let db_for_insert = db_path.clone();
        let insert_outcome = spawn_blocking(move || {
            let conn = duckdb::Connection::open(&db_for_insert).map_err(|e| {
                QuoteIntakeError::Storage(format!("open tenant DB for insert: {e}"))
            })?;
            log_table::insert_intake(
                &conn,
                &tenant_for_insert,
                &quote_id_for_insert,
                &invoice_id,
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

        let writeback_note = format!(
            "ABERP draft invoice {} created at {}",
            mapping_outcome.invoice_id.to_prefixed_string(),
            now.format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_else(|_| "unknown".to_string())
        );
        match self
            .transport
            .writeback_status(&quote.id, QUOTE_INVOICED_STATUS, &writeback_note)
            .await
        {
            Ok(()) => {
                let tenant_for_mark = tenant_id.clone();
                let quote_id_for_mark = quote.id.clone();
                let db_for_mark = db_path.clone();
                let mark_outcome = spawn_blocking(move || {
                    let conn = duckdb::Connection::open(&db_for_mark).map_err(|e| {
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
        let db_for_list = self.deps.db_path.clone();
        match spawn_blocking(move || {
            let conn = duckdb::Connection::open(&db_for_list)
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

        for quote_id in pending {
            let note = "ABERP writeback retry";
            match self
                .transport
                .writeback_status(quote_id, QUOTE_INVOICED_STATUS, note)
                .await
            {
                Ok(()) => {
                    let now = OffsetDateTime::now_utc();
                    let tenant_for_mark = tenant_id.clone();
                    let qid = quote_id.clone();
                    let db_for_mark = db_path.clone();
                    let _ = spawn_blocking(move || {
                        let conn = duckdb::Connection::open(&db_for_mark).map_err(|e| {
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

    async fn write_audit_if_needed(&self, summary: &PollSummary) {
        let payload = QuoteIntakePollPayload {
            idempotency_key: Ulid::new().to_string(),
            trigger: summary.trigger.as_audit_str().to_string(),
            fetched_count: summary.fetched,
            created_count: summary.created,
            skipped_duplicate_count: summary.skipped_duplicate,
            writeback_retried_count: summary.writeback_retried,
            writeback_failed_count: summary.writeback_failed,
            failed_count: summary.failed.len() as u32,
            elapsed_ms: summary.elapsed_ms,
            error: summary.error.clone(),
        };
        if !should_emit(&payload) {
            return;
        }
        let db_path = self.deps.db_path.clone();
        let tenant = self.deps.tenant.clone();
        let binary_hash = self.deps.binary_hash;
        let login = self.deps.operator_login.clone();
        let outcome = spawn_blocking(move || -> Result<(), QuoteIntakeError> {
            let mut conn = duckdb::Connection::open(&db_path)
                .map_err(|e| QuoteIntakeError::Storage(format!("open DB for audit append: {e}")))?;
            aberp_audit_ledger::ensure_schema(&conn).map_err(|e| {
                QuoteIntakeError::Storage(format!("ensure audit-ledger schema: {e}"))
            })?;
            let tx = conn
                .transaction()
                .map_err(|e| QuoteIntakeError::Storage(format!("open audit tx: {e}")))?;
            let meta = LedgerMeta::new(tenant.clone(), binary_hash);
            let actor = Actor::from_local_cli(Ulid::new().to_string(), &login);
            write_poll_audit_entry(&tx, &meta, actor, &payload)?;
            tx.commit()
                .map_err(|e| QuoteIntakeError::Storage(format!("commit audit tx: {e}")))?;
            Ok(())
        })
        .await;
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "QuoteIntakePollCompleted audit write failed");
            }
            Err(e) => {
                tracing::warn!(error = %e, "QuoteIntakePollCompleted audit task panicked");
            }
        }
    }

    pub fn poll_interval(&self) -> Duration {
        self.config.poll_interval
    }

    pub async fn run_daemon_forever(self) {
        let cadence = self.config.poll_interval;
        let service = Arc::new(self);
        tokio::time::sleep(Duration::from_secs(30)).await;
        loop {
            let s = service.clone();
            let summary = s.poll_once(PollTrigger::Daemon).await;
            tracing::info!(
                fetched = summary.fetched,
                created = summary.created,
                skipped = summary.skipped_duplicate,
                writeback_retried = summary.writeback_retried,
                writeback_failed = summary.writeback_failed,
                failed = summary.failed.len(),
                elapsed_ms = summary.elapsed_ms,
                error = ?summary.error,
                "quote-intake cycle complete"
            );
            tokio::time::sleep(cadence).await;
        }
    }
}
