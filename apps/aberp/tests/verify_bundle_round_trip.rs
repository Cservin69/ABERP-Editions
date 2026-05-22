//! End-to-end writer-verifier round-trip for PR-22 / ADR-0035.
//!
//! Drives [`aberp::export_invoice_bundle::run`] against an in-process
//! DuckDB + audit-ledger fixture, then immediately runs
//! [`aberp_verify::verify_bundle`] against the produced archive and
//! asserts the verifier reports OK. The test pins the writer-verifier
//! agreement at the SHAPE level — any future drift between the bundle
//! writer (apps/aberp/src/export_invoice_bundle.rs) and the verifier
//! parser (crates/aberp-verify/src/bundle.rs + verify.rs) surfaces
//! here, not in a NAV inspector's complaint.
//!
//! Lives under apps/aberp/tests/ rather than crates/aberp-verify/tests/
//! because exercising the real writer requires the full aberp
//! dependency surface (DuckDB transitive, billing types, etc.) — the
//! verifier crate's own tests stay narrowly-scoped per ADR-0035 §2.
//!
//! Not env-gated. Runs in CI.
//!
//! Includes a TAMPER negative case: mutate the bundle's chain.jsonl
//! payload field after writing, re-pack with a forged but still-
//! parseable shape, and assert the verifier surfaces the per-entry
//! hash-recomputation FAIL.

use std::io::Read;
use std::path::PathBuf;

use aberp::audit_payloads;
use aberp::cli::ExportInvoiceBundleArgs;
use aberp_audit_ledger::{Actor, BinaryHash, EventKind, Ledger, TenantId};
use aberp_billing::IdempotencyKey;

const TEST_BINARY_HASH: BinaryHash = BinaryHash::from_bytes([0xCD; 32]);

fn temp_path(tag: &str, ext: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "aberp-verify-rt-{}-{}-{:?}-{}.{}",
        std::process::id(),
        tag,
        std::thread::current().id(),
        ulid::Ulid::new(),
        ext,
    ));
    p
}

fn seed_ledger_for(db_path: &std::path::Path, tenant: &str, invoice_id: &str) {
    let tenant_id = TenantId::new(tenant.to_string()).unwrap();
    let mut ledger = Ledger::open(db_path, tenant_id, TEST_BINARY_HASH).unwrap();
    let actor = Actor::from_local_cli("sess".to_string(), "test-user");
    let idem = IdempotencyKey::new();

    let attempt = audit_payloads::InvoiceSubmissionAttemptPayload::new(
        invoice_id,
        idem,
        "test",
        b"<ManageInvoiceRequest>rt</ManageInvoiceRequest>".to_vec(),
    );
    ledger
        .append(
            EventKind::InvoiceSubmissionAttempt,
            attempt.to_bytes(),
            actor.clone(),
            Some(idem.to_canonical_string()),
        )
        .unwrap();

    let response = audit_payloads::InvoiceSubmissionResponsePayload::new(
        invoice_id,
        idem,
        "TXID-RT",
        b"<ManageInvoiceResponse>rt</ManageInvoiceResponse>".to_vec(),
    );
    ledger
        .append(
            EventKind::InvoiceSubmissionResponse,
            response.to_bytes(),
            actor.clone(),
            Some(idem.to_canonical_string()),
        )
        .unwrap();

    let ack = audit_payloads::InvoiceAckStatusPayload::new(
        invoice_id,
        "TXID-RT",
        "SAVED",
        b"<QueryTransactionStatusResponse>SAVED</QueryTransactionStatusResponse>".to_vec(),
    );
    ledger
        .append(EventKind::InvoiceAckStatus, ack.to_bytes(), actor, None)
        .unwrap();
}

