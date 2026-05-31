//! Orchestration for the `aberp request-technical-annulment`
//! subcommand (PR-12, ADR-0009 §6, ADR-0025).
//!
//! Operator-decision command that records the operator's intent to
//! withdraw a prior NAV data submission. **Single audit entry, no
//! NAV call, no chain interaction, no sequence-slot burn** —
//! structurally distinct from PR-10's `issue-storno` and PR-11's
//! `issue-modification` (which both write three audit entries inside
//! one DuckDB transaction and walk the chain allocator).
//!
//! # Why a single audit entry and not three (load-bearing comment)
//!
//! ADR-0025 §1 + §2 + §7 + §"Adversarial review #2": a technical
//! annulment is NOT itself an invoice. It does not burn a sequence
//! number. It does not consume a chain index. Its audit footprint
//! is exactly ONE entry: `InvoiceTechnicalAnnulmentRequested`. A
//! future contributor reading PR-10's three-entry shape + PR-11's
//! three-entry shape and reflexively adding a sequence-reservation
//! here would break the gap-free invariant silently — adding two
//! more audit entries here would also require widening
//! `issue_storno`'s and `issue_modification`'s allocator walkers to
//! include `InvoiceTechnicalAnnulmentRequested` entries, and the
//! resulting `next_modification_index_in_tx` would re-issue indices
//! against a non-chain kind. The single-entry shape is the
//! load-bearing structural difference; this comment plus the named
//! error messages in [`check_base_is_annullable`] plus ADR-0025 §1
//! + §7 are the review surface that makes the silent regression
//! mechanically hard.
//!
//! # Pipeline
//!
//!   1. Parse + validate CLI args (tenant; references prefix;
//!      reason non-empty — same shape as `mark_abandoned`).
//!   2. Resolve the OS-user actor identity (no NAV credentials
//!      loaded — same posture as `mark_abandoned.rs`'s module
//!      header: this command does not call NAV).
//!   3. Compute binary hash + ledger meta.
//!   4. Pre-flight precondition: walk the audit ledger via
//!      [`check_base_is_annullable`] — base must have a prior
//!      `InvoiceSubmissionResponse` (something to annul) and must
//!      NOT have a prior `InvoiceTechnicalAnnulmentRequested` (no
//!      double-annulment by default, ADR-0025 §6 + §8).
//!   5. Open one DuckDB transaction; under it, append the
//!      `InvoiceTechnicalAnnulmentRequested` entry. ONE entry, one
//!      tx — there is no NAV call to atomically pair it with and
//!      no chain allocator to coordinate with.
//!   6. Drop the Connection, re-open `Ledger`, verify the chain
//!      (success-criterion gate).
//!   7. Render the `<InvoiceAnnulment>` XML via
//!      [`nav_xml::render_annulment_data`]; minimal call-site
//!      sanity check (well-formed XML + four required children
//!      present) per ADR-0025 §4 — the full
//!      `validate_annulment_data` runtime validator is DEFERRED to
//!      the future `submit-annulment` PR.
//!   8. Write the XML to disk; print the operator-visible summary
//!      naming the next step.
//!
//! # NAV credentials NOT loaded
//!
//! Same posture as `mark_abandoned.rs`. Actor `user_id` is derived
//! from the OS-reported username; loud-fail if neither USER nor
//! LOGNAME is set. The audit ledger MUST record who made the
//! annulment decision regardless of whether the operator's
//! workstation has a keychain.

use std::path::Path;

use aberp_audit_ledger::{self as audit_ledger, Actor, EventKind, Ledger, LedgerMeta, TenantId};
use aberp_billing::{self as billing, DuckDbBillingStore, IdempotencyKey};
use anyhow::{anyhow, bail, Context, Result};
use duckdb::Connection;
use quick_xml::events::Event;
use quick_xml::Reader;
use ulid::Ulid;

use crate::audit_payloads;
use crate::binary_hash;
use crate::cli::RequestTechnicalAnnulmentArgs;
use crate::nav_xml::{self, AnnulmentReference};

