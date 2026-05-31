//! `[seller.branding]` reader + section preservation helper for the
//! S195 / PR-195 PDF branding-color slice.
//!
//! # Posture
//!
//! Operator-edited TOML section under `seller.toml`:
//!
//! ```toml
//! [seller.branding]
//! primary_color = "#1a2332"
//! ```
//!
//! Single optional key in v1 (`primary_color`). When set, the PDF
//! renderer uses the parsed RGB to replace the silver title under-rule,
//! the silver table-header rule, AND the gold totals-banner rule. Absent
//! section → renderer falls back to the pre-PR-195 ADR-0044 palette,
//! byte-for-byte. A malformed hex string downgrades to a `tracing::warn!`
//! and the same fallback — legal-document rendering must never be
//! blocked by a branding asset (same posture as PR-185 logo handling).
//!
//! No SPA settings UI in v1 (per the S195 brief). Operator hand-edits
//! the file. Persistence across the four SPA write paths
//! (identity / banks / smtp / numbering — see
//! [`crate::seller_toml_backup`] memory + ADR-0040 / PR-170) is handled
//! by:
//!
//! 1. the three section-replace writers (banks / smtp / numbering)
//!    naturally preserving every line outside their own section, so an
//!    unknown `[seller.branding]` block survives unchanged; and
//! 2. the identity writer in
//!    [`crate::setup_seller_info`] explicitly re-appending the parsed
//!    branding block via [`to_toml_section`] (mirrors the smtp +
//!    numbering preservation patterns added in PR-170 / S170).
//!
//! Both round-trip pinned by tests in the respective modules.

use std::fs;
use std::path::Path;

use anyhow::{anyhow, Context, Result};

/// Parsed `[seller.branding]` section. v1 carries one optional field —
/// the raw hex string the operator typed. Color parsing into the
/// renderer's `(f32, f32, f32)` happens at the PDF render call site
/// via [`parse_color_hex`], not here, so a stale TOML string never
/// gets re-canonicalized through the read+write round-trip (avoids
/// drift between "what the operator typed" and "what the renderer
/// uses" in audit / backup snapshots).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrandingConfig {
    pub primary_color: String,
}

/// Read the `[seller.branding]` section of `seller.toml` at `path`.
/// Returns `Ok(None)` when the file exists but carries no branding
/// section (the byte-for-byte fallback path — current behaviour for
/// every tenant that has not opted in to a custom brand color). `Err`
/// only on I/O failure.
///
/// Mirrors [`crate::smtp_config::read_smtp_config`]'s
/// missing-section-is-`None` posture.
pub fn read_branding_config(path: &Path) -> Result<Option<BrandingConfig>> {
    if !path.exists() {
        return Ok(None);
    }
    let body = fs::read_to_string(path)
        .with_context(|| format!("read seller.toml at {}", path.display()))?;
    parse_branding_section(&body)
}

/// Parse the `[seller.branding]` section out of an in-memory
/// seller.toml body. Hand-rolled line walker matching
/// [`crate::smtp_config::parse_smtp_section`]'s style — keeps the
/// dependency surface narrow (no `toml` crate floor for one field).
///
/// Missing section → `Ok(None)` (default-color path). Section present
/// but `primary_color` missing → `Ok(None)` too — the block is
/// vestigial (operator deleted the value but kept the header), so
/// behave as if absent.
pub fn parse_branding_section(body: &str) -> Result<Option<BrandingConfig>> {
    let mut primary_color: Option<String> = None;
    let mut in_section = false;
    for raw in body.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with("[[") && line.ends_with("]]") {
            in_section = false;
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            let inner = line[1..line.len() - 1].trim();
            in_section = inner == "seller.branding";
            continue;
        }
        if !in_section {
            continue;
        }
        let (k, v) = match line.split_once('=') {
            Some(p) => p,
            None => {
                return Err(anyhow!(
                    "[seller.branding] expected `key = value`, got `{line}`"
                ))
            }
        };
        let key = k.trim();
        let value = strip_quotes(v.trim()).to_string();
        match key {
            "primary_color" => {
                if !value.is_empty() {
                    primary_color = Some(value);
                }
            }
            _ => {
                // Silently ignore unknown keys — forward-compat with a
                // future v2 addition (e.g. `accent_color`,
                // `logo_position`).
            }
        }
    }
    match primary_color {
        Some(c) => Ok(Some(BrandingConfig { primary_color: c })),
        None => Ok(None),
    }
}

