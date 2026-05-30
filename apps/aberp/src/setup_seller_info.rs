//! `POST /api/setup-seller-info` shared core — operator-typed seller
//! identity (legal name, tax number, address) + bank info persisted to
//! `~/.aberp/<tenant>/seller.toml`.
//!
//! # PR-51 / session-71 — closes the "no terminal ever required"
//! milestone for the issuance pipeline.
//!
//! Pre-PR-51 an operator had to `cp samples/seller.toml.example
//! ~/.aberp/<tenant>/seller.toml` then edit it by hand. PR-51 routes
//! the same writes through a SPA SetupWizard mirroring the PR-46α NAV-
//! credentials wizard: missing-config detected at boot → wizard fires
//! → operator types the fields → wizard POSTs → backend writes the
//! file atomically + transitions the boot state to Ready.
//!
//! # Scope discipline (PR-51)
//!
//! - The `seller.toml` schema is NOT changed — the wizard writes the
//!   same flat-key shape `print_invoice::parse_seller_toml` already
//!   reads. Identity fields (`legal_name`, `tax_number`,
//!   `eu_vat_number`, address.*) are documented in
//!   `samples/seller.toml.example` and parsed here; the bank fields
//!   (`bank_account_number`, `iban`, `bank_name`, `swift_bic`) remain
//!   the PDF-renderer's existing surface and round-trip cleanly.
//!
//! - Validation reuses [`nav_xml::parse_hungarian_tax_number`] (the
//!   session-70 shape parser) — the canonical `xxxxxxxx-y-zz` ADÓSZÁM
//!   form is enforced ONCE at the persistence boundary, not duplicated
//!   in both the route and the wizard composer.
//!
//! - The write path is atomic (write to tempfile in the same dir,
//!   `fsync` + rename) with `0600` permissions on POSIX. The tenant
//!   directory is auto-created with `0700` if missing.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context as _, Result};

use crate::nav_xml::{parse_hungarian_tax_number, SupplierConfigError};

/// PR-51 / session-71 — operator-typed seller-config form, mirror of
/// the SPA's `seller-config.ts::SellerConfigForm`. Snake_case wire
/// names so the HTTP route can deserialise the request body straight
/// into this struct.
///
/// Bank fields are `Option<String>` because the existing
/// `SellerToml` shape treats them all as optional (the PDF renderer
/// hides empty rows). Identity fields are required by the wizard's
/// validator — empty after trim is a hard 400 — but the inner type is
/// `String` to make that contract loud at the struct boundary.
#[derive(Debug, Clone)]
pub struct SellerInfoInputs {
    pub legal_name: String,
    pub tax_number: String,
    pub eu_vat_number: Option<String>,
    pub address_country_code: String,
    pub address_postal_code: String,
    pub address_city: String,
    pub address_street: String,
    pub bank_account_number: Option<String>,
    pub iban: Option<String>,
    pub bank_name: Option<String>,
    pub swift_bic: Option<String>,
}

/// PR-51 / session-71 — typed validation failure, one variant per
/// failing field so the SPA can render a field-level inline error
/// instead of a generic banner. `Backend` covers the atomic-write I/O
/// failure (parent-dir mkdir, tempfile write, rename, chmod).
#[derive(Debug)]
pub enum SetupSellerInfoError {
    /// One or more identity fields failed validation. Map is keyed by
    /// the form field name (camelCase, matching the wizard composer's
    /// shape) → operator-readable inline message.
    Validation(Vec<FieldError>),
    /// Filesystem error during the atomic write — parent-dir mkdir,
    /// tempfile open / write / fsync / rename, or chmod.
    Backend(anyhow::Error),
}

/// PR-51 / session-71 — one inline error returned to the SPA. The
/// `field` matches the wizard composer's camelCase form-field name so
/// the SPA can highlight the offending input without a lookup table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldError {
    pub field: &'static str,
    pub message: String,
}

