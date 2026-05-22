//! Bundle archive reading + parsing (PR-22, ADR-0035 §3 checks 1-6
//! + §4 NAV-XML extraction).
//!
//! Owns the structural read-side: takes the `.tar.zst` path, decompresses
//! + untars it into in-memory blobs, and parses the manifest +
//! chain.jsonl into typed values. The §3 INVARIANTS that operate on
//! these values live in `crate::verify`; this module's only job is
//! "give me typed bytes I can check."
//!
//! # Failure mode
//!
//! Per ADR-0035: structural failures (file-not-found, malformed
//! archive bytes, missing manifest, manifest fails JSON parse) bubble
//! up as `Err(_)` from `read_archive`. Per-entry decode failures
//! (chain.jsonl line that fails JSON, hex/base64 decode failure on
//! a hash or payload, RFC3339 parse failure on time_wall, unknown
//! EventKind storage string) surface as `Err(_)` from
//! `parse_chain_jsonl` — the verifier cannot continue past a
//! malformed entry shape per CLAUDE.md rule 12.
//!
//! Semantic failures (chain link broken, hash mismatch, root
//! element wrong) are NOT this module's concern; they surface as
//! FAIL entries on the [`crate::report::Report`] from
//! `crate::verify::run_checks`.

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;

use aberp_audit_ledger::{
    Actor, BinaryHash, Entry, EntryHash, EntryId, EventKind, Sequence, TenantId,
};
use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;
use serde::Deserialize;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// Internal top-level directory inside the archive per ADR-0029 §3.
/// The verifier asserts every archive entry's path starts with
/// `"bundle/"` (see `Archive::extract_paths`).
const BUNDLE_DIR: &str = "bundle";

/// Manifest schema version this verifier understands per ADR-0029 §3
/// + ADR-0035 §3 check 2. A bundle whose `version` field differs
/// FAILs the manifest-version check with a forward-compatibility
/// note ("a newer aberp-verify may understand this bundle").
pub const SUPPORTED_MANIFEST_VERSION: u32 = 1;

/// In-memory representation of the unpacked bundle. The whole bundle
/// is held in memory because the verifier walks it multiple times
/// (once per ADR-0035 §3 check) and the bundle size is bounded at
/// the per-invoice-slice level per ADR-0029 §3.
#[derive(Debug)]
pub struct Archive {
    /// Raw bytes of `bundle/manifest.json`.
    pub manifest_bytes: Vec<u8>,
    /// Raw bytes of `bundle/chain.jsonl`.
    pub chain_jsonl_bytes: Vec<u8>,
    /// Map of archive-relative path (without the `bundle/` prefix)
    /// to verbatim bytes for every `bundle/nav/<file>.xml` entry.
    /// Keyed by the `nav/...` portion so a lookup keyed on
    /// "nav/00012_invoice_submission_attempt.xml" matches.
    pub nav_files: HashMap<String, Vec<u8>>,
    /// Every entry path observed inside the archive, in iteration
    /// order. The verifier asserts every path starts with
    /// `"bundle/"` (ADR-0029 §3 — single top-level directory).
    pub all_paths: Vec<String>,
}

