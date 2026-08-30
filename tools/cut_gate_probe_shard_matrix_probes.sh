#!/usr/bin/env bash
#
# cut_gate_probe_shard_matrix_probes.sh — proves cut_gate_probe_shard_matrix.sh
# has TEETH, the same way every other gate here is proved: plant the violation
# in a throwaway copy and assert the check goes RED.
#
# This one matters more than its size suggests. The shard-matrix check is the
# ONLY thing standing between a mistyped matrix and a silently untested slice of
# the negative-probe suite — a failure with no other symptom, because every
# shard job still passes its own arithmetic. A check like that must be shown to
# fire, and must be shown NOT to fire on the one edit it has to keep allowing:
# raising the shard count, which is the documented knob for wall-time.
#
# Cheap by construction: the check reads a workflow file, so a "copy of the
# tree" is two files. Runs in well under a second.
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SRC_WF="$ROOT/.github/workflows/cut-gate.yml"
SRC_CK="$ROOT/tools/cut_gate_probe_shard_matrix.sh"
RUN_TMP="$(mktemp -d "${TMPDIR:-/tmp}/cutgate-shardmx.XXXXXX")"
trap 'rm -rf "$RUN_TMP"' EXIT
pass=0; bad=0
MATRIX='shard: \[1, 2, 3, 4, 5, 6\]'

stage() {  # $1 sed-expr (applied to the workflow) -> echoes the staged dir
  local d; d="$(mktemp -d "$RUN_TMP/case.XXXXXX")"
  mkdir -p "$d/tools" "$d/.github/workflows"
  cp "$SRC_CK" "$d/tools/"
  sed "$1" "$SRC_WF" > "$d/.github/workflows/cut-gate.yml"
  printf '%s' "$d"
}
expect_red() {  # $1 label  $2 sed-expr
  local d out rc; d="$(stage "$2")"
  out="$(cd "$d" && bash tools/cut_gate_probe_shard_matrix.sh 2>&1)"; rc=$?
  # A no-op mutation would make this probe test nothing, so require the plant to
  # have actually changed the workflow (the HARNESS-BUG class this file family
  # has been bitten by before: a sed that silently matched nothing).
  if cmp -s "$SRC_WF" "$d/.github/workflows/cut-gate.yml"; then
    printf '  ✗ HARNESS BUG: %s — the plant changed NOTHING, so this probe tests nothing.\n' "$1"; bad=$((bad+1)); return
  fi
  if [[ "$rc" != "0" ]]; then
    printf '  ✓ caught: %s\n' "$1"; pass=$((pass+1))
  else
    printf '  ✗ ESCAPED: %s — the check exited 0 on a matrix that loses coverage.\n' "$1"
    printf '%s\n' "$out" | sed 's/^/        /'; bad=$((bad+1))
  fi
}
expect_green() {  # $1 label  $2 sed-expr ('' = unmutated)
  local d out rc; d="$(stage "${2:-}")"
  out="$(cd "$d" && bash tools/cut_gate_probe_shard_matrix.sh 2>&1)"; rc=$?
  if [[ "$rc" == "0" ]]; then
    printf '  ✓ %s\n' "$1"; pass=$((pass+1))
  else
    printf '  ✗ CRIES WOLF: %s — the check went red on a legitimate workflow.\n' "$1"
    printf '%s\n' "$out" | sed 's/^/        /'; bad=$((bad+1))
  fi
}

echo "negative probes for the negative-probe SHARD MATRIX check"
echo

# Each of these leaves at least one shard index covered by NO job while every
# shard job still passes its own share check — the whole reason this exists.
expect_red "a duplicated index displaces another (shard 6 runs nowhere)" \
  "s/$MATRIX/shard: [1, 1, 2, 3, 4, 5]/"
expect_red "a gap in the range (shard 6 runs nowhere; shard 7 is out of range)" \
  "s/$MATRIX/shard: [1, 2, 3, 4, 5, 7]/"
expect_red "0-based off-by-one (shard 6 runs nowhere)" \
  "s/$MATRIX/shard: [0, 1, 2, 3, 4, 5]/"
expect_red "a non-integer index" \
  "s/$MATRIX/shard: [1, 2, 3, 4, 5, six]/"
expect_red "an EMPTY matrix — zero probe coverage must not read as a fast gate" \
  "s/$MATRIX/shard: []/"
expect_red "the matrix deleted entirely (check must fail CLOSED, not skip itself)" \
  "/$MATRIX/d"
# The total written as a literal is the silent one: with a literal one HIGHER
# than the job count, a whole shard's probes run nowhere and every job passes.
expect_red "PROBE_SHARD_TOTAL written as a literal instead of derived" \
  's/PROBE_SHARD_TOTAL: ${{ strategy.job-total }}/PROBE_SHARD_TOTAL: "6"/'

echo "  --- non-triggers: a gate that cries wolf gets switched off ---"
expect_green "the unmutated workflow PASSES" ""
# This is the documented remedy when a shard approaches its cap. If this check
# ever blocked it, the next person would delete the check instead of resharding.
expect_green "raising the shard count 6 -> 8 is ALLOWED (1..8 stays intact)" \
  "s/$MATRIX/shard: [1, 2, 3, 4, 5, 6, 7, 8]/"
expect_green "lowering the shard count 6 -> 3 is ALLOWED (1..3 stays intact)" \
  "s/$MATRIX/shard: [1, 2, 3]/"

echo
echo "probes passed: $pass   broken/escaped: $bad"
if [[ "$bad" -ne 0 ]]; then echo "SHARD-MATRIX PROBES: ✗ FAILED"; exit 1; fi
echo "SHARD-MATRIX PROBES: ✓ THE SHARD-SET CHECK HAS TEETH"
