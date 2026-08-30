# ABERP — Editions tree (Portable + Defense), sawed off from frozen Prod

This repository is the **sawed-off active-editions tree** for ABERP
**Portable** and **Defense**, separated from the frozen unified **Prod**
line per **ADR-0093**. All future Portable/Defense work — including the
deferred crash-safe-checkpoint (ART-corruption) fix — lands **here**, never
in prod.

## ⛔ CARDINAL, INVIOLABLE RULE

**DO NOT TOUCH ABERP PROD.** Its tree, code, DB (`~/.aberp/prod/aberp.duckdb`),
or runtime — nothing, no exceptions, ever. Prod stays frozen at
`PROD_v2.27.76` and byte-for-byte untouched. Verify prod untouched after
every step.

## Prod baseline (immutable reference — used to PROVE "untouched")

| Ref | Value |
|---|---|
| Prod branch | `PROD_v2.27.76` |
| Prod commit SHA | `f7519b4077fa9af4f3c7949e58aa29f4268ff9e9` |
| Prod **tree-hash** (content identity) | `2d612811dd487a50f33476c484d1768cc8e99a51` |
| Source `main` fork point | `2bd2adff51737e3eb9729dbc325db0a16bf238e4` |

Prove prod untouched in the **original** repo (read-only) at any time:

```bash
git -C <original-ABERP> rev-parse 'origin/PROD_v2.27.76^{tree}'
# MUST equal 2d612811dd487a50f33476c484d1768cc8e99a51
```

## Decisions (ADR-0093)

- **One combined Portable+Defense tree** (not two) — same source today;
  isolation that matters is *from prod*. Split Defense out later only if
  ITAR/EAR/CUI access-control demands it.
- **Separate repository** (not a sub-dir) — only physical repo separation
  guarantees a future fix can't touch prod.
- **Fork point** `main` `2bd2adf`; **fork-with-history**, independent objects
  (provenance preserved; prod rides along as immutable ancestor).
- **Own DB root + own write path per edition:** Defense →
  `~/.aberp-defense/<tenant>/`, Portable → `~/.aberp-portable/<tenant>/`
  (disjoint from `~/.aberp/prod/`). The checkpoint fix lands only here.

## Saw-off roadmap (chunked · gated · prod-verified each step)

1. **Stand up the sawed-off tree** — remove prod launch surface, ADR-0093,
   cut-gate + CI, this doc. Prod proven untouched. ✅
2. **Build-locked edition binding** — compile-time `Edition` (build_profile.rs);
   Defense/Portable resolve their OWN `~/.aberp-<edition>/` roots and physically
   refuse prod's + the sibling's DB (`tenant_registry::ensure_db_path_isolated`,
   reused at the `serve` boot guard); tests (own-DB, can't-cross,
   no-path-resolves-`~/.aberp/prod`, fresh-start) in
   `apps/aberp/tests/edition_db_isolation.rs` + `build_profile`/`tenant_registry`
   units; `run_defense.sh`/`run_portable.sh` (+ upgraders) repointed to the
   sibling roots; cut-gate CHECK 3 flipped to **ENFORCED**
   (`ENFORCE_EDITION_DB_BINDING=1`, hardened 4 ways). ✅