impl std::fmt::Display for SetupSellerInfoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SetupSellerInfoError::Validation(fields) => {
                let joined: Vec<String> = fields
                    .iter()
                    .map(|fe| format!("{}: {}", fe.field, fe.message))
                    .collect();
                write!(f, "validation failed: {}", joined.join("; "))
            }
            SetupSellerInfoError::Backend(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for SetupSellerInfoError {}

/// PR-51 / session-71 — shared core that validates the operator's
/// wizard inputs and writes them to `~/.aberp/<tenant>/seller.toml`.
/// Backs the `POST /api/setup-seller-info` HTTP route the SPA's
/// SellerConfigWizard hits. (No CLI subcommand mirror today — the
/// PR-46α NAV-credentials wizard has one because it pre-dates the
/// SPA wizard; PR-51's SPA-only surface is the surgical pick.)
///
/// On success returns the same [`SellerInfoInputs`] back so the route
/// can echo it in the 200 body (the SPA renders nothing from the
/// echo today; the field is forward-looking for a future "settings
/// screen" that displays the saved values without re-reading the
/// file).
///
/// Validation rules (one [`FieldError`] per failing field, all
/// surfaced together so the operator sees every issue at once
/// rather than discovering them one-by-one on resubmit):
///
/// - `legal_name` — non-empty after trim.
/// - `tax_number` — non-empty + passes `parse_hungarian_tax_number`
///   (shape `xxxxxxxx-y-zz`).
/// - `eu_vat_number` — accepted as-is (operator-typed, no
///   shape enforcement — NAV cross-border rules vary by country).
/// - `address_country_code` — non-empty after trim. ISO 3166-1
///   alpha-2 (e.g. `HU`) is the conventional value but no shape check
///   here — the wizard's default is `HU`, and the underlying NAV
///   render accepts arbitrary strings.
/// - `address_postal_code` / `address_city` / `address_street` —
///   non-empty after trim.
/// - Bank fields — none required.
pub fn setup_seller_info_from_inputs(
    tenant: &str,
    inputs: &SellerInfoInputs,
) -> std::result::Result<SellerInfoInputs, SetupSellerInfoError> {
    let path = seller_toml_path_for_tenant(tenant).map_err(SetupSellerInfoError::Backend)?;
    setup_seller_info_to_path(&path, inputs)
}

/// PR-51 / session-71 — path-explicit sibling of
/// [`setup_seller_info_from_inputs`]. Used by the production helper
/// (which derives the path from tenant + HOME) AND by the integration
/// test (which targets a per-test tempfile to avoid mutating the
/// dev box's actual `~/.aberp/`). The Rust 2024 `unsafe set_var`
/// discipline forbids HOME mutation under cargo's parallel test
/// runner; the explicit-path overload sidesteps the issue per the
/// `serve_pdf_route.rs::seller_toml_override` precedent.
pub fn setup_seller_info_to_path(
    path: &Path,
    inputs: &SellerInfoInputs,
) -> std::result::Result<SellerInfoInputs, SetupSellerInfoError> {
    // PR-170 defense-in-depth: snapshot the prior seller.toml body
    // before we touch it. Best-effort — see seller_toml_backup module
    // docs for the failure posture. The snapshot is the recovery
    // handle if a future write-path regression costs operator state.
    let _ = crate::seller_toml_backup::snapshot_and_rotate(path);

    let mut errors: Vec<FieldError> = Vec::new();
    require_nonblank(
        "legalName",
        &inputs.legal_name,
        "Legal name is required",
        &mut errors,
    );
    match parse_hungarian_tax_number(&inputs.tax_number) {
        Ok(_) => {}
        Err(SupplierConfigError::MissingTaxNumber) => errors.push(FieldError {
            field: "taxNumber",
            message: SupplierConfigError::MissingTaxNumber.to_string(),
        }),
        Err(e @ SupplierConfigError::MalformedTaxNumber { .. }) => errors.push(FieldError {
            field: "taxNumber",
            message: e.to_string(),
        }),
    }
    require_nonblank(
        "addressCountryCode",
        &inputs.address_country_code,
        "Country code is required (default: HU)",
        &mut errors,
    );
    require_nonblank(
        "addressPostalCode",
        &inputs.address_postal_code,
        "Postal code is required",
        &mut errors,
    );
    require_nonblank(
        "addressCity",
        &inputs.address_city,
        "City is required",
        &mut errors,
    );
    require_nonblank(
        "addressStreet",
        &inputs.address_street,
        "Street is required",
        &mut errors,
    );
    if !errors.is_empty() {
        return Err(SetupSellerInfoError::Validation(errors));
    }

    // PR-75 / session-99 — preserve the existing [[seller.banks]] block
    // across the identity write. Pre-PR-75 this helper rendered the
    // whole file from identity-only inputs, wiping any bank entries the
    // operator had added via Tenant Settings → Bank accounts on every
    // subsequent identity save (the regression Ervin caught in live
    // test). read_seller_banks tolerates the missing-file case (empty
    // collection) AND folds legacy forms (`[seller.bank]`, flat-root
    // keys) into new-form entries on the way through; a malformed bank
    // section loud-fails per CLAUDE.md rule 12 rather than silently
    // dropping data.
    //
    // PR-170 / session-170 — same preservation discipline now extended
    // to `[seller.smtp]` and `[seller.numbering]`. Ervin's PROD_v1.0 →
    // PROD_v1.1 update pilot lost both because the identity-write
    // surface only re-appended banks. The compliance risk is real:
    // losing the numbering template silently flips the rendered prefix
    // back to `INV-default/00043` (counter state is in DuckDB so the
    // sequence number itself does not duplicate, but format-drift on
    // the legal invoice number is just as bad). Both reads use the
    // *_if_present helpers so a tenant that never configured one does
    // NOT have a phantom default baked into seller.toml on each save.
    let preserved = crate::seller_banks::read_seller_banks(path).map_err(|e| {
        SetupSellerInfoError::Backend(anyhow!(
            "read existing bank section for preservation across identity write: {e}"
        ))
    })?;
    let preserved_smtp = crate::smtp_config::read_smtp_config(path).map_err(|e| {
        SetupSellerInfoError::Backend(anyhow!(
            "read existing [seller.smtp] for preservation across identity write: {e}"
        ))
    })?;
    let preserved_numbering =
        crate::numbering::read_numbering_section_if_present(path).map_err(|e| {
            SetupSellerInfoError::Backend(anyhow!(
                "read existing [seller.numbering] for preservation across identity write: {e}"
            ))
        })?;

    let mut body = render_seller_toml(inputs);
    if !preserved.entries().is_empty() {
        append_section(&mut body, &preserved.to_toml_section());
    }
    if let Some(smtp) = preserved_smtp.as_ref() {
        append_section(&mut body, &crate::smtp_config::to_toml_section(smtp));
    }
    if let Some(template) = preserved_numbering.as_ref() {
        append_section(&mut body, &crate::numbering::to_toml_section(template));
    }

    write_atomic(path, body.as_bytes()).map_err(SetupSellerInfoError::Backend)?;
    Ok(inputs.clone())
}

/// PR-170 — append a TOML section to `body` with exactly one blank-line
/// separator between blocks. Centralised so the three preservation
/// re-appends (banks, smtp, numbering) compose deterministically — a
/// missing separator would let the next section's `[header]` chain onto
/// the previous section's last key, breaking the line-walker parsers.
fn append_section(body: &mut String, section: &str) {
    if section.is_empty() {
        return;
    }
    if !body.ends_with('\n') {
        body.push('\n');
    }
    body.push('\n');
    body.push_str(section);
}

fn require_nonblank(field: &'static str, value: &str, message: &str, out: &mut Vec<FieldError>) {
    if value.trim().is_empty() {
        out.push(FieldError {
            field,
            message: message.to_string(),
        });
    }
}

/// PR-51 / session-71 — `~/.aberp/<tenant>/seller.toml` path resolver.
/// Mirrors `print_invoice::resolve_seller_toml_path`'s home discipline
/// without the `Option<&Path>` override (the wizard always writes to
/// the canonical per-tenant location — operators who want a different
/// path keep using the CLI's `--seller-toml` flag).
pub fn seller_toml_path_for_tenant(tenant: &str) -> Result<PathBuf> {
    let home = std::env::var("HOME").map_err(|_| {
        anyhow!("HOME environment variable not set; cannot resolve seller.toml path")
    })?;
    Ok(PathBuf::from(home)
        .join(".aberp")
        .join(tenant)
        .join("seller.toml"))
}

/// PR-51 / session-71 — render the operator-typed inputs into the
/// flat-key TOML body `print_invoice::parse_seller_toml` already
/// reads. Section headers (`[seller]`, `[seller.address]`) are
/// decorative — the existing parser ignores them, but they make the
/// file human-readable for an operator who later opens it in a text
/// editor. Optional bank fields are omitted entirely when blank
/// (matching the example template's "uncomment to populate" shape).
fn render_seller_toml(inputs: &SellerInfoInputs) -> String {
    let mut out = String::new();
    out.push_str("# ABERP seller config — written by the SetupWizard (PR-51).\n");
    out.push_str("# Edit this file directly only if you understand the schema;\n");
    out.push_str("# the SPA wizard rewrites it atomically on each successful submit.\n\n");

    out.push_str("[seller]\n");
    out.push_str(&format!("legal_name = \"{}\"\n", inputs.legal_name.trim()));
    out.push_str(&format!("tax_number = \"{}\"\n", inputs.tax_number.trim()));
    if let Some(ev) = inputs
        .eu_vat_number
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        out.push_str(&format!("eu_vat_number = \"{ev}\"\n"));
    }
    out.push('\n');

    out.push_str("[seller.address]\n");
    out.push_str(&format!(
        "country_code = \"{}\"\n",
        inputs.address_country_code.trim()
    ));
    out.push_str(&format!(
        "postal_code = \"{}\"\n",
        inputs.address_postal_code.trim()
    ));
    out.push_str(&format!("city = \"{}\"\n", inputs.address_city.trim()));
    out.push_str(&format!("street = \"{}\"\n", inputs.address_street.trim()));
    out.push('\n');

    // Bank block — consumed live today by `print_invoice::parse_seller_toml`.
    // Emit only the populated fields so the parser sees fewer empty-value
    // lines (its tolerance is fine either way; this is cosmetic).
    out.push_str("# Bank info — consumed by the printed-invoice PDF footer.\n");
    if let Some(b) = inputs
        .bank_account_number
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        out.push_str(&format!("bank_account_number = \"{b}\"\n"));
    }
    if let Some(b) = inputs
        .iban
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        out.push_str(&format!("iban = \"{b}\"\n"));
    }
    if let Some(b) = inputs
        .bank_name
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        out.push_str(&format!("bank_name = \"{b}\"\n"));
    }
    if let Some(b) = inputs
        .swift_bic
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        out.push_str(&format!("swift_bic = \"{b}\"\n"));
    }
    out
}