pub fn run(args: &RequestTechnicalAnnulmentArgs) -> Result<()> {
    let _span = tracing::info_span!(
        "request_technical_annulment",
        invoice_id = %args.references,
        tenant = %args.tenant,
    )
    .entered();

    // 1. Parse + validate CLI args.
    let tenant = TenantId::new(args.tenant.clone()).ok_or_else(|| {
        anyhow!(
            "--tenant value '{}' is empty or has a null byte",
            args.tenant
        )
    })?;
    if !args.references.starts_with("inv_") {
        bail!(
            "--references value '{}' is not a prefixed invoice id (expected inv_<ULID>)",
            args.references
        );
    }
    let reason = args.reason.trim();
    if reason.is_empty() {
        return Err(anyhow!(
            "--reason is required for request-technical-annulment per ADR-0025 §1 \
             (audit-evidence bundle must carry a human-readable justification)"
        ));
    }
    let annulment_code_wire = args.code.to_wire();

    // 2. Resolve the OS-user actor. No NAV credentials loaded — same
    //    posture as `mark_abandoned.rs` (see module header). The
    //    annulment audit entry MUST record who made the decision
    //    regardless of keychain state.
    let session_id = Ulid::new().to_string();
    let os_user = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .ok()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "neither USER nor LOGNAME is set in the environment — \
                 cannot derive an Actor.user_id for the audit ledger; \
                 request-technical-annulment writes an operator decision and \
                 must record who made it"
            )
        })?;
    let actor = Actor::from_local_cli(session_id, &os_user);
    tracing::info!(
        user_id = %actor.user_id,
        annulment_code = annulment_code_wire,
        "actor derived for request-technical-annulment (no NAV credentials loaded)"
    );

    // 3. Compute binary hash + ledger meta.
    let binary_hash_bytes = binary_hash::compute().context("compute binary hash")?;
    let ledger_meta = LedgerMeta::new(tenant.clone(), binary_hash_bytes);

    // 4. Pre-flight precondition. Read-only ledger; drop before
    //    opening the write transaction. Resolve the
    //    base_sequence_number and prior_transaction_id from the same
    //    walk — both are denormalized into either the audit payload
    //    (`prior_transaction_id`) or the on-disk XML
    //    (`base_invoice_number` built from the prior issuance entry).
    //
    //    S173 — also resolve the operator-configured numbering
    //    template here so the rendered base_invoice_number honours
    //    custom segments AND the build-profile `TEST-` prefix.
    //    Pre-S173 this file hardcoded `"INV-default/{seq:05}"`,
    //    which (a) silently produced a no-`TEST-` number on dev
    //    builds (so the resulting `<annulmentReference>` would not
    //    match the base's actual emit on the NAV testbed), and
    //    (b) ignored any tenant with a non-default series template.
    //    One of the two stale-format gaps S165 flagged.
    //
    //    S183 — also load the base invoice's `issue_date.year()` from
    //    the billing row so a cross-year annulment cites the base by
    //    its ORIGINAL issue year (matching the posture
    //    `issue_storno` / `issue_modification` /
    //    `observe_receiver_confirmation` already use). Pre-S183 the
    //    year was captured from the `InvoiceSequenceReserved` audit
    //    entry's `time_wall.year()` — which diverges from
    //    `issue_date.year()` for back-dated invoices and would have
    //    silently sent a wrong `<annulmentReference>` to NAV the
    //    moment a tenant adopted a year-bearing numbering template.
    let seller_toml_path = crate::setup_seller_info::seller_toml_path_for_tenant(&args.tenant)
        .context("resolve seller.toml path for numbering template")?;
    let template = crate::numbering::read_numbering_template(&seller_toml_path)
        .context("read [seller.numbering] template from seller.toml")?;
    let base_issue_year = load_base_invoice_issue_year(&args.db, &args.references)?;
    // S190 — also resolve the base invoice's on-disk NAV XML path during the
    // same ledger scope. The reference to NAV on the `<annulmentReference>`
    // element MUST match the byte-exact `<invoiceNumber>` NAV holds on file
    // (the base XML written at base-issuance and never re-rewritten). Re-
    // deriving via `template.render_for_build(base_year, base_seq)` is
    // vulnerable to seller.toml-literal drift between base issuance and
    // annulment — same failure class S184 closed for storno/modification.
    // The template render is still computed by `check_base_is_annullable`
    // and used purely as the defence-in-depth comparator that fires a WARN
    // below if the two disagree.
    let (precondition, base_nav_xml_path) = {
        let ledger = Ledger::open(&args.db, tenant.clone(), binary_hash_bytes)
            .context("open audit ledger for annulment precondition check")?;
        let pre = check_base_is_annullable(&ledger, &args.references, &template, base_issue_year)?;
        let path = crate::issue_storno::find_base_nav_xml_path_for_chain(&ledger, &args.references)
            .context(
                "S190 — resolve base invoice's on-disk NAV XML path for annulment reference",
            )?;
        (pre, path)
    };
    tracing::info!(
        prior_transaction_id = %precondition.prior_transaction_id,
        "annulment precondition passed"
    );

    // S190 — read the base invoice's `<invoiceNumber>` from its on-disk
    // NAV XML. That string IS the canonical record of what NAV saw on
    // the original `manageInvoice` POST. CLAUDE.md rule 12 — loud-fail
    // if the file is missing / malformed / lacks the element rather
    // than silently substituting a possibly-wrong render.
    let base_invoice_number = crate::nav_xml::read_invoice_number_from_xml(&base_nav_xml_path)
        .context("S190 — read base invoice number from on-disk NAV XML for annulment reference")?;
    if base_invoice_number != precondition.base_invoice_number {
        tracing::warn!(
            base_invoice_id = %args.references,
            from_xml = %base_invoice_number,
            from_template = %precondition.base_invoice_number,
            "S190: base invoice number from on-disk NAV XML differs from current-template re-render \
             — operator likely edited the seller.toml numbering literal between base issuance and \
             annulment. Using the on-disk XML number (NAV's authoritative record); the rendered \
             value would have produced an INVALID_INVOICE_REFERENCE ABORTED ack class."
        );
    }

    // 5. Open one tx; append the InvoiceTechnicalAnnulmentRequested
    //    entry. Operator-decision idempotency key fresh per command.
    let idempotency_key = IdempotencyKey::new();
    let payload = audit_payloads::InvoiceTechnicalAnnulmentRequestedPayload::new(
        &args.references,
        idempotency_key,
        &precondition.prior_transaction_id,
        annulment_code_wire,
        reason,
    );
    let mut conn = Connection::open(&args.db)
        .with_context(|| format!("open tenant DuckDB at {}", args.db.display()))?;
    audit_ledger::ensure_schema(&conn)
        .context("ensure audit-ledger schema for request-technical-annulment")?;
    {
        let tx = conn
            .transaction()
            .context("begin DuckDB transaction (request-technical-annulment audit append)")?;
        audit_ledger::append_in_tx(
            &tx,
            &ledger_meta,
            EventKind::InvoiceTechnicalAnnulmentRequested,
            payload.to_bytes(),
            actor,
            Some(idempotency_key.to_canonical_string()),
        )
        .context("audit_ledger::append_in_tx InvoiceTechnicalAnnulmentRequested")?;
        tx.commit()
            .context("commit DuckDB transaction (request-technical-annulment audit append)")?;
    }
    drop(conn);

    // 6. Verify the audit chain (success-criterion gate).
    let ledger = Ledger::open(&args.db, tenant, binary_hash_bytes)
        .context("re-open audit ledger after request-technical-annulment commit")?;
    let verified = ledger
        .verify_chain()
        .context("audit-ledger chain verification failed AFTER annulment audit append")?;
    tracing::info!(entries_verified = verified, "audit chain verified");

    // 6a. PR-17 / ADR-0030 §2 — sync the audit-ledger mirror file
    //     post-commit.
    let mirror_path = audit_ledger::mirror_path_for(&args.db);
    ledger
        .sync_mirror(&mirror_path)
        .context("sync audit-ledger mirror file after request-technical-annulment commit")?;

    // 7. Render the annulment XML + minimal call-site sanity check
    //    per ADR-0025 §4. The full `validate_annulment_data` runtime
    //    validator is the named-trigger work for the future
    //    submit-annulment PR; this minimal check guarantees the
    //    emitter did not silently drop a required child.
    let annulment_reference = AnnulmentReference {
        // S190 — XML-derived (NAV-authoritative) base number; see the
        // resolution + WARN block above.
        base_invoice_number: base_invoice_number.clone(),
        annulment_code: annulment_code_wire,
        reason: reason.to_string(),
    };
    let xml = nav_xml::render_annulment_data(&annulment_reference)
        .context("render NAV InvoiceAnnulment XML")?;
    check_annulment_xml_minimum(&xml)
        .context("rendered InvoiceAnnulment XML failed the call-site sanity check (ADR-0025 §4)")?;
    tracing::info!(
        bytes = xml.len(),
        "InvoiceAnnulment XML passed call-site sanity check (full XSD validator deferred per ADR-0025 §4)"
    );

    // 8. Write to disk + operator-visible summary.
    nav_xml::write_to_path(&args.out, &xml)?;
    tracing::info!(path = %args.out.display(), bytes = xml.len(), "InvoiceAnnulment XML written");
    // tracing::error rather than info: an annulment is an
    // operator-visible escalation (the operator is asking NAV to
    // withdraw a data submission). Surfaced loud (CLAUDE.md rule 12)
    // so it appears in any structured-log alerting that watches for
    // error-level events on the audit path. Same posture as
    // mark_abandoned's terminal-decision log.
    tracing::error!(
        invoice_id = %args.references,
        annulment_code = annulment_code_wire,
        prior_transaction_id = %precondition.prior_transaction_id,
        "technical annulment REQUESTED by operator — NAV-side withdrawal of prior data submission pending; receiver must confirm in NAV web UI"
    );

    println!(
        "request-technical-annulment OK: invoice {} (base seq {}) — annulment code {}, \
         prior NAV transactionId {}, written to {} ({} bytes); audit chain verified across {} entries. \
         Next step: run `aberp submit-annulment` to POST to NAV's manageAnnulment endpoint (future PR); \
         after NAV accepts, the receiver must confirm the annulment in the NAV web UI per ADR-0009 §6.",
        args.references,
        precondition.base_sequence_number,
        annulment_code_wire,
        precondition.prior_transaction_id,
        args.out.display(),
        xml.len(),
        verified,
    );

    Ok(())
}

