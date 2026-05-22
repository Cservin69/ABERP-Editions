//! Orchestration for the `aberp issue-storno` subcommand (PR-10,
//! ADR-0009 §6, ADR-0023).
//!
//! Pipeline (mirrors `issue_invoice.rs`'s shape — same `run` /
//! `run_single_tx` split, same idempotency-replay branch, same
//! drop-then-reopen pattern for the post-commit chain verification):
//!
//! 1. Parse the JSON input into a [`crate::issue_invoice::InvoiceInputJson`]
//!    struct — the storno's own line content uses the same JSON shape
//!    as `issue-invoice --in` (ADR-0023 §1).
//! 2. Resolve tenant id and series code (loud-fail on invalid input).
//! 3. **Load NAV credentials from the OS keychain.** Same posture as
//!    `issue_invoice.rs` step 3 — required for the operator's session
//!    identity baked into every audit-ledger entry via
//!    [`Actor::from_local_cli`] (closes F15). `issue-storno` does
//!    not call NAV (ADR-0023 §1), but the actor identity discipline
//!    is the same.
//! 4. Compute the binary hash and build [`LedgerMeta`].
//! 5. **Pre-flight precondition check** — walk the audit ledger (via a
//!    fresh `Ledger::open`, no tx needed since we're only reading
//!    historical state) and confirm the `--references` target carries
//!    a terminal-positive `InvoiceAckStatus` of `"SAVED"` (ADR-0023
//!    §1). Loud-fail if not — `NeverSubmitted`, `Stuck`, `Rejected`,
//!    or `Abandoned` bases cannot be stornoed in PR-10 scope.
//! 6. Pre-tx setup (idempotent): ensure billing schema, ensure series
//!    exists, hand back the Connection, ensure audit-ledger schema.
//! 7. Open a single DuckDB transaction; under it:
//!    - Load the base invoice's row via [`billing::load_ready_invoice_by_id`]
//!      so we capture its NAV-facing `sequence_number` for the chain
//!      payload's `base_sequence_number` field (denormalized by design
//!      — see ADR-0023 §3 + Adversarial review #2).
//!    - Walk the `audit_ledger` table inside the SAME transaction for
//!      every prior `InvoiceStornoIssued` payload pointing at the
//!      same base; allocate `modification_index = max + 1` (or 1 if
//!      empty). ADR-0023 §4 same-transaction rule — guards the
//!      cross-process race the adversarial review #1 names.
//!    - Call [`billing::allocate_in_tx`] to burn the storno's own
//!      sequence number + write the storno's reservation + invoice
//!      rows (same path as `issue_invoice.rs`).
//!    - On the `Fresh` branch, write THREE audit-ledger entries:
//!      `InvoiceSequenceReserved`, `InvoiceDraftCreated`, and the
//!      chain-link `InvoiceStornoIssued`. All three share the
//!      storno's idempotency key and are appended in this same `tx`.
//!    - Commit.
//! 8. Drop the Connection, re-open a fresh `Ledger`, and verify the
//!    chain — same success-criterion gate as `issue_invoice.rs`.
//! 9. Render the storno's `<InvoiceData>` XML via
//!    [`nav_xml::render_storno_data`] — negated amounts plus the
//!    `<invoiceReference>` chain block. Run the ADR-0022 runtime XSD
//!    invariant check; on failure, the audit entries from the
//!    pre-commit step remain in the ledger (per the same recovery
//!    posture `issue_invoice.rs` documents) and the operator re-runs
//!    after fixing the emitter or input.
//! 10. Write the XML to disk and print the storno's invoice number +
//!     chain link.
//!
//! # Why this is its own module (not extending `issue_invoice.rs`)
//!
//! The two paths share allocator semantics + the `Fresh` / `Replay`
//! branch, but they differ in:
//!
//!   - The audit-payload set (storno writes a third chain-link entry).
//!   - The precondition check (storno requires a SAVED base; issue
//!     has no such precondition).
//!   - The XML emitter (storno emits `<invoiceReference>` + negated
//!     amounts; issue emits a fresh invoice).
//!   - Operator-visible output (storno prints the chain link).
//!
//! Forcing both through one `run_with_optional_storno_reference` would
//! be the speculative-abstraction trap CLAUDE.md rule 2 names. The
//! shared shape lives in the call sequence (steps 1–4 + 6–8) but the
//! per-step contents diverge enough that a parallel module reads
//! cleaner. Future MODIFY PR (PR-11/PR-12) gets a third sibling
//! module `issue_modification.rs` with the same parallel shape.
//!
//! # `modification_index` allocator — local-base path only (PR-10)
//!
//! ADR-0023 §4 names two paths for chain-index allocation:
//!
//! 1. **Local base** (ABERP issued the base itself): walk the local
//!    audit ledger, take `max(modification_index for this base) + 1`.
//!    THIS path is what PR-10 implements.
//! 2. **Migrated-from-Billingo base**: call NAV's
//!    `queryInvoiceChainDigest` to learn the canonical chain (Billingo
//!    may have issued amendments ABERP has no local record of).
//!    Deferred — the local invoice schema has no `origin` column
//!    today, so the conditional in ADR-0023 §4 never fires. Named
//!    trigger for the migrated-base path: the first PR that lands the
//!    Billingo migration read (ADR-0010 build phase).

use std::path::Path;

