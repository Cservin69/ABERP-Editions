//! The canary trap — shared observation types and the classifier.
//!
//! # The premise
//!
//! ADR-0113 §G2 says an unauthenticated observer must not be able to
//! establish that the portal exists. The flip side is the useful one:
//! **the host has no legitimate unauthenticated traffic at all.** It is
//! never linked, never crawled, never referenced. So any request that
//! arrives without a valid knock token is, by construction, a probe.
//!
//! That makes a canary trap almost free — there is no false-positive
//! population of ordinary visitors to sift out.
//!
//! # The one rule
//!
//! **The trap must never change the response.** A probe that trips the
//! canary receives the byte-identical uniform 404 that every other
//! probe receives, in the same shape of time. If tripping the wire
//! were observable, the wire would itself be the fingerprint §3.2
//! forbids — and a scanner would learn more from finding the trap than
//! Ervin learns from it firing.
//!
//! Everything here is therefore *pure*: [`classify`] takes what was
//! observed and returns a severity. It performs no I/O, allocates
//! nothing per call beyond the answer, and is invoked identically for
//! every request. The recording, coalescing and alerting all happen on
//! another task, off the response path.
//!
//! # Where the parts live
//!
//! - **Front (VPS)** observes and classifies, keeps nothing on disk,
//!   coalesces into batches and pushes them down the existing tunnel.
//! - **Agent (Mac)** receives batches, writes the rotating probe log,
//!   rate-limits, and sends the alert through the SMTP SPOC.
//!
//! The alert is sent from the **Mac**, never the VPS. Putting SMTP
//! credentials on the relay would be the single largest regression
//! available to this build: §2.4 makes "no authentication material at
//! rest on the VPS" absolute, and ADR-0047 makes the keychain the only
//! home for the SMTP password. The tunnel already exists and already
//! runs in the right direction, so the canary rides it.

use serde::{Deserialize, Serialize};

/// The compiled-in default decoy path.
///
/// A resource **no legitimate flow ever references**: it is not in the
/// shell, not in any redirect, not in `robots.txt` (there is no
/// `robots.txt`), and not reachable from anywhere. Nothing but a
/// directory brute-forcer or someone acting on leaked knowledge will
/// ever ask for it, so a hit is unambiguous.
///
/// Chosen to be attractive to scanners while naming nothing: it does
/// not contain "aberp", "portal", "invoice" or any other word that
/// would confirm what runs here if the path were ever observed. As
/// with every other request, the answer is the uniform 404.
///
/// Overridable at deploy time via `PORTAL_TRIPWIRE_PATH` on the agent,
/// which pushes it to the relay in the tunnel handshake — so rotating
/// the decoy needs no relay redeploy and leaves no value in this repo.
pub const DEFAULT_TRIPWIRE_PATH: &str = "/admin/config.backup";

/// How a probe was classified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Not a probe: this source passed the knock moments ago, so this
    /// is almost certainly the operator's own browser making an
    /// automatic request (`/favicon.ico`, `/apple-touch-icon.png`)
    /// against the bare host. Counted, never alerted.
    Suppressed,
    /// Internet background noise: reached the IP, did not name the
    /// portal's hostname, asked for nothing meaningful. The whole
    /// internet does this to every IP all day.
    Low,
    /// Someone knows something. Either they addressed the portal by
    /// its label, or they asked for something whose *shape* is
    /// portal-specific, or they hit the decoy.
    High,
}

impl Severity {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Suppressed => "suppressed",
            Self::Low => "low",
            Self::High => "high",
        }
    }
}

/// Why a probe was classified the way it was. Fixed vocabulary — these
/// strings reach the probe log and the alert, so none of them may
/// carry attacker-chosen text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// The source passed the knock within the grace window.
    RecentlyAuthorised,
    /// The decoy resource. Unambiguous.
    Tripwire,
    /// The request named the portal's hostname in `Host`. The label is
    /// supposed to be known only to Ervin's bookmark.
    NamedTheHost,
    /// The first path segment has the exact shape of a knock token but
    /// is not one — somebody is guessing tokens.
    KnockShaped,
    /// The path is portal-API-shaped. The API is only reachable behind
    /// the knock, so its shape should not be known.
    ApiShaped,
    /// None of the above.
    BackgroundNoise,
}

impl Reason {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RecentlyAuthorised => "recently authorised source",
            Self::Tripwire => "decoy resource requested",
            Self::NamedTheHost => "request named the portal hostname",
            Self::KnockShaped => "knock-shaped path segment",
            Self::ApiShaped => "portal-API-shaped path",
            Self::BackgroundNoise => "internet background noise",
        }
    }

    #[must_use]
    pub fn severity(self) -> Severity {
        match self {
            Self::RecentlyAuthorised => Severity::Suppressed,
            Self::Tripwire | Self::NamedTheHost | Self::KnockShaped | Self::ApiShaped => {
                Severity::High
            }
            Self::BackgroundNoise => Severity::Low,
        }
    }
}

