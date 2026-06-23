//! S291 / PR-272 — runtime discovery file.
//!
//! ## Why this exists
//!
//! Pre-S291 the dev-test path (PROD_v2.27.4..v2.27.6 evening tests)
//! required the operator to `lsof -iTCP:LISTEN | grep aberp` to find
//! ABERP's kernel-assigned loopback port, then hand-paste it into the
//! storefront's `ABERP_INTERNAL_BASE_URL` env var on every launch. The
//! port shifts on every restart (`args.port=0` → kernel picks) and the
//! storefront has no way to discover it. That's a [[trust-code-not-operator]]
//! gap, fixed two ways in PR-272:
//!
//! - `ABERP_HTTPS_PORT` env var pins the port (so the URL is stable
//!   across restarts);
//! - this module writes a tiny JSON descriptor next to the existing
//!   serve artifacts so the storefront's dev launcher can read the
//!   resolved port + TLS fingerprint + keychain pointer without
//!   `lsof` or operator memory.
//!
//! ## Layout
//!
//! `~/.aberp/<tenant>/runtime.json` — per brief D. Distinct from the
//! existing `~/.aberp/serve/<tenant>/` (cert PEM + key PEM + fingerprint
//! file) so a stale runtime.json from a crashed prior run is trivially
//! identifiable by mtime and survives a `rm -rf ~/.aberp/serve/<tenant>`
//! cert-rotation. The file is overwritten on every successful boot and
//! deleted on graceful shutdown; a leaked file from a crash is fine —
//! the next boot rewrites it.
//!
//! ## What the file is NOT
//!
//! Not a secret store. The bearer token lives in the keychain — the
//! discovery file only points at the keychain entry by name. Per
//! [[trust-code-not-operator]] safety belongs in code: a `chmod 0600`
//! file isn't an auth surface, the keychain is.

use std::fs;
use std::io::Write as _;
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};

/// Resolve `~/.aberp/<tenant>/runtime.json`. Uses HOME / USERPROFILE so
/// we don't take a `dirs` dep — matches `serve_artifacts_dir`'s posture
/// in `serve.rs`.
pub fn runtime_file_path(tenant: &str) -> Result<PathBuf> {
    let home = if let Ok(h) = std::env::var("HOME") {
        if !h.is_empty() {
            PathBuf::from(h)
        } else if let Ok(p) = std::env::var("USERPROFILE") {
            PathBuf::from(p)
        } else {
            return Err(anyhow!(
                "neither HOME nor USERPROFILE is set — cannot locate ~/.aberp/<tenant>/runtime.json"
            ));
        }
    } else if let Ok(p) = std::env::var("USERPROFILE") {
        PathBuf::from(p)
    } else {
        return Err(anyhow!(
            "neither HOME nor USERPROFILE is set — cannot locate ~/.aberp/<tenant>/runtime.json"
        ));
    };
    Ok(home
        .join(crate::build_profile::edition_data_dirname())
        .join(tenant)
        .join("runtime.json"))
}

/// Inputs for [`write`]. Owned strings keep the call site free of
/// lifetime gymnastics at the boot seam.
pub struct RuntimeDiscovery {
    pub tenant: String,
    /// `https://127.0.0.1:<port>` — the resolved listener address, with
    /// scheme. The storefront's dev launcher uses this directly as
    /// `ABERP_INTERNAL_BASE_URL`.
    pub base_url: String,
    /// SHA-256 hex of the loopback TLS cert. The storefront's TLS
    /// client pins on this; same value the SPA pins on via the
    /// handshake line.
    pub tls_fingerprint: String,
    /// RFC-3339 / ISO-8601 timestamp of boot. Used by the storefront
    /// launcher as a sanity check ("this file is from THIS boot, not
    /// a stale crash leftover").
    pub started_at: String,
    /// Single string of the form `<service>.<account>` pointing at the
    /// keychain entry that holds the email-relay bearer. Per brief D
    /// the launcher reads this and runs `security find-generic-password
    /// -s <service> -a <account>` (after splitting on the last `.`) to
    /// fetch the token without ever putting plaintext on disk.
    pub relay_token_keychain_service: String,
}

