//! Parser for the handshake line `aberp serve` prints on stdout.
//!
//! Shape (locked by `apps/aberp/src/serve.rs`'s `println!`,
//! PR-45a + PR-46α):
//!
//! ```text
//! READY 127.0.0.1:<port> sha256:<hex>                       # legacy (pre-46α)
//! READY 127.0.0.1:<port> sha256:<hex> state=ready           # 46α, Ready
//! READY 127.0.0.1:<port> sha256:<hex> state=needs-setup     # 46α, NeedsSetup
//! ```
//!
//! # Why a machine-parseable line
//!
//! Pre-PR-45a the handshake reused the operator-facing log line
//! (`aberp serve: https://127.0.0.1:<port>/ (fingerprint sha256:<hex>)`).
//! That line carried the `args.port` value verbatim — when the
//! operator launched with `--port 0` (kernel-picks), the line read
//! `https://127.0.0.1:0/` and the shell's parser had no way to learn
//! the resolved port. The desktop Tauri shell timed out on the
//! handshake and exited.
//!
//! The PR-45a shape is three whitespace-separated tokens — literal
//! `READY`, a `SocketAddr` that round-trips through
//! `parse::<SocketAddr>()`, and `sha256:<64-hex>`. The line is
//! printed exactly once, after the listener has fully bound to its
//! kernel-assigned port, so the addr token is always the live port.
//! The operator-readable `aberp serve: …` line moved to the backend
//! tracing log (the shell forwards stderr verbatim, so it still
//! shows up in the same window — but it's no longer load-bearing on
//! the handshake parser).
//!
//! # PR-46α — optional state=<token> suffix
//!
//! PR-46α extends the line with an OPTIONAL fourth token
//! `state=ready` or `state=needs-setup`. A missing token is treated
//! as `state=ready` for backwards compatibility (the PR-45a integration
//! tests pre-date the suffix; they continue to pass without
//! modification). The Tauri shell's boot-status dispatch reads the
//! parsed [`Handshake::state`] field to route the SPA's first paint
//! between the normal app and the first-run setup wizard.
//!
//! Per CLAUDE.md rule 12, the parser is intentionally pedantic:
//! anything other than the expected shape — non-loopback host,
//! malformed addr, wrong sha256 prefix, malformed hex, malformed
//! `state=<unknown>` value, trailing junk — is a hard error.
//! "We silently fell back to a default" is the failure mode rule 12
//! names.

use std::net::{IpAddr, SocketAddr};

use anyhow::{anyhow, Result};

/// PR-46α / session-62 — boot lifecycle discriminator parsed off the
/// handshake line's optional `state=<token>` suffix. The Tauri shell
/// reads this to route the SPA's first paint between the normal app
/// (`Ready`), the first-run NAV-credentials wizard (`NeedsSetup`),
/// and the seller-identity wizard (`NeedsSellerConfig`, added in
/// PR-51 / session-71).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServeBootState {
    Ready,
    NeedsSetup,
    NeedsSellerConfig,
}

/// The structured outcome of parsing one handshake line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handshake {
    /// `https://127.0.0.1:<port>` — the base URL the shell uses for
    /// every subsequent request. NO trailing slash so callers can
    /// concatenate `/invoices` etc. without double-slashes.
    pub url: String,
    /// Loopback port the listener bound to.
    pub port: u16,
    /// Hex SHA-256 fingerprint of the loopback cert DER (lower-case,
    /// no colons). Matches `apps/aberp/src/serve.rs::compute_cert_fingerprint`
    /// output exactly.
    pub fingerprint_hex: String,
    /// PR-46α / session-62 — boot lifecycle discriminator. Parsed
    /// from the optional `state=<token>` suffix on the handshake
    /// line; defaults to [`ServeBootState::Ready`] when the token
    /// is absent (backwards-compat with the PR-45a line shape).
    pub state: ServeBootState,
}

/// Locked to the value in `apps/aberp/src/serve.rs`. If `serve.rs`
/// ever changes its `println!`, this constant + the test below
/// catches it.
pub const HANDSHAKE_PREFIX: &str = "READY";

/// Recognised fingerprint marker. Locked to the same `println!`.
pub const FINGERPRINT_MARKER: &str = "sha256:";

/// PR-46α / session-62 — recognised marker for the optional state
/// suffix. The token form is `state=<value>` with no quoting;
/// recognised values are `ready` and `needs-setup`.
pub const STATE_MARKER: &str = "state=";

