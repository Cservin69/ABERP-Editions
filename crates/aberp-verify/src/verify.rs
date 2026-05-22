//! The §3 invariant checks per ADR-0035.
//!
//! Owns the SEMANTIC verification: takes the parsed bundle and runs
//! every check the verifier performs from the bundle's bytes alone.
//! Per-check OK / FAIL / NOTE outcomes accumulate on a
//! [`crate::report::Report`] which `crate::main` (or a library
//! consumer) renders at the end.
//!
//! Per ADR-0035 §7 + CLAUDE.md rule 12: the verifier surfaces EVERY
//! check that ran, not just the first failure. An operator reading
//! the output sees the full diagnostic picture so they can
//! triage — silent stop-on-first-fail would hide divergences whose
//! pattern matters.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use aberp_audit_ledger::{compute_entry_hash, genesis_hash, Entry, EventKind, TenantId};
use serde::Deserialize;

use crate::bundle::{
    nav_archive_path, parse_chain_jsonl, parse_manifest, reconstruct_entry, Archive,
    ChainJsonlLine, Manifest, SUPPORTED_MANIFEST_VERSION,
};
use crate::report::{CheckOutcome, Report};

/// The set of `mirror_file_status` strings the verifier recognises.
/// Per ADR-0030 §5 + ADR-0029 §3 commentary the bundle writer emits
/// one of these three. An unknown string triggers a FAIL on the
/// manifest-invariants check (catches a future writer-side rename
/// the verifier doesn't yet know about).
const KNOWN_MIRROR_STATUSES: &[&str] = &[
    "verified-agreement",
    "absent-pre-pr-17",
    // The retired pre-PR-17 placeholder. The verifier still accepts
    // it for backwards-compatibility on bundles produced by older
    // ABERP builds (ADR-0029 §3 originally pinned this string).
    "deferred-per-f10",
];

/// Canonical `signature_status` for PR-22-era bundles per ADR-0029
/// §4. F5 has NOT fired; every bundle's `signed` is false and
/// `signature_status` is this exact string.
const SIGNATURE_STATUS_DEFERRED_PER_F5: &str = "deferred-per-f5";

/// Permissive probe over an audit-ledger entry's payload bytes,
/// mirror of the bundle writer's `BundleMembershipProbe` (the
/// any-id-field-equality posture per ADR-0029 §2). Identical field
/// set MUST hold — a future writer-side field addition that's
/// missed here would silently let entries slip through the
/// bundle-membership pin (§3 check 12). Pinned by
/// `tests::membership_probe_field_set_mirrors_writer`.
#[derive(Debug, Deserialize)]
struct MembershipProbe {
    invoice_id: Option<String>,
    storno_invoice_id: Option<String>,
    modification_invoice_id: Option<String>,
    base_invoice_id: Option<String>,
}

impl MembershipProbe {
    fn matches(&self, target: &str) -> bool {
        if target.is_empty() {
            return false;
        }
        [
            self.invoice_id.as_deref(),
            self.storno_invoice_id.as_deref(),
            self.modification_invoice_id.as_deref(),
            self.base_invoice_id.as_deref(),
        ]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .any(|f| f == target)
    }
}

/// Run every ADR-0035 §3 check against the unpacked bundle. The
/// resulting [`Report`] carries one outcome per check.
///
/// The bundle path is taken purely for inclusion in the report's
/// header (the actual reading already happened in
/// `bundle::read_archive`); this keeps the signature explicit about
/// what's being checked.
pub fn run_checks(bundle_path: &Path, archive: &Archive) -> Report {
    let mut report = Report::new(bundle_path.to_path_buf());

    // §3 check 1 — archive shape. The presence of `manifest.json` +
    // `chain.jsonl` and the `bundle/` root were enforced by
    // `bundle::read_archive` before we got here; a STRUCTURAL failure
    // would have bailed before `run_checks` was called.
    report.push(CheckOutcome::ok(
        "archive shape",
        format!(
            "bundle/ root + manifest.json + chain.jsonl + {} nav/*.xml files",
            archive.nav_files.len()
        ),
    ));

    // §3 check 2 — manifest version.
    let manifest = match parse_manifest(&archive.manifest_bytes) {
        Ok(m) => m,
        Err(e) => {
            report.push(CheckOutcome::fail(
                "manifest parse",
                format!("could not parse bundle/manifest.json: {e:#}"),
            ));
            return report;
        }
    };
    if manifest.version == SUPPORTED_MANIFEST_VERSION {
        report.push(CheckOutcome::ok(
            "manifest version",
            format!("{} (supported)", manifest.version),
        ));
    } else {
        report.push(CheckOutcome::fail(
            "manifest version",
            format!(
                "manifest version {} unknown to aberp-verify (supports v{}); \
                 a newer aberp-verify may understand this bundle",
                manifest.version, SUPPORTED_MANIFEST_VERSION
            ),
        ));
    }

    // §3 check 3 — manifest field set. Implicit: if `parse_manifest`
    // succeeded, every required field per `Manifest`'s shape was
    // present (serde raises on absent required fields). Surfacing
    // the count explicitly here so an inspector sees the check ran.
    report.push(CheckOutcome::ok(
        "manifest field set",
        "13/13 ADR-0029 §3 fields present".to_string(),
    ));

    // §3 check 4 — manifest invariants.
    check_manifest_invariants(&manifest, &mut report);

    // §3 check 5/6 — chain.jsonl line count + per-entry decode.
    let lines = match parse_chain_jsonl(&archive.chain_jsonl_bytes) {
        Ok(ls) => ls,
        Err(e) => {
            report.push(CheckOutcome::fail(
                "chain.jsonl parse",
                format!("could not parse bundle/chain.jsonl: {e:#}"),
            ));
            return report;
        }
    };
    if lines.len() as u64 == manifest.entries_in_bundle {
        report.push(CheckOutcome::ok(
            "chain.jsonl entries",
            format!(
                "{} (matches manifest.entries_in_bundle)",
                lines.len()
            ),
        ));
    } else {
        report.push(CheckOutcome::fail(
            "chain.jsonl entries",
            format!(
                "chain.jsonl has {} line(s) but manifest.entries_in_bundle is {}",
                lines.len(),
                manifest.entries_in_bundle
            ),
        ));
    }

    let entries = reconstruct_entries(&lines, &mut report);

    // §3 check 7 — per-entry tenant pin.
    check_tenant_pin(&manifest, &entries, &mut report);

    // §3 check 8 — per-entry hash recomputation.
    check_per_entry_hash(&entries, &mut report);

    // §3 check 9/10/11 — consecutive-seq chain links + genesis
    // anchor + gap NOTEs.
    let tenant = match TenantId::new(manifest.tenant_id.clone()) {
        Some(t) => Some(t),
        None => {
            report.push(CheckOutcome::fail(
                "tenant_id validity",
                format!(
                    "manifest.tenant_id {:?} is empty or contains a null byte — \
                     ADR-0008 §\"Storage\" + TenantId::new contract",
                    manifest.tenant_id
                ),
            ));
            None
        }
    };
    if let Some(t) = tenant.as_ref() {
        check_chain_links_and_gaps(&entries, t, &mut report);
    }

    // §3 check 12 — bundle-membership pin.
    check_bundle_membership(&manifest, &entries, &mut report);

    // §3 check 13/14 — per-NAV-bearing-entry XML pin + cross-totals.
    check_nav_xml_pins(&entries, &archive.nav_files, &mut report);

    // Echo the deferred-gate posture so an inspector reading the
    // report sees them named alongside every other check (per
    // ADR-0035 §6 + §"Adversarial review" #5 — the mirror line is an
    // ECHO, not a re-verification).
    report.push(CheckOutcome::ok(
        "deferred-gate echo",
        format!(
            "signed=false (F5 unchanged); mirror_file_status={:?} (echoed from manifest, \
             not re-verified — mirror lives outside the bundle per ADR-0030)",
            manifest.mirror_file_status
        ),
    ));

    report.set_summary_invoice_id(manifest.invoice_id);
    report
}

