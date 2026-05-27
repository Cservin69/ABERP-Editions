//! Orchestration for the `aberp issue-invoice` subcommand.
//!
//! Pipeline:
//!
//! 1. Parse the JSON input into a [`InvoiceInputJson`] struct.
//! 2. Resolve tenant id and series code (loud-fail on invalid input).
//! 3. **Load NAV credentials from the OS keychain** (PR-7-A — closes
//!    F15 and F6). Required for the operator's session identity, even
//!    though PR-7-A does not yet submit to NAV. Missing keychain
//!    items fail loud per CLAUDE.md rule 12 before any DB write.
//! 4. Compute the binary hash and build [`aberp_audit_ledger::LedgerMeta`].
//! 5. Open a tenant DuckDB connection.
//! 6. Pre-tx setup (idempotent, no allocation occurs here):
//!    - Ensure the billing schema exists via `DuckDbBillingStore::ensure_schema`.
//!    - Ensure the requested series exists (auto-create on first run).
//!    - Take the Connection back via `into_connection`.
//!    - Ensure the audit-ledger schema exists.
//! 7. Build the [`aberp_billing::IssueInvoiceCommand`] and the
//!    [`aberp_billing::AllocateArgs`] from the parsed input.
//! 8. Open a single DuckDB transaction; under it:
//!    - Call [`aberp_billing::allocate_in_tx`] to burn the next number
//!      and write the reservation + invoice rows (ADR-0009 §3 steps 1–5).
//!    - On the `Fresh` branch, call [`aberp_audit_ledger::append_in_tx`]
//!      twice: `InvoiceSequenceReserved` then `InvoiceDraftCreated`
//!      (ADR-0009 §3 step 6) using the keychain-derived
//!      [`Actor::from_local_cli`] — NOT `Actor::test_only` (F15).
//!    - Commit (ADR-0009 §3 step 7).
//! 9. Drop the Connection to release the DuckDB file lock, then re-open
//!    a fresh `Ledger` for `verify_chain` (the verify path stays
//!    Connection-owning per session-6's verify-path decision).
//! 10. Serialize the [`ReadyInvoice`] to NAV `InvoiceData` XML.
//!
//! # ADR-0008 §Storage conformance (PR-6 close-out)
//!
//! Steps 7's billing writes and audit appends are in the **same DuckDB
//! transaction**. A crash or returned error between [`allocate_in_tx`]
//! and `tx.commit()` rolls back *both* halves cleanly — the tenant DB is
//! left exactly as before the issuance attempt. The rollback contract is
//! pinned by the conformance tests in
//! `apps/aberp/tests/rollback_conformance.rs` (panic-injection + drop
//! variants).
//!
//! The replay branch (`AllocateOutcome::Replay`) intentionally skips the
//! audit appends: the prior issuance already wrote its entries, and
//! ADR-0008's append-only contract forbids writing duplicates for the
//! same business event.

use std::path::Path;
use std::str::FromStr;

use aberp_audit_ledger::{self as audit_ledger, Actor, EventKind, Ledger, LedgerMeta, TenantId};
use aberp_billing::{
    self as billing, huf_equivalent_round_half_even, AllocateArgs, AllocateOutcome,
    BankAccountSnapshot, BillingStore, Currency, CustomerId, DraftInvoice, DuckDbBillingStore, Huf,
    IdempotencyKey, InvoiceId, InvoiceSeries, IssueInvoiceCommand, LineItem, RateMetadata,
    ResetPolicy, SeriesCode, SeriesId,
};
use aberp_mnb_rates::{MnbError, MnbRate, SOURCE as MNB_SOURCE};
use aberp_nav_transport::NavCredentials;
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use duckdb::Connection;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use ulid::Ulid;

use crate::audit_payloads;
use crate::binary_hash;
use crate::cli::IssueInvoiceArgs;
use crate::mnb_rates_provider::{LiveMnbRatesProvider, MnbRatesProvider};
use crate::nav_xml::{
    self, CustomerAddress, CustomerInfo, NavParties, SupplierConfigError, SupplierInfo,
};
use crate::submission_queue;

/// Maximum number of days to walk back per ADR-0037 §2.b when MNB has no
/// rate for the supply-fulfillment date. A139 — the cap is **7
/// calendar days** (one week), chosen because:
///
/// 1. **Hungarian non-publication windows are short.** Standard weekend
///    (Sat + Sun) + a Monday public holiday gives 3 consecutive
///    non-publication days; the longest Hungarian holiday window
///    historically is the Christmas → New Year stretch (≤ 5 days in
///    practice). A 7-day cap absorbs that with margin.
///
/// 2. **Operator-clock-skew loud-fail.** A larger cap would silently
///    accept a fulfillment-date pushed back into a pre-MNB-publication
///    epoch (operator clock skew, or a typo in the supply date). 7 days
///    is the largest window where "this is a real holiday stretch"
///    remains the more-likely explanation than "the date is wrong";
///    beyond that, loud-fail is the CLAUDE.md rule 12 posture.
///
/// 3. **Calendar-week is operator-intuitive.** The cap surfaces in the
///    typed loud-fail error as "no MNB rate found in the 7 days
///    preceding {date}"; one calendar week is a unit operators
///    reconcile against without needing to count business days.
///
/// A larger cap is a deferred candidate per ADR-0037 §5; the trigger
/// would be a real operational case where a legitimate fulfillment date
/// falls into a > 7-day non-publication window AND the operator
/// confirms the rate is still regulatorily applicable.
pub const MNB_WALKBACK_DAYS_CAP: i64 = 7;

/// PR-44γ — sentinel substring on the walk-back-exhausted loud-fail
/// message; pinned by the offline integration test so a future
/// refactor that drops the C2 + ADR-0037 §2.b loud-fail surface
/// would break the test, not silently regress. Per CLAUDE.md rule 12.
pub const ERR_NO_RATE_AFTER_WALKBACK: &str = "no MNB rate published";

