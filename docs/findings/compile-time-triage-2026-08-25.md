# Compile-time triage — the "493 of 515" stall and the CI `build + lint + test` wall

Investigated at `main` = `79ed238` (v0.6.1) in an isolated worktree
(`ABERP-Editions-wt-compiletime`, branch `perf/compile-time-regression`).
CI toolchain matched locally with `rustup toolchain install 1.98.0` /
`cargo +1.98.0`.

**Verdict: there is no compile-time regression.** Two separate things were
folded into one report, and neither is rustc getting slower on our code:

1. **The local "stalls at 493 of 515"** is the `libduckdb-sys` build script —
   the bundled DuckDB C++ source tree (352 translation units). It is a
   cold-target-dir cost only, ~8m27s on a 4-core CI runner. The remaining
   22 units are exactly the crates that cannot start until it finishes.
2. **The CI `defense · build + lint + test` wall** is the cut-gate negative-probe
   harness, not compilation. Compilation is **4%** of the job; the probes are
   **70%**.

---

## 1. Where the CI wall actually goes

Step durations for the `defense · build + lint + test` job, read from the
GitHub Actions API (warm cargo cache unless noted). This is the whole
history of the job, sampled:

| date | Build | Test | Clippy | **Cut-gate negative probes** | job total |
|---|---|---|---|---|---|
| 2026-06-25 | 126s | 353s | 41s | **4s** | — |
| 2026-06-29 | 134s | 387s | 42s | **6s** | — |
| 2026-07-04 | 135s | 531s | 39s | **301s** | — |
| 2026-07-07 | 143s | 572s | 45s | 631s | — |
| 2026-07-21 | 129s | 562s | 39s | 619s | — |
| 2026-08-04 | 150s | 617s | 42s | 668s | — |
| 2026-08-10 | 155s | 643s | 41s | 914s | — |
| 2026-08-14 | 146s | 660s | 43s | 924s | — |
| 2026-08-23 | 155s | 672s | 34s | 941s | 34m16s |
| 2026-08-25 (`b91fdc1`) | 180s | 610s | 35s | 2679s | — |
| 2026-08-25 (`79ed238`) | **162s** | **691s** | **34s** | **2883s** | **68m12s** |

Growth since late June, in seconds:

| step | delta | share of all growth |
|---|---|---|
| Cut-gate negative probes | **+2879s** | **83.6%** |
| Test | +338s | 9.8% |
| Build | +36s | 1.0% |
| Clippy | −7s | — |

**The Build step has been flat at 126–180s for two months.** No commit in the
audit-fork R3–R6 work, the QC work, or the event-kind machinery moved it. The
`ALL_KINDS_COUNT` / 195-variant `EventKind` enum was checked directly and is
not a codegen cost: it derives only `Debug, Clone, PartialEq, Eq` (no serde),
and its `ALL_KINDS` slice is a plain `&[EventKind]` static.

### The probe harness, measured

`tools/cut_gate_negative_probes.sh` re-runs the **entire** 20-CHECK gate scan
(`tools/cut_gate_db_isolation.sh`, 1278 lines of grep/awk over the whole tree)
once per probe. Per-probe deltas from the `79ed238` run log:

```
probe step window: 2880.6s
58 probes, ~48.7s each  (two at ~97s)
```

The gate scan itself went **19s → 38s → 48s** across 2026-08-23 → 08-25 as
ADR-0099 R3–R6 added the CHECK 10L/10N/10P arms, and the harness pays that cost
58 times. `48.7 × 58 ≈ 2825s` — the observed 2883s.

This is the already-filed SAW-OFF item, now quantified.

**Where the 48s goes** — timed locally on this worktree
(`time bash tools/cut_gate_db_isolation.sh`):

```
55.95s user  16.23s system  28% cpu  4:09.14 total     (contended box)
```

72s of CPU to grep/awk **276 `.rs` files**. The cost is process spawning, not
scanning: five separate `while read f; do awk -f "$scan" "$f"; done <
<(find apps/aberp/src modules crates -name '*.rs' ...)` loops (lines 712, 814,
888, 922, 1002), each spawning one `awk` per file, plus a second per-file `awk`
against the frozen-residual manifest in CHECK 10i. That is **~2,700 short-lived
`awk` processes per gate run**, and CI pays it **58 times** — on the order of
**150,000 process spawns per PR**, in each of the two workflows that run the
harness.

`awk` accepts a file list in one invocation (`FILENAME` / `FNR==1` to reset
per-file state and prefix the output). Batching each loop into a single `awk`
call is where the ~45 minutes is, and it changes no CHECK's semantics — but it
*is* an edit to gate code, so it needs its own change with the negative probes
re-run against it, not a drive-by.

## 2. The real timeout risk: a cache miss on top of today's probe cost

Two runs in the sample missed the cargo cache (`2026-07-21`, `2026-08-23`,
both after a `Cargo.lock` change). Cold-cache costs, measured from the
`32651754691` job log:

```
Compiling libduckdb-sys v1.10503.1        16:46:18
Compiling duckdb v1.10503.1               16:54:45   →  build script = 507s (8m27s)
Compiling aberp v0.0.0                    16:54:51
Finished `dev` profile ... in 11m 25s     16:56:59   →  apps/aberp    = 128s
```

Cold `Build` = 688–708s, cold `Clippy` = 513–531s. Against today's warm run:

| | warm (`79ed238`) | cold-cache equivalent |
|---|---|---|
| Build | 162s | ~690s (+528s) |
| Clippy | 34s | ~530s (+496s) |
| **job total** | **68m12s** | **~85m16s** |

The cap is `timeout-minutes: 90`. **A cache miss at today's probe cost leaves
under 5 minutes of headroom**, and the probe step itself has varied 2208–2883s
(±11 min) across three runs on the same day. That combination will exceed 90
minutes. The headroom that used to absorb a cache miss was consumed by the
probe step, not by rustc.

## 3. The local stall: `libduckdb-sys`, not our code

`cargo +1.98.0 build --features production` parks its progress counter at
`493/515` because `libduckdb-sys`'s build script is the long pole and
everything downstream of it is blocked. The arithmetic is exact:

```
$ cargo tree --features production -e normal -i duckdb --prefix none | sort -u
aberp, aberp-audit-ledger, aberp-billing, aberp-db, aberp-dispatch,
aberp-inventory, aberp-invoice-pdf, aberp-mes, aberp-mnb-rates, aberp-qa,
aberp-quote-intake, aberp-snapshot, aberp-verify, aberp-work-orders, duckdb
```

15 crates, plus the `aberp` bin unit, `aberp-ui`, and `libduckdb-sys`'s own lib
unit — the ~22 units that make up `515 − 493`. Everything not behind DuckDB
finishes, the counter stops, and the box spends the next several minutes in
`clang++` compiling **352 C++ translation units** (`base` 257 +
`core_functions` 23 + `parquet` 61 + `json` 11, from the crate's
`manifest.json`).

This is paid once per target directory. It is re-paid whenever the target dir
is fresh (a new worktree), or the toolchain changes — which is what the
2026-08-23 move to rustc 1.98 (`b05a7fc`) did to every local checkout.

### Why it was "ages" and not eight minutes

Measured on this machine during the investigation: **two** bundled-DuckDB
builds were running concurrently in different trees (this worktree's `debug`
build and a `release` build under `~/ABERP-Defense`), each spawning ~20
`clang++` jobs on a 10-core / 16 GB box:

```
load average: 86.78          38 concurrent clang++
vm.swapusage: used = 9535M / 10240M      Pages free: 2520 (≈40 MB)
```

The machine was in swap. Object-file completion rate fell to 2.7/min against
a 352-file target. The workload is ~8.5 minutes on an idle 4-core CI runner;
the hours are memory oversubscription, not the build.

## 4. What the warm build actually spends its time on

From the `79ed238` Build step log (`--all-targets`, 153s wall):

```
18:38:36  Compiling aberp-audit-ledger, aberp-quote-engine, aberp-billing, ... (21 crates)
18:38:45  Compiling aberp
18:41:09  Finished `dev` profile in 2m 39s
```

Every other workspace crate is done within ~10s. **`apps/aberp` alone is 143s
of the 153s (93%)** — its lib (151k LoC of Rust, of which `src/serve.rs` is
34,983 lines with 151 chained `.route()` calls in one `build_router`), its bin,
and its 84 integration-test binaries.

So `apps/aberp` *is* the slow unit — but at 143s it is 3.5% of a 68-minute job.
Eliminating it entirely would not change the timeout picture.

## 5. Fix: none applied

No change is made on this branch. Nothing measured supports one:

- **There is no regression to revert.** Build has been flat for two months.
- **A profile tweak is not worth it.** `[profile.dev] debug = "line-tables-only"`
  and `[profile.dev.package.libduckdb-sys] debug = false` are already set
  (S393). Dropping dep debuginfo to `0` would shave seconds off a 162s step
  that is 4% of the wall, at the cost of backtrace quality through dep frames.
- **Dropping DuckDB's `parquet` extension is not available.** It would cut 61
  of 352 C++ TUs (~17% of the cold DuckDB build), but
  `EXPORT DATABASE '…' (FORMAT PARQUET)` is the snapshot mechanism
  (`crates/aberp-snapshot/src/take.rs:306`,
  `crates/aberp-snapshot/src/crash_safe.rs:546`). Disabling it breaks
  durability. Rejected.
- **Splitting `apps/aberp` / `serve.rs`** is the only change that would move the
  compile number meaningfully, and it is a large refactor of a 35k-line file on
  the money path for a 143s saving that does not touch the timeout. Not a
  compile-time decision.

## 6. Recommendations, in order of value

1. **Shard or memoise the negative-probe harness** (already filed in
   `SAW-OFF.md`). 58 probes × a 48.7s full-tree rescan is the entire problem.
   The gate must not be weakened — but each probe only perturbs one file, so
   the scan does not need to re-walk the whole tree 58 times. This is ~45
   minutes of every PR, in **both** `ci.yml` and `cut-gate.yml`.
2. **Do not treat the 90-minute cap as headroom.** At today's probe cost a
   cold-cache run lands at ~85 minutes. Either land (1), or raise the cap again
   knowing why.
3. **Locally: do not run two bundled-DuckDB builds at once on a 16 GB box.**
   One `cargo build` per machine at a time, or point worktrees at a shared
   `CARGO_TARGET_DIR` so DuckDB is built once (cargo will serialise them on the
   target lock, which is strictly better than swapping).
4. **Expect a full DuckDB rebuild after any toolchain change.** The 1.98 move
   invalidated every local target dir. That is normal, one-off, and ~8.5
   minutes on an unloaded machine.
