//! The one COSE shape a passkey RP needs: an EC2 / P-256 / ES256
//! public key.
//!
//! WebAuthn wraps the credential public key as a COSE_Key (RFC 8152)
//! CBOR map inside the attested credential data. Apple's platform
//! authenticator — Face ID on the iPhone, Touch ID on the Mac, the two
//! authenticators ADR-0115 §4.1 targets — produces exactly one
//! algorithm: `alg: -7`, ES256, ECDSA over NIST P-256. So this parser
//! accepts exactly that and refuses everything else, rather than
//! carrying a general COSE decoder whose extra branches nobody
//! exercises.
//!
//! Refusing unknown algorithms is a control, not an omission: the
//! registration ceremony advertises `pubKeyCredParams: [{alg: -7}]`,
//! and an authenticator that answered with something else is either
//! broken or not the authenticator we think it is.

/// COSE map labels used below (RFC 8152 §7.1 / §13.1).
mod label {
    pub const KTY: i128 = 1;
    pub const ALG: i128 = 3;
    pub const CRV: i128 = -1;
    pub const X: i128 = -2;
    pub const Y: i128 = -3;
}

/// The values we require.
mod expect {
    /// `kty: EC2`
    pub const KTY_EC2: i128 = 2;
    /// `alg: ES256`
    pub const ALG_ES256: i128 = -7;
    /// `crv: P-256`
    pub const CRV_P256: i128 = 1;
}

/// `alg` value advertised in `pubKeyCredParams` and required back.
pub const ES256: i32 = -7;

#[derive(Debug, thiserror::Error)]
pub enum CoseError {
    #[error("credential public key is not valid CBOR: {0}")]
    Cbor(String),
    #[error("credential public key is not a CBOR map")]
    NotAMap,
    #[error("credential public key is missing COSE label {0}")]
    MissingLabel(i128),
    #[error("credential public key label {label} has an unexpected value (wanted {wanted})")]
    Unexpected { label: i128, wanted: i128 },
    #[error("credential public key coordinate is {len} bytes, expected 32")]
    BadCoordinate { len: usize },
    #[error("credential public key is not a valid P-256 point")]
    NotOnCurve,
}

/// A parsed ES256 credential public key, kept as the raw affine
/// coordinates so the credential store is a pair of hex strings rather
/// than an opaque blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Es256PublicKey {
    pub x: [u8; 32],
    pub y: [u8; 32],
}

impl Es256PublicKey {
    /// Parse a COSE_Key from its CBOR bytes.
    pub fn from_cose_cbor(bytes: &[u8]) -> Result<Self, CoseError> {
        let value: ciborium::value::Value =
            ciborium::from_reader(bytes).map_err(|e| CoseError::Cbor(e.to_string()))?;
        Self::from_cose_value(&value)
    }

    /// Parse an already-decoded COSE_Key value.
    pub fn from_cose_value(value: &ciborium::value::Value) -> Result<Self, CoseError> {
        let map = value.as_map().ok_or(CoseError::NotAMap)?;

        let int_at = |label: i128| -> Option<i128> {
            map.iter()
                .find_map(|(k, v)| (as_int(k)? == label).then(|| as_int(v)).flatten())
        };
        let bytes_at = |label: i128| -> Option<&Vec<u8>> {
            map.iter()
                .find_map(|(k, v)| (as_int(k)? == label).then(|| v.as_bytes()).flatten())
        };

        for (label, wanted) in [
            (label::KTY, expect::KTY_EC2),
            (label::ALG, expect::ALG_ES256),
            (label::CRV, expect::CRV_P256),
        ] {
            let got = int_at(label).ok_or(CoseError::MissingLabel(label))?;
            if got != wanted {
                return Err(CoseError::Unexpected { label, wanted });
            }
        }

        let x = coord(bytes_at(label::X).ok_or(CoseError::MissingLabel(label::X))?)?;
        let y = coord(bytes_at(label::Y).ok_or(CoseError::MissingLabel(label::Y))?)?;
        let key = Self { x, y };
        // Reject a point that is not actually on the curve before it
        // ever reaches the credential store: a stored non-point would
        // fail every later assertion in a way that looks like a broken
        // authenticator rather than a rejected registration.
        key.verifying_key()?;
        Ok(key)
    }

