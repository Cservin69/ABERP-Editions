//! S211 / PR-210 — Quote-intake bearer token in the OS keychain.
//!
//! Mirrors [`crate::smtp_credentials`]: the bearer token lives in the
//! OS keychain ONLY, never on disk, never in TOML, never in logs.
//! Non-secret quote-intake settings (URL, interval, enabled flag) live
//! in `[quote_intake]` of `~/.aberp/<tenant>/seller.toml` per the
//! keychain/TOML split anchored by [[trust-code-not-operator]].
//!
//! Service-and-account naming convention (stable across platforms via
//! the `keyring` crate's abstraction):
//!
//!   service:  `aberp.quote_intake.<tenant_id>`
//!   account:  `quote_intake_token`
//!
//! # Security
//!
//! - Read/write/delete are the only operations exposed.
//! - The token is wrapped in `Zeroizing<String>` on read so the
//!   buffer is overwritten on drop.
//! - No `Debug` impl on the token string — accidental
//!   `tracing::debug!(?token)` would not compile.

use keyring::Entry;
use zeroize::Zeroizing;

/// Item-name for the quote-intake bearer-token keychain entry.
pub const ITEM_QUOTE_INTAKE_TOKEN: &str = "quote_intake_token";

/// Compose the keychain `service` field for a tenant.
pub fn service_name(tenant_id: &str) -> String {
    format!("aberp.quote_intake.{tenant_id}")
}

#[derive(Debug)]
pub enum QuoteIntakeCredentialsError {
    Missing { tenant_id: String },
    Backend(String),
}

impl std::fmt::Display for QuoteIntakeCredentialsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QuoteIntakeCredentialsError::Missing { tenant_id } => write!(
                f,
                "quote-intake bearer token is not set in the keychain for tenant `{tenant_id}`"
            ),
            QuoteIntakeCredentialsError::Backend(msg) => {
                write!(f, "keychain backend error: {msg}")
            }
        }
    }
}

impl std::error::Error for QuoteIntakeCredentialsError {}

/// Write the bearer token to the OS keychain for `tenant_id`.
/// Overwrites any existing entry. Per CLAUDE.md rule 12 the validation
/// (non-empty) happens at the route layer; this seam writes whatever
/// it's given so the rotation surface stays simple.
pub fn write_token(tenant_id: &str, token: &str) -> Result<(), QuoteIntakeCredentialsError> {
    let service = service_name(tenant_id);
    let entry = Entry::new(&service, ITEM_QUOTE_INTAKE_TOKEN)
        .map_err(|e| QuoteIntakeCredentialsError::Backend(format!("Entry::new: {e}")))?;
    entry
        .set_password(token)
        .map_err(|e| QuoteIntakeCredentialsError::Backend(format!("set_password: {e}")))
}

/// Read the bearer token from the OS keychain. Wrapped in `Zeroizing`.
pub fn read_token(tenant_id: &str) -> Result<Zeroizing<String>, QuoteIntakeCredentialsError> {
    let service = service_name(tenant_id);
    let entry = Entry::new(&service, ITEM_QUOTE_INTAKE_TOKEN)
        .map_err(|e| QuoteIntakeCredentialsError::Backend(format!("Entry::new: {e}")))?;
    match entry.get_password() {
        Ok(s) => Ok(Zeroizing::new(s)),
        Err(keyring::Error::NoEntry) => Err(QuoteIntakeCredentialsError::Missing {
            tenant_id: tenant_id.to_string(),
        }),
        Err(other) => Err(QuoteIntakeCredentialsError::Backend(format!(
            "get_password: {other}"
        ))),
    }
}

/// Delete the bearer-token keychain entry for `tenant_id`. Idempotent.
#[allow(dead_code)]
pub fn delete_token(tenant_id: &str) -> Result<bool, QuoteIntakeCredentialsError> {
    let service = service_name(tenant_id);
    let entry = Entry::new(&service, ITEM_QUOTE_INTAKE_TOKEN)
        .map_err(|e| QuoteIntakeCredentialsError::Backend(format!("Entry::new: {e}")))?;
    match entry.delete_password() {
        Ok(()) => Ok(true),
        Err(keyring::Error::NoEntry) => Ok(false),
        Err(other) => Err(QuoteIntakeCredentialsError::Backend(format!(
            "delete_password: {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_name_format_is_stable() {
        assert_eq!(service_name("acme"), "aberp.quote_intake.acme");
        assert_eq!(
            service_name("t-uuid-1234"),
            "aberp.quote_intake.t-uuid-1234"
        );
    }

    #[test]
    fn item_name_is_stable() {
        assert_eq!(ITEM_QUOTE_INTAKE_TOKEN, "quote_intake_token");
    }

    #[test]
    fn quote_intake_service_name_does_not_collide_with_nav_or_smtp() {
        let tenant = "production";
        let qi = service_name(tenant);
        let smtp = crate::smtp_credentials::service_name(tenant);
        let nav = aberp_nav_transport::credentials::keychain::service_name(tenant);
        assert_ne!(qi, smtp);
        assert_ne!(qi, nav);
        assert!(qi.starts_with("aberp.quote_intake."));
    }
}
