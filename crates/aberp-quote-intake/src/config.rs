//! Env-var-driven config for the quote-intake daemon (S210).
//! S211 / PR-210 adds [`QuoteIntakeConfig::from_toml_and_keychain_parts`]
//! so the operator can configure the daemon via the Tenant Settings UI
//! (with the bearer token in the OS keychain).
//!
//! Env vars (S210):
//! - `ABERP_QUOTE_INTAKE_URL`            — base URL.
//! - `ABERP_QUOTE_INTAKE_TOKEN`          — bearer token (`Zeroizing`).
//! - `ABERP_QUOTE_INTAKE_INTERVAL_SECS`  — cadence (default 60).
//! - `ABERP_QUOTE_INTAKE_ENABLED`        — `true` to spawn.
//!
//! Refuse-to-start: `ENABLED=true` + missing URL/TOKEN → `Config` err.
//!
//! Precedence (S211): env vars take precedence over the toml+keychain
//! source. The caller in `serve.rs` tries env first; if it returns
//! `Disabled`, it falls back to toml+keychain — this keeps the env-var
//! ops escape hatch intact while giving the SPA a clean config surface.

use std::time::Duration;

use zeroize::Zeroizing;

use crate::error::QuoteIntakeError;

pub const MIN_POLL_INTERVAL_SECS: u64 = 10;
pub const MAX_POLL_INTERVAL_SECS: u64 = 3600;
pub const DEFAULT_POLL_INTERVAL_SECS: u64 = 60;

#[derive(Debug, Clone)]
pub struct QuoteIntakeConfig {
    pub base_url: String,
    pub bearer_token: Zeroizing<String>,
    pub poll_interval: Duration,
    pub enabled: bool,
}

impl QuoteIntakeConfig {
    pub fn from_env() -> Result<Self, QuoteIntakeError> {
        let enabled = std::env::var("ABERP_QUOTE_INTAKE_ENABLED")
            .ok()
            .map(|v| v.trim().eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        if !enabled {
            return Err(QuoteIntakeError::Disabled);
        }

        let base_url = std::env::var("ABERP_QUOTE_INTAKE_URL")
            .map_err(|_| QuoteIntakeError::Config("ABERP_QUOTE_INTAKE_URL not set".to_string()))?
            .trim()
            .trim_end_matches('/')
            .to_string();
        if base_url.is_empty() {
            return Err(QuoteIntakeError::Config(
                "ABERP_QUOTE_INTAKE_URL is empty".to_string(),
            ));
        }
        if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
            return Err(QuoteIntakeError::Config(format!(
                "ABERP_QUOTE_INTAKE_URL must start with http:// or https:// (got {base_url:?})"
            )));
        }

        let token = std::env::var("ABERP_QUOTE_INTAKE_TOKEN").map_err(|_| {
            QuoteIntakeError::Config("ABERP_QUOTE_INTAKE_TOKEN not set".to_string())
        })?;
        let token = token.trim().to_string();
        if token.is_empty() {
            return Err(QuoteIntakeError::Config(
                "ABERP_QUOTE_INTAKE_TOKEN is empty".to_string(),
            ));
        }

        let interval_secs = match std::env::var("ABERP_QUOTE_INTAKE_INTERVAL_SECS") {
            Ok(s) => s.trim().parse::<u64>().map_err(|_| {
                QuoteIntakeError::Config(format!(
                    "ABERP_QUOTE_INTAKE_INTERVAL_SECS not an integer: {s:?}"
                ))
            })?,
            Err(_) => DEFAULT_POLL_INTERVAL_SECS,
        };
        let interval_secs = interval_secs.clamp(MIN_POLL_INTERVAL_SECS, MAX_POLL_INTERVAL_SECS);

        Ok(Self {
            base_url,
            bearer_token: Zeroizing::new(token),
            poll_interval: Duration::from_secs(interval_secs),
            enabled: true,
        })
    }

