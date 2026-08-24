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
use duckdb::{params, Connection};
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

// ───────────────────────────────────────────────────────────────────────────
// ADR-0098 R1 — unified torn-tail policy (Fable-5 findings D + E).
//
// A crash during a mirror append leaves the commonest artifact: EXACTLY ONE
// unterminated trailing line ("the append never durably happened", ADR-0030
// §3). The strict `read_mirror_entries` correctly rejects it — but its two
// recovery-sensitive callers historically took OPPOSITE, both-wrong stances:
// the boot reconciler (`ensure_consistent_with_db`) SILENTLY
// `rebuild_mirror_from_db` (`.truncate(true)`), destroying an intact prefix
// that may hold entries the DB lost via a dropped WAL tail (finding D); the
// recovery mirror-read (`aberp_snapshot::recover_or_refuse`) HARD-REFUSED,
// bricking auto-recovery and demanding operator JSONL hand-surgery (finding E).
//
// `read_mirror_under_tail_policy` is the ONE code path both now share: it
// PRESERVES the original first, then trims a lone torn tail and continues, or
// refuses on anything deeper. It NEVER silently rebuilds-from-DB and NEVER
// truncates a prefix that may hold entries the DB lacks.
// ───────────────────────────────────────────────────────────────────────────

/// The three-way classification of a mirror file under the unified torn-tail
/// policy, returned by [`read_mirror_under_tail_policy`]. The boot reconciler
/// [`ensure_consistent_with_db`] and `aberp_snapshot::recover_or_refuse` both
/// route on it, so the two take ONE coherent stance on a torn trailing line.
#[derive(Debug)]
pub enum MirrorTailPolicy {
    /// Parsed clean — no corruption. Carries the entries.
    Clean(Vec<MirrorEntry>),
    /// EXACTLY one unterminated/partial FINAL line — a torn tail. The original
    /// was PRESERVED to `preserved`, the file was durably trimmed to the
    /// verified-intact prefix, and `entries` are that prefix. The caller
    /// CONTINUES (loud log + audit event); the boot arm then reconciles the
    /// trimmed head against the DB, so a still-ahead trimmed mirror trips the
    /// P0 ahead-of-DB preserve+refuse rather than being silently accepted.
    TornTail {
        entries: Vec<MirrorEntry>,
        preserved: PathBuf,
        dropped_bytes: u64,
    },
    /// Corruption DEEPER than a torn tail (a break/gap/JSON/chain mismatch NOT
    /// at the final line). The original was PRESERVED to `preserved`. The
    /// caller REFUSES — never rebuild-from-DB, never hand-edit the JSONL.
    DeepCorrupt { preserved: PathBuf, reason: String },
}

/// PURE torn-tail decision core (ADR-0098 R1), I/O- and serde-free so it is
/// faithfully unit-testable without a filesystem or DuckDB (the saw-off gate,
/// which cannot build the bundled libduckdb amalgamation).
///
/// * `terminated` — the mirror's last byte is `\n` (no partial trailing line).
/// * `prefix_ok`  — the newline-terminated PREFIX region parses AND
///   re-verifies (JSON valid, seq ascending-contiguous from 1, hash-chain
///   links intact).
///
/// The four combinations map to exactly one disposition:
///
/// | `terminated` | `prefix_ok` | disposition |
/// |--------------|-------------|-------------|
/// | true         | true        | `Clean`    — fully terminated, prefix intact |
/// | true         | false       | `Deep`     — a COMPLETE line is broken (not a torn tail) |
/// | false        | true        | `TornTail` — lone partial final line, prefix intact |
/// | false        | false       | `Deep`     — partial final line AND a deeper break |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TailDecision {
    Clean,
    TornTail,
    Deep,
}

/// The pure R1 torn-tail branch (see [`TailDecision`]). Copied verbatim into
/// the saw-off gate's `rustc --test` extraction.
pub(crate) fn decide_tail(terminated: bool, prefix_ok: bool) -> TailDecision {
    match (terminated, prefix_ok) {
        (true, true) => TailDecision::Clean,
        (false, true) => TailDecision::TornTail,
        (_, false) => TailDecision::Deep,
    }
}

/// STRICT parse + hash-chain RE-VERIFICATION of a newline-terminated mirror
/// region. Same JSON + ascending-contiguous-seq-from-1 invariant as
/// [`read_mirror_entries`], PLUS a chain-LINK check (each entry's `prev_hash`
/// equals the previous entry's `entry_hash`) — the "re-verify the chain over
/// the trimmed prefix" R1 requires before it will accept a torn-tail trim. An
/// empty region is vacuously clean (`Ok(vec![])`).
fn parse_and_reverify_prefix(prefix: &[u8]) -> Result<Vec<MirrorEntry>, String> {
    let mut out: Vec<MirrorEntry> = Vec::new();
    for (idx, raw) in prefix.split_inclusive(|&b| b == b'\n').enumerate() {
        let line_no = idx as u64 + 1;
        let text = std::str::from_utf8(raw)
            .map_err(|e| format!("non-UTF8 bytes at line {line_no}: {e}"))?;
        let trimmed = text.trim_end_matches('\n').trim_end_matches('\r');
        if trimmed.is_empty() {
            return Err(format!("empty line at line {line_no}"));
        }
        let record: MirrorEntry = serde_json::from_str(trimmed)
            .map_err(|e| format!("JSON decode failure at line {line_no}: {e}"))?;
        let expected = out.len() as u64 + 1;
        if record.seq != expected {
            return Err(format!(
                "seq jump at line {line_no}: expected seq={expected}, found seq={}",
                record.seq
            ));
        }
        if let Some(prev) = out.last() {
            if record.prev_hash != prev.entry_hash {
                return Err(format!(
                    "hash-chain break at seq {}: prev_hash does not match the seq {} entry_hash",
                    record.seq, prev.seq
                ));
            }
        }
        out.push(record);
    }
    Ok(out)
}

/// Split the mirror bytes at the last `\n`, strictly parse+re-verify the
/// terminated prefix, and classify. Returns `(decision, prefix_entries,
/// prefix_len_bytes, deep_reason)`. The serde/parse half is exercised on the
/// Mac/CI gate; the pure branch structure is [`decide_tail`].
fn classify_mirror_bytes(bytes: &[u8]) -> (TailDecision, Vec<MirrorEntry>, usize, Option<String>) {
    if bytes.is_empty() {
        return (TailDecision::Clean, Vec::new(), 0, None);
    }
    let terminated = bytes.last() == Some(&b'\n');
    // The terminated prefix = bytes up to AND INCLUDING the last `\n` (empty if
    // the whole file is a single unterminated line).
    let prefix_len = match bytes.iter().rposition(|&b| b == b'\n') {
        Some(i) => i + 1,
        None => 0,
    };
    match parse_and_reverify_prefix(&bytes[..prefix_len]) {
        Ok(entries) => (decide_tail(terminated, true), entries, prefix_len, None),
        Err(reason) => (
            decide_tail(terminated, false),
            Vec::new(),
            prefix_len,
            Some(reason),
        ),
    }
}

