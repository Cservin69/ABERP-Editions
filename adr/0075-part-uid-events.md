# ADR-0075 — `part.*` audit EventKind family + the MIL-STD-130N IUID format model: the per-unit serialization surface.

- **Status:** Proposed
- **Date:** 2026-06-11
- **Deciders:** Ervin (via S358 / PR-45 defense-pivot batch session 3).
- **Supersedes:** none — first ADR of the per-unit serialization strand.
- **Related:** ADR-0008 (audit ledger — the hash-chained home these events live in), ADR-0071 (`aberp-compliance` crate — home of the new `uid::Iuid` / `IuidConstruct1` / `IuidConstruct2` / `validate_iac` types these events carry; wired as a dep of `apps/aberp` by S355), ADR-0073 (`personnel.*` family — the access-trail strand), ADR-0074 (`material.*` family — the material-traceability strand, same no-firing-site-yet posture and split rationale), the defense-aerospace gap analysis (S330, `[[defense-aerospace-pivot]]`), and `[[trust-code-not-operator]]`, `[[no-sql-specific]]`.

## Context

The aerospace/defense gap analysis (S330) named the structural must-builds for the pivot: operator identity + e-signature (ADR-0070 / ADR-0073's `personnel.*`), lot/heat material traceability (ADR-0074's `material.*`), and — this ADR's subject — **per-unit serialization and Item Unique Identification (IUID)**.

DoD 5000.64 and MIL-STD-130N require that serially-managed items carry a globally-unique, machine-readable Item Unique Identifier (the "UII" / UID) physically marked on the item (a 2D Data Matrix), so a part's pedigree can be resolved for the life of the item. MIL-STD-130N defines the UII as a concatenation of a small set of data elements in one of two valid **constructs**:

1. **Construct 1** — Issuing Agency Code (IAC) + Enterprise Identifier (EID) + Serial Number (no part number). The serial is unique across *all* items the enterprise produces.
2. **Construct 2** — IAC + EID + Original Part Number + Serial Number. The serial is unique *within the part number* for that enterprise.

> **Correction (S367, 2026-06-12 — review F5):** S358 originally defined these two constructs in reverse (Construct 1 carried the part number; Construct 2 was serial-only). That is backwards against MIL-STD-130N and the DoD *Guide to Uniquely Identifying Items*, where Construct #1 is the serial-only form and Construct #2 adds the original part number. No firing site or ledger row existed, so S367 swapped the struct contents, `to_iri()` renderings, and every pinned test in the same commit. The §3 examples below reflect the corrected mapping.

The IAC (per ISO/IEC 15459) names the registration authority that guarantees the EID's uniqueness; for a DoD contractor the EID is typically a CAGE code or DUNS number, each reachable through its registered IAC.

ABERP's commercial core has no notion of an individual, serialized part instance — it quotes and invoices quantities of a grade. Two distinct facts in the serialization lifecycle need a durable, tamper-evident home, and the audit ledger (ADR-0008) is the right one. What it lacked is the **typed vocabulary** that lets a forensic walker glob "every serial assignment / UID mark on this install" without scraping free-text — and a validated **format model** so a malformed IUID can never be recorded. This session adds both; it does **not** wire firing sites (later session, when MES / QA serialization and marking flows exist in code).

## Decision

**A new `part.*` EventKind prefix family with two members, plus a new `aberp_compliance::uid` module holding the validated MIL-STD-130N IUID format types. The kinds carry free-form `serde_json::Value` payloads (the same posture every recent `quote.*` / `personnel.*` / `material.*` kind takes); their documented field shapes are pinned by serialization tests so a later firing site has a stable contract. No firing site is added in S358.**

### 1. The two kinds

| EventKind | storage string | fires when | payload |
| --- | --- | --- | --- |
| `PartSerialAssigned` | `part.serial_assigned` | a serial number is assigned to a part | `{part_id, serial_number, assigned_at_ms, operator_user_id, related_invoice_id?, related_work_order_id?}` |
| `PartUidMarked` | `part.uid_marked` | the MIL-STD-130N UID is physically marked on the part | `{part_id, uid_iri, uid_construct_code, mil_std_130_compliant, marked_at_ms, operator_user_id}` |

### 2. Why `serial_assigned` (record) is split from `uid_marked` (state transition)

This is the load-bearing design call, and it mirrors the ADR-0074 cert/heat-lot split rationale: distinct facts get distinct kinds so neither is inferred by diffing payloads.

- **Different times, different actors.** A serial is *assigned* — a logical fact — possibly at order entry or work-order release, by a planner. The UID is *physically marked* — a shop-floor fact — later, by an operator or a marking station. Collapsing them would force a reader to diff payloads to tell "serial allocated" from "metal marked".
- **One can occur without the other.** A serial can be assigned to a part that is never marked (scrapped before marking); a re-mark can occur against an already-assigned serial (the first mark was illegible). An immutable assignment record and a (re-)markable physical-mark transition have different audit semantics — `serial_assigned` is purely additive, `uid_marked` is a transition whose history reconstructs the marking pedigree.
- **Payload shape.** A serial assignment answers "which serial, triggered by which order/work-order, by whom" (`serial_number` / `related_invoice_id` / `related_work_order_id`). A UID mark answers "what IRI, which construct, did it pass MIL-STD-130N, by whom" (`uid_iri` / `uid_construct_code` / `mil_std_130_compliant`). The fields barely overlap; one kind would be a mostly-null union.

