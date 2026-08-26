# ADR-0090 — Defense quality management: NCR + CAPA workflow + open-NCR shipment gate.

- **Status:** Accepted
- **Date:** 2026-06-16
- **Deciders:** Ervin (via S439 brief — defense quality-management session, auto-mode).
- **Implements:** the AS9100 §10.2 / IATF 16949 §10.2 Non-Conformance-Report (NCR) + Corrective-And-Preventive-Action (CAPA) workflow. Closes the defense quality loop: every S438-marked part UID can now carry a traceable history of quality events, and a part with a known unresolved issue cannot ship.
- **Related:** ADR-0089 (S438 per-unit part-UID marking — the `part_uid`s an NCR references and the part-UID shipment gate this extends), ADR-0085 (S432 heat-lot traceability — the `affected_heat_lots` an NCR can cite), ADR-0064 (dispatch state machine — `mark_shipped` is the Shipment transition both gates guard), S428 (`partner.customer_type` — the defense/aerospace discriminant), ADR-0081 (`aberp-verify` NAV-leakage coverage pin), `[[trust-code-not-operator]]`, `[[hulye-biztos]]`, `[[no-sql-specific]]`, `[[customer-journey-e2e-gate]]`.

## Context

The S438 defense chain (customer → quote → WO → heat-lot → part-UID) records *what a part is*. AS9100 / IATF 16949 additionally require recording *when a part or process failed inspection and what was done about it*: an **NCR** (the non-conformance) and a linked **CAPA** (the corrective + preventive response, its approval, and an effectiveness review). These are the staple defense/aerospace quality records.

Three facts from the codebase shaped the design (verified, not assumed):

1. **The S438 gate resolver `resolve_part_uid_gate` lives in `serve.rs`** (not in the domain crate), is self-contained (derives `customer_type` from the dispatch's `partner_id`, reads the WO's marked count), and is unit-tested via `aberp::serve::resolve_part_uid_gate`. The open-NCR gate mirrors this exactly rather than inventing a second shape (CLAUDE.md rule 7 — pick one pattern).
2. **`part.uid_marked` etc. already existed in `ALL_KINDS`** (counted in the 150 pin). The nine new kinds are genuinely new, giving a real delta of **150 → 159**.
3. **`wo_part_marks` (S438) is the authoritative source of a WO's per-unit part UIDs.** The gate reads the WO's marked UIDs from there and intersects them against `Open`/`Contained` NCRs' `affected_part_uids` — no new linkage table needed.

## Decision

**Add three additive quality tables (`ncrs`, append-only `ncr_transitions`, `capas`), a Quality operational SPA module (NCR list + create + in-page detail with transition timeline + linked CAPAs), a boot-time critical-NCR escalation scan, and a second dispatch-ship gate that refuses a defense/aerospace shipment when any of the WO's marked part UIDs is referenced by an `Open`/`Contained` NCR.** Nine new `ncr.*` / `capa.*` EventKinds (count 150 → 159).

### Schema (additive, natural-keyed, no SQL DEFAULT / no CHECK / no index)

- `ncrs` — `ncr_<ULID>` PK (natural, no surrogate), with severity/category/state stored as the lowercase enum tokens, and `affected_part_uids` / `affected_wo_ids` / `affected_heat_lots` / `photos` as JSON-text array columns (the flexibility the brief asks for, without a join table). No CHECK / no DEFAULT (`[[no-sql-specific]]` + the DuckDB replay-clobber trap); a non-defense tenant simply has zero rows.
- `ncr_transitions` — append-only log keyed by `(tenant_id, ncr_id, seq)`; every state change (incl. the opening `"" → open` and auto-escalations) appends a row.
- `capas` — `capa_<ULID>` PK linked to a parent `ncr_id`.

Filter/sort/page is done in Rust over a full scan (no index — S341/S410, `[[no-sql-specific]]`), matching the audit-screen + AVL precedents.

### Trust the code, not the operator ([[trust-code-not-operator]])

Three invariants live in code:

