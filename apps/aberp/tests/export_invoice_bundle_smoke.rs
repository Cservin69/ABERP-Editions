//! Smoke test for the PR-16 `aberp export-invoice-bundle` orchestration.
//!
//! Drives [`aberp::export_invoice_bundle::run`] against an in-process
//! DuckDB + audit-ledger fixture (no NAV calls; no keychain access),
//! then untars + zstd-decompresses the produced archive and asserts the
//! manifest's fields + the `chain.jsonl` line count + the per-NAV-XML
//! file presence match ADR-0029 §3's contract.
//!
//! Not env-gated. Runs in CI.

use std::path::PathBuf;

use aberp::audit_payloads;
use aberp::cli::ExportInvoiceBundleArgs;
use aberp_audit_ledger::{Actor, BinaryHash, EventKind, Ledger, TenantId};
use aberp_billing::IdempotencyKey;

const TEST_BINARY_HASH: BinaryHash = BinaryHash::from_bytes([0xAB; 32]);

fn temp_path(tag: &str, ext: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "aberp-bundle-smoke-{}-{}-{:?}-{}.{}",
        std::process::id(),
        tag,
        std::thread::current().id(),
        ulid::Ulid::new(),
        ext,
    ));
    p
}

fn seed_ledger(db_path: &std::path::Path, invoice_id: &str) {
    let tenant = TenantId::new("tenant-bundle-smoke".to_string()).unwrap();
    let mut ledger = Ledger::open(db_path, tenant, TEST_BINARY_HASH).unwrap();
    let actor = Actor::from_local_cli("sess".to_string(), "test-user");
    let idem = IdempotencyKey::new();

    // Submission attempt — carries verbatim request_xml.
    let attempt = audit_payloads::InvoiceSubmissionAttemptPayload::new(
        invoice_id,
        idem,
        "test",
        b"<ManageInvoiceRequest>smoke</ManageInvoiceRequest>".to_vec(),
    );
    ledger
        .append(
            EventKind::InvoiceSubmissionAttempt,
            attempt.to_bytes(),
            actor.clone(),
            Some(idem.to_canonical_string()),
        )
        .unwrap();

    // Submission response — carries verbatim response_xml.
    let response = audit_payloads::InvoiceSubmissionResponsePayload::new(
        invoice_id,
        idem,
        "TXID-SMOKE",
        b"<ManageInvoiceResponse>smoke</ManageInvoiceResponse>".to_vec(),
    );
    ledger
        .append(
            EventKind::InvoiceSubmissionResponse,
            response.to_bytes(),
            actor.clone(),
            Some(idem.to_canonical_string()),
        )
        .unwrap();

    // Ack-status — carries verbatim response_xml.
    let ack = audit_payloads::InvoiceAckStatusPayload::new(
        invoice_id,
        "TXID-SMOKE",
        "SAVED",
        b"<QueryTransactionStatusResponse>SAVED</QueryTransactionStatusResponse>".to_vec(),
    );
    ledger
        .append(
            EventKind::InvoiceAckStatus,
            ack.to_bytes(),
            actor,
            None,
        )
        .unwrap();
}

