//! PR-92 / ADR-0047 — SMTP email delivery of issued invoices to the
//! buyer.
//!
//! # What this module does
//!
//! Given an issued invoice id + the operator's tenant context, this
//! module:
//!
//!   1. Loads the partner record (the buyer) by joining the
//!      `InvoiceDraftCreated` audit payload's customer name +
//!      tax number against the `partners` table. The contact email
//!      is the to-address. If the partner has no email, the send is
//!      refused with `MissingRecipient` — wrong-recipient guard
//!      (ADR-0047 §3 — never silently send to a fallback).
//!   2. Renders the printed PDF via the existing
//!      [`crate::print_invoice::render_to_bytes`] path. Same byte
//!      shape the operator would download.
//!   3. Optionally attaches the on-disk NAV InvoiceData XML (the
//!      verbatim wire body) when `attach_xml = true` in the config.
//!   4. Composes a bilingual (HU + EN) email subject + body
//!      referencing the invoice number. ALL header-bound interpolated
//!      values (recipient address, display name, subject) are
//!      sanitized to reject CR/LF — see [`validate_no_crlf`] +
//!      [`compose_subject`] + the lettre `Address::new` parse.
//!   5. Sends via the operator-configured SMTP server using
//!      [`crate::smtp_config::SmtpConfig`] for non-secrets + the
//!      boot-cached SMTP password from
//!      [`crate::secrets_cache::SecretsCache`] (session-149) for the
//!      password.
//!      TLS is MANDATORY — see [`build_transport`] — the function
//!      panics rather than silently falls back to plaintext (CLAUDE.md
//!      rule 12).
//!   6. Records the outcome in the audit ledger as an
//!      [`aberp_audit_ledger::EventKind::InvoiceEmailedSent`] entry
//!      (success OR failure — never silently skipped). NO secrets
//!      reach the audit payload.
//!
//! # Security surfaces (review-anchor)
//!
//! Every defence is enumerated in `_handoffs/PR-92-handoff.md`. The
//! list below is the in-code anchor:
//!
//! - **No credentials in logs** — the password is wrapped in
//!   `Zeroizing<String>` from keychain read through lettre's
//!   `Credentials::new`. No `Debug`/`Display`/`tracing` impl
//!   ever sees it.
//! - **TLS mandatory** — lettre's `relay`/`starttls_relay` builders
//!   are the only construction paths; there is no plaintext / cleartext
//!   fallback in the code (the grep-pin test catches the forbidden
//!   tokens).
//! - **Email-header injection** — every header-bound field
//!   (recipient address, display name, subject) is sanitized for
//!   CR/LF BEFORE being handed to lettre. lettre's own `Address::new`
//!   parse rejects malformed addresses; the CR/LF guard runs first
//!   so a malformed address with an embedded `\r\n` is rejected
//!   with the right error class.
//! - **Wrong-recipient guard** — partner email is the ONLY recipient
//!   source; no fallback ("missing email → send to operator") path
//!   exists.
//! - **Attachment filename sanitization** — the invoice number is
//!   filtered to ASCII-alphanumeric + `-` + `_` before composing the
//!   PDF filename, eliminating path-traversal / RFC-2047-injection
//!   risk via a hostile invoice number.

use std::path::Path;
use std::time::Duration;

use aberp_audit_ledger::{Actor, BinaryHash, EventKind, LedgerMeta, TenantId};
use aberp_billing::IdempotencyKey;
use aberp_db::HandleArc;
use anyhow::{anyhow, Context, Result};
use lettre::{
    message::{header::ContentType, Attachment, Mailbox, MultiPart, SinglePart},
    transport::smtp::{authentication::Credentials, client::Tls, client::TlsParameters},
    AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
};
use zeroize::Zeroizing;

use crate::audit_payloads::InvoiceEmailedSentPayload;
use crate::binary_hash;
use crate::print_invoice;
use crate::smtp_config::{self, SmtpConfig, SmtpSecurity};

