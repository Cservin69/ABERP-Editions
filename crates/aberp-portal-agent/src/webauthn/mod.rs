//! The WebAuthn relying party — **on the Mac** (ADR-0115 §4.2).
//!
//! This module is the load-bearing half of the whole design. §4.2:
//!
//! > The agent — not the VPS — stores credential public keys, issues
//! > challenges, verifies assertions, and mints sessions. The front
//! > merely relays ceremony messages as opaque frames.
//!
//! So: a relay compromise cannot mint a session and cannot read the
//! credential store. Nothing in `aberp-portal-relay` can reach this
//! code — it is not even in that crate's dependency list, which is the
//! cheapest possible proof.
//!
//! # Enrolment: what this used to claim, and what is actually true
//!
//! This paragraph used to say a relay compromise "cannot enrol a
//! credential" as well. That was **overstated**, and the correction is
//! worth stating plainly rather than quietly editing, because the same
//! sentence appeared in two other places and was load-bearing in all
//! three.
//!
//! What was true: the relay cannot *mint* an enrolment token, because
//! only the Mac's console does that. What did not follow: that it could
//! not *use* one. Until hardening H1 (§9.4, Phase 2) the enrolment
//! ceremony crosses relay memory in plaintext, so a compromised relay
//! can observe a live, console-minted, not-yet-spent token — and under
//! the original `attestation: "none"` posture there was nothing
//! downstream able to tell a Secure Enclave key from one the relay had
//! generated and held the private half of. It could therefore enrol,
//! and an enrolled credential is standing access that survives knock
//! rotation, relay redeploys and the compromise being cleaned up.
//!
//! Two Phase-0 controls now close that, neither of which depends on the
//! relay being honest:
//!
//! 1. [`attestation`] (§4.3a) requires the authenticator to prove it is
//!    Apple hardware, against a pinned root. Software cannot produce
//!    that chain.
//! 2. `crate::enrol::StagingStore` (§4.3b) commits nothing until a
//!    human at the Mac types a confirmation code, and alerts on every
//!    attempt.
//!
//! The honest summary is therefore: **a relay compromise cannot enrol a
//! credential, and the reason is these two controls — not the enrolment
//! token being secret from it.** H1 remains the fix for the relay
//! *seeing* the ceremony at all.
//!
//! # What is verified, and why each check is here
//!
//! | Check | Defeats |
//! |---|---|
//! | `type` is `webauthn.create`/`webauthn.get` | cross-ceremony replay |
//! | `challenge` matches an outstanding, unexpired, single-use nonce | replay of a captured ceremony |
//! | `origin` equals the configured portal origin | a phishing clone on another host (§G3) |
//! | `rpIdHash` equals SHA-256(rp_id) | the same, from the authenticator's side |
//! | `UV` flag set | a tap without the biometric (§4.3 `userVerification: required`) |
//! | signature over `authData ‖ SHA-256(clientDataJSON)` | everything else |
//! | sign-count regression | a cloned authenticator, where the platform counts |
//!
//! # Posture: passwordless, passkey-only
//!
//! Ervin's §9.1 decision, and the ADR's own recommendation. There is no
//! password anywhere in this crate — no hash, no verify, no reset, no
//! fallback. The recovery path is physical presence at the Mac
//! ([`crate::enrol`]), not a weaker second door.

pub mod attestation;
pub mod authdata;
pub mod cose;

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use p256::ecdsa::signature::Verifier as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use attestation::AttestationError;

use crate::credstore::Credential;
use crate::rand;
use authdata::AuthenticatorData;
use cose::Es256PublicKey;

/// Challenge lifetime. ADR-0115 §4.3: "single-use challenge (nonce,
/// 60 s TTL)".
pub const CHALLENGE_TTL: Duration = Duration::from_secs(60);

/// Ceiling on simultaneously outstanding challenges.
///
/// The ceremony routes are necessarily unauthenticated — they are how
/// one becomes authenticated — so anyone who has the knock token can
/// ask for challenges as fast as the relay will park them. Without a cap that
/// is an unbounded map on the Mac. One operator with a handful of
/// devices never approaches 256 within a 60-second window; a caller
/// who does is not Ervin, and is told so rather than being served
/// silently.
pub const MAX_OUTSTANDING_CHALLENGES: usize = 256;

