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
//! - Audit-ledger writes from inside the allocator. ADR-0008 requires them
//!   to be transactional with the state change; that wiring lands in PR-5
//!   when the binary owns the DuckDB connection and can share it with the
//!   audit-ledger crate. Until then, audit entries for invoice events are
//!   written by the caller after the allocator commits — surfaced loudly
//!   in PR-5's commit message.
//! - Storno / modification chain (ADR-0009 §6).
//! - Startup reconciliation scan (ADR-0009 §3 "Startup reconciliation").

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod adapters;
pub mod api;
pub mod app;
pub mod domain;
pub mod ports;

pub use api::*;
