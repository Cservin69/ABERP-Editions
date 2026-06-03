//! [`AdapterRegistry`] — runtime registry of registered adapters per
//! ADR-0060 §"The `AdapterRegistry`".
//!
//! Holds `HashMap<String, Arc<dyn Adapter>>`. NOT persisted —
//! per `[[no-sql-specific]]` extended by the Stage 3 memo, adapter
//! membership belongs in code (boot config + dynamic registration), not
//! in a DuckDB table. A future `aberp-mes-config` crate may load
//! adapter definitions from operator configuration (TOML), but that
//! lives outside this PR.

use std::collections::HashMap;
use std::sync::Arc;

use crate::adapter::{Adapter, AdapterHealth};
use crate::error::{AdapterError, RegistryError};

/// Runtime registry of adapters. NOT thread-safe by itself — wrap in
/// `Arc<Mutex<AdapterRegistry>>` or `Arc<RwLock<AdapterRegistry>>` at
/// the call site if shared. (The bound adapters themselves are
/// `Send + Sync`; only the HashMap-of-Arcs needs external sync.)
#[derive(Debug, Default)]
pub struct AdapterRegistry {
    adapters: HashMap<String, Arc<dyn Adapter>>,
}

impl AdapterRegistry {
    /// Construct an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an adapter. Returns
    /// [`RegistryError::DuplicateName`] if an adapter with the same
    /// `name()` is already registered — the registry NEVER silently
    /// overwrites.
    pub fn register(&mut self, adapter: Arc<dyn Adapter>) -> Result<(), RegistryError> {
        let name = adapter.name().to_string();
        if self.adapters.contains_key(&name) {
            return Err(RegistryError::DuplicateName(name));
        }
        self.adapters.insert(name, adapter);
        Ok(())
    }

    /// Remove an adapter from the registry. Returns the adapter so the
    /// caller can `stop()` it. `None` if no adapter under that name.
    pub fn unregister(&mut self, name: &str) -> Option<Arc<dyn Adapter>> {
        self.adapters.remove(name)
    }

