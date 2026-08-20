//! Mutual peer pinning for Leg B (ADR-0113 §2.3).
//!
//! > The **relay pins the agent** […] The **agent pins the relay** […]
//! > The public WebPKI is not consulted on Leg B, so a mis-issued
//! > public cert for the relay's hostname buys an attacker nothing.
//!
//! Both directions are the same primitive: accept the peer iff
//! `SHA-256(leaf certificate DER)` equals a fingerprint configured
//! out-of-band. No chain building, no name checking, no root store —
//! there is no CA in this design, only two certificates that know each
//! other.
//!
//! # One difference from `apps/aberp-ui/src/pinned_client.rs`
//!
//! That verifier — for the loopback leg to `aberp serve` — accepts the
//! handshake signature unconditionally, which is defensible on
//! `127.0.0.1`. Leg B crosses the internet, so this module **does**
//! verify `CertificateVerify` (via rustls's own
//! [`rustls::crypto::verify_tls13_signature`]). Without that check a
//! fingerprint pin proves only that the peer can *show* a certificate,
//! and certificates are public: anyone who captured one could replay it
//! without ever holding the private key. Pinning the leaf and verifying
//! possession of its key are two halves of one control; on this leg we
//! need both.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{
    ClientConfig, DigitallySignedStruct, DistinguishedName, ServerConfig, SignatureScheme,
};
use sha2::{Digest, Sha256};

use crate::ct;

/// Pinning / TLS-config construction failures. All of them are
/// misconfiguration, and all of them must stop a daemon from starting:
/// a portal that silently fell back to WebPKI would be a different
/// design (§2.3).
#[derive(Debug, thiserror::Error)]
pub enum PinError {
    #[error("fingerprint `{raw}` is not valid hex: {source}")]
    NotHex {
        raw: String,
        #[source]
        source: hex::FromHexError,
    },
    #[error("fingerprint `{raw}` decoded to {len} bytes, expected 32 (SHA-256)")]
    WrongLength { raw: String, len: usize },
    #[error("no peer fingerprints configured — Leg B refuses to run unpinned")]
    NoPeersPinned,
    #[error("building the TLS config: {0}")]
    Rustls(#[from] rustls::Error),
}

/// A pinned peer: the SHA-256 of its leaf certificate's DER encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedFingerprint([u8; 32]);

impl PinnedFingerprint {
    /// Parse the 64-hex-character form used in config and in
    /// `~/.aberp-*/serve/<tenant>/` fingerprint files.
    pub fn from_hex(raw: &str) -> Result<Self, PinError> {
        let raw = raw.trim();
        let bytes = hex::decode(raw).map_err(|source| PinError::NotHex {
            raw: raw.to_string(),
            source,
        })?;
        let len = bytes.len();
        let arr: [u8; 32] = bytes.try_into().map_err(|_| PinError::WrongLength {
            raw: raw.to_string(),
            len,
        })?;
        Ok(Self(arr))
    }

    /// The fingerprint of a certificate as presented on the wire.
    #[must_use]
    pub fn of_cert(der: &CertificateDer<'_>) -> Self {
        let mut h = Sha256::new();
        h.update(der.as_ref());
        let out = h.finalize();
        let mut arr = [0u8; 32];
        arr.copy_from_slice(out.as_slice());
        Self(arr)
    }

