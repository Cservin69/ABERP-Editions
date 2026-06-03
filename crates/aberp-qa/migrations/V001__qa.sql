-- S233 / PR-229 / ADR-0063 — QA queue v1 schema.
--
-- ONE table — `qa_inspections` — auto-created when a routing-op flips
-- to Completed. Closed-vocab `state` column (no CHECK per
-- [[no-sql-specific]] — the transition table lives in
-- `aberp_qa::state::next_qa_state`). `superseded_by` is the
-- denormalised cross-actor-override pointer per ADR-0063 §4 (denormalised
-- so the live-state SELECT is one filter, not an audit walk — same
-- posture as the [[storno-workflow-adr0049]] `is_storno` field).
--
-- Posture: `CREATE TABLE IF NOT EXISTS` so re-running this migration
-- against a tenant that already has QA rows is a no-op — same
-- idempotent posture every other ABERP boot migration uses.

CREATE TABLE IF NOT EXISTS qa_inspections (
    qa_id              VARCHAR NOT NULL PRIMARY KEY,
    tenant_id          VARCHAR NOT NULL,
    wo_id              VARCHAR NOT NULL,
    routing_op_id      VARCHAR NOT NULL,
    state              VARCHAR NOT NULL,
    decided_at         VARCHAR,
    decided_by         VARCHAR,
    reason             VARCHAR,
    measurement        VARCHAR,
    source_event_id    VARCHAR,
    created_at         VARCHAR NOT NULL,
    superseded_by      VARCHAR
);

-- Queue-list-by-state is the SPA's primary read pattern (the QA tab
-- defaults to the `Pending` filter). Compound on (tenant_id, state,
-- created_at) supports both the chip filter and the default ascending
-- order without a fanout.
CREATE INDEX IF NOT EXISTS qa_inspections_tenant_state_created_idx
    ON qa_inspections (tenant_id, state, created_at);

-- Live-inspection-per-routing-op read (the WorkOrderDetail per-op QA
-- chip + the WO-completion gate both filter on this triple). The
-- `superseded_by IS NULL` predicate is applied at query time per the
-- [[no-sql-specific]] posture; no partial index — DuckDB does not
-- support them and they would split DB-engine portability anyway.
CREATE INDEX IF NOT EXISTS qa_inspections_tenant_wo_routing_idx
    ON qa_inspections (tenant_id, wo_id, routing_op_id);
