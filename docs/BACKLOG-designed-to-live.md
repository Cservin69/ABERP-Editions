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

**Missing for Live.** The five open defects, in priority order. All are
mutation-verified from adversarial pass 8, and all but N2 **under-quote**
(they read the part as cheaper than it is):

1. **N4 — undercut spherical cavity** (ball-end seat; nose radius exceeds
   bore radius by `e >= 4e-7`). Reads **up to 87% short** *and* reports a
   blind pocket as **through**, because the walk ends at the sphere's top
   pole in mid-void. The trigger is an **ordinary** feature — ball-end
   undercuts and spherical seats are routine — which combined with the
   87% makes this the **worst offender**.
2. **N3 — dropped hole.** A Ø16 bore with undercut `e` in `{4e-7, 1e-6}`
   returns **zero holes**: a silent under-count, not a wrong number. The
   trigger is narrow, but the outcome is the worst of the set — the
   feature vanishes with no signal.
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

**Blocked on.** Nothing external. This is purely our own work in one file.
The adversarial-8 repros lived in the branch's `scratchpad/` and were not
committed; they are regenerable from the descriptions above and from
`tools/generate_step_fixtures.py`.

**Size.** Medium-to-large. N3 and N4 share the undercut root cause and the
half-order robustness window documented on the round-7 commit
(`SURFACE_CONFUSION_MM = 1e-7`: correct at `e <= 3x`, fails at `4x`), so
they are likely one fix. Items 3, 4 and 5 are independent, and item 4 in
particular is a constant change whose blast radius is the whole fixture
corpus.

> ### ✅ ALL FIVE CLOSED — 2026-09-01
>
> On branch `feat/d-19-located-holes-geometry`, in `holes.py` alone. Every
> defect was reproduced first, fixed as geometry rather than as a
> tolerance, and pinned by a FAMILY rather than by the one part it was
> found on. The 43 committed fixtures keep their answers bit-identically;
> three new ones are committed, one per root cause.
>
> The five turned out to be **two** root causes, not four:
>
> - **N4 + N3 — the wrong root of a coaxial cap.** Round 7 already held
>   that a root inward of the mouth lies in the void the bore hollowed out
>   — and applied it only to a TIE. An *undercut* seat (nose wider than
>   bore) has its two roots at unequal distances, so the tie never fired
>   and nearest-the-mouth took the sphere's phantom near pole.
>
>   **Which side of the mouth a root falls on is NOT the discriminator**,
>   and two cuts of this were wrong before the third was right — both
>   caught by the adversarial pass, not by the suite. Bounding by the
>   mouth's axial *mean* took a conical boss's MIRROR cone 7 mm above the
>   apex (a mouth cut across a slope straddles its own average). Bounding
>   by the mouth's full axial *reach* fixed that and was still wrong in
>   general, because a **concave** cap's true root is genuinely inward: a
>   bore leaving through a spherical dimple ends at the dimple's own floor
>   BELOW where its wall starts, and an outward-only rule marched it to
>   that sphere's far pole 9.6 mm above the part.
>
>   The discriminator is whether the root is on the part's **real
>   boundary** — the trimmed cap face — and it is a PREFERENCE, never a
>   bound (`_root_is_on_the_face`). It is also gated hard on
>   **coaxiality** (`_cap_is_coaxial`), because **round 4 had already
>   measured that an unrestricted on-face gate reopens its blocker 2** and
>   pinned that as `test_r4_an_on_face_trim_test_would_have_re_broken_the_domes`.
>   Round 4 is right: a Ø4 bore out through a torus wall meets its cap's
>   carrier at x = −20, −4, 4, 20; its own end at x=4 sits in the hole it
>   cut and is *not* on the face, while x=−4 on the far side of the donut
>   *is*, and preferring on-face there measures 24.0 against a true 16.0.
>   A **coaxial** cap is the one shape where distance says nothing at all
>   — the roots are that surface's own poles — so the preference is asked
>   there and nowhere else. Verified across 4 cutters x 8 undercuts
>   spanning six decades, and 12 nose depths for the drop.
>
> **Two siblings FLAGGED, not fixed** (both pre-existing, both reading the
> same at `origin/main`, both UNDER-reporting, neither among D-19's five;
> pinned at their current values by
> `test_r8_two_siblings_of_the_undercut_family_are_flagged_not_fixed` so a
> later round must change a test on purpose):
>
> - a **spherical relief wider than the bore at its MOUTH** shortens the
>   bore by about its own radius (a Ø8 through bore in a 20 mm plate reads
>   16.0) — neither pole is on the face, so nearest applies and takes the
>   relief's pole;
> - an **undercut seat deep enough to BREAK the far face** puts its true
>   pole off the part, so no root is on any face and the phantom near pole
>   wins again;
> - and, on the boss side, the family **outside** item 3's named region.
>   The named band (cone heights 14–20 × offsets 37.0–38.5) is closed to
>   the bit; widen the sweep and a randomised 108-part family goes from
>   **18 wrong to 10**, every remaining one already wrong at
>   `origin/main`. The residual is a different mechanism —
>   `_barrier_track` reconstructs a cut part edge by marching its
>   *untrimmed* curve past the vertex, which assumes the BORE is what cut
>   it; where a fused boss also swallowed part of that edge the curve is
>   laid back down across ground it never covered, and a bore whose whole
>   share of the cone's mouth lies beyond the plate's edge then has no
>   unobstructed ray at all. Same shape of error as the seam, one level
>   down: an edge asserted where the topology has none.
> - **Item 3 + N2 + N1 — a parametric artifact read as geometry.** The
>   *seam* marched as a barrier (item 3: 69 of 81 configurations short,
>   0.04–3.98 mm, a contiguous band; closed by `_same_carrier`), and the
>   *apex* admitted as a cap (N2 over-quoting by the point length; N1
>   measuring 22.0 mm into a 20 mm plate, exiting in mid-air below the
>   part; closed by `_collapsed_isoline_radius`).
>
> **Item 4 was NOT closed by the candidate fix the entry proposed.**
> Raising `DEGENERATE_ISOLINE_RATIO` to ~`1e-6` would have moved a
> tolerance; the defect is that a *dimensionless ratio against 1e-9* was
> being asked to locate a degeneracy that `GeomAPI_IntCS` only resolves to
> about the kernel's confusion (measured: the apex arrives at `v = 7.9e-08`
> with a ratio of `6.8e-08`, 89x the admitted figure). The ratio arm keeps
> the anisotropy job it was fit for; a second, MODEL-SPACE arm at
> `1e3 x SURFACE_CONFUSION_MM` does the resolution job it was not. The two
> populations were measured over the whole corpus plus the sweeps —
> collapsed roots at ≤`1.09e-07` mm/rad, the smallest regular one at
> `2.5` mm/rad — so the floor sits three orders above the noise and four
> below the nearest real surface, and the answers are flat across every
> decade between.
>
> **Flagged — one decision, not a math fix.** N1's *end condition* is a
> design call: when the point breaks the far face the plate is holed but
> the full-diameter bore is blind, and nothing in the geometry settles
> which a drilling cycle prices from. Taken conservatively at BLIND on the
> full-diameter depth (a blind cycle pecks and dwells, so it is the dearer
> and therefore the visible direction), pinned as its own named test so
> changing it is deliberate. The *depth* half of N1 was a pure math fix.
>
> **Three round-7 guards were re-pointed, none dropped.** The seam fix took
> the pinch off `bore_beside_a_conical_boss`, so the guards for the ray
> refinement and the barrier-chord floor no longer showed anything there.
> Both were re-measured over a 203-part randomised family — the refinement
> still changes an answer on 1, the floor on 13 — and moved onto parts that
> still exercise them. The band's tie-break guard was superseded outright
> and replaced with a revert-proof for the rule that now does the job.
>
> Mining cost is unchanged: 2.33 ms/bore at 150 holes and 8.96 at 600,
> against 2.33 and 8.76 on `origin/main`. The one measurable regression —
> the new same-carrier test asked before the cheap filters, which doubled
> the bill on a 600-hole plate — was found by measurement and reordered.
>
> **Slice C is now UNBLOCKED and is NOT built here.** Drilling cycle-time
> pricing needs a cycle-time model (SFM/feed per material and diameter,
> peck logic on depth:diameter, tool change and approach), a *researched*
> seed table — ADR-0097 shipped fully wired with an all-zero seed and
> priced 0.00 EUR forever, which is the lesson — a catalogue/calibration
> path, reasoning-log lines, and its own adversarial round. That is a
> separate meaningful chunk, not a wiring exercise: `located_holes`
> remains computed-and-unconsumed at `engine.rs`'s
> `feature_machining_minutes`, exactly as ADR-0112 Part B left it.

