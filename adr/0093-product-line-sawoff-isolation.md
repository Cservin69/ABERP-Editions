# ADR-0093 — Product-line saw-off: Portable + Defense isolated from the frozen Prod tree

- **Status:** Accepted (auto-mode, Ervin pre-authorized 2026-06-23)
- **Date:** 2026-06-23
- **Deciders:** Ervin
- **Grounds:** FOUNDATION.md §2 (cornerstone: database-per-tenant) + §5
  (multi-tenancy; process bound to one tenant, no in-process switching),
  ADR-0002 (tenant isolation: database-per-tenant), ADR-0056 (release-branch
  versioning model: `PROD_vX.Y` branch-from-`main`), ADR-0082 (validated
  DuckDB snapshot system + the 2026-06-11 prod ART-corruption incident),
  ADR-0007 (build provenance / supply chain).

## Context

ABERP ships three lines off **one** git tree (`Cservin69/ABERP`, trunk
`main`): the **frozen** unified `PROD_v2.27.76` (HU production, real NAV,
real money, live-invoicing, `~/.aberp/prod/aberp.duckdb`), and the two
active editions cut from `main` — **Portable** (`PROD_Portable_v0.1.2`,
dev-profile, NAV-off, demo tenant) and **Defense** (`PROD_Defense_v0.2.1`,
`--features production` + the defense/aerospace compliance stack).

Verified topology (2026-06-23): all three release branches are points on a
single linear `main` history with **no commits `main` lacks**; prod
`v2.27.76` (`f7519b4`, tree `2d61281`) is a strict **ancestor** of both
active editions. The editions are therefore **not divergent codebases** —
they are the same trunk built two ways (compile-time `production` feature +
launcher + runtime tenant/NAV). Differentiation today lives in
`build_profile.rs` (`IS_PRODUCTION_BUILD`, `expected_tenant_identity()` →
`("prod","24904362-2-41")`), `serve.rs:243` (`guard_tenant_matches_build`,
literal `tenant=="prod"`), `tenant_registry.rs:673` (`tenant_db_path` =
`~/.aberp/<slug>/aberp.duckdb`), and the `run/run_*.sh` launchers.

Two problems follow. (1) **The binding violates ADR-0002 at the product
line.** A `--features production` Defense build is hard-locked to literally
`tenant=="prod"`, so Defense resolves onto prod's *exact* DB file
(`~/.aberp/prod/aberp.duckdb`) — the opposite of database-per-tenant. (2)
**Shared code means a shared blast radius.** The recurring ART/checkpoint
corruption (ADR-0082; the 2026-06-11 incident cost ~5h of hand-surgery on
the live prod DB) will need a crash-safe-checkpoint fix to the DuckDB
write/checkpoint path in the shared `crates/aberp-snapshot` + `apps/aberp`.
On one shared tree, **that fix necessarily edits the same code prod runs** —
it cannot be made "for the editions only". In-place conditional isolation
does not solve this: it still shares the files.

Ervin's constraint is absolute: **prod's tree (`v2.27.76`), code, DB
(`~/.aberp/prod/aberp.duckdb`), and runtime must stay frozen and
byte-for-byte untouched, forever.** The future crash-safe-checkpoint fix
must land **only** in the sawed-off tree.

## Decision

**Saw the active editions off the shared trunk into their OWN, physically
separate repository, leaving prod frozen in place.** Concretely:

1. **One combined Portable+Defense tree, not two.** They are the same source
   today; two repos would immediately duplicate `serve.rs` (1.3 MB) + 18
   workspace crates for a single maintainer and double every security/NAV
   patch. The isolation that matters — and that Ervin requires — is **from
   prod**, which a single separate repo fully achieves. Portable vs Defense
   stays a build-flavor split (feature + launcher + edition-locked DB root)
   inside the one tree, exactly as today.
   - *Flagged split-trigger:* if Defense ever needs a private / access-
     controlled repo (ITAR/EAR/CUI segregation), split Portable out then —
     cheap (fork the editions repo again). Not warranted now.

2. **Separate repository, not a second top-level directory.** Only a
   physically separate repo guarantees a future change (the checkpoint fix)
   *cannot* touch prod's tree. A directory in the same repo still shares
   history + objects.

3. **Fork point = `main` tip (`2bd2adf`),** the superset trunk carrying the
   latest of both editions. Prod `v2.27.76` (`f7519b4`) rides along as an
   immutable ancestor and is never modified.

4. **Fork WITH history, independent object store.** Preserves provenance
   (ADR-0007 build-provenance / ADR-0008 audit culture; defense compliance
   needs blame) and keeps the launchers' Frankenstein-build git-ancestry
   refusal working. A clean-baseline (squashed) fork is refused — it destroys
   provenance and the ancestry checks.

