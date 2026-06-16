# ADR-0086 — DÁP eAzonosítás operator-login identity flow.

- **Status:** Proposed
- **Date:** 2026-06-16
- **Deciders:** Ervin (decisions locked 2026-06-16 13:57 UTC — **Path A**, timestamp-anchored audit chain; per-login signing ceremony acceptable; no prior QTSP account; szeusz.gov.hu org registration in progress).
- **Implements:** the first real backend behind the `DigitalIdProvider` trait scaffolded in ADR-0070 (which shipped only `MockProvider`). This ADR designs the Hungarian **DÁP eAzonosítás** identity face; ADR-0087 designs the audit-chain signing/anchoring that consumes the identity it establishes; ADR-0088 designs unattended-daemon writes.
- **Related:** ADR-0070 (`DigitalIdProvider` trait + `Signed<T>` foundation), ADR-0080 (second digital-id provider — the trait already proven swappable), ADR-0008 (audit ledger — the consumer), the DÁP research findings (`docs/findings/dap-research-2026-06-16.md`), `[[trust-code-not-operator]]`, `[[hulye-biztos]]`, `[[no-ask-user-question]]`, `[[defense-aerospace-pivot]]`.

## Context

ABERP's Defense line targets HU operators. From 2026 the **DÁP mobile app is the only government login** for HU citizens (Ügyfélkapu EOL 31 Dec 2025). Every Defense operator already holds DÁP. The audit chain (ADR-0087) needs a court-admissible answer to *who authorised this session* — and the only nationally-recognised answer for an HU operator is a DÁP-attested identity.

The research (§1) established the developer surface, with several facts that constrain the design (verified, not assumed):

1. **DÁP has two distinct faces.** `eAzonosítás` (identity / wallet eID) and `eAláírás` (a genuine personal QES). **We integrate eAzonosítás only.** `eAláírás` is **personal-use-only and PDF-document-bound** (upload a PDF, enter a signing password) — it cannot cleanly sign our arbitrary session-key endorsement blob, so it is *not* the primitive for binding a session key. The eIDAS-grade per-session anchor in Path A is a NETLOCK **qualified timestamp** (ADR-0087), not DÁP eAláírás. (See ADR-0087 for why a timestamp anchor, not a per-session QES, is sufficient for the targeted legal bar.)
2. **DÁP eAzonosítás is an OpenID4VP-style verifiable-credentials flow** — RP builds a request object, delivers it as a **QR code / deep link**, the citizen approves in the DÁP app (PIN/biometric-gated), the RP validates the signed presentation. The PID claim set (surname, given name, place + date of birth, issuing authority/country, expiry, citizenship) is human-readable; the machine claim names and credential format (mdoc vs SD-JWT-VC) are **not public** — gated behind KAÜ login on szeusz.gov.hu.
3. **The federated IdP will not mint our binding.** OIDC/OpenID4VP never lets a client dictate a claim *value*; the IdP exposes a fixed claim set. So the session-key→identity binding cannot live inside the DÁP attestation. ADR-0087 owns that binding (qualified timestamp over `operator_dap_subject || … || session_pubkey`); the cheap transitive binding (`hash(session_pubkey‖tenant)` in the request `nonce`) is an optional belt-and-suspenders, kept behind the trait.
4. **The exact protocol surface is not yet public.** szeusz.gov.hu org registration is in progress; KAÜ is SAML-lineage but the new DÁP wallet pushes OpenID4VP 1.0 + DCQL (a 2026 non-backward-compatible standard change). The ADR therefore commits to an **abstraction, not a wire format**: the protocol confirms on RP registration, and swapping it is a single-impl change behind `DigitalIdProvider`.

## Decision

**Add one real `DigitalIdProvider` implementation — `DapProvider` — that performs DÁP eAzonosítás once per login via an OpenID4VP-style mobile-wallet flow, with a loopback callback, a sandbox/production env switch, and a code-enforced local-admin fallback when DÁP is unreachable. Four new `auth.*` EventKinds record the login lifecycle (count 148 → 152). The wire protocol stays behind the trait so the szeusz.gov.hu-confirmed final protocol is a single-impl swap.**

### 1. Which DÁP face, and the abstraction boundary

`DapProvider` wraps **eAzonosítás** for identity only. It implements the existing trait:

```rust
impl DigitalIdProvider for DapProvider {
    fn name(&self) -> &str { "dap-eazonositas" }
    fn current_operator(&self) -> Result<DigitalId, ProviderError>; // last completed login
    fn sign(&self, payload: &[u8]) -> Result<Signature, ProviderError>; // see note
    fn verify(&self, payload: &[u8], sig: &Signature) -> Result<bool, ProviderError>;
}
```

`current_operator()` returns the PID-derived `DigitalId { id, display_name, issuer, scope, issued_at_ms }`, where `id` is the stable DÁP subject identifier (`operator_dap_subject`, the natural-person id from the presentation), `issuer = "DAP"`, `display_name` from PID surname+given name.

**`sign()`/`verify()` on `DapProvider` do NOT call DÁP.** DÁP eAzonosítás authenticates identity; it is not an arbitrary-bytes signing oracle (fact #1). The session-key signer that signs audit entries is a *separate* object owned by ADR-0087; `DapProvider::sign` delegates to that session key (the key endorsed at this provider's login) so the trait contract holds. This keeps ADR-0070's `algorithm`-tag discipline: the returned `Signature.algorithm` is `ed25519-session`, never a fake DÁP signature.

The **protocol** (OpenID4VP request-object construction, presentation validation, claim parsing) lives behind a private `DapTransport` seam inside the impl. The ADR assumes **OpenID4VP 1.0 + DCQL** as the target profile (the 2026 DÁP direction). **TO-BE-CONFIRMED on szeusz.gov.hu RP registration:** exact endpoint paths, scope/claim identifiers, credential format (mdoc vs SD-JWT-VC), redirect-URI rules, and whether KAÜ also exposes classic OIDC with a client-controllable `nonce`. None of these leak past `DapTransport`; confirming them is a single-impl edit, not a re-architecture.

### 2. Callback strategy — loopback, ephemeral port ([[trust-code-not-operator]], [[hulye-biztos]])

ABERP is a **desktop app**, not a web app — there is no public redirect URI. The login flow uses an **OS-assigned loopback listener**:

- At login start, bind `TcpListener` on `127.0.0.1:0` (OS picks a free ephemeral port — ASCII-safe, no operator port config). Read the actual port; build the callback `http://127.0.0.1:<port>/auth/dap/callback`. **Bind before** initiating the auth request (the redirect target must be live first).
- The request object (QR / deep link) carries that callback. The operator approves in the DÁP app; the wallet/agent redirects to the loopback; the listener captures the response, validates it, then **closes immediately** (one-shot; the listener lives only for the duration of one login).
- **OS-firewall friendliness:** loopback-only binds never trigger the macOS "accept incoming connections" prompt (it fires for non-loopback binds). No operator firewall gymnastics — the design requirement of `[[hulye-biztos]]`. This mirrors the standard native-app OAuth loopback pattern (RFC 8252 §7.3).

Loopback is the OAuth-for-native-apps norm and is **per-login disposable**: a fresh port each login, the listener never outlives the callback, nothing is left listening.

### 3. Sandbox vs production swap

A single env var **`DAP_ENV`** selects the endpoint set: `sandbox` | `production`. **Default is `production` on Defense builds** (the Defense launcher, `run/run_defense.sh`, sets it; a Portable/demo build never constructs `DapProvider` at all — it runs the mock or NAV-off path). An unrecognised `DAP_ENV` value is a **loud boot error**, never a silent fall-through to either environment (mirrors ADR-0070's `ABERP_DIGITAL_ID_PROVIDER` loud-fallback posture). The endpoint set behind each value is filled from the szeusz.gov.hu spec at RP-registration time.

### 4. Fallback when DÁP is down or the phone is unreachable

A hard external dependency on DÁP at login is a single point of failure for the shop floor. The fallback is **refuse-by-default, code-gated, audited**:

- If the DÁP flow fails (timeout, network down, wallet unreachable, presentation invalid), login is **refused** with a clear operator-facing error ("Sign-in with DÁP failed — <reason>"). No silent degradation.
- An **emergency local-admin-credential bypass** exists, but it is **refused unless** the operator's tenant-config role carries `dap_bypass_allowed = true`. The flag defaults **false**; an operator cannot self-grant it; it is set in tenant config by a tenant administrator. This is the `[[trust-code-not-operator]]` property: the bypass eligibility lives in a config-checked code path, not in operator discipline or a "are you sure?" dialog.
- A successful bypass login emits **`DapLoginFallback`** carrying `{ operator, tenant, reason, bypass_authorised_by }` — so every emergency login is a first-class, queryable audit record an auditor can find, never an invisible side door.
- The bypass still opens a normal signed session (ADR-0087): the session key is generated and the chain is signed and timestamp-anchored exactly as a DÁP login, **minus the DÁP identity attestation**. The anchor payload records `identity_source = "local_admin_fallback"` so the reduced assurance is visible in the chain itself, not just in one event.

### 5. New EventKinds (4; count 148 → 152), all `auth.*`

| Variant (`as_str`) | When it fires | Payload sketch |
|---|---|---|
| `auth.dap_login_initiated` | loopback bound, request object built, QR/deep-link shown | `{ tenant, callback_port, dap_env, flow_id }` |
| `auth.dap_login_completed` | presentation validated, operator established | `{ tenant, operator_dap_subject, flow_id, login_at_utc }` |
| `auth.dap_login_failed` | flow failed before an operator was established | `{ tenant, flow_id, error_class, dap_env }` |
| `auth.dap_login_fallback` | local-admin emergency bypass used | `{ tenant, operator, reason, bypass_authorised_by }` |

All four sit under the `auth.*` prefix (a new domain prefix, globbable for the S424 audit screen). App-layer JSON payloads only — **never NAV XML** — so each must be added to **both** NAV-leakage exhaustive arms (`aberp-verify::extract_nav_xml`, `export_invoice_bundle::extract_nav_xml`) with a `// app-only, never serialized to NAV` decision, and the count pin bumped to `152` in all three sites (the ADR-0081 ritual). The full F12 four-edit ritual (`as_str` + `from_storage_str` + `round_trip_for_every_variant` + `ALL_KINDS` + `ALL_KINDS_COUNT`) fires for each. **Note the combined count delta across the trio: +4 here, +5 in ADR-0087, +3 in ADR-0088 → 148 → 160 if all three implement.** Each ADR bumps the pin to its own running total in sequence; the implementation order sets which pin value lands first.

### 6. Operator UX ([[hulye-biztos]])

One button: **"Sign in with DÁP."** Tap → a QR code (and a deep-link button if the DÁP app is on the same device) → the operator approves in the DÁP app (PIN/biometric) → the dashboard. No protocol awareness, no port config, no certificate handling, no firewall prompt. The ~10-second QR-scan ceremony is the locked-acceptable cost. If DÁP fails and the operator is bypass-eligible, a clearly-labelled **"Emergency local admin login"** secondary path appears; otherwise it does not render at all.

## Acceptance criteria (for the implementation session)

1. `DapProvider` implements `DigitalIdProvider`; constructed at boot behind `ABERP_DIGITAL_ID_PROVIDER=dap` (or the Defense launcher default), with `MockProvider` still the default elsewhere. A misconfigured `DAP_ENV` is a loud boot error.
2. The loopback listener binds `127.0.0.1:0`, captures exactly one callback, and is dropped (port released) before login returns — proven by a test asserting the port is unbound afterward.
3. The four `auth.*` EventKinds pass the full F12 ritual; `ALL_KINDS_COUNT` and both NAV-leakage drift assertions read the new running total; both NAV arms classify all four as app-only.
4. A DÁP-flow failure refuses login and fires `auth.dap_login_failed`; it never opens a session.
5. The fallback path is reachable **only** when `dap_bypass_allowed = true`; a test proves a non-eligible operator is refused, and that a successful bypass fires `auth.dap_login_fallback` and opens a session tagged `identity_source = "local_admin_fallback"`.
6. `DapTransport`'s protocol details (endpoints, claim names, credential format, profile) are isolated such that swapping OpenID4VP↔SAML↔confirmed-DÁP-profile touches one module and no EventKind/trait/call-site.

## Consequences

- **Positive:** the trait's first real backend; the szeusz.gov.hu protocol uncertainty is quarantined behind one seam; the fallback removes DÁP as a hard shop-floor SPOF without weakening the audit trail (every bypass is recorded and assurance-tagged); zero schema change here (this ADR is identity-flow only; ADR-0087 owns the schema).
- **Deferred / TO-BE-CONFIRMED:** exact DÁP wire protocol, endpoints, claim identifiers, and credential format — pending RP registration. The ADR is protocol-flexible by construction; these fill in without re-litigation.
- **Out of scope:** DÁP eAláírás (personal QES) — explicitly not integrated (fact #1). The qualified anchor is NETLOCK timestamp (ADR-0087). A future per-session personal-QES tier (research Candidate B) can slot behind the same trait if a defense contract demands personal non-repudiation, but it is not this design.
