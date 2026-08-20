//! The agent's local, append-only audit log (ADR-0113 §6.5).
//!
//! > on the Mac, append-only, no bodies logged, and refusals are logged
//! > as loudly as successes.
//!
//! Deliberately a plain JSONL file rather than an ABERP audit-ledger
//! chain: the agent must keep working with ABERP stopped (§2.2), so it
//! cannot depend on a DuckDB handle to record that ABERP is down.
//! Promoting these to `portal.*` `EventKind`s in the ledger proper is
//! named in §6.5 as a build-time decision for the full D-17 build; the
//! constraint fixed by the ADR — and honoured here — is append-only,
//! body-free, refusals-included.
//!
//! # No bodies, structurally
//!
//! [`Event`] has no field that can hold a request or response body.
//! Keeping the *type* incapable of carrying one is what makes the "no
//! bodies logged" claim durable against a future edit that would
//! otherwise just add a `body` to a `serde_json::Value` bag.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// One audit record. Metadata only.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Event {
    /// RFC-3339 UTC.
    pub ts: String,
    /// Dotted, greppable event name — `portal.knock.accepted`,
    /// `portal.auth.verified`, `portal.proxy.refused`, …
    pub kind: &'static str,
    /// HTTP method as the browser sent it, when the event is a request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// Request path (never the query string: it can carry identifiers
    /// and this log is metadata-only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Response status the agent produced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// Credential id (base64url) for auth events.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential_id: Option<String>,
    /// Why a refusal happened. Fixed vocabulary at the call sites — not
    /// a place to spill inputs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Peer address as the front reported it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer: Option<String>,
}

impl Event {
    #[must_use]
    pub fn new(kind: &'static str) -> Self {
        Self {
            ts: now_rfc3339(),
            kind,
            method: None,
            path: None,
            status: None,
            credential_id: None,
            reason: None,
            peer: None,
        }
    }

    #[must_use]
    pub fn request(mut self, method: &str, path: &str) -> Self {
        self.method = Some(method.to_string());
        self.path = Some(path.to_string());
        self
    }

    #[must_use]
    pub fn status(mut self, status: u16) -> Self {
        self.status = Some(status);
        self
    }

    #[must_use]
    pub fn reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }

    #[must_use]
    pub fn credential(mut self, id: impl Into<String>) -> Self {
        self.credential_id = Some(id.into());
        self
    }

    #[must_use]
    pub fn peer(mut self, peer: Option<&str>) -> Self {
        self.peer = peer.map(str::to_string);
        self
    }
}

fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

/// Append-only log file under the agent's state directory.
#[derive(Debug, Clone)]
pub struct AuditLog {
    path: PathBuf,
}

impl AuditLog {
    #[must_use]
    pub fn in_dir(state_dir: &Path) -> Self {
        Self {
            path: state_dir.join("audit.log"),
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one record.
    ///
    /// A failure to write is logged and swallowed rather than
    /// propagated: refusing to serve because the audit file is
    /// unwritable would convert a disk problem into an outage on a
    /// surface whose whole point is being reachable when things are
    /// broken. The failure is visible in the daemon's own tracing
    /// output, which launchd captures.
    pub fn append(&self, event: &Event) {
        if let Err(e) = self.try_append(event) {
            tracing::error!(
                error = %e,
                path = %self.path.display(),
                kind = event.kind,
                "portal audit append failed — the event is lost from the file but not from the daemon log"
            );
        }
    }

    fn try_append(&self, event: &Event) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let line = serde_json::to_string(event)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        // `append(true)` is the append-only posture: no seek, no
        // truncate, no rewrite path anywhere in this module.
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(f, "{line}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("portal-audit-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).expect("temp dir");
        d
    }

    #[test]
    fn appends_one_line_per_event_and_never_rewrites() {
        let dir = tmpdir("append");
        let log = AuditLog::in_dir(&dir);
        log.append(&Event::new("portal.knock.accepted"));
        log.append(&Event::new("portal.proxy.refused").reason("method not allowed"));
        let body = std::fs::read_to_string(log.path()).expect("read");
        let lines: Vec<_> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("portal.knock.accepted"));
        assert!(lines[1].contains("portal.proxy.refused"));
        // The first line is byte-identical after the second append —
        // append-only means earlier records cannot move.
        log.append(&Event::new("portal.session.minted"));
        let after = std::fs::read_to_string(log.path()).expect("read");
        assert!(after.starts_with(lines[0]));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn refusals_are_recorded_as_loudly_as_successes() {
        // §6.5's explicit requirement, pinned: a refusal carries the
        // same fields a success does, including the reason.
        let e = Event::new("portal.proxy.refused")
            .request("POST", "/api/invoices")
            .status(405)
            .reason("mutating verb");
        let json = serde_json::to_string(&e).expect("serialise");
        assert!(json.contains("\"method\":\"POST\""));
        assert!(json.contains("\"status\":405"));
        assert!(json.contains("mutating verb"));
    }

    #[test]
    fn no_field_can_carry_a_body_or_a_query_string() {
        // Structural, not stylistic: this is the test that fails if a
        // future edit adds a body/query field to the record type.
        let e = Event::new("portal.proxy.ok").request("GET", "/api/invoices/inv-1");
        let v: serde_json::Value = serde_json::to_value(&e).expect("to value");
        let obj = v.as_object().expect("object");
        for forbidden in ["body", "body_b64", "query", "payload", "response"] {
            assert!(
                !obj.contains_key(forbidden),
                "audit record grew a `{forbidden}` field — ADR-0113 §6.5 forbids bodies in this log"
            );
        }
    }
}
