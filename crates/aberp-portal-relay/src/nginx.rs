//! The disguise (ADR-0115 §3.2) — every un-authenticated answer is the
//! answer a **parked nginx** would have given, byte for byte.
//!
//! # The conflict this module resolves
//!
//! Two earlier readings of §3.2 pulled in opposite directions:
//!
//! - *"one uniform 404 for everything"* — simple, and provably free of
//!   a path oracle, because literally every request gets the same
//!   bytes; but
//! - *"look like the VPS's default vhost"* — because a host that
//!   answers `404` to a malformed request line, to `HTTP/9.9`, and to a
//!   TLS `ClientHello` sent at a cleartext port is a host that is
//!   **not** running nginx, and says so to anyone who checks.
//!
//! The second wins, and the reason is that the first is not actually
//! uniform where it counts. A uniform 404 is uniform *across paths*,
//! which is the property §3.2 was written for — but it is wildly
//! non-uniform *across request classes*, and a scanner that sends one
//! deliberately broken request line learns more from a `404` than it
//! ever learns from probing paths. Real nginx answers `400` there. So
//! the rule is now stated per class:
//!
//! > within a request class the answer is fixed and path-independent;
//! > across classes the answer is whatever nginx does.
//!
//! Path-independence — the anti-oracle property — is untouched: nothing
//! in this module reads the target, and [`Class`] is chosen entirely
//! from the shape of the request, never from whether a path exists.
//!
//! # Where the bytes come from
//!
//! Not from memory and not from the RFCs — from a real nginx. The
//! captures live in `tests/fixtures/nginx-goldens.txt` (nginx 1.31.4,
//! `server_tokens off`), and `tests/nginx_indistinguishable.rs` pins
//! this module against them per class. The capture procedure is in
//! ADR-0115 §3.3 so it can be re-run against a newer nginx.
//!
//! Two findings from that capture are worth naming, because both
//! contradict what one would write from first principles:
//!
//! 1. the 404 body is **146** bytes, not 150 — an earlier hand-written
//!    body in this crate omitted nginx's `<hr><center>nginx</center>`
//!    line and was 4 bytes short of the real thing, which is a
//!    fingerprint on its own;
//! 2. `Connection` **echoes the client's intent**. nginx keeps an
//!    HTTP/1.1 connection alive through a 404 and through a 405. A
//!    server that closes every un-knocked connection is trivially
//!    distinguishable from nginx by opening one socket and sending two
//!    requests — which is why [`crate::http1`] implements real
//!    keep-alive on the un-knocked path rather than always closing.
//!
//! # No HSTS, no CSP, not here
//!
//! A parked nginx sends five headers and no more. Every security header
//! this portal wants — HSTS, CSP, `Referrer-Policy`,
//! `X-Content-Type-Options` — is therefore **absent** from every
//! response in this module and present only on the authenticated shell
//! and API (see `render_portal` in [`crate::http1`]). Stamping
//! HSTS "uniformly, on every answer including the 404" was the previous
//! posture and it was exactly backwards: it made the parked surface
//! unique in the only way that matters, since the thing it is
//! pretending to be does not send it.

use std::fmt::Write as _;

/// The `Server` line. `server_tokens off` drops the version, which is
/// both the commonest hardened configuration and the one that gives an
/// impersonator the least to get wrong.
pub const SERVER: &str = "nginx";

/// One nginx answer class.
///
/// Chosen from the *shape* of the request — its line, its version, its
/// headers — and never from its target. See the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// The ordinary parked answer: an unknown path on a well-formed
    /// request. 146 bytes.
    NotFound,
    /// A malformed request line, a missing or duplicated `Host` on
    /// HTTP/1.1, a bad header name, a space in the target, `OPTIONS *`,
    /// or a TLS `ClientHello` arriving at a cleartext port. 150 bytes.
    BadRequest,
    /// A well-formed method token outside nginx's static-module set
    /// (`GET`/`HEAD`/`POST`). 150 bytes. Note this one does **not**
    /// close the connection.
    NotAllowed,
    /// A request line past the 8 KiB `large_client_header_buffers`
    /// slot. 170 bytes.
    UriTooLarge,
    /// Any version that is not `HTTP/1.0` or `HTTP/1.1` — including
    /// `HTTP/2.0` sent in the clear at an http1 listener. 180 bytes.
    VersionNotSupported,
}

