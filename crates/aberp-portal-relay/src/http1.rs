//! We own the connection (ADR-0115 §3.3).
//!
//! # Why this exists at all
//!
//! The disguise in [`crate::nginx`] is only as good as the *lowest*
//! layer that can answer. Running the front on `axum`/`hyper` meant
//! hyper answered anything that failed to parse — its own `400`, with
//! its own header set, its own header **order**, and no `Server:` line
//! — before a single line of this crate ran. Every protocol-level
//! probe therefore bypassed the mimic entirely, and those are exactly
//! the probes a scanner sends first.
//!
//! There is no hook for this. hyper's connection-level error responses
//! are not a `Service` and cannot be replaced. The only way to answer a
//! malformed request line the way nginx answers it is to be the code
//! that reads the request line. So this module is a small, complete,
//! deliberately boring HTTP/1.1 server: it parses, it bounds, it hands
//! a [`RequestHead`] to a [`Handler`], and it writes every response
//! byte itself.
//!
//! The relay's dependency list got *shorter* as a result — `axum`,
//! `axum-server` and `tower` are gone — which for a component whose
//! whole claim is "it holds nothing and decides nothing" is a
//! secondary win worth naming.
//!
//! # What it deliberately does not implement
//!
//! No HTTP/2, no upgrades, no `100-continue`, no trailers, no
//! compression. A parked nginx with a static root exercises none of
//! them, and each would be another surface to get subtly wrong. HTTP/2
//! arriving in the clear at this listener is a
//! [`nginx::Class::VersionNotSupported`], which is precisely what real
//! nginx does with it.
//!
//! # The guarantee, stated exactly
//!
//! **No hang, no socket desynchronisation, and byte-identical bytes on
//! the enumerated common request classes** (ADR-0115 §2). Not "byte
//! parity with nginx", which is broader than this code holds and
//! broader than it needs to be.
//!
//! *No hang* is the load-bearing half, and it is the half rounds 3, 4
//! and 5 had to fix. Six inputs each bought a ~60-second **silent**
//! hold for the price of one short line: a real HTTP/0.9 request, a
//! `Transfer-Encoding` that merely *contained* `chunked`, a chunked
//! trailer section that never terminates, an endless stream of leading
//! CRLFs, **any request that declares a body and then withholds it**,
//! and **any doomed request line that carries no `\n`** — `GET\rZ`, five
//! bytes. Each was worse than a wrong answer. Real nginx answers all
//! six in microseconds — measured — so the hang identified the host
//! outright, and each returned before `observe_protocol_error`, so the
//! canary never saw the probe that found it. Four rules keep them
//! dead:
//!
//! 1. **A request line is decided as soon as it CAN be — which is
//!    usually before its newline, and sometimes before its last
//!    byte.** Only HTTP/1.0 and 1.1 have a header block to wait for;
//!    everything else is complete when the line is. But "when the
//!    line is" was still too late, and that was the sixth primitive:
//!    a line that can never parse is refused at the offending byte,
//!    with no terminator required, exactly as nginx's scanner does —
//!    see [`request_line_prefix_verdict`]. The converse is equally
//!    load-bearing: a prefix that could still become valid is waited
//!    for in silence, because answering where nginx waits is the same
//!    tell pointing the other way.
//! 2. **Body framing is decided from the head**, before a byte of body
//!    is read. Every way of disagreeing about framing is answered
//!    immediately rather than discovered halfway through a decoder.
//! 3. **Budgets are totals, armed once — by the first byte read, and
//!    checked in the loop.** A per-read timeout is not a bound: one
//!    byte every 59 seconds renews it forever. Nor is a timeout *around
//!    the read* enough on its own — it fires only when the inner future
//!    is pending at the deadline, and a peer that keeps the socket full
//!    makes every read ready. A timeout that wraps an I/O future bounds
//!    waiting, not work.
//! 4. **A body is never waited for to decide an answer that does not
//!    depend on it.** Every class an unauthenticated caller can reach
//!    — the parked 404, the 405, a protocol refusal — is answered from
//!    the head, and the body is *discarded* rather than read: whatever
//!    already arrived, before the write; the rest afterwards, on
//!    [`LINGERING_TIMEOUT`]. Only the post-knock `/api/` forward reads
//!    a body, and it costs a valid knock token to reach. This is the
//!    round-4 fix, and note what it is *not*: shortening
//!    [`BODY_TIMEOUT`] would not have worked, because nginx answers at
//!    once regardless, so any wait at all before the answer is
//!    measurable. See [`Handler::needs_body`] and [`Discard`].
//!
//! Silence is correct in exactly one place, and it is captured rather
//! than chosen: a *partial but well-formed* head is waited for and then
//! dropped without a word, because that is what nginx does with a
//! `client_header_timeout` — it does **not** send the `408` the RFC
//! would suggest, and volunteering one would be as distinguishing as
//! the hang was.
//!
//! **The residual, stated at the strength it actually holds.** It used
//! to be written as three named cases. That was an overstatement, and
//! round 4 found it by measuring rather than by re-reading: a lowercase
//! method, a tab or NUL in the target, `HTTP/1.11`, `Content-Length:
//! +5` and a `+A` chunk size were each a status difference nobody had
//! enumerated — and the last two were withheld-body hangs as well,
//! because Rust's `parse` accepts a leading `+` and nginx does not.
//! Those are all closed now, and are cases in the differential.
//!
//! What is claimed is therefore:
//!
//! > **Promptness holds everywhere. Byte-parity holds on the
//! > enumerated classes. Outside them the status class may differ.**
//!
//! The remainder is not a fixed list of three, and is not asserted to
//! be: `RESIDUAL_CASES` in the differential carries the ones that are
//! known — nginx's `501` for a transfer-coding it does not implement,
//! its `413` for an over-long body, its distinct longer `400` for an
//! oversized header block, and a non-UTF-8 byte in the target, which
//! nginx passes through to its ordinary 404 and this parser refuses
//! because it holds the request line as a `&str`. Each is asserted to
//! be *prompt*, which is the property the disguise rests on. See
//! [`framing_of`] and ADR-0115 §2 for why chasing nginx's full status
//! table is bottomless and why a status difference is a far weaker
//! signal than a hang.
//!
//! # Parsing posture
//!
//! Liberal exactly where nginx is liberal (leading CRLFs, bare-LF line
//! endings, absolute-form targets) and strict exactly where nginx is
//! strict (a space inside the target, a duplicated `Host`, a `Host`
//! missing on HTTP/1.1, a malformed header name). Every one of those
//! is a captured behaviour, not a guess — see
//! `tests/fixtures/nginx-goldens.txt`.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

use crate::nginx::{self, Class};

/// Longest request line, matching nginx's default 8 KiB
/// `large_client_header_buffers` slot. Past it: `414`.
pub const MAX_REQUEST_LINE: usize = 8 * 1024;

/// Longest complete header block. Past it: `400`.
pub const MAX_HEAD: usize = 32 * 1024;

/// Longest request body the front will read.
///
/// The only bodies the portal has are WebAuthn ceremony JSON — a few
/// kilobytes. A relay that buffers whatever it is sent is an OOM
/// waiting to happen on a box whose entire job is to hold nothing
/// (§2.4), so the limit is explicit rather than inherited from a
/// framework default a future upgrade could change.
pub const MAX_REQUEST_BODY: usize = 64 * 1024;

/// Longest trailer section after a chunked body's final chunk.
///
/// Bounded because the trailer loop is otherwise the cheapest hold in
/// the whole parser: `0\r\n` followed by `X: y\r\n` forever is a few
/// bytes per iteration and the loop had no exit but the peer's good
/// manners. The portal has no use for trailers at all — this is the
/// budget for *refusing* them, not for reading them.
pub const MAX_TRAILERS: usize = 4 * 1024;

/// nginx's `client_header_timeout`.
///
/// A **total** budget for the head, armed by its first byte, not a
/// per-read one. nginx arms this timer once in
/// `ngx_http_wait_request_handler` and does not rearm it as bytes
/// arrive; a per-read timeout would let a client drip one byte every
/// 59 seconds and hold a slot indefinitely while never once timing out.
pub const HEADER_TIMEOUT: Duration = Duration::from_secs(60);
/// nginx's `client_body_timeout`. Total for the body, same reasoning.
///
/// Reached only on the one path whose ANSWER depends on the body — the
/// post-knock `/api/` forward, which costs a valid knock token to
/// reach. Every other class is answered from the head and its body
/// merely discarded on [`LINGERING_TIMEOUT`]; see [`serve`].
pub const BODY_TIMEOUT: Duration = Duration::from_secs(60);

/// nginx's `lingering_timeout` — how long a body whose answer has
/// **already gone out** may take to finish arriving.
///
/// Measured against nginx 1.31.4: `POST /nope` with `Content-Length:
/// 10` and no body is answered in ~0 ms with the ordinary keep-alive
/// `404` (294 bytes, identical to any other `404`), and the socket is
/// dropped 5 s later. The answer is prompt; the drain is what lingers.
/// Reproducing that split is the whole of the round-4 body-side fix —
/// waiting for the body *before* answering was a 60-second silent hold
/// where nginx answers instantly.
pub const LINGERING_TIMEOUT: Duration = Duration::from_secs(5);

/// Most connections one listener serves at once.
///
/// `main.rs` used to `tokio::spawn` per accept with no bound at all,
/// which is an unbounded task-and-buffer allocator for anybody who can
/// open sockets. The permit is taken **before** `accept()` so surplus
/// connections wait in the kernel's listen backlog rather than being
/// accepted and dropped — which is both what nginx does when
/// `worker_connections` is exhausted and the only version that does not
/// hand a prober a distinguishable "accepted, then immediately closed"
/// signature.
pub const MAX_CONNECTIONS: usize = 512;

/// How long a TLS handshake may take before its slot is reclaimed.
///
/// Without it, opening a socket and sending nothing holds a connection
/// slot forever: the handshake future never completes and none of the
/// timeouts below have started, because none of this module is running
/// yet.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// nginx's `keepalive_timeout` — how long an idle kept-alive
/// connection is held before being closed.
pub const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(75);

/// The HTTP version on the request line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version {
    /// No version at all — `GET /path`. Answered with the bare body.
    Http09,
    /// `HTTP/1.0`: closes by default, `Host` is optional.
    Http10,
    /// `HTTP/1.1`: keeps alive by default, `Host` is mandatory.
    Http11,
}

/// A parsed request head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestHead {
    /// Uppercase-as-sent method token.
    pub method: String,
    /// The request target exactly as sent — origin-form or
    /// absolute-form, never decoded, never normalised.
    pub target: String,
    pub version: Version,
    /// Header names lowercased; values trimmed of surrounding spaces.
    pub headers: Vec<(String, String)>,
    /// What the client asked for, before any class-specific override.
    pub client_wants_keep_alive: bool,
}

impl RequestHead {
    /// First value for `name`, which is already lowercase.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// The path component of [`RequestHead::target`], without the query.
    ///
    /// Purely lexical, and absolute-form aware only to the extent of
    /// skipping the authority: `http://host/a?b` yields `/a`. No
    /// percent-decoding and no `..` collapsing, so there is no
    /// decoded-versus-compared discrepancy for an attacker to wedge
    /// apart.
    #[must_use]
    pub fn path(&self) -> &str {
        let t = &self.target;
        let after_authority = match t.find("://") {
            Some(i) => t[i + 3..].find('/').map_or("/", |j| &t[i + 3 + j..]),
            None => t.as_str(),
        };
        after_authority
            .split_once('?')
            .map_or(after_authority, |(p, _)| p)
    }

    /// The raw query string, without `?`.
    #[must_use]
    pub fn query(&self) -> Option<&str> {
        self.target.split_once('?').map(|(_, q)| q)
    }

    /// `true` for `HEAD`, where a response carries its headers and
    /// `Content-Length` but no body.
    #[must_use]
    pub fn is_head(&self) -> bool {
        self.method == "HEAD"
    }
}

/// What a [`Handler`] decided to answer with.
#[derive(Debug)]
pub enum Answer {
    /// Be a parked nginx. Byte-for-byte [`crate::nginx`].
    Nginx(Class),
    /// A real portal response — the shell, or an answer the Mac
    /// produced. This is the only branch that ever carries a security
    /// header, a cookie or a cache directive.
    Portal(Box<PortalAnswer>),
}

impl Answer {
    /// The ordinary parked answer.
    #[must_use]
    pub const fn not_found() -> Self {
        Self::Nginx(Class::NotFound)
    }
}

/// An authenticated response, past the knock.
#[derive(Debug)]
pub struct PortalAnswer {
    pub status: u16,
    pub reason: &'static str,
    pub content_type: String,
    pub body: Vec<u8>,
    /// A complete `Set-Cookie` minted by the agent. The relay never
    /// constructs one — it cannot mint a session (§4.2).
    pub set_cookie: Option<String>,
}

/// Something that can answer a parsed request.
///
/// Boxed rather than an RPIT so this module stays free of any knowledge
/// of [`crate::front`] — which also makes it testable against a stub.
pub trait Handler: Send + Sync + 'static {
    fn handle<'a>(
        &'a self,
        head: &'a RequestHead,
        body: &'a [u8],
        peer: Option<SocketAddr>,
    ) -> Pin<Box<dyn Future<Output = Answer> + Send + 'a>>;

    /// Called for a request this module refused before it could reach
    /// [`Handler::handle`] — a malformed request line, an unsupported
    /// version, an over-long URI, framing headers that cannot be
    /// agreed, or a body that could not be read.
    ///
    /// The body cases were added in round 3 and are not incidental:
    /// the three hang primitives all lived in the body reader and all
    /// returned *before* this was called, so the trap never saw the
    /// probes that found them — a blind spot at precisely the place a
    /// prober was proved to aim.
    ///
    /// Synchronous and required to be non-blocking: it runs on the
    /// connection task, immediately before the refusal is written, and
    /// the whole point of the disguise is that the refusal is not
    /// slower than anything else.
    ///
    /// `hint` is the first line of the request as it arrived, truncated
    /// and lossily decoded. It is raw attacker bytes and must be
    /// sanitised before it reaches any log.
    fn observe_protocol_error(&self, class: Class, peer: Option<SocketAddr>, hint: Option<&str>);

    /// Whether this handler needs the request BODY to decide its
    /// answer to `head`.
    ///
    /// Required rather than defaulted, because getting it wrong in the
    /// permissive direction reinstates a 60-second silent hold and
    /// getting it wrong in the other direction is a loud, immediate
    /// test failure. A new handler does not get to not think about it.
    ///
    /// Answering `false` does **not** mean the body is ignored: it is
    /// still drained, so the socket cannot desynchronise. It means the
    /// answer is written from the head *first* and the drain lingers
    /// afterwards, which is what nginx does and what this relay used
    /// not to do. See [`serve`].
    ///
    /// Almost everything is `false`. On the front, only the post-knock
    /// `/api/` forward genuinely needs bytes to hand the Mac — and
    /// reaching it costs a valid knock token, so no prober is on that
    /// path.
    fn needs_body(&self, head: &RequestHead) -> bool;

    /// Largest request body this handler will accept.
    ///
    /// Per-handler because the two listeners have genuinely different
    /// needs: the browser front only ever receives WebAuthn ceremony
    /// JSON and holds itself to [`MAX_REQUEST_BODY`], while the agent
    /// leg receives whole invoice PDFs and needs
    /// `aberp_portal_core::proto::MAX_BODY_BYTES`. Making the front
    /// carry the larger of the two would hand every anonymous caller an
    /// 8 MiB allocation for the asking.
    fn max_body(&self) -> usize {
        MAX_REQUEST_BODY
    }
}

