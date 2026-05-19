//! Per-entry hash computation and the `next_*` helpers callers use to
//! build the next entry's `prev_hash` and `seq` fields before append.

use sha2::{Digest, Sha256};

use crate::canonical::canonical_bytes_for_hashing;
use crate::chain::genesis::genesis_hash;
use crate::entry::{Entry, EntryHash, Sequence, TenantId};

/// Compute an entry's `entry_hash` from its canonical CBOR bytes.
///
/// The entry's own `entry_hash` field is ignored by
/// [`canonical_bytes_for_hashing`], so passing in an entry whose
/// `entry_hash` is uninitialized (or stale) is correct: this function
/// produces the value that `entry_hash` should be set to.
pub fn compute_entry_hash(entry: &Entry) -> EntryHash {
    let bytes = canonical_bytes_for_hashing(entry);
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let hash: [u8; 32] = hasher.finalize().into();
    EntryHash::from_bytes(hash)
}

/// Convenience: compute the `prev_hash` field that the next entry to be
/// appended should carry. If no entries exist yet, this returns the
/// tenant genesis hash; otherwise it returns the most recent entry's
/// `entry_hash`.
pub fn next_prev_hash(tenant: &TenantId, head: Option<&Entry>) -> EntryHash {
    match head {
        None => genesis_hash(tenant),
        Some(entry) => entry.entry_hash,
    }
}

/// Convenience: compute the `seq` field that the next entry to be
/// appended should carry. Starts at 1, advances by 1.
pub fn next_seq(head: Option<&Entry>) -> Sequence {
    match head {
        None => Sequence::FIRST,
        Some(entry) => entry.seq.next(),
    }
}
