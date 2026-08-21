//! Authenticator data parsing (WebAuthn Level 2, §6.1).
//!
//! ```text
//! rpIdHash (32) | flags (1) | signCount (4, big-endian) | [attested credential data] | [extensions]
//! ```
//!
//! and, when the `AT` flag is set, the attested credential data is
//!
//! ```text
//! aaguid (16) | credentialIdLength (2, big-endian) | credentialId | credentialPublicKey (COSE)
//! ```
//!
//! The three checks ADR-0115 §4.3 names — RP-ID hash, user
//! verification, sign-count regression — all read fields parsed here.

use sha2::{Digest, Sha256};

use super::cose::{CoseError, Es256PublicKey};

/// `UP` — user present.
pub const FLAG_UP: u8 = 0x01;
/// `UV` — user verified (the biometric/passcode gate actually fired).
/// ADR-0115 §4.3 requires `userVerification: required`, so this bit
/// being clear is a refusal, not a downgrade.
pub const FLAG_UV: u8 = 0x04;
/// `AT` — attested credential data present (registration only).
pub const FLAG_AT: u8 = 0x40;

#[derive(Debug, thiserror::Error)]
pub enum AuthDataError {
    #[error("authenticator data is {len} bytes, shorter than the 37-byte minimum")]
    TooShort { len: usize },
    #[error("authenticator data claims a {len}-byte credential id that runs past the buffer")]
    CredentialIdOverrun { len: usize },
    #[error("attested credential data is absent but this is a registration")]
    MissingAttestedData,
    #[error("credential public key: {0}")]
    Cose(#[from] CoseError),
}

/// Parsed authenticator data.
#[derive(Debug, Clone)]
pub struct AuthenticatorData {
    pub rp_id_hash: [u8; 32],
    pub flags: u8,
    pub sign_count: u32,
    /// Present iff the `AT` flag was set.
    pub credential_id: Option<Vec<u8>>,
    /// Present iff the `AT` flag was set.
    pub credential_public_key: Option<Es256PublicKey>,
}

impl AuthenticatorData {
    /// Parse. `raw` is the exact bytes the authenticator signed over.
    pub fn parse(raw: &[u8]) -> Result<Self, AuthDataError> {
        if raw.len() < 37 {
            return Err(AuthDataError::TooShort { len: raw.len() });
        }
        let mut rp_id_hash = [0u8; 32];
        rp_id_hash.copy_from_slice(&raw[0..32]);
        let flags = raw[32];
        let sign_count = u32::from_be_bytes([raw[33], raw[34], raw[35], raw[36]]);

        let (credential_id, credential_public_key) = if flags & FLAG_AT != 0 {
            // aaguid(16) + credIdLen(2) starts at 37.
            if raw.len() < 55 {
                return Err(AuthDataError::TooShort { len: raw.len() });
            }
            let id_len = u16::from_be_bytes([raw[53], raw[54]]) as usize;
            let id_start: usize = 55;
            let id_end = id_start
                .checked_add(id_len)
                .ok_or(AuthDataError::CredentialIdOverrun { len: id_len })?;
            if id_end > raw.len() {
                return Err(AuthDataError::CredentialIdOverrun { len: id_len });
            }
            let id = raw[id_start..id_end].to_vec();
            // The COSE key is the next CBOR item; extensions may follow
            // it, so we decode from the remaining slice and let the
            // CBOR decoder find the item boundary.
            let key = Es256PublicKey::from_cose_cbor(&raw[id_end..])?;
            (Some(id), Some(key))
        } else {
            (None, None)
        };

        Ok(Self {
            rp_id_hash,
            flags,
            sign_count,
            credential_id,
            credential_public_key,
        })
    }

    /// `true` iff this data was produced for `rp_id`.
    ///
    /// This is the origin binding that makes a passkey unphishable
    /// (ADR-0115 §G3): an assertion minted for a look-alike host hashes
    /// to a different value here and verifies against nothing.
    #[must_use]
    pub fn rp_id_matches(&self, rp_id: &str) -> bool {
        let expected = Sha256::digest(rp_id.as_bytes());
        aberp_portal_core::ct::eq(&self.rp_id_hash, expected.as_slice())
    }

    #[must_use]
    pub fn user_present(&self) -> bool {
        self.flags & FLAG_UP != 0
    }

    #[must_use]
    pub fn user_verified(&self) -> bool {
        self.flags & FLAG_UV != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(rp_id: &str, flags: u8, count: u32) -> Vec<u8> {
        let mut v = Sha256::digest(rp_id.as_bytes()).to_vec();
        v.push(flags);
        v.extend_from_slice(&count.to_be_bytes());
        v
    }

    #[test]
    fn parses_an_assertion_shaped_buffer() {
        let raw = header("portal.example", FLAG_UP | FLAG_UV, 5);
        let d = AuthenticatorData::parse(&raw).expect("parses");
        assert!(d.rp_id_matches("portal.example"));
        assert!(!d.rp_id_matches("portal.example.evil"));
        assert!(d.user_present() && d.user_verified());
        assert_eq!(d.sign_count, 5);
        assert!(d.credential_id.is_none());
    }

    #[test]
    fn refuses_a_truncated_buffer() {
        assert!(matches!(
            AuthenticatorData::parse(&[0u8; 36]),
            Err(AuthDataError::TooShort { len: 36 })
        ));
    }

    #[test]
    fn refuses_an_attested_length_that_runs_past_the_buffer() {
        // AT set, credentialIdLength claims 4096 with nothing behind it:
        // the classic length-field overrun.
        let mut raw = header("portal.example", FLAG_UP | FLAG_UV | FLAG_AT, 0);
        raw.extend_from_slice(&[0u8; 16]); // aaguid
        raw.extend_from_slice(&4096u16.to_be_bytes());
        assert!(matches!(
            AuthenticatorData::parse(&raw),
            Err(AuthDataError::CredentialIdOverrun { len: 4096 })
        ));
    }

    #[test]
    fn user_verified_is_false_when_only_presence_was_signalled() {
        // A tap without a biometric. §4.3 requires UV, so the caller
        // must be able to tell these apart.
        let raw = header("portal.example", FLAG_UP, 0);
        let d = AuthenticatorData::parse(&raw).expect("parses");
        assert!(d.user_present());
        assert!(!d.user_verified());
    }
}
