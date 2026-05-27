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

pub mod audit_payloads;
pub mod audit_query;
pub mod binary_hash;
pub mod cli;
pub mod drain_pending_retries;
pub mod drain_submission_queue;
pub mod export_invoice_bundle;
pub mod invoice_bank_snapshot;
pub mod invoice_currency_metadata;
pub mod issue_invoice;
pub mod issue_modification;
pub mod issue_preflight;
pub mod issue_storno;
pub mod mark_abandoned;
pub mod mark_invoice_paid;
pub mod mnb_rates_provider;
pub mod nav_xml;
pub mod observe_receiver_confirmation;
pub mod partners;
pub mod poll_ack;
pub mod poll_annulment_ack;
pub mod print_invoice;
pub mod recover_from_nav;
pub mod request_technical_annulment;
pub mod retry_submission;
pub mod seller_banks;
pub mod serve;
pub mod setup_nav_credentials;
pub mod setup_seller_info;
pub mod submission_queue;
pub mod submit_annulment;
pub mod submit_invoice;
