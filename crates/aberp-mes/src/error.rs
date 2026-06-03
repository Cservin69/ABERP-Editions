//! Error machinery — closed-vocab adapter + registry errors.
//!
//! Two enums:
//!
//! - [`AdapterError`] — what an adapter implementation can return from
//!   `start` / `stop`. Phase α leaves the leaf variants intentionally
//!   thin; a future adapter that needs richer error classes either
//!   extends this enum or wraps with `#[source]`.
//! - [`RegistryError`] — what the registry returns from `register` /
//!   `unregister`. Only one variant today (duplicate name); the closed
//!   vocab grows as the registry surface grows.

use thiserror::Error;

/// Error returned by an [`Adapter`](crate::Adapter)'s lifecycle methods.
///
/// Closed vocabulary today; extending requires adding the variant and an
/// associated downstream-handler case. No `Other(String)` catch-all —
/// silent-fallback is the failure mode CLAUDE.md rule 12 names.
#[derive(Debug, Error)]
pub enum AdapterError {
    /// Adapter could not start its background tasks. The `String` is
    /// operator-readable; no credential bytes inside.
    #[error("adapter start failed: {0}")]
    StartFailed(String),

    /// Adapter could not stop cleanly within its internal deadline.
    /// Operator-readable; safe to log.
    #[error("adapter stop failed: {0}")]
    StopFailed(String),

    /// Adapter detected an internal invariant violation. Used by future
    /// adapter implementations that want to fail-loud during setup.
    #[error("adapter invariant violated: {0}")]
    InvariantViolated(String),
}

/// Error returned by the [`AdapterRegistry`](crate::AdapterRegistry).
///
/// Closed vocabulary; the only failure mode today is a duplicate-name
/// registration. Future failure modes (e.g. registry frozen, capacity
/// exceeded) add variants here.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum RegistryError {
    /// An adapter with the same [`Adapter::name`](crate::Adapter::name)
    /// is already registered. The registry NEVER silently overwrites.
    #[error("adapter '{0}' is already registered")]
    DuplicateName(String),
}