3. **Own write/checkpoint path** — edition-scoped `crates/aberp-snapshot`
   (snapshots go to `~/Documents/ABERP-snapshots-<edition>/`, never prod's
   bare store) + the deferred **crash-safe durable-checkpoint** fix (ADR-0082)
   as a dedicated `crash_safe` module (atomic build-aside + `rename(2)` +
   `fsync` of file *and* parent dir, reusing DuckDB's own corruption-free
   logical `EXPORT`/`IMPORT`) wired into the snapshot crate + a clean-shutdown
   checkpoint; `ensure_not_prod_path` refuses any prod path on every
   snapshot/restore; and **reconcile safety** — boot no longer silently
   truncates a mirror AHEAD of the DB (it preserves the ahead mirror + refuses
   so a lost-commit isn't erased). Cut-gate CHECK 4 added (ENFORCED). ✅ **(this chunk)**
4. **Cut-gate / CI hardening** — full ADR-0002 DB-isolation enforcement. ✅ **(this chunk)**
5. **Publish** — create GitHub repo(s), push (auth-gated; stop on PAT
   failure), confirm original repo frozen at `v2.27.76`.
   🔸 **PREPARED — awaiting Ervin's authenticated run** (2026-06-23):
   publish + branch-protection commands authored and statically validated
   against the canonical bundle (`efb55f0`); the saved PAT is flagged
   DEAD/COMPROMISED and was **not** used; the authenticated push runs on
   Ervin's Mac under his own `gh`/git auth. See "Chunk 5 — publish status".
6. **Storefront-reach isolation (S2)** — the quote-intake / pricing pipeline's
   reach to the customer storefront (`abenerp.com`: poll for uploaded CAD, push
   the catalogue, write back priced PDFs / status) is now a COMPILE-TIME
   **Defense-only** capability, mirroring the DB binding. `build_profile`
   carries the predicate `storefront_polling_allowed[_for]` (only
   `Edition::Defense`) + the runtime backstop `assert_storefront_reach_allowed`;
   `serve.rs` has `guard_storefront_reach_matches_edition` (FATAL + `exit(1)`,
   the mirror of `guard_db_matches_edition`) wired into BOTH the resolved- and
   malformed-`[quote_intake]`-config arms, so a Portable build handed
   `[quote_intake]` config or `ABERP_QUOTE_INTAKE_*` env **refuses at boot**
   rather than poll. Every storefront daemon spawn (quote-intake, catalogue-
   push, pricing-pipeline, email-outbox-poll, pdf-rerender) and every on-demand
   storefront handler (test-connection ×2, config-PUT, REFUSE-saga writeback)
   sits behind the gate; the local quote engine + manual quoting stay available
   in BOTH editions. Cut-gate CHECK 8 added (ENFORCED, `ENFORCE_STOREFRONT_GATE=1`)
   with four negative probes; both-arms decision proof in
   `tools/storefront_gate_decision_probe.rs` (`rustc --test`, Portable⇒refuse /
   Defense⇒allow). Full both-arm `cargo test` is **CI-deferred** (serve.rs is
   DuckDB/HTTP-backed; repo CI build-proves both arms). ✅ **(this chunk)**

## Gating note

`cargo`/`rustc` were unavailable in the chunk-1 environment, so the full
`cargo build + clippy + cargo test --workspace` gate (incl. the new
prod-unchanged / isolation / can't-cross tests) runs in a follow-on session
that has the Rust+Tauri toolchain (mirror `.github/workflows/ci.yml`). Chunk 1
is gated by what is verifiable without a compiler: the cut-gate, `bash -n`
on all launchers, CI-yaml validity, a structural audit, and the prod
tree-hash proof.

**Chunk 2 gating note (2026-06-23).** The Rust toolchain (rustup stable 1.96)
was installed and used this chunk. **Verified green in-session:** `cargo fmt
--check` over every edited source + test file; the **ENFORCED** DB-isolation
cut-gate (`tools/cut_gate_db_isolation.sh`, including two negative tests that
confirm it now *fails* on a planted prod-DB launcher line and on a removed
compile-time assert); `bash -n` on all four launchers; and a standalone
`rustc --test` of the FAITHFUL core logic (the compile-time `Edition` binding,
the `assert!(!matches!(EDITION, Edition::Prod))` proof, and
`ensure_db_path_isolated`) for BOTH the Portable and Defense arms — 10/10 —
plus `clippy-driver` clean on that extract. **Deferred to Ervin's Mac
(honest, NOT green here):** the full `cargo build/clippy/test --workspace` and
the `apps/aberp-ui` Tauri build. `duckdb` is pinned `features=["bundled"]`, so
the build compiles DuckDB's amalgamation as one ~8-minute C++ translation
unit; this sandbox kills background work at ~45-second call boundaries and has
no swap (4 GB), so that single unit cannot finish in-environment, and
`aberp-ui` additionally needs the webkit2gtk/Tauri system libraries. Run on
the Mac (mirror `.github/workflows/ci.yml`):
`cargo build --workspace && cargo clippy --workspace --all-targets -- -D warnings
&& cargo test --workspace` plus the same three with `--features production`
(the Defense arm) — this exercises `apps/aberp/tests/edition_db_isolation.rs`
and the updated `serve_tenant_feature_guard.rs`. The code is complete and
committed; do not treat the deferred build as green until that run is clean.

**Chunk 3 gating note (2026-06-23).** Chunk 3 lands the edition-scoped
snapshot/restore + write path, the crash-safe durable-checkpoint fix
(ADR-0082), and reconcile safety. The Rust toolchain (rustup 1.96.0 +
rustfmt + clippy) was installed and used. **Verified green in-session:**
`rustfmt --check` over all 12 edited source + test files; the **ENFORCED**
DB-isolation cut-gate `tools/cut_gate_db_isolation.sh` (now CHECK 1–4) PASS,
plus two fresh negative probes confirming the new **CHECK 4** *fails* when a
silent-truncate path (`RecoveryAction::Truncated`) is re-introduced and when
the binary store resolver falls back to prod's `default_store_dir`; `bash -n`
on the cut-gate; and standalone `rustc --test` of the FAITHFUL core logic for
the durability + reconcile + edition-isolation changes — **11/11** (5
crash-safe COMMIT tests: atomic rename + WAL-clear + verified-good marker
currency + the crash-before-rename-leaves-old-good-DB property; 3 reconcile
tests: ahead → preserve+refuse leaving the mirror intact, behind → extend,
equal → unchanged; 3 edition path-isolation tests across BOTH the Defense and
Portable arms: store edition-scoped + disjoint from prod, `ensure_not_prod_path`
refuses prod DB root + prod store while allowing edition roots/stores) — plus
`clippy-driver -D warnings` clean on all three extracts. **Deferred to Ervin's
Mac (honest, NOT green here):** the full bundled-DuckDB build/clippy/test and
the Tauri UI — the same constraints as chunk 2 (bundled DuckDB is one ~8-min
C++ unit; the sandbox kills work at ~45 s boundaries, 4 GB no-swap; `aberp-ui`
needs webkit2gtk). In particular the DuckDB-backed crash-injection integration
tests CANNOT run here. Run on the Mac (mirror `.github/workflows/ci.yml`),
for BOTH edition arms (default + `--features production`):

```
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
# the new chunk-3 tests specifically (bundled DuckDB + binary wiring):
cargo test -p aberp-snapshot --test crash_safe_checkpoint_tests   # durable_checkpoint round-trip / refuse-corrupt / repeatable
cargo test -p aberp-snapshot --test edition_isolation_tests        # edition_store_dir disjoint + ensure_not_prod_path
cargo test -p aberp-audit-ledger ensure_consistent_refuses_and_preserves_when_mirror_ahead_of_db
cargo test -p aberp --test edition_snapshot_isolation              # resolve_store edition-scoped + refuses prod --store
# then repeat the three --workspace commands with --features production (Defense arm)
```

The code is complete and committed; do not treat the deferred bundled-DuckDB
build/tests as green until that run is clean on the Mac.

**Chunk 4 gating note (2026-06-23).** Chunk 4 is the cut-gate / CI hardening
step — it makes chunks 1–3's deferred build/test/isolation gate run *for real*
on a full-toolchain runner, and extends the bash cut-gate with three new
invariants (each with a negative probe). **No application code changed.**
**Verified green in-session (sandbox-runnable):** `bash -n` on
`tools/cut_gate_db_isolation.sh` and the new `tools/cut_gate_negative_probes.sh`;
the cut-gate (now CHECK 1–7) PASSES on the clean tree; the negative-probe
harness is **8/8** (clean copy passes; every CHECK 1–7 goes red when its
invariant is violated); both `.github/workflows/*.yml` parse
(`yaml.safe_load`) and every `run:` block passes `bash -n` for BOTH arms
(empty and `--features production` substitution). **NOT run here (honest —
authored + statically validated only):** the GitHub Actions run itself — the
full `cargo build/clippy/test --workspace` (both arms), the bundled-DuckDB
crash-injection + edition-isolation integration tests, and the `aberp-ui`
Tauri link — same constraints as chunks 2–3 (bundled DuckDB is one ~8-min C++
unit; the sandbox kills work at ~45 s boundaries, 4 GB no-swap; `aberp-ui`
needs webkit2gtk). The CI is therefore **authored + statically validated; its
first real run happens on GitHub Actions / Ervin's Mac.** Do not call the CI
"passing" until that first runner job is green.

> **Prod-untouched provenance note (2026-06-23).** The content anchor is the
> **tree-hash** `2d612811…`, which `PROD_v2.27.76^{tree}` resolves to in the
> prod repo — re-verified **unchanged** this chunk (and `~/.aberp/` is out of
> reach of the sandbox). The commit SHA recorded in the baseline table
> (`f7519b4…`) is the tag's commit in the canonical/GitHub repo; the local
> working copy of prod was observed carrying the same `PROD_v2.27.76` tag on a
> *different commit object* (`079db9c…`) with the **identical tree**
> (`2d612811…`). Content identity (the tree) is the load-bearing proof of
> "byte-for-byte untouched", and it holds. Flagged for chunk 5 to reconcile
> the commit-object label when it confirms the frozen tag on GitHub.

## What CI enforces now (chunk 4) — and for which arms

On every push / PR to `main`, on a full Rust+Tauri runner, `ci.yml` runs a
two-cell matrix — **Portable** (default features) and **Defense**
(`--features production`). Each arm runs, in order:

| Gate | Portable | Defense |
|---|---|---|
| ADR-0093 DB-isolation cut-gate + negative probes — **moved to `cut-gate.yml` 2026-08-30**, see note below | — | — |
| `cargo fmt --all -- --check` | ✅ | ✅ |
| `cargo build --workspace --locked --all-targets` | ✅ default | ✅ `--features production` |
| `cargo test --workspace --locked` | ✅ default | ✅ `--features production` |
| Named integration tests (below) | ✅ | ✅ (aberp-pkg tests get `--features production`) |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | ✅ default | ✅ `--features production` |
| `cargo deny check` + `cargo audit` (arm-independent) | ✅ once | — |

Named integration tests (the bundled-DuckDB / wiring tests the sandbox could
not run, per the chunk-3 handoff): `aberp --test edition_db_isolation`,
`aberp --test edition_snapshot_isolation`,
`aberp-snapshot --test crash_safe_checkpoint_tests`,
`aberp-snapshot --test edition_isolation_tests`, and
`aberp-audit-ledger ensure_consistent_refuses_and_preserves_when_mirror_ahead_of_db`.
The `production` feature lives only on `aberp`/`aberp-ui`, so the `aberp`
binary tests take `--features production` on the Defense arm while the
edition-agnostic crate tests run featureless.

`cut-gate.yml` runs the cut-gate + negative probes (plus the ADR-0110
durable-ack and ADR-0111 checkpoint-site gates and their probes) as a
standalone, toolchain-free job, so the isolation mandate is gated
independently of the 8-minute build.

> **2026-08-30 (ADR-0116 / v0.6.4 landing) — the gate steps were removed from
> `ci.yml`.** Both workflows had been running `cut_gate_db_isolation.sh`,
> `cut_gate_negative_probes.sh`, `cut_gate_checkpoint_sites.sh` and
> `cut_gate_checkpoint_probes.sh` with identical `ENFORCE_*` env on identical
> triggers — so the probe harness was billed to two jobs per PR. ADR-0116
> touched no workflow file; it grew those two gate *scripts*, and the harness
> multiplied the growth. The `ci.yml` copy went 50 m 07 s (main `5020773`) to
> 69 m 18 s, which pushed that job past its 90-min cap and got `Test`
> CANCELLED — a false red on a required check. The four steps now run once,
> in `cut-gate.yml`, whose job name **is** the required context
> `ADR-0093 DB-isolation cut-gate`; nothing is de-gated. `ci.yml` keeps the
> strict-SVG lint (never duplicated) and remains the **sole** home of fmt,
> build, the workspace test suite, the named integration tests,
> `clippy -D warnings` and `cargo deny` — which is why that job was **not**
> deleted, only slimmed. The one thing given up is fail-fast ordering inside
> `ci.yml`; `cut-gate.yml` runs concurrently and still reddens in ~51 s.
> The underlying O(probes × full-gate-run) cost is still open below.

The three cut-gate CHECKs added this chunk (each defended by a negative probe
in `tools/cut_gate_negative_probes.sh`):

- **CHECK 5** — the durable checkpoint is *build-aside + atomic `rename(2)` +
  fsync(file & parent dir)*, **never an in-place rewrite** of the live DB
  (ADR-0082). Probe: swap the `rename` for an in-place `copy` → gate fails.
- **CHECK 6** — no editions **binary** source resolves prod's *bare* snapshot
  store `~/Documents/ABERP-snapshots/` (the `default_store_dir` resolver or
  the bare component); editions use `ABERP-snapshots-<edition>` (ADR-0093 §5).
  Generalizes CHECK 4d from `snapshot.rs` to the whole binary. Probe: plant a
  `default_store_dir(` call under `apps/aberp/src` → gate fails.
- **CHECK 7** — edition launchers bind a **single, matching** root; arms don't
  cross (a `--features production` launcher binds `.aberp-defense`, never the
  sibling/prod root). Catches what CHECK 3b cannot — a rogue/mismatched
  launcher. Probe: a new launcher that builds `--features production` but binds
  `.aberp-portable` → gate fails.

Toolchain: `dtolnay/rust-toolchain@stable`, matching the repo's
`rust-toolchain.toml` channel pin (ADR-0001/0021). **Flagged:** *not*
hard-pinned to 1.96 — the repo deliberately pins the stable *channel*, not a
version, and the MSRV floor lives in each `Cargo.toml`'s `rust-version`. If a
future stable bump adds a clippy lint, `-D warnings` can newly fail; that is
the repo's accepted, documented trade.

## Documented Mac-side / runner-only steps (NOT faked in CI)

- **The first real CI run.** Everything above is authored + statically
  validated in-sandbox; it has never executed. Its first green is on GitHub
  Actions (or the Mac mirroring `ci.yml`).
- **The packaged Tauri *bundle / installer*.** CI builds and tests the
  `aberp-ui` Rust crate (webkit2gtk-linked) on Linux for both arms, but the
  shippable installer (`tauri build` → `.dmg` on macOS, `.AppImage`/`.deb` on
  Linux) is a release-packaging step run Mac-side via `run/release.sh`. CI
  proves the code compiles/links/tests; it does not produce the signed
  installer.

## Required status check (chunk 5 wiring)

The cut-gate is intended to be a **required status check** on the editions
GitHub repo so the ADR-0093/0002 DB-isolation mandate cannot be merged away.
Once chunk 5 has created + pushed the repo (auth-gated), wire branch
protection on `main`:

- **UI (authoritative):** Settings → Branches → Add branch protection rule →
  pattern `main` → *Require status checks to pass before merging* → select
  **`ADR-0093 DB-isolation cut-gate`** (the `cut-gate.yml` job name); optionally
  also require **`portable · build + lint + test`** and
  **`defense · build + lint + test`** (the `ci.yml` matrix jobs). Enable
  *Require branches to be up to date before merging*.
- **API (`gh`, JSON body — the reliable form):**

      gh api -X PUT repos/<owner>/<repo>/branches/main/protection \
        -H "Accept: application/vnd.github+json" --input - <<'JSON'
      {
        "required_status_checks": {
          "strict": true,
          "checks": [
            {"context": "ADR-0093 DB-isolation cut-gate"},
            {"context": "portable · build + lint + test"},
            {"context": "defense · build + lint + test"}
          ]
        },
        "enforce_admins": true,
        "required_pull_request_reviews": null,
        "restrictions": null
      }
      JSON

  Status-check *contexts* are the job `name:` values. The cut-gate job is the
  load-bearing required check (toolchain-free, fast); the two `ci.yml` arms are
  recommended-additional.

## Chunk 5 — publish (handoff)

1. **Create the GitHub repo(s)** for the editions tree and push `main`
   (fork-with-history per ADR-0093 §4). **Auth-gated:** the saved PAT is
   **FLAGGED FOR ROTATION** and likely invalid — chunk 5 must **STOP and ask
   Ervin for a fresh token** rather than fail or improvise, and must never
   embed the token in a committed file.
2. **Wire the required status check** (section above) once the repo exists.
3. **Confirm prod still frozen** in the *original* repo (read-only):
   `git -C <original-ABERP> rev-parse 'PROD_v2.27.76^{tree}'` must equal
   `2d612811dd487a50f33476c484d1768cc8e99a51`; reconcile the tag's commit-object
   label per the provenance note above.
4. **Source of truth = the bundle.** The editions working checkout at
   `~/Documents/Claude/Projects/ABERP-Editions` was observed at a *different
   head* (`14d0b06`) than the written-back bundle; the **bundle is canonical**.
   Chunk 5 must sync that checkout from the bundle before pushing and must not
   push the stale checkout.


## Chunk 5 — publish status (PREPARED 2026-06-23, awaiting authenticated run)

Publish is **PREPARED, not executed.** It is auth-gated; the authenticated
push happens on Ervin's Mac under his own `gh`/git auth.

**Done in-session (sandbox, no auth):**
- Confirmed the **canonical bundle** head `efb55f0` (`git bundle verify` =
  complete history) and cloned it **fresh** — the stale `14d0b06` working
  checkout was **not** used.
- Grep-confirmed the branch-protection required-check **contexts** equal the
  actual workflow job `name:` values (byte-for-byte, incl. the U+00B7 `·`):
  `ADR-0093 DB-isolation cut-gate` (`cut-gate.yml`),
  `portable · build + lint + test` and `defense · build + lint + test`
  (`ci.yml` matrix arms).
- Verified every named integration-test target referenced by the consolidated
  build+test block exists (`aberp --test edition_db_isolation` /
  `edition_snapshot_isolation`; `aberp-snapshot --test crash_safe_checkpoint_tests`
  / `edition_isolation_tests`; the `aberp-audit-ledger` mirror-ahead reconcile
  `#[test]`), and that the `production` feature lives only on `aberp`/`aberp-ui`
  (so the snapshot/audit-ledger tests run featureless).
- `bash -n`-validated the publish block, the plain-git fallback, and the
  consolidated build+test block; validated the branch-protection JSON body.

**NOT done here (auth boundary — honest):** the GitHub repo creation, the
`git push`, and the branch-protection API call. The saved PAT is flagged
**DEAD/COMPROMISED** and was deliberately **not** used or echoed; Ervin
authenticates with `gh auth login` and runs the prepared command block.

### Commit-label reconciliation (resolves the chunk-4 provenance flag)

Re-verified **read-only** in the prod repo this chunk. `PROD_v2.27.76` exists
as **both** a branch and an **annotated tag**, and they reconcile cleanly:

- `refs/remotes/origin/PROD_v2.27.76` (branch) → commit **`f7519b4…`**, tree
  **`2d612811…`**.
- `refs/tags/PROD_v2.27.76` (annotated tag) → tag-object **`079db9c…`**, which
  **peels (`^{commit}`) to the same commit `f7519b4…`** (tree `2d612811…`).

The chunk-4 flag — "the tag sits on a *different commit object* `079db9c…`" —
was an **annotated-tag-object vs commit** conflation: `079db9c…` is the
annotated tag's **wrapper object** SHA (what a bare `rev-parse PROD_v2.27.76`
returns), **not** a commit. Peeled, it is `f7519b4…` — **exactly the SAW-OFF
baseline commit**. Both refs carry the **identical tree `2d612811…`** (the
content anchor). So there is **no commit-object discrepancy**: commit =
`f7519b4…` (baseline) and content = `2d612811…` (baseline) ⇒ **prod untouched**.
**No action taken on prod** (read-only verification only).

## Cut-gate cost — the harness multiplies the gate (CLOSED 2026-08-30 by sharding)

**Filed 2026-08-25, at the v0.6.1 cut.** The required check
`ADR-0093 DB-isolation cut-gate` was CANCELLED by its `timeout-minutes` cap —
*not* by a failing probe. Every probe passed and the gate exited 0; the job
was killed mid-suite and surfaced as a false red on a REQUIRED check. The cap
was raised 30 → 75 to unblock the cut, which gates strictly **more** than
before (the full suite now actually runs to completion). That is
accommodation, not a fix.

**The shape of the cost.** Two multiplying factors:

1. *Per-run cost tripled at ADR-0099 R6.* R6 added ~145 lines of gate script,
   a new 313-line scanner (`tools/adr0099_audit_writer_scan.awk`) behind
   CHECK 10P, and widened the R6 scan scope to the whole `crates/` tree. The
   awk scans now dominate a single gate run.
2. *The negative-probe harness re-runs the whole gate once per probe.*
   `tools/cut_gate_negative_probes.sh` is O(probes × (full-tree-copy +
   full-gate-run)) — 56 probes at the time of filing. So a 3× per-run
   regression arrives at CI as a 3× regression on the **whole job**: ~46 min
   against what was a 30-min cap.

**Why this is urgent even though the cap now fits.** The cap is the wrong
control surface. Left as is, the next scanner or the next batch of probes
puts us back here, and the cheapest way out of a red will look like dropping
a probe or narrowing a scan scope. That trade must never be taken: a probe is
what proves the corresponding check has teeth, and a narrowed scope is a
silent de-gate. **The timeout must not become the ceiling on how much we are
allowed to gate.**

**Directions, in preference order — all of them preserve every probe:**

- **Shard the harness across matrix jobs.** The probes are independent by
  construction (each plants its violation in a throwaway copy). A matrix over
  N shards divides wall-clock by N with zero change to what is checked. The
  required-check *context* name must be preserved — either keep one job that
  depends on the shards, or update branch protection to require all shard
  contexts, or the required check silently stops existing.
- **Cut the per-probe fixed cost.** The long-standing v0.2.6 flag above
  `timeout-minutes` still applies: `fresh()` copies the whole tree per probe.
  A `git archive` export, a shared read-only tree with the plant applied as an
  overlay, or one copy reused across probes would take the fixed cost to near
  zero.
- **Cut the per-run scan cost.** The R6 scanners walk the whole `crates/`
  tree per invocation. Scope narrowing is **not** acceptable, but a single
  pass that feeds several checks, or a file-list computed once and reused, is.

**Update 2026-08-30 (v0.6.4): the DOUBLE billing is gone; the cost is not.**
`ci.yml` was running the same harness as a fail-fast prelude, so every PR paid
it twice — and ADR-0116's script growth (50 m 07 s -> 69 m 18 s per copy) blew
`ci.yml`'s 90-min cap and got its `Test` step CANCELLED. Those four gate steps
were removed from `ci.yml`; they now run once, in `cut-gate.yml`, which is the
required context. That halves the spend and unblocked the v0.6.4 cut, but it
does **not** close this entry: `cut-gate.yml` alone is at **54 m 38 s against
its 75-min cap** (73%), and the per-probe cost still tracks gate size, so the
next probe-adding ADR walks into the same cliff with no second copy left to
remove. The three fixes below are still the fixes.

**Acceptance for closing this entry:** the full suite — every probe, every
`ENFORCE_*` check, at the current or wider scope — completes with margin
against a cap well under 75 minutes, and `timeout-minutes` is lowered again in
the same change. Closing it by removing a probe, skipping a check, or
narrowing a scan scope does not count and should be rejected in review.

**CLOSED 2026-08-30 — the harness is sharded; the acceptance test is met.**
The first direction above was taken, in the form that preserves every probe:
`tools/cut_gate_negative_probes.sh` now takes `PROBE_SHARD_INDEX` /
`PROBE_SHARD_TOTAL` and runs a disjoint round-robin subset of the SAME probes —
same plant, same full `fresh()` tree copy, same signature asserted against the
same COMPLETE gate. `cut-gate.yml` runs 6 such shards in parallel.

*Nothing was traded away for the speed.* No probe was removed, no `ENFORCE_*`
check skipped, no scan scope narrowed, and no probe now runs against anything
less than the whole gate. The per-probe cost is unchanged; only the number of
probes each JOB carries went down.

*The partition is itself gated*, because "fast because it silently stopped
running" is the exact failure this file exists to refuse. The script freezes
`EXPECTED_PROBES` and fails unless (a) the suite's probe ordinal count still
equals that freeze, (b) run + skipped accounts for every probe, and (c) this
shard ran *exactly* its arithmetic share. So a probe deleted, a `fresh()` that
lost its accounting site, a probe skipped by every shard (zero coverage), and an
accounting site that was never made shard-aware are each RED — not quietly fast.

*The required context name is unchanged.* `ADR-0093 DB-isolation cut-gate` is
now a fan-in job that requires every gate job and every shard to be `success`;
`failure`, `cancelled` and `skipped` all fail it. Branch protection was not
touched — renaming that context would have silently un-gated `main`.

Caps came down in the same change, as the acceptance required: the gate job
75 → 20 min, each probe shard capped at 30. **If a shard ever approaches its
cap, raise the shard count — do not raise the cap.** Shard count is a free knob
until the per-shard fixed cost (one tree copy per probe, paid in every shard)
dominates; that fixed cost is the residual, and the second direction above
(cutting `fresh()`) remains the way to attack it if it ever matters.

---

## CHECK 11 — two removal spellings out of model, and one probe-harness asymmetry

**Filed** 2026-08-29, from the ADR-0116 rev-3 adversarial review (§4). **Not a
hole in the contract; a limit on what a green CHECK 11 means.** Deferred
deliberately rather than half-closed, because the cheap versions of both fixes
make the check worse.

The rev-3 adversarial planted nine mutations against
`tools/adr0116_evidence_removal_scan.awk`. Seven are RED at the gate or at the
behavioural pin. Two walk past the scanner:

- **MF — an aliased import.** `use std::fs::remove_file as rm;` … `rm(p)`.
  The matcher is name-keyed, so the alias is invisible. Closing it needs a
  per-file `use … as X;` alias table feeding the matcher — not a wider regex,
  which would fire on every short call in the tree.
- **MH — destruction by truncation.** `std::fs::write(p, b"")` or
  `File::create(p)` over an existing artefact destroys it without unlinking it.
  This is a different *verb*, not a different spelling of removal. Adding
  `fs::write` / `File::create` to a REMOVAL matcher would classify every
  legitimate write in every tenant-home helper and flood the frozen manifest —
  the exact "the check gets switched off" failure the in-gate
  `self.remove_file()` / `fn remove_file` probe already exists to prevent.

**Why this is a note and not a fix-first.** Both mutations are killed by
`f7_prune_refuses_a_protected_directory_and_does_not_report_it_removed`, and the
same neutering inside the other guarded function is killed by
`ac6_guarded_remove_refuses_evidence_and_permits_a_live_transient`. Those two
tests cover both GUARDED functions in the tree, and the adversarial verified
each pin is RED under four independent neuterings. The scanner is one layer of
a two-layer design and the behavioural layer is the load-bearing one. The scope
limit is now stated in the scanner's own header so a green is not read as more
than it is.

**Also filed here, same review, one line of work:**
`tools/cut_gate_negative_probes.sh` calls `assert_planted` from `expect_fail`
but not from `expect_pass`. A no-op plant on a non-trigger probe would silently
degrade to a sanity check rather than failing as a HARNESS BUG. The one
`expect_pass` CHECK-11 probe appends with `printf >>` so it cannot no-op today —
but this is the BSD-sed class already on this repo's record (cut-gate CHECK 5),
and the asymmetry should not survive the next probe someone adds.

**Acceptance for closing:** the alias table lands with its own `expect_fail`
probe planting `use std::fs::remove_file as rm;`; truncation either lands as a
SEPARATE verdict class (never folded into the removal matcher) or the header
note stands as the deliberate answer; `expect_pass` asserts its plant. Closing
any of these by widening the removal regex until the manifest floods does not
count.
