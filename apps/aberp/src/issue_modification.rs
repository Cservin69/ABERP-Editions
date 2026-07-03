//! Orchestration for the `aberp issue-modification` subcommand
//! (PR-11, ADR-0009 §6, ADR-0024).
//!
//! Structural parallel to `apps/aberp/src/issue_storno.rs` — same
//! `run` / `run_single_tx` split, same idempotency-replay branch,
//! same drop-then-reopen pattern for the post-commit chain
//! verification. Diverges from STORNO in three places (ADR-0024 §1,
//! §3, §4, §6):
//!
//!   - Precondition walker accepts BOTH a `Finalized` base (last ack
//!     `SAVED`) and an already-`Amended` base (prior
//!     `InvoiceModificationIssued` entry against the base). Rejects
//!     a `Storno`-cancelled base loudly per ADR-0024 §6 — modifying
//!     a base that has been legally cancelled is malformed.
//!
//!   - The XML emitter is [`nav_xml::render_modification_data`] —
//!     full-replace body (NOT negated like STORNO), with an
//!     `<invoiceReference>` block byte-identical to STORNO's (S381/F1 —
//!     the v2.0-only `<modificationIssueDate>` was removed; the wire
//!     operation is derived from the audit ledger by
//!     `submission_queue::operation_for_invoice`, not from the body).
//!
//!   - The chain-index allocator walks BOTH `InvoiceStornoIssued` AND
//!     `InvoiceModificationIssued` payloads against the same base
//!     (ADR-0024 §7); the symmetric walker in `issue_storno.rs` was
//!     widened in the same commit. Both must stay in sync.
//!
//! Pipeline:
//!
//! 1. Parse the JSON input via [`crate::issue_invoice::InvoiceInputJson`]
//!    (same shape as `issue-invoice --in` / `issue-storno --in`).
//! 2. Validate the operator-supplied `--modification-date` is a
//!    well-formed `YYYY-MM-DD` per ADR-0024 §1 (no silent
//!    today-default — CLAUDE.md rule 4 + 12).
//! 3. Resolve tenant id and series code.
//! 4. Load NAV credentials from the OS keychain (Actor identity
//!    discipline; closes F15 the same way `issue_storno.rs` does).
//! 5. Compute the binary hash + ledger meta.
//! 6. Pre-flight precondition: walk the audit ledger (read-only
//!    `Ledger::open`) and confirm the `--references` target carries
//!    either a `SAVED` last-ack OR a prior MODIFY chain entry, AND
//!    has NOT been stornoed.
//! 7. Pre-tx setup: schemas + series (same helper shape as STORNO).
//! 8. Open one DuckDB transaction; under it: load the base row,
//!    walk the chain (both kinds), allocate, write three audit
//!    entries (`InvoiceSequenceReserved`, `InvoiceDraftCreated`,
//!    `InvoiceModificationIssued`), commit.
//! 9. Drop the Connection, re-open `Ledger`, verify the chain.
//! 10. Render the modification's `<InvoiceData>` XML via
//!     [`nav_xml::render_modification_data`], run the ADR-0022
//!     runtime XSD invariant check, write to disk.
//!
//! # `modification_index` allocator — local-base path only (PR-11)
//!
//! Same posture as `issue_storno.rs`'s local-base path. The
//! migrated-from-Billingo `queryInvoiceChainDigest` path is the same
//! deferral (ADR-0023 §4 + ADR-0024 §7 / F23) — the local invoice
//! schema has no `origin` column today, so the migrated-base
//! conditional never fires.

#[allow(unused_imports)]
use duckdb::Connection;
use std::path::PathBuf;

use aberp_audit_ledger::{self as audit_ledger, Actor, EventKind, Ledger, LedgerMeta, TenantId};
use aberp_billing::{
    self as billing, AllocateArgs, AllocateOutcome, BillingStore, Currency, CustomerId,
    DraftInvoice, DuckDbBillingStore, Huf, IdempotencyKey, InvoiceId, InvoiceSeries,
    IssueInvoiceCommand, LineItem, RateMetadata, ReadyInvoice, ResetPolicy, SeriesCode, SeriesId,
};
use aberp_nav_transport::NavCredentials;
use anyhow::{anyhow, bail, Context, Result};
use time::macros::format_description;
use time::Date;
use time::OffsetDateTime;
use ulid::Ulid;

use crate::audit_payloads;
use crate::binary_hash;
use crate::cli::IssueModificationArgs;
use crate::invoice_bank_snapshot::load_invoice_bank_snapshot_in_tx;
use crate::invoice_currency_metadata::{
    inherit_rate_metadata_for_chain, load_invoice_currency_metadata_in_tx,
    require_chain_currency_match,
};
use crate::issue_invoice::InvoiceInputJson;
use crate::nav_xml::{
    self, CustomerAddress, CustomerInfo, ModificationReference, NavParties, SupplierInfo,
};

// ──────────────────────────────────────────────────────────────────────
// Entry point
// ──────────────────────────────────────────────────────────────────────