/// Read the mirror under the unified torn-tail policy (ADR-0098 R1) — the ONE
/// code path shared by the boot reconciler ([`ensure_consistent_with_db`]) and
/// the recovery mirror-read (`aberp_snapshot::recover_or_refuse`), so the two
/// are coherent (today they take opposite stances — boot too lax, recovery too
/// strict).
///
/// * a clean parse → [`MirrorTailPolicy::Clean`];
/// * a lone torn trailing line whose intact prefix re-verifies → PRESERVE the
///   original to `<mirror>.corrupt-<nanos>.bak` FIRST, durably TRIM the file to
///   the prefix, return [`MirrorTailPolicy::TornTail`];
/// * anything deeper → PRESERVE, return [`MirrorTailPolicy::DeepCorrupt`] (the
///   caller refuses).
///
/// A missing file surfaces as `MirrorIo(NotFound)` (callers handle it as they
/// did before — boot creates, recovery refuses). Any other read I/O is loud.
pub fn read_mirror_under_tail_policy(mirror_path: &Path) -> Result<MirrorTailPolicy, AppendError> {
    read_mirror_under_tail_policy_inner(mirror_path, true)
}

/// ADR-0099 R2 — the body of [`read_mirror_under_tail_policy`] with the trim's
/// advisory lock made a parameter, so [`ensure_consistent_with_db`] can call it
/// while already holding the mirror lock (see [`trim_mirror_to_inner`]).
fn read_mirror_under_tail_policy_inner(
    mirror_path: &Path,
    take_lock: bool,
) -> Result<MirrorTailPolicy, AppendError> {
    let bytes = std::fs::read(mirror_path).map_err(AppendError::MirrorIo)?;
    let (decision, entries, prefix_len, reason) = classify_mirror_bytes(&bytes);
    match decision {
        TailDecision::Clean => Ok(MirrorTailPolicy::Clean(entries)),
        TailDecision::TornTail => {
            // PRESERVE the original byte-for-byte BEFORE mutating anything.
            let preserved = preserve_corrupt_mirror(mirror_path)?;
            // Durably trim to the verified-intact prefix so a subsequent append
            // cannot concatenate onto the non-durable partial line.
            trim_mirror_to_inner(mirror_path, prefix_len as u64, take_lock)?;
            Ok(MirrorTailPolicy::TornTail {
                entries,
                preserved,
                dropped_bytes: (bytes.len() - prefix_len) as u64,
            })
        }
        TailDecision::Deep => {
            let preserved = preserve_corrupt_mirror(mirror_path)?;
            Ok(MirrorTailPolicy::DeepCorrupt {
                preserved,
                reason: reason.unwrap_or_else(|| "mirror is malformed".to_string()),
            })
        }
    }
}

/// ADR-0099 R3 — preserve a mirror that DIVERGES from the DB to
/// `<mirror>.diverged-<nanos>.bak`. A byte-for-byte copy; the original is left
/// in place, so the boot reconcile keeps surfacing the condition until a human
/// resolves it.
///
/// Its own infix, because divergence is its own class and the two existing names
/// both misdescribe it. `.ahead-` is what R2 reused, and the seq numbers are
/// precisely NOT ahead in the behind- and equal-length shapes — mislabelling
/// this is how the incident got misdiagnosed the first time (§R2.1).
/// `.corrupt-` points the operator at the mirror, when the mirror is the intact
/// party and the DB is the one that lost entries.
fn preserve_diverged_mirror(mirror_path: &Path) -> Result<PathBuf, AppendError> {
    preserve_mirror_copy(mirror_path, "diverged")
}

/// Copy `mirror_path` aside to `<mirror>.<infix>-<nanos>.bak` — unless an
/// existing `.<infix>-*.bak` is already byte-identical, in which case that one
/// IS the evidence and its path is returned instead.
///
/// ADR-0099 R3 (round 4) — the de-duplication is load-bearing now that a
/// divergence is TERMINAL. Under R2 the diverged route "succeeded" and stopped;
/// under R3 it refuses, so a supervisor restart loop re-runs the reconcile every
/// boot, and `reconcile_mirror_for` is best-effort so a *running* process also
/// re-runs it every snapshot cycle. Copying unconditionally meant one full copy
/// of the mirror per attempt, forever — measured at 4 copies in 4 calls — and
/// the audit mirror is typically the largest file in the tenant directory, on
/// the same filesystem as the DB. Filling that disk while refusing to boot would
/// turn a recoverable incident into an unrecoverable one.
///
/// Byte-comparison, not just existence: if the mirror has CHANGED since the last
/// preserve (the torn-tail path trims it after preserving), the new state is new
/// evidence and gets its own copy. Nothing is ever overwritten or removed.
fn preserve_mirror_copy(mirror_path: &Path, infix: &str) -> Result<PathBuf, AppendError> {
    let current = std::fs::read(mirror_path).map_err(AppendError::MirrorIo)?;
    let prefix = format!(
        "{}.{infix}-",
        mirror_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    );
    if let Some(dir) = mirror_path.parent() {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let name = e.file_name();
                let name = name.to_string_lossy();
                if name.starts_with(&prefix)
                    && name.ends_with(".bak")
                    && std::fs::read(e.path()).is_ok_and(|b| b == current)
                {
                    return Ok(e.path());
                }
            }
        }
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut os = mirror_path.as_os_str().to_owned();
    os.push(format!(".{infix}-{nanos}.bak"));
    let backup = PathBuf::from(os);
    std::fs::copy(mirror_path, &backup).map_err(AppendError::MirrorIo)?;
    Ok(backup)
}

/// Preserve the current (corrupt) mirror to a timestamped side file so the
/// evidence is never destroyed — the torn-tail analogue of
/// [`preserve_ahead_mirror`], writing `<mirror>.corrupt-<nanos>.bak`. Returns
/// the backup path for the surfaced log/error.
fn preserve_corrupt_mirror(mirror_path: &Path) -> Result<PathBuf, AppendError> {
    preserve_mirror_copy(mirror_path, "corrupt")
}

/// Durably truncate the mirror to `keep_len` bytes (the verified-intact
/// prefix), dropping a non-durable torn trailing line. fsync so the trim
/// itself survives a crash. The dropped bytes were preserved by
/// [`preserve_corrupt_mirror`] FIRST, so this destroys no evidence.
///
/// ADR-0099 R2 — the advisory lock is a parameter. `take_lock = false` is for callers that ALREADY hold the mirror's
/// exclusive lock ([`ensure_consistent_with_db`]): `flock` is per open-file-
/// description, so a second `lock_exclusive` on a second fd blocks even inside
/// one process — re-locking here would self-deadlock the reconciler.
fn trim_mirror_to_inner(
    mirror_path: &Path,
    keep_len: u64,
    take_lock: bool,
) -> Result<(), AppendError> {
    let file = OpenOptions::new()
        .write(true)
        .read(true)
        .open(mirror_path)
        .map_err(AppendError::MirrorIo)?;
    if take_lock {
        file.lock_exclusive().map_err(AppendError::MirrorIo)?;
    }
    file.set_len(keep_len).map_err(AppendError::MirrorIo)?;
    file.sync_all().map_err(AppendError::MirrorIo)?;
    Ok(())
}

