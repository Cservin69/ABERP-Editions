//! Crate-level error type. `thiserror`-derived per ADR-0021 Part A item 2.
//!
//! # No bearer-token leaks
//!
//! `Unauthorized` carries NO message body containing the token. The
//! transport layer constructs the `Authorization: Bearer <…>` header
//! locally and never threads the secret through `reqwest::Error`'s
//! `Display` impl.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum QuoteIntakeError {
    #[error("quote-intake daemon disabled via ABERP_QUOTE_INTAKE_ENABLED")]
    Disabled,

    #[error("quote-intake config error: {0}")]
    Config(String),

    #[error("HTTP transport error: {0}")]
    Transport(String),

    #[error("quote-intake unauthorized (401) — check ABERP_QUOTE_INTAKE_TOKEN")]
    Unauthorized,

    #[error("quote-intake unavailable (503) — sister service not ready")]
    ServiceUnavailable,

    #[error("quote-intake unexpected HTTP status {status}")]
    UnexpectedStatus { status: u16 },

    #[error("quote-intake response parse error: {0}")]
    Parse(String),

    #[error("quote-intake DuckDB error: {0}")]
    Storage(String),

    #[error("quote-intake mapping error for quote {quote_id}: {message}")]
    Mapping { quote_id: String, message: String },
}

impl QuoteIntakeError {
    /// Did this error abort the whole cycle, or just one row?
    pub fn is_cycle_aborting(&self) -> bool {
        matches!(
            self,
            QuoteIntakeError::Transport(_)
                | QuoteIntakeError::Unauthorized
                | QuoteIntakeError::ServiceUnavailable
                | QuoteIntakeError::UnexpectedStatus { .. }
                | QuoteIntakeError::Config(_)
                | QuoteIntakeError::Disabled
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_strings_do_not_carry_token_bytes() {
        for e in [
            QuoteIntakeError::Disabled,
            QuoteIntakeError::Config("URL missing".to_string()),
            QuoteIntakeError::Transport("DNS".to_string()),
            QuoteIntakeError::Unauthorized,
            QuoteIntakeError::ServiceUnavailable,
            QuoteIntakeError::UnexpectedStatus { status: 500 },
            QuoteIntakeError::Parse("missing field".to_string()),
            QuoteIntakeError::Storage("locked".to_string()),
            QuoteIntakeError::Mapping {
                quote_id: "q-abc".to_string(),
                message: "no email".to_string(),
            },
        ] {
            let s = e.to_string();
            assert!(!s.contains("Bearer"), "{s:?}");
            assert!(!s.contains("bearer"), "{s:?}");
        }
    }

    #[test]
    fn cycle_aborting_classification() {
        assert!(QuoteIntakeError::Unauthorized.is_cycle_aborting());
        assert!(QuoteIntakeError::ServiceUnavailable.is_cycle_aborting());
        assert!(QuoteIntakeError::Transport("x".into()).is_cycle_aborting());
        assert!(!QuoteIntakeError::Parse("x".into()).is_cycle_aborting());
        assert!(!QuoteIntakeError::Storage("x".into()).is_cycle_aborting());
        assert!(!QuoteIntakeError::Mapping {
            quote_id: "q".into(),
            message: "x".into()
        }
        .is_cycle_aborting());
    }
}