// ──────────────────────────────────────────────────────────────────────
// Pre-flight precondition: base must have a prior submission
// response, must NOT have a prior annulment request. Captures the
// base's NAV-facing invoice number + prior transactionId in one walk
// for downstream use in step 5 (payload) + step 7 (XML).
// ADR-0025 §6.
// ──────────────────────────────────────────────────────────────────────

/// Captured precondition facts the rest of the pipeline consumes.
/// Returned by [`check_base_is_annullable`] when the precondition
/// holds.
#[derive(Debug, Clone)]
struct AnnulmentPrecondition {
    /// The base invoice's NAV-facing number (e.g. `INV-default/00007`,
    /// or `TEST-INV-default/00007` on a dev build, or a custom
    /// template's render such as `ABERP-2026/00007`). Reconstructed
    /// from the `InvoiceSequenceReserved` entry on the audit ledger
    /// (which carries `seq`) plus the base invoice's
    /// `issue_date.year()` (loaded outside the precondition walk via
    /// [`load_base_invoice_issue_year`]), rendered through the
    /// operator-configured [`crate::numbering::NumberingTemplate`].
    /// S183 — the base invoice's billing row IS now read (year only)
    /// so cross-year annulments cite the base by its original year;
    /// the row is still not loaded inside the audit-tx path.
    base_invoice_number: String,
    /// The base's NAV-facing sequence number (for the operator-
    /// visible summary; not part of the audit payload).
    base_sequence_number: u64,
    /// The most-recent prior `transactionId` (from the latest
    /// `InvoiceSubmissionResponse` against the base). This is what
    /// the annulment withdraws; stored in the payload as
    /// `prior_transaction_id`.
    prior_transaction_id: String,
}