impl Class {
    /// The numeric status.
    #[must_use]
    pub const fn code(self) -> u16 {
        match self {
            Self::NotFound => 404,
            Self::BadRequest => 400,
            Self::NotAllowed => 405,
            Self::UriTooLarge => 414,
            Self::VersionNotSupported => 505,
        }
    }

    /// The reason phrase. nginx's are not all the IANA spellings —
    /// `405` is "Not Allowed", not "Method Not Allowed", and `414` is
    /// "Request-URI Too Large", not "URI Too Long". Getting either
    /// "right" per the RFC would be getting it wrong per nginx.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::NotFound => "Not Found",
            Self::BadRequest => "Bad Request",
            Self::NotAllowed => "Not Allowed",
            Self::UriTooLarge => "Request-URI Too Large",
            Self::VersionNotSupported => "HTTP Version Not Supported",
        }
    }

    /// `false` for the classes nginx refuses to keep a connection open
    /// through. `true` means "echo whatever the client asked for" —
    /// which is what [`Class::NotFound`] and [`Class::NotAllowed`] do.
    ///
    /// This is not a tidiness detail. A protocol error leaves the
    /// connection in an unknown state — bytes of a body may still be in
    /// flight — so nginx closes, and so must we; conversely a server
    /// that closes after a *clean* 404 is distinguishable from nginx
    /// with two requests down one socket.
    #[must_use]
    pub const fn may_keep_alive(self) -> bool {
        matches!(self, Self::NotFound | Self::NotAllowed)
    }

    /// The `<h1>` and `<title>` text: `"404 Not Found"`.
    #[must_use]
    pub fn status_line(self) -> String {
        format!("{} {}", self.code(), self.reason())
    }

    /// The error page, byte-exact.
    ///
    /// Every nginx error page is this one template with the status line
    /// substituted twice, which is why the lengths come out as
    /// `120 + 2 × status_line`: 146, 150, 170, 180. That relationship
    /// is asserted in the tests, so a body edited by hand that happens
    /// to keep the right length cannot slip through.
    #[must_use]
    pub fn body(self) -> String {
        let s = self.status_line();
        let mut out = String::with_capacity(120 + 2 * s.len());
        // Written with explicit \r\n rather than a raw string so the
        // line endings survive any future reformat of this file. nginx
        // uses CRLF inside the body; an editor that "helpfully"
        // normalised a raw literal to LF would shift every length.
        let _ = write!(
            out,
            "<html>\r\n<head><title>{s}</title></head>\r\n<body>\r\n<center><h1>{s}</h1></center>\r\n<hr><center>nginx</center>\r\n</body>\r\n</html>\r\n"
        );
        out
    }

    /// Length of [`Class::body`] — 146 / 150 / 170 / 180.
    #[must_use]
    pub fn content_length(self) -> usize {
        BODY_OVERHEAD + 2 * self.status_line().len()
    }
}

/// The fixed bytes of the error-page template, excluding the two copies
/// of the status line.
const BODY_OVERHEAD: usize = 120;

