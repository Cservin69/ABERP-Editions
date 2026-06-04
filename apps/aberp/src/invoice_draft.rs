//! S236 / PR-230b — pre-allocation invoice-draft staging.
//!
//! New `invoice_draft` table that holds invoices in a "staged" state
//! BEFORE the gap-free sequence allocator (ADR-0009 §3 / §169) runs.
//! Solves the §169 vs. Stage 3 hand-off problem PR-230 deliberately
//! left open: a dispatch can now materialise a visible draft on
//! Mark-Shipped without burning a sequence slot — the operator's
//! Issue click in the SPA is the sole allocator gate.
//!
//! # Why a separate table (not nullable `invoice.sequence_number`)
//!
//! Mirrors the [[aberp-ap-module-backend-s177]] / [[nav-as-dr-restore-s180]]
//! / [[quote-intake-crate-s210]] precedent — stage-not-burn in a
//! sibling table, never touching the regulated `invoice` table. Three
//! reasons over the alternative of widening `invoice.sequence_number`
//! to NULL:
//!
//! 1. The `invoice` table's `NOT NULL` + `UNIQUE (series_id,
//!    fiscal_year, sequence_number)` invariants are load-bearing for
//!    ADR-0009 §169 (gap-free). NULL widening would force every
//!    allocator-walk and audit-replay query to filter on
//!    `sequence_number IS NOT NULL` — a maintenance trap.
//! 2. DuckDB's `ALTER COLUMN` is blocked by foreign / CHECK references
//!    per [[duckdb-alter-column-type-check-constraint]]. A schema
//!    migration through `add/backfill/drop/rename` on a regulated
//!    table mid-PR would be risky against existing prod databases.
//! 3. The "row may be edited or deleted freely" affordance only
//!    applies before allocation. Keeping it in a sibling table means
//!    the regulated `invoice` table stays append-only-after-allocation
//!    by construction.
//!
//! # Row lifecycle
//!
//! ```text
//! create_draft_in_tx       (no seq allocated, emits InvoiceStaged)
//!         │
//!         ▼
//! ┌──────────────────┐
//! │ invoice_draft    │── delete_draft_in_tx ──▶ row gone, no seq burn
//! └──────────────────┘
//!         │
//!         │  promote_draft_to_invoice
//!         ▼
//! issue_invoice path runs (allocates seq, inserts `invoice`,
//! emits InvoiceSequenceReserved + InvoiceDraftCreated)
//! ```
//!
//! # Audit chain
//!
//! - `InvoiceStaged` (`invoice.staged`) fires once at draft creation.
//!   Carries `draft_id`, `partner_id`, `source_dispatch_id`, the
//!   actor, and the F8 idempotency key. The prefix is `invoice.*` per
//!   the rationale in [`aberp_audit_ledger::EventKind::InvoiceStaged`]
//!   docs.
//! - On promotion the existing `InvoiceSequenceReserved` +
//!   `InvoiceDraftCreated` pair fires from `issue_invoice::run`. The
//!   chain back to the draft rides the idempotency-key suffix
//!   convention.
//! - On operator delete `InvoiceDraftDeleted` does NOT fire in v1 —
//!   the audit-walk is happy without it; the existence of
//!   `InvoiceStaged` without a matching downstream is the "deleted"
//!   signal. If a future audit need surfaces, that's an additive
//!   F12 ritual.

use anyhow::{anyhow, Context, Result};
use duckdb::{params, Connection, Transaction};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use ulid::Ulid;

use aberp_audit_ledger::{append_in_tx, Actor, EventKind, LedgerMeta};
use aberp_dispatch::{Dispatch, InvoiceSpawner};

// ──────────────────────────────────────────────────────────────────────
// Schema.
// ──────────────────────────────────────────────────────────────────────

const INVOICE_DRAFT_SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS invoice_draft (
    drf_id              VARCHAR NOT NULL PRIMARY KEY,
    tenant_id           VARCHAR NOT NULL,
    partner_id          VARCHAR NOT NULL,
    source_dispatch_id  VARCHAR,
    source_wo_id        VARCHAR,
    product_id          VARCHAR NOT NULL,
    qty                 VARCHAR NOT NULL,
    notes               VARCHAR,
    created_at          VARCHAR NOT NULL,
    UNIQUE (tenant_id, source_dispatch_id)
);
CREATE INDEX IF NOT EXISTS invoice_draft_tenant_partner_idx
    ON invoice_draft (tenant_id, partner_id);
