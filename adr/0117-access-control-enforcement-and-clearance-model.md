# ADR-0117 — Access-control enforcement point and the operator clearance model

- **Status:** Proposed
- **Date:** 2026-09-08
- **Deciders:** Ervin Áben
- **Related:** ADR-0070 (`DigitalIdProvider` — the source of an operator's
  `scope` set), ADR-0073 (`personnel.*` — `access_granted` / `access_denied`
  kinds), ADR-0077 (`cui.*` — the `cui.access_event` grant/deny decision kind),
  ADR-0007 (security baseline), ADR-0071 (`aberp-compliance` crate — the home
  for the new `access` module). Unblocks backlog rows **D-08** (CUI deny path)
  and **D-15** (`personnel.access_granted` / `_denied` firing sites).

## Context

Two shipped features record an access decision but can only ever record a
**grant**:

- **D-08 (CUI).** `cui.access_event` fires on every read of a CUI-marked
  product, always with `decision = "granted"` and reason
  *"lawful government purpose (authenticated operator; no clearance model)."*
  The read route has no way to produce a **deny**.
- **D-15 (personnel).** `personnel.access_granted` / `personnel.access_denied`
  are defined (ADR-0073) with no firing site. The e-signature ceremony
  (sub-slices 1–2) shipped; the access kinds did not, for the same reason.

Both were landed with the deny path **explicitly deferred**, each carrying the
same two-part blocker: (a) there is **no defined enforcement point** — no single
place that decides grant-vs-deny — and (b) there is **no clearance/role model**
to decide *against*. This ADR resolves both, once, so a single decision unblocks
both features. It is a **design pass**: it specifies the seam and the model; the
firing-site wiring is the follow-on build slices for D-08 and D-15.

What already exists and constrains the shape:

1. **The identity already carries scopes.** `aberp_digital_id::DigitalId` has a
   `scope: Vec<String>` field, documented as *"authorisation scopes carried by
   this identity, e.g. `["operator", "cui-cleared"]`."* The mock issues an empty
   or minimal set today; a real CAC/eID backend (D-07) would populate it from
   the certificate. The clearance primitive is therefore **already issued by the
   identity layer** — it is not something this ADR needs to invent a store for.
2. **The resource already declares its sensitivity.** A CUI artifact carries a
   typed `CuiMarking` (ADR-0077); a personnel-gated action names the resource it
   guards.
3. **The audit vocabulary already exists.** `cui.access_event`
   (`{entity_kind, entity_id, operator_user_id, decision, reason, accessed_at_ms}`)
   and `personnel.access_granted` / `_denied`
   (`{operator_user_id, resource_kind, resource_id, granted_by, reason |
   denied_reason}`) are pinned. This ADR does not add or change a kind.

The gap is a **decision**, not missing plumbing: *where* the check lives and
*what* turns `(operator, resource)` into grant/deny.

