# ADR-0053 — NAV-as-DR RESTORE confirmation token: server-side enforcement, not SPA-only ceremony

**Status:** Accepted (implicit; documented retroactively in S198 / PR-198,
2026-05-31). De facto pinned by PR-186 / S186, which moved the literal
`"RESTORE"` gate from the SPA wizard's `isRestoreConfirmed` JS check into
the backend route's required `confirm_token` field.
**Author:** Ervin Áben (ABERP), session 198 brief — close the 💭 question
raised by the S172-S181 adversarial review.
**Supersedes / amends:** none — pins a posture the original S180 PR left
ambiguous (the route's doc-comment did not name whether the ceremony was
SPA-only or load-bearing).
**Related:** ADR-0007 (operator-as-threat-actor + server-stamps-immutable
posture), ADR-0034 (recover-from-NAV — CLI-only operator command, no SPA
ceremony), the NAV-as-DR restore wizard (S180), the SPA RESTORE-token gate
(`apps/aberp-ui/ui/src/lib/restore-wizard.ts`).

## Context

The S180 NAV-as-DR restore wizard introduced an operator-typed `"RESTORE"`
confirmation token as a SPA-side ceremony (`restore-wizard.ts:67` —
`isRestoreConfirmed = typedConfirm.trim().toUpperCase() === "RESTORE"`).
The intent was to make the wizard's "this will mirror an entire year of NAV
invoices into a new local table" feel as deliberate as a real-money issuance
— operators tend to muscle-memory-click through "are you sure?" dialogs, but
typing a five-letter literal is intrinsically deliberate.

