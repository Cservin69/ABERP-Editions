# Correction — the Defense suite figures quoted in `e922317` (ADR-0125 merge)

**Date:** 2026-09-16. **Applies to:** commit `e922317`,
"merge: ADR-0125 — the buried veto closes ADR-0112 R3".

## What that commit message claims

```
Defense test suite          4075 passed, 0 FAILED, 5 ignored, 267 suites
```

## What is actually true

Re-measured on the same `main` content, one clean run:

```
Defense test suite          3923 passed, 0 FAILED, 3 ignored, 233 suites
```

## Why the original figure was wrong

**The log it came from was two interleaved runs.** A foreground
`cargo test --workspace --features production --no-fail-fast` was reported as
killed (the tool returned exit 144), but the `cargo` process kept running and
kept writing through its still-open file descriptor. A second run was then
started with `nohup … > <same path>`, which truncated the file and wrote from
offset zero while the first process continued writing at its own offset. The
result is one file containing fragments of two runs, and totals summed across
both.

**The signature, for anyone auditing a future run.** A single run emits exactly
one ``Finished `test` profile`` line and one `Doc-tests` section per crate. The
corrupted log had **one** `Finished` line but **52** `Doc-tests` sections for
26 crates — every crate twice. The clean re-run has 1 and 26.

## What this does and does not affect

**It does not change the merge decision, and none of the evidence it rested on
came from that log:**

- the 6-shard negative-probe harness — 6/6 shards, 80/80 probes, a separate
  run with its own per-shard verdict lines;
- all 50 committed STEP fixtures bit-identical at `%.17g`;
- the boss family going from 10 wrong to 0;
- the single portable red (`serve_boot_auto_recovers_ahead_mirror_with_no_operator_step`)
  proven independent by construction — it was measured in a checkout containing
  zero occurrences of `_cap_is_buried`.

**What is wrong is the suite count and the pass count quoted alongside them**,
and the ignored count (3, not 5). No failure appeared in either fragment, so
there is no evidence anything was failing — but "0 FAILED" read off a mangled
log is not a claim worth standing behind, which is why it is restated here from
a run whose single-run provenance is shown.

## Why this is a new commit and not an amended message

`e922317` has been reported and mirrored to
`origin/backup/local-main-20260916`. Rewriting it would change a SHA that is
already referenced elsewhere, which is the opposite of what a correction is
for. This repository's whole posture is append-only and correct-in-place-marked-
and-dated (ADR-0123 §D8, ADR-0124 §3); git history is the one genuinely
immutable artifact here, so the correction is appended rather than substituted.

## Process fix

Never write two runs to one path. Each gate run now writes a fresh log, and the
``Finished `test` profile`` and `Doc-tests` counts are printed with the totals
as single-run provenance.
