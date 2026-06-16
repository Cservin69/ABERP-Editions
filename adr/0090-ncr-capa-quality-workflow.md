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
3. **Refuse-Shipment gate** — `resolve_open_ncr_gate(conn, tenant, dispatch)` (in `serve.rs`, mirroring `resolve_part_uid_gate`) returns `Blocked` when the dispatch is defense/aerospace AND any of the WO's marked part UIDs is referenced by an `Open`/`Contained` NCR. Enforced at `mark_dispatch_shipped_request` right after the S438 part-UID gate; fires `ncr.wo_blocked_by_open_ncr` + 409. The commercial path is unaffected.

### EventKinds (count 150 → 159)

Five `ncr.*` (`created`, `state_changed`, `escalated`, `closed`, `wo_blocked_by_open_ncr`) + four `capa.*` (`created`, `approved`, `effectiveness_reviewed`, `closed`). All app-layer JSON, never NAV XML — folded into the no-NAV arm of both `extract_nav_xml` sites, pinned by the two `const _` count assertions (ADR-0081) and per-family `*_no_nav_bytes` runtime tests. A new `ncr.*`/`capa.*` prefix keeps the quality surface globbable without sweeping fiscal traffic; the per-OUTGOING-invoice bundle's `invoice.*` glob never sweeps a quality row.

### Photos

Stored under `~/.aberp/<tenant>/ncr-photos/<ncr_id>/` (mirrors the S197 `ap-artifacts` / S281 `email-relay-attachments` per-tenant layout — no new top-level dir). The SPA has no multipart path, so photos ride the existing JSON `invoke` bridge as base64 (prefix-stripped), decoded + written server-side with a sanitized filename (reusing `email_relay_queue::sanitize_attachment_filename`) and an 8 MiB-per-photo cap.

## Consequences

- **Positive:** the defense quality loop is closed end-to-end (part marked → NCR → CAPA → resolved → shipped); a part with a known unresolved issue cannot ship; the escalation timer + transition rules are enforced in code, not operator memory; every quality state change is hash-chained in the audit ledger.
- **Negative / deferred:** events fire UNSIGNED (the DÁP / QES signature thread remains deferred, as with S438). "Approved operator" is modelled as a CAPA sign-off (approve + verify) rather than a per-operator RBAC role — there is no role system in PROD yet. Photos are operator-attested, not content-validated.
- **Neutral:** the NCR detail is an in-page panel, not a deep route — the SPA router is single-level by design (no path params).
