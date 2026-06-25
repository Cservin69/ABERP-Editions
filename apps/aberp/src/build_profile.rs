//! S165 / prod-prep PR #1 — the compile-time build-profile switch.
//!
//! The `production` Cargo feature is the single hülye-biztos lever that
//! flips ABERP from the NAV test environment to the real one. It is
//! COMPILE-TIME on purpose: there is no env-var override, so a binary
//! cannot be talked into prod NAV at runtime — the compiler bakes the
//! choice in at build time (`cargo build --features production`). The
//! go-live ceremony depends on this: a binary built without the feature
//! physically cannot submit to the real NAV endpoint.
//!
//! Everything that branches on prod-vs-test reads [`IS_PRODUCTION_BUILD`]
//! (or one of the helpers below) so there is exactly one source of truth:
//!
//!   - [`nav_endpoint`] — which [`NavEndpoint`] the serve/daemon paths
//!     target. The URL strings themselves live in `nav-transport`'s
//!     `NavEndpoint::base_url`; this module only selects the variant, so
//!     the literals stay single-sourced (CLAUDE.md rule 8).
//!   - [`assert_endpoint_allowed`] — defence-in-depth gate: a dev build
//!     refuses a `Production` endpoint no matter how it was handed one.
//!   - [`INVOICE_NUMBER_TEST_PREFIX`] — the `TEST-` render prefix that
//!     dev/test builds prepend to every emitted invoice number.

use aberp_nav_transport::NavEndpoint;

/// `true` iff this binary was compiled with `--features production`.
#[cfg(feature = "production")]
pub const IS_PRODUCTION_BUILD: bool = true;
/// `false` for every non-production build (the default).
#[cfg(not(feature = "production"))]
pub const IS_PRODUCTION_BUILD: bool = false;

/// Render prefix prepended to every emitted invoice number on dev/test
/// builds, empty on production builds. `TEST-` is NAV-`invoiceNumber`
/// charset-legal (`[0-9A-Za-z\-/]`, hyphen — NOT underscore, which the
/// validator rejects) so a prefixed number passes XSD at submit time.
/// Purely render-side: the DB counter is unchanged, so switching builds
/// never resets or skips a sequence number.
pub const INVOICE_NUMBER_TEST_PREFIX: &str = if IS_PRODUCTION_BUILD { "" } else { "TEST-" };

/// S166 / prod-prep PR #2 — the tenant identity a build is allowed to
/// run as, used by the boot sanity check (`serve::sanity_check_environment`).
///
/// Returns `Some((tenant_name, expected_tax_number))` on a PRODUCTION
/// build — the documented prod entity (Áben Consulting Kft.). A prod
/// binary that finds a seller.toml with a different `tax_number` refuses
/// to start: hülye-biztos protection so a prod build can only ever run
/// against the one documented prod identity.
///
/// Returns `None` on a dev/test build — dev tenants can have arbitrary
/// identity, so the sanity check enforces nothing there. The value is
/// NOT hardcoded at the check site; this helper is the single source of
/// truth (CLAUDE.md rule 8).
pub fn expected_tenant_identity() -> Option<(&'static str, &'static str)> {
    if IS_PRODUCTION_BUILD {
        Some(("prod", "24904362-2-41"))
    } else {
        None
    }
}

/// The NAV endpoint this build targets. Production builds hit the real
/// `api.onlineszamla.nav.gov.hu`; every other build hits the
/// `api-test.onlineszamla.nav.gov.hu` conformance host.
pub fn nav_endpoint() -> NavEndpoint {
    if IS_PRODUCTION_BUILD {
        NavEndpoint::Production
    } else {
        NavEndpoint::Test
    }
}

/// Audit-ledger label for the endpoint this build targets — the string
/// stamped into the NAV-submit audit entries. Mirrors the
/// `NavEnv::{Test,Production}` `"test"`/`"production"` labels the CLI
/// paths already use.
pub fn nav_endpoint_audit_label() -> &'static str {
    if IS_PRODUCTION_BUILD {
        "production"
    } else {
        "test"
    }
}

