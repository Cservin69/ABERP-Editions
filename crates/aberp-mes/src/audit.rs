//! Audit-ledger integration per ADR-0060 §"Audit-ledger integration".
//!
//! Every emitted [`CanonicalEvent`] becomes one audit-ledger entry of
//! kind [`EventKind::MesAdapterEvent`]. The payload carries the
//! adapter's `name`, an operator-decision idempotency key, and the
//! typed event itself.
//!
//! One kind for all MES events — the canonical-event vocabulary
//! evolves in `aberp-mes` (Rust enum extension) without touching the
//! audit-ledger crate. Future Stage 3 PRs adding new audit sub-surfaces
//! (e.g. an adapter-registered event distinct from per-event-recording)
//! add `EventKind` variants but keep the `mes.` prefix.

use duckdb::Transaction;
use serde::{Deserialize, Serialize};

use aberp_audit_ledger::{append_in_tx, Actor, AppendError, EventKind, LedgerMeta};

use crate::events::CanonicalEvent;

/// Audit-ledger payload for [`EventKind::MesAdapterEvent`].
///
/// `adapter_name` records WHO emitted the event (matching the
/// adapter's [`Adapter::name`](crate::Adapter::name)). `idempotency_key`
/// is the F8 pattern — same `ulid` shape used by every other
/// system-prefixed kind.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MesAdapterEventPayload {
    pub adapter_name: String,
    pub idempotency_key: String,
    pub event: CanonicalEvent,
}

impl MesAdapterEventPayload {
    /// Construct a payload tying an event to its source adapter.
    pub fn new(
        adapter_name: impl Into<String>,
        idempotency_key: impl Into<String>,
        event: CanonicalEvent,
    ) -> Self {
        Self {
            adapter_name: adapter_name.into(),
            idempotency_key: idempotency_key.into(),
            event,
        }
    }

    /// Encode to the audit-ledger's byte form (JSON). Mirrors
    /// `aberp-quote-intake::QuoteIntakePollPayload::to_bytes` for
    /// consistency across system-prefixed audit producers.
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("JSON serialization of MesAdapterEventPayload cannot fail")
    }
}

/// Render the on-disk EventKind string. Useful for SQL `WHERE kind = …`
/// queries against the audit-ledger.
pub fn audit_kind_string() -> &'static str {
    EventKind::MesAdapterEvent.as_str()
}

/// Append one MES-adapter-event entry into the audit ledger inside an
/// already-open DuckDB transaction. Same write surface
/// `aberp-quote-intake` uses for `QuoteIntakePollCompleted`.
///
/// The caller MUST already be inside the same DuckDB transaction the
/// upstream state change rode in (per ADR-0008 §"Storage" — ledger
/// entries write in the same transaction as the state change they
/// describe). For Phase α the upstream "state change" is the broadcast
/// emission; β will pin the runtime task that subscribes to the
/// broadcast and calls this helper.
pub fn write_mes_adapter_event(
    tx: &Transaction<'_>,
    meta: &LedgerMeta,
    actor: Actor,
    payload: &MesAdapterEventPayload,
) -> Result<(), AppendError> {
    append_in_tx(
        tx,
        meta,
        EventKind::MesAdapterEvent,
        payload.to_bytes(),
        actor,
        Some(payload.idempotency_key.clone()),
    )
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::events::{CanonicalEvent, MachineState};

    fn sample_payload() -> MesAdapterEventPayload {
        MesAdapterEventPayload::new(
            "dmg-mori-nmh-6300-cell-A",
            "01H8MES1234567890ABCDEFGHJK",
            CanonicalEvent::MachineStateChanged {
                machine_id: "dmg-mori-nmh-6300-cell-A".into(),
                previous_state: MachineState::Idle,
                new_state: MachineState::Running,
                at_iso8601: "2026-06-03T08:30:00Z".into(),
            },
        )
    }

    #[test]
    fn audit_kind_string_matches_event_kind() {
        assert_eq!(audit_kind_string(), EventKind::MesAdapterEvent.as_str());
    }

    #[test]
    fn audit_kind_uses_mes_prefix() {
        // Pinned at the producer-crate level (audit-ledger has its own
        // pin); this guards against a future refactor that wired
        // `aberp-mes` to a different EventKind variant.
        assert!(audit_kind_string().starts_with("mes."));
        assert!(!audit_kind_string().starts_with("invoice."));
        assert!(!audit_kind_string().starts_with("system."));
    }

    #[test]
    fn payload_round_trips_through_bytes() {
        let p = sample_payload();
        let bytes = p.to_bytes();
        let back: MesAdapterEventPayload = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn payload_carries_event_discriminator() {
        let p = sample_payload();
        let json: serde_json::Value = serde_json::from_slice(&p.to_bytes()).unwrap();
        assert_eq!(
            json["adapter_name"].as_str(),
            Some("dmg-mori-nmh-6300-cell-A")
        );
        assert_eq!(
            json["event"]["type"].as_str(),
            Some("machine_state_changed"),
            "event payload must keep CanonicalEvent's `type` discriminator visible for SQL JSON-extract queries"
        );
    }

    /// Integration test — exercise the full ledger-append path against
    /// a fresh in-memory DuckDB DB. Mirrors the pattern
    /// `aberp-quote-intake` uses (`service.rs::run_one_cycle` builds
    /// `LedgerMeta::new(tenant, binary_hash)` and threads it into the
    /// audit writer).
    ///
    /// This pins the contract that `write_mes_adapter_event` is
    /// callable through `aberp-audit-ledger`'s existing surface AND
    /// that the resulting ledger row carries the expected kind +
    /// payload bytes.
    #[test]
    fn ledger_append_end_to_end() {
        use aberp_audit_ledger::{ensure_schema, BinaryHash, LedgerMeta, TenantId};

        let mut conn = duckdb::Connection::open_in_memory().unwrap();
        ensure_schema(&conn).unwrap();
        let tenant = TenantId::new("ten_test_mes").expect("tenant id");
        let binary_hash = BinaryHash::from_bytes([0u8; 32]);
        let meta = LedgerMeta::new(tenant, binary_hash);
        // `Actor::test_only` is `#[cfg(test)]`-gated inside the
        // audit-ledger crate; from an external test we use the
        // production constructor with sentinel inputs (same pattern
        // `aberp-quote-intake::service` uses).
        let actor = Actor::from_local_cli("test-session".to_string(), "test-user");

        let payload = sample_payload();

        let tx = conn.transaction().unwrap();
        write_mes_adapter_event(&tx, &meta, actor, &payload).unwrap();
        tx.commit().unwrap();

        // Query back: one row, kind = mes.adapter_event, payload bytes
        // round-trip to the same struct.
        let mut stmt = conn
            .prepare("SELECT kind, payload FROM audit_ledger ORDER BY seq ASC")
            .unwrap();
        let rows: Vec<(String, Vec<u8>)> = stmt
            .query_map([], |row| {
                let kind: String = row.get(0)?;
                let payload: Vec<u8> = row.get(1)?;
                Ok((kind, payload))
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "mes.adapter_event");
        let back: MesAdapterEventPayload = serde_json::from_slice(&rows[0].1).unwrap();
        assert_eq!(back, payload);
    }
}
