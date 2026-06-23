//! PR-17 / ADR-0030 — audit-ledger mirror file (`<db>.audit.log`).
//!
//! The mirror is a per-tenant on-disk JSON-Lines artifact that
//! shadows the DuckDB `audit_ledger` table. Per ADR-0008
//! §"Storage", "the ledger is also mirrored to an append-only file
//! outside the DB on every commit, fsync'd." PR-17 realises that
//! sentence; ADR-0030 decides the format, the write-time hook
//! location, the recovery posture on partial writes, and the
//! read-time surface the bundle reader consumes.
//!
//! # Concepts
//!
//! - **Path convention** (`mirror_path_for`) — `<db_path>.audit.log`.
//!   ADR-0008 §"Storage" named `<tenant>.audit.log`; the literal-
//!   suffix convention here is operationally identical because
//!   ADR-0002 names one DB file per tenant, and avoids a separate
//!   path-resolution surface.
//! - **Write-time hook** (`sync_mirror`) — invoked by the binary
//!   path AFTER `tx.commit()`. Reads the mirror's last line,
//!   verifies it against the DB's matching entry, reads DB entries
//!   with `seq > mirror_head`, appends each as one JSON-Lines line,
//!   fsyncs. Per ADR-0030 §2, the mirror reflects committed state
//!   only — running the hook pre-commit would create permanent
//!   divergence on a rollback.
//! - **Recovery on partial writes** — fail loud (per ADR-0030 §3 +
//!   CLAUDE.md rule 12). Three new `AppendError` variants:
//!   `MirrorCorrupt` (last line not newline-terminated, or non-
//!   ascending/duplicate seqs, or JSON decode failure),
//!   `MirrorDivergent` (mirror's `entry_hash[seq]` disagrees with
//!   DB's), `MirrorIo` (filesystem error). The DB-committed entry
//!   is NOT rolled back.
//! - **Bootstrap** (`sync_mirror` when mirror file is absent) —
//!   implicit one-time backfill from the DB. INFO-level log line
//!   `audit_mirror_initialized` names the event loud per ADR-0030
//!   §7 + CLAUDE.md rule 12.
//! - **Read-time surface** (`read_mirror_entries`) — used by the
//!   per-invoice export bundle reader at
//!   `apps/aberp/src/export_invoice_bundle.rs`. Returns the
//!   seq-ordered vector of `MirrorEntry`; the bundle reader
//!   compares against DB entries at the `entry_hash` level.
//!
//! # Per-tenant lock posture (ADR-0030 §6)
//!
//! The DuckDB single-writer file-lock blocks concurrent DB commits;
//! the mirror's `fs2::FileExt::lock_exclusive` blocks concurrent
//! mirror appends. Two ABERP processes that both committed a DB
//! entry serialize on the mirror lock; the second process's
//! `sync_mirror` call sees the first process's append in the file
//! and skips ahead. Cloud multi-writer per ADR-0016 is deferred
//! unchanged.
//!
//! # What this module does and does not do
//!
//! - It DOES NOT couple to `append_in_tx` — the mirror write runs
//!   post-commit at the binary path per ADR-0030 §2 "Surfaced
//!   conflict 1, Reading B".
//! - It DOES NOT define new `EventKind` variants — the mirror
//!   records the same kinds the DB records; F12 four-edit ritual
//!   does NOT fire.
//! - It DOES NOT sign the mirror — F5 attestation signing remains
//!   deferred; the mirror's value is "best-effort secondary
//!   evidence" per ADR-0008 §"Adversarial review" bullet 1.
//! - It DOES NOT auto-sync on read paths — only the binary's post-
//!   commit code path calls `sync_mirror`. The bundle reader uses
//!   `read_mirror_entries` (pure read) and never mutates the
//!   mirror.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;
use duckdb::Connection;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::entry::{Actor, Entry, EntryHash, EntryId, EventKind, Sequence};
use crate::error::AppendError;
use crate::storage::LedgerMeta;

/// The literal filename suffix appended to the DB path to produce
/// the mirror path. Inlined here rather than threaded through a
/// `const PATH_SUFFIX` indirection per CLAUDE.md rule 2 — the
/// suffix never changes.
const MIRROR_PATH_SUFFIX: &str = ".audit.log";

/// Resolve the mirror file path for a given DB file path. The
/// suffix is appended to the full file name (not the
/// extension-only suffix) so `t-1.duckdb` becomes
/// `t-1.duckdb.audit.log` per ADR-0030 §1.
pub fn mirror_path_for(db_path: &Path) -> PathBuf {
    let mut s = db_path.as_os_str().to_owned();
    s.push(MIRROR_PATH_SUFFIX);
    PathBuf::from(s)
}

/// One JSON-Lines record in the mirror file. Public so the bundle
/// reader can compare against DB-sourced [`Entry`] values at the
/// `entry_hash` level (which is the canonical agreement key per
/// ADR-0030 §4).
///
/// Field shape MUST match the bundle's `chain.jsonl` line shape
/// (PR-16's `ChainJsonlEntry`) so the bundle reader's mirror-file
/// consumption path is SYMMETRIC with the DB-sourced consumption
/// path per ADR-0030 §1 + CLAUDE.md rule 7 (one canonical format,
/// two consumers).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MirrorEntry {
    pub id: String,
    pub seq: u64,
    /// Hex-encoded 32-byte SHA-256.
    pub prev_hash: String,
    pub time_wall: String,
    pub time_mono: u64,
    pub actor: Actor,
    /// Hex-encoded 32-byte SHA-256 of the producing binary.
    pub binary_hash: String,
    pub tenant_id: String,
    pub kind: String,
    /// Base64-encoded payload bytes.
    pub payload: String,
    pub idempotency_key: Option<String>,
    /// Hex-encoded 32-byte SHA-256 (the chain link).
    pub entry_hash: String,
}

