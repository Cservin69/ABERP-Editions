//! Differential test: our front versus a **live nginx**, byte for byte.
//!
//! `nginx_indistinguishable.rs` pins the front against goldens that
//! were transcribed by hand into `tests/fixtures/nginx-goldens.txt`.
//! Transcription is exactly where a disguise rots — the earlier
//! hand-written 404 body in this crate was 4 bytes short of the real
//! one for precisely that reason. So this test removes the human step:
//! it starts a real nginx with `server_tokens off`, sends the same raw
//! bytes to both servers, and diffs the responses.
//!
//! # It is `#[ignore]`d, and it FAILS when nginx is absent
//!
//! It used to `return` with an `eprintln!` when nginx was missing —
//! which `cargo test` reports as **`ok`**. A green tick for a check
//! that did not run is worse than no check: it is a check that reads as
//! passing on every machine in the world that lacks nginx, including
//! CI, and it stayed that way for the whole life of the branch. Nothing
//! ever told anyone.
//!
//! So the skip is gone. The test is `#[ignore]`d, which `cargo test`
//! reports as `ignored` with the reason printed — visible, and not a
//! lie — and when it *is* run, a missing nginx is a hard failure rather
//! than a shrug.
//!
//! Run it after any change to `nginx.rs` or `http1.rs`:
//!
//! ```text
//! brew install nginx     # or apt-get install nginx-light
//! cargo test -p aberp-portal-relay --test nginx_differential -- --ignored --nocapture
//! ```
//!
//! **It still does not run in CI.** That is now a stated gap rather
//! than a hidden one — see D-20, which carries it as a named follow-on:
//! CI needs an `nginx-light` install step before this can gate a merge.
//!
//! # Two lists, because the guarantee has two halves
//!
//! [`CASES`] is the byte-parity claim: for these, our bytes and nginx's
//! must be identical. [`RESIDUAL_CASES`] is the honest remainder — the
//! pathological inputs where nginx reaches for a status class this
//! relay does not implement (`501`, `413`, its distinct oversized-header
//! `400`). There the assertion is the one the disguise actually rests
//! on: **a prompt answer, never a hang**. See ADR-0115 §2, "the named
//! residual".

use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use aberp_portal_relay::{http1, Broker, Canary, Front};

