//! Trait definitions per ADR-0006 §"`ports/` ← trait definitions: storage,
//! clock, id, event-publisher, external-api".
//!
//! Sub-files:
//!
//! - [`storage`] [`BillingStore`] — persistence + atomic sequence allocator.
//! - [`clock`]   [`Clock`] — injectable wall clock per ADR-0007
//!   §"Operator-as-threat-actor": issue date is server-clock-only.

pub mod clock;
pub mod storage;
