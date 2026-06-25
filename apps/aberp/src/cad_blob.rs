//! S430 / ADR-0083 — CAD-blob encryption-at-rest + read-audit.
//!
//! ## S2 / ADR-0093 — storefront reach is gated upstream
//!
//! This module is LOCAL at-rest crypto, not itself a network surface. The CAD
//! DOWNLOAD whose bytes it protects happens in
//! [`crate::quote_pricing_pipeline`], whose daemon is spawned ONLY in a
//! Defense build ([`crate::build_profile::storefront_polling_allowed`] / the
//! [`crate::serve`] boot guard). A Portable build never pulls customer CAD off
//! abenerp.com, so a storefront-sourced blob is never produced there.
//!
//! # Why
//!
//! The auto-quote pricing pipeline downloads a customer's CAD file
//! (`.stl` / `.step`) from the storefront and lands it on the local
//! filesystem under `artifact_dir/<quote_id>/<filename>` (see
//! [`crate::quote_pricing_pipeline`]). Pre-S430 those bytes sat on disk
//! in the clear — a customer's proprietary geometry, readable by anyone
//! with filesystem access. ADR-0067 §"blob AES-GCM at rest, audit on
//! read" promised encryption; this module delivers it.
//!
//! # Shape (kept deliberately small — CLAUDE.md rule 2/13)
//!
//! - One AES-256-GCM key per tenant, minted on first boot and stored in
//!   the OS keychain (service `aberp.cad.<tenant>`, item
//!   [`ITEM_CAD_BLOB_KEY`]) — mirrors the NAV-credentials keychain
//!   pattern in `nav-transport::credentials::keychain`.
//! - **New writes** are encrypted: on-disk layout is
//!   `MAGIC(8) || nonce(12) || ciphertext+tag`. The magic header lets a
//!   reader tell an encrypted blob from a legacy plaintext one.
//! - **Reads** try the magic header. Encrypted → decrypt (a flipped bit
//!   fails loudly — GCM is authenticated). Plaintext (no magic) → pass
//!   through unchanged and flag it so the caller can emit
//!   `CadBlobLegacyPlaintextRead` for a later migration sweep. We do NOT
//!   auto-migrate existing plaintext blobs here (future maintenance
//!   task); we only stop accumulating new plaintext.
//! - The Python extractor reads a **file path**, not bytes, so a read
//!   for extraction decrypts to a short-lived sibling temp file
//!   ([`DecryptedTempFile`]) that is deleted on drop.
//!
//! The operator never sees any of this ([[hulye-biztos]]): the file just
//! works. Encryption/decryption is automatic and the read-audit fires on
//! every fetch with no opt-out ([[trust-code-not-operator]]).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aes_gcm::aead::generic_array::GenericArray;
use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Key};
use anyhow::{anyhow, Context, Result};
use base64::Engine as _;
use keyring::Entry;
use serde::Serialize;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use ulid::Ulid;
use zeroize::Zeroizing;

use aberp_audit_ledger::{append_in_tx, Actor, BinaryHash, EventKind, LedgerMeta, TenantId};

/// Magic header marking an ABERP-encrypted CAD blob (v1). 8 bytes, chosen
/// so it cannot be confused with a real CAD file: ASCII STL starts with
/// `solid`, binary STL with an arbitrary 80-byte header, STEP with
/// `ISO-10303-21`. A legacy plaintext blob whose first 8 bytes happen to
/// equal this string is astronomically unlikely; the worst case is one
/// misclassified read, never data loss.
pub const MAGIC: &[u8; 8] = b"ABRPCAD1";

/// AES-256 key length in bytes.
const KEY_LEN: usize = 32;

/// AES-GCM nonce length in bytes (96-bit, the GCM standard).
const NONCE_LEN: usize = 12;

/// Read-audit debounce window. A second read of the SAME blob by the SAME
/// requester within this window does not emit a second `CadBlobRead`
/// (avoids audit spam from rapid UI re-renders / retry loops).
const DEBOUNCE_WINDOW: Duration = Duration::from_secs(60);

/// Keychain item name for the per-tenant CAD-blob key. Part of the
/// on-disk operator contract — a rename orphans the operator's existing
/// key, so the value is pinned by a unit test.
pub const ITEM_CAD_BLOB_KEY: &str = "cad_blob_key";

/// Compose the keychain `service` field for a tenant's CAD-blob key.
/// Mirrors `nav-transport`'s `aberp.nav.<tenant>` convention.
pub fn service_name(tenant_id: &str) -> String {
    format!("aberp.cad.{tenant_id}")
}

