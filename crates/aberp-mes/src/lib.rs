//! ABERP Stage 3 manufacturing-adapter framework.
//!
//! Per ADR-0060 (`adr/0060-stage3-manufacturing-adapter-framework.md`).
//!
//! # What this crate does
//!
//! - Defines the **canonical event vocabulary**
//!   ([`CanonicalEvent`]) — seven variants covering machine state, part
//!   movement, quality, scan, work-order, robot-task-queued and
//!   robot-state. Closed Rust enum; the vocabulary IS the schema. Future
//!   variants land incrementally per ADR-0060 §"The canonical event
//!   vocabulary".
//! - Defines the **[`Adapter`] trait** — async surface with `name`,
//!   `kind`, `endpoint_host`, `endpoint_port`, `start`, `stop`, `health`
//!   and `subscribe`. Read-only by design: the trait carries no
//!   write-back to the machine.
//! - Ships the **[`AdapterRegistry`]** — runtime map of
//!   `Arc<dyn Adapter>` keyed by adapter `name()`. NOT persisted per
//!   `[[no-sql-specific]]` extended by the Stage 3 memo.
//! - Ships **five concrete adapters with real transports**, in the
//!   [`adapters`] module of this crate (the per-vendor-crate split
//!   ADR-0060 anticipated has not been needed): barcode scanner (TCP
//!   listener), Zebra label printer (raw TCP ZPL), CNC over MTConnect
//!   (HTTP `/current` polling), Universal Robots (RTDE over TCP), and
//!   Trumpf laser (via an MTConnect agent, reusing the CNC adapter's
//!   `poll_once` and parser). [`build_adapter`] constructs one from an
//!   operator-supplied [`AdapterConfigEntry`]; the `[mes]` `seller.toml`
//!   section and the SPA's adapter CRUD surface are both live.
//! - Ships the **[`NoopAdapter`]** — reference implementation that does
//!   nothing real. Used by the framework's own tests and as a starting
//!   point for adapter authors.
//! - Provides the **audit-ledger integration**
//!   ([`MesAdapterEventPayload`], [`write_mes_adapter_event`]) — every
//!   emitted canonical event records one audit-ledger entry of kind
//!   `EventKind::MesAdapterEvent` (storage string `mes.adapter_event`) —
//!   plus [`spawn_ledger_writer`], the runtime task that subscribes to
//!   an adapter's broadcast stream and does that writing.
//!
//! # What this crate does NOT do
//!
//! - **No bidirectional control on the trait.** [`Adapter`] is
//!   read-only. [`ZebraAdapter`] carries an inherent `print_zpl` for the
//!   label path, but nothing in `apps/aberp` calls it yet, and there is
//!   no `AdapterCommand` / `dispatch` surface.
//! - **No probe-value ingestion.** QC probe sources
//!   (`MtconnectProbeSource`, `RenishawCentralSource`) live in
//!   `aberp-qa` and are `todo!` — see backlog D-02.
//! - **No OPC-UA or Oseon laser backends.** Both are declared in
//!   [`adapters::trumpf`], return an error rather than panicking, and
//!   are never constructed by [`build_adapter`] — see backlog D-13/D-14.
//! - **No MTConnect beyond `/current` snapshot polling.** No `/sample`
//!   subscription, no `/probe` catalog, no `/assets`, no SHDR
//!   Adapter-side code. The parser extracts six leaf data items but only
//!   `Execution` currently drives an event — see backlog D-16.
//! - **No DB schema.** Adapter configuration lives in `seller.toml`, not
//!   in a table.
//! - **No cell-controller / offline-first split.** A single ABERP
//!   process is still assumed.
//!
//! The backlog IDs above index
//! `docs/BACKLOG-designed-to-live.md` at the repo root.
//!
//! # The next adapter author's first hour
//!
//! See `README.md` in this crate's directory for a copy-paste-and-fill-in
//! template walking through a minimal adapter implementation.

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

mod adapter;
mod adapter_config;
pub mod adapters;
mod audit;
mod error;
mod events;
mod ledger_writer;
mod noop;
mod registry;

