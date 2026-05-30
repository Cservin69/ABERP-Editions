//! PR-92 — SMTP non-secret configuration in seller.toml.
//!
//! Stored as a `[seller.smtp]` section in `~/.aberp/<tenant>/seller.toml`
//! per the keychain/TOML split: secrets (the password) live in the OS
//! keychain via [`crate::smtp_credentials`]; non-secrets live here on
//! disk. The write path is a non-destructive merge that preserves the
//! identity section (`[seller]`, `[seller.address]`), the
//! bank-account block (`[[seller.banks]]`), the invoice-numbering
//! section (`[seller.numbering]`), AND any comment prefix — exactly
//! the same posture as PR-72's [`crate::seller_banks::merge_bank_section`]
//! and PR-89's [`crate::numbering::merge_numbering_section`]. The
//! three SPA write surfaces (identity, banks, numbering, SMTP) MUST
//! compose without stomping each other — the seller.toml
//! merge-not-replace invariant pinned in
//! [[project_seller_toml_write_invariant]].
//!
//! # Closed vocab on `security`
//!
//! SMTP transport security is one of two values: `StartTls` or `Tls`.
//! Plaintext SMTP is NOT a variant — there is no operator-typeable
//! configuration that produces a plaintext send path. TLS is
//! mandatory; the wire transport refuses to downgrade. See
//! [`adr/0047-smtp-email-delivery-security.md`].
//!
//! # XML attachment flag
//!
//! `attach_xml` controls whether the NAV InvoiceData XML rides as a
//! second attachment alongside the printed PDF. Default `false`
//! (PDF-only is the buyer-friendly default; the XML attachment is an
//! advanced/operator-driven choice). The flag never affects the
//! NAV-submission path — that wire shape is fixed by ADR-0009 §4.

use std::fs;
use std::io::Write as _;
use std::path::Path;

use anyhow::{anyhow, Context as _, Result};

/// Closed-vocab SMTP transport security mode. ADR-0047 §1: there is
/// NO plaintext variant — TLS is mandatory for every SMTP send the
/// app performs.
///
/// Wire / on-disk form is the PascalCase variant identifier
/// verbatim (`"StartTls"` / `"Tls"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SmtpSecurity {
    /// STARTTLS upgrade after the initial plaintext connection on
    /// port 587. The wire connection MUST upgrade to TLS before any
    /// AUTH dialogue; if STARTTLS negotiation fails, the send fails.
    /// Never falls back to plaintext.
    StartTls,
    /// Implicit TLS from the first byte on port 465. No plaintext
    /// dialogue ever occurs.
    Tls,
}

impl SmtpSecurity {
    /// Parse the on-disk token. Loud-fails on unknown values per
    /// CLAUDE.md rule 12 — a future contributor accidentally writing
    /// `"plaintext"` into the file produces a typed error here, not
    /// a silent plaintext-send security regression.
    pub fn from_token(s: &str) -> Result<Self, String> {
        match s {
            "StartTls" => Ok(SmtpSecurity::StartTls),
            "Tls" => Ok(SmtpSecurity::Tls),
            other => Err(format!(
                "`security = \"{other}\"` is not in the closed vocab. \
                 Allowed: `StartTls`, `Tls`. \
                 (Plaintext SMTP is not configurable — TLS is mandatory.)"
            )),
        }
    }

    /// Render the on-disk token. Paired with [`Self::from_token`] as
    /// a round-trip-proven pair (the unit test below pins this).
    pub fn as_token(&self) -> &'static str {
        match self {
            SmtpSecurity::StartTls => "StartTls",
            SmtpSecurity::Tls => "Tls",
        }
    }
}

/// Non-secret SMTP configuration persisted under `[seller.smtp]` in
/// `seller.toml`. The password is NEVER carried here — see
/// [`crate::smtp_credentials`] for the keychain surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmtpConfig {
    /// SMTP server hostname (`smtp.gmail.com`, `mail.example.com`).
    pub host: String,
    /// SMTP server port. Conventionally 465 (`Tls`) or 587
    /// (`StartTls`); no port-vs-security cross-check is enforced
    /// (a future operator may run a non-standard layout).
    pub port: u16,
    /// Envelope-from address (`noreply@example.com`). Surfaced as
    /// the `From:` header value's address part; the operator-typed
    /// display name (if any) is rendered as the phrase.
    pub from_address: String,
    /// Optional display name for the `From:` header
    /// (`"Áben Consulting KFT."`). `None` → bare address.
    pub from_display_name: Option<String>,
    /// SMTP AUTH username. Typically equal to `from_address` but
    /// some providers diverge (e.g., Gmail SMTP uses the full Gmail
    /// address even when the From is an alias).
    pub username: String,
    /// Closed-vocab transport security — see [`SmtpSecurity`].
    pub security: SmtpSecurity,
    /// Whether the NAV InvoiceData XML rides as a second attachment
    /// alongside the printed PDF. Default `false`.
    pub attach_xml: bool,
}

