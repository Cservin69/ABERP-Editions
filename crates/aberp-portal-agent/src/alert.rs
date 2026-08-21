//! Where a canary alert goes — the SMTP single point of contact.
//!
//! # Why the alert is sent from the Mac
//!
//! The probe is seen on the VPS, but the mail is sent from here.
//! ADR-0115 §2.4 makes "no authentication material at rest on the VPS"
//! absolute and ADR-0047 makes the OS keychain the only home for the
//! SMTP password; a relay that could send mail would need a credential
//! that a relay compromise would then hand over. The poll loop already
//! exists and already runs relay → agent, so the canary rides it and
//! the credential never leaves the machine it belongs to.
//!
//! Cost of that choice, stated: while Leg B is down, no alert can
//! be sent. The front keeps a bounded in-memory backlog and flushes on
//! reconnect. A relay that is *hostile* rather than merely offline can
//! suppress canaries entirely — inherent, since a hostile relay can
//! drop any frame, and not something a mailer on the VPS would fix.
//!
//! # "Single SMTP SPOC" — what is single, and what is not yet
//!
//! Single in **configuration and policy**: this reads the same
//! `[seller.smtp]` section of the same `seller.toml`, the same
//! `aberp.smtp.<tenant>` keychain entry, the same closed-vocab
//! `security` field with no plaintext variant, and builds the transport
//! with the same `Tls::Wrapper` / `Tls::Required` posture as
//! `apps/aberp/src/email_invoice.rs::build_transport`.
//!
//! Not yet single in **code**. The agent cannot link `apps/aberp` — that
//! would drag DuckDB, NAV and Tauri into a daemon whose entire purpose
//! is to keep running when all of them are stopped (§2.2). Unifying the
//! two properly means extracting `smtp_config.rs` plus the transport
//! builder into a shared crate, which edits the frozen invoice
//! application. That is a real should-fix, and it is deliberately not
//! taken unilaterally inside a portal build. Until it is, the ADR-0047
//! guarantee is held here by [`tests::transport_source_has_no_plaintext_fallback`],
//! the same source-scanning pin the original site uses.

use std::path::{Path, PathBuf};

use lettre::message::{Mailbox, Message};
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};
use zeroize::Zeroizing;

use crate::config::SecretSource;

/// Send timeout. An alert that cannot be delivered promptly is retried
/// on the next batch rather than holding the agent's task.
const SMTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum AlertError {
    #[error("no `[seller.smtp]` section in {path} — the SMTP SPOC is not configured")]
    NotConfigured { path: PathBuf },
    #[error("reading {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("`security = \"{got}\"` is not in the closed vocab. Allowed: `StartTls`, `Tls`. (Plaintext SMTP is not configurable — TLS is mandatory.)")]
    BadSecurity { got: String },
    #[error("`{field}` is missing from the `[seller.smtp]` section")]
    MissingField { field: &'static str },
    #[error("reading the SMTP password: {0}")]
    Password(#[from] crate::config::SecretError),
    #[error("composing the alert: {0}")]
    Compose(String),
    #[error("SMTP transport: {0}")]
    Transport(String),
    #[error("a header value contained a forbidden control byte")]
    HeaderInjection,
}

/// Transport security. The closed vocab of ADR-0047 §1, mirrored:
/// there is no plaintext variant, so no operator-typed configuration
/// can produce a plaintext send path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmtpSecurity {
    /// STARTTLS upgrade, required — never falls back.
    StartTls,
    /// Implicit TLS from byte zero.
    Tls,
}

impl SmtpSecurity {
    fn from_token(s: &str) -> Result<Self, AlertError> {
        match s {
            "StartTls" => Ok(Self::StartTls),
            "Tls" => Ok(Self::Tls),
            other => Err(AlertError::BadSecurity {
                got: other.to_string(),
            }),
        }
    }
}

/// The non-secret half of the SPOC, read from `seller.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpocConfig {
    pub host: String,
    pub port: u16,
    pub from_address: String,
    pub from_display_name: Option<String>,
    pub username: String,
    pub security: SmtpSecurity,
}

