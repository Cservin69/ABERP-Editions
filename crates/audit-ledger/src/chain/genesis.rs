//! Per-tenant genesis hash for the audit-ledger chain.
//!
//! ADR-0008 §"Entry shape" requires that the first entry's `prev_hash` be
//! "a tenant-specific genesis hash". It does not specify the construction.
//! This file is that construction; it is cornerstone-class for an
//! existing chain (changing it invalidates every historical chain) and
//! is subject to the same caution as [`crate::canonical`].

use sha2::{Digest, Sha256};

use crate::entry::{EntryHash, TenantId};

/// Domain-separated, versioned, tenant-specific genesis hash.
///
/// Construction: `SHA-256(b"aberp-audit-ledger-v1-genesis\0" || tenant.as_bytes())`.
///
/// - The leading magic string identifies this as an audit-ledger genesis,
///   preventing cross-protocol confusion if the same SHA-256 primitive
///   is reused elsewhere with the same tenant id as input.
/// - The trailing `\0` is the domain separator: [`TenantId::new`] rejects
///   tenant strings containing null bytes, so the prefix cannot collide
///   with a tenant whose name happens to encode the prefix.
/// - The `-v1-` token reserves room for a future supersede: a `v2` chain
///   would start fresh under a different genesis without colliding with
///   any `v1` history.
pub fn genesis_hash(tenant: &TenantId) -> EntryHash {
    const MAGIC: &[u8] = b"aberp-audit-ledger-v1-genesis\0";
    let mut hasher = Sha256::new();
    hasher.update(MAGIC);
    hasher.update(tenant.as_bytes());
    let bytes: [u8; 32] = hasher.finalize().into();
    EntryHash::from_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_per_tenant() {
        let t1 = TenantId::new("tenant-a").unwrap();
        let t2 = TenantId::new("tenant-a").unwrap();
        assert_eq!(genesis_hash(&t1), genesis_hash(&t2));
    }

    #[test]
    fn differs_across_tenants() {
        let t1 = TenantId::new("tenant-a").unwrap();
        let t2 = TenantId::new("tenant-b").unwrap();
        assert_ne!(genesis_hash(&t1), genesis_hash(&t2));
    }
}
