//! ADR-0093 — the edition × tenant data-root MATRIX pin.
//!
//! `edition_db_isolation.rs` proves each build refuses the *foreign* roots it
//! knows about. This file proves the stronger, whole-matrix property Ervin
//! actually asked for, and the one a future edit is most likely to break:
//!
//!   for EVERY (edition, tenant) pair the binary can resolve, the resulting
//!   data root is pairwise DISTINCT from every other pair's, and no root is
//!   NESTED inside another.
//!
//! Distinctness alone is not enough. `~/.aberp/demo/` and `~/.aberp/` are
//! distinct paths, yet the first lives *inside* the second — so prod's
//! registry scan, fresh-install probe, snapshot sweep or WAL fold would walk
//! straight over the other line's files. Nesting is the failure mode that
//! reads as "isolated" on a casual look. (It is exactly the shape
//! `ABERP.git`'s `run/run_portable.sh` still has: `PORTABLE_HOME` =
//! `${HOME}/.aberp/${TENANT}`, inside prod's root rather than a sibling of
//! it. This tree's roots are siblings under `$HOME`; that is what the
//! assertion below locks in.)
//!
//! Nesting is checked COMPONENT-WISE, never by string prefix: `.aberp` is a
//! string prefix of `.aberp-portable` while being a completely separate
//! directory. A `starts_with` on strings would false-positive here and get
//! "fixed" by weakening the test.
//!
//! The whole matrix is checkable in ONE compile because
//! `build_profile::data_dirname` is a pure `const fn` total over `Edition` —
//! the binary only ever *binds* its own [`EDITION`], but the mapping every
//! edition would resolve is inspectable from any build.
//!
//! The last test is a NEGATIVE PROBE: it feeds the same collision detector a
//! deliberately collapsed mapping and asserts it goes RED. A safety test that
//! cannot fail is decoration (CLAUDE.md rule 9).

use std::path::{Path, PathBuf};

use aberp::build_profile::{data_dirname, Edition, EDITION};

/// Every edition whose data root this codebase can name. `Prod` is included
/// deliberately: this tree never *binds* it, but prod's `~/.aberp/` is the
/// root the editions must stay disjoint from, so it belongs in the matrix.
const EDITIONS: [Edition; 3] = [Edition::Prod, Edition::Defense, Edition::Portable];

/// Tenant slugs the matrix is proved over. Includes each line's launcher
/// default (`prod` / `defense` / `demo`), the dev-loop default (`test`), the
/// CLI default (`default`), and a customer-shaped slug (`acme`) — plus
/// `prod` under EVERY edition, which is the adversarial case: the reserved
/// slug must not let a non-prod edition land on prod's tenant directory.
const TENANTS: [&str; 6] = ["prod", "defense", "demo", "test", "default", "acme"];

/// A fixed `$HOME` — the matrix is about path *structure*, not about the
/// machine the test runs on, and hard-coding it keeps the test hermetic and
/// nowhere near a real `~/.aberp**` tree.
const HOME: &str = "/Users/op";

/// `$HOME/<edition-dirname>/<tenant>` — the same derivation
/// `tenant_registry::tenant_db_path` performs (`aberp_root()` = `$HOME` joined
/// with the compile-time `edition_data_dirname()`, then the tenant slug),
/// evaluated for an arbitrary edition rather than only this build's.
fn tenant_root(edition: Edition, tenant: &str) -> PathBuf {
    Path::new(HOME).join(data_dirname(edition)).join(tenant)
}

fn label(edition: Edition, tenant: &str) -> String {
    format!("{edition:?}/{tenant}")
}

/// True iff `a` is `b`, or `a` lives inside `b`. Component-wise, so
/// `.aberp-portable` is NOT treated as living inside `.aberp`.
fn is_at_or_under(a: &Path, b: &Path) -> bool {
    a.starts_with(b)
}

/// The single collision detector both the real assertion and the negative
/// probe run. Returns the first offending pair as
/// `(label_a, path_a, label_b, path_b, kind)`, or `None` if the set is
/// cleanly disjoint. Pure and order-independent: every unordered pair is
/// tested in both directions, so a nesting is found whichever side it is on.
fn first_collision(
    roots: &[(String, PathBuf)],
) -> Option<(String, PathBuf, String, PathBuf, &'static str)> {
    for (i, (la, pa)) in roots.iter().enumerate() {
        for (lb, pb) in roots.iter().skip(i + 1) {
            if pa == pb {
                return Some((la.clone(), pa.clone(), lb.clone(), pb.clone(), "IDENTICAL"));
            }
            if is_at_or_under(pa, pb) {
                return Some((la.clone(), pa.clone(), lb.clone(), pb.clone(), "NESTED"));
            }
            if is_at_or_under(pb, pa) {
                return Some((lb.clone(), pb.clone(), la.clone(), pa.clone(), "NESTED"));
            }
        }
    }
    None
}

/// The full edition × tenant matrix, as the binary resolves it.
fn matrix() -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    for ed in EDITIONS {
        for t in TENANTS {
            out.push((label(ed, t), tenant_root(ed, t)));
        }
    }
    out
}

// ── the property ─────────────────────────────────────────────────────────

