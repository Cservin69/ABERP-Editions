//! Pure quote → prepared-draft mapping.
//!
//! `PreparedDraft` captures everything the operator's pickup needs;
//! `series_id` + `customer_id` choices are made at pickup (S211).
//! The `invoice_id` ULID is minted at intake so re-pickup is
//! idempotent at the allocator's `IdempotencyKey`.

use rust_decimal::Decimal;
use time::{Date, Duration, OffsetDateTime};
use ulid::Ulid;

use aberp_billing::{Currency, Huf, InvoiceId, LineItem, NavUnitOfMeasure, ProductUnit};

use crate::error::QuoteIntakeError;
use crate::payload::Quote;

const DEFAULT_VAT_BP: u16 = 2700;
const DEFAULT_PAYMENT_DEADLINE_DAYS: i64 = 30;
/// Customer-facing storefront status written back when intake stages an
/// internal DRAFT invoice from an accepted quote.
///
/// S398 (Bug #3 — fiscal misrepresentation): this used to be `"invoiced"`,
/// which flipped the customer portal to "Számlázva / Invoiced" the instant the
/// daemon auto-staged a *draft* — pre-DEAL, with no fiscal invoice in
/// existence. Draft staging is automatic; it is NOT a DEAL/operator
/// confirmation and NOT a fiscal invoice, so the truthful customer-facing state
/// is `"processing"` ("Feldolgozás alatt / In progress"). The storefront
/// reserves `"invoiced"` for a real invoice-issuance writeback and its status
/// state machine only permits `approved → processing`, so a stale `"invoiced"`
/// build pointed at a post-S398 storefront would be rejected, not silently
/// mislabel again.
pub const QUOTE_PROCESSING_STATUS: &str = "processing";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuggestedPartner {
    pub name: String,
    pub email: String,
    pub company: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PreparedDraft {
    pub invoice_id: String,
    pub source_quote_id: String,
    pub suggested_partner: SuggestedPartnerJson,
    pub invoice_note: String,
    pub email_recipient_override: String,
    pub lines: Vec<PreparedLine>,
    pub delivery_date: String,
    pub payment_deadline: String,
    pub currency: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SuggestedPartnerJson {
    pub name: String,
    pub email: String,
    #[serde(default)]
    pub company: Option<String>,
}

impl From<SuggestedPartner> for SuggestedPartnerJson {
    fn from(value: SuggestedPartner) -> Self {
        Self {
            name: value.name,
            email: value.email,
            company: value.company,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PreparedLine {
    pub description: String,
    pub quantity: String,
    pub unit_price_huf: i64,
    pub vat_rate_basis_points: u16,
    pub unit: String,
}

#[derive(Debug, Clone)]
pub struct MappingOutcome {
    pub invoice_id: InvoiceId,
    pub suggested_partner: SuggestedPartner,
    pub lines_typed: Vec<LineItem>,
    pub delivery_date: Date,
    pub payment_deadline: Date,
    pub currency: Currency,
    pub prepared: PreparedDraft,
}

pub fn quote_to_draft_invoice(
    quote: &Quote,
    now: OffsetDateTime,
    default_currency: Currency,
) -> Result<MappingOutcome, QuoteIntakeError> {
    if quote.status != "approved" {
        return Err(QuoteIntakeError::Mapping {
            quote_id: quote.id.clone(),
            message: format!("quote status is {:?}, expected \"approved\"", quote.status),
        });
    }

    let email = quote.contact.email.trim().to_string();
    if email.is_empty() {
        return Err(QuoteIntakeError::Mapping {
            quote_id: quote.id.clone(),
            message: "quote contact email is empty".to_string(),
        });
    }
    let name = quote.contact.name.trim().to_string();
    if name.is_empty() {
        return Err(QuoteIntakeError::Mapping {
            quote_id: quote.id.clone(),
            message: "quote contact name is empty".to_string(),
        });
    }
    let company = {
        let c = quote.contact.company.trim();
        if c.is_empty() {
            None
        } else {
            Some(c.to_string())
        }
    };

    let suggested_partner = SuggestedPartner {
        name,
        email: email.clone(),
        company,
    };

    let quantity_i64 = quote.request.quantity.unwrap_or(1).max(1);
    let quantity = Decimal::from(quantity_i64);

    let line_description = format!("Custom CNC part per quote {}", quote.id);
    let line_typed = LineItem {
        description: line_description.clone(),
        quantity,
        unit_price: Huf::ZERO,
        vat_rate_basis_points: DEFAULT_VAT_BP,
        // ADR-0101 — quote-intake maps to the default numeric-rate kind
        // (a quote is always a standard-percentage line today).
        vat_rate_kind: aberp_billing::VatRateKind::Percent,
        note: None,
        unit: Some(ProductUnit::Nav(NavUnitOfMeasure::Piece)),
    };

    let delivery_date = now.date();
    let payment_deadline = delivery_date
        .checked_add(Duration::days(DEFAULT_PAYMENT_DEADLINE_DAYS))
        .ok_or_else(|| QuoteIntakeError::Mapping {
            quote_id: quote.id.clone(),
            message: "payment deadline date overflow".to_string(),
        })?;

    let invoice_note = compose_invoice_note(quote);

    let invoice_id = InvoiceId(Ulid::new());

    let date_fmt = time::macros::format_description!("[year]-[month]-[day]");
    let delivery_date_str =
        delivery_date
            .format(&date_fmt)
            .map_err(|e| QuoteIntakeError::Mapping {
                quote_id: quote.id.clone(),
                message: format!("format delivery date: {e}"),
            })?;
    let payment_deadline_str =
        payment_deadline
            .format(&date_fmt)
            .map_err(|e| QuoteIntakeError::Mapping {
                quote_id: quote.id.clone(),
                message: format!("format payment deadline: {e}"),
            })?;

    let prepared = PreparedDraft {
        invoice_id: invoice_id.to_prefixed_string(),
        source_quote_id: quote.id.clone(),
        suggested_partner: suggested_partner.clone().into(),
        invoice_note,
        email_recipient_override: email,
        lines: vec![PreparedLine {
            description: line_description,
            quantity: quantity.to_string(),
            unit_price_huf: 0,
            vat_rate_basis_points: DEFAULT_VAT_BP,
            unit: "PIECE".to_string(),
        }],
        delivery_date: delivery_date_str,
        payment_deadline: payment_deadline_str,
        currency: default_currency.iso_code().to_string(),
    };

    Ok(MappingOutcome {
        invoice_id,
        suggested_partner,
        lines_typed: vec![line_typed],
        delivery_date,
        payment_deadline,
        currency: default_currency,
        prepared,
    })
}

fn compose_invoice_note(quote: &Quote) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(4);
    parts.push(format!("Friboard quote {}", quote.id));
    if !quote.request.material_preference.trim().is_empty() {
        parts.push(format!(
            "material: {}",
            quote.request.material_preference.trim()
        ));
    }
    if let Some(q) = quote.request.quantity {
        parts.push(format!("qty: {q}"));
    }
    if let Some(d) = quote.request.deadline.as_ref() {
        let d = d.trim();
        if !d.is_empty() {
            parts.push(format!("deadline: {d}"));
        }
    }
    let mut note = parts.join(" — ");
    let customer_notes = quote.request.notes.trim();
    if !customer_notes.is_empty() {
        note.push_str(". Customer notes: ");
        note.push_str(customer_notes);
    }
    note.push_str(". CAD files attached on quote.");
    note
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payload::{QuoteContact, QuoteRequest};

    fn sample_quote() -> Quote {
        Quote {
            id: "q-123".to_string(),
            received_at: "2026-05-31T12:00:00Z".to_string(),
            contact: QuoteContact {
                name: "Ada Lovelace".to_string(),
                email: "ada@example.com".to_string(),
                company: "Babbage & Co".to_string(),
                extra: serde_json::Value::Null,
            },
            request: QuoteRequest {
                material_preference: "aluminum 6061".to_string(),
                quantity: Some(3),
                deadline: Some("2026-07-01".to_string()),
                notes: "anodized matte black".to_string(),
                extra: serde_json::Value::Null,
            },
            files: vec![],
            status: "approved".to_string(),
            consent_at: "2026-05-31T11:59:00Z".to_string(),
            status_history: None,
            extra: serde_json::Value::Null,
        }
    }

    fn fixed_now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_748_649_600).unwrap()
    }

    #[test]
    fn happy_path_existing_company_quote() {
        let q = sample_quote();
        let out = quote_to_draft_invoice(&q, fixed_now(), Currency::Huf).unwrap();
        assert_eq!(out.suggested_partner.email, "ada@example.com");
        assert_eq!(out.suggested_partner.name, "Ada Lovelace");
        assert_eq!(
            out.suggested_partner.company.as_deref(),
            Some("Babbage & Co")
        );
        assert_eq!(out.lines_typed.len(), 1);
        assert_eq!(out.lines_typed[0].unit_price, Huf::ZERO);
        assert_eq!(out.lines_typed[0].quantity, Decimal::from(3));
        assert_eq!(out.lines_typed[0].vat_rate_basis_points, 2700);
        assert_eq!(
            out.lines_typed[0].unit,
            Some(ProductUnit::Nav(NavUnitOfMeasure::Piece))
        );
        assert_eq!(out.currency, Currency::Huf);
        assert_eq!(out.delivery_date, fixed_now().date());
        assert_eq!(
            out.payment_deadline,
            fixed_now().date() + Duration::days(30)
        );
        assert!(out.prepared.invoice_note.contains("Friboard quote q-123"));
        assert!(out.prepared.invoice_note.contains("aluminum 6061"));
        assert!(out.prepared.invoice_note.contains("anodized matte black"));
        assert!(out.prepared.invoice_id.starts_with("inv_"));
    }

    #[test]
    fn quote_without_company_yields_no_company() {
        let mut q = sample_quote();
        q.contact.company = "   ".to_string();
        let out = quote_to_draft_invoice(&q, fixed_now(), Currency::Huf).unwrap();
        assert_eq!(out.suggested_partner.company, None);
    }

    #[test]
    fn quote_without_quantity_defaults_to_one() {
        let mut q = sample_quote();
        q.request.quantity = None;
        q.request.deadline = None;
        let out = quote_to_draft_invoice(&q, fixed_now(), Currency::Huf).unwrap();
        assert_eq!(out.lines_typed[0].quantity, Decimal::from(1));
        assert!(out.prepared.invoice_note.contains("aluminum 6061"));
        assert!(!out.prepared.invoice_note.contains("qty:"));
        assert!(!out.prepared.invoice_note.contains("deadline:"));
    }

    #[test]
    fn rejects_non_approved_status() {
        let mut q = sample_quote();
        q.status = "new".to_string();
        let err = quote_to_draft_invoice(&q, fixed_now(), Currency::Huf).unwrap_err();
        assert!(matches!(err, QuoteIntakeError::Mapping { .. }));
        assert!(err.to_string().contains("approved"));
    }

    #[test]
    fn rejects_missing_email() {
        let mut q = sample_quote();
        q.contact.email = "   ".to_string();
        let err = quote_to_draft_invoice(&q, fixed_now(), Currency::Huf).unwrap_err();
        assert!(err.to_string().contains("email"));
    }

    #[test]
    fn rejects_missing_name() {
        let mut q = sample_quote();
        q.contact.name = String::new();
        let err = quote_to_draft_invoice(&q, fixed_now(), Currency::Huf).unwrap_err();
        assert!(err.to_string().contains("name"));
    }

    #[test]
    fn prepared_draft_round_trips_json() {
        let q = sample_quote();
        let out = quote_to_draft_invoice(&q, fixed_now(), Currency::Huf).unwrap();
        let json = serde_json::to_string(&out.prepared).unwrap();
        let back: PreparedDraft = serde_json::from_str(&json).unwrap();
        assert_eq!(back, out.prepared);
    }
}
