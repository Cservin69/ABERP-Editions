-- S234 / PR-230 / ADR-0064 — Dispatch board v1 schema.
--
-- ONE table — `dispatches` — created in the Drafted state by the
-- operator's "Create dispatch" action (or future adapter), flipped to
-- Shipped when the operator marks shipped (emits the Dispatch
-- stock_movement + spawns the Stage 1 invoice draft in the SAME
-- caller-owned transaction). One dispatch per WO in v1; partial
-- shipments are out of scope (ADR-0064 §"Out of scope").
--
-- `state` + `carrier_kind` are plain VARCHAR (no CHECK per
-- [[no-sql-specific]] + ADR-0064 §8 #2) — the closed-vocab
-- `DispatchState` + `CarrierKind` live in `aberp_dispatch::types` and
-- the transition handler refuses illegal edges with 400 at the route
-- boundary. No DB-level FK to work_orders / partners / invoice — the
-- ULID columns are application-level pointers per ADR-0019.
--
-- Posture: `CREATE TABLE IF NOT EXISTS` so re-running this migration
-- against a tenant that already has dispatch rows is a no-op — same
-- idempotent posture every other ABERP boot migration uses.

CREATE TABLE IF NOT EXISTS dispatches (
    dsp_id              VARCHAR NOT NULL PRIMARY KEY,
    tenant_id           VARCHAR NOT NULL,
    wo_id               VARCHAR NOT NULL,
    partner_id          VARCHAR NOT NULL,
    state               VARCHAR NOT NULL,
    created_at          VARCHAR NOT NULL,
    shipped_at          VARCHAR,
    cancelled_at        VARCHAR,
    carrier_kind        VARCHAR,
    tracking_number     VARCHAR,
    spawned_invoice_id  VARCHAR,
    notes               VARCHAR
);

-- Lists by tenant + state are the SPA's primary read pattern (the
-- DispatchList state-facet chips). Default sort is `created_at DESC`
-- per ADR-0064 §7. The (tenant_id, state, created_at) compound
-- supports both the chip filter and the default order without a
-- fanout.
CREATE INDEX IF NOT EXISTS dispatches_tenant_state_created_idx
    ON dispatches (tenant_id, state, created_at);

-- One-dispatch-per-WO uniqueness probe (ADR-0064 §2 #2) AND the
-- WorkOrderDetail dispatch-chip lookup. Application-layer uniqueness
-- gate inside the create_dispatch tx; no DB-level UNIQUE per the
-- [[no-sql-specific]] posture.
CREATE INDEX IF NOT EXISTS dispatches_tenant_wo_idx
    ON dispatches (tenant_id, wo_id);