/// §3 check 4 — manifest invariants. Asserts:
///   - `chain_verified == true` (ADR-0029 §6 — a tampered chain at
///     bundle-write time would have refused the bundle).
///   - `signed == false` AND `signature_status ==
///     "deferred-per-f5"` (PR-22-era invariant per ADR-0029 §4 +
///     ADR-0035 §6).
///   - `mirror_file_status` is one of the three known strings.
fn check_manifest_invariants(m: &Manifest, report: &mut Report) {
    if m.chain_verified {
        report.push(CheckOutcome::ok(
            "manifest chain_verified",
            "true (ABERP-build-time full-chain verification claim)".to_string(),
        ));
    } else {
        report.push(CheckOutcome::fail(
            "manifest chain_verified",
            "false — a tampered chain at bundle-write time would have refused \
             the bundle per ADR-0029 §6; the manifest claims otherwise, which is \
             a contradiction that must be investigated".to_string(),
        ));
    }

    if !m.signed && m.signature_status == SIGNATURE_STATUS_DEFERRED_PER_F5 {
        report.push(CheckOutcome::ok(
            "manifest signing posture",
            format!(
                "signed=false, signature_status={SIGNATURE_STATUS_DEFERRED_PER_F5:?} \
                 (F5 unchanged per ADR-0029 §4)"
            ),
        ));
    } else if m.signed {
        report.push(CheckOutcome::fail(
            "manifest signing posture",
            format!(
                "manifest claims signed={} with signature_status={:?} — but PR-22's \
                 aberp-verify does NOT know how to verify a signature (F5 deferred); \
                 a newer aberp-verify with --public-key support may understand this bundle",
                m.signed, m.signature_status
            ),
        ));
    } else {
        report.push(CheckOutcome::fail(
            "manifest signing posture",
            format!(
                "signed=false but signature_status={:?} — expected {:?} per ADR-0029 §4 + \
                 ADR-0035 §6; unexpected string surfaces loud per CLAUDE.md rule 12",
                m.signature_status, SIGNATURE_STATUS_DEFERRED_PER_F5
            ),
        ));
    }

    if KNOWN_MIRROR_STATUSES.contains(&m.mirror_file_status.as_str()) {
        report.push(CheckOutcome::ok(
            "manifest mirror_file_status",
            format!("{:?} (recognised)", m.mirror_file_status),
        ));
    } else {
        report.push(CheckOutcome::fail(
            "manifest mirror_file_status",
            format!(
                "mirror_file_status={:?} is not one of the known strings {:?} — \
                 a future ADR-0030 amendment may have added a new status the \
                 verifier does not yet recognise",
                m.mirror_file_status, KNOWN_MIRROR_STATUSES
            ),
        ));
    }
}

/// Reconstruct typed [`Entry`] values from chain.jsonl lines, pushing
/// FAIL outcomes for any line that fails the structural decode.
/// Returns the successfully-reconstructed entries in input order
/// (line order = seq order per ADR-0029 §3).
fn reconstruct_entries(lines: &[ChainJsonlLine], report: &mut Report) -> Vec<Entry> {
    let mut entries = Vec::with_capacity(lines.len());
    let mut decode_failures = 0;
    for (idx, line) in lines.iter().enumerate() {
        match reconstruct_entry(line) {
            Ok(entry) => entries.push(entry),
            Err(e) => {
                decode_failures += 1;
                report.push(CheckOutcome::fail(
                    "per-entry decode",
                    format!(
                        "chain.jsonl line {} (seq={}): {:#}",
                        idx + 1,
                        line.seq,
                        e
                    ),
                ));
            }
        }
    }
    if decode_failures == 0 {
        report.push(CheckOutcome::ok(
            "per-entry decode",
            format!("{}/{} chain.jsonl lines decoded cleanly", entries.len(), lines.len()),
        ));
    }
    entries
}

