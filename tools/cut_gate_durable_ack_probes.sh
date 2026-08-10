#!/usr/bin/env bash
#
# cut_gate_durable_ack_probes.sh — teeth for the ADR-0110 D3 durable-ack gate.
#
# A negative probe that cannot fail is decoration. Each probe plants ONE
# regression into a throwaway copy of the tree and asserts the gate goes RED,
# then pins the non-triggers that would otherwise make the gate cry wolf — and a
# gate that cries wolf gets switched off, which is how ADR-0110 §1's original
# blind spot survived for months.
#
# P1  delete the durable_ack() call from invoice issuance          -> RED
# P2  comment it out ("just for now")                              -> RED
# P3  add an unregistered call site in a non-money path            -> RED
# P4  delete a censused file's entry from the census (count drift) -> RED
# P5  de-gate the script (ENFORCE_DURABLE_ACK=0) on a broken tree  -> GREEN, loudly
# P0  the unmutated tree                                           -> GREEN
# P6  a doc-comment mention of `Handle::durable_ack` alone         -> RED (not a call)
# P7  the call kept but its error SWALLOWED (rule-11 downgrade)    -> RED (D3-C)
# P8  P7's mutation flags ONLY the mutated site                    -> 1 swallowed, 4 propagate

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
GATE="tools/cut_gate_durable_ack.sh"
CENSUS="tools/adr0110_durable_ack_sites.txt"
fail=0
pass() { printf '  ✓ %s\n' "$*"; }
bad()  { printf '  ✗ %s\n' "$*"; fail=1; }

echo "ADR-0110 D3 durable-ack gate — negative probes"

# A throwaway copy of just what the gate reads: the census, the gate, and the
# money-path sources. Copying the whole tree would be slower for no gain.
mktree() {
  local d; d="$(mktemp -d)"
  mkdir -p "$d/tools" "$d/apps/aberp/src"
  cp "$ROOT/$GATE" "$d/tools/" && cp "$ROOT/$CENSUS" "$d/tools/"
  cp "$ROOT"/apps/aberp/src/*.rs "$d/apps/aberp/src/"
  printf '%s' "$d"
}

run_gate() { ( cd "$1" && bash "$GATE" >"$1/out.txt" 2>&1; echo $? ); }

probe() { # name expected_exit mutator
  local name="$1" want="$2" mut="$3" d rc
  d="$(mktree)"
  ( cd "$d" && eval "$mut" )
  rc="$(run_gate "$d")"
  if [[ "$rc" == "$want" ]]; then
    pass "$name (exit $rc as expected)"
  else
    bad "$name — expected exit $want, got $rc"
    sed 's/^/      /' "$d/out.txt"
  fi
  rm -rf "$d"
}

probe "P0 unmutated tree stays GREEN" 0 "true"

probe "P1 durable_ack() DELETED from invoice issuance -> RED" 1 \
  "grep -v '\.durable_ack()' apps/aberp/src/issue_invoice.rs > t && mv t apps/aberp/src/issue_invoice.rs"

probe "P2 durable_ack() COMMENTED OUT in storno -> RED" 1 \
  "sed 's|^\( *\)db\.durable_ack()|\1// db.durable_ack()|' apps/aberp/src/issue_storno.rs > t && mv t apps/aberp/src/issue_storno.rs"

probe "P3 UNREGISTERED call site in a non-money path -> RED" 1 \
  "printf 'fn f(db: \&Handle) { db.durable_ack().unwrap(); }\n' >> apps/aberp/src/products.rs"

probe "P4 a censused entry DROPPED from the census (count drift) -> RED" 1 \
  "grep -v 'mark_invoice_paid' tools/adr0110_durable_ack_sites.txt > t && mv t tools/adr0110_durable_ack_sites.txt"

probe "P6 a doc MENTION is not a call — deleting the call leaves the mention -> RED" 1 \
  "grep -v '\.durable_ack()' apps/aberp/src/mark_invoice_paid.rs > t && mv t apps/aberp/src/mark_invoice_paid.rs"

# P7/P8 — the CHECK D3-C regression, and the reason A/B are not enough. The call
# stays; only its error handling changes, to the exact `if let Err(e) = ... {
# warn!(..) }` downgrade ADR-0110 R3 and CLAUDE.md rule 11 forbid by name. A and
# B both stay satisfied — the call is present and censused — so without D3-C
# this is a GREEN gate over an ack that lies to the operator.
SWALLOW='if let Err(e) = db.durable_ack() {\n        tracing::warn!(error = %e, "durable-ack failed; continuing");\n    }'
swallow_mut="perl -0pi -e 's/    db\.durable_ack\(\)\n        \.context\(\"ADR-0110 D3 durable-ack fsync after invoice issuance commit\"\)\?;/    ${SWALLOW}/' apps/aberp/src/issue_invoice.rs"

probe "P7 the call kept but its error SWALLOWED (rule-11 downgrade) -> RED" 1 "$swallow_mut"

# P8 — precision, not just sensitivity. A check that flagged every site would
# also go red here and would be useless; assert it names the ONE mutated site
# and clears the other four.
d="$(mktree)"
( cd "$d" && eval "$swallow_mut" )
out="$( cd "$d" && bash "$GATE" 2>&1 )"
nswallow="$(printf '%s' "$out" | grep -c 'SWALLOWED durable-ack failure')"
# Match the per-site verdict line only — the CHECK D3-C header also contains
# the word "PROPAGATES" and would otherwise be counted as a sixth site.
nprop="$(printf '%s' "$out" | grep -c '— PROPAGATES')"
if [[ "$nswallow" == "1" && "$nprop" == "4" ]] \
   && printf '%s' "$out" | grep -q 'SWALLOWED durable-ack failure at apps/aberp/src/issue_invoice.rs'; then
  pass "P8 D3-C flags ONLY the mutated site (1 swallowed, 4 propagate)"
else
  bad "P8 D3-C imprecise — expected 1 swallowed / 4 propagates, got $nswallow / $nprop"
  printf '%s\n' "$out" | sed 's/^/      /'
fi
rm -rf "$d"

# P5 — fail-closed. With enforcement off the gate must still RUN and REPORT the
# defect (exit 0 by construction), so a de-gated CI step is visible in the log
# rather than indistinguishable from a clean tree.
d="$(mktree)"
( cd "$d" && grep -v '\.durable_ack()' apps/aberp/src/issue_invoice.rs > t && mv t apps/aberp/src/issue_invoice.rs )
rc="$( cd "$d" && ENFORCE_DURABLE_ACK=0 bash "$GATE" >"$d/out.txt" 2>&1; echo $? )"
if [[ "$rc" == "0" ]] && grep -q "enforcement disabled" "$d/out.txt"; then
  pass "P5 de-gated run passes but SAYS SO (enforcement disabled is in the log)"
else
  bad "P5 de-gated run did not announce itself — a silent de-gate is worse than no gate"
  sed 's/^/      /' "$d/out.txt"
fi
rm -rf "$d"

echo
if [[ "$fail" -ne 0 ]]; then echo "PROBES: ✗ FAILED"; exit 1; fi
echo "PROBES: ✓ PASSED — the gate has teeth"
