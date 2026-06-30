//! Library helper for the `POST /api/invoices/:id/mark-paid` route
//! (PR-70 / ADR-0039). Records an operational "payment received"
//! event against a `Finalized` invoice without changing the NAV
//! regulatory state ladder.
//!
//! # Why this module exists
//!
//! Mark-paid is operational metadata, not a NAV submission. It
//! shares no NAV credentials, no submission queue, no XML emission
//! with the existing issue/submit/storno paths — so a sibling
//! module keeps the surface area narrow per CLAUDE.md rule 2. The
//! orchestration is short enough to live next to the route handler
//! but living here means the integration test in
//! `apps/aberp/tests/` can drive it without spinning up the HTTPS
//! listener (same posture as `storno_invoice_request` per A159).
//!
//! # Pipeline
//!
//!   1. Validate the `paid_at` string parses as ISO-8601 YYYY-MM-DD.
//!   2. Validate `method` (closed-vocab; route already deserialises
//!      via [`PaymentMethod`]'s serde).
//!   3. Open the audit ledger; reject with `AlreadyPaid` if any
//!      `InvoicePaymentRecorded` entry exists for this invoice
//!      (no-double-pay invariant per ADR-0039 §3).
//!   4. Append the `InvoicePaymentRecorded` entry under a single
//!      DuckDB transaction.
//!   5. Verify the audit chain post-commit (success-criterion gate).
//!   6. Sync the audit-ledger mirror file.
//!   7. Return the appended `PaymentRecord` to the caller.
//!
//! State-gate (the invoice must be `Finalized`) and currency-match
//! (`body.currency == invoice.currency`) live at the route layer
//! per the existing `storno_invoice_request` precedent — both need
//! `derive_state_for` and the billing DB read which the route
//! handler already does.

use aberp_audit_ledger::{self as audit_ledger, Actor, EventKind, Ledger, LedgerMeta, TenantId};
use aberp_billing::IdempotencyKey;
use anyhow::{anyhow, Context, Result};
use duckdb::Connection;

use crate::audit_payloads::{self, PaymentMethod};
use crate::audit_query::{self, PaymentRecord};

/// Typed outcome of an attempted mark-paid orchestration. Mirrors
/// the closed-vocab `SubmitRouteError` shape from `serve.rs` so the
/// route handler can map each variant to the right HTTP status
/// without re-parsing error strings.
#[derive(Debug)]
pub enum MarkPaidError {
    /// The invoice already has an `InvoicePaymentRecorded` audit
    /// entry. Route maps this to `409 Conflict` and echoes the
    /// existing payment record so the SPA can render the duplicate
    /// gracefully.
    AlreadyPaid(PaymentRecord),
    /// The supplied `paid_at` string is not a valid ISO-8601 date.
    /// Route maps this to `400 Bad Request`.
    InvalidPaidAt(String),
    /// `Other` swallows every propagated anyhow error per the
    /// existing `SubmitRouteError::Other` precedent. Route maps to
    /// `500 Internal Server Error`.
    Other(anyhow::Error),
}

impl From<anyhow::Error> for MarkPaidError {
    fn from(e: anyhow::Error) -> Self {
        MarkPaidError::Other(e)
    }
}

/// Inputs to the mark-paid orchestration. Mirrors the route's
/// JSON body shape with one extra `idempotency_key` field the
/// route mints fresh per request (no operator-supplied retry-key
/// surface in v1 per the session-92 brief out-of-scope list).
#[derive(Debug, Clone)]
pub struct MarkPaidInput {
    pub invoice_id: String,
    pub paid_at: String,
    pub amount_minor: i64,
    pub currency: String,
    pub method: PaymentMethod,
    pub reference: Option<String>,
}

/// Successful outcome — the appended `PaymentRecord` plus the
/// post-commit verify count for parity with the existing mutation
/// route response shapes.
#[derive(Debug, Clone)]
pub struct MarkPaidOutcome {
    pub payment: PaymentRecord,
    pub entries_verified: u64,
}

