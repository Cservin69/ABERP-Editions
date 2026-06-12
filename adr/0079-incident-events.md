# ADR-0079 — `incident.*` audit EventKind family + cyber-incident vocabulary: the DFARS 252.204-7012 reporting spine.

- **Status:** Proposed
- **Date:** 2026-06-12
- **Deciders:** Ervin (via S362 / PR-49 defense-pivot batch session 7).
- **Supersedes:** none — first ADR of the cyber-incident-reporting strand.
- **Related:** ADR-0008 (audit ledger — the hash-chained home these events live in), ADR-0071 (`aberp-compliance` crate — home of the new `incident` module; wired as a dep of `apps/aberp` by S355), ADR-0073 (`personnel.*` family — the access-trail strand whose decision-event posture this kind shares), ADR-0076 (`export.*` family — the export-control strand whose `Jurisdiction` storage-string discipline the `IncidentSeverity` / `DetectionSource` newtypes parallel), ADR-0077 (`cui.*` family — the CUI strand whose `cui_affected` boolean this kind references), ADR-0078 (`supplier.*` family — immediately preceding prefix family, same no-firing-site-yet posture and storage-string-newtype rationale; relevant when the detected incident is a supplier breach), the defense-aerospace gap analysis (S330, `[[defense-aerospace-pivot]]`), and `[[mock-everything-principle]]`, `[[trust-code-not-operator]]`.

## Context

A defense contractor that processes, stores, or transmits **Controlled Defense Information (CDI)** on its covered information systems is bound by **DFARS 252.204-7012**. Subparagraph **(c)(1)** is the load-bearing obligation: when the contractor *discovers* a cyber incident that affects CDI (or its ability to perform requirements designated operationally critical), it must **rapidly report** the incident to the DoD **within 72 hours of discovery** — the report is filed through the DIBNet medium-assurance portal and surfaces in SPRS. The 72-hour clock starts at *discovery*, so the moment of detection — *when* it was found, *who* found it, *what* it touched, *whether* CDI / CUI is implicated — is the accountable anchor the whole reporting obligation hangs off.

ABERP's audit ledger (ADR-0008) is the tamper-evident hash chain that already carries the fiscal moat, the access trail (ADR-0073), material traceability (ADR-0074), per-unit serialization (ADR-0075), export-control (ADR-0076), CUI (ADR-0077), and supplier-AVL (ADR-0078) surfaces. A cyber-incident detection is exactly the kind of fact that belongs in that chain: a forensic walker auditing a DFARS posture must be able to glob "every cyber-incident detection on this install" and reconstruct, for each, the metadata that proves the 72-hour clock was started and tracked. The ledger had no vocabulary for it. This session adds that vocabulary. It does **not** wire a firing site (no incident-entry surface exists in code yet), it does **not** submit anything to SPRS, and it does **not** integrate a SIEM — all of that is later, mock-first.

## Decision

**A new `incident.*` EventKind prefix family with one member, plus a small `aberp_compliance::incident` module of closed vocabularies (`IncidentSeverity` / `DetectionSource`, each with an `as_str` / `from_storage_str` storage-string pair) and the `dod_72h_report_due_at_ms` deadline helper. The kind carries a free-form `serde_json::Value` payload (the same posture every recent `personnel.*` / `material.*` / `part.*` / `export.*` / `cui.*` / `supplier.*` kind takes); its documented field shape is pinned by a serialization test with all fields populated. No firing site is added in S362.**

### 1. The one kind

| EventKind | storage string | fires when | payload |
| --- | --- | --- | --- |
| `IncidentCyberDetected` | `incident.cyber_detected` | a cyber incident affecting (or potentially affecting) a covered system is detected | `{detected_at_ms, operator_user_id, severity, scope_description, cdi_affected, ocs_affected, cui_affected, exfiltration_suspected, affected_systems, detection_source, mitigation_notes?, dod_72h_report_due_at_ms?}` |

### 2. Why one kind (detection) and not a detect/report/close lifecycle

