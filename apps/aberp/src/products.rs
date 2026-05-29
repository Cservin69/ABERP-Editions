//! Products module — operator-managed catalog master-data (PR-91).
//!
//! # Scope
//!
//! Second master-data entity after Partners (PR-48α). The operator
//! manages a per-tenant catalog of saleable items: name + unit of
//! measure + currency + set price. The catalog is the seed for future
//! invoice-line autofill (currently out of scope — IssueInvoice does
//! not yet read this table) and the eventual inventory / AP modules.
//!
//! # The unit-of-measure model — load-bearing
//!
//! NAV's v3.0 InvoiceData schema requires every `<line>` to carry a
//! `<unitOfMeasure>` element whose body is one of a closed enum of
//! tokens, OR the literal `OWN` paired with a `<unitOfMeasureOwn>`
//! free-text element. The product's unit MUST map to that wire shape
//! cleanly so a future "pick product → autofill line" feature can hand
//! the operator's catalog entry straight to the NAV emitter.
//!
//! Model:
//!
//!   - [`NavUnitOfMeasure`] — closed-vocab enum mirroring the NAV
//!     tokens. PIECE / KILOGRAM / TON / KWH / DAY / HOUR / MINUTE /
//!     MONTH / LITER / KILOMETER / CUBIC_METER / METER / LINEAR_METER
//!     / CARTON / PACK. (`OWN` is NOT a variant — it's expressed at
//!     the outer enum.)
//!   - [`ProductUnit`] — sum type `Nav(NavUnitOfMeasure) | Own(String)`.
//!     Wire shape uses serde's internally-tagged form so the JSON is
//!     readable (`{"kind":"Nav","value":"PIECE"}` /
//!     `{"kind":"Own","value":"liter@15C"}`).
//!
//! Ervin's Hungarian examples map to NAV variants:
//!   - `db` (darab) → `Nav(PIECE)`
//!   - `nap` → `Nav(DAY)`
//!   - `tonna` → `Nav(TON)`
//!   - `kg` → `Nav(KILOGRAM)`
//!   - `óra` → `Nav(HOUR)`
//!   - `liter` → `Nav(LITER)`
//!   - `m` → `Nav(METER)`
//!
//! `liter@15C` (temperature-corrected litre, a fuel measure) has NO
//! NAV enum counterpart — NAV's plain LITER is volumetric, not
//! temperature-corrected — so it's `Own("liter@15C")` which the
//! future NAV emitter renders as `<unitOfMeasure>OWN</unitOfMeasure>`
//! + `<unitOfMeasureOwn>liter@15C</unitOfMeasureOwn>`. See ADR-0046
//! for the rationale.
//!
//! # Price model
//!
//! `unit_price_minor: i64` — stored in the currency's minor units
//! (HUF: whole forints, EUR: cents) per ADR-0037. The SPA parses
//! operator input with PR-88's `parseAmountToMinor` rule (bare ints
//! are WHOLE major units; cents only when an explicit separator is
//! typed). No silent factor-of-100 surprises.
//!
//! # History posture
//!
//! Mirrors Partners (PR-48α §A-decision): row-level `created_at` /
//! `updated_at` / `deleted_at` timestamps, soft-delete on remove. NO
//! entries in the `aberp_audit_ledger` (that ledger is reserved for
//! the invoice hash-chain per ADR-0008; extending the `EventKind`
//! ladder to cover catalog operations would couple product CRUD to
//! invoice integrity verification — wrong surface). Per-field history
//! is NOT recorded; a future `products_history` append-only table is
//! a back-compat add if/when an audit ask lands.
//!
//! # tenant_id on the row
//!
//! Same defensive denormalisation as `partners` and `audit_ledger`
//! per ADR-0002 — each tenant has its own DuckDB file but every query
//! filters by `tenant_id` so a future shared-DB shift requires zero
//! query changes.

use anyhow::{Context, Result};
use duckdb::{params, Connection};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use ulid::Ulid;