/// ADR-0099 R2 — take THE exclusive advisory lock that serializes every writer
/// of the audit mirror, and hand back the locked handle (the lock lives as long
/// as the returned `File`).
///
/// The mirror is half of the audit ledger, and it had TWO writers with no shared
/// serialization: the lockstep [`sync_mirror`] fired from `aberp_db`'s
/// `WriteGuard::drop`, and the reconciler [`ensure_consistent_with_db`] run by
/// the snapshot daemon on its own connection. `sync_mirror` was internally
/// atomic (it locks, then reads the head it appends after). The reconciler was
/// NOT: it sampled the DB head and the mirror head with NO lock held and only
/// locked inside the append helper, so a lockstep append landing in that window
/// made it re-append the SAME seqs — duplicate mirror lines whose seqs no longer
/// ascend, which the next boot reads as a forked/corrupt mirror and REFUSES.
/// That is the seq-2508 recurrence: the duplicated rows were whatever the
/// daemons happened to write in the window, i.e. poll heartbeats.
///
/// Opened `append`-mode so every write lands at EOF regardless of the file
/// offset, and `create` so the bootstrap path can lock before the file exists.
///
/// ADR-0099 R3 — the wait is BOUNDED ([`RECONCILE_LOCK_TIMEOUT`]). `flock` is
/// cross-process, and the only caller ([`ensure_consistent_with_db`]) is invoked
/// by `aberp::snapshot::reconcile_mirror_for` while that fn holds `aberp_db`'s
/// single writer mutex. An untimed wait therefore let ANY stuck peer — a hung
/// `aberp` CLI, a crashed-but-not-reaped process still owning the fd — freeze
/// every DB write in the serve process behind it, with no diagnostic. R2 left
/// this untimed on the reasoning that a timeout must choose between refusing to
/// boot and proceeding unsynchronised; that is a false dilemma. The timeout
/// FAILS LOUD ([`AppendError::MirrorLockTimeout`]) and never proceeds
/// unsynchronised, so the TOCTOU R2 closed stays closed.
///
/// This bound alone did NOT remove the wedge, which is what R3 first claimed:
/// the per-commit taker was still untimed, and that is the one holding the
/// writer mutex on every commit. [`sync_mirror_lockstep`] bounds that one
/// (round 4), benignly (round 5).
fn lock_mirror_exclusive(mirror_path: &Path) -> Result<File, AppendError> {
    let file = open_mirror_for_append(mirror_path)?;
    lock_exclusive_bounded(&file, mirror_path, RECONCILE_LOCK_TIMEOUT)?;
    Ok(file)
}

/// ADR-0099 R3 — take the mirror's exclusive `flock` with a BOUNDED wait,
/// returning [`AppendError::MirrorLockTimeout`] rather than blocking forever.
///
/// This is the **bounded-FATAL** form, and (round 5) its only caller is
/// [`lock_mirror_exclusive`], i.e. the booting/pre-snapshot reconciler. That is
/// the one taker for which a timeout is both genuinely fatal AND safe: it has
/// committed nothing, so refusing loses no work, and refusing to proceed is what
/// keeps R2's TOCTOU closed.
///
/// The per-commit taker is deliberately NOT here. It needs the same bound for a
/// different reason (it holds `aberp_db`'s writer mutex) but the opposite
/// consequence (its transaction has already committed, so an `Err` would report
/// failure for durable work). It goes through [`try_lock_exclusive_within`] and
/// reports [`LockstepSync::SkippedLockContended`] — see [`sync_mirror_lockstep`].
fn lock_exclusive_bounded(
    file: &File,
    mirror_path: &Path,
    budget: std::time::Duration,
) -> Result<(), AppendError> {
    if try_lock_exclusive_within(file, budget)? {
        Ok(())
    } else {
        Err(AppendError::MirrorLockTimeout {
            path: mirror_path.display().to_string(),
            waited_ms: budget.as_millis() as u64,
        })
    }
}

/// Try to take `file`'s exclusive `flock`, giving up after `budget`.
/// `Ok(false)` means the budget ran out with a peer still holding it.
///
/// ADR-0099 R3 (round 5) — "we did not get the lock" and "that is fatal" are
/// two different questions, and only the CALLER can answer the second. Round 4
/// fused them into one helper that always returned `Err`, which is why the 2 s
/// budget reached fifteen already-committed money-CLI callers as a hard error.
/// The spin (`fs2` exposes no timed blocking acquire) polls at
/// [`MIRROR_LOCK_POLL`]; only the documented contention sentinel is retried, any
/// other I/O error is a real failure and is surfaced verbatim.
fn try_lock_exclusive_within(
    file: &File,
    budget: std::time::Duration,
) -> Result<bool, AppendError> {
    let deadline = std::time::Instant::now() + budget;
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(true),
            Err(e) if e.kind() == fs2::lock_contended_error().kind() => {
                if std::time::Instant::now() >= deadline {
                    return Ok(false);
                }
                std::thread::sleep(MIRROR_LOCK_POLL);
            }
            Err(e) => return Err(AppendError::MirrorIo(e)),
        }
    }
}