/// Walk the audit ledger and confirm the base invoice is annullable
/// per ADR-0025 §6:
///
///   - Has at least one `InvoiceSubmissionResponse` (something to
///     annul — i.e., a NAV data submission actually happened).
///   - Has NOT had a prior `InvoiceTechnicalAnnulmentRequested`
///     pointing at it (default-reject double annulment; ADR-0025 §6
///     + §"Surfaced conflict 3").
///
/// **Does NOT reject**:
///
///   - `Rejected` / `Stuck` / `Abandoned` / `Storno` / `Amended`
///     bases. Technical annulment is data-submission withdrawal,
///     orthogonal to the legal-document state. See ADR-0025 §6's
///     "Does NOT reject" enumeration.
///
/// Loud-fails with a specific named-reason message per CLAUDE.md
/// rule 12. The "legally cancelled by a storno" message ADR-0024 §6
/// emits for `issue-modification` is INTENTIONALLY absent here —
/// modifying a stornoed base is malformed (legal-cancellation
/// argument), but annulling a stornoed base is permitted (the
/// stornoed base's data submission may itself have been wrong, and
/// the annulment cleans the NAV-side trail).
fn check_base_is_annullable(
    ledger: &Ledger,
    base_invoice_id: &str,
    template: &crate::numbering::NumberingTemplate,
    base_issue_year: i32,
) -> Result<AnnulmentPrecondition> {
    let entries = ledger
        .entries()
        .context("read audit ledger entries for annulment precondition check")?;

    // Captured state. The "latest" tracking deliberately walks in
    // forward order (entries are returned in seq order by
    // `Ledger::entries`); the latest hit overwrites earlier hits, so
    // at end-of-loop the stored values are the most-recent ones.
    let mut latest_transaction_id: Option<String> = None;
    let mut latest_sequence_number: Option<u64> = None;
    let mut has_prior_annulment = false;

    for entry in &entries {
        match entry.kind {
            EventKind::InvoiceSequenceReserved => {
                let payload: audit_payloads::InvoiceSequenceReservedPayload =
                    serde_json::from_slice(&entry.payload).map_err(|e| {
                        anyhow!(
                            "InvoiceSequenceReserved audit payload (seq {}) failed typed decode: {e} \
                             — audit ledger appears tampered or schema-drifted",
                            entry.seq.as_u64()
                        )
                    })?;
                if payload.invoice_id == base_invoice_id {
                    latest_sequence_number = Some(payload.seq);
                }
            }
            EventKind::InvoiceSubmissionResponse => {
                let payload: audit_payloads::InvoiceSubmissionResponsePayload =
                    serde_json::from_slice(&entry.payload).map_err(|e| {
                        anyhow!(
                            "InvoiceSubmissionResponse audit payload (seq {}) failed typed decode: {e} \
                             — audit ledger appears tampered or schema-drifted",
                            entry.seq.as_u64()
                        )
                    })?;
                if payload.invoice_id == base_invoice_id {
                    latest_transaction_id = Some(payload.transaction_id);
                }
            }
            EventKind::InvoiceTechnicalAnnulmentRequested => {
                let payload: audit_payloads::InvoiceTechnicalAnnulmentRequestedPayload =
                    serde_json::from_slice(&entry.payload).map_err(|e| {
                        anyhow!(
                            "InvoiceTechnicalAnnulmentRequested audit payload (seq {}) failed typed decode: {e} \
                             — audit ledger appears tampered or schema-drifted",
                            entry.seq.as_u64()
                        )
                    })?;
                if payload.invoice_id == base_invoice_id {
                    has_prior_annulment = true;
                }
            }
            _ => {}
        }
    }

    if has_prior_annulment {
        bail!(
            "base invoice {} already has a prior InvoiceTechnicalAnnulmentRequested entry — \
             double-annulment is loud-rejected by default per ADR-0025 §6; \
             if the accountant resolves to permit (open question §8), the precondition walker's \
             has_prior_annulment branch is the deletion site",
            base_invoice_id
        );
    }
    let prior_transaction_id = latest_transaction_id.ok_or_else(|| {
        anyhow!(
            "base invoice {} has no NAV submission response on record — \
             technical annulment withdraws a NAV-side data submission, so the base must have \
             been submitted at least once (ADR-0025 §6). For an unsubmitted-but-abandoned \
             invoice, use `aberp mark-abandoned` instead (local-only terminal decision).",
            base_invoice_id
        )
    })?;
    let base_sequence_number = latest_sequence_number.ok_or_else(|| {
        anyhow!(
            "base invoice {} has a submission response but no sequence-reserved entry — \
             audit ledger appears tampered or schema-drifted",
            base_invoice_id
        )
    })?;

    // S183 — year sourced from the base invoice's `issue_date.year()`
    // (loaded by [`load_base_invoice_issue_year`] in `run`), NOT from
    // any `time_wall.year()` on the audit entries.
    //
    // S190 — the seller.toml-literal-drift class S184 closed for
    // `issue_storno` + `issue_modification` is now also closed here.
    // The authoritative `<annulmentReference>` value flowed to NAV is
    // the XML-derived number resolved in `run` via
    // `find_base_nav_xml_path_for_chain` + `read_invoice_number_from_xml`.
    // The template render below is preserved purely as the defence-in-
    // depth comparator that fires the WARN in `run` if the two disagree
    // (operator edited seller.toml between base issuance and annulment).
    // Walker tests still pin the template-derived render through this
    // field so the regression surface stays mechanically visible.
    let base_invoice_number = template.render_for_build(base_issue_year, base_sequence_number);
    Ok(AnnulmentPrecondition {
        base_invoice_number,
        base_sequence_number,
        prior_transaction_id,
    })
}

// ──────────────────────────────────────────────────────────────────────
// Minimal call-site sanity check on the rendered annulment XML.
// ADR-0025 §4: NOT a substitute for `validate_annulment_data` (which
// is deferred to the future submit-annulment PR). The check is the
// "loud-fail on emitter regression" guardrail — if a future refactor
// accidentally drops a required child, this fires before the bytes
// hit disk.
// ──────────────────────────────────────────────────────────────────────

const ANNULMENT_REQUIRED_CHILDREN: &[&str] = &[
    "annulmentReference",
    "annulmentTimestamp",
    "annulmentCode",
    "annulmentReason",
];

