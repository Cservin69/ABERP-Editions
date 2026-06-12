# ADR-0073 — `personnel.*` audit EventKind family: the defense-grade access-trail surface (identity, e-signature, access grant/deny).

- **Status:** Proposed
- **Date:** 2026-06-11
- **Deciders:** Ervin (via S355 / PR-43 defense-pivot batch session 1).
- **Supersedes:** none — first ADR of the personnel / access-trail strand.
- **Related:** ADR-0008 (audit ledger — the hash-chained home these events live in), ADR-0070 (`DigitalIdProvider` — the identity seam these events attest), ADR-0071 (`aberp-compliance` crate — now wired as a dep of `apps/aberp` by this session), the defense-aerospace gap analysis (S330, `[[defense-aerospace-pivot]]`), and `[[trust-code-not-operator]]`, `[[hulye-biztos]]`.

## Context

The aerospace/defense pivot gap analysis (S330) named three structural must-builds. ADR-0070 laid the **operator-identity seam** (`DigitalIdProvider` + the optional `Signed<T>` audit wrapper); ADR-0071 scaffolded the **compliance crate** (export-control, CUI, lot/heat, AVL, NIST SP 800-171). Both shipped as foundation — types and seams, no audit surface.

A Part-11 / DFARS-grade system must do more than record *what* a fiscal/manufacturing action changed (the existing `invoice.*` / `mes.*` families). It must record the **people facts**: that an operator's digital identity was registered, that a signature ceremony was performed against a record, and — most load-bearing for CUI / export-controlled data — that access to a controlled resource was granted or denied, by whom, and why. NIST SP 800-171 AC-3.1.1 ("limit system access to authorized users") and AU-3.3.1 ("create and retain audit records") both require this trail to be durable and reviewable, not log-file ephemera.

ABERP already has the right home for durable, tamper-evident people-facts: the ADR-0008 audit ledger. What it lacks is the **typed vocabulary** — the EventKinds — that let a forensic walker glob "every identity / signature / access decision on this install" without scraping free-text. This session adds that vocabulary; it does **not** wire firing sites (that lands in S356+, when the resources being controlled — CUI documents, export-controlled drawings — actually exist in code).

## Decision

**A new `personnel.*` EventKind prefix family with four members, plus `aberp-compliance` wired as a regular dependency of `apps/aberp`. The kinds carry free-form `serde_json::Value` payloads (the same posture every recent `quote.*` kind takes); their documented field shapes are pinned by serialization tests so S356+ firing sites have a stable contract. No firing site is added in S355.**

### 1. The four kinds

| EventKind | storage string | fires when | payload |
| --- | --- | --- | --- |
| `PersonnelIdRegistered` | `personnel.id_registered` | a `DigitalIdProvider` mints an operator identity | `{operator_user_id, provider_name, registered_at_ms}` |
| `PersonnelSignatureApplied` | `personnel.signature_applied` | an e-signature is applied to a record | `{operator_user_id, signed_record_kind, signed_record_id, signature_algorithm, signed_at_ms}` |
| `PersonnelAccessGranted` | `personnel.access_granted` | CUI / export-controlled access is granted | `{operator_user_id, resource_kind, resource_id, granted_by, reason}` |
| `PersonnelAccessDenied` | `personnel.access_denied` | such access is denied | `{operator_user_id, resource_kind, resource_id, denied_reason}` |

The grant/deny **pair** is deliberate: recording only denials (the easy, defensive half) would leave the more sensitive fact — who was *let in* to controlled data, and on whose authority — untracked. `granted_by` is the two-person-integrity anchor; `reason` / `denied_reason` are the loud-fail justifications (CLAUDE.md rule 12 — a silently-swallowed access decision is the worst-class failure for an access trail).

`signature_algorithm` on `PersonnelSignatureApplied` is load-bearing for the same reason ADR-0070 §1 pins it on `Signature`: a verifier checks the algorithm tag before recomputing, so a `mock-hmac-sha256` signature can never be silently accepted by a future `ecdsa-p256` verifier.

### 2. A new `personnel.*` prefix family

