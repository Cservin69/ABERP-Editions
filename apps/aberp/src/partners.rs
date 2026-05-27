//! Partners module — operator-managed customer/supplier records.
//!
//! # PR-48α / session-68 scope
//!
//! Pre-PR-48α every SPA-issued invoice (PR-44ζ from session 59) made
//! the operator retype buyer name, address, and ADÓSZÁM from scratch.
//! Partners is the saved-list-of-reusable-buyers entity that fixes
//! this. PR-48α ships the backend foundation only:
//!   - DuckDB `partners` table (lazy `CREATE TABLE IF NOT EXISTS`).
//!   - `Partner` domain type + `PartnerKind` closed-vocab enum +
//!     `PartnerId` prefixed-ULID newtype (`prt_<ULID>`).
//!   - `validate_tax_number` (Hungarian `xxxxxxxx-y-zz` format check) +
//!     `validate_partner_inputs` (structured field-level errors).
//!   - Five CRUD ops:
//!     [`create_partner`], [`get_partner`], [`list_partners`],
//!     [`update_partner`], [`soft_delete_partner`].
//!
//! The SPA management screen + the typeahead wiring into the issue
//! form are deferred to PR-48β / PR-48γ.
//!
//! # History posture (PR-48α A-decision)
//!
//! Partner CRUD does NOT fire audit-ledger entries. The audit ledger
//! (`aberp_audit_ledger`) is reserved for invoice lifecycle per
//! ADR-0008 — extending the `EventKind` ladder would couple partner
//! operations to invoice-hash-chain verification, which is the wrong
//! integrity surface.
//!
//! Partner history is captured at the row level:
//!   - `created_at` — row insertion timestamp (Rfc3339).
//!   - `updated_at` — most-recent mutation timestamp.
//!   - `deleted_at` — soft-delete tombstone; `NULL` ⇒ active.
//!
//! Per-field history (e.g. "what address did this partner have last
//! month?") is NOT recorded — but the issued invoice IS the immutable
//! regulatory record per PR-44ζ's denormalised `CustomerJson` shape, so
//! the partner table is operational state rather than legal record.
//! A future `partner_history` append-only table is a backwards-
//! compatible add if per-field history becomes a compliance ask.
//!
//! # tenant_id on the row
//!
//! Each tenant has its own DuckDB file (ADR-0002), so tenant scoping is
//! enforced at the file level. The `partners.tenant_id` column is a
//! defensive denormalisation mirroring `audit_ledger.tenant_id` — every
//! query filters by it so a future shared-DB shift requires zero query
//! changes.
//!
//! # Timestamp column type
//!
//! `created_at` / `updated_at` / `deleted_at` are `VARCHAR` columns
//! holding Rfc3339 strings — same convention as `invoice_series.created_at`,
//! `invoice_sequence_state.updated_at`, `invoice.issue_date`. DuckDB has
//! a native `TIMESTAMP` type; using VARCHAR matches the existing
//! codebase per CLAUDE.md rule 11.

use anyhow::{Context, Result};
use duckdb::{params, Connection};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use ulid::Ulid;

// ──────────────────────────────────────────────────────────────────────
// PartnerId — prefixed-ULID newtype.
//
// Mirrors `aberp_billing::InvoiceId` / `CustomerId` shape (the
// `ulid_newtype!` macro lives inside the billing crate and is not
// exported; reproducing the four-method API here is two dozen lines
// and avoids widening the billing crate's public surface for a
// single-call-site consumer per CLAUDE.md rule 2).
// ──────────────────────────────────────────────────────────────────────

/// ULID newtype rendered as `prt_<26-char-ULID>` on the wire. Per
/// ADR-0005 every entity gets a newtype; type confusion is a compile
/// error, not a runtime hunt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PartnerId(pub Ulid);

impl PartnerId {
    pub fn new() -> Self {
        Self(Ulid::new())
    }

    pub fn to_prefixed_string(&self) -> String {
        format!("prt_{}", self.0)
    }

    pub fn as_ulid(&self) -> Ulid {
        self.0
    }
}