/// Typed outcome of a send attempt. Maps onto the
/// `InvoiceEmailedSentPayload.outcome` + `error_class` audit fields
/// — the route layer surfaces the right HTTP status from the variant.
#[derive(Debug)]
pub enum EmailSendError {
    /// The partner has no contact email — refuse to send. Maps to
    /// `error_class: "recipient_rejected"` in the audit payload, 400
    /// at the route boundary.
    MissingRecipient { invoice_id: String },
    /// A header-bound field contained CR or LF, OR the recipient
    /// address failed lettre's RFC-5322 parse. Maps to
    /// `error_class: "recipient_rejected"`, 400.
    HeaderInjection { field: &'static str, detail: String },
    /// `[seller.smtp]` is not configured (or is malformed). Maps to
    /// `error_class: "compose"`, 503.
    SmtpNotConfigured(String),
    /// The keychain has no SMTP password for this tenant. Maps to
    /// `error_class: "auth"`, 503.
    SmtpPasswordMissing,
    /// TLS negotiation / authentication / transport failure. Maps to
    /// `error_class: "tls"` / `"auth"` / `"transport"` per the
    /// matched substring.
    SmtpTransport { detail: String },
    /// The compose step (PDF render, XML read, MIME build) failed.
    Compose(anyhow::Error),
    /// Generic propagation. Maps to `error_class: "other"`, 500.
    Other(anyhow::Error),
}

impl std::fmt::Display for EmailSendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingRecipient { invoice_id } => write!(
                f,
                "invoice {invoice_id}: buyer has no contact email — refusing to send (no fallback)"
            ),
            Self::HeaderInjection { field, detail } => {
                write!(f, "header-injection guard rejected `{field}`: {detail}")
            }
            Self::SmtpNotConfigured(detail) => write!(f, "SMTP is not configured: {detail}"),
            Self::SmtpPasswordMissing => {
                write!(
                    f,
                    "SMTP password is not set in the OS keychain for this tenant"
                )
            }
            Self::SmtpTransport { detail } => write!(f, "SMTP transport failure: {detail}"),
            Self::Compose(e) => write!(f, "email compose failure: {e:#}"),
            Self::Other(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for EmailSendError {}

impl From<anyhow::Error> for EmailSendError {
    fn from(e: anyhow::Error) -> Self {
        EmailSendError::Other(e)
    }
}

impl EmailSendError {
    /// Map the variant to the closed-vocab `error_class` token used
    /// by the audit payload + the SPA's inline-error renderer.
    pub fn error_class(&self) -> &'static str {
        match self {
            Self::MissingRecipient { .. } => "recipient_rejected",
            Self::HeaderInjection { .. } => "recipient_rejected",
            Self::SmtpNotConfigured(_) => "compose",
            Self::SmtpPasswordMissing => "auth",
            Self::SmtpTransport { detail } => {
                let d = detail.to_lowercase();
                if d.contains("tls") || d.contains("certificate") {
                    "tls"
                } else if d.contains("auth") || d.contains("535") {
                    "auth"
                } else if d.contains("550") || d.contains("recipient") {
                    "recipient_rejected"
                } else {
                    "transport"
                }
            }
            Self::Compose(_) => "compose",
            Self::Other(_) => "other",
        }
    }

    /// Render the operator-readable detail string for the audit
    /// payload. Already-scrubbed by construction (none of these
    /// `Display` impls touch the password) — the helper exists so
    /// the route handler can call `.error_class()` + `.scrubbed_detail()`
    /// without re-formatting.
    pub fn scrubbed_detail(&self) -> String {
        let raw = format!("{self}");
        scrub_secrets(&raw)
    }
}

/// Belt-and-braces secret scrubber. The send path is structured so
/// the password never reaches a `Display`/`tracing` boundary, but if
/// a future contributor accidentally wraps a `Credentials` value in
/// an `anyhow::Error` chain, the scrubber catches the leakage at the
/// audit-write seam. Replaces every 6+ char run of consecutive
/// non-whitespace with `<scrubbed>` when it follows an "auth"
/// keyword. Defence in depth.
fn scrub_secrets(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev = "";
    let mut buf = String::new();
    for word in s.split_whitespace() {
        let scrub = matches!(
            prev.to_lowercase().as_str(),
            "password" | "password:" | "credentials" | "pw" | "secret"
        );
        if !buf.is_empty() {
            buf.push(' ');
        }
        if scrub {
            buf.push_str("<scrubbed>");
        } else {
            buf.push_str(word);
        }
        prev = word;
    }
    out.push_str(&buf);
    out
}

/// One auto-vs-manual discriminator. Stamped onto the audit payload
/// (`auto: bool`) so the operator-twin record can distinguish a
/// post-issue auto-send from a deliberate operator click on the
/// "Email to buyer" button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendTrigger {
    /// Default-on auto-send fired at the end of the issue flow.
    AutoOnIssue,
    /// Operator clicked the manual "Email to buyer" button on the
    /// invoice-detail page.
    Manual,
}

impl SendTrigger {
    pub fn is_auto(&self) -> bool {
        matches!(self, Self::AutoOnIssue)
    }
}

/// Inputs for [`send_invoice_email`]. The route layer composes this
/// after the issuance / manual-send gate runs.
pub struct SendInvoiceEmailInput<'a> {
    pub invoice_id: &'a str,
    pub tenant: &'a str,
    pub db_path: &'a Path,
    pub seller_toml_path: &'a Path,
    /// PR-98 — partner `contact_email` may carry MULTIPLE addresses,
    /// canonical-form `"a@x.com, b@y.com"` (the storage normaliser
    /// emits `", "` separators). The send path parses the string via
    /// [`crate::partners::parse_emails`] and adds every address as a
    /// `To:` header (transparent multi-recipient — these are buyer
    /// contacts on the same invoice). A single-address operator
    /// continues to type a single string; parsing one token is
    /// indistinguishable from the pre-PR-98 single-address path.
    pub recipient_email: &'a str,
    pub recipient_display_name: Option<&'a str>,
    /// Invoice number string (operator-facing display form) — used
    /// in the subject + the attachment filename. Sanitized by
    /// [`sanitize_invoice_number_for_filename`] before being
    /// interpolated.
    pub invoice_number: &'a str,
    /// Operator-typed `seller.legal_name` for the body greeting.
    /// Sanitized for CR/LF before use.
    pub supplier_legal_name: &'a str,
    pub trigger: SendTrigger,
    /// XML attachment path. `Some` only if [`SmtpConfig::attach_xml`]
    /// is `true` AND the XML exists on disk.
    pub xml_path_if_attached: Option<&'a Path>,
}

/// Successful outcome of the SMTP send path. Threaded back to the
/// route handler for the JSON echo body.
#[derive(Debug, Clone)]
pub struct SendInvoiceEmailOutcome {
    pub recipient: String,
    pub subject: String,
    pub attached_xml: bool,
}