/// Decompress + untar the bundle file at `path` into an [`Archive`].
/// Loud-fails on:
///   - file-not-found / unreadable.
///   - zstd decompression failure.
///   - tar parse failure.
///   - missing `bundle/manifest.json` or `bundle/chain.jsonl`.
pub fn read_archive(path: &Path) -> Result<Archive> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("read bundle file at {}", path.display()))?;
    let decoded = zstd::stream::decode_all(bytes.as_slice())
        .context("zstd-decompress bundle archive")?;
    let mut ar = tar::Archive::new(decoded.as_slice());

    let mut manifest_bytes: Option<Vec<u8>> = None;
    let mut chain_jsonl_bytes: Option<Vec<u8>> = None;
    let mut nav_files: HashMap<String, Vec<u8>> = HashMap::new();
    let mut all_paths: Vec<String> = Vec::new();

    for entry_result in ar.entries().context("iterate tar entries")? {
        let mut entry = entry_result.context("read tar entry header")?;
        let path = entry
            .path()
            .context("decode tar entry path")?
            .display()
            .to_string();
        all_paths.push(path.clone());
        if !path.starts_with(&format!("{BUNDLE_DIR}/")) {
            bail!(
                "tar entry {path:?} does not live under the {BUNDLE_DIR}/ \
                 top-level directory required by ADR-0029 §3"
            );
        }
        let mut buf = Vec::new();
        entry
            .read_to_end(&mut buf)
            .with_context(|| format!("read bytes of tar entry {path}"))?;
        let rel = path
            .strip_prefix(&format!("{BUNDLE_DIR}/"))
            .expect("checked above");
        match rel {
            "manifest.json" => manifest_bytes = Some(buf),
            "chain.jsonl" => chain_jsonl_bytes = Some(buf),
            _ if rel.starts_with("nav/") => {
                nav_files.insert(rel.to_string(), buf);
            }
            _ => {
                // Unknown archive entry; the §3 check 1 invariant
                // says "manifest + chain.jsonl + nav/*". An entry
                // outside this set is a structural divergence that
                // should fail loud per CLAUDE.md rule 12 (no silent
                // skip of mystery files).
                bail!(
                    "tar entry {path:?} is neither manifest.json, chain.jsonl, \
                     nor under nav/ — bundle shape divergence per ADR-0029 §3"
                );
            }
        }
    }

    let manifest_bytes = manifest_bytes
        .ok_or_else(|| anyhow!("bundle archive missing {BUNDLE_DIR}/manifest.json"))?;
    let chain_jsonl_bytes = chain_jsonl_bytes
        .ok_or_else(|| anyhow!("bundle archive missing {BUNDLE_DIR}/chain.jsonl"))?;

    Ok(Archive {
        manifest_bytes,
        chain_jsonl_bytes,
        nav_files,
        all_paths,
    })
}

/// Manifest shape per ADR-0029 §3 — every field named explicitly so
/// a JSON parser surfaces a missing field (`serde` default-disabled
/// derive raises on absent required fields). The bundle writer's
/// `BundleManifest` shape pins this; PR-22's verifier shape pins it
/// from the other side.
///
/// `serde(deny_unknown_fields)` is deliberately OFF: a future
/// additive manifest field (e.g., the F5-lift PR's `signature_*`
/// block) must NOT cause an older verifier to fail-loud on the
/// field's presence — forward compatibility per ADR-0029 §3.
#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub invoice_id: String,
    pub tenant_id: String,
    pub generated_at: String,
    pub binary_hash: String,
    pub nav_xsd_version: String,
    pub chain_verified: bool,
    pub chain_verified_entries: u64,
    pub entries_in_bundle: u64,
    pub signed: bool,
    pub signature_status: String,
    pub mirror_file_present: bool,
    pub mirror_file_status: String,
}

/// Parse `bundle/manifest.json` bytes into a typed [`Manifest`].
pub fn parse_manifest(bytes: &[u8]) -> Result<Manifest> {
    serde_json::from_slice(bytes).context("parse bundle/manifest.json")
}

/// Per-line shape of `bundle/chain.jsonl` per ADR-0029 §3. Mirrors
/// the bundle writer's `ChainJsonlEntry` — every field present on
/// the wire is mirrored here.
#[derive(Debug, Deserialize)]
pub struct ChainJsonlLine {
    pub id: String,
    pub seq: u64,
    pub prev_hash: String,
    pub time_wall: String,
    pub time_mono: u64,
    pub actor: Actor,
    pub binary_hash: String,
    pub tenant_id: String,
    pub kind: String,
    pub payload: String,
    pub idempotency_key: Option<String>,
    pub entry_hash: String,
}

/// Parse `bundle/chain.jsonl` bytes into a vector of typed lines.
/// One-per-line + trailing newline shape per ADR-0029 §3; the
/// verifier asserts no blank lines, no trailing junk.
pub fn parse_chain_jsonl(bytes: &[u8]) -> Result<Vec<ChainJsonlLine>> {
    let mut lines = Vec::new();
    let text = std::str::from_utf8(bytes).context("chain.jsonl bytes are not UTF-8")?;
    for (idx, raw) in text.lines().enumerate() {
        if raw.is_empty() {
            bail!(
                "chain.jsonl line {} is empty — ADR-0029 §3 requires one entry per line",
                idx + 1
            );
        }
        let line: ChainJsonlLine = serde_json::from_str(raw)
            .with_context(|| format!("parse chain.jsonl line {}", idx + 1))?;
        lines.push(line);
    }
    Ok(lines)
}

