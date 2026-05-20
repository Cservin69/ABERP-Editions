//! NAV application-level authentication hashes per ADR-0009 §4 + ADR-0020 §2.
//!
//! Two artifacts go into every NAV SOAP envelope's `<user>` block:
//!
//!   1. `passwordHash` — SHA-512 of the technical-user password.
//!      `cryptoType="SHA-512"`. Recomputed per request.
//!   2. `requestSignature` — SHA3-512 over the documented input string for
//!      the called operation. `cryptoType="SHA3-512"`. For non-`manageInvoice`
//!      / non-`manageAnnulment` operations the input is exactly:
//!
//!      ```text
//!      requestId || requestTimestamp(YYYYMMDDhhmmss, UTC) || xmlSignKey
//!      ```
//!
//!      For `manageInvoice` and `manageAnnulment` the input is extended by a
//!      per-invoice-index suffix:
//!
//!      ```text
//!      ... || SHA3-512_hex(operation || base64(invoiceData))   per index,
//!                                                                concatenated
//!      ```
//!
//! # Output encoding
//!
//! Both hashes are emitted as **uppercase hexadecimal** strings. The NAV v3.0
//! XSD types `SHA512Type` and `SHA3-512Type` both pin `[0-9A-F]{128}`. Lower-
//! case hex is REJECTED by NAV with `INVALID_REQUEST_SIGNATURE`. Verified by
//! inspection against the v3.0 XSD; cross-checked against `pzs/php-nav-online-
//! szamla` (PHP) and `angro-kft/nav-online-szamla` (Node) — both emit upper-
//! case hex.
//!
//! # What this module does NOT do
//!
//!   - It does not load credentials (that is `crate::credentials`).
//!   - It does not build the SOAP envelope (that is `crate::soap`).
//!   - It does not call NAV (that is `crate::operations`).
//!
//! This module is pure: same inputs → same outputs, byte-for-byte. The unit
//! tests below assert that property against fixed inputs so regressions
//! surface at unit-test time, not at the first failed NAV submission.
//!
//! # Why the per-invoice extension is built as a separate helper
//!
//! `request_signature_manage` is exposed alongside `request_signature` rather
//! than collapsed into a single "maybe-pass-Vec" function because (a) the two
//! input strings are different shapes (extra suffix vs not), (b) the call
//! sites are different operations with different validation rules, and (c)
//! the failure mode of "accidentally pass `&[]` to the manage form when you
//! meant to call the non-manage form" is exactly the silent-degradation
//! pattern CLAUDE.md rule 12 names.

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;
use sha2::{Digest as _, Sha512};
use sha3::Sha3_512;

/// SHA-512 of `password` rendered as uppercase hex. Used as the per-request
/// `<passwordHash>` value with `cryptoType="SHA-512"`.
///
/// Note the input is `&[u8]` (not `&str`): the password is a secret the
/// caller fetched from the keychain via `NavCredentials::password_bytes()`,
/// and the byte slice form keeps the caller from accidentally `Display`-
/// formatting it through a `&str` somewhere.
pub fn password_hash(password: &[u8]) -> String {
    let mut hasher = Sha512::new();
    hasher.update(password);
    hex_upper(&hasher.finalize())
}

/// SHA3-512 of the request-signature input for a non-`manageInvoice` /
/// non-`manageAnnulment` operation (e.g. `tokenExchange`,
/// `queryTransactionStatus`, `queryInvoiceCheck`, `queryInvoiceDigest`).
///
/// Input concatenation per ADR-0009 §4 + ADR-0020 §2:
///
///   request_id || request_timestamp || xml_sign_key
///
/// `request_timestamp` must already be in the NAV-mandated form
/// `YYYYMMDDhhmmss` UTC (see [`crate::soap::parts::request_timestamp`]).
/// `xml_sign_key` is the bytes returned by
/// `NavCredentials::sign_key_bytes()`.
pub fn request_signature(request_id: &str, request_timestamp: &str, xml_sign_key: &[u8]) -> String {
    let mut hasher = Sha3_512::new();
    hasher.update(request_id.as_bytes());
    hasher.update(request_timestamp.as_bytes());
    hasher.update(xml_sign_key);
    hex_upper(&hasher.finalize())
}