// ----- key ----------------------------------------------------------

/// A tenant's AES-256-GCM CAD-blob key. Cheap to clone (`Arc`-backed);
/// the 32 secret bytes are zeroized when the last clone drops.
#[derive(Clone)]
pub struct CadBlobKey(Arc<Zeroizing<[u8; KEY_LEN]>>);

impl std::fmt::Debug for CadBlobKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the key material.
        f.write_str("CadBlobKey(<redacted>)")
    }
}

impl CadBlobKey {
    /// Build from raw 32 bytes (used by the keychain loader and by tests).
    pub fn from_bytes(bytes: [u8; KEY_LEN]) -> Self {
        Self(Arc::new(Zeroizing::new(bytes)))
    }

    fn cipher(&self) -> Aes256Gcm {
        Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(self.0.as_slice()))
    }

    /// Encrypt `plaintext` into the on-disk wire form
    /// `MAGIC || nonce || ciphertext+tag`. A fresh random nonce is minted
    /// per call (never reused under the same key).
    pub fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ciphertext = self
            .cipher()
            .encrypt(&nonce, plaintext)
            .map_err(|e| anyhow!("AES-256-GCM encrypt failed: {e}"))?;
        let mut out = Vec::with_capacity(MAGIC.len() + NONCE_LEN + ciphertext.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(nonce.as_slice());
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    /// Decrypt an on-disk blob, OR pass a legacy plaintext blob through
    /// unchanged.
    ///
    /// - Starts with [`MAGIC`] → authenticated-decrypt. A tampered or
    ///   truncated blob, or the wrong key, fails loudly with `Err` (GCM
    ///   tag mismatch) — CLAUDE.md rule 12.
    /// - No magic → the blob predates S430; return its bytes verbatim and
    ///   flag `was_legacy_plaintext = true` so the caller can audit it.
    pub fn open(&self, on_disk: &[u8]) -> Result<OpenedBlob> {
        if on_disk.len() < MAGIC.len() || &on_disk[..MAGIC.len()] != MAGIC {
            return Ok(OpenedBlob {
                plaintext: on_disk.to_vec(),
                was_legacy_plaintext: true,
            });
        }
        let rest = &on_disk[MAGIC.len()..];
        if rest.len() < NONCE_LEN {
            return Err(anyhow!(
                "encrypted CAD blob truncated: {} bytes after magic, need >= {NONCE_LEN}",
                rest.len()
            ));
        }
        let (nonce_bytes, ciphertext) = rest.split_at(NONCE_LEN);
        // `decrypt`'s signature fixes the nonce size (U12), so the
        // GenericArray length is inferred here.
        let nonce = GenericArray::from_slice(nonce_bytes);
        let plaintext = self
            .cipher()
            .decrypt(nonce, ciphertext)
            .map_err(|e| anyhow!("AES-256-GCM decrypt failed (tampered blob or wrong key): {e}"))?;
        Ok(OpenedBlob {
            plaintext,
            was_legacy_plaintext: false,
        })
    }
}

/// Result of [`CadBlobKey::open`].
pub struct OpenedBlob {
    pub plaintext: Vec<u8>,
    /// `true` when the on-disk blob had no magic header (pre-S430
    /// plaintext); the caller should emit `CadBlobLegacyPlaintextRead`.
    pub was_legacy_plaintext: bool,
}

impl std::fmt::Debug for OpenedBlob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Redact the plaintext geometry; only the length + legacy flag.
        f.debug_struct("OpenedBlob")
            .field("plaintext_len", &self.plaintext.len())
            .field("was_legacy_plaintext", &self.was_legacy_plaintext)
            .finish()
    }
}

// ----- keychain provisioning ----------------------------------------

/// Whether [`load_or_provision_key`] read an existing key or minted a
/// fresh one. A `Minted` outcome means the caller must emit
/// `CadBlobKeyProvisioned`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyProvision {
    Loaded,
    Minted,
}

