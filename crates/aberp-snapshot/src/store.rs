//! Snapshot store layout, sequence derivation, and listing.
//!
//! Layout: `<store>/snap-<seq>-<UTC-ts>/` where each directory holds a
//! DuckDB `EXPORT DATABASE` (schema.sql, load.sql, *.parquet) plus a
//! `meta.json` ([`crate::SnapshotMeta`]). A `*.partial` suffix marks an
//! in-progress export not yet finalized; those are ignored by listing and
//! sequence derivation.

use std::path::{Path, PathBuf};

use time::OffsetDateTime;

use crate::{Result, SnapshotError, SnapshotMeta};

/// Filename of the per-snapshot metadata sidecar.
pub(crate) const META_FILE: &str = "meta.json";

/// Suffix marking an export directory that is not yet finalized.
pub(crate) const PARTIAL_SUFFIX: &str = ".partial";

/// A finalized snapshot on disk: its directory plus parsed metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotRecord {
    /// Absolute path to the snapshot directory (`snap-<seq>-<ts>`).
    pub dir: PathBuf,
    /// Parsed `meta.json`.
    pub meta: SnapshotMeta,
}

impl SnapshotRecord {
    /// Age of the snapshot relative to `now` (UTC). Saturates at zero for
    /// clock skew where `created_at` is slightly in the future.
    pub fn age(&self, now: OffsetDateTime) -> time::Duration {
        let d = now - self.meta.created_at;
        if d.is_negative() {
            time::Duration::ZERO
        } else {
            d
        }
    }
}

/// Resolve `~/Documents/ABERP-snapshots/<tenant>/`. Uses HOME / USERPROFILE
/// directly (no `dirs` dep), keeping the store OUTSIDE the repo and OUTSIDE
/// `~/.aberp/` — the same posture S393 used so a tenant reset or a restore
/// never deletes the rollback copies.
pub fn default_store_dir(tenant: &str) -> Result<PathBuf> {
    let home = std::env::var("HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .or_else(|| std::env::var("USERPROFILE").ok().filter(|p| !p.is_empty()))
        .ok_or_else(|| {
            SnapshotError::io(
                PathBuf::from("$HOME"),
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "neither HOME nor USERPROFILE is set",
                ),
            )
        })?;
    Ok(PathBuf::from(home)
        .join("Documents")
        .join("ABERP-snapshots")
        .join(sanitise_tenant(tenant)))
}

/// Sanitise a tenant so it can never escape the store dir (no `/`, `..`).
pub(crate) fn sanitise_tenant(tenant: &str) -> String {
    tenant
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Build a finalized snapshot directory name `snap-<seq>-<UTC-ts>`. The
/// timestamp format matches S393 (`YYYYMMDD-HHMMSS`) so the two stores read
/// the same at a glance.
pub(crate) fn snapshot_dir_name(seq: u64, now: OffsetDateTime) -> Result<String> {
    use time::macros::format_description;
    const TS: &[time::format_description::FormatItem<'_>] =
        format_description!("[year][month][day]-[hour][minute][second]");
    let ts = now.format(TS).map_err(|e| SnapshotError::BadMeta {
        path: PathBuf::from("<timestamp>"),
        detail: format!("format snapshot timestamp: {e}"),
    })?;
    Ok(format!("snap-{seq}-{ts}"))
}

/// Parse the seq out of a finalized directory name `snap-<seq>-<ts>`.
/// Returns `None` for `*.partial` dirs and anything not matching.
pub(crate) fn seq_of_dir_name(name: &str) -> Option<u64> {
    if name.ends_with(PARTIAL_SUFFIX) {
        return None;
    }
    name.strip_prefix("snap-")
        .and_then(|rest| rest.split_once('-'))
        .and_then(|(seq, _ts)| seq.parse::<u64>().ok())
}

/// Next sequence number for the store: `max(existing seq) + 1`, or 1 for an
/// empty/absent store. Crashed `*.partial` dirs do not consume a seq.
pub(crate) fn next_seq(store_dir: &Path) -> Result<u64> {
    let mut max = 0u64;
    match std::fs::read_dir(store_dir) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry.map_err(|e| SnapshotError::io(store_dir, e))?;
                if let Some(name) = entry.file_name().to_str() {
                    if let Some(seq) = seq_of_dir_name(name) {
                        max = max.max(seq);
                    }
                }
            }
        }
        // A not-yet-created store is simply empty → start at 1.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(SnapshotError::io(store_dir, e)),
    }
    Ok(max + 1)
}

