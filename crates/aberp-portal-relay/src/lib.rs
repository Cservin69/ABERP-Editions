//! ADR-0115 Phase 0 — the VPS-side relay and front.
//!
//! # What this is
//!
//! One small binary on a VPS that does three things:
//!
//! - [`broker`] parks authenticated front requests in a bounded queue
//!   and hands them to the Mac when the Mac comes asking;
//! - [`agentleg`] is the two-endpoint surface the Mac polls;
//! - [`front`] answers browsers: **the parked nginx** to everyone, the
//!   portal to a caller carrying the current knock token;
//! - [`canary`] watches the un-knocked traffic. Since this host has no
//!   legitimate unauthenticated visitors, every such request is a probe
//!   — recorded and coalesced here, alerted from the Mac.
//!
//! [`http1`] and [`nginx`] are the two modules that make the disguise
//! real: the relay owns its own connections rather than delegating to a
//! framework whose parse errors it cannot control, and every
//! un-authenticated answer is a byte-for-byte reproduction of what a
//! parked nginx would have said to that request class.
//!
//! # The transport, in one paragraph
//!
//! The Mac **polls**; the relay never pushes. There is no held-open
//! tunnel. A browser request is parked in memory, collected by the
//! Mac's next long-poll, answered on a second outbound request, and
//! handed back to the waiting front task. "Mac down or wedged → the
//! host is simply not there" is enforced by a presence **lease** that
//! the Mac renews by polling, not by a socket closing — which also
//! covers the wedged-but-connected case a socket close does not.
//!
//! # What this deliberately is not
//!
//! It holds nothing at rest. It has no database, no disk spool, no
//! credential store, no session store, no request/response body
//! logging, **no SMTP credentials** (the canary alert is sent from the
//! Mac, on the poll that already runs), and no ability to verify a
//! WebAuthn assertion — the relying party is the agent on the Mac
//! (ADR-0115 §4.2). The dependency list in `Cargo.toml` is the shortest
//! proof of that: there is no `p256`, no `x509-cert`, no keychain, no
//! storage crate, and no dependency on `aberp-portal-agent`. As of the
//! round-2 reshape it no longer has `axum`, `axum-server` or `tower`
//! either — owning the connection made them unnecessary, and a shorter
//! list is a shorter thing to audit.
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
//! cannot mint one, cannot widen the agent's allowlist, and cannot
//! recover anything afterwards. See [`broker`].
//!
//! It cannot enrol a passkey either — but that is a claim about two
//! controls on the Mac (Apple attestation, §4.3a, and console
//! confirmation, §4.3b), NOT about the enrolment token being secret
//! from this process. It is not: a live token crosses this memory in
//! plaintext until hardening H1. See
//! `aberp_portal_agent::webauthn` for the full correction.

pub mod agentleg;
pub mod broker;
pub mod canary;
pub mod front;
pub mod http1;
pub mod nginx;

pub use agentleg::AgentLeg;
pub use broker::Broker;
pub use canary::Canary;
pub use front::Front;
pub use nginx::Class;
