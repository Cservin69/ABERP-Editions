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

// ── ADR-0093 §5 DECISION TABLE — every spelling, one execution ───────
//
// The single test that carries the §5 claim. Each row is a real path in a
// real temp tree, handed to the REAL `ensure_db_path_isolated` — no
// replica of the rule, which is exactly how the case bypass survived
// review: the shipped guard compared dirnames BYTE-EXACTLY, and macOS
// APFS opens `~/.ABERP/prod/aberp.duckdb` and `~/.aberp/prod/aberp.duckdb`
// as the same file. A Defense build with `ABERP_DB` set to the first
// spelling opened the live prod DuckDB read-write (executed 2026-07-21).
//
// The table prints in full BEFORE it asserts, so a mutation run (drop the
// canonicalization, or restore the byte-exact compare) shows exactly
// which rows go red and which stay green. A guard that refuses
// everything is not a fix, so the ALLOW rows — every legitimate Defense
// and Portable launch path — carry the same weight as the REFUSE rows.
#[cfg(unix)]
#[test]
fn adr0093_guard_decision_table() {
    use std::fs;
    use std::path::PathBuf;

    use build_profile::{foreign_data_dirnames, PROD_DATA_DIRNAME};

    let base = std::env::temp_dir()
        .join("aberp-iso-decision-table")
        .join(std::process::id().to_string());
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&base).expect("create table base");

    // The simulated FOREIGN prod root, with a seeded victim file standing
    // in for the operator's live `~/.aberp/prod/aberp.duckdb`. The real
    // one is never read, written, or resolved: the guard consults no
    // `$HOME`-relative path of its own, so a temp tree exercises byte-
    // identical code.
    let foreign_root = base.join(PROD_DATA_DIRNAME);
    fs::create_dir_all(foreign_root.join("prod")).expect("create foreign root");
    let victim = foreign_root.join("prod").join("aberp.duckdb");
    fs::write(&victim, b"ERVIN INVOICES").expect("seed victim file");

    // The sibling edition's root — foreign too, and deliberately NOT
    // created, so its rows exercise the not-yet-existing root that
    // canonicalization cannot correct.
    let sibling = *foreign_data_dirnames()
        .iter()
        .find(|d| **d != PROD_DATA_DIRNAME)
        .expect("an editions build always has a foreign sibling root");

    // `sneaky -> <base>/.aberp`: an innocuous name over the foreign root.
    let sneaky = base.join("sneaky");
    std::os::unix::fs::symlink(&foreign_root, &sneaky).expect("sneaky symlink");

    // The foreign root spelled correctly but backed by a symlink — the
    // direction that would break if canonicalization REPLACED the raw
    // walk instead of joining it.
    let linked = base.join("linked");
    fs::create_dir_all(linked.join("elsewhere").join("prod")).expect("create backing dir");
    std::os::unix::fs::symlink(linked.join("elsewhere"), linked.join(PROD_DATA_DIRNAME))
        .expect("linked-root symlink");

    // A hardlink to the victim FILE, parked outside the foreign root.
    let hardlink = base.join("decoy.duckdb");
    fs::hard_link(&victim, &hardlink).expect("hardlink");

    // An ordinary existing scratch dir, for the resolvable-`..` row.
    fs::create_dir_all(base.join("scratch")).expect("create scratch");

    // NFC/NFD: one name, two encodings. APFS compares normalization-
    // insensitively, ext4 does not — probe the filesystem instead of
    // assuming, and report the rows as n/a rather than faking them.
    let nfc = format!("alias-{}", '\u{FC}'); // "alias-ü", precomposed
    let nfd = format!("alias-u{}", '\u{308}'); // "alias-ü", decomposed
    let mirror_nfd = format!("mirror-u{}", '\u{308}');
    let mirror_nfc = format!("mirror-{}", '\u{FC}');
    std::os::unix::fs::symlink(&foreign_root, base.join(&nfc)).expect("NFC symlink");
    std::os::unix::fs::symlink(&foreign_root, base.join(&mirror_nfd)).expect("NFD symlink");
    let nfc_nfd_equivalent = fs::metadata(base.join(&nfd)).is_ok();

    let own_root = aberp_root().expect("HOME/USERPROFILE is set in the test env");
    let db = |p: PathBuf| p.join("aberp.duckdb");

    const REFUSE: bool = true;
    const ALLOW: bool = false;

    let mut rows: Vec<(&str, PathBuf, bool, &str)> = vec![
        // ── the baseline the case rows are measured against ──────────
        (
            "baseline .aberp, spelled",
            db(foreign_root.join("prod")),
            REFUSE,
            "the rule's original case — must never regress",
        ),
        // ── THE BREAK: one-character case change on a case-insensitive fs
        (
            "case .ABERP, root exists",
            db(base.join(".ABERP").join("prod")),
            REFUSE,
            "APFS opens the same directory as .aberp",
        ),
        (
            "case .Aberp, root exists",
            db(base.join(".Aberp").join("prod")),
            REFUSE,
            "same, mixed case",
        ),
        (
            "case .ABERP, root absent",
            db(base.join("absent").join(".ABERP").join("prod")),
            REFUSE,
            "nothing on disk to canonicalize — the dirname compare carries it alone",
        ),
        (
            "sibling root, exact",
            db(base.join(sibling).join("acme")),
            REFUSE,
            "the other edition's root is foreign too",
        ),
        // ── symlinks, both directions ────────────────────────────────
        (
            "symlink into the foreign root",
            db(sneaky.join("prod")),
            REFUSE,
            "S2 escape: no .aberp component when spelled",
        ),
        (
            "foreign root IS a symlink, spelled",
            db(linked.join(PROD_DATA_DIRNAME).join("prod")),
            REFUSE,
            "resolving strips the component — why both walks stay",
        ),
        (
            "resolvable .. through the symlink",
            db(sneaky.join("prod").join("..").join("prod")),
            REFUSE,
            "every component exists, so canonicalization resolves it",
        ),
        // ── F3: a `..` that canonicalization could not resolve ───────
        (
            ".. behind a missing component",
            db(base.join("missing").join("..").join("sneaky").join("prod")),
            REFUSE,
            "unresolved — names no fixed directory, so refuse",
        ),
        // ── spelling noise that must not change the verdict ──────────
        (
            "trailing separator",
            PathBuf::from(format!("{}/", db(foreign_root.join("prod")).display())),
            REFUSE,
            "trailing / must not hide the component",
        ),
        (
            "doubled separators //",
            PathBuf::from(format!("{}//prod//aberp.duckdb", foreign_root.display())),
            REFUSE,
            "empty components must not hide it either",
        ),
        (
            "/./ segments",
            PathBuf::from(format!("{}/./prod/./aberp.duckdb", foreign_root.display())),
            REFUSE,
            "CurDir components are noise",
        ),
        (
            "the bare foreign root",
            foreign_root.clone(),
            REFUSE,
            "the root itself belongs to no edition here",
        ),
        (
            "foreign tenants.toml",
            foreign_root.join("tenants.toml"),
            REFUSE,
            "the registry inside a foreign root is foreign",
        ),
        (
            "absolute prod path, no fs backing",
            PathBuf::from("/Users/op/.aberp/prod/aberp.duckdb"),
            REFUSE,
            "the classic ABERP_DB misconfiguration",
        ),
        // ── the residual this guard cannot close ─────────────────────
        (
            "hardlink to the victim file",
            hardlink.clone(),
            ALLOW,
            "KNOWN RESIDUAL — a hardlink is a second name for the inode, \
             not a link the fs will resolve; see ADR-0093 §5",
        ),
        // ── NEGATIVE CONTROLS: every legitimate launch path ──────────
        (
            "own edition root, tenant acme",
            tenant_db_path("acme").expect("HOME set"),
            ALLOW,
            "run_defense.sh / run_portable.sh ABERP_DB",
        ),
        (
            "own edition root, tenant 'prod'",
            db(own_root.join("prod")),
            ALLOW,
            "a slug that normalises to prod is still THIS edition's tenant",
        ),
        (
            "own edition root, tenant 'PROD'",
            db(own_root.join("PROD")),
            ALLOW,
            "same, upper-cased — the guard keys on the ROOT, not the slug",
        ),
        (
            "run_desktop.sh default ./aberp.duckdb",
            PathBuf::from("./aberp.duckdb"),
            ALLOW,
            "the dev-loop default must keep working",
        ),
        (
            "an ordinary temp path",
            PathBuf::from("/tmp/whatever/aberp.duckdb"),
            ALLOW,
            "outside every edition root",
        ),
        (
            "a scratch copy under the test tree",
            db(base.join("scratch")),
            ALLOW,
            "test fixtures live here",
        ),
        (
            "resolvable .. outside every root",
            db(base.join("scratch").join("..").join("scratch")),
            ALLOW,
            "a `..` that DOES resolve must not be refused",
        ),
    ];

    if nfc_nfd_equivalent {
        rows.push((
            "unicode NFC on disk, spelled NFD",
            db(base.join(&nfd).join("prod")),
            REFUSE,
            "normalization-insensitive fs — same symlink",
        ));
        rows.push((
            "unicode NFD on disk, spelled NFC",
            db(base.join(&mirror_nfc).join("prod")),
            REFUSE,
            "the mirror direction",
        ));
    }

    // The sibling row spelled in upper case only means something when the
    // sibling name has letters to fold — it always does, but keep the
    // construction explicit rather than clever.
    rows.push((
        "sibling root, upper-case",
        db(base.join(sibling.to_uppercase()).join("acme")),
        REFUSE,
        "case bypass on the sibling edition's root",
    ));

    println!(
        "\nADR-0093 §5 — edition DB-path guard decision table ({} build, {} on disk)",
        build_profile::edition_label(),
        if nfc_nfd_equivalent {
            "normalization-insensitive fs"
        } else {
            "normalization-sensitive fs: NFC/NFD rows n/a"
        }
    );
    println!(
        "{:<6} {:<38} {:<7} {:<7} why",
        "", "case", "expect", "guard"
    );

    let mut failures = Vec::new();
    for (label, path, expect_refuse, why) in &rows {
        let verdict = ensure_db_path_isolated(path);
        let got_refuse = verdict.is_err();
        let ok = got_refuse == *expect_refuse;
        let word = |r: bool| if r { "REFUSE" } else { "ALLOW" };
        println!(
            "{:<6} {:<38} {:<7} {:<7} {}",
            if ok { "ok" } else { "*** FAIL" },
            label,
            word(*expect_refuse),
            word(got_refuse),
            why
        );
        if !ok {
            failures.push(format!(
                "{label}: expected {}, got {} for {}",
                word(*expect_refuse),
                word(got_refuse),
                path.display()
            ));
        }
    }

    // The victim file must still hold exactly what was seeded — the guard
    // is a decision, never a writer.
    let after = fs::read(&victim).expect("victim file still readable");
    let victim_intact = after == b"ERVIN INVOICES";

    let _ = fs::remove_dir_all(&base);

    assert!(
        victim_intact,
        "the guard must never touch the file it is asked about"
    );
    assert!(
        failures.is_empty(),
        "ADR-0093 decision table has {} mismatching row(s):\n  {}",
        failures.len(),
        failures.join("\n  ")
    );
}
