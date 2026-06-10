//! Clap CLI structs for the `aberp` binary.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(name = "aberp", version, about = "ABERP — modular ERP backend")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Issue an invoice: read a JSON spec, allocate a sequence number,
    /// emit NAV v3.0 InvoiceData XML on disk, and write audit-ledger
    /// entries for the issuance.
    ///
    /// Commit #1 success criterion (see docs/commit-1-success-criterion.md):
    /// the XML structurally matches NAV InvoiceData and the audit chain
    /// verifies cleanly after the run.
    IssueInvoice(IssueInvoiceArgs),

    /// Submit a previously-issued invoice to NAV via `tokenExchange` +
    /// `manageInvoice` (PR-7-B-3). The invoice XML on disk (produced by
    /// `issue-invoice --out ...`) is the body that goes on the wire,
    /// base64-encoded inside the SOAP envelope.
    ///
    /// On a successful `manageInvoice` response, the NAV transaction id
    /// is recorded in the audit ledger; the invoice's typestate
    /// advances from `Ready` to `Submitted` in code. The terminal
    /// `SAVED` / `ABORTED` outcome is the responsibility of PR-7-C's
    /// `queryTransactionStatus` poll loop.
    SubmitInvoice(SubmitInvoiceArgs),

    /// Populate the four NAV credential artifacts in the OS keychain
    /// for a tenant. Operator-tooling helper for PR-7-B-2/3 (needed by
    /// the env-gated live tests; surfaced as a real subcommand because
    /// the integration is operator-visible regardless).
    ///
    /// **The prompts read from stdin in clear text.** Use a stdin
    /// redirect from a file with restrictive permissions, or run on
    /// a workstation where shell history is not synced.
    SetupNavCredentials(SetupNavCredentialsArgs),

    /// Poll NAV's `queryTransactionStatus` for a previously-submitted
    /// invoice and advance the typestate to its terminal state per
    /// ADR-0009 §2 (PR-7-C-2).
    ///
    /// The `transactionId` is looked up from the most-recent
    /// `InvoiceSubmissionResponse` audit-ledger entry for the given
    /// `--invoice-id` — operators do NOT pass it explicitly, both
    /// because it is opaque and because the audit-ledger lookup is
    /// the load-bearing source of truth per the PR-7-B-3 design
    /// assumption A5/A6 ("the audit ledger carries the
    /// submission_state fact; no billing column").
    ///
    /// The bounded poll loop runs up to 5 attempts with exponential
    /// backoff (1s, 2s, 4s, 8s, 16s — total wait cap 31s) per
    /// ADR-0009 §5. On `SAVED` the invoice advances to
    /// `FinalizedInvoice`; on `ABORTED` to `RejectedInvoice`; on
    /// bounded retries exhausted (still RECEIVED/PROCESSING after the
    /// last poll, or repeated retryable NAV errors) to
    /// `SubmissionStuckInvoice` with a loud operator alert via
    /// tracing.
    PollAck(PollAckArgs),

    /// Re-submit an invoice that is in the `SubmissionStuck` posture
    /// per ADR-0009 §5 (PR-8-1). The retry re-runs `tokenExchange` +
    /// `manageInvoice` via the same pipeline as `submit-invoice`, and
    /// writes one extra `InvoiceRetryRequested` audit entry that
    /// records the operator's decision distinctly from the per-
    /// attempt NAV evidence.
    ///
    /// Precondition: the audit ledger must show this invoice in the
    /// `Stuck` state — there must be an `InvoiceSubmissionResponse`
    /// for it, no `InvoiceMarkedAbandoned` for it, and the most-
    /// recent `InvoiceAckStatus` for it (if any) must be non-terminal
    /// (`RECEIVED` / `PROCESSING`). A SAVED, ABORTED, or already-
    /// abandoned invoice loud-fails before any NAV call.
    ///
    /// On success the invoice is left at the `Submitted` typestate
    /// with a fresh NAV `transactionId`; the operator runs
    /// `aberp poll-ack` next to drive the terminal state.
    RetrySubmission(RetrySubmissionArgs),

    /// Mark a `SubmissionStuck` invoice as abandoned per ADR-0009 §5
    /// (PR-8-2). Records the operator's decision to stop retrying;
    /// **terminal** in the audit ledger — no further `aberp`
    /// subcommand will operate on this invoice afterward.
    ///
    /// `mark-abandoned` does NOT call NAV. Per ADR-0009 §6, this is
    /// distinct from a **technical annulment** (which DOES call
    /// `manageAnnulment` to withdraw a faulty data submission from
    /// NAV's side). Abandonment is a local audit-ledger fact: ABERP
    /// has decided not to keep retrying; the invoice's status at NAV
    /// remains whatever NAV last reported.
    ///
    /// Precondition: same `Stuck` precondition as `retry-submission`.
    MarkAbandoned(MarkAbandonedArgs),

    /// Start the loopback HTTPS+JSON listener that the Tauri/Svelte
    /// UI shell consumes (PR-9-1; ADR-0021 §Part B). Long-running:
    /// binds `127.0.0.1:<port>`, terminates TLS via a self-signed
    /// cert generated on first launch and persisted next to the
    /// keychain material (per ADR-0007 §Transport). Routes are
    /// read-only over the billing DB + audit ledger. Mutations
    /// remain on the CLI subcommands.
    ///
    /// On first launch a session token is also minted into the OS
    /// keychain (service `aberp.nav.<tenant>`, account
    /// `session_token`). Clients present `Authorization: Bearer
    /// <token>`. Future operator-action routes will land
    /// incrementally as the Svelte shell asks for them.
    Serve(ServeArgs),

    /// Issue a storno (cancellation invoice) against a previously-
    /// finalized base invoice per ADR-0009 §6 / ADR-0023 (PR-10).
    ///
    /// A storno is itself an invoice: it burns its own sequence
    /// number from the requested series via the same allocator path
    /// as `issue-invoice`, writes its own `<InvoiceData>` XML on
    /// disk (with the `<invoiceReference>` chain block + negated
    /// amounts), and lands three audit-ledger entries in one
    /// DuckDB transaction — `InvoiceSequenceReserved`,
    /// `InvoiceDraftCreated`, and the chain-link
    /// `InvoiceStornoIssued`. The base invoice's typestate
    /// transition (`Finalized → Storno`) is DERIVED from the
    /// chain-link entry; no separate ledger entry is written
    /// against the base (ADR-0023 §2).
    ///
    /// **`issue-storno` does NOT call NAV** (ADR-0023 §1). After
    /// this command writes the storno XML on disk, the operator's
    /// next step is `aberp submit-invoice --invoice-xml <storno.xml>
    /// --invoice-id <storno-id> --endpoint {test|production}` — the
    /// existing wire path detects the storno shape from the
    /// `<invoiceReference>` element and submits with
    /// `InvoiceOperation::Storno`.
    ///
    /// **Precondition.** `--references` must point at an invoice
    /// whose audit-ledger trace shows a terminal-positive
    /// `InvoiceAckStatus` of `"SAVED"` (i.e. the base is
    /// `Finalized` per ADR-0009 §2). Stornos against an unsubmitted
    /// invoice, a stuck invoice, a NAV-rejected invoice, or an
    /// abandoned invoice are loud-fails before any write
    /// (CLAUDE.md rule 12).
    IssueStorno(IssueStornoArgs),

    /// Issue a modification (MODIFY) invoice that corrects a
    /// previously-finalized base invoice per ADR-0009 §6 / ADR-0024
    /// (PR-11).
    ///
    /// Structural parallel to `issue-storno`: the modification is
    /// itself an invoice that burns its own sequence number, writes
    /// its own `<InvoiceData>` XML on disk (with an
    /// `<invoiceReference>` chain block carrying
    /// `<modificationIssueDate>` PLUS the same fields a storno's
    /// `<invoiceReference>` carries), and lands three audit-ledger
    /// entries in one DuckDB transaction —
    /// `InvoiceSequenceReserved`, `InvoiceDraftCreated`, and the
    /// chain-link `InvoiceModificationIssued`. The base invoice's
    /// derived typestate (`Finalized → Amended`) is observed from
    /// the chain-link entry; no separate ledger entry is written
    /// against the base (ADR-0024 §2).
    ///
    /// **Key contrast with `issue-storno`:** the modification body
    /// is **full-replace** (carries the complete corrected invoice
    /// line content, NOT a delta against the base — ADR-0024 §4).
    /// Line / summary amounts are NOT negated; they are the new
    /// effective values.
    ///
    /// **`issue-modification` does NOT call NAV** (same posture as
    /// `issue-storno`). After this command writes the modification's
    /// XML on disk, the operator's next step is `aberp submit-invoice
    /// --invoice-xml <modification.xml> --invoice-id
    /// <modification-id> --endpoint {test|production}` — the existing
    /// wire path detects the MODIFY shape from the presence of
    /// `<modificationIssueDate>` inside `<invoiceReference>` and
    /// submits with `InvoiceOperation::Modify` (ADR-0024 §3).
    ///
    /// **Precondition** (ADR-0024 §6). `--references` must point at
    /// an invoice in `Finalized` (NAV terminal `SAVED`) OR already
    /// in `Amended` (a prior `InvoiceModificationIssued` chain entry
    /// points at it). Modifications against an unsubmitted, stuck,
    /// rejected, abandoned, OR Storno-cancelled base are loud-fails
    /// before any ledger write (CLAUDE.md rule 12).
    IssueModification(IssueModificationArgs),

    /// Submit a previously-requested technical annulment to NAV via
    /// `tokenExchange` + `manageAnnulment` (ADR-0009 §6, ADR-0026;
    /// PR-13). The annulment XML on disk (produced by
    /// `request-technical-annulment --out ...`) is the body that
    /// goes on the wire, base64-encoded inside the SOAP envelope.
    ///
    /// **Different NAV endpoint** from `submit-invoice`. The
    /// `manageAnnulment` endpoint and the `<InvoiceAnnulment>`
    /// body shape are distinct from `manageInvoice` /
    /// `<InvoiceData>` per ADR-0009 §6 / ADR-0025 §1; that's why
    /// this is a separate subcommand rather than an extension of
    /// `submit-invoice` (which would have required a five-way
    /// detector on the body root element). See ADR-0026 §1.
    ///
    /// On a successful `manageAnnulment` response, the NAV-assigned
    /// annulment transaction id is recorded in the audit ledger
    /// (the future `query-annulment-status` poll will key on it).
    /// **The base invoice's typestate does NOT advance** per
    /// ADR-0025 §2: annulment is data-submission withdrawal, not
    /// legal cancellation. NAV-side fulfillment requires the
    /// receiver to confirm the annulment in the NAV web UI per
    /// ADR-0009 §6; ABERP observes that asynchronously via the
    /// future polling PR.
    ///
    /// **Precondition** (ADR-0026 §6). `--invoice-id` must point at
    /// an invoice that has at least one
    /// `InvoiceTechnicalAnnulmentRequested` audit entry (i.e., the
    /// operator's annulment-request decision was actually recorded
    /// — run `aberp request-technical-annulment` first if not).
    /// A successful prior `InvoiceAnnulmentSubmissionResponse`
    /// against the same annulment-request idempotency key
    /// loud-rejects the submission (default-reject of double wire
    /// submission per ADR-0026 §"Surfaced conflict 3"); a failed
    /// prior wire attempt without a successful response permits
    /// retry.
    SubmitAnnulment(SubmitAnnulmentArgs),

    /// Poll NAV's `queryTransactionStatus` for a previously-
    /// submitted technical annulment and record the wire-side
    /// ack status (ADR-0009 §6, ADR-0027; PR-14).
    ///
    /// The annulment-side `transactionId` is looked up from the
    /// most-recent `InvoiceAnnulmentSubmissionResponse` audit
    /// entry for `--invoice-id` (ADR-0027 §4). Operators do NOT
    /// pass it explicitly — same posture as `poll-ack`.
    ///
    /// **Reuses `queryTransactionStatus`** per ADR-0027 §3 +
    /// §"Surfaced conflict 1": NAV v3.0 documents one poll
    /// endpoint that takes any `transactionId`. PR-14 does NOT
    /// add a new nav-transport operation; the discriminator-level
    /// fork lives at the audit-ledger
    /// `InvoiceAnnulmentAckStatus` variant per ADR-0027 §2.
    ///
    /// The bounded poll loop runs up to 5 attempts with
    /// exponential backoff (1s, 2s, 4s, 8s, 16s — same cap as
    /// `poll-ack` per ADR-0009 §5).
    ///
    /// **What this command does NOT do:**
    ///
    ///   - It does NOT call `manageAnnulment`. PR-13's
    ///     `submit-annulment` does that; this command only polls
    ///     the result.
    ///   - It does NOT poll for receiver confirmation. NAV's
    ///     `SAVED` for an annulment submission means "NAV
    ///     accepted the annulment for processing," NOT "the
    ///     receiver has confirmed in the NAV web UI." The
    ///     receiver-confirmation observation is a separate
    ///     future PR per ADR-0027 §"Surfaced conflict 3"; the
    ///     operator-visible message on terminal SAVED names the
    ///     gap loud (CLAUDE.md rule 12).
    ///   - It does NOT mutate any billing row. Annulment is
    ///     not an invoice operation; the base invoice's
    ///     typestate is unchanged per ADR-0025 §2 + ADR-0026.
    ///
    /// **Precondition** (ADR-0027 §6). `--invoice-id` must point
    /// at an invoice that has at least one
    /// `InvoiceAnnulmentSubmissionResponse` audit entry with a
    /// non-empty `transaction_id` — i.e., `submit-annulment` was
    /// run successfully against this invoice. Loud-fail with a
    /// message steering the operator to run `submit-annulment`
    /// first otherwise.
    PollAnnulmentAck(PollAnnulmentAckArgs),

    /// Observe NAV-side receiver-confirmation of a previously-
    /// submitted technical annulment (ADR-0009 §6, ADR-0028;
    /// PR-15). Closes the final ADR-0009 §6 observation gap at
    /// the audit-evidence level.
    ///
    /// **One-shot, not bounded-poll.** Receiver-confirmation is
    /// human-paced — the receiver logs into the NAV web UI on
    /// their own schedule. The operator runs this command once
    /// to record an observation; if the receiver has not yet
    /// confirmed, the operator re-runs the command later at
    /// their cadence (ADR-0028 §4 + §"Surfaced conflict 2").
    ///
    /// **Calls `queryInvoiceData` against the BASE invoice's
    /// NAV-facing invoice number.** The NAV-facing invoice
    /// number is built from the base invoice's series code +
    /// sequence number (per ADR-0028 §1's "Does NOT take
    /// --nav-invoice-number" — operators do not pass
    /// secondary keys that ABERP can derive itself, avoiding
    /// the typo class CLAUDE.md rule 12 names).
    ///
    /// **What this command does NOT do:**
    ///
    ///   - It does NOT call `manageAnnulment`. PR-13's
    ///     `submit-annulment` does that.
    ///   - It does NOT call `queryTransactionStatus`. PR-14's
    ///     `poll-annulment-ack` does that.
    ///   - It does NOT parse a receiver-confirmation status
    ///     field. Per ADR-0028 §"Surfaced conflict 3" the
    ///     verbatim-bytes-only posture applies until NAV-
    ///     testbed verification surfaces the actual response
    ///     shape; the operator inspects the response_xml in
    ///     the audit ledger OR consults the NAV web UI
    ///     directly to determine receiver-confirmation state.
    ///   - It does NOT loop. One query per invocation.
    ///   - It does NOT mutate any billing row.
    ///
    /// **Precondition** (ADR-0028 §6). `--invoice-id` must
    /// point at an invoice that has at least one
    /// `InvoiceAnnulmentSubmissionResponse` audit entry with
    /// a non-empty `transaction_id` — i.e., `submit-annulment`
    /// was run successfully against this invoice. Loud-fail
    /// with a message steering the operator to run
    /// `submit-annulment` first otherwise.
    ObserveReceiverConfirmation(ObserveReceiverConfirmationArgs),

    /// Export a per-invoice audit-evidence bundle as a single
    /// `.tar.zst` archive that a NAV inspector can audit
    /// without trusting ABERP at runtime (ADR-0008 §"Export",
    /// ADR-0009 §8, ADR-0029; PR-16). The bundle contains the
    /// full audit-ledger trail for one invoice (every entry
    /// whose primary or chain-link invoice-id field matches),
    /// every NAV-side request/response XML extracted as a
    /// separate file inside the archive, and a top-level
    /// manifest with the bundle's metadata + chain-verify
    /// result.
    ///
    /// **Read-only.** No NAV calls. No audit-ledger writes.
    /// No billing mutations. The keychain is not consulted.
    /// Per ADR-0008 §"What goes in the ledger" + §"Access":
    /// read-only queries do not produce audit entries; the
    /// operator-visible event lands in `tracing` output
    /// (RUST_LOG-routed), not in the audit ledger.
    ///
    /// **Chain verification gates the write.** Per ADR-0029
    /// §6: `Ledger::verify_chain()` runs over the FULL
    /// tenant chain BEFORE any archive bytes are produced.
    /// On verify failure, the orchestration aborts loud
    /// (CLAUDE.md rule 12) and produces no output file.
    /// A tampered chain that shipped as a bundle would
    /// mislead the inspector into trusting a forged history.
    ///
    /// **Unsigned + no-mirror at PR-16 time.** Per ADR-0029
    /// §4 + §5: the bundle ships unsigned (F5 attestation-
    /// signing key deferred; trigger has not fired) and
    /// without mirror-file second-source assertion (F10
    /// deferred to PR-17). Both gaps are named LOUD in the
    /// manifest's `signed: false` / `signature_status:
    /// "deferred-per-f5"` / `mirror_file_present: false` /
    /// `mirror_file_status: "deferred-per-f10"` fields. A
    /// future PR additively extends the manifest when the
    /// gates lift; PR-16 bundles remain valid in their
    /// stored form.
    ///
    /// **Bundle membership (ADR-0029 §2).** Every audit
    /// entry whose payload has any invoice-id-shaped field
    /// equal to `--invoice-id` is included: `invoice_id`,
    /// `storno_invoice_id`, `modification_invoice_id`, OR
    /// `base_invoice_id`. The BASE invoice's bundle thus
    /// picks up the storno + modification chain-link entries
    /// (which reference the base via `base_invoice_id`) as
    /// well as its own primary-id entries; the chain
    /// invoice's bundle picks up its own primary-id entries
    /// + the same chain-link entry (which references it via
    /// `storno_invoice_id` / `modification_invoice_id`).
    /// Entries are written to `chain.jsonl` in `seq` order.
    ///
    /// **Refuse-overwrite by default.** Per ADR-0029 §1:
    /// the orchestrator refuses to overwrite an existing
    /// `--out` file unless `--allow-overwrite` is passed.
    /// Preserves operator-visible artifacts from accidental
    /// clobbering.
    ExportInvoiceBundle(ExportInvoiceBundleArgs),

    /// Reconstruct the missing local `InvoiceSubmissionResponse`
    /// audit entry for a state-2 Pending invoice that NAV already
    /// has, per ADR-0009 §5 / ADR-0034 (PR-21). Closes F48 (the
    /// second half of ADR-0009 §5's Layer-2 idempotency intent:
    /// "fetch the chain via `queryInvoiceData` and reconstruct
    /// local state").
    ///
    /// **Precondition** (ADR-0034 §5). `--invoice-id` must point
    /// at an invoice classified as `Stuck(StuckStage::Pending)` by
    /// `audit_query::stuck_precondition` AND whose most-recent
    /// `InvoiceCheckPerformed` audit entry has `outcome="exists"`.
    /// The Layer-2 Exists evidence is produced by PR-20 / ADR-0033
    /// §1's `retry-submission` Phase 0 step — if no prior
    /// `InvoiceCheckPerformed` exists for this invoice, run
    /// `aberp retry-submission` first to disambiguate the NAV-side
    /// state. A state-3 (AwaitingAck), terminal, or abandoned
    /// invoice loud-fails before any NAV call.
    ///
    /// **Calls `queryInvoiceData` against the invoice's NAV-facing
    /// invoice number** (constructed from the billing row's series
    /// code + sequence number per ADR-0009 §3 — same shape every
    /// `<invoiceNumber>` element ABERP emits). One-shot per
    /// ADR-0034 §2 / ADR-0028 §4 (no loop). Parses
    /// `<auditData>/<transactionId>` from the verbatim response
    /// bytes per ADR-0034 §3 to recover the NAV-assigned
    /// transactionId of the original submission.
    ///
    /// **Writes ONE recovered `InvoiceSubmissionResponse` audit
    /// entry** carrying the recovered transactionId + the verbatim
    /// `<QueryInvoiceDataResponse>` bytes as provenance evidence.
    /// Reuses the existing payload shape per ADR-0034 §4 — no
    /// new `EventKind` variant, no schema change, F12 four-edit
    /// ritual does NOT fire. The preceding
    /// `InvoiceCheckPerformed(outcome=exists)` entry IS the
    /// recovered-from-NAV provenance marker.
    ///
    /// **Does NOT fabricate `InvoiceAckStatus`** per ADR-0034
    /// §"Why reconstruct only `InvoiceSubmissionResponse`, not
    /// `InvoiceAckStatus`" + CLAUDE.md rule 12. The operator's
    /// next move is `aberp poll-ack` against the recovered
    /// transactionId; `queryTransactionStatus` produces the
    /// authoritative ack status via the existing PR-7-C-2 path.
    ///
    /// **`audit_query::stuck_precondition` UNCHANGED.** The
    /// PR-20 / ADR-0033 §6 pin tests stay valid; Layer-2 entries
    /// remain informational-only at the shared classifier per
    /// ADR-0034 §"Surfaced conflict 3 Reading B".
    ///
    /// **What this command does NOT do:**
    ///
    ///   - It does NOT call `manageInvoice`. There is no re-POST
    ///     on the recovery path — NAV already has the invoice.
    ///   - It does NOT call `queryInvoiceCheck`. The Layer-2
    ///     check is PR-20 / `retry-submission`'s responsibility;
    ///     the precondition consumes the existing
    ///     `InvoiceCheckPerformed(outcome=exists)` entry.
    ///   - It does NOT call `queryTransactionStatus`. PR-7-C-2's
    ///     `poll-ack` is the operator's next step.
    ///   - It does NOT loop. One queryInvoiceData call per
    ///     invocation.
    ///   - It does NOT take a `--reason` flag. The recovery is
    ///     mechanical reconstruction; the audit-evidence chain
    ///     itself is the justification per ADR-0034 §1.
    ///   - It does NOT mutate any billing row.
    RecoverFromNav(RecoverFromNavArgs),

    /// Drain the offline submission queue per ADR-0009 §7 /
    /// ADR-0031 (PR-18). Walks the audit ledger, classifies every
    /// invoice with `InvoiceDraftCreated` but no
    /// `InvoiceSubmissionResponse` / `InvoiceMarkedAbandoned` as
    /// `pending`, and submits them to NAV in FIFO order by issue
    /// date.
    ///
    /// **Per-invoice pipeline:** read the NAV InvoiceData XML
    /// from the recorded path (per ADR-0031 §2 — populated by
    /// `issue-*` since PR-18) OR from a per-invocation
    /// `--xml-path-override <invoice-id>=<path>` mapping for
    /// pre-PR-18 entries; validate via the NAV v3.0 invariant
    /// check (ADR-0022, same gate every `submit-*` runs); call
    /// `tokenExchange` + `manageInvoice` per ADR-0009 §4; write
    /// `InvoiceSubmissionAttempt` + `InvoiceSubmissionResponse`
    /// audit entries under one DuckDB transaction; verify the
    /// audit chain; sync the audit-ledger mirror file per
    /// ADR-0030 §2; print the per-invoice OK line.
    ///
    /// **Offline detection** (ADR-0031 §4). Drain stops on the
    /// first NAV TRANSPORT-layer error (HTTP / TLS / DNS); the
    /// remaining pending invoices stay pending for the next
    /// drain run. NAV-side APPLICATION errors (non-success HTTP
    /// status, response-parse failures, credential errors) DO
    /// NOT stop the drain — drain surfaces the per-invoice
    /// failure LOUD and continues to the next invoice.
    ///
    /// **Alert thresholds** (ADR-0031 §6). Before the loop body,
    /// drain prints a WARN if `pending.len() >= 5` OR the
    /// oldest pending invoice's issue date is more than 30
    /// minutes ago. Non-control-flow; the hard cap of 50 lives
    /// in `issue-invoice` (ADR-0031 §5).
    ///
    /// **What this command does NOT do:**
    ///
    ///   - It does NOT process `SubmissionStuck` invoices.
    ///     `retry-submission` is the operator-confirmed path
    ///     for post-submission stuck invoices.
    ///   - It does NOT enforce the ADR-0009 §7 24h soft /
    ///     72h hard submission deadlines. Deferred per ADR-0031
    ///     §7 (F41 named-trigger).
    ///   - It does NOT write a per-attempt audit entry on a
    ///     failed NAV call. Deferred per ADR-0031 §7 (F40 named-
    ///     trigger).
    ///   - It does NOT call NAV for credential setup. Per the
    ///     `submit-*` family's posture, NAV credentials are
    ///     loaded once at the start.
    DrainSubmissionQueue(DrainSubmissionQueueArgs),

    /// Drain state-2 Pending invoices through the automatic retry
    /// loop per ADR-0032 §4 (PR-42, F45 closure). Walks the audit
    /// ledger, classifies every invoice that has an
    /// `InvoiceSubmissionAttempt` but no `InvoiceSubmissionResponse`
    /// and no `InvoiceMarkedAbandoned` as state-2 Pending, and drives
    /// each one through the same Layer-2 + TX1 + wire + TX2 pipeline
    /// the operator-confirmed `aberp retry-submission` uses (PR-19 /
    /// ADR-0032 §1, PR-20 / ADR-0033 §1).
    ///
    /// **Per-invoice pipeline:** read NAV InvoiceData XML from the
    /// recorded `nav_xml_path` (loud-fail on pre-PR-18 entries — the
    /// operator drains those via the manual `aberp retry-submission
    /// --invoice-xml <path>` command); validate via the NAV v3.0
    /// invariant check (ADR-0022); run `queryInvoiceCheck` Phase 0
    /// (PR-20 / ADR-0033 §1) — on Exists skip the re-POST and
    /// continue the loop (operator next-step is `aberp recover-from-
    /// nav`), on Absent proceed to TX1 (RetryRequested + Attempt) +
    /// wire + TX2 (Response or AttemptFailed); verify the audit
    /// chain; sync the mirror; print the per-invoice OK line.
    ///
    /// **Auto-reason text:** the operator's decision is to run this
    /// drain command; the per-invoice `InvoiceRetryRequested` audit
    /// entries carry a fixed reason string naming the F45 closure +
    /// ADR-0032 §4 — distinct from `aberp retry-submission --reason`
    /// (operator-supplied per invoice).
    ///
    /// **Transport-vs-application fork** (ADR-0031 §4 / ADR-0032 §2).
    /// Same posture as `drain-submission-queue`: a transport-class
    /// wire failure at any phase (Layer-2 check or manageInvoice
    /// POST) short-circuits the FIFO loop; application-class failures
    /// surface per-invoice LOUD and the loop continues. Layer-2
    /// Exists is a per-invoice "skip re-POST" decision (not a stop);
    /// Layer-2 Failure is classified into the same fork by substring
    /// scan on the typed-error message.
    ///
    /// **What this command does NOT do:**
    ///
    ///   - It does NOT process state-1 Draft invoices.
    ///     `drain-submission-queue` is the operator surface for those.
    ///   - It does NOT process state-3 AwaitingAck invoices. The
    ///     classifier filters only state-2 Pending; state-3 invoices
    ///     are recoverable via `aberp retry-submission` or
    ///     `aberp poll-ack` depending on intent.
    ///   - It does NOT support `--xml-path-override`. Pre-PR-18
    ///     state-2 invoices loud-fail at the `nav_xml_path: None`
    ///     check; the operator drains those manually via
    ///     `aberp retry-submission --invoice-xml <path>`.
    ///   - It does NOT take a `--reason` flag. The auto-reason
    ///     names the drain run; per-invoice operator-decision
    ///     rationale lives on the manual `aberp retry-submission
    ///     --reason` command.
    ///   - It does NOT poll `queryTransactionStatus`. The operator
    ///     runs `aberp poll-ack` after the drain (or schedules it
    ///     independently).
    DrainPendingRetries(DrainPendingRetriesArgs),

    /// Request a NAV-side technical annulment of a prior data
    /// submission against an invoice (ADR-0009 §6, ADR-0025; PR-12).
    /// A technical annulment **withdraws** the data submission to
    /// NAV — used for true submission-side errors such as a test
    /// invoice accidentally sent to production. It is **distinct
    /// from a storno** (which legally cancels the invoice as a
    /// document) and from `mark-abandoned` (which is a local-only
    /// decision to stop retrying a stuck invoice).
    ///
    /// **Key contrasts with `issue-storno` / `issue-modification`:**
    ///
    ///   - A technical annulment is **not itself an invoice.** No
    ///     sequence number is burned, no allocator slot is consumed,
    ///     no `<invoiceReference>` chain block is emitted. The audit
    ///     footprint is a single `InvoiceTechnicalAnnulmentRequested`
    ///     entry — not the three-entry pair that storno + modify
    ///     write.
    ///   - The base invoice's derived typestate is **not** changed by
    ///     the annulment request alone. NAV-side fulfillment requires
    ///     the receiver to confirm the annulment in the NAV web UI;
    ///     ABERP observes that asynchronously via a future polling PR.
    ///
    /// **`request-technical-annulment` does NOT call NAV.** Same
    /// posture as `issue-storno` / `issue-modification`. After this
    /// command writes the annulment XML on disk + the operator-
    /// decision audit entry, the operator's next step (when that
    /// command lands) is `aberp submit-annulment --annulment-xml
    /// ... --invoice-id ... --endpoint {test|production}` — a NEW
    /// wire command that calls NAV's `manageAnnulment` endpoint
    /// (distinct from `submit-invoice`'s `manageInvoice` endpoint).
    ///
    /// **Precondition** (ADR-0025 §6). `--references` must point at
    /// an invoice that has at least one `InvoiceSubmissionResponse`
    /// audit entry (i.e., a data submission was actually made to NAV
    /// — there is something to annul). Double-annulment (a prior
    /// `InvoiceTechnicalAnnulmentRequested` against the same base)
    /// is loud-rejected by default per the open accountant question
    /// in ADR-0025 §8. Annulment of a `Rejected` / `Stuck` /
    /// `Abandoned` / already-Stornoed / already-Amended base is
    /// **permitted** — annulment is data-submission withdrawal,
    /// orthogonal to legal cancellation.
    RequestTechnicalAnnulment(RequestTechnicalAnnulmentArgs),

    /// Render a finalized invoice to a Billingo-style A4 PDF per
    /// ADR-0037 §1.a + ADR-0021 "Print rendering path" (PR-44ε.1 /
    /// A152). The PDF is the operator-deliverable artifact §169 + §172
    /// name (the §80 + NAV submission contract is the wire body; this
    /// is the human-readable counterpart).
    ///
    /// **Inputs:** the invoice's audit-ledger `InvoiceDraftCreated`
    /// entry (the source of truth for currency + rate-metadata stamp),
    /// the on-disk NAV `<InvoiceData>` body (the source of truth for
    /// parties + line content + amounts), and the per-tenant
    /// `seller.toml` (the source of truth for bank account / IBAN /
    /// SWIFT — fields that don't live on the NAV body but appear on the
    /// printed invoice).
    ///
    /// **On-disk posture (A155).** The NAV body bytes are read
    /// verbatim from the `nav_xml_path` recorded at issuance time per
    /// ADR-0031 §2 + PR-18 — no re-render, no MNB re-fetch, no
    /// billing-row consultation. The printed PDF is byte-deterministic
    /// given a committed audit chain.
    ///
    /// **HUF + EUR.** HUF invoices print the classic Billingo single-
    /// currency layout (no Árfolyam line, no MEGJEGYZÉS rate note).
    /// EUR invoices add the §80(1)(g) HUF-equivalent row + the
    /// Árfolyam line + the MEGJEGYZÉS rate note, populated from the
    /// audit-ledger rate stamp (NOT a fresh MNB fetch). The §1.c per-
    /// VAT-rate HUF amounts are computed via the round-half-even
    /// helper per A137 / C11.
    ///
    /// **Refuses overwrite by default.** Pass `--allow-overwrite` to
    /// permit clobbering an existing `--out` file. Same posture as
    /// `export-invoice-bundle`.
    PrintInvoice(PrintInvoiceArgs),
}