impl Default for PartnerId {
    fn default() -> Self {
        Self::new()
    }
}

// ──────────────────────────────────────────────────────────────────────
// PartnerKind — closed-vocab enum.
// ──────────────────────────────────────────────────────────────────────

/// Discriminator on a partner: who they are to the operator's
/// business. Hungarian invoice law differentiates customer-vs-supplier
/// treatment for some VAT scenarios, so the entity itself is tagged at
/// the data layer rather than derived from invoice direction.
///
/// Serde emits PascalCase variant names (`"Customer"`, `"Supplier"`,
/// `"Both"`) — same shape as `InvoiceState` in `serve.rs`. The SPA's
/// string-union mirror reads these literally.
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone, Copy)]
pub enum PartnerKind {
    Customer,
    Supplier,
    Both,
}

impl PartnerKind {
    fn as_db_str(&self) -> &'static str {
        match self {
            PartnerKind::Customer => "Customer",
            PartnerKind::Supplier => "Supplier",
            PartnerKind::Both => "Both",
        }
    }

    fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "Customer" => Some(PartnerKind::Customer),
            "Supplier" => Some(PartnerKind::Supplier),
            "Both" => Some(PartnerKind::Both),
            _ => None,
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Partner — domain + wire shape.
//
// Single struct serves both the in-memory representation AND the JSON
// wire body. Per CLAUDE.md rule 13 (delete before optimize) a separate
// `PartnerView` adds ceremony without surfacing any field difference;
// the brief's "PartnerView" is read here as "the serialized form of
// Partner."
// ──────────────────────────────────────────────────────────────────────

#[derive(Serialize, Debug, PartialEq, Eq, Clone)]
pub struct Partner {
    /// Prefixed-ULID `prt_<26-char-ULID>`.
    pub id: String,
    pub display_name: String,
    pub legal_name: String,
    pub kind: PartnerKind,
    pub tax_number: String,
    pub eu_vat_number: Option<String>,
    pub address_street: Option<String>,
    pub address_postal_code: Option<String>,
    pub address_city: Option<String>,
    pub address_country: Option<String>,
    pub bank_account: Option<String>,
    pub contact_email: Option<String>,
    pub contact_phone: Option<String>,
    /// Rfc3339-formatted UTC timestamp. Row creation time.
    pub created_at: String,
    /// Rfc3339-formatted UTC timestamp. Most-recent mutation.
    pub updated_at: String,
    /// `None` when the partner is active; `Some(rfc3339)` after a
    /// soft-delete. The row stays in the DB for historical-invoice
    /// lookups.
    pub deleted_at: Option<String>,
}

/// Request-body shape for create / update.
///
/// All optional fields default to `None` so the SPA can omit them
/// from JSON without serde rejecting the body. `kind` is mandatory
/// and must be one of the three `PartnerKind` literals.
#[derive(Deserialize, Debug, Clone)]
pub struct PartnerInputs {
    pub display_name: String,
    pub legal_name: String,
    pub kind: PartnerKind,
    pub tax_number: String,
    #[serde(default)]
    pub eu_vat_number: Option<String>,
    #[serde(default)]
    pub address_street: Option<String>,
    #[serde(default)]
    pub address_postal_code: Option<String>,
    #[serde(default)]
    pub address_city: Option<String>,
    #[serde(default)]
    pub address_country: Option<String>,
    #[serde(default)]
    pub bank_account: Option<String>,
    #[serde(default)]
    pub contact_email: Option<String>,
    #[serde(default)]
    pub contact_phone: Option<String>,
}

/// Structured validation error. The HTTP route emits a JSON body
/// `{ "error": "validation_failed", "fields": [{ "field": ..., "message": ... }] }`
/// so the SPA can render per-field inline messages (A157 pattern).
#[derive(Serialize, Debug, PartialEq, Eq, Clone)]
pub struct ValidationError {
    pub field: &'static str,
    pub message: String,
}

// ──────────────────────────────────────────────────────────────────────
// Validation helpers.
// ──────────────────────────────────────────────────────────────────────