pub use adapter::{Adapter, AdapterHealth};
pub use adapter_config::{
    build_adapter, AdapterConfigEntry, AdapterConfigError, AdapterConfigFieldError, AdapterKind,
};
pub use adapters::barcode_scanner::{
    aim_id_to_symbology, split_aim_prefix, BarcodeScannerAdapter, BarcodeScannerConfig,
    DEFAULT_CHANNEL_CAPACITY, DEFAULT_LISTEN_PORT, DEFAULT_MAX_CONCURRENT_CONNECTIONS,
    DEFAULT_MAX_PAYLOAD_LEN,
};
pub use adapters::mtconnect::{
    MtconnectAdapter, MtconnectAdapterConfig, DEFAULT_AGENT_PORT as MTCONNECT_DEFAULT_AGENT_PORT,
    DEFAULT_CHANNEL_CAPACITY as MTCONNECT_DEFAULT_CHANNEL_CAPACITY,
    DEFAULT_MAX_RESPONSE_BYTES as MTCONNECT_DEFAULT_MAX_RESPONSE_BYTES,
    DEFAULT_POLL_INTERVAL as MTCONNECT_DEFAULT_POLL_INTERVAL,
    DEFAULT_REQUEST_TIMEOUT as MTCONNECT_DEFAULT_REQUEST_TIMEOUT,
    DEFAULT_SLOW_THRESHOLD as MTCONNECT_DEFAULT_SLOW_THRESHOLD,
};
pub use adapters::trumpf::{
    LaserSnapshot, MockLaserSource, MtconnectLaserSource, OpcUaLaserSource, OseonLaserSource,
    TrumpfAdapter, TrumpfAdapterConfig, TrumpfSource,
    DEFAULT_AGENT_PORT as TRUMPF_DEFAULT_AGENT_PORT,
    DEFAULT_CHANNEL_CAPACITY as TRUMPF_DEFAULT_CHANNEL_CAPACITY,
    DEFAULT_DEVICE_NAME as TRUMPF_DEFAULT_DEVICE_NAME,
    DEFAULT_MAX_RESPONSE_BYTES as TRUMPF_DEFAULT_MAX_RESPONSE_BYTES,
    DEFAULT_POLL_INTERVAL as TRUMPF_DEFAULT_POLL_INTERVAL,
    DEFAULT_REQUEST_TIMEOUT as TRUMPF_DEFAULT_REQUEST_TIMEOUT,
    DEFAULT_SLOW_THRESHOLD as TRUMPF_DEFAULT_SLOW_THRESHOLD,
    MAX_DATA_ITEM_LEN as TRUMPF_MAX_DATA_ITEM_LEN,
};
pub use adapters::ur_rtde::{
    UrRtdeAdapter, UrRtdeAdapterConfig, DEFAULT_BACKOFF_CAP as UR_RTDE_DEFAULT_BACKOFF_CAP,
    DEFAULT_CHANNEL_CAPACITY as UR_RTDE_DEFAULT_CHANNEL_CAPACITY,
    DEFAULT_HANDSHAKE_TIMEOUT as UR_RTDE_DEFAULT_HANDSHAKE_TIMEOUT,
    DEFAULT_INITIAL_BACKOFF as UR_RTDE_DEFAULT_INITIAL_BACKOFF,
    DEFAULT_MAX_FRAME_BYTES as UR_RTDE_DEFAULT_MAX_FRAME_BYTES,
    DEFAULT_PAUSE_TIMEOUT as UR_RTDE_DEFAULT_PAUSE_TIMEOUT,
    DEFAULT_RTDE_PORT as UR_RTDE_DEFAULT_PORT,
    DEFAULT_STALL_THRESHOLD as UR_RTDE_DEFAULT_STALL_THRESHOLD,
};
pub use adapters::zebra::{
    ZebraAdapter, ZebraAdapterConfig, DEFAULT_CONNECT_TIMEOUT as ZEBRA_DEFAULT_CONNECT_TIMEOUT,
    DEFAULT_LISTEN_PORT as ZEBRA_DEFAULT_LISTEN_PORT,
    DEFAULT_MAX_PAYLOAD_LEN as ZEBRA_DEFAULT_MAX_PAYLOAD_LEN,
    DEFAULT_PROBE_INTERVAL as ZEBRA_DEFAULT_PROBE_INTERVAL,
    DEFAULT_RETRY_BACKOFF as ZEBRA_DEFAULT_RETRY_BACKOFF,
    DEFAULT_SLOW_THRESHOLD as ZEBRA_DEFAULT_SLOW_THRESHOLD,
};
pub use audit::{audit_kind_string, write_mes_adapter_event, MesAdapterEventPayload};
pub use error::{AdapterError, RegistryError};
pub use events::{
    CanonicalEvent, MachineState, QualityOutcome, RobotMode, SafetyMode, WorkOrderState,
};
pub use ledger_writer::{spawn_ledger_writer, LedgerWriterActor, LedgerWriterDeps};
pub use noop::NoopAdapter;
pub use registry::{AdapterHealthEntry, AdapterRegistry};
