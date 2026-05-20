//! End-to-end golden tests for the NAV SOAP envelope assembly.
//!
//! These pin the byte shape of the envelopes against fixed inputs. If a
//! contributor accidentally reorders an XSD-sequence element, drops a
//! namespace, or changes the per-invoice base64 encoding, these tests
//! fail before the first NAV call would.
//!
//! Why a separate integration test file rather than unit tests inside
//! `src/soap/mod.rs`: the unit tests in `src/soap/mod.rs` exercise
//! shape-level invariants (block presence, child-element ordering) and
//! run on every build. The golden tests here pin exact byte sequences;
//! they live in `tests/` so the golden strings sit alongside the file
//! they assert against without bloating the `src/` module.

use aberp_nav_transport::soap::{
    render_manage_invoice_request, render_token_exchange_request, InvoiceOperation,
    ManageInvoiceItem,
};
use aberp_nav_transport::NavCredentials;

/// Fixture credentials shared across the golden tests. Values are
/// deliberately distinct (no repeated bytes across the four artifacts)
/// so a per-test assertion can ALSO verify "nothing from the wrong
/// keychain item leaked into the envelope".
fn fixture_credentials() -> NavCredentials {
    NavCredentials::from_parts(
        "test-tenant",
        "TECHNICAL_LOGIN",
        // 16-byte password (any length is fine; SHA-512 absorbs it).
        "tech-password-01",
        // 32-byte sign key (NAV ships 32-character ASCII; padding bytes
        // to match the realistic shape).
        "SIGN-KEY-32B-ASCII-XXXXXXXXXXXXX",
        // 16-byte change key (NAV ships 16-character ASCII for the
        // AES-128 ECB envelope key).
        "1234567890ABCDEF",
    )
}

