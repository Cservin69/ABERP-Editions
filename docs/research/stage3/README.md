# Stage 3 manufacturing integration — research package

**Status**: preparatory research. The Phase α framework decision **has** since
landed — see [ADR-0060](../../../adr/0060-stage3-manufacturing-adapter-framework.md)
(PR-224 / S228, 2026-06-03) — but the per-protocol / per-vendor surveys in
this package remain pre-commitment research until a real adapter for that
vendor ships. **Authored**: 2026-06-02 (framework-α update: 2026-06-03).
**Audience**: future-Ervin and future-Dispatch sessions doing Stage 3 work.

## Framework α landed (2026-06-03)

PR-224 / S228 shipped the framework skeleton: the `crates/aberp-mes/` crate
with the canonical event vocabulary (`PartMoved`, `MachineStateChanged`,
`QualityResultReceived`, `ScanReceived`, `WorkOrderStateChanged`,
`RobotTaskQueued`), the `Adapter` trait, the `AdapterRegistry`, the
`NoopAdapter` reference impl, and the audit-ledger integration
(`EventKind::MesAdapterEvent`, storage string `mes.adapter_event` — a third
prefix family alongside `invoice.*` and `system.*`). **No real hardware
adapter** lives there yet — `NoopAdapter` is the only impl. Phase β picks the
first real adapter; the README's standing recommendation of "barcode scanner
first" still stands.

The architectural decision behind that framework is
[ADR-0060](../../../adr/0060-stage3-manufacturing-adapter-framework.md).
Read it before any Phase β work — the trait shape, the canonical vocabulary,
and the audit-ledger prefix family are now load-bearing contracts, not
research starting points.

## Why this exists

ABERP today is invoicing (Stage 1) plus a beginning of the customer-facing storefront strand (Stage 2). Stage 3 is the multi-year vision: when 3-4 DMG-Mori CNCs, a laser, robot transport, and a Renishaw quality gate land in Áben's shop, ABERP becomes the orchestration brain — the work-order dispatch, the audit ledger, the status board, the operations dashboard.

That work doesn't begin until 2026 H2 at earliest, and most of it slots into 2027+. But the integration landscape — what protocols machines speak, what robot controllers expose, what an open-standard adapter looks like — is something we can map *now*, before any hardware lands, so the eventual α-phase ADR (the "Phase α" framework decision named in the Stage 3 planning memo) starts from a sourced foundation rather than a blank page.

The architectural posture is fixed: **adapter-pattern, open-standard-first, vendor-neutral**. This package documents what each option *actually is*, so when the time comes to pick one we're choosing with eyes open instead of taking the first vendor pitch at face value.

## What this is NOT

- **Not an ADR.** ADRs are commitments; this is preparation.
- **Not implementation guidance.** No code samples that you can `cargo new` from. Architectural shape and protocol surface, that's all.
- **Not a buying guide.** Vendor pricing where public is included for context, but the brand selection (which DMG model, which robot) is Ervin's decision and isn't time-critical here.
- **Not a 2026 plan.** Stage 1 polish + Stage 2 build-out are still the priority. This research sits on the shelf until Phase α kicks off.

## Reading order

1. **[01-machine-protocols.md](01-machine-protocols.md)** — CNC machine integration landscape (CELOS vs MTConnect vs OPC-UA vs fallback). Start here; it frames the rest.
2. **[02-renishaw-quality-gate.md](02-renishaw-quality-gate.md)** — Renishaw measurement output, the Equator and on-machine probes.
3. **[03-robot-controllers.md](03-robot-controllers.md)** — Survey of robot vendors and their integration surfaces.
4. **[04-barcode-qr-scanners.md](04-barcode-qr-scanners.md)** — Scanner integration models (HID, TCP, MQTT).
5. **[05-cell-controllers.md](05-cell-controllers.md)** — The local computer on the shop floor (Pi 5 vs industrial mini-PC).
6. **[06-mtconnect-deep-dive.md](06-mtconnect-deep-dive.md)** — The standard at the heart of any vendor-neutral CNC strategy.
7. **[07-oee-mes-metrics.md](07-oee-mes-metrics.md)** — Operations dashboard math (parallel to the financial dashboard).
8. **[08-fms-orchestration.md](08-fms-orchestration.md)** — Multi-machine cell coordination.
9. **[09-laser-workflow.md](09-laser-workflow.md)** — Sheet-metal parallel workflow.
10. **[10-references.md](10-references.md)** — Consolidated citations.

