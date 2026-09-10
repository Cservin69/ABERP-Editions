//! ADR-0122 — the **shipment** evidence bundle: the per-dispatch slice, and
//! (from slice 3) the `qc/` document writer that closes D-99 residual 1 / AC10.
//!
//! # Why a second bundle instead of widening the invoice one
//!
//! The QC report is bound to a **shipment**, not to an invoice:
//! `bind_reports_to_dispatch` runs inside `mark_shipped`'s single transaction
//! (ADR-0199 §D6), so the document set that answers "what left the building,
//! and was it conforming?" is dispatch-scoped by construction.
//!
//! It could not have been invoice-scoped anyway. ADR-0122 §F1 records the
//! severance in full: `mark_shipped`'s `spawned_invoice_id` names a `drf_*`
//! DRAFT, promotion to an `inv_*` is a form-fill the SPA performs, and the
//! delete that follows NULLs the dispatch's own pointer — so `inv_A -> dsp_X`
//! is derivable from neither the ledger nor the tables.
//!
//! # The membership rule (ADR-0122 §D1)
//!
//! Two explicit passes, stated rather than smuggled into one probe:
//!
//! 1. **Flat**, over a hand-listed field set — every entry whose payload names
//!    the target dispatch.
//! 2. **Transitive, exactly ONE declared hop** — every entry carrying a
//!    `qcr_id` that pass 1 collected. This is what puts `qcr.report_issued` in
//!    `chain.jsonl`, without which the verifier has no `rendered_sha256` to
//!    check the bundled PDF against.
//!
//! No closure is taken. A rule that chases every id it meets walks the whole
//! ledger; one declared hop over a named field is auditable and terminates.

use aberp_audit_ledger::{Entry, EventKind, Ledger, TenantId};
use anyhow::{anyhow, Context, Result};
use duckdb::Connection;
use serde::Deserialize;
use std::collections::BTreeSet;

use crate::binary_hash;
use crate::cli::ExportShipmentBundleArgs;
use crate::export_invoice_bundle::{
    build_chain_jsonl, detect_mirror_agreement, extract_nav_xml, pack_bundle, BundleManifest,
    MirrorAgreementStatus, NavXmlFile, QcOmission, QcPdfFile, MANIFEST_VERSION,
    MIRROR_FILE_STATUS_ABSENT_PRE_PR17, MIRROR_FILE_STATUS_VERIFIED, SIGNATURE_STATUS_DEFERRED,
};

/// `scope_kind` for the per-shipment bundle (ADR-0122 §D5).
pub(crate) const SCOPE_KIND_DISPATCH: &str = "dispatch";

/// `entity_kind` discriminant for the one `export.*` payload that names a
/// dispatch polymorphically — `ExportAccessCheckPayload`, written at
/// `apps/aberp/src/serve.rs:19650`. Its sibling
/// `ExportClassificationSetPayload` uses the same two fields for a *product*
/// and is deliberately out of a shipment slice (see the module docs).
const ENTITY_KIND_DISPATCH: &str = "dispatch";

/// Permissive probe over one entry's payload bytes (ADR-0122 §D1).
///
/// # The field set is LOAD-BEARING
///
/// Pass 1 reads three different field names because the payloads that name a
/// dispatch do not agree on one:
///
/// | field | payloads |
/// |---|---|
/// | `dsp_id` | `mes.dispatch_created`, `mes.dispatch_shipped`, `qcr.report_attached_to_shipment` |
/// | `shipment_id` | `export.shipment_logged` |
/// | `entity_id` (+ `entity_kind == "dispatch"`) | `export.access_check` |
///
/// The round-1 adversarial pass caught the single-field version of this rule
/// dropping the `export.*` family — the denied-party screening decision and
/// the export shipment record — out of a defense evidence bundle, with nothing
/// in the archive saying so.
///
/// # `export.classification_set` is NOT here, and that is correct
///
/// Round 1's table listed it alongside `export.access_check` because both use
/// `entity_kind`/`entity_id`. Building this proved that wrong: its firing site
/// writes `entity_kind: "product"` with the WO's `product_id`
/// (`crates/aberp-dispatch/src/repository.rs:698`). It is a determination about
/// a **commodity**, not about this shipment, and its id is a `prd_*` that no
/// dispatch-keyed rule can match.
///
/// Sweeping it in would need a third declared hop (dispatch → WO → product),
/// and that hop would pull **every other shipment's** classification rows for
/// the same product into a per-shipment bundle. The per-shipment fact is
/// carried where it belongs: `export.shipment_logged.ecn_or_authorization` is
/// "populated from the same determination the `export.classification_set` row
/// carries", scoped to this shipment, and it IS in the slice.
///
/// Pinned by `a_product_scoped_classification_row_is_not_in_a_shipment_slice`.
///
/// **A future payload type that names a dispatch by a fourth field name MUST
/// extend this struct in the same PR.** Its sibling
/// `BundleMembershipProbe` carries the same warning and has still been missed
/// once (`DispatchShippedPayload::spawned_invoice_id` is an invoice-id-shaped
/// field its hand-listed test never listed), so the pin below round-trips the
/// REAL payload structs rather than re-listing strings.
#[derive(Debug, Default, Deserialize)]
struct ShipmentMembershipProbe {
    // ── pass 1: the dispatch-naming field set ──
    dsp_id: Option<String>,
    shipment_id: Option<String>,
    entity_id: Option<String>,
    entity_kind: Option<String>,
    // ── pass 2: the one declared hop ──
    qcr_id: Option<String>,
}

