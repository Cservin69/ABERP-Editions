//! The front (ADR-0113 §3) — one handler, two possible answers.
//!
//! Every request that reaches this process, by any method, on any path,
//! with any headers, gets exactly one of:
//!
//! 1. the **uniform 404** ([`UNIFORM_404_BODY`]) — byte-identical
//!    whether the portal exists, the Mac is offline, the knock is
//!    wrong, or the path is garbage; or
//! 2. the portal, because the request carried the current knock token.
//!
//! # Why this is a single fallback handler and not a route table
//!
//! A `Router` with real routes answers `405 Method Not Allowed` for a
//! known path with the wrong verb, `404` for an unknown one, and may
//! vary headers between them. Each of those differences is a bit of
//! information about whether a path exists — exactly what §3.2 forbids:
//!
//! > every unauthenticated request — wrong path, right path, `HEAD`,
//! > `POST`, garbage SNI, direct IP — receives the same minimal 404:
//! > same status, same headers […] same body bytes.
//!
//! So there is no route table. One handler sees everything, and the
//! only branch that can produce something other than the uniform 404 is
//! a constant-time knock comparison against a token the Mac supplied.
//!
//! # And when the Mac is gone
//!
//! [`Broker::knock_matches`] answers `false` when no agent is
//! connected, so a tunnel outage collapses the portal to the uniform
//! 404 for everyone — including a correctly-bookmarked, fully-enrolled
//! Ervin. Ervin's §9.5 decision: "keep the pure 404 (unreachable =
//! invisible, no exceptions)".

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{ConnectInfo, DefaultBodyLimit, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Router;

use aberp_portal_core::proto::PortalRequest;

use crate::broker::Broker;

/// The one body an unauthenticated caller ever sees.
///
/// Shaped like the default 404 of the commonest parked-host server so
/// the portal blends into the background of the internet rather than
/// standing out as "something custom is running here" (§3.2: "The
/// identical response is also what the VPS's default vhost returns").
pub const UNIFORM_404_BODY: &str = "<html>\r
<head><title>404 Not Found</title></head>\r
<body>\r
<center><h1>404 Not Found</h1></center>\r
</body>\r
</html>\r
";

/// The `Server` header, sent on **every** response.
///
/// Uniform on purpose: a header that appeared only on portal responses
/// would be a discriminator all by itself. §3.2 asks for "a bare,
/// common server line".
pub const SERVER_HEADER: &str = "nginx";

/// The portal shell, compiled in. Served only to a caller that passed
/// the knock — §3.2's "No portal artifact pre-gate: no favicon, no JS
/// bundle, no manifest […] the app shell literally is not served".
pub const SHELL_HTML: &str = include_str!("../assets/shell.html");

/// Largest request body the front will read.
///
/// The only bodies the portal has are WebAuthn ceremony JSON — a few
/// kilobytes at most. A relay that buffers whatever it is sent is an
/// OOM waiting to happen on a box whose entire job is to hold nothing
/// (§2.4), so the limit is explicit rather than inherited from a
/// framework default that a future upgrade could change.
pub const MAX_REQUEST_BODY: usize = 64 * 1024;

/// Front state.
#[derive(Debug)]
pub struct Front {
    pub broker: Arc<Broker>,
}

/// Build the router. One fallback, no routes — see the module docs.
pub fn router(front: Arc<Front>) -> Router {
    Router::new()
        .fallback(handle)
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY))
        .with_state(front)
}

async fn handle(
    State(front): State<Arc<Front>>,
    connect_info: Option<ConnectInfo<SocketAddr>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    // `Result<Bytes, _>` rather than `Bytes`: a bare extractor rejection
    // would be answered by axum itself, BEFORE the knock is checked, and
    // a `413` where everyone else gets a `404` is a discriminator. Taking
    // the rejection as a value keeps every answer inside this function.
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let path = uri.path();
    let Some((knock, rest)) = split_knock(path) else {
        return uniform_404();
    };
    if !front.broker.knock_matches(knock) {
        return uniform_404();
    }

    // Past the gate. The shell, or a forwarded API call — nothing else.
    match rest {
        "" | "/" => shell(),
        api if api.starts_with("/api/") => match body {
            Ok(body) => {
                forward(
                    &front,
                    connect_info,
                    &method,
                    api,
                    uri.query(),
                    &headers,
                    body,
                )
                .await
            }
            // Oversized or unreadable body from a knocked caller: still
            // the uniform 404, so the limit is not an oracle either.
            Err(e) => {
                tracing::info!(reason = %e, "request body refused");
                uniform_404()
            }
        },
        // A knocked caller asking for a path the portal does not have
        // gets the same 404 as a stranger. There is no "page not found"
        // page to learn the shape of the app from.
        _ => uniform_404(),
    }
}

