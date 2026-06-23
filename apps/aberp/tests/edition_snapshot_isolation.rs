//! ADR-0093 chunk 3 — the BINARY's snapshot store is edition-scoped and
//! cannot be pointed at the frozen prod line.
//!
//! Companion to `edition_db_isolation.rs` (chunk 2, the DB roots) and the
//! crate-level `aberp-snapshot/tests/edition_isolation_tests.rs` (the pure
//! guards). Here we pin the *binary* wiring: `snapshot::resolve_store`
//! derives the store from the COMPILE-TIME edition and refuses a
//! hand-passed `--store` that targets prod.

use std::path::PathBuf;

use aberp::build_profile::edition_store_segment;
use aberp::snapshot::resolve_store;

fn home() -> PathBuf {
    std::env::var("HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/home/tester"))
}

#[test]
fn resolve_store_default_is_edition_scoped() {
    let store = resolve_store("acme", None).expect("HOME set");
    let s = store.to_string_lossy();
    // The store segment is this build's compile-time edition, never prod.
    let seg = edition_store_segment();
    assert!(
        seg == "defense" || seg == "portable",
        "editions tree only: {seg}"
    );
    assert!(
        s.contains(&format!("ABERP-snapshots-{seg}")),
        "store must be edition-scoped (ABERP-snapshots-{seg}); got {s}"
    );
    // Never prod's bare store, never under a live ~/.aberp/ root.
    assert!(
        !s.contains("/.aberp/"),
        "store must not sit under prod's ~/.aberp/: {s}"
    );
    assert!(
        !s.ends_with("ABERP-snapshots/acme") && !s.contains("ABERP-snapshots/acme"),
        "store must not be prod's bare ABERP-snapshots/<tenant>: {s}"
    );
}

#[test]
fn resolve_store_refuses_explicit_prod_store() {
    let prod_store = home()
        .join("Documents")
        .join("ABERP-snapshots")
        .join("acme");
    let err = resolve_store("acme", Some(prod_store.as_path()))
        .expect_err("a --store under prod's snapshot store must be refused");
    assert!(
        err.to_string().contains("prod"),
        "error should name the prod refusal: {err}"
    );
}

#[test]
fn resolve_store_refuses_explicit_prod_db_root() {
    let prod_root = home().join(".aberp").join("prod");
    let err = resolve_store("acme", Some(prod_root.as_path()))
        .expect_err("a --store under prod's ~/.aberp/ must be refused");
    assert!(err.to_string().contains("prod"), "error names prod: {err}");
}
