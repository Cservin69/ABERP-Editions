//! The [`DigitalIdProvider`] swap-point trait + its error type.

use crate::{DigitalId, Signature};

/// Failure modes a [`DigitalIdProvider`] can surface.
///
/// Typed (not stringly) so the boot/audit layer can branch — e.g. a missing
/// current operator is a different posture from a signing backend that is
/// configured but unreachable.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    /// No operator is currently authenticated with this provider.
    #[error("no operator is currently authenticated")]
    NoCurrentOperator,
    /// The signing backend is configured but could not produce a signature
    /// (hardware token absent, certificate expired, vendor service down, …).
    #[error("signing backend unavailable: {0}")]
    SigningUnavailable(String),
}

/// The abstraction every audit-emitting operation will eventually consult
/// for the operator's digital identity.
///
/// `Send + Sync` so a single `Arc<dyn DigitalIdProvider>` can be cloned into
/// `AppState` and shared across every axum handler + background daemon, the
/// same way [`StorefrontCredentialHandle`] and friends are shared today.
///
/// [`StorefrontCredentialHandle`]: apps/aberp's storefront credential SPOC.
pub trait DigitalIdProvider: Send + Sync {
    /// Short backend tag, e.g. `"mock"`, `"hu-eid"`, `"us-dod-cac"`. Used in
    /// the boot log line and as a fast discriminator in tests.
    fn name(&self) -> &str;

    /// The currently-authenticated operator's identity.
    fn current_operator(&self) -> Result<DigitalId, ProviderError>;

    /// Sign arbitrary payload bytes, producing an algorithm-tagged
    /// [`Signature`] linked back to the current operator.
    fn sign(&self, payload: &[u8]) -> Result<Signature, ProviderError>;

    /// Verify that `sig` is a valid signature over `payload` for this
    /// provider's algorithm. Returns `Ok(false)` for a well-formed but
    /// non-matching signature (tampered payload, tampered bytes, wrong
    /// algorithm); `Err` only when verification itself could not run.
    fn verify(&self, payload: &[u8], sig: &Signature) -> Result<bool, ProviderError>;
}
