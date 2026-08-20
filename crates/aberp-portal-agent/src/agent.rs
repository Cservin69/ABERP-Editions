//! The agent's request dispatcher — everything the portal can ask, and
//! the order the checks happen in.
//!
//! # The shape of the API
//!
//! | Route | Session? | What it is |
//! |---|---|---|
//! | `GET  /api/session` | no | pre-auth status: is anyone enrolled, is an enrolment window open |
//! | `POST /api/auth/begin` | no | assertion options (ADR-0113 §4.3) |
//! | `POST /api/auth/finish` | no | assertion verify → session mint (§4.4) |
//! | `POST /api/enrol/begin` | no, but needs a console-minted token | creation options (§4.3) |
//! | `POST /api/enrol/finish` | same | registration verify → credential stored |
//! | `GET  /api/status` | yes | the ABERP up/down card (§5.2) |
//! | `GET  /api/health` | yes | proxied `serve.rs` `/health` (§6.2 row 1) |
//! | `GET  /api/invoices` | yes | proxied invoice list (§6.2 row 2) |
//! | `GET  /api/invoices/:id` | yes | proxied invoice detail (§6.2 row 3) |
//! | `GET  /api/invoices/:id/pdf` | yes | proxied PDF (§6.2 row 4) |
//! | anything else | — | refused here, on the Mac |
//!
//! The ceremony routes are unauthenticated *by necessity* — they are
//! how one becomes authenticated — but they are not unguarded: they sit
//! behind the knock (which the relay enforced before this code ran) and
//! behind either a single-use challenge or a console-minted enrolment
//! token. §1.2's "someone who learns the subdomain" row lands exactly
//! here: "a WebAuthn challenge that cannot be satisfied — no username
//! field, no password form, nothing to guess or stuff".
//!
//! # Why the proxy re-probes before refusing
//!
//! §5.2 says the agent refuses proxy requests when ABERP is down, and
//! the poller runs every 10 s. A read arriving 2 s after ABERP came
//! back would be refused on stale information, so the down path (and
//! only the down path) takes one fresh probe before answering. The up
//! path never pays for it.

use std::path::PathBuf;
use std::sync::Arc;

use aberp_portal_core::{PortalRequest, PortalResponse};
use serde::Deserialize;

use crate::allowlist::{self, Decision};
use crate::audit::{AuditLog, Event};
use crate::config::AgentConfig;
use crate::credstore::{Credential, CredentialStore};
use crate::enrol::EnrolStore;
use crate::health::{self, HealthMonitor};
use crate::knock::KnockStore;
use crate::rand;
use crate::session::{self, SessionStore};
use crate::webauthn::{
    AssertionResponse, ChallengeStore, RegistrationResponse, RelyingParty, WebAuthnError,
};

/// Everything the agent owns. One instance per daemon.
#[derive(Debug)]
pub struct Agent {
    pub cfg: AgentConfig,
    pub rp: RelyingParty,
    pub challenges: ChallengeStore,
    pub sessions: SessionStore,
    pub credentials: CredentialStore,
    pub enrolment: EnrolStore,
    pub knock: KnockStore,
    pub health: HealthMonitor,
    pub audit: AuditLog,
}

impl Agent {
    /// Build from config, creating the state directory if needed.
    pub fn new(cfg: AgentConfig) -> std::io::Result<Arc<Self>> {
        std::fs::create_dir_all(&cfg.state_dir)?;
        let rp = RelyingParty {
            rp_id: cfg.rp_id.clone(),
            rp_name: cfg.rp_name.clone(),
            origin: cfg.origin.clone(),
        };
        Ok(Arc::new(Self {
            challenges: ChallengeStore::new(),
            sessions: SessionStore::new(),
            credentials: CredentialStore::in_dir(&cfg.state_dir),
            enrolment: EnrolStore::in_dir(&cfg.state_dir),
            knock: KnockStore::in_dir(&cfg.state_dir),
            health: HealthMonitor::new(),
            audit: AuditLog::in_dir(&cfg.state_dir),
            rp,
            cfg,
        }))
    }

