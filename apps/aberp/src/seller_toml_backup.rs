//! PR-170 / session-170 — defense-in-depth seller.toml snapshot.
//!
//! Every atomic seller.toml write (identity, banks, smtp, numbering)
//! routes through [`snapshot_and_rotate`] FIRST so the prior file body
//! is preserved as `<dir>/.seller.toml.backup-<unix-timestamp>` before
//! the new bytes land. Rotation keeps the 5 newest backups and silently
//! drops older ones.
//!
//! # Why
//!
//! S170 production-update regression: Ervin's PROD_v1.0 → PROD_v1.1
//! upgrade silently lost `[seller.smtp]` + `[seller.numbering]` because
//! the identity-only writer rendered the file from scratch without
//! re-appending those sections. PR-170 fixes the root cause (the
//! identity writer now preserves all three secondary sections), but the
//! cost of Ervin's diagnosis time + re-typing was real. This backup
//! posture is the belt+suspenders: the NEXT class-of-bug that clobbers
//! seller.toml leaves a 30-second recovery path (`cp .seller.toml.backup-N
//! seller.toml`) rather than a hand-rebuild from memory.
//!
//! # Failure posture
//!
//! Backup is best-effort. A copy or prune failure logs via
//! `tracing::warn` and returns `Ok(())` so the underlying write is
//! never blocked by a backup-helper bug. The goal is "if it works we
//! have a recovery handle"; a backup that itself fails MUST NOT cost
//! the operator their save. CLAUDE.md rule 12 (fail loud) does not
//! apply here — the recovery handle is the bonus, not the contract.

use std::fs;
use std::path::Path;

use anyhow::Result;

/// Number of `<dotted-stem>.backup-*` snapshots to keep per file. The
/// 5-file ceiling matches "a few recent writes" without growing the
/// tenant directory unbounded under a chatty UI (each Tenant Settings
/// save triggers one snapshot; 5 covers a normal cluster of related
/// edits without spilling weeks of dormant state into the prod home).
pub const BACKUP_RETENTION: usize = 5;

/// Snapshot `path` to a sibling `<dir>/.<filename>.backup-<unix-secs>`
/// before the caller replaces it, then prune older sibling backups
/// keeping only the [`BACKUP_RETENTION`] newest.
///
/// Returns `Ok(())` unconditionally — see the module docs for the
/// best-effort posture rationale. Logs via `tracing::warn` on
/// per-step failure so the operator (or future debugger) has a
/// trail without the underlying write being aborted.
pub fn snapshot_and_rotate(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let parent = match path.parent() {
        Some(p) => p,
        None => {
            tracing::warn!(
                target = %path.display(),
                "snapshot_and_rotate: target has no parent dir; skipping backup"
            );
            return Ok(());
        }
    };
    let filename = match path.file_name().and_then(|n| n.to_str()) {
        Some(n) => n,
        None => {
            tracing::warn!(
                target = %path.display(),
                "snapshot_and_rotate: target file_name is non-utf8; skipping backup"
            );
            return Ok(());
        }
    };
    let prefix = format!(".{filename}.backup-");
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let backup_path = parent.join(format!("{prefix}{ts}"));

    if let Err(e) = fs::copy(path, &backup_path) {
        tracing::warn!(
            target = %path.display(),
            backup = %backup_path.display(),
            error = %e,
            "snapshot_and_rotate: copy failed; backup skipped (write continues)"
        );
        return Ok(());
    }

    // Best-effort 0600 on the backup so it matches the source's
    // permission posture (the live seller.toml is chmod 0600).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(&backup_path) {
            let mut perms = meta.permissions();
            perms.set_mode(0o600);
            let _ = fs::set_permissions(&backup_path, perms);
        }
    }

    prune_old_backups(parent, &prefix, BACKUP_RETENTION);
    Ok(())
}

