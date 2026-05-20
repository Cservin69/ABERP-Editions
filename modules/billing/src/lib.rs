//! ABERP billing module — NAV invoice issuing (ADR-0009).
//!
//! Module shape per ADR-0006:
//!
//! - [`domain`]   pure types (no IO, no async, no DB types, no logging).
//! - [`ports`]    trait definitions (storage, clock).
//! - [`adapters`] concrete implementations (DuckDB, in-memory for tests).
//! - [`app`]      command handlers and orchestration.
//! - [`api`]      public surface — re-exported at the crate root.
//!
//! # PR-4 scope
//!
//! - Domain types: invoice + line items + customer reference; ULID-typed IDs
//!   per ADR-0005.
//! - Sequence allocator per ADR-0009 §3: atomic, gap-free, idempotent under
//!   retry. Only the `Never` reset policy is implemented here;
//!   `AnnualOnFiscalYear` lands when the first non-default series ships.
//! - Storage port + DuckDB and in-memory adapters (ADR-0006 §Conformance:
//!   "at least one in-memory adapter for every port").
//! - `IssueInvoiceCommand` handler that drives the allocator.
//!
//! # Out of PR-4 scope
//!
//! - NAV submission, ack polling, retry queue.
//! - Storno / modification chain (ADR-0009 §6).
//! - Startup reconciliation scan (ADR-0009 §3 "Startup reconciliation").
//!
//! # PR-6 addition: tx-aware allocator
//!
//! [`adapters::duckdb_store::allocate_in_tx`] is the free-function flavor
//! of the trait-method allocator. The binary in `apps/aberp` calls it
//! against a borrowed `duckdb::Transaction` so the audit-ledger appends
//! for the issuance ride the same transaction (ADR-0008 §Storage,
//! ADR-0009 §3 step 6). The trait method
//! [`adapters::duckdb_store::DuckDbBillingStore::allocate_and_insert`]
//! delegates to it for the non-coordinated callers (in-memory tests,
//! future single-module callers).

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod adapters;
pub mod api;
pub mod app;
pub mod domain;
pub mod ports;

pub use api::*;
