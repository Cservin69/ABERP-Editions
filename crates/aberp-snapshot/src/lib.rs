//! Periodic, validated, logical DuckDB snapshot subsystem (ADR-0082).
//!
//! # Why this exists
//!
//! On **2026-06-11** an ART (adaptive-radix-tree) index corruption in the
//! live prod DuckDB cost ~5 hours of hand-surgery. It is the same on-disk
//! corruption family S332/S341/S375/S410 have chased (`duckdb#23046` and
//! relatives) and it recurs. `[[trust-code-not-operator]]`: the rollback
//! point must be produced by code on a timer, not by an operator
//! remembering to copy a file.
//!
//! # Why logical export, not a file copy
//!
//! ART corruption is internal to the *live data file*. A byte-for-byte
//! copy (S393's panic button) copies the corruption. DuckDB's
//! `EXPORT DATABASE 'dir' (FORMAT PARQUET)` instead walks the **logical**
//! rows and writes `schema.sql` + `load.sql` + one Parquet file per table —
//! independent of the source's physical index/checkpoint structure. The
//! snapshot is corruption-free *by construction* even while the live ART
//! degrades, and `IMPORT DATABASE` rebuilds a pristine file with fresh
//! indexes.
//!
//! # Shape
//!
//! - [`take_snapshot`] — `EXPORT` to `<store>/snap-<seq>-<ts>/`, validate,
//!   tag with seq + UTC timestamp + source-DB SHA-256 in `meta.json`.
//! - [`validate_export`] — `IMPORT` into a throwaway in-memory DuckDB and
//!   run the smoke set (count `invoice`, count `audit_ledger`, re-verify
//!   the ADR-0008 hash chain). A failed snapshot is kept but marked
//!   `valid=false`; the caller emits `SnapshotValidationFailed`.
//! - [`list_snapshots`] — scan the store, parse each `meta.json`.
//! - [`plan_retention`] / [`prune`] — pure retention math + the pruning it
//!   implies (keep last N + daily-30d + weekly-1y, never the newest valid).
//! - [`ensure_restore_allowed`] / [`restore_into`] — the guarded restore.
//!   The safety (refuse to overwrite a live `~/.aberp/` DB without
//!   `--confirm`) lives **in this binary**, not in operator discipline.
//!
//! The store is `~/Documents/ABERP-snapshots/<tenant>/` — outside the repo
//! and outside `~/.aberp/`, so a tenant reset or a restore never deletes
//! the rollback copies. The seq is derived by scanning directory names:
//! the filesystem *is* the index, with no separate manifest to drift
//! (`[[hulye-biztos]]`).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

mod crash_safe;
mod evidence;
mod recover;
mod retention;
mod store;
mod take;

pub use crash_safe::{
    atomic_install, checkpoint_is_current, durable_checkpoint, install_intent_path, marker_path,
    read_install_intent, read_marker, resume_pending_install, write_install_intent, write_marker,
    CheckpointMarker, CheckpointReport, InstallIntent, ResumeAction,
};
pub use evidence::{
    archive_then_remove, guarded_remove, is_protected_evidence, list_evidence,
    name_is_evidence_shaped, name_is_live, normalise_incident_tag, path_is_under_evidence_root,
    path_is_under_tenant_home, plan_evidence_release, ArchivedArtefact, EvidenceArtefact,
    EvidenceDisposition, EvidencePolicy, RetainReason, EVIDENCE_FRAGMENTS, EVIDENCE_SUFFIXES,
    LIVE_TENANT_NAMES, LIVE_TRANSIENT_INFIXES,
};
pub use recover::{
    live_durable_checkpoint, provision_atomic, recover_or_refuse, recover_or_refuse_with_audit,
    RecoveredMeta, RecoveryOutcome, StagedAuditRow,
};
pub use retention::{plan_retention, prune, RetentionPlan, RetentionPolicy};
pub use store::{
    default_store_dir, edition_store_dir, find_snapshot, list_snapshots, resolve_selector,
    resolve_selector_in, snapshot_identity, SnapshotRecord, SnapshotSelector,
};
pub use take::{
    ensure_not_prod_path, ensure_restore_allowed, find_pre_restore_units, pre_restore_move_back,
    pre_restore_unit_is_partly_moved_back, resolve_db_path, restore_in_place, restore_into,
    take_snapshot, take_snapshot_with, validate_export, validate_installed_db, wal_path_for,
    InPlaceRestoreReport, MirrorReconcile, PreservedUnit, ValidationReport, PRE_RESTORE_INFIX,
};

