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
every adversarial round, and none was introduced by the ADR-0112 round-7
landing that first shipped the miner.

**Surface today.** The STEP hole miner in
`python/aberp-cad-extract/aberp_cad_extract/holes.py` emits `located_holes`
into FeatureGraph v6. The named parts below are where each defect lives:
`_skin_over_axis`, `_root_for_end`, `_walk_caps`,
`_cap_axis_intersections`, `_across_a_relief`, `_opens_outward`,
`_has_flat_bottom`, `SURFACE_CONFUSION_MM`. The two items still open —
3 and 5 — live in `_skin_over_axis` and in the breakout arm of
`_root_for_end`.

**Missing for Live.** Nine items, in priority order — eight defects and
one convention. All are mutation-verified: items 1-5 from adversarial
pass 8, items 6-8 from the D-19 round-2 pass, item 9 from the D-19
round-6 behavioural sweep. **Seven are CLOSED** — 1, 2, 4, 6, 7, 8 and 9,
the last three by D-19 round 7. **Items 3 and 5 remain, and the gate
stands until they are closed.** Note that the defects do **not** all fail
in the same direction: 1, 2, 3, 5 and 7 under-quote, 4 and 6 over-quote,
and 6 does so by three orders of magnitude; 9 does both, on different
parts.

**The depth convention is RATIFIED AT BOTH ENDS — Ervin, D-19 round 7.**

> *A located hole's depth is the DRILL's own tool travel: from the part
> FACE to the deepest point the drill reaches. Anything a DIFFERENT
> operation makes — a countersink, a chamfer, an O-ring gland, a relief,
> a breakout cavity — is its own feature and is NOT folded into the
> hole's depth.*

On the conservative ground that *we will not lose jobs over it*: where
two readings are defensible, take the one that never under-quotes. The
rule has two ends and they are measured to two different things, which is
the part that had to be ruled rather than derived — the **mouth** is
measured from the part's FACE, so a relief there does not SHORTEN the
hole; the **far end** is measured to the deepest point the drill reaches,
so a cavity or a gland there does not LENGTHEN it. Both halves are the
same sentence, *the relief is not the hole*, and each is the conservative
reading at its own end.

What that settled, and what it cost:

- at the far end, **nothing is re-pinned and no fixture moves** — the
  corpus has been at the pole since D-19 round 1. What changes is the
  STATUS of a family of answers: the undercut ball seat, the ball nose,
  the wide spherical chamber and the domed floor are **working as
  intended**, not over-quote defects awaiting a fix. Any future oracle
  measuring a bore to the last place its own WALL is the boundary
  disagrees with the ratified convention and is wrong about this repo,
  not about the geometry;
- at the mouth, **seven committed rows across six geometries are
  re-pinned**, longer, and on the four blind ones the entry point moves
  with the depth. Item 8 has the table;
- the toroidal gland (item 7) is closed by the same mechanism, and a
  fixture for it is committed.

Recorded in full under items 7, 8 and 9 below, and in **ADR-0112 §B.2.1**,
which is the statement of record and is where slice C's `L = h.depth_mm`
reads it.

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
4. ~~**N2 — 118° drill-point apex admitted as a cap.**~~ **— CLOSED by
   D-19 round 4.** `DEGENERATE_ISOLINE_RATIO` was `1e-9` and the apex
   passed as a cap, **over-quoting** by the point length (~0.3·D), in
   both topologies. The round-4 diagnosis is sharper than "the ratio is
   too tight", and it changes the fix: **the quantity is not a ratio at
   all.** On a surface of revolution the collapsed derivative *is* the
   root's own distance from the axis in millimetres, and its partner is
   O(1) mm per mm, so the test only read as dimensionless by accident and
   really asked *"is this root within a nanometre of the apex"* — which
   only a part whose bore runs down a **world axis** answers yes to. So
   the constant is now a LENGTH, `DEGENERATE_ISOLINE_SPAN_MM = 1e-4`,
   with the margin measured across the corpus under nine orientations:
   every collapsed isoline at most **2.4e-07 mm**, every live one at
   least **1.0 mm**, nothing in between. The regression sweep item 4
   asked for is `test_d19r4_backlog_item_4_the_drill_point_apex_is_closed`
   — two topologies × three included angles × five point positions — and
   the same constant is what a rotated **countersink** needed, which is
   how round 4 found it. See round 4 under item 6.
