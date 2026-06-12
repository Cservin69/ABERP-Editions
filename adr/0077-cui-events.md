# ADR-0077 — `cui.*` audit EventKind family + the CUI banner model: the Controlled-Unclassified-Information surface.

- **Status:** Proposed
- **Date:** 2026-06-12
- **Deciders:** Ervin (via S360 / PR-47 defense-pivot batch session 5).
- **Supersedes:** none — first ADR of the CUI-handling strand.
- **Related:** ADR-0008 (audit ledger — the hash-chained home these events live in), ADR-0071 (`aberp-compliance` crate — home of the `cui` module these events lean on; wired as a dep of `apps/aberp` by S355), ADR-0073 (`personnel.*` family — the access-trail strand whose access-decision posture the `cui.access_event` kind mirrors), ADR-0074 (`material.*` family) and ADR-0075 (`part.*` family — same no-firing-site-yet posture and kind-split rationale), ADR-0076 (`export.*` family — the export-control strand, immediately preceding prefix family, whose `classification_set` / `access_check` shape this family parallels), the defense-aerospace gap analysis (S330, `[[defense-aerospace-pivot]]`), and `[[mock-everything-principle]]`, `[[trust-code-not-operator]]`.

## Context

The aerospace/defense gap analysis (S330) named CUI handling as a structural requirement of the pivot: a U.S. manufacturer that receives or generates Controlled Unclassified Information — controlled technical information, export-controlled data, proprietary flowdowns — must record, in a tamper-evident form, *what marking* an artifact carries and *who accessed it*.

**CUI** is the governmentwide category established by Executive Order 13556 and codified in **32 CFR Part 2002**, administered by NARA's Information Security Oversight Office (ISOO). It replaced the legacy patchwork (FOUO, SBU, etc.) with a single registry of categories (Controlled Technical Information, Privacy, Export Control, Proprietary Business Information, …) and a uniform **banner-marking convention**: `<CONTROL>//<CATEGORY>//<LIMITED-DISSEMINATION>`, e.g. `CUI//SP-PROPIN//FEDCON`. Two handling rules make the audit trail load-bearing:

1. **Lawful government purpose (32 CFR § 2002.4 / § 2002.16):** access to CUI is permitted only for a lawful government purpose. That makes the *access-decision* trail — who asked, granted or denied, why — a compliance artifact, not just an operational log.
2. **Marking accountability (32 CFR § 2002.20):** the designating authority must mark CUI at the point of designation. The *who marked it / when* record is the accountability anchor.

`aberp-compliance::cui` (S345 / ADR-0071) already scaffolded the *marking vocabulary*: the `CuiMarking` enum (Unclassified / Cui(category) / Confidential / Secret / TopSecret), the `CuiCategory` starter subset, and `display_marking` / `is_cui` / `is_classified` helpers. What it lacked is (a) the **limited-dissemination segment** the full DoD banner needs, and (b) the **audit vocabulary** so a forensic walker can glob "every CUI marking / access on this install" without scraping free text. This session adds both. It does **not** wire firing sites — no document/drawing/spec workflow exists in code yet — and it does **not** add a real GRC backend (markings come from the typed model, per `[[mock-everything-principle]]`).

## Decision

**A new `cui.*` EventKind prefix family with two members, plus a `DisseminationControl` enum and a `to_banner_str` helper in `aberp_compliance::cui`. The kinds carry free-form `serde_json::Value` payloads (the same posture every recent `personnel.*` / `material.*` / `part.*` / `export.*` kind takes); their documented field shapes are pinned by serialization tests so a later firing site has a stable contract. No firing site is added in S360.**

### 1. The two kinds

| EventKind | storage string | fires when | payload |
| --- | --- | --- | --- |
| `CuiMarkingApplied` | `cui.marking_applied` | a CUI banner is applied to a document / drawing / spec | `{entity_kind, entity_id, cui_marking_str, operator_user_id, applied_at_ms}` |
| `CuiAccessEvent` | `cui.access_event` | access to a CUI-marked artifact is checked | `{entity_kind, entity_id, operator_user_id, decision, reason, accessed_at_ms}` |