/// PR-51 / session-71 — POSIX-atomic write of `body` to `path`. The
/// tempfile lives in the same directory as the destination so the
/// final `rename` is atomic on the same filesystem. On success:
/// `path` either contains the full new body OR remains untouched
/// (caller-observable atomicity per CLAUDE.md rule 12 — no half-
/// written config files).
///
/// Permissions:
///   - parent directory gets `0700` if newly created (mirrors macOS
///     `~/.aberp/serve/<tenant>/` discipline from PR-9-1).
///   - destination file gets `0600` after rename (mirrors the loopback
///     `loopback.key.pem` chmod in `serve::ensure_loopback_cert`).
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

    // Tempfile in the same dir so the final `rename` is atomic on the
    // same filesystem. Suffix is `PID-NANOS-COUNTER` so concurrent
    // writes in the same process (cargo's parallel test runner, or a
    // future SPA re-submit case) don't collide on the same tempfile.
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp_name = format!(".seller.toml.tmp.{}-{}-{}", std::process::id(), nanos, seq,);
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

/// PR-51 / session-71 — typed read-side counterpart of
/// [`render_seller_toml`]. Returns `Ok(None)` when the file is absent
/// (the wizard fires) and `Ok(Some(...))` when the identity block
/// parses cleanly. A file that exists but lacks identity fields
/// returns `Ok(None)` so the boot-state detection treats it as
/// "needs config" (PR-51 motivation: bank-only legacy files predate
/// identity persistence; the wizard fires once to fill them in).
///
/// Bank fields are ignored here — they're still read by
/// `print_invoice::parse_seller_toml` for the PDF render. This reader
/// is the issuance-side identity surface only.
pub fn read_seller_identity(path: &Path) -> Result<Option<SellerIdentity>> {
    if !path.exists() {
        return Ok(None);
    }
    let body = fs::read_to_string(path)
        .with_context(|| format!("read seller.toml at {}", path.display()))?;
    Ok(parse_seller_identity(&body))
}