DFARS 252.204-7012(c)(1) is the *detection-and-report* obligation; the durable, accountable fact S362 must capture is the **detection** that starts the 72-hour clock. The downstream facts — *the report was submitted to SPRS*, *the incident was closed* — are real, but they are **future** events tied to surfaces that do not exist yet (no SPRS submission path, no incident-close workflow). Shipping a `report_submitted` / `incident_closed` kind now would be speculative audit surface (CLAUDE.md #2 / #13) with no firing site and no tested contract. The detection kind is the spine; the lifecycle kinds get added when the workflows that emit them exist. This mirrors the every-recent-family discipline of adding kinds only as the facts they record become real.

### 3. The payload — what is and is NOT at rest

The payload carries enough to evidence the reporting obligation was triggered and tracked, and **nothing that would itself be a leak**:

- `detected_at_ms` — the epoch-ms discovery stamp, the 72-hour clock's start.
- `operator_user_id` — an **opaque accountability handle**, never PII (same posture as every `personnel.*` / `supplier.*` operator id; the canonical operator-identity key — see the S367 / F1 correction at the foot of this ADR).
- `severity` — the rendered `IncidentSeverity::as_str` string (`informational` / `low` / `medium` / `high` / `critical`).
- `scope_description` — a **free-text scope summary**, NOT raw log dumps. The ledger records *what was affected at a summary level*, not the forensic evidence itself; the actual logs / packet captures live in the incident-response system, not the audit chain. This is the same no-controlled-content-at-rest posture ADR-0077 takes for CUI (the `cui.*` payload never carries the controlled content).
- `cdi_affected` / `ocs_affected` / `cui_affected` / `exfiltration_suspected` — booleans. `cdi_affected` **and** `ocs_affected` are the **two halves** of the DFARS 252.204-7012(c)(1)(i) trigger: CDI affected (A) OR operationally critical support affected (B). `cui_affected` references 32 CFR Part 2002 (ADR-0077's domain).
- `affected_systems` — a string array of system **identifiers**, not their contents.
- `detection_source` — the rendered `DetectionSource::as_str` string (`siem` / `user_report` / `vendor_notification` / `audit` / `other`).
- `mitigation_notes?` — optional free-text.
- `dod_72h_report_due_at_ms?` — optional; present when `cdi_affected` **or** `ocs_affected` is true (either half of the clause's trigger), computed by `dod_72h_report_due_at_ms(detected_at_ms, cdi_affected, ocs_affected)` which returns `Some(deadline)` on a trigger and `None` otherwise. Storing the deadline in the row makes the reporting obligation self-evident to a forensic walker without re-deriving it.

### 4. The `incident` compliance module — closed vocabularies + deadline arithmetic

Two payload fields (`severity`, `detection_source`) are closed vocabularies, so they get the same storage-string-newtype treatment the `export.*` / `supplier.*` families established (ADR-0076 §3 / ADR-0078 §4): `IncidentSeverity` and `DetectionSource`, each with an `as_str` / `from_storage_str` round-trip-proven pair. The firing site (later session) validates an inbound severity / source through `from_storage_str` before it reaches the ledger — a free-text value can never be persisted, and an unknown string **fails loud** (a mis-parse of an unrecognised severity to `Informational` would silently downgrade a reportable incident below the 72-hour threshold).

The module also owns the **deadline arithmetic**: `dod_72h_report_due_at_ms(detected_at_ms)` returns `detected_at_ms + DFARS_72H_REPORT_WINDOW_MS` (72 h in ms). One function, one place — the 72-hour window is added one way everywhere, not re-spelled at each call site. It is pure arithmetic (no `Date::now()`); the caller supplies the detection stamp and the deadline derives deterministically. The enums are deliberately **not** `serde`-derived: unlike the `avl` records they are only ever stored as their `as_str` token inside a `serde_json::Value` payload, so a `Serialize` / `Deserialize` impl would be unused surface (CLAUDE.md #13) — the round-trip pair is the whole contract.

This is a **conservative, load-bearing** module, not speculative scaffolding: the two enums back two real payload fields and the helper backs a real (optional) payload field. It is the minimum the `incident.cyber_detected` contract needs, no more — the `IncidentDetectionProvider` trait + mock (the SIEM swap-point) is deferred to the session that wires real detection, per `[[mock-everything-principle]]`.

### 5. A new `incident.*` prefix family (the thirteenth)

The codebase segregates audit traffic by prefix so each consumer's glob stays narrow: `invoice.*` (per-OUTGOING-invoice export bundle, ADR-0009 §8), `system.*`, `mes.*`, `quote.*`, `inventory.*`, `email.*`, `personnel.*` (ADR-0073), `material.*` (ADR-0074), `part.*` (ADR-0075), `export.*` (ADR-0076), `cui.*` (ADR-0077), `supplier.*` (ADR-0078). A cyber-incident detection is none of these. Folding it into `system.*` would conflate operational system events with security incidents; folding it into `cui.*` would imply every incident is a CUI access event (most are not); folding it into `invoice.*` would let the per-invoice export bundle's `invoice.*` glob sweep an incident row into an invoice's evidence bundle.

So `incident.*` is a **thirteenth** prefix family. The per-invoice export bundle excludes it by construction (the glob is `invoice.*`); the two workspace exhaustive-match gates (`aberp-verify::extract_nav_xml`, `apps/aberp::export_invoice_bundle::extract_nav_xml`) classify the kind on the no-NAV-bytes arm — it carries app-layer JSON, never verbatim NAV XML — with a belt-and-braces runtime pin in `export_invoice_bundle`.

### 6. The mock-everything boundary for SPRS / DIBNet + SIEM

The real DFARS reporting workflow consults two external systems S362 deliberately stubs out: a **SIEM / IDS** that detects the incident (the upstream source of `detection_source: siem`), and the **DIBNet / SPRS portal** that receives the 72-hour report (the downstream submission). S362 adds **neither**: per `[[mock-everything-principle]]`, the detection swap-point will be an `IncidentDetectionProvider` trait + mock (later session), and the SPRS submission is a wholly separate downstream concern (later session). The `incident.*` audit vocabulary and the `incident` compliance types are the durable contract; the real SIEM feed and the real SPRS submission slot in behind those boundaries later without re-litigating the event shape or the storage tokens. The `detection_source` field exists precisely so a mock feed (`other` / a manual `user_report`) and a future real SIEM (`siem`) are distinguishable in the ledger.

### 7. Payload contract pinned in tests, not typed structs

Same call as ADR-0073 §3 / ADR-0076 §6 / ADR-0077 §6 / ADR-0078 §7: every recent kind stores a free-form `serde_json::Value` built inline at the firing site, not a typed `audit_payloads.rs` struct. S362 has no firing site yet, so a typed struct now would be unused speculative surface (CLAUDE.md #2 / #13). The payload's documented shape is pinned by a `*_payload_serializes` test that round-trips a sample **with all fields populated** (both optionals included) through serde and asserts the documented fields and JSON types — including that `dod_72h_report_due_at_ms` is exactly 72 h after `detected_at_ms`. The contract lives in-repo and is enforced; the struct does not exist until a firing site needs it.

## Consequences

**Positive.** The audit ledger now *speaks* cyber-incident detection: a forensic walker auditing a DFARS 252.204-7012 posture can glob `incident.*` and reconstruct, for each detection, when the 72-hour clock started, who logged it, what it touched, and whether CDI / CUI is implicated — all inside the same tamper-evident hash chain that carries the rest of the compliance moat. `aberp-compliance` has the canonical severity / source vocabulary and the one-place deadline arithmetic both the (future) firing site and any future incident-record column need. The new prefix keeps every existing glob consumer untouched. The SIEM feed and SPRS submission stay un-wired, so no live external call is made until a session deliberately wires one.

**Negative / deferred.** Nothing fires this kind yet — the cyber-incident surface is an empty contract until a firing site lands (which itself waits on an incident-entry surface existing in code). The payload is an untyped `serde_json::Value`; a future session may promote it to a typed struct once a firing site fixes the shape. There is no incident-entry UI, no SPRS submission path, no SIEM integration, and no automated 72-hour deadline alerting. The `dod_72h_report_due_at_ms` field is computed but nothing *acts* on the deadline yet (the alerting cron is future work). Only the detection kind exists — no `report_submitted` / `incident_closed` lifecycle kinds.

**Future work (not S362):** the firing site for the detection kind; the incident-entry surface (log a detected incident); an `IncidentDetectionProvider` trait + mock for the SIEM swap-point; the SPRS / DIBNet report-submission path (mock-first); automated 72-hour deadline alerting (a cron that surfaces incidents approaching the DFARS deadline); the `report_submitted` / `incident_closed` lifecycle kinds once those workflows exist; promoting the `incident.cyber_detected` payload to a typed struct once a firing site fixes the shape.

## §5 — Corrections (S367, 2026-06-12)

- **Review F16 — OCS half of the trigger was missing.** S362 stamped `dod_72h_report_due_at_ms` only when `cdi_affected` was true, and the payload had no operationally-critical-support flag. DFARS 252.204-7012(c)(1)(i) triggers rapid reporting on a cyber incident affecting CDI **(A) OR** the contractor's ability to perform requirements designated **operationally critical support (B)**. S367 added the `ocs_affected: bool` payload field and changed `dod_72h_report_due_at_ms(detected_at_ms, cdi_affected, ocs_affected) -> Option<i64>` to return the deadline when *either* trigger fires. No firing site or ledger row existed, so the contract was widened freely.
- **Review F1 — operator-identity key canonicalized.** This ADR's payload originally named the operator field `reporter_operator_id`. S366 found ten different operator-identity spellings across ADR-0073…0079; S367 canonicalized them all to **`operator_user_id`** (the Bearer-subject convention already used by the two firing families in `apps/aberp::serve`). See the `aberp_compliance::prelude` "Identity-key canonicalization" note for the standing rule.
