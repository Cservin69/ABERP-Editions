# ADR-0076 — `export.*` audit EventKind family + the ITAR/EAR `Jurisdiction` model: the export-control surface.

- **Status:** Proposed
- **Date:** 2026-06-11
- **Deciders:** Ervin (via S359 / PR-46 defense-pivot batch session 4).
- **Supersedes:** none — first ADR of the export-control strand.
- **Related:** ADR-0008 (audit ledger — the hash-chained home these events live in), ADR-0071 (`aberp-compliance` crate — home of the `export_control` module these events lean on; wired as a dep of `apps/aberp` by S355), ADR-0073 (`personnel.*` family — the access-trail strand whose access-decision posture the `export.access_check` kind mirrors), ADR-0074 (`material.*` family — same no-firing-site-yet posture and three-kind split rationale), ADR-0075 (`part.*` family — the per-unit serialization strand, immediately preceding prefix family), the defense-aerospace gap analysis (S330, `[[defense-aerospace-pivot]]`), and `[[mock-everything-principle]]`, `[[trust-code-not-operator]]`.

## Context

The aerospace/defense gap analysis (S330) named export-control compliance as a structural requirement of the pivot: a U.S. manufacturer of controlled parts and technical data must record, in a tamper-evident form, *what* an artifact is classified as, *who* may access it, and *when* controlled goods physically leave.

Two distinct bodies of U.S. law govern exports:

1. **ITAR** — the International Traffic in Arms Regulations (22 CFR §§ 120-130), administered by the State Department's Directorate of Defense Trade Controls (DDTC). Governs defense articles and defense services on the **United States Munitions List (USML)**. A USML item is identified by its **USML category** (e.g. Category VIII for aircraft). ITAR's reach includes "technical data" (drawings, specs, software) and the **deemed-export** rule (22 CFR § 120.62): disclosing ITAR technical data to a foreign person — even on U.S. soil — is itself an export. That makes the *access-decision* trail load-bearing, not just the physical-shipment trail.
2. **EAR** — the Export Administration Regulations (15 CFR §§ 730-774), administered by Commerce's Bureau of Industry and Security (BIS). Governs dual-use and most commercial items on the **Commerce Control List (CCL)**. A CCL item is identified by its **ECCN** (Export Control Classification Number, e.g. `7A994`). The catch-all for items *subject to the EAR but not listed on the CCL* is **EAR99** — most commercial goods, usually exportable without a licence (subject to embargo / denied-party screening).

`aberp-compliance::export_control` (S345 / ADR-0071) already scaffolded the *classification + screening* boundary: the `ExportControlProvider` trait, the `ExportClassification` enum (ECCN code / USML category / EAR99 / NotClassified / Pending), `Classifiable`, `PartyRef` / `ScreeningResult`, and the `MockExportControlProvider`. What it lacked is (a) the **typed jurisdiction axis** the audit events need to record, and (b) the **audit vocabulary** so a forensic walker can glob "every export-control determination / access-check / shipment on this install" without scraping free text. This session adds both. It does **not** wire firing sites — no drawing/spec/document workflow exists in code yet — and it does **not** add a real BIS/State.gov classification backend (the mock answers `NotClassified` until a real customer demands real classification, per `[[mock-everything-principle]]`).

## Decision

**A new `export.*` EventKind prefix family with three members, plus a new `Jurisdiction` enum in `aberp_compliance::export_control` holding the ITAR/EAR/EAR99/NOT_CONTROLLED/UNKNOWN regime axis. The kinds carry free-form `serde_json::Value` payloads (the same posture every recent `quote.*` / `personnel.*` / `material.*` / `part.*` kind takes); their documented field shapes are pinned by serialization tests so a later firing site has a stable contract. No firing site is added in S359.**

### 1. The three kinds

