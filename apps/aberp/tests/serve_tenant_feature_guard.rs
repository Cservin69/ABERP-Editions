//! S165 / prod-prep PR #1 — pins the hülye-biztos cross-stream guard in
//! `serve::run` (`guard_tenant_matches_build`). The guard is the FIRST
//! statement in the serve boot path, before the binary-hash thread, the
//! keychain, or the DB are touched, so a mismatched launch dies instantly
//! with a remediation hint instead of silently talking to the wrong NAV
//! environment (CLAUDE.md rule 12).
//!
//! This integration test exec's the REAL built `aberp` binary with a
//! mismatched env and asserts the process refuses to start. The CI gate
//! (`cargo test --workspace`, default features) builds the DEV flavour,
//! Under ADR-0093 BOTH editions refuse the frozen prod line's reserved
//! `prod` tenant (the old "PROD build hard-locks tenant==prod" binding that
//! pointed Defense at `~/.aberp/prod/` is removed). This test pins the
//! Portable (default-feature) arm: a non-prod build refuses `--tenant prod`.
//! The compile-time edition→root binding is pinned in `build_profile::tests`
//! and `tests/edition_db_isolation.rs`.

#[cfg(not(feature = "production"))]
use std::process::Command;

/// A dev build (no `production` feature) launched as `--tenant prod`
/// must exit non-zero BEFORE binding a port or reading the keychain,
/// and name the mismatch + the remediation in stderr.
#[test]
#[cfg(not(feature = "production"))]
fn dev_build_refuses_tenant_prod() {
    let output = Command::new(env!("CARGO_BIN_EXE_aberp"))
        .args(["serve", "--tenant", "prod"])
        // A bogus DB path is harmless: the guard fires before any DB or
        // keychain access, so the process never reaches it.
        .args(["--db", "/nonexistent/aberp-guard-test.duckdb"])
        .output()
        .expect("spawn `aberp serve --tenant prod`");

    assert!(
        !output.status.success(),
        "dev build must REFUSE to start as tenant=prod; got success exit"
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "guard must exit(1); got {:?}",
        output.status.code()
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("reserved tenant") && stderr.contains("ADR-0093"),
        "stderr must name the reserved-prod-tenant refusal; got:\n{stderr}"
    );
    assert!(
        stderr.contains("run/run_portable.sh"),
        "stderr must steer the operator to an editions launcher; got:\n{stderr}"
    );
}
