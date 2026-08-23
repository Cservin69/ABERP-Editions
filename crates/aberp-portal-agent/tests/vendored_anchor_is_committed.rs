//! The pinned Apple root must be **in the repository**, not merely on
//! the machine that wrote it.
//!
//! This is a regression guard for a live defect, not a hypothetical.
//! `.gitignore` carries a blanket `*.pem` secret guard whose only
//! exception is `!crates/*/roots/*.pem`. The anchor was first vendored
//! into `crates/aberp-portal-agent/assets/`, which that exception does
//! not reach — so `git add` silently did nothing, the file never
//! entered a commit, and the branch **did not build from a clean
//! checkout**, because `webauthn/attestation.rs` includes it with
//! `include_str!` at compile time. It built for everyone who already
//! had the untracked file on disk, which is precisely the class of
//! defect that survives review.
//!
//! `attestation.rs` already pins the file's SHA-256, so a *substituted*
//! anchor fails. This asserts the other half: that there is a file at
//! all for a fresh clone to compile against.

use std::path::{Path, PathBuf};
use std::process::Command;

/// `crates/aberp-portal-agent/roots/Apple_WebAuthn_Root_CA.pem`,
/// relative to the repository root.
const ANCHOR: &str = "crates/aberp-portal-agent/roots/Apple_WebAuthn_Root_CA.pem";

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is `<repo>/crates/aberp-portal-agent`.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the crate lives two levels below the repository root")
        .to_path_buf()
}

#[test]
fn the_vendored_anchor_is_tracked_by_git() {
    let root = repo_root();
    assert!(
        root.join(ANCHOR).is_file(),
        "{ANCHOR} is missing from the working tree"
    );

    let out = Command::new("git")
        .args(["ls-files", "--error-unmatch", ANCHOR])
        .current_dir(&root)
        .output()
        .expect("running `git ls-files` — this test needs git and a checkout");

    assert!(
        out.status.success(),
        "{ANCHOR} exists on disk but is NOT tracked by git, so a clean \
         checkout cannot compile this crate. It is almost certainly being \
         swallowed by the blanket `*.pem` rule in .gitignore, whose only \
         exception is `!crates/*/roots/*.pem`. git said: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
}

#[test]
fn the_anchor_directory_is_the_one_gitignore_un_ignores() {
    // The exception is `!crates/*/roots/*.pem` — one path segment for
    // the crate, then literally `roots`. A future anchor dropped into
    // `assets/`, `vendor/` or `roots/apple/` is invisible to it, and
    // the failure mode is a build that works only on the machine that
    // wrote the file.
    let rules =
        std::fs::read_to_string(repo_root().join(".gitignore")).expect("reading .gitignore");
    assert!(
        rules.lines().any(|l| l.trim() == "!crates/*/roots/*.pem"),
        "the un-ignore rule this crate's build depends on is gone from .gitignore"
    );
    let mut segments = ANCHOR.split('/');
    assert_eq!(segments.next(), Some("crates"));
    let _crate_dir = segments.next().expect("a crate directory");
    assert_eq!(
        segments.next(),
        Some("roots"),
        "the anchor must sit directly in `roots/` for the exception to reach it"
    );
    assert!(segments.next().is_some(), "and then the file itself");
    assert_eq!(segments.next(), None, "with no directory in between");
}