impl ShipmentMembershipProbe {
    /// True iff this payload names `target` as its dispatch.
    ///
    /// Plain equality on `entity_id` is sound **because ids are
    /// prefix-namespaced** — a `dsp_<ULID>` can never collide with a `prd_` or
    /// `wo_`. `entity_kind` is checked anyway as belt-and-braces; a later
    /// contributor who drops it should know the prefix is what they are then
    /// leaning on.
    fn names_dispatch(&self, target: &str) -> bool {
        if target.is_empty() {
            return false;
        }
        if some_eq(&self.dsp_id, target) || some_eq(&self.shipment_id, target) {
            return true;
        }
        if some_eq(&self.entity_id, target) {
            // Absent `entity_kind` is accepted: the id prefix already
            // identifies the subject, and refusing here would drop a payload
            // that named the dispatch correctly.
            return match self.entity_kind.as_deref() {
                None => true,
                Some(k) => k == ENTITY_KIND_DISPATCH,
            };
        }
        false
    }

    /// The report id this payload is about, if any and non-empty.
    fn qcr_id(&self) -> Option<&str> {
        self.qcr_id.as_deref().filter(|s| !s.is_empty())
    }
}

fn some_eq(field: &Option<String>, target: &str) -> bool {
    matches!(field.as_deref(), Some(v) if !v.is_empty() && v == target)
}

/// Resolve the per-dispatch slice, in `seq` order (oldest first).
///
/// Refuses rather than returning a misleading archive:
///
/// - an empty `dsp_id` (defence-in-depth, mirroring `filter_invoice_slice`);
/// - a dispatch the ledger has never heard of;
/// - a dispatch that **never shipped** (ADR-0122 §D1b). Pass 1 matches
///   `mes.dispatch_created`, so a Drafted or cancelled dispatch yields a
///   one-entry slice — not empty, so a zero-entry check does not catch it, and
///   the operator would receive an archive named for a shipment that never
///   happened, **indistinguishable in shape from a shipment whose documents
///   were dropped**.
pub fn dispatch_slice(entries: &[Entry], dsp_id: &str) -> Result<Vec<Entry>> {
    if dsp_id.trim().is_empty() {
        return Err(anyhow!(
            "--dispatch-id is empty — the slice cannot match the empty string against \
             any payload's dispatch-shaped fields"
        ));
    }
    let target = dsp_id.trim();

    // One deserialization per entry, reused by both passes. A payload that
    // fails permissive JSON decode is excluded, exactly as the invoice
    // bundle's probe excludes it: `chain.jsonl` could not carry it cleanly
    // anyway, and the exclusion is visible in the entry counts.
    let probes: Vec<ShipmentMembershipProbe> = entries
        .iter()
        .map(|e| serde_json::from_slice(&e.payload).unwrap_or_default())
        .collect();

    // ── Pass 1 ──
    let mut in_slice: Vec<bool> = Vec::with_capacity(entries.len());
    let mut report_ids: BTreeSet<&str> = BTreeSet::new();
    let mut shipped = false;
    for (entry, probe) in entries.iter().zip(&probes) {
        let hit = probe.names_dispatch(target);
        if hit {
            if entry.kind == EventKind::DispatchShipped {
                shipped = true;
            }
            // Empties never enter the set. Unlike its sibling — which guards
            // only the TARGET — this set is built FROM payloads, so one
            // payload carrying `"qcr_id": ""` would otherwise sweep every
            // entry with an empty `qcr_id` into the slice.
            if let Some(id) = probe.qcr_id() {
                report_ids.insert(id);
            }
        }
        in_slice.push(hit);
    }

    if !in_slice.iter().any(|h| *h) {
        return Err(anyhow!(
            "no audit-ledger entries name dispatch {target} in any dispatch-shaped field \
             (dsp_id, shipment_id, entity_id) — check the id is correct and that --tenant \
             + --db point at the DB the ship command wrote to"
        ));
    }
    if !shipped {
        return Err(anyhow!(
            "dispatch {target} has no mes.dispatch_shipped entry — it was never shipped \
             (or was cancelled), and a SHIPMENT evidence bundle for it would be an \
             archive named for a delivery that did not happen"
        ));
    }

    // ── Pass 2: one declared hop, no closure ──
    //
    // The set is NOT re-grown from what this pass finds. `superseded_by_qcr_id`
    // on the void payload is a different field name, so the rule could not
    // chase it even by accident — but the bound is the rule's, not the field
    // names'.
    for (i, probe) in probes.iter().enumerate() {
        if in_slice[i] {
            continue;
        }
        if let Some(id) = probe.qcr_id() {
            if report_ids.contains(id) {
                in_slice[i] = true;
            }
        }
    }

    let mut slice: Vec<Entry> = entries
        .iter()
        .zip(&in_slice)
        .filter(|(_, hit)| **hit)
        .map(|(e, _)| e.clone())
        .collect();
    // Both passes walk the same list, so the union is already in order; the
    // sort states the invariant `chain.jsonl` depends on rather than relying
    // on it.
    slice.sort_by_key(|e| e.seq.as_u64());
    Ok(slice)
}