/// Hungarian ADÓSZÁM format check: `^\d{8}-\d-\d{2}$`.
///
/// The 8-digit base, 1-digit VAT code, and 2-digit county code are
/// the documented shape; the regex above is the operator-friendly
/// gate. Deeper semantic checks (county-code range, VAT-code range)
/// are NOT done here — NAV's submission path rejects malformed
/// numbers at the wire boundary, which is the authoritative validator.
/// PR-48α's check exists so a typo at SPA-form time surfaces inline
/// before the operator submits the partner row.
pub fn validate_tax_number(s: &str) -> Result<(), String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("tax number is required".to_string());
    }
    let mut chars = s.chars();
    for i in 0..8 {
        match chars.next() {
            Some(c) if c.is_ascii_digit() => {}
            _ => {
                return Err(format!(
                    "tax number must start with 8 digits (got `{}` at position {})",
                    s, i
                ));
            }
        }
    }
    match chars.next() {
        Some('-') => {}
        _ => {
            return Err(format!(
                "tax number must have `-` after the 8 digits (got `{}`)",
                s
            ))
        }
    }
    match chars.next() {
        Some(c) if c.is_ascii_digit() => {}
        _ => {
            return Err(format!(
                "tax number must have a single digit (VAT code) after the first `-` (got `{}`)",
                s
            ))
        }
    }
    match chars.next() {
        Some('-') => {}
        _ => {
            return Err(format!(
                "tax number must have `-` after the VAT-code digit (got `{}`)",
                s
            ))
        }
    }
    for i in 0..2 {
        match chars.next() {
            Some(c) if c.is_ascii_digit() => {}
            _ => {
                return Err(format!(
                    "tax number must end with 2 digits (county code) (got `{}` at trailing position {})",
                    s, i
                ));
            }
        }
    }
    if chars.next().is_some() {
        return Err(format!(
            "tax number has trailing characters after `xxxxxxxx-y-zz` (got `{}`)",
            s
        ));
    }
    Ok(())
}

/// Validate all field-level rules; returns a `Vec` of errors so the SPA
/// can surface every problem at once rather than the operator fixing
/// them one-at-a-time across multiple round-trips.
///
/// Per CLAUDE.md rule 9: each branch pins a distinct rule.
pub fn validate_partner_inputs(inputs: &PartnerInputs) -> Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();
    if inputs.display_name.trim().is_empty() {
        errors.push(ValidationError {
            field: "display_name",
            message: "display name is required".to_string(),
        });
    }
    if inputs.legal_name.trim().is_empty() {
        errors.push(ValidationError {
            field: "legal_name",
            message: "legal name is required".to_string(),
        });
    }
    if let Err(msg) = validate_tax_number(&inputs.tax_number) {
        errors.push(ValidationError {
            field: "tax_number",
            message: msg,
        });
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

// ──────────────────────────────────────────────────────────────────────
// DuckDB schema + CRUD.
// ──────────────────────────────────────────────────────────────────────

const PARTNERS_SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS partners (
    id                  VARCHAR NOT NULL PRIMARY KEY,
    tenant_id           VARCHAR NOT NULL,
    display_name        VARCHAR NOT NULL,
    legal_name          VARCHAR NOT NULL,
    kind                VARCHAR NOT NULL CHECK (kind IN ('Customer','Supplier','Both')),
    tax_number          VARCHAR NOT NULL,
    eu_vat_number       VARCHAR,
    address_street      VARCHAR,
    address_postal_code VARCHAR,
    address_city        VARCHAR,
    address_country     VARCHAR,
    bank_account        VARCHAR,
    contact_email       VARCHAR,
    contact_phone       VARCHAR,
    created_at          VARCHAR NOT NULL,
    updated_at          VARCHAR NOT NULL,
    deleted_at          VARCHAR
);
CREATE INDEX IF NOT EXISTS partners_tenant_deleted_idx
    ON partners (tenant_id, deleted_at);
CREATE INDEX IF NOT EXISTS partners_tenant_display_idx
    ON partners (tenant_id, display_name);
";

/// Idempotent `CREATE TABLE IF NOT EXISTS` for the partners table.
/// Callers (HTTP route handlers, tests) hit this on every entry so a
/// fresh tenant DB picks up the schema lazily — same posture as
/// `aberp_billing::DuckDbBillingStore::ensure_schema` /
/// `aberp_audit_ledger::ensure_schema`.
pub fn ensure_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(PARTNERS_SCHEMA_SQL)
        .context("ensure partners schema")
}

