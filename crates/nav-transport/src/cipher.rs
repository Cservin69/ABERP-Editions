//! AES-128/ECB decryption of NAV's `exchangeToken` envelope.
//!
//! # Why ECB, here, and only here
//!
//! Electronic Code Book is a footgun in every other context: identical
//! plaintext blocks produce identical ciphertext blocks, leaking
//! structural information. ABERP would never choose ECB. **NAV chose it
//! for us.** Per ADR-0020 §2 and ADR-0021 §A9 (adversarial-review bullet
//! 3), the NAV Online Számla v3.0 `tokenExchange` flow returns a
//! 16-byte-aligned ciphertext that the client MUST AES-128/ECB-decrypt
//! with the tenant's `xmlChangeKey`. There is no alternative on offer;
//! ABERP's posture toward NAV is upper-bounded by what NAV exposes (the
//! framing constraint from ADR-0020 §"Forward stance").
//!
//! Two structural constraints from ADR-0021 §A9 are honoured here:
//!
//!   1. **One adapter, one call site.** AES-128/ECB appears in exactly
//!      one place in the workspace: [`decrypt_exchange_token`] below.
//!      No other module imports `aes::Aes128`. A future conformance
//!      check (named in ADR-0021 §A9 adversarial-review bullet 3) can
//!      grep for additional call sites and fail.
//!   2. **Call-site comment naming the constraint.** Per ADR-0021 §A9:
//!      *"a call-site comment is required"*. This file IS that comment;
//!      the doc-paragraph above and the block-level comment inside the
//!      function body restate the constraint so a future contributor
//!      reading the implementation cannot miss it.
//!
//! # What this module does NOT do
//!
//!   - It does not encrypt anything (NAV does not require ABERP to
//!     encrypt anything toward NAV).
//!   - It does not pad / unpad (NAV's plaintext token is a known-shape
//!     ASCII string; the decrypted output is returned verbatim; the
//!     caller in `crate::operations::token_exchange` trims any trailing
//!     PKCS#7-style padding bytes if present per the NAV behaviour
//!     observed in the consulted clients).
//!   - It does not perform key derivation. The 16-byte AES-128 key IS
//!     the tenant's `xmlChangeKey` byte-for-byte (NAV ships a 16-byte
//!     printable-ASCII key per technical user; the printable bytes ARE
//!     the AES key material).
//!
//! # Why no `Zeroizing` wrapper on the output
//!
//! The decrypted token IS a secret (NAV invalidates a leaked token; an
//! attacker with the token can impersonate the technical user for the
//! token's lifetime). The caller (`crate::operations::token_exchange`)
//! wraps the decoded UTF-8 string in `Zeroizing<String>` before
//! returning it up the stack. This module returns a `Vec<u8>` so the
//! caller can choose its own wrapper shape; passing a `Zeroizing` in/out
//! at this layer would force every caller through the same wrapper
//! choice, which would be the wrong constraint to bake in.

use aes::cipher::{generic_array::GenericArray, BlockDecrypt as _, KeyInit as _};
use aes::Aes128;

use crate::error::NavTransportError;

/// AES block size in bytes. Pinned as a `const` so the call site reads
/// like the spec rather than like a magic number.
const AES_BLOCK_SIZE: usize = 16;

/// AES-128 key size in bytes. NAV ships 16-byte `xmlChangeKey` values
/// per technical user; the printable-ASCII bytes ARE the AES-128 key
/// material (no derivation, no encoding step on the key itself).
const AES128_KEY_SIZE: usize = 16;

