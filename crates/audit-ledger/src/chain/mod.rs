//! Hash-chain construction and verification per ADR-0008.
//!
//! The chain has three pieces, one per sub-file:
//!
//! - [`genesis`] — tenant-specific genesis hash (`prev_hash` for entry 1).
//! - [`compute`] — `compute_entry_hash` and the `next_*` helpers.
//! - [`verify`] — full-chain integrity verification.
//!
//! Callers inside this crate reach the items via the long path
//! (`crate::chain::compute::compute_entry_hash` etc.). No `pub use`
//! short-path re-exports here: those would be unused inside the crate
//! (rustc warns), and external consumers do not need direct access to
//! the chain primitives — [`crate::Ledger`] is the public surface.

pub mod compute;
pub mod genesis;
pub mod verify;
