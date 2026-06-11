//! Common re-exports for downstream consumers.
//!
//! `use aberp_compliance::prelude::*;` brings the public types and traits of
//! every module into scope in one line. The `nist_800_171` control constants
//! are intentionally NOT glob-exported here — there are 110 of them and they
//! are referenced by their fully-qualified path at the tagging site.

pub use crate::avl::{
    ApprovedSupplierEntry, DpasRating, ExportScreeningStatus, PartnerRef, QualLevel,
};
pub use crate::cui::{CuiCategory, CuiMarking};
pub use crate::export_control::{
    Classifiable, ExportClassification, ExportControlError, ExportControlProvider, Jurisdiction,
    MockExportControlProvider, PartyRef, ScreeningResult,
};
pub use crate::lot_heat::{HeatId, LotId, MaterialTraceabilitySeed, TraceabilityError, MAX_ID_LEN};
pub use crate::uid::{
    validate_iac, Iuid, IuidConstruct1, IuidConstruct2, UidError, MAX_IAC_LEN, MAX_UID_FIELD_LEN,
};