### 2. Why two kinds (marking / access) and not one

These are two different facts in the CUI lifecycle, separated for the same reason ADR-0073 split identity from access and ADR-0076 split classification from access: distinct facts get distinct kinds so none is inferred by diffing payloads.

- **`marking_applied` is a designation record.** It answers *"what banner does this artifact carry, applied by whom?"* It is purely additive — a re-marking is a new record, and the history reconstructs how an artifact's marking was decided over time. The `cui_marking_str` is the rendered DoD banner (see §4); the controlled content itself is never carried.
- **`access_event` is a decision event.** It answers *"who asked for this artifact, were they granted or denied, and why?"* CUI's lawful-government-purpose rule makes *every* access a compliance-relevant decision, so both grants and denials are recorded (not denials-only). Its `decision` / `reason` shape is the `personnel.access_granted` / `personnel.access_denied` (ADR-0073) and `export.access_check` (ADR-0076) posture specialised to a CUI-marked artifact.

A single `cui.event` kind with a `type` discriminator was rejected for the same reasons ADR-0076 §2 gives: it would force every consumer to branch on an in-payload tag the prefix family is meant to make globbable, and it would defeat the per-kind exhaustiveness gate.

### 3. No PII / no controlled content at rest

The audit ledger is the durable, hash-chained, forever-retained record. CUI banners frequently sit on artifacts whose *content* is itself controlled (export-controlled technical data, privacy records). So the payloads deliberately record only **which** artifact (`entity_id`, an opaque key) and **who** acted (`operator_user_id`, an opaque accountability handle) and the **banner string** — never the controlled body, never a name or other PII. The marking record proves a marking happened; resolving the artifact's content is a separate, access-gated lookup. This keeps the ledger itself from becoming a CUI spillage vector (CLAUDE.md #12 — fail loud, never silently widen what is stored).

### 4. The `DisseminationControl` enum + `to_banner_str` — completing the DoD banner

S345's `display_marking` renders the base marking (`UNCLASSIFIED`, `CUI//CTI`, `SECRET`, …) but not the trailing limited-dissemination segment the full DoD banner carries (`CUI//CTI//NOFORN`). S360 adds:

- **`DisseminationControl`** — a small `Copy` enum, a deliberate starter subset (`NOFORN` / `FEDCON` / `NOCON` / `DL ONLY`) mirroring the `CuiCategory` starter-subset posture, each with an `abbreviation()`. These are an *orthogonal axis* to the category: the category says *what kind* of CUI it is, the dissemination control says *who may receive it*.
- **`CuiMarking::to_banner_str(&self, dissemination: &[DisseminationControl])`** — the full banner: `display_marking()` plus a trailing `//<DISSEM1>/<DISSEM2>` segment when controls are supplied. With an empty slice it is exactly `display_marking()`, so the no-further-limits form falls out for free. This is the string the `cui.marking_applied` payload's `cui_marking_str` carries — rendered from typed values so a free-text banner can never reach the ledger (the same discipline ADR-0076 §3 applied to `Jurisdiction`).

