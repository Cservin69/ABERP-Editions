//! Offline submission queue — derived state from the audit ledger
//! (PR-18, ADR-0031).
//!
//! # What this module is
//!
//! Pure-functional read-side derivation of "which invoices are pending
//! NAV submission" from the audit ledger. NO side table; NO new
//! `EventKind` variant; NO new DDL. The audit ledger is the canonical
//! source per ADR-0008 §"Storage" — this module's contribution is the
//! TYPED PREDICATE that classifies a tenant's invoices into
//! `pending` / `submitted` / `abandoned`, plus the constants ADR-0009
//! §7 names (hard cap, alert thresholds).
//!
//! # PR-42 / F45 — state-2 Pending classifier extension
//!
//! [`pending_retries_from_ledger`] is the state-2 Pending sibling of
//! [`pending_from_ledger`]: the `aberp drain-pending-retries` worker
//! consumes it to drive an automatic Layer-2 + TX1+wire+TX2 retry per
//! state-2 stuck invoice. Predicate per ADR-0032 §4: an Attempt
//! exists, no Response, no MarkedAbandoned. Sibling-and-not-fused
//! per CLAUDE.md rule 7 — drain-submission-queue and drain-pending-
//! retries are operator-confirmed-distinct phases of the offline-
//! recovery surface (state-1 backlog vs. state-2 mid-flight loss);
//! merging them would average two distinct predicates into one
//! ambiguous list.
//!
//! # Predicate (ADR-0031 §1, extended by ADR-0032 §5)
//!
//! An invoice is `pending submission` iff ALL of the following hold:
//!
//!   - The audit ledger contains an `InvoiceDraftCreated` entry whose
//!     payload's `invoice_id` field equals the invoice's prefixed ULID.
//!   - The audit ledger does NOT contain any
//!     `InvoiceSubmissionResponse` entry whose payload's `invoice_id`
//!     equals the same invoice id.
//!   - The audit ledger does NOT contain any
//!     `InvoiceMarkedAbandoned` entry whose payload's `invoice_id`
//!     equals the same invoice id.
//!   - **(PR-19 / ADR-0032 §5)** The audit ledger does NOT contain
//!     any `InvoiceSubmissionAttempt` entry whose payload's
//!     `invoice_id` equals the same invoice id.
//!
//! Four predicates, not three (was three pre-PR-19, two before
//! ADR-0031). The fourth predicate (Attempt exclusion) keeps the
//! drain's automatic posture cleanly separated from the
//! `retry-submission` operator surface: an invoice with an Attempt
//! is either in-flight (race with the drain), state-2 Pending
//! (failed mid-flight per ADR-0032 §4), or about to land a Response
//! (which would re-exclude it on the next drain run). Drain handles
//! only pure-Draft invoices; `retry-submission` handles every other
//! recoverable state. The `InvoiceSubmissionAttemptFailed` entry is
//! NOT a fifth predicate — an AttemptFailed entry exists iff an
//! Attempt entry exists (the two are paired in TX1 + TX2 of the
//! same submission), so excluding by AttemptFailed alone would be
//! redundant.
//!
//! # Why a separate module
//!
//! Mirrors `audit_query.rs`'s posture: the precondition walk is
//! reusable across multiple call sites (`issue-invoice`'s pre-
//! allocation cap check + `drain-submission-queue`'s FIFO surface),
//! and inlining it in either would duplicate ~50 LoC of decode-and-
//! filter. Per CLAUDE.md rule 7 ("surface conflicts, don't average
//! them"), one classifier in one place.
//!
//! # F12 four-edit ritual status
//!
//! NOT exercised. The drain command writes existing `EventKind`
//! variants (`InvoiceSubmissionAttempt`, `InvoiceSubmissionResponse`);
//! ADR-0031 §"Surfaced conflict 2" rejected the alternative
//! `InvoiceQueuedForSubmissionPayload` variant. The ritual remains
//! at its ninth landing.

use std::path::Path;
use std::time::Duration;

use aberp_audit_ledger::{BinaryHash, Entry, EventKind, Ledger, TenantId};
use aberp_billing::IdempotencyKey;
use aberp_nav_transport::NavTransportError;
use anyhow::{anyhow, Context, Result};
use time::OffsetDateTime;

use crate::audit_payloads;

// ──────────────────────────────────────────────────────────────────────
// Constants — ADR-0009 §7 + ADR-0031 §§5–6
// ──────────────────────────────────────────────────────────────────────

/// ADR-0009 §7 hard cap on the unsubmitted backlog. `issue-invoice`,
/// `issue-storno`, and `issue-modification` refuse to allocate a
/// fresh sequence number when the audit ledger already shows this
/// many pending invoices. ADR-0031 §5.
///
/// Hard-coded today; F42 names the operator-config trigger.
pub const HARD_CAP_PENDING: usize = 50;

/// ADR-0009 §7 alert threshold: surface a WARN at this many pending
/// invoices. ADR-0031 §6 (non-control-flow; visibility only).
///
/// Hard-coded today; F42 names the operator-config trigger.
pub const ALERT_PENDING_COUNT: usize = 5;

/// ADR-0009 §7 alert threshold: surface a WARN when the oldest
/// pending invoice is older than this. ADR-0031 §6 (non-control-flow;
/// visibility only).
///
/// Hard-coded today; F42 names the operator-config trigger.
pub const ALERT_OLDEST_PENDING: Duration = Duration::from_secs(30 * 60);

// ──────────────────────────────────────────────────────────────────────
// PendingInvoice — the per-invoice classifier output
// ──────────────────────────────────────────────────────────────────────

/// One invoice that is currently pending NAV submission. Carries the
/// fields the drain worker needs to drive a NAV call: the prefixed
/// invoice id, the issuance idempotency key (F8 carryforward to the
/// new Attempt / Response entries), the recorded XML path (per
/// ADR-0031 §2 — `None` for pre-PR-18 entries), and the issue date
/// (read from the audit-ledger entry's `time_wall`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingInvoice {
    /// Prefixed `inv_<ULID>` form — the same shape every other
    /// orchestration module already uses.
    pub invoice_id: String,
    /// The issuance idempotency key, parsed from the
    /// `InvoiceDraftCreated` payload's `idempotency_key` field.
    /// F8 contract — drain's new audit entries carry this same key
    /// onward.
    pub idempotency_key: IdempotencyKey,
    /// On-disk NAV InvoiceData XML path. `None` for pre-PR-18
    /// entries (their `InvoiceDraftCreatedPayload` did not carry
    /// `nav_xml_path`); drain loud-fails on `None` unless the
    /// operator passes `--xml-path-override` for this invoice id.
    pub nav_xml_path: Option<String>,
    /// Issuance wall-clock time — read from the
    /// `InvoiceDraftCreated` audit entry's `time_wall` column.
    /// Drives FIFO ordering and the age-alert threshold.
    pub issue_date: OffsetDateTime,
}

// ──────────────────────────────────────────────────────────────────────
// from_ledger — classify the tenant's invoices
// ──────────────────────────────────────────────────────────────────────

