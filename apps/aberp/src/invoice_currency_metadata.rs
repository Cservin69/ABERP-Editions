//! PR-44γ.1 / ADR-0037 §4 invariant C6 — chain-currency-match support.
//!
//! Two callers need to read the five PR-44γ-added currency columns
//! (`currency`, `exchange_rate`, `exchange_rate_source`,
//! `exchange_rate_date`, `huf_equivalent_total`) off an `invoice` row
//! inside a duckdb transaction:
//!
//!   1. `serve.rs`'s `read_invoice_row` / `get_invoice_detail` — for
//!      the SPA list + detail wire shape (PR-44ε / session 53).
//!   2. `issue_storno.rs` + `issue_modification.rs` — to inherit the
//!      base invoice's currency + rate metadata onto the chain-child
//!      storno or modification per the C6 invariant (THIS PR / session
//!      54).
//!
//! Session 53 placed the helper privately inside `serve.rs`. Session
//! 54 lifts it into this shared module so the chain-issuance paths
//! can reuse the same read (single source of truth for the column
//! shape; a future schema-drift on those columns surfaces in one
//! place). Per A149 the new module is the minimum-new-surface choice:
//! one shared helper + one decode + one validator instead of three
//! sister copies in three call sites.
//!
//! # What this module owns
//!
//! - `InvoiceCurrencyMetadata` — the stored-row shape, read back as
//!   plain `Option<String>` / `Option<i64>` fields (no `rust_decimal`
//!   dep at the read boundary).
//! - `load_invoice_currency_metadata_in_tx` — the SELECT, in the
//!   caller's tx, with the same casts (DECIMAL → VARCHAR,
//!   DECIMAL → BIGINT, DATE → VARCHAR) the session-53 helper used.
//! - `inherit_rate_metadata_for_chain` — parse the stored
//!   String/i64 fields back into an `aberp_billing::RateMetadata`
//!   value the chain child can pass to `allocate_in_tx` /
//!   `render_storno_data` / `render_modification_data`. The chain
//!   child INHERITS rate + source + date verbatim (regulatory
//!   consistency per A150) but COMPUTES its own
//!   `huf_equivalent_total` from its own gross EUR cents.
//! - `require_chain_currency_match` — defensive C6 invariant guard.
//!   Called at the chain-issuance entry point to loud-fail if the
//!   constructed `AllocateArgs.currency` does not equal the base's
//!   stored currency. Unit-tested directly so the loud-fail surface
//!   has a pin even when the CLI never produces a mismatch by
//!   construction.

use aberp_billing::{Currency, RateMetadata};
use anyhow::{anyhow, Result};
use rust_decimal::Decimal;
use std::str::FromStr;
use time::macros::format_description;
use time::Date;

/// Stored-row currency + rate metadata. Mirrors the five PR-44γ-added
/// columns on the `invoice` DuckDB row.
///
/// `currency` is decoded from VARCHAR via the closed vocab per
/// ADR-0037 §3 (`"HUF"` → `Currency::Huf`, `"EUR"` → `Currency::Eur`,
/// NULL → `Currency::Huf` per the migration backfill posture). The
/// four optional fields are populated iff `currency != Currency::Huf`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvoiceCurrencyMetadata {
    pub currency: Currency,
    /// 6-decimal `rust_decimal::Decimal::to_string` form
    /// (`"405.230000"`); `None` iff `currency == Currency::Huf`.
    pub exchange_rate: Option<String>,
    /// `"MNB"` (per ADR-0037 §2.a); `None` iff
    /// `currency == Currency::Huf`.
    pub exchange_rate_source: Option<String>,
    /// ISO-8601 `YYYY-MM-DD`; `None` iff `currency == Currency::Huf`.
    pub exchange_rate_date: Option<String>,
    /// Whole forints; `None` iff `currency == Currency::Huf`.
    pub huf_equivalent_total: Option<i64>,
}