CREATE INDEX IF NOT EXISTS invoice_draft_tenant_created_idx
    ON invoice_draft (tenant_id, created_at);
";

/// Idempotent `CREATE TABLE IF NOT EXISTS` for the `invoice_draft`
/// table. Same boot-time posture as `incoming_invoices::ensure_schema`
/// / `restore_from_nav_outgoing::ensure_schema`.
pub fn ensure_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(INVOICE_DRAFT_SCHEMA_SQL)
        .context("ensure invoice_draft schema")
}

// ──────────────────────────────────────────────────────────────────────
// Row shape.
// ──────────────────────────────────────────────────────────────────────

/// One row from `invoice_draft`. Surfaced in the SPA invoice-list as
/// `InvoiceState::Draft` per the third row source (alongside `invoice`
/// → Ready+ states and `restored_invoice` → ExtNav rows).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InvoiceDraft {
    pub drf_id: String,
    pub tenant_id: String,
    pub partner_id: String,
    pub source_dispatch_id: Option<String>,
    pub source_wo_id: Option<String>,
    pub product_id: String,
    /// Decimal-as-string for the wire form; matches the
    /// `aberp_inventory` / `aberp_work_orders` convention.
    pub qty: String,
    pub notes: Option<String>,
    pub created_at: String,
}

// ──────────────────────────────────────────────────────────────────────
// Audit payload.
// ──────────────────────────────────────────────────────────────────────

/// `invoice.staged` payload. Carries the draft id + bridge fields so
/// a future audit-walk can reconstruct "this draft was spawned by
/// dispatch dsp_X against partner ptr_Y on behalf of WO wo_Z".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InvoiceStagedPayload {
    pub draft_id: String,
    pub tenant_id: String,
    pub partner_id: String,
    pub source_dispatch_id: Option<String>,
    pub source_wo_id: Option<String>,
    pub product_id: String,
    /// Decimal-as-string.
    pub qty: String,
    pub actor: String,
    pub idempotency_key: String,
}

impl InvoiceStagedPayload {
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("JSON serialization of InvoiceStagedPayload cannot fail")
    }
}

// ──────────────────────────────────────────────────────────────────────
// Create / read / delete.
// ──────────────────────────────────────────────────────────────────────

/// Inputs to [`create_draft_in_tx`]. `actor` and `idempotency_key` are
/// the audit attribution + F8 dedup pair; `partner_id` is the
/// recipient resolved by the caller (the dispatch's
/// `partner_id` in the spawner path).
#[derive(Debug, Clone)]
pub struct CreateDraftInputs {
    pub tenant: String,
    pub partner_id: String,
    pub source_dispatch_id: Option<String>,
    pub source_wo_id: Option<String>,
    pub product_id: String,
    pub qty: Decimal,
    pub notes: Option<String>,
    pub actor: String,
    pub idempotency_key: String,
}

