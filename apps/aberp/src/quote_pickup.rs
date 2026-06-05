//! S255 / PR-244 — operator-clicked "Create draft invoice" on a
//! quote_intake_log row. Mints an `invoice_draft` with
//! `source_quote_id` set + emits `InvoicePickedUpFromQuote`.
//!
//! # Why this is a separate module (not a serve.rs handler arm)
//!
//! [[think-then-act]] — the pickup combines three independent
//! sub-systems (quote_intake_log read, partners resolve-or-create,
//! invoice_draft.create_draft_in_tx). Routing the handler arm through
//! a typed [`PickupQuoteOutcome`] keeps the serve.rs file thin and
//! pins the contract for the new integration tests.
//!
//! # Idempotency
//!
//! The route's idempotency anchor is the `quote_intake_log.picked_up_drf_id`
//! column + an audit-walk on `EventKind::InvoicePickedUpFromQuote`:
//!
//!   1. Read the quote_intake_log row by `(tenant, quote_id)`.
//!   2. If `picked_up_drf_id` is `Some(drf_X)` AND `drf_X` still
//!      exists in `invoice_draft` → return `{ drf_id: drf_X,
//!      partner_created: false, was_existing: true }` (no audit
//!      append, no INSERT).
//!   3. Otherwise (never picked up, OR prior draft was deleted via
//!      S239) → resolve/create partner, mint new draft, append audit,
//!      UPDATE `picked_up_drf_id`.
//!
//! Per [[no-sql-specific]] the dedup lives in Rust, not in a DB
//! UNIQUE constraint — the re-pickup-after-delete flow needs the
//! same `quote_id` to map to a fresh `drf_id`, which a UNIQUE block
//! would prevent.
//!
//! # Partner resolution ladder
//!
//! Quote payloads carry `contact.{name, email, company}` and never a
//! Hungarian tax number (Phase 2 quotes are anonymous web-form
//! submissions per S210 / PR-204). The resolver therefore skips
//! `find_partner_by_tax_number` and tries:
//!
//!   1. `find_partner_by_name_and_address` with `legal_name =
//!      contact.name` + all four address slots `None` — picks up the
//!      narrow "same name, no address" case (a repeat quote from the
//!      same individual).
//!   2. Fall through → create a new `Partner` with
//!      `kind: Customer`, `customer_vat_status: PrivatePerson`. The
//!      operator can promote to `Domestic` (with a real tax number)
//!      later via the PartnerForm.
//!
//! The brief's "state: from_quote_pickup" partner discriminator does
//! not exist in the schema and is intentionally NOT added — the
//! Partner table has no state column today, and adding one for a
//! single use case violates CLAUDE.md rule 2 / rule 13.

use anyhow::{anyhow, Context, Result};
use duckdb::Connection;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

use aberp_audit_ledger::{append_in_tx, Actor, EventKind, LedgerMeta};
use aberp_quote_intake::PreparedDraft;

use crate::invoice_draft;
use crate::nav_xml::CustomerVatStatus;
use crate::partners::{self, PartnerInputs, PartnerKind};

/// Sentinel `product_id` for quote-pickup drafts. Quotes do not name
/// a product from the `products` table — they reference a CAD upload
/// + operator-quoted price. The draft's `product_id NOT NULL` column
/// (carried over from the dispatch-spawn path) needs SOME value; this
/// sentinel signals "the line description rides in `notes`, fill the
/// real product at Issue time."
///
/// The sentinel is not registered in the `products` table; the
/// existing `create_draft_in_tx` validator only checks non-empty and
/// does NOT enforce a FK reference. See [[no-sql-specific]] for the
/// general posture.
pub const QUOTE_PICKUP_PRODUCT_SENTINEL: &str = "prd_FROM_QUOTE";

