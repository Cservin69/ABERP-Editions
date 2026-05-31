//! ABERP quote-intake crate — pulls approved quotes from a sister
//! storefront (ABERP-site) and stages them as **pending intake rows**
//! the operator picks up later via the SPA (S211).
//!
//! S210 / PR-204 — first backend module of the **2.0 cutover** strand
//! (Stage 2 per ADR-0056 + [[aberp-versioning-policy]]).
//!
//! # Conservative scope cap — DAEMON DOES NOT TOUCH `invoice` TABLE
//!
//! The ABERP `invoice` table is the canonical regulated outgoing-invoice
//! surface — every row is a sequence-burned, audit-chained, NAV-bound
//! record (ADR-0009 §2-§3). Auto-creating sequence-burned invoices
//! from background polled data would be irreversible and would couple
//! a remote ABERP-site outage to the regulated invoice surface in a
//! way nothing else in this codebase does.
//!
//! The conservative posture per CLAUDE.md rule 2 +
//! [[trust-code-not-operator]]: the daemon stages quotes in a
//! purpose-built `quote_intake_log` DuckDB table, along with a
//! **pre-prepared `DraftInvoice` JSON** the operator can adopt
//! verbatim. S211 surfaces the pending intake queue in the SPA, and
//! the operator-clicked pickup routes through the normal
//! `issue-invoice` pipeline — sequence burn, audit chain, NAV
//! submission stay operator-gated.
//!
//! # Surface
//!
//! - [`QuoteIntakeConfig`] — env-driven config.
//! - [`QuoteIntakeService`] — owns the HTTP client, the audit-ledger
//!   handle, and the DuckDB path.
//! - [`service::PollSummary`] — per-cycle outcome.

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod audit;
pub mod config;
pub mod error;
pub mod log_table;
pub mod mapping;
pub mod payload;
pub mod service;
pub mod transport;

pub use audit::{audit_kind_string, write_poll_audit_entry, QuoteIntakePollPayload};
pub use config::QuoteIntakeConfig;
pub use error::QuoteIntakeError;
pub use mapping::{quote_to_draft_invoice, MappingOutcome, PreparedDraft};
pub use payload::{Quote, QuoteContact, QuoteFile, QuoteRequest};
pub use service::{PollSummary, QuoteIntakeService};
pub use transport::QuoteIntakeTransport;
