# ADR-0068 — Vendor-PO spend authorization: per-PO ceiling + rolling daily cap, operator gate above, lightweight intent record (no purchasing module in v1)

- **Status:** Partially superseded — S440 / ADR-0091 delivers the operator-driven purchasing/PO module this ADR deferred in v1; the autonomous-firing spend ceiling (per-PO + rolling daily cap on DEAL-saga auto-POs) decided below remains future work that would layer onto `aberp::purchasing::create_po`.
- **Date:** 2026-06-06
- **Deciders:** Ervin (via S265 auto-quoting ground-zero brief)
- **Supersedes:** none.
- **Related:** ADR-0067 (DEAL saga — the caller, and the pause-seam this ADR gates), ADR-0061 (inventory — purchasing/supplier mgmt explicitly deferred there), ADR-0047 (SMTP email security — the supplier email rides the SMTP SPOC), ADR-0008 (audit ledger), the design doc [`docs/design/auto-quoting-ground-zero.md`](../docs/design/auto-quoting-ground-zero.md), and [[trust-code-not-operator]], [[hulye-biztos]], [[aberp-smtp-spoc]].

## Context

When a DEAL needs a material the shop doesn't have on hand (ATP short, per ADR-0069), it must procure — fire a purchase order to a supplier. Two failure modes bound the design:

1. **Autonomous overspend.** A daemon-driven cascade that fires POs without a ceiling could commit Áben to arbitrary spend from background automation. The whole project's posture (trust-code-not-operator, CLAUDE.md rule 2) forbids letting code make unbounded commitments on the operator's behalf.
2. **Operator-gate fatigue.** Gating *every* PO on an operator click defeats the automation — most shortfalls are a few kilos of common stock the operator would approve without thought. A blanket gate trains the operator to rubber-stamp.

The resolution is a **threshold**: small, routine POs fire autonomously; large or cumulatively-large POs gate on the operator. And a scope reality: **there is no purchasing/supplier module** — ADR-0061 explicitly defers "purchasing / supplier management" to a future ADR. This ADR decides the *authorization posture*, not a procurement workflow.

## Decision

**Two ceilings in `quoting_parameters` gate autonomous PO firing; a breach pauses the DEAL saga for an operator decision; the PO itself is a lightweight intent record + a supplier email, not a procurement workflow.**

### 1. Two ceilings

| Knob (`quoting_parameters`) | Meaning |
|---|---|
| `max_auto_po_eur` | per-PO ceiling — a single PO at or below this may fire autonomously |
| `auto_po_daily_cap_eur` | cumulative ceiling — sum of autonomous POs in a rolling 24h window |

A PO fires autonomously **iff** `po_eur <= max_auto_po_eur` **AND** `daily_running_total + po_eur <= auto_po_daily_cap_eur`. Both must hold. The daily cap is the backstop against many-small-POs runaway that the per-PO ceiling alone would miss.

The rolling daily total is computed from the audit ledger (`po.vendor_po_fired` entries in the last 24h) — no side counter to drift. Per no-sql-specific, the window is an app-layer query, not a DB trigger.

### 2. Below threshold → autonomous fire

Record a `vendor_pos` row, send the supplier an email over the SMTP SPOC (ADR-0047 — the same single shared credentials all email surfaces use), emit `po.vendor_po_fired`. This happens inside the DEAL transaction (ADR-0067 §3).

### 3. Above either threshold → operator gate

Emit `po.auto_threshold_exceeded`, **pause** the DEAL (ADR-0067 §4 pause-seam — prior reservations held, not rolled back), surface the single-token gate: *"PO of €X for `material` from `supplier` exceeds your €Y auto-limit — Approve or Decline."* Approve authorizes the PO and resumes the saga; Decline rolls the DEAL back.

The gate is one typed token (hülye-biztos), not a form. The audit entry + the paused quote state are durable across a crash.

### 4. The `vendor_pos` table — lightweight intent, NOT a procurement module

```text
vendor_pos (
  po_id            ULID PRIMARY KEY,    -- prefix `po_`
  tenant_id        ULID NOT NULL,
  quote_id         ULID NOT NULL,       -- the DEAL that triggered it
  material_grade   VARCHAR NOT NULL,    -- FK-by-convention to quoting_materials.grade
  qty              DECIMAL(18,6) NOT NULL,
  eur              DECIMAL(18,6) NOT NULL,
  supplier_name    VARCHAR NOT NULL,    -- operator-typed or from a minimal list
  supplier_email   VARCHAR,             -- where the PO email went
  state            VARCHAR NOT NULL,    -- closed-vocab PoState
  authorized_by    VARCHAR,             -- operator attribution if gated; NULL if autonomous
  created_at       TIMESTAMP NOT NULL,
  notes            VARCHAR
);
```

`PoState`: `fired` (email sent) / `cancelled`. No receiving, no GRN, no supplier master, no price catalogue, no approval hierarchy — those are the future purchasing module. v1's PO is: a record that Áben told a supplier to send material, gated by spend authority. **Receiving the material is a manual `Receipt` stock movement** (ADR-0061) the operator posts when goods arrive; v1 does not auto-link PO → receipt.

