# The dream shop — end-to-end no-touch workflow, and the dot-peen traceability spine

> ## ⚠️ NUMBERS PROVISIONAL — assign unique at merge
>
> Every ADR and backlog id quoted in this document that is **not already on
> `origin/main`** is provisional. Numbering is currently colliding across
> unmerged branches: **ADR-0113** (auto-probe pricing, branch
> `docs/adr-auto-probe-inspection` @ `3058b20`), **ADR-0115** (internal
> portal, branch `docs/adr-0113-internal-portal`), **ADR-0199** (QC/FAIR
> report, branch `docs/adr-qc-inspection-report` @ `1520f29`), and **D-20**
> claimed three times over (portal, auto-probe, `fix/pricing-queue-head-of-line`).
> The highest ADR on `origin/main` is **0112**; the highest backlog id is **D-19**.
> This document introduces **D-90 (EPIC)** and the provisional component ids
> **D-91 … D-95**, deliberately far out of band. Whoever merges must renumber
> the EPIC, its children, the anchors, and the cross-links in both directions.

- **Status:** North-star architecture. **Design only — no code written, no component ADR re-designed.**
- **Date:** 2026-08-22
- **Base:** ABERP-Editions `origin/main` @ `9e4a6ee`. Every file:line below was reproduced at that SHA.
- **Deciders:** Ervin — mandate: *"cook it and form our dream shop workflow"* for a
  no-touch, lights-out defense/aerospace machine shop, with **dot-peen barcode
  tracking** flagged as the piece of specific interest.
- **Scope:** **Defense edition only.** Portable is frozen (ADR-0093 saw-off);
  nothing here may move it. The frozen prod line (`ABERP.git`) is never a build target.
- **What this document is:** the **connective tissue**. It says how the pieces already
  in design fit into one flow, and it designs the one piece that is missing —
  the **traceability spine**. It does not re-open the component ADRs; it
  references them and states where each plugs in.
- **What this document is not:** a component design. If you want the probe pricing
  model, read ADR-0113. The report layout, ADR-0199. The verdict math, ADR-0092.

**Component designs this ties together** (read these; they are not summarised here):

