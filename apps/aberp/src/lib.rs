//! Library face of the ABERP binary — re-exports the internal modules so
//! integration tests under `apps/aberp/tests/` can drive the same
//! orchestration the `aberp` binary uses.
//!
//! # Why a library at the binary boundary
//!
//! Cargo does not let an integration test reach into `src/main.rs`'s
//! sibling modules. PR-7-B-3 needs an end-to-end conformance test
//! ("issue an invoice → submit it → assert transactionId persisted →
//! assert audit chain still verifies") that drives `submit_invoice::run`
//! directly. Splitting the binary crate into a thin `lib.rs` + a
//! `main.rs` that delegates is the standard Cargo workaround.
//!
//! The library exposes the modules at their existing paths so the
//! binary code (`main.rs`) and the integration tests share one set
//! of imports. Public surface is intentionally narrow: each module
//! is `pub` here only because the integration tests need it; nothing
//! is re-exported at the crate root because no other crate imports
//! `aberp`.

#![forbid(unsafe_code)]

pub mod ap_sync;
pub mod audit_payloads;
pub mod audit_query;
pub mod binary_hash;
pub mod branding_config;
pub mod build_profile;
pub mod catalogue_push;
pub mod cli;
pub mod drain_pending_retries;
pub mod drain_submission_queue;
pub mod email_invoice;
pub mod export_invoice_bundle;
pub mod first_launch;
pub mod incoming_invoices;
pub mod invoice_bank_snapshot;
pub mod invoice_currency_metadata;
// S236 / PR-230b — pre-allocation Draft state. New `invoice_draft`
// table + `BillingInvoiceSpawner` replaces `NoopInvoiceSpawner` from
// PR-230. Closes the Stage 3 → Stage 1 hand-off without burning a
// gap-free sequence slot per ADR-0009 §169.
pub mod invoice_draft;
pub mod issue_invoice;
pub mod issue_modification;
pub mod issue_preflight;
pub mod issue_storno;
pub mod mark_abandoned;
pub mod mark_invoice_paid;
// S229 / PR-225 — Stage 3 manufacturing-adapter framework boot wiring.
// Reads `ABERP_BARCODE_SCANNER_*` env vars and spawns the barcode
// scanner adapter + per-adapter ledger-writer task. Default-off.
pub mod mes_boot;
// S257 / PR-246 — `[[mes.adapters]]` seller.toml slot (7th preservation
// section) + operator-managed adapter lifecycle (Settings → Adapters).
pub mod mes_adapters_config;
pub mod mes_manager;
pub mod mnb_rates_provider;
pub mod nav_xml;
pub mod notes_history;
pub mod numbering;
pub mod observe_receiver_confirmation;
pub mod partners;
pub mod poll_ack;
pub mod poll_annulment_ack;
pub mod print_invoice;
pub mod products;
pub mod quote_intake_config;
pub mod quote_intake_credentials;
pub mod quote_intake_query;
// S255 / PR-244 — operator-clicked "Create draft invoice" on a staged
// quote_intake_log row. Mints an `invoice_draft` row with
// `source_quote_id` set + emits `InvoicePickedUpFromQuote`.
pub mod quote_pickup;
// S272 / PR-261 — DEAL saga (ADR-0067). Operator clicks DEAL on a quote
// intake row → single-tx mint of SO/WO placeholder ids + 3 audit
// entries. Replay-protected via a CAS on `deal_issued_at IS NULL`.
// Enforces EVE addendum 2 (REFRESH-typed ack when `stock_alert`) and
// addendum 3 (BIG/RED/single-use DEAL token, validated server-side).
pub mod quote_deal;
// S271 / PR-260 — pure `stock_alert` recompute (EVE addendum 2). Used by
// the SPA Quotes list route to mark accepted quotes whose material has
// downgraded since acceptance. Sticky: only operator REFRESH (S272+)
// untriggers an alert.
pub mod quote_stock_alert;
pub mod quoting_materials;
// S267 / PR-256 — four tunable tables feeding the future
// `aberp-quote-engine`: complexity rules, tolerance multipliers, the
// global parameters singleton, and per-material × stock-status price
// adjustments. None of these push to the storefront — they are
// quoting-engine internals.
pub mod quoting_tunables;
pub mod recover_from_nav;
pub mod reports;
pub mod request_technical_annulment;
pub mod restore_from_nav_extract;
pub mod restore_from_nav_outgoing;
pub mod retry_submission;
pub mod secrets_cache;
pub mod seller_banks;
pub mod seller_toml_backup;
pub mod serve;
pub mod setup_nav_credentials;
pub mod setup_seller_info;
pub mod shutdown;
pub mod smtp_config;
pub mod smtp_credentials;
pub mod submission_queue;
pub mod submit_annulment;
pub mod submit_invoice;
pub mod upgrade_snapshot;