/// Happy path: write a bundle with three NAV-bearing entries, run
/// the verifier against it, assert every check passes. The
/// verifier's OK report is the contract.
#[test]
fn verifier_passes_real_writer_round_trip_three_entries() {
    let db = temp_path("db", "duckdb");
    let out = temp_path("bundle", "tar.zst");
    let _ = std::fs::remove_file(&db);
    let _ = std::fs::remove_file(&out);

    let tenant = "tenant-verify-rt";
    let invoice_id = "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV";
    seed_ledger_for(&db, tenant, invoice_id);

    let args = ExportInvoiceBundleArgs {
        invoice_id: invoice_id.to_string(),
        out: out.clone(),
        allow_overwrite: false,
        db: db.clone(),
        tenant: tenant.to_string(),
    };
    aberp::export_invoice_bundle::run(&args)
        .expect("writer succeeds on seeded ledger (round-trip baseline)");
    assert!(out.exists(), "writer produced the bundle file");

    let report = aberp_verify::verify_bundle(&out)
        .expect("verifier reads the writer-produced bundle without structural failure");
    assert!(
        report.is_ok(),
        "verifier must report OK on a writer-produced bundle (writer-verifier drift if not). \
         Failures must surface as report-level FAIL outcomes, NOT as panics."
    );

    // Cleanup.
    let _ = std::fs::remove_file(&out);
    let _ = std::fs::remove_file(&db);
    let _ = std::fs::remove_file(aberp_audit_ledger::mirror_path_for(&db));
}

/// Mirror-synced variant: when the operator has run a post-PR-17
/// `sync_mirror` call, the manifest's `mirror_file_present` flips
/// to true and `mirror_file_status` to `"verified-agreement"`. The
/// verifier must accept this happy-path mirror posture and surface
/// the echo NOTE (per ADR-0035 §"Adversarial review" #5 — the
/// verifier echoes the status; does NOT independently re-verify
/// the mirror file).
#[test]
fn verifier_passes_on_writer_output_with_verified_mirror_agreement() {
    let db = temp_path("db", "duckdb");
    let out = temp_path("bundle", "tar.zst");
    let _ = std::fs::remove_file(&db);
    let _ = std::fs::remove_file(&out);

    let tenant = "tenant-verify-rt-mirror";
    let invoice_id = "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV";
    seed_ledger_for(&db, tenant, invoice_id);

    // Sync the mirror via the same surface the binary's post-commit
    // code uses — mirrors `export_invoice_bundle_smoke::run_produces_verified_agreement_bundle_when_mirror_is_synced`.
    let tenant_id = TenantId::new(tenant.to_string()).unwrap();
    let ledger = Ledger::open(&db, tenant_id, TEST_BINARY_HASH).unwrap();
    let mirror_path = aberp_audit_ledger::mirror_path_for(&db);
    let head = ledger.sync_mirror(&mirror_path).unwrap();
    assert_eq!(head, 3, "mirror backfills all three seeded entries");

    let args = ExportInvoiceBundleArgs {
        invoice_id: invoice_id.to_string(),
        out: out.clone(),
        allow_overwrite: false,
        db: db.clone(),
        tenant: tenant.to_string(),
    };
    aberp::export_invoice_bundle::run(&args).expect("writer succeeds");
    let report = aberp_verify::verify_bundle(&out).expect("verifier reads bundle");
    assert!(
        report.is_ok(),
        "verifier accepts the verified-agreement mirror posture"
    );

    // Cleanup.
    let _ = std::fs::remove_file(&out);
    let _ = std::fs::remove_file(&db);
    let _ = std::fs::remove_file(&mirror_path);
}