/// Empty-string-to-None coercion so the SPA can emit `""` for unset
/// optional fields without persisting a meaningless empty string.
/// Applied at the storage boundary so the in-memory `Partner` round-
/// trips cleanly across the wire and back.
fn normalize_optional(s: Option<&str>) -> Option<String> {
    match s {
        Some(v) if !v.trim().is_empty() => Some(v.trim().to_string()),
        _ => None,
    }
}

fn inputs_to_normalized(inputs: &PartnerInputs) -> NormalizedInputs {
    NormalizedInputs {
        display_name: inputs.display_name.trim().to_string(),
        legal_name: inputs.legal_name.trim().to_string(),
        kind: inputs.kind,
        tax_number: inputs.tax_number.trim().to_string(),
        eu_vat_number: normalize_optional(inputs.eu_vat_number.as_deref()),
        address_street: normalize_optional(inputs.address_street.as_deref()),
        address_postal_code: normalize_optional(inputs.address_postal_code.as_deref()),
        address_city: normalize_optional(inputs.address_city.as_deref()),
        address_country: normalize_optional(inputs.address_country.as_deref()),
        bank_account: normalize_optional(inputs.bank_account.as_deref()),
        contact_email: normalize_optional(inputs.contact_email.as_deref()),
        contact_phone: normalize_optional(inputs.contact_phone.as_deref()),
    }
}

struct NormalizedInputs {
    display_name: String,
    legal_name: String,
    kind: PartnerKind,
    tax_number: String,
    eu_vat_number: Option<String>,
    address_street: Option<String>,
    address_postal_code: Option<String>,
    address_city: Option<String>,
    address_country: Option<String>,
    bank_account: Option<String>,
    contact_email: Option<String>,
    contact_phone: Option<String>,
}

/// Insert a new partner row. Caller MUST have run `validate_partner_inputs`
/// first; this function does not re-validate. Returns the newly-created
/// row (with server-minted `id`, `created_at`, `updated_at`).
pub fn create_partner(conn: &Connection, tenant: &str, inputs: &PartnerInputs) -> Result<Partner> {
    ensure_schema(conn)?;
    let normalized = inputs_to_normalized(inputs);
    let id = PartnerId::new().to_prefixed_string();
    let now = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .context("format created_at as Rfc3339")?;
    conn.execute(
        "INSERT INTO partners (
            id, tenant_id, display_name, legal_name, kind, tax_number,
            eu_vat_number, address_street, address_postal_code, address_city,
            address_country, bank_account, contact_email, contact_phone,
            created_at, updated_at, deleted_at
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL);",
        params![
            &id,
            tenant,
            &normalized.display_name,
            &normalized.legal_name,
            normalized.kind.as_db_str(),
            &normalized.tax_number,
            normalized.eu_vat_number.as_deref(),
            normalized.address_street.as_deref(),
            normalized.address_postal_code.as_deref(),
            normalized.address_city.as_deref(),
            normalized.address_country.as_deref(),
            normalized.bank_account.as_deref(),
            normalized.contact_email.as_deref(),
            normalized.contact_phone.as_deref(),
            &now,
            &now,
        ],
    )
    .context("INSERT into partners")?;

    Ok(Partner {
        id,
        display_name: normalized.display_name,
        legal_name: normalized.legal_name,
        kind: normalized.kind,
        tax_number: normalized.tax_number,
        eu_vat_number: normalized.eu_vat_number,
        address_street: normalized.address_street,
        address_postal_code: normalized.address_postal_code,
        address_city: normalized.address_city,
        address_country: normalized.address_country,
        bank_account: normalized.bank_account,
        contact_email: normalized.contact_email,
        contact_phone: normalized.contact_phone,
        created_at: now.clone(),
        updated_at: now,
        deleted_at: None,
    })
}