/// Load the tenant's CAD-blob key from the keychain, or mint + store a
/// fresh AES-256 key on first boot.
///
/// Fail-loud (rule 12): a backend error (locked keychain, permission
/// denied) propagates; there is no silent fallback to an unencrypted
/// path. A malformed stored key (wrong length / bad base64) is also an
/// error rather than a silent re-mint, because re-minting would orphan
/// every blob the old key encrypted.
pub fn load_or_provision_key(tenant_id: &str) -> Result<(CadBlobKey, KeyProvision)> {
    let service = service_name(tenant_id);
    let entry =
        Entry::new(&service, ITEM_CAD_BLOB_KEY).context("open CAD-blob-key keychain entry")?;
    match entry.get_password() {
        Ok(b64) => {
            let raw = base64::engine::general_purpose::STANDARD
                .decode(b64.trim())
                .context("decode stored CAD-blob key (base64)")?;
            let bytes: [u8; KEY_LEN] = raw.as_slice().try_into().map_err(|_| {
                anyhow!(
                    "stored CAD-blob key for tenant {tenant_id} is {} bytes, expected {KEY_LEN} \
                     — refusing to re-mint (would orphan existing encrypted blobs)",
                    raw.len()
                )
            })?;
            Ok((CadBlobKey::from_bytes(bytes), KeyProvision::Loaded))
        }
        Err(keyring::Error::NoEntry) => {
            let key_arr = Aes256Gcm::generate_key(&mut OsRng);
            let bytes: [u8; KEY_LEN] = key_arr.into();
            let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
            entry
                .set_password(&b64)
                .context("store freshly-minted CAD-blob key in keychain")?;
            Ok((CadBlobKey::from_bytes(bytes), KeyProvision::Minted))
        }
        Err(other) => Err(anyhow!(
            "CAD-blob-key keychain backend error for tenant {tenant_id}: {other}"
        )),
    }
}

// ----- read purpose + debounce --------------------------------------

/// Why a CAD blob was fetched. Carried verbatim on the `CadBlobRead`
/// audit payload (closed vocabulary — the audit string set is stable so
/// no payload migration is needed when a new fetch site lands).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadPurpose {
    /// Operator opened the CAD for visual preview.
    Preview,
    /// The quote engine read the geometry to (re-)price the job.
    Reprice,
    /// A customer downloaded their own CAD via the storefront relay.
    CustomerDownload,
    /// An automated revalidation pass re-read the geometry.
    SystemRevalidate,
}

impl ReadPurpose {
    pub fn as_str(self) -> &'static str {
        match self {
            ReadPurpose::Preview => "preview",
            ReadPurpose::Reprice => "reprice",
            ReadPurpose::CustomerDownload => "customer_download",
            ReadPurpose::SystemRevalidate => "system_revalidate",
        }
    }
}

/// In-memory 60-second read-audit debounce, keyed by `(requester,
/// blob_id)`. Process-lifetime only (cleared on restart — worst case is
/// one extra audited read after a restart, which is correct-but-noisy,
/// not wrong). Cheap to clone (`Arc`-backed).
#[derive(Clone, Default)]
pub struct ReadDebounce(Arc<Mutex<HashMap<(String, String), Instant>>>);

impl ReadDebounce {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `true` if a `CadBlobRead` should be emitted now, recording
    /// `now` as the last-emit instant. Returns `false` if the same
    /// requester read the same blob within [`DEBOUNCE_WINDOW`].
    pub fn should_emit(&self, requester: &str, blob_id: &str, now: Instant) -> bool {
        let mut map = self.0.lock().expect("read-debounce mutex poisoned");
        let key = (requester.to_string(), blob_id.to_string());
        match map.get(&key) {
            Some(&last) if now.duration_since(last) < DEBOUNCE_WINDOW => false,
            _ => {
                map.insert(key, now);
                true
            }
        }
    }
}

/// The per-service CAD-blob context: the tenant key plus the shared
/// read-audit debounce. Threaded onto the pricing pipeline so its write
/// path encrypts and its read path decrypts + audits.
#[derive(Clone, Debug)]
pub struct CadBlobCtx {
    pub key: CadBlobKey,
    pub debounce: ReadDebounce,
}

impl std::fmt::Debug for ReadDebounce {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ReadDebounce(..)")
    }
}

impl CadBlobCtx {
    pub fn new(key: CadBlobKey) -> Self {
        Self {
            key,
            debounce: ReadDebounce::new(),
        }
    }

    /// Deterministic key for tests (no keychain access). NOT for prod.
    #[cfg(test)]
    pub fn with_test_key() -> Self {
        Self::new(CadBlobKey::from_bytes([7u8; KEY_LEN]))
    }
}

// ----- decrypt-to-temp ----------------------------------------------

/// A plaintext CAD file decrypted to disk so the Python extractor (which
/// reads a path, not bytes) can ingest it. Deleted on drop — the
/// plaintext never outlives the extraction.
pub struct DecryptedTempFile {
    path: PathBuf,
}