1. **State transitions** — `allowed_transition(from, to)` is the only legal-edge gate (`Open → Contained → UnderInvestigation → CorrectionApplied → Closed`, with `Escalated` reachable from any non-terminal state and recoverable). A `→ Closed` additionally requires a linked CAPA that is **approved AND effectiveness-Verified** (`Capa::permits_ncr_close`). The SPA mirrors the graph for instant feedback; the POST route re-validates and is the source of truth (a bad close returns 409).
2. **Escalation timer** — a `Critical` NCR not closed within `CRITICAL_ESCALATION_HOURS` (24h) auto-escalates on the boot scan (`escalate_overdue_ncrs`), firing `ncr.escalated`; the operator dashboard surfaces a red banner. Non-fatal at boot ([[hulye-biztos]]) — mirrors the S431 AVL overdue scan.
3. **Refuse-Shipment gate** — `resolve_open_ncr_gate(conn, tenant, dispatch)` (in `serve.rs`, mirroring `resolve_part_uid_gate`) returns `Blocked` when the dispatch is defense/aerospace AND the WO — or any of its marked part UIDs — is referenced by an NCR that has **not reached `Closed`** and carries **no signed management waiver for that WO**. Enforced at `mark_dispatch_shipped_request` right after the S438 part-UID gate; fires `ncr.wo_blocked_by_open_ncr` + 409. The commercial path is unaffected. *(Amended — round 7; it originally read `Open`/`Contained`, which let the escalation timer release the shipment. See the amendment at the end of this ADR.)*

### EventKinds (count 150 → 159)

Five `ncr.*` (`created`, `state_changed`, `escalated`, `closed`, `wo_blocked_by_open_ncr`) + four `capa.*` (`created`, `approved`, `effectiveness_reviewed`, `closed`). All app-layer JSON, never NAV XML — folded into the no-NAV arm of both `extract_nav_xml` sites, pinned by the two `const _` count assertions (ADR-0081) and per-family `*_no_nav_bytes` runtime tests. A new `ncr.*`/`capa.*` prefix keeps the quality surface globbable without sweeping fiscal traffic; the per-OUTGOING-invoice bundle's `invoice.*` glob never sweeps a quality row.

### Photos

Stored under `~/.aberp/<tenant>/ncr-photos/<ncr_id>/` (mirrors the S197 `ap-artifacts` / S281 `email-relay-attachments` per-tenant layout — no new top-level dir). The SPA has no multipart path, so photos ride the existing JSON `invoke` bridge as base64 (prefix-stripped), decoded + written server-side with a sanitized filename (reusing `email_relay_queue::sanitize_attachment_filename`) and an 8 MiB-per-photo cap.

## Consequences

- **Positive:** the defense quality loop is closed end-to-end (part marked → NCR → CAPA → resolved → shipped); a part with a known unresolved issue cannot ship; the escalation timer + transition rules are enforced in code, not operator memory; every quality state change is hash-chained in the audit ledger.
- **Negative / deferred:** events fire UNSIGNED (the DÁP / QES signature thread remains deferred, as with S438). "Approved operator" is modelled as a CAPA sign-off (approve + verify) rather than a per-operator RBAC role — there is no role system in PROD yet. Photos are operator-attested, not content-validated.
- **Neutral:** the NCR detail is an in-page panel, not a deep route — the SPA router is single-level by design (no path params).

## Amendment — 2026-08-26 (round 7, B-1): only `Closed` or a signed waiver releases

### What was wrong

`NcrState::blocks_shipment()` returned true for `Open | Contained` only. The other three non-terminal states — `UnderInvestigation`, `CorrectionApplied`, `Escalated` — all **released** the shipment.

`Escalated` is the one that turns this from a modelling quibble into a release path. Invariant 2 above, the escalation timer, moves a `Critical` NCR to `Escalated` on the boot scan `CRITICAL_ESCALATION_HOURS` (24h) after discovery, with **no human in the loop**. So the worst class of defect, left unresolved for a day, un-blocked its own shipment *by ageing*. Invariants 2 and 3 were in direct contradiction and invariant 2 won: the mechanism whose whole purpose was to raise the alarm was also the mechanism that silenced the gate.

