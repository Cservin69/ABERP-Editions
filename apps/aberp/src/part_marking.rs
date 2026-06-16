//! S438 (ADR-0089) — per-unit part UID / serial marking + Part UID Lookup.
//!
//! The aerospace-pivot strand that FIRES the never-fired `part.uid_marked` /
//! `part.serial_assigned` EventKinds (S358 foundation, kind-only). Builds on
//! S432 heat-lot traceability + S428 `customer_type` + ADR-0064 dispatch.
//!
//! ## What this is
//!
//! When a defense/aerospace WO is `Completed`, the operator marks each produced
//! unit with a machine-readable IUID. We mint a `dp-`-prefixed ULID per unit
//! (the `dp-` prefix distinguishes a defense part UID from an internal ULID and
//! from operator-typed text), accept an optional operator serial (auto-derived
//! `<wo_id>-<index>` when blank), and store the DataMatrix payload string.
//!
//! ## The shipment gate
//!
//! [[trust-code-not-operator]] — a defense/aerospace dispatch CANNOT be
//! `Shipped` until every unit of its WO bears a part UID. The refusal lives at
//! the dispatch-ship route (mirrors S432's WO-start heat-lot gate), NOT in
//! operator discipline. The non-defense path is unaffected.
//!
//! ## Sparse by design (CLAUDE.md rule 12)
//!
//! Forward trace (part_uid → WO + heat lot + quote + customer) resolves the
//! customer through the WO's originating quote (same chain as
//! [`crate::material_traceability`]); reverse trace (customer → all part UIDs
//! shipped) joins through `Shipped` dispatches. Per-unit physical material
//! consumption is still the WO's recorded `heat_lot_reference` snapshot — what
//! the install HAS, surfaced rather than silently omitted.
//!
//! ## NOT in scope (DÁP / S438+ deferred)
//!
//! Events fire UNSIGNED. The digital-ID / QES signature thread (DÁP) lands in a
//! later session and retroactively brings these rows into the signed chain.

use anyhow::{Context, Result};
use duckdb::{params, Connection};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use ulid::Ulid;

use aberp_audit_ledger::{Actor, BinaryHash, EventKind, Ledger, TenantId};

// ── Pure: part UID / serial / DataMatrix payload ────────────────────

/// Defense part-UID prefix. Distinguishes a marked part UID (`dp-<ULID>`) from
/// an internal `prt_`/`wo_`/`so_` ULID and from operator-typed free text, so a
/// scanned DataMatrix is never mistaken for another id namespace.
pub const PART_UID_PREFIX: &str = "dp-";

/// Crockford base32 alphabet a `ulid::Ulid` renders into (uppercase, no
/// I/L/O/U). Used to validate a `dp-`-stripped body is a real ULID body.
const CROCKFORD: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Render a part UID from a ULID body. Pure — the deterministic half of
/// [`generate_part_uid`], so the format is unit-testable without minting.
pub fn format_part_uid(ulid_body: &str) -> String {
    format!("{PART_UID_PREFIX}{ulid_body}")
}

/// Mint a fresh part UID: `dp-<26-char-ULID>`. The ULID is time+random so two
/// calls never collide; the `dp-` prefix is the namespace tag.
pub fn generate_part_uid() -> String {
    format_part_uid(&Ulid::new().to_string())
}

/// Validate a part UID is `dp-` + a 26-char uppercase-Crockford ULID body.
/// Loud-rejects operator-typed text or a wrong-namespace id (CLAUDE.md rule 12).
pub fn validate_part_uid(s: &str) -> std::result::Result<(), &'static str> {
    let Some(body) = s.strip_prefix(PART_UID_PREFIX) else {
        return Err("part UID must start with 'dp-'");
    };
    if body.len() != 26 {
        return Err("part UID body must be a 26-char ULID");
    }
    if !body.bytes().all(|b| CROCKFORD.contains(&b)) {
        return Err("part UID body has non-Crockford-base32 characters");
    }
    Ok(())
}

