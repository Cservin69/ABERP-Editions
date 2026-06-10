# ADR-0070 — DigitalIdProvider: trait + mock-first abstraction for operator digital identity. Day-1 foundation of the defense-aerospace pivot.

- **Status:** Proposed
- **Date:** 2026-06-10
- **Deciders:** Ervin (via S344 / PR-38 defense-pivot Day-1 brief)
- **Supersedes:** none — first ADR of the digital-identity / electronic-signature strand.
- **Related:** ADR-0008 (audit ledger — the eventual consumer of the signer attestation), the defense-aerospace gap analysis (S330, `[[defense-aerospace-pivot]]`), and `[[mock-everything-principle]]`, `[[trust-code-not-operator]]`, `[[hulye-biztos]]`.

## Context

The aerospace/defense pivot gap analysis (S330) named three structural must-builds; the first is **operator identity + electronic signature** to Part-11 / DFARS grade. Every audit-emitting operation in ABERP must eventually be able to attest *who* — under a verified digital identity (a US DoD CAC certificate, an HU eID assertion, a Qatar MFA factor, a national-eID-signed token) — authorised each fiscal/manufacturing action, not merely *what* changed.

We do not yet have a target customer, and therefore do not yet know which identity backend ships first. Wiring any single vendor into the audit emit sites now would be the canonical CLAUDE.md #2 / #13 trap: speculative coupling to a surface we cannot yet specify.

The proven ABERP pattern for exactly this shape is **abstraction-then-implementations**: `StorefrontCredentialHandle`, the email-outbox queue, the MES `Adapter` trait — each defines a seam first and lets concrete backends land behind it. Identity is the same problem.

## Decision

**A new focused crate `crates/aberp-digital-id` defining a single `DigitalIdProvider` trait, plus one deterministic, explicitly-non-production `MockProvider`. The provider is constructed once at boot into `AppState`, held as `Arc<dyn DigitalIdProvider>`, and swapped via the (future) `ABERP_DIGITAL_ID_PROVIDER` env var. Real backends are out of scope for S344 and slot in behind the same trait per customer demand.**

### 1. The trait

```rust
pub trait DigitalIdProvider: Send + Sync {
    fn name(&self) -> &str;
    fn current_operator(&self) -> Result<DigitalId, ProviderError>;
    fn sign(&self, payload: &[u8]) -> Result<Signature, ProviderError>;
    fn verify(&self, payload: &[u8], sig: &Signature) -> Result<bool, ProviderError>;
}
```

`Send + Sync` so one `Arc<dyn DigitalIdProvider>` clones into every axum handler + daemon, exactly like the other shared `AppState` handles. `DigitalId` carries `{ id, display_name, issuer, scope, issued_at_ms }`; `Signature` carries `{ algorithm, bytes, signer_id, signed_at_ms }`. The `algorithm` tag is load-bearing — a verifier checks it before recomputing, so a `mock-hmac-sha256` signature can never be silently accepted by a future `ecdsa-p256` verifier.

### 2. The mock is NOT production crypto

`MockProvider` "signs" with a **hand-rolled HMAC-SHA256** keyed on a hardcoded, publicly-known constant (`b"MOCK_TEST_KEY_NEVER_USE_IN_PROD"`). It proves the sign/verify *shape*, nothing more. Three guardrails make it impossible to ship silently:

1. It logs `DigitalIdProvider: MOCK — NOT FOR PRODUCTION USE` at **WARN** on every construction.
2. Its identity is a fixed stub (`mock-op-001` / "Mock Operator" / issuer `mock`) with a pinned timestamp — deterministic, obviously not a real person.
3. The HMAC is hand-rolled in-tree (over the already-present `sha2`) rather than pulled from the `hmac` crate, precisely so the "this is not real crypto" boundary is unmistakable and no production supply-chain edge is created for a throwaway backend.

`verify()` recomputes and compares with a constant-time byte comparison; a foreign `algorithm` tag is rejected before recomputation.

### 3. Boot wiring & the swap point

`build_digital_id_provider()` in `serve.rs` is the single construction site. Today it always returns the mock and emits one INFO line (`DigitalIdProvider configured: provider=mock, operator=Mock Operator (mock-op-001)`). The `ABERP_DIGITAL_ID_PROVIDER` env var is the **future** swap point: any value other than `mock` currently logs a WARN and falls back to the mock, so a half-configured deployment is loud, never silently unsigned. Real backends register here.

### 4. Audit-ledger foundation, not wiring

`audit-ledger` gains an OPTIONAL `Signed<T> { payload: T, signer: Option<DigitalIdRef> }` wrapper + a plain `DigitalIdRef` DTO (`crates/audit-ledger/src/signer.rs`). **No existing event uses it and no new `EventKind` is added in S344** — so the F12 four-edit ritual does not fire. The wrapper exists so a future event (S346) opts in by populating `signer`; legacy/unsigned events serialize unchanged (`#[serde(default)]` hydrates absent `signer` to `None`, pinned by a legacy-bytes round-trip test). `DigitalIdRef` deliberately does **not** depend on `aberp-digital-id` — `audit-ledger` is a low-level crate, and the binary layer that owns both projects the identity down into the DTO, keeping the dependency direction clean.

## Consequences

**Positive.** The seam exists and is proven by tests; downstream sessions add real providers + audit opt-in without re-litigating the shape. No vendor lock-in chosen prematurely. The mock gives SPA/audit work a stable identity to develop against immediately.

**Negative / deferred.** No real identity is enforced yet — the mock is a placeholder, and an operator could in principle run prod on it (mitigated only by the loud WARN; a hard prod-build refusal is future work, like the NAV-credentials sanity check). Electronic-signature *ceremony* (UI, re-authentication on signing, certificate revocation checking) is entirely future. The `Signed<T>` wrapper is unused until S346 wires the first event.

**Future work (not S344):** real backends (HU eID, US DoD CAC, Qatar MFA) per customer demand; audit `EventKind`s populating `signer` (S346); a prod-build guard that refuses to boot on the mock; electronic-signature ceremony + certificate lifecycle.
