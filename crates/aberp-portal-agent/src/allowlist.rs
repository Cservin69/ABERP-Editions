//! Read-only, enforced at the agent (ADR-0113 §6.3).
//!
//! > The allowlist is compiled into the agent as **(method, exact route
//! > shape)** pairs — the four rows above, `GET` only. Everything else
//! > is refused *at the agent, on the Mac, inside the trust boundary*:
//! > any non-`GET` verb, any unlisted path, any query-string smuggling
//! > of a different route.
//!
//! Three design choices make that refusal hard to talk around, and each
//! answers a line in §"Adversarial review" ("route-shape matching vs
//! axum's actual path normalization — smuggling via encoding is the
//! classic hole"):
//!
//! 1. **The parser never percent-decodes.** A path containing `%` is
//!    refused outright. There is therefore no decode step whose output
//!    could differ from what the matcher inspected — the whole class of
//!    `%2e%2e%2f` and double-encoding tricks has nowhere to live.
//! 2. **The invoice id is charset-restricted, not merely
//!    delimiter-checked.** `[A-Za-z0-9_-]{1,64}` and nothing else, so
//!    `.`, `/`, `\`, `:`, `?`, `#`, NUL and every multibyte sequence
//!    are refused before the id reaches a URL.
//! 3. **The query string is dropped, not forwarded.** None of the four
//!    upstream routes takes a query parameter (the tenant is bound in
//!    `serve.rs`'s `AppState` at boot), so there is no reason to carry
//!    one and every reason not to.
//!
//! The upstream URL is *rebuilt* from the matched shape rather than
//! passed through, so what the allowlist approved and what Leg C
//! requests cannot drift apart.

/// The four `GET` routes of `apps/aberp/src/serve.rs` that Phase 0/1
/// exposes (ADR-0113 §6.2). Verbatim — `tests/route_drift.rs` asserts
/// they still exist in `serve.rs` so a rename upstream fails the build
/// rather than the portal at runtime (§7: "drift fails *closed*").
pub const UPSTREAM_ROUTES: [&str; 4] =
    ["/health", "/invoices", "/invoices/:id", "/invoices/:id/pdf"];

/// The one method the portal may use.
pub const ALLOWED_METHOD: &str = "GET";

/// What the agent decided to do with a proxied request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Proxy this exact upstream path (no query, `GET`).
    Allow { upstream_path: String },
    /// Refuse, with a fixed reason for the audit log.
    Refuse(Refusal),
}

/// Why a request was refused. A closed vocabulary: these strings reach
/// the audit log (§6.5) and must never carry attacker-chosen input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// A mutating verb — the §G5 refusal.
    MethodNotAllowed,
    /// Not one of the four shapes.
    PathNotAllowed,
    /// The path carried a percent-escape.
    EncodedPath,
    /// The invoice id was outside the permitted charset or too long.
    BadInvoiceId,
    /// A query string was present; the allowlisted routes take none.
    QueryNotAllowed,
}

impl Refusal {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MethodNotAllowed => "method not allowed (read-only portal)",
            Self::PathNotAllowed => "path not on the allowlist",
            Self::EncodedPath => "percent-encoded path refused",
            Self::BadInvoiceId => "invoice id outside the permitted charset",
            Self::QueryNotAllowed => "query string not permitted on this route",
        }
    }

    /// The HTTP status the shell sees. `405` for a verb, `404`
    /// otherwise — the portal is already behind the knock and a
    /// session, so distinguishing these to an *authenticated* caller
    /// costs nothing (§3.2's indistinguishability is a property of the
    /// UNauthenticated surface, which never reaches this code).
    #[must_use]
    pub fn status(self) -> u16 {
        match self {
            Self::MethodNotAllowed => 405,
            _ => 404,
        }
    }
}

/// Literal segments that appear as siblings of `:id` under
/// `/invoices/` in `serve.rs` — today just `issue`
/// (`POST /invoices/issue`). They are refused as invoice ids so the
/// agent never *builds* a URL that collides with a mutating route.
///
/// Upstream would answer such a `GET` with axum's own `405`, so this is
/// belt-and-braces rather than a hole being closed. It is here because
/// §6.3's promise is that the refusal happens on the Mac, and "the
/// other end would have refused it" is exactly the reasoning that
/// promise exists to avoid relying on.
const RESERVED_INVOICE_SEGMENTS: [&str; 1] = ["issue"];

