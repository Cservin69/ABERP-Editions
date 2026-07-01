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

use crate::nav_xml::CustomerVatStatus;

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

/// S428 — closed-vocab customer segment that drives the margin profile (hence
/// the auto-quote price). `Unset` is the back-compat default: every pre-S428
/// partner backfills to it (the migration writes 'unset' for NULL), and a
/// quote for an `Unset` buyer uses the global default margin. The db-strings
/// MUST match `margin_profiles.customer_type` so the engine resolution can
/// join the two — invariant in code, not a SQL FK ([[no-sql-specific]]).
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone, Copy, Default)]
pub enum CustomerType {
    Industrial,
    Defense,
    Aerospace,
    Research,
    PrototypeShop,
    Oem,
    Consumer,
    #[default]
    Unset,
}

impl CustomerType {
    pub fn as_db_str(&self) -> &'static str {
        match self {
            CustomerType::Industrial => "industrial",
            CustomerType::Defense => "defense",
            CustomerType::Aerospace => "aerospace",
            CustomerType::Research => "research",
            CustomerType::PrototypeShop => "prototype_shop",
            CustomerType::Oem => "oem",
            CustomerType::Consumer => "consumer",
            CustomerType::Unset => "unset",
        }
    }

    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "industrial" => Some(CustomerType::Industrial),
            "defense" => Some(CustomerType::Defense),
            "aerospace" => Some(CustomerType::Aerospace),
            "research" => Some(CustomerType::Research),
            "prototype_shop" => Some(CustomerType::PrototypeShop),
            "oem" => Some(CustomerType::Oem),
            "consumer" => Some(CustomerType::Consumer),
            "unset" => Some(CustomerType::Unset),
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
    /// PR-97 / ADR-0048 — closed-vocab buyer-kind discriminant.
    /// Pre-PR-97 rows backfill to `Domestic` per the migration's
    /// `DEFAULT 'Domestic'` clause so existing data does not shift
    /// meaning. Drives whether [`Self::tax_number`] is required
    /// (Domestic) or forbidden (PrivatePerson) at the partner-form
    /// validation gate. `Other` is named in the closed vocab but
    /// v1-deferred per ADR-0048 §7 (the SPA disables the radio option
    /// with a v2 hint; if a wire body still carries `Other` the
    /// preflight loud-fails).
    pub customer_vat_status: CustomerVatStatus,
    /// S428 — closed-vocab customer segment driving the margin profile.
    /// Pre-S428 rows backfill to `Unset` (the migration writes 'unset'
    /// for NULL). Changing it is the one partner mutation that fires an
    /// audit row (`PartnerCustomerTypeChanged`).
    pub customer_type: CustomerType,
    /// PR-97 / ADR-0048 — nullable for non-Domestic statuses.
    /// `Domestic` requires `Some(_)` matching `xxxxxxxx-y-zz`;
    /// `PrivatePerson` requires `None` (or empty-trimmed); `Other`
    /// requires `None` today (v2 will introduce
    /// `community_vat_number` / `third_state_tax_id` siblings).
    pub tax_number: Option<String>,
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
    /// PR-97 / ADR-0048 (Ervin override 1) — `true` iff this partner
    /// has ≥1 issued invoice referencing it. Computed at read time as
    /// `issued_invoice_count > 0`. Drives the SPA's PartnerForm
    /// FIELD-SELECTIVE lock posture: when `true`, the operator can no
    /// longer edit `tax_number` or `customer_vat_status` (those two
    /// fields encode the partner's intrinsic legal identity; a change
    /// is effectively a new partner). Other fields (address, email,
    /// display_name, legal_name) stay editable — companies rename,
    /// move addresses, change emails.
    pub has_issued_invoices: bool,
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
    /// PR-97 / ADR-0048 — defaults to `Domestic` when omitted on the
    /// wire so pre-PR-97 SPA / CLI callers and integration tests still
    /// type-check; same back-compat shape as the wire-side
    /// [`crate::issue_invoice::CustomerJson::vat_status`].
    #[serde(default)]
    pub customer_vat_status: CustomerVatStatus,
    /// S428 — defaults to `Unset` when omitted on the wire so pre-S428
    /// callers and existing integration tests still type-check.
    #[serde(default)]
    pub customer_type: CustomerType,
    /// PR-97 / ADR-0048 — nullable for non-Domestic statuses. The
    /// partner-form validator (`validate_partner_inputs`) enforces the
    /// per-status invariant (required-when-Domestic,
    /// forbidden-when-PrivatePerson).
    #[serde(default)]
    pub tax_number: Option<String>,
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

// ── PR-98 — multi-email contact_email validator ───────────────────────

/// PR-98 — split an operator-typed `contact_email` value into the list
/// of individual addresses. Separators are any combination of comma
/// (`,`), semicolon (`;`), or whitespace. The canonical separator the
/// codebase emits on serialise back to TOML / wire / DB is `", "`
/// (comma+space), but the parser is tolerant on the way in.
///
/// Empty / whitespace-only input parses as `vec![]`. Each non-empty
/// token is trimmed; an empty token (two separators with nothing
/// between them) is skipped silently. The function does NOT validate
/// per-token shape — call [`validate_email_token`] for that.
pub fn parse_emails(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    for token in s.split(|c: char| c == ',' || c == ';' || c.is_whitespace()) {
        let t = token.trim();
        if !t.is_empty() {
            out.push(t.to_string());
        }
    }
    out
}

/// PR-98 — light-touch RFC-5322 sanity gate for a single email token.
/// The full RFC grammar is exotic (quoted local parts, comments,
/// IP-domain literals); we enforce the operator-typed shape the
/// invoice-buyer use case will hit: one `@`, non-empty local part,
/// non-empty domain with at least one `.`, no whitespace / list
/// separators / angle brackets / quotes. Lettre's `Address::new`
/// runs as the second line of defence at the send seam.
pub fn validate_email_token(token: &str) -> Result<(), String> {
    let t = token.trim();
    if t.is_empty() {
        return Err("email token is empty".to_string());
    }
    let bad_char =
        |c: char| c.is_whitespace() || matches!(c, ',' | ';' | '<' | '>' | '"' | '\r' | '\n');
    if let Some(c) = t.chars().find(|c| bad_char(*c)) {
        return Err(format!(
            "email `{t}` contains forbidden character `{c}` \
             (no whitespace, comma, semicolon, angle brackets, or quotes)"
        ));
    }
    let (local, domain) = match t.split_once('@') {
        Some((l, d)) => (l, d),
        None => return Err(format!("email `{t}` is missing `@`")),
    };
    if local.is_empty() {
        return Err(format!("email `{t}` has empty local part"));
    }
    if domain.is_empty() {
        return Err(format!("email `{t}` has empty domain"));
    }
    if !domain.contains('.') {
        return Err(format!("email `{t}` domain `{domain}` is missing `.`"));
    }
    if domain.starts_with('.') || domain.ends_with('.') {
        return Err(format!(
            "email `{t}` domain `{domain}` cannot start or end with `.`"
        ));
    }
    Ok(())
}

/// PR-98 — parse + validate `contact_email` as a list. Returns the
/// parsed tokens on success; a typed message naming the first
/// offending token on failure. Empty / whitespace-only input is OK
/// (returns `Ok(vec![])`) — `contact_email` is optional on the
/// partner record.
pub fn parse_and_validate_emails(s: &str) -> Result<Vec<String>, String> {
    let tokens = parse_emails(s);
    for t in &tokens {
        validate_email_token(t)?;
    }
    Ok(tokens)
}

