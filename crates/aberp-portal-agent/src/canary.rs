//! The canary trap, Mac half: the durable probe log and the alert.
//!
//! The front sees the probes; this side keeps the record and tells
//! Ervin. That split is not an accident of layering — it is the same
//! rule the rest of ADR-0115 follows. Durable state and credentials
//! live on the Mac; the VPS holds nothing (§2.4). So the probe log is
//! here, and so is the SMTP password ([`crate::alert`]).
//!
//! # Rate limiting, on top of the front's coalescing
//!
//! The front already collapses a burst into one [`CanaryBatch`]
//! (30-second windows, 60-second minimum between HIGH batches). This
//! side adds a second ceiling, because the two limits protect different
//! things: the front's protects the relay, and this one protects
//! Ervin's attention and the SMTP relay's reputation. A `/16` sweep
//! that lasts an hour must produce a handful of mails, not one every
//! half minute.
//!
//! Counts accumulate across suppressed alerts and are reported in the
//! next one, so nothing is lost — only deferred. "23 probes since the
//! last alert" is more useful than 23 mails anyway.
//!
//! # The probe log
//!
//! Append-only JSONL next to the audit log, rotated at a size cap with
//! exactly one generation kept — Ervin's §9.5 "metadata-only, short
//! rotation", applied to the Mac side as well. Metadata-only is
//! structural: [`ProbeSample`] has no field that can hold a body, a
//! cookie or a token, and the strings that *are* attacker-controlled
//! were sanitised at the front.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use aberp_portal_core::canary::{CanaryBatch, Severity};

use crate::alert::AlertSink;

/// Minimum gap between two HIGH alerts.
pub const HIGH_ALERT_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// Minimum gap between two LOW alerts. Background noise is constant on
/// any public IP; hourly is a digest, not a pager.
pub const LOW_ALERT_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// Rotate the probe log past this size, keeping one generation.
pub const LOG_ROTATE_BYTES: u64 = 1024 * 1024;

/// Counts carried across suppressed alerts.
#[derive(Debug, Default, Clone, Copy)]
struct Deferred {
    batches: u64,
    high: u64,
    low: u64,
    sources: u64,
    dropped: u64,
}

impl Deferred {
    fn add(&mut self, batch: &CanaryBatch) {
        self.batches += 1;
        self.high += batch.high;
        self.low += batch.low;
        self.sources += batch.distinct_sources;
        self.dropped += batch.dropped;
    }

    fn is_empty(&self) -> bool {
        self.batches == 0
    }
}

#[derive(Debug)]
struct State {
    last_high: Option<Instant>,
    last_low: Option<Instant>,
    deferred: Deferred,
}

/// The Mac-side trap.
#[derive(Debug)]
pub struct CanaryWatch {
    log_path: PathBuf,
    sink: AlertSink,
    state: Mutex<State>,
}

impl CanaryWatch {
    #[must_use]
    pub fn new(state_dir: &Path, sink: AlertSink) -> Self {
        Self {
            log_path: state_dir.join("canary.log"),
            sink,
            state: Mutex::new(State {
                last_high: None,
                last_low: None,
                deferred: Deferred::default(),
            }),
        }
    }

    #[must_use]
    pub fn log_path(&self) -> &Path {
        &self.log_path
    }

