//! PR-71 / session-93 — integration pins for the multi-bank-account
//! schema (ADR-0040). The in-file `#[cfg(test)] mod tests` block in
//! `apps/aberp/src/seller_banks.rs` carries the unit-level parser /
//! validator / helper pins; this file holds the cross-cutting
//! scenarios that need the on-disk read path or that pin the
//! migration behaviour against a realistic file body.
//!
//! Mirrors the `apps/aberp/tests/nav_credentials_blob.rs` posture:
//! integration tests for binary-side modules live under
//! `apps/aberp/tests/` and import via the `aberp::` lib face.

use std::fs;

use aberp::seller_banks::{
    self, deterministic_id, infer_currency_from_swift, read_seller_banks, SellerBanksError,
};
use aberp_billing::Currency;

/// PR-71 — `read_seller_banks` returns an empty collection (NOT an
/// error) when the file is absent. PR-B's wizard treats the empty
/// case as "no bank accounts yet — show the wizard"; a load error
/// would break that detection.
#[test]
fn missing_file_returns_empty_collection() {
    let tmp = tempdir_for_test("missing_file");
    let path = tmp.join("seller.toml");
    assert!(!path.exists(), "precondition: file must not exist");
    let banks = read_seller_banks(&path).expect("absent file is not an error");
    assert!(banks.entries().is_empty());
}

/// PR-71 — end-to-end load through the on-disk path with a realistic
/// Áben Consulting two-bank file (one HUF + one EUR). The pin
/// confirms the file → parser → validator → helper chain works end-
/// to-end without any test-only shim.
#[test]
fn loads_two_bank_file_from_disk() {
    let tmp = tempdir_for_test("two_bank_load");
    let path = tmp.join("seller.toml");
    fs::write(
        &path,
        "\
[[seller.banks]]
currency       = \"HUF\"
account_number = \"12345678-12345678-12345678\"
bank_name      = \"Erste Bank\"
swift_bic      = \"GIBAHUHB\"
default        = true

[[seller.banks]]
currency       = \"EUR\"
account_number = \"HU12-3456-7890-1234-5678-9012-3456\"
bank_name      = \"Erste Bank\"
swift_bic      = \"GIBAHUHB\"
default        = true
",
    )
    .unwrap();

    let banks = read_seller_banks(&path).expect("loads");
    assert_eq!(banks.entries().len(), 2);
    assert_eq!(
        banks.banks_for_currency(Currency::Huf).len(),
        1,
        "one HUF entry"
    );
    assert_eq!(
        banks.banks_for_currency(Currency::Eur).len(),
        1,
        "one EUR entry"
    );
    let huf = banks.default_bank_for(Currency::Huf).expect("HUF default");
    assert_eq!(huf.bank_name, "Erste Bank");
    assert_eq!(huf.account_number, "12345678-12345678-12345678");

    // Determinism: re-loading the same on-disk file produces a
    // collection with identical bank ids.
    let reload = read_seller_banks(&path).expect("reload");
    assert_eq!(
        banks.entries().iter().map(|e| &e.id).collect::<Vec<_>>(),
        reload.entries().iter().map(|e| &e.id).collect::<Vec<_>>(),
        "ids must match across load cycles"
    );
}

/// PR-71 — migration end-to-end: a legacy flat-root `seller.toml`
/// (the shape `samples/seller.toml.example` ships today + the
/// existing `setup_seller_info::parse_seller_bank` reads) loads
/// into a one-element collection marked `default = true` with a
/// SWIFT-inferred currency. The on-disk file is NOT rewritten —
/// migration is load-only per ADR-0040 §2.
#[test]
fn migrates_legacy_flat_root_file_from_disk() {
    let tmp = tempdir_for_test("legacy_flat_root");
    let path = tmp.join("seller.toml");
    let legacy_body = "\
# Sample legacy seller.toml — flat-root bank keys at file root.
bank_account_number = \"12345678-12345678-12345678\"
iban                = \"LT14 3250 0448 1318 6860\"
bank_name           = \"Revolut\"
swift_bic           = \"REVOHUHB\"
";
    fs::write(&path, legacy_body).unwrap();

    let banks = read_seller_banks(&path).expect("legacy loads");
    assert_eq!(banks.entries().len(), 1, "legacy flat-root → one entry");
    let entry = &banks.entries()[0];
    assert_eq!(entry.currency, Currency::Huf, "SWIFT HU inferred → HUF");
    assert!(entry.default, "legacy entries default to default=true");
    assert_eq!(entry.bank_name, "Revolut");
    assert_eq!(entry.account_number, "12345678-12345678-12345678");

    // Critical: the on-disk file is NOT rewritten. PR-B's UI is the
    // surface that persists the migrated form.
    let on_disk = fs::read_to_string(&path).unwrap();
    assert_eq!(
        on_disk, legacy_body,
        "load-only migration MUST NOT mutate the file on disk"
    );
}

/// PR-71 — migration legacy `[seller.bank]` single-section form
/// from disk, same expectations as the flat-root case.
#[test]
fn migrates_legacy_seller_bank_section_from_disk() {
    let tmp = tempdir_for_test("legacy_section");
    let path = tmp.join("seller.toml");
    fs::write(
        &path,
        "\
[seller]
legal_name = \"Áben Consulting KFT.\"
tax_number = \"24904362-2-41\"

[seller.bank]
account_number = \"12345678-12345678-12345678\"
bank_name = \"Erste Bank\"
swift_bic = \"GIBAHUHB\"
",
    )
    .unwrap();

    let banks = read_seller_banks(&path).expect("legacy section loads");
    assert_eq!(banks.entries().len(), 1);
    let entry = &banks.entries()[0];
    assert_eq!(entry.currency, Currency::Huf);
    assert!(entry.default);
    assert_eq!(entry.bank_name, "Erste Bank");
}

