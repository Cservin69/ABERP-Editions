//! Byte-equality with a real nginx, per request class (ADR-0115 §3.3).
//!
//! Every case here was captured from nginx 1.31.4 with
//! `server_tokens off` — the transcript is in
//! `tests/fixtures/nginx-goldens.txt` and the capture procedure is in
//! the ADR. These tests drive the **real** front over a **real** TCP
//! socket with raw request bytes, so what is compared is what a scanner
//! would actually receive: status line, header order, every header
//! value, the body, and the connection disposition.
//!
//! `Date` is the only field allowed to differ, and it is normalised
//! rather than ignored — its *format* is pinned separately in
//! `nginx::tests`, because a differently-spelled date is a fingerprint
//! too.
//!
//! # Why raw sockets and not a client library
//!
//! Half these cases are requests no HTTP client will send. A library
//! will not emit a bad request line, a duplicated `Host`, `HTTP/9.9`,
//! or an HTTP/0.9 request at all — and those are precisely the probes
//! that used to bypass the mimic entirely and get answered by hyper.
//! Testing through a client would test everything except the thing that
//! was broken.

use std::sync::Arc;
use std::time::Duration;

use aberp_portal_relay::{http1, Broker, Canary, Front};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

/// Stand up the real front on a loopback port with **no agent lease**,
/// which is the state every un-authenticated caller sees.
async fn front() -> u16 {
    let (canary, _rx) = Canary::new();
    let front = Arc::new(Front {
        broker: Arc::new(Broker::new()),
        canary,
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        loop {
            let Ok((tcp, peer)) = listener.accept().await else {
                return;
            };
            let front = Arc::clone(&front);
            tokio::spawn(async move { http1::serve(tcp, Some(peer), front).await });
        }
    });
    port
}

/// Send raw bytes, read until the peer closes or goes quiet.
async fn exchange(port: u16, raw: &[u8]) -> Vec<u8> {
    let mut s = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    s.write_all(raw).await.expect("write");
    let mut out = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        // A short read timeout is how a kept-alive connection — which
        // by design does NOT close — terminates the read.
        match tokio::time::timeout(Duration::from_millis(400), s.read(&mut chunk)).await {
            Ok(Ok(0)) | Err(_) => break,
            Ok(Ok(n)) => out.extend_from_slice(&chunk[..n]),
            Ok(Err(_)) => break,
        }
    }
    out
}

/// Replace the `Date` value so the comparison is byte-exact everywhere
/// else. Asserts the header is present and non-empty — an absent `Date`
/// would otherwise normalise to a passing test.
fn normalise(raw: &[u8]) -> String {
    let s = String::from_utf8_lossy(raw).to_string();
    let mut out = String::with_capacity(s.len());
    let mut saw_date = false;
    for (i, line) in s.split("\r\n").enumerate() {
        if i > 0 {
            out.push_str("\r\n");
        }
        if let Some(v) = line.strip_prefix("Date: ") {
            assert!(!v.is_empty(), "an empty Date header");
            saw_date = true;
            out.push_str("Date: {DATE}");
        } else {
            out.push_str(line);
        }
    }
    assert!(
        saw_date || !s.starts_with("HTTP/"),
        "a framed response with no Date header"
    );
    out
}

/// The nginx error page for a status line, byte for byte.
fn page(status_line: &str) -> String {
    format!(
        "<html>\r\n<head><title>{status_line}</title></head>\r\n<body>\r\n\
         <center><h1>{status_line}</h1></center>\r\n\
         <hr><center>nginx</center>\r\n</body>\r\n</html>\r\n"
    )
}

/// One captured golden response.
fn golden(status_line: &str, connection: &str) -> String {
    let body = page(status_line);
    format!(
        "HTTP/1.1 {status_line}\r\n\
         Server: nginx\r\n\
         Date: {{DATE}}\r\n\
         Content-Type: text/html\r\n\
         Content-Length: {}\r\n\
         Connection: {connection}\r\n\
         \r\n{body}",
        body.len()
    )
}