/// Longest permitted invoice id. ABERP invoice ids are ULID- and
/// number-shaped; 64 is generous and bounds the URL the agent builds.
const MAX_INVOICE_ID: usize = 64;

/// Decide what to do with a browser request that already carried a
/// valid session.
///
/// `path` is the portal-side path with the knock prefix stripped —
/// `/api/invoices/01J.../pdf`.
#[must_use]
pub fn decide(method: &str, path: &str, query: Option<&str>) -> Decision {
    if method != ALLOWED_METHOD {
        return Decision::Refuse(Refusal::MethodNotAllowed);
    }
    if query.is_some_and(|q| !q.is_empty()) {
        return Decision::Refuse(Refusal::QueryNotAllowed);
    }
    if path.contains('%') {
        return Decision::Refuse(Refusal::EncodedPath);
    }

    let Some(rest) = path.strip_prefix("/api/") else {
        return Decision::Refuse(Refusal::PathNotAllowed);
    };
    let segments: Vec<&str> = rest.split('/').collect();
    match segments.as_slice() {
        ["health"] => Decision::Allow {
            upstream_path: "/health".to_string(),
        },
        ["invoices"] => Decision::Allow {
            upstream_path: "/invoices".to_string(),
        },
        ["invoices", id] => match invoice_id(id) {
            Ok(id) => Decision::Allow {
                upstream_path: format!("/invoices/{id}"),
            },
            Err(r) => Decision::Refuse(r),
        },
        ["invoices", id, "pdf"] => match invoice_id(id) {
            Ok(id) => Decision::Allow {
                upstream_path: format!("/invoices/{id}/pdf"),
            },
            Err(r) => Decision::Refuse(r),
        },
        _ => Decision::Refuse(Refusal::PathNotAllowed),
    }
}

