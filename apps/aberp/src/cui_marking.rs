//! D-08 (S360 / ADR-0077) — CUI marking + access-event trail for an artifact.
//!
//! Stores a typed [`aberp_compliance::cui::CuiMarking`] (+ its limited-
//! dissemination controls) against an artifact, renders the DoD banner through
//! [`CuiMarking::to_banner_str`] (so a free-text banner can never reach the
//! ledger), and fires the two `cui.*` audit kinds:
//!
//! * [`EventKind::CuiMarkingApplied`] when a marking is applied, and
//! * [`EventKind::CuiAccessEvent`] on every read of a marked artifact — CUI's
//!   "lawful government purpose" basic-handling rule (32 CFR § 2002.4) makes
//!   the access trail load-bearing, so a GRANT is recorded, not only a denial.
//!
//! Scope decision (D-08, this slice): the artifact is a **product** (the one
//! existing entity with a stable read route). Access CONTROL is a recorded
//! GRANT — the Defense pilot is single-operator-per-tenant and there is no
//! clearance/role model yet, so a deny path has no input to branch on. That
//! path is DEFERRED (flagged in the backlog) pending a role model; the append
//! rides the caller's tx on the shared `aberp_db::Handle` (ADR-0099).
//!
//! No controlled content at rest: the row + payloads record WHICH artifact
//! carries WHICH banner and WHO touched it (opaque operator id), never the
//! controlled content itself.

use anyhow::{Context, Result};
use duckdb::{params, Connection, Transaction};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use ulid::Ulid;

use aberp_audit_ledger::{append_in_tx, Actor, EventKind, LedgerMeta};
use aberp_compliance::cui::{CuiCategory, CuiMarking, DisseminationControl};

/// The entity kind this slice marks. Free-text in the payload per the S360
/// schema; a single constant keeps the value consistent across the row, the
/// two audit payloads, and the read path.
pub const PRODUCT_ENTITY_KIND: &str = "product";

pub const CUI_MARKINGS_SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS cui_markings (
    tenant_id VARCHAR NOT NULL,
    entity_kind VARCHAR NOT NULL,
    entity_id VARCHAR NOT NULL,
    band VARCHAR NOT NULL,
    category VARCHAR,
    dissemination VARCHAR NOT NULL,
    banner_str VARCHAR NOT NULL,
    applied_by_operator VARCHAR NOT NULL,
    applied_at_utc VARCHAR NOT NULL,
    PRIMARY KEY (tenant_id, entity_kind, entity_id)
);
";

/// Operator intake for applying a marking. `band` is the classification band
/// (`unclassified` / `cui` / `confidential` / `secret` / `top_secret`);
/// `category` is required iff `band == "cui"` and rejected otherwise.
#[derive(Debug, Clone, Deserialize)]
pub struct CuiMarkingInput {
    pub band: String,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub dissemination: Vec<String>,
}

/// The stored marking, echoed back to the operator. `banner_str` is the
/// authoritative rendered DoD banner (e.g. `"CUI//SP-CTI//NOFORN"`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CuiMarkingRecord {
    pub entity_kind: String,
    pub entity_id: String,
    pub band: String,
    pub category: Option<String>,
    pub dissemination: Vec<String>,
    pub banner_str: String,
    pub applied_by_operator: String,
    pub applied_at_utc: String,
}

/// Apply-time rejections — all operator-fixable, so the serve layer maps every
/// variant to 400.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum CuiMarkingError {
    #[error("unknown band {value:?} (expected unclassified/cui/confidential/secret/top_secret)")]
    UnknownBand { value: String },
    #[error("unknown CUI category {value:?}")]
    UnknownCategory { value: String },
    #[error("band \"cui\" requires a category")]
    CategoryRequiredForCui,
    #[error("category is only valid for band \"cui\" (band {band:?} carries none)")]
    CategoryNotAllowedForBand { band: String },
    #[error("unknown dissemination control {value:?} (expected noforn/fedcon/nocon/dl_only)")]
    UnknownDissemination { value: String },
}

/// Access decision recorded on a read. Only `Granted` is reachable today (no
/// clearance model — see the module doc); `Denied` is modelled so the payload
/// contract is stable when a role model lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessDecision {
    Granted,
    Denied,
}

impl AccessDecision {
    pub fn as_str(self) -> &'static str {
        match self {
            AccessDecision::Granted => "granted",
            AccessDecision::Denied => "denied",
        }
    }
}