    #[must_use]
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// Constant-time equality — see [`crate::ct`].
    #[must_use]
    pub fn matches(&self, other: &Self) -> bool {
        ct::eq(&self.0, &other.0)
    }
}

/// Install the process-wide rustls crypto provider, idempotently.
///
/// The workspace compiles both `aws-lc-rs` (rcgen, axum-server) and
/// `ring` (transitively via lettre), so rustls refuses to pick one
/// implicitly and `ClientConfig::builder()` would panic. Every binary
/// and test in the portal calls this before building a config —
/// `aberp serve` does the same thing at boot.
pub fn install_default_crypto_provider() {
    // Err means someone already installed one, which is the outcome we
    // want; there is nothing to report.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

fn verification_algorithms() -> Arc<rustls::crypto::CryptoProvider> {
    rustls::crypto::CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
}

/// The agent's side of Leg B: pin exactly one relay certificate, and
/// present the agent's own client certificate.
pub fn agent_client_config(
    pinned_relay: PinnedFingerprint,
    client_chain: Vec<CertificateDer<'static>>,
    client_key: PrivateKeyDer<'static>,
) -> Result<ClientConfig, PinError> {
    install_default_crypto_provider();
    let verifier = Arc::new(PinnedPeerVerifier {
        pinned: vec![pinned_relay],
        role: "relay",
        provider: verification_algorithms(),
        hint_subjects: Vec::new(),
    });
    let cfg = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(client_chain, client_key)?;
    Ok(cfg)
}

/// Leg C's side of the same primitive: pin one server certificate and
/// present none of our own.
///
/// The agent talks to `aberp serve` over loopback, whose listener uses
/// a self-signed `rcgen` certificate with no chain to any public CA
/// (`apps/aberp/src/serve.rs`). `reqwest`'s `add_root_certificate`
/// MERGES with the webpki defaults, which is the opposite of what is
/// wanted, so the config is built here and handed to reqwest whole —
/// the same argument `apps/aberp-ui/src/pinned_client.rs` records for
/// the Tauri shell.
pub fn loopback_client_config(pinned_server: PinnedFingerprint) -> Result<ClientConfig, PinError> {
    install_default_crypto_provider();
    let verifier = Arc::new(PinnedPeerVerifier {
        pinned: vec![pinned_server],
        role: "ABERP loopback listener",
        provider: verification_algorithms(),
        hint_subjects: Vec::new(),
    });
    Ok(ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth())
}

/// The relay's side of Leg B: demand a client certificate and accept
/// only the pinned agent(s).
///
/// The allowlist is a `Vec` rather than a single value because §2.3
/// already anticipates "a short allowlist for a future second Mac".
/// An empty list is refused outright: a relay that accepts any client
/// is not this design.
pub fn relay_server_config(
    pinned_agents: Vec<PinnedFingerprint>,
    server_chain: Vec<CertificateDer<'static>>,
    server_key: PrivateKeyDer<'static>,
) -> Result<ServerConfig, PinError> {
    install_default_crypto_provider();
    if pinned_agents.is_empty() {
        return Err(PinError::NoPeersPinned);
    }
    let verifier = Arc::new(PinnedPeerVerifier {
        pinned: pinned_agents,
        role: "agent",
        provider: verification_algorithms(),
        hint_subjects: Vec::new(),
    });
    let cfg = ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(server_chain, server_key)?;
    Ok(cfg)
}

/// One verifier type serves both directions — the check is symmetric.
#[derive(Debug)]
struct PinnedPeerVerifier {
    pinned: Vec<PinnedFingerprint>,
    /// `"relay"` or `"agent"`, for the error message only.
    role: &'static str,
    provider: Arc<rustls::crypto::CryptoProvider>,
    hint_subjects: Vec<DistinguishedName>,
}

impl PinnedPeerVerifier {
    fn check(&self, end_entity: &CertificateDer<'_>) -> Result<(), rustls::Error> {
        let observed = PinnedFingerprint::of_cert(end_entity);
        if self.pinned.iter().any(|p| p.matches(&observed)) {
            return Ok(());
        }
        // The message names the observed fingerprint so a rotation that
        // was not propagated is a one-line diagnosis. It is written to
        // the local log of whichever side rejected; nothing is sent to
        // the peer beyond a TLS alert.
        Err(rustls::Error::General(format!(
            "Leg B {} certificate is not pinned: observed {}",
            self.role,
            observed.to_hex()
        )))
    }
}

impl ServerCertVerifier for PinnedPeerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.check(end_entity)?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

impl ClientCertVerifier for PinnedPeerVerifier {
    /// There is no CA to hint at; the peer already knows which
    /// certificate to send because there is exactly one.
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &self.hint_subjects
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        self.check(end_entity)?;
        Ok(ClientCertVerified::assertion())
    }

    /// Mandatory: an anonymous client is dropped inside the handshake,
    /// "before any application byte" (§2.3).
    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_parses_and_renders() {
        let hex_fp = "00".repeat(32);
        let fp = PinnedFingerprint::from_hex(&hex_fp).expect("parses");
        assert_eq!(fp.to_hex(), hex_fp);
    }

    #[test]
    fn fingerprint_rejects_non_hex() {
        assert!(matches!(
            PinnedFingerprint::from_hex("zz".repeat(32).as_str()),
            Err(PinError::NotHex { .. })
        ));
    }

    #[test]
    fn fingerprint_rejects_short_input() {
        // 32 hex chars is 16 bytes — a SHA-1-length pin would silently
        // halve the security of the check if it were accepted.
        assert!(matches!(
            PinnedFingerprint::from_hex(&"ab".repeat(16)),
            Err(PinError::WrongLength { len: 16, .. })
        ));
    }

    #[test]
    fn fingerprint_of_cert_is_sha256_of_der() {
        let der = CertificateDer::from(vec![1u8, 2, 3]);
        let expected = hex::encode(Sha256::digest([1u8, 2, 3]));
        assert_eq!(PinnedFingerprint::of_cert(&der).to_hex(), expected);
    }

    #[test]
    fn relay_refuses_to_start_with_an_empty_allowlist() {
        install_default_crypto_provider();
        let err = relay_server_config(
            Vec::new(),
            vec![CertificateDer::from(vec![0u8])],
            // The emptiness check fires before the key is ever parsed;
            // this placeholder only has to typecheck.
            PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(vec![0u8])),
        );
        assert!(matches!(err, Err(PinError::NoPeersPinned)));
    }

    #[test]
    fn verifier_accepts_only_the_pinned_leaf() {
        install_default_crypto_provider();
        let pinned_der = CertificateDer::from(vec![9u8, 9, 9]);
        let v = PinnedPeerVerifier {
            pinned: vec![PinnedFingerprint::of_cert(&pinned_der)],
            role: "relay",
            provider: verification_algorithms(),
            hint_subjects: Vec::new(),
        };
        assert!(v.check(&pinned_der).is_ok());
        assert!(v.check(&CertificateDer::from(vec![9u8, 9, 8])).is_err());
    }
}