fn invoice_id(raw: &str) -> Result<&str, Refusal> {
    if raw.is_empty() || raw.len() > MAX_INVOICE_ID {
        return Err(Refusal::BadInvoiceId);
    }
    if RESERVED_INVOICE_SEGMENTS.contains(&raw) {
        return Err(Refusal::BadInvoiceId);
    }
    if raw
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        Ok(raw)
    } else {
        Err(Refusal::BadInvoiceId)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed(path: &str) -> Option<String> {
        match decide("GET", path, None) {
            Decision::Allow { upstream_path } => Some(upstream_path),
            Decision::Refuse(_) => None,
        }
    }

    fn refusal(method: &str, path: &str, query: Option<&str>) -> Refusal {
        match decide(method, path, query) {
            Decision::Refuse(r) => r,
            Decision::Allow { upstream_path } => {
                panic!("expected a refusal, got an allow for {upstream_path}")
            }
        }
    }

    #[test]
    fn the_four_allowlisted_shapes_map_to_the_serve_routes() {
        assert_eq!(allowed("/api/health").as_deref(), Some("/health"));
        assert_eq!(allowed("/api/invoices").as_deref(), Some("/invoices"));
        assert_eq!(
            allowed("/api/invoices/INV-2026-001").as_deref(),
            Some("/invoices/INV-2026-001")
        );
        assert_eq!(
            allowed("/api/invoices/INV-2026-001/pdf").as_deref(),
            Some("/invoices/INV-2026-001/pdf")
        );
    }

    #[test]
    fn every_mutating_verb_is_refused() {
        // §G5: "Phase 1 is read-only by construction". This is the test
        // that says so.
        for m in [
            "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS", "TRACE", "CONNECT", "get",
        ] {
            assert_eq!(
                refusal(m, "/api/invoices", None),
                Refusal::MethodNotAllowed,
                "method {m} was not refused"
            );
        }
    }

    #[test]
    fn mutating_serve_routes_are_unreachable_even_with_get() {
        // The routes that DO mutate in serve.rs — issue, submit, storno,
        // snapshot, tenant switch. None is on the allowlist, so even the
        // allowed verb cannot reach them.
        for p in [
            "/api/invoices/INV-1/submit",
            "/api/invoices/INV-1/poll-ack",
            "/api/api/invoices/INV-1/storno",
            "/api/snapshots/now",
            "/api/tenants/x/switch",
        ] {
            assert_eq!(
                refusal("GET", p, None),
                Refusal::PathNotAllowed,
                "{p} leaked"
            );
        }
        // `issue` is a legal id *shape*, so it is caught by the reserved
        // -segment rule rather than by shape matching.
        assert_eq!(
            refusal("GET", "/api/invoices/issue", None),
            Refusal::BadInvoiceId
        );
    }

    #[test]
    fn percent_encoding_is_refused_outright() {
        // The §"Adversarial review" hole, closed by refusing to decode.
        for p in [
            "/api/invoices/%2e%2e%2fhealth",
            "/api/invoices/INV%2F1/pdf",
            "/api/%69nvoices",
            "/api/invoices/%00",
        ] {
            assert_eq!(refusal("GET", p, None), Refusal::EncodedPath, "{p} leaked");
        }
    }

    #[test]
    fn traversal_and_delimiter_smuggling_in_the_invoice_id_is_refused() {
        for p in [
            "/api/invoices/..",
            "/api/invoices/a.b",
            "/api/invoices/a:b",
            "/api/invoices/a b",
            "/api/invoices/a\\b",
            "/api/invoices/a#b",
            "/api/invoices/árvíztűrő",
        ] {
            assert_eq!(refusal("GET", p, None), Refusal::BadInvoiceId, "{p} leaked");
        }
    }

    #[test]
    fn an_over_long_invoice_id_is_refused() {
        let long = "a".repeat(MAX_INVOICE_ID + 1);
        assert_eq!(
            refusal("GET", &format!("/api/invoices/{long}"), None),
            Refusal::BadInvoiceId
        );
    }

    #[test]
    fn an_empty_invoice_id_is_refused_rather_than_collapsing_to_the_list() {
        assert_eq!(
            refusal("GET", "/api/invoices/", None),
            Refusal::BadInvoiceId
        );
        assert_eq!(
            refusal("GET", "/api/invoices//pdf", None),
            Refusal::BadInvoiceId
        );
    }

    #[test]
    fn a_query_string_is_refused_not_stripped() {
        // Refused rather than silently dropped: a shell that started
        // sending a query is a shell whose author expected it to mean
        // something, and a silent drop would hide that.
        assert_eq!(
            refusal("GET", "/api/invoices", Some("tenant=prod")),
            Refusal::QueryNotAllowed
        );
        // An empty query is the same as none — `?` with nothing after it
        // is what some clients emit for a bare URL.
        assert!(matches!(
            decide("GET", "/api/invoices", Some("")),
            Decision::Allow { .. }
        ));
    }

    #[test]
    fn paths_outside_the_api_prefix_are_refused() {
        for p in ["/health", "/invoices", "/", "/api", "/apix/health", ""] {
            assert_eq!(
                refusal("GET", p, None),
                Refusal::PathNotAllowed,
                "{p} leaked"
            );
        }
    }

    #[test]
    fn deeper_paths_under_an_allowed_prefix_are_refused() {
        for p in [
            "/api/invoices/INV-1/pdf/extra",
            "/api/health/sub",
            "/api/invoices/INV-1/audit",
            // Traversal that survives as a THREE-segment path fails the
            // shape match, not the charset check — both are refusals.
            "/api/invoices/../health",
        ] {
            assert_eq!(
                refusal("GET", p, None),
                Refusal::PathNotAllowed,
                "{p} leaked"
            );
        }
    }

    #[test]
    fn refusal_reasons_never_echo_the_input() {
        // The audit log takes these strings; they must be a closed
        // vocabulary, not attacker-shaped text (§6.5).
        for r in [
            Refusal::MethodNotAllowed,
            Refusal::PathNotAllowed,
            Refusal::EncodedPath,
            Refusal::BadInvoiceId,
            Refusal::QueryNotAllowed,
        ] {
            assert!(!r.as_str().is_empty());
        }
        assert_eq!(Refusal::MethodNotAllowed.status(), 405);
        assert_eq!(Refusal::PathNotAllowed.status(), 404);
    }
}
