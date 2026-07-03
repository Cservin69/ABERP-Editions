//! S281 / PR-266 — Background drain for [`crate::email_relay_queue`].
//!
//! Walks `Queued` rows through `Sending → Sent` (or `→ Failed` after
//! exhausting retries) by composing a MIME message and calling
//! `lettre` with ABERP's existing SMTP creds (per [[aberp-smtp-spoc]]).
//!
//! ## Drain cadence
//!
//! 2s baseline tick. On SMTP failure the row is **requeued** with an
//! exponential backoff against wall-clock — but the daemon itself
//! sleeps a flat 2s between ticks; backoff state lives on the row
//! (`last_error` is observed, `attempt_n` caps the retry budget). A
//! single-tenant deployment means one daemon, no contention.
//!
//! Retry budget per row: 5 attempts. After the 5th SMTP failure the
//! row moves `Sending → Failed` and a terminal
//! [`EventKind::EmailRelayFailed`] audit row lands.
//!
//! ## Why one daemon
//!
//! ABERP runs one `aberp serve` per tenant (single-process). One
//! drain loop is sufficient; the CAS in
//! [`email_relay_queue::claim_next_queued`] makes the design robust
//! against accidental double-spawn at boot.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio_util::sync::CancellationToken;
use ulid::Ulid;
use zeroize::Zeroizing;

use aberp_audit_ledger::{append_in_tx, Actor, BinaryHash, EventKind, LedgerMeta, TenantId};

use crate::daemon_tick_guard::guard_write_tick;
use lettre::{
    message::{header::ContentType, Attachment, Mailbox, MultiPart, SinglePart},
    transport::smtp::{authentication::Credentials, client::Tls, client::TlsParameters},
    AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
};

use crate::audit_payloads::EmailRelayAuditPayload;
use crate::email_relay_queue::{
    self, claim_next_queued, mark_failed, mark_sent, read_row, reconcile_orphaned_sending,
    requeue_for_retry, OutboundEmailRow,
};
use crate::secrets_cache::SecretsCache;
use crate::smtp_config::{self, SmtpConfig, SmtpSecurity};

/// Drain tick — how often the loop wakes when there's nothing to do.
pub const DRAIN_TICK_SECS: u64 = 2;
/// Retry cap per row. The 5th failure transitions the row to
/// terminal `Failed`.
pub const MAX_ATTEMPTS_PER_ROW: u32 = 5;

/// Operator-facing kill switch — ADR-0098 S0 (the interim-stopgap
/// bridge). Set `ABERP_EMAIL_RELAY_DRAIN_DISABLED=1` (or `true`,
/// case-insensitive, surrounding whitespace ignored) to suppress this
/// daemon's spawn at boot.
///
/// Why it exists: this drain is the one **unconditional** ~2s
/// separate-instance DuckDB opener — it opens the live tenant DB every
/// [`DRAIN_TICK_SECS`] to claim a row. ADR-0098's interim stopgap
/// quiesces every *other* high-frequency opener (quote-intake /
/// catalogue-push / email-outbox / pdf-rerender) through existing
/// flags, but had no way to silence this one without a code gate
/// (ADR-0098 D8 / the ADR's "Interim stopgap" section). With this set
/// **plus** the storefront / outbox / rerender quiesce flags, the only
/// residual live-DB openers are the 4-h snapshot daemon and human-paced
/// SPA writes.
///
/// Named `POLL_DISABLE_ENV` to mirror
/// [`crate::email_outbox_poll_daemon::POLL_DISABLE_ENV`] and
/// [`crate::quote_pdf_rerender_daemon::POLL_DISABLE_ENV`] (and the
/// snapshot daemon's) — the house convention for a daemon's disable
/// const; only the *value* is drain-specific.
///
/// NOTE: disabling the drain means queued outbound email is **not
/// sent** until it is re-enabled (rows stay `Queued` and drain on the
/// next boot without the flag). Risk-reduction for a degraded
/// manual-invoicing session, **not** the full fix — Session B's single
/// shared `aberp_db::Handle` is what actually makes the process
/// single-writer.
pub const POLL_DISABLE_ENV: &str = "ABERP_EMAIL_RELAY_DRAIN_DISABLED";

