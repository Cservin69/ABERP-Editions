//! ADR-0112 Part A — pin the CAD extractor's edition reach, and pin the
//! STL rejection's failure classification.
//!
//! ## Why a test and not a `#[cfg]`
//!
//! `CadExtractor::extract` has exactly ONE production call site
//! (`quote_pricing_pipeline.rs`, in `advance_extract`), inside a module
//! whose entire storefront reach is already Defense-gated by ADR-0093's
//! `storefront_polling_allowed()`. A Portable build compiles the wrapper
//! crate and never calls it: the pricing-pipeline daemon is never
//! spawned, so no `quote_pricing_jobs` row is ever created, so nothing
//! ever advances to the extract step. A Portable operator has no way to
//! submit a CAD file at all.
//!
//! So the extractor is Defense-only **by reachability, not by
//! construction**, and ADR-0112 explicitly declines to add a redundant
//! `#[cfg]` gate: doing so would imply the existing gate is
//! insufficient. What reachability lacks is *durability* — it is a
//! property of the current call graph, and a future manual-upload path
//! in Portable could silently reopen it. These tests are that durability.
//!
//! Mirrors the posture of `edition_db_isolation.rs`: call the library
//! decision functions directly, assert both edition arms where the
//! `_for(edition)` parameterisation allows it, and assert this build's
//! own arm under `#[cfg]`.

use aberp::build_profile::{
    self, storefront_polling_allowed, storefront_polling_allowed_for, Edition, EDITION,
};
use aberp::quote_pricing_jobs::FailureKind;
use aberp::quote_pricing_pipeline::classify_failure;

// ── 1. Both arms of the gate, provable in ONE compile ────────────────────

#[test]
fn cad_reach_gate_is_defense_only_on_every_arm() {
    // The `_for(edition)` parameterisation exists precisely so a single
    // compile can check every arm, not merely the one this binary pins.
    assert!(
        storefront_polling_allowed_for(Edition::Defense),
        "Defense is the edition that reaches the storefront and therefore the CAD extractor"
    );
    assert!(
        !storefront_polling_allowed_for(Edition::Portable),
        "Portable must never reach the storefront — no storefront reach, no CAD to extract"
    );
    assert!(
        !storefront_polling_allowed_for(Edition::Prod),
        "the frozen prod line is never a build target from this tree"
    );
}

// ── 2. THIS build's arm ──────────────────────────────────────────────────

/// The Portable arm — what `cargo test --workspace` (feature OFF) runs.
///
/// This is the assertion ADR-0112 Part A actually asks for: in the frozen
/// Portable edition the CAD extractor is unreachable, so nothing in Part
/// A (STEP-only) or Part B (located holes) can move a Portable price.
#[cfg(not(feature = "production"))]
#[test]
#[allow(clippy::assertions_on_constants)] // pinning a compile-time gate IS the test
fn portable_build_cannot_reach_the_cad_extractor() {
    assert!(matches!(EDITION, Edition::Portable));
    assert!(
        !storefront_polling_allowed(),
        "Portable must have storefront reach compiled OUT"
    );

    // The reachability chain, stated as an assertion rather than a
    // comment. `storefront_polling_allowed() == false` is the single
    // predicate that gates every link:
    //
    //   no storefront poll  ->  no `quote_pricing_jobs` row is ever
    //   inserted            ->  `advance_extract` never runs
    //                       ->  `CadExtractor::extract` is never called.
    //
    // `serve.rs` spawns the pricing-pipeline daemon only inside
    // `if storefront_polling_allowed()`, and carries a `debug_assert!`
    // on the same predicate at the spawn itself.
    assert!(
        !build_profile::storefront_polling_allowed_for(EDITION),
        "the pricing-pipeline daemon spawn gate must be closed on this build"
    );

    // And the runtime backstop refuses by name, so a hand-edited config
    // cannot talk a Portable build into fetching a customer's CAD.
    let refused =
        build_profile::assert_storefront_reach_allowed("fetch customer CAD for extraction");
    let err = refused.expect_err("Portable must REFUSE storefront reach");
    let msg = err.to_string();
    assert!(
        msg.contains("refuses storefront reach"),
        "refusal must name what it refused: {msg}"
    );
}

/// The Defense arm — what `cargo test --features production` runs.
#[cfg(feature = "production")]
#[test]
#[allow(clippy::assertions_on_constants)]
fn defense_build_may_reach_the_cad_extractor() {
    assert!(matches!(EDITION, Edition::Defense));
    assert!(
        storefront_polling_allowed(),
        "Defense is the edition that runs the CAD line"
    );
    build_profile::assert_storefront_reach_allowed("fetch customer CAD for extraction")
        .expect("Defense must PERMIT storefront reach");
}

