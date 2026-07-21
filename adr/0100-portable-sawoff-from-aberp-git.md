# ADR-0100 — Portable saw-off from `ABERP.git`

- Status: Proposed
- Date: 2026-07-21
- Corrects: ADR-0093 §6
- Depends on: ADR-0093 (product-line saw-off), ADR-0099

## 1. Context

ADR-0093 moved the Portable and Defense product lines into this repository
(`ABERP-Editions`), leaving `ABERP.git` as the Hungarian production line. The
move was never completed on the `ABERP.git` side: three Portable release
branches, three annotated Portable tags, and three Portable-only source files
are still live there, and `run/upgrade_prod.sh` there still accepts
`PROD_Portable_*` version strings.

The residue is not cosmetic. `run/run_portable.sh:51` in `ABERP.git` reads:

```sh
readonly PORTABLE_HOME="${HOME}/.aberp/${PORTABLE_TENANT}"
```

That is **prod's data root**. A Portable launch out of `ABERP.git` writes into
the live HU production store. The corresponding line in this repository is
`${HOME}/.aberp-portable/${PORTABLE_TENANT}`, backed by a compile-time
mechanism that cannot be talked out of it. Closing this gap is the point of
the saw-off; everything below is the sequencing.

This ADR settles two questions that stages S3 and S5 both block on
(§3, §4), records the surface map and capability gap (§5, §6), fixes the
staged plan (§7), corrects a false statement in ADR-0093 (§8), and names the
process failure that let the drift persist (§9).

**§3 and §4 are decisions taken in this ADR. They were not inherited from
ADR-0093, from any `ABERP.git` session, or from prior saw-off work — no prior
artifact settles either question.**

## 2. Ref reachability in `ABERP.git`

Verified against `ABERP.git` (read-only; nothing was mutated):

| Ref | Type | Object | Ancestor of `origin/main`? |
| --- | --- | --- | --- |
| `refs/remotes/origin/PROD_Portable_v0.1.0` | commit | `7b849f7` | **yes** |
| `refs/remotes/origin/PROD_Portable_v0.1.1` | commit | `9dbecb7` | **yes** |
| `refs/remotes/origin/PROD_Portable_v0.1.2` | commit | `6a51d4f` | **yes** |
| `refs/tags/PROD_Portable_v0.1.0` | tag | `07d3159` | (annotated tag object) |
| `refs/tags/PROD_Portable_v0.1.1` | tag | `059b498` | (annotated tag object) |
| `refs/tags/PROD_Portable_v0.1.2` | tag | `e4de7dc` | (annotated tag object) |

All three branch tips are ancestors of `origin/main` (`git merge-base
--is-ancestor` → 0 for each). **Deleting the six Portable refs orphans no
commit and no tree.** Every line of Portable history remains reachable from
`main` regardless of what happens to these refs.

The only genuinely GC-eligible objects are the three annotated tag objects
`07d3159`, `059b498`, `e4de7dc` — ~150 bytes of tagger metadata each, no
content. They are the entire preservation problem, and §3 preserves them
byte-identically.

## 3. Decision A — the mirrored refs land under `refs/tags/archive/aberp-git/*`

**Decision.** Mirror each of the three annotated tag objects into this
repository under

```
refs/tags/archive/aberp-git/PROD_Portable_v0.1.0
refs/tags/archive/aberp-git/PROD_Portable_v0.1.1
refs/tags/archive/aberp-git/PROD_Portable_v0.1.2
```

and mirror no branches at all.

### Why not `refs/archive/portable/*`

`refs/archive/*` was the obvious candidate and it **fails the preservation
requirement**. Probed empirically on a throwaway pair of repositories: a ref
pushed to `refs/archive/portable/tags/PROD_Portable_v0.1.0`, followed by a
plain `git clone`, yields a clone containing `refs/remotes/origin/main` and
**nothing else** — `git tag -l` is empty, the archive ref is absent, the tag
object is not present. Git's default clone refspec is
`+refs/heads/*:refs/remotes/origin/*` plus tag auto-follow over `refs/tags/*`;
anything outside those two hierarchies is simply not transferred. A namespace
that survives only in the origin's object store, invisible to every clone, is
not preservation — it is a single-copy artifact one server-side accident from
gone.

