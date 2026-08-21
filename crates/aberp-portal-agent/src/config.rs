//! Everything the agent reads from its environment.
//!
//! # The hostname is NOT in this repository
//!
//! ADR-0115 §3.2 closes the Certificate-Transparency leak by presenting
//! the **wildcard** `*.abenerp.com` certificate, so the portal's label
//! never enters a public CT log. That control is worth exactly nothing
//! if the label is committed to a git repository instead, so the
//! concrete hostname is minted at deploy time and read from
//! [`PORTAL_HOST_ENV`] at runtime. There is no default, no fallback and
//! no example value anywhere in this crate — a missing `PORTAL_HOST` is
//! a hard startup error.
//!
//! `tests/no_committed_hostname.rs` enforces the rule mechanically, so
//! a future "just for the docs" hostname literal fails CI rather than
//! quietly publishing the label.
//!
//! Ervin's decision on §9.2 overrides the ADR's own recommendation: the
//! label is a **random but memorable multi-word triad**, not `internal`.
//! That is a deploy-time choice about a value this crate never sees; the
//! only code consequence is the one above — no literal, ever.
//!
//! # Secrets come from the keychain in production
//!
//! Three secrets exist on the Mac: the agent's mTLS client key (§2.2),
//! the ABERP session bearer (§6.4), and the SMTP password the canary
//! alert is sent with (ADR-0047). All are [`SecretSource`]s so the
//! keychain path is the production default while dev and test read a
//! file or an env var — the DEV keychain-bypass rule, so no test ever
//! prompts for, or touches, real keychain material.

use std::path::{Path, PathBuf};

use aberp_portal_core::PinnedFingerprint;

/// The env var carrying the portal's runtime hostname. Read the module
/// docs before adding a default to this.
pub const PORTAL_HOST_ENV: &str = "PORTAL_HOST";

/// Config failures. Every one of them stops the daemon before it opens
/// a socket: a portal running on a guessed hostname would mint passkeys
/// bound to the wrong relying party (§4.1), and an unpinned Leg B is
/// not this design (§2.3).
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{0} is not set — the portal hostname is minted at deploy time and never committed (ADR-0115 §3.2)")]
    MissingHost(&'static str),
    #[error("{var} is not set")]
    Missing { var: &'static str },
    #[error("{var}: {source}")]
    BadFingerprint {
        var: &'static str,
        #[source]
        source: aberp_portal_core::PinError,
    },
    #[error("neither HOME nor USERPROFILE is set — cannot locate the agent state directory")]
    NoHome,
}

/// Where a secret is read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretSource {
    /// Production: the OS keychain, `service`/`account` as the entry
    /// coordinates. For the ABERP bearer these are `aberp.nav.<tenant>`
    /// / `session_token` — the same entry `serve.rs` provisions, read
    /// rather than duplicated (ADR-0115 §2.2).
    Keychain { service: String, account: String },
    /// Dev/test: a file on disk. Never used in production; the
    /// keychain is the auth surface, a `chmod 0600` file is not
    /// (`runtime_discovery.rs` makes the same point).
    File(PathBuf),
    /// Dev/test: the value inline from the environment — mirrors
    /// `run/dev-test.sh`'s `ABERP_INTERNAL_BEARER`.
    Inline(String),
}