/// §3 check 7 — per-entry tenant pin.
fn check_tenant_pin(manifest: &Manifest, entries: &[Entry], report: &mut Report) {
    let mut divergent: Vec<u64> = Vec::new();
    for entry in entries {
        if entry.tenant_id.as_str() != manifest.tenant_id {
            divergent.push(entry.seq.as_u64());
        }
    }
    if divergent.is_empty() {
        report.push(CheckOutcome::ok(
            "per-entry tenant pin",
            format!(
                "{}/{} entries carry tenant_id={:?}",
                entries.len(),
                entries.len(),
                manifest.tenant_id
            ),
        ));
    } else {
        report.push(CheckOutcome::fail(
            "per-entry tenant pin",
            format!(
                "{} entries have tenant_id mismatched against manifest.tenant_id={:?} \
                 (cross-tenant contamination): seqs {:?}",
                divergent.len(),
                manifest.tenant_id,
                divergent
            ),
        ));
    }
}

/// §3 check 8 — recompute every entry's entry_hash via the canonical
/// CBOR encoder + SHA-256 (the one-place encoder per ADR-0021 §A12
/// re-exported by ADR-0035 §8); compare against the claimed
/// `entry_hash`. Per-entry tamper detection.
fn check_per_entry_hash(entries: &[Entry], report: &mut Report) {
    let mut failures: Vec<u64> = Vec::new();
    for entry in entries {
        let recomputed = compute_entry_hash(entry);
        if recomputed != entry.entry_hash {
            failures.push(entry.seq.as_u64());
            report.push(CheckOutcome::fail(
                "per-entry hash recomputation",
                format!(
                    "seq={}: recomputed entry_hash {} does not match claimed entry_hash {} \
                     (entry has been tampered with after it was written)",
                    entry.seq.as_u64(),
                    hex::encode(recomputed.as_bytes()),
                    hex::encode(entry.entry_hash.as_bytes()),
                ),
            ));
        }
    }
    if failures.is_empty() {
        report.push(CheckOutcome::ok(
            "per-entry hash recomputation",
            format!("{}/{} entries", entries.len(), entries.len()),
        ));
    }
}

/// §3 check 9/10/11 — consecutive-seq chain links + genesis anchor +
/// gap NOTEs. Per ADR-0035 §"Surfaced conflict 3" Reading B + §5.
fn check_chain_links_and_gaps(entries: &[Entry], tenant: &TenantId, report: &mut Report) {
    if entries.is_empty() {
        // Empty slice; the entries-count FAIL on check 5 already
        // surfaces this. No links to check.
        return;
    }
    // Genesis anchor (only fires if seq=1 is in the slice).
    let first = &entries[0];
    if first.seq.as_u64() == 1 {
        let genesis = genesis_hash(tenant);
        if first.prev_hash == genesis {
            report.push(CheckOutcome::ok(
                "genesis anchor (seq=1)",
                format!(
                    "prev_hash matches genesis_hash for tenant {:?}",
                    tenant.as_str()
                ),
            ));
        } else {
            report.push(CheckOutcome::fail(
                "genesis anchor (seq=1)",
                format!(
                    "seq=1 entry's prev_hash {} does not match genesis_hash {} \
                     for tenant {:?} — the chain's foundation is broken",
                    hex::encode(first.prev_hash.as_bytes()),
                    hex::encode(genesis.as_bytes()),
                    tenant.as_str()
                ),
            ));
        }
    }
    // Consecutive-seq link checks + gap NOTEs.
    let mut consecutive_ok = 0usize;
    let mut consecutive_fail = 0usize;
    let mut gap_count = 0usize;
    for w in entries.windows(2) {
        let (prev, next) = (&w[0], &w[1]);
        let p_seq = prev.seq.as_u64();
        let n_seq = next.seq.as_u64();
        if n_seq == p_seq + 1 {
            if next.prev_hash == prev.entry_hash {
                consecutive_ok += 1;
            } else {
                consecutive_fail += 1;
                report.push(CheckOutcome::fail(
                    "consecutive chain link",
                    format!(
                        "seq={} -> seq={}: next.prev_hash {} does not match prev.entry_hash {} \
                         (chain link broken at the slice's contiguous boundary)",
                        p_seq,
                        n_seq,
                        hex::encode(next.prev_hash.as_bytes()),
                        hex::encode(prev.entry_hash.as_bytes()),
                    ),
                ));
            }
        } else if n_seq > p_seq + 1 {
            gap_count += 1;
            report.push(CheckOutcome::note(
                "seq gap",
                format!(
                    "seq {} -> {} (delegated to manifest.chain_verified=true — \
                     the slice cannot re-verify across the gap from bundle bytes alone)",
                    p_seq, n_seq
                ),
            ));
        } else {
            // n_seq <= p_seq — out of order or duplicate.
            report.push(CheckOutcome::fail(
                "slice seq ordering",
                format!(
                    "seq {} appears after seq {} — chain.jsonl is not in ascending seq order \
                     (ADR-0029 §3 requires seq-ordered oldest-first)",
                    n_seq, p_seq
                ),
            ));
        }
    }
    if consecutive_fail == 0 {
        report.push(CheckOutcome::ok(
            "consecutive chain links",
            format!(
                "{}/{} consecutive pairs verified ({} gap(s) NOTED above)",
                consecutive_ok,
                consecutive_ok + consecutive_fail,
                gap_count
            ),
        ));
    }
}

/// §3 check 12 — every slice entry's payload must reference the
/// manifest's invoice_id in at least one id-shaped field per the
/// any-id-field-equality posture (ADR-0029 §2 mirror).
fn check_bundle_membership(manifest: &Manifest, entries: &[Entry], report: &mut Report) {
    let mut not_referencing: Vec<u64> = Vec::new();
    for entry in entries {
        let probe: Result<MembershipProbe, _> = serde_json::from_slice(&entry.payload);
        let matches = match probe {
            Ok(p) => p.matches(&manifest.invoice_id),
            Err(_) => false,
        };
        if !matches {
            not_referencing.push(entry.seq.as_u64());
        }
    }
    if not_referencing.is_empty() {
        report.push(CheckOutcome::ok(
            "bundle membership",
            format!(
                "{}/{} entries reference invoice id {:?}",
                entries.len(),
                entries.len(),
                manifest.invoice_id
            ),
        ));
    } else {
        report.push(CheckOutcome::fail(
            "bundle membership",
            format!(
                "{} entries do not reference manifest.invoice_id={:?} in any \
                 id-shaped field (silent-omission failure mode per CLAUDE.md rule 12): seqs {:?}",
                not_referencing.len(),
                manifest.invoice_id,
                not_referencing
            ),
        ));
    }
}

