//! WinAnsi byte encoding for the printed-invoice surface.
//!
//! The renderer uses the built-in PDF font `Helvetica` with
//! `WinAnsiEncoding`. WinAnsi covers Latin-1 + the Microsoft extension
//! range (0x80-0x9F: smart quotes, dashes, `€`, etc.); it does NOT
//! cover the Hungarian-specific double-acute characters `ő/ű/Ő/Ű`
//! (those live in Latin-2 / Unicode U+0150/U+0151/U+0170/U+0171). The
//! reference template's Hungarian text includes these glyphs in words
//! like "időszak", "követelés", "összeg", "törvény", "végösszeg".
//!
//! # The substitution decision (A-decision recorded in
//! `_handoffs/56-session-56-close.md`)
//!
//! Per CLAUDE.md rule 12 (fail loud) + rule 2 (simplicity first):
//! - We substitute `ő → ö` and `ű → ü` at the byte-emit boundary,
//!   matching the visually-closest WinAnsi-covered diacritic.
//! - The substitution is documented LOUD inline (this module) so a
//!   future reader sees it immediately.
//! - The PR-44ε.2 deferred row in the session-56 close handoff names
//!   "proper Unicode font embedding (Type0/CIDFontType2 with Identity-H
//!   encoding)" as the fix — the renderer's WinAnsi posture is the
//!   surface that PR-44ε.2 replaces. The substitution is OBSERVABLE in
//!   the rendered PDF; it is NOT a silent loss.
//!
//! Why not embed a Unicode font now: per the brief's "ship what fits
//! and name the deferred piece" — Type0 font embedding is ~300 LoC of
//! CIDFontType2 glue (font subsetting, ToUnicode cmap, glyph-index
//! lookup via ttf-parser) that bloats THIS PR substantially. A152
//! records the trade-off; PR-44ε.2 lifts it.

/// Map a Unicode `char` to its WinAnsi byte, substituting Hungarian
/// double-acute characters to their WinAnsi-covered single-acute
/// equivalents. Unknown chars (anything outside ASCII + Latin-1 +
/// WinAnsi's 0x80-0x9F supplement, and the post-substitution
/// Hungarian set) emit `0x3F` (`?`) — visible to the reader, not a
/// silent drop.
///
/// The WinAnsi code-point assignments below come from the Adobe
/// WinAnsiEncoding spec (a near-superset of CP-1252).
pub fn winansi_byte_for_char(c: char) -> u8 {
    match c {
        // ASCII identity range.
        c if (c as u32) < 0x80 => c as u8,

        // Hungarian double-acute substitutions per A152. Documented
        // LOUD in this module's preamble; PR-44ε.2 lifts via font
        // embedding.
        '\u{0150}' => 0xD6, // Ő → Ö
        '\u{0151}' => 0xF6, // ő → ö
        '\u{0170}' => 0xDC, // Ű → Ü
        '\u{0171}' => 0xFC, // ű → ü

        // WinAnsi 0x80-0x9F supplement — the codepoints that diverge
        // from pure Latin-1.
        '\u{20AC}' => 0x80, // €
        '\u{201A}' => 0x82, // ‚
        '\u{0192}' => 0x83, // ƒ
        '\u{201E}' => 0x84, // „
        '\u{2026}' => 0x85, // …
        '\u{2020}' => 0x86, // †
        '\u{2021}' => 0x87, // ‡
        '\u{02C6}' => 0x88, // ˆ
        '\u{2030}' => 0x89, // ‰
        '\u{0160}' => 0x8A, // Š
        '\u{2039}' => 0x8B, // ‹
        '\u{0152}' => 0x8C, // Œ
        '\u{017D}' => 0x8E, // Ž
        '\u{2018}' => 0x91, // ‘
        '\u{2019}' => 0x92, // ’
        '\u{201C}' => 0x93, // “
        '\u{201D}' => 0x94, // ”
        '\u{2022}' => 0x95, // •
        '\u{2013}' => 0x96, // –
        '\u{2014}' => 0x97, // —
        '\u{02DC}' => 0x98, // ˜
        '\u{2122}' => 0x99, // ™
        '\u{0161}' => 0x9A, // š
        '\u{203A}' => 0x9B, // ›
        '\u{0153}' => 0x9C, // œ
        '\u{017E}' => 0x9E, // ž
        '\u{0178}' => 0x9F, // Ÿ

        // Latin-1 range — byte values are the same as Unicode code
        // points in 0xA0-0xFF.
        c if (c as u32) >= 0xA0 && (c as u32) <= 0xFF => c as u8,

        // Anything else: visible question mark per CLAUDE.md rule 12.
        _ => b'?',
    }
}

/// Convert a `&str` into the WinAnsi byte sequence the PDF content
/// stream's `Tj` operator consumes.
pub fn winansi_bytes(s: &str) -> Vec<u8> {
    s.chars().map(winansi_byte_for_char).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_identity() {
        assert_eq!(winansi_bytes("Szamla 2026"), b"Szamla 2026".to_vec());
    }

    #[test]
    fn euro_glyph_maps_to_0x80() {
        assert_eq!(
            winansi_bytes("\u{20AC}8 636"),
            vec![0x80, b'8', b' ', b'6', b'3', b'6']
        );
    }

    #[test]
    fn hungarian_single_acute_round_trip() {
        // á é í ó ú ö ü + Á É Í Ó Ú Ö Ü all in WinAnsi Latin-1 range.
        let bytes = winansi_bytes("Számla Összeg Áfa");
        assert_eq!(
            bytes,
            vec![
                b'S', b'z', 0xE1, b'm', b'l', b'a', b' ', 0xD6, b's', b's', b'z', b'e', b'g', b' ',
                0xC1, b'f', b'a',
            ]
        );
    }

    #[test]
    fn hungarian_double_acute_substituted_to_single_acute() {
        // ő → ö (0xF6); ű → ü (0xFC). Per A152 the substitution is
        // intentional and documented loud in the module preamble.
        assert_eq!(winansi_byte_for_char('\u{0151}'), 0xF6);
        assert_eq!(winansi_byte_for_char('\u{0171}'), 0xFC);
        assert_eq!(winansi_byte_for_char('\u{0150}'), 0xD6);
        assert_eq!(winansi_byte_for_char('\u{0170}'), 0xDC);
    }

    #[test]
    fn unknown_codepoint_maps_to_question_mark() {
        assert_eq!(winansi_byte_for_char('\u{4E2D}'), b'?'); // CJK
    }
}