/// Render a [`BrandingConfig`] as the canonical `[seller.branding]`
/// section. Used by [`crate::setup_seller_info`] to re-append the
/// preserved branding block across an identity write — same posture
/// as [`crate::smtp_config::to_toml_section`] +
/// [`crate::numbering::to_toml_section`].
pub fn to_toml_section(cfg: &BrandingConfig) -> String {
    let mut out = String::new();
    out.push_str("[seller.branding]\n");
    out.push_str(&format!("primary_color = \"{}\"\n", cfg.primary_color));
    out
}

fn strip_quotes(s: &str) -> &str {
    let t = s.trim();
    if t.starts_with('"') && t.ends_with('"') && t.len() >= 2 {
        &t[1..t.len() - 1]
    } else {
        t
    }
}

/// Parse a `#RRGGBB` or `#RRGGBBAA` hex color into the PDF
/// renderer's `(f32, f32, f32)` RGB-in-0..=1 shape. Alpha (when
/// supplied) is consumed and discarded — the PDF surface is opaque
/// throughout (per ADR-0044), so a transparency request is
/// not actionable at the renderer level. Strict — any other length
/// or a non-hex digit returns `None` and the caller WARN-logs +
/// falls back to the default palette.
///
/// Accepts upper- and lowercase a-f. Returns `None` for:
///   - empty / missing `#` prefix
///   - body length other than 6 or 8 hex digits
///   - any non-hex digit in the body
///
/// Pin: round-trip a single channel — `#ff0000` → `(1.0, 0.0, 0.0)`.
pub fn parse_color_hex(hex: &str) -> Option<(f32, f32, f32)> {
    let body = hex.strip_prefix('#')?;
    if body.len() != 6 && body.len() != 8 {
        return None;
    }
    let parse = |s: &str| u8::from_str_radix(s, 16).ok();
    let r = parse(&body[0..2])?;
    let g = parse(&body[2..4])?;
    let b = parse(&body[4..6])?;
    // Alpha (body[6..8]) is parsed-and-discarded only as input
    // validation — if it's present and malformed, the whole string
    // is rejected. We don't carry it into the return shape because
    // the PDF surface is opaque per ADR-0044.
    if body.len() == 8 && parse(&body[6..8]).is_none() {
        return None;
    }
    Some((r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_color_hex_pure_red() {
        assert_eq!(parse_color_hex("#ff0000"), Some((1.0, 0.0, 0.0)));
        assert_eq!(parse_color_hex("#FF0000"), Some((1.0, 0.0, 0.0)));
    }

    #[test]
    fn parse_color_hex_brief_example_navy() {
        // The brief's example `#1a2332` = (26, 35, 50) / 255.
        let (r, g, b) = parse_color_hex("#1a2332").expect("valid 6-digit hex");
        let approx = |a: f32, b: f32| (a - b).abs() < 1e-4;
        assert!(approx(r, 26.0 / 255.0), "r = {r}");
        assert!(approx(g, 35.0 / 255.0), "g = {g}");
        assert!(approx(b, 50.0 / 255.0), "b = {b}");
    }

    #[test]
    fn parse_color_hex_accepts_eight_digit_with_alpha_and_discards() {
        // `#1a2332ff` — alpha 0xff (fully opaque); the renderer
        // surface is opaque throughout so the alpha is consumed
        // for validation then discarded.
        let result = parse_color_hex("#1a2332ff");
        let approx = |a: f32, b: f32| (a - b).abs() < 1e-4;
        let (r, g, b) = result.expect("8-digit form must parse");
        assert!(approx(r, 26.0 / 255.0));
        assert!(approx(g, 35.0 / 255.0));
        assert!(approx(b, 50.0 / 255.0));
    }

    #[test]
    fn parse_color_hex_rejects_missing_hash() {
        // CLAUDE.md rule 12 — fail loud, no implicit hash insertion.
        assert_eq!(parse_color_hex("1a2332"), None);
    }

    #[test]
    fn parse_color_hex_rejects_short_form() {
        // `#abc` shorthand (CSS-style three-digit) — deliberately
        // unsupported. Strict to keep the parser unambiguous and the
        // operator-visible shape one-pattern.
        assert_eq!(parse_color_hex("#abc"), None);
    }

    #[test]
    fn parse_color_hex_rejects_non_hex_digits() {
        assert_eq!(parse_color_hex("#xyz123"), None);
        assert_eq!(parse_color_hex("#1a23xy"), None);
        assert_eq!(parse_color_hex("#1a2332zz"), None, "alpha non-hex");
    }

    #[test]
    fn parse_color_hex_rejects_wrong_length() {
        assert_eq!(parse_color_hex("#1234"), None);
        assert_eq!(parse_color_hex("#1234567"), None);
        assert_eq!(parse_color_hex("#1234567890"), None);
        assert_eq!(parse_color_hex("#"), None);
    }

    #[test]
    fn parse_section_returns_none_when_absent() {
        let body = "[seller]\nlegal_name = \"X\"\n";
        let parsed = parse_branding_section(body).unwrap();
        assert!(parsed.is_none(), "no [seller.branding] ⇒ None");
    }

    #[test]
    fn parse_section_reads_primary_color() {
        let body = r##"
[seller]
legal_name = "X"

[seller.branding]
primary_color = "#1a2332"
"##;
        let parsed = parse_branding_section(body).unwrap().expect("present");
        assert_eq!(parsed.primary_color, "#1a2332");
    }

    #[test]
    fn parse_section_returns_none_when_section_present_but_value_missing() {
        // Operator deleted the value but kept the header — degrade to
        // the default path rather than re-erroring (the next SPA write
        // would otherwise loud-fail before the operator could fix it).
        let body = "[seller.branding]\n# primary_color = \"#1a2332\"\n";
        let parsed = parse_branding_section(body).unwrap();
        assert!(parsed.is_none());
    }

    #[test]
    fn parse_section_silently_ignores_unknown_keys() {
        // Forward-compat: a v2 addition (e.g. accent_color) reading
        // through a v1 binary must not error.
        let body = r##"
[seller.branding]
primary_color = "#1a2332"
accent_color = "#deadbeef"
"##;
        let parsed = parse_branding_section(body).unwrap().expect("present");
        assert_eq!(parsed.primary_color, "#1a2332");
    }

    #[test]
    fn to_toml_section_round_trips_through_parser() {
        let cfg = BrandingConfig {
            primary_color: "#1a2332".to_string(),
        };
        let section = to_toml_section(&cfg);
        let parsed = parse_branding_section(&section).unwrap().expect("present");
        assert_eq!(parsed, cfg);
    }

    #[test]
    fn parse_section_stops_at_next_section_header() {
        // The walker tracks `in_section`; a sibling section after
        // branding must not pollute the primary_color slot.
        let body = r##"
[seller.branding]
primary_color = "#1a2332"

[seller.numbering]
primary_color = "#ffffff"
"##;
        let parsed = parse_branding_section(body).unwrap().expect("present");
        assert_eq!(
            parsed.primary_color, "#1a2332",
            "the numbering section's spurious key must not leak through"
        );
    }
}