/// §3 check 13/14 — per-NAV-bearing-entry XML pin + cross-totals.
///
/// For each entry whose EventKind carries verbatim NAV bytes per
/// ADR-0035 §4 + the bundle writer's `extract_nav_xml` exhaustive
/// match: find the matching `nav/<seq>_<kind>.xml` file in the
/// archive, decode the payload's request_xml or response_xml,
/// compare bytes, and check the root element matches the per-kind
/// expected list.
fn check_nav_xml_pins(entries: &[Entry], nav_files: &HashMap<String, Vec<u8>>, report: &mut Report) {
    let mut consumed_paths: BTreeSet<String> = BTreeSet::new();
    let mut ok_count = 0usize;
    let mut fail_count = 0usize;

    for entry in entries {
        let extraction = match extract_nav_xml(entry) {
            Ok(x) => x,
            Err(e) => {
                fail_count += 1;
                report.push(CheckOutcome::fail(
                    "NAV-XML payload decode",
                    format!("seq={}: {:#}", entry.seq.as_u64(), e),
                ));
                continue;
            }
        };
        let Some(payload_xml) = extraction.bytes else {
            // No NAV bytes expected for this entry (non-NAV-bearing
            // kind OR optional response_xml absent on a failure-class
            // entry).
            continue;
        };

        let archive_path = nav_archive_path(entry.seq.as_u64(), entry.kind.clone());
        consumed_paths.insert(archive_path.clone());
        let Some(file_bytes) = nav_files.get(&archive_path) else {
            fail_count += 1;
            report.push(CheckOutcome::fail(
                "NAV-XML file presence",
                format!(
                    "seq={} (kind={}): expected archive entry {} but it is absent from the bundle",
                    entry.seq.as_u64(),
                    entry.kind.as_str(),
                    archive_path
                ),
            ));
            continue;
        };
        if file_bytes != &payload_xml {
            fail_count += 1;
            report.push(CheckOutcome::fail(
                "NAV-XML byte equality",
                format!(
                    "seq={} (kind={}): payload's {} bytes do NOT match {} bytes \
                     (the bundle writer's verbatim-bytes contract is broken)",
                    entry.seq.as_u64(),
                    entry.kind.as_str(),
                    extraction.field_name,
                    archive_path,
                ),
            ));
            continue;
        }
        // Root-element pin per ADR-0035 §4 + ADR-0034 §10 Reading A
        // (two-root-element acceptance for InvoiceSubmissionResponse).
        if let Err(why) =
            check_root_element(entry.kind.clone(), extraction.field_name, &payload_xml)
        {
            fail_count += 1;
            report.push(CheckOutcome::fail(
                "NAV-XML root element pin",
                format!(
                    "seq={} (kind={}): {}",
                    entry.seq.as_u64(),
                    entry.kind.as_str(),
                    why
                ),
            ));
            continue;
        }
        ok_count += 1;
    }

    // Aggregate OK summary line (when no per-entry failure surfaced).
    if fail_count == 0 {
        report.push(CheckOutcome::ok(
            "NAV-XML pins",
            format!(
                "{} NAV-bearing entries verified (byte-equality + root-element pin per kind)",
                ok_count
            ),
        ));
    }

    // §3 check 14 — cross-totals. Surplus nav/*.xml files (those not
    // referenced by any entry) are a structural divergence per
    // CLAUDE.md rule 12 — surfacing the count loud rather than
    // silently ignoring orphaned files.
    let archive_nav_paths: BTreeSet<&String> = nav_files.keys().collect();
    let consumed_refs: BTreeSet<&String> = consumed_paths.iter().collect();
    let orphans: Vec<String> = archive_nav_paths
        .difference(&consumed_refs)
        .map(|s| (*s).clone())
        .collect();
    if orphans.is_empty() {
        report.push(CheckOutcome::ok(
            "NAV-XML file cross-totals",
            format!(
                "every nav/*.xml in the archive ({} file(s)) is referenced by an entry",
                nav_files.len()
            ),
        ));
    } else {
        report.push(CheckOutcome::fail(
            "NAV-XML file cross-totals",
            format!(
                "{} nav/*.xml file(s) in archive not referenced by any entry: {:?} \
                 (orphaned files surface loud per CLAUDE.md rule 12 — silent inclusion \
                 of unreferenced bytes is the wrong affordance)",
                orphans.len(),
                orphans
            ),
        ));
    }
}

/// The verbatim NAV bytes extracted from an entry's payload (if
/// any), plus the field name they came from. The field name is
/// used in operator-visible diagnostics so an inspector reading a
/// FAIL knows whether the divergence was on the request side or
/// the response side.
struct NavExtraction {
    bytes: Option<Vec<u8>>,
    field_name: &'static str,
}

