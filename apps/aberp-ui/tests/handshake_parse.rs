//! End-to-end conformance test for the handshake contract.
//!
//! The handshake line shape is the load-bearing contract between
//! `apps/aberp/src/serve.rs` (println) and
//! `apps/aberp-ui/src/handshake.rs` (parser). Either side drifting
//! silently is exactly the failure mode CLAUDE.md rule 12 names; this
//! test re-builds the exact string `serve.rs` prints and asserts the
//! parser accepts it.
//!
//! PR-45a / session-61 rewires this for the new READY-line shape —
//! `READY 127.0.0.1:<port> sha256:<hex>`. The addr token must
//! round-trip through `parse::<SocketAddr>()`; the brief named this
//! invariant as the load-bearing pin.
//!
//! PR-46α / session-62 — extends the round-trip to cover the optional
//! `state=<token>` suffix on the line. The legacy (pre-46α) shape
//! continues to parse cleanly as `state: Ready` for backwards
//! compatibility; the new `state=needs-setup` token routes the SPA's
//! first paint to the first-run setup wizard.

use std::net::SocketAddr;

use aberp_ui::handshake;
use aberp_ui::handshake::ServeBootState;

#[test]
fn serve_println_round_trip() {
    // The line shape — exactly as `apps/aberp/src/serve.rs` builds it
    // via `println!("READY {} sha256:{} state={}", resolved_addr,
    // fingerprint_hex, state_token)` (PR-45a + PR-46α). We assemble
    // it ourselves rather than spawning `aberp` because cargo test
    // runs without the binary on PATH; the format string is the
    // contract, not the spawn.
    let port = 51847u16;
    let fingerprint_hex: String = (0..32).map(|i| format!("{:02x}", (i * 7) as u8)).collect();
    let serve_println = format!("READY 127.0.0.1:{port} sha256:{fingerprint_hex} state=ready");

    let parsed = handshake::parse(&serve_println)
        .expect("round-trip from serve.rs format string MUST parse");

    assert_eq!(parsed.port, port);
    assert_eq!(parsed.url, format!("https://127.0.0.1:{port}"));
    assert_eq!(parsed.fingerprint_hex, fingerprint_hex);
    assert_eq!(parsed.state, ServeBootState::Ready);
}

/// PR-46α / session-62 — the NeedsSetup arm of the round-trip. When
/// the keychain is empty for the tenant, `aberp serve` emits
/// `state=needs-setup`; the Tauri shell parses it and the SPA's
/// first paint renders the first-run setup wizard. Brief-named as
/// the load-bearing pin for the first-run path.
#[test]
fn serve_println_round_trip_needs_setup() {
    let port = 51847u16;
    let fingerprint_hex: String = (0..32).map(|i| format!("{:02x}", (i * 7) as u8)).collect();
    let serve_println =
        format!("READY 127.0.0.1:{port} sha256:{fingerprint_hex} state=needs-setup");

    let parsed = handshake::parse(&serve_println).expect("needs-setup line MUST parse");

    assert_eq!(parsed.state, ServeBootState::NeedsSetup);
    assert_eq!(parsed.port, port);
}

/// PR-46α / session-62 — backwards-compat pin: a legacy three-token
/// line (no `state=` suffix) parses as Ready. Locks the additive
/// nature of the suffix so a future drift that requires the token
/// surfaces here loud.
#[test]
fn serve_println_round_trip_legacy_defaults_ready() {
    let port = 51847u16;
    let fingerprint_hex: String = (0..32).map(|i| format!("{:02x}", (i * 7) as u8)).collect();
    let serve_println = format!("READY 127.0.0.1:{port} sha256:{fingerprint_hex}");

    let parsed = handshake::parse(&serve_println).expect("legacy line MUST parse");
    assert_eq!(parsed.state, ServeBootState::Ready);
}

/// PR-45a / session-61 — brief-named pin: the addr token in the
/// handshake line MUST `parse::<SocketAddr>()` cleanly. A future PR
/// that introduced (say) IPv6 square-brackets or a trailing slash
/// would break the shell's handshake; this test names that contract.
#[test]
fn addr_token_parses_as_socket_addr() {
    let port = 33333u16;
    let fingerprint_hex: String = (0..32).map(|i| format!("{:02x}", (i * 5) as u8)).collect();
    let line = format!("READY 127.0.0.1:{port} sha256:{fingerprint_hex}");

    // The addr token is the second whitespace-separated word.
    let addr_token = line
        .split_ascii_whitespace()
        .nth(1)
        .expect("READY line has an addr token at index 1");

    let socket: SocketAddr = addr_token
        .parse()
        .expect("addr token must `parse::<SocketAddr>()` cleanly");
    assert_eq!(socket.port(), port);
    assert!(socket.ip().is_loopback());
}

#[test]
fn parser_constants_pin_println_contract() {
    // If someone edits one constant they have to edit the test too —
    // and the test only passes when each reflects the verbatim text in
    // serve.rs's println. The other half of the contract lives in the
    // unit test `handshake_constants_match_serve_println_shape`; this
    // integration test additionally checks the round-trip behaviour.
    // PR-46α / session-62 added STATE_MARKER as the third constant.
    assert_eq!(handshake::HANDSHAKE_PREFIX, "READY");
    assert_eq!(handshake::FINGERPRINT_MARKER, "sha256:");
    assert_eq!(handshake::STATE_MARKER, "state=");
}
