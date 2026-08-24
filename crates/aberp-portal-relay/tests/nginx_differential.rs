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
//! `400`) or accepts bytes this parser cannot hold. There the assertion
//! is the one the disguise actually rests on: **a prompt answer, never
//! a hang**. See ADR-0115 §2.
//!
//! # And [`WITHHELD`], because byte-parity is not the whole claim
//!
//! The fifth hang primitive sent nginx's *exact bytes* — sixty seconds
//! late. A diff cannot see that. So the requests that declare a body
//! and never send it are timed to their first byte against nginx's, and
//! the connection slot one of them pins is measured against nginx's own
//! five seconds. Every body-bearing case in [`CASES`] before round 4
//! sent a COMPLETE body, which is precisely the gap the bug lived in.

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
    //
    // These three were named "unterminated" and were nothing of the
    // sort — they end in `\r\n`, which is the very newline the parser
    // keys on, so they answered instantly while the real case behind
    // them hung for sixty seconds. Renamed to what they actually are,
    // and the LF-less family they were standing in for is below.
    ("400 bad request line, no blank line", b"NOT A VALID REQUEST LINE\r\n"),
    ("400 space in target, no blank line", b"GET /no pe HTTP/1.1\r\n"),
    ("505 HTTP/9.9, no blank line", b"GET /nope HTTP/9.9\r\n"),
    // ── Request lines with NO `\n` ANYWHERE ─────────────────────────
    //
    // The sixth hang primitive. nginx rejects a line it can already
    // prove wrong at the offending BYTE — it never waits for a
    // terminator to do it — and each of these used to be a 60-second
    // silent hold that the canary never even saw. `GET\rZ` is five
    // bytes.
    ("400 garbage line, no LF", b"NOT A VALID REQUEST LINE"),
    ("400 bare CR after the method", b"GET\rZ"),
    ("400 bare CR, nothing after", b"GET\r"),
    ("400 bare CR after a complete line", b"GET /nope HTTP/1.1\rX"),
    ("400 NUL in target, no LF", b"GET /no\0pe HTTP/1.1"),
    ("400 tab in target, no LF", b"GET /no\tpe HTTP/1.1"),
    ("400 DEL in target, no LF", b"GET /no\x7fpe HTTP/1.1"),
    ("400 ctrl in target, no LF", b"GET /no\x01pe HTTP/1.1"),
    ("400 space in target, no LF", b"GET /no pe HTTP/1.1"),
    ("400 fourth field, no LF", b"GET /nope HTTP/1.1 junk"),
    ("400 authority form on GET, no LF", b"GET x:44"),
    ("400 lowercase method, no LF", b"get /nope HTTP/1.1"),
    ("400 malformed version, no LF", b"GET /nope HTTP/x"),
    ("400 zero major, no LF", b"GET /nope HTTP/0"),
    ("400 leading-zero major, no LF", b"GET /nope HTTP/01"),
    ("400 not HTTP at all, no LF", b"GET /nope HTTPX"),
    ("400 trailing junk in version, no LF", b"GET /nope HTTP/1x"),
    ("505 unsupported major, no LF", b"GET /nope HTTP/9"),
    ("505 two-digit major, no LF", b"GET /nope HTTP/10"),
    ("505 HTTP/2.0, no LF", b"GET /nope HTTP/2.0"),
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
    // ── Bodies that are DECLARED and WITHHELD ───────────────────────
    //
    // The gap that let the fifth hang primitive through: every
    // body-bearing case above sends a COMPLETE body, so none of them
    // ever asked what happens when the head promises bytes that never
    // arrive. Each of these used to make the front go silent for the
    // full 60 s `BODY_TIMEOUT` and only then answer, while nginx
    // answers in ~0 ms — measured, both servers, on this exact list.
    //
    // Note what nginx's answer IS, because it is not what "the body
    // could not be drained" suggests: the ordinary **keep-alive** 404,
    // the same 294 bytes as any other 404, with the socket dropped five
    // seconds later. The prompt answer and the lingering drain are two
    // different things, and only the first is on the wire.
    ("404 withheld CL body", b"POST /nope HTTP/1.1\r\nHost: x\r\nContent-Length: 10\r\n\r\n"),
    ("404 withheld CL body, HEAD", b"HEAD /nope HTTP/1.1\r\nHost: x\r\nContent-Length: 10\r\n\r\n"),
    ("404 partial CL body", b"POST /nope HTTP/1.1\r\nHost: x\r\nContent-Length: 10\r\n\r\nab"),
    ("404 withheld CL body, Connection: close", b"POST /nope HTTP/1.1\r\nHost: x\r\nContent-Length: 10\r\nConnection: close\r\n\r\n"),
    ("404 withheld CL body, HTTP/1.0", b"POST /nope HTTP/1.0\r\nHost: x\r\nContent-Length: 10\r\n\r\n"),
    ("405 withheld CL body", b"FROBNICATE /nope HTTP/1.1\r\nHost: x\r\nContent-Length: 10\r\n\r\n"),
    ("404 withheld chunk size", b"POST /nope HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n"),
    ("404 withheld chunk data", b"POST /nope HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nab"),
    ("404 withheld trailer terminator", b"POST /nope HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n"),
    ("400 no Host, withheld body", b"POST /nope HTTP/1.1\r\nContent-Length: 10\r\n\r\n"),
    ("505 withheld body", b"POST /nope HTTP/2.0\r\nHost: x\r\nContent-Length: 10\r\n\r\n"),
    // ── A leading `+` is not a number ───────────────────────────────
    //
    // `parse::<u64>` and `from_str_radix` both accept one; nginx
    // accepts neither. The status divergence was the small half — the
    // large half is that `+5` and `+A` PARSED, so each was also a
    // withheld-body hang wearing a different hat.
    ("400 Content-Length with a leading plus", b"POST /nope HTTP/1.1\r\nHost: x\r\nContent-Length: +5\r\n\r\nabcde"),
    ("404-close chunk size with a leading plus", b"POST /nope HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n+A\r\n"),
    ("404-close chunk size with a leading minus", b"POST /nope HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n-A\r\n"),
    ("404-close chunk size with a leading space", b"POST /nope HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n 3\r\nabc\r\n0\r\n\r\n"),
    ("404 chunk size padded after the digits", b"POST /nope HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n3 ;a=b\r\nabc\r\n0\r\n\r\n"),
    ("404 Content-Length with leading zeroes", b"POST /nope HTTP/1.1\r\nHost: x\r\nContent-Length: 005\r\n\r\nabcde"),
    // ── A control character in the target is a 400, not a 404 ───────
    //
    // The one class of input where the target's CONTENT changed the
    // answer, in a mimic whose whole claim is that it never reads the
    // target. A high byte is passed through, and stays a 404.
    ("400 tab in target", b"GET /no\tpe HTTP/1.1\r\nHost: x\r\n\r\n"),
    ("400 NUL in target", b"GET /no\0pe HTTP/1.1\r\nHost: x\r\n\r\n"),
    ("400 bare CR in target", b"GET /no\rpe HTTP/1.1\r\nHost: x\r\n\r\n"),
    ("400 DEL in target", b"GET /no\x7fpe HTTP/1.1\r\nHost: x\r\n\r\n"),
    // ── The method charset is [A-Z_-], not RFC-9110 `token` ─────────
    ("400 lowercase method", b"get /nope HTTP/1.1\r\nHost: x\r\n\r\n"),
    ("400 mixed-case method", b"Get /nope HTTP/1.1\r\nHost: x\r\n\r\n"),
    ("400 digit in method", b"GET2 /nope HTTP/1.1\r\nHost: x\r\n\r\n"),
    ("400 tilde in method", b"GE~T /nope HTTP/1.1\r\nHost: x\r\n\r\n"),
    ("400 dot in method", b"GE.T /nope HTTP/1.1\r\nHost: x\r\n\r\n"),
    ("405 underscore in method", b"GET_X /nope HTTP/1.1\r\nHost: x\r\n\r\n"),
    ("405 hyphen in method", b"GE-T /nope HTTP/1.1\r\nHost: x\r\n\r\n"),
    // TRACE and CONNECT are the two verbs nginx singles out: a 405 that
    // closes, and a 400 from the request-line parser.
    ("405-close TRACE", b"TRACE /nope HTTP/1.1\r\nHost: x\r\n\r\n"),
    ("405-close CONNECT authority form", b"CONNECT x:443 HTTP/1.1\r\nHost: x\r\n\r\n"),
    ("400 CONNECT with a path target", b"CONNECT /nope HTTP/1.1\r\nHost: x\r\n\r\n"),
    ("400 GET with an authority-form target", b"GET x:443 HTTP/1.1\r\nHost: x\r\n\r\n"),
    ("405 PUT", b"PUT /nope HTTP/1.1\r\nHost: x\r\n\r\n"),
    // ── The version grammar, which was wrong in five directions ─────
    ("404 HTTP/1.11", b"GET /nope HTTP/1.11\r\nHost: x\r\n\r\n"),
    ("404 HTTP/1.2", b"GET /nope HTTP/1.2\r\nHost: x\r\n\r\n"),
    ("404 HTTP/1.9", b"GET /nope HTTP/1.9\r\nHost: x\r\n\r\n"),
    ("404 HTTP/1.01", b"GET /nope HTTP/1.01\r\nHost: x\r\n\r\n"),
    ("404 HTTP/1.999", b"GET /nope HTTP/1.999\r\nHost: x\r\n\r\n"),
    // A zero minor is HTTP/1.0 semantics, however it is spelled — which
    // is what decides keep-alive, so this is a 289-byte close.
    ("404-close HTTP/1.00", b"GET /nope HTTP/1.00\r\nHost: x\r\n\r\n"),
    ("400 HTTP/01.1", b"GET /nope HTTP/01.1\r\nHost: x\r\n\r\n"),
    ("400 HTTP/0.9", b"GET /nope HTTP/0.9\r\nHost: x\r\n\r\n"),
    ("400 HTTP/1.1000", b"GET /nope HTTP/1.1000\r\nHost: x\r\n\r\n"),
    ("400 HTTP/1.", b"GET /nope HTTP/1.\r\nHost: x\r\n\r\n"),
    ("505 HTTP/11.1", b"GET /nope HTTP/11.1\r\nHost: x\r\n\r\n"),
    ("505 HTTP/1000.1", b"GET /nope HTTP/1000.1\r\nHost: x\r\n\r\n"),
];

