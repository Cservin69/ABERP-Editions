//! S429 — closed-loop calibration of quote→actual machining minutes.
//!
//! The wiring half of the [`aberp_quote_engine::calibration`] math:
//!
//! - **Samples** live in `quote_calibration_samples` — one append-only row per
//!   Completed work-order that was linked to an auto-quote AND carried a
//!   recorded actual machining time. Never edited or deleted (history pure,
//!   [[trust-code-not-operator]]).
//! - **The hook** ([`record_calibration_for_completed_wo`]) runs from the serve
//!   WO-transition handler AFTER the Complete transaction commits — the crate
//!   can't reach `quote_pricing_jobs`, so emission lives app-side.
//! - **The table** ([`materialize_table`]) is rebuilt on every quote-create
//!   (cheap query) and fed to [`aberp_quote_engine::quote_with_calibration`].
//!
//! Conventions mirror [`crate::quoting_machines`]: prefixed-ULID id
//! (`qcs_<ULID>`), lazy `CREATE TABLE IF NOT EXISTS`, invariants in code not in
//! SQL ([[no-sql-specific]]), the `machine_family` column stores the stable
//! db-string ([`aberp_quote_engine::MachineFamily::as_db_str`]).

use anyhow::{Context, Result};
use duckdb::{params, Connection};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use ulid::Ulid;

use aberp_audit_ledger::{Actor, BinaryHash, EventKind, Ledger, TenantId};
use aberp_quote_engine::{
    coefficient, CalibrationSample, CalibrationTable, MachineFamily, QuoteBreakdown,
};

use crate::audit_payloads::{
    QuoteCalibrationCoefficientShiftedPayload, QuoteCalibrationSampleRecordedPayload,
    QuoteCalibrationSampleSkippedPayload,
};

/// Below this absolute change a recomputed coefficient is treated as
/// unchanged — no `QuoteCalibrationCoefficientShifted` audit is emitted.
const COEFFICIENT_SHIFT_EPSILON: f64 = 1e-6;

/// How many recent samples per family the SPA chart shows.
const OVERVIEW_SAMPLES_PER_FAMILY: usize = 20;
/// How many recent ledger entries the skip-list scan reads.
const SKIP_SCAN_WINDOW: u32 = 2000;
/// How many recent skips the SPA shows.
const OVERVIEW_SKIPS: usize = 20;

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS quote_calibration_samples (
    id                VARCHAR NOT NULL PRIMARY KEY,
    tenant_id         VARCHAR NOT NULL,
    job_id            VARCHAR NOT NULL,
    machine_family    VARCHAR NOT NULL,
    estimated_minutes DOUBLE  NOT NULL,
    actual_minutes    DOUBLE  NOT NULL,
    sample_at_utc     VARCHAR NOT NULL
);
";

/// Lazily create the samples table. Called at daemon boot and defensively by
/// every reader/writer below.
pub fn ensure_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA_SQL)
        .context("ensure quote_calibration_samples schema")
}

/// Load every sample for the tenant in chronological order (oldest first) — the
/// order [`coefficient`] / [`CalibrationTable::from_samples`] expect (the window
/// takes the most recent). Unknown family db-strings are skipped, not bucketed.
pub fn load_samples(conn: &Connection, tenant: &str) -> Result<Vec<CalibrationSample>> {
    ensure_schema(conn)?;
    let mut stmt = conn
        .prepare(
            "SELECT machine_family, estimated_minutes, actual_minutes
             FROM quote_calibration_samples
             WHERE tenant_id = ?
             ORDER BY sample_at_utc ASC, id ASC;",
        )
        .context("prepare load_samples")?;
    let rows = stmt
        .query_map(params![tenant], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, f64>(1)?,
                row.get::<_, f64>(2)?,
            ))
        })
        .context("query load_samples")?;
    let mut out = Vec::new();
    for r in rows {
        let (family_str, estimated_minutes, actual_minutes) = r.context("read sample row")?;
        if let Some(family) = MachineFamily::from_db_str(&family_str) {
            out.push(CalibrationSample {
                family,
                estimated_minutes,
                actual_minutes,
            });
        }
    }
    Ok(out)
}