use aberp_audit_ledger::{
    self as audit_ledger, Actor, EventKind, Ledger, LedgerMeta, TenantId,
};
use aberp_billing::{
    self as billing, AllocateArgs, AllocateOutcome, BillingStore, CustomerId, DraftInvoice,
    DuckDbBillingStore, Huf, IdempotencyKey, InvoiceId, InvoiceSeries, IssueInvoiceCommand,
    LineItem, ReadyInvoice, ResetPolicy, SeriesCode, SeriesId,
};
use aberp_nav_transport::NavCredentials;
use anyhow::{anyhow, bail, Context, Result};
use duckdb::Connection;
use time::OffsetDateTime;
use ulid::Ulid;

use crate::audit_payloads;
use crate::binary_hash;
use crate::cli::IssueStornoArgs;
use crate::issue_invoice::InvoiceInputJson;
use crate::nav_xml::{self, CustomerInfo, NavParties, StornoReference, SupplierInfo};

// ──────────────────────────────────────────────────────────────────────
// Entry point
// ──────────────────────────────────────────────────────────────────────

pub fn run(args: &IssueStornoArgs) -> Result<()> {
    let _span = tracing::info_span!("issue_storno").entered();

    // 1. Read + parse the JSON input. Shape mirrors `issue-invoice --in`.
    let input_bytes = std::fs::read(&args.r#in)
        .with_context(|| format!("read input JSON from {}", args.r#in.display()))?;
    let input: InvoiceInputJson =
        serde_json::from_slice(&input_bytes).context("parse input JSON")?;
    tracing::info!(lines = input.lines.len(), "JSON input parsed");

    if input.lines.is_empty() {
        return Err(anyhow!("input JSON has no lines"));
    }

    // 2. Resolve tenant id + series code (loud-fail on invalid input).
    let tenant = TenantId::new(args.tenant.clone()).ok_or_else(|| {
        anyhow!(
            "--tenant value '{}' is empty or has a null byte",
            args.tenant
        )
    })?;
    let series_code = SeriesCode::new(args.series.clone()).ok_or_else(|| {
        anyhow!(
            "--series value '{}' fails SeriesCode validation",
            args.series
        )
    })?;

    // Validate the --references shape minimally up-front. The full
    // existence + finalized check happens in step 5 (audit-ledger
    // walk) and step 7 (DB row load); a malformed prefix is cheaper
    // to reject here than to discover via a "no such invoice" load.
    if !args.references.starts_with("inv_") {
        bail!(
            "--references value '{}' is not a prefixed invoice id (expected inv_<ULID>)",
            args.references
        );
    }

    // 3. Load NAV credentials BEFORE any DB write — same Actor-identity
    //    discipline as `issue_invoice.rs` step 3, closes F15.
    let credentials = NavCredentials::load_from_keychain(&args.tenant)
        .context("load NAV credentials from OS keychain")?;
    let session_id = Ulid::new().to_string();
    let actor = Actor::from_local_cli(session_id, credentials.login());
    tracing::info!(
        tenant = %args.tenant,
        session_id = %actor.session_id,
        user_id = %actor.user_id,
        "NAV credentials loaded; actor derived for this CLI invocation"
    );

    // 4. Compute binary hash + ledger meta. Cloned per-append.
    let binary_hash_bytes = binary_hash::compute().context("compute binary hash")?;
    let ledger_meta = LedgerMeta::new(tenant.clone(), binary_hash_bytes);

    // 4a. PR-18 / ADR-0031 §5 — pre-allocation hard-cap check.
    //     A storno burns its own sequence number, so it counts
    //     against the same ADR-0009 §7 backlog as a fresh invoice.
    //     Loud-fail before any allocator tx opens so the
    //     sequence-slot invariant is preserved.
    let pending_count = crate::submission_queue::count_pending(
        &args.db,
        tenant.clone(),
        binary_hash_bytes,
    )
    .context("count pending submissions (ADR-0031 §5 cap check) for storno")?;
    if pending_count >= crate::submission_queue::HARD_CAP_PENDING {
        return Err(anyhow!(
            "submission queue is full ({}/{} pending invoices per ADR-0009 §7 / ADR-0031 §5); \
             run `aberp drain-submission-queue --endpoint <test|production> --tax-number ...` \
             to submit the backlog before issuing a storno",
            pending_count,
            crate::submission_queue::HARD_CAP_PENDING,
        ));
    }

    // 5. Pre-flight precondition: base must be Finalized (last ack =
    //    SAVED). Open a fresh Ledger for the read; close it before
    //    opening the write transaction so the file lock is released.
    {
        let ledger = Ledger::open(&args.db, tenant.clone(), binary_hash_bytes)
            .context("open audit ledger for storno precondition check")?;
        check_base_is_finalized(&ledger, &args.references)?;
        // ledger drops here, releasing the DuckDB read connection
    }

    // 6. Pre-tx setup: schemas + series. Reuses the helper shape
    //    `issue_invoice.rs` uses (kept inlined here to avoid a
    //    speculative shared-helper extraction — rule 2).
    let (conn, series) = pre_tx_setup(&args.db, &series_code)?;

    // 7. Build the IssueInvoiceCommand for the STORNO's own content
    //    + AllocateArgs. The storno burns its own sequence number;
    //    the chain link to the base lives in the audit-ledger
    //    chain-link payload, not in the billing row.
    let command = build_storno_command(&input, &series_code)?;
    let idempotency_key = command.idempotency_key;
    let issue_date = OffsetDateTime::now_utc();
    let draft = DraftInvoice {
        id: InvoiceId::new(),
        series_id: series.id,
        customer_id: command.customer_id,
        lines: command.lines,
        issue_date,
    };
    let allocate_args = AllocateArgs {
        series_id: series.id,
        draft,
        idempotency_key,
    };

    // 8. One transaction across base-load + chain-index walk + storno
    //    allocator + three audit-ledger appends.
    //
    //    PR-18 / ADR-0031 §2: thread the storno's --out path so the
    //    InvoiceDraftCreated payload records where the storno XML
    //    will be written.
    let outcome = run_single_tx(
        conn,
        &ledger_meta,
        allocate_args,
        idempotency_key,
        actor,
        &args.references,
        args.out.clone(),
    )?;

    let storno = outcome.storno;
    let modification_index = outcome.modification_index;
    let base_sequence_number = outcome.base_sequence_number;
    let was_fresh = outcome.was_fresh;
    tracing::info!(
        seq = storno.sequence_number,
        modification_index,
        base_sequence_number,
        fresh = was_fresh,
        idempotency_key = ?idempotency_key,
        "storno issued"
    );

    // 9. Verify the audit chain — success-criterion gate.
    let ledger = Ledger::open(&args.db, tenant.clone(), binary_hash_bytes)
        .context("re-open audit ledger after storno commit")?;
    let verified = ledger
        .verify_chain()
        .context("audit-ledger chain verification failed AFTER storno issuance")?;
    tracing::info!(entries_verified = verified, "audit chain verified");

    // 9a. PR-17 / ADR-0030 §2 — sync the audit-ledger mirror file
    //     post-commit (matches the issue_invoice posture).
    let mirror_path = audit_ledger::mirror_path_for(&args.db);
    ledger
        .sync_mirror(&mirror_path)
        .context("sync audit-ledger mirror file after storno commit")?;

    // 10. Render the storno's <InvoiceData> XML with negated amounts +
    //     <invoiceReference> chain block. Then run ADR-0022's runtime
    //     XSD invariant check before writing to disk.
    let parties = NavParties {
        supplier: SupplierInfo {
            tax_number: input.supplier.tax_number,
            name: input.supplier.name,
            address_country_code: input.supplier.address.country_code,
            address_postal_code: input.supplier.address.postal_code,
            address_city: input.supplier.address.city,
            address_street: input.supplier.address.street,
        },
        customer: CustomerInfo {
            tax_number: input.customer.tax_number,
            name: input.customer.name,
        },
    };
    // The base's invoice-number-on-NAV is `<series>/<5-digit-seq>` —
    // we don't know the base's *series* directly without another DB
    // hop; for PR-10 we use the same series as the storno (the
    // operator-default), which matches the storno-against-same-series
    // case the ADR §1 default covers. The override path (operator
    // sets a dedicated storno series) makes this denormalization
    // wrong — surfaced as F22 in the PR-10 commit message; for PR-10
    // the operator running issue-storno with --series different from
    // the base's series is out of the supported surface (and the
    // precondition check above does not load the base's series_id
    // because we don't need it for chain-index allocation).
    let base_invoice_number = format!(
        "{}/{:05}",
        series_code.as_str(),
        base_sequence_number
    );
    let storno_reference = StornoReference {
        base_invoice_number,
        modification_index,
    };
    let xml = nav_xml::render_storno_data(&storno, &series_code, &parties, &storno_reference)
        .context("render NAV storno XML")?;
    aberp_nav_xsd_validator::validate_invoice_data(&xml)
        .context("NAV InvoiceData v3.0 invariant check (ADR-0022) failed for rendered storno XML")?;
    tracing::info!(
        bytes = xml.len(),
        nav_xsd_version = aberp_nav_xsd_validator::NAV_XSD_VERSION,
        "NAV storno InvoiceData XML passed v3.0 invariant check"
    );
    nav_xml::write_to_path(&args.out, &xml)?;
    tracing::info!(path = %args.out.display(), bytes = xml.len(), "NAV storno XML written");

    println!(
        "issued storno {}/{:05} -> {} (references {} as modificationIndex {}, audit chain verified across {} entries)",
        series_code.as_str(),
        storno.sequence_number,
        args.out.display(),
        args.references,
        modification_index,
        verified,
    );
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────
// Pre-flight precondition: base invoice must be Finalized (last ack
// status = "SAVED"). Mirrors `audit_query::stuck_precondition`'s
// classifier walk but answers a different question.
// ──────────────────────────────────────────────────────────────────────

/// Walk the audit ledger and confirm that `base_invoice_id` is in the
/// local-typestate-equivalent of `Finalized` per ADR-0009 §2 — i.e.
/// the most-recent `InvoiceAckStatus` payload for it carries
/// `ack_status = "SAVED"` and no `InvoiceMarkedAbandoned` follows.
///
/// Loud-fail with a specific named-reason message per CLAUDE.md
/// rule 12; the operator's first read of the error tells them what
/// to do next (issue corrective, run poll-ack, etc.).
fn check_base_is_finalized(ledger: &Ledger, base_invoice_id: &str) -> Result<()> {
    let entries = ledger
        .entries()
        .context("read audit ledger entries for storno precondition check")?;

    // Track per-base flags as we walk newest → oldest is more
    // efficient, but we need the LAST ack and the existence of an
    // abandoned. Walk forward once; record both.
    let mut has_marked_abandoned = false;
    let mut latest_ack_status: Option<String> = None;
    let mut has_submission_response = false;

    for entry in &entries {
        match entry.kind {
            EventKind::InvoiceMarkedAbandoned => {
                if payload_invoice_id_matches::<audit_payloads::InvoiceMarkedAbandonedPayload>(
                    &entry.payload,
                    base_invoice_id,
                    "InvoiceMarkedAbandoned",
                    entry.seq.as_u64(),
                )? {
                    has_marked_abandoned = true;
                }
            }
            EventKind::InvoiceSubmissionResponse => {
                if payload_invoice_id_matches::<audit_payloads::InvoiceSubmissionResponsePayload>(
                    &entry.payload,
                    base_invoice_id,
                    "InvoiceSubmissionResponse",
                    entry.seq.as_u64(),
                )? {
                    has_submission_response = true;
                }
            }
            EventKind::InvoiceAckStatus => {
                // Decode + filter; only update the latest if it matches.
                let payload: audit_payloads::InvoiceAckStatusPayload =
                    serde_json::from_slice(&entry.payload).map_err(|e| {
                        anyhow!(
                            "InvoiceAckStatus audit payload (seq {}) failed typed decode: {e} \
                             — audit ledger appears tampered or schema-drifted",
                            entry.seq.as_u64()
                        )
                    })?;
                if payload.invoice_id == base_invoice_id {
                    latest_ack_status = Some(payload.ack_status);
                }
            }
            _ => {}
        }
    }

    if has_marked_abandoned {
        bail!(
            "base invoice {} is ABANDONED (operator previously ran \
             `aberp mark-abandoned`); no storno can be issued against it",
            base_invoice_id
        );
    }
    if !has_submission_response {
        bail!(
            "base invoice {} has no NAV submission response on record — \
             run `aberp submit-invoice` and `aberp poll-ack` first \
             to finalize it before issuing a storno",
            base_invoice_id
        );
    }
    match latest_ack_status.as_deref() {
        Some("SAVED") => Ok(()),
        Some("ABORTED") => bail!(
            "base invoice {} was REJECTED by NAV (last ack: ABORTED) — \
             a storno is only valid against a SAVED (finalized) invoice; \
             issue a corrective new invoice instead",
            base_invoice_id
        ),
        Some(other) => bail!(
            "base invoice {} is STUCK (last ack: {}) — finalize it via \
             `aberp poll-ack` (or unblock via `aberp retry-submission`) \
             before issuing a storno; storno against a not-yet-finalized \
             invoice is rejected per ADR-0023 §1",
            base_invoice_id,
            other
        ),
        None => bail!(
            "base invoice {} has a submission response but no ack status — \
             run `aberp poll-ack` first; storno requires the base to be \
             finalized (NAV terminal SAVED) per ADR-0023 §1",
            base_invoice_id
        ),
    }
}

/// Decode a typed audit payload and return whether its `invoice_id`
/// field matches the target. Wraps the decode in a loud-fail error
/// message that names the seq + kind so a tampered ledger surfaces
/// the exact entry. Generic over the payload type so the four match
/// arms in `check_base_is_finalized` don't each open-code the same
/// `from_slice + map_err` shape.
fn payload_invoice_id_matches<P>(
    payload_bytes: &[u8],
    target_invoice_id: &str,
    kind_label: &'static str,
    seq: u64,
) -> Result<bool>
where
    P: serde::de::DeserializeOwned + HasInvoiceId,
{
    let payload: P = serde_json::from_slice(payload_bytes).map_err(|e| {
        anyhow!(
            "{kind_label} audit payload (seq {seq}) failed typed decode: {e} \
             — audit ledger appears tampered or schema-drifted"
        )
    })?;
    Ok(payload.invoice_id_field() == target_invoice_id)
}

/// Tiny accessor trait so [`payload_invoice_id_matches`] can be
/// generic without depending on every payload type carrying a public
/// `invoice_id` field directly. Implemented for the two payload
/// types `check_base_is_finalized` walks where the `invoice_id`
/// shape matters; `InvoiceAckStatus` is handled inline (because it
/// also reads `ack_status`).
trait HasInvoiceId {
    fn invoice_id_field(&self) -> &str;
}

impl HasInvoiceId for audit_payloads::InvoiceMarkedAbandonedPayload {
    fn invoice_id_field(&self) -> &str {
        &self.invoice_id
    }
}

impl HasInvoiceId for audit_payloads::InvoiceSubmissionResponsePayload {
    fn invoice_id_field(&self) -> &str {
        &self.invoice_id
    }
}

// ──────────────────────────────────────────────────────────────────────
// Pre-tx setup — same shape as issue_invoice.rs
// ──────────────────────────────────────────────────────────────────────

fn pre_tx_setup(db_path: &Path, series_code: &SeriesCode) -> Result<(Connection, InvoiceSeries)> {
    let mut billing = DuckDbBillingStore::open(db_path)
        .with_context(|| format!("open billing DuckDB at {}", db_path.display()))?;
    billing.ensure_schema().context("ensure billing schema")?;
    let series = ensure_series(&mut billing, series_code)?;
    let conn = billing.into_connection();
    audit_ledger::ensure_schema(&conn).context("ensure audit-ledger schema")?;
    Ok((conn, series))
}

fn ensure_series<S: BillingStore + ?Sized>(
    store: &mut S,
    code: &SeriesCode,
) -> Result<InvoiceSeries> {
    if let Some(series) = store.find_series_by_code(code)? {
        return Ok(series);
    }
    let series = InvoiceSeries {
        id: SeriesId::new(),
        code: code.clone(),
        reset_policy: ResetPolicy::Never,
        fiscal_year: None,
        created_at: OffsetDateTime::now_utc(),
    };
    store.create_series(&series).context("create series")?;
    tracing::info!(series = code.as_str(), "auto-created series");
    Ok(series)
}

// ──────────────────────────────────────────────────────────────────────
// The single transaction — base load, chain-index allocation, storno
// allocator, three audit appends.
// ──────────────────────────────────────────────────────────────────────

/// Outcome the caller needs after commit.
struct TxOutcome {
    storno: ReadyInvoice,
    modification_index: u32,
    base_sequence_number: u64,
    was_fresh: bool,
}

/// Open one DuckDB transaction; under it: load the base invoice row,
/// walk the audit-ledger for the chain index, allocate the storno,
/// write the three audit entries, commit. Rollback contract matches
/// `issue_invoice::run_single_tx` (drop-on-error rolls back both
/// halves; `apps/aberp/tests/rollback_conformance.rs` exercises the
/// shape).
fn run_single_tx(
    mut conn: Connection,
    ledger_meta: &LedgerMeta,
    allocate_args: AllocateArgs,
    idempotency_key: IdempotencyKey,
    actor: Actor,
    base_invoice_id: &str,
    nav_xml_path: std::path::PathBuf,
) -> Result<TxOutcome> {
    let tx = conn
        .transaction()
        .context("begin DuckDB transaction (storno: billing + audit-ledger)")?;

    // (a) Load the base invoice's row so we capture its NAV-facing
    //     sequence number for the chain-link payload's
    //     `base_sequence_number` field (denormalized by design per
    //     ADR-0023 §3). Loud-fail if the row is absent — the audit
    //     ledger said the base was Finalized in step 5, so a missing
    //     row here would mean direct DB tampering between step 5 and
    //     step 7. CLAUDE.md rule 12.
    let (base_invoice, _base_idem) = billing::load_ready_invoice_by_id(&tx, base_invoice_id)
        .context("billing::load_ready_invoice_by_id (storno base)")?
        .ok_or_else(|| {
            anyhow!(
                "base invoice {} exists in audit ledger but not in billing table — \
                 tenant DB appears tampered between precondition check and storno tx",
                base_invoice_id
            )
        })?;
    let base_sequence_number = base_invoice.sequence_number;

    // (b) Walk the audit ledger inside the SAME tx for prior
    //     `InvoiceStornoIssued` payloads pointing at this base.
    //     Allocate modification_index = max + 1 (or 1 if empty).
    //     Same-tx walk is the ADR-0023 §4 cross-process-race close.
    let modification_index = next_modification_index_in_tx(&tx, base_invoice_id)?;

    // (c) Standard allocator path: burn the storno's own sequence
    //     number + write its reservation + invoice rows.
    let now = OffsetDateTime::now_utc();
    let outcome =
        billing::allocate_in_tx(&tx, allocate_args, now).context("billing::allocate_in_tx (storno)")?;

    let (storno_invoice, reservation, was_fresh) = match outcome {
        AllocateOutcome::Fresh {
            invoice,
            reservation,
        } => (invoice, reservation, true),
        AllocateOutcome::Replay {
            invoice,
            reservation,
        } => (invoice, reservation, false),
    };

    if was_fresh {
        let idem_str = idempotency_key.to_canonical_string();

        // 1) InvoiceSequenceReserved for the STORNO's own sequence.
        let seq_payload = audit_payloads::InvoiceSequenceReservedPayload::from_outcome(
            &storno_invoice,
            &reservation,
            idempotency_key,
        );
        audit_ledger::append_in_tx(
            &tx,
            ledger_meta,
            EventKind::InvoiceSequenceReserved,
            seq_payload.to_bytes(),
            actor.clone(),
            Some(idem_str.clone()),
        )
        .context("audit_ledger::append_in_tx InvoiceSequenceReserved (storno)")?;

        // 2) InvoiceDraftCreated for the STORNO. PR-18 / ADR-0031 §2
        //    — record the operator's --out path so the drain worker
        //    can submit without a per-invocation path argument.
        let draft_payload = audit_payloads::InvoiceDraftCreatedPayload::from_invoice_with_xml_path(
            &storno_invoice,
            idempotency_key,
            nav_xml_path,
        );
        audit_ledger::append_in_tx(
            &tx,
            ledger_meta,
            EventKind::InvoiceDraftCreated,
            draft_payload.to_bytes(),
            actor.clone(),
            Some(idem_str.clone()),
        )
        .context("audit_ledger::append_in_tx InvoiceDraftCreated (storno)")?;

        // 3) InvoiceStornoIssued — the chain-link payload.
        let storno_payload = audit_payloads::InvoiceStornoIssuedPayload::new(
            &storno_invoice.id.to_prefixed_string(),
            storno_invoice.sequence_number,
            &reservation.id.to_prefixed_string(),
            idempotency_key,
            base_invoice_id,
            base_sequence_number,
            modification_index,
        );
        audit_ledger::append_in_tx(
            &tx,
            ledger_meta,
            EventKind::InvoiceStornoIssued,
            storno_payload.to_bytes(),
            actor,
            Some(idem_str),
        )
        .context("audit_ledger::append_in_tx InvoiceStornoIssued")?;
    } else {
        tracing::info!("replay path: no new audit entries written (storno idempotency hit)");
    }

    tx.commit()
        .context("commit DuckDB transaction (storno: billing + audit-ledger)")?;
    Ok(TxOutcome {
        storno: storno_invoice,
        modification_index,
        base_sequence_number,
        was_fresh,
    })
}

/// Walk `audit_ledger` inside the borrowed transaction for every
/// chain entry (BOTH `InvoiceStornoIssued` AND
/// `InvoiceModificationIssued`), decode each payload, filter by
/// `base_invoice_id`, return `max(modification_index) + 1` — or `1`
/// if no prior chain entry exists.
///
/// Runs inside the caller's tx so concurrent commands against the
/// same base are serialized by DuckDB's single-writer file lock
/// (ADR-0009 §3). On the Postgres-per-tenant variant (ADR-0016) the
/// equivalent is a `SELECT ... FOR UPDATE` on the base row; PR-10's
/// DuckDB path needs no extra locking primitive.
///
/// **The walk considers BOTH chain kinds** per ADR-0024 §7: NAV's
/// uniqueness rule says `modificationIndex` is unique per
/// `invoiceReference` regardless of operation kind, so a storno-only
/// walk would re-issue an index already used by a prior MODIFY and
/// NAV would reject with `INVOICE_NUMBER_NOT_UNIQUE`-shape at submit
/// time. Walking both kinds closes the failure mode at the
/// allocator. The symmetric walker lives in
/// `issue_modification::next_modification_index_in_tx`; both must
/// stay in sync — if a third chain kind ever appears, both functions
/// extend together (ADR-0024 §7 names the trigger for extracting a
/// shared `chain_allocator` module).
fn next_modification_index_in_tx(
    tx: &duckdb::Transaction<'_>,
    base_invoice_id: &str,
) -> Result<u32> {
    let mut max_index: u32 = 0;

    // STORNO entries.
    {
        let mut stmt = tx
            .prepare("SELECT seq, payload FROM audit_ledger WHERE kind = ?;")
            .context("prepare audit_ledger scan for storno chain index")?;
        let rows = stmt
            .query_map([EventKind::InvoiceStornoIssued.as_str()], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?))
            })
            .context("query audit_ledger for storno chain index")?;
        for row in rows {
            let (seq, payload_bytes) =
                row.context("read audit_ledger row during storno chain-index walk")?;
            let payload: audit_payloads::InvoiceStornoIssuedPayload =
                serde_json::from_slice(&payload_bytes).map_err(|e| {
                    anyhow!(
                        "InvoiceStornoIssued audit payload (seq {seq}) failed typed decode: {e} \
                         — audit ledger appears tampered or schema-drifted"
                    )
                })?;
            if payload.base_invoice_id == base_invoice_id
                && payload.modification_index > max_index
            {
                max_index = payload.modification_index;
            }
        }
    }

    // MODIFICATION entries (PR-11 / ADR-0024 §7).
    {
        let mut stmt = tx
            .prepare("SELECT seq, payload FROM audit_ledger WHERE kind = ?;")
            .context("prepare audit_ledger scan for modification chain index")?;
        let rows = stmt
            .query_map([EventKind::InvoiceModificationIssued.as_str()], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?))
            })
            .context("query audit_ledger for modification chain index")?;
        for row in rows {
            let (seq, payload_bytes) =
                row.context("read audit_ledger row during modification chain-index walk")?;
            let payload: audit_payloads::InvoiceModificationIssuedPayload =
                serde_json::from_slice(&payload_bytes).map_err(|e| {
                    anyhow!(
                        "InvoiceModificationIssued audit payload (seq {seq}) failed typed decode: {e} \
                         — audit ledger appears tampered or schema-drifted"
                    )
                })?;
            if payload.base_invoice_id == base_invoice_id
                && payload.modification_index > max_index
            {
                max_index = payload.modification_index;
            }
        }
    }

    // First chain entry against a base starts at 1 per NAV's spec.
    Ok(max_index.saturating_add(1))
}