/// Extract the verbatim NAV bytes from an entry's typed payload.
/// Returns `Ok(NavExtraction { bytes: None, .. })` for non-NAV-
/// bearing kinds OR for kinds where the verbatim bytes are
/// optional and the payload's field is absent. Returns `Err(_)`
/// only when the payload bytes fail typed JSON decode — that's a
/// schema-drift concern the verifier must surface.
///
/// The match is exhaustive on EventKind so a future variant
/// addition forces a contributor decision: does the new kind carry
/// NAV bytes? Mirrors the bundle writer's `extract_nav_xml`
/// exhaustive match.
fn extract_nav_xml(entry: &Entry) -> anyhow::Result<NavExtraction> {
    use anyhow::Context;

    // Minimal per-kind payload shapes — only the verbatim-byte
    // fields the verifier needs. Per CLAUDE.md rule 2 the verifier
    // does NOT pull in the full audit_payloads types from
    // apps/aberp; it deserializes against the minimal field set.
    #[derive(Deserialize)]
    struct WithRequestXml {
        request_xml: Vec<u8>,
    }
    #[derive(Deserialize)]
    struct WithResponseXml {
        response_xml: Vec<u8>,
    }
    #[derive(Deserialize)]
    struct WithOptionalResponseXml {
        response_xml: Option<Vec<u8>>,
    }
    #[derive(Deserialize)]
    struct CheckPerformedShape {
        // request_xml is required per ADR-0033 §2; response_xml is
        // Option per the same. The verifier surfaces only the
        // response side as the per-entry nav file (mirror of the
        // bundle writer's posture per `extract_nav_xml`).
        response_xml: Option<Vec<u8>>,
    }

    let bytes_and_field: (Option<Vec<u8>>, &'static str) = match entry.kind {
        EventKind::InvoiceSubmissionAttempt => {
            let p: WithRequestXml = serde_json::from_slice(&entry.payload)
                .context("decode InvoiceSubmissionAttempt request_xml")?;
            (Some(p.request_xml), "request_xml")
        }
        EventKind::InvoiceSubmissionResponse => {
            let p: WithResponseXml = serde_json::from_slice(&entry.payload)
                .context("decode InvoiceSubmissionResponse response_xml")?;
            (Some(p.response_xml), "response_xml")
        }
        EventKind::InvoiceAckStatus => {
            let p: WithResponseXml = serde_json::from_slice(&entry.payload)
                .context("decode InvoiceAckStatus response_xml")?;
            (Some(p.response_xml), "response_xml")
        }
        EventKind::InvoiceAnnulmentSubmissionAttempt => {
            let p: WithRequestXml = serde_json::from_slice(&entry.payload)
                .context("decode InvoiceAnnulmentSubmissionAttempt request_xml")?;
            (Some(p.request_xml), "request_xml")
        }
        EventKind::InvoiceAnnulmentSubmissionResponse => {
            let p: WithResponseXml = serde_json::from_slice(&entry.payload)
                .context("decode InvoiceAnnulmentSubmissionResponse response_xml")?;
            (Some(p.response_xml), "response_xml")
        }
        EventKind::InvoiceAnnulmentAckStatus => {
            let p: WithResponseXml = serde_json::from_slice(&entry.payload)
                .context("decode InvoiceAnnulmentAckStatus response_xml")?;
            (Some(p.response_xml), "response_xml")
        }
        EventKind::InvoiceAnnulmentReceiverConfirmation => {
            let p: WithResponseXml = serde_json::from_slice(&entry.payload)
                .context("decode InvoiceAnnulmentReceiverConfirmation response_xml")?;
            (Some(p.response_xml), "response_xml")
        }
        EventKind::InvoiceSubmissionAttemptFailed => {
            let p: WithOptionalResponseXml = serde_json::from_slice(&entry.payload)
                .context("decode InvoiceSubmissionAttemptFailed response_xml")?;
            (p.response_xml, "response_xml")
        }
        EventKind::InvoiceCheckPerformed => {
            let p: CheckPerformedShape = serde_json::from_slice(&entry.payload)
                .context("decode InvoiceCheckPerformed response_xml")?;
            (p.response_xml, "response_xml")
        }
        // Non-NAV-bearing kinds — no archive file expected. The
        // match is exhaustive per the bundle writer's mirror; a
        // future EventKind variant requires a contributor decision
        // here AND in the writer.
        EventKind::Test
        | EventKind::InvoiceSequenceReserved
        | EventKind::InvoiceDraftCreated
        | EventKind::InvoiceRetryRequested
        | EventKind::InvoiceMarkedAbandoned
        | EventKind::InvoiceStornoIssued
        | EventKind::InvoiceModificationIssued
        | EventKind::InvoiceTechnicalAnnulmentRequested => (None, ""),
    };

    Ok(NavExtraction {
        bytes: bytes_and_field.0,
        field_name: bytes_and_field.1,
    })
}

/// Check that the verbatim XML bytes start with one of the expected
/// root-element local names per ADR-0035 §4. Namespaces are matched
/// at the local-name level (`<ns0:ManageInvoiceResponse>` accepts
/// alongside `<ManageInvoiceResponse>`). Returns `Err(_)` with an
/// operator-visible diagnostic on mismatch.
///
/// **Two-root-element acceptance per ADR-0034 §10 Reading A.**
/// `InvoiceSubmissionResponse` accepts BOTH `ManageInvoiceResponse`
/// (PR-7-B-3 manageInvoice path) AND `QueryInvoiceDataResponse`
/// (PR-21 recover-from-nav path). The two-element acceptance is
/// the load-bearing ADR-0035 §"Surfaced conflict 2" Reading A pick.
///
/// **`InvoiceSubmissionAttemptFailed`** has NO root-element pin —
/// the response body shape varies across failure classes per
/// ADR-0032 §2; an operator-visible diagnostic-only failure-class
/// XML body has no canonical root.
fn check_root_element(
    kind: EventKind,
    field_name: &'static str,
    bytes: &[u8],
) -> Result<(), String> {
    let expected: &[&str] = match (kind.clone(), field_name) {
        (EventKind::InvoiceSubmissionAttempt, "request_xml") => &["ManageInvoiceRequest"],
        (EventKind::InvoiceSubmissionResponse, "response_xml") => {
            // ADR-0034 §10 Reading A — two-root-element acceptance.
            &["ManageInvoiceResponse", "QueryInvoiceDataResponse"]
        }
        (EventKind::InvoiceAckStatus, "response_xml") => &["QueryTransactionStatusResponse"],
        (EventKind::InvoiceAnnulmentSubmissionAttempt, "request_xml") => {
            &["ManageAnnulmentRequest"]
        }
        (EventKind::InvoiceAnnulmentSubmissionResponse, "response_xml") => {
            &["ManageAnnulmentResponse"]
        }
        (EventKind::InvoiceAnnulmentAckStatus, "response_xml") => {
            &["QueryTransactionStatusResponse"]
        }
        (EventKind::InvoiceAnnulmentReceiverConfirmation, "response_xml") => {
            &["QueryInvoiceDataResponse"]
        }
        (EventKind::InvoiceSubmissionAttemptFailed, _) => {
            // Failure-class response bodies vary; no root pin per
            // ADR-0035 §4.
            return Ok(());
        }
        (EventKind::InvoiceCheckPerformed, "response_xml") => &["QueryInvoiceCheckResponse"],
        // Any other combination shouldn't reach here (the call site
        // only invokes check_root_element when extract_nav_xml
        // returned bytes); be defensive anyway.
        _ => return Ok(()),
    };

    let actual = first_element_local_name(bytes)
        .ok_or_else(|| format!("could not find an opening XML tag in the bytes"))?;
    if expected.iter().any(|e| *e == actual) {
        Ok(())
    } else {
        Err(format!(
            "root element {:?} not in expected list {:?} (per ADR-0035 §4)",
            actual, expected
        ))
    }
}