/// End-to-end: seed a ledger with the three NAV-bearing lifecycle
/// entries for one invoice, run `export-invoice-bundle`, decompress
/// the produced archive, and assert the bundle's shape matches
/// ADR-0029 §3:
///
///   - top-level `bundle/` directory inside the archive,
///   - `bundle/manifest.json` with the right `invoice_id` +
///     `entries_in_bundle` count + deferred-gate strings,
///   - `bundle/chain.jsonl` with one line per entry,
///   - `bundle/nav/<seq>_<kind>.xml` for each NAV-bearing entry,
///     carrying the verbatim XML bytes.
#[test]
fn run_produces_well_formed_tar_zst_bundle() {
    let db = temp_path("db", "duckdb");
    let out = temp_path("bundle", "tar.zst");
    let _ = std::fs::remove_file(&db);
    let _ = std::fs::remove_file(&out);

    let invoice_id = "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV";
    seed_ledger(&db, invoice_id);

    let args = ExportInvoiceBundleArgs {
        invoice_id: invoice_id.to_string(),
        out: out.clone(),
        allow_overwrite: false,
        db: db.clone(),
        tenant: "tenant-bundle-smoke".to_string(),
    };
    aberp::export_invoice_bundle::run(&args).expect("bundle run succeeds against seeded ledger");

    assert!(out.exists(), "bundle output file must exist after run");

    // Decompress + read.
    let compressed = std::fs::read(&out).expect("read bundle file");
    let decoded = zstd::stream::decode_all(&compressed[..]).expect("zstd round-trip");
    let mut archive = tar::Archive::new(&decoded[..]);

    let mut manifest_json: Option<Vec<u8>> = None;
    let mut chain_jsonl: Option<Vec<u8>> = None;
    let mut nav_files: Vec<(String, Vec<u8>)> = Vec::new();

    for entry in archive.entries().expect("tar entries") {
        let mut entry = entry.expect("tar entry");
        let path = entry.path().expect("entry path").display().to_string();
        let mut bytes = Vec::new();
        std::io::copy(&mut entry, &mut bytes).expect("copy entry body");
        if path == "bundle/manifest.json" {
            manifest_json = Some(bytes);
        } else if path == "bundle/chain.jsonl" {
            chain_jsonl = Some(bytes);
        } else if path.starts_with("bundle/nav/") {
            nav_files.push((path, bytes));
        } else {
            panic!("unexpected archive path: {path}");
        }
    }

    // Manifest checks.
    let manifest_json = manifest_json.expect("manifest.json present in archive");
    let manifest: serde_json::Value =
        serde_json::from_slice(&manifest_json).expect("manifest.json parses as JSON");
    assert_eq!(manifest["version"], serde_json::json!(1));
    assert_eq!(manifest["invoice_id"], serde_json::json!(invoice_id));
    assert_eq!(
        manifest["tenant_id"],
        serde_json::json!("tenant-bundle-smoke")
    );
    assert_eq!(manifest["chain_verified"], serde_json::json!(true));
    assert_eq!(manifest["entries_in_bundle"], serde_json::json!(3));
    assert_eq!(manifest["signed"], serde_json::json!(false));
    assert_eq!(
        manifest["signature_status"],
        serde_json::json!("deferred-per-f5")
    );
    assert_eq!(manifest["mirror_file_present"], serde_json::json!(false));
    assert_eq!(
        manifest["mirror_file_status"],
        serde_json::json!("deferred-per-f10")
    );

    // chain.jsonl: one line per entry, three entries total.
    let chain_jsonl = chain_jsonl.expect("chain.jsonl present in archive");
    let line_count = chain_jsonl.iter().filter(|&&b| b == b'\n').count();
    assert_eq!(line_count, 3, "chain.jsonl carries one line per entry");

    // nav/: three NAV-bearing entries -> three files.
    assert_eq!(
        nav_files.len(),
        3,
        "expected 3 nav/*.xml files for the 3 NAV-bearing entries; got {:?}",
        nav_files.iter().map(|(p, _)| p).collect::<Vec<_>>()
    );

    // Verbatim XML preservation — the response_xml bytes come back
    // exactly as written to the audit payload.
    // Filename uses underscores throughout (the dotted
    // storage form `invoice.ack_status` is transformed to
    // `invoice_ack_status` per the bundle-filename safety
    // posture in `export_invoice_bundle::extract_nav_xml`).
    let saved_payload = nav_files
        .iter()
        .find(|(p, _)| p.contains("invoice_ack_status"))
        .expect("ack_status XML present");
    assert_eq!(
        saved_payload.1,
        b"<QueryTransactionStatusResponse>SAVED</QueryTransactionStatusResponse>",
        "verbatim NAV response bytes must round-trip through the bundle"
    );

    // Cleanup.
    let _ = std::fs::remove_file(&out);
    let _ = std::fs::remove_file(&db);
}

