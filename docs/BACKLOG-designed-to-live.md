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

<a id="d-19"></a>
### D-19 — Located-holes geometry hardening — GATES slice C (drilling cycle-time pricing)

**HARD GATE.** Slice C — the drilling cycle-time pricing that would price
off `located_holes` — **must not ship until every defect below is closed**.
This is a *pre-emptive* gate, not an incident report. As of ADR-0112 slice
B, `located_holes` is **computed but unconsumed**: no quote path and no
toolpath reads it, so there is **zero live quoting impact today**. Every
defect listed is **pre-existing on the feature** — each was present in
every adversarial round, none is a regression introduced by the round-7
landing.

**Surface today.** The STEP hole miner in
`python/aberp-cad-extract/aberp_cad_extract/holes.py` emits `located_holes`
into FeatureGraph v6. The named parts below are where each defect lives:
`_skin_over_axis`, `_root_for_end`, `_walk_caps`,
`DEGENERATE_ISOLINE_RATIO`, `SURFACE_CONFUSION_MM`.

**Missing for Live.** Eight items, in priority order — seven defects and
one undecided convention. All are mutation-verified: items 1-5 from
adversarial pass 8, items 6-8 from the D-19 round-2 pass. **Three are
CLOSED**; five remain, and the gate stands until all of them are. Note
that the defects do **not** all fail in the same direction: 1, 2, 3, 5 and
7 under-quote, 4 and 6 over-quote, and 6 does so by three orders of
magnitude. Item 8 is a decision, not a direction.

1. ~~**N4 — undercut spherical cavity**~~ **— CLOSED.** (Ball-end seat;
   nose radius exceeds bore radius by `e >= 4e-7`.) Read **up to 87%
   short** *and* reported a blind pocket as **through**, because the walk
   ended at the sphere's top pole in mid-void. Fixed by promoting round
   6's own reason — a crossing inside the bore's own hollow bounds
   nothing — from a tie-break trigger to the rule: `_root_for_end` now
   discards any crossing inward of the mouth
   (`_mouth_inward_bound`), of which the ball-nose tangency is the
   zero-undercut member. Four committed fixtures plus a 90-member
   size x undercut sweep and a 144-step depth walk.
2. ~~**N3 — dropped hole**~~ **— CLOSED by the same fix.** A Ø16 bore
   with undercut `e` in `{4e-7, 1e-6}` returned **zero holes**: the pole
   in the void landed at z=20.000001 on a 20 mm plate, so the span came
   out negative and the bore was dropped. Same root cause as N4, same
   one-line rule; `undercut_ball_seat_at_the_confusion_edge.step` pins
   it, and `undercut_ball_seat_below_the_confusion.step` pins the
   sub-confusion neighbour that was already right.
3. **Zero-caps boss-overhang band.** `_skin_over_axis` narrows to zero
   caps, so the caller falls back to the innermost crossing of the *whole*
   rim and under-reports depth by **0.05–4.36 mm (<= 17.9%)**. This is a
   **contiguous band** — roughly **42%** of its local region, cone heights
   ~14–20 crossed with bore offsets ~37.0–38.5 — and it is a **barrier-set**
   defect, insensitive to ray count, *not* a sampling artifact. (The
   round-7 commit first recorded this as "4 of 92 configs"; that count was
   a grid-spacing artifact of the sweep and is corrected in that commit
   message.)
4. **N2 — 118° drill-point apex admitted as a cap.** `DEGENERATE_ISOLINE_RATIO`
   is `1e-9`, about **89× tighter** than `GeomAPI_IntCS`'s own noise floor,
   so the apex passes as a cap. **Over-quotes** by the point length
   (~0.3·D). Topology-dependent: 7/180 fused, ~49/100 two-step. Candidate
   fix is raising the ratio to ~`1e-6`, which needs its own regression
   sweep.
5. **N1 — breakout hole.** When the nose/point breaks the far face, the
   hole reads **blind** with a depth *exceeding the plate*, because the
   measurement runs to a pole in the air below the part.
