//! The front (ADR-0115 §3) — one handler, two possible answers.
//!
//! Every request that reaches this listener, by any method, on any
//! path, with any headers, gets exactly one of:
//!
//! 1. **the parked nginx** ([`crate::nginx`]) — the answer a default,
//!    empty vhost would have given to that same request class, byte for
//!    byte, whether the portal exists, the Mac is offline, the knock is
//!    wrong, or the request line is garbage; or
//! 2. the portal, because the request carried the current knock token.
//!
//! # Why this is a single handler and not a route table
//!
//! A router with real routes answers `405` for a known path with the
//! wrong verb and `404` for an unknown one, and may vary headers
//! between them. Each difference is a bit of information about whether
//! a path exists — exactly what §3.2 forbids:
//!
//! > every unauthenticated request — wrong path, right path, `HEAD`,
//! > `POST`, garbage SNI, direct IP — receives the same minimal
//! > answer: same status, same headers […] same body bytes.
//!
//! So there is no route table. One handler sees everything, and the
//! only branch that can produce something other than the parked answer
//! is a constant-time knock comparison against a token the Mac
//! supplied.
//!
//! Note what §3.2 does and does not promise, restated after the B1/B2
//! reconciliation in [`crate::nginx`]: the answer is fixed and
//! **path-independent**, which is the anti-oracle property it was
//! written for. It is not identical across request *classes*, because a
//! real nginx is not either, and pretending otherwise was the louder
//! tell of the two.
//!
//! # And when the Mac is gone
//!
//! [`Broker::knock_matches`] answers `false` when no lease is live, so
//! an outage collapses the portal to the parked 404 for everyone —
//! including a correctly-bookmarked, fully-enrolled Ervin. Ervin's §9.5
//! decision: "keep the pure 404 (unreachable = invisible, no
//! exceptions)".
//!
//! # Nothing escapes the trap
//!
//! Every path that ends in a parked response feeds the canary — the
//! ordinary un-knocked 404, a wrong knock, an overloaded queue, a Mac
//! that never answered, **and** the protocol-level refusals that never
//! reach a parsed request at all ([`Front::observe_protocol_error`]).
//! The observation is silent and the response is byte-identical either
//! way, so the trap costs a prober nothing observable and misses
//! nothing.

use std::net::SocketAddr;
use std::sync::Arc;

use aberp_portal_core::proto::PortalRequest;

use crate::broker::Broker;
use crate::canary::{Canary, Observation};
use crate::http1::{Answer, Handler, PortalAnswer, RequestHead};
use crate::nginx::Class;

/// The portal shell, compiled in. Served only to a caller that passed
/// the knock — §3.2's "No portal artifact pre-gate: no favicon, no JS
/// bundle, no manifest […] the app shell literally is not served".
pub const SHELL_HTML: &str = include_str!("../assets/shell.html");

/// Front state.
#[derive(Debug)]
pub struct Front {
    pub broker: Arc<Broker>,
    /// The scanner trap. Fed on every parked-response path.
    pub canary: Arc<Canary>,
}

impl Handler for Front {
    fn handle<'a>(
        &'a self,
        head: &'a RequestHead,
        body: &'a [u8],
        peer: Option<SocketAddr>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Answer> + Send + 'a>> {
        Box::pin(self.respond(head, body, peer))
    }

    fn observe_protocol_error(&self, class: Class, peer: Option<SocketAddr>, hint: Option<&str>) {
        // A malformed request line never produced a `RequestHead`, so
        // there is no method and no path to record — but "somebody sent
        // this host a deliberately broken request" is a strong signal
        // on a host with no legitimate unauthenticated visitors, and
        // losing it because the parser refused first would be a hole in
        // the trap exactly where scanners aim.
        self.canary.observe(Observation {
            wall: time::OffsetDateTime::now_utc(),
            source: peer.map(|a| a.ip()),
            method: format!("<{}>", class.code()),
            // Sanitised and truncated downstream by
            // `aberp_portal_core::canary::sanitise`; this is attacker
            // bytes and never reaches a log unfiltered.
            path: hint.unwrap_or("<malformed>").to_string(),
            user_agent: None,
            host: None,
        });
    }
}

