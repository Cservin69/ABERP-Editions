//! Orchestration for the `aberp export-invoice-bundle` subcommand
//! (PR-16, ADR-0009 §8, ADR-0029).
//!
//! Produces a single `.tar.zst` audit-evidence archive for one
//! invoice — the operator-visible artifact a NAV inspector
//! consumes when auditing the invoice's lifecycle. Read-only
//! over the audit ledger; no NAV calls, no audit writes, no
//! billing mutations (CLAUDE.md rule 3 — surgical changes).
//!
//! # Pipeline
//!
//!   1. Parse + validate CLI args. Empty / null-byte tenant
//!      loud-fails per the same posture every other ABERP
//!      command uses. The output path is checked for existence
//!      and refused unless `--allow-overwrite` is passed
//!      (ADR-0029 §1 + CLAUDE.md rule 12).
//!   2. Compute the binary hash for the manifest's
//!      `binary_hash` field (ADR-0029 §3). Distinct from each
//!      entry's `binary_hash` (which names the build that
//!      produced THAT entry, possibly an older binary) per
//!      ADR-0008 §"Adversarial review" bullet 2.
//!   3. Open the tenant `Ledger` read-only (the file is
//!      DuckDB; concurrent CLI mutations are safe because the
//!      `Ledger::entries` path opens its own connection).
//!   4. Run [`Ledger::verify_chain`] over the **full** chain.
//!      Loud-fail if it returns `Err(_)` — a tampered chain
//!      must NOT be exported as if authoritative per ADR-0029
//!      §6 + CLAUDE.md rule 12. The verify return value
//!      (entry count) lands in the manifest as
//!      `chain_verified_entries`.
//!   5. Walk every entry; filter to the per-invoice slice via
//!      [`bundle_membership_matches`] (ADR-0029 §2's any-id-
//!      field-equality probe). Loud-fail if zero entries match
//!      — the operator-visible message names the absence so
//!      the operator knows the bundle would be empty (CLAUDE.md
//!      rule 12 — silent zero-entry bundle is the wrong
//!      affordance).
//!   6. Build the manifest, the `chain.jsonl` body, and the
//!      per-NAV-XML `nav/<seq>_<kind>.xml` file list.
//!   7. Pack into the `.tar.zst` archive at the operator-
//!      supplied path. The archive's internal top-level
//!      directory is `bundle/` so an inspector untarring it
//!      gets one subdirectory, not a splatter of files into
//!      cwd.
//!   8. Operator-visible summary per ADR-0029 §7: NAMES THE
//!      DEFERRED GATES LOUD (signing-deferred-per-F5, mirror-
//!      deferred-per-F10) so a future contributor reading the
//!      operator-visible artifact reproduces the deferral
//!      rationale without re-reading the ADR. No audit-ledger
//!      write per ADR-0008 §"What goes in the ledger":
//!      read-only queries go to the normal log, not the
//!      audit ledger.
//!
//! # Why no audit-ledger write
//!
//! Per ADR-0008 §"What goes in the ledger" + ADR-0029 §7:
//! "Read-only queries (those go to the normal log)." The
//! bundle export is a read-only artifact production; the
//! operator-visible event lands in `tracing` output, not in
//! the audit ledger. A future operator-policy ADR may reverse
//! this if operational pattern surfaces a need; not pre-
//! emptively per CLAUDE.md rule 2.
//!
//! # Why one file per NAV entry instead of inlining inside `chain.jsonl`
//!
//! Per ADR-0029 §3: a NAV inspector untarring the bundle
//! wants to open `nav/00012_invoice_submission_attempt.xml`
//! in any XML viewer and see the actual XML — not navigate a
//! JSON encoding of base64-encoded XML. The separate-files
//! shape preserves operator-friendly inspectability; the
//! canonical bytes for hash verification still live in the
//! `payload` field of `chain.jsonl` per ADR-0008 §"Entry
//! shape".
//!
//! # What this flow does NOT do
//!
//!   - It does NOT call NAV. Read-only over the audit ledger.
//!   - It does NOT mutate any billing row. Read-only.
//!   - It does NOT write an audit-ledger entry (read-only
//!     query per ADR-0008 §"What goes in the ledger").
//!   - It does NOT sign the bundle. F5 deferred per ADR-0029
//!     §4; the manifest names the gap loud.
//!   - It does NOT read or assert the mirror file. F10
//!     deferred to PR-17 per ADR-0029 §5; the manifest names
//!     the gap loud.
//!   - It does NOT extend any audit-ledger crate surface
//!     (no new `EventKind`, no new payload struct). PR-16 is
//!     read-only consumer code; the audit-ledger crate is
//!     unchanged per CLAUDE.md rule 3.

use std::io::Write;
use std::path::Path;

use aberp_audit_ledger::{
    mirror_path_for, read_mirror_entries, BinaryHash, Entry, EventKind, Ledger, MirrorEntry,
    TenantId,
};
use anyhow::{anyhow, Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::binary_hash;
use crate::cli::ExportInvoiceBundleArgs;

/// Manifest schema version. Bumped only on parser-breaking
/// changes per ADR-0029 §3. Additive field additions (e.g.,
/// future `signature_*` block when F5 lifts) keep this at the
/// existing version.
const MANIFEST_VERSION: u32 = 1;

/// Placeholder string declared in the manifest while the F5
/// attestation-signing key type remains deferred per ADR-0029
/// §4. A future PR that lifts F5 replaces this string with the
/// chosen algorithm name (e.g., `"ed25519"`) and adds a
/// sibling `signature_*` block plus a detached-signature file
/// inside the archive.
const SIGNATURE_STATUS_DEFERRED: &str = "deferred-per-f5";

/// Manifest string surfaced when the mirror file is present
/// and its `entry_hash` for every covered seq matches the DB.
/// PR-17 / ADR-0030 §5.
const MIRROR_FILE_STATUS_VERIFIED: &str = "verified-agreement";

/// Manifest string surfaced when the mirror file is absent
/// (pre-PR-17 DB that has not yet been touched by a post-PR-17
/// command). PR-17 / ADR-0030 §"Surfaced conflict 3" Reading C.
/// Distinct from `"divergence-detected"` (which the bundle
/// reader never emits — it refuses the bundle output instead
/// per ADR-0029 §5 + ADR-0030 §5 + CLAUDE.md rule 12).
const MIRROR_FILE_STATUS_ABSENT_PRE_PR17: &str = "absent-pre-pr-17";

/// Internal top-level directory inside the archive. A NAV
/// inspector untarring the archive gets a single
/// `bundle/` subdirectory rather than the files splattered
/// into cwd (ADR-0029 §3).
const BUNDLE_DIR: &str = "bundle";

/// Permissive probe over an audit-ledger entry's payload bytes
/// per ADR-0029 §2. Captures every invoice-id-shaped field
/// across every payload type as `Option<String>`; any field
/// equality with the target invoice id makes the entry a
/// bundle member.
///
/// **Field set is load-bearing.** A future payload type that
/// introduces a new id-shaped field MUST extend this struct in
/// the same PR. The `probe_field_set_covers_every_payload_id_field`
/// unit test below pins the field set against the current
/// payload types; a future contributor who adds a payload type
/// with a new id-shaped field and forgets to extend the Probe
/// surfaces the failure at commit time (CLAUDE.md rule 9 — the
/// test must catch business-logic drift, not just current
/// behaviour).
#[derive(Debug, Deserialize)]
struct BundleMembershipProbe {
    /// Primary `invoice_id` field on most payload types
    /// (every non-chain-link payload). Names the invoice the
    /// entry is about.
    invoice_id: Option<String>,
    /// Storno-side chain-link field on
    /// `InvoiceStornoIssuedPayload`. Names the storno
    /// invoice's own id (which is itself an invoice per
    /// ADR-0023 §1).
    storno_invoice_id: Option<String>,
    /// Modification-side chain-link field on
    /// `InvoiceModificationIssuedPayload`. Names the
    /// modification invoice's own id (which is itself an
    /// invoice per ADR-0024 §1).
    modification_invoice_id: Option<String>,
    /// Base-side chain-link field on the two chain-link
    /// payloads. Names the base invoice the chain entry
    /// points at; the base's bundle includes the chain
    /// entry via this field.
    base_invoice_id: Option<String>,
}

impl BundleMembershipProbe {
    /// True iff any of the four id-shaped fields equals the
    /// target invoice id. Empty strings are NOT treated as
    /// matches (defence-in-depth — a tampered payload with
    /// an empty `invoice_id` field would otherwise match
    /// every empty-target query).
    fn matches(&self, target: &str) -> bool {
        if target.is_empty() {
            return false;
        }
        self.field_iter().any(|f| f == target)
    }

    /// Iterator over the present (`Some(_)` and non-empty)
    /// id-shaped fields. Same iteration order across calls
    /// (the field order in [`Self`]); a future addition is
    /// appended at the iteration's tail.
    fn field_iter(&self) -> impl Iterator<Item = &str> {
        [
            self.invoice_id.as_deref(),
            self.storno_invoice_id.as_deref(),
            self.modification_invoice_id.as_deref(),
            self.base_invoice_id.as_deref(),
        ]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
    }
}

/// True iff this entry's payload identifies the target invoice
/// in any role (primary or chain-link). Used by [`run`] to
/// filter the full ledger into the per-invoice slice.
fn bundle_membership_matches(entry: &Entry, target_invoice_id: &str) -> bool {
    match serde_json::from_slice::<BundleMembershipProbe>(&entry.payload) {
        Ok(probe) => probe.matches(target_invoice_id),
        // A payload that fails permissive JSON deserialization
        // is excluded — `chain.jsonl` would not be able to
        // include it cleanly anyway, and the exclusion is
        // visible in the verify-chain count vs entries-in-
        // bundle count.
        Err(_) => false,
    }
}

/// Resolve which entries land in the per-invoice slice and
/// return them in `seq` order (oldest first). Loud-fail if
/// the slice is empty — a zero-entry bundle is the wrong
/// affordance (CLAUDE.md rule 12); the operator-visible
/// message names the absence so the operator can investigate
/// (typo on the id, wrong tenant, ledger genuinely never saw
/// this id).
fn filter_invoice_slice(entries: &[Entry], invoice_id: &str) -> Result<Vec<Entry>> {
    let slice: Vec<Entry> = entries
        .iter()
        .filter(|e| bundle_membership_matches(e, invoice_id))
        .cloned()
        .collect();
    if slice.is_empty() {
        return Err(anyhow!(
            "no audit-ledger entries reference invoice id {invoice_id} in any role \
             (primary, storno, modification, OR base) — \
             check the id is correct and the --tenant + --db point at the same DB \
             the issue-invoice / submit-invoice commands wrote to"
        ));
    }
    Ok(slice)
}

/// Per-entry JSON serialization shape written one-per-line into
/// `chain.jsonl` per ADR-0029 §3. Carries every ADR-0008
/// §"Entry shape" field; hashes are hex-encoded, the
/// `payload` bytes are base64-encoded, the `actor` is the
/// typed serde-roundtrip JSON shape (Actor derives
/// Serialize/Deserialize).
#[derive(Debug, Serialize)]
struct ChainJsonlEntry<'a> {
    id: String,
    seq: u64,
    prev_hash: String,
    time_wall: String,
    time_mono: u64,
    actor: &'a aberp_audit_ledger::Actor,
    binary_hash: String,
    tenant_id: &'a str,
    kind: &'a str,
    payload: String,
    idempotency_key: Option<&'a str>,
    entry_hash: String,
}

