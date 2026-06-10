//! S339 / PR-24 — the storefront **origin shared secret** in the OS
//! keychain (optional, deploy-infra credential).
//!
//! ## Why this exists
//!
//! The storefront fronts its origin (Lightsail/adapter-node) with
//! CloudFront. Its global request guard (`src/hooks.server.ts`) rejects
//! every non-`/healthz` request that does **not** carry an
//! `X-CloudFront-Secret` header matching the storefront's
//! `CLOUDFRONT_SHARED_SECRET` env var with `403 "forbidden: missing
//! origin signature"`. The error string says "origin signature" but the
//! check is a **static shared-secret header compare**, NOT an HMAC —
//! there is no signing, no canonical string, no timestamp (verified
//! S339 cross-repo read of `ABERP-site/src/hooks.server.ts` +
//! `src/lib/server/auth.ts`).
//!
//! CloudFront is configured to inject that header on origin requests, so
//! traffic that traverses CloudFront passes automatically. ABERP's
//! catalogue push, however, can hit the origin on a path/behaviour that
//! CloudFront does not cover (CloudFront behaviours are per-path — see
//! S249 finding 23), in which case the header is absent and the push
//! 403s (surfacing as the daemon's `unexpected_status` outcome, since
//! 403 ≠ 401). Letting ABERP send the same secret itself closes that
//! gap for the direct/origin path.
//!
//! ## Posture (mirrors [`crate::quote_intake_credentials`] /
//! [`crate::smtp_credentials`])
//!
//! - The secret lives in the OS keychain ONLY (or an env override for
//!   the dev-test launcher / CI). Never on disk, never in TOML, never
//!   in logs.
//! - It is **optional**: a tenant that hasn't provisioned it (the common
//!   case today) resolves to `None` and the catalogue push behaves
//!   exactly as before — no header, relies on CloudFront injecting it.
//!   So provisioning the secret is purely additive and reversible.
//! - Read wraps in `Zeroizing<String>` so the buffer is wiped on drop.
//!
//! Service-and-account naming (stable across platforms via `keyring`):
//!
//!   service:  `aberp.storefront.<tenant_id>`
//!   account:  `storefront_origin_secret`
//!
//! Env override (highest precedence): `ABERP_STOREFRONT_ORIGIN_SECRET`.

use keyring::Entry;
use zeroize::Zeroizing;

/// Item-name for the origin-secret keychain entry.
pub const ITEM_ORIGIN_SECRET: &str = "storefront_origin_secret";

/// Env override consulted before the keychain. The dev-test launcher /
/// CI set this so a local run can carry the header without touching the
/// OS keychain.
pub const ORIGIN_SECRET_ENV: &str = "ABERP_STOREFRONT_ORIGIN_SECRET";

/// Compose the keychain `service` field for a tenant.
pub fn service_name(tenant_id: &str) -> String {
    format!("aberp.storefront.{tenant_id}")
}

#[derive(Debug)]
pub enum StorefrontOriginSecretError {
    Backend(String),
}

impl std::fmt::Display for StorefrontOriginSecretError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StorefrontOriginSecretError::Backend(msg) => {
                write!(f, "keychain backend error: {msg}")
            }
        }
    }
}

impl std::error::Error for StorefrontOriginSecretError {}

/// Write the origin secret to the OS keychain for `tenant_id`.
/// Overwrites any existing entry. Non-empty validation is the caller's
/// job (matches `quote_intake_credentials::write_token`).
#[allow(dead_code)]
pub fn write_secret(tenant_id: &str, secret: &str) -> Result<(), StorefrontOriginSecretError> {
    let service = service_name(tenant_id);
    let entry = Entry::new(&service, ITEM_ORIGIN_SECRET)
        .map_err(|e| StorefrontOriginSecretError::Backend(format!("Entry::new: {e}")))?;
    entry
        .set_password(secret)
        .map_err(|e| StorefrontOriginSecretError::Backend(format!("set_password: {e}")))
}

/// Delete the origin-secret keychain entry. Idempotent.
#[allow(dead_code)]
pub fn delete_secret(tenant_id: &str) -> Result<bool, StorefrontOriginSecretError> {
    let service = service_name(tenant_id);
    let entry = Entry::new(&service, ITEM_ORIGIN_SECRET)
        .map_err(|e| StorefrontOriginSecretError::Backend(format!("Entry::new: {e}")))?;
    match entry.delete_password() {
        Ok(()) => Ok(true),
        Err(keyring::Error::NoEntry) => Ok(false),
        Err(other) => Err(StorefrontOriginSecretError::Backend(format!(
            "delete_password: {other}"
        ))),
    }
}

/// Resolve the optional origin secret for `tenant_id`. Precedence:
/// env override → keychain → `None`.
///
/// **Boot-resilient by design.** A missing keychain entry is the
/// expected default (`None`, no header). A keychain *backend* error is
/// logged at WARN and also degrades to `None` rather than aborting the
/// catalogue-push daemon — the push then relies on CloudFront injecting
/// the header (the pre-S339 behaviour), so a flaky keychain never makes
/// the catalogue worse than it was.
pub fn resolve(tenant_id: &str) -> Option<Zeroizing<String>> {
    if let Ok(v) = std::env::var(ORIGIN_SECRET_ENV) {
        if !v.is_empty() {
            return Some(Zeroizing::new(v));
        }
    }
    let service = service_name(tenant_id);
    let entry = match Entry::new(&service, ITEM_ORIGIN_SECRET) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "storefront origin-secret keychain Entry::new failed; \
                 catalogue push will not send X-CloudFront-Secret \
                 (relying on CloudFront injection)"
            );
            return None;
        }
    };
    match entry.get_password() {
        Ok(s) if !s.is_empty() => Some(Zeroizing::new(s)),
        Ok(_) => None,
        Err(keyring::Error::NoEntry) => None,
        Err(other) => {
            tracing::warn!(
                error = %other,
                "storefront origin-secret keychain read failed; \
                 catalogue push will not send X-CloudFront-Secret \
                 (relying on CloudFront injection)"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_name_format_is_stable() {
        assert_eq!(service_name("acme"), "aberp.storefront.acme");
        assert_eq!(service_name("t-uuid-1234"), "aberp.storefront.t-uuid-1234");
    }

    #[test]
    fn item_and_env_names_are_stable() {
        assert_eq!(ITEM_ORIGIN_SECRET, "storefront_origin_secret");
        assert_eq!(ORIGIN_SECRET_ENV, "ABERP_STOREFRONT_ORIGIN_SECRET");
    }

    #[test]
    fn service_name_does_not_collide_with_quote_intake_or_smtp_or_nav() {
        let tenant = "production";
        let storefront = service_name(tenant);
        let qi = crate::quote_intake_credentials::service_name(tenant);
        let smtp = crate::smtp_credentials::service_name(tenant);
        let nav = aberp_nav_transport::credentials::keychain::service_name(tenant);
        assert_ne!(storefront, qi);
        assert_ne!(storefront, smtp);
        assert_ne!(storefront, nav);
        assert!(storefront.starts_with("aberp.storefront."));
    }
}
