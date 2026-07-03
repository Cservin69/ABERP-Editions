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
use std::sync::Mutex;
use std::time::Instant;

use duckdb::{params, Connection};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use ulid::Ulid;

use crate::chain::compute::{compute_entry_hash, next_prev_hash, next_seq};
use crate::chain::genesis::genesis_hash;
use crate::chain::verify::verify_chain;
use crate::entry::{Actor, BinaryHash, Entry, EntryHash, EntryId, EventKind, Sequence, TenantId};
use crate::error::{AppendError, VerifyError};
use crate::session::anchors::{self, Anchor, AnchorKind};
use crate::session::tsa::TimestampAuthority;
use crate::session::SessionContext;

/// Process-wide serializer for the full audit-write critical section
/// (`begin tx → read head → insert → commit`).
///
/// # Why an app-level lock replaced `UNIQUE(seq)`
///
/// The `audit_ledger` schema used to carry inline `UNIQUE(seq)` /
/// `UNIQUE(id)` constraints. Those are the ONLY ART (Adaptive Radix
/// Tree) indexes the table had — and DuckDB 1.5.x corrupts the on-disk
/// ART of a file-backed database on insert (upstream
/// `duckdb/duckdb#23046`, "ART index constraint enforcement corrupts the
/// heap on file-backed", introduced in 1.5.0, still open in the latest
/// 1.5.3). That corruption is what made every audit-bearing commit panic
/// in `FixedSizeAllocator::New → Prefix::New` (S332). Dropping the
/// `UNIQUE` constraints removes the ART entirely — the corruption class
/// cannot occur because there is no secondary index to corrupt.
///
/// The constraint was, in any case, NOT the cross-writer fork guard it
/// looked like: ABERP's own S186/PR-186 finding
/// (`apps/aberp/src/incoming_invoices.rs`) established that "DuckDB's
/// UNIQUE constraint does NOT fire across two `Connection::open` handles
/// in the same process". So `UNIQUE(seq)` never prevented a concurrent
/// in-process fork — and an empirical probe in S341 confirmed two
/// concurrent `Connection::open` handles coexist with no exclusive lock.
///
/// Integrity is therefore enforced at two layers, neither of them the
/// ART:
///   1. **Detection** — the tamper-evident hash chain ([`verify_chain`])
///      catches any duplicate / reordered / forked `seq` loudly.
///   2. **Prevention (in-process)** — this `Mutex` serializes the whole
///      read-head→insert→commit window so two in-process writers cannot
///      read the same head and both append `seq = head + 1`. Each write
///      re-reads the committed head under the lock (NOT a cached
///      counter, which would go stale across processes — the exact
///      hazard S335 documented for persistent connections).
///
/// Cross-PROCESS writers (a CLI subcommand racing `aberp serve`) are
/// outside this lock's reach; they are backstopped by the hash chain's
/// detection, the same as before this change (`UNIQUE` never covered
/// them either). A single serialized audit-writer actor across the whole
/// process tree remains the documented future hardening (S335 §3.4).
static AUDIT_APPEND_LOCK: Mutex<()> = Mutex::new(());