pub fn run(args: &IssueModificationArgs) -> Result<()> {
    let _span = tracing::info_span!("issue_modification").entered();

    // 1. Read + parse the JSON input.
    let input_bytes = std::fs::read(&args.r#in)
        .with_context(|| format!("read input JSON from {}", args.r#in.display()))?;
    let input: InvoiceInputJson =
        serde_json::from_slice(&input_bytes).context("parse input JSON")?;
    tracing::info!(lines = input.lines.len(), "JSON input parsed");

    // 2. Load NAV credentials BEFORE any DB write — Actor identity
    //    discipline (closes F15 the same way issue_storno does).
    //    PR-47β / session-65: the actor derivation moves here (the CLI
    //    wrapper) so the library helper `modification_from_inputs` can
    //    be called from the SPA route with a pre-loaded actor (the
    //    route mints its own Actor from AppState's startup-cached
    //    operator_login; modification — like storno — does not call
    //    NAV at issuance time per ADR-0024 §3, so the route does not
    //    even need fresh credentials).
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

    // ADR-0098 C2 — the one-shot CLI path builds its own shared Handle (same
    // dual-use resolution as poll_ack/submit/storno run()).
    let tenant_for_handle = TenantId::new(args.tenant.clone())
        .ok_or_else(|| anyhow!("tenant value '{}' is empty or has a null byte", args.tenant))?;
    let db_handle = aberp_db::Handle::open_default(&args.db, tenant_for_handle)
        .with_context(|| format!("open shared DuckDB handle at {}", args.db.display()))?;
    let summary = modification_from_inputs(
        input,
        &db_handle,
        &args.tenant,
        &args.series,
        &args.references,
        &args.modification_date,
        args.out.clone(),
        actor,
    )?;

    println!(
        "issued modification {} -> {} (references {} as modificationIndex {} \
         on {}, audit chain verified across {} entries)",
        summary.invoice_number,
        args.out.display(),
        args.references,
        summary.modification_index,
        args.modification_date,
        summary.entries_verified,
    );
    Ok(())
}

/// PR-47β / session-65 — operator-visible summary of a single
/// modification issuance, returned by [`modification_from_inputs`].
/// Mirrors [`crate::issue_storno::StornoIssuedSummary`] field-for-field
/// so the SPA route surfaces a uniform wire shape across the two
/// chain-child actions; the only domain-relevant difference is the
/// audit kind on the chain link (`InvoiceModificationIssued` vs
/// `InvoiceStornoIssued`).
#[derive(Debug, Clone)]
pub struct ModificationIssuedSummary {
    /// Prefixed-ULID id of the modification invoice itself
    /// (`inv_<ULID>`). Distinct from the BASE invoice's id.
    pub invoice_id: String,
    /// NAV-facing number of the modification
    /// (`<series>/<5-digit-seq>`).
    pub invoice_number: String,
    /// 1-based chain index allocated to this modification per
    /// ADR-0024 §7 (`modificationIndex` on the wire). Shared name
    /// space with prior storno entries against the same base.
    pub modification_index: u32,
    /// Ledger entry count `verify_chain` walked post-commit.
    pub entries_verified: u64,
}

/// PR-47β / session-65 — library helper that wires the modification
/// pipeline over an already-parsed `InvoiceInputJson` + an already-
/// derived `Actor`. The CLI's `run` calls into this after parsing
/// `--in` and loading NAV credentials; the SPA route calls into this
/// with the operator-edited body and an Actor minted from
/// `AppState::operator_login`.
///
/// Mirrors `issue_storno::storno_from_inputs` per the same library-
/// helper posture (A159 / PR-47α). `pub` so the integration test
/// (`tests/serve_modification_route.rs`) can drive it without
/// spinning the HTTPS listener.
///
/// Steps 1 + 3-10 from the pre-PR-47β `run` body, moved here verbatim.
/// Step 2 (NAV-creds + actor derivation) stays on the caller because
/// the SPA route mints its Actor differently (per-request, from
/// `operator_login`).
#[allow(clippy::too_many_arguments)]
pub fn modification_from_inputs(
    input: InvoiceInputJson,
    db: &aberp_db::HandleArc,
    tenant_str: &str,
    series_str: &str,
    references: &str,
    modification_date: &str,
    nav_xml_out: PathBuf,
    actor: Actor,
) -> Result<ModificationIssuedSummary> {
    if input.lines.is_empty() {
        return Err(anyhow!("input has no lines"));
    }

    // Validate modification_date is canonical YYYY-MM-DD per ADR-0024
    // §1. No silent today-default. The parsed Date is discarded (the
    // audit payload stores the original String per ADR-0024 §5).
    let date_format = format_description!("[year]-[month]-[day]");
    let _parsed_date = Date::parse(modification_date, &date_format).map_err(|e| {
        anyhow!(
            "modification_date '{}' is not a well-formed YYYY-MM-DD: {e} \
             (ADR-0024 §1 requires explicit operator-supplied date — no silent default)",
            modification_date
        )
    })?;

    // Resolve tenant id + series code.
    let tenant = TenantId::new(tenant_str.to_string())
        .ok_or_else(|| anyhow!("tenant value '{}' is empty or has a null byte", tenant_str))?;
    let series_code = SeriesCode::new(series_str.to_string())
        .ok_or_else(|| anyhow!("series value '{}' fails SeriesCode validation", series_str))?;
    if !references.starts_with("inv_") {
        bail!(
            "references value '{}' is not a prefixed invoice id (expected inv_<ULID>)",
            references
        );
    }

    // Compute binary hash + ledger meta.
    let binary_hash_bytes = binary_hash::compute().context("compute binary hash")?;
    let ledger_meta = LedgerMeta::new(tenant.clone(), binary_hash_bytes);

    // ADR-0031 §5 pre-allocation hard-cap check.
    let pending_count =
        crate::submission_queue::count_pending(db, tenant.clone(), binary_hash_bytes)
            .context("count pending submissions (ADR-0031 §5 cap check) for modification")?;
    if pending_count >= crate::submission_queue::HARD_CAP_PENDING {
        return Err(anyhow!(
            "submission queue is full ({}/{} pending invoices per ADR-0009 §7 / ADR-0031 §5); \
             run `aberp drain-submission-queue --endpoint <test|production> --tax-number ...` \
             to submit the backlog before issuing a modification",
            pending_count,
            crate::submission_queue::HARD_CAP_PENDING,
        ));
    }

    // Pre-flight precondition: base is Finalized OR already Amended,
    // and not Storno-cancelled. Read-only ledger; drop before opening
    // the write transaction.
    // S184 — same read pass extracts the BASE invoice's on-disk NAV XML
    // path so the modification emit reads the base's actual
    // `<invoiceNumber>` for the chain reference instead of re-deriving
    // via the current numbering template. See `find_base_nav_xml_path`
    // in `issue_storno.rs` for the rationale + the NAV ABORTED ack that
    // forced this change (INVALID_INVOICE_REFERENCE on seller.toml
    // literal drift).
    // S391/A — alongside the base NAV XML path, fold the TOTAL prior
    // chain line count (base + every SAVED prior modification's lines)
    // off the same read-only ledger handle. A modify-after-modify chain
    // must offset its CREATE lines' `<lineNumberReference>` past every
    // saved prior modification, not just the base — the MODIFY mirror of
    // the S384/F5 storno fix. Resolved PRE-tx (the render closure has no
    // tx handle), same discipline as the storno path.
    let (base_nav_xml_path, total_prior_chain_line_count) = {
        // ADR-0098 C2 — read the chain via a shared read clone (from_connection),
        // not an independent Ledger::open of the live path.
        let pre_conn = db
            .read()
            .context("shared read: modification precondition check (ADR-0098 Gap 1a C2)")?;
        let ledger = Ledger::from_connection(pre_conn, tenant.clone(), binary_hash_bytes);
        check_base_is_modifiable(&ledger, references)?;
        let base_path = crate::issue_storno::find_base_nav_xml_path_for_chain(&ledger, references)?;
        let total =
            crate::issue_storno::total_prior_chain_line_count(&ledger, &base_path, references)?;
        (base_path, total)
        // ledger drops here, releasing the DuckDB read connection
    };

    // PR-90 / ADR-0045 §2 — resolve the operator's numbering template
    // once. Drives both the series's reset_policy sync in
    // `ensure_series` AND the rendered modification + base invoice
    // numbers below. Loud-fail on parse error (no silent fallback —
    // CLAUDE.md rule 12).
    let seller_toml_path = crate::setup_seller_info::seller_toml_path_for_tenant(tenant_str)
        .context("resolve seller.toml path for numbering template")?;
    let template = crate::numbering::read_numbering_template(&seller_toml_path)
        .context("read [seller.numbering] template from seller.toml")?;

    // Pre-tx setup: schemas + series.
    let series = pre_tx_setup(db, &series_code, template.reset_policy.to_billing())?;

    // Build the modification command.
    let command = build_modification_command(&input, &series_code)?;
    let idempotency_key = command.idempotency_key;
    let issue_date = OffsetDateTime::now_utc();
    // PR-84 — MODIFICATION chains default both invoice-date fields to
    // the server-clock issue date. Same posture as
    // `apps/aberp/src/issue_storno.rs`: the modification UX does not
    // yet surface operator-supplied delivery / payment pickers
    // (out of scope per the PR-84 brief — "keep PR-84 to the issue
    // path"); preserving pre-PR-84 wire behaviour is surgical.
    let default_calendar_date = issue_date.date();
    let draft = DraftInvoice {
        id: InvoiceId::new(),
        series_id: series.id,
        customer_id: command.customer_id,
        lines: command.lines,
        issue_date,
        payment_deadline: default_calendar_date,
        delivery_date: default_calendar_date,
    };
    // PR-44γ.1 — placeholder defaults; `run_single_tx` reads the
    // base's stored currency metadata and overrides per ADR-0037 §4
    // invariant C6 (chain-currency-match by inheritance).
    // PR-73 / ADR-0040 §addendum — same inheritance posture for the
    // bank-account snapshot: the modification chain inherits the base
    // invoice's bank-account quintet inside `run_single_tx`, so the
    // placeholder here is `None`. Diverges from the SPA-issue route
    // (`serve.rs::issue_from_parsed`) which resolves a snapshot from
    // the route's `bank_account_id` request field, because the
    // modification flow's regulatory record IS the base invoice's
    // bank account — the operator cannot choose a different one.
    let allocate_args = AllocateArgs {
        series_id: series.id,
        draft,
        idempotency_key,
        currency: Currency::Huf,
        rate_metadata: None,
        bank_snapshot: None,
        // PR-82 — modification-level note ("Megjegyzés") is out of
        // scope for PR-82's chain paths. Per-line notes inherited from
        // the base flow through `draft.lines[i].note` naturally.
        // Chain-level note threading lands at PR-83.
        invoice_note: None,
        // PR-203 / S203 — operator-typed per-modification email recipient
        // override. The SPA's modification form re-binds this from the
        // operator's edit (validated at the route boundary); CLI
        // modifications inherit the base's value through the base's
        // side-stored `input.json` round-trip (default `None` for
        // pre-PR-203 bases). Persisted on the modification's OWN
        // `invoice.email_recipient_override` row — independent of the
        // base's value going forward.
        email_recipient_override: input.email_recipient_override.clone(),
        // PR-90 — operator-configured counter seed. Modification burns
        // its own sequence number from the same `(series, fiscal_year)`
        // bucket; `start_value` only applies on the bucket's first
        // INSERT, so a modification landing in an existing bucket is
        // unaffected.
        start_value: template.start_value,
        // S392 — the NAV `queryInvoiceCheck` pre-flight gates the issue
        // path only; modification chains are out of scope here, so no
        // floor is forced (numbers advance from the stored counter).
        sequence_floor: None,
    };

    // S375 — read the BASE invoice's NAV-side identity from its on-disk
    // XML BEFORE the tx so the render closure (run inside the tx, before
    // commit) has everything it needs. S184 (`<invoiceNumber>`, immune to
    // seller.toml numbering-literal drift) + S369 (base line count, so
    // the modification's full-replace CREATE lines continue PAST the
    // base's line numbers — NAV INVOICE_LINE_ALREADY_EXISTS / S370) —
    // the same canonical-record reads issue_storno does pre-tx.
    let base_invoice_number = crate::nav_xml::read_invoice_number_from_xml(&base_nav_xml_path)?;
    // S391/A — `total_prior_chain_line_count` (base + SAVED prior mods)
    // was folded pre-tx above; it is the `<lineNumberReference>` offset.

    // S375 — build the modification's NAV parties BEFORE the tx; the
    // render closure captures them by move.
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
            // PR-97 / ADR-0048 — inherit the base invoice's
            // `customer.vat_status` so the modification's wire body
            // mirrors the base's PRIVATE_PERSON / DOMESTIC shape. Same
            // back-compat posture as the storno path.
            customer_vat_status: input.customer.vat_status,
            tax_number: {
                let trimmed = input.customer.tax_number.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                }
            },
            name: input.customer.name,
            // PR-77 / session-101 — same `customerAddress` inheritance
            // posture as the storno path; the modification's parties
            // come from the operator-supplied (or reconstructed) base
            // invoice content, which now carries the address shape.
            address: input.customer.address.map(|a| CustomerAddress {
                country_code: a.country_code,
                postal_code: a.postal_code,
                city: a.city,
                street: a.street,
            }),
        },
    };
    let render_series_code = series_code.clone();
    let render_payment_method = input.payment_method;
    let render_nav_out = nav_xml_out.clone();
    // S375 — render+XSD-validate+write the modification's <InvoiceData>
    // INSIDE the tx, AFTER the three audit appends and BEFORE commit, so
    // a render/validate/write failure rolls the allocation + appends back
    // (atomic). `issue_modification.rs` was the last un-ported sibling of
    // the pre-S375 storno bug (S382/F1 / task_8de31599): pre-S375 this
    // ran post-commit, so a failure left a committed modification row +
    // audit chain-link with no XML on disk — a phantom row whose Submit
    // is broken. The chain-dependent values (modification seq,
    // modification_index, inherited currency + rate metadata) are passed
    // in by `run_single_tx` because they are only known inside the tx.
    // Captures `template` by move — its only remaining use is here.
    let render_and_write = move |modification: &ReadyInvoice,
                                 modification_index: u32,
                                 chain_currency: Currency,
                                 chain_rate_metadata: Option<&RateMetadata>|
          -> Result<String> {
        let modification_invoice_number =
            template.render_for_build(modification.issue_date.year(), modification.sequence_number);
        let modification_reference = ModificationReference {
            base_invoice_number,
            modification_index,
            // S391/A — total prior chain line count (base + SAVED prior
            // modifications), not the base-only count, so a modify-after-
            // modify chain's lines continue past everything NAV holds.
            base_line_count: total_prior_chain_line_count,
        };
        let xml = nav_xml::render_modification_data_with_number(
            modification,
            &render_series_code,
            &parties,
            &modification_reference,
            chain_currency,
            chain_rate_metadata,
            // S160 — the modification inherits the base invoice's payment
            // method, which rides the base's side-stored `input.json`
            // (defaults to `Transfer` for pre-S160 bases).
            render_payment_method,
            Some(&modification_invoice_number),
        )
        .context("render NAV modification XML")?;
        aberp_nav_xsd_validator::validate_invoice_data(&xml).context(
            "NAV InvoiceData v3.0 invariant check (ADR-0022) failed for rendered modification XML",
        )?;
        tracing::info!(
            bytes = xml.len(),
            nav_xsd_version = aberp_nav_xsd_validator::NAV_XSD_VERSION,
            "NAV modification InvoiceData XML passed v3.0 invariant check"
        );
        nav_xml::write_to_path(&render_nav_out, &xml)?;
        tracing::info!(
            path = %render_nav_out.display(),
            bytes = xml.len(),
            "NAV modification XML written"
        );
        Ok(modification_invoice_number)
    };

    // One transaction across base-load + chain-index walk + modification
    // allocator + three audit appends + (S375) the NAV-XML render+write.
    // `run_single_tx` runs the render closure before committing and hands
    // the post-commit Connection back so the verify path below reuses it
    // (no crash-prone Ledger::open re-open).
    let outcome = run_single_tx(
        db,
        &ledger_meta,
        allocate_args,
        idempotency_key,
        actor,
        references,
        modification_date,
        nav_xml_out.clone(),
        // PR-97 / ADR-0048 — pass buyer-kind discriminator from the
        // base's side-stored input.json through to the audit payload.
        input.customer.vat_status,
        render_and_write,
    )?;

    let modification = outcome.modification;
    let modification_index = outcome.modification_index;
    tracing::info!(
        seq = modification.sequence_number,
        modification_index,
        base_sequence_number = outcome.base_sequence_number,
        fresh = outcome.was_fresh,
        idempotency_key = ?idempotency_key,
        "modification issued"
    );

    // Verify the audit chain — success-criterion gate. S375 — run
    // `verify_chain` + `sync_mirror` on the SAME post-commit Connection
    // `run_single_tx` handed back, rather than dropping it and calling
    // `Ledger::open` (a fresh `Connection::open` that triggers DuckDB
    // 1.5.x's LoadCheckpoint/ReadIndex ART assertion, S332 / duckdb#23046
    // — the same crash family S375 closed for invoice + storno). No file
    // re-open → that crash is unreachable.
    // ADR-0098 C2 — verify via a shared READ clone (Ledger::from_connection);
    // the mirror was already synced on the run_single_tx WriteGuard drop. No
    // independent Connection::open / Ledger::open re-open.
    let verify_conn = db
        .read()
        .context("shared read: verify chain after modification issuance (ADR-0098 Gap 1a C2)")?;
    let ledger = Ledger::from_connection(verify_conn, tenant.clone(), binary_hash_bytes);
    let verified = ledger
        .verify_chain()
        .context("audit-ledger chain verification failed AFTER modification issuance")?;
    tracing::info!(entries_verified = verified, "audit chain verified");

    // S174 — surface the SAME render that flowed to NAV on the
    // operator-visible summary (the render closure built it inside the tx
    // and returned it on `TxOutcome::modification_invoice_number`).
    Ok(ModificationIssuedSummary {
        invoice_id: modification.id.to_prefixed_string(),
        invoice_number: outcome.modification_invoice_number,
        modification_index,
        entries_verified: verified,
    })
}

