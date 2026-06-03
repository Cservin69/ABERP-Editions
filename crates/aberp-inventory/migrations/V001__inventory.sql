-- S231 / PR-227 / ADR-0061 — Inventory v1 schema.
--
-- Two changes, both forward-only additive per ADR-0061 §1 + the
-- [[no-sql-specific]] posture (no CHECK constraints on derived
-- quantities, no triggers, no stored procedures — the invariant
-- "stock_qty = SUM(qty_delta)" lives in `inventory::record_movement`,
-- not in the storage engine).
--
-- 1) Extend `products` with the denormalised inventory cache columns.
-- 2) Create the append-only `stock_movements` ledger.
--
-- Posture: `IF NOT EXISTS` / `ADD COLUMN IF NOT EXISTS` so re-running
-- this migration against a tenant that already has inventory rows is
-- a no-op (same idempotent CREATE pattern every other ABERP boot
-- migration uses — products / partners / ap_invoice / restored_invoice).

-- 1. Cache columns on products.
--
-- NOTE: DuckDB rejects `ADD COLUMN ... NOT NULL DEFAULT 0` ("Adding
-- columns with constraints not yet supported"). The columns are
-- nullable at the schema layer; the application layer treats NULL as
-- 0 via `COALESCE(stock_qty, 0)` at every read site and never
-- inserts NULL through `record_movement` (which always writes a
-- numeric `stock_qty` back). The follow-up UPDATEs below stamp
-- existing rows so the cache is well-defined out of the gate.
ALTER TABLE products ADD COLUMN IF NOT EXISTS stock_qty        DECIMAL(18,6);
ALTER TABLE products ADD COLUMN IF NOT EXISTS min_stock        DECIMAL(18,6);
ALTER TABLE products ADD COLUMN IF NOT EXISTS bin_location     VARCHAR;
ALTER TABLE products ADD COLUMN IF NOT EXISTS last_movement_at VARCHAR;

-- Backfill NULLs to 0 so future SELECTs do not depend on COALESCE
-- semantics for the existing rows. Idempotent — re-running this is a
-- no-op once every row is non-NULL.
UPDATE products SET stock_qty = 0 WHERE stock_qty IS NULL;
UPDATE products SET min_stock = 0 WHERE min_stock IS NULL;

-- 2. Append-only ledger. `mvt_<ULID>` PK per ADR-0061 §1. Closed-vocab
--    `reason` + `ref_kind` columns are plain VARCHAR — the closed
--    vocabulary lives in `aberp_inventory::types::MovementReason` /
--    `MovementRefKind`, NOT in a DB-level CHECK. Per ADR-0061 §3 +
--    [[no-sql-specific]]: the application layer is the invariant
--    author; engine-portability matters.
-- NOTE: ADR-0061 §1 spells the timestamp column `at`. DuckDB reserves
-- `AT` for `AT TIME ZONE`, so a column literally named `at` is a
-- parser-syntax error. Renamed to `at_iso8601` here — matches the
-- `aberp_mes::CanonicalEvent::ScanReceived.at_iso8601` precedent for
-- RFC3339-stringified timestamps; the spec change is otherwise a pure
-- rename (no semantic divergence from ADR-0061).
CREATE TABLE IF NOT EXISTS stock_movements (
    movement_id     VARCHAR       NOT NULL PRIMARY KEY,
    tenant_id       VARCHAR       NOT NULL,
    product_id      VARCHAR       NOT NULL,
    qty_delta       DECIMAL(18,6) NOT NULL,
    reason          VARCHAR       NOT NULL,
    ref_kind        VARCHAR,
    ref_id          VARCHAR,
    at_iso8601      VARCHAR       NOT NULL,
    operator        VARCHAR       NOT NULL,
    idempotency_key VARCHAR       NOT NULL,
    notes           VARCHAR
);

-- Per-product chronological reads (the SPA's "Stock movements" tab
-- shows descending-by-at for one product) + per-tenant rebuild
-- (`rebuild-stock-cache` walks all movements for the tenant).
CREATE INDEX IF NOT EXISTS stock_movements_tenant_product_at_idx
    ON stock_movements (tenant_id, product_id, at_iso8601);