impl SecretSource {
    /// Resolve to the secret value.
    pub fn read(&self) -> Result<String, SecretError> {
        match self {
            Self::Keychain { service, account } => {
                let entry = keyring::Entry::new(service, account).map_err(|source| {
                    SecretError::Keychain {
                        service: service.clone(),
                        account: account.clone(),
                        source,
                    }
                })?;
                entry
                    .get_password()
                    .map_err(|source| SecretError::Keychain {
                        service: service.clone(),
                        account: account.clone(),
                        source,
                    })
            }
            Self::File(p) => std::fs::read_to_string(p)
                .map(|s| s.trim().to_string())
                .map_err(|source| SecretError::File {
                    path: p.clone(),
                    source,
                }),
            Self::Inline(v) => Ok(v.clone()),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("keychain entry {service}/{account}: {source}")]
    Keychain {
        service: String,
        account: String,
        #[source]
        source: keyring::Error,
    },
    #[error("reading {path}: {source}")]
    File {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// How Leg C's coordinates are found on each probe.
///
/// `serve.rs` picks a kernel-assigned port unless `ABERP_HTTPS_PORT`
/// pins one, and rewrites `runtime.json` on every boot — so a portal
/// that resolved the URL once at startup would report a restarted
/// ABERP as down forever. [`UpstreamDiscovery::RuntimeJson`] re-reads
/// the file on every probe; [`UpstreamDiscovery::Fixed`] is the
/// explicit-coordinates path (`run/dev-test.sh`'s `ABERP_INTERNAL_*`
/// trio, and the end-to-end test).
///
/// This is a config *value* rather than a runtime environment lookup on
/// purpose: the probe path must be decidable by reading the config, and
/// a test must be able to construct an agent without mutating
/// process-global state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpstreamDiscovery {
    /// Use the coordinates in [`UpstreamConfig`] as given.
    Fixed,
    /// Re-read `~/.aberp-defense/<tenant>/runtime.json` each probe.
    RuntimeJson { tenant: String },
}

/// How the agent reaches the local ABERP process (Leg C).
#[derive(Debug, Clone)]
pub struct UpstreamConfig {
    /// `https://127.0.0.1:<port>` — loopback only, as `serve.rs` binds.
    pub base_url: String,
    /// SHA-256 of the loopback listener's self-signed leaf, pinned the
    /// way the Tauri shell pins it (`apps/aberp-ui/src/pinned_client.rs`).
    pub tls_fingerprint: String,
    /// The existing `serve.rs` bearer. §6.4 names the liability: this
    /// is an all-routes token today, confined by the agent's allowlist
    /// rather than by its own scope. H2 fixes that in `serve.rs`.
    pub bearer: SecretSource,
}

/// Full agent configuration.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// WebAuthn RP ID — the portal hostname, from `PORTAL_HOST`.
    pub rp_id: String,
    /// Expected `origin` in every `clientDataJSON` (§4.1). Defaults to
    /// `https://{rp_id}`; overridable only so the loopback end-to-end
    /// test can use its own origin.
    pub origin: String,
    /// Human-facing RP name shown by the OS during a ceremony.
    pub rp_name: String,
    /// `host:port` of the relay the agent dials OUT to.
    pub relay_addr: String,
    /// TLS `ServerName` presented on Leg B.
    pub relay_server_name: String,
    /// The relay's pinned leaf fingerprint (§2.3).
    pub relay_fingerprint: PinnedFingerprint,
    /// PEM of the agent's own client certificate chain.
    pub client_cert_pem: PathBuf,
    /// The matching private key.
    pub client_key: SecretSource,
    /// Agent-owned state: credential store, knock token, audit log.
    pub state_dir: PathBuf,
    /// Leg C.
    pub upstream: UpstreamConfig,
    /// How Leg C is (re-)discovered on each probe.
    pub discovery: UpstreamDiscovery,
    /// `false` drops the `Secure` cookie attribute. Test-only; the
    /// production path never sets it (§4.4 requires `Secure`).
    pub cookie_secure: bool,
    /// The decoy path the canary treats as an unambiguous scanner hit.
    /// Published to the relay on every poll, so rotating it
    /// needs no relay redeploy and leaves no value in this repository.
    pub tripwire_path: String,
    /// Where canary alerts are delivered.
    pub alert_sink: crate::alert::AlertSink,
}

fn env(var: &'static str) -> Result<String, ConfigError> {
    std::env::var(var)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .ok_or(ConfigError::Missing { var })
}

fn home() -> Result<PathBuf, ConfigError> {
    for var in ["HOME", "USERPROFILE"] {
        if let Ok(v) = std::env::var(var) {
            if !v.is_empty() {
                return Ok(PathBuf::from(v));
            }
        }
    }
    Err(ConfigError::NoHome)
}

/// The Defense edition's data root. ADR-0093 binds each edition to its
/// own `~/.aberp-<edition>/`; this repository is Defense-only, so the
/// agent's default state lives under the Defense root. It is a *default*
/// rather than a compile-time lock because the agent holds no tenant
/// database — it is infrastructure state, and `ABERP_PORTAL_STATE_DIR`
/// is what the launchd plist sets.
pub const DEFENSE_DATA_DIRNAME: &str = ".aberp-defense";

/// Default agent state directory: `~/.aberp-defense/portal-agent/`.
pub fn default_state_dir() -> Result<PathBuf, ConfigError> {
    Ok(home()?.join(DEFENSE_DATA_DIRNAME).join("portal-agent"))
}

/// Parse `~/.aberp-defense/<tenant>/runtime.json` — the discovery file
/// `aberp serve` writes at boot (`apps/aberp/src/runtime_discovery.rs`).
///
/// Parsed rather than imported: depending on `apps/aberp` would drag
/// DuckDB, NAV and Tauri into a daemon that must keep running when all
/// of them are stopped. The file is three stable string fields.
///
/// Its **absence is itself a health signal** — no discovery file means
/// no ABERP boot completed, which the health surface reports as down
/// (§5.1) without ever needing ABERP to answer.
pub fn read_runtime_discovery(path: &Path) -> Option<(String, String)> {
    let raw = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let base = v.get("base_url")?.as_str()?.to_string();
    let fp = v.get("tls_fingerprint")?.as_str()?.to_string();
    if base.is_empty() || fp.is_empty() {
        return None;
    }
    Some((base, fp))
}

/// Path of the discovery file for `tenant`.
pub fn runtime_discovery_path(tenant: &str) -> Result<PathBuf, ConfigError> {
    Ok(home()?
        .join(DEFENSE_DATA_DIRNAME)
        .join(tenant)
        .join("runtime.json"))
}

impl AgentConfig {
    /// The `https://` origin the agent polls (ADR-0115 §2.1).
    ///
    /// Built from [`AgentConfig::relay_server_name`] rather than from
    /// [`AgentConfig::relay_addr`] so the TLS `ServerName` on the wire
    /// is the one the operator configured. Where the two differ, the
    /// poll loop pins the address with reqwest's `resolve` instead of
    /// letting DNS choose — see `crate::poll::connect`.
    ///
    /// The name is not a security control on this leg: Leg B is
    /// verified by leaf-certificate SHA-256 and the public WebPKI is
    /// not consulted at all (§2.3). It is here so a relay behind a name
    /// gets the SNI it expects.
    #[must_use]
    pub fn relay_base_url(&self) -> String {
        let port = self.relay_addr.rsplit_once(':').map_or("443", |(_, p)| p);
        format!("https://{}:{}", self.relay_server_name, port)
    }