<a id="d-20"></a>
### D-20 — Pricing-queue hardening: four follow-ups the D-PRICEQ fix did not close

**Surface today.** The head-of-line fix on the pricing daemon
(`apps/aberp/src/quote_pricing_pipeline.rs`) closed the prod wedge: the
extract path fails loud instead of returning `Err` forever, `poll_once`
skips an erroring job instead of abandoning the cycle, a stale-job reaper
backstops rows nothing can move, every non-idle cycle writes a
`quote.pricing_cycle_outcome` audit row, and the operator has
a Retry route. Round 3 additionally routed every remaining bare-`?` error
exit on the advance path through the audited failure path, so no advance
error can leave a row non-terminal and therefore beyond `retry_job`'s reach.

**How the reaper is gated (round 4).** A row is condemned only when it has
been unmoved since *before both* of two bounds, whichever is **earlier**:

* a **full window of cycles that really ran** — the daemon keeps the
  timestamps of its last `STALE_JOB_REAP_AFTER / poll_interval` completed
  cycles (30 at the 60s cadence), and reaps nothing until that window is
  full; and
* the ordinary **wall-clock floor**, `now - STALE_JOB_REAP_AFTER`.

Both bounds are load-bearing and in opposite directions. The window is what
stops ordinary app downtime, a sleeping laptop, or a storefront outage from
condemning a mid-flight row: a cycle that bails before the advance loop
records no mark, so an outage drains the window instead of ageing the queue,
and the window simply stops advancing while the machine is asleep. The
wall-clock floor is what stops the window opening *early*: `poll_loop` backs
off 5s then 15s after an errored cycle, so a run of errored-but-completed
cycles fills a thirty-cycle window in about two and a half minutes.

Note what this is **not**: a monotonic count of cycles since process start.
That was the round-3 gate, and because it never resets it stood permanently
open roughly half an hour into any uptime — at which point the reaper was
pure wall clock again and both freezes above condemned a row on the first
cycle back. A count cannot express *recently*; a bounded window of recent
marks can, and drains itself by not being refilled.

The four items below were found by the adversarial passes on that fix, are
**not** closed by it, and are recorded here deliberately rather than folded
into it.

**Missing for Live.**

1. **A1 — the reaper conflates "stuck" with "starved".** The reaper's only
   signal is `updated_at`, so it cannot tell a row *nothing can move* from a
   row *the advance loop never reached*. `MAX_JOBS_PER_CYCLE` is 5, and the
   advance loop spends one iteration per erroring job (they go on the
   per-cycle skip list). With five or more erroring jobs ahead of it in
   strict FIFO — reachable in practice because the operator Retry route
   preserves `fetched_at`, so a retried old row jumps back to the head — a
   healthy mid-flight row behind them never gets an attempt, and being the
   **oldest** `updated_at` it is the row the reaper terminalises **first**.
   The live-cycle gate does **not** cover this: completed cycles accrue
   perfectly well while the row is being starved — indeed they accrue
   *because* cycles are running. Round 4's window gate does not cover it
   either, for the same reason — the marks record that a cycle RAN, not that
   it reached any particular row.

   *A second shape of the same defect, found in round 4.* If
   `next_actionable_job_excluding` itself returns `Err` — a DB-level lookup
   fault, not one job's problem — `poll_once` breaks out of the advance loop
   but still falls through to the counter bump and the mark. Thirty such
   cycles fill the window with cycles that offered NO row an attempt, and the
   next reap condemns whatever is mid-flight. Deliberately NOT closed inside
   round 4: the round's mechanism was reviewed as prototyped, and skipping
   the mark there is a behaviour change of its own (it would disable the
   reaper whenever the lookup is unwell — arguably right, since a daemon that
   cannot look a row up cannot clear a wedge either, but that is a call to
   make deliberately). Narrow in practice: it needs the lookup SELECT to fail
   persistently while the storefront list, the reaper's own write lock and
   `started_non_terminal_jobs` all keep working. Same severity as the
   starvation shape above — a wrongly-Failed quote the operator can Retry —
   and the same honest fix closes both: a persisted `last_attempt_at` on the
   row, so "un-reached" is legible in the row itself rather than inferred
   from what the daemon was doing.

   *Why it is not fixed here.* Every clean version of the fix changes
   design already reviewed and cleared. (a) Gating the reap on the set of
   jobs the advance loop actually attempted this cycle requires moving the
   reaper **after** the advance loop; it deliberately runs first, so the
   same cycle's FIFO lookup already steps past a wedge, and moving it
   changes what `d_priceq_erroring_head_does_not_freeze_the_jobs_behind_it`
   asserts (`errored == 1`, `reaped == 1`) because a stale row would now
   get one live attempt first and may terminalise through the ordinary path
   under a different stage. (b) Keeping the order and carrying the
   *previous* cycle's attempted set means new cross-cycle daemon state with
   its own eviction and restart semantics. The honest fix is to give the
   reaper a **second signal** rather than a heuristic — persist a
   `last_attempt_at` on the job row, bumped whenever the advance loop picks
   the row up, so "un-reached" is distinguishable from "stuck" in the row
   itself and the reaper keeps running where it runs today. That is a
   schema change plus a migration, which is its own piece of work.

   *Severity.* No customer-visible loss beyond a wrongly-Failed quote the
   operator can Retry, and it needs a five-deep erroring backlog to fire.

2. **A2 — the enqueue loop re-downloads and re-encrypts every still-
   `received` quote, every cycle, before the idempotency check.** Cycle
   wall-clock therefore grows with the size of the un-enqueued storefront
   backlog rather than with new work. Round 4 took the reaper's timing
   sensitivity off this: a slow cycle pushes the window's oldest mark
   further into the past, which only ever *delays* a reap, and the
   wall-clock floor bounds the other direction. It remains a real latency
   defect. The fix is to hoist the idempotency check above the download,
   which needs care that the check stays correct for a row whose artifact
   write was interrupted.

3. **A3 — the Retry route is not edition-gated.** The route and its
   response codes are compiled into Portable as well as Defense. Harmless
   today — Portable never runs the pricing daemon, so `quote_pricing_jobs`
   is always empty there and every retry answers 404 — but it is an
   edition-surface drift, and the sibling routes around it are gated.

