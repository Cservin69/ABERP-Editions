//! Command handlers and orchestration per ADR-0006 §"`app/`".
//!
//! Sub-files:
//!
//! - [`error`]          [`BillingError`] — thiserror enum for the module.
//! - [`issue_invoice`]  [`IssueInvoiceCommand`] + handler driving the
//!   ADR-0009 §3 atomic allocator.

pub mod error;
pub mod issue_invoice;
