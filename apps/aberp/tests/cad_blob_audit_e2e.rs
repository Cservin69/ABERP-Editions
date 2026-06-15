//! S430 / ADR-0083 — CAD-blob read-audit emission + debounce, against a
//! real audit ledger.
//!
//! The crypto round-trip / tamper / legacy-passthrough cases live as unit
//! tests in `aberp::cad_blob`; the pipeline write-encrypts + read-decrypts
//! + tamper→Failed cases live as in-module tests in
//! `quote_pricing_pipeline`. This file proves the third leg: the three new
//! `cad.*` audit events actually land in the ledger, and the 60-second
//! debounce yields exactly one `CadBlobRead` per fetch burst.

use std::time::{Duration, Instant};

use aberp::cad_blob::{
    emit_blob_read, emit_key_provisioned, emit_legacy_plaintext_read, ReadDebounce, ReadPurpose,
};
use aberp_audit_ledger::{ensure_schema, recent_entries, BinaryHash};

fn temp_db(tag: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "aberp-s430-audit-{tag}-{}.duckdb",
        ulid::Ulid::new()
    ));
    let _ = std::fs::remove_file(&p);
    p
}

fn count_kind(db: &std::path::Path, kind: &str) -> usize {
    let conn = duckdb::Connection::open(db).expect("reopen");
    recent_entries(&conn, 200)
        .expect("recent")
        .iter()
        .filter(|e| e.kind.as_str() == kind)
        .count()
}

fn hash() -> BinaryHash {
    BinaryHash::from_bytes([0u8; 32])
}

#[test]
fn key_provisioned_event_lands_in_ledger() {
    let db = temp_db("prov");
    {
        let mut conn = duckdb::Connection::open(&db).unwrap();
        ensure_schema(&conn).unwrap();
        emit_key_provisioned(&mut conn, "T", hash(), "ervin").unwrap();
    }
    assert_eq!(count_kind(&db, "cad.blob_key_provisioned"), 1);
    let _ = std::fs::remove_file(&db);
}

#[test]
fn legacy_plaintext_read_event_lands_in_ledger() {
    let db = temp_db("legacy");
    {
        let mut conn = duckdb::Connection::open(&db).unwrap();
        ensure_schema(&conn).unwrap();
        emit_legacy_plaintext_read(&mut conn, "T", hash(), "ervin", "q1", "ervin").unwrap();
    }
    assert_eq!(count_kind(&db, "cad.blob_legacy_plaintext_read"), 1);
    let _ = std::fs::remove_file(&db);
}

/// The customer-journey gate's read leg: preview (audited) → reprice
/// (within 60s, debounced) → a later reprice (after the window, audited).
/// Exactly TWO `CadBlobRead` rows — the middle reprice is suppressed.
#[test]
fn read_audit_debounces_within_60s_and_records_purpose() {
    let db = temp_db("read");
    let debounce = ReadDebounce::new();
    let t0 = Instant::now();
    {
        let mut conn = duckdb::Connection::open(&db).unwrap();
        ensure_schema(&conn).unwrap();

        // Fetch 1 — operator preview.
        assert!(debounce.should_emit("ervin", "q1", t0));
        emit_blob_read(
            &mut conn,
            "T",
            hash(),
            "ervin",
            "q1",
            "ervin",
            ReadPurpose::Preview,
        )
        .unwrap();

        // Fetch 2 — reprice 30s later, SAME requester + blob → debounced.
        if debounce.should_emit("ervin", "q1", t0 + Duration::from_secs(30)) {
            emit_blob_read(
                &mut conn,
                "T",
                hash(),
                "ervin",
                "q1",
                "ervin",
                ReadPurpose::Reprice,
            )
            .unwrap();
        }

        // Fetch 3 — reprice 61s later → window elapsed, audited again.
        if debounce.should_emit("ervin", "q1", t0 + Duration::from_secs(61)) {
            emit_blob_read(
                &mut conn,
                "T",
                hash(),
                "ervin",
                "q1",
                "ervin",
                ReadPurpose::Reprice,
            )
            .unwrap();
        }
    }

    assert_eq!(
        count_kind(&db, "cad.blob_read"),
        2,
        "preview + post-window reprice emit; the within-window reprice is debounced"
    );

    // The first row's payload carries the purpose verbatim.
    let conn = duckdb::Connection::open(&db).unwrap();
    let entries = recent_entries(&conn, 200).unwrap();
    let first_read = entries
        .iter()
        .filter(|e| e.kind.as_str() == "cad.blob_read")
        .min_by_key(|e| e.seq.as_u64())
        .expect("a read row");
    let payload: serde_json::Value = serde_json::from_slice(&first_read.payload).unwrap();
    assert_eq!(payload["purpose"], "preview");
    assert_eq!(payload["blob_id"], "q1");
    assert_eq!(payload["requester"], "ervin");
    drop(conn);
    let _ = std::fs::remove_file(&db);
}