#[test]
fn every_edition_tenant_pair_resolves_to_a_distinct_unnested_root() {
    let roots = matrix();
    assert_eq!(
        roots.len(),
        EDITIONS.len() * TENANTS.len(),
        "matrix must be fully enumerated"
    );

    if let Some((la, pa, lb, pb, kind)) = first_collision(&roots) {
        panic!(
            "ADR-0093 edition-root isolation BROKEN — {kind}:\n  \
             {la} -> {}\n  {lb} -> {}\n\
             Every (edition, tenant) pair must resolve to its own root, with no root \
             inside another. A change to build_profile::data_dirname or to the \
             tenant path derivation collapsed two of them.",
            pa.display(),
            pb.display()
        );
    }
}

#[test]
fn edition_roots_are_siblings_under_home_not_nested() {
    // The per-EDITION roots (tenant-independent) are the ones a registry
    // scan, fresh-install probe or snapshot sweep operates on. They must be
    // siblings under `$HOME` — never one inside another.
    let roots: Vec<(String, PathBuf)> = EDITIONS
        .iter()
        .map(|&ed| (format!("{ed:?}"), Path::new(HOME).join(data_dirname(ed))))
        .collect();

    assert!(
        first_collision(&roots).is_none(),
        "edition roots must be pairwise distinct and non-nested: {roots:?}"
    );

    // …and their only shared ancestor is `$HOME` itself, which no ABERP code
    // path treats as a data root.
    for (_, p) in &roots {
        assert_eq!(
            p.parent(),
            Some(Path::new(HOME)),
            "every edition root must sit DIRECTLY under $HOME (a sibling), not \
             nested inside another line's root: {}",
            p.display()
        );
    }
}

#[test]
fn reserved_prod_slug_never_lands_on_prods_tenant_directory() {
    // The adversarial pair: tenant `prod` under a non-prod edition must not
    // resolve to prod's `~/.aberp/prod/`. This is the case a launcher export
    // or `ABERP_TENANT=prod` would reach for.
    let prods_own = tenant_root(Edition::Prod, "prod");
    for ed in [Edition::Defense, Edition::Portable] {
        assert_ne!(
            tenant_root(ed, "prod"),
            prods_own,
            "{ed:?} with tenant=prod must not resolve prod's own tenant root"
        );
        assert!(
            !is_at_or_under(
                &tenant_root(ed, "prod"),
                Path::new(HOME).join(".aberp").as_path()
            ),
            "{ed:?}/prod must not land under prod's ~/.aberp/ root"
        );
    }
}

#[test]
fn this_build_binds_a_root_that_is_disjoint_from_every_other_editions() {
    // Ties the matrix back to the running binary: whichever edition this is,
    // its own root is disjoint from both others'.
    let own = Path::new(HOME).join(data_dirname(EDITION));
    for ed in EDITIONS {
        if ed == EDITION {
            continue;
        }
        let other = Path::new(HOME).join(data_dirname(ed));
        assert_ne!(own, other);
        assert!(!is_at_or_under(&own, &other));
        assert!(!is_at_or_under(&other, &own));
    }
}

// ── negative probe — proof the detector can go red ───────────────────────

#[test]
fn detector_catches_an_identical_root_collision() {
    // Collapse Defense onto prod's dirname — the exact regression the pin
    // exists to catch (a `data_dirname` edit returning `.aberp` for Defense).
    let bad = vec![
        (
            "Prod/prod".to_string(),
            Path::new(HOME).join(".aberp").join("prod"),
        ),
        (
            "Defense/prod".to_string(),
            Path::new(HOME).join(".aberp").join("prod"),
        ),
    ];
    let hit = first_collision(&bad).expect("detector MUST flag two identical roots");
    assert_eq!(hit.4, "IDENTICAL");
}

#[test]
fn detector_catches_a_nested_root_collision() {
    // The `ABERP.git` run_portable.sh shape: a line's tenant home placed
    // INSIDE prod's root instead of beside it. Distinct paths, still fatal.
    let bad = vec![
        ("Prod-root".to_string(), Path::new(HOME).join(".aberp")),
        (
            "Portable/demo".to_string(),
            Path::new(HOME).join(".aberp").join("demo"),
        ),
    ];
    let hit = first_collision(&bad).expect("detector MUST flag a nested root");
    assert_eq!(hit.4, "NESTED");
    // Reversed input order finds it too — the detector is not order-dependent.
    let mut rev = bad;
    rev.reverse();
    assert_eq!(
        first_collision(&rev)
            .expect("detector MUST flag a nested root regardless of order")
            .4,
        "NESTED"
    );
}

#[test]
fn detector_does_not_false_positive_on_a_shared_string_prefix() {
    // `.aberp` is a string prefix of `.aberp-portable`. If this ever fails,
    // someone replaced the component-wise check with a `starts_with` on
    // strings — which would make the real matrix test unfixably red and
    // invite weakening it.
    let ok = vec![
        ("Prod".to_string(), Path::new(HOME).join(".aberp")),
        (
            "Portable".to_string(),
            Path::new(HOME).join(".aberp-portable"),
        ),
        (
            "Defense".to_string(),
            Path::new(HOME).join(".aberp-defense"),
        ),
    ];
    assert!(
        first_collision(&ok).is_none(),
        "sibling roots sharing a string prefix are NOT a collision"
    );
}