/// Rebuild the per-family coefficient table from the current samples. Cheap
/// enough to call on every quote-create.
pub fn materialize_table(conn: &Connection, tenant: &str) -> Result<CalibrationTable> {
    Ok(CalibrationTable::from_samples(&load_samples(conn, tenant)?))
}

/// Insert one append-only sample. Returns the minted `qcs_<ULID>` id.
fn insert_sample(
    conn: &Connection,
    tenant: &str,
    job_id: &str,
    family: MachineFamily,
    estimated_minutes: f64,
    actual_minutes: f64,
    now: OffsetDateTime,
) -> Result<String> {
    ensure_schema(conn)?;
    let id = format!("qcs_{}", Ulid::new());
    let sample_at = now.format(&Rfc3339).context("format sample_at_utc")?;
    conn.execute(
        "INSERT INTO quote_calibration_samples (
            id, tenant_id, job_id, machine_family,
            estimated_minutes, actual_minutes, sample_at_utc
         ) VALUES (?, ?, ?, ?, ?, ?, ?);",
        params![
            &id,
            tenant,
            job_id,
            family.as_db_str(),
            estimated_minutes,
            actual_minutes,
            &sample_at,
        ],
    )
    .context("INSERT quote_calibration_sample")?;
    Ok(id)
}

/// The engine's PRE-coefficient base estimate + routed family for a priced
/// quote, read back from its persisted breakdown. `None` when the quote has no
/// priced breakdown (never priced, or pre-S271 row).
///
/// `estimated_total = (machining_minutes / calibration_coefficient) * quantity`
/// — recovering the base from the post-coefficient value the engine stored.
fn read_quote_estimate(
    conn: &Connection,
    tenant: &str,
    job_id: &str,
) -> Result<Option<(MachineFamily, f64)>> {
    let row = conn
        .query_row(
            "SELECT breakdown_json, quantity
             FROM quote_pricing_jobs
             WHERE tenant_id = ? AND quote_id = ? LIMIT 1;",
            params![tenant, job_id],
            |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, i64>(1)?)),
        )
        .map(Some)
        .or_else(|e| match e {
            duckdb::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })
        .context("read quote_pricing_jobs for calibration estimate")?;

    let Some((Some(breakdown_json), quantity)) = row else {
        return Ok(None);
    };
    let breakdown: QuoteBreakdown =
        serde_json::from_str(&breakdown_json).context("decode breakdown_json")?;
    let coeff = if breakdown.calibration_coefficient > 0.0 {
        breakdown.calibration_coefficient
    } else {
        1.0
    };
    let base_per_part = breakdown.machining_minutes / coeff;
    let estimated_total = base_per_part * (quantity.max(0) as f64);
    let family = MachineFamily::for_route(breakdown.route_to_5_axis);
    Ok(Some((family, estimated_total)))
}

/// Append calibration audit entries through one ledger open. Mirrors
/// [`crate::quoting_machines::append_machine_event`] but batches the (recorded
/// + possible shift) pair. The caller MUST drop its DuckDB write connection
/// before calling this — opening the ledger is a second connection to the same
/// file and a held write conn silently loses the append (the S427 bug).
fn append_calibration_events(
    db_path: &std::path::Path,
    tenant: TenantId,
    binary_hash: BinaryHash,
    operator_login: &str,
    events: Vec<(EventKind, Vec<u8>)>,
) -> Result<()> {
    if events.is_empty() {
        return Ok(());
    }
    let mut ledger = Ledger::open(db_path, tenant, binary_hash)
        .context("open audit ledger to record calibration event")?;
    for (kind, payload) in events {
        let actor = Actor::from_local_cli(Ulid::new().to_string(), operator_login);
        ledger
            .append(kind, payload, actor, None)
            .context("append calibration audit entry")?;
    }
    Ok(())
}