Constraints: the Defense edition is a single-operator-per-tenant pilot; the
identity is the mock provider today (real hardware is D-07); the change must be
edition-agnostic; it must fail **loud** (CLAUDE.md #12 — a silently-swallowed
access decision is the worst-class failure for an access trail); and it must not
add speculative surface (CLAUDE.md #2 / #13).

Already ruled out before this ADR: a full RBAC engine (no second actor in the
pilot to constrain) and per-artifact ACLs (no sharing model). See Alternatives.

## Decision

1. **Scope-set clearance model.** An operator's clearances **are** their
   `DigitalId.scope` set — the ADR-0070 identity layer is the single source of
   authorisation truth, mock today, cert-derived under D-07. A resource declares
   a **required clearance**: a set of scope tokens. Access is

   > **granted iff `required ⊆ subject.scope`, else denied.**

   A flat set-containment check. No role hierarchy, no inheritance, no lattice.

2. **One enforcement seam.** A new `aberp-compliance::access` module owns the
   sole decision function:

   ```rust
   pub struct AccessSubject { pub operator_user_id: String, pub scope: Vec<String> }
   pub struct RequiredClearance(pub BTreeSet<String>);   // empty = no control
   pub enum AccessDecision { Granted, Denied { reason: DenyReason } }

   pub fn authorize(subject: &AccessSubject, required: &RequiredClearance)
       -> AccessDecision;
   ```

   `authorize` is **pure and total** — no I/O, no ledger, no clock. Every access
   site calls it and is the ONLY caller that decides grant/deny; a site MUST NOT
   inline the containment comparison. The site then fires the matching audit
   kind with the decision and reason. Keeping the append at the site (not inside
   `authorize`) matches the AVL/CUI firing-site posture (ADR-0084, ADR-0077) and
   keeps the seam trivially unit-testable as a truth table.

3. **Required-clearance derivation lives next to the resource, not inside
   `authorize`.** `authorize` never learns resource types. Each surface provides
   its own derivation:

   - **CUI (D-08):** `RequiredClearance::for_cui_marking(&CuiMarking)`:
     | marking | required set |
     |---|---|
     | `Unclassified` | `{}` (empty — no control) |
     | `Cui(_)` | `{"cui"}` |
     | `Confidential` | `{"clearance:confidential"}` |
     | `Secret` | `{"clearance:secret"}` |
     | `TopSecret` | `{"clearance:top-secret"}` |
   - **Personnel (D-15):** the gated action supplies its own `RequiredClearance`
     (a caller-named scope set). `authorize` is byte-identical.

4. **Fail-closed for any non-empty required set; fail-open only for the empty
   set.** A scope-less operator reading a CUI-marked product is **denied**
   (`cui.access_event` `decision="denied"`, `reason="missing_clearance"`) and the
   read returns **403**. An `Unclassified` resource has an empty required set, so
   any authenticated operator is granted — and the grant is still **recorded**
   (32 CFR § 2002.4 requires *every* CUI access decision on the trail, not only
   denials; Unclassified is by definition the complement of the controlled set,
   so "no control required" is a correct grant, not a skipped check).

5. **The pilot still demonstrates both paths.** The bootstrap/mock operator is
   granted the scopes the demo needs (e.g. `["operator","cui"]`) so a marked
   product opens and logs a grant; a **second, scope-less identity** exercises
   the deny. The scopes live on the *identity*, never hard-coded in the check —
   so a real CAC operator whose cert lacks the CUI scope is denied by the exact
   same code path, with zero change. The convenience of the pilot's grant path
   does not weaken the check: the check is on scopes, and a scope-less subject is
   denied.

6. **Closed denial-reason vocab.** `DenyReason` starts with the single reason the
   containment check can produce — `MissingClearance` — rendered as
   `"missing_clearance"` into `cui.access_event.reason` (on a deny) and
   `personnel.access_denied.denied_reason`. The enum is treated as extensible
   (future reasons: revocation, time-boxing, two-person-integrity refusal) so a
   later model adds a reason without breaking the pinned payloads.

7. **`granted_by` (ADR-0073 two-person-integrity anchor)** is, in the
   single-operator pilot, the operator's own identity issuer (a self-service
   grant). True two-person integrity — a *distinct* approver — is deferred until
   a second identity exists (Open Q1); the field stays in the payload so the
   contract does not change when it becomes load-bearing.

## Consequences

**What gets easier.** One seam unblocks both D-08's deny path and D-15's
`access_granted` / `_denied`. A scope-less read is denied and logged; a cleared
read is granted and logged. The decision is a pure function → a grant/deny truth
table is the whole unit test, and the audit fires at the site (the seam stays
I/O-free).

**What gets harder / what we lock in.** We commit to a **scope-set** model, not
RBAC: graded implication, role inheritance, and separation-of-duty are a
*superseding* ADR, not an edit here. The mock identity's `scope` set becomes
load-bearing for the demo (it must contain `"cui"` for the CUI demo to grant),
and a real backend must map cert attributes onto these tokens. The CUI read
route gains a real **403** (today it is always 200), so the SPA must render a
denied state.

**Blast radius.** New pure module in `aberp-compliance` + two derivation helpers
+ deny handling at two existing call sites (the CUI read route; the future
personnel-gated action). No audit-kind change, no identity-layer change, no DB
schema change. The append posture (site-fired, on the shared `aberp_db::Handle`
in the surrounding tx — ADR-0099) is unchanged.

## Adversarial review