/// PR-98 — re-emit a parsed email list in canonical `", "` form. Used
/// at the storage boundary so the DB row always carries the same
/// separator regardless of what the operator typed.
pub fn join_emails_canonical(addresses: &[String]) -> String {
    addresses.join(", ")
}

/// Validate all field-level rules; returns a `Vec` of errors so the SPA
/// can surface every problem at once rather than the operator fixing
/// them one-at-a-time across multiple round-trips.
///
/// Per CLAUDE.md rule 9: each branch pins a distinct rule.
///
/// PR-97 / ADR-0048 — `customer_vat_status` discriminator drives the
/// tax-number invariant: `Domestic` requires a well-formed
/// `xxxxxxxx-y-zz` Hungarian ADÓSZÁM; `PrivatePerson` requires the
/// field to be absent (`None` or empty-after-trim); `Other` is v1
/// named-deferred and surfaces a typed validation error pointing the
/// operator at the radio.
pub fn validate_partner_inputs(inputs: &PartnerInputs) -> Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();
    if inputs.display_name.trim().is_empty() {
        errors.push(ValidationError {
            field: "display_name",
            message: "display name is required".to_string(),
        });
    }
    // Session-148 (Ervin override 3) — `legal_name` is UNCONDITIONALLY
    // required for every customer type. The buyer name is mandatory on
    // the invoice per Áfa tv. §169 (ADR-0048 amendment, PR-104); the
    // PR-99 GDPR carve-out that let a PrivatePerson partner be saved
    // name-less is removed. A name-less partner produced a null buyer
    // name on the issued invoice that blocked issuance downstream;
    // requiring it here keeps the partner record the single source of a
    // valid buyer name. "forget GDPR, show the name, always."
    if inputs.legal_name.trim().is_empty() {
        errors.push(ValidationError {
            field: "legal_name",
            message: "A vevő neve kötelező a számlán (Áfa tv. §169) \
                      / Buyer name required per §169"
                .to_string(),
        });
    }
    // PR-98 — multi-email contact_email validator. Empty / whitespace-
    // only input is OK (the field is optional). A non-empty value
    // splits on `,` / `;` / whitespace; each token is sanity-gated
    // and the first malformed token fails the whole list. The DB
    // stores the canonical `", "`-joined form (see
    // `inputs_to_normalized` below).
    if let Some(raw) = inputs.contact_email.as_deref() {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            if let Err(msg) = parse_and_validate_emails(trimmed) {
                errors.push(ValidationError {
                    field: "contact_email",
                    message: msg,
                });
            }
        }
    }
    match inputs.customer_vat_status {
        CustomerVatStatus::Domestic => {
            // Pre-PR-97 invariant: ADÓSZÁM required and well-formed.
            let raw = inputs.tax_number.as_deref().unwrap_or("");
            if let Err(msg) = validate_tax_number(raw) {
                errors.push(ValidationError {
                    field: "tax_number",
                    message: msg,
                });
            }
        }
        CustomerVatStatus::PrivatePerson => {
            // Symmetric invariant: a natural-person partner MUST NOT
            // carry an ADÓSZÁM. The form disables the input under this
            // status; a non-empty value reaching this branch is either
            // operator confusion or a wire-bypass; either way surface
            // loud so the data does not silently regress on edit.
            if inputs
                .tax_number
                .as_deref()
                .is_some_and(|s| !s.trim().is_empty())
            {
                errors.push(ValidationError {
                    field: "tax_number",
                    message: "Magánszemély vevőhöz nem tartozhat adószám. \
                              Természetes személy partnernél hagyja üresen a mezőt. \
                              / Natural-person buyers must NOT carry a tax number."
                        .to_string(),
                });
            }
        }
        CustomerVatStatus::Other => {
            // v1 named-deferred per ADR-0048 §7. The SPA disables the
            // Külföldi radio option with an inline v2 hint; if a wire
            // body still arrives with Other (CLI / integration test
            // / non-SPA client) surface a typed validation error so the
            // operator sees the explicit "not yet supported" signal
            // rather than an opaque downstream NAV reject.
            errors.push(ValidationError {
                field: "customer_vat_status",
                message: "Külföldi vevő (OTHER) támogatása későbbi verzióban érkezik (v2). \
                          Jelenleg csak Adóalany / Magánszemély választható. \
                          / Foreign-buyer (OTHER) support is named-deferred to v2; \
                          please pick Domestic or PrivatePerson for now."
                    .to_string(),
            });
        }
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

// S410 / [[no-sql-specific]] — no DB-level CHECK on `kind`. The
// `PartnerKind` closed vocab is enforced in Rust: writes go through
// `PartnerKind::as_db_str` and reads reject out-of-vocab values via
// `PartnerKind::from_db_str`.
const PARTNERS_SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS partners (
    id                    VARCHAR NOT NULL PRIMARY KEY,
    tenant_id             VARCHAR NOT NULL,
    display_name          VARCHAR NOT NULL,
    legal_name            VARCHAR NOT NULL,
    kind                  VARCHAR NOT NULL,
    tax_number            VARCHAR,
    eu_vat_number         VARCHAR,
    address_street        VARCHAR,
    address_postal_code   VARCHAR,
    address_city          VARCHAR,
    address_country       VARCHAR,
    bank_account          VARCHAR,
    contact_email         VARCHAR,
    contact_phone         VARCHAR,
    customer_vat_status   VARCHAR NOT NULL DEFAULT 'Domestic',
    issued_invoice_count  BIGINT  NOT NULL DEFAULT 0,
    created_at            VARCHAR NOT NULL,
    updated_at            VARCHAR NOT NULL,
    deleted_at            VARCHAR
);
CREATE INDEX IF NOT EXISTS partners_tenant_deleted_idx
    ON partners (tenant_id, deleted_at);
CREATE INDEX IF NOT EXISTS partners_tenant_display_idx
    ON partners (tenant_id, display_name);
";

/// PR-97 / ADR-0048 — additive migration for pre-PR-97 partner tables.
/// Idempotent at boot per the PR-73a discipline: a hot-path call from
/// `ensure_schema` adds the new column when it does not already exist.
/// `ADD COLUMN IF NOT EXISTS` carries a `DEFAULT 'Domestic'` so every
/// pre-PR-97 row backfills cleanly without a separate UPDATE pass —
/// the implicit pre-PR-97 posture (\"every partner is a Hungarian
/// business buyer\") is preserved verbatim.
///
/// The pre-PR-97 `tax_number VARCHAR NOT NULL` constraint is RELAXED;
/// DuckDB does not have a portable `ALTER COLUMN ... DROP NOT NULL`
/// across all versions in our supply chain, so the relaxation is done
/// by app-layer invariant: the [`validate_partner_inputs`] gate is the
/// single source of truth for whether `tax_number` is required, and
/// new PrivatePerson rows insert empty-string / NULL through the
/// nullable-column code path. Existing rows that were inserted under
/// the prior NOT NULL discipline continue to satisfy whatever
/// constraint history their physical column carries; the migration
/// does not need to retroactively alter the column type on those
/// boots, because the inserts on this surface always go through the
/// validation layer first.
/// DuckDB does NOT support `ALTER TABLE ... ADD COLUMN ... NOT NULL
/// DEFAULT 'X'` (the parser rejects the constraint-on-ADD-COLUMN
/// shape with "Parser Error: Adding columns with constraints not yet
/// supported"). Workaround: add the column unconstrained, then
/// backfill NULLs in a follow-on UPDATE. Both statements are
/// idempotent — `ADD COLUMN IF NOT EXISTS` is a no-op on a post-PR-97
/// boot, and the UPDATE narrows by `customer_vat_status IS NULL` so a
/// post-backfill row is untouched. The not-null + closed-vocab
/// invariant lives in [`validate_partner_inputs`] at the application
/// layer per ADR-0048 §"Open question #3" answer.
const PARTNERS_PR97_MIGRATION_SQL: &str = "
ALTER TABLE partners
    ADD COLUMN IF NOT EXISTS customer_vat_status VARCHAR;
UPDATE partners
    SET customer_vat_status = 'Domestic'
    WHERE customer_vat_status IS NULL;
ALTER TABLE partners
    ADD COLUMN IF NOT EXISTS issued_invoice_count BIGINT;
UPDATE partners
    SET issued_invoice_count = 0
    WHERE issued_invoice_count IS NULL;
";

/// S361 / PR-48 (ADR-0078) — additive Approved-Vendor-List (AVL) overlay
/// columns on the partner record, the data-model half of the `supplier.*`
/// audit family. All four are nullable and carry NO SQL `DEFAULT` — the same
/// DuckDB DEFAULT-on-replay trap the S357 `quoting_materials` migration pins:
/// `ADD COLUMN IF NOT EXISTS … DEFAULT V` re-applies `V` on every replay, and
/// `ensure_schema` runs at the top of every writer, so a DEFAULT-bearing
/// column would be clobbered on every unrelated partner `set_*` call. NULL is
/// the "not yet captured" sentinel (no DPAS rating / not yet screened); the
/// future firing site (later session — AVL CRUD does not exist yet) interprets
/// NULL in the app layer.
///
/// - `dpas_rating VARCHAR` — the DPAS priority the supplier is approved to
///   service, written by the `supplier.dpas_priority_set` firing site (later
///   session — no production writer exists yet). The future write boundary MUST
///   route the value through [`aberp_compliance::avl::DpasRating::parse`] (15
///   CFR 700.12 form `<DO|DX>-<program symbol>`, e.g. `DO-A1`), NOT a DB CHECK
///   (per [[no-sql-specific]]); unrated suppliers store NULL. (S366 review F13:
///   the rating model was remodelled in S367 from the old closed `DO-C1` /
///   `DX-C1` enum.)
/// - `eccn VARCHAR` — the supplier's product Export Control Classification
///   Number (EAR / Commerce Control List). An ECCN is a structured but open
///   vocabulary (`7A994`, `EAR99`, …) the classification service determines; the
///   future write boundary validates its *shape* through
///   [`aberp_compliance::export_control::validate_eccn`] (a 5-char
///   `[0-9][A-E][0-9]{3}` code or the literal `EAR99`), never a closed enum.
/// - `export_screening_status VARCHAR` — the stored denial-list screening
///   outcome, validated against the
///   [`aberp_compliance::avl::ExportScreeningStatus`] `as_str` vocab
///   (`not_screened` / `clear` / `hit` / `inconclusive`) at the write boundary.
/// - `export_screened_at VARCHAR` — RFC3339 stamp of the last screen. VARCHAR
///   (not a SQL `TIMESTAMP`) to match this table's `created_at` / `updated_at`
///   convention (every timestamp on `partners` is a `VARCHAR NOT NULL`
///   RFC3339 string); keeping one timestamp representation per table beats
///   matching the brief's loose "timestamp" wording. Flagged in the PR report
///   (the same S357 `cert_attached_at` flag).
const PARTNERS_S361_AVL_MIGRATION_SQL: &str = "
ALTER TABLE partners
    ADD COLUMN IF NOT EXISTS dpas_rating VARCHAR;
ALTER TABLE partners
    ADD COLUMN IF NOT EXISTS eccn VARCHAR;
ALTER TABLE partners
    ADD COLUMN IF NOT EXISTS export_screening_status VARCHAR;
ALTER TABLE partners
    ADD COLUMN IF NOT EXISTS export_screened_at VARCHAR;
";

/// S428 — additive `customer_type` column. Same DuckDB-DEFAULT-on-replay
/// trap the PR-97 migration documents: `ADD COLUMN IF NOT EXISTS … DEFAULT V`
/// re-applies `V` on every `ensure_schema` (which runs at the top of every
/// writer), so the column is added unconstrained and a follow-on UPDATE
/// backfills NULL → 'unset'. The closed-vocab invariant lives in
/// [`CustomerType::from_db_str`] at the app layer ([[no-sql-specific]]).
const PARTNERS_S428_CUSTOMER_TYPE_MIGRATION_SQL: &str = "
ALTER TABLE partners
    ADD COLUMN IF NOT EXISTS customer_type VARCHAR;
UPDATE partners
    SET customer_type = 'unset'
    WHERE customer_type IS NULL;
";

/// Idempotent `CREATE TABLE IF NOT EXISTS` + PR-97 additive migration
/// for the partners table. Callers (HTTP route handlers, tests) hit
/// this on every entry so a fresh tenant DB picks up the schema lazily
/// — same posture as `aberp_billing::DuckDbBillingStore::ensure_schema`
/// / `aberp_audit_ledger::ensure_schema`.
pub fn ensure_schema(conn: &Connection) -> Result<()> {
    // ADR-0098 C2 fix-forward — no-op on a read-only conn (read_returns_readonly
    // read()-side); the schema is created by a writer before any read reaches
    // here. A genuine write mis-routed through read() still fails loud (F5).
    if aberp_audit_ledger::connection_is_read_only(conn) {
        return Ok(());
    }
    conn.execute_batch(PARTNERS_SCHEMA_SQL)
        .context("ensure partners base schema")?;
    // PR-97 / ADR-0048 — additive `ADD COLUMN IF NOT EXISTS` for
    // pre-PR-97 partner tables (PR-48α onwards). Idempotent: on a
    // post-PR-97 boot the ALTER is a no-op.
    conn.execute_batch(PARTNERS_PR97_MIGRATION_SQL)
        .context("apply PR-97 partners migration (customer_vat_status)")?;
    // S361 / PR-48 — additive AVL overlay columns. Idempotent on a post-S361
    // boot (each ALTER is `IF NOT EXISTS`); fills pre-S361 rows with NULL (no
    // DPAS rating / not yet screened — the "not yet on the AVL" state).
    conn.execute_batch(PARTNERS_S361_AVL_MIGRATION_SQL)
        .context("apply S361 partners AVL migration (dpas/eccn/screening)")?;
    // S428 — additive `customer_type` column. Idempotent on a post-S428
    // boot (ALTER is IF NOT EXISTS); fills pre-S428 rows with 'unset'.
    conn.execute_batch(PARTNERS_S428_CUSTOMER_TYPE_MIGRATION_SQL)
        .context("apply S428 partners customer_type migration")?;
    Ok(())
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
        customer_vat_status: inputs.customer_vat_status,
        customer_type: inputs.customer_type,
        // PR-97 / ADR-0048 — `tax_number` is `Option<String>`. The
        // normaliser trims; for PrivatePerson the field is stored as
        // `None` even if the input arrived as an empty-trimmed string.
        tax_number: normalize_optional(inputs.tax_number.as_deref()),
        eu_vat_number: normalize_optional(inputs.eu_vat_number.as_deref()),
        address_street: normalize_optional(inputs.address_street.as_deref()),
        address_postal_code: normalize_optional(inputs.address_postal_code.as_deref()),
        address_city: normalize_optional(inputs.address_city.as_deref()),
        address_country: normalize_optional(inputs.address_country.as_deref()),
        bank_account: normalize_optional(inputs.bank_account.as_deref()),
        // PR-98 — multi-email canonical normalisation. Operator may
        // type addresses separated by comma / semicolon / whitespace;
        // storage uses the canonical `", "` separator. Validator
        // already ran on the caller side; if a malformed token slips
        // through, fall back to the raw trimmed string so we do not
        // silently drop the operator's input.
        contact_email: normalize_emails(inputs.contact_email.as_deref()),
        contact_phone: normalize_optional(inputs.contact_phone.as_deref()),
    }
}