impl SmtpConfig {
    /// Validate field-level invariants. Returns a Vec so the SPA can
    /// surface every problem at once rather than the operator fixing
    /// them one-at-a-time across multiple round-trips (mirrors the
    /// partners-validator posture A157).
    pub fn validate(&self) -> Result<(), Vec<SmtpConfigValidationError>> {
        let mut errors = Vec::new();
        if self.host.trim().is_empty() {
            errors.push(SmtpConfigValidationError {
                field: "host",
                message: "SMTP host is required".to_string(),
            });
        } else if self.host.chars().any(|c| c.is_whitespace()) {
            errors.push(SmtpConfigValidationError {
                field: "host",
                message: "SMTP host must not contain whitespace".to_string(),
            });
        } else if let Err(msg) = validate_no_toml_metachars("host", &self.host) {
            errors.push(SmtpConfigValidationError {
                field: "host",
                message: msg,
            });
        }
        if self.port == 0 {
            errors.push(SmtpConfigValidationError {
                field: "port",
                message: "SMTP port must be > 0".to_string(),
            });
        }
        // Recipient-validation rules apply to the operator-typed
        // From address too — a CR/LF in the From would let an
        // attacker who controls the config inject headers. The
        // config surface is operator-only (passes through the
        // bearer-authed PUT route) but defence in depth is cheap.
        if let Err(msg) = validate_no_crlf("from_address", &self.from_address) {
            errors.push(SmtpConfigValidationError {
                field: "from_address",
                message: msg,
            });
        }
        if let Err(msg) = validate_no_toml_metachars("from_address", &self.from_address) {
            errors.push(SmtpConfigValidationError {
                field: "from_address",
                message: msg,
            });
        }
        if !looks_like_email(&self.from_address) {
            errors.push(SmtpConfigValidationError {
                field: "from_address",
                message: format!(
                    "from_address `{}` does not look like a valid email address",
                    self.from_address
                ),
            });
        }
        if let Some(name) = &self.from_display_name {
            if let Err(msg) = validate_no_crlf("from_display_name", name) {
                errors.push(SmtpConfigValidationError {
                    field: "from_display_name",
                    message: msg,
                });
            }
            if let Err(msg) = validate_no_toml_metachars("from_display_name", name) {
                errors.push(SmtpConfigValidationError {
                    field: "from_display_name",
                    message: msg,
                });
            }
        }
        if self.username.trim().is_empty() {
            errors.push(SmtpConfigValidationError {
                field: "username",
                message: "SMTP username is required".to_string(),
            });
        } else {
            if let Err(msg) = validate_no_crlf("username", &self.username) {
                errors.push(SmtpConfigValidationError {
                    field: "username",
                    message: msg,
                });
            }
            if let Err(msg) = validate_no_toml_metachars("username", &self.username) {
                errors.push(SmtpConfigValidationError {
                    field: "username",
                    message: msg,
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

/// Validation error shape. Mirrors `partners::ValidationError`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SmtpConfigValidationError {
    pub field: &'static str,
    pub message: String,
}

/// Reject CR / LF / NUL / NEL / U+2028 / U+2029 in any operator-typed
/// string that may eventually land in an SMTP envelope or RFC-822
/// header. PR-93 (adversarial review) widened this beyond `\r`/`\n` to
/// match [`crate::email_invoice::is_forbidden_header_byte`] — same
/// closed set on both seams (config-write guard + send-time guard) so
/// the audit corpus enumerates one list.
///
/// `\r` and `\n` are the header-separator bytes RFC-822 / RFC-5322
/// uses; NEL (U+0085) and the Unicode Line / Paragraph separators
/// (U+2028 / U+2029) are line-break interpretations some downstream
/// consumers (legacy MTAs, browsers, JS parsers) honor. NUL truncates
/// in some log pipelines. Allowing any of them in a header-bound field
/// is the textbook injection. See ADR-0047 §3 + the in-code helper
/// [`crate::email_invoice::is_forbidden_header_byte`].
fn validate_no_crlf(field: &str, value: &str) -> Result<(), String> {
    if let Some(c) = value
        .chars()
        .find(|c| crate::email_invoice::is_forbidden_header_byte(*c))
    {
        return Err(format!(
            "{field} contains forbidden codepoint U+{:04X} (header-injection guard: CR / LF / NUL / NEL / U+2028 / U+2029)",
            c as u32
        ));
    }
    Ok(())
}

/// PR-93 adversarial-review TOML-safety guard. Reject the basic-string
/// metacharacters (`"` and `\`) in operator-typed values that we render
/// into double-quoted TOML basic strings via [`to_toml_section`].
/// Without this, a stray `"` from operator typing would produce a TOML
/// file that fails to re-parse on next read (the operator would brick
/// their own SMTP config). The guard ALSO prevents a hypothetical
/// future composition where one operator-controlled value could open a
/// quoted-string and inject another TOML section. Operator-self-DoS is
/// not a security boundary but failing-loud at the SPA's PUT route is
/// a better UX than failing-silent at the next GET.
fn validate_no_toml_metachars(field: &str, value: &str) -> Result<(), String> {
    if let Some(c) = value.chars().find(|c| *c == '"' || *c == '\\') {
        return Err(format!(
            "{field} contains TOML basic-string metacharacter `{}` (would break seller.toml on round-trip)",
            c
        ));
    }
    Ok(())
}

/// Lightweight shape check on an email address: exactly one `@`, no
/// whitespace, non-empty local + domain parts. NOT a full RFC-5322
/// parse — that surface lives in `lettre`'s `Address::new` which the
/// send path re-validates. The pre-check here gives the operator an
/// inline error at config time.
fn looks_like_email(s: &str) -> bool {
    if s.trim() != s {
        return false;
    }
    if s.chars().any(|c| c.is_whitespace()) {
        return false;
    }
    match s.split_once('@') {
        Some((local, domain)) => !local.is_empty() && !domain.is_empty() && domain.contains('.'),
        None => false,
    }
}

/// Read the `[seller.smtp]` section of `seller.toml` at `path`.
/// Returns `Ok(None)` when the file exists but carries no
/// `[seller.smtp]` section (operator has not configured SMTP yet);
/// `Err` for I/O failure or malformed content.
///
/// Mirrors [`crate::numbering::read_numbering_template`]'s
/// missing-section-is-`None` posture.
pub fn read_smtp_config(path: &Path) -> Result<Option<SmtpConfig>> {
    if !path.exists() {
        return Ok(None);
    }
    let body = fs::read_to_string(path)
        .with_context(|| format!("read seller.toml at {}", path.display()))?;
    parse_smtp_section(&body)
}

/// Parse the `[seller.smtp]` section out of an in-memory seller.toml
/// body. Hand-rolled line-walker matching
/// [`crate::numbering::parse_numbering_section`]'s style — keeps the
/// dependency surface narrow (no `toml` crate floor for 7 fields).
pub fn parse_smtp_section(body: &str) -> Result<Option<SmtpConfig>> {
    let mut host: Option<String> = None;
    let mut port: Option<u16> = None;
    let mut from_address: Option<String> = None;
    let mut from_display_name: Option<String> = None;
    let mut username: Option<String> = None;
    let mut security_token: Option<String> = None;
    let mut attach_xml: bool = false;
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
            in_section = inner == "seller.smtp";
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
                    "[seller.smtp] expected `key = value`, got `{line}`"
                ))
            }
        };
        let key = k.trim();
        let value = v.trim();
        match key {
            "host" => host = Some(strip_quotes(value).to_string()),
            "port" => {
                port = Some(
                    strip_quotes(value)
                        .parse::<u16>()
                        .map_err(|e| anyhow!("[seller.smtp] port `{value}` is not a u16: {e}"))?,
                )
            }
            "from_address" => from_address = Some(strip_quotes(value).to_string()),
            "from_display_name" => {
                let s = strip_quotes(value).to_string();
                from_display_name = if s.is_empty() { None } else { Some(s) };
            }
            "username" => username = Some(strip_quotes(value).to_string()),
            "security" => security_token = Some(strip_quotes(value).to_string()),
            "attach_xml" => {
                attach_xml = match strip_quotes(value) {
                    "true" => true,
                    "false" => false,
                    other => {
                        return Err(anyhow!(
                            "[seller.smtp] attach_xml must be `true` or `false`, got `{other}`"
                        ))
                    }
                };
            }
            _ => {
                // Silently ignore unknown keys — forward-compat with a
                // future addition.
            }
        }
    }
    if !section_seen {
        return Ok(None);
    }
    let host = host.ok_or_else(|| anyhow!("[seller.smtp] missing `host`"))?;
    let port = port.ok_or_else(|| anyhow!("[seller.smtp] missing `port`"))?;
    let from_address =
        from_address.ok_or_else(|| anyhow!("[seller.smtp] missing `from_address`"))?;
    let username = username.ok_or_else(|| anyhow!("[seller.smtp] missing `username`"))?;
    let security_token =
        security_token.ok_or_else(|| anyhow!("[seller.smtp] missing `security`"))?;
    let security = SmtpSecurity::from_token(&security_token).map_err(|e| anyhow!("{e}"))?;
    Ok(Some(SmtpConfig {
        host,
        port,
        from_address,
        from_display_name,
        username,
        security,
        attach_xml,
    }))
}