/// Base URL of the NAV invoiceService v3 endpoint for this build, with
/// trailing slash. Thin delegate to [`NavEndpoint::base_url`] so the URL
/// literals stay owned by `nav-transport`.
pub fn nav_endpoint_base_url() -> &'static str {
    nav_endpoint().base_url()
}

/// Defence-in-depth prod-endpoint gate (deliverable #2).
///
/// A production build has the gate LIFTED — prod NAV calls succeed. A
/// non-production build REFUSES any `Production` endpoint, no matter how
/// it was injected: if a dev binary somehow gets handed the prod
/// endpoint, it still loud-fails rather than touching real NAV. Test
/// endpoints are always allowed.
pub fn assert_endpoint_allowed(endpoint: NavEndpoint) -> anyhow::Result<()> {
    if !IS_PRODUCTION_BUILD && endpoint == NavEndpoint::Production {
        anyhow::bail!(
            "this is a DEV build but a PRODUCTION NAV endpoint ({}) was selected — refusing to \
             submit to real NAV. Rebuild with `--features production` to target prod.",
            endpoint.hostname()
        );
    }
    Ok(())
}

// ── ADR-0093 — compile-time Edition identity + edition-locked data root ──
//
// The saw-off (ADR-0093 §"Build-locked binding") binds each edition to its
// OWN on-disk data root at COMPILE time, exactly as the `production` feature
// binds the NAV endpoint above: there is no env-var or launcher-string
// override (FOUNDATION §5 — the path is *derived*, never user-supplied). A
// build therefore physically cannot resolve another edition's root, and —
// critically — the sawed-off editions tree can never resolve the frozen prod
// line's `~/.aberp/` root at all: it uses sibling `~/.aberp-<edition>/` roots
// that are provably disjoint from `~/.aberp/prod/`.

/// The product-line edition a binary is compiled as. The frozen Prod line
/// lives in a *different* repository (ADR-0093 §6); this sawed-off tree only
/// ever compiles as [`Edition::Defense`] (`--features production`) or
/// [`Edition::Portable`] (the default). `Prod` is named for totality of the
/// forbidden-root logic, but the compile-time assertion below proves this
/// tree never *binds* it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edition {
    /// Frozen unified prod line — NOT built from this repo. Its data root
    /// `~/.aberp/` (tenant `~/.aberp/prod/`) is the one the editions must
    /// never resolve or open.
    #[allow(dead_code)] // intentionally never constructed in the editions tree
    Prod,
    /// Defense / aerospace line — the `--features production` build.
    Defense,
    /// Portable / NAV-off line — the default (non-production) build.
    Portable,
}

/// The edition THIS binary was compiled as. Derived from the same
/// compile-time `production` feature that drives [`IS_PRODUCTION_BUILD`]:
/// `--features production` ⇒ [`Edition::Defense`], otherwise
/// [`Edition::Portable`]. There is deliberately no `Prod` arm.
pub const EDITION: Edition = if IS_PRODUCTION_BUILD {
    Edition::Defense
} else {
    Edition::Portable
};

/// Compile-time proof that the sawed-off editions tree never binds the
/// frozen prod edition. If a future edit ever wired `EDITION` to
/// `Edition::Prod`, the build would FAIL here rather than silently let a
/// binary resolve `~/.aberp/` (ADR-0093: "prod is untouchable by
/// construction").
const _: () = assert!(!matches!(EDITION, Edition::Prod));

/// `$HOME`-relative data-root dir name for the frozen prod line. The
/// editions must NEVER resolve or open anything under this.
pub const PROD_DATA_DIRNAME: &str = ".aberp";
/// `$HOME`-relative data-root dir name for the Defense edition.
pub const DEFENSE_DATA_DIRNAME: &str = ".aberp-defense";
/// `$HOME`-relative data-root dir name for the Portable edition.
pub const PORTABLE_DATA_DIRNAME: &str = ".aberp-portable";

/// The `$HOME`-relative data-root dir name for a given edition — the single
/// source of truth every per-tenant path resolver joins under `$HOME`
/// (`tenant_registry::aberp_root` and friends). `const fn` so it is usable
/// in const context and trivially inlined.
pub const fn data_dirname(edition: Edition) -> &'static str {
    match edition {
        Edition::Prod => PROD_DATA_DIRNAME,
        Edition::Defense => DEFENSE_DATA_DIRNAME,
        Edition::Portable => PORTABLE_DATA_DIRNAME,
    }
}