/// Insert one `invoice_draft` row + emit one `InvoiceStaged` audit
/// entry, all in the supplied transaction. Defence-in-depth:
///   - `idempotency_key` non-empty (the ledger pin is the actual gate;
///     this is the early-validate)
///   - `partner_id` non-empty
///   - `product_id` non-empty
pub fn create_draft_in_tx(
    tx: &Transaction<'_>,
    ledger_meta: &LedgerMeta,
    ledger_actor: Actor,
    inputs: CreateDraftInputs,
) -> Result<InvoiceDraft> {
    if inputs.idempotency_key.trim().is_empty() {
        return Err(anyhow!("idempotency_key must be non-empty"));
    }
    if inputs.partner_id.trim().is_empty() {
        return Err(anyhow!("partner_id must be non-empty"));
    }
    if inputs.product_id.trim().is_empty() {
        return Err(anyhow!("product_id must be non-empty"));
    }

    let drf_id = format!("drf_{}", Ulid::new());
    let now = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .context("format invoice_draft.created_at as RFC3339")?;
    let qty_str = inputs.qty.to_string();

    tx.execute(
        "INSERT INTO invoice_draft (
            drf_id, tenant_id, partner_id,
            source_dispatch_id, source_wo_id,
            product_id, qty, notes, created_at
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?);",
        params![
            &drf_id,
            &inputs.tenant,
            &inputs.partner_id,
            inputs.source_dispatch_id.as_deref(),
            inputs.source_wo_id.as_deref(),
            &inputs.product_id,
            &qty_str,
            inputs.notes.as_deref(),
            &now,
        ],
    )
    .context("INSERT invoice_draft")?;

    let payload = InvoiceStagedPayload {
        draft_id: drf_id.clone(),
        tenant_id: inputs.tenant.clone(),
        partner_id: inputs.partner_id.clone(),
        source_dispatch_id: inputs.source_dispatch_id.clone(),
        source_wo_id: inputs.source_wo_id.clone(),
        product_id: inputs.product_id.clone(),
        qty: qty_str.clone(),
        actor: inputs.actor.clone(),
        idempotency_key: inputs.idempotency_key.clone(),
    };
    append_in_tx(
        tx,
        ledger_meta,
        EventKind::InvoiceStaged,
        payload.to_bytes(),
        ledger_actor,
        Some(inputs.idempotency_key.clone()),
    )
    .context("audit append InvoiceStaged")?;

    Ok(InvoiceDraft {
        drf_id,
        tenant_id: inputs.tenant,
        partner_id: inputs.partner_id,
        source_dispatch_id: inputs.source_dispatch_id,
        source_wo_id: inputs.source_wo_id,
        product_id: inputs.product_id,
        qty: qty_str,
        notes: inputs.notes,
        created_at: now,
    })
}

/// Read one draft by id within the tenant; `None` if absent.
pub fn read_draft(conn: &Connection, tenant: &str, drf_id: &str) -> Result<Option<InvoiceDraft>> {
    let mut stmt = conn
        .prepare(
            "SELECT drf_id, tenant_id, partner_id, source_dispatch_id, source_wo_id,
                    product_id, qty, notes, created_at
             FROM invoice_draft
             WHERE tenant_id = ? AND drf_id = ?
             LIMIT 1;",
        )
        .context("prepare read_draft")?;
    let mut rows = stmt
        .query_map(params![tenant, drf_id], |row| {
            Ok(InvoiceDraft {
                drf_id: row.get(0)?,
                tenant_id: row.get(1)?,
                partner_id: row.get(2)?,
                source_dispatch_id: row.get(3)?,
                source_wo_id: row.get(4)?,
                product_id: row.get(5)?,
                qty: row.get(6)?,
                notes: row.get(7)?,
                created_at: row.get(8)?,
            })
        })
        .context("query read_draft")?;
    match rows.next() {
        Some(r) => Ok(Some(r.context("decode invoice_draft row")?)),
        None => Ok(None),
    }
}

/// List all drafts in a tenant; ordered by `created_at DESC`.
pub fn list_drafts(conn: &Connection, tenant: &str) -> Result<Vec<InvoiceDraft>> {
    let mut stmt = conn
        .prepare(
            "SELECT drf_id, tenant_id, partner_id, source_dispatch_id, source_wo_id,
                    product_id, qty, notes, created_at
             FROM invoice_draft
             WHERE tenant_id = ?
             ORDER BY created_at DESC;",
        )
        .context("prepare list_drafts")?;
    let rows = stmt
        .query_map(params![tenant], |row| {
            Ok(InvoiceDraft {
                drf_id: row.get(0)?,
                tenant_id: row.get(1)?,
                partner_id: row.get(2)?,
                source_dispatch_id: row.get(3)?,
                source_wo_id: row.get(4)?,
                product_id: row.get(5)?,
                qty: row.get(6)?,
                notes: row.get(7)?,
                created_at: row.get(8)?,
            })
        })
        .context("query list_drafts")?;
    rows.collect::<duckdb::Result<Vec<_>>>()
        .context("collect list_drafts")
}