#[derive(Debug, Parser)]
pub struct IssueInvoiceArgs {
    /// Path to the input JSON file (NAV-aligned shape; see
    /// fixtures/invoice_minimal.json for the canonical example).
    #[arg(long)]
    pub r#in: PathBuf,

    /// Path to write the NAV InvoiceData XML.
    #[arg(long)]
    pub out: PathBuf,

    /// Path to the tenant DuckDB file. Created on first run.
    #[arg(long, default_value = "./aberp.duckdb")]
    pub db: PathBuf,

    /// Tenant identifier — used for the audit-ledger genesis hash.
    #[arg(long, default_value = "default")]
    pub tenant: String,

    /// Invoice series code. Auto-created on first run if it does not
    /// already exist (with reset_policy = Never).
    #[arg(long, default_value = "INV-default")]
    pub series: String,

    /// PR-44γ / ADR-0037 §3. Invoice currency. Default `Huf` preserves
    /// pre-PR-44γ behaviour byte-identically (the C10 invariant
    /// prerequisite); `Eur` lights up the MNB-rate-fetch + HUF-equivalent
    /// stamp path. The closed vocab is enforced by clap's `ValueEnum`
    /// derive (per ADR-0037 §4 invariant C8 — invalid values are
    /// rejected at parse time, before any DB write).
    #[arg(long, value_enum, default_value_t = CurrencyArg::Huf)]
    pub currency: CurrencyArg,
}