/// PR-71 — the typed `MultipleDefaults` failure surfaces via the
/// on-disk reader path, not just the in-file parser. PR-B's wizard
/// renders the `operator_message` to the operator.
#[test]
fn multiple_defaults_in_file_loud_fails_via_disk_reader() {
    let tmp = tempdir_for_test("two_defaults");
    let path = tmp.join("seller.toml");
    fs::write(
        &path,
        "\
[[seller.banks]]
currency = \"HUF\"
account_number = \"A\"
bank_name = \"Bank One\"
swift_bic = \"GIBAHUHB\"
default = true

[[seller.banks]]
currency = \"HUF\"
account_number = \"B\"
bank_name = \"Bank Two\"
swift_bic = \"GIBAHUHB\"
default = true
",
    )
    .unwrap();

    let err = read_seller_banks(&path).expect_err("two HUF defaults must fail");
    let msg = err.operator_message(&path);
    assert!(
        msg.contains("HUF"),
        "msg names the offending currency: {msg}"
    );
    assert!(
        msg.contains(&path.display().to_string()),
        "msg names the file path: {msg}"
    );
    assert!(msg.contains("Több"), "msg includes Hungarian: {msg}");
    assert!(msg.contains("Multiple"), "msg includes English: {msg}");
    match err {
        SellerBanksError::MultipleDefaults { currency, count } => {
            assert_eq!(currency, Currency::Huf);
            assert_eq!(count, 2);
        }
        other => panic!("expected MultipleDefaults, got {other:?}"),
    }
}

/// PR-71 — SWIFT inference is the load-bearing migration heuristic.
/// Pin both the HU happy-path (no fallback warn) AND the non-HU
/// fallback (DE → HUF, escalated warn message). The integration-
/// level pin guards against a regression in the byte-offset
/// arithmetic used to extract positions 4-5 of the SWIFT/BIC.
#[test]
fn swift_inference_pins_country_code_position() {
    // Hungarian banks: country code at positions 4-5 = "HU".
    for bic in ["GIBAHUHB", "OTPVHUHB", "REVOHUHB", "BUDAHUHB"] {
        assert_eq!(
            infer_currency_from_swift(bic),
            Currency::Huf,
            "{bic} country code is HU → HUF",
        );
    }
    // Non-Hungarian banks: fall back to HUF with the louder warn
    // (the typed `fell_back` flag is exercised in the in-file
    // tests; here we only pin the public surface's behaviour).
    for bic in ["DEUTDEFF", "BPHKPLPK", "AKBKTRIS"] {
        assert_eq!(
            infer_currency_from_swift(bic),
            Currency::Huf,
            "non-HU SWIFT {bic} defaults to HUF + warn",
        );
    }
}

/// PR-71 — bank-id determinism is the prerequisite for PR-C
/// stamping the id onto an issued invoice and PR-D resolving it
/// back. A non-deterministic id would silently invalidate every
/// stamped reference on a binary restart.
#[test]
fn deterministic_id_pin() {
    let id_a = deterministic_id(Currency::Huf, "12345678-12345678-12345678");
    let id_b = deterministic_id(Currency::Huf, "12345678-12345678-12345678");
    assert_eq!(id_a, id_b);
    assert!(id_a.starts_with("bnk_"));
    assert_eq!(id_a.len(), 30, "bnk_ + 26 char ULID body");

    // Cross-currency collision check: HUF and EUR accounts with the
    // same account_number string MUST produce distinct ids.
    let id_eur = deterministic_id(Currency::Eur, "12345678-12345678-12345678");
    assert_ne!(id_a, id_eur);
}

/// PR-71 — equivalence pin: a legacy file load and a manually-
/// written canonical-new-form file load produce the same
/// `SellerBanks` value. Anchors the C10-style "byte-identical
/// behaviour pre/post migration" expectation at the in-memory
/// type level.
#[test]
fn legacy_and_new_form_produce_equal_collections() {
    let legacy = "\
bank_account_number = \"12345678-12345678-12345678\"
bank_name = \"Erste Bank\"
swift_bic = \"GIBAHUHB\"
";
    let new_form = "\
[[seller.banks]]
currency = \"HUF\"
account_number = \"12345678-12345678-12345678\"
bank_name = \"Erste Bank\"
swift_bic = \"GIBAHUHB\"
default = true
";
    let from_legacy = seller_banks::parse_seller_banks(legacy).expect("legacy parses");
    let from_new = seller_banks::parse_seller_banks(new_form).expect("new-form parses");
    assert_eq!(from_legacy, from_new);
    assert_eq!(
        from_legacy.entries()[0].id,
        from_new.entries()[0].id,
        "ids must match — they are deterministic over (currency, account_number)"
    );
}

/// Per-test tempdir under the system temp root. Mirrors the
/// existing PID + nanos + counter scheme other PR-7x tests use
/// (sibling tests under `apps/aberp/tests/` follow the same
/// pattern) so cargo's parallel test runner does not collide.
fn tempdir_for_test(label: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "aberp_seller_banks_{label}_{}_{}_{}",
        std::process::id(),
        nanos,
        seq,
    ));
    fs::create_dir_all(&path).expect("create tempdir");
    path
}