/// What the front observed. Everything here is attacker-controlled
/// except `recently_authorised` and `matched_expected_host`.
#[derive(Debug, Clone)]
pub struct ProbeInput<'a> {
    /// Request path, as received. Never decoded.
    pub path: &'a str,
    /// `true` iff the `Host` header equalled the hostname the agent
    /// published for this tunnel.
    pub matched_expected_host: bool,
    /// `true` iff the path is exactly the tripwire.
    pub tripwire: bool,
    /// `true` iff this source passed the knock inside the grace window.
    pub recently_authorised: bool,
}

/// Length of a knock token in its base64url form — 32 random bytes.
/// A first path segment of exactly this length and charset is a token
/// guess, not a stray crawl.
pub const KNOCK_TOKEN_CHARS: usize = 43;

/// Classify one probe. Pure, allocation-free, and identical work for
/// every request — see the module docs on why that matters.
#[must_use]
pub fn classify(input: &ProbeInput<'_>) -> Reason {
    // Checked first so the operator's own browser fetching
    // `/favicon.ico` against the bare host never pages anyone. It also
    // means a suppressed source cannot be *escalated* by what it asks
    // for, which is the trade named in the residuals: an attacker
    // sharing Ervin's egress IP inside the grace window is suppressed
    // too.
    if input.recently_authorised {
        return Reason::RecentlyAuthorised;
    }
    if input.tripwire {
        return Reason::Tripwire;
    }
    if input.matched_expected_host {
        return Reason::NamedTheHost;
    }
    if first_segment_is_knock_shaped(input.path) {
        return Reason::KnockShaped;
    }
    if input.path.starts_with("/api/") || input.path.contains("/api/") {
        return Reason::ApiShaped;
    }
    Reason::BackgroundNoise
}

fn first_segment_is_knock_shaped(path: &str) -> bool {
    let Some(rest) = path.strip_prefix('/') else {
        return false;
    };
    let seg = rest.split('/').next().unwrap_or("");
    seg.len() == KNOCK_TOKEN_CHARS
        && seg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// One probe, as it appears in the probe log and the alert.
///
/// Metadata only, and structurally so: there is no field that can hold
/// a request body, a cookie, a query string or a token — the same
/// discipline `aberp-portal-agent`'s audit record follows, for the same
/// reason. The one attacker-controlled string that survives (`path`,
/// `user_agent`) is sanitised and truncated by [`sanitise`] before it
/// is ever stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeSample {
    /// RFC-3339 UTC, stamped by the front.
    pub at: String,
    pub severity: Severity,
    pub reason: Reason,
    /// Source address as the front saw it.
    pub source_ip: String,
    /// Method, uppercase.
    pub method: String,
    /// Sanitised, truncated path.
    pub path: String,
    /// Sanitised, truncated `User-Agent`, if any.
    pub user_agent: Option<String>,
    /// Whether `Host` matched the portal's label.
    ///
    /// Deliberately a **boolean, not the hostname**. The alert this
    /// feeds travels by email, and email is not end-to-end encrypted;
    /// writing the label Ervin took care to keep out of Certificate
    /// Transparency into a mailbox would undo that control. Ervin knows
    /// his own hostname — what he needs told is that somebody else
    /// used it.
    pub named_the_host: bool,
    /// TLS SNI, when the listener can supply it.
    ///
    /// Always `None` today. See the residual in
    /// `aberp-portal-relay::canary`: recovering SNI (and any JA3-style
    /// client fingerprint) needs a custom TLS acceptor rather than
    /// `axum-server`'s, which is Phase-2 work. `named_the_host` covers
    /// most of what SNI would tell us, with the caveat that `Host` is
    /// client-controlled and SNI is observed.
    pub sni: Option<String>,
}

/// Strip control characters and truncate. Everything an attacker
/// controls goes through here before it is written anywhere.
///
/// CR and LF are the reason this exists and not merely the tidiness:
/// the probe log is line-delimited JSON and the alert becomes an email,
/// so an unescaped newline in a `User-Agent` is a log-forging and
/// header-injection primitive handed to exactly the population this
/// code exists to watch. (`serde_json` would escape them; this makes
/// the guarantee independent of that.)
#[must_use]
pub fn sanitise(raw: &str, max: usize) -> String {
    let mut out: String = raw
        .chars()
        .map(|c| if c.is_control() { '\u{fffd}' } else { c })
        .take(max)
        .collect();
    if raw.chars().count() > max {
        out.push('…');
    }
    out
}

