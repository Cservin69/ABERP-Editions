//! S211 / PR-210 — `[quote_intake]` non-secret config in seller.toml.
//!
//! Stored as a top-level `[quote_intake]` section in
//! `~/.aberp/<tenant>/seller.toml`. Keychain/TOML split mirrors PR-92's
//! SMTP posture: the bearer token lives in the OS keychain via
//! [`crate::quote_intake_credentials`]; non-secrets live here on disk.
//!
//! The merge writer preserves every other section verbatim — identity
//! ([seller]/[seller.address]), banks ([[seller.banks]]), numbering
//! ([seller.numbering]), SMTP ([seller.smtp]), branding
//! ([seller.branding]) — same posture as PR-89 / PR-92 / PR-195. The
//! six SPA write surfaces compose without stomping each other (the
//! seller.toml write invariant pinned in
//! [[project_seller_toml_write_invariant]] now extended to 6 slots).
//!
//! # Why a top-level `[quote_intake]` (not `[seller.quote_intake]`)
//!
//! `quote_intake` is operational config for a sister-service poller —
//! not a property of the seller's invoice identity. Top-level naming
//! signals that the table addresses a distinct subsystem; every other
//! `seller.*` subtable describes how invoices the seller emits are
//! shaped or routed.

use std::fs;
use std::io::Write as _;
use std::path::Path;

use anyhow::{anyhow, Context as _, Result};

/// S211 — config bounds. Matches `aberp-quote-intake` crate
/// `{MIN,MAX,DEFAULT}_POLL_INTERVAL_SECS`.
pub const MIN_POLL_INTERVAL_SECS: u64 = 10;
pub const MAX_POLL_INTERVAL_SECS: u64 = 3600;
pub const DEFAULT_POLL_INTERVAL_SECS: u64 = 60;

/// Non-secret quote-intake config persisted under `[quote_intake]`. The
/// bearer token is NEVER carried here — see
/// [`crate::quote_intake_credentials`] for the keychain surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuoteIntakeTomlConfig {
    /// Whether the daemon should spawn at boot. `false` = dormant.
    pub enabled: bool,
    /// Sister-service base URL (no trailing slash). MUST start with
    /// `http://` or `https://`. Optional in TOML so an operator can
    /// pre-disable without filling fields; required when `enabled`.
    pub base_url: Option<String>,
    /// Poll cadence in seconds, clamped to
    /// [`MIN_POLL_INTERVAL_SECS`..=`MAX_POLL_INTERVAL_SECS`]. `None`
    /// = default 60s.
    pub poll_interval_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct QuoteIntakeConfigValidationError {
    pub field: &'static str,
    pub message: String,
}

