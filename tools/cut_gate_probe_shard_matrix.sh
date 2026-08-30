#!/usr/bin/env bash
#
# cut_gate_probe_shard_matrix.sh — the negative-probe harness is SHARDED, and
# this holds the shard SET closed.
#
# tools/cut_gate_negative_probes.sh proves its own arithmetic: each shard checks
# that it ran exactly the probes whose ordinal is congruent to its index. What a
# single job structurally CANNOT see is whether the other shards exist. That is
# the one way sharding can lose coverage silently:
#
#   shard: [1, 1, 2, 3, 4, 5]     # a typo, or a bad merge
#
# `strategy.job-total` is 6, so every job runs, every job checks its own share
# against a total of 6, and every job PASSES — while the probes belonging to
# shard 6 run in NO job at all. The fan-in required check goes green. Nothing
# anywhere reports that a sixth of the suite stopped being tested.
#
# So the matrix itself is an invariant, and this is where it is enforced: the
# shard list must be exactly 1..N, each index present once, with N the matrix
# length — and PROBE_SHARD_TOTAL must be DERIVED from that length rather than
# written a second time as a literal that can drift from it.
#
# Fails CLOSED: if the matrix cannot be found or does not have the shape this
# script knows how to read, that is a failure, not a pass. A check that quietly
# skips itself when its input moves is the exact vacuous-green this whole gate
# family exists to refuse.
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
WF=".github/workflows/cut-gate.yml"
fail=0
note() { printf '  %s\n' "$*"; }
echo "[SHARD MATRIX] negative-probe shard set is exactly 1..N (no gap, no duplicate, total derived)"

if [[ ! -f "$WF" ]]; then
  note "✗ $WF not found — cannot verify the shard matrix"; exit 1
fi

# ── the shard list ───────────────────────────────────────────────────────────
nlines="$(grep -cE '^[[:space:]]*shard:[[:space:]]*\[' "$WF" || true)"
if [[ "$nlines" != "1" ]]; then
  note "✗ expected exactly ONE 'shard: [...]' matrix line in $WF, found $nlines."
  note "  If the matrix moved or grew a second form, teach this script the new shape —"
  note "  do not delete the check, or the shard set stops being held closed."
  exit 1
fi
raw="$(grep -E '^[[:space:]]*shard:[[:space:]]*\[' "$WF" | sed -E 's/^[^[]*\[//; s/\].*$//')"
list="$(printf '%s' "$raw" | tr ',' '\n' | sed -E 's/[[:space:]]//g' | grep -vE '^$' || true)"
n="$(printf '%s\n' "$list" | grep -c . || true)"

if [[ "${n:-0}" -lt 1 ]]; then
  note "✗ the shard matrix is empty — that is zero probe coverage, not a fast gate"; exit 1
fi
if printf '%s\n' "$list" | grep -qvE '^[1-9][0-9]*$'; then
  note "✗ the shard matrix holds a non-positive-integer entry:"
  printf '%s\n' "$list" | grep -vE '^[1-9][0-9]*$' | sed 's/^/      /'
  fail=1
fi
dups="$(printf '%s\n' "$list" | sort -n | uniq -d)"
if [[ -n "$dups" ]]; then
  note "✗ duplicate shard index in the matrix — the duplicated index runs twice and the index it"
  note "  displaced runs in NO job, while every job still passes its own share check:"
  printf '%s\n' "$dups" | sed 's/^/      /'
  fail=1
fi
want="$(seq 1 "$n")"
got="$(printf '%s\n' "$list" | sort -n)"
if [[ "$want" != "$got" ]]; then
  note "✗ the shard matrix is not exactly 1..$n. Every index in that range must appear exactly once,"
  note "  because probe ordinals are assigned round-robin over 1..PROBE_SHARD_TOTAL and"
  note "  PROBE_SHARD_TOTAL is the matrix LENGTH ($n). A missing index is a silently untested"
  note "  1/$n of the suite."
  note "  expected: $(printf '%s' "$want" | tr '\n' ' ')"
  note "  found:    $(printf '%s' "$got"  | tr '\n' ' ')"
  fail=1
fi

# ── the total must be DERIVED, never written twice ───────────────────────────
if ! grep -qF 'PROBE_SHARD_TOTAL: ${{ strategy.job-total }}' "$WF"; then
  note "✗ PROBE_SHARD_TOTAL is not derived from \${{ strategy.job-total }} in $WF."
  note "  A literal total that disagrees with the matrix length is invisible: with a total one"
  note "  higher than the number of jobs, a whole shard's probes run nowhere and every job still"
  note "  reports that it ran exactly its share."
  grep -n 'PROBE_SHARD_TOTAL' "$WF" | sed 's/^/      /'
  fail=1
fi

if [[ "$fail" -ne 0 ]]; then echo "SHARD MATRIX: ✗ FAILED"; exit 1; fi
echo "✓ shard matrix is 1..$n, each index exactly once, PROBE_SHARD_TOTAL derived from the matrix length"