/// Split `/<knock>/rest` into `("<knock>", "/rest")`.
///
/// Purely lexical: no percent-decoding, no normalisation, no `..`
/// collapsing. Whatever the client sent is what gets compared, so
/// there is no decoded-vs-compared discrepancy for an attacker to
/// wedge apart.
fn split_knock(path: &str) -> Option<(&str, &str)> {
    let trimmed = path.strip_prefix('/')?;
    match trimmed.find('/') {
        Some(i) => Some((&trimmed[..i], &trimmed[i..])),
        None => Some((trimmed, "")),
    }
}

async fn forward(
    front: &Arc<Front>,
    connect_info: Option<ConnectInfo<SocketAddr>>,
    method: &Method,
    path: &str,
    query: Option<&str>,
    headers: &HeaderMap,
    body: Bytes,
) -> Response {
    use base64::Engine as _;

    let req = PortalRequest {
        // Verbatim. The relay has no opinion about verbs; §6.3 puts
        // that opinion on the Mac.
        method: method.as_str().to_string(),
        path: path.to_string(),
        query: query.map(str::to_string),
        cookie: headers
            .get(axum::http::header::COOKIE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
        body_b64: (!body.is_empty())
            .then(|| base64::engine::general_purpose::STANDARD.encode(&body)),
        peer: connect_info.map(|ConnectInfo(a)| a.ip().to_string()),
    };

    match front.broker.dispatch(req).await {
        Ok(res) => render(&res),
        // Every failure to reach the Mac collapses to the uniform 404.
        // A "502 Bad Gateway" here would confirm that something exists
        // behind this host — the one thing §G2 forbids.
        Err(e) => {
            tracing::info!(reason = ?e, "dispatch failed; answering the uniform 404");
            uniform_404()
        }
    }
}

fn render(res: &aberp_portal_core::PortalResponse) -> Response {
    let Some(body) = res.body() else {
        return uniform_404();
    };
    let status = StatusCode::from_u16(res.status).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut response = (status, body).into_response();
    let h = response.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&res.content_type) {
        h.insert(axum::http::header::CONTENT_TYPE, v);
    }
    // The cookie was minted on the Mac; the relay only carries it.
    if let Some(cookie) = &res.set_cookie {
        if let Ok(v) = HeaderValue::from_str(cookie) {
            h.insert(axum::http::header::SET_COOKIE, v);
        }
    }
    stamp_common_headers(h);
    response
}

fn shell() -> Response {
    let mut response = (StatusCode::OK, SHELL_HTML).into_response();
    let h = response.headers_mut();
    h.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    // The shell must never be cached: it is the only artifact that
    // proves the portal exists, and a cached copy in a shared browser
    // would outlive the knock rotation that was supposed to revoke it.
    h.insert(
        axum::http::header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    );
    stamp_common_headers(h);
    response
}

/// The uniform 404. Every field is fixed; nothing about the request
/// influences any byte of it.
#[must_use]
pub fn uniform_404() -> Response {
    let mut response = (StatusCode::NOT_FOUND, UNIFORM_404_BODY).into_response();
    let h = response.headers_mut();
    h.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("text/html"),
    );
    stamp_common_headers(h);
    response
}

fn stamp_common_headers(h: &mut HeaderMap) {
    h.insert(
        axum::http::header::SERVER,
        HeaderValue::from_static(SERVER_HEADER),
    );
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
    fn the_uniform_404_body_carries_no_portal_artifact() {
        // If this ever grows a link, a script tag, a token or a name,
        // the whole §3.2 posture is gone.
        let b = UNIFORM_404_BODY.to_ascii_lowercase();
        for forbidden in ["script", "aberp", "portal", "invoice", "knock", "webauthn"] {
            assert!(
                !b.contains(forbidden),
                "the uniform 404 mentions `{forbidden}`"
            );
        }
    }
}