/// PR-53 / session-73 — bank-block subset of the `seller.toml` file.
/// All four fields are optional (the wizard treats them as optional;
/// the PDF renderer hides empty rows). Returns an all-`None` value
/// when the file is absent so the Tenant Settings page can render
/// blank inputs without a separate file-missing branch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SellerBank {
    pub account_number: Option<String>,
    pub iban: Option<String>,
    pub name: Option<String>,
    pub swift_bic: Option<String>,
}

/// PR-53 / session-73 — read the bank block out of `seller.toml`.
/// Returns an empty [`SellerBank`] when the file is absent (the
/// Settings page renders blank inputs); a present file with any
/// recognised bank-key is folded into the matching slot.
pub fn read_seller_bank(path: &Path) -> Result<SellerBank> {
    if !path.exists() {
        return Ok(SellerBank::default());
    }
    let body = fs::read_to_string(path)
        .with_context(|| format!("read seller.toml at {}", path.display()))?;
    Ok(parse_seller_bank(&body))
}

/// PR-53 / session-73 — line-oriented parser for the bank block.
/// Mirrors `parse_seller_identity`'s tolerance (blank / `#`-comment /
/// `[section]` lines skipped; `key = "value"` lines collected) and
/// folds the four recognised bank keys into the typed [`SellerBank`].
pub fn parse_seller_bank(body: &str) -> SellerBank {
    let mut bank = SellerBank::default();
    for raw in body.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
            continue;
        }
        let (k, v) = match line.split_once('=') {
            Some(pair) => pair,
            None => continue,
        };
        let key = k.trim();
        let value = v.trim().trim_matches('"').to_string();
        if value.is_empty() {
            continue;
        }
        match key {
            "bank_account_number" => bank.account_number = Some(value),
            "iban" => bank.iban = Some(value),
            "bank_name" => bank.name = Some(value),
            "swift_bic" => bank.swift_bic = Some(value),
            _ => {}
        }
    }
    bank
}

/// PR-51 / session-71 — parsed identity-block subset of the
/// `seller.toml` file. Returns `None` if any required identity field
/// is missing so the caller can fold it into the
/// "needs-seller-config" boot-state detection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SellerIdentity {
    pub legal_name: String,
    pub tax_number: String,
    pub eu_vat_number: Option<String>,
    pub address_country_code: String,
    pub address_postal_code: String,
    pub address_city: String,
    pub address_street: String,
}

