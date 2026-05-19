//! Audit-ledger entry types per ADR-0008 §"Entry shape".
//!
//! The 12 fields ADR-0008 enumerates live across this module:
//!
//! - The [`Entry`] struct itself is here.
//! - Value types (newtypes for ids, hashes, sequence) live in [`ids`].
//! - The [`Actor`] type lives in [`actor`].
//! - The [`EventKind`] enum lives in [`event_kind`].
//!
//! Re-exports below provide a flat public surface so callers can write
//! `use crate::Entry` rather than `use crate::entry::Entry`.

use time::OffsetDateTime;

pub mod actor;
pub mod event_kind;
pub mod ids;

pub use actor::Actor;
pub use event_kind::EventKind;
pub use ids::{BinaryHash, EntryHash, EntryId, Sequence, TenantId};

/// A fully-formed audit-ledger entry, including its computed `entry_hash`.
///
/// The order of fields here is the declaration order. The canonical CBOR
/// encoding ([`crate::canonical`]) re-orders fields per RFC 8949 §4.2.1
/// (length-first, then lexicographic on bytes). The declaration order in
/// this struct is irrelevant to the hash; only the canonical encoder's
/// fixed order matters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub id: EntryId,
    pub seq: Sequence,
    pub prev_hash: EntryHash,
    pub time_wall: OffsetDateTime,
    pub time_mono: u64,
    pub actor: Actor,
    pub binary_hash: BinaryHash,
    pub tenant_id: TenantId,
    pub kind: EventKind,
    pub payload: Vec<u8>,
    pub idempotency_key: Option<String>,
    pub entry_hash: EntryHash,
}
