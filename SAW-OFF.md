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