/// Serialise one nginx answer.
///
/// `keep_alive` is the connection disposition already resolved by
/// [`crate::http1`] against both the client's intent and
/// [`Class::may_keep_alive`]; this function does not second-guess it,
/// it only renders it.
///
/// `include_body` is `false` for `HEAD`, where nginx sends the headers
/// — `Content-Length: 146` included — and no body at all.
///
/// The header order is Server, Date, Content-Type, Content-Length,
/// Connection. That order is as load-bearing as the values: a response
/// carrying identical headers in a different sequence identifies the
/// server just as well as a `Server:` line would. Writing the bytes by
/// hand rather than through a `HeaderMap` is deliberate — a map has no
/// stable order to promise, and this one is a promise.
#[must_use]
pub fn response(class: Class, date: &str, keep_alive: bool, include_body: bool) -> Vec<u8> {
    let body = class.body();
    let mut out = String::with_capacity(body.len() + 192);
    let _ = write!(out, "HTTP/1.1 {} {}\r\n", class.code(), class.reason());
    let _ = write!(out, "Server: {SERVER}\r\n");
    let _ = write!(out, "Date: {date}\r\n");
    let _ = write!(out, "Content-Type: text/html\r\n");
    let _ = write!(out, "Content-Length: {}\r\n", body.len());
    let _ = write!(
        out,
        "Connection: {}\r\n",
        if keep_alive { "keep-alive" } else { "close" }
    );
    out.push_str("\r\n");
    if include_body {
        out.push_str(&body);
    }
    out.into_bytes()
}

/// The HTTP/0.9 answer: the body alone.
///
/// A request line with no version (`GET /nope`) is HTTP/0.9, and nginx
/// answers it with the error page and **nothing else** — no status
/// line, no headers — then closes. Rare enough to be forgotten, common
/// enough that scanners send it precisely because it is forgotten.
#[must_use]
pub fn response_http_0_9(class: Class) -> Vec<u8> {
    class.body().into_bytes()
}

/// `Date`, in the RFC-9110 IMF-fixdate spelling nginx uses:
/// `Fri, 21 Aug 2026 21:08:46 GMT`.
///
/// Always UTC and always this format. A locale-dependent or
/// offset-carrying date would be a fingerprint of its own.
#[must_use]
pub fn http_date(at: time::OffsetDateTime) -> String {
    const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let at = at.to_offset(time::UtcOffset::UTC);
    format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
        DAYS[at.weekday().number_days_from_monday() as usize],
        at.day(),
        MONTHS[at.month() as usize - 1],
        at.year(),
        at.hour(),
        at.minute(),
        at.second(),
    )
}

