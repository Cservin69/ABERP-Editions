//! Canonical CBOR encoding for the audit-ledger hash chain.
//!
//! Per ADR-0021 §12: "the canonical encoding is CBOR per RFC 8949 §4.2.1,
//! produced by `ciborium`'s deterministic-encoding mode, and the encoder
//! function lives in **one place inside the audit-ledger crate** (not at
//! call sites)."
//!
//! This file is that one place. No other module in the workspace should
//! re-implement `Entry → bytes`; a conformance check will enforce that as
//! the workspace grows.
//!
//! # Determinism contract
//!
//! Two semantically-identical [`Entry`]s must produce identical bytes from
//! [`canonical_bytes_for_hashing`]. The contract is enforced by:
//!
//! 1. Fixed field order in this file, sorted per RFC 8949 §4.2.1 (length
//!    of the encoded key first, then lexicographic on the encoded key
//!    bytes). For text keys, the encoded length is the UTF-8 byte length.
//! 2. Explicit construction of a `ciborium::Value::Map` with the fields in
//!    that fixed order. We do **not** rely on `BTreeMap` (which sorts only
//!    alphabetically) or on `serde` derive (which uses declaration order).
//! 3. Exclusion of the `entry_hash` field from the encoded map per
//!    ADR-0008 §"Entry shape": "entry_hash — SHA-256 over the
//!    canonical-serialized entry minus this field".
//!
//! Any change to this function is a chain-breaking change for every
//! existing ledger: the hashes computed before the change will not match
//! the hashes computed after. Treat this file as cornerstone-class —
//! changes require a superseding ADR per CLAUDE.md rule 7 (surface
//! conflicts, don't average them).

use ciborium::value::Integer;
use ciborium::Value;

use crate::entry::Entry;

/// Produce the canonical CBOR byte string for an [`Entry`], excluding
/// the `entry_hash` field. This is the byte input to SHA-256 when
/// computing the entry's own `entry_hash` and when verifying the chain.
///
/// # Field order (RFC 8949 §4.2.1)
///
/// Sorted by (UTF-8-byte-length-of-key, then lexicographic-on-key-bytes):
///
/// | # | Key (length)            |
/// |---|-------------------------|
/// | 1 | `id`              (2)   |
/// | 2 | `seq`             (3)   |
/// | 3 | `kind`            (4)   |
/// | 4 | `actor`           (5)   |
/// | 5 | `payload`         (7)   |
/// | 6 | `prev_hash`       (9)   |
/// | 7 | `tenant_id`       (9)   |
/// | 8 | `time_mono`       (9)   |
/// | 9 | `time_wall`       (9)   |
/// |10 | `binary_hash`    (11)   |
/// |11 | `idempotency_key`(15)   |
///
/// Within the length-9 group: `prev_hash` < `tenant_id` < `time_mono` <
/// `time_wall` (lexicographic on bytes).
pub(crate) fn canonical_bytes_for_hashing(entry: &Entry) -> Vec<u8> {
    let map = Value::Map(vec![
        // 1. id — prefixed string form per ADR-0005 ("aud_<ULID>").
        (txt("id"), Value::Text(entry.id.to_prefixed_string())),
        // 2. seq — CBOR integer (u64 fits the Integer range).
        (
            txt("seq"),
            Value::Integer(u64_to_integer(entry.seq.as_u64())),
        ),
        // 3. kind — the typed-kind discriminant as a string. Schema version
        //    is implicit in the kind name; bumping the schema renames the
        //    kind and the old kind remains valid for historical entries.
        (txt("kind"), Value::Text(entry.kind.as_str().to_string())),
        // 4. actor — encoded as a nested CBOR map with its own
        //    field order (also length-then-lex). See `encode_actor` below.
        (txt("actor"), encode_actor(&entry.actor)),
        // 5. payload — opaque byte string supplied by the caller. The
        //    caller's responsibility to choose deterministic bytes.
        (txt("payload"), Value::Bytes(entry.payload.clone())),
        // 6. prev_hash — 32 raw SHA-256 bytes.
        (
            txt("prev_hash"),
            Value::Bytes(entry.prev_hash.as_bytes().to_vec()),
        ),
        // 7. tenant_id — UTF-8 string.
        (
            txt("tenant_id"),
            Value::Text(entry.tenant_id.as_str().to_string()),
        ),
        // 8. time_mono — u64 nanoseconds since process start.
        (
            txt("time_mono"),
            Value::Integer(u64_to_integer(entry.time_mono)),
        ),
        // 9. time_wall — RFC3339 with offset, per ADR-0008.
        (
            txt("time_wall"),
            Value::Text(format_rfc3339(entry.time_wall)),
        ),
        //10. binary_hash — 32 raw SHA-256 bytes of the producing binary.
        (
            txt("binary_hash"),
            Value::Bytes(entry.binary_hash.as_bytes().to_vec()),
        ),
        //11. idempotency_key — optional UTF-8 string; CBOR null when absent.
        (
            txt("idempotency_key"),
            match &entry.idempotency_key {
                Some(s) => Value::Text(s.clone()),
                None => Value::Null,
            },
        ),
    ]);

    let mut bytes = Vec::new();
    ciborium::into_writer(&map, &mut bytes)
        .expect("CBOR encoding of a fixed-shape Value::Map cannot fail in memory");
    bytes
}

