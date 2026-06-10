//! [`Ledger`] — the public storage adapter for the audit ledger.
//!
//! Owns a [`duckdb::Connection`] and offers the four operations ADR-0008
//! enumerates as the legitimate write/read surface:
//!
//! - [`Ledger::open`] / [`Ledger::open_in_memory`] — create or attach the
//!   `audit_ledger` table.
//! - [`Ledger::append`] — append-only write path. The only write API; per
//!   ADR-0008 there is no update or delete API.
//! - [`Ledger::verify_chain`] — full-chain integrity check.
//! - [`Ledger::entries`] — read all entries in seq order (used by tests
//!   and by the export-bundle path in a later PR).
//!
//! `Ledger::open` accepts a `tenant_id` and a `binary_hash`. Multi-tenant
//! separation is at the DuckDB-file level per ADR-0002; one `Ledger`
//! instance == one tenant's chain.
//!
//! # Cross-crate transactional appends (PR-6)
//!
//! [`Ledger::append`] above opens its own DuckDB transaction. For the
//! binary path where the same transaction must also cover billing-state
//! writes (ADR-0008 §Storage: "Entries are written in the same
//! transaction as the state change they describe"), this module exposes
//! [`ensure_schema`] and [`append_in_tx`] as free functions. The binary
//! (`apps/aberp/src/issue_invoice.rs`) owns the `Connection`, opens one
//! `Transaction`, calls [`crate::storage::ensure_schema`] up-front,
//! drives both the billing allocator and [`append_in_tx`] inside it, and
//! commits once. A panic or `Err(_)` between those calls and `commit`
//! rolls back both halves cleanly; conformance tests in
//! `apps/aberp/tests/rollback_conformance.rs` exercise both rollback
//! paths.
//!
//! The `Ledger::append` trait-style wrapper delegates to
//! [`append_in_tx`] so there is one body of insert logic, not two.

pub mod schema;

use std::path::Path;
use std::time::Instant;

use duckdb::{params, Connection};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use ulid::Ulid;

use crate::chain::compute::{compute_entry_hash, next_prev_hash, next_seq};
use crate::chain::verify::verify_chain;
use crate::entry::{Actor, BinaryHash, Entry, EntryHash, EntryId, EventKind, Sequence, TenantId};
use crate::error::{AppendError, VerifyError};

/// Per-tenant invariants the append path needs but the borrowed
/// [`duckdb::Transaction`] cannot supply on its own. Constructed once
/// per process by the binary (or once per `Ledger` by the trait-style
/// wrapper) and threaded into [`append_in_tx`] as `&LedgerMeta`.
///
/// `process_start` is captured at construction and never updated, per
/// ADR-0008 §"Adversarial review" — `time_mono` resets across
/// processes by design.
#[derive(Debug, Clone)]
pub struct LedgerMeta {
    tenant_id: TenantId,
    binary_hash: BinaryHash,
    process_start: Instant,
}

impl LedgerMeta {
    /// Build a `LedgerMeta` and anchor `time_mono` to "now". One call
    /// per process is the expected pattern; the binary builds it once
    /// at startup and re-uses it for every append.
    pub fn new(tenant_id: TenantId, binary_hash: BinaryHash) -> Self {
        Self {
            tenant_id,
            binary_hash,
            process_start: Instant::now(),
        }
    }

    pub fn tenant_id(&self) -> &TenantId {
        &self.tenant_id
    }
}

/// Append-only tamper-evident audit ledger backed by DuckDB.
#[derive(Debug)]
pub struct Ledger {
    conn: Connection,
    meta: LedgerMeta,
}

impl Ledger {
    /// Open or create a `Ledger` backed by a DuckDB file on disk. The
    /// `audit_ledger` table is created via `CREATE TABLE IF NOT EXISTS`,
    /// so calling this against an existing tenant DB is non-destructive.
    pub fn open(
        path: impl AsRef<Path>,
        tenant_id: TenantId,
        binary_hash: BinaryHash,
    ) -> Result<Self, AppendError> {
        let conn = Connection::open(path)?;
        Self::initialise(conn, tenant_id, binary_hash)
    }

    /// In-memory DuckDB ledger for tests. Backed by `:memory:`.
    pub fn open_in_memory(
        tenant_id: TenantId,
        binary_hash: BinaryHash,
    ) -> Result<Self, AppendError> {
        let conn = Connection::open_in_memory()?;
        Self::initialise(conn, tenant_id, binary_hash)
    }