`to_banner_str` is purely additive and is the payload-facing helper the brief named. Modelling multiple categories in one banner (`CUI//SP-PROPIN//SP-PRVCY`) is **not** done — `CuiMarking::Cui` holds a single category, and a multi-category artifact is not representable in code yet; promoting `Cui` to a category set is deferred to a firing site that actually needs it (CLAUDE.md #2 / #13 — no speculative bloat).

> **Correction (S367, 2026-06-12 — review F10):** S360 originally rendered every category as `CUI//<ABBREV>`, dropping the **`SP-` Specified prefix**. The DoD CUI Registry (and the DoD CUI Marking Handbook / 32 CFR 2002.20) distinguishes **CUI Basic** from **CUI Specified**: a Specified category is governed by a law / regulation / government-wide policy prescribing specific controls, and its banner takes the `SP-` prefix — `CUI//SP-CTI`, not `CUI//CTI`. Of the starter `CuiCategory` subset, **CTI** (Controlled Technical Information; DoDI 5230.24 / DFARS) and **EXPT** (Export Controlled; ITAR / EAR) are Specified. S367 added `CuiCategory::is_specified()` and made `display_marking` (hence `to_banner_str`) prepend `SP-` for Specified categories; the remaining categories are treated as CUI Basic until a registry row demands otherwise. The §4 examples above (`CUI//CTI//NOFORN`) reflect the pre-correction abbreviation form for a Basic-style category; a Specified category now renders `CUI//SP-CTI//NOFORN`. No firing site or ledger row existed, so the change rewrote zero rows.

### 5. A new `cui.*` prefix family (the eleventh)

The codebase segregates audit traffic by prefix so each consumer's glob stays narrow: `invoice.*` (per-OUTGOING-invoice export bundle, ADR-0009 §8), `system.*`, `mes.*`, `quote.*`, `inventory.*`, `email.*`, `personnel.*` (ADR-0073), `material.*` (ADR-0074), `part.*` (ADR-0075), `export.*` (ADR-0076). CUI-handling events are none of these. Folding them into `personnel.*` would conflate operator-identity / generic-access traffic with CUI-marking designations; folding them into `export.*` would conflate the export-jurisdiction surface with the broader CUI surface (export control is one CUI category, not the whole of it); folding them into `invoice.*` would let the per-invoice export bundle's `invoice.*` glob sweep a CUI row into an invoice's evidence bundle.

So `cui.*` is an **eleventh** prefix family. The per-invoice export bundle excludes it by construction (the glob is `invoice.*`); the two workspace exhaustive-match gates (`aberp-verify::extract_nav_xml`, `apps/aberp::export_invoice_bundle::extract_nav_xml`) classify both kinds on the no-NAV-bytes arm — they carry app-layer JSON, never verbatim NAV XML — with a belt-and-braces runtime pin in `export_invoice_bundle`.

### 6. Payload contract pinned in tests, not typed structs

Same call as ADR-0073 §3 / ADR-0074 §5 / ADR-0075 §5 / ADR-0076 §6: every recent kind stores a free-form `serde_json::Value` built inline at the firing site, not a typed `audit_payloads.rs` struct. S360 has no firing site yet, so a typed struct now would be unused speculative surface (CLAUDE.md #2 / #13). Each payload's documented shape is pinned by a `*_payload_serializes` test that round-trips a sample through serde and asserts the documented fields and JSON types (including the `decision` string). The contract lives in-repo and is enforced; the struct does not exist until a firing site needs it.

## Consequences

**Positive.** The audit ledger now *speaks* CUI-handling events, and `aberp-compliance` can *render* the full DoD banner the marking record needs; a later session adds firing sites (marking at document designation, access-check at artifact open) without re-litigating the vocabulary or the banner model. The marking / access split puts the two CUI facts in the same tamper-evident hash chain that already carries the fiscal moat, the access trail, the material-traceability surface, the per-unit serialization surface, and the export-control surface. The new prefix keeps every existing glob consumer untouched. The no-content-at-rest stance keeps the ledger from becoming a CUI spillage vector.

**Negative / deferred.** Nothing fires these kinds yet — the CUI surface is an empty contract until firing sites land (which themselves wait on a document/drawing/spec workflow existing in code). The payloads are untyped `serde_json::Value`; a future session may promote them to typed structs once a firing site fixes the shape. `CuiMarking::Cui` still holds a single category; the multi-category banner is deferred. No data-model table records an artifact's current marking; that is a larger document-model change deferred with the firing sites. The `DisseminationControl` / `CuiCategory` sets are starter subsets — real flowdowns will demand specific additions.

**Future work (not S360):** firing sites for both kinds; the document/drawing/spec data model; the CUI-marking-entry UI; the access-control gate that consults the marking + emits `cui.access_event`; the lawful-government-purpose check wired into the access gate; promoting `CuiMarking::Cui` to a category set for multi-category banners; encrypting the underlying document store (a separate session).