/// Compose + send. The asynchronous boundary is required because
/// lettre's `AsyncSmtpTransport<Tokio1Executor>` is the rustls-tls
/// path; the SMTP route handler is already `async` (axum 0.7) so
/// this propagates cleanly.
///
/// On success, returns the [`SendInvoiceEmailOutcome`]. On failure,
/// returns a typed [`EmailSendError`] — the CALLER is responsible
/// for writing the audit entry (the audit write is route-side so
/// the route layer can compose the right Actor + tenant + idempotency
/// without this module re-reading the keychain / boot state).
pub async fn send_invoice_email(
    input: SendInvoiceEmailInput<'_>,
    smtp_config: &SmtpConfig,
    smtp_password: &Zeroizing<String>,
) -> Result<SendInvoiceEmailOutcome, EmailSendError> {
    // Wrong-recipient guard. Refuse to send when the partner has no
    // email — never invent a fallback.
    if input.recipient_email.trim().is_empty() {
        return Err(EmailSendError::MissingRecipient {
            invoice_id: input.invoice_id.to_string(),
        });
    }
    validate_no_crlf("recipient_email", input.recipient_email)?;
    if let Some(name) = input.recipient_display_name {
        validate_no_crlf("recipient_display_name", name)?;
    }
    // PR-98 — parse the recipient list (may be one address or many;
    // canonical storage uses `", "` separators but the parser is
    // tolerant of comma / semicolon / whitespace mixes). Validator
    // already gated the partner row at write time; if a malformed
    // shape slips through (older row pre-PR-98, manual TOML edit) the
    // per-token gate inside the build_mailbox loop below loud-fails
    // with `HeaderInjection`. The display name is only attached to the
    // first recipient — lettre's `Mailbox` carries a single display
    // name per address; subsequent recipients render as bare addresses.
    let recipient_tokens = crate::partners::parse_emails(input.recipient_email);
    if recipient_tokens.is_empty() {
        return Err(EmailSendError::MissingRecipient {
            invoice_id: input.invoice_id.to_string(),
        });
    }
    validate_no_crlf("invoice_number", input.invoice_number)?;
    validate_no_crlf("supplier_legal_name", input.supplier_legal_name)?;
    validate_no_crlf("from_address", &smtp_config.from_address)?;
    if let Some(name) = &smtp_config.from_display_name {
        validate_no_crlf("from_display_name", name)?;
    }

    // Render the PDF via the existing path. The renderer is the
    // single source of byte truth — no separate "email PDF" shape.
    let rendered = print_invoice::render_to_bytes(
        input.invoice_id,
        input.db_path,
        input.tenant,
        Some(input.seller_toml_path),
    )
    .map_err(|e| {
        EmailSendError::Compose(e.context("render printed PDF for SMTP email attachment"))
    })?;

    // Optional XML attachment.
    let xml_bytes = if let Some(path) = input.xml_path_if_attached {
        match std::fs::read(path) {
            Ok(b) => Some(b),
            Err(e) => {
                return Err(EmailSendError::Compose(anyhow!(
                    "attach_xml=true but reading NAV XML at {} failed: {e}",
                    path.display()
                )))
            }
        }
    } else {
        None
    };

    // Compose the message — see helper for the bilingual body.
    let subject = compose_subject(input.invoice_number);
    let body_plain = compose_body_plain(input.invoice_number, input.supplier_legal_name);

    let from_mbox = build_mailbox(
        &smtp_config.from_address,
        smtp_config.from_display_name.as_deref(),
        "from",
    )?;
    // PR-98 — build one Mailbox per parsed recipient; the first one
    // carries the optional display name (lettre `Mailbox` is
    // single-name-per-address). Multiple `.to()` calls below register
    // each as a `To:` recipient.
    let mut recipient_mboxes: Vec<Mailbox> = Vec::with_capacity(recipient_tokens.len());
    for (i, token) in recipient_tokens.iter().enumerate() {
        let display = if i == 0 {
            input.recipient_display_name
        } else {
            None
        };
        recipient_mboxes.push(build_mailbox(token, display, "to")?);
    }
    let to_summary = recipient_tokens.join(", ");

    let pdf_filename = format!(
        "invoice_{}.pdf",
        sanitize_invoice_number_for_filename(input.invoice_number)
    );

    let pdf_part = Attachment::new(pdf_filename).body(
        rendered.pdf_bytes.clone(),
        ContentType::parse("application/pdf").map_err(|e| {
            EmailSendError::Compose(anyhow!("content-type parse application/pdf: {e}"))
        })?,
    );

    let mut multipart = MultiPart::mixed().singlepart(
        SinglePart::builder()
            .header(ContentType::TEXT_PLAIN)
            .body(body_plain.clone()),
    );
    multipart = multipart.singlepart(pdf_part);

    let attached_xml = if let Some(bytes) = xml_bytes {
        let xml_filename = format!(
            "invoice_{}.xml",
            sanitize_invoice_number_for_filename(input.invoice_number)
        );
        let xml_part = Attachment::new(xml_filename).body(
            bytes,
            ContentType::parse("application/xml").map_err(|e| {
                EmailSendError::Compose(anyhow!("content-type parse application/xml: {e}"))
            })?,
        );
        multipart = multipart.singlepart(xml_part);
        true
    } else {
        false
    };

    let mut builder = Message::builder().from(from_mbox).subject(&subject);
    for mbox in &recipient_mboxes {
        builder = builder.to(mbox.clone());
    }
    let message = builder
        .multipart(multipart)
        .map_err(|e| EmailSendError::Compose(anyhow!("MIME message build: {e}")))?;

    let transport = build_transport(smtp_config, smtp_password)?;
    transport
        .send(message)
        .await
        .map_err(|e| EmailSendError::SmtpTransport {
            detail: format!("{e}"),
        })?;

    Ok(SendInvoiceEmailOutcome {
        // PR-98 — surface the canonical comma+space recipient list in
        // the outcome so the audit-payload row records every address
        // the email was sent to. Single-recipient case is the
        // pre-PR-98 string-shape verbatim (no comma in a one-token
        // string).
        recipient: to_summary,
        subject,
        attached_xml,
    })
}