    /// The `p256` verifying key for this credential.
    pub fn verifying_key(&self) -> Result<p256::ecdsa::VerifyingKey, CoseError> {
        let mut sec1 = [0u8; 65];
        sec1[0] = 0x04; // uncompressed point
        sec1[1..33].copy_from_slice(&self.x);
        sec1[33..65].copy_from_slice(&self.y);
        p256::ecdsa::VerifyingKey::from_sec1_bytes(&sec1).map_err(|_| CoseError::NotOnCurve)
    }
}

fn coord(raw: &[u8]) -> Result<[u8; 32], CoseError> {
    raw.try_into()
        .map_err(|_| CoseError::BadCoordinate { len: raw.len() })
}

fn as_int(v: &ciborium::value::Value) -> Option<i128> {
    match v {
        ciborium::value::Value::Integer(i) => Some(i128::from(*i)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ciborium::value::Value;

    /// A real P-256 point, so `verifying_key` has something valid to
    /// accept. Derived here rather than hard-coded so the test cannot
    /// drift from the curve implementation.
    fn valid_point() -> ([u8; 32], [u8; 32]) {
        let sk = p256::ecdsa::SigningKey::from_bytes(&[7u8; 32].into()).expect("test key");
        let pk = sk.verifying_key().to_encoded_point(false);
        let mut x = [0u8; 32];
        let mut y = [0u8; 32];
        x.copy_from_slice(pk.x().expect("x"));
        y.copy_from_slice(pk.y().expect("y"));
        (x, y)
    }

    fn cose(kty: i128, alg: i128, crv: i128, x: &[u8], y: &[u8]) -> Value {
        Value::Map(vec![
            (
                Value::Integer(1.into()),
                Value::Integer(kty.try_into().expect("kty")),
            ),
            (
                Value::Integer(3.into()),
                Value::Integer(alg.try_into().expect("alg")),
            ),
            (
                Value::Integer((-1i64).into()),
                Value::Integer(crv.try_into().expect("crv")),
            ),
            (Value::Integer((-2i64).into()), Value::Bytes(x.to_vec())),
            (Value::Integer((-3i64).into()), Value::Bytes(y.to_vec())),
        ])
    }

    #[test]
    fn parses_an_es256_key() {
        let (x, y) = valid_point();
        let k = Es256PublicKey::from_cose_value(&cose(2, -7, 1, &x, &y)).expect("parses");
        assert_eq!(k.x, x);
        assert_eq!(k.y, y);
        k.verifying_key().expect("is a curve point");
    }

    #[test]
    fn refuses_a_non_es256_algorithm() {
        // -8 is EdDSA. The ceremony never asks for it; an authenticator
        // that returns it is not the one we advertised to.
        let (x, y) = valid_point();
        assert!(matches!(
            Es256PublicKey::from_cose_value(&cose(2, -8, 1, &x, &y)),
            Err(CoseError::Unexpected { label: 3, .. })
        ));
    }

    #[test]
    fn refuses_a_non_ec2_key_type() {
        let (x, y) = valid_point();
        assert!(matches!(
            Es256PublicKey::from_cose_value(&cose(3, -7, 1, &x, &y)),
            Err(CoseError::Unexpected { label: 1, .. })
        ));
    }

    #[test]
    fn refuses_a_wrong_curve() {
        let (x, y) = valid_point();
        assert!(matches!(
            Es256PublicKey::from_cose_value(&cose(2, -7, 2, &x, &y)),
            Err(CoseError::Unexpected { label: -1, .. })
        ));
    }

    #[test]
    fn refuses_a_short_coordinate() {
        let (_, y) = valid_point();
        assert!(matches!(
            Es256PublicKey::from_cose_value(&cose(2, -7, 1, &[1u8; 31], &y)),
            Err(CoseError::BadCoordinate { len: 31 })
        ));
    }

    #[test]
    fn refuses_a_point_not_on_the_curve() {
        assert!(matches!(
            Es256PublicKey::from_cose_value(&cose(2, -7, 1, &[9u8; 32], &[9u8; 32])),
            Err(CoseError::NotOnCurve)
        ));
    }

    #[test]
    fn refuses_non_cbor_bytes() {
        assert!(matches!(
            Es256PublicKey::from_cose_cbor(&[0xff, 0xff, 0xff]),
            Err(CoseError::Cbor(_))
        ));
    }
}