    /// The stable WebAuthn user handle, minted once per installation.
    ///
    /// WebAuthn groups credentials by user id, so every passkey Ervin
    /// enrols — iPhone and Mac (§4.3) — must share one. It is a random
    /// opaque value, never an email or a name: §3.2's no-fingerprint
    /// rule extends to what the ceremony JSON would reveal to anyone
    /// who got past the knock.
    pub fn user_handle(&self) -> Result<String, rand::RandError> {
        let path: PathBuf = self.cfg.state_dir.join("user.handle");
        if let Ok(existing) = std::fs::read_to_string(&path) {
            if !existing.trim().is_empty() {
                return Ok(existing.trim().to_string());
            }
        }
        let handle = rand::token()?;
        let _ = std::fs::write(&path, format!("{handle}\n"));
        Ok(handle)
    }

    /// Dispatch one request that arrived over the tunnel.
    pub async fn handle(&self, req: &PortalRequest, tunnel_id: &str) -> PortalResponse {
        let path = req.path.as_str();
        let method = req.method.as_str();

        match (method, path) {
            ("GET", "/api/session") => self.session_status(),
            ("POST", "/api/auth/begin") => self.auth_begin(req),
            ("POST", "/api/auth/finish") => self.auth_finish(req, tunnel_id),
            ("POST", "/api/enrol/begin") => self.enrol_begin(req),
            ("POST", "/api/enrol/finish") => self.enrol_finish(req, tunnel_id),
            _ => self.authenticated(req, tunnel_id).await,
        }
    }

    /// Pre-auth status. Says only what the shell must know to render a
    /// button, and nothing about *who* is enrolled.
    fn session_status(&self) -> PortalResponse {
        let enrolled = self.credentials.load().map(|c| c.len()).unwrap_or(0);
        json_ok(&serde_json::json!({
            "enrolled": enrolled,
            "enrolment_open": self.enrolment.is_open(),
        }))
    }

    fn auth_begin(&self, req: &PortalRequest) -> PortalResponse {
        let enrolled = match self.credentials.load() {
            Ok(c) => c,
            Err(e) => return self.refuse(req, 500, "credential store unreadable", &e.to_string()),
        };
        if enrolled.is_empty() {
            // Nobody is enrolled: there is nothing to authenticate
            // against, and saying so to a knocked caller is fine — they
            // already passed the gate.
            return self.refuse(req, 409, "no credential enrolled", "no credential enrolled");
        }
        match self.rp.request_options(&self.challenges, &enrolled) {
            Ok(opts) => {
                self.audit
                    .append(&Event::new("portal.auth.challenge_issued").peer(req.peer.as_deref()));
                json_ok(&opts)
            }
            Err(e) => self.refuse(req, 500, "challenge mint failed", &e.to_string()),
        }
    }

    fn auth_finish(&self, req: &PortalRequest, tunnel_id: &str) -> PortalResponse {
        #[derive(Deserialize)]
        struct Body {
            #[serde(flatten)]
            assertion: AssertionResponse,
        }
        let body: Body = match parse_body(req) {
            Some(b) => b,
            None => return self.refuse(req, 400, "malformed assertion", "malformed assertion"),
        };

        let credential = match self.credentials.get(&body.assertion.id) {
            Ok(Some(c)) => c,
            Ok(None) => {
                return self.refuse(req, 401, "unknown credential", "unknown credential");
            }
            Err(e) => return self.refuse(req, 500, "credential store unreadable", &e.to_string()),
        };

        match self
            .rp
            .verify_assertion(&self.challenges, &credential, &body.assertion)
        {
            Ok(new_count) => {
                let _ = self
                    .credentials
                    .update_sign_count(&credential.id, new_count);
                self.mint_session(req, tunnel_id, &credential.id, "portal.auth.verified")
            }
            Err(e) => {
                // The reason is the agent's own typed error, never the
                // caller's input (§6.5).
                self.audit.append(
                    &Event::new("portal.auth.failed")
                        .credential(credential.id.clone())
                        .reason(webauthn_reason(&e))
                        .peer(req.peer.as_deref()),
                );
                unauthorised()
            }
        }
    }