/// The closed-loop hook — run after a WO Complete transaction commits.
///
/// - WO not linked to a quote (`source_quote_id` is `None`) → nothing.
/// - Linked but no priced breakdown to compare against → skip + audit.
/// - Linked but no recorded actual machining time → skip + audit.
/// - Linked WITH an actual → record a sample, emit the recorded audit, and (if
///   the family's coefficient moved) a coefficient-shift audit.
///
/// Calibration is observational: a failure here is logged loud but never
/// unwinds the WO Complete (that already committed).
pub fn record_calibration_for_completed_wo(
    db_path: &std::path::Path,
    tenant: &TenantId,
    binary_hash: BinaryHash,
    operator_login: &str,
    work_order_id: &str,
    source_quote_id: Option<&str>,
    actual_machining_minutes: Option<f64>,
) -> Result<()> {
    let Some(job_id) = source_quote_id else {
        // Not a quote-originated WO — calibration has nothing to learn.
        return Ok(());
    };

    let conn = Connection::open(db_path).context("open tenant DuckDB for calibration hook")?;

    let estimate = read_quote_estimate(&conn, tenant.as_str(), job_id)?;

    // Skip helper: emit one skip audit and return. The conn is dropped first so
    // the ledger append cannot race the write connection.
    let skip = |conn: Connection, reason: &str| -> Result<()> {
        drop(conn);
        let payload = QuoteCalibrationSampleSkippedPayload {
            quote_id: job_id.to_string(),
            work_order_id: work_order_id.to_string(),
            reason: reason.to_string(),
        };
        append_calibration_events(
            db_path,
            tenant.clone(),
            binary_hash,
            operator_login,
            vec![(EventKind::QuoteCalibrationSampleSkipped, payload.to_bytes())],
        )
    };

    let Some((family, estimated_total)) = estimate else {
        return skip(
            conn,
            "linked quote has no priced breakdown to calibrate against",
        );
    };

    let Some(actual) = actual_machining_minutes else {
        return skip(
            conn,
            "work order completed without a recorded actual machining time",
        );
    };

    // Coefficient BEFORE this sample, then INSERT, then AFTER — so a shift is
    // detectable. All DuckDB work finishes before the ledger is opened.
    let before = coefficient(family, &load_samples(&conn, tenant.as_str())?);
    let now = OffsetDateTime::now_utc();
    let sample_id = insert_sample(
        &conn,
        tenant.as_str(),
        job_id,
        family,
        estimated_total,
        actual,
        now,
    )?;
    let after = coefficient(family, &load_samples(&conn, tenant.as_str())?);
    drop(conn);

    let mut events: Vec<(EventKind, Vec<u8>)> = Vec::with_capacity(2);
    events.push((
        EventKind::QuoteCalibrationSampleRecorded,
        QuoteCalibrationSampleRecordedPayload {
            sample_id,
            quote_id: job_id.to_string(),
            work_order_id: work_order_id.to_string(),
            machine_family: family.as_db_str().to_string(),
            estimated_minutes: estimated_total,
            actual_minutes: actual,
        }
        .to_bytes(),
    ));
    if (after - before).abs() > COEFFICIENT_SHIFT_EPSILON {
        events.push((
            EventKind::QuoteCalibrationCoefficientShifted,
            QuoteCalibrationCoefficientShiftedPayload {
                machine_family: family.as_db_str().to_string(),
                previous_coefficient: before,
                new_coefficient: after,
            }
            .to_bytes(),
        ));
    }
    append_calibration_events(db_path, tenant.clone(), binary_hash, operator_login, events)
}

// ── SPA read model ──────────────────────────────────────────────────

/// One sample point for the SPA chart (chronological; the chart plots by
/// index, oldest→newest).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CalibrationSamplePoint {
    pub estimated_minutes: f64,
    pub actual_minutes: f64,
    /// `actual / estimated` (the empirical coefficient for this single job).
    pub ratio: f64,
}

/// Per-family calibration view.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FamilyCalibration {
    /// Family db-string (`MachineFamily::as_db_str`).
    pub machine_family: String,
    /// The current applied coefficient (trimmed mean, clamped).
    pub coefficient: f64,
    /// Total samples on record for this family.
    pub sample_count: usize,
    /// Most-recent samples (oldest→newest), capped for the chart.
    pub samples: Vec<CalibrationSamplePoint>,
}

/// A WO that lost calibration signal (no MES actual / no priced breakdown).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CalibrationSkip {
    pub at_utc: String,
    pub quote_id: String,
    pub work_order_id: String,
    pub reason: String,
}

/// The whole Calibration page payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CalibrationOverview {
    pub families: Vec<FamilyCalibration>,
    pub recent_skips: Vec<CalibrationSkip>,
    /// Hash of the active coefficient set (reproducibility — matches the
    /// `QuoteCalibrationApplied` audit).
    pub coefficient_set_hash: String,
}