/// Delete one draft by id within the tenant. Returns `true` iff a row
/// was actually deleted. No audit row is written in v1 (see module docs).
pub fn delete_draft_in_tx(tx: &Transaction<'_>, tenant: &str, drf_id: &str) -> Result<bool> {
    let n = tx
        .execute(
            "DELETE FROM invoice_draft WHERE tenant_id = ? AND drf_id = ?;",
            params![tenant, drf_id],
        )
        .context("DELETE invoice_draft")?;
    Ok(n > 0)
}

// ──────────────────────────────────────────────────────────────────────
// Dispatch-spawner adapter.
// ──────────────────────────────────────────────────────────────────────

/// Real `InvoiceSpawner` that PR-230b lands. Replaces
/// `aberp_dispatch::NoopInvoiceSpawner` in the serve boot path.
///
/// On every `dispatches → Shipped` flip, this inserts an
/// `invoice_draft` row + emits one `InvoiceStaged` audit entry in the
/// dispatch's own transaction per ADR-0064 §"Invariants pinned" #6.
/// Returns the `drf_<ULID>` id so the dispatch row's
/// `spawned_invoice_id` records the bridge.
///
/// # Why the spawner stores a `drf_*` id in `spawned_invoice_id`
///
/// PR-230 wired `Dispatch.spawned_invoice_id: Option<String>` as
/// "whatever the spawner returned." The brief named "spawned invoice
/// id" but the value is in fact "spawned-invoice-or-draft id" — the
/// dispatch tracks the bridge target regardless of allocation state.
/// The SPA's dispatch-detail click-through resolves which table to
/// open by prefix: `drf_*` → draft detail; `inv_*` → invoice detail
/// (post-promotion path, when the operator's Issue click flipped the
/// draft into a real invoice and the dispatch row was UPDATEd with
/// the new `inv_*` id — that re-link is the SPA's responsibility, not
/// the spawner's).
pub struct BillingInvoiceSpawner<'a> {
    pub tenant: String,
    pub actor: String,
    pub ledger_meta: &'a LedgerMeta,
    pub ledger_actor: Actor,
}

impl<'a> InvoiceSpawner for BillingInvoiceSpawner<'a> {
    fn spawn(
        &self,
        tx: &Transaction<'_>,
        dispatch: &Dispatch,
        wo_product_id: &str,
        wo_qty_target: Decimal,
        idempotency_key: &str,
    ) -> anyhow::Result<Option<String>> {
        let draft = create_draft_in_tx(
            tx,
            self.ledger_meta,
            self.ledger_actor.clone(),
            CreateDraftInputs {
                tenant: self.tenant.clone(),
                partner_id: dispatch.partner_id.clone(),
                source_dispatch_id: Some(dispatch.dsp_id.clone()),
                source_wo_id: Some(dispatch.wo_id.clone()),
                product_id: wo_product_id.to_string(),
                qty: wo_qty_target,
                notes: dispatch.notes.clone(),
                actor: self.actor.clone(),
                idempotency_key: idempotency_key.to_string(),
            },
        )?;
        Ok(Some(draft.drf_id))
    }
}