    fn enrol_begin(&self, req: &PortalRequest) -> PortalResponse {
        #[derive(Deserialize)]
        struct Body {
            token: String,
        }
        let Some(body) = parse_body::<Body>(req) else {
            return self.refuse(
                req,
                400,
                "malformed enrolment request",
                "malformed enrolment request",
            );
        };
        // Peek without consuming: the token is spent only once the
        // ceremony actually completes, so a browser that opened the URL
        // and then hit cancel has not burned Ervin's 10 minutes.
        if !self.enrolment.is_open() {
            self.audit.append(
                &Event::new("portal.enrol.refused")
                    .reason("no enrolment window open")
                    .peer(req.peer.as_deref()),
            );
            return unauthorised();
        }
        let enrolled = self.credentials.load().unwrap_or_default();
        let handle = match self.user_handle() {
            Ok(h) => h,
            Err(e) => return self.refuse(req, 500, "user handle mint failed", &e.to_string()),
        };
        match self
            .rp
            .creation_options(&self.challenges, &handle, &enrolled)
        {
            Ok(opts) => {
                self.audit
                    .append(&Event::new("portal.enrol.challenge_issued").peer(req.peer.as_deref()));
                // Echo the token back so `finish` can present it again
                // with the ceremony result; it is the caller's own
                // value, already known to them.
                json_ok(&serde_json::json!({ "options": opts, "token": body.token }))
            }
            Err(e) => self.refuse(req, 500, "challenge mint failed", &e.to_string()),
        }
    }

    fn enrol_finish(&self, req: &PortalRequest, tunnel_id: &str) -> PortalResponse {
        #[derive(Deserialize)]
        struct Body {
            token: String,
            #[serde(flatten)]
            registration: RegistrationResponse,
        }
        let Some(body) = parse_body::<Body>(req) else {
            return self.refuse(
                req,
                400,
                "malformed enrolment request",
                "malformed enrolment request",
            );
        };

        // Consume FIRST: single-use means the window closes whether or
        // not the ceremony that follows verifies.
        let label = match self.enrolment.consume(&body.token) {
            Ok(l) => l,
            Err(e) => {
                self.audit.append(
                    &Event::new("portal.enrol.refused")
                        .reason(e.to_string())
                        .peer(req.peer.as_deref()),
                );
                return unauthorised();
            }
        };

        match self
            .rp
            .verify_registration(&self.challenges, &body.registration)
        {
            Ok(v) => {
                let credential = Credential {
                    id: v.credential_id.clone(),
                    x: hex::encode(v.public_key.x),
                    y: hex::encode(v.public_key.y),
                    sign_count: v.sign_count,
                    label,
                    created_at: now_rfc3339(),
                };
                if let Err(e) = self.credentials.add(credential) {
                    return self.refuse(req, 500, "credential store unwritable", &e.to_string());
                }
                self.mint_session(req, tunnel_id, &v.credential_id, "portal.enrol.registered")
            }
            Err(e) => {
                self.audit.append(
                    &Event::new("portal.enrol.failed")
                        .reason(webauthn_reason(&e))
                        .peer(req.peer.as_deref()),
                );
                unauthorised()
            }
        }
    }

    fn mint_session(
        &self,
        req: &PortalRequest,
        tunnel_id: &str,
        credential_id: &str,
        kind: &'static str,
    ) -> PortalResponse {
        match self.sessions.mint(tunnel_id) {
            Ok(token) => {
                self.audit.append(
                    &Event::new(kind)
                        .credential(credential_id.to_string())
                        .peer(req.peer.as_deref()),
                );
                self.audit
                    .append(&Event::new("portal.session.minted").peer(req.peer.as_deref()));
                let mut resp = json_ok(&serde_json::json!({ "ok": true }));
                resp.set_cookie = Some(session::cookie_header(&token, self.cfg.cookie_secure));
                resp
            }
            Err(e) => self.refuse(req, 500, "session mint failed", &e.to_string()),
        }
    }

    /// Everything past the auth wall.
    async fn authenticated(&self, req: &PortalRequest, tunnel_id: &str) -> PortalResponse {
        if !self.sessions.validate(req.cookie.as_deref(), tunnel_id) {
            self.audit.append(
                &Event::new("portal.request.unauthenticated")
                    .request(&req.method, &req.path)
                    .status(401)
                    .peer(req.peer.as_deref()),
            );
            return unauthorised();
        }

        // The status card is served by the agent from its own
        // observations — it must work when ABERP does not (§5.1).
        if req.method == "GET" && req.path == "/api/status" {
            self.health.tick(&self.cfg).await;
            return json_ok(&self.health.view());
        }

        match allowlist::decide(&req.method, &req.path, req.query.as_deref()) {
            Decision::Refuse(r) => {
                self.audit.append(
                    &Event::new("portal.proxy.refused")
                        .request(&req.method, &req.path)
                        .status(r.status())
                        .reason(r.as_str())
                        .peer(req.peer.as_deref()),
                );
                PortalResponse::json(
                    r.status(),
                    &serde_json::json!({ "error": r.as_str() }).to_string(),
                )
            }
            Decision::Allow { upstream_path } => self.proxy(req, &upstream_path).await,
        }
    }