| EventKind | storage string | fires when | payload |
| --- | --- | --- | --- |
| `ExportClassificationSet` | `export.classification_set` | a drawing / spec / document is export-classified | `{entity_kind, entity_id, eccn?, usml_category?, jurisdiction, classified_by_operator_id, classified_at_ms}` |
| `ExportAccessCheck` | `export.access_check` | access to an export-controlled artifact is checked | `{entity_kind, entity_id, requesting_operator_id, decision, reason, checked_at_ms}` |
| `ExportShipmentLogged` | `export.shipment_logged` | an export-controlled shipment leaves | `{shipment_id, exporter_party_id, recipient_party_id, recipient_country, ecn_or_authorization?, shipped_at_ms, shipped_by_operator_id}` |

### 2. Why three kinds (classification / access / shipment) and not one

These are three different facts in the export-control lifecycle, separated for the same reason ADR-0073 split identity from access and ADR-0074 split cert from heat/lot: distinct facts get distinct kinds so none is inferred by diffing payloads.

- **`classification_set` is a determination record.** It answers *"what is this artifact, under which regime, decided by whom?"* It is purely additive — a re-classification is a new record, and the history reconstructs how an artifact's jurisdiction was decided over time. It is the anchor the other two kinds reference.
- **`access_check` is a decision event.** It answers *"who asked for this artifact, were they granted or denied, and why?"* ITAR's deemed-export rule makes *every* access a potentially reportable export, so both grants and denials are recorded (not denials-only). Its `decision` / `reason` shape is the `personnel.access_granted` / `personnel.access_denied` posture (ADR-0073) specialised to an export-controlled artifact rather than a generic resource.
- **`shipment_logged` is a physical-export event.** It answers *"what controlled goods crossed to which party / country under which authorization?"* Its fields barely overlap the other two (party ids, destination country, the cited licence / exception / ECCN); folding it into either would be a mostly-null union. It is the record a BIS / DDTC auditor resolves an export's lawfulness through.

A single `export.event` kind with a `type` discriminator was rejected: it would force every consumer to branch on an in-payload tag the prefix family is meant to make globbable, and it would defeat the per-kind exhaustiveness gate (a new sub-type would slip in without a compile error).

### 3. The `Jurisdiction` enum — a distinct axis from `ExportClassification`

The `export.classification_set` payload's `jurisdiction` field is one of five regime tokens: `ITAR` / `EAR` / `EAR99` / `NOT_CONTROLLED` / `UNKNOWN`. This is modelled as a **new** `Jurisdiction` enum in `aberp_compliance::export_control`, *not* by extending the existing `ExportClassification`.

`ExportClassification` and `Jurisdiction` are two different axes:

- `ExportClassification` answers *"what is the code?"* — an ECCN string, a USML category string, or the bare EAR99 catch-all (plus `NotClassified` / `Pending` lifecycle states).
- `Jurisdiction` answers *"which body of law governs it?"* — the regime.

They overlap only at EAR99 (both a classification and, trivially, an EAR-jurisdiction item). Cramming the regime tokens into `ExportClassification` would have produced category errors: an `ExportClassification::ITAR` variant makes no sense — ITAR is the *regime*; the USML category is its *classification*. Per CLAUDE.md #7 ("surface conflicts, don't average them") and #2/#13 (simplicity, delete-don't-bloat), the two concerns stay separate. `Jurisdiction` is a small `Copy` enum with an `as_str` / `from_storage_str` round-trip mirroring the `EventKind` discipline, so the firing site renders the audit payload's `jurisdiction` string through the typed enum — a free-text regime can never reach the ledger, and a mis-parse fails loud (CLAUDE.md #12) rather than defaulting to a regime.

`UNKNOWN` is the conservative default (no determination made), distinct from `NOT_CONTROLLED` (a *positive* determination that the item is neither ITAR nor EAR, e.g. published / public-domain information per EAR § 734.7).

### 4. The `MockExportControlProvider` boundary

