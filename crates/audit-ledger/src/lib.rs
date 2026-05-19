//! ABERP audit-ledger crate.
//!
//! Tamper-evident, hash-chained, append-only audit ledger per ADR-0008.
//!
//! # Design references
//!
//! - **ADR-0008** — entry shape (12 fields), hash chain
//!   (`entry_hash[N] = SHA-256(canonical(entry[N] with prev_hash = entry_hash[N-1]))`),
//!   append-only API surface, external attestation (deferred to a later PR).
//! - **ADR-0019** — storage strategy: relational source-of-truth, no foreign
//!   keys. Audit ledger lives in its own DuckDB table inside the tenant DB.
//! - **ADR-0021** — pre-code consolidated baseline. The crates this module
//!   uses (`duckdb`, `ciborium`, `sha2`, `time`, `ulid`, `thiserror`) are
//!   pinned in workspace `Cargo.toml`.
//!
//! # What this crate does and does not do (PR-3 scope)
//!
//! Per `_handoffs/05-session-5-code-can-start.md`:
//!
//! - `Ledger::append` API ✅
//! - Canonical CBOR encoder via `ciborium` ✅ (one place per ADR-0021 §12)
//! - SHA-256 hash chain via `sha2` ✅
//! - DuckDB table with unique-`seq` index ✅
//! - Conformance test for chain verification ✅
//!
//! Deferred to later PRs (not scoped to PR-3):
//!
//! - Append-only mirror file `<tenant>.audit.log` (ADR-0008 §"Storage").
//! - External attestation checkpoints (ADR-0008 §"External attestation").
//! - Schema-versioned payload kinds beyond the test variant
//!   (ADR-0008 §"What goes in the ledger" — landing with PR-4).
//! - Export-bundle generation (ADR-0008 §"Export").

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

mod canonical;
mod chain;
mod entry;
mod error;
mod storage;

pub use entry::{Actor, BinaryHash, Entry, EntryHash, EntryId, EventKind, Sequence, TenantId};
pub use error::{AppendError, VerifyError};
pub use storage::{Ledger, LedgerVerifyError};
