//! S272 / PR-261 — DEAL saga on a `quote_intake_log` row.
//!
//! # What this module does
//!
//! Operator clicks DEAL on a quote intake row → this saga validates
//! preconditions, mints two placeholder ids (sales-order +
//! work-order), writes them onto the row inside a single DB
//! transaction, and emits three audit entries:
//!
//!   1. [`EventKind::QuoteDealIssued`] — top-level saga marker.
//!   2. [`EventKind::QuoteSalesOrderCreated`] — SO-side placeholder
//!      (brief pushback #1 — full SO module is named-deferred).
//!   3. [`EventKind::QuoteWorkOrderCreated`] — WO-side placeholder
//!      (PR-228 WO crate needs `product_id` + routing ops that the
//!      quote intake row does not yet carry).
//!
//! All three entries + the column writes share ONE [`Transaction`] per
//! ADR-0067: any step failing rolls back the whole saga.
//!
//! # Single-use invariant (EVE addendum 3)
//!
//! The CAS guard in [`log_table::mark_deal_issued_in_tx`] sets
//! `deal_issued_at` only `WHERE deal_issued_at IS NULL`; a replay
//! against an already-dealt row updates zero rows. This module then
//! converts that zero into [`DealSagaError::DealAlreadyIssued`], which
//! the route maps to HTTP 409 with machine code `deal_already_issued`.
//!
//! # EVE addendum 2 (stock_alert × REFRESH)
//!
//! If the row's `stock_alert` is TRUE, the saga refuses to deal until
//! the operator submits `refresh_ack = Some("REFRESH")` — case-sensitive,
//! literal, per [[hulye-biztos]]. The DEAL UI hides the BIG/RED token
//! field until the typed REFRESH token unlocks it; the server-side
//! gate is defense-in-depth.
//!
//! # DEAL token
//!
//! The expected token is the first 8 characters of the row's
//! `quote_id` — copyable verbatim from the row's reference column.
//! Storefront-side production quote ids are UUID-shaped (`0226e154-…`);
//! the test-only `q-1`-style ids fall through with the WHOLE id as the
//! token (the helper caps at `min(8, len)` so short ids stay valid).

use anyhow::{anyhow, Context, Result};
use duckdb::Connection;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use ulid::Ulid;

use aberp_audit_ledger::{append_in_tx, Actor, EventKind, LedgerMeta};
use aberp_quote_intake::log_table;

/// Literal token the operator must type into the REFRESH input when the
/// row's `stock_alert` column is TRUE. Case-sensitive per
/// [[hulye-biztos]]; the SPA input is NOT auto-uppercased so
/// operator-typed casing reaches the server verbatim.
pub const REFRESH_ACK_TOKEN: &str = "REFRESH";

/// Cap on the DEAL token comparison window. Storefront-side production
/// `quote_id`s are UUID-shaped (32+ chars); the test-only `q-1`-style
/// ids stay valid because [`expected_deal_token`] collapses to
/// `min(QUOTE_DEAL_TOKEN_LEN, quote_id.len())`.
pub const QUOTE_DEAL_TOKEN_LEN: usize = 8;

/// Closed-vocab DEAL-saga failure modes. Each variant maps to a
/// distinct HTTP 409 machine code the SPA renders as a red operator
/// toast (see `apps/aberp-ui/ui/src/routes/QuotesList.svelte`). The
/// pure module returns these; the route layer (`serve.rs`) does the
/// HTTP translation per [[trust-code-not-operator]].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum DealSagaError {
    /// The quote intake row does not exist for `(tenant, quote_id)`.
    /// Route maps to 404.
    #[error("quote {quote_id} not staged in quote_intake_log (tenant {tenant})")]
    NotStaged { tenant: String, quote_id: String },
    /// The row exists but its `intake_state` is not `staged` (it's
    /// `error` or `irrelevant`). Route maps to 409
    /// `not_actionable`.
    #[error("quote {quote_id} intake_state is {state:?}; only staged rows can be dealt")]
    NotActionable { quote_id: String, state: String },
    /// `deal_issued_at` is already set — EVE-addendum-3 single-use
    /// invariant. Route maps to 409 `deal_already_issued`.
    #[error("quote {quote_id} has already been dealt")]
    DealAlreadyIssued { quote_id: String },
    /// `stock_alert` is TRUE and the request did not carry
    /// `refresh_ack = "REFRESH"`. Route maps to 409
    /// `stock_alert_refresh_required`.
    #[error("quote {quote_id} has stock_alert TRUE; type REFRESH to acknowledge first")]
    StockAlertRefreshRequired { quote_id: String },
    /// The DEAL token operator typed does not equal the row's
    /// expected `quote_id[..min(8, len)]`. Route maps to 409
    /// `deal_token_mismatch`.
    #[error("quote {quote_id} DEAL token mismatch")]
    DealTokenMismatch { quote_id: String },
}