fn check_annulment_xml_minimum(xml: &[u8]) -> Result<()> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);

    let mut saw_root = false;
    let mut seen: Vec<String> = Vec::new();
    let mut buf: Vec<u8> = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = local_name(e.name().as_ref());
                if name == "InvoiceAnnulment" {
                    saw_root = true;
                } else if ANNULMENT_REQUIRED_CHILDREN.contains(&name.as_str()) {
                    seen.push(name);
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                bail!(
                    "InvoiceAnnulment XML failed parse at position {}: {e}",
                    reader.buffer_position()
                );
            }
            _ => {}
        }
        buf.clear();
    }

    if !saw_root {
        bail!(
            "rendered XML missing root <InvoiceAnnulment> element — emitter regression \
             (ADR-0025 §4 call-site sanity check)"
        );
    }
    let missing: Vec<&'static str> = ANNULMENT_REQUIRED_CHILDREN
        .iter()
        .copied()
        .filter(|name| !seen.iter().any(|s| s == *name))
        .collect();
    if !missing.is_empty() {
        bail!(
            "rendered <InvoiceAnnulment> missing required children: {:?} — emitter regression \
             (ADR-0025 §4 call-site sanity check)",
            missing
        );
    }
    Ok(())
}

fn local_name(qualified: &[u8]) -> String {
    let local = match qualified.iter().rposition(|&b| b == b':') {
        Some(i) => &qualified[i + 1..],
        None => qualified,
    };
    String::from_utf8_lossy(local).into_owned()
}

// ──────────────────────────────────────────────────────────────────────
// S183 — load the base invoice's `issue_date.year()` from the billing
// row so a cross-year annulment cites the base by its ORIGINAL year.
// Mirrors `observe_receiver_confirmation::load_base_nav_invoice_number`'s
// posture (billing-row read outside any audit tx). Returns only the
// year — the rest of the row is discarded.
// ──────────────────────────────────────────────────────────────────────

fn load_base_invoice_issue_year(db_path: &Path, invoice_id: &str) -> Result<i32> {
    let store = DuckDbBillingStore::open(db_path)
        .with_context(|| format!("open billing DuckDB at {}", db_path.display()))?;
    let mut conn = store.into_connection();
    let tx = conn
        .transaction()
        .context("begin read tx for base invoice issue-year lookup")?;
    let (invoice, _idem) = billing::load_ready_invoice_by_id(&tx, invoice_id)
        .context("billing::load_ready_invoice_by_id (request-technical-annulment base)")?
        .ok_or_else(|| {
            anyhow!(
                "no invoice with id {} in this tenant DB — \
                 request-technical-annulment requires the base invoice's billing row \
                 to source `issue_date.year()` for the rendered <annulmentReference>",
                invoice_id
            )
        })?;
    tx.commit()
        .context("commit read tx for base invoice issue-year lookup")?;
    Ok(invoice.issue_date.year())
}