Classification is a felony to get wrong, so the real determination comes from a licensed classification service / commodity-jurisdiction determination — never inferred in code. S345 already established the swap-point: `MockExportControlProvider` answers `ExportClassification::NotClassified` + `ScreeningResult::Clear` for everything and WARNs loudly on construction so a production boot that falls through to it is never silent. S359 does **not** change that — it adds the `Jurisdiction` vocabulary the *audit record* needs, but the *determination* still comes from the (currently mock) provider. A real BIS/State.gov backend slots in behind the same trait when a customer demands it, per `[[mock-everything-principle]]` (`[[defense-aerospace-pivot]]`). The mock's `NotClassified` maps to `Jurisdiction::Unknown` at the future firing site — the honest "we have not classified this yet" posture.

### 5. A new `export.*` prefix family (the tenth)

The codebase segregates audit traffic by prefix so each consumer's glob stays narrow: `invoice.*` (per-OUTGOING-invoice export bundle, ADR-0009 §8), `system.*`, `mes.*`, `quote.*`, `inventory.*`, `email.*`, `personnel.*` (ADR-0073), `material.*` (ADR-0074), `part.*` (ADR-0075). Export-control events are none of these. Folding them into `personnel.*` would conflate operator-identity / generic-access traffic with export-jurisdiction determinations; folding them into `invoice.*` would let the per-invoice export bundle's `invoice.*` glob sweep an export-control row into an invoice's evidence bundle (note the unfortunate name collision: the per-*OUTGOING-invoice* "export bundle" of ADR-0009 is unrelated to *export-control* — keeping `export.*` out of `invoice.*` is exactly what prevents the two from tangling).

So `export.*` is a **tenth** prefix family. The per-invoice export bundle excludes it by construction (the glob is `invoice.*`); the two workspace exhaustive-match gates (`aberp-verify::extract_nav_xml`, `apps/aberp::export_invoice_bundle::extract_nav_xml`) classify all three kinds on the no-NAV-bytes arm — they carry app-layer JSON, never verbatim NAV XML — with a belt-and-braces runtime pin in `export_invoice_bundle`.

### 6. Payload contract pinned in tests, not typed structs

Same call as ADR-0073 §3 / ADR-0074 §5 / ADR-0075 §5: every recent kind stores a free-form `serde_json::Value` built inline at the firing site, not a typed `audit_payloads.rs` struct. S359 has no firing site yet, so a typed struct now would be unused speculative surface (CLAUDE.md #2 / #13). Each payload's documented shape is pinned by a `*_payload_serializes` test that round-trips a sample through serde and asserts the documented fields and JSON types (including the optional `eccn` / `usml_category` / `ecn_or_authorization` and the `decision` string). The contract lives in-repo and is enforced; the struct does not exist until a firing site needs it.

## Consequences

**Positive.** The audit ledger now *speaks* export-control events, and `aberp-compliance` can *type* the ITAR/EAR jurisdiction axis the classification record needs; a later session adds firing sites (classification at drawing release, access-check at artifact open, shipment-logged at dispatch) without re-litigating the vocabulary or the regime model. The classification / access / shipment split puts the three export-control facts in the same tamper-evident hash chain that already carries the fiscal moat, the access trail, the material-traceability surface, and the per-unit serialization surface. The new prefix keeps every existing glob consumer untouched.

**Negative / deferred.** Nothing fires these kinds yet — the export-control surface is an empty contract until firing sites land (which themselves wait on a drawing/spec/document workflow existing in code). The payloads are untyped `serde_json::Value`; a future session may promote them to typed structs once a firing site fixes the shape. Classification itself is still the mock's `NotClassified` until a real backend is demanded — the events can *record* a determination, but nothing *makes* a real one yet. No data-model table records an artifact's current classification or a shipment's line items; that is a larger document-model change deferred with the firing sites.

**Future work (not S359):** firing sites for all three kinds; the drawing/spec/document data model; the classification UI; the access-control gate that consults `ExportControlProvider` + emits `export.access_check`; a real BIS/State.gov classification backend behind `ExportControlProvider`; the deemed-export U.S.-person check wired into the access gate; capturing the exporter-of-record / consignee parties from the dispatch flow into the shipment payload.