/// Encode the actor as a CBOR map with RFC 8949 §4.2.1-sorted keys.
///
/// Actor's fields are `capabilities` (12), `session_id` (10), `user_id` (7).
/// Sorted: user_id (7) < session_id (10) < capabilities (12).
fn encode_actor(actor: &crate::entry::Actor) -> Value {
    // `capabilities` is a `BTreeSet<String>` — already sorted; we encode it
    // as a CBOR array in that sorted order, which is deterministic.
    let capabilities_array = Value::Array(
        actor
            .capabilities
            .iter()
            .map(|c| Value::Text(c.clone()))
            .collect(),
    );

    Value::Map(vec![
        (txt("user_id"), Value::Text(actor.user_id.clone())),
        (txt("session_id"), Value::Text(actor.session_id.clone())),
        (txt("capabilities"), capabilities_array),
    ])
}

#[inline]
fn txt(s: &'static str) -> Value {
    Value::Text(s.to_string())
}

#[inline]
fn u64_to_integer(n: u64) -> Integer {
    // `ciborium::value::Integer` accepts values in the full u64 range via
    // `From<u64>`; the conversion cannot fail.
    Integer::from(n)
}

/// Format an `OffsetDateTime` as RFC 3339 with offset, e.g.
/// `2026-05-19T12:34:56.123456789Z`. Stable across process restarts because
/// the format is fully specified.
fn format_rfc3339(t: time::OffsetDateTime) -> String {
    // `time::format_description::well_known::Rfc3339` is stable across
    // versions of the `time` crate. We pin the formatter so future minor
    // bumps of `time` cannot drift the output silently.
    t.format(&time::format_description::well_known::Rfc3339)
        .expect("RFC3339 formatting of a valid OffsetDateTime cannot fail")
}

#[cfg(test)]
mod tests {
    //! Unit tests verify the determinism contract of this encoder. The
    //! end-to-end chain conformance test lives in
    //! `tests/chain_conformance.rs`.

    use super::*;
    use crate::entry::{Actor, BinaryHash, EntryHash, EntryId, EventKind, Sequence, TenantId};

    fn fixture() -> Entry {
        Entry {
            id: EntryId::new(),
            seq: Sequence::FIRST,
            prev_hash: EntryHash::from_bytes([0u8; 32]),
            time_wall: time::OffsetDateTime::UNIX_EPOCH,
            time_mono: 0,
            actor: Actor::test_only(),
            binary_hash: BinaryHash::from_bytes([0u8; 32]),
            tenant_id: TenantId::new("t-1").unwrap(),
            kind: EventKind::Test,
            payload: b"payload-bytes".to_vec(),
            idempotency_key: Some("idem-1".to_string()),
            entry_hash: EntryHash::from_bytes([0u8; 32]),
        }
    }

    #[test]
    fn encoder_is_deterministic_for_identical_entries() {
        let e = fixture();
        let a = canonical_bytes_for_hashing(&e);
        let b = canonical_bytes_for_hashing(&e);
        assert_eq!(a, b, "two encodings of the same entry must match");
    }

    #[test]
    fn encoder_changes_when_payload_changes() {
        let mut a = fixture();
        let mut b = fixture();
        // Force the same id so only payload differs.
        b.id = a.id;
        b.payload = b"different-bytes".to_vec();
        assert_ne!(
            canonical_bytes_for_hashing(&a),
            canonical_bytes_for_hashing(&b)
        );
        // Sanity — also assert the entry_hash mutation does NOT change the
        // canonical bytes (because entry_hash is excluded from the hash).
        b.payload = a.payload.clone();
        a.entry_hash = EntryHash::from_bytes([1u8; 32]);
        b.entry_hash = EntryHash::from_bytes([2u8; 32]);
        assert_eq!(
            canonical_bytes_for_hashing(&a),
            canonical_bytes_for_hashing(&b)
        );
    }
}