use aberp_billing::Currency;
// S159 — `NavUnitOfMeasure` + `ProductUnit` moved DOWN into billing's
// domain so `LineItem` can carry a line's unit to the NAV
// `<unitOfMeasure>` emit without a backwards `billing → app` dependency.
// Re-exported here so the products module's public path
// (`aberp::products::{NavUnitOfMeasure, ProductUnit}`) is unchanged.
pub use aberp_billing::{NavUnitOfMeasure, ProductUnit};

// ──────────────────────────────────────────────────────────────────────
// ProductId — prefixed-ULID newtype (`prd_<26-char-ULID>`).
// ──────────────────────────────────────────────────────────────────────

/// ULID newtype rendered as `prd_<26-char-ULID>` on the wire. Mirrors
/// `PartnerId` per ADR-0005 (every entity gets a newtype — type
/// confusion is a compile error).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProductId(pub Ulid);

impl ProductId {
    pub fn new() -> Self {
        Self(Ulid::new())
    }

    pub fn to_prefixed_string(&self) -> String {
        format!("prd_{}", self.0)
    }
}

impl Default for ProductId {
    fn default() -> Self {
        Self::new()
    }
}

// ──────────────────────────────────────────────────────────────────────
// ProductUnit DB-column (de)serialisation.
//
// `NavUnitOfMeasure` + `ProductUnit` themselves live in billing's domain
// (S159 — see the re-export above). The two-column DB persistence is a
// products-table concern, so it stays here as free functions over the
// re-exported types (inherent methods cannot be added to a foreign type).
// ──────────────────────────────────────────────────────────────────────

/// Serialise a [`ProductUnit`] to the two-column DB form: `(kind, value)`.
///
/// - `Nav(token)` → (`"Nav"`, `token.nav_token()`).
/// - `Own(label)` → (`"Own"`, `label`).
///
/// Two columns rather than a single JSON blob so a future "filter products
/// by NAV unit" query is a plain SQL predicate instead of a JSON-extract.
fn unit_to_db_columns(unit: &ProductUnit) -> (&'static str, String) {
    match unit {
        ProductUnit::Nav(token) => ("Nav", token.nav_token().to_string()),
        ProductUnit::Own(label) => ("Own", label.clone()),
    }
}

/// Reconstruct a [`ProductUnit`] from the two DB columns. Loud-fails on an
/// unknown `kind` or a `Nav` value outside the closed vocab.
fn unit_from_db_columns(kind: &str, value: &str) -> Result<ProductUnit> {
    match kind {
        "Nav" => match NavUnitOfMeasure::from_nav_token(value) {
            Some(token) => Ok(ProductUnit::Nav(token)),
            None => Err(anyhow::anyhow!(
                "products.unit_value `{}` is not a known NAV unitOfMeasure token",
                value
            )),
        },
        "Own" => Ok(ProductUnit::Own(value.to_string())),
        other => Err(anyhow::anyhow!(
            "products.unit_kind has unexpected value `{}` (expected Nav | Own)",
            other
        )),
    }
}

// ──────────────────────────────────────────────────────────────────────
// Product — domain + wire shape.
// ──────────────────────────────────────────────────────────────────────

#[derive(Serialize, Debug, PartialEq, Eq, Clone)]
pub struct Product {
    /// Prefixed-ULID `prd_<26-char-ULID>`.
    pub id: String,
    pub name: String,
    pub unit: ProductUnit,
    pub currency: Currency,
    /// Unit price in the currency's minor units (HUF: whole forints,
    /// EUR: cents) per ADR-0037. SPA parses operator input via
    /// PR-88's `parseAmountToMinor` rule.
    pub unit_price_minor: i64,
    pub created_at: String,
    pub updated_at: String,
    pub deleted_at: Option<String>,
}

/// Request-body shape for create / update.
#[derive(Deserialize, Debug, Clone)]
pub struct ProductInputs {
    pub name: String,
    pub unit: ProductUnit,
    pub currency: Currency,
    pub unit_price_minor: i64,
}

/// Structured validation error. Same envelope as Partners' for the
/// shared A157 inline-error renderer on the SPA.
#[derive(Serialize, Debug, PartialEq, Eq, Clone)]
pub struct ValidationError {
    pub field: &'static str,
    pub message: String,
}