/// Read the five PR-44γ-added currency columns off an `invoice` row
/// in the caller's transaction.
///
/// DECIMAL columns are CAST to VARCHAR so the duckdb-rs side-of-the-
/// wire types stay simple (no `rust_decimal` dep at the read path;
/// callers that need typed arithmetic parse via
/// [`inherit_rate_metadata_for_chain`]). The DATE column is CAST
/// to VARCHAR so the wire form is canonical ISO-8601 `YYYY-MM-DD`.
///
/// NULL in the `currency` column is treated as HUF — the only
/// pre-PR-44γ value (the migration backfill writes `'HUF'` on
/// existing rows; fresh DBs INSERT the explicit string). An unknown
/// non-null currency string is a loud-fail per CLAUDE.md rule 12
/// (DB tampering or schema drift).
pub fn load_invoice_currency_metadata_in_tx(
    tx: &duckdb::Transaction<'_>,
    invoice_id: &str,
) -> Result<InvoiceCurrencyMetadata> {
    let mut stmt = tx
        .prepare(
            "SELECT currency,
                    CAST(exchange_rate AS VARCHAR),
                    exchange_rate_source,
                    CAST(exchange_rate_date AS VARCHAR),
                    CAST(huf_equivalent_total AS BIGINT)
             FROM invoice WHERE id = ?;",
        )
        .map_err(|e| anyhow!("prepare invoice currency-metadata SELECT: {e}"))?;
    let row = stmt
        .query_row([invoice_id], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<i64>>(4)?,
            ))
        })
        .map_err(|e| anyhow!("read invoice currency-metadata for id {invoice_id}: {e}"))?;
    let currency = match row.0.as_deref() {
        None | Some("HUF") => Currency::Huf,
        Some("EUR") => Currency::Eur,
        Some(other) => {
            return Err(anyhow!(
                "invoice.currency column for {invoice_id} has unknown value {other:?} \
                 — DB tampering or schema drift (ADR-0037 §3)"
            ))
        }
    };
    Ok(InvoiceCurrencyMetadata {
        currency,
        exchange_rate: row.1,
        exchange_rate_source: row.2,
        exchange_rate_date: row.3,
        huf_equivalent_total: row.4,
    })
}

/// Build the chain-child's `(Currency, Option<RateMetadata>)` from the
/// BASE invoice's stored metadata + the chain child's own gross EUR
/// cents.
///
/// Per A150 (frozen rate, recomputed huf_equivalent): the chain child
/// INHERITS `rate`, `source`, `date` from the base — these are
/// regulatorily frozen at the base's value (a fresh MNB fetch at
/// storno-issuance time would risk a different rate for the same
/// referenced invoice, which ADR-0037 §4 invariant C6 prohibits).
/// The chain child COMPUTES its own `huf_equivalent_total` against
/// its own gross EUR cents using the inherited rate. For a storno,
/// the caller passes the negated gross (matching what
/// `nav_xml::render_storno_data` emits on the wire); for a
/// modification, the caller passes the full-replace gross.
///
/// For a HUF base the function returns `(Currency::Huf, None)` —
/// HUF chain children carry no rate metadata, same C10 byte-identical
/// invariant prerequisite as fresh-issuance HUF invoices.
///
/// # Loud-fail cases
///
/// - Base currency is non-HUF but any of `exchange_rate` /
///   `exchange_rate_source` / `exchange_rate_date` is missing —
///   the DB row is corrupt or hand-edited (C1's read-side counterpart).
/// - `exchange_rate` does not parse as `rust_decimal::Decimal` — same
///   DB-tamper class.
/// - `exchange_rate_date` does not parse as `YYYY-MM-DD` — same.
/// - Computing `huf_equivalent_round_half_even(child_gross_cents, rate)`
///   returns `None` (arithmetic overflow per §1.c).
pub fn inherit_rate_metadata_for_chain(
    base_metadata: &InvoiceCurrencyMetadata,
    child_gross_cents: i64,
) -> Result<(Currency, Option<RateMetadata>)> {
    if matches!(base_metadata.currency, Currency::Huf) {
        return Ok((Currency::Huf, None));
    }
    let rate_str = base_metadata.exchange_rate.as_deref().ok_or_else(|| {
        anyhow!(
            "base invoice has non-HUF currency {} but missing exchange_rate column \
             — DB row corrupt (ADR-0037 §4 invariant C1, read-side counterpart)",
            base_metadata.currency.iso_code(),
        )
    })?;
    let source = base_metadata.exchange_rate_source.clone().ok_or_else(|| {
        anyhow!(
            "base invoice has non-HUF currency {} but missing exchange_rate_source column \
             — DB row corrupt (ADR-0037 §4 invariant C1, read-side counterpart)",
            base_metadata.currency.iso_code(),
        )
    })?;
    let date_str = base_metadata.exchange_rate_date.as_deref().ok_or_else(|| {
        anyhow!(
            "base invoice has non-HUF currency {} but missing exchange_rate_date column \
             — DB row corrupt (ADR-0037 §4 invariant C1, read-side counterpart)",
            base_metadata.currency.iso_code(),
        )
    })?;
    let rate = Decimal::from_str(rate_str).map_err(|_| {
        anyhow!(
            "base invoice exchange_rate value `{}` is not a parseable decimal — \
             DB row corrupt or schema-drifted",
            rate_str
        )
    })?;
    let date =
        Date::parse(date_str, &format_description!("[year]-[month]-[day]")).map_err(|e| {
            anyhow!(
                "base invoice exchange_rate_date `{}` does not parse as YYYY-MM-DD: {e} \
             — DB row corrupt or schema-drifted",
                date_str
            )
        })?;
    let huf_equivalent_total =
        aberp_billing::huf_equivalent_round_half_even(child_gross_cents, &rate).ok_or_else(
            || {
                anyhow!(
                    "chain-child gross {} cents × inherited rate {} overflows i64 HUF equivalent \
             (ADR-0037 §1.c)",
                    child_gross_cents,
                    rate
                )
            },
        )?;
    Ok((
        base_metadata.currency,
        Some(RateMetadata {
            rate,
            source,
            date,
            huf_equivalent_total,
        }),
    ))
}

