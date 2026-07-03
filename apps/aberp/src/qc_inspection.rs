//! S443 / ADR-0092 — QC inspection app orchestrator.
//!
//! The glue between the domain core in `aberp-qa::qc` (verdict + row +
//! the five inspection audit events) and the S439 NCR pipeline in
//! [`crate::quality`]. aberp-qa cannot depend on the app layer, so the
//! auto-NCR step lives here:
//!
//! 1. Open a tx, call `aberp_qa::record_inspection` (verdict computed in
//!    code; row + `QcInspectionRecorded`/`Passed`/`Failed`/stale-warning
//!    ride one commit), commit.
//! 2. On a failing verdict (Minor/Major/Critical) call
//!    [`crate::quality::create_ncr`] (Workmanship, severity = tier), then
//!    link it back onto the row + emit `QcAutoNcrCreated`.
//!
//! A `CalibrationStale` verdict records the row + a warning, NO NCR (a
//! probe that may be lying must not manufacture a false defect — ISO 9001
//! §7.1.5.2). The resulting Open NCR engages the existing S438/S439
//! Refuse-Shipment gate unchanged.
//!
//! Manual operator entry works TODAY (probe sources are `todo!()`-stubbed
//! — ADR-0092 §Decision). When a real `ProbeIngestionSource` lands it
//! feeds this same path with `QcSource::Probe`.

use std::path::Path;

use aberp_audit_ledger::{Actor, BinaryHash, LedgerMeta, TenantId};
use aberp_inventory::ActorKind;
use duckdb::Connection;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use ulid::Ulid;

use aberp_qa::{
    get_inspection_plan, link_auto_ncr, record_inspection, QcError, QcInspection, QcSource,
    QcWriteContext, RecordInspectionInputs, Verdict,
};

/// Operator/probe-supplied inputs for one inspection. Linkage fields are
/// resolved by the route layer (which holds the WO/part/heat-lot context).
#[derive(Debug, Clone)]
pub struct ManualInspectionRequest {
    pub plan_id: String,
    pub actual_value: f64,
    pub source: QcSource,
    /// The measurement's units. `None` → derive from the plan (the manual
    /// UI auto-fills it, so it always matches).
    pub units: Option<String>,
    pub source_event_id: Option<String>,
    pub probe_serial: Option<String>,
    /// RFC3339 UTC. `None` → not a calibrated probe (skip the stale check).
    pub last_calibration_at: Option<String>,
    pub wo_id: Option<String>,
    pub part_uid: Option<String>,
    pub heat_lot: Option<String>,
}

/// Result of a recorded inspection: the row (with `auto_ncr_id` set if an
/// NCR was spawned) + the NCR itself when one was created.
#[derive(Debug, Clone)]
pub struct InspectionResult {
    pub inspection: QcInspection,
    pub auto_ncr: Option<crate::quality::Ncr>,
}