| Piece | Design | State on `origin/main` |
|---|---|---|
| Auto-probe priced into every quote | ADR-0113 *(provisional; branch `docs/adr-auto-probe-inspection` @ `3058b20`)* | Not on main — design only |
| Probe capture → `qc_inspections` → verdict → auto-NCR | [ADR-0092](../adr/0092-on-machine-probe-ingestion-to-qc.md) | **Built** (S443); transports `todo!()` — [D-02](BACKLOG-designed-to-live.md#d-02) |
| Tolerance taxonomy + cost driver | [ADR-0097](../adr/0097-quote-engine-machining-tolerance-cost-driver.md) | **Live** |
| QC / FAIR report + Certificate of Conformance | ADR-0199 *(provisional; branch `docs/adr-qc-inspection-report` @ `1520f29`)* | Not on main — design only |
| Per-unit part UID marking + shipment gate | [ADR-0089](../adr/0089-part-uid-marking.md) | **Live** (data only — see §2.3) |
| Heat/lot material traceability + WO-start gate | [ADR-0085](../adr/0085-heat-lot-traceability.md) | **Live** |
| NCR / CAPA + refuse-shipment gate | [ADR-0090](../adr/0090-ncr-capa-quality-workflow.md) | **Live** |
| MES adapter framework (barcode, Zebra, MTConnect, UR RTDE, Trumpf) | [ADR-0060](../adr/0060-stage3-manufacturing-adapter-framework.md) | **Live** |
| Durability, hash-chained ledger, checkpoint under the writer lock | ADR-0110 / [ADR-0111](../adr/0111-editions-checkpoint-under-the-writer-lock.md) | **Live** |
| Unattended-write service identity (the "no operator" leg) | [ADR-0088](../adr/0088-unattended-write-service-identity.md) | **Proposed** — see §6.4 |
| STEP-only CAD pipeline, located holes | [ADR-0112](../adr/0112-step-only-cad-pipeline-located-holes-and-drilling-cycle-time.md) | Slice A live; slice C gated on [D-19](BACKLOG-designed-to-live.md#d-19) |

---

## 1. The flow, end to end

### 1.1 Diagram

```
┌─ CUSTOMER ────────────────────────────────────────────────────────────────┐
│  STEP file  ──►  ① QUOTE                                                  │
│                   quote engine: material · machining · setup · CAD/CAM     │
│                   · tolerance (ADR-0097) · INSPECTION (ADR-0113 prov.)     │
│                   every part auto-probe-priced, non-optional, in code      │
│                   ▼ accepted                                              │
└───────────────────┼───────────────────────────────────────────────────────┘
                    │
              ② ORDER → WORK ORDER release
                    │   heat/lot assigned + gate (ADR-0085): a Defense WO
                    │   cannot Start without a heat lot
                    │
              ★ UID MINTED HERE ★  part_uid = dp-<ULID>, one per unit
                    │   (§2.4 — moved earlier than ADR-0089's mint-at-marking)
                    │   the cell order, the NC program, the probe stream and
                    │   the mark all carry the SAME key from this point on
                    ▼
┌─ LIGHTS-OUT CELL ─────────────────────────────────────────────────────────┐
│                                                                           │
│   ③ ROBOT LOAD           UR cobot, RTDE (ADR-0060 · ur_rtde.rs)           │
│        │                 ⚠ today read-only telemetry — §6.1               │
│        ▼                                                                  │
│   ③ MACHINING            DMG MORI NTX turn-mill, FANUC control            │
│        │                 MTConnect /current → Execution (Live)            │
│        ▼                                                                  │
│   ③ IN-PROCESS PROBE     Renishaw / Blum spindle probe, part still        │
│        │                 clamped in its own datum frame (ADR-0113 D1)     │
│        │                                                                  │
│        ├──────────► ⑥ CAPTURE ── two channels, two jobs (§4)             │
│        │              MTConnect  → cycle visibility (state/program/alarm) │
│        │              DPRNT file → the measured VALUES                    │
│        │                     ▼                                            │
│        │              ProbeIngestionSource (ADR-0092, built)              │
│        │                     ▼  record_inspection → compute_verdict       │
│        │              qc_inspections  ── out-of-tol ──► auto-NCR (0090)   │
│        ▼                                                                  │
│   ③ ROBOT UNLOAD                                                          │
│        ▼                                                                  │
│   ④ DOT-PEEN MARK        Data Matrix ECC200, payload = the UID (§2)       │
│        ▼                                                                  │
│   ④ MARK-VERIFY READ-BACK  ── FAIL ──► quarantine position                │
│        │  (DPM-capable reader in-cell)      + part.mark_verify_failed     │
│        │  a no-touch cell must not emit an anonymous part                 │
│        ▼ PASS → part.mark_verified                                        │
└────────┼──────────────────────────────────────────────────────────────────┘
         │
         ▼
   ⑤ BARCODE TRACKING — every scan is a GENEALOGY EVENT (§3)
         post-machining ──► inspection ──► dispatch
         barcode adapter (Live, TCP) → resolve_scan() chokepoint
         → part_genealogy_events (regulated state, NOT the lossy MES broadcast — §3.1)
         │
         ▼
   ⑦ QC / FAIR REPORT + CoC  (ADR-0199 prov.)
         nominal + tolerance (ADR-0097 / qc_inspection_plans)
              × probe actuals (⑥)   keyed by part_uid
         → qc_reports / qc_report_lines (frozen snapshot, never a live view)
         │
         ▼
   ⑧ SHIPMENT — dispatch mark_shipped, ONE transaction
         three Defense gates, same shape, all pure resolvers:
           part-UID gate (ADR-0089, Live)  ── §2.6 tightens to VERIFIED marks
           open-NCR gate (ADR-0090, Live)
           QC-report gate (ADR-0199 prov.)
         report bound to dsp_id in the same tx; the marked part carries its UID out
         │
         ▼
   ⑨ AUDIT — hash-chained ledger, durable-acked (ADR-0110/0111), mirrored
         one part_uid joins: order · heat/MTR · machine+program · probe actuals
         · QC report SHA-256 · shipment · every scan · and WHO (or what) acted
```

### 1.2 The chain in one line

```
quote(auto-probe priced) → order → heat lot → ★UID★ → cut → probe → mark → verify
    → scan · scan · scan → report → ship → retained, hash-chained, joinable by ★UID★
```

**The UID is the spine.** Everything above hangs off one key. That is the whole
architectural claim of this document, and §2 designs it.

### 1.3 Where each piece plugs in — the seam table

| # | Step | Plugs into | Seam it uses | Status |
|---|---|---|---|---|
| ① | Quote | quote engine subtotal | `inspection_cost` term after `tolerance_op_cost`; `CatalogueSnapshot.inspection_policy: Option<_>` | ADR-0113, design |
| ② | Order → WO | `work_orders` + `inventory_balances` | `resolve_heat_lot_gate` at `Start` | Live |
| ★ | UID mint | `wo_part_marks` | mint moves from Completed→**Released** (§2.4) | **New — the spine** |
| ③ | Robot | `aberp-mes` `UrRtdeAdapter` | RTDE port 30004 | Telemetry Live; **command path absent** (§6.1) |
| ③ | Machine | `MtconnectAdapter` | `GET /{device}/current` | Live; `/sample` is [D-16](BACKLOG-designed-to-live.md#d-16) slot 2 |
| ④ | Dot-peen | *no adapter exists* | new `MarkerAdapter`, same family as Zebra | **New — the spine** |
| ④ | Verify | `BarcodeScannerAdapter` (TCP) | `ScanReceived` | Adapter Live; **no consumer** (§3.1) |
| ⑤ | Scans | same adapter | `resolve_scan` → `part_genealogy_events` | **New — the spine** |
| ⑥ | Capture | `ProbeIngestionSource` trait | 3rd impl: `DprntFileSource` (§4.2) | Trait built; impls `todo!()` — [D-02](BACKLOG-designed-to-live.md#d-02) |
| ⑦ | Report | `qc_reports` / `aberp-qc-pdf` | `submit_measured_characteristics` | ADR-0199, design |
| ⑧ | Ship | `mark_shipped` tx | 3rd gate + `ShipmentDocumentBinder` | ADR-0199, design |
| ⑨ | Audit | `audit-ledger` + `aberp_db::Handle` | regulated EventKinds, hash chain, durable ack | Live |

---

## 2. The dot-peen traceability spine

This is the flagged piece, and it is the piece that does not exist. Sections
2.1–2.7 are the design.

### 2.1 What is actually true today (verified, not assumed)

Three facts shape everything below.

1. **The "mark" is a database string.** ADR-0089 is Live and shipped the *data*:
   `wo_part_marks` carries `part_uid`, `serial_number`, `data_matrix_payload`,
   `heat_lot_reference` (`apps/aberp/src/part_marking.rs:149-156`). ADR-0089 says
   so explicitly — *"this session ships the data, not the printer."* **Nothing in
   the repo ever puts a mark on a part.** `ZebraAdapter::print_zpl` exists
   (`crates/aberp-mes/src/adapters/zebra.rs`) and has **zero call sites** outside
   its own module and two doc comments. The printer is registered, supervised and
   health-probed; it has never been printed to.
2. **A scan reaches the ledger but reaches no business logic.**
   `BarcodeScannerAdapter` is Live and emits `CanonicalEvent::ScanReceived`
   (`barcode_scanner.rs:433`). Grepping every consumer: the only non-test
   references are the adapter itself, the ledger writer, and a doc comment. **No
   code anywhere resolves a scanned payload to a part.**
3. **A measurement is not yet attached to a unit.** `qc_inspections` has a
   `linked_part_uid` column and the manual route never populates it
   (ADR-0199 §C2.4). `RawProbeEvent` carries no serial today; ADR-0199 §D3(b)
   adds `part_serial` / `characteristic_number` / `program_id` as `Option`s.

So the physical→digital link is **designed at both ends and joined in the
middle by nothing.** That gap is this section.

### 2.2 What the Data Matrix encodes — a pointer, not a record

**Decision: the mark carries the UID and nothing else. The digital twin is the
source of truth; the mark is a pointer to it.**

```
   MARKED:   dp-01JB8Q3K7YFN2WMXZR4V6TCE90          (29 chars)
   NOT:      dp-01JB8Q3K7YFN2WMXZR4V6TCE90|SN-4471|H24Q1188   (today's payload)
```

This is a **divergence from what ADR-0089 shipped**, which builds
`dp-<ULID>|<serial>|<heat_lot_8chars>` (`part_marking.rs:121-126`). The divergence
is deliberate, and here is the whole argument:

- **Single source of truth.** A serial and a heat lot printed into steel are a
  *copy*. Copies go stale: a heat lot corrected after a supplier re-issues an
  MTR, a serial re-allocated after a scrap-and-remake, a rework under a second
  lot. The database row can be corrected; **a peened part cannot.** A pointer is
  never stale, because it asserts nothing except identity.
- **The truncation is lossy and unverifiable.** `heat_lot_8chars` is a *prefix* of
  a validated `LotId` of up to 32 chars (ADR-0085). Eight characters of a heat
  lot is not a heat lot — it cannot be verified against the MTR and it can
  collide. Carrying a lossy copy of a compliance field is worse than carrying none.
- **Mark cycle time and mark real estate.** Dot peen cost is *dots*. A 29-char
  alphanumeric Data Matrix is a **16×16** ECC200 symbol; the ~55-char composite
  needs **22×22** — roughly **1.9× the cells**, so ~1.9× the peening time on a
  cycle that is inside the priced bottleneck, and a materially larger minimum
  markable area on a small part. Marking the pointer is the cheaper mark on
  every axis.
- **Parsing the mark should not constrain business data.** ADR-0089 must reject
  any serial containing `|` *because the mark format uses it as a delimiter*.
  That is the tail wagging the dog. A pointer has no delimiter and no reserved
  characters.
- **The read is a lookup, and the lookup is the point.** A scan that resolves
  tells you everything — order, heat, MTR, machine, program, every probe touch,
  the QC report, the shipment. A scan that does not resolve tells you something
  more important: *this part is not ours, or its record is missing.* A composite
  payload hides that behind a partial local parse.

**The one honest objection, and the answer.** A pointer is worthless if ABERP is
unreachable. Answer: the human-readable travelling copy is the **document**, not
the part. The CoC and the dimensional report (ADR-0199) carry serial, heat lot
and MTR reference in human-readable form and travel with the shipment; MIL-STD-130
separately requires human-readable text alongside the machine-readable mark for
most items, so a defense part carries a **human-readable line and a Data Matrix**
— readable data on the part, resolvable identity in the code. That is the right
split, and it is what §2.3 encodes.

**Migration cost of changing this: zero, today. Not zero later.** No part has ever
been physically marked (§2.1), so no legacy symbol exists in the world. The
resolver should nonetheless accept both spellings — take the leading token up to
the first `|` — because `wo_part_marks` rows already contain composite strings and
the *stored* payload is what a re-print would emit. Change the emitter now while
it costs nothing.

### 2.3 Standard — ECC200, and three payload variants behind one resolver

**Symbology.** Data Matrix **ECC200**, square, per **ISO/IEC 16022**. Not a
decision so much as the only answer: it is the mandated symbology for both
MIL-STD-130 and ATA Spec 2000, it is Reed-Solomon error-corrected (a peened
symbol with damaged cells still reads), and every DPM-capable reader speaks it.

**Mark quality grading.** Direct part marks are graded under **ISO/IEC TR 29158
(AIM DPM-1-2006)**, *not* ISO/IEC 15415 — 15415 is the print-contrast method for
labels and gives meaningless results on peened metal. The aerospace process spec
for the mark itself (dot size, spacing, depth, placement) is **SAE AS9132**.
**⚠ FLAGGED (§7.2):** minimum acceptable DPM grade. Default proposed: **grade C
or better** at a stated aperture/wavelength, verified by the in-cell read-back
(§2.5), with a periodic graded verification on a sample rather than every part.

**The payload standard is contract-dependent, and that is an architecture problem,
not a settings problem.** Three regimes:

| Regime | When it applies | Payload shape |
|---|---|---|
| **`AberpPointer`** *(default)* | Everything today — EU/HU commercial, EU defense, pilot | `dp-<ULID>` |
| **`Iuid15434`** | Items delivered on a **US DoD** contract → **MIL-STD-130** | ISO/IEC 15434 Format 06 envelope, DIs, UII Construct 1 or 2 (IAC + enterprise id + serial) |
| **`Ata2000`** | Civil aerospace parts under **ATA Spec 2000 Ch. 9** | ISO/IEC 15434 Format 06, `MFR` (CAGE) + `PNR` + `SER` data identifiers |

Note what this table says: **MIL-STD-130 and ATA Spec 2000 both mandate a
*structured, semantic* payload** — the exact opposite of §2.2's pointer. That is
not a contradiction to resolve by argument; it is a variant to model in code.

**Decision:** one typed, closed enum of payload variants, resolved **server-side**
from the buyer partner / contract, with a **single resolver** that parses all
three back to a `part_uid`:

```rust
enum MarkPayloadKind { AberpPointer, Iuid15434, Ata2000 }

fn render_mark_payload(kind: MarkPayloadKind, mark: &PartMark) -> String;
fn resolve_scan(raw: &str) -> ScanResolution;   // parses ALL variants + legacy composite
```

Consequences worth stating: MIL-STD-130 is **already tracked** as
[D-03](BACKLOG-designed-to-live.md#d-03) — `aberp-compliance::uid` implements
`IuidConstruct1`/`IuidConstruct2`, `validate_iac()` and IRI rendering
(`crates/aberp-compliance/src/uid/mod.rs`), blocked on an assigned **Issuing
Agency Code + enterprise identifier**. ATA Spec 2000 needs a **CAGE code**. Both
are registrations, not vendor dependencies. So: **the pilot ships `AberpPointer`;
the other two are one render function each behind an assignment Ervin can
pursue.** ⚠ FLAGGED (§7.1): is a US DoD deliverable in scope at all? If yes,
D-03 stops being optional.

### 2.4 When the UID is minted — at job release, not at marking

**Decision: mint the per-unit UIDs when the work order is released into the cell,
before any metal is cut.** This moves the mint from ADR-0089's position (an
operator action on a **Completed** WO).

Why it must move:

- **The probe stream needs a key while the part is still in the machine.** The
  DPRNT header and the MTConnect cycle both need to say *which unit this is*. If
  the UID does not exist until after machining, every measurement has to be
  reconciled backwards from cycle order — the exact fragile join that produces
  anonymous data. Mint first, and the machine simply reports the key it was given.
- **A no-touch cell has no operator to press "Mark Parts".** ADR-0089's mint is an
  operator action in a modal. There is no operator. The mint must be a
  consequence of *release*, which is already a system transition.
- **It makes the mark the last step, not the first.** The physical mark becomes
  what it should be: the moment an already-existing digital identity is made
  physical. Everything before it is *allocated but unembodied*, which is exactly
  the truth about a part that is still a bar of stock.

**So `wo_part_marks` gains a lifecycle**, replacing "the row exists ⇒ it is marked":

```
Allocated ──► Marked ──► Verified ──► Shipped
    │            │
    │            └──► MarkVerifyFailed ──► ReMarked ──► Verified
    └──► Scrapped (never embodied; the UID is retired, never reused)
```

**⚠ This changes the meaning of a Live gate.** `resolve_part_uid_gate`
(`serve.rs:17250`) today compares `count_part_marks(wo)` against
`qty_to_units(qty_target)`. Under mint-at-release, rows exist from the moment of
release, so **that count becomes vacuously satisfied** — the Defense shipment gate
would silently stop gating. The gate predicate must move to counting rows in state
**`Verified`** *in the same change that moves the mint*. This is the single most
dangerous edit in the whole spine: it is a one-line predicate change that, done
half-way, converts a working compliance gate into a no-op that still looks green.
Named here so no later session discovers it the hard way. (Backlog: [D-91](BACKLOG-designed-to-live.md#d-91).)

### 2.5 Where and how it is marked — and why read-back is not optional

**Placement in the cycle:**

```
machining complete → in-process probe → robot unload → PRESENT TO MARKER
                                                     → peen
                                                     → PRESENT TO READER
                                                     → read-back verify
                                                     → place: good rack | quarantine
```

**Why a marker station in the cell, and not a spindle-mounted marking tool.**
A marking stylus in the NTX toolchanger is possible and it is the wrong trade:
it burns *spindle* minutes — the resource ADR-0113's whole cost model prices as
the bottleneck — it puts peening debris in the cutting envelope, and it forces the
mark to happen before unclamp, which means before the part is proven complete. A
benchtop pin marker in the cell costs the robot a few seconds of travel that is
already free (the arm is holding the part), and it decouples mark cycle time from
machine cycle time. **A dot-peen marker is a benchtop device that speaks TCP; it
belongs on the same seam as the Zebra printer.**

**Mark-then-verify, in-cell, before the part leaves.** This is the load-bearing
rule of the whole spine and it deserves its own statement:

> **An unverified mark is worse than no mark.** A missing mark is visible — the
> gate blocks the shipment. An *unreadable* mark is invisible: the part looks
> marked to a human, the row says `Marked`, and the failure surfaces weeks later
> at a customer's receiving dock, or at an audit, as a part nobody can identify.
> In a lights-out cell there is nobody to notice. So the cell verifies its own
> work: a DPM-capable reader reads the symbol back and the read must resolve to
> the UID that was just written.

Failure handling, in code, not in operator discipline:

1. **Read-back fails.** Emit `part.mark_verify_failed`. Do **not** re-peen the same
   cells — over-peening a partially-formed symbol destroys it rather than
   deepening it. Re-mark **once**, at the defined alternate location on the part
   (the mark location is per-product master data), then re-verify.
2. **Second failure.** The robot places the part in the **quarantine** position and
   the cell **continues with the next part**. A single unmarkable part must not
   wedge a lights-out cell. (This is the same class of bug as the pricing-queue
   head-of-line wedge; do not repeat it here.)
3. **The part stays `MarkVerifyFailed` forever** unless a human dispositions it.
   It cannot reach `Verified`, so it cannot ship (§2.4). The genealogy is honest:
   *we made a part, we could not identify it, here is when and why.*
4. **Quarantine position full** → the cell reports `Degraded` and stops taking new
   work. Better a stopped cell than an unmarked pile.

**⚠ FLAGGED (§7.3):** mark location per product (a drawing-driven field ABERP does
not carry — the same gap ADR-0199 §C2.3 names for drawing number/revision), and
whether an unmarkable part is auto-NCR'd (ADR-0090) or merely quarantined.

### 2.6 The genealogy event model

**Where scans happen:** post-machining (in-cell verify, ④), inspection bench,
dispatch/packing, plus goods-in and quarantine. Each scanner is a **station**;
the station identity is already encoded in the adapter's `scanner_id` — the MES
framework made that call deliberately (`events.rs:82-87`: *"the station identity
is encoded inside `scanner_id`"*).

#### The one decision that matters: genealogy is regulated state, not telemetry

Every MES adapter event today lands as one generic `EventKind::MesAdapterEvent`,
and that path is **documented as lossy**. From `event_kind.rs` itself, describing
why inventory refused to ride it:

> stock movements are **regulated state**, not adapter telemetry, so they emit a
> distinct EventKind rather than riding on `EventKind::MesAdapterEvent` (which is
> subject to broadcast lossiness per ADR-0060 §"Consequences" #4 — losing a stock
> movement means the cache drifts and inventory is wrong).

**A dropped genealogy scan is a hole in an AS9100 pedigree.** It is the same class
of loss, with worse consequences, so it gets the same answer: **a dedicated
regulated write path**, not the broadcast. Concretely:

- A **`part_genealogy_events` table**, written through the shared
  `aberp_db::Handle` inside a transaction, with the audit append in **the same
  tx** — the `record_inspection` template (`crates/aberp-qa/src/qc/inspections.rs`),
  which is the codebase's proven atomic firing-site pattern. **Not** the
  AVL pattern (business-commit then audit-append in a second transaction).
- A new **`genealogy.*` EventKind family**, a new prefix for the reason ADR-0092
  gives when it created `qc.*`: keep every existing prefix consumer's glob narrow.
- `ScanReceived` on the broadcast stays exactly as it is — it remains the *live
  dashboard* signal, and losing one of those costs nothing. The genealogy row is
  written by the resolver, not by the broadcast consumer.

#### Row shape

```
part_genealogy_events
  pge_id            pge_<ULID>            natural PK, no surrogate
  tenant_id
  part_uid          nullable              NULL ⇔ the scan did not resolve
  raw_payload       bounded excerpt       capped; the barcode adapter already caps at 4096
  symbology         nullable              from the AIM ID prefix
  station_id                              = scanner_id
  station_role                            closed vocab (below)
  resolution                              resolved | unknown_code | ambiguous | malformed
  state_from / state_to                   the lifecycle transition, closed vocab
  occurred_at_utc                         device-reported — UNTRUSTED
  recorded_at_utc                         ABERP's clock — the ordering key
  actor                                   Adapter(name) | Operator(login) | System
  wo_id / dsp_id    nullable              resolved at write time, snapshotted
```

#### Five rules, all in code

1. **Every scan lands a row — including the ones that fail to resolve.** An
   unknown code at the dispatch bench is a *finding*, not noise: it means an
   unmarked part, a foreign part, or a mis-keyed record. Dropping it is the
   failure mode. This mirrors `qc.probe_ingestion_failed`, which is already a Live
   fail-loud emitter.
2. **The station→state map is code; the station→role map is config.** An operator
   configures *which scanner sits at the dispatch bench*. An operator does not get
   to decide *what scanning at dispatch means*. Closed role vocab:
   `InCellVerify | PostMachining | Inspection | Dispatch | GoodsIn | Quarantine`.
3. **Device time is untrusted.** Order by `recorded_at_utc`; display
   `occurred_at_utc` and show the skew when it is large. A scanner's clock is
   whatever it was set to.
4. **Debounce, because scanners fire repeatedly.** A scan that would produce the
   identical `(part_uid, station_role, state_to)` as that part's most recent event
   within `GENEALOGY_DEBOUNCE` collapses into it. **⚠ FLAGGED (§7.5):** default
   60s. Too short and the log fills with duplicates; too long and a legitimate
   re-scan after rework is swallowed.
5. **Bound every vendor-supplied string.** The Trumpf adapter's review already
   caught an uncapped vendor `work_order_id` reaching the ledger. Same rule here:
   cap the excerpt, never let a device dictate a row size.

#### An illegal transition is an event, not an error

Scanning a part at *dispatch* that was never `Verified`, or scanning a
`Scrapped` part anywhere, records the attempt with `resolution=resolved` and a
**rejected** transition, and refuses the state change. The refusal is the audit
value: it is evidence the shop's controls fired. (Same posture as
`part.wo_blocked_no_uid` and `ncr.wo_blocked_by_open_ncr`.)

### 2.7 How the UID threads the whole flow

One key, seven joins. Everything in this table exists today **except the two
marked NEW** — which is the precise measure of how small the missing spine is.

| Link | Join | Where |
|---|---|---|
| UID → **order / customer** | `wo_part_marks.wo_id` → `work_orders.source_quote_id` → quote `buyer_partner_id` → `partners` | `trace_part_uid` (`part_marking.rs:471`) — Live |
| UID → **material heat / MTR** | `wo_part_marks.heat_lot_reference` → `inventory_balances.{heat_lot_number, mill_test_report_url}` | ADR-0085 — Live |
| UID → **machine + NC program** | `qc_inspections.{machine_identifier, program_id}` snapshotted at ingest | `program_id` added by ADR-0199 §D3(b) |
| UID → **probe actuals** | `qc_inspections.linked_part_uid` | **NEW — column exists, nothing populates it** |
| UID → **QC report** | `qc_report_lines` per serialised unit | ADR-0199, design |
| UID → **shipment** | `qc_reports.dsp_id` bound inside `mark_shipped`'s tx | ADR-0199, design |
| UID → **every physical touch** | `part_genealogy_events.part_uid` | **NEW — this document** |
| UID → **audit** | hash-chained ledger, durable-acked, mirrored | ADR-0110/0111 — Live |

**The single missing join is measurement → unit**, and it is the spine's first
code task. Under mint-at-release (§2.4) it is nearly free: the cell order already
knows the UID, so the DPRNT header carries it and `linked_part_uid` is populated
at ingest, with `RawProbeEvent.part_serial` as a **cross-check**, not the source.
If the control's reported serial and the cell order's UID disagree, that is a
`qc.probe_ingestion_failed` — fail loud, never pick one.

---

## 3. Cycle-time and probe capture — MC Connect and DPRNT

*(Numbered ⑥ in §1.1; called out separately because it is where the two data
channels get confused with each other.)*

### 3.1 Two channels, two different jobs

| | MTConnect | DPRNT-to-file |
|---|---|---|
| **Answers** | *what is the machine doing?* | *what did the probe measure?* |
| Carries | Execution state, program name, spindle, alarms, timestamps | numeric actuals per touch |
| Shape | XML, structured, standardised | **unstructured text — we define the format** |
| Status | `/current` **Live**; `/sample` = [D-16](BACKLOG-designed-to-live.md#d-16) slot 2 | nothing exists |
| Feeds | cycle visibility, machine state, the "no operator was present" timeline | `qc_inspections` actuals |

**Do not try to get the measurements out of base MTConnect.** ADR-0092 already
established why: there is no `ProbeData` item, the `Probe` SAMPLE subType is
deprecated, and the nominal/tolerance/verdict triple lives only in QIF assets or
OPC UA GMS — both future-tier. MTConnect gives you the *value* at best, and only
if the agent is configured to expose it.

### 3.2 We own the DPRNT format

The shop's own Renishaw research reached the operative conclusion:

> treat Inspection Plus output as unstructured text the CNC emits — there is no
> documented structured format that's portable across controls
> (`docs/research/stage3/02-renishaw-quality-gate.md:36`)

Most shops read that as "so it's unusable." For a vertically-integrated shop that
writes its own probe macros and post-processor, it is the opposite: **nobody else
defines the format, so we do.** Proposal — **`ABERP-PROBE v1`**, a fixed,
versioned, line-oriented format the macro emits and ABERP parses:

- A **header line** carrying the format version, `part_uid`, `wo_id`, `program_id`,
  `machine_id`, `cycle_id`, and the probe serial + last calibration.
- **One line per touch**: characteristic id (or feature name), actual, units.
- A **terminator line** with a touch count.

And four parser rules, all fail-loud:

1. **No terminator ⇒ ingest nothing.** A file still being written by the control
   is a torn file; a partially-ingested probe run is a fabricated inspection.
2. **Unknown version ⇒ reject the file, emit `qc.probe_ingestion_failed`.** Never
   best-effort a format you do not recognise.
3. **Count mismatch ⇒ reject.** The terminator's count is the checksum.
4. **Units mismatch against the plan ⇒ reject**, never coerce (ADR-0092 rule 2,
   already built).

Implementation shape: a **third `ProbeIngestionSource`** — `DprntFileSource` —
beside the two `todo!()` ones (`crates/aberp-qa/src/qc/probe.rs:95,:128`). The
cursor semantics the trait already defines map cleanly onto a watched directory
(file name = cursor). **And it is the only probe transport that can be built and
tested today, against a fixture file, with no machine present** — which is why
§5 puts it in Phase 0.5 rather than waiting on hardware.

**⚠ FLAGGED (§7.6):** the FANUC control needs the data-server / Ethernet option
for DPRNT-to-file (or a serial-to-Ethernet bridge), and the share ABERP watches
is a CUI-handling surface ([D-08](BACKLOG-designed-to-live.md#d-08)).

---

## 4. Making the "no operator was present" claim true

Step ⑨ says the audit retains *"no-operator"* as evidence. Two observations, one
comfortable and one not.

**The comfortable one.** ABERP already refuses to let an adapter impersonate a
human: `qc_inspections` distinguishes `QcSource::Manual` from adapter ingestion,
and ADR-0063 anticipated `decide_qa(actor: Adapter(name))`. The genealogy row's
`actor` field (§2.6) follows the same pattern. So "this was done by a machine" is
recordable in the domain today.

**The uncomfortable one.** *Cryptographically*, a lights-out event has no human
root of trust yet. ADR-0088 is the design for exactly this — a per-tenant service
identity, keychain-persisted, endorsed once by an operator at login, timestamp-
anchored — and it is **Proposed, not built**, and its anchoring leg depends on
ADR-0087 + NETLOCK ([D-06](BACKLOG-designed-to-live.md#d-06)).

So state the truth in the report and in this document: **today the chain proves
*integrity and order* (hash-chained, durable-acked, mirrored) and records *which
adapter* acted. It does not yet prove *origin* cryptographically for unattended
writes.** That is honest, it is enough for AS9100 record-keeping, and ADR-0088 is
the named upgrade. Do not let a QC report imply more than the chain delivers.

---

## 5. Phasing — the digital spine is real before the machine lands

The organising principle: **every phase must leave the shop with something that
works.** Nothing here is staged so that value arrives only at the end.

### Phase 0 — traceability and reporting, with no new hardware *(now)*

Ships against a **Zebra label printer and a TCP barcode scanner** — both already
Live adapters. The mark is a **printed Data Matrix label**, not yet peened. Every
piece of the spine is exercised end to end; only the marking *device* is a stand-in.

1. **UID minted at WO release** + the `wo_part_marks` lifecycle (§2.4), and the
   part-UID shipment gate re-pointed at `Verified` — **together, in one change**.
2. **`MarkPayloadKind` enum + `render_mark_payload` + `resolve_scan`** (§2.3),
   default `AberpPointer`, legacy composite parsed for compatibility.
3. **`part_genealogy_events` + the `genealogy.*` kinds + the resolver chokepoint**
   (§2.6) — on the regulated write path, not the broadcast.
4. **The first `print_zpl` caller in the repo's history**: ZPL `^BX` renders a
   Data Matrix natively, so the label is a few hundred bytes of ZPL and no image
   library. The Zebra adapter is untouched.
5. **Scan-after-print is the Phase-0 verify** — the same read-back rule (§2.5),
   with a human presenting the label instead of a robot presenting the part. The
   verify *logic* ships now and never changes.
6. **QC report + CoC (ADR-0199 Phase 1)** on manual/entered actuals, keyed by UID.
7. **Audit retention** — nothing new; the rows ride the existing `Handle`.

**What the shop can do at the end of Phase 0, with no machine:** serialise every
part, mark it, scan it at three stations, see its full genealogy, print a QC
report and a CoC keyed to the mark, attach both to a shipment, block a shipment
that is missing any of it, and hand an auditor a hash-chained trail. **That is
the whole workflow except the metal.**

### Phase 0.5 — the real marker, and the probe format *(no machine either)*

8. **`MarkerAdapter`** — a dot-peen marker on the Zebra seam: raw TCP command
   set, health probe, bounded payload, auto-reconnect. The label path stays as
   the fallback and the test double.
9. **`DprntFileSource`** (§3.2) — built and tested against fixture files. The
   format contract is agreed with whoever writes the probe macros **before** the
   machine arrives, which is the entire reason to do it now.

### Phase 1 — the cell *(machine + robot present)*

10. **Cell orchestration.** ⚠ This is the biggest single gap: `ur_rtde` is
    **read-only** (`ur_rtde.rs:99-103` — RTDE inputs and the Dashboard server on
    port 29999 are both explicit non-goals). Orchestrating a cell needs a
    **command path**. See §6.1 — this is a decision, not a task.
11. **In-cell mark + read-back verify** — the Phase-0 logic, now driven by the
    robot and a fixed DPM reader, with the quarantine branch.
12. **MTConnect `/sample` + gap-safe cursor** ([D-16](BACKLOG-designed-to-live.md#d-16)
    slot 2) for cycle visibility; `/probe` capture to confirm what the NTX's agent
    actually exposes ([D-02](BACKLOG-designed-to-live.md#d-02) research gap #1).
13. **`linked_part_uid` auto-populated** from the cell order, cross-checked
    against the control's reported serial.
14. **ADR-0199 Phase 2** — actuals arrive by transport instead of keyboard, and
    **no report code changes**. That is the test of whether the seam was drawn in
    the right place.

### What is hardware-blocked and what is not

| Blocked on hardware | Not blocked — buildable today |
|---|---|
| Cell orchestration (⑩) | UID lifecycle + gate (①) |
| In-cell mark/verify (⑪) | Payload variants + resolver (②) |
| MTConnect `/sample`, `/probe` (⑫) | Genealogy events (③) |
| Auto-populated actuals (⑬⑭) | ZPL Data Matrix + verify logic (④⑤) |
| | QC report + CoC (⑥), audit retention (⑦) |
| | Marker adapter (⑧), DPRNT parser (⑨) |

**Nine of fourteen items need no machine.** That is the case for building the
spine now.

---

## 6. Gaps this document opens deliberately

### 6.1 The cell has no command path *(the big one)*

Every hardware adapter in the tree is **read-only or fire-and-forget**. `ur_rtde`
subscribes to seven RTDE output variables and explicitly does not do RTDE inputs
or the Dashboard server. `MtconnectAdapter` polls. `ZebraAdapter::print_zpl` is
the *only* outbound call in the whole framework, and nothing calls it.

"Lights-out cell orchestration" means something has to **command**: load program,
start cycle, move to marker, fire marker, present to reader, place to rack. Three
architectures, and the choice is Ervin's:

- **(A) ABERP orchestrates.** ABERP drives the robot (RTDE inputs + Dashboard
  server) and sequences the cell. Maximum vertical integration, maximum blast
  radius: ABERP becomes safety-adjacent real-time software, and a stall in a
  business app becomes a stall in a cell holding a part.
- **(B) The cell orchestrates itself; ABERP issues *orders* and consumes
  *events*.** A UR program (or a small cell controller) runs the sequence; ABERP
  posts a **cell order** ("make unit `dp-…` from WO `wo_…`, program `O1234`") and
  receives completion/verify events. **Recommended.** It keeps ABERP as the system
  of record, keeps real-time control where real-time control belongs, and matches
  every seam already built (ABERP observes; devices act).
- **(C) A third-party cell controller / FMS.** Fastest, and it is precisely the
  vendor lock-in the codebase has refused five times (ZPL over vendor SDK, RTDE
  over PolyScope SDK, MTConnect over the DMG MORI IoTconnector API).

Under (B) the only new ABERP surface is a **cell-order queue** with a status
callback — a small, testable, non-real-time thing. ⚠ FLAGGED (§7.7).

### 6.2 The cell is a single point of head-of-line failure

A lights-out cell is a queue, and this codebase has already been bitten by one:
a missing CAD artifact on the oldest in-flight pricing job silently wedged the
entire quote pipeline (bare `?` instead of an emitted failure, no reaper, a
cycle-wide `break`, and the error never audited). Every one of those four
mistakes is available to a cell queue. Design in the reaper, the per-item
failure emission, and the `continue`-not-`break` from the start.

### 6.3 Drawing number and revision still do not exist

ADR-0199 §C2.3 names it as its largest data gap; §2.5 above adds *mark location*
to the same missing per-product drawing record. Both want one small
`part_drawing_refs`-shaped surface with revision history. One gap, two consumers —
do it once.

### 6.4 The "no operator" claim outruns the crypto

See §4. ADR-0088 is Proposed; NETLOCK anchoring is [D-06](BACKLOG-designed-to-live.md#d-06).

### 6.5 Event-kind arithmetic will collide

This document implies a new `genealogy.*` family (roughly 6–8 kinds) on top of
ADR-0199's `qcr.*` (6). Both branches will bump `ALL_KINDS_COUNT` against a
moving base. **Do not trust any absolute count in either document — recompute at
merge.** The count is `Self::ALL_KINDS.len()` with double-entry pins, so a stale
number fails loudly rather than silently; that is the good case, but it will fail.

---

## 7. Open decisions — flagged, not resolved

Each has a defensible default so that nothing below blocks implementation.

| # | Decision | Default proposed | Why it matters |
|---|---|---|---|
| **7.1** | Is a **US DoD deliverable** ever in scope? | **No** for the pilot ⇒ `AberpPointer` | If yes, MIL-STD-130 mandates a 15434/UII payload and [D-03](BACKLOG-designed-to-live.md#d-03) (IAC + enterprise id) becomes blocking, not optional. Same question for a **CAGE code** and ATA Spec 2000. |
| **7.2** | **Mark quality**: DPM grade, verification cadence | ISO/IEC TR 29158 **grade C**, in-cell read-back every part, graded verification on a sample | AS9132 sets the process spec; a customer may impose a stricter grade or 100% graded verification. |
| **7.3** | **Mark location** per product, and mark **depth/force** by material | Per-product master data alongside drawing number; conservative depth by material class | Peening a thin wall or a fatigue-critical surface is a *part defect*. Some prints forbid marking in named zones. This is an engineering call, not a config. |
| **7.4** | **Pointer vs composite** payload (§2.2) — this reverses ADR-0089's shipped format | **Pointer** | Free to change today (nothing physically marked), expensive after the first real mark. Needs an explicit yes. |
| **7.5** | **Genealogy debounce window** | 60s on identical `(uid, station_role, state_to)` | Too short: duplicate spam. Too long: a legitimate re-scan after rework is swallowed. |
| **7.6** | **DPRNT transport**: does the NTX's FANUC have the data-server/Ethernet option? Where does the file land? | Watched share, `ABERP-PROBE v1`, CUI-handled | Determines whether §3.2 is a directory watcher or a serial bridge. Ties to [D-08](BACKLOG-designed-to-live.md#d-08). |
| **7.7** | **Cell orchestration architecture** (§6.1) | **(B)** ABERP posts cell orders; the cell sequences itself | Decides whether ABERP takes on real-time/safety-adjacent responsibility. The most consequential item on this page. |
| **7.8** | **Marker procurement constraint** | Buy only a marker with a documented **TCP/ASCII or Modbus-TCP** command set; refuse Windows-DLL-only | This is the rule that produced ZPL over a vendor SDK and RTDE over PolyScope. It has to be applied *before* the purchase order, not after. |
| **7.9** | **Unmarkable part disposition** | Quarantine + `part.mark_verify_failed`; auto-NCR **off** by default | An auto-NCR per marker misfeed floods the NCR queue; no NCR risks a silent quality signal. |
| **7.10** | **Scrapped-UID reuse** | **Never reuse.** A retired UID stays retired | Reuse makes the genealogy ambiguous forever. Cheap to promise now. |
| **7.11** | **In-process probe vs final probe** | Both, one plan; ADR-0113 prices the post-machining routine | An in-process touch that drives an offset is *process control*; a final touch is *acceptance*. Only the second belongs on a QC report — but both are worth recording. **Needs a distinction ADR-0092 does not currently draw.** |
| **7.12** | **Genealogy retention** | Same as the QC report — nothing deletes | Aerospace retention is contract-specified (7–40 years, sometimes life-of-type). Same open question as ADR-0199 §6. |

---

## 8. Summary

- **The flow is nine steps and the seams for eight of them exist.** Quote, order,
  cell, mark, scan, capture, report, ship, audit — every one lands on a Live or
  designed surface.
- **The missing piece is the middle of the mark→scan→measurement join**, and it is
  small: a UID lifecycle, a payload variant enum with one resolver, a genealogy
  event table on the regulated write path, and the first caller of a printer that
  has been sitting supervised and unused since S245.
- **The mark is a pointer.** The part carries identity; ABERP carries truth; the
  document carries the human-readable copy.
- **Mark-then-verify is the rule that makes a lights-out cell honest.** Everything
  else in the spine is bookkeeping; this is the part that stops the cell from
  quietly producing anonymous parts.
- **Nine of fourteen work items need no machine.** Build the spine on labels now,
  swap in the peen and the cell later, and change no report code when it lands.
