# Apple WebAuthn Root CA — provenance

`Apple_WebAuthn_Root_CA.pem` is the trust anchor
`crates/aberp-portal-agent/src/webauthn/attestation.rs` verifies Apple
platform-authenticator attestations against (ADR-0115 §4.3a).

| | |
|---|---|
| Source | <https://www.apple.com/certificateauthority/Apple_WebAuthn_Root_CA.pem> |
| Fetched | 2026-08-21 |
| Subject / Issuer | `CN=Apple WebAuthn Root CA, O=Apple Inc., ST=California` (self-signed) |
| Valid | 2020-03-18 → 2045-03-15 |
| Key | ECDSA P-384 |
| SHA-256 fingerprint | `0915DD5C07A28DB549D1F677BB5A75D4BFBE9561A773424327762E9E02F9BB29` |

## Why it is vendored rather than fetched

A trust anchor fetched at runtime is not a trust anchor: whoever can
answer the fetch chooses what the agent trusts, and this agent's entire
enrolment defence rests on this one certificate. Vendoring makes the
anchor part of the reviewed artifact — changing it is a diff.

It is also not fetched at *build* time, which would make a green build
depend on Apple's web server being reachable and would put an
unreviewed certificate into a binary.

## Why it lives in `roots/` and not `assets/`

Because `.gitignore` has a blanket `*.pem` secret guard, and the only
exception is `!crates/*/roots/*.pem`. This file spent one round in
`crates/aberp-portal-agent/assets/`, where that exception did not reach
it — so it was never committed, and the branch did **not** build from a
clean checkout: `attestation.rs` includes it at compile time. The
directory name is load-bearing. Vendored public trust anchors go in
`crates/<crate>/roots/`, which is also where ADR-0020's NAV anchor
lives. `tests/vendored_anchor_is_committed.rs` now asserts the file is
tracked, so the next one cannot be swallowed silently.

## Verifying this copy

```sh
curl -fsS https://www.apple.com/certificateauthority/Apple_WebAuthn_Root_CA.pem \
  | openssl x509 -noout -fingerprint -sha256
# must print the fingerprint in the table above
```

`attestation.rs` re-asserts that fingerprint at test time, so a
substituted file fails the build rather than silently widening trust.

## Expiry

2045. Long past anything this repository plans for, but the parser
checks validity dates on every certificate in the chain including this
one, so an expired anchor fails closed — it does not silently stop
being checked.