fn parse_category(s: &str) -> std::result::Result<CuiCategory, CuiMarkingError> {
    match s.trim().to_ascii_lowercase().as_str() {
        "cti" => Ok(CuiCategory::Cti),
        "prvcy" => Ok(CuiCategory::Prvcy),
        "expt" => Ok(CuiCategory::Expt),
        "crit" => Ok(CuiCategory::Crit),
        "lei" => Ok(CuiCategory::Lei),
        "ifg" => Ok(CuiCategory::Ifg),
        "inf" => Ok(CuiCategory::Inf),
        "isvi" => Ok(CuiCategory::Isvi),
        "proc" => Ok(CuiCategory::Proc),
        "prop" => Ok(CuiCategory::Prop),
        _ => Err(CuiMarkingError::UnknownCategory {
            value: s.to_string(),
        }),
    }
}

/// Canonical lower-case token for a category (round-trips through
/// `parse_category`).
fn category_token(cat: CuiCategory) -> String {
    cat.abbreviation().to_ascii_lowercase()
}

fn parse_dissemination(s: &str) -> std::result::Result<DisseminationControl, CuiMarkingError> {
    match s.trim().to_ascii_lowercase().as_str() {
        "noforn" => Ok(DisseminationControl::NoForn),
        "fedcon" => Ok(DisseminationControl::FedCon),
        "nocon" => Ok(DisseminationControl::NoCon),
        // "DL ONLY" renders with a space; the operator token is `dl_only`.
        "dl_only" | "dlonly" => Ok(DisseminationControl::DlOnly),
        _ => Err(CuiMarkingError::UnknownDissemination {
            value: s.to_string(),
        }),
    }
}

fn dissemination_token(d: DisseminationControl) -> &'static str {
    match d {
        DisseminationControl::NoForn => "noforn",
        DisseminationControl::FedCon => "fedcon",
        DisseminationControl::NoCon => "nocon",
        DisseminationControl::DlOnly => "dl_only",
    }
}

/// Validate an intake into a `(CuiMarking, dissemination)` pair.
fn build_marking(
    input: &CuiMarkingInput,
) -> std::result::Result<(CuiMarking, Vec<DisseminationControl>), CuiMarkingError> {
    let band = input.band.trim().to_ascii_lowercase();
    let category_present = input
        .category
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let marking = match band.as_str() {
        "cui" => {
            let cat = category_present.ok_or(CuiMarkingError::CategoryRequiredForCui)?;
            CuiMarking::Cui(parse_category(cat)?)
        }
        "unclassified" => {
            if category_present.is_some() {
                return Err(CuiMarkingError::CategoryNotAllowedForBand { band });
            }
            CuiMarking::Unclassified
        }
        "confidential" | "secret" | "top_secret" => {
            if category_present.is_some() {
                return Err(CuiMarkingError::CategoryNotAllowedForBand { band });
            }
            match band.as_str() {
                "confidential" => CuiMarking::Confidential,
                "secret" => CuiMarking::Secret,
                _ => CuiMarking::TopSecret,
            }
        }
        _ => {
            return Err(CuiMarkingError::UnknownBand {
                value: input.band.clone(),
            })
        }
    };

    let mut dissemination = Vec::with_capacity(input.dissemination.len());
    for raw in &input.dissemination {
        let d = parse_dissemination(raw)?;
        if !dissemination.contains(&d) {
            dissemination.push(d);
        }
    }
    Ok((marking, dissemination))
}

impl CuiMarkingRecord {
    /// Validate + render an intake into a storable, ledger-ready record.
    pub fn from_input(
        input: &CuiMarkingInput,
        entity_id: &str,
        operator: &str,
        now: OffsetDateTime,
    ) -> std::result::Result<Self, CuiMarkingError> {
        let (marking, dissemination) = build_marking(input)?;
        let banner_str = marking.to_banner_str(&dissemination);
        let (band, category) = match marking {
            CuiMarking::Unclassified => ("unclassified".to_string(), None),
            CuiMarking::Cui(cat) => ("cui".to_string(), Some(category_token(cat))),
            CuiMarking::Confidential => ("confidential".to_string(), None),
            CuiMarking::Secret => ("secret".to_string(), None),
            CuiMarking::TopSecret => ("top_secret".to_string(), None),
        };
        let applied_at_utc = now
            .format(&Rfc3339)
            .expect("Rfc3339 format of OffsetDateTime is infallible");
        Ok(Self {
            entity_kind: PRODUCT_ENTITY_KIND.to_string(),
            entity_id: entity_id.to_string(),
            band,
            category,
            dissemination: dissemination
                .iter()
                .map(|d| dissemination_token(*d).to_string())
                .collect(),
            banner_str,
            applied_by_operator: operator.to_string(),
            applied_at_utc,
        })
    }
}

