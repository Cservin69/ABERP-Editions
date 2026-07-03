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
mod recover;
mod retention;
mod store;
mod take;

pub use crash_safe::{
    atomic_install, checkpoint_is_current, durable_checkpoint, install_intent_path, marker_path,
    read_install_intent, read_marker, resume_pending_install, write_install_intent, write_marker,
    CheckpointMarker, CheckpointReport, InstallIntent, ResumeAction,
};
pub use recover::{live_durable_checkpoint, provision_atomic, recover_or_refuse, RecoveryOutcome};
pub use retention::{plan_retention, prune, RetentionPlan, RetentionPolicy};
pub use store::{
    default_store_dir, edition_store_dir, find_snapshot, list_snapshots, SnapshotRecord,
};
pub use take::{
    ensure_not_prod_path, ensure_restore_allowed, restore_into, take_snapshot, validate_export,
    ValidationReport,
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
    /// Monotonic sequence number (1-based), unique within the store.
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
}
