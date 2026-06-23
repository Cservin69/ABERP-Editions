//! S166 / prod-prep PR #2 — the one-time first-production-launch
//! acknowledgement touchfile.
//!
//! A production binary submits invoices to NAV's REAL endpoint with real
//! credentials. Before the very first launch can proceed to the normal
//! app, the operator must consent at a blocking confirmation modal (typed
//! `ABERP`). That consent is recorded as a touchfile:
//!
//!   `~/.aberp/<tenant>/.first-launch-acknowledged`
//!
//! one line, an RFC3339 timestamp. The file's PRESENCE is the gate; its
//! contents are a human-readable record only. The touchfile is namespaced
//! by tenant for the same hülye-biztos symmetry as every other
//! `~/.aberp/<tenant>/` artifact: a prod acknowledgement never satisfies a
//! different tenant's gate.
//!
//! Dev/test builds never gate on this — [`first_prod_launch_required`]
//! is `false` whenever `IS_PRODUCTION_BUILD` is false, regardless of the
//! touchfile, so the dev loop is unchanged.
//!
//! The path-resolving wrappers ([`is_acknowledged`], [`write_acknowledgement`])
//! delegate to path-based cores ([`is_acknowledged_at`],
//! [`write_acknowledgement_at`]) so the on-disk logic is unit-testable
//! without mutating the process-global `HOME`.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

use crate::build_profile::IS_PRODUCTION_BUILD;

/// File name of the per-tenant acknowledgement touchfile.
const TOUCHFILE_NAME: &str = ".first-launch-acknowledged";

/// Resolve `~/.aberp/<tenant>/.first-launch-acknowledged` for `tenant`.
///
/// Mirrors `setup_seller_info::seller_toml_path_for_tenant` so the two
/// artifacts share one base-path convention (CLAUDE.md rule 8).
pub fn touchfile_path(tenant: &str) -> Result<PathBuf> {
    let home = std::env::var("HOME").map_err(|_| {
        anyhow!("HOME environment variable not set; cannot resolve first-launch touchfile path")
    })?;
    Ok(PathBuf::from(home)
        .join(crate::build_profile::edition_data_dirname())
        .join(tenant)
        .join(TOUCHFILE_NAME))
}

/// Path-based core of [`is_acknowledged`]: the gate is the file's
/// presence.
pub fn is_acknowledged_at(path: &Path) -> bool {
    path.exists()
}

/// `true` iff the first-launch ceremony has already been completed for
/// `tenant` (the touchfile exists). A path-resolution failure (no `HOME`)
/// is treated as "not acknowledged" so the gate fails CLOSED — better to
/// re-show the confirmation than to silently skip it.
pub fn is_acknowledged(tenant: &str) -> bool {
    match touchfile_path(tenant) {
        Ok(path) => is_acknowledged_at(&path),
        Err(_) => false,
    }
}

/// Pure decision core: the SPA must block its main routes iff this is a
/// production build AND the ceremony has not been acknowledged. Factored
/// out so the full truth table is unit-testable — the real entry point
/// feeds it the compile-time `IS_PRODUCTION_BUILD`, which a dev test
/// binary cannot flip.
pub fn first_prod_launch_required_for(is_production: bool, acknowledged: bool) -> bool {
    is_production && !acknowledged
}

/// Whether the SPA must block its main routes behind the one-time
/// first-production-launch confirmation. True ONLY on a production build
/// whose acknowledgement touchfile is absent. Dev/test builds always
/// return false. This is the single predicate the `/health` route and the
/// boot sanity check both read.
pub fn first_prod_launch_required(tenant: &str) -> bool {
    first_prod_launch_required_for(IS_PRODUCTION_BUILD, is_acknowledged(tenant))
}

/// Path-based core of [`write_acknowledgement`]: write `acknowledged_at`
/// (an RFC3339 instant) to `path`, creating the parent directory if
/// needed. Idempotent.
pub fn write_acknowledgement_at(path: &Path, acknowledged_at: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "create parent directory {} for first-launch touchfile",
                parent.display()
            )
        })?;
    }
    std::fs::write(path, format!("{acknowledged_at}\n"))
        .with_context(|| format!("write first-launch touchfile at {}", path.display()))?;
    Ok(())
}

/// Write the acknowledgement touchfile for `tenant`, stamping it with
/// `acknowledged_at` (an RFC3339 instant). Creates the parent
/// `~/.aberp/<tenant>/` directory if needed. Idempotent: re-acknowledging
/// simply rewrites the stamp, which is harmless.
pub fn write_acknowledgement(tenant: &str, acknowledged_at: &str) -> Result<()> {
    let path = touchfile_path(tenant)?;
    write_acknowledgement_at(&path, acknowledged_at)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Unique scratch path under the OS temp dir — no `tempfile` dep, no
    /// `HOME` mutation (so these tests never race other env-reading tests
    /// in the same binary). Uniqueness comes from pid + a process-local
    /// counter, not randomness.
    fn scratch_touchfile() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir()
            .join(format!(
                "aberp-s166-first-launch-{}-{n}",
                std::process::id()
            ))
            .join("prod")
            .join(TOUCHFILE_NAME)
    }

    #[test]
    fn absent_touchfile_is_not_acknowledged() {
        let path = scratch_touchfile();
        assert!(!is_acknowledged_at(&path));
    }

    /// The `/health` `first_prod_launch_required` truth table — the
    /// brief's three /health scenarios, pinned at the pure seam since the
    /// real entry point reads the compile-time `IS_PRODUCTION_BUILD`.
    #[test]
    fn first_prod_launch_required_truth_table() {
        // prod + no touchfile → blocked (modal required)
        assert!(first_prod_launch_required_for(true, false));
        // prod + touchfile present → not required
        assert!(!first_prod_launch_required_for(true, true));
        // dev build → never required, regardless of touchfile
        assert!(!first_prod_launch_required_for(false, false));
        assert!(!first_prod_launch_required_for(false, true));
    }

    #[test]
    fn write_then_read_round_trips_and_is_idempotent() {
        let path = scratch_touchfile();
        assert!(!is_acknowledged_at(&path));
        write_acknowledgement_at(&path, "2026-06-01T08:00:00Z").unwrap();
        assert!(is_acknowledged_at(&path));
        // Idempotent — a second write must not fail and rewrites the stamp.
        write_acknowledgement_at(&path, "2026-06-01T09:00:00Z").unwrap();
        assert!(is_acknowledged_at(&path));
        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body.trim(), "2026-06-01T09:00:00Z");
        // Clean up the scratch tree.
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }
}
