//! The [`Adapter`] trait + adjacent value types per ADR-0060 §"The
//! `Adapter` trait".
//!
//! An adapter is a Rust object that speaks one vendor's protocol and
//! emits canonical ABERP events. Adapters live in their own per-vendor
//! crates (e.g. future `aberp-adapter-mtconnect`). This crate ships only
//! the trait + the [`NoopAdapter`](crate::NoopAdapter) reference impl.
//!
//! ## Lifecycle
//!
//! ```text
//! Stopped --start()--> Starting --(internal)--> Healthy
//!    ^                                              |
//!    |                                            stop()
//!    +-- (internal join) <-- Stopped <--(internal)--+
//! ```
//!
//! `start` and `stop` are idempotent — calling either while already in
//! the target state is Ok and a no-op. Adapters that fail to start
//! return `AdapterError::StartFailed` and remain in `Stopped`.

use async_trait::async_trait;
use tokio::sync::broadcast;

use crate::error::AdapterError;
use crate::events::CanonicalEvent;

/// The canonical adapter contract.
///
/// All methods are `&self` — adapter implementations carry interior
/// mutability for their JoinHandles / cancellation tokens / state
/// trackers. This matches the `Arc<dyn Adapter>` ownership shape the
/// [`AdapterRegistry`](crate::AdapterRegistry) uses.
#[async_trait]
pub trait Adapter: Send + Sync + std::fmt::Debug {
    /// Stable identifier — typically `vendor-model-instance`. Used as
    /// the registry key; MUST be unique across registered adapters.
    fn name(&self) -> &str;

    /// Boot background tasks. Returns once the adapter is up; does NOT
    /// block until cancellation. Idempotent.
    async fn start(&self) -> Result<(), AdapterError>;

    /// Signal background tasks to halt and await their completion.
    /// Idempotent.
    async fn stop(&self) -> Result<(), AdapterError>;

    /// Current health snapshot. Sync — adapter tracks state internally
    /// and answers from cached state.
    fn health(&self) -> AdapterHealth;

    /// Subscribe to the adapter's event broadcast. Each call returns a
    /// fresh receiver — multiple consumers (ledger writer, future SPA
    /// push, future operations-dashboard projection) can subscribe
    /// independently.
    fn subscribe(&self) -> broadcast::Receiver<CanonicalEvent>;
}

/// Adapter health snapshot. Closed vocab — every reachable state has a
/// named variant. The `String` reason fields are operator-readable;
/// MUST NOT carry credential bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterHealth {
    /// Not yet started, or stopped after a clean `stop()`.
    Stopped,
    /// `start()` was called and background tasks are spinning up.
    Starting,
    /// Background tasks are running and the upstream system responded
    /// to the last health probe (if any) within the adapter's
    /// freshness budget.
    Healthy,
    /// Background tasks are running but a non-fatal condition exists
    /// (slow upstream, recent event-stream lag). Reason is
    /// operator-readable.
    Degraded { reason: String },
    /// Background tasks halted or unable to reach the upstream system.
    /// Reason is operator-readable.
    Unhealthy { reason: String },
}

impl AdapterHealth {
    /// True when the adapter is in a state that can serve subscribers
    /// — `Healthy` or `Degraded`. Used by the registry's health summary
    /// to count "available" adapters distinct from totally-down ones.
    pub fn is_serving(&self) -> bool {
        matches!(
            self,
            AdapterHealth::Healthy | AdapterHealth::Degraded { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_serving_table() {
        assert!(!AdapterHealth::Stopped.is_serving());
        assert!(!AdapterHealth::Starting.is_serving());
        assert!(AdapterHealth::Healthy.is_serving());
        assert!(AdapterHealth::Degraded {
            reason: "slow".to_string(),
        }
        .is_serving());
        assert!(!AdapterHealth::Unhealthy {
            reason: "down".to_string(),
        }
        .is_serving());
    }
}