/// The withheld-body family, timed rather than merely diffed.
///
/// [`CASES`] proves these are answered with the same BYTES; this list
/// is where the same inputs are held to the same CLOCK. Byte parity
/// alone would have been satisfied by the bug: the old code sent
/// exactly these bytes — sixty seconds late.
const WITHHELD: &[(&str, &[u8])] = &[
    (
        "withheld CL body",
        b"POST /nope HTTP/1.1\r\nHost: x\r\nContent-Length: 10\r\n\r\n",
    ),
    (
        "withheld CL body, HEAD",
        b"HEAD /nope HTTP/1.1\r\nHost: x\r\nContent-Length: 10\r\n\r\n",
    ),
    (
        "withheld CL body on the 405 path",
        b"FROBNICATE /nope HTTP/1.1\r\nHost: x\r\nContent-Length: 10\r\n\r\n",
    ),
    (
        "withheld chunk size",
        b"POST /nope HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n",
    ),
    (
        "withheld chunk data",
        b"POST /nope HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nab",
    ),
    (
        "withheld trailer terminator",
        b"POST /nope HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n",
    ),
    (
        "partial CL body",
        b"POST /nope HTTP/1.1\r\nHost: x\r\nContent-Length: 10\r\n\r\nab",
    ),
    (
        "withheld body behind a leading-plus chunk size",
        b"POST /nope HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n+A\r\n",
    ),
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
        "transfer-coding LIST (nginx 501, ours 400)",
        b"POST /nope HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked, chunked\r\n\r\n0\r\n\r\n",
    ),
    (
        "over-long Content-Length (nginx 413, ours 400)",
        b"POST /nope HTTP/1.1\r\nHost: x\r\nContent-Length: 99999999\r\n\r\n",
    ),
    // A chunk larger than `MAX_REQUEST_BODY`. Same status as nginx —
    // it has not seen the body yet either — but we mark the connection
    // unreusable where nginx keeps it alive, because we have already
    // decided we will not drain it. `client_max_body_size` is per-site
    // configuration, so there is no single nginx answer to match.
    (
        "over-long chunk (same 404, ours closes where nginx keeps alive)",
        b"POST /nope HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n200000\r\n",
    ),
    // Found in round 4 while closing the control-character oracle, and
    // named here rather than quietly left out: nginx's request line is
    // BYTES, ours is a `&str`, so a high byte in the target that nginx
    // passes through to its ordinary 404 is a 400 here. Closing it
    // means parsing the head at byte level — a real change to the
    // parser round 3 cleared, not a one-liner — so it is carried as a
    // follow-on in D-20 rather than rushed in behind this fix.
    (
        "non-UTF-8 byte in the target (nginx 404, ours 400)",
        b"GET /no\x80pe HTTP/1.1\r\nHost: x\r\n\r\n",
    ),
    // ── Enumerated in round 5's adversarial, measured here ──────────
    //
    // The lesson of the `+` in a `Content-Length` is that an
    // unenumerated divergence is where the next hang hides, so these
    // are written down and held to the promptness bound rather than
    // left latent. Each is a place the two parsers disagree about
    // VALIDITY — in both directions, which is what makes them
    // fingerprints rather than mere strictness.
    (
        "repeated space in the request line (nginx 404, ours 400)",
        b"GET  /nope HTTP/1.1\r\nHost: x\r\n\r\n",
    ),
    (
        "header line with no colon (nginx ignores it and 404s, ours 400)",
        b"GET /nope HTTP/1.1\r\nHost: x\r\nBadHeaderNoColon\r\n\r\n",
    ),
    (
        "NUL in a header value (nginx 400, ours accepts and 404s)",
        b"GET /nope HTTP/1.1\r\nHost: x\r\nX: a\0b\r\n\r\n",
    ),
    (
        "bare CR in a header value (nginx 400, ours accepts and 404s)",
        b"GET /nope HTTP/1.1\r\nHost: x\r\nX: a\rb\r\n\r\n",
    ),
    // Not a parser divergence at all — a CONFIGURATION one, and the
    // only case in this file whose nginx answer is decided by the
    // fixture rather than by nginx. This test's parked vhost has an
    // EMPTY root, so `/` is a 403; a parked vhost with the stock
    // `index.html` would be a 200, and ours is a 404 whatever the
    // path. Recorded because `/` is the single likeliest request a
    // scanner sends, and because what the production vhost is parked
    // as is a deployment decision this crate cannot make for it.
    (
        "the root path (fixture nginx 403, ours 404 — config-dependent)",
        b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
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

/// Time to the FIRST response byte, and the bytes that came with it.
///
/// Distinct from [`exchange`], which reads to EOF: the withheld-body
/// answers are `keep-alive`, so "read to EOF" would be timing the
/// lingering drain rather than the answer. The answer is the tell.
fn first_byte(port: u16, raw: &[u8]) -> (Vec<u8>, Duration) {
    let started = std::time::Instant::now();
    let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)) else {
        return (Vec::new(), started.elapsed());
    };
    s.set_read_timeout(Some(PROMPT)).ok();
    if s.write_all(raw).is_err() {
        return (Vec::new(), started.elapsed());
    }
    let mut chunk = [0u8; 8192];
    match s.read(&mut chunk) {
        Ok(n) if n > 0 => (chunk[..n].to_vec(), started.elapsed()),
        _ => (Vec::new(), started.elapsed()),
    }
}

