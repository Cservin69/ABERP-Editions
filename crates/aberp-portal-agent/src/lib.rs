//! ADR-0113 Phase 0 — the Mac-side portal agent.
//!
//! # The one-paragraph version
//!
//! A small daemon, separate from `aberp serve`, that dials **out** to a
//! relay on a VPS and answers browser requests the relay forwards down
//! that connection. It is the **WebAuthn relying party** — it holds the
//! credential store, issues and verifies challenges, and mints sessions
//! — so a relay compromise can neither mint a session nor read anything
//! at rest (ADR-0113 §4.2, §G4). It proxies exactly four `GET` routes to
//! the local ABERP behind the existing keychain bearer and refuses every
//! mutating verb (§6.3). The Mac opens **no inbound port** (§G1).
//!
//! # Where each locked decision lives
//!
//! | Decision | Module |
//! |---|---|
//! | Passwordless, passkey-only; RP on the Mac | [`webauthn`] |
//! | Recovery = the Mac; console-only enrolment | [`enrol`] |
//! | Hostname never committed (`PORTAL_HOST`) | [`config`] |
//! | High-entropy knock token, minted at the agent | [`knock`] |
//! | Read-only, enforced at the agent | [`allowlist`] |
//! | Sessions bound to the tunnel, no refresh tokens | [`session`] |
//! | Outbound-only, mutually pinned, jittered reconnect | [`tunnel`] |
//! | ABERP up/down independent of ABERP | [`health`] |
//! | Metadata-only, append-only, refusals logged | [`audit`] |
//! | Scanner trap: probe log + rate-limited alert | [`canary`], [`alert`] |
//!
//! # What Phase 0 does NOT do, honestly
//!
//! - **No inner browser↔agent encryption.** Ervin's §9.4 decision puts
//!   HPKE (hardening H1) in Phase 2. Until it lands, Leg A's TLS
//!   terminates at the VPS and payloads — including invoice data —
//!   transit relay memory in plaintext. A live root-level compromise of
//!   the relay can read a session while it happens. Bounded by: the
//!   data is read-only, no standing access is gained, challenges are
//!   single-use, and sessions die with the tunnel.
//! - **No mTLS on the browser leg.** §9.3 chose the knock token for
//!   Phase 0; client certificates (§3.3a, hardening H3) remain
//!   available for desktop-only use later.
//! - **No read-only-scoped upstream bearer.** §6.4's liability stands
//!   until hardening H2 lands in `serve.rs`.
//! - **The canary records no TLS SNI or client fingerprint** — see
//!   [`canary`] and `aberp-portal-relay::canary` for why that needs a
//!   custom TLS acceptor, and why it is Phase 2 rather than half-done.
//! - **The SMTP SPOC is single in configuration and policy, not yet in
//!   code** — see [`alert`].

pub mod agent;
pub mod alert;
pub mod allowlist;
pub mod audit;
pub mod canary;
pub mod config;
pub mod credstore;
pub mod enrol;
pub mod health;
pub mod knock;
pub mod rand;
pub mod session;
pub mod tunnel;
pub mod upstream;
pub mod webauthn;

pub use agent::Agent;
pub use config::AgentConfig;