/// SHA3-512 of the request-signature input for `manageInvoice` /
/// `manageAnnulment`.
///
/// Same prefix as [`request_signature`], extended by — for each per-index
/// invoice in `invoice_inputs` (in index order; index 1, 2, 3, ...) — the
/// **uppercase-hex** SHA3-512 of:
///
/// ```text
/// operation || base64(invoice_data_xml)
/// ```
///
/// concatenated onto the running input. Per ADR-0009 §4:
///
///   "`manageInvoice` / `manageAnnulment`: same input, plus per
///    invoice-index a SHA3-512 of `operation || base64(invoiceData)`,
///    concatenated in index order."
///
/// `operation` is the SOAP `manageInvoiceOperation/operation` enum value
/// (`"CREATE"`, `"MODIFY"`, `"STORNO"` for `manageInvoice`; `"ANNUL"` for
/// `manageAnnulment`). `invoice_data_xml` is the raw XML bytes of the
/// `<InvoiceData>` element (the same bytes the binary writes to disk in
/// PR-5, base64-encoded onto the wire). The base64 encoding here is
/// **standard alphabet with padding** per RFC 4648 §4 — NAV does NOT
/// accept URL-safe or unpadded forms.
///
/// **Length constraint:** NAV's v3.0 `manageInvoice` request caps the per-
/// index `<invoiceOperation>` block at 100 entries. This function does not
/// enforce that — the caller (the `manage_invoice` operation in
/// `crate::operations`) validates length before invoking. Keeping the
/// validation upstream of the signature lets the unit tests here exercise
/// the signature math without inheriting a business-rule constraint.
pub fn request_signature_manage(
    request_id: &str,
    request_timestamp: &str,
    xml_sign_key: &[u8],
    invoice_inputs: &[InvoiceSignatureInput<'_>],
) -> String {
    let mut hasher = Sha3_512::new();
    hasher.update(request_id.as_bytes());
    hasher.update(request_timestamp.as_bytes());
    hasher.update(xml_sign_key);
    for input in invoice_inputs {
        let suffix_hex = per_invoice_hex(input);
        hasher.update(suffix_hex.as_bytes());
    }
    hex_upper(&hasher.finalize())
}

/// One per-invoice-index contribution to a `manageInvoice` /
/// `manageAnnulment` signature.
///
/// Borrowing both fields keeps the caller's payload-ownership model
/// unaltered — the invoice XML stays in its `Vec<u8>`/`&[u8]` original
/// without an additional copy.
#[derive(Debug, Clone, Copy)]
pub struct InvoiceSignatureInput<'a> {
    /// `"CREATE"` | `"MODIFY"` | `"STORNO"` for `manageInvoice`;
    /// `"ANNUL"` for `manageAnnulment`. Passed through verbatim — the
    /// operation must already match what the SOAP envelope's
    /// `<invoiceOperation>/<operation>` element carries; mismatch
    /// produces `INVALID_REQUEST_SIGNATURE` from NAV.
    pub operation: &'a str,

    /// Raw `<InvoiceData>` XML bytes for this index. Base64-encoded
    /// (standard alphabet with padding) before hashing per ADR-0009 §4.
    pub invoice_data_xml: &'a [u8],
}

// ──────────────────────────────────────────────────────────────────────
// Internal helpers
// ──────────────────────────────────────────────────────────────────────

/// Compute the per-invoice-index hex suffix for one input.
/// Extracted so the unit test below can exercise the per-index math
/// directly without going through the full `request_signature_manage`.
fn per_invoice_hex(input: &InvoiceSignatureInput<'_>) -> String {
    let base64_xml = BASE64_STANDARD.encode(input.invoice_data_xml);
    let mut hasher = Sha3_512::new();
    hasher.update(input.operation.as_bytes());
    hasher.update(base64_xml.as_bytes());
    hex_upper(&hasher.finalize())
}

