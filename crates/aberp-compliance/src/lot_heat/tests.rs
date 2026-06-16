//! Unit tests for the S345 lot/heat traceability seed types.

use super::{
    validate_mtr_url, HeatId, LotId, MaterialTraceabilitySeed, MtrUrlError, TraceabilityError,
    MAX_ID_LEN, MAX_MTR_URL_LEN,
};

// ── S432 (ADR-0085) — MTR URL validation ────────────────────────────────────

#[test]
fn s432_mtr_url_empty_is_none() {
    assert_eq!(validate_mtr_url(""), Ok(None));
    assert_eq!(validate_mtr_url("   "), Ok(None));
}

#[test]
fn s432_mtr_url_file_scheme_accepted_and_trimmed() {
    assert_eq!(
        validate_mtr_url("  file:///certs/heat-9f3a.pdf  "),
        Ok(Some("file:///certs/heat-9f3a.pdf".to_string()))
    );
}

#[test]
fn s432_mtr_url_rejects_non_file_scheme() {
    assert_eq!(
        validate_mtr_url("https://certs.example/x.pdf"),
        Err(MtrUrlError::NotFileScheme {
            got: "https://certs.example/x.pdf".to_string(),
        })
    );
}

#[test]
fn s432_mtr_url_rejects_too_long() {
    let long = format!("file://{}", "a".repeat(MAX_MTR_URL_LEN));
    assert!(matches!(
        validate_mtr_url(&long),
        Err(MtrUrlError::TooLong { .. })
    ));
}

#[test]
fn s345_lot_id_accepts_alphanumeric_and_dash() {
    let id = LotId::new("LOT-2026-06-11-A3").expect("valid lot id");
    assert_eq!(id.as_str(), "LOT-2026-06-11-A3");
}

#[test]
fn s345_lot_id_rejects_empty() {
    assert_eq!(LotId::new(""), Err(TraceabilityError::Empty));
}

#[test]
fn s345_lot_id_rejects_whitespace() {
    // A space is neither alphanumeric nor '-', so it is the first invalid char.
    assert_eq!(
        LotId::new("LOT 123"),
        Err(TraceabilityError::InvalidChar { ch: ' ' })
    );
}

#[test]
fn s345_lot_id_rejects_underscore_and_symbols() {
    assert_eq!(
        LotId::new("LOT_123"),
        Err(TraceabilityError::InvalidChar { ch: '_' })
    );
    assert_eq!(
        LotId::new("LOT/123"),
        Err(TraceabilityError::InvalidChar { ch: '/' })
    );
}

#[test]
fn s345_lot_id_rejects_too_long() {
    // 33 chars — one over the limit.
    let raw = "A".repeat(MAX_ID_LEN + 1);
    assert_eq!(
        LotId::new(&raw),
        Err(TraceabilityError::TooLong {
            len: MAX_ID_LEN + 1
        })
    );
    // Exactly at the limit is accepted.
    let at_limit = "B".repeat(MAX_ID_LEN);
    assert!(LotId::new(&at_limit).is_ok());
}

#[test]
fn s345_heat_id_validates_with_same_rules() {
    assert!(HeatId::new("HEAT-99X").is_ok());
    assert_eq!(HeatId::new(""), Err(TraceabilityError::Empty));
    assert_eq!(
        HeatId::new("HEAT 99"),
        Err(TraceabilityError::InvalidChar { ch: ' ' })
    );
}

#[test]
fn s345_traceability_seed_construction_and_roundtrip() {
    let seed = MaterialTraceabilitySeed {
        lot: LotId::new("LOT-001").expect("lot"),
        heat: HeatId::new("HEAT-77").expect("heat"),
        mill_cert_id: Some("CERT-3.1-ABC".to_string()),
        country_of_origin: Some("DE".to_string()),
        melt_date: Some(1_700_000_000_000),
    };
    let json = serde_json::to_string(&seed).expect("serialize");
    let back: MaterialTraceabilitySeed = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(seed, back);
    assert_eq!(back.lot.as_str(), "LOT-001");
    assert_eq!(back.heat.as_str(), "HEAT-77");
}

#[test]
fn s345_traceability_seed_optional_fields_default_none() {
    let seed = MaterialTraceabilitySeed {
        lot: LotId::new("LOT-002").expect("lot"),
        heat: HeatId::new("HEAT-88").expect("heat"),
        mill_cert_id: None,
        country_of_origin: None,
        melt_date: None,
    };
    assert!(seed.mill_cert_id.is_none());
    assert!(seed.country_of_origin.is_none());
    assert!(seed.melt_date.is_none());
}