impl DecryptedTempFile {
    /// Write `plaintext` to a sibling of `encrypted_path` whose filename
    /// is prefixed with `._decrypted_` so it keeps the original extension
    /// (the extractor dispatches STL vs STEP by extension).
    pub fn write_beside(encrypted_path: &Path, plaintext: &[u8]) -> Result<Self> {
        let parent = encrypted_path.parent().unwrap_or_else(|| Path::new("."));
        let fname = encrypted_path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "blob".to_string());
        // Per-extraction unique prefix so concurrent extracts of the same
        // blob don't collide.
        let path = parent.join(format!("._decrypted_{}_{fname}", Ulid::new()));
        std::fs::write(&path, plaintext)
            .with_context(|| format!("write decrypted CAD temp file {}", path.display()))?;
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for DecryptedTempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

// ----- audit payloads + emit helpers --------------------------------

#[derive(Serialize)]
struct CadBlobKeyProvisionedPayload {
    tenant_id: String,
    key_algorithm: &'static str,
    actor: &'static str,
    provisioned_at: String,
    idempotency_key: String,
}

#[derive(Serialize)]
struct CadBlobReadPayload {
    tenant_id: String,
    blob_id: String,
    requester: String,
    purpose: &'static str,
    actor: &'static str,
    read_at: String,
}

#[derive(Serialize)]
struct CadBlobLegacyPlaintextReadPayload {
    tenant_id: String,
    blob_id: String,
    requester: String,
    actor: &'static str,
    read_at: String,
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "unknown".to_string())
}

/// Emit `CadBlobKeyProvisioned` once, when a fresh key is minted. The
/// idempotency key makes a re-run (e.g. a double boot before the keychain
/// settles) a no-op rather than a duplicate row.
pub fn emit_key_provisioned(
    conn: &mut duckdb::Connection,
    tenant_id: &str,
    binary_hash: BinaryHash,
    login: &str,
) -> Result<()> {
    let idempotency_key = format!("cad_blob_key_provisioned:{tenant_id}");
    let payload = CadBlobKeyProvisionedPayload {
        tenant_id: tenant_id.to_string(),
        key_algorithm: "AES-256-GCM",
        actor: "system",
        provisioned_at: now_rfc3339(),
        idempotency_key: idempotency_key.clone(),
    };
    emit(
        conn,
        tenant_id,
        binary_hash,
        login,
        EventKind::CadBlobKeyProvisioned,
        &payload,
        Some(idempotency_key),
    )
}

/// Emit `CadBlobRead` for a fetch. NOT idempotent — every fetch is a new
/// fact (the debounce in [`ReadDebounce`] is the only suppression).
pub fn emit_blob_read(
    conn: &mut duckdb::Connection,
    tenant_id: &str,
    binary_hash: BinaryHash,
    login: &str,
    blob_id: &str,
    requester: &str,
    purpose: ReadPurpose,
) -> Result<()> {
    let payload = CadBlobReadPayload {
        tenant_id: tenant_id.to_string(),
        blob_id: blob_id.to_string(),
        requester: requester.to_string(),
        purpose: purpose.as_str(),
        actor: "system",
        read_at: now_rfc3339(),
    };
    emit(
        conn,
        tenant_id,
        binary_hash,
        login,
        EventKind::CadBlobRead,
        &payload,
        None,
    )
}

/// Emit `CadBlobLegacyPlaintextRead` — a blob with no magic header was
/// read, so a future migration sweep should re-encrypt it.
pub fn emit_legacy_plaintext_read(
    conn: &mut duckdb::Connection,
    tenant_id: &str,
    binary_hash: BinaryHash,
    login: &str,
    blob_id: &str,
    requester: &str,
) -> Result<()> {
    let payload = CadBlobLegacyPlaintextReadPayload {
        tenant_id: tenant_id.to_string(),
        blob_id: blob_id.to_string(),
        requester: requester.to_string(),
        actor: "system",
        read_at: now_rfc3339(),
    };
    emit(
        conn,
        tenant_id,
        binary_hash,
        login,
        EventKind::CadBlobLegacyPlaintextRead,
        &payload,
        None,
    )
}