/// Encode a hash as uppercase hex. NAV's XSD types pin `[0-9A-F]{128}`;
/// lowercase or mixed-case is rejected with `INVALID_REQUEST_SIGNATURE`.
/// Kept private so all hex emission for NAV goes through one place.
fn hex_upper(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        // `format!` with `{:02X}` is the obvious option but allocates
        // per byte; a hand loop with the lookup table is constant-time
        // (cache-line) and avoids the per-call allocation. The hex
        // crate would do this too but adds a dep for two ten-line
        // helpers (CLAUDE.md rule 2 — simplicity first).
        const TABLE: &[u8; 16] = b"0123456789ABCDEF";
        out.push(TABLE[(b >> 4) as usize] as char);
        out.push(TABLE[(b & 0x0F) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── password_hash ────────────────────────────────────────────────

    /// SHA-512("") expected output, uppercase hex. Pinned against NIST's
    /// FIPS-180-4 published test vector for the empty input. If this
    /// fails, the sha2 crate is doing something exotic — surface loud.
    const EMPTY_SHA512_HEX: &str =
        "CF83E1357EEFB8BDF1542850D66D8007D620E4050B5715DC83F4A921D36CE9CE\
         47D0D13C5D85F2B0FF8318D2877EEC2F63B931BD47417A81A538327AF927DA3E";

    #[test]
    fn password_hash_matches_fips_180_4_empty_vector() {
        assert_eq!(
            password_hash(b""),
            EMPTY_SHA512_HEX.replace([' ', '\n'], "")
        );
    }

    #[test]
    fn password_hash_is_deterministic() {
        let a = password_hash(b"correct horse battery staple");
        let b = password_hash(b"correct horse battery staple");
        assert_eq!(a, b);
        assert_eq!(a.len(), 128, "SHA-512 hex is exactly 128 chars");
    }

    #[test]
    fn password_hash_is_uppercase_hex_only() {
        let h = password_hash(b"hunter2");
        assert!(
            h.chars().all(|c| matches!(c, '0'..='9' | 'A'..='F')),
            "NAV requires uppercase hex; got: {h}"
        );
    }

    #[test]
    fn password_hash_handles_non_ascii_password_bytes() {
        // Passwords sourced from the keychain are byte-shaped; UTF-8
        // sequences must hash by their bytes, not by their codepoints.
        // The test guards against a future contributor reaching for a
        // `&str`-only API on `sha2`.
        let utf8_password = "jelszó-Á-日本".as_bytes();
        let h = password_hash(utf8_password);
        assert_eq!(h.len(), 128);
    }

    // ── request_signature (non-manage) ───────────────────────────────

    #[test]
    fn request_signature_is_deterministic() {
        let a = request_signature("REQ-1", "20260520T120000Z", b"sign-key");
        let b = request_signature("REQ-1", "20260520T120000Z", b"sign-key");
        assert_eq!(a, b);
        assert_eq!(a.len(), 128);
    }

    #[test]
    fn request_signature_distinguishes_inputs() {
        let base = request_signature("REQ-1", "20260520T120000Z", b"sign-key");
        let other_id = request_signature("REQ-2", "20260520T120000Z", b"sign-key");
        let other_ts = request_signature("REQ-1", "20260520T120001Z", b"sign-key");
        let other_key = request_signature("REQ-1", "20260520T120000Z", b"sign-key-2");
        assert_ne!(base, other_id);
        assert_ne!(base, other_ts);
        assert_ne!(base, other_key);
    }

    /// SHA3-512("") expected output (FIPS 202 / Keccak Team test vector).
    /// Concatenating three empty inputs is the same as hashing the empty
    /// string — guards against a future contributor accidentally inserting
    /// a separator byte between `update()` calls.
    const EMPTY_SHA3_512_HEX: &str =
        "A69F73CCA23A9AC5C8B567DC185A756E97C982164FE25859E0D1DCC1475C80A6\
         15B2123AF1F5F94C11E3E9402C3AC558F500199D95B6D3E301758586281DCD26";

    #[test]
    fn request_signature_concatenation_has_no_implicit_separator() {
        let h = request_signature("", "", b"");
        assert_eq!(h, EMPTY_SHA3_512_HEX.replace([' ', '\n'], ""));
    }

    // ── request_signature_manage ─────────────────────────────────────

    #[test]
    fn manage_signature_with_zero_invoices_equals_non_manage() {
        // A manage call with no invoice inputs reduces to the same hash
        // as the non-manage helper. This protects against a future
        // contributor inserting "if invoices.is_empty() { return X }"
        // shortcut that takes a different code path.
        let manage = request_signature_manage("REQ-1", "20260520T120000Z", b"sk", &[]);
        let plain = request_signature("REQ-1", "20260520T120000Z", b"sk");
        assert_eq!(manage, plain);
    }

    #[test]
    fn manage_signature_changes_when_invoice_payload_changes() {
        let h_a = request_signature_manage(
            "REQ-1",
            "20260520T120000Z",
            b"sk",
            &[InvoiceSignatureInput {
                operation: "CREATE",
                invoice_data_xml: b"<InvoiceData>A</InvoiceData>",
            }],
        );
        let h_b = request_signature_manage(
            "REQ-1",
            "20260520T120000Z",
            b"sk",
            &[InvoiceSignatureInput {
                operation: "CREATE",
                invoice_data_xml: b"<InvoiceData>B</InvoiceData>",
            }],
        );
        assert_ne!(h_a, h_b, "different payload → different signature");
    }

    #[test]
    fn manage_signature_is_order_sensitive() {
        let a = InvoiceSignatureInput {
            operation: "CREATE",
            invoice_data_xml: b"<InvoiceData>1</InvoiceData>",
        };
        let b = InvoiceSignatureInput {
            operation: "CREATE",
            invoice_data_xml: b"<InvoiceData>2</InvoiceData>",
        };
        let ab = request_signature_manage("REQ-1", "20260520T120000Z", b"sk", &[a, b]);
        let ba = request_signature_manage("REQ-1", "20260520T120000Z", b"sk", &[b, a]);
        assert_ne!(ab, ba, "per-ADR-0009 §4: concatenated in index order");
    }

    #[test]
    fn manage_signature_distinguishes_operation() {
        let xml = b"<InvoiceData>x</InvoiceData>";
        let create = request_signature_manage(
            "REQ-1",
            "20260520T120000Z",
            b"sk",
            &[InvoiceSignatureInput {
                operation: "CREATE",
                invoice_data_xml: xml,
            }],
        );
        let storno = request_signature_manage(
            "REQ-1",
            "20260520T120000Z",
            b"sk",
            &[InvoiceSignatureInput {
                operation: "STORNO",
                invoice_data_xml: xml,
            }],
        );
        assert_ne!(create, storno);
    }

    // ── per_invoice_hex internal ────────────────────────────────────

    #[test]
    fn per_invoice_hex_uses_standard_base64_with_padding() {
        // base64("ab") == "YWI=" (one pad). If the implementation ever
        // switches to URL-safe or unpadded form the suffix changes and
        // NAV rejects the request — catch it here, not in production.
        //
        // We don't pin the SHA3-512 hex (the value will need updating if
        // base64 alphabet ever changes); we just assert that the suffix
        // is exactly SHA3-512_hex("OP" || "YWI=").
        let observed = per_invoice_hex(&InvoiceSignatureInput {
            operation: "OP",
            invoice_data_xml: b"ab",
        });
        let mut expected = Sha3_512::new();
        expected.update(b"OP");
        expected.update(b"YWI=");
        let expected_hex = hex_upper(&expected.finalize());
        assert_eq!(observed, expected_hex);
    }

    // ── hex_upper invariant ─────────────────────────────────────────

    #[test]
    fn hex_upper_round_trips_zero_byte_and_high_byte() {
        // Hash bytes span the full 0x00..=0xFF range; ensure the
        // table-lookup hex emitter handles both ends. A handful of
        // bytes is enough to surface "the high nibble was masked"
        // class of bugs.
        assert_eq!(hex_upper(&[0x00]), "00");
        assert_eq!(hex_upper(&[0xFF]), "FF");
        assert_eq!(hex_upper(&[0xAB, 0xCD]), "ABCD");
        assert_eq!(hex_upper(&[0x12, 0x34, 0x56, 0x78]), "12345678");
    }
}