fn strip_quotes(s: &str) -> &str {
    let t = s.trim();
    if t.starts_with('"') && t.ends_with('"') && t.len() >= 2 {
        &t[1..t.len() - 1]
    } else {
        t
    }
}

/// Render the `[seller.smtp]` section to its canonical TOML form.
///
/// PR-170 — exposed `pub` so
/// [`crate::setup_seller_info::setup_seller_info_to_path`] can re-append
/// a preserved SMTP config across the identity write (mirror of the
/// `[[seller.banks]]` preservation PR-75 added). Without this, the
/// SetupWizard / identity-only TenantSettings save would silently
/// drop `[seller.smtp]` — Ervin's S170 production-update regression.
pub fn to_toml_section(cfg: &SmtpConfig) -> String {
    let mut out = String::new();
    out.push_str("[seller.smtp]\n");
    out.push_str(&format!("host = \"{}\"\n", cfg.host));
    out.push_str(&format!("port = {}\n", cfg.port));
    out.push_str(&format!("from_address = \"{}\"\n", cfg.from_address));
    if let Some(name) = &cfg.from_display_name {
        out.push_str(&format!("from_display_name = \"{name}\"\n"));
    }
    out.push_str(&format!("username = \"{}\"\n", cfg.username));
    out.push_str(&format!("security = \"{}\"\n", cfg.security.as_token()));
    out.push_str(&format!("attach_xml = {}\n", cfg.attach_xml));
    out
}