/// Parse the `[seller.smtp]` section out of a `seller.toml` body.
///
/// Hand-rolled line-walker matching the style of
/// `apps/aberp/src/smtp_config.rs::parse_smtp_section` — the same
/// reasoning applies (no `toml` crate floor for six fields), and it
/// keeps this daemon's dependency surface narrow.
pub fn parse_spoc_section(body: &str) -> Result<Option<SpocConfig>, AlertError> {
    let mut in_section = false;
    let mut fields: Vec<(String, String)> = Vec::new();
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_section = trimmed == "[seller.smtp]";
            continue;
        }
        if !in_section || trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        let value = value.trim();
        // Strip an inline comment outside of a quoted value.
        let value = if value.starts_with('"') {
            value
                .strip_prefix('"')
                .and_then(|v| v.split('"').next())
                .unwrap_or("")
                .to_string()
        } else {
            value.split('#').next().unwrap_or("").trim().to_string()
        };
        fields.push((key.trim().to_string(), value));
    }
    if !fields.iter().any(|(k, _)| k == "host") {
        return Ok(None);
    }
    let get = |name: &'static str| -> Result<String, AlertError> {
        fields
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .filter(|v| !v.is_empty())
            .ok_or(AlertError::MissingField { field: name })
    };
    Ok(Some(SpocConfig {
        host: get("host")?,
        port: get("port")?
            .parse()
            .map_err(|_| AlertError::MissingField { field: "port" })?,
        from_address: get("from_address")?,
        from_display_name: fields
            .iter()
            .find(|(k, _)| k == "from_display_name")
            .map(|(_, v)| v.clone())
            .filter(|v| !v.is_empty()),
        username: get("username")?,
        security: SmtpSecurity::from_token(&get("security")?)?,
    }))
}

/// Where an alert is delivered.
#[derive(Debug, Clone)]
pub enum AlertSink {
    /// Production: the SMTP SPOC.
    Spoc {
        /// `~/.aberp-defense/<tenant>/seller.toml`.
        seller_toml: PathBuf,
        /// The keychain entry holding the SMTP password.
        password: SecretSource,
        /// Recipient. Defaults to the SPOC's own `from_address`.
        to: Option<String>,
    },
    /// Dev and test: append the alert to a file. No network, no
    /// keychain, no real secrets — the DEV keychain-bypass rule.
    File(PathBuf),
    /// Alerting off. The probe log is still written; only the mail is
    /// suppressed. Loud in the daemon log so this cannot be the
    /// accidental state.
    Disabled,
}

impl AlertSink {
    /// Deliver one alert.
    pub async fn send(&self, subject: &str, body: &str) -> Result<(), AlertError> {
        match self {
            Self::Disabled => {
                tracing::warn!(subject, "canary alerting is DISABLED — alert not sent");
                Ok(())
            }
            Self::File(path) => {
                use std::io::Write as _;
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|source| AlertError::Io {
                        path: parent.to_path_buf(),
                        source,
                    })?;
                }
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .map_err(|source| AlertError::Io {
                        path: path.clone(),
                        source,
                    })?;
                writeln!(f, "Subject: {subject}\n{body}\n---").map_err(|source| AlertError::Io {
                    path: path.clone(),
                    source,
                })
            }
            Self::Spoc {
                seller_toml,
                password,
                to,
            } => {
                let cfg = read_spoc(seller_toml)?;
                let password = Zeroizing::new(password.read()?);
                let to = to.clone().unwrap_or_else(|| cfg.from_address.clone());
                send_via_spoc(&cfg, &password, &to, subject, body).await
            }
        }
    }

    /// A one-word label for the daemon log.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Spoc { .. } => "smtp-spoc",
            Self::File(_) => "file",
            Self::Disabled => "disabled",
        }
    }
}