Keeping them separate lets a forensic walker answer "show me every serial ever assigned" (`part.serial_assigned`) and "reconstruct the physical-marking history" (`part.uid_marked`) as two clean globs.

`uid_iri` on `PartUidMarked` is the load-bearing identifier. The firing site (later session) constructs it through `aberp_compliance::uid::Iuid::to_iri()` — built only from a validated `IuidConstruct1` / `IuidConstruct2` (validated IAC + EID + part/serial at the write boundary) — so a malformed IUID can never reach the ledger.

### 3. The IRI construction per MIL-STD-130N

The `uid` module renders the UII as the straight concatenation of its construct's data elements:

- **Construct 1:** `IAC + EID + Serial` → e.g. `D0LH12SN-0001`.
- **Construct 2:** `IAC + EID + Original Part Number + Serial` → e.g. `D` + `0LH12` + `BRACKET-7781` + `SN-0001` = `D0LH12BRACKET-7781SN-0001`.

`validate_iac` checks the IAC's *format* (non-empty, ≤ 2 uppercase-alphanumeric chars — the ISO/IEC 15459 shape); whether a code is currently *registered* is an external-registry question out of scope here. The EID / part number / serial use the same `[A-Za-z0-9-]`, length-bounded gate as the S345 `lot_heat` ids — the explicit instruction was to reuse that defensive newtype pattern. Each construct's `new()` validates every component, so an `Iuid` cannot exist in an invalid state; the firing site reads `mil_std_130_compliant` as the boolean verdict that the mark satisfied the standard's format gate.

### 4. A new `part.*` prefix family (the ninth)

The codebase segregates audit traffic by prefix so each consumer's glob stays narrow: `invoice.*` (per-OUTGOING-invoice export bundle, ADR-0009 §8), `system.*`, `mes.*`, `quote.*`, `inventory.*`, `email.*`, `personnel.*` (ADR-0073), `material.*` (ADR-0074). Per-unit serialization events are none of these. Folding them into `material.*` would conflate per-grade traceability (which lot/heat) with per-instance identity (which serial/UID); folding them into `invoice.*` would let the per-invoice export bundle's `invoice.*` glob sweep a serial assignment into an invoice's evidence bundle, which is wrong.

So `part.*` is a **ninth** prefix family. The per-invoice export bundle excludes it by construction (the glob is `invoice.*`); the two workspace exhaustive-match gates (`aberp-verify::extract_nav_xml`, `apps/aberp::export_invoice_bundle::extract_nav_xml`) classify both kinds on the no-NAV-bytes arm — they carry app-layer JSON, never verbatim NAV XML — with a belt-and-braces runtime pin in each.

### 5. Payload contract pinned in tests, not typed structs

Same call as ADR-0073 §3 / ADR-0074 §5: every recent kind stores a free-form `serde_json::Value` built inline at the firing site, not a typed `audit_payloads.rs` struct. S358 has no firing site yet, so a typed struct now would be unused speculative surface (CLAUDE.md #2 / #13). Each payload's documented shape is pinned by a `*_payload_serializes` test that round-trips a sample through serde and asserts the documented fields and JSON types (including the `mil_std_130_compliant` bool). The contract lives in-repo and is enforced; the struct does not exist until a firing site needs it.

## Consequences

**Positive.** The audit ledger now *speaks* per-unit serialization events, and `aberp-compliance` can *build and validate* a MIL-STD-130N IUID; a later session adds firing sites (serial assignment at work-order release, UID marking at the marking station) without re-litigating the vocabulary or the format model. The serial-record / UID-mark split puts the DoD 5000.64 / MIL-STD-130N facts in the same tamper-evident hash chain that already carries the fiscal moat, the access trail, and the material-traceability surface. The new prefix keeps every existing glob consumer untouched.

**Negative / deferred.** Nothing fires these kinds yet — the serialization surface is an empty contract until the firing sites land (which themselves wait on the MES / QA serialization and marking flows existing in code). The payloads are untyped `serde_json::Value`; a future session may promote them to typed structs once a firing site fixes the shape. The `uid` module models the UII *reference string* only — it does not generate the physical 2D Data Matrix / barcode (the actual mark), which is a later `aberp-marking` crate. No data-model column records a part's current serial/UID (there is no part-instance table yet); that is a larger inventory-model change deferred with the firing sites.

**Future work (not S358):** firing sites for both kinds; the serial-entry UI; the UID-marking station integration; the part-instance data model; the `aberp-marking` crate that renders the IUID as a MIL-STD-130N-compliant Data Matrix; wiring the captured CAGE/DUNS into the EID at the firing site.