/// Outcome of a successful pickup. Surfaced verbatim on the route's
/// 200 JSON body so the SPA can render the navigation + confirm-modal
/// affordances.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PickupQuoteOutcome {
    /// `drf_<ULID>` of the new (or existing-idempotent) invoice_draft
    /// row.
    pub drf_id: String,
    /// `prt_<ULID>` of the resolved buyer. Always populated; either
    /// an existing partner the resolver matched OR a fresh one just
    /// created.
    pub partner_id: String,
    /// `true` iff the resolver minted a fresh Partner row this call.
    /// `false` iff an existing partner matched OR the call was an
    /// idempotent retry (`was_existing == true`). The SPA's
    /// ConfirmActionModal uses this to surface the "creating new
    /// partner record" warning copy.
    pub partner_created: bool,
    /// `true` iff the call short-circuited via the idempotency walk
    /// (the quote was already picked up AND that draft still exists).
    /// `false` for a fresh pickup OR a re-pickup after S239 delete.
    pub was_existing: bool,
}

/// `EventKind::InvoicePickedUpFromQuote` audit payload. Captures the
/// pickup chain anchor — the SPA does NOT consume this directly; it
/// is for future forensic walks ("which quotes have been picked up,
/// and which drafts did they produce?").
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InvoicePickedUpFromQuotePayload {
    pub quote_id: String,
    pub draft_id: String,
    pub tenant_id: String,
    pub partner_id: String,
    pub partner_created: bool,
    pub actor: String,
    pub idempotency_key: String,
    pub picked_up_at: String,
}

impl InvoicePickedUpFromQuotePayload {
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self)
            .expect("JSON serialization of InvoicePickedUpFromQuotePayload cannot fail")
    }
}

/// Resolve-or-create a Partner from a `PreparedDraft.suggested_partner`.
///
/// Pure function so the unit test can drive it on an in-memory
/// DuckDB without spinning the rest of `quote_pickup`.
///
/// Returns `(partner_id, created)` where `created == true` iff a
/// fresh row was inserted.
pub fn resolve_or_create_partner(
    conn: &Connection,
    tenant: &str,
    prepared: &PreparedDraft,
) -> Result<(String, bool)> {
    // PrivatePerson buyers have NO tax_number per ADR-0048; skip the
    // tax-number lookup arm entirely.
    let legal_name = prepared.suggested_partner.name.trim().to_string();
    if legal_name.is_empty() {
        return Err(anyhow!(
            "quote prepared_draft.suggested_partner.name is empty — \
             refusing to create a buyer-less partner"
        ));
    }

    if let Some(existing) = partners::find_partner_by_name_and_address(
        conn,
        tenant,
        &legal_name,
        None,
        None,
        None,
        None,
    )
    .context("look up existing partner by name+empty-address")?
    {
        return Ok((existing.id, false));
    }

    // Fresh partner. PrivatePerson because quote payloads are
    // anonymous web-form submissions; operator can promote to
    // Domestic with a tax_number via PartnerForm.
    let display_name = if prepared
        .suggested_partner
        .company
        .as_deref()
        .unwrap_or("")
        .is_empty()
    {
        legal_name.clone()
    } else {
        // When a company name is supplied, use it as the
        // display_name so the operator's typeahead surfaces the
        // recognisable label. legal_name stays as the individual
        // contact (the buyer named on the invoice).
        prepared
            .suggested_partner
            .company
            .as_deref()
            .unwrap()
            .trim()
            .to_string()
    };
    let inputs = PartnerInputs {
        display_name,
        legal_name,
        kind: PartnerKind::Customer,
        customer_vat_status: CustomerVatStatus::PrivatePerson,
        tax_number: None,
        eu_vat_number: None,
        address_street: None,
        address_postal_code: None,
        address_city: None,
        address_country: None,
        bank_account: None,
        contact_email: Some(prepared.suggested_partner.email.clone()),
        contact_phone: None,
    };
    partners::validate_partner_inputs(&inputs)
        .map_err(|errs| anyhow!("partner inputs invalid: {errs:?}"))?;
    let created = partners::create_partner(conn, tenant, &inputs).context("create_partner")?;
    Ok((created.id, true))
}