/// Kill-switch check. Returns `true` iff the daemon should **not** be
/// spawned. Mirrors [`crate::email_outbox_poll_daemon::is_disabled`] and
/// [`crate::quote_pdf_rerender_daemon::is_disabled`] byte-for-byte so all
/// three operator switches parse identically: `1` / `true`
/// (case-insensitive, trimmed) disables; everything else — including
/// `0`, `false`, and empty — leaves the daemon ENABLED.
pub fn is_disabled() -> bool {
    std::env::var(POLL_DISABLE_ENV)
        .ok()
        .map(|v| {
            let t = v.trim();
            t == "1" || t.eq_ignore_ascii_case("true")
        })
        .unwrap_or(false)
}

/// Bound the wall-clock on each SMTP send. Inherited from
/// [`crate::email_invoice`]'s `SMTP_SEND_TIMEOUT`.
/// `pub(crate)` so [`crate::email_outbox_poll_daemon`] (S307) reuses the
/// same timeout — one source of truth per [[aberp-smtp-spoc]].
pub(crate) const SMTP_SEND_TIMEOUT_SECS: u64 = 30;

/// Dependencies the daemon needs threaded from `AppState`.
#[derive(Clone)]
pub struct EmailRelayDaemonDeps {
    pub db_path: PathBuf,
    /// ADR-0098 Session B (Gap 1a) — the one shared DuckDB handle. This drain
    /// was the unconditional ~2s separate-instance opener that drove the
    /// 17:02 re-tear; it now claims/marks rows through the single instance.
    pub db: aberp_db::HandleArc,
    pub tenant: TenantId,
    pub binary_hash: BinaryHash,
    pub operator_login: String,
    pub seller_toml_path: PathBuf,
    pub secrets_cache: SecretsCache,
}

/// Run the drain loop until `cancel` fires. Cooperative shutdown via
/// the standard [`tokio_util::sync::CancellationToken`] coordinator
/// per [[graceful-shutdown-s213]].
pub async fn run_drain_loop(deps: EmailRelayDaemonDeps, cancel: CancellationToken) {
    tracing::info!("email-relay drain daemon started");

    // S409 — heal rows orphaned in `Sending` by a previous process whose
    // terminal transition didn't land (the DuckDB secondary-index UPDATE
    // failure this PR removes). Runs ONCE, before any claim — so every
    // `Sending` row at this point is definitively orphaned, not mid-flight.
    // At-most-once: walked to `Sent`, never re-sent (no duplicate reaches
    // the customer, per [[hulye-biztos]]).
    match reconcile_startup(&deps).await {
        Ok(0) => {}
        Ok(n) => tracing::warn!(
            reconciled = n,
            "email-relay startup reconcile: walked orphaned Sending rows to Sent \
             without re-sending (see each row's last_error for the reason)"
        ),
        Err(e) => tracing::error!(error = ?e, "email-relay startup reconcile failed"),
    }

    let tick = Duration::from_secs(DRAIN_TICK_SECS);
    loop {
        if cancel.is_cancelled() {
            tracing::info!("email-relay drain daemon shutting down");
            return;
        }

        // Process one row per tick. Single-process deployment so no
        // need for a worker pool; the 2s cadence is well inside the
        // human-noticeable threshold.
        match process_one_row(&deps).await {
            Ok(true) => {
                // Worked something — loop immediately to drain
                // backlog. Cancel-checked above.
                continue;
            }
            Ok(false) => {
                // Nothing to do — wait a tick or until cancel.
            }
            Err(e) => {
                tracing::error!(error = ?e, "email-relay drain step failed");
            }
        }

        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("email-relay drain daemon shutting down");
                return;
            }
            _ = tokio::time::sleep(tick) => {}
        }
    }
}

