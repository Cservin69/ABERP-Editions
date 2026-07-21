# ADR-0056 — Versioning policy: PATCH / MINOR / MAJOR for the `PROD_vX.Y[.Z]` release branches

**Status:** Accepted — S201 / PR-201 (2026-05-31). Names the rules that
decide which segment bumps on a release, so the version string carries
operator-visible signal about scope. Extends `run/release.sh`'s validator
(2-segment OR 3-segment) and pins the heuristics for choosing between
patch and minor within the 1.x invoicing strand.
**Author:** Ervin Áben (ABERP), session 201 brief — versioning policy
locked 2026-05-31.
**Supersedes / amends:** none — additive policy ADR. The release-branch
shape (`PROD_v<digits>[.digits...]`, branch-not-tag) is unchanged from
ADR-0021's pre-code baseline + S169's release model. This ADR pins the
*meaning* of the segment bumps.
**Related:** ADR-0021 (pre-code consolidated baseline), the S169 release
model (`run/release.sh` + per-release-branch publish), the S200 upgrade
model (`run/upgrade_prod.sh`), the cutover runbook
(`docs/CUTOVER_RUNBOOK.md`), the Stage 2 storefront ground-zero
(`docs/e2e-shop/ground-zero.md`).

## Context

The release model since S169 publishes a `PROD_vX.Y` branch from `main`
per release. The validator regex was originally exactly 2-segment
(`^PROD_v[0-9]+\.[0-9]+$`) — it accepted nothing else. From PROD_v1.0
through PROD_v1.4 (the cutovers up to 2026-05-31) every release bumped
the minor segment, regardless of whether the change was a bugfix or a
feature.

This worked through PROD_v1.4 because the gap between cutovers was small
enough that the operator's mental model could absorb "every bump is a
new minor". But two pressures surfaced:

1. **Bugfix-only releases need a quieter signal.** A release that ships
   nothing more than a one-line fix for a NAV-side regression should not
   read as the same kind of event as a release that ships a SPA
   navigation rework. Both are "real" releases (they reach prod and
   change operator-facing behavior), but the *scope* differs by an
   order of magnitude. A flat minor-bump model collapses that
   distinction.