/// PR-98 — normalise `contact_email` to canonical comma+space form for
/// storage. Empty input collapses to `None`. Returns the original
/// trimmed string if parsing yields zero tokens (defence in depth: the
/// caller-side validator should have caught this).
fn normalize_emails(s: Option<&str>) -> Option<String> {
    let raw = match s {
        Some(v) => v.trim(),
        None => return None,
    };
    if raw.is_empty() {
        return None;
    }
    let tokens = parse_emails(raw);
    if tokens.is_empty() {
        return Some(raw.to_string());
    }
    Some(join_emails_canonical(&tokens))
}

struct NormalizedInputs {
    display_name: String,
    legal_name: String,
    kind: PartnerKind,
    customer_vat_status: CustomerVatStatus,
    customer_type: CustomerType,
    tax_number: Option<String>,
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
            customer_vat_status, issued_invoice_count, created_at, updated_at, deleted_at,
            customer_type
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, ?, ?, NULL, ?);",
        params![
            &id,
            tenant,
            &normalized.display_name,
            &normalized.legal_name,
            normalized.kind.as_db_str(),
            normalized.tax_number.as_deref(),
            normalized.eu_vat_number.as_deref(),
            normalized.address_street.as_deref(),
            normalized.address_postal_code.as_deref(),
            normalized.address_city.as_deref(),
            normalized.address_country.as_deref(),
            normalized.bank_account.as_deref(),
            normalized.contact_email.as_deref(),
            normalized.contact_phone.as_deref(),
            normalized.customer_vat_status.as_db_str(),
            &now,
            &now,
            normalized.customer_type.as_db_str(),
        ],
    )
    .context("INSERT into partners")?;

    Ok(Partner {
        id,
        display_name: normalized.display_name,
        legal_name: normalized.legal_name,
        kind: normalized.kind,
        customer_vat_status: normalized.customer_vat_status,
        customer_type: normalized.customer_type,
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
        // Fresh partner — never issued an invoice yet.
        has_issued_invoices: false,
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
                customer_vat_status, issued_invoice_count, created_at, updated_at, deleted_at,
                customer_type
         FROM partners
         WHERE tenant_id = ? AND id = ? AND deleted_at IS NULL;",
    )?;
    let mut rows = stmt.query_map(params![tenant, id], row_to_partner)?;
    match rows.next() {
        Some(r) => Ok(Some(r??)),
        None => Ok(None),
    }
}