impl MirrorEntry {
    /// Encode an in-memory [`Entry`] into the JSON-Lines record
    /// shape. Public-crate so [`crate::storage`] and tests can
    /// reuse it.
    pub(crate) fn from_entry(entry: &Entry) -> Result<Self, AppendError> {
        let time_wall = entry.time_wall.format(&Rfc3339)?;
        Ok(Self {
            id: entry.id.to_prefixed_string(),
            seq: entry.seq.as_u64(),
            prev_hash: hex::encode(entry.prev_hash.as_bytes()),
            time_wall,
            time_mono: entry.time_mono,
            actor: entry.actor.clone(),
            binary_hash: hex::encode(entry.binary_hash.as_bytes()),
            tenant_id: entry.tenant_id.as_str().to_string(),
            kind: entry.kind.as_str().to_string(),
            payload: BASE64_STANDARD.encode(&entry.payload),
            idempotency_key: entry.idempotency_key.clone(),
            entry_hash: hex::encode(entry.entry_hash.as_bytes()),
        })
    }

    /// `seq` accessor for the bundle reader's seq-ordered scan.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// `entry_hash` accessor — hex-encoded; the canonical
    /// agreement key per ADR-0030 §4.
    pub fn entry_hash(&self) -> &str {
        &self.entry_hash
    }
}

/// Encode a [`MirrorEntry`] as one JSON-Lines line (terminating
/// `\n` included). Single-line `serde_json::to_string` — NOT
/// `to_string_pretty` — so each entry occupies exactly one line.
fn encode_line(record: &MirrorEntry) -> Result<Vec<u8>, AppendError> {
    let mut bytes = serde_json::to_vec(record)?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Append-only read of the mirror file. Returns the seq-ordered
/// vector of records. ADR-0030 §4.
///
/// # Errors
///
/// - `AppendError::MirrorIo(NotFound)` if the file does not exist.
///   Callers (the bundle reader) treat this as
///   `MirrorAgreementStatus::AbsentPrePr17`.
/// - `AppendError::MirrorIo(_)` for any other I/O failure.
/// - `AppendError::MirrorCorrupt { reason }` if:
///   - any line fails JSON decoding;
///   - the trailing line is non-empty AND lacks a final `\n`;
///   - seqs are non-ascending, non-contiguous from 1, or duplicate.
pub fn read_mirror_entries(mirror_path: &Path) -> Result<Vec<MirrorEntry>, AppendError> {
    let file = File::open(mirror_path).map_err(AppendError::MirrorIo)?;
    let mut reader = BufReader::new(&file);

    // Detect "trailing line lacks newline" by inspecting the last
    // byte of the file before line-iteration. An empty file is OK
    // (no entries yet); a non-empty file with no trailing newline
    // is a partial-write signal per ADR-0030 §3.
    let len = file.metadata().map_err(AppendError::MirrorIo)?.len();
    if len > 0 {
        let mut tail = [0u8; 1];
        let mut last_byte_reader = File::open(mirror_path).map_err(AppendError::MirrorIo)?;
        last_byte_reader
            .seek(SeekFrom::End(-1))
            .map_err(AppendError::MirrorIo)?;
        last_byte_reader
            .read_exact(&mut tail)
            .map_err(AppendError::MirrorIo)?;
        if tail[0] != b'\n' {
            return Err(AppendError::MirrorCorrupt {
                reason: "last line lacks trailing newline — prior write was interrupted; \
                         operator must truncate the partial line before continuing"
                    .to_string(),
            });
        }
    }

    let mut out: Vec<MirrorEntry> = Vec::new();
    let mut line_no: u64 = 0;
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).map_err(AppendError::MirrorIo)?;
        if n == 0 {
            break;
        }
        line_no += 1;
        // Strip the trailing `\n` (and `\r` if a CRLF FS slipped
        // one in) before JSON-decoding.
        let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
        if trimmed.is_empty() {
            return Err(AppendError::MirrorCorrupt {
                reason: format!("empty line at line {line_no}"),
            });
        }
        let record: MirrorEntry =
            serde_json::from_str(trimmed).map_err(|e| AppendError::MirrorCorrupt {
                reason: format!("JSON decode failure at line {line_no}: {e}"),
            })?;
        // Ascending-contiguous seq from 1 — same invariant
        // `verify_chain` enforces on the DB side.
        let expected = (out.len() as u64) + 1;
        if record.seq != expected {
            return Err(AppendError::MirrorCorrupt {
                reason: format!(
                    "seq jump at line {line_no}: expected seq={expected}, found seq={}",
                    record.seq
                ),
            });
        }
        out.push(record);
    }
    Ok(out)
}