1. **"A scope-set containment check is not RBAC — you've under-modelled the
   problem."** *Accepted, deliberately.* The pilot is single-operator; RBAC's
   roles, inheritance, and separation-of-duty have no second actor to constrain,
   so they would be speculative surface (CLAUDE.md #2). Scope-set containment is
   the minimal model that makes **deny real** and maps 1:1 onto the
   `DigitalId.scope` the identity layer already issues. RBAC becomes a
   superseding ADR the moment a real multi-operator backend lands (Open Q1) — and
   because `authorize` is a pure seam, swapping the model is a localized change,
   not a surgery across call sites.

2. **"Fail-open on `Unclassified` is a silent widening of what's readable."**
   *Refuted.* `Unclassified` is, by 32 CFR definition, the *complement* of the
   controlled set — "no control required" is the correct answer, not a bypassed
   check. Critically, the access is still **recorded** as a grant, so the trail
   is complete; nothing is silently skipped. The fail-**closed** default governs
   every non-empty required set, which is every controlled marking.

3. **"The mock operator is handed the scopes, so the demo always grants — you
   never actually prove deny works."** *Answered by mandate.* This ADR requires
   (a) a `authorize` truth-table unit test whose scope-less subject MUST be
   denied, and (b) a demo path with a second, scope-less identity that MUST get a
   `decision="denied"` `cui.access_event` and a 403. The grant path being
   convenient in the pilot is irrelevant to correctness: the deny is produced by
   the same containment code, exercised against a scope-less subject.

4. **"`TopSecret` not implying `Secret` will surprise an operator and
   under-grant."** *Accepted and flagged (Open Q2).* Scope tokens are not a
   lattice; `authorize` is a dumb containment check by design. If graded
   implication is wanted, it is an **explicit multi-scope grant** on the identity
   (`["clearance:secret","clearance:top-secret"]`), decided by whoever issues
   clearances — never inferred by `authorize`. Encoding a DoD ladder into the
   check would smuggle policy into a primitive that must stay total and obvious.

5. **"Putting derivation at the call site means each site can drift its own
   policy."** *Accepted with a guardrail.* Derivation is centralized per surface
   in one helper (`for_cui_marking` for all CUI reads); a site calls the helper,
   it does not hand-roll a required set. The site owns *which resource* it is
   guarding (local knowledge); the helper owns *what that resource requires*
   (one place). `authorize` owns the comparison (one place). No site owns the
   grant/deny logic.

## Alternatives considered

- **Full RBAC (roles → permissions, with inheritance + separation-of-duty).**
  Lost: no second actor in the pilot to justify the machinery; it needs a role
  store that does not exist; and `DigitalId.scope` already provides the simpler
  primitive the identity layer issues. "More capable" is not a reason to build it
  before a second operator exists. Revisit under Open Q1.
- **Per-artifact ACL (an explicit grant list per resource).** Lost: there is no
  sharing or collaboration model — every artifact is tenant-owned and
  single-operator. An ACL store is speculative surface (CLAUDE.md #2) with no
  consumer.
- **Keep deferring (status-quo always-grant).** Lost: it leaves D-08 and D-15
  permanently half-built and the access trail structurally **unable to record a
  denial** — the exact fact 32 CFR / NIST AC-3 most want evidenced. "It's a pilot"
  does not justify shipping a compliance surface that cannot say no.
- **Put the derivation inside `authorize`.** Lost: `authorize` would have to know
  every resource type (CUI now, personnel actions now, future kinds later),
  making the seam a growing switch instead of a total comparison. Keeping
  resource knowledge at the call-site helper keeps `authorize` pure and closed.

## Open questions

- **Q1 — multi-operator roles + real two-person integrity for `granted_by`.**
  Deferred to a future RBAC/authority ADR that lands with the second
  `DigitalIdProvider` (ADR-0080) or the real hardware backend (D-07). Until then
  `granted_by` = the operator's own issuer (self-service).
- **Q2 — graded clearance implication** (does `secret` imply `confidential`?).
  Decided **not** here: scope tokens are flat; graded access is an explicit
  multi-scope grant. A lattice is a superseding ADR if a real deployment demands
  it.
- **Q3 — provenance of operator scopes in production.** Where the `scope` set
  comes from on a real backend (CAC certificate attributes / DÁP eID scopes /
  an operator-admin surface). Resolved by D-07 / ADR-0086; this model consumes
  `DigitalId.scope` regardless of issuer.