/// Compose the `notes` column on the new `invoice_draft` row. Carries
/// the line description from the prepared draft + the operator-
/// readable invoice note (material, quote id, customer notes). The
/// draft's single-product schema cannot represent a free-text line
/// item; `notes` is the carrier until the operator opens IssueInvoice
/// to type the real line.
pub fn compose_draft_notes(prepared: &PreparedDraft) -> String {
    let line_desc = prepared
        .lines
        .first()
        .map(|l| l.description.as_str())
        .unwrap_or("");
    let mut out = String::new();
    if !line_desc.is_empty() {
        out.push_str(line_desc);
    }
    if !prepared.invoice_note.is_empty() {
        if !out.is_empty() {
            out.push_str(" — ");
        }
        out.push_str(&prepared.invoice_note);
    }
    out
}

/// Idempotency-key composer. `quote_pickup:<quote_id>` for the fresh
/// pickup; `quote_pickup:<quote_id>:retry<N>` for a re-pickup after
/// the prior draft was deleted via S239. `N` is the count of prior
/// drafts (audit-ledger-derived) for this quote — never re-used so
/// the F8 audit gate accepts the new entry.
pub fn pickup_idempotency_key(quote_id: &str, retry_n: u32) -> String {
    if retry_n == 0 {
        format!("quote_pickup:{quote_id}")
    } else {
        format!("quote_pickup:{quote_id}:retry{retry_n}")
    }
}

/// Inputs to [`pickup_quote_as_draft`]. The route handler builds this
/// from the request path + operator_login + AppState plumbing.
#[derive(Debug, Clone)]
pub struct PickupQuoteInputs {
    pub tenant: String,
    pub quote_id: String,
    pub actor: String,
    /// `quote_pickup:<quote_id>[:retryN]` — caller-decided so the
    /// route layer can scope the retry counter on the audit-ledger
    /// walk per-request.
    pub idempotency_key: String,
}

