//! ADR-0115 Phase 0 — the wire between the Mac agent and the VPS relay.
//!
//! # What this crate is
//!
//! The portal is three deployables (ADR-0115 §0):
//!
//! - the **agent** on the Mac — polls out, is the WebAuthn relying
//!   party, proxies read-only `GET`s to the local `aberp serve`;
//! - the **relay + front** on a VPS — parks browser requests for the
//!   agent to pull, and answers everyone else with one uniform 404;
//! - the **portal shell** — the HTML the front serves post-knock.
//!
//! This crate is the *only* code the first two share. That is
//! deliberate: ADR-0115 §2.4 makes "the relay holds nothing and decides
//! nothing" a load-bearing claim, and the cheapest way to keep that
//! claim auditable is for the shared surface to contain no policy, no
//! credential handling, and no persistence — just
//!
//! - [`canary`] — the scanner-trap observation types and classifier
//!   (the trap itself is split across the front and the agent);
//! - [`proto`] — the poll/deliver shapes that cross Leg B;
//! - [`pin`] — the mutual peer-pinning TLS configs (ADR-0115 §2.3);
//! - [`ct`] — constant-time comparison, used by every token check.
//!
//! # What this crate deliberately does NOT contain
//!
//! No WebAuthn verification, no attestation trust anchor, no session
//! minting, no credential store, no knock-token *authority*, no
//! allowlist. Every one of those lives in the agent, on the Mac, inside
//! the trust boundary (ADR-0115 §4.2, §6.3). A reader auditing "could a
//! hostile relay build mint a session?" only has to establish that this
//! crate exposes nothing that would let it.
//!
//! # Leg B in one paragraph
//!
//! The Mac **polls** the relay (never the reverse — ADR-0115 §G1), over
//! HTTPS with **both peers pinned by leaf-certificate SHA-256** (§2.3:
//! the public WebPKI is not consulted on this leg at all). The relay
//! parks an authenticated front request in a bounded queue; the Mac's
//! long-poll pulls it, runs it locally, and posts the answer back on a
//! second outbound request. Nothing is pushed, nothing is held open past
//! [`proto::MAX_POLL_WAIT`], and the relay's knowledge of the knock
//! token expires with [`proto::PRESENCE_TTL`] if the Mac stops asking.

pub mod canary;
pub mod ct;
pub mod pin;
pub mod proto;

pub use canary::{CanaryBatch, ProbeSample, Reason, Severity};
pub use pin::{PinError, PinnedFingerprint};
pub use proto::{
    AgentIdentity, Delivery, DeliveryAck, Heartbeat, PollRequest, PollResponse, PortalRequest,
    PortalResponse, Work,
};