/// Ceiling on outstanding challenges from **one source address**.
///
/// The global cap alone was a denial-of-service against the operator,
/// not a defence of the Mac. Anyone holding the knock could mint 256
/// challenges in a fraction of a second and hold the table full for a
/// minute at a time, and the next caller refused would be Ervin — who
/// would see a portal that had simply stopped letting him sign in, with
/// no indication why. An attacker who cannot authenticate but *can*
/// lock the operator out has achieved most of what they wanted.
///
/// Sixteen is far beyond any real device: one ceremony at a time per
/// device, a handful of devices, a 60-second TTL. A source past it is
/// not a browser.
pub const MAX_CHALLENGES_PER_SOURCE: usize = 16;

/// Which ceremony a challenge was minted for. A `create` challenge
/// presented in a `get` is refused: without this, an enrolment nonce
/// observed in flight could be steered into an authentication.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Ceremony {
    Create,
    Get,
}

impl Ceremony {
    /// The `type` field WebAuthn puts in `clientDataJSON`.
    #[must_use]
    pub fn client_data_type(self) -> &'static str {
        match self {
            Self::Create => "webauthn.create",
            Self::Get => "webauthn.get",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WebAuthnError {
    #[error("minting a challenge: {0}")]
    Rand(#[from] rand::RandError),
    #[error("clientDataJSON is not valid base64url")]
    ClientDataEncoding,
    #[error("clientDataJSON is not valid JSON: {0}")]
    ClientDataJson(#[from] serde_json::Error),
    #[error("ceremony type is `{got}`, expected `{want}`")]
    WrongCeremonyType { got: String, want: &'static str },
    #[error("challenge is unknown, expired, or already used")]
    UnknownChallenge,
    #[error(
        "too many ceremonies in flight ({MAX_OUTSTANDING_CHALLENGES}) — refusing to mint another"
    )]
    TooManyChallenges,
    #[error("origin is `{got}`, expected `{want}` — an assertion for another host verifies against nothing")]
    WrongOrigin { got: String, want: String },
    #[error("attestationObject is not valid base64url")]
    AttestationEncoding,
    #[error("attestationObject is not valid CBOR: {0}")]
    AttestationCbor(String),
    #[error("attestation refused: {0}")]
    Attestation(#[from] AttestationError),
    #[error("attestationObject has no fmt")]
    AttestationNoFmt,
    #[error("attestationObject has no authData")]
    AttestationNoAuthData,
    #[error("authenticator data: {0}")]
    AuthData(#[from] authdata::AuthDataError),
    #[error("authenticatorData is not valid base64url")]
    AuthDataEncoding,
    #[error("signature is not valid base64url")]
    SignatureEncoding,
    #[error("rpIdHash does not match this relying party")]
    WrongRelyingParty,
    #[error("user verification did not happen — `userVerification: required` (ADR-0115 §4.3)")]
    UserNotVerified,
    #[error("registration returned no attested credential data")]
    NoAttestedCredential,
    #[error("assertion signature does not verify")]
    BadSignature,
    #[error(
        "signature counter went backwards ({got} <= {stored}) — possible cloned authenticator"
    )]
    SignCountRegression { got: u32, stored: u32 },
    #[error("credential public key: {0}")]
    Cose(#[from] cose::CoseError),
}

/// The relying party's identity, from [`crate::config::AgentConfig`].
#[derive(Debug, Clone)]
pub struct RelyingParty {
    /// RP ID = the portal hostname (never committed — see
    /// [`crate::config`]).
    pub rp_id: String,
    /// Display name the OS shows during the ceremony.
    pub rp_name: String,
    /// Exact expected `origin` string.
    pub origin: String,
}

/// Outstanding challenges. In memory only: a challenge that did not
/// survive a daemon restart is a ceremony the user simply repeats, and
/// persisting nonces would create a replay window across restarts for
/// no benefit.
#[derive(Debug, Default)]
pub struct ChallengeStore {
    outstanding: Mutex<HashMap<String, Outstanding>>,
}

/// One outstanding challenge.
#[derive(Debug, Clone)]
struct Outstanding {
    ceremony: Ceremony,
    expires: Instant,
    /// Who asked. `None` when the relay supplied no peer — those are
    /// pooled together under one budget rather than being exempt from
    /// the cap, which would be the obvious way around it.
    source: Option<String>,
}

impl ChallengeStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Mint a fresh single-use challenge for `ceremony`.
    ///
    /// `source` is the peer address the relay reported — metadata, never
    /// a trust input. It is used only to decide *whose* budget this
    /// challenge comes out of, and the failure mode of a spoofed value
    /// is that the spoofer splits their own budget across more buckets,
    /// which does not help them.
    ///
    /// # Two caps, and why the second one evicts instead of refusing
    ///
    /// A source past [`MAX_CHALLENGES_PER_SOURCE`] is refused: it is not
    /// a browser, and telling it so is correct.
    ///
    /// When the *global* table is full, refusing would hand the
    /// attacker the operator lockout the per-source cap exists to
    /// prevent — a distributed flood would fill the table from many
    /// sources, each under its own cap, and Ervin would be refused
    /// again. So instead the heaviest source loses its oldest
    /// challenge. Ervin, who has one or two outstanding, is never the
    /// heaviest source and is therefore never the one evicted: the
    /// table degrades against whoever is flooding it, and the operator
    /// keeps working. That is the fail-open direction, and it is safe
    /// because an evicted challenge is not a weakened one — it simply
    /// stops existing, and the ceremony that owned it fails closed.
    pub fn mint(&self, ceremony: Ceremony, source: Option<&str>) -> Result<String, WebAuthnError> {
        let challenge = rand::token()?;
        let now = Instant::now();
        let mut g = self.lock();
        g.retain(|_, o| o.expires > now);

        let mine = g.values().filter(|o| o.source.as_deref() == source).count();
        if mine >= MAX_CHALLENGES_PER_SOURCE {
            return Err(WebAuthnError::TooManyChallenges);
        }

        if g.len() >= MAX_OUTSTANDING_CHALLENGES {
            evict_from_heaviest_source(&mut g);
        }
        // If eviction somehow found nothing to drop, fail closed rather
        // than letting the table grow past its bound.
        if g.len() >= MAX_OUTSTANDING_CHALLENGES {
            return Err(WebAuthnError::TooManyChallenges);
        }

        g.insert(
            challenge.clone(),
            Outstanding {
                ceremony,
                expires: now + CHALLENGE_TTL,
                source: source.map(str::to_string),
            },
        );
        Ok(challenge)
    }

    /// How many unexpired challenges are outstanding — the number the
    /// cap is measured against.
    #[must_use]
    pub fn outstanding(&self) -> usize {
        self.lock().len()
    }

    /// Consume `challenge` if it is outstanding, unexpired, and was
    /// minted for `ceremony`. Single-use: a second consume fails.
    pub fn consume(&self, challenge: &str, ceremony: Ceremony) -> bool {
        let mut g = self.lock();
        match g.get(challenge) {
            Some(o) if o.ceremony == ceremony && o.expires > Instant::now() => {
                g.remove(challenge);
                true
            }
            _ => false,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Outstanding>> {
        // A poisoned mutex means another thread panicked mid-ceremony.
        // The map holds only nonces, so recovering the guard is safe
        // and strictly better than taking the daemon down.
        self.outstanding
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Drop the oldest challenge belonging to whichever source holds the
/// most. See [`ChallengeStore::mint`] for why this direction.
fn evict_from_heaviest_source(g: &mut HashMap<String, Outstanding>) {
    let mut counts: HashMap<Option<String>, usize> = HashMap::new();
    for o in g.values() {
        *counts.entry(o.source.clone()).or_default() += 1;
    }
    let Some((heaviest, _)) = counts.into_iter().max_by_key(|(_, n)| *n) else {
        return;
    };
    let oldest = g
        .iter()
        .filter(|(_, o)| o.source == heaviest)
        .min_by_key(|(_, o)| o.expires)
        .map(|(k, _)| k.clone());
    if let Some(k) = oldest {
        g.remove(&k);
    }
}

/// Wall-clock seconds, for certificate validity windows.
///
/// Wall-clock rather than monotonic because it is compared against
/// dates printed inside certificates. A Mac whose clock is badly wrong
/// fails enrolment closed, which is the right direction: the
/// alternative is accepting an expired attestation chain.
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The subset of `clientDataJSON` the RP checks.
#[derive(Debug, Deserialize)]
struct ClientData {
    #[serde(rename = "type")]
    ceremony_type: String,
    challenge: String,
    origin: String,
}

/// What the browser posts back after `navigator.credentials.create()`.
#[derive(Debug, Deserialize)]
pub struct RegistrationResponse {
    /// base64url `clientDataJSON`.
    pub client_data_json: String,
    /// base64url `attestationObject`.
    pub attestation_object: String,
}

/// What the browser posts back after `navigator.credentials.get()`.
#[derive(Debug, Deserialize)]
pub struct AssertionResponse {
    /// base64url credential id (`rawId`).
    pub id: String,
    /// base64url `clientDataJSON`.
    pub client_data_json: String,
    /// base64url `authenticatorData`.
    pub authenticator_data: String,
    /// base64url DER ECDSA signature.
    pub signature: String,
}

/// Credential-creation options, serialised straight to the shell.
#[derive(Debug, Serialize)]
pub struct CreationOptions {
    pub rp: RpEntity,
    pub user: UserEntity,
    pub challenge: String,
    #[serde(rename = "pubKeyCredParams")]
    pub pub_key_cred_params: Vec<PubKeyCredParam>,
    pub timeout: u32,
    pub attestation: &'static str,
    #[serde(rename = "authenticatorSelection")]
    pub authenticator_selection: AuthenticatorSelection,
    #[serde(rename = "excludeCredentials")]
    pub exclude_credentials: Vec<CredentialDescriptor>,
}

#[derive(Debug, Serialize)]
pub struct RpEntity {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Serialize)]
pub struct UserEntity {
    pub id: String,
    pub name: String,
    #[serde(rename = "displayName")]
    pub display_name: String,
}

#[derive(Debug, Serialize)]
pub struct PubKeyCredParam {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub alg: i32,
}

#[derive(Debug, Serialize)]
pub struct AuthenticatorSelection {
    /// `platform` — Face ID / Touch ID, the §4.1 target. A roaming key
    /// is not what this design enrols.
    #[serde(rename = "authenticatorAttachment")]
    pub authenticator_attachment: &'static str,
    #[serde(rename = "residentKey")]
    pub resident_key: &'static str,
    /// Always `required` (§4.3).
    #[serde(rename = "userVerification")]
    pub user_verification: &'static str,
}

#[derive(Debug, Serialize)]
pub struct CredentialDescriptor {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub id: String,
}

/// Assertion-request options.
#[derive(Debug, Serialize)]
pub struct RequestOptions {
    pub challenge: String,
    #[serde(rename = "rpId")]
    pub rp_id: String,
    pub timeout: u32,
    #[serde(rename = "allowCredentials")]
    pub allow_credentials: Vec<CredentialDescriptor>,
    #[serde(rename = "userVerification")]
    pub user_verification: &'static str,
}

/// A verified registration, ready for the credential store.
#[derive(Debug)]
pub struct VerifiedRegistration {
    pub credential_id: String,
    pub public_key: Es256PublicKey,
    pub sign_count: u32,
}

impl RelyingParty {
    /// Build `navigator.credentials.create()` options.
    ///
    /// `exclude` carries the already-enrolled credential ids so the
    /// platform refuses to create a second passkey on a device that
    /// already has one.
    pub fn creation_options(
        &self,
        challenges: &ChallengeStore,
        user_handle: &str,
        exclude: &[Credential],
        source: Option<&str>,
    ) -> Result<CreationOptions, WebAuthnError> {
        Ok(CreationOptions {
            rp: RpEntity {
                id: self.rp_id.clone(),
                name: self.rp_name.clone(),
            },
            user: UserEntity {
                id: user_handle.to_string(),
                name: "operator".to_string(),
                display_name: format!("{} operator", self.rp_name),
            },
            challenge: challenges.mint(Ceremony::Create, source)?,
            pub_key_cred_params: vec![PubKeyCredParam {
                kind: "public-key",
                alg: cose::ES256,
            }],
            timeout: CHALLENGE_TTL.as_millis() as u32,
            // `direct` — ADR-0115 §4.3a. Phase 0 originally asked for
            // `none` on the reasoning that physical presence at the
            // Mac was the credential. That reasoning has a gap: the
            // enrolment ceremony crosses relay memory in plaintext
            // until hardening H1, so a compromised relay sees a live
            // console-minted token, and with `none` there was nothing
            // downstream able to tell a Secure Enclave key from one the
            // relay generated itself. See
            // [`attestation`] for the full argument.
            //
            // The "identifying certificate on the wire" objection that
            // motivated `none` does not apply to what Apple actually
            // sends: its platform attestation is *anonymous* — a
            // per-credential certificate carrying no device serial, no
            // account, and no stable identifier. There is nothing to
            // correlate across enrolments.
            attestation: "direct",
            authenticator_selection: AuthenticatorSelection {
                authenticator_attachment: "platform",
                resident_key: "preferred",
                user_verification: "required",
            },
            exclude_credentials: exclude
                .iter()
                .map(|c| CredentialDescriptor {
                    kind: "public-key",
                    id: c.id.clone(),
                })
                .collect(),
        })
    }

    /// Build `navigator.credentials.get()` options.
    pub fn request_options(
        &self,
        challenges: &ChallengeStore,
        enrolled: &[Credential],
        source: Option<&str>,
    ) -> Result<RequestOptions, WebAuthnError> {
        Ok(RequestOptions {
            challenge: challenges.mint(Ceremony::Get, source)?,
            rp_id: self.rp_id.clone(),
            timeout: CHALLENGE_TTL.as_millis() as u32,
            allow_credentials: enrolled
                .iter()
                .map(|c| CredentialDescriptor {
                    kind: "public-key",
                    id: c.id.clone(),
                })
                .collect(),
            user_verification: "required",
        })
    }

    fn check_client_data(
        &self,
        challenges: &ChallengeStore,
        client_data_json_b64: &str,
        ceremony: Ceremony,
    ) -> Result<Vec<u8>, WebAuthnError> {
        let raw =
            rand::b64url_decode(client_data_json_b64).ok_or(WebAuthnError::ClientDataEncoding)?;
        let cd: ClientData = serde_json::from_slice(&raw)?;

        let want_type = ceremony.client_data_type();
        if cd.ceremony_type != want_type {
            return Err(WebAuthnError::WrongCeremonyType {
                got: cd.ceremony_type,
                want: want_type,
            });
        }
        if cd.origin != self.origin {
            return Err(WebAuthnError::WrongOrigin {
                got: cd.origin,
                want: self.origin.clone(),
            });
        }
        // Consumed LAST of the three so a mismatched type or origin
        // cannot burn a legitimate outstanding challenge.
        if !challenges.consume(&cd.challenge, ceremony) {
            return Err(WebAuthnError::UnknownChallenge);
        }
        Ok(raw)
    }

    /// Verify a registration and return what to store.
    pub fn verify_registration(
        &self,
        challenges: &ChallengeStore,
        response: &RegistrationResponse,
    ) -> Result<VerifiedRegistration, WebAuthnError> {
        self.check_client_data(challenges, &response.client_data_json, Ceremony::Create)?;

        let att_raw = rand::b64url_decode(&response.attestation_object)
            .ok_or(WebAuthnError::AttestationEncoding)?;
        let att: ciborium::value::Value = ciborium::from_reader(att_raw.as_slice())
            .map_err(|e| WebAuthnError::AttestationCbor(e.to_string()))?;
        let map = att.as_map().ok_or(WebAuthnError::AttestationNoAuthData)?;
        let field = |name: &str| {
            map.iter()
                .find(|(k, _)| k.as_text() == Some(name))
                .map(|(_, v)| v)
        };

        let auth_data_bytes = field("authData")
            .and_then(ciborium::value::Value::as_bytes)
            .ok_or(WebAuthnError::AttestationNoAuthData)?;
        let fmt = field("fmt")
            .and_then(ciborium::value::Value::as_text)
            .ok_or(WebAuthnError::AttestationNoFmt)?;

        let data = AuthenticatorData::parse(auth_data_bytes)?;
        if !data.rp_id_matches(&self.rp_id) {
            return Err(WebAuthnError::WrongRelyingParty);
        }
        if !data.user_verified() {
            return Err(WebAuthnError::UserNotVerified);
        }
        let (Some(id), Some(key)) = (data.credential_id, data.credential_public_key) else {
            return Err(WebAuthnError::NoAttestedCredential);
        };

        // §4.3a. The chain must attest THIS credential's key, for THIS
        // ceremony — so it is verified after the key is in hand and
        // against the raw `clientDataJSON` bytes as they arrived, never
        // a re-serialisation of them (the nonce is over the bytes).
        //
        // Deliberately AFTER the challenge has been consumed by
        // `check_client_data`: a forged attestation must still burn the
        // challenge it was presented against, or a rejected attempt
        // would leave a live challenge for the next try.
        let client_data_raw = rand::b64url_decode(&response.client_data_json)
            .ok_or(WebAuthnError::ClientDataEncoding)?;
        let x5c: Vec<Vec<u8>> = field("attStmt")
            .and_then(ciborium::value::Value::as_map)
            .and_then(|stmt| {
                stmt.iter()
                    .find(|(k, _)| k.as_text() == Some("x5c"))
                    .and_then(|(_, v)| v.as_array())
            })
            .map(|certs| certs.iter().filter_map(|c| c.as_bytes().cloned()).collect())
            .unwrap_or_default();

        attestation::verify_apple(
            fmt,
            &x5c,
            auth_data_bytes,
            &client_data_raw,
            &key,
            now_unix(),
        )?;

        Ok(VerifiedRegistration {
            credential_id: rand::b64url(&id),
            public_key: key,
            sign_count: data.sign_count,
        })
    }

    /// Verify an assertion against a stored credential; returns the new
    /// signature counter to persist.
    pub fn verify_assertion(
        &self,
        challenges: &ChallengeStore,
        credential: &Credential,
        response: &AssertionResponse,
    ) -> Result<u32, WebAuthnError> {
        let client_data_raw =
            self.check_client_data(challenges, &response.client_data_json, Ceremony::Get)?;

        let auth_data_raw = rand::b64url_decode(&response.authenticator_data)
            .ok_or(WebAuthnError::AuthDataEncoding)?;
        let data = AuthenticatorData::parse(&auth_data_raw)?;
        if !data.rp_id_matches(&self.rp_id) {
            return Err(WebAuthnError::WrongRelyingParty);
        }
        if !data.user_verified() {
            return Err(WebAuthnError::UserNotVerified);
        }

        let sig_der =
            rand::b64url_decode(&response.signature).ok_or(WebAuthnError::SignatureEncoding)?;
        let signature =
            p256::ecdsa::Signature::from_der(&sig_der).map_err(|_| WebAuthnError::BadSignature)?;

        // The signed message is authenticatorData ‖ SHA-256(clientDataJSON).
        let mut signed = auth_data_raw.clone();
        signed.extend_from_slice(Sha256::digest(&client_data_raw).as_slice());

        let key = credential
            .public_key()
            .map_err(|_| WebAuthnError::Cose(cose::CoseError::NotOnCurve))?
            .verifying_key()?;
        key.verify(&signed, &signature)
            .map_err(|_| WebAuthnError::BadSignature)?;

        // §4.3's sign-count regression check, "where the platform
        // provides it". Apple's platform authenticators always report
        // 0; a stored 0 and a fresh 0 therefore mean "no counter", not
        // "replay". Only a counter that once moved and then went
        // backwards is evidence of a clone.
        if data.sign_count != 0
            && credential.sign_count != 0
            && data.sign_count <= credential.sign_count
        {
            return Err(WebAuthnError::SignCountRegression {
                got: data.sign_count,
                stored: credential.sign_count,
            });
        }
        Ok(data.sign_count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rp() -> RelyingParty {
        RelyingParty {
            rp_id: "portal.example".into(),
            rp_name: "ABERP".into(),
            origin: "https://portal.example".into(),
        }
    }

    #[test]
    fn a_challenge_is_single_use() {
        let s = ChallengeStore::new();
        let c = s.mint(Ceremony::Get, None).expect("mint");
        assert!(s.consume(&c, Ceremony::Get));
        assert!(!s.consume(&c, Ceremony::Get), "replay must fail");
    }

    #[test]
    fn a_challenge_is_bound_to_its_ceremony() {
        let s = ChallengeStore::new();
        let c = s.mint(Ceremony::Create, None).expect("mint");
        assert!(
            !s.consume(&c, Ceremony::Get),
            "cross-ceremony use must fail"
        );
        assert!(s.consume(&c, Ceremony::Create));
    }

    #[test]
    fn challenge_minting_is_capped() {
        // Behind the knock, but still unauthenticated: the ceremony
        // routes must not be an unbounded allocator on the Mac.
        //
        // The binding cap for a single caller is now the PER-SOURCE
        // one — the global 256 was a lockout weapon on its own, see
        // `MAX_CHALLENGES_PER_SOURCE`. The global bound is still
        // enforced; it is exercised by
        // `a_full_table_evicts_the_flooder_rather_than_refusing_the_operator`.
        let s = ChallengeStore::new();
        for _ in 0..MAX_CHALLENGES_PER_SOURCE {
            s.mint(Ceremony::Get, Some("203.0.113.7"))
                .expect("under the cap");
        }
        assert!(matches!(
            s.mint(Ceremony::Get, Some("203.0.113.7")),
            Err(WebAuthnError::TooManyChallenges)
        ));
        assert_eq!(s.outstanding(), MAX_CHALLENGES_PER_SOURCE);
    }

    #[test]
    fn consuming_a_challenge_makes_room_again() {
        let s = ChallengeStore::new();
        let who = Some("203.0.113.7");
        let mut minted = Vec::new();
        for _ in 0..MAX_CHALLENGES_PER_SOURCE {
            minted.push(s.mint(Ceremony::Get, who).expect("under the cap"));
        }
        assert!(s.mint(Ceremony::Get, who).is_err());
        assert!(s.consume(&minted[0], Ceremony::Get));
        s.mint(Ceremony::Get, who).expect("room after a consume");
    }

    #[test]
    fn an_unknown_challenge_is_refused() {
        let s = ChallengeStore::new();
        assert!(!s.consume("never-minted", Ceremony::Get));
    }

    #[test]
    fn creation_options_demand_user_verification_and_es256_only() {
        let s = ChallengeStore::new();
        let o = rp()
            .creation_options(&s, "user-handle", &[], None)
            .expect("options");
        assert_eq!(o.authenticator_selection.user_verification, "required");
        assert_eq!(
            o.authenticator_selection.authenticator_attachment,
            "platform"
        );
        assert_eq!(o.pub_key_cred_params.len(), 1);
        assert_eq!(o.pub_key_cred_params[0].alg, cose::ES256);
        // §4.3a — `direct`, not `none`. The whole enrolment defence
        // starts by ASKING for the attestation; a regression here would
        // make `verify_apple` unreachable and silently reopen software
        // enrolment.
        assert_eq!(o.attestation, "direct");
    }

    #[test]
    fn request_options_demand_user_verification() {
        let s = ChallengeStore::new();
        let o = rp().request_options(&s, &[], None).expect("options");
        assert_eq!(o.user_verification, "required");
        assert_eq!(o.rp_id, "portal.example");
    }

    #[test]
    fn one_source_cannot_exhaust_the_challenge_table() {
        // The operator-lockout this cap exists to prevent: before it,
        // anyone with the knock could fill the global table and the
        // next caller refused would be Ervin.
        let s = ChallengeStore::new();
        for i in 0..MAX_CHALLENGES_PER_SOURCE {
            s.mint(Ceremony::Get, Some("203.0.113.7"))
                .unwrap_or_else(|e| panic!("mint {i}: {e}"));
        }
        assert!(
            matches!(
                s.mint(Ceremony::Get, Some("203.0.113.7")),
                Err(WebAuthnError::TooManyChallenges)
            ),
            "a flooding source was not capped"
        );
        // …and the operator, on a different address, is unaffected.
        assert!(s.mint(Ceremony::Get, Some("198.51.100.4")).is_ok());
    }

    #[test]
    fn a_sourceless_caller_shares_one_budget_rather_than_escaping_the_cap() {
        // `None` must be a bucket, not an exemption — otherwise a relay
        // that simply omitted `peer` would turn the cap off.
        let s = ChallengeStore::new();
        for _ in 0..MAX_CHALLENGES_PER_SOURCE {
            s.mint(Ceremony::Get, None).expect("mint");
        }
        assert!(matches!(
            s.mint(Ceremony::Get, None),
            Err(WebAuthnError::TooManyChallenges)
        ));
    }

    #[test]
    fn a_full_table_evicts_the_flooder_rather_than_refusing_the_operator() {
        // The fail-open direction. A distributed flood keeps every
        // source under its own cap, so without eviction the global cap
        // would hand the attacker the lockout anyway.
        let s = ChallengeStore::new();
        let mut source = 0usize;
        while s.outstanding() < MAX_OUTSTANDING_CHALLENGES {
            let who = format!("198.51.100.{}", source % 200);
            if s.mint(Ceremony::Get, Some(&who)).is_err() {
                source += 1;
                continue;
            }
            source += 1;
        }
        assert_eq!(s.outstanding(), MAX_OUTSTANDING_CHALLENGES);

        // Ervin arrives at a completely full table and is served.
        let ervin = s
            .mint(Ceremony::Get, Some("203.0.113.9"))
            .expect("the operator was refused at a full table");
        assert!(
            s.consume(&ervin, Ceremony::Get),
            "the operator's own challenge was the one evicted"
        );
        assert!(
            s.outstanding() <= MAX_OUTSTANDING_CHALLENGES,
            "the table grew past its bound"
        );
    }

    #[test]
    fn client_data_from_a_lookalike_origin_is_refused() {
        let s = ChallengeStore::new();
        let challenge = s.mint(Ceremony::Get, None).expect("mint");
        let cd = serde_json::json!({
            "type": "webauthn.get",
            "challenge": challenge,
            "origin": "https://portal.example.evil.test",
        });
        let encoded = rand::b64url(cd.to_string().as_bytes());
        let err = rp()
            .check_client_data(&s, &encoded, Ceremony::Get)
            .expect_err("must refuse");
        assert!(matches!(err, WebAuthnError::WrongOrigin { .. }));
        // And the legitimate challenge must still be outstanding — a
        // phishing attempt must not be able to burn Ervin's nonce.
        assert!(s.consume(&challenge, Ceremony::Get));
    }

    #[test]
    fn client_data_with_the_wrong_ceremony_type_is_refused() {
        let s = ChallengeStore::new();
        let challenge = s.mint(Ceremony::Get, None).expect("mint");
        let cd = serde_json::json!({
            "type": "webauthn.create",
            "challenge": challenge,
            "origin": "https://portal.example",
        });
        let encoded = rand::b64url(cd.to_string().as_bytes());
        assert!(matches!(
            rp().check_client_data(&s, &encoded, Ceremony::Get),
            Err(WebAuthnError::WrongCeremonyType { .. })
        ));
    }

    #[test]
    fn client_data_that_is_not_base64url_is_refused() {
        let s = ChallengeStore::new();
        assert!(matches!(
            rp().check_client_data(&s, "###", Ceremony::Get),
            Err(WebAuthnError::ClientDataEncoding)
        ));
    }
}
