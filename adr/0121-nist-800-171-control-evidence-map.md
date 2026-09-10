# ADR-0121 — NIST SP 800-171 control tagging: a static `EventKind`→control evidence map + a coverage report (D-04)

- **Status:** **Accepted** (2026-09-10) — passed adversarial review (two
  sweeps; the observed-vs-satisfied over-claim, the assessment-window gap, and
  the coverage-inflation risk designed out, six properties confirmed). Safe to
  build, in the sub-slices below.
- **Date:** 2026-09-10
- **Deciders:** Design pass + adversarial review, 2026-09-10.
- **Related:** `crates/aberp-compliance/src/nist_800_171/mod.rs` (the 110
  control constants + `ALL_CONTROLS`), `crates/audit-ledger/src/entry/event_kind.rs`
  (`EventKind`, `EventKind::ALL_KINDS`, `ALL_KINDS_COUNT`), ADR-0094 (the
  no-new-`EventKind` blast-radius clause), D-01 / D-03 (the "a mock/derived
  claim must never overstate what actually happened" discipline on an
  append-only ledger), `[[trust-code-not-operator]]`.

## Context

`crates/aberp-compliance/src/nist_800_171/mod.rs` defines all 110 NIST SP
800-171 Rev. 2 security requirements as `&'static str` constants
(`"<dotted-id>: <short title>"`) with an `ALL_CONTROLS: [&str; 110]` array.
**Nothing in the repo consumes them.** The module doc already states the intent:
"a future audit `EventKind` that contributes evidence toward a specific control
references the corresponding constant, so a System Security Plan / assessment
can trace ledger events back to the control they satisfy."

D-04 is that consumer. The deliverable an assessor asks for is a **coverage
map**: for each of the 110 controls, which ledger events (if any) evidence it,
and whether such events have actually occurred. ABERP already emits ~191
`EventKind`s to a tamper-evident, append-only ledger (`EventKind::ALL_KINDS`,
pinned by `ALL_KINDS_COUNT`); many of them are exactly the kind of activity a
control is about (a unique-user-traceable action evidences AU 3.3.2; an access
decision evidences AC 3.1.x; an identity registration evidences IA 3.5.1).

**Why this needs a design pass, not just wiring.** The one real decision is
*where a control tag lives*, and the wrong choice is unforgiving:

- The mapping "kind K evidences control C" is **analyst / SSP judgment that will
  be revised** as the assessment matures and as new kinds are added. It is not a
  fact about any single event.
- The ledger is **append-only and hash-pinned** (each payload's bytes are
  covered by the chain hash). Anything written into a payload is permanent and
  uncorrectable.

So putting a `nist_controls: [...]` field on each event payload — the obvious
"tag the event" reading — would bake a revisable analyst opinion into
uncorrectable bytes, at ~191 firing sites, forever. That is the same
anti-pattern D-01 ("the mock must keep answering `not_determined`; making it
answer `granted` writes an uncorrectable claim") and D-03 (derive the IUID on
read, never persist the enterprise id) were careful to avoid. The mapping must
live somewhere revisable, and the evidence must be **derived**, not stamped.

## Decision

Tag at the **kind level, in code, read-side only.** A static
`EventKind → controls` **evidence map** (in `apps/aberp`, see the home-crate
note below), plus a coverage report that reads the existing ledger. **No per-event payload field, no new
`EventKind`, no schema change, no new firing site.**

### The evidence map (where the tag lives)

A static table in **`apps/aberp/src/nist_coverage.rs`** keyed on the `EventKind`
**enum variant** (not its string).

**Home-crate note (build-time correction).** The map references *both* the
`nist_800_171` control constants (in `aberp-compliance`) *and* `EventKind` (in
`aberp-audit-ledger`). `aberp-compliance` is a deliberately lean leaf crate
(serde-derive only, no runtime deps) that does **not** depend on
`aberp-audit-ledger`, and must not — dragging the ledger crate (and duckdb) into
it to key on `EventKind` would wreck that posture. So the map lives in
`apps/aberp`, which already depends on both — the same home the D-08/D-09/D-10
feature modules (`cui_marking.rs`, `cyber_incident.rs`, `dpas_rating.rs`) use.
The control *constants* stay in `aberp-compliance`; only the *mapping* (which
needs `EventKind`) is aberp-side. This keeps the compile-time enum key intact
(the point below), which a string key would have sacrificed.

```rust
/// One asserted evidentiary link: emitting `kind` contributes evidence toward
/// satisfying `control`. `rationale` states WHY, in one line, so a reviewer or
/// assessor can challenge the link — a map entry is an analyst claim, and the
/// claim must be legible.
pub struct EvidenceLink {
    pub kind: EventKind,
    pub control: &'static str,   // an ALL_CONTROLS member, by constant
    pub rationale: &'static str,
}

/// The complete, reviewed set of links. Keyed on the compile-time enum, so a
/// renamed or removed `EventKind` is a COMPILE error, and referenced controls
/// are the `nist_800_171` constants, so a typo'd id does not compile either.
pub fn evidence_links() -> &'static [EvidenceLink];
```

