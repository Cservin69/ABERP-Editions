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
//!   - It strips PKCS#7 padding after decryption (NAV's tokenExchange
//!     response ciphertext is block-aligned with PKCS#7 padding).
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
    // `as_chunks::<AES_BLOCK_SIZE>()` yields exactly the 16-byte blocks
    // `chunks_exact(AES_BLOCK_SIZE)` yielded, in the same order; the
    // multiple-of-16 guard above makes the remainder provably empty, so
    // no input byte is dropped. Shape unchanged — still block-by-block
    // ECB per ADR-0020 §2 / ADR-0021 §A9.
    let (blocks, remainder) = ciphertext.as_chunks::<AES_BLOCK_SIZE>();
    debug_assert!(
        remainder.is_empty(),
        "length guard above guarantees a whole number of AES blocks"
    );
    for chunk in blocks {
        block.copy_from_slice(chunk);
        cipher.decrypt_block(&mut block);
        out.extend_from_slice(block.as_slice());
    }

    // Strip PKCS#7 padding if present. NAV's tokenExchange response
    // ciphertext is padded to AES block boundaries; the padding byte
    // value equals the number of padding bytes. A trailing byte of
    // 0x10 means 16 padding bytes (a full block). If stripping would
    // remove the entire payload, return empty (malformed padding).
    let pad_len = out.last().copied().unwrap_or(0) as usize;
    if pad_len > 0 && pad_len <= AES_BLOCK_SIZE {
        let data_len = out.len().saturating_sub(pad_len);
        if data_len > 0 && out[data_len..].iter().all(|&b| b == pad_len as u8) {
            out.truncate(data_len);
        }
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
        for chunk in plaintext.as_chunks::<16>().0 {
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

    /// Known-answer byte-equivalence pin for the ECB block loop.
    ///
    /// The round-trip tests above route BOTH directions through the
    /// block loops in this file, so they cannot distinguish "the loop is
    /// correct" from "the loop is wrong the same way twice". These two
    /// tests pin decrypt output against ciphertext computed OUTSIDE this
    /// crate, so any change to the loop's chunking — block size, block
    /// count, or block ORDER — shows up as a byte diff.
    ///
    /// Vector: FIPS-197 §C.1, AES-128.
    ///   key        000102030405060708090a0b0c0d0e0f
    ///   plaintext  00112233445566778899aabbccddeeff
    ///   ciphertext 69c4e0d86a7b0430d8cdb78070b4c55a
    ///
    /// Neither plaintext ends in a byte ≤ 16, so the PKCS#7 strip below
    /// the loop is a no-op here and the assertion sees the raw AES
    /// output.
    #[test]
    fn decrypts_fips197_known_answer_vector() {
        let key: [u8; 16] = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f,
        ];
        let ciphertext: [u8; 16] = [
            0x69, 0xc4, 0xe0, 0xd8, 0x6a, 0x7b, 0x04, 0x30, 0xd8, 0xcd, 0xb7, 0x80, 0x70, 0xb4,
            0xc5, 0x5a,
        ];
        let expected: [u8; 16] = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        let got = decrypt_exchange_token(&key, &ciphertext).expect("decrypt");
        assert_eq!(
            got, expected,
            "FIPS-197 C.1 AES-128 vector must decrypt exactly"
        );
    }

    /// Two-block KAT with DISTINCT blocks, so a loop that reversed,
    /// duplicated, or dropped a block cannot pass. Block 2 is the ASCII
    /// token `NAV-TOKEN-016BYT` under the same key; its ECB ciphertext
    /// is 3685ffd03dd80a9617d2be0d104bd885.
    #[test]
    fn decrypts_two_block_known_answer_vector_in_order() {
        let key: [u8; 16] = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f,
        ];
        let ciphertext: [u8; 32] = [
            // block 1 -> 00112233445566778899aabbccddeeff
            0x69, 0xc4, 0xe0, 0xd8, 0x6a, 0x7b, 0x04, 0x30, 0xd8, 0xcd, 0xb7, 0x80, 0x70, 0xb4,
            0xc5, 0x5a, // block 2 -> b"NAV-TOKEN-016BYT"
            0x36, 0x85, 0xff, 0xd0, 0x3d, 0xd8, 0x0a, 0x96, 0x17, 0xd2, 0xbe, 0x0d, 0x10, 0x4b,
            0xd8, 0x85,
        ];
        let mut expected = vec![
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        expected.extend_from_slice(b"NAV-TOKEN-016BYT");
        let got = decrypt_exchange_token(&key, &ciphertext).expect("decrypt");
        assert_eq!(got, expected, "both blocks must decrypt, in input order");
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