/// Fetch a partner by id, scoped to the tenant. Returns `Ok(None)` if
/// the row does not exist OR if it has been soft-deleted; the HTTP
/// route maps both to 404.
pub fn get_partner(conn: &Connection, tenant: &str, id: &str) -> Result<Option<Partner>> {
    ensure_schema(conn)?;
    let mut stmt = conn.prepare(
        "SELECT id, display_name, legal_name, kind, tax_number,
                eu_vat_number, address_street, address_postal_code, address_city,
                address_country, bank_account, contact_email, contact_phone,
                created_at, updated_at, deleted_at
         FROM partners
         WHERE tenant_id = ? AND id = ? AND deleted_at IS NULL;",
    )?;
    let mut rows = stmt.query_map(params![tenant, id], row_to_partner)?;
    match rows.next() {
        Some(r) => Ok(Some(r??)),
        None => Ok(None),
    }
}

/// List active partners for the tenant. `search` is a case-insensitive
/// prefix filter applied to `display_name` OR `legal_name`. Result is
/// ordered by `display_name` ASC (the natural typeahead order).
pub fn list_partners(
    conn: &Connection,
    tenant: &str,
    search: Option<&str>,
) -> Result<Vec<Partner>> {
    ensure_schema(conn)?;
    let trimmed = search.map(|s| s.trim()).filter(|s| !s.is_empty());

    let mut out = Vec::new();
    match trimmed {
        Some(needle) => {
            let pattern = format!("{}%", needle.to_lowercase());
            let mut stmt = conn.prepare(
                "SELECT id, display_name, legal_name, kind, tax_number,
                        eu_vat_number, address_street, address_postal_code, address_city,
                        address_country, bank_account, contact_email, contact_phone,
                        created_at, updated_at, deleted_at
                 FROM partners
                 WHERE tenant_id = ?
                   AND deleted_at IS NULL
                   AND (LOWER(display_name) LIKE ? OR LOWER(legal_name) LIKE ?)
                 ORDER BY display_name ASC;",
            )?;
            let rows = stmt.query_map(params![tenant, &pattern, &pattern], row_to_partner)?;
            for r in rows {
                out.push(r??);
            }
        }
        None => {
            let mut stmt = conn.prepare(
                "SELECT id, display_name, legal_name, kind, tax_number,
                        eu_vat_number, address_street, address_postal_code, address_city,
                        address_country, bank_account, contact_email, contact_phone,
                        created_at, updated_at, deleted_at
                 FROM partners
                 WHERE tenant_id = ? AND deleted_at IS NULL
                 ORDER BY display_name ASC;",
            )?;
            let rows = stmt.query_map(params![tenant], row_to_partner)?;
            for r in rows {
                out.push(r??);
            }
        }
    }
    Ok(out)
}

/// Update an existing partner. Returns `Ok(None)` if the row does not
/// exist OR is soft-deleted (the HTTP route maps both to 404). Caller
/// MUST have run `validate_partner_inputs` first.
pub fn update_partner(
    conn: &Connection,
    tenant: &str,
    id: &str,
    inputs: &PartnerInputs,
) -> Result<Option<Partner>> {
    ensure_schema(conn)?;
    // Existence check before the UPDATE so we can distinguish
    // "no such row" (404) from "row exists but UPDATE matched zero
    // for some other reason" (500). DuckDB's UPDATE returns row-count
    // but not the post-update row, so a separate SELECT after the
    // UPDATE is the read-back path.
    if get_partner(conn, tenant, id)?.is_none() {
        return Ok(None);
    }

    let normalized = inputs_to_normalized(inputs);
    let now = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .context("format updated_at as Rfc3339")?;
    conn.execute(
        "UPDATE partners SET
            display_name        = ?,
            legal_name          = ?,
            kind                = ?,
            tax_number          = ?,
            eu_vat_number       = ?,
            address_street      = ?,
            address_postal_code = ?,
            address_city        = ?,
            address_country     = ?,
            bank_account        = ?,
            contact_email       = ?,
            contact_phone       = ?,
            updated_at          = ?
         WHERE tenant_id = ? AND id = ? AND deleted_at IS NULL;",
        params![
            &normalized.display_name,
            &normalized.legal_name,
            normalized.kind.as_db_str(),
            &normalized.tax_number,
            normalized.eu_vat_number.as_deref(),
            normalized.address_street.as_deref(),
            normalized.address_postal_code.as_deref(),
            normalized.address_city.as_deref(),
            normalized.address_country.as_deref(),
            normalized.bank_account.as_deref(),
            normalized.contact_email.as_deref(),
            normalized.contact_phone.as_deref(),
            &now,
            tenant,
            id,
        ],
    )
    .context("UPDATE partners")?;

    get_partner(conn, tenant, id)
}