/// Synchronise the mirror file to the DB's current head. ADR-0030
/// §2. Called by the binary path after `tx.commit()`.
///
/// Behaviour:
/// - Acquires an exclusive advisory lock on the mirror file
///   (`fs2::FileExt::lock_exclusive`) for the duration of the call;
///   the lock is released on `Drop` of the `File` handle (or
///   explicit unlock in the error paths).
/// - If the mirror file does not exist AND the DB is non-empty,
///   runs the implicit one-time backfill per ADR-0030 §7. Logs at
///   INFO level with `audit_mirror_initialized`.
/// - If the mirror file exists, reads its last line (the "head"),
///   verifies it against the DB's matching entry by `entry_hash`,
///   then appends each DB entry with `seq > mirror_head_seq`.
/// - Returns the new mirror head seq on success.
///
/// # Errors
///
/// - `AppendError::Storage(_)` for DuckDB read failures.
/// - `AppendError::MirrorCorrupt { reason }` per `read_mirror_entries`'s
///   contract, plus any partial-line detection.
/// - `AppendError::MirrorDivergent { seq, reason }` if the
///   mirror's `entry_hash[seq]` disagrees with the DB's
///   corresponding entry. Per ADR-0030 §3 the DB is NOT rolled back.
/// - `AppendError::MirrorIo(_)` for any filesystem I/O failure
///   (open, lock, seek, read, write, fsync).
pub fn sync_mirror(
    conn: &Connection,
    meta: &LedgerMeta,
    mirror_path: &Path,
) -> Result<u64, AppendError> {
    // 1. Open (or create) the mirror file in append+read mode. The
    //    advisory lock is held on this handle for the whole call.
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(mirror_path)
        .map_err(AppendError::MirrorIo)?;
    file.lock_exclusive().map_err(AppendError::MirrorIo)?;

    // 2. Re-stat now that the lock is held — the bytes we read are
    //    the bytes we own. `read_mirror_entries` opens the file
    //    separately for read; that's fine because the lock is
    //    advisory and we hold it on the directory entry.
    let bytes_at_lock = file.metadata().map_err(AppendError::MirrorIo)?.len();

    let mirror_head_seq: u64;
    let mirror_head_hash: Option<String>;

    if bytes_at_lock == 0 {
        // Empty (or just-created) mirror file. Both the "first
        // call ever on a fresh DB" and "implicit backfill on
        // a pre-PR-17 DB" paths land here; the difference is
        // resolved by whether the DB has prior entries (handled
        // below in step 5).
        mirror_head_seq = 0;
        mirror_head_hash = None;
    } else {
        // Read the last line via a tail scan. For typical per-
        // tenant volumes (annual invoice counts for one SME) the
        // mirror is bounded and reading the full file is cheap;
        // we still use the existing `read_mirror_entries`
        // function so the partial-line + non-ascending checks
        // surface uniformly. If hyperscale volume becomes a
        // pattern, F39 (ADR-0029) is the named trigger.
        let entries = read_mirror_entries(mirror_path)?;
        match entries.last() {
            Some(last) => {
                mirror_head_seq = last.seq;
                mirror_head_hash = Some(last.entry_hash.clone());
            }
            None => {
                mirror_head_seq = 0;
                mirror_head_hash = None;
            }
        }
    }

    // 3. Read the DB entries strictly after mirror_head_seq.
    let new_entries = read_db_entries_after(conn, mirror_head_seq)?;

    // 4. If the mirror has a head, verify the DB's matching entry
    //    has the same `entry_hash`. Disagreement is divergence
    //    (CLAUDE.md rule 12 — refuse the next append).
    if let Some(mirror_hash) = mirror_head_hash.as_ref() {
        let db_head_at_mirror = read_db_entry_at_seq(conn, mirror_head_seq)?;
        match db_head_at_mirror {
            None => {
                return Err(AppendError::MirrorDivergent {
                    seq: mirror_head_seq,
                    reason: format!(
                        "DB has no entry at seq={mirror_head_seq} but mirror does — \
                         mirror is ahead of DB; operator must investigate before re-running"
                    ),
                });
            }
            Some(entry) => {
                let db_hash = hex::encode(entry.entry_hash.as_bytes());
                if &db_hash != mirror_hash {
                    return Err(AppendError::MirrorDivergent {
                        seq: mirror_head_seq,
                        reason: format!(
                            "mirror entry_hash={mirror_hash} disagrees with DB entry_hash={db_hash}; \
                             operator must investigate before re-running"
                        ),
                    });
                }
            }
        }
    }

    // 5. Bootstrap detection: empty mirror + non-empty DB = the
    //    implicit one-time backfill path per ADR-0030 §7. LOUD
    //    INFO log line names the event so the operator sees it
    //    in the command's output.
    let bootstrap_count = if mirror_head_seq == 0 && !new_entries.is_empty() {
        new_entries.len()
    } else {
        0
    };

    // 6. Append every new entry as one JSON-Lines line. The
    //    `OpenOptions::append(true)` mode makes each `write_all`
    //    call append-atomic on POSIX (up to PIPE_BUF, which a
    //    single audit line never exceeds in practice). Fsync
    //    once at the end per ADR-0008 §"Storage".
    let mut appended: u64 = 0;
    for entry in &new_entries {
        let record = MirrorEntry::from_entry(entry)?;
        let line = encode_line(&record)?;
        (&file).write_all(&line).map_err(AppendError::MirrorIo)?;
        appended += 1;
    }
    if appended > 0 {
        (&file).flush().map_err(AppendError::MirrorIo)?;
        file.sync_all().map_err(AppendError::MirrorIo)?;
    }

    let new_head_seq = mirror_head_seq + appended;
    let tenant_id_str = meta.tenant_id().as_str();

    if bootstrap_count > 0 {
        tracing::info!(
            tenant = %tenant_id_str,
            mirror_path = %mirror_path.display(),
            entries_backfilled = bootstrap_count,
            new_head_seq,
            "audit_mirror_initialized"
        );
    } else if appended > 0 {
        tracing::debug!(
            tenant = %tenant_id_str,
            mirror_path = %mirror_path.display(),
            entries_appended = appended,
            new_head_seq,
            "audit_mirror_synced"
        );
    }

    // Advisory lock released by `Drop` of `file`.
    Ok(new_head_seq)
}

/// What boot-time reconciliation did to make the mirror consistent
/// with the DB. Session 152b — the mirror is a derivable cache, not a
/// source of truth: between processes, boot restores the invariant
/// instead of letting the next post-commit [`sync_mirror`] 500.
///
/// Each variant carries the entry count so the boot log names the
/// magnitude loudly per CLAUDE.md rule 12.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryAction {
    /// Mirror already agreed with the DB (head seqs equal, last
    /// `entry_hash` matched). Idempotent no-op.
    Unchanged,
    /// Mirror file was absent; created fresh from DB entries
    /// `[1..=db_max_seq]`.
    Created { entries_written: u64 },
    /// Mirror was behind the DB; replayed the missing DB entries
    /// `[mirror_max_seq+1..=db_max_seq]`.
    Extended { entries_added: u64 },
    /// Mirror was the same length as the DB but its last `entry_hash`
    /// disagreed (or the mirror file was corrupt/unparseable). Full
    /// rebuild from the DB.
    Rebuilt { entries_written: u64 },
}