impl Front {
    async fn respond(&self, head: &RequestHead, body: &[u8], peer: Option<SocketAddr>) -> Answer {
        let path = head.path();
        let source = peer.map(|a| a.ip());

        let Some((knock, rest)) = split_knock(path) else {
            self.trip(source, head);
            return Answer::not_found();
        };
        if !self.broker.knock_matches(knock) {
            self.trip(source, head);
            return Answer::not_found();
        }
        // A valid knock is the operator. Remember the source briefly so
        // the browser's own follow-up requests to the bare host —
        // `/favicon.ico` and friends, which carry no knock — do not
        // page anyone. See `canary::AUTHORISED_GRACE`, and
        // `aberp_portal_core::canary::AUTHORISED_CHATTER_PATHS` for how
        // narrow that exemption is.
        self.canary.note_authorised(source);

        // Past the gate. The shell, or a forwarded API call — nothing
        // else.
        match rest {
            "" | "/" => shell(),
            api if api.starts_with("/api/") => self.forward(head, api, body, peer, knock).await,
            // A knocked caller asking for a path the portal does not
            // have gets the same answer as a stranger. There is no
            // "page not found" page to learn the shape of the app from.
            // No canary: they presented the token, so they are the
            // operator or someone who already has it — which the
            // knock-shaped classifier would only mislabel.
            _ => Answer::not_found(),
        }
    }

    /// Hand one probe to the canary.
    ///
    /// Everything expensive happens on the aggregator task; this is a
    /// struct build and a `try_send`. Identical work for every probe —
    /// tripping the decoy costs exactly what brushing the host costs,
    /// so the trap cannot be found by timing it.
    fn trip(&self, source: Option<std::net::IpAddr>, head: &RequestHead) {
        self.canary.observe(Observation {
            wall: time::OffsetDateTime::now_utc(),
            source,
            method: head.method.clone(),
            path: head.path().to_string(),
            user_agent: head.header("user-agent").map(str::to_string),
            host: head.header("host").map(str::to_string),
        });
    }

    async fn forward(
        &self,
        head: &RequestHead,
        path: &str,
        body: &[u8],
        peer: Option<SocketAddr>,
        knock: &str,
    ) -> Answer {
        use base64::Engine as _;

        let req = PortalRequest {
            // Verbatim. The relay has no opinion about verbs; §6.3 puts
            // that opinion on the Mac.
            method: head.method.clone(),
            path: path.to_string(),
            query: head.query().map(str::to_string),
            cookie: head.header("cookie").map(str::to_string),
            body_b64: (!body.is_empty())
                .then(|| base64::engine::general_purpose::STANDARD.encode(body)),
            peer: peer.map(|a| a.ip().to_string()),
        };

        match self.broker.park(req).await {
            Ok(res) => render(&res, knock),
            // Every failure to reach the Mac collapses to the parked
            // answer. A `502 Bad Gateway` here would confirm that
            // something exists behind this host — the one thing §G2
            // forbids — and a `503` under load would do the same.
            //
            // NO canary here, deliberately. This caller presented a
            // valid knock, so they are the operator or someone who
            // already holds the token; a timeout on a slow PDF render,
            // or a full queue, would otherwise classify Ervin's own
            // `/api/...` request as an `ApiShaped` HIGH and page him
            // for a degradation he is already looking at. An alert that
            // fires during normal degraded operation is an alert that
            // gets ignored, which costs more than the observation is
            // worth. The failure is logged here and the Mac's own
            // heartbeat gap (`aberp_portal_agent::poll`) is what
            // reports a relay that has genuinely stopped answering.
            Err(e) => {
                tracing::info!(reason = ?e, "dispatch failed; answering as a parked host");
                Answer::not_found()
            }
        }
    }
}

/// Split `/<knock>/rest` into `("<knock>", "/rest")`.
///
/// Purely lexical: no percent-decoding, no normalisation, no `..`
/// collapsing. Whatever the client sent is what gets compared, so there
/// is no decoded-versus-compared discrepancy for an attacker to wedge
/// apart.
fn split_knock(path: &str) -> Option<(&str, &str)> {
    let trimmed = path.strip_prefix('/')?;
    match trimmed.find('/') {
        Some(i) => Some((&trimmed[..i], &trimmed[i..])),
        None => Some((trimmed, "")),
    }
}

/// Render an answer the Mac produced.
fn render(res: &aberp_portal_core::PortalResponse, knock: &str) -> Answer {
    let Some(body) = res.body() else {
        // An agent we cannot render is an agent we do not advertise.
        return Answer::not_found();
    };
    Answer::Portal(Box::new(PortalAnswer {
        status: res.status,
        reason: reason_for(res.status),
        content_type: res.content_type.clone(),
        body,
        set_cookie: res.set_cookie.as_ref().map(|c| scope_cookie(c, knock)),
    }))
}