/// CLI mirror of `aberp_billing::Currency`. The two enums are pinned
/// closed-vocab-to-closed-vocab; a regression that drops a variant on
/// one side surfaces as a missing match arm via the
/// `CurrencyArg::to_billing_currency` conversion. Per ADR-0037 §3 +
/// CLAUDE.md rule 11 (match codebase conventions: the
/// `aberp_billing::Currency` enum is the domain-side canon; the CLI's
/// clap-`ValueEnum` shape is the operator-facing canon).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CurrencyArg {
    /// HUF (Hungarian forint) — the pre-PR-44γ default. No MNB-rate
    /// fetch; no rate-metadata stamp.
    Huf,
    /// EUR (Euro) — the PR-44γ EUR path. Triggers an MNB-rate fetch
    /// (with D-1 walk-back per ADR-0037 §2.b), HUF-equivalent
    /// computation (round-half-even per A137 / C11), and the rate-
    /// metadata stamp on the DuckDB row + audit-ledger entry.
    Eur,
}

impl CurrencyArg {
    /// Convert to the domain-side typed `Currency`. Closed-vocab pair
    /// per ADR-0037 §3; the `match` is exhaustive at the type level so
    /// adding a CLI variant without adding a domain variant (or vice
    /// versa) is a compile error.
    pub fn to_billing_currency(self) -> aberp_billing::Currency {
        match self {
            CurrencyArg::Huf => aberp_billing::Currency::Huf,
            CurrencyArg::Eur => aberp_billing::Currency::Eur,
        }
    }
}