4. **A4 — bounded auto-retry-with-backoff for `Transient` pipeline
   failures.** `classify_failure` labels every terminal failure
   `Transient` / `Permanent` / `Unknown`, and round 4 widened `Transient` to
   cover the daemon's own transient-local sites (a full disk under the PDF
   write or the decrypted-CAD temp, a transient audit-tx fault on a CAD-blob
   read append). **Nothing consumes the label.**
   `next_actionable_job` excludes `Failed` rows outright, so a `Transient`
   row waits for an operator Retry click exactly as a `Permanent` one does;
   `UNKNOWN_AUTO_RETRY_CAP` is a constant with no reader on any scheduling
   path. The verdict's live consumers are the operator panel badge and the
   `QuotePricingFailureClassified` audit payload — genuinely useful, but not
   self-healing.

   *What to build.* A capped, backed-off auto re-enqueue for `Transient`
   rows, so a network blip or a briefly-full disk resolves itself instead of
   waiting on an operator who may be asleep. That is the never-lose-a-job
   ethos the rest of this daemon is built to: a quote should not sit dead
   overnight because a volume was unmounted for a minute.

   *Why it is not built here.* It is a scheduler change, and the scheduler
   is the exact surface the D-PRICEQ head-of-line incident came out of. An
   auto-retry that re-enqueues at the FIFO head, or that does not cap, or
   that does not distinguish "retried and failed the same way" from "not yet
   retried", re-introduces the wedge in a new costume. It needs its own
   design: where the row re-enters the queue, how the attempt counter and
   backoff persist across restarts, how the audit chain records attempt *n*,
   and how it interacts with the stale-job reaper (a row being auto-retried
   is moving, so it must not be reapable — and must not be able to
   auto-retry forever either). Until then, the honest position is stated in
   `classify_failure`'s own doc comment: it labels; it does not retry.

**Blocked on.** Nothing external; all four are our own work in the pricing
daemon.

**Size.** A1 medium — the schema change is small, the design decision (what
the reaper is allowed to conclude from) is the real content. A2 small-to-
medium. A3 small. A4 medium — the code is small, the scheduling design is
the content.

---

<a id="d-21"></a>
### D-21 — Audit single-writer: the three residuals ADR-0099 R2 did not close

**Surface today.** Every in-process writer of *either* half of the audit
ledger — the DuckDB `audit_ledger` table and the `<db>.audit.log` mirror —
now routes through the one shared `aberp_db::Handle` writer, and
`ensure_consistent_with_db` holds the mirror's exclusive `flock` across its
whole decide→act window. Cut-gate **CHECK 10P**
(`tools/adr0099_audit_writer_scan.awk` +
`tools/adr0099_audit_writer_residuals.txt`) classifies all 214 runtime write
sites across the workspace and fails the build on any site that cannot prove
its serialization domain. See ADR-0099 §R2.

The three items below are named in ADR-0099 §R2.8 as honest residuals. None
is implicated in any incident to date; each is a way the second-writer class
could return that R2 does not structurally prevent.