/// Auto-derive a serial from the WO id + 1-based unit index when the operator
/// leaves the serial blank. Pure.
pub fn auto_serial(wo_id: &str, unit_index: u32) -> String {
    format!("{wo_id}-{unit_index}")
}

/// Validate (and trim) an operator-typed serial. The `|` byte is REJECTED
/// because it is the DataMatrix payload delimiter — a serial carrying it would
/// corrupt the recoverable three-field scan. Empty is the caller's signal to
/// auto-derive, so this is only called on a non-empty serial.
pub fn validate_serial(s: &str) -> std::result::Result<(), &'static str> {
    let t = s.trim();
    if t.is_empty() {
        return Err("serial must not be blank (caller auto-derives instead)");
    }
    if t.len() > 64 {
        return Err("serial must be at most 64 characters");
    }
    if t.contains('|') {
        return Err("serial must not contain '|' (the DataMatrix delimiter)");
    }
    if t.bytes().any(|b| b.is_ascii_control()) {
        return Err("serial must not contain control characters");
    }
    Ok(())
}

/// The first 8 characters of a heat lot (ASME Y14.41 material-chain tail), or
/// empty when no heat lot was consumed. Pure.
pub fn heat_lot_tail(heat_lot: Option<&str>) -> String {
    heat_lot
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .map(|h| h.chars().take(8).collect())
        .unwrap_or_default()
}

/// Build the DataMatrix payload: `dp-<ULID>|<serial>|<heat_lot_8chars>`.
/// Scanning it at any future point recovers all three identifiers, and the
/// heat-lot tail lets a scanner re-enter the material chain. Pure.
pub fn data_matrix_payload(part_uid: &str, serial: &str, heat_lot: Option<&str>) -> String {
    format!("{part_uid}|{serial}|{}", heat_lot_tail(heat_lot))
}

/// How many discrete units a WO `qty_target` represents. Parts are discrete, so
/// a fractional target rounds UP (defensive — a partial unit still ships whole).
pub fn qty_to_units(qty_target: Decimal) -> u32 {
    let ceil = qty_target.ceil();
    if ceil <= Decimal::ZERO {
        0
    } else {
        ceil.to_u32().unwrap_or(u32::MAX)
    }
}

// ── Schema ──────────────────────────────────────────────────────────

/// Additive per-unit WO-output table. NO surrogate id (natural composite
/// `(tenant_id, wo_id, unit_index)` per the codebase convention), NO CHECK / NO
/// DEFAULT ([[no-sql-specific]] + the DuckDB replay-clobber trap). A non-defense
/// WO simply has zero rows here — the absence IS the back-compat default.
const PART_MARKS_SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS wo_part_marks (
    tenant_id            VARCHAR NOT NULL,
    wo_id                VARCHAR NOT NULL,
    unit_index           INTEGER NOT NULL,
    part_uid             VARCHAR NOT NULL,
    serial_number        VARCHAR NOT NULL,
    data_matrix_payload  VARCHAR NOT NULL,
    heat_lot_reference   VARCHAR,
    marked_at_utc        VARCHAR NOT NULL,
    marked_by_operator   VARCHAR NOT NULL
);
";

/// Idempotent `CREATE TABLE IF NOT EXISTS`. Called at the head of every reader
/// + the mark-parts write path (mirrors `material_inventory::ensure_schema`).
pub fn ensure_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(PART_MARKS_SCHEMA_SQL)
        .context("ensure wo_part_marks schema")
}

// ── Row shape ───────────────────────────────────────────────────────

/// One marked unit. `part_uid` is minted server-side; `serial_number` is
/// operator-typed or auto-derived; `data_matrix_payload` is the scan string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartMark {
    pub wo_id: String,
    pub unit_index: u32,
    pub part_uid: String,
    pub serial_number: String,
    pub data_matrix_payload: String,
    pub heat_lot_reference: Option<String>,
    pub marked_at_utc: String,
    pub marked_by_operator: String,
}