// ──────────────────────────────────────────────────────────────────────
// Validation helpers.
// ──────────────────────────────────────────────────────────────────────

/// Validate field-level rules. Returns every problem at once (same UX
/// posture as `validate_partner_inputs`).
pub fn validate_product_inputs(inputs: &ProductInputs) -> Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();
    if inputs.name.trim().is_empty() {
        errors.push(ValidationError {
            field: "name",
            message: "name is required".to_string(),
        });
    }
    // Reject negative prices; zero is permitted (operator might catalogue
    // a "to be quoted" item with a 0 placeholder).
    if inputs.unit_price_minor < 0 {
        errors.push(ValidationError {
            field: "unit_price_minor",
            message: "unit price must be zero or positive".to_string(),
        });
    }
    if let ProductUnit::Own(label) = &inputs.unit {
        if label.trim().is_empty() {
            errors.push(ValidationError {
                field: "unit",
                message: "Own unit requires a non-empty label".to_string(),
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

const PRODUCTS_SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS products (
    id               VARCHAR NOT NULL PRIMARY KEY,
    tenant_id        VARCHAR NOT NULL,
    name             VARCHAR NOT NULL,
    unit_kind        VARCHAR NOT NULL CHECK (unit_kind IN ('Nav','Own')),
    unit_value       VARCHAR NOT NULL,
    currency         VARCHAR NOT NULL CHECK (currency IN ('HUF','EUR')),
    unit_price_minor BIGINT  NOT NULL,
    created_at       VARCHAR NOT NULL,
    updated_at       VARCHAR NOT NULL,
    deleted_at       VARCHAR
);
CREATE INDEX IF NOT EXISTS products_tenant_deleted_idx
    ON products (tenant_id, deleted_at);
CREATE INDEX IF NOT EXISTS products_tenant_name_idx
    ON products (tenant_id, name);
";

/// Idempotent `CREATE TABLE IF NOT EXISTS` for the products table.
/// Mirrors `partners::ensure_schema`. Called at serve boot per
/// PR-73a's hot-path migration discipline.
pub fn ensure_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(PRODUCTS_SCHEMA_SQL)
        .context("ensure products schema")
}

fn currency_to_db(c: Currency) -> &'static str {
    c.iso_code()
}

fn currency_from_db(s: &str) -> Result<Currency> {
    match s {
        "HUF" => Ok(Currency::Huf),
        "EUR" => Ok(Currency::Eur),
        other => Err(anyhow::anyhow!(
            "products.currency has unexpected value `{}` (expected HUF | EUR)",
            other
        )),
    }
}

/// Insert a new product row. Caller MUST have run
/// `validate_product_inputs` first.
pub fn create_product(conn: &Connection, tenant: &str, inputs: &ProductInputs) -> Result<Product> {
    ensure_schema(conn)?;
    let id = ProductId::new().to_prefixed_string();
    let now = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .context("format created_at as Rfc3339")?;
    let name = inputs.name.trim().to_string();
    let unit = match &inputs.unit {
        ProductUnit::Nav(token) => ProductUnit::Nav(*token),
        ProductUnit::Own(label) => ProductUnit::Own(label.trim().to_string()),
    };
    let (unit_kind, unit_value) = unit_to_db_columns(&unit);
    conn.execute(
        "INSERT INTO products (
            id, tenant_id, name, unit_kind, unit_value, currency,
            unit_price_minor, created_at, updated_at, deleted_at
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NULL);",
        params![
            &id,
            tenant,
            &name,
            unit_kind,
            &unit_value,
            currency_to_db(inputs.currency),
            inputs.unit_price_minor,
            &now,
            &now,
        ],
    )
    .context("INSERT into products")?;

    Ok(Product {
        id,
        name,
        unit,
        currency: inputs.currency,
        unit_price_minor: inputs.unit_price_minor,
        created_at: now.clone(),
        updated_at: now,
        deleted_at: None,
    })
}

/// Fetch a product by id, scoped to the tenant. Returns `None` for
/// missing OR soft-deleted rows (HTTP layer maps both to 404).
pub fn get_product(conn: &Connection, tenant: &str, id: &str) -> Result<Option<Product>> {
    ensure_schema(conn)?;
    let mut stmt = conn.prepare(
        "SELECT id, name, unit_kind, unit_value, currency,
                unit_price_minor, created_at, updated_at, deleted_at
         FROM products
         WHERE tenant_id = ? AND id = ? AND deleted_at IS NULL;",
    )?;
    let mut rows = stmt.query_map(params![tenant, id], row_to_product)?;
    match rows.next() {
        Some(r) => Ok(Some(r??)),
        None => Ok(None),
    }
}

/// List active products for the tenant. `search` is a case-insensitive
/// prefix filter on `name`. Result is `ORDER BY name ASC`.
pub fn list_products(
    conn: &Connection,
    tenant: &str,
    search: Option<&str>,
) -> Result<Vec<Product>> {
    ensure_schema(conn)?;
    let trimmed = search.map(|s| s.trim()).filter(|s| !s.is_empty());

    let mut out = Vec::new();
    match trimmed {
        Some(needle) => {
            let pattern = format!("{}%", needle.to_lowercase());
            let mut stmt = conn.prepare(
                "SELECT id, name, unit_kind, unit_value, currency,
                        unit_price_minor, created_at, updated_at, deleted_at
                 FROM products
                 WHERE tenant_id = ?
                   AND deleted_at IS NULL
                   AND LOWER(name) LIKE ?
                 ORDER BY name ASC;",
            )?;
            let rows = stmt.query_map(params![tenant, &pattern], row_to_product)?;
            for r in rows {
                out.push(r??);
            }
        }
        None => {
            let mut stmt = conn.prepare(
                "SELECT id, name, unit_kind, unit_value, currency,
                        unit_price_minor, created_at, updated_at, deleted_at
                 FROM products
                 WHERE tenant_id = ? AND deleted_at IS NULL
                 ORDER BY name ASC;",
            )?;
            let rows = stmt.query_map(params![tenant], row_to_product)?;
            for r in rows {
                out.push(r??);
            }
        }
    }
    Ok(out)
}

/// Update an existing product. Returns `None` for missing / soft-deleted.
pub fn update_product(
    conn: &Connection,
    tenant: &str,
    id: &str,
    inputs: &ProductInputs,
) -> Result<Option<Product>> {
    ensure_schema(conn)?;
    if get_product(conn, tenant, id)?.is_none() {
        return Ok(None);
    }

    let name = inputs.name.trim().to_string();
    let unit = match &inputs.unit {
        ProductUnit::Nav(token) => ProductUnit::Nav(*token),
        ProductUnit::Own(label) => ProductUnit::Own(label.trim().to_string()),
    };
    let (unit_kind, unit_value) = unit_to_db_columns(&unit);
    let now = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .context("format updated_at as Rfc3339")?;
    conn.execute(
        "UPDATE products SET
            name             = ?,
            unit_kind        = ?,
            unit_value       = ?,
            currency         = ?,
            unit_price_minor = ?,
            updated_at       = ?
         WHERE tenant_id = ? AND id = ? AND deleted_at IS NULL;",
        params![
            &name,
            unit_kind,
            &unit_value,
            currency_to_db(inputs.currency),
            inputs.unit_price_minor,
            &now,
            tenant,
            id,
        ],
    )
    .context("UPDATE products")?;

    get_product(conn, tenant, id)
}

/// Soft-delete a product. The row stays in the DB so future
/// "products on past invoices" lookups can still resolve.
pub fn soft_delete_product(conn: &Connection, tenant: &str, id: &str) -> Result<bool> {
    ensure_schema(conn)?;
    let now = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .context("format deleted_at as Rfc3339")?;
    let changed = conn
        .execute(
            "UPDATE products SET deleted_at = ?, updated_at = ?
             WHERE tenant_id = ? AND id = ? AND deleted_at IS NULL;",
            params![&now, &now, tenant, id],
        )
        .context("UPDATE products SET deleted_at")?;
    Ok(changed > 0)
}

fn row_to_product(row: &duckdb::Row<'_>) -> duckdb::Result<Result<Product>> {
    let id: String = row.get(0)?;
    let name: String = row.get(1)?;
    let unit_kind: String = row.get(2)?;
    let unit_value: String = row.get(3)?;
    let currency_str: String = row.get(4)?;
    let unit_price_minor: i64 = row.get(5)?;
    let created_at: String = row.get(6)?;
    let updated_at: String = row.get(7)?;
    let deleted_at: Option<String> = row.get(8)?;

    let unit = match unit_from_db_columns(&unit_kind, &unit_value) {
        Ok(u) => u,
        Err(e) => return Ok(Err(e)),
    };
    let currency = match currency_from_db(&currency_str) {
        Ok(c) => c,
        Err(e) => return Ok(Err(e)),
    };

    Ok(Ok(Product {
        id,
        name,
        unit,
        currency,
        unit_price_minor,
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
    use super::*;

    // ── NavUnitOfMeasure round-trip ────────────────────────────────────

    #[test]
    fn nav_unit_serde_round_trip_pin() {
        // Each NAV-token variant must serialise as the
        // SCREAMING_SNAKE_CASE token and round-trip cleanly. The wire
        // body and the NAV XML body agree by construction.
        for (variant, literal) in [
            (NavUnitOfMeasure::Piece, "\"PIECE\""),
            (NavUnitOfMeasure::Kilogram, "\"KILOGRAM\""),
            (NavUnitOfMeasure::Ton, "\"TON\""),
            (NavUnitOfMeasure::Kwh, "\"KWH\""),
            (NavUnitOfMeasure::Day, "\"DAY\""),
            (NavUnitOfMeasure::Hour, "\"HOUR\""),
            (NavUnitOfMeasure::Minute, "\"MINUTE\""),
            (NavUnitOfMeasure::Month, "\"MONTH\""),
            (NavUnitOfMeasure::Liter, "\"LITER\""),
            (NavUnitOfMeasure::Kilometer, "\"KILOMETER\""),
            (NavUnitOfMeasure::CubicMeter, "\"CUBIC_METER\""),
            (NavUnitOfMeasure::Meter, "\"METER\""),
            (NavUnitOfMeasure::LinearMeter, "\"LINEAR_METER\""),
            (NavUnitOfMeasure::Carton, "\"CARTON\""),
            (NavUnitOfMeasure::Pack, "\"PACK\""),
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(
                json, literal,
                "NavUnitOfMeasure::{:?} must emit {}",
                variant, literal
            );
            let back: NavUnitOfMeasure = serde_json::from_str(&json).unwrap();
            assert_eq!(back, variant);
            // nav_token() must agree with the serde wire form (the
            // future NAV XML emitter reads nav_token() directly).
            assert_eq!(
                format!("\"{}\"", variant.nav_token()),
                literal,
                "nav_token() and serde must agree for {:?}",
                variant
            );
        }
    }

    // ── ProductUnit shape pins ──────────────────────────────────────────

    #[test]
    fn product_unit_serde_nav_variant_pin() {
        let unit = ProductUnit::Nav(NavUnitOfMeasure::Piece);
        let json = serde_json::to_string(&unit).unwrap();
        // Internally-tagged: {"kind":"Nav","value":"PIECE"}.
        assert_eq!(json, r#"{"kind":"Nav","value":"PIECE"}"#);
        let back: ProductUnit = serde_json::from_str(&json).unwrap();
        assert_eq!(back, unit);
    }

    #[test]
    fn product_unit_serde_own_variant_pin() {
        // The canonical OWN case — temperature-corrected litre,
        // NOT present in NAV's enum, so the operator types it as a
        // free-text label and the future NAV emitter pairs it with
        // the literal OWN token. See ADR-0046.
        let unit = ProductUnit::Own("liter@15C".to_string());
        let json = serde_json::to_string(&unit).unwrap();
        assert_eq!(json, r#"{"kind":"Own","value":"liter@15C"}"#);
        let back: ProductUnit = serde_json::from_str(&json).unwrap();
        assert_eq!(back, unit);
    }

    #[test]
    fn product_unit_db_columns_round_trip() {
        // Nav variant.
        let unit = ProductUnit::Nav(NavUnitOfMeasure::Day);
        let (kind, value) = unit_to_db_columns(&unit);
        assert_eq!((kind, value.as_str()), ("Nav", "DAY"));
        assert_eq!(unit_from_db_columns(kind, &value).unwrap(), unit);

        // Own variant — the load-bearing liter@15C case.
        let unit = ProductUnit::Own("liter@15C".to_string());
        let (kind, value) = unit_to_db_columns(&unit);
        assert_eq!((kind, value.as_str()), ("Own", "liter@15C"));
        assert_eq!(unit_from_db_columns(kind, &value).unwrap(), unit);
    }

    #[test]
    fn product_unit_from_db_rejects_unknown_nav_token() {
        // Defence against a hand-edited DuckDB row: an unrecognised
        // unit_value paired with `Nav` must surface as a loud error
        // rather than silently coerce.
        let err = unit_from_db_columns("Nav", "NOT_A_REAL_TOKEN").unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("NOT_A_REAL_TOKEN"),
            "error message should name the offending token, got: {msg}"
        );
    }

    #[test]
    fn product_unit_from_db_rejects_unknown_kind() {
        let err = unit_from_db_columns("Bogus", "anything").unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("Bogus"),
            "error message should name the offending kind, got: {msg}"
        );
    }

    // ── validate_product_inputs ─────────────────────────────────────────

    fn minimal_valid_inputs() -> ProductInputs {
        ProductInputs {
            name: "Konzultáció".to_string(),
            unit: ProductUnit::Nav(NavUnitOfMeasure::Hour),
            currency: Currency::Huf,
            unit_price_minor: 25_000,
        }
    }

    #[test]
    fn validate_product_inputs_accepts_minimal_valid() {
        assert!(validate_product_inputs(&minimal_valid_inputs()).is_ok());
    }

    #[test]
    fn validate_product_inputs_surfaces_every_problem_at_once() {
        let bad = ProductInputs {
            name: "   ".to_string(),
            unit: ProductUnit::Own("   ".to_string()),
            currency: Currency::Eur,
            unit_price_minor: -1,
        };
        let errors = validate_product_inputs(&bad).expect_err("must reject");
        let fields: std::collections::BTreeSet<&'static str> =
            errors.iter().map(|e| e.field).collect();
        assert!(fields.contains("name"), "must flag name");
        assert!(fields.contains("unit"), "must flag empty Own label");
        assert!(
            fields.contains("unit_price_minor"),
            "must flag negative price"
        );
    }

    #[test]
    fn validate_product_inputs_accepts_zero_price() {
        // Zero is a permitted placeholder for "to be quoted" catalog
        // entries; only negative prices are rejected.
        let inputs = ProductInputs {
            unit_price_minor: 0,
            ..minimal_valid_inputs()
        };
        assert!(validate_product_inputs(&inputs).is_ok());
    }

    // ── ProductId prefix discipline ─────────────────────────────────────

    #[test]
    fn product_id_renders_with_prd_prefix() {
        let id = ProductId::new().to_prefixed_string();
        assert!(
            id.starts_with("prd_"),
            "ProductId must render as `prd_<ULID>`; got `{}`",
            id
        );
        assert_eq!(id.len(), 30, "prefixed ProductId must be 30 chars");
    }

    // ── In-memory DuckDB CRUD round-trip ────────────────────────────────

    fn open_in_memory() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        ensure_schema(&conn).unwrap();
        conn
    }

    #[test]
    fn crud_round_trip_nav_unit() {
        let conn = open_in_memory();
        let inputs = ProductInputs {
            name: "Tanácsadói nap".to_string(),
            unit: ProductUnit::Nav(NavUnitOfMeasure::Day),
            currency: Currency::Huf,
            unit_price_minor: 250_000,
        };
        let created = create_product(&conn, "tenant_a", &inputs).unwrap();
        assert!(created.id.starts_with("prd_"));
        assert_eq!(created.name, "Tanácsadói nap");
        assert_eq!(created.unit, ProductUnit::Nav(NavUnitOfMeasure::Day));
        assert_eq!(created.currency, Currency::Huf);
        assert_eq!(created.unit_price_minor, 250_000);
        assert_eq!(created.deleted_at, None);

        let fetched = get_product(&conn, "tenant_a", &created.id)
            .unwrap()
            .unwrap();
        assert_eq!(fetched, created);

        let listed = list_products(&conn, "tenant_a", None).unwrap();
        assert_eq!(listed, vec![created.clone()]);
    }

    #[test]
    fn crud_round_trip_own_unit_liter_at_15c() {
        // The canonical Own case — proves the OWN escape-hatch
        // survives a full round-trip through DuckDB.
        let conn = open_in_memory();
        let inputs = ProductInputs {
            name: "Gázolaj".to_string(),
            unit: ProductUnit::Own("liter@15C".to_string()),
            currency: Currency::Huf,
            unit_price_minor: 650,
        };
        let created = create_product(&conn, "tenant_a", &inputs).unwrap();
        assert_eq!(created.unit, ProductUnit::Own("liter@15C".to_string()));

        let fetched = get_product(&conn, "tenant_a", &created.id)
            .unwrap()
            .unwrap();
        assert_eq!(fetched.unit, ProductUnit::Own("liter@15C".to_string()));
    }

    #[test]
    fn update_bumps_timestamps_and_replaces_fields() {
        let conn = open_in_memory();
        let created = create_product(&conn, "tenant_a", &minimal_valid_inputs()).unwrap();
        // Updated inputs flip every mutable field.
        let updated_inputs = ProductInputs {
            name: "Renamed".to_string(),
            unit: ProductUnit::Own("liter@15C".to_string()),
            currency: Currency::Eur,
            unit_price_minor: 199,
        };
        let updated = update_product(&conn, "tenant_a", &created.id, &updated_inputs)
            .unwrap()
            .unwrap();
        assert_eq!(updated.name, "Renamed");
        assert_eq!(updated.unit, ProductUnit::Own("liter@15C".to_string()));
        assert_eq!(updated.currency, Currency::Eur);
        assert_eq!(updated.unit_price_minor, 199);
        assert_eq!(updated.created_at, created.created_at);
    }

    #[test]
    fn soft_delete_hides_row_from_get_and_list() {
        let conn = open_in_memory();
        let created = create_product(&conn, "tenant_a", &minimal_valid_inputs()).unwrap();
        assert!(soft_delete_product(&conn, "tenant_a", &created.id).unwrap());
        assert!(get_product(&conn, "tenant_a", &created.id)
            .unwrap()
            .is_none());
        assert!(list_products(&conn, "tenant_a", None).unwrap().is_empty());
        // Soft-deleting again is a no-op (returns false).
        assert!(!soft_delete_product(&conn, "tenant_a", &created.id).unwrap());
    }

    #[test]
    fn tenant_scoping_isolates_rows() {
        let conn = open_in_memory();
        let a = create_product(&conn, "tenant_a", &minimal_valid_inputs()).unwrap();
        let _b = create_product(&conn, "tenant_b", &minimal_valid_inputs()).unwrap();
        let listed_a = list_products(&conn, "tenant_a", None).unwrap();
        assert_eq!(listed_a.len(), 1);
        assert_eq!(listed_a[0].id, a.id);
        // tenant_b cannot fetch tenant_a's row.
        assert!(get_product(&conn, "tenant_b", &a.id).unwrap().is_none());
    }

    #[test]
    fn search_filters_by_name_prefix_case_insensitive() {
        let conn = open_in_memory();
        for name in ["Apple juice", "apricot", "Banana"] {
            let inputs = ProductInputs {
                name: name.to_string(),
                ..minimal_valid_inputs()
            };
            create_product(&conn, "tenant_a", &inputs).unwrap();
        }
        let hits = list_products(&conn, "tenant_a", Some("ap")).unwrap();
        let names: Vec<&str> = hits.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["Apple juice", "apricot"]);
    }
}
