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
