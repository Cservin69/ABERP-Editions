//! `aberp-verify` — bundle verifier library face (PR-22, ADR-0035).
//!
//! Re-verifies a per-invoice export bundle (`.tar.zst` per ADR-0029
//! §3) from its own bytes alone. The verifier:
//!
//!   1. Decompresses + untars the archive (`bundle::read_archive`).
//!   2. Parses the manifest and chain.jsonl
//!      (`bundle::parse_manifest`, `bundle::parse_chain_jsonl`).
//!   3. Runs the §3 invariant list per ADR-0035 against the parsed
//!      slice + the gathered `nav/<seq>_<kind>.xml` files
//!      (`verify::run_checks`).
//!   4. Composes the operator-visible report
//!      (`report::Report::print`) and exits 0 on all-OK, 1 on any
//!      FAIL per ADR-0035 §7.
//!
//! # What the verifier does NOT do
//!
//!   - Does NOT call NAV. The bundle is the universe of bytes
//!     consulted.
//!   - Does NOT open any DB. No DuckDB connection; the
//!     `aberp-audit-ledger` transitive duckdb cost is compile-time
//!     only per ADR-0035 §"Adversarial review" #4.
//!   - Does NOT verify a signature. The bundle is unsigned per
//!     ADR-0029 §4 (F5 deferred); the verifier asserts
//!     `manifest.signed == false` and
//!     `manifest.signature_status == "deferred-per-f5"` to catch
//!     a bundle that claims signed without the verifier knowing how
//!     to verify.
//!   - Does NOT cross-check the mirror file (the mirror lives
//!     outside the bundle per ADR-0030; the verifier echoes
//!     `manifest.mirror_file_status` without independent
//!     re-verification per ADR-0035 §"Adversarial review" #5).
//!   - Does NOT write any audit-ledger entry. Read-only per the
//!     same posture every `export-*` and `verify-*` verb uses.

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod bundle;
pub mod report;
pub mod verify;

use std::path::Path;

use anyhow::{Context, Result};

pub use report::Report;

/// Verify a bundle file at `bundle_path`. Returns the populated
/// [`Report`] on success (any per-check FAIL surfaces in the report;
/// the caller decides how to act on it — typically `report.is_ok()`
/// drives the process exit code per ADR-0035 §7).
///
/// Returns `Err(_)` only on STRUCTURAL failures the verifier cannot
/// continue past: file-not-found, malformed archive bytes, or a
/// manifest that fails JSON parse. Semantic failures (chain link
/// broken, hash mismatch, XML root mismatch) become FAIL entries on
/// the report rather than `Err(_)` — the verifier surfaces every
/// check that ran so the operator sees the full picture per
/// CLAUDE.md rule 12 + ADR-0035 §7.
pub fn verify_bundle(bundle_path: &Path) -> Result<Report> {
    let _span = tracing::info_span!("verify_bundle", bundle = %bundle_path.display()).entered();
    let archive = bundle::read_archive(bundle_path)
        .with_context(|| format!("read bundle archive at {}", bundle_path.display()))?;
    let report = verify::run_checks(bundle_path, &archive);
    Ok(report)
}