/// Build the read-only Calibration page model: per-family coefficient + recent
/// samples, plus recent skips from the audit ledger.
pub fn calibration_overview(conn: &Connection, tenant: &str) -> Result<CalibrationOverview> {
    ensure_schema(conn)?;
    let samples = load_samples(conn, tenant)?;
    let table = CalibrationTable::from_samples(&samples);

    // Group samples by family in chronological order.
    let mut families: Vec<FamilyCalibration> = Vec::new();
    for family in MachineFamily::ALL {
        let fam_samples: Vec<&CalibrationSample> =
            samples.iter().filter(|s| s.family == family).collect();
        if fam_samples.is_empty() {
            continue;
        }
        let sample_count = fam_samples.len();
        let points: Vec<CalibrationSamplePoint> = fam_samples
            .iter()
            .rev()
            .take(OVERVIEW_SAMPLES_PER_FAMILY)
            .rev()
            .map(|s| CalibrationSamplePoint {
                estimated_minutes: s.estimated_minutes,
                actual_minutes: s.actual_minutes,
                ratio: if s.estimated_minutes > 0.0 {
                    s.actual_minutes / s.estimated_minutes
                } else {
                    0.0
                },
            })
            .collect();
        families.push(FamilyCalibration {
            machine_family: family.as_db_str().to_string(),
            coefficient: table.coefficient(family),
            sample_count,
            samples: points,
        });
    }

    let recent_skips = read_recent_skips(conn)?;

    Ok(CalibrationOverview {
        families,
        recent_skips,
        coefficient_set_hash: table.set_hash(),
    })
}

