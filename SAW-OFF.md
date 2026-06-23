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
   cut-gate + CI, this doc. Prod proven untouched. ✅ **(this chunk)**
2. **Build-locked edition binding** — compile-time `Edition`; Defense/Portable
   resolve their OWN roots and physically refuse prod's DB; tests
   (own-DB, can't-cross, prod-resolves-`~/.aberp/prod`-unchanged, fresh-start).
   Repoint `run_defense.sh`/`run_portable.sh`; flip cut-gate CHECK 3 →
   `ENFORCE_EDITION_DB_BINDING=1`. *(needs Rust toolchain — see gating note)*
3. **Own write/checkpoint path** — edition-scoped `crates/aberp-snapshot` +
   DuckDB write path; extend `ensure_restore_allowed` to refuse `~/.aberp/prod`.
4. **Cut-gate / CI hardening** — full ADR-0002 DB-isolation enforcement.
5. **Publish** — create GitHub repo(s), push (auth-gated; stop on PAT
   failure), confirm original repo frozen at `v2.27.76`.

## Gating note

`cargo`/`rustc` were unavailable in the chunk-1 environment, so the full
`cargo build + clippy + cargo test --workspace` gate (incl. the new
prod-unchanged / isolation / can't-cross tests) runs in a follow-on session
that has the Rust+Tauri toolchain (mirror `.github/workflows/ci.yml`). Chunk 1
is gated by what is verifiable without a compiler: the cut-gate, `bash -n`
on all launchers, CI-yaml validity, a structural audit, and the prod
tree-hash proof.