    /// Build the config from the environment.
    ///
    /// Production sets `PORTAL_*` plus nothing else and gets: keychain
    /// secrets, `Secure` cookies, and Leg C discovered from
    /// `runtime.json`. Dev/test additionally sets the
    /// `ABERP_INTERNAL_*` trio (exactly as `run/dev-test.sh` already
    /// does for the storefront) and never touches the keychain.
    pub fn from_env() -> Result<Self, ConfigError> {
        let rp_id = std::env::var(PORTAL_HOST_ENV)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .ok_or(ConfigError::MissingHost(PORTAL_HOST_ENV))?
            .trim()
            .to_string();

        let origin = std::env::var("PORTAL_ORIGIN")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| format!("https://{rp_id}"));

        let relay_addr = env("PORTAL_RELAY_ADDR")?;
        let relay_server_name = std::env::var("PORTAL_RELAY_SERVER_NAME")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| {
                relay_addr
                    .rsplit_once(':')
                    .map_or(relay_addr.clone(), |(h, _)| h.to_string())
            });

        let fp_raw = env("PORTAL_RELAY_CERT_SHA256")?;
        let relay_fingerprint =
            PinnedFingerprint::from_hex(&fp_raw).map_err(|source| ConfigError::BadFingerprint {
                var: "PORTAL_RELAY_CERT_SHA256",
                source,
            })?;

        let client_cert_pem = PathBuf::from(env("PORTAL_AGENT_CERT_PEM")?);
        let client_key = match std::env::var("PORTAL_AGENT_KEY_PEM") {
            Ok(p) if !p.trim().is_empty() => SecretSource::File(PathBuf::from(p)),
            _ => SecretSource::Keychain {
                service: std::env::var("PORTAL_AGENT_KEY_KEYCHAIN_SERVICE")
                    .unwrap_or_else(|_| "aberp.portal-agent".to_string()),
                account: "mtls_client_key".to_string(),
            },
        };

        let state_dir = match std::env::var("ABERP_PORTAL_STATE_DIR") {
            Ok(p) if !p.trim().is_empty() => PathBuf::from(p),
            _ => default_state_dir()?,
        };

        let tenant = std::env::var("ABERP_TENANT").unwrap_or_else(|_| "test".to_string());