/// PR-44γ — sentinel substring on the transport / parse / other-MNB-
/// error loud-fail message (every non-`NoRateForCurrency` variant
/// propagates as-is per ADR-0037 §4 invariant C2; no silent fallback).
pub const ERR_MNB_FETCH_FAILED: &str = "MNB rate-fetch failed";

// ──────────────────────────────────────────────────────────────────────
// Input JSON shape (NAV-aligned per Ervin's preference, session 5)
// ──────────────────────────────────────────────────────────────────────

// PR-47α / session-64 — `Serialize` added so the SPA-issue route can
// side-store the operator's invoice-content payload alongside the
// NAV-output XML at `~/.aberp/serve/<tenant>/issued/<ULID>.input.json`.
// The storno route reads this sibling file back to reconstruct the
// storno's own body content (lines + parties) — the storno's wire
// content is the base's content, modulo the negation that the
// `render_storno_data` emitter performs at render time. The CLI's
// `--in <PATH>` flow does NOT consume the side-stored file (the CLI
// operator owns their own JSON); side-store is purely the SPA-storno
// reconstruction path. Field-name discipline matches the existing
// `Deserialize`-side `serde(rename = "...")` so the round-trip preserves
// the camelCase wire form.
#[derive(Debug, Deserialize, Serialize)]
pub struct InvoiceInputJson {
    pub supplier: SupplierJson,
    pub customer: CustomerJson,
    pub lines: Vec<LineJson>,
    /// PR-82 — buyer-facing per-invoice global note ("Megjegyzés").
    /// Optional; `None` for invoices the operator does not annotate.
    /// Recipient-facing only — NEVER emitted into the NAV InvoiceData
    /// XML (see `adr/0042-invoice-notes-never-in-nav-xml.md`).
    ///
    /// `#[serde(default)]` keeps pre-PR-82 side-stored input.json files
    /// readable — they deserialise with `invoice_note: None`. The
    /// `rename = "invoiceNote"` matches the SPA's camelCase wire form.
    #[serde(default, rename = "invoiceNote")]
    pub invoice_note: Option<String>,
    /// PR-84 — operator-supplied payment deadline (Fizetési határidő) in
    /// canonical YYYY-MM-DD form. `None` for pre-PR-84 side-stored
    /// bodies AND for CLI callers that do not yet surface the field —
    /// the issuance pipeline defaults to the system issue date in that
    /// case (preserves pre-PR-84 wire-on-disk behaviour).
    #[serde(default, rename = "paymentDeadline")]
    pub payment_deadline: Option<String>,
    /// PR-84 — operator-supplied delivery / fulfillment date
    /// (Teljesítési dátum) in canonical YYYY-MM-DD form. REGULATORY:
    /// drives NAV's VAT-period assignment via `<invoiceDeliveryDate>`.
    /// `None` for pre-PR-84 bodies / CLI callers — defaulted to issue
    /// date.
    #[serde(default, rename = "deliveryDate")]
    pub delivery_date: Option<String>,
    /// PR-84 — audit discriminant captured at form time when the
    /// operator picked a delivery date OUTSIDE the comfort zone
    /// [invoice_date, payment_deadline]. The SPA's inline "Are you
    /// sure?" affordance stamps this as `"BeforeInvoiceDate"` or
    /// `"AfterPaymentDeadline"`; in-range choices send `None` (no
    /// audit flag, default operator path). The server independently
    /// re-classifies post-issuance via
    /// `aberp_billing::classify_delivery_date` for defence in depth;
    /// the wire value here is what the operator SAW + confirmed at
    /// form time, persisted to the audit payload verbatim so the
    /// tamper-evident trail records the operator's UI experience.
    #[serde(default, rename = "deliveryDateOverride")]
    pub delivery_date_override: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct SupplierJson {
    #[serde(rename = "taxNumber")]
    pub tax_number: String,
    pub name: String,
    pub address: AddressJson,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct AddressJson {
    #[serde(rename = "countryCode")]
    pub country_code: String,
    #[serde(rename = "postalCode")]
    pub postal_code: String,
    pub city: String,
    pub street: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct CustomerJson {
    #[serde(rename = "taxNumber")]
    pub tax_number: String,
    pub name: String,
    /// PR-77 / session-101 — NAV business-rule `CUSTOMER_DATA_EXPECTED`
    /// requires a full `<customerAddress>` block whenever
    /// `customerVatStatus != PRIVATE_PERSON`. The SPA populates this
    /// field from the operator-selected partner record (PR-54 buyer
    /// combobox); CLI callers can supply it directly. Optional on the
    /// wire so pre-PR-77 side-stored `input.json` files still
    /// deserialize — but the issuance pipeline now refuses any DOMESTIC
    /// invoice without it (preflight `CustomerAddressMissing` + the
    /// validator's customerVatStatus-aware required-children rule).
    #[serde(default)]
    pub address: Option<AddressJson>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct LineJson {
    pub description: String,
    pub quantity: u32,
    #[serde(rename = "unitPrice")]
    pub unit_price: i64,
    #[serde(rename = "vatRatePercent")]
    pub vat_rate_percent: u16,
    /// PR-82 — buyer-facing per-line note ("Megjegyzés"). Optional;
    /// `None` for lines the operator does not annotate. Recipient-
    /// facing only — NEVER reaches the NAV InvoiceData XML. Pre-PR-82
    /// side-stored bodies deserialise with `note: None` via
    /// `#[serde(default)]`.
    #[serde(default)]
    pub note: Option<String>,
}

// ──────────────────────────────────────────────────────────────────────
// Entry point
// ──────────────────────────────────────────────────────────────────────

pub fn run(args: &IssueInvoiceArgs) -> Result<()> {
    // PR-44γ — construct the production MNB-rates provider only when
    // the EUR (non-HUF) path actually needs one. The HUF path stays
    // network-free (no reqwest::Client built); a hypothetical
    // `reqwest::Client::builder().build()` failure does NOT loud-fail
    // a HUF issuance.
    //
    // PR-60 / session-80 — the issuance pipeline (`issue_from_parsed`)
    // is now async-native because `MnbRatesProvider::fetch_official_rate`
    // is async (it `.await`s `MnbClient::fetch_official_rate` directly,
    // no inner runtime). The CLI owns the tokio runtime at this
    // top-level boundary and `block_on`s the async pipeline exactly
    // once — outside any pre-existing runtime, so no nested-runtime
    // panic. Same shape as `submit_invoice::run` and `poll_ack::run`
    // (PR-56 / session-76).
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio current-thread runtime for issue-invoice CLI")?;
    runtime.block_on(async {
        match args.currency.to_billing_currency() {
            Currency::Huf => run_with_provider(args, &NeverProvider).await,
            _ => {
                let provider = LiveMnbRatesProvider::new()
                    .context("build MNB rates provider for non-HUF issuance")?;
                run_with_provider(args, &provider).await
            }
        }
    })
}

/// PR-44γ — stand-in [`MnbRatesProvider`] for the HUF code path.
/// Never expected to be invoked (the HUF branch of
/// `run_with_provider` does not consult the provider); any call here
/// is a bug, so the impl panics with a named message rather than
/// silently returning a placeholder rate. Pinned by the
/// `huf_default_path_unchanged_no_rate_metadata`-style tests via the
/// call-count assertions on the fake provider.
struct NeverProvider;

#[async_trait]
impl MnbRatesProvider for NeverProvider {
    async fn fetch_official_rate(
        &self,
        _currency: Currency,
        _date: time::Date,
    ) -> Result<MnbRate, MnbError> {
        unreachable!(
            "NeverProvider must not be consulted — the HUF issuance path is rate-free per ADR-0037 §1"
        )
    }
}

/// PR-44γ — `run`'s body, parameterised on the
/// [`MnbRatesProvider`]. Production calls reach here via `run()` with
/// the real `LiveMnbRatesProvider`; tests inject a fake provider to
/// exercise the EUR path offline.
///
/// PR-44ζ / session-59 — refactored to a thin wrapper over
/// [`issue_from_parsed`]. The CLI-specific responsibilities (read JSON
/// from `--in`, load NAV credentials from the keychain, mint the
/// `Actor`, print the success line) stay here; the
/// allocation-and-NAV-XML pipeline moves to the library function so
/// the new `POST /invoices/issue` route (`serve.rs::issue_invoice_request`)
/// can call the same path without re-implementing it.
pub async fn run_with_provider<P: MnbRatesProvider + ?Sized>(
    args: &IssueInvoiceArgs,
    provider: &P,
) -> Result<()> {
    // PR-60 / session-80 — span is scoped via Instrument-on-the-future
    // shape (NOT `.entered()`) so the !Send EnteredSpan guard does NOT
    // cross the inner `.await`. Same fix shape as PR-56's poll_loop
    // (session-76) — the structural invariant being preserved is that
    // any code path reachable from the axum handler keeps a Send
    // future across yield points.
    use tracing::Instrument;
    let span = tracing::info_span!("issue_invoice");
    async move {
        // 1. Read + parse the JSON input.
        let input_bytes = std::fs::read(&args.r#in)
            .with_context(|| format!("read input JSON from {}", args.r#in.display()))?;
        let input: InvoiceInputJson =
            serde_json::from_slice(&input_bytes).context("parse input JSON")?;
        tracing::info!(lines = input.lines.len(), "JSON input parsed");

        // 2. Load NAV credentials from the OS keychain BEFORE any DB write.
        //    Per ADR-0009 §4 + ADR-0020 §3 + CLAUDE.md rule 12: a missing
        //    keychain item is a hard error, not a silent fallback. Failing
        //    here keeps the tenant DB pristine if credentials aren't set up.
        //
        //    The login is then the user_id baked into every audit-ledger
        //    entry written by this CLI invocation (Actor::from_local_cli),
        //    closing fortnightly review F15 — Actor::test_only is no longer
        //    reachable on a production code path.
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

        let summary = issue_from_parsed(
            input,
            &args.db,
            &args.tenant,
            &args.series,
            args.currency.to_billing_currency(),
            args.out.clone(),
            actor,
            provider,
            // PR-73 — CLI invocation does not exercise the bank picker
            // (the SPA route is the only surface that resolves the
            // `bank_account_id`). CLI-issued rows persist NULL across
            // the five `bank_account_*` invoice columns; the printed
            // PDF render (PR-D) falls back to the seller.toml
            // legacy-flat-root bank for those rows.
            None,
        )
        .await?;

        // Match the XML's invoice-number format exactly (5-digit padding) so
        // operator logs, audit entries, and the XML body all agree.
        println!(
            "issued invoice {} -> {} (audit chain verified)",
            summary.invoice_number,
            args.out.display(),
        );
        Ok::<(), anyhow::Error>(())
    }
    .instrument(span)
    .await
}

/// PR-44ζ / session-59 — library-callable issuance entry. Consumed by
/// [`run_with_provider`] (the CLI path) AND by
/// `serve::issue_invoice_request` (the loopback `POST /invoices/issue`
/// route landing at THIS PR). Both surfaces share one allocation +
/// audit-ledger + NAV-XML pipeline so a regression in the issuance
/// path surfaces at both gates.
///
/// Pipeline (steps map to the pre-PR-44ζ `run_with_provider` numbering
/// in this module's doc comment):
///
///   4.  Compute binary hash + build [`LedgerMeta`].
///   4a. ADR-0031 §5 pre-allocation cap check.
///   5–6. Pre-tx setup (schemas + series).
///   7.  Build command + (for non-HUF) fetch MNB rate + stamp metadata.
///   8.  Single DuckDB transaction: allocate + audit appends.
///   9.  `verify_chain` (success-criterion gate).
///   9a. ADR-0030 §2 audit-ledger mirror sync.
///   10. Render NAV XML + XSD validate + write to `nav_xml_out`.
///
/// What this fn does NOT do (CLI/route boundary):
///   - Read JSON from disk (caller hands in parsed [`InvoiceInputJson`]).
///   - Load NAV credentials (caller builds [`Actor`] from whatever
///     identity surface they have — keychain on CLI, the AppState's
///     session-derived login on the route).
///   - Print the operator-facing success line.
///
/// `nav_xml_out` carries the on-disk path the NAV body is written to;
/// recorded on the `InvoiceDraftCreated` payload's `nav_xml_path` field
/// per ADR-0031 §2 so the downstream drain worker + the
/// `print-invoice` orchestrator can read it back. The CLI threads the
/// operator-supplied `--out` path here; the route mints a server-side
/// deterministic path under `~/.aberp/serve/<tenant>/issued/<ulid>.xml`
/// (see `serve::issued_xml_path`).
#[allow(clippy::too_many_arguments)]
pub async fn issue_from_parsed<P: MnbRatesProvider + ?Sized>(
    input: InvoiceInputJson,
    db: &Path,
    tenant_str: &str,
    series_str: &str,
    currency: Currency,
    nav_xml_out: std::path::PathBuf,
    actor: Actor,
    provider: &P,
    bank_snapshot: Option<aberp_billing::BankAccountSnapshot>,
) -> Result<IssuedInvoiceSummary> {
    if input.lines.is_empty() {
        return Err(anyhow!("input has no lines"));
    }

    // PR-50 / session-70 — pre-issuance supplier shape guard. Refuse
    // to burn a sequence slot when the supplier's tax number isn't
    // a valid Hungarian ADÓSZÁM, so the audit ledger never carries
    // a fresh draft that the NAV submit endpoint will reject hours
    // later for a malformed `<supplierTaxNumber>`. The route layer
    // (`serve::validate_issue_request`) also calls this so the SPA
    // gets a typed 400 before reaching the issuance pipeline; this
    // guard is the defence in depth for the CLI surface (and any
    // future library caller) per CLAUDE.md rule 12.
    let supplier_for_check = SupplierInfo {
        tax_number: input.supplier.tax_number.clone(),
        name: input.supplier.name.clone(),
        address_country_code: input.supplier.address.country_code.clone(),
        address_postal_code: input.supplier.address.postal_code.clone(),
        address_city: input.supplier.address.city.clone(),
        address_street: input.supplier.address.street.clone(),
    };
    if let Err(e) = nav_xml::validate_supplier_info(&supplier_for_check) {
        return Err(supplier_config_error_anyhow(e));
    }

    // 2. Resolve tenant id + series code (loud-fail on invalid input).
    let tenant = TenantId::new(tenant_str.to_string())
        .ok_or_else(|| anyhow!("tenant value '{}' is empty or has a null byte", tenant_str))?;
    let series_code = SeriesCode::new(series_str.to_string())
        .ok_or_else(|| anyhow!("series value '{}' fails SeriesCode validation", series_str))?;

    // 4. Compute binary hash, then build the audit-ledger metadata once
    //    for the entire process. `LedgerMeta` anchors `time_mono` and is
    //    cheap to clone; threaded into every append_in_tx call.
    let binary_hash_bytes = binary_hash::compute().context("compute binary hash")?;
    let ledger_meta = LedgerMeta::new(tenant.clone(), binary_hash_bytes);

    // 4a. PR-18 / ADR-0031 §5 — pre-allocation hard-cap check
    //     against the offline submission queue. Refuses fresh
    //     allocation when the ledger already shows the
    //     `HARD_CAP_PENDING` threshold of unsubmitted invoices.
    //     Loud-fail per CLAUDE.md rule 12 BEFORE the allocator
    //     tx opens so the sequence-slot invariant (ADR-0009 §3)
    //     is preserved. The check opens + drops its own Ledger
    //     handle; pre_tx_setup below opens a fresh Connection.
    let pending_count = submission_queue::count_pending(db, tenant.clone(), binary_hash_bytes)
        .context("count pending submissions (ADR-0031 §5 cap check)")?;
    if pending_count >= submission_queue::HARD_CAP_PENDING {
        return Err(anyhow!(
            "submission queue is full ({}/{} pending invoices per ADR-0009 §7 / ADR-0031 §5); \
             run `aberp drain-submission-queue --endpoint <test|production> --tax-number ...` \
             to submit the backlog, or `aberp mark-abandoned --invoice-id <id> --reason ...` \
             on invoices the operator has decided not to submit",
            pending_count,
            submission_queue::HARD_CAP_PENDING,
        ));
    }
    tracing::info!(
        pending_count = pending_count,
        cap = submission_queue::HARD_CAP_PENDING,
        "ADR-0031 §5 cap check passed"
    );

    // 5–6. Pre-tx setup: schemas + series.
    let (conn, series) = pre_tx_setup(db, &series_code)?;

    // 7. Build IssueInvoiceCommand + AllocateArgs for the tx body.
    let command = build_command(&input, &series_code)?;
    let idempotency_key = command.idempotency_key;
    // PR-84 — server-stamp the immutable invoice date (no client clock
    // ever sets this). The two operator-supplied dates (payment
    // deadline + delivery date) parse off the wire body; absent fields
    // fall back to the issue date (preserves pre-PR-84 behaviour for
    // CLI callers / older side-stored input.json files).
    let issue_date = OffsetDateTime::now_utc();
    let issue_date_calendar = issue_date.date();
    let date_fmt = time::macros::format_description!("[year]-[month]-[day]");
    let payment_deadline_date = match input.payment_deadline.as_deref() {
        Some(s) => time::Date::parse(s, &date_fmt).map_err(|_| {
            anyhow!(
                "payment_deadline `{}` is not a YYYY-MM-DD calendar date (PR-84 wire shape)",
                s
            )
        })?,
        None => issue_date_calendar,
    };
    let delivery_date_date = match input.delivery_date.as_deref() {
        Some(s) => time::Date::parse(s, &date_fmt).map_err(|_| {
            anyhow!(
                "delivery_date `{}` is not a YYYY-MM-DD calendar date (PR-84 wire shape)",
                s
            )
        })?,
        None => issue_date_calendar,
    };
    let draft = DraftInvoice {
        id: InvoiceId::new(),
        series_id: series.id,
        customer_id: command.customer_id,
        lines: command.lines.clone(),
        issue_date,
        payment_deadline: payment_deadline_date,
        delivery_date: delivery_date_date,
    };

    // PR-44γ — for non-HUF currencies, fetch the MNB rate (with D-1
    // walk-back per ADR-0037 §2.b up to A139's 7-day cap) and compute
    // the round-half-even HUF-equivalent total per §1.c + C11. The
    // rate fetch happens BEFORE the tx opens so a fetch failure
    // leaves the tenant DB unchanged (no half-issued state).
    let rate_metadata: Option<RateMetadata> = if matches!(currency, Currency::Huf) {
        None
    } else {
        Some(fetch_and_stamp_rate(provider, currency, issue_date.date(), &command.lines).await?)
    };

    let allocate_args = AllocateArgs {
        series_id: series.id,
        draft,
        idempotency_key,
        currency,
        rate_metadata: rate_metadata.clone(),
        // PR-73 / ADR-0040 §addendum — bank snapshot resolved by the
        // route handler (or `None` for CLI / library callers).
        bank_snapshot: bank_snapshot.clone(),
        // PR-82 — buyer-facing global note. Threads from the wire body
        // straight to DuckDB via `allocate_in_tx`. The audit payload's
        // `with_notes` builder stamps the same value below so the
        // operator-twin's record of "what was issued" is complete.
        invoice_note: input.invoice_note.clone(),
    };

    // 8. One transaction across the billing writes and audit appends.
    //    `run_single_tx` owns the tx lifecycle: it commits on Ok and
    //    relies on `Transaction::drop` for rollback on Err or panic.
    //
    //    PR-18 / ADR-0031 §2: the `nav_xml_out` path is threaded into
    //    `run_single_tx` so the InvoiceDraftCreated payload's
    //    `nav_xml_path` field records where the XML will be written.
    //    The drain worker consumes this at submit time without
    //    requiring an operator-supplied path argument.
    let outcome = run_single_tx(
        conn,
        &ledger_meta,
        allocate_args,
        idempotency_key,
        actor,
        nav_xml_out.clone(),
        currency,
        rate_metadata.clone(),
        bank_snapshot.clone(),
        // PR-82 — invoice-level buyer note. Threaded into the audit
        // payload alongside the per-line notes (which the audit payload
        // builder reads off `outcome.invoice.lines[i].note`).
        input.invoice_note.clone(),
        // PR-84 — operator's SPA-form override discriminant for the
        // delivery-date choice. `None` means in-range (default path,
        // no audit flag); `Some(...)` carries the operator's confirmed
        // out-of-range choice verbatim for the audit trail. The audit
        // payload's `with_invoice_dates` builder also stamps the two
        // calendar dates so an inspector can reconstruct the
        // comfort-zone classification independently.
        input.delivery_date_override.clone(),
    )?;

    let invoice = outcome.invoice;
    let is_fresh = outcome.was_fresh;
    tracing::info!(
        seq = invoice.sequence_number,
        fresh = is_fresh,
        idempotency_key = ?idempotency_key,
        "invoice issued"
    );

    // 9. Verify the audit chain — the success-criterion gate. Per the
    //    session-6 verify-path decision: re-open a fresh Ledger after
    //    the tx commits and the tx-Connection drops.
    let ledger =
        Ledger::open(db, tenant.clone(), binary_hash_bytes).context("open audit ledger")?;
    let verified = ledger
        .verify_chain()
        .context("audit-ledger chain verification failed AFTER issuance")?;
    tracing::info!(entries_verified = verified, "audit chain verified");

    // 9a. PR-17 / ADR-0030 §2 — sync the audit-ledger mirror file
    //     post-commit. On a fresh DB (or first post-PR-17 invocation
    //     on a pre-existing DB) `sync_mirror` runs the implicit
    //     one-time backfill per ADR-0030 §7 and logs
    //     `audit_mirror_initialized` at INFO.
    let mirror_path = audit_ledger::mirror_path_for(db);
    ledger
        .sync_mirror(&mirror_path)
        .context("sync audit-ledger mirror file after commit")?;

    // 10. Serialize the ReadyInvoice to NAV XML.
    //
    // PR-44δ — currency + rate_metadata thread into `render_invoice_data`
    // so EUR invoices serialize `<currencyCode>EUR`, `<exchangeRate>` at
    // 6 decimals, and per-VAT-rate `*HUF` amounts computed from the
    // stamped MNB rate (NOT re-fetched — read from the in-memory
    // `RateMetadata` we just stamped onto the DuckDB row + audit payload
    // earlier in this same call). HUF invoices serialize the same
    // byte-near-identical shape as pre-PR-44δ with `<exchangeRate>1.000000`
    // (uniformly 6-decimal per C11 — the prior `1` form is superseded).
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
            // PR-77 / session-101 — `<customerAddress>` is required for any
            // DOMESTIC (non-PRIVATE_PERSON) customerVatStatus. The wire
            // body's `customer.address` is `Option<_>` so pre-PR-77
            // CLI-issued bodies still parse; the preflight in
            // `serve.rs::issue_invoice_request` fires
            // `CustomerAddressMissing` when an address is required but
            // absent so the operator surfaces the gap BEFORE the
            // sequence is burned.
            address: input.customer.address.map(|a| CustomerAddress {
                country_code: a.country_code,
                postal_code: a.postal_code,
                city: a.city,
                street: a.street,
            }),
        },
    };
    let xml = nav_xml::render_invoice_data(
        &invoice,
        &series_code,
        &parties,
        currency,
        rate_metadata.as_ref(),
    )
    .context("render NAV XML")?;
    // PR-9-0 / ADR-0022: runtime <InvoiceData> v3.0 invariant check
    // between render and disk write. On failure the typed
    // `NavXsdValidationError` flows up as `anyhow::Error` — loud-fail
    // per CLAUDE.md rule 12 keeps malformed XML off both disk and the
    // wire. Audit entries from the prior commit DO remain in the
    // ledger (they describe what happened — the sequence was
    // allocated); recovery is to fix the emitter/validator and re-run
    // with the same input JSON, hitting the Replay branch which
    // returns the same invoice and re-renders cleanly.
    aberp_nav_xsd_validator::validate_invoice_data(&xml)
        .context("NAV InvoiceData v3.0 invariant check (ADR-0022) failed for rendered XML")?;
    tracing::info!(
        bytes = xml.len(),
        nav_xsd_version = aberp_nav_xsd_validator::NAV_XSD_VERSION,
        "NAV InvoiceData XML passed v3.0 invariant check"
    );
    nav_xml::write_to_path(&nav_xml_out, &xml)?;
    tracing::info!(path = %nav_xml_out.display(), bytes = xml.len(), "NAV XML written");

    let invoice_number = format!("{}/{:05}", series_code.as_str(), invoice.sequence_number);
    tracing::info!(
        invoice_number = %invoice_number,
        entries_verified = verified,
        "issuance completed"
    );
    Ok(IssuedInvoiceSummary {
        invoice_id: invoice.id.to_prefixed_string(),
        invoice_number,
        nav_xml_path: nav_xml_out,
    })
}

/// PR-44ζ / session-59 — minimal carrier the two issuance entry points
/// hand back to their caller. The CLI path consumes
/// [`invoice_number`] for the operator-facing success line; the
/// `POST /invoices/issue` route consumes [`invoice_id`] for the SPA's
/// detail-modal navigation. [`nav_xml_path`] is returned so the route
/// handler can log the on-disk write location (operator inspection +
/// debug); the CLI already knows it from `args.out`.
#[derive(Debug)]
pub struct IssuedInvoiceSummary {
    /// Prefixed-ULID invoice id (e.g. `inv_01ARZ3NDEK...`) — the
    /// audit-ledger primary key the SPA uses to open the detail modal.
    pub invoice_id: String,
    /// NAV-aligned `<series>/<5-digit-seq>` form (e.g.
    /// `INV-default/00013`) — operator-facing identifier matching the
    /// NAV body's `<invoiceNumber>`.
    pub invoice_number: String,
    /// Server-determined on-disk path of the rendered NAV XML. Recorded
    /// on the `InvoiceDraftCreated` payload's `nav_xml_path` field; the
    /// drain worker + `print-invoice` orchestrator read it back at
    /// submit / render time.
    pub nav_xml_path: std::path::PathBuf,
}

// ──────────────────────────────────────────────────────────────────────
// Pre-tx setup
// ──────────────────────────────────────────────────────────────────────

/// Open the tenant DB, run idempotent schema creation for both crates,
/// and ensure the requested series exists. Returns the Connection
/// (handed back from the billing store via `into_connection`) and the
/// resolved `InvoiceSeries`. No allocation occurs here; ADR-0008
/// §Storage's transactional contract is engaged in `run_single_tx`.
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
// The single transaction (PR-6 close-out)
// ──────────────────────────────────────────────────────────────────────

/// Carrier for the single-tx outcome that the caller actually needs
/// after commit: the ready invoice and the fresh-vs-replay bit. Keeps
/// `run_single_tx`'s return type narrow.
struct TxOutcome {
    invoice: aberp_billing::ReadyInvoice,
    was_fresh: bool,
}

/// Open one DuckDB transaction, run the ADR-0009 §3 allocator and the
/// ADR-0008 §Storage audit appends inside it, and commit. Returns the
/// outcome the caller needs after commit.
///
/// Rollback contract: if any step returns `Err`, the function returns
/// without committing; `Transaction::drop` rolls back. If a panic
/// unwinds across this function, the same `drop` runs. Both paths leave
/// the tenant DB in its pre-call state. Exercised by
/// `apps/aberp/tests/rollback_conformance.rs`.
#[allow(clippy::too_many_arguments)]
fn run_single_tx(
    mut conn: Connection,
    ledger_meta: &LedgerMeta,
    allocate_args: AllocateArgs,
    idempotency_key: IdempotencyKey,
    actor: Actor,
    nav_xml_path: std::path::PathBuf,
    currency: Currency,
    rate_metadata: Option<RateMetadata>,
    bank_snapshot: Option<BankAccountSnapshot>,
    // PR-82 — invoice-level buyer note ("Megjegyzés"). Stamped onto the
    // `InvoiceDraftCreated` audit payload via `with_notes` so the
    // operator-twin's regulatory record of "what was issued" includes
    // the note alongside the wire-XML pointer. Per-line notes ride on
    // `outcome.invoice.lines[i].note` and are read inline by the
    // payload builder.
    invoice_note: Option<String>,
    // PR-84 — operator's SPA-form override discriminant for the
    // delivery-date choice. `None` (in-range) leaves the audit payload's
    // `delivery_date_override` field at `None`; `Some("BeforeInvoiceDate")`
    // / `Some("AfterPaymentDeadline")` records the operator's confirmed
    // out-of-range choice verbatim. The two calendar dates ride on the
    // allocated `ReadyInvoice` and are stamped by the same `with_invoice_dates`
    // builder so the audit row carries the full triple
    // (invoice_date + payment_deadline + delivery_date + override).
    delivery_date_override: Option<String>,
) -> Result<TxOutcome> {
    let tx = conn
        .transaction()
        .context("begin DuckDB transaction (billing + audit-ledger)")?;

    let now = OffsetDateTime::now_utc();
    let outcome =
        billing::allocate_in_tx(&tx, allocate_args, now).context("billing::allocate_in_tx")?;

    let (invoice, reservation, was_fresh) = match outcome {
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
        // Actor is the keychain-derived `from_local_cli` value built in
        // `run()` (PR-7-A closes F15). Canonical on-disk string per
        // ADR-0005 (PR-6.1 F8). Stable across crate versions; the
        // `Debug` derive that PR-5 used was not.
        let idem_str = idempotency_key.to_canonical_string();

        // Typed payloads serialized via `serde_json::to_vec` per
        // PR-6.1 F9. `format!`-built JSON would have to be hand-
        // escaped against quotes / backslashes / control chars /
        // non-ASCII — for the values PR-5 used it happened to be
        // safe, but PR-7's NAV verbatim-XML payloads would not be.
        let seq_payload = audit_payloads::InvoiceSequenceReservedPayload::from_outcome(
            &invoice,
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
        .context("audit_ledger::append_in_tx InvoiceSequenceReserved")?;

        // PR-18 / ADR-0031 §2 — record the operator's --out path on
        // the audit payload so the drain worker can submit without
        // a per-invocation path argument.
        //
        // PR-44γ / ADR-0037 — for non-HUF invoices the currency +
        // rate metadata are stamped onto the same payload (existing
        // EventKind reused per the brief's task #4; no F12 ritual).
        // For HUF the existing path is preserved (currency stamped
        // explicitly as "HUF"; rate fields all `None`).
        let draft_payload = if let Some(rate) = rate_metadata.as_ref() {
            audit_payloads::InvoiceDraftCreatedPayload::from_invoice_with_rate(
                &invoice,
                idempotency_key,
                Some(nav_xml_path),
                currency,
                rate,
            )
        } else {
            audit_payloads::InvoiceDraftCreatedPayload::from_invoice_with_xml_path(
                &invoice,
                idempotency_key,
                nav_xml_path,
            )
        }
        // PR-73 / ADR-0040 §addendum — stamp the bank-account snapshot
        // (resolved by the route handler or inherited from the base
        // for chain children). `None` is a no-op so the CLI path
        // continues to emit a snapshot-free payload.
        .with_bank_snapshot(bank_snapshot.as_ref())
        // PR-82 — stamp the buyer-facing notes onto the audit payload.
        // `with_notes` reads the invoice's per-line notes off
        // `invoice.lines[i].note` and persists them alongside the
        // operator-typed `invoice_note` so the operator-twin record
        // captures everything the buyer will see on the PDF.
        .with_notes(&invoice, invoice_note.as_deref())
        // PR-84 — stamp the three invoice-date fields onto the audit
        // payload. `payment_deadline` + `delivery_date` come off the
        // freshly-allocated `ReadyInvoice`; `delivery_date_override`
        // carries the operator's SPA-form comfort-zone discriminant
        // verbatim (None for in-range, Some("BeforeInvoiceDate") /
        // Some("AfterPaymentDeadline") for confirmed out-of-range).
        .with_invoice_dates(&invoice, delivery_date_override.as_deref());
        audit_ledger::append_in_tx(
            &tx,
            ledger_meta,
            EventKind::InvoiceDraftCreated,
            draft_payload.to_bytes(),
            actor,
            Some(idem_str),
        )
        .context("audit_ledger::append_in_tx InvoiceDraftCreated")?;
    } else {
        tracing::info!("replay path: no new audit entries written");
    }

    tx.commit()
        .context("commit DuckDB transaction (billing + audit-ledger)")?;
    Ok(TxOutcome { invoice, was_fresh })
}

// ──────────────────────────────────────────────────────────────────────
// Command construction
// ──────────────────────────────────────────────────────────────────────

fn build_command(input: &InvoiceInputJson, code: &SeriesCode) -> Result<IssueInvoiceCommand> {
    let lines = input
        .lines
        .iter()
        .map(|l| LineItem {
            description: l.description.clone(),
            quantity: l.quantity,
            unit_price: Huf(l.unit_price),
            vat_rate_basis_points: percent_to_basis_points(l.vat_rate_percent),
            // PR-82 — per-line buyer note threads from the wire body
            // through to `LineItem`. The NAV emitter does not consume
            // this field; the printed-PDF + SPA detail surfaces do.
            note: l.note.clone(),
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
// PR-44γ — MNB rate fetch + walk-back + round-half-even HUF conversion
// ──────────────────────────────────────────────────────────────────────

/// Fetch the MNB official mid-rate for `currency` on `supply_date`,
/// walking back per ADR-0037 §2.b up to [`MNB_WALKBACK_DAYS_CAP`]
/// days when MNB returns `NoRateForCurrency` (non-publication day —
/// weekend or holiday). Compute the round-half-even HUF-equivalent
/// of the invoice's gross total per ADR-0037 §1.c + §4 C11 / A137.
///
/// # Returns
///
/// `Ok(RateMetadata)` carrying the parsed rate, the literal source
/// identifier (`MNB_SOURCE` const = `"MNB"` per ADR-0037 §1.a), the
/// publication date MNB actually answered with (which may be < supply
/// date per the walk-back), and the rounded HUF total.
///
/// # Errors
///
/// - [`IssueRateError::NoRateAfterWalkback`] — walked back the full
///   cap without finding a publication; per ADR-0037 §4 invariant
///   C2 the loud-fail is mandatory, no silent fallback.
/// - [`IssueRateError::Mnb`] — any other [`MnbError`] (transport,
///   HTTP status, envelope parse, unsupported currency, etc.) —
///   loud-fail per C2.
/// - [`IssueRateError::MalformedDecimal`] — MNB's rate value did
///   not parse as a `rust_decimal::Decimal`. The mnb-rates crate
///   stores the value as a verbatim dot-decimal string (per A135);
///   parse failure here is a regression in MNB's response shape OR
///   in our normalizer.
/// - [`IssueRateError::HufOverflow`] — extreme operand combination
///   (loud-fail per CLAUDE.md rule 12 + §1.c arithmetic).
pub async fn fetch_and_stamp_rate<P: MnbRatesProvider + ?Sized>(
    provider: &P,
    currency: Currency,
    supply_date: time::Date,
    lines: &[LineItem],
) -> Result<RateMetadata> {
    let currency_iso = currency.iso_code().to_string();
    let supply_date_str = supply_date.to_string();

    // ── ADR-0037 §2.b walk-back. Up to MNB_WALKBACK_DAYS_CAP days back
    //    inclusive of the supply date (offset 0). The first MNB
    //    response with a rate wins; the publication date that MNB
    //    answered with may differ from `candidate` if MNB internally
    //    answered with its own walk-back (the mnb-rates crate
    //    returns whatever date MNB names in the response).
    for offset in 0..=MNB_WALKBACK_DAYS_CAP {
        let candidate = supply_date - time::Duration::days(offset);
        tracing::debug!(
            target: "issue_invoice",
            currency = %currency_iso,
            candidate = %candidate,
            offset,
            "MNB walk-back fetch attempt"
        );
        match provider.fetch_official_rate(currency, candidate).await {
            Ok(rate) => {
                return finalize_rate(&rate, lines);
            }
            Err(MnbError::NoRateForCurrency { .. }) => {
                continue;
            }
            Err(other) => {
                return Err(anyhow!(
                    "{} for {} on {}: {}",
                    ERR_MNB_FETCH_FAILED,
                    currency_iso,
                    candidate,
                    other
                ));
            }
        }
    }
    Err(anyhow!(
        "{} for {} in the {} days preceding (and including) {}; \
         the supply-fulfillment date may be in a multi-day non-publication window \
         OR before MNB began publishing this currency (ADR-0037 §2.b walk-back exhausted)",
        ERR_NO_RATE_AFTER_WALKBACK,
        currency_iso,
        MNB_WALKBACK_DAYS_CAP,
        supply_date_str
    ))
}

/// Parse the MNB rate value into a `Decimal`, sum the invoice's
/// gross total in EUR cents, compute the round-half-even HUF
/// equivalent, and assemble the [`RateMetadata`] stamp. Pulled out
/// of [`fetch_and_stamp_rate`] so the offset-loop body stays narrow
/// — the post-fetch arithmetic is uniform across happy + walked-back
/// branches.
fn finalize_rate(rate: &MnbRate, lines: &[LineItem]) -> Result<RateMetadata> {
    let rate_decimal = Decimal::from_str(&rate.value).map_err(|_| {
        anyhow!(
            "MNB rate value `{}` is not a parseable decimal (expected rust_decimal-compatible canonical form)",
            rate.value
        )
    })?;

    // Invoice gross total. The line `unit_price` is typed `Huf` today
    // (PR-44α preserved the field; PR-44γ does NOT lift it per the
    // session-51 brief's "Surgical posture"). For an EUR invoice the
    // operator-supplied `unitPrice` JSON values are interpreted as
    // EUR cents, stored in the `Huf` wrapper as an i64; the
    // round-half-even conversion below treats them as cents
    // explicitly. PR-44δ will lift this to a typed-EUR LineItem.
    let gross_total_minor_units: i64 = lines.iter().try_fold(0i64, |acc, line| -> Result<i64> {
        let line_gross = line
            .gross_total()
            .ok_or_else(|| anyhow!("line gross total overflowed i64"))?;
        acc.checked_add(line_gross.as_i64())
            .ok_or_else(|| anyhow!("invoice gross total overflowed i64"))
    })?;

    let huf_equivalent_total =
        huf_equivalent_round_half_even(gross_total_minor_units, &rate_decimal).ok_or_else(
            || {
                anyhow!(
                    "EUR amount {} cents × rate {} overflows i64 HUF equivalent (ADR-0037 §1.c)",
                    gross_total_minor_units,
                    rate.value
                )
            },
        )?;

    Ok(RateMetadata {
        rate: rate_decimal,
        source: MNB_SOURCE.to_string(),
        date: rate.date,
        huf_equivalent_total,
    })
}

/// PR-50 / session-70 — error sentinel substring that the SPA route
/// (`serve::handle_issue_invoice`) and the integration test
/// `serve_issue_route::issue_request_400_on_malformed_supplier_tax_number`
/// pattern-match on to detect a supplier-config loud-fail emerging
/// from `issue_from_parsed`. Hard-coded into the anyhow message
/// rather than threaded via a downcast because `issue_from_parsed`
/// is `Result<_, anyhow::Error>` and the route layer's defence in
/// depth runs BEFORE the issuance call anyway — the sentinel
/// surfaces only the rare CLI-bypass path where this guard fires.
pub const ERR_SUPPLIER_CONFIG_INVALID: &str = "supplier_config_invalid";

fn supplier_config_error_anyhow(e: SupplierConfigError) -> anyhow::Error {
    anyhow!("{ERR_SUPPLIER_CONFIG_INVALID}: {e}")
}