Three properties fall out of keying on the enum + the constants:

- **A renamed/removed `EventKind` breaks the build** — the map cannot silently
  point at a kind that no longer exists (contrast a string map).
- **A typo'd control id breaks the build** — links reference the `AC_3_1_1`-style
  constants, never a bare string.
- **The mapping's provenance is git** — changing "which kind evidences which
  control" is a reviewed code change with history, which is exactly the
  audit trail an SSP mapping should itself have. It is deliberately NOT
  operator-editable config (an operator editing the evidence mapping is a
  foot-gun; the mapping is developer/analyst knowledge).

Derived helpers: `controls_for(kind) -> &[&'static str]` and the inverse
`kinds_for(control) -> &[EventKind]`.

### The coverage report (what is derived, and what it must NOT claim)

`coverage(observed: &ObservedKinds, window: Option<TimeWindow>) -> CoverageReport`
folds the set of kinds actually present in the ledger against the map. For each
of the **110** controls it reports one of three honest states:

- **Evidenced** — the control is mapped to ≥1 kind AND ≥1 such event is present
  (in `window`, if given). Carries the evidencing kinds + observed counts + each
  link's rationale.
- **Mapped, not yet exercised** — the control is mapped, but no such event has
  occurred (a designed evidence path with no activity behind it).
- **No automated evidence in ABERP** — the control has no mapped kind at all.
  Many of the 110 are organizational / physical / policy controls (AT training,
  PE physical protection, most of PS) that no software event can evidence; the
  report says so plainly rather than hiding them.