/// Reconstruct an [`Entry`] from a parsed [`ChainJsonlLine`].
///
/// Performs the typed decoding the verifier needs before it can call
/// `aberp_audit_ledger::compute_entry_hash`: hex-decodes the hashes,
/// base64-decodes the payload, parses the ULID-prefixed entry id,
/// parses the RFC3339 time_wall, parses the EventKind storage
/// string. Loud-fails on any malformed field per CLAUDE.md rule 12.
///
/// The `tenant_id` is parsed via `TenantId::new` — empty / null-byte
/// values are rejected. The `binary_hash` is decoded into a 32-byte
/// `BinaryHash`. The `entry_hash` and `prev_hash` are decoded into
/// 32-byte `EntryHash` values.
pub fn reconstruct_entry(line: &ChainJsonlLine) -> Result<Entry> {
    let id = parse_entry_id(&line.id)
        .with_context(|| format!("parse entry id {} on chain.jsonl line", line.id))?;
    let seq = Sequence(line.seq);
    let prev_hash = parse_entry_hash(&line.prev_hash)
        .with_context(|| format!("decode prev_hash {} on chain.jsonl line", line.prev_hash))?;
    let entry_hash = parse_entry_hash(&line.entry_hash)
        .with_context(|| format!("decode entry_hash {} on chain.jsonl line", line.entry_hash))?;
    let binary_hash = parse_binary_hash(&line.binary_hash)
        .with_context(|| format!("decode binary_hash {} on chain.jsonl line", line.binary_hash))?;
    let time_wall = OffsetDateTime::parse(&line.time_wall, &Rfc3339)
        .with_context(|| format!("parse time_wall {} as RFC3339", line.time_wall))?;
    let kind = EventKind::from_storage_str(&line.kind).map_err(|e| {
        anyhow!(
            "unknown EventKind storage string {:?} on chain.jsonl line: {}",
            line.kind,
            e
        )
    })?;
    let tenant_id = TenantId::new(line.tenant_id.clone())
        .ok_or_else(|| anyhow!("tenant_id {:?} is empty or contains a null byte", line.tenant_id))?;
    let payload = BASE64_STANDARD
        .decode(&line.payload)
        .with_context(|| format!("base64-decode payload bytes for entry {}", line.id))?;

    Ok(Entry {
        id,
        seq,
        prev_hash,
        time_wall,
        time_mono: line.time_mono,
        actor: line.actor.clone(),
        binary_hash,
        tenant_id,
        kind,
        payload,
        idempotency_key: line.idempotency_key.clone(),
        entry_hash,
    })
}

/// Parse the `aud_<26-char-Crockford-ULID>` prefixed-string form per
/// ADR-0005 into an [`EntryId`].
fn parse_entry_id(s: &str) -> Result<EntryId> {
    let rest = s
        .strip_prefix("aud_")
        .ok_or_else(|| anyhow!("entry id {:?} missing required `aud_` prefix per ADR-0005", s))?;
    let ulid = ulid::Ulid::from_string(rest)
        .map_err(|e| anyhow!("entry id {:?} is not a valid ULID: {}", s, e))?;
    Ok(EntryId(ulid))
}

/// Hex-decode a 32-byte [`EntryHash`] from its lowercase-hex
/// chain.jsonl form. Loud-fails on length / hex-digit errors.
fn parse_entry_hash(s: &str) -> Result<EntryHash> {
    let bytes = hex::decode(s).with_context(|| format!("hex-decode hash {s:?}"))?;
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow!("hash {s:?} is not 32 bytes (SHA-256 width)"))?;
    Ok(EntryHash::from_bytes(arr))
}

/// Hex-decode a 32-byte [`BinaryHash`] from its lowercase-hex
/// chain.jsonl form.
fn parse_binary_hash(s: &str) -> Result<BinaryHash> {
    let bytes = hex::decode(s).with_context(|| format!("hex-decode binary_hash {s:?}"))?;
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow!("binary_hash {s:?} is not 32 bytes"))?;
    Ok(BinaryHash::from_bytes(arr))
}