// ──────────────────────────────────────────────────────────────────────
// Pre-flight precondition: base must be Finalized OR Amended, and
// not Storno-cancelled. ADR-0024 §6.
// ──────────────────────────────────────────────────────────────────────

/// Walk the audit ledger and confirm that `base_invoice_id` is in a
/// modifiable state per ADR-0024 §6:
///
///   - Has an `InvoiceSubmissionResponse` (was submitted).
///   - Not abandoned (no `InvoiceMarkedAbandoned`).
///   - Not stornoed (no `InvoiceStornoIssued` against it as base).
///   - Last ack is `SAVED` (Finalized) — OR — at least one prior
///     `InvoiceModificationIssued` against the base exists (Amended).
///   - Last ack is NOT `ABORTED` (Rejected); NOT a non-terminal value
///     (Stuck).
///
/// Loud-fail with a specific named-reason message per CLAUDE.md
/// rule 12.
fn check_base_is_modifiable(ledger: &Ledger, base_invoice_id: &str) -> Result<()> {
    let entries = ledger
        .entries()
        .context("read audit ledger entries for modification precondition check")?;

    let mut has_marked_abandoned = false;
    let mut has_submission_response = false;
    let mut has_storno = false;
    let mut has_prior_modification = false;
    let mut latest_ack_status: Option<String> = None;

    for entry in &entries {
        match entry.kind {
            EventKind::InvoiceMarkedAbandoned => {
                let payload: audit_payloads::InvoiceMarkedAbandonedPayload =
                    serde_json::from_slice(&entry.payload).map_err(|e| {
                        anyhow!(
                            "InvoiceMarkedAbandoned audit payload (seq {}) failed typed decode: {e} \
                             — audit ledger appears tampered or schema-drifted",
                            entry.seq.as_u64()
                        )
                    })?;
                if payload.invoice_id == base_invoice_id {
                    has_marked_abandoned = true;
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
                    has_submission_response = true;
                }
            }
            EventKind::InvoiceAckStatus => {
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
            EventKind::InvoiceStornoIssued => {
                let payload: audit_payloads::InvoiceStornoIssuedPayload =
                    serde_json::from_slice(&entry.payload).map_err(|e| {
                        anyhow!(
                            "InvoiceStornoIssued audit payload (seq {}) failed typed decode: {e} \
                             — audit ledger appears tampered or schema-drifted",
                            entry.seq.as_u64()
                        )
                    })?;
                if payload.base_invoice_id == base_invoice_id {
                    has_storno = true;
                }
            }
            EventKind::InvoiceModificationIssued => {
                let payload: audit_payloads::InvoiceModificationIssuedPayload =
                    serde_json::from_slice(&entry.payload).map_err(|e| {
                        anyhow!(
                            "InvoiceModificationIssued audit payload (seq {}) failed typed decode: {e} \
                             — audit ledger appears tampered or schema-drifted",
                            entry.seq.as_u64()
                        )
                    })?;
                if payload.base_invoice_id == base_invoice_id {
                    has_prior_modification = true;
                }
            }
            _ => {}
        }
    }

    if has_marked_abandoned {
        bail!(
            "base invoice {} is ABANDONED (operator previously ran \
             `aberp mark-abandoned`); no modification can be issued against it",
            base_invoice_id
        );
    }
    if has_storno {
        bail!(
            "base invoice {} has been legally cancelled by a storno — \
             a modification against a stornoed base is malformed; issue \
             a fresh corrective new invoice instead (ADR-0024 §6)",
            base_invoice_id
        );
    }
    if !has_submission_response {
        bail!(
            "base invoice {} has no NAV submission response on record — \
             run `aberp submit-invoice` and `aberp poll-ack` first \
             to finalize it before issuing a modification",
            base_invoice_id
        );
    }
    match latest_ack_status.as_deref() {
        Some("SAVED") => Ok(()),
        Some("ABORTED") => bail!(
            "base invoice {} was REJECTED by NAV (last ack: ABORTED) — \
             a modification is only valid against a SAVED (finalized) invoice; \
             issue a corrective new invoice instead",
            base_invoice_id
        ),
        Some(other) => bail!(
            "base invoice {} is STUCK (last ack: {}) — finalize it via \
             `aberp poll-ack` (or unblock via `aberp retry-submission`) \
             before issuing a modification; modification against a not-yet- \
             finalized invoice is rejected per ADR-0024 §6",
            base_invoice_id,
            other
        ),
        None => {
            // No ack on record. Permit IFF there is at least one
            // prior MODIFY against this base — that means the base is
            // in the derived `Amended` state per ADR-0024 §2, which
            // is modifiable (modify-after-modify is permitted by
            // default — ADR-0024 §6 + §8 default-permit posture).
            if has_prior_modification {
                Ok(())
            } else {
                bail!(
                    "base invoice {} has a submission response but no ack status \
                     and no prior modification — run `aberp poll-ack` first; \
                     modification requires the base to be Finalized (SAVED) or \
                     already Amended (ADR-0024 §6)",
                    base_invoice_id
                )
            }
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Pre-tx setup — same shape as issue_storno.rs / issue_invoice.rs
// ──────────────────────────────────────────────────────────────────────

fn pre_tx_setup(
    db: &aberp_db::HandleArc,
    series_code: &SeriesCode,
    template_reset_policy: ResetPolicy,
) -> Result<InvoiceSeries> {
    // ADR-0098 C2 — billing schema + series setup through the shared Handle's
    // writer window. The billing store needs an OWNED Connection, so it runs on
    // a `try_clone` of the shared instance: same Database, NO second OS open;
    // the CREATE TABLE + series INSERT commit to the one instance and are
    // visible to the issuance tx on the same handle (the coherence dividend,
    // aberp-db lib.rs single-instance). The guard drop fires the post-commit
    // hook (cheap: schema/series only).
    let guard = db
        .write()
        .context("shared writer: billing+audit schema & series setup (ADR-0098 Gap 1a C2)")?;
    let setup_conn = guard
        .try_clone()
        .context("try_clone shared instance for billing store setup (ADR-0098 C2)")?;
    let mut billing = DuckDbBillingStore::from_connection(setup_conn);
    billing.ensure_schema().context("ensure billing schema")?;
    let series = ensure_series(&mut billing, series_code, template_reset_policy)?;
    audit_ledger::ensure_schema(&guard).context("ensure audit-ledger schema")?;
    Ok(series)
}

/// PR-90 / ADR-0045 §2 — same shape as `issue_invoice::ensure_series`
/// and `issue_storno::ensure_series`.
fn ensure_series<S: BillingStore + ?Sized>(
    store: &mut S,
    code: &SeriesCode,
    template_reset_policy: ResetPolicy,
) -> Result<InvoiceSeries> {
    if let Some(mut series) = store.find_series_by_code(code)? {
        if series.reset_policy != template_reset_policy {
            tracing::info!(
                series = code.as_str(),
                from = ?series.reset_policy,
                to = ?template_reset_policy,
                "syncing series.reset_policy to template choice (PR-90)"
            );
            store
                .update_series_reset_policy(series.id, template_reset_policy)
                .context("sync series.reset_policy to template")?;
            series.reset_policy = template_reset_policy;
        }
        return Ok(series);
    }
    let series = InvoiceSeries {
        id: SeriesId::new(),
        code: code.clone(),
        reset_policy: template_reset_policy,
        fiscal_year: None,
        created_at: OffsetDateTime::now_utc(),
    };
    store.create_series(&series).context("create series")?;
    tracing::info!(
        series = code.as_str(),
        reset_policy = ?template_reset_policy,
        "auto-created series"
    );
    Ok(series)
}

// ──────────────────────────────────────────────────────────────────────
// The single transaction — base load, chain-index allocation,
// modification allocator, three audit appends.
// ──────────────────────────────────────────────────────────────────────

struct TxOutcome {
    modification: ReadyInvoice,
    modification_index: u32,
    base_sequence_number: u64,
    was_fresh: bool,
    /// S375 — the modification's `<series>/<seq>` NAV number, rendered
    /// inside the tx by the `render_and_write` closure and returned so
    /// the caller threads it straight into the operator-facing summary
    /// (no second template render). The inherited currency + rate
    /// metadata the closure used are consumed inside the tx and no longer
    /// cross the commit boundary. Mirrors
    /// `issue_storno::TxOutcome::storno_invoice_number`.
    modification_invoice_number: String,
}

/// One DuckDB transaction across: load base row, walk chain (both
/// kinds), allocate modification, write three audit entries, (S375)
/// render+validate+write the NAV XML, commit, and hand the post-commit
/// `Connection` back. Rollback contract matches
/// `issue_storno::run_single_tx` (drop-on-error rolls back both halves;
/// a render `Err` arriving before `tx.commit()` rolls everything back).
#[allow(clippy::too_many_arguments)]
fn run_single_tx<F>(
    db: &aberp_db::HandleArc,
    ledger_meta: &LedgerMeta,
    mut allocate_args: AllocateArgs,
    idempotency_key: IdempotencyKey,
    actor: Actor,
    base_invoice_id: &str,
    modification_issue_date: &str,
    nav_xml_path: std::path::PathBuf,
    // PR-97 / ADR-0048 — buyer-kind discriminator inherited from the
    // base invoice's side-stored input.json. Stamped onto the
    // modification's `InvoiceDraftCreated` audit payload alongside the
    // bank snapshot via the chainable builders.
    customer_vat_status: crate::nav_xml::CustomerVatStatus,
    // S375 — NAV-XML render+validate+write step, run inside the tx AFTER
    // the three audit appends and BEFORE commit so the modification is
    // atomic. Receives the chain-dependent values only known inside the
    // tx (modification seq via `&ReadyInvoice`, modification_index,
    // inherited currency + rate metadata) and returns the rendered NAV
    // invoice number.
    render_and_write: F,
) -> Result<TxOutcome>
where
    F: FnOnce(&ReadyInvoice, u32, Currency, Option<&RateMetadata>) -> Result<String>,
{
    // ADR-0098 C2 — the issuance tx runs on the shared Handle's serialized
    // writer; the WriteGuard drop fires the post-commit hook. The render closure
    // runs inside this tx (sync, no await) so the writer covers the issuance only.
    let mut conn = db
        .write()
        .context("shared writer: modification issuance tx (ADR-0098 Gap 1a C2)")?;
    let tx = conn
        .transaction()
        .context("begin DuckDB transaction (modification: billing + audit-ledger)")?;

    // (a) Load the base row to capture its NAV-facing sequence
    //     number for the chain-link payload's `base_sequence_number`
    //     (denormalized per ADR-0023 §3, carried forward to MODIFY
    //     per ADR-0024 §5). Loud-fail if the row is absent.
    let (base_invoice, _base_idem) = billing::load_ready_invoice_by_id(&tx, base_invoice_id)
        .context("billing::load_ready_invoice_by_id (modification base)")?
        .ok_or_else(|| {
            anyhow!(
                "base invoice {} exists in audit ledger but not in billing table — \
                 tenant DB appears tampered between precondition check and modification tx",
                base_invoice_id
            )
        })?;
    let base_sequence_number = base_invoice.sequence_number;

    // (a') PR-44γ.1 — read the base invoice's stored currency + rate
    //      metadata and inherit onto the modification. The modification
    //      is full-replace per ADR-0024 §4, so `huf_equivalent_total`
    //      is computed against the modification's OWN positive gross.
    let base_currency_metadata = load_invoice_currency_metadata_in_tx(&tx, base_invoice_id)
        .context("load base invoice currency metadata for modification (ADR-0037 §4 C6)")?;
    let modification_gross_cents: i64 =
        allocate_args
            .draft
            .lines
            .iter()
            .try_fold(0i64, |acc, line| {
                let line_gross = line
                    .gross_total()
                    .ok_or_else(|| anyhow!("modification line gross_total overflow"))?;
                acc.checked_add(line_gross.as_i64())
                    .ok_or_else(|| anyhow!("modification gross accumulator overflow"))
            })?;
    let (inherited_currency, inherited_rate_metadata) =
        inherit_rate_metadata_for_chain(&base_currency_metadata, modification_gross_cents)
            .context("inherit rate metadata for modification chain child")?;
    allocate_args.currency = inherited_currency;
    allocate_args.rate_metadata = inherited_rate_metadata.clone();
    // PR-73 / ADR-0040 §addendum — chain children inherit the BASE
    // invoice's bank-account snapshot verbatim. Same posture as
    // `issue_storno.rs` — the regulatory record is "the bank account
    // the base asked to be paid to"; re-resolving against current
    // `seller.toml` could surface a different account if the operator
    // rotated the per-currency default between base issuance and
    // modification. A `None` snapshot (pre-PR-73 base) propagates as
    // `None`.
    let inherited_bank_snapshot = load_invoice_bank_snapshot_in_tx(&tx, base_invoice_id)
        .context("load base invoice bank snapshot for modification chain inheritance")?
        .into_typed();
    allocate_args.bank_snapshot = inherited_bank_snapshot.clone();
    require_chain_currency_match(
        base_currency_metadata.currency,
        allocate_args.currency,
        base_invoice_id,
    )?;

    // (b) Walk the audit ledger inside the SAME tx for prior chain
    //     entries (BOTH kinds per ADR-0024 §7) against this base.
    //     Allocate modification_index = max + 1 (or 1 if empty).
    let modification_index = next_modification_index_in_tx(&tx, base_invoice_id)?;

    // (c) Standard allocator path: burn the modification's own
    //     sequence number + write its reservation + invoice rows.
    let now = OffsetDateTime::now_utc();
    let outcome = billing::allocate_in_tx(&tx, allocate_args, now)
        .context("billing::allocate_in_tx (modification)")?;

    let (modification_invoice, reservation, was_fresh) = match outcome {
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

        // 1) InvoiceSequenceReserved for the MODIFICATION's own sequence.
        let seq_payload = audit_payloads::InvoiceSequenceReservedPayload::from_outcome(
            &modification_invoice,
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
        .context("audit_ledger::append_in_tx InvoiceSequenceReserved (modification)")?;

        // 2) InvoiceDraftCreated for the MODIFICATION. PR-18 /
        //    ADR-0031 §2 — record the operator's --out path on
        //    the audit payload.
        //
        //    PR-44γ.1 / ADR-0037 — non-HUF MODIFY stamps the inherited
        //    currency + rate metadata via `from_invoice_with_rate`; HUF
        //    keeps the existing PR-18 path. Same shape as issue_storno's
        //    branch.
        let draft_payload = if let Some(rate) = inherited_rate_metadata.as_ref() {
            audit_payloads::InvoiceDraftCreatedPayload::from_invoice_with_rate(
                &modification_invoice,
                idempotency_key,
                Some(nav_xml_path),
                inherited_currency,
                rate,
            )
        } else {
            audit_payloads::InvoiceDraftCreatedPayload::from_invoice_with_xml_path(
                &modification_invoice,
                idempotency_key,
                nav_xml_path,
            )
        }
        // PR-73 / ADR-0040 §addendum — inherit the base's bank-account
        // snapshot onto the modification's audit payload.
        .with_bank_snapshot(inherited_bank_snapshot.as_ref())
        // PR-97 / ADR-0048 — stamp buyer-kind discriminator inherited
        // from the base invoice.
        .with_customer_vat_status(customer_vat_status);
        audit_ledger::append_in_tx(
            &tx,
            ledger_meta,
            EventKind::InvoiceDraftCreated,
            draft_payload.to_bytes(),
            actor.clone(),
            Some(idem_str.clone()),
        )
        .context("audit_ledger::append_in_tx InvoiceDraftCreated (modification)")?;

        // 3) InvoiceModificationIssued — the chain-link payload.
        let modification_payload = audit_payloads::InvoiceModificationIssuedPayload::new(
            &modification_invoice.id.to_prefixed_string(),
            modification_invoice.sequence_number,
            &reservation.id.to_prefixed_string(),
            idempotency_key,
            base_invoice_id,
            base_sequence_number,
            modification_index,
            modification_issue_date,
        );
        audit_ledger::append_in_tx(
            &tx,
            ledger_meta,
            EventKind::InvoiceModificationIssued,
            modification_payload.to_bytes(),
            actor,
            Some(idem_str),
        )
        .context("audit_ledger::append_in_tx InvoiceModificationIssued")?;
    } else {
        tracing::info!("replay path: no new audit entries written (modification idempotency hit)");
    }

    // S375 — render + XSD-validate + write the modification NAV XML
    // BEFORE commit, on both the Fresh and Replay paths (matches the
    // pre-S375 unconditional post-commit render). A failure here returns
    // `Err` so the tx drops un-committed → the allocation + three appends
    // roll back together and no committed-but-XML-less modification row
    // survives.
    let modification_invoice_number = render_and_write(
        &modification_invoice,
        modification_index,
        inherited_currency,
        inherited_rate_metadata.as_ref(),
    )
    .context("render + XSD-validate + write NAV modification XML before commit (S375 atomicity)")?;

    tx.commit()
        .context("commit DuckDB transaction (modification: billing + audit-ledger)")?;
    Ok(TxOutcome {
        modification: modification_invoice,
        modification_index,
        base_sequence_number,
        was_fresh,
        modification_invoice_number,
    })
}

/// Walk `audit_ledger` inside the borrowed transaction for every
/// chain entry (BOTH `InvoiceStornoIssued` AND
/// `InvoiceModificationIssued`), decode each payload, filter by
/// `base_invoice_id`, return `max(modification_index) + 1` — or `1`
/// if no prior chain entry exists.
///
/// **Symmetric** to `issue_storno::next_modification_index_in_tx`
/// per ADR-0024 §7 — both walkers must stay in sync. NAV's
/// `modificationIndex` uniqueness is per `invoiceReference`
/// regardless of operation kind; walking only one kind would
/// re-issue an index the other kind already burned, and NAV would
/// reject at the wire (far from the allocator). The two functions
/// are NOT extracted to a shared `chain_allocator` module — that
/// would be the speculative-abstraction trap (CLAUDE.md rule 2); if
/// a third chain kind ever appears, both functions extend together
/// and the extraction trigger is named in ADR-0024 §7.
fn next_modification_index_in_tx(
    tx: &duckdb::Transaction<'_>,
    base_invoice_id: &str,
) -> Result<u32> {
    // S381/F4 — count only SAVED-confirmed chain members (symmetric with
    // `issue_storno::next_modification_index_in_tx`; see that function +
    // `issue_storno::saved_chain_member_ids_in_tx` for the rationale —
    // NAV-ABORTed attempts never burned an index).
    let saved_ids = crate::issue_storno::saved_chain_member_ids_in_tx(tx)?;
    let mut max_index: u32 = 0;

    // STORNO entries.
    {
        let mut stmt = tx
            .prepare("SELECT seq, payload FROM audit_ledger WHERE kind = ?;")
            .context("prepare audit_ledger scan for storno chain index (modification walker)")?;
        let rows = stmt
            .query_map([EventKind::InvoiceStornoIssued.as_str()], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?))
            })
            .context("query audit_ledger for storno chain index (modification walker)")?;
        for row in rows {
            let (seq, payload_bytes) =
                row.context("read audit_ledger row during storno chain-index walk (modification)")?;
            let payload: audit_payloads::InvoiceStornoIssuedPayload =
                serde_json::from_slice(&payload_bytes).map_err(|e| {
                    anyhow!(
                        "InvoiceStornoIssued audit payload (seq {seq}) failed typed decode: {e} \
                         — audit ledger appears tampered or schema-drifted"
                    )
                })?;
            if payload.base_invoice_id == base_invoice_id
                && saved_ids.contains(&payload.storno_invoice_id)
                && payload.modification_index > max_index
            {
                max_index = payload.modification_index;
            }
        }
    }

    // MODIFICATION entries.
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
                && saved_ids.contains(&payload.modification_invoice_id)
                && payload.modification_index > max_index
            {
                max_index = payload.modification_index;
            }
        }
    }

    Ok(max_index.saturating_add(1))
}

