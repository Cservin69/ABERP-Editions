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
//! *No hang* is the load-bearing half, and it is the half round 3 had
//! to fix. Three inputs each bought a ~60-second **silent** hold for
//! the price of one short line: a real HTTP/0.9 request, a
//! `Transfer-Encoding` that merely *contained* `chunked`, and a chunked
//! trailer section that never terminates. Each was worse than a wrong
//! answer. Real nginx answers all three in microseconds — measured —
//! so the hang identified the host outright, and each returned before
//! `observe_protocol_error`, so the canary never saw the probe that
//! found it. Three rules keep them dead:
//!
//! 1. **A request line is decided at its newline.** Only HTTP/1.0 and
//!    1.1 have a header block to wait for. Everything else — 0.9, an
//!    unsupported version, a line that can never parse — is complete
//!    when the line is.
//! 2. **Body framing is decided from the head**, before a byte of body
//!    is read. Every way of disagreeing about framing is answered
//!    immediately rather than discovered halfway through a decoder.
//! 3. **Budgets are totals, armed once.** A per-read timeout is not a
//!    bound: one byte every 59 seconds renews it forever.
//!
//! Silence is correct in exactly one place, and it is captured rather
//! than chosen: a *partial but well-formed* head is waited for and then
//! dropped without a word, because that is what nginx does with a
//! `client_header_timeout` — it does **not** send the `408` the RFC
//! would suggest, and volunteering one would be as distinguishing as
//! the hang was.
//!
//! **Named residual.** For malformed input outside the enumerated
//! classes the *status class* may differ: nginx has `501` for an
//! unknown transfer-coding, `413` for an over-long `Content-Length` and
//! a distinct longer `400` for an oversized header block, and
//! [`Class`] has none of them. Deliberate — see [`framing_of`] and
//! ADR-0115 §2 for why chasing nginx's full status table is bottomless
//! and why a status difference is a far weaker signal than a hang.
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
pub const BODY_TIMEOUT: Duration = Duration::from_secs(60);

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
/// with `400`. That is the named status-code residual in ADR-0115 §2 —
/// see the module docs. What matters for the disguise is that the
/// answer is **immediate**, which it now is; a status-code difference
/// on a request no client sends is a far weaker signal than a hang, and
/// real nginx deployments vary on it too.
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
        // `parse::<u64>` is what rejects `-1` and `abc`, both of which
        // nginx answers with 400.
        Some(raw) => raw
            .trim()
            .parse::<u64>()
            .map(Framing::Length)
            .map_err(|_| Class::BadRequest),
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

        let head_bytes = match read_head(&mut stream, &mut buf, idle).await {
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

        // A method nginx's static module does not serve. Checked before
        // the body is read, as nginx does: the 405 comes back without
        // waiting for a payload.
        if !matches!(head.method.as_str(), "GET" | "HEAD" | "POST") {
            let mut keep = head.client_wants_keep_alive && Class::NotAllowed.may_keep_alive();
            // The body must be drained even though it is discarded.
            // Leaving it on the socket would desynchronise the next
            // request on a kept-alive connection — and a server that
            // desynchronises where nginx does not is a server that has
            // been identified, which is the one thing this whole module
            // exists to prevent. If it cannot be drained, the
            // connection is no longer trustworthy, so close instead.
            if read_body(&mut stream, &mut buf, &head, handler.max_body())
                .await
                .is_err()
            {
                keep = false;
            }
            // Still fed to the trap: an odd verb against this host is a
            // probe like any other, and the answer is identical either
            // way.
            let _ = handler.handle(&head, &[], peer).await;
            let bytes = nginx::response(
                Class::NotAllowed,
                &nginx::http_date_now(),
                keep,
                !head.is_head(),
            );
            let _ = stream.write_all(&bytes).await;
            let _ = stream.flush().await;
            if !keep {
                return;
            }
            continue;
        }

        let body = match read_body(&mut stream, &mut buf, &head, handler.max_body()).await {
            Ok(b) => b,
            Err(e) => {
                // Either way this is answered and closed, never held.
                // The class is the difference: a framing error is the
                // protocol error nginx calls it, while a body that
                // simply did not arrive gets nginx's ordinary parked
                // page with `Connection: close` — measured against
                // nginx 1.31.4, which does not treat an undrainable
                // body as a protocol error.
                let class = match e {
                    BodyError::Refuse(c) => c,
                    BodyError::Undrainable => Class::NotFound,
                };
                // Fed to the trap in both cases. The body reader is
                // exactly where the three hang primitives lived, and a
                // refusal the canary never sees is a blind spot at the
                // one place a prober was proved to aim.
                handler.observe_protocol_error(class, peer, Some(&hint_of(&head_bytes)));
                let bytes = nginx::response(class, &nginx::http_date_now(), false, !head.is_head());
                let _ = stream.write_all(&bytes).await;
                let _ = stream.flush().await;
                return;
            }
        };

        let answer = handler.handle(&head, &body, peer).await;
        let (bytes, keep) = render(&answer, &head);
        if stream.write_all(&bytes).await.is_err() || stream.flush().await.is_err() {
            return;
        }
        if !keep {
            return;
        }
    }
}