/// This binary's edition-locked data-root dir name. Compile-time constant —
/// `.aberp-defense` or `.aberp-portable`, NEVER `.aberp`.
pub const EDITION_DATA_DIRNAME: &str = data_dirname(EDITION);

/// Ergonomic accessor for [`EDITION_DATA_DIRNAME`] — the segment every
/// per-tenant path resolver joins under `$HOME` (ADR-0093 §5).
pub const fn edition_data_dirname() -> &'static str {
    EDITION_DATA_DIRNAME
}

/// Human-facing edition label for guard / diagnostic messages.
pub const fn edition_label() -> &'static str {
    match EDITION {
        Edition::Prod => "Prod",
        Edition::Defense => "Defense",
        Edition::Portable => "Portable",
    }
}

/// Lowercase store segment for this edition (`"defense"` / `"portable"`) —
/// the suffix the edition-scoped snapshot store joins under
/// `~/Documents/ABERP-snapshots-<segment>/` (ADR-0093 §5: snapshot stores
/// are likewise edition-scoped). Derived from the same compile-time
/// [`EDITION`], never an env/launcher string (FOUNDATION §5), so an
/// editions build can never silently share prod's `ABERP-snapshots/` store.
pub const fn edition_store_segment() -> &'static str {
    match EDITION {
        Edition::Prod => "prod",
        Edition::Defense => "defense",
        Edition::Portable => "portable",
    }
}

/// The data-root dir names this build must REFUSE to resolve or open —
/// every edition's root except its own. For the editions tree this ALWAYS
/// includes the frozen prod root (`.aberp`) plus the sibling edition's root,
/// so a build can never cross into prod's or the other edition's database
/// (ADR-0093 — "physically refuses").
pub const fn foreign_data_dirnames() -> [&'static str; 2] {
    match EDITION {
        Edition::Portable => [PROD_DATA_DIRNAME, DEFENSE_DATA_DIRNAME],
        Edition::Defense => [PROD_DATA_DIRNAME, PORTABLE_DATA_DIRNAME],
        // Unreachable in the editions tree (see the compile-time assert
        // above); present for match totality, no wildcard.
        Edition::Prod => [DEFENSE_DATA_DIRNAME, PORTABLE_DATA_DIRNAME],
    }
}

// ── S2 / ADR-0093 — compile-time storefront-reach edition gate ───────────
//
// The quote-intake / pricing pipeline POLLS the customer-facing storefront
// (`abenerp.com`) for uploaded CAD and PUSHES the catalogue / priced PDFs /
// status back to it. That REACH pulls real customer data over the network,
// so — exactly like the prod-NAV endpoint above and the edition DB root
// below — it is bound to the edition at COMPILE time, not merely gated by
// config. Only the Defense edition may reach the storefront; a Portable
// (demo) build has the capability compiled OUT and physically cannot poll
// or push, no matter what `[quote_intake]` config / `ABERP_QUOTE_INTAKE_*`
// env it is handed (FOUNDATION §5 — capability derived from the build,
// never user-supplied).
//
// SCOPE: this gate covers ONLY the network reach to the storefront. The
// pure local quote engine (`aberp-quote-engine`) and the operator/manual
// quoting paths stay fully available in BOTH editions — a Portable demo can
// still price a part the operator types in; it just cannot pull a
// customer's CAD off abenerp.com or push a catalogue to it.

/// Whether a given [`Edition`] may reach the customer storefront. Pure,
/// total over `Edition`, and the SINGLE source of truth for the reach
/// decision: ONLY [`Edition::Defense`]. Parameterised so BOTH edition arms
/// are provable in one compile (the binary only ever pins its own
/// [`EDITION`], but a test can check every arm). `const fn` for const-context
/// use + trivial inlining.
pub const fn storefront_polling_allowed_for(edition: Edition) -> bool {
    matches!(edition, Edition::Defense)
}