/// A coalesced report: everything the front saw in one window.
///
/// Batching is what keeps a scan burst to one alert. A `/16` sweep can
/// produce thousands of probes a second; the agent must learn *that it
/// happened*, not receive it one message at a time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanaryBatch {
    /// RFC-3339 UTC of the first and last observation in the window.
    pub window_start: String,
    pub window_end: String,
    /// Total probes observed, including suppressed ones.
    pub total: u64,
    pub high: u64,
    pub low: u64,
    pub suppressed: u64,
    /// How many distinct source addresses.
    pub distinct_sources: u64,
    /// Observations dropped because the queue was full — a scan large
    /// enough to overrun the buffer is itself a finding, so the count
    /// is reported rather than hidden.
    pub dropped: u64,
    /// A capped selection of individual probes, worst-first.
    pub samples: Vec<ProbeSample>,
}

impl CanaryBatch {
    /// The highest severity present.
    #[must_use]
    pub fn severity(&self) -> Severity {
        if self.high > 0 {
            Severity::High
        } else if self.low > 0 {
            Severity::Low
        } else {
            Severity::Suppressed
        }
    }

    /// `true` iff anything in here is worth telling Ervin about.
    #[must_use]
    pub fn is_reportable(&self) -> bool {
        self.high > 0 || self.low > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(path: &str) -> ProbeInput<'_> {
        ProbeInput {
            path,
            matched_expected_host: false,
            tripwire: false,
            recently_authorised: false,
        }
    }

    #[test]
    fn background_noise_is_low() {
        for p in ["/", "/wp-login.php", "/.env", "/index.html", "/favicon.ico"] {
            assert_eq!(classify(&input(p)), Reason::BackgroundNoise, "{p}");
            assert_eq!(classify(&input(p)).severity(), Severity::Low);
        }
    }

    #[test]
    fn the_tripwire_is_high_and_unambiguous() {
        let mut i = input(DEFAULT_TRIPWIRE_PATH);
        i.tripwire = true;
        assert_eq!(classify(&i), Reason::Tripwire);
        assert_eq!(classify(&i).severity(), Severity::High);
    }

    #[test]
    fn naming_the_host_is_high() {
        // Someone typed the label. The whole point of §3.2 is that
        // nobody should be able to.
        let mut i = input("/");
        i.matched_expected_host = true;
        assert_eq!(classify(&i), Reason::NamedTheHost);
        assert_eq!(classify(&i).severity(), Severity::High);
    }

    #[test]
    fn a_knock_shaped_segment_is_high() {
        let guess = "A".repeat(KNOCK_TOKEN_CHARS);
        assert_eq!(classify(&input(&format!("/{guess}"))), Reason::KnockShaped);
        assert_eq!(
            classify(&input(&format!("/{guess}/api/status"))),
            Reason::KnockShaped
        );
        // One character short is not the shape of a token.
        let near = "A".repeat(KNOCK_TOKEN_CHARS - 1);
        assert_eq!(
            classify(&input(&format!("/{near}"))),
            Reason::BackgroundNoise
        );
        // Right length, wrong charset.
        let bad = ".".repeat(KNOCK_TOKEN_CHARS);
        assert_eq!(
            classify(&input(&format!("/{bad}"))),
            Reason::BackgroundNoise
        );
    }

    #[test]
    fn an_api_shaped_path_is_high() {
        assert_eq!(classify(&input("/api/invoices")), Reason::ApiShaped);
        assert_eq!(classify(&input("/x/api/status")), Reason::ApiShaped);
    }

    #[test]
    fn a_recently_authorised_source_is_suppressed_whatever_it_asks_for() {
        // The operator's own browser fetching /favicon.ico against the
        // bare host must not page anyone at 02:00.
        let mut i = input("/favicon.ico");
        i.recently_authorised = true;
        i.matched_expected_host = true;
        assert_eq!(classify(&i), Reason::RecentlyAuthorised);
        assert_eq!(classify(&i).severity(), Severity::Suppressed);
        // Even the tripwire — see the residual note in `classify`.
        i.tripwire = true;
        assert_eq!(classify(&i), Reason::RecentlyAuthorised);
    }

    #[test]
    fn sanitise_strips_newlines_and_truncates() {
        assert_eq!(sanitise("a\r\nb", 10), "a\u{fffd}\u{fffd}b");
        assert_eq!(sanitise("abcdef", 3), "abc…");
        assert_eq!(sanitise("abc", 3), "abc");
        assert_eq!(sanitise("a\0b", 10), "a\u{fffd}b");
    }

    #[test]
    fn batch_severity_is_the_worst_present() {
        let mut b = CanaryBatch {
            window_start: "x".into(),
            window_end: "y".into(),
            total: 3,
            high: 0,
            low: 0,
            suppressed: 3,
            distinct_sources: 1,
            dropped: 0,
            samples: Vec::new(),
        };
        assert_eq!(b.severity(), Severity::Suppressed);
        assert!(!b.is_reportable());
        b.low = 1;
        assert_eq!(b.severity(), Severity::Low);
        assert!(b.is_reportable());
        b.high = 1;
        assert_eq!(b.severity(), Severity::High);
    }
}
