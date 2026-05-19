//! Typed error enums for the audit-ledger crate.
//!
//! Per ADR-0021 Part A item 2, library crates use `thiserror` for typed
//! errors. The binary's `anyhow::Result` boundary converts these on demand.
//! No `anyhow` import here — that would be a conformance failure.

use thiserror::Error;

/// Errors returned by [`crate::Ledger::append`] and the supporting open
/// path. Each variant names the failure source loudly per ADR-0007.
#[derive(Debug, Error)]
pub enum AppendError {
    /// DuckDB schema creation, query, or transaction commit failed.
    #[error("storage I/O error: {0}")]
    Storage(#[from] duckdb::Error),

    /// The tenant id supplied at open time was invalid (empty or contained
    /// a null byte, which is reserved for the genesis-hash separator).
    #[error("invalid tenant id (empty or contains a null byte)")]
    InvalidTenantId,

    /// An attempt to append produced a sequence that already exists. The
    /// `UNIQUE(seq)` index in DuckDB is the structural enforcement; this
    /// variant surfaces it loudly.
    #[error("sequence conflict at seq={seq}")]
    SequenceConflict { seq: u64 },

    /// A wall-clock formatter or parser failed. RFC3339 formatting of a
    /// valid `OffsetDateTime` cannot fail in practice, so this surfaces
    /// only if a stored row's `time_wall` text is corrupted.
    #[error("time format error: {0}")]
    TimeFormat(#[from] time::error::Format),

    /// A stored row's `time_wall` text could not be parsed back to an
    /// `OffsetDateTime`. Indicates DB corruption or schema drift.
    #[error("time parse error: {0}")]
    TimeParse(#[from] time::error::Parse),

    /// The `actor` column held JSON that could not be deserialized into
    /// [`crate::entry::Actor`]. Indicates schema drift or DB corruption.
    #[error("actor JSON deserialization error: {0}")]
    ActorJson(#[from] serde_json::Error),

    /// A stored row's `id` text was not a valid prefixed ULID
    /// (`aud_<26-char-Crockford>`) or its `tenant_id`/`hash` columns
    /// had the wrong byte length. Indicates DB corruption.
    #[error("invalid stored row at seq={seq}: {reason}")]
    CorruptRow { seq: u64, reason: &'static str },
}

/// Errors returned by [`crate::chain::verify_chain`]. Each variant names
/// the divergence point so an operator can locate the first bad entry.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum VerifyError {
    /// Entries arrived out of seq order, or the chain has a gap.
    /// `expected` is the next seq the verifier was waiting for;
    /// `found` is what it actually got.
    #[error("out of order: expected seq={expected}, found seq={found}")]
    OutOfOrder { expected: u64, found: u64 },

    /// `entry[seq].prev_hash` does not match the previous entry's
    /// `entry_hash` (or the tenant genesis hash, for seq=1). The chain
    /// link is broken at this entry.
    #[error("chain broken at seq={seq} (prev_hash mismatch)")]
    ChainBroken { seq: u64 },

    /// `entry[seq].entry_hash` does not match SHA-256 of the canonical
    /// encoding of the entry. The entry has been tampered with after
    /// it was written.
    #[error("tamper detected at seq={seq} (entry_hash mismatch)")]
    TamperedAt { seq: u64 },
}