/// Parse exactly one handshake line. Whitespace around the line is
/// tolerated; everything inside the line is pedantic.
pub fn parse(line: &str) -> Result<Handshake> {
    let line = line.trim();
    let mut tokens = line.split_ascii_whitespace();

    let prefix = tokens
        .next()
        .ok_or_else(|| anyhow!("handshake line was empty"))?;
    if prefix != HANDSHAKE_PREFIX {
        return Err(anyhow!(
            "handshake line did not start with `{HANDSHAKE_PREFIX}` — got `{line}`"
        ));
    }

    let addr_token = tokens.next().ok_or_else(|| {
        anyhow!(
            "handshake line missing the addr token (expected `127.0.0.1:<port>`) — got `{line}`"
        )
    })?;
    let socket: SocketAddr = addr_token
        .parse()
        .map_err(|e| anyhow!("handshake addr `{addr_token}` failed parse::<SocketAddr>: {e}"))?;
    validate_addr_is_loopback(&socket)?;
    let port = socket.port();
    if port == 0 {
        // Port 0 on the wire means `serve.rs` never resolved its
        // kernel-assigned port. Loud per rule 12.
        return Err(anyhow!(
            "handshake printed port=0 — serve did not resolve the kernel-assigned port"
        ));
    }

    let fp_token = tokens
        .next()
        .ok_or_else(|| anyhow!("handshake line missing the `sha256:<hex>` token — got `{line}`"))?;
    let fingerprint_hex = fp_token
        .strip_prefix(FINGERPRINT_MARKER)
        .ok_or_else(|| {
            anyhow!("handshake fingerprint token missing `{FINGERPRINT_MARKER}` marker — got `{fp_token}`")
        })?
        .to_string();
    validate_fingerprint_hex(&fingerprint_hex)?;

    // PR-46α / session-62 — optional `state=<token>` suffix. A
    // missing token defaults to Ready (backwards-compat with the
    // pre-PR-46α two-state line shape; the PR-45a integration tests
    // continue to pass without modification). A `state=<unknown>` token
    // is a loud-fail per rule 12 — silent acceptance of an unrecognised
    // boot-state would risk dispatching the SPA's first paint into the
    // wrong view-mode.
    let state = match tokens.next() {
        None => ServeBootState::Ready,
        Some(state_token) => {
            let value = state_token.strip_prefix(STATE_MARKER).ok_or_else(|| {
                anyhow!(
                    "handshake state token missing `{STATE_MARKER}` marker — got `{state_token}`"
                )
            })?;
            parse_state_value(value)?
        }
    };

    if tokens.next().is_some() {
        return Err(anyhow!(
            "handshake line has trailing tokens after the state token — got `{line}`"
        ));
    }

    Ok(Handshake {
        url: format!("https://{socket}"),
        port,
        fingerprint_hex,
        state,
    })
}

fn parse_state_value(value: &str) -> Result<ServeBootState> {
    match value {
        "ready" => Ok(ServeBootState::Ready),
        "needs-setup" => Ok(ServeBootState::NeedsSetup),
        "needs-seller-config" => Ok(ServeBootState::NeedsSellerConfig),
        other => Err(anyhow!(
            "handshake state value `{other}` is not one of [ready, needs-setup, needs-seller-config]"
        )),
    }
}

/// The loopback HTTPS listener is bound to `127.0.0.1` per ADR-0021
/// §Part B; nothing else is accepted. A `localhost` literal would be
/// indistinguishable from a hosts-file override and is refused at the
/// `SocketAddr::parse` layer (it only accepts IP literals).
fn validate_addr_is_loopback(addr: &SocketAddr) -> Result<()> {
    let ip = addr.ip();
    if !ip.is_loopback() {
        return Err(anyhow!(
            "handshake addr `{addr}` host is not loopback — refusing to connect"
        ));
    }
    if !matches!(ip, IpAddr::V4(_)) {
        return Err(anyhow!(
            "handshake addr `{addr}` is not IPv4 — serve binds 127.0.0.1 only"
        ));
    }
    Ok(())
}