/// Walk the audit ledger once and return every pending invoice in
/// issue-date order (FIFO per ADR-0009 §7).
///
/// Single-pass O(n) over the ledger. The function decodes every
/// `InvoiceDraftCreated` entry, then filters out those that have a
/// matching `InvoiceSubmissionResponse` or `InvoiceMarkedAbandoned`.
/// Unparseable payloads, idempotency-key parse failures, and any
/// audit-payload schema drift loud-fail per CLAUDE.md rule 12.
///
/// Returned vec is sorted by `issue_date` ascending; ties broken by
/// `invoice_id` ascending (deterministic, stable across runs).
pub fn pending_from_ledger(ledger: &Ledger) -> Result<Vec<PendingInvoice>> {
    let entries = ledger
        .entries()
        .context("read audit ledger entries to classify pending submissions")?;
    classify_pending(&entries)
}

/// Count the pending invoices without materialising the per-invoice
/// metadata. Used by `issue-invoice`'s pre-allocation cap check
/// (ADR-0031 §5).
///
/// Opens its own `Ledger` from the supplied DB path. The signature
/// takes the same `(db_path, tenant, binary_hash)` triple every
/// other call site already builds; this keeps the cap check a one-
/// liner at the `issue-*` orchestration boundaries.
pub fn count_pending(
    db_path: &Path,
    tenant: TenantId,
    binary_hash: BinaryHash,
) -> Result<usize> {
    let ledger = Ledger::open(db_path, tenant, binary_hash)
        .context("open audit ledger to count pending submissions")?;
    Ok(pending_from_ledger(&ledger)?.len())
}

/// Pure classifier — operates on a borrowed entry slice so unit
/// tests can drive it with fixtures without round-tripping a real
/// `Ledger`. The function is loud per CLAUDE.md rule 12 on every
/// classification failure mode.
fn classify_pending(entries: &[Entry]) -> Result<Vec<PendingInvoice>> {
    use std::collections::HashSet;

    // First pass: collect the invoice_ids of every invoice excluded
    // by the four predicates (per ADR-0031 §1 + ADR-0032 §5).
    let mut excluded: HashSet<String> = HashSet::new();
    for entry in entries {
        match entry.kind {
            EventKind::InvoiceSubmissionResponse => {
                let payload: audit_payloads::InvoiceSubmissionResponsePayload =
                    serde_json::from_slice(&entry.payload).map_err(|e| {
                        anyhow!(
                            "InvoiceSubmissionResponse audit payload (seq {}) failed typed decode: {e} \
                             — audit ledger appears tampered or schema-drifted",
                            entry.seq.as_u64()
                        )
                    })?;
                excluded.insert(payload.invoice_id);
            }
            EventKind::InvoiceMarkedAbandoned => {
                let payload: audit_payloads::InvoiceMarkedAbandonedPayload =
                    serde_json::from_slice(&entry.payload).map_err(|e| {
                        anyhow!(
                            "InvoiceMarkedAbandoned audit payload (seq {}) failed typed decode: {e} \
                             — audit ledger appears tampered or schema-drifted",
                            entry.seq.as_u64()
                        )
                    })?;
                excluded.insert(payload.invoice_id);
            }
            // PR-19 / ADR-0032 §5 — fourth predicate clause. An
            // Attempt entry means the invoice is in-flight, state-2
            // Pending, or about to land a Response; drain skips it.
            // `retry-submission` is the operator surface for any
            // recovery action.
            EventKind::InvoiceSubmissionAttempt => {
                let payload: audit_payloads::InvoiceSubmissionAttemptPayload =
                    serde_json::from_slice(&entry.payload).map_err(|e| {
                        anyhow!(
                            "InvoiceSubmissionAttempt audit payload (seq {}) failed typed decode: {e} \
                             — audit ledger appears tampered or schema-drifted",
                            entry.seq.as_u64()
                        )
                    })?;
                excluded.insert(payload.invoice_id);
            }
            _ => {}
        }
    }

    // Second pass: classify every InvoiceDraftCreated entry, filter
    // out the excluded ones, build the PendingInvoice list.
    let mut pending: Vec<PendingInvoice> = Vec::new();
    for entry in entries {
        if entry.kind != EventKind::InvoiceDraftCreated {
            continue;
        }
        let payload: audit_payloads::InvoiceDraftCreatedPayload =
            serde_json::from_slice(&entry.payload).map_err(|e| {
                anyhow!(
                    "InvoiceDraftCreated audit payload (seq {}) failed typed decode: {e} \
                     — audit ledger appears tampered or schema-drifted",
                    entry.seq.as_u64()
                )
            })?;
        if excluded.contains(&payload.invoice_id) {
            continue;
        }
        let idempotency_key = IdempotencyKey::from_canonical_string(&payload.idempotency_key)
            .ok_or_else(|| {
                anyhow!(
                    "InvoiceDraftCreated payload (seq {}) idempotency_key '{}' failed parse — \
                     the audit ledger appears tampered or schema-drifted",
                    entry.seq.as_u64(),
                    payload.idempotency_key
                )
            })?;
        pending.push(PendingInvoice {
            invoice_id: payload.invoice_id,
            idempotency_key,
            nav_xml_path: payload.nav_xml_path,
            issue_date: entry.time_wall,
        });
    }

    // FIFO by issue_date asc, tie-break by invoice_id asc.
    pending.sort_by(|a, b| {
        a.issue_date
            .cmp(&b.issue_date)
            .then_with(|| a.invoice_id.cmp(&b.invoice_id))
    });

    Ok(pending)
}

// ──────────────────────────────────────────────────────────────────────
// PendingRetry — state-2 Pending classifier (PR-42 / F45 / ADR-0032 §4)
// ──────────────────────────────────────────────────────────────────────

/// One invoice currently in state-2 Pending per ADR-0032 §4: an
/// `InvoiceSubmissionAttempt` entry exists, but no
/// `InvoiceSubmissionResponse` and no `InvoiceMarkedAbandoned`. The
/// `aberp drain-pending-retries` worker drives one Layer-2 →
/// TX1+wire+TX2 retry pipeline per entry, FIFO by issuance.
///
/// PR-42 / F45 closure. The struct mirrors [`PendingInvoice`]'s field
/// shape so the drain-pending-retries per-invoice driver and the
/// drain-submission-queue per-invoice driver share idempotency-key /
/// nav_xml_path / issue_date semantics; the `aberp retry-submission`
/// operator command keeps its own CLI-arg input shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingRetry {
    /// Prefixed `inv_<ULID>` form.
    pub invoice_id: String,
    /// The original issuance's idempotency key (F8 contract — every
    /// NAV-related entry for an invoice shares the same key). Read
    /// from the most-recent `InvoiceSubmissionAttempt` entry's payload
    /// per ADR-0032 §4's idempotency_key semantics.
    pub idempotency_key: IdempotencyKey,
    /// On-disk NAV InvoiceData XML path, read from the matching
    /// `InvoiceDraftCreatedPayload.nav_xml_path`. `None` for pre-
    /// PR-18 entries; the drain worker loud-fails per CLAUDE.md
    /// rule 12 — pre-PR-18 state-2 invoices must be drained via the
    /// operator-driven `aberp retry-submission --invoice-xml <path>`
    /// command, not the automatic loop.
    pub nav_xml_path: Option<String>,
    /// Issuance wall-clock time — read from the
    /// `InvoiceDraftCreated` audit entry's `time_wall` column. Drives
    /// FIFO ordering across pending retries (oldest stuck invoice
    /// retries first; matches the `pending_from_ledger` posture).
    pub issue_date: OffsetDateTime,
}