        // Leg C. The explicit trio wins so `run/dev-test.sh`-style
        // launches need no keychain; otherwise discover from
        // runtime.json + keychain, which is the production path.
        let (base_url, tls_fingerprint, discovery) = match (
            std::env::var("ABERP_INTERNAL_BASE_URL")
                .ok()
                .filter(|v| !v.is_empty()),
            std::env::var("ABERP_INTERNAL_TLS_FINGERPRINT")
                .ok()
                .filter(|v| !v.is_empty()),
        ) {
            (Some(b), Some(f)) => (b, f, UpstreamDiscovery::Fixed),
            _ => {
                let (b, f) = runtime_discovery_path(&tenant)
                    .ok()
                    .as_deref()
                    .and_then(read_runtime_discovery)
                    .unwrap_or_default();
                (
                    b,
                    f,
                    UpstreamDiscovery::RuntimeJson {
                        tenant: tenant.clone(),
                    },
                )
            }
        };
        let bearer = match std::env::var("ABERP_INTERNAL_BEARER") {
            Ok(v) if !v.is_empty() => SecretSource::Inline(v),
            _ => SecretSource::Keychain {
                service: format!("aberp.nav.{tenant}"),
                account: "session_token".to_string(),
            },
        };

        let tripwire_path = std::env::var("PORTAL_TRIPWIRE_PATH")
            .ok()
            .filter(|v| v.starts_with('/'))
            .unwrap_or_else(|| aberp_portal_core::canary::DEFAULT_TRIPWIRE_PATH.to_string());

        // `PORTAL_ALERT_SINK`: `smtp` (default), `file:<path>`, `off`.
        // The default is the real one — an operator who never sets the
        // variable gets alerts, not silence.
        let alert_sink = match std::env::var("PORTAL_ALERT_SINK").as_deref() {
            Ok("off") => crate::alert::AlertSink::Disabled,
            Ok(v) if v.starts_with("file:") => {
                crate::alert::AlertSink::File(PathBuf::from(&v["file:".len()..]))
            }
            _ => crate::alert::AlertSink::Spoc {
                seller_toml: home()?
                    .join(DEFENSE_DATA_DIRNAME)
                    .join(&tenant)
                    .join("seller.toml"),
                // The SAME keychain entry `apps/aberp` uses — the SPOC
                // is single in configuration even where it is not yet
                // single in code (see `alert.rs`).
                password: SecretSource::Keychain {
                    service: format!("aberp.smtp.{tenant}"),
                    account: "smtp_password".to_string(),
                },
                to: std::env::var("PORTAL_ALERT_TO")
                    .ok()
                    .filter(|v| !v.is_empty()),
            },
        };

        // Opt-OUT, never opt-in: an unset variable yields `Secure`.
        let cookie_secure = std::env::var("PORTAL_COOKIE_INSECURE_FOR_TEST")
            .map(|v| v != "1")
            .unwrap_or(true);

        Ok(Self {
            rp_id,
            origin,
            rp_name: std::env::var("PORTAL_RP_NAME").unwrap_or_else(|_| "ABERP".to_string()),
            relay_addr,
            relay_server_name,
            relay_fingerprint,
            client_cert_pem,
            client_key,
            state_dir,
            upstream: UpstreamConfig {
                base_url,
                tls_fingerprint,
                bearer,
            },
            discovery,
            cookie_secure,
            tripwire_path,
            alert_sink,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_discovery_parses_the_serve_written_shape() {
        let dir = std::env::temp_dir().join(format!("portal-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let p = dir.join("runtime.json");
        std::fs::write(
            &p,
            r#"{"tenant":"test","base_url":"https://127.0.0.1:18443","tls_fingerprint":"ab","started_at":"x"}"#,
        )
        .expect("write");
        assert_eq!(
            read_runtime_discovery(&p),
            Some(("https://127.0.0.1:18443".into(), "ab".into()))
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_discovery_file_is_none_not_a_panic() {
        // "ABERP has never booted" must be a health signal, not a crash.
        assert_eq!(
            read_runtime_discovery(Path::new("/nonexistent/x.json")),
            None
        );
    }

    #[test]
    fn discovery_file_missing_fields_is_none() {
        let dir = std::env::temp_dir().join(format!("portal-cfg2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let p = dir.join("runtime.json");
        std::fs::write(&p, r#"{"tenant":"test"}"#).expect("write");
        assert_eq!(read_runtime_discovery(&p), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn inline_secret_reads_back() {
        assert_eq!(
            SecretSource::Inline("t0ken".into()).read().expect("read"),
            "t0ken"
        );
    }
}
