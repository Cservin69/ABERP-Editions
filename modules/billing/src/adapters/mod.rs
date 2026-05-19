//! Concrete implementations of the [`crate::ports`] traits.
//!
//! Per ADR-0006 §Conformance: every module ships at least one in-memory
//! adapter for every port, so tests run without real IO. The DuckDB
//! adapter is the production backend (ADR-0019 §1).
//!
//! Sub-files:
//!
//! - [`in_memory_store`] — pure-Rust adapter used by tests.
//! - [`duckdb_store`]    — DuckDB-backed production adapter.

pub mod duckdb_store;
pub mod in_memory_store;