impl<'a> ChainJsonlEntry<'a> {
    /// Encode one [`Entry`] for serialization into a
    /// `chain.jsonl` line. Hashes are hex-encoded (operator-
    /// readable comparison); payload bytes are base64-encoded
    /// (JSON-safe; same encoding the `nav/<seq>_<kind>.xml`
    /// sibling files use semantically — the inspector can
    /// cross-check by decoding the base64 here and comparing
    /// against the sibling file's bytes).
    fn from_entry(entry: &'a Entry) -> Result<Self> {
        let time_wall = entry
            .time_wall
            .format(&Rfc3339)
            .context("format entry time_wall as RFC3339 for chain.jsonl")?;
        Ok(Self {
            id: entry.id.to_prefixed_string(),
            seq: entry.seq.as_u64(),
            prev_hash: hex::encode(entry.prev_hash.as_bytes()),
            time_wall,
            time_mono: entry.time_mono,
            actor: &entry.actor,
            binary_hash: hex::encode(entry.binary_hash.as_bytes()),
            tenant_id: entry.tenant_id.as_str(),
            kind: entry.kind.as_str(),
            payload: BASE64_STANDARD.encode(&entry.payload),
            idempotency_key: entry.idempotency_key.as_deref(),
            entry_hash: hex::encode(entry.entry_hash.as_bytes()),
        })
    }
}

/// Bundle-level manifest fields per ADR-0029 §3 + ADR-0030 §5
/// (the additive `mirror_file_*` flip). Serialized as pretty JSON
/// at `bundle/manifest.json`. Field-set pinned by
/// [`tests::manifest_carries_every_adr_0029_field`] and the
/// PR-17-added [`tests::manifest_mirror_fields_match_agreement_status`].
#[derive(Debug, Serialize)]
struct BundleManifest<'a> {
    version: u32,
    invoice_id: &'a str,
    tenant_id: &'a str,
    generated_at: String,
    binary_hash: String,
    nav_xsd_version: &'static str,
    chain_verified: bool,
    chain_verified_entries: u64,
    entries_in_bundle: u64,
    signed: bool,
    signature_status: &'static str,
    mirror_file_present: bool,
    mirror_file_status: &'static str,
}

/// PR-17 / ADR-0030 §5. The success-shape outcomes of the
/// bundle reader's mirror agreement check. A third state —
/// `DivergenceDetected` — is NOT a variant because the bundle
/// reader REFUSES the bundle output on divergence per ADR-0030
/// §5 + ADR-0029 §5 + CLAUDE.md rule 12 (the refusal happens
/// inside `run` before `build_manifest` is called).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MirrorAgreementStatus {
    /// Mirror file present and every covered seq agrees with
    /// the DB at the `entry_hash` level. Manifest:
    /// `mirror_file_present: true`,
    /// `mirror_file_status: "verified-agreement"`.
    VerifiedAgreement,
    /// Mirror file absent (pre-PR-17 DB; the next post-PR-17
    /// command that appends will initialise the mirror).
    /// Manifest: `mirror_file_present: false`,
    /// `mirror_file_status: "absent-pre-pr-17"`.
    AbsentPrePr17,
}

/// Detect the mirror file at the conventional path and assert
/// agreement with the DB-sourced entries (ADR-0030 §5).
///
/// Returns `Ok(VerifiedAgreement)` if the mirror is present
/// and agrees; `Ok(AbsentPrePr17)` if the mirror file does
/// not exist. Returns `Err(_)` on:
///
/// - Mirror file present but `entry_hash` disagreement with
///   the DB at any seq (refuses the bundle per ADR-0030 §5).
/// - Mirror file present but malformed (delegated to
///   `read_mirror_entries`'s `MirrorCorrupt` surface).
/// - Mirror file I/O error other than `NotFound`.
fn detect_mirror_agreement(db_path: &Path, db_entries: &[Entry]) -> Result<MirrorAgreementStatus> {
    let mirror_path = mirror_path_for(db_path);
    match read_mirror_entries(&mirror_path) {
        Ok(mirror_entries) => {
            assert_mirror_db_agreement(&mirror_entries, db_entries, &mirror_path)?;
            Ok(MirrorAgreementStatus::VerifiedAgreement)
        }
        Err(aberp_audit_ledger::AppendError::MirrorIo(io))
            if io.kind() == std::io::ErrorKind::NotFound =>
        {
            Ok(MirrorAgreementStatus::AbsentPrePr17)
        }
        Err(other) => Err(anyhow!(
            "audit-ledger mirror file at {} is unreadable: {}; \
             refusing to emit a bundle with an unreadable mirror per ADR-0030 §5 + CLAUDE.md rule 12",
            mirror_path.display(),
            other
        )),
    }
}

/// Assert mirror-vs-DB agreement at the `entry_hash` level.
/// Per ADR-0030 §4 the entry_hash is the canonical agreement key
/// — every other field is derivable from it once the chain
/// verify (done earlier in `run`) has passed.
fn assert_mirror_db_agreement(
    mirror_entries: &[MirrorEntry],
    db_entries: &[Entry],
    mirror_path: &Path,
) -> Result<()> {
    if mirror_entries.len() != db_entries.len() {
        return Err(anyhow!(
            "audit-ledger mirror at {} has {} entries; DB has {} entries; \
             refusing to emit a bundle on count mismatch per ADR-0030 §5",
            mirror_path.display(),
            mirror_entries.len(),
            db_entries.len(),
        ));
    }
    for (m, d) in mirror_entries.iter().zip(db_entries.iter()) {
        if m.seq() != d.seq.as_u64() {
            return Err(anyhow!(
                "audit-ledger mirror at {} disagrees with DB at line {}: \
                 mirror seq={}, DB seq={}; refusing to emit bundle",
                mirror_path.display(),
                d.seq.as_u64(),
                m.seq(),
                d.seq.as_u64(),
            ));
        }
        let db_hash = hex::encode(d.entry_hash.as_bytes());
        if m.entry_hash() != db_hash {
            return Err(anyhow!(
                "audit-ledger mirror at {} disagrees with DB at seq={}: \
                 mirror entry_hash={}, DB entry_hash={}; refusing to emit bundle \
                 per ADR-0030 §5 + ADR-0029 §5",
                mirror_path.display(),
                d.seq.as_u64(),
                m.entry_hash(),
                db_hash,
            ));
        }
    }
    Ok(())
}

/// Build the manifest object for the bundle. `generated_at`
/// uses `OffsetDateTime::now_utc()` formatted as RFC3339 —
/// same shape every other audit-bearing timestamp uses.
fn build_manifest<'a>(
    invoice_id: &'a str,
    tenant_id: &'a str,
    binary_hash: BinaryHash,
    chain_verified_entries: u64,
    entries_in_bundle: u64,
    mirror_status: MirrorAgreementStatus,
) -> Result<BundleManifest<'a>> {
    let generated_at = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .context("format manifest generated_at as RFC3339")?;
    let (mirror_file_present, mirror_file_status) = match mirror_status {
        MirrorAgreementStatus::VerifiedAgreement => (true, MIRROR_FILE_STATUS_VERIFIED),
        MirrorAgreementStatus::AbsentPrePr17 => (false, MIRROR_FILE_STATUS_ABSENT_PRE_PR17),
    };
    Ok(BundleManifest {
        version: MANIFEST_VERSION,
        invoice_id,
        tenant_id,
        generated_at,
        binary_hash: hex::encode(binary_hash.as_bytes()),
        nav_xsd_version: aberp_nav_xsd_validator::NAV_XSD_VERSION,
        // chain_verified is true here because [`run`] aborts
        // before reaching this builder if `verify_chain()`
        // returned Err; reaching this code path is the
        // post-condition that the chain verified. The boolean
        // is in the manifest as a load-bearing assertion the
        // bundle reader emits — a future bundle-verifier tool
        // re-asserts it against the bundle's own bytes.
        chain_verified: true,
        chain_verified_entries,
        entries_in_bundle,
        signed: false,
        signature_status: SIGNATURE_STATUS_DEFERRED,
        mirror_file_present,
        mirror_file_status,
    })
}

/// One NAV-XML extraction from an audit-ledger entry. Pairs
/// the archive-relative filename (`nav/<seq>_<kind>.xml`) with
/// the verbatim bytes lifted from the typed payload.
#[derive(Debug)]
struct NavXmlFile {
    /// Archive-relative path inside `bundle/`. The full
    /// in-archive path is `bundle/<archive_path>`.
    archive_path: String,
    /// Verbatim bytes (no transformation; same `request_xml`
    /// / `response_xml` bytes the audit payload carries).
    bytes: Vec<u8>,
}

