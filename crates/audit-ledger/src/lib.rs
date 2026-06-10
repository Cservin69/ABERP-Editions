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
mod mirror;
mod storage;

pub use entry::{Actor, BinaryHash, Entry, EntryHash, EntryId, EventKind, Sequence, TenantId};
pub use error::{AppendError, VerifyError};
pub use mirror::{
    ensure_consistent_with_db, mirror_path_for, read_mirror_entries, sync_mirror, MirrorEntry,
    RecoveryAction,
};
pub use storage::{
    append_in_tx, append_reopen, ensure_schema, recent_entries, Ledger, LedgerMeta,
    LedgerVerifyError,
};

// PR-22 / ADR-0035 §8 — additive `pub use` re-exports of the chain
// primitives that `aberp-verify` needs to re-verify a per-invoice
// export bundle from its own bytes alone. Both items live in the
// private `chain` module; the re-exports here are the only new
// public surface PR-22 adds.
//
// `compute_entry_hash` re-runs the canonical CBOR encoder + SHA-256
// over an [`Entry`] (excluding the `entry_hash` field itself); the
// verifier compares the result against the entry's claimed
// `entry_hash` to catch per-entry tampering. `genesis_hash` is the
// chain anchor for entries with `seq == 1`; the verifier asserts
// the first-entry `prev_hash` matches the genesis derived from the
// manifest's tenant id.
//
// No behaviour change. No new EventKind variant. No F12 four-edit
// ritual firing. ADR-0021 §A12's "one place for the canonical
// encoder" discipline is preserved — the verifier reuses this
// implementation rather than copying it.
pub use chain::compute::compute_entry_hash;
pub use chain::genesis::genesis_hash;
