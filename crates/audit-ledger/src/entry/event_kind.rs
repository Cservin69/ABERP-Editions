//! [`EventKind`] — typed event kinds per ADR-0008 §"Entry shape".
//!
//! `kind` is the type discriminant for `payload`'s schema. Schema versioning
//! is implicit in the kind name: bumping a payload schema renames the kind,
//! and the old kind remains valid for historical entries.
//!
//! No serde derive: PR-3 stores the kind as a plain text column in DuckDB
//! via [`EventKind::as_str`]. Serde will join when a serialization path
//! (export bundle, wire protocol) actually needs it.

/// PR-3 ships one variant — `Test` — for the conformance test. Real kinds
/// (`InvoiceDraftCreated`, `InvoiceSequenceReserved`, `InvoiceFinalized`,
/// ...) per ADR-0009 §2 land in PR-4.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventKind {
    /// Test-only kind used by `tests/chain_conformance.rs`. Not allowed in
    /// production code; PR-4 will gate this via a conformance check.
    Test,
}

impl EventKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            EventKind::Test => "test",
        }
    }
}