/// How long a server holds the connection slot for a body that never
/// arrives — measured from the request to the close.
fn time_to_close(port: u16, raw: &[u8], patience: Duration) -> Option<Duration> {
    let started = std::time::Instant::now();
    let mut s = TcpStream::connect(("127.0.0.1", port)).ok()?;
    s.set_read_timeout(Some(patience)).ok();
    s.write_all(raw).ok()?;
    let mut chunk = [0u8; 8192];
    loop {
        match s.read(&mut chunk) {
            Ok(0) => return Some(started.elapsed()),
            Ok(_) => {}
            Err(_) => return None,
        }
    }
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

    // ── The withheld body, held to the CLOCK as well as the bytes ───
    //
    // The fifth hang primitive, and the reason this section exists at
    // all: byte parity is not enough when the bug SENDS THE RIGHT BYTES
    // and merely sends them a minute late.
    println!("\nwithheld bodies — the answer is prompt, the drain is what lingers:");
    for (name, raw) in WITHHELD {
        let (theirs, nginx_took) = first_byte(nginx.port, raw);
        let (mine, our_took) = first_byte(ours, raw);
        assert!(
            !theirs.is_empty(),
            "{name}: nginx itself said nothing — the case no longer probes what it claims to"
        );
        assert!(
            !mine.is_empty(),
            "{name}: WE said nothing inside {PROMPT:?}. This is the exact shape of the \n\
             de-anonymising tell: nginx answered in {nginx_took:?}, and a host that goes \n\
             silent where every other server on the internet answers has identified itself."
        );
        assert!(
            our_took < PROMPT,
            "{name}: our first byte took {our_took:?} — nginx took {nginx_took:?}. \n\
             Waiting for a body before deciding an answer that never depended on it is \n\
             the whole bug, and shortening the body timeout does not fix it."
        );
        println!(
            "  ok   {name}\n         nginx: {} ({nginx_took:?})\n         ours : {} ({our_took:?})",
            status_of(&theirs),
            status_of(&mine)
        );
    }

    // And the slot: a connection that declares a body and withholds it
    // must be given up on the lingering budget, not held for the body
    // one. It used to cost one packet to pin one of `MAX_CONNECTIONS`
    // for a full minute; nginx gives up after five seconds and so do
    // we. The bound is generous either side of both, so it cannot pass
    // by accident — but it is four times under the 60 s this cost.
    const SLOT: Duration = Duration::from_secs(15);
    let raw = &b"POST /nope HTTP/1.1\r\nHost: x\r\nContent-Length: 10\r\n\r\n"[..];
    let theirs = time_to_close(nginx.port, raw, SLOT);
    let mine = time_to_close(ours, raw, SLOT);
    assert!(
        theirs.is_some(),
        "nginx did not give the slot up inside {SLOT:?} — the case no longer probes what it claims to"
    );
    let mine = mine.unwrap_or_else(|| {
        panic!(
            "we held the connection slot for more than {SLOT:?} on a body that was never \n\
             sent, where nginx let go after {:?}. That is a one-packet slot-exhaustion \n\
             primitive against MAX_CONNECTIONS.",
            theirs.unwrap_or_default()
        )
    });
    println!(
        "\n  ok   the withheld-body slot frees\n         nginx: {:?}\n         ours : {mine:?}",
        theirs.unwrap_or_default()
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
        // Timed to the FIRST byte, not to EOF. Several of these are
        // answered `keep-alive` by one side or the other, and reading
        // to EOF there measures the probe's own patience rather than
        // the server's — it reports ~700 ms for an answer that arrived
        // in 200 µs, which is both meaningless and enough to fail the
        // bound. The same distinction the withheld-body family is
        // built on.
        let (theirs, nginx_took) = first_byte(nginx.port, raw);
        let (mine, our_took) = first_byte(ours, raw);

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
        // The other half of the sixth hang, and not optional: a host
        // that ANSWERS where nginx waits has identified itself just as
        // surely as one that waits where nginx answers. Every one of
        // these is a request-line prefix that could still become valid.
        ("a valid line, no terminator", b"GET /nope HTTP/1.1"),
        ("mid-version", b"GET /nope HTTP/1."),
        ("mid-major", b"GET /nope HTTP/1"),
        ("mid-literal", b"GET /nope HTT"),
        ("mid-target", b"GET /no"),
        ("mid-method", b"GE"),
        ("an underscore method", b"GET_X /nope"),
        ("absolute form, one slash so far", b"GET http:/"),
        ("absolute form", b"GET http://x/p"),
        ("a bare host, no colon yet", b"GET x"),
        ("a trailing space", b"GET /nope HTTP/1.1 "),
        ("the CR of a line it has", b"GET /nope HTTP/1.1\r"),
        ("a four-digit zero minor", b"GET /nope HTTP/1.0000"),
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