/// Defensive guard for ADR-0037 §4 invariant C6 — chain children MUST
/// be denominated in the same currency as their base. The natural
/// inheritance path (build the chain child's `AllocateArgs.currency`
/// from the base's stored currency via
/// [`inherit_rate_metadata_for_chain`]) makes a runtime mismatch
/// impossible by construction; this helper exists as a defensive
/// invariant check so a future code change that breaks inheritance
/// (e.g., a CLI `--currency` flag added without the matching
/// inheritance refusal) surfaces LOUD instead of silently coercing.
///
/// Pinned by the `chain_currency_mismatch_*` unit tests in this
/// module — both directions (HUF child + EUR base; EUR child + HUF
/// base) loud-fail with the literal `ChainCurrencyMismatch` token in
/// the error message so an audit-evidence reader can grep for it.
pub fn require_chain_currency_match(
    base_currency: Currency,
    child_currency: Currency,
    base_invoice_id: &str,
) -> Result<()> {
    if base_currency == child_currency {
        Ok(())
    } else {
        Err(anyhow!(
            "ChainCurrencyMismatch: chain-child invoice currency {} does not match base \
             invoice {} currency {} (ADR-0037 §4 invariant C6 — cross-currency chain \
             children are not regulatorily permitted)",
            child_currency.iso_code(),
            base_invoice_id,
            base_currency.iso_code(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aberp_billing::{huf_equivalent_round_half_even, Currency};

    fn eur_base_metadata() -> InvoiceCurrencyMetadata {
        InvoiceCurrencyMetadata {
            currency: Currency::Eur,
            exchange_rate: Some("405.230000".to_string()),
            exchange_rate_source: Some("MNB".to_string()),
            exchange_rate_date: Some("2026-05-08".to_string()),
            huf_equivalent_total: Some(40_523),
        }
    }

    fn huf_base_metadata() -> InvoiceCurrencyMetadata {
        InvoiceCurrencyMetadata {
            currency: Currency::Huf,
            exchange_rate: None,
            exchange_rate_source: None,
            exchange_rate_date: None,
            huf_equivalent_total: None,
        }
    }

    /// Happy-path inheritance: EUR base + 10000 EUR cents (100 EUR)
    /// of child gross. Inherited rate `405.230000` × 100 = `40523.0`
    /// HUF; round-half-even rounds to 40523.
    #[test]
    fn inherit_rate_metadata_eur_base_inherits_rate_source_date_and_recomputes_huf() {
        let base = eur_base_metadata();
        let (currency, rate_metadata) = inherit_rate_metadata_for_chain(&base, 10_000).unwrap();
        assert_eq!(currency, Currency::Eur);
        let rate = rate_metadata.expect("EUR child must carry rate_metadata");
        // Decimal `to_string` preserves the canonical decimal form
        // it was parsed from; we re-parse via Decimal so the
        // assertion is shape-agnostic on trailing zeros.
        assert_eq!(rate.rate, Decimal::from_str("405.230000").unwrap());
        assert_eq!(rate.source, "MNB");
        assert_eq!(rate.date.to_string(), "2026-05-08");
        // Recomputed huf_equivalent — NOT base's stored 40_523.
        let expected =
            huf_equivalent_round_half_even(10_000, &Decimal::from_str("405.230000").unwrap())
                .unwrap();
        assert_eq!(rate.huf_equivalent_total, expected);
    }

    /// Storno-direction: negated chain-child gross produces a negative
    /// huf_equivalent. Pins the regulatory negation invariant for the
    /// audit-ledger stamp: a storno's regulatory HUF amount is negative
    /// (the reversal of the base's positive amount).
    #[test]
    fn inherit_rate_metadata_eur_base_negative_child_gross_produces_negative_huf() {
        let base = eur_base_metadata();
        let (_currency, rate_metadata) = inherit_rate_metadata_for_chain(&base, -10_000).unwrap();
        let rate = rate_metadata.unwrap();
        assert!(
            rate.huf_equivalent_total < 0,
            "negated EUR cents must produce negative HUF equivalent: got {}",
            rate.huf_equivalent_total
        );
    }

    /// HUF base → HUF child, no rate metadata (the C10 byte-identical
    /// invariant prerequisite holds for chain children too).
    #[test]
    fn inherit_rate_metadata_huf_base_produces_none() {
        let base = huf_base_metadata();
        let (currency, rate_metadata) = inherit_rate_metadata_for_chain(&base, 12_345).unwrap();
        assert_eq!(currency, Currency::Huf);
        assert!(rate_metadata.is_none());
    }

    /// A non-HUF base with a NULL exchange_rate column means a corrupt
    /// row (the column is required when currency is non-HUF per
    /// C1's read-side counterpart). Loud-fail per CLAUDE.md rule 12.
    #[test]
    fn inherit_rate_metadata_loud_fails_on_eur_base_with_missing_rate() {
        let mut base = eur_base_metadata();
        base.exchange_rate = None;
        let err = inherit_rate_metadata_for_chain(&base, 1).unwrap_err();
        assert!(
            format!("{err:#}").contains("missing exchange_rate"),
            "error must name the missing column: {err}"
        );
    }

    /// ChainCurrencyMismatch loud-fail: HUF child against EUR base.
    /// The literal `ChainCurrencyMismatch` token appears in the
    /// message so an audit-evidence reader can grep for it.
    #[test]
    fn chain_currency_mismatch_huf_child_against_eur_base_loud_fails() {
        let err = require_chain_currency_match(Currency::Eur, Currency::Huf, "inv_BASE_FOR_EUR")
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("ChainCurrencyMismatch"),
            "loud-fail message must carry the ChainCurrencyMismatch token: {msg}"
        );
        assert!(
            msg.contains("inv_BASE_FOR_EUR"),
            "loud-fail message must name the base invoice id: {msg}"
        );
    }

    /// ChainCurrencyMismatch loud-fail: EUR child against HUF base.
    /// Symmetric to the above; ensures the helper doesn't silently
    /// coerce in either direction.
    #[test]
    fn chain_currency_mismatch_eur_child_against_huf_base_loud_fails() {
        let err = require_chain_currency_match(Currency::Huf, Currency::Eur, "inv_BASE_FOR_HUF")
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("ChainCurrencyMismatch"),
            "loud-fail message must carry the ChainCurrencyMismatch token: {err:#}"
        );
    }

    /// Matching pairs accept silently — the natural inheritance path
    /// the chain code paths take. Both directions pinned.
    #[test]
    fn chain_currency_match_matching_pairs_accepted() {
        require_chain_currency_match(Currency::Eur, Currency::Eur, "inv_x").unwrap();
        require_chain_currency_match(Currency::Huf, Currency::Huf, "inv_y").unwrap();
    }
}