/// A SHA-256 fingerprint is exactly 64 lower-case hex characters.
/// `apps/aberp/src/serve.rs` produces that shape via `hex::encode`;
/// any deviation is a wire-format break.
fn validate_fingerprint_hex(s: &str) -> Result<()> {
    if s.len() != 64 {
        return Err(anyhow!(
            "fingerprint `{s}` length is {}, expected 64",
            s.len()
        ));
    }
    if !s
        .bytes()
        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(anyhow!(
            "fingerprint `{s}` contains non-lower-hex characters"
        ));
    }
    // Verify decode succeeds — defence in depth against the bit-twiddle
    // above missing some edge case.
    hex::decode(s).map_err(|e| anyhow!("fingerprint `{s}` failed hex decode: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_fingerprint(byte: u8) -> String {
        hex::encode(vec![byte; 32])
    }

    #[test]
    fn parses_well_formed_line() {
        let fp = make_fingerprint(0xab);
        let line = format!("READY 127.0.0.1:54321 sha256:{fp}");
        let parsed = parse(&line).expect("well-formed line must parse");
        assert_eq!(parsed.url, "https://127.0.0.1:54321");
        assert_eq!(parsed.port, 54321);
        assert_eq!(parsed.fingerprint_hex, fp);
        // PR-46α / session-62 — backwards-compat default: a line
        // missing the `state=` suffix is treated as Ready.
        assert_eq!(parsed.state, ServeBootState::Ready);
    }

    /// PR-46α / session-62 — explicit `state=ready` parses to Ready.
    #[test]
    fn parses_state_ready_suffix() {
        let fp = make_fingerprint(0xab);
        let line = format!("READY 127.0.0.1:54321 sha256:{fp} state=ready");
        let parsed = parse(&line).expect("state=ready line must parse");
        assert_eq!(parsed.state, ServeBootState::Ready);
    }

    /// PR-46α / session-62 — `state=needs-setup` parses to
    /// NeedsSetup. This is the load-bearing token: when the SPA's
    /// first-paint dispatch sees this, it renders the setup wizard
    /// instead of the normal app.
    #[test]
    fn parses_state_needs_setup_suffix() {
        let fp = make_fingerprint(0xab);
        let line = format!("READY 127.0.0.1:54321 sha256:{fp} state=needs-setup");
        let parsed = parse(&line).expect("state=needs-setup line must parse");
        assert_eq!(parsed.state, ServeBootState::NeedsSetup);
        assert_eq!(parsed.port, 54321);
    }

    /// PR-51 / session-71 — `state=needs-seller-config` parses to
    /// `NeedsSellerConfig`. The Tauri shell's first-paint dispatch
    /// reads this to render the seller-info wizard.
    #[test]
    fn parses_state_needs_seller_config_suffix() {
        let fp = make_fingerprint(0xab);
        let line = format!("READY 127.0.0.1:54321 sha256:{fp} state=needs-seller-config");
        let parsed = parse(&line).expect("state=needs-seller-config line must parse");
        assert_eq!(parsed.state, ServeBootState::NeedsSellerConfig);
        assert_eq!(parsed.port, 54321);
    }

    /// PR-46α / session-62 — an unrecognised state value loud-fails
    /// per CLAUDE.md rule 12.
    #[test]
    fn rejects_unknown_state_value() {
        let fp = make_fingerprint(0xab);
        let line = format!("READY 127.0.0.1:54321 sha256:{fp} state=loading");
        assert!(parse(&line).is_err());
    }

    /// PR-46α / session-62 — a `state=` token missing its prefix
    /// also loud-fails (defence against a future drift that emits
    /// the raw token without the marker).
    #[test]
    fn rejects_state_token_missing_marker() {
        let fp = make_fingerprint(0xab);
        let line = format!("READY 127.0.0.1:54321 sha256:{fp} ready");
        assert!(parse(&line).is_err());
    }

    #[test]
    fn tolerates_surrounding_whitespace() {
        let fp = make_fingerprint(0x01);
        let line = format!("   READY 127.0.0.1:1234 sha256:{fp}   ");
        let parsed = parse(&line).expect("surrounding whitespace is fine");
        assert_eq!(parsed.port, 1234);
    }

    #[test]
    fn addr_token_round_trips_socket_addr_parse() {
        // The brief named pin: the addr token must `parse::<SocketAddr>()`
        // cleanly. If a future PR ever introduces zero-padding or
        // square-bracket-IPv6 here, this test names the contract.
        let fp = make_fingerprint(0x02);
        let line = format!("READY 127.0.0.1:8443 sha256:{fp}");
        // First parse via the public surface;
        let parsed = parse(&line).expect("must parse");
        // Then re-extract the addr token and confirm SocketAddr is happy.
        let addr_token = line.split_ascii_whitespace().nth(1).unwrap();
        let socket: std::net::SocketAddr = addr_token
            .parse()
            .expect("addr token must parse::<SocketAddr>() cleanly");
        assert_eq!(socket.port(), parsed.port);
    }

    #[test]
    fn rejects_wrong_prefix() {
        let fp = make_fingerprint(0xcc);
        let line = format!("HELLO 127.0.0.1:1 sha256:{fp}");
        assert!(parse(&line).is_err());
    }

    #[test]
    fn rejects_legacy_serve_human_readable_line() {
        // The pre-PR-45a operator-facing line is no longer the
        // handshake source. The shell's stderr forward picks it up as
        // a regular log line; the parser rejects it loud.
        let fp = make_fingerprint(0xa0);
        let line = format!("aberp serve: https://127.0.0.1:12345/ (fingerprint sha256:{fp})");
        assert!(parse(&line).is_err());
    }

    #[test]
    fn rejects_non_loopback_host() {
        let fp = make_fingerprint(0xdd);
        let line = format!("READY 10.0.0.5:8443 sha256:{fp}");
        assert!(parse(&line).is_err());
    }

    #[test]
    fn rejects_localhost_literal() {
        // `SocketAddr::parse` itself refuses `localhost` (only IP
        // literals); the validation layer additionally rejects
        // anything that resolves non-loopback, so two layers of
        // defence per rule 12.
        let fp = make_fingerprint(0xee);
        let line = format!("READY localhost:8443 sha256:{fp}");
        assert!(parse(&line).is_err());
    }

    #[test]
    fn rejects_port_zero() {
        // serve.rs is supposed to resolve a kernel-assigned port
        // before printing; 0 on the wire is a contract break.
        let fp = make_fingerprint(0xff);
        let line = format!("READY 127.0.0.1:0 sha256:{fp}");
        assert!(parse(&line).is_err());
    }

    #[test]
    fn rejects_truncated_fingerprint() {
        let line = "READY 127.0.0.1:8443 sha256:abc";
        assert!(parse(line).is_err());
    }

    #[test]
    fn rejects_uppercase_fingerprint() {
        // `hex::encode` emits lower-case; an upper-case fingerprint
        // would be `format!("{:X}", ...)` and is a contract break.
        let fp_upper = "ABABABABABABABABABABABABABABABABABABABABABABABABABABABABABABABAB";
        let line = format!("READY 127.0.0.1:8443 sha256:{fp_upper}");
        assert!(parse(&line).is_err());
    }

    #[test]
    fn rejects_missing_fingerprint_token() {
        let line = "READY 127.0.0.1:1";
        assert!(parse(line).is_err());
    }

    #[test]
    fn rejects_missing_fingerprint_marker() {
        let fp = make_fingerprint(0x20);
        let line = format!("READY 127.0.0.1:1 {fp}");
        assert!(parse(&line).is_err());
    }

    #[test]
    fn rejects_port_over_u16() {
        let fp = make_fingerprint(0x30);
        let line = format!("READY 127.0.0.1:65536 sha256:{fp}");
        assert!(parse(&line).is_err());
    }

    #[test]
    fn rejects_trailing_tokens_after_state() {
        // PR-46α / session-62 — the fourth token is now the optional
        // `state=<value>`. Any FIFTH token is still rejected loud per
        // rule 12 — silent acceptance of trailing metadata would leave
        // the parser ambiguous about which token to trust.
        let fp = make_fingerprint(0x40);
        let line = format!("READY 127.0.0.1:1 sha256:{fp} state=ready extra-token");
        assert!(parse(&line).is_err());
    }

    /// PR-46α / session-62 — a non-`state=` fourth token (e.g.
    /// `extra-token`) loud-fails because the parser requires the
    /// `state=` marker on the slot. Defence in depth against a
    /// future drift that appended a different optional suffix to the
    /// same slot — the SPA would otherwise mis-interpret it as a
    /// boot-state value.
    #[test]
    fn rejects_unknown_fourth_token() {
        let fp = make_fingerprint(0x40);
        let line = format!("READY 127.0.0.1:1 sha256:{fp} extra-token");
        assert!(parse(&line).is_err());
    }

    #[test]
    fn handshake_constants_match_serve_println_shape() {
        // Conformance check: the constants here are the load-bearing
        // contract with `apps/aberp/src/serve.rs`'s `println!`. If
        // any drifts, this test name names the contract that broke.
        // PR-46α added STATE_MARKER as the third constant.
        assert_eq!(HANDSHAKE_PREFIX, "READY");
        assert_eq!(FINGERPRINT_MARKER, "sha256:");
        assert_eq!(STATE_MARKER, "state=");
    }
}