/// One drain step. Returns `Ok(true)` when a row was processed,
/// `Ok(false)` when the queue was empty. Errors propagate to the
/// caller for logging; the loop keeps running.
async fn process_one_row(deps: &EmailRelayDaemonDeps) -> Result<bool> {
    let db = deps.db.clone();
    let now = time::OffsetDateTime::now_utc();
    let claimed = tokio::task::spawn_blocking(move || -> Result<Option<OutboundEmailRow>> {
        guard_write_tick("email-relay drain claim", move || {
            let conn = db
                .write()
                .context("shared writer: email-relay drain claim (ADR-0098 Gap 1a)")?;
            email_relay_queue::ensure_schema(&conn)?;
            claim_next_queued(&conn, now)
        })
    })
    .await
    .context("join claim task")??;

    let row = match claimed {
        Some(r) => r,
        None => return Ok(false),
    };

    let attempt_n = row.attempt_n;
    let smtp_outcome = send_one(deps, &row).await;

    match smtp_outcome {
        Ok(()) => {
            let id = row.id.clone();
            let db = deps.db.clone();
            let now2 = time::OffsetDateTime::now_utc();
            tokio::task::spawn_blocking(move || -> Result<()> {
                guard_write_tick("email-relay mark Sent", move || {
                    let conn = db
                        .write()
                        .context("shared writer: mark Sent (ADR-0098 Gap 1a)")?;
                    mark_sent(&conn, &id, now2)
                })
            })
            .await
            .context("join mark_sent task")??;
            write_relay_audit(
                deps,
                EventKind::EmailRelaySent,
                EmailRelayAuditPayload::sent(
                    &row.submitter,
                    &row.id,
                    &row.recipient_hash,
                    &row.subject,
                    row.byte_size,
                    attempt_n,
                ),
            )
            .await;
            Ok(true)
        }
        Err(e) => {
            // Retryable failure — requeue if budget remains, else mark
            // terminal Failed + audit.
            let detail = scrub_for_audit(&e.to_string());
            if attempt_n >= MAX_ATTEMPTS_PER_ROW {
                let id = row.id.clone();
                let db = deps.db.clone();
                let detail_for_db = detail.clone();
                tokio::task::spawn_blocking(move || -> Result<()> {
                    guard_write_tick("email-relay mark Failed", move || {
                        let conn = db
                            .write()
                            .context("shared writer: mark Failed (ADR-0098 Gap 1a)")?;
                        mark_failed(&conn, &id, &detail_for_db)
                    })
                })
                .await
                .context("join mark_failed task")??;
                write_relay_audit(
                    deps,
                    EventKind::EmailRelayFailed,
                    EmailRelayAuditPayload::failed(
                        &row.submitter,
                        &row.id,
                        &row.recipient_hash,
                        &row.subject,
                        row.byte_size,
                        attempt_n,
                        &detail,
                    ),
                )
                .await;
                tracing::warn!(
                    row_id = %row.id,
                    attempts = attempt_n,
                    "email-relay row terminally failed"
                );
            } else {
                let id = row.id.clone();
                let db = deps.db.clone();
                let detail_for_db = detail.clone();
                tokio::task::spawn_blocking(move || -> Result<()> {
                    guard_write_tick("email-relay requeue", move || {
                        let conn = db
                            .write()
                            .context("shared writer: requeue (ADR-0098 Gap 1a)")?;
                        requeue_for_retry(&conn, &id, &detail_for_db)
                    })
                })
                .await
                .context("join requeue task")??;
                tracing::warn!(
                    row_id = %row.id,
                    attempts = attempt_n,
                    "email-relay row requeued for retry"
                );
            }
            Ok(true)
        }
    }
}

