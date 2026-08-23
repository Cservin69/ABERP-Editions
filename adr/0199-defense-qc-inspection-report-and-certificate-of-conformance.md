# ADR-0199 (PROVISIONAL NUMBER) — Defense edition: dimensional inspection reports + Certificate of Conformance, attached to the shipment and retained audit-grade

> ## ⚠️ NUMBER PROVISIONAL — assign unique at merge
>
> **`0199` and `D-99` are deliberate placeholders, not a claim on the sequence.**
> Multiple unmerged branches have already collided on ADR/D-numbers:
> the internal-portal ADR (ADR-0115 / D-20), the auto-probe pricing ADR
> (ADR-0113 / D-20, branch `docs/adr-auto-probe-inspection`), and the
> pricing-queue head-of-line fix (D-20, branch `fix/pricing-queue-head-of-line`).
> The highest ADR on `origin/main` at the time of writing is **0112**; the
> highest backlog id is **D-19**. Whoever merges this **must** renumber the
> file, the in-file references, the backlog anchor (`d-99`), and the
> cross-links in both directions. `0199` is chosen far enough out of band that
> it cannot be mistaken for a real allocation.
>
> ### Merge reconciliation — measured at implementation time (2026-08-23)
>
> The three sibling branches were diffed against `main` while implementing
> this, so the merge cost is known rather than guessed:
>
> - **`docs/adr-auto-probe-inspection` is DOCS-ONLY.** It touches no `.rs`
>   at all — only its own ADR and `docs/BACKLOG-designed-to-live.md`. The
>   sole conflict with this branch is the backlog file and the D-number.
> - **`fix/pricing-queue-head-of-line` adds TWO event kinds**
>   (`quote.pricing_cycle_outcome`, `quote.pricing_job_retried`) and pins
>   `ALL_KINDS_COUNT == 189`. This branch adds six and pins `193`. **Merged
>   together the correct pin is 195**, in all three places that carry it:
>   `event_kind.rs`'s `all_kinds_count_is_pinned`, the `const _` in
>   `aberp-verify::verify.rs`, and the `const _` in
>   `export_invoice_bundle.rs`. Do not trust either branch's number.
> - **`serve.rs` does not actually collide.** The pricing-queue hunks are in
>   the quote-pricing handler region (~line 23 900); this branch touches
>   `build_router`'s tail, the shipment-gate block (~17 500) and a new
>   handler block (~26 800). Different regions — but re-read the merged
>   `build_router` anyway, since both branches add routes.
> - The real three-way conflicts are `event_kind.rs`, `aberp-verify/verify.rs`,
>   `export_invoice_bundle.rs` (each: variant list + exhaustive match + count
>   pin) and `docs/BACKLOG-designed-to-live.md`.

- **Status:** **Accepted — Phase 1 implemented** (2026-08-23). Ervin accepted the specification and **every flagged decision at its conservative default**, and confirmed two of them explicitly: **AS9102 Rev C is the default FAIR form**, and **an incomplete report BLOCKS a Defense shipment** (Q3 = yes, block). See *Open questions / decisions flagged for Ervin* below — each entry now carries its RESOLVED verdict. The design pass that authored this file wrote no code; the Phase-1 implementation session that followed wrote the code described in §Phasing → Phase 1 and is recorded in §"Phase 1 — what actually landed".
- **Date:** 2026-08-22
- **Deciders:** Ervin — verbatim direction: *"every part is auto-probed no-touch on a DMG MORI NTX; the probe measured values must land in ABERP so that ABERP will print the QC reports attached to shipments, also part of the aerospace audit."*
- **Base:** Editions `origin/main` @ `9e4a6ee`. Every file:line below was reproduced in this session at that SHA.
- **Grounds (read before implementing):**
  - **ADR-0092** (on-machine probe ingestion → QC inspections → auto-NCR) — **implemented as S443**, not just proposed. `qc_inspection_plans` + `qc_inspections` + the pure verdict + the `ProbeIngestionSource` trait are all in the tree. This ADR is a *reporting* layer on top of them; it invents no second measurement model.
  - **ADR-0113** (100% automated on-machine probing priced into every Defense quote — branch `docs/adr-auto-probe-inspection`, commit `3058b20`, **not on `main`**). This ADR is its downstream sibling: 0113 prices the probe cycle, this one turns the resulting measurements into the customer-facing and audit-facing record. Neither depends on the other's code.
  - **ADR-0097** (machining-tolerance cost driver) — the professional tolerance taxonomy (ISO 2768 class / ISO 286 IT grade / explicit ±) and per-feature critical callouts. This is where a *quoted* tolerance lives; §D3 below states precisely why the report does **not** read it.
  - **ADR-0064** (dispatch board) + **ADR-0089 / ADR-0090** (part-UID and open-NCR shipment gates) — the shipment record this attaches to, and the exact gate template §D6 reuses.
  - **ADR-0085** (heat/lot traceability), **ADR-0089** (part UID marking + `trace_part_uid`), **ADR-0029** (`aberp-verify` evidence bundle), **ADR-0087** (timestamp-anchored chain), **ADR-0110 / ADR-0111** (durable ack + checkpoint under the writer lock).
  - **ADR-0093** (product-line saw-off — the compile-time `Edition` binding and the `storefront_polling_allowed_for` capability-gate pattern reused verbatim in §D8).
  - `[[trust-code-not-operator]]`, `[[hulye-biztos]]`, `[[no-sql-specific]]`, FOUNDATION.md §2 (pure domain).