// ──────────────────────────────────────────────────────────────────────
// Storno command construction — same shape as issue_invoice
// ──────────────────────────────────────────────────────────────────────

fn build_storno_command(input: &InvoiceInputJson, code: &SeriesCode) -> Result<IssueInvoiceCommand> {
    let lines = input
        .lines
        .iter()
        .map(|l| LineItem {
            description: l.description.clone(),
            quantity: l.quantity,
            unit_price: Huf(l.unit_price),
            vat_rate_basis_points: percent_to_basis_points(l.vat_rate_percent),
        })
        .collect();
    Ok(IssueInvoiceCommand {
        idempotency_key: IdempotencyKey::new(),
        series_code: code.clone(),
        customer_id: CustomerId::new(),
        lines,
    })
}

fn percent_to_basis_points(percent: u16) -> u16 {
    percent.saturating_mul(100)
}

// ──────────────────────────────────────────────────────────────────────
// Tests — focused on the chain-index allocator (the one piece of
// logic that lives only in this file). The full happy-path lives in
// `apps/aberp/tests/issue_storno_local.rs`.
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use aberp_audit_ledger::{Actor, BinaryHash, Ledger, TenantId};

    /// Build a Connection-owning fixture: ensure the audit-ledger
    /// schema, then append the given `(base_invoice_id, modification_index)`
    /// entries inside one tx. Returns the Connection; the caller
    /// opens its own tx to invoke `next_modification_index_in_tx`.
    fn fixture_ledger_with_chain(entries: &[(&str, u32)]) -> Connection {
        let tenant = TenantId::new("t1".to_string()).unwrap();
        let bh = BinaryHash::from_bytes([0u8; 32]);
        let actor = Actor::from_local_cli("sess".to_string(), "test-user");
        let meta = LedgerMeta::new(tenant, bh);

        let mut conn = Connection::open_in_memory().unwrap();
        audit_ledger::ensure_schema(&conn).unwrap();
        {
            let tx = conn.transaction().unwrap();
            for (i, (base, idx)) in entries.iter().enumerate() {
                let idem = IdempotencyKey::new();
                let payload = audit_payloads::InvoiceStornoIssuedPayload::new(
                    &format!("inv_storno_{i}"),
                    100 + i as u64,
                    &format!("rsv_storno_{i}"),
                    idem,
                    base,
                    42, // dummy base_sequence_number — not under test here
                    *idx,
                );
                audit_ledger::append_in_tx(
                    &tx,
                    &meta,
                    EventKind::InvoiceStornoIssued,
                    payload.to_bytes(),
                    actor.clone(),
                    Some(idem.to_canonical_string()),
                )
                .unwrap();
            }
            tx.commit().unwrap();
        }
        conn
    }

    #[test]
    fn next_modification_index_starts_at_1_when_chain_is_empty() {
        let mut conn = fixture_ledger_with_chain(&[]);
        let tx = conn.transaction().unwrap();
        let idx = next_modification_index_in_tx(&tx, "inv_BASE").unwrap();
        assert_eq!(idx, 1);
    }

    #[test]
    fn next_modification_index_increments_past_max_against_same_base() {
        let mut conn =
            fixture_ledger_with_chain(&[("inv_BASE", 1), ("inv_BASE", 2)]);
        let tx = conn.transaction().unwrap();
        let idx = next_modification_index_in_tx(&tx, "inv_BASE").unwrap();
        assert_eq!(idx, 3);
    }

    /// CLAUDE.md rule 12: silently mixing chains for different bases
    /// is the "completed successfully with 14% of records skipped"
    /// failure mode. The walker must isolate by base_invoice_id.
    #[test]
    fn next_modification_index_ignores_unrelated_base() {
        let mut conn = fixture_ledger_with_chain(&[
            ("inv_OTHER", 1),
            ("inv_OTHER", 2),
            ("inv_OTHER", 3),
        ]);
        let tx = conn.transaction().unwrap();
        let idx = next_modification_index_in_tx(&tx, "inv_BASE").unwrap();
        assert_eq!(
            idx, 1,
            "BASE has no chain; index must start at 1 regardless of OTHER's chain"
        );
    }

    /// Non-contiguous chain (a gap, however unusual) still allocates
    /// to `max + 1` per ADR-0023 §4. A gap is itself a reconciliation
    /// anomaly that the §4 integrity scan will catch; the allocator
    /// does NOT re-fill the gap.
    #[test]
    fn next_modification_index_skips_gaps_uses_max_plus_one() {
        let mut conn = fixture_ledger_with_chain(&[
            ("inv_BASE", 1),
            ("inv_BASE", 3), // gap at 2
        ]);
        let tx = conn.transaction().unwrap();
        let idx = next_modification_index_in_tx(&tx, "inv_BASE").unwrap();
        assert_eq!(idx, 4);
    }

    /// PR-11 / ADR-0024 §7 symmetry: the storno walker MUST also see
    /// `InvoiceModificationIssued` entries against the same base so
    /// it does not re-issue an index a prior MODIFY already burned.
    /// Without this, two operators on the same base who issue MODIFY
    /// then STORNO would both end up with `modification_index = 1`
    /// and NAV would reject the second with
    /// `INVOICE_NUMBER_NOT_UNIQUE`-shape — failure at the wire, far
    /// from the allocator. CLAUDE.md rule 12 fail-loud + the F22
    /// closure depend on this.
    #[test]
    fn next_modification_index_for_storno_sees_prior_modify_entries() {
        // Build a fixture: one prior MODIFY against inv_BASE at index 1.
        let tenant = TenantId::new("t1".to_string()).unwrap();
        let bh = BinaryHash::from_bytes([0u8; 32]);
        let actor = Actor::from_local_cli("sess".to_string(), "test-user");
        let meta = LedgerMeta::new(tenant, bh);

        let mut conn = Connection::open_in_memory().unwrap();
        audit_ledger::ensure_schema(&conn).unwrap();
        {
            let tx = conn.transaction().unwrap();
            let idem = IdempotencyKey::new();
            let payload = audit_payloads::InvoiceModificationIssuedPayload::new(
                "inv_modif_0",
                100,
                "rsv_modif_0",
                idem,
                "inv_BASE",
                42, // dummy base seq
                1,  // chain index from the MODIFY
                "2026-05-21",
            );
            audit_ledger::append_in_tx(
                &tx,
                &meta,
                EventKind::InvoiceModificationIssued,
                payload.to_bytes(),
                actor,
                Some(idem.to_canonical_string()),
            )
            .unwrap();
            tx.commit().unwrap();
        }

        // A subsequent storno against inv_BASE must allocate index 2,
        // not 1 — the storno walker must see the MODIFY entry too.
        let tx = conn.transaction().unwrap();
        let idx = next_modification_index_in_tx(&tx, "inv_BASE").unwrap();
        assert_eq!(
            idx, 2,
            "storno walker must consider prior MODIFY entries against the same base \
             (ADR-0024 §7 symmetry)"
        );
    }

    /// Precondition walker — Finalized base.
    #[test]
    fn check_base_is_finalized_accepts_saved_ack() {
        let tenant = TenantId::new("t1".to_string()).unwrap();
        let bh = BinaryHash::from_bytes([0u8; 32]);
        let mut ledger = Ledger::open_in_memory(tenant, bh).unwrap();
        let actor = Actor::from_local_cli("sess".to_string(), "test-user");
        let idem = IdempotencyKey::new();
        // Submission response + SAVED ack.
        let resp = audit_payloads::InvoiceSubmissionResponsePayload::new(
            "inv_A",
            idem,
            "TXID-A",
            b"<response/>".to_vec(),
        );
        ledger
            .append(
                EventKind::InvoiceSubmissionResponse,
                resp.to_bytes(),
                actor.clone(),
                Some(idem.to_canonical_string()),
            )
            .unwrap();
        let ack = audit_payloads::InvoiceAckStatusPayload::new(
            "inv_A",
            "TXID-A",
            "SAVED",
            b"<ack/>".to_vec(),
        );
        ledger
            .append(EventKind::InvoiceAckStatus, ack.to_bytes(), actor, None)
            .unwrap();

        check_base_is_finalized(&ledger, "inv_A").expect("SAVED base must be accepted");
    }

    #[test]
    fn check_base_is_finalized_rejects_never_submitted() {
        let tenant = TenantId::new("t1".to_string()).unwrap();
        let bh = BinaryHash::from_bytes([0u8; 32]);
        let ledger = Ledger::open_in_memory(tenant, bh).unwrap();
        let err = check_base_is_finalized(&ledger, "inv_A").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no NAV submission response"),
            "error must name the missing submission: got {msg}"
        );
    }

    #[test]
    fn check_base_is_finalized_rejects_aborted() {
        let tenant = TenantId::new("t1".to_string()).unwrap();
        let bh = BinaryHash::from_bytes([0u8; 32]);
        let mut ledger = Ledger::open_in_memory(tenant, bh).unwrap();
        let actor = Actor::from_local_cli("sess".to_string(), "test-user");
        let idem = IdempotencyKey::new();
        let resp = audit_payloads::InvoiceSubmissionResponsePayload::new(
            "inv_A",
            idem,
            "TXID-A",
            b"<response/>".to_vec(),
        );
        ledger
            .append(
                EventKind::InvoiceSubmissionResponse,
                resp.to_bytes(),
                actor.clone(),
                Some(idem.to_canonical_string()),
            )
            .unwrap();
        let ack = audit_payloads::InvoiceAckStatusPayload::new(
            "inv_A",
            "TXID-A",
            "ABORTED",
            b"<ack/>".to_vec(),
        );
        ledger
            .append(EventKind::InvoiceAckStatus, ack.to_bytes(), actor, None)
            .unwrap();
        let err = check_base_is_finalized(&ledger, "inv_A").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("REJECTED"), "got {msg}");
    }

    #[test]
    fn check_base_is_finalized_rejects_abandoned_even_after_saved() {
        let tenant = TenantId::new("t1".to_string()).unwrap();
        let bh = BinaryHash::from_bytes([0u8; 32]);
        let mut ledger = Ledger::open_in_memory(tenant, bh).unwrap();
        let actor = Actor::from_local_cli("sess".to_string(), "test-user");
        let idem = IdempotencyKey::new();
        let resp = audit_payloads::InvoiceSubmissionResponsePayload::new(
            "inv_A",
            idem,
            "TXID-A",
            b"<response/>".to_vec(),
        );
        ledger
            .append(
                EventKind::InvoiceSubmissionResponse,
                resp.to_bytes(),
                actor.clone(),
                Some(idem.to_canonical_string()),
            )
            .unwrap();
        let ack = audit_payloads::InvoiceAckStatusPayload::new(
            "inv_A",
            "TXID-A",
            "SAVED",
            b"<ack/>".to_vec(),
        );
        ledger
            .append(
                EventKind::InvoiceAckStatus,
                ack.to_bytes(),
                actor.clone(),
                None,
            )
            .unwrap();
        let aban = audit_payloads::InvoiceMarkedAbandonedPayload::new(
            "inv_A",
            idem,
            Some("TXID-A".to_string()),
            Some("SAVED".to_string()),
            "operator decision",
        );
        ledger
            .append(
                EventKind::InvoiceMarkedAbandoned,
                aban.to_bytes(),
                actor,
                Some(idem.to_canonical_string()),
            )
            .unwrap();
        let err = check_base_is_finalized(&ledger, "inv_A").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("ABANDONED"), "got {msg}");
    }

    /// Cross-invoice contamination — same defence-in-depth as the
    /// `audit_query` precondition test. SAVED ack against `inv_B`
    /// must NOT mark `inv_A` as Finalized.
    #[test]
    fn check_base_is_finalized_does_not_cross_invoice_ids() {
        let tenant = TenantId::new("t1".to_string()).unwrap();
        let bh = BinaryHash::from_bytes([0u8; 32]);
        let mut ledger = Ledger::open_in_memory(tenant, bh).unwrap();
        let actor = Actor::from_local_cli("sess".to_string(), "test-user");
        let idem = IdempotencyKey::new();
        let resp = audit_payloads::InvoiceSubmissionResponsePayload::new(
            "inv_B",
            idem,
            "TXID-B",
            b"<response/>".to_vec(),
        );
        ledger
            .append(
                EventKind::InvoiceSubmissionResponse,
                resp.to_bytes(),
                actor.clone(),
                Some(idem.to_canonical_string()),
            )
            .unwrap();
        let ack = audit_payloads::InvoiceAckStatusPayload::new(
            "inv_B",
            "TXID-B",
            "SAVED",
            b"<ack/>".to_vec(),
        );
        ledger
            .append(EventKind::InvoiceAckStatus, ack.to_bytes(), actor, None)
            .unwrap();
        // inv_A has no entries; must loud-fail with NeverSubmitted.
        let err = check_base_is_finalized(&ledger, "inv_A").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no NAV submission response"),
            "inv_A should be NeverSubmitted regardless of inv_B's state: got {msg}"
        );
    }
}
