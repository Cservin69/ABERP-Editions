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

// ── CLOSED GAP — a symlinked / `..`-spelled foreign root is refused ──────
//
// `ensure_db_path_isolated` used to compare raw path COMPONENT NAMES and
// never resolve symlinks, so `<dir>/link/prod/aberp.duckdb` with
// `link -> <dir>/.aberp` carried no `.aberp` component and was accepted —
// the build then opened (and WALed) the database inside the foreign root.
// Proven end-to-end in S2:
// see `docs/findings/s2-aberp-db-symlink-escapes-edition-isolation.md`.
//
// The guard now canonicalizes before the component walk (matching
// `ABERP.git` d9b64a2), so these assert the behaviour its doc comment has
// always promised: "refuses ... no matter how the path arrived". They were
// `#[ignore]`d while the gap was open; the `#[ignore]` is dropped with the
// fix, as that note required.
//
// Both arms run these: prod's `~/.aberp/` is foreign to Portable AND to
// Defense, so there is nothing edition-conditional to pin here.

/// Removing `canonicalize_deepest` from the guard turns this red while the
/// direct-component cases above stay green — that asymmetry is what shows
/// the canonicalization carries its own weight.
#[cfg(unix)]
#[test]
fn foreign_root_reached_through_a_symlink_is_refused() {
    let base = std::env::temp_dir().join("aberp-iso-symlink-test");
    let _ = std::fs::remove_dir_all(&base);
    let foreign = base.join(".aberp").join("prod");
    std::fs::create_dir_all(&foreign).expect("create simulated foreign root");

    // A link whose OWN name is innocuous, pointing at the forbidden root.
    let link = base.join("sneaky");
    std::os::unix::fs::symlink(base.join(".aberp"), &link).expect("symlink");

    // Resolves to <base>/.aberp/prod/aberp.duckdb — a foreign root.
    let disguised = link.join("prod").join("aberp.duckdb");

    // Guard against the test silently stopping to exercise the residual.
    assert!(
        !disguised.components().any(|c| c.as_os_str() == ".aberp"),
        "test is not exercising the residual: path still carries a .aberp component"
    );

    let verdict = ensure_db_path_isolated(&disguised);
    let _ = std::fs::remove_dir_all(&base);

    assert!(
        verdict.is_err(),
        "a path that RESOLVES into the foreign prod root must be refused, \
         however it is spelled; got Ok for {}",
        disguised.display()
    );
}

/// A `..` traversal that climbs out of an innocuous directory and back into
/// the foreign root through an aliased name. `..` alone is not an escape —
/// `<base>/work/../.aberp/prod/x` still carries a `.aberp` component and the
/// raw walk caught it. It becomes one only when the climb-out lands on a
/// symlink, so the resolved path names the foreign root and the spelled path
/// never does. Canonicalization resolves both halves at once.
#[cfg(unix)]
#[test]
fn foreign_root_reached_through_a_dotdot_traversal_is_refused() {
    let base = std::env::temp_dir().join("aberp-iso-dotdot-test");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join(".aberp").join("prod")).expect("create foreign root");
    std::fs::create_dir_all(base.join("work").join("nested")).expect("create innocuous dirs");

    // <base>/work/nested/../alias/prod — `alias` is a symlink to the foreign
    // root, so the spelled path carries no `.aberp` component at all.
    std::os::unix::fs::symlink(base.join(".aberp"), base.join("work").join("alias"))
        .expect("symlink");

    let disguised = base
        .join("work")
        .join("nested")
        .join("..")
        .join("alias")
        .join("prod")
        .join("aberp.duckdb");

    assert!(
        !disguised.components().any(|c| c.as_os_str() == ".aberp"),
        "test is not exercising the residual: path still carries a .aberp component"
    );

    let verdict = ensure_db_path_isolated(&disguised);
    let _ = std::fs::remove_dir_all(&base);

    assert!(
        verdict.is_err(),
        "a `..` traversal that resolves into the foreign prod root must be \
         refused; got Ok for {}",
        disguised.display()
    );
}

/// The OTHER direction, and the reason the guard walks both the spelled and
/// the resolved form rather than replacing one with the other. This rule is
/// a deny-list on dirnames, so canonicalizing *instead of* matching raw
/// would lose refusals that work today: if the foreign root is itself a
/// symlink, resolving it strips the `.aberp` component and the path starts
/// passing. Swapping the raw walk out for the resolved one turns this red.
#[cfg(unix)]
#[test]
fn a_symlinked_foreign_root_does_not_become_allowed() {
    let base = std::env::temp_dir().join("aberp-iso-linked-root-test");
    let _ = std::fs::remove_dir_all(&base);
    // The real storage lives under an innocuous name; `.aberp` is the link.
    std::fs::create_dir_all(base.join("elsewhere").join("prod")).expect("create backing dir");
    std::os::unix::fs::symlink(base.join("elsewhere"), base.join(".aberp")).expect("symlink");

    // Spelled with `.aberp`, but it resolves to `<base>/elsewhere/prod/...`,
    // which carries no foreign component at all.
    let spelled = base.join(".aberp").join("prod").join("aberp.duckdb");
    let verdict = ensure_db_path_isolated(&spelled);
    let _ = std::fs::remove_dir_all(&base);

    assert!(
        verdict.is_err(),
        "a path spelled with the foreign root must stay refused even when \
         that root is a symlink; got Ok for {}",
        spelled.display()
    );
}