/// Pin a `Set-Cookie` the Mac minted to the knock prefix it was minted
/// under.
///
/// The agent mints the cookie and owns its flags (§4.4:
/// `Secure; HttpOnly; SameSite=Strict`), but the relay is the only
/// party that knows the URL prefix the browser actually used, because
/// the knock is stripped before the request crosses Leg B. So the
/// `Path` is stamped here.
///
/// Why it matters: a cookie at `Path=/` is sent to **every** path on the
/// host, including the un-knocked ones. That hands the session cookie
/// to any request that brushes the bare hostname — a mistyped URL, a
/// prefetch, an embedded image — and undoes the point of putting the
/// gate in the path. Scoping it to `/<knock>/` means the browser only
/// ever offers it inside the portal.
///
/// If the agent already set a `Path`, it is replaced: the agent cannot
/// know the right one.
fn scope_cookie(cookie: &str, knock: &str) -> String {
    let kept: Vec<&str> = cookie
        .split(';')
        .map(str::trim)
        .filter(|a| !a.is_empty() && !a.to_ascii_lowercase().starts_with("path="))
        .collect();
    format!("{}; Path=/{}/", kept.join("; "), knock)
}

fn shell() -> Answer {
    Answer::Portal(Box::new(PortalAnswer {
        status: 200,
        reason: "OK",
        content_type: "text/html; charset=utf-8".to_string(),
        body: SHELL_HTML.as_bytes().to_vec(),
        set_cookie: None,
    }))
}

/// Reason phrases for the statuses the agent can produce.
///
/// A fixed table rather than a formatted number: the reason phrase is
/// on the wire, and an unrecognised status must not put an
/// attacker-influenced string there.
fn reason_for(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Not Allowed",
        429 => "Too Many Requests",
        503 => "Service Unavailable",
        _ => "OK",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn knock_split_is_lexical_only() {
        assert_eq!(split_knock("/abc"), Some(("abc", "")));
        assert_eq!(split_knock("/abc/"), Some(("abc", "/")));
        assert_eq!(split_knock("/abc/api/status"), Some(("abc", "/api/status")));
        // No decoding: an encoded slash stays inside the knock segment
        // and therefore fails the comparison, rather than being decoded
        // into a path separator afterwards.
        assert_eq!(split_knock("/ab%2Fc/api"), Some(("ab%2Fc", "/api")));
        assert_eq!(split_knock("/"), Some(("", "")));
        assert_eq!(split_knock("no-leading-slash"), None);
    }

    #[test]
    fn the_shell_is_the_only_artifact_and_it_is_behind_the_knock() {
        // If the shell ever became reachable without a knock, §3.2's
        // "no portal artifact pre-gate" is gone.
        let b = SHELL_HTML.to_ascii_lowercase();
        assert!(!b.is_empty());
        // And the parked answer must still mention none of it.
        let parked = Class::NotFound.body().to_ascii_lowercase();
        for forbidden in ["script", "aberp", "portal", "invoice", "knock", "webauthn"] {
            assert!(
                !parked.contains(forbidden),
                "the parked answer mentions `{forbidden}`"
            );
        }
    }

    #[test]
    fn a_session_cookie_is_scoped_to_the_knock_prefix() {
        let got = scope_cookie("s=abc; Secure; HttpOnly; SameSite=Strict", "KNOCK");
        assert_eq!(
            got,
            "s=abc; Secure; HttpOnly; SameSite=Strict; Path=/KNOCK/"
        );
        assert!(
            !got.contains("Path=/;"),
            "a Path=/ cookie is offered to the un-knocked surface too"
        );
    }

    #[test]
    fn an_agent_supplied_path_is_replaced_not_appended() {
        // The agent cannot know the right prefix — the knock is
        // stripped before the request reaches it.
        let got = scope_cookie("s=abc; Path=/; HttpOnly", "K");
        assert_eq!(got, "s=abc; HttpOnly; Path=/K/");
        assert_eq!(got.matches("Path=").count(), 1);
        let got = scope_cookie("s=abc; path=/wrong; HttpOnly", "K");
        assert_eq!(got, "s=abc; HttpOnly; Path=/K/", "case-insensitively");
    }

    #[test]
    fn an_unknown_status_gets_a_fixed_reason_phrase() {
        assert_eq!(reason_for(200), "OK");
        assert_eq!(reason_for(418), "OK");
        assert_eq!(reason_for(401), "Unauthorized");
    }

    #[test]
    fn a_body_the_relay_cannot_decode_becomes_the_parked_answer() {
        // Not a 502: that would confirm something is behind this host.
        let res = aberp_portal_core::PortalResponse {
            status: 200,
            content_type: "application/json".into(),
            body_b64: "!!!not base64!!!".into(),
            set_cookie: None,
        };
        assert!(matches!(render(&res, "k"), Answer::Nginx(Class::NotFound)));
    }
}