    /// Open an existing `Ledger` file in READ-ONLY mode. Unlike
    /// [`Ledger::open`] this does NOT run `ensure_schema` (a read-only
    /// connection cannot `CREATE`), so the `audit_ledger` table MUST
    /// already exist — the caller gets a loud DuckDB error on the first
    /// query otherwise.
    ///
    /// Added for the S341 ART-rebuild path: the row dump + chain verify
    /// must not write a single byte (so `--dry-run` is a guaranteed
    /// no-op and leaves the file mtime untouched). The read path is also
    /// safe against the corrupt ART — the S332 crash is on INSERT, not
    /// SELECT.
    pub fn open_read_only(
        path: impl AsRef<Path>,
        tenant_id: TenantId,
        binary_hash: BinaryHash,
    ) -> Result<Self, AppendError> {
        let config = duckdb::Config::default().access_mode(duckdb::AccessMode::ReadOnly)?;
        let conn = Connection::open_with_flags(path, config)?;
        Ok(Self {
            conn,
            meta: LedgerMeta::new(tenant_id, binary_hash),
        })
    }

    fn initialise(
        conn: Connection,
        tenant_id: TenantId,
        binary_hash: BinaryHash,
    ) -> Result<Self, AppendError> {
        ensure_schema(&conn)?;
        Ok(Self {
            conn,
            meta: LedgerMeta::new(tenant_id, binary_hash),
        })
    }

    /// Tenant identifier this ledger belongs to.
    pub fn tenant_id(&self) -> &TenantId {
        &self.meta.tenant_id
    }

    /// Append a new entry. Opens a fresh DuckDB transaction, delegates
    /// to [`append_in_tx`], and commits. Used by callers that are not
    /// coordinating a state change in the same transaction.
    ///
    /// The binary path in `apps/aberp/src/issue_invoice.rs` does **not**
    /// use this method; it drives [`append_in_tx`] directly under a tx
    /// shared with `aberp-billing` so ADR-0008 §Storage holds.
    pub fn append(
        &mut self,
        kind: EventKind,
        payload: Vec<u8>,
        actor: Actor,
        idempotency_key: Option<String>,
    ) -> Result<EntryId, AppendError> {
        let tx = self.conn.transaction()?;
        let id = append_in_tx(&tx, &self.meta, kind, payload, actor, idempotency_key)?;
        tx.commit()?;
        Ok(id)
    }

