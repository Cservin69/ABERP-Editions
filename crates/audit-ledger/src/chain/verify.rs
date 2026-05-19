//! Full-chain integrity verification.
//!
//! [`verify_chain`] walks a sequence of [`crate::entry::Entry`] values and
//! checks four invariants:
//!
//! 1. Order — `seq` starts at 1 and advances by 1 contiguously.
//! 2. Chain link — `entry[N].prev_hash == entry[N-1].entry_hash`, or the
//!    tenant genesis hash for N=1.
//! 3. Per-entry integrity — `entry[N].entry_hash` equals the SHA-256 of
//!    the canonical-encoded entry minus `entry_hash` itself.
//! 4. Loud failure — the first divergence identifies the first tampered
//!    or out-of-order entry; the verifier does not continue past it.

use crate::chain::compute::compute_entry_hash;
use crate::chain::genesis::genesis_hash;
use crate::entry::{Entry, TenantId};
use crate::error::VerifyError;

/// Verify a sequence of entries against the per-tenant genesis hash.
///
/// Returns `Ok(count)` on success (number of entries walked) or a
/// [`VerifyError`] describing the first divergence. ADR-0007 §"Fail loud"
/// applies: a tampered chain returns the precise `seq` and reason, not a
/// generic "verification failed".
pub fn verify_chain<'a, I>(tenant: &TenantId, entries: I) -> Result<u64, VerifyError>
where
    I: IntoIterator<Item = &'a Entry>,
{
    let mut expected_seq: u64 = 1;
    let mut prev_hash = genesis_hash(tenant);
    let mut count: u64 = 0;

    for entry in entries {
        // 1. Order check — contiguous from seq=1 upward.
        if entry.seq.as_u64() != expected_seq {
            return Err(VerifyError::OutOfOrder {
                expected: expected_seq,
                found: entry.seq.as_u64(),
            });
        }

        // 2. Chain link check.
        if entry.prev_hash != prev_hash {
            return Err(VerifyError::ChainBroken {
                seq: entry.seq.as_u64(),
            });
        }

        // 3. Per-entry integrity.
        let recomputed = compute_entry_hash(entry);
        if recomputed != entry.entry_hash {
            return Err(VerifyError::TamperedAt {
                seq: entry.seq.as_u64(),
            });
        }

        // 4. Advance.
        prev_hash = entry.entry_hash;
        expected_seq = expected_seq
            .checked_add(1)
            .expect("audit-ledger sequence overflow during verify");
        count += 1;
    }

    Ok(count)
}
