//! The portal's hostname must never be committed (ADR-0113 §3.2 +
//! Ervin's §9.2 decision).
//!
//! The wildcard `*.abenerp.com` certificate keeps the label out of
//! every Certificate Transparency log. That control is worth nothing if
//! the label is written into a source file instead — a repository is a
//! far easier place to read than a CT log. So the concrete hostname is
//! minted at deploy time and reaches the agent through `PORTAL_HOST`.
//!
//! This test is the mechanical enforcement. It scans the three portal
//! crates for anything that looks like a real portal hostname, and
//! fails if one appears — including in a comment, a doc example, a
//! default value, or a test fixture.
//!
//! What it does NOT forbid: the apex on its own, the **wildcard**
//! `*.<apex>` (that certificate exists so the label stays private — it
//! is the control, not the leak), and placeholders like
//! `<PORTAL_HOST>`. Those are how the value is referred to without
//! being one.

use std::path::{Path, PathBuf};

/// The apex the portal's label sits under.
///
/// Assembled from fragments rather than written out, so this file — the
/// one place that has to reason about the pattern — does not itself
/// become the leak, and needs no self-exemption from its own scan. A
/// gate with an exemption is a gate with a hiding place.
fn apex() -> String {
    format!(".{}{}", "abenerp", ".com")
}

fn portal_sources() -> Vec<PathBuf> {
    let crates_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut out = Vec::new();
    for crate_name in [
        "aberp-portal-core",
        "aberp-portal-agent",
        "aberp-portal-relay",
    ] {
        collect(&crates_dir.join(crate_name), &mut out);
    }
    assert!(
        out.len() > 10,
        "the hostname scan found only {} files — it is not looking where it thinks it is",
        out.len()
    );
    out
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            collect(&path, out);
        } else if path
            .extension()
            .is_some_and(|e| e == "rs" || e == "toml" || e == "html")
        {
            out.push(path);
        }
    }
}

#[test]
fn no_portal_source_file_contains_a_real_hostname() {
    let mut offenders = Vec::new();
    for path in portal_sources() {
        let Ok(body) = std::fs::read_to_string(&path) else {
            continue;
        };
        for (n, line) in body.lines().enumerate() {
            if let Some(label) = committed_label(&line.to_ascii_lowercase()) {
                offenders.push(format!(
                    "{}:{}: label `{label}` — {}",
                    path.display(),
                    n + 1,
                    line.trim()
                ));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "ADR-0113 §3.2 — the portal hostname is minted at deploy time and must never be \
         committed. The wildcard certificate keeps the label out of CT logs; a source file \
         would put it right back. Read it from `PORTAL_HOST` instead.\n{}",
        offenders.join("\n")
    );
}

/// Return the committed label, if this line names one.
///
/// The **apex** is not a secret and appears in prose here and in
/// ADR-0113 itself; the **wildcard** `*.<apex>` is not a secret either
/// — it is the certificate that exists precisely so the label stays
/// private. What must never appear is a concrete label in front of the
/// apex. So: find the apex, read backwards over the label characters,
/// and complain only if a real label is sitting there.
fn committed_label(lower: &str) -> Option<String> {
    let apex = apex();
    for (i, _) in lower.match_indices(&apex) {
        let before = &lower[..i];
        let label: String = before
            .chars()
            .rev()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        if !label.is_empty() {
            return Some(label);
        }
    }
    None
}

#[test]
fn the_scan_would_actually_catch_something() {
    // A gate with no teeth is worse than no gate: it reads as coverage.
    // Every fixture is assembled, never written out — see `apex`.
    let a = apex();
    assert_eq!(
        committed_label(&format!("https://tapir-vellum-brisk{a}/")),
        // The hyphenated triad is one label — Ervin's §9.2 decision is
        // a memorable multi-word name, and it must be caught whole.
        Some("tapir-vellum-brisk".to_string()),
        "a concrete label must be caught"
    );
    assert_eq!(
        committed_label(&format!("https://internal{a}/x")),
        Some("internal".to_string()),
        "the ADR's original label must be caught too — Ervin's §9.2 decision replaced it"
    );
    // …and the things that are NOT the secret must pass: the wildcard
    // certificate, a placeholder, and the bare apex in prose.
    assert_eq!(committed_label(&format!("the wildcard *{a} cert")), None);
    assert_eq!(committed_label(&format!("https://<portal_host>{a}/")), None);
    // The bare apex, with no label in front of it, is prose — not a
    // hostname. (Note the fixture has no leading dot: that IS the case.)
    assert_eq!(
        committed_label(&format!("the {}{} apex", "abenerp", ".com")),
        None
    );
    assert_eq!(committed_label("kept internal. the agent decides"), None);
}

#[test]
fn the_config_module_has_no_default_for_the_host() {
    // Belt to the braces above: a default would make the missing-value
    // error unreachable and quietly bind passkeys to the wrong RP.
    let cfg = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/config.rs");
    let body = std::fs::read_to_string(cfg).expect("config.rs");
    assert!(
        body.contains("ConfigError::MissingHost"),
        "PORTAL_HOST must be a hard startup error, not a defaulted value"
    );
}
