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

use aberp_audit_ledger::{self as audit_ledger, Actor, EventKind, Ledger, LedgerMeta, TenantId};
use aberp_billing::IdempotencyKey;
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
    let seller_toml_path = crate::setup_seller_info::seller_toml_path_for_tenant(&args.tenant)
        .context("resolve seller.toml path for numbering template")?;
    let template = crate::numbering::read_numbering_template(&seller_toml_path)
        .context("read [seller.numbering] template from seller.toml")?;
    let precondition = {
        let ledger = Ledger::open(&args.db, tenant.clone(), binary_hash_bytes)
            .context("open audit ledger for annulment precondition check")?;
        check_base_is_annullable(&ledger, &args.references, &template)?
    };
    tracing::info!(
        prior_transaction_id = %precondition.prior_transaction_id,
        "annulment precondition passed"
    );

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
        base_invoice_number: precondition.base_invoice_number.clone(),
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
    /// (which carries `seq` + the wall-clock year) rendered through
    /// the operator-configured [`crate::numbering::NumberingTemplate`].
    /// The base invoice billing row is NOT loaded here — the annulment
    /// is not an invoice operation and the billing row is not in the
    /// transactional path.
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
    // S173 — year captured from the same `InvoiceSequenceReserved`
    // entry that minted the base's sequence number. Used to render
    // the base_invoice_number against a year-bearing template
    // (`Segment::Year{Two|Four}`). For the default template (which
    // has no Year segment) this year is ignored by the renderer, so
    // pre-S173 callers keep producing the same string.
    let mut latest_sequence_year: Option<i32> = None;
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
                    latest_sequence_year = Some(entry.time_wall.year());
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
    // `latest_sequence_year` is `Some(_)` whenever
    // `latest_sequence_number` is — they are set on the same line
    // of the walk above. The `expect` documents that invariant.
    let base_sequence_year = latest_sequence_year.expect(
        "latest_sequence_year is set on the same walk line as latest_sequence_number; \
         reaching here with Some(seq) but None(year) is a programmer error",
    );

    let base_invoice_number = template.render_for_build(base_sequence_year, base_sequence_number);
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
        let pre = check_base_is_annullable(&ledger, "inv_A", &template)
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

    /// S173 — when the operator-configured template carries a
    /// `Segment::Year` segment, the rendered base_invoice_number must
    /// use the year captured on the matching `InvoiceSequenceReserved`
    /// audit entry (the `Entry.time_wall.year()`), NOT the year of
    /// the current annulment run. This is what makes a cross-year
    /// annulment cite the base by its original year — the same
    /// posture `issue_storno` / `issue_modification` already use for
    /// the base reference (`base_issue_year`).
    #[test]
    fn check_base_is_annullable_renders_year_from_sequence_reserved_entry() {
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
        let pre = check_base_is_annullable(&ledger, "inv_A", &template)
            .expect("submitted base must be annullable");
        // The in-memory ledger's `time_wall` for the
        // `InvoiceSequenceReserved` entry was stamped at append() time
        // (i.e. the test-run wall clock). We assert the rendered shape
        // pattern + the captured-year-end-to-end discipline rather than
        // a hardcoded year string — the year segment must equal
        // `time::OffsetDateTime::now_utc().year()` ± 1 day (in case
        // the test crosses midnight UTC, defence-in-depth).
        let now_year = time::OffsetDateTime::now_utc().year();
        let valid_years = [now_year - 1, now_year, now_year + 1];
        let prefix = crate::build_profile::INVOICE_NUMBER_TEST_PREFIX;
        let matched = valid_years
            .iter()
            .any(|y| pre.base_invoice_number == format!("{prefix}ABERP-{y:04}/00007"));
        assert!(
            matched,
            "base_invoice_number must render with the year captured on the \
             InvoiceSequenceReserved audit entry; got {}",
            pre.base_invoice_number
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
        let err = check_base_is_annullable(&ledger, "inv_A", &template).unwrap_err();
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
        let err = check_base_is_annullable(&ledger, "inv_A", &template).unwrap_err();
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
        check_base_is_annullable(&ledger, "inv_A", &template)
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
        let err = check_base_is_annullable(&ledger, "inv_A", &template).unwrap_err();
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
}