### Why `refs/tags/archive/aberp-git/*` satisfies both constraints

*It survives.* Probed on the same throwaway pair: pushed to
`refs/tags/archive/aberp-git/PROD_Portable_v0.1.0`, a plain `git clone` lands
the ref, `git cat-file -t` reports `tag`, the tag object SHA is **identical**
to the source (`a1ca9c8…` in the probe — a ref rename never rewrites the
object), and `…^{commit}` dereferences to the tagged commit. Tag hierarchy =
clone hierarchy; every future clone carries the archive.

*It is not installable.* `run/upgrade_portable.sh` gates on two independent
checks, and the archive name fails both:

1. `:126` — `[[ ! "$version" =~ $VERSION_RE ]]` with
   `VERSION_RE='^PROD_Portable_v[0-9]+\.[0-9]+(\.[0-9]+)?$'`. The anchored
   pattern rejects `archive/aberp-git/PROD_Portable_v0.1.2` outright; the
   script dies before touching the network.
2. `:205` — `git ls-remote --exit-code --heads origin "$version"`. Probed:
   exit status **2** both for the bare name `PROD_Portable_v0.1.0` and for
   the archive path, because neither exists under `refs/heads/*`. `--heads`
   never sees `refs/tags/*`. Even if someone widened the regex, the script
   dies here with "release branch does not exist on origin".

*It cannot collide.* The `archive/aberp-git/` prefix is outside the shape any
Editions release script produces, so a future Editions Portable tag can never
land on an archived name, and no operator typing a real release name can
reach the archived code by accident.

**Deliberately rejected: mirroring under the bare names `refs/tags/PROD_Portable_v0.1.x`.**
It would satisfy both mechanical constraints (tags survive clone; `--heads`
does not see tags). It is rejected because it leaves ABERP prod-line Portable
code checkoutable in this repository under a name that reads exactly like an
Editions release, and it burns the `v0.1.x` names in the Editions tag
namespace. The prefix costs nothing and removes both hazards.

**And absolutely not as branches under their existing names.** Because
`upgrade_portable.sh:205/:273` resolve a release from `origin/<version>` — a
**branch**, not a tag — mirroring the tips as branches under their current
names would make `./run/upgrade_portable.sh PROD_Portable_v0.1.2` install
`ABERP.git`'s prod-line Portable code, including
`PORTABLE_HOME="${HOME}/.aberp/…"`, out of this repository. That is the exact
coupling the saw-off exists to sever, re-created by the act of severing it.

## 4. Decision B — the first Editions Portable release is `PROD_Portable_v1.0.0`

**Decision.** The first Portable release cut from this repository is
`PROD_Portable_v1.0.0`. The Editions Portable line starts at `v1.x`; `v0.x`
belongs permanently to the archived `ABERP.git` lineage and is never reissued
here.

Reasoning:

- **No collision, no continuation.** Any `v0.1.3` / `v0.2.0` choice reads as
  the next entry in ABERP's lineage — the reading is wrong (different data
  root, different edition mechanism, different repository) and nothing in the
  version string says so. A major-version step is the one signal in semver
  that means "this is not compatible with what came before", which is
  literally true here: `~/.aberp/` → `~/.aberp-portable/`.
- **The break is real, not bookkeeping.** ABERP Portable v0.1.2 and Editions
  Portable resolve *different data roots*. An operator upgrading across that
  boundary does not find their data. A major bump is the honest encoding.
- **Room for the archive.** The whole `v0.x` space stays free of Editions
  releases, so the archived tags and any later forensic reference to them are
  unambiguous forever.