The session 182 adversarial review observed that the backend route (`POST
/api/restore-from-nav-outgoing`) gated only on `require_ready` + bearer +
year-bounds. The literal token check lived ONLY in the SPA. A buggy SPA
build, a malicious browser extension, or a `curl` request with a valid bearer
could POST the route with no token. Blast radius was limited (the path is
idempotent, NAV is read-only, restored rows land in a separate
`restored_invoice` table that is segregated from the canonical `invoice`
table by ADR-0019's no-foreign-keys + named-table convention), so the
review framed it as "the ceremony is cosmetic, not a security gap" — but
also flagged that the route's doc-comment did not name it as cosmetic.

The 💭 question: should the ceremony move server-side, or is the SPA gate
the intended layer? If the latter, the route's doc-comment should name the
SPA-side gate explicitly so a future maintainer reading only the backend
does not assume the SPA gate is load-bearing when the backend does not
enforce it.

## Decision

**The ceremony is server-side.** The literal `"RESTORE"` token is a REQUIRED
field on the request body, validated by the backend before the route's
business logic runs.

Concretely (per PR-186 / S186):

1. `RestoreFromNavOutgoingRequest` carries a REQUIRED `confirm_token:
   String` field. Missing field is a 400 from axum's `Json<T>` extractor
   (serde-required).
2. The handler equality-checks the body against
   `RESTORE_CONFIRMATION_TOKEN: &str = "RESTORE"` BEFORE the year-validation
   gate fires (so a tokenless request never reaches the NAV pipeline).
   Mismatch returns a 400 with a body explaining the contract.
3. The SPA wizard's `restoreFromNavOutgoing(year)` API became
   `restoreFromNavOutgoing(year, confirmToken)` and forwards the operator-
   typed token verbatim. The wizard's `canSubmit` gate guarantees the token
   equals `"RESTORE"` by the time the fetch fires — so the typical operator
   path is unchanged.
4. The exact-match contract is uppercase-only, no surrounding whitespace
   (the SPA's `.trim().toUpperCase()` happens before the body is built, so
   the literal sent to the backend is already normalized). The backend does
   NOT re-normalize — it checks for byte-exact `"RESTORE"`. This is
   deliberate: a request body that already trims/uppercases is one the
   operator's discipline already touched; a request body that doesn't is a
   non-SPA caller that needs to send the literal correctly.

### Why not SPA-only?

The SPA gate has three failure modes a backend gate doesn't:

1. **Buggy SPA build.** A future SPA refactor that accidentally bypasses the
   `canSubmit` check (e.g., a Svelte store wiring bug that always returns
   `true`) would let a misclick reach the backend with no token. The backend
   would then process the request — restoring an entire year of NAV
   invoices that the operator only meant to preview.
2. **Browser extension or third-party automation.** A clipboard-grabber
   extension, a screen-reader's auto-fill, or a Tauri-IPC fuzzer could
   bypass the JS gate without the operator noticing. The bearer token is in
   localStorage by ADR-0007's loopback-HTTPS posture; an extension with
   `tabs` permission can read it.
3. **`curl` / `aberp` CLI / future automation.** Any non-SPA caller (an
   operator's troubleshooting `curl`, a future `aberp restore-from-nav`
   CLI, a backup-restore script) needs to know whether the ceremony is
   load-bearing. SPA-only means the contract is invisible to non-SPA
   callers; server-side means the contract is in the request schema, which
   is the canonical place for it.

### Why a literal token, not a per-request nonce?

A per-request nonce (server-issued, single-use, time-bound) would defend
against replay attacks. The threat model here is operator-as-threat-actor
(per ADR-0007 §"Operator-as-threat-actor"), not network-attacker — the
loopback HTTPS listener with bearer auth already defends against external
replay. The literal token defends against the threat the SPA ceremony was
built for: operator-typing-something-deliberate. A per-request nonce is
invisible to the operator (the SPA injects it transparently) and would
defeat the ceremony's purpose.

### Cosmetic vs load-bearing — pinned

Per this ADR, the SPA ceremony is the **operator-facing** layer of a
**server-enforced** contract. The SPA gate is operator UX (the typing
ceremony makes the operator deliberate); the server gate is the security
contract (no caller can bypass it). Both layers exist; neither is
redundant; neither is cosmetic in isolation.

## Consequences

### Wins

- The contract is in the request schema (serde-required field), which is the
  canonical place for non-SPA callers to discover it.
- Bypassing the ceremony requires either (a) sending the literal `"RESTORE"`
  in the request body (which is itself a deliberate act) or (b) a backend
  bug that disables the check (which would be caught by
  `restore_request_serde_requires_confirm_token` and
  `restore_confirm_token_literal_is_exact_match_uppercase_restore` failing).
- The SPA ceremony's UX value (operator types a deliberate token) is
  preserved without making the SPA gate load-bearing for security.

### Trade-offs

- A non-SPA caller (a future `aberp restore-from-nav` CLI, a backup-restore
  script) MUST send the literal `"RESTORE"` in the request body. This is
  an additive cost on every non-SPA caller, but exactly the cost the
  ceremony is designed to impose.
- The literal is hardcoded; a future re-wording (e.g., to a tenant-specific
  literal like the tenant's tax number) would need to land in both the SPA
  and the backend simultaneously. No current trigger to do this; flagged
  in §"Future" below.

### When to revisit

- A second DR-style route lands (e.g., NAV-as-DR INBOUND restore). The
  RESTORE-token pattern generalizes; consider extracting a `DangerousRoute`
  middleware that gates on a per-route literal. Until the second route
  lands, the bespoke check is the minimum surgical pattern.
- An operator reports that the SPA ceremony is friction without value
  (extremely unlikely — the ceremony exists by operator request). If it
  ever happens, the SPA gate is removable independently of this ADR (this
  ADR pins the server gate, not the SPA gate).

## Adversarial review

- *"What if a non-SPA caller hits this and gets a confusing 400?"* The
  400's body names the contract: "restore requires `confirm_token` field
  equal to the literal `RESTORE` (case-sensitive, no surrounding
  whitespace)". A non-SPA caller hitting this once learns the contract;
  the error is self-documenting.
- *"What if the operator types lowercase in the SPA?"* The SPA's
  `isRestoreConfirmed` is uppercase-only — the Submit button stays
  disabled. The operator never gets to the request-body construction with
  a lowercase token. The backend's case-sensitive check is the second line
  of defence.
- *"What if a future SPA refactor accidentally sends an empty string?"*
  `restore_request_serde_requires_confirm_token` pins that the field MUST
  be present in the JSON. An empty string would deserialize to
  `String::new()` and fail the equality check — second-line-of-defence
  again. Both pin tests live in `serve.rs`.

## Alternatives considered

- **SPA-only ceremony (status quo before PR-186).** Rejected per the three
  failure modes in §"Why not SPA-only?".
- **Per-request nonce (server-issued, time-bound).** Rejected per §"Why a
  literal token, not a per-request nonce?".
- **Tenant-specific literal (operator types the tenant's tax number).**
  Considered + rejected as premature: the literal `"RESTORE"` is the
  smallest token that achieves the ceremony's purpose. A tenant-specific
  literal adds a second layer of "did the operator know which tenant they
  were restoring" but the current single-tenant-per-process model
  (ADR-0002) makes this redundant.
- **Move the literal to a `dangerous_route_config.toml`.** Considered +
  rejected: a config-file literal can drift between SPA and backend; a
  hardcoded literal in both code paths is the strictest pin (a refactor
  that changes one MUST change the other or the test
  `restore_confirm_token_literal_is_exact_match_uppercase_restore` fails).

## Invariants pinned by test

- `restore_request_serde_requires_confirm_token` — confirms the field is
  serde-required (missing field → 400 from axum's extractor).
- `restore_confirm_token_literal_is_exact_match_uppercase_restore` — pins
  the literal value at byte-exact `"RESTORE"`. A drift in either the SPA's
  `isRestoreConfirmed` check or the backend's
  `RESTORE_CONFIRMATION_TOKEN` constant trips this test.
- The SPA-side `restore-wizard.test.ts` continues to pin the wizard's
  helper surface (operator-types-`"RESTORE"` → `canSubmit` true).

## Future

A future `DangerousRoute` middleware (per §"When to revisit" — second DR
route) would lift the bespoke per-handler check into a route-attribute. The
attribute would carry the literal as a parameter, preserving the per-route
ceremony while eliminating the per-handler boilerplate. Out of scope until
the second route lands.
