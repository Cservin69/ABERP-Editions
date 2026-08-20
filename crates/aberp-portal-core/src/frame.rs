//! Length-prefixed frame codec for Leg B.
//!
//! `[u32 big-endian length][JSON payload]`. Nothing cleverer is needed:
//! both ends of this leg are our own code, the leg is already inside
//! mutually-pinned TLS (ADR-0113 §2.3), and a hand-rolled 30-line codec
//! is a smaller supply-chain surface than a WebSocket stack — the same
//! reasoning `crates/nav-transport` applies to SOAP and
//! `crates/nav-xsd-validator` applies to XSD.
//!
//! **Deviation from ADR-0113 §2.1, flagged:** the ADR names this leg
//! "WSS". WebSocket framing buys proxy traversal and browser
//! compatibility; neither applies here (no browser touches Leg B, and
//! the agent dials 443 directly). Upgrading to WSS later is a change to
//! this one module — the frames above it do not move. If the deployed
//! Mac ever sits behind a proxy that demands HTTP semantics on 443,
//! that is the trigger to do it.
//!
//! # The size cap is a security control, not a tidiness rule
//!
//! The relay must hold nothing at rest (§2.4) and buffers frames in
//! memory. An unbounded length prefix from a compromised peer is a
//! trivial OOM. [`MAX_FRAME_BYTES`] bounds it, and a frame that
//! declares more is a hard error that drops the connection rather than
//! a truncation that might be mistaken for a short read.

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Largest frame either side will send or accept, in bytes.
///
/// 8 MiB is the invoice-PDF ceiling with room to spare (the
/// `crates/invoice-pdf` renderer emits single-page documents measured
/// in tens of kilobytes). **Residual, named:** Phase 0 buffers a whole
/// response in relay memory rather than streaming it, which ADR-0113 §7
/// flags as the Phase-1 posture to fix ("stream, don't buffer whole on
/// the VPS"). Buffered-but-capped is bounded and transient; the
/// no-at-rest rule is not weakened. Streaming is a Phase-1 item.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// Frame-level failures. Every variant drops the connection: on a leg
/// where both peers are pinned, a malformed frame is not a hiccup to
/// recover from, it is a peer behaving unlike itself.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("leg B I/O: {0}")]
    Io(#[from] io::Error),
    #[error("peer closed the tunnel")]
    Closed,
    #[error("frame declared {declared} bytes, cap is {MAX_FRAME_BYTES}")]
    TooLarge { declared: u64 },
    #[error("frame payload is not valid JSON for this protocol: {0}")]
    Malformed(#[from] serde_json::Error),
}

/// Reads frames off the tunnel.
pub struct FrameReader<R> {
    inner: R,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    pub fn new(inner: R) -> Self {
        Self { inner }
    }

    /// Read exactly one frame. Returns [`FrameError::Closed`] on a
    /// clean EOF at a frame boundary (an orderly shutdown) and
    /// [`FrameError::Io`] on EOF mid-frame (a truncated one).
    pub async fn read_frame<T: serde::de::DeserializeOwned>(&mut self) -> Result<T, FrameError> {
        let mut len_buf = [0u8; 4];
        match self.inner.read_exact(&mut len_buf).await {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Err(FrameError::Closed),
            Err(e) => return Err(FrameError::Io(e)),
        }
        let declared = u32::from_be_bytes(len_buf) as u64;
        if declared > MAX_FRAME_BYTES as u64 {
            return Err(FrameError::TooLarge { declared });
        }
        let mut payload = vec![0u8; declared as usize];
        self.inner.read_exact(&mut payload).await?;
        Ok(serde_json::from_slice(&payload)?)
    }
}

/// Writes frames onto the tunnel.
pub struct FrameWriter<W> {
    inner: W,
}

impl<W: AsyncWrite + Unpin> FrameWriter<W> {
    pub fn new(inner: W) -> Self {
        Self { inner }
    }

    /// Serialise and write one frame, then flush. Refuses to emit a
    /// frame over the cap — a local bug should fail here, loudly, and
    /// not travel to a peer that would drop the connection for it.
    pub async fn write_frame<T: serde::Serialize>(&mut self, frame: &T) -> Result<(), FrameError> {
        let payload = serde_json::to_vec(frame)?;
        if payload.len() > MAX_FRAME_BYTES {
            return Err(FrameError::TooLarge {
                declared: payload.len() as u64,
            });
        }
        let len = u32::try_from(payload.len()).expect("payload is <= MAX_FRAME_BYTES, so it fits");
        self.inner.write_all(&len.to_be_bytes()).await?;
        self.inner.write_all(&payload).await?;
        self.inner.flush().await?;
        Ok(())
    }

    /// Close the underlying stream.
    pub async fn shutdown(&mut self) -> Result<(), FrameError> {
        self.inner.shutdown().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{Frame, PortalRequest, PROTOCOL_VERSION};

    /// A duplex pipe stands in for the TLS stream.
    async fn roundtrip(frames: Vec<Frame>) -> Vec<Frame> {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let sent = frames.clone();
        let writer = tokio::spawn(async move {
            let mut w = FrameWriter::new(client);
            for f in &sent {
                w.write_frame(f).await.expect("write");
            }
            w.shutdown().await.expect("shutdown");
        });
        let mut r = FrameReader::new(server);
        let mut got = Vec::new();
        loop {
            match r.read_frame::<Frame>().await {
                Ok(f) => got.push(f),
                Err(FrameError::Closed) => break,
                Err(e) => panic!("unexpected read error: {e}"),
            }
        }
        writer.await.expect("writer task");
        got
    }

    #[tokio::test]
    async fn frames_roundtrip_in_order() {
        let frames = vec![
            Frame::Hello {
                protocol_version: PROTOCOL_VERSION,
                knock_token: "tok".into(),
                expected_host: None,
                tripwire_path: "/decoy".into(),
                tunnel_id: "tid".into(),
            },
            Frame::Request {
                id: 1,
                req: PortalRequest {
                    method: "GET".into(),
                    path: "/api/health".into(),
                    query: None,
                    cookie: None,
                    body_b64: None,
                    peer: None,
                },
            },
            Frame::Ping { nonce: 42 },
        ];
        assert_eq!(roundtrip(frames.clone()).await, frames);
    }

    #[tokio::test]
    async fn oversized_declared_length_is_refused_without_allocating() {
        // Hand-craft a header that claims 4 GiB. A codec that trusted it
        // would try to allocate that much before reading a single byte.
        let (mut client, server) = tokio::io::duplex(64);
        tokio::spawn(async move {
            let _ = client.write_all(&u32::MAX.to_be_bytes()).await;
            // Deliberately send no payload.
        });
        let mut r = FrameReader::new(server);
        match r.read_frame::<Frame>().await {
            Err(FrameError::TooLarge { declared }) => assert_eq!(declared, u64::from(u32::MAX)),
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn clean_eof_at_frame_boundary_reports_closed() {
        let (client, server) = tokio::io::duplex(64);
        drop(client);
        let mut r = FrameReader::new(server);
        assert!(matches!(
            r.read_frame::<Frame>().await,
            Err(FrameError::Closed)
        ));
    }

    #[tokio::test]
    async fn garbage_payload_is_malformed_not_a_panic() {
        let (mut client, server) = tokio::io::duplex(64);
        tokio::spawn(async move {
            let body = b"{not json";
            let _ = client.write_all(&(body.len() as u32).to_be_bytes()).await;
            let _ = client.write_all(body).await;
        });
        let mut r = FrameReader::new(server);
        assert!(matches!(
            r.read_frame::<Frame>().await,
            Err(FrameError::Malformed(_))
        ));
    }
}
