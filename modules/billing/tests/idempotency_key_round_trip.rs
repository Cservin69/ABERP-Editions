//! PR-49 — round-trip + format-stability pins for the
//! `invoice.idempotency_key` column.
//!
//! Two pins, both load-bearing for the SPA list+detail handlers (PR-44ζ
//! `list_invoices` / `get_invoice_detail` → `read_invoice_row` →
//! `billing::load_ready_invoice_by_id`):
//!
//! 1. **Round-trip** — write a row through `allocate_in_tx` with a known
//!    [`IdempotencyKey`], read it back through `load_ready_invoice_by_id`,
//!    assert the parsed key equals the original. This is the canonical
//!    guard against the format mismatch that lay latent from PR-6 →
//!    PR-7-B-1: WRITE used `Ulid::to_string()`, READ used
//!    `IdempotencyKey::from_canonical_string()`, no test exercised both
//!    sides in one tx until the SPA first issued + listed a row.
//!
//! 2. **Format stability** — query the raw `idempotency_key` column and
//!    assert it matches the canonical `idem_<26-char-Crockford-base32-ULID>`
//!    shape per ADR-0005. A future serde-rename or impl-change that
//!    drifts the on-disk form silently is caught here, not by a
//!    production read-back failure 70ms after the first issuance.

use duckdb::Connection;
use time::OffsetDateTime;

use aberp_billing::{
    allocate_in_tx, load_ready_invoice_by_id, AllocateArgs, AllocateOutcome, BillingStore,
    Currency, CustomerId, DraftInvoice, DuckDbBillingStore, Huf, IdempotencyKey, InvoiceId,
    InvoiceSeries, LineItem, ResetPolicy, SeriesCode, SeriesId,
};

fn temp_db_path(label: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "aberp-idem-rw-{}-{}.duckdb",
        label,
        ulid::Ulid::new()
    ));
    let _ = std::fs::remove_file(&p);
    p
}

fn series_code() -> SeriesCode {
    SeriesCode::new("INV-test").expect("series code valid")
}

fn pre_tx_setup(path: &std::path::Path) -> (Connection, SeriesId) {
    let mut billing = DuckDbBillingStore::open(path).expect("open billing store");
    billing.ensure_schema().expect("ensure billing schema");
    let series = InvoiceSeries {
        id: SeriesId::new(),
        code: series_code(),
        reset_policy: ResetPolicy::Never,
        fiscal_year: None,
        created_at: OffsetDateTime::now_utc(),
    };
    billing.create_series(&series).expect("create series");
    let series_id = series.id;
    let conn = billing.into_connection();
    (conn, series_id)
}

fn build_allocate_args(series_id: SeriesId, idem: IdempotencyKey) -> AllocateArgs {
    AllocateArgs {
        series_id,
        draft: DraftInvoice {
            id: InvoiceId::new(),
            series_id,
            customer_id: CustomerId::new(),
            lines: vec![LineItem {
                description: "round-trip line".to_string(),
                quantity: rust_decimal::Decimal::from(1),
                unit_price: Huf(1_000),
                vat_rate_basis_points: 2700,
                vat_rate_kind: aberp_billing::VatRateKind::Percent,
                note: None,
                unit: None,
            }],
            issue_date: OffsetDateTime::now_utc(),
            // PR-84 — in-memory store / round-trip test path. Default
            // both invoice-date fields to the issue date (preserves
            // pre-PR-84 behaviour for unit-test surfaces; the SPA path
            // is the only surface that exercises operator-supplied
            // dates today).
            payment_deadline: OffsetDateTime::now_utc().date(),
            delivery_date: OffsetDateTime::now_utc().date(),
        },
        idempotency_key: idem,
        currency: Currency::Huf,
        rate_metadata: None,
        // PR-73 — round-trip test exercises the in-memory store path
        // which does not run the route resolver. Match the
        // `modules/billing/src/app/issue_invoice.rs` handler's posture
        // (per the comment there): in-process callers pass `None`;
        // only the SPA-issue route populates this field.
        bank_snapshot: None,
        invoice_note: None,
        email_recipient_override: None,
        start_value: 1,
        sequence_floor: None,
    }
}