/// Write the discovery file. Parent dir is created if missing.
/// Per CLAUDE.md rule 12 a partial / unreadable file is worse than no
/// file at all, so we write to a sibling `.tmp` and rename — atomic on
/// the same filesystem.
pub fn write(d: &RuntimeDiscovery) -> Result<PathBuf> {
    let path = runtime_file_path(&d.tenant)?;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("runtime.json path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent).with_context(|| {
        format!(
            "create runtime-discovery parent dir at {}",
            parent.display()
        )
    })?;

    // Hand-rolled JSON. Three reasons over `serde_json::to_string_pretty`:
    // 1. The shape is tiny and the consumer is a bash `jq`/`python` one-
    //    liner — no schema evolution surface.
    // 2. We don't pull a new dep on `serde_json` here (the workspace
    //    already has it, but the boot path stays sequential and readable).
    // 3. Keys are sorted by hand so a diff between two runs reads cleanly
    //    (only `base_url` and `started_at` should change across reboots
    //    on the same tenant — the rest is stable).
    let body = format!(
        "{{\n  \"base_url\": {},\n  \"relay_token_keychain_service\": {},\n  \"started_at\": {},\n  \"tenant\": {},\n  \"tls_fingerprint\": {}\n}}\n",
        json_string(&d.base_url),
        json_string(&d.relay_token_keychain_service),
        json_string(&d.started_at),
        json_string(&d.tenant),
        json_string(&d.tls_fingerprint),
    );

    let tmp = path.with_extension("json.tmp");
    {
        let mut f = fs::File::create(&tmp)
            .with_context(|| format!("create runtime-discovery tmp at {}", tmp.display()))?;
        f.write_all(body.as_bytes())
            .with_context(|| format!("write runtime-discovery tmp at {}", tmp.display()))?;
        f.sync_all().ok();
    }
    fs::rename(&tmp, &path).with_context(|| {
        format!(
            "atomic-rename runtime-discovery {} → {}",
            tmp.display(),
            path.display()
        )
    })?;
    Ok(path)
}

/// Delete the discovery file. Called on graceful shutdown. Best-effort:
/// a missing file is success (idempotent), other errors log and
/// continue — never blocks shutdown.
pub fn delete(tenant: &str) -> Result<bool> {
    let path = runtime_file_path(tenant)?;
    match fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(anyhow!(
            "delete runtime-discovery at {}: {e}",
            path.display()
        )),
    }
}