/// Line-oriented parser matching `print_invoice::parse_seller_toml`'s
/// tolerance: blank / `#`-comment / `[section]` lines skipped; `key =
/// "value"` lines collected. Returns `None` if any of the four
/// required identity fields (`legal_name`, `tax_number`,
/// `country_code`, `postal_code`/`city`/`street`) is absent.
pub fn parse_seller_identity(body: &str) -> Option<SellerIdentity> {
    let mut legal_name: Option<String> = None;
    let mut tax_number: Option<String> = None;
    let mut eu_vat_number: Option<String> = None;
    let mut country_code: Option<String> = None;
    let mut postal_code: Option<String> = None;
    let mut city: Option<String> = None;
    let mut street: Option<String> = None;
    for raw in body.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
            continue;
        }
        let (k, v) = match line.split_once('=') {
            Some(pair) => pair,
            None => continue,
        };
        let key = k.trim();
        let value = v.trim().trim_matches('"').to_string();
        if value.is_empty() {
            continue;
        }
        match key {
            "legal_name" => legal_name = Some(value),
            "tax_number" => tax_number = Some(value),
            "eu_vat_number" => eu_vat_number = Some(value),
            "country_code" => country_code = Some(value),
            "postal_code" => postal_code = Some(value),
            "city" => city = Some(value),
            "street" => street = Some(value),
            _ => {}
        }
    }
    Some(SellerIdentity {
        legal_name: legal_name?,
        tax_number: tax_number?,
        eu_vat_number,
        address_country_code: country_code?,
        address_postal_code: postal_code?,
        address_city: city?,
        address_street: street?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn good_inputs() -> SellerInfoInputs {
        SellerInfoInputs {
            legal_name: "Áben Consulting KFT.".to_string(),
            tax_number: "24904362-2-41".to_string(),
            eu_vat_number: Some("HU24904362".to_string()),
            address_country_code: "HU".to_string(),
            address_postal_code: "1037".to_string(),
            address_city: "Budapest".to_string(),
            address_street: "Visszatérő köz 6".to_string(),
            bank_account_number: Some("12345678-12345678-12345678".to_string()),
            iban: Some("LT14 3250 0448 1318 6860".to_string()),
            bank_name: Some("Revolut".to_string()),
            swift_bic: Some("REVOLT21".to_string()),
        }
    }

    #[test]
    fn render_round_trips_identity_via_parser() {
        let inputs = good_inputs();
        let body = render_seller_toml(&inputs);
        let id = parse_seller_identity(&body).expect("identity parses");
        assert_eq!(id.legal_name, "Áben Consulting KFT.");
        assert_eq!(id.tax_number, "24904362-2-41");
        assert_eq!(id.eu_vat_number.as_deref(), Some("HU24904362"));
        assert_eq!(id.address_country_code, "HU");
        assert_eq!(id.address_postal_code, "1037");
        assert_eq!(id.address_city, "Budapest");
        assert_eq!(id.address_street, "Visszatérő köz 6");
    }

    /// The wizard writes the same flat-key shape the legacy
    /// `print_invoice::parse_seller_toml` reads — bank fields must
    /// round-trip cleanly so the PDF footer keeps working after the
    /// wizard rewrites the file.
    #[test]
    fn render_round_trips_bank_via_legacy_parser() {
        let inputs = good_inputs();
        let body = render_seller_toml(&inputs);
        let bank = crate::print_invoice::parse_seller_toml(&body).expect("legacy parses");
        assert_eq!(
            bank.bank_account_number.as_deref(),
            Some("12345678-12345678-12345678"),
        );
        assert_eq!(bank.iban.as_deref(), Some("LT14 3250 0448 1318 6860"));
        assert_eq!(bank.bank_name.as_deref(), Some("Revolut"));
        assert_eq!(bank.swift_bic.as_deref(), Some("REVOLT21"));
    }

    #[test]
    fn render_omits_blank_optional_eu_vat_and_bank() {
        let mut inputs = good_inputs();
        inputs.eu_vat_number = Some("   ".to_string());
        inputs.bank_account_number = None;
        inputs.iban = Some(String::new());
        inputs.bank_name = None;
        inputs.swift_bic = None;
        let body = render_seller_toml(&inputs);
        assert!(
            !body.contains("eu_vat_number"),
            "blank EU VAT must be omitted: {body}"
        );
        assert!(
            !body.contains("bank_account_number"),
            "absent bank account must be omitted: {body}"
        );
        assert!(!body.contains("iban"), "blank IBAN must be omitted: {body}");
        assert!(
            !body.contains("bank_name"),
            "absent bank_name must be omitted: {body}"
        );
        assert!(
            !body.contains("swift_bic"),
            "absent swift_bic must be omitted: {body}"
        );
    }

    #[test]
    fn parse_identity_returns_none_when_any_required_field_absent() {
        // Bank-only legacy file (no identity) — wizard fires.
        let body = "bank_account_number = \"abc\"\niban = \"def\"\n";
        assert!(parse_seller_identity(body).is_none());

        // Identity missing street — wizard fires.
        let body = r#"
[seller]
legal_name = "X"
tax_number = "24904362-2-41"
[seller.address]
country_code = "HU"
postal_code = "1037"
city = "Budapest"
"#;
        assert!(parse_seller_identity(body).is_none());
    }

    #[test]
    fn validation_rejects_blank_legal_name() {
        let mut inputs = good_inputs();
        inputs.legal_name = "   ".to_string();
        let err = setup_seller_info_from_inputs("t-validation", &inputs)
            .expect_err("blank legal name must fail");
        match err {
            SetupSellerInfoError::Validation(fields) => {
                assert!(
                    fields.iter().any(|fe| fe.field == "legalName"),
                    "expected legalName field error, got: {fields:?}"
                );
            }
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[test]
    fn validation_rejects_malformed_tax_number() {
        let mut inputs = good_inputs();
        inputs.tax_number = "24904362".to_string(); // bare 8-digit, missing dashes
        let err = setup_seller_info_from_inputs("t-validation", &inputs)
            .expect_err("malformed tax must fail");
        match err {
            SetupSellerInfoError::Validation(fields) => {
                let tax = fields
                    .iter()
                    .find(|fe| fe.field == "taxNumber")
                    .expect("taxNumber error present");
                assert!(
                    tax.message.contains("not a valid Hungarian"),
                    "message must surface the ADÓSZÁM hint, got: {}",
                    tax.message
                );
            }
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[test]
    fn validation_collects_all_errors_at_once() {
        // Blank legal_name + missing tax + blank city all surface in
        // one response so the operator fixes them together.
        let mut inputs = good_inputs();
        inputs.legal_name = String::new();
        inputs.tax_number = String::new();
        inputs.address_city = "   ".to_string();
        let err = setup_seller_info_from_inputs("t-validation", &inputs)
            .expect_err("multi-field invalid must fail");
        match err {
            SetupSellerInfoError::Validation(fields) => {
                let names: Vec<&str> = fields.iter().map(|fe| fe.field).collect();
                assert!(names.contains(&"legalName"), "{names:?}");
                assert!(names.contains(&"taxNumber"), "{names:?}");
                assert!(names.contains(&"addressCity"), "{names:?}");
            }
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    /// Allocate a unique temp dir under the system temp root. Mirrors
    /// the `print_invoice_render::test_dir` posture (CLAUDE.md rule 13:
    /// no `tempfile` dev-dep; leak the per-test ULID directory at end-
    /// of-test, which is acceptable for the OS-temp-root convention).
    fn test_dir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join("aberp-setup-seller-info-test")
            .join(format!("{}-{}", label, ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    /// PR-75 / session-99 — Ervin's live-test regression. The wizard /
    /// Tenant-Settings identity save MUST NOT wipe `[[seller.banks]]`
    /// entries the operator added via the bank-accounts subsection.
    /// Pre-PR-75 `setup_seller_info_to_path` rendered the whole file
    /// from identity-only inputs, truncating the bank block. The fix
    /// reads the existing bank section + appends it back across the
    /// atomic write.
    #[test]
    fn identity_save_preserves_existing_seller_banks_block() {
        let tmp = test_dir("preserves_banks");
        let path = tmp.join("seller.toml");
        // Pre-condition: file already has identity + one HUF bank.
        let pre = "\
[seller]
legal_name = \"Old Name\"
tax_number = \"24904362-2-41\"

[seller.address]
country_code = \"HU\"
postal_code = \"1037\"
city = \"Budapest\"
street = \"Old Street\"

[[seller.banks]]
currency       = \"HUF\"
account_number = \"12345678-12345678-12345678\"
bank_name      = \"Erste Bank\"
swift_bic      = \"GIBAHUHB\"
default        = true
";
        std::fs::write(&path, pre).expect("write pre-condition file");

        // Identity-only payload (mirror of the SellerConfigWizard +
        // TenantSettings save: legacy bank fields blank, identity fields
        // edited).
        let inputs = SellerInfoInputs {
            legal_name: "New Name".to_string(),
            tax_number: "24904362-2-41".to_string(),
            eu_vat_number: None,
            address_country_code: "HU".to_string(),
            address_postal_code: "1037".to_string(),
            address_city: "Budapest".to_string(),
            address_street: "Visszatérő köz 6".to_string(),
            bank_account_number: None,
            iban: None,
            bank_name: None,
            swift_bic: None,
        };

        setup_seller_info_to_path(&path, &inputs).expect("identity save");

        let after = std::fs::read_to_string(&path).expect("re-read seller.toml");
        // Identity edited:
        assert!(
            after.contains("legal_name = \"New Name\""),
            "identity updated: {after}"
        );
        assert!(
            after.contains("street = \"Visszatérő köz 6\""),
            "address updated: {after}"
        );
        // Bank PRESERVED — the load-bearing PR-75 invariant.
        assert!(
            after.contains("12345678-12345678-12345678"),
            "[[seller.banks]] entry must survive identity write: {after}"
        );
        assert!(
            after.contains("Erste Bank"),
            "bank name must survive identity write: {after}"
        );
        // Re-parse the file via the seller_banks reader to confirm the
        // preserved block round-trips cleanly (no malformed output).
        let reparsed = crate::seller_banks::read_seller_banks(&path).expect("re-parse banks");
        assert_eq!(reparsed.entries().len(), 1);
        assert_eq!(
            reparsed.entries()[0].account_number,
            "12345678-12345678-12345678"
        );
        assert!(reparsed.entries()[0].default);
    }

    /// PR-170 / session-170 — Ervin's prod-update regression. The
    /// SetupWizard / TenantSettings identity save MUST NOT wipe
    /// `[seller.smtp]`. Pre-PR-170 the identity-write surface only
    /// re-appended `[[seller.banks]]`, so any operator-configured
    /// SMTP block disappeared on the next identity save. The PROD_v1.0
    /// → PROD_v1.1 update pilot lost SMTP this way; this pin catches
    /// any regression.
    #[test]
    fn identity_save_preserves_existing_smtp_section() {
        let tmp = test_dir("preserves_smtp");
        let path = tmp.join("seller.toml");
        let pre = "\
[seller]
legal_name = \"Old Name\"
tax_number = \"24904362-2-41\"

[seller.address]
country_code = \"HU\"
postal_code = \"1037\"
city = \"Budapest\"
street = \"Old Street\"

[seller.smtp]
host = \"smtppro.zoho.eu\"
port = 465
from_address = \"ervin@aben.ch\"
from_display_name = \"Áben Consulting Számlázás\"
username = \"ervin@aben.ch\"
security = \"Tls\"
attach_xml = true
";
        std::fs::write(&path, pre).expect("write pre-condition file");

        let inputs = SellerInfoInputs {
            legal_name: "New Name".to_string(),
            tax_number: "24904362-2-41".to_string(),
            eu_vat_number: None,
            address_country_code: "HU".to_string(),
            address_postal_code: "1037".to_string(),
            address_city: "Budapest".to_string(),
            address_street: "Visszatérő köz 6".to_string(),
            bank_account_number: None,
            iban: None,
            bank_name: None,
            swift_bic: None,
        };

        setup_seller_info_to_path(&path, &inputs).expect("identity save");

        // Re-parse via the SMTP module's own reader — round-trip MUST
        // recover the original SmtpConfig byte-for-byte at the value
        // level. A surface-level `contains("smtppro.zoho.eu")` would
        // miss a regression that mangled `port`/`security`.
        let smtp = crate::smtp_config::read_smtp_config(&path)
            .expect("re-read smtp")
            .expect("smtp section present after identity save");
        assert_eq!(smtp.host, "smtppro.zoho.eu");
        assert_eq!(smtp.port, 465);
        assert_eq!(smtp.from_address, "ervin@aben.ch");
        assert_eq!(
            smtp.from_display_name.as_deref(),
            Some("Áben Consulting Számlázás")
        );
        assert_eq!(smtp.username, "ervin@aben.ch");
        assert_eq!(smtp.security, crate::smtp_config::SmtpSecurity::Tls);
        assert!(smtp.attach_xml);
        // Identity edited:
        let after = std::fs::read_to_string(&path).expect("re-read seller.toml");
        assert!(
            after.contains("legal_name = \"New Name\""),
            "identity updated: {after}"
        );
    }

    /// PR-170 / session-170 — same pin for `[seller.numbering]`. This
    /// is the compliance-load-bearing one: silently losing the operator's
    /// template flips the next invoice number from `ABERP/2026/00043`
    /// to `INV-default/00043` (default_template's prefix). The counter
    /// sequence itself survives in DuckDB so no duplicate numbers, but
    /// format-drift on a Hungarian invoice number is a §169 fail.
    #[test]
    fn identity_save_preserves_existing_numbering_section() {
        let tmp = test_dir("preserves_numbering");
        let path = tmp.join("seller.toml");
        let pre = "\
[seller]
legal_name = \"Old Name\"
tax_number = \"24904362-2-41\"

[seller.address]
country_code = \"HU\"
postal_code = \"1037\"
city = \"Budapest\"
street = \"Old Street\"

[seller.numbering]
segments = [{ kind = \"Literal\", text = \"ABERP/\" }, { kind = \"Year\", digits = 4 }, { kind = \"Literal\", text = \"/\" }, { kind = \"Counter\", pad_width = 5 }]
reset_policy = \"on_year_change\"
start_value = 1
";
        std::fs::write(&path, pre).expect("write pre-condition file");

        let inputs = SellerInfoInputs {
            legal_name: "New Name".to_string(),
            tax_number: "24904362-2-41".to_string(),
            eu_vat_number: None,
            address_country_code: "HU".to_string(),
            address_postal_code: "1037".to_string(),
            address_city: "Budapest".to_string(),
            address_street: "Visszatérő köz 6".to_string(),
            bank_account_number: None,
            iban: None,
            bank_name: None,
            swift_bic: None,
        };

        setup_seller_info_to_path(&path, &inputs).expect("identity save");

        let template =
            crate::numbering::read_numbering_template(&path).expect("re-read numbering template");
        // The render path MUST yield Ervin's prod prefix, not the
        // default `INV-default/`. This is the load-bearing assertion.
        let rendered = template.render(2026, 43);
        assert_eq!(
            rendered, "ABERP/2026/00043",
            "numbering template lost across identity save — rendered `{rendered}`"
        );
        assert_eq!(template.start_value, 1);
        assert!(matches!(
            template.reset_policy,
            crate::numbering::ResetPolicy::OnYearChange
        ));
    }

    /// PR-170 / session-170 — four-way invariant. The full live-prod
    /// shape (identity + banks + smtp + numbering) MUST survive an
    /// identity save with every section recovered via its own typed
    /// reader. This is the full reproduction of Ervin's production
    /// seller.toml; a regression that drops ANY of the three preserved
    /// sections trips this pin.
    #[test]
    fn identity_save_preserves_all_three_sections_together() {
        let tmp = test_dir("preserves_all_three");
        let path = tmp.join("seller.toml");
        let pre = "\
[seller]
legal_name = \"ÁBEN CONSULTING KFT.\"
tax_number = \"24904362-2-41\"
eu_vat_number = \"HU24904362\"

[seller.address]
country_code = \"HU\"
postal_code = \"1037\"
city = \"Budapest\"
street = \"Old Street\"

[[seller.banks]]
currency       = \"HUF\"
account_number = \"HU71 12011375 01945291 00100002\"
bank_name      = \"Raiffeisen\"
swift_bic      = \"RAIFHUHB\"
default        = true

[[seller.banks]]
currency       = \"EUR\"
account_number = \"LT143250044813186860\"
bank_name      = \"Revolut\"
swift_bic      = \"REVOLT21\"
default        = true

[seller.smtp]
host = \"smtppro.zoho.eu\"
port = 465
from_address = \"ervin@aben.ch\"
from_display_name = \"Áben Consulting Számlázás\"
username = \"ervin@aben.ch\"
security = \"Tls\"
attach_xml = true

[seller.numbering]
segments = [{ kind = \"Literal\", text = \"ABERP/\" }, { kind = \"Year\", digits = 4 }, { kind = \"Literal\", text = \"/\" }, { kind = \"Counter\", pad_width = 5 }]
reset_policy = \"on_year_change\"
start_value = 1
";
        std::fs::write(&path, pre).expect("write pre-condition file");

        let inputs = SellerInfoInputs {
            legal_name: "ÁBEN CONSULTING KFT.".to_string(),
            tax_number: "24904362-2-41".to_string(),
            eu_vat_number: Some("HU24904362".to_string()),
            address_country_code: "HU".to_string(),
            address_postal_code: "1037".to_string(),
            address_city: "Budapest".to_string(),
            address_street: "Visszatérő köz 6".to_string(),
            bank_account_number: None,
            iban: None,
            bank_name: None,
            swift_bic: None,
        };

        setup_seller_info_to_path(&path, &inputs).expect("identity save");

        let banks = crate::seller_banks::read_seller_banks(&path).expect("re-read banks");
        assert_eq!(banks.entries().len(), 2, "both bank entries must survive");

        let smtp = crate::smtp_config::read_smtp_config(&path)
            .expect("re-read smtp")
            .expect("smtp section present");
        assert_eq!(smtp.host, "smtppro.zoho.eu");

        let template =
            crate::numbering::read_numbering_template(&path).expect("re-read numbering template");
        assert_eq!(template.render(2026, 1), "ABERP/2026/00001");
    }

    /// PR-170 / session-170 — defense-in-depth wiring pin. The identity
    /// writer MUST trigger [`crate::seller_toml_backup::snapshot_and_rotate`]
    /// before clobbering the prior file body. Proves the wiring works
    /// end-to-end (the helper's own unit tests cover the backup logic
    /// in isolation; this one proves the writer actually calls it).
    #[test]
    fn identity_save_creates_backup_of_prior_seller_toml() {
        let tmp = test_dir("creates_backup");
        let path = tmp.join("seller.toml");
        let pre_body = "[seller]\nlegal_name = \"Old\"\ntax_number = \"24904362-2-41\"\n";
        std::fs::write(&path, pre_body).expect("write pre-condition file");

        let inputs = good_inputs();
        setup_seller_info_to_path(&path, &inputs).expect("identity save");

        let backups: Vec<_> = std::fs::read_dir(&tmp)
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(".seller.toml.backup-")
            })
            .collect();
        assert_eq!(backups.len(), 1, "exactly one backup of the prior body");
        let backup_body = std::fs::read_to_string(backups[0].path()).unwrap();
        assert_eq!(
            backup_body, pre_body,
            "backup captures the prior body byte-for-byte"
        );
    }

    /// PR-170 / session-170 — pin the "no phantom default" invariant.
    /// A tenant that never configured a numbering template must not
    /// gain one on identity save. Without the *_if_present helper, the
    /// existing `read_numbering_template` falls back to the default
    /// template, which would bake `INV-default/` into seller.toml on
    /// every save — silently flipping a tenant from "auto-default" to
    /// "explicit-default" behaviour. This pin catches that regression.
    #[test]
    fn identity_save_does_not_materialise_default_numbering_when_section_absent() {
        let tmp = test_dir("no_phantom_numbering");
        let path = tmp.join("seller.toml");
        let pre = "\
[seller]
legal_name = \"Old Name\"
tax_number = \"24904362-2-41\"

[seller.address]
country_code = \"HU\"
postal_code = \"1037\"
city = \"Budapest\"
street = \"Old Street\"
";
        std::fs::write(&path, pre).expect("write pre-condition file");

        let inputs = good_inputs();
        setup_seller_info_to_path(&path, &inputs).expect("identity save");

        let after = std::fs::read_to_string(&path).expect("re-read seller.toml");
        assert!(
            !after.contains("[seller.numbering]"),
            "no phantom [seller.numbering] section when none existed pre-write: {after}"
        );
        assert!(
            !after.contains("INV-default"),
            "no default-template prefix leaked into file: {after}"
        );
    }

    /// PR-75 / session-99 — exercises the
    /// "no existing banks → identity write writes identity-only file"
    /// branch so the preservation logic does not gain a phantom empty
    /// bank section when there is nothing to preserve.
    #[test]
    fn identity_save_without_existing_banks_writes_identity_only() {
        let tmp = test_dir("identity_only");
        let path = tmp.join("seller.toml");
        // No pre-existing file (first-run wizard scenario).
        assert!(!path.exists());

        let inputs = good_inputs();
        setup_seller_info_to_path(&path, &inputs).expect("first-run identity save");

        let after = std::fs::read_to_string(&path).expect("re-read seller.toml");
        assert!(
            !after.contains("[[seller.banks]]"),
            "no phantom bank section when nothing to preserve: {after}"
        );
        // The legacy bank fields the wizard rendered live at root
        // (good_inputs populates them) — PR-D will remove this surface,
        // but PR-75's preservation MUST NOT introduce duplication on the
        // first-run path.
        assert!(after.contains("legal_name"), "identity present: {after}");
    }
}