// ──────────────────────────────────────────────────────────────────────
// Unit tests.
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use aberp_audit_ledger::{
        ensure_schema as audit_ensure_schema, BinaryHash, LedgerMeta, TenantId,
    };
    use rust_decimal::Decimal;
    use std::str::FromStr;

    fn open_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("open in-memory DuckDB");
        audit_ensure_schema(&conn).expect("audit-ledger schema");
        ensure_schema(&conn).expect("invoice_draft schema");
        conn
    }

    fn ledger_meta() -> LedgerMeta {
        LedgerMeta::new(
            TenantId::new("test-tenant").unwrap(),
            BinaryHash::from_bytes([0u8; 32]),
        )
    }

    fn actor() -> Actor {
        Actor::test_only()
    }

    #[test]
    fn create_draft_inserts_row_and_emits_audit() {
        let mut conn = open_conn();
        let tx = conn.transaction().unwrap();
        let draft = create_draft_in_tx(
            &tx,
            &ledger_meta(),
            actor(),
            CreateDraftInputs {
                tenant: "test-tenant".to_string(),
                partner_id: "ptr_TESTPARTNER000000000000".to_string(),
                source_dispatch_id: Some("dsp_TESTDISPATCH00000000000".to_string()),
                source_wo_id: Some("wo_TESTWORKORDER0000000000".to_string()),
                product_id: "prd_TESTPRODUCT00000000000".to_string(),
                qty: Decimal::from_str("3.5").unwrap(),
                notes: Some("test note".to_string()),
                actor: "test-actor".to_string(),
                idempotency_key: "idem-key-1".to_string(),
            },
        )
        .expect("create_draft_in_tx");
        tx.commit().unwrap();

        assert!(draft.drf_id.starts_with("drf_"));
        assert_eq!(draft.qty, "3.5");

        let round_trip = read_draft(&conn, "test-tenant", &draft.drf_id)
            .unwrap()
            .expect("draft round-trip");
        assert_eq!(round_trip, draft);
    }

    #[test]
    fn delete_draft_removes_row_without_audit() {
        let mut conn = open_conn();
        let tx = conn.transaction().unwrap();
        let draft = create_draft_in_tx(
            &tx,
            &ledger_meta(),
            actor(),
            CreateDraftInputs {
                tenant: "test-tenant".to_string(),
                partner_id: "ptr_X".to_string(),
                source_dispatch_id: None,
                source_wo_id: None,
                product_id: "prd_X".to_string(),
                qty: Decimal::ONE,
                notes: None,
                actor: "a".to_string(),
                idempotency_key: "idem".to_string(),
            },
        )
        .unwrap();
        tx.commit().unwrap();

        let tx2 = conn.transaction().unwrap();
        let deleted = delete_draft_in_tx(&tx2, "test-tenant", &draft.drf_id).unwrap();
        tx2.commit().unwrap();
        assert!(deleted);
        assert!(read_draft(&conn, "test-tenant", &draft.drf_id)
            .unwrap()
            .is_none());
    }

    #[test]
    fn create_draft_refuses_empty_idempotency_key() {
        let mut conn = open_conn();
        let tx = conn.transaction().unwrap();
        let err = create_draft_in_tx(
            &tx,
            &ledger_meta(),
            actor(),
            CreateDraftInputs {
                tenant: "test-tenant".to_string(),
                partner_id: "ptr_X".to_string(),
                source_dispatch_id: None,
                source_wo_id: None,
                product_id: "prd_X".to_string(),
                qty: Decimal::ONE,
                notes: None,
                actor: "a".to_string(),
                idempotency_key: "  ".to_string(),
            },
        );
        assert!(err.is_err());
    }

    #[test]
    fn invoice_staged_payload_round_trip() {
        let p = InvoiceStagedPayload {
            draft_id: "drf_X".to_string(),
            tenant_id: "t".to_string(),
            partner_id: "ptr_X".to_string(),
            source_dispatch_id: Some("dsp_X".to_string()),
            source_wo_id: Some("wo_X".to_string()),
            product_id: "prd_X".to_string(),
            qty: "1".to_string(),
            actor: "a".to_string(),
            idempotency_key: "i".to_string(),
        };
        let back: InvoiceStagedPayload = serde_json::from_slice(&p.to_bytes()).unwrap();
        assert_eq!(back, p);
    }

    /// `source_dispatch_id: None` MUST serialize as JSON null so the
    /// audit-walker can distinguish "dispatch-spawned" from "writer
    /// forgot." Same posture as the dispatch shipped pin
    /// `shipped_payload_spawned_invoice_id_none_serializes_as_null_not_omitted`.
    #[test]
    fn invoice_staged_payload_none_serializes_as_null_not_omitted() {
        let p = InvoiceStagedPayload {
            draft_id: "drf_X".to_string(),
            tenant_id: "t".to_string(),
            partner_id: "ptr_X".to_string(),
            source_dispatch_id: None,
            source_wo_id: None,
            product_id: "prd_X".to_string(),
            qty: "1".to_string(),
            actor: "a".to_string(),
            idempotency_key: "i".to_string(),
        };
        let v: serde_json::Value = serde_json::from_slice(&p.to_bytes()).unwrap();
        assert!(v["source_dispatch_id"].is_null());
        assert!(v["source_wo_id"].is_null());
    }

    /// S236 / PR-230b — `BillingInvoiceSpawner` is the type that
    /// replaces `aberp_dispatch::NoopInvoiceSpawner` in the production
    /// `serve.rs` `mark_dispatch_shipped_request` path. Driving the
    /// trait directly pins the exact return shape `mark_shipped`
    /// consumes (`Ok(Some(drf_<ULID>))`); a future refactor that drops
    /// the dispatch bridge fields would surface here as well as in
    /// the dispatch crate's `mark_shipped_writes_movement_and_spawns_
    /// draft_in_same_tx` integration test.
    #[test]
    fn billing_spawner_returns_some_draft_id_and_persists_row() {
        use aberp_dispatch::{CarrierKind, Dispatch, DispatchState, InvoiceSpawner};
        let mut conn = open_conn();
        let dsp = Dispatch {
            dsp_id: "dsp_TEST".to_string(),
            wo_id: "wo_TEST".to_string(),
            partner_id: "ptr_TEST".to_string(),
            state: DispatchState::Drafted,
            created_at: "2026-06-03T00:00:00Z".to_string(),
            shipped_at: None,
            cancelled_at: None,
            carrier_kind: Some(CarrierKind::CustomerPickup),
            tracking_number: None,
            spawned_invoice_id: None,
            notes: Some("test note".to_string()),
        };
        let meta = ledger_meta();
        let spawner = BillingInvoiceSpawner {
            tenant: "test-tenant".to_string(),
            actor: "test-actor".to_string(),
            ledger_meta: &meta,
            ledger_actor: actor(),
        };
        let tx = conn.transaction().unwrap();
        let returned = spawner
            .spawn(
                &tx,
                &dsp,
                "prd_TEST",
                Decimal::from_str("5.0").unwrap(),
                "idem-spawn",
            )
            .expect("spawner");
        tx.commit().unwrap();
        let drf_id = returned.expect("spawner returns Some(drf_...)");
        assert!(drf_id.starts_with("drf_"));
        let row = read_draft(&conn, "test-tenant", &drf_id)
            .unwrap()
            .expect("draft row");
        assert_eq!(row.partner_id, "ptr_TEST");
        assert_eq!(row.product_id, "prd_TEST");
        assert_eq!(row.qty, "5.0");
        assert_eq!(row.source_dispatch_id, Some("dsp_TEST".to_string()));
        assert_eq!(row.source_wo_id, Some("wo_TEST".to_string()));
        assert_eq!(row.notes, Some("test note".to_string()));
    }

    /// S236 / PR-230b — second `BillingInvoiceSpawner.spawn` call with
    /// the same idempotency key MUST loud-fail (the audit ledger's F8
    /// pin is the gate). Without this pin, a retried dispatch ship
    /// could silently create two `invoice_draft` rows for the same
    /// dispatch — the [[trust-code-not-operator]] failure mode this
    /// PR is meant to close.
    #[test]
    fn billing_spawner_loud_fails_on_duplicate_idempotency_key() {
        use aberp_dispatch::{CarrierKind, Dispatch, DispatchState, InvoiceSpawner};
        let mut conn = open_conn();
        let dsp = Dispatch {
            dsp_id: "dsp_TEST".to_string(),
            wo_id: "wo_TEST".to_string(),
            partner_id: "ptr_TEST".to_string(),
            state: DispatchState::Drafted,
            created_at: "2026-06-03T00:00:00Z".to_string(),
            shipped_at: None,
            cancelled_at: None,
            carrier_kind: Some(CarrierKind::CustomerPickup),
            tracking_number: None,
            spawned_invoice_id: None,
            notes: None,
        };
        let meta = ledger_meta();
        let spawner = BillingInvoiceSpawner {
            tenant: "test-tenant".to_string(),
            actor: "test-actor".to_string(),
            ledger_meta: &meta,
            ledger_actor: actor(),
        };
        let tx = conn.transaction().unwrap();
        spawner
            .spawn(&tx, &dsp, "prd_TEST", Decimal::ONE, "idem-dup")
            .unwrap();
        tx.commit().unwrap();
        let tx2 = conn.transaction().unwrap();
        let err = spawner.spawn(&tx2, &dsp, "prd_TEST", Decimal::ONE, "idem-dup");
        assert!(
            err.is_err(),
            "BillingInvoiceSpawner must loud-fail on duplicate idempotency key (got {err:?})"
        );
    }
}