impl DealSagaError {
    /// Machine code surfaced on the 409 body so the SPA's toast routes
    /// to the right copy. Mirrors the storno-route pattern.
    pub fn machine_code(&self) -> &'static str {
        match self {
            DealSagaError::NotStaged { .. } => "not_staged",
            DealSagaError::NotActionable { .. } => "not_actionable",
            DealSagaError::DealAlreadyIssued { .. } => "deal_already_issued",
            DealSagaError::StockAlertRefreshRequired { .. } => "stock_alert_refresh_required",
            DealSagaError::DealTokenMismatch { .. } => "deal_token_mismatch",
        }
    }
}

/// Compute the expected DEAL token from a row's `quote_id`. The
/// operator must type the FIRST `QUOTE_DEAL_TOKEN_LEN` characters
/// verbatim; for sub-8-char ids the whole id is the token (defensive —
/// test-fixture `q-1` ids never reach prod but should not panic).
///
/// Case-sensitive per [[hulye-biztos]]: storefront-pushed UUID ids are
/// already lower-case; an uppercase typo on a UUID character set is a
/// legitimate mismatch.
pub fn expected_deal_token(quote_id: &str) -> String {
    let take = QUOTE_DEAL_TOKEN_LEN.min(quote_id.len());
    quote_id[..take].to_string()
}

/// Inputs to [`run_deal_saga`]. The route handler builds this from the
/// path param + JSON body + AppState plumbing.
#[derive(Debug, Clone)]
pub struct DealSagaInputs {
    pub tenant: String,
    pub quote_id: String,
    /// Operator login string for the audit `Actor`.
    pub actor: String,
    /// Operator-typed DEAL token (the BIG/RED single-use input). Must
    /// equal `expected_deal_token(quote_id)`.
    pub deal_token: String,
    /// Operator-typed REFRESH ack token. `Some("REFRESH")` unlocks the
    /// saga when the row's `stock_alert` is TRUE; `None` / any other
    /// value is rejected per EVE addendum 2.
    pub refresh_ack: Option<String>,
}

/// Outcome of a successful DEAL saga. Surfaced verbatim on the route's
/// 200 JSON body so the SPA can render the post-deal affordances.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DealSagaOutcome {
    /// `so_<ULID>` placeholder for the future Sales Order module.
    pub sales_order_id: String,
    /// `wo_<ULID>` placeholder for the future Work Order wire-up.
    pub work_order_id: String,
    /// ISO-8601 (RFC 3339) timestamp of the DEAL commit.
    pub deal_issued_at: String,
    /// `true` iff the saga consumed a REFRESH token (i.e. the row had
    /// `stock_alert = TRUE` at saga time).
    pub refresh_acknowledged: bool,
}

/// JSON payload for the top-level [`EventKind::QuoteDealIssued`] entry.
/// Captures everything a forensic walk needs to reconstruct the saga
/// without re-deriving from sibling entries.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuoteDealIssuedPayload {
    pub quote_id: String,
    pub tenant_id: String,
    pub sales_order_id: String,
    pub work_order_id: String,
    pub deal_token: String,
    pub refresh_acknowledged: bool,
    pub actor: String,
    pub idempotency_key: String,
    pub deal_issued_at: String,
}

impl QuoteDealIssuedPayload {
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("JSON serialize QuoteDealIssuedPayload")
    }
}

