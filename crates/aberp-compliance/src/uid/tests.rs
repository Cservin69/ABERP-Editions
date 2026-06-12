//! Unit tests for the S358 MIL-STD-130N IUID format types (ADR-0075).
//!
//! S366 review F5 corrected the Construct 1 / Construct 2 swap: per MIL-STD-130N
//! and the DoD Guide to Uniquely Identifying Items, **Construct 1** = IAC + EID
//! + Serial (no part number, serial unique enterprise-wide); **Construct 2** =
//! IAC + EID + Original Part Number + Serial (serial unique within the part
//! number). These tests pin the corrected mapping.

use super::{
    validate_iac, Iuid, IuidConstruct1, IuidConstruct2, UidError, MAX_IAC_LEN, MAX_UID_FIELD_LEN,
};

#[test]
fn s358_construct1_builds_and_renders_iri() {
    // Construct 1 has no part number — serial is enterprise-wide unique.
    // IAC "D" (DUNS authority) + EID + serial.
    let c = IuidConstruct1::new("D", "0LH12", "SN-0001").expect("valid construct 1");
    assert_eq!(c.iac(), "D");
    assert_eq!(c.eid(), "0LH12");
    assert_eq!(c.serial(), "SN-0001");
    // IRI is the straight concatenation IAC + EID + SER.
    assert_eq!(c.to_iri(), "D0LH12SN-0001");
}

#[test]
fn s358_construct1_round_trips_through_serde() {
    let c = IuidConstruct1::new("UN", "CAGE9", "S-99").expect("valid");
    let json = serde_json::to_string(&c).expect("serialize");
    let back: IuidConstruct1 = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(c, back);
    assert_eq!(back.to_iri(), "UNCAGE9S-99");
}

#[test]
fn s358_construct2_builds_and_renders_iri() {
    // Construct 2 carries the original part number — serial is unique within it.
    let c =
        IuidConstruct2::new("D", "0LH12", "BRACKET-7781", "SN-0001").expect("valid construct 2");
    assert_eq!(c.iac(), "D");
    assert_eq!(c.eid(), "0LH12");
    assert_eq!(c.original_part_number(), "BRACKET-7781");
    assert_eq!(c.serial(), "SN-0001");
    // IRI is the straight concatenation IAC + EID + PNO + SER.
    assert_eq!(c.to_iri(), "D0LH12BRACKET-7781SN-0001");
}

#[test]
fn s358_construct2_round_trips_through_serde() {
    let c = IuidConstruct2::new("LH", "12345", "PN-42", "SN-7").expect("valid");
    let json = serde_json::to_string(&c).expect("serialize");
    let back: IuidConstruct2 = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(c, back);
    assert_eq!(back.to_iri(), "LH12345PN-42SN-7");
}

#[test]
fn s358_iuid_enum_dispatches_iri_and_construct_code() {
    let c1 = Iuid::Construct1(IuidConstruct1::new("D", "0LH12", "S-1").expect("c1"));
    let c2 = Iuid::Construct2(IuidConstruct2::new("D", "0LH12", "PN-1", "S-1").expect("c2"));
    // Construct 1 = serial-only; Construct 2 = +part number.
    assert_eq!(c1.to_iri(), "D0LH12S-1");
    assert_eq!(c2.to_iri(), "D0LH12PN-1S-1");
    assert_eq!(c1.construct_code(), "construct_1");
    assert_eq!(c2.construct_code(), "construct_2");
    // The two constructs are distinct discriminators.
    assert_ne!(c1.construct_code(), c2.construct_code());
}

#[test]
fn s358_iuid_enum_round_trips_through_serde() {
    let original =
        Iuid::Construct2(IuidConstruct2::new("UN", "CAGE9", "PN-42", "S-99").expect("c2"));
    let json = serde_json::to_string(&original).expect("serialize");
    let back: Iuid = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(original, back);
    assert_eq!(back.to_iri(), "UNCAGE9PN-42S-99");
}

#[test]
fn s358_validate_iac_accepts_registered_shapes() {
    // Single-char and two-char uppercase-alphanumeric IACs.
    assert!(validate_iac("D").is_ok());
    assert!(validate_iac("UN").is_ok());
    assert!(validate_iac("LH").is_ok());
    assert!(validate_iac("0A").is_ok());
}

#[test]
fn s358_validate_iac_rejects_bad_shapes() {
    // Empty.
    assert_eq!(validate_iac(""), Err(UidError::Empty { field: "iac" }));
    // Too long (3 chars, over the 2-char ceiling).
    assert_eq!(
        validate_iac("ABC"),
        Err(UidError::TooLong {
            field: "iac",
            len: 3,
            max: MAX_IAC_LEN,
        })
    );
    // Lowercase is not allowed — IACs are canonically uppercase.
    assert_eq!(
        validate_iac("d"),
        Err(UidError::InvalidChar {
            field: "iac",
            ch: 'd',
        })
    );
    // Symbol.
    assert_eq!(
        validate_iac("D!"),
        Err(UidError::InvalidChar {
            field: "iac",
            ch: '!',
        })
    );
}

#[test]
fn s358_construct_new_rejects_invalid_components() {
    // Bad IAC propagates from validate_iac (Construct 1, serial-only form).
    assert_eq!(
        IuidConstruct1::new("abc", "0LH12", "S-1"),
        Err(UidError::TooLong {
            field: "iac",
            len: 3,
            max: MAX_IAC_LEN,
        })
    );
    // Empty serial is rejected by the shared field gate, naming the field.
    assert_eq!(
        IuidConstruct1::new("D", "0LH12", ""),
        Err(UidError::Empty { field: "serial" })
    );
    // Whitespace in the enterprise id is an invalid char.
    assert_eq!(
        IuidConstruct1::new("D", "0L H12", "S-1"),
        Err(UidError::InvalidChar {
            field: "eid",
            ch: ' ',
        })
    );
    // A field over the ceiling is rejected with its length — the Construct 2
    // original part number, which only that construct carries.
    let too_long = "A".repeat(MAX_UID_FIELD_LEN + 1);
    assert_eq!(
        IuidConstruct2::new("D", "0LH12", &too_long, "S-1"),
        Err(UidError::TooLong {
            field: "original_part_number",
            len: MAX_UID_FIELD_LEN + 1,
            max: MAX_UID_FIELD_LEN,
        })
    );
}