/// Upsert the marking row (one marking per artifact; re-applying replaces).
pub fn store_marking_in_tx(
    tx: &Transaction<'_>,
    tenant: &str,
    record: &CuiMarkingRecord,
) -> Result<()> {
    tx.execute_batch(CUI_MARKINGS_SCHEMA_SQL)
        .context("ensure cui_markings schema")?;
    tx.execute(
        "INSERT INTO cui_markings (
            tenant_id, entity_kind, entity_id, band, category,
            dissemination, banner_str, applied_by_operator, applied_at_utc
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT (tenant_id, entity_kind, entity_id) DO UPDATE SET
            band = excluded.band,
            category = excluded.category,
            dissemination = excluded.dissemination,
            banner_str = excluded.banner_str,
            applied_by_operator = excluded.applied_by_operator,
            applied_at_utc = excluded.applied_at_utc",
        params![
            tenant,
            record.entity_kind,
            record.entity_id,
            record.band,
            record.category,
            record.dissemination.join(","),
            record.banner_str,
            record.applied_by_operator,
            record.applied_at_utc,
        ],
    )
    .context("upsert cui_markings row")?;
    Ok(())
}

/// Read the marking on `entity_id` (a product), if any.
pub fn read_marking(
    conn: &Connection,
    tenant: &str,
    entity_id: &str,
) -> Result<Option<CuiMarkingRecord>> {
    if !aberp_audit_ledger::connection_is_read_only(conn) {
        conn.execute_batch(CUI_MARKINGS_SCHEMA_SQL)
            .context("ensure cui_markings schema for read")?;
    }
    let mut stmt = conn
        .prepare(
            "SELECT entity_kind, entity_id, band, category, dissemination,
                    banner_str, applied_by_operator, applied_at_utc
               FROM cui_markings
              WHERE tenant_id = ?1 AND entity_kind = ?2 AND entity_id = ?3",
        )
        .context("prepare read_marking")?;
    let mut rows = stmt
        .query_map(params![tenant, PRODUCT_ENTITY_KIND, entity_id], |row| {
            let dissem: String = row.get(4)?;
            Ok(CuiMarkingRecord {
                entity_kind: row.get(0)?,
                entity_id: row.get(1)?,
                band: row.get(2)?,
                category: row.get(3)?,
                dissemination: if dissem.is_empty() {
                    Vec::new()
                } else {
                    dissem.split(',').map(str::to_string).collect()
                },
                banner_str: row.get(5)?,
                applied_by_operator: row.get(6)?,
                applied_at_utc: row.get(7)?,
            })
        })
        .context("query read_marking")?;
    match rows.next() {
        Some(r) => Ok(Some(r.context("row read_marking")?)),
        None => Ok(None),
    }
}

/// Append `cui.marking_applied` for `record` inside the caller's tx.
pub fn append_cui_marking_applied_in_tx(
    tx: &Transaction<'_>,
    ledger_meta: &LedgerMeta,
    ledger_actor: Actor,
    record: &CuiMarkingRecord,
    applied_at_ms: i64,
) -> Result<()> {
    let payload = serde_json::json!({
        "entity_kind": record.entity_kind,
        "entity_id": record.entity_id,
        "cui_marking_str": record.banner_str,
        "operator_user_id": record.applied_by_operator,
        "applied_at_ms": applied_at_ms,
    });
    append_in_tx(
        tx,
        ledger_meta,
        EventKind::CuiMarkingApplied,
        serde_json::to_vec(&payload).expect("serialize cui.marking_applied"),
        ledger_actor,
        Some(format!(
            "cui_marking:{}:{}",
            record.entity_kind, record.entity_id
        )),
    )
    .context("audit append CuiMarkingApplied")?;
    Ok(())
}