fn read_spoc(path: &Path) -> Result<SpocConfig, AlertError> {
    let body = std::fs::read_to_string(path).map_err(|source| AlertError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    parse_spoc_section(&body)?.ok_or(AlertError::NotConfigured {
        path: path.to_path_buf(),
    })
}

/// Reject CR / LF / NUL / NEL / U+2028 / U+2029 in anything that lands
/// in an RFC-822 header — the same closed set the invoice mailer
/// guards, for the same reason. Doubly load-bearing here: the alert's
/// content is derived from probe metadata, which is attacker-supplied.
/// (`aberp_portal_core::canary::sanitise` already strips these at the
/// front; this is the second belt, at the seam that would be exploited.)
fn validate_no_control(value: &str) -> Result<(), AlertError> {
    if value.chars().any(|c| {
        c == '\r' || c == '\n' || c == '\0' || c == '\u{85}' || c == '\u{2028}' || c == '\u{2029}'
    }) {
        return Err(AlertError::HeaderInjection);
    }
    Ok(())
}

async fn send_via_spoc(
    cfg: &SpocConfig,
    password: &Zeroizing<String>,
    to: &str,
    subject: &str,
    body: &str,
) -> Result<(), AlertError> {
    validate_no_control(subject)?;
    validate_no_control(to)?;
    validate_no_control(&cfg.from_address)?;
    if let Some(name) = &cfg.from_display_name {
        validate_no_control(name)?;
    }

    let from: Mailbox = match &cfg.from_display_name {
        Some(name) => format!("{name} <{}>", cfg.from_address),
        None => cfg.from_address.clone(),
    }
    .parse()
    .map_err(|e| AlertError::Compose(format!("from address: {e}")))?;
    let to: Mailbox = to
        .parse()
        .map_err(|e| AlertError::Compose(format!("to address: {e}")))?;

    let message = Message::builder()
        .from(from)
        .to(to)
        .subject(subject)
        .body(body.to_string())
        .map_err(|e| AlertError::Compose(e.to_string()))?;

    let transport = build_transport(cfg, password)?;
    transport
        .send(message)
        .await
        .map_err(|e| AlertError::Transport(e.to_string()))?;
    Ok(())
}

/// Build the lettre transport. TLS is MANDATORY, mirroring
/// `apps/aberp/src/email_invoice.rs::build_transport`:
///
///   - `SmtpSecurity::Tls` → implicit TLS from byte zero;
///   - `SmtpSecurity::StartTls` → required STARTTLS upgrade, and if
///     negotiation fails the send fails.
///
/// There is no plaintext construction in this function. A contributor
/// would have to add one deliberately, which
/// `tests::transport_source_has_no_plaintext_fallback` catches by
/// reading this source file — the same pin ADR-0047 §1 put on the
/// original site.
fn build_transport(
    cfg: &SpocConfig,
    password: &Zeroizing<String>,
) -> Result<AsyncSmtpTransport<Tokio1Executor>, AlertError> {
    let tls_params = TlsParameters::new(cfg.host.clone())
        .map_err(|e| AlertError::Compose(format!("rustls TlsParameters for {}: {e}", cfg.host)))?;
    let credentials = Credentials::new(cfg.username.clone(), password.as_str().to_string());
    let builder = match cfg.security {
        SmtpSecurity::Tls => AsyncSmtpTransport::<Tokio1Executor>::relay(&cfg.host)
            .map_err(|e| AlertError::Compose(format!("lettre relay({}): {e}", cfg.host)))?
            .tls(Tls::Wrapper(tls_params)),
        SmtpSecurity::StartTls => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&cfg.host)
            .map_err(|e| AlertError::Compose(format!("lettre starttls_relay({}): {e}", cfg.host)))?
            .tls(Tls::Required(tls_params)),
    };
    Ok(builder
        .port(cfg.port)
        .credentials(credentials)
        .timeout(Some(SMTP_TIMEOUT))
        .build())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SELLER_TOML: &str = r#"
[seller]
name = "Example Kft."

[seller.smtp]
host = "smtp.example.test"
port = 465
from_address = "noreply@example.test"
from_display_name = "Example Kft."
username = "noreply@example.test"
security = "Tls"
attach_xml = false

[seller.numbering]
prefix = "INV"
"#;

    #[test]
    fn parses_the_spoc_section_without_touching_its_neighbours() {
        let cfg = parse_spoc_section(SELLER_TOML)
            .expect("parses")
            .expect("section present");
        assert_eq!(cfg.host, "smtp.example.test");
        assert_eq!(cfg.port, 465);
        assert_eq!(cfg.from_address, "noreply@example.test");
        assert_eq!(cfg.from_display_name.as_deref(), Some("Example Kft."));
        assert_eq!(cfg.username, "noreply@example.test");
        assert_eq!(cfg.security, SmtpSecurity::Tls);
    }

    #[test]
    fn no_section_is_none_not_an_error() {
        // A tenant that has never configured mail is a normal state;
        // the canary then logs and says so, rather than failing.
        assert!(parse_spoc_section("[seller]\nname = \"x\"\n")
            .expect("parses")
            .is_none());
    }

    #[test]
    fn a_plaintext_security_token_is_refused_loudly() {
        // ADR-0047 §1's closed vocab. This is the line that makes
        // "TLS is mandatory" a property of the config parser rather
        // than a convention.
        let body = SELLER_TOML.replace(r#"security = "Tls""#, r#"security = "plaintext""#);
        assert!(matches!(
            parse_spoc_section(&body),
            Err(AlertError::BadSecurity { .. })
        ));
        let body = SELLER_TOML.replace(r#"security = "Tls""#, r#"security = "None""#);
        assert!(matches!(
            parse_spoc_section(&body),
            Err(AlertError::BadSecurity { .. })
        ));
    }

    #[test]
    fn starttls_parses_and_is_the_other_permitted_value() {
        let body = SELLER_TOML.replace(r#"security = "Tls""#, r#"security = "StartTls""#);
        let cfg = parse_spoc_section(&body).expect("parses").expect("present");
        assert_eq!(cfg.security, SmtpSecurity::StartTls);
    }

    #[test]
    fn a_missing_required_field_is_named() {
        let body = SELLER_TOML.replace("username = \"noreply@example.test\"\n", "");
        assert!(matches!(
            parse_spoc_section(&body),
            Err(AlertError::MissingField { field: "username" })
        ));
    }

    #[test]
    fn transport_source_has_no_plaintext_fallback() {
        // The same pin `apps/aberp/src/email_invoice.rs` carries, on
        // this second transport site. Tokens are composed at runtime so
        // the assertion strings do not trip their own grep.
        let src = include_str!("alert.rs");
        let forbidden_tls_none = ["Tls", "::", "None"].concat();
        let forbidden_unencrypted = ["unencrypted", "_", "localhost"].concat();
        assert!(
            !src.contains(&forbidden_tls_none),
            "ADR-0047 §1: SMTP plaintext is forbidden, but a plaintext-Tls token was found"
        );
        assert!(
            !src.contains(&forbidden_unencrypted),
            "ADR-0047 §1: SMTP plaintext is forbidden, but the unencrypted-localhost token was found"
        );
    }

    #[test]
    fn only_one_transport_constructor_site_exists_here() {
        // Mirrors the app's `pr_93_only_one_transport_constructor_call_site`:
        // catches a NEW seam that also builds a transport, which the
        // token scan above would not see.
        let src = include_str!("alert.rs");
        let ctor = ["AsyncSmtpTransport", "::<", "Tokio1Executor", ">::"].concat();
        assert_eq!(
            src.matches(&ctor).count(),
            2,
            "expected exactly the two constructor calls inside build_transport"
        );
    }

    #[test]
    fn header_injection_is_refused() {
        assert!(matches!(
            validate_no_control("subject\r\nBcc: attacker@evil.test"),
            Err(AlertError::HeaderInjection)
        ));
        assert!(validate_no_control("an ordinary subject").is_ok());
        for c in ['\u{85}', '\u{2028}', '\u{2029}', '\0'] {
            assert!(
                validate_no_control(&format!("a{c}b")).is_err(),
                "{c:?} allowed"
            );
        }
    }

    #[tokio::test]
    async fn the_file_sink_writes_the_alert_and_touches_nothing_else() {
        let dir = std::env::temp_dir().join(format!("portal-alert-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("alerts.log");
        let sink = AlertSink::File(path.clone());
        sink.send("subj", "body").await.expect("send");
        sink.send("subj2", "body2").await.expect("send");
        let body = std::fs::read_to_string(&path).expect("read");
        assert!(body.contains("Subject: subj\nbody"));
        assert!(body.contains("Subject: subj2\nbody2"));
        assert_eq!(sink.label(), "file");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn the_disabled_sink_is_a_no_op() {
        AlertSink::Disabled.send("s", "b").await.expect("no-op");
    }
}