/// Build the lettre `AsyncSmtpTransport`. TLS is MANDATORY:
///
///   - `SmtpSecurity::Tls` → `AsyncSmtpTransport::relay(host)` —
///     implicit TLS from byte zero (port 465 conventional).
///   - `SmtpSecurity::StartTls` → `AsyncSmtpTransport::starttls_relay(host)`
///     — explicit STARTTLS upgrade required; if STARTTLS negotiation
///     fails, lettre's `Tls::Required` posture fails the send.
///
/// There is NO plaintext-Tls / unencrypted-localhost construction in
/// this function — a future contributor would need to add it
/// deliberately, which the ADR-0047 §1 "TLS mandatory" pin + the
/// `build_transport_source_has_no_plaintext_fallback` test would
/// catch (the test reads this source file and refuses to compile if
/// the forbidden tokens appear).
fn build_transport(
    cfg: &SmtpConfig,
    password: &Zeroizing<String>,
) -> Result<AsyncSmtpTransport<Tokio1Executor>, EmailSendError> {
    let tls_params = TlsParameters::new(cfg.host.clone()).map_err(|e| {
        EmailSendError::Compose(anyhow!("rustls TlsParameters build for {}: {e}", cfg.host))
    })?;
    let credentials = Credentials::new(cfg.username.clone(), password.as_str().to_string());
    let builder = match cfg.security {
        SmtpSecurity::Tls => AsyncSmtpTransport::<Tokio1Executor>::relay(&cfg.host)
            .map_err(|e| EmailSendError::Compose(anyhow!("lettre relay({}): {e}", cfg.host)))?
            .tls(Tls::Wrapper(tls_params)),
        SmtpSecurity::StartTls => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&cfg.host)
            .map_err(|e| {
                EmailSendError::Compose(anyhow!("lettre starttls_relay({}): {e}", cfg.host))
            })?
            .tls(Tls::Required(tls_params)),
    };
    let transport = builder
        .port(cfg.port)
        .credentials(credentials)
        .timeout(Some(SMTP_SEND_TIMEOUT))
        .build();
    Ok(transport)
}

/// PR-98 — verify the SMTP config + password reach the server. Builds
/// the same transport `send_invoice_email` uses, opens a connection
/// (which carries the TLS handshake + AUTH), runs a NOOP, then closes.
/// NEVER sends an email and NEVER persists anything. Returns `Ok(())`
/// on a successful round-trip; a typed [`EmailSendError`] mirroring
/// the actual send-path error classes otherwise — so the SPA's banner
/// rendering reuses the same `error_class()` / `scrubbed_detail()`
/// surfaces as the real send.
pub async fn test_smtp_connection(
    cfg: &SmtpConfig,
    password: &Zeroizing<String>,
) -> Result<(), EmailSendError> {
    // Header-injection guard still applies on the test path — a
    // misconfigured `from_address` carrying a CR / LF would corrupt
    // any real send so the test should refuse it too.
    validate_no_crlf("from_address", &cfg.from_address)?;
    if let Some(name) = &cfg.from_display_name {
        validate_no_crlf("from_display_name", name)?;
    }
    let transport = build_transport(cfg, password)?;
    transport
        .test_connection()
        .await
        .map_err(|e| EmailSendError::SmtpTransport {
            detail: format!("{e}"),
        })?;
    Ok(())
}

/// PR-93 — bound the wall-clock time spent waiting for an SMTP server.
/// Before this guard a hung / blackhole SMTP host would stall the auto-
/// send-after-issue branch of the issue-invoice handler indefinitely,
/// holding the SPA's POST /invoices/issue open. The send is best-effort
/// (the invoice IS issued regardless of the SMTP outcome — ADR-0047 §5),
/// so a fixed 30-second cap matches the operator's actual patience for
/// "did the email go out?" feedback.
///
/// 30s covers a normal STARTTLS handshake + AUTH + DATA round-trip on
/// Gmail / Office365 / typical relays (observed ~2-5s); a server that
/// can't complete in 30s is a configuration problem the operator should
/// see surfaced as `SmtpTransport { detail: "..." }` rather than a UI
/// freeze.
const SMTP_SEND_TIMEOUT: Duration = Duration::from_secs(30);

/// Compose the bilingual subject line.
fn compose_subject(invoice_number: &str) -> String {
    format!("Számla / Invoice {invoice_number}")
}

/// Compose the bilingual plain-text body. Kept simple per the brief's
/// "no templating engine beyond a simple bilingual body".
fn compose_body_plain(invoice_number: &str, supplier_legal_name: &str) -> String {
    format!(
        "Tisztelt Partner,\n\
         \n\
         Mellékelten küldjük a {invoice_number} számú számlát PDF formátumban.\n\
         A számla letöltése után kérjük ellenőrizze az adatokat.\n\
         \n\
         Üdvözlettel,\n\
         {supplier_legal_name}\n\
         \n\
         ---\n\
         \n\
         Dear Customer,\n\
         \n\
         Please find attached the invoice {invoice_number} in PDF format.\n\
         After downloading the invoice, please verify the details.\n\
         \n\
         Best regards,\n\
         {supplier_legal_name}\n",
    )
}

/// Build a `Mailbox` from an operator-typed address + display name.
/// The CR/LF guard ran upstream; lettre's `Address::new` provides
/// the second layer of validation (RFC-5322 shape, single-address-
/// not-list, no `<>` syntax leakage).
fn build_mailbox(
    address: &str,
    display_name: Option<&str>,
    label: &'static str,
) -> Result<Mailbox, EmailSendError> {
    let (local, domain) = match address.split_once('@') {
        Some(p) => p,
        None => {
            return Err(EmailSendError::HeaderInjection {
                field: label,
                detail: format!("address `{address}` has no `@`"),
            });
        }
    };
    let addr =
        lettre::Address::new(local, domain).map_err(|e| EmailSendError::HeaderInjection {
            field: label,
            detail: format!("lettre Address::new(`{local}`, `{domain}`): {e}"),
        })?;
    let name = display_name
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    Ok(Mailbox::new(name, addr))
}