/// TAMPER negative case: write a real bundle, then mutate the
/// chain.jsonl `payload` field on one entry (preserving the claimed
/// `entry_hash`) and re-pack. The verifier MUST surface the per-
/// entry hash-recomputation FAIL — silent acceptance of tampered
/// audit evidence is the exact failure mode CLAUDE.md rule 12 names.
///
/// Tampering happens at the bundle-byte level (not the DB level)
/// because the bundle reader's chain_verified gate would refuse to
/// emit a bundle from a tampered DB; the threat model here is "an
/// attacker modifies the bundle file in transit, after a clean
/// ABERP produced it." The verifier's job is catching that
/// post-production tampering.
#[test]
fn verifier_fails_on_post_production_chain_jsonl_payload_tampering() {
    let db = temp_path("db", "duckdb");
    let out = temp_path("bundle", "tar.zst");
    let tampered_out = temp_path("bundle-tampered", "tar.zst");
    let _ = std::fs::remove_file(&db);
    let _ = std::fs::remove_file(&out);
    let _ = std::fs::remove_file(&tampered_out);

    let tenant = "tenant-verify-rt-tamper";
    let invoice_id = "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV";
    seed_ledger_for(&db, tenant, invoice_id);

    let args = ExportInvoiceBundleArgs {
        invoice_id: invoice_id.to_string(),
        out: out.clone(),
        allow_overwrite: false,
        db: db.clone(),
        tenant: tenant.to_string(),
    };
    aberp::export_invoice_bundle::run(&args).expect("writer succeeds (clean baseline)");

    // Read the clean bundle, mutate one chain.jsonl entry's payload
    // to base64("{}"), re-pack.
    let clean = std::fs::read(&out).unwrap();
    let decoded = zstd::stream::decode_all(&clean[..]).unwrap();
    let mut ar = tar::Archive::new(&decoded[..]);
    let mut manifest_bytes: Vec<u8> = Vec::new();
    let mut chain_bytes: Vec<u8> = Vec::new();
    let mut nav_files: Vec<(String, Vec<u8>)> = Vec::new();
    for entry in ar.entries().unwrap() {
        let mut entry = entry.unwrap();
        let path = entry.path().unwrap().display().to_string();
        let mut buf = Vec::new();
        entry.read_to_end(&mut buf).unwrap();
        if path == "bundle/manifest.json" {
            manifest_bytes = buf;
        } else if path == "bundle/chain.jsonl" {
            chain_bytes = buf;
        } else if path.starts_with("bundle/nav/") {
            nav_files.push((path, buf));
        } else {
            panic!("unexpected archive path: {path}");
        }
    }

    // Replace the FIRST line's payload field with base64("{}") =
    // "e30=". The line's entry_hash claim stays the same, so when
    // the verifier recomputes the hash from the mutated payload
    // bytes it'll diverge — exactly the tamper detection target.
    let text = String::from_utf8(chain_bytes).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3, "baseline has three lines");
    let first = lines[0];
    let mut first_obj: serde_json::Value = serde_json::from_str(first).unwrap();
    first_obj["payload"] = serde_json::json!("e30=");
    let tampered_first = serde_json::to_string(&first_obj).unwrap();
    let new_text = format!("{}\n{}\n{}\n", tampered_first, lines[1], lines[2]);
    let tampered_chain = new_text.into_bytes();

    // Re-pack: manifest + tampered chain.jsonl + the original nav/
    // files. Same archive shape; the verifier should still parse
    // and reach the per-entry-hash check.
    let file = std::fs::File::create(&tampered_out).unwrap();
    let zstd_enc = zstd::stream::write::Encoder::new(file, 0).unwrap().auto_finish();
    let mut builder = tar::Builder::new(zstd_enc);
    let append = |builder: &mut tar::Builder<_>, rel: &str, bytes: &[u8]| {
        let full = format!("bundle/{}", rel);
        let mut header = tar::Header::new_gnu();
        header.set_path(&full).unwrap();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(0);
        header.set_cksum();
        builder.append(&header, bytes).unwrap();
    };
    append(&mut builder, "manifest.json", &manifest_bytes);
    append(&mut builder, "chain.jsonl", &tampered_chain);
    for (path, bytes) in &nav_files {
        let rel = path.strip_prefix("bundle/").unwrap();
        append(&mut builder, rel, bytes);
    }
    let zstd_enc = builder.into_inner().unwrap();
    drop(zstd_enc);

    let report = aberp_verify::verify_bundle(&tampered_out)
        .expect("verifier parses the tampered bundle (semantic failure surfaces in report)");
    assert!(
        !report.is_ok(),
        "verifier MUST surface the per-entry-hash divergence as a FAIL on a tampered bundle. \
         A pass on tampered evidence is the exact silent-acceptance failure mode \
         CLAUDE.md rule 12 names."
    );

    // Cleanup.
    let _ = std::fs::remove_file(&out);
    let _ = std::fs::remove_file(&tampered_out);
    let _ = std::fs::remove_file(&db);
    let _ = std::fs::remove_file(aberp_audit_ledger::mirror_path_for(&db));
}
