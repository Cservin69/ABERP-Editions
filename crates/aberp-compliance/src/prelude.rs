//! Common re-exports for downstream consumers.
//!
//! `use aberp_compliance::prelude::*;` brings the public types and traits of
//! every module into scope in one line. The `nist_800_171` control constants
//! are intentionally NOT glob-exported here — there are 110 of them and they
//! are referenced by their fully-qualified path at the tagging site.
//!
//! # Identity-key canonicalization
//!
//! Every compliance audit payload that records *who acted* uses the single key
//! **`operator_user_id`** — the Bearer-subject operator login, the convention
//! already established by the two firing families that exist today (S350
//! `QuotePricingMaterialEdited`, S354 `QuotePricingOperatorAccepted` in
//! `apps/aberp::serve`). S366 review F1 found ten different spellings across
//! ADR-0073…0079 (`operator_id`, `signed_by_operator_id`,
//! `marked_by_operator_id`, …); S367 collapsed them all to `operator_user_id`
//! before any compliance firing site landed. New EventKinds in this domain
//! MUST reuse `operator_user_id` for the acting operator — do not coin a new
//! `*_by_operator_id` variant. (The distinct authorizing-party field on the
//! access-grant kinds, `granted_by`, is a *different concept* — the supervisor
//! who authorized, not the acting subject — and is deliberately left as-is.)

pub use crate::avl::{
    ApprovedSupplierEntry, DpasPriority, DpasRating, ExportScreeningStatus, PartnerRef,
    ProgramSymbolError, QualLevel,
};
pub use crate::cui::{CuiCategory, CuiMarking, DisseminationControl};
pub use crate::export_control::{
    validate_eccn, Classifiable, EccnError, ExportClassification, ExportControlError,
    ExportControlProvider, Jurisdiction, MockExportControlProvider, PartyRef, ScreeningResult,
};
pub use crate::incident::{
    dod_72h_report_due_at_ms, DetectionSource, IncidentSeverity, DFARS_72H_REPORT_WINDOW_MS,
};
pub use crate::lot_heat::{HeatId, LotId, MaterialTraceabilitySeed, TraceabilityError, MAX_ID_LEN};
pub use crate::uid::{
    validate_iac, Iuid, IuidConstruct1, IuidConstruct2, UidError, MAX_IAC_LEN, MAX_UID_FIELD_LEN,
};
