//! Currency + decimal formatting for the Hungarian printed-invoice surface.
//!
//! Per the reference template (`reference_aberp_invoice_template.md`):
//! - Hungarian decimal **comma**, NOT period.
//! - Thousand-separator: narrow non-breaking space. WinAnsi has no
//!   narrow-NBSP code point; we use a regular space (U+0020). The
//!   reference's "narrow" suggestion is typographic polish; the
//!   regulatory record is the amount value, not the separator glyph.
//!   PR-44ε.2 may upgrade to a proper narrow space once the renderer
//!   embeds a Unicode font.
//! - EUR: `€` prefix with a regular space, two decimals.
//! - HUF: no symbol, suffix ` Ft` with a regular space, integer.
//! - Negative amounts: minus sign prefix INSIDE the currency symbol
//!   (`-€8 636,00`), not outside. Storno chain children carry negative
//!   line totals per ADR-0009 §6.

use aberp_billing::Currency;
use rust_decimal::Decimal;

/// S157 — format a decimal line quantity for the printed invoice using the
/// Hungarian decimal comma. `.normalize()` drops the trailing zeros a
/// DECIMAL(18,6) read-back carries, so `1.500000` → `1,5`, `3.000000` →
/// `3`, and `0.25` → `0,25`. `Decimal::to_string` is locale-independent
/// (always `.`), so the single `replace('.', ",")` is the only locale
/// transform needed. No thousands grouping — quantities are small counts,
/// not money amounts.
pub fn quantity(qty: Decimal) -> String {
    qty.normalize().to_string().replace('.', ",")
}

/// Format a minor-unit amount in the given currency for printed-invoice
/// display. EUR cents → "€X XXX,XX"; HUF forints → "X XXX Ft".
///
/// The `€` glyph is WinAnsi code point 0x80; the byte-emitter maps it
/// correctly via `text::winansi_byte_for_char`.
pub fn money(currency: Currency, minor: i64) -> String {
    match currency {
        Currency::Eur => format_eur_cents(minor),
        Currency::Huf => format_huf_forints(minor),
    }
}

/// Same as [`money`] but right-aligned by padding with leading spaces
/// to `width` columns. Used by the totals block where every label /
/// amount pair stacks on the right edge.
pub fn money_right_aligned(currency: Currency, minor: i64, width: usize) -> String {
    let s = money(currency, minor);
    if s.chars().count() >= width {
        s
    } else {
        let pad = width - s.chars().count();
        format!("{}{}", " ".repeat(pad), s)
    }
}

fn format_eur_cents(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let abs = cents.unsigned_abs();
    let whole = abs / 100;
    let frac = abs % 100;
    format!("{}\u{20AC}{},{:02}", sign, group_thousands(whole), frac)
}

fn format_huf_forints(forints: i64) -> String {
    let sign = if forints < 0 { "-" } else { "" };
    let abs = forints.unsigned_abs();
    format!("{}{} Ft", sign, group_thousands(abs))
}

/// Format a Hungarian-rate decimal value for the MEGJEGYZÉS / Árfolyam
/// surface — display precision, NOT the wire 6-decimal precision per
/// ADR-0037 §1.c ("the printed-invoice display per §1.a MAY show fewer
/// decimals"). The reference template uses two decimals (e.g.,
/// `356,69 Ft`). The conversion: parse the canonical-decimal string
/// from the audit-payload, format with the Hungarian comma, drop
/// trailing zeros beyond two decimals.
pub fn rate_for_display(canonical_decimal: &str) -> String {
    let (whole, frac) = canonical_decimal
        .split_once('.')
        .map(|(w, f)| (w, f))
        .unwrap_or((canonical_decimal, ""));
    let frac_two = if frac.len() >= 2 {
        &frac[..2]
    } else if frac.is_empty() {
        "00"
    } else {
        // Single-digit fractional — pad to two digits.
        return format!(
            "{},{}0",
            group_thousands(whole.parse::<u64>().unwrap_or(0)),
            frac,
        );
    };
    format!(
        "{},{}",
        group_thousands(whole.parse::<u64>().unwrap_or(0)),
        frac_two,
    )
}

/// Group thousands with a space separator (`1234567` → `1 234 567`).
fn group_thousands(mut n: u64) -> String {
    if n == 0 {
        return "0".to_string();
    }
    let mut groups: Vec<String> = Vec::new();
    while n > 0 {
        groups.push(format!("{:03}", n % 1000));
        n /= 1000;
    }
    let last_idx = groups.len() - 1;
    groups[last_idx] = groups[last_idx].trim_start_matches('0').to_string();
    if groups[last_idx].is_empty() {
        groups[last_idx] = "0".to_string();
    }
    groups.reverse();
    groups.join(" ")
}

/// Hungarian short-date format used on the reference template:
/// `2026. 05. 08.`
pub fn hungarian_date(d: time::Date) -> String {
    format!("{:04}. {:02}. {:02}.", d.year(), d.month() as u8, d.day())
}

/// ISO-style date for the performance period sub-line — matches the
/// reference template's `2026.04.01 – 2026.04.30`.
pub fn iso_dotted_date(d: time::Date) -> String {
    format!("{:04}.{:02}.{:02}", d.year(), d.month() as u8, d.day())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eur_cents_round_trip() {
        assert_eq!(money(Currency::Eur, 863_600), "\u{20AC}8 636,00");
        assert_eq!(money(Currency::Eur, 50), "\u{20AC}0,50");
        assert_eq!(money(Currency::Eur, -863_600), "-\u{20AC}8 636,00");
    }

    #[test]
    fn huf_forints_round_trip() {
        assert_eq!(money(Currency::Huf, 3_080_374), "3 080 374 Ft");
        assert_eq!(money(Currency::Huf, 0), "0 Ft");
        assert_eq!(money(Currency::Huf, -654_883), "-654 883 Ft");
    }

    #[test]
    fn quantity_uses_hungarian_comma_and_trims_trailing_zeros() {
        // S157 — headline: 1.5 renders as 1,5 (comma), integers stay bare.
        assert_eq!(quantity(Decimal::new(15, 1)), "1,5");
        assert_eq!(quantity(Decimal::from(1)), "1");
        // DECIMAL(18,6) read-back carries trailing zeros — normalize drops them.
        assert_eq!(quantity(Decimal::new(1_500_000, 6)), "1,5");
        assert_eq!(quantity(Decimal::new(3_000_000, 6)), "3");
        assert_eq!(quantity(Decimal::new(25, 2)), "0,25");
    }

    #[test]
    fn rate_display_drops_trailing_zeros_to_two_decimals() {
        assert_eq!(rate_for_display("356.690000"), "356,69");
        assert_eq!(rate_for_display("405.230000"), "405,23");
        assert_eq!(rate_for_display("100"), "100,00");
        assert_eq!(rate_for_display("100.5"), "100,50");
    }

    #[test]
    fn hungarian_date_format() {
        let d = time::Date::from_calendar_date(2026, time::Month::May, 8).unwrap();
        assert_eq!(hungarian_date(d), "2026. 05. 08.");
    }
}