/// How long [`ensure_consistent_with_db`] waits for the mirror lock. Generous:
/// it runs at boot and before a snapshot, its failure is FATAL (boot refuses),
/// and every legitimate holder is orders of magnitude below this.
const RECONCILE_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// How long [`sync_mirror_lockstep`] waits. Deliberately much shorter than
/// [`RECONCILE_LOCK_TIMEOUT`], because the two have opposite consequences and
/// the budget follows the consequence, not the caller:
///
/// * The reconciler failing is fatal, so waiting is worth it.
/// * The lockstep sync failing is BENIGN — it reports
///   [`LockstepSync::SkippedLockContended`], `WriteGuard::drop` logs it and
///   continues, the mirror simply stays BEHIND, and the next write or the
///   pre-snapshot reconcile catches it up.
///
/// **A behind mirror is the safe direction, with one witness caveat.** It is
/// safe in the sense ADR-0110 D3 means: the dangerous state is a mirror that
/// runs AHEAD of the DB, because that is the one the reconciler must treat as a
/// possible lost DB commit. What a skipped sync costs is the mirror's role as
/// the WITNESS for the rows it skipped. Had the sync run, a later loss of those
/// rows would surface at the next reconcile as
/// [`AppendError::MirrorAheadOfDb`] and be replayed back with ZERO row loss;
/// with the sync skipped, DB and mirror simply agree that the rows are absent
/// and that loss is INVISIBLE until some later, synced row is lost and the
/// control catches THAT one as `MirrorAheadOfDb`. So skipping trades detection
/// latency for liveness — it never creates the dangerous direction, but it does
/// not leave detection untouched either.
///
/// So on the hot path we tolerate a normal hand-off — brief contention with the
/// in-process reconciler is routine and a zero-wait `try_lock` would skip
/// spuriously — and bail out on a genuine wedge. That BOUNDS the damage rather
/// than removing it: while a peer stays stuck, every commit still pays the full
/// budget with `aberp_db`'s writer mutex held (measured 2.06 s per commit), so
/// serve is slowed, not kept responsive. An unbounded freeze becomes a bounded
/// per-commit cost, which is the whole of the claim.
const SYNC_MIRROR_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Poll interval for the bounded lock waits. Short enough that an
/// uncontended-after-a-moment hand-off is not perceptibly delayed, long enough
/// that a 10 s wait is ~200 syscalls rather than a spin loop.
const MIRROR_LOCK_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// Replay the mirror's append-only JSONL delta — every record with
/// `seq > after_seq` — back into the `audit_ledger` table of `conn`,
/// **byte-faithfully** and in seq order, inside one transaction. Returns
/// the number of entries replayed.
///
/// ADR-0095 §1 step 4 — the boot recovery engine
/// (`aberp_snapshot::recover_or_refuse`) restores the latest VALID logical
/// snapshot (whose audit head is `after_seq`) into a private staging DB,
/// then calls this to re-apply the committed entries the snapshot predates,
/// reconstructing the live DB up to the mirror head. The mirror is **read
/// only** here — never truncated (the ADR-0095 §1 guard-rail; truncating it
/// would destroy the only record of the lost commits).
///
/// Each record is re-inserted through the same 12-canonical-column mapping
/// the append path uses (`schema::INSERT`): the hex hashes and base64
/// payload are decoded back to their stored blob form and the
/// `seq`/`prev_hash`/`entry_hash` bytes are preserved exactly, so the
/// rebuilt rows reproduce the originals and the tamper-evident hash chain
/// verifies end-to-end (the caller re-runs [`crate::Ledger::verify_chain`]
/// as the gate). The S441 session columns are written NULL — they are
/// excluded from the `entry_hash` preimage, so a verbatim replay never
/// carried them and the chain is unaffected.
///
/// # Errors
///
/// - [`AppendError::MirrorCorrupt`] if the mirror is unreadable/malformed
///   (per [`read_mirror_entries`]) or a hash/payload field will not decode.
/// - [`AppendError::Storage`] for any DuckDB write failure.
/// - [`AppendError::SequenceConflict`] if a row with that `seq` already
///   exists in `conn` (the staging DB must hold only `[1..=after_seq]`).
pub fn replay_mirror_delta(
    conn: &mut Connection,
    mirror_path: &Path,
    after_seq: u64,
) -> Result<u64, AppendError> {
    let entries = read_mirror_entries(mirror_path)?;
    let tx = conn.transaction()?;
    let mut replayed = 0u64;
    for record in entries.iter().filter(|e| e.seq > after_seq) {
        insert_mirror_entry_verbatim(&tx, record)?;
        replayed += 1;
    }
    tx.commit()?;
    Ok(replayed)
}

/// Insert one [`MirrorEntry`] into `audit_ledger` exactly as the mirror
/// recorded it. The hex hashes and base64 payload are decoded to their
/// stored blob form and every other column is written from the record's own
/// value, so the row reproduces the original byte-for-byte. The S441 session
/// columns (`session_id`/`session_pubkey`/`event_sig`) are written NULL: the
/// mirror line shape (ADR-0030 §1) never carried them and they are excluded
/// from the `entry_hash` preimage, so the chain is preserved.
///
/// Mirrors `crate::storage::insert_entry_verbatim` but binds from the JSONL
/// record rather than an in-memory [`Entry`](crate::Entry), so the recovery
/// engine can replay the mirror without reconstructing typed entries first.
fn insert_mirror_entry_verbatim(conn: &Connection, m: &MirrorEntry) -> Result<(), AppendError> {
    let decode_hex = |hex_str: &str, field: &str| -> Result<Vec<u8>, AppendError> {
        hex::decode(hex_str).map_err(|e| AppendError::MirrorCorrupt {
            reason: format!("{field} at seq {} is not valid hex: {e}", m.seq),
        })
    };
    let prev_hash = decode_hex(&m.prev_hash, "prev_hash")?;
    let binary_hash = decode_hex(&m.binary_hash, "binary_hash")?;
    let entry_hash = decode_hex(&m.entry_hash, "entry_hash")?;
    let payload =
        BASE64_STANDARD
            .decode(m.payload.as_bytes())
            .map_err(|e| AppendError::MirrorCorrupt {
                reason: format!("payload at seq {} is not valid base64: {e}", m.seq),
            })?;

    let inserted = conn.execute(
        crate::storage::schema::INSERT,
        params![
            m.id,
            m.seq as i64,
            prev_hash.as_slice(),
            m.time_wall,
            m.time_mono as i64,
            m.actor.to_storage_json(),
            binary_hash.as_slice(),
            m.tenant_id,
            m.kind,
            payload.as_slice(),
            m.idempotency_key.as_deref(),
            entry_hash.as_slice(),
            None::<&str>,
            None::<&str>,
            None::<&str>,
        ],
    )?;
    if inserted != 1 {
        return Err(AppendError::SequenceConflict { seq: m.seq });
    }
    Ok(())
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
    let file = open_mirror_for_append(mirror_path)?;
    // ADR-0099 R3 (round 5) — this acquire is UNBOUNDED, deliberately, and the
    // bound lives on [`sync_mirror_lockstep`] instead. Round 4 put a 2 s
    // bounded-FATAL budget here, which is the wrong place: every direct caller
    // of this fn has ALREADY COMMITTED (and, on the money paths, already
    // submitted to NAV) and propagates the result with `?`. A budget here turns
    // a slow peer into a command that reports failure for work that landed —
    // measured by the round-4 adversarial as `Err(MirrorLockTimeout)` at 2.05 s
    // where the untimed acquire returned `Ok(1)` at 4.99 s against a 5 s peer.
    // Waiting is the benign outcome for these callers; the wedge that actually
    // needed bounding is the per-commit one, which holds `aberp_db`'s writer
    // mutex — see [`sync_mirror_lockstep`].
    file.lock_exclusive().map_err(AppendError::MirrorIo)?;
    sync_mirror_locked(conn, meta, mirror_path, file)
}

/// What the LOCKSTEP mirror sync did. ADR-0099 R3 (round 5).
///
/// The per-commit sync is the one taker that must never block for long and must
/// never fail its caller: it runs in `aberp_db`'s `WriteGuard::drop`, after the
/// transaction has committed, with the single writer mutex still held. Both of
/// those force the outcome to be a REPORT rather than a `Result` the caller can
/// only propagate — hence a variant for "did not run", not an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockstepSync {
    /// The mirror was locked, verified and brought to the DB's head, whose seq
    /// this carries (identical to [`sync_mirror`]'s success value).
    Synced(u64),
    /// A peer still held the mirror's `flock` when the budget ran out, so the
    /// sync was SKIPPED. The commit itself is untouched and durable; the mirror
    /// is left BEHIND the DB — see [`SYNC_MIRROR_LOCK_TIMEOUT`] for what that
    /// costs and what it does not.
    SkippedLockContended {
        /// How long the acquire waited before giving up, in milliseconds.
        waited_ms: u64,
    },
}

