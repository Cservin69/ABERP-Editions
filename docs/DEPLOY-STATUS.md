# Deploy status — what has been CUT, and what is actually RUNNING

**Generated 2026-09-02 against `origin/main` @ `019d368`.**

This file answers one question honestly: *which Defense releases exist, and
which one is on the pilot machine?*

The two halves have very different confidence levels, and this document
keeps them apart on purpose:

| | Confidence |
|---|---|
| **What has been cut** | **Certain.** Enumerated below straight from `origin`. Every row is a ref you can resolve today. |
| **What is deployed** | **UNKNOWN.** See [§3](#3-what-is-deployed--unknown). A git repository cannot observe a machine. |

> **On the "7 undeployed cuts" figure.** An earlier note put a specific
> number on how far the pilot box had fallen behind. That number was not
> supported by anything in this repository — nothing here records what the
> box is running, so no such count can be derived from it. It is
> **withdrawn**, and is not restated anywhere below. The cut history in
> §2 is the real data; the deployment answer has to come from the machine
> itself via the command in §1.

---

## 1. How to find out what a box is actually running

Run this **on the machine**, against its Defense checkout:

```bash
git -C ~/ABERP-Defense branch --show-current
```

That one line is the answer. `upgrade_defense.sh` deploys by
`git checkout -B <version> origin/<version>`, so **the checked-out branch
name IS the deployed version.** The script uses this exact form itself and
documents why: `git branch --show-current` is the only spelling that
returns the unprefixed branch name if a same-named tag ever exists.

Fuller version, if you want the SHA and whether it is even up:

```bash
git -C ~/ABERP-Defense branch --show-current; \
git -C ~/ABERP-Defense rev-parse --short HEAD; \
git -C ~/ABERP-Defense status --porcelain | head; \
pgrep -fl "$HOME/ABERP-Defense/target/release/aberp"
```

Reading it:

- **line 1** — the deployed version (e.g. `PROD_Defense_v0.6.2`). Empty
  output means a detached HEAD: the box is not on a release branch at all,
  and line 2's SHA is the only identity it has.
- **line 2** — the SHA. Cross-check it against the table in §2. A SHA that
  appears in no row means the box is on something that was never cut.
- **line 3** — any output here means a **dirty tree**: the running code is
  not the cut code, and the version name on line 1 is no longer a truthful
  description of what is deployed.
- **line 4** — the running binaries. No output means the app is down (or
  was launched from a different checkout).

Substitute the real checkout path if it is not `~/ABERP-Defense` — the
README's clone step names that directory, but an older box may use `~/ABERP`.

---

## 2. What has been cut — the complete Defense history

24 release branches, oldest first. **All 23 Defense cuts and the single
Portable cut are ancestors of `main`** — every one is contained in mainline
history, so there is no cut carrying work that main has lost.

Dates are the **head commit's** date, not the date the branch was pushed.
Releases here were cut by hand, sometimes days after the commit was
authored, so read the date as *"no earlier than"*.

| # | Release | SHA | Head commit | Δ commits | Head-commit subject |
|---|---|---|---|---|---|
| 1 | `PROD_Defense_v0.1.0` | `8f3a5cb` | 2026-06-25 | — | add inert gears/gear_cost to FeatureGraph/QuoteBreakdown literals |
| 2 | `PROD_Defense_v0.2.0` | `cc722fa` | 2026-06-28 | +20 | ADR-0095 editions durability hardening + lopdf 0.42 (RUSTSEC-2026-0187) |
| 3 | `PROD_Defense_v0.2.2` | `cc722fa` | 2026-06-28 | **+0** | ⚠ **identical SHA to v0.2.0** — see §4 |
| 4 | `PROD_Defense_v0.2.3` | `ed8de48` | 2026-06-29 | +22 | ADR-0097 tolerance validation golden + 2-arm CI build-proof |
| 5 | `PROD_Defense_v0.2.4` | `9c35ebb` | 2026-06-29 | +2 | wire customer storefront tolerance into Defense quote pricing |
| 6 | `PROD_Defense_v0.2.5` | `c662e39` | 2026-07-04 | +69 | raise cut-gate.yml timeout 5 → 15 |
| 7 | `PROD_Defense_v0.2.6` | `5fe151e` | 2026-07-05 | +2 | ADR-0098 R7 rustfmt the new regression tests |
| 8 | `PROD_Defense_v0.2.7` | `1a56872` | 2026-07-06 | +3 | ADR-0098 R7 drop redundant u64 cast |
| 9 | `PROD_Defense_v0.2.8` | `1e6097d` | 2026-07-07 | +4 | ADR-0099 tighten CHECK 10i frozen residual-opener counts |
| 10 | `PROD_Defense_v0.2.9` | `97bb3d2` | 2026-07-09 | +2 | clippy fixes for rust 1.97 lint drift |
| 11 | `PROD_Defense_v0.2.10` | `b5c8f5f` | 2026-07-10 | +2 | UI border-radius token sweep (68 surfaces) |
| 12 | `PROD_Defense_v0.2.11` | `7520ed2` | 2026-07-10 | +2 | brand: the real Á mark |
| 13 | `PROD_Defense_v0.2.12` | `46c9f5f` | 2026-07-11 | +1 | hermetic serve-boot mirror-ahead auto-heal e2e |
| 14 | `PROD_Defense_v0.3.0` | `691bfc9` | 2026-08-04 | +42 | printed-PDF VAT exemption reference (PR #29) |
| 15 | `PROD_Defense_v0.4.0` | `a4b107b` | 2026-08-10 | +45 | ADR-0097 tolerance cost-rate seed, PR #38 adversarial close |
| 16 | `PROD_Defense_v0.4.1` | `7153092` | 2026-08-10 | +5 | brand the operator banner "ABERP-Defense" |
| 17 | `PROD_Defense_v0.4.2` | `72c7106` | 2026-08-14 | +7 | financial stats AR/DSO (PR #42) |
| 18 | `PROD_Defense_v0.5.0` | `9e4a6ee` | 2026-08-20 | +25 | scope ADR-0112 to the Defense edition |
| 19 | `PROD_Defense_v0.6.0` | `6182c6e` | 2026-08-23 | +6 | pricing-queue reaper measures a WINDOW, not a since-boot count |
| 20 | `PROD_Defense_v0.6.1` | `79ed238` | 2026-08-25 | +6 | raise the cut-gate cap 30 → 75 |
| 21 | `PROD_Defense_v0.6.2` | `bae151d` | 2026-08-26 | +10 | ADR-0199 QC round 8 — stale probe / premature waiver |
| 22 | `PROD_Defense_v0.6.3` | `5020773` | 2026-08-27 | +3 | ADR-0114 / D-22 money-CLI durability |
| 23 | `PROD_Defense_v0.6.4` | `5bd846e` | 2026-08-30 | +21 | ADR-0116 snapshot system; stop double-billing the probe harness |

**Portable** — one cut, in this same repository:

| Release | SHA | Head commit | Head-commit subject |
|---|---|---|---|
| `PROD_Portable_v1.0.0` | `234b598` | 2026-07-21 | cut-gate probe portability (PR #19) |

**Not yet cut:** `main` is **12 commits ahead of `PROD_Defense_v0.6.4`** as
of `019d368`. That tail is the D-19 located-holes geometry work, the
cut-gate probe-harness sharding, and the customer-demo walkthrough. No
release branch points at it.

---

## 3. What is deployed — UNKNOWN

**This repository does not know, and cannot know, which version the pilot
machine is running.** Nothing in git records a deployment. There is no
deploy log, no callback, no heartbeat, no version file written by the
installer, and no `PROD_Defense` tag whose absence or presence would imply
anything.

So the honest status is:

| Machine | Deployed version | Basis |
|---|---|---|
| Defense pilot box | **UNKNOWN — requires confirmation from the machine** | — |

To resolve it, run the §1 command on the box and record the answer here.
Until then, treat any statement of the form *"the pilot is N versions
behind"* as unsupported, including in customer conversations.

Two things worth knowing before reading the answer:

- **A version name is not proof of what is running.** If the tree is dirty
  (line 3 of the fuller command) or the binary was built from a different
  checkout, the branch name overstates what is deployed.
- **Upgrades are one-way across a DuckDB storage bump.** Take a snapshot
  before moving a box forward — `upgrade_defense.sh` forces one for
  Defense and does not allow the skip that Portable does.

---

## 4. Anomalies in the cut history

These are real and worth knowing before anyone reasons from version numbers.

- **`PROD_Defense_v0.2.1` does not exist.** Not as a branch, not as a tag,
  not in this repository and not in the pre-split `Cservin69/ABERP`. The
  sequence goes `v0.2.0` → `v0.2.2`. Until 2026-09-02 the README named
  `v0.2.1` in **10 places** — as the current stable release, and in clone,
  reset, upgrade and `ls-remote` commands operators were told to run.
  Every one of those instructions was unrunnable. Fixed in the same change
  that added this file.

- **`v0.2.0` and `v0.2.2` are the same commit** (`cc722fa`, +0 commits
  between them). Two release names, one artifact. Most likely v0.2.1 was
  skipped or withdrawn and v0.2.2 re-cut the same tree.

- **Release refs are branches, not tags.** `git tag -l` lists **no**
  `PROD_Defense_*` or `PROD_Portable_*` tag at all. The only tags in this
  repository are three `archive/aberp-git/PROD_Portable_v0.1.*` archive
  tags and one reconcile backup. `upgrade_defense.sh` resolves
  `origin/<version>` as a **branch**; anything reaching for `refs/tags/…`
  finds nothing.

- **The legacy line lives elsewhere.** `Cservin69/ABERP` holds the
  pre-split `PROD_v1.x`–`PROD_v2.35.0` history (as branches *and* tags) and
  the `run_prod.sh` / `upgrade_prod.sh` launchers. It contains **zero**
  `PROD_Defense_*` and **zero** `PROD_Portable_*` refs — both edition lines
  are cut here, in `ABERP-Editions`.

---

## 5. Keeping this file honest

Regenerate §2 with:

```bash
for b in $(git branch -r | grep -E 'origin/PROD_(Defense|Portable)_v' | sed 's/ *//' | sort -V); do
  printf '%-24s %s  %s  %s\n' "${b#origin/}" \
    "$(git rev-parse --short=7 "$b")" \
    "$(git log -1 --format=%cd --date=short "$b")" \
    "$(git log -1 --format=%s "$b")"
done
```

§3 cannot be regenerated from the repository. It changes only when someone
runs the §1 command on a real machine and writes the answer down.