/// Write `meta.json` into a snapshot directory.
pub(crate) fn write_meta(dir: &Path, meta: &SnapshotMeta) -> Result<()> {
    let path = dir.join(META_FILE);
    let bytes = serde_json::to_vec_pretty(meta).map_err(|e| SnapshotError::BadMeta {
        path: path.clone(),
        detail: format!("serialize meta.json: {e}"),
    })?;
    std::fs::write(&path, bytes).map_err(|e| SnapshotError::io(path, e))
}

/// Read `meta.json` from a snapshot directory.
pub(crate) fn read_meta(dir: &Path) -> Result<SnapshotMeta> {
    let path = dir.join(META_FILE);
    let bytes = std::fs::read(&path).map_err(|e| SnapshotError::io(path.clone(), e))?;
    serde_json::from_slice(&bytes).map_err(|e| SnapshotError::BadMeta {
        path,
        detail: format!("parse meta.json: {e}"),
    })
}

/// List finalized snapshots in `store_dir`, newest seq first. A directory
/// whose `meta.json` is missing/unreadable is **skipped with a warning**
/// rather than failing the whole list — one corrupt sidecar must not hide
/// every other rollback point (`[[fail-loud]]` without taking the operator
/// down). An absent store lists empty.
pub fn list_snapshots(store_dir: &Path) -> Result<Vec<SnapshotRecord>> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(store_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(SnapshotError::io(store_dir, e)),
    };
    for entry in entries {
        let entry = entry.map_err(|e| SnapshotError::io(store_dir, e))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if seq_of_dir_name(name).is_none() {
            continue;
        }
        let dir = entry.path();
        match read_meta(&dir) {
            Ok(meta) => out.push(SnapshotRecord { dir, meta }),
            Err(e) => tracing::warn!(
                dir = %dir.display(),
                error = %e,
                "snapshot directory has unreadable meta.json — skipping it in the listing"
            ),
        }
    }
    out.sort_by(|a, b| b.meta.seq.cmp(&a.meta.seq));
    Ok(out)
}

/// Find a snapshot by seq (`"42"`) or by exact directory name
/// (`"snap-42-20260615-143000"`). Used by the restore CLI.
pub fn find_snapshot(store_dir: &Path, selector: &str) -> Result<SnapshotRecord> {
    let records = list_snapshots(store_dir)?;
    if let Ok(seq) = selector.parse::<u64>() {
        if let Some(r) = records.iter().find(|r| r.meta.seq == seq) {
            return Ok(r.clone());
        }
    }
    if let Some(r) = records
        .iter()
        .find(|r| r.dir.file_name().and_then(|n| n.to_str()) == Some(selector))
    {
        return Ok(r.clone());
    }
    // Also accept a timestamp substring match (e.g. "20260615-143000").
    let matches: Vec<&SnapshotRecord> = records
        .iter()
        .filter(|r| {
            r.dir
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.contains(selector))
        })
        .collect();
    match matches.as_slice() {
        [one] => Ok((*one).clone()),
        _ => Err(SnapshotError::NotFound(selector.to_string())),
    }
}

/// Sum of regular-file sizes directly inside `dir` (the export is flat).
pub(crate) fn dir_size(dir: &Path) -> Result<u64> {
    let mut total = 0u64;
    for entry in std::fs::read_dir(dir).map_err(|e| SnapshotError::io(dir, e))? {
        let entry = entry.map_err(|e| SnapshotError::io(dir, e))?;
        let meta = entry.metadata().map_err(|e| SnapshotError::io(dir, e))?;
        if meta.is_file() {
            total += meta.len();
        }
    }
    Ok(total)
}