/// Count the marked units recorded for a WO. The shipment gate compares this to
/// the WO `qty_target`; the SPA reads it to render the part-UID chip.
pub fn count_part_marks(conn: &Connection, tenant: &str, wo_id: &str) -> Result<u32> {
    ensure_schema(conn)?;
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM wo_part_marks WHERE tenant_id = ?1 AND wo_id = ?2",
            params![tenant, wo_id],
            |r| r.get(0),
        )
        .context("count wo_part_marks")?;
    Ok(n.max(0) as u32)
}

/// List the marks recorded for a WO, ordered by unit index.
pub fn list_part_marks(conn: &Connection, tenant: &str, wo_id: &str) -> Result<Vec<PartMark>> {
    ensure_schema(conn)?;
    let mut stmt = conn
        .prepare(
            "SELECT wo_id, unit_index, part_uid, serial_number, data_matrix_payload,
                    heat_lot_reference, marked_at_utc, marked_by_operator
               FROM wo_part_marks
              WHERE tenant_id = ?1 AND wo_id = ?2
              ORDER BY unit_index",
        )
        .context("prepare list_part_marks")?;
    let rows = stmt
        .query_map(params![tenant, wo_id], |r| {
            Ok(PartMark {
                wo_id: r.get::<_, String>(0)?,
                unit_index: r.get::<_, i64>(1)?.max(0) as u32,
                part_uid: r.get::<_, String>(2)?,
                serial_number: r.get::<_, String>(3)?,
                data_matrix_payload: r.get::<_, String>(4)?,
                heat_lot_reference: r.get::<_, Option<String>>(5)?,
                marked_at_utc: r.get::<_, String>(6)?,
                marked_by_operator: r.get::<_, String>(7)?,
            })
        })
        .context("query list_part_marks")?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.context("read part mark row")?);
    }
    Ok(out)
}

/// Error from [`record_part_marks`].
#[derive(Debug, thiserror::Error)]
pub enum PartMarkError {
    /// The WO already has marks — re-marking is REFUSED so an accidental second
    /// save can never mint fresh UIDs for parts already physically marked
    /// ([[hulye-biztos]] — you mark once).
    #[error("work order {wo_id} is already marked ({n} units)")]
    AlreadyMarked { wo_id: String, n: u32 },
    /// A supplied serial / part UID failed validation.
    #[error("invalid mark for unit {unit_index}: {reason}")]
    Invalid { unit_index: u32, reason: String },
    /// Anything else (DB / I/O).
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Insert the marks for a WO in one batch. Refuses if the WO is already marked.
/// Validates every part UID + serial before any write so a bad unit aborts the
/// whole batch (no half-marked WO). Audit emission is the caller's
/// [`append_mark_events`] (own `Ledger` after the conn drops — S432 pattern).
pub fn record_part_marks(
    conn: &Connection,
    tenant: &str,
    wo_id: &str,
    marks: &[PartMark],
) -> std::result::Result<(), PartMarkError> {
    ensure_schema(conn)?;
    let existing = count_part_marks(conn, tenant, wo_id)?;
    if existing > 0 {
        return Err(PartMarkError::AlreadyMarked {
            wo_id: wo_id.to_string(),
            n: existing,
        });
    }
    for m in marks {
        validate_part_uid(&m.part_uid).map_err(|e| PartMarkError::Invalid {
            unit_index: m.unit_index,
            reason: e.to_string(),
        })?;
        validate_serial(&m.serial_number).map_err(|e| PartMarkError::Invalid {
            unit_index: m.unit_index,
            reason: e.to_string(),
        })?;
    }
    for m in marks {
        conn.execute(
            "INSERT INTO wo_part_marks (
                tenant_id, wo_id, unit_index, part_uid, serial_number,
                data_matrix_payload, heat_lot_reference, marked_at_utc, marked_by_operator
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                tenant,
                m.wo_id,
                m.unit_index as i64,
                m.part_uid,
                m.serial_number,
                m.data_matrix_payload,
                m.heat_lot_reference,
                m.marked_at_utc,
                m.marked_by_operator,
            ],
        )
        .context("insert wo_part_marks row")?;
    }
    Ok(())
}