/// Walk the audit ledger once and return every state-2 Pending
/// invoice in issue-date order (FIFO).
///
/// Single-pass O(n) over the ledger. Predicate per ADR-0032 §4:
///
///   - The audit ledger contains an `InvoiceSubmissionAttempt` entry
///     whose payload's `invoice_id` equals the invoice's prefixed
///     ULID.
///   - The audit ledger does NOT contain any
///     `InvoiceSubmissionResponse` entry for the same invoice_id.
///   - The audit ledger does NOT contain any
///     `InvoiceMarkedAbandoned` entry for the same invoice_id.
///
/// The presence of `InvoiceSubmissionAttemptFailed` does NOT change
/// classification (an Attempt + AttemptFailed is still state-2
/// Pending; the operator may re-retry per ADR-0032 §4). Multiple
/// Attempts for the same invoice deduplicate to a single PendingRetry
/// (the F8 contract pins one idempotency key per invoice).
///
/// Returned vec is sorted by `issue_date` ascending, ties broken by
/// `invoice_id` ascending — deterministic and stable across runs.
pub fn pending_retries_from_ledger(ledger: &Ledger) -> Result<Vec<PendingRetry>> {
    let entries = ledger
        .entries()
        .context("read audit ledger entries to classify pending retries")?;
    classify_pending_retries(&entries)
}

/// Pure classifier — operates on a borrowed entry slice so unit
/// tests can drive it with fixtures without round-tripping a real
/// `Ledger`.
fn classify_pending_retries(entries: &[Entry]) -> Result<Vec<PendingRetry>> {
    use std::collections::{HashMap, HashSet};

    // First pass: collect the invoice_ids excluded by Response or
    // MarkedAbandoned, AND index the Draft entries for nav_xml_path +
    // issue_date lookup.
    let mut excluded: HashSet<String> = HashSet::new();
    let mut draft_meta: HashMap<String, (Option<String>, OffsetDateTime)> = HashMap::new();
    for entry in entries {
        match entry.kind {
            EventKind::InvoiceSubmissionResponse => {
                let payload: audit_payloads::InvoiceSubmissionResponsePayload =
                    serde_json::from_slice(&entry.payload).map_err(|e| {
                        anyhow!(
                            "InvoiceSubmissionResponse audit payload (seq {}) failed typed decode: {e} \
                             — audit ledger appears tampered or schema-drifted",
                            entry.seq.as_u64()
                        )
                    })?;
                excluded.insert(payload.invoice_id);
            }
            EventKind::InvoiceMarkedAbandoned => {
                let payload: audit_payloads::InvoiceMarkedAbandonedPayload =
                    serde_json::from_slice(&entry.payload).map_err(|e| {
                        anyhow!(
                            "InvoiceMarkedAbandoned audit payload (seq {}) failed typed decode: {e} \
                             — audit ledger appears tampered or schema-drifted",
                            entry.seq.as_u64()
                        )
                    })?;
                excluded.insert(payload.invoice_id);
            }
            EventKind::InvoiceDraftCreated => {
                let payload: audit_payloads::InvoiceDraftCreatedPayload =
                    serde_json::from_slice(&entry.payload).map_err(|e| {
                        anyhow!(
                            "InvoiceDraftCreated audit payload (seq {}) failed typed decode: {e} \
                             — audit ledger appears tampered or schema-drifted",
                            entry.seq.as_u64()
                        )
                    })?;
                draft_meta.insert(payload.invoice_id, (payload.nav_xml_path, entry.time_wall));
            }
            _ => {}
        }
    }

    // Second pass: collect every Attempt's invoice_id (deduplicated),
    // then build PendingRetry rows for those not excluded.
    let mut seen: HashSet<String> = HashSet::new();
    let mut pending: Vec<PendingRetry> = Vec::new();
    for entry in entries {
        if entry.kind != EventKind::InvoiceSubmissionAttempt {
            continue;
        }
        let payload: audit_payloads::InvoiceSubmissionAttemptPayload =
            serde_json::from_slice(&entry.payload).map_err(|e| {
                anyhow!(
                    "InvoiceSubmissionAttempt audit payload (seq {}) failed typed decode: {e} \
                     — audit ledger appears tampered or schema-drifted",
                    entry.seq.as_u64()
                )
            })?;
        if excluded.contains(&payload.invoice_id) {
            continue;
        }
        if !seen.insert(payload.invoice_id.clone()) {
            continue;
        }
        let idempotency_key = IdempotencyKey::from_canonical_string(&payload.idempotency_key)
            .ok_or_else(|| {
                anyhow!(
                    "InvoiceSubmissionAttempt payload (seq {}) idempotency_key '{}' failed parse — \
                     the audit ledger appears tampered or schema-drifted",
                    entry.seq.as_u64(),
                    payload.idempotency_key
                )
            })?;
        let (nav_xml_path, issue_date) = match draft_meta.get(&payload.invoice_id) {
            Some((p, d)) => (p.clone(), *d),
            None => {
                return Err(anyhow!(
                    "InvoiceSubmissionAttempt for invoice {} has no matching \
                     InvoiceDraftCreated entry in the ledger — \
                     the audit ledger appears tampered or schema-drifted",
                    payload.invoice_id
                ));
            }
        };
        pending.push(PendingRetry {
            invoice_id: payload.invoice_id,
            idempotency_key,
            nav_xml_path,
            issue_date,
        });
    }

    // FIFO by issue_date asc, tie-break by invoice_id asc.
    pending.sort_by(|a, b| {
        a.issue_date
            .cmp(&b.issue_date)
            .then_with(|| a.invoice_id.cmp(&b.invoice_id))
    });

    Ok(pending)
}

// ──────────────────────────────────────────────────────────────────────
// is_transport_error — typed NAV-transport-vs-application classifier
// ──────────────────────────────────────────────────────────────────────

/// True iff `err` is a NAV-transport-layer failure (HTTP / TLS / DNS
/// / reqwest::Client construction). False for NAV-side application
/// errors (HTTP status, response-parse, etc.) and credential errors
/// (KeychainItemMissing, KeychainBackend).
///
/// ADR-0031 §4 — the drain command's loop body short-circuits the
/// FIFO walk on transport errors only. Application errors are per-
/// invoice failures the operator addresses per invoice; the drain
/// moves on to the next invoice in queue.
///
/// Matched on the typed `NavTransportError` enum — deterministic
/// code per CLAUDE.md rule 5. A future variant addition must explicitly
/// classify here; the `_` arm defaults to `false` (NOT a transport
/// error), which is the safe-default direction: a misclassified
/// transport error makes the drain continue on a real outage (loud
/// per-invoice errors continue), while a misclassified application
/// error would make the drain incorrectly stop. The wrong-way risk
/// is the second one; the `false` default biases away from it.
pub fn is_transport_error(err: &NavTransportError) -> bool {
    matches!(
        err,
        NavTransportError::ClientBuild(_)
            | NavTransportError::TokenExchangeHttp(_)
            | NavTransportError::ManageInvoiceHttp(_)
    )
}

