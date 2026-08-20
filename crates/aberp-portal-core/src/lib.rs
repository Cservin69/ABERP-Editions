//! ADR-0113 Phase 0 — the wire between the Mac agent and the VPS relay.
//!
//! # What this crate is
//!
//! The portal is three deployables (ADR-0113 §0):
//!
//! - the **agent** on the Mac — dials out, is the WebAuthn relying
//!   party, proxies read-only `GET`s to the local `aberp serve`;
//! - the **relay + front** on a VPS — accepts the agent's outbound
//!   connection and brokers browser sessions down it;
//! - the **portal shell** — the HTML the front serves post-knock.
//!
//! This crate is the *only* code the first two share. That is
//! deliberate: ADR-0113 §2.4 makes "the relay holds nothing and decides
//! nothing" a load-bearing claim, and the cheapest way to keep that
//! claim auditable is for the shared surface to contain no policy, no
//! credential handling, and no persistence — just
//!
//! - [`frame`] — the length-prefixed codec that carries Leg B;
//! - [`proto`] — the request/response shapes that cross it;
//! - [`pin`] — the mutual peer-pinning TLS configs (ADR-0113 §2.3);
//! - [`ct`] — constant-time comparison, used by every token check.
//!
//! # What this crate deliberately does NOT contain
//!
//! No WebAuthn verification, no session minting, no credential store,
//! no knock-token *authority*, no allowlist. Every one of those lives
//! in the agent, on the Mac, inside the trust boundary (ADR-0113 §4.2,
//! §6.3). A reader auditing "could a hostile relay build mint a
//! session?" only has to establish that this crate exposes nothing that
//! would let it.
//!
//! # Leg B in one paragraph
//!
//! The Mac dials the relay (never the reverse — ADR-0113 §G1), TLS with
//! **both peers pinned by leaf-certificate SHA-256** (§2.3: the public
//! WebPKI is not consulted on this leg at all). Once up, the connection
//! carries JSON [`proto::Frame`]s, each preceded by a 4-byte big-endian
//! length. The agent sends [`proto::Frame::Hello`] first — which is how
//! the relay learns the current knock token, since the token is minted
//! and rotated at the agent like everything else (§3.3). The relay
//! sends [`proto::Frame::Request`]s; the agent answers with
//! [`proto::Frame::Response`]s carrying the same `id`.

pub mod ct;
pub mod frame;
pub mod pin;
pub mod proto;

pub use frame::{FrameError, FrameReader, FrameWriter, MAX_FRAME_BYTES};
pub use pin::{PinError, PinnedFingerprint};
pub use proto::{Frame, PortalRequest, PortalResponse};