/// PR-92 — fetch a partner by tax number, scoped to the tenant. Used
/// by the SMTP email path to look up the buyer's contact email after
/// finding the customer's tax number in the `InvoiceDraftCreated`
/// audit payload. Returns `Ok(None)` if no partner matches (the
/// invoice was issued against an off-list buyer — the send path then
/// surfaces a `MissingRecipient` error rather than a fallback).
pub fn find_partner_by_tax_number(
    conn: &Connection,
    tenant: &str,
    tax_number: &str,
) -> Result<Option<Partner>> {
    ensure_schema(conn)?;
    let mut stmt = conn.prepare(
        "SELECT id, display_name, legal_name, kind, tax_number,
                eu_vat_number, address_street, address_postal_code, address_city,
                address_country, bank_account, contact_email, contact_phone,
                customer_vat_status, issued_invoice_count, created_at, updated_at, deleted_at,
                customer_type
         FROM partners
         WHERE tenant_id = ? AND tax_number = ? AND deleted_at IS NULL
         LIMIT 1;",
    )?;
    let mut rows = stmt.query_map(params![tenant, tax_number], row_to_partner)?;
    match rows.next() {
        Some(r) => Ok(Some(r??)),
        None => Ok(None),
    }
}

/// S196 / PR-196 — find a `PrivatePerson` partner by `(legal_name,
/// address_country, address_postal_code, address_city, address_street)`
/// tuple. Used by the NAV-as-DR restore wizard's catalog-extraction
/// pass to dedupe natural-person buyers that lack a tax_number (so
/// [`find_partner_by_tax_number`] cannot key on them). All five
/// components are matched case-insensitively after trim — same
/// canonicalisation the extract module applies to NAV-XML candidates
/// before issuing this lookup, so a wire body that varies only in
/// casing or surrounding whitespace dedupes cleanly.
///
/// NULL columns on the DB side compare equal to the NULL-equivalent
/// input (`None`) via the `IS NOT DISTINCT FROM` predicate — DuckDB
/// supports this directly. A trimmed-empty input is mapped to NULL
/// at the caller in the extract module so storage-side NULLs survive
/// the round-trip.
pub fn find_partner_by_name_and_address(
    conn: &Connection,
    tenant: &str,
    legal_name: &str,
    address_country: Option<&str>,
    address_postal_code: Option<&str>,
    address_city: Option<&str>,
    address_street: Option<&str>,
) -> Result<Option<Partner>> {
    ensure_schema(conn)?;
    let mut stmt = conn.prepare(
        "SELECT id, display_name, legal_name, kind, tax_number,
                eu_vat_number, address_street, address_postal_code, address_city,
                address_country, bank_account, contact_email, contact_phone,
                customer_vat_status, issued_invoice_count, created_at, updated_at, deleted_at,
                customer_type
         FROM partners
         WHERE tenant_id = ?
           AND deleted_at IS NULL
           AND LOWER(legal_name) = LOWER(?)
           AND LOWER(address_country)     IS NOT DISTINCT FROM LOWER(?)
           AND LOWER(address_postal_code) IS NOT DISTINCT FROM LOWER(?)
           AND LOWER(address_city)        IS NOT DISTINCT FROM LOWER(?)
           AND LOWER(address_street)      IS NOT DISTINCT FROM LOWER(?)
         LIMIT 1;",
    )?;
    let mut rows = stmt.query_map(
        params![
            tenant,
            legal_name,
            address_country,
            address_postal_code,
            address_city,
            address_street,
        ],
        row_to_partner,
    )?;
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
                        customer_vat_status, issued_invoice_count, created_at, updated_at, deleted_at,
                        customer_type
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
                        customer_vat_status, issued_invoice_count, created_at, updated_at, deleted_at,
                        customer_type
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
    // PR-97 / ADR-0048 (Ervin override 1) — read the existing row to
    // check the field-selective lock. If the partner has ≥1 issued
    // invoice, `tax_number` + `customer_vat_status` are FROZEN; the
    // UPDATE preserves the existing values for those two fields and
    // applies the operator's edits to everything else. Defence in
    // depth — the SPA disables the inputs, but a curl bypass cannot
    // mutate a partner's legal identity after invoicing.
    let existing = match get_partner(conn, tenant, id)? {
        Some(p) => p,
        None => return Ok(None),
    };

    let mut normalized = inputs_to_normalized(inputs);
    if existing.has_issued_invoices {
        // Freeze the two identity fields; everything else (address,
        // email, names) stays operator-editable.
        normalized.tax_number = existing.tax_number.clone();
        normalized.customer_vat_status = existing.customer_vat_status;
    }
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
            customer_vat_status = ?,
            customer_type       = ?,
            updated_at          = ?
         WHERE tenant_id = ? AND id = ? AND deleted_at IS NULL;",
        params![
            &normalized.display_name,
            &normalized.legal_name,
            normalized.kind.as_db_str(),
            normalized.tax_number.as_deref(),
            normalized.eu_vat_number.as_deref(),
            normalized.address_street.as_deref(),
            normalized.address_postal_code.as_deref(),
            normalized.address_city.as_deref(),
            normalized.address_country.as_deref(),
            normalized.bank_account.as_deref(),
            normalized.contact_email.as_deref(),
            normalized.contact_phone.as_deref(),
            normalized.customer_vat_status.as_db_str(),
            normalized.customer_type.as_db_str(),
            &now,
            tenant,
            id,
        ],
    )
    .context("UPDATE partners")?;

    get_partner(conn, tenant, id)
}

