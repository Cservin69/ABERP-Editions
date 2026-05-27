//! PR-73 / ADR-0040 §addendum — denormalized bank-account snapshot
//! read helpers.
//!
//! Mirror module for [`crate::invoice_currency_metadata`] — the same
//! shape (one stored-row struct + one tx-borrowing load helper) for
//! the five PR-73 `bank_account_*` invoice columns.
//!
//! Three callers consume the load helper:
//!
//!   1. `serve.rs`'s list + detail handlers — to surface the snapshot
//!      on the wire so the SPA's `InvoiceDetail.svelte` "Pay to"
//!      sub-section renders the bank name + account number + SWIFT/BIC
//!      the invoice was issued with.
//!   2. `issue_storno.rs` — to inherit the BASE invoice's snapshot
//!      onto the storno chain child (the regulatory record is "the
//!      bank account the base asked to be paid to"; re-resolving
//!      against current `seller.toml` could surface a different
//!      account if the operator rotated the default between
//!      issuance and storno).
//!   3. `issue_modification.rs` — same inheritance posture.

use aberp_billing::BankAccountSnapshot;
use anyhow::{anyhow, Result};

/// Stored-row bank-account snapshot. Mirrors the five PR-73-added
/// columns on the `invoice` DuckDB row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvoiceBankSnapshot {
    pub bank_account_id: Option<String>,
    pub bank_account_currency: Option<String>,
    pub bank_account_number: Option<String>,
    pub bank_account_bank_name: Option<String>,
    pub bank_account_swift_bic: Option<String>,
}

impl InvoiceBankSnapshot {
    /// True iff all five fields are populated.
    pub fn present(&self) -> bool {
        self.bank_account_id.is_some()
            && self.bank_account_currency.is_some()
            && self.bank_account_number.is_some()
            && self.bank_account_bank_name.is_some()
            && self.bank_account_swift_bic.is_some()
    }

    /// Convert into the typed `BankAccountSnapshot` iff all five fields
    /// are present. Used by chain-issuance paths to inherit the base's
    /// snapshot onto the chain child's `AllocateArgs.bank_snapshot`.
    pub fn into_typed(self) -> Option<BankAccountSnapshot> {
        match (
            self.bank_account_id,
            self.bank_account_currency,
            self.bank_account_number,
            self.bank_account_bank_name,
            self.bank_account_swift_bic,
        ) {
            (Some(id), Some(currency), Some(account_number), Some(bank_name), Some(swift_bic)) => {
                Some(BankAccountSnapshot {
                    id,
                    currency,
                    account_number,
                    bank_name,
                    swift_bic,
                })
            }
            _ => None,
        }
    }
}

/// PR-73 — read the five bank-account snapshot columns off an
/// `invoice` row in the caller's transaction.
pub fn load_invoice_bank_snapshot_in_tx(
    tx: &duckdb::Transaction<'_>,
    invoice_id: &str,
) -> Result<InvoiceBankSnapshot> {
    let mut stmt = tx
        .prepare(
            "SELECT bank_account_id,
                    bank_account_currency,
                    bank_account_number,
                    bank_account_bank_name,
                    bank_account_swift_bic
             FROM invoice WHERE id = ?;",
        )
        .map_err(|e| anyhow!("prepare invoice bank-snapshot SELECT: {e}"))?;
    let row = stmt
        .query_row([invoice_id], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })
        .map_err(|e| anyhow!("read invoice bank-snapshot for id {invoice_id}: {e}"))?;
    Ok(InvoiceBankSnapshot {
        bank_account_id: row.0,
        bank_account_currency: row.1,
        bank_account_number: row.2,
        bank_account_bank_name: row.3,
        bank_account_swift_bic: row.4,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn present_requires_all_five_fields() {
        let full = InvoiceBankSnapshot {
            bank_account_id: Some("bnk_x".to_string()),
            bank_account_currency: Some("HUF".to_string()),
            bank_account_number: Some("123".to_string()),
            bank_account_bank_name: Some("Bank".to_string()),
            bank_account_swift_bic: Some("GIBAHUHB".to_string()),
        };
        assert!(full.present());

        let mut missing_one = full.clone();
        missing_one.bank_account_currency = None;
        assert!(!missing_one.present());
    }

    #[test]
    fn into_typed_returns_snapshot_when_all_fields_present() {
        let full = InvoiceBankSnapshot {
            bank_account_id: Some("bnk_z".to_string()),
            bank_account_currency: Some("EUR".to_string()),
            bank_account_number: Some("HU12-3456".to_string()),
            bank_account_bank_name: Some("Erste".to_string()),
            bank_account_swift_bic: Some("GIBAHUHB".to_string()),
        };
        let typed = full.into_typed().expect("full snapshot must convert");
        assert_eq!(typed.id, "bnk_z");
        assert_eq!(typed.currency, "EUR");
    }

    #[test]
    fn into_typed_returns_none_for_partial_snapshot() {
        let partial = InvoiceBankSnapshot {
            bank_account_id: Some("bnk_p".to_string()),
            bank_account_currency: None,
            bank_account_number: Some("999".to_string()),
            bank_account_bank_name: Some("X".to_string()),
            bank_account_swift_bic: Some("Y".to_string()),
        };
        assert!(partial.into_typed().is_none());
    }
}