/// Which NAV environment a submission targets. Explicit value rather
/// than a default per ADR-0009 §1 + ADR-0020 §1 — silently submitting
/// to production when the operator meant test is exactly the failure
/// mode CLAUDE.md rule 12 names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum NavEnv {
    /// `api-test.onlineszamla.nav.gov.hu` — no real fiscal effect.
    Test,
    /// `api.onlineszamla.nav.gov.hu` — real submission.
    Production,
}

#[derive(Debug, Parser)]
pub struct SubmitInvoiceArgs {
    /// Path to the `<InvoiceData>` XML written by a prior
    /// `aberp issue-invoice --out ...` run. The bytes on disk are the
    /// body submitted (base64-encoded inside the SOAP envelope).
    #[arg(long = "invoice-xml")]
    pub invoice_xml: PathBuf,

    /// Invoice id (prefixed form, `inv_<ULID>`) of the invoice to
    /// submit. Used to look up the persisted idempotency key from the
    /// billing store so the submit audit entries link to the same key
    /// as the issuance entries (F8 contract).
    #[arg(long = "invoice-id")]
    pub invoice_id: String,

    /// Hungarian tax number of the submitter. Accepted forms:
    /// `12345678`, `12345678-1`, `12345678-1-42`. Only the first 8
    /// digits go to NAV per ADR-0009 §4; the dashed suffix (VAT type
    /// digit + county code) is parsed and discarded here.
    #[arg(long = "tax-number")]
    pub tax_number: String,