### 5. Scope boundary — implementation is post-S275

This ADR is the **standing spec**; it is filed now so the authorization posture is decided before anyone builds it. The S266–S275 session list (design doc §15) does **not** schedule vendor-PO implementation — the DEAL saga skeleton (S273) ships the PO step as a typed seam, S274 fills reservation, and the PO module slots in afterward against this ADR. Per delete-the-part, no code is written here.

## Consequences

- **Background automation cannot overspend.** Two ceilings bound autonomous commitment; everything above gates on a human.
- **Routine procurement stays automatic.** A few kilos of common stock under the ceiling fires without bothering the operator — the automation earns its keep.
- **The daily cap caps the blast radius of a bad day.** Even if every individual PO is small, the day's autonomous spend is bounded.
- **No procurement module is built prematurely.** v1 ships the minimum: an intent record + an email + the gate. Receiving, supplier master, and approval chains are deferred to a real purchasing ADR with a real trigger.
- **The supplier email couples to the SMTP SPOC.** One more consumer of the single shared SMTP credentials (ADR-0047); same boot-cache discipline.

## Adversarial review

- *"A rolling-24h daily cap computed from the audit ledger is a scan on every PO."* The ledger query is bounded (24h of `po.vendor_po_fired` entries — at artisan volume, single digits). Microseconds, like ADR-0061's ATP sum. No side counter means no drift; the audit ledger is the truth.
- *"An operator could set `max_auto_po_eur` to a huge number and defeat the gate."* True — the ceiling is operator-tunable by design (it is *their* spend authority to set). The audit trail records every autonomous PO; an operator who sets a reckless ceiling sees the consequences in their own `po.vendor_po_fired` timeline. The gate protects against *automation* overspending beyond *configured* authority, not against an operator choosing high authority.
- *"Firing a supplier email from inside the DEAL transaction means a rollback can't un-send the email."* Real — email is not transactional. Mitigation: the email send is the **last** action before COMMIT in the autonomous path, and on the gated path it fires only after operator Approve. A rollback after a sent email is the rare residual; the supplier gets a PO that Áben then cancels by a follow-up (manual, v1). The `vendor_pos` row is transactional even if the email isn't; the row is the record of truth.
- *"No supplier master means `supplier_email` is operator-typed and could be wrong — the PO goes to the void."* v1 accepts this; the operator sees the `vendor_pos` row with the typed address and can resend. A supplier master is the future purchasing module's job.
- *"Two concurrent DEALs could each pass the daily-cap check before either commits, jointly breaching it."* Single-process serialization (DuckDB) plus computing the running total at write time inside the tx means the second DEAL's check sees the first's committed PO. Multi-process needs the same `FOR UPDATE` posture as ADR-0061/0067.

## Alternatives considered

- **Gate every PO on the operator.** Rejected — gate fatigue defeats automation; the operator rubber-stamps and the protection becomes theater.
- **No gate, fire all POs autonomously.** Rejected — unbounded autonomous spend is exactly CLAUDE.md rule 2's failure mode.
- **Per-PO ceiling only, no daily cap.** Rejected — many-small-POs runaway slips under it.
- **Build the full purchasing module now** (supplier master, GRN, receiving, approval chains). Rejected — massive scope for a step that, at artisan volume, is a record + an email. Deferred to a real ADR with a real trigger.
- **Approval hierarchy (different ceilings per operator role).** Rejected — single-operator tenant today. Trigger: multi-operator deployment.

## Open questions

1. **PO → receipt auto-link.** Trigger: the purchasing module ADR, or operator pain reconciling POs against `Receipt` movements. Likely `MovementRefKind::Invoice`/a new `PurchaseOrder` ref (ADR-0061 reserved `Invoice`).
2. **Supplier master data.** Trigger: the purchasing module ADR. v1 is operator-typed.
3. **Per-role spend authority.** Trigger: multi-operator tenant.
4. **PO currency.** v1 prices the PO in EUR (the catalogue's `cost_per_kg` unit). A supplier quoting HUF needs an FX step (MNB integration exists). Trigger: first non-EUR supplier.

## Invariants pinned

1. **A PO fires autonomously only if `po_eur <= max_auto_po_eur` AND `daily_running + po_eur <= auto_po_daily_cap_eur`.** Pinned by `po_fires_only_under_both_ceilings`.
2. **A threshold breach emits `po.auto_threshold_exceeded` and pauses the DEAL; it does not autonomously fire.** Pinned by `po_over_threshold_pauses_not_fires`.
3. **The rolling daily total is computed from `po.vendor_po_fired` audit entries in the last 24h — no side counter.** Pinned by `daily_cap_derived_from_audit_ledger`.
4. **An autonomous PO records `authorized_by = NULL`; a gated PO records the operator attribution.** Pinned by `gated_po_records_authorizer`.
5. **`vendor_pos` carries no receiving/GRN state in v1; receipt is a manual `Receipt` stock movement.** Pinned by code review (no receive route exists).