**Blocker for S5 (not resolved by this ADR, recorded so it is not
discovered late):** `run/release.sh:72` in this repository is
`VERSION_RE='^PROD_v[0-9]+\.[0-9]+(\.[0-9]+)?$'` — it accepts neither
`PROD_Portable_*` nor `PROD_Defense_*`. This repository has **no script that
can cut a Portable release at all**, while `upgrade_portable.sh:70` and
`upgrade_defense.sh:83` both expect release branches only `release.sh` can
produce. Widening `release.sh` is S5 work and is out of scope here.

## 5. Portable surface map of `ABERP.git`

The complete Portable line in `ABERP.git` is `7b849f7^..6a51d4f` — seven
paths, 889 insertions, 4 deletions.

| Path | Δ | Class | Disposition |
| --- | --- | --- | --- |
| `run/run_portable.sh` | +199 | **Portable-only** | delete |
| `run/upgrade_portable.sh` | +379 | **Portable-only** | delete |
| `apps/aberp/tests/portable_demo_boot_e2e.rs` | +297 | **Portable-only** | delete |
| `apps/aberp/src/serve.rs` | +8 −4 | **shared** | keep verbatim |
| `run/upgrade_prod.sh` | +10 −4 | **shared, live prod launcher** | narrow — ambiguity (a) |
| `run/dev-test.sh` | mode bit only | unrelated | no-op |
| `run/tests/upgrade_prod_running_check_test.sh` | mode bit only | unrelated | no-op |

Notes on the two shared files:

- **`serve.rs`** — the only change is `fn build_router` → `pub fn
  build_router`, plus its doc comment. The visibility widening is consumed by
  the Portable `/health` smoke, but it is not Portable-specific and reverting
  it would be an unrelated API change to a live prod file. **Keep verbatim.**
- **`upgrade_prod.sh`** — the change widened `VERSION_RE` from
  `^PROD_v[0-9]+\.[0-9]+(\.[0-9]+)?$` to
  `^PROD_(Defense_|Portable_)?v[0-9]+\.[0-9]+(\.[0-9]+)?$`, so that
  `ABERP.git`'s prod launcher would install Portable releases. Narrowing it
  back is the only change in the whole saw-off that touches a **live prod
  launcher** — see ambiguity (a) in §7.

### Correction to a prior finding: `ABERP_DB` is **not** vestigial

S1 recorded that `ABERP_DB` "is exported by `run_portable.sh` but the binary
never reads it — one occurrence in the whole tree, a doc comment, no
`env::var` anywhere." **That is wrong**, in both repositories, and the
correction strengthens rather than weakens the case:

- `apps/aberp-ui/src/lib.rs:762` (ABERP) / `:781` (Editions):
  `std::env::var("ABERP_DB")` in `boot_backend`, defaulting to
  `./aberp.duckdb`, then handed to `backend::spawn(&aberp_bin, &tenant,
  &db_path, …)`.
- `run/run_portable.sh:73` exports it and `:198–199` launches
  `cargo run --bin aberp-ui` with it set. So the exported value reaches the
  spawned `aberp serve` process. The path is live end to end.

`ABERP_DB` is therefore a **real, operator-reachable input to the data root**
— which is precisely why `guard_tenant_matches_build` (`serve.rs:790`) and
the foreign-root refusal (`serve.rs:282/292/1076`, `tenant_registry.rs:691`)
have to exist and have to be compile-time. Those guards are load-bearing
against an actual attack surface, not defence against a dead variable. The
Editions guard message names the vector explicitly: *"Point `--db` /
`ABERP_DB` at this edition's `~/<dirname>/<tenant>/aberp.duckdb`"*.

### Editions Portable is two compile-time mechanisms deep (confirmed, and deeper than recorded)

`apps/aberp/src/build_profile.rs`:

- `EDITION_DATA_DIRNAME: &str = data_dirname(EDITION)` (`:178`) — a `const`
  derived from the compile-time `EDITION`, `.aberp-defense` or
  `.aberp-portable`, never `.aberp`.
- `foreign_data_dirnames() -> [&'static str; 2]` (`:214`) — for a Portable
  build, `[".aberp", ".aberp-defense"]`; every root this binary must refuse.
- **Third mechanism, not previously recorded:** `const _: () =
  assert!(!matches!(EDITION, Edition::Prod));` (`:154`) — a compile-time
  assertion that fails the **build** if anyone ever wires `EDITION` to
  `Edition::Prod`. ADR-0093's "prod is untouchable by construction" is
  enforced by the compiler, not by review.

The data root of an Editions build cannot be handed `.aberp` by env, config,
or launcher. `ABERP_DB` can *carry* such a path in; the guard *refuses* it.
`ABERP.git`'s Portable has no equivalent — its launcher simply hardcodes
`~/.aberp/`.

## 6. Capability gap: `ABERP.git` `PROD_Portable_v0.1.2` vs Editions Portable

All three Portable-only files exist in both repositories. Diffing
`ABERP.git@6a51d4f` against this repository's working tree:

| File | Diff | Substance |
| --- | --- | --- |
| `run/run_portable.sh` | 4 lines | `PORTABLE_HOME`: `~/.aberp/$TENANT` → `~/.aberp-portable/$TENANT`. **This one line is the entire saw-off.** |
| `run/upgrade_portable.sh` | 36 lines | `tenant="demo"` → `tenant="${ABERP_TENANT:-demo}"`, plus edition-scoped paths |
| `apps/aberp/tests/portable_demo_boot_e2e.rs` | 14 lines | ADR-0093 posture pins (`is_production_build:false`, NAV-off demo → `Ready`) |

**The gap runs one way.** Editions Portable is a strict superset: same three
files, same behaviour, plus the compile-time edition binding, plus the
foreign-root refusal, plus the `Edition::Prod` build assertion. `ABERP.git`'s
`PROD_Portable_v0.1.2` has **no** capability Editions Portable lacks.

Deleting the Portable surface from `ABERP.git` therefore costs zero
capability. There is nothing to port forward — only refs to archive and files
to remove.

## 7. Staged plan S2 → S5

Restore is one line per stage. File deletion in git is inherently reversible;
no rollback ceremony beyond the line shown.

**S2 — archive the tag objects into Editions.** Push the three annotated tag
objects to `refs/tags/archive/aberp-git/PROD_Portable_v0.1.{0,1,2}` in
`ABERP-Editions`. No branches. Nothing in `ABERP.git` changes.
*Restore:* `git push origin --delete refs/tags/archive/aberp-git/PROD_Portable_v0.1.{0,1,2}`

**S3 — prune the six Portable refs in `ABERP.git`.** Delete the three
`PROD_Portable_v0.1.x` branches and the three same-named tags. Per §2 this
orphans no commit and no tree; per S2 the tag objects already exist
elsewhere. Depends on Decision A being executed first.
*Restore:* `git push origin 7b849f7:refs/heads/PROD_Portable_v0.1.0 9dbecb7:refs/heads/PROD_Portable_v0.1.1 6a51d4f:refs/heads/PROD_Portable_v0.1.2` (tags likewise from the archived objects)

**S4 — excise the Portable surface from `ABERP.git`'s tree, plus ambiguity (a).**
Two commits, in this order, never combined:

- *S4a — deletions only.* `git rm run/run_portable.sh run/upgrade_portable.sh
  apps/aberp/tests/portable_demo_boot_e2e.rs`. `serve.rs` untouched.
  *Restore:* `git revert <S4a>`