/// Compose the archive-relative `nav/<seq:05>_<kind>.xml` path the
/// bundle writer used for an entry. Mirrors the writer's
/// `extract_nav_xml` filename composition (dots in the kind storage
/// string are transformed to underscores per ADR-0029 §3).
pub fn nav_archive_path(seq: u64, kind: EventKind) -> String {
    format!("nav/{:05}_{}.xml", seq, kind.as_str().replace('.', "_"))
}

// ──────────────────────────────────────────────────────────────────────
// Tests — manifest + chain.jsonl parse round-trips + entry
// reconstruction + nav-path composition.
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use aberp_audit_ledger::Actor;
    use std::collections::BTreeSet;

    fn fixture_manifest_json() -> Vec<u8> {
        let m = serde_json::json!({
            "version": 1,
            "invoice_id": "inv_TEST",
            "tenant_id": "t1",
            "generated_at": "2026-05-22T00:00:00Z",
            "binary_hash": "00".repeat(32),
            "nav_xsd_version": "3.0",
            "chain_verified": true,
            "chain_verified_entries": 5,
            "entries_in_bundle": 3,
            "signed": false,
            "signature_status": "deferred-per-f5",
            "mirror_file_present": true,
            "mirror_file_status": "verified-agreement",
        });
        serde_json::to_vec(&m).unwrap()
    }

    #[test]
    fn parse_manifest_round_trips_every_adr_0029_field() {
        let bytes = fixture_manifest_json();
        let parsed = parse_manifest(&bytes).unwrap();
        assert_eq!(parsed.version, 1);
        assert_eq!(parsed.invoice_id, "inv_TEST");
        assert_eq!(parsed.tenant_id, "t1");
        assert_eq!(parsed.nav_xsd_version, "3.0");
        assert!(parsed.chain_verified);
        assert_eq!(parsed.chain_verified_entries, 5);
        assert_eq!(parsed.entries_in_bundle, 3);
        assert!(!parsed.signed);
        assert_eq!(parsed.signature_status, "deferred-per-f5");
        assert!(parsed.mirror_file_present);
        assert_eq!(parsed.mirror_file_status, "verified-agreement");
    }

    /// Forward-compat: a manifest with an extra field MUST parse
    /// successfully (the F5-lift PR's future `signature_*` block must
    /// not break older verifiers).
    #[test]
    fn parse_manifest_accepts_unknown_extra_fields_for_forward_compat() {
        let m = serde_json::json!({
            "version": 1,
            "invoice_id": "inv_X",
            "tenant_id": "t",
            "generated_at": "2026-01-01T00:00:00Z",
            "binary_hash": "00".repeat(32),
            "nav_xsd_version": "3.0",
            "chain_verified": true,
            "chain_verified_entries": 1,
            "entries_in_bundle": 1,
            "signed": false,
            "signature_status": "deferred-per-f5",
            "mirror_file_present": false,
            "mirror_file_status": "absent-pre-pr-17",
            "future_field": "added-by-some-future-pr",
            "signature_algorithm": "ed25519",
        });
        let bytes = serde_json::to_vec(&m).unwrap();
        let parsed = parse_manifest(&bytes).unwrap();
        assert_eq!(parsed.invoice_id, "inv_X");
    }

    #[test]
    fn parse_manifest_fails_on_missing_required_field() {
        // Missing `version` — a required field.
        let m = serde_json::json!({
            "invoice_id": "inv_X",
            "tenant_id": "t",
        });
        let bytes = serde_json::to_vec(&m).unwrap();
        let err = parse_manifest(&bytes).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("manifest") || msg.contains("version"),
            "missing required field must loud-fail naming the field: {msg}"
        );
    }

    fn fixture_chain_jsonl_line() -> String {
        let line = serde_json::json!({
            "id": format!("aud_{}", ulid::Ulid::new()),
            "seq": 1,
            "prev_hash": "ab".repeat(32),
            "time_wall": "2026-01-01T00:00:00Z",
            "time_mono": 12345u64,
            "actor": {
                "session_id": "sess",
                "user_id": "test-user",
                "capabilities": ["audit.append"],
            },
            "binary_hash": "00".repeat(32),
            "tenant_id": "t",
            "kind": "invoice.submission_attempt",
            "payload": BASE64_STANDARD.encode(b"{\"invoice_id\":\"inv_X\"}"),
            "idempotency_key": "idem-1",
            "entry_hash": "cd".repeat(32),
        });
        serde_json::to_string(&line).unwrap()
    }

    #[test]
    fn parse_chain_jsonl_one_line_round_trip() {
        let mut body = fixture_chain_jsonl_line();
        body.push('\n');
        let lines = parse_chain_jsonl(body.as_bytes()).unwrap();
        assert_eq!(lines.len(), 1);
        let l = &lines[0];
        assert_eq!(l.seq, 1);
        assert_eq!(l.kind, "invoice.submission_attempt");
        assert_eq!(l.tenant_id, "t");
        assert_eq!(l.idempotency_key.as_deref(), Some("idem-1"));
    }

    #[test]
    fn parse_chain_jsonl_loud_fails_on_blank_line() {
        let line = fixture_chain_jsonl_line();
        // Two newlines (blank line in the middle).
        let body = format!("{line}\n\n{line}\n");
        let err = parse_chain_jsonl(body.as_bytes()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("empty"),
            "blank line must loud-fail naming the cause: {msg}"
        );
    }

    #[test]
    fn reconstruct_entry_round_trip_succeeds_on_valid_line() {
        let line_str = fixture_chain_jsonl_line();
        let line: ChainJsonlLine = serde_json::from_str(&line_str).unwrap();
        let entry = reconstruct_entry(&line).unwrap();
        assert_eq!(entry.seq.as_u64(), 1);
        assert_eq!(entry.kind, EventKind::InvoiceSubmissionAttempt);
        assert_eq!(entry.payload, b"{\"invoice_id\":\"inv_X\"}");
    }

    #[test]
    fn reconstruct_entry_loud_fails_on_unknown_event_kind() {
        let mut line: ChainJsonlLine =
            serde_json::from_str(&fixture_chain_jsonl_line()).unwrap();
        line.kind = "invoice.future_kind_not_yet_known".to_string();
        let err = reconstruct_entry(&line).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("EventKind") && msg.contains("future_kind"),
            "unknown kind must loud-fail naming the string: {msg}"
        );
    }

    #[test]
    fn reconstruct_entry_loud_fails_on_short_hash() {
        let mut line: ChainJsonlLine =
            serde_json::from_str(&fixture_chain_jsonl_line()).unwrap();
        line.entry_hash = "ab".repeat(16); // 16 bytes, not 32
        let err = reconstruct_entry(&line).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("32 bytes"),
            "short hash must loud-fail naming the 32-byte width: {msg}"
        );
    }

    #[test]
    fn reconstruct_entry_loud_fails_on_missing_aud_prefix() {
        let mut line: ChainJsonlLine =
            serde_json::from_str(&fixture_chain_jsonl_line()).unwrap();
        line.id = ulid::Ulid::new().to_string(); // No prefix
        let err = reconstruct_entry(&line).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("aud_"),
            "missing prefix must loud-fail naming the convention: {msg}"
        );
    }

    /// ADR-0035 §4 filename composition mirror — the verifier MUST
    /// compute the same archive-relative path the writer used (dots
    /// replaced with underscores; 5-digit zero-padded seq) so the
    /// nav/<seq>_<kind>.xml lookup succeeds.
    #[test]
    fn nav_archive_path_matches_writer_filename_composition() {
        assert_eq!(
            nav_archive_path(12, EventKind::InvoiceSubmissionAttempt),
            "nav/00012_invoice_submission_attempt.xml"
        );
        assert_eq!(
            nav_archive_path(1, EventKind::InvoiceCheckPerformed),
            "nav/00001_invoice_check_performed.xml"
        );
        // Five-digit padding holds at the upper end of the
        // foreseeable per-tenant range (per ADR-0029 §3).
        assert_eq!(
            nav_archive_path(99999, EventKind::InvoiceAckStatus),
            "nav/99999_invoice_ack_status.xml"
        );
    }

    /// Defensive sanity: Actor's serde round-trip works against the
    /// chain.jsonl shape the writer produces. This catches a future
    /// rename of Actor's serde fields breaking the verifier silently.
    #[test]
    fn actor_round_trips_via_chain_jsonl_shape() {
        let actor = Actor::test_only();
        let json = serde_json::to_string(&actor).unwrap();
        let decoded: Actor = serde_json::from_str(&json).unwrap();
        let _: BTreeSet<String> = decoded.capabilities;
    }
}
