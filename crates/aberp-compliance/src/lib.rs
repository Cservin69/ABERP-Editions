//! ABERP defense-grade compliance subsystems (S345 / PR-39, ADR-0071).
//!
//! # Why this crate exists
//!
//! The defense-aerospace pivot (`[[defense-aerospace-pivot]]`) requires a
//! family of compliance capabilities that ABERP's commercial-ERP core does
//! not have: export-control classification + denied-party screening (ITAR /
//! EAR), CUI marking (32 CFR Part 2002 / DoD CUI Registry), lot/heat material
//! traceability (AS9100D §8.5.2), an approved-vendor list with DPAS priority
//! ratings (FAR 11.6), and the NIST SP 800-171 control set (DFARS
//! 252.204-7012). These are *separate* concerns from the audit ledger
//! (ADR-0008) and the digital-identity layer (ADR-0070 / `aberp-digital-id`),
//! so they get their own home crate rather than accreting onto either.
//!
//! # Scope (S345 — FOUNDATION ONLY)
//!
//! This session ships **types, traits, and mock backends only**. There are
//! no real screening services, no mill-cert capture wiring, no audit
//! `EventKind`s, and `aberp-compliance` is deliberately NOT yet a dependency
//! of `apps/aberp`. Each module is the swap-point boundary its real backend
//! slots in behind later (mock-first, per `[[mock-everything-principle]]`):
//!
//! - [`export_control`] — [`ExportControlProvider`] trait + classification /
//!   screening types + [`Jurisdiction`] (ITAR / EAR / EAR99 / NOT_CONTROLLED /
//!   UNKNOWN regime axis, S359) + [`MockExportControlProvider`].
//! - [`cui`] — [`CuiMarking`] / [`CuiCategory`] + marking helpers.
//! - [`lot_heat`] — validated [`LotId`] / [`HeatId`] newtypes +
//!   [`MaterialTraceabilitySeed`].
//! - [`avl`] — [`ApprovedSupplierEntry`] + [`DpasRating`] / [`QualLevel`] /
//!   [`ExportScreeningStatus`].
//! - [`nist_800_171`] — the 110 control identifiers as constants, for future
//!   audit-event tagging.
//! - [`uid`] — MIL-STD-130N IUID format types ([`Iuid`] / [`IuidConstruct1`] /
//!   [`IuidConstruct2`] + [`validate_iac`]) for DoD item unique identification.
//!
//! Out of scope (future work): real providers, audit `EventKind`s that
//! reference these types, the e-signature ceremony, the SPA surfaces.
//!
//! [`ExportControlProvider`]: export_control::ExportControlProvider
//! [`Jurisdiction`]: export_control::Jurisdiction
//! [`MockExportControlProvider`]: export_control::MockExportControlProvider
//! [`CuiMarking`]: cui::CuiMarking
//! [`CuiCategory`]: cui::CuiCategory
//! [`LotId`]: lot_heat::LotId
//! [`HeatId`]: lot_heat::HeatId
//! [`MaterialTraceabilitySeed`]: lot_heat::MaterialTraceabilitySeed
//! [`ApprovedSupplierEntry`]: avl::ApprovedSupplierEntry
//! [`DpasRating`]: avl::DpasRating
//! [`QualLevel`]: avl::QualLevel
//! [`ExportScreeningStatus`]: avl::ExportScreeningStatus
//! [`Iuid`]: uid::Iuid
//! [`IuidConstruct1`]: uid::IuidConstruct1
//! [`IuidConstruct2`]: uid::IuidConstruct2
//! [`validate_iac`]: uid::validate_iac

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod avl;
pub mod cui;
pub mod export_control;
pub mod lot_heat;
pub mod nist_800_171;
pub mod prelude;
pub mod uid;