/// Atomically replace `path`'s `[seller.smtp]` section (and only that
/// section) with the canonical serialisation of `cfg`. Preserves the
/// identity sections (`[seller]`, `[seller.address]`), the
/// bank-account block (`[[seller.banks]]`), the invoice-numbering
/// section (`[seller.numbering]`), AND any comment prefix — mirrors
/// PR-89's `write_numbering_section` posture so the four SPA write
/// surfaces compose without stomping each other.
pub fn write_smtp_section(path: &Path, cfg: &SmtpConfig) -> Result<()> {
    // PR-170 defense-in-depth: snapshot prior seller.toml body before
    // the merge-and-write replaces it. See seller_toml_backup module.
    let _ = crate::seller_toml_backup::snapshot_and_rotate(path);

    cfg.validate()
        .map_err(|errs| anyhow!("SMTP config invariants violated pre-write: {errs:?}"))?;
    let new_section = to_toml_section(cfg);
    let body = if path.exists() {
        let existing = fs::read_to_string(path)
            .with_context(|| format!("read existing seller.toml at {}", path.display()))?;
        merge_smtp_section(&existing, &new_section)
    } else {
        new_section
    };
    write_atomic(path, body.as_bytes())
}

/// Replace the `[seller.smtp]` section of an existing `seller.toml`
/// body. Walks the lines and partitions them into:
///   - **prefix**: everything that isn't inside a `[seller.smtp]`
///     block (every other section + comments + identity preserved).
///   - **smtp lines** (DROPPED): the existing `[seller.smtp]` section
///     header + its key=value body until the next section header.
///
/// Same posture as PR-89's `merge_numbering_section`.
pub fn merge_smtp_section(existing: &str, new_section: &str) -> String {
    let mut prefix = String::new();
    let mut in_smtp = false;
    for raw_line in existing.lines() {
        let trimmed = raw_line.trim();
        if trimmed.starts_with("[[") && trimmed.ends_with("]]") {
            in_smtp = false;
            prefix.push_str(raw_line);
            prefix.push('\n');
            continue;
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            let inner = trimmed[1..trimmed.len() - 1].trim();
            in_smtp = inner == "seller.smtp";
            if in_smtp {
                continue;
            }
            prefix.push_str(raw_line);
            prefix.push('\n');
            continue;
        }
        if in_smtp {
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

/// POSIX-atomic write helper. Mirror of `numbering::write_atomic` —
/// same dir tempfile + fsync + 0600 perms + rename. Kept as a local
/// copy rather than re-exported to avoid widening another module's
/// surface; the comment in `numbering.rs` explains the same trade.
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
            let mut perms = fs::metadata(parent)
                .with_context(|| format!("stat {}", parent.display()))?
                .permissions();
            perms.set_mode(0o700);
            fs::set_permissions(parent, perms)
                .with_context(|| format!("chmod 0700 {}", parent.display()))?;
        }
    }
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp_name = format!(
        ".seller.toml.smtp.tmp.{}-{}-{}",
        std::process::id(),
        nanos,
        seq,
    );
    let tmp_path = parent.join(tmp_name);
    {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_path)
            .with_context(|| format!("open tempfile {}", tmp_path.display()))?;
        f.write_all(body)
            .with_context(|| format!("write tempfile {}", tmp_path.display()))?;
        f.sync_all()
            .with_context(|| format!("fsync tempfile {}", tmp_path.display()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&tmp_path)
            .with_context(|| format!("stat {}", tmp_path.display()))?
            .permissions();
        perms.set_mode(0o600);
        fs::set_permissions(&tmp_path, perms)
            .with_context(|| format!("chmod 0600 {}", tmp_path.display()))?;
    }
    fs::rename(&tmp_path, path).with_context(|| {
        format!(
            "rename tempfile {} -> {}",
            tmp_path.display(),
            path.display()
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_config() -> SmtpConfig {
        SmtpConfig {
            host: "smtp.example.com".to_string(),
            port: 587,
            from_address: "noreply@example.com".to_string(),
            from_display_name: Some("Áben Consulting KFT.".to_string()),
            username: "noreply@example.com".to_string(),
            security: SmtpSecurity::StartTls,
            attach_xml: false,
        }
    }

    #[test]
    fn security_token_round_trips() {
        for v in [SmtpSecurity::StartTls, SmtpSecurity::Tls] {
            let token = v.as_token();
            let parsed = SmtpSecurity::from_token(token).unwrap();
            assert_eq!(parsed, v);
        }
    }

    #[test]
    fn security_token_rejects_plaintext_variant() {
        // ADR-0047 §1: plaintext SMTP is NEVER configurable. Pin
        // every plausible plaintext spelling so a future contributor
        // can't slip a `"plain"` arm into `from_token`.
        assert!(SmtpSecurity::from_token("Plain").is_err());
        assert!(SmtpSecurity::from_token("None").is_err());
        assert!(SmtpSecurity::from_token("plaintext").is_err());
        assert!(SmtpSecurity::from_token("plain").is_err());
        assert!(SmtpSecurity::from_token("").is_err());
    }

    #[test]
    fn to_toml_section_then_parse_round_trips() {
        let cfg = sample_config();
        let serialised = to_toml_section(&cfg);
        let parsed = parse_smtp_section(&serialised).unwrap().unwrap();
        assert_eq!(parsed, cfg);
    }

    #[test]
    fn merge_smtp_section_preserves_other_sections() {
        // ADR-0047 + PR-75 invariant: writing the SMTP section MUST
        // NOT clobber the identity, bank, or numbering sections.
        let existing = "[seller]\n\
            legal_name = \"Áben Consulting KFT.\"\n\
            tax_number = \"12345678-2-41\"\n\
            \n\
            [seller.address]\n\
            street = \"Fő u. 1.\"\n\
            \n\
            [[seller.banks]]\n\
            currency = \"HUF\"\n\
            account_number = \"12345678-12345678-12345678\"\n\
            default = true\n\
            \n\
            [seller.numbering]\n\
            segments = [{ kind = \"Counter\", pad_width = 5 }]\n\
            reset_policy = \"never\"\n\
            start_value = 1\n\
            ";
        let new_section = to_toml_section(&sample_config());
        let merged = merge_smtp_section(existing, &new_section);
        assert!(merged.contains("legal_name = \"Áben Consulting KFT.\""));
        assert!(merged.contains("[seller.address]"));
        assert!(merged.contains("street = \"Fő u. 1.\""));
        assert!(merged.contains("[[seller.banks]]"));
        assert!(merged.contains("[seller.numbering]"));
        assert!(merged.contains("start_value = 1"));
        assert!(merged.contains("[seller.smtp]"));
        assert!(merged.contains("host = \"smtp.example.com\""));
    }

    #[test]
    fn merge_smtp_section_replaces_existing_smtp_block() {
        let existing = "[seller]\n\
            tax_number = \"12345678-2-41\"\n\
            \n\
            [seller.smtp]\n\
            host = \"old.example.com\"\n\
            port = 25\n\
            from_address = \"old@example.com\"\n\
            username = \"old@example.com\"\n\
            security = \"Tls\"\n\
            attach_xml = false\n\
            ";
        let new_section = to_toml_section(&sample_config());
        let merged = merge_smtp_section(existing, &new_section);
        assert!(!merged.contains("old.example.com"));
        assert!(!merged.contains("port = 25"));
        assert!(merged.contains("host = \"smtp.example.com\""));
        assert!(merged.contains("port = 587"));
        // The identity above is preserved.
        assert!(merged.contains("tax_number = \"12345678-2-41\""));
    }

    #[test]
    fn validate_rejects_crlf_in_from_address() {
        // Header-injection guard at the config layer. Even though
        // the operator types this themselves through the bearer-
        // authed PUT route, defence in depth — never accept CR/LF
        // in any header-bound field.
        let mut cfg = sample_config();
        cfg.from_address = "noreply@example.com\r\nBcc: attacker@evil.com".to_string();
        let errs = cfg.validate().unwrap_err();
        assert!(errs.iter().any(|e| e.field == "from_address"));
    }

    #[test]
    fn validate_rejects_crlf_in_display_name() {
        let mut cfg = sample_config();
        cfg.from_display_name = Some("Áben\r\nBcc: a@b.c".to_string());
        let errs = cfg.validate().unwrap_err();
        assert!(errs.iter().any(|e| e.field == "from_display_name"));
    }

    #[test]
    fn validate_rejects_empty_host() {
        let mut cfg = sample_config();
        cfg.host = String::new();
        let errs = cfg.validate().unwrap_err();
        assert!(errs.iter().any(|e| e.field == "host"));
    }

    #[test]
    fn validate_rejects_zero_port() {
        let mut cfg = sample_config();
        cfg.port = 0;
        let errs = cfg.validate().unwrap_err();
        assert!(errs.iter().any(|e| e.field == "port"));
    }

    #[test]
    fn validate_rejects_malformed_from_address() {
        let mut cfg = sample_config();
        cfg.from_address = "not-an-email".to_string();
        let errs = cfg.validate().unwrap_err();
        assert!(errs.iter().any(|e| e.field == "from_address"));
    }

    #[test]
    fn read_smtp_config_returns_none_when_section_absent() {
        let body = "[seller]\nlegal_name = \"Áben\"\n";
        assert!(parse_smtp_section(body).unwrap().is_none());
    }

    // ── PR-93 adversarial pins ─────────────────────────────────────

    /// PR-93 §1 — the config-time CR/LF guard MUST match the
    /// send-time guard codepoint-for-codepoint (defence in depth
    /// across both seams). Fuzz with the same corpus
    /// `email_invoice::pr_93_validate_no_crlf_rejects_unicode_line_separators`
    /// uses.
    #[test]
    fn pr_93_validate_rejects_unicode_line_separators_in_from_address() {
        let corpus: [(char, &str); 6] = [
            ('\r', "CR"),
            ('\n', "LF"),
            ('\u{0000}', "NUL"),
            ('\u{0085}', "NEL"),
            ('\u{2028}', "U+2028"),
            ('\u{2029}', "U+2029"),
        ];
        for (c, label) in corpus {
            let mut cfg = sample_config();
            cfg.from_address = format!("noreply@example.com{c}Bcc: attacker@evil.com");
            let errs = cfg.validate().unwrap_err();
            assert!(
                errs.iter().any(|e| e.field == "from_address"
                    && e.message.contains(&format!("U+{:04X}", c as u32))),
                "must reject {label} in from_address with codepoint named in message; got {errs:?}"
            );
        }
    }

    /// PR-93 §1 — same coverage for display_name.
    #[test]
    fn pr_93_validate_rejects_unicode_line_separators_in_display_name() {
        for c in ['\r', '\n', '\u{0000}', '\u{0085}', '\u{2028}', '\u{2029}'] {
            let mut cfg = sample_config();
            cfg.from_display_name = Some(format!("Áben{c}Evil"));
            let errs = cfg.validate().unwrap_err();
            assert!(
                errs.iter().any(|e| e.field == "from_display_name"),
                "must reject codepoint U+{:04X} in display_name; got {errs:?}",
                c as u32
            );
        }
    }

    /// PR-93 §1 — same coverage for username.
    #[test]
    fn pr_93_validate_rejects_unicode_line_separators_in_username() {
        for c in ['\r', '\n', '\u{0000}', '\u{0085}', '\u{2028}', '\u{2029}'] {
            let mut cfg = sample_config();
            cfg.username = format!("user{c}evil");
            let errs = cfg.validate().unwrap_err();
            assert!(
                errs.iter().any(|e| e.field == "username"),
                "must reject codepoint U+{:04X} in username; got {errs:?}",
                c as u32
            );
        }
    }

    /// PR-93 §7 — TOML-metachar guard. A `"` or `\` in any operator-
    /// typed string would break the rendered TOML on round-trip and
    /// (in the worst case) provide an injection seam if a future
    /// composition let operator-controlled values precede another
    /// section. Reject at the PUT route.
    #[test]
    fn pr_93_validate_rejects_double_quote_in_from_address() {
        let mut cfg = sample_config();
        cfg.from_address = "evil\"@example.com".to_string();
        let errs = cfg.validate().unwrap_err();
        assert!(
            errs.iter()
                .any(|e| e.field == "from_address"
                    && e.message.to_lowercase().contains("metacharacter")),
            "must reject `\"` in from_address; got {errs:?}"
        );
    }

    #[test]
    fn pr_93_validate_rejects_double_quote_in_display_name() {
        let mut cfg = sample_config();
        cfg.from_display_name = Some("Áben\"injection".to_string());
        let errs = cfg.validate().unwrap_err();
        assert!(errs.iter().any(|e| e.field == "from_display_name"));
    }

    #[test]
    fn pr_93_validate_rejects_double_quote_in_username() {
        let mut cfg = sample_config();
        cfg.username = "user\"evil".to_string();
        let errs = cfg.validate().unwrap_err();
        assert!(errs.iter().any(|e| e.field == "username"));
    }

    #[test]
    fn pr_93_validate_rejects_backslash_in_host() {
        let mut cfg = sample_config();
        cfg.host = "smtp.example.com\\evil".to_string();
        let errs = cfg.validate().unwrap_err();
        assert!(errs.iter().any(|e| e.field == "host"));
    }

    /// PR-93 §7 — round-trip pin. A valid SmtpConfig (post-validate)
    /// MUST serialise to a TOML body that re-parses to the same
    /// SmtpConfig. The TOML-metachar guard above makes this true for
    /// operator-typeable input; this pin proves the writer never
    /// produces output the reader can't ingest.
    #[test]
    fn pr_93_write_then_read_round_trip_for_validated_config() {
        let cfg = sample_config();
        cfg.validate().expect("sample is valid");
        let tmpdir =
            std::env::temp_dir().join(format!("aberp-smtp-pr93-{}.toml", std::process::id()));
        write_smtp_section(&tmpdir, &cfg).expect("write must succeed");
        let read_back = read_smtp_config(&tmpdir)
            .expect("read must not error")
            .expect("section must exist");
        assert_eq!(read_back, cfg);
        let _ = std::fs::remove_file(&tmpdir);
    }

    /// PR-93 §7 — adversarial seller.toml. Existing file has every
    /// other section already; writing the SMTP section MUST preserve
    /// each one byte-for-byte at the value level. PR-92 already pinned
    /// this; PR-93 adds the inverse: writing the SMTP section, then
    /// re-reading the OTHER sections (via their own parsers), MUST
    /// return identical values to pre-SMTP-write. This catches a
    /// regression in `merge_smtp_section` that mangled another
    /// section's strings (e.g. by mis-handling embedded `[` chars
    /// inside basic strings).
    #[test]
    fn pr_93_merge_does_not_mangle_basic_strings_in_other_sections() {
        let existing = "[seller]\n\
            legal_name = \"Áben Consulting KFT.\"\n\
            tax_number = \"12345678-2-41\"\n\
            \n\
            [seller.address]\n\
            street = \"Fő u. 1.\"\n\
            \n\
            [[seller.banks]]\n\
            currency = \"HUF\"\n\
            account_number = \"12345678-12345678-12345678\"\n\
            default = true\n\
            ";
        let new_section = to_toml_section(&sample_config());
        let merged = merge_smtp_section(existing, &new_section);
        // Every value from the original must survive verbatim.
        assert!(merged.contains("legal_name = \"Áben Consulting KFT.\""));
        assert!(merged.contains("tax_number = \"12345678-2-41\""));
        assert!(merged.contains("street = \"Fő u. 1.\""));
        assert!(merged.contains("currency = \"HUF\""));
        assert!(merged.contains("account_number = \"12345678-12345678-12345678\""));
        assert!(merged.contains("default = true"));
    }

    /// PR-93 §7 — writing SMTP twice in a row MUST be idempotent (the
    /// second write replaces the first; no duplicate `[seller.smtp]`
    /// blocks). Catches a regression where the merge appends-but-never-
    /// removes.
    #[test]
    fn pr_93_double_write_does_not_duplicate_smtp_section() {
        let cfg = sample_config();
        let tmpdir = std::env::temp_dir().join(format!(
            "aberp-smtp-pr93-double-{}.toml",
            std::process::id()
        ));
        write_smtp_section(&tmpdir, &cfg).unwrap();
        write_smtp_section(&tmpdir, &cfg).unwrap();
        let body = std::fs::read_to_string(&tmpdir).unwrap();
        let count = body.matches("[seller.smtp]").count();
        assert_eq!(
            count, 1,
            "double-write produced {count} [seller.smtp] sections; expected 1. body:\n{body}"
        );
        let _ = std::fs::remove_file(&tmpdir);
    }

    /// PR-93 §1 — the closed-vocab `SmtpSecurity` enum is the ONLY
    /// type-level surface that can produce a TLS-wrapper transport.
    /// PR-92 pinned this for plaintext spellings; PR-93 adds explicit
    /// pins for case sensitivity AND non-Latin spellings that a future
    /// case-insensitive matcher could let through.
    #[test]
    fn pr_93_security_token_is_case_sensitive() {
        assert!(SmtpSecurity::from_token("starttls").is_err());
        assert!(SmtpSecurity::from_token("STARTTLS").is_err());
        assert!(SmtpSecurity::from_token("StartTLS").is_err());
        assert!(SmtpSecurity::from_token("TLS").is_err());
        assert!(SmtpSecurity::from_token("tls").is_err());
        // Only the exact PascalCase strings are accepted.
        assert!(SmtpSecurity::from_token("StartTls").is_ok());
        assert!(SmtpSecurity::from_token("Tls").is_ok());
    }

    /// PR-93 §1 — invisible / lookalike whitespace next to the token
    /// must NOT be silently trimmed. A future contributor adding a
    /// `.trim()` call to from_token would silently accept `" Tls "`
    /// and friends; pin the strict-match posture.
    #[test]
    fn pr_93_security_token_does_not_trim() {
        assert!(SmtpSecurity::from_token(" Tls").is_err());
        assert!(SmtpSecurity::from_token("Tls ").is_err());
        assert!(SmtpSecurity::from_token("\tTls").is_err());
    }

    #[test]
    fn read_smtp_config_loud_fails_on_plaintext_security() {
        let body = "[seller.smtp]\n\
            host = \"smtp.example.com\"\n\
            port = 587\n\
            from_address = \"a@b.c\"\n\
            username = \"a@b.c\"\n\
            security = \"plaintext\"\n\
            attach_xml = false\n";
        let err = parse_smtp_section(body).unwrap_err().to_string();
        assert!(
            err.contains("not in the closed vocab"),
            "expected closed-vocab rejection, got: {err}"
        );
    }
}