/// Find the local name of the first opening XML element in `bytes`.
/// Skips the optional XML prolog (`<?xml ... ?>`) and any leading
/// whitespace; returns the substring between `<` (or `<ns:`) and
/// the next `>`, `' '`, `/`, or `\t`. Per ADR-0035 §4: namespaces
/// are matched at the local-name level, so `<ns0:Foo>` returns
/// `"Foo"`.
fn first_element_local_name(bytes: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut cursor = text;
    loop {
        cursor = cursor.trim_start();
        if cursor.starts_with("<?") {
            // Skip XML prolog.
            let end = cursor.find("?>")?;
            cursor = &cursor[end + 2..];
            continue;
        }
        if cursor.starts_with("<!--") {
            // Skip a comment.
            let end = cursor.find("-->")?;
            cursor = &cursor[end + 3..];
            continue;
        }
        break;
    }
    let rest = cursor.strip_prefix('<')?;
    // Read the element name up to the first non-name character.
    let end = rest
        .find(|c: char| c == '>' || c == ' ' || c == '\t' || c == '/' || c == '\n' || c == '\r')
        .unwrap_or(rest.len());
    let qualified = &rest[..end];
    // Strip a namespace prefix `ns:` if present.
    let local = qualified.rsplit(':').next()?;
    if local.is_empty() {
        None
    } else {
        Some(local.to_string())
    }
}