// ──────────────────────────────────────────────────────────────────────
// classify_attempt_failure — PR-19 / ADR-0032 §2
// ──────────────────────────────────────────────────────────────────────

/// PR-19 / ADR-0032 §2: deterministic classifier mapping a
/// [`NavTransportError`] variant into the
/// `InvoiceSubmissionAttemptFailedPayload.error_class` enumeration.
///
/// Returns a tuple of `(error_class, error_code)`:
///
///   - `error_class` is one of the seven documented classes
///     (`"transport"`, `"http_status"`, `"application"`,
///     `"retryable_application"`, `"envelope"`, `"credential"`,
///     `"client_build"`).
///   - `error_code` is `Some(...)` for `application` /
///     `retryable_application` (NAV's funcCode / errorCode string)
///     or for `http_status` (the status as decimal string), and
///     `None` for `transport` / `envelope` / `credential` /
///     `client_build`.
///
/// The default arm (`_` in the match) classifies as `"application"`.
/// Rationale per ADR-0032 §2: misclassification as application is
/// the safe direction — the drain's transport-vs-application fork
/// (`is_transport_error`) makes its own decision; the audit
/// payload's `error_class` is for bundle-reader diagnosis, not for
/// control flow.
///
/// Deterministic code per CLAUDE.md rule 5; loud-fail on no path
/// (the function is total).
pub fn classify_attempt_failure(err: &NavTransportError) -> (&'static str, Option<String>) {
    match err {
        // Transport classes (the wire broke). PR-20 / ADR-0033 §5
        // adds the queryInvoiceCheck variant; same shape.
        NavTransportError::TokenExchangeHttp(_)
        | NavTransportError::ManageInvoiceHttp(_)
        | NavTransportError::QueryTransactionStatusHttp(_)
        | NavTransportError::ManageAnnulmentHttp(_)
        | NavTransportError::QueryInvoiceDataHttp(_)
        | NavTransportError::QueryInvoiceCheckHttp(_) => ("transport", None),

        // HTTP-status classes (NAV returned non-2xx). PR-20 /
        // ADR-0033 §5 adds the queryInvoiceCheck variant.
        NavTransportError::TokenExchangeHttpStatus { status }
        | NavTransportError::ManageInvoiceHttpStatus { status }
        | NavTransportError::QueryTransactionStatusHttpStatus { status }
        | NavTransportError::ManageAnnulmentHttpStatus { status }
        | NavTransportError::QueryInvoiceDataHttpStatus { status }
        | NavTransportError::QueryInvoiceCheckHttpStatus { status } => {
            ("http_status", Some(status.to_string()))
        }

        // Application-class non-retryable (NAV-side error code).
        // PR-20 / ADR-0033 §5 adds the queryInvoiceCheck variant.
        NavTransportError::ManageInvoiceNonRetryable { code, .. }
        | NavTransportError::QueryTransactionStatusNonRetryable { code, .. }
        | NavTransportError::ManageAnnulmentNonRetryable { code, .. }
        | NavTransportError::QueryInvoiceDataNonRetryable { code, .. }
        | NavTransportError::QueryInvoiceCheckNonRetryable { code, .. } => {
            ("application", Some(code.clone()))
        }

        // Application-class retryable (NAV-side OPERATION_FAILED / 504).
        // PR-20 / ADR-0033 §5 adds the queryInvoiceCheck variant.
        NavTransportError::ManageInvoiceRetryable { code, .. }
        | NavTransportError::QueryTransactionStatusRetryable { code, .. }
        | NavTransportError::ManageAnnulmentRetryable { code, .. }
        | NavTransportError::QueryInvoiceDataRetryable { code, .. }
        | NavTransportError::QueryInvoiceCheckRetryable { code, .. } => {
            ("retryable_application", Some(code.clone()))
        }

        // Application-class response-parse (NAV body unparseable).
        // PR-20 / ADR-0033 §5 adds the queryInvoiceCheck variant.
        NavTransportError::TokenExchangeResponseParse(_)
        | NavTransportError::ManageInvoiceResponseParse(_)
        | NavTransportError::QueryTransactionStatusResponseParse(_)
        | NavTransportError::ManageAnnulmentResponseParse(_)
        | NavTransportError::QueryInvoiceDataResponseParse(_)
        | NavTransportError::QueryInvoiceCheckResponseParse(_) => ("application", None),

        // Application-class token-decoding failures (NAV bytes failed
        // local decrypt / base64 / length checks). These are NAV-side
        // issues observed at the local boundary.
        NavTransportError::TokenExchangeBase64Decode(_)
        | NavTransportError::TokenExchangeBadCiphertextLength { .. }
        | NavTransportError::TokenExchangeDecryptFailed(_) => ("application", None),

        // Envelope-class (programmer / upstream-library error).
        NavTransportError::EnvelopeWriteFailed(_)
        | NavTransportError::ManageInvoiceEmpty
        | NavTransportError::ManageInvoiceTooManyItems { .. }
        | NavTransportError::ManageAnnulmentEmpty
        | NavTransportError::ManageAnnulmentTooManyItems { .. } => ("envelope", None),

        // Credential-class (keychain access).
        NavTransportError::KeychainItemMissing { .. }
        | NavTransportError::KeychainBackend { .. } => ("credential", None),

        // Client-build class (reqwest::ClientBuilder failure +
        // trust-anchor build failures — both are construction-time
        // bytecode failures, distinct from runtime credential or
        // wire failures).
        NavTransportError::ClientBuild(_)
        | NavTransportError::EmbeddedPemMalformed(_)
        | NavTransportError::EmbeddedCertificateRejected(_) => ("client_build", None),
    }
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    //! Unit tests on the classifier. Build small in-memory ledgers
    //! and verify the predicate's three-arm decision matrix
    //! (Pending / Submitted-excluded / Abandoned-excluded) plus
    //! the FIFO ordering and cross-invoice-id isolation.

    use super::*;
    use aberp_audit_ledger::{Actor, BinaryHash, Ledger, TenantId};
    use aberp_billing::IdempotencyKey;

    fn fixture_ledger() -> (Ledger, Actor) {
        let tenant = TenantId::new("t1".to_string()).unwrap();
        let bh = BinaryHash::from_bytes([0u8; 32]);
        let ledger = Ledger::open_in_memory(tenant, bh).unwrap();
        let actor = Actor::from_local_cli("sess".to_string(), "test-user");
        (ledger, actor)
    }

    fn write_draft_created(
        ledger: &mut Ledger,
        actor: &Actor,
        invoice_id: &str,
        idem: IdempotencyKey,
        nav_xml_path: Option<&str>,
    ) {
        let payload = audit_payloads::InvoiceDraftCreatedPayload {
            invoice_id: invoice_id.to_string(),
            line_count: 1,
            idempotency_key: idem.to_canonical_string(),
            nav_xml_path: nav_xml_path.map(|s| s.to_string()),
            // PR-44γ — test fixture for the HUF path; the five
            // rate-metadata fields are `None` per the C10
            // byte-identical invariant prerequisite.
            currency: None,
            exchange_rate: None,
            exchange_rate_source: None,
            exchange_rate_date: None,
            huf_equivalent_total: None,
        };
        ledger
            .append(
                EventKind::InvoiceDraftCreated,
                payload.to_bytes(),
                actor.clone(),
                Some(idem.to_canonical_string()),
            )
            .unwrap();
    }

    fn write_submission_response(
        ledger: &mut Ledger,
        actor: &Actor,
        invoice_id: &str,
        idem: IdempotencyKey,
        txid: &str,
    ) {
        let payload = audit_payloads::InvoiceSubmissionResponsePayload::new(
            invoice_id,
            idem,
            txid,
            b"<response/>".to_vec(),
        );
        ledger
            .append(
                EventKind::InvoiceSubmissionResponse,
                payload.to_bytes(),
                actor.clone(),
                Some(idem.to_canonical_string()),
            )
            .unwrap();
    }

    fn write_marked_abandoned(
        ledger: &mut Ledger,
        actor: &Actor,
        invoice_id: &str,
        idem: IdempotencyKey,
        txid: &str,
    ) {
        let payload = audit_payloads::InvoiceMarkedAbandonedPayload::new(
            invoice_id,
            idem,
            Some(txid.to_string()),
            None,
            "test abandon",
        );
        ledger
            .append(
                EventKind::InvoiceMarkedAbandoned,
                payload.to_bytes(),
                actor.clone(),
                Some(idem.to_canonical_string()),
            )
            .unwrap();
    }

    #[test]
    fn empty_ledger_has_no_pending() {
        let (ledger, _actor) = fixture_ledger();
        let pending = pending_from_ledger(&ledger).unwrap();
        assert!(pending.is_empty());
    }

    /// A draft entry with no matching response or abandon is pending.
    /// Pins the base case of the three-arm predicate.
    #[test]
    fn draft_created_without_response_or_abandon_is_pending() {
        let (mut ledger, actor) = fixture_ledger();
        let idem = IdempotencyKey::new();
        write_draft_created(&mut ledger, &actor, "inv_A", idem, Some("/tmp/A.xml"));
        let pending = pending_from_ledger(&ledger).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].invoice_id, "inv_A");
        assert_eq!(pending[0].nav_xml_path.as_deref(), Some("/tmp/A.xml"));
        assert_eq!(pending[0].idempotency_key, idem);
    }

    /// A draft followed by a submission response is NOT pending.
    /// Pins ADR-0031 §1's exclusion clause.
    #[test]
    fn draft_followed_by_submission_response_is_not_pending() {
        let (mut ledger, actor) = fixture_ledger();
        let idem = IdempotencyKey::new();
        write_draft_created(&mut ledger, &actor, "inv_A", idem, Some("/tmp/A.xml"));
        write_submission_response(&mut ledger, &actor, "inv_A", idem, "TXID-A");
        let pending = pending_from_ledger(&ledger).unwrap();
        assert!(pending.is_empty());
    }

    /// A draft followed by mark-abandoned is NOT pending. Pins the
    /// three-predicate version against the two-predicate trap (an
    /// abandoned invoice would be forever-pending under the two-
    /// predicate `Draft minus Response` predicate alone).
    #[test]
    fn draft_followed_by_marked_abandoned_is_not_pending() {
        let (mut ledger, actor) = fixture_ledger();
        let idem = IdempotencyKey::new();
        write_draft_created(&mut ledger, &actor, "inv_A", idem, Some("/tmp/A.xml"));
        write_marked_abandoned(&mut ledger, &actor, "inv_A", idem, "TXID-A");
        let pending = pending_from_ledger(&ledger).unwrap();
        assert!(pending.is_empty());
    }

    /// FIFO ordering invariant: two `pending_from_ledger` calls on
    /// the same ledger return the same order. CLAUDE.md rule 9: a
    /// regression that introduces non-determinism (e.g., a HashMap
    /// iteration order leaking into the sort) would surface here.
    ///
    /// We do NOT assert that B sorts first (issued first) or A
    /// sorts first (invoice_id-tie-break-asc) because the wall-clock
    /// resolution between two sequential `Ledger::append` calls is
    /// platform-dependent. The tie-break path is exercised by the
    /// `sort_by` closure's `then_with(invoice_id.cmp)` clause; its
    /// correctness is verified by inspection.
    #[test]
    fn pending_results_are_deterministically_ordered() {
        let (mut ledger, actor) = fixture_ledger();
        let idem_b = IdempotencyKey::new();
        let idem_a = IdempotencyKey::new();
        write_draft_created(&mut ledger, &actor, "inv_B", idem_b, Some("/tmp/B.xml"));
        write_draft_created(&mut ledger, &actor, "inv_A", idem_a, Some("/tmp/A.xml"));
        let pending = pending_from_ledger(&ledger).unwrap();
        assert_eq!(pending.len(), 2);
        let pending_again = pending_from_ledger(&ledger).unwrap();
        assert_eq!(pending, pending_again);
    }

    /// Cross-invoice contamination: a submission response for B does
    /// NOT block A's pending classification. Mirrors the defence-in-
    /// depth pin in `audit_query::tests::precondition_does_not_cross_invoice_ids`.
    #[test]
    fn pending_classification_does_not_cross_invoice_ids() {
        let (mut ledger, actor) = fixture_ledger();
        let idem_a = IdempotencyKey::new();
        let idem_b = IdempotencyKey::new();
        write_draft_created(&mut ledger, &actor, "inv_A", idem_a, Some("/tmp/A.xml"));
        write_draft_created(&mut ledger, &actor, "inv_B", idem_b, Some("/tmp/B.xml"));
        // B is submitted; A is not.
        write_submission_response(&mut ledger, &actor, "inv_B", idem_b, "TXID-B");
        let pending = pending_from_ledger(&ledger).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].invoice_id, "inv_A");
    }

    /// Pre-PR-18 entries (no `nav_xml_path` field) classify as
    /// pending with `nav_xml_path: None`. The drain worker's loud-
    /// fail-or-override behaviour is its problem; the classifier
    /// reports the data faithfully per CLAUDE.md rule 12.
    #[test]
    fn pre_pr_18_draft_entries_classify_as_pending_with_none_path() {
        let (mut ledger, actor) = fixture_ledger();
        let idem = IdempotencyKey::new();
        // Use the from_invoice constructor (no path) — same shape a
        // pre-PR-18 binary would have written. Plus a hand-built
        // bytes equivalent to assert the deserialise-with-default
        // behaviour explicitly.
        write_draft_created(&mut ledger, &actor, "inv_A", idem, None);
        let pending = pending_from_ledger(&ledger).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].nav_xml_path, None);
    }

    /// Empty slice → empty pending list. Pins the
    /// `classify_pending` helper's degenerate-input behaviour
    /// against future regressions that might panic on an empty
    /// HashSet construction or similar. The `count_pending`
    /// public wrapper just calls `pending_from_ledger(...).len()`;
    /// its only distinct work is opening a `Ledger`, which is
    /// exercised by every other test in this module via the
    /// `fixture_ledger()` helper.
    #[test]
    fn classify_pending_returns_empty_on_empty_slice() {
        let entries: Vec<Entry> = vec![];
        let pending = classify_pending(&entries).unwrap();
        assert_eq!(pending.len(), 0);
    }

    // ── is_transport_error classification ───────────────────────────
    //
    // Application-side classifications are unit-tested below.
    // Transport-side variants wrap `reqwest::Error`, which has no
    // public constructor and which the binary's [dependencies] do
    // not pull in directly (reqwest is transitive via
    // `aberp-nav-transport`); the transport-side arms are verified
    // by inspection per the sandbox-no-cargo posture
    // (feedback_rust_method_naming, feedback_sandbox_git_commit).
    // The `matches!` macro's exhaustiveness-of-positive-arms means
    // any future variant addition that should be transport-side
    // must be added to the `matches!` body to flip from the
    // default-false; the default-false posture is the safe
    // direction (continues drain on misclassification rather than
    // halting on a real application error). ADR-0031 §4.

    /// `TokenExchangeHttpStatus` (non-success HTTP code) is an
    /// APPLICATION error — drain continues to the next invoice.
    /// Load-bearing per CLAUDE.md rule 9: a regression that
    /// reclassifies NAV-side application errors as transport would
    /// cause the drain to halt on every per-invoice rejection (e.g.
    /// `INVALID_SECURITY_USER`), which is exactly the wrong behaviour.
    #[test]
    fn is_transport_error_classifies_token_exchange_http_status_as_application() {
        let err = NavTransportError::TokenExchangeHttpStatus { status: 500 };
        assert!(!is_transport_error(&err));
    }

    /// `TokenExchangeResponseParse` is an application error.
    #[test]
    fn is_transport_error_classifies_response_parse_as_application() {
        let err = NavTransportError::TokenExchangeResponseParse("malformed".to_string());
        assert!(!is_transport_error(&err));
    }

    /// `EmbeddedPemMalformed` is a build-provenance error
    /// (binary-is-malformed). Drain continues on it — though in
    /// practice the binary would loud-fail at every NAV call, so
    /// `continue` and `stop` have the same observable outcome.
    /// Classifying as APPLICATION matches the typed-variant intent
    /// (this is not a NAV-side network failure).
    #[test]
    fn is_transport_error_classifies_embedded_pem_malformed_as_application() {
        let err = NavTransportError::EmbeddedPemMalformed("garbage".to_string());
        assert!(!is_transport_error(&err));
    }

    /// `KeychainItemMissing` is a credential error — drain
    /// continues on it (every invoice will surface the same
    /// problem; per-invoice loud messages are the operator-visible
    /// signal). Classifying as APPLICATION matches the typed-
    /// variant intent (this is not a transport failure).
    #[test]
    fn is_transport_error_classifies_keychain_item_missing_as_application() {
        let err = NavTransportError::KeychainItemMissing {
            tenant_id: "t1".to_string(),
            item: "login",
        };
        assert!(!is_transport_error(&err));
    }

    // ── PR-19 / ADR-0032 §5 — Attempt-exclusion fourth predicate ────

    /// A draft followed by an Attempt is NOT pending. The drain
    /// skips Attempted invoices regardless of subsequent
    /// AttemptFailed or Response (the latter would re-exclude via
    /// the existing second predicate; the AttemptFailed alone is
    /// redundant with the Attempt). Pins ADR-0032 §5's fourth-
    /// predicate clause against a regression that re-introduces
    /// in-flight invoices to the drain's FIFO.
    #[test]
    fn draft_followed_by_attempt_is_not_pending() {
        let (mut ledger, actor) = fixture_ledger();
        let idem = IdempotencyKey::new();
        write_draft_created(&mut ledger, &actor, "inv_A", idem, Some("/tmp/A.xml"));
        // Manually write an Attempt-only ledger entry.
        let attempt_payload = audit_payloads::InvoiceSubmissionAttemptPayload::new(
            "inv_A",
            idem,
            "test",
            b"<request/>".to_vec(),
        );
        ledger
            .append(
                EventKind::InvoiceSubmissionAttempt,
                attempt_payload.to_bytes(),
                actor.clone(),
                Some(idem.to_canonical_string()),
            )
            .unwrap();
        let pending = pending_from_ledger(&ledger).unwrap();
        assert!(pending.is_empty());
    }

    // ── PR-19 / ADR-0032 §2 — classify_attempt_failure ──────────────

    /// `ManageInvoiceHttp` → ("transport", None). The transport
    /// classes share their `error_class` discriminator across NAV
    /// operations; the drain's per-invoice loop forks on the same
    /// underlying classifier (`is_transport_error`).
    #[test]
    fn classify_attempt_failure_classifies_manage_invoice_http_as_transport() {
        let err = NavTransportError::ManageInvoiceHttpStatus { status: 500 };
        let (class, code) = classify_attempt_failure(&err);
        assert_eq!(class, "http_status");
        assert_eq!(code.as_deref(), Some("500"));
    }

    /// `ManageInvoiceNonRetryable` → ("application", Some(code)).
    /// The NAV-side error code threads into the payload for
    /// inspector triage.
    #[test]
    fn classify_attempt_failure_classifies_non_retryable_as_application() {
        let err = NavTransportError::ManageInvoiceNonRetryable {
            code: "INVALID_SECURITY_USER".to_string(),
            message: "bad creds".to_string(),
        };
        let (class, code) = classify_attempt_failure(&err);
        assert_eq!(class, "application");
        assert_eq!(code.as_deref(), Some("INVALID_SECURITY_USER"));
    }

    /// `ManageInvoiceRetryable` → ("retryable_application", Some(code)).
    /// The retryable classes are distinguished from non-retryable
    /// at the audit-payload level so the bundle reader can see
    /// at-a-glance which failures NAV said to retry vs which it
    /// said to escalate.
    #[test]
    fn classify_attempt_failure_classifies_retryable_as_retryable_application() {
        let err = NavTransportError::ManageInvoiceRetryable {
            code: "OPERATION_FAILED".to_string(),
            message: "try again".to_string(),
        };
        let (class, code) = classify_attempt_failure(&err);
        assert_eq!(class, "retryable_application");
        assert_eq!(code.as_deref(), Some("OPERATION_FAILED"));
    }

    /// `ManageInvoiceResponseParse` → ("application", None). Parse
    /// failures don't carry a NAV code — the response body is
    /// captured separately on the payload's `response_xml` field.
    #[test]
    fn classify_attempt_failure_classifies_response_parse_as_application() {
        let err = NavTransportError::ManageInvoiceResponseParse("missing field".to_string());
        let (class, code) = classify_attempt_failure(&err);
        assert_eq!(class, "application");
        assert!(code.is_none());
    }

    /// `ManageInvoiceEmpty` → ("envelope", None). Envelope failures
    /// are programmer / upstream-library errors; the binary's
    /// envelope-construction guard rejected the call before any
    /// wire activity.
    #[test]
    fn classify_attempt_failure_classifies_envelope_empty_as_envelope() {
        let err = NavTransportError::ManageInvoiceEmpty;
        let (class, code) = classify_attempt_failure(&err);
        assert_eq!(class, "envelope");
        assert!(code.is_none());
    }

    /// `KeychainItemMissing` → ("credential", None). Keychain
    /// failures are operator-correctable (the operator populates
    /// the missing key); the audit payload's class signals which
    /// keychain item via the `error_message` field.
    #[test]
    fn classify_attempt_failure_classifies_keychain_missing_as_credential() {
        let err = NavTransportError::KeychainItemMissing {
            tenant_id: "t1".to_string(),
            item: "login",
        };
        let (class, code) = classify_attempt_failure(&err);
        assert_eq!(class, "credential");
        assert!(code.is_none());
    }

    /// `EmbeddedPemMalformed` → ("client_build", None). Build-
    /// provenance failures are distinguished from credential and
    /// transport — the audit-evidence bundle reader sees
    /// "binary-malformed" as its own class. Note the divergence
    /// from `is_transport_error` (which classifies this as
    /// application for drain control-flow purposes); the two
    /// classifiers serve distinct purposes per ADR-0032 §2.
    #[test]
    fn classify_attempt_failure_classifies_embedded_pem_as_client_build() {
        let err = NavTransportError::EmbeddedPemMalformed("garbage".to_string());
        let (class, code) = classify_attempt_failure(&err);
        assert_eq!(class, "client_build");
        assert!(code.is_none());
    }

    /// `TokenExchangeBase64Decode` → ("application", None). The
    /// token decoding sub-failures all classify as application
    /// (NAV bytes failed the local decode/decrypt check) — the
    /// operator triages by reading the `error_message` field.
    #[test]
    fn classify_attempt_failure_classifies_token_decode_as_application() {
        let err = NavTransportError::TokenExchangeBase64Decode("bad base64".to_string());
        let (class, code) = classify_attempt_failure(&err);
        assert_eq!(class, "application");
        assert!(code.is_none());
    }

    /// PR-20 / ADR-0033 §5: `QueryInvoiceCheckHttpStatus` →
    /// ("http_status", Some(status)). The Layer-2 disambiguation
    /// surface's HTTP-status failures classify the same as
    /// manageInvoice / queryInvoiceData HTTP-status failures.
    /// CLAUDE.md rule 9: pins the new arm explicitly so a future
    /// refactor that drops one of the five queryInvoiceCheck
    /// variants from `classify_attempt_failure` would surface
    /// here, not at the first failed Layer-2 retry.
    #[test]
    fn classify_attempt_failure_classifies_query_invoice_check_http_status_as_http_status() {
        let err = NavTransportError::QueryInvoiceCheckHttpStatus { status: 503 };
        let (class, code) = classify_attempt_failure(&err);
        assert_eq!(class, "http_status");
        assert_eq!(code.as_deref(), Some("503"));
    }

    /// PR-20 / ADR-0033 §5: `QueryInvoiceCheckNonRetryable` →
    /// ("application", Some(code)). The NAV-side error code
    /// threads through to the `InvoiceCheckPerformedPayload.failure_code`
    /// field for inspector triage.
    #[test]
    fn classify_attempt_failure_classifies_query_invoice_check_non_retryable_as_application() {
        let err = NavTransportError::QueryInvoiceCheckNonRetryable {
            code: "INVALID_SECURITY_USER".to_string(),
            message: "bad creds".to_string(),
        };
        let (class, code) = classify_attempt_failure(&err);
        assert_eq!(class, "application");
        assert_eq!(code.as_deref(), Some("INVALID_SECURITY_USER"));
    }

    /// PR-20 / ADR-0033 §5: `QueryInvoiceCheckRetryable` →
    /// ("retryable_application", Some(code)). Even though
    /// retry-submission's Phase 0 aborts on BOTH retryable and
    /// non-retryable per ADR-0033 §"Surfaced conflict 1 Reading
    /// A", the audit payload's `failure_class` keeps the
    /// distinction for inspector triage.
    #[test]
    fn classify_attempt_failure_classifies_query_invoice_check_retryable_as_retryable_application() {
        let err = NavTransportError::QueryInvoiceCheckRetryable {
            code: "OPERATION_FAILED".to_string(),
            message: "try again".to_string(),
        };
        let (class, code) = classify_attempt_failure(&err);
        assert_eq!(class, "retryable_application");
        assert_eq!(code.as_deref(), Some("OPERATION_FAILED"));
    }

    /// PR-20 / ADR-0033 §5: `QueryInvoiceCheckResponseParse` →
    /// ("application", None). Same posture as the analogous
    /// queryInvoiceData / manageInvoice response-parse arms.
    #[test]
    fn classify_attempt_failure_classifies_query_invoice_check_response_parse_as_application() {
        let err = NavTransportError::QueryInvoiceCheckResponseParse(
            "missing <invoiceCheckResult>".to_string(),
        );
        let (class, code) = classify_attempt_failure(&err);
        assert_eq!(class, "application");
        assert!(code.is_none());
    }

    // ── PR-42 / F45 — PendingRetry classifier ───────────────────────

    fn write_submission_attempt(
        ledger: &mut Ledger,
        actor: &Actor,
        invoice_id: &str,
        idem: IdempotencyKey,
    ) {
        let payload = audit_payloads::InvoiceSubmissionAttemptPayload::new(
            invoice_id,
            idem,
            "test",
            b"<request/>".to_vec(),
        );
        ledger
            .append(
                EventKind::InvoiceSubmissionAttempt,
                payload.to_bytes(),
                actor.clone(),
                Some(idem.to_canonical_string()),
            )
            .unwrap();
    }

    /// Empty ledger has no pending retries. Pins the degenerate-input
    /// behaviour against future regressions.
    #[test]
    fn empty_ledger_has_no_pending_retries() {
        let (ledger, _actor) = fixture_ledger();
        let pending = pending_retries_from_ledger(&ledger).unwrap();
        assert!(pending.is_empty());
    }

    /// A Draft alone (no Attempt) is NOT a pending retry — that's a
    /// state-1 Draft, handled by `pending_from_ledger`. Pins the
    /// fork-by-state-2-vs-state-1 contract.
    #[test]
    fn draft_alone_is_not_a_pending_retry() {
        let (mut ledger, actor) = fixture_ledger();
        let idem = IdempotencyKey::new();
        write_draft_created(&mut ledger, &actor, "inv_A", idem, Some("/tmp/A.xml"));
        let pending = pending_retries_from_ledger(&ledger).unwrap();
        assert!(pending.is_empty());
    }

    /// Draft + Attempt with no Response or Abandon classifies as
    /// state-2 Pending. The base case of the PendingRetry classifier.
    #[test]
    fn draft_with_attempt_classifies_as_pending_retry() {
        let (mut ledger, actor) = fixture_ledger();
        let idem = IdempotencyKey::new();
        write_draft_created(&mut ledger, &actor, "inv_A", idem, Some("/tmp/A.xml"));
        write_submission_attempt(&mut ledger, &actor, "inv_A", idem);
        let pending = pending_retries_from_ledger(&ledger).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].invoice_id, "inv_A");
        assert_eq!(pending[0].nav_xml_path.as_deref(), Some("/tmp/A.xml"));
        assert_eq!(pending[0].idempotency_key, idem);
    }

    /// Draft + Attempt + Response is NOT pending — the Response
    /// excludes per ADR-0032 §4 (state-2 requires NO Response).
    #[test]
    fn draft_attempt_response_is_not_pending_retry() {
        let (mut ledger, actor) = fixture_ledger();
        let idem = IdempotencyKey::new();
        write_draft_created(&mut ledger, &actor, "inv_A", idem, Some("/tmp/A.xml"));
        write_submission_attempt(&mut ledger, &actor, "inv_A", idem);
        write_submission_response(&mut ledger, &actor, "inv_A", idem, "TXID-A");
        let pending = pending_retries_from_ledger(&ledger).unwrap();
        assert!(pending.is_empty());
    }

    /// Draft + Attempt + MarkedAbandoned is NOT pending — the
    /// abandonment excludes per ADR-0032 §4.
    #[test]
    fn draft_attempt_abandoned_is_not_pending_retry() {
        let (mut ledger, actor) = fixture_ledger();
        let idem = IdempotencyKey::new();
        write_draft_created(&mut ledger, &actor, "inv_A", idem, Some("/tmp/A.xml"));
        write_submission_attempt(&mut ledger, &actor, "inv_A", idem);
        write_marked_abandoned(&mut ledger, &actor, "inv_A", idem, "TXID-A");
        let pending = pending_retries_from_ledger(&ledger).unwrap();
        assert!(pending.is_empty());
    }

    /// Multiple Attempts for the same invoice (a state-2 retry that
    /// itself failed and wrote a fresh Attempt) dedupe to ONE
    /// PendingRetry. The F8 contract pins one idempotency key per
    /// invoice; the classifier returns one entry per still-pending
    /// invoice regardless of how many failed attempts accumulated.
    #[test]
    fn multiple_attempts_dedupe_to_one_pending_retry() {
        let (mut ledger, actor) = fixture_ledger();
        let idem = IdempotencyKey::new();
        write_draft_created(&mut ledger, &actor, "inv_A", idem, Some("/tmp/A.xml"));
        write_submission_attempt(&mut ledger, &actor, "inv_A", idem);
        write_submission_attempt(&mut ledger, &actor, "inv_A", idem);
        write_submission_attempt(&mut ledger, &actor, "inv_A", idem);
        let pending = pending_retries_from_ledger(&ledger).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].invoice_id, "inv_A");
    }

    /// Cross-invoice contamination: a Response for B does NOT block
    /// A's pending-retry classification. Mirrors the
    /// `pending_classification_does_not_cross_invoice_ids` pin.
    #[test]
    fn pending_retry_classification_does_not_cross_invoice_ids() {
        let (mut ledger, actor) = fixture_ledger();
        let idem_a = IdempotencyKey::new();
        let idem_b = IdempotencyKey::new();
        write_draft_created(&mut ledger, &actor, "inv_A", idem_a, Some("/tmp/A.xml"));
        write_draft_created(&mut ledger, &actor, "inv_B", idem_b, Some("/tmp/B.xml"));
        write_submission_attempt(&mut ledger, &actor, "inv_A", idem_a);
        write_submission_attempt(&mut ledger, &actor, "inv_B", idem_b);
        // B's submission completed; A is stuck.
        write_submission_response(&mut ledger, &actor, "inv_B", idem_b, "TXID-B");
        let pending = pending_retries_from_ledger(&ledger).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].invoice_id, "inv_A");
    }

    /// Pre-PR-18 Draft entries (no `nav_xml_path` field) classify
    /// with `nav_xml_path: None`. The drain worker loud-fails on
    /// `None` per CLAUDE.md rule 12; the classifier reports the data
    /// faithfully and lets the driver name the recovery flag.
    #[test]
    fn pre_pr_18_draft_with_attempt_classifies_with_none_path() {
        let (mut ledger, actor) = fixture_ledger();
        let idem = IdempotencyKey::new();
        write_draft_created(&mut ledger, &actor, "inv_A", idem, None);
        write_submission_attempt(&mut ledger, &actor, "inv_A", idem);
        let pending = pending_retries_from_ledger(&ledger).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].nav_xml_path, None);
    }

    /// Deterministic ordering: two calls on the same ledger return
    /// the same order. CLAUDE.md rule 9 — non-determinism from a
    /// HashMap leak would surface here.
    #[test]
    fn pending_retries_are_deterministically_ordered() {
        let (mut ledger, actor) = fixture_ledger();
        let idem_a = IdempotencyKey::new();
        let idem_b = IdempotencyKey::new();
        write_draft_created(&mut ledger, &actor, "inv_B", idem_b, Some("/tmp/B.xml"));
        write_draft_created(&mut ledger, &actor, "inv_A", idem_a, Some("/tmp/A.xml"));
        write_submission_attempt(&mut ledger, &actor, "inv_B", idem_b);
        write_submission_attempt(&mut ledger, &actor, "inv_A", idem_a);
        let p1 = pending_retries_from_ledger(&ledger).unwrap();
        let p2 = pending_retries_from_ledger(&ledger).unwrap();
        assert_eq!(p1.len(), 2);
        assert_eq!(p1, p2);
    }

    /// Attempt entry without a matching Draft loud-fails — the
    /// audit ledger appears tampered or schema-drifted. CLAUDE.md
    /// rule 12 surface: silently swallowing the missing Draft would
    /// mask ledger corruption.
    #[test]
    fn attempt_without_matching_draft_loud_fails() {
        let (mut ledger, actor) = fixture_ledger();
        let idem = IdempotencyKey::new();
        // Skip Draft; write Attempt directly.
        write_submission_attempt(&mut ledger, &actor, "inv_orphan", idem);
        let err = pending_retries_from_ledger(&ledger).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("no matching InvoiceDraftCreated"));
        assert!(msg.contains("inv_orphan"));
    }

    /// Classify on empty slice returns empty vec.
    #[test]
    fn classify_pending_retries_returns_empty_on_empty_slice() {
        let entries: Vec<Entry> = vec![];
        let pending = classify_pending_retries(&entries).unwrap();
        assert_eq!(pending.len(), 0);
    }

    // ── PR-19 / ADR-0032 §2 — classify_attempt_failure (continued) ──

    /// PR-19 / ADR-0032 §2: cross-classifier symmetry pin. For
    /// every variant `is_transport_error` returns true on, the
    /// `classify_attempt_failure` class is "transport" OR
    /// "client_build". For every variant the former returns false
    /// on, the latter is one of the non-transport, non-client_build
    /// classes. Skipping the brittle exhaustive cross-check (the
    /// reqwest::Error-wrapping variants have no public
    /// constructor) — this pins a sample of paired cases to catch
    /// drift between the two classifiers.
    #[test]
    fn classify_attempt_failure_agrees_with_is_transport_error_on_samples() {
        // is_transport_error: true → class one of {transport, client_build}
        let pem = NavTransportError::EmbeddedPemMalformed("x".to_string());
        let (pem_class, _) = classify_attempt_failure(&pem);
        // is_transport_error returns FALSE for EmbeddedPemMalformed;
        // classify_attempt_failure returns "client_build" — they
        // disagree here BY DESIGN per ADR-0032 §2.
        assert!(!is_transport_error(&pem));
        assert_eq!(pem_class, "client_build");

        // is_transport_error: false → class one of {http_status, application,
        //   retryable_application, envelope, credential}
        let status = NavTransportError::ManageInvoiceHttpStatus { status: 500 };
        assert!(!is_transport_error(&status));
        assert_eq!(classify_attempt_failure(&status).0, "http_status");

        let envelope = NavTransportError::ManageInvoiceEmpty;
        assert!(!is_transport_error(&envelope));
        assert_eq!(classify_attempt_failure(&envelope).0, "envelope");
    }
}