/// PR-97 / ADR-0048 (Ervin override 1) — increment a partner's
/// `issued_invoice_count` after an invoice has been issued against
/// it. Called from the issue path (`apps/aberp/src/issue_invoice.rs::
/// run_single_tx`) when the wire body carries a `customer.partnerId`
/// (the operator picked a saved partner via the typeahead). Idempotent
/// on the row level — the counter is monotonic and the SPA always
/// supplies the partner_id for SPA-typed invoices.
///
/// `Ok(false)` if no row matched (partner deleted between issue and
/// increment, or the partner_id was forged); `Ok(true)` if the
/// counter advanced. The issue path does not currently react to the
/// false outcome (the regulatory invoice has already been issued —
/// the counter is a UX-lock signal, not a data-integrity invariant).
///
/// Chain operations (storno / modification) do NOT call this —
/// they're modifications of an existing invoice, not net-new
/// issuances against the partner.
pub fn increment_issued_invoice_count(
    conn: &Connection,
    tenant: &str,
    partner_id: &str,
) -> Result<bool> {
    ensure_schema(conn)?;
    let changed = conn
        .execute(
            "UPDATE partners
                SET issued_invoice_count = issued_invoice_count + 1
                WHERE tenant_id = ? AND id = ? AND deleted_at IS NULL;",
            params![tenant, partner_id],
        )
        .context("UPDATE partners SET issued_invoice_count")?;
    Ok(changed > 0)
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
    // PR-97 / ADR-0048 — tax_number is now nullable on the column;
    // pre-PR-97 rows are guaranteed to hold a non-NULL string (the
    // prior NOT NULL column type), post-PR-97 PrivatePerson rows hold
    // NULL.
    let tax_number: Option<String> = row.get(4)?;
    let eu_vat_number: Option<String> = row.get(5)?;
    let address_street: Option<String> = row.get(6)?;
    let address_postal_code: Option<String> = row.get(7)?;
    let address_city: Option<String> = row.get(8)?;
    let address_country: Option<String> = row.get(9)?;
    let bank_account: Option<String> = row.get(10)?;
    let contact_email: Option<String> = row.get(11)?;
    let contact_phone: Option<String> = row.get(12)?;
    let customer_vat_status_str: String = row.get(13)?;
    let issued_invoice_count: i64 = row.get(14)?;
    let created_at: String = row.get(15)?;
    let updated_at: String = row.get(16)?;
    let deleted_at: Option<String> = row.get(17)?;
    // S428 — `customer_type` is ordinal-LAST (additive column). Pre-S428
    // rows backfill to 'unset' via the migration UPDATE.
    let customer_type_str: String = row.get(18)?;

    let kind = match PartnerKind::from_db_str(&kind_str) {
        Some(k) => k,
        None => {
            return Ok(Err(anyhow::anyhow!(
                "partners.kind has unexpected value `{}` (expected Customer | Supplier | Both)",
                kind_str
            )));
        }
    };
    let customer_vat_status = match CustomerVatStatus::from_db_str(&customer_vat_status_str) {
        Some(s) => s,
        None => {
            return Ok(Err(anyhow::anyhow!(
                "partners.customer_vat_status has unexpected value `{}` \
                 (expected Domestic | PrivatePerson | Other per ADR-0048)",
                customer_vat_status_str
            )));
        }
    };
    let customer_type = match CustomerType::from_db_str(&customer_type_str) {
        Some(t) => t,
        None => {
            return Ok(Err(anyhow::anyhow!(
                "partners.customer_type has unexpected value `{}` (expected one of \
                 industrial|defense|aerospace|research|prototype_shop|oem|consumer|unset)",
                customer_type_str
            )));
        }
    };

    Ok(Ok(Partner {
        id,
        display_name,
        legal_name,
        kind,
        customer_vat_status,
        customer_type,
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
        // PR-97 / ADR-0048 (Ervin override 1) — derive the lock flag
        // from the persisted counter. `issued_invoice_count >= 1`
        // marks the partner as locked.
        has_issued_invoices: issued_invoice_count > 0,
    }))
}