    /// Read every entry in seq order.
    pub fn entries(&self) -> Result<Vec<Entry>, AppendError> {
        let mut stmt = self.conn.prepare(schema::SELECT_ALL)?;
        let rows = stmt.query_map([], row_to_entry)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Read the most-recent `limit` entries in DESC seq order. Thin
    /// wrapper around the free [`recent_entries`] function so callers
    /// that already own a [`Ledger`] don't have to reach for the
    /// connection. Added in S235 / PR-231 for the operator dashboard
    /// tile.
    pub fn recent(&self, limit: u32) -> Result<Vec<Entry>, AppendError> {
        recent_entries(&self.conn, limit)
    }

    /// Verify the full chain against the tenant genesis. See
    /// [`crate::chain::verify_chain`] for the exact contract.
    pub fn verify_chain(&self) -> Result<u64, LedgerVerifyError> {
        let entries = self.entries().map_err(LedgerVerifyError::Read)?;
        verify_chain(&self.meta.tenant_id, entries.iter()).map_err(LedgerVerifyError::Chain)
    }

    /// Synchronise the mirror file at `mirror_path` to the DB's
    /// current head per ADR-0030 §2. Delegates to
    /// [`crate::sync_mirror`]; method shape exists so the binary
    /// path's post-commit re-open + verify pattern extends with
    /// one extra line.
    ///
    /// # Errors
    ///
    /// Surfaces every [`AppendError`] variant
    /// [`crate::sync_mirror`] returns. See its module docs for
    /// the partial-write recovery posture (`MirrorCorrupt`),
    /// the divergence-detection posture (`MirrorDivergent`),
    /// and the bootstrap path (implicit backfill from the DB
    /// on first call).
    pub fn sync_mirror(&self, mirror_path: &Path) -> Result<u64, AppendError> {
        crate::mirror::sync_mirror(&self.conn, &self.meta, mirror_path)
    }
}

// ──────────────────────────────────────────────────────────────────────
// Cross-crate transactional surface (PR-6).
//
// The binary owns the [`Connection`], opens one transaction via
// [`Connection::transaction`], runs the billing allocator + audit-ledger
// appends against that single `&Transaction`, and commits once. Schema
// creation runs separately because DDL inside a transaction is awkward;
// `ensure_schema` is idempotent and is called before opening the tx.
// ──────────────────────────────────────────────────────────────────────

/// Create the `audit_ledger` table if it does not yet exist. Idempotent.
/// Callers expecting to drive transactional appends through
/// [`append_in_tx`] must invoke this against the [`Connection`] before
/// opening their transaction; DuckDB DDL inside a multi-statement tx is
/// not the path PR-6 wants to defend.
pub fn ensure_schema(conn: &Connection) -> Result<(), AppendError> {
    conn.execute_batch(schema::CREATE_TABLE)?;
    Ok(())
}

/// Read the most-recent `limit` entries in descending seq order
/// (newest first). Used by the operator dashboard's recent-activity
/// tile (PR-231 / S235) and any future "tail" surface. Per-tenant
/// scoping comes from the tenant DuckDB file (ADR-0002); this is
/// NOT a multi-tenant query.
pub fn recent_entries(conn: &Connection, limit: u32) -> Result<Vec<Entry>, AppendError> {
    let mut stmt = conn.prepare(schema::SELECT_RECENT)?;
    let rows = stmt.query_map(params![limit as i64], row_to_entry)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Append a new entry inside a caller-owned transaction. The caller is
/// responsible for `commit()`; an error return or panic before commit
/// leaves the ledger and any sibling state unchanged (Drop on
/// `Transaction` rolls back).
///
/// Computes `seq`, `prev_hash`, `time_wall`, `time_mono`, and
/// `entry_hash` from `meta` + the current chain head read inside the
/// same `tx`. Caller supplies the business content (`kind`, `payload`,
/// `actor`, `idempotency_key`).
pub fn append_in_tx(
    tx: &duckdb::Transaction<'_>,
    meta: &LedgerMeta,
    kind: EventKind,
    payload: Vec<u8>,
    actor: Actor,
    idempotency_key: Option<String>,
) -> Result<EntryId, AppendError> {
    // Resolve the chain head inside the tx so the seq/prev_hash we
    // compute reflect any sibling appends that already landed earlier
    // in the same tx (e.g., the binary appends two entries per
    // issuance — InvoiceSequenceReserved, then InvoiceDraftCreated).
    let head = read_head(tx)?;
    let seq = next_seq(head.as_ref());
    let prev_hash = next_prev_hash(&meta.tenant_id, head.as_ref());

    // Capture clocks.
    let time_wall = OffsetDateTime::now_utc();
    let time_mono = meta.process_start.elapsed().as_nanos() as u64;

    // Build the entry with a zero entry_hash, then compute the real
    // hash from the canonical bytes, then patch the field.
    let mut entry = Entry {
        id: EntryId::new(),
        seq,
        prev_hash,
        time_wall,
        time_mono,
        actor,
        binary_hash: meta.binary_hash,
        tenant_id: meta.tenant_id.clone(),
        kind,
        payload,
        idempotency_key,
        entry_hash: EntryHash::from_bytes([0u8; 32]),
    };
    entry.entry_hash = compute_entry_hash(&entry);

    // One body of insert logic — shared with the verbatim rebuild path
    // ([`insert_entry_verbatim`]) so the column/parameter mapping lives
    // in exactly one place (CLAUDE.md rule 8). Here `entry` carries the
    // freshly-computed seq/prev_hash/entry_hash; the verbatim path passes
    // an `Entry` decoded straight off disk.
    insert_entry_verbatim(tx, &entry)?;
    Ok(entry.id)
}

/// Insert an [`Entry`] into `audit_ledger` exactly as given — no field
/// is recomputed. Every column (`seq`, `prev_hash`, `entry_hash`,
/// `time_*`, `payload`, …) is written from the entry's own values, so
/// round-tripping a row through [`row_to_entry`] → `insert_entry_verbatim`
/// reproduces it byte-for-byte and the hash chain is preserved.
///
/// This is the workhorse of the S341 ART-rebuild path
/// ([`rebuild_table_in_tx`]): re-inserting the decoded rows into a
/// freshly-`CREATE`d table regenerates a clean ART index without
/// altering the tamper-evident chain. It is NOT an append API — it does
/// not compute `seq`/`prev_hash`, so callers outside the rebuild path
/// must use [`append_in_tx`].
fn insert_entry_verbatim(tx: &duckdb::Transaction<'_>, entry: &Entry) -> Result<(), AppendError> {
    let inserted = tx.execute(
        schema::INSERT,
        params![
            entry.id.to_prefixed_string(),
            entry.seq.as_u64() as i64,
            entry.prev_hash.as_bytes().as_slice(),
            entry.time_wall.format(&Rfc3339)?,
            entry.time_mono as i64,
            entry.actor.to_storage_json(),
            entry.binary_hash.as_bytes().as_slice(),
            entry.tenant_id.as_str(),
            entry.kind.as_str(),
            entry.payload.as_slice(),
            entry.idempotency_key.as_deref(),
            entry.entry_hash.as_bytes().as_slice(),
        ],
    )?;

    if inserted != 1 {
        return Err(AppendError::SequenceConflict {
            seq: entry.seq.as_u64(),
        });
    }
    Ok(())
}

/// Drop and recreate the `audit_ledger` table inside the caller-owned
/// transaction, re-inserting `entries` verbatim in the given order.
///
/// This is the surgical core of the S341 / PR-36 ART rebuild. The
/// DuckDB on-disk ART secondary index (`UNIQUE(seq)` / `UNIQUE(id)`)
/// can land in a corrupt state where every subsequent insert panics in
/// `FixedSizeAllocator::New` → `Prefix::New` (S332). The rows themselves
/// are intact; only the index image is bad. Dropping the table destroys
/// the corrupt ART; recreating it with the IDENTICAL schema
/// ([`schema::CREATE_TABLE`] — `UNIQUE(seq)` / `UNIQUE(id)` PRESERVED,
/// never dropped, per S332 §8.3) and re-inserting the decoded rows
/// rebuilds the ART from a clean image.
///
/// # Caller contract
///
/// - `entries` MUST be the full ledger in `seq` order and MUST already
///   have passed [`verify_chain`] — this function does NOT re-verify; it
///   writes whatever it is given. The binary's `audit_rebuild` path
///   verifies before AND after.
/// - The caller owns the transaction and is responsible for `commit()`.
///   On any error the `Transaction`'s `Drop` rolls back, leaving the
///   original (corrupt-index but data-intact) table untouched.
/// - After this returns, the caller typically appends one
///   `AuditLedgerRebuilt` marker via [`append_in_tx`] (which reads the
///   now-rebuilt head inside the same tx) and then commits.
///
/// DuckDB executes DDL transactionally, so the `DROP` + `CREATE` +
/// re-`INSERT` either all land at `commit` or none do.
pub fn rebuild_table_in_tx(
    tx: &duckdb::Transaction<'_>,
    entries: &[Entry],
) -> Result<(), AppendError> {
    // DROP destroys the corrupt ART; CREATE rebuilds the schema with the
    // SAME inline UNIQUE(seq)/UNIQUE(id) constraints (a fresh, empty ART).
    tx.execute_batch("DROP TABLE IF EXISTS audit_ledger;")?;
    tx.execute_batch(schema::CREATE_TABLE)?;

    // Re-insert every row verbatim, in seq order, regenerating the ART
    // entry by entry. Each row's stored seq/prev_hash/entry_hash are
    // written unchanged, so the chain that verified going in still
    // verifies coming out.
    for entry in entries {
        insert_entry_verbatim(tx, entry)?;
    }
    Ok(())
}

/// Read the chain head (highest seq) inside the borrowed transaction.
/// Shared between [`Ledger`] (which used to own this as a method) and
/// [`append_in_tx`].
fn read_head(tx: &duckdb::Transaction<'_>) -> Result<Option<Entry>, AppendError> {
    let mut stmt = tx.prepare(schema::SELECT_HEAD)?;
    let mut rows = stmt.query_map([], row_to_entry)?;
    match rows.next() {
        Some(r) => Ok(Some(r?)),
        None => Ok(None),
    }
}

/// Composite error for [`Ledger::verify_chain`]: either reading the rows
/// failed, or the chain verification surfaced a divergence.
#[derive(Debug, thiserror::Error)]
pub enum LedgerVerifyError {
    #[error("failed to read entries: {0}")]
    Read(#[source] AppendError),

    #[error(transparent)]
    Chain(#[from] VerifyError),
}

// ──────────────────────────────────────────────────────────────────────
// Row decoding
// ──────────────────────────────────────────────────────────────────────

fn row_to_entry(row: &duckdb::Row<'_>) -> duckdb::Result<Entry> {
    let id_prefixed: String = row.get(0)?;
    let seq: i64 = row.get(1)?;
    let prev_hash_blob: Vec<u8> = row.get(2)?;
    let time_wall_str: String = row.get(3)?;
    let time_mono_i: i64 = row.get(4)?;
    let actor_json: String = row.get(5)?;
    let binary_hash_blob: Vec<u8> = row.get(6)?;
    let tenant_str: String = row.get(7)?;
    let kind_str: String = row.get(8)?;
    let payload: Vec<u8> = row.get(9)?;
    let idempotency_key: Option<String> = row.get(10)?;
    let entry_hash_blob: Vec<u8> = row.get(11)?;

    // Decode the prefixed ULID. Returning a duckdb-shaped error keeps
    // query_map's signature happy; loud failure via the `?` in the caller.
    let id_ulid_str = id_prefixed
        .strip_prefix("aud_")
        .ok_or_else(|| duckdb_decode_err("entry id missing `aud_` prefix"))?;
    let id_ulid = Ulid::from_string(id_ulid_str)
        .map_err(|_| duckdb_decode_err("entry id is not a valid Crockford-base32 ULID"))?;

    let prev_hash = to_hash32(&prev_hash_blob, "prev_hash")?;
    let binary_hash = to_hash32(&binary_hash_blob, "binary_hash")?;
    let entry_hash = to_hash32(&entry_hash_blob, "entry_hash")?;

    let tenant_id = TenantId::new(tenant_str)
        .ok_or_else(|| duckdb_decode_err("tenant_id is empty or contains a null byte"))?;
    let time_wall = OffsetDateTime::parse(&time_wall_str, &Rfc3339)
        .map_err(|_| duckdb_decode_err("time_wall is not RFC3339"))?;
    let actor = Actor::from_storage_json(&actor_json)
        .map_err(|_| duckdb_decode_err("actor JSON failed to deserialize"))?;
    // Single source of truth for the kind round-trip lives next to
    // `as_str` in `entry::event_kind`. PR-6.1 (F12) replaced the
    // hand-maintained match here with a delegate so adding a variant
    // touches one file, not two.
    let kind = EventKind::from_storage_str(&kind_str)
        .map_err(|_| duckdb_decode_err("unknown event kind"))?;

    Ok(Entry {
        id: EntryId(id_ulid),
        seq: Sequence(seq as u64),
        prev_hash: EntryHash::from_bytes(prev_hash),
        time_wall,
        time_mono: time_mono_i as u64,
        actor,
        binary_hash: BinaryHash::from_bytes(binary_hash),
        tenant_id,
        kind,
        payload,
        idempotency_key,
        entry_hash: EntryHash::from_bytes(entry_hash),
    })
}

fn to_hash32(blob: &[u8], field: &'static str) -> duckdb::Result<[u8; 32]> {
    if blob.len() != 32 {
        return Err(duckdb_decode_err_owned(format!(
            "{field} blob has length {} (expected 32)",
            blob.len()
        )));
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(blob);
    Ok(out)
}

fn duckdb_decode_err(msg: &'static str) -> duckdb::Error {
    // `Type::Text` is the lowest-common-denominator variant across duckdb-rs
    // minor versions; the field marker (`0`) is a placeholder — the loud
    // message in `msg` carries the real diagnostic.
    duckdb::Error::FromSqlConversionFailure(
        0,
        duckdb::types::Type::Text,
        Box::<dyn std::error::Error + Send + Sync>::from(msg),
    )
}

fn duckdb_decode_err_owned(msg: String) -> duckdb::Error {
    duckdb::Error::FromSqlConversionFailure(
        0,
        duckdb::types::Type::Text,
        Box::<dyn std::error::Error + Send + Sync>::from(msg),
    )
}