**The load-bearing honesty rule (this domain's "do not regress").** The report
states **"evidence present in the ledger,"** never **"control satisfied"** or
**"compliant."** Presence of a mapped event is necessary, not sufficient — an
assessor decides satisfaction. The types enforce the gap: there is an
`EvidenceState` (the three cases above), and there is **no** `Satisfied` /
`Compliant` value anywhere in the report. ABERP reports what its ledger shows;
it never grades itself. This is the AC-side analogue of D-01's rule that the
mock must not answer `granted`.

**Conservative mapping.** Coverage is inflated the instant a kind is mapped to a
control it does not truly evidence, and no test can judge that semantic claim —
so the guard is procedural: every link carries a `rationale`, the map is
reviewed like code, and the mapping is deliberately **under-claiming** — a kind
is linked only where its emission is direct, defensible evidence of the control's
activity, not a loose thematic association. When in doubt, leave it unmapped
(the report then shows the control honestly as unevidenced) rather than assert a
link that inflates the coverage number an assessor is trusting.

### What the report reads

The observed-kinds set comes from the ledger the report is run against
(per-tenant — a coverage report is scoped to one tenant's chain, no
cross-tenant contamination). Slice 2 adds a cheap `SELECT kind, COUNT(*), MIN/MAX(ts)
GROUP BY kind` reader (bounded, not a full `entries()` materialization) so the
report needs only the distinct kinds + counts + first/last timestamps, not every
row.

## Consequences

The 110 constants finally have a consumer, and ABERP can render the
ledger-event-to-control trace an SP 800-171 assessment asks for — as a derived,
revisable, git-versioned mapping over the events it already emits, with **zero**
new event kinds, payload fields, firing sites, or schema. The mapping can be
corrected or extended by a normal reviewed code change without ever touching a
historical ledger row. The report is honest by construction about the large
share of controls no software event can evidence.

**Costs / what is locked in.** The `EventKind→control` analysis is real work and
lands incrementally (Open Q1) — slice 1 ships the high-confidence subset and the
machinery, later passes grow the map. The report is only ever as complete as the
mapping, and it deliberately refuses to output a compliance verdict, so it is an
evidence aid, not a compliance attestation — which is the correct and honest
scope.

## Adversarial review

Two sweeps against the draft (2026-09-10). Real defects found and designed out,
plus confirmations:

1. **FOUND — the observed⇒satisfied over-claim.** A draft that renders a mapped-
   and-observed control as "covered/compliant" would assert compliance from mere
   event presence — the exact uncorrectable over-claim D-01 forbids for the
   screening mock. FIXED: the report's `EvidenceState` has no `Satisfied`/
   `Compliant` value; it says "evidence present," and satisfaction is the
   assessor's call. Encoded in the type, not just the copy.
2. **FOUND — the assessment-window gap.** "Has this control ever been evidenced"
   lets a two-year-old event mark a control evidenced for a current assessment
   period. FIXED: `coverage` takes an optional `TimeWindow`; evidence counts
   only within it, and the report states the window it used (default all-time,
   but stated).
3. **FOUND — coverage inflation by a loose link.** Mapping a kind to a control it
   only thematically touches inflates the number an assessor trusts, and no test
   can catch a semantic mis-link. FIXED (procedural): every `EvidenceLink`
   carries a `rationale`; the map is reviewed; the policy is to under-claim and
   leave doubtful controls unmapped rather than assert a weak link.
4. **CONFIRMED — no hash-pinning risk.** Nothing is written to any payload; the
   map is code and the evidence is derived on read. No new `EventKind`
   (ADR-0094), no schema change. Historical rows are never touched.
5. **CONFIRMED — drift-safe.** Links key on the `EventKind` enum (a rename/remove
   is a compile error) and reference the control constants (a typo is a compile
   error). `EventKind::ALL_KINDS` + `ALL_KINDS_COUNT` already trip a test when a
   kind is added, so a new kind cannot silently escape the evidence analysis —
   and an observed-but-unmapped kind is surfaced by the report, not dropped.
6. **CONFIRMED — per-tenant scope.** The report reads one tenant's ledger; no
   cross-tenant evidence bleed.
7. **CONFIRMED — union semantics are sound.** A control mapped to several kinds
   is evidenced if ANY is observed; a kind mapped to several controls evidences
   each. Coverage is boolean per control (counts are informational), so there is
   no double-count.
8. **CONFIRMED — the map's own provenance.** Because the mapping is code, every
   change to "what evidences what" carries a git author + review + history — the
   mapping is itself auditable, which an operator-editable table would not be.

## Alternatives considered

- **A `nist_controls: [..]` field on each event payload, stamped at the firing
  site.** Rejected: it bakes a revisable analyst mapping into append-only,
  hash-pinned bytes at ~191 sites — uncorrectable, high blast radius, and the
  D-01/D-03 anti-pattern. The mapping must be revisable; payload bytes are not.
- **A DB mapping table seeded at boot.** Rejected: it makes the evidence mapping
  operator-editable (a compliance foot-gun), gives it no git provenance, and
  cannot key on the compile-time `EventKind` enum, so it loses the rename/typo
  compile-time safety. The SSP mapping is developer/analyst knowledge, not
  tenant data.
- **A new `EventKind` per control (or a `control.evidenced` kind).** Rejected per
  ADR-0094 (a new variant in a ~191-variant cross-crate enum the sandbox cannot
  compile-verify), and wrong on the merits: a control is not an event, and
  emitting a "control evidenced" event would itself be the unfounded compliance
  claim this ADR exists to avoid.
- **Render a compliance score / percentage-compliant.** Rejected: presence of
  evidence is not satisfaction; a percentage invites reading the tool as an
  attestation. The report shows coverage of *evidence*, explicitly not
  compliance.

## Open questions

- **Q1 — the mapping content.** Which of the ~191 kinds evidence which of the 110
  controls is incremental analyst work. Slice 1 ships the machinery + a
  high-confidence starter set (the AU audit-family controls the ledger plainly
  evidences — 3.3.1 create/retain logs, 3.3.2 uniquely trace to user; the AC
  access-decision kinds; the IA identity kinds; the CM change-logging kinds).
  Later passes grow it. The map is designed to grow by reviewed code change.
- **Q2 — a cut-gate CHECK forbidding control ids in payloads.** A static check
  that no audit-payload struct references a `nist_800_171` constant would harden
  the "map is the only home" rule against a future regression. Deferred (the ADR
  prohibition + review cover it for now); note it if a payload ever grows a
  control field.
- **Q3 — the SPA coverage surface.** A Compliance-area screen rendering the
  110-row coverage matrix (evidenced / mapped-unexercised / out-of-system, with
  drill-down to the evidencing kinds + counts). Additive UI, a later slice.

## Build slices

- **Slice 1 — the map + the fold (library-only, inert).** `EvidenceLink`,
  `evidence_links()`, `controls_for` / `kinds_for`, the `EvidenceState` +
  `CoverageReport` types, and `coverage(observed, window)`. Plus the
  high-confidence starter mapping (Q1). Tests: every link's control ∈
  `ALL_CONTROLS`; no duplicate `(kind, control)` pair; `coverage` over a known
  observed set yields the expected three-way partition; the report exposes no
  compliance/satisfied verdict; an observed-but-unmapped kind is surfaced.
- **Slice 2 — the ledger reader + report renderer.** The bounded
  `kind, COUNT, MIN/MAX(ts) GROUP BY kind` reader (per tenant), and a renderer
  (a `aberp nist-coverage` CLI emitting JSON + a table, and/or a serve route)
  that feeds observed kinds into `coverage` and prints the 110-control matrix.
  Reads existing ledger rows only.
- **Slice 3 (optional) — the SPA coverage matrix** (Q3), out of scope for the
  first cut.