    /// S211 / PR-210 — assemble a config from operator-provided parts:
    /// non-secret `base_url` + `poll_interval_secs` from
    /// `[quote_intake]` in seller.toml, plus the bearer token read by
    /// the caller from the OS keychain. The caller (serve.rs boot
    /// block) checks the toml `enabled` flag BEFORE invoking this, so
    /// reaching here implies the operator opted in.
    ///
    /// Refuse-to-start mirrors `from_env`: empty URL / wrong scheme /
    /// empty token return a typed `Config` error.
    pub fn from_toml_and_keychain_parts(
        base_url: String,
        bearer_token: Zeroizing<String>,
        poll_interval_secs: Option<u64>,
    ) -> Result<Self, QuoteIntakeError> {
        let base_url = base_url.trim().trim_end_matches('/').to_string();
        if base_url.is_empty() {
            return Err(QuoteIntakeError::Config(
                "[quote_intake] base_url is empty".to_string(),
            ));
        }
        if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
            return Err(QuoteIntakeError::Config(format!(
                "[quote_intake] base_url must start with http:// or https:// (got {base_url:?})"
            )));
        }
        let token_str: &str = &bearer_token;
        if token_str.trim().is_empty() {
            return Err(QuoteIntakeError::Config(
                "[quote_intake] keychain bearer token is empty".to_string(),
            ));
        }
        let interval_secs = poll_interval_secs
            .unwrap_or(DEFAULT_POLL_INTERVAL_SECS)
            .clamp(MIN_POLL_INTERVAL_SECS, MAX_POLL_INTERVAL_SECS);
        Ok(Self {
            base_url,
            bearer_token,
            poll_interval: Duration::from_secs(interval_secs),
            enabled: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn clear() {
        std::env::remove_var("ABERP_QUOTE_INTAKE_ENABLED");
        std::env::remove_var("ABERP_QUOTE_INTAKE_URL");
        std::env::remove_var("ABERP_QUOTE_INTAKE_TOKEN");
        std::env::remove_var("ABERP_QUOTE_INTAKE_INTERVAL_SECS");
    }

    #[test]
    fn disabled_when_toggle_unset() {
        let _g = ENV_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
        clear();
        match QuoteIntakeConfig::from_env() {
            Err(QuoteIntakeError::Disabled) => {}
            other => panic!("expected Disabled, got {other:?}"),
        }
    }

    #[test]
    fn disabled_when_toggle_false() {
        let _g = ENV_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
        clear();
        std::env::set_var("ABERP_QUOTE_INTAKE_ENABLED", "false");
        assert!(matches!(
            QuoteIntakeConfig::from_env(),
            Err(QuoteIntakeError::Disabled)
        ));
        clear();
    }

    #[test]
    fn refuse_to_start_when_url_missing() {
        let _g = ENV_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
        clear();
        std::env::set_var("ABERP_QUOTE_INTAKE_ENABLED", "true");
        std::env::set_var("ABERP_QUOTE_INTAKE_TOKEN", "tok");
        match QuoteIntakeConfig::from_env() {
            Err(QuoteIntakeError::Config(m)) => assert!(m.contains("URL"), "{m}"),
            other => panic!("expected Config, got {other:?}"),
        }
        clear();
    }

    #[test]
    fn refuse_to_start_when_url_scheme_wrong() {
        let _g = ENV_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
        clear();
        std::env::set_var("ABERP_QUOTE_INTAKE_ENABLED", "true");
        std::env::set_var("ABERP_QUOTE_INTAKE_URL", "localhost:3000");
        std::env::set_var("ABERP_QUOTE_INTAKE_TOKEN", "tok");
        match QuoteIntakeConfig::from_env() {
            Err(QuoteIntakeError::Config(m)) => {
                assert!(m.contains("http://") || m.contains("https://"), "{m}")
            }
            other => panic!("expected Config, got {other:?}"),
        }
        clear();
    }

    #[test]
    fn refuse_to_start_when_token_empty() {
        let _g = ENV_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
        clear();
        std::env::set_var("ABERP_QUOTE_INTAKE_ENABLED", "true");
        std::env::set_var("ABERP_QUOTE_INTAKE_URL", "http://localhost:3000");
        std::env::set_var("ABERP_QUOTE_INTAKE_TOKEN", "   ");
        match QuoteIntakeConfig::from_env() {
            Err(QuoteIntakeError::Config(m)) => assert!(m.contains("TOKEN"), "{m}"),
            other => panic!("expected Config, got {other:?}"),
        }
        clear();
    }

    #[test]
    fn happy_path_clamps_and_strips_trailing_slash() {
        let _g = ENV_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
        clear();
        std::env::set_var("ABERP_QUOTE_INTAKE_ENABLED", "TRUE");
        std::env::set_var("ABERP_QUOTE_INTAKE_URL", "http://localhost:3000/");
        std::env::set_var("ABERP_QUOTE_INTAKE_TOKEN", "  s3cret  ");
        std::env::set_var("ABERP_QUOTE_INTAKE_INTERVAL_SECS", "5");
        let cfg = QuoteIntakeConfig::from_env().expect("happy");
        assert_eq!(cfg.base_url, "http://localhost:3000");
        assert_eq!(&*cfg.bearer_token, "s3cret");
        assert_eq!(
            cfg.poll_interval,
            Duration::from_secs(MIN_POLL_INTERVAL_SECS)
        );
        assert!(cfg.enabled);
        clear();
    }

    #[test]
    fn happy_path_default_interval() {
        let _g = ENV_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
        clear();
        std::env::set_var("ABERP_QUOTE_INTAKE_ENABLED", "true");
        std::env::set_var("ABERP_QUOTE_INTAKE_URL", "https://aberp.example.com");
        std::env::set_var("ABERP_QUOTE_INTAKE_TOKEN", "t");
        let cfg = QuoteIntakeConfig::from_env().expect("happy");
        assert_eq!(
            cfg.poll_interval,
            Duration::from_secs(DEFAULT_POLL_INTERVAL_SECS)
        );
        clear();
    }

    // ── S211 / PR-210 — from_toml_and_keychain_parts pins ───────────

    #[test]
    fn toml_parts_happy_path() {
        let cfg = QuoteIntakeConfig::from_toml_and_keychain_parts(
            "https://aberp.example.com/".to_string(),
            Zeroizing::new("s3cret".to_string()),
            Some(120),
        )
        .expect("happy");
        assert_eq!(cfg.base_url, "https://aberp.example.com");
        assert_eq!(&*cfg.bearer_token, "s3cret");
        assert_eq!(cfg.poll_interval, Duration::from_secs(120));
        assert!(cfg.enabled);
    }

    #[test]
    fn toml_parts_default_interval_when_none() {
        let cfg = QuoteIntakeConfig::from_toml_and_keychain_parts(
            "http://localhost:3000".to_string(),
            Zeroizing::new("t".to_string()),
            None,
        )
        .expect("happy");
        assert_eq!(
            cfg.poll_interval,
            Duration::from_secs(DEFAULT_POLL_INTERVAL_SECS)
        );
    }

    #[test]
    fn toml_parts_clamps_interval() {
        let too_low = QuoteIntakeConfig::from_toml_and_keychain_parts(
            "http://x".to_string(),
            Zeroizing::new("t".to_string()),
            Some(1),
        )
        .expect("happy");
        assert_eq!(
            too_low.poll_interval,
            Duration::from_secs(MIN_POLL_INTERVAL_SECS)
        );
        let too_high = QuoteIntakeConfig::from_toml_and_keychain_parts(
            "http://x".to_string(),
            Zeroizing::new("t".to_string()),
            Some(999_999),
        )
        .expect("happy");
        assert_eq!(
            too_high.poll_interval,
            Duration::from_secs(MAX_POLL_INTERVAL_SECS)
        );
    }

    #[test]
    fn toml_parts_refuse_empty_url() {
        match QuoteIntakeConfig::from_toml_and_keychain_parts(
            "   ".to_string(),
            Zeroizing::new("t".to_string()),
            None,
        ) {
            Err(QuoteIntakeError::Config(m)) => assert!(m.contains("base_url"), "{m}"),
            other => panic!("expected Config, got {other:?}"),
        }
    }

    #[test]
    fn toml_parts_refuse_wrong_scheme() {
        match QuoteIntakeConfig::from_toml_and_keychain_parts(
            "localhost:3000".to_string(),
            Zeroizing::new("t".to_string()),
            None,
        ) {
            Err(QuoteIntakeError::Config(m)) => {
                assert!(m.contains("http://") || m.contains("https://"), "{m}")
            }
            other => panic!("expected Config, got {other:?}"),
        }
    }

    #[test]
    fn toml_parts_refuse_empty_token() {
        match QuoteIntakeConfig::from_toml_and_keychain_parts(
            "http://x".to_string(),
            Zeroizing::new("   ".to_string()),
            None,
        ) {
            Err(QuoteIntakeError::Config(m)) => assert!(m.contains("token"), "{m}"),
            other => panic!("expected Config, got {other:?}"),
        }
    }
}