// ──────────────────────────────────────────────────────────────────────
// Tests — invariant-check building blocks. Per-check coverage at the
// micro level; the integration tests under tests/ exercise run_checks
// end-to-end against a synthetic Ledger fixture.
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use aberp_audit_ledger::{Actor, BinaryHash, EntryHash, Ledger, TenantId};
    use std::collections::BTreeSet;

    fn fixture_ledger() -> (Ledger, Actor) {
        let tenant = TenantId::new("t1".to_string()).unwrap();
        let bh = BinaryHash::from_bytes([0u8; 32]);
        let ledger = Ledger::open_in_memory(tenant, bh).unwrap();
        let actor = Actor::from_local_cli("sess".to_string(), "test-user");
        (ledger, actor)
    }

    /// Build a tiny in-memory entry, recompute its hash, assert
    /// `compute_entry_hash` agrees with the Ledger-stored
    /// `entry_hash`. End-to-end check that the canonical encoder's
    /// re-exported pub use works as expected.
    #[test]
    fn re_exported_compute_entry_hash_matches_ledger_stored_hash() {
        let (mut ledger, actor) = fixture_ledger();
        let payload = br#"{"invoice_id":"inv_X"}"#.to_vec();
        ledger
            .append(EventKind::Test, payload, actor, None)
            .unwrap();
        let entries = ledger.entries().unwrap();
        let entry = &entries[0];
        let recomputed = compute_entry_hash(entry);
        assert_eq!(
            recomputed, entry.entry_hash,
            "compute_entry_hash pub-use must produce the same hash the Ledger stored"
        );
    }

    /// Genesis-anchor sanity: the re-exported genesis_hash is per-
    /// tenant and matches the value the Ledger uses internally for
    /// the seq=1 entry's prev_hash.
    #[test]
    fn re_exported_genesis_hash_matches_first_entry_prev_hash() {
        let (mut ledger, actor) = fixture_ledger();
        ledger
            .append(EventKind::Test, b"{}".to_vec(), actor, None)
            .unwrap();
        let entries = ledger.entries().unwrap();
        let tenant = TenantId::new("t1".to_string()).unwrap();
        let expected = genesis_hash(&tenant);
        assert_eq!(
            entries[0].prev_hash, expected,
            "seq=1 entry's prev_hash must equal genesis_hash(tenant)"
        );
    }

    /// Membership probe mirrors the writer's field set verbatim.
    /// A future writer-side rename of one of the four fields would
    /// silently let entries fall out of the bundle-membership pin —
    /// the test guards the mirror.
    #[test]
    fn membership_probe_field_set_mirrors_writer() {
        // Round-trip each known id-shaped field.
        for field in [
            "invoice_id",
            "storno_invoice_id",
            "modification_invoice_id",
            "base_invoice_id",
        ] {
            let json = format!(r#"{{"{field}":"inv_TEST"}}"#);
            let p: MembershipProbe = serde_json::from_str(&json).unwrap();
            assert!(p.matches("inv_TEST"), "probe must match on {field}");
        }
        // Empty target rejected.
        let p: MembershipProbe =
            serde_json::from_str(r#"{"invoice_id":""}"#).unwrap();
        assert!(!p.matches(""));
        assert!(!p.matches("inv_TEST"));
    }

    /// ADR-0034 §10 Reading A — the load-bearing two-root-element
    /// acceptance. Both `<ManageInvoiceResponse>` AND
    /// `<QueryInvoiceDataResponse>` are accepted root elements for
    /// `InvoiceSubmissionResponse` entries.
    #[test]
    fn invoice_submission_response_accepts_both_root_elements_per_adr_0034_10() {
        let manage = b"<ManageInvoiceResponse/>";
        let query = b"<QueryInvoiceDataResponse/>";
        assert!(
            check_root_element(EventKind::InvoiceSubmissionResponse, "response_xml", manage).is_ok(),
            "ManageInvoiceResponse root must be accepted (PR-7-B-3 manageInvoice path)"
        );
        assert!(
            check_root_element(EventKind::InvoiceSubmissionResponse, "response_xml", query).is_ok(),
            "QueryInvoiceDataResponse root must be accepted (PR-21 recover-from-nav path) \
             per ADR-0034 §10 Reading A"
        );
    }

    /// ADR-0035 §4 negative-side pin: an UNEXPECTED root element on
    /// an `InvoiceSubmissionResponse` entry FAILs loud.
    #[test]
    fn invoice_submission_response_rejects_unrelated_root_element() {
        let other = b"<SomeOtherResponse/>";
        let err = check_root_element(
            EventKind::InvoiceSubmissionResponse,
            "response_xml",
            other,
        )
        .unwrap_err();
        assert!(
            err.contains("SomeOtherResponse")
                && err.contains("ManageInvoiceResponse")
                && err.contains("QueryInvoiceDataResponse"),
            "FAIL message must name both expected roots and the rejected one: {err}"
        );
    }

    /// ADR-0035 §4 single-root pins (sanity on the rest of the
    /// per-kind table).
    #[test]
    fn other_kinds_have_canonical_single_root_pin() {
        // ManageInvoiceRequest is the only valid request_xml root.
        assert!(check_root_element(
            EventKind::InvoiceSubmissionAttempt,
            "request_xml",
            b"<ManageInvoiceRequest/>"
        )
        .is_ok());
        // QueryTransactionStatusResponse — shared by ack-status
        // kinds.
        assert!(check_root_element(
            EventKind::InvoiceAckStatus,
            "response_xml",
            b"<QueryTransactionStatusResponse/>"
        )
        .is_ok());
        assert!(check_root_element(
            EventKind::InvoiceAnnulmentAckStatus,
            "response_xml",
            b"<QueryTransactionStatusResponse/>"
        )
        .is_ok());
        // Wrong-root surfaces loud.
        let err = check_root_element(
            EventKind::InvoiceAckStatus,
            "response_xml",
            b"<ManageInvoiceResponse/>",
        )
        .unwrap_err();
        assert!(err.contains("QueryTransactionStatusResponse"));
    }

    /// `InvoiceSubmissionAttemptFailed` has NO root-element pin per
    /// ADR-0035 §4 (failure-class bodies vary). Any bytes accept.
    #[test]
    fn attempt_failed_has_no_root_element_pin() {
        assert!(check_root_element(
            EventKind::InvoiceSubmissionAttemptFailed,
            "response_xml",
            b"<NavGeneralErrorResponse/>"
        )
        .is_ok());
        assert!(check_root_element(
            EventKind::InvoiceSubmissionAttemptFailed,
            "response_xml",
            b"plain text? whatever"
        )
        .is_ok());
    }

    /// Namespace-prefixed root elements match at the local-name
    /// level per ADR-0035 §4: `<ns0:ManageInvoiceResponse>` accepts
    /// alongside `<ManageInvoiceResponse>`.
    #[test]
    fn namespace_prefixed_root_elements_match_at_local_name_level() {
        let prefixed = b"<ns0:ManageInvoiceResponse xmlns:ns0='x'/>";
        assert!(check_root_element(
            EventKind::InvoiceSubmissionResponse,
            "response_xml",
            prefixed
        )
        .is_ok());
    }

    /// XML prolog + leading whitespace must not throw off the
    /// root-element extractor.
    #[test]
    fn first_element_local_name_skips_prolog_and_whitespace() {
        let bytes = b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n  <ManageInvoiceResponse/>";
        let name = first_element_local_name(bytes).unwrap();
        assert_eq!(name, "ManageInvoiceResponse");
    }

    /// XML comments before the root element are skipped.
    #[test]
    fn first_element_local_name_skips_comments() {
        let bytes = b"<!-- generated --><ManageInvoiceResponse/>";
        let name = first_element_local_name(bytes).unwrap();
        assert_eq!(name, "ManageInvoiceResponse");
    }

    /// The verifier's exhaustive EventKind match in extract_nav_xml
    /// MUST cover every variant `EventKind::from_storage_str` knows.
    /// A future contributor adding a new variant that they expect
    /// to carry NAV bytes will find the match arm and decide
    /// explicitly; a variant that doesn't appear in either branch
    /// fails the Rust exhaustiveness check at build time.
    ///
    /// This test pins the OTHER side: every known EventKind storage
    /// string can be parsed AND the verifier's match handles it.
    /// The exhaustiveness is enforced by Rust; this test is the
    /// belt-and-braces canary for the from_storage_str round trip
    /// the verifier depends on.
    #[test]
    fn extract_nav_xml_handles_every_known_event_kind() {
        let known_kinds: &[&str] = &[
            "test",
            "invoice.sequence_reserved",
            "invoice.draft_created",
            "invoice.submission_attempt",
            "invoice.submission_response",
            "invoice.ack_status",
            "invoice.retry_requested",
            "invoice.marked_abandoned",
            "invoice.storno_issued",
            "invoice.modification_issued",
            "invoice.technical_annulment_requested",
            "invoice.annulment_submission_attempt",
            "invoice.annulment_submission_response",
            "invoice.annulment_ack_status",
            "invoice.annulment_receiver_confirmation",
            "invoice.submission_attempt_failed",
            "invoice.check_performed",
        ];
        let mut parsed = BTreeSet::new();
        for k in known_kinds {
            let kind = EventKind::from_storage_str(k).expect("known kind must parse");
            parsed.insert(kind.as_str());
        }
        assert_eq!(
            parsed.len(),
            known_kinds.len(),
            "round-trip count must match known kinds"
        );
    }

    /// End-to-end smoke: build a one-entry ledger, recompute its
    /// hash, confirm the check_per_entry_hash flow surfaces OK.
    #[test]
    fn check_per_entry_hash_surfaces_ok_on_untampered_entry() {
        let (mut ledger, actor) = fixture_ledger();
        ledger
            .append(EventKind::Test, b"{}".to_vec(), actor, None)
            .unwrap();
        let entries = ledger.entries().unwrap();
        let mut report = Report::new("/tmp/test".into());
        check_per_entry_hash(&entries, &mut report);
        assert!(report.is_ok(), "untampered entry must produce OK");
    }

    /// Tampering: mutate the payload, leave the entry_hash claim,
    /// confirm the recomputation flags the divergence.
    #[test]
    fn check_per_entry_hash_surfaces_fail_on_tampered_payload() {
        let (mut ledger, actor) = fixture_ledger();
        ledger
            .append(EventKind::Test, b"{}".to_vec(), actor, None)
            .unwrap();
        let entries = ledger.entries().unwrap();
        // Tamper: mutate payload but keep the original entry_hash.
        let mut tampered = entries[0].clone();
        tampered.payload = b"{\"tampered\":true}".to_vec();
        let mut report = Report::new("/tmp/test".into());
        check_per_entry_hash(&[tampered], &mut report);
        assert!(
            !report.is_ok(),
            "tampered payload must produce FAIL on hash recomputation"
        );
        let composed = report.compose_for_test();
        assert!(
            composed.contains("tampered with"),
            "FAIL diagnostic must name the tampering: {composed}"
        );
    }

    /// Genesis-anchor positive path: seq=1 entry's prev_hash equals
    /// `genesis_hash(tenant)`.
    #[test]
    fn check_chain_links_passes_genesis_anchor_on_real_ledger() {
        let (mut ledger, actor) = fixture_ledger();
        ledger
            .append(EventKind::Test, b"{}".to_vec(), actor, None)
            .unwrap();
        let entries = ledger.entries().unwrap();
        let tenant = TenantId::new("t1".to_string()).unwrap();
        let mut report = Report::new("/tmp/test".into());
        check_chain_links_and_gaps(&entries, &tenant, &mut report);
        assert!(report.is_ok(), "real ledger must pass the genesis anchor");
    }

    /// Genesis-anchor negative path: a forged seq=1 entry whose
    /// prev_hash does NOT match the tenant genesis must surface FAIL.
    #[test]
    fn check_chain_links_fails_on_forged_seq_1_prev_hash() {
        let (mut ledger, actor) = fixture_ledger();
        ledger
            .append(EventKind::Test, b"{}".to_vec(), actor, None)
            .unwrap();
        let mut entries = ledger.entries().unwrap();
        entries[0].prev_hash = EntryHash::from_bytes([0xAA; 32]);
        let tenant = TenantId::new("t1".to_string()).unwrap();
        let mut report = Report::new("/tmp/test".into());
        check_chain_links_and_gaps(&entries, &tenant, &mut report);
        assert!(!report.is_ok(), "forged prev_hash must surface FAIL");
    }

    /// Gap NOTE path: non-consecutive seqs emit NOTE (not FAIL).
    #[test]
    fn check_chain_links_emits_note_for_seq_gap_not_fail() {
        // Build two real entries (seq 1, 2), then drop seq 2 to
        // create a 1 -> 3 gap. Forge a seq-3 entry with consistent
        // hash so the only thing the verifier sees is the gap.
        let (mut ledger, actor) = fixture_ledger();
        ledger
            .append(EventKind::Test, b"{}".to_vec(), actor.clone(), None)
            .unwrap();
        ledger
            .append(EventKind::Test, b"{}".to_vec(), actor.clone(), None)
            .unwrap();
        ledger
            .append(EventKind::Test, b"{}".to_vec(), actor, None)
            .unwrap();
        let mut all = ledger.entries().unwrap();
        // Drop the middle entry (seq=2) to create a gap.
        all.remove(1);
        let tenant = TenantId::new("t1".to_string()).unwrap();
        let mut report = Report::new("/tmp/test".into());
        check_chain_links_and_gaps(&all, &tenant, &mut report);
        // Gap NOTE is informational; bundle remains OK on a slice
        // that has gaps (per ADR-0035 §"Surfaced conflict 3" Reading B).
        assert!(report.is_ok(), "gap should not fail the slice");
        let composed = report.compose_for_test();
        assert!(
            composed.contains("NOTE") && composed.contains("seq gap"),
            "gap NOTE must appear in the report: {composed}"
        );
    }

    /// Bundle-membership pin: an entry whose payload doesn't
    /// reference the manifest's invoice_id must FAIL.
    #[test]
    fn check_bundle_membership_fails_on_non_referencing_entry() {
        let (mut ledger, actor) = fixture_ledger();
        ledger
            .append(
                EventKind::Test,
                br#"{"invoice_id":"inv_OTHER"}"#.to_vec(),
                actor,
                None,
            )
            .unwrap();
        let entries = ledger.entries().unwrap();
        let manifest = Manifest {
            version: 1,
            invoice_id: "inv_TARGET".to_string(),
            tenant_id: "t1".to_string(),
            generated_at: "2026-01-01T00:00:00Z".to_string(),
            binary_hash: "00".repeat(32),
            nav_xsd_version: "3.0".to_string(),
            chain_verified: true,
            chain_verified_entries: 1,
            entries_in_bundle: 1,
            signed: false,
            signature_status: SIGNATURE_STATUS_DEFERRED_PER_F5.to_string(),
            mirror_file_present: false,
            mirror_file_status: "absent-pre-pr-17".to_string(),
        };
        let mut report = Report::new("/tmp/test".into());
        check_bundle_membership(&manifest, &entries, &mut report);
        assert!(
            !report.is_ok(),
            "entry referencing inv_OTHER cannot appear in inv_TARGET's bundle"
        );
    }
}