/// Typed error surface for the snapshot subsystem. Library crate → no
/// `anyhow` (ADR-0021 Part A).
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("duckdb error: {0}")]
    Duck(#[from] duckdb::Error),

    #[error("source database {0} does not exist — nothing to snapshot")]
    SourceMissing(PathBuf),

    #[error("no snapshot matching '{0}' found in the store")]
    NotFound(String),

    #[error("refusing to restore: {0}")]
    RestoreRefused(String),

    #[error(
        "snapshot '{0}' failed validation and is marked invalid — refusing to restore from it"
    )]
    RestoreFromInvalid(String),

    #[error("snapshot metadata at {path} is unreadable: {detail}")]
    BadMeta { path: PathBuf, detail: String },

    #[error("atomic provisioning of {path} failed: {detail}")]
    Provision { path: PathBuf, detail: String },

    /// ADR-0116 D2 — a caller tried to delete recovery evidence. This is
    /// never a recoverable condition to paper over: the artefact is the only
    /// record of a durability incident, and the correct response is to route
    /// the removal through `aberp evidence archive` or not to remove it.
    #[error(
        "refusing to delete recovery evidence at {0} — protected by ADR-0116 D2          (`aberp evidence archive` is the only sanctioned release path)"
    )]
    EvidenceProtected(PathBuf),

    /// ADR-0116 D3.3 / G6 — `seq` is recycled after a prune, so a selector
    /// can name more than one snapshot. Refuse; never guess.
    #[error(
        "snapshot selector '{selector}' is AMBIGUOUS — it matches {count} snapshots          ({candidates}). `seq` is recycled after a prune, so address the snapshot by its          full directory name or by seq@created_at"
    )]
    AmbiguousSelector {
        selector: String,
        count: usize,
        candidates: String,
    },

    /// **ADR-0116 F2** — the in-place restore installed a file that does not
    /// verify, or does not match the snapshot it came from. The previous
    /// database is intact inside the named `.PRE-RESTORE-` unit.
    #[error(
        "the IN-PLACE restore installed {db} but it failed re-verification: {detail}. The \
         previous database is intact at {preserved} (with its .wal, .ckpt-ok and .audit.log \
         siblings) — move the unit back to recover it"
    )]
    InstalledVerifyFailed {
        db: PathBuf,
        preserved: String,
        detail: String,
    },

    /// **ADR-0116 D3.4** — a fresh audit mirror could not be written for the
    /// restored chain. Non-fatal at the call site (an absent mirror is what
    /// the boot path creates), carried as a typed error for the log line.
    #[error("could not rebuild the audit mirror for the restored database: {0}")]
    MirrorRebuildFailed(String),
}

impl SnapshotError {
    /// Small helper so call sites can attach the offending path to a bare
    /// [`std::io::Error`] without a `.map_err` closure each time.
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        SnapshotError::Io {
            path: path.into(),
            source,
        }
    }
}

/// Result alias for the crate.
pub type Result<T> = std::result::Result<T, SnapshotError>;