    /// Path to the tenant DuckDB file.
    #[arg(long, default_value = "./aberp.duckdb")]
    pub db: PathBuf,

    /// Tenant identifier — drives both the audit-ledger genesis hash
    /// and the keychain service-name lookup
    /// (`aberp.nav.<tenant_id>` per `crate::credentials::keychain`).
    #[arg(long, default_value = "default")]
    pub tenant: String,

    /// Which NAV environment to submit against. No default — explicit
    /// per ADR-0020 §1.
    #[arg(long, value_enum)]
    pub endpoint: NavEnv,
}

#[derive(Debug, Parser)]
pub struct PollAckArgs {
    /// Invoice id (prefixed form, `inv_<ULID>`) of the previously-
    /// submitted invoice to poll. The transactionId is looked up from
    /// the audit ledger — operators do not pass it on the CLI.
    #[arg(long = "invoice-id")]
    pub invoice_id: String,

    /// Hungarian tax number of the submitter. Accepted forms:
    /// `12345678`, `12345678-1`, `12345678-1-42`. Only the first 8
    /// digits go to NAV per ADR-0009 §4. Same parser as
    /// `submit-invoice`; passing the dashed full form produces
    /// `INVALID_SECURITY_USER` from NAV.
    #[arg(long = "tax-number")]
    pub tax_number: String,

    /// Path to the tenant DuckDB file.
    #[arg(long, default_value = "./aberp.duckdb")]
    pub db: PathBuf,

    /// Tenant identifier — drives both the audit-ledger genesis hash
    /// and the keychain service-name lookup
    /// (`aberp.nav.<tenant_id>` per `crate::credentials::keychain`).
    #[arg(long, default_value = "default")]
    pub tenant: String,

    /// Which NAV environment to poll against. No default — explicit
    /// per ADR-0020 §1 (same posture as `submit-invoice`).
    #[arg(long, value_enum)]
    pub endpoint: NavEnv,
}

#[derive(Debug, Parser)]
pub struct RetrySubmissionArgs {
    /// Path to the `<InvoiceData>` XML written by the prior
    /// `aberp issue-invoice --out ...` run. The retry submits the
    /// same bytes — the original invoice content (and its sequence
    /// number / issue date) does not change, only the wire attempt.
    #[arg(long = "invoice-xml")]
    pub invoice_xml: PathBuf,

    /// Invoice id (prefixed form, `inv_<ULID>`) of the stuck invoice
    /// to retry.
    #[arg(long = "invoice-id")]
    pub invoice_id: String,

    /// Hungarian tax number of the submitter. Same accepted forms +
    /// parser as `submit-invoice` / `poll-ack` (`12345678`,
    /// `12345678-1`, `12345678-1-42`).
    #[arg(long = "tax-number")]
    pub tax_number: String,

    /// Path to the tenant DuckDB file.
    #[arg(long, default_value = "./aberp.duckdb")]
    pub db: PathBuf,

    /// Tenant identifier — drives both the audit-ledger genesis hash
    /// and the keychain service-name lookup.
    #[arg(long, default_value = "default")]
    pub tenant: String,

    /// Which NAV environment to retry against. No default — explicit
    /// per ADR-0020 §1 (same posture as `submit-invoice` / `poll-ack`).
    #[arg(long, value_enum)]
    pub endpoint: NavEnv,

    /// Operator-supplied reason for the retry. Required per
    /// ADR-0009 §5 — the audit-evidence bundle (ADR-0009 §8) must
    /// carry a human-readable justification for each operator
    /// unblock decision.
    #[arg(long)]
    pub reason: String,
}

#[derive(Debug, Parser)]
pub struct MarkAbandonedArgs {
    /// Invoice id (prefixed form, `inv_<ULID>`) of the stuck invoice
    /// to mark abandoned.
    #[arg(long = "invoice-id")]
    pub invoice_id: String,

    /// Path to the tenant DuckDB file.
    #[arg(long, default_value = "./aberp.duckdb")]
    pub db: PathBuf,

    /// Tenant identifier — drives the audit-ledger genesis hash.
    /// (NAV credentials are NOT loaded — `mark-abandoned` does not
    /// call NAV, so the keychain is not consulted.)
    #[arg(long, default_value = "default")]
    pub tenant: String,

    /// Operator-supplied reason for the abandonment. Required per
    /// ADR-0009 §5 — a terminal operator decision must carry a
    /// human-readable justification.
    #[arg(long)]
    pub reason: String,

    /// PR-43 / F49 closure — override the Layer-2-aware guard. By
    /// default, `mark-abandoned` consults the most-recent
    /// `InvoiceCheckPerformed` audit entry for this invoice (written
    /// by `retry-submission` or `drain-pending-retries` Phase 0 per
    /// ADR-0033 §1) and loud-fails when the outcome is `"exists"` —
    /// NAV already has the invoice, and a local abandonment would
    /// create a silent divergence between ABERP's terminal-abandoned
    /// state and NAV's accepted-submission state.
    ///
    /// Pass `--force-despite-nav-exists` to override the guard when
    /// the operator has out-of-band knowledge that abandonment is
    /// correct anyway (e.g., NAV's accepted record will be technically
    /// annulled separately, or the divergence is documented in the
    /// reason text). The override is loud in the audit-evidence
    /// bundle: the resulting `InvoiceMarkedAbandoned` entry's
    /// `reason` field is automatically suffixed with a
    /// `[forced-despite-nav-side-exists]` marker so the bundle
    /// reader sees the override flag's effect without consulting
    /// CLI history.
    ///
    /// Default `false`. The explicit opt-in makes the override a
    /// deliberate operator decision per CLAUDE.md rule 12.
    #[arg(long = "force-despite-nav-exists", default_value_t = false)]
    pub force_despite_nav_exists: bool,
}

#[derive(Debug, Parser)]
pub struct ServeArgs {
    /// Path to the tenant DuckDB file (the same one the CLI
    /// subcommands operate on). The serve routes are read-only;
    /// concurrent CLI mutations on the same file are safe because
    /// DuckDB's file-locking discipline funnels them through.
    #[arg(long, default_value = "./aberp.duckdb")]
    pub db: PathBuf,

    /// Tenant identifier — drives both the audit-ledger genesis hash
    /// and the keychain service-name lookup
    /// (`aberp.nav.<tenant>`). The session-token entry lives at the
    /// same service name under account `session_token`.
    #[arg(long, default_value = "default")]
    pub tenant: String,

    /// TCP port to bind on `127.0.0.1`. `0` means the kernel picks
    /// an unused port; the chosen port is printed on stdout for the
    /// Tauri shell to read.
    ///
    /// We default to `0` because the operator workstation may
    /// already have something on a memorable port; a future
    /// PR-9-1.5 can persist the chosen port in the same artifacts
    /// directory as the cert if "remember last port" turns out to
    /// matter to the SPA.
    #[arg(long, default_value_t = 0)]
    pub port: u16,
}

#[derive(Debug, Parser)]
pub struct IssueStornoArgs {
    /// Invoice id (prefixed form, `inv_<ULID>`) of the base invoice
    /// this storno cancels. Must already be in the local `Finalized`
    /// typestate — i.e. the audit ledger carries an
    /// `InvoiceAckStatus` of `"SAVED"` for it (ADR-0023 §1). A
    /// storno against a not-yet-finalized invoice loud-fails before
    /// any ledger write.
    #[arg(long = "references")]
    pub references: String,