/// Synchronise the mirror on the PER-COMMIT path, bounded and BENIGN.
/// ADR-0099 R3 (round 5).
///
/// Identical to [`sync_mirror`] except for how it takes the lock. This is the
/// variant `aberp_db`'s `WriteGuard::drop` calls, and it is the only caller in
/// the tree that both (a) runs while the single writer mutex is held, so an
/// untimed acquire freezes every DB write in the process behind a stuck peer,
/// and (b) has nowhere to propagate a failure to, because its transaction has
/// already committed. So the wait is bounded at [`SYNC_MIRROR_LOCK_TIMEOUT`]
/// and exhausting it yields [`LockstepSync::SkippedLockContended`] — a report,
/// not an `Err`.
///
/// Round 4 measured the unbounded form: one ordinary `Handle::write()` + guard
/// drop took 30.03 s against a peer holding the mirror `flock` for 30 s.
///
/// # Errors
///
/// The same set as [`sync_mirror`] — a contended lock is NOT among them.
pub fn sync_mirror_lockstep(
    conn: &Connection,
    meta: &LedgerMeta,
    mirror_path: &Path,
) -> Result<LockstepSync, AppendError> {
    let file = open_mirror_for_append(mirror_path)?;
    if !try_lock_exclusive_within(&file, SYNC_MIRROR_LOCK_TIMEOUT)? {
        return Ok(LockstepSync::SkippedLockContended {
            waited_ms: SYNC_MIRROR_LOCK_TIMEOUT.as_millis() as u64,
        });
    }
    Ok(LockstepSync::Synced(sync_mirror_locked(
        conn,
        meta,
        mirror_path,
        file,
    )?))
}

/// Open (or create) the mirror in append+read mode — the shape every mirror
/// writer needs. `append` so writes land at EOF regardless of the file offset,
/// `create` so the bootstrap path can lock before the file exists.
fn open_mirror_for_append(mirror_path: &Path) -> Result<File, AppendError> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(mirror_path)
        .map_err(AppendError::MirrorIo)
}

/// The body of [`sync_mirror`], from the point the mirror's exclusive `flock` is
/// already held on `file`. Split out so the two entry points can differ ONLY in
/// how they acquire that lock and what a contended acquire means to them.
fn sync_mirror_locked(
    conn: &Connection,
    meta: &LedgerMeta,
    mirror_path: &Path,
    file: File,
) -> Result<u64, AppendError> {
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
}

