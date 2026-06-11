//! S352 / PR-41 — integration tests for the operator-facing PDF
//! view/download route (`GET /api/quote-pricing-jobs/:quote_id/pdf`).
//!
//! Tests hit the `pub` library discriminator `read_pricing_job_pdf`
//! directly (the same posture as `serve_pdf_route.rs` for the invoice
//! PDF: the HTTP status/header emission is structural in the handler —
//! axum's `into_response` builds the `(headers, body)` tuple — so the
//! load-bearing pin is the 404-vs-200 discrimination). The Bearer 401
//! gate is the shared `check_bearer_rejection`, pinned by its own unit
//! tests in `serve.rs`; the `Content-Disposition` filename is pinned by
//! `pricing_job_pdf_filename_carries_ref` in `serve.rs`.
//!
//! Covered here:
//! 1. **happy path** — a rendered row → `Found(bytes)`, bytes match the
//!    file written to disk verbatim.
//! 2. **not yet rendered** — a row whose `pdf_path` is NULL →
//!    `NotRendered` (the 404 `PdfNotRendered` discriminator).
//! 3. **wrong tenant** — a rendered row owned by another tenant is
//!    invisible → `NotFound` (404, not 403).
//! 4. **file missing on disk** — `pdf_path` set but the file was wiped →
//!    `FileMissing` (the operator-actionable 404 `PdfFileMissing`).

use std::path::PathBuf;

use ulid::Ulid;

use aberp::quote_pricing_jobs::{self, JobState};
use aberp::serve::{read_pricing_job_pdf, PricingJobPdfOutcome};

const TEST_TENANT: &str = "serve_pricing_pdf_test";
const OTHER_TENANT: &str = "serve_pricing_pdf_other";

fn test_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("aberp-serve-pricing-pdf")
        .join(format!("{}-{}", label, Ulid::new()));
    std::fs::create_dir_all(&dir).expect("create test dir");
    dir
}

fn fixed_ts() -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp(1_750_000_000).unwrap()
}

/// Insert a Fetched row carrying the customer's submission. No PDF yet.
fn seed_fetched_row(db_path: &PathBuf, tenant: &str, quote_id: &str) {
    let conn = duckdb::Connection::open(db_path).expect("open db");
    quote_pricing_jobs::insert_fetched_job(
        &conn,
        quote_id,
        tenant,
        "cust@example.com",
        "Customer Kft.",
        "6061-T6",
        4,
        "bracket.step",
        "/tmp/bracket.step",
        fixed_ts(),
    )
    .expect("insert job");
}

/// Drive a freshly-inserted row all the way to a rendered PDF, stamping
/// `pdf_path` to `pdf_path` (which the caller may or may not have
/// actually created on disk — the "file missing" case plants a path to
/// a file it never writes).
fn drive_to_rendered(db_path: &PathBuf, tenant: &str, quote_id: &str, pdf_path: &str) {
    let mut conn = duckdb::Connection::open(db_path).expect("open db");
    quote_pricing_jobs::set_state(&conn, quote_id, tenant, JobState::Extracting, fixed_ts())
        .expect("ex");
    quote_pricing_jobs::set_extracted(&mut conn, quote_id, tenant, "blake3:x", "{}", fixed_ts())
        .expect("extract");
    quote_pricing_jobs::set_priced(&mut conn, quote_id, tenant, "{}", 10.0, fixed_ts())
        .expect("price");
    quote_pricing_jobs::set_rendered(
        &mut conn,
        quote_id,
        tenant,
        pdf_path,
        "2026-07-06",
        fixed_ts(),
    )
    .expect("render");
}

#[test]
fn pdf_happy_path_returns_bytes_verbatim() {
    let dir = test_dir("happy");
    let db = dir.join("aberp.duckdb");
    let qid = "550e8400-e29b-41d4-a716-446655440000";
    let pdf_path = dir.join("priced.pdf");
    // A minimal PDF-ish byte sequence; the route streams bytes verbatim
    // (it does not parse), so any non-empty blob proves the round-trip.
    let body = b"%PDF-1.7\n stub quote pdf bytes \n%%EOF";
    std::fs::write(&pdf_path, body).expect("write pdf");

    seed_fetched_row(&db, TEST_TENANT, qid);
    drive_to_rendered(&db, TEST_TENANT, qid, pdf_path.to_str().unwrap());

    let conn = duckdb::Connection::open(&db).expect("open db");
    let outcome = read_pricing_job_pdf(&conn, qid, TEST_TENANT).expect("read pdf");
    match outcome {
        PricingJobPdfOutcome::Found(bytes) => assert_eq!(bytes, body),
        other => panic!("expected Found, got {}", outcome_name(&other)),
    }
}

#[test]
fn pdf_not_rendered_is_distinct_404() {
    let dir = test_dir("notrendered");
    let db = dir.join("aberp.duckdb");
    let qid = "11111111-1111-1111-1111-111111111111";
    seed_fetched_row(&db, TEST_TENANT, qid);

    let conn = duckdb::Connection::open(&db).expect("open db");
    let outcome = read_pricing_job_pdf(&conn, qid, TEST_TENANT).expect("read pdf");
    assert!(
        matches!(outcome, PricingJobPdfOutcome::NotRendered),
        "a Fetched row with NULL pdf_path must map to NotRendered, got {}",
        outcome_name(&outcome)
    );
}

#[test]
fn pdf_wrong_tenant_is_not_found() {
    let dir = test_dir("wrongtenant");
    let db = dir.join("aberp.duckdb");
    let qid = "22222222-2222-2222-2222-222222222222";
    let pdf_path = dir.join("priced.pdf");
    std::fs::write(&pdf_path, b"%PDF-1.7\n other tenant \n%%EOF").expect("write pdf");

    // Render the row under OTHER_TENANT, then read it as TEST_TENANT.
    seed_fetched_row(&db, OTHER_TENANT, qid);
    drive_to_rendered(&db, OTHER_TENANT, qid, pdf_path.to_str().unwrap());

    let conn = duckdb::Connection::open(&db).expect("open db");
    let outcome = read_pricing_job_pdf(&conn, qid, TEST_TENANT).expect("read pdf");
    assert!(
        matches!(outcome, PricingJobPdfOutcome::NotFound),
        "a foreign-tenant row must be invisible (NotFound), got {}",
        outcome_name(&outcome)
    );
}

#[test]
fn pdf_file_missing_on_disk_is_actionable_404() {
    let dir = test_dir("filemissing");
    let db = dir.join("aberp.duckdb");
    let qid = "33333333-3333-3333-3333-333333333333";
    // Stamp pdf_path to a file we deliberately NEVER create on disk.
    let phantom = dir.join("priced.pdf");

    seed_fetched_row(&db, TEST_TENANT, qid);
    drive_to_rendered(&db, TEST_TENANT, qid, phantom.to_str().unwrap());

    let conn = duckdb::Connection::open(&db).expect("open db");
    let outcome = read_pricing_job_pdf(&conn, qid, TEST_TENANT).expect("read pdf");
    assert!(
        matches!(outcome, PricingJobPdfOutcome::FileMissing),
        "pdf_path set but file gone must map to FileMissing, got {}",
        outcome_name(&outcome)
    );
}

fn outcome_name(o: &PricingJobPdfOutcome) -> &'static str {
    match o {
        PricingJobPdfOutcome::NotFound => "NotFound",
        PricingJobPdfOutcome::NotRendered => "NotRendered",
        PricingJobPdfOutcome::FileMissing => "FileMissing",
        PricingJobPdfOutcome::Found(_) => "Found",
    }
}
