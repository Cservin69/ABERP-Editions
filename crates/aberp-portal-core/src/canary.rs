//! The canary trap — shared observation types and the classifier.
//!
//! # The premise
//!
//! ADR-0115 §G2 says an unauthenticated observer must not be able to
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
//!   coalesces into batches and hands them to the next poll response.
//! - **Agent (Mac)** receives batches, writes the rotating probe log,
//!   rate-limits, and sends the alert through the SMTP SPOC.
//!
//! The alert is sent from the **Mac**, never the VPS. Putting SMTP
//! credentials on the relay would be the single largest regression
//! available to this build: §2.4 makes "no authentication material at
//! rest on the VPS" absolute, and ADR-0047 makes the keychain the only
//! home for the SMTP password. The poll loop already runs and already
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
/// which publishes it to the relay on every poll — so rotating
/// the decoy needs no relay redeploy and leaves no value in this repo.
pub const DEFAULT_TRIPWIRE_PATH: &str = "/admin/config.backup";

/// How a probe was classified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Not a probe: this source passed the knock moments ago AND
    /// asked for one of the fixed paths a browser fetches on its own
    /// (`/favicon.ico`, `/apple-touch-icon*.png`, `/manifest.json`)
    /// against the bare host. Counted, never alerted. Nothing else a
    /// knocked source asks for lands here.
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
    /// The source passed the knock within the grace window AND
    /// asked for one of the handful of paths a browser fetches by
    /// itself. See [`AUTHORISED_CHATTER_PATHS`] — this is a path
    /// exemption, not a licence for the source.
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
    /// published for this presence lease.
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

/// The **only** paths a recently-authorised source may ask for without
/// being classified as a probe.
///
/// This list exists for one concrete false positive and nothing else: a
/// browser that has just loaded the portal will, entirely on its own,
/// ask the **bare host** for these. They carry no knock and they DO
/// carry the portal's hostname, so without an exemption every
/// legitimate visit would page Ervin at HIGH severity — and an alert
/// that fires on normal use is an alert that gets ignored.
///
/// It is a fixed *path* allowlist rather than a blanket per-source
/// suppression, and the difference is the whole point. A blanket
/// suppression meant a source that had passed the knock in the last
/// five minutes could ask for **anything** — including the decoy —
/// without raising a thing, and because each knock renewed the window,
/// a knocked source hammering the tripwire produced tens of thousands
/// of hits and zero alerts. Whoever holds the knock token is exactly
/// the population worth watching once they start asking for things the
/// portal does not have.
pub const AUTHORISED_CHATTER_PATHS: [&str; 2] = ["/favicon.ico", "/manifest.json"];

/// Longest path accepted by the `apple-touch-icon` family match. iOS's
/// longest real form is `/apple-touch-icon-precomposed.png` at 32;
/// 64 is generous and bounds what the matcher will walk.
const MAX_CHATTER_PATH: usize = 64;

/// `true` iff `path` is one of the automatic requests a browser makes
/// against the bare host after loading the portal.
///
/// Exact for the fixed names; a bounded family match for iOS's
/// `apple-touch-icon` variants (`-precomposed`, `-120x120`, and the
/// cross product). Nothing here percent-decodes, nothing accepts a
/// second path segment, and the middle of the icon name is restricted
/// to `[A-Za-z0-9._-]` — so the exemption cannot be widened by a
/// crafted path.
#[must_use]
pub fn is_authorised_chatter(path: &str) -> bool {
    if path.len() > MAX_CHATTER_PATH {
        return false;
    }
    if AUTHORISED_CHATTER_PATHS.contains(&path) {
        return true;
    }
    const PREFIX: &str = "/apple-touch-icon";
    const SUFFIX: &str = ".png";
    let Some(rest) = path.strip_prefix(PREFIX) else {
        return false;
    };
    let Some(middle) = rest.strip_suffix(SUFFIX) else {
        return false;
    };
    middle
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
}

