//! [`NoopAdapter`] — a reference implementation of [`Adapter`] that
//! does nothing.
//!
//! Two purposes per ADR-0060 §"The `NoopAdapter` reference
//! implementation":
//!
//! - The framework's own tests need a non-trivial `dyn Adapter` to
//!   exercise the registry without coupling to a real protocol crate.
//! - Future adapter authors copy it as a starting point: it shows the
//!   minimal shape that satisfies the trait — `name()`, idempotent
//!   `start`/`stop`, sync `health`, broadcast `subscribe()` — without
//!   pulling in any external protocol crate.
//!
//! The state machine is `Stopped → Starting → Healthy → Stopped`, gated
//! by an `AtomicU8`. The broadcast channel exists but is never written
//! to by this adapter (no real events to emit).

use std::sync::atomic::{AtomicU8, Ordering};

use async_trait::async_trait;
use tokio::sync::broadcast;

use crate::adapter::{Adapter, AdapterHealth};
use crate::error::AdapterError;
use crate::events::CanonicalEvent;

const STATE_STOPPED: u8 = 0;
const STATE_STARTING: u8 = 1;
const STATE_HEALTHY: u8 = 2;

/// Default broadcast channel capacity. Real adapters will tune this;
/// `NoopAdapter` never writes events so the value is cosmetic.
const DEFAULT_CHANNEL_CAPACITY: usize = 16;

/// A no-op [`Adapter`] suitable for tests and as a reference shape.
///
/// Thread-safe and clone-cheap (the channel sender is the only owned
/// state). Construct via [`NoopAdapter::new`] or
/// [`NoopAdapter::with_capacity`]; the channel capacity has no behaviour
/// effect for this adapter (no events are emitted) but exists so a
/// future copy-paste author can see where the capacity choice lives.
#[derive(Debug)]
pub struct NoopAdapter {
    name: String,
    state: AtomicU8,
    sender: broadcast::Sender<CanonicalEvent>,
}

impl NoopAdapter {
    /// Construct a stopped adapter with the default channel capacity.
    pub fn new(name: impl Into<String>) -> Self {
        Self::with_capacity(name, DEFAULT_CHANNEL_CAPACITY)
    }

    /// Construct a stopped adapter with a custom channel capacity.
    pub fn with_capacity(name: impl Into<String>, capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self {
            name: name.into(),
            state: AtomicU8::new(STATE_STOPPED),
            sender,
        }
    }

    /// Test-only helper that emits an event into the broadcast as if a
    /// real adapter had produced one. Future copy-paste authors do NOT
    /// expose this method publicly; it exists for the framework's own
    /// integration tests (registry → broadcast → ledger writer end-to-
    /// end). `#[doc(hidden)]` keeps it out of the public docs.
    #[doc(hidden)]
    pub fn emit_for_test(&self, event: CanonicalEvent) -> usize {
        // `send` returns the receiver count; we forward it so tests can
        // assert at least one subscriber received the event.
        self.sender.send(event).unwrap_or(0)
    }
}

#[async_trait]
impl Adapter for NoopAdapter {
    fn name(&self) -> &str {
        &self.name
    }

    async fn start(&self) -> Result<(), AdapterError> {
        // Idempotent: Stopped → Healthy via Starting; Healthy stays.
        // Compare-and-swap from Stopped→Starting first; if that fails
        // because we're already Healthy, that's fine — return Ok.
        match self.state.compare_exchange(
            STATE_STOPPED,
            STATE_STARTING,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) => {
                // Real adapters would spawn background tasks here. We
                // immediately transition Starting → Healthy.
                self.state.store(STATE_HEALTHY, Ordering::SeqCst);
                Ok(())
            }
            Err(current) if current == STATE_HEALTHY || current == STATE_STARTING => Ok(()),
            Err(_) => Ok(()),
        }
    }

    async fn stop(&self) -> Result<(), AdapterError> {
        // Idempotent: Healthy → Stopped; Stopped stays.
        self.state.store(STATE_STOPPED, Ordering::SeqCst);
        Ok(())
    }

    fn health(&self) -> AdapterHealth {
        match self.state.load(Ordering::SeqCst) {
            STATE_STOPPED => AdapterHealth::Stopped,
            STATE_STARTING => AdapterHealth::Starting,
            STATE_HEALTHY => AdapterHealth::Healthy,
            // Unreachable — but per CLAUDE.md rule 12 fail loud, not
            // return a fake healthy.
            other => AdapterHealth::Unhealthy {
                reason: format!("noop adapter in invalid state {other}"),
            },
        }
    }

    fn subscribe(&self) -> broadcast::Receiver<CanonicalEvent> {
        self.sender.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::events::{CanonicalEvent, MachineState};

    #[tokio::test]
    async fn lifecycle_stopped_to_healthy_to_stopped() {
        let adapter = NoopAdapter::new("test-noop");
        assert_eq!(adapter.health(), AdapterHealth::Stopped);
        adapter.start().await.unwrap();
        assert_eq!(adapter.health(), AdapterHealth::Healthy);
        adapter.stop().await.unwrap();
        assert_eq!(adapter.health(), AdapterHealth::Stopped);
    }

    #[tokio::test]
    async fn start_is_idempotent() {
        let adapter = NoopAdapter::new("test-noop");
        adapter.start().await.unwrap();
        adapter.start().await.unwrap();
        adapter.start().await.unwrap();
        assert_eq!(adapter.health(), AdapterHealth::Healthy);
    }

    #[tokio::test]
    async fn stop_is_idempotent() {
        let adapter = NoopAdapter::new("test-noop");
        adapter.stop().await.unwrap();
        adapter.start().await.unwrap();
        adapter.stop().await.unwrap();
        adapter.stop().await.unwrap();
        assert_eq!(adapter.health(), AdapterHealth::Stopped);
    }

    #[tokio::test]
    async fn name_is_stable() {
        let adapter = NoopAdapter::new("dmg-mori-nmh-6300-cell-A");
        assert_eq!(adapter.name(), "dmg-mori-nmh-6300-cell-A");
    }

    #[tokio::test]
    async fn subscribe_returns_fresh_receivers() {
        let adapter = NoopAdapter::new("test-noop");
        let _r1 = adapter.subscribe();
        let _r2 = adapter.subscribe();
        // Both receivers exist independently; no panic.
    }

    #[tokio::test]
    async fn emit_for_test_delivers_to_subscriber() {
        let adapter = NoopAdapter::new("test-noop");
        let mut rx = adapter.subscribe();
        adapter.start().await.unwrap();
        let event = CanonicalEvent::MachineStateChanged {
            machine_id: "test".into(),
            previous_state: MachineState::Idle,
            new_state: MachineState::Running,
            at_iso8601: "2026-06-03T00:00:00Z".into(),
        };
        let delivered = adapter.emit_for_test(event.clone());
        assert!(
            delivered >= 1,
            "expected at least one subscriber to receive the event"
        );
        let received = rx.recv().await.unwrap();
        assert_eq!(received, event);
    }

    /// `NoopAdapter` MUST be `dyn`-castable through `Arc<dyn Adapter>`
    /// — this is the registry's storage shape. Compile-time pin: if
    /// the trait gains a method that breaks dyn-compat (e.g. a
    /// non-dispatchable AFIT), this fails to compile.
    #[test]
    fn noop_adapter_is_dyn_safe() {
        let adapter: std::sync::Arc<dyn Adapter> =
            std::sync::Arc::new(NoopAdapter::new("test-noop"));
        assert_eq!(adapter.name(), "test-noop");
    }
}