/// Soft-delete a partner. Returns `Ok(true)` if a row was deleted,
/// `Ok(false)` if the row does not exist or was already deleted. The
/// row stays in the DB so historical-invoice lookups can still resolve
/// the buyer reference.
pub fn soft_delete_partner(conn: &Connection, tenant: &str, id: &str) -> Result<bool> {
    ensure_schema(conn)?;
    let now = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .context("format deleted_at as Rfc3339")?;
    let changed = conn
        .execute(
            "UPDATE partners SET deleted_at = ?, updated_at = ?
             WHERE tenant_id = ? AND id = ? AND deleted_at IS NULL;",
            params![&now, &now, tenant, id],
        )
        .context("UPDATE partners SET deleted_at")?;
    Ok(changed > 0)
}

fn row_to_partner(row: &duckdb::Row<'_>) -> duckdb::Result<Result<Partner>> {
    let id: String = row.get(0)?;
    let display_name: String = row.get(1)?;
    let legal_name: String = row.get(2)?;
    let kind_str: String = row.get(3)?;
    let tax_number: String = row.get(4)?;
    let eu_vat_number: Option<String> = row.get(5)?;
    let address_street: Option<String> = row.get(6)?;
    let address_postal_code: Option<String> = row.get(7)?;
    let address_city: Option<String> = row.get(8)?;
    let address_country: Option<String> = row.get(9)?;
    let bank_account: Option<String> = row.get(10)?;
    let contact_email: Option<String> = row.get(11)?;
    let contact_phone: Option<String> = row.get(12)?;
    let created_at: String = row.get(13)?;
    let updated_at: String = row.get(14)?;
    let deleted_at: Option<String> = row.get(15)?;

    let kind = match PartnerKind::from_db_str(&kind_str) {
        Some(k) => k,
        None => {
            return Ok(Err(anyhow::anyhow!(
                "partners.kind has unexpected value `{}` (expected Customer | Supplier | Both)",
                kind_str
            )));
        }
    };

    Ok(Ok(Partner {
        id,
        display_name,
        legal_name,
        kind,
        tax_number,
        eu_vat_number,
        address_street,
        address_postal_code,
        address_city,
        address_country,
        bank_account,
        contact_email,
        contact_phone,
        created_at,
        updated_at,
        deleted_at,
    }))
}

