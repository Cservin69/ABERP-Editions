#!/usr/bin/env bash
# Workflow plumbing, not a check: the aggregating required context of
# cut-gate.yml. Reads the results of the gate job and the probe-shard matrix
# (GATES, PROBES from `needs.*.result`). Only the literal 'success' passes;
# failure, cancelled and skipped are all NOT proven. Nothing to run locally:
# locally every gate it aggregates is run directly. Listed in ci/gate-parity.sh.
set -euo pipefail
: "${GATES:?GATES must be the gates job result}"
: "${PROBES:?PROBES must be the negative-probes matrix result}"
echo "gates            = $GATES"
echo "negative-probes  = $PROBES   (matrix aggregate: 'success' only if EVERY shard succeeded)"
rc=0
[ "$GATES" = "success" ]  || { echo "✗ the ENFORCED gate job did not succeed ($GATES)"; rc=1; }
[ "$PROBES" = "success" ] || { echo "✗ at least one negative-probe shard did not succeed ($PROBES)"; rc=1; }
if [ "$rc" -ne 0 ]; then
  echo "ADR-0093 DB-isolation cut-gate: ✗ FAILED"
  exit 1
fi
echo "ADR-0093 DB-isolation cut-gate: ✓ every ENFORCED check passed and every probe shard has teeth"
