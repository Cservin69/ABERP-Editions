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

/// nginx's `client_header_timeout`.
pub const HEADER_TIMEOUT: Duration = Duration::from_secs(60);
/// nginx's `client_body_timeout`.
pub const BODY_TIMEOUT: Duration = Duration::from_secs(60);
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

    /// Called for a request this module refused before it could ever
    /// become a [`RequestHead`] — a malformed request line, an
    /// unsupported version, an over-long URI.
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
            Err(class) => {
                handler.observe_protocol_error(class, peer, Some(&hint_of(&head_bytes)));
                let bytes = nginx::response(class, &nginx::http_date_now(), false, true);
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
    loop {
        // Leading CRLFs before a request line are skipped by nginx —
        // they are legal debris from a previous pipelined request.
        while buf.first().is_some_and(|b| *b == b'\r' || *b == b'\n') {
            buf.remove(0);
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

        let mut chunk = [0u8; 4096];
        let n = match tokio::time::timeout(idle, stream.read(&mut chunk)).await {
            // A silent client is dropped without a word, like nginx.
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
) -> Result<Vec<u8>, Class>
where
    S: AsyncRead + Unpin,
{
    let chunked = head
        .header("transfer-encoding")
        .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));

    if chunked {
        return read_chunked(stream, buf, max_body).await;
    }

    let Some(raw_len) = head.header("content-length") else {
        return Ok(Vec::new());
    };
    let len: usize = raw_len.parse().map_err(|_| Class::BadRequest)?;
    if len > max_body {
        return Err(Class::BadRequest);
    }
    while buf.len() < len {
        let mut chunk = [0u8; 4096];
        let n = match tokio::time::timeout(BODY_TIMEOUT, stream.read(&mut chunk)).await {
            Ok(Ok(n)) if n > 0 => n,
            // Truncated body: the stream is unusable either way.
            _ => return Err(Class::BadRequest),
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
) -> Result<Vec<u8>, Class>
where
    S: AsyncRead + Unpin,
{
    let mut body = Vec::new();
    loop {
        let line = read_line(stream, buf).await?;
        // A chunk-size line may carry `;ext=...`.
        let size_text = line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16).map_err(|_| Class::BadRequest)?;
        if body.len().saturating_add(size) > max_body {
            return Err(Class::BadRequest);
        }
        if size == 0 {
            // Trailers, then the final blank line.
            loop {
                let t = read_line(stream, buf).await?;
                if t.is_empty() {
                    break;
                }
            }
            return Ok(body);
        }
        // The chunk plus its trailing CRLF.
        while buf.len() < size + 2 {
            let mut chunk = [0u8; 4096];
            let n = match tokio::time::timeout(BODY_TIMEOUT, stream.read(&mut chunk)).await {
                Ok(Ok(n)) if n > 0 => n,
                _ => return Err(Class::BadRequest),
            };
            buf.extend_from_slice(&chunk[..n]);
        }
        body.extend_from_slice(&buf[..size]);
        buf.drain(..size + 2);
    }
}

/// One CRLF- or LF-terminated line from the buffered stream.
async fn read_line<S>(stream: &mut S, buf: &mut Vec<u8>) -> Result<String, Class>
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
            return Err(Class::BadRequest);
        }
        let mut chunk = [0u8; 4096];
        let n = match tokio::time::timeout(BODY_TIMEOUT, stream.read(&mut chunk)).await {
            Ok(Ok(n)) if n > 0 => n,
            _ => return Err(Class::BadRequest),
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
            Err(Class::BadRequest)
        );
    }
}