`UnderInvestigation` and `CorrectionApplied` released for a duller reason: "we are looking into it" and "we think we fixed it" are not "it is fixed". Only `Closed` is gated on an approved, effectiveness-Verified CAPA (`Capa::permits_ncr_close`).

### Ervin's ruling

**An open nonconformity blocks the shipment, full stop.** There are exactly two releases:

1. the NCR reaches `Closed` — which already demands the verified CAPA; or
2. a **named manager signs an explicit, reasoned waiver** (waiver / deviation / MRB disposition) for that exact `(ncr_id, work_order_id)` pair.

**A timer must never be able to sign anything off. A person must.**

### What changed

- `NcrState::blocks_shipment()` is now `!matches!(self, NcrState::Closed)`.
- New append-only table `ncr_shipment_waivers` (`waiver_id` = `wvr_<ULID>`, `ncr_id`, `work_order_id`, `approved_by_operator`, `reason`, `approved_at_utc`, `ncr_state_at_waiver`) — the fourth quality table, same additive / no-CHECK / no-index shape as the other three.
- `quality::grant_ncr_shipment_waiver(...)` writes the row and appends one **`ncr.shipment_waiver_granted`** ledger entry carrying the actor, the NCR, the WO, the NCR's state at signing, and the stated reason. **The ledger entry is the accountability** — not the table row, and not the state name. A state machine can be advanced by a background job; a hash-chained entry naming a person cannot.
- `POST /api/ncrs/:id/shipment-waiver` (`{work_order_id, reason}`) is the only way in. `require_ready` supplies the operator login, so the approver cannot be spoofed in the request body, and no background task can reach the route at all.
- `open_ncr_ids_blocking_wo` now takes the waiver set and drops an NCR only when a waiver matches **both** its `ncr_id` **and** this `wo_id`.

### Deliberately narrow

- **No wildcard.** `work_order_id` is required and matched exactly — the manager signs for a shipment they can see, not for every future one.
- **A `Closed` NCR is refused a waiver.** It blocks nothing, so the signature would release nothing; recording one would only manufacture the appearance of a deliberated release.
- **A reason must be a reason.** Blank, `ok`, `approved` are rejected (`validate_waiver_reason`, ≥ 12 characters).
- **Append-only, no revoke.** An over-broad waiver is corrected through the NCR it names, not by editing history.
- **A waiver is not a transition.** The NCR stays exactly where it was; the ledger records that a shipment was released while it was there.

### EventKinds (count 195 → 196)

One new kind: `ncr.shipment_waiver_granted`, tenth member of the `ncr.*` family. Full F12 ritual — `as_str`, `from_storage_str`, `ALL_KINDS`, the independent `round_trip_for_every_variant` hand-list, both NAV-leakage arms (app-only, never NAV XML), and both `const _` count pins bumped `195 → 196`.

### Consequences

- **This refuses more than it used to.** A shop that shipped while an NCR sat in `UnderInvestigation` now needs either the close or the signature. That is the point, and it is the conservative direction.
- **Still no RBAC.** "Named manager" is the authenticated operator login, exactly as the CAPA approve/verify sign-off already is — there is no role system in PROD yet, so nothing here can enforce that the signer is *entitled* to sign. What it does enforce is that a **person** signed, that they said **why**, and that both are in the hash chain. Flagged, not closed.
- **No SPA affordance yet.** The waiver is an API route only; the Quality
  module's NCR detail panel has no "sign off for shipment" control. An
  operator blocked by the widened gate therefore needs the route (or the NCR
  closed) until the panel gains a button. Flagged deliberately rather than
  bundled — a management sign-off is a UI surface that wants its own
  confirmation copy, not a fourth transition button squeezed into this change.
- **The escalation timer keeps its job** — it still raises the alarm and still fires `ncr.escalated`. It simply no longer decides anything about shipping.
- **The `qcr` report annotation is deliberately NOT waived.** `open_ncr_against` still drives the `accept_with_ncr` disposition from the un-waived NCR set: a waiver releases a *shipment*, it does not make the nonconformity stop existing, and the certificate should keep saying so.