/// Every request class, as raw bytes. Deliberately includes several no
/// HTTP client would ever send — those are the ones that used to be
/// answered by hyper rather than by us.
const CASES: &[(&str, &[u8])] = &[
    ("404 close", b"GET /nope HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"),
    ("404 keep-alive", b"GET /nope HTTP/1.1\r\nHost: x\r\n\r\n"),
    ("404 deep path", b"GET /a/b/c/d.php?q=1 HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"),
    ("404 HTTP/1.0", b"GET /nope HTTP/1.0\r\nHost: x\r\n\r\n"),
    ("404 HTTP/1.0 keep-alive", b"GET /nope HTTP/1.0\r\nHost: x\r\nConnection: keep-alive\r\n\r\n"),
    ("404 HTTP/1.0 no Host", b"GET /nope HTTP/1.0\r\n\r\n"),
    ("404 absolute form", b"GET http://x/nope HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"),
    ("404 leading CRLF", b"\r\n\r\nGET /nope HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"),
    ("404 bare LF", b"GET /nope HTTP/1.1\nHost: x\nConnection: close\n\n"),
    ("404 uppercase Connection", b"GET /nope HTTP/1.1\r\nHost: x\r\nConnection: KEEP-ALIVE\r\n\r\n"),
    ("404 POST", b"POST /nope HTTP/1.1\r\nHost: x\r\nContent-Length: 3\r\n\r\nabc"),
    ("404 chunked", b"POST /nope HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\n"),
    ("404 HEAD", b"HEAD /nope HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"),
    ("400 bad request line", b"NOT A VALID REQUEST LINE\r\n\r\n"),
    ("400 no Host on 1.1", b"GET /nope HTTP/1.1\r\n\r\n"),
    ("400 duplicate Host", b"GET /nope HTTP/1.1\r\nHost: x\r\nHost: y\r\n\r\n"),
    ("400 space in target", b"GET /no pe HTTP/1.1\r\nHost: x\r\n\r\n"),
    ("400 bad header name", b"GET /nope HTTP/1.1\r\nHost: x\r\nBad Header Name: v\r\n\r\n"),
    ("400 asterisk form", b"OPTIONS * HTTP/1.1\r\nHost: x\r\n\r\n"),
    ("400 TLS ClientHello", b"\x16\x03\x01\x00\x50\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00"),
    ("405 unknown method", b"FROBNICATE /nope HTTP/1.1\r\nHost: x\r\n\r\n"),
    ("505 HTTP/9.9", b"GET /nope HTTP/9.9\r\nHost: x\r\n\r\n"),
    ("505 HTTP/2.0", b"GET /nope HTTP/2.0\r\nHost: x\r\n\r\n"),
    ("0.9 no version", b"GET /nope\r\n\r\n"),
    // The REAL HTTP/0.9 wire form: no version, no headers, and **no
    // blank line**, because 0.9 has no header block to terminate. The
    // case above sends a terminator no 0.9 client would send, and so
    // never touched the path that mattered — `read_head` waited 60
    // seconds for it and then returned silence, while nginx answers in
    // under a millisecond.
    ("0.9 real wire form", b"GET /nope\r\n"),
    // The same family: a complete request line that will never be
    // followed by a blank line, because it will never be followed by
    // anything. nginx answers each of these instantly.
    ("400 bad request line, unterminated", b"NOT A VALID REQUEST LINE\r\n"),
    ("400 space in target, unterminated", b"GET /no pe HTTP/1.1\r\n"),
    ("505 HTTP/9.9, unterminated", b"GET /nope HTTP/9.9\r\n"),
    // Body framing the head can settle on its own.
    ("400 duplicate Content-Length", b"POST /nope HTTP/1.1\r\nHost: x\r\nContent-Length: 3\r\nContent-Length: 3\r\n\r\nabc"),
    ("400 Content-Length and chunked", b"POST /nope HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\nContent-Length: 3\r\n\r\n0\r\n\r\n"),
    ("400 chunked on HTTP/1.0", b"POST /nope HTTP/1.0\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n"),
    ("400 Content-Length not a number", b"POST /nope HTTP/1.1\r\nHost: x\r\nContent-Length: abc\r\n\r\n"),
    ("400 negative Content-Length", b"POST /nope HTTP/1.1\r\nHost: x\r\nContent-Length: -1\r\n\r\n"),
    // Bodies nginx cannot drain. It answers its ordinary parked page
    // and closes — it does not call them protocol errors, and it does
    // not wait for them.
    ("404-close bogus chunk size", b"POST /nope HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\nZZZZ\r\n"),
    ("404-close chunk not CRLF-terminated", b"POST /nope HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabcZZ0\r\n\r\n"),
    ("404 chunked then pipelined request", b"POST /nope HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\nGET /nope HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"),
];

/// Inputs where our **status class** is knowingly not nginx's.
///
/// nginx reaches for `501 Not Implemented` on an unknown
/// transfer-coding, `413 Request Entity Too Large` on an over-long
/// `Content-Length`, and a *distinct*, longer `400` body on an
/// oversized header block. [`aberp_portal_relay::Class`] has none of
/// those, and chasing byte-parity into nginx's full status table is
/// bottomless: every one added is another body, another length, another
/// capture to keep true.
///
/// So the claim is narrowed rather than faked, and this list is the
/// narrowing written down and executed. What is asserted here is what
/// the disguise actually rests on and what these inputs used to break:
/// **both servers answer, and both answer promptly.** A status-code
/// difference on a request no client sends is a far weaker fingerprint
/// than a 60-second hang — and one real nginx deployments vary on
/// themselves, since `client_max_body_size` and
/// `large_client_header_buffers` are per-site configuration.
const RESIDUAL_CASES: &[(&str, &[u8])] = &[
    (
        "unknown transfer-coding (nginx 501, ours 400)",
        b"POST /nope HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: xchunked\r\n\r\nabc",
    ),
    (
        "over-long Content-Length (nginx 413, ours 400)",
        b"POST /nope HTTP/1.1\r\nHost: x\r\nContent-Length: 99999999\r\n\r\n",
    ),
];

