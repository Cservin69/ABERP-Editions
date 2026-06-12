# ADR-0080 — A second `DigitalIdProvider` (US-DoD-CAC stub) as pattern-proof for the trait abstraction.

- **Status:** Proposed
- **Date:** 2026-06-12
- **Deciders:** Ervin (via S363 / PR-50 defense-pivot batch 8/10 brief)
- **Supersedes:** none.
- **Related:** ADR-0070 (S344 — the `DigitalIdProvider` trait + `MockProvider`, the foundation this validates), ADR-0072 (S354 operator accept-on-behalf, the audit/identity strand), the defense-aerospace gap analysis (S330, `[[defense-aerospace-pivot]]`), and `[[mock-everything-principle]]`.

## Context

ADR-0070 introduced `DigitalIdProvider` as the swap-point seam for operator digital identity, shipping exactly one implementation: the deterministic, non-production `MockProvider` (a hand-rolled HMAC-SHA256 over a publicly-known test key). A single implementation cannot demonstrate that a trait *abstracts* — with only one impl behind it, a trait can silently calcify around that impl's incidental shape. The mock's shape is HMAC-flavoured in three ways that a real backend would not share:

1. **One static operator.** `current_operator()` always succeeds — there is no "no operator" path exercised, so the `ProviderError::NoCurrentOperator` arm has no producer.
2. **Keyed-MAC signing.** `sign()` is an HMAC over a fixed key; the signature carries no notion of *who is trusted*.
3. **MAC-equality verification.** `verify()` recomputes the HMAC and constant-time-compares. Trust is implicit in possession of the key.

Real identity backends named in ADR-0070 (US DoD CAC, HU eID, Qatar MFA) are certificate / assertion / factor based and differ on all three axes. If the trait only ever saw the mock, we would not learn — until a real backend lands, expensively, against a paying customer — whether the seam actually holds.

`[[mock-everything-principle]]` says: prove the seam with mocks now, wire real backends only on customer demand. The missing step is a *second* mock with a deliberately different shape.

## Decision

**Add a second deterministic, explicitly-non-production backend — `UsDodCacProvider` — to `crates/aberp-digital-id`, behind the unchanged `DigitalIdProvider` trait, reachable via the existing `ABERP_DIGITAL_ID_PROVIDER` env-var dispatch in `serve.rs`. Its purpose is solely to validate the abstraction; it implements no real cryptography and is selected only when explicitly requested. The default stays `mock`, so production behaviour is unchanged.**

The CAC stub is chosen (over the HU-eID / Qatar-MFA personas) because it differs from the mock on all three axes at once, giving the strongest abstraction signal per line of code:

### 1. Different signing persona — certificate-bound digest, not keyed HMAC

`sign()` produces a `stub-ecdsa-p256-cac`-tagged signature whose bytes are `SHA-256(signer_id ‖ 0x00 ‖ payload)` — a plain, **un-keyed** digest folding in the card's EDIPI. This is visibly a different (and visibly fake) construction from the mock's keyed HMAC: a different operator yields a different signature over the same payload, modelling a certificate-bound signature without any key material. The distinct algorithm tag means a CAC signature can never be recomputed under the mock's verifier, or vice versa.

### 2. Session-based `current_operator()`, not static

The provider holds an `Option<CacSession>` — `Some` when a card is inserted, `None` once ejected (`UsDodCacProvider::ejected()`). With no card, `current_operator()`, `sign()`, and `verify()` all surface `ProviderError::NoCurrentOperator`. This finally gives the trait's error arm a real producer and proves the boot wiring tolerates a fallible operator lookup (it already logs a WARN on `current_operator()` error, untouched).

### 3. Cert-chain-membership verification, not MAC equality

`verify()` first checks the claimed `signer_id` is present in the reader's trusted chain (a real reader trusts the DoD PKI path, not the payload), and only then recomputes the stub digest and constant-time-compares. A signature that is internally self-consistent but whose signer is *absent from the chain* is rejected — a rejection the mock's pure-HMAC verify cannot even express. The constant-time comparator is **reused** from the mock module rather than duplicated (CLAUDE.md #8/#13).

### Reachability & default

`build_digital_id_provider()` in `serve.rs` becomes a `match` on the env value: `mock` (default + fallback), `us-dod-cac`, and any unknown value → loud WARN + mock fallback. **No callsite that consumes `AppState.digital_id` changes** — the entire selection is one `match` arm at the single boot construction site. That invariant is the thing this ADR set out to prove.

## Consequences

**Positive.** The trait is now demonstrated multi-impl-clean: a second backend with a fundamentally different signing/session/verification shape slots in behind it with zero churn to the trait, the `DigitalId`/`Signature` types, or any consumer. The `NoCurrentOperator` arm is exercised. A future real backend (real CAC, HU eID, Qatar MFA) inherits a seam that has been shown to flex, not one shaped around a single mock. Mock-everything posture is intact — no real crypto, no supply-chain edge added.

**Negative / deferred.** The CAC stub's "ECDSA" is a SHA-256 stand-in — it is NOT cryptographically meaningful and, like the mock, must never back a production identity (guarded only by the loud WARN-on-construct; a hard prod-build refusal remains future work per ADR-0070). Real PKI-path validation, certificate revocation, and the electronic-signature ceremony are out of scope. No `EventKind` is added — sign/verify audit coverage already exists via `personnel.signature_applied` (S355). Storage of issued/revoked credentials and a provider-selection UI are separate sessions.

**Future work (not S363):** real cryptographic backends per customer demand; a prod-build guard that refuses to boot on any stub provider; credential lifecycle storage; provider-selection UI.