/// Boot-time reconciliation of the mirror against the DB. Session
/// 152b / Part A. Called once per process at serve boot AFTER
/// [`crate::ensure_schema`] succeeds, and BEFORE any request can
/// trigger a per-write [`sync_mirror`].
///
/// The DB is the source of truth; the mirror is a derivable cache.
/// This function restores the between-process invariant
/// "mirror == DB" without ever mutating a DB entry. The decision
/// tree (Part B):
///
/// - mirror file missing → create fresh from DB → [`RecoveryAction::Created`]
/// - mirror behind DB → replay missing entries → [`RecoveryAction::Extended`]
/// - mirror ahead of DB → preserve the ahead mirror + REFUSE
///   ([`AppendError::MirrorAheadOfDb`]) — never silently truncated
/// - equal length, last hash matches → [`RecoveryAction::Unchanged`]
/// - equal length, last hash differs, OR mirror corrupt → full
///   rebuild → [`RecoveryAction::Rebuilt`]
///
/// Idempotent: a second call on a healthy state returns
/// [`RecoveryAction::Unchanged`].
///
/// # Errors
///
/// - `AppendError::Storage(_)` for DuckDB read failures.
/// - `AppendError::MirrorIo(_)` for filesystem I/O failures OTHER than
///   `NotFound` (a `NotFound` is the "missing mirror" case, handled
///   as `Created`). A disk/permission failure is loud, not silently
///   "recovered".
///
/// A `MirrorCorrupt` from the read path is NOT surfaced — it is
/// reinterpreted as "rebuild the cache" (the whole point of treating
/// the mirror as derivable).
pub fn ensure_consistent_with_db(
    conn: &Connection,
    mirror_path: &Path,
) -> Result<RecoveryAction, AppendError> {
    let db_max_seq = read_db_max_seq(conn)?;

    // Read the mirror. Missing → Created. Corrupt/unparseable →
    // Rebuilt. Any other I/O error (permissions, disk) is loud.
    let mirror_entries = match read_mirror_entries(mirror_path) {
        Ok(entries) => entries,
        Err(AppendError::MirrorIo(io)) if io.kind() == std::io::ErrorKind::NotFound => {
            let written = rebuild_mirror_from_db(conn, mirror_path)?;
            tracing::info!(
                mirror_path = %mirror_path.display(),
                entries_written = written,
                db_max_seq,
                "audit_mirror_recovered action=created (mirror file was absent)"
            );
            return Ok(RecoveryAction::Created {
                entries_written: written,
            });
        }
        Err(AppendError::MirrorCorrupt { reason }) => {
            let written = rebuild_mirror_from_db(conn, mirror_path)?;
            tracing::warn!(
                mirror_path = %mirror_path.display(),
                entries_written = written,
                db_max_seq,
                %reason,
                "audit_mirror_recovered action=rebuilt (mirror file was corrupt)"
            );
            return Ok(RecoveryAction::Rebuilt {
                entries_written: written,
            });
        }
        Err(other) => return Err(other),
    };

    let mirror_max_seq = mirror_entries.last().map(|e| e.seq).unwrap_or(0);

    if mirror_max_seq < db_max_seq {
        let added = append_db_entries_after(conn, mirror_path, mirror_max_seq)?;
        tracing::info!(
            mirror_path = %mirror_path.display(),
            mirror_max_seq,
            db_max_seq,
            entries_added = added,
            "audit_mirror_recovered action=extended (mirror was behind DB)"
        );
        Ok(RecoveryAction::Extended {
            entries_added: added,
        })
    } else if mirror_max_seq > db_max_seq {
        // The mirror is AHEAD of the DB — the fingerprint of a torn-write /
        // lost DB commit (the 2026-06-22 corruption class) or a dev DB-nuke.
        // ADR-0093 chunk 3 / ADR-0082 reconcile safety: NEVER silently
        // truncate (that destroys the only surviving record of what the DB
        // lost). Preserve the ahead mirror to a side file FIRST, then
        // refuse-and-surface so a human investigates before any rebuild.
        let entries_ahead = mirror_max_seq - db_max_seq;
        let preserved = preserve_ahead_mirror(mirror_path)?;
        tracing::error!(
            mirror_path = %mirror_path.display(),
            mirror_max_seq,
            db_max_seq,
            entries_ahead,
            preserved = %preserved.display(),
            "audit_mirror_AHEAD_of_db — REFUSING to auto-truncate; preserved the ahead \
             mirror and surfacing (possible lost DB commit — investigate before re-running)"
        );
        Err(AppendError::MirrorAheadOfDb {
            mirror_max_seq,
            db_max_seq,
            preserved: preserved.display().to_string(),
        })
    } else if db_max_seq == 0 {
        // Both empty (mirror file present but zero entries, DB empty).
        Ok(RecoveryAction::Unchanged)
    } else {
        // Equal non-zero length: compare last entry_hash. The chain
        // is a hash chain, so the head hash is a sound proxy for the
        // whole prefix's integrity (Part B "equal" branch).
        let db_head = read_db_entry_at_seq(conn, db_max_seq)?;
        let db_hash = db_head.map(|e| hex::encode(e.entry_hash.as_bytes()));
        let mirror_hash = mirror_entries.last().map(|e| e.entry_hash.clone());
        if db_hash == mirror_hash {
            Ok(RecoveryAction::Unchanged)
        } else {
            let written = rebuild_mirror_from_db(conn, mirror_path)?;
            tracing::warn!(
                mirror_path = %mirror_path.display(),
                db_max_seq,
                entries_written = written,
                "audit_mirror_recovered action=rebuilt (head entry_hash disagreed with DB)"
            );
            Ok(RecoveryAction::Rebuilt {
                entries_written: written,
            })
        }
    }
}

/// Read the DB's max entry seq (0 if the table is empty). Reuses the
/// storage layer's `SELECT_HEAD` projection.
fn read_db_max_seq(conn: &Connection) -> Result<u64, AppendError> {
    let mut stmt = conn.prepare(crate::storage::schema::SELECT_HEAD)?;
    let mut rows = stmt.query_map([], row_to_entry_for_mirror)?;
    match rows.next() {
        Some(r) => Ok(r?.seq.as_u64()),
        None => Ok(0),
    }
}

