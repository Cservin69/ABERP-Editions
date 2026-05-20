//! Live `tokenExchange` conformance test against NAV `api-test`.
//!
//! ENV-GATED. The test body runs only when `ABERP_NAV_LIVE_TEST=1` is
//! set; otherwise it returns early with a tracing-style `eprintln!`. This
//! matches the PR-7-A pattern (`tls_handshake.rs`) so CI does not need
//! NAV creds and offline contributors do not have a flaky-by-design test.
//!
//! Required environment when ABERP_NAV_LIVE_TEST=1 is set:
//!
//!   ABERP_NAV_LIVE_TEST=1
//!   ABERP_NAV_TENANT_ID=<tenant id whose keychain is populated>
//!   ABERP_NAV_TEST_TAX_NUMBER=<8-digit base of the test taxpayer>
//!
//! The four credential artifacts are loaded from the OS keychain via
//! the same `NavCredentials::load_from_keychain` path the binary uses
//! in production; the test does not accept credentials via env vars to
//! prevent a CI accident from leaking the test taxpayer's password
//! into a job log.

use aberp_nav_transport::{operations::token_exchange, NavCredentials, NavEndpoint, NavTransport};

#[tokio::test(flavor = "current_thread")]
async fn token_exchange_against_api_test() {
    if std::env::var("ABERP_NAV_LIVE_TEST").ok().as_deref() != Some("1") {
        eprintln!(
            "skipping token_exchange_against_api_test \
             (set ABERP_NAV_LIVE_TEST=1 + ABERP_NAV_TENANT_ID + \
             ABERP_NAV_TEST_TAX_NUMBER to run)"
        );
        return;
    }

    let tenant_id = std::env::var("ABERP_NAV_TENANT_ID")
        .expect("ABERP_NAV_TENANT_ID must be set when ABERP_NAV_LIVE_TEST=1");
    let tax_number_8 = std::env::var("ABERP_NAV_TEST_TAX_NUMBER")
        .expect("ABERP_NAV_TEST_TAX_NUMBER must be set when ABERP_NAV_LIVE_TEST=1");
    assert_eq!(
        tax_number_8.len(),
        8,
        "tax number base must be 8 digits, got {tax_number_8:?}"
    );

    let credentials = NavCredentials::load_from_keychain(&tenant_id)
        .expect("NAV credentials must be present in the OS keychain for this tenant");
    let transport = NavTransport::new(NavEndpoint::Test).expect("transport must construct");

    let outcome = token_exchange::call(&transport, &credentials, &tax_number_8)
        .await
        .expect("tokenExchange must succeed against api-test");

    // NAV's exchange tokens are short printable ASCII strings (the
    // consulted clients observed 16–32 chars). If this assertion ever
    // fires, it is most likely that the AES key was wrong (garbage
    // bytes through the UTF-8 check would be detected upstream, but
    // wrong-key UTF-8 has a non-zero probability of decoding) or that
    // NAV changed the token shape — both are loud-fail-worthy events.
    assert!(
        !outcome.decoded_token.is_empty(),
        "decoded token must be non-empty"
    );
    assert!(
        outcome.decoded_token.chars().all(|c| c.is_ascii_graphic()),
        "decoded token must be printable ASCII; got bytes that decoded \
         oddly — suspect xmlChangeKey mismatch"
    );
    assert!(
        !outcome.request_xml.is_empty(),
        "request_xml must be captured for audit"
    );
    assert!(
        !outcome.response_xml.is_empty(),
        "response_xml must be captured for audit"
    );
}