// ──────────────────────────────────────────────────────────────────────
// Modification command construction — same shape as issue_storno's
// build_storno_command. The MODIFY semantics are full-replace
// (ADR-0024 §4) — the input.lines ARE the new effective lines, no
// negation.
// ──────────────────────────────────────────────────────────────────────

fn build_modification_command(
    input: &InvoiceInputJson,
    code: &SeriesCode,
) -> Result<IssueInvoiceCommand> {
    let lines = input
        .lines
        .iter()
        .map(|l| LineItem {
            description: l.description.clone(),
            quantity: l.quantity,
            unit_price: Huf(l.unit_price),
            vat_rate_basis_points: percent_to_basis_points(l.vat_rate_percent),
            // PR-82 — pass through any per-line note. Modification
            // chains inherit the base's notes naturally; operator-
            // facing edits to per-line notes on modifications are
            // out of scope for PR-82 (PR-83 wires the storno-reason
            // surface; a future PR can extend to modification-line
            // notes if needed).
            note: l.note.clone(),
            // S159 — carry each line's unit through the modification chain
            // so the replacement lines emit the SAME `<unitOfMeasure>` as
            // the side-store `input.json` recorded at the base's issuance.
            unit: l.unit.clone(),
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
// Tests — chain-index allocator (union walk over both kinds) +
// precondition walker (modify-after-modify, storno rejection, etc.).
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use aberp_audit_ledger::{Actor, BinaryHash, Ledger, TenantId};

    /// Build a Connection-owning fixture: ensure the audit-ledger
    /// schema, then append chain entries of mixed kinds inside one
    /// tx. The `entries` slice is `(kind_label, base_invoice_id,
    /// modification_index)` — `kind_label` is `"S"` for STORNO and
    /// `"M"` for MODIFY. Returns the Connection; the caller opens
    /// its own tx to invoke `next_modification_index_in_tx`.
    fn fixture_ledger_with_mixed_chain(entries: &[(&str, &str, u32)]) -> Connection {
        let tenant = TenantId::new("t1".to_string()).unwrap();
        let bh = BinaryHash::from_bytes([0u8; 32]);
        let actor = Actor::from_local_cli("sess".to_string(), "test-user");
        let meta = LedgerMeta::new(tenant, bh);

        let mut conn = Connection::open_in_memory().unwrap();
        audit_ledger::ensure_schema(&conn).unwrap();
        {
            let tx = conn.transaction().unwrap();
            for (i, (kind_label, base, idx)) in entries.iter().enumerate() {
                let idem = IdempotencyKey::new();
                // S381/F4 — the chain member's own id; a SAVED ack is
                // appended below so the SAVED-filter in
                // `next_modification_index_in_tx` counts it (these
                // fixtures model members that reached NAV).
                let own_id = match *kind_label {
                    "S" => {
                        let own_id = format!("inv_storno_{i}");
                        let payload = audit_payloads::InvoiceStornoIssuedPayload::new(
                            &own_id,
                            100 + i as u64,
                            &format!("rsv_storno_{i}"),
                            idem,
                            base,
                            42,
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
                        own_id
                    }
                    "M" => {
                        let own_id = format!("inv_modif_{i}");
                        let payload = audit_payloads::InvoiceModificationIssuedPayload::new(
                            &own_id,
                            100 + i as u64,
                            &format!("rsv_modif_{i}"),
                            idem,
                            base,
                            42,
                            *idx,
                            "2026-05-21",
                        );
                        audit_ledger::append_in_tx(
                            &tx,
                            &meta,
                            EventKind::InvoiceModificationIssued,
                            payload.to_bytes(),
                            actor.clone(),
                            Some(idem.to_canonical_string()),
                        )
                        .unwrap();
                        own_id
                    }
                    other => panic!("unknown kind_label {other}"),
                };
                let ack = audit_payloads::InvoiceAckStatusPayload::new(
                    &own_id,
                    &format!("txn_{i}"),
                    "SAVED",
                    Vec::new(),
                );
                audit_ledger::append_in_tx(
                    &tx,
                    &meta,
                    EventKind::InvoiceAckStatus,
                    ack.to_bytes(),
                    actor.clone(),
                    None,
                )
                .unwrap();
            }
            tx.commit().unwrap();
        }
        conn
    }

    #[test]
    fn next_modification_index_starts_at_1_when_chain_is_empty() {
        let mut conn = fixture_ledger_with_mixed_chain(&[]);
        let tx = conn.transaction().unwrap();
        let idx = next_modification_index_in_tx(&tx, "inv_BASE").unwrap();
        assert_eq!(idx, 1);
    }

    /// MODIFY-only chain: prior MODIFY at 1, next MODIFY allocates 2.
    /// Pins the MODIFY half of the union walk in isolation.
    #[test]
    fn next_modification_index_increments_past_modify_only_chain() {
        let mut conn = fixture_ledger_with_mixed_chain(&[("M", "inv_BASE", 1)]);
        let tx = conn.transaction().unwrap();
        let idx = next_modification_index_in_tx(&tx, "inv_BASE").unwrap();
        assert_eq!(idx, 2);
    }

    /// **ADR-0024 §7 union-walk pin.** Mixed chain: prior STORNO at
    /// 1, prior MODIFY at 2. Next allocation must be 3 — the walker
    /// must consider both kinds. Without this, the MODIFY-only walker
    /// would see 2 and return 3 (correct by coincidence here), but
    /// the STORNO-only walker would see 1 and return 2, which would
    /// duplicate the existing MODIFY's index — exactly the
    /// `INVOICE_NUMBER_NOT_UNIQUE` failure mode the union walk is
    /// designed to prevent. CLAUDE.md rule 9: this test asserts the
    /// intent (union over both kinds), not just any allocator
    /// behaviour.
    #[test]
    fn next_modification_index_unions_storno_and_modify_against_same_base() {
        let mut conn =
            fixture_ledger_with_mixed_chain(&[("S", "inv_BASE", 1), ("M", "inv_BASE", 2)]);
        let tx = conn.transaction().unwrap();
        let idx = next_modification_index_in_tx(&tx, "inv_BASE").unwrap();
        assert_eq!(
            idx, 3,
            "modification walker must union STORNO + MODIFY chains \
             (ADR-0024 §7) — got {idx}"
        );
    }

    /// Defence-in-depth: chain isolation by base. A STORNO + MODIFY
    /// against `inv_OTHER` must not contaminate `inv_BASE`'s chain.
    #[test]
    fn next_modification_index_ignores_unrelated_base() {
        let mut conn = fixture_ledger_with_mixed_chain(&[
            ("S", "inv_OTHER", 1),
            ("M", "inv_OTHER", 2),
            ("M", "inv_OTHER", 3),
        ]);
        let tx = conn.transaction().unwrap();
        let idx = next_modification_index_in_tx(&tx, "inv_BASE").unwrap();
        assert_eq!(idx, 1);
    }

    /// S381/F4 — the modification walker also applies the SAVED-filter
    /// (it calls the same `saved_chain_member_ids_in_tx`). A prior MODIFY
    /// whose NAV ack is ABORTED must not inflate the next index.
    #[test]
    fn next_modification_index_ignores_aborted_prior_modify() {
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
                "inv_modif_aborted",
                100,
                "rsv_modif_aborted",
                idem,
                "inv_BASE",
                42,
                1,
                "2026-05-21",
            );
            audit_ledger::append_in_tx(
                &tx,
                &meta,
                EventKind::InvoiceModificationIssued,
                payload.to_bytes(),
                actor.clone(),
                Some(idem.to_canonical_string()),
            )
            .unwrap();
            let ack = audit_payloads::InvoiceAckStatusPayload::new(
                "inv_modif_aborted",
                "txn_aborted",
                "ABORTED",
                Vec::new(),
            );
            audit_ledger::append_in_tx(
                &tx,
                &meta,
                EventKind::InvoiceAckStatus,
                ack.to_bytes(),
                actor,
                None,
            )
            .unwrap();
            tx.commit().unwrap();
        }
        let tx = conn.transaction().unwrap();
        let idx = next_modification_index_in_tx(&tx, "inv_BASE").unwrap();
        assert_eq!(
            idx, 1,
            "an ABORTed prior modification must not inflate the next modificationIndex (S381/F4)"
        );
    }

    /// Modification of a Finalized base — accepted.
    #[test]
    fn check_base_is_modifiable_accepts_saved_ack() {
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
            .append(EventKind::InvoiceAckStatus, ack.to_bytes(), actor, None)
            .unwrap();

        check_base_is_modifiable(&ledger, "inv_A").expect("SAVED base must be modifiable");
    }

    /// Modification of an Amended base (prior MODIFY exists) —
    /// accepted per ADR-0024 §6 default-permit posture for
    /// modify-after-modify.
    #[test]
    fn check_base_is_modifiable_accepts_already_amended_base() {
        let tenant = TenantId::new("t1".to_string()).unwrap();
        let bh = BinaryHash::from_bytes([0u8; 32]);
        let mut ledger = Ledger::open_in_memory(tenant, bh).unwrap();
        let actor = Actor::from_local_cli("sess".to_string(), "test-user");
        let idem_submit = IdempotencyKey::new();
        let resp = audit_payloads::InvoiceSubmissionResponsePayload::new(
            "inv_A",
            idem_submit,
            "TXID-A",
            b"<response/>".to_vec(),
        );
        ledger
            .append(
                EventKind::InvoiceSubmissionResponse,
                resp.to_bytes(),
                actor.clone(),
                Some(idem_submit.to_canonical_string()),
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
        // Prior MODIFY at index 1 against inv_A.
        let idem_modify = IdempotencyKey::new();
        let modif = audit_payloads::InvoiceModificationIssuedPayload::new(
            "inv_M1",
            7,
            "rsv_M1",
            idem_modify,
            "inv_A",
            1,
            1,
            "2026-05-20",
        );
        ledger
            .append(
                EventKind::InvoiceModificationIssued,
                modif.to_bytes(),
                actor,
                Some(idem_modify.to_canonical_string()),
            )
            .unwrap();

        check_base_is_modifiable(&ledger, "inv_A")
            .expect("Amended base (prior MODIFY) must remain modifiable");
    }

    /// **ADR-0024 §6 hard rejection: modify-of-a-Storno-base.** The
    /// inline citation in `check_base_is_modifiable`'s storno-branch
    /// + its error message is what the §6 named "load-bearing review
    /// surface". This test fires the branch with a real fixture.
    #[test]
    fn check_base_is_modifiable_rejects_stornoed_base() {
        let tenant = TenantId::new("t1".to_string()).unwrap();
        let bh = BinaryHash::from_bytes([0u8; 32]);
        let mut ledger = Ledger::open_in_memory(tenant, bh).unwrap();
        let actor = Actor::from_local_cli("sess".to_string(), "test-user");
        let idem_submit = IdempotencyKey::new();
        let resp = audit_payloads::InvoiceSubmissionResponsePayload::new(
            "inv_A",
            idem_submit,
            "TXID-A",
            b"<response/>".to_vec(),
        );
        ledger
            .append(
                EventKind::InvoiceSubmissionResponse,
                resp.to_bytes(),
                actor.clone(),
                Some(idem_submit.to_canonical_string()),
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
        // STORNO against inv_A.
        let idem_storno = IdempotencyKey::new();
        let storno = audit_payloads::InvoiceStornoIssuedPayload::new(
            "inv_S1",
            7,
            "rsv_S1",
            idem_storno,
            "inv_A",
            1,
            1,
        );
        ledger
            .append(
                EventKind::InvoiceStornoIssued,
                storno.to_bytes(),
                actor,
                Some(idem_storno.to_canonical_string()),
            )
            .unwrap();

        let err = check_base_is_modifiable(&ledger, "inv_A").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("legally cancelled by a storno"),
            "error message must name the storno cancellation; got: {msg}"
        );
    }

    #[test]
    fn check_base_is_modifiable_rejects_never_submitted() {
        let tenant = TenantId::new("t1".to_string()).unwrap();
        let bh = BinaryHash::from_bytes([0u8; 32]);
        let ledger = Ledger::open_in_memory(tenant, bh).unwrap();
        let err = check_base_is_modifiable(&ledger, "inv_A").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no NAV submission response"),
            "error must name the missing submission: got {msg}"
        );
    }

    #[test]
    fn check_base_is_modifiable_rejects_aborted() {
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
        let err = check_base_is_modifiable(&ledger, "inv_A").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("REJECTED"), "got {msg}");
    }

    #[test]
    fn check_base_is_modifiable_rejects_abandoned() {
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
        let aban = audit_payloads::InvoiceMarkedAbandonedPayload::new(
            "inv_A",
            idem,
            Some("TXID-A".to_string()),
            Some("PROCESSING".to_string()),
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
        let err = check_base_is_modifiable(&ledger, "inv_A").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("ABANDONED"), "got {msg}");
    }

    /// Cross-invoice contamination — SAVED ack against `inv_B` must
    /// NOT mark `inv_A` as modifiable.
    #[test]
    fn check_base_is_modifiable_does_not_cross_invoice_ids() {
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
        let err = check_base_is_modifiable(&ledger, "inv_A").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no NAV submission response"),
            "inv_A should be NeverSubmitted regardless of inv_B's state: got {msg}"
        );
    }

    /// S174 — pin that the operator-visible `ModificationIssuedSummary
    /// .invoice_number` matches the same `NumberingTemplate::render_for_build`
    /// shape the NAV-facing `<invoiceNumber>` carries. Pre-S174 the
    /// summary built its own `format!("{}/{:05}", series, seq)` which
    /// silently omitted the dev-build TEST- prefix and any operator-
    /// configured template — so the SPA display + CLI line + PDF
    /// filename diverged from what NAV got.
    ///
    /// The pin is a string-equality assertion at the template layer:
    /// the production code reuses the SAME `modification_invoice_number`
    /// local that flows to `render_modification_data_with_number`, so
    /// if the wire string and the summary string agree at the template
    /// render-site, they agree end-to-end. Mirrors the S173 pin in
    /// `request_technical_annulment.rs::base_invoice_number_carries_test_prefix_in_dev_build`.
    #[test]
    fn modification_invoice_number_matches_render_for_build_under_test_build() {
        use crate::build_profile::INVOICE_NUMBER_TEST_PREFIX;
        use crate::numbering::default_template;

        let template = default_template();
        let rendered = template.render_for_build(2026, 42);
        let expected = format!("{INVOICE_NUMBER_TEST_PREFIX}INV-default/00042");
        assert_eq!(
            rendered, expected,
            "modification summary's invoice_number is reused from this exact \
             render — divergence here means the SPA/CLI/PDF display would diverge \
             from the NAV-facing <invoiceNumber>"
        );
    }
}