/// Emit the marking audit trail (own `Ledger`, after the read/write conn is
/// dropped — mirrors `material_inventory::append_heat_lot_events`). Fires ONE
/// batch `part.serial_assigned` (the logical serial-assignment record) + ONE
/// batch `part.uid_marked` (the physical-mark record, the brief's payload).
/// Both are UNSIGNED (DÁP signature thread is S438+ deferred).
pub fn append_mark_events(
    db_path: &std::path::Path,
    tenant: TenantId,
    binary_hash: BinaryHash,
    wo_id: &str,
    operator: &str,
    marked_at: &str,
    heat_lot_reference: Option<&str>,
    marks: &[PartMark],
) -> Result<usize> {
    let mut ledger = Ledger::open(db_path, tenant, binary_hash)
        .context("open audit ledger to record part marking")?;

    let serials: Vec<_> = marks
        .iter()
        .map(|m| serde_json::json!({ "part_uid": m.part_uid, "serial": m.serial_number }))
        .collect();
    let serial_payload = serde_json::json!({
        "work_order_id": wo_id,
        "qty": marks.len(),
        "serials": serials,
        "operator_user_id": operator,
        "assigned_at": marked_at,
    });
    ledger
        .append(
            EventKind::PartSerialAssigned,
            serde_json::to_vec(&serial_payload).expect("serialize serial-assigned payload"),
            Actor::from_local_cli(Ulid::new().to_string(), operator),
            None,
        )
        .context("append part.serial_assigned")?;

    let parts: Vec<_> = marks
        .iter()
        .map(|m| {
            serde_json::json!({
                "part_uid": m.part_uid,
                "serial": m.serial_number,
                "data_matrix_payload": m.data_matrix_payload,
                "marker": m.marked_by_operator,
            })
        })
        .collect();
    let uid_payload = serde_json::json!({
        "work_order_id": wo_id,
        "qty": marks.len(),
        "parts": parts,
        "heat_lot_reference": heat_lot_reference,
        "operator_user_id": operator,
        "marked_at": marked_at,
    });
    ledger
        .append(
            EventKind::PartUidMarked,
            serde_json::to_vec(&uid_payload).expect("serialize uid-marked payload"),
            Actor::from_local_cli(Ulid::new().to_string(), operator),
            None,
        )
        .context("append part.uid_marked")?;

    Ok(2)
}

// ── Traceability (forward + reverse) ────────────────────────────────

/// How the operator addressed the Part UID Lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PartTraceQueryKind {
    /// Forward trace: a part UID → its WO, heat lot, quote, customer.
    PartUid,
    /// Reverse trace: a customer (partner id) → every part UID shipped to them.
    Customer,
}

impl PartTraceQueryKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            PartTraceQueryKind::PartUid => "part_uid",
            PartTraceQueryKind::Customer => "customer",
        }
    }
}

/// One traced part with its production + customer chain resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartTraceRow {
    pub part_uid: String,
    pub serial_number: String,
    pub data_matrix_payload: String,
    pub heat_lot_reference: Option<String>,
    pub wo_id: String,
    pub wo_number: String,
    pub wo_state: String,
    pub source_quote_id: Option<String>,
    pub customer_partner_id: Option<String>,
    pub customer_name: Option<String>,
}

/// The Part UID Lookup report. `parts` is one row for a forward trace (zero
/// when the UID is unknown), many rows for a reverse trace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartTraceReport {
    pub query_kind: PartTraceQueryKind,
    pub query_value: String,
    pub found: bool,
    pub parts: Vec<PartTraceRow>,
}

