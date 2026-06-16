# ADR-0089 — Per-unit part UID / serial marking: marking UI, defense shipment gate, Part UID Lookup.

- **Status:** Accepted
- **Date:** 2026-06-16
- **Deciders:** Ervin (via S438 brief — third defense firing-site session, auto-mode).
- **Implements:** the firing site for the `part.uid_marked` + `part.serial_assigned` EventKinds that the S344–S371 defense foundation shipped kind-only (S358 / ADR-0075), never fired. The S425 audit flagged these as the LAST unbuilt firing-site kinds from the foundation. Closes the chain customer → quote → WO → heat-lot (S432) → **part-UID** (this session).
- **Related:** ADR-0075 (`part.*` per-unit serialization family + the never-fired `uid_marked`/`serial_assigned` kinds — this is the first firing site), ADR-0085 (S432 heat-lot traceability — the heat lot a marked part references), ADR-0064 (dispatch state machine — `mark_shipped` is the Shipment transition the gate guards), S428 (`partner.customer_type` — the defense/aerospace discriminant), S429 (`work_orders.source_quote_id` — the WO→quote→customer link the trace walks), ADR-0062 (WO state machine — `Completed` is when marking opens), ADR-0081 (`aberp-verify` NAV-leakage coverage), `[[trust-code-not-operator]]`, `[[hulye-biztos]]`, `[[no-sql-specific]]`.

## Context

The defense foundation shipped `EventKind::PartSerialAssigned` + `PartUidMarked` (S358) with a documented MIL-STD-130N IUID / IRI payload sketch (referencing an `aberp_compliance::uid` module) but **no firing site**. The S438 brief specifies a simpler, deterministic scheme — a `dp-`-prefixed ULID per part plus an ASME-Y14.41-style DataMatrix payload — and ships unsigned events (the DÁP / QES signature thread is S438+ deferred and will retroactively bring these rows into the signed chain).

Three facts from the codebase shaped the design (verified, not assumed):

1. **The WO state machine has no `Closed`/`Shipped` state.** `WorkOrderState` is `Created → Released → InProgress → Completed / Cancelled / OnHold` (ADR-0062). The brief's "Closed" maps to `Completed`; the brief's "Shipped" maps to the **dispatch** crate's `mark_shipped` transition (ADR-0064), the real point a part leaves.
2. **`part.uid_marked` + `part.serial_assigned` already exist in `ALL_KINDS`** (counted in the 148 pin). The brief named four "introduced" kinds, but two pre-exist — so the real count delta is **+2** (`part.wo_blocked_no_uid`, `part.traceability_viewed`), giving **148 → 150**, not 152.
3. **A WO carries no direct customer link**, but `source_quote_id` (S429) → `quote_pricing_jobs.{material_grade, buyer_partner_id}` (S428) → `partners.customer_type` is a clean, persisted chain (reused from ADR-0085's heat-lot gate). The dispatch additionally carries `partner_id` directly — the shipment gate uses that.

## Decision

**Add an additive per-unit `wo_part_marks` table, mint a `dp-<ULID>` part UID per produced unit from a new Mark-Parts action on a Completed defense/aerospace WO, refuse a defense/aerospace dispatch's `Shipped` transition when any unit is unmarked, and extend the Material Traceability tab with a forward/reverse Part UID Lookup.** Two new `part.*` EventKinds (count 148 → 150).

### Schema (additive, per-unit, no SQL DEFAULT)

A new `wo_part_marks` table keyed by the natural composite `(tenant_id, wo_id, unit_index)` — no surrogate id, per the codebase's natural-key convention; no CHECK / no DEFAULT (`[[no-sql-specific]]` + the DuckDB replay-clobber trap). The brief's "additive schema on WO outputs, all nullable for back-compat" is satisfied by the table's absence of rows: a non-defense (or unmarked) WO simply has **zero** rows — that absence IS the gate signal and the back-compat default. A per-unit *row* (not nullable columns on `work_orders`) is required because `qty_target > 1` means N distinct UIDs per WO.

### Part UID + DataMatrix payload (deterministic, pure)

- `part_uid` = `dp-<26-char-ULID>`. The `dp-` prefix tags the namespace so a scanned UID is never confused with an internal `prt_`/`wo_` ULID or operator text. Minted server-side ([[hulye-biztos]] — the operator never types a UID).
- `serial_number` = operator-typed (validated: ≤64 chars, no `|`, no control chars) **or** auto-derived `<wo_id>-<index>` when blank.
- `data_matrix_payload` = `dp-<ULID>|<serial>|<heat_lot_8chars>` — scanning it recovers all three identifiers; the heat-lot tail re-enters the S432 material chain. The `|` delimiter is why a serial carrying `|` is rejected.

### Divergence from the S358 IUID sketch (surfaced, not blended)

S358's doc-comment sketched a MIL-STD-130N IRI payload via `aberp_compliance::uid`. The S438 firing follows the brief's `dp-`/DataMatrix scheme instead — the kinds are untyped at the ledger, so they are reused; the divergence is documented here (same posture as ADR-0085's reuse of `material.heat_lot_assigned`). The S358 payload-shape tests are hand-built JSON literals and do not constrain the firing site. `part.uid_marked` and `part.serial_assigned` fire once each per Mark-Parts save, as batch records over all units.

### New EventKinds (2; count 148 → 150), all `part.*`

- `part.wo_blocked_no_uid` — the defense/aerospace Shipment refusal record.
- `part.traceability_viewed` — the DoD-IUID / AS9100D §8.5.2 record-of-access: who ran a Part UID Lookup, when.

Both sit in `part.*` to keep the per-unit-serialization surface globbable as one prefix; app-layer JSON payloads only, never NAV XML — added to both NAV-leakage exhaustive arms (`aberp-verify`, `export_invoice_bundle`) + all three `== 150` count pins.

### The shipment gate ([[trust-code-not-operator]])

`resolve_part_uid_gate` (pure, unit-tested): dispatch `partner_id` → `customer_type`; if `Defense`/`Aerospace`, require the WO's marked-part count to reach `qty_to_units(qty_target)`. Enforced at the dispatch-ship route **before** `mark_shipped` (mirrors ADR-0085's WO-start heat-lot gate — the gate lives at the serve layer, not inside the `aberp-dispatch` crate, so the crate stays unaware of part marks). On block: one `part.wo_blocked_no_uid` audit entry + a 409 naming the first unmarked unit. The commercial path and quote-less WOs are unaffected.

### Traceability (forward + reverse)

The existing Material Traceability tab gains a Part UID Lookup section (no new route). Forward: `part_uid` → the WO, heat lot, originating quote, customer. Reverse: `customer_id` → every part UID on a **Shipped** dispatch to that partner. Both fire `part.traceability_viewed`. DataMatrix image rendering (label printing) is explicitly out of scope — this session ships the data, not the printer.

## Consequences

- **Good:** the foundation's last unfired kinds now fire; defense parts cannot ship unmarked (code-enforced); a scanned DataMatrix recovers the full pedigree; the trace closes the customer→…→part chain.
- **Deferred:** events are UNSIGNED until the DÁP/QES thread (S438+) anchors them; DataMatrix image rendering; per-unit physical material consumption (the heat lot is the WO-grade snapshot recorded at mark time, same sparseness ADR-0085 surfaces).
- **Cost:** one new app-layer table + module, two EventKinds, one serve gate + two routes/commands, three SPA pieces (modal, chips, lookup section). No change to the `aberp-dispatch` or `aberp-work-orders` crates.