// ──────────────────────────────────────────────────────────────────────
// Domain unit tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    // ── S410 / [[no-sql-specific]] — closed-vocab gate lives in code ───
    /// The `CHECK (kind IN ('Customer','Supplier','Both'))` DDL constraint
    /// was dropped; this pins the read-side rejection that replaced it.
    #[test]
    fn partner_kind_from_db_str_rejects_out_of_vocab() {
        assert_eq!(
            PartnerKind::from_db_str("Customer"),
            Some(PartnerKind::Customer)
        );
        assert_eq!(
            PartnerKind::from_db_str("Supplier"),
            Some(PartnerKind::Supplier)
        );
        assert_eq!(PartnerKind::from_db_str("Both"), Some(PartnerKind::Both));
        // The dropped CHECK's job, now in code:
        assert_eq!(PartnerKind::from_db_str("customer"), None);
        assert_eq!(PartnerKind::from_db_str("Vendor"), None);
        assert_eq!(PartnerKind::from_db_str(""), None);
    }

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
            customer_vat_status: CustomerVatStatus::Domestic,
            customer_type: CustomerType::Unset,
            tax_number: Some("12345678-1-42".to_string()),
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
            tax_number: Some("not-a-tax-number".to_string()),
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

    // ── PR-97 / ADR-0048 — customer_vat_status conditional validation ──

    #[test]
    fn validate_partner_inputs_accepts_private_person_with_no_tax_number() {
        // PrivatePerson buyers MUST NOT carry an ADÓSZÁM — `None` is the
        // valid shape (the SPA form disables the input under this radio).
        let inputs = PartnerInputs {
            customer_vat_status: CustomerVatStatus::PrivatePerson,
            tax_number: None,
            display_name: "Kovács János".to_string(),
            legal_name: "Kovács János".to_string(),
            kind: PartnerKind::Customer,
            ..minimal_valid_inputs()
        };
        assert!(validate_partner_inputs(&inputs).is_ok());
    }

    #[test]
    fn validate_partner_inputs_accepts_private_person_with_empty_trimmed_tax_number() {
        // Empty-after-trim is treated the same as `None` — the
        // normaliser drops the empty string at the storage boundary.
        let inputs = PartnerInputs {
            customer_vat_status: CustomerVatStatus::PrivatePerson,
            tax_number: Some("   ".to_string()),
            display_name: "Kovács János".to_string(),
            legal_name: "Kovács János".to_string(),
            kind: PartnerKind::Customer,
            ..minimal_valid_inputs()
        };
        assert!(validate_partner_inputs(&inputs).is_ok());
    }

    #[test]
    fn validate_partner_inputs_rejects_private_person_with_populated_tax_number() {
        // Symmetric invariant: a PrivatePerson partner carrying a
        // populated ADÓSZÁM is operator confusion (or wire-bypass).
        // Loud-fail with a bilingual message; do NOT silently drop the
        // tax number on storage (that would silently misrepresent the
        // input).
        let inputs = PartnerInputs {
            customer_vat_status: CustomerVatStatus::PrivatePerson,
            tax_number: Some("12345678-1-42".to_string()),
            display_name: "Kovács János".to_string(),
            legal_name: "Kovács János".to_string(),
            kind: PartnerKind::Customer,
            ..minimal_valid_inputs()
        };
        let errors =
            validate_partner_inputs(&inputs).expect_err("PrivatePerson + tax_number must reject");
        assert!(
            errors.iter().any(|e| e.field == "tax_number"),
            "must flag tax_number; got {errors:?}"
        );
    }

    #[test]
    fn validate_partner_inputs_rejects_domestic_with_no_tax_number() {
        // Domestic buyers REQUIRE the ADÓSZÁM. `None` (and
        // empty-after-trim via the validator's trim) both fail the
        // gate per the pre-PR-97 invariant.
        let inputs = PartnerInputs {
            customer_vat_status: CustomerVatStatus::Domestic,
            tax_number: None,
            ..minimal_valid_inputs()
        };
        let errors =
            validate_partner_inputs(&inputs).expect_err("Domestic + None tax_number must reject");
        assert!(
            errors.iter().any(|e| e.field == "tax_number"),
            "must flag tax_number; got {errors:?}"
        );
    }

    // ── Session-148 (Ervin override 3) — legal_name UNCONDITIONALLY
    //    required for every customer_vat_status (§169) ──

    #[test]
    fn validate_partner_inputs_rejects_private_person_with_empty_legal_name() {
        // Session-148 — the PR-99 GDPR carve-out is removed. A
        // name-less PrivatePerson partner produced a null buyer name on
        // the issued invoice that blocked issuance; the buyer name is
        // mandatory per Áfa tv. §169 for ALL customer types, so the
        // validator must now REJECT an empty legal_name under
        // PrivatePerson with the bilingual §169 message.
        let inputs = PartnerInputs {
            customer_vat_status: CustomerVatStatus::PrivatePerson,
            tax_number: None,
            display_name: "Kovács János".to_string(),
            legal_name: "".to_string(),
            kind: PartnerKind::Customer,
            ..minimal_valid_inputs()
        };
        let errors = validate_partner_inputs(&inputs)
            .expect_err("PrivatePerson + empty legal_name must reject");
        let legal = errors
            .iter()
            .find(|e| e.field == "legal_name")
            .expect("must flag legal_name");
        assert!(
            legal.message.contains("§169"),
            "legal_name message must cite §169, got {}",
            legal.message
        );
    }

    #[test]
    fn validate_partner_inputs_rejects_private_person_with_whitespace_legal_name() {
        // Empty-after-trim is treated the same as truly empty per the
        // existing trim posture across the validator.
        let inputs = PartnerInputs {
            customer_vat_status: CustomerVatStatus::PrivatePerson,
            tax_number: None,
            display_name: "Kovács János".to_string(),
            legal_name: "   ".to_string(),
            kind: PartnerKind::Customer,
            ..minimal_valid_inputs()
        };
        assert!(
            validate_partner_inputs(&inputs).is_err(),
            "PrivatePerson + whitespace-only legal_name must reject"
        );
    }

    #[test]
    fn validate_partner_inputs_accepts_private_person_with_legal_name() {
        // Session-148 — happy path: a PrivatePerson partner WITH a
        // name (and no tax number) saves cleanly.
        let inputs = PartnerInputs {
            customer_vat_status: CustomerVatStatus::PrivatePerson,
            tax_number: None,
            display_name: "Teszt Magánszemély".to_string(),
            legal_name: "Teszt Magánszemély".to_string(),
            kind: PartnerKind::Customer,
            ..minimal_valid_inputs()
        };
        assert!(
            validate_partner_inputs(&inputs).is_ok(),
            "PrivatePerson + present legal_name must be accepted"
        );
    }

    #[test]
    fn validate_partner_inputs_rejects_domestic_with_empty_legal_name() {
        // Domestic STILL requires legal_name (unchanged by Session-148
        // — it was already required for non-PrivatePerson).
        let inputs = PartnerInputs {
            customer_vat_status: CustomerVatStatus::Domestic,
            tax_number: Some("12345678-1-42".to_string()),
            display_name: "BSCE".to_string(),
            legal_name: "".to_string(),
            kind: PartnerKind::Customer,
            ..minimal_valid_inputs()
        };
        let errors =
            validate_partner_inputs(&inputs).expect_err("Domestic + empty legal_name must reject");
        assert!(
            errors.iter().any(|e| e.field == "legal_name"),
            "must flag legal_name; got {errors:?}"
        );
    }

    #[test]
    fn validate_partner_inputs_rejects_other_status_in_v1() {
        // v1 named-defers OTHER per ADR-0048 §7; the gate fires a
        // typed validation error pointing at the radio.
        let inputs = PartnerInputs {
            customer_vat_status: CustomerVatStatus::Other,
            tax_number: None,
            ..minimal_valid_inputs()
        };
        let errors = validate_partner_inputs(&inputs)
            .expect_err("Other status must surface the v1-deferred error");
        assert!(
            errors.iter().any(|e| e.field == "customer_vat_status"),
            "must flag customer_vat_status; got {errors:?}"
        );
    }

    // ── PR-97 / ADR-0048 (Ervin override 1) — field-selective lock ──

    /// `increment_issued_invoice_count` advances the counter so a
    /// subsequent read returns `has_issued_invoices = true`. Pinned
    /// at the public-helper level so the v1 lock-detection contract
    /// is observable from outside the issue tx.
    #[test]
    fn increment_issued_invoice_count_flips_has_issued_invoices_flag() {
        let conn = Connection::open_in_memory().expect("in-memory DuckDB");
        ensure_schema(&conn).expect("schema");
        let tenant = "test-tenant";
        let p = create_partner(&conn, tenant, &minimal_valid_inputs()).expect("create");
        assert!(
            !p.has_issued_invoices,
            "fresh partner must report has_issued_invoices = false"
        );

        let advanced =
            increment_issued_invoice_count(&conn, tenant, &p.id).expect("increment counter");
        assert!(advanced, "counter must advance for an existing row");

        let after = get_partner(&conn, tenant, &p.id)
            .expect("get partner")
            .expect("partner present");
        assert!(
            after.has_issued_invoices,
            "post-increment read must report has_issued_invoices = true"
        );
    }

    /// `update_partner` on a LOCKED partner (≥1 issued invoice)
    /// preserves the two identity fields verbatim — even if the
    /// operator-supplied inputs ask for different values. Other
    /// fields (display_name, legal_name, address, etc.) DO apply.
    /// Defence in depth: the SPA disables the inputs, but a curl
    /// bypass cannot mutate the locked fields.
    #[test]
    fn update_partner_freezes_tax_number_and_vat_status_post_issuance() {
        let conn = Connection::open_in_memory().expect("in-memory DuckDB");
        ensure_schema(&conn).expect("schema");
        let tenant = "test-tenant";
        let original = create_partner(&conn, tenant, &minimal_valid_inputs()).expect("create");
        increment_issued_invoice_count(&conn, tenant, &original.id).expect("increment");

        // Operator tries to change BOTH locked fields AND a free
        // field (display_name). Backend must reject the locked
        // mutations + accept the free ones.
        let attempted_inputs = PartnerInputs {
            display_name: "Renamed Co".to_string(),
            legal_name: "Renamed Co Kft.".to_string(),
            kind: PartnerKind::Customer,
            customer_vat_status: CustomerVatStatus::PrivatePerson, // attempted change
            tax_number: Some("99999999-9-99".to_string()),         // attempted change
            ..minimal_valid_inputs()
        };
        let updated = update_partner(&conn, tenant, &original.id, &attempted_inputs)
            .expect("update accepts free fields")
            .expect("row present");

        assert_eq!(
            updated.tax_number, original.tax_number,
            "post-issuance update must NOT mutate tax_number"
        );
        assert_eq!(
            updated.customer_vat_status, original.customer_vat_status,
            "post-issuance update must NOT mutate customer_vat_status"
        );
        assert_eq!(
            updated.display_name, "Renamed Co",
            "post-issuance update MUST apply display_name change"
        );
        assert_eq!(
            updated.legal_name, "Renamed Co Kft.",
            "post-issuance update MUST apply legal_name change"
        );
    }

    /// `update_partner` on an UN-LOCKED partner (zero issued invoices)
    /// applies ALL field changes including the two
    /// would-be-locked-after-issuance fields. The lock activates ONLY
    /// after the first issuance.
    #[test]
    fn update_partner_allows_full_edits_before_first_issuance() {
        let conn = Connection::open_in_memory().expect("in-memory DuckDB");
        ensure_schema(&conn).expect("schema");
        let tenant = "test-tenant";
        let original = create_partner(&conn, tenant, &minimal_valid_inputs()).expect("create");
        // No invoice issued — counter stays at 0.

        let new_inputs = PartnerInputs {
            customer_vat_status: CustomerVatStatus::PrivatePerson,
            tax_number: None,
            display_name: "Reclassified".to_string(),
            legal_name: "Reclassified Person".to_string(),
            kind: PartnerKind::Customer,
            ..minimal_valid_inputs()
        };
        let updated = update_partner(&conn, tenant, &original.id, &new_inputs)
            .expect("update")
            .expect("present");
        assert_eq!(
            updated.customer_vat_status,
            CustomerVatStatus::PrivatePerson
        );
        assert_eq!(updated.tax_number, None);
        assert_eq!(updated.display_name, "Reclassified");
    }

    // ── PR-98 — multi-email contact_email parser/validator ────────────

    #[test]
    fn parse_emails_splits_on_comma_semicolon_and_whitespace() {
        // Each of the three separators must work in isolation AND in
        // combination. Operator may paste a contact list from any
        // upstream source; the parser is forgiving.
        let a = parse_emails("a@example.com,b@example.com,c@example.com");
        assert_eq!(a, vec!["a@example.com", "b@example.com", "c@example.com"]);
        let b = parse_emails("a@example.com b@example.com\tc@example.com");
        assert_eq!(b, vec!["a@example.com", "b@example.com", "c@example.com"]);
        let c = parse_emails("a@example.com;b@example.com;c@example.com");
        assert_eq!(c, vec!["a@example.com", "b@example.com", "c@example.com"]);
        // Mixed separators with surplus whitespace.
        let d = parse_emails("  a@example.com, b@example.com;\tc@example.com ");
        assert_eq!(d, vec!["a@example.com", "b@example.com", "c@example.com"]);
    }

    #[test]
    fn parse_emails_returns_empty_for_blank_input() {
        assert_eq!(parse_emails(""), Vec::<String>::new());
        assert_eq!(parse_emails("   "), Vec::<String>::new());
        assert_eq!(parse_emails(",,,"), Vec::<String>::new());
    }

    #[test]
    fn validate_email_token_accepts_canonical_shapes() {
        assert!(validate_email_token("buyer@example.com").is_ok());
        assert!(validate_email_token("first.last+tag@sub.example.co.uk").is_ok());
        assert!(validate_email_token("a@b.io").is_ok());
    }

    #[test]
    fn validate_email_token_rejects_malformed_input() {
        // Missing @
        assert!(validate_email_token("not-an-email").is_err());
        // Empty parts
        assert!(validate_email_token("@example.com").is_err());
        assert!(validate_email_token("local@").is_err());
        // No `.` in domain
        assert!(validate_email_token("local@example").is_err());
        // Edge dots in domain
        assert!(validate_email_token("local@.example.com").is_err());
        assert!(validate_email_token("local@example.com.").is_err());
        // Whitespace / list separators inside the token
        assert!(validate_email_token("buyer @example.com").is_err());
        assert!(validate_email_token("buyer,extra@example.com").is_err());
        assert!(validate_email_token("buyer;extra@example.com").is_err());
        assert!(validate_email_token("<buyer@example.com>").is_err());
    }

    #[test]
    fn parse_and_validate_emails_rejects_the_whole_list_on_first_bad_token() {
        let err = parse_and_validate_emails("ok@example.com, not-an-email, also@example.com")
            .expect_err("malformed middle token must fail the list");
        assert!(
            err.contains("not-an-email"),
            "error must name the offending token; got: {err}"
        );
    }

    #[test]
    fn parse_and_validate_emails_accepts_an_empty_or_blank_input() {
        assert_eq!(parse_and_validate_emails("").unwrap(), Vec::<String>::new());
        assert_eq!(
            parse_and_validate_emails("   ").unwrap(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn validate_partner_inputs_flags_malformed_contact_email() {
        let inputs = PartnerInputs {
            contact_email: Some("ok@example.com, not-an-email".to_string()),
            ..minimal_valid_inputs()
        };
        let errors = validate_partner_inputs(&inputs).expect_err("must reject malformed list");
        assert!(
            errors.iter().any(|e| e.field == "contact_email"),
            "must flag contact_email; got {errors:?}"
        );
    }

    #[test]
    fn validate_partner_inputs_accepts_a_multi_email_list() {
        let inputs = PartnerInputs {
            contact_email: Some("a@example.com, b@example.com; c@example.com".to_string()),
            ..minimal_valid_inputs()
        };
        assert!(validate_partner_inputs(&inputs).is_ok());
    }

    #[test]
    fn normalize_emails_emits_canonical_comma_space_form() {
        let n = normalize_emails(Some("a@example.com;b@example.com  c@example.com"));
        assert_eq!(
            n,
            Some("a@example.com, b@example.com, c@example.com".to_string())
        );
        // Single-entry round-trip is unchanged shape-wise.
        let n_one = normalize_emails(Some("buyer@example.com"));
        assert_eq!(n_one, Some("buyer@example.com".to_string()));
        // Empty input collapses to None.
        assert_eq!(normalize_emails(Some("   ")), None);
        assert_eq!(normalize_emails(None), None);
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

    // ── S164 — email recipient lookup must key on partner_id ──────────

    /// A PrivatePerson partner carries NO tax_number (ADR-0048) yet may
    /// hold one or more contact emails. The pre-S164 email send path
    /// looked the partner up by tax_number, so it could NEVER find a
    /// PrivatePerson buyer and loud-failed "no contact email" even when
    /// emails were configured. This pins the fix: the partner is found
    /// by its durable id (the key `resolve_recipient_email` now uses),
    /// its multi-address `contact_email` parses to BOTH addresses, AND
    /// the old tax-number lookup with the empty buyer tax string finds
    /// nothing (documenting why the old path failed).
    #[test]
    fn private_person_recipient_found_by_id_not_by_empty_tax_number() {
        let conn = Connection::open_in_memory().expect("in-memory DuckDB");
        ensure_schema(&conn).expect("schema");
        let tenant = "test-tenant";

        let inputs = PartnerInputs {
            display_name: "Szilvi férje".to_string(),
            legal_name: "Szilvi férje".to_string(),
            kind: PartnerKind::Customer,
            customer_vat_status: CustomerVatStatus::PrivatePerson,
            tax_number: None,
            contact_email: Some("ervin@aben.ch, ervin.csengeri@gmail.com".to_string()),
            ..minimal_valid_inputs()
        };
        let created =
            create_partner(&conn, tenant, &inputs).expect("create private-person partner");
        assert!(
            created.tax_number.is_none(),
            "PrivatePerson partner must persist with no tax_number"
        );

        // The S164 lookup key — by durable id — finds the partner and
        // its full multi-address contact email.
        let by_id = get_partner(&conn, tenant, &created.id)
            .expect("get_partner by id")
            .expect("partner present by id");
        let emails = parse_emails(
            by_id
                .contact_email
                .as_deref()
                .expect("contact_email present"),
        );
        assert_eq!(
            emails,
            vec![
                "ervin@aben.ch".to_string(),
                "ervin.csengeri@gmail.com".to_string()
            ],
            "both configured addresses must parse out of contact_email"
        );

        // The pre-S164 lookup key — by tax_number — fails: a
        // PrivatePerson buyer's tax string is empty, and no row matches.
        let by_empty_tax =
            find_partner_by_tax_number(&conn, tenant, "").expect("tax lookup must not error");
        assert!(
            by_empty_tax.is_none(),
            "empty buyer tax_number must NOT resolve the PrivatePerson partner — \
             this is the bug S164 fixes by keying on partner_id"
        );
    }

    // ── S361 / PR-48 (ADR-0078) — AVL overlay migration ──────────────────

    /// The additive AVL migration is idempotent: `ensure_schema` (which runs
    /// `PARTNERS_S361_AVL_MIGRATION_SQL` after the `CREATE TABLE IF NOT EXISTS`)
    /// may be called any number of times without error, and — critically — a
    /// value written to one of the new columns SURVIVES a subsequent
    /// `ensure_schema` call. The survival assertion is the real teeth: it proves
    /// the columns carry NO SQL `DEFAULT`, so the DuckDB DEFAULT-on-replay trap
    /// (which would clobber the value on every unrelated writer's `ensure_schema`
    /// call) does not fire. A "no error" check alone could pass while the trap
    /// silently reset the data. Mirrors the S357 `quoting_materials` pattern.
    #[test]
    fn s361_avl_migration_is_idempotent_and_does_not_clobber() {
        use aberp_compliance::avl::{DpasPriority, DpasRating, ExportScreeningStatus};

        let conn = Connection::open_in_memory().expect("in-memory DuckDB");
        // First ensure_schema runs the migration once.
        ensure_schema(&conn).expect("first ensure_schema");
        // Running it again must be a no-op (each ALTER is `IF NOT EXISTS`).
        ensure_schema(&conn).expect("second ensure_schema is a no-op");

        let tenant = "test-tenant";
        let p = create_partner(&conn, tenant, &minimal_valid_inputs()).expect("create");

        // Populate the new AVL columns directly (no firing site exists yet —
        // this stands in for the future writer) with canonical-form values.
        conn.execute(
            "UPDATE partners
             SET dpas_rating = ?,
                 eccn = ?,
                 export_screening_status = ?,
                 export_screened_at = ?
             WHERE tenant_id = ? AND id = ?;",
            params![
                DpasRating::new(DpasPriority::Dx, "A1").unwrap().as_str(),
                "7A994",
                ExportScreeningStatus::Hit.as_str(),
                "2026-06-12T10:00:00Z",
                tenant,
                &p.id
            ],
        )
        .expect("populate AVL columns");

        // Replay the migration twice more — the DEFAULT-on-replay trap, if it
        // existed, would clobber the values here.
        ensure_schema(&conn).expect("third ensure_schema");
        ensure_schema(&conn).expect("fourth ensure_schema");

        let (dpas, eccn, status, at): (
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT dpas_rating, eccn, export_screening_status, export_screened_at
                 FROM partners WHERE tenant_id = ? AND id = ?;",
                params![tenant, &p.id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .expect("read back AVL columns");
        assert_eq!(dpas.as_deref(), Some("DX-A1"), "dpas_rating clobbered");
        assert_eq!(eccn.as_deref(), Some("7A994"), "eccn clobbered");
        assert_eq!(
            status.as_deref(),
            Some("hit"),
            "export_screening_status clobbered"
        );
        assert_eq!(
            at.as_deref(),
            Some("2026-06-12T10:00:00Z"),
            "export_screened_at clobbered"
        );
        // The stored strings round-trip back through the validated newtypes —
        // proving the column only ever holds well-formed values.
        assert_eq!(
            DpasRating::parse(&dpas.unwrap()).expect("valid dpas"),
            DpasRating::new(DpasPriority::Dx, "A1").unwrap()
        );
        assert_eq!(
            ExportScreeningStatus::from_storage_str(&status.unwrap()).expect("valid status"),
            ExportScreeningStatus::Hit
        );
    }

    /// A freshly-created partner reads NULL on all four AVL columns — the
    /// documented "not yet on the AVL" sentinel (no DPAS rating, never
    /// screened). Confirms the migration adds the columns without a backfill.
    #[test]
    fn s361_fresh_partner_reads_null_avl_columns() {
        let conn = Connection::open_in_memory().expect("in-memory DuckDB");
        ensure_schema(&conn).expect("schema");
        let tenant = "test-tenant";
        let p = create_partner(&conn, tenant, &minimal_valid_inputs()).expect("create");

        let null_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM partners
                 WHERE tenant_id = ? AND id = ?
                   AND dpas_rating IS NULL
                   AND eccn IS NULL
                   AND export_screening_status IS NULL
                   AND export_screened_at IS NULL;",
                params![tenant, &p.id],
                |r| r.get(0),
            )
            .expect("count untouched row");
        assert_eq!(
            null_count, 1,
            "a fresh partner must read NULL on all four AVL columns"
        );
    }
}