/// S409 — one-shot startup reconcile of rows orphaned in `Sending`.
/// Opens its own short-lived connection (mirrors the per-op connection
/// posture of [`process_one_row`]). Returns the count reconciled.
async fn reconcile_startup(deps: &EmailRelayDaemonDeps) -> Result<u64> {
    let db = deps.db.clone();
    let now = time::OffsetDateTime::now_utc();
    tokio::task::spawn_blocking(move || -> Result<u64> {
        guard_write_tick("email-relay startup reconcile", move || {
            let conn = db
                .write()
                .context("shared writer: email-relay startup reconcile (ADR-0098 Gap 1a)")?;
            reconcile_orphaned_sending(&conn, now)
        })
    })
    .await
    .context("join reconcile task")?
}

/// Compose + send one queued row via SMTP. Returns `Ok(())` on
/// success, `Err(_)` with an operator-readable cause on failure.
async fn send_one(deps: &EmailRelayDaemonDeps, row: &OutboundEmailRow) -> Result<()> {
    let cfg = smtp_config::read_smtp_config(&deps.seller_toml_path)
        .context("read [seller.smtp] from seller.toml")?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no [seller.smtp] section in {}",
                deps.seller_toml_path.display()
            )
        })?;
    let password = deps
        .secrets_cache
        .smtp_password()
        .ok_or_else(|| anyhow::anyhow!("SMTP password not present in secrets cache"))?;

    let to_addrs: Vec<String> =
        serde_json::from_str(&row.to_recipients_json).context("parse to_recipients_json")?;
    let cc_addrs: Vec<String> = match row.cc_recipients_json.as_deref() {
        Some(s) => serde_json::from_str(s).context("parse cc_recipients_json")?,
        None => Vec::new(),
    };

    // Build From mailbox.
    let from_mbox = build_mailbox(&cfg.from_address, cfg.from_display_name.as_deref(), "from")?;

    // Assemble multipart body.
    let mut multipart = MultiPart::mixed().singlepart(
        SinglePart::builder()
            .header(ContentType::TEXT_PLAIN)
            .body(row.body_text.clone()),
    );
    if let Some(html) = &row.body_html {
        multipart = multipart.singlepart(
            SinglePart::builder()
                .header(ContentType::TEXT_HTML)
                .body(html.clone()),
        );
    }

    // Attachments — read from disk (basenames sit under attachments_dir).
    if let Some(rel_dir) = &row.attachments_dir {
        let root = email_relay_queue::attachments_root_for_tenant(deps.tenant.as_str())
            .context("resolve attachments root")?;
        let dir = root.join(rel_dir);
        if dir.exists() {
            let mut entries: Vec<std::fs::DirEntry> = std::fs::read_dir(&dir)
                .with_context(|| format!("read attachments dir {}", dir.display()))?
                .collect::<std::io::Result<Vec<_>>>()
                .with_context(|| format!("collect entries of {}", dir.display()))?;
            // Sort by basename so the index prefix delivers a stable order.
            entries.sort_by_key(|e| e.file_name());
            for ent in entries {
                let path = ent.path();
                let bytes = std::fs::read(&path)
                    .with_context(|| format!("read attachment {}", path.display()))?;
                let raw_name = ent.file_name().to_string_lossy().into_owned();
                // Strip the `NN_` index prefix the writer prepended.
                let display_name = match raw_name.split_once('_') {
                    Some((idx, rest)) if idx.chars().all(|c| c.is_ascii_digit()) => {
                        rest.to_string()
                    }
                    _ => raw_name.clone(),
                };
                let part = Attachment::new(display_name).body(
                    bytes,
                    ContentType::parse("application/octet-stream").context("content-type parse")?,
                );
                multipart = multipart.singlepart(part);
            }
        }
    }

    let mut builder = Message::builder()
        .from(from_mbox)
        .subject(row.subject.clone());
    for addr in &to_addrs {
        let mbox = build_mailbox(addr, None, "to")?;
        builder = builder.to(mbox);
    }
    for addr in &cc_addrs {
        let mbox = build_mailbox(addr, None, "cc")?;
        builder = builder.cc(mbox);
    }
    let message = builder.multipart(multipart).context("build MIME message")?;

    let transport = build_transport(&cfg, &password)?;
    transport
        .send(message)
        .await
        .map_err(|e| anyhow::anyhow!("SMTP transport: {e}"))?;
    Ok(())
}