// ── 3. The STL rejection classifies Permanent, for free ──────────────────

/// ADR-0112 A.2's "free classification" trick, pinned.
///
/// The new Python-side STL rejection keeps the literal substring
/// `Unsupported file extension`, which `classify_failure` already maps
/// to `Permanent`. That is what buys correct no-auto-retry behaviour
/// with ZERO change to the classifier — but it is a *string* coupling
/// across a language boundary, so nothing would fail to compile if the
/// message were reworded. If it drifts, STL failures fall through to
/// `Unknown` and enter the capped auto-retry loop: the daemon would
/// re-download, re-decrypt and re-extract a file that can never succeed,
/// on every cycle, for every STL quote.
#[test]
fn stl_rejection_message_classifies_permanent() {
    // The message verbatim from `cli.py::_route`, as it arrives on the
    // Rust side (the wrapper bubbles Python stderr into the reason).
    let stl_message = "Unsupported file extension '.stl'. STL is a triangle mesh with no \
                       topology: it cannot carry hole axes, depths or diameters, so it cannot \
                       be priced for drilling or programmed for toolpaths. Re-export the part \
                       as STEP (AP203/AP214/AP242). Supported: .step, .stp";

    assert_eq!(
        classify_failure("extract", stl_message),
        FailureKind::Permanent,
        "an STL submission must fail ONCE, permanently — never enter the auto-retry loop"
    );
}

#[test]
fn generic_unsupported_format_still_classifies_permanent() {
    // The other rejection branch — `.iges`/`.dxf`/`.sldprt`/… — is
    // unchanged by Part A and must stay Permanent too.
    let generic = "Unsupported file extension '.iges'. Supported: .step, .stp";
    assert_eq!(classify_failure("extract", generic), FailureKind::Permanent);
}

#[test]
fn the_classifier_keys_on_the_substring_not_the_whole_message() {
    // Pin the mechanism, so a future reword that KEEPS the substring is
    // provably still safe (and one that drops it is provably not).
    assert_eq!(
        classify_failure("extract", "unsupported file extension"),
        FailureKind::Permanent,
        "lowercase form must classify — classify_failure lowercases both sides"
    );
    assert_ne!(
        classify_failure("extract", "STL files cannot be quoted; please send STEP"),
        FailureKind::Permanent,
        "a reword that DROPS the substring must NOT silently keep classifying Permanent — \
         this is the assertion that makes the pin above meaningful"
    );
}

// ── 4. The storefront CAD picker is STEP-only too ────────────────────────

/// ADR-0112 Part A, the `apps/aberp` half.
///
/// Dropping `.stl` from the Python dispatcher alone would leave the
/// daemon still *selecting* STL attachments: it would download the file,
/// encrypt it at rest, insert a job row and advance it — burning a fetch
/// and a blob — purely to fail Permanent at the extract step. Not picking
/// it routes the quote to `enqueue_failed_no_cad` instead, which is the
/// honest verdict.
#[test]
fn cad_picker_accepts_step_and_rejects_stl() {
    use aberp::quote_pricing_pipeline::is_extractable_cad;

    assert!(is_extractable_cad("bracket.step"));
    assert!(is_extractable_cad("bracket.stp"));
    // The storefront echoes the customer's own filename verbatim.
    assert!(is_extractable_cad("BRACKET.STEP"));
    assert!(is_extractable_cad("Bracket.Stp"));

    assert!(
        !is_extractable_cad("bracket.stl"),
        "STL must not be selected — it can only fail Permanent one step later"
    );
    assert!(!is_extractable_cad("BRACKET.STL"));
    // C6's wider storefront allow-list — none of these are extractable.
    for name in [
        "p.iges", "p.igs", "p.dxf", "p.dwg", "p.sldprt", "p.3mf", "p.obj",
    ] {
        assert!(!is_extractable_cad(name), "{name} must not be selected");
    }
    // A quote-request PDF sitting alongside the CAD must never be picked.
    assert!(!is_extractable_cad("drawing.pdf"));
    // …and a filename that merely CONTAINS the token is not a suffix match.
    assert!(!is_extractable_cad("step-by-step-notes.txt"));
}

#[test]
fn picker_and_extractor_accept_the_same_set() {
    use aberp::quote_pricing_pipeline::EXTRACTABLE_CAD_SUFFIXES;

    // Mirrors `cli.SUPPORTED_SUFFIXES` on the Python side. The two live in
    // different languages so nothing enforces the equality at compile
    // time; the Python-side twin of this assertion is
    // `test_cli_supported_suffixes_are_exactly_step_and_stp`.
    let mut got: Vec<&str> = EXTRACTABLE_CAD_SUFFIXES.to_vec();
    got.sort_unstable();
    assert_eq!(got, vec![".step", ".stp"]);
}