/// Decrypt a NAV exchangeToken ciphertext with the tenant's
/// `xmlChangeKey`. The ciphertext bytes are the raw output of base64-
/// decoding NAV's `<encodedExchangeToken>` element; the caller in
/// `crate::operations::token_exchange` performs that decode.
///
/// Loud-fails on:
///
///   - `ciphertext.len() == 0` — NAV always returns a non-empty
///     ciphertext on a successful response; an empty buffer is the
///     parser pulled the wrong field (e.g., a `<message>` instead of
///     `<encodedExchangeToken>`).
///   - `ciphertext.len() % 16 != 0` — AES-128/ECB operates on whole
///     blocks; an unaligned input is malformed.
///   - `change_key.len() != 16` — NAV's keys are 16-byte ASCII. A key
///     of any other length means the keychain item is malformed (or
///     the operator populated the wrong artifact; the loud failure
///     surfaces the keychain hygiene problem rather than masking it).
///
/// **The block-by-block ECB decrypt loop is the protocol-imposed shape
/// per ADR-0020 §2; do not refactor toward CBC or GCM here. ECB is the
/// only mode NAV accepts on this surface.**
pub fn decrypt_exchange_token(
    change_key: &[u8],
    ciphertext: &[u8],
) -> Result<Vec<u8>, NavTransportError> {
    if change_key.len() != AES128_KEY_SIZE {
        return Err(NavTransportError::TokenExchangeDecryptFailed(format!(
            "xmlChangeKey length is {} (expected {})",
            change_key.len(),
            AES128_KEY_SIZE
        )));
    }
    if ciphertext.is_empty() {
        return Err(NavTransportError::TokenExchangeBadCiphertextLength { len: 0 });
    }
    if !ciphertext.len().is_multiple_of(AES_BLOCK_SIZE) {
        return Err(NavTransportError::TokenExchangeBadCiphertextLength {
            len: ciphertext.len(),
        });
    }

    // Protocol-imposed ECB per ADR-0020 §2 + ADR-0021 §A9.
    // Do not generalize; do not switch to a chained mode here.
    let key_array = GenericArray::from_slice(change_key);
    let cipher = Aes128::new(key_array);

    let mut out = Vec::with_capacity(ciphertext.len());
    let mut block = GenericArray::<u8, aes::cipher::typenum::U16>::default();
    for chunk in ciphertext.chunks_exact(AES_BLOCK_SIZE) {
        block.copy_from_slice(chunk);
        cipher.decrypt_block(&mut block);
        out.extend_from_slice(block.as_slice());
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encrypt a single block with AES-128 so we have a known-good
    /// ciphertext to feed `decrypt_exchange_token`. We only ever
    /// exercise the decrypt path in production; the encrypt helper
    /// stays scoped to tests so a future contributor cannot accidentally
    /// reach for it.
    fn encrypt_blocks(key: &[u8; 16], plaintext: &[u8]) -> Vec<u8> {
        use aes::cipher::BlockEncrypt as _;
        let key_array = GenericArray::from_slice(key);
        let cipher = Aes128::new(key_array);
        let mut out = Vec::with_capacity(plaintext.len());
        let mut block = GenericArray::<u8, aes::cipher::typenum::U16>::default();
        for chunk in plaintext.chunks_exact(16) {
            block.copy_from_slice(chunk);
            cipher.encrypt_block(&mut block);
            out.extend_from_slice(block.as_slice());
        }
        out
    }

    #[test]
    fn round_trips_a_single_block() {
        let key = *b"0123456789ABCDEF";
        let plaintext = *b"NAV-TOKEN-016BYT";
        let ciphertext = encrypt_blocks(&key, &plaintext);
        let decrypted = decrypt_exchange_token(&key, &ciphertext).expect("decrypt");
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn round_trips_multiple_blocks() {
        let key = *b"0123456789ABCDEF";
        // 48 bytes = 3 blocks exactly. NAV's tokens are typically
        // 16–32 ASCII characters but we exercise three blocks here to
        // surface any off-by-one in the loop bounds.
        let plaintext = *b"FIRST-BLOCK-0000SECOND-BLOCK-000THIRD-BLOCK-0000";
        let ciphertext = encrypt_blocks(&key, &plaintext);
        let decrypted = decrypt_exchange_token(&key, &ciphertext).expect("decrypt");
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn rejects_empty_ciphertext() {
        let key = *b"0123456789ABCDEF";
        let err = decrypt_exchange_token(&key, &[]).expect_err("empty ciphertext loud-fails");
        assert!(matches!(
            err,
            NavTransportError::TokenExchangeBadCiphertextLength { len: 0 }
        ));
    }

    #[test]
    fn rejects_unaligned_ciphertext() {
        let key = *b"0123456789ABCDEF";
        // 17 bytes — one block plus a stray byte. AES-128 cannot
        // decrypt this; truncating to 16 would silently corrupt the
        // token. Loud-fail is the contract.
        let bad = vec![0u8; 17];
        let err = decrypt_exchange_token(&key, &bad).expect_err("unaligned loud-fails");
        assert!(matches!(
            err,
            NavTransportError::TokenExchangeBadCiphertextLength { len: 17 }
        ));
    }

    #[test]
    fn rejects_wrong_length_key() {
        // NAV always ships 16-byte keys; a 15- or 17-byte value in the
        // keychain means the operator populated the wrong artifact
        // (e.g., the sign key in the change-key slot — they look similar
        // and have similar lengths).
        let short_key = b"0123456789ABCDE";
        let block = vec![0u8; 16];
        let err =
            decrypt_exchange_token(short_key, &block).expect_err("wrong key length loud-fails");
        match err {
            NavTransportError::TokenExchangeDecryptFailed(msg) => {
                assert!(
                    msg.contains("length is 15"),
                    "diagnostic should name the bad length, got: {msg}"
                );
            }
            other => panic!("expected TokenExchangeDecryptFailed, got {other:?}"),
        }
    }

    #[test]
    fn wrong_key_returns_garbage_not_error() {
        // AES-128/ECB does not authenticate; decryption with the wrong
        // key produces garbage bytes, not an error. The caller in
        // `crate::operations::token_exchange` defends downstream by
        // checking the decoded token shape (UTF-8 ASCII, reasonable
        // length). This test pins that contract so a future contributor
        // does not add a "did it look right?" check inside this module.
        let key = *b"0123456789ABCDEF";
        let wrong_key = *b"FEDCBA9876543210";
        let plaintext = *b"NAV-TOKEN-016BYT";
        let ciphertext = encrypt_blocks(&key, &plaintext);
        let decrypted = decrypt_exchange_token(&wrong_key, &ciphertext).expect("decrypt ok");
        assert_ne!(
            decrypted, plaintext,
            "wrong key should not magically recover the plaintext"
        );
    }
}
