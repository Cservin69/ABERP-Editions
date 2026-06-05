//! Concrete adapter implementations bundled with the framework.
//!
//! Phase β (S229 / PR-225) ships the first real adapter:
//! [`barcode_scanner::BarcodeScannerAdapter`] — a TCP socket listener
//! suitable for the well-known industrial pattern where a Cognex /
//! Datalogic / Honeywell scanner emits decoded payloads as
//! line-delimited UTF-8 over plain TCP.
//!
//! Phase δ (S245 / PR-238) ships the first hardware-output adapter:
//! [`zebra::ZebraAdapter`] — a raw-TCP ZPL II writer for Zebra-protocol
//! thermal label printers (Zebra ZD/ZT/GK + ZPL-compatible clones).
//! Used by Dispatch (S234 / PR-230) for shipping labels and by Inventory
//! (S231 / PR-227) for bin/lot/product labels.
//!
//! Phase δ also ships the second hardware-input adapter
//! (S247 / PR-240): [`mtconnect::MtconnectAdapter`] — an HTTP-poll
//! consumer of the open MTConnect Streams XML protocol every modern CNC
//! controller speaks (DMG MORI, Mazak, Haas, Okuma, …). Lives in-tree
//! because the wire shape is HTTP+XML — no vendor SDK to isolate.
//!
//! Future phases add OPC-UA / Renishaw / robot adapters; each lives
//! either inside this module (when the protocol code is small and
//! self-contained) or in a per-vendor crate (when it pulls vendor SDKs).
//!
//! Per ADR-0060 §"The next adapter author's first hour" — adapters
//! speak vendor-specific protocols on one side and emit
//! [`CanonicalEvent`](crate::CanonicalEvent)s on the other.

pub mod barcode_scanner;
pub mod mtconnect;
pub mod zebra;
