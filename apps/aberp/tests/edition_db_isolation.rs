//! ADR-0093 chunk 2 — compile-time edition→data-root binding + isolation.
//!
//! Pins the saw-off invariants the cut-gate enforces mechanically:
//!   1. own-root resolution   — the edition resolves its OWN `~/.aberp-<ed>/`.
//!   2. never-resolves-prod    — NO edition path ever lands under `~/.aberp/prod`.
//!   3. can't-cross            — a build physically refuses prod's root AND the
//!                               sibling edition's root, however the path arrives.
//!   4. fresh-start            — a fresh edition install is detected on its own
//!                               root (so the bundled demo seeds, never inheriting
//!                               prod's billing data).
//!
//! These call the library directly (the binding lives in
//! `aberp::build_profile` + `aberp::tenant_registry`), plus one spawn test
//! proving `serve` refuses a foreign `--db` before it touches a port/keychain.

use std::path::Path;

use aberp::build_profile::{self, edition_data_dirname};
use aberp::tenant_registry::{
    aberp_root, ensure_db_path_isolated, is_fresh_install, tenant_db_path,
};

// ── 1. own-root resolution ───────────────────────────────────────────────

#[test]
fn edition_resolves_its_own_root() {
    let root = aberp_root().expect("HOME/USERPROFILE is set in the test env");
    let root_s = root.to_string_lossy();
    // Compile-time edition dir, e.g. `.aberp-portable` / `.aberp-defense`.
    assert!(
        root_s.ends_with(edition_data_dirname()),
        "edition root {root_s} must end with {}",
        edition_data_dirname()
    );
    // The own dir is one of the two sibling editions, never prod's `.aberp`.
    assert_ne!(edition_data_dirname(), build_profile::PROD_DATA_DIRNAME);
    assert!(edition_data_dirname().starts_with(".aberp-"));
}

// ── 2. never-resolves-prod (the cornerstone assertion) ───────────────────

#[test]
fn no_edition_path_ever_resolves_under_prod_root() {
    // The edition root never sits under the frozen prod base `~/.aberp/`.
    let root = aberp_root().expect("HOME set");
    let root_s = root.to_string_lossy();
    assert!(
        !root_s.contains("/.aberp/"),
        "edition root must not sit under prod's ~/.aberp/: {root_s}"
    );
    assert!(
        !root_s.ends_with("/.aberp"),
        "edition root must not be prod's ~/.aberp: {root_s}"
    );

    // Even the LITERAL `prod` slug stays under the edition's own root —
    // it can never resolve to prod's `~/.aberp/prod/aberp.duckdb`.
    let dbp = tenant_db_path("prod").expect("HOME set");
    let dbp_s = dbp.to_string_lossy();
    assert!(
        !dbp_s.contains("/.aberp/prod/"),
        "a build must never resolve prod's DB path; got {dbp_s}"
    );
    assert!(
        dbp_s.contains(edition_data_dirname()),
        "the path must live under the edition root; got {dbp_s}"
    );
}

// ── 3. can't-cross (prod + sibling refused; own + dev allowed) ───────────

#[test]
fn prod_db_root_is_refused_by_every_edition() {
    // Prod's `~/.aberp/...` is foreign to EVERY edition, unconditionally.
    assert!(ensure_db_path_isolated(Path::new("/Users/op/.aberp/prod/aberp.duckdb")).is_err());
    assert!(ensure_db_path_isolated(Path::new("/home/x/.aberp/demo/aberp.duckdb")).is_err());
    assert!(ensure_db_path_isolated(Path::new("/Users/op/.aberp/tenants.toml")).is_err());
}

#[test]
fn own_root_and_ordinary_dev_paths_are_allowed() {
    // The edition's OWN derived path is fine.
    let own = tenant_db_path("acme").expect("HOME set");
    assert!(ensure_db_path_isolated(&own).is_ok());
    // Relative / temp paths carry no `.aberp*` component → allowed (dev loop,
    // CLI tools, tests keep working).
    assert!(ensure_db_path_isolated(Path::new("./aberp.duckdb")).is_ok());
    assert!(ensure_db_path_isolated(Path::new("/tmp/whatever/aberp.duckdb")).is_ok());
}

#[cfg(not(feature = "production"))]
#[test]
fn portable_refuses_defense_sibling_and_prod_but_keeps_own() {
    // Portable build: prod AND Defense are foreign; Portable is own.
    assert!(ensure_db_path_isolated(Path::new("/Users/x/.aberp/prod/aberp.duckdb")).is_err());
    assert!(
        ensure_db_path_isolated(Path::new("/Users/x/.aberp-defense/acme/aberp.duckdb")).is_err()
    );
    assert!(
        ensure_db_path_isolated(Path::new("/Users/x/.aberp-portable/acme/aberp.duckdb")).is_ok()
    );
}