/// Resolve the WO + customer chain for a single part-mark row.
fn enrich_row(conn: &Connection, tenant: &str, mark: &PartMark) -> Result<Option<PartTraceRow>> {
    let Some(wo) = aberp_work_orders::read_work_order(conn, tenant, &mark.wo_id)? else {
        return Ok(None);
    };
    // Customer via the WO's originating quote → buyer partner (same chain as
    // material_traceability + the heat-lot gate).
    let (customer_partner_id, customer_name) = match wo.source_quote_id.as_deref() {
        Some(qid) => match crate::quote_pricing_jobs::get_job_detail(conn, qid, tenant)? {
            Some(job) => match job.buyer_partner_id.as_deref() {
                Some(pid) => match crate::partners::get_partner(conn, tenant, pid)? {
                    Some(p) => (Some(p.id), Some(p.display_name)),
                    None => (Some(pid.to_string()), None),
                },
                None => (None, None),
            },
            None => (None, None),
        },
        None => (None, None),
    };
    Ok(Some(PartTraceRow {
        part_uid: mark.part_uid.clone(),
        serial_number: mark.serial_number.clone(),
        data_matrix_payload: mark.data_matrix_payload.clone(),
        heat_lot_reference: mark.heat_lot_reference.clone(),
        wo_id: wo.wo_id,
        wo_number: wo.wo_number,
        wo_state: wo.state.as_str().to_string(),
        source_quote_id: wo.source_quote_id,
        customer_partner_id,
        customer_name,
    }))
}

/// Forward trace: a part UID → the chain that produced it. Read-only; the route
/// layer fires `part.traceability_viewed`.
pub fn trace_part_uid(conn: &Connection, tenant: &str, part_uid: &str) -> Result<PartTraceReport> {
    ensure_schema(conn)?;
    let value = part_uid.trim().to_string();
    let mark: Option<PartMark> = {
        let mut stmt = conn
            .prepare(
                "SELECT wo_id, unit_index, part_uid, serial_number, data_matrix_payload,
                        heat_lot_reference, marked_at_utc, marked_by_operator
                   FROM wo_part_marks
                  WHERE tenant_id = ?1 AND part_uid = ?2
                  LIMIT 1",
            )
            .context("prepare trace_part_uid")?;
        let mut rows = stmt
            .query(params![tenant, value])
            .context("query trace_part_uid")?;
        match rows.next().context("read trace_part_uid row")? {
            Some(r) => Some(PartMark {
                wo_id: r.get::<_, String>(0)?,
                unit_index: r.get::<_, i64>(1)?.max(0) as u32,
                part_uid: r.get::<_, String>(2)?,
                serial_number: r.get::<_, String>(3)?,
                data_matrix_payload: r.get::<_, String>(4)?,
                heat_lot_reference: r.get::<_, Option<String>>(5)?,
                marked_at_utc: r.get::<_, String>(6)?,
                marked_by_operator: r.get::<_, String>(7)?,
            }),
            None => None,
        }
    };
    let parts = match &mark {
        Some(m) => enrich_row(conn, tenant, m)?.into_iter().collect(),
        None => Vec::new(),
    };
    Ok(PartTraceReport {
        found: !parts.is_empty(),
        parts,
        query_kind: PartTraceQueryKind::PartUid,
        query_value: value,
    })
}