/// Core pickup logic. Pure function over the connection + ledger so
/// it can be driven by both the HTTP route AND the integration tests
/// without spinning AppState.
///
/// Returns:
///   - `Ok(PickupQuoteOutcome { was_existing: true, .. })` if the
///     quote was already picked up AND that draft still exists.
///   - `Ok(PickupQuoteOutcome { was_existing: false, .. })` if a
///     fresh draft was minted (first-time pickup OR re-pickup after
///     S239 delete).
///   - `Err(_)` for: quote not staged, quote payload corrupted,
///     partner resolver failure, audit-ledger F8 collision (shouldn't
///     happen for a correctly-computed `idempotency_key`).
///
/// The `conn` argument is `&mut` because the function opens an
/// internal transaction for the draft INSERT + audit emit, mirroring
/// the dispatch-spawn pattern.
pub fn pickup_quote_as_draft(
    conn: &mut Connection,
    ledger_meta: &LedgerMeta,
    ledger_actor: Actor,
    inputs: PickupQuoteInputs,
) -> Result<PickupQuoteOutcome> {
    invoice_draft::ensure_schema(conn).context("ensure invoice_draft schema")?;
    partners::ensure_schema(conn).context("ensure partners schema")?;
    aberp_quote_intake::log_table::ensure_schema(conn)
        .map_err(|e| anyhow!("ensure quote_intake_log schema: {e}"))?;

    // Step 1 — read the quote-side row.
    let row =
        aberp_quote_intake::log_table::read_for_pickup(conn, &inputs.tenant, &inputs.quote_id)
            .map_err(|e| anyhow!("read quote_intake_log row: {e}"))?
            .ok_or_else(|| {
                anyhow!(
                    "quote {} is not staged in quote_intake_log (tenant {})",
                    inputs.quote_id,
                    inputs.tenant
                )
            })?;

    // Step 2 — idempotency walk. If the recorded draft still exists,
    // short-circuit. If it was deleted via S239, fall through to the
    // mint path.
    if let Some(prior_drf_id) = row.picked_up_drf_id.as_deref() {
        if let Some(existing) = invoice_draft::read_draft(conn, &inputs.tenant, prior_drf_id)
            .context("look up prior pickup draft")?
        {
            return Ok(PickupQuoteOutcome {
                drf_id: existing.drf_id,
                partner_id: existing.partner_id,
                partner_created: false,
                was_existing: true,
            });
        }
        // Else: prior draft is gone (operator deleted via S239).
        // Carry on to the fresh-mint path; the audit ledger's F8
        // pin protects against double-emit because the caller passed
        // a retry-suffixed idempotency_key.
    }

    // Step 3 — parse prepared_draft JSON.
    let prepared: PreparedDraft = serde_json::from_str(&row.prepared_draft)
        .context("decode quote_intake_log.prepared_draft as PreparedDraft")?;

    // Step 4 — resolve / create partner.
    let (partner_id, partner_created) = resolve_or_create_partner(conn, &inputs.tenant, &prepared)
        .context("resolve or create partner for quote pickup")?;

    // Step 5 — pull qty from the prepared draft's first line. Default
    // to ONE if the prepared draft has no lines (defensive; the
    // S210 mapper always emits exactly one).
    let qty: Decimal = prepared
        .lines
        .first()
        .and_then(|l| Decimal::from_str(&l.quantity).ok())
        .unwrap_or(Decimal::ONE);
    let notes = compose_draft_notes(&prepared);

    // Step 6 — mint the draft inside a tx so the INSERT + audit emit
    // are atomic. Mirrors the dispatch-spawn pattern.
    let now_iso = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .context("format pickup timestamp")?;

    let tx = conn.transaction().context("begin pickup tx")?;
    let draft = invoice_draft::create_draft_in_tx(
        &tx,
        ledger_meta,
        ledger_actor.clone(),
        invoice_draft::CreateDraftInputs {
            tenant: inputs.tenant.clone(),
            partner_id: partner_id.clone(),
            source_dispatch_id: None,
            source_wo_id: None,
            source_quote_id: Some(inputs.quote_id.clone()),
            product_id: QUOTE_PICKUP_PRODUCT_SENTINEL.to_string(),
            qty,
            notes: if notes.is_empty() { None } else { Some(notes) },
            actor: inputs.actor.clone(),
            // `create_draft_in_tx`'s F8 anchor is `staged:<drf>` —
            // distinct from this route's pickup F8. Compose a draft
            // staging key that is guaranteed unique per pickup attempt
            // by combining the pickup key with the draft id's prefix.
            // (`drf_<ULID>` is freshly minted inside create_draft_in_tx
            // so we cannot pre-compute it; instead we anchor on the
            // pickup key, which IS unique per call by construction.)
            idempotency_key: format!("staged:{}", inputs.idempotency_key),
        },
    )
    .context("create invoice_draft row")?;

    // Pickup-specific audit entry — quote-side anchor.
    let pickup_payload = InvoicePickedUpFromQuotePayload {
        quote_id: inputs.quote_id.clone(),
        draft_id: draft.drf_id.clone(),
        tenant_id: inputs.tenant.clone(),
        partner_id: partner_id.clone(),
        partner_created,
        actor: inputs.actor.clone(),
        idempotency_key: inputs.idempotency_key.clone(),
        picked_up_at: now_iso,
    };
    append_in_tx(
        &tx,
        ledger_meta,
        EventKind::InvoicePickedUpFromQuote,
        pickup_payload.to_bytes(),
        ledger_actor,
        Some(inputs.idempotency_key.clone()),
    )
    .context("audit append InvoicePickedUpFromQuote")?;

    // Stamp picked_up_drf_id INSIDE the tx so the SPA's "→ Draft" link
    // and the route mint are atomic. A failure mid-cascade rolls back
    // all three writes (draft row, two audit entries, log column).
    tx.execute(
        "UPDATE quote_intake_log
            SET picked_up_drf_id = ?1
          WHERE quote_id = ?2 AND tenant_id = ?3",
        duckdb::params![draft.drf_id, inputs.quote_id, inputs.tenant,],
    )
    .context("UPDATE quote_intake_log.picked_up_drf_id")?;

    tx.commit().context("commit pickup tx")?;

    Ok(PickupQuoteOutcome {
        drf_id: draft.drf_id,
        partner_id,
        partner_created,
        was_existing: false,
    })
}