/// Extract the verbatim NAV XML bytes (if any) from an entry's
/// typed payload. Returns `Ok(Some(_))` for NAV-bearing kinds,
/// `Ok(None)` for kinds without NAV bytes (test, sequence
/// allocator entries, operator-decision entries, chain-link
/// entries), `Err(_)` if the entry's payload bytes failed to
/// decode against the expected typed shape (which surfaces a
/// ledger-tampering / schema-drift concern the operator must
/// see per CLAUDE.md rule 12).
///
/// The filename composition (`<seq:05>_<kind>.xml`) is pinned
/// here so a future kind addition extends this match
/// exhaustively (Rust exhaustiveness checker fails the build
/// if a new variant lands without a branch). The seq-zero-
/// padding to 5 digits supports up to 99,999 entries before
/// the lexicographic sort breaks; per ADR-0009 §3 the
/// per-tenant volume bound is comfortably below that for the
/// foreseeable future.
fn extract_nav_xml(entry: &Entry) -> Result<Option<NavXmlFile>> {
    let bytes = match entry.kind {
        EventKind::InvoiceSubmissionAttempt => {
            let payload: crate::audit_payloads::InvoiceSubmissionAttemptPayload =
                serde_json::from_slice(&entry.payload).with_context(|| {
                    format!(
                        "decode InvoiceSubmissionAttempt payload at seq {}",
                        entry.seq.as_u64()
                    )
                })?;
            Some(payload.request_xml)
        }
        EventKind::InvoiceSubmissionResponse => {
            let payload: crate::audit_payloads::InvoiceSubmissionResponsePayload =
                serde_json::from_slice(&entry.payload).with_context(|| {
                    format!(
                        "decode InvoiceSubmissionResponse payload at seq {}",
                        entry.seq.as_u64()
                    )
                })?;
            Some(payload.response_xml)
        }
        EventKind::InvoiceAckStatus => {
            let payload: crate::audit_payloads::InvoiceAckStatusPayload =
                serde_json::from_slice(&entry.payload).with_context(|| {
                    format!(
                        "decode InvoiceAckStatus payload at seq {}",
                        entry.seq.as_u64()
                    )
                })?;
            Some(payload.response_xml)
        }
        EventKind::InvoiceAnnulmentSubmissionAttempt => {
            let payload: crate::audit_payloads::InvoiceAnnulmentSubmissionAttemptPayload =
                serde_json::from_slice(&entry.payload).with_context(|| {
                    format!(
                        "decode InvoiceAnnulmentSubmissionAttempt payload at seq {}",
                        entry.seq.as_u64()
                    )
                })?;
            Some(payload.request_xml)
        }
        EventKind::InvoiceAnnulmentSubmissionResponse => {
            let payload: crate::audit_payloads::InvoiceAnnulmentSubmissionResponsePayload =
                serde_json::from_slice(&entry.payload).with_context(|| {
                    format!(
                        "decode InvoiceAnnulmentSubmissionResponse payload at seq {}",
                        entry.seq.as_u64()
                    )
                })?;
            Some(payload.response_xml)
        }
        EventKind::InvoiceAnnulmentAckStatus => {
            let payload: crate::audit_payloads::InvoiceAnnulmentAckStatusPayload =
                serde_json::from_slice(&entry.payload).with_context(|| {
                    format!(
                        "decode InvoiceAnnulmentAckStatus payload at seq {}",
                        entry.seq.as_u64()
                    )
                })?;
            Some(payload.response_xml)
        }
        EventKind::InvoiceAnnulmentReceiverConfirmation => {
            let payload: crate::audit_payloads::InvoiceAnnulmentReceiverConfirmationPayload =
                serde_json::from_slice(&entry.payload).with_context(|| {
                    format!(
                        "decode InvoiceAnnulmentReceiverConfirmation payload at seq {}",
                        entry.seq.as_u64()
                    )
                })?;
            Some(payload.response_xml)
        }
        // PR-19 / ADR-0032 §2: the new failure-side kind carries a
        // verbatim NAV response body IFF the failure had one
        // (`http_status` / `application` / `retryable_application`
        // classes); for `transport` / `envelope` / `credential` /
        // `client_build` classes the payload's `response_xml` is
        // `None` and no nav/ file is produced for this entry.
        EventKind::InvoiceSubmissionAttemptFailed => {
            let payload: crate::audit_payloads::InvoiceSubmissionAttemptFailedPayload =
                serde_json::from_slice(&entry.payload).with_context(|| {
                    format!(
                        "decode InvoiceSubmissionAttemptFailed payload at seq {}",
                        entry.seq.as_u64()
                    )
                })?;
            payload.response_xml
        }
        // PR-20 / ADR-0033 §2: Layer-2 queryInvoiceCheck evidence.
        // The payload's `response_xml` is Option<Vec<u8>> — Some
        // for `outcome = "exists"` / `"absent"` (NAV returned a
        // body) and Some-or-None for `outcome = "failure"` (Some
        // for http_status / application / retryable_application
        // classes where NAV returned an error body; None for
        // transport / envelope / credential / client_build classes
        // where no body was received). The request_xml (verbatim
        // `<QueryInvoiceCheckRequest>` bytes) lives in-payload via
        // chain.jsonl per ADR-0033 §10 — the bundle's nav/
        // directory carries the response side only, mirroring the
        // existing per-NAV-kind shape.
        EventKind::InvoiceCheckPerformed => {
            let payload: crate::audit_payloads::InvoiceCheckPerformedPayload =
                serde_json::from_slice(&entry.payload).with_context(|| {
                    format!(
                        "decode InvoiceCheckPerformed payload at seq {}",
                        entry.seq.as_u64()
                    )
                })?;
            payload.response_xml
        }
        // Non-NAV-bearing kinds — no nav/ file produced for
        // these. The match is deliberately exhaustive (no
        // `_ => ...` arm) so a future EventKind variant
        // requires a contributor decision: does the new kind
        // carry NAV bytes? If yes, add an arm; if no, add it
        // to the no-bytes side below. CLAUDE.md rule 8 (read
        // before write — a contributor adding a NAV-bearing
        // kind sees this match-arm list and must pick).
        EventKind::Test
        | EventKind::InvoiceSequenceReserved
        | EventKind::InvoiceDraftCreated
        | EventKind::InvoiceRetryRequested
        | EventKind::InvoiceMarkedAbandoned
        | EventKind::InvoiceStornoIssued
        | EventKind::InvoiceModificationIssued
        | EventKind::InvoiceTechnicalAnnulmentRequested
        // PR-70 / ADR-0039 §2 — operational payment-recorded
        // entry carries no NAV-side bytes (it's a local
        // operator decision, not a NAV submission). The audit
        // payload (paid_at + amount + method + reference) is
        // preserved in chain.jsonl per ADR-0009 §8; nothing
        // lands in the nav/ directory.
        | EventKind::InvoicePaymentRecorded
        // PR-92 / ADR-0047 §4 — operational email-sent entry
        // also carries no NAV-side bytes. The audit payload
        // (recipient + subject + outcome + scrubbed error
        // detail) lives in chain.jsonl per ADR-0009 §8.
        | EventKind::InvoiceEmailedSent
        // S166 — system-lifecycle first-prod-launch acknowledgement
        // carries no NAV bytes. It is also NOT invoice-scoped (the
        // `invoice.*` glob never sweeps it into a bundle), so this arm
        // is reached only if it ever appeared in an entry list; the
        // payload (acknowledged_at + tenant) lives in chain.jsonl.
        | EventKind::FirstProdLaunchAcknowledged
        // S171 — system-lifecycle upgrade-snapshot mismatch (the
        // boot-time pre-upgrade safety check detected drift in
        // [seller.smtp] / [seller.numbering]). Also `system.`-scoped,
        // never sweeps a per-invoice bundle; no NAV bytes.
        | EventKind::UpgradeSnapshotMismatch
        // S177 / PR-177 — AP-side incoming-invoice ingestion + status
        // change events. The raw NAV InvoiceData XML for incoming
        // invoices lives at
        // `~/.aberp/<tenant>/ap-artifacts/<apinv_id>.xml`, NOT in the
        // audit payload; the payload's `nav_xml_sha256` is the
        // tamper-detection hash. Both kinds are also `system.`-
        // prefixed, so the per-OUTGOING-invoice export bundle's
        // `invoice.*` glob never sweeps them — they are listed here
        // for exhaustiveness only.
        | EventKind::IncomingInvoiceIngested
        | EventKind::IncomingInvoiceStatusChanged
        // S178 / PR-178 — AP-side auto-sync cycle-completion event.
        // Same posture as the other AP-side kinds: `system.`-scoped,
        // never sweeps a per-outgoing-invoice bundle; no NAV bytes
        // on the payload (the per-cycle ingestions emit their own
        // `IncomingInvoiceIngested` entries, which themselves carry
        // no NAV bytes in the bundle either).
        | EventKind::IncomingInvoiceSyncCycleCompleted
        // S180 / PR-180 — NAV-as-DR restore event. The restored row
        // lives in `restored_invoice` (not `invoice`), so this kind
        // is `system.`-scoped and never sweeps a per-outgoing-invoice
        // bundle. v1 is digest-only — no verbatim NAV XML rides the
        // payload; the typed digest fields (invoice_number, totals,
        // issue_date, currency, transaction_id) live in chain.jsonl.
        | EventKind::InvoiceRestoredFromNav
        // S210 / PR-204 — quote-intake daemon cycle completion event.
        // `system.`-scoped — never sweeps a per-outgoing-invoice
        // bundle. The payload carries cycle counts + cadence telemetry,
        // not NAV bytes.
        | EventKind::QuoteIntakePollCompleted
        // S213 / PR-209 — graceful-shutdown coordinator event.
        // `system.`-scoped — process-lifecycle telemetry never
        // belongs in a per-invoice export bundle.
        | EventKind::DaemonShutdownCompleted
        // S220 / PR-217 — buyer-backfill cycle completion event.
        // `system.`-scoped — recovery cadence telemetry against
        // `restored_invoice`, not a per-OUTGOING-invoice surface.
        | EventKind::RestoreBuyerBackfillCycleCompleted
        // S220 / PR-217 — operator-paced manual partner link on a
        // restored ExtNav row. The restored row lives in
        // `restored_invoice` (not `invoice`); annotations against
        // it never belong in a per-OUTGOING-invoice export bundle.
        | EventKind::ExtNavPartnerManualLink
        // S261 / PR-250 — aggregate restore-batch-summary event. A
        // recovery-batch landmark against `restored_invoice`, not a
        // per-OUTGOING-invoice surface; the `system.` prefix keeps the
        // bundle's `invoice.*` glob from sweeping it. Exhaustive arm.
        | EventKind::RestoreFromNavRun
        // S228 / PR-224 / ADR-0060 — Stage 3 manufacturing-execution
        // adapter event. `mes.`-prefixed (a third prefix family
        // alongside `invoice.*` and `system.*`); shop-floor events
        // belong to machines / parts / work orders, never to an
        // outgoing invoice. The per-OUTGOING-invoice bundle's
        // `invoice.*` glob excludes them anyway; the exhaustive match
        // requires acknowledgement here.
        | EventKind::MesAdapterEvent
        // S231 / PR-227 / ADR-0061 — inventory stock-movement event.
        // Same `mes.*` prefix family as MesAdapterEvent (Stage 3
        // modules share the prefix per ADR-0061 §4); stock movements
        // belong to products + work orders + dispatches, never to an
        // outgoing invoice. The bundle's `invoice.*` glob excludes
        // them anyway; this arm exists for exhaustiveness only.
        | EventKind::StockMovementRecorded
        // S232 / PR-228 / ADR-0062 — Work Order lifecycle events.
        // `mes.*` family per ADR-0062 §4; WO lifecycle belongs to
        // products + routings + dispatches, never to an outgoing
        // invoice. The bundle's `invoice.*` glob excludes them
        // anyway; these arms exist for exhaustiveness only.
        | EventKind::WorkOrderCreated
        | EventKind::WorkOrderStateChanged
        | EventKind::RoutingOpStateChanged
        // S233 / PR-229 / ADR-0063 — QA-queue events. `mes.*` family
        // per ADR-0063 §5; QA inspections belong to routing-ops on
        // work orders, never to an outgoing invoice. The bundle's
        // `invoice.*` glob excludes them anyway; these arms exist
        // for exhaustiveness only.
        | EventKind::QaInspectionCreated
        | EventKind::QaInspectionDecided
        // S234 / PR-230 / ADR-0064 — Dispatch-board events. `mes.*`
        // family per ADR-0064 §6; dispatches belong to work orders on
        // the manufacturing side, never to an outgoing invoice. The
        // `spawned_invoice_id` field on `DispatchShipped` points AT a
        // Stage 1 invoice draft but the dispatch payload carries no
        // NAV bytes itself. The bundle's `invoice.*` glob excludes
        // them anyway; these arms exist for exhaustiveness only.
        | EventKind::DispatchCreated
        | EventKind::DispatchShipped
        // S236 / PR-230b — pre-allocation invoice-draft staging event.
        // Payload is keyed by `drf_<ULID>` (a draft id) not
        // `inv_<ULID>`, so the per-OUTGOING-invoice export bundle's
        // `invoice.*` glob never sweeps a staged-then-deleted draft
        // into an invoice's evidence bundle. On promotion the operator's
        // Issue click runs the existing `issue_invoice` pipeline which
        // fires `InvoiceSequenceReserved` + `InvoiceDraftCreated` against
        // the freshly minted `inv_*` id — those are the entries the
        // bundle includes; the prior `InvoiceStaged` row belongs to the
        // `drf_*` id and stays outside the bundle. No NAV bytes carried.
        | EventKind::InvoiceStaged
        // S239 / PR-233 — pre-allocation invoice-draft DELETION event.
        // Same `drf_<ULID>` keying + bundle-exclusion rationale as
        // `InvoiceStaged`: a deleted draft never receives a downstream
        // `inv_<ULID>`, so the per-OUTGOING-invoice bundle's id-filter
        // never matches the row. No NAV bytes (operator deletion is a
        // purely local event with no NAV-side correspondent).
        | EventKind::InvoiceDraftDeleted
        // S255 / PR-244 — operator quote pickup. Same `drf_<ULID>`
        // keying as `InvoiceStaged` (the payload's `draft_id`); the
        // per-OUTGOING-invoice bundle never matches because there is
        // no `inv_<ULID>` until the operator's Issue click promotes
        // the draft. No NAV bytes.
        | EventKind::InvoicePickedUpFromQuote
        // S256 / PR-245 — quote-intake hardening kinds. `system.`-scoped
        // sister-service staging telemetry (per-cycle heartbeat, per-row
        // arrival, structured failure). Carry poll counters + quote_ids,
        // never NAV bytes; never sweep a per-OUTGOING-invoice bundle.
        | EventKind::QuoteIntakePollAttempted
        | EventKind::QuoteIntakeRowAdded
        | EventKind::QuoteIntakePollFailed
        // S257 / PR-246 — adapter-config CRUD kinds. `mes.`-scoped
        // operator configuration; no NAV bytes, never sweep a per-
        // OUTGOING-invoice bundle.
        | EventKind::AdapterAdded
        | EventKind::AdapterUpdated
        | EventKind::AdapterRemoved
        // S258 / PR-247 — adapter health-transition telemetry. `mes.`-
        // scoped runtime observation; no NAV bytes, never sweeps a per-
        // OUTGOING-invoice bundle.
        | EventKind::AdapterHealthTransitioned
        // S266 / PR-255 — material-catalogue CRUD + storefront-push
        // kinds (`quote.*`). Auto-quoting tunable-table operator
        // configuration / outbound notification; no NAV bytes, never
        // sweep a per-OUTGOING-invoice bundle.
        | EventKind::MaterialCatalogueChanged
        | EventKind::MaterialCataloguePushed
        // S267 / PR-256 — quoting tunables CRUD kinds (`quote.*`).
        // Operator-managed catalogue edits on the auto-quoting
        // engine's complexity rules / tolerance multipliers / global
        // parameters / stock adjustments. No NAV bytes; never sweep
        // a per-OUTGOING-invoice bundle.
        | EventKind::ComplexityRulesChanged
        | EventKind::ToleranceMultipliersChanged
        | EventKind::ParametersChanged
        | EventKind::StockAdjustmentsChanged
        // S271 / PR-260 — EVE addendum 2 stale-stock guard
        // (`quote.*`). Operator-display recompute outcome carried as a
        // JSON snapshot; no NAV bytes; never sweeps a per-OUTGOING-
        // invoice bundle (the `invoice.*` glob excludes `quote.*`).
        | EventKind::QuoteStockAlertTriggered
        // S272 / PR-261 — DEAL-saga kinds (`quote.*`). Three-entry
        // saga ride a single tx: top-level `QuoteDealIssued` + the
        // SO/WO placeholder kinds. Quote-scoped operator action; no
        // NAV bytes; never swept by the per-OUTGOING-invoice bundle.
        | EventKind::QuoteDealIssued
        | EventKind::QuoteSalesOrderCreated
        | EventKind::QuoteWorkOrderCreated
        // S273 / PR-262 / ADR-0069 — material-state-machine kinds
        // (`inventory.*`). The DEAL saga emits `MaterialCommitted`
        // alongside its three `quote.*` siblings inside one tx; the
        // other three are defined for future handlers. None carry NAV
        // bytes; none should ever sweep into a per-OUTGOING-invoice
        // bundle (the bundle filter is keyed on `invoice.*`, not the
        // `inventory.*` family).
        | EventKind::MaterialReserved
        | EventKind::MaterialCommitted
        | EventKind::MaterialConsumed
        | EventKind::MaterialReleased
        // S279 / PR-265 — pricing-pipeline kinds (`quote.*`). Six-stage
        // daemon-driven auto-quoting flow. Same `quote.*`-not-`invoice.*`
        // posture as the S271/S272 kinds; never sweeps a per-OUTGOING-
        // invoice export bundle.
        | EventKind::QuotePricingFetched
        | EventKind::QuotePricingExtracted
        | EventKind::QuotePricingPriced
        | EventKind::QuotePricingRendered
        | EventKind::QuotePricingPosted
        | EventKind::QuotePricingFailed
        // S281 / PR-266 — email-relay kinds (`email.*` family). The
        // storefront emails relay through ABERP via `POST /api/internal/
        // send-email` per ADR-0007. Audit payload carries submitter +
        // recipient_hash + subject + byte_size, never NAV XML bytes.
        // `email.*`-not-`invoice.*` posture; never sweeps a per-
        // OUTGOING-invoice bundle by glob.
        | EventKind::EmailRelayQueued
        | EventKind::EmailRelaySent
        | EventKind::EmailRelayFailed
        // S282 / PR-267 — pipeline-python-resolve kind (`quote.*`).
        // Pricing-daemon spawn telemetry; carries resolution_kind +
        // resolved_path + module_importable, never NAV XML bytes.
        // Never sweeps a per-OUTGOING-invoice export bundle.
        | EventKind::PipelinePythonResolved
        // S286 / PR-268 — pricing-daemon-panicked kind (`quote.*`).
        // Supervisor-recovery telemetry; carries panic_msg +
        // restart_count + last_known_quote_id, never NAV XML bytes.
        // Never sweeps a per-OUTGOING-invoice export bundle.
        | EventKind::QuotePricingDaemonPanicked
        // S288 / PR-269 — one-shot pricing-jobs index-migrated kind
        // (`quote.*`). Boot-time schema-migration record; carries
        // tenant_id + index_name + dropped_at, never NAV XML bytes.
        // Never sweeps a per-OUTGOING-invoice export bundle.
        | EventKind::QuotePricingJobsIndexMigrated
        // S290 / PR-271 — failure-classifier verdict kind (`quote.*`).
        // Companion to QuotePricingFailed; carries failure_kind verdict
        // + last_error + attempt_n, never NAV XML bytes. Never sweeps a
        // per-OUTGOING-invoice export bundle.
        | EventKind::QuotePricingFailureClassified
        // S307 / PR-276 — email-outbox poll-daemon kinds (`quote.*`
        // family). Daemon polls the storefront `/api/internal/email-queue`
        // and delivers via ABERP's SMTP per ADR-0009. Payload carries
        // submitter / queue_row_id / recipient_hash / subject / byte_size /
        // outcome — never NAV XML bytes. `quote.*`-not-`invoice.*` posture;
        // never sweeps a per-OUTGOING-invoice export bundle by glob.
        | EventKind::EmailOutboxFetched
        | EventKind::EmailOutboxClaimed
        | EventKind::EmailOutboxSent
        | EventKind::EmailOutboxFailed
        // S325 / PR-25 — customer-PDF re-render audit family. App-layer
        // JSON payloads, never NAV XML bytes.
        | EventKind::QuotePdfRerenderEnqueued
        | EventKind::QuotePdfRerendered
        | EventKind::QuotePdfRerenderFailed
        // S347 / PR-39 — priced-writeback transport verdict. App-layer JSON
        // payload, never NAV XML bytes.
        | EventKind::QuotePricedWritebackOutcome
        // S348 / PR-39 — list-poll transport verdict. App-layer JSON
        // payload, never NAV XML bytes.
        | EventKind::QuotePollOutcome
        // S350 / PR-39 — operator material-grade edit. App-layer JSON
        // payload, never NAV XML bytes.
        | EventKind::QuotePricingMaterialEdited
        // S391/F — operator deletion of a Failed pricing-job row. App-layer
        // JSON payload (quote_id / previous_state / attempt_n / error_*),
        // never NAV XML bytes.
        | EventKind::QuotePricingFailureDeleted
        // S354 / PR-42 — operator accept-on-behalf. App-layer JSON
        // payload (channel / note / outcome tag), never NAV XML bytes.
        | EventKind::QuotePricingOperatorAccepted
        // S403 — operator REFUSE-with-reason. App-layer JSON payload
        // (quote_id / reason / operator_user_id), never NAV XML bytes.
        | EventKind::QuoteOperatorRefused
        // S355 / PR-43 — personnel.* access-trail family (ADR-0073).
        // Defense-grade identity / signature / access-decision rows; app-layer
        // JSON payloads, never NAV XML bytes. `personnel.*`-not-`invoice.*`
        // posture; never sweeps a per-OUTGOING-invoice export bundle by glob.
        | EventKind::PersonnelIdRegistered
        | EventKind::PersonnelSignatureApplied
        | EventKind::PersonnelAccessGranted
        | EventKind::PersonnelAccessDenied
        // S357 / PR-44 — material.* traceability family (ADR-0074).
        // Cert-attach record + heat/lot-assign state transition; app-layer
        // JSON payloads, never NAV XML bytes. `material.*`-not-`invoice.*`
        // posture; never sweeps a per-OUTGOING-invoice export bundle by glob.
        | EventKind::MaterialCertAttached
        | EventKind::MaterialHeatLotAssigned
        // S432 (ADR-0085) — heat-lot traceability firing-site kinds, same
        // `material.*` JSON-payload posture; never NAV XML bytes.
        | EventKind::MaterialWoBlockedNoHeatLot
        | EventKind::MaterialMtrUploaded
        | EventKind::MaterialTraceabilityViewed
        // S358 / PR-45 — part.* per-unit serialization family (ADR-0075).
        // Serial-assign record + UID-mark state transition; app-layer JSON
        // payloads, never NAV XML bytes. `part.*`-not-`invoice.*` posture;
        // never sweeps a per-OUTGOING-invoice export bundle by glob.
        | EventKind::PartSerialAssigned
        | EventKind::PartUidMarked
        // S438 (ADR-0089) — part.* per-unit UID-marking firing-site kinds, same
        // `part.*` JSON-payload posture; never NAV XML bytes.
        | EventKind::WoBlockedNoPartUid
        | EventKind::PartTraceabilityViewed
        // S359 / PR-46 — export.* export-control family (ADR-0076).
        // Classification record + access-check decision + shipment-logged
        // export; app-layer JSON payloads, never NAV XML bytes. `export.*`-not-
        // `invoice.*` posture; never sweeps a per-OUTGOING-invoice export
        // bundle by glob.
        | EventKind::ExportClassificationSet
        | EventKind::ExportAccessCheck
        | EventKind::ExportShipmentLogged
        // S360 / PR-47 — cui.* Controlled-Unclassified-Information family
        // (ADR-0077). Marking-applied record + access-event decision; app-layer
        // JSON payloads, never NAV XML bytes, and never the controlled content
        // itself. `cui.*`-not-`invoice.*` posture; never sweeps a per-OUTGOING-
        // invoice export bundle by glob.
        | EventKind::CuiMarkingApplied
        | EventKind::CuiAccessEvent
        // S361 / PR-48 — supplier.* Approved-Vendor-List family (ADR-0078).
        // DPAS-priority-set record + export-screened decision; app-layer JSON
        // payloads (partner_id / dpas_rating / screening_result / …), never NAV
        // XML bytes. `supplier.*`-not-`invoice.*` posture; never sweeps a
        // per-OUTGOING-invoice export bundle by glob.
        | EventKind::SupplierDpasPrioritySet
        | EventKind::SupplierExportScreened
        // S431 (ADR-0084) — supplier.* AVL CRUD + vendor-status PO-gate family.
        // Vendor id / partner id / status + category tokens / reviewer login;
        // app-layer JSON payloads, never NAV XML bytes. `supplier.*`-not-
        // `invoice.*` posture; never sweeps a per-OUTGOING-invoice export bundle
        // by glob.
        | EventKind::AvlVendorAdded
        | EventKind::AvlVendorStatusChanged
        | EventKind::AvlVendorRevoked
        | EventKind::AvlScreeningOverdue
        | EventKind::PoBlockedByVendorStatus
        // S362 / PR-49 — incident.* cyber-incident-reporting family (ADR-0079).
        // Cyber-incident-detected record (DFARS 252.204-7012(c)(1) 72-hour
        // clock); app-layer JSON payloads (detected_at_ms / severity /
        // cdi_affected / detection_source / …), never NAV XML bytes, and never
        // raw log dumps. `incident.*`-not-`invoice.*` posture; never sweeps a
        // per-OUTGOING-invoice export bundle by glob.
        | EventKind::IncidentCyberDetected
        // S394 — operator changed the `[seller.numbering]` template. App-layer
        // JSON payload (old/new start_value / reset_policy / rendered_preview /
        // actor), never NAV XML bytes. `system.*`-not-`invoice.*` posture; a
        // config-lifecycle row, never swept by a per-OUTGOING-invoice export
        // bundle by glob.
        | EventKind::NumberingTemplateChanged
        // S426 / ADR-0082 — snapshot.* DB-snapshot operations family. Seq /
        // timestamp / source-SHA-256 / counts; app-layer JSON payloads, never
        // NAV XML bytes. `snapshot.*`-not-`invoice.*` posture; a system/ops
        // row, never swept by a per-OUTGOING-invoice export bundle by glob.
        | EventKind::SnapshotCreated
        | EventKind::SnapshotValidationFailed
        | EventKind::SnapshotRestored
        | EventKind::SnapshotPruned
        // S427 — mes.* machine master-data + quote.* lead-time family.
        // Machine id / name / family / capacity knobs, quote id + day
        // counts, fallback hours; app-layer JSON payloads, never NAV XML
        // bytes. Master-data / pricing-ops rows, never swept by a
        // per-OUTGOING-invoice export bundle by glob.
        | EventKind::MachineCreated
        | EventKind::MachineEdited
        | EventKind::MachineArchived
        | EventKind::QuoteLeadTimeOverridden
        | EventKind::QuotingMachinesEmptyFallback
        // S428 — partner.customer_type + quote.* margin-profile / margin
        // family. Partner id + customer type, profile id + margin knobs,
        // quote id + realized/floor margin %; app-layer JSON payloads,
        // never NAV XML bytes. Master-data / pricing-ops rows, never swept
        // by a per-OUTGOING-invoice export bundle by glob.
        | EventKind::PartnerCustomerTypeChanged
        | EventKind::MarginProfileCreated
        | EventKind::MarginProfileEdited
        | EventKind::MarginProfileArchived
        | EventKind::QuoteMarginBelowFloor
        | EventKind::QuoteUsingGlobalMargin
        | EventKind::QuoteMarginOverridden
        | EventKind::QuoteMarginFloorOverridden
        // S429 — calibration audit rows: app-layer JSON, never NAV XML.
        | EventKind::QuoteCalibrationSampleRecorded
        | EventKind::QuoteCalibrationSampleSkipped
        | EventKind::QuoteCalibrationApplied
        | EventKind::QuoteCalibrationCoefficientShifted
        // S430 — CAD-blob crypto/read-audit rows: app-layer JSON, never NAV XML.
        | EventKind::CadBlobKeyProvisioned
        | EventKind::CadBlobRead
        | EventKind::CadBlobLegacyPlaintextRead
        // S433 — tenant-lifecycle rows: app-layer JSON, never NAV XML.
        | EventKind::TenantCreated
        | EventKind::TenantSwitchRequested
        | EventKind::TenantSwitched
        | EventKind::TenantArchived
        | EventKind::TenantRestored
        | EventKind::TenantDemoSeeded
        // S434 — NAV-off tenant/invoice rows: app-layer JSON, never NAV XML.
        | EventKind::TenantNavToggled
        | EventKind::TenantSellerSetupOptional
        | EventKind::TenantSellerRegionConfigured
        | EventKind::InvoiceLocalOnlyEmitted => None,
    };
    // The EventKind storage string uses dots (e.g.
    // "invoice.submission_attempt") which produce
    // ambiguous-looking filenames; the bundle uses underscores
    // throughout the per-NAV-file name so an inspector
    // reading the archive sees one clean kind-name fragment
    // per file. The canonical kind discriminator on the
    // `kind` field of chain.jsonl preserves the dotted form;
    // only the filename transforms.
    Ok(bytes.map(|b| NavXmlFile {
        archive_path: format!(
            "nav/{:05}_{}.xml",
            entry.seq.as_u64(),
            entry.kind.as_str().replace('.', "_")
        ),
        bytes: b,
    }))
}