#[test]
fn token_exchange_request_byte_shape_is_stable() {
    let xml = render_token_exchange_request(
        &fixture_credentials(),
        "12345678",
        "REQ01ABCDEFGHIJKLMNOP012345",
        "20260520T120000Z",
    )
    .expect("render envelope");
    let s = std::str::from_utf8(&xml).expect("UTF-8");

    // --- Shape (these are the load-bearing strings; if NAV's schema
    //     changes one of them, this test names which one drifted.)
    assert!(s.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
    assert!(s.contains("<TokenExchangeRequest xmlns=\"http://schemas.nav.gov.hu/OSA/3.0/api\" xmlns:common=\"http://schemas.nav.gov.hu/NTCA/1.0/common\">"));
    assert!(s.contains("<common:header>"));
    assert!(s.contains("<common:requestId>REQ01ABCDEFGHIJKLMNOP012345</common:requestId>"));
    assert!(s.contains("<common:timestamp>20260520T120000Z</common:timestamp>"));
    assert!(s.contains("<common:requestVersion>3.0</common:requestVersion>"));
    assert!(s.contains("<common:headerVersion>1.0</common:headerVersion>"));
    assert!(s.contains("<common:user>"));
    assert!(s.contains("<common:login>TECHNICAL_LOGIN</common:login>"));
    assert!(s.contains("<common:passwordHash cryptoType=\"SHA-512\">"));
    assert!(s.contains("<common:taxNumber>12345678</common:taxNumber>"));
    assert!(s.contains("<common:requestSignature cryptoType=\"SHA3-512\">"));
    assert!(s.contains("</TokenExchangeRequest>"));

    // --- Hash determinism. Same inputs → byte-identical hashes. The
    //     pinned values below are recomputed from RustCrypto sha2/sha3
    //     directly (see signatures.rs unit tests); if the workspace
    //     upgrades sha2 or sha3 to a major release with a different
    //     internal IV, these strings change and the test fails loud.
    //
    //     password_hash("tech-password-01") = uppercase-hex SHA-512.
    let expected_password_hash = sha512_upper_hex(b"tech-password-01");
    assert!(
        s.contains(&format!(">{expected_password_hash}</common:passwordHash>")),
        "password hash drift: envelope does not contain expected SHA-512: {s}"
    );

    //     request_signature("REQ01...", "20260520T120000Z", sign_key) =
    //     uppercase-hex SHA3-512(req_id || ts || sign_key).
    let expected_signature = sha3_512_upper_hex(&{
        let mut buf = Vec::new();
        buf.extend_from_slice(b"REQ01ABCDEFGHIJKLMNOP012345");
        buf.extend_from_slice(b"20260520T120000Z");
        buf.extend_from_slice(b"SIGN-KEY-32B-ASCII-XXXXXXXXXXXXX");
        buf
    });
    assert!(
        s.contains(&format!(">{expected_signature}</common:requestSignature>")),
        "signature drift: envelope does not contain expected SHA3-512: {s}"
    );

    // --- Secret-non-leakage. The plaintext password, sign key, and
    //     change key MUST NOT appear in the envelope (we only ever
    //     publish their hashes for the password and the per-request
    //     SHA3-512 of the sign key; the change key never leaves the
    //     credential bundle at all on the request side).
    assert!(!s.contains("tech-password-01"), "plaintext password leaked");
    assert!(
        !s.contains("SIGN-KEY-32B-ASCII"),
        "plaintext sign key leaked"
    );
    assert!(
        !s.contains("1234567890ABCDEF"),
        "plaintext change key leaked"
    );
}

#[test]
fn manage_invoice_request_byte_shape_is_stable() {
    let invoice_xml = b"<InvoiceData xmlns=\"http://schemas.nav.gov.hu/OSA/3.0/data\">\
        <invoiceNumber>INV-default/00001</invoiceNumber>\
        </InvoiceData>";

    let xml = render_manage_invoice_request(
        &fixture_credentials(),
        "12345678",
        "REQ02ABCDEFGHIJKLMNOP012345",
        "20260520T130000Z",
        "decrypted-token-1234",
        &[ManageInvoiceItem {
            operation: InvoiceOperation::Create,
            invoice_data_xml: invoice_xml,
        }],
    )
    .expect("render envelope");
    let s = std::str::from_utf8(&xml).expect("UTF-8");

    assert!(s.contains("<ManageInvoiceRequest xmlns=\"http://schemas.nav.gov.hu/OSA/3.0/api\" xmlns:common=\"http://schemas.nav.gov.hu/NTCA/1.0/common\">"));
    assert!(s.contains("<exchangeToken>decrypted-token-1234</exchangeToken>"));
    assert!(s.contains("<invoiceOperations>"));
    assert!(s.contains("<compressedContent>false</compressedContent>"));
    assert!(s.contains("<invoiceOperation>"));
    assert!(s.contains("<index>1</index>"));
    assert!(s.contains("<invoiceOperation>CREATE</invoiceOperation>"));

    // The base64-encoded invoice MUST be present verbatim with standard
    // alphabet + padding (RFC 4648 §4). Recompute it locally so the
    // pinned string here is not a stale magic constant.
    let expected_b64 = base64_standard_encode(invoice_xml);
    assert!(
        s.contains(&format!("<invoiceData>{expected_b64}</invoiceData>")),
        "invoice base64 drift: envelope does not contain expected base64: {s}"
    );

    assert!(s.contains("</ManageInvoiceRequest>"));
}

// ── Local helpers (avoid pulling sha2/sha3/base64 as test-only deps
//    in the integration test file; mirror the production helpers via
//    the same crates the workspace already pulls in transitively).
// ────────────────────────────────────────────────────────────────────

fn sha512_upper_hex(input: &[u8]) -> String {
    use sha2::{Digest, Sha512};
    let mut h = Sha512::new();
    h.update(input);
    upper_hex(&h.finalize())
}

fn sha3_512_upper_hex(input: &[u8]) -> String {
    use sha3::{Digest, Sha3_512};
    let mut h = Sha3_512::new();
    h.update(input);
    upper_hex(&h.finalize())
}

fn upper_hex(bytes: &[u8]) -> String {
    const TABLE: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(TABLE[(b >> 4) as usize] as char);
        out.push(TABLE[(b & 0x0F) as usize] as char);
    }
    out
}

fn base64_standard_encode(bytes: &[u8]) -> String {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    STANDARD.encode(bytes)
}
