//! Env-var-driven config for the quote-intake daemon (S210).
//! S211 migrates to keychain + tenant settings UI.
//!
//! Env vars:
//! - `ABERP_QUOTE_INTAKE_URL`            — base URL.
//! - `ABERP_QUOTE_INTAKE_TOKEN`          — bearer token (`Zeroizing`).
//! - `ABERP_QUOTE_INTAKE_INTERVAL_SECS`  — cadence (default 60).
//! - `ABERP_QUOTE_INTAKE_ENABLED`        — `true` to spawn.
//!
//! Refuse-to-start: `ENABLED=true` + missing URL/TOKEN → `Config` err.

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
}