// ──────────────────────────────────────────────────────────────────────
// Domain unit tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    // ── PartnerKind serde round-trip ──────────────────────────────────

    #[test]
    fn partner_kind_serde_round_trip_pin() {
        // Each variant must round-trip through serde JSON as its
        // PascalCase literal. Mirrors the `InvoiceState` round-trip
        // discipline — variant-name drift between this enum and the
        // SPA's string-union mirror surfaces here first.
        for (variant, literal) in [
            (PartnerKind::Customer, "\"Customer\""),
            (PartnerKind::Supplier, "\"Supplier\""),
            (PartnerKind::Both, "\"Both\""),
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(
                json, literal,
                "PartnerKind::{:?} must emit {}",
                variant, literal
            );
            let back: PartnerKind = serde_json::from_str(&json).unwrap();
            assert_eq!(back, variant, "PartnerKind round-trip for {}", literal);
        }
    }

    // ── validate_tax_number ───────────────────────────────────────────

    #[test]
    fn validate_tax_number_accepts_canonical_shape() {
        assert!(validate_tax_number("12345678-1-42").is_ok());
        assert!(validate_tax_number("87654321-2-13").is_ok());
        // Surrounding whitespace tolerated — the validator trims.
        assert!(validate_tax_number("  12345678-1-42  ").is_ok());
    }

    #[test]
    fn validate_tax_number_rejects_empty() {
        assert!(validate_tax_number("").is_err());
        assert!(validate_tax_number("   ").is_err());
    }

    #[test]
    fn validate_tax_number_rejects_wrong_digit_counts() {
        // 7 digits, not 8
        assert!(validate_tax_number("1234567-1-42").is_err());
        // 9 digits, not 8
        assert!(validate_tax_number("123456789-1-42").is_err());
        // Missing VAT-code digit
        assert!(validate_tax_number("12345678--42").is_err());
        // 1 county digit, not 2
        assert!(validate_tax_number("12345678-1-4").is_err());
        // 3 county digits, not 2
        assert!(validate_tax_number("12345678-1-421").is_err());
    }

    #[test]
    fn validate_tax_number_rejects_non_digit_characters() {
        assert!(validate_tax_number("1234567X-1-42").is_err());
        assert!(validate_tax_number("12345678-X-42").is_err());
        assert!(validate_tax_number("12345678-1-XX").is_err());
    }

    #[test]
    fn validate_tax_number_rejects_missing_or_swapped_separators() {
        assert!(validate_tax_number("12345678 1 42").is_err());
        assert!(validate_tax_number("12345678/1/42").is_err());
        assert!(validate_tax_number("123456781-42").is_err());
    }

    // ── validate_partner_inputs ───────────────────────────────────────

    fn minimal_valid_inputs() -> PartnerInputs {
        PartnerInputs {
            display_name: "BSCE".to_string(),
            legal_name: "BSCE Kft.".to_string(),
            kind: PartnerKind::Customer,
            tax_number: "12345678-1-42".to_string(),
            eu_vat_number: None,
            address_street: None,
            address_postal_code: None,
            address_city: None,
            address_country: None,
            bank_account: None,
            contact_email: None,
            contact_phone: None,
        }
    }

    #[test]
    fn validate_partner_inputs_accepts_minimal_valid() {
        assert!(validate_partner_inputs(&minimal_valid_inputs()).is_ok());
    }

    #[test]
    fn validate_partner_inputs_surfaces_every_problem_at_once() {
        // All three required-field rules fail simultaneously — the
        // validator must surface all three errors, not short-circuit
        // at the first one. Pinned per the "fix-everything-in-one-pass"
        // operator UX (A157 inline error rendering).
        let bad = PartnerInputs {
            display_name: "   ".to_string(),
            legal_name: "".to_string(),
            kind: PartnerKind::Customer,
            tax_number: "not-a-tax-number".to_string(),
            ..minimal_valid_inputs()
        };
        let errors = validate_partner_inputs(&bad).expect_err("must reject");
        let fields: BTreeMap<&'static str, &str> = errors
            .iter()
            .map(|e| (e.field, e.message.as_str()))
            .collect();
        assert!(
            fields.contains_key("display_name"),
            "must flag display_name"
        );
        assert!(fields.contains_key("legal_name"), "must flag legal_name");
        assert!(fields.contains_key("tax_number"), "must flag tax_number");
    }

    // ── PartnerId prefix discipline ───────────────────────────────────

    #[test]
    fn partner_id_renders_with_prt_prefix() {
        let id = PartnerId::new().to_prefixed_string();
        assert!(
            id.starts_with("prt_"),
            "PartnerId must render as `prt_<ULID>`; got `{}`",
            id
        );
        // 4 chars prefix + 26-char ULID = 30 total
        assert_eq!(id.len(), 30, "prefixed PartnerId must be 30 chars");
    }
}