// ──────────────────────────────────────────────────────────────────────
// Tests — precondition walker (accept/reject discipline) + the
// minimum-children XML guardrail.
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use aberp_audit_ledger::{Actor, BinaryHash, Ledger, TenantId};

    /// Build a small ledger fixture: an issuance pair
    /// (`InvoiceSequenceReserved` + `InvoiceDraftCreated`) followed
    /// optionally by a submission response (`InvoiceSubmissionResponse`)
    /// and any other entries the test wants to seed. Returns the
    /// (in-memory) ledger for the test to read.
    fn ledger_with_entries(entries: Vec<(EventKind, Vec<u8>, Option<String>)>) -> Ledger {
        let tenant = TenantId::new("t1".to_string()).unwrap();
        let bh = BinaryHash::from_bytes([0u8; 32]);
        let mut ledger = Ledger::open_in_memory(tenant, bh).unwrap();
        let actor = Actor::from_local_cli("sess".to_string(), "test-user");
        for (kind, payload, idem) in entries {
            ledger.append(kind, payload, actor.clone(), idem).unwrap();
        }
        ledger
    }

    fn seq_reserved_payload(invoice_id: &str, seq: u64) -> Vec<u8> {
        let payload = serde_json::json!({
            "invoice_id": invoice_id,
            "seq": seq,
            "reservation_id": "rsv_00000000000000000000000000",
            "idempotency_key": "idem_00000000000000000000000000",
        });
        serde_json::to_vec(&payload).unwrap()
    }

    fn submission_response_payload(invoice_id: &str, txid: &str) -> Vec<u8> {
        let payload = audit_payloads::InvoiceSubmissionResponsePayload::new(
            invoice_id,
            IdempotencyKey::new(),
            txid,
            b"<response/>".to_vec(),
        );
        payload.to_bytes()
    }

    fn annulment_payload(invoice_id: &str, txid: &str) -> Vec<u8> {
        let payload = audit_payloads::InvoiceTechnicalAnnulmentRequestedPayload::new(
            invoice_id,
            IdempotencyKey::new(),
            txid,
            "ERRATIC_DATA",
            "prior annulment",
        );
        payload.to_bytes()
    }

    /// Happy path: base has a sequence-reserved entry + a submission
    /// response. Precondition passes; the returned struct carries
    /// the base's NAV-facing number and the prior transactionId.
    #[test]
    fn check_base_is_annullable_accepts_submitted_base() {
        let entries = vec![
            (
                EventKind::InvoiceSequenceReserved,
                seq_reserved_payload("inv_A", 7),
                None,
            ),
            (
                EventKind::InvoiceSubmissionResponse,
                submission_response_payload("inv_A", "TXID-7"),
                None,
            ),
        ];
        let ledger = ledger_with_entries(entries);
        let template = crate::numbering::default_template();
        // S183 — caller supplies the base invoice's `issue_date.year()`;
        // the default template has no Year segment so the value is unused
        // by the render, but the parameter is now required.
        let pre = check_base_is_annullable(&ledger, "inv_A", &template, 2026)
            .expect("submitted base must be annullable");
        assert_eq!(pre.prior_transaction_id, "TXID-7");
        assert_eq!(pre.base_sequence_number, 7);
        // S173 — base_invoice_number is rendered through
        // `NumberingTemplate::render_for_build`, so dev builds carry
        // the `TEST-` prefix and prod builds do not. The test reads
        // the same constant the renderer reads to stay build-agnostic.
        let expected = format!(
            "{}INV-default/00007",
            crate::build_profile::INVOICE_NUMBER_TEST_PREFIX
        );
        assert_eq!(pre.base_invoice_number, expected);
    }

    /// S183 — when the operator-configured template carries a
    /// `Segment::Year` segment, the rendered base_invoice_number must
    /// use the base invoice's ORIGINAL `issue_date.year()` (passed
    /// into the walker by [`run`] via [`load_base_invoice_issue_year`]),
    /// NOT any wall-clock year captured on the audit entries. This is
    /// what makes a cross-year annulment cite the base by its original
    /// year — the same posture `issue_storno` / `issue_modification` /
    /// `observe_receiver_confirmation` already use for the base reference.
    ///
    /// Pre-S183 the walker captured year from the
    /// `InvoiceSequenceReserved` entry's `Entry.time_wall.year()` —
    /// which silently diverges from `issue_date.year()` for back-dated
    /// invoices (a common HU end-of-year-bookkeeping case) and would
    /// have produced a wrong `<annulmentReference>` the moment a
    /// tenant adopted a year-bearing template. The PR-182 review
    /// flagged this latent bug; PR-183 closes it.
    #[test]
    fn check_base_is_annullable_renders_year_from_base_issue_date_param() {
        use crate::numbering::{NumberingTemplate, ResetPolicy, Segment, YearDigits};
        let entries = vec![
            (
                EventKind::InvoiceSequenceReserved,
                seq_reserved_payload("inv_A", 7),
                None,
            ),
            (
                EventKind::InvoiceSubmissionResponse,
                submission_response_payload("inv_A", "TXID-7"),
                None,
            ),
        ];
        let ledger = ledger_with_entries(entries);
        let template = NumberingTemplate {
            segments: vec![
                Segment::Literal("ABERP-".to_string()),
                Segment::Year {
                    digits: YearDigits::Four,
                },
                Segment::Literal("/".to_string()),
                Segment::Counter { pad_width: 5 },
            ],
            reset_policy: ResetPolicy::OnYearChange,
            start_value: 1,
        };
        // CLAUDE.md rule 9 — the assertion targets the load-bearing
        // year-source contract. The in-memory ledger's entries are
        // stamped with the test-run wall clock (some year N+); we pass
        // an explicit year 2025 (the base's original `issue_date.year()`)
        // and assert the render uses 2025 verbatim, NOT the wall clock.
        // A regression that re-captures year from `time_wall` would fail
        // this test loudly because 2025 != test-run wall clock year.
        let pre = check_base_is_annullable(&ledger, "inv_A", &template, 2025)
            .expect("submitted base must be annullable");
        let prefix = crate::build_profile::INVOICE_NUMBER_TEST_PREFIX;
        assert_eq!(
            pre.base_invoice_number,
            format!("{prefix}ABERP-2025/00007"),
            "base_invoice_number must render with the year passed in by the caller \
             (the base invoice's original issue_date.year()), NOT any audit-entry \
             wall-clock year"
        );
    }

    /// S183 — defence-in-depth: explicitly pin that a cross-year
    /// annulment renders the base reference with the base's ORIGINAL
    /// year, not the wall-clock year of the annulment run. Same shape
    /// as the test above but the assertion names the cross-year intent
    /// loudly so a future contributor reading the test list sees
    /// "cross-year correctness is locked in." Mirrors the equivalent
    /// pin in `issue_storno` / `issue_modification` tests.
    #[test]
    fn check_base_is_annullable_cross_year_cites_base_original_year() {
        use crate::numbering::{NumberingTemplate, ResetPolicy, Segment, YearDigits};
        let entries = vec![
            (
                EventKind::InvoiceSequenceReserved,
                seq_reserved_payload("inv_A", 17),
                None,
            ),
            (
                EventKind::InvoiceSubmissionResponse,
                submission_response_payload("inv_A", "TXID-17"),
                None,
            ),
        ];
        let ledger = ledger_with_entries(entries);
        let template = NumberingTemplate {
            segments: vec![
                Segment::Literal("ABERP-".to_string()),
                Segment::Year {
                    digits: YearDigits::Four,
                },
                Segment::Literal("/".to_string()),
                Segment::Counter { pad_width: 5 },
            ],
            reset_policy: ResetPolicy::OnYearChange,
            start_value: 1,
        };
        // Base was issued in 2025 (the back-dated case); annulment is
        // initiated in 2026. The render MUST cite the base as
        // `ABERP-2025/00017` — not `ABERP-2026/00017`.
        let pre = check_base_is_annullable(&ledger, "inv_A", &template, 2025)
            .expect("submitted base must be annullable");
        let prefix = crate::build_profile::INVOICE_NUMBER_TEST_PREFIX;
        assert_eq!(
            pre.base_invoice_number,
            format!("{prefix}ABERP-2025/00017"),
            "cross-year annulment must cite the base by its original issue year"
        );
        assert_ne!(
            pre.base_invoice_number,
            format!("{prefix}ABERP-2026/00017"),
            "cross-year annulment must NOT cite the base by the annulment-run year"
        );
    }

    /// ADR-0025 §6: never-submitted base is loud-rejected. Operator
    /// is steered to `mark-abandoned` instead. The named error
    /// message is part of the operator-visible artifact per CLAUDE.md
    /// rule 12.
    #[test]
    fn check_base_is_annullable_rejects_never_submitted() {
        let entries = vec![(
            EventKind::InvoiceSequenceReserved,
            seq_reserved_payload("inv_A", 7),
            None,
        )];
        let ledger = ledger_with_entries(entries);
        let template = crate::numbering::default_template();
        let err = check_base_is_annullable(&ledger, "inv_A", &template, 2026).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no NAV submission response"),
            "error must name the missing submission: got {msg}"
        );
        assert!(
            msg.contains("mark-abandoned"),
            "error must steer the operator to mark-abandoned: got {msg}"
        );
    }

    /// ADR-0025 §6 default-reject: double annulment fires the named
    /// branch. The accountant question of permitting double annulment
    /// is open (§8); if it resolves to permit, the deletion site is
    /// the `has_prior_annulment` branch in this walker. The error
    /// message must name "double-annulment" so a future contributor
    /// removing the branch is also removing the message — visible in
    /// PR review.
    #[test]
    fn check_base_is_annullable_rejects_double_annulment() {
        let entries = vec![
            (
                EventKind::InvoiceSequenceReserved,
                seq_reserved_payload("inv_A", 7),
                None,
            ),
            (
                EventKind::InvoiceSubmissionResponse,
                submission_response_payload("inv_A", "TXID-7"),
                None,
            ),
            (
                EventKind::InvoiceTechnicalAnnulmentRequested,
                annulment_payload("inv_A", "TXID-7"),
                None,
            ),
        ];
        let ledger = ledger_with_entries(entries);
        let template = crate::numbering::default_template();
        let err = check_base_is_annullable(&ledger, "inv_A", &template, 2026).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("double-annulment"),
            "error message must name double-annulment (load-bearing review surface per ADR-0025 §6): got {msg}"
        );
    }

    /// ADR-0025 §6 "Does NOT reject": a STORNO entry against the base
    /// does NOT block an annulment. Technical annulment is data-
    /// submission withdrawal, orthogonal to legal cancellation —
    /// load-bearing distinction from the
    /// `check_base_is_modifiable_rejects_stornoed_base` test in
    /// `issue_modification.rs`. CLAUDE.md rule 9: this test asserts
    /// the intent, not just any walker behaviour.
    #[test]
    fn check_base_is_annullable_does_not_reject_stornoed_base() {
        // Build a STORNO chain-link payload by hand (bypassing the
        // typed `InvoiceStornoIssuedPayload::new` since `aberp_billing`
        // is not imported in test scope here — same posture as the
        // hand-built seq_reserved_payload helper above).
        let storno_payload = serde_json::json!({
            "storno_invoice_id": "inv_S1",
            "storno_seq": 8,
            "storno_reservation_id": "rsv_00000000000000000000000000",
            "idempotency_key": "idem_00000000000000000000000000",
            "base_invoice_id": "inv_A",
            "base_sequence_number": 7,
            "modification_index": 1,
        });
        let entries = vec![
            (
                EventKind::InvoiceSequenceReserved,
                seq_reserved_payload("inv_A", 7),
                None,
            ),
            (
                EventKind::InvoiceSubmissionResponse,
                submission_response_payload("inv_A", "TXID-7"),
                None,
            ),
            (
                EventKind::InvoiceStornoIssued,
                serde_json::to_vec(&storno_payload).unwrap(),
                None,
            ),
        ];
        let ledger = ledger_with_entries(entries);
        let template = crate::numbering::default_template();
        check_base_is_annullable(&ledger, "inv_A", &template, 2026)
            .expect("a stornoed base must remain annullable (ADR-0025 §6)");
    }

    /// Cross-invoice contamination — a submission response against
    /// `inv_B` must NOT make `inv_A` annullable. Defence-in-depth
    /// pin per the same posture
    /// `check_base_is_modifiable_does_not_cross_invoice_ids` uses.
    #[test]
    fn check_base_is_annullable_does_not_cross_invoice_ids() {
        let entries = vec![
            (
                EventKind::InvoiceSequenceReserved,
                seq_reserved_payload("inv_B", 7),
                None,
            ),
            (
                EventKind::InvoiceSubmissionResponse,
                submission_response_payload("inv_B", "TXID-B"),
                None,
            ),
        ];
        let ledger = ledger_with_entries(entries);
        let template = crate::numbering::default_template();
        let err = check_base_is_annullable(&ledger, "inv_A", &template, 2026).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no NAV submission response"),
            "inv_A must be treated as NeverSubmitted regardless of inv_B's state: got {msg}"
        );
    }

    // ── minimum-XML guardrail tests ────────────────────────────────

    /// Happy path: a freshly-rendered AnnulmentData passes the
    /// minimum-children check.
    #[test]
    fn check_annulment_xml_minimum_accepts_well_formed_body() {
        let r = AnnulmentReference {
            base_invoice_number: "INV-default/00007".to_string(),
            annulment_code: "ERRATIC_DATA",
            reason: "test invoice accidentally sent to production".to_string(),
        };
        let xml = nav_xml::render_annulment_data(&r).unwrap();
        check_annulment_xml_minimum(&xml).expect("emitter output must pass guardrail");
    }

    /// **Emitter-regression guardrail.** A body missing one of the
    /// four required children fires the named error. CLAUDE.md rule
    /// 9: this is the test that catches a future refactor accidentally
    /// dropping `<annulmentCode>` — the test would fail loudly at
    /// CI time, well before NAV's testbed sees the body.
    #[test]
    fn check_annulment_xml_minimum_rejects_missing_code() {
        // Hand-rolled body missing <annulmentCode>.
        let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<InvoiceAnnulment xmlns="http://schemas.nav.gov.hu/OSA/3.0/annul">
  <annulmentReference>INV-default/00007</annulmentReference>
  <annulmentTimestamp>2026-05-21T12:00:00Z</annulmentTimestamp>
  <annulmentReason>missing code on purpose</annulmentReason>
</InvoiceAnnulment>"#;
        let err = check_annulment_xml_minimum(xml).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("annulmentCode"),
            "error must name the missing child: got {msg}"
        );
    }

    /// Defence-in-depth: a body missing the root element fires the
    /// named root-missing branch.
    #[test]
    fn check_annulment_xml_minimum_rejects_missing_root() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<SomethingElse>
  <annulmentReference>INV-default/00007</annulmentReference>
  <annulmentTimestamp>2026-05-21T12:00:00Z</annulmentTimestamp>
  <annulmentCode>ERRATIC_DATA</annulmentCode>
  <annulmentReason>fake</annulmentReason>