5. **N1 — breakout hole.** When the nose/point breaks the far face, the
   hole reads **BLIND where it is through**.

   **Re-characterised, D-19 round 5.** The item as written had two
   halves and only one of them reproduces. The end-condition half does:
   a Ø8 bore whose ball nose or 118° point breaks the far face of a
   20 mm plate reads BLIND at every breakout depth measured. The
   **"depth exceeding the plate"** half does **not** — the depth clamps
   to the material and never runs past it. Measured over the whole
   breakout family, shoulder at z = 6.0 down to −1.0 in a 20 mm plate:

   | shoulder z | ball tip z | ball nose reads | 118° tip z | 118° point reads |
   |---|---|---|---|---|
   | 6.0 | +2.000 | 18 BLIND | +3.597 | 14 BLIND |
   | 4.0 | +0.000 | 20 BLIND | +1.597 | 16 BLIND |
   | 3.0 | −1.000 | 17 BLIND — **wrong** | +0.597 | 17 BLIND |
   | 2.0 | −2.000 | 18 BLIND — **wrong** | −0.403 | 18 BLIND — **wrong** |
   | 1.0 | −3.000 | 19 BLIND — **wrong** | −1.403 | 19 BLIND — **wrong** |
   | 0.5 | −3.500 | 19.5 BLIND — **wrong** | −1.903 | 19.5 BLIND — **wrong** |
   | 0.0 | −4.000 | 20 THROUGH | −2.403 | 20 THROUGH |

   (A negative tip z means the nose is out the far face; those are the
   rows that should read THROUGH. The two noses break out at different
   shoulder depths, which is why the rows disagree at z = 3.0.)

   So the direction is the opposite of what the item recorded: the depth
   stops at the bore's own cylindrical shoulder, which is **short** of
   the plate rather than past it — an **under-quote**, and it is the
   whole point length short at the moment of breakout. Nothing anywhere
   in the family reads past 20.0. The mechanism the item named — "the
   measurement runs to a pole in the air below the part" — is what D-19
   round 1's void bound and round 3's per-face extent between them
   already stop; what survives is the end reading CAPPED, because the
   nose face still carries that end and its outward normal still points
   back into the bore.

   Still open, still a gate, and still under-quoting; the entry is
   corrected rather than closed.
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

   **Round 3 — whose extent.** The refusal half above shipped keyed to
   the **part's world bounding box**, and a bounding box is a property of
   the whole part while the crossing it judges is a property of one face
   of one bore. Any unrelated feature that reached past the crossing
   enlarged the box and turned the refusal off, on a **single body cut
   from one block**: a 4 mm leg under the far corner of the plate put
   `far_opening_through_bore` back to **24 mm of hole in a 20 mm plate**
   (+100%), and a 2.6 mm boss put `spherical_mouth_undercut_bore` back to
   **13.188 and UNKNOWN** (+45.9%). Neither feature touches the bore, is
   reached by it, or is within 20 mm of it. It was invisible because
   every committed fixture held the part's overall extent fixed while
   varying the bore, and because the direct pin on the selection rules
   answered the refusal from a **stub**, which cannot have an extent at
   all. Fixed by asking the extent of the **face that produced the
   crossing** — the thing that made the claim, and the thing nothing
   elsewhere on the part can move. Still a box, deliberately, because a
   doubly-curved convex face's real material boundary lies *outside* its
   own trim curve (a dome's crown sits 0.4 mm past it) and an on-face
   test would re-break the domes. Three committed fixtures that vary the
   part's extent while holding the bore fixed — a leg, a boss and a rib,
   one per case — plus a four-shape / both-directions sweep bracketing
   the flip at the pole itself, and the refusal re-pinned on a real face
   and a real part instead of a stub. The stub is now barred from
   answering it. Cheaper as well as right: the world box was built for
   **every part mined**, and the face box is built only where a survivor
   has to be judged — twelve times across the whole corpus, none of them
   on an ordinary plate.

   **Round 4 — the bore that does not run down a world axis.** Round 3
   asked the right OBJECT with the wrong RULER. A world-axis-aligned
   bounding box projected onto a **tilted** axis is the face's real
   extent *plus* its lateral extents times the direction's other
   components, and that slack grows with the tilt — so the refusal that
   stops the round-2 over-quote silently stops firing once the part is
   tipped. Measured on the same breakout face: real axial extent ~10.7 mm
   at every tilt, world-box projection 8.0 at 0°, 14.8 at 15°, 18.9 at
   45°. The crossover is between **13° and 14°**, and past it
   `far_opening_through_bore` is back to **24 mm of hole in a 20 mm
   plate**. The probe that finds it is a whole-part **rotation**, and
   under the round-3 rules **four of the 58 committed fixtures change
   their answer** under one — both of round 2's flagship parts straight
   back to the answers round 2 was written to fix. It was invisible
   because every fixture that reaches this refusal has a
   world-axis-parallel bore.
   A second, independent leak in the same question: `BRepBndLib::Add`
   bounds a B-spline by its **poles**, which stand outside the surface,
   so the identical part read **12.0 THROUGH** as an analytic sphere and
   **24.0 BLIND** one `BRepBuilderAPI_NurbsConvert` later — two answers
   for one geometry, chosen by the exporter.
   Fixed by asking the extent of the face's own **surface patch, in the
   BORE's frame, optimally bounded**: the UV rectangle with the trim
   thrown away (so a convex crown hidden under the bore's own exit hole
   is still on it — a tight box of the *trimmed* face shortens
   `bore_through_torus_wall` from 16.0 to 15.8997), transformed so the
   bore's axis is Z, and `AddOptimal` rather than `Add` so a NURBS cap
   measures as its analytic twin. **All 58 pre-existing fixtures are
   bit-identical**, and the corpus is now invariant under **4350 rigid
   motions** — 72 rotations and translations per part — where the
   round-3 rules moved under 10.
   Four new fixtures: a 20°-tilted breakout, the NURBS twin of
   `far_opening_through_bore`, and two committed parts turned bodily.
   Cost is split into ASKED and SPENT, because a bore whose mouth an
   exporter has split into many edges reaches the same face once per
   edge: `nurbs_far_opening_through_bore` asks the extent question 121
   times and the material question 488 times about the same handful of
   answers, which cost 1.5 s before the per-face and per-`t` memos and
   47 ms after. Eleven boxes are built across the whole corpus and the
   worst part spends 19 classifier queries.

   **Round 5 — the CLASS, not the instances.** Round 4 replaced two
   world-frame quantities and its docstrings read as though it had
   closed the question. It had not: two more instances of the same class
   survived, one of them written by round 4 itself, and both were found
   by the same probe with one thing added — **the motions compose**.
   Round 4's table applied one rotation *or* one translation at a time
   and the whole corpus passed it. A rotation puts the bore's axis
   across the world axes and a translation then makes the coordinates
   large *in the directions the rotation opened up*; it is the product
   that breaks things, and a 12 m translation is an ordinary
   assembly-exported STEP coordinate rather than a stress figure.
   - `_cap_axis_intersections` ran `GeomAPI_IntCS` in **world
     coordinates**, and that intersector's absolute error grows with
     them. The countersink's apex root, 5e-16 mm off the axis upright,
     lands **1.13e-04 mm** off it once the part is rotated and moved
     12.3 m — past the 1e-4 mm floor round 4 set to recognise it. So the
     cone stopped reading as a cone and capped the bore at its own apex:
     `countersunk_blind_bore` **7.000209 against a true 11.0** (−36 %,
     entry 4 mm inside solid metal) and `countersunk_through_bore`
     **13.0002 against 17.0**. Round 4's docstring claimed "three and a
     half orders" of margin here; the real figure was **1.10x, on the
     wrong side**, and nothing in the suite computed it. Fixed by
     running the intersection **in the bore's own frame** — the same
     frame `_face_axial_span` already uses — which bounds the error by
     the *part's* size instead of by its distance from the world origin:
     the same worst case measures **5.5e-10 mm**.
   - `_has_flat_bottom` decided whether a planar face reached the bore
     from a **world-axis `Bnd_Box`** of that face inflated by the bore
     radius. Spin a part about the axis of one of its own bores — which
     moves no point of the bore relative to any point of the part — and
     the box turns, grows and shrinks with it. A Ø8 **ball-nose** pocket
     beside a coplanar slot floor reads `flat_bottom` False at 0°, True
     at 30°–60°, False at 75°–90°, **True again at 135°**: a round floor
     priced as a flat-bottom drill on nothing but the export angle, and
     `flat_bottom` is a signal slice C prices off. A box in the *bore's*
     frame does not fix it either — `gp_Ax3(P, Dir)`'s X direction is an
     arbitrary choice of OCCT's — so the lateral question is answered
     where it belongs, as a **distance from the axis point to the
     trimmed face**.

   The **whole class** was then swept: every `Bnd_Box`/`BRepBndLib` use,
   every absolute tolerance compared against intersector output, every
   world-axis projection. Three world-frame quantities remain, all
   deliberate and none able to move a verdict — `_canonical_direction`
   and `_canonical_origin` choose a *frame to report in* (ADR-0112 S3,
   and neither is part of `_rigid_invariants`), and `_perp_basis` turns
   with the part but is only ever read through signed areas and interval
   unions, which a common rotation cannot change. All three are pinned
   rather than argued, and a **structural test fails on any new bounding
   box anywhere in the module**.

   **Round 6 — the point classifier's own frame, and a BOUND instead of
   a claim.** Round 5 swept "every `Bnd_Box`, every intersector
   tolerance, every world-axis projection" and reported the class closed.
   It was not, and the instance it missed was in neither bucket:
   `_AxisMaterial._inside` built its probe point in **world coordinates**
   and handed it to `BRepClass3d_SolidClassifier` with an absolute
   tolerance, to resolve a step that is **intrinsic** — a
   `_tangency_band`, 1.26e-03 mm on a Ø4 bore. A solid classifier's
   usable precision is *relative* to the coordinates it is given, so the
   step shrinks into the kernel's noise as the part moves out and
   `is_exit` ends up comparing two answers that are both noise.

   Measured on the material boundary the probe exists to find, **located
   by bisection rather than assumed**: the world-frame classifier puts it
   3.1e-08 mm off with the part 100 m out, 3.2e-05 mm at 1 km and
   **3.9e-03 mm at 10 km — wider than the step itself**. What that costs:
   `undercut_ball_seat_blind_bore_d8`, a committed fixture and the very
   part D-19 rounds 1 and 2 were written for, reads **7.0999999978
   THROUGH against a true 12.1 BLIND** 10 km out — 41 % of the depth gone
   *and* the end condition lost, which is round 1's own defect restored
   by nothing but an assembly-scale coordinate.

   Fixed by asking the classifier in a frame **anchored on the bore** —
   the solid translated so the middle of the bore's own parametric span
   is the origin. The obvious frame, translating by `-origin`, does
   almost nothing and that is worth writing down: `_canonical_origin`
   puts `origin` at the foot of the perpendicular from the **world**
   origin, which is a point on the axis *line* and not a point on the
   *part*, so a part 17 km out along its own bore direction still gets
   `t = 17 km` in that frame.

   **The two factors this entry used to quote — "`-origin` moves the
   first flip by 1.04x, the bore anchor by 23x" — do not reconcile with
   round 6's own artefacts, and are corrected here.** What the artefacts
   actually measured, and both numbers come from runs that put all three
   candidates through the same method:

   - by BISECTION over the whole motion grid, the first categorical flip
     anywhere sits at **4.22e+06 mm** in the world frame, **5.43e+06 mm**
     under `-origin` (**1.29x**) and **8.35e+06 mm** anchored on the bore
     (**1.98x**);
   - by the LADDER, which is the method the table below is built from and
     the one that does not bisect a non-monotone predicate, the world
     frame's last clean rung is **4.64e+06 mm** and the shipped anchor's
     is **1.47e+07 mm** — **3.16x**.

   The 23x was never reproducible from any artefact in the branch and is
   withdrawn. What the anchor is worth is better said by what it does to
   the boundary itself than by a ratio of ladder rungs: the same material
   boundary that moved 3.9e-03 mm at 10 km moves **2.3e-10 mm**, and
   stays linear in the distance out to 1e+11 mm, which is the floor of
   what a double can represent at those coordinates rather than a
   property of this module. **All 62 committed fixtures were bit-identical
   at 17 significant digits** — the fix changes a *boolean*, and only
   where the boolean was wrong.

   And one thing round 6 could not have known, added by round 7: the
   classifier's frame and the INTERSECTION frame (item 9) compound.
   With item 9 anchored, the world-frame classifier no longer loses
   `undercut_ball_seat_blind_bore_d8`'s floor at 1e+07 mm, or at 1e+08 —
   it loses it at **1e+09**. That does not make round 6's fix
   unnecessary: at 1e+09 the world-frame classifier is still 41 % short
   on a committed fixture, and every rung past it depends on the fix.
   Two conditioning fixes feeding each other is the same effect this
   entry already records between `-origin` and the bore anchor.
   `test_d19r6_an_undercut_seat_ten_kilometres_out_keeps_its_floor`
   carries both magnitudes.

   **THE SWEEP IS BEHAVIOURAL NOW, which is round 6's real finding.**
   Rounds 4 and 5 each scoped their sweep by *mechanism* and each time
   the next instance was a mechanism that was not in the bucket
   (round 4 → round 5's UV box, round 5 → round 6's classifier). So the
   criterion is stated over the **answers**: for every located-hole
   verdict, over every committed fixture (62 at round 6, 63 now), under
   seven rotations crossed
   with three translation directions in both orders, walk the coordinate
   magnitude on a ladder and find the first categorical flip. The
   predicate is **not monotone** in the magnitude — a part can answer
   correctly at 8 km and wrongly at 6 km — so it is a ladder and not a
   bisection.

   The bound it measured, which is what this feature claims and all it
   claims. It is a **ladder's** bound and is stated as one: rungs
   `10 ** (1/6)` apart (1.468x), walked from 1e+04 mm to 1e+09 mm, with
   everything below 1.5e+04 mm already covered by round 5's own sweep.

   **The bound quoted is the LAST RUNG THAT WAS CLEAN, and round 6's
   table quoted the rung the flip was found AT — one rung optimistic in
   every column.** Corrected below. "Nothing moves below X" can only mean
   "every rung up to and including X was clean under every motion", and
   the rung above X is precisely the one where something did move; the
   true first flip sits somewhere in between.

   | quantity | before round 6 | after |
   |---|---|---|
   | nothing DISCRETE (count / end condition / flat bottom) moves below | 4.64e+06 mm | **1.47e+07 mm** |
   | no DEPTH moves by more than a micron below | 3.16e+06 mm | **6.81e+06 mm** |
   | fixtures that flip anywhere on the ladder | 18 | **11** |

   (Round 6's own figures for those two rows were 6.81e+06 / 2.15e+07 and
   4.64e+06 / 1.00e+07 — each one rung high.)

   **RE-WALKED AT ROUND 7**, with the intersection frame anchored (item 9)
   and the depth convention ratified (items 7 and 8), and out to 1e+11 mm
   this time because the first flip is now past the end of round 6's own
   ladder:

   | quantity | round 6 | round 7 |
   |---|---|---|
   | nothing DISCRETE moves at or below | 4.64e+06 mm | **3.16e+09 mm (3162 km)** — 681× |
   | no DEPTH moves by more than a micron at or below | 3.16e+06 mm | **4.64e+08 mm (464 km)** — 147× |
   | fixtures moving anything over round 6's own range (1e+04 → 1e+09) | 11 | **0 discrete, 2 by more than a micron** |

   And the residuals are named rather than hidden behind "closed". The
   DEPTH one is round 7's own mouth walk: at 6.81e+08 mm
   `countersunk_through_bore` reads 16.999999963 against 20.0 — the
   ownership and flare tests stop resolving, the walk declines to step
   across the countersink, and the hole falls back to the top of its own
   wall, which is the pre-round-7 answer. It degrades to the ROUND-6
   reading rather than to nonsense, and that reading is the SHORT one,
   which is the direction to watch. The DISCRETE one is at 4.64e+09 mm,
   on `angled_blind_hole`'s `flat_bottom` flag. Over the whole ladder to
   1e+11 mm, 16 fixtures move something discrete and 23 move a depth by
   more than a micron; below 1e+09 mm, none and two.

   And over the full motion grid at 4 km, nothing discrete moves and the
   worst depth moves 3.7e-04 mm; at 1 km, 1.8e-05 mm; at 100 m,
   2.2e-07 mm. `test_d19r6_no_verdict_moves_below_the_measured_bound`
   keeps the 1 km row standing on every commit; the ladder itself takes a
   quarter of an hour and is recorded here rather than re-walked.

   And what is claimed is that measured **bound**, not closure. See item
   9, which is what the sweep found next — and which is the mechanism
   behind **every** residual flip in the "after" column: all eleven
   return to the upright answer when that frame is anchored on the bore,
   checked one by one rather than inferred from the first few.

   Honest accounting of the cost, and the figures here are round 7's
   because round 6's own were wrong in three places: moving an
   intersection into another frame is arithmetic, so it cannot be
   bit-preserving. **Eight committed hole rows move, by at most 4.6e-13
   mm** — six in `depth_mm` and two in `entry_point_mm`, and **three** of
   the eight land on an exact nominal, which is what a better-conditioned
   frame does. Round 6 recorded "all in `depth_mm`, at most 1.5e-13 mm,
   seven of eight towards nominal"; item 9 carries the corrected table
   row by row. Diameter, axis, end condition and `flat_bottom` are
   unchanged bit for bit on every row. The other honest note: the frame
   is the fix,
   and round 5's second change — making the collapse floor a **fraction
   of the bore's radius** judged against the isoline's real extent
   rather than an absolute 1e-4 mm judged against a derivative — moves
   **no** answer, measured across four decades of bore radius. It is
   kept as hardening and pinned as inert, the posture `_void_slack`
   already sets.
7. ~~**R2-B — toroidal undercut (O-ring / snap-ring gland).**~~
   **— CLOSED, D-19 round 7.** A Ø8 bore with an R4 x 1.5 gland at z=12
   read **6.5 THROUGH** — the top of the bore's own cylindrical wall,
   where the gland has eaten the bottom 1.5 mm of it — on a pocket with
   **12 mm of metal under it**. Short AND misclassified.

   The mechanism was that a **ring torus never crosses its own axis**, so
   that cap face offered no crossing at all, root selection was never
   asked, the end fell back to the bore's parametric bound and the
   touching vote called it air. A different mechanism from items 1 and 6,
   which is why round 2 left it rather than half-fixing it.

   **Both readings the item carried are now answered, and by the
   convention rather than by a rule invented for a torus.** The gland is
   cut radially, by a different tool, in a different operation: its floor
   at z=10.5 is the GROOVING tool's travel and not the drill's. So the
   hole is **8.0 BLIND** — the drill's own reach to the flat floor at
   z=12 — and 9.5 is the feature's depth, which is not the hole's. The
   item's own question, *does a radial relief in the wall lengthen the
   axial drilling depth?*, is answered **no**, by the same sentence that
   answers *does a relief at the mouth shorten it?*

   **Fixed by `_across_a_relief`**, the round-7 walk item 8 shares: a
   face the drill's axis does not END on is a different operation's face,
   so step across it and ask the next one. The gland's torus offers the
   axis nothing, so the walk crosses it — both halves of it, because OCCT
   splits a torus at its own equator — and reaches the pocket's flat
   floor. What it finds there earns the end by the MATERIAL rule: the
   axis genuinely leaves the metal at z=12 and does not at z=10.5, so the
   drill's 8.0 is measured and the gland's 9.5 is never a candidate.

   Committed as `toroidal_gland_blind_bore.step` and pinned field by
   field. Mutation-verified on its own: take only the material arm out of
   the walk and the gland goes back to 6.5 THROUGH while the seven
   mouth-datum rows do not move —
   `test_d19r7_each_ruling_is_reverted_on_its_own_and_only_its_rows_go_red`.

8. ~~**R2-C — depth convention.**~~ **— RATIFIED AND IMPLEMENTED, D-19
   round 7. A hole's depth is the DRILL's own travel, from the part FACE
   to the deepest point the drill reaches; what a DIFFERENT operation
   makes is its own feature and is not folded in.** Ervin's ruling, on
   the conservative ground that *we will not lose jobs over it*: between
   two defensible readings, take the one that never under-quotes. The
   header above is the short statement; ADR-0112 §B.2.1 is the statement
   of record.

   **THE FAR END WAS ALREADY RIGHT AND DID NOT MOVE.** A bore that opens
   into a cavity wider than itself ends at that cavity's **pole**,
   because that is where a tool travelling down the axis stops. The
   corpus has been pinned to the pole since D-19 round 1 and stays:
   **no re-pin, no fixture moves, no code change.** What moved is the
   classification. The family below is **working as intended**, not a set
   of over-quote defects waiting on a fix:

   - `undercut_ball_seat_blind_bore` (**14.1**) and
     `undercut_ball_seat_blind_bore_d8`, and the whole undercut sweep
     between them;
   - the **ball-nose** family, which is the `undercut = 0` member of the
     same sweep;
   - the **wide spherical chamber** — a Ø4 access hole into an R8
     cavity, **33.0**, which the round-3 re-adversarial reported as a
     "+91 % over-quote" against a solid-derived 17.254. It is not an
     over-quote. It is the convention, and the oracle that produced
     17.254 measures a bore to the last place its own **wall** is the
     boundary, which is the reading this repo does not use. By that same
     oracle the committed `undercut_ball_seat_blind_bore` would be 6.9005
     against its pinned 14.1 — a 51 % disagreement on a part nobody has
     ever called defective, which is what showed the two were one
     semantic rather than a defect beside a correct answer;
   - the **domed-floor** pocket family, whose floor is its crown for the
     same reason.

   The evidence that this is the only rule here that measures rather than
   invents: the miner's answer is exactly "to the pole" at **every**
   undercut from a ball nose to a chamber, continuous, with no feature
   anywhere along it to hang a rule on. Every candidate rule that would
   have closed the chamber while keeping the committed fixture was a
   **threshold on the undercut** — a boundary invented to make one part
   agree with an oracle.
   `test_d19r4_the_wide_chamber_is_the_undercut_seat_family_not_a_defect`
   walks that sweep and pins it as the ratified convention, and also pins
   that the chamber does not move under any rigid motion, so it is not an
   orientation defect wearing this one's clothes.

   **AND A BREAKOUT AT THE FAR END STILL DOES NOT LENGTHEN A HOLE.**
   `far_opening_through_bore` is a Ø8 bore into a spherical cavity that
   breaks out of the plate's bottom face; the drill's travel stops at
   z=8, where the axis leaves the metal, and the answer is **12.0** and
   not the plate's 20.0. Unchanged, and it is the same sentence as the
   gland: the cavity is a different operation's feature.

   **THE MOUTH DATUM IS THE PART'S FACE — RULED, AND THE SEVEN ROWS ARE
   RE-PINNED.** Depth used to be measured from the top of the bore's own
   cylindrical **wall**, so a countersink, a chamfer or a spherical dish
   at the mouth shortened the hole by the full depth of the relief. At
   this end the convention's own rationale and the corpus's pinning
   pointed in **opposite** directions — the tool starts cutting at the
   part's face, so tool travel is the LONGER reading — and pricing off
   the old pinning under-quoted by the whole relief, which is the
   direction this module treats as the worse one because it never appears
   in a reasoning log. Round 6 measured the cost over the whole corpus at
   **seven rows across six geometries**, and round 7 re-pinned every one
   of them:

   | fixture | was (bore wall) | now (part face) |
   |---|---|---|
   | `countersunk_blind_bore` | 11.0 | **14.0** |
   | `countersunk_blind_bore_turned` | 11.000000000000137 | **13.999999999997355** |
   | `countersunk_through_bore` | 17.0 | **20.0** |
   | `countersunk_bore_120` | 18.267949192431 | **20.0** |
   | `chamfered_mouth_bore` | 18.5 | **20.0** |
   | `spherical_mouth_undercut_bore` | 9.040417140076999 | **10.632** |
   | `spherical_mouth_undercut_bore_with_a_boss` | 9.040417140076999 | **10.632** |

   Six distinct geometries: `countersunk_blind_bore` and its `_turned`
   twin are one part exported two ways, and the two spherical-mouth rows
   differ only by a boss on the far corner that never touches the bore.
   On the four BLIND rows the `entry_point_mm` moves with the depth —
   (20,20,17) → (20,20,20) and (30,30,18.408417) → (30,30,20) — because
   the drill starts at the face, and both earlier answers put the entry
   somewhere the drill was not.

   Note the 120° countersink. Its old figure, 20 − 3/tan(60°), moved with
   the countersink's **angle**; the ratified one does not, and a 90° and
   a 120° countersink over the same bore in the same plate now measure
   the same drilling. That is asserted rather than observed —
   `test_r3_countersink_angle_does_not_decide_the_end_condition`.

   **How it is implemented, and the one thing the geometry does not
   settle.** `_across_a_relief`: a face the drill's axis does not END on
   is a different operation's face, so step across it and ask the next
   one. A countersink's cone meets the axis only at its apex and a touch
   is not a crossing; a dish's two crossings are one in the bore's own
   hollow and one off the part. Beyond each is the plate's top face.

   What has to be decided there is *which end of the hole this is*, and
   **no question about the shape answers it**: a countersink at a mouth
   and a spherical breakout at a far end are **mirror images** —
   `spherical_mouth_undercut_bore` is a plate with a Ø4 bore and a sphere
   fused into the cutter at the top face, `far_opening_through_bore` is
   the same construction reflected — and they are pinned to different
   readings, 10.632 from the face and 12.0 to the drill's reach. What
   tells them apart is what could have MADE them: a tool travelling down
   the bore's own axis cannot leave a feature wider inside than the
   opening it comes in through. Measured over the corpus, every mouth
   relief is exactly its own rim (0.0 on five rows, 8.9e-16 mm on two)
   and every breakout stands **0.062–0.557 mm** proud of its own. The
   slack that separates them, `SURFACE_CONFUSION_MM = 1e-7`, sits five
   and a half orders below the nearest breakout and eight above the
   widest flare, so any figure across those eight decades separates the
   same two sets. `test_d19r7_the_flare_test_is_measured_not_tuned`
   walks both populations rather than asserting the gap.

   Mutation-verified on its own: make that rule refuse everything and
   exactly the seven rows above go back to their round-6 values, with the
   gland unmoved —
   `test_d19r7_each_ruling_is_reverted_on_its_own_and_only_its_rows_go_red`.

   **Gate.** Slice C may price off `depth_mm` at **both** ends. The mouth
   half of the gate is lifted.

   **How the far end came to be asked (D-19 round 4).** The chamber
   arrived as a reported defect, and the useful part of the history is
   how it stopped being one. Round 3's re-adversarial filed the Ø4/R8
   chamber as a +91 % over-quote against its own solid-derived oracle.
   The part turned out to be *literally*
   `tools/generate_step_fixtures.py::_undercut_ball_seat` with a bigger
   undercut — the same construction as a committed fixture nobody had
   ever questioned — and applying the oracle's rule to that fixture
   condemned it too, by 51 %. One rule, two parts, two verdicts that
   could not both be about geometry: what the pair actually exposed was a
   **semantic** the corpus and the oracle disagreed about, which is why
   round 4 recorded it here instead of fixing it.

9. ~~**The INTERSECTION frame is anchored off the part.**~~ **— CLOSED,
   D-19 round 7.** Found by round 6's behavioural sweep, and it was the
   same root cause as the classifier instance round 6 closed rather than
   a new kind of thing. `_cap_axis_intersections` ran `GeomAPI_IntCS` in
   `gp_Ax3(origin, direction)` — "the bore's frame" — and
   `_canonical_origin` puts `origin` at the **foot of the perpendicular
   from the WORLD origin**, which is a point on the axis *line* and not a
   point on the *part*. Removing the perpendicular offset is all that
   frame did, so a translation **along the bore's own axis** left the
   surface at world magnitude inside it, and `GeomAPI_IntCS` started
   losing roots.

   Round 5 knew about that motion — "the one motion that makes `t` large
   without making the part large" — and tested it at **15 m**, where
   nothing moves. The ladder walks it to 1e+09 mm and it does.

   Measured, on committed fixtures, and it failed in **both** directions:

   | fixture | true | reads | at |
   |---|---|---|---|
   | `far_opening_through_bore` | 12.0 THROUGH | **17.33 BLIND** | 2.15e+08 mm |
   | `far_opening_through_bore_turned` | 12.0 THROUGH | **17.33 BLIND** | 2.15e+08 mm |
   | `far_opening_through_bore_with_a_leg` | 12.0 THROUGH | **17.33 BLIND** | 2.15e+08 mm |
   | `angled_far_opening_through_bore` | 12.64 THROUGH | **17.98 BLIND** | 2.15e+08 mm |
   | `domed_floor_pocket_proud` | 7.0 BLIND | **13.1 THROUGH** | 2.15e+08 mm |
   | `bore_beside_a_conical_boss` | 25.0 THROUGH | **30.0** (+20 %) | 1.47e+08 mm |
   | `bore_beside_a_taller_conical_boss` | 28.75 THROUGH | **35.0** (+21.7 %) | 2.15e+08 mm |
   | `undercut_ball_seat_blind_bore_d8` | 12.1 BLIND | **7.1 THROUGH** (−41 %) | 2.15e+07 mm |
   | `undercut_ball_seat_blind_bore` | 14.1 BLIND | **6.9 THROUGH** (−51 %) | 3.16e+07 mm |
   | `undercut_ball_seat_at_the_confusion_edge` | 16.0 BLIND | **8.0 THROUGH** | 2.15e+08 mm |

   The mechanism, stated as the numbers it turns on:
   `far_opening_through_bore`'s breakout sphere gives the axis two roots
   at 1e+08 mm — t = −1.31 and t = 11.97, the near one inside the bore's
   own hollow — and **one** root at 2.15e+08 mm. With only one root the
   void bound discards nothing, so `_root_for_end`'s material arm never
   runs, `_AxisMaterial` is never built, and the surviving root caps the
   bore in mid-air. Every rule D-19 rounds 1, 2, 3 and 6 added to that
   arm is simply not reached.

   **The fix is one argument, and it is not free.** That `gp_Ax3` is
   anchored on the bore — the same anchor round 6 gave the classifier,
   the middle of the bore's own parametric span — and every row above
   returns to its exact nominal. But moving an intersection into a
   different frame is arithmetic, so **eight of the 63 committed hole
   rows move**, by at most **4.6e-13 mm**. Round 6 costed this as "eight
   fixtures, all in `depth_mm`, at most 1.5e-13 mm, seven of the eight
   towards nominal". Two of those three figures were wrong, and the
   corrected accounting is:

   | fixture | field | was | now |
   |---|---|---|---|
   | `undercut_ball_seat_blind_bore` | depth | 14.100000000000001 | **14.1** |
   | `undercut_ball_seat_blind_bore_d8` | depth | 12.100000000000001 | **12.1** |
   | `cross_drilled_shaft` | entry x | −11.180339887498947 | **−11.180339887498949** |
   | `domed_floor_pocket` | depth | 11.998800000000976 | 11.998800000001431 |
   | `domed_floor_pocket_with_a_rib` | depth | 11.998800000000976 | 11.998800000001431 |
   | `bore_beside_a_conical_boss` | depth | 24.999999999992752 | 24.99999999999274 |
   | `bore_beside_a_taller_conical_boss` | depth | 28.75000000001983 | 28.750000000019842 |
   | `bore_through_nurbs_dome` | entry z | −19.999999999999996 | −19.999999999999993 |

   - **not all in `depth_mm`**: two rows move in `entry_point_mm`
     instead;
   - **at most 4.6e-13 mm**, not 1.5e-13 — a fifth of a picometre either
     way, and twelve orders below the tightest tolerance any of these
     parts could be made to;
   - **three move onto an exact nominal, not seven**: the two undercut
     seats onto 14.1 and 12.1, and the cross-drilled shaft's entry onto
     the exact double of −sqrt(125), which nine decimals cannot express
     and which round 6's "closer to nominal" arithmetic therefore counted
     the wrong way. The other five drift in the last bits of computations
     whose nominal they were not landing on before either — the two
     conical bosses were already 7.2e-12 and 2.0e-11 off 25.0 and 28.75,
     because their caps are cones met at a shallow angle.

   Diameter, axis, end condition and `flat_bottom` are unchanged bit for
   bit on every row.

   **Pinned from both sides** by
   `test_d19r7_the_intersection_frame_is_anchored_and_its_flips_are_closed`:
   the unanchored frame still reads the broken depth on six of the ten
   rows above under one translation (the other four flip under motions a
   single translation does not reproduce), the shipped frame reads the
   exact nominal, and reverting the anchor moves exactly the eight rows
   tabled here and no others. That last assertion is the item's mutation
   proof and its blast-radius measurement at once.

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
Items 3 and 5 are independent; item 4 was the constant change whose
blast radius was said to be the whole fixture corpus and turned out to
move none of it.

Items 7, 8 and 9 closed together in D-19 round 7 and are worth reading as
one change, because they were: 8 was never a fix at all — it is a
convention — and once it was ratified at BOTH ends, 7 and 8 turned out to
be one mechanism (`_across_a_relief`: a face the drill's axis does not end
on is a different operation's face, so step across it and ask the next
one) and 9 was the arithmetic that had to be right for either to be
measured at an assembly-scale coordinate. Round 7 is therefore the first
round on this feature whose bar is **not** bit-identity: fifteen committed
rows move, seven of them because the convention says they should, and each
one is named with its mechanism in
`test_d19r7_every_moved_row_is_named_and_the_rest_are_bit_identical`.

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
