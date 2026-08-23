-- ADR-0199 (PROVISIONAL NUMBER) — QC inspection reports + Certificate of
-- Conformance: the REPORTING layer on top of the S443 / ADR-0092
-- measurement model in `V002__qc.sql`.
--
-- STRICTLY ADDITIVE. No existing column changes type or nullability; no
-- existing row is rewritten. Three new tables + six new nullable columns
-- on `qc_inspection_plans`.
--
-- Posture (inherited verbatim from V002): natural-key PKs,
-- `CREATE TABLE IF NOT EXISTS` / `ADD COLUMN IF NOT EXISTS` (idempotent —
-- `ensure_schema` runs at the top of every writer), and **no CHECK, no
-- DEFAULT, no UNIQUE**. Every invariant (closed vocabularies, the
-- accountability arithmetic, one current drawing revision per product,
-- the disposition rule) lives in code per [[no-sql-specific]].
--
-- The DEFAULT-on-replay trap the partners PR-97 migration documents is
-- why no ADD COLUMN here carries a DEFAULT: `ADD COLUMN IF NOT EXISTS …
-- DEFAULT V` re-applies V on every replay, and a replay happens on every
-- unrelated write. NULL is the honest "not captured yet" sentinel and the
-- app layer reads it as such.

-- ── (a) Characteristic identity on the EXISTING plan table ──────────
--
-- ADR-0199 §D3(a): `qc_inspection_plans` is already the nominal/tolerance
-- source of truth and already what the verdict is computed against. The
-- AS9102 Form 3 fields are the plan's missing IDENTITY metadata, not a
-- second concept — a separate "characteristic" table would immediately
-- mean two places to state a tolerance.
--
--   characteristic_number     balloon no. as it appears on the drawing ("14", "7.2")
--   characteristic_designator key | critical | major | minor | none  (closed vocab in code)
--   characteristic_type       dimensional | material | process | note | functional
--   inspection_method         on_machine_probe | cmm | gauge | visual | cert_review
--   sheet_zone                drawing sheet + zone, e.g. "2/B4"
--   is_required               counts toward accountability (ADR-0199 §D4)
ALTER TABLE qc_inspection_plans
    ADD COLUMN IF NOT EXISTS characteristic_number VARCHAR;
ALTER TABLE qc_inspection_plans
    ADD COLUMN IF NOT EXISTS characteristic_designator VARCHAR;
ALTER TABLE qc_inspection_plans
    ADD COLUMN IF NOT EXISTS characteristic_type VARCHAR;
ALTER TABLE qc_inspection_plans
    ADD COLUMN IF NOT EXISTS inspection_method VARCHAR;
ALTER TABLE qc_inspection_plans
    ADD COLUMN IF NOT EXISTS sheet_zone VARCHAR;
ALTER TABLE qc_inspection_plans
    ADD COLUMN IF NOT EXISTS is_required BOOLEAN;

-- ── (b) Drawing identity ────────────────────────────────────────────
--
-- ADR-0199 §C2 #3: NO drawing number and NO drawing revision exist
-- anywhere in the repo, so AS9102 Form 1 fields 6-7 cannot be filled from
-- today's data. This is the smallest table that closes that gap.
--
-- Revision history is KEPT (`superseded_at`), never overwritten: a report
-- issued in 2026 must still name the revision it was inspected against in
-- 2033. "Exactly one current revision per (tenant, product_id)" is
-- enforced in `qc::drawings`, not by a SQL UNIQUE — same posture as
-- `qc::plans`' own (product, feature) uniqueness.
CREATE TABLE IF NOT EXISTS part_drawing_refs (
    drawing_ref_id  VARCHAR NOT NULL PRIMARY KEY,  -- pdr_<ULID>
    tenant_id       VARCHAR NOT NULL,
    product_id      VARCHAR NOT NULL,
    drawing_number  VARCHAR NOT NULL,
    drawing_rev     VARCHAR NOT NULL,
    effective_from  VARCHAR NOT NULL,   -- RFC3339
    superseded_at   VARCHAR,            -- NULL = current
    created_at      VARCHAR NOT NULL,
    created_by      VARCHAR NOT NULL
);

CREATE INDEX IF NOT EXISTS part_drawing_refs_tenant_product_idx
    ON part_drawing_refs (tenant_id, product_id);