    /// Path to the input JSON file describing the storno's own line
    /// content. Same shape as `issue-invoice --in`; the storno
    /// subcommand sets the implicit "this is a storno" flag so the
    /// XML emitter negates line/summary amounts and emits the
    /// `<invoiceReference>` chain block (ADR-0023 §1).
    #[arg(long)]
    pub r#in: PathBuf,

    /// Path to write the storno's NAV InvoiceData XML. Same on-disk
    /// gate as `issue-invoice --out`; the resulting bytes are what
    /// `submit-invoice` later POSTs to NAV.
    #[arg(long)]
    pub out: PathBuf,

    /// Path to the tenant DuckDB file.
    #[arg(long, default_value = "./aberp.duckdb")]
    pub db: PathBuf,

    /// Tenant identifier — used for the audit-ledger genesis hash
    /// and the keychain service-name lookup
    /// (`aberp.nav.<tenant>`).
    #[arg(long, default_value = "default")]
    pub tenant: String,

    /// Series the storno's own sequence number is drawn from. By
    /// default the same series as the base invoice. Override iff
    /// the accountant has set up a dedicated storno series — no
    /// silent series switch happens (ADR-0023 §1).
    #[arg(long, default_value = "INV-default")]
    pub series: String,
}

#[derive(Debug, Parser)]
pub struct IssueModificationArgs {
    /// Invoice id (prefixed form, `inv_<ULID>`) of the base invoice
    /// this modification corrects. Must be in `Finalized` (NAV
    /// terminal `SAVED`) OR already `Amended` (a prior
    /// `InvoiceModificationIssued` entry points at it). A
    /// modification against an unsubmitted, stuck, rejected,
    /// abandoned, or Storno-cancelled base loud-fails before any
    /// ledger write (ADR-0024 §6).
    #[arg(long = "references")]
    pub references: String,

    /// Path to the input JSON file describing the modification's
    /// **full corrected** line content. Same JSON shape as
    /// `issue-invoice --in` / `issue-storno --in`; ABERP's MODIFY
    /// semantics are full-replace, not delta (ADR-0024 §4) — the
    /// modification carries the complete corrected invoice body, not
    /// just the changed lines.
    #[arg(long)]
    pub r#in: PathBuf,

    /// Path to write the modification's NAV InvoiceData XML.
    /// Same on-disk validator gate as `issue-invoice --out` /
    /// `issue-storno --out`; the resulting bytes are what
    /// `submit-invoice` later POSTs to NAV (with operation MODIFY
    /// detected from the body shape per ADR-0024 §3).
    #[arg(long)]
    pub out: PathBuf,

    /// Path to the tenant DuckDB file.
    #[arg(long, default_value = "./aberp.duckdb")]
    pub db: PathBuf,

    /// Tenant identifier — used for the audit-ledger genesis hash
    /// and the keychain service-name lookup
    /// (`aberp.nav.<tenant>`).
    #[arg(long, default_value = "default")]
    pub tenant: String,

    /// Series the modification's own sequence number is drawn from.
    /// By default the same series as the base invoice. Same
    /// override-path caveat as `issue-storno --series` (ADR-0023 §1).
    #[arg(long, default_value = "INV-default")]
    pub series: String,

    /// `<modificationIssueDate>` in canonical `YYYY-MM-DD` form.
    /// NAV-required for MODIFY (and the discriminator that
    /// `submit-invoice`'s detector keys on per ADR-0024 §3). No
    /// default — silently defaulting to "today" would mask an
    /// accountant filing a back-dated correction with explicit dates
    /// (CLAUDE.md rule 4: no hidden defaults on audit-bearing
    /// fields; rule 12: fail loud). Validated against
    /// `time::Date::parse(YYYY-MM-DD)` at the CLI boundary.
    #[arg(long = "modification-date")]
    pub modification_date: String,
}

/// NAV's four technical-annulment codes per ADR-0025 §"Surfaced
/// conflict 2". Exposed as a clap `ValueEnum` so the parse boundary
/// loud-fails on unknown codes (operator typo, accidental new code
/// from a future NAV revision); the audit-payload stores the
/// canonical SCREAMING_SNAKE_CASE wire form via
/// [`AnnulmentCode::to_wire`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AnnulmentCode {
    /// `ERRATIC_DATA` — generic "the data was wrong" classification.
    /// Used when no more specific code fits (e.g., line content
    /// errors, supplier or customer data errors).
    ErraticData,
    /// `ERRATIC_INVOICE_NUMBER` — the invoice number itself was
    /// wrong (collision, off-by-one, wrong series).
    ErraticInvoiceNumber,
    /// `ERRATIC_INVOICE_ISSUE_DATE` — the issue date was wrong.
    ErraticInvoiceIssueDate,
    /// `ERRATIC_ELECTRONIC_HASH_VALUE` — the electronic hash value
    /// was wrong (post-submission discovery of a hash mismatch
    /// against the legally-stored copy).
    ErraticElectronicHashValue,
}

impl AnnulmentCode {
    /// Convert to NAV's canonical wire form. The clap-flavoured
    /// hyphen-lowercase shape (`erratic-data`, etc.) is what the
    /// operator types on the CLI; the wire form
    /// (`ERRATIC_DATA`, etc.) is what NAV expects in
    /// `<annulmentCode>` and what the audit payload stores
    /// canonically per ADR-0025 §3.
    pub fn to_wire(self) -> &'static str {
        match self {
            AnnulmentCode::ErraticData => "ERRATIC_DATA",
            AnnulmentCode::ErraticInvoiceNumber => "ERRATIC_INVOICE_NUMBER",
            AnnulmentCode::ErraticInvoiceIssueDate => "ERRATIC_INVOICE_ISSUE_DATE",
            AnnulmentCode::ErraticElectronicHashValue => "ERRATIC_ELECTRONIC_HASH_VALUE",
        }
    }
}

#[derive(Debug, Parser)]
pub struct RequestTechnicalAnnulmentArgs {
    /// Invoice id (prefixed form, `inv_<ULID>`) of the base invoice
    /// whose prior NAV data submission is being withdrawn. Must
    /// have at least one `InvoiceSubmissionResponse` audit entry
    /// (ADR-0025 §6) — annulment of a never-submitted invoice is
    /// malformed; use `mark-abandoned` for the local-only "stop
    /// retrying" decision instead. Double-annulment (a prior
    /// `InvoiceTechnicalAnnulmentRequested` against the same base)
    /// is loud-rejected by default.
    #[arg(long = "references")]
    pub references: String,

    /// NAV annulment code. One of `erratic-data` /
    /// `erratic-invoice-number` / `erratic-invoice-issue-date` /
    /// `erratic-electronic-hash-value` — clap-ValueEnum-validated at
    /// parse time so an unknown code loud-fails before any ledger
    /// write. Stored canonically in the audit payload as
    /// `ERRATIC_DATA` / `ERRATIC_INVOICE_NUMBER` /
    /// `ERRATIC_INVOICE_ISSUE_DATE` / `ERRATIC_ELECTRONIC_HASH_VALUE`
    /// per ADR-0025 §3.
    #[arg(long, value_enum)]
    pub code: AnnulmentCode,

    /// Free-form operator-supplied reason text. Required at the CLI
    /// boundary so the audit-evidence bundle (ADR-0009 §8) always
    /// carries a human-readable justification for the annulment
    /// decision. Same posture as `retry-submission --reason` /
    /// `mark-abandoned --reason`.
    #[arg(long)]
    pub reason: String,

    /// Path to write the annulment's `<InvoiceAnnulment>` XML. The
    /// resulting bytes are what the future `submit-annulment`
    /// command will POST to NAV's `manageAnnulment` endpoint.
    #[arg(long)]
    pub out: std::path::PathBuf,

    /// Path to the tenant DuckDB file.
    #[arg(long, default_value = "./aberp.duckdb")]
    pub db: PathBuf,

    /// Tenant identifier — used for the audit-ledger genesis hash.
    /// (NAV credentials are NOT loaded —
    /// `request-technical-annulment` does not call NAV, so the
    /// keychain is not consulted. Same posture as `mark-abandoned`.)
    #[arg(long, default_value = "default")]
    pub tenant: String,
}

/// Args for `aberp submit-annulment` (PR-13, ADR-0026 §1).
///
/// Same shape as [`SubmitInvoiceArgs`] except for one rename
/// (`--invoice-xml` → `--annulment-xml`, naming the body shape
/// instead of the generic "invoice xml"). The `--invoice-id` field
/// names the BASE invoice (which the annulment is FOR), matching
/// the `--references` semantics in
/// [`RequestTechnicalAnnulmentArgs`].
#[derive(Debug, Parser)]
pub struct SubmitAnnulmentArgs {
    /// Path to the `<InvoiceAnnulment>` XML written by a prior
    /// `aberp request-technical-annulment --out ...` run. The bytes
    /// on disk are the body submitted (base64-encoded inside the
    /// SOAP envelope per ADR-0026 §3).
    #[arg(long = "annulment-xml")]
    pub annulment_xml: PathBuf,