    /// Look up an adapter by name. Clone-cheap — returns an `Arc`.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Adapter>> {
        self.adapters.get(name).cloned()
    }

    /// Snapshot of registered adapter names. Returns an owned `Vec<String>`
    /// to keep the borrow short (typical caller iterates over the
    /// snapshot while issuing further registry calls).
    pub fn names(&self) -> Vec<String> {
        let mut out: Vec<String> = self.adapters.keys().cloned().collect();
        out.sort();
        out
    }

    /// Number of registered adapters.
    pub fn len(&self) -> usize {
        self.adapters.len()
    }

    /// True if no adapters are registered.
    pub fn is_empty(&self) -> bool {
        self.adapters.is_empty()
    }

    /// Snapshot of every adapter's current health.
    pub fn health(&self) -> HashMap<String, AdapterHealth> {
        self.adapters
            .iter()
            .map(|(name, adapter)| (name.clone(), adapter.health()))
            .collect()
    }

    /// Start every registered adapter in sequence. Returns a vector of
    /// `(name, result)` pairs in registration-name-sorted order. A
    /// failure on one adapter does NOT short-circuit the rest — every
    /// adapter is given a start attempt.
    pub async fn start_all(&self) -> Vec<(String, Result<(), AdapterError>)> {
        let mut out = Vec::with_capacity(self.adapters.len());
        for name in self.names() {
            let adapter = self.adapters.get(&name).expect("present per names()");
            out.push((name.clone(), adapter.start().await));
        }
        out
    }

    /// Stop every registered adapter in sequence. Same all-or-each
    /// semantics as `start_all`.
    pub async fn stop_all(&self) -> Vec<(String, Result<(), AdapterError>)> {
        let mut out = Vec::with_capacity(self.adapters.len());
        for name in self.names() {
            let adapter = self.adapters.get(&name).expect("present per names()");
            out.push((name.clone(), adapter.stop().await));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::noop::NoopAdapter;

    #[test]
    fn new_registry_is_empty() {
        let r = AdapterRegistry::new();
        assert!(r.is_empty());
        assert_eq!(r.len(), 0);
        assert!(r.names().is_empty());
    }

    #[test]
    fn register_then_lookup() {
        let mut r = AdapterRegistry::new();
        let adapter: Arc<dyn Adapter> = Arc::new(NoopAdapter::new("a"));
        r.register(adapter).unwrap();
        assert_eq!(r.len(), 1);
        assert!(r.get("a").is_some());
        assert!(r.get("b").is_none());
    }

    #[test]
    fn register_rejects_duplicate_name() {
        let mut r = AdapterRegistry::new();
        r.register(Arc::new(NoopAdapter::new("a"))).unwrap();
        let err = r.register(Arc::new(NoopAdapter::new("a"))).unwrap_err();
        assert_eq!(err, RegistryError::DuplicateName("a".to_string()));
        // The original survives — no silent overwrite.
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn unregister_removes_and_returns_adapter() {
        let mut r = AdapterRegistry::new();
        r.register(Arc::new(NoopAdapter::new("a"))).unwrap();
        let removed = r.unregister("a").unwrap();
        assert_eq!(removed.name(), "a");
        assert!(r.is_empty());
    }

    #[test]
    fn unregister_unknown_returns_none() {
        let mut r = AdapterRegistry::new();
        assert!(r.unregister("ghost").is_none());
    }

    #[test]
    fn names_returns_sorted_snapshot() {
        let mut r = AdapterRegistry::new();
        r.register(Arc::new(NoopAdapter::new("zebra"))).unwrap();
        r.register(Arc::new(NoopAdapter::new("alpha"))).unwrap();
        r.register(Arc::new(NoopAdapter::new("middle"))).unwrap();
        assert_eq!(r.names(), vec!["alpha", "middle", "zebra"]);
    }

    #[tokio::test]
    async fn health_snapshot_reflects_all_adapters() {
        let mut r = AdapterRegistry::new();
        r.register(Arc::new(NoopAdapter::new("a"))).unwrap();
        r.register(Arc::new(NoopAdapter::new("b"))).unwrap();
        let h = r.health();
        assert_eq!(h.len(), 2);
        assert_eq!(h.get("a"), Some(&AdapterHealth::Stopped));
        assert_eq!(h.get("b"), Some(&AdapterHealth::Stopped));
    }

    #[tokio::test]
    async fn start_all_then_stop_all_flips_health() {
        let mut r = AdapterRegistry::new();
        r.register(Arc::new(NoopAdapter::new("a"))).unwrap();
        r.register(Arc::new(NoopAdapter::new("b"))).unwrap();
        let started = r.start_all().await;
        assert_eq!(started.len(), 2);
        for (_, result) in &started {
            assert!(result.is_ok());
        }
        let h = r.health();
        assert_eq!(h.get("a"), Some(&AdapterHealth::Healthy));
        assert_eq!(h.get("b"), Some(&AdapterHealth::Healthy));

        let stopped = r.stop_all().await;
        assert_eq!(stopped.len(), 2);
        let h = r.health();
        assert_eq!(h.get("a"), Some(&AdapterHealth::Stopped));
        assert_eq!(h.get("b"), Some(&AdapterHealth::Stopped));
    }

    /// The registry's start/stop iterate in sorted order. Pin so a
    /// future HashMap iteration switch doesn't silently change the
    /// adapter-startup order (which affects the audit ledger's
    /// emission ordering when adapters fire boot events).
    #[tokio::test]
    async fn start_all_visits_adapters_in_sorted_order() {
        let mut r = AdapterRegistry::new();
        r.register(Arc::new(NoopAdapter::new("zebra"))).unwrap();
        r.register(Arc::new(NoopAdapter::new("alpha"))).unwrap();
        let result = r.start_all().await;
        let names: Vec<&str> = result.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["alpha", "zebra"]);
    }
}
