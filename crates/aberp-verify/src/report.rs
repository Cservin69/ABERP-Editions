//! Operator-visible reporting per ADR-0035 §7.
//!
//! [`Report`] collects [`CheckOutcome`] entries (one per ADR-0035
//! §3 invariant check) and renders them at the end. The exit-code
//! discipline (0 on all-OK, 1 on any FAIL) lives in `main.rs`; this
//! module owns the report's STATE + the operator-visible rendering.

use std::path::{Path, PathBuf};

/// One check's outcome — OK, NOTE, or FAIL — plus the check's name
/// and a one-line detail string. Per ADR-0035 §7: the rendered form
/// is one line per outcome, with the prefix `[OK]` / `[NOTE]` /
/// `[FAIL]`.
#[derive(Debug, Clone)]
pub struct CheckOutcome {
    /// `Ok` / `Note` / `Fail`. Drives both the line prefix and the
    /// report's overall is_ok() determination.
    pub level: CheckLevel,
    /// Short check name (e.g., `"manifest version"`). Becomes the
    /// `[OK] <name>: ...` prefix in the rendered output.
    pub name: &'static str,
    /// One-line detail. Multi-line details fold to one line; the
    /// verifier's diagnostics are deliberately single-line so
    /// piping the report through tools like `grep FAIL` works
    /// without surprises.
    pub detail: String,
}

/// The three outcome levels per ADR-0035 §7.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckLevel {
    /// The check passed. Does not affect is_ok().
    Ok,
    /// Informational — typically a seq gap delegated to the
    /// manifest's chain_verified claim per ADR-0035 §"Surfaced
    /// conflict 3" Reading B. Does NOT cause is_ok() to return
    /// false.
    Note,
    /// The check failed. is_ok() returns false.
    Fail,
}

impl CheckOutcome {
    pub fn ok(name: &'static str, detail: String) -> Self {
        Self {
            level: CheckLevel::Ok,
            name,
            detail,
        }
    }
    pub fn note(name: &'static str, detail: String) -> Self {
        Self {
            level: CheckLevel::Note,
            name,
            detail,
        }
    }
    pub fn fail(name: &'static str, detail: String) -> Self {
        Self {
            level: CheckLevel::Fail,
            name,
            detail,
        }
    }
}

/// Accumulated checks for one bundle verification run.
#[derive(Debug)]
pub struct Report {
    bundle_path: PathBuf,
    outcomes: Vec<CheckOutcome>,
    /// Captured from the parsed manifest so the summary line names
    /// the invoice id explicitly. `None` until `set_summary_invoice_id`
    /// is called (which `verify::run_checks` does after parsing
    /// the manifest).
    summary_invoice_id: Option<String>,
}

impl Report {
    pub fn new(bundle_path: PathBuf) -> Self {
        Self {
            bundle_path,
            outcomes: Vec::new(),
            summary_invoice_id: None,
        }
    }

    pub fn push(&mut self, outcome: CheckOutcome) {
        self.outcomes.push(outcome);
    }

    pub fn set_summary_invoice_id(&mut self, id: String) {
        self.summary_invoice_id = Some(id);
    }

    /// True iff every outcome is [`CheckLevel::Ok`] or
    /// [`CheckLevel::Note`]. A single Fail makes this false; drives
    /// the binary's exit code.
    pub fn is_ok(&self) -> bool {
        !self
            .outcomes
            .iter()
            .any(|o| matches!(o.level, CheckLevel::Fail))
    }

    /// Number of outcomes at each level. Used in the summary line.
    pub fn counts(&self) -> (usize, usize, usize) {
        let mut ok = 0;
        let mut note = 0;
        let mut fail = 0;
        for o in &self.outcomes {
            match o.level {
                CheckLevel::Ok => ok += 1,
                CheckLevel::Note => note += 1,
                CheckLevel::Fail => fail += 1,
            }
        }
        (ok, note, fail)
    }