5. **Own DB root + own write path per edition.** Defense →
   `~/.aberp-defense/<tenant>/aberp.duckdb`; Portable →
   `~/.aberp-portable/<tenant>/aberp.duckdb` — sibling roots **provably
   disjoint** from `~/.aberp/prod/`. The editions tree carries its OWN copy
   of `crates/aberp-snapshot` + the DuckDB write/checkpoint path; the
   crash-safe-checkpoint fix lands there and **only** there. Snapshot stores
   likewise edition-scoped. This is database-per-tenant (ADR-0002 / FOUNDATION
   §2) taken to its strongest form: separate binaries, from a separate repo,
   on separate roots.

6. **Prod is frozen in place.** The original repo stays at `v2.27.76`; no
   new prod release exists or will (ADR-0056 line retired in README). Prod
   operators' clone/upgrade workflow is unchanged. Defense starts on a
   **fresh** `~/.aberp-defense/` store — it never inherits, copies, or reads
   prod's billing data.

### Build-locked binding (the engineering, landing chunk 2)

Replace the literal `tenant=="prod"` gate with a compile-time **edition**
identity (Prod | Defense | Portable). A build derives its tenant namespace
and DB root from its own edition at compile time — *not* from an env var or
launcher string (FOUNDATION §5: path derived, not user-supplied) — and
**physically refuses** to open another edition's root. The editions binary
literally cannot open `~/.aberp/prod/…`. This also reuses ADR-0082's existing
`ensure_restore_allowed` precedent (already refuses writes under `~/.aberp/`).

### NOT in scope / explicitly deferred

The crash-safe-checkpoint (ART-corruption) fix itself (ADR-0082 follow-up) —
designed and implemented later, in the editions tree only. Prod never
receives it. Cloud/Postgres-per-tenant (ADR-0002 future) unchanged.

## Consequences

- **Prod is untouchable by construction.** Future edition work — including
  the checkpoint fix — lives in a different repo on different on-disk roots;
  it cannot reach prod's tree or `~/.aberp/prod/`. Verified after every step
  via prod's immutable tree-hash (`2d61281`).
- **Divergence is the accepted cost.** A fix to the editions' copy of
  `audit-ledger`/`nav-transport` will not flow to frozen prod (and vice
  versa). Prod is frozen by decision, so it takes no new fixes anyway; this
  is the deliberate trade for absolute prod safety.
- **Strongest ADR-0002 posture.** DB-per-edition-per-tenant via physically
  separate binaries; the original Defense→prod-DB violation is removed.
- **Cut-gate prevents silent drift** (`tools/cut_gate_db_isolation.sh` + CI).

## Adversarial review

- *"One tree re-couples Portable and Defense."* They are already one tree;
  this ADR does not add coupling, it removes coupling **to prod**, which is
  the requirement. Splitting the two editions is a later, cheap move if a
  real driver (ITAR access-control) appears.
- *"Duplicating security-critical crates (audit-ledger, nav-transport) is a
  patch-surface risk."* True. Mitigated by prod being frozen (no parallel
  maintenance) and a single editions line forward. If a third live consumer
  ever appears, extract a published crate then — not speculatively (CLAUDE.md
  #2/#13).
- *"Fork-with-history copies prod's history into the editions repo."* It
  copies it as **immutable ancestry**, never as a writable surface; prod's
  commit/tree is never altered. Provenance kept; prod untouched.
- *"How is prod proven untouched?"* Prod's branch SHA (`f7519b4`) and tree
  hash (`2d61281`) are recorded; every step re-checks them, and `~/.aberp/`
  is out of reach of the build environment. Reproducible builds (FOUNDATION
  §10) mean a rebuild of `v2.27.76` is byte-identical.

## Alternatives considered

- **In-place conditional isolation (the original brief).** Refused by Ervin:
  a shared checkpoint fix would still edit prod's files. Does not meet the
  "never touch prod" bar.
- **Two separate repos (Portable, Defense).** Refused now: duplicates an
  identical tree for one maintainer; isolation-from-prod (the actual goal) is
  already met by one separate repo. Kept as a future option behind the
  ITAR trigger.
- **Separate top-level dir in the same repo.** Refused: shares history +
  objects; a fix could still touch prod's tree.
- **Clean-baseline fork (squash history).** Refused: destroys provenance
  (ADR-0007/0008) and breaks the launcher ancestry checks.

## Saw-off roadmap (chunked, gated, prod-verified each step)

1. **Stand up the sawed-off tree** (this ADR; prod proven untouched). ✅
2. **Build-locked edition binding** — compile-time Edition; Defense/Portable
   resolve OWN roots, physically refuse prod; tests (own-DB, can't-cross,
   prod-resolves-unchanged, fresh-start). Tighten cut-gate CHECK 3 → ENFORCED.
3. **Own write/checkpoint path** — edition-scoped `aberp-snapshot` + DuckDB
   write path; extend `ensure_restore_allowed` to refuse `~/.aberp/prod`.
4. **Cut-gate / CI hardening** — full ADR-0002 DB-isolation enforcement.
5. **Publish** — create the GitHub repo(s), push (auth-gated; stop on PAT
   failure), confirm the original repo frozen at `v2.27.76`.