- **Scope guard:** Authored in the **ABERP-Editions** tree. **Defense only.** The **Portable edition is frozen** and must be provably byte-identical after this lands. The frozen prod line (`ABERP.git`, `PROD_v2.27.76`) is never a build target.
- **Companion backlog entry:** [D-99 (PROVISIONAL)](../docs/BACKLOG-designed-to-live.md#d-99).

---

## Context

### C0 — Ervin's direction, restated as a specification

The chain Ervin named, end to end:

```
quote (auto-probe priced — ADR-0113)
  → make (no-touch cell + probe cycle)
    → CAPTURE (probe measured values land in ABERP — ADR-0092, ALREADY BUILT)
      → REPORT  ← THIS ADR
        → ATTACH to the SHIPMENT
          → RETAIN in the AUDIT trail for AS9100 audits
```

Three of those five links exist in the tree today. The two that do not are
**REPORT** and **ATTACH** — and "retain" exists as infrastructure but has
never been pointed at a QC document.

### C1 — What already exists (verified at `9e4a6ee`, cited by file)

**The measurement record is real and shipped.** ADR-0092 is not a paper
design: `crates/aberp-qa/src/qc/` is 1 357 lines of implemented code.

| Piece | Where | State |
|---|---|---|
| `qc_inspection_plans` (nominal + upper/lower tol + units per `(tenant, product_id, feature_name)`) | `crates/aberp-qa/migrations/V002__qc.sql:22` | **Live** |
| `qc_inspections` (one row per measured feature; plan values **denormalised as a snapshot**) | `V002__qc.sql:47` | **Live** |
| `compute_verdict` — pure `Pass / Minor / Major / Critical / CalibrationStale` over the *half-width* overage ratio, calibration-stale checked first | `crates/aberp-qa/src/qc/verdict.rs` | **Live** |
| `record_inspection` — the single write chokepoint: verdict + row + audit events **in the caller's tx** | `crates/aberp-qa/src/qc/inspections.rs` | **Live** |
| `ProbeIngestionSource` trait + `RawProbeEvent` + `ProbeCursor`; `MockProbeSource` works, `MtconnectProbeSource` / `RenishawCentralSource` are `todo!()` | `crates/aberp-qa/src/qc/probe.rs` | **Trait Live, transports stubbed ([D-02](../docs/BACKLOG-designed-to-live.md#d-02))** |
| `qc.*` audit family (6 kinds: recorded / passed / failed / auto-NCR / calibration-stale / ingestion-failed) | `crates/audit-ledger/src/entry/event_kind.rs:3086` | **Live** |
| HTTP: `GET|POST /api/qc-inspections`, plan CRUD, `GET /api/qc-stale-calibration` | `apps/aberp/src/serve.rs:4595-4612` | **Live** |
| Manual actual-value entry (`QcSource::Manual`) | `qc/inspections.rs` | **Live** |

**The traceability spine is real.** `wo_part_marks` carries `part_uid`,
`serial_number`, `data_matrix_payload`, `heat_lot_reference`,
`marked_at_utc`, `marked_by_operator`
(`apps/aberp/src/part_marking.rs:177`); `trace_part_uid` already resolves
part UID → WO → source quote → customer partner
(`part_marking.rs:471`); `MaterialTraceabilitySeed` carries
`lot` / `heat` / `mill_cert_id` / `country_of_origin` / `melt_date`
(`crates/aberp-compliance/src/lot_heat/mod.rs:164`).

**The shipment record is real, and it already has two gates.**
`aberp-dispatch` (`Drafted → Shipped | Cancelled`), `mark_shipped` doing
stock movement + invoice spawn + the three `export.*` audit appends in
**one transaction** (`crates/aberp-dispatch/src/repository.rs:530`), plus
two shipment gates at the route with an identical shape:

```
resolve_part_uid_gate(conn, tenant, &dispatch) -> PartUidGate    (serve.rs:17250, pure)
enforce_part_uid_gate_for_shipment(state, dsp_id, login)          (serve.rs:17286, 409 + audit)
resolve_open_ncr_gate(conn, tenant, &dispatch) -> OpenNcrGate     (serve.rs:17384, pure)
enforce_open_ncr_gate_for_shipment(...)                            (serve.rs:17427, 409 + audit)
```

Both are **defense/aerospace-only** (`CustomerType::Defense | Aerospace`
off the dispatch's partner), both return `Pass` on an unknown partner or a
missing WO, and both emit one denial audit row through the shared `Handle`
writer before the 409. This is the template §D6 reuses verbatim.

**The PDF precedent is render-on-demand, not stored-bytes.**
`aberp-invoice-pdf` and `aberp-quote-pdf` are both **pure renderers** —
"no clock, no I/O, no RNG, no async. Same inputs ⇒ byte-identical output"
(`crates/aberp-quote-pdf/src/lib.rs:12-18`). `GET /api/invoices/:id/pdf`
re-renders from the ledger on every request
(`apps/aberp/src/print_invoice.rs:148`, route at `serve.rs:4283`). **No PDF
bytes are persisted anywhere in this repo.** The only persisted binary
artifact is the AP NAV XML, written to
`~/.aberp/<tenant>/ap-artifacts/<id>.xml` with its SHA-256 stamped into the
audit payload and the bytes deliberately kept *out* of the chain
(`apps/aberp/src/incoming_invoices.rs:448-536`).

**The evidence bundle is real.** `aberp-verify` reads a tar of
`bundle/manifest.json` + `bundle/chain.jsonl` + `nav/…`, and **fails loud
on any file outside that set** — *"tar entry is neither manifest.json,
chain.jsonl, nor under nav/ — bundle shape divergence per ADR-0029 §3"*
(`crates/aberp-verify/src/bundle.rs:120`). Any new artifact class must
extend that allow-list explicitly; it cannot be smuggled in.

### C2 — What is verifiably absent

Four independent sweeps over `*.rs` / `*.sql` / `*.svelte`:

1. **No report entity of any kind.** No `qc_report*` table, no CoC, no FAIR, no
   renderer, no route. The word "report" in a QC sense appears only as
   `PartTraceReport` (a lookup result, not a document) and
   `aberp_verify::Report` (a check list).
2. **No characteristic numbering.** `qc_inspection_plans` is keyed by
   `feature_name` (free text). There is **no balloon number, no key-characteristic
   flag, no characteristic type, no inspection method, no required/optional
   distinction** — every field AS9102 Form 3 requires per characteristic.
3. **No drawing number and no drawing revision anywhere in the repo.**
   `work_orders` carries `product_id` and nothing else identifying a drawing
   (`crates/aberp-work-orders/migrations/V001__work_orders.sql:29-43`). A grep for
   `drawing_number|drawing_rev` returns zero product hits. AS9102 Form 1
   fields 6–7 cannot be filled from today's data. **This is the single
   largest data gap this ADR opens.**
4. **No link from a measurement to a serialised unit or to a shipment.**
   `qc_inspections` links to `linked_wo_id` / `linked_part_uid` /
   `linked_heat_lot`, but `linked_part_uid` is never populated by the manual
   route today, and there is no `dsp_id` column. `RawProbeEvent` carries no
   serial and no characteristic number.

### C3 — Why the report cannot be a live view over `qc_inspections`

A QC report is a **compliance record**, in the same class as an issued
invoice: once it goes out the door attached to a shipment, what it said at
that moment is the fact. `qc_inspection_plans` rows are mutable
(`update_plan`, `archive_plan` — `qc/plans.rs:157/198`). A report rendered
live would silently change its own history the first time an operator
edits a tolerance. The `qc_inspections` table already made this exact call
and documented it — *"Plan nominal/tol/feature/units are DENORMALISED
(snapshot) so the row records what it was actually measured against even if
the plan later changes — an audit/traceability requirement"*
(`V002__qc.sql:41-46`). The report layer inherits that discipline one level
up.

---

## Decision

**Add a Defense-only `qc_reports` / `qc_report_lines` snapshot record and a
pure `aberp-qc-pdf` renderer that emits three document shapes from one
record — a per-shipment Dimensional Inspection Report, a Certificate of
Conformance, and (Phase 1b) an AS9102 First Article Inspection Report,
Forms 1/2/3. The report is bound to a dispatch inside `mark_shipped`'s
existing single transaction, gated at the ship route by a third gate
mirroring the two that exist, and retained by pinning the SHA-256 of the
rendered bytes into the hash-chained audit ledger rather than by storing the
bytes. Measured actuals arrive through a batch extension of the already-built
`ProbeIngestionSource` interface, so Phase 1 is fully functional on today's
manual entry and Phase 2 is a transport swap.**

### D1 — Standard and document structure

**Three shapes, one record.** All three render from the same
`qc_reports` + `qc_report_lines` snapshot; they differ only in layout and
in which blocks they print.

| Shape | When | Contents |
|---|---|---|
| **`DimensionalInspection`** (the default; the per-shipment doc Ervin asked for) | Every Defense/Aerospace shipment | Header (report no., date, customer, PO/quote, WO, drawing + rev, qty). One row **per characteristic per serialised unit**: balloon #, characteristic name, type, nominal, tol −/+, **actual measured**, deviation, units, method, verdict, pass/fail. Traceability block. Overall **disposition**. Signature block. |
| **`CertificateOfConformance`** | Alongside every shipment (default on) | One page: the conformance statement, part number + drawing rev, qty, serial range, WO, heat/lot + mill cert ref, applicable specs, the QC report number it certifies against, disposition, signature block. **No characteristic table.** |
| **`As9102Fair`** (Phase 1b) | First article of a part number / after a change that resets FAI | **Form 1** part number accountability; **Form 2** product accountability — materials, special processes, functional testing; **Form 3** characteristic accountability — one row per characteristic with designator, requirement, results, and the tooling/inspection method. |

**Rationale.** AS9102 is the correct industry baseline and the safe default
— it is the standard a prime will name if it names one, and its Form 3
characteristic-accountability model is a strict superset of what a
per-shipment dimensional report needs, so building the data model to Form 3
means the per-shipment report is a projection of it rather than a second
model. But a FAIR is by definition a **first-article** event, not a
per-shipment one: AS9102 requires a FAI on the first production part and on
defined change/lapse triggers, *not* on every delivery. Making the FAIR the
per-shipment document would be both wrong and expensive. So the
per-shipment document is the Dimensional Inspection Report + CoC (the pair
a prime's receiving inspection actually expects in the box), and the FAIR is
a separate shape generated on demand from the same characteristics.

**⚠️ FLAGGED — confirm-later default.** AS9102 **Rev C** form layout is the
default. Rev B is still contractually named by some primes, and
Nadcap-accredited special processes and individual primes (Boeing, Airbus,
Safran, Leonardo…) mandate their own forms on top. The data model is
designed so a template swap is a rendering change, never a schema change —
see D2.

**Rejected alternatives.**

- *FAIR as the per-shipment document.* Wrong lifecycle event (see above);
  would make every delivery carry a first-article ceremony.
- *A free-text "QC notes" field on the dispatch.* Not a record. Not
  attributable, not verifiable, not accountable per characteristic — the exact
  clipboard-and-Excel failure ADR-0092 exists to end.
- *ISO 9001 §8.6-only release note (no characteristic table).* Adequate for
  commercial work, inadequate for aerospace: AS9100D §8.6 requires evidence of
  conformity to acceptance criteria, which means numbers.
- *Emitting a QIF (Quality Information Framework) XML instead of a PDF.*
  QIF is the right *interchange* format and is named as a future tier
  (ADR-0092 already names QIF assets as the MTConnect Part 4.4 route). It is
  not what goes in the box with the parts, and no customer here has asked for
  it. **Named as a v-future export, not built.**

### D2 — Customer-specific templating, without a template engine

**`QcReportTemplate` is a closed-vocab enum**, resolved per customer:

```rust
pub enum QcReportTemplate {
    AbenStandard,      // house dimensional-inspection layout (default)
    As9102RevC,        // Forms 1/2/3
    CocOnly,           // certificate of conformance, no characteristic table
}
```

Resolution order, most specific first:
`partners.qc_report_template` (new nullable column) → tenant default
(`quoting_parameters`-style singleton) → `AbenStandard`.

**Rationale.** A prime that mandates its own form gets a new enum variant
and a new render function in the same PR that acquires the form — the same
posture `CarrierKind` takes on Hungarian carriers (*"operators who use a
sixth Hungarian carrier submit a PR to extend the enum"*,
`crates/aberp-dispatch/src/types.rs:70-74`).

**Rejected: a real template engine (Tera / Handlebars / operator-uploaded
layouts).** A compliance document assembled from operator-editable templates
is a falsification surface: an operator who can edit the template can hide a
failing characteristic, and nothing in the audit chain would show it. The
renderer must be **pure and golden-tested** so that the same rows always
produce the same bytes and a diff in the output is a code change with a
review attached. This is `[[trust-code-not-operator]]` applied to the
document itself, and it is *why* the SHA-256 pin in D7 means anything.

### D3 — Data model

Everything below is **additive**. No existing column changes type or
nullability; no existing row is rewritten.

#### (a) Characteristics — extend `qc_inspection_plans`, do not fork it

Six new nullable columns (`ALTER TABLE … ADD COLUMN IF NOT EXISTS`, the
established migration idiom):

```sql
characteristic_number    VARCHAR   -- balloon no. as it appears on the drawing ("14", "7.2")
characteristic_designator VARCHAR  -- key/critical/major/minor/none (closed vocab in code)
characteristic_type      VARCHAR   -- dimensional | material | process | note | functional
inspection_method        VARCHAR   -- on_machine_probe | cmm | gauge | visual | cert_review
sheet_zone               VARCHAR   -- drawing sheet + zone, e.g. "2/B4"
is_required              BOOLEAN   -- counts toward accountability (see D4)
```

Plus one small new table, because nothing in the repo can identify a
drawing today (C2 #3):

```sql
CREATE TABLE IF NOT EXISTS part_drawing_refs (
    drawing_ref_id  VARCHAR NOT NULL PRIMARY KEY,  -- pdr_<ULID>
    tenant_id       VARCHAR NOT NULL,
    product_id      VARCHAR NOT NULL,
    drawing_number  VARCHAR NOT NULL,
    drawing_rev     VARCHAR NOT NULL,
    effective_from  VARCHAR NOT NULL,   -- RFC3339
    superseded_at   VARCHAR,            -- NULL = current
    created_at      VARCHAR NOT NULL,
    created_by      VARCHAR NOT NULL
);
```

Revision history is kept (`superseded_at`) rather than overwritten: a report
issued in 2026 must still name the revision it was inspected against in
2033. Uniqueness of the current revision per `(tenant, product_id)` is
enforced **in code**, not by a SQL `UNIQUE` — `[[no-sql-specific]]`, matching
`qc::plans`' own posture.

**Why extend rather than fork.** `qc_inspection_plans` is *already* the
nominal/tolerance source of truth and *already* the thing the verdict is
computed against. A second "characteristic" table would immediately mean two
places to state a tolerance and a silent divergence between what was
measured against and what was reported. The AS9102 fields are the plan's
missing *identity* metadata, not a different concept.

#### (b) Measured actuals — the transport-independent input interface

**The interface already exists. Do not invent a second one.**
`ProbeIngestionSource` + `RawProbeEvent` (`crates/aberp-qa/src/qc/probe.rs:52`)
is exactly the transport-independent seam Ervin described: whether the
values arrive by FANUC `DPRNT`-to-file, Siemens OPC-UA R-parameters, MTConnect
`SAMPLE subType="ACTUAL"`, Renishaw Central, a CMM export, or an operator's
keyboard, they land as `RawProbeEvent` and go through `record_inspection`.
**No transport is designed in this ADR.**

Two additive changes are needed for the report layer, and only two.

**(i) Three new fields on `RawProbeEvent`** — all `Option`, so every existing
construction site and the `MockProbeSource` compile unchanged:

```rust
pub struct RawProbeEvent {
    // ── existing, unchanged ──
    pub source_event_id: String,
    pub timestamp_utc: OffsetDateTime,
    pub probe_serial: String,
    pub feature_name: String,
    pub actual_value: f64,
    pub units: String,
    pub cycle_id: Option<String>,
    pub machine_identifier: String,
    pub last_calibration_at_utc: OffsetDateTime,

    // ── NEW ──
    /// Which serialised unit was measured. Matched against
    /// `wo_part_marks.serial_number`. `None` ⇒ a lot-level measurement
    /// (reported once for the shipment, never attributed to a serial).
    pub part_serial: Option<String>,
    /// The plan's balloon number when the control emits it. When `None`,
    /// the characteristic is resolved by `feature_name` (today's behaviour).
    pub characteristic_number: Option<String>,
    /// NC program identity — part of the report's traceability block.
    pub program_id: Option<String>,
}
```

**(ii) One new batch entry point** in `aberp-qa`, which is the interface the
report layer and the future MC Connect pipeline both call:

```rust
/// Which plan row a measurement is for. Number wins when both are present;
/// an unresolvable key fails the batch loud (never a silently dropped
/// characteristic — that would produce an "accounted for" report with holes).
pub enum CharacteristicKey {
    Number(String),
    FeatureName(String),
}

pub struct MeasuredCharacteristic {
    pub characteristic_key: CharacteristicKey,
    pub actual_value: f64,
    pub units: String,                       // must equal the plan's — fail loud, never coerce
    pub measured_at: OffsetDateTime,
    pub source_event_id: Option<String>,
    pub probe_serial: Option<String>,
    pub last_calibration_at: Option<OffsetDateTime>,
}

pub struct MeasuredCharacteristicBatch<'a> {
    pub wo_id: &'a str,
    pub part_uid: Option<&'a str>,
    pub part_serial: Option<&'a str>,
    pub product_id: &'a str,
    pub machine_id: Option<&'a str>,         // e.g. the NTX's device id
    pub program_id: Option<&'a str>,
    pub operator: &'a str,
    pub source: QcSource,                    // Manual | Probe | Cmm | Other — already exists
    pub measurements: Vec<MeasuredCharacteristic>,
}

/// Record a whole unit's measurements atomically in the caller's tx.
/// Each element goes through the EXISTING `record_inspection` chokepoint,
/// so the verdict, the calibration-stale rule, the five `qc.*` audit
/// events, and the auto-NCR recommendation are unchanged and unduplicated.
pub fn submit_measured_characteristics(
    tx: &Transaction<'_>,
    ctx: &QcWriteContext<'_>,
    batch: MeasuredCharacteristicBatch<'_>,
    current_time: OffsetDateTime,
    stale_window_seconds: u64,
) -> Result<Vec<RecordedInspection>, QcError>;
```

**All-or-nothing per batch.** An unknown characteristic key, a units
mismatch, or a duplicate key inside one batch fails the whole call with the
offending index named, and emits `qc.probe_ingestion_failed` (the existing
kind, existing emitter at `qc/inspections.rs:338`). **Rationale:** a
partially-ingested unit is the worst possible outcome — it produces a report
that *looks* complete because every row it contains passed, while the rows
that would have failed are simply absent. Fail loud
(CLAUDE.md rule 12).

**What is deliberately NOT here:** how a DPRNT file is parsed, how OPC-UA
R-parameters are read, polling cadence, cursor semantics for a file-based
source. Those are [D-02](../docs/BACKLOG-designed-to-live.md#d-02) /
[D-16](../docs/BACKLOG-designed-to-live.md#d-16) and they land behind this
same signature.

#### (c) Traceability — resolved, snapshotted, never re-derived at render time

Every field is available today; the report *resolves once at issuance* and
stores the resolved values on the report row:

| Report field | Source today |
|---|---|
| part UID, serial, data-matrix payload | `wo_part_marks` (`part_marking.rs:177`) |
| heat / lot reference | `wo_part_marks.heat_lot_reference`; mill cert via `MaterialTraceabilitySeed.mill_cert_id` |
| work order, WO number, state | `work_orders` |
| source quote → buyer partner → customer name | `trace_part_uid` / `enrich_row` (`part_marking.rs:435-465`) |
| machine (the NTX) + NC program | `RawProbeEvent.machine_identifier` / `program_id`, snapshotted onto the inspection |
| operator | `qc_inspections.recorded_by` |
| date/time of measurement | `qc_inspections.measured_at_utc` |
| drawing number + revision | `part_drawing_refs` (**new** — D3(a)) |
| probe serial + last calibration | `qc_inspections.probe_serial` / `last_calibration_at_utc` |
| order / dispatch | `dispatches` (`dsp_id`, carrier, tracking) |

#### (d) The report record itself

```sql
CREATE TABLE IF NOT EXISTS qc_reports (
    qcr_id              VARCHAR NOT NULL PRIMARY KEY,   -- qcr_<ULID>
    tenant_id           VARCHAR NOT NULL,
    report_number       VARCHAR NOT NULL,   -- operator-facing, allocated in code
    report_kind         VARCHAR NOT NULL,   -- dimensional_inspection | coc | as9102_fair
    template            VARCHAR NOT NULL,   -- QcReportTemplate token
    state               VARCHAR NOT NULL,   -- drafted | issued | superseded | voided
    wo_id               VARCHAR NOT NULL,
    product_id          VARCHAR NOT NULL,
    dsp_id              VARCHAR,            -- set when bound to a shipment (D6)
    partner_id          VARCHAR NOT NULL,
    source_quote_id     VARCHAR,
    drawing_number      VARCHAR,            -- SNAPSHOT
    drawing_rev         VARCHAR,            -- SNAPSHOT
    qty_reported        INTEGER NOT NULL,
    serial_range        VARCHAR,            -- human-readable, snapshot
    heat_lot_reference  VARCHAR,
    mill_cert_id        VARCHAR,
    machine_id          VARCHAR,
    program_id          VARCHAR,
    disposition         VARCHAR NOT NULL,   -- accept | accept_with_ncr | reject | incomplete
    characteristics_required   INTEGER NOT NULL,
    characteristics_measured   INTEGER NOT NULL,
    characteristics_passed     INTEGER NOT NULL,
    characteristics_failed     INTEGER NOT NULL,
    characteristics_unaccounted INTEGER NOT NULL,
    rendered_sha256     VARCHAR,            -- set at issuance (D7)
    renderer_version    VARCHAR,            -- set at issuance
    issued_at_utc       VARCHAR,
    issued_by           VARCHAR,
    superseded_by_qcr_id VARCHAR,
    created_at          VARCHAR NOT NULL,
    created_by          VARCHAR NOT NULL,
    notes               VARCHAR
);

CREATE TABLE IF NOT EXISTS qc_report_lines (
    qcrl_id                 VARCHAR NOT NULL PRIMARY KEY,   -- qcrl_<ULID>
    tenant_id               VARCHAR NOT NULL,
    qcr_id                  VARCHAR NOT NULL,
    line_no                 INTEGER NOT NULL,               -- render order, stable
    part_serial             VARCHAR,                        -- NULL = lot-level
    part_uid                VARCHAR,
    characteristic_number   VARCHAR,
    characteristic_name     VARCHAR NOT NULL,
    characteristic_designator VARCHAR,
    characteristic_type     VARCHAR NOT NULL,
    inspection_method       VARCHAR,
    sheet_zone              VARCHAR,
    nominal_value           DOUBLE,
    upper_tol               DOUBLE,
    lower_tol               DOUBLE,
    units                   VARCHAR,
    actual_value            DOUBLE,          -- NULL iff accountability = not_measured
    deviation               DOUBLE,
    verdict                 VARCHAR,         -- reuses aberp_qa::qc::Verdict tokens
    accountability          VARCHAR NOT NULL,-- measured | not_measured | not_applicable
    qci_id                  VARCHAR,         -- the qc_inspections row this froze
    measured_at_utc         VARCHAR,
    measured_by             VARCHAR,
    probe_serial            VARCHAR,
    created_at              VARCHAR NOT NULL
);
```

No `CHECK`, no `DEFAULT`, no `UNIQUE` — every invariant lives in code,
matching `V002__qc.sql`'s stated posture and the DuckDB replay-clobber trap
it names. Two indexes only, mirroring `V002__qc.sql`:
`(tenant_id, dsp_id)` and `(tenant_id, qcr_id)`.

### D4 — Characteristic accountability, and the pass/fail model

**Per line:** `verdict` is copied from the frozen `qc_inspections` row.
`Pass` → pass. `Minor | Major | Critical` → fail.
`CalibrationStale` → **neither**: it prints as `CAL-STALE` and drives
disposition `incomplete`, because a measurement from an uncalibrated probe is
not evidence of conformity (ISO 9001 §7.1.5.2 — the rule ADR-0092 already
encoded in `compute_verdict`, checked *before* the tier).

**Overall disposition** — computed, never operator-typed:

```
any line failed                                   → reject
any required characteristic unaccounted-for
  or any line CAL-STALE                           → incomplete
all required accounted for, all pass,
  but an NCR is open against a listed part UID    → accept_with_ncr
otherwise                                         → accept
```

**Accountability is the AS9102 Form 3 discipline and the reason this is not
just a table dump.** For each serialised unit in scope, the report enumerates
**every enabled, required, non-archived plan characteristic for the product**
and joins measurements onto it. A characteristic with no measurement renders
as an explicit row with `accountability = not_measured` and a blank actual —
**it is never silently omitted**. `characteristics_unaccounted > 0` forces
disposition `incomplete`, which the D6 gate refuses to ship.

This is the single most important behaviour in the ADR. A QC report that
lists only what was measured is exactly the selective-recording failure mode
ADR-0092 names in its Context (*"re-measure the marginal feature until it
passes"*), moved from the shop floor to the printer.

### D5 — Rendering: a new pure crate, `aberp-qc-pdf`

A sibling of `aberp-quote-pdf` and `aberp-invoice-pdf`, same contract:

```rust
pub struct QcReportInputs<'a> {
    pub report: &'a QcReport,          // the frozen qc_reports row
    pub lines: &'a [QcReportLine],     // frozen qc_report_lines, in line_no order
    pub seller: &'a SellerIdentity,    // reuses the invoice-pdf identity block
    pub customer: &'a PartyInfo,
    pub template: QcReportTemplate,
}

pub fn render(inputs: &QcReportInputs<'_>) -> Result<Vec<u8>, QcPdfError>;
```

**Pure — no clock, no I/O, no RNG, no async.** Same inputs ⇒ byte-identical
output. This is load-bearing, not stylistic: D7's SHA-256 pin is only
meaningful if a re-render in 2033 reproduces the 2026 bytes.

**Rejected: extending `aberp-invoice-pdf`.** Its structure is organised
around NAV §169/§172 invariants (party blocks, VAT-rate breakdown, HUF/EUR
rate stamping, per-line tax columns) with no correspondence in a QC report —
the same reasoning `aberp-quote-pdf` already recorded when it declined to
build on it (`quote-pdf/src/lib.rs:44-51`). The palette and the footer
identity grammar are ported the way `aberp-quote-pdf` ported them; the day a
third caller needs the Helvetica/WinAnsi byte tables, a `pdf-style-helpers`
crate is the right factor-out — same call, unchanged.

Visual style: the ADR-0044 silver/gold palette, matching the invoice and the
quote, so all three customer-facing documents read as one company.

### D6 — Attaching to the shipment

**Two mechanisms, deliberately separate.**

**(1) The gate — a third instance of the existing pattern.** Pure resolver +
route enforcement, cloned from `resolve_open_ncr_gate`:

```rust
pub enum QcReportGate {
    Pass,
    Blocked {
        work_order_id: String,
        customer_type: String,
        reason: QcReportBlockReason,   // NoIssuedReport | Incomplete | Rejected
        qcr_id: Option<String>,
    },
}

pub fn resolve_qc_report_gate(
    conn: &Connection, tenant: &str, dispatch: &aberp_dispatch::Dispatch,
) -> anyhow::Result<QcReportGate>;

fn enforce_qc_report_gate_for_shipment(
    state: &AppState, dsp_id: &str, operator_login: &str,
) -> Result<(), WorkOrderRouteError>;   // 409 + one qcr.shipment_blocked audit row
```

Same three early `Pass` exits as its two siblings: unknown partner → `Pass`;
`CustomerType` not `Defense | Aerospace` → `Pass` (**the commercial path is
completely unaffected**); WO gone → `Pass`, defer to the dispatch crate's own
checks.

**(2) The binding — inside `mark_shipped`'s existing transaction.**
`qc_reports.dsp_id` is set in the same tx as the state flip, the stock
movement, the invoice spawn, and the three `export.*` appends. `aberp-dispatch`
must not gain a dependency on `aberp-qa`, so the binder is **injected**,
exactly as `InvoiceSpawner` and `ExportControlContext` already are
(`repository.rs:530` signature):

```rust
pub trait ShipmentDocumentBinder {
    /// Bind the issued QC report(s) for this WO to `dsp_id`, in `tx`.
    /// Returns the bound report ids for the `dispatch.shipped` payload.
    fn bind_qc_reports(
        &self, tx: &Transaction<'_>, tenant: &str, wo_id: &str, dsp_id: &str,
    ) -> Result<Vec<String>, anyhow::Error>;
}
```

**Rationale for atomicity.** A shipment that commits while its report binding
rolls back is a shipped part with no attached QC record — precisely the audit
finding this feature exists to prevent. Conversely a bound report on a
rolled-back shipment is a document claiming a delivery that never happened.
One transaction, both or neither. The precedent is exact: `mark_shipped`
already refuses to let a failed invoice spawn commit a shipment
(`DispatchError::InvoiceSpawnFailed` → *"this rolls back the ENTIRE
mark_shipped transaction"*).

**Delivery to the customer.** `GET /api/dispatches/:id/qc-report.pdf` and
`GET /api/qc-reports/:id/pdf` re-render from the frozen rows, exactly as
`GET /api/invoices/:id/pdf` does. The SPA Dispatch detail gains a
"QC documents" block; the existing email-invoice path gains the report as an
optional attachment (**Phase 1c, flagged — see Open questions**).

### D7 — Audit-grade retention

**Pin the hash, not the bytes.**

At issuance the renderer runs once, the SHA-256 of the emitted bytes is
computed, and `qcr.report_issued` is appended to the hash-chained ledger
carrying `rendered_sha256` + `renderer_version` + the full accountability
counts + the disposition + the traceability keys. The bytes themselves are
**not** stored: the report re-renders deterministically from the frozen
`qc_report_lines`, and the chain proves the bytes anyone re-renders are the
bytes that were issued.

**Why this and not the two alternatives:**

- *Store the PDF bytes in the DB.* Rejected. The DuckDB file is the object
  the whole durability apparatus is carrying — durable checkpoints under the
  writer lock (ADR-0111), the audit mirror, `durable_ack` (ADR-0110), boot
  auto-recovery (ADR-0095). Putting a few hundred KB of PDF per shipment
  behind that machinery makes every checkpoint, every mirror sync, and every
  snapshot proportionally more expensive to protect a payload that is
  *derivable*. Bad trade.
- *Write bytes to disk, AP-artifact style
  (`~/.aberp/<tenant>/ap-artifacts/<id>.xml`).* Rejected **as the source of
  truth**, and the reason is specific: that pattern is sound for AP invoices
  because **NAV holds the master copy** — the local file is a convenience,
  and its loss is recoverable from the authority. **Nobody else holds a QC
  report's master copy.** A loose file that can be deleted or replaced with
  no chain evidence is not an audit-grade record. (An *export* to disk for an
  auditor is fine and is exactly what the bundle below is.)

**Restorability, concretely.** The report rows ride the same
`aberp_db::Handle` writer, the same mirror, the same durable-ack path as
invoices — nothing new is required, which is the point of choosing rows over
blobs. For the auditor-facing export, `aberp-verify`'s bundle gains an
optional `qc/` directory alongside `nav/`, carrying the re-rendered PDF plus
its recorded SHA. **This requires an explicit change to
`crates/aberp-verify/src/bundle.rs:120`**, which currently bails on any tar
entry outside `{manifest.json, chain.jsonl, nav/}`; the allow-list must be
widened deliberately and a new check added that re-renders and compares the
SHA against the chain. Adding files without touching that guard is not
possible, by design — and that is a feature.

**Retention.** Aerospace retention is contract-specified and long (commonly
7 to 40 years, sometimes life-of-type). Nothing here deletes a report: there
is no delete path, only `voided` and `superseded_by_qcr_id`, matching the
invoice posture where a mistake is corrected by a new document, never by
editing the old one. **⚠️ FLAGGED:** the actual retention obligation is a
contract term Ervin must supply.

### D8 — Audit events: a new `qcr.*` family (187 → 193)

Six new kinds. A **new prefix**, not an extension of `qc.*`, for the reason
ADR-0092 gives when it created `qc.*`: keep each existing prefix consumer's
glob narrow.

1. **`QcReportDrafted`** — a report record was created. Payload: `qcr_id`,
   `report_kind`, `template`, `wo_id`, `product_id`, counts.
2. **`QcReportIssued`** — the load-bearing one. Payload: `qcr_id`,
   `report_number`, `wo_id`, `product_id`, `partner_id`, `drawing_number` +
   `drawing_rev`, serial list, heat/lot, `machine_id`, `program_id`,
   `disposition`, the five accountability counts, `rendered_sha256`,
   `renderer_version`, `issued_by`, `issued_at_utc`.
3. **`QcReportAttachedToShipment`** — `qcr_id` + `dsp_id` + `wo_id`. Fired
   inside `mark_shipped`'s tx.
4. **`QcReportRendered`** — every re-render. Payload: `qcr_id`, the SHA of
   the bytes just produced, `renderer_version`. **A divergence from the
   issued SHA is detectable in the chain** without anyone storing a byte.
5. **`QcReportShipmentBlocked`** — the D6 gate's denial row. Payload:
   `dsp_id`, `wo_id`, `partner_id`, `customer_type`, `reason`, `qcr_id?`,
   `operator_user_id`, `blocked_at`. Appended standalone (the 409 path has no
   business tx to ride), mirroring `ncr.wo_blocked_by_open_ncr` exactly.
6. **`QcReportVoided`** — `qcr_id`, `reason`, `superseded_by_qcr_id?`,
   `operator_user_id`.

All six get the full ritual: `as_str` / `from_storage_str` round-trip,
`ALL_KINDS` entry, both NAV-leakage pins, and the `ALL_KINDS_COUNT` bump
**187 → 193** (`event_kind.rs:4004` pins 187 at `9e4a6ee`). ⚠️ Other
unmerged branches also add kinds — reconcile the arithmetic at merge, do not
trust the 193.

### D9 — Edition scope

**Defense only.** A new capability predicate in
`apps/aberp/src/build_profile.rs`, parameterised so both arms are provable in
one compile — the exact shape of `storefront_polling_allowed_for`
(`build_profile.rs:249`):

```rust
pub const fn qc_reporting_allowed_for(edition: Edition) -> bool {
    matches!(edition, Edition::Defense)
}
```

Portable: the routes are not mounted, the gate resolves `Pass`
unconditionally, and the migration — while it runs — leaves a Portable
tenant with zero rows in all three new tables. The Portable byte-identity
test is an acceptance criterion (§AC7).

### D10 — Operator UX (`[[hulye-biztos]]`)

The operator sees, on the WO and on the Dispatch detail: a
green / amber / red / grey chip per characteristic (matching the QC chip
ADR-0092 already specified), a **"characteristics accounted for: 11 / 14"**
counter that is red until complete, a "Generate QC report" button that is
disabled with a specific reason when it cannot proceed, and a preview before
issuance. No AS9102, MTConnect, QIF, or template awareness anywhere in the
operator surface.

---

## Phasing

### Phase 1 — the report is real on its own, before MC Connect exists

Ships against **today's manual actuals** (`QcSource::Manual`, the live
`POST /api/qc-inspections` path) and against `MockProbeSource` in tests.

1. The six `qc_inspection_plans` columns + `part_drawing_refs` + their CRUD.
2. `qc_reports` + `qc_report_lines` + the accountability computation (D4).
3. `submit_measured_characteristics` + the three `RawProbeEvent` fields (D3b)
   — **the interface, not a transport.**
4. `aberp-qc-pdf`: `DimensionalInspection` + `CertificateOfConformance`.
5. The D6 gate + the `ShipmentDocumentBinder` binding in `mark_shipped`.
6. The six `qcr.*` kinds + the SHA-256 issuance pin.
7. Routes + SPA surface + the Portable byte-identity pin.

**Phase 1b** — `As9102Fair` (Forms 1/2/3) as a third render shape. Same
data, new layout; separable and schedulable on its own.

**Phase 1c** — attach the report to the shipment e-mail. Flagged, because it
sends a compliance document to a customer automatically.

**Blocked on:** nothing external. Everything Phase 1 reads exists at
`9e4a6ee`, except the drawing number/revision, which is operator-entered
data, not an integration.

### Phase 2 — auto-populated actuals

Replace the operator's keyboard with the MC Connect probe-results pipeline
(FANUC `DPRNT`-to-file / Siemens OPC-UA R-parameters / MTConnect). This is
**[D-02](../docs/BACKLOG-designed-to-live.md#d-02)** and shares its MTConnect
work with **[D-16](../docs/BACKLOG-designed-to-live.md#d-16)**; it lands
behind `submit_measured_characteristics` and changes **no report code at
all**. That is the test of whether D3(b) drew the seam in the right place.

**Blocked on:** physical access to the NTX and its control.

---

## Consequences

- **New:** three tables (`qc_reports`, `qc_report_lines`, `part_drawing_refs`),
  six columns on `qc_inspection_plans`, one column on `partners`, one crate
  (`aberp-qc-pdf`), six `qcr.*` event kinds (187 → 193), one shipment gate,
  one injected binder trait on `mark_shipped`, ~4 routes, one SPA surface.
- **`mark_shipped`'s signature grows a sixth parameter.** It already takes
  five (`tx`, `ctx`, `dsp_id`, `inputs`, `spawner`, `export_ctx`). Every
  caller and every test updates. This is the honest cost of keeping the
  binding atomic.
- **`aberp-verify`'s bundle shape changes** (the `qc/` allow-list at
  `bundle.rs:120`). Older verifiers reject newer bundles — a deliberate,
  loud, versioned break, not silent forward-compat.
- **A new master-data burden.** Balloon numbers, designators, methods, and
  drawing revisions must be maintained per product. Today's plans have none
  of it. A CAM/drawing import (AP242 PMI extraction — ADR-0097 R5 /
  [D-19](../docs/BACKLOG-designed-to-live.md#d-19) territory) is the eventual
  answer; **Phase 1 is operator-entered**, and a product with no
  characteristics simply cannot produce a Defense shipment once the gate is
  on.
- **Defense shipments can now be blocked by a third gate.** With D6 on and no
  plan populated, `mark_shipped` 409s. This is intended and it is why the
  gate is flagged as a policy decision (Q3) rather than assumed.
- **No change** to `qa_inspections`, `qc_inspections`' write path,
  `compute_verdict`, the two existing gates, the commercial path, the
  invoice/NAV path, or the quote engine. **Nothing in this ADR touches
  pricing** — ADR-0113 owns that side and the two do not interact in code.

---

## Phase 1 — what actually landed (2026-08-23)

Implemented on branch `docs/adr-qc-inspection-report`, off `origin/main`
`9e4a6ee`, in the same commit series as this ADR so the design travels with
the code.

| Piece | Where |
|---|---|
| `qc_reports` + `qc_report_lines` + `part_drawing_refs` + the six additive plan columns | `crates/aberp-qa/migrations/V003__qc_report.sql` (additive only; no CHECK / DEFAULT / UNIQUE) |
| Closed vocabularies (`QcReportKind`, `QcReportTemplate`, `QcReportState`, `CharacteristicDesignator`, `CharacteristicType`, `InspectionMethod`, `Accountability`, `Disposition`) | `crates/aberp-qa/src/qc/vocab.rs` |
| Drawing refs with revision history (`supersede_and_create`, one-current-per-product enforced in code) | `crates/aberp-qa/src/qc/drawings.rs` |
| The frozen record + the **pure** accountability core (`build_report_lines` / `summarise` / `compute_disposition`) + freeze / issue / bind / render-audit / void | `crates/aberp-qa/src/qc/reports.rs` |
| The pure renderer — `DimensionalInspection`, `CertificateOfConformance`, **AS9102 Rev C Forms 1/2/3** | `crates/aberp-qc-pdf/` (new crate) |
| `ShipmentDocumentBinder` + `NoopShipmentDocumentBinder`, `mark_shipped`'s 7th parameter, `DispatchError::ShipmentDocumentBindFailed` | `crates/aberp-dispatch/src/repository.rs`, `error.rs` |
| Orchestration: traceability resolution, issuance + SHA pin, `QcShipmentDocumentBinder` | `apps/aberp/src/qc_report.rs` (new) |
| The third shipment gate (`resolve_qc_report_gate` + `enforce_qc_report_gate_for_shipment`) and eight routes | `apps/aberp/src/serve.rs` |
| `qc_reporting_allowed_for(Edition)` + `assert_qc_reporting_allowed` | `apps/aberp/src/build_profile.rs` |
| `partners.qc_report_template` + `resolve_qc_report_template` | `apps/aberp/src/partners.rs` |
| Six `qcr.*` kinds, `ALL_KINDS_COUNT` 187 → 193, both NAV-leakage pins | `crates/audit-ledger/src/entry/event_kind.rs`, `aberp-verify/src/verify.rs`, `apps/aberp/src/export_invoice_bundle.rs` |
| Bundle allow-list widened to `qc/` + the SHA re-hash check | `crates/aberp-verify/src/bundle.rs`, `verify.rs` |

### Deltas from the design pass — stated, not silent

1. **AS9102 Forms 1/2/3 shipped in Phase 1, not Phase 1b.** Ervin named Rev C
   as the default form, so it is built rather than scheduled.
2. **`submit_measured_characteristics` and the three `RawProbeEvent` fields
   (§D3(b)) were NOT built.** They are the Phase-2 seam and have no Phase-1
   caller: the report reads the `qc_inspections` rows the live manual route
   already writes. Shipping an interface with no consumer would have been
   speculative, so D3(b) moves to the Phase-2 session, where its first real
   caller lives. **§D3(b)'s design stands unchanged** — this is a scheduling
   change, not a redesign.
3. **The `qc/` bundle directory is READ-side only.** `aberp-verify` accepts,
   re-hashes and cross-totals `qc/` entries; nothing WRITES one yet.
   `export_invoice_bundle` is invoice-scoped and would need an
   invoice→dispatch→WO→report join to decide which reports belong in a
   slice. That join is real scope and was not in this session's brief. The
   verifier half is what makes the widening safe to land now; the writer is
   an auditor-export feature that can follow.
4. **The tenant-default template tier was not built.** §D2's resolution order
   names `partner → tenant default → AbenStandard`; the implementation is
   `partner → AbenStandard`, funnelled through one function
   (`partners::resolve_qc_report_template`) so adding the middle tier is a
   one-line change in exactly one place. No tenant asked for it, and an
   unused settings row is a place for configuration to drift away from what
   customers actually receive.
5. **`mill_cert_id`, `machine_id` and `program_id` snapshot as `NULL` on the
   manual path.** No mill-cert record is wired to a WO today, and the machine
   and NC-program identities arrive on a probe event — which is Phase 2. They
   print as blanks rather than guesses.
6. **The issued SHA-256 is not printed on the page it hashes, and neither
   is the dispatch id.** Both fall out of one constraint that the design
   pass did not surface, and that implementation did:

   > **Nothing that changes after issuance may appear in the hashed bytes.**

   The gate requires a report to be ISSUED before the shipment may proceed,
   so `qc_reports.dsp_id` is written by `mark_shipped` strictly *after* the
   hash is taken. A first cut printed the dispatch in the identity block —
   which would have made **every correctly shipped report report itself as
   tampered** on the first download, i.e. turned the tamper signal into
   noise on exactly the documents that matter. Four fields are affected
   (`rendered_sha256`, `dsp_id`, `state`, `superseded_by_qcr_id`); all four
   are normalised by ONE function, `qc_report::canonical_for_render`, which
   both issuance and re-render go through so they provably agree. The
   renderer carries a NOTE at the former site so a future edit does not
   "restore the missing row".

   Nothing is lost: the dispatch linkage is a chain event
   (`qcr.report_attached_to_shipment`) and is on both HTTP surfaces; the
   hash is in the chain, on the API response, and in the
   `x-aberp-qc-sha-matches-issued` response header (which reports `draft`,
   not `false`, when there is no pin to compare against).

7. **`qty_reported` is the marked-unit count.** With no marked units the
   report degrades to a single lot-level document and records `1`. The WO's
   `qty_target` is not read: the report states what it accounted for, and
   claiming a quantity it did not enumerate would be the same class of
   overstatement the accountability rule exists to prevent.

### Behaviours pinned by mutation testing

Both safety-critical behaviours were verified by mutation rather than by
coverage: each mutation below was applied to the working tree, the scoped
test suite was confirmed RED, and the mutation reverted.

- **(a) missing required characteristic ⇒ `incomplete` ⇒ shipment refused**
  — 10/10 mutations killed, spanning the disposition rule, the line builder
  (silent omission), `permits_shipment`, the tally, the NULL-`is_required`
  reading, four gate arms, and the route's gate call.
- **(b) tampering the frozen snapshot or the `rendered_sha256` ⇒ verify
  fails loud** — 13/13 mutations killed, spanning the SHA comparison, the
  unpinned-document arm, the orphan cross-total, both halves of the bundle
  allow-list, renderer determinism, the blank-vs-zero cell, strict decoding
  of frozen line and header vocabularies, the re-issue refusal, the hash
  recipe, and the measured-band-vs-live-plan freeze.
- **(c) the canonical hashed form** — 5/5 mutations killed, covering each
  normalised field, over-normalisation in both directions, and the
  operator-login trim. This set exists because writing it is what surfaced
  the `dsp_id` bug in delta 6 above; the first two mutations initially
  SURVIVED, which is what showed the guard was untested.

---

## Acceptance criteria (for the Phase-1 implementation session)

1. **End-to-end, manual actuals:** plan with 4 required characteristics →
   `submit_measured_characteristics` for 2 serials → draft report →
   accountability shows 8/8 → issue → PDF renders → `mark_shipped` binds
   `dsp_id` in the same tx → `qcr.report_issued` and
   `qcr.report_attached_to_shipment` both in the chain.
2. **Accountability is not a table dump:** measure 3 of 4 characteristics →
   the report renders **4** rows, the 4th marked `not_measured` with a blank
   actual, `characteristics_unaccounted == 1`, disposition `incomplete`, and
   the D6 gate 409s the shipment with reason `Incomplete`.
3. **Byte-determinism:** render the same frozen rows twice (different
   processes, different wall clocks) → identical bytes, identical SHA-256,
   equal to the `rendered_sha256` on the issued row. A property test that the
   renderer never panics on any `qc_report_lines` shape the writer can
   produce.
4. **Snapshot immunity:** issue a report, then `update_plan` the tolerance and
   `archive_plan` a characteristic → re-render → **byte-identical to the
   issued bytes**.
5. **Atomicity:** a `ShipmentDocumentBinder` that errors rolls back the whole
   `mark_shipped` — no state flip, no stock movement, no invoice draft, no
   audit rows. Mirror the existing `InvoiceSpawnFailed` test.
6. **Batch fails loud:** a units mismatch or an unresolvable
   `CharacteristicKey` at index *n* fails the whole batch naming *n*, writes
   **zero** `qc_inspections` rows, and emits `qc.probe_ingestion_failed`.
7. **Edition arms:** `qc_reporting_allowed_for(Defense) == true`,
   `(Portable) == false`, `(Prod) == false`; a Portable build mounts no
   route, resolves the gate `Pass`, and the Portable wire/binary identity pin
   holds.
8. **Commercial path unaffected:** a non-Defense/non-Aerospace partner ships
   with no report and no 409 — the same assertion the two existing gates
   already carry.
9. **Calibration-stale is not a pass:** a `CalibrationStale` line prints
   `CAL-STALE`, counts as neither pass nor fail, forces `incomplete`, and
   raises **no** NCR (ADR-0092's rule, unchanged).
10. **Verify bundle:** a bundle containing `qc/` verifies; the re-rendered
    SHA is compared against the chain; a tampered PDF in the bundle fails the
    check loudly.
11. **Gates green** (`ABERP_TEST_PYTHON` unset): fmt, clippy
    `--workspace --all-targets` 0-warn, `cargo test --workspace` 0-fail,
    vitest, svelte 0/0, `ALL_KINDS_COUNT` pin + round-trip double-entry both
    updated.

---

## Open questions / decisions flagged for Ervin

> ### ✅ ALL RESOLVED — 2026-08-23
>
> **Ervin accepted the specification and every flagged decision at its
> conservative default**, confirming two explicitly:
>
> - **Q1 — AS9102 Rev C is the default FAIR form.** Rev C it is; Rev B is
>   not built. A prime-specific or Nadcap form remains a future
>   `QcReportTemplate` variant plus a render function, never a schema change.
> - **Q3 — an incomplete report BLOCKS a Defense shipment: YES.** The gate
>   is live as the third clone of the part-UID / open-NCR pattern. Once a
>   Defense/Aerospace dispatch's WO has required characteristics, that
>   dispatch cannot ship without an `issued`, `accept`-or-`accept_with_ncr`
>   report bound to it.
>
> Q2, Q4–Q10 stand at their stated defaults; Q11 (the number) is still
> open and is a merge-time chore, not a design decision. Each entry below
> now carries its resolved verdict inline. **Resolution is recorded here,
> not re-litigated:** any later change to one of these is a new decision
> with its own ADR entry.

Every one of these was decided conservatively so implementation is not
blocked. Each is a default that can be overridden without redesign.

1. **⚠️ Exact standard and forms.** Default: **AS9102 Rev C** shape, with the
   per-shipment pair (Dimensional Inspection Report + CoC) as the primary
   output and the FAIR as Phase 1b. Confirm Rev C vs Rev B, and name any
   prime-specific or Nadcap-mandated form now — each is a `QcReportTemplate`
   variant and a render function.

    **✅ RESOLVED — Rev C, confirmed explicitly.** `As9102RevC` is the only FAIR variant built, and Forms 1/2/3 ship in Phase 1 rather than 1b. `AbenStandard` (house dimensional layout) and `CocOnly` are the other two `QcReportTemplate` tokens. No prime-specific or Nadcap form was named, so none is built.

2. **⚠️ One report per shipment, or one per part?** Default: **one report per
   dispatch**, with a characteristic table per serialised unit inside it, plus
   one CoC. Some primes want one FAIR per part number and one CoC per
   shipment; a few want a per-serial certificate. Changes layout only, not the
   schema.

    **✅ RESOLVED — default accepted.** One report per dispatch, characteristic rows per serialised unit inside it, plus one CoC. Per-serial certificates stay a layout change if a prime later asks.

3. **⚠️ Should a missing/incomplete/rejected report BLOCK the shipment?**
   Default: **yes, block** — Defense/Aerospace customers only, mirroring the
   part-UID and open-NCR gates. This is a process commitment: once on, a
   Defense shipment cannot leave without an issued, complete report. The
   alternative (warn-only) is one predicate away.

    **✅ RESOLVED — YES, block; confirmed explicitly.** Implemented as `resolve_qc_report_gate` / `enforce_qc_report_gate_for_shipment`, the third clone of the part-UID and open-NCR gates: pure resolver, 409, one `qcr.report_shipment_blocked` audit row. Defense/Aerospace partners only; the commercial path is untouched.

4. **⚠️ Drawing number + revision — where do they come from?** Nothing in the
   repo carries them. Default: **operator-entered per product**, with revision
   history. Alternatives: extract from the STEP file header (unreliable —
   varies by CAD), or from the customer PO. This is the largest data-entry
   burden the feature adds.

    **✅ RESOLVED — default accepted.** Operator-entered per product via `part_drawing_refs`, with revision history (`superseded_at`). No STEP-header or customer-PO extraction. The data-entry burden is accepted.

5. **⚠️ Signature on the CoC.** Default Phase 1: **printed name + operator
   login + the `qcr.report_issued` chain reference**, no cryptographic
   signature. A real signing ceremony is
   **[D-15](../docs/BACKLOG-designed-to-live.md#d-15)** (`personnel.*` +
   e-signature) and **[D-06](../docs/BACKLOG-designed-to-live.md#d-06)**
   (NETLOCK qualified TSA). Confirm whether an unsigned CoC is acceptable to
   the customers in question — for some primes it is not.

    **✅ RESOLVED — default accepted.** Phase 1 prints name + operator login + the `qcr.report_issued` chain reference. No cryptographic signature; D-15 / D-06 still own the real signing ceremony.

6. **⚠️ Retention period.** Contract-specified in aerospace, commonly 7–40
   years. Nothing deletes a report today; confirm the obligation so it can be
   stated on the document and in the tenant policy.

    **✅ RESOLVED for the mechanism; the contract term is still owed.** Nothing deletes a report — there is no delete path, only `voided` and `superseded_by_qcr_id`. The number of years to *print* on the document is a contract fact Ervin still has to supply, and no code depends on it.

7. **⚠️ 100% inspection, or a sampling plan?** Default: **100%**, consistent
   with ADR-0113's "every part is auto-probed". No AQL / C=0 sampling model is
   designed. If any customer accepts sampling, that is a real feature
   (sampling plan, lot definition, AQL tables), not a knob.

    **✅ RESOLVED — default accepted.** 100% inspection. No AQL / C=0 sampling model exists or is stubbed.

8. **⚠️ Auto-attach to the shipment e-mail (Phase 1c)?** Default: **off** —
   generating a compliance document is one decision, mailing it to a customer
   automatically is another.

    **✅ RESOLVED — default accepted: OFF.** Phase 1 ships no automatic mailing of a compliance document. Not built.

9. **⚠️ Language.** Default: **English** for QC documents (aerospace audits and
   primes are English-speaking), unlike the invoice/quote which are HU-first.
   Bilingual HU/EN is a layout change if wanted.

    **✅ RESOLVED — default accepted.** QC documents render in English only.

10. **⚠️ Lot-level characteristics.** Default: a measurement with no
    `part_serial` renders once for the shipment, attributed to the lot. Confirm
    this is how material/process characteristics (hardness, coating thickness,
    heat-treat cert) should read.

    **✅ RESOLVED — default accepted.** A line with no `part_serial` is lot-level: it renders once for the shipment and is never attributed to a serial.

11. **The number.** `ADR-0199` / `D-99` are placeholders. See the banner at the
    top of this file.

    **⏳ STILL OPEN — a merge-time chore, not a design decision.** `ADR-0199` / `D-99` remain placeholders; see the banner at the top of this file.