    /// Base invoice id (prefixed form, `inv_<ULID>`) — the invoice
    /// the annulment is FOR. Used to look up the prior
    /// `InvoiceTechnicalAnnulmentRequested` audit entry so the new
    /// wire-evidence entries share its idempotency key per the F8
    /// contract (ADR-0026 §"F8 contract").
    #[arg(long = "invoice-id")]
    pub invoice_id: String,

    /// Hungarian tax number of the submitter. Same accepted forms +
    /// parser as `submit-invoice` (`12345678`, `12345678-1`,
    /// `12345678-1-42`); only the 8-digit base goes to NAV per
    /// ADR-0009 §4.
    #[arg(long = "tax-number")]
    pub tax_number: String,

    /// Path to the tenant DuckDB file.
    #[arg(long, default_value = "./aberp.duckdb")]
    pub db: PathBuf,

    /// Tenant identifier — drives both the audit-ledger genesis
    /// hash and the keychain service-name lookup
    /// (`aberp.nav.<tenant>`).
    #[arg(long, default_value = "default")]
    pub tenant: String,

    /// Which NAV environment to submit against. No default —
    /// explicit per ADR-0020 §1 / ADR-0026 §1. Silently submitting
    /// an annulment to production when the operator meant test is
    /// the exact failure mode CLAUDE.md rule 12 names.
    #[arg(long, value_enum)]
    pub endpoint: NavEnv,
}

/// Args for `aberp recover-from-nav` (PR-21, ADR-0034 §1).
///
/// Five fields — same shape as
/// [`ObserveReceiverConfirmationArgs`]. ABERP looks up the
/// previously-issued invoice's NAV-facing invoice number from
/// the billing store (no `--nav-invoice-number` flag — same
/// posture as `observe-receiver-confirmation` per ADR-0028 §1).
/// No `--reason` flag per ADR-0034 §1: the recovery is
/// mechanical reconstruction of state NAV already has; the
/// audit-evidence chain (the preceding
/// `InvoiceCheckPerformed(outcome=exists)` entry plus the
/// recovered `InvoiceSubmissionResponse`) is itself the
/// justification.
#[derive(Debug, Parser)]
pub struct RecoverFromNavArgs {
    /// Invoice id (prefixed form, `inv_<ULID>`) — the state-2
    /// Pending invoice whose local Response chain needs
    /// reconstruction from NAV's prior record. The precondition
    /// (state-2 Pending AND most-recent
    /// `InvoiceCheckPerformed.outcome == "exists"`) is
    /// resolved from the audit ledger per ADR-0034 §5; loud-fail
    /// with operator-visible guidance on every non-recoverable
    /// shape (no prior check entry → run `aberp retry-submission`
    /// first to produce the Layer-2 Exists evidence).
    #[arg(long = "invoice-id")]
    pub invoice_id: String,

    /// Hungarian tax number of the submitter. Same accepted
    /// forms + parser as every other `submit-*` / `poll-*` /
    /// `observe-*` / `retry-*` command (`12345678`,
    /// `12345678-1`, `12345678-1-42`); only the 8-digit base
    /// goes to NAV per ADR-0009 §4.
    #[arg(long = "tax-number")]
    pub tax_number: String,

    /// Path to the tenant DuckDB file.
    #[arg(long, default_value = "./aberp.duckdb")]
    pub db: PathBuf,

    /// Tenant identifier — drives both the audit-ledger
    /// genesis hash and the keychain service-name lookup
    /// (`aberp.nav.<tenant>`).
    #[arg(long, default_value = "default")]
    pub tenant: String,

    /// Which NAV environment to query against. No default —
    /// explicit per ADR-0020 §1 / ADR-0034 §1 (same posture as
    /// every other NAV-touching command).
    #[arg(long, value_enum)]
    pub endpoint: NavEnv,
}

/// Args for `aberp observe-receiver-confirmation` (PR-15,
/// ADR-0028 §1).
///
/// Five fields — same shape as
/// [`PollAnnulmentAckArgs`]. ABERP looks up the base
/// invoice's NAV-facing invoice number from the billing store
/// (no `--nav-invoice-number` flag per ADR-0028 §1's "Does NOT
/// take --nav-invoice-number" posture).
#[derive(Debug, Parser)]
pub struct ObserveReceiverConfirmationArgs {
    /// Base invoice id (prefixed form, `inv_<ULID>`) — the
    /// invoice whose annulment-receiver-confirmation state is
    /// being observed. The annulment-side `transactionId` +
    /// idempotency key are resolved from the most-recent
    /// `InvoiceAnnulmentSubmissionResponse` audit entry per
    /// ADR-0028 §6 + §7; the NAV-facing invoice number is
    /// constructed from the base's billing row per
    /// ADR-0028 §1 / §8. Loud-fail if no prior wire response
    /// or if the billing row is missing.
    #[arg(long = "invoice-id")]
    pub invoice_id: String,

    /// Hungarian tax number of the submitter. Same accepted
    /// forms + parser as every other `submit-*` / `poll-*`
    /// command (`12345678`, `12345678-1`, `12345678-1-42`);
    /// only the 8-digit base goes to NAV per ADR-0009 §4.
    #[arg(long = "tax-number")]
    pub tax_number: String,

    /// Path to the tenant DuckDB file.
    #[arg(long, default_value = "./aberp.duckdb")]
    pub db: PathBuf,

    /// Tenant identifier — drives both the audit-ledger
    /// genesis hash and the keychain service-name lookup
    /// (`aberp.nav.<tenant>`).
    #[arg(long, default_value = "default")]
    pub tenant: String,

    /// Which NAV environment to query against. No default —
    /// explicit per ADR-0020 §1 / ADR-0028 §1 (same posture
    /// as every other `submit-*` / `poll-*` command).
    #[arg(long, value_enum)]
    pub endpoint: NavEnv,
}

/// Args for `aberp poll-annulment-ack` (PR-14, ADR-0027 §1).
///
/// Same shape as [`PollAckArgs`] — five fields, identical
/// semantics. The annulment-side `transactionId` is looked up
/// from the audit ledger (most-recent
/// `InvoiceAnnulmentSubmissionResponse` for `--invoice-id` per
/// ADR-0027 §4); operators do not pass it on the CLI.
#[derive(Debug, Parser)]
pub struct PollAnnulmentAckArgs {
    /// Base invoice id (prefixed form, `inv_<ULID>`) — the
    /// invoice whose annulment-submission ack status is being
    /// polled. The annulment-side `transactionId` is resolved
    /// from the most-recent
    /// `InvoiceAnnulmentSubmissionResponse` audit entry per
    /// ADR-0027 §4. Loud-fail if no such entry exists.
    #[arg(long = "invoice-id")]
    pub invoice_id: String,

    /// Hungarian tax number of the submitter. Same accepted
    /// forms + parser as `poll-ack` (`12345678`, `12345678-1`,
    /// `12345678-1-42`); only the 8-digit base goes to NAV per
    /// ADR-0009 §4.
    #[arg(long = "tax-number")]
    pub tax_number: String,

    /// Path to the tenant DuckDB file.
    #[arg(long, default_value = "./aberp.duckdb")]
    pub db: PathBuf,

    /// Tenant identifier — drives both the audit-ledger genesis
    /// hash and the keychain service-name lookup
    /// (`aberp.nav.<tenant>`).
    #[arg(long, default_value = "default")]
    pub tenant: String,

    /// Which NAV environment to poll against. No default —
    /// explicit per ADR-0020 §1 / ADR-0027 §1 (same posture as
    /// every other `submit-*` / `poll-*` command).
    #[arg(long, value_enum)]
    pub endpoint: NavEnv,
}

/// Args for `aberp export-invoice-bundle` (PR-16, ADR-0029 §1).
///
/// Four-plus-one fields: required invoice id + output path,
/// optional opt-in overwrite flag, plus the standard `--db` /
/// `--tenant` pair every CLI command takes. No `--tax-number`,
/// no `--endpoint`: the bundle reader does not call NAV.
#[derive(Debug, Parser)]
pub struct ExportInvoiceBundleArgs {
    /// Invoice id (prefixed form, `inv_<ULID>`) of the invoice
    /// whose audit-evidence bundle to produce. Every audit
    /// entry whose primary or chain-link invoice-id field
    /// matches per ADR-0029 §2 ("any-id-field-equality")
    /// lands in the bundle's `chain.jsonl` in `seq` order.
    /// Loud-fail if the bundle would contain zero entries
    /// (no audit-ledger entries reference this invoice id
    /// in any role).
    #[arg(long = "invoice-id")]
    pub invoice_id: String,