/// Walk `parent` for files whose name starts with `prefix`, sort by
/// name (the unix-seconds suffix orders chronologically), and
/// `remove_file` the oldest entries beyond `keep`. Silently tolerates
/// missing-dir / unreadable-entry failures.
fn prune_old_backups(parent: &Path, prefix: &str, keep: usize) {
    let entries = match fs::read_dir(parent) {
        Ok(e) => e,
        Err(_) => return,
    };
    let mut backups: Vec<std::path::PathBuf> = entries
        .flatten()
        .filter_map(|e| {
            let n = e.file_name();
            let s = n.to_str()?;
            if s.starts_with(prefix) {
                Some(e.path())
            } else {
                None
            }
        })
        .collect();
    backups.sort();
    if backups.len() > keep {
        let to_drop = backups.len() - keep;
        for p in &backups[..to_drop] {
            if let Err(e) = fs::remove_file(p) {
                tracing::warn!(
                    backup = %p.display(),
                    error = %e,
                    "snapshot_and_rotate: prune failed; leaving stale backup in place"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join("aberp-seller-toml-backup-test")
            .join(format!("{label}-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    #[test]
    fn no_op_when_target_missing() {
        let dir = test_dir("no_op_missing");
        let target = dir.join("seller.toml");
        assert!(!target.exists());
        snapshot_and_rotate(&target).expect("no-op succeeds");
        // No `.seller.toml.backup-*` should exist.
        let count = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(count, 0, "no files materialised by no-op");
    }

    #[test]
    fn creates_backup_copy_with_body_intact() {
        let dir = test_dir("creates_copy");
        let target = dir.join("seller.toml");
        let body = "[seller]\nlegal_name = \"X\"\n";
        std::fs::write(&target, body).unwrap();
        snapshot_and_rotate(&target).unwrap();

        let prefix = ".seller.toml.backup-";
        let backups: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(prefix))
            .collect();
        assert_eq!(backups.len(), 1, "exactly one backup created");
        let backup_body = std::fs::read_to_string(backups[0].path()).unwrap();
        assert_eq!(backup_body, body, "backup body matches source");
    }

    #[test]
    fn rotation_keeps_only_newest_n_backups() {
        let dir = test_dir("rotation");
        let target = dir.join("seller.toml");
        std::fs::write(&target, "initial").unwrap();

        let prefix = ".seller.toml.backup-";
        // Manually plant BACKUP_RETENTION + 3 older backups with
        // monotonic-ascending suffixes. The rotation runs at the END of
        // snapshot_and_rotate, so we plant the older ones with smaller
        // timestamps, then call snapshot_and_rotate once — it will add
        // one NEW backup and prune to keep only BACKUP_RETENTION.
        for i in 1..=BACKUP_RETENTION + 3 {
            // Pad to 10 chars so lexicographic sort matches numeric sort.
            let name = format!("{prefix}{:010}", i);
            std::fs::write(dir.join(name), "old").unwrap();
        }
        let pre = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(prefix))
            .count();
        assert_eq!(pre, BACKUP_RETENTION + 3);

        snapshot_and_rotate(&target).unwrap();

        let post: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(prefix))
            .collect();
        assert_eq!(
            post.len(),
            BACKUP_RETENTION,
            "rotation prunes to retention ceiling: got {} backups, expected {}",
            post.len(),
            BACKUP_RETENTION
        );
        // The newest BACKUP_RETENTION must remain — the smallest-named
        // (oldest) ones were the planted "1..3", which should be gone.
        let names: Vec<String> = post
            .iter()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            !names.iter().any(|n| n == &format!("{prefix}{:010}", 1)),
            "oldest planted backup must be pruned: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n == &format!("{prefix}{:010}", 3)),
            "third-oldest planted backup must be pruned: {names:?}"
        );
    }

    #[test]
    fn does_not_disturb_unrelated_files_in_dir() {
        let dir = test_dir("unrelated");
        let target = dir.join("seller.toml");
        std::fs::write(&target, "body").unwrap();
        // Plant unrelated files that share NOTHING with the backup prefix.
        std::fs::write(dir.join("aberp.duckdb"), "db").unwrap();
        std::fs::write(dir.join(".first-launch-acknowledged"), "ack").unwrap();
        // Plant a sibling tempfile from a write-atomic call mid-flight —
        // this MUST NOT be treated as a backup (different prefix).
        std::fs::write(dir.join(".seller.toml.tmp.12345"), "tmp").unwrap();

        snapshot_and_rotate(&target).unwrap();

        // All unrelated files must still exist.
        assert!(dir.join("aberp.duckdb").exists());
        assert!(dir.join(".first-launch-acknowledged").exists());
        assert!(dir.join(".seller.toml.tmp.12345").exists());
    }
}