/// The first line of a malformed request, truncated for the canary.
///
/// Bounded hard: this is the one place attacker-chosen bytes are
/// carried forward from a request that was never parsed, and an
/// unbounded copy of it would be a memory-growth primitive.
const MAX_HINT: usize = 96;

fn hint_of(raw: &[u8]) -> String {
    let line = raw.split(|b| *b == b'\n').next().unwrap_or(raw);
    let end = line.len().min(MAX_HINT);
    String::from_utf8_lossy(&line[..end])
        .trim_end_matches('\r')
        .to_string()
}

/// Why the head could not be turned into a [`RequestHead`].
///
/// Every variant carries the [`Class`] nginx answers it with, so the
/// mapping from "what went wrong" to "what the wire sees" is one table
/// in one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadError {
    /// The peer closed before sending anything. Not an error, and
    /// notably answered with **nothing at all** — nginx says nothing to
    /// a connection that opens and closes, and a server that volunteers
    /// a `400` there is distinguishable by a bare port scan.
    Closed,
    /// Answer with this class and close.
    Refuse(Class),
}

/// Why a request body could not be produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyError {
    /// A framing error. Answer with this class and close.
    Refuse(Class),
    /// The body was well-framed but did not arrive — truncated, or the
    /// body budget expired.
    ///
    /// Deliberately **not** a `400`. Measured against nginx 1.31.4: a
    /// static-file request whose body cannot be drained is answered
    /// with the ordinary parked page and `Connection: close` — 289
    /// bytes rather than the 294 of the keep-alive form — not with a
    /// protocol error. A `400` here was a fingerprint.
    Undrainable,
}

/// How a request's body is framed, decided entirely from the head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Framing {
    /// No body.
    None,
    /// `Content-Length`.
    Length(u64),
    /// `Transfer-Encoding: chunked`, and nothing else.
    Chunked,
}

/// Resolve the framing, refusing every combination nginx refuses.
///
/// Called from [`parse_head`], so a request whose framing cannot be
/// agreed is answered **from the head, without reading a byte of body**
/// — which is the whole point. `Transfer-Encoding: xchunked` used to be
/// matched with `contains("chunked")` and routed into the chunked
/// decoder, where it blocked on a chunk size that was really the first
/// bytes of a `Content-Length` body: a 60-second silent hold, obtained
/// for the price of one header, against a host whose disguise is that
/// it answers instantly like everything else on the internet.
///
/// # Where this knowingly differs from nginx
///
/// nginx answers an *unknown transfer-coding* with `501 Not
/// Implemented` and an over-long `Content-Length` with `413 Request
/// Entity Too Large`. [`Class`] has neither, and both are answered here
/// with `400`. That is part of the status-code residual in ADR-0115 §2
/// — see the module docs, and note that the residual is a *shape* of
/// difference rather than a closed list of three, which is how a
/// leading `+` in a `Content-Length` sat outside it unnoticed for two
/// rounds. What matters for the disguise is that the answer is
/// **immediate**, which it is; a status-code difference on a request no
/// client sends is a far weaker signal than a hang, and real nginx
/// deployments vary on it too.
fn framing_of(version: Version, headers: &[(String, String)]) -> Result<Framing, Class> {
    let value = |want: &str| -> Vec<&str> {
        headers
            .iter()
            .filter(|(k, _)| k == want)
            .map(|(_, v)| v.as_str())
            .collect()
    };
    let te = value("transfer-encoding");
    let cl = value("content-length");

    // Two `Content-Length` lines are a request-smuggling primitive
    // before they are a malformed request: the two ends of a chain can
    // pick different ones and disagree about where this request stops
    // and the next begins. nginx answers 400; measured, not assumed.
    if cl.len() > 1 {
        return Err(Class::BadRequest);
    }

    if let Some(first) = te.first() {
        // Both framings at once: same disagreement, same answer. nginx
        // answers 400 here even though it answers 501 to an unknown
        // coding — the two cases are genuinely different to it.
        if !cl.is_empty() || te.len() > 1 {
            return Err(Class::BadRequest);
        }
        // Chunked is an HTTP/1.1 framing. nginx answers 400 to it on
        // 1.0, and a 1.0 request that claims it is one we would
        // otherwise mis-frame.
        if version != Version::Http11 {
            return Err(Class::BadRequest);
        }
        // Exactly `chunked`, not "contains chunked". nginx supports no
        // other transfer-coding and no coding list.
        if !first.trim().eq_ignore_ascii_case("chunked") {
            return Err(Class::BadRequest);
        }
        return Ok(Framing::Chunked);
    }

    match cl.first() {
        None => Ok(Framing::None),
        Some(raw) => {
            let text = raw.trim();
            // DIGIT+ and nothing else. `parse::<u64>` alone rejects
            // `-1` and `abc` — which nginx also answers with 400 — but
            // it *accepts* a leading `+`, and nginx does not: measured,
            // `Content-Length: +5` gets nginx's 400. That was two
            // faults in one character. The status divergence was the
            // small half; the large half is that `+5` sailed through
            // into the body reader, which then waited for five bytes
            // the sender never had to send.
            if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
                return Err(Class::BadRequest);
            }
            text.parse::<u64>()
                .map(Framing::Length)
                .map_err(|_| Class::BadRequest)
        }
    }
}

/// A chunk-size line, by nginx's grammar rather than Rust's.
///
/// `usize::from_str_radix(_, 16)` is more liberal than nginx in exactly
/// the ways that matter: it accepts a leading `+` (and `-`, harmlessly,
/// for an unsigned target). Measured against nginx 1.31.4, each of
/// `+A`, `-A`, `0x3` and a chunk size with a **leading** space gets the
/// ordinary parked page with `Connection: close` — 289 bytes — while
/// `003`, `3 `, `3;` and `3 ;a=b` are accepted and keep the connection
/// alive. So: HEXDIG+ with optional trailing whitespace and an optional
/// `;ext`, and nothing else.
///
/// `+A` was a hang variant as well as a status divergence — it parsed
/// as 10, so the decoder waited for ten bytes of chunk data that were
/// never coming.
fn chunk_size(line: &str) -> Option<usize> {
    let text = line
        .split(';')
        .next()
        .unwrap_or("")
        .trim_end_matches([' ', '\t']);
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    usize::from_str_radix(text, 16).ok()
}

/// Where a body that will be **discarded** has got to.
///
/// nginx discards a body it is not going to use in two bites, and the
/// split is the whole of the round-4 fix. Everything already readable
/// is consumed *before* the response goes out — which is what decides
/// `Connection:` — and the remainder is drained *after*, on
/// [`LINGERING_TIMEOUT`]. Reproducing that means the decoder has to be
/// pausable across the write, so its state is a value rather than a
/// stack frame.
///
/// Measured against nginx 1.31.4, and every row of this is a case in
/// `tests/nginx_differential.rs`:
///
/// | body | nginx answers |
/// |---|---|
/// | complete, in the first segment | 404 `keep-alive`, 294 B |
/// | declared and withheld entirely | 404 `keep-alive`, 294 B, socket dropped 5 s later |
/// | partial (`2` of `10` bytes) | 404 `keep-alive`, 294 B |
/// | bogus chunk size, **in the first segment** | 404 `close`, 289 B |
/// | the same bogus chunk size, **arriving 150 ms later** | 404 `keep-alive`, 294 B |
///
/// The last two rows are one input answered two ways, and they are the
/// reason phase one waits for nothing at all: nginx's verdict depends
/// only on what had already arrived when it looked. That race is not a
/// flaw to be smoothed over — it *is* the behaviour, and a mimic that
/// waited for the bytes to settle would answer `close` where nginx
/// answers `keep-alive`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Discard {
    /// The whole declared body is consumed; the socket is aligned.
    Done,
    /// This many more bytes of a `Content-Length` body are owed.
    Length(usize),
    /// A chunk-size line is expected next.
    ChunkSize,
    /// This many more bytes of the current chunk's data are owed.
    ChunkData(usize),
    /// The CRLF that closes a chunk is expected next.
    ChunkCrlf,
    /// Trailer lines, with this much of the trailer budget spent.
    Trailers(usize),
    /// The bytes already in hand contradict the declared framing.
    /// nginx marks the connection unreusable and closes.
    Malformed,
}

impl Discard {
    /// The state a request's head implies, before a byte is consumed.
    fn start(head: &RequestHead) -> Self {
        match framing_of(head.version, &head.headers) {
            // Unreachable in practice: `parse_head` agrees the framing
            // before a `RequestHead` exists, so a request whose framing
            // cannot be agreed never gets here. Answered as malformed
            // rather than unwrapped, because "unreachable" and
            // "panics the connection task" are different bets.
            Err(_) => Self::Malformed,
            Ok(Framing::None) => Self::Done,
            Ok(Framing::Length(0)) => Self::Done,
            Ok(Framing::Length(len)) => usize::try_from(len).map_or(Self::Malformed, Self::Length),
            Ok(Framing::Chunked) => Self::ChunkSize,
        }
    }

    /// Nothing more will change without more bytes from the peer.
    const fn is_settled(self) -> bool {
        matches!(self, Self::Done | Self::Malformed)
    }

    /// Consume as much of `buf` as this state allows.
    ///
    /// Pure — no I/O, no `await`, no clock. That is what lets phase one
    /// run without waiting for anything, which is the difference
    /// between answering in microseconds and answering in a minute.
    /// Chunk data is dropped as it arrives rather than accumulated, so
    /// a discarded body costs O(1) memory however large it was
    /// declared to be.
    fn pump(&mut self, buf: &mut Vec<u8>, max_body: usize) {
        loop {
            match *self {
                Self::Done | Self::Malformed => return,
                Self::Length(owed) => {
                    let take = owed.min(buf.len());
                    buf.drain(..take);
                    *self = if owed == take {
                        Self::Done
                    } else {
                        Self::Length(owed - take)
                    };
                    if !self.is_settled() {
                        return;
                    }
                }
                Self::ChunkData(owed) => {
                    let take = owed.min(buf.len());
                    buf.drain(..take);
                    if owed == take {
                        *self = Self::ChunkCrlf;
                    } else {
                        *self = Self::ChunkData(owed - take);
                        return;
                    }
                }
                Self::ChunkCrlf => {
                    if buf.len() < 2 {
                        return;
                    }
                    // Those two bytes are CHECKED, never assumed. If
                    // they are not CRLF the sender and this parser
                    // disagree about where the chunk ended, which is
                    // the exact shape of a desynchronised socket.
                    if &buf[..2] != b"\r\n" {
                        *self = Self::Malformed;
                        return;
                    }
                    buf.drain(..2);
                    *self = Self::ChunkSize;
                }
                Self::ChunkSize => {
                    let Some(line) = take_line(buf) else {
                        // A chunk-size line that never ends is not a
                        // line, it is a memory-growth primitive.
                        if buf.len() > MAX_REQUEST_LINE {
                            *self = Self::Malformed;
                        }
                        return;
                    };
                    let Some(size) = chunk_size(&line) else {
                        *self = Self::Malformed;
                        return;
                    };
                    if size > max_body {
                        *self = Self::Malformed;
                        return;
                    }
                    *self = if size == 0 {
                        Self::Trailers(0)
                    } else {
                        Self::ChunkData(size)
                    };
                }
                Self::Trailers(spent) => {
                    let Some(line) = take_line(buf) else {
                        if buf.len() > MAX_REQUEST_LINE {
                            *self = Self::Malformed;
                        }
                        return;
                    };
                    if line.is_empty() {
                        *self = Self::Done;
                        return;
                    }
                    // Bounded, because an endless run of `X: y\r\n` is
                    // a loop with no exit but the peer's good manners,
                    // and the peer here is by assumption hostile.
                    let spent = spent.saturating_add(line.len() + 2);
                    if spent > MAX_TRAILERS {
                        *self = Self::Malformed;
                        return;
                    }
                    *self = Self::Trailers(spent);
                }
            }
        }
    }
}

/// One CRLF- or LF-terminated line out of `buf`, or `None` if it has
/// not finished arriving. Never reads.
fn take_line(buf: &mut Vec<u8>) -> Option<String> {
    let i = buf.iter().position(|b| *b == b'\n')?;
    let line = String::from_utf8_lossy(&buf[..i])
        .trim_end_matches('\r')
        .to_string();
    buf.drain(..=i);
    Some(line)
}