-- ── (c) The report record — a FROZEN snapshot ───────────────────────
--
-- ADR-0199 §C3: a QC report is a COMPLIANCE RECORD, in the same class as
-- an issued invoice. Once it goes out the door attached to a shipment,
-- what it said at that moment is the fact. `qc_inspection_plans` rows are
-- mutable (`update_plan` / `archive_plan`), so a report rendered live
-- would silently rewrite its own history the first time an operator edits
-- a tolerance. `qc_inspections` already made this exact call one level
-- down (its plan values are denormalised snapshots); the report layer
-- inherits that discipline.
--
-- Every `drawing_*` / `heat_lot_*` / `serial_range` / `machine_id` column
-- below is therefore a SNAPSHOT resolved once at issuance, never
-- re-derived at render time.
--
-- `rendered_sha256` + `renderer_version` are set at issuance and are the
-- audit-retention mechanism (ADR-0199 §D7): the BYTES are never stored,
-- the hash is pinned into the chain, and the report re-renders
-- deterministically from these rows.
CREATE TABLE IF NOT EXISTS qc_reports (
    qcr_id              VARCHAR NOT NULL PRIMARY KEY,   -- qcr_<ULID>
    tenant_id           VARCHAR NOT NULL,
    report_number       VARCHAR NOT NULL,   -- operator-facing, allocated in code
    report_kind         VARCHAR NOT NULL,   -- dimensional_inspection | coc | as9102_fair
    template            VARCHAR NOT NULL,   -- QcReportTemplate token
    state               VARCHAR NOT NULL,   -- drafted | issued | superseded | voided
    wo_id               VARCHAR NOT NULL,
    product_id          VARCHAR NOT NULL,
    dsp_id              VARCHAR,            -- set when bound to a shipment (§D6)
    partner_id          VARCHAR NOT NULL,
    source_quote_id     VARCHAR,
    drawing_number      VARCHAR,            -- SNAPSHOT
    drawing_rev         VARCHAR,            -- SNAPSHOT
    qty_reported        INTEGER NOT NULL,
    serial_range        VARCHAR,            -- human-readable, snapshot
    heat_lot_reference  VARCHAR,
    mill_cert_id        VARCHAR,
    machine_id          VARCHAR,
    program_id          VARCHAR,
    disposition         VARCHAR NOT NULL,   -- accept | accept_with_ncr | reject | incomplete
    characteristics_required    INTEGER NOT NULL,
    characteristics_measured    INTEGER NOT NULL,
    characteristics_passed      INTEGER NOT NULL,
    characteristics_failed      INTEGER NOT NULL,
    characteristics_unaccounted INTEGER NOT NULL,
    rendered_sha256     VARCHAR,            -- set at issuance (§D7)
    renderer_version    VARCHAR,            -- set at issuance
    issued_at_utc       VARCHAR,
    issued_by           VARCHAR,
    superseded_by_qcr_id VARCHAR,
    created_at          VARCHAR NOT NULL,
    created_by          VARCHAR NOT NULL,
    notes               VARCHAR
);

-- ── (d) The frozen characteristic lines ─────────────────────────────
--
-- ADR-0199 §D4 — accountability. For each serialised unit in scope the
-- report enumerates EVERY enabled, required, non-archived plan
-- characteristic and joins measurements onto it. A characteristic with no
-- measurement is written as an explicit row with
-- `accountability = 'not_measured'` and a NULL `actual_value` — it is
-- NEVER silently omitted. That omission is exactly the selective-recording
-- failure mode ADR-0092 exists to end, moved from the shop floor to the
-- printer.
CREATE TABLE IF NOT EXISTS qc_report_lines (
    qcrl_id                 VARCHAR NOT NULL PRIMARY KEY,   -- qcrl_<ULID>
    tenant_id               VARCHAR NOT NULL,
    qcr_id                  VARCHAR NOT NULL,
    line_no                 INTEGER NOT NULL,               -- render order, stable
    part_serial             VARCHAR,                        -- NULL = lot-level
    part_uid                VARCHAR,
    characteristic_number   VARCHAR,
    characteristic_name     VARCHAR NOT NULL,
    characteristic_designator VARCHAR,
    characteristic_type     VARCHAR NOT NULL,
    inspection_method       VARCHAR,
    sheet_zone              VARCHAR,
    nominal_value           DOUBLE,
    upper_tol               DOUBLE,
    lower_tol               DOUBLE,
    units                   VARCHAR,
    actual_value            DOUBLE,          -- NULL iff accountability = not_measured
    deviation               DOUBLE,
    verdict                 VARCHAR,         -- reuses aberp_qa::qc::Verdict tokens
    accountability          VARCHAR NOT NULL,-- measured | not_measured | not_applicable
    qci_id                  VARCHAR,         -- the qc_inspections row this froze
    measured_at_utc         VARCHAR,
    measured_by             VARCHAR,
    probe_serial            VARCHAR,
    created_at              VARCHAR NOT NULL
);

-- Two indexes only, mirroring V002's restraint.
CREATE INDEX IF NOT EXISTS qc_reports_tenant_dsp_idx
    ON qc_reports (tenant_id, dsp_id);

CREATE INDEX IF NOT EXISTS qc_report_lines_tenant_report_idx
    ON qc_report_lines (tenant_id, qcr_id);