2. **Stage 2 modules are coming.** `docs/e2e-shop/ground-zero.md` (S199)
   sketches the Friboard backend / e-shop strand; future Stage 2 modules
   (Ordering, Inventory, Cloud sync, etc., per ADR-0021 §"Items deferred
   to build phase") will eventually ship. A new top-level module is a
   strictly different event from "we shipped another invoicing
   refinement". The version string should be able to carry that signal.

The decision to extend `run/release.sh` to accept an optional patch
segment (S201 / PR-201) is the mechanical surface. This ADR is the
policy that says *which segment to bump when*.

## Decision

The release-branch naming convention is `PROD_v<MAJOR>.<MINOR>[.<PATCH>]`,
governed by these rules:

### 1.x — Stage 1 invoicing (current + future polish + future major invoicing-only features)

- **PATCH** (e.g. `PROD_v1.4.1`, `PROD_v1.4.2`): bugfixes and small
  features that do NOT materially change the invoicing experience.
- **MINOR** (e.g. `PROD_v1.5`, `PROD_v1.6`): MAJOR feature changes
  WITHIN invoicing scope, *before any Stage 2 module ships*.
- **MAJOR** (e.g. `PROD_v2.0`): first NEW MODULE from the Stage 2 ERP
  buildout per `docs/e2e-shop/ground-zero.md` and ADR-0021 §"Items
  deferred to build phase". Likely candidate triggers: Ordering module,
  Friboard backend integration, or Inventory module — whichever lands
  first.

### Heuristic — patch vs minor

If the release notes need a "what's new" section longer than **2
bullets**, it is a MINOR. If 2 bullets or fewer cover everything an
operator needs to know to use the new release, it is a PATCH.

This is the operator-facing test: *what does the operator need to read
before they trust the upgrade?* A patch is read in a glance; a minor
warrants a moment.

Examples that would be MINOR (under the 1.x strand):
- SPA navigation rework (multiple new top-level routes).
- "Draft invoices" concept (a new lifecycle state with its own UI flow).
- Real automated payment matching module (a new operator workflow).

Examples that would be PATCH:
- One-line NAV XML fix that closes a v3.0 spec edge case.
- Tightening a closed-vocab parse to reject one new variant.
- A timeline-display tweak that doesn't change backend behavior.
- A minor SPA polish (button label, sort default, etc.).

### What counts as a "module" for the 2.0 trigger

"Module" is defined explicitly to keep the 2.0 trigger objective:

A **module** is a NEW top-level operational concept with its OWN routes,
ITS OWN schema additions (one or more new DuckDB tables OR side-store
directories), and ITS OWN audit kinds (one or more new `EventKind`
variants — F12 ritual fires per ADR-0008). All three properties must
hold.

By that definition:

- The Storno workflow (S156) — extends invoicing, no new module → MINOR
  if grouped, otherwise PATCH-stream.
- The AP module (S177) — new routes (`/api/incoming-invoices/...`), new
  table (`ap_invoice`), new audit kinds (`IncomingInvoice*`,
  `system.IncomingInvoiceSyncCycleCompleted`). All three hold — **would
  have been a 2.0 trigger** had it landed under this policy. Did not
  because the policy did not exist; called out here for completeness.
  Future modules of this shape are 2.0 triggers.
- The NAV-as-DR wizard (S180) — extends an operator-driven recovery
  flow, lives under invoicing scope → PATCH-stream by this rule.

The "all three hold" gate is deliberate. A bare route addition is not a
module; a bare new audit kind is not a module; a side-store directory
with no schema or routes is not a module. The compound test
discriminates *real* new operational surfaces from *extensions* to
existing ones.

## Consequences

### Wins

- **Operator clarity.** A PATCH bump signals "this is safe, the
  upgrade window is short, the smoke-test surface is small". A MINOR
  bump signals "read the release notes, exercise the new flows, expect
  some retraining". A MAJOR bump signals "a new module landed — the
  cutover is its own event".
- **Patch hygiene.** Bugfix-only releases can ship without burning a
  minor-number slot, which preserves the minor-number stream as a
  signal of substantive feature work.
- **Roadmap signal.** The 2.0 trigger is named explicitly. When the
  first Stage 2 module reaches release, the version string carries the
  event; operators (and the maintainer's future self) can reason about
  "what version is invoicing-only" vs "what version has Stage 2 surface"
  at a glance.

### Trade-offs

- **One more thing to remember at release time.** The PR author must
  decide patch-vs-minor before invoking `release.sh`. The 2-bullet
  heuristic is the cheap forcing function (and the script accepts both
  shapes, so a wrong call is reversible — re-publish under the right
  shape).
- **`PROD_v1.4.1` is now valid; old `PROD_v1.0` ... `PROD_v1.4` stay
  valid.** No retroactive renaming. The validator stops *rejecting*
  the 3-segment form; existing branches are not touched. (Pre-policy
  bumps like S177 that would have been 2.0 under this policy stay
  under their as-shipped version.)
- **The "module" definition is opinionated.** A future PR could add a
  surface that arguably qualifies as a module but only meets two of
  the three properties (e.g. routes + audit kinds but no schema). The
  default is "not a module" (favors minor over major). If the call is
  genuinely ambiguous, the maintainer picks the conservative shape and
  documents the call in the release notes — the ADR can be amended via
  a superseding ADR if the rule needs to evolve.

### When to revisit

- The first Stage 2 module reaches release-readiness and the 2.0
  trigger fires. At that point the post-cutover review confirms the
  "all three properties" definition held; if it didn't, this ADR is
  amended.
- A bugfix release accidentally introduces a behavior change that an
  operator notices (a quiet "what's new" turned out to be a "what
  changed"). The 2-bullet heuristic gets tightened — possibly to a
  positive-list test (PATCH only when no operator-facing flow changes)
  rather than a length test.
- The version string runs into a fourth segment requirement (build
  metadata, hotfix sub-patch, etc.). This is not anticipated; if it
  surfaces it is its own ADR.

## Adversarial review

- *"What about pre-release / RC / beta tags?"* Explicitly rejected.
  `PROD_v` is for releases that reach a real prod machine. RC / beta
  testing happens on the dev clone against the NAV test endpoint
  before publishing the release branch. The release-branch ref is
  the cutover marker; there is no "release candidate that is sort of
  a release". `run/release.sh` enforces this (rejects any suffix on
  the version arg).
- *"What if a patch turns out to break something — do we yank the
  branch?"* No. The branch stays; a fix-forward `PROD_vX.Y.(Z+1)` is
  the resolution per the cutover runbook §"Roll back" guidance. The
  3-segment form makes this cheap: the operator can ship a
  `PROD_v1.4.2` immediately without having to debate "is this big
  enough to be `PROD_v1.5`?".
- *"Why not full SemVer (with build metadata, pre-release identifiers,
  etc.)?"* SemVer is designed for libraries consumed by other code.
  ABERP is a binary deployed to one machine by one operator. The
  minimal subset (major.minor.patch with no suffix) carries every
  signal that surface needs. Adding the SemVer extensions would be a
  CLAUDE.md rule 13 violation (don't add what isn't earning its keep).
- *"What if the maintainer disagrees with the 2-bullet heuristic mid-
  release?"* The heuristic is advisory in spirit but enforced by
  habit: the maintainer drafts the release notes BEFORE invoking
  `release.sh`. If the draft is 3+ bullets, the bump is minor. If 2
  or fewer, patch. The discipline is "write the notes first, pick the
  segment second". If a discipline failure surfaces in practice, the
  ADR is amended (possibly with a mechanical gate — e.g. a
  pre-release-branch checklist).
- *"Why was S177 (the AP module) shipped as a minor instead of a
  major?"* Because the policy didn't exist at S177's release time
  (PROD_v1.x cutovers up to PROD_v1.4 all predated this ADR). The
  policy is forward-looking. The first new module *after* this ADR
  lands is the 2.0 trigger; S177's history stays as it shipped.

## Alternatives considered

- **Stay 2-segment forever.** Rejected per §"Context" — collapses the
  bugfix / feature distinction. Operator's mental model loses signal.
- **2-segment + a fourth segment for hotfixes (e.g. `PROD_v1.4-hotfix1`
  or `PROD_v1.4_h1`).** Rejected: introduces a parser fork (the
  release.sh validator gets uglier), and the suffix shape is harder to
  reason about than a third numeric segment. The 3-segment form reuses
  the existing dot-numeric vocabulary.
- **Calendar versioning (`PROD_v2026.05`).** Considered + rejected:
  calendar versioning loses scope signal entirely (every release is
  "the next one"). The whole point of the patch-vs-minor distinction
  is to surface scope, which calendar versioning erases.
- **Defer the policy until the first 2.0 trigger fires.** Rejected:
  the 2.0 question is the *easy* one (we already agree on the
  trigger). The harder question is *patch-vs-minor within 1.x*, and
  that is hitting now (PROD_v1.4.1 is the immediate motivating case).
  Deferring the policy means doing the same decision-making ad-hoc on
  every release, which is what this ADR avoids.

## Invariants pinned

- `run/release.sh`'s `VERSION_RE` regex accepts exactly
  `^PROD_(Defense_|Portable_)?v[0-9]+\.[0-9]+(\.[0-9]+)?$` — an optional
  product-line segment, then 2-segment OR 3-segment digits, no suffixes.
  (**Amended 2026-07-21** — see §"Amendment" below; the original form was
  `^PROD_v[0-9]+\.[0-9]+(\.[0-9]+)?$`.) The validator is the mechanical
  enforcement of the policy shape; the policy ADR is the *meaning* of the
  shape.
- The "module" gate for the 2.0 trigger is the compound test (new
  routes AND new schema/side-store AND new audit kinds). A new surface
  meeting only one or two of those is not a module by this policy.
- The 2-bullet release-notes heuristic is the patch-vs-minor forcing
  function. Release notes are drafted before `release.sh` is invoked.
- Existing release branches (PROD_v1.0 ... PROD_v1.4) are not
  retroactively renamed or reclassified. The policy is forward-looking
  from PROD_v1.4.1 onward.

---

## Amendment — 2026-07-21 — product-line prefixes (`PROD_Defense_*`, `PROD_Portable_*`)

**Status:** Accepted. Amends the `VERSION_RE` invariant above. Raised by
ADR-0100 §4 ("Blocker for S5"), landed in S2 of the Portable saw-off.

### What changed

`run/release.sh`'s validator becomes:

```
^PROD_(Defense_|Portable_)?v[0-9]+\.[0-9]+(\.[0-9]+)?$
```

The digit grammar, the 2-vs-3-segment meaning, the 2-bullet heuristic and the
2.0 module trigger are all **unchanged**. The only addition is an optional,
**closed** product-line segment.

### Why the original regex had to be widened

ADR-0093 sawed the product lines into this repository (`ABERP-Editions`), which
builds exactly two editions. Each has its own installer, and each resolves a
release from `origin/<version>` as a **branch**:

| Line | Installer | Installer `VERSION_RE` |
| --- | --- | --- |
| Defense | `run/upgrade_defense.sh:83` | `^PROD_Defense_v[0-9]+\.[0-9]+(\.[0-9]+)?$` |
| Portable | `run/upgrade_portable.sh:70` | `^PROD_Portable_v[0-9]+\.[0-9]+(\.[0-9]+)?$` |

`release.sh` is the only script that creates those branches — and its validator
accepted neither shape. **This repository therefore had no release path for
either of the two product lines it exists to ship.**

### How the existing `PROD_Defense_v*` releases were actually cut: by hand

This is recorded because it was not previously written down anywhere, and
because it explains several oddities in the ref list. Evidence:

1. `run/release.sh` has **never been modified in this repository** — its last
   touch is `893d5db` (PR-206), inherited from `ABERP.git` before the saw-off.
   Its `VERSION_RE` has never mentioned `Defense_` or `Portable_`, so every
   `PROD_Defense_v*` cut would have died at arg validation.
2. `release.sh` requires `HEAD == main` and pushes `main:refs/heads/<version>`.
   `PROD_Defense_v0.2.12` (`46c9f5f`) is **one commit ahead of `origin/main`** —
   a ref this script structurally cannot produce. It was cut from a working
   branch that was never merged.
3. The sequence has a **gap and a duplicate**: no `PROD_Defense_v0.2.1` branch
   exists at all (though `README.md` still cites it as current stable, pointing
   at `ABERP.git`), and `v0.2.0` and `v0.2.2` are the same commit `cc722fa`.
   `release.sh`'s "release branch already exists on origin" preflight exists
   precisely to prevent the latter; it never ran.
4. No runbook, ADR, or script in the repository documents a Defense cut command.

So Defense's release path was undocumented and equally broken, and is fixed by
the same change rather than left for a later surprise.

### Why a closed alternation rather than a general prefix

`(Defense_|Portable_)?` enumerates the two lines this repository actually
builds. A permissive `[A-Za-z]+_?` would accept `PROD_Portible_v1.0.0` and
publish a branch no installer can consume — the failure would surface on the
operator's machine at upgrade time instead of at `release.sh` arg validation.
Adding a third line is a deliberate one-word edit here, in the two installers,
and in `run/tests/release_version_re_test.sh`.

The bare `PROD_v*` form is retained: the frozen HU prod line still cuts its own
releases from `ABERP.git`, and `release.sh` is shared code.

### What is deliberately NOT changed

- **The dev-workspace sentinel stays.** `release.sh` still refuses to publish
  from anything under `/Documents/Claude/Projects/`. Cutting an editions release
  means cloning outside the dev tree and running it there, exactly as the S169
  model intends. Hand-cutting bypassed this guard along with all the others;
  widening the regex is not a licence to also drop it.
- **The branch is still the release.** No annotated tag is created. Both
  installers resolve `origin/<version>` as a branch (`--heads`), so a tag would
  be invisible to them.
- **Version-number semantics per line.** The Portable line starts at
  `PROD_Portable_v1.0.0` per ADR-0100 §4 (Decision B); `v0.x` belongs
  permanently to the archived `ABERP.git` Portable lineage. Defense continues
  its existing `v0.2.x` sequence.

### Invariant added

- The set of names `run/release.sh` accepts must equal the union of
  `upgrade_defense.sh`'s and `upgrade_portable.sh`'s `VERSION_RE`s plus the bare
  `PROD_v*` prod form; and the two installers' name-spaces must stay disjoint.
  Pinned mechanically by `run/tests/release_version_re_test.sh`, which extracts
  all three regexes from the scripts themselves so the test cannot drift out of
  agreement with the code.
- The "branch already exists" suggester must replay the product-line prefix it
  was given. Suggesting `PROD_v0.2.13` to an operator who typed
  `PROD_Defense_v0.2.12` would name a branch no installer accepts.