// ADR-0099 R3 — there is NO `Rebuilt` variant, deliberately. It was carried
// here (and in this fn's doc decision-tree) long after its last producer was
// removed, still describing a "full rebuild from the DB" on equal-length
// divergence. That branch now PRESERVES AND REFUSES, because rebuilding the
// mirror from the DB discards the only surviving copy of what the DB lost —
// exactly the manoeuvre ADR-0099 R2.4 identifies as the one that makes this
// incident class invisible. A variant no code can produce is an invitation to
// write the arm back, so it is gone rather than documented.

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
/// - mirror behind DB, shared prefix AGREES → replay missing entries →
///   [`RecoveryAction::Extended`]
/// - mirror ahead of DB, shared prefix AGREES → preserve the ahead mirror +
///   REFUSE ([`AppendError::MirrorAheadOfDb`]) — never silently truncated
/// - equal length, last hash matches → [`RecoveryAction::Unchanged`]
/// - **shared prefix DISAGREES at any length** (behind, ahead, or equal) →
///   preserve + REFUSE ([`AppendError::MirrorDivergedFromDb`])
///
/// Nothing here rebuilds the mirror from the DB. ADR-0099 R2.4: that is the one
/// manoeuvre that destroys the only surviving copy of what the DB lost.
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
    // ADR-0099 R2 — ONE exclusive lock for the WHOLE decide→act window.
    //
    // Every branch below acts on a SAMPLE of two heads (the DB's and the
    // mirror's). Before R2 the sample was taken with no lock held and only the
    // append helper locked, so a lockstep `sync_mirror` could extend the mirror
    // between the sample and the act. Two of the branches then misfire:
    //
    //   * `Extended` re-appends `[mirror_max+1 ..= db_max]` that the lockstep
    //     append already wrote — DUPLICATE seqs in the mirror. This is the
    //     seq-2508 recurrence (the duplicated rows were poll heartbeats: the
    //     highest-frequency writer is the one most likely to land in the
    //     window). A mirror whose seqs no longer ascend reads as corrupt at the
    //     next boot, and Defense REFUSES to start.
    //   * `mirror_max > db_max` fires a spurious `MirrorAheadOfDb`, which
    //     preserves a side file and refuses on a perfectly healthy pair.
    //
    // Holding the lock from BEFORE the DB-head read until after the act makes
    // the sample and the action one atomic step against every mirror writer, in
    // this process and out of it (`flock` is advisory but every mirror writer
    // takes it). Lock ORDER is always handle-mutex → mirror-flock: the lockstep
    // path holds `aberp_db`'s writer mutex and then blocks here, and the only
    // caller that holds both (`aberp::snapshot`'s pre-snapshot reconcile) takes
    // the handle mutex first. `aberp-audit-ledger` depends on nothing that can
    // call back into `aberp-db`, so the inverse order does not exist.
    let existed = mirror_path.try_exists().map_err(AppendError::MirrorIo)?;
    let lock = lock_mirror_exclusive(mirror_path)?;

    // Read the DB head UNDER the lock, so the pair of heads is one sample.
    let db_max_seq = read_db_max_seq(conn)?;

    // The mirror file did not exist before `lock_mirror_exclusive` created it,
    // so this is the bootstrap (`Created`) path. Checked from the pre-lock
    // probe rather than from a `NotFound` read error, because the lock had to
    // create the file to be able to hold it at all.
    if !existed {
        let written = rebuild_mirror_from_db_locked(conn, &lock)?;
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

    // Read the mirror under the unified ADR-0098 R1 torn-tail policy (findings
    // D+E): a lone torn trailing line is preserved + trimmed and we CONTINUE on
    // the intact prefix; corruption deeper than a torn tail is preserved +
    // REFUSED (never rebuild-from-DB); a missing mirror is (re)built from the DB.
    // `take_lock = false`: we already hold it (see `trim_mirror_to_inner`).
    let mirror_entries = match read_mirror_under_tail_policy_inner(mirror_path, false) {
        Ok(MirrorTailPolicy::Clean(entries)) => entries,
        // Torn tail — "the append never durably happened". The original was
        // preserved and the file trimmed to the verified-intact prefix; continue
        // and let the reconcile below compare the trimmed head to the DB (a
        // still-ahead trimmed mirror trips the P0 ahead-of-DB preserve+refuse).
        Ok(MirrorTailPolicy::TornTail {
            entries,
            preserved,
            dropped_bytes,
        }) => {
            tracing::warn!(
                target: "audit_event",
                event = "audit_mirror_torn_tail_trimmed",
                mirror_path = %mirror_path.display(),
                preserved = %preserved.display(),
                dropped_bytes,
                trimmed_head_seq = entries.last().map(|e| e.seq).unwrap_or(0),
                db_max_seq,
                "audit_mirror torn trailing line — preserved original and trimmed to the intact \
                 prefix; continuing (ADR-0098 R1; the dropped line was never durably committed)"
            );
            entries
        }
        // Deeper than a torn tail — NEVER rebuild-from-DB (that could destroy a
        // prefix the DB lacks). Preserve + REFUSE: boot exits non-zero with an
        // operator-actionable message naming the preserved path + recovery cmd.
        Ok(MirrorTailPolicy::DeepCorrupt { preserved, reason }) => {
            tracing::error!(
                target: "audit_event",
                event = "audit_mirror_deep_corrupt_refused",
                mirror_path = %mirror_path.display(),
                preserved = %preserved.display(),
                %reason,
                "audit_mirror is corrupt beyond a torn tail — REFUSING (preserved original; do \
                 NOT rebuild-from-DB, do NOT hand-edit the JSONL) (ADR-0098 R1)"
            );
            return Err(AppendError::MirrorCorruptPreserved {
                preserved: preserved.display().to_string(),
                reason,
            });
        }
        // Missing mirror: nothing to preserve; (re)build from the DB (Created).
        // Unreachable after R2 (the lock created the file and we hold it), kept
        // as the defensive fallback for a file unlinked under us.
        Err(AppendError::MirrorIo(io)) if io.kind() == std::io::ErrorKind::NotFound => {
            let written = rebuild_mirror_from_db_locked(conn, &lock)?;
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
        Err(other) => return Err(other),
    };

    let mirror_max_seq = mirror_entries.last().map(|e| e.seq).unwrap_or(0);

    // ADR-0099 R2/R3 — PROVE THE SHARED PREFIX BEFORE ACTING, IN EVERY BRANCH.
    //
    // R2 proved it only on the BEHIND branch, which is where the fifth
    // recurrence hid: `Extended` used to append DB rows after `mirror_max_seq`
    // without ever comparing the two stores, so a mirror that DISAGREED with the
    // DB got DB rows stapled onto it — destroying the length asymmetry
    // `MirrorAheadOfDb` keys on AND the head-hash equality the equal-length
    // branch keys on, and relabelling a lost DB commit as "corrupt mirror".
    //
    // R3 hoists the proof ABOVE the length branch, because the AHEAD branch had
    // the same hole with a worse consequence. An ahead-AND-diverged mirror
    // reported plain `MirrorAheadOfDb`, boot routed that to the snapshot+replay
    // auto-recovery, and recovery replays only the MIRROR — so the DB's
    // divergent rows, present in neither the snapshot nor the mirror, were
    // DISCARDED. Divergence is a property of the SHARED PREFIX
    // `[1..=min(mirror_max, db_max)]`, not of which store is longer, so it is
    // decided once, here, over exactly that prefix.
    //
    // The prefix slice matters: `mirror_entries` is seq-ascending, so
    // `partition_point` cuts it at `db_max_seq`. Without the cut, an AHEAD
    // mirror's legitimately DB-absent tail (`seq > db_max_seq`) would read as a
    // divergence at `db_max_seq + 1` and turn every honest lost-tail — including
    // an intentional dev DB-nuke — into a boot-fatal refusal.
    let shared_prefix = &mirror_entries[..mirror_entries.partition_point(|e| e.seq <= db_max_seq)];
    if let Some(seq) = first_divergent_seq(conn, shared_prefix)? {
        let preserved = preserve_diverged_mirror(mirror_path)?;
        tracing::error!(
            target: "audit_event",
            event = "audit_mirror_diverged_from_db",
            mirror_path = %mirror_path.display(),
            first_divergent_seq = seq,
            mirror_max_seq,
            db_max_seq,
            preserved = %preserved.display(),
            "audit_mirror DIVERGES from the DB at seq {seq} over their SHARED prefix — the DB \
             lost entries the mirror still holds and then re-used their seqs. REFUSING (never \
             graft, never truncate, never auto-recover: the DB's rows at those seqs exist in \
             NEITHER the mirror nor any snapshot, so any automatic resolution drops them). \
             Preserved the mirror (ADR-0099 R3)"
        );
        return Err(AppendError::MirrorDivergedFromDb {
            first_divergent_seq: seq,
            mirror_max_seq,
            db_max_seq,
            preserved: preserved.display().to_string(),
        });
    }

    if mirror_max_seq < db_max_seq {
        let added = append_db_entries_after_locked(conn, &lock, mirror_max_seq)?;
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
        // The mirror is AHEAD of the DB and their SHARED PREFIX AGREES (proven
        // above) — the DB lost a clean TAIL and nothing else, which is the
        // fingerprint of a torn-write / lost DB commit (the 2026-06-22
        // corruption class) or a dev DB-nuke.
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
            // ADR-0098 R1 (finding D) / ADR-0099 R3 — equal-length head-hash
            // divergence. At equal length the shared prefix IS the whole mirror,
            // so the hoisted prefix proof above already refused every case this
            // arm was written for and it is unreachable in practice. Kept as a
            // fail-closed backstop: if the head hashes ever disagree while
            // `first_divergent_seq` reports agreement, that is a contradiction
            // between two reads of the same rows, and the one thing we must not
            // do is fall through to `Unchanged`.
            let preserved = preserve_diverged_mirror(mirror_path)?;
            tracing::error!(
                target: "audit_event",
                event = "audit_mirror_diverged_from_db",
                first_divergent_seq = db_max_seq,
                mirror_path = %mirror_path.display(),
                db_max_seq,
                preserved = %preserved.display(),
                ?db_hash,
                ?mirror_hash,
                "audit_mirror head entry_hash DIVERGES from the DB at equal length while the \
                 prefix comparison reported agreement — contradictory reads. REFUSING \
                 (preserved original; never auto-resolve equal-length divergence) (ADR-0098 R1)"
            );
            Err(AppendError::MirrorDivergedFromDb {
                first_divergent_seq: db_max_seq,
                mirror_max_seq,
                db_max_seq,
                preserved: preserved.display().to_string(),
            })
        }
    }
}

