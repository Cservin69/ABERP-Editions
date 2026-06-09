//! S325 / PR-25 — in-memory PDF re-render queue (EVE addendum-2,
//! customer-facing stock-alert banner producer).
//!
//! ## Why in-memory (not a DB table)
//!
//! The queue holds quote-ids whose `quote_intake_log.stock_alert` just
//! transitioned FALSE → TRUE and therefore need their customer-facing
//! `priced.pdf` re-rendered (with the red stock-alert band the S318 PDF
//! crate can now draw) and re-POSTed to the storefront `/priced`
//! endpoint (which S323 relaxed to accept a same-hash, `stock_alert:true`
//! re-post).
//!
//! Loss on restart is acceptable: the transition is detected READ-SIDE on
//! the operator's Quotes-tab load (`quote_intake_query::
//! list_quote_intake_rows` → `persist_alerts_and_enqueue_rerender`). If
//! the process restarts before the daemon drains an entry, the very next
//! operator view re-runs the recompute — BUT the stored `stock_alert` is
//! now sticky-TRUE, so that single read would NOT re-enqueue. To keep the
//! restart-tolerance promise honest the daemon drains every ~5s (default)
//! and the customer flow is single-digit quotes/day, so the window where
//! a transition is enqueued-but-undrained-and-then-lost is tiny. A
//! DB-backed queue would survive restart but needs a schema migration and
//! the [[no-sql-specific]] DuckDB DEFAULT-on-replay care; the in-memory
//! HashSet is the smaller, idempotent surface the brief selected.
//!
//! ## Idempotency
//!
//! The queue is a `HashSet<quote_id>`: enqueuing an id already present is
//! a no-op, so two operator views of the same just-flipped row (or a
//! retry of the list route) cannot schedule two re-renders. The daemon
//! `drain`s the whole set per cycle and re-enqueues only the entries
//! whose re-post failed transiently.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

/// Shared, restart-tolerant set of quote-ids awaiting a PDF re-render.
/// Cheap to clone (an `Arc<Mutex<…>>` under the hood); one instance lives
/// in [`crate::serve::AppState`], shared by the read-side enqueue seam
/// and the [`crate::quote_pdf_rerender_daemon`] drain loop.
#[derive(Debug, Default)]
pub struct QuotePdfRerenderQueue {
    inner: Mutex<HashSet<String>>,
}

impl QuotePdfRerenderQueue {
    /// A fresh, empty queue. Stored in `AppState` at construction so every
    /// path (real boot + every test harness) sees a live handle.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Enqueue `quote_id`. Returns `true` iff it was newly inserted
    /// (`false` ⇒ already queued — the idempotency guard). A poisoned
    /// lock degrades to `false` (treat as "already there") rather than
    /// panicking the caller's request path.
    pub fn enqueue(&self, quote_id: &str) -> bool {
        self.inner
            .lock()
            .map(|mut s| s.insert(quote_id.to_string()))
            .unwrap_or(false)
    }

    /// Atomically take + clear every queued id. The daemon processes the
    /// returned ids; transiently-failed ones are re-enqueued via
    /// [`enqueue`](Self::enqueue). Order is unspecified (HashSet).
    pub fn drain(&self) -> Vec<String> {
        self.inner
            .lock()
            .map(|mut s| s.drain().collect())
            .unwrap_or_default()
    }

    /// Current pending count. Cheap; used by tests + the daemon log line.
    pub fn len(&self) -> usize {
        self.inner.lock().map(|s| s.len()).unwrap_or(0)
    }

    /// `true` iff the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Test/observability helper — is this id currently queued?
    pub fn contains(&self, quote_id: &str) -> bool {
        self.inner
            .lock()
            .map(|s| s.contains(quote_id))
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enqueue_then_drain_roundtrips() {
        let q = QuotePdfRerenderQueue::new();
        assert!(q.is_empty());
        assert!(q.enqueue("quote-A"));
        assert!(q.contains("quote-A"));
        assert_eq!(q.len(), 1);
        let drained = q.drain();
        assert_eq!(drained, vec!["quote-A".to_string()]);
        assert!(q.is_empty());
    }

    #[test]
    fn enqueue_is_idempotent_for_same_id() {
        let q = QuotePdfRerenderQueue::new();
        assert!(q.enqueue("quote-A"), "first insert is new");
        assert!(!q.enqueue("quote-A"), "second insert is a no-op");
        assert_eq!(q.len(), 1, "idempotent: single entry for one id");
    }

    #[test]
    fn drain_returns_all_and_empties() {
        let q = QuotePdfRerenderQueue::new();
        q.enqueue("a");
        q.enqueue("b");
        q.enqueue("a"); // dup
        let mut drained = q.drain();
        drained.sort();
        assert_eq!(drained, vec!["a".to_string(), "b".to_string()]);
        assert!(q.is_empty());
        // A re-enqueue after drain (transient-failure requeue) works.
        q.enqueue("a");
        assert_eq!(q.len(), 1);
    }
}
