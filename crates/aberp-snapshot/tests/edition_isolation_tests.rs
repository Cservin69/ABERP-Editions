//! ADR-0093 chunk 3 — snapshot/restore edition isolation.
//!
//! Proves the crate-level guarantees the sawed-off editions tree relies on:
//!   1. `edition_store_dir` is edition-scoped and provably DISJOINT from
//!      prod's `~/Documents/ABERP-snapshots/` store.
//!   2. `ensure_not_prod_path` refuses prod's DB root AND prod's snapshot
//!      store, while allowing the edition's own roots/stores.
//!   3. `ensure_restore_allowed` refuses any restore target under a live
//!      `~/.aberp*` home (prod OR an edition) and the prod snapshot store,
//!      but permits a side path with `--confirm`.
//!
//! These are pure path-decision tests (no DuckDB), but they live in the
//! crate's `tests/` so they run under `cargo test -p aberp-snapshot` on the
//! Mac gate alongside the DuckDB round-trip tests.

use std::path::PathBuf;

use aberp_snapshot::{
    default_store_dir, edition_store_dir, ensure_not_prod_path, ensure_restore_allowed,
};

fn home() -> PathBuf {
    std::env::var("HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/home/tester"))
}

// ── 1. edition_store_dir is edition-scoped + disjoint from prod ───────────

#[test]
fn edition_store_dir_is_edition_scoped_and_disjoint_from_prod() {
    let defense = edition_store_dir("defense", "acme").expect("HOME set");
    let portable = edition_store_dir("portable", "acme").expect("HOME set");
    let prod = default_store_dir("acme").expect("HOME set");

    assert!(
        defense.ends_with("Documents/ABERP-snapshots-defense/acme"),
        "defense store {} must be ABERP-snapshots-defense/<tenant>",
        defense.display()
    );
    assert!(portable.ends_with("Documents/ABERP-snapshots-portable/acme"));

    // Disjoint from prod's store: neither edition store is nested under
    // prod's `ABERP-snapshots/`, and vice versa.
    assert!(
        !defense.starts_with(&prod),
        "defense store must not be under prod's"
    );
    assert!(
        !prod.starts_with(&defense),
        "prod store must not be under defense's"
    );
    assert!(!portable.starts_with(&prod));
    // …and never under a live DB root.
    for p in [&defense, &portable] {
        let s = p.to_string_lossy();
        assert!(
            !s.contains("/.aberp/"),
            "store must not sit under ~/.aberp/: {s}"
        );
    }
}

// ── 2. ensure_not_prod_path: prod refused, edition allowed ────────────────

#[test]
fn ensure_not_prod_path_refuses_prod_db_root() {
    let prod_db = home().join(".aberp").join("prod").join("aberp.duckdb");
    let err = ensure_not_prod_path(&prod_db).expect_err("prod DB root must be refused");
    assert!(
        err.to_string().contains(".aberp"),
        "msg names ~/.aberp/: {err}"
    );
}

#[test]
fn ensure_not_prod_path_refuses_prod_snapshot_store() {
    let prod_store = home()
        .join("Documents")
        .join("ABERP-snapshots")
        .join("acme");
    let err = ensure_not_prod_path(&prod_store).expect_err("prod snapshot store must be refused");
    assert!(
        err.to_string().contains("ABERP-snapshots"),
        "msg names the store: {err}"
    );
}

#[test]
fn ensure_not_prod_path_allows_edition_roots_and_stores() {
    // The edition's own DB root and snapshot store are NOT prod surfaces.
    let def_db = home()
        .join(".aberp-defense")
        .join("acme")
        .join("aberp.duckdb");
    let def_store = home()
        .join("Documents")
        .join("ABERP-snapshots-defense")
        .join("acme");
    let por_store = home()
        .join("Documents")
        .join("ABERP-snapshots-portable")
        .join("acme");
    ensure_not_prod_path(&def_db).expect("edition DB root is allowed");
    ensure_not_prod_path(&def_store).expect("edition store is allowed");
    ensure_not_prod_path(&por_store).expect("edition store is allowed");
}

// ── 3. ensure_restore_allowed end-to-end ──────────────────────────────────

#[test]
fn ensure_restore_allowed_refuses_prod_and_edition_homes() {
    // prod live DB — refused.
    let prod = home().join(".aberp").join("prod").join("aberp.duckdb");
    assert!(ensure_restore_allowed(&prod, true).is_err());
    // edition's OWN live DB home — also refused (restore to a side path).
    let edition_home = home()
        .join(".aberp-defense")
        .join("acme")
        .join("aberp.duckdb");
    assert!(
        ensure_restore_allowed(&edition_home, true).is_err(),
        "must refuse restoring directly onto a live edition DB"
    );
    // prod snapshot store as a target — refused.
    let prod_store = home()
        .join("Documents")
        .join("ABERP-snapshots")
        .join("x.duckdb");
    assert!(ensure_restore_allowed(&prod_store, true).is_err());
}

#[test]
fn ensure_restore_allowed_permits_side_path_with_confirm() {
    let side = PathBuf::from("/tmp/aberp-recovery/aberp.duckdb");
    ensure_restore_allowed(&side, true).expect("side path + confirm is allowed");
    // …but never without --confirm.
    assert!(ensure_restore_allowed(&side, false).is_err());
}