/// Count prior `InvoicePickedUpFromQuote` audit entries for a quote
/// so the retry suffix in the next idempotency key is monotonic. Used
/// only on the re-pickup-after-S239-delete path. `0` on a never-picked-up
/// quote.
pub fn count_prior_pickups(ledger: &aberp_audit_ledger::Ledger, quote_id: &str) -> Result<u32> {
    let mut n: u32 = 0;
    for entry in ledger
        .entries()
        .context("read audit-ledger for pickup count")?
    {
        if entry.kind != EventKind::InvoicePickedUpFromQuote {
            continue;
        }
        let parsed: InvoicePickedUpFromQuotePayload = match serde_json::from_slice(&entry.payload) {
            Ok(p) => p,
            Err(_) => continue,
        };
        if parsed.quote_id == quote_id {
            n = n.saturating_add(1);
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aberp_audit_ledger::{
        ensure_schema as audit_ensure_schema, BinaryHash, LedgerMeta, TenantId,
    };
    use aberp_quote_intake::{
        log_table as quote_log,
        mapping::{PreparedLine, SuggestedPartnerJson},
    };

    fn open_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("open in-memory DuckDB");
        audit_ensure_schema(&conn).expect("audit-ledger schema");
        invoice_draft::ensure_schema(&conn).expect("invoice_draft schema");
        partners::ensure_schema(&conn).expect("partners schema");
        quote_log::ensure_schema(&conn).expect("quote_intake_log schema");
        conn
    }

    fn ledger_meta() -> LedgerMeta {
        LedgerMeta::new(
            TenantId::new("test-tenant").unwrap(),
            BinaryHash::from_bytes([0u8; 32]),
        )
    }

    fn actor() -> Actor {
        Actor::test_only()
    }

    fn sample_prepared_draft(quote_id: &str, name: &str, email: &str) -> PreparedDraft {
        PreparedDraft {
            invoice_id: "inv_TESTPLACEHOLDER000000000".to_string(),
            source_quote_id: quote_id.to_string(),
            suggested_partner: SuggestedPartnerJson {
                name: name.to_string(),
                email: email.to_string(),
                company: None,
            },
            invoice_note: "Friboard quote q-X — material: alu".to_string(),
            email_recipient_override: email.to_string(),
            lines: vec![PreparedLine {
                description: "Custom CNC part per quote q-X".to_string(),
                quantity: "3".to_string(),
                unit_price_huf: 0,
                vat_rate_basis_points: 2700,
                unit: "PIECE".to_string(),
            }],
            delivery_date: "2026-06-05".to_string(),
            payment_deadline: "2026-07-05".to_string(),
            currency: "HUF".to_string(),
        }
    }

    fn stage_quote(
        conn: &Connection,
        tenant: &str,
        quote_id: &str,
        prepared: &PreparedDraft,
        raw_contact_email: &str,
    ) {
        let now = time::OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let raw_payload = serde_json::json!({
            "id": quote_id,
            "contact": {
                "name": prepared.suggested_partner.name,
                "email": raw_contact_email,
                "company": prepared.suggested_partner.company,
            },
            "material": "alu",
            "quantity": 3,
        })
        .to_string();
        let prepared_json = serde_json::to_string(prepared).unwrap();
        quote_log::insert_intake(
            conn,
            tenant,
            quote_id,
            &prepared.invoice_id,
            "2026-06-05T08:00:00Z",
            now,
            &raw_payload,
            &prepared_json,
        )
        .unwrap();
    }

    #[test]
    fn pickup_creates_draft_with_source_quote_id() {
        let mut conn = open_conn();
        let prepared = sample_prepared_draft("q-1", "Ada Lovelace", "ada@example.com");
        stage_quote(&conn, "test-tenant", "q-1", &prepared, "ada@example.com");
        let meta = ledger_meta();
        let outcome = pickup_quote_as_draft(
            &mut conn,
            &meta,
            actor(),
            PickupQuoteInputs {
                tenant: "test-tenant".to_string(),
                quote_id: "q-1".to_string(),
                actor: "operator-A".to_string(),
                idempotency_key: pickup_idempotency_key("q-1", 0),
            },
        )
        .expect("pickup");
        assert!(outcome.drf_id.starts_with("drf_"));
        assert!(outcome.partner_id.starts_with("prt_"));
        assert!(outcome.partner_created);
        assert!(!outcome.was_existing);

        let draft = invoice_draft::read_draft(&conn, "test-tenant", &outcome.drf_id)
            .unwrap()
            .expect("draft row");
        assert_eq!(draft.source_quote_id, Some("q-1".to_string()));
        assert_eq!(draft.source_dispatch_id, None);
        assert_eq!(draft.partner_id, outcome.partner_id);
        assert_eq!(draft.qty, "3");
        assert_eq!(draft.product_id, QUOTE_PICKUP_PRODUCT_SENTINEL);
        assert!(draft
            .notes
            .as_deref()
            .unwrap()
            .contains("Custom CNC part per quote"));
    }

    #[test]
    fn pickup_is_idempotent_returns_existing_drf_id() {
        let mut conn = open_conn();
        let prepared = sample_prepared_draft("q-2", "Grace Hopper", "grace@example.com");
        stage_quote(&conn, "test-tenant", "q-2", &prepared, "grace@example.com");
        let meta = ledger_meta();
        let first = pickup_quote_as_draft(
            &mut conn,
            &meta,
            actor(),
            PickupQuoteInputs {
                tenant: "test-tenant".to_string(),
                quote_id: "q-2".to_string(),
                actor: "operator-A".to_string(),
                idempotency_key: pickup_idempotency_key("q-2", 0),
            },
        )
        .unwrap();
        let second = pickup_quote_as_draft(
            &mut conn,
            &meta,
            actor(),
            PickupQuoteInputs {
                tenant: "test-tenant".to_string(),
                quote_id: "q-2".to_string(),
                actor: "operator-A".to_string(),
                idempotency_key: pickup_idempotency_key("q-2", 0),
            },
        )
        .unwrap();
        assert_eq!(first.drf_id, second.drf_id);
        assert!(!first.was_existing);
        assert!(second.was_existing);
        assert!(!second.partner_created);
    }

    #[test]
    fn pickup_creates_new_partner_when_none_matches() {
        let mut conn = open_conn();
        let prepared = sample_prepared_draft("q-3", "Brand New Buyer", "new@example.com");
        stage_quote(&conn, "test-tenant", "q-3", &prepared, "new@example.com");
        let meta = ledger_meta();
        let outcome = pickup_quote_as_draft(
            &mut conn,
            &meta,
            actor(),
            PickupQuoteInputs {
                tenant: "test-tenant".to_string(),
                quote_id: "q-3".to_string(),
                actor: "operator-A".to_string(),
                idempotency_key: pickup_idempotency_key("q-3", 0),
            },
        )
        .unwrap();
        assert!(outcome.partner_created);
        let row = partners::get_partner(&conn, "test-tenant", &outcome.partner_id)
            .unwrap()
            .expect("partner row");
        assert_eq!(row.legal_name, "Brand New Buyer");
        assert_eq!(row.customer_vat_status, CustomerVatStatus::PrivatePerson);
        assert_eq!(row.tax_number, None);
        assert_eq!(row.contact_email.as_deref(), Some("new@example.com"));
    }

    #[test]
    fn pickup_links_existing_partner_by_name() {
        let mut conn = open_conn();
        // Pre-seed an existing partner with the same legal_name.
        let inputs = PartnerInputs {
            display_name: "Existing".to_string(),
            legal_name: "Existing Buyer".to_string(),
            kind: PartnerKind::Customer,
            customer_vat_status: CustomerVatStatus::PrivatePerson,
            tax_number: None,
            eu_vat_number: None,
            address_street: None,
            address_postal_code: None,
            address_city: None,
            address_country: None,
            bank_account: None,
            contact_email: Some("old@example.com".to_string()),
            contact_phone: None,
        };
        let existing = partners::create_partner(&conn, "test-tenant", &inputs).unwrap();

        let prepared = sample_prepared_draft("q-4", "Existing Buyer", "new@example.com");
        stage_quote(&conn, "test-tenant", "q-4", &prepared, "new@example.com");
        let meta = ledger_meta();
        let outcome = pickup_quote_as_draft(
            &mut conn,
            &meta,
            actor(),
            PickupQuoteInputs {
                tenant: "test-tenant".to_string(),
                quote_id: "q-4".to_string(),
                actor: "operator-A".to_string(),
                idempotency_key: pickup_idempotency_key("q-4", 0),
            },
        )
        .unwrap();
        assert!(!outcome.partner_created);
        assert_eq!(outcome.partner_id, existing.id);
    }

    #[test]
    fn pickup_writes_back_picked_up_drf_id() {
        let mut conn = open_conn();
        let prepared = sample_prepared_draft("q-5", "Some Buyer", "buyer@example.com");
        stage_quote(&conn, "test-tenant", "q-5", &prepared, "buyer@example.com");
        let meta = ledger_meta();
        let outcome = pickup_quote_as_draft(
            &mut conn,
            &meta,
            actor(),
            PickupQuoteInputs {
                tenant: "test-tenant".to_string(),
                quote_id: "q-5".to_string(),
                actor: "operator-A".to_string(),
                idempotency_key: pickup_idempotency_key("q-5", 0),
            },
        )
        .unwrap();
        let row = quote_log::read_for_pickup(&conn, "test-tenant", "q-5")
            .unwrap()
            .expect("row");
        assert_eq!(
            row.picked_up_drf_id.as_deref(),
            Some(outcome.drf_id.as_str())
        );
    }

    #[test]
    fn pickup_emits_invoice_picked_up_from_quote_audit() {
        let mut conn = open_conn();
        let prepared = sample_prepared_draft("q-6", "Audit Buyer", "audit@example.com");
        stage_quote(&conn, "test-tenant", "q-6", &prepared, "audit@example.com");
        let meta = ledger_meta();
        let outcome = pickup_quote_as_draft(
            &mut conn,
            &meta,
            actor(),
            PickupQuoteInputs {
                tenant: "test-tenant".to_string(),
                quote_id: "q-6".to_string(),
                actor: "operator-A".to_string(),
                idempotency_key: pickup_idempotency_key("q-6", 0),
            },
        )
        .unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT payload FROM audit_ledger
                 WHERE kind = 'invoice.picked_up_from_quote'
                 ORDER BY seq DESC LIMIT 1;",
            )
            .unwrap();
        let bytes: Vec<u8> = stmt.query_row([], |row| row.get(0)).unwrap();
        let payload: InvoicePickedUpFromQuotePayload = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(payload.quote_id, "q-6");
        assert_eq!(payload.draft_id, outcome.drf_id);
        assert_eq!(payload.partner_id, outcome.partner_id);
        assert_eq!(payload.actor, "operator-A");
        assert_eq!(payload.idempotency_key, "quote_pickup:q-6");
    }

    #[test]
    fn pickup_re_pickup_after_draft_deletion_creates_new_draft() {
        let mut conn = open_conn();
        let prepared = sample_prepared_draft("q-7", "Repeat Buyer", "rep@example.com");
        stage_quote(&conn, "test-tenant", "q-7", &prepared, "rep@example.com");
        let meta = ledger_meta();
        // Boot a minimal `dispatches` schema for the S239 cascade
        // path's defensive UPDATE.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS dispatches (
                dsp_id              VARCHAR NOT NULL PRIMARY KEY,
                tenant_id           VARCHAR NOT NULL,
                spawned_invoice_id  VARCHAR
            );",
        )
        .unwrap();
        let first = pickup_quote_as_draft(
            &mut conn,
            &meta,
            actor(),
            PickupQuoteInputs {
                tenant: "test-tenant".to_string(),
                quote_id: "q-7".to_string(),
                actor: "operator-A".to_string(),
                idempotency_key: pickup_idempotency_key("q-7", 0),
            },
        )
        .unwrap();
        // Simulate operator's S239 delete (the actual route does
        // more — null dispatch ptr, audit emit — exercised in the
        // invoice_draft tests).
        let tx = conn.transaction().unwrap();
        invoice_draft::delete_draft_in_tx(
            &tx,
            &meta,
            actor(),
            invoice_draft::DeleteDraftInputs {
                tenant: "test-tenant".to_string(),
                drf_id: first.drf_id.clone(),
                actor: "operator-A".to_string(),
            },
        )
        .unwrap();
        tx.commit().unwrap();

        // Now re-pickup with retry-suffixed key.
        let second = pickup_quote_as_draft(
            &mut conn,
            &meta,
            actor(),
            PickupQuoteInputs {
                tenant: "test-tenant".to_string(),
                quote_id: "q-7".to_string(),
                actor: "operator-A".to_string(),
                idempotency_key: pickup_idempotency_key("q-7", 1),
            },
        )
        .expect("re-pickup");
        assert_ne!(first.drf_id, second.drf_id);
        assert!(!second.was_existing);
        // The picked_up_drf_id column tracks the LATEST drf_id only.
        let row = quote_log::read_for_pickup(&conn, "test-tenant", "q-7")
            .unwrap()
            .expect("row");
        assert_eq!(
            row.picked_up_drf_id.as_deref(),
            Some(second.drf_id.as_str())
        );
    }

    #[test]
    fn pickup_loud_fails_on_missing_quote() {
        let mut conn = open_conn();
        let meta = ledger_meta();
        let err = pickup_quote_as_draft(
            &mut conn,
            &meta,
            actor(),
            PickupQuoteInputs {
                tenant: "test-tenant".to_string(),
                quote_id: "q-MISSING".to_string(),
                actor: "operator-A".to_string(),
                idempotency_key: pickup_idempotency_key("q-MISSING", 0),
            },
        );
        let err_str = err.unwrap_err().to_string();
        assert!(
            err_str.contains("not staged"),
            "missing-quote error must name the gap; got {err_str}"
        );
    }

    #[test]
    fn invoice_picked_up_payload_round_trip() {
        let p = InvoicePickedUpFromQuotePayload {
            quote_id: "q-X".to_string(),
            draft_id: "drf_X".to_string(),
            tenant_id: "t".to_string(),
            partner_id: "prt_X".to_string(),
            partner_created: true,
            actor: "operator".to_string(),
            idempotency_key: "quote_pickup:q-X".to_string(),
            picked_up_at: "2026-06-05T12:00:00Z".to_string(),
        };
        let back: InvoicePickedUpFromQuotePayload = serde_json::from_slice(&p.to_bytes()).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn pickup_idempotency_key_shape() {
        assert_eq!(pickup_idempotency_key("q-1", 0), "quote_pickup:q-1");
        assert_eq!(pickup_idempotency_key("q-1", 1), "quote_pickup:q-1:retry1");
        assert_eq!(
            pickup_idempotency_key("q-1", 42),
            "quote_pickup:q-1:retry42"
        );
    }

    #[test]
    fn compose_draft_notes_joins_line_and_invoice_note() {
        let mut p = sample_prepared_draft("q-X", "n", "e@example.com");
        let composed = compose_draft_notes(&p);
        assert!(composed.contains("Custom CNC part"));
        assert!(composed.contains("Friboard quote"));
        // Empty line description — only the invoice_note remains.
        p.lines.clear();
        let composed = compose_draft_notes(&p);
        assert!(composed.contains("Friboard quote"));
        assert!(!composed.starts_with(" — "));
    }

    #[test]
    fn resolve_or_create_partner_uses_company_as_display_name_when_present() {
        let conn = open_conn();
        let mut prepared = sample_prepared_draft("q-X", "Ada Lovelace", "ada@example.com");
        prepared.suggested_partner.company = Some("Babbage & Co".to_string());
        let (pid, created) = resolve_or_create_partner(&conn, "test-tenant", &prepared).unwrap();
        assert!(created);
        let row = partners::get_partner(&conn, "test-tenant", &pid)
            .unwrap()
            .expect("row");
        assert_eq!(row.display_name, "Babbage & Co");
        assert_eq!(row.legal_name, "Ada Lovelace");
    }
}