/// Preserve the current (AHEAD-of-DB) mirror to a timestamped side file so
/// the evidence of what the DB lost is never destroyed (ADR-0093 chunk 3 /
/// ADR-0082 reconcile safety). A byte-for-byte copy to
/// `<mirror>.ahead-<nanos>.bak`; the original mirror is left in place, so
/// the boot reconcile keeps surfacing the AHEAD condition until a human
/// resolves it. Returns the backup path for the surfaced error message.
fn preserve_ahead_mirror(mirror_path: &Path) -> Result<PathBuf, AppendError> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut os = mirror_path.as_os_str().to_owned();
    os.push(format!(".ahead-{nanos}.bak"));
    let backup = PathBuf::from(os);
    std::fs::copy(mirror_path, &backup).map_err(AppendError::MirrorIo)?;
    Ok(backup)
}

/// Truncate the mirror and rewrite it from the DB's full entry set
/// `[1..=db_max_seq]`. Used by the Created and Rebuilt recovery paths — in
/// both `up_to == db_max_seq`, so the full DB scan IS `[1..=db_max_seq]`.
/// Returns the entry count written. (The mirror-ahead case no longer
/// rebuilds: it preserves + refuses via [`preserve_ahead_mirror`].)
fn rebuild_mirror_from_db(conn: &Connection, mirror_path: &Path) -> Result<u64, AppendError> {
    let entries = read_db_entries_after(conn, 0)?;
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .read(true)
        .open(mirror_path)
        .map_err(AppendError::MirrorIo)?;
    file.lock_exclusive().map_err(AppendError::MirrorIo)?;
    let mut written: u64 = 0;
    for entry in &entries {
        let record = MirrorEntry::from_entry(entry)?;
        let line = encode_line(&record)?;
        (&file).write_all(&line).map_err(AppendError::MirrorIo)?;
        written += 1;
    }
    (&file).flush().map_err(AppendError::MirrorIo)?;
    file.sync_all().map_err(AppendError::MirrorIo)?;
    Ok(written)
}

/// Append DB entries with `seq > after_seq` to the existing mirror.
/// The Extended recovery path. Returns the count appended.
fn append_db_entries_after(
    conn: &Connection,
    mirror_path: &Path,
    after_seq: u64,
) -> Result<u64, AppendError> {
    let entries = read_db_entries_after(conn, after_seq)?;
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(mirror_path)
        .map_err(AppendError::MirrorIo)?;
    file.lock_exclusive().map_err(AppendError::MirrorIo)?;
    let mut added: u64 = 0;
    for entry in &entries {
        let record = MirrorEntry::from_entry(entry)?;
        let line = encode_line(&record)?;
        (&file).write_all(&line).map_err(AppendError::MirrorIo)?;
        added += 1;
    }
    if added > 0 {
        (&file).flush().map_err(AppendError::MirrorIo)?;
        file.sync_all().map_err(AppendError::MirrorIo)?;
    }
    Ok(added)
}

/// Read DB entries with `seq > after_seq`, in ascending seq order.
/// Mirror-internal helper; mirrors `Ledger::entries` but with a
/// seq-bound filter so the sync path doesn't load the full ledger
/// each time.
fn read_db_entries_after(conn: &Connection, after_seq: u64) -> Result<Vec<Entry>, AppendError> {
    let mut stmt = conn.prepare(SELECT_AFTER_SEQ)?;
    let rows = stmt.query_map([after_seq as i64], row_to_entry_for_mirror)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Read the DB entry at the given seq (if present). Used by the
/// mirror's divergence check.
fn read_db_entry_at_seq(conn: &Connection, seq: u64) -> Result<Option<Entry>, AppendError> {
    let mut stmt = conn.prepare(SELECT_AT_SEQ)?;
    let mut rows = stmt.query_map([seq as i64], row_to_entry_for_mirror)?;
    match rows.next() {
        Some(r) => Ok(Some(r?)),
        None => Ok(None),
    }
}

/// Local mirror of the storage-layer `row_to_entry` decoder. Kept
/// here because making the storage decoder `pub(crate)` would widen
/// the crate's internal API surface unnecessarily; the row shape is
/// stable (it matches the `schema::CREATE_TABLE` column order) and
/// the duplication is small (~30 lines).
fn row_to_entry_for_mirror(row: &duckdb::Row<'_>) -> duckdb::Result<Entry> {
    use crate::entry::{BinaryHash, TenantId};
    use ulid::Ulid;

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

    let id_ulid_str = id_prefixed
        .strip_prefix("aud_")
        .ok_or_else(|| decode_err("entry id missing `aud_` prefix"))?;
    let id_ulid = Ulid::from_string(id_ulid_str)
        .map_err(|_| decode_err("entry id is not a valid Crockford-base32 ULID"))?;

    let prev_hash = to_hash32(&prev_hash_blob, "prev_hash")?;
    let binary_hash = to_hash32(&binary_hash_blob, "binary_hash")?;
    let entry_hash = to_hash32(&entry_hash_blob, "entry_hash")?;

    let tenant_id = TenantId::new(tenant_str)
        .ok_or_else(|| decode_err("tenant_id is empty or contains a null byte"))?;
    let time_wall = OffsetDateTime::parse(&time_wall_str, &Rfc3339)
        .map_err(|_| decode_err("time_wall is not RFC3339"))?;
    let actor = Actor::from_storage_json(&actor_json)
        .map_err(|_| decode_err("actor JSON failed to deserialize"))?;
    let kind =
        EventKind::from_storage_str(&kind_str).map_err(|_| decode_err("unknown event kind"))?;

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
        // S441 — the ADR-0030 mirror is a hash-chain DIVERGENCE detector and
        // does not carry the session-signing columns; mirror-decoded entries
        // read None. The `entry_hash` still matches the DB (the session
        // fields are excluded from the canonical preimage), so mirror
        // consistency checks are unaffected.
        session_id: None,
        session_pubkey: None,
        event_sig: None,
    })
}