Note the scope honestly: this gate covers the **second-writer** class. The
seq-2508 incident was **not** that class — it was a lost DB commit (ADR-0099
§R2.2), tracked separately as [D-22](#d-22).

**R1 — cross-process table-side forks are still detection-only.** A CLI
subcommand (`aberp retry-submission`, `aberp drain-pending-retries`, …)
running while `aberp serve` holds the DB is outside every in-process lock,
exactly as `AUDIT_APPEND_LOCK`'s own docs state. R2 closes the *mirror* half
of this (its `flock` is genuinely cross-process); the table half is
backstopped by hash-chain detection alone. The fix is the whole-DB advisory
lock ADR-0099 already flagged for v0.2.9 — an `fs2` flock on the tenant DB,
pattern `submission_lock.rs`, so a CLI **refuses** while serve holds it
rather than racing it.
*Blocked on:* our own work. *Size:* medium — the lock is small; deciding
what a refused CLI should print, and whether `serve` must publish liveness,
is the content.

**R2 — `serve.rs::run` is allow-listed by CHECK 10M, 10N and 10P alike.** A
fork planted in the boot fn passes all three gates. The allow-list is
justified (it runs before `open_tenant_handle`, single-threaded, no daemons
spawned yet) but it is a real hole in a 3,000-line function that only grows.
The fix is to extract the pre-Handle boot sequence into named, individually
allow-listed fns so the exemption covers the boot *steps* rather than the
whole of `run`.
*Blocked on:* our own work. *Size:* medium, mostly mechanical, but it
touches the boot ordering, which is load-bearing for durability (ADR-0110
D3 / ADR-0111).

**R3 — `append_in_tx` still takes no lock.** The two serialization domains
(the handle writer mutex, and audit-ledger's `AUDIT_APPEND_LOCK`) remain
distinct; `Handle::with_ledger` is still the only construct that holds both.
CHECK 10P's `LEDGER_LOCKED` verdict is a *classification* — "this site holds
the append lock end-to-end" — not a proof that a `Ledger`-domain writer
cannot race a handle-domain one. Closing it means making the audit-append
chokepoint itself acquire the one lock, which needs a split public/`_locked`
pair so `Ledger::append` and `append_reopen` (which already hold it) do not
deadlock re-entrantly, **and** the lock must span the *caller's* commit, not
just the insert — otherwise two writers on different connections still read
the same head. That last point is why it is not a small change.
*Blocked on:* our own work. *Size:* large — it is ADR-0105's residual, and
the commit-spanning requirement means an API change at ~120 call sites, not
a lock added inside one function.

**Not in scope here.** The scanner is a heuristic over text with no type
information: a connection that reaches a writer through a struct field or a
trait object lands in `UNCLASSIFIED`, which is RED — the intended direction
— but it means the frozen residual file, not the compiler, is what keeps the
sanctioned set honest. Making that a type-level property is a different
(and much larger) piece of work than R1–R3.

---

<a id="d-22"></a>
### D-22 — Audit durability: the 15 CLI sites that fsync the mirror and not the DB

> ### ✅ CLOSED — 2026-08-26, [ADR-0114](../adr/0114-editions-money-cli-durability-d22.md)
>
> All thirteen `apps/aberp` sites are on the shared `aberp_db::Handle` and
> every money-path ack boundary across the eleven NAV commands now calls
> `db.durable_ack()?` before the operator is told the write landed. The
> `cut_gate_durable_ack.sh` census grew 5 → 26 sites, and the eight migrated
> files were **promoted** out of CHECK 10i's frozen residual ledger into CHECK
> 10h's zero-openers-ENFORCED set — a re-added opener is now a red build rather
> than a tolerated count. CHECK 10L-b's and CHECK 10N's frozen fork manifests
> are both EMPTY.
>
> The gap this entry described in 2b below turned out to have a **second
> shape** the entry did not name: `submit_invoice`, `poll_ack` and
> `drain_pending_retries` were already Handle-routed, so the flush ran — but
> nothing CLAIMED its outcome, so a failed flush printed
> `submitted invoice … -> NAV transactionId …` anyway. `submit_invoice` is
> described below as "the one benign case"; it was benign in its *ordering* and
> not in its *reporting*. All three now ack.
>
> **The two `aberp-snapshot::recover` sites are deliberately NOT converted** —
> see ADR-0114 §5. They are boot-time, pre-`Handle`, on a private staging file,
> neither writes a money row, and `build_and_validate` §5d's mirror top-up is an
> INPUT to the ahead-snapshot self-certification gate (its result decides
> Recover vs Refuse), so it structurally cannot move after the install. So the
> count closed is **13 of 15**, with the remaining two named rather than
> dropped.
>
> Pinned by `apps/aberp/tests/d22_money_cli_power_loss_durability.rs`: a
> power-loss spec on the real `mark-abandoned` path plus a fault-injection test
> that breaks the filesystem reach and demands the ack refuse. Three mutations
> were RUN and killed (ADR-0114 §6) — including the one that shows a
> debounce-shadow design for this spec is **vacuous**, because the durable-set
> copy takes whole files.

**Why this exists.** ADR-0099 §R2 established that the seq-2508 incident was a
**lost DB commit**, not a writer fork: pre-ADR-0110-D3, `WriteGuard::drop`
`fsync`ed the audit MIRROR on every commit and never flushed the DB — the
durability ordering exactly inverted, so an unclean stop keeps the mirror line
and loses the DB row. D3 fixed that for the serve path in v0.4.0, and
`daemon_heartbeats_are_power_loss_durable` pins it.

**The same inversion is still live on the CLI paths — as CODE, on this tree.**
ADR-0099 §R2.2's "a pre-existing residual of the DEPLOYED binary, not a live code
gap" is true of the **serve/daemon** path only (durability closed by ADR-0110 D3
/ ADR-0111 from v0.4.x, power-loss-proven by
`daemon_heartbeats_are_power_loss_durable`). It is **not** true of these sites:
they are the pre-D3 inversion, unfixed at `main`, on NAV **money** paths. That
makes D-22 an elevated fix awaiting scheduling — not a backlog nice-to-have.
15 runtime sites do:

```rust
let ledger = Ledger::open(db_path, …);   // independent connection, no Handle
ledger.append(…);                        // commit
ledger.sync_mirror(&mirror_path);        // mirror → sync_all()  DURABLE
```

No Handle, no `durable_ack`, no `fsync_data_paths` — and `Ledger::open` sets
`PRAGMA disable_checkpoint_on_shutdown`, so the connection's close deliberately
folds nothing either. The mirror is explicitly made durable; the DB's durability
is left to whatever DuckDB does with its WAL on commit, which is precisely the
assumption D3 exists to stop relying on.

**Where.** `drain_submission_queue` (×3), `retry_submission` (×4),
`submit_annulment`, `poll_annulment_ack`, `observe_receiver_confirmation`,
`recover_from_nav`, `mark_abandoned`, `request_technical_annulment`, plus two in
`aberp-snapshot::recover`. These are NAV **money** paths.

`submit_invoice` is the one benign case and is worth reading as the target
shape: it opens a shared Handle and does its `db.write()` windows first, so the
guard drop flushes the DB *before* the later explicit `sync_mirror`.

**Why it is not fixed in R2.** Deliberately out of scope: seq-2508 was a
serve-path daemon heartbeat, and folding a money-path change into that fix would
have made it much harder to review. Splitting it was Ervin's call.

**Missing for Live.** Bring the 15 under the same *data-before-the-record-that-
points-to-it* ordering — route them through the shared Handle where the process
has one, or give them an explicit flush before the mirror sync where it does
not. Then extend the `cut_gate_durable_ack.sh` census so a new unordered site
cannot appear unlisted: today that census tracks Handle-routed `durable_ack()`
money-path sites, and these use `Ledger::open` instead, so they were never in
its scope.

**Blocked on:** our own work. **Size:** medium — the edit per site is small and
mechanical; the content is deciding the CLI posture (a cross-process advisory
lock is the neighbouring question, [D-21](#d-21) R1) and extending the census
without making it churn.

---

<a id="d-99"></a>
### D-99 (PROVISIONAL NUMBER) — QC inspection reports + Certificate of Conformance, attached to the shipment

> **⚠️ NUMBER PROVISIONAL — assign unique at merge; collides with
> auto-probe / portal.** `D-99` and its ADR number (`ADR-0199`) are
> deliberate placeholders. **D-20** is now taken on `origin/main` — the
> pricing-queue head-of-line fix landed as `6182c6e` and owns the entry
> directly above this one. Two *unmerged* branches still claim **D-20** as
> well and will each need renumbering in turn: the internal-portal ADR
> (ADR-0115 / D-20) and the auto-probe pricing ADR (ADR-0113 / D-20,
> `docs/adr-auto-probe-inspection`). The highest id in this file on
> `origin/main` is therefore **D-20**; the highest ADR is **0112**. Whoever
> merges this must renumber the entry, the anchor, the ADR file, and the
> cross-links in both directions.

**Not a README row.** Like [D-19](#d-19), this is a *build and phasing*
entry, not a "Designed — awaiting hardware/endpoint" capability. It has no
README counterpart and adding one would be wrong — the 1:1 rule above
governs the Designed rows, not gate/phasing entries. Design:
[ADR-0199 (provisional)](../adr/0199-defense-qc-inspection-report-and-certificate-of-conformance.md).

**Surface today.** The *measurement* half is Live and the *reporting* half
does not exist.

Live (ADR-0092 / S443, all reproduced at `origin/main` `9e4a6ee`):
`qc_inspection_plans` and `qc_inspections`
(`crates/aberp-qa/migrations/V002__qc.sql:22`, `:47`), the pure
`compute_verdict` pass/minor/major/critical + calibration-stale rule
(`crates/aberp-qa/src/qc/verdict.rs`), the `record_inspection` write
chokepoint that emits the `qc.*` events inside the caller's tx
(`crates/aberp-qa/src/qc/inspections.rs`), the `ProbeIngestionSource` trait +
`RawProbeEvent` (`crates/aberp-qa/src/qc/probe.rs:52`), six `qc.*` event
kinds (`crates/audit-ledger/src/entry/event_kind.rs:3086`), and the manual
entry route `GET|POST /api/qc-inspections` (`apps/aberp/src/serve.rs:4603`).

Adjacent surfaces the reporting work plugs into: the two existing
defense-only shipment gates, whose shape the third one clones —
`resolve_part_uid_gate` / `enforce_part_uid_gate_for_shipment`
(`serve.rs:17250`, `:17286`) and `resolve_open_ncr_gate` /
`enforce_open_ncr_gate_for_shipment` (`serve.rs:17384`, `:17427`);
`mark_shipped`'s single transaction with its injected `InvoiceSpawner` +
`ExportControlContext` (`crates/aberp-dispatch/src/repository.rs:530`);
the pure PDF renderers `aberp-quote-pdf` (`src/lib.rs:214`) and
`aberp-invoice-pdf` with their render-on-demand route precedent
(`apps/aberp/src/print_invoice.rs:148`, `serve.rs:4283`); the traceability
spine `wo_part_marks` + `trace_part_uid`
(`apps/aberp/src/part_marking.rs:177`, `:471`) and
`MaterialTraceabilitySeed.mill_cert_id`
(`crates/aberp-compliance/src/lot_heat/mod.rs:164`); and the evidence bundle
whose allow-list must be widened deliberately
(`crates/aberp-verify/src/bundle.rs:120`).

**Verifiably absent** (four sweeps over `*.rs` / `*.sql` / `*.svelte`):
no report entity of any kind; no balloon/characteristic number, designator,
type, method, or required flag on `qc_inspection_plans`; **no drawing number
and no drawing revision anywhere in the repo** (`work_orders` carries
`product_id` and nothing else — `crates/aberp-work-orders/migrations/V001__work_orders.sql:29-43`);
and no link from a measurement to a serialised unit or to a dispatch.

> ### ✅ PHASE 1 IMPLEMENTED — 2026-08-23
>
> Ervin accepted the spec and **every flagged decision at its conservative
> default**, confirming two explicitly: **AS9102 Rev C** is the default FAIR
> form, and an **incomplete report BLOCKS a Defense shipment**. The Phase-1
> list below is built, on branch `docs/adr-qc-inspection-report`, with two
> deliberate deltas from what the design pass predicted:
>
> - **AS9102 Forms 1/2/3 shipped in Phase 1, not Phase 1b** — Ervin named
>   Rev C as the default form, so it is built rather than scheduled.
> - **`submit_measured_characteristics` + the three `RawProbeEvent` fields
>   were NOT built.** That batch entry point is the Phase-2 seam and it has
>   no Phase-1 caller: reports read the `qc_inspections` rows the live
>   manual route already writes. Building an interface with no caller would
>   have been speculative, so it moves to the Phase-2 session — where its
>   first real consumer is.
>
> Phase 1c (auto-attach to the shipment e-mail) remains OFF and unbuilt, as
> decided. See the ADR's §"Open questions" for every resolved decision.

**Missing for Live — Phase 1 (ships on its own; works on today's manual
actuals, before any probe transport exists).**

- Six additive columns on `qc_inspection_plans` (`characteristic_number`,
  `characteristic_designator`, `characteristic_type`, `inspection_method`,
  `sheet_zone`, `is_required`) + a new `part_drawing_refs` table with
  revision history, because nothing today can name a drawing.
- `qc_reports` + `qc_report_lines` — a **frozen snapshot**, not a live view
  over `qc_inspections`. Plans are mutable (`update_plan` /`archive_plan`,
  `qc/plans.rs:157`/`:198`), so a live report would silently rewrite its own
  history; `qc_inspections` already made this exact call and documented it
  (`V002__qc.sql:41-46`).
- **Characteristic accountability** (the AS9102 Form 3 discipline): the
  report enumerates every enabled required characteristic for the product and
  renders an explicit `not_measured` row for any without a measurement.
  Unaccounted > 0 ⇒ disposition `incomplete`. A report that lists only what
  was measured is the selective-recording failure mode moved to the printer.
- The measured-characteristics input interface: three `Option` fields on
  `RawProbeEvent` (`part_serial`, `characteristic_number`, `program_id`) and
  one batch entry point `submit_measured_characteristics`, all-or-nothing per
  unit, routing every element through the existing `record_inspection`
  chokepoint. **Interface only — no transport.**
- `aberp-qc-pdf`, a pure sibling of `aberp-quote-pdf`
  (no clock / no I/O / no RNG ⇒ byte-identical output), rendering
  `DimensionalInspection` + `CertificateOfConformance`.
- The third shipment gate (`resolve_qc_report_gate` +
  `enforce_qc_report_gate_for_shipment`) and a `ShipmentDocumentBinder`
  injected into `mark_shipped` so the `dsp_id` binding rides the same
  transaction as the state flip and the invoice spawn.
- Six `qcr.*` event kinds (**187 → 193**; the pin is at
  `event_kind.rs:4004` — reconcile the arithmetic at merge, other unmerged
  branches also add kinds), with `rendered_sha256` + `renderer_version` on
  `qcr.report_issued`. **The hash is pinned, the bytes are not stored** —
  storing PDFs in the DB loads every durable checkpoint, mirror sync and
  snapshot with a derivable payload, and the AP-artifact-on-disk pattern is
  only sound for AP invoices because NAV holds the master copy. Nobody else
  holds a QC report's master copy.
- `qc_reporting_allowed_for(Edition)` in `build_profile.rs` (the
  `storefront_polling_allowed_for` shape, `:249`), routes, SPA surface, and
  the Portable byte-identity pin.

**Phase 1b:** `As9102Fair` Forms 1/2/3 as a third render shape over the same
data. **Phase 1c:** attaching the report to the shipment e-mail (off by
default — mailing a compliance document automatically is its own decision).

**Missing for Live — Phase 2 (auto-populated actuals).** The MC Connect
probe-results pipeline (FANUC `DPRNT`-to-file / Siemens OPC-UA R-parameters /
MTConnect) landing behind `submit_measured_characteristics`. This is
[D-02](#d-02) and it shares its MTConnect work with [D-16](#d-16). If the
seam in Phase 1 is drawn correctly, Phase 2 changes **no report code at
all** — that is the test.

**Blocked on.**

- **Phase 1: nothing external.** Everything it reads exists at `9e4a6ee`.
  The one non-code dependency is the drawing number/revision, which is
  operator-entered master data, not an integration.
- **Phase 2: physical access to the NTX and its control** — the same block as
  [D-02](#d-02).
- **Three decisions that are process commitments, not code.** Whether a
  missing or incomplete report **blocks** a Defense shipment (the design
  default is yes, mirroring the two existing gates — once on, a Defense
  shipment cannot leave without a complete report); which standard/forms the
  customers actually mandate (default AS9102 Rev C; primes and Nadcap may
  layer their own); and whether an **unsigned** CoC is acceptable (a real
  signing ceremony is [D-15](#d-15) + [D-06](#d-06)). All eleven flagged
  decisions are in the ADR's Open questions.
- **One documented breaking change.** `mark_shipped` gains a sixth parameter
  and `aberp-verify`'s bundle allow-list (`bundle.rs:120`) must be widened
  for a `qc/` directory — older verifiers will reject newer bundles, loudly
  and deliberately.

**Size.** Phase 1: large — three tables, one new crate, one gate, one
injected trait through an existing atomic transaction, six event kinds, and
an SPA surface. The renderer and the accountability computation are the two
pieces with real logic; the rest follows existing templates closely enough
to be mechanical. Phase 1b: small (one more layout over the same rows).
Phase 2: medium per transport, and shared with [D-02](#d-02)/[D-16](#d-16).

#### Still owed after Phase 1 (round-2 review, 2026-08-23)

Three items the adversarial pass named that were deliberately NOT closed in
the round-2 fix. Each is scope, not an oversight.

1. **A WRITER for the `qc/` bundle entry — the AC10 scope gap.** Retention
   itself is wired: `qcr.report_issued` pins the SHA-256 into the chain, and
   `aberp-verify` accepts, re-hashes and cross-totals `qc/` entries. What
   has no producer is the auditor-facing bundle: `qc_archive_path` has zero
   non-test callers, so no export ever emits a `qc/` file for the verifier
   to check. Closing it needs the invoice→dispatch→WO→report join that
   decides which reports belong in an invoice-scoped slice.
   *Size:* small-to-medium, and it is the last thing between AC10 and a
   genuinely auditor-ready export.
2. ~~**`plan_drift` cannot see a characteristic PROMOTED from optional to
   required.**~~ **CLOSED in round 3, and the round-2 reasoning above it
   was wrong.** The note claimed detection needed an additive `is_required`
   column on `qc_report_lines`. It does not. `required` is indeed not
   persisted (`parse_line_row` reconstructs it as `true`), but
   `accountability` *is* persisted and read back — and an unmeasured
   characteristic's frozen row is exactly an
   `Accountability::NotMeasured` accountability row. Excluding those rows
   from the gate's `covered` set closes the promotion case with no schema
   change: a promoted-but-never-measured characteristic is required today
   and not covered by the report, so `plan_drift` fires. Pinned by
   `an_optional_characteristic_promoted_to_required_re_blocks_the_shipment`
   and — for the PER-UNIT form of the same bypass, which the one-unit test
   cannot separate —
   `a_characteristic_measured_on_only_some_units_is_not_covered_for_the_rest`.
3. **A VOID/SUPERSEDED-stamped rendering for auditors.** A report that is
   no longer current — `Voided`, or `Superseded` by a later one — is
   refused with a 409 rather than rendered, because `state` cannot appear
   in the hashed bytes and it would otherwise look exactly like a valid
   certificate. Round 3 extended the refusal from `Voided` to `Superseded`
   on the same reasoning, and the superseded case is the sharper one: the
   report a supersede replaces is typically the flattering early `accept`
   that a later `reject` corrected, which is precisely the document the
   shipment gate refuses to ship on. A stamped copy is strictly better than
   a refusal for both, but the stamp has to be drawn OUTSIDE the byte-form
   the SHA is taken over — a renderer change, not a route one. Until then
   the full record stays available on `GET /api/qc-reports/:id` and in the
   chain; only the *unmarked PDF* is withheld.
   *Size:* small, but it needs a design decision on how the stamp and the
   hash coexist.
4. **An unparseable stored timestamp fails the operation loud, rather than
   being repaired.** Round 3 found the same string-vs-instant ordering
   defect in two places — the gate's report recency
   (`report_recency_key`) and the report's own measurement selection
   (`latest_measurement`) — and fixed both to order by the parsed instant.
   An `Option<OffsetDateTime>` sorts `None` lowest, which would silently
   DEMOTE a row nothing can date; since demoting the `reject` is how a bad
   part ships, both call sites now refuse instead
   (`refuse_unparseable_report_timestamps` at the gate,
   a `QcError::Validation` in `freeze_report`). Both are unreachable by
   construction today — every such timestamp is minted through one
   `rfc3339` helper — so the refusal is a backstop against rows written
   outside the application. What is *not* built is any operator-facing way
   to see or repair such a row: today it is a 409/500 an operator must
   escalate.
   *Size:* small; it is a diagnostic surface, not a mechanism.
5. **The QC-report LIST routes order by the `created_at` string.**
   `list_reports_for_wo` / `list_reports_for_dispatch` carry
   `ORDER BY created_at`, which inverts on exactly the trimmed-fraction
   pairs round 3 fixed in the two places that DECIDE. Deliberately left:
   the shipment gate re-sorts in-process (so it does not depend on another
   crate's `ORDER BY`), and issuance and void address a report by id — so
   this is a display order only, and two reports frozen inside the same
   second may list the wrong way round. Worth fixing when the list gets a
   real operator surface.
   *Size:* small.

#### Closed in round 4 (2026-08-24)

The round-3 adversarial confirmed every round-3 fix and found two further
paths to the same outcome. **Neither is a round-3 regression** — both are
original Phase-1 gaps, and both are joins the gate performs on data the
WRITES normalise differently, or do not re-read at all. Both are closed;
neither needed a schema change or a new event kind.

6. **`ensure_unique` compared the RAW plan name while the writes stored it
   TRIMMED.** A second active plan submitted as `" Bore D "` matched no
   existing row, passed the in-code uniqueness check, and was then written
   under the same STORED name as the first. `(product, feature_name)` is the
   key the shipment gate joins on and `required_now` is a SET of trimmed
   names, so the two plans collapsed to one element — the first plan's
   measurement covered the second plan's name, and a required characteristic
   nobody ever measured shipped with it. The round-3 `NotMeasured`
   subtraction could not see it: the duplicate is created after the freeze
   and has no frozen line. `ensure_unique` now normalises both key columns
   with the same `.trim()` the writes apply. Pinned by
   `a_padded_duplicate_plan_name_cannot_collapse_the_gates_join`, which
   carries the self-collision and distinct-characteristic counter-directions.
7. **The gate never re-checked the report's UNIT SCOPE.** A report frozen
   and issued BEFORE any part was marked takes `build_report_lines`'
   `units.is_empty()` branch — every characteristic degrades to one
   lot-level line matched against ANY measurement of it — so it issues as a
   clean `accept` with `serial_range = None`. Mark N parts afterwards and the
   name-keyed coverage join still passed: N serialised units released on a
   document enumerating none of them. Blocked now under a new gate reason
   `UnitDrift`, whose 409 points at the marks rather than at a characteristic
   that is not the problem. The check is written as scope EQUALITY against a
   recomputed `serial_range_of`, which is wider than the finding's `is_none`
   form; the extra reach is a BACKSTOP, not a second live case
   (`record_part_marks` refuses once a WO has any mark, so the mark set is
   written once, all at once). Pinned by
   `a_report_frozen_before_part_marking_does_not_release_the_marked_units`
   and, for the backstop arm,
   `a_report_covering_only_some_marked_units_does_not_release_the_rest`.
   **No new EventKind:** the gate emits one kind and carries the cause as a
   `reason` string, so `ALL_KINDS_COUNT` stays **195** at all three pins.

#### Closed in round 5 (2026-08-25)

The round-4 adversarial confirmed both round-4 fixes (12/12 mutations killed)
and found that item 6 was only **half** closed: it shut the case where two
*simultaneously active* plans collapse onto one stored name, and left the
case where they are never active at the same time.

9. **The coverage join keyed on a BORROWABLE label, not on the plan.**
   `qc_report_lines` has no `plan_id` column and `line_from` copies
   `characteristic_name` from the MEASUREMENT's snapshot, so the gate asked
   "is this NAME covered?". `ensure_unique` enforces its key only among
   NON-archived rows — deliberately — while a frozen line outlives archival,
   and a stored name is freed by archiving *and* by renaming. Three ordinary
   sequences therefore let one plan's measurement stand in for a different
   plan's required characteristic: archive the measured plan and re-create
   its name; rename an existing optional plan onto the freed name and promote
   it; or, with **no archival at all**, rename the measured plan and demote it
   to optional, then create a new required plan under the name that frees up.
   In every one a required, never-measured characteristic shipped on evidence
   describing a different characteristic — measured against the OLD tolerance
   band. Coverage is now ALSO keyed on plan identity, via
   `qc_report_lines.qci_id` → `qc_inspections.inspection_plan_id` (the same
   key `latest_measurement` joined on when the line was built), read through
   the already-public `list_inspections_for_wo`. **No schema change, no
   migration, no new EventKind** — the block reuses `PlanDrift` and
   `ALL_KINDS_COUNT` stays **195**. It subsumes the `product_id` variant of
   item 6 for free. Both joins are kept and the mutation shows neither
   subsumes the other: dropping the identity term reddens exactly the three
   new probes
   (`an_archived_plans_measurement_cannot_cover_its_recreated_namesake`,
   `a_plan_renamed_onto_an_archived_name_is_not_covered_by_its_measurement`,
   `demoting_the_measured_plan_frees_its_name_but_not_its_coverage`), while
   dropping the NAME term instead reddens
   `a_characteristic_measured_on_only_some_units_is_not_covered_for_the_rest`
   on its own. The two comments that made this easy to reason past — both
   calling `(product, feature_name)` "the plan table's own uniqueness key",
   true only among currently-active rows — are corrected in the same change.

#### Closed in round 6 (2026-08-25)

The round-5 adversarial confirmed the identity fix closed (all three
archived/renamed-plan variants block; both coverage terms non-subsuming;
31/31) and found one undisclosed blocker plus the COMPOSITION of two items
this list had recorded separately.

11. **The NCR belt matched on PART UIDs only — and composed with item 8 it
    released a failed part.** `open_ncr_ids_blocking_part_uids` intersected an
    open NCR's `affected_part_uids` with the WO's marked units, while an
    auto-NCR carries exactly what the measurement carried
    (`req.part_uid.clone().into_iter().collect()`). A batch / first-article /
    lot-level measurement names no unit, so a failing one spawns an `Open`
    NCR with an EMPTY part list and the intersection matched nothing.
    Composed with item 8 — the report is frozen at issuance and a later
    failure does not re-open the QC-report gate — the live sequence is:
    measure the unit, issue an `accept` report, record the failing batch
    measurement, ship. A real Open Critical nonconformity stood against the
    order and nothing refused it. Closed exactly where round 5 said it would
    be, in the pure helper: `open_ncr_ids_blocking_wo` matches an NCR naming
    one of the WO's marked units **or** naming the WORK ORDER, which is the
    one key both the dispatch and the failing measurement always carry. The
    second arm round 5 flagged is closed with it — the
    `part_uids.is_empty() → Pass` early exit is gone, since an unmarked WO is
    precisely the shape a lot-level nonconformity is raised against (the
    Defense path still refuses an unmarked WO at `resolve_part_uid_gate`).
    **No new EventKind and no new gate reason** — the block reuses
    `OpenNcrGate::Blocked` and the existing `ncr.wo_blocked_by_open_ncr` row;
    `ALL_KINDS_COUNT` stays **195**. Pinned by
    `a_failing_lot_level_measurement_after_issuance_still_blocks_the_shipment`
    (the whole composition, ending in the refusal, and asserting the
    QC-report gate is still `Pass` so the block cannot be coming from
    somewhere else) plus two counter-direction unit tests.
12. **`characteristic_type` was an editable field that decided the
    accountability ARITHMETIC.** `build_report_lines` partitions on
    `is_lot_level`: `Material` / `Process` report ONCE for the whole
    shipment, everything else once per serialised unit. One ordinary
    `PUT /api/inspection-plans/:id` changing only that field — same
    `plan_id`, same name, still required — re-partitioned an existing row.
    With two units marked and only SN-001 measured: before, `incomplete` →
    gate `Blocked`; after, the two lines collapse to one, that line swallowed
    SN-001's measurement, `unaccounted` fell 1 → 0, and the report came out
    `accept` while `serial_range` still read *SN-001 … SN-002 (2 units)*.
    **Neither round-5 coverage term could catch it** — `compute_disposition`
    itself flipped, so the gate had already passed `permits_shipment()` and
    both terms are satisfied by construction once the single line is
    `Measured`. Root cause of the invisibility: the lot-level partition was
    covered only in the direction that could not fail — the one test that
    built a `Material` plan supplied a genuine LOT measurement, which passes
    under both the old rule and the new one, and `Process` was never
    constructed at all. Closed
    with two belts — a typed `Evidence` rule (a lot-level line in a
    unit-scoped report is backed only by a measurement recorded as a lot
    fact, i.e. carrying no `linked_part_uid`) and a refusal in `update_plan`
    to change `characteristic_type` at all once the plan has recorded
    inspections. The permissive no-marked-units arm is deliberately
    unchanged: tightening it yields an EMPTY report, not a stricter one, and
    that case is already `UnitDrift`'s. **No new EventKind** —
    `ALL_KINDS_COUNT` stays **195**.
13. **`record_part_marks` inserted `m.wo_id`, not the `wo_id` it guarded
    on.** The `AlreadyMarked` guard counts marks on the `wo_id` PARAMETER;
    the insert wrote each mark's own copy. Not client-reachable — the route
    hard-sets `m.wo_id` from the path — but it made the guard bypassable by a
    future internal caller. The insert now uses the guarded parameter.

#### Still owed after round 6

8. **The report does not re-open on a LATE measurement.** `UnitDrift`
   compares the unit scope and the round-3 subtraction compares the
   characteristic names; neither re-reads `qc_inspections`. A measurement
   recorded or corrected after issuance therefore does not re-open the gate
   — by design, since the report is a frozen record and a live
   re-derivation is what the §D7 hash pin forbids. The remedy today is to
   supersede the report, which is a manual operator step with no prompt.
   **Round 6 removed its teeth without closing it:** the failure a late
   measurement records now spawns an NCR the belt sees (item 11), so the
   shipment is refused even though the report still says `accept`. What
   remains is that the DOCUMENT is stale, and an operator has to notice.
   *Size:* small; it is a nudge on the report surface, not a mechanism.
10. **`UnitDrift` compares the RENDERED range string, not the unit set.**
    (Stated in ADR-0199's round-4 residuals; recorded here so the backlog and
    the ADR agree.) `serial_range_of` prints *first … last (n units)*, so a
    mark set with the same first serial, last serial and count but a
    different middle compares equal — and it reads only `part_serial`, so a
    set whose serials are all preserved but whose `part_uid`s were rewritten
    also compares equal. `part_uid` is the term the per-serial measurement
    join and the NCR belt key on, so the identity that matters most to
    coverage is the one the comparison cannot see. Both shapes are
    out-of-band-only: `record_part_marks` writes the mark set once and
    refuses a second write. Comparing the full set would mean re-deriving the
    enumeration from `qc_report_lines`, which carries no per-unit rows when
    every characteristic is lot-level.
    *Size:* small, but it needs the enumeration to come from somewhere other
    than the printed string.
14. **An NCR that names NEITHER a part UID nor a WO still blocks nothing.**
    There is no key left to join on, and inventing one (heat lot, product)
    would refuse shipments the operator never associated with the order.
    Every auto-NCR from `record_manual_inspection` names at least the WO
    whenever the measurement did, so this is a shape only a hand-written NCR
    reaches.
    *Size:* not a defect so much as the belt's outer edge; revisit if a real
    NCR surface starts producing unattributed rows.
15. **`open_ncr_against` — the `accept_with_ncr` disposition arm in
    `qc_report.rs` — still matches on part UIDs only.** Deliberately left:
    it decides how a report is LABELLED, not whether a shipment leaves, and
    `AcceptWithNcr` permits shipment either way. Widening it would change a
    printed document's wording, which wants its own review.
    *Size:* small.
16. **A characteristic's type is immutable once measured, with no override.**
    An operator who mis-sets it must archive the characteristic and create a
    new one. That is one step more than an edit, and it is the step that
    makes the replacement measurable on its own identity — but there is no
    operator-facing prompt saying so; today it is a 400 with a message.
    *Size:* small; it is a UX nudge on the plan form.
17. **A shipment waiver names an authenticated LOGIN, not a role.**
    ADR-0090's round-7 amendment makes an audited management sign-off the
    only release for a non-`Closed` NCR, and the sign-off records the
    `require_ready` operator login as the approver. There is no RBAC in
    PROD, so nothing enforces that the signer is *entitled* to sign — the
    same limitation the CAPA approve/verify sign-off has always had. What is
    enforced is that a person signed, that they stated why, and that both
    are in the hash chain.
    *Size:* large — it is the role system, not a patch to this route.
18. **A shipment waiver cannot be revoked.** `ncr_shipment_waivers` is
    append-only; an over-broad waiver is corrected through the NCR it names.
    A revoke needs a second EventKind and a precedence rule between the two,
    and neither was asked for.
    *Size:* small, but it wants a decision before it wants code.
19. **A post-issuance `CalibrationStale` measurement still surfaces
    nowhere.** Round 7's B-2 fix keeps a stale-calibration measurement out of
    a report being FROZEN, and the NCR belt catches a post-issuance FAILURE
    — but `CalibrationStale` raises no NCR by design (an untrusted probe must
    not manufacture a false defect), and an issued report is immutable. So a
    probe found out of calibration after the certificate was issued reaches
    neither gate. Closing it means a second belt keyed on stale-calibration
    measurements against the WO, not a change to `build_report_lines`.
    *Size:* medium; it is a new gate arm, and it needs a policy answer on
    what a stale probe should do to an already-issued document.
20. **The shipment waiver has no SPA affordance.** `POST
    /api/ncrs/:id/shipment-waiver` is live, but the Quality module's NCR
    detail panel has no control for it, so an operator blocked by the widened
    gate needs the route or a closed NCR. A management sign-off wants its own
    confirmation copy, not a fourth transition button.
    *Size:* small, but it is a UX writing job as much as a code one.
21. **Nothing stops an operator signing off their own NCR.** The shipment
    waiver has no separation-of-duties check. Adding one would wedge a
    one-person shop, which the Defense pilot is. The ledger records who
    signed; who is *allowed* to sign is item 17's RBAC question.
    *Size:* falls out of item 17.
22. **The per-serial report arm stays linkage-scoped.** Round 7's B-2 fix
    makes the LOT line's negative direction linkage-blind: any newer failing
    or `CalibrationStale` measurement of the characteristic condemns it. The
    per-serial lines are deliberately untouched — a newer failure linked to a
    DIFFERENT unit must not condemn this unit's line. The one shape left
    open is a newer LOT-level failure against a per-serial characteristic
    (one lot fact fanning out to N unit lines), which is a different join and
    was not the reported defect.
    *Size:* medium; it needs the fan-out rule decided first.

---

<a id="d-20"></a>
### D-20 — Outbound-only remote portal (ADR-0115 Phase 0) — deploy it

**Surface today.** Three crates, complete and tested, with nothing
deployed:

- `crates/aberp-portal-core` — the poll/deliver wire shapes
  (`PROTOCOL_VERSION = 3`), mutual leaf-certificate pinning (`pin.rs`),
  constant-time compare, and the canary classifier.
- `crates/aberp-portal-agent` — the Mac daemon. Polls **out** to the relay
  (`poll.rs`), IS the WebAuthn relying party (`webauthn/`), verifies Apple
  attestation against a vendored root (`webauthn/attestation.rs`), stages
  enrolments for console confirmation (`enrol.rs`), and proxies a
  four-route read-only allowlist to the local `aberp serve`
  (`allowlist.rs`). There is no `TcpListener` anywhere in the crate.
- `crates/aberp-portal-relay` — the VPS binary. Parks requests in a
  bounded queue (`broker.rs`), owns its own HTTP/1.1 connections
  (`http1.rs`), and answers every un-authenticated request exactly as a
  parked nginx would (`nginx.rs`).

The end-to-end path is exercised in one process on loopback —
`crates/aberp-portal-agent/tests/e2e_portal.rs` and `e2e_canary.rs` — with
the real front, the real relay, the real pinned Leg-B handshake, the real
poll transport and the real relying party. The disguise is diffed against a
**live nginx** across 101 request classes by
`crates/aberp-portal-relay/tests/nginx_differential.rs`, which also pins
fifteen prefixes both servers must answer with silence.

**What the disguise claims, exactly.** Round 3 narrowed it, because the
round-2 wording was broader than the code could hold: **no hang, no socket
desynchronisation, byte-identical on the enumerated common request
classes**. Round 4 found a fifth hang and, in the course of closing it,
found that the residual had been *understated as a closed list of three*.

The fifth hang was the body-side twin of the head-side family: any request
that **declared a body and then withheld it** — a `Content-Length` with no
payload, a `Transfer-Encoding: chunked` with no chunk size — made the front
go silent for the full 60-second body timeout and only then answer, where
real nginx answers in under a millisecond with its ordinary keep-alive
page. It was both the de-anonymising tell the disguise exists to remove and
a one-packet way to pin one of 512 connection slots for a minute. The fix
is structural rather than a shorter timeout (nginx answers at once
regardless, so *any* wait is measurable): the body is read only on the one
path whose answer needs it — the post-knock `/api/` forward, which costs a
valid knock token to reach — and every other class is answered from the
head with the body discarded, before the write for whatever already
arrived and afterwards on nginx's five-second lingering budget for the
rest.

The **residual** is status *class* on pathological malformed input, and it
is now stated as a shape rather than a list, because the list was wrong:
round 4 measured five more divergences nobody had enumerated (lowercase
method, a control character in the target, `HTTP/1.11`, `Content-Length:
+5`, a `+A` chunk size — the last two also hangs, since Rust's `parse`
accepts a leading `+` and nginx does not). All five are closed. What is
claimed now: **promptness holds everywhere, byte-parity holds on the
enumerated classes, and outside them the status class may differ.**
`RESIDUAL_CASES` in the differential carries the known remainder and
asserts of each that both servers answer and both answer promptly. Full
reasoning in ADR-0115 §2.

**Follow-on, named rather than assumed: four enumerated status
divergences, and one configuration question.** Round 5's adversarial
measured four more places the two parsers disagree about validity, in
both directions: a repeated space in the request line (nginx 404, ours
400), a header line with no colon (nginx ignores it and 404s, ours
400), and a NUL or bare CR in a header *value* (nginx 400, ours accepts
and 404s). All four are prompt, all four are now in `RESIDUAL_CASES`,
and none is fixed here — the round that closes them should be the round
that adversarially reviews them, not one that bundles them behind a
hang fix. Separately, and needing a human decision rather than a patch:
**`GET /`**. The differential's fixture nginx has an empty root, so `/`
is a **403**; a vhost parked on the stock `index.html` would be a
**200**; ours is a `404` whatever the path. `/` is the likeliest
request a scanner sends, so what the production vhost is actually
parked as is a deployment decision that should be made deliberately.

**Follow-on, named rather than assumed: the request line is a `&str`, and
nginx's is bytes.** `GET /no\x80pe` — a high byte in the target — is passed
through by nginx to its ordinary `404`, and refused here with `400`,
because `parse_head` requires the whole head to be valid UTF-8. It is a
genuine status oracle and it is in `RESIDUAL_CASES`. It is *not* fixed in
round 4 on purpose: closing it means parsing the head at byte level
throughout, which is a real change to a parser two adversarial rounds have
cleared, and bundling it behind a hang fix is how a fix becomes a
regression. The safe shape is a lossy target — a replacement character can
never match a knock token, which is ASCII — but it wants its own round.

**Follow-on, named rather than assumed: the differential test does not run
in CI.** It needs a live nginx, CI has none, and until round 3 it *silently
passed* when nginx was absent — reporting `ok` for a check that never ran,
on every machine without nginx, for the whole life of the branch. Three
sixty-second hang primitives shipped behind that green tick. It is now
`#[ignore]`d (so `cargo test` prints `ignored` with the reason rather than a
tick) and hard-fails when run without nginx. Making it *gate* a merge needs
an `nginx-light` install step in `.github/workflows/ci.yml`; that step is
**not** added here, because the goldens were captured on nginx 1.31.4 and
the runner ships a different version, so it must be validated against the
runner's nginx before it can be allowed to block a protected branch. Until
then the transcribed goldens in `tests/fixtures/nginx-goldens.txt` are the
CI-side guard, and this test is a local pre-merge step run by hand.

**Missing for Live.** Not code — deployment, and three things only the
operator can supply:

1. **A VPS and a hostname.** The portal's label is a deploy-time secret
   that deliberately never enters this repository, this binary, or a
   Certificate Transparency log — hence the wildcard certificate
   (ADR-0115 §3.2). `tests/no_committed_hostname.rs` is what keeps that
   true.
2. **The wildcard `*.abenerp.com` certificate and the two Leg-B
   identities**, plus each side's pinned SHA-256. Nothing is generated
   here; `--pin-agent` is required and an empty allowlist is refused at
   startup.
3. **The SMTP SPOC credentials** on the Mac, for the canary and enrolment
   alerts. They live on the Mac and nowhere else — §2.4 forbids the VPS to
   hold them, which is why the alert is sent from the side that polls.

Then: a launchd unit for the agent, a systemd unit for the relay, one
console enrolment, and one confirmation typed at the Mac.

**Blocked on.** Our own work, plus an operator decision to rent a VPS. No
vendor, no account application, no licence — the reason this sits in the
unblocked half.

**Size.** Small for the deployment itself. The three known follow-ons are
each their own ADR and are not part of reaching Live:

- **H1 — browser↔agent inner encryption (HPKE).** Phase 2. Until it lands,
  Leg A's TLS terminates at the VPS and payloads transit relay memory in
  plaintext. ADR-0115 §4.3a/§4.3b remove that compromise's ability to
  *enrol*; they do not remove its ability to *watch*.
- **H2 — a read-only-scoped upstream bearer** in `serve.rs`. Today the
  agent holds the same bearer the desktop app does and restrains itself by
  allowlist.
- **H3 — client certificates on the browser leg**, available for
  desktop-only use; §9.3 chose the knock token for Phase 0.

**Note on the frozen tree.** Nothing here touches the Prod invoice tree or
the Portable edition. `cargo tree -p aberp` contains no portal crate, and
that is the check to re-run if this entry ever grows.

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
