//! ADR-0113 Phase 0 — the VPS-side relay and front.
//!
//! # What this is
//!
//! One small binary on a VPS that does two things:
//!
//! - [`broker`] accepts the Mac agent's **outbound** connection over
//!   mutually-pinned TLS and forwards opaque frames down it;
//! - [`front`] answers browsers: the uniform 404 to everyone, the
//!   portal to a caller carrying the current knock token;
//! - [`canary`] watches the un-knocked traffic. Since this host has no
//!   legitimate unauthenticated visitors, every such request is a
//!   probe — recorded and coalesced here, alerted from the Mac.
//!
//! # What this deliberately is not
//!
//! It holds nothing at rest. It has no database, no disk spool, no
//! credential store, no session store, no request/response body
//! logging, **no SMTP credentials** (the canary alert is sent from the
//! Mac, down the tunnel that already exists), and no ability to verify
//! a WebAuthn assertion — the
//! relying party is the agent on the Mac (ADR-0113 §4.2). The
//! dependency list in `Cargo.toml` is the shortest proof of that:
//! there is no `p256`, no `keyring`, no storage crate, and no
//! dependency on `aberp-portal-agent`.
//!
//! Its disk, stolen whole, is: its own TLS keys, the pinned agent
//! certificate (public), and metadata-only connection logs — Ervin's
//! §9.5 decision.
//!
//! # The residual, stated plainly
//!
//! Leg A's TLS terminates here, so until hardening H1 (browser↔agent
//! HPKE — Phase 2 per Ervin's §9.4 decision) everything crossing this
//! process, including invoice payloads, is in its memory in plaintext.
//! A live root-level compromise can read sessions as they happen. It
//! cannot mint one, cannot enrol a passkey, cannot widen the agent's
//! allowlist, and cannot recover anything afterwards. See [`broker`].

pub mod broker;
pub mod canary;
pub mod front;

pub use broker::Broker;
pub use canary::Canary;
pub use front::{router, Front, UNIFORM_404_BODY};
