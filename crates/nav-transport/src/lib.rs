//! `aberp-nav-transport` — NAV TLS transport, credentials, SOAP envelope,
//! signature primitives, AES-128/ECB exchange-token decryption, and the
//! typed `tokenExchange` / `manageInvoice` operations.
//!
//! See ADR-0009 §4 (NAV authentication and credentials), ADR-0020 §1-3
//! (transport / credential / threat-model correction), ADR-0021 §A9 +
//! §A14 (AES-128/ECB / keychain).
//!
//! # PR-7-A scope (landed)
//!
//!   - [`NavEndpoint`], [`NavTransport`] — pinned-trust reqwest client.
//!   - [`NavCredentials`] — four-artifact keychain bundle.
//!
//! # PR-7-B-1 scope (this PR's first commit)
//!
//!   - [`signatures`] — SHA-512 `passwordHash`, SHA3-512
//!     `requestSignature` (with per-invoice-index extension for
//!     `manageInvoice` / `manageAnnulment`).
//!   - [`soap`] — hand-rolled NAV v3.0 SOAP envelope assembly
//!     (`<TokenExchangeRequest>`, `<ManageInvoiceRequest>`) per
//!     ADR-0021 §A8.
//!
//! # PR-7-B-2 scope (this PR's second commit)
//!
//!   - [`cipher`] — AES-128/ECB decryption of NAV's exchangeToken
//!     envelope per ADR-0020 §2 + ADR-0021 §A9 ("protocol-imposed by
//!     NAV; must not generalize").
//!   - [`operations::token_exchange`] — `tokenExchange` call against
//!     the pinned [`NavTransport`].
//!
//! # PR-7-B-3 scope (this PR's third commit)
//!
//!   - [`operations::manage_invoice`] — `manageInvoice` call + typed
//!     response parsing + retryable/non-retryable error mapping per
//!     ADR-0009 §5.
//!
//! # What this crate still does NOT provide
//!
//!   - `queryTransactionStatus` ack-poll loop (PR-7-C).
//!   - `manageAnnulment` (technical annulment, PR-7-C+).
//!   - Audit-ledger writes — those are the binary's responsibility,
//!     called from the NAV submission path in `apps/aberp/src/
//!     submit_invoice.rs`.

#![forbid(unsafe_code)]

pub mod cipher;
pub mod credentials;
pub mod endpoint;
pub mod error;
pub mod operations;
pub mod signatures;
pub mod soap;
pub mod trust;

mod client;

pub use client::NavTransport;
pub use credentials::NavCredentials;
pub use endpoint::NavEndpoint;
pub use error::NavTransportError;