/// Separate lock serializing the one-time boot migration that drops the
/// legacy `UNIQUE`-ART schema (see [`migrate_drop_unique_art_if_present`]).
/// Distinct from [`AUDIT_APPEND_LOCK`] so the migration (which runs
/// inside `ensure_schema`, before the append lock is taken) cannot
/// deadlock against an in-flight append.
static AUDIT_MIGRATION_LOCK: Mutex<()> = Mutex::new(());

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
        // ADR-0098 R3 (finding C) — suppress DuckDB's implicit close-checkpoint
        // (in-place WAL fold, duckdb#23046) on every Ledger::open connection, so
        // this central opener's ~145 callers inherit the guard. Exact pragma
        // string from crates/aberp-snapshot/src/take.rs:208 / aberp-db
        // open_runtime_connection. An unknown pragma errors HARD (loud, never
        // silent) — the desired fail-hard posture.
        conn.execute_batch("PRAGMA disable_checkpoint_on_shutdown;")?;
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

    /// Wrap an ALREADY-OPEN DuckDB [`Connection`] as a `Ledger`
    /// WITHOUT re-opening the file (S375).
    ///
    /// The binary's issue / storno paths own the post-commit
    /// [`Connection`] and need `verify_chain` + `sync_mirror` after the
    /// tx commits. The pre-S375 pattern dropped that Connection and
    /// called [`Ledger::open`] again — a fresh `Connection::open(path)`
    /// that triggers DuckDB 1.5.x's `LoadCheckpoint`/`ReadIndex` replay,
    /// which hits the metadata-pointer assertion of the
    /// checkpoint/ART corruption family (`duckdb/duckdb#23046`, S332).
    /// Reusing the already-open handle never re-opens the file, so that
    /// assertion is unreachable.
    ///
    /// Schema is assumed already present — the issue / storno paths run
    /// [`ensure_schema`] in their pre-tx setup — so this does NOT re-run
    /// DDL (and therefore does no checkpoint replay of its own).
    pub fn from_connection(conn: Connection, tenant_id: TenantId, binary_hash: BinaryHash) -> Self {
        Self {
            conn,
            meta: LedgerMeta::new(tenant_id, binary_hash),
        }
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
    ///
    /// Holds [`AUDIT_APPEND_LOCK`] across the whole read-head → insert →
    /// commit window so concurrent in-process appends cannot read the
    /// same head and fork the chain (the prevention layer that replaced
    /// the dropped `UNIQUE(seq)` ART — see [`AUDIT_APPEND_LOCK`]).
    pub fn append(
        &mut self,
        kind: EventKind,
        payload: Vec<u8>,
        actor: Actor,
        idempotency_key: Option<String>,
    ) -> Result<EntryId, AppendError> {
        let _guard = AUDIT_APPEND_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let tx = self.conn.transaction()?;
        let id = append_in_tx(&tx, &self.meta, kind, payload, actor, idempotency_key)?;
        tx.commit()?;
        Ok(id)
    }

    /// S441 / ADR-0087 — append a SIGNED entry under a session. Mirrors
    /// [`Ledger::append`] but routes through the signing chokepoint
    /// ([`append_in_tx_signed`]); `session = None` is byte-identical to
    /// [`Ledger::append`] (the back-compat unsigned path).
    #[allow(clippy::too_many_arguments)]
    pub fn append_signed(
        &mut self,
        kind: EventKind,
        subject: &str,
        payload: Vec<u8>,
        actor: Actor,
        idempotency_key: Option<String>,
        session: Option<&SessionContext>,
    ) -> Result<EntryId, AppendError> {
        let _guard = AUDIT_APPEND_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let tx = self.conn.transaction()?;
        let id = append_in_tx_signed(
            &tx,
            &self.meta,
            kind,
            subject,
            payload,
            actor,
            idempotency_key,
            session,
        )?;
        tx.commit()?;
        Ok(id)
    }

    /// The current chain head `entry_hash` as hex, or the tenant genesis
    /// hash hex if the chain is empty. The value an anchor commits to.
    pub fn chain_head_hash_hex(&self) -> Result<String, AppendError> {
        let mut stmt = self.conn.prepare(schema::SELECT_HEAD)?;
        let mut rows = stmt.query_map([], row_to_entry)?;
        let head_hash = match rows.next() {
            Some(r) => r?.entry_hash,
            None => genesis_hash(&self.meta.tenant_id),
        };
        Ok(hex::encode(head_hash.as_bytes()))
    }

    /// S441 / ADR-0087 — take a qualified-timestamp anchor over the current
    /// chain head. Never blocks on the TSA (a network failure queues a
    /// `pending` row — see [`anchors::take_anchor`]).
    pub fn take_anchor(
        &self,
        tsa: &dyn TimestampAuthority,
        session_id: &str,
        kind: AnchorKind,
    ) -> Result<Anchor, AppendError> {
        let head_hex = self.chain_head_hash_hex()?;
        anchors::take_anchor(
            &self.conn,
            tsa,
            self.meta.tenant_id.as_str(),
            session_id,
            kind,
            &head_hex,
        )
    }

    /// All anchor rows for this tenant in `created_at` order.
    pub fn anchors(&self) -> Result<Vec<Anchor>, AppendError> {
        anchors::anchors_for_tenant(&self.conn, self.meta.tenant_id.as_str())
    }

    /// Session ids opened but never cleanly closed — the orphan sessions
    /// ADR-0087 crash recovery closes on boot.
    pub fn open_sessions_without_close(&self) -> Result<Vec<String>, AppendError> {
        anchors::open_sessions_without_close(&self.conn, self.meta.tenant_id.as_str())
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

/// Create the `audit_ledger` table if it does not yet exist, then run
/// the transparent boot migration that drops the legacy `UNIQUE`-ART
/// schema if it is still present. Idempotent.
///
/// Every audit entry point (the binary's boot, each CLI subcommand, the
/// reopen-per-write daemons) calls `ensure_schema` before appending, so
/// folding the migration in here makes the recovery automatic and
/// operator-action-free regardless of which path reaches the corrupt DB
/// first. After the one-time migration the detection query is a cheap
/// metadata read that returns "no UNIQUE" and the function is a no-op.
/// ADR-0098 C2 fix-forward — is this DuckDB connection read-only
/// (`AccessMode::ReadOnly`)? The read()-side connections handed out by
/// `aberp_db::Handle::read` under `read_returns_readonly` are read-only; the
/// idempotent `ensure_schema` DDL must be a no-op on them (the schema is
/// guaranteed to have been created by a writer conn at boot/first-write
/// before any read reaches it). Detected via the `access_mode` setting the
/// `duckdb` crate sets from `AccessMode::ReadOnly` ("READ_ONLY") — a stable
/// metadata read, NOT a fragile error-string match. On any query error we
/// return `false` (attempt the DDL, which then fails loud on a genuine
/// read-only conn per F5 — no worse than before this helper).
pub fn connection_is_read_only(conn: &Connection) -> bool {
    conn.query_row("SELECT current_setting('access_mode')", [], |r| {
        r.get::<_, String>(0)
    })
    .map(|mode| mode.eq_ignore_ascii_case("read_only"))
    .unwrap_or(false)
}

pub fn ensure_schema(conn: &Connection) -> Result<(), AppendError> {
    // ADR-0098 C2 — skip the idempotent schema DDL on a read-only conn
    // (read_returns_readonly read()-side). The schema already exists; a
    // genuine write mis-routed through read() still fails loud (F5).
    if connection_is_read_only(conn) {
        return Ok(());
    }
    conn.execute_batch(schema::CREATE_TABLE)?;
    // S441 / ADR-0087 — add the three nullable session-signing columns to
    // existing tenant DBs BEFORE the unique-ART migration runs (that
    // migration does a 15-column SELECT_ALL, so the columns must be present
    // first; a fresh DB already has them from CREATE_TABLE).
    migrate_add_session_columns_if_absent(conn)?;
    migrate_drop_unique_art_if_present(conn)?;
    // S441 / ADR-0087 — the qualified-timestamp anchors table. Additive,
    // no PK/UNIQUE (duckdb#23046, like audit_ledger). Idempotent.
    conn.execute_batch(crate::session::anchors::CREATE_ANCHORS_TABLE)?;
    Ok(())
}

/// S441 / ADR-0087 — additively add `session_id`, `session_pubkey`,
/// `event_sig` to an existing `audit_ledger` table. All nullable, no
/// `DEFAULT` (the DuckDB replay-clobber trap, S434/S341 lineage). Detected
/// via `duckdb_columns()` so the `ALTER` runs at most once; idempotent.
///
/// Additive + nullable means every legacy row reads back `None` for the
/// three fields, and because they are excluded from the `entry_hash`
/// canonical preimage, every legacy `entry_hash` stays byte-identical —
/// `verify_chain` over a migrated DB is unaffected.
fn migrate_add_session_columns_if_absent(conn: &Connection) -> Result<(), AppendError> {
    if audit_ledger_has_column(conn, "session_id")? {
        return Ok(());
    }
    // Each column added separately so a partially-migrated DB (only some
    // columns present) converges. `ADD COLUMN` defaults to NULLABLE.
    for col in ["session_id", "session_pubkey", "event_sig"] {
        if !audit_ledger_has_column(conn, col)? {
            conn.execute_batch(&format!(
                "ALTER TABLE audit_ledger ADD COLUMN {col} VARCHAR;"
            ))?;
        }
    }
    tracing::warn!(
        "migrated audit_ledger: added session_id/session_pubkey/event_sig columns (S441/ADR-0087)"
    );
    Ok(())
}

/// `true` iff `audit_ledger` carries a column named `col`. Metadata read
/// (`duckdb_columns()`), safe against any ART state.
fn audit_ledger_has_column(conn: &Connection, col: &str) -> Result<bool, AppendError> {
    let mut stmt = conn.prepare(
        "SELECT count(*) FROM duckdb_columns() \
         WHERE table_name = 'audit_ledger' AND column_name = ?",
    )?;
    let n: i64 = stmt.query_row(params![col], |r| r.get(0))?;
    Ok(n > 0)
}

/// Transparent, one-time migration off the legacy `audit_ledger` schema
/// that carried inline `UNIQUE(seq)` / `UNIQUE(id)` constraints — the
/// only ART indexes on the table, and the ones DuckDB 1.5.x corrupts on
/// file-backed inserts (`duckdb/duckdb#23046`, S332). The migration
/// rebuilds the table WITHOUT those constraints, which both eliminates
/// the corruption class going forward AND repairs an already-corrupt ART
/// (the `DROP TABLE` discards the bad index; the rows are read back
/// intact because the crash is on INSERT, not SELECT — S332 §5).
///
/// Steps (no operator action, runs at boot / first append after upgrade):
///   1. Detect — `duckdb_constraints()` reports any `UNIQUE` on
///      `audit_ledger`? If none, return immediately (the post-migration
///      steady state).
///   2. Serialize via [`AUDIT_MIGRATION_LOCK`] + re-check (another
///      in-process caller may have just migrated).
///   3. Dump every row in `seq` order and `verify_chain` them — PROVE
///      the rows are intact (only the index was corrupt). A broken chain
///      ABORTS loud: that is data tampering, not index corruption, and a
///      rebuild would faithfully re-index a tampered chain.
///   4. In one transaction: `DROP TABLE` (discards the corrupt ART) +
///      `CREATE TABLE` (new no-`UNIQUE` schema) + re-insert every row
///      verbatim (same `seq`/`prev_hash`/`entry_hash` bytes). Commit.
///   5. Re-`verify_chain` the rebuilt table as the safety gate.
///
/// The rebuild preserves the rows byte-for-byte, so the chain that
/// verified going in verifies coming out and NO audit row is added — the
/// migration leaves no trace in the ledger content (provenance is the
/// `tracing::warn!` below). It is a pure structural recovery.
fn migrate_drop_unique_art_if_present(conn: &Connection) -> Result<(), AppendError> {
    if !audit_ledger_has_unique_constraints(conn)? {
        return Ok(());
    }

    let _guard = AUDIT_MIGRATION_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // Re-check under the lock: a concurrent in-process caller may have
    // migrated between our detect and our lock acquisition.
    if !audit_ledger_has_unique_constraints(conn)? {
        return Ok(());
    }

    // 1. Dump every row in seq order (SELECT is safe against the corrupt
    //    ART — the S332 crash is on INSERT only).
    let entries = read_all_entries(conn)?;

    // 2. Prove the rows are intact. Single tenant per file (ADR-0002), so
    //    every row carries the same tenant_id; derive it from the head.
    if let Some(first) = entries.first() {
        verify_chain(&first.tenant_id, entries.iter()).map_err(|e| {
            AppendError::Migration(format!(
                "refusing to migrate audit_ledger off the UNIQUE-ART schema: the dumped rows do \
                 NOT verify ({e}). This is data tampering, not index corruption — a structural \
                 rebuild would faithfully re-index a tampered chain. Aborting; investigate the \
                 ledger file."
            ))
        })?;
    }

    // 3. Rebuild without the UNIQUE constraints, in one transaction.
    //    Manual BEGIN/COMMIT keeps this on the `&Connection` ensure_schema
    //    received (no `&mut` ripple through every ensure_schema caller);
    //    DuckDB executes the DROP+CREATE+INSERTs transactionally, so a
    //    failure ROLLBACKs and leaves the original table untouched.
    conn.execute_batch("BEGIN TRANSACTION;")?;
    if let Err(e) = rebuild_table(conn, &entries) {
        let _ = conn.execute_batch("ROLLBACK;");
        return Err(e);
    }
    conn.execute_batch("COMMIT;")?;

    // 4. Safety gate — the rebuilt table must still verify.
    if let Some(first) = entries.first() {
        let rebuilt = read_all_entries(conn)?;
        verify_chain(&first.tenant_id, rebuilt.iter()).map_err(|e| {
            AppendError::Migration(format!(
                "post-migration verify FAILED ({e}) — the audit_ledger rebuild did not preserve \
                 the chain. This must never happen."
            ))
        })?;
    }

    tracing::warn!(
        rows = entries.len(),
        "migrated audit_ledger off the legacy UNIQUE-ART schema (duckdb#23046 / S332): dropped \
         UNIQUE(seq)/UNIQUE(id), rebuilt without a secondary index; hash chain verified intact"
    );
    Ok(())
}

/// `true` iff `audit_ledger` still carries any `UNIQUE` constraint (the
/// legacy ART schema). Reads `duckdb_constraints()` — a metadata query
/// that does not touch the ART insert path, so it is safe even when the
/// on-disk ART is corrupt.
fn audit_ledger_has_unique_constraints(conn: &Connection) -> Result<bool, AppendError> {
    let mut stmt = conn.prepare(
        "SELECT count(*) FROM duckdb_constraints() \
         WHERE table_name = 'audit_ledger' AND constraint_type = 'UNIQUE'",
    )?;
    let n: i64 = stmt.query_row([], |r| r.get(0))?;
    Ok(n > 0)
}

/// Read every entry in `seq` order off a borrowed [`Connection`] (the
/// migration path owns a `&Connection`, not a [`Ledger`]). Mirrors
/// [`Ledger::entries`].
fn read_all_entries(conn: &Connection) -> Result<Vec<Entry>, AppendError> {
    let mut stmt = conn.prepare(schema::SELECT_ALL)?;
    let rows = stmt.query_map([], row_to_entry)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Serialized reopen-per-write append: open a fresh [`Connection`] (so
/// the head read sees the current on-disk state — the coherence
/// mechanism every ABERP daemon relies on, S335), ensure the schema /
/// run the boot migration, then append + commit under
/// [`AUDIT_APPEND_LOCK`].
///
/// This is the safe replacement for the hand-rolled
/// `Connection::open` + `ensure_schema` + `append_in_tx` + `commit`
/// pattern the high-frequency daemons used: identical reopen-per-write
/// semantics, plus the process-wide append lock so two in-process
/// writers cannot read the same head and fork the chain now that the
/// `UNIQUE(seq)` ART is gone. The lock is taken AFTER `ensure_schema`
/// (which has its own [`AUDIT_MIGRATION_LOCK`]) to avoid re-entrancy.
pub fn append_reopen(
    db_path: &Path,
    meta: &LedgerMeta,
    kind: EventKind,
    payload: Vec<u8>,
    actor: Actor,
    idempotency_key: Option<String>,
) -> Result<EntryId, AppendError> {
    // Hold the lock across the WHOLE open → ensure_schema → append →
    // commit → close window, not just the append. Two reasons:
    //   1. Fork prevention — a concurrent in-process writer must not read
    //      the same committed head and append a duplicate `seq` (the role
    //      the dropped `UNIQUE(seq)` ART nominally played).
    //   2. DuckDB stability — independent `Connection::open` handles
    //      touching the same file concurrently can trip a DuckDB-internal
    //      assertion (observed: `RLEScanState`). Serializing the whole
    //      reopen-per-write keeps the in-process audit path single-writer.
    // `ensure_schema` (called here) takes its own `AUDIT_MIGRATION_LOCK`,
    // never this one, so there is no re-entrant deadlock.
    let _guard = AUDIT_APPEND_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut conn = Connection::open(db_path)?;
    ensure_schema(&conn)?;
    let tx = conn.transaction()?;
    let id = append_in_tx(&tx, meta, kind, payload, actor, idempotency_key)?;
    tx.commit()?;
    Ok(id)
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
    // Legacy / unsigned append: route through the one chokepoint with no
    // session (session_id/session_pubkey/event_sig left NULL — back-compat
    // for the ~109 existing audit writers, ADR-0087).
    append_in_tx_signed(tx, meta, kind, "", payload, actor, idempotency_key, None)
}

/// S441 / ADR-0087 — the audit-write CHOKEPOINT. Every audit row is built
/// and inserted here (the legacy [`append_in_tx`] delegates with no
/// session). When `session.is_some()`, the entry is SIGNED: `event_sig` =
/// Ed25519 over `prev_hash || kind || subject || SHA-256(payload)`, and
/// `session_id` + `session_pubkey` are persisted. When `session.is_none()`,
/// the three columns stay NULL (legacy behaviour).
///
/// The signature is an INDEPENDENT layer — it is NOT folded into
/// `entry_hash` (whose canonical preimage is unchanged), so signed and
/// legacy entries share one tamper-evidence hash chain and one hash
/// computation. See ADR-0087 §"Why the signature is a separate layer".
#[allow(clippy::too_many_arguments)]
pub fn append_in_tx_signed(
    tx: &duckdb::Transaction<'_>,
    meta: &LedgerMeta,
    kind: EventKind,
    subject: &str,
    payload: Vec<u8>,
    actor: Actor,
    idempotency_key: Option<String>,
    session: Option<&SessionContext>,
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

    // Sign BEFORE building the entry: the preimage covers prev_hash, so the
    // signature chains to the link structure even though it stays out of
    // the entry_hash canonical map.
    let (session_id, session_pubkey, event_sig) = match session {
        Some(s) => {
            let preimage = crate::session::event_sig_preimage(&prev_hash, &kind, subject, &payload);
            let sig = s.sign(&preimage);
            (
                Some(s.session_id.clone()),
                Some(s.pubkey_hex()),
                Some(hex::encode(sig)),
            )
        }
        None => (None, None, None),
    };

    // Build the entry with a zero entry_hash, then compute the real
    // hash from the canonical bytes (which EXCLUDE the three session
    // fields), then patch the field.
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
        session_id,
        session_pubkey,
        event_sig,
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
/// Shared by [`append_in_tx`] (which passes a freshly-computed entry)
/// and the S341 boot migration ([`rebuild_table_in_tx`], which passes
/// rows decoded straight off disk to re-seat them in the new no-`UNIQUE`
/// table without altering the tamper-evident chain). It is NOT an append
/// API — it does not compute `seq`/`prev_hash`, so callers outside the
/// migration path must use [`append_in_tx`].
fn insert_entry_verbatim(conn: &Connection, entry: &Entry) -> Result<(), AppendError> {
    let inserted = conn.execute(
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
            entry.session_id.as_deref(),
            entry.session_pubkey.as_deref(),
            entry.event_sig.as_deref(),
        ],
    )?;

    if inserted != 1 {
        return Err(AppendError::SequenceConflict {
            seq: entry.seq.as_u64(),
        });
    }
    Ok(())
}

/// Drop and recreate the `audit_ledger` table, re-inserting `entries`
/// verbatim in the given order. The caller wraps this in a transaction
/// (the migration's manual `BEGIN`/`COMMIT`).
///
/// The structural core of [`migrate_drop_unique_art_if_present`].
/// `DROP TABLE` discards the legacy table (and its corrupt ART, if any);
/// `CREATE TABLE` recreates it from [`schema::CREATE_TABLE`] — the NEW,
/// no-`UNIQUE` schema, so the rebuilt table has NO secondary index and
/// cannot re-enter the `duckdb#23046` corruption class. The decoded rows
/// are re-inserted verbatim (same `seq`/`prev_hash`/`entry_hash` bytes),
/// so the hash chain is preserved exactly.
///
/// `entries` MUST be the full ledger in `seq` order and SHOULD already
/// have passed [`verify_chain`] — this does NOT re-verify; it writes
/// whatever it is given. The migration verifies before AND after.
fn rebuild_table(conn: &Connection, entries: &[Entry]) -> Result<(), AppendError> {
    conn.execute_batch("DROP TABLE IF EXISTS audit_ledger;")?;
    conn.execute_batch(schema::CREATE_TABLE)?;
    for entry in entries {
        insert_entry_verbatim(conn, entry)?;
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
    // S441 / ADR-0087 — nullable session-signing columns (NULL for legacy
    // + unsigned rows).
    let session_id: Option<String> = row.get(12)?;
    let session_pubkey: Option<String> = row.get(13)?;
    let event_sig: Option<String> = row.get(14)?;

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
        session_id,
        session_pubkey,
        event_sig,
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

#[cfg(test)]
mod migration_tests {
    use super::*;

    /// The legacy schema this migration exists to retire: identical to
    /// the current [`schema::CREATE_TABLE`] except for the two inline
    /// `UNIQUE` constraints (the ART indexes duckdb#23046 corrupts).
    // The unique-ART migration always runs AFTER the session-column
    // migration in `ensure_schema`, so by the time it sees a legacy table
    // the 15 columns are present. This fixture carries them + the UNIQUE
    // constraints it exists to retire.
    const OLD_DDL_WITH_UNIQUE: &str = "
CREATE TABLE IF NOT EXISTS audit_ledger (
    id              VARCHAR     NOT NULL,
    seq             BIGINT      NOT NULL CHECK (seq >= 1),
    prev_hash       BLOB        NOT NULL,
    time_wall       VARCHAR     NOT NULL,
    time_mono       BIGINT      NOT NULL CHECK (time_mono >= 0),
    actor           VARCHAR     NOT NULL,
    binary_hash     BLOB        NOT NULL,
    tenant_id       VARCHAR     NOT NULL,
    kind            VARCHAR     NOT NULL,
    payload         BLOB        NOT NULL,
    idempotency_key VARCHAR,
    entry_hash      BLOB        NOT NULL,
    session_id      VARCHAR,
    session_pubkey  VARCHAR,
    event_sig       VARCHAR,
    UNIQUE (seq),
    UNIQUE (id)
);
";

    fn tenant() -> TenantId {
        TenantId::new("mig-test".to_string()).unwrap()
    }
    fn meta() -> LedgerMeta {
        LedgerMeta::new(tenant(), BinaryHash::from_bytes([9u8; 32]))
    }
    fn actor() -> Actor {
        Actor::from_local_cli("01H0000000000000000000000Z".to_string(), "t")
    }

    /// Seed an in-memory DB carrying the OLD (UNIQUE-ART) schema with `n`
    /// valid-chain rows written through the normal append path.
    fn seed_old_schema(n: usize) -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(OLD_DDL_WITH_UNIQUE).unwrap();
        let m = meta();
        for i in 0..n {
            let tx = conn.transaction().unwrap();
            append_in_tx(
                &tx,
                &m,
                EventKind::Test,
                format!("{{\"i\":{i}}}").into_bytes(),
                actor(),
                None,
            )
            .unwrap();
            tx.commit().unwrap();
        }
        conn
    }

    #[test]
    fn fresh_schema_has_no_unique_constraints() {
        // The new CREATE_TABLE must carry NO UNIQUE (no ART → no
        // duckdb#23046 corruption class).
        let conn = Connection::open_in_memory().unwrap();
        ensure_schema(&conn).unwrap();
        assert!(
            !audit_ledger_has_unique_constraints(&conn).unwrap(),
            "fresh audit_ledger must have no UNIQUE constraints"
        );
    }

    #[test]
    fn migration_detects_and_rebuilds_off_unique_schema() {
        let conn = seed_old_schema(10);
        assert!(
            audit_ledger_has_unique_constraints(&conn).unwrap(),
            "seeded DB must start on the legacy UNIQUE schema"
        );

        migrate_drop_unique_art_if_present(&conn).unwrap();

        // No UNIQUE constraints remain; all rows preserved; chain intact.
        assert!(!audit_ledger_has_unique_constraints(&conn).unwrap());
        let rows = read_all_entries(&conn).unwrap();
        assert_eq!(rows.len(), 10, "all rows preserved across migration");
        let seqs: Vec<u64> = rows.iter().map(|e| e.seq.as_u64()).collect();
        assert_eq!(seqs, (1..=10).collect::<Vec<_>>(), "seq dense + monotonic");
        assert_eq!(
            verify_chain(&tenant(), rows.iter()).unwrap(),
            10,
            "hash chain verifies after migration"
        );
    }

    #[test]
    fn migration_is_idempotent() {
        let conn = seed_old_schema(5);
        migrate_drop_unique_art_if_present(&conn).unwrap();
        // Second call is a fast no-op (no UNIQUE left to detect).
        migrate_drop_unique_art_if_present(&conn).unwrap();
        assert!(!audit_ledger_has_unique_constraints(&conn).unwrap());
        assert_eq!(read_all_entries(&conn).unwrap().len(), 5);
    }

    #[test]
    fn ensure_schema_runs_the_migration() {
        // The transparent path: a caller that only calls ensure_schema
        // (the daemons, mark_abandoned, etc.) gets migrated automatically.
        let conn = seed_old_schema(4);
        assert!(audit_ledger_has_unique_constraints(&conn).unwrap());
        ensure_schema(&conn).unwrap();
        assert!(!audit_ledger_has_unique_constraints(&conn).unwrap());
        assert_eq!(
            verify_chain(&tenant(), read_all_entries(&conn).unwrap().iter()).unwrap(),
            4
        );
    }

    #[test]
    fn migration_aborts_on_tampered_chain() {
        // If the dumped rows do NOT verify, the data — not just the index
        // — is suspect; the migration must refuse rather than faithfully
        // re-index a tampered chain (CLAUDE.md rule 12).
        let conn = seed_old_schema(6);
        // Tamper: overwrite a payload in place so the entry_hash no longer
        // matches the canonical encoding.
        conn.execute(
            "UPDATE audit_ledger SET payload = ? WHERE seq = 3",
            params![b"tampered".as_slice()],
        )
        .unwrap();

        let err = migrate_drop_unique_art_if_present(&conn)
            .expect_err("migration must refuse a tampered chain");
        assert!(matches!(err, AppendError::Migration(_)), "got {err:?}");
        // The table is untouched (still on the old schema, still 6 rows) —
        // the rebuild ran inside a transaction that never committed.
        assert!(audit_ledger_has_unique_constraints(&conn).unwrap());
        assert_eq!(read_all_entries(&conn).unwrap().len(), 6);
    }

    #[test]
    fn migration_on_empty_legacy_table_is_safe() {
        let conn = seed_old_schema(0);
        migrate_drop_unique_art_if_present(&conn).unwrap();
        assert!(!audit_ledger_has_unique_constraints(&conn).unwrap());
        assert_eq!(read_all_entries(&conn).unwrap().len(), 0);
    }
}

#[cfg(test)]
mod from_connection_tests {
    //! S375 — [`Ledger::from_connection`] lets the binary's post-commit
    //! issue / storno path run `verify_chain` + `sync_mirror` on the
    //! Connection it already holds, instead of dropping it and calling
    //! [`Ledger::open`] (which re-opens the file and triggers the
    //! DuckDB 1.5.x checkpoint/ART assertion — S332 / duckdb#23046).
    //!
    //! The load-bearing claim these tests pin: a file-backed Connection
    //! that has just committed appends can be wrapped via
    //! `from_connection` and verified WITHOUT a second
    //! `Connection::open`. That is the exact post-commit shape the
    //! issue / storno paths now use.

    use super::*;

    fn tenant() -> TenantId {
        TenantId::new("from-conn-test".to_string()).unwrap()
    }
    fn meta() -> LedgerMeta {
        LedgerMeta::new(tenant(), BinaryHash::from_bytes([7u8; 32]))
    }
    fn actor() -> Actor {
        Actor::from_local_cli("01H0000000000000000000000Z".to_string(), "t")
    }

    fn temp_db_path(tag: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "aberp-s375-from-conn-{}-{}-{:?}.duckdb",
            std::process::id(),
            tag,
            std::thread::current().id(),
        ));
        let _ = std::fs::remove_file(&p);
        p
    }

    /// Append N entries to a FILE-backed Connection, commit, then wrap
    /// that SAME Connection with `from_connection` and verify the chain.
    /// No second `Connection::open` — this is the crash-avoidance the
    /// S375 fix relies on.
    #[test]
    fn from_connection_verifies_chain_on_post_commit_handle_without_reopen() {
        let path = temp_db_path("verify");
        let m = meta();

        // Open the file once, ensure schema, append + commit 3 entries —
        // exactly the pre-commit half of the issue / storno path.
        let mut conn = Connection::open(&path).unwrap();
        ensure_schema(&conn).unwrap();
        for i in 0..3 {
            let tx = conn.transaction().unwrap();
            append_in_tx(
                &tx,
                &m,
                EventKind::Test,
                format!("{{\"i\":{i}}}").into_bytes(),
                actor(),
                None,
            )
            .unwrap();
            tx.commit().unwrap();
        }

        // Wrap the already-open handle — NOT a fresh Ledger::open — and
        // verify. This is what `run_single_tx`'s returned Connection now
        // feeds into.
        let ledger = Ledger::from_connection(conn, tenant(), BinaryHash::from_bytes([7u8; 32]));
        let verified = ledger.verify_chain().expect("verify on reused handle");
        assert_eq!(verified, 3, "all three committed entries must verify");

        // sync_mirror must also work off the reused handle (the second
        // post-commit call the issue / storno path makes).
        let mirror = path.with_extension("mirror.jsonl");
        let synced = ledger
            .sync_mirror(&mirror)
            .expect("sync mirror off reused handle");
        assert_eq!(synced, 3, "mirror sync must report all three entries");

        drop(ledger);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&mirror);
    }

    /// Empty (genesis) case: a freshly-schema'd Connection with no
    /// appends wraps + verifies to 0, mirroring the replay/no-op path.
    #[test]
    fn from_connection_verifies_empty_genesis() {
        let path = temp_db_path("empty");
        let conn = Connection::open(&path).unwrap();
        ensure_schema(&conn).unwrap();
        let ledger = Ledger::from_connection(conn, tenant(), BinaryHash::from_bytes([7u8; 32]));
        assert_eq!(ledger.verify_chain().expect("verify empty"), 0);
        drop(ledger);
        let _ = std::fs::remove_file(&path);
    }
}