    async fn proxy(&self, req: &PortalRequest, upstream_path: &str) -> PortalResponse {
        if !self.health.is_up() {
            // One fresh probe before refusing — see the module docs.
            self.health.tick(&self.cfg).await;
        }
        if !self.health.is_up() {
            self.audit.append(
                &Event::new("portal.proxy.backend_down")
                    .request(&req.method, &req.path)
                    .status(503)
                    .peer(req.peer.as_deref()),
            );
            return PortalResponse::json(
                503,
                &serde_json::json!({
                    "error": "ABERP is not running",
                    "aberp_up": false,
                })
                .to_string(),
            );
        }

        let client = match self
            .health
            .upstream_for(&health::resolve_upstream_config(&self.cfg))
        {
            Ok(c) => c,
            Err(detail) => {
                return self.refuse(req, 503, "ABERP is not reachable", &detail);
            }
        };

        match client.get(upstream_path).await {
            Ok(r) => {
                self.audit.append(
                    &Event::new("portal.proxy.ok")
                        .request(&req.method, &req.path)
                        .status(r.status)
                        .peer(req.peer.as_deref()),
                );
                PortalResponse::bytes(r.status, &r.content_type, &r.body)
            }
            Err(e) => {
                self.audit.append(
                    &Event::new("portal.proxy.backend_error")
                        .request(&req.method, &req.path)
                        .status(502)
                        .reason(e.to_string())
                        .peer(req.peer.as_deref()),
                );
                PortalResponse::json(
                    502,
                    &serde_json::json!({ "error": "ABERP did not answer" }).to_string(),
                )
            }
        }
    }

    fn refuse(
        &self,
        req: &PortalRequest,
        status: u16,
        public: &str,
        audit_reason: &str,
    ) -> PortalResponse {
        self.audit.append(
            &Event::new("portal.request.refused")
                .request(&req.method, &req.path)
                .status(status)
                .reason(audit_reason.to_string())
                .peer(req.peer.as_deref()),
        );
        PortalResponse::json(status, &serde_json::json!({ "error": public }).to_string())
    }
}

/// One shape for every rejection past the knock: no detail, no
/// distinction between "wrong credential" and "no credential".
fn unauthorised() -> PortalResponse {
    PortalResponse::json(401, r#"{"error":"unauthorised"}"#)
}

fn json_ok<T: serde::Serialize>(value: &T) -> PortalResponse {
    match serde_json::to_string(value) {
        Ok(s) => PortalResponse::json(200, &s),
        Err(_) => PortalResponse::json(500, r#"{"error":"response serialisation failed"}"#),
    }
}

fn parse_body<T: serde::de::DeserializeOwned>(req: &PortalRequest) -> Option<T> {
    use base64::Engine as _;
    let raw = req.body_b64.as_ref()?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(raw.as_bytes())
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// A short, fixed-vocabulary reason for the audit log. Deliberately not
/// `e.to_string()` for the caller-supplied cases: the log must not echo
/// attacker-shaped input (§6.5).
fn webauthn_reason(e: &WebAuthnError) -> &'static str {
    match e {
        WebAuthnError::Rand(_) => "csprng failure",
        WebAuthnError::ClientDataEncoding | WebAuthnError::ClientDataJson(_) => {
            "malformed clientData"
        }
        WebAuthnError::WrongCeremonyType { .. } => "wrong ceremony type",
        WebAuthnError::UnknownChallenge => "unknown or replayed challenge",
        WebAuthnError::WrongOrigin { .. } => "origin mismatch",
        WebAuthnError::AttestationEncoding
        | WebAuthnError::AttestationCbor(_)
        | WebAuthnError::AttestationNoAuthData => "malformed attestation",
        WebAuthnError::AuthData(_) | WebAuthnError::AuthDataEncoding => {
            "malformed authenticator data"
        }
        WebAuthnError::SignatureEncoding => "malformed signature encoding",
        WebAuthnError::WrongRelyingParty => "rp id mismatch",
        WebAuthnError::UserNotVerified => "user verification absent",
        WebAuthnError::NoAttestedCredential => "no attested credential",
        WebAuthnError::BadSignature => "signature did not verify",
        WebAuthnError::SignCountRegression { .. } => "sign counter regression",
        WebAuthnError::Cose(_) => "unsupported credential key",
    }
}

fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}