fn to_hash32(blob: &[u8], field: &'static str) -> duckdb::Result<[u8; 32]> {
    if blob.len() != 32 {
        return Err(decode_err_owned(format!(
            "{field} blob has length {} (expected 32)",
            blob.len()
        )));
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(blob);
    Ok(out)
}

fn decode_err(msg: &'static str) -> duckdb::Error {
    duckdb::Error::FromSqlConversionFailure(
        0,
        duckdb::types::Type::Text,
        Box::<dyn std::error::Error + Send + Sync>::from(msg),
    )
}

fn decode_err_owned(msg: String) -> duckdb::Error {
    duckdb::Error::FromSqlConversionFailure(
        0,
        duckdb::types::Type::Text,
        Box::<dyn std::error::Error + Send + Sync>::from(msg),
    )
}

// SQL constants for the mirror's DB reads. Same column projection
// as `schema::SELECT_ALL`; differs only in the `WHERE seq > ?`
// (after-seq) or `WHERE seq = ?` (at-seq) clause.

const SELECT_AFTER_SEQ: &str = "
SELECT id, seq, prev_hash, time_wall, time_mono, actor,
       binary_hash, tenant_id, kind, payload, idempotency_key, entry_hash
FROM audit_ledger
WHERE seq > ?
ORDER BY seq ASC;
";

const SELECT_AT_SEQ: &str = "
SELECT id, seq, prev_hash, time_wall, time_mono, actor,
       binary_hash, tenant_id, kind, payload, idempotency_key, entry_hash
FROM audit_ledger
WHERE seq = ?
LIMIT 1;
";

// ──────────────────────────────────────────────────────────────────────
// Unit tests — path resolution, line encoding, partial-line detection,
// divergence detection, bootstrap path, idempotent re-sync.
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::{Actor, BinaryHash, TenantId};
    use crate::storage::{append_in_tx, ensure_schema, LedgerMeta};

    fn mk_meta() -> LedgerMeta {
        LedgerMeta::new(
            TenantId::new("t-1").unwrap(),
            BinaryHash::from_bytes([0u8; 32]),
        )
    }

    fn open_conn_with_two_entries() -> (Connection, LedgerMeta) {
        let mut conn = Connection::open_in_memory().unwrap();
        ensure_schema(&conn).unwrap();
        let meta = mk_meta();
        {
            let tx = conn.transaction().unwrap();
            append_in_tx(
                &tx,
                &meta,
                EventKind::Test,
                b"payload-1".to_vec(),
                Actor::test_only(),
                Some("idem-1".to_string()),
            )
            .unwrap();
            append_in_tx(
                &tx,
                &meta,
                EventKind::Test,
                b"payload-2".to_vec(),
                Actor::test_only(),
                Some("idem-2".to_string()),
            )
            .unwrap();
            tx.commit().unwrap();
        }
        (conn, meta)
    }

    fn append_one(conn: &mut Connection, meta: &LedgerMeta, idem_tag: &str, payload: &[u8]) {
        let tx = conn.transaction().unwrap();
        append_in_tx(
            &tx,
            meta,
            EventKind::Test,
            payload.to_vec(),
            Actor::test_only(),
            Some(idem_tag.to_string()),
        )
        .unwrap();
        tx.commit().unwrap();
    }

    #[test]
    fn mirror_path_appends_audit_log_suffix_to_full_db_filename() {
        let db = Path::new("/var/aberp/t-1.duckdb");
        let mirror = mirror_path_for(db);
        assert_eq!(mirror, Path::new("/var/aberp/t-1.duckdb.audit.log"));
    }

    #[test]
    fn mirror_path_handles_db_path_without_extension() {
        let db = Path::new("/tmp/tenant-db");
        let mirror = mirror_path_for(db);
        assert_eq!(mirror, Path::new("/tmp/tenant-db.audit.log"));
    }

    #[test]
    fn read_mirror_entries_returns_notfound_when_file_absent() {
        let dir = tempdir_under_target();
        let mirror = dir.join("absent.audit.log");
        let err = read_mirror_entries(&mirror).unwrap_err();
        match err {
            AppendError::MirrorIo(io) => {
                assert_eq!(io.kind(), std::io::ErrorKind::NotFound);
            }
            other => panic!("expected MirrorIo(NotFound), got {other:?}"),
        }
        // cleanup: tempdir_under_target leaves the dir; remove it.
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_mirror_entries_rejects_partial_trailing_line() {
        let dir = tempdir_under_target();
        let mirror = dir.join("partial.audit.log");
        std::fs::write(&mirror, b"{\"seq\":1,\"partial-no-newline\":true}").unwrap();
        let err = read_mirror_entries(&mirror).unwrap_err();
        match err {
            AppendError::MirrorCorrupt { reason } => {
                assert!(
                    reason.contains("trailing newline"),
                    "expected partial-line message, got {reason}"
                );
            }
            other => panic!("expected MirrorCorrupt, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sync_mirror_bootstrap_backfills_existing_db_entries() {
        let dir = tempdir_under_target();
        let mirror = dir.join("bootstrap.audit.log");
        let (conn, meta) = open_conn_with_two_entries();

        // Mirror does not exist yet. First sync should backfill
        // both DB entries.
        let head = sync_mirror(&conn, &meta, &mirror).unwrap();
        assert_eq!(head, 2);

        let entries = read_mirror_entries(&mirror).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].seq, 1);
        assert_eq!(entries[1].seq, 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sync_mirror_is_idempotent_when_no_new_entries() {
        let dir = tempdir_under_target();
        let mirror = dir.join("idempotent.audit.log");
        let (conn, meta) = open_conn_with_two_entries();
        let head1 = sync_mirror(&conn, &meta, &mirror).unwrap();
        let head2 = sync_mirror(&conn, &meta, &mirror).unwrap();
        assert_eq!(head1, 2);
        assert_eq!(head2, 2);
        let entries = read_mirror_entries(&mirror).unwrap();
        assert_eq!(entries.len(), 2, "second sync must not duplicate entries");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sync_mirror_appends_only_new_entries_on_second_call() {
        let dir = tempdir_under_target();
        let mirror = dir.join("incremental.audit.log");
        let (mut conn, meta) = open_conn_with_two_entries();
        let head_after_first = sync_mirror(&conn, &meta, &mirror).unwrap();
        assert_eq!(head_after_first, 2);

        // Append a third DB entry. Re-sync.
        append_one(&mut conn, &meta, "idem-3", b"payload-3");

        let head_after_second = sync_mirror(&conn, &meta, &mirror).unwrap();
        assert_eq!(head_after_second, 3);

        let entries = read_mirror_entries(&mirror).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[2].seq, 3);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sync_mirror_detects_divergence_when_mirror_hash_disagrees_with_db() {
        let dir = tempdir_under_target();
        let mirror = dir.join("divergent.audit.log");
        let (mut conn, meta) = open_conn_with_two_entries();
        sync_mirror(&conn, &meta, &mirror).unwrap();

        // Mutate the mirror's entry_hash on seq=2 to simulate
        // divergence. The mirror is plain JSON-Lines, so we
        // re-read, mutate, and re-write.
        let entries = read_mirror_entries(&mirror).unwrap();
        let mut tampered = entries.clone();
        tampered[1].entry_hash = "00".repeat(32);
        let mut tampered_bytes = Vec::new();
        for r in &tampered {
            tampered_bytes.extend_from_slice(&encode_line(r).unwrap());
        }
        std::fs::write(&mirror, &tampered_bytes).unwrap();

        // Append a third DB entry so sync_mirror has a reason to
        // run + a head to check.
        append_one(&mut conn, &meta, "idem-3", b"payload-3");

        let err = sync_mirror(&conn, &meta, &mirror).unwrap_err();
        match err {
            AppendError::MirrorDivergent { seq, .. } => {
                assert_eq!(seq, 2, "divergence should land at the disagreeing seq");
            }
            other => panic!("expected MirrorDivergent, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sync_mirror_loud_fails_on_partial_trailing_line() {
        let dir = tempdir_under_target();
        let mirror = dir.join("partial-sync.audit.log");
        let (conn, meta) = open_conn_with_two_entries();
        sync_mirror(&conn, &meta, &mirror).unwrap();

        // Truncate the trailing newline to simulate an
        // interrupted prior write.
        let bytes = std::fs::read(&mirror).unwrap();
        assert!(bytes.last().copied() == Some(b'\n'));
        std::fs::write(&mirror, &bytes[..bytes.len() - 1]).unwrap();

        let err = sync_mirror(&conn, &meta, &mirror).unwrap_err();
        match err {
            AppendError::MirrorCorrupt { reason } => {
                assert!(reason.contains("trailing newline"));
            }
            other => panic!("expected MirrorCorrupt, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mirror_entry_round_trips_through_jsonl_encoding() {
        // One handcrafted Entry; encode to mirror line; decode
        // back via read_mirror_entries; compare canonical fields.
        let dir = tempdir_under_target();
        let mirror = dir.join("roundtrip.audit.log");
        let (conn, meta) = open_conn_with_two_entries();
        sync_mirror(&conn, &meta, &mirror).unwrap();
        let entries = read_mirror_entries(&mirror).unwrap();
        assert_eq!(entries.len(), 2);
        // Re-encode the first entry's mirror record; the line we
        // get out must exactly match the bytes already on disk
        // (modulo the trailing newline, which encode_line
        // includes).
        let re_encoded = encode_line(&entries[0]).unwrap();
        let file_bytes = std::fs::read(&mirror).unwrap();
        assert!(
            file_bytes.starts_with(&re_encoded),
            "encoded line must match the bytes on disk"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ──────────────────────────────────────────────────────────────
    // Session 152b — boot-time `ensure_consistent_with_db` recovery.
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn ensure_consistent_creates_empty_mirror_on_fresh_db() {
        // Fresh DB + no mirror file → create (empty) mirror, Created{0}.
        let dir = tempdir_under_target();
        let mirror = dir.join("fresh.audit.log");
        let conn = Connection::open_in_memory().unwrap();
        ensure_schema(&conn).unwrap();

        let action = ensure_consistent_with_db(&conn, &mirror).unwrap();
        assert_eq!(action, RecoveryAction::Created { entries_written: 0 });
        assert!(mirror.exists(), "mirror file must be created");
        assert_eq!(read_mirror_entries(&mirror).unwrap().len(), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_consistent_creates_mirror_backfilled_from_db() {
        // DB has entries, mirror absent → create + backfill, Created{2}.
        let dir = tempdir_under_target();
        let mirror = dir.join("missing.audit.log");
        let (conn, _meta) = open_conn_with_two_entries();
        assert!(!mirror.exists());

        let action = ensure_consistent_with_db(&conn, &mirror).unwrap();
        assert_eq!(action, RecoveryAction::Created { entries_written: 2 });
        let entries = read_mirror_entries(&mirror).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].seq, 1);
        assert_eq!(entries[1].seq, 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_consistent_unchanged_when_mirror_in_sync() {
        // DB + mirror in sync → Unchanged.
        let dir = tempdir_under_target();
        let mirror = dir.join("insync.audit.log");
        let (conn, meta) = open_conn_with_two_entries();
        sync_mirror(&conn, &meta, &mirror).unwrap();

        let action = ensure_consistent_with_db(&conn, &mirror).unwrap();
        assert_eq!(action, RecoveryAction::Unchanged);
        assert_eq!(read_mirror_entries(&mirror).unwrap().len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_consistent_extends_when_mirror_behind_db() {
        // DB ahead of mirror (mirror was synced, then DB grew) →
        // replay missing entries, Extended{count}.
        let dir = tempdir_under_target();
        let mirror = dir.join("behind.audit.log");
        let (mut conn, meta) = open_conn_with_two_entries();
        sync_mirror(&conn, &meta, &mirror).unwrap();
        assert_eq!(read_mirror_entries(&mirror).unwrap().len(), 2);

        // DB grows to 4 while the mirror stays at 2.
        append_one(&mut conn, &meta, "idem-3", b"payload-3");
        append_one(&mut conn, &meta, "idem-4", b"payload-4");

        let action = ensure_consistent_with_db(&conn, &mirror).unwrap();
        assert_eq!(action, RecoveryAction::Extended { entries_added: 2 });
        let entries = read_mirror_entries(&mirror).unwrap();
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[3].seq, 4);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_consistent_refuses_and_preserves_when_mirror_ahead_of_db() {
        // The mirror is AHEAD of the DB (old DB had 2 entries the mirror
        // synced to; the DB now has only 1 — a torn-write / lost commit, or
        // a dev DB-nuke). Chunk-3 reconcile safety: boot must NOT silently
        // truncate the ahead mirror (that would destroy the only record of
        // what the DB lost). It preserves the ahead mirror to a side file
        // and REFUSES (surfaces) so a human investigates.
        let dir = tempdir_under_target();
        let mirror = dir.join("ahead.audit.log");
        let (conn_old, meta_old) = open_conn_with_two_entries();
        sync_mirror(&conn_old, &meta_old, &mirror).unwrap();
        assert_eq!(read_mirror_entries(&mirror).unwrap().len(), 2);
        let before = std::fs::read(&mirror).unwrap();

        let mut conn_fresh = Connection::open_in_memory().unwrap();
        ensure_schema(&conn_fresh).unwrap();
        let meta_fresh = mk_meta();
        append_one(&mut conn_fresh, &meta_fresh, "fresh-1", b"fresh-payload-1");

        let err = ensure_consistent_with_db(&conn_fresh, &mirror)
            .expect_err("mirror ahead of DB must REFUSE, never silently truncate");
        match err {
            AppendError::MirrorAheadOfDb {
                mirror_max_seq,
                db_max_seq,
                preserved,
            } => {
                assert_eq!(mirror_max_seq, 2);
                assert_eq!(db_max_seq, 1);
                // The ahead mirror was preserved byte-for-byte as evidence.
                let backup = std::fs::read(&preserved).expect("preserved backup exists");
                assert_eq!(backup, before, "backup must be the intact ahead mirror");
            }
            other => panic!("expected MirrorAheadOfDb, got {other:?}"),
        }

        // The LIVE mirror is NOT truncated — recovery evidence survives, and
        // the next boot keeps surfacing the AHEAD condition until resolved.
        assert_eq!(
            read_mirror_entries(&mirror).unwrap().len(),
            2,
            "the ahead mirror must be left intact (never auto-truncated)"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_consistent_rebuilds_on_head_hash_mismatch() {
        // Equal length but mirror head entry_hash disagrees → Rebuilt.
        let dir = tempdir_under_target();
        let mirror = dir.join("mismatch.audit.log");
        let (conn, meta) = open_conn_with_two_entries();
        sync_mirror(&conn, &meta, &mirror).unwrap();

        let entries = read_mirror_entries(&mirror).unwrap();
        let mut tampered = entries.clone();
        tampered[1].entry_hash = "00".repeat(32);
        let mut bytes = Vec::new();
        for r in &tampered {
            bytes.extend_from_slice(&encode_line(r).unwrap());
        }
        std::fs::write(&mirror, &bytes).unwrap();

        let action = ensure_consistent_with_db(&conn, &mirror).unwrap();
        assert_eq!(action, RecoveryAction::Rebuilt { entries_written: 2 });
        let rebuilt = read_mirror_entries(&mirror).unwrap();
        assert_eq!(rebuilt.len(), 2);
        let db_head = read_db_entry_at_seq(&conn, 2).unwrap().unwrap();
        assert_eq!(
            rebuilt[1].entry_hash,
            hex::encode(db_head.entry_hash.as_bytes()),
            "rebuilt head must match DB head"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_consistent_rebuilds_on_corrupt_mirror() {
        // A corrupt mirror (partial trailing line) is reinterpreted as
        // "rebuild the cache" rather than surfaced as an error — the
        // whole point of treating the mirror as derivable.
        let dir = tempdir_under_target();
        let mirror = dir.join("corrupt.audit.log");
        let (conn, meta) = open_conn_with_two_entries();
        sync_mirror(&conn, &meta, &mirror).unwrap();

        let bytes = std::fs::read(&mirror).unwrap();
        assert_eq!(bytes.last().copied(), Some(b'\n'));
        std::fs::write(&mirror, &bytes[..bytes.len() - 1]).unwrap();

        let action = ensure_consistent_with_db(&conn, &mirror).unwrap();
        assert_eq!(action, RecoveryAction::Rebuilt { entries_written: 2 });
        assert_eq!(read_mirror_entries(&mirror).unwrap().len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_consistent_is_idempotent() {
        // Run twice: first Created, second Unchanged.
        let dir = tempdir_under_target();
        let mirror = dir.join("idem-recover.audit.log");
        let (conn, _meta) = open_conn_with_two_entries();

        let first = ensure_consistent_with_db(&conn, &mirror).unwrap();
        assert_eq!(first, RecoveryAction::Created { entries_written: 2 });
        let second = ensure_consistent_with_db(&conn, &mirror).unwrap();
        assert_eq!(second, RecoveryAction::Unchanged);
        assert_eq!(read_mirror_entries(&mirror).unwrap().len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `CARGO_TARGET_TMPDIR` is the canonical per-crate temp dir
    /// for tests. Falls back to `std::env::temp_dir()` if unset
    /// (e.g., out-of-cargo invocations). Returns a fresh
    /// subdirectory unique to this test invocation.
    ///
    /// The suffix combines `process::id()` (cross-process guard,
    /// so parallel integration-test binaries sharing
    /// `CARGO_TARGET_TMPDIR` do not collide) with a monotonic
    /// `AtomicUsize` (within-process guard, so parallel
    /// `#[test]` threads do not collide). A `SystemTime`-based
    /// suffix is not safe here: two threads can sample the same
    /// nanosecond on a fast machine and produce the same path.
    fn tempdir_under_target() -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let base = std::env::var_os("CARGO_TARGET_TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let unique = format!(
            "aberp-mirror-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
        );
        let dir = base.join(unique);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