/// Reverse trace: a customer (partner id) → every part UID `Shipped` to them.
/// Joins `wo_part_marks` to `Shipped` `dispatches` on `wo_id`. Read-only.
pub fn trace_customer(
    conn: &Connection,
    tenant: &str,
    customer_id: &str,
) -> Result<PartTraceReport> {
    ensure_schema(conn)?;
    aberp_dispatch::ensure_schema(conn)?;
    let value = customer_id.trim().to_string();
    let marks: Vec<PartMark> = {
        let mut stmt = conn
            .prepare(
                "SELECT pm.wo_id, pm.unit_index, pm.part_uid, pm.serial_number,
                        pm.data_matrix_payload, pm.heat_lot_reference, pm.marked_at_utc,
                        pm.marked_by_operator
                   FROM wo_part_marks pm
                   JOIN dispatches d
                     ON d.wo_id = pm.wo_id AND d.tenant_id = pm.tenant_id
                  WHERE pm.tenant_id = ?1 AND d.partner_id = ?2 AND d.state = 'shipped'
                  ORDER BY pm.wo_id, pm.unit_index",
            )
            .context("prepare trace_customer")?;
        let rows = stmt
            .query_map(params![tenant, value], |r| {
                Ok(PartMark {
                    wo_id: r.get::<_, String>(0)?,
                    unit_index: r.get::<_, i64>(1)?.max(0) as u32,
                    part_uid: r.get::<_, String>(2)?,
                    serial_number: r.get::<_, String>(3)?,
                    data_matrix_payload: r.get::<_, String>(4)?,
                    heat_lot_reference: r.get::<_, Option<String>>(5)?,
                    marked_at_utc: r.get::<_, String>(6)?,
                    marked_by_operator: r.get::<_, String>(7)?,
                })
            })
            .context("query trace_customer")?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.context("read trace_customer row")?);
        }
        out
    };
    let mut parts = Vec::new();
    for m in &marks {
        if let Some(row) = enrich_row(conn, tenant, m)? {
            parts.push(row);
        }
    }
    Ok(PartTraceReport {
        found: !parts.is_empty(),
        parts,
        query_kind: PartTraceQueryKind::Customer,
        query_value: value,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_and_validate_part_uid_round_trips() {
        let uid = generate_part_uid();
        assert!(uid.starts_with("dp-"), "minted UID has dp- prefix: {uid}");
        assert_eq!(uid.len(), 3 + 26, "dp- + 26-char ULID body");
        validate_part_uid(&uid).expect("a freshly minted UID validates");
    }

    #[test]
    fn validate_part_uid_rejects_bad_shapes() {
        assert!(validate_part_uid("WO-12").is_err(), "operator text");
        assert!(
            validate_part_uid("01ARZ3NDEKTSV4RRFFQ69G5FAV").is_err(),
            "no dp- prefix"
        );
        assert!(validate_part_uid("dp-tooshort").is_err(), "short body");
        // Crockford excludes I/L/O/U — a body using them is rejected.
        assert!(validate_part_uid("dp-ILOU3NDEKTSV4RRFFQ69G5FAV").is_err());
    }

    #[test]
    fn part_uids_are_unique_within_a_batch() {
        let n = 50;
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..n {
            assert!(seen.insert(generate_part_uid()), "minted UID collided");
        }
        assert_eq!(seen.len(), n);
    }

    #[test]
    fn auto_serial_uses_wo_and_index() {
        assert_eq!(auto_serial("wo_ABC", 1), "wo_ABC-1");
        assert_eq!(auto_serial("wo_ABC", 12), "wo_ABC-12");
    }

    #[test]
    fn validate_serial_rejects_delimiter_and_blank() {
        assert!(validate_serial("SN-001").is_ok());
        assert!(validate_serial("A|B").is_err(), "pipe is the delimiter");
        assert!(validate_serial("   ").is_err(), "blank");
        assert!(validate_serial(&"x".repeat(65)).is_err(), "too long");
    }

    #[test]
    fn data_matrix_payload_recovers_three_fields() {
        let p = data_matrix_payload(
            "dp-01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "SN-7",
            Some("HEAT-1234567"),
        );
        let segs: Vec<&str> = p.split('|').collect();
        assert_eq!(segs.len(), 3);
        assert_eq!(segs[0], "dp-01ARZ3NDEKTSV4RRFFQ69G5FAV");
        assert_eq!(segs[1], "SN-7");
        assert_eq!(segs[2], "HEAT-123", "first 8 chars of the heat lot");
    }

    #[test]
    fn data_matrix_payload_empty_heat_tail_when_no_lot() {
        let p = data_matrix_payload("dp-01ARZ3NDEKTSV4RRFFQ69G5FAV", "SN-7", None);
        assert!(p.ends_with('|'), "third segment empty: {p}");
    }

    #[test]
    fn qty_to_units_ceils_and_floors() {
        assert_eq!(qty_to_units(Decimal::from(5)), 5);
        assert_eq!(qty_to_units(Decimal::ZERO), 0);
        assert_eq!(
            qty_to_units(Decimal::from_str_exact("2.1").unwrap()),
            3,
            "fractional rounds up"
        );
    }

    fn open_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("open in-memory DuckDB");
        aberp_audit_ledger::ensure_schema(&conn).expect("audit-ledger schema");
        ensure_schema(&conn).expect("part-marks schema");
        conn
    }

    fn sample_mark(wo_id: &str, i: u32) -> PartMark {
        let part_uid = generate_part_uid();
        let serial = auto_serial(wo_id, i);
        let payload = data_matrix_payload(&part_uid, &serial, Some("HEAT-1234"));
        PartMark {
            wo_id: wo_id.to_string(),
            unit_index: i,
            part_uid,
            serial_number: serial,
            data_matrix_payload: payload,
            heat_lot_reference: Some("HEAT-1234".to_string()),
            marked_at_utc: "2026-06-16T00:00:00Z".to_string(),
            marked_by_operator: "op".to_string(),
        }
    }

    #[test]
    fn record_then_count_and_list_round_trips() {
        let conn = open_conn();
        let marks: Vec<_> = (1..=3).map(|i| sample_mark("wo-1", i)).collect();
        record_part_marks(&conn, "t", "wo-1", &marks).unwrap();
        assert_eq!(count_part_marks(&conn, "t", "wo-1").unwrap(), 3);
        let listed = list_part_marks(&conn, "t", "wo-1").unwrap();
        assert_eq!(listed, marks, "round-trips byte-for-byte, ordered by index");
        // A WO with no marks reads zero (non-defense back-compat default).
        assert_eq!(count_part_marks(&conn, "t", "wo-2").unwrap(), 0);
    }

    #[test]
    fn record_refuses_double_marking() {
        let conn = open_conn();
        record_part_marks(&conn, "t", "wo-1", &[sample_mark("wo-1", 1)]).unwrap();
        let err = record_part_marks(&conn, "t", "wo-1", &[sample_mark("wo-1", 1)]).unwrap_err();
        assert!(matches!(err, PartMarkError::AlreadyMarked { n: 1, .. }));
    }

    #[test]
    fn record_rejects_invalid_part_uid_atomically() {
        let conn = open_conn();
        let mut bad = sample_mark("wo-1", 1);
        bad.part_uid = "WO-typed-by-operator".to_string();
        let err = record_part_marks(&conn, "t", "wo-1", &[bad]).unwrap_err();
        assert!(matches!(err, PartMarkError::Invalid { unit_index: 1, .. }));
        // Nothing was written — the batch aborts before any INSERT.
        assert_eq!(count_part_marks(&conn, "t", "wo-1").unwrap(), 0);
    }

    /// `append_mark_events` fires EXACTLY one `part.serial_assigned` + one
    /// `part.uid_marked` (the two foundation kinds this session FIRES).
    #[test]
    fn append_mark_events_fires_both_part_kinds() {
        let dir = std::env::temp_dir()
            .join("aberp-part-mark-test")
            .join(Ulid::new().to_string());
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("aberp.duckdb");
        {
            let conn = Connection::open(&db_path).unwrap();
            aberp_audit_ledger::ensure_schema(&conn).unwrap();
            ensure_schema(&conn).unwrap();
        }
        let tenant = TenantId::new("t").unwrap();
        let hash = BinaryHash::from_bytes([0u8; 32]);
        let marks: Vec<_> = (1..=2).map(|i| sample_mark("wo-1", i)).collect();
        let n = append_mark_events(
            &db_path,
            tenant,
            hash,
            "wo-1",
            "op",
            "2026-06-16T00:00:00Z",
            Some("HEAT-1234"),
            &marks,
        )
        .unwrap();
        assert_eq!(n, 2);

        let conn = Connection::open(&db_path).unwrap();
        let one = |kind: &str| -> i64 {
            conn.query_row(
                "SELECT COUNT(*) FROM audit_ledger WHERE kind = ?1",
                params![kind],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(one("part.serial_assigned"), 1);
        assert_eq!(one("part.uid_marked"), 1);
    }
}