/// On-disk metadata written into each snapshot directory as `meta.json`.
///
/// This is the *only* persisted state — there is no separate manifest.
/// The seq, timestamp, and source SHA-256 are the snapshot's identity; the
/// validation verdict tells retention whether the snapshot is restorable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotMeta {
    /// ADR-0116 D4 — format version of THIS `meta.json`.
    ///
    /// Prod's live `meta.json` files already carry a `secondary_index_count`
    /// field Editions' `SnapshotMeta` does not have: the format had already
    /// drifted between the two lines with no version marker, and cross-parsing
    /// worked only because `serde` ignores unknown fields and because the
    /// stores are disjoint (ADR-0093). Pre-D4 files default to `1`.
    #[serde(default = "meta_version_pre_d4")]
    pub meta_version: u32,
    /// Sequence number, `max(surviving seq) + 1` at creation time.
    ///
    /// **NOT a stable identity.** `next_seq` derives it by scanning the store,
    /// so a pruned seq is RECYCLED: seq 24 names three different snapshots in
    /// prod's ledger. Unique *instantaneously*; recycled after a prune. Every
    /// snapshot-addressing surface uses the full
    /// `(seq, created_at, source_db_sha256)` identity instead — see
    /// [`SnapshotSelector`] and ADR-0116 D3.3/G6.
    ///
    /// The store *sort* is unaffected: a new snapshot always takes `max+1`, so
    /// within-store seq order still tracks creation order.
    pub seq: u64,
    /// When the snapshot was taken (UTC).
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
    /// Hex SHA-256 of the live source DB file at snapshot time. Records
    /// *which* physical DB state this logical export came from.
    pub source_db_sha256: String,
    /// Total byte size of the export directory (sum of parquet + sql).
    pub byte_size: u64,
    /// `true` iff the snapshot passed [`validate_export`] (re-import +
    /// smoke + hash-chain verify).
    pub valid: bool,
    /// `count(*)` of the `invoice` table in the re-imported snapshot, or
    /// `-1` if the table was absent / unreadable.
    pub invoice_count: i64,
    /// `count(*)` of `audit_ledger` in the re-imported snapshot.
    pub audit_count: i64,
    /// Number of audit entries the hash chain re-verified end-to-end.
    pub chain_len: u64,
    /// When `valid == false`, the human-readable reason.
    pub validation_error: Option<String>,

    // ─── ADR-0116 D4 — anchor coverage, RECORDED, never gating ──────────
    /// Count of `audit_ledger_anchors` rows in the snapshot.
    ///
    /// **`-1` means "not recorded"**, NEVER "zero anchors": a snapshot taken
    /// before this field existed, or one whose anchor table was unreadable,
    /// reads back `-1`. A bare `#[serde(default)]` would yield `0` here, which
    /// is indistinguishable from a snapshot *verified* to carry none — and for
    /// a field whose only purpose is telling a restoring operator what a
    /// database can prove in court, defaulting to the worst-case-looking value
    /// while meaning "unknown" is exactly backwards. The `-1` sentinel follows
    /// the in-tree precedent set by prod's `secondary_index_count` after the
    /// 2026-08-03 incident.
    #[serde(default = "anchor_count_unrecorded")]
    pub anchor_count: i64,
    /// Highest `audit_ledger` seq covered by a VERIFIED anchor — an anchor
    /// whose `chain_head_hash_at_anchor` resolves to an entry in the snapshot's
    /// own chain and which actually carries a timestamp token.
    ///
    /// `None` == not recorded (a `u64` cannot carry the sentinel at all).
    /// `Some(0)` would mean "recorded, and nothing is anchored" — a different
    /// statement, which is why the two are kept distinguishable.
    #[serde(default)]
    pub anchored_through_seq: Option<u64>,
}

/// `meta_version` for a `meta.json` written before ADR-0116 D4 added the
/// field. See [`SnapshotMeta::meta_version`].
fn meta_version_pre_d4() -> u32 {
    1
}

/// ADR-0116 D4 / F7 — the "not recorded" sentinel for
/// [`SnapshotMeta::anchor_count`]. NEVER `0`.
fn anchor_count_unrecorded() -> i64 {
    -1
}

/// `meta_version` written by this build.
pub const META_VERSION_CURRENT: u32 = 2;