/// Scan the recent audit ledger for calibration skip events (newest first).
fn read_recent_skips(conn: &Connection) -> Result<Vec<CalibrationSkip>> {
    let entries = aberp_audit_ledger::recent_entries(conn, SKIP_SCAN_WINDOW)
        .context("read recent audit entries for calibration skips")?;
    let mut out = Vec::new();
    for entry in entries.into_iter().rev() {
        if entry.kind != EventKind::QuoteCalibrationSampleSkipped {
            continue;
        }
        if let Ok(payload) =
            serde_json::from_slice::<QuoteCalibrationSampleSkippedPayload>(&entry.payload)
        {
            out.push(CalibrationSkip {
                at_utc: entry
                    .time_wall
                    .format(&Rfc3339)
                    .unwrap_or_else(|_| String::new()),
                quote_id: payload.quote_id,
                work_order_id: payload.work_order_id,
                reason: payload.reason,
            });
        }
        if out.len() >= OVERVIEW_SKIPS {
            break;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        // The overview reads the audit ledger for the skip list; ensure both
        // schemas (production always has the audit table at boot).
        aberp_audit_ledger::ensure_schema(&conn).unwrap();
        ensure_schema(&conn).unwrap();
        conn
    }

    #[test]
    fn insert_and_load_round_trip_in_order() {
        let conn = mem();
        let t0 = OffsetDateTime::from_unix_timestamp(1_000_000).unwrap();
        let t1 = OffsetDateTime::from_unix_timestamp(2_000_000).unwrap();
        insert_sample(
            &conn,
            "T",
            "q1",
            MachineFamily::ThreeAxisMill,
            10.0,
            12.0,
            t1,
        )
        .unwrap();
        insert_sample(
            &conn,
            "T",
            "q0",
            MachineFamily::ThreeAxisMill,
            10.0,
            8.0,
            t0,
        )
        .unwrap();
        let samples = load_samples(&conn, "T").unwrap();
        // Oldest (t0, actual 8) first.
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0].actual_minutes, 8.0);
        assert_eq!(samples[1].actual_minutes, 12.0);
    }

    #[test]
    fn materialize_table_isolates_families() {
        let conn = mem();
        let now = OffsetDateTime::from_unix_timestamp(1_000_000).unwrap();
        for i in 0..6 {
            let t = now + time::Duration::seconds(i);
            insert_sample(&conn, "T", "q", MachineFamily::ThreeAxisMill, 10.0, 18.0, t).unwrap();
        }
        let table = materialize_table(&conn, "T").unwrap();
        assert!((table.coefficient(MachineFamily::ThreeAxisMill) - 1.8).abs() < 1e-9);
        // No samples → default.
        assert_eq!(table.coefficient(MachineFamily::Lathe), 1.0);
    }

    #[test]
    fn overview_groups_by_family() {
        let conn = mem();
        let now = OffsetDateTime::from_unix_timestamp(1_000_000).unwrap();
        for i in 0..5 {
            let t = now + time::Duration::seconds(i);
            insert_sample(&conn, "T", "q", MachineFamily::FiveAxisMill, 10.0, 11.0, t).unwrap();
        }
        let ov = calibration_overview(&conn, "T").unwrap();
        assert_eq!(ov.families.len(), 1);
        assert_eq!(ov.families[0].machine_family, "5-axis-mill");
        assert_eq!(ov.families[0].sample_count, 5);
        assert!(!ov.coefficient_set_hash.is_empty());
    }

    // ── File-DB hook + customer-journey e2e ───────────────────────────

    use aberp_audit_ledger::{BinaryHash, TenantId};
    use aberp_quote_engine::QuoteBreakdown;
    use std::path::PathBuf;

    const TT: &str = "T";

    /// Per-test scratch dir (no tempfile dep — mirrors the S325 convention).
    struct Scratch {
        root: PathBuf,
    }
    impl Scratch {
        fn new() -> Self {
            let mut root = std::env::temp_dir();
            root.push(format!("aberp-s429-{}", Ulid::new()));
            std::fs::create_dir_all(&root).unwrap();
            Self { root }
        }
        fn db(&self) -> PathBuf {
            self.root.join("aberp.duckdb")
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn setup_file_db(db: &std::path::Path) {
        let conn = Connection::open(db).unwrap();
        aberp_audit_ledger::ensure_schema(&conn).unwrap();
        crate::quote_pricing_jobs::ensure_schema(&conn).unwrap();
        ensure_schema(&conn).unwrap();
    }

    /// Seed a priced quote row whose breakdown carries `machining_minutes`
    /// (per part), `quantity`, and a neutral coefficient — so
    /// `read_quote_estimate` recovers `machining_minutes * quantity` as base.
    fn seed_priced_quote(db: &std::path::Path, quote_id: &str, mins_per_part: f64, qty: i64) {
        let conn = Connection::open(db).unwrap();
        let bd = QuoteBreakdown {
            gear_cost: 0.0,
            material_cost: 0.0,
            machining_cost: 0.0,
            cad_cam_cost: 0.0,
            setup_cost: 0.0,
            overhead: 0.0,
            margin: 0.0,
            total_price: 21.0,
            machining_minutes: mins_per_part,
            inspection_minutes: 0.0,
            route_to_5_axis: false,
            calibration_coefficient: 1.0,
            engine_version: "test".to_string(),
            reasoning_log: vec![],
        };
        let json = serde_json::to_string(&bd).unwrap();
        conn.execute(
            "INSERT INTO quote_pricing_jobs (
                quote_id, tenant_id, state, fetched_at, updated_at,
                customer_email, customer_name, material_grade, quantity,
                cad_filename, cad_local_path, feature_graph_hash,
                feature_graph_json, breakdown_json, pdf_path,
                total_price_eur, valid_until_iso, attempt_n
             ) VALUES (?1, ?2, 'Posted', 'now', 'now',
                'c@x.com', 'Cust', 'AL_6061_T6', ?3,
                'part.stl', '/tmp/part.stl', 'blake3:abc',
                '{}', ?4, '/tmp/p.pdf', 21.0, '2026-07-06', 0)",
            params![quote_id, TT, qty, &json],
        )
        .unwrap();
    }

    fn audit_count(db: &std::path::Path, kind: &str) -> i64 {
        let conn = Connection::open(db).unwrap();
        conn.query_row(
            "SELECT count(*) FROM audit_ledger WHERE kind = ?1",
            params![kind],
            |r| r.get(0),
        )
        .unwrap()
    }

    fn sample_count(db: &std::path::Path) -> i64 {
        let conn = Connection::open(db).unwrap();
        conn.query_row(
            "SELECT count(*) FROM quote_calibration_samples WHERE tenant_id = ?1",
            params![TT],
            |r| r.get(0),
        )
        .unwrap()
    }

    fn tenant() -> TenantId {
        TenantId::new(TT).unwrap()
    }

    #[test]
    fn hook_records_sample_when_actual_present() {
        let s = Scratch::new();
        setup_file_db(&s.db());
        seed_priced_quote(&s.db(), "q1", 10.0, 4); // base = 40 min
        record_calibration_for_completed_wo(
            &s.db(),
            &tenant(),
            BinaryHash::from_bytes([0u8; 32]),
            "op",
            "wo1",
            Some("q1"),
            Some(48.0),
        )
        .unwrap();
        assert_eq!(sample_count(&s.db()), 1);
        assert_eq!(audit_count(&s.db(), "quote.calibration_sample_recorded"), 1);
        assert_eq!(audit_count(&s.db(), "quote.calibration_sample_skipped"), 0);
        // The stored sample is base=40, actual=48.
        let samples = load_samples(&Connection::open(s.db()).unwrap(), TT).unwrap();
        assert_eq!(samples[0].estimated_minutes, 40.0);
        assert_eq!(samples[0].actual_minutes, 48.0);
    }

    #[test]
    fn hook_skips_when_no_actual() {
        let s = Scratch::new();
        setup_file_db(&s.db());
        seed_priced_quote(&s.db(), "q1", 10.0, 4);
        record_calibration_for_completed_wo(
            &s.db(),
            &tenant(),
            BinaryHash::from_bytes([0u8; 32]),
            "op",
            "wo1",
            Some("q1"),
            None,
        )
        .unwrap();
        assert_eq!(sample_count(&s.db()), 0);
        assert_eq!(audit_count(&s.db(), "quote.calibration_sample_skipped"), 1);
        assert_eq!(audit_count(&s.db(), "quote.calibration_sample_recorded"), 0);
    }

    #[test]
    fn hook_does_nothing_when_not_linked() {
        let s = Scratch::new();
        setup_file_db(&s.db());
        record_calibration_for_completed_wo(
            &s.db(),
            &tenant(),
            BinaryHash::from_bytes([0u8; 32]),
            "op",
            "wo1",
            None, // not linked to a quote
            Some(48.0),
        )
        .unwrap();
        assert_eq!(sample_count(&s.db()), 0);
        assert_eq!(audit_count(&s.db(), "quote.calibration_sample_skipped"), 0);
        assert_eq!(audit_count(&s.db(), "quote.calibration_sample_recorded"), 0);
    }

    /// [[customer-journey-e2e-gate]] — WO-close → sample → coefficient-shift →
    /// next quote uses the new coefficient. Five Completes (each ratio 1.2)
    /// move the 3-axis coefficient from the default 1.0 to 1.2; the 5th close
    /// crosses the MIN_SAMPLES threshold and emits exactly one shift event; the
    /// rebuilt table (the one the pipeline feeds the engine) now carries 1.2.
    #[test]
    fn e2e_closes_shift_coefficient_for_next_quote() {
        let s = Scratch::new();
        setup_file_db(&s.db());
        seed_priced_quote(&s.db(), "q1", 10.0, 4); // base = 40 min

        for i in 0..5 {
            record_calibration_for_completed_wo(
                &s.db(),
                &tenant(),
                BinaryHash::from_bytes([0u8; 32]),
                "op",
                &format!("wo{i}"),
                Some("q1"),
                Some(48.0), // ratio 48/40 = 1.2
            )
            .unwrap();
        }

        assert_eq!(sample_count(&s.db()), 5);
        assert_eq!(audit_count(&s.db(), "quote.calibration_sample_recorded"), 5);
        // The shift fires once — when sample 5 crosses MIN_SAMPLES (1.0 → 1.2).
        assert_eq!(
            audit_count(&s.db(), "quote.calibration_coefficient_shifted"),
            1
        );

        // The table the next quote-create feeds the engine now carries 1.2.
        let conn = Connection::open(s.db()).unwrap();
        let table = materialize_table(&conn, TT).unwrap();
        assert!(
            (table.coefficient(MachineFamily::ThreeAxisMill) - 1.2).abs() < 1e-9,
            "coefficient should have shifted to 1.2, got {}",
            table.coefficient(MachineFamily::ThreeAxisMill)
        );
        // Engine application of this table is proven in
        // aberp-quote-engine/tests/calibration.rs (quote_with_calibration).
    }
}