/// Append `cui.access_event` for a read of a marked artifact inside the tx.
#[allow(clippy::too_many_arguments)]
pub fn append_cui_access_event_in_tx(
    tx: &Transaction<'_>,
    ledger_meta: &LedgerMeta,
    ledger_actor: Actor,
    entity_id: &str,
    operator: &str,
    decision: AccessDecision,
    reason: &str,
    accessed_at_ms: i64,
) -> Result<()> {
    let payload = serde_json::json!({
        "entity_kind": PRODUCT_ENTITY_KIND,
        "entity_id": entity_id,
        "operator_user_id": operator,
        "decision": decision.as_str(),
        "reason": reason,
        "accessed_at_ms": accessed_at_ms,
    });
    append_in_tx(
        tx,
        ledger_meta,
        EventKind::CuiAccessEvent,
        serde_json::to_vec(&payload).expect("serialize cui.access_event"),
        ledger_actor,
        // Every access is its own row — a ULID suffix keeps the business key
        // unique (unlike the marking key, which is one-per-artifact).
        Some(format!(
            "cui_access:{PRODUCT_ENTITY_KIND}:{entity_id}:{}",
            Ulid::new()
        )),
    )
    .context("audit append CuiAccessEvent")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(band: &str, category: Option<&str>, dissem: &[&str]) -> CuiMarkingInput {
        CuiMarkingInput {
            band: band.to_string(),
            category: category.map(str::to_string),
            dissemination: dissem.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn rec(band: &str, category: Option<&str>, dissem: &[&str]) -> CuiMarkingRecord {
        CuiMarkingRecord::from_input(
            &input(band, category, dissem),
            "prd_1",
            "op",
            OffsetDateTime::UNIX_EPOCH,
        )
        .unwrap()
    }

    #[test]
    fn cui_specified_category_renders_sp_prefix_and_dissemination() {
        // CTI is CUI Specified → SP- prefix; NOFORN is a limited-dissemination
        // control appended after `//`.
        let r = rec("cui", Some("cti"), &["noforn"]);
        assert_eq!(r.banner_str, "CUI//SP-CTI//NOFORN");
        assert_eq!(r.band, "cui");
        assert_eq!(r.category.as_deref(), Some("cti"));
        assert_eq!(r.dissemination, vec!["noforn".to_string()]);
    }

    #[test]
    fn cui_basic_category_has_no_sp_prefix() {
        // PROC is CUI Basic (not Specified) → no SP- prefix.
        let r = rec("cui", Some("proc"), &[]);
        assert_eq!(r.banner_str, "CUI//PROC");
    }

    #[test]
    fn classified_bands_render_plain_and_carry_no_category() {
        assert_eq!(rec("secret", None, &[]).banner_str, "SECRET");
        assert_eq!(rec("top_secret", None, &[]).banner_str, "TOP SECRET");
        assert_eq!(rec("confidential", None, &[]).band, "confidential");
        assert!(rec("secret", None, &[]).category.is_none());
    }

    #[test]
    fn unclassified_renders_plain() {
        assert_eq!(rec("unclassified", None, &[]).banner_str, "UNCLASSIFIED");
    }

    #[test]
    fn multiple_dissemination_controls_join_and_dedupe() {
        let r = rec("cui", Some("expt"), &["noforn", "fedcon", "noforn"]);
        // EXPT is Specified → SP-; controls joined by `/`, dupes dropped.
        assert_eq!(r.banner_str, "CUI//SP-EXPT//NOFORN/FEDCON");
        assert_eq!(
            r.dissemination,
            vec!["noforn".to_string(), "fedcon".to_string()]
        );
    }

    #[test]
    fn band_is_case_insensitive() {
        assert_eq!(
            rec("CUI", Some("CTI"), &["NOFORN"]).banner_str,
            "CUI//SP-CTI//NOFORN"
        );
    }

    #[test]
    fn cui_without_category_is_rejected() {
        assert_eq!(
            CuiMarkingRecord::from_input(
                &input("cui", None, &[]),
                "p",
                "o",
                OffsetDateTime::UNIX_EPOCH
            ),
            Err(CuiMarkingError::CategoryRequiredForCui)
        );
    }

    #[test]
    fn category_on_non_cui_band_is_rejected() {
        assert_eq!(
            CuiMarkingRecord::from_input(
                &input("secret", Some("cti"), &[]),
                "p",
                "o",
                OffsetDateTime::UNIX_EPOCH
            ),
            Err(CuiMarkingError::CategoryNotAllowedForBand {
                band: "secret".to_string()
            })
        );
    }

    #[test]
    fn unknown_band_category_and_dissemination_are_rejected() {
        assert!(matches!(
            CuiMarkingRecord::from_input(
                &input("mystery", None, &[]),
                "p",
                "o",
                OffsetDateTime::UNIX_EPOCH
            ),
            Err(CuiMarkingError::UnknownBand { .. })
        ));
        assert!(matches!(
            CuiMarkingRecord::from_input(
                &input("cui", Some("zzz"), &[]),
                "p",
                "o",
                OffsetDateTime::UNIX_EPOCH
            ),
            Err(CuiMarkingError::UnknownCategory { .. })
        ));
        assert!(matches!(
            CuiMarkingRecord::from_input(
                &input("cui", Some("cti"), &["telepathic"]),
                "p",
                "o",
                OffsetDateTime::UNIX_EPOCH
            ),
            Err(CuiMarkingError::UnknownDissemination { .. })
        ));
    }
}