- *S4b — ambiguity (a), alone.* Narrow `run/upgrade_prod.sh:107` back from
  `^PROD_(Defense_|Portable_)?v…$` toward `^PROD_v…$`, and the two `die`
  message examples with it. **This is the only change in the entire saw-off
  that touches a live prod launcher.** It lands **alone, behind a dry-run
  against a real `PROD_v2.32.1` argument**, and never rides along with a
  deletion commit. Open question it must first answer: whether `Defense_`
  comes out too, or only `Portable_` — `ABERP.git` has no Defense line
  either, but that is a separate line's saw-off and this ADR does not decide
  it.
  *Restore:* `git revert <S4b>`

**S5 — stand up an Editions Portable release path and cut `PROD_Portable_v1.0.0`.**
Widen `run/release.sh:72` (§4 blocker) so it can cut `PROD_Portable_*`
branches/tags the way `upgrade_portable.sh:70` already expects, then cut the
first release at the version decided in §4.
*Restore:* `git push origin --delete PROD_Portable_v1.0.0` (branch and tag)

## 8. Correction to ADR-0093 §6

ADR-0093 §6 states:

> **Prod is frozen in place.** The original repo stays at `v2.27.76`; no new
> prod release exists or will (ADR-0056 line retired in README).

**This is false and has been false for some time.** `ABERP.git` has shipped
six prod releases past `PROD_v2.27.76`:

`PROD_v2.28.0` → `PROD_v2.29.0` → `PROD_v2.30.0` → `PROD_v2.31.0` →
`PROD_v2.32.0` → `PROD_v2.32.1`

`PROD_v2.32.0`'s own commit message describes it as a "linear descendant of
PROD_v2.31.0 … SAFE TO CUT", carrying ADR-0101 and ADR-0102 — active
feature work, not maintenance.

ADR-0093 §6 should read: *the prod line stays in `ABERP.git` and continues to
release on its own cadence; the editions never inherit, copy, or read its
store.* The **isolation** claim in §6 is intact and unaffected — Defense and
Portable still start on fresh, compile-time-bound roots. Only the **frozen**
claim is wrong.

That matters here directly: §7 S4b edits `run/upgrade_prod.sh`, which is a
*live, actively used* prod launcher — not, as ADR-0093 §6 implies, a script
nobody will ever run again.

## 9. Root cause: why this drifted

ADR-0093 was filed **only in `ABERP-Editions`**. `ABERP.git` never received a
copy, a pointer, or a stub.

Sessions working in `ABERP.git` therefore had no statement of intent to read.
They inferred intent from repository state — and the state said Portable
lives here: three release branches, three tags, a working launcher, a green
e2e test, and a `VERSION_RE` in the prod launcher explicitly widened to
accept `PROD_Portable_*`. Every one of those is affirmative evidence that
Portable belongs in `ABERP.git`. Acting on it was correct given what was
visible; the decision that said otherwise was in a repository those sessions
had no reason to open.

The same mechanism produced §8: ADR-0093 §6 asserted prod was frozen, and six
prod releases shipped past it without anything forcing a re-read. A decision
recorded in one repository about another repository's contents decays silently
in both directions.

Mitigation, once S4 lands: leave a stub ADR in `ABERP.git` naming the
saw-off, pointing at ADR-0093 and this ADR, and stating that Portable and
Defense do not live there. A file in the tree is the only artifact an
`ABERP.git` session reliably sees.

## 10. Scope note

This ADR is a document-only change. No refs were created, moved, or deleted;
no code was excised; nothing was pushed to `ABERP.git`. All findings above
were obtained by read-only inspection of `ABERP.git` plus probes against
throwaway scratch repositories.

**Numbering.** `0100` is the next free number in *this* repository (Editions
runs to `0099`). It collides across repositories: `ABERP.git` independently
holds `0100` (SaaS migration, re-sequenced), `0101`, and `0102`. The two ADR
sequences forked at `0093` and have been diverging since — itself an instance
of §9. Renumbering is not attempted here; cite this ADR as
`ABERP-Editions ADR-0100` when the repository is not obvious from context.