/// Phase one: consume every byte of the declared body that has
/// **already arrived**, waiting for none.
///
/// This is `recv()`-until-`EAGAIN`, which is what
/// `ngx_http_discard_request_body` does before nginx writes its
/// response. The read is polled exactly once per pass and a `Pending`
/// socket ends the loop, so this function cannot wait however hostile
/// the peer is — there is no timer here to get wrong.
async fn discard_arrived<S>(stream: &mut S, buf: &mut Vec<u8>, state: &mut Discard, max_body: usize)
where
    S: AsyncRead + Unpin,
{
    loop {
        state.pump(buf, max_body);
        if state.is_settled() {
            return;
        }
        let mut chunk = [0u8; 4096];
        let n = std::future::poll_fn(|cx| {
            let mut got = tokio::io::ReadBuf::new(&mut chunk);
            match Pin::new(&mut *stream).poll_read(cx, &mut got) {
                std::task::Poll::Ready(Ok(())) => std::task::Poll::Ready(got.filled().len()),
                // A dead socket and an empty one are the same answer
                // here: there is nothing more to take right now.
                std::task::Poll::Ready(Err(_)) | std::task::Poll::Pending => {
                    std::task::Poll::Ready(0)
                }
            }
        })
        .await;
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// Phase two: drain what is left of a body whose answer is already on
/// the wire, on nginx's lingering budget.
///
/// Returns whether the socket is still aligned and may carry another
/// request. A body that never finishes arriving costs the peer a
/// connection slot for [`LINGERING_TIMEOUT`] — five seconds, against
/// the seventy-five an ordinary idle keep-alive connection is worth
/// and the sixty a socket that says nothing at all is worth. Declaring
/// a body and withholding it is therefore no longer the cheapest way
/// to pin a slot; it is the dearest.
async fn discard_lingering<S>(
    stream: &mut S,
    buf: &mut Vec<u8>,
    state: &mut Discard,
    max_body: usize,
) -> bool
where
    S: AsyncRead + Unpin,
{
    // One total budget, armed here. Every read below shares it, so no
    // amount of dripping extends it.
    let deadline = tokio::time::Instant::now() + LINGERING_TIMEOUT;
    loop {
        state.pump(buf, max_body);
        match *state {
            Discard::Done => return true,
            Discard::Malformed => return false,
            _ => {}
        }
        // Checked in the loop as well as around the read: a peer that
        // keeps the socket full makes every read ready, and a timeout
        // that only wraps an I/O future bounds waiting, not work.
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        let mut chunk = [0u8; 4096];
        let n = match tokio::time::timeout_at(deadline, stream.read(&mut chunk)).await {
            Ok(Ok(n)) if n > 0 => n,
            _ => return false,
        };
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// A declared body this handler will not accept, refused from the
/// declared length alone — before a byte of it is read.
///
/// nginx answers `413 Request Entity Too Large` here and [`Class`] has
/// no `413`, so this is the named status residual (ADR-0115 §2). What
/// is *not* residual is that it is decided from the head.
fn oversized_body(head: &RequestHead, max_body: usize) -> Option<Class> {
    match framing_of(head.version, &head.headers) {
        Ok(Framing::Length(len)) if len > max_body as u64 => Some(Class::BadRequest),
        _ => None,
    }
}

/// A bound on how many connections a listener serves at once.
///
/// A [`tokio::sync::Semaphore`] with a name and a doc comment, because
/// where the permit is taken is the whole design: an accept loop that
/// acquires **before** `accept()` leaves surplus connections in the
/// kernel's listen backlog, while one that acquires after has already
/// allocated the task it was trying not to allocate.
#[derive(Debug)]
pub struct ConnectionLimit {
    slots: Arc<tokio::sync::Semaphore>,
}

impl ConnectionLimit {
    #[must_use]
    pub fn new(max: usize) -> Self {
        Self {
            slots: Arc::new(tokio::sync::Semaphore::new(max)),
        }
    }

    /// Wait for a free slot. The returned permit must be held for as
    /// long as the connection lives.
    ///
    /// Never returns `Err`: the semaphore is owned by this struct and
    /// is never closed, so the only failure mode `acquire_owned`
    /// documents cannot arise.
    pub async fn acquire(&self) -> tokio::sync::OwnedSemaphorePermit {
        Arc::clone(&self.slots)
            .acquire_owned()
            .await
            .expect("the connection-limit semaphore is never closed")
    }

    /// Slots not currently held. For tests and for a health line.
    #[must_use]
    pub fn available(&self) -> usize {
        self.slots.available_permits()
    }
}

impl Default for ConnectionLimit {
    fn default() -> Self {
        Self::new(MAX_CONNECTIONS)
    }
}

/// Serve one accepted connection to completion.
///
/// Handles keep-alive: a well-formed request whose class permits it
/// leaves the socket open for the next one, exactly as nginx does. That
/// is a security property, not a performance one — see
/// [`crate::nginx`] on why always-closing is a fingerprint.
pub async fn serve<S, H>(mut stream: S, peer: Option<SocketAddr>, handler: Arc<H>)
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
    H: Handler,
{
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut first = true;

    loop {
        // The first head gets the header timeout; a subsequent one is
        // an idle kept-alive connection and gets the longer keepalive
        // timeout, matching nginx's two separate knobs.
        let idle = if first {
            HEADER_TIMEOUT
        } else {
            KEEPALIVE_TIMEOUT
        };
        first = false;

        let head_bytes = match read_head(&mut stream, &mut buf, idle, HEADER_TIMEOUT).await {
            Ok(v) => v,
            Err(HeadError::Closed) => return,
            Err(HeadError::Refuse(class)) => {
                handler.observe_protocol_error(class, peer, Some(&hint_of(&buf)));
                // `include_body: true` — we could not parse the method,
                // so we cannot know it was a HEAD. nginx is in the same
                // position and sends the body.
                let bytes = nginx::response(class, &nginx::http_date_now(), false, true);
                let _ = stream.write_all(&bytes).await;
                let _ = stream.flush().await;
                return;
            }
        };

        let head = match parse_head(&head_bytes) {
            Ok(h) => h,
            Err(HeadError::Closed) => return,
            Err(HeadError::Refuse(class)) => {
                handler.observe_protocol_error(class, peer, Some(&hint_of(&head_bytes)));
                let bytes = nginx::response(class, &nginx::http_date_now(), false, true);
                let _ = stream.write_all(&bytes).await;
                let _ = stream.flush().await;
                return;
            }
        };

        // HTTP/0.9 has no headers and no response framing: the body,
        // then the close. It never keeps alive.
        if head.version == Version::Http09 {
            let answer = handler.handle(&head, &[], peer).await;
            let class = match answer {
                Answer::Nginx(c) => c,
                // A 0.9 request cannot have carried a knock in a way
                // the portal would honour, but if a handler ever
                // answers one, it still leaves as a parked 404: the
                // shell is not servable without headers.
                Answer::Portal(_) => Class::NotFound,
            };
            let _ = stream.write_all(&nginx::response_http_0_9(class)).await;
            let _ = stream.flush().await;
            return;
        }

        // A method nginx's static module does not serve. The answer is
        // ours rather than the handler's, but the handler is still told
        // — an odd verb against this host is a probe like any other.
        let unserved =
            (!matches!(head.method.as_str(), "GET" | "HEAD" | "POST")).then_some(Class::NotAllowed);

        // ── Does the ANSWER depend on the body? ──────────────────────
        //
        // Almost never, and that is the whole of the round-4 fix. This
        // loop used to run `read_body` TO COMPLETION before
        // `handler.handle` was allowed to decide anything — so any
        // request that DECLARED a body and then WITHHELD it held the
        // front silent for the full `BODY_TIMEOUT` and only then
        // answered. Sixty seconds, for the price of one header, on a
        // request whose answer never depended on the body at all:
        //
        //     POST /nope HTTP/1.1 / Host: x / Content-Length: 10   (+ no body)
        //
        // Real nginx answers that in ~0 ms with the ordinary
        // keep-alive 404 — the *same 294 bytes* as any other 404 — and
        // drops the socket five seconds later. The silent minute was
        // the body-side twin of the head-side family round 3 killed:
        // the same de-anonymising tell (§2's "no hang"), and a
        // one-packet way to pin one of `MAX_CONNECTIONS` for a minute.
        //
        // Shortening `BODY_TIMEOUT` would not have fixed it. nginx
        // answers at once regardless, so *any* wait before the answer
        // is measurable. The fix is structural: the body is read only
        // on the one path whose answer needs it, and every other class
        // is answered from the head with the body merely discarded —
        // before the write for whatever has already arrived, after it
        // for the rest.
        let needs_body = unserved.is_none() && handler.needs_body(&head);

        if needs_body {
            let body = match read_body(
                &mut stream,
                &mut buf,
                &head,
                handler.max_body(),
                BODY_TIMEOUT,
            )
            .await
            {
                Ok(b) => b,
                Err(e) => {
                    // Either way this is answered and closed, never
                    // held. The class is the difference: a framing
                    // error is the protocol error nginx calls it,
                    // while a body that simply did not arrive gets
                    // nginx's ordinary parked page with
                    // `Connection: close` — measured against nginx
                    // 1.31.4, which does not treat an undrainable
                    // body as a protocol error.
                    let class = match e {
                        BodyError::Refuse(c) => c,
                        BodyError::Undrainable => Class::NotFound,
                    };
                    // Fed to the trap in both cases. The body
                    // reader is exactly where the hang primitives
                    // lived, and a refusal the canary never sees is
                    // a blind spot at the one place a prober was
                    // proved to aim.
                    handler.observe_protocol_error(class, peer, Some(&hint_of(&head_bytes)));
                    let bytes =
                        nginx::response(class, &nginx::http_date_now(), false, !head.is_head());
                    let _ = stream.write_all(&bytes).await;
                    let _ = stream.flush().await;
                    return;
                }
            };
            let answer = handler.handle(&head, &body, peer).await;
            let (bytes, keep) = render(&answer, &head, true);
            if stream.write_all(&bytes).await.is_err() || stream.flush().await.is_err() {
                return;
            }
            if !keep {
                return;
            }
            continue;
        }

        // ── Head-decidable ───────────────────────────────────────────
        //
        // A body too large to be worth draining is refused from its
        // DECLARED length, without reading a byte of it — the one body
        // decision that is still made before the answer, because it can
        // be made from the head.
        if let Some(class) = oversized_body(&head, handler.max_body()) {
            handler.observe_protocol_error(class, peer, Some(&hint_of(&head_bytes)));
            let bytes = nginx::response(class, &nginx::http_date_now(), false, !head.is_head());
            let _ = stream.write_all(&bytes).await;
            let _ = stream.flush().await;
            return;
        }

        // Phase one of the discard: everything already in hand, waiting
        // for nothing. Its only effect on the wire is `Connection:`,
        // and that is exactly nginx's rule — see [`Discard`].
        let mut discard = Discard::start(&head);
        discard_arrived(&mut stream, &mut buf, &mut discard, handler.max_body()).await;

        if discard == Discard::Malformed {
            // The bytes already in hand contradict the framing the head
            // declared. Measured: a bogus chunk size present in the
            // first segment gets nginx's ordinary parked page and
            // `Connection: close` — 289 bytes — not a protocol status,
            // and not a wait. The connection is no longer trustworthy,
            // so it ends here rather than risking a desynchronised
            // next request.
            let class = unserved.unwrap_or(Class::NotFound);
            handler.observe_protocol_error(class, peer, Some(&hint_of(&head_bytes)));
            let bytes = nginx::response(class, &nginx::http_date_now(), false, !head.is_head());
            let _ = stream.write_all(&bytes).await;
            let _ = stream.flush().await;
            return;
        }

        // The body is NEVER passed here, even when all of it happened
        // to arrive. Handing over an empty slice is what makes
        // "answered from the head" a property of the code rather than
        // an accident of timing.
        let answered = handler.handle(&head, &[], peer).await;
        let answer = match unserved {
            Some(class) => Answer::Nginx(class),
            None => answered,
        };
        let (bytes, keep) = render(&answer, &head, !method_forbids_reuse(&head.method));
        if stream.write_all(&bytes).await.is_err() || stream.flush().await.is_err() {
            return;
        }
        if !keep {
            return;
        }
        // Phase two. The answer is already on the wire; what is left of
        // the body is drained so a pipelined request cannot be read as
        // body, and if it never comes the connection closes at the
        // lingering timeout rather than being held for the body one.
        if !discard.is_settled()
            && !discard_lingering(&mut stream, &mut buf, &mut discard, handler.max_body()).await
        {
            return;
        }
    }
}

/// Serialise an answer, returning the bytes and whether the connection
/// survives.
///
/// `may_reuse` is the connection's own verdict, folded in on top of the
/// client's intent and the class's: `TRACE` is refused a kept-alive
/// connection by nginx, and so is a request whose body contradicted its
/// own framing. Both are measured — see [`method_forbids_reuse`] and
/// [`Discard`].
fn render(answer: &Answer, head: &RequestHead, may_reuse: bool) -> (Vec<u8>, bool) {
    let date = nginx::http_date_now();
    match answer {
        Answer::Nginx(class) => {
            let keep = may_reuse && head.client_wants_keep_alive && class.may_keep_alive();
            (nginx::response(*class, &date, keep, !head.is_head()), keep)
        }
        Answer::Portal(p) => {
            let keep = may_reuse && head.client_wants_keep_alive;
            (render_portal(p, &date, keep, !head.is_head()), keep)
        }
    }
}

/// Serialise an authenticated response.
///
/// This is the **only** function in the crate that emits a security
/// header. ADR-0115 §3.2 puts CSP, `Referrer-Policy`,
/// `X-Content-Type-Options`, `X-Frame-Options` and HSTS here and
/// nowhere else, because a parked nginx sends none of them and a
/// response that did would be unique on the whole host. They are worth
/// having on the shell — that is a real browser context with real
/// invoice data in it — and actively harmful on the 404.
fn render_portal(p: &PortalAnswer, date: &str, keep_alive: bool, include_body: bool) -> Vec<u8> {
    use std::fmt::Write as _;

    let mut out = String::with_capacity(p.body.len() + 512);
    let _ = write!(out, "HTTP/1.1 {} {}\r\n", p.status, p.reason);
    let _ = write!(out, "Server: {}\r\n", nginx::SERVER);
    let _ = write!(out, "Date: {date}\r\n");
    let _ = write!(
        out,
        "Content-Type: {}\r\n",
        sanitise_header(&p.content_type)
    );
    let _ = write!(out, "Content-Length: {}\r\n", p.body.len());
    let _ = write!(
        out,
        "Connection: {}\r\n",
        if keep_alive { "keep-alive" } else { "close" }
    );

    // Invoice data must not settle into a phone's HTTP cache: this
    // surface is read from a device that may be shared, synced or lost,
    // and a cached response outlives both the session and a knock
    // rotation.
    out.push_str("Cache-Control: no-store\r\n");
    // HSTS. Deliberately without `includeSubDomains`: the storefront is
    // a different host with its own posture and this surface does not
    // get to speak for it (§8 — "the storefront is untouched").
    out.push_str("Strict-Transport-Security: max-age=31536000\r\n");
    // `frame-ancestors 'none'` is the load-bearing one — it is what
    // stops the portal being framed by a page that already holds a
    // session cookie. `X-Frame-Options` repeats it for anything that
    // predates CSP level 2.
    out.push_str(
        "Content-Security-Policy: default-src 'self'; \
         script-src 'self'; style-src 'self'; img-src 'self' data:; \
         connect-src 'self'; object-src 'none'; base-uri 'none'; \
         form-action 'none'; frame-ancestors 'none'\r\n",
    );
    out.push_str("X-Frame-Options: DENY\r\n");
    out.push_str("X-Content-Type-Options: nosniff\r\n");
    // The knock token is IN THE PATH. Without this, following any
    // outbound link would put the token in a `Referer` header and hand
    // the whole gate to a third party.
    out.push_str("Referrer-Policy: no-referrer\r\n");

    if let Some(c) = &p.set_cookie {
        let _ = write!(out, "Set-Cookie: {}\r\n", sanitise_header(c));
    }
    out.push_str("\r\n");

    let mut bytes = out.into_bytes();
    if include_body {
        bytes.extend_from_slice(&p.body);
    }
    bytes
}

/// Strip anything that could split a header.
///
/// The agent is trusted, but "trusted" is not "unvalidated": a
/// `Set-Cookie` or content type carrying a CR or LF would let a
/// compromised or merely buggy Mac inject arbitrary headers — or a
/// whole second response — into the browser's view. Cheap to prevent
/// here, impossible to detect later.
fn sanitise_header(v: &str) -> String {
    v.chars()
        .filter(|c| *c != '\r' && *c != '\n' && *c != '\0')
        .collect()
}

/// Read bytes until the end of the header block.
///
/// Returns the head **excluding** the terminating blank line; anything
/// read past it stays in `buf` for the body reader (or, on a pipelined
/// connection, for the next request).
async fn read_head<S>(
    stream: &mut S,
    buf: &mut Vec<u8>,
    idle: Duration,
    budget: Duration,
) -> Result<Vec<u8>, HeadError>
where
    S: AsyncRead + Unpin,
{
    // Armed by the first byte READ — not by the first byte *left in the
    // buffer after the CRLF skip*, which is what it used to be and
    // which was a hole big enough to hold a connection open forever.
    // A stream of nothing but `\r\n` leaves the buffer empty on every
    // pass, so the budget never armed, the idle wait was recomputed
    // from `now` each time round, and the peer could keep the slot for
    // as long as it kept typing. Free, silent and unbounded — the
    // fourth costume of the same primitive, found by attacking the
    // round-3 fix rather than the round-2 code.
    let mut deadline: Option<tokio::time::Instant> = None;
    // Set once the request line has been checked, so a head arriving a
    // byte at a time is not re-parsed on every pass. Without it a
    // dribbled 8 KiB request line costs O(n²) parses for O(n) bytes.
    let mut line_checked = false;

    loop {
        // A budget enforced only by wrapping the read is not enforced
        // at all: `timeout_at` yields `Err` only if the inner future is
        // *pending* when the deadline passes, and a peer that keeps the
        // socket full makes every read ready. The check has to be here,
        // in the loop, where it holds however fast the bytes arrive.
        if deadline.is_some_and(|d| tokio::time::Instant::now() >= d) {
            return Err(HeadError::Closed);
        }
        // Leading CRLFs before a request line are skipped by nginx —
        // they are legal debris from a previous pipelined request.
        // Drained in one shift rather than one byte at a time, because
        // `remove(0)` per byte is quadratic and the bytes are
        // attacker-chosen.
        let debris = buf
            .iter()
            .position(|b| *b != b'\r' && *b != b'\n')
            .unwrap_or(buf.len());
        if debris > 0 {
            buf.drain(..debris);
        }
        if let Some((head, rest)) = split_head(buf) {
            *buf = rest;
            return Ok(head);
        }
        // nginx validates the method token as it reads and answers the
        // instant it sees a byte that cannot appear in one. It does NOT
        // wait for a line terminator — which matters, because the
        // commonest malformed input on a public port is a TLS
        // `ClientHello` sent to the cleartext listener, and that never
        // sends a terminator at all. A server that waited would sit
        // silent where nginx answers, and silence is as distinguishing
        // as a wrong answer.
        if method_is_impossible(buf) {
            return Err(HeadError::Refuse(Class::BadRequest));
        }
        // ...and so is the REST of the request line. Round 5 shipped
        // with only the method checked here, which left the sixth
        // costume of the same primitive: a request line that can never
        // parse but contains no `\n` was waited on for the full
        // `HEADER_TIMEOUT` and then dropped in silence. `GET\rZ` — five
        // bytes — bought sixty seconds and never reached the canary,
        // because the timeout path returns `Closed`.
        if let Some(class) = request_line_prefix_verdict(buf) {
            return Err(HeadError::Refuse(class));
        }
        if buf.len() > MAX_HEAD {
            return Err(HeadError::Refuse(Class::BadRequest));
        }
        // A request line that has run past its slot without a newline
        // is 414, and must be answered as such before we have any hope
        // of parsing it.
        if first_line_len(buf) > MAX_REQUEST_LINE {
            return Err(HeadError::Refuse(Class::UriTooLarge));
        }
        // A COMPLETE request line is decided now, not after a blank
        // line that may never come. This is the fix for a whole family
        // of silent 60-second holds, of which HTTP/0.9 was only the
        // most obvious:
        //
        //   GET /nope\r\n                 real 0.9 — sends no blank line
        //   NOT A VALID REQUEST LINE\r\n  malformed — 400
        //   GET /no pe HTTP/1.1\r\n       space in target — 400
        //   GET /nope HTTP/9.9\r\n        unsupported version — 505
        //
        // Every one of those was buffered while `read_head` waited for
        // a terminator the client had no reason to send, then returned
        // `Closed` — silence. Real nginx answers all four in under a
        // millisecond (measured, nginx 1.31.4). Silence where nginx
        // answers instantly is the loudest tell this host can emit, and
        // it costs an attacker one short line to obtain.
        //
        // Only 1.0 and 1.1 have a header block to wait for; everything
        // else is complete at the end of its first line.
        if !line_checked {
            if let Some(head) = request_line_settles_it(buf)? {
                return Ok(head);
            }
            // `request_line_settles_it` returns `None` either because
            // the line has not finished arriving or because it is a
            // 1.0/1.1 line whose headers are still to come. Only the
            // second is settled, and only that one may be memoised.
            line_checked = buf.contains(&b'\n');
        }

        let until = deadline.unwrap_or_else(|| tokio::time::Instant::now() + idle);
        let mut chunk = [0u8; 4096];
        let n = match tokio::time::timeout_at(until, stream.read(&mut chunk)).await {
            // A silent client is dropped without a word, like nginx.
            // Measured: nginx 1.31.4 answers a `client_header_timeout`
            // with SILENCE, not with the 408 the RFC would suggest —
            // both for a socket that says nothing at all and for one
            // that sends a partial head. Volunteering a 408 here would
            // be as distinguishing as the hang this module just
            // removed, in the opposite direction.
            Err(_) => return Err(HeadError::Closed),
            Ok(Err(_)) => return Err(HeadError::Closed),
            Ok(Ok(0)) => {
                // Clean close. Nothing buffered means an ordinary idle
                // hang-up; a partial head means a truncated request,
                // which nginx also answers with silence.
                return Err(HeadError::Closed);
            }
            Ok(Ok(n)) => n,
        };
        // Whatever it was — request line, header bytes, or pure CRLF
        // debris — the request has started and the budget starts here.
        deadline.get_or_insert_with(|| tokio::time::Instant::now() + budget);
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// If the buffered bytes hold a complete request line that needs no
/// header block, take it; if they hold one that cannot be valid, refuse
/// it now.
///
/// `Ok(None)` means "keep reading" — either the line has not finished
/// arriving, or it is a 1.0/1.1 line whose headers are still to come.
fn request_line_settles_it(buf: &mut Vec<u8>) -> Result<Option<Vec<u8>>, HeadError> {
    let Some(nl) = buf.iter().position(|b| *b == b'\n') else {
        return Ok(None);
    };
    let complete_at_line = {
        let line = std::str::from_utf8(&buf[..nl])
            .map_err(|_| HeadError::Refuse(Class::BadRequest))?
            .trim_end_matches('\r');
        // Propagates the refusal for a line that can never parse.
        matches!(parse_request_line(line)?.version, Version::Http09)
    };
    if !complete_at_line {
        return Ok(None);
    }
    // The head IS the request line. Anything after it on the wire is
    // not ours to interpret: HTTP/0.9 has no headers and no keep-alive,
    // so this connection ends with the answer. (Verified against nginx:
    // `GET /nope\r\nHost: x\r\n\r\n` is answered as 0.9, headers
    // ignored.)
    let head = buf[..nl].to_vec();
    buf.drain(..=nl);
    Ok(Some(head))
}

/// The verdict on a request line that has **not finished arriving**.
///
/// [`request_line_settles_it`] answers a request line at its newline.
/// That is not early enough, and round 5 shipped believing it was: the
/// three cases in the differential named "unterminated" all ended in
/// `\r\n`, so they carried the very newline the parser keys on and
/// answered instantly. Strip it — which nginx does not require — and
/// each was a sixty-second silent hold, answered by nobody and seen by
/// nothing, since the head timeout returns [`HeadError::Closed`] and
/// never reaches `observe_protocol_error`.
///
/// nginx does not wait for a terminator to reject a line it can
/// already prove wrong, and it does not reject one it cannot. Measured
/// against nginx 1.31.4, with **no `\n` anywhere** in the input:
///
/// | prefix | nginx |
/// |---|---|
/// | `GET\r`, `GET\rZ` | 400 |
/// | `GET /nope HTTP/1.1\r` | *waits* — the CR of a line it accepted |
/// | `GET /nope HTTP/1.1\rX` | 400 |
/// | `GET /no\x01pe …`, `\t`, `\0`, `\x7f` | 400 |
/// | `GET x:44` | 400 — not origin-form, and not `scheme://` either |
/// | `GET http:/`, `GET http://x/p`, `GET x` | *waits* |
/// | `GET /no p` | 400 — a third field that cannot be a version |
/// | `GET /nope HTTP/1.1 j` | 400 — a fourth field |
/// | `GET /nope HTTP/1.1 ` | *waits* — an empty fourth field is a space |
/// | `GET /nope HTTP/9`, `/2`, `/10` | 505 — before the minor arrives |
/// | `GET /nope HTTP/1`, `/1.`, `/1.1` | *waits* |
/// | `GET /nope HTTP/0`, `/01`, `/x`, `/1x`, `HTTPX` | 400 |
///
/// Both halves matter. Refusing too late is the hang; refusing too
/// early is the same tell pointing the other way, because a host that
/// answers where nginx waits has also identified itself. Every "waits"
/// row above is a case this function must return `None` for, and they
/// are pinned in the differential's silence section.
fn request_line_prefix_verdict(buf: &[u8]) -> Option<Class> {
    // Only the first line is this function's business. If a newline
    // has arrived, the full parser has already had its say.
    let line = match buf.iter().position(|b| *b == b'\n') {
        Some(i) => &buf[..i],
        None => buf,
    };

    // A CR is legal in exactly one place: immediately before the LF
    // that ends a request line nginx has already accepted. That is why
    // `GET\r` is refused with nothing after it at all, while
    // `GET /nope HTTP/1.1\r` is waited for — nginx is not waiting for
    // the line there, it is waiting for the LF of a line it has.
    let Some(cr) = line.iter().position(|b| *b == b'\r') else {
        return fields_verdict(line);
    };
    // Whatever the bytes before the CR already prove, they prove
    // regardless of the CR — and "complete" is not "valid". A line
    // ending `HTTP/9.9` is COMPLETE and answered 505; asking only
    // whether it PARSES would call it malformed and answer 400.
    if let Some(class) = fields_verdict(&line[..cr]) {
        return Some(class);
    }
    let complete = std::str::from_utf8(&line[..cr])
        .ok()
        .is_some_and(|s| parse_request_line(s).is_ok());
    if !complete || cr + 1 < line.len() {
        return Some(Class::BadRequest);
    }
    None
}

/// The space-delimited half of [`request_line_prefix_verdict`], on a
/// run of bytes known to contain no CR and no LF.
fn fields_verdict(line: &[u8]) -> Option<Class> {
    let mut fields = line.split(|b| *b == b' ');
    // The method's charset is [`method_is_impossible`]'s job, and it
    // has already run.
    fields.next();
    let target = fields.next()?;
    if let Some(class) = target_prefix_verdict(target) {
        return Some(class);
    }
    let version = fields.next()?;
    if let Some(class) = version_prefix_verdict(version) {
        return Some(class);
    }
    // A fourth field cannot become part of a request line. An EMPTY
    // one is a trailing space, which nginx waits on.
    fields.any(|f| !f.is_empty()).then_some(Class::BadRequest)
}

/// Whether a partially-arrived target is already impossible.
fn target_prefix_verdict(target: &[u8]) -> Option<Class> {
    // A control character is refused at the byte, not at the newline.
    if target.iter().any(|b| *b < 0x20 || *b == 0x7f) {
        return Some(Class::BadRequest);
    }
    if target.is_empty() || target.first() == Some(&b'/') {
        return None;
    }
    // Not origin-form, so the only thing it can still become is
    // absolute-form. Measured: `GET x:44` is refused the moment the
    // byte after the colon is not a slash, while `GET http:/` — one
    // slash so far — is still waited for.
    let colon = target.iter().position(|b| *b == b':')?;
    target[colon + 1..]
        .iter()
        .take(2)
        .any(|b| *b != b'/')
        .then_some(Class::BadRequest)
}

/// Whether a partially-arrived version is already impossible — or
/// already known to be one this server does not speak.
fn version_prefix_verdict(v: &[u8]) -> Option<Class> {
    const LIT: &[u8] = b"HTTP/";
    let bad = Some(Class::BadRequest);
    if v.len() < LIT.len() {
        return if LIT.starts_with(v) { None } else { bad };
    }
    if !v.starts_with(LIT) {
        return bad;
    }
    let rest = &v[LIT.len()..];
    let &first = rest.first()?;
    if !first.is_ascii_digit() || first == b'0' {
        return bad;
    }
    let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
    // The major is decided the moment it cannot be 1, which is before
    // the minor has arrived at all: `HTTP/9` and `HTTP/10` are both
    // 505 on their own.
    if first > b'1' || digits > 1 {
        return Some(Class::VersionNotSupported);
    }
    match rest.get(digits) {
        // Still inside the major digit run.
        None => return None,
        Some(b'.') => {}
        Some(_) => return bad,
    }
    // Compared by VALUE, not by digit count: `HTTP/1.0000` is a
    // perfectly good HTTP/1.0 and nginx serves it, while `HTTP/1.1000`
    // is malformed. Digits only ever get appended, so a value that has
    // already passed 999 can never come back under it.
    let mut minor: u32 = 0;
    for b in &rest[digits + 1..] {
        if !b.is_ascii_digit() {
            return bad;
        }
        minor = (minor * 10 + u32::from(b - b'0')).min(1_000);
    }
    if minor > 999 {
        return bad;
    }
    None
}

/// `true` once the buffered bytes prove the request line cannot be
/// valid, whether or not it has finished arriving.
///
/// Looks only at the method token — the bytes before the first space —
/// because that is the part nginx can reject without ambiguity, and it
/// is enough to catch the cases that matter: binary protocol probes
/// (TLS, SSH, SMB) and pipelined junk.
fn method_is_impossible(buf: &[u8]) -> bool {
    let end = buf
        .iter()
        .position(|b| *b == b' ' || *b == b'\n' || *b == b'\r')
        .unwrap_or(buf.len());
    // A method token longer than any real one is also impossible, and
    // bounds how far this walks on a stream of valid token bytes.
    if end > 64 {
        return true;
    }
    buf[..end].iter().any(|b| !is_method_byte(*b))
}

/// Length of the buffered bytes up to the first newline, or the whole
/// buffer if there is none yet.
fn first_line_len(buf: &[u8]) -> usize {
    buf.iter().position(|b| *b == b'\n').unwrap_or(buf.len())
}

/// Split off a complete header block. Accepts CRLFCRLF and LFLF —
/// nginx tolerates bare-LF line endings and so do we.
fn split_head(buf: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let crlf = find(buf, b"\r\n\r\n").map(|i| (i, i + 4));
    let lf = find(buf, b"\n\n").map(|i| (i, i + 2));
    let (end, next) = match (crlf, lf) {
        (Some(a), Some(b)) => {
            if a.0 <= b.0 {
                a
            } else {
                b
            }
        }
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => return None,
    };
    Some((buf[..end].to_vec(), buf[next..].to_vec()))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// The three fields of a request line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RequestLine<'a> {
    method: &'a str,
    target: &'a str,
    version: Version,
}

/// Parse a request line on its own.
///
/// Split out of [`parse_head`] because [`read_head`] needs the same
/// verdict *before* a header block has arrived — that is what lets a
/// complete-but-unterminated request line be answered immediately
/// instead of waiting out `HEADER_TIMEOUT` in silence. One function so
/// the eager answer and the full parse can never disagree about what a
/// request line means.
fn parse_request_line(line: &str) -> Result<RequestLine<'_>, HeadError> {
    if line.len() > MAX_REQUEST_LINE {
        return Err(HeadError::Refuse(Class::UriTooLarge));
    }

    // METHOD SP TARGET [SP VERSION]. Split on single spaces, strictly:
    // an extra space inside the target is what makes `GET /no pe` a
    // 400 rather than a request for `/no`.
    let mut parts = line.split(' ');
    let method = parts
        .next()
        .filter(|m| !m.is_empty())
        .ok_or(HeadError::Refuse(Class::BadRequest))?;
    if !is_method(method) {
        return Err(HeadError::Refuse(Class::BadRequest));
    }
    let target = parts
        .next()
        .filter(|t| !t.is_empty())
        .ok_or(HeadError::Refuse(Class::BadRequest))?;
    // Which target forms a verb may carry. The asterisk-form is legal
    // per RFC for OPTIONS; nginx's static server rejects it, and
    // matching nginx is the whole job.
    //
    // The authority form (`host:port`) belongs to `CONNECT` and to
    // nothing else — measured, and measured in both directions:
    // `CONNECT x:443` is a `405` that closes, while `CONNECT /nope`,
    // `CONNECT x`, `CONNECT http://x/p`, `GET x:443` and `PUT x:443`
    // are all `400`. A single rule for "is this target shaped right"
    // got `CONNECT x:443` wrong, which is the form a proxy scan
    // actually sends.
    if method == "CONNECT" {
        let authority_form = target.contains(':') && !target.contains('/');
        if !authority_form {
            return Err(HeadError::Refuse(Class::BadRequest));
        }
    } else if !target.starts_with('/') && !target.contains("://") {
        return Err(HeadError::Refuse(Class::BadRequest));
    }
    // A control character in the target is a `400`, not a `404` for a
    // path that happens to contain one. Measured: TAB, NUL, a bare CR,
    // `0x01` and DEL each get nginx's 400, while a high byte (`0x80`)
    // is passed through to the ordinary 404. Each of these used to be a
    // status oracle — the one class of input where the target's
    // *content* changed the answer, in a mimic whose whole claim is
    // that it does not read the target.
    if target.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return Err(HeadError::Refuse(Class::BadRequest));
    }

    let version = match parts.next() {
        None => Version::Http09,
        Some(v) => parse_version(v)?,
    };
    // Trailing junk after the version.
    if parts.next().is_some() {
        return Err(HeadError::Refuse(Class::BadRequest));
    }

    Ok(RequestLine {
        method,
        target,
        version,
    })
}

/// nginx's request-line version grammar, measured rather than guessed.
///
/// The old rule here was "`HTTP/1.1` and `HTTP/1.0` are served,
/// anything else digit-shaped is `505`". That is not what nginx does,
/// and the difference is a status oracle on inputs a scanner sends:
///
/// | input | nginx | old behaviour |
/// |---|---|---|
/// | `HTTP/1.11`, `HTTP/1.2`, `HTTP/1.9`, `HTTP/1.01` | 404, served as 1.1 | 505 |
/// | `HTTP/1.00`, `HTTP/1.0000` | 404, served as 1.0 | 505 |
/// | `HTTP/0.9`, `HTTP/01.1` | 400 — a leading zero is malformed | 505 |
/// | `HTTP/1.1000` | 400 — the minor is capped at 999 | 505 |
/// | `HTTP/11.1`, `HTTP/1000.1`, `HTTP/2.0` | 505 | 505 |
///
/// So: the first major digit is `1`–`9`, both fields are digits, the
/// minor is at most 999, a major that is not `1` is a version this
/// server does not speak, and a minor of zero means HTTP/1.0
/// semantics — which is the part with teeth, because it decides
/// keep-alive and whether `Host` is mandatory.
fn parse_version(v: &str) -> Result<Version, HeadError> {
    let bad = HeadError::Refuse(Class::BadRequest);
    let rest = v.strip_prefix("HTTP/").ok_or(bad)?;
    let (major, minor) = rest.split_once('.').ok_or(bad)?;
    if !matches!(major.as_bytes().first(), Some(b'1'..=b'9'))
        || !major.bytes().all(|b| b.is_ascii_digit())
        || minor.is_empty()
        || !minor.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(bad);
    }
    // nginx accumulates the minor into a bounded field and calls
    // anything over 999 malformed. Compared as a number rather than by
    // digit count so `HTTP/1.0000` stays valid, which it is.
    let Ok(minor) = minor.parse::<u32>() else {
        return Err(bad);
    };
    if minor > 999 {
        return Err(bad);
    }
    // Well-formed but not ours. `HTTP/2.0` in the clear lands here,
    // which is exactly what real nginx does with it.
    if major != "1" {
        return Err(HeadError::Refuse(Class::VersionNotSupported));
    }
    Ok(if minor == 0 {
        Version::Http10
    } else {
        Version::Http11
    })
}

/// Whether a verb can appear in a request line at all.
///
/// **Not** RFC-9110 `token`, which is what this used to use. nginx's
/// request-line scanner accepts only `A`–`Z`, `_` and `-` in a method,
/// and answers anything else with `400` — measured: `get`, `Get`,
/// `GET2`, `GE~T` and `GE.T` are all `400`, while `GET_X`, `GE-T` and a
/// forty-character run of `A` are `405`. A lowercase verb answered
/// `405` here where nginx answers `400`, which is a one-request oracle
/// and free to close.
fn is_method(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(is_method_byte)
}

/// One byte that may appear in a method. Shared with
/// [`method_is_impossible`] so the eager reject and the full parse can
/// never disagree about what is legal.
fn is_method_byte(b: u8) -> bool {
    b.is_ascii_uppercase() || b == b'_' || b == b'-'
}

/// Verbs nginx refuses to keep a connection alive for.
///
/// Measured: `TRACE` and `CONNECT` are each a `405` with `Connection:
/// close` (295 bytes), where `PUT`, `DELETE`, `OPTIONS`, `PATCH` and an
/// unknown verb are a `405` with `Connection: keep-alive` (300 bytes).
fn method_forbids_reuse(method: &str) -> bool {
    matches!(method, "TRACE" | "CONNECT")
}

/// Turn a header block into a [`RequestHead`], or into the nginx class
/// that rejects it.
fn parse_head(raw: &[u8]) -> Result<RequestHead, HeadError> {
    // nginx accepts a request line in any byte encoding and only cares
    // about the ASCII structure, but anything non-UTF-8 here is
    // malformed for every purpose we have.
    let text = std::str::from_utf8(raw).map_err(|_| HeadError::Refuse(Class::BadRequest))?;
    let mut lines = text.split('\n');

    let line = lines
        .next()
        .ok_or(HeadError::Refuse(Class::BadRequest))?
        .trim_end_matches('\r');
    let RequestLine {
        method,
        target,
        version,
    } = parse_request_line(line)?;

    let mut headers: Vec<(String, String)> = Vec::new();
    let mut hosts = 0usize;
    for raw_line in lines {
        let l = raw_line.trim_end_matches('\r');
        if l.is_empty() {
            continue;
        }
        // Obsolete line folding. nginx rejects it; so do we, rather
        // than trying to reassemble something ambiguous.
        if l.starts_with(' ') || l.starts_with('\t') {
            return Err(HeadError::Refuse(Class::BadRequest));
        }
        let (name, value) = l
            .split_once(':')
            .ok_or(HeadError::Refuse(Class::BadRequest))?;
        if !is_token(name) {
            return Err(HeadError::Refuse(Class::BadRequest));
        }
        let name = name.to_ascii_lowercase();
        if name == "host" {
            hosts += 1;
        }
        headers.push((name, value.trim().to_string()));
    }

    // A duplicated `Host` is a request-smuggling primitive as much as a
    // malformed request; nginx answers 400 and so do we.
    if hosts > 1 {
        return Err(HeadError::Refuse(Class::BadRequest));
    }
    // Mandatory on HTTP/1.1, optional on HTTP/1.0 — the captured
    // behaviour, and the one most hand-written mimics get wrong.
    if version == Version::Http11 && hosts == 0 {
        return Err(HeadError::Refuse(Class::BadRequest));
    }

    // Body framing is agreed HERE, from the head alone, so that every
    // way of disagreeing about it is answered without reading a body.
    // See [`framing_of`] — this is what stops `Transfer-Encoding:
    // xchunked` reaching the chunked decoder and parking there.
    framing_of(version, &headers).map_err(HeadError::Refuse)?;

    let connection = headers
        .iter()
        .find(|(k, _)| k == "connection")
        .map(|(_, v)| v.to_ascii_lowercase());
    let client_wants_keep_alive = match version {
        Version::Http11 => !connection.as_deref().is_some_and(|c| c.contains("close")),
        Version::Http10 => connection
            .as_deref()
            .is_some_and(|c| c.contains("keep-alive")),
        Version::Http09 => false,
    };

    Ok(RequestHead {
        method: method.to_string(),
        target: target.to_string(),
        version,
        headers,
        client_wants_keep_alive,
    })
}

/// RFC-9110 `token`.
fn is_token(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(is_token_byte)
}

/// One `tchar`. Shared with [`method_is_impossible`] so the eager
/// reject and the full parse can never disagree about what is legal.
fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

/// Read the request body, if any.
///
/// Reached only from the one path whose ANSWER needs the bytes — see
/// [`Handler::needs_body`]. Everything else discards its body through
/// [`Discard`] instead, *after* answering, because waiting for a body
/// nobody is going to read was a 60-second silent hold available to
/// anyone who could write one header.
async fn read_body<S>(
    stream: &mut S,
    buf: &mut Vec<u8>,
    head: &RequestHead,
    max_body: usize,
    budget: Duration,
) -> Result<Vec<u8>, BodyError>
where
    S: AsyncRead + Unpin,
{
    // One total budget for the whole body, armed here. Every read below
    // shares it, so no amount of dripping extends it.
    let deadline = tokio::time::Instant::now() + budget;

    // Already validated by `parse_head`; re-resolved rather than
    // threaded through `RequestHead`, whose fields are public and whose
    // shape is not this module's to change.
    let framing = framing_of(head.version, &head.headers).map_err(BodyError::Refuse)?;

    let len = match framing {
        Framing::None => return Ok(Vec::new()),
        Framing::Chunked => return read_chunked(stream, buf, max_body, deadline).await,
        Framing::Length(len) => len,
    };
    // Refused before a byte is read, from the declared length alone.
    // nginx answers 413 here; we answer 400 — the named status-code
    // residual, see [`framing_of`].
    if len > max_body as u64 {
        return Err(BodyError::Refuse(Class::BadRequest));
    }
    let len = usize::try_from(len).map_err(|_| BodyError::Refuse(Class::BadRequest))?;

    while buf.len() < len {
        let mut chunk = [0u8; 4096];
        let n = match tokio::time::timeout_at(deadline, stream.read(&mut chunk)).await {
            Ok(Ok(n)) if n > 0 => n,
            // Truncated, or out of budget. Well-framed either way, so
            // this is nginx's undrainable-body answer, not a 400.
            _ => return Err(BodyError::Undrainable),
        };
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = buf[..len].to_vec();
    buf.drain(..len);
    Ok(body)
}

/// Decode a chunked body, bounded by [`MAX_REQUEST_BODY`].
///
/// Implemented rather than refused because a parked nginx accepts
/// chunked bodies and answers the ordinary keep-alive 404 — a server
/// that closed instead would be distinguishable with one request.
async fn read_chunked<S>(
    stream: &mut S,
    buf: &mut Vec<u8>,
    max_body: usize,
    deadline: tokio::time::Instant,
) -> Result<Vec<u8>, BodyError>
where
    S: AsyncRead + Unpin,
{
    let mut body = Vec::new();
    loop {
        let line = read_line(stream, buf, deadline).await?;
        // A chunk size that is not hex is answered NOW, from the bytes
        // already in hand. Measured against nginx 1.31.4: a bogus chunk
        // size gets the ordinary parked page with `Connection: close`,
        // immediately — nginx never blocks on a body it cannot drain.
        // Shared with [`Discard::pump`] so the reading decoder and the
        // discarding one can never disagree about what a chunk size is.
        let Some(size) = chunk_size(&line) else {
            return Err(BodyError::Undrainable);
        };
        if body.len().saturating_add(size) > max_body {
            return Err(BodyError::Refuse(Class::BadRequest));
        }
        if size == 0 {
            // Trailers, then the final blank line — bounded, because an
            // endless run of `X: y\r\n` was an unbounded loop with no
            // exit but the peer's good manners, and the peer here is by
            // assumption hostile. The portal wants no trailers at all;
            // this is the budget for refusing them.
            let mut spent = 0usize;
            loop {
                let t = read_line(stream, buf, deadline).await?;
                if t.is_empty() {
                    break;
                }
                spent = spent.saturating_add(t.len() + 2);
                if spent > MAX_TRAILERS {
                    return Err(BodyError::Undrainable);
                }
            }
            return Ok(body);
        }
        // The chunk plus its trailing CRLF.
        while buf.len() < size + 2 {
            let mut chunk = [0u8; 4096];
            let n = match tokio::time::timeout_at(deadline, stream.read(&mut chunk)).await {
                Ok(Ok(n)) if n > 0 => n,
                _ => return Err(BodyError::Undrainable),
            };
            buf.extend_from_slice(&chunk[..n]);
        }
        // Those two bytes were *assumed* to be CRLF and drained
        // unexamined. If they are not, the sender and this parser
        // disagree about where the chunk ended — which is the exact
        // shape of a desynchronised socket, and desynchronising where
        // nginx does not is the failure this module exists to prevent.
        // Checked rather than assumed, and refused rather than guessed.
        if &buf[size..size + 2] != b"\r\n" {
            return Err(BodyError::Undrainable);
        }
        body.extend_from_slice(&buf[..size]);
        buf.drain(..size + 2);
    }
}

/// One CRLF- or LF-terminated line from the buffered stream, inside the
/// body's total budget.
async fn read_line<S>(
    stream: &mut S,
    buf: &mut Vec<u8>,
    deadline: tokio::time::Instant,
) -> Result<String, BodyError>
where
    S: AsyncRead + Unpin,
{
    loop {
        if let Some(i) = buf.iter().position(|b| *b == b'\n') {
            let line = String::from_utf8_lossy(&buf[..i])
                .trim_end_matches('\r')
                .to_string();
            buf.drain(..=i);
            return Ok(line);
        }
        if buf.len() > MAX_REQUEST_LINE {
            return Err(BodyError::Refuse(Class::BadRequest));
        }
        let mut chunk = [0u8; 4096];
        let n = match tokio::time::timeout_at(deadline, stream.read(&mut chunk)).await {
            Ok(Ok(n)) if n > 0 => n,
            _ => return Err(BodyError::Undrainable),
        };
        buf.extend_from_slice(&chunk[..n]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<RequestHead, HeadError> {
        // The caller passes the head without its terminating blank line,
        // exactly as `read_head` yields it.
        parse_head(s.as_bytes())
    }

    #[test]
    fn an_ordinary_request_parses() {
        let h = parse("GET /nope?a=1 HTTP/1.1\r\nHost: x\r\nUser-Agent: curl").expect("parses");
        assert_eq!(h.method, "GET");
        assert_eq!(h.path(), "/nope");
        assert_eq!(h.query(), Some("a=1"));
        assert_eq!(h.version, Version::Http11);
        assert_eq!(h.header("user-agent"), Some("curl"));
        assert!(h.client_wants_keep_alive, "1.1 defaults to keep-alive");
    }

    #[test]
    fn the_captured_400_class_is_reproduced_for_every_captured_input() {
        // Each of these produced exactly `400 Bad Request` from a real
        // nginx — see tests/fixtures/nginx-goldens.txt.
        for bad in [
            "NOT A VALID REQUEST LINE",
            "GET /nope HTTP/1.1",                       // HTTP/1.1 with no Host
            "GET /nope HTTP/1.1\r\nHost: x\r\nHost: y", // duplicate Host
            "GET /no pe HTTP/1.1\r\nHost: x",           // space in the target
            "GET /nope HTTP/1.1\r\nHost: x\r\nBad Header Name: v",
            "OPTIONS * HTTP/1.1\r\nHost: x",
            "GET /nope HTTP/x\r\nHost: x", // malformed version
            "GET /nope HTTP/1.1 extra\r\nHost: x",
        ] {
            assert_eq!(
                parse(bad),
                Err(HeadError::Refuse(Class::BadRequest)),
                "input: {bad:?}"
            );
        }
    }

    #[test]
    fn a_well_formed_unsupported_version_is_505_not_400() {
        // The distinction a hand-written mimic misses: `HTTP/9.9` and
        // `HTTP/2.0` are 505, `HTTP/x` is 400.
        for v in ["HTTP/9.9", "HTTP/2.0", "HTTP/3.0"] {
            assert_eq!(
                parse(&format!("GET /nope {v}\r\nHost: x")),
                Err(HeadError::Refuse(Class::VersionNotSupported)),
                "{v}"
            );
        }
    }

    #[test]
    fn http_1_0_without_a_host_is_fine_and_closes_by_default() {
        // Captured: a 404, NOT the 400 that HTTP/1.1 gets. Host is only
        // mandatory on 1.1.
        let h = parse("GET /nope HTTP/1.0").expect("1.0 needs no Host");
        assert_eq!(h.version, Version::Http10);
        assert!(!h.client_wants_keep_alive);
    }

    #[test]
    fn connection_intent_is_read_case_insensitively_in_both_directions() {
        let h = parse("GET / HTTP/1.0\r\nHost: x\r\nConnection: KEEP-ALIVE").expect("parses");
        assert!(h.client_wants_keep_alive, "1.0 opts in");
        let h = parse("GET / HTTP/1.1\r\nHost: x\r\nConnection: CLOSE").expect("parses");
        assert!(!h.client_wants_keep_alive, "1.1 opts out");
    }

    #[test]
    fn a_missing_version_is_http_0_9() {
        let h = parse("GET /nope").expect("0.9 parses");
        assert_eq!(h.version, Version::Http09);
        assert!(!h.client_wants_keep_alive);
    }

    #[test]
    fn an_absolute_form_target_yields_its_path() {
        // Captured as a 404, not a 400.
        let h = parse("GET http://x/nope HTTP/1.1\r\nHost: x").expect("parses");
        assert_eq!(h.path(), "/nope");
        let h = parse("GET http://x HTTP/1.1\r\nHost: x").expect("parses");
        assert_eq!(h.path(), "/", "an authority with no path is /");
    }

    #[test]
    fn an_over_long_request_line_is_414() {
        let long = format!("GET /{} HTTP/1.1\r\nHost: x", "a".repeat(MAX_REQUEST_LINE));
        assert_eq!(parse(&long), Err(HeadError::Refuse(Class::UriTooLarge)));
    }

    #[test]
    fn bare_lf_line_endings_are_accepted() {
        let h = parse("GET /nope HTTP/1.1\nHost: x\nConnection: close").expect("parses");
        assert_eq!(h.path(), "/nope");
        assert!(!h.client_wants_keep_alive);
    }

    #[test]
    fn the_path_is_never_decoded_or_normalised() {
        // No decoded-versus-compared gap: what arrived is what the
        // knock comparison sees.
        let h = parse("GET /ab%2Fc/../x HTTP/1.1\r\nHost: h").expect("parses");
        assert_eq!(h.path(), "/ab%2Fc/../x");
    }

    #[test]
    fn a_binary_probe_is_rejected_before_any_terminator_arrives() {
        // A TLS ClientHello at a cleartext port never sends a line
        // terminator. nginx answers 400 anyway; so must we.
        assert!(method_is_impossible(b"\x16\x03\x01\x00\x50"));
        assert!(method_is_impossible(b"\x00\x00\x00"));
        // …and a partial but legal method must NOT be rejected early.
        assert!(!method_is_impossible(b"GE"));
        assert!(!method_is_impossible(b"GET"));
        assert!(!method_is_impossible(b"GET /nope HTTP/1.1"));
        assert!(!method_is_impossible(b""));
        // An absurdly long token is impossible too, and bounds the walk.
        assert!(method_is_impossible(&[b'A'; 65]));
    }

    #[test]
    fn a_head_block_splits_on_either_terminator() {
        let (head, rest) = split_head(b"GET / HTTP/1.1\r\nHost: x\r\n\r\nBODY").expect("split");
        assert_eq!(head, b"GET / HTTP/1.1\r\nHost: x");
        assert_eq!(rest, b"BODY");
        let (head, rest) = split_head(b"GET / HTTP/1.1\nHost: x\n\nBODY").expect("split");
        assert_eq!(head, b"GET / HTTP/1.1\nHost: x");
        assert_eq!(rest, b"BODY");
        assert!(split_head(b"GET / HTTP/1.1\r\nHost: x\r\n").is_none());
    }

    #[test]
    fn a_portal_response_carries_the_security_headers_the_404_must_not() {
        let p = PortalAnswer {
            status: 200,
            reason: "OK",
            content_type: "text/html; charset=utf-8".into(),
            body: b"<html></html>".to_vec(),
            set_cookie: Some("s=1; HttpOnly".into()),
        };
        let s = String::from_utf8(render_portal(&p, "D", true, true)).expect("utf8");
        for want in [
            "Content-Security-Policy:",
            "frame-ancestors 'none'",
            "Referrer-Policy: no-referrer",
            "X-Content-Type-Options: nosniff",
            "X-Frame-Options: DENY",
            "Strict-Transport-Security:",
            "Cache-Control: no-store",
            "Set-Cookie: s=1; HttpOnly",
        ] {
            assert!(s.contains(want), "the shell is missing `{want}`");
        }
    }

    #[test]
    fn a_header_value_cannot_split_the_response() {
        // A compromised — or merely buggy — Mac must not be able to
        // inject a header, or a whole second response, into the
        // browser's view.
        //
        // The property is that the CR/LF is REMOVED, so the injected
        // text degrades into inert characters inside the value it came
        // from. It is deliberately not that the text disappears:
        // silently rewriting a value would hide the bug, while a
        // mangled `Content-Type` is visible and harmless.
        let clean = PortalAnswer {
            status: 200,
            reason: "OK",
            content_type: "text/html".into(),
            body: Vec::new(),
            set_cookie: Some("s=1".into()),
        };
        let hostile = PortalAnswer {
            status: 200,
            reason: "OK",
            content_type: "text/html\r\nX-Injected: yes".into(),
            body: Vec::new(),
            set_cookie: Some("s=1\r\nX-Also-Injected: yes".into()),
        };
        let clean = String::from_utf8(render_portal(&clean, "D", false, true)).expect("utf8");
        let hostile = String::from_utf8(render_portal(&hostile, "D", false, true)).expect("utf8");

        // Same number of header lines: nothing was injected.
        let lines = |s: &str| s.split("\r\n\r\n").next().unwrap_or("").lines().count();
        assert_eq!(lines(&hostile), lines(&clean), "a header line was injected");
        // And no second response was smuggled into the body.
        assert_eq!(
            hostile.matches("HTTP/1.1").count(),
            1,
            "a whole response was injected"
        );
        for line in hostile.split("\r\n") {
            assert!(
                !line.starts_with("X-Injected") && !line.starts_with("X-Also-Injected"),
                "`{line}` became a header of its own"
            );
        }
    }

    #[test]
    fn a_head_request_keeps_the_length_and_drops_the_body() {
        let head = parse("HEAD /nope HTTP/1.1\r\nHost: x\r\nConnection: close").expect("parses");
        let (bytes, keep) = render(&Answer::not_found(), &head, true);
        assert!(!keep);
        let s = String::from_utf8(bytes).expect("utf8");
        assert!(s.contains("Content-Length: 146"));
        assert!(s.ends_with("\r\n\r\n"), "a HEAD carries no body");
    }

    #[test]
    fn a_clean_404_stays_open_when_the_client_wants_it_to() {
        // The two-requests-down-one-socket tell.
        let head = parse("GET /nope HTTP/1.1\r\nHost: x").expect("parses");
        let (_, keep) = render(&Answer::not_found(), &head, true);
        assert!(keep, "always-closing is a fingerprint");
    }

    #[tokio::test]
    async fn a_chunked_body_is_decoded_and_leaves_the_stream_aligned() {
        let head =
            parse("POST /x HTTP/1.1\r\nHost: h\r\nTransfer-Encoding: chunked").expect("parses");
        let mut buf = b"3\r\nabc\r\n2\r\nde\r\n0\r\n\r\nNEXT".to_vec();
        let mut empty: &[u8] = &[];
        let body = read_body(&mut empty, &mut buf, &head, MAX_REQUEST_BODY, BODY_TIMEOUT)
            .await
            .expect("body");
        assert_eq!(body, b"abcde");
        assert_eq!(buf, b"NEXT", "the next request must still be readable");
    }

    #[tokio::test]
    async fn an_oversized_content_length_is_refused_before_it_is_read() {
        let head = parse(&format!(
            "POST /x HTTP/1.1\r\nHost: h\r\nContent-Length: {}",
            MAX_REQUEST_BODY + 1
        ))
        .expect("parses");
        let mut buf = Vec::new();
        let mut empty: &[u8] = &[];
        assert_eq!(
            read_body(&mut empty, &mut buf, &head, MAX_REQUEST_BODY, BODY_TIMEOUT).await,
            Err(BodyError::Refuse(Class::BadRequest))
        );
    }

    // ───────────────────────────────────────────────────────────────
    // The three hang primitives, and the family the first belongs to.
    //
    // Each of these used to buy an attacker a ~60-second SILENT hold on
    // a connection for the price of one short line. Three things were
    // wrong with that at once: real nginx answers every one of them in
    // under a millisecond, so the hang de-anonymised the host; the hang
    // returned *before* `observe_protocol_error`, so the canary never
    // saw the probe that found it; and the slot was held for free.
    //
    // The assertion in every case is the same and is deliberately
    // crude: **an answer came back, and it came back fast**. The
    // `BOUNDED` budget is two orders of magnitude under the 60 s these
    // inputs used to take and three above what they now take, so it
    // cannot pass by accident in either direction.
    // ───────────────────────────────────────────────────────────────

    /// Generous next to the microseconds these now take, and far under
    /// the `HEADER_TIMEOUT`/`BODY_TIMEOUT` they used to take.
    const BOUNDED: Duration = Duration::from_secs(5);

    #[derive(Default)]
    struct Stub {
        protocol_errors: std::sync::Mutex<Vec<Class>>,
    }

    impl Handler for Stub {
        fn handle<'a>(
            &'a self,
            _head: &'a RequestHead,
            _body: &'a [u8],
            _peer: Option<SocketAddr>,
        ) -> Pin<Box<dyn Future<Output = Answer> + Send + 'a>> {
            Box::pin(async { Answer::not_found() })
        }

        /// The parked front's answer: nothing on this listener needs a
        /// body except the post-knock forward, which a stub has no way
        /// to be. Matching `Front` here is the point — a stub that
        /// answered `true` would test a code path no prober can reach.
        fn needs_body(&self, _head: &RequestHead) -> bool {
            false
        }

        fn observe_protocol_error(&self, class: Class, _: Option<SocketAddr>, _: Option<&str>) {
            self.protocol_errors
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(class);
        }
    }

    /// Write `raw` to a real `serve`, **leave the write half open**,
    /// and return what came back — failing if it did not come back
    /// inside [`BOUNDED`].
    ///
    /// Leaving the socket open is the whole point: every one of these
    /// inputs is answered instantly by nginx *without* the client
    /// closing, and a test that shut the write half would let a parser
    /// that only answers on EOF pass.
    async fn bounded_exchange(raw: &[u8]) -> (Vec<u8>, Vec<Class>) {
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let handler = Arc::new(Stub::default());
        let task = tokio::spawn(serve(server, None, Arc::clone(&handler)));
        client.write_all(raw).await.expect("write");

        let out = tokio::time::timeout(BOUNDED, async {
            // `serve` answers, then drops its half of the socket.
            task.await.expect("the connection task panicked");
            let mut out = Vec::new();
            client.read_to_end(&mut out).await.expect("read");
            out
        })
        .await
        .expect("the parser HELD THE CONNECTION instead of answering");

        let seen = handler
            .protocol_errors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        (out, seen)
    }

    #[tokio::test]
    async fn hang_1_a_real_http_0_9_request_is_answered_without_a_blank_line() {
        // `GET /nope\r\n` is what an HTTP/0.9 client actually sends: no
        // version, no headers, and NO blank line, because 0.9 has no
        // header block to terminate. `read_head` waited for one anyway.
        //
        // The existing differential case sent `GET /nope\r\n\r\n`, which
        // has the terminator and therefore never touched this path —
        // the test and the bug passed each other in the dark.
        let (out, _) = bounded_exchange(b"GET /nope\r\n").await;
        assert_eq!(out.len(), 146, "0.9 gets the bare body and nothing else");
        assert!(
            !out.starts_with(b"HTTP/"),
            "0.9 has no status line: {:?}",
            String::from_utf8_lossy(&out[..out.len().min(32)])
        );
    }

    #[tokio::test]
    async fn hang_2_transfer_encoding_that_merely_contains_chunked_is_refused_from_the_head() {
        // `contains("chunked")` matched `xchunked`, which routed a
        // Content-Length body into the chunked decoder, which then
        // blocked reading `abc` as a chunk size. One header, 60 seconds.
        let (out, seen) = bounded_exchange(
            b"POST /n HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: xchunked\r\n\r\nabc",
        )
        .await;
        assert!(
            out.starts_with(b"HTTP/1.1 400 Bad Request"),
            "got {:?}",
            String::from_utf8_lossy(&out[..out.len().min(48)])
        );
        assert_eq!(
            seen,
            vec![Class::BadRequest],
            "the canary must see the probe that used to hang the parser"
        );
    }

    #[tokio::test]
    async fn hang_3_an_unterminated_trailer_section_cannot_hold_the_connection() {
        // `0\r\n` opens the trailer section; the loop then read lines
        // forever with no budget. A few bytes per iteration, no exit.
        let mut raw =
            b"POST /n HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n".to_vec();
        for _ in 0..2_000 {
            raw.extend_from_slice(b"X: y\r\n");
        }
        let (out, seen) = bounded_exchange(&raw).await;
        assert!(!out.is_empty(), "the trailer loop answered with silence");
        assert!(
            out.starts_with(b"HTTP/1.1 404 Not Found"),
            "an undrainable body gets nginx's parked page, got {:?}",
            String::from_utf8_lossy(&out[..out.len().min(48)])
        );
        assert!(
            out.windows(18).any(|w| w == b"Connection: close\r"),
            "and closes, as nginx does when it cannot drain a body"
        );
        assert_eq!(seen, vec![Class::NotFound], "the canary sees it too");
    }

    #[tokio::test]
    async fn every_unterminated_request_line_is_answered_rather_than_awaited() {
        // HTTP/0.9 was one member of a family. A request line is
        // complete at its newline for every version but 1.0 and 1.1,
        // and a line that can never parse is complete the moment it
        // ends — nginx answers all of these in well under a
        // millisecond, none of them wait for a blank line.
        for (raw, want) in [
            (&b"NOT A VALID REQUEST LINE\r\n"[..], "HTTP/1.1 400"),
            (b"GET /no pe HTTP/1.1\r\n", "HTTP/1.1 400"),
            (b"GET /nope HTTP/x\r\n", "HTTP/1.1 400"),
            (b"OPTIONS * HTTP/1.1\r\n", "HTTP/1.1 400"),
            (b"GET /nope HTTP/9.9\r\n", "HTTP/1.1 505"),
            (b"GET /nope HTTP/2.0\r\n", "HTTP/1.1 505"),
        ] {
            let (out, seen) = bounded_exchange(raw).await;
            let got = String::from_utf8_lossy(&out[..out.len().min(want.len())]).to_string();
            assert_eq!(got, want, "input {:?}", String::from_utf8_lossy(raw));
            assert_eq!(seen.len(), 1, "and each one reaches the canary");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn hang_6_a_doomed_request_line_with_no_newline_is_not_awaited() {
        // Round 5 checked only the METHOD before a newline arrived, so
        // any request line that could never parse but carried no `\n`
        // was held for the full HEADER_TIMEOUT and then dropped in
        // SILENCE — never answered, and never seen by the canary,
        // because the timeout path returns `Closed`. `GET\rZ` is five
        // bytes. nginx answers every one of these in ~0.1 ms.
        //
        // The differential's three "unterminated" cases all ended in
        // `\r\n`, which is the newline the parser keys on — the test
        // and the bug passed each other in the dark for the second
        // time.
        for (name, raw, want) in [
            (
                "garbage line",
                &b"NOT A VALID REQUEST LINE"[..],
                &b"HTTP/1.1 400"[..],
            ),
            ("bare CR after the method", b"GET\rZ", b"HTTP/1.1 400"),
            ("bare CR, nothing after it", b"GET\r", b"HTTP/1.1 400"),
            (
                "bare CR after a complete line",
                b"GET /nope HTTP/1.1\rX",
                b"HTTP/1.1 400",
            ),
            (
                "NUL in the target",
                b"GET /no\0pe HTTP/1.1",
                b"HTTP/1.1 400",
            ),
            (
                "tab in the target",
                b"GET /no\tpe HTTP/1.1",
                b"HTTP/1.1 400",
            ),
            (
                "DEL in the target",
                b"GET /no\x7fpe HTTP/1.1",
                b"HTTP/1.1 400",
            ),
            (
                "space in the target",
                b"GET /no pe HTTP/1.1",
                b"HTTP/1.1 400",
            ),
            (
                "a fourth field",
                b"GET /nope HTTP/1.1 junk",
                b"HTTP/1.1 400",
            ),
            ("authority form on GET", b"GET x:44", b"HTTP/1.1 400"),
            ("malformed version", b"GET /nope HTTP/x", b"HTTP/1.1 400"),
            ("zero major", b"GET /nope HTTP/0", b"HTTP/1.1 400"),
            ("leading zero major", b"GET /nope HTTP/01", b"HTTP/1.1 400"),
            ("not HTTP at all", b"GET /nope HTTPX", b"HTTP/1.1 400"),
            // Decided before the minor has arrived at all.
            ("unsupported major", b"GET /nope HTTP/9", b"HTTP/1.1 505"),
            ("two-digit major", b"GET /nope HTTP/10", b"HTTP/1.1 505"),
            (
                "HTTP/2 in the clear",
                b"GET /nope HTTP/2.0",
                b"HTTP/1.1 505",
            ),
        ] {
            let (out, waited) = timed_answer(raw).await;
            assert!(!out.is_empty(), "{name}: answered with silence");
            assert!(
                waited < Duration::from_secs(1),
                "{name}: took {waited:?}; nginx answers it in ~0.1 ms"
            );
            assert!(
                out.starts_with(want),
                "{name}: got {:?}",
                String::from_utf8_lossy(&out[..out.len().min(48)])
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_line_that_could_still_become_valid_is_not_refused_early() {
        // The other half, and it is not optional: a host that ANSWERS
        // where nginx waits has identified itself just as surely as one
        // that waits where nginx answers. Every one of these is a
        // prefix nginx stays silent on — measured — so refusing them
        // eagerly would trade the sixth hang for a sixth tell.
        for (name, raw) in [
            (
                "a valid line, no terminator yet",
                &b"GET /nope HTTP/1.1"[..],
            ),
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
            let (mut client, server) = tokio::io::duplex(4096);
            let task = tokio::spawn(serve(server, None, Arc::new(Stub::default())));
            client.write_all(raw).await.expect("write");
            let mut byte = [0u8; 1];
            let spoke = tokio::time::timeout(Duration::from_secs(1), client.read(&mut byte)).await;
            assert!(
                spoke.is_err(),
                "{name}: we volunteered {byte:?} where nginx stays silent"
            );
            task.abort();
        }
    }

    #[tokio::test]
    async fn a_partial_head_on_a_supported_version_is_still_awaited_in_silence() {
        // The other half of the property, and the reason the fix is
        // "decide at the request line" rather than "never wait": an
        // HTTP/1.1 request line is NOT complete, its headers are still
        // to come, and nginx waits — then says nothing at all. Measured
        // against nginx 1.31.4, which answers `client_header_timeout`
        // with silence, not with a 408.
        let (mut client, server) = tokio::io::duplex(4096);
        let task = tokio::spawn(serve(server, None, Arc::new(Stub::default())));
        client
            .write_all(b"GET /nope HTTP/1.1\r\nHost: x\r\n")
            .await
            .expect("write");
        let mut byte = [0u8; 1];
        let spoke = tokio::time::timeout(Duration::from_millis(300), client.read(&mut byte)).await;
        assert!(
            spoke.is_err(),
            "the host volunteered {byte:?} where nginx stays silent"
        );
        task.abort();
    }

    #[tokio::test]
    async fn the_framing_headers_nginx_refuses_are_refused_from_the_head() {
        for (name, raw) in [
            (
                "duplicate Content-Length",
                &b"POST /n HTTP/1.1\r\nHost: x\r\nContent-Length: 3\r\nContent-Length: 3\r\n\r\nabc"[..],
            ),
            (
                "Content-Length and Transfer-Encoding together",
                b"POST /n HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\nContent-Length: 3\r\n\r\n0\r\n\r\n",
            ),
            (
                "chunked on HTTP/1.0",
                b"POST /n HTTP/1.0\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
            ),
            (
                "Content-Length that is not a number",
                b"POST /n HTTP/1.1\r\nHost: x\r\nContent-Length: abc\r\n\r\n",
            ),
            (
                "negative Content-Length",
                b"POST /n HTTP/1.1\r\nHost: x\r\nContent-Length: -1\r\n\r\n",
            ),
        ] {
            let (out, _) = bounded_exchange(raw).await;
            assert!(
                out.starts_with(b"HTTP/1.1 400 Bad Request"),
                "{name}: got {:?}",
                String::from_utf8_lossy(&out[..out.len().min(48)])
            );
        }
    }

    #[tokio::test]
    async fn a_chunk_not_terminated_by_crlf_is_refused_rather_than_assumed() {
        // The two bytes after a chunk were drained unexamined. If they
        // are not CRLF, the sender and this parser disagree about where
        // the chunk ended — a desynchronised socket, which is the one
        // outcome this module exists to prevent.
        let (out, _) = bounded_exchange(
            b"POST /n HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabcZZ0\r\n\r\n",
        )
        .await;
        assert!(
            out.starts_with(b"HTTP/1.1 404 Not Found"),
            "got {:?}",
            String::from_utf8_lossy(&out[..out.len().min(48)])
        );
        assert!(out.windows(18).any(|w| w == b"Connection: close\r"));
    }

    #[tokio::test]
    async fn a_bogus_chunk_size_is_answered_the_way_nginx_answers_it() {
        // Measured: nginx 1.31.4 answers the ordinary parked page and
        // closes — it does not treat an undrainable body as a protocol
        // error, and it does not wait for one either.
        let (out, _) = bounded_exchange(
            b"POST /n HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\nZZZZ\r\n",
        )
        .await;
        assert!(
            out.starts_with(b"HTTP/1.1 404 Not Found"),
            "got {:?}",
            String::from_utf8_lossy(&out[..out.len().min(48)])
        );
    }

    #[tokio::test]
    async fn a_well_formed_chunked_body_still_leaves_the_socket_aligned() {
        // The regression guard for all of the above: the legitimate
        // case must still decode, still keep the connection alive, and
        // still leave the next pipelined request readable.
        let (out, _) = bounded_exchange(
            b"POST /nope HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n3 ;pad=1\r\nabc\r\n0\r\n\r\nGET /nope HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert_eq!(
            out.windows(8).filter(|w| *w == b"HTTP/1.1").count(),
            2,
            "the second, pipelined request was not answered — the socket desynchronised"
        );
    }

    // ───────────────────────────────────────────────────────────────
    // The FIFTH hang primitive: a body DECLARED and WITHHELD.
    //
    // The body-side twin of the head-side family above, and it survived
    // round 3 for a precise reason: every body-bearing case in the
    // differential sent a COMPLETE body, so nothing ever asked what
    // happens when the head promises bytes that never arrive. `serve`
    // ran `read_body` to completion before `handle` was allowed to
    // decide anything, so one header bought a 60-second silent hold —
    // and one of `MAX_CONNECTIONS` with it.
    //
    // These tests run on a PAUSED clock. That is what makes them
    // regression tests rather than slow ones: a wait the fix removed
    // costs sixty *virtual* seconds and no real ones, so reverting the
    // fix fails them in milliseconds.
    // ───────────────────────────────────────────────────────────────

    /// Write `raw`, leave the write half open, and return the first
    /// answer with the time it took on the runtime's clock.
    async fn timed_answer(raw: &[u8]) -> (Vec<u8>, Duration) {
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let task = tokio::spawn(serve(server, None, Arc::new(Stub::default())));
        client.write_all(raw).await.expect("write");
        let started = tokio::time::Instant::now();
        let mut got = [0u8; 1024];
        let n = tokio::time::timeout(BOUNDED, client.read(&mut got))
            .await
            .expect("the parser HELD THE CONNECTION instead of answering")
            .expect("read");
        let waited = started.elapsed();
        task.abort();
        (got[..n].to_vec(), waited)
    }

    #[tokio::test(start_paused = true)]
    async fn hang_5_a_declared_but_withheld_body_is_answered_from_the_head() {
        for (name, raw, want) in [
            (
                "Content-Length, no body at all",
                &b"POST /nope HTTP/1.1\r\nHost: x\r\nContent-Length: 10\r\n\r\n"[..],
                &b"HTTP/1.1 404"[..],
            ),
            (
                "HEAD with a withheld body",
                b"HEAD /nope HTTP/1.1\r\nHost: x\r\nContent-Length: 10\r\n\r\n",
                b"HTTP/1.1 404",
            ),
            (
                "a partial Content-Length body",
                b"POST /nope HTTP/1.1\r\nHost: x\r\nContent-Length: 10\r\n\r\nab",
                b"HTTP/1.1 404",
            ),
            (
                "an unserved verb with a withheld body",
                b"FROBNICATE /nope HTTP/1.1\r\nHost: x\r\nContent-Length: 10\r\n\r\n",
                b"HTTP/1.1 405",
            ),
            (
                "chunked, withheld chunk size",
                b"POST /nope HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n",
                b"HTTP/1.1 404",
            ),
            (
                "chunked, withheld chunk data",
                b"POST /nope HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nab",
                b"HTTP/1.1 404",
            ),
            (
                "chunked, withheld trailer terminator",
                b"POST /nope HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n",
                b"HTTP/1.1 404",
            ),
        ] {
            let (out, waited) = timed_answer(raw).await;
            assert!(!out.is_empty(), "{name}: the front answered with silence");
            assert!(
                waited < Duration::from_secs(1),
                "{name}: the answer took {waited:?}. Real nginx answers this in under a \
                 millisecond; a silent hold where every other server on the internet \
                 answers is the de-anonymising tell ADR-0115 §2 exists to remove."
            );
            assert!(
                out.starts_with(want),
                "{name}: got {:?}",
                String::from_utf8_lossy(&out[..out.len().min(48)])
            );
            // And it is the ORDINARY answer. Measured against nginx
            // 1.31.4: a withheld body gets the same keep-alive page as
            // any other request of its class — 294 bytes, not the 289
            // of the close form. Closing here would be a different tell
            // in the other direction.
            assert!(
                out.windows(23).any(|w| w == b"Connection: keep-alive\r"),
                "{name}: we closed where nginx keeps the connection alive"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_withheld_body_gives_its_connection_slot_up_on_the_lingering_budget() {
        // The other half of the bug. Even once the ANSWER is prompt,
        // a body that never arrives must not pin a slot for the body
        // timeout: one packet against `MAX_CONNECTIONS` was a denial of
        // service as well as a fingerprint. nginx gives up after five
        // seconds — measured, both servers, in the differential.
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let task = tokio::spawn(serve(server, None, Arc::new(Stub::default())));
        client
            .write_all(b"POST /nope HTTP/1.1\r\nHost: x\r\nContent-Length: 10\r\n\r\n")
            .await
            .expect("write");
        let started = tokio::time::Instant::now();
        let mut seen = Vec::new();
        tokio::time::timeout(BODY_TIMEOUT / 2, client.read_to_end(&mut seen))
            .await
            .expect("the slot was still held at half the body timeout")
            .expect("read");
        task.await.expect("the connection task panicked");
        let held = started.elapsed();
        assert!(
            held <= LINGERING_TIMEOUT + Duration::from_secs(1),
            "one packet pinned a connection slot for {held:?}"
        );
        assert!(seen.starts_with(b"HTTP/1.1 404 Not Found"));
    }

    /// A handler that records what body it was handed, and can be told
    /// whether it wants one.
    struct Watcher {
        wants: bool,
        bodies: std::sync::Mutex<Vec<Vec<u8>>>,
    }

    impl Handler for Watcher {
        fn handle<'a>(
            &'a self,
            _head: &'a RequestHead,
            body: &'a [u8],
            _peer: Option<SocketAddr>,
        ) -> Pin<Box<dyn Future<Output = Answer> + Send + 'a>> {
            self.bodies
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(body.to_vec());
            Box::pin(async { Answer::not_found() })
        }

        fn needs_body(&self, _head: &RequestHead) -> bool {
            self.wants
        }

        fn observe_protocol_error(&self, _: Class, _: Option<SocketAddr>, _: Option<&str>) {}
    }

    async fn what_the_handler_saw(wants: bool, raw: &[u8]) -> Vec<Vec<u8>> {
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let handler = Arc::new(Watcher {
            wants,
            bodies: std::sync::Mutex::new(Vec::new()),
        });
        let task = tokio::spawn(serve(server, None, Arc::clone(&handler)));
        client.write_all(raw).await.expect("write");
        let mut sink = [0u8; 1024];
        let _ = tokio::time::timeout(BOUNDED, client.read(&mut sink))
            .await
            .expect("the parser held the connection instead of answering");
        task.abort();
        let seen = handler
            .bodies
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        seen
    }

    #[tokio::test(start_paused = true)]
    async fn a_head_decidable_answer_never_sees_the_body_even_when_it_all_arrived() {
        // The structural half of the fix, and the reason it is not just
        // "a shorter timeout": on the head-decidable path the handler
        // is handed an EMPTY slice whether or not the body turned up,
        // so "answered from the head" is a property of the code rather
        // than an accident of what happened to be buffered.
        let seen = what_the_handler_saw(
            false,
            b"POST /nope HTTP/1.1\r\nHost: x\r\nContent-Length: 3\r\n\r\nabc",
        )
        .await;
        assert_eq!(seen, vec![Vec::<u8>::new()], "the body reached the handler");
    }

    #[tokio::test(start_paused = true)]
    async fn a_handler_that_needs_the_body_still_gets_it() {
        // The guard on the other side: the post-knock `/api/` forward
        // has to hand real bytes to the Mac, and a fix that starved it
        // would be a silent outage rather than a hang.
        let seen = what_the_handler_saw(
            true,
            b"POST /nope HTTP/1.1\r\nHost: x\r\nContent-Length: 3\r\n\r\nabc",
        )
        .await;
        assert_eq!(seen, vec![b"abc".to_vec()]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_withheld_body_still_leaves_a_pipelined_request_readable() {
        // Answering before the body is drained is only safe if the
        // drain still happens. If it did not, the ten body bytes would
        // be parsed as the next request line and the socket would be
        // desynchronised — which is the failure this module exists to
        // prevent, traded for the one it just fixed.
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let task = tokio::spawn(serve(server, None, Arc::new(Stub::default())));
        client
            .write_all(b"POST /nope HTTP/1.1\r\nHost: x\r\nContent-Length: 10\r\n\r\n")
            .await
            .expect("write");
        let mut first = [0u8; 512];
        let n = tokio::time::timeout(BOUNDED, client.read(&mut first))
            .await
            .expect("held")
            .expect("read");
        assert!(first[..n].starts_with(b"HTTP/1.1 404 Not Found"));
        // Now the body, late, followed by a real second request.
        client
            .write_all(b"0123456789GET /nope HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .await
            .expect("write");
        let mut rest = Vec::new();
        tokio::time::timeout(BOUNDED, client.read_to_end(&mut rest))
            .await
            .expect("the second request was never answered")
            .expect("read");
        assert!(
            rest.starts_with(b"HTTP/1.1 404 Not Found"),
            "the late body was read as a request line: {:?}",
            String::from_utf8_lossy(&rest[..rest.len().min(48)])
        );
        task.abort();
    }

    #[test]
    fn the_request_line_grammar_is_nginx_s_and_not_the_rfc_s() {
        // Every row measured against nginx 1.31.4 and pinned in
        // `tests/nginx_differential.rs`; this is the same table without
        // the nginx dependency, so a regression is caught by `cargo
        // test` on a machine that has none.
        let bad = Err(HeadError::Refuse(Class::BadRequest));
        let unsupported = Err(HeadError::Refuse(Class::VersionNotSupported));
        for (line, want) in [
            // The method charset is [A-Z_-], not RFC-9110 `token`.
            ("GET /n HTTP/1.1", Ok(Version::Http11)),
            ("get /n HTTP/1.1", bad),
            ("Get /n HTTP/1.1", bad),
            ("GET2 /n HTTP/1.1", bad),
            ("GE~T /n HTTP/1.1", bad),
            ("GE.T /n HTTP/1.1", bad),
            ("GET_X /n HTTP/1.1", Ok(Version::Http11)),
            ("GE-T /n HTTP/1.1", Ok(Version::Http11)),
            // A control character in the target is a 400, not a 404 for
            // a path that happens to contain one.
            ("GET /n\tx HTTP/1.1", bad),
            ("GET /n\0x HTTP/1.1", bad),
            ("GET /n\rx HTTP/1.1", bad),
            ("GET /n\x7fx HTTP/1.1", bad),
            // The authority form belongs to CONNECT and nothing else.
            ("CONNECT x:443 HTTP/1.1", Ok(Version::Http11)),
            ("CONNECT /n HTTP/1.1", bad),
            ("CONNECT x HTTP/1.1", bad),
            ("GET x:443 HTTP/1.1", bad),
            // The version grammar: 1.x is served, a leading zero and an
            // over-999 minor are malformed, and only major 1 is ours.
            ("GET /n HTTP/1.0", Ok(Version::Http10)),
            ("GET /n HTTP/1.00", Ok(Version::Http10)),
            ("GET /n HTTP/1.0000", Ok(Version::Http10)),
            ("GET /n HTTP/1.01", Ok(Version::Http11)),
            ("GET /n HTTP/1.2", Ok(Version::Http11)),
            ("GET /n HTTP/1.9", Ok(Version::Http11)),
            ("GET /n HTTP/1.11", Ok(Version::Http11)),
            ("GET /n HTTP/1.999", Ok(Version::Http11)),
            ("GET /n HTTP/1.1000", bad),
            ("GET /n HTTP/01.1", bad),
            ("GET /n HTTP/0.9", bad),
            ("GET /n HTTP/1.", bad),
            ("GET /n HTTP/x.1", bad),
            ("GET /n HTTP/1.1x", bad),
            ("GET /n HTTP/2.0", unsupported),
            ("GET /n HTTP/9.9", unsupported),
            ("GET /n HTTP/11.1", unsupported),
            ("GET /n HTTP/1000.1", unsupported),
        ] {
            assert_eq!(
                parse_request_line(line).map(|r| r.version),
                want,
                "request line {line:?}"
            );
        }
    }

    #[test]
    fn a_leading_plus_is_not_a_number_in_a_length_or_a_chunk_size() {
        // Rust's `parse` and `from_str_radix` both accept one; nginx
        // accepts neither. The status divergence was the small half —
        // `+5` and `+A` PARSED, so each was a withheld-body hang in
        // disguise: the decoder went on to wait for bytes the sender
        // never had to send.
        let cl = |v: &str| {
            framing_of(
                Version::Http11,
                &[("content-length".to_string(), v.to_string())],
            )
        };
        assert_eq!(cl("5"), Ok(Framing::Length(5)));
        assert_eq!(cl("005"), Ok(Framing::Length(5)), "leading zeroes are fine");
        assert_eq!(cl("  5 "), Ok(Framing::Length(5)), "so is surrounding OWS");
        assert_eq!(cl("+5"), Err(Class::BadRequest));
        assert_eq!(cl("-1"), Err(Class::BadRequest));
        assert_eq!(cl("0x5"), Err(Class::BadRequest));
        assert_eq!(cl("5 5"), Err(Class::BadRequest));
        assert_eq!(cl(""), Err(Class::BadRequest));

        // A chunk size is HEXDIG+ with optional trailing padding and an
        // optional `;ext` — never a sign, never leading whitespace.
        assert_eq!(chunk_size("3"), Some(3));
        assert_eq!(chunk_size("003"), Some(3));
        assert_eq!(chunk_size("3 "), Some(3));
        assert_eq!(chunk_size("3;a=b"), Some(3));
        assert_eq!(chunk_size("3 ;a=b"), Some(3));
        assert_eq!(chunk_size("ff"), Some(255));
        assert_eq!(chunk_size("+A"), None);
        assert_eq!(chunk_size("-A"), None);
        assert_eq!(chunk_size(" 3"), None, "nginx refuses a LEADING space");
        assert_eq!(chunk_size("0x3"), None);
        assert_eq!(chunk_size(""), None);
    }

    /// A socket that is *always* ready with more `\r\n`. Never
    /// pending, never closed — the shape a peer with a fast link and a
    /// grudge presents.
    struct EndlessCrlf;

    impl AsyncRead for EndlessCrlf {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            b: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            let n = b.remaining().min(512);
            b.put_slice(&b"\r\n".repeat(n / 2));
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn an_endless_crlf_stream_runs_out_of_budget_rather_than_out_of_patience() {
        // The bug this catches is subtle twice over, and neither half
        // is visible from a `duplex` test:
        //
        // 1. the head budget was armed by "the buffer is non-empty
        //    after the CRLF skip", and a stream of pure CRLFs leaves it
        //    empty every pass, so the budget never armed at all;
        // 2. even armed, wrapping the read in `timeout_at` would not
        //    have helped — that yields `Err` only when the inner future
        //    is PENDING at the deadline, and this peer's reads are
        //    always ready. The budget has to be checked in the loop.
        //
        // Driven with a tiny budget so the test is fast and exact; the
        // production path passes `HEADER_TIMEOUT`.
        //
        // Run on its own thread and joined with a channel timeout,
        // rather than inside a `tokio::time::timeout`, because against
        // the bug this loop NEVER YIELDS: every read is ready, so
        // nothing returns `Pending`, the timer is never polled, and an
        // in-runtime timeout can never fire. The test would hang — the
        // worst outcome a regression guard can have — and so would the
        // runtime's own shutdown.
        //
        // That is also the honest measure of the severity. Pre-fix this
        // was not merely a held connection: it was an unyielding spin
        // on a shared runtime, which starves every other connection the
        // relay is serving.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .expect("runtime");
            let got = rt.block_on(read_head(
                &mut EndlessCrlf,
                &mut Vec::new(),
                Duration::from_secs(60),
                Duration::from_millis(50),
            ));
            let _ = tx.send(got);
        });

        match rx.recv_timeout(BOUNDED) {
            Ok(got) => assert_eq!(
                got,
                Err(HeadError::Closed),
                "an exhausted head budget is answered with silence, as nginx answers it"
            ),
            Err(_) => panic!(
                "read_head never returned. The head budget is not enforced against a \
                 peer whose reads are always ready — it is still spinning now."
            ),
        }
    }

    #[tokio::test]
    async fn a_flood_of_leading_crlfs_cannot_hold_the_connection_open() {
        // Found by attacking the round-3 fix itself, and it is the same
        // primitive in a fourth costume.
        //
        // nginx skips leading CRLFs before a request line — legal
        // debris from a pipelined predecessor — and so do we. But the
        // head budget was armed by "the buffer is non-empty AFTER the
        // skip", and a stream of nothing but CRLFs leaves the buffer
        // empty every single time. The budget never armed, the idle
        // wait was recomputed from `now` on every pass, and the
        // connection was held for as long as the peer cared to keep
        // typing `\r\n`. Free, silent, unbounded.
        //
        // The budget is now armed by the first byte READ, whatever that
        // byte turns out to be.
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let task = tokio::spawn(serve(server, None, Arc::new(Stub::default())));

        // Enough CRLFs to run well past any plausible number of
        // iterations, then a real request that must still be answered
        // — the skip is a real nginx behaviour and must survive.
        let mut raw = b"\r\n".repeat(8_000);
        raw.extend_from_slice(b"GET /nope HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
        client.write_all(&raw).await.expect("write");

        let out = tokio::time::timeout(BOUNDED, async {
            task.await.expect("connection task");
            let mut out = Vec::new();
            client.read_to_end(&mut out).await.expect("read");
            out
        })
        .await
        .expect("the CRLF skip held the connection instead of answering");

        assert!(
            out.starts_with(b"HTTP/1.1 404 Not Found"),
            "the debris must be skipped and the request behind it answered, got {:?}",
            String::from_utf8_lossy(&out[..out.len().min(48)])
        );
    }

    #[tokio::test]
    async fn the_connection_limit_makes_a_caller_wait_rather_than_allocating() {
        let limit = ConnectionLimit::new(2);
        assert_eq!(limit.available(), 2);
        let a = limit.acquire().await;
        let b = limit.acquire().await;
        assert_eq!(limit.available(), 0);

        // The third caller must not proceed. In the accept loop this is
        // taken BEFORE `accept()`, so waiting here means the connection
        // stays in the kernel's listen backlog rather than becoming a
        // task and a buffer.
        assert!(
            tokio::time::timeout(Duration::from_millis(200), limit.acquire())
                .await
                .is_err(),
            "the cap handed out more slots than it has"
        );

        drop(a);
        let c = tokio::time::timeout(Duration::from_millis(200), limit.acquire())
            .await
            .expect("a released slot must become available again");
        drop(c);
        drop(b);
    }
}