/// JSON payload for the [`EventKind::QuoteSalesOrderCreated`] entry.
/// Placeholder per brief pushback #1 — the full SO module is
/// named-deferred; this audit row carries the `so_<ULID>` that a
/// future SO backfill can adopt.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuoteSalesOrderCreatedPayload {
    pub quote_id: String,
    pub tenant_id: String,
    pub sales_order_id: String,
    pub actor: String,
    pub idempotency_key: String,
    pub created_at: String,
}

impl QuoteSalesOrderCreatedPayload {
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("JSON serialize QuoteSalesOrderCreatedPayload")
    }
}

/// JSON payload for the [`EventKind::QuoteWorkOrderCreated`] entry.
/// Placeholder — the PR-228 WO crate requires `product_id` plus at
/// least one routing op, neither of which lives on the quote intake
/// row at this stage of the auto-quoting pipeline. The `wo_<ULID>`
/// here is reserved for the future plumbing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuoteWorkOrderCreatedPayload {
    pub quote_id: String,
    pub tenant_id: String,
    pub work_order_id: String,
    pub actor: String,
    pub idempotency_key: String,
    pub created_at: String,
}

impl QuoteWorkOrderCreatedPayload {
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("JSON serialize QuoteWorkOrderCreatedPayload")
    }
}

/// Idempotency-key composer for the saga. `quote_deal:<quote_id>` for
/// the one-and-only DEAL on this row. The CAS guard makes the key
/// unique by construction (a replay never reaches `append_in_tx` —
/// the saga aborts before the audit emits).
pub fn deal_idempotency_key(quote_id: &str) -> String {
    format!("quote_deal:{quote_id}")
}