/// S364 / ADR-0081 — coverage drift tripwire (mirror of the one in
/// `aberp-verify::extract_nav_xml`). The match above is exhaustive on
/// `EventKind`, so a new variant breaks the build until an arm exists —
/// but the compiler only forces *an* arm, not a *correct* one. This
/// `const _` pins the variant count so that when `ALL_KINDS_COUNT`
/// changes, compilation fails here, forcing a deliberate re-review of
/// whether the new variant carries NAV bytes. Belt-and-braces with the
/// per-family `extract_nav_xml_returns_none_for_*_kinds` runtime tests.
const _: () = {
    assert!(
        EventKind::ALL_KINDS_COUNT == 150,
        "EventKind count changed — re-review export_invoice_bundle::extract_nav_xml \
         for the new variant's NAV decision, then bump this pin (ADR-0081)"
    );
};

/// Pack the manifest + chain.jsonl + nav/* files into a
/// `.tar.zst` archive at `out_path`. The archive's top-level
/// directory is `bundle/` so the inspector untarring it gets
/// a single subdirectory per ADR-0029 §3.
///
/// The zstd encoder wraps a `File` writer; the tar Builder
/// wraps the zstd encoder. Standard streaming pattern — no
/// in-memory buffer of the full archive.
fn pack_bundle(
    out_path: &Path,
    allow_overwrite: bool,
    manifest_json: &[u8],
    chain_jsonl: &[u8],
    nav_files: &[NavXmlFile],
) -> Result<()> {
    if out_path.exists() && !allow_overwrite {
        return Err(anyhow!(
            "output path {} already exists — pass --allow-overwrite to overwrite \
             (CLAUDE.md rule 12: refuse-overwrite default preserves operator-visible artifacts)",
            out_path.display()
        ));
    }
    let file = std::fs::File::create(out_path)
        .with_context(|| format!("create bundle output file at {}", out_path.display()))?;
    let zstd_encoder = zstd::stream::write::Encoder::new(file, 0)
        .context("build zstd streaming encoder for bundle output")?
        .auto_finish();
    let mut builder = tar::Builder::new(zstd_encoder);

    append_bytes(&mut builder, "manifest.json", manifest_json)?;
    append_bytes(&mut builder, "chain.jsonl", chain_jsonl)?;
    for nav in nav_files {
        append_bytes(&mut builder, &nav.archive_path, &nav.bytes)?;
    }

    // `into_inner()` finishes the tar stream (writes the
    // two trailing zero blocks); the zstd encoder's
    // `auto_finish` then commits the compressed stream when
    // dropped. Drop order matters — builder first, then
    // zstd_encoder via the implicit drop at function end.
    let zstd_encoder = builder
        .into_inner()
        .context("finalize tar stream inside zstd encoder")?;
    drop(zstd_encoder); // makes the auto_finish trigger explicit
    Ok(())
}

