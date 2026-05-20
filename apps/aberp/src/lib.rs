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
pub mod issue_invoice;
pub mod mark_abandoned;
pub mod nav_xml;
pub mod poll_ack;
pub mod retry_submission;
pub mod setup_nav_credentials;
pub mod submit_invoice;
