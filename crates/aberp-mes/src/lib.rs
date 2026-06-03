//! ABERP Stage 3 manufacturing-adapter framework — Phase α.
//!
//! Per ADR-0060 (`adr/0060-stage3-manufacturing-adapter-framework.md`).
//!
//! # What this crate does
//!
//! - Defines the **canonical event vocabulary**
//!   ([`CanonicalEvent`]) — six initial variants covering machine state,
//!   part movement, quality, scan, work-order, robot-task. Closed Rust
//!   enum; the vocabulary IS the schema. Future variants land
//!   incrementally per ADR-0060 §"The canonical event vocabulary".
//! - Defines the **[`Adapter`] trait** — minimal async surface with
//!   `name`, `start`, `stop`, `health`, `subscribe`. Vendor-specific
//!   impls live in per-vendor crates (future: `aberp-adapter-mtconnect`,
//!   `aberp-adapter-renishaw`, etc.).
//! - Ships the **[`AdapterRegistry`]** — runtime map of
//!   `Arc<dyn Adapter>` keyed by adapter `name()`. NOT persisted per
//!   `[[no-sql-specific]]` extended by the Stage 3 memo.
//! - Ships the **[`NoopAdapter`]** — reference implementation that does
//!   nothing real. Used by the framework's own tests and as a starting
//!   point for adapter authors.
//! - Provides the **audit-ledger integration**
//!   ([`MesAdapterEventPayload`], [`write_mes_adapter_event`]) — every
//!   emitted canonical event records one audit-ledger entry of kind
//!   `EventKind::MesAdapterEvent` (storage string `mes.adapter_event`).
//!
//! # What this crate does NOT do (Phase α)
//!
//! - No real hardware integration. No MTConnect HTTP calls, no
//!   SSH-to-robot, no Renishaw probe parsing. NoopAdapter only.
//! - No runtime task that subscribes to broadcast streams and writes
//!   to the ledger. That lands in Phase β when the first real adapter
//!   gets its own crate and the SPA surfaces adapter status.
//! - No UI / CLI surface. No new HTTP routes. No DB schema changes.
//! - No operator-facing configuration (the `[mes]` `seller.toml` slot is
//!   future work).
//! - No bidirectional control (write-back to adapters). Future trait
//!   extension when the first real adapter needs it.
//!
//! # The next adapter author's first hour
//!
//! See `README.md` in this crate's directory for a copy-paste-and-fill-in
//! template walking through a minimal adapter implementation.

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

mod adapter;
mod audit;
mod error;
mod events;
mod noop;
mod registry;

pub use adapter::{Adapter, AdapterHealth};
pub use audit::{audit_kind_string, write_mes_adapter_event, MesAdapterEventPayload};
pub use error::{AdapterError, RegistryError};
pub use events::{CanonicalEvent, MachineState, QualityOutcome, WorkOrderState};
pub use noop::NoopAdapter;
pub use registry::AdapterRegistry;