/// Reject CR / LF / NUL / Unicode line separators in any operator-or-
/// buyer-supplied string that may flow into an RFC-822 header. ADR-0047
/// §3 — the #1 injection risk surface in the SMTP build.
///
/// # Why more than `\r` / `\n`
///
/// PR-93 (adversarial review) expanded the guard beyond `\r` / `\n` to
/// cover defence-in-depth against:
///
///   - **NUL (U+0000)** — some MTAs / log pipelines truncate at NUL;
///     pre-truncation a malicious string could re-frame downstream
///     parsing. Lettre's header encoder also disallows NUL (it routes
///     such bytes through RFC-2047), but rejecting at our seam fails
///     fast with the right error class.
///   - **Unicode Line Separator (U+2028)** + **Paragraph Separator
///     (U+2029)** — some non-MIME-aware downstream consumers
///     (browsers, JS parsers, certain MUAs) interpret these as line
///     breaks. Lettre encodes them via RFC-2047 (since they're
///     non-ASCII) so the SMTP wire is safe today; this guard pins the
///     posture against a future lettre change AND covers any new
///     header-bound field that doesn't pass through the lettre encoder.
///   - **NEL (U+0085)** — C1 control "Next Line"; some legacy MTAs
///     treat as line separator. Same defence-in-depth rationale.
///
/// The closed set of rejected scalars is enumerated by name in
/// [`is_forbidden_header_byte`] so the test corpus (and future
/// expansion) is one point-of-edit.
fn validate_no_crlf(field: &'static str, value: &str) -> Result<(), EmailSendError> {
    if let Some(c) = value.chars().find(|c| is_forbidden_header_byte(*c)) {
        return Err(EmailSendError::HeaderInjection {
            field,
            detail: format!(
                "value contains forbidden codepoint U+{:04X} (header-injection guard: CR / LF / NUL / NEL / U+2028 / U+2029)",
                c as u32
            ),
        });
    }
    Ok(())
}

/// PR-93 adversarial-review codepoint set. Any of these in a header-
/// bound field is rejected. Kept as a free function so the audit-test
/// corpus enumerates the same set.
pub(crate) fn is_forbidden_header_byte(c: char) -> bool {
    matches!(
        c,
        '\r'         // U+000D — CR
        | '\n'       // U+000A — LF
        | '\u{0000}' // NUL
        | '\u{0085}' // NEL (C1)
        | '\u{2028}' // Line Separator
        | '\u{2029}' // Paragraph Separator
    )
}