/// What became of one QC report the shipment is bound to (ADR-0122 §D2/§D3).
#[derive(Debug)]
enum QcDocumentOutcome {
    /// Re-rendered and its SHA matches the chain pin. Goes in the archive.
    Bundled(QcPdfFile),
    /// Belongs to this shipment, is not in the archive, and the manifest
    /// says so. An auditor must never have to infer a hole in a document set.
    Omitted(QcOmission),
}

/// Re-render one issued report and decide what to do with it (ADR-0122 §D2).
///
/// # The verdict is a 2x2, not a boolean
///
/// `render_report`'s `matches` is a SHA comparison and nothing else, so any
/// change to `aberp-qc-pdf`'s layout moves every byte of every report ever
/// issued. Refusing on that alone would make the first export after any
/// renderer deploy fail for every dispatch, forever, with no operator remedy —
/// and announce a routine upgrade as tampering.
///
/// `aberp-qc-pdf` carries its own crate version rather than the workspace's
/// `0.0.0` precisely so this question is answerable (`Cargo.toml:4-10`: "was
/// the RENDERER changed or were the rows TAMPERED with?"), and the version
/// prints into the page footer, so a bump necessarily changes the bytes.
///
/// | stored version | SHA | verdict |
/// |---|---|---|
/// | equal | equal | bundle it |
/// | **equal** | **differs** | **REFUSE the whole export** — the frozen rows moved |
/// | differs | differs | omit + name it: those bytes are no longer reproducible |
/// | differs | equal | cannot occur; treated as the ordinary path |
fn resolve_qc_document(conn: &Connection, tenant: &str, qcr_id: &str) -> Result<QcDocumentOutcome> {
    let current_renderer = aberp_qc_pdf::QC_PDF_RENDERER_VERSION;
    match crate::qc_report::render_report(conn, tenant, qcr_id) {
        Ok((report, bytes, sha, matches)) => {
            let stored_renderer = report.renderer_version.clone().unwrap_or_default();
            let same_renderer = stored_renderer == current_renderer;
            match matches {
                // Ordinary path, and the `differs`/`equal` cell that cannot
                // occur: an assertion there could only turn a harmless
                // surprise into an abort.
                Some(true) | None => Ok(QcDocumentOutcome::Bundled(QcPdfFile {
                    archive_path: format!("qc/{qcr_id}.pdf"),
                    bytes,
                })),
                Some(false) if same_renderer => {
                    let number = &report.report_number;
                    let pinned = report.rendered_sha256.as_deref().unwrap_or("(unpinned)");
                    Err(anyhow!(
                        "QC report {qcr_id} ({number}) re-renders to {sha} but the audit \
                         chain pins {pinned} — SAME renderer ({current_renderer}), \
                         DIFFERENT bytes, which means the frozen report rows changed under \
                         a report that is supposed to be frozen. Refusing to emit an \
                         evidence bundle from it."
                    ))
                }
                Some(false) => Ok(QcDocumentOutcome::Omitted(QcOmission {
                    qcr_id: qcr_id.to_string(),
                    report_number: Some(report.report_number.clone()),
                    reason: format!(
                        "renderer_version {stored_renderer} is no longer available \
                         (current {current_renderer}) — the issued bytes cannot be \
                         reproduced, and a document that does not hash to its chain pin \
                         must not be presented as the issued one (ADR-0122 §F2)"
                    ),
                })),
            }
        }
        // A report that is no longer CURRENT renders no document at all
        // (ADR-0199 round 3): its unmarked PDF would read as a valid
        // certificate. The refusal stands; the omission is NAMED.
        Err(crate::qc_report::QcReportError::NotCurrent(why)) => {
            Ok(QcDocumentOutcome::Omitted(QcOmission {
                qcr_id: qcr_id.to_string(),
                report_number: None,
                reason: format!(
                    "{why} — no document is issued for a report that no longer stands; \
                     the full record remains on GET /api/qc-reports/{qcr_id} and in \
                     chain.jsonl"
                ),
            }))
        }
        Err(e) => Err(anyhow!("render QC report {qcr_id} for the bundle: {e}")),
    }
}