/// Append the `InvoicePaymentRecorded` audit entry for an invoice
/// after the route layer has cleared state-gate + currency-match
/// preconditions. Returns the resulting [`PaymentRecord`] for the
/// route's JSON echo body.
pub fn mark_paid(
    db: &aberp_db::HandleArc,
    tenant: TenantId,
    binary_hash: aberp_audit_ledger::BinaryHash,
    operator_login: &str,
    input: MarkPaidInput,
) -> std::result::Result<MarkPaidOutcome, MarkPaidError> {
    // 1. Validate paid_at.
    if !is_canonical_iso_date(&input.paid_at) {
        return Err(MarkPaidError::InvalidPaidAt(format!(
            "paid_at '{}' is not a valid ISO-8601 YYYY-MM-DD date",
            input.paid_at
        )));
    }

    // 2. Idempotency gate — refuse double-payment.
    // ADR-0098 C2 — idempotency read via a shared read clone (from_connection),
    // not an independent Ledger::open of the live path.
    let ledger_for_check = {
        let conn = db
            .read()
            .context("shared read: mark-paid idempotency gate (ADR-0098 Gap 1a C2)")?;
        Ledger::from_connection(conn, tenant.clone(), binary_hash)
    };
    if let Some(existing) = audit_query::payment_record_for(&ledger_for_check, &input.invoice_id)? {
        return Err(MarkPaidError::AlreadyPaid(existing));
    }
    drop(ledger_for_check);

    // 3. Mint actor + idempotency key.
    let session_id = ulid::Ulid::new().to_string();
    let actor = Actor::from_local_cli(session_id, operator_login);
    let idempotency_key = IdempotencyKey::new();

    // 4. Append the InvoicePaymentRecorded entry under one tx, through the
    //    shared Handle's writer (ADR-0098 C2). The WriteGuard's post-commit hook
    //    runs the lockstep sync_mirror on drop, so the explicit Ledger::open +
    //    sync_mirror (a second live opener) is removed.
    let ledger_meta = LedgerMeta::new(tenant.clone(), binary_hash);
    {
        let mut conn = db
            .write()
            .context("shared writer: mark-paid audit append (ADR-0098 Gap 1a C2)")?;
        write_payment_audit_entry(&mut conn, &ledger_meta, actor, idempotency_key, &input)?;
        // WriteGuard drops here -> post-commit hook runs the lockstep sync_mirror.
    }

    // 5. Verify chain post-commit via a shared READ clone (Ledger::from_connection)
    //    — sees the just-committed append; no independent Connection::open /
    //    Ledger::open re-open (the duckdb#23046 replay locus).
    let verify_conn = db
        .read()
        .context("shared read: verify chain after mark-paid (ADR-0098 C2)")?;
    let ledger = Ledger::from_connection(verify_conn, tenant, binary_hash);
    let verified = ledger
        .verify_chain()
        .context("audit-ledger chain verification failed AFTER mark-paid")?;

    // 7. Build the PaymentRecord echo from the inputs (the ledger
    //    has just persisted them verbatim).
    let payment = PaymentRecord {
        paid_at: input.paid_at,
        amount_minor: input.amount_minor,
        currency: input.currency,
        method: input.method,
        reference: input.reference,
    };
    Ok(MarkPaidOutcome {
        payment,
        entries_verified: verified,
    })
}

fn write_payment_audit_entry(
    conn: &mut Connection,
    ledger_meta: &LedgerMeta,
    actor: Actor,
    idempotency_key: IdempotencyKey,
    input: &MarkPaidInput,
) -> Result<()> {
    audit_ledger::ensure_schema(conn).context("ensure audit-ledger schema for mark-paid")?;
    let tx = conn
        .transaction()
        .context("begin DuckDB transaction (mark-paid audit append)")?;
    let payload = audit_payloads::InvoicePaymentRecordedPayload::new(
        &input.invoice_id,
        idempotency_key,
        &input.paid_at,
        input.amount_minor,
        &input.currency,
        input.method,
        input.reference.clone(),
    );
    audit_ledger::append_in_tx(
        &tx,
        ledger_meta,
        EventKind::InvoicePaymentRecorded,
        payload.to_bytes(),
        actor,
        Some(idempotency_key.to_canonical_string()),
    )
    .map_err(|e| anyhow!("audit_ledger::append_in_tx InvoicePaymentRecorded: {e}"))?;
    tx.commit()
        .context("commit DuckDB transaction (mark-paid audit append)")?;
    Ok(())
}

/// Strict YYYY-MM-DD validator. Uses `time::Date::parse` to reject
/// malformed input (alphabetic, out-of-range month/day, missing
/// dashes, etc.) per CLAUDE.md rule 12 — silent acceptance of
/// `"2026/05/26"` or `"26 May 2026"` would lock the wrong shape
/// into the audit ledger forever.
fn is_canonical_iso_date(s: &str) -> bool {
    let format = time::macros::format_description!("[year]-[month]-[day]");
    time::Date::parse(s, &format).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Happy path: well-formed dates parse cleanly.
    #[test]
    fn is_canonical_iso_date_accepts_well_formed_dates() {
        assert!(is_canonical_iso_date("2026-05-26"));
        assert!(is_canonical_iso_date("2026-01-01"));
        assert!(is_canonical_iso_date("2026-12-31"));
    }

    /// Malformed strings fail loud per CLAUDE.md rule 12. Pin each
    /// failure mode the operator might trip into.
    #[test]
    fn is_canonical_iso_date_rejects_malformed_strings() {
        assert!(!is_canonical_iso_date(""));
        assert!(!is_canonical_iso_date("2026/05/26"));
        assert!(!is_canonical_iso_date("26-05-2026"));
        assert!(!is_canonical_iso_date("2026-5-26")); // zero-pad required
        assert!(!is_canonical_iso_date("2026-13-01")); // bad month
        assert!(!is_canonical_iso_date("2026-02-30")); // bad day
        assert!(!is_canonical_iso_date("twenty-twenty-six")); // alphabetic
        assert!(!is_canonical_iso_date("2026-05-26T00:00:00")); // timestamp form
    }
}