/// ADR-0099 R2 — does the mirror's prefix `[1..=mirror_max_seq]` agree with the
/// DB's? Returns `Ok(None)` when it does.
///
/// **O(1) on the happy path, by construction** — two reads, no scan. The full
/// scan happens only once we already know we are going to refuse, to name the
/// seq for the operator.
///
/// The two reads are BOTH required, and the second is the one ADR-0099 R3's
/// first pass was missing:
///
/// 1. **Head hash.** Both stores are hash chains, so the mirror's head
///    `entry_hash` commits to its entire prefix and the DB's row at that seq
///    commits to its own. Equal hashes prove the two agree *about the rows the
///    DB still has*.
/// 2. **Cardinality.** A hash chain commits to its history, NOT to its own
///    continued existence in the table. `DELETE FROM audit_ledger WHERE seq = 3`
///    leaves every surviving row's `entry_hash` untouched, head included — so
///    the head compare passes and an interior hole reads as agreement. Measured:
///    a 5-entry ledger with seq 3 deleted from the DB reconciled `Ok(Unchanged)`
///    while the mirror still held the row. That is a lost committed audit entry
///    reported as healthy — the exact class this ADR exists for. Requiring the
///    DB to hold `head.seq` rows over `[1..=head.seq]` closes it for one more
///    aggregate — `COUNT(*)` **and** `COUNT(DISTINCT seq)`, because without a
///    `UNIQUE(seq)` a duplicate offsets a hole and `COUNT(*)` alone still reads
///    as agreement (round 5; see [`read_db_seq_counts_up_to`]).
///
/// `Ok(Some(seq))` = the earliest seq at which the two disagree, or at which the
/// DB has no row at all (the DB lost it).
fn first_divergent_seq(
    conn: &Connection,
    mirror_entries: &[MirrorEntry],
) -> Result<Option<u64>, AppendError> {
    let Some(head) = mirror_entries.last() else {
        return Ok(None);
    };
    // The O(1) proof: the head hash AND the row count it presupposes.
    match read_db_entry_at_seq(conn, head.seq)? {
        Some(e)
            if hex::encode(e.entry_hash.as_bytes()) == head.entry_hash
                && read_db_seq_counts_up_to(conn, head.seq)? == (head.seq, head.seq) =>
        {
            return Ok(None)
        }
        _ => {}
    }
    // Refusal path only: locate the earliest disagreement for the message.
    let db = read_db_entries_after(conn, 0)?;
    for m in mirror_entries {
        match db.iter().find(|e| e.seq.as_u64() == m.seq) {
            None => return Ok(Some(m.seq)),
            Some(e) if hex::encode(e.entry_hash.as_bytes()) != m.entry_hash => {
                return Ok(Some(m.seq))
            }
            _ => {}
        }
    }
    // The head disagreed but no interior did — the head IS the divergence.
    Ok(Some(head.seq))
}