fn nginx_available() -> bool {
    std::process::Command::new("nginx")
        .arg("-v")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// An oversized header block. Built rather than literal because it is
/// 40 KiB, and residual because nginx answers it with a `400` carrying
/// a *different, longer* body than its ordinary one ("Request Header Or
/// Cookie Too Large"), which this relay has no class for.
fn oversized_header_block() -> Vec<u8> {
    let mut raw = b"GET /nope HTTP/1.1\r\nHost: x\r\n".to_vec();
    for i in 0..200 {
        raw.extend_from_slice(format!("X-Pad-{i}: {}\r\n", "a".repeat(200)).as_bytes());
    }
    raw.extend_from_slice(b"\r\n");
    raw
}

/// How long a probe waits for an answer.
///
/// This is the assertion in [`RESIDUAL_CASES`] and half the assertion
/// everywhere else. Every input in this file is answered by real nginx
/// in under a millisecond; the three hang primitives this round removed
/// each took a full **sixty seconds** and then said nothing. A budget
/// of 700 ms cannot be met by accident by a parser that waits.
const PROMPT: Duration = Duration::from_millis(700);

/// Blocking raw exchange — the whole point is to bypass every client
/// abstraction, so this is a socket and two syscalls.
fn exchange(port: u16, raw: &[u8]) -> Vec<u8> {
    let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)) else {
        return Vec::new();
    };
    s.set_read_timeout(Some(PROMPT)).ok();
    if s.write_all(raw).is_err() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut chunk = [0u8; 8192];
    while let Ok(n) = s.read(&mut chunk) {
        if n == 0 {
            break;
        }
        out.extend_from_slice(&chunk[..n]);
    }
    out
}

/// The status line of a response, for the residual report.
fn status_of(raw: &[u8]) -> String {
    if raw.is_empty() {
        return "<SILENCE>".to_string();
    }
    String::from_utf8_lossy(raw)
        .split("\r\n")
        .next()
        .unwrap_or_default()
        .to_string()
}