/// `aberp export-shipment-bundle` (ADR-0122).
pub fn run(args: &ExportShipmentBundleArgs) -> Result<()> {
    // 1. Edition gate (§D6). QC reporting is Defense-only, and on Portable no
    //    report can exist at all — emitting an archive with an empty `qc/`
    //    would state an absence the edition, not the shipment, is responsible
    //    for.
    crate::build_profile::assert_qc_reporting_allowed("export a shipment evidence bundle")?;

    let tenant = TenantId::new(args.tenant.clone()).ok_or_else(|| {
        anyhow!(
            "--tenant value '{}' is empty or has a null byte",
            args.tenant
        )
    })?;
    if args.out.exists() && !args.allow_overwrite {
        return Err(anyhow!(
            "output path {} already exists — pass --allow-overwrite to overwrite",
            args.out.display()
        ));
    }

    let binary_hash_bytes = binary_hash::compute().context("compute binary hash")?;
    let ledger = Ledger::open(&args.db, tenant.clone(), binary_hash_bytes)
        .context("open audit ledger for export-shipment-bundle")?;

    // 2. Full-chain verify, same posture as the invoice bundle (ADR-0029 §6):
    //    a tampered chain must not be exported as if authoritative.
    let chain_verified_entries = ledger.verify_chain().with_context(|| {
        format!(
            "audit-ledger chain verification failed for tenant {} — refusing to emit a \
             bundle from a tampered chain",
            args.tenant
        )
    })?;
    let entries = ledger
        .entries()
        .context("read audit ledger entries for the shipment slice")?;

    // 3. The slice, and the mirror assertion (ADR-0030 §5 — Open Q2's default
    //    answer: same code, same refusal). Both read `entries`, so they run
    //    BEFORE the Ledger is consumed.
    let slice = dispatch_slice(&entries, &args.dispatch_id)?;
    let mirror_status = detect_mirror_agreement(&args.db, &entries)?;

    // 4. §D4 — give up the Ledger and keep its Connection. From here there is
    //    no chain API in scope, which is the point: the connection is
    //    transferred, never loaned.
    let conn = ledger.into_connection();

    // 5. Re-render each report the slice's own `qcr.report_issued` entries
    //    name. Keyed on the ISSUED entries specifically, because those are
    //    exactly the ones `aberp-verify` will look for a `rendered_sha256` on;
    //    bundling a document whose issued entry is not in `chain.jsonl` would
    //    be an orphan the verifier FAILs.
    let mut qc_files: Vec<QcPdfFile> = Vec::new();
    let mut qc_omitted: Vec<QcOmission> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for entry in &slice {
        if entry.kind != EventKind::QcReportIssued {
            continue;
        }
        let probe: ShipmentMembershipProbe =
            serde_json::from_slice(&entry.payload).unwrap_or_default();
        let Some(qcr_id) = probe.qcr_id() else {
            continue;
        };
        if !seen.insert(qcr_id.to_string()) {
            continue;
        }
        match resolve_qc_document(&conn, tenant.as_str(), qcr_id)? {
            QcDocumentOutcome::Bundled(f) => qc_files.push(f),
            QcDocumentOutcome::Omitted(o) => qc_omitted.push(o),
        }
    }

    // 6. Manifest + bodies.
    let (mirror_file_present, mirror_file_status) = match mirror_status {
        MirrorAgreementStatus::VerifiedAgreement => (true, MIRROR_FILE_STATUS_VERIFIED),
        MirrorAgreementStatus::AbsentPrePr17 => (false, MIRROR_FILE_STATUS_ABSENT_PRE_PR17),
    };
    let generated_at = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .context("format manifest generated_at as RFC3339")?;
    let manifest = BundleManifest {
        version: MANIFEST_VERSION,
        scope_kind: SCOPE_KIND_DISPATCH,
        scope_id: args.dispatch_id.trim(),
        // §F1 — a shipment cannot be joined back to the invoice that bills it,
        // so claiming one here would be a fabricated link.
        invoice_id: None,
        qc_documents: qc_files.len() as u64,
        qc_documents_omitted: qc_omitted,
        tenant_id: tenant.as_str(),
        generated_at,
        binary_hash: hex::encode(binary_hash_bytes.as_bytes()),
        nav_xsd_version: aberp_nav_xsd_validator::NAV_XSD_VERSION,
        chain_verified: true,
        chain_verified_entries,
        entries_in_bundle: slice.len() as u64,
        signed: false,
        signature_status: SIGNATURE_STATUS_DEFERRED,
        mirror_file_present,
        mirror_file_status,
    };
    let manifest_bytes =
        serde_json::to_vec_pretty(&manifest).context("serialize manifest.json (pretty)")?;
    let chain_jsonl_bytes = build_chain_jsonl(&slice)?;
    let mut nav_files: Vec<NavXmlFile> = Vec::new();
    for entry in &slice {
        if let Some(nav) = extract_nav_xml(entry)? {
            nav_files.push(nav);
        }
    }

    pack_bundle(
        &args.out,
        args.allow_overwrite,
        &manifest_bytes,
        &chain_jsonl_bytes,
        &nav_files,
        &qc_files,
    )?;

    let omitted = manifest.qc_documents_omitted.len();
    tracing::info!(
        dispatch_id = %args.dispatch_id,
        out = %args.out.display(),
        chain_verified_entries,
        entries_in_bundle = slice.len(),
        qc_documents = qc_files.len(),
        qc_documents_omitted = omitted,
        ?mirror_status,
        "export-shipment-bundle OK"
    );
    println!(
        "export-shipment-bundle OK: dispatch {} -> wrote bundle to {} (audit chain verified \
         across {} entries; {} entries in bundle; {} NAV-XML file(s); {} QC document(s)). \
         {}NOTE: this bundle is UNSIGNED (signing deferred per F5).",
        args.dispatch_id,
        args.out.display(),
        chain_verified_entries,
        slice.len(),
        nav_files.len(),
        qc_files.len(),
        if omitted == 0 {
            String::new()
        } else {
            // Named, never silent: a document set with a hole in it must say
            // so on the operator's screen as well as in the manifest.
            format!(
                "{omitted} QC document(s) belong to this shipment and are NOT in the \
                 archive — see manifest.qc_documents_omitted for the reason on each. "
            )
        }
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    //! ADR-0122 §D1. The pins that matter here are the ones round 1 named as
    //! blocking or non-blocking fixes; each is written so that reverting the
    //! fix fails the test, not so that it agrees with whatever the code does.

    use super::*;
    use aberp_audit_ledger::{Actor, BinaryHash, Ledger, TenantId};

    const DSP: &str = "dsp_01ARZ3NDEKTSV4RRFFQ69G5FAV";
    const OTHER_DSP: &str = "dsp_01BRZ3NDEKTSV4RRFFQ69G5FAV";
    const QCR: &str = "qcr_01CRZ3NDEKTSV4RRFFQ69G5FAV";

    fn actor() -> Actor {
        Actor::from_local_cli("01H0000000000000000000000Z".to_string(), "t")
    }

    /// Build a ledger from `(kind, payload-bytes)` pairs and read the entries
    /// back, so every test runs against REAL `Entry` values (real seq, real
    /// chain) rather than hand-built structs.
    fn entries_of(rows: Vec<(EventKind, Vec<u8>)>) -> Vec<Entry> {
        let mut ledger = Ledger::open_in_memory(
            TenantId::new("t-shipment-slice").unwrap(),
            BinaryHash::from_bytes([9u8; 32]),
        )
        .expect("in-memory ledger");
        for (kind, payload) in rows {
            ledger
                .append(kind, payload, actor(), None)
                .expect("append test entry");
        }
        ledger.entries().expect("read entries")
    }

    fn shipped_payload(dsp_id: &str) -> Vec<u8> {
        aberp_dispatch::DispatchShippedPayload {
            dsp_id: dsp_id.to_string(),
            wo_id: "wo_1".to_string(),
            partner_id: "ptr_1".to_string(),
            carrier_kind: aberp_dispatch::CarrierKind::SelfDelivery,
            tracking_number: None,
            shipped_at: "2026-01-01T00:00:00Z".to_string(),
            spawned_invoice_id: Some("drf_1".to_string()),
            actor: "op".to_string(),
            idempotency_key: "idem-1".to_string(),
        }
        .to_bytes()
    }

    fn created_payload(dsp_id: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "dsp_id": dsp_id,
            "wo_id": "wo_1",
            "partner_id": "ptr_1",
        }))
        .unwrap()
    }

    /// `qcr.report_attached_to_shipment`, field-for-field as
    /// `bind_reports_to_dispatch` emits it.
    fn attached_payload(qcr_id: &str, dsp_id: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "qcr_id": qcr_id,
            "report_number": "AS9102-1",
            "report_kind": "dimensional_inspection",
            "dsp_id": dsp_id,
            "wo_id": "wo_1",
            "disposition": "accept",
        }))
        .unwrap()
    }

    /// `qcr.report_issued` — note it carries NO `dsp_id`. That is the whole
    /// reason pass 2 exists: without the hop, the entry the verifier reads
    /// `rendered_sha256` from never reaches `chain.jsonl`.
    fn issued_payload(qcr_id: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "qcr_id": qcr_id,
            "report_number": "AS9102-1",
            "wo_id": "wo_1",
            "rendered_sha256": "ab".repeat(32),
            "renderer_version": "aberp-qc-pdf@1.0.0",
        }))
        .unwrap()
    }

    /// **B1 revert-proof — the `export.*` family reaches the slice.**
    ///
    /// Round 1 caught a `dsp_id`-only probe dropping these, which would have
    /// shipped a defense evidence bundle without its export-control record and
    /// said nothing about it.
    ///
    /// Both payloads are built from the REAL structs, so renaming
    /// `shipment_id` or `entity_id` breaks this test rather than silently
    /// shrinking the slice — which is the failure mode the sibling
    /// `BundleMembershipProbe`'s hand-listed string test did not catch on
    /// `spawned_invoice_id`.
    #[test]
    fn the_export_family_is_in_the_slice_by_its_own_field_names() {
        let shipment_logged = aberp_dispatch::ExportShipmentLoggedPayload {
            shipment_id: DSP.to_string(),
            exporter_party_id: "t".to_string(),
            recipient_party_id: "ptr_1".to_string(),
            recipient_country: "US".to_string(),
            ecn_or_authorization: Some("EAR99".to_string()),
            shipped_at_ms: 0,
            operator_user_id: "op".to_string(),
        }
        .to_bytes();
        let access_check = aberp_dispatch::ExportAccessCheckPayload {
            entity_kind: "dispatch".to_string(),
            entity_id: DSP.to_string(),
            operator_user_id: "op".to_string(),
            decision: "not_determined".to_string(),
            reason: "unscreened".to_string(),
            backend: "mock".to_string(),
            checked_at_ms: 0,
        }
        .to_bytes();

        let entries = entries_of(vec![
            (EventKind::DispatchShipped, shipped_payload(DSP)),
            (EventKind::ExportShipmentLogged, shipment_logged),
            (EventKind::ExportAccessCheck, access_check),
        ]);
        let slice = dispatch_slice(&entries, DSP).expect("slice resolves");

        assert_eq!(
            slice.len(),
            3,
            "shipment_id and entity_id name the dispatch just as dsp_id does"
        );
        for kind in [
            EventKind::ExportShipmentLogged,
            EventKind::ExportAccessCheck,
        ] {
            assert!(
                slice.iter().any(|e| e.kind == kind),
                "{kind:?} must be in a shipment evidence bundle"
            );
        }
    }

    /// The correction to round 1's D1 table, pinned so nobody "fixes" it back.
    ///
    /// `export.classification_set` is keyed `entity_kind: "product"` with the
    /// WO's `product_id` (`aberp-dispatch/src/repository.rs:698`) — a
    /// determination about a COMMODITY. It is not shipment-scoped, its id is a
    /// `prd_*`, and sweeping it would need a dispatch → WO → product hop that
    /// would drag every OTHER shipment's rows for the same product into this
    /// bundle.
    #[test]
    fn a_product_scoped_classification_row_is_not_in_a_shipment_slice() {
        let classification = aberp_dispatch::ExportClassificationSetPayload {
            entity_kind: "product".to_string(),
            entity_id: "prd_1".to_string(),
            eccn: Some("EAR99".to_string()),
            usml_category: None,
            jurisdiction: "EAR99".to_string(),
            operator_user_id: "op".to_string(),
            classified_at_ms: 0,
        }
        .to_bytes();
        let entries = entries_of(vec![
            (EventKind::DispatchShipped, shipped_payload(DSP)),
            (EventKind::ExportClassificationSet, classification),
        ]);
        let slice = dispatch_slice(&entries, DSP).expect("slice resolves");
        assert_eq!(slice.len(), 1, "only the shipment row names this dispatch");
        assert!(slice
            .iter()
            .all(|e| e.kind != EventKind::ExportClassificationSet));
    }

    /// Pass 2's reason for existing: `qcr.report_issued` carries no `dsp_id`,
    /// so without the one declared hop the verifier would have no
    /// `rendered_sha256` in `chain.jsonl` to check a bundled PDF against.
    #[test]
    fn the_one_hop_pulls_in_the_issued_entry_that_carries_the_sha_pin() {
        let entries = entries_of(vec![
            (EventKind::DispatchShipped, shipped_payload(DSP)),
            (EventKind::QcReportIssued, issued_payload(QCR)),
            (
                EventKind::QcReportAttachedToShipment,
                attached_payload(QCR, DSP),
            ),
        ]);
        let slice = dispatch_slice(&entries, DSP).expect("slice resolves");
        assert_eq!(slice.len(), 3);
        assert!(slice.iter().any(|e| e.kind == EventKind::QcReportIssued));
        // And it is in chain order, not pass order: the issued entry was
        // appended BEFORE the attach that reached it.
        let seqs: Vec<u64> = slice.iter().map(|e| e.seq.as_u64()).collect();
        let mut sorted = seqs.clone();
        sorted.sort_unstable();
        assert_eq!(seqs, sorted, "chain.jsonl depends on seq order");
    }

    /// A report bound to ANOTHER dispatch does not reach this slice — the hop
    /// runs over the ids pass 1 found, not over every `qcr_id` in the ledger.
    #[test]
    fn a_report_bound_to_another_dispatch_is_not_swept_in() {
        let other_qcr = "qcr_01DRZ3NDEKTSV4RRFFQ69G5FAV";
        let entries = entries_of(vec![
            (EventKind::DispatchShipped, shipped_payload(DSP)),
            (EventKind::QcReportIssued, issued_payload(other_qcr)),
            (
                EventKind::QcReportAttachedToShipment,
                attached_payload(other_qcr, OTHER_DSP),
            ),
        ]);
        let slice = dispatch_slice(&entries, DSP).expect("slice resolves");
        assert_eq!(slice.len(), 1, "only this dispatch's own shipment row");
    }

    /// **C2 revert-proof — an empty `qcr_id` cannot sweep the ledger.**
    ///
    /// Unlike its sibling, which guards only the TARGET, this set is built
    /// FROM payloads. One row carrying `"qcr_id": ""` would otherwise put the
    /// empty string in the set and drag in every entry with an empty
    /// `qcr_id` — including ones belonging to other dispatches entirely.
    #[test]
    fn an_empty_report_id_never_enters_the_hop_set() {
        let empty_attach = serde_json::to_vec(&serde_json::json!({
            "qcr_id": "",
            "dsp_id": DSP,
        }))
        .unwrap();
        // A foreign row that also carries an empty qcr_id and names NO
        // dispatch: it must stay out.
        let foreign = serde_json::to_vec(&serde_json::json!({
            "qcr_id": "",
            "wo_id": "wo_OTHER",
        }))
        .unwrap();
        let entries = entries_of(vec![
            (EventKind::DispatchShipped, shipped_payload(DSP)),
            (EventKind::QcReportAttachedToShipment, empty_attach),
            (EventKind::QcReportIssued, foreign),
        ]);
        let slice = dispatch_slice(&entries, DSP).expect("slice resolves");
        assert_eq!(
            slice.len(),
            2,
            "the shipment row and the (malformed) attach that names the dispatch — \
             NOT the foreign row an empty-string match would have dragged in"
        );
        assert!(slice.iter().all(|e| e.kind != EventKind::QcReportIssued));
    }

    /// The hop does not chase `superseded_by_qcr_id`: it is a different field
    /// name, and the rule is one declared hop over `qcr_id` with no closure.
    #[test]
    fn the_hop_does_not_chase_a_supersede_pointer() {
        let successor = "qcr_01ERZ3NDEKTSV4RRFFQ69G5FAV";
        let voided = serde_json::to_vec(&serde_json::json!({
            "qcr_id": QCR,
            "report_number": "AS9102-1",
            "reason": "corrected",
            "superseded_by_qcr_id": successor,
            "new_state": "superseded",
        }))
        .unwrap();
        let entries = entries_of(vec![
            (EventKind::DispatchShipped, shipped_payload(DSP)),
            (
                EventKind::QcReportAttachedToShipment,
                attached_payload(QCR, DSP),
            ),
            (EventKind::QcReportVoided, voided),
            // The SUCCESSOR's own issued entry. It is not bound to this
            // dispatch, so no closure may reach it.
            (EventKind::QcReportIssued, issued_payload(successor)),
        ]);
        let slice = dispatch_slice(&entries, DSP).expect("slice resolves");
        assert_eq!(
            slice.len(),
            3,
            "the void entry joins (its own qcr_id is in the set); the successor's \
             issued entry does NOT — a closure would have taken it"
        );
        // Asserted on CONTENT, not on a seq number: the successor's id must
        // appear nowhere in the slice's bytes. A positional assertion would
        // keep passing if the entry were swept in under a different seq.
        assert!(
            !slice
                .iter()
                .any(|e| String::from_utf8_lossy(&e.payload).contains(successor)
                    && e.kind == EventKind::QcReportIssued),
            "the successor's issued entry is not bound to this dispatch and must not be reached"
        );
    }

    /// **C1 revert-proof — a dispatch that never shipped is refused.**
    ///
    /// Pass 1 matches `mes.dispatch_created`, so a Drafted or cancelled
    /// dispatch yields a one-entry slice. Non-empty, so a zero-entry check
    /// does not catch it, and the operator would get an archive named for a
    /// delivery that never happened — indistinguishable in shape from a
    /// shipment whose documents were dropped.
    #[test]
    fn a_dispatch_that_never_shipped_is_refused_not_bundled() {
        let entries = entries_of(vec![(EventKind::DispatchCreated, created_payload(DSP))]);
        let err = dispatch_slice(&entries, DSP).expect_err("a created-only dispatch must refuse");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("never shipped") && msg.contains(DSP),
            "the refusal must name the dispatch and say why: {msg}"
        );
    }

    /// An unknown dispatch refuses with the operator-actionable message, not
    /// with an empty archive.
    #[test]
    fn an_unknown_dispatch_refuses() {
        let entries = entries_of(vec![(EventKind::DispatchShipped, shipped_payload(DSP))]);
        let err = dispatch_slice(&entries, OTHER_DSP).expect_err("unknown dispatch must refuse");
        assert!(format!("{err:#}").contains("no audit-ledger entries name dispatch"));
    }

    /// An empty target matches nothing, mirroring
    /// `BundleMembershipProbe::matches`' defence-in-depth guard.
    #[test]
    fn an_empty_dispatch_id_is_refused() {
        let entries = entries_of(vec![(EventKind::DispatchShipped, shipped_payload(DSP))]);
        assert!(dispatch_slice(&entries, "").is_err());
        assert!(dispatch_slice(&entries, "   ").is_err());
    }

    /// A payload whose `entity_kind` names something else does not match, even
    /// if its `entity_id` were somehow the dispatch id. The id prefix already
    /// makes this unreachable in practice; the discriminant is the belt.
    #[test]
    fn an_entity_of_another_kind_does_not_match_on_id_alone() {
        let mismarked = serde_json::to_vec(&serde_json::json!({
            "entity_kind": "product",
            "entity_id": DSP,
        }))
        .unwrap();
        let entries = entries_of(vec![
            (EventKind::DispatchShipped, shipped_payload(DSP)),
            (EventKind::CuiMarkingApplied, mismarked),
        ]);
        let slice = dispatch_slice(&entries, DSP).expect("slice resolves");
        assert_eq!(slice.len(), 1);
    }

    /// A payload that is not JSON at all is excluded, not fatal — the same
    /// posture `bundle_membership_matches` takes.
    #[test]
    fn an_undecodable_payload_is_excluded_rather_than_fatal() {
        let entries = entries_of(vec![
            (EventKind::DispatchShipped, shipped_payload(DSP)),
            (EventKind::Test, b"not json at all".to_vec()),
        ]);
        let slice = dispatch_slice(&entries, DSP).expect("slice still resolves");
        assert_eq!(slice.len(), 1);
    }
}