/// `true` iff THIS build's edition may reach the customer storefront
/// (`abenerp.com`): poll for CAD uploads, push the catalogue, write back
/// priced PDFs / status. Compile-time constant derived from [`EDITION`] via
/// [`storefront_polling_allowed_for`]: Defense ⇒ `true`, Portable (and the
/// never-built `Prod` arm) ⇒ `false`.
pub const fn storefront_polling_allowed() -> bool {
    storefront_polling_allowed_for(EDITION)
}

/// Defence-in-depth storefront-reach gate — the runtime backstop behind the
/// compile-time [`storefront_polling_allowed`] binding, mirroring
/// [`assert_endpoint_allowed`] (prod NAV) and
/// [`crate::tenant_registry::ensure_db_path_isolated`] (edition DB root). A
/// Defense build has the gate LIFTED — storefront reach proceeds. Any other
/// build (Portable) REFUSES, no matter how the storefront config arrived
/// (`[quote_intake]` in seller.toml, the `ABERP_QUOTE_INTAKE_ENABLED` /
/// `ABERP_QUOTE_INTAKE_URL` env, a hand-edited launcher, the SPA): it
/// loud-fails rather than poll `abenerp.com` or pull a customer's CAD.
/// `intent` names the refused surface so the boot/handler message is
/// specific. Pure decision function ⇒ unit-testable for both edition arms
/// without standing up serve.rs.
pub fn assert_storefront_reach_allowed(intent: &str) -> anyhow::Result<()> {
    if !storefront_polling_allowed() {
        anyhow::bail!(
            "ADR-0093 edition isolation: the {} edition refuses storefront reach ({}) — \
             polling abenerp.com for customer CAD / pushing the catalogue is a Defense-only \
             capability, compiled OUT of this build. The local quote engine + manual quoting \
             stay available; only the abenerp.com reach is Defense-only. Rebuild as Defense \
             (`cargo build --features production`) to enable storefront reach.",
            edition_label(),
            intent,
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // The feature is compile-time, so each build flavour can only pin
    // its own arm. `cargo test --workspace` (feature off) runs the dev
    // arm; `cargo test --features production` runs the prod arm.

    #[cfg(not(feature = "production"))]
    #[test]
    #[allow(clippy::assertions_on_constants)] // pinning the compile-time gate is the test's purpose.
    fn dev_build_targets_test_endpoint_and_prefixes() {
        assert!(!IS_PRODUCTION_BUILD);
        assert_eq!(nav_endpoint(), NavEndpoint::Test);
        assert_eq!(
            nav_endpoint_base_url(),
            "https://api-test.onlineszamla.nav.gov.hu/invoiceService/v3/"
        );
        assert_eq!(nav_endpoint_audit_label(), "test");
        assert_eq!(INVOICE_NUMBER_TEST_PREFIX, "TEST-");
    }

    #[cfg(feature = "production")]
    #[test]
    #[allow(clippy::assertions_on_constants)] // pinning the compile-time gate is the test's purpose.
    fn production_build_targets_prod_endpoint_and_no_prefix() {
        assert!(IS_PRODUCTION_BUILD);
        assert_eq!(nav_endpoint(), NavEndpoint::Production);
        assert_eq!(
            nav_endpoint_base_url(),
            "https://api.onlineszamla.nav.gov.hu/invoiceService/v3/"
        );
        assert_eq!(nav_endpoint_audit_label(), "production");
        assert_eq!(INVOICE_NUMBER_TEST_PREFIX, "");
    }

    #[cfg(not(feature = "production"))]
    #[test]
    fn dev_build_refuses_production_endpoint_but_allows_test() {
        // The gate STAYS on a dev build: Production is refused…
        assert!(assert_endpoint_allowed(NavEndpoint::Production).is_err());
        // …while Test is always fine.
        assert!(assert_endpoint_allowed(NavEndpoint::Test).is_ok());
    }

    #[cfg(feature = "production")]
    #[test]
    fn production_build_allows_both_endpoints() {
        // The gate is LIFTED on a production build.
        assert!(assert_endpoint_allowed(NavEndpoint::Production).is_ok());
        assert!(assert_endpoint_allowed(NavEndpoint::Test).is_ok());
    }

    // ── ADR-0093 — edition binding pins ──────────────────────────────
    // The edition is compile-time, so each build flavour pins its own
    // arm: the default (feature off) build is Portable; `--features
    // production` is Defense. Both assert they NEVER bind prod's root.

    #[test]
    fn editions_tree_never_binds_prod_edition() {
        // Total over Edition, and proves the sawed-off invariant: the
        // running build is never the frozen prod edition.
        assert_ne!(EDITION, Edition::Prod);
        // Dirname mapping is the single source of truth.
        assert_eq!(data_dirname(Edition::Prod), ".aberp");
        assert_eq!(data_dirname(Edition::Defense), ".aberp-defense");
        assert_eq!(data_dirname(Edition::Portable), ".aberp-portable");
        // This build's own root is never prod's, and prod's root is
        // ALWAYS in the forbidden set.
        assert_ne!(edition_data_dirname(), PROD_DATA_DIRNAME);
        assert!(foreign_data_dirnames().contains(&PROD_DATA_DIRNAME));
        // The own root is never listed as foreign.
        assert!(!foreign_data_dirnames().contains(&edition_data_dirname()));
    }

    #[cfg(not(feature = "production"))]
    #[test]
    fn portable_build_binds_portable_root() {
        assert_eq!(EDITION, Edition::Portable);
        assert_eq!(edition_data_dirname(), ".aberp-portable");
        assert_eq!(edition_label(), "Portable");
        // Both prod AND the sibling Defense root are foreign (refused).
        assert!(foreign_data_dirnames().contains(&PROD_DATA_DIRNAME));
        assert!(foreign_data_dirnames().contains(&DEFENSE_DATA_DIRNAME));
        assert!(!foreign_data_dirnames().contains(&PORTABLE_DATA_DIRNAME));
    }

    #[cfg(feature = "production")]
    #[test]
    fn defense_build_binds_defense_root() {
        assert_eq!(EDITION, Edition::Defense);
        assert_eq!(edition_data_dirname(), ".aberp-defense");
        assert_eq!(edition_label(), "Defense");
        // Both prod AND the sibling Portable root are foreign (refused).
        assert!(foreign_data_dirnames().contains(&PROD_DATA_DIRNAME));
        assert!(foreign_data_dirnames().contains(&PORTABLE_DATA_DIRNAME));
        assert!(!foreign_data_dirnames().contains(&DEFENSE_DATA_DIRNAME));
    }

    // ── S2 / ADR-0093 — storefront-reach edition gate pins ───────────
    // The reach predicate is compile-time, so each build flavour pins
    // its own arm: Portable refuses storefront reach, Defense allows it.
    // The PARAMETERISED decision fn lets one compile prove BOTH arms.

    #[test]
    fn storefront_reach_predicate_total_over_edition_both_arms() {
        // Both arms, in a single compile: ONLY Defense may reach the
        // storefront. Prod is never built here but must never be allowed.
        assert!(storefront_polling_allowed_for(Edition::Defense));
        assert!(!storefront_polling_allowed_for(Edition::Portable));
        assert!(!storefront_polling_allowed_for(Edition::Prod));
        // This build's predicate agrees with its own EDITION.
        assert_eq!(storefront_polling_allowed(), EDITION == Edition::Defense);
    }

    #[cfg(not(feature = "production"))]
    #[test]
    fn portable_build_refuses_storefront_reach() {
        // Portable: the capability is compiled OUT.
        assert_eq!(EDITION, Edition::Portable);
        assert!(!storefront_polling_allowed());
        // The runtime backstop loud-fails even when config is "present"
        // — the storefront base_url / token never reaches the network.
        assert!(assert_storefront_reach_allowed("quote-intake poll daemon").is_err());
    }

    #[cfg(feature = "production")]
    #[test]
    fn defense_build_allows_storefront_reach() {
        // Defense: the gate is LIFTED.
        assert_eq!(EDITION, Edition::Defense);
        assert!(storefront_polling_allowed());
        assert!(assert_storefront_reach_allowed("quote-intake poll daemon").is_ok());
    }
}
