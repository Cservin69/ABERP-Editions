//! DuckDB schema for the audit-ledger table.
//!
//! Single table, one row per entry. Per ADR-0019, no foreign keys.
//! `UNIQUE(seq)` and `UNIQUE(id)` are the integrity invariants;
//! `CHECK(seq >= 1)` rejects garbage at the schema boundary.
//!
//! Per ADR-0008 §"Storage", the ledger "lives in its own DuckDB table
//! inside the tenant database" — i.e. one `audit_ledger` table per
//! tenant DuckDB file. Multi-tenant separation is at the file level,
//! not at the row level (ADR-0002).
//!
//! The table name `audit_ledger` is inlined into the SQL constants
//! below rather than threaded through a `const TABLE: &str`. The name
//! never changes; an indirection would be ceremony per CLAUDE.md rule 2.

/// `CREATE TABLE IF NOT EXISTS` DDL for the audit-ledger table.
///
/// Column order intentionally matches ADR-0008 §"Entry shape" reading
/// order for review clarity. The canonical CBOR encoding does NOT use
/// this order — it uses [`crate::canonical`]'s RFC 8949 §4.2.1 order —
/// so changes to this DDL never affect the hash chain.
pub const CREATE_TABLE: &str = "
CREATE TABLE IF NOT EXISTS audit_ledger (
    id              VARCHAR     NOT NULL,
    seq             BIGINT      NOT NULL CHECK (seq >= 1),
    prev_hash       BLOB        NOT NULL,
    time_wall       VARCHAR     NOT NULL,
    time_mono       BIGINT      NOT NULL CHECK (time_mono >= 0),
    actor           VARCHAR     NOT NULL,
    binary_hash     BLOB        NOT NULL,
    tenant_id       VARCHAR     NOT NULL,
    kind            VARCHAR     NOT NULL,
    payload         BLOB        NOT NULL,
    idempotency_key VARCHAR,
    entry_hash      BLOB        NOT NULL,
    UNIQUE (seq),
    UNIQUE (id)
);
";

/// SQL to insert a row. Parameter order matches the `?` placeholders.
pub const INSERT: &str = "
INSERT INTO audit_ledger
    (id, seq, prev_hash, time_wall, time_mono, actor,
     binary_hash, tenant_id, kind, payload, idempotency_key, entry_hash)
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?);
";

/// SQL to read all rows in seq order.
pub const SELECT_ALL: &str = "
SELECT id, seq, prev_hash, time_wall, time_mono, actor,
       binary_hash, tenant_id, kind, payload, idempotency_key, entry_hash
FROM audit_ledger
ORDER BY seq ASC;
";

/// SQL to read the latest entry (highest seq) — used by `append` to
/// compute `prev_hash` and `seq` for the new row.
pub const SELECT_HEAD: &str = "
SELECT id, seq, prev_hash, time_wall, time_mono, actor,
       binary_hash, tenant_id, kind, payload, idempotency_key, entry_hash
FROM audit_ledger
ORDER BY seq DESC
LIMIT 1;
";