The codebase segregates audit traffic by prefix so each consumer's glob stays narrow: `invoice.*` (per-OUTGOING-invoice export bundle, ADR-0009 §8), `system.*` (lifecycle / AP-side), `mes.*` (manufacturing), `quote.*` (auto-quoting), `inventory.*`, `email.*`. Access-trail events are none of these — they are orthogonal people-facts that cut across every domain. Folding them into `system.*` would force the AP-side and lifecycle consumers to filter personnel traffic out; folding them into `invoice.*` would let the per-invoice export bundle's `invoice.*` glob sweep an access-decision row into an invoice's evidence bundle, which is wrong.

So `personnel.*` is a **seventh** prefix family. The per-invoice export bundle excludes it by construction (the glob is `invoice.*`); the two workspace exhaustive-match gates (`aberp-verify::extract_nav_xml`, `apps/aberp::export_invoice_bundle::extract_nav_xml`) classify all four kinds on the no-NAV-bytes arm — they carry app-layer JSON, never verbatim NAV XML.

### 3. Payload contract pinned in tests, not typed structs

Every recent kind (S347–S354) stores a free-form `serde_json::Value` and builds it inline at the firing site rather than through a typed `audit_payloads.rs` struct. S355 has no firing site yet, so adding typed payload structs now would be unused speculative surface (CLAUDE.md #2 / #13). Instead the documented field shape of each payload is pinned by a `*_payload_serializes` test that round-trips a sample through serde and asserts the documented fields are present with the documented JSON types. The contract lives in-repo and is enforced; the struct does not exist until a firing site needs it.

### 4. `aberp-compliance` wired as a dependency

ADR-0071 left `aberp-compliance` a workspace member but deliberately NOT a dep of `apps/aberp`. S355 wires it in (regular path dep, the same convention as `aberp-digital-id`). It is unused by `apps/aberp` code this session — cargo does not warn on an unused path dependency, and no workspace lint requires otherwise — but the wiring is the load-bearing step so S356+ firing sites can construct `ExportScreeningStatus` / `CuiMarking` / `LotId` values and reference them in the `personnel.*` payloads without a separate plumbing PR.

## Consequences

**Positive.** The audit ledger now *speaks* defense-grade access-trail events; S356+ adds firing sites without re-litigating the vocabulary. The grant/deny pair, `granted_by`, and the algorithm tag put the Part-11 / NIST AC-3.1.1 facts in the same tamper-evident hash chain that already carries the fiscal moat. The new prefix keeps every existing glob consumer untouched.

**Negative / deferred.** Nothing fires these kinds yet — the access trail is an empty contract until S356+ wires the firing sites (which themselves wait on the controlled resources — CUI documents, export-controlled drawings — existing in code). The payloads are untyped `serde_json::Value`; a future session may promote them to typed structs in `audit_payloads.rs` once a firing site fixes the shape. The e-signature *ceremony* (re-authentication on signing, certificate revocation checking) remains future work per ADR-0070.

**Future work (not S355):** firing sites for all four kinds (S356+); typed payload structs once a firing site stabilizes the shape; the access-control resource model (what a "CUI document" / "export-controlled drawing" is in ABERP's data model); wiring `aberp-compliance` types into the `resource_kind` / `denied_reason` vocabularies.

## §6 — Correction (S367, 2026-06-12 — review F1)

The four payloads above originally named the acting-operator field `operator_id`. S366 found that across ADR-0073…0079 ten different spellings had accumulated (`operator_id`, `signed_by_operator_id`, `marked_by_operator_id`, `assigned_by_operator_id`, `classified_by_operator_id`, `requesting_operator_id`, `shipped_by_operator_id`, `applied_by_operator_id`, `set_by_operator_id`, `screened_by_operator_id`, `reporter_operator_id`) while the two families that actually fire (S350 `QuotePricingMaterialEdited`, S354 `QuotePricingOperatorAccepted` in `apps/aberp::serve`) used **`operator_user_id`** — the Bearer-subject operator login. Since no compliance firing site existed yet, S367 canonicalized every compliance payload's acting-operator key to **`operator_user_id`** and pinned it in the per-EventKind serialization tests. The standing rule lives in the `aberp_compliance::prelude` "Identity-key canonicalization" doc note: new compliance EventKinds reuse `operator_user_id`; the distinct `granted_by` authorizing-party field on the access-grant kinds is a *different concept* (the supervisor who authorized, not the acting subject) and is deliberately left as-is.