fn emit<P: Serialize>(
    conn: &mut duckdb::Connection,
    tenant_id: &str,
    binary_hash: BinaryHash,
    login: &str,
    kind: EventKind,
    payload: &P,
    idempotency_key: Option<String>,
) -> Result<()> {
    let tx = conn.transaction().context("open cad-blob-audit tx")?;
    let meta = LedgerMeta::new(TenantId::new(tenant_id).context("tenant id")?, binary_hash);
    let actor = Actor::from_local_cli(Ulid::new().to_string(), login);
    let bytes = serde_json::to_vec(payload).context("encode cad-blob audit payload")?;
    append_in_tx(&tx, &meta, kind, bytes, actor, idempotency_key)
        .context("append cad-blob audit row")?;
    tx.commit().context("commit cad-blob-audit")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> CadBlobKey {
        CadBlobKey::from_bytes([42u8; KEY_LEN])
    }

    #[test]
    fn keychain_item_name_is_stable() {
        assert_eq!(ITEM_CAD_BLOB_KEY, "cad_blob_key");
        assert_eq!(service_name("acme"), "aberp.cad.acme");
    }

    #[test]
    fn round_trip_encrypt_decrypt_preserves_bytes_and_length() {
        let plaintext = b"solid cube\n  facet normal 0 0 1\nendsolid".to_vec();
        let blob = key().encrypt(&plaintext).expect("encrypt");
        // Encrypted form carries the magic header + nonce + tag overhead.
        assert_eq!(&blob[..MAGIC.len()], MAGIC);
        assert!(blob.len() > plaintext.len() + NONCE_LEN);
        let opened = key().open(&blob).expect("decrypt");
        assert!(!opened.was_legacy_plaintext);
        assert_eq!(opened.plaintext, plaintext);
        assert_eq!(opened.plaintext.len(), plaintext.len());
    }

    #[test]
    fn nonce_is_unique_per_encrypt() {
        let pt = b"same plaintext";
        let a = key().encrypt(pt).unwrap();
        let b = key().encrypt(pt).unwrap();
        // Different nonce ⇒ different ciphertext for identical input.
        assert_ne!(a, b, "two encryptions of the same bytes must differ");
    }

    #[test]
    fn tamper_one_bit_in_ciphertext_fails_decryption() {
        let plaintext = b"proprietary geometry".to_vec();
        let mut blob = key().encrypt(&plaintext).expect("encrypt");
        // Flip a bit in the ciphertext body (past magic + nonce).
        let last = blob.len() - 1;
        blob[last] ^= 0x01;
        let err = key().open(&blob).expect_err("tampered blob must fail");
        assert!(err.to_string().contains("decrypt failed"), "got: {err}");
    }

    #[test]
    fn wrong_key_fails_decryption() {
        let blob = key().encrypt(b"secret").unwrap();
        let other = CadBlobKey::from_bytes([99u8; KEY_LEN]);
        assert!(other.open(&blob).is_err(), "wrong key must not decrypt");
    }

    #[test]
    fn legacy_plaintext_without_magic_passes_through() {
        let legacy = b"ISO-10303-21;\nHEADER;\n".to_vec();
        let opened = key().open(&legacy).expect("legacy passthrough");
        assert!(opened.was_legacy_plaintext);
        assert_eq!(opened.plaintext, legacy);
    }

    #[test]
    fn read_purpose_strings_are_closed_vocab() {
        assert_eq!(ReadPurpose::Preview.as_str(), "preview");
        assert_eq!(ReadPurpose::Reprice.as_str(), "reprice");
        assert_eq!(ReadPurpose::CustomerDownload.as_str(), "customer_download");
        assert_eq!(ReadPurpose::SystemRevalidate.as_str(), "system_revalidate");
    }

    #[test]
    fn debounce_suppresses_second_read_within_window() {
        let d = ReadDebounce::new();
        let t0 = Instant::now();
        assert!(d.should_emit("ervin", "q1", t0), "first read emits");
        assert!(
            !d.should_emit("ervin", "q1", t0 + Duration::from_secs(30)),
            "second read within 60s is debounced"
        );
        assert!(
            d.should_emit("ervin", "q1", t0 + Duration::from_secs(61)),
            "read after 60s emits again"
        );
        // A different requester or blob is a distinct key.
        assert!(d.should_emit("other", "q1", t0 + Duration::from_secs(30)));
        assert!(d.should_emit("ervin", "q2", t0 + Duration::from_secs(30)));
    }

    #[test]
    fn decrypted_temp_file_keeps_extension_and_deletes_on_drop() {
        let dir = std::env::temp_dir().join(format!("aberp-cadtmp-{}", Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let enc = dir.join("bracket.step");
        let path = {
            let tmp = DecryptedTempFile::write_beside(&enc, b"plaintext").unwrap();
            let p = tmp.path().to_path_buf();
            assert!(p.exists());
            assert!(
                p.to_string_lossy().ends_with(".step"),
                "temp file must keep .step extension, got {}",
                p.display()
            );
            p
        };
        // Dropped — file gone.
        assert!(!path.exists(), "temp plaintext must be deleted on drop");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