/// Pin 1 — round-trip.
///
/// Issue an invoice via `allocate_in_tx`, then read it back via
/// `load_ready_invoice_by_id` in a fresh tx. The returned
/// `IdempotencyKey` must equal the one passed in. If the WRITE side
/// drifts away from the canonical form (or the READ side drifts away
/// from it), this fails loud and names the regression.
#[test]
fn allocate_in_tx_idempotency_key_round_trips_through_load_ready() {
    let path = temp_db_path("round-trip");
    let (mut conn, series_id) = pre_tx_setup(&path);

    let original_key = IdempotencyKey::new();
    let draft_id_str = {
        let tx = conn.transaction().expect("begin tx");
        let args = build_allocate_args(series_id, original_key);
        let id_str = args.draft.id.to_prefixed_string();
        let outcome =
            allocate_in_tx(&tx, args, OffsetDateTime::now_utc()).expect("allocate_in_tx Ok");
        match outcome {
            AllocateOutcome::Fresh { .. } => {}
            AllocateOutcome::Replay { .. } => panic!("fresh issuance must not Replay"),
        }
        tx.commit().expect("commit write tx");
        id_str
    };

    let read_back = {
        let tx = conn.transaction().expect("begin read tx");
        let pair =
            load_ready_invoice_by_id(&tx, &draft_id_str).expect("load_ready_invoice_by_id Ok");
        tx.commit().expect("commit read tx");
        pair.expect("invoice must be present after allocate+commit")
    };

    let (_invoice, parsed_key) = read_back;
    assert_eq!(
        parsed_key, original_key,
        "round-trip: WRITE (allocate_in_tx) and READ (load_ready_invoice_by_id) \
         must agree on the on-disk format of invoice.idempotency_key"
    );

    drop(conn);
    let _ = std::fs::remove_file(&path);
}

/// Pin 2 — format stability.
///
/// Open the raw DuckDB Connection and SELECT the `idempotency_key`
/// column directly. Assert the on-disk string matches the canonical
/// `idem_<26-char-Crockford-base32-ULID>` shape per ADR-0005 — the same
/// shape the audit-ledger's `idempotency_key` column carries (F8
/// contract). A drift in either direction (silently switching to bare
/// `Ulid::to_string()`, or prefixing with something else, or adding a
/// serde transform) fails this pin before any read-back path is
/// affected.
#[test]
fn invoice_idempotency_key_on_disk_format_is_canonical_idem_prefix() {
    let path = temp_db_path("on-disk-format");
    let (mut conn, series_id) = pre_tx_setup(&path);

    let key = IdempotencyKey::new();
    let expected_on_disk = key.to_canonical_string();
    {
        let tx = conn.transaction().expect("begin tx");
        let args = build_allocate_args(series_id, key);
        allocate_in_tx(&tx, args, OffsetDateTime::now_utc()).expect("allocate_in_tx Ok");
        tx.commit().expect("commit");
    }

    let stored: String = {
        let mut stmt = conn
            .prepare("SELECT idempotency_key FROM invoice;")
            .expect("prepare select idempotency_key");
        let mut rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .expect("query_map");
        rows.next()
            .expect("at least one invoice row")
            .expect("read idempotency_key value")
    };

    assert_eq!(
        stored, expected_on_disk,
        "on-disk `invoice.idempotency_key` must equal IdempotencyKey::to_canonical_string()"
    );
    assert!(
        stored.starts_with("idem_"),
        "canonical form must carry the `idem_` prefix per ADR-0005 (got {stored:?})"
    );
    assert_eq!(
        stored.len(),
        5 + 26,
        "canonical form is exactly `idem_` + 26-char ULID (got {} chars)",
        stored.len()
    );

    drop(conn);
    let _ = std::fs::remove_file(&path);
}