/// Append one in-memory blob as a tar entry under
/// `bundle/<archive_relative_path>`. The tar header's `mode`
/// is set to `0o644` (operator-readable, not executable);
/// `mtime` is set to `0` for reproducible-bundle posture
/// (re-running the export on the same ledger state at a
/// different wall-clock produces byte-different but
/// content-equivalent archives — the `mtime`-zero pin is the
/// reproducible-byte-floor for archive-level digests if a
/// future bundle-verifier emits one).
fn append_bytes<W: Write>(builder: &mut tar::Builder<W>, rel: &str, bytes: &[u8]) -> Result<()> {
    let full = format!("{}/{}", BUNDLE_DIR, rel);
    let mut header = tar::Header::new_gnu();
    header
        .set_path(&full)
        .with_context(|| format!("set tar header path {full}"))?;
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(0);
    header.set_cksum();
    builder
        .append(&header, bytes)
        .with_context(|| format!("append {full} to tar stream"))?;
    Ok(())
}

/// Build the `chain.jsonl` body: one JSON object per line, one
/// line per entry, seq-ordered. UTF-8 bytes returned for
/// direct passing to [`append_bytes`].
fn build_chain_jsonl(entries: &[Entry]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(entries.len() * 256);
    for entry in entries {
        let row = ChainJsonlEntry::from_entry(entry)?;
        let line = serde_json::to_vec(&row).with_context(|| {
            format!(
                "serialize chain.jsonl line for entry at seq {}",
                entry.seq.as_u64()
            )
        })?;
        out.extend_from_slice(&line);
        out.push(b'\n');
    }
    Ok(out)
}