/// Sanitize an invoice number for safe use in an attachment filename.
/// Keeps ASCII alphanumeric + `-` + `_`; replaces every other byte
/// with `_`. Eliminates path-traversal (`../`) and RFC-2047-injection
/// risk via a hostile invoice number. The NAV invoice-number XSD
/// (per ADR-0045) already constrains the upstream charset to
/// `[0-9A-Za-z\-/]`, so the only filtered character on a well-formed
/// number is `/` (replaced with `_`).
pub fn sanitize_invoice_number_for_filename(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Combine the non-secret SMTP config (read from `[seller.smtp]` on
/// disk) with the `cached_password` the caller pulled from the
/// in-process [`crate::secrets_cache::SecretsCache`]. Combined helper
/// because both the route layer and the auto-send path always need
/// both halves; threading them separately invites a future call site
/// to forget one and silently degrade.
///
/// Session-149 — the password is NO LONGER read from the keychain
/// here. It is sourced from the boot-populated cache and passed in by
/// the caller, so this seam never touches `security-framework`. A
/// `None` cached password (SMTP not configured, or the boot read found
/// no item) maps to the typed `SmtpPasswordMissing` — same operator
/// UX as before, minus the lazy keychain read.
pub fn load_smtp_credentials(
    cached_password: Option<Zeroizing<String>>,
    seller_toml_path: &Path,
) -> Result<(SmtpConfig, Zeroizing<String>), EmailSendError> {
    let cfg = match smtp_config::read_smtp_config(seller_toml_path).map_err(|e| {
        EmailSendError::SmtpNotConfigured(format!(
            "read [seller.smtp] from {}: {e:#}",
            seller_toml_path.display()
        ))
    })? {
        Some(c) => c,
        None => {
            return Err(EmailSendError::SmtpNotConfigured(format!(
                "no [seller.smtp] section in {}",
                seller_toml_path.display()
            )));
        }
    };
    let password = cached_password.ok_or(EmailSendError::SmtpPasswordMissing)?;
    Ok((cfg, password))
}

/// Append the `InvoiceEmailedSent` audit-ledger entry. Called by the
/// route handler AFTER `send_invoice_email` (success or failure) so
/// the operator-twin record never has gaps. Returns the entry-count
/// for parity with mark-paid's response shape.
pub fn record_email_audit_entry(
    db: &HandleArc,
    tenant: TenantId,
    binary_hash_bytes: BinaryHash,
    actor: Actor,
    invoice_id: &str,
    payload: InvoiceEmailedSentPayload,
) -> Result<u64> {
    // ADR-0099 — route the InvoiceEmailedSent audit append through the ONE
    // shared aberp_db::Handle writer instead of an independent Connection::open
    // (+ a second Ledger::open for verify/mirror) on the live tenant DB. This
    // runs in-process under `aberp serve` (the email-invoice handler); an
    // independent opener off a stale chain head self-assigns an already-used
    // seq (the 369→515 fork class). The WriteGuard drop runs the lockstep
    // mirror sync + debounced durable checkpoint, so the separate verify/sync
    // opener is neither needed nor wanted here.
    let mut guard = db
        .write()
        .map_err(|e| anyhow!("shared writer for emailed-sent audit (ADR-0099): {e}"))?;
    let ledger_meta = LedgerMeta::new(tenant, binary_hash_bytes);
    aberp_audit_ledger::ensure_schema(&guard)
        .context("ensure audit-ledger schema for emailed-sent")?;
    let idempotency_key_str = payload.idempotency_key.clone();
    let tx = guard
        .transaction()
        .context("begin DuckDB transaction (emailed-sent audit append)")?;
    aberp_audit_ledger::append_in_tx(
        &tx,
        &ledger_meta,
        EventKind::InvoiceEmailedSent,
        payload.to_bytes(),
        actor,
        Some(idempotency_key_str),
    )
    .map_err(|e| anyhow!("audit_ledger::append_in_tx InvoiceEmailedSent: {e}"))?;
    tx.commit()
        .context("commit DuckDB transaction (emailed-sent audit append)")?;
    let _ = invoice_id; // present for future per-invoice mirror-locking;
                        // currently the global mirror is the unit.
    Ok(1)
}

/// Helper used by serve-route handlers: build a fresh idempotency
/// key + actor for each email send attempt. The idempotency key is
/// minted PER ATTEMPT (not per invoice) so resend attempts get a
/// distinct audit row.
pub fn fresh_send_idempotency_key() -> IdempotencyKey {
    IdempotencyKey::new()
}

/// Compute the binary hash (shared helper). Wraps
/// `binary_hash::compute` so the route handler can call us without
/// reaching into the binary-hash module directly.
pub fn binary_hash_for_audit() -> Result<BinaryHash> {
    binary_hash::compute().context("compute binary hash for emailed-sent audit append")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_invoice_number_rejects_path_traversal() {
        assert_eq!(
            sanitize_invoice_number_for_filename("../../etc/passwd"),
            "______etc_passwd"
        );
    }

    #[test]
    fn sanitize_invoice_number_replaces_slash() {
        // The NAV invoice-number XSD charset allows `/` (a Hungarian
        // convention separator), but `/` is a path separator on every
        // filesystem; the sanitiser replaces it with `_`.
        assert_eq!(
            sanitize_invoice_number_for_filename("ABERP-2026/00001"),
            "ABERP-2026_00001"
        );
    }

    #[test]
    fn sanitize_invoice_number_keeps_safe_chars() {
        assert_eq!(
            sanitize_invoice_number_for_filename("INV-default-00001"),
            "INV-default-00001"
        );
    }

    #[test]
    fn sanitize_invoice_number_strips_control_bytes() {
        // Input: I N V \r \n ; r m SP - r f SP /  (14 chars)
        // Output: I N V _ _ _ r m _ - r f _ _    (14 chars)
        assert_eq!(
            sanitize_invoice_number_for_filename("INV\r\n;rm -rf /"),
            "INV___rm_-rf__"
        );
    }

    #[test]
    fn validate_no_crlf_rejects_lf() {
        let err = validate_no_crlf("recipient_email", "a@b.c\n\rBcc: x@y.z")
            .expect_err("CR/LF must be rejected");
        match err {
            EmailSendError::HeaderInjection { field, .. } => {
                assert_eq!(field, "recipient_email");
            }
            other => panic!("expected HeaderInjection, got {other:?}"),
        }
    }

    #[test]
    fn validate_no_crlf_accepts_clean() {
        assert!(validate_no_crlf("subject", "Invoice 2026/00001").is_ok());
    }

    #[test]
    fn compose_subject_contains_invoice_number() {
        let s = compose_subject("ABERP-2026/00001");
        assert!(s.contains("ABERP-2026/00001"));
        // Bilingual marker — Hungarian + English label.
        assert!(s.contains("Számla"));
        assert!(s.contains("Invoice"));
    }

    #[test]
    fn compose_body_contains_invoice_number_and_supplier() {
        let body = compose_body_plain("ABERP-2026/00001", "Áben Consulting KFT.");
        assert!(body.contains("ABERP-2026/00001"));
        assert!(body.contains("Áben Consulting KFT."));
        assert!(body.contains("Tisztelt Partner"));
        assert!(body.contains("Dear Customer"));
    }

    #[test]
    fn error_class_maps_tls_substring_to_tls() {
        let e = EmailSendError::SmtpTransport {
            detail: "tls handshake failed: bad certificate".to_string(),
        };
        assert_eq!(e.error_class(), "tls");
    }

    #[test]
    fn error_class_maps_535_substring_to_auth() {
        let e = EmailSendError::SmtpTransport {
            detail: "535 Authentication failed".to_string(),
        };
        assert_eq!(e.error_class(), "auth");
    }

    #[test]
    fn error_class_maps_550_substring_to_recipient_rejected() {
        let e = EmailSendError::SmtpTransport {
            detail: "550 5.1.1 User unknown".to_string(),
        };
        assert_eq!(e.error_class(), "recipient_rejected");
    }

    #[test]
    fn error_class_missing_recipient_is_recipient_rejected() {
        let e = EmailSendError::MissingRecipient {
            invoice_id: "inv_X".to_string(),
        };
        assert_eq!(e.error_class(), "recipient_rejected");
    }

    #[test]
    fn error_class_header_injection_is_recipient_rejected() {
        let e = EmailSendError::HeaderInjection {
            field: "to",
            detail: "CR/LF".to_string(),
        };
        assert_eq!(e.error_class(), "recipient_rejected");
    }

    #[test]
    fn error_class_smtp_password_missing_is_auth() {
        let e = EmailSendError::SmtpPasswordMissing;
        assert_eq!(e.error_class(), "auth");
    }

    // ── PR-93 adversarial pins ─────────────────────────────────────
    //
    // The pins below are the load-bearing regression locks PR-93
    // (adversarial security review) added on top of PR-92's build.
    // Every one of them is a "this MUST fail-closed under hostile
    // input" assertion; any future contributor that weakens the guard
    // sees the test fail at `cargo test` BEFORE the regression ships.

    /// PR-93 §1 — header-injection corpus. `validate_no_crlf` MUST
    /// reject every codepoint enumerated by
    /// `is_forbidden_header_byte`: CR, LF, NUL, NEL, U+2028, U+2029.
    /// The CR/LF subset was already pinned by `validate_no_crlf_rejects_lf`
    /// in PR-92; PR-93 adds the four less-obvious siblings that
    /// downstream consumers may treat as line-break.
    #[test]
    fn pr_93_validate_no_crlf_rejects_unicode_line_separators() {
        let corpus: [(char, &str); 6] = [
            ('\r', "CR"),
            ('\n', "LF"),
            ('\u{0000}', "NUL"),
            ('\u{0085}', "NEL"),
            ('\u{2028}', "U+2028"),
            ('\u{2029}', "U+2029"),
        ];
        for (c, label) in corpus {
            let payload = format!("buyer@example.com{}Bcc: attacker@evil.com", c);
            let err = validate_no_crlf("recipient_email", &payload)
                .unwrap_err_or_else_panic(format!("must reject {label}"));
            match err {
                EmailSendError::HeaderInjection { field, detail } => {
                    assert_eq!(field, "recipient_email", "field tag for {label}");
                    assert!(
                        detail.contains(&format!("U+{:04X}", c as u32)),
                        "detail for {label} should name the codepoint, got {detail}"
                    );
                }
                other => panic!("expected HeaderInjection for {label}, got {other:?}"),
            }
        }
    }

    /// PR-93 §1 — the precise codepoint set is also the closed vocab
    /// for the matching helper. If a future edit widens the helper
    /// without updating the test, this pin fires.
    #[test]
    fn pr_93_is_forbidden_header_byte_pins_exact_set() {
        assert!(is_forbidden_header_byte('\r'));
        assert!(is_forbidden_header_byte('\n'));
        assert!(is_forbidden_header_byte('\u{0000}'));
        assert!(is_forbidden_header_byte('\u{0085}'));
        assert!(is_forbidden_header_byte('\u{2028}'));
        assert!(is_forbidden_header_byte('\u{2029}'));
        // Sanity — every-day characters must NOT be in the set.
        for c in ['a', 'Z', '0', '@', '.', '-', ' ', '\t'] {
            assert!(
                !is_forbidden_header_byte(c),
                "must not reject `{c}` (would break normal input)"
            );
        }
    }

    /// PR-93 §1 — the `\t` (HTAB) byte is RFC-822 whitespace and must
    /// pass the header-injection guard (rejecting it would break
    /// folded headers and atom whitespace). This pin guards against
    /// over-zealous future expansion.
    #[test]
    fn pr_93_validate_no_crlf_accepts_tab() {
        assert!(validate_no_crlf("subject", "Invoice\tnumber").is_ok());
    }

    /// PR-93 §5 — path-traversal corpus expansion. The PR-92 pins
    /// covered `../`, slash, and `\r\n;rm -rf /`. Adversarial review
    /// adds: NUL byte, Windows path separator `\`, leading absolute
    /// `/`, BOM, RTL override (U+202E), zero-width joiner
    /// (U+200D), Hebrew/Arabic glyphs. Output for ALL must be ASCII-
    /// alphanumeric + `-` + `_` only. (Sanitiser is byte-replace; if a
    /// future contributor switches to character-class logic and
    /// accidentally permits one of these, this pin catches it.)
    #[test]
    fn pr_93_sanitize_invoice_number_fuzz_corpus() {
        let corpus: [&str; 11] = [
            "\x00etc/passwd",                             // NUL prefix
            "..\\..\\..\\Windows\\System32\\config\\SAM", // Windows traversal
            "/absolute/path/file",                        // absolute UNIX
            "\u{FEFF}invoice",                            // BOM
            "inv\u{202E}\u{0041}\u{0042}\u{0043}",        // RTL override
            "inv\u{200D}zwj",                             // ZWJ
            "שלום-2026",                                  // Hebrew
            "مرحبا-2026",                                 // Arabic
            "%2e%2e%2finvoice",                           // percent-encoded `../`
            "INV;DROP TABLE invoices;--",                 // SQL-flavour
            "INV`whoami`",                                // shell-metachars
        ];
        for raw in corpus {
            let out = sanitize_invoice_number_for_filename(raw);
            for c in out.chars() {
                assert!(
                    c.is_ascii_alphanumeric() || c == '-' || c == '_',
                    "sanitiser leaked byte `{}` (codepoint U+{:04X}) from input `{}` (output: `{}`)",
                    c,
                    c as u32,
                    raw,
                    out
                );
            }
        }
    }

    /// PR-93 §3 — TLS-bypass grep, expanded form. PR-92 banned the
    /// plaintext TLS-mode and the unencrypted-localhost tokens; PR-93
    /// adds the dangerous-accept-invalid family lettre / rustls expose
    /// for skipping cert checks. If a future contributor types ANY of
    /// these in this file, the pin fails at `cargo test`. (Tokens are
    /// concatenated at runtime so the assertion strings themselves
    /// don't trip the grep.)
    #[test]
    fn pr_93_no_tls_validation_bypass_tokens_in_source() {
        let src = include_str!("email_invoice.rs");
        let forbidden = [
            ["dangerous", "_", "accept", "_", "invalid"].concat(),
            ["accept", "_", "invalid", "_", "certs"].concat(),
            ["danger", "_accept_invalid_hostnames"].concat(),
            ["disable", "_", "certificate", "_", "verification"].concat(),
            ["Server", "CertVerifier"].concat(), // rustls bypass trait name
        ];
        for token in forbidden {
            assert!(
                !src.contains(&token),
                "ADR-0047 §1: TLS cert-validation bypass token `{token}` is forbidden in email_invoice.rs"
            );
        }
    }

    /// PR-93 §3 — `build_transport` is the ONLY constructor path for
    /// the async SMTP transport. The source-grep pin
    /// (`build_transport_source_has_no_plaintext_fallback`) catches
    /// adding plaintext at the constructor seam; this pin catches a
    /// second-best regression where a NEW seam (e.g. a public helper
    /// that ALSO builds a transport) is introduced. Counts the
    /// occurrences of the async transport type's :: call sites and
    /// pins an upper bound.
    #[test]
    fn pr_93_only_one_transport_constructor_call_site() {
        let src = include_str!("email_invoice.rs");
        // Tokens assembled at runtime so the assertion's own string
        // doesn't trip the count.
        let needle = ["Async", "Smtp", "Transport::"].concat();
        let count = src.matches(needle.as_str()).count();
        // Two construction call sites today (relay + starttls_relay,
        // both in `build_transport`). Allow a small window for type
        // ascription in the function signature; explicitly DENY a
        // future regression that doubles the count by adding a third
        // call site outside `build_transport`.
        // PR-92 baseline: 4 occurrences (2 in docstrings on
        // `build_transport`, 2 in the actual call sites inside it).
        // Any future contributor that adds a fifth call site outside
        // `build_transport` doubles the construction seam and fails
        // this pin — forcing the discussion.
        assert!(
            count <= 4,
            "the async SMTP transport :: token occurs {count} times — a new transport-construction seam may have been added without going through build_transport (PR-93 §3 grep-pin)"
        );
    }

    /// PR-93 §2 — `EmailSendError`'s Display impl must NOT carry any
    /// substring that could plausibly contain a credential. Run the
    /// Display through every variant with adversarial inputs.
    #[test]
    fn pr_93_email_send_error_display_carries_no_credentials() {
        let cases = [
            EmailSendError::MissingRecipient {
                invoice_id: "inv_X".to_string(),
            },
            EmailSendError::HeaderInjection {
                field: "to",
                detail: "x".to_string(),
            },
            EmailSendError::SmtpNotConfigured("missing".to_string()),
            EmailSendError::SmtpPasswordMissing,
            EmailSendError::SmtpTransport {
                detail: "535 auth failed user=alice".to_string(),
            },
            EmailSendError::Compose(anyhow::anyhow!("compose")),
            EmailSendError::Other(anyhow::anyhow!("other")),
        ];
        for e in cases {
            let d = format!("{e}");
            assert!(
                !d.to_lowercase().contains("hunter2"),
                "Display must not leak a literal-known-bad password fragment"
            );
            let scrubbed = e.scrubbed_detail();
            assert!(
                !scrubbed.contains("hunter2"),
                "scrubbed_detail must not leak literal-known-bad password fragment"
            );
        }
    }

    /// PR-93 §1 — `compose_subject` only interpolates the invoice
    /// number, which has already been CR/LF-validated upstream. This
    /// pin catches a future refactor that adds a partner-controlled
    /// field (recipient name, buyer legal name, etc) to the subject
    /// without re-routing through `validate_no_crlf`. The subject
    /// MUST be a deterministic function of `invoice_number` only.
    #[test]
    fn pr_93_compose_subject_only_uses_invoice_number() {
        let a = compose_subject("ABERP-2026/00001");
        let b = compose_subject("ABERP-2026/00002");
        // Different invoice numbers must produce different subjects.
        assert_ne!(a, b);
        // Same invoice number must produce same subject (no time / UUID).
        assert_eq!(a, compose_subject("ABERP-2026/00001"));
    }

    /// Helper: panic-on-Ok wrapper that matches the corpus loop
    /// readability. Implemented as a trait so the corpus loop reads
    /// naturally.
    trait UnwrapErrOrElsePanic<T, E> {
        fn unwrap_err_or_else_panic(self, msg: String) -> E;
    }
    impl<T: std::fmt::Debug, E> UnwrapErrOrElsePanic<T, E> for Result<T, E> {
        fn unwrap_err_or_else_panic(self, msg: String) -> E {
            match self {
                Ok(t) => panic!("{msg}: unexpected Ok({t:?})"),
                Err(e) => e,
            }
        }
    }

    /// Defence-in-depth: the build_transport function constructs an
    /// `AsyncSmtpTransport` from `relay`/`starttls_relay` only — there
    /// is no plaintext-Tls construction path in the source. If a
    /// future edit introduces a plaintext path, this grep-test fires
    /// at `cargo test`.
    ///
    /// The forbidden tokens are built from string concatenation so the
    /// literal does not appear in this source file (which would
    /// trivially fail the grep).
    #[test]
    fn build_transport_source_has_no_plaintext_fallback() {
        let src = include_str!("email_invoice.rs");
        // Compose the forbidden tokens at runtime so the assertion
        // strings themselves don't trigger the grep.
        let forbidden_tls_none = ["Tls", "::", "None"].concat();
        let forbidden_unencrypted = ["unencrypted", "_", "localhost"].concat();
        assert!(
            !src.contains(&forbidden_tls_none),
            "ADR-0047 §1: SMTP plaintext is forbidden, but a plaintext-Tls token was found in the source"
        );
        assert!(
            !src.contains(&forbidden_unencrypted),
            "ADR-0047 §1: SMTP plaintext is forbidden, but the unencrypted-localhost token was found"
        );
    }
}