/// `Date` for right now.
#[must_use]
pub fn http_date_now() -> String {
    http_date(time::OffsetDateTime::now_utc())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_captured_length_is_reproduced() {
        // The numbers on the left are from a real nginx — see
        // tests/fixtures/nginx-goldens.txt. If one of these ever
        // changes, the disguise is broken and the test is right.
        assert_eq!(Class::NotFound.content_length(), 146);
        assert_eq!(Class::BadRequest.content_length(), 150);
        assert_eq!(Class::NotAllowed.content_length(), 150);
        assert_eq!(Class::UriTooLarge.content_length(), 170);
        assert_eq!(Class::VersionNotSupported.content_length(), 180);
    }

    #[test]
    fn the_computed_length_agrees_with_the_rendered_body() {
        // Guards the `120 + 2×status_line` arithmetic against a hand
        // edit of the template that keeps the total by accident.
        for c in [
            Class::NotFound,
            Class::BadRequest,
            Class::NotAllowed,
            Class::UriTooLarge,
            Class::VersionNotSupported,
        ] {
            assert_eq!(c.body().len(), c.content_length(), "{c:?}");
        }
    }

    #[test]
    fn the_body_carries_the_nginx_signature_line() {
        // The 4 bytes an earlier hand-written body was missing.
        assert!(Class::NotFound
            .body()
            .contains("<hr><center>nginx</center>"));
    }

    #[test]
    fn the_404_is_byte_identical_to_the_capture() {
        let got = response(
            Class::NotFound,
            "Fri, 21 Aug 2026 21:08:46 GMT",
            false,
            true,
        );
        let want = concat!(
            "HTTP/1.1 404 Not Found\r\n",
            "Server: nginx\r\n",
            "Date: Fri, 21 Aug 2026 21:08:46 GMT\r\n",
            "Content-Type: text/html\r\n",
            "Content-Length: 146\r\n",
            "Connection: close\r\n",
            "\r\n",
            "<html>\r\n<head><title>404 Not Found</title></head>\r\n<body>\r\n",
            "<center><h1>404 Not Found</h1></center>\r\n",
            "<hr><center>nginx</center>\r\n</body>\r\n</html>\r\n",
        );
        assert_eq!(String::from_utf8(got).expect("utf8"), want);
        // The whole capture was 289 bytes on the wire.
        assert_eq!(want.len(), 289);
    }

    #[test]
    fn a_head_answer_keeps_the_length_and_drops_the_body() {
        let got = response(
            Class::NotFound,
            "Fri, 21 Aug 2026 21:12:46 GMT",
            false,
            false,
        );
        assert_eq!(got.len(), 143, "the captured HEAD was 143 bytes");
        let s = String::from_utf8(got).expect("utf8");
        assert!(s.contains("Content-Length: 146"));
        assert!(s.ends_with("\r\n\r\n"));
    }

    #[test]
    fn no_security_header_ever_appears_on_a_parked_answer() {
        // The posture inversion this module exists to fix: a parked
        // nginx does not send these, so neither may we.
        for c in [
            Class::NotFound,
            Class::BadRequest,
            Class::NotAllowed,
            Class::UriTooLarge,
            Class::VersionNotSupported,
        ] {
            for keep in [true, false] {
                let s = String::from_utf8(response(c, "D", keep, true)).expect("utf8");
                let lower = s.to_ascii_lowercase();
                for forbidden in [
                    "strict-transport-security",
                    "content-security-policy",
                    "referrer-policy",
                    "x-content-type-options",
                    "x-frame-options",
                    "cache-control",
                    "set-cookie",
                ] {
                    assert!(!lower.contains(forbidden), "{c:?} leaked `{forbidden}`");
                }
            }
        }
    }

    #[test]
    fn the_header_order_is_the_nginx_order() {
        let s = String::from_utf8(response(Class::BadRequest, "D", false, true)).expect("utf8");
        let head = s.split("\r\n\r\n").next().expect("head");
        let names: Vec<&str> = head
            .lines()
            .skip(1)
            .filter_map(|l| l.split(':').next())
            .collect();
        assert_eq!(
            names,
            [
                "Server",
                "Date",
                "Content-Type",
                "Content-Length",
                "Connection"
            ]
        );
    }

    #[test]
    fn only_a_clean_answer_may_stay_open() {
        assert!(Class::NotFound.may_keep_alive());
        assert!(Class::NotAllowed.may_keep_alive());
        // A protocol error leaves the stream in an unknown state.
        assert!(!Class::BadRequest.may_keep_alive());
        assert!(!Class::UriTooLarge.may_keep_alive());
        assert!(!Class::VersionNotSupported.may_keep_alive());
    }

    #[test]
    fn http_0_9_is_the_bare_body() {
        let b = response_http_0_9(Class::NotFound);
        assert_eq!(b.len(), 146);
        assert!(!String::from_utf8(b).expect("utf8").contains("HTTP/1.1"));
    }

    #[test]
    fn the_date_matches_the_captured_spelling() {
        // 2026-08-21 21:08:46 UTC was a Friday.
        let t = time::OffsetDateTime::from_unix_timestamp(1_787_346_526).expect("timestamp");
        assert_eq!(http_date(t), "Fri, 21 Aug 2026 21:08:46 GMT");
    }

    #[test]
    fn the_reason_phrases_are_nginxs_and_not_ianas() {
        // Both of these are wrong per the RFC and right per nginx.
        assert_eq!(Class::NotAllowed.reason(), "Not Allowed");
        assert_eq!(Class::UriTooLarge.reason(), "Request-URI Too Large");
    }
}