/// `--allow-overwrite=false` (the default) loud-fails when the
/// output path already exists. Defence-in-depth pin matching the
/// unit-test in `export_invoice_bundle::tests`.
#[test]
fn run_refuses_overwrite_by_default() {
    let db = temp_path("db", "duckdb");
    let out = temp_path("bundle", "tar.zst");
    let _ = std::fs::remove_file(&db);
    let _ = std::fs::remove_file(&out);
    // Pre-create the output file.
    std::fs::write(&out, b"existing").unwrap();

    let invoice_id = "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV";
    seed_ledger(&db, invoice_id);

    let args = ExportInvoiceBundleArgs {
        invoice_id: invoice_id.to_string(),
        out: out.clone(),
        allow_overwrite: false,
        db: db.clone(),
        tenant: "tenant-bundle-smoke".to_string(),
    };
    let err = aberp::export_invoice_bundle::run(&args).expect_err("refuse-overwrite default");
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
    let _ = std::fs::remove_file(&out);
    let _ = std::fs::remove_file(&db);
}

/// `--allow-overwrite=true` lets a follow-up export overwrite an
/// existing artifact. The opt-in surface is the operator's deliberate
/// decision per ADR-0029 §1.
#[test]
fn run_overwrites_when_explicitly_allowed() {
    let db = temp_path("db", "duckdb");
    let out = temp_path("bundle", "tar.zst");
    let _ = std::fs::remove_file(&db);
    let _ = std::fs::remove_file(&out);
    // Pre-create the output file.
    std::fs::write(&out, b"existing").unwrap();

    let invoice_id = "inv_01ARZ3NDEKTSV4RRFFQ69G5FAV";
    seed_ledger(&db, invoice_id);

    let args = ExportInvoiceBundleArgs {
        invoice_id: invoice_id.to_string(),
        out: out.clone(),
        allow_overwrite: true,
        db: db.clone(),
        tenant: "tenant-bundle-smoke".to_string(),
    };
    aberp::export_invoice_bundle::run(&args).expect("overwrite-allowed run succeeds");

    // The file is no longer the 8-byte "existing" placeholder.
    let bytes = std::fs::read(&out).unwrap();
    assert!(
        bytes.len() > 8,
        "overwrite produced a real archive, not the placeholder: {} bytes",
        bytes.len()
    );

    // Cleanup.
    let _ = std::fs::remove_file(&out);
    let _ = std::fs::remove_file(&db);
}

/// Missing invoice id (no entries in the ledger reference it) loud-
/// fails per ADR-0029 §1 + CLAUDE.md rule 12 — the silent-empty-
/// bundle failure mode is the wrong affordance.
#[test]
fn run_loud_fails_on_invoice_with_no_entries() {
    let db = temp_path("db", "duckdb");
    let out = temp_path("bundle", "tar.zst");
    let _ = std::fs::remove_file(&db);
    let _ = std::fs::remove_file(&out);

    // Seed the ledger for inv_REAL but ask for inv_GHOST.
    seed_ledger(&db, "inv_REAL");

    let args = ExportInvoiceBundleArgs {
        invoice_id: "inv_GHOST".to_string(),
        out: out.clone(),
        allow_overwrite: false,
        db: db.clone(),
        tenant: "tenant-bundle-smoke".to_string(),
    };
    let err = aberp::export_invoice_bundle::run(&args).expect_err("no-entries loud-fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("no audit-ledger entries reference invoice id"),
        "loud-fail must name the absence: got {msg}"
    );
    // No bundle file produced when run aborts before pack_bundle.
    assert!(
        !out.exists(),
        "bundle output must not exist after loud-fail; got file at {}",
        out.display()
    );

    // Cleanup.
    let _ = std::fs::remove_file(&db);
}
