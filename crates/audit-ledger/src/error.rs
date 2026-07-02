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

    /// An insert affected a row count other than 1. Historically this
    /// surfaced the inline `UNIQUE(seq)` index rejecting a duplicate;
    /// since S341 dropped that ART index (duckdb#23046 / S332), it is a
    /// defensive catch for an unexpected affected-row count. Duplicate
    /// `seq` is now prevented in-process by `AUDIT_APPEND_LOCK` and
    /// detected globally by the hash chain (`verify_chain`).
    #[error("sequence conflict at seq={seq}")]
    SequenceConflict { seq: u64 },

    /// The transparent boot migration that drops the legacy `UNIQUE`-ART
    /// schema (S341) refused or failed — e.g. the dumped rows did not
    /// verify (data tampering, not index corruption), or the rebuilt
    /// table failed its post-migration chain check. Loud-fail per
    /// CLAUDE.md rule 12: a migration that cannot prove integrity must
    /// never silently proceed.
    #[error("audit-ledger schema migration failed: {0}")]
    Migration(String),

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

    /// PR-17 / ADR-0030 — the audit-ledger mirror file `<db>.audit.log`
    /// is malformed: a partial trailing line (no newline terminator),
    /// non-ascending seqs, duplicate seqs, or a line that fails JSON
    /// decoding. The DB-committed entry is not rolled back; the
    /// operator's recovery is to inspect the mirror, repair it, and
    /// re-run (the next `sync_mirror` call catches up).
    #[error("audit-ledger mirror file is malformed: {reason}")]
    MirrorCorrupt { reason: String },
    /// PR-17 / ADR-0030 — the audit-ledger mirror file disagrees with
    /// the DB at the given seq (`entry_hash` mismatch). Surfaces both
    /// "the DB was tampered with after the last mirror append" and
    /// "the mirror was tampered with"; the operator's recovery is to
    /// investigate before re-running. Per ADR-0030 §3 the DB-committed
    /// entry is NOT rolled back; per CLAUDE.md rule 12 the next append
    /// is refused until the operator investigates.
    #[error(
        "audit-ledger mirror diverges from DB at seq={seq}: \
         {reason}"
    )]
    MirrorDivergent { seq: u64, reason: String },
    /// PR-17 / ADR-0030 — the mirror file's I/O surface failed
    /// (open, read, write, fsync, or advisory lock). Wraps the
    /// `std::io::Error`. The DB-committed entry is not rolled back;
    /// the operator's recovery is to investigate disk space /
    /// permissions / FS readiness and re-run.
    #[error("audit-ledger mirror I/O error: {0}")]
    MirrorIo(#[source] std::io::Error),

    /// ADR-0093 chunk 3 / ADR-0082 reconcile safety — at boot the audit
    /// mirror (`<db>.audit.log`) was found AHEAD of the DB (its max seq is
    /// greater than the DB's). This is the fingerprint of a torn-write /
    /// lost-commit on the DB side (the 2026-06-22 corruption class), or a
    /// dev DB-nuke. The editions tree REFUSES to silently auto-truncate the
    /// ahead mirror (that would destroy the only surviving record of what
    /// the DB lost); the ahead mirror is first PRESERVED to a side file and
    /// boot surfaces this so a human investigates before anything is
    /// rebuilt. Recovery: inspect `preserved`, restore the DB from the
    /// newest valid snapshot if a commit was truly lost, or (for an
    /// intentional dev-nuke) move the stale mirror aside and re-run.
    #[error(
        "audit-ledger mirror is AHEAD of the DB (mirror seq {mirror_max_seq} > DB seq \
         {db_max_seq}); refusing to auto-truncate — the ahead mirror was preserved to \
         {preserved}. Investigate (possible lost DB commit) before re-running. \
         Magyarul: a napló-tükör előrébb tart a DB-nél; nem csonkítom, először vizsgáld ki."
    )]
    MirrorAheadOfDb {
        mirror_max_seq: u64,
        db_max_seq: u64,
        preserved: String,
    },

    /// ADR-0098 R1 (Fable-5 findings D + E) — the audit-ledger mirror is
    /// corrupt in a way the unified torn-tail policy will NOT auto-heal:
    /// either corruption DEEPER than a single torn trailing line (a
    /// break/gap/JSON/hash-chain mismatch not at the final line), or a head
    /// `entry_hash` that DIVERGES from the DB at equal length. In every case
    /// the original mirror was PRESERVED byte-for-byte to `preserved` BEFORE
    /// refusing. The editions tree NEVER rebuilds-from-DB here (that could
    /// destroy a prefix the DB lost via a dropped WAL tail) and the operator
    /// must NEVER hand-edit the JSONL. Recovery: inspect `preserved`, then run
    /// `aberp recover`.
    #[error(
        "audit-ledger mirror is unrecoverable ({reason}); the original was preserved to \
         {preserved}. Recover with `aberp recover --db <db> --tenant <tenant> --store <store>`; \
         do NOT hand-edit the mirror JSONL. Magyarul: a napló-tükör sérült; az eredetit \
         félretettem, ne szerkeszd kézzel."
    )]
    MirrorCorruptPreserved { preserved: String, reason: String },

    /// S441 / ADR-0087 — a non-network timestamp-authority failure while
    /// taking an anchor. A *network* TSA failure NEVER reaches this
    /// variant: it queues a `pending` anchor instead (`take_anchor` never
    /// blocks the audit chain on the TSA). This surfaces only a genuine
    /// authority rejection.
    #[error("timestamp authority error: {0}")]
    Tsa(String),

    /// S441 / ADR-0087 — an `audit_ledger_anchors` insert affected an
    /// unexpected row count.
    #[error("anchor write error: {0}")]
    Anchor(String),

    /// S441 / ADR-0087 — minting a session signing key from the OS CSPRNG
    /// failed at session open.
    #[error("session crypto error: {0}")]
    Crypto(String),
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

    /// S441 / ADR-0087 — a signed entry's `event_sig` did not verify
    /// against its `session_pubkey` over the signing preimage. The entry
    /// was altered, or its signature forged, after signing.
    #[error("invalid session signature at seq={seq}")]
    SignatureInvalid { seq: u64 },

    /// S441 / ADR-0087 — the anti-strip membership rule: an entry whose
    /// `session_id` belongs to an anchored session carries no `event_sig`.
    /// A stripped signature inside a signed session range is a failure.
    #[error("missing signature at seq={seq} inside an anchored session")]
    SignatureMissingInSignedSession { seq: u64 },

    /// S441 / ADR-0087 — an anchor's qualified-timestamp token did not
    /// verify against its reconstructed payload (the chain head it claims
    /// to commit to was altered, or the token was forged).
    #[error("invalid timestamp anchor {anchor_id}")]
    AnchorTampered { anchor_id: String },
}