#[cfg(feature = "production")]
#[test]
fn defense_refuses_portable_sibling_and_prod_but_keeps_own() {
    // Defense build: prod AND Portable are foreign; Defense is own.
    assert!(ensure_db_path_isolated(Path::new("/Users/x/.aberp/prod/aberp.duckdb")).is_err());
    assert!(
        ensure_db_path_isolated(Path::new("/Users/x/.aberp-portable/acme/aberp.duckdb")).is_err()
    );
    assert!(
        ensure_db_path_isolated(Path::new("/Users/x/.aberp-defense/acme/aberp.duckdb")).is_ok()
    );
}

// ── 4. fresh-start (on the edition's own root, never inheriting prod) ─────

#[test]
fn fresh_edition_install_detected_then_not() {
    let base = std::env::temp_dir().join(format!(
        "aberp-edition-fresh-{}-{}",
        std::process::id(),
        edition_data_dirname()
    ));
    let root = base.join(edition_data_dirname());
    let _ = std::fs::remove_dir_all(&base);

    // No edition root on disk yet → genuinely fresh (demo will seed).
    assert!(is_fresh_install(&root).expect("scan empty root"));

    // A tenant DB under the edition root → no longer fresh.
    std::fs::create_dir_all(root.join("demo")).unwrap();
    std::fs::write(root.join("demo").join("aberp.duckdb"), b"x").unwrap();
    assert!(!is_fresh_install(&root).expect("scan populated root"));

    let _ = std::fs::remove_dir_all(&base);
}

// ── serve refuses a foreign --db (env/misconfig backstop) ────────────────

#[test]
#[cfg(not(feature = "production"))]
fn serve_refuses_foreign_db_path_before_binding() {
    use std::process::Command;
    // `--tenant demo` clears the tenant guard; the foreign `--db` (prod's
    // `.aberp` base) must then be refused by the DB-binding guard, exiting
    // (1) before any port/keychain/DB access.
    let out = Command::new(env!("CARGO_BIN_EXE_aberp"))
        .args(["serve", "--tenant", "demo"])
        .args(["--db", "/tmp/aberp-iso-test/.aberp/prod/aberp.duckdb"])
        .output()
        .expect("spawn `aberp serve` with a foreign --db");

    assert!(
        !out.status.success(),
        "serve must REFUSE a foreign --db; got success exit"
    );
    assert_eq!(out.status.code(), Some(1), "DB-binding guard must exit(1)");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("edition isolation") || stderr.contains("ADR-0093"),
        "stderr must name the edition-isolation refusal; got:\n{stderr}"
    );
}

// ── KNOWN GAP — symlinked foreign root walks through the guard ───────────
//
// `ensure_db_path_isolated` compares path COMPONENT NAMES and never resolves
// symlinks, so `<dir>/link/prod/aberp.duckdb` with `link -> <dir>/.aberp`
// carries no `.aberp` component and is accepted — the build then opens (and
// WALs) the database inside the foreign root. Proven end-to-end in S2:
// see `docs/findings/s2-aberp-db-symlink-escapes-edition-isolation.md`.
//
// This test asserts the behaviour the guard's own doc comment promises
// ("refuses ... no matter how the path arrived", "resolves into a FOREIGN
// edition's root"). It FAILS today, so it is `#[ignore]`d rather than left to
// red the gates — S2's brief was to report the finding, not to change a
// load-bearing security guard. Whoever lands the fix (canonicalize before the
// component walk) should drop the `#[ignore]` in the same commit.
//
// Run explicitly with:
//   cargo test --test edition_db_isolation -- --ignored
#[test]
#[ignore = "KNOWN GAP: symlink bypasses the foreign-root guard — see \
            docs/findings/s2-aberp-db-symlink-escapes-edition-isolation.md"]
#[cfg(not(feature = "production"))]
fn foreign_root_reached_through_a_symlink_is_refused() {
    let base = std::env::temp_dir().join("aberp-iso-symlink-test");
    let _ = std::fs::remove_dir_all(&base);
    let foreign = base.join(".aberp").join("prod");
    std::fs::create_dir_all(&foreign).expect("create simulated foreign root");

    // A link whose OWN name is innocuous, pointing at the forbidden root.
    let link = base.join("sneaky");
    #[cfg(unix)]
    std::os::unix::fs::symlink(base.join(".aberp"), &link).expect("symlink");

    // Resolves to <base>/.aberp/prod/aberp.duckdb — a foreign root.
    let disguised = link.join("prod").join("aberp.duckdb");

    let verdict = ensure_db_path_isolated(&disguised);
    let _ = std::fs::remove_dir_all(&base);

    assert!(
        verdict.is_err(),
        "a path that RESOLVES into the foreign prod root must be refused, \
         however it is spelled; got Ok for {}",
        disguised.display()
    );
}