/// Minimal JSON string encoder for ASCII URLs / hex / RFC-3339
/// timestamps / closed-vocab keychain service strings. Escapes the
/// six characters JSON-mandated for a string body (CLAUDE.md rule 12 —
/// loud-fail on the impossible cases would be wrong here; bytes in a
/// URL or fingerprint hex are always printable). Non-ASCII control
/// codepoints round-trip as `\uXXXX`.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ulid::Ulid;

    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Per-test HOME swap. The same ENV_MUTEX guards both the swap and
    /// the workspace-shared HOME so a parallel test cannot torpedo the
    /// path resolution. The pattern mirrors `seller_toml_backup.rs`'s
    /// `with_temp_home` (CLAUDE.md rule 11 — match existing test
    /// conventions).
    fn with_temp_home<F: FnOnce(&PathBuf, &str)>(f: F) {
        let _g = ENV_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
        let prior_home = std::env::var("HOME").ok();
        let unique = format!("aberp-runtime-discovery-{}", Ulid::new());
        let tmp = std::env::temp_dir().join(&unique);
        std::fs::create_dir_all(&tmp).expect("mk temp HOME");
        std::env::set_var("HOME", &tmp);
        let tenant = format!("test-{}", Ulid::new());
        f(&tmp, &tenant);
        let _ = std::fs::remove_dir_all(&tmp);
        if let Some(h) = prior_home {
            std::env::set_var("HOME", h);
        } else {
            std::env::remove_var("HOME");
        }
    }

    fn sample(tenant: &str) -> RuntimeDiscovery {
        RuntimeDiscovery {
            tenant: tenant.into(),
            base_url: "https://127.0.0.1:18443".into(),
            tls_fingerprint: "0a1b2c3d4e5f60718293a4b5c6d7e8f90011223344556677889900aabbccddeeff"
                .into(),
            started_at: "2026-06-08T20:00:00Z".into(),
            relay_token_keychain_service: format!("aberp.email_relay.{tenant}.email_relay_token"),
        }
    }

    #[test]
    fn runtime_path_under_home_dot_aberp_tenant() {
        with_temp_home(|home, tenant| {
            let p = runtime_file_path(tenant).unwrap();
            assert_eq!(
                p,
                home.join(crate::build_profile::edition_data_dirname())
                    .join(tenant)
                    .join("runtime.json")
            );
        });
    }

    #[test]
    fn write_creates_parent_dir_and_atomic_renames() {
        with_temp_home(|home, tenant| {
            let path = write(&sample(tenant)).expect("write ok");
            assert!(path.exists());
            assert_eq!(
                path,
                home.join(crate::build_profile::edition_data_dirname())
                    .join(tenant)
                    .join("runtime.json")
            );
            let tmp = path.with_extension("json.tmp");
            assert!(!tmp.exists(), "tmp sibling lingered: {tmp:?}");
        });
    }

    #[test]
    fn write_body_is_valid_json_with_expected_keys() {
        with_temp_home(|_home, tenant| {
            let path = write(&sample(tenant)).unwrap();
            let body = fs::read_to_string(&path).unwrap();
            let parsed: serde_json::Value =
                serde_json::from_str(&body).expect("file body is valid JSON");
            assert_eq!(parsed["tenant"], tenant);
            assert_eq!(parsed["base_url"], "https://127.0.0.1:18443");
            assert_eq!(
                parsed["tls_fingerprint"],
                "0a1b2c3d4e5f60718293a4b5c6d7e8f90011223344556677889900aabbccddeeff"
            );
            assert_eq!(parsed["started_at"], "2026-06-08T20:00:00Z");
            assert_eq!(
                parsed["relay_token_keychain_service"],
                format!("aberp.email_relay.{tenant}.email_relay_token")
            );
        });
    }

    #[test]
    fn delete_returns_true_after_write_and_false_when_missing() {
        with_temp_home(|_home, tenant| {
            write(&sample(tenant)).unwrap();
            assert!(delete(tenant).unwrap(), "first delete removes file");
            assert!(!delete(tenant).unwrap(), "second delete is no-op");
        });
    }

    #[test]
    fn write_overwrites_prior_file() {
        with_temp_home(|_home, tenant| {
            let d1 = sample(tenant);
            let path = write(&d1).unwrap();
            let d2 = RuntimeDiscovery {
                base_url: "https://127.0.0.1:22222".into(),
                started_at: "2026-06-08T20:00:00Z".into(),
                ..d1
            };
            write(&d2).unwrap();
            let body = fs::read_to_string(&path).unwrap();
            assert!(body.contains("22222"), "{body}");
            assert!(!body.contains("18443"), "{body}");
        });
    }

    #[test]
    fn json_string_escapes_control_chars_and_quotes() {
        assert_eq!(json_string("a\"b"), "\"a\\\"b\"");
        assert_eq!(json_string("a\\b"), "\"a\\\\b\"");
        assert_eq!(json_string("a\nb"), "\"a\\nb\"");
        assert_eq!(json_string("a\x01b"), "\"a\\u0001b\"");
    }
}