    /// Path to write the `.tar.zst` archive. Refuses to
    /// overwrite an existing file by default per ADR-0029
    /// §1 + CLAUDE.md rule 12; opt in to overwrite via
    /// [`Self::allow_overwrite`].
    #[arg(long)]
    pub out: PathBuf,

    /// Opt-in to overwriting an existing `--out` file. Default
    /// `false`. The refuse-overwrite default preserves
    /// operator-visible artifacts from accidental clobbering;
    /// the explicit opt-in makes overwrite a deliberate
    /// operator decision per CLAUDE.md rule 12.
    #[arg(long = "allow-overwrite", default_value_t = false)]
    pub allow_overwrite: bool,

    /// Path to the tenant DuckDB file. Read-only access; the
    /// audit-ledger crate opens the file via `Ledger::open`
    /// and runs `Ledger::entries()` + `Ledger::verify_chain()`
    /// against it. No DDL, no mutations.
    #[arg(long, default_value = "./aberp.duckdb")]
    pub db: PathBuf,

    /// Tenant identifier — drives the audit-ledger genesis
    /// hash. (NAV credentials are NOT loaded —
    /// `export-invoice-bundle` does not call NAV, so the
    /// keychain is not consulted. Same posture as
    /// `mark-abandoned` / `request-technical-annulment`.)
    #[arg(long, default_value = "default")]
    pub tenant: String,
}

/// Args for `aberp drain-submission-queue` (PR-18, ADR-0031 §3).
///
/// Five-plus-two fields. The five mirror every other NAV-touching
/// subcommand (`--tax-number`, `--tenant`, `--db`, `--endpoint`,
/// implicit per-invocation actor); the two extra are
/// `--xml-path-override` (repeatable; rescues pre-PR-18 entries
/// whose audit payload lacks `nav_xml_path`) and `--max-invoices`
/// (bounds the per-run wall-clock; default unbounded = whole queue).
#[derive(Debug, Parser)]
pub struct DrainSubmissionQueueArgs {
    /// Hungarian tax number of the submitter. Same accepted forms +
    /// parser as `submit-invoice` (`12345678`, `12345678-1`,
    /// `12345678-1-42`); only the 8-digit base goes to NAV per
    /// ADR-0009 §4.
    #[arg(long = "tax-number")]
    pub tax_number: String,

    /// Path to the tenant DuckDB file.
    #[arg(long, default_value = "./aberp.duckdb")]
    pub db: PathBuf,

    /// Tenant identifier — drives both the audit-ledger genesis
    /// hash and the keychain service-name lookup
    /// (`aberp.nav.<tenant>`).
    #[arg(long, default_value = "default")]
    pub tenant: String,

    /// Which NAV environment to submit against. No default —
    /// explicit per ADR-0020 §1 (same posture as every other
    /// `submit-*` / `poll-*` command).
    #[arg(long, value_enum)]
    pub endpoint: NavEnv,

    /// Per-invocation rescue mapping for pending invoices whose
    /// `InvoiceDraftCreatedPayload.nav_xml_path` is `None` (i.e.
    /// pre-PR-18 entries). Format: `<invoice-id>=<path>`. Repeatable
    /// (`--xml-path-override inv_X=/p/X.xml --xml-path-override
    /// inv_Y=/p/Y.xml`). The mapping takes precedence over the
    /// recorded path; this lets an operator drain a recovered
    /// backup with a different on-disk layout (`/Volumes/Backup/...`
    /// instead of `/Users/.../Documents/...`). Loud-fail per
    /// CLAUDE.md rule 12 if both the recorded path is `None` AND no
    /// override is supplied for the invoice id in question.
    #[arg(long = "xml-path-override")]
    pub xml_path_overrides: Vec<String>,

    /// Bound the number of invoices the drain submits in this run.
    /// `0` (default) means unbounded — drain the entire queue. Use
    /// a non-zero value when the operator wants to inspect the
    /// progress after a few invoices (e.g., the first NAV-testbed
    /// drain on a backlog).
    #[arg(long = "max-invoices", default_value_t = 0)]
    pub max_invoices: usize,
}

/// Args for `aberp drain-pending-retries` (PR-42, F45 closure /
/// ADR-0032 §4).
///
/// Five fields — mirror of [`DrainSubmissionQueueArgs`] minus
/// `--xml-path-override` (pre-PR-18 state-2 invoices loud-fail and
/// route to the manual `aberp retry-submission --invoice-xml <path>`
/// command) and minus `--reason` (the auto-reason names the drain
/// run; per-invoice operator-decision rationale lives on the manual
/// command).
#[derive(Debug, Parser)]
pub struct DrainPendingRetriesArgs {
    /// Hungarian tax number of the submitter. Same accepted forms +
    /// parser as `retry-submission` (`12345678`, `12345678-1`,
    /// `12345678-1-42`); only the 8-digit base goes to NAV per
    /// ADR-0009 §4.
    #[arg(long = "tax-number")]
    pub tax_number: String,

    /// Path to the tenant DuckDB file.
    #[arg(long, default_value = "./aberp.duckdb")]
    pub db: PathBuf,

    /// Tenant identifier — drives both the audit-ledger genesis hash
    /// and the keychain service-name lookup
    /// (`aberp.nav.<tenant>`).
    #[arg(long, default_value = "default")]
    pub tenant: String,

    /// Which NAV environment to retry against. No default — explicit
    /// per ADR-0020 §1 (same posture as every other `submit-*` /
    /// `poll-*` / `retry-*` / `drain-*` command).
    #[arg(long, value_enum)]
    pub endpoint: NavEnv,

    /// Bound the number of state-2 invoices the drain retries in
    /// this run. `0` (default) means unbounded — drain the whole
    /// state-2 backlog. Use a non-zero value when the operator
    /// wants to inspect progress after a few retries (e.g., the
    /// first NAV-testbed drain of a state-2 accumulation).
    #[arg(long = "max-invoices", default_value_t = 0)]
    pub max_invoices: usize,
}

#[derive(Debug, Parser)]
pub struct SetupNavCredentialsArgs {
    /// Tenant identifier whose keychain entries to populate (the
    /// service name becomes `aberp.nav.<tenant>`).
    #[arg(long, default_value = "default")]
    pub tenant: String,

    /// If set, exit non-zero rather than overwrite any keychain entry
    /// that already exists. Default behaviour is to overwrite,
    /// matching the operator-rotation flow per ADR-0009 §4.
    #[arg(long = "refuse-overwrite")]
    pub refuse_overwrite: bool,
}

/// Args for `aberp print-invoice` (PR-44ε.1, ADR-0037 §1.a + ADR-0021
/// "Print rendering path"). Five-plus-two fields:
///
///   - `--id <INV_ULID>`         — the invoice to render (the
///     audit-ledger `InvoiceDraftCreated.invoice_id` value).
///   - `--out <PATH>`            — where to write the PDF.
///   - `--db <PATH>`             — the tenant DuckDB file (read-only
///     for this command — the audit ledger is consulted; no writes).
///   - `--tenant <NAME>`         — the tenant identifier (drives the
///     audit-ledger genesis hash + the default seller.toml path).
///   - `--seller-toml <PATH>`    — override the default
///     `~/.aberp/<tenant>/seller.toml` location; optional. Lets
///     tests + offline runs pass a fixture file.
#[derive(Debug, Parser)]
pub struct PrintInvoiceArgs {
    /// Invoice id (prefixed form, `inv_<ULID>`) of the finalized invoice
    /// to print. The audit ledger MUST carry an `InvoiceDraftCreated`
    /// entry for this id — i.e., the invoice was issued via
    /// `aberp issue-invoice` / `issue-storno` / `issue-modification`
    /// against this tenant DB.
    #[arg(long)]
    pub id: String,

    /// Path to write the printed-invoice PDF. Refuses to overwrite an
    /// existing file unless `--allow-overwrite` is passed (same posture
    /// as `export-invoice-bundle --out` per CLAUDE.md rule 12).
    #[arg(long)]
    pub out: PathBuf,

    /// Path to the tenant DuckDB file. Read-only access — the
    /// print-invoice command does not write to the audit ledger or
    /// the billing tables. Same default + override shape as every
    /// other `--db` flag on this CLI.
    #[arg(long, default_value = "./aberp.duckdb")]
    pub db: PathBuf,

    /// Tenant identifier — drives the audit-ledger genesis hash and the
    /// default seller-info TOML location. (NAV credentials are NOT
    /// loaded — this command does not call NAV.)
    #[arg(long, default_value = "default")]
    pub tenant: String,

    /// Override path for the seller-info TOML (bank account / IBAN /
    /// SWIFT / bank name). When unset, defaults to
    /// `$HOME/.aberp/<tenant>/seller.toml`. The expected shape is
    /// flat key-value lines (`bank_account_number = "..."`, `iban =
    /// "..."`, `bank_name = "..."`, `swift_bic = "..."`); see
    /// `apps/aberp/tests/fixtures/seller_minimal.toml` for an example.
    #[arg(long = "seller-toml")]
    pub seller_toml: Option<PathBuf>,
}