/// Run the DEAL saga end-to-end. Mirrors the [`crate::quote_pickup`]
/// posture: pure function over the `Connection` + ledger so the
/// integration test can drive it without spinning AppState.
///
/// On success, returns [`DealSagaOutcome`] carrying the freshly-minted
/// SO/WO placeholder ids. On any precondition failure, returns
/// `Err(`[`anyhow::Error`]`)` wrapping a [`DealSagaError`] — the route
/// downcast translates it to the right 4xx machine code.
pub fn run_deal_saga(
    conn: &mut Connection,
    ledger_meta: &LedgerMeta,
    ledger_actor: Actor,
    inputs: DealSagaInputs,
) -> Result<DealSagaOutcome> {
    log_table::ensure_schema(conn).map_err(|e| anyhow!("ensure quote_intake_log schema: {e}"))?;

    // ── Precondition reads (outside the tx) ───────────────────────────
    let row = log_table::read_for_deal(conn, &inputs.tenant, &inputs.quote_id)
        .map_err(|e| anyhow!("read quote_intake_log for DEAL: {e}"))?
        .ok_or_else(|| {
            anyhow!(DealSagaError::NotStaged {
                tenant: inputs.tenant.clone(),
                quote_id: inputs.quote_id.clone(),
            })
        })?;

    if row.intake_state != log_table::STATE_STAGED {
        return Err(anyhow!(DealSagaError::NotActionable {
            quote_id: inputs.quote_id.clone(),
            state: row.intake_state,
        }));
    }

    // Single-use precondition pre-check. The CAS guard in
    // `mark_deal_issued_in_tx` is the source of truth (defends against
    // a concurrent DEAL between the read here and the tx); this early
    // exit gives the operator a faster 409 + skips the audit-emit work
    // on the common replay path.
    if row.deal_issued_at.is_some() {
        return Err(anyhow!(DealSagaError::DealAlreadyIssued {
            quote_id: inputs.quote_id.clone(),
        }));
    }

    // Single-use takes precedence over REFRESH/token so a replay
    // attempt with a wrong token never overrides
    // `deal_already_issued` — the route's 409 stays consistent across
    // attack vectors per [[trust-code-not-operator]].
    if row.stock_alert {
        let acked = inputs.refresh_ack.as_deref() == Some(REFRESH_ACK_TOKEN);
        if !acked {
            return Err(anyhow!(DealSagaError::StockAlertRefreshRequired {
                quote_id: inputs.quote_id.clone(),
            }));
        }
    }

    let expected = expected_deal_token(&inputs.quote_id);
    if inputs.deal_token != expected {
        return Err(anyhow!(DealSagaError::DealTokenMismatch {
            quote_id: inputs.quote_id.clone(),
        }));
    }

    // ── Saga tx ───────────────────────────────────────────────────────
    let now_iso = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .context("format DEAL timestamp")?;
    let sales_order_id = format!("so_{}", Ulid::new());
    let work_order_id = format!("wo_{}", Ulid::new());
    let idempotency_key = deal_idempotency_key(&inputs.quote_id);
    let refresh_acked_at = if row.stock_alert {
        Some(now_iso.clone())
    } else {
        None
    };

    let tx = conn.transaction().context("begin DEAL saga tx")?;

    // CAS write — single-use guard. Zero rows updated means a
    // concurrent DEAL won between our precondition read and this CAS
    // (or the precondition read raced with the same caller and we
    // arrived here twice in two HTTP requests). Either way, abort and
    // surface `deal_already_issued`.
    let claimed = log_table::mark_deal_issued_in_tx(
        &tx,
        &inputs.tenant,
        &inputs.quote_id,
        &sales_order_id,
        &work_order_id,
        &now_iso,
        refresh_acked_at.as_deref(),
    )
    .map_err(|e| anyhow!("CAS mark_deal_issued_in_tx: {e}"))?;
    if claimed != 1 {
        // Drop the tx → roll back any prior partial state. (None in
        // this case — the CAS was the first write — but the pattern
        // mirrors quote_pickup's lost-race rollback for symmetry.)
        drop(tx);
        return Err(anyhow!(DealSagaError::DealAlreadyIssued {
            quote_id: inputs.quote_id.clone(),
        }));
    }

    // Three audit entries — top-level + SO + WO. All ride the same tx
    // so they appear/disappear atomically per ADR-0067.
    let top_payload = QuoteDealIssuedPayload {
        quote_id: inputs.quote_id.clone(),
        tenant_id: inputs.tenant.clone(),
        sales_order_id: sales_order_id.clone(),
        work_order_id: work_order_id.clone(),
        deal_token: inputs.deal_token.clone(),
        refresh_acknowledged: row.stock_alert,
        actor: inputs.actor.clone(),
        idempotency_key: idempotency_key.clone(),
        deal_issued_at: now_iso.clone(),
    };
    append_in_tx(
        &tx,
        ledger_meta,
        EventKind::QuoteDealIssued,
        top_payload.to_bytes(),
        ledger_actor.clone(),
        Some(idempotency_key.clone()),
    )
    .context("audit append QuoteDealIssued")?;

    let so_payload = QuoteSalesOrderCreatedPayload {
        quote_id: inputs.quote_id.clone(),
        tenant_id: inputs.tenant.clone(),
        sales_order_id: sales_order_id.clone(),
        actor: inputs.actor.clone(),
        idempotency_key: format!("{idempotency_key}:so"),
        created_at: now_iso.clone(),
    };
    append_in_tx(
        &tx,
        ledger_meta,
        EventKind::QuoteSalesOrderCreated,
        so_payload.to_bytes(),
        ledger_actor.clone(),
        Some(format!("{idempotency_key}:so")),
    )
    .context("audit append QuoteSalesOrderCreated")?;

    let wo_payload = QuoteWorkOrderCreatedPayload {
        quote_id: inputs.quote_id.clone(),
        tenant_id: inputs.tenant.clone(),
        work_order_id: work_order_id.clone(),
        actor: inputs.actor.clone(),
        idempotency_key: format!("{idempotency_key}:wo"),
        created_at: now_iso.clone(),
    };
    append_in_tx(
        &tx,
        ledger_meta,
        EventKind::QuoteWorkOrderCreated,
        wo_payload.to_bytes(),
        ledger_actor,
        Some(format!("{idempotency_key}:wo")),
    )
    .context("audit append QuoteWorkOrderCreated")?;

    tx.commit().context("commit DEAL saga tx")?;

    Ok(DealSagaOutcome {
        sales_order_id,
        work_order_id,
        deal_issued_at: now_iso,
        refresh_acknowledged: row.stock_alert,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aberp_audit_ledger::{
        ensure_schema as audit_ensure_schema, BinaryHash, LedgerMeta, TenantId,
    };
    use aberp_quote_intake::log_table as quote_log;
    use duckdb::Connection;

    fn open_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("open in-memory DuckDB");
        audit_ensure_schema(&conn).expect("audit-ledger schema");
        quote_log::ensure_schema(&conn).expect("quote_intake_log schema");
        conn
    }

    fn ledger_meta() -> LedgerMeta {
        LedgerMeta::new(
            TenantId::new("test-tenant").unwrap(),
            BinaryHash::from_bytes([0u8; 32]),
        )
    }

    fn actor() -> Actor {
        Actor::test_only()
    }

    fn stage_quote(conn: &Connection, quote_id: &str) {
        let now = time::OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        quote_log::insert_intake(
            conn,
            "test-tenant",
            quote_id,
            "inv_A",
            "2026-06-05T08:00:00Z",
            now,
            "{}",
            "{}",
        )
        .unwrap();
    }

    fn full_uuid_quote_id() -> &'static str {
        // Realistic storefront-shape; the first 8 chars are the DEAL
        // token. Mirrors the example in the design doc.
        "0226e154-9e6c-4c0a-9001-f3a8a0c0a000"
    }

    #[test]
    fn expected_deal_token_is_first_eight_chars_of_long_id() {
        assert_eq!(expected_deal_token(full_uuid_quote_id()), "0226e154");
    }

    #[test]
    fn expected_deal_token_caps_at_id_length_for_short_ids() {
        assert_eq!(expected_deal_token("q-1"), "q-1");
        assert_eq!(expected_deal_token("12345"), "12345");
        assert_eq!(expected_deal_token("12345678"), "12345678");
        assert_eq!(expected_deal_token("123456789"), "12345678");
    }

    /// Happy path on a stock-OK row: no REFRESH needed, the correct
    /// DEAL token commits the saga and surfaces SO/WO ids + the
    /// dealt-at timestamp.
    #[test]
    fn run_deal_saga_happy_path_no_stock_alert() {
        let mut conn = open_conn();
        let quote_id = full_uuid_quote_id();
        stage_quote(&conn, quote_id);
        let meta = ledger_meta();
        let outcome = run_deal_saga(
            &mut conn,
            &meta,
            actor(),
            DealSagaInputs {
                tenant: "test-tenant".to_string(),
                quote_id: quote_id.to_string(),
                actor: "operator-A".to_string(),
                deal_token: "0226e154".to_string(),
                refresh_ack: None,
            },
        )
        .expect("happy DEAL");

        assert!(outcome.sales_order_id.starts_with("so_"));
        assert!(outcome.work_order_id.starts_with("wo_"));
        assert!(!outcome.refresh_acknowledged);

        // The row now carries the saga's writes.
        let row = quote_log::read_for_deal(&conn, "test-tenant", quote_id)
            .unwrap()
            .unwrap();
        assert!(row.deal_issued_at.is_some());

        // Three audit entries landed.
        let mut count_kinds: std::collections::HashMap<String, i64> = Default::default();
        let mut stmt = conn
            .prepare("SELECT kind, COUNT(*) FROM audit_ledger GROUP BY kind")
            .unwrap();
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        });
        for r in rows.unwrap() {
            let (k, n) = r.unwrap();
            count_kinds.insert(k, n);
        }
        assert_eq!(count_kinds.get("quote.deal_issued"), Some(&1));
        assert_eq!(count_kinds.get("quote.sales_order_created"), Some(&1));
        assert_eq!(count_kinds.get("quote.work_order_created"), Some(&1));
    }

    /// Replay = single-use guard. The second saga call returns
    /// `DealAlreadyIssued`; the SPA routes it to a red toast.
    #[test]
    fn replay_returns_deal_already_issued() {
        let mut conn = open_conn();
        let quote_id = full_uuid_quote_id();
        stage_quote(&conn, quote_id);
        let meta = ledger_meta();
        run_deal_saga(
            &mut conn,
            &meta,
            actor(),
            DealSagaInputs {
                tenant: "test-tenant".to_string(),
                quote_id: quote_id.to_string(),
                actor: "operator-A".to_string(),
                deal_token: "0226e154".to_string(),
                refresh_ack: None,
            },
        )
        .expect("first DEAL");

        let err = run_deal_saga(
            &mut conn,
            &meta,
            actor(),
            DealSagaInputs {
                tenant: "test-tenant".to_string(),
                quote_id: quote_id.to_string(),
                actor: "operator-A".to_string(),
                deal_token: "0226e154".to_string(),
                refresh_ack: None,
            },
        )
        .unwrap_err();
        let saga = err.downcast::<DealSagaError>().expect("typed saga error");
        assert_eq!(saga.machine_code(), "deal_already_issued");
    }

    /// stock_alert TRUE + no REFRESH ack → 409 `stock_alert_refresh_required`.
    /// Submitting REFRESH then unlocks the saga.
    #[test]
    fn stock_alert_blocks_without_refresh_ack() {
        let mut conn = open_conn();
        let quote_id = full_uuid_quote_id();
        stage_quote(&conn, quote_id);
        quote_log::flip_stock_alert_to_true(&conn, "test-tenant", quote_id).unwrap();
        let meta = ledger_meta();

        // No REFRESH ack → 409.
        let err = run_deal_saga(
            &mut conn,
            &meta,
            actor(),
            DealSagaInputs {
                tenant: "test-tenant".to_string(),
                quote_id: quote_id.to_string(),
                actor: "operator-A".to_string(),
                deal_token: "0226e154".to_string(),
                refresh_ack: None,
            },
        )
        .unwrap_err();
        let saga = err.downcast::<DealSagaError>().expect("typed saga error");
        assert_eq!(saga.machine_code(), "stock_alert_refresh_required");

        // Lower-case "refresh" must NOT pass per [[hulye-biztos]].
        let err = run_deal_saga(
            &mut conn,
            &meta,
            actor(),
            DealSagaInputs {
                tenant: "test-tenant".to_string(),
                quote_id: quote_id.to_string(),
                actor: "operator-A".to_string(),
                deal_token: "0226e154".to_string(),
                refresh_ack: Some("refresh".to_string()),
            },
        )
        .unwrap_err();
        assert_eq!(
            err.downcast::<DealSagaError>().unwrap().machine_code(),
            "stock_alert_refresh_required",
            "lowercase refresh must not unlock the saga"
        );

        // Verbatim REFRESH → saga succeeds + `refresh_acknowledged`
        // reflects the ack.
        let outcome = run_deal_saga(
            &mut conn,
            &meta,
            actor(),
            DealSagaInputs {
                tenant: "test-tenant".to_string(),
                quote_id: quote_id.to_string(),
                actor: "operator-A".to_string(),
                deal_token: "0226e154".to_string(),
                refresh_ack: Some(REFRESH_ACK_TOKEN.to_string()),
            },
        )
        .expect("REFRESH-acked DEAL");
        assert!(outcome.refresh_acknowledged);
    }

    /// Wrong DEAL token → 409 `deal_token_mismatch`. The expected
    /// token is the row's `quote_id[..8]` verbatim, case-sensitive.
    #[test]
    fn wrong_deal_token_rejected_case_sensitive() {
        let mut conn = open_conn();
        let quote_id = full_uuid_quote_id();
        stage_quote(&conn, quote_id);
        let meta = ledger_meta();

        for bad in [
            "00000000",  // numeric typo
            "0226E154",  // case-flipped
            "0226e15",   // too short
            "0226e1545", // too long
            "",          // empty
        ] {
            let err = run_deal_saga(
                &mut conn,
                &meta,
                actor(),
                DealSagaInputs {
                    tenant: "test-tenant".to_string(),
                    quote_id: quote_id.to_string(),
                    actor: "operator-A".to_string(),
                    deal_token: bad.to_string(),
                    refresh_ack: None,
                },
            )
            .unwrap_err();
            let saga = err.downcast::<DealSagaError>().unwrap();
            assert_eq!(
                saga.machine_code(),
                "deal_token_mismatch",
                "bad token {bad:?} must be rejected as deal_token_mismatch"
            );
        }

        // The row stays un-dealt (no partial state from the rejected
        // attempts).
        let row = quote_log::read_for_deal(&conn, "test-tenant", quote_id)
            .unwrap()
            .unwrap();
        assert_eq!(row.deal_issued_at, None);
    }

    /// EVE-addendum-3 single-use takes precedence over the REFRESH
    /// check: a replay on an ALREADY-DEALT row (no matter the
    /// stock_alert / REFRESH ack state) returns `deal_already_issued`,
    /// NOT `stock_alert_refresh_required`. Pinned because the SPA's
    /// 409 toast routing depends on the machine code being stable.
    #[test]
    fn replay_precedence_over_stock_alert_check() {
        let mut conn = open_conn();
        let quote_id = full_uuid_quote_id();
        stage_quote(&conn, quote_id);
        let meta = ledger_meta();

        // First DEAL on a stock-OK row.
        run_deal_saga(
            &mut conn,
            &meta,
            actor(),
            DealSagaInputs {
                tenant: "test-tenant".to_string(),
                quote_id: quote_id.to_string(),
                actor: "operator-A".to_string(),
                deal_token: "0226e154".to_string(),
                refresh_ack: None,
            },
        )
        .unwrap();

        // Now flip stock_alert TRUE — a replay with NO REFRESH could
        // otherwise be misclassified as `stock_alert_refresh_required`.
        // Single-use must win.
        quote_log::flip_stock_alert_to_true(&conn, "test-tenant", quote_id).unwrap();
        let err = run_deal_saga(
            &mut conn,
            &meta,
            actor(),
            DealSagaInputs {
                tenant: "test-tenant".to_string(),
                quote_id: quote_id.to_string(),
                actor: "operator-A".to_string(),
                deal_token: "0226e154".to_string(),
                refresh_ack: None,
            },
        )
        .unwrap_err();
        assert_eq!(
            err.downcast::<DealSagaError>().unwrap().machine_code(),
            "deal_already_issued"
        );
    }

    #[test]
    fn missing_quote_returns_not_staged() {
        let mut conn = open_conn();
        let meta = ledger_meta();
        let err = run_deal_saga(
            &mut conn,
            &meta,
            actor(),
            DealSagaInputs {
                tenant: "test-tenant".to_string(),
                quote_id: "q-MISSING".to_string(),
                actor: "operator-A".to_string(),
                deal_token: "q-MISSIN".to_string(),
                refresh_ack: None,
            },
        )
        .unwrap_err();
        assert_eq!(
            err.downcast::<DealSagaError>().unwrap().machine_code(),
            "not_staged"
        );
    }

    #[test]
    fn dismissed_row_cannot_be_dealt() {
        let mut conn = open_conn();
        let quote_id = full_uuid_quote_id();
        stage_quote(&conn, quote_id);
        quote_log::mark_irrelevant(&conn, "test-tenant", quote_id).unwrap();
        let meta = ledger_meta();
        let err = run_deal_saga(
            &mut conn,
            &meta,
            actor(),
            DealSagaInputs {
                tenant: "test-tenant".to_string(),
                quote_id: quote_id.to_string(),
                actor: "operator-A".to_string(),
                deal_token: "0226e154".to_string(),
                refresh_ack: None,
            },
        )
        .unwrap_err();
        assert_eq!(
            err.downcast::<DealSagaError>().unwrap().machine_code(),
            "not_actionable"
        );
    }

    #[test]
    fn payload_round_trip_top_level() {
        let p = QuoteDealIssuedPayload {
            quote_id: "q-X".to_string(),
            tenant_id: "t".to_string(),
            sales_order_id: "so_X".to_string(),
            work_order_id: "wo_X".to_string(),
            deal_token: "q-X".to_string(),
            refresh_acknowledged: true,
            actor: "operator".to_string(),
            idempotency_key: "quote_deal:q-X".to_string(),
            deal_issued_at: "2026-06-06T12:00:00Z".to_string(),
        };
        let back: QuoteDealIssuedPayload = serde_json::from_slice(&p.to_bytes()).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn deal_idempotency_key_shape() {
        assert_eq!(deal_idempotency_key("q-1"), "quote_deal:q-1");
        assert_eq!(
            deal_idempotency_key("0226e154-..."),
            "quote_deal:0226e154-..."
        );
    }
}