6. ~~**R2-A — convex floor / cap off the part**~~ **— CLOSED.** The
   *mirror* of N4, found by the round-2 adversarial pass, and the only
   item here that **over-quotes**. Two compounding faults in
   `_root_for_end`: the void bound from item 1 discarded the **crown of a
   doubly-curved convex floor** — which *is* the floor, since the metal
   starts there — and the nearest pick then took the carrier's *other*
   crossing, which nothing constrained. A Ø12 pocket with a crown
   standing 1.2e-3 mm proud read **30012 mm deep and THROUGH in a 20 mm
   plate**, flipping at exactly the bound's slack; a Ø8 bore breaking out
   through a sphere read **24 in a 20**; a Ø4 bore with a spherical mouth
   undercut put its far end 2.556 mm **above** the plate's top face and
   read UNKNOWN. Fixed by validating against the **solid**: where the
   bound discards, a crossing the axis really leaves the metal at takes
   the end (rescuing the crown), and a survivor outside the part's own
   extent is refused outright rather than inherited. Position and normal
   cannot separate these — a convex crown and an undercut pole sit in the
   same place and both read `(0,0,+1)`. Four committed fixtures, a
   13-crown sweep across the slack, a 6-cutter sweep, and direct
   arithmetic pins on all three selection rules.
7. **R2-B — toroidal undercut (O-ring / snap-ring gland).** A gland at the
   bore's end reads **short and THROUGH**: a Ø8 bore with a R4 x 1.5
   gland at z=12 mines **6.5 THROUGH** where the axis has metal below
   z=12 (so it is BLIND) and the gland's own floor is at z=10.5 — **8.0
   or 9.5** depending on whether depth is measured on the axis or to the
   deepest point of the feature, which is itself undecided. **Pre-existing
   and unchanged by the round-2 fix**, verified by running both rules over
   the same part: a **ring torus never crosses its own axis**, so that cap
   face offers *no* crossing at all and root selection is never asked. The
   end falls back to the bore's parametric bound and the touching vote
   calls it open. Fixing it means giving `_cap_axis_intersections` or
   `_EndEvidence.resolve` something to say when a cap face contributes no
   axis crossing — a different mechanism from items 1 and 6, which is why
   it was left rather than half-fixed.
8. **R2-C — depth convention at a relieved mouth.** Not a defect so much
   as an **undecided convention**, recorded because the round-2 pass
   surfaced a disagreement about it. The miner measures a hole to the top
   of its own **cylindrical wall**, so a countersink, a chamfer or a
   spherical dish at the mouth is *not* part of the hole:
   `countersunk_blind_bore` is pinned at **11.0, not 14.0**,
   `chamfered_mouth_bore` at **18.5, not 20.0**, and
   `spherical_mouth_undercut_bore` at **9.0404, not 10.632**. Measuring
   from the part's face instead is defensible — it is what a drawing
   dimension usually means — but it is a **corpus-wide convention change**
   touching at least those three committed fixtures and their goldens, so
   it needs its own decision and its own pass. Slice C must not price off
   `located_holes` until it is settled, because the two readings differ by
   the full depth of the relief.

**Blocked on.** Nothing external. This is purely our own work in one file.
The adversarial-8 repros lived in the branch's `scratchpad/` and were not
committed; they are regenerable from the descriptions above and from
`tools/generate_step_fixtures.py`. The N3/N4 repros no longer need to be:
they are committed fixtures and in-memory sweeps in
`aberp_cad_extract/tests/test_holes.py`.

**Size.** Medium-to-large. N3 and N4 shared the undercut root cause and
the half-order robustness window documented on the round-7 commit
(`SURFACE_CONFUSION_MM = 1e-7`: correct at `e <= 3x`, fails at `4x`), and
they were indeed one fix — the boundary is gone rather than moved, so the
answer no longer depends on whether OCCT can tell the seat from a tangent
ball nose. Item 6 is that same fix's mirror and closed the same way, by
replacing a presumption about position with a question put to the solid.
Items 3, 4, 5 and 7 are independent; item 4 is a constant change whose
blast radius is the whole fixture corpus, and item 8 is a decision rather
than a fix.

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