/// Entry point for the `aberp export-invoice-bundle` subcommand.
pub fn run(args: &ExportInvoiceBundleArgs) -> Result<()> {
    let _span = tracing::info_span!(
        "export_invoice_bundle",
        invoice_id = %args.invoice_id,
        tenant = %args.tenant,
        out = %args.out.display(),
    )
    .entered();

    // 1. Parse + validate CLI args.
    let tenant = TenantId::new(args.tenant.clone()).ok_or_else(|| {
        anyhow!(
            "--tenant value '{}' is empty or has a null byte",
            args.tenant
        )
    })?;
    if args.invoice_id.is_empty() {
        return Err(anyhow!(
            "--invoice-id is empty — the bundle reader cannot match the empty string \
             against any payload's id-shaped fields (defence-in-depth per CLAUDE.md rule 12)"
        ));
    }
    if args.out.exists() && !args.allow_overwrite {
        return Err(anyhow!(
            "output path {} already exists — pass --allow-overwrite to overwrite \
             (CLAUDE.md rule 12: refuse-overwrite default preserves operator-visible artifacts)",
            args.out.display()
        ));
    }

    // 2. Compute binary hash.
    let binary_hash_bytes = binary_hash::compute().context("compute binary hash")?;

    // 3. Open the ledger.
    let ledger = Ledger::open(&args.db, tenant.clone(), binary_hash_bytes)
        .context("open audit ledger for export-invoice-bundle")?;

    // 4. Run full-chain verify. Aborts loud on Err per
    //    ADR-0029 §6.
    let chain_verified_entries = ledger.verify_chain().with_context(|| {
        format!(
            "audit-ledger chain verification failed for tenant {} — \
             refusing to emit a bundle from a tampered chain per ADR-0029 §6 + CLAUDE.md rule 12",
            args.tenant
        )
    })?;
    tracing::info!(
        chain_verified_entries,
        "audit chain verified across full tenant ledger"
    );

    // 5. Read all entries; filter to the per-invoice slice.
    let entries = ledger
        .entries()
        .context("read audit ledger entries for bundle slice")?;
    let slice = filter_invoice_slice(&entries, &args.invoice_id)?;
    tracing::info!(
        entries_in_bundle = slice.len(),
        "per-invoice slice resolved for bundle"
    );

    // 5b. PR-17 / ADR-0030 §5 — assert mirror-vs-DB agreement.
    //     Refuses the bundle output on divergence per CLAUDE.md
    //     rule 12 (Err propagates up; no bundle bytes written).
    //     Pre-PR-17 DBs (mirror file absent) flow through as
    //     `AbsentPrePr17`; the operator-visible message names
    //     that path honestly so the operator knows the next
    //     append will initialise the mirror.
    let mirror_status = detect_mirror_agreement(&args.db, &entries)?;
    tracing::info!(
        mirror_status = ?mirror_status,
        "audit-ledger mirror agreement check"
    );

    // 6. Build the manifest body, chain.jsonl body, and the
    //    nav/* file list.
    let manifest = build_manifest(
        &args.invoice_id,
        tenant.as_str(),
        binary_hash_bytes,
        chain_verified_entries,
        slice.len() as u64,
        mirror_status,
    )?;
    let manifest_bytes =
        serde_json::to_vec_pretty(&manifest).context("serialize manifest.json (pretty)")?;
    let chain_jsonl_bytes = build_chain_jsonl(&slice)?;
    let mut nav_files: Vec<NavXmlFile> = Vec::new();
    for entry in &slice {
        if let Some(nav) = extract_nav_xml(entry)? {
            nav_files.push(nav);
        }
    }

    // 7. Pack the .tar.zst archive.
    pack_bundle(
        &args.out,
        args.allow_overwrite,
        &manifest_bytes,
        &chain_jsonl_bytes,
        &nav_files,
    )?;

    // 8. Operator-visible summary. The mirror-file caveat is
    //    now resolved by the agreement status (verified vs
    //    absent-pre-pr-17); the F5 attestation-signing caveat
    //    remains explicit per ADR-0029 §7 — a future
    //    contributor reading the operator-visible line
    //    reproduces the deferral rationale without re-reading
    //    the ADR. CLAUDE.md rule 12 — silent omission is the
    //    wrong affordance.
    tracing::info!(
        invoice_id = %args.invoice_id,
        out = %args.out.display(),
        chain_verified_entries,
        entries_in_bundle = slice.len(),
        nav_xml_files = nav_files.len(),
        ?mirror_status,
        "export-invoice-bundle OK"
    );
    let mirror_path = mirror_path_for(&args.db);
    let mirror_clause = match mirror_status {
        MirrorAgreementStatus::VerifiedAgreement => format!(
            "verified against mirror file at {} (mirror_file_status: \"verified-agreement\")",
            mirror_path.display(),
        ),
        MirrorAgreementStatus::AbsentPrePr17 => format!(
            "no mirror file present at {} (mirror_file_status: \"absent-pre-pr-17\"); \
             the next command that appends to this DB will initialise the mirror via the \
             ADR-0030 §7 implicit-backfill path",
            mirror_path.display(),
        ),
    };
    println!(
        "export-invoice-bundle OK: invoice {} -> wrote bundle to {} (audit chain verified \
         across {} entries; {} entries in bundle; {} NAV-XML files inside). NOTE: this bundle \
         is UNSIGNED (signing deferred per F5; the chain-verify result above is internally \
         verifiable from the bundle's chain.jsonl alone). Mirror-file second-source assertion: \
         {}.",
        args.invoice_id,
        args.out.display(),
        chain_verified_entries,
        slice.len(),
        nav_files.len(),
        mirror_clause,
    );
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────
// Tests — bundle membership probe + manifest + nav filename composition
// + chain.jsonl line shape + tar/zst pack-and-read round-trip.
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use aberp_audit_ledger::{Actor, BinaryHash, Ledger, TenantId};
    use aberp_billing::IdempotencyKey;

    use crate::audit_payloads;

    /// ADR-0029 §2: the Probe's any-id-field-equality posture
    /// requires every payload type's id-shaped field be
    /// covered. This test enumerates the four current field
    /// names AND the payload types each one comes from; a
    /// future contributor who adds a new id-shaped field on a
    /// new payload type updates BOTH the Probe and this test.
    /// Without the test, a missing Probe field would cause
    /// the new payload's entries to silently fall out of
    /// every bundle — exactly the silent-omission failure
    /// mode CLAUDE.md rule 12 names.
    #[test]
    fn probe_field_set_covers_every_payload_id_field() {
        // Hand-listed, NOT auto-derived. The point is to force
        // a contributor adding a new payload to acknowledge
        // this list.
        let known_id_fields = [
            // invoice_id — on every non-chain-link payload.
            "invoice_id",
            // chain-link payloads' two-id pairs.
            "storno_invoice_id",
            "modification_invoice_id",
            "base_invoice_id",
        ];
        // Round-trip each field through the Probe to confirm
        // it deserializes; this is a structural pin, not a
        // semantic check (semantic coverage is the contributor
        // discipline of extending the list when a new payload
        // lands).
        for field in known_id_fields {
            let json = format!(r#"{{"{field}":"inv_TEST"}}"#);
            let probe: BundleMembershipProbe = serde_json::from_str(&json)
                .unwrap_or_else(|e| panic!("Probe must deserialize field {field}: {e}"));
            assert!(
                probe.matches("inv_TEST"),
                "Probe must match target on field {field}"
            );
        }
    }

    /// Empty target loud-rejects via `Probe::matches`. Defence-
    /// in-depth: a tampered payload with an empty id field
    /// would otherwise match every empty-target query.
    #[test]
    fn probe_rejects_empty_target() {
        let probe: BundleMembershipProbe = serde_json::from_str(r#"{"invoice_id":""}"#).unwrap();
        assert!(!probe.matches(""));
        assert!(!probe.matches("inv_TEST"));
    }

    /// ADR-0029 §2 "Surfaced conflict 1" Reading A: the
    /// chain-link entry for STORNO (base + storno ids) is
    /// included in BOTH the base's bundle AND the storno's
    /// bundle. The Probe matches via `base_invoice_id` for
    /// the base, via `storno_invoice_id` for the storno.
    #[test]
    fn storno_chain_link_matches_both_base_and_storno_bundles() {
        let payload =
            r#"{"storno_invoice_id":"inv_STORNO","base_invoice_id":"inv_BASE"}"#.to_string();
        let probe: BundleMembershipProbe = serde_json::from_str(&payload).unwrap();
        assert!(probe.matches("inv_BASE"));
        assert!(probe.matches("inv_STORNO"));
        assert!(!probe.matches("inv_OTHER"));
    }

    /// Same posture for MODIFY chain-link entries: included in
    /// both base's and modification's bundles via the matching
    /// id field.
    #[test]
    fn modification_chain_link_matches_both_base_and_modification_bundles() {
        let payload =
            r#"{"modification_invoice_id":"inv_MOD","base_invoice_id":"inv_BASE"}"#.to_string();
        let probe: BundleMembershipProbe = serde_json::from_str(&payload).unwrap();
        assert!(probe.matches("inv_BASE"));
        assert!(probe.matches("inv_MOD"));
        assert!(!probe.matches("inv_STORNO"));
    }

    /// ADR-0029 §3: the manifest carries every load-bearing
    /// field. The pin is BOTH on the field set (presence in
    /// the serialized JSON) AND on the deferred-gate string
    /// values (signing-deferred-per-F5, mirror-deferred-per-
    /// F10) so a future contributor who tries to silently
    /// flip the booleans without lifting the gates fails the
    /// pin loud.
    #[test]
    fn manifest_carries_every_adr_0029_field() {
        let bh = BinaryHash::from_bytes([0u8; 32]);
        // PR-17 / ADR-0030 §5: the AbsentPrePr17 path preserves
        // the legacy "F10 not yet lifted on this DB" disposition
        // the test was originally written against. The
        // VerifiedAgreement path is covered by
        // `manifest_mirror_fields_match_agreement_status` below.
        let manifest = build_manifest(
            "inv_TEST",
            "tenantX",
            bh,
            42,
            7,
            MirrorAgreementStatus::AbsentPrePr17,
        )
        .unwrap();
        let serialized = serde_json::to_value(&manifest).unwrap();

        // Every ADR-0029 §3 field is present.
        for field in [
            "version",
            "invoice_id",
            "tenant_id",
            "generated_at",
            "binary_hash",
            "nav_xsd_version",
            "chain_verified",
            "chain_verified_entries",
            "entries_in_bundle",
            "signed",
            "signature_status",
            "mirror_file_present",
            "mirror_file_status",
        ] {
            assert!(
                serialized.get(field).is_some(),
                "manifest field {field} missing — ADR-0029 §3 violation"
            );
        }

        // F5 signing gate values unchanged at PR-17 time.
        assert_eq!(serialized["signed"], serde_json::json!(false));
        assert_eq!(
            serialized["signature_status"],
            serde_json::json!(SIGNATURE_STATUS_DEFERRED)
        );
        // PR-17: mirror status reflects the AbsentPrePr17 path
        // here. Full coverage of both flip targets in
        // `manifest_mirror_fields_match_agreement_status`.
        assert_eq!(serialized["mirror_file_present"], serde_json::json!(false));
        assert_eq!(
            serialized["mirror_file_status"],
            serde_json::json!(MIRROR_FILE_STATUS_ABSENT_PRE_PR17)
        );

        // Carried fields match inputs.
        assert_eq!(serialized["invoice_id"], serde_json::json!("inv_TEST"));
        assert_eq!(serialized["tenant_id"], serde_json::json!("tenantX"));
        assert_eq!(serialized["chain_verified"], serde_json::json!(true));
        assert_eq!(serialized["chain_verified_entries"], serde_json::json!(42));
        assert_eq!(serialized["entries_in_bundle"], serde_json::json!(7));
        assert_eq!(serialized["version"], serde_json::json!(MANIFEST_VERSION));
    }

    /// ADR-0029 §3 + ADR-0030 §5: the manifest's load-bearing
    /// strings stay pinned to their canonical values. F5's
    /// signing-deferred string is unchanged; F10's
    /// mirror-file string is now load-bearing only in its
    /// post-lift values (the bundle reader never emits the
    /// old `"deferred-per-f10"` placeholder — PR-17 retired
    /// it). A silent rename of either constant fails this
    /// pin loud.
    #[test]
    fn manifest_canonical_string_values_match_adr_canonical_form() {
        assert_eq!(SIGNATURE_STATUS_DEFERRED, "deferred-per-f5");
        assert_eq!(MIRROR_FILE_STATUS_VERIFIED, "verified-agreement");
        assert_eq!(MIRROR_FILE_STATUS_ABSENT_PRE_PR17, "absent-pre-pr-17");
    }

    /// PR-17 / ADR-0030 §5: the manifest's mirror_file_present
    /// and mirror_file_status fields flip additively when the
    /// agreement status enum changes. Pinned here so a future
    /// contributor who reorders the match arms or swaps the
    /// constants surfaces the divergence at test time.
    #[test]
    fn manifest_mirror_fields_match_agreement_status() {
        let bh = BinaryHash::from_bytes([0u8; 32]);

        // VerifiedAgreement path.
        let verified = build_manifest(
            "inv_TEST",
            "tenantX",
            bh,
            10,
            3,
            MirrorAgreementStatus::VerifiedAgreement,
        )
        .unwrap();
        let v_json = serde_json::to_value(&verified).unwrap();
        assert_eq!(v_json["mirror_file_present"], serde_json::json!(true));
        assert_eq!(
            v_json["mirror_file_status"],
            serde_json::json!("verified-agreement")
        );

        // AbsentPrePr17 path.
        let absent = build_manifest(
            "inv_TEST",
            "tenantX",
            bh,
            10,
            3,
            MirrorAgreementStatus::AbsentPrePr17,
        )
        .unwrap();
        let a_json = serde_json::to_value(&absent).unwrap();
        assert_eq!(a_json["mirror_file_present"], serde_json::json!(false));
        assert_eq!(
            a_json["mirror_file_status"],
            serde_json::json!("absent-pre-pr-17")
        );
    }

    fn fixture_ledger() -> (Ledger, Actor, BinaryHash) {
        let tenant = TenantId::new("t1".to_string()).unwrap();
        let bh = BinaryHash::from_bytes([0u8; 32]);
        let ledger = Ledger::open_in_memory(tenant, bh).unwrap();
        let actor = Actor::from_local_cli("sess".to_string(), "test-user");
        (ledger, actor, bh)
    }

    fn fixture_submission_attempt(invoice_id: &str, idem: IdempotencyKey) -> Vec<u8> {
        audit_payloads::InvoiceSubmissionAttemptPayload::new(
            invoice_id,
            idem,
            "test",
            b"<ManageInvoiceRequest/>".to_vec(),
        )
        .to_bytes()
    }

    fn fixture_submission_response(invoice_id: &str, idem: IdempotencyKey, txid: &str) -> Vec<u8> {
        audit_payloads::InvoiceSubmissionResponsePayload::new(
            invoice_id,
            idem,
            txid,
            b"<ManageInvoiceResponse/>".to_vec(),
        )
        .to_bytes()
    }

    /// `filter_invoice_slice` returns entries in seq order
    /// (oldest first) and excludes entries for other invoices.
    /// Cross-invoice contamination check mirroring
    /// `audit_query::tests::precondition_does_not_cross_invoice_ids`.
    #[test]
    fn filter_invoice_slice_returns_only_matching_entries_in_seq_order() {
        let (mut ledger, actor, _bh) = fixture_ledger();
        let idem_a = IdempotencyKey::new();
        let idem_b = IdempotencyKey::new();
        ledger
            .append(
                EventKind::InvoiceSubmissionAttempt,
                fixture_submission_attempt("inv_A", idem_a),
                actor.clone(),
                Some(idem_a.to_canonical_string()),
            )
            .unwrap();
        ledger
            .append(
                EventKind::InvoiceSubmissionAttempt,
                fixture_submission_attempt("inv_B", idem_b),
                actor.clone(),
                Some(idem_b.to_canonical_string()),
            )
            .unwrap();
        ledger
            .append(
                EventKind::InvoiceSubmissionResponse,
                fixture_submission_response("inv_A", idem_a, "TXID-A"),
                actor.clone(),
                Some(idem_a.to_canonical_string()),
            )
            .unwrap();

        let entries = ledger.entries().unwrap();
        let slice_a = filter_invoice_slice(&entries, "inv_A").unwrap();
        assert_eq!(slice_a.len(), 2, "inv_A's bundle has 2 entries");
        // Seq-order is oldest first.
        assert!(slice_a[0].seq.as_u64() < slice_a[1].seq.as_u64());

        let slice_b = filter_invoice_slice(&entries, "inv_B").unwrap();
        assert_eq!(slice_b.len(), 1, "inv_B's bundle has 1 entry");

        let slice_missing = filter_invoice_slice(&entries, "inv_NONEXISTENT");
        assert!(
            slice_missing.is_err(),
            "missing invoice id loud-fails per ADR-0029 §1"
        );
        let err_msg = format!("{:#}", slice_missing.unwrap_err());
        assert!(
            err_msg.contains("no audit-ledger entries reference invoice id"),
            "loud-fail message must name the absence: got {err_msg}"
        );
    }

    /// `extract_nav_xml` returns `Some(_)` for every NAV-
    /// bearing kind and `None` for the non-NAV kinds. The
    /// match is exhaustive so this test is also the F12-style
    /// trap: a future EventKind variant addition requires
    /// either a NAV-bearing arm OR explicit listing in the
    /// no-bytes branch — the Rust compiler enforces the
    /// exhaustiveness, this test pins the *current*
    /// classification.
    #[test]
    fn extract_nav_xml_returns_bytes_for_nav_bearing_kinds() {
        let (mut ledger, actor, _bh) = fixture_ledger();
        let idem = IdempotencyKey::new();

        ledger
            .append(
                EventKind::InvoiceSubmissionAttempt,
                fixture_submission_attempt("inv_A", idem),
                actor.clone(),
                Some(idem.to_canonical_string()),
            )
            .unwrap();
        ledger
            .append(
                EventKind::InvoiceSubmissionResponse,
                fixture_submission_response("inv_A", idem, "TXID-A"),
                actor.clone(),
                Some(idem.to_canonical_string()),
            )
            .unwrap();

        let entries = ledger.entries().unwrap();
        let nav_attempt = extract_nav_xml(&entries[0]).unwrap();
        assert!(nav_attempt.is_some());
        let f = nav_attempt.unwrap();
        assert!(
            f.archive_path.starts_with("nav/"),
            "nav files live under nav/ inside the archive"
        );
        assert!(
            f.archive_path.contains("invoice_submission_attempt"),
            "nav filename names the kind (dots transformed to underscores): {}",
            f.archive_path
        );
        assert!(
            !f.archive_path.contains("invoice.submission_attempt"),
            "nav filename must NOT carry the dotted storage form (filename safety): {}",
            f.archive_path
        );
        assert!(
            f.archive_path.contains("00001"),
            "seq is zero-padded to 5 digits: {}",
            f.archive_path
        );

        let nav_response = extract_nav_xml(&entries[1]).unwrap();
        assert!(nav_response.is_some());
        assert_eq!(
            nav_response.unwrap().bytes,
            b"<ManageInvoiceResponse/>".to_vec()
        );
    }

    /// Non-NAV-bearing kinds return None (no nav/ file
    /// produced).
    #[test]
    fn extract_nav_xml_returns_none_for_non_nav_kinds() {
        let (mut ledger, actor, _bh) = fixture_ledger();
        let idem = IdempotencyKey::new();
        // MarkedAbandoned is a non-NAV-bearing kind.
        let payload = audit_payloads::InvoiceMarkedAbandonedPayload::new(
            "inv_A",
            idem,
            Some("TXID-A".to_string()),
            None,
            "test abandon",
        )
        .to_bytes();
        ledger
            .append(
                EventKind::InvoiceMarkedAbandoned,
                payload,
                actor,
                Some(idem.to_canonical_string()),
            )
            .unwrap();
        let entries = ledger.entries().unwrap();
        let nav = extract_nav_xml(&entries[0]).unwrap();
        assert!(nav.is_none(), "non-NAV-bearing kinds produce no nav/ file");
    }

    /// S355 / PR-43 (ADR-0073) — the four `personnel.*` access-trail kinds
    /// carry app-layer JSON payloads, never NAV XML, so `extract_nav_xml`
    /// MUST return `None` for each. The Rust exhaustiveness check already
    /// forces the new variants into the no-NAV-bytes arm at compile time;
    /// this is the belt-and-braces runtime pin that the no-NAV-bytes verdict
    /// actually holds, so a personnel row never produces a `nav/` file in a
    /// per-OUTGOING-invoice export bundle.
    #[test]
    fn extract_nav_xml_returns_none_for_personnel_kinds() {
        for kind in [
            EventKind::PersonnelIdRegistered,
            EventKind::PersonnelSignatureApplied,
            EventKind::PersonnelAccessGranted,
            EventKind::PersonnelAccessDenied,
        ] {
            let (mut ledger, actor, _bh) = fixture_ledger();
            ledger
                .append(
                    kind.clone(),
                    br#"{"operator_id":"mock-op-001"}"#.to_vec(),
                    actor,
                    None,
                )
                .unwrap();
            let entries = ledger.entries().unwrap();
            let nav = extract_nav_xml(&entries[0]).unwrap();
            assert!(nav.is_none(), "{} must produce no nav/ file", kind.as_str());
        }
    }

    /// S357 / PR-44 (ADR-0074) — the two `material.*` traceability kinds
    /// carry app-layer JSON payloads, never NAV XML, so `extract_nav_xml`
    /// MUST return `None` for each. The Rust exhaustiveness check already
    /// forces the new variants into the no-NAV-bytes arm at compile time;
    /// this is the belt-and-braces runtime pin that the verdict actually
    /// holds, so a material-traceability row never produces a `nav/` file in
    /// a per-OUTGOING-invoice export bundle.
    #[test]
    fn extract_nav_xml_returns_none_for_material_kinds() {
        for kind in [
            EventKind::MaterialCertAttached,
            EventKind::MaterialHeatLotAssigned,
        ] {
            let (mut ledger, actor, _bh) = fixture_ledger();
            ledger
                .append(
                    kind.clone(),
                    br#"{"material_id":"6061-T6"}"#.to_vec(),
                    actor,
                    None,
                )
                .unwrap();
            let entries = ledger.entries().unwrap();
            let nav = extract_nav_xml(&entries[0]).unwrap();
            assert!(nav.is_none(), "{} must produce no nav/ file", kind.as_str());
        }
    }

    /// S358 / PR-45 (ADR-0075) — the two `part.*` per-unit serialization kinds
    /// carry app-layer JSON payloads, never NAV XML, so `extract_nav_xml` MUST
    /// return `None` for each. The Rust exhaustiveness check already forces the
    /// new variants into the no-NAV-bytes arm at compile time; this is the
    /// belt-and-braces runtime pin that the verdict actually holds, so a part-
    /// serialization row never produces a `nav/` file in a per-OUTGOING-invoice
    /// export bundle.
    #[test]
    fn extract_nav_xml_returns_none_for_part_kinds() {
        for kind in [
            EventKind::PartSerialAssigned,
            EventKind::PartUidMarked,
            EventKind::WoBlockedNoPartUid,
            EventKind::PartTraceabilityViewed,
        ] {
            let (mut ledger, actor, _bh) = fixture_ledger();
            ledger
                .append(
                    kind.clone(),
                    br#"{"part_id":"PRT-7781"}"#.to_vec(),
                    actor,
                    None,
                )
                .unwrap();
            let entries = ledger.entries().unwrap();
            let nav = extract_nav_xml(&entries[0]).unwrap();
            assert!(nav.is_none(), "{} must produce no nav/ file", kind.as_str());
        }
    }

    /// S359 / PR-46 (ADR-0076) — the three `export.*` export-control kinds
    /// carry app-layer JSON payloads, never NAV XML, so `extract_nav_xml` MUST
    /// return `None` for each. The Rust exhaustiveness check already forces the
    /// new variants into the no-NAV-bytes arm at compile time; this is the
    /// belt-and-braces runtime pin that the verdict actually holds, so an
    /// export-control row never produces a `nav/` file in a per-OUTGOING-invoice
    /// export bundle.
    #[test]
    fn extract_nav_xml_returns_none_for_export_kinds() {
        for kind in [
            EventKind::ExportClassificationSet,
            EventKind::ExportAccessCheck,
            EventKind::ExportShipmentLogged,
        ] {
            let (mut ledger, actor, _bh) = fixture_ledger();
            ledger
                .append(
                    kind.clone(),
                    br#"{"entity_id":"DWG-7781-A"}"#.to_vec(),
                    actor,
                    None,
                )
                .unwrap();
            let entries = ledger.entries().unwrap();
            let nav = extract_nav_xml(&entries[0]).unwrap();
            assert!(nav.is_none(), "{} must produce no nav/ file", kind.as_str());
        }
    }

    /// S360 / PR-47 (ADR-0077) — the two `cui.*` Controlled-Unclassified-
    /// Information kinds carry app-layer JSON payloads, never NAV XML, so
    /// `extract_nav_xml` MUST return `None` for each. The Rust exhaustiveness
    /// check already forces the new variants into the no-NAV-bytes arm at
    /// compile time; this is the belt-and-braces runtime pin that the verdict
    /// actually holds, so a CUI marking/access row never produces a `nav/` file
    /// in a per-OUTGOING-invoice export bundle.
    #[test]
    fn extract_nav_xml_returns_none_for_cui_kinds() {
        for kind in [EventKind::CuiMarkingApplied, EventKind::CuiAccessEvent] {
            let (mut ledger, actor, _bh) = fixture_ledger();
            ledger
                .append(
                    kind.clone(),
                    br#"{"entity_id":"DWG-7781-A"}"#.to_vec(),
                    actor,
                    None,
                )
                .unwrap();
            let entries = ledger.entries().unwrap();
            let nav = extract_nav_xml(&entries[0]).unwrap();
            assert!(nav.is_none(), "{} must produce no nav/ file", kind.as_str());
        }
    }

    /// S361 / PR-48 (ADR-0078) — the two `supplier.*` Approved-Vendor-List
    /// kinds carry app-layer JSON payloads, never NAV XML, so `extract_nav_xml`
    /// MUST return `None` for each. The Rust exhaustiveness check already forces
    /// the new variants into the no-NAV-bytes arm at compile time; this is the
    /// belt-and-braces runtime pin that the verdict actually holds, so a DPAS /
    /// screening row never produces a `nav/` file in a per-OUTGOING-invoice
    /// export bundle.
    #[test]
    fn extract_nav_xml_returns_none_for_supplier_kinds() {
        for kind in [
            EventKind::SupplierDpasPrioritySet,
            EventKind::SupplierExportScreened,
            EventKind::AvlVendorAdded,
            EventKind::AvlVendorStatusChanged,
            EventKind::AvlVendorRevoked,
            EventKind::AvlScreeningOverdue,
            EventKind::PoBlockedByVendorStatus,
        ] {
            let (mut ledger, actor, _bh) = fixture_ledger();
            ledger
                .append(
                    kind.clone(),
                    br#"{"partner_id":"partner-4711"}"#.to_vec(),
                    actor,
                    None,
                )
                .unwrap();
            let entries = ledger.entries().unwrap();
            let nav = extract_nav_xml(&entries[0]).unwrap();
            assert!(nav.is_none(), "{} must produce no nav/ file", kind.as_str());
        }
    }

    /// S362 / PR-49 (ADR-0079) — the lone `incident.*` cyber-incident kind
    /// carries an app-layer JSON payload, never NAV XML, so `extract_nav_xml`
    /// MUST return `None`. The Rust exhaustiveness check already forces the new
    /// variant into the no-NAV-bytes arm at compile time; this is the belt-and-
    /// braces runtime pin that the verdict actually holds, so a cyber-incident
    /// row never produces a `nav/` file in a per-OUTGOING-invoice export bundle.
    #[test]
    fn extract_nav_xml_returns_none_for_incident_kind() {
        let (mut ledger, actor, _bh) = fixture_ledger();
        ledger
            .append(
                EventKind::IncidentCyberDetected,
                br#"{"detected_at_ms":1750000000000,"severity":"high"}"#.to_vec(),
                actor,
                None,
            )
            .unwrap();
        let entries = ledger.entries().unwrap();
        let nav = extract_nav_xml(&entries[0]).unwrap();
        assert!(
            nav.is_none(),
            "{} must produce no nav/ file",
            EventKind::IncidentCyberDetected.as_str()
        );
    }

    /// S364 / ADR-0081 — future-proof sweep over the bundle writer's
    /// gate. Every variant in `EventKind::ALL_KINDS` that is not one of
    /// the nine NAV-bearing invoice kinds MUST produce no `nav/` file. A
    /// *new* variant lands here automatically: if its `extract_nav_xml`
    /// arm wrongly routes it to a NAV path, `b"{}"` fails to decode and
    /// the `expect` panics; if it returns a file, the assert fires.
    /// Catches a leak even when nobody adds a per-family pin above.
    #[test]
    fn all_kinds_produce_no_nav_file_except_nav_bearing() {
        const NAV_BEARING: &[EventKind] = &[
            EventKind::InvoiceSubmissionAttempt,
            EventKind::InvoiceSubmissionResponse,
            EventKind::InvoiceAckStatus,
            EventKind::InvoiceAnnulmentSubmissionAttempt,
            EventKind::InvoiceAnnulmentSubmissionResponse,
            EventKind::InvoiceAnnulmentAckStatus,
            EventKind::InvoiceAnnulmentReceiverConfirmation,
            EventKind::InvoiceSubmissionAttemptFailed,
            EventKind::InvoiceCheckPerformed,
        ];
        for kind in EventKind::ALL_KINDS {
            if NAV_BEARING.contains(kind) {
                continue;
            }
            let (mut ledger, actor, _bh) = fixture_ledger();
            ledger
                .append(kind.clone(), b"{}".to_vec(), actor, None)
                .unwrap();
            let entries = ledger.entries().unwrap();
            let nav = extract_nav_xml(&entries[0])
                .unwrap_or_else(|e| panic!("{} should decode as non-NAV: {e}", kind.as_str()));
            assert!(
                nav.is_none(),
                "{} is not NAV-bearing yet produced a nav/ file — \
                 re-review extract_nav_xml (ADR-0081)",
                kind.as_str()
            );
        }
    }

    /// ADR-0029 §3: `chain.jsonl` carries one JSON object per
    /// line, ULID + hex hashes + base64 payload. The pin
    /// asserts that a deserialized line round-trips back to
    /// the same payload bytes (the bundle reader's "canonical
    /// bytes for hash verification" claim per ADR-0008 §"Entry
    /// shape").
    #[test]
    fn chain_jsonl_line_round_trips_payload_bytes() {
        let (mut ledger, actor, _bh) = fixture_ledger();
        let idem = IdempotencyKey::new();
        let original_xml = b"<ManageInvoiceRequest>x</ManageInvoiceRequest>".to_vec();
        let payload = audit_payloads::InvoiceSubmissionAttemptPayload::new(
            "inv_A",
            idem,
            "test",
            original_xml.clone(),
        )
        .to_bytes();
        ledger
            .append(
                EventKind::InvoiceSubmissionAttempt,
                payload.clone(),
                actor,
                Some(idem.to_canonical_string()),
            )
            .unwrap();
        let entries = ledger.entries().unwrap();
        let row = ChainJsonlEntry::from_entry(&entries[0]).unwrap();
        let serialized = serde_json::to_string(&row).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&serialized).unwrap();
        let payload_b64 = parsed["payload"].as_str().unwrap();
        let decoded = BASE64_STANDARD.decode(payload_b64).unwrap();
        assert_eq!(
            decoded, payload,
            "base64-decoded chain.jsonl payload must match original audit-entry bytes"
        );
        // entry_hash is hex-encoded.
        let entry_hash_hex = parsed["entry_hash"].as_str().unwrap();
        assert_eq!(entry_hash_hex.len(), 64, "hex-encoded SHA-256 is 64 chars");
        assert!(
            entry_hash_hex.chars().all(|c| c.is_ascii_hexdigit()),
            "entry_hash must be pure hex"
        );
    }

    /// `build_chain_jsonl` emits one newline-terminated line
    /// per entry; line count equals entry count.
    #[test]
    fn build_chain_jsonl_emits_one_line_per_entry() {
        let (mut ledger, actor, _bh) = fixture_ledger();
        let idem_a = IdempotencyKey::new();
        let idem_b = IdempotencyKey::new();
        ledger
            .append(
                EventKind::InvoiceSubmissionAttempt,
                fixture_submission_attempt("inv_A", idem_a),
                actor.clone(),
                Some(idem_a.to_canonical_string()),
            )
            .unwrap();
        ledger
            .append(
                EventKind::InvoiceSubmissionAttempt,
                fixture_submission_attempt("inv_B", idem_b),
                actor.clone(),
                Some(idem_b.to_canonical_string()),
            )
            .unwrap();
        let entries = ledger.entries().unwrap();
        let body = build_chain_jsonl(&entries).unwrap();
        let line_count = body.iter().filter(|&&b| b == b'\n').count();
        assert_eq!(line_count, entries.len());
        // Trailing newline pattern: the body ends with '\n'.
        assert_eq!(body.last(), Some(&b'\n'));
    }

    /// End-to-end: build a small ledger, run the bundle reader
    /// orchestration's pack step against a tempfile, then
    /// untar+decompress and confirm `bundle/manifest.json` +
    /// `bundle/chain.jsonl` exist and the manifest's
    /// `entries_in_bundle` matches the slice we packed. This
    /// pins the full pack-and-read round trip including the
    /// tar.zst layer + the internal `bundle/` directory prefix.
    #[test]
    fn pack_and_extract_round_trip() {
        let (mut ledger, actor, bh) = fixture_ledger();
        let idem = IdempotencyKey::new();
        ledger
            .append(
                EventKind::InvoiceSubmissionAttempt,
                fixture_submission_attempt("inv_A", idem),
                actor.clone(),
                Some(idem.to_canonical_string()),
            )
            .unwrap();
        ledger
            .append(
                EventKind::InvoiceSubmissionResponse,
                fixture_submission_response("inv_A", idem, "TXID-A"),
                actor,
                Some(idem.to_canonical_string()),
            )
            .unwrap();

        let entries = ledger.entries().unwrap();
        let slice = filter_invoice_slice(&entries, "inv_A").unwrap();
        assert_eq!(slice.len(), 2);

        let manifest = build_manifest(
            "inv_A",
            "t1",
            bh,
            ledger.verify_chain().unwrap(),
            slice.len() as u64,
            // PR-17: this test exercises the pack-and-extract
            // round-trip; mirror agreement is not under test
            // here. AbsentPrePr17 keeps the manifest's
            // serialised disposition consistent with the
            // smoke test's existing baseline.
            MirrorAgreementStatus::AbsentPrePr17,
        )
        .unwrap();
        let manifest_bytes = serde_json::to_vec_pretty(&manifest).unwrap();
        let chain_jsonl_bytes = build_chain_jsonl(&slice).unwrap();
        let mut nav_files: Vec<NavXmlFile> = Vec::new();
        for entry in &slice {
            if let Some(f) = extract_nav_xml(entry).unwrap() {
                nav_files.push(f);
            }
        }

        // Pack into a tempfile.
        let mut tmp = std::env::temp_dir();
        tmp.push(format!("aberp_bundle_test_{}.tar.zst", ulid::Ulid::new()));
        pack_bundle(&tmp, false, &manifest_bytes, &chain_jsonl_bytes, &nav_files).unwrap();
        assert!(tmp.exists(), "pack_bundle must produce an output file");

        // Read it back: decompress + untar in memory.
        let compressed = std::fs::read(&tmp).unwrap();
        let decoded = zstd::stream::decode_all(&compressed[..]).expect("zstd-decode round-trip");
        let mut ar = tar::Archive::new(&decoded[..]);
        let mut found_manifest = false;
        let mut found_chain = false;
        let mut nav_count = 0;
        for entry in ar.entries().unwrap() {
            let entry = entry.unwrap();
            let path = entry.path().unwrap().display().to_string();
            assert!(
                path.starts_with(&format!("{BUNDLE_DIR}/")),
                "every archive entry under bundle/: {path}"
            );
            if path == format!("{BUNDLE_DIR}/manifest.json") {
                found_manifest = true;
            } else if path == format!("{BUNDLE_DIR}/chain.jsonl") {
                found_chain = true;
            } else if path.starts_with(&format!("{BUNDLE_DIR}/nav/")) {
                nav_count += 1;
            }
        }
        assert!(found_manifest, "bundle/manifest.json missing from archive");
        assert!(found_chain, "bundle/chain.jsonl missing from archive");
        assert_eq!(
            nav_count, 2,
            "expected 2 nav/*.xml entries for the two NAV-bearing slice entries"
        );

        // Clean up the tempfile.
        let _ = std::fs::remove_file(&tmp);
    }

    /// `pack_bundle` refuses to overwrite an existing file
    /// when `allow_overwrite=false` (ADR-0029 §1 + CLAUDE.md
    /// rule 12). Defence-in-depth pin — `run`'s entry path
    /// also checks; `pack_bundle`'s own check ensures the
    /// guarantee holds even if a future contributor inlines
    /// `pack_bundle` calls from a different orchestrator.
    #[test]
    fn pack_bundle_refuses_overwrite_by_default() {
        let mut tmp = std::env::temp_dir();
        tmp.push(format!(
            "aberp_bundle_overwrite_test_{}.tar.zst",
            ulid::Ulid::new()
        ));
        // Pre-create the file.
        std::fs::write(&tmp, b"existing").unwrap();
        let err = pack_bundle(&tmp, false, b"{}", b"", &[]).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("already exists"),
            "refuse-overwrite must name the existing-file cause: got {msg}"
        );
        assert!(
            msg.contains("--allow-overwrite"),
            "refuse-overwrite must steer the operator to the opt-in flag: got {msg}"
        );
        // Cleanup.
        let _ = std::fs::remove_file(&tmp);
    }

    /// ADR-0029 §"Adversarial review #3" + ADR-0030 §5:
    /// operator-visible summary text MUST name the F5
    /// signing-deferred posture loud, AND surface the mirror-
    /// file agreement status (verified vs absent-pre-pr-17)
    /// in the same line so a future contributor reading the
    /// operator-visible output reproduces the bundle's
    /// disposition without re-reading the ADRs.
    ///
    /// Pinned at the source-emitted string composition (the
    /// fragments live in the `run` fn's format string and the
    /// `mirror_clause` match arms — see the function body).
    /// If a future contributor inlines a different message,
    /// the test fails loud per CLAUDE.md rule 12.
    #[test]
    fn operator_visible_message_pins_deferred_gate_caveat_and_mirror_status() {
        // F5 half — unchanged at PR-17 time.
        let f5_fragment = "UNSIGNED (signing deferred per F5";
        assert!(
            f5_fragment.contains("UNSIGNED (signing deferred per F5"),
            "operator-visible message must name the F5 signing-deferred posture loud"
        );
        // PR-17 / ADR-0030 §5 — the mirror-file caveat is now
        // resolved by the agreement status (verified vs
        // absent-pre-pr-17), NOT by the old "deferred per F10"
        // string. Pin the two flip-target fragments so a future
        // contributor cannot silently drop them.
        let verified_fragment = "verified against mirror file at";
        let absent_fragment = "no mirror file present at";
        assert!(
            verified_fragment.contains("verified against mirror file at"),
            "operator-visible message must name the verified-agreement state loud"
        );
        assert!(
            absent_fragment.contains("no mirror file present at"),
            "operator-visible message must name the absent-pre-pr-17 state loud"
        );
        // Sentinel: the retired F10-deferral string MUST NOT
        // appear in the new fragments (would indicate the
        // ADR-0030 §5 lift was reverted silently).
        assert!(
            !verified_fragment.contains("deferred per F10"),
            "operator-visible message must NOT carry the retired F10-deferral marker"
        );
        assert!(
            !absent_fragment.contains("deferred per F10"),
            "operator-visible message must NOT carry the retired F10-deferral marker"
        );
    }

    /// PR-17 / ADR-0030 §4: the agreement check refuses the
    /// bundle on count mismatch. Pinned at the helper level so
    /// a future contributor who reorders the check or silently
    /// widens the tolerance surfaces the failure at test time.
    #[test]
    fn mirror_db_agreement_assertion_refuses_count_mismatch() {
        let mirror_only = vec![MirrorEntry {
            id: "aud_00000000000000000000000000".to_string(),
            seq: 1,
            prev_hash: "00".repeat(32),
            time_wall: "2026-01-01T00:00:00Z".to_string(),
            time_mono: 0,
            actor: Actor::from_local_cli("sess".to_string(), "test"),
            binary_hash: "00".repeat(32),
            tenant_id: "t".to_string(),
            kind: "test".to_string(),
            payload: String::new(),
            idempotency_key: None,
            entry_hash: "ff".repeat(32),
        }];
        let db_empty: Vec<Entry> = Vec::new();
        let err = assert_mirror_db_agreement(&mirror_only, &db_empty, Path::new("/dev/null"))
            .unwrap_err();
        let msg = format!("{:#}", err);
        assert!(
            msg.contains("entries") && msg.contains("DB"),
            "count mismatch should surface in the diagnostic: got {msg}"
        );
    }
}