Files 01-06 are protocol/vendor-surface oriented. Files 07-09 are operational/architectural. File 10 is the bibliography. You can read 01 then jump to whichever topic is relevant; only 06 (MTConnect deep-dive) assumes you've read 01 first.

## Decision deadlines

**None.** Nothing in this package is time-critical. The Stage 3 α-phase ADR is the next checkpoint — and that's named-deferred to roughly 2026 H2.

The single thing that could move sooner is **barcode/QR adoption** (covered in `04-barcode-qr-scanners.md`). Ervin could label inventory and start status-tracking manual work *today* without buying any CNC hardware. If that becomes a Phase β candidate ahead of schedule, 04 is the file to read first.

## How to maintain

This research will go stale. Plan for it.

- **When a machine arrives**: the file covering its category (01 for CNC, 09 for laser, 02 for Renishaw, 03 for the robot) graduates from "research" to "operational" — copy the relevant adapter notes into a real ADR, and update the file's header to "superseded by ADR-NNNN".
- **When a protocol version bumps**: e.g. MTConnect 2.5 lands. Update `06-mtconnect-deep-dive.md` and note in `10-references.md` what was fetched-when versus what's current.
- **When a vendor radically changes posture**: e.g. DMG MORI drops MTConnect support, or KUKA opens up RSI further. Add a `## 2027 update` section to the relevant file rather than rewriting — historical context matters.
- **When the brand is selected**: e.g. Ervin picks Universal Robots cobots. The file's "survey" framing becomes outdated; promote the chosen vendor's section into its own working doc and leave a pointer here.

This package is markdown only — no code, no compile-time dependencies. It can rot quietly without breaking anything, which is exactly what we want from forward-looking research: useful when needed, harmless when not.

## House style for these docs

- **Citations inline** as superscript-style `[^N]` references; consolidated bibliography in each file's footer plus a master index in `10-references.md`.
- **Honest gaps**: where a vendor doesn't publish numbers or a spec is paywalled, say so. Better to flag "couldn't confirm" than to fabricate a plausible number.
- **HU+EN aware**: where Hungarian operator-facing strings matter (the future shop floor UI is bilingual), note the translation concern. Most protocols are English XML/JSON — that maps to ABERP's canonical event names in the adapter, not in the UI layer.
- **No code blocks** except for one-line wire-shape illustrations (e.g. a single MTConnect XML sample to show the document shape).
- **Tables for comparison**: vendor × protocol × open-standard × estimated-cost. Hard to scan otherwise.
- **Recommendation framework, not recommendation**: each file ends with "if I were picking today, here's the order I'd try" — explicitly framed as a starting point that the actual α-phase ADR can override.

## Cross-cutting principle

Every file ends with the same architectural recommendation framework because the same posture applies everywhere: **open standard first, proprietary protocol only if forced, never proprietary as the primary path.** This isn't a copy-paste; the actual options differ per topic. But the *shape* of the choice — adapter crate translating vendor dialect into ABERP's canonical event types — is constant.

That posture comes from Ervin's standing direction: "we will do SpaceX way except the regulatory dependencies — develop in time everything vertically," combined with the existing principle that nothing is SQL-engine-specific (invariants in application code, no DB-vendor lock). When a future session reads this package and finds itself drawn to a vendor-lock-in option ("CELOS X is just so polished, let's commit"), that's the moment to push back.
