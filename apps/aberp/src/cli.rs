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