/// Normalise the one field that legitimately differs.
fn normalise(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw)
        .split("\r\n")
        .map(|l| {
            if l.starts_with("Date: ") {
                "Date: {DATE}".to_string()
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\r\n")
}

struct Nginx {
    port: u16,
    dir: PathBuf,
    child: std::process::Child,
}

impl Drop for Nginx {
    fn drop(&mut self) {
        let _ = std::process::Command::new("nginx")
            .args([
                "-p",
                &self.dir.display().to_string(),
                "-c",
                "nginx.conf",
                "-s",
                "stop",
            ])
            .status();
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn start_nginx() -> Option<Nginx> {
    // A per-process port so two concurrent runs cannot collide.
    let port = 20_000 + (std::process::id() % 20_000) as u16;
    let dir = std::env::temp_dir().join(format!("portal-nginx-diff-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("logs")).ok()?;
    std::fs::create_dir_all(dir.join("html")).ok()?;
    let conf = format!(
        "worker_processes 1;\n\
         daemon off;\n\
         error_log logs/error.log;\n\
         pid logs/nginx.pid;\n\
         events {{ worker_connections 64; }}\n\
         http {{\n\
           access_log off;\n\
           server_tokens off;\n\
           default_type application/octet-stream;\n\
           server {{ listen {port}; server_name _; root html; }}\n\
         }}\n"
    );
    std::fs::write(dir.join("nginx.conf"), conf).ok()?;
    let child = std::process::Command::new("nginx")
        .args(["-p", &dir.display().to_string(), "-c", "nginx.conf"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let mut n = Nginx { port, dir, child };
    // Wait for the listener rather than napping a fixed amount.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while std::time::Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", n.port)).is_ok() {
            return Some(n);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = n.child.kill();
    None
}

#[test]
#[ignore = "needs nginx on PATH: cargo test -p aberp-portal-relay --test nginx_differential -- --ignored --nocapture"]
fn our_front_is_byte_identical_to_a_live_nginx_across_every_request_class() {
    assert!(
        nginx_available(),
        "no `nginx` on PATH. This is the differential guard for the ADR-0115 §3.2 \n\
         disguise and it does not get to pass by not running — that is what it used \n\
         to do, and it is why three sixty-second hang primitives shipped. \n\
         Install it (`brew install nginx` / `apt-get install nginx-light`) and re-run."
    );
    let nginx = start_nginx().expect("nginx is installed but would not start");

    // Our front, on its own runtime thread, with no agent lease — the
    // state every un-authenticated caller sees.
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let ours = rt.block_on(async {
        let (canary, rx) = Canary::new();
        // Held so the queue never closes under the front.
        std::mem::forget(rx);
        let front = Arc::new(Front {
            broker: Arc::new(Broker::new()),
            canary,
        });
        let l = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = l.local_addr().expect("addr").port();
        tokio::spawn(async move {
            loop {
                let Ok((tcp, peer)) = l.accept().await else {
                    return;
                };
                let front = Arc::clone(&front);
                tokio::spawn(async move { http1::serve(tcp, Some(peer), front).await });
            }
        });
        port
    });

    let mut mismatches = Vec::new();
    for (name, raw) in CASES {
        let theirs = normalise(&exchange(nginx.port, raw));
        let mine = normalise(&exchange(ours, raw));
        if theirs == mine {
            println!("  ok   {name} ({} bytes)", theirs.len());
        } else {
            println!("  DIFF {name}\n    nginx: {theirs:?}\n    ours : {mine:?}");
            mismatches.push(*name);
        }
    }
    assert!(
        mismatches.is_empty(),
        "the disguise differs from a live nginx in {} of {} classes: {:?}",
        mismatches.len(),
        CASES.len(),
        mismatches
    );
    println!(
        "\nall {} request classes are byte-identical to nginx",
        CASES.len()
    );

    // ── The named residual, asserted rather than asserted-away ──────
    //
    // These are the inputs where the status class knowingly differs.
    // The property that is NOT allowed to differ is that both servers
    // answer, and answer promptly — which is the whole content of the
    // disguise once the hangs are gone.
    println!("\nnamed residual — status class differs, promptness does not:");
    let oversized = oversized_header_block();
    let residual: Vec<(&str, &[u8])> = RESIDUAL_CASES
        .iter()
        .copied()
        .chain(std::iter::once((
            "oversized header block (nginx's longer 400, ours the ordinary one)",
            oversized.as_slice(),
        )))
        .collect();

    for (name, raw) in residual {
        let started = std::time::Instant::now();
        let theirs = exchange(nginx.port, raw);
        let nginx_took = started.elapsed();
        let started = std::time::Instant::now();
        let mine = exchange(ours, raw);
        let our_took = started.elapsed();

        assert!(
            !theirs.is_empty(),
            "{name}: nginx itself said nothing — the case no longer probes what it claims to"
        );
        assert!(
            !mine.is_empty(),
            "{name}: WE said nothing. A silent hold where nginx answers is the \
             de-anonymising tell this whole module exists to remove."
        );
        assert!(
            our_took < PROMPT,
            "{name}: we took {our_took:?} — nginx took {nginx_took:?}. \
             The residual is allowed to be a different STATUS, never a different \
             response TIME."
        );
        println!(
            "  ok   {name}\n         nginx: {} ({nginx_took:?})\n         ours : {} ({our_took:?})",
            status_of(&theirs),
            status_of(&mine)
        );
    }

    // ── And the one place silence is correct ────────────────────────
    //
    // A partial head on a supported version: nginx waits out
    // `client_header_timeout` and then says NOTHING — not the 408 the
    // RFC would suggest. Measured, and the reason this module does not
    // volunteer a 408 either. Both sides must be silent inside the
    // probe window.
    for (name, raw) in [
        ("partial head", &b"GET /nope HTTP/1.1\r\nHost: x\r\n"[..]),
        ("nothing at all", b""),
    ] {
        assert!(
            exchange(nginx.port, raw).is_empty(),
            "{name}: nginx answered where it was expected to stay silent"
        );
        assert!(
            exchange(ours, raw).is_empty(),
            "{name}: we volunteered an answer where nginx stays silent — as \
             distinguishing as the hang, in the other direction"
        );
        println!("  ok   silence: {name}");
    }
}
