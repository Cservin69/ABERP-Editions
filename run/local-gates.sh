#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# local-gates.sh — the pre-merge harness: every gate CI would run, run here.
#
# Defense pushes with Actions off, so this is the only place the gates run.
# The list is not kept here: it is `ci/gate-parity.sh --list`, the gates in
# the order the workflows call them. A gate added to a workflow is picked up
# here the same day; a check added to a workflow without a gate fails parity,
# which this runs first and refuses to continue past.
#
# Every gate runs even after one fails, so one red does not hide another.
# Exit 0 only if all are green. Nothing is skipped — the unsharded negative
# probes (tools/cut_gate_negative_probes.sh) are the slow one.
#
# Environment the gates read: ABERP_EDITION_FEATURES (default
# "--features production"), ABERP_TEST_PYTHON (default: the extractor's
# .venv), CARGO_TARGET_DIR (set one per worktree).
#
# Usage: ./run/local-gates.sh
# ---------------------------------------------------------------------------
set -uo pipefail
ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# The gates default every ENFORCE_* to 1, as the workflows set them. One left
# exported at 0 in this shell would quietly weaken a local run; drop them all.
for v in $(compgen -e | grep '^ENFORCE_' || true); do unset "$v"; done

./ci/gate-parity.sh || { echo "local-gates: parity is red — fix that first"; exit 1; }
gates="$(./ci/gate-parity.sh --list)" || exit 1

pass=0; failed=()
while IFS= read -r g; do
  [ "$g" = "ci/gate-parity.sh" ] && { pass=$((pass + 1)); continue; }
  echo; echo "── $g"
  start=$SECONDS
  # </dev/null: the list is this loop's stdin; a gate that read stdin would
  # swallow the rest of it and those gates would never run.
  if bash "$g" </dev/null; then
    pass=$((pass + 1)); echo "── $g ✓ ($((SECONDS - start))s)"
  else
    failed+=("$g"); echo "── $g ✗ ($((SECONDS - start))s)"
  fi
done <<< "$gates"

total=$(( pass + ${#failed[@]} ))
listed=$(printf '%s\n' "$gates" | grep -c .)
echo
if [ "$total" -ne "$listed" ]; then
  echo "local-gates: ✗ $listed gates listed, $total ran — refusing to call that green"
  exit 1
fi
if [ "${#failed[@]}" -eq 0 ]; then
  echo "local-gates: ✓ $pass/$total green"
  exit 0
fi
echo "local-gates: ✗ $pass/$total green — red:"
printf '  %s\n' "${failed[@]}"
exit 1