#[derive(Debug, thiserror::Error)]
pub enum QcRecordError {
    #[error("inspection plan not found")]
    PlanNotFound,
    #[error("{0}")]
    Validation(String),
    #[error("invalid calibration timestamp {0:?}: must be RFC3339")]
    BadCalibrationTimestamp(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl From<QcError> for QcRecordError {
    fn from(e: QcError) -> Self {
        match e {
            QcError::NotFound => QcRecordError::PlanNotFound,
            QcError::Validation(m) => QcRecordError::Validation(m),
            QcError::UnitsMismatch { .. } => QcRecordError::Validation(e.to_string()),
            QcError::Storage(err) => QcRecordError::Other(err),
        }
    }
}

fn map_quality_err(e: crate::quality::QualityError) -> QcRecordError {
    match e {
        crate::quality::QualityError::Invalid(m) => QcRecordError::Validation(m),
        other => QcRecordError::Other(anyhow::anyhow!("auto-NCR failed: {other}")),
    }
}

/// Map a failing verdict to the NCR severity tier. `Pass` /
/// `CalibrationStale` never reach here (no NCR).
fn severity_for(verdict: Verdict) -> Option<crate::quality::NcrSeverity> {
    match verdict {
        Verdict::Minor => Some(crate::quality::NcrSeverity::Minor),
        Verdict::Major => Some(crate::quality::NcrSeverity::Major),
        Verdict::Critical => Some(crate::quality::NcrSeverity::Critical),
        Verdict::Pass | Verdict::CalibrationStale => None,
    }
}

/// Record one inspection end-to-end. `now` and `stale_window_seconds` are
/// supplied by the caller so the verdict is deterministic (the route
/// passes `OffsetDateTime::now_utc()` + the tenant's configured window).
pub fn record_manual_inspection(
    db_path: &Path,
    tenant: TenantId,
    binary_hash: BinaryHash,
    operator: &str,
    now: OffsetDateTime,
    stale_window_seconds: u64,
    req: ManualInspectionRequest,
) -> Result<InspectionResult, QcRecordError> {
    // DuckDB allows ONE read-write handle per file. `create_ncr` opens its
    // own connection, so the inspection-record + link phases each open AND
    // DROP their connection in a scope — never holding two handles to the
    // same file at once (the S427 "scope the write conn" lesson).
    let last_calibration_at = match req.last_calibration_at.as_deref() {
        Some(s) => Some(
            OffsetDateTime::parse(s.trim(), &Rfc3339)
                .map_err(|_| QcRecordError::BadCalibrationTimestamp(s.to_string()))?,
        ),
        None => None,
    };
    let session_id = Ulid::new().to_string();

    // ── Phase 1: record the row + verdict events (own connection) ──
    let (recorded, plan) = {
        let mut conn = Connection::open(db_path)
            .map_err(|e| QcRecordError::Other(anyhow::anyhow!("open DuckDB: {e}")))?;
        conn.execute_batch("PRAGMA disable_checkpoint_on_shutdown;")
            .map_err(|e| {
                QcRecordError::Other(anyhow::anyhow!(
                    "PRAGMA disable_checkpoint_on_shutdown on residual opener (ADR-0098 R3): {e}"
                ))
            })?;
        let plan = get_inspection_plan(&conn, tenant.as_str(), &req.plan_id)?
            .ok_or(QcRecordError::PlanNotFound)?;
        if plan.archived_at.is_some() {
            return Err(QcRecordError::Validation(
                "inspection plan is archived".into(),
            ));
        }
        let units = req.units.clone().unwrap_or_else(|| plan.units.clone());
        let ledger_meta = LedgerMeta::new(tenant.clone(), binary_hash);
        let ctx = QcWriteContext {
            tenant: tenant.as_str(),
            actor: ActorKind::SpaOperator {
                operator_login: operator.to_string(),
            },
            ledger_meta: &ledger_meta,
            ledger_actor: Actor::from_local_cli(session_id.clone(), operator),
        };
        let tx = conn
            .transaction()
            .map_err(|e| QcRecordError::Other(anyhow::anyhow!("begin inspection tx: {e}")))?;
        let recorded = record_inspection(
            &tx,
            &ctx,
            RecordInspectionInputs {
                plan: &plan,
                source: req.source,
                source_event_id: req.source_event_id.clone(),
                actual_value: req.actual_value,
                units,
                probe_serial: req.probe_serial.clone(),
                last_calibration_at,
                measured_at: now,
                current_time: now,
                stale_window_seconds,
                linked_part_uid: req.part_uid.clone(),
                linked_heat_lot: req.heat_lot.clone(),
                linked_wo_id: req.wo_id.clone(),
                recorded_by: operator.to_string(),
            },
        )?;
        tx.commit()
            .map_err(|e| QcRecordError::Other(anyhow::anyhow!("commit inspection tx: {e}")))?;
        (recorded, plan)
    };

    let mut inspection = recorded.inspection;

    // ── Phase 2: auto-NCR on a failing verdict (mirrors S440 receiving) ──
    let auto_ncr = if let Some(severity) = severity_for(recorded.verdict) {
        let band = format!("[{}, {}]", plan.lower_tol, plan.upper_tol);
        let description = format!(
            "Inspection failed: feature {feature} measured {actual} {units} \
             (nominal {nominal}, tolerance band {band}). Verdict: {tier}. \
             Inspection ID: {qci}.",
            feature = plan.feature_name.trim(),
            actual = inspection.actual_value,
            units = inspection.units,
            nominal = plan.nominal_value,
            band = band,
            tier = recorded.verdict.as_str(),
            qci = inspection.qci_id,
        );
        let ncr = crate::quality::create_ncr(
            db_path,
            tenant.clone(),
            binary_hash,
            operator,
            crate::quality::NewNcr {
                severity,
                category: crate::quality::NcrCategory::Workmanship,
                description,
                affected_part_uids: req.part_uid.clone().into_iter().collect(),
                affected_wo_ids: req.wo_id.clone().into_iter().collect(),
                affected_heat_lots: req.heat_lot.clone().into_iter().collect(),
                photos: vec![],
            },
        )
        .map_err(map_quality_err)?;

        // Link the NCR back onto the inspection row + emit QcAutoNcrCreated
        // (own connection, opened only after create_ncr's handle is gone).
        {
            let mut conn = Connection::open(db_path)
                .map_err(|e| QcRecordError::Other(anyhow::anyhow!("open DuckDB for link: {e}")))?;
            conn.execute_batch("PRAGMA disable_checkpoint_on_shutdown;")
                .map_err(|e| QcRecordError::Other(anyhow::anyhow!("PRAGMA disable_checkpoint_on_shutdown on residual opener (ADR-0098 R3): {e}")))?;
            let link_meta = LedgerMeta::new(tenant.clone(), binary_hash);
            let link_ctx = QcWriteContext {
                tenant: tenant.as_str(),
                actor: ActorKind::SpaOperator {
                    operator_login: operator.to_string(),
                },
                ledger_meta: &link_meta,
                ledger_actor: Actor::from_local_cli(session_id.clone(), operator),
            };
            let tx2 = conn
                .transaction()
                .map_err(|e| QcRecordError::Other(anyhow::anyhow!("begin link tx: {e}")))?;
            link_auto_ncr(
                &tx2,
                &link_ctx,
                &inspection.qci_id,
                &ncr.ncr_id,
                recorded.verdict,
            )?;
            tx2.commit()
                .map_err(|e| QcRecordError::Other(anyhow::anyhow!("commit link tx: {e}")))?;
        }
        inspection.auto_ncr_id = Some(ncr.ncr_id.clone());
        Some(ncr)
    } else {
        None
    };

    Ok(InspectionResult {
        inspection,
        auto_ncr,
    })
}