/// Serialise an answer, returning the bytes and whether the connection
/// survives.
fn render(answer: &Answer, head: &RequestHead) -> (Vec<u8>, bool) {
    let date = nginx::http_date_now();
    match answer {
        Answer::Nginx(class) => {
            let keep = head.client_wants_keep_alive && class.may_keep_alive();
            (nginx::response(*class, &date, keep, !head.is_head()), keep)
        }
        Answer::Portal(p) => {
            let keep = head.client_wants_keep_alive;
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
) -> Result<Vec<u8>, HeadError>
where
    S: AsyncRead + Unpin,
{
    // Armed by the first byte of the request and never rearmed. See
    // [`HEADER_TIMEOUT`] on why this is a total budget rather than a
    // per-read one.
    let mut deadline: Option<tokio::time::Instant> = None;

    loop {
        // Leading CRLFs before a request line are skipped by nginx —
        // they are legal debris from a previous pipelined request.
        while buf.first().is_some_and(|b| *b == b'\r' || *b == b'\n') {
            buf.remove(0);
        }
        if deadline.is_none() && !buf.is_empty() {
            deadline = Some(tokio::time::Instant::now() + HEADER_TIMEOUT);
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
        if let Some(head) = request_line_settles_it(buf)? {
            return Ok(head);
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
    buf[..end].iter().any(|b| !is_token_byte(*b))
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
    if !is_token(method) {
        return Err(HeadError::Refuse(Class::BadRequest));
    }
    let target = parts
        .next()
        .filter(|t| !t.is_empty())
        .ok_or(HeadError::Refuse(Class::BadRequest))?;
    // The asterisk-form is legal per RFC for OPTIONS; nginx's static
    // server rejects it, and matching nginx is the whole job.
    if !target.starts_with('/') && !target.contains("://") {
        return Err(HeadError::Refuse(Class::BadRequest));
    }

    let version = match parts.next() {
        None => Version::Http09,
        Some("HTTP/1.1") => Version::Http11,
        Some("HTTP/1.0") => Version::Http10,
        Some(v) if v.starts_with("HTTP/") => {
            // A well-formed but unsupported version is 505; a
            // malformed one is 400. `HTTP/9.9` and `HTTP/2.0` are the
            // former, `HTTP/x` the latter.
            let rest = &v["HTTP/".len()..];
            let well_formed = matches!(rest.split_once('.'),
                Some((maj, min))
                    if !maj.is_empty()
                        && !min.is_empty()
                        && maj.bytes().all(|b| b.is_ascii_digit())
                        && min.bytes().all(|b| b.is_ascii_digit()));
            return Err(HeadError::Refuse(if well_formed {
                Class::VersionNotSupported
            } else {
                Class::BadRequest
            }));
        }
        Some(_) => return Err(HeadError::Refuse(Class::BadRequest)),
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
/// Bodies are read and **discarded** on the parked path, but they must
/// still be read: leaving unconsumed bytes on a kept-alive socket
/// desynchronises the next request, and a server that desynchronises
/// where nginx does not is a server that has been identified.
async fn read_body<S>(
    stream: &mut S,
    buf: &mut Vec<u8>,
    head: &RequestHead,
    max_body: usize,
) -> Result<Vec<u8>, BodyError>
where
    S: AsyncRead + Unpin,
{
    // One total budget for the whole body, armed here. Every read below
    // shares it, so no amount of dripping extends it.
    let deadline = tokio::time::Instant::now() + BODY_TIMEOUT;

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
        // A chunk-size line may carry `;ext=...`.
        let size_text = line.split(';').next().unwrap_or("").trim();
        // A chunk size that is not hex is answered NOW, from the bytes
        // already in hand. Measured against nginx 1.31.4: a bogus chunk
        // size gets the ordinary parked page with `Connection: close`,
        // immediately — nginx never blocks on a body it cannot drain.
        let Ok(size) = usize::from_str_radix(size_text, 16) else {
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
        let (bytes, keep) = render(&Answer::not_found(), &head);
        assert!(!keep);
        let s = String::from_utf8(bytes).expect("utf8");
        assert!(s.contains("Content-Length: 146"));
        assert!(s.ends_with("\r\n\r\n"), "a HEAD carries no body");
    }

    #[test]
    fn a_clean_404_stays_open_when_the_client_wants_it_to() {
        // The two-requests-down-one-socket tell.
        let head = parse("GET /nope HTTP/1.1\r\nHost: x").expect("parses");
        let (_, keep) = render(&Answer::not_found(), &head);
        assert!(keep, "always-closing is a fingerprint");
    }

    #[tokio::test]
    async fn a_chunked_body_is_decoded_and_leaves_the_stream_aligned() {
        let head =
            parse("POST /x HTTP/1.1\r\nHost: h\r\nTransfer-Encoding: chunked").expect("parses");
        let mut buf = b"3\r\nabc\r\n2\r\nde\r\n0\r\n\r\nNEXT".to_vec();
        let mut empty: &[u8] = &[];
        let body = read_body(&mut empty, &mut buf, &head, MAX_REQUEST_BODY)
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
            read_body(&mut empty, &mut buf, &head, MAX_REQUEST_BODY).await,
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
            b"POST /nope HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n              3\r\nabc\r\n0\r\n\r\nGET /nope HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert_eq!(
            out.windows(8).filter(|w| *w == b"HTTP/1.1").count(),
            2,
            "the second, pipelined request was not answered — the socket desynchronised"
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