/// ADR-0099 R3 — `(rows, distinct_seqs)` that the DB holds over `[1..=up_to]`.
/// A hash chain proves what its surviving rows say; it cannot prove that none
/// were DELETEd out from under it, because removing an interior row rewrites
/// nothing. This is the cardinality half of that proof.
///
/// **Round 5 — `COUNT(*)` alone is not the cardinality half.** `audit_ledger`
/// has no `UNIQUE(seq)` (S341 dropped that ART index, duckdb#23046 / S332), so
/// a duplicate seq is representable and OFFSETS a hole one-for-one: with
/// `db = [1, 2, 2, 4, 5]` against `mirror = [1..=5]` the head hash matches and
/// `COUNT(*)` is 5, so the pair reconciled `Ok(Unchanged)` while the DB had lost
/// its committed entry at seq 3. Requiring `COUNT(DISTINCT seq)` to agree as
/// well closes it: the two counts can only BOTH equal `head.seq` when the DB
/// holds each of `[1..=head.seq]` exactly once. Still O(1) — one query, two
/// aggregates.
fn read_db_seq_counts_up_to(conn: &Connection, up_to: u64) -> Result<(u64, u64), AppendError> {
    let (rows, distinct): (i64, i64) = conn.query_row(
        "SELECT COUNT(*), COUNT(DISTINCT seq) FROM audit_ledger WHERE seq <= ?",
        [up_to as i64],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok((rows.max(0) as u64, distinct.max(0) as u64))
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
    preserve_mirror_copy(mirror_path, "ahead")
}

/// Truncate the mirror and rewrite it from the DB's full entry set
/// `[1..=db_max_seq]`. Used by the Created (bootstrap) recovery path ONLY —
/// there `up_to == db_max_seq`, so the full DB scan IS `[1..=db_max_seq]`, and
/// there is no pre-existing mirror content to destroy.
/// Returns the entry count written. (The mirror-ahead case no longer
/// rebuilds: it preserves + refuses via [`preserve_ahead_mirror`].)
fn rebuild_mirror_from_db_locked(conn: &Connection, file: &File) -> Result<u64, AppendError> {
    let entries = read_db_entries_after(conn, 0)?;
    // ADR-0099 R2 — truncate UNDER the caller's lock. The pre-R2 code opened
    // with `truncate(true)` and only THEN called `lock_exclusive`: the file was
    // already zero-length before the lock was granted, so a concurrent holder's
    // just-appended bytes could be destroyed by a rebuild that was still waiting
    // for the lock. `set_len(0)` here happens with the lock already held.
    file.set_len(0).map_err(AppendError::MirrorIo)?;
    // `impl Write for &File` takes `&mut self`, so the sink is a mutable
    // binding OF the shared reference — not a mutable borrow of the file.
    let mut sink: &File = file;
    let mut written: u64 = 0;
    for entry in &entries {
        let record = MirrorEntry::from_entry(entry)?;
        let line = encode_line(&record)?;
        sink.write_all(&line).map_err(AppendError::MirrorIo)?;
        written += 1;
    }
    sink.flush().map_err(AppendError::MirrorIo)?;
    file.sync_all().map_err(AppendError::MirrorIo)?;
    Ok(written)
}

/// Append DB entries with `seq > after_seq` to the existing mirror.
/// The Extended recovery path. Returns the count appended.
fn append_db_entries_after_locked(
    conn: &Connection,
    file: &File,
    after_seq: u64,
) -> Result<u64, AppendError> {
    let entries = read_db_entries_after(conn, after_seq)?;
    let mut sink: &File = file;
    let mut added: u64 = 0;
    for entry in &entries {
        let record = MirrorEntry::from_entry(entry)?;
        let line = encode_line(&record)?;
        sink.write_all(&line).map_err(AppendError::MirrorIo)?;
        added += 1;
    }
    if added > 0 {
        sink.flush().map_err(AppendError::MirrorIo)?;
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
        //
        // ADR-0099 R3 — the DB must be a genuine PREFIX of the mirror for this
        // to be the AHEAD case at all. The fixture used to build a SEPARATE DB
        // with its own seq-1 entry, so the two disagreed at seq 1 and the shape
        // was really a divergence; R2's prefix proof ran only on the BEHIND
        // branch, so nothing noticed. That distinction is load-bearing — a clean
        // AHEAD is the one condition boot auto-recovers, and routing a
        // divergence there discards the DB's own rows. The diverged variant of
        // this fixture is pinned just below.
        let dir = tempdir_under_target();
        let mirror = dir.join("ahead.audit.log");
        let (conn_old, meta_old) = open_conn_with_two_entries();
        sync_mirror(&conn_old, &meta_old, &mirror).unwrap();
        assert_eq!(read_mirror_entries(&mirror).unwrap().len(), 2);
        let before = std::fs::read(&mirror).unwrap();

        // The DB loses its TAIL and nothing else: seq 1 stays byte-identical.
        conn_old
            .execute_batch("DELETE FROM audit_ledger WHERE seq >= 2;")
            .unwrap();
        let conn_fresh = conn_old;

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

    /// ADR-0099 R3 — ahead on COUNT is not enough. A mirror whose entries
    /// disagree with the DB over their SHARED prefix is a DIVERGENCE however the
    /// lengths compare, and must never be reported as the auto-recoverable AHEAD:
    /// recovery rebuilds from a snapshot and replays the mirror, so the DB's
    /// divergent rows — in neither input — would be discarded.
    #[test]
    fn ensure_consistent_reports_an_ahead_but_diverged_mirror_as_divergence() {
        let dir = tempdir_under_target();
        let mirror = dir.join("ahead-diverged.audit.log");
        let (conn_old, meta_old) = open_conn_with_two_entries();
        sync_mirror(&conn_old, &meta_old, &mirror).unwrap();
        let before = std::fs::read(&mirror).unwrap();

        // A DIFFERENT DB holding its own seq 1: mirror head 2 > DB head 1, and
        // yet the two disagree at seq 1.
        let mut conn_other = Connection::open_in_memory().unwrap();
        ensure_schema(&conn_other).unwrap();
        let meta_other = mk_meta();
        append_one(&mut conn_other, &meta_other, "fresh-1", b"fresh-payload-1");

        match ensure_consistent_with_db(&conn_other, &mirror) {
            Err(AppendError::MirrorDivergedFromDb {
                first_divergent_seq,
                mirror_max_seq,
                db_max_seq,
                preserved,
            }) => {
                assert_eq!(first_divergent_seq, 1);
                assert_eq!((mirror_max_seq, db_max_seq), (2, 1));
                assert_eq!(std::fs::read(&preserved).unwrap(), before);
            }
            other => panic!(
                "ahead-on-count but disagreeing at seq 1 must be a DIVERGENCE, got {other:?}"
            ),
        }
        assert_eq!(
            std::fs::read(&mirror).unwrap(),
            before,
            "refusal must leave the mirror untouched"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_consistent_refuses_and_preserves_on_head_hash_mismatch() {
        // ADR-0098 R1 (finding D): equal length but the mirror head entry_hash
        // DIVERGES from the DB → PRESERVE + REFUSE (never silently rebuild).
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
        let before = std::fs::read(&mirror).unwrap();

        let err = ensure_consistent_with_db(&conn, &mirror).unwrap_err();
        match err {
            // ADR-0099 R2 — this used to surface as a generic
            // `MirrorCorruptPreserved{reason:"…equal length…"}`. It is not
            // generic corruption: both stores hold a row at the divergent seq
            // and they disagree, which is the signature of a lost DB commit.
            // The variant now says so AND names the seq, so boot can route it
            // to the recovery arm and the operator is not sent looking at the
            // wrong subsystem.
            AppendError::MirrorDivergedFromDb {
                first_divergent_seq,
                preserved,
                ..
            } => {
                assert_eq!(
                    first_divergent_seq, 2,
                    "the tampered entry is seq 2; the refusal must name it, not just \
                     report that the heads differ"
                );
                assert!(
                    std::path::Path::new(&preserved).exists(),
                    "the original must be preserved to {preserved}"
                );
            }
            other => panic!("expected MirrorDivergedFromDb, got {other:?}"),
        }
        // The refusal must NOT mutate the live mirror (no rebuild-from-DB).
        assert_eq!(
            std::fs::read(&mirror).unwrap(),
            before,
            "refusal must leave the mirror untouched"
        );
        // Exactly one preserved copy, named for the class it actually is
        // (ADR-0099 R3): the mirror is the INTACT party here — the DB is the one
        // that lost entries — so `.corrupt-` would point the operator at the
        // wrong subsystem, which is how this incident got misdiagnosed.
        assert_eq!(
            count_diverged_baks(&dir),
            1,
            "exactly one preserved diverged copy"
        );
        assert_eq!(
            count_corrupt_baks(&dir),
            0,
            "a divergence is NOT mirror corruption and must not be labelled as such"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_consistent_trims_torn_tail_and_continues() {
        // ADR-0098 R1 (findings D+E): a lone torn trailing line (final newline
        // lost) is NOT a silent rebuild-from-DB. The original is preserved, the
        // file is trimmed to the intact prefix, and the reconcile continues —
        // the DB still holds the dropped entry here, so it is re-extended.
        let dir = tempdir_under_target();
        let mirror = dir.join("corrupt.audit.log");
        let (conn, meta) = open_conn_with_two_entries();
        sync_mirror(&conn, &meta, &mirror).unwrap();

        let bytes = std::fs::read(&mirror).unwrap();
        assert_eq!(bytes.last().copied(), Some(b'\n'));
        std::fs::write(&mirror, &bytes[..bytes.len() - 1]).unwrap();

        let action = ensure_consistent_with_db(&conn, &mirror).unwrap();
        // Trimmed to seq 1; DB has seq 2 → the missing entry is re-extended.
        assert_eq!(action, RecoveryAction::Extended { entries_added: 1 });
        // The mirror re-reads clean at 2 entries.
        assert_eq!(read_mirror_entries(&mirror).unwrap().len(), 2);
        // The torn original was preserved, not destroyed.
        assert_eq!(
            count_corrupt_baks(&dir),
            1,
            "torn original preserved to a .corrupt-*.bak"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_consistent_refuses_and_preserves_on_deep_corruption() {
        // ADR-0098 R1 (findings D+E): corruption DEEPER than a torn tail — a
        // newline-terminated file with a seq GAP — is PRESERVED + REFUSED,
        // never rebuilt-from-DB (which could destroy a prefix the DB lacks).
        let dir = tempdir_under_target();
        let mirror = dir.join("deep.audit.log");
        let (conn, meta) = open_conn_with_two_entries();
        sync_mirror(&conn, &meta, &mirror).unwrap();

        // Keep entry 1, then append a well-formed, newline-TERMINATED entry
        // whose seq is 3 — a contiguity break that is NOT at a torn tail.
        let entries = read_mirror_entries(&mirror).unwrap();
        let mut e3 = entries[1].clone();
        e3.seq = 3;
        let mut bytes = encode_line(&entries[0]).unwrap();
        bytes.extend_from_slice(&encode_line(&e3).unwrap());
        std::fs::write(&mirror, &bytes).unwrap();
        let before = std::fs::read(&mirror).unwrap();

        let err = ensure_consistent_with_db(&conn, &mirror).unwrap_err();
        assert!(
            matches!(err, AppendError::MirrorCorruptPreserved { .. }),
            "expected MirrorCorruptPreserved, got {err:?}"
        );
        assert_eq!(
            std::fs::read(&mirror).unwrap(),
            before,
            "refusal must leave the mirror untouched"
        );
        assert_eq!(
            count_corrupt_baks(&dir),
            1,
            "deep-corrupt original preserved"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn decide_tail_maps_the_four_cases() {
        // The PURE R1 torn-tail decision core (also extracted for the saw-off
        // gate). Only a lone partial FINAL line (`!terminated && prefix_ok`) is
        // a trim-able torn tail; every other unclean shape is Deep.
        assert_eq!(decide_tail(true, true), TailDecision::Clean);
        assert_eq!(decide_tail(false, true), TailDecision::TornTail);
        assert_eq!(decide_tail(true, false), TailDecision::Deep);
        assert_eq!(decide_tail(false, false), TailDecision::Deep);
    }

    /// Count `<mirror>.corrupt-*.bak` preserved copies in a test dir.
    fn count_diverged_baks(dir: &std::path::Path) -> usize {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".diverged-"))
            .count()
    }

    fn count_corrupt_baks(dir: &std::path::Path) -> usize {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".corrupt-"))
            .count()
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