#[tokio::test]
async fn the_404_class_is_byte_identical() {
    let port = front().await;
    let got = exchange(
        port,
        b"GET /nope HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(got.len(), 289, "the capture was 289 bytes on the wire");
    assert_eq!(normalise(&got), golden("404 Not Found", "close"));
}

#[tokio::test]
async fn the_404_is_the_same_bytes_for_every_path_shape() {
    // The anti-oracle property: within a class, the answer must not
    // depend on the target at all. A path that exists behind the knock,
    // one that never could, the decoy, and a knock-shaped guess must be
    // indistinguishable.
    let port = front().await;
    let mut seen: Option<String> = None;
    for path in [
        "/nope",
        "/api/invoices",
        "/admin/config.backup",
        "/.env",
        "/wp-login.php",
        "/AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "/%2e%2e%2f%2e%2e%2fetc%2fpasswd",
    ] {
        let raw = format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
        let got = normalise(&exchange(port, raw.as_bytes()).await);
        match &seen {
            None => seen = Some(got),
            Some(first) => assert_eq!(&got, first, "`{path}` answered differently"),
        }
    }
    assert_eq!(
        seen.as_deref(),
        Some(golden("404 Not Found", "close")).as_deref()
    );
}

#[tokio::test]
async fn the_400_class_is_byte_identical_for_every_captured_input() {
    let port = front().await;
    let want = golden("400 Bad Request", "close");
    for (name, raw) in [
        ("bad request line", &b"NOT A VALID REQUEST LINE\r\n\r\n"[..]),
        ("missing Host on 1.1", b"GET /nope HTTP/1.1\r\n\r\n"),
        (
            "duplicate Host",
            b"GET /nope HTTP/1.1\r\nHost: x\r\nHost: y\r\n\r\n",
        ),
        (
            "space in the target",
            b"GET /no pe HTTP/1.1\r\nHost: x\r\n\r\n",
        ),
        (
            "malformed header name",
            b"GET /nope HTTP/1.1\r\nHost: x\r\nBad Header Name: v\r\n\r\n",
        ),
        ("asterisk form", b"OPTIONS * HTTP/1.1\r\nHost: x\r\n\r\n"),
        (
            "TLS ClientHello at a cleartext port",
            b"\x16\x03\x01\x00\x50\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00",
        ),
    ] {
        let got = exchange(port, raw).await;
        assert_eq!(got.len(), 295, "{name}: the capture was 295 bytes");
        assert_eq!(normalise(&got), want, "{name}");
    }
}

#[tokio::test]
async fn the_505_class_is_byte_identical() {
    // Including HTTP/2.0 in the clear, which is what a scanner probing
    // for h2c sends.
    let port = front().await;
    let want = golden("505 HTTP Version Not Supported", "close");
    for v in ["HTTP/9.9", "HTTP/2.0", "HTTP/3.0"] {
        let raw = format!("GET /nope {v}\r\nHost: x\r\n\r\n");
        let got = exchange(port, raw.as_bytes()).await;
        assert_eq!(got.len(), 340, "{v}: the capture was 340 bytes");
        assert_eq!(normalise(&got), want, "{v}");
    }
}

#[tokio::test]
async fn the_405_class_is_byte_identical_and_stays_open() {
    // Captured with `Connection: keep-alive` — nginx does NOT close on
    // a 405, and a server that did would be identified by one request.
    let port = front().await;
    let got = exchange(port, b"FROBNICATE /nope HTTP/1.1\r\nHost: x\r\n\r\n").await;
    assert_eq!(got.len(), 300, "the capture was 300 bytes");
    assert_eq!(normalise(&got), golden("405 Not Allowed", "keep-alive"));
}

#[tokio::test]
async fn an_unknown_verb_with_a_body_does_not_desynchronise_the_socket() {
    // The 405 is answered without reading the body — but the body must
    // still be DRAINED, or the next request on a kept-alive connection
    // starts mid-payload and gets answered as garbage. A server that
    // desynchronises where nginx does not has identified itself.
    let port = front().await;
    // Built by concatenation, not one long literal: `cargo fmt` will
    // happily collapse a continued byte string and keep the source
    // indentation inside it, which silently corrupts the boundary
    // between the two requests — the exact thing under test.
    let mut raw = Vec::new();
    raw.extend_from_slice(b"FROBNICATE /a HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\r\n");
    raw.extend_from_slice(b"HELLO");
    raw.extend_from_slice(b"GET /b HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    let got = exchange(port, &raw).await;
    let want = format!(
        "{}{}",
        golden("405 Not Allowed", "keep-alive"),
        golden("404 Not Found", "close")
    );
    assert_eq!(
        normalise(&got),
        want,
        "the second request was not read cleanly"
    );
}

#[tokio::test]
async fn the_414_class_is_byte_identical() {
    let port = front().await;
    let raw = format!("GET /{} HTTP/1.1\r\nHost: x\r\n\r\n", "a".repeat(9000));
    let got = exchange(port, raw.as_bytes()).await;
    assert_eq!(got.len(), 325, "the capture was 325 bytes");
    assert_eq!(
        normalise(&got),
        golden("414 Request-URI Too Large", "close")
    );
}

#[tokio::test]
async fn keep_alive_is_echoed_rather_than_always_closed() {
    // The tell this test exists for: two requests down one socket. A
    // server that closes after the first is not nginx.
    let port = front().await;
    let got = exchange(
        port,
        b"GET /a HTTP/1.1\r\nHost: x\r\n\r\nGET /b HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(got.len(), 583, "the capture was 583 bytes for the pair");
    let want = format!(
        "{}{}",
        golden("404 Not Found", "keep-alive"),
        golden("404 Not Found", "close")
    );
    assert_eq!(normalise(&got), want);
}

#[tokio::test]
async fn http_1_0_defaults_to_close_and_opts_in_to_keep_alive() {
    let port = front().await;
    let got = exchange(port, b"GET /nope HTTP/1.0\r\nHost: x\r\n\r\n").await;
    assert_eq!(got.len(), 289);
    assert_eq!(normalise(&got), golden("404 Not Found", "close"));

    let got = exchange(
        port,
        b"GET /nope HTTP/1.0\r\nHost: x\r\nConnection: keep-alive\r\n\r\n",
    )
    .await;
    assert_eq!(got.len(), 294);
    assert_eq!(normalise(&got), golden("404 Not Found", "keep-alive"));
}

#[tokio::test]
async fn http_1_0_without_a_host_is_404_not_400() {
    // `Host` is mandatory only on HTTP/1.1. Answering 400 here — the
    // obvious reading of the RFC — is a fingerprint.
    let port = front().await;
    let got = exchange(port, b"GET /nope HTTP/1.0\r\n\r\n").await;
    assert_eq!(got.len(), 289);
    assert_eq!(normalise(&got), golden("404 Not Found", "close"));
}

#[tokio::test]
async fn a_head_carries_the_length_and_no_body() {
    let port = front().await;
    let got = exchange(
        port,
        b"HEAD /nope HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(got.len(), 143, "the capture was 143 bytes");
    let s = normalise(&got);
    assert!(s.contains("Content-Length: 146"));
    assert!(s.ends_with("\r\n\r\n"));
}

#[tokio::test]
async fn http_0_9_gets_the_bare_body() {
    // No status line, no headers — the forgotten class scanners send
    // precisely because it is forgotten.
    let port = front().await;
    let got = exchange(port, b"GET /nope\r\n\r\n").await;
    assert_eq!(got.len(), 146);
    assert_eq!(String::from_utf8_lossy(&got), page("404 Not Found"));
}

#[tokio::test]
async fn a_chunked_body_is_drained_and_the_connection_survives() {
    let port = front().await;
    let got = exchange(
        port,
        b"POST /nope HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\n",
    )
    .await;
    assert_eq!(got.len(), 294);
    assert_eq!(normalise(&got), golden("404 Not Found", "keep-alive"));
}

#[tokio::test]
async fn leading_crlfs_and_bare_lf_endings_are_tolerated() {
    let port = front().await;
    for raw in [
        &b"\r\n\r\nGET /nope HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"[..],
        b"GET /nope HTTP/1.1\nHost: x\nConnection: close\n\n",
    ] {
        let got = exchange(port, raw).await;
        assert_eq!(normalise(&got), golden("404 Not Found", "close"));
    }
}

#[tokio::test]
async fn an_absolute_form_target_is_404_not_400() {
    let port = front().await;
    let got = exchange(
        port,
        b"GET http://x/nope HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(normalise(&got), golden("404 Not Found", "close"));
}

#[tokio::test]
async fn a_connection_that_says_nothing_is_answered_with_nothing() {
    // nginx says nothing to a socket that opens and closes. A server
    // that volunteers a 400 there is distinguishable by a bare port
    // scan — the cheapest probe there is.
    let port = front().await;
    let s = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    drop(s);
    let got = exchange(port, b"").await;
    assert!(got.is_empty(), "the host volunteered {got:?}");
}

#[tokio::test]
async fn no_parked_answer_ever_carries_a_security_header() {
    // The posture inversion: a parked nginx sends none of these, so a
    // response that did would be unique on the whole host. They belong
    // on the authenticated shell and nowhere else.
    let port = front().await;
    for raw in [
        &b"GET /nope HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"[..],
        b"NOT A VALID REQUEST LINE\r\n\r\n",
        b"GET /nope HTTP/9.9\r\nHost: x\r\n\r\n",
        b"FROBNICATE /nope HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    ] {
        let got = String::from_utf8_lossy(&exchange(port, raw).await).to_ascii_lowercase();
        for forbidden in [
            "strict-transport-security",
            "content-security-policy",
            "referrer-policy",
            "x-content-type-options",
            "x-frame-options",
            "cache-control",
            "set-cookie",
        ] {
            assert!(
                !got.contains(forbidden),
                "a parked answer leaked `{forbidden}`"
            );
        }
    }
}

#[tokio::test]
async fn the_goldens_fixture_still_describes_what_the_code_does() {
    // A cheap guard against the fixture and the implementation drifting
    // apart: the file is the evidence for every assertion above, and a
    // fixture nobody reads is a fixture that quietly stops being true.
    let fixture = include_str!("fixtures/nginx-goldens.txt");
    for want in [
        "Content-Length: 146",
        "Content-Length: 150",
        "Content-Length: 170",
        "Content-Length: 180",
        "<hr><center>nginx</center>",
        "Server: nginx",
        "server_tokens off",
    ] {
        assert!(
            fixture.contains(want),
            "the goldens no longer mention `{want}`"
        );
    }
}