/// `pub(crate)` so [`crate::email_outbox_poll_daemon`] (S307) shares the
/// same lettre `Mailbox` builder — one validation path for both relay
/// and outbox sender, per [[aberp-smtp-spoc]].
pub(crate) fn build_mailbox(
    address: &str,
    display_name: Option<&str>,
    label: &str,
) -> Result<Mailbox> {
    let (local, domain) = address
        .split_once('@')
        .ok_or_else(|| anyhow::anyhow!("{label} address `{address}` has no `@`"))?;
    let addr = lettre::Address::new(local, domain)
        .map_err(|e| anyhow::anyhow!("{label} lettre Address::new({local}, {domain}): {e}"))?;
    let name = display_name
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    Ok(Mailbox::new(name, addr))
}

/// `pub(crate)` so [`crate::email_outbox_poll_daemon`] (S307) shares the
/// same lettre transport builder — one TLS/StartTLS branch, one timeout
/// constant, one credential threading, per [[aberp-smtp-spoc]].
pub(crate) fn build_transport(
    cfg: &SmtpConfig,
    password: &Zeroizing<String>,
) -> Result<AsyncSmtpTransport<Tokio1Executor>> {
    let tls_params = TlsParameters::new(cfg.host.clone())
        .with_context(|| format!("TlsParameters for {}", cfg.host))?;
    let credentials = Credentials::new(cfg.username.clone(), password.as_str().to_string());
    let builder = match cfg.security {
        SmtpSecurity::Tls => AsyncSmtpTransport::<Tokio1Executor>::relay(&cfg.host)
            .with_context(|| format!("lettre relay({})", cfg.host))?
            .tls(Tls::Wrapper(tls_params)),
        SmtpSecurity::StartTls => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&cfg.host)
            .with_context(|| format!("lettre starttls_relay({})", cfg.host))?
            .tls(Tls::Required(tls_params)),
    };
    Ok(builder
        .port(cfg.port)
        .credentials(credentials)
        .timeout(Some(Duration::from_secs(SMTP_SEND_TIMEOUT_SECS)))
        .build())
}

/// Strip any bearer / password fragments from a transport error
/// before it lands in an audit row. Mirror of catalogue_push's
/// `scrub`. `pub(crate)` so [`crate::email_outbox_poll_daemon`] (S307)
/// reuses the same scrubbing rule for its terminal-failure events.
pub(crate) fn scrub_for_audit(s: &str) -> String {
    let mut out = s.to_string();
    if let Some(pos) = out.find("Bearer ") {
        out.replace_range(pos.., "Bearer <redacted>");
    }
    // Truncate at 1000 chars per ADR-0007 (header-injection-safe).
    if out.len() > 1000 {
        out.truncate(1000);
    }
    out
}

/// Append one `email.*` audit entry. Mirror of catalogue_push's
/// `write_audit` posture: spawn_blocking around the DuckDB write.
pub(crate) async fn write_relay_audit(
    deps: &EmailRelayDaemonDeps,
    kind: EventKind,
    payload: EmailRelayAuditPayload,
) {
    let db = deps.db.clone();
    let tenant = deps.tenant.clone();
    let binary_hash = deps.binary_hash;
    let login = deps.operator_login.clone();
    let res = tokio::task::spawn_blocking(move || -> Result<()> {
        let mut conn = db
            .write()
            .context("shared writer: email-relay audit (ADR-0098 Gap 1a)")?;
        aberp_audit_ledger::ensure_schema(&conn).context("ensure audit schema")?;
        let bytes = payload.to_bytes();
        let tx = conn.transaction().context("begin email-relay audit tx")?;
        let meta = LedgerMeta::new(tenant, binary_hash);
        let actor = Actor::from_local_cli(Ulid::new().to_string(), &login);
        append_in_tx(&tx, &meta, kind, bytes, actor, None).context("append email-relay audit")?;
        tx.commit().context("commit email-relay audit")?;
        Ok(())
    })
    .await;
    match res {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::error!(error = ?e, "email-relay audit write failed"),
        Err(join) => tracing::error!(%join, "email-relay audit task panicked"),
    }
}

