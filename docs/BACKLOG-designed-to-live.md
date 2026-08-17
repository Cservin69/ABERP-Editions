# Backlog — Designed → Live

Every capability the [README](../README.md) lists as **Designed — awaiting
hardware/endpoint** has one entry here. The goal for each is to drive it to
**Live**: a real emitter or handler, exercised end to end.

The file also tracks **expansion slots on capabilities that are already
Live** — a base that works, sitting on a seam with more capacity than is
currently drawn on. [D-16](#d-16) is the first of these. An expansion entry
never downgrades the Live claim it hangs off; it itemises what is *not yet*
drawn from a socket that already carries load.

The two lists are **1:1**. If you add a Designed row or an expansion
pointer to the README, add an entry here; if an entry reaches Live, flip the
README row and delete the entry. A README reference with no backlog anchor
(or the reverse) is a drift bug.

Each entry records:

- **Surface today** — the code that already exists, cited by file. This is
  what justifies listing the capability at all; a capability with no code
  surface does not belong in either document.
- **Missing for Live** — the concrete hardware, external endpoint, or
  wiring that is absent.
- **Blocked on** — whether it is waiting on something external (an
  account, credentials, a machine, a licence) or is purely our own work.
  This is the useful axis: the externally-blocked ones cannot be scheduled,
  the unblocked ones can.
- **Size** — a rough shape only, inferred from the surface that exists. Not
  an estimate, and there are no dates anywhere in this file.

Line numbers drift; treat them as a starting point, not a contract.

---

## Externally blocked

Nothing here can start until something outside the repo arrives.

<a id="d-01"></a>
### D-01 — Denied-party / export-control screening backend

**Surface today.** `ExportControlProvider` trait with `classify` and
`screen_party` (`crates/aberp-compliance/src/export_control/mod.rs:389`).
`MockExportControlProvider` is the only implementation
(`crates/aberp-compliance/src/export_control/mock.rs:17`), installed at boot
(`apps/aberp/src/serve.rs:18203`). The consuming half is already Live: all
three `export.*` kinds fire inside `mark_shipped`'s transaction, a blocking
decision refuses the shipment, and the recorded row carries
`decision: "not_determined"` with `backend: "mock"` so a mock answer can be
told apart from a real screen after the data leaves the process.

**Missing for Live.** A real screening service behind the trait — the US
Consolidated Screening List or a commercial equivalent — plus classification
data for the commodity side. Nothing else has to move: the trait, the
firing sites, the atomicity, and the `backend` tag are all in place.

**Blocked on.** A chosen provider and its account/API access.

**Size.** Small once a provider is picked — one trait impl plus its error
mapping. The `AccessDecision` vocabulary already covers
`granted` / `restricted` / `denied` / `not_determined`.

**Do not regress.** The mock must keep answering `not_determined`. Making
it answer `granted` would write an uncorrectable claim that a screen ran
and cleared, onto an append-only ledger.

<a id="d-02"></a>
### D-02 — On-machine QC probe ingestion

**Surface today.** `ProbeIngestionSource` trait with `ProbeCursor` and
`RawProbeEvent` (`crates/aberp-qa/src/qc/probe.rs:52`). Three
implementations: `MockProbeSource` works (`:63`), `MtconnectProbeSource`
(`:95`) and `RenishawCentralSource` (`:128`) are `todo!`. The
failure path is already Live — `qc.probe_ingestion_failed` has a real
emitter at `crates/aberp-qa/src/qc/inspections.rs:338` — as is the whole
manual inspection workflow it would feed.

**Missing for Live.** A DMG MORI machine exposing probe results over
MTConnect, and a Renishaw Central deployment, to read and design against.
The result-to-verdict math, the tolerance tiers, and the auto-NCR are
already built for hand-entered values and would be reused.

**Blocked on.** Physical machine access.

**Size.** Medium per source: a poll loop and a parser against a real
sample. The cursor semantics are already defined by the trait.

**Shares work with [D-16](#d-16).** The MTConnect arm of this entry needs
gap-safe `/sample?from=<nextSequence>` polling and a `/probe` capture to
confirm the probe `Sensor` items are on the wire — both are D-16 expansion
slots. Doing them once in the shared MTConnect transport serves this entry
and the CNC/laser adapters together; doing them twice is the failure mode
to avoid.

<a id="d-05"></a>
### D-05 — DÁP eAzonosítás operator login

**Surface today.** `DapTransport` trait
(`crates/aberp-digital-id/src/dap_transport.rs:82`), a working
`MockDapTransport` (`:97`), and `OidcDapTransport` (`:160`) whose methods
are `todo!` pending relying-party registration. The mock is not
shelf-ware — `POST /api/dap/mock-login`
(route at `apps/aberp/src/serve.rs:4427`, `handle_dap_mock_login`) drives initiate +
complete server-side and returns the synthetic identity, so the flow shape
is exercised. Four `auth.dap_login_*` EventKinds are defined with
documented payloads and no firing site.

**Missing for Live.** szeusz.gov.hu relying-party credentials and spec
access; then the real OIDC exchange behind the trait, the operator-facing
login overlay, and the four firing sites.

**Blocked on.** Government RP registration.

**Size.** Medium — an OIDC client plus a UI overlay. The transport
boundary and the audit vocabulary already exist.

<a id="d-06"></a>
### D-06 — NETLOCK qualified timestamp authority

**Surface today.** `TimestampAuthority` trait
(`crates/audit-ledger/src/session/tsa.rs:63`), `MockTimestampAuthority`
(`:82`) running today, and `NetlockTsa` (`:172`) which compiles into the
binary with every method `todo!`. The surrounding machinery is Live behind
the per-tenant `dap_enabled` toggle (**default off**): ed25519 session
keys, service sessions opened at boot, crash-recovered sessions, a
heartbeat actor, and `audit.timestamp_anchor_taken` / `_delayed` emitters.

**Missing for Live.** NETLOCK account onboarding, then an RFC-3161 client
behind the trait and swapping the authority at the anchor site.

**Blocked on.** NETLOCK commercial onboarding.

**Size.** Small-to-medium — RFC-3161 is a well-specified exchange and the
delayed-anchor path (`TsaStatus`, token-NULL rows) is already modelled.

**Do not regress.** Until this lands, the chain signs with a mock. It is a
structural floor, not a qualified signature, and must not be described as
one.

<a id="d-07"></a>
### D-07 — CAC / PIV operator identity

**Surface today.** `DigitalIdProvider` trait
(`crates/aberp-digital-id/src/provider.rs:29`) with two selectable
backends chosen at boot by `build_digital_id_provider`
(`apps/aberp/src/serve.rs:4201`): `MockProvider`, and `UsDodCacProvider`
(`crates/aberp-digital-id/src/cac.rs:70`). The CAC backend is a
deliberate second stub that proves the trait actually abstracts — it holds
an `Option<CacSession>` so `current_operator()` and `sign()` are genuinely
fallible when no card is inserted, it is EDIPI-keyed, and it verifies by
cert-chain membership rather than MAC equality. It WARNs loudly on
construction.

**Missing for Live.** A real card reader (PKCS#11), real DoD PKI chain
validation, and real signing primitives. The mock "signs" with an HMAC
keyed on a hardcoded, publicly-known test key and must never back a
production identity.

**Blocked on.** A customer who actually requires CAC, plus reader
hardware and PKI trust material.

**Size.** Large — real smartcard integration and certificate-path
validation.

<a id="d-13"></a>
### D-13 — Trumpf laser: OPC UA backend

**Surface today.** `OpcUaLaserSource` implements the `TrumpfSource` trait
(`crates/aberp-mes/src/adapters/trumpf.rs:332`; trait at `:242`). It
returns a descriptive error rather than panicking, and `build_adapter`
never constructs it, so it cannot be reached from operator config. The
MTConnect-backed sibling is Live.

**Missing for Live.** An OPC-UA client dependency and an address-space
capture from the target machine — which items map to state and program
identity is machine-specific and cannot be guessed.

**Blocked on.** Access to a target machine's address space.

**Size.** Medium, and it adds a dependency — the current adapter set is
deliberately zero-new-deps.

<a id="d-14"></a>
### D-14 — Trumpf laser: Oseon / TruTops Fab backend

**Surface today.** `OseonLaserSource` implements `TrumpfSource`
(`crates/aberp-mes/src/adapters/trumpf.rs:356`), same non-panicking,
not-constructible posture as D-13.

**Missing for Live.** A licensed Oseon deployment to design against.
Availability, API surface, and auth model are all shop-specific and
possibly commercially gated.

**Blocked on.** A licensed deployment.

**Size.** Unknown — the API surface is not public. Worth noting this is
where *job-level* data lives (order linkage, nest identity, completion),
which is closer to what ABERP wants than the machine telemetry D-13 and
the Live MTConnect backend expose.

---

## Unblocked — our own work

These need no external party. They are firing sites and surfaces against
types that already exist.

<a id="d-15"></a>
### D-15 — `personnel.*` audit events and the e-signature ceremony

**Surface today.** Four EventKinds — `personnel.id_registered`,
`personnel.access_granted`, `personnel.access_denied`,
`personnel.signature_applied` — each with a documented payload schema,
round-trip storage-form validation, and exhaustive handling in the bundle
classifier and `aberp-verify`. The identity layer they would record is
D-07's `DigitalIdProvider`.

**Missing for Live.** The firing sites, and the e-signature ceremony UI
for `signature_applied`.

**Blocked on.** Nothing external — though a signature event recorded
against a stub identity provider is of limited value, so this is most
useful alongside D-07.

**Size.** Medium; the ceremony UI is the bulk of it.

<a id="d-08"></a>
### D-08 — CUI marking and access control

**Surface today.** `aberp-compliance::cui` implements `CuiMarking`
(`crates/aberp-compliance/src/cui/mod.rs:26`), `CuiCategory` (`:45`),
`DisseminationControl` (`:114`), plus `display_marking()` and
`to_banner_str()` — the 32 CFR Part 2002 / DoD CUI Registry vocabulary.
Two EventKinds are defined: `cui.marking_applied` and `cui.access_event`,
the latter documented to record *every* access decision, not only denials,
because improper CUI disclosure is itself reportable.

**Missing for Live.** Storage of a marking against an artifact (CAD blob,
document, product), the access-check call site, the two firing sites, and
the SPA banner that `to_banner_str()` exists to render.

**Blocked on.** Nothing external.

**Size.** Medium — it touches artifact storage and the SPA, and the
access check needs a defined enforcement point.

<a id="d-09"></a>
### D-09 — DFARS 252.204-7012 cyber-incident reporting

**Surface today.** `aberp-compliance::incident` implements
`IncidentSeverity` (`crates/aberp-compliance/src/incident/mod.rs:36`),
`DetectionSource`, and `dod_72h_report_due_at_ms()` (`:155`), which
computes the 72-hour reporting deadline. `incident.cyber_detected` is
defined with a full payload schema — CDI / OCS / CUI affected flags,
exfiltration suspicion, affected systems, detection source, mitigation
notes, and the computed deadline.

**Missing for Live.** An operator intake surface for declaring an
incident, and the firing site. Whether anything should *act* on the
deadline (an alert, a dashboard countdown) is an open design question, not
a decided one.

**Blocked on.** Nothing external.

**Size.** Small-to-medium — the types and the deadline math are done; this
is a form, a route, and an append.

<a id="d-10"></a>
### D-10 — DPAS priority rating audit event

**Surface today.** `DpasRating` (`crates/aberp-compliance/src/avl/mod.rs:90`)
and `DpasPriority` (`:28`) implement the FAR 11.6 / 15 CFR 700 rating
vocabulary. The storage half is **already Live**: partners carry a
`dpas_rating VARCHAR` column (`apps/aberp/src/partners.rs:717`) whose
values are validated through `DpasRating::parse`, so a free-text rating
cannot be stored. What is missing is only the audit trail.

**Missing for Live.** A firing site for `supplier.dpas_priority_set` at
the point the rating is assigned or changed.

**Blocked on.** Nothing external.

**Size.** Small — this is the closest entry to Live in the file. The
value is validated and stored today; only the append is absent.

<a id="d-11"></a>
### D-11 — Material reserve / release / consume, and certificate capture

**Surface today.** `inventory.material_committed` is Live, with a typed
payload and a real emitter (`apps/aberp/src/material_inventory.rs:701`).
Three sibling kinds — `inventory.material_reserved`, `_released`,
`_consumed` — plus `material.cert_attached` are defined and documented
with no firing site. `aberp-compliance::lot_heat` already validates lot
and heat ids, and heat-lot assignment, MTR upload, and the
work-order-start gate are all Live.

**Missing for Live.** The reserve/release/consume state transitions in
inventory (the events are the easy half; the state machine is the work),
and certificate capture for `cert_attached` — mill cert, CoA, heat
treatment — which needs blob storage and a document-type discriminator.

**Blocked on.** Nothing external.

**Size.** Medium — reserve/release/consume is a real inventory state
machine, not just three appends.

<a id="d-12"></a>
### D-12 — Out-of-band quote acceptance writeback

**Surface today.** `quote.pricing_operator_accepted` is defined with an
unusually complete payload schema: acceptance channel
(`phone` / `email` / `in_person` / `other`), operator note, the accepting
operator, the timestamp bound into the HMAC sent to the storefront, an
optional path to a scanned confirmation, the writeback outcome tag, a
`retry_available` flag, and an idempotency key. The in-app siblings
`quote.operator_accepted` and `quote.priced_writeback_outcome` are Live,
as is the storefront writeback transport.

**Missing for Live.** The operator UI for accepting on a customer's behalf
and the firing site. The payload design has already thought through
retries and idempotency; that is the hard part.

**Blocked on.** Nothing external.

**Size.** Small-to-medium — mostly UI, on top of an existing transport.

<a id="d-03"></a>
### D-03 — MIL-STD-130N IUID minting

**Surface today.** `aberp-compliance::uid` implements `IuidConstruct1`,
`IuidConstruct2`, the `Iuid` enum
(`crates/aberp-compliance/src/uid/mod.rs:265`), `validate_iac()` (`:117`),
and IRI rendering. Per-unit part marking is **Live** — it mints a
`dp-`-prefixed ULID with a DataMatrix payload, enforces a
shipment gate that refuses to ship a defense dispatch until every unit is
marked, and supports forward and reverse trace. It just is not a DoD IUID.

**Missing for Live.** An assigned Issuing Agency Code and enterprise
identifier for the shop, then swapping the minting site to construct a
real `Iuid` and render its IRI into the DataMatrix payload. Existing
`dp-` UIDs would need a migration decision.

**Blocked on.** An enterprise identifier assignment — arguably external,
but it is a registration the operator can pursue, not a vendor
dependency.

**Size.** Small in code; the migration question for already-marked units
is the real decision.

<a id="d-04"></a>
### D-04 — NIST SP 800-171 control tagging

**Surface today.** All 110 DFARS 252.204-7012 control identifiers exist as
string constants in `crates/aberp-compliance/src/nist_800_171/mod.rs`,
with an `ALL_CONTROLS: [&str; 110]` array. Nothing in the repo consumes
them — this is the thinnest surface in this file, and it is listed
because the control set is exactly what an assessor asks to see mapped.

**Missing for Live.** A decision on where a control tag lives (an audit
payload field? a separate mapping table?), then tagging the events that
satisfy each control, and a report that renders the coverage.

**Blocked on.** Nothing external — but it wants a design pass first.

**Size.** Medium, and mostly analysis rather than code: deciding which
existing events evidence which controls is the work.

<a id="d-18"></a>
### D-18 — Normalise solid CAD interchange formats to a canonical STEP, in the pipeline

**Not a customer-facing feature.** No convert hint, no converter UI, and the
customer never receives a STEP back. The conversion is invisible plumbing;
the deliverable to them stays the quote today and the toolpath later. What
changes is only *which uploads reach the extractor instead of dead-ending*.

**Surface today.** The kernel that would do the work is already provisioned
and already load-bearing. `run/provision_pipeline_venv.sh:75` installs the
extractor package with the `[step]` extra — the ~63 MB `cadquery-ocp` wheel,
i.e. OCCT — and the verify gate at `:78` refuses to pass unless
`import aberp_cad_extract, OCP` succeeds; the two upgrade scripts carry the
same contract (`run/upgrade_defense.sh:356`,
`run/upgrade_portable.sh:341`), and `run/tests/provision_pipeline_venv_step_test.sh`
regression-tests it. The STEP path off that kernel is Live end to end:
`_load_step_shape` drives `STEPControl_Reader`
(`python/aberp-cad-extract/aberp_cad_extract/extractors/step.py:82`, reader
constructed at `:105`) into `extract_step` (`:128`), the Rust side runs it
as an isolated subprocess so an OCCT crash cannot take the daemon down
(`crates/aberp-cad-extract-wrapper/src/lib.rs:41`, request contract at
`:248`), and the pricing daemon consumes it as stage 2 of four —
storefront poll, extract, price, render
(`apps/aberp/src/quote_pricing_pipeline.rs:15`, extractor constructed at
`:1078`).

The gap is documented, not hypothetical, and it is currently a
**dead-end**. Three allowlists disagree, widest first:

1. The storefront accepts far more than three formats — `ALLOWED_EXT` in
   **`ABERP-site/src/routes/api/quote/+server.ts:17–32`** *(cross-repo;
   ABERP-site is not checked out here)*, with per-format magic-byte
   validation in **`ABERP-site/src/lib/server/cad-validate.ts:1–313`**
   *(cross-repo)*. Both are recorded from this side at
   `docs/findings/s346-audit-quote-workflow.md:85`. Beyond `.stl` /
   `.step` / `.stp` it admits `.iges`, `.igs`, `.x_t`, `.x_b`, `.sldprt`,
   `.ipt`, `.f3d`, `.dxf`, `.dwg`, `.3mf`, `.obj`
   (`docs/reviews/S296-adversarial-s286-s295-overnight.md:50`).
2. The pricing daemon's file picker takes the first `.stl` / `.step` /
   `.stp` file in the quote and nothing else
   (`apps/aberp/src/quote_pricing_pipeline.rs:626`) — an IGES-only quote
   never even gets picked; it falls to `enqueue_failed_no_cad`.
3. The Python dispatcher `_route` routes the same three
   (`python/aberp-cad-extract/aberp_cad_extract/cli.py:29`, error at
   `:36`).

So IGES is *already* accepted at the front door and already fails: PR-274 /
S297 F1 added a classifier rule so `Unsupported file extension '.iges'`
lands as `FailureKind::Permanent` rather than `Unknown`
(`apps/aberp/src/quote_pricing_pipeline.rs:2862` and the rule at `:2912`,
pinned by `pr274_classify_unsupported_extension_is_permanent` at `:5197`).
That rule is correct as a *fail-fast*, and it is exactly what this entry
converts into a *transcode* for the formats the kernel can actually read.

**Missing for Live.** An offline transcode step, then reuse — do not fork
the STEP path:

- **Broaden accept to the solid interchange formats OCCT reads, and only
  those.** Confirmed against the wheel this repo installs — `cadquery-ocp`
  **7.9.3.1.1** (OCCT 7.9.3) in `python/aberp-cad-extract/.venv`, by
  importing the reader classes rather than reading vendor docs:
  `STEPControl_Reader` / `STEPCAFControl_Reader`, `IGESControl_Reader` /
  `IGESCAFControl_Reader`, and `BRepTools::Read` all import; `RWObj` is
  **absent from the build entirely**, and there is no `Parasolid`, `ACIS`,
  `JT`, or `DE_Wrapper` module of any kind. The exact set is therefore
  **STEP `.step`/`.stp` (already accepted) + IGES `.iges`/`.igs` + BREP
  `.brep`** — three formats, not an open-ended rule. "Other OCCT-readable
  solid formats" resolves to nothing further in this build; a fourth format
  means a different OCCT distribution and its own decision. Note also that
  `.x_t`/`.x_b`/`.sldprt`/`.ipt`/`.f3d` are accepted at the storefront
  today and are **unreadable by this kernel** — the storefront allowlist
  promises what OCCT cannot keep, and narrowing it belongs with this work.
- **Mesh formats stay rejected. This entry does not reverse the STL drop.**
  A mesh cannot be turned back into a solid — there are no faces, no axes,
  no cylinders to mine, so no located holes and no honest quote. `.stl`,
  `.3mf`, and `.obj` stay out, as do the 2D `.dxf`/`.dwg`. `StlAPI_Reader`
  and `RWGltf_CafReader` do exist in the wheel and must not be mistaken for
  a way in: they yield a triangulation, not a B-Rep body. STL is dropped by
  ADR-0112 Slice A (`b141436` on branch
  `feat/adr-0112-step-only-located-holes`, unmerged as of this entry) —
  anyone reading D-18 as licence to re-add mesh has read it backwards.
- **The transcode itself: server-side, offline, no SaaS.** OCCT/OCP reads
  the uploaded solid and writes **one canonical STEP** into the existing
  artifact directory next to the upload; from there the pipeline proceeds
  unchanged. **FreeCAD is explicitly not needed** — it would only wrap the
  same kernel we already ship and pull in a GUI-scale dependency for
  nothing. It runs inside the existing subprocess isolation, so a
  transcode-side OCCT crash has the same non-fatal shape as an extract-side
  one. IGES is the one with real work in it: it is surface-based, so a
  read may land as a shell rather than a solid and needs an explicit
  sew/solid-check with a loud failure when it will not close — an
  unclosed shell must fail, never quietly become a quote.
- **Then reuse the STEP path verbatim.** Canonical STEP → the existing
  extractor → FeatureGraph
  (`python/aberp-cad-extract/aberp_cad_extract/feature_graph.py:57`,
  `HOLE` at `:35`;
  located holes land with ADR-0112 Slice B, `f7f234a` on the branch above)
  → quote → toolpath as the downstream possibility. One extractor, one
  schema, one quote path.
- **Two repos, two surfaces — but three allowlists.** The **front door** is
  the ABERP-site storefront upload/content-sniff validation (the two
  cross-repo anchors above): broaden the accepted-solid allowlist and add
  the matching magic-byte arms there, and drop the formats OCCT cannot
  read. The **transcode, extract, and quote** live here in the Editions
  pipeline. Do not miss that the Editions side has *two* gates of its own —
  the daemon's CAD picker (`quote_pricing_pipeline.rs:626`) and the Python
  `_route` (`cli.py:29`) — and both must learn the same widened set, or the
  front door widens onto the same dead end it does today.

**Blocked on.** Nothing external. The kernel is already installed,
provisioned, and gate-tested; no account, licence, machine, or vendor SDK
is involved. It does need the ABERP-site change to land in step with this
one — that is coordination across two repos we own, not an external
dependency.

**Size.** Medium. Small per format for STEP and BREP; IGES carries most of
the risk (sew-to-solid, and the failure mode when it will not close).
Widening three allowlists in lockstep is the coordination cost, and the
quote-side maths does not move at all.

**Do not regress.** The `FailureKind::Permanent` classification for an
unsupported extension stays for everything still out of scope — mesh, 2D,
and the native/Parasolid formats. Softening it to `Unknown` or `Transient`
would put the operator back on the futile-Retry loop PR-274 closed. And
the canonical STEP is an internal artifact: it must never be offered back
to the customer as a download, or the invisible plumbing becomes a
product promise nobody agreed to ship.

---

## Expansion slots on a Live capability

<a id="d-16"></a>
### D-16 — MTConnect: develop the base transport into a full socket

**Status: the base transport is Live and stays Live.** This entry does not
downgrade it. MTConnect was picked over N proprietary vendor SDKs precisely
because one open standard covers the whole controller population — DMG
MORI, Mazak, Haas, Okuma, Fanuc, Heidenhain — and that bet has already paid
out inside the repo: the CNC adapter
(`crates/aberp-mes/src/adapters/mtconnect.rs`) and the Trumpf laser both
run on the same code. `trumpf.rs:106` imports `poll_once` and
`map_execution_to_state` from the MTConnect module and reuses
`parse_mtconnect_current` verbatim. Two machine families, one transport.

This entry itemises the capacity that seam has and we are not yet drawing.
Each slot below has real code behind it today.

#### Slot 1 — five parsed data items with no consumer

`parse_mtconnect_current` extracts six leaf data items into
`MtconnectSnapshot` (`mtconnect.rs:352`): `Execution`, `Availability`,
`Program`, `ControllerMode`, `PartCount`, and `RotaryVelocity` /
`SpindleSpeed` (parse arms at `mtconnect.rs:533`–`:542`). But `apply_poll_outcome`
reads **only** `snapshot.execution` — the other five are parsed off the
wire every five seconds and dropped.

The seam is proven, not theoretical: `MtconnectLaserSource` already
consumes a second field (`snapshot.program`, `trumpf.rs:293`) through the
same parser. Nothing structural blocks the rest.

*Missing:* consumers, and in most cases a new `CanonicalEvent` variant to
carry the value. Today's five variants — `PartMoved`,
`MachineStateChanged`, `QualityResultReceived`, `ScanReceived`,
`WorkOrderStateChanged` (`crates/aberp-mes/src/events.rs`) — have no shape
for a part count or a spindle speed. `PartCount` is the obvious first one:
it is a production-count signal the shop actually wants, it is already
parsed and integer-validated, and it needs no hardware we do not have.

*Blocked on:* nothing external.

*Size:* small per data item, once the event shape is decided. The event
shape is the design call, not the parsing.

#### Slot 2 — `/sample` subscription with sequence gap detection

v1 re-pulls `/current` every 5s, which works against any Agent but misses
state pulses shorter than the poll interval. The module docs carry this as
a named PR-240 follow-up (`mtconnect.rs:75`): long-poll or chunked
`/sample?from=X&count=Y` with sequence-based gap detection.

*Missing:* the `/sample` client and cursor handling. **This is the same
work [D-02](#d-02)'s MTConnect probe arm needs** — its `todo!` names
`poll /sample?from=<nextSequence>` explicitly. Build it once here.

*Blocked on:* nothing external to write it; validating it against a real
Agent's sequence numbering wants machine access.

*Size:* medium, and it is the highest-leverage slot — it unblocks D-02 and
raises fidelity for every adapter on the seam.

#### Slot 3 — Condition / fault stream parsing

Only `Execution` currently triggers a state change. MTConnect Conditions
and Warnings carry their own vocabulary, and `MachineState::Fault` already
exists as a mapping target (named as future work at `mtconnect.rs:79`).

*Missing:* the Condition parse arms and a `Condition.fault` →
`MachineState::Fault` mapping. Today a machine in a fault condition that
still reports `Execution: ACTIVE` reads as Running.

*Blocked on:* nothing external, though real fault documents are worth
capturing before fixing the mapping.

*Size:* small-to-medium.

#### Slot 4 — `/probe` device-catalog introspection

`/probe` returns the device's data-item catalog. Named as future work at
`mtconnect.rs:83`.

*Missing:* the call plus a validation pass. Two payoffs: it tells an
operator at adapter-registration time whether their controller actually
exposes the items we consume instead of failing silently into `Unknown`,
and it is **literally step one of [D-02](#d-02)'s MTConnect arm** — the
`MtconnectProbeSource` doc calls for a `/probe` + `/current` capture from
the target machine to confirm probe `Sensor` items are on the wire.

*Blocked on:* nothing external to write; the confirmation step needs a
machine.

*Size:* small.

#### Slot 5 — `/assets`: cutting tool and workpiece tracking

MTConnect asset tracking (`cuttingTool`, `workpiece`), named at
`mtconnect.rs:86`.

*Missing:* everything but the transport. Worth noting the adjacency: tool
life and workpiece identity are the natural join to the Live part-UID and
heat-lot traceability chains, which is what would make this more than
telemetry.

*Blocked on:* nothing external.

*Size:* medium, and it wants a design pass on how assets relate to the
existing traceability model before any code.

#### Slot 6 — SHDR-side Adapter SDK

Named at `mtconnect.rs:87`. We assume the Agent is already up — built-in on
modern controllers, sidecar on legacy ones — and ship no Adapter-side code.

*Missing:* the SHDR publishing side, for shops with a legacy controller and
no Agent in front of it.

*Blocked on:* a shop that actually has this problem. Do not build it
speculatively; the population it serves is exactly the one we cannot
currently reach, so it is unmeasured.

*Size:* large, and the widest scope in this entry.

#### Do not regress

The DoS bounds are enforced in code, not operator config
(`max_response_bytes` 4 MiB, `request_timeout` 4s set deliberately below
the 5s `poll_interval` so stalled requests cannot pile up across ticks).
Any new endpoint added above inherits that posture — a `/sample` long-poll
in particular needs its own explicit bound, since it is the one shape here
that is *supposed* to hold a connection open.