    /// Render the report to stdout per ADR-0035 §7. `quiet=true`
    /// suppresses OK lines; NOTE + FAIL lines and the summary
    /// always print.
    pub fn print(&self, bundle_path: &Path, quiet: bool) {
        println!("aberp-verify: {}", bundle_path.display());
        for o in &self.outcomes {
            match o.level {
                CheckLevel::Ok => {
                    if !quiet {
                        println!("  [OK]   {}: {}", o.name, o.detail);
                    }
                }
                CheckLevel::Note => {
                    println!("  [NOTE] {}: {}", o.name, o.detail);
                }
                CheckLevel::Fail => {
                    println!("  [FAIL] {}: {}", o.name, o.detail);
                }
            }
        }
        let (ok, note, fail) = self.counts();
        let inv = self
            .summary_invoice_id
            .as_deref()
            .unwrap_or("<manifest-parse-failed>");
        if self.is_ok() {
            println!(
                "\nSUMMARY: bundle OK ({} check(s) passed, {} note(s)). \
                 Invoice {}. This bundle is UNSIGNED (signing deferred per F5 — \
                 ADR-0029 §4); full-chain claim trusted via \
                 manifest.chain_verified=true.",
                ok, note, inv
            );
        } else {
            println!(
                "\nSUMMARY: bundle FAILED ({} fail(s), {} note(s), {} ok). \
                 Invoice {}. Resolve the [FAIL] lines above before treating \
                 this bundle as authoritative audit evidence.",
                fail, note, ok, inv
            );
        }
        // Path is also referenced here so a future grep over the
        // report can correlate by path; kept inside the body for
        // single-source-of-truth output.
        let _ = self.bundle_path.display();
    }

    /// Test-only helper: render the report to a String so unit
    /// tests can grep for FAIL / NOTE substrings without hijacking
    /// stdout.
    #[cfg(test)]
    pub fn compose_for_test(&self) -> String {
        use std::fmt::Write;
        let mut out = String::new();
        let _ = writeln!(out, "aberp-verify: {}", self.bundle_path.display());
        for o in &self.outcomes {
            let prefix = match o.level {
                CheckLevel::Ok => "[OK]",
                CheckLevel::Note => "[NOTE]",
                CheckLevel::Fail => "[FAIL]",
            };
            let _ = writeln!(out, "  {} {}: {}", prefix, o.name, o.detail);
        }
        let (ok, note, fail) = self.counts();
        let _ = writeln!(out, "counts ok={ok} note={note} fail={fail}");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_report_is_ok() {
        let r = Report::new("/tmp/x".into());
        assert!(r.is_ok());
        assert_eq!(r.counts(), (0, 0, 0));
    }

    #[test]
    fn note_does_not_fail_the_report() {
        let mut r = Report::new("/tmp/x".into());
        r.push(CheckOutcome::ok("a", "ok".to_string()));
        r.push(CheckOutcome::note("b", "informational".to_string()));
        assert!(r.is_ok(), "NOTE must not flip is_ok to false");
        assert_eq!(r.counts(), (1, 1, 0));
    }

    #[test]
    fn fail_flips_is_ok_to_false() {
        let mut r = Report::new("/tmp/x".into());
        r.push(CheckOutcome::ok("a", "ok".to_string()));
        r.push(CheckOutcome::fail("b", "broken".to_string()));
        assert!(!r.is_ok(), "any FAIL must flip is_ok to false");
    }

    #[test]
    fn compose_for_test_carries_every_outcome() {
        let mut r = Report::new("/tmp/x".into());
        r.push(CheckOutcome::ok("a", "ok".to_string()));
        r.push(CheckOutcome::note("b", "info".to_string()));
        r.push(CheckOutcome::fail("c", "broken".to_string()));
        let composed = r.compose_for_test();
        assert!(composed.contains("[OK] a"));
        assert!(composed.contains("[NOTE] b"));
        assert!(composed.contains("[FAIL] c"));
        assert!(composed.contains("counts ok=1 note=1 fail=1"));
    }
}