#[allow(dead_code)]
pub(crate) fn read_row_helper(
    db: &aberp_db::HandleArc,
    id: &str,
) -> Result<Option<OutboundEmailRow>> {
    let conn = db
        .read()
        .context("shared read: read_row_helper (ADR-0098 Gap 1a)")?;
    read_row(&conn, id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrub_for_audit_strips_bearer_token_suffix() {
        let raw = "request failed: Bearer abcDEF1234567890";
        let scrubbed = scrub_for_audit(raw);
        assert!(!scrubbed.contains("abcDEF1234567890"));
        assert!(scrubbed.contains("<redacted>"));
    }

    #[test]
    fn scrub_for_audit_truncates_long_strings() {
        let big = "x".repeat(5000);
        let out = scrub_for_audit(&big);
        assert!(out.len() <= 1000);
    }

    #[test]
    fn scrub_for_audit_preserves_short_clean_errors() {
        let s = "connection refused";
        assert_eq!(scrub_for_audit(s), s);
    }

    #[test]
    fn retry_cap_matches_brief() {
        // PR-266 brief §B: "max 5 attempts". Pin the value so a
        // future contributor can't silently relax it.
        assert_eq!(MAX_ATTEMPTS_PER_ROW, 5);
    }

    // ── ADR-0098 S0 (bridge) — the `ABERP_EMAIL_RELAY_DRAIN_DISABLED`
    //    kill switch. The email-relay-drain spawn in `serve.rs` branches
    //    solely on `is_disabled()`: set => the `if`-arm logs and does NOT
    //    spawn; unset => the `else`-arm spawns (today's behavior). The
    //    full serve boot can't run in this sandbox (bundled DuckDB/Tauri),
    //    so these unit-test the gate decision directly — the exact
    //    predicate the two spawn branches read. Mirrors the email-outbox /
    //    pdf-rerender `is_disabled` tests; a process-wide `ENV_LOCK`
    //    serializes the env mutation so the default parallel test runner
    //    can't race the key.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn relay_drain_is_disabled_false_by_default_so_drain_spawns() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::remove_var(POLL_DISABLE_ENV);
        // Unset => NOT disabled => serve takes the `else` arm and spawns
        // the daemon, byte-for-byte today's behavior.
        assert!(!is_disabled());
    }

    #[test]
    fn relay_drain_is_disabled_true_for_canonical_values_so_spawn_is_skipped() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        for v in ["1", "true", "TRUE", "True", " 1 ", " true "] {
            std::env::set_var(POLL_DISABLE_ENV, v);
            assert!(is_disabled(), "expected disabled=true for {v:?}");
        }
        // Falsey / non-canonical values keep the daemon ENABLED — no
        // foot-gun where a typo silently stops outbound email.
        for v in ["0", "false", "no", "off", "", "yes"] {
            std::env::set_var(POLL_DISABLE_ENV, v);
            assert!(!is_disabled(), "expected disabled=false for {v:?}");
        }
        std::env::remove_var(POLL_DISABLE_ENV);
    }

    #[test]
    fn relay_drain_disable_env_name_is_the_documented_stopgap_flag() {
        // Pin the operator-facing contract: the exact env var name in
        // ADR-0098 §"Interim stopgap" / D8 and the S0 ops doc. A rename
        // here would silently break the documented degraded-mode flag-set.
        assert_eq!(POLL_DISABLE_ENV, "ABERP_EMAIL_RELAY_DRAIN_DISABLED");
    }
}