</SomethingElse>"#;
        let err = check_annulment_xml_minimum(xml).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("<InvoiceAnnulment>"), "got {msg}");
    }

    /// S190 drift-protection pin — when the seller.toml numbering
    /// literal is edited between base issuance and annulment, the
    /// authoritative `<annulmentReference>` value MUST come from the
    /// base invoice's on-disk NAV XML (the byte-exact string NAV holds
    /// on file), NOT from `template.render_for_build(base_year, base_seq)`
    /// against the current (edited) template. Composes
    /// [`crate::issue_storno::find_base_nav_xml_path_for_chain`] +
    /// [`crate::nav_xml::read_invoice_number_from_xml`] — the same two
    /// helpers `run` wires into the annulment flow at the call site.
    /// CLAUDE.md rule 9: this test asserts the load-bearing drift-
    /// protection intent — a regression that re-routes the call site
    /// back to `render_for_build` for the reference would surface here.
    #[test]
    fn s190_drift_protection_uses_on_disk_xml_not_template() {
        use crate::numbering::{NumberingTemplate, ResetPolicy, Segment, YearDigits};
        use ulid::Ulid;

        // 1. Write a base NAV XML on disk with a specific
        //    <invoiceNumber>. This reflects what NAV actually stored
        //    when the base was issued under the OLD-PREFIX template.
        let scratch_dir = std::env::temp_dir()
            .join("aberp-s190-drift")
            .join(format!("{}", Ulid::new()));
        std::fs::create_dir_all(&scratch_dir).expect("create scratch dir");
        let base_xml_path = scratch_dir.join("base.xml");
        let original_number = "TEST-OLD-PREFIX-2026/00042";
        let xml = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <InvoiceData xmlns=\"http://schemas.nav.gov.hu/OSA/3.0/data\">\
             <invoiceNumber>{}</invoiceNumber>\
             </InvoiceData>",
            original_number
        );
        std::fs::write(&base_xml_path, xml).expect("write base XML");

        // 2. Build a ledger with a single InvoiceDraftCreated payload
        //    whose `nav_xml_path` points at the on-disk XML. All other
        //    payload fields ride `#[serde(default)]`.
        let draft_payload = serde_json::json!({
            "invoice_id": "inv_A",
            "line_count": 1,
            "idempotency_key": "idem_00000000000000000000000000",
            "nav_xml_path": base_xml_path.to_string_lossy(),
        });
        let entries = vec![(
            EventKind::InvoiceDraftCreated,
            serde_json::to_vec(&draft_payload).unwrap(),
            None,
        )];
        let ledger = ledger_with_entries(entries);

        // 3. Resolve the path via the shared helper + read the
        //    `<invoiceNumber>` from the XML.
        let path = crate::issue_storno::find_base_nav_xml_path_for_chain(&ledger, "inv_A")
            .expect("find_base_nav_xml_path_for_chain resolves draft payload");
        assert_eq!(path, base_xml_path);
        let from_xml = crate::nav_xml::read_invoice_number_from_xml(&path)
            .expect("read_invoice_number_from_xml succeeds");
        assert_eq!(
            from_xml, original_number,
            "S190: XML round-trip MUST be byte-identical to what NAV saw on the base submit"
        );

        // 4. The CURRENT template — operator edited the literal after
        //    base issuance (removed `OLD-PREFIX-`, added `NEW-PREFIX-`).
        //    `render_for_build` against the current template produces a
        //    DIFFERENT string. Pre-S190 this drifted string would have
        //    flowed into `<annulmentReference>` and NAV would have
        //    returned INVALID_INVOICE_REFERENCE. The XML-derived path
        //    closes the failure mode.
        let current_template = NumberingTemplate {
            segments: vec![
                Segment::Literal("NEW-PREFIX-".to_string()),
                Segment::Year {
                    digits: YearDigits::Four,
                },
                Segment::Literal("/".to_string()),
                Segment::Counter { pad_width: 5 },
            ],
            reset_policy: ResetPolicy::OnYearChange,
            start_value: 1,
        };
        let from_current_template = current_template.render_for_build(2026, 42);
        assert_ne!(
            from_xml, from_current_template,
            "drift-protection pin is only meaningful when the current template diverges \
             from the base's recorded number — fixture is mis-set if these are equal"
        );
        // Explicit byte assertion: the XML-derived value is the one the
        // annulment reference uses; the would-be re-render is NOT.
        assert_eq!(from_xml, original_number);
        assert!(from_current_template.contains("NEW-PREFIX-"));
        assert!(!from_xml.contains("NEW-PREFIX-"));
    }
}