impl QuoteIntakeTomlConfig {
    /// Validate field-level invariants. When `enabled=true`, `base_url`
    /// MUST be present and scheme-correct. Returns a Vec so the SPA
    /// can surface every problem at once.
    pub fn validate(&self) -> Result<(), Vec<QuoteIntakeConfigValidationError>> {
        let mut errors = Vec::new();
        if self.enabled {
            match self.base_url.as_deref() {
                None => errors.push(QuoteIntakeConfigValidationError {
                    field: "base_url",
                    message: "base_url is required when enabled=true".to_string(),
                }),
                Some(url) if url.trim().is_empty() => {
                    errors.push(QuoteIntakeConfigValidationError {
                        field: "base_url",
                        message: "base_url must be non-empty when enabled=true".to_string(),
                    })
                }
                Some(url) => {
                    if !(url.starts_with("http://") || url.starts_with("https://")) {
                        errors.push(QuoteIntakeConfigValidationError {
                            field: "base_url",
                            message: format!(
                                "base_url must start with http:// or https:// (got `{url}`)"
                            ),
                        });
                    }
                    if let Err(msg) = validate_no_toml_metachars("base_url", url) {
                        errors.push(QuoteIntakeConfigValidationError {
                            field: "base_url",
                            message: msg,
                        });
                    }
                }
            }
        }
        if let Some(interval) = self.poll_interval_secs {
            if !(MIN_POLL_INTERVAL_SECS..=MAX_POLL_INTERVAL_SECS).contains(&interval) {
                errors.push(QuoteIntakeConfigValidationError {
                    field: "poll_interval_secs",
                    message: format!(
                        "poll_interval_secs must be between {MIN_POLL_INTERVAL_SECS} and {MAX_POLL_INTERVAL_SECS} (got {interval})"
                    ),
                });
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

/// Reject TOML metachars in URL field. Same posture as
/// `smtp_config::validate_no_toml_metachars` — defence against an
/// operator typing a value that breaks the line-walker parser on the
/// next read.
fn validate_no_toml_metachars(field: &'static str, value: &str) -> Result<(), String> {
    for c in value.chars() {
        if c == '"' || c == '\n' || c == '\r' {
            return Err(format!(
                "{field} contains a character that would break TOML serialisation: {c:?}"
            ));
        }
    }
    Ok(())
}

pub fn read_quote_intake_config(path: &Path) -> Result<Option<QuoteIntakeTomlConfig>> {
    if !path.exists() {
        return Ok(None);
    }
    let body = fs::read_to_string(path)
        .with_context(|| format!("read seller.toml at {}", path.display()))?;
    parse_quote_intake_section(&body)
}

/// Parse the top-level `[quote_intake]` section out of an in-memory
/// seller.toml body. Hand-rolled line-walker matching
/// `smtp_config::parse_smtp_section`'s style.
pub fn parse_quote_intake_section(body: &str) -> Result<Option<QuoteIntakeTomlConfig>> {
    let mut enabled: Option<bool> = None;
    let mut base_url: Option<String> = None;
    let mut poll_interval_secs: Option<u64> = None;
    let mut in_section = false;
    let mut section_seen = false;
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
            in_section = inner == "quote_intake";
            if in_section {
                section_seen = true;
            }
            continue;
        }
        if !in_section {
            continue;
        }
        let (k, v) = match line.split_once('=') {
            Some(p) => p,
            None => {
                return Err(anyhow!(
                    "[quote_intake] expected `key = value`, got `{line}`"
                ))
            }
        };
        let key = k.trim();
        let value = v.trim();
        match key {
            "enabled" => {
                enabled = Some(match strip_quotes(value) {
                    "true" => true,
                    "false" => false,
                    other => {
                        return Err(anyhow!(
                            "[quote_intake] enabled must be `true` or `false`, got `{other}`"
                        ))
                    }
                });
            }
            "base_url" => base_url = Some(strip_quotes(value).to_string()),
            "poll_interval_secs" => {
                poll_interval_secs = Some(strip_quotes(value).parse::<u64>().map_err(|e| {
                    anyhow!("[quote_intake] poll_interval_secs `{value}` is not a u64: {e}")
                })?);
            }
            _ => {
                // Silently ignore unknown keys (forward-compat).
            }
        }
    }
    if !section_seen {
        return Ok(None);
    }
    let cfg = QuoteIntakeTomlConfig {
        enabled: enabled.unwrap_or(false),
        base_url: base_url.filter(|s| !s.is_empty()),
        poll_interval_secs,
    };
    Ok(Some(cfg))
}

fn strip_quotes(s: &str) -> &str {
    let t = s.trim();
    if t.starts_with('"') && t.ends_with('"') && t.len() >= 2 {
        &t[1..t.len() - 1]
    } else {
        t
    }
}

/// Render the `[quote_intake]` section to its canonical TOML form. The
/// bearer token is NEVER rendered here — keychain only.
pub fn to_toml_section(cfg: &QuoteIntakeTomlConfig) -> String {
    let mut out = String::new();
    out.push_str("[quote_intake]\n");
    out.push_str(&format!("enabled = {}\n", cfg.enabled));
    if let Some(url) = &cfg.base_url {
        out.push_str(&format!("base_url = \"{url}\"\n"));
    }
    if let Some(interval) = cfg.poll_interval_secs {
        out.push_str(&format!("poll_interval_secs = {interval}\n"));
    }
    out
}

/// Atomically replace `path`'s `[quote_intake]` section (and only that
/// section). Preserves every other section verbatim — mirrors PR-92's
/// `write_smtp_section` posture.
pub fn write_quote_intake_section(path: &Path, cfg: &QuoteIntakeTomlConfig) -> Result<()> {
    let _ = crate::seller_toml_backup::snapshot_and_rotate(path);

    cfg.validate()
        .map_err(|errs| anyhow!("quote-intake config invariants violated pre-write: {errs:?}"))?;
    let new_section = to_toml_section(cfg);
    let body = if path.exists() {
        let existing = fs::read_to_string(path)
            .with_context(|| format!("read existing seller.toml at {}", path.display()))?;
        merge_quote_intake_section(&existing, &new_section)
    } else {
        new_section
    };
    write_atomic(path, body.as_bytes())
}

/// Replace the `[quote_intake]` section of an existing seller.toml
/// body. Same line-walker shape as `merge_smtp_section` /
/// `merge_numbering_section`.
pub fn merge_quote_intake_section(existing: &str, new_section: &str) -> String {
    let mut prefix = String::new();
    let mut in_section = false;
    for raw_line in existing.lines() {
        let trimmed = raw_line.trim();
        if trimmed.starts_with("[[") && trimmed.ends_with("]]") {
            in_section = false;
            prefix.push_str(raw_line);
            prefix.push('\n');
            continue;
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            let inner = trimmed[1..trimmed.len() - 1].trim();
            in_section = inner == "quote_intake";
            if in_section {
                continue;
            }
            prefix.push_str(raw_line);
            prefix.push('\n');
            continue;
        }
        if in_section {
            continue;
        }
        prefix.push_str(raw_line);
        prefix.push('\n');
    }
    while prefix.ends_with("\n\n") {
        prefix.pop();
    }
    if prefix.is_empty() {
        return new_section.to_string();
    }
    if new_section.is_empty() {
        return prefix;
    }
    if !prefix.ends_with('\n') {
        prefix.push('\n');
    }
    prefix.push('\n');
    prefix.push_str(new_section);
    prefix
}

fn write_atomic(path: &Path, body: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("seller.toml path `{}` has no parent dir", path.display()))?;
    if !parent.exists() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create parent dir {}", parent.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
        }
    }
    let tmp = parent.join(format!(".seller.toml.tmp.{}", std::process::id()));
    let mut f =
        fs::File::create(&tmp).with_context(|| format!("create tempfile {}", tmp.display()))?;
    f.write_all(body)
        .with_context(|| format!("write tempfile {}", tmp.display()))?;
    f.sync_all()
        .with_context(|| format!("fsync tempfile {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
    }
    fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_returns_none_when_section_absent() {
        let body = "[seller]\nlegal_name = \"X\"\n";
        assert_eq!(parse_quote_intake_section(body).unwrap(), None);
    }

    #[test]
    fn parse_round_trips_full_section() {
        let body = "[quote_intake]\nenabled = true\nbase_url = \"http://localhost:3000\"\npoll_interval_secs = 60\n";
        let cfg = parse_quote_intake_section(body).unwrap().unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.base_url.as_deref(), Some("http://localhost:3000"));
        assert_eq!(cfg.poll_interval_secs, Some(60));
    }

    #[test]
    fn parse_defaults_enabled_false_when_omitted() {
        let body = "[quote_intake]\nbase_url = \"http://x\"\n";
        let cfg = parse_quote_intake_section(body).unwrap().unwrap();
        assert!(!cfg.enabled);
    }

    #[test]
    fn parse_unknown_key_silently_skipped() {
        let body = "[quote_intake]\nenabled = true\nbase_url = \"http://x\"\nfuture_key = \"x\"\n";
        let cfg = parse_quote_intake_section(body).unwrap().unwrap();
        assert!(cfg.enabled);
    }

    #[test]
    fn validate_enabled_requires_base_url() {
        let cfg = QuoteIntakeTomlConfig {
            enabled: true,
            base_url: None,
            poll_interval_secs: None,
        };
        let errs = cfg.validate().unwrap_err();
        assert!(errs.iter().any(|e| e.field == "base_url"));
    }

    #[test]
    fn validate_disabled_is_lax() {
        // Disabled config tolerates missing URL — operator may
        // pre-disable while iterating on the form.
        let cfg = QuoteIntakeTomlConfig {
            enabled: false,
            base_url: None,
            poll_interval_secs: None,
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_rejects_bad_url_scheme() {
        let cfg = QuoteIntakeTomlConfig {
            enabled: true,
            base_url: Some("localhost:3000".to_string()),
            poll_interval_secs: None,
        };
        let errs = cfg.validate().unwrap_err();
        assert!(errs.iter().any(|e| e.message.contains("http://")));
    }

    #[test]
    fn validate_rejects_interval_out_of_bounds() {
        for bad in [0, 5, MAX_POLL_INTERVAL_SECS + 1, 100_000] {
            let cfg = QuoteIntakeTomlConfig {
                enabled: true,
                base_url: Some("http://x".to_string()),
                poll_interval_secs: Some(bad),
            };
            let errs = cfg.validate().unwrap_err();
            assert!(
                errs.iter().any(|e| e.field == "poll_interval_secs"),
                "expected poll_interval_secs error for {bad}, got {errs:?}"
            );
        }
    }

    #[test]
    fn merge_replaces_only_quote_intake_section() {
        let existing = "[seller]\nlegal_name = \"X\"\n\n[quote_intake]\nenabled = false\n\n[seller.smtp]\nhost = \"smtp.x\"\nport = 465\n";
        let new_section = "[quote_intake]\nenabled = true\nbase_url = \"http://new\"\n";
        let merged = merge_quote_intake_section(existing, new_section);
        assert!(merged.contains("[seller]"));
        assert!(merged.contains("legal_name = \"X\""));
        assert!(merged.contains("[seller.smtp]"));
        assert!(merged.contains("host = \"smtp.x\""));
        assert!(merged.contains("[quote_intake]"));
        assert!(merged.contains("base_url = \"http://new\""));
        // Old enabled=false line must be gone.
        let enabled_count = merged.matches("enabled").count();
        assert_eq!(enabled_count, 1, "merged body: {merged}");
    }

    #[test]
    fn merge_inserts_when_section_absent() {
        let existing = "[seller]\nlegal_name = \"X\"\n";
        let new_section = "[quote_intake]\nenabled = true\nbase_url = \"http://new\"\n";
        let merged = merge_quote_intake_section(existing, new_section);
        assert!(merged.contains("[seller]"));
        assert!(merged.contains("[quote_intake]"));
        assert!(merged.contains("base_url = \"http://new\""));
    }

    #[test]
    fn merge_preserves_smtp_and_banks() {
        let existing = "[seller]\nlegal_name = \"X\"\n\n[[seller.banks]]\ncurrency = \"HUF\"\naccount_number = \"123\"\nbank_name = \"B\"\nswift_bic = \"S\"\n\n[seller.smtp]\nhost = \"smtp.x\"\nport = 465\n\n[quote_intake]\nenabled = false\n";
        let new_section = "[quote_intake]\nenabled = true\nbase_url = \"http://x\"\n";
        let merged = merge_quote_intake_section(existing, new_section);
        assert!(merged.contains("[[seller.banks]]"));
        assert!(merged.contains("currency = \"HUF\""));
        assert!(merged.contains("[seller.smtp]"));
        assert!(merged.contains("host = \"smtp.x\""));
    }

    #[test]
    fn to_toml_section_omits_token_field() {
        let cfg = QuoteIntakeTomlConfig {
            enabled: true,
            base_url: Some("http://x".to_string()),
            poll_interval_secs: Some(60),
        };
        let s = to_toml_section(&cfg);
        assert!(
            !s.contains("token"),
            "TOML rendering MUST NOT include token: {s}"
        );
        assert!(
            !s.contains("bearer"),
            "TOML rendering MUST NOT include bearer: {s}"
        );
    }
}