    #[must_use]
    pub fn sink_label(&self) -> &'static str {
        self.sink.label()
    }

    /// A handle to the alert path, for the one caller that is not a
    /// canary batch.
    ///
    /// Enrolment alerts (§4.3b) go out over the same configured SPOC as
    /// probe alerts — one mail path, one place to get the credentials
    /// right, one thing to test. They deliberately do NOT go through
    /// [`CanaryWatch::record`]: that applies probe rate limiting, and an
    /// enrolment must never be coalesced away.
    #[must_use]
    pub fn clone_sink(&self) -> AlertSink {
        self.sink.clone()
    }

    /// Record a batch and alert if the rate limit allows.
    pub async fn record(&self, batch: &CanaryBatch) {
        self.append(batch);

        if !batch.is_reportable() {
            // Suppressed-only windows are logged and never alerted:
            // that is the operator's own browser.
            return;
        }

        let severity = batch.severity();
        let Some(deferred) = self.claim_alert(batch, severity) else {
            return;
        };
        let (subject, body) = render(batch, severity, deferred);
        if let Err(e) = self.sink.send(&subject, &body).await {
            // A mail that could not be sent must not lose the probe:
            // the log already has it, and the counts roll into the next
            // alert.
            tracing::error!(error = %e, "canary alert could not be delivered");
            self.restore(batch);
        }
    }

    /// Report that the relay has stopped answering (ADR-0115 §3.4).
    ///
    /// The canary's weakest link is silence. A relay that has crashed,
    /// been firewalled, or been taken over and told to drop canary
    /// batches produces exactly the same observable as a quiet
    /// internet: nothing. Every poll response carries a heartbeat
    /// precisely so that this case becomes *detectable*, and this is
    /// where the detection is acted on.
    ///
    /// Rate-limited on the HIGH stamp, because that is what it is: a
    /// portal that cannot report probes is a portal running blind, and
    /// it is worth the same interruption as a scan. It shares the stamp
    /// rather than taking its own so a relay outage and a sweep during
    /// that outage cannot double-page.
    pub async fn report_silence(&self, quiet_for: Duration) {
        {
            let mut g = self.lock();
            let now = Instant::now();
            if g.last_high
                .is_some_and(|t| now.duration_since(t) < HIGH_ALERT_INTERVAL)
            {
                return;
            }
            g.last_high = Some(now);
        }
        let subject = "ABERP portal: the relay has gone quiet".to_string();
        let body = format!(
            "The portal agent has had no answer from the relay for {} seconds.\n\
             \n\
             While this lasts the portal is invisibly down — every request to the\n\
             host gets the ordinary parked answer — and, more importantly, scanner\n\
             probes CANNOT be reported. Treat a long silence as an unmonitored\n\
             window rather than a quiet one.\n\
             \n\
             Nothing is lost: probe batches are held at the relay until this agent\n\
             acknowledges them, and the agent's own probe log is unaffected.\n\
             \n\
             This is the ADR-0115 §3.4 silence detector. It fires at most once per\n\
             alert interval.\n",
            quiet_for.as_secs()
        );
        if let Err(e) = self.sink.send(&subject, &body).await {
            tracing::error!(error = %e, "relay-silence alert could not be delivered");
        }
    }

    /// Decide whether to alert now. Returns the deferred counts to fold
    /// into this alert, or `None` if the rate limit says wait.
    fn claim_alert(&self, batch: &CanaryBatch, severity: Severity) -> Option<Deferred> {
        let interval = match severity {
            Severity::High => HIGH_ALERT_INTERVAL,
            Severity::Low => LOW_ALERT_INTERVAL,
            Severity::Suppressed => return None,
        };
        let now = Instant::now();
        let mut g = self.lock();
        let last = match severity {
            Severity::High => g.last_high,
            _ => g.last_low,
        };
        // A HIGH always escapes the LOW limiter: an hourly digest is
        // the wrong cadence for "someone typed your hostname".
        if last.is_some_and(|t| now.duration_since(t) < interval) {
            g.deferred.add(batch);
            return None;
        }
        match severity {
            Severity::High => g.last_high = Some(now),
            _ => g.last_low = Some(now),
        }
        Some(std::mem::take(&mut g.deferred))
    }

    /// Put a failed alert's counts back so the next one carries them.
    /// Fold a batch whose alert could not be sent back into the
    /// deferred counts.
    ///
    /// The stamp is deliberately **kept**. Clearing it — the previous
    /// behaviour — meant a failed send reset the rate limiter, so with
    /// SMTP down every subsequent batch tried to send immediately: a
    /// scan arriving while the mail path was broken turned into an
    /// unbounded retry loop against the SMTP server, at exactly the
    /// moment the network was already unhealthy. It also inverted the
    /// intent of the limiter, which exists to make a sweep produce one
    /// alert rather than thousands.
    ///
    /// Keeping the stamp costs at most one alert interval of delay, and
    /// costs nothing in information: the counts are in `deferred` and
    /// the probe log already has the batch, so the next alert that does
    /// go out carries everything that happened in between.
    fn restore(&self, batch: &CanaryBatch) {
        let mut g = self.lock();
        g.deferred.add(batch);
    }

    fn append(&self, batch: &CanaryBatch) {
        if let Err(e) = self.try_append(batch) {
            tracing::error!(error = %e, "canary probe log append failed");
        }
    }

    fn try_append(&self, batch: &CanaryBatch) -> std::io::Result<()> {
        use std::io::Write as _;
        if let Some(parent) = self.log_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        self.rotate_if_needed()?;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)?;
        // One line per batch summary, then one per sample. The summary
        // carries the exact counts; the samples are the evidence.
        let summary = serde_json::json!({
            "kind": "portal.canary.window",
            "window_start": batch.window_start,
            "window_end": batch.window_end,
            "total": batch.total,
            "high": batch.high,
            "low": batch.low,
            "suppressed": batch.suppressed,
            "distinct_sources": batch.distinct_sources,
            "dropped": batch.dropped,
        });
        writeln!(f, "{summary}")?;
        for sample in &batch.samples {
            let line = serde_json::to_string(sample)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            writeln!(f, "{line}")?;
        }
        Ok(())
    }

    /// Short rotation: one generation, dropped on the next rotate.
    fn rotate_if_needed(&self) -> std::io::Result<()> {
        let Ok(meta) = std::fs::metadata(&self.log_path) else {
            return Ok(());
        };
        if meta.len() < LOG_ROTATE_BYTES {
            return Ok(());
        }
        let previous = self.log_path.with_extension("log.1");
        // Replaces any older generation — deliberately keeping one, not
        // an archive. This is a tripwire record, not evidence to hoard.
        std::fs::rename(&self.log_path, &previous)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Render the alert.
///
/// Two things are deliberately absent from the output: the portal's
/// **hostname** (see [`aberp_portal_core::ProbeSample::named_the_host`]
/// — the mail travels unencrypted-at-rest to a mailbox, and the label
/// is the thing the wildcard certificate exists to protect) and the
/// **knock token** in any form. A knock-shaped probe is reported as its
/// shape, never its value.
fn render(batch: &CanaryBatch, severity: Severity, deferred: Deferred) -> (String, String) {
    let subject = match severity {
        Severity::High => format!(
            "ABERP perimeter: HIGH — {} targeted probe(s) from {} source(s)",
            batch.high, batch.distinct_sources
        ),
        _ => format!(
            "ABERP perimeter: background scanning — {} probe(s)",
            batch.low
        ),
    };

    let mut body = String::new();
    body.push_str("A request reached the remote-access host without a valid knock token.\n");
    body.push_str("That host has no legitimate unauthenticated traffic, so every such\n");
    body.push_str("request is a probe. The prober saw the same 404 as always.\n\n");
    body.push_str(&format!("Severity : {}\n", severity.as_str()));
    body.push_str(&format!(
        "Window   : {} .. {}\n",
        batch.window_start, batch.window_end
    ));
    body.push_str(&format!(
        "Probes   : {} total ({} high, {} low, {} suppressed) from {} source(s)\n",
        batch.total, batch.high, batch.low, batch.suppressed, batch.distinct_sources
    ));
    if batch.dropped > 0 {
        body.push_str(&format!(
            "Dropped  : {} observation(s) — the queue overran, so the burst was larger than shown\n",
            batch.dropped
        ));
    }
    if !deferred.is_empty() {
        body.push_str(&format!(
            "Deferred : {} earlier window(s) held back by rate limiting ({} high, {} low)\n",
            deferred.batches, deferred.high, deferred.low
        ));
    }

    body.push_str("\nSamples (worst first):\n");
    for s in &batch.samples {
        body.push_str(&format!(
            "  [{}] {} {} {} — {}{}\n",
            s.severity.as_str(),
            s.at,
            s.source_ip,
            s.method,
            s.reason.as_str(),
            if s.named_the_host {
                " (addressed the portal by name)"
            } else {
                ""
            }
        ));
        body.push_str(&format!("        path: {}\n", s.path));
        if let Some(ua) = &s.user_agent {
            body.push_str(&format!("        ua  : {ua}\n"));
        }
    }

    if severity == Severity::High {
        body.push_str(
            "\nWhat HIGH means: the prober either addressed the host by its label,\n\
             asked for something whose shape is portal-specific, or hit the decoy\n\
             resource. The label is supposed to be known only to your bookmark.\n\
             If this was not you: rotate the knock token at the Mac\n\
             (`aberp-portal-agent rotate-knock`). Passkeys are unaffected —\n\
             nothing here suggests an authentication attempt succeeded, and none\n\
             could without your Face ID or Touch ID.\n",
        );
    }
    (subject, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aberp_portal_core::canary::{ProbeSample, Reason};

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("portal-canary-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("temp dir");
        d
    }

    fn sample(severity: Severity, reason: Reason) -> ProbeSample {
        ProbeSample {
            at: "2026-08-20T10:00:00Z".into(),
            severity,
            reason,
            source_ip: "203.0.113.9".into(),
            method: "GET".into(),
            path: "/admin/config.backup".into(),
            user_agent: Some("masscan".into()),
            named_the_host: reason == Reason::NamedTheHost,
            sni: None,
        }
    }

    fn batch(high: u64, low: u64, suppressed: u64) -> CanaryBatch {
        let mut samples = Vec::new();
        if high > 0 {
            samples.push(sample(Severity::High, Reason::Tripwire));
        }
        if low > 0 {
            samples.push(sample(Severity::Low, Reason::BackgroundNoise));
        }
        CanaryBatch {
            window_start: "2026-08-20T10:00:00Z".into(),
            window_end: "2026-08-20T10:00:30Z".into(),
            total: high + low + suppressed,
            high,
            low,
            suppressed,
            distinct_sources: 1,
            dropped: 0,
            samples,
        }
    }

    fn alerts(dir: &Path) -> String {
        std::fs::read_to_string(dir.join("alerts.log")).unwrap_or_default()
    }

    #[tokio::test]
    async fn a_reportable_batch_is_logged_and_alerted() {
        let dir = tmpdir("basic");
        let w = CanaryWatch::new(&dir, AlertSink::File(dir.join("alerts.log")));
        w.record(&batch(1, 0, 0)).await;

        let log = std::fs::read_to_string(w.log_path()).expect("probe log");
        assert!(log.contains("portal.canary.window"));
        // The log is machine-readable JSONL: the reason appears as its
        // serialised variant, not the prose the alert renders.
        assert!(log.contains(r#""reason":"tripwire""#), "{log}");
        assert!(log.contains(r#""severity":"high""#));
        let mail = alerts(&dir);
        assert!(mail.contains("HIGH"), "{mail}");
        assert!(mail.contains("rotate-knock"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_suppressed_only_window_is_logged_but_never_alerted() {
        // The operator's own browser. Recorded for forensics, silent.
        let dir = tmpdir("suppressed");
        let w = CanaryWatch::new(&dir, AlertSink::File(dir.join("alerts.log")));
        w.record(&batch(0, 0, 4)).await;
        assert!(std::fs::read_to_string(w.log_path())
            .expect("probe log")
            .contains("\"suppressed\":4"));
        assert!(alerts(&dir).is_empty(), "a suppressed window paged someone");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_burst_produces_one_alert_not_a_flood() {
        // The headline requirement: a scan is one alert.
        let dir = tmpdir("burst");
        let w = CanaryWatch::new(&dir, AlertSink::File(dir.join("alerts.log")));
        for _ in 0..50 {
            w.record(&batch(3, 0, 0)).await;
        }
        let mail = alerts(&dir);
        assert_eq!(
            mail.matches("Subject:").count(),
            1,
            "50 batches produced more than one alert"
        );
        // …and every batch is still in the probe log.
        let log = std::fs::read_to_string(w.log_path()).expect("probe log");
        assert_eq!(log.matches("portal.canary.window").count(), 50);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn deferred_counts_ride_along_on_the_next_alert() {
        let dir = tmpdir("deferred");
        let w = CanaryWatch::new(&dir, AlertSink::File(dir.join("alerts.log")));
        w.record(&batch(1, 0, 0)).await;
        for _ in 0..4 {
            w.record(&batch(2, 0, 0)).await;
        }
        // Force the limiter open, as five minutes would.
        w.lock().last_high = None;
        w.record(&batch(1, 0, 0)).await;
        let mail = alerts(&dir);
        assert!(
            mail.contains("Deferred : 4 earlier window(s)"),
            "the held-back windows were lost rather than folded in:\n{mail}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn low_and_high_have_independent_limiters() {
        // An hourly digest is the wrong cadence for "someone typed your
        // hostname", so a HIGH must not be held behind a LOW.
        let dir = tmpdir("independent");
        let w = CanaryWatch::new(&dir, AlertSink::File(dir.join("alerts.log")));
        w.record(&batch(0, 5, 0)).await;
        w.record(&batch(1, 0, 0)).await;
        let mail = alerts(&dir);
        assert_eq!(mail.matches("Subject:").count(), 2);
        assert!(mail.contains("background scanning"));
        assert!(mail.contains("HIGH"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn the_alert_never_carries_the_hostname_or_a_token() {
        let dir = tmpdir("noleak");
        let w = CanaryWatch::new(&dir, AlertSink::File(dir.join("alerts.log")));
        let mut b = batch(1, 0, 0);
        b.samples[0].named_the_host = true;
        b.samples[0].reason = Reason::NamedTheHost;
        w.record(&b).await;
        let mail = alerts(&dir);
        assert!(mail.contains("addressed the portal by name"));
        // The fact, not the value.
        assert!(!mail.contains("://"), "the alert carried a URL: {mail}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn the_probe_log_rotates_and_keeps_one_generation() {
        let dir = tmpdir("rotate");
        let w = CanaryWatch::new(&dir, AlertSink::Disabled);
        // Seed a file already past the cap.
        std::fs::write(w.log_path(), "x".repeat(LOG_ROTATE_BYTES as usize + 1)).expect("seed");
        w.record(&batch(0, 1, 0)).await;
        assert!(w.log_path().with_extension("log.1").exists());
        let current = std::fs::read_to_string(w.log_path()).expect("current");
        assert!(current.contains("portal.canary.window"));
        assert!(
            (current.len() as u64) < LOG_ROTATE_BYTES,
            "the rotated file was not replaced by a fresh one"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_failed_send_does_not_lose_the_probe() {
        // The file sink cannot fail easily, so aim it at a path whose
        // parent is a file — an unwritable location.
        let dir = tmpdir("failsend");
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, "not a directory").expect("write");
        let w = CanaryWatch::new(&dir, AlertSink::File(blocker.join("alerts.log")));
        w.record(&batch(1, 0, 0)).await;
        // The probe log has it regardless…
        assert!(std::fs::read_to_string(w.log_path())
            .expect("probe log")
            .contains("portal.canary.window"));
        // …and the counts were put back for the next attempt.
        assert_eq!(w.lock().deferred.batches, 1);
        // The stamp is KEPT — this is the flipped pin (should-fix 1).
        //
        // It used to be cleared, on the reasoning that a failed send
        // should not be rate-limited into silence. But clearing it meant
        // a failed send RESET the limiter, so with SMTP down every
        // subsequent batch tried to send immediately: a scan arriving
        // while the mail path was broken became an unbounded retry loop
        // against an SMTP server on an already-unhealthy network. The
        // interval is the right backoff, and nothing is lost — the
        // counts are deferred and the probe log has the batch.
        assert!(
            w.lock().last_high.is_some(),
            "a failed send reset the rate limiter into an unbounded retry loop"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