/// Classify one probe. Pure, allocation-free, and identical work for
/// every request — see the module docs on why that matters.
///
/// # Order of checks, and why it is this order
///
/// 1. **The tripwire first, before anything can suppress it.** Nothing
///    legitimate ever requests the decoy — not a crawler, not a
///    redirect, and emphatically not the operator's own browser, which
///    has never been told the path exists. A hit is unambiguous
///    regardless of who the source is, so no later branch gets to
///    silence it.
/// 2. **Then the narrow browser-chatter exemption**, for a
///    recently-authorised source only, and only for the exact paths in
///    [`AUTHORISED_CHATTER_PATHS`] (plus the `apple-touch-icon`
///    family). Everything else a knocked source asks for classifies
///    normally.
/// 3. Then the three "somebody knows something" signals, then noise.
#[must_use]
pub fn classify(input: &ProbeInput<'_>) -> Reason {
    if input.tripwire {
        return Reason::Tripwire;
    }
    if input.recently_authorised && is_authorised_chatter(input.path) {
        return Reason::RecentlyAuthorised;
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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
    fn a_recently_authorised_source_is_suppressed_only_for_browser_chatter() {
        // The operator's own browser fetching /favicon.ico against the
        // bare host must not page anyone at 02:00 …
        for chatter in [
            "/favicon.ico",
            "/manifest.json",
            "/apple-touch-icon.png",
            "/apple-touch-icon-precomposed.png",
            "/apple-touch-icon-120x120.png",
            "/apple-touch-icon-120x120-precomposed.png",
        ] {
            let mut i = input(chatter);
            i.recently_authorised = true;
            i.matched_expected_host = true;
            assert_eq!(classify(&i), Reason::RecentlyAuthorised, "{chatter}");
            assert_eq!(classify(&i).severity(), Severity::Suppressed, "{chatter}");
        }
    }

    #[test]
    fn a_recently_authorised_source_is_not_a_licence_to_probe() {
        // This is the flipped pin. The previous behaviour suppressed a
        // recently-authorised source WHATEVER it asked for, and since a
        // knock renewed the window, a knocked source hammering the decoy
        // produced 21,600 hits and zero alerts. Everything outside the
        // chatter allowlist must now classify normally.
        let mut i = input("/admin/config.backup");
        i.recently_authorised = true;
        i.tripwire = true;
        assert_eq!(
            classify(&i),
            Reason::Tripwire,
            "a knocked source silenced the tripwire"
        );
        assert_eq!(classify(&i).severity(), Severity::High);

        let mut i = input("/api/invoices");
        i.recently_authorised = true;
        assert_eq!(classify(&i), Reason::ApiShaped);

        let mut i = input("/.env");
        i.recently_authorised = true;
        i.matched_expected_host = true;
        assert_eq!(classify(&i), Reason::NamedTheHost);

        let guess = format!("/{}", "A".repeat(KNOCK_TOKEN_CHARS));
        let mut i = input(&guess);
        i.recently_authorised = true;
        assert_eq!(classify(&i), Reason::KnockShaped);
    }

    #[test]
    fn the_tripwire_outranks_every_suppression() {
        // Checked first, so no ordering change downstream can silence it.
        let mut i = input("/favicon.ico");
        i.tripwire = true;
        i.recently_authorised = true;
        assert_eq!(classify(&i), Reason::Tripwire);
    }

    #[test]
    fn the_chatter_allowlist_cannot_be_widened_by_a_crafted_path() {
        // Every one of these is a near-miss that a prefix/suffix match
        // done carelessly would have admitted.
        for hostile in [
            "/favicon.ico/../admin",
            "/favicon.ico%00",
            "/apple-touch-icon/../../etc/passwd.png",
            "/apple-touch-icon-%2e%2e.png",
            "/apple-touch-icon.png.bak",
            "/manifest.json/",
            "/Favicon.ico",
            "/apple-touch-icon-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.png",
        ] {
            assert!(
                !is_authorised_chatter(hostile),
                "the chatter allowlist admitted `{hostile}`"
            );
        }
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
