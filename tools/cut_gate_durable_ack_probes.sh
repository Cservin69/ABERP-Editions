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
# P8  P7's mutation flags ONLY the mutated site                    -> 1 swallowed, rest propagate
# P11 an ack MOVED between two censused files (total unchanged)    -> RED via D3-A, D3-B green

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

# P9/P10 — PR #37 adversarial B1. P7 above only fails because the site it
# mutates (issue_invoice) happens to have no `?` in the three lines after the
# ack. The adversarial defeated the window-based D3-C by downgrading the ack at
# mark_invoice_paid.rs and letting an UNRELATED trailing `?;` satisfy the
# window — gate green, clippy clean, ack dead. These probe that exact shape at
# a site that is NOT the P7 site, so the calibration accident cannot hide it.
B1_SWALLOW='if let Err(e) = db.durable_ack() {\n        tracing::warn!(error = %e, "durable-ack failed; continuing");\n    }\n    let _unrelated = is_canonical_iso_date(\&input.paid_at)\n        .then_some(())\n        .ok_or_else(|| anyhow!("unrelated"))?;'
b1_mut="perl -0pi -e 's/    db\.durable_ack\(\)\n        \.context\(\"ADR-0110 D3 durable-ack fsync after mark-paid commit\"\)\?;/    ${B1_SWALLOW}/' apps/aberp/src/mark_invoice_paid.rs"

probe "P9 ack downgraded but an UNRELATED trailing ?; follows (B1 bypass) -> RED" 1 "$b1_mut"

# P10 — the non-wrapper half of the same bypass: the call is left bare (its own
# Result dropped on the floor) with a propagating statement right after it. The
# window check read the neighbour's `?` and passed; the statement-anchored
# check must not.
BARE='db.durable_ack();\n    let _unrelated = is_canonical_iso_date(\&input.paid_at)\n        .then_some(())\n        .ok_or_else(|| anyhow!("unrelated"))?;'
bare_mut="perl -0pi -e 's/    db\.durable_ack\(\)\n        \.context\(\"ADR-0110 D3 durable-ack fsync after mark-paid commit\"\)\?;/    ${BARE}/' apps/aberp/src/mark_invoice_paid.rs"

probe "P10 bare ack + a neighbouring ?; (statement-anchor test) -> RED" 1 "$bare_mut"

# P8 — precision, not just sensitivity. A check that flagged every site would
# also go red here and would be useless; assert it names the ONE mutated site
# and clears every other one.
#
# The expected propagate count is DERIVED, never written down. It was a literal
# `4` from the five-site census, and D-22 grew the census to 26 — so the probe
# went red on a gate whose PROPERTY (exactly one swallowed, all others
# propagating) still held perfectly. Bumping the constant would just re-arm the
# same rot at the next census growth. So: measure the unmutated tree's D3-C
# verdict count first, and expect that minus the one site P7 downgrades.
#
# Match the per-site verdict line only — the CHECK D3-C header also contains
# the word "PROPAGATES" and would otherwise be counted as an extra site.
d="$(mktree)"
base_out="$( cd "$d" && bash "$GATE" 2>&1 )"
base_prop="$(printf '%s' "$base_out" | grep -c '— PROPAGATES')"
rm -rf "$d"

# A derived expectation can go vacuous the way a literal cannot: if the matcher
# died and the baseline were 0 or 1, "expect base-1" would be satisfied by a
# gate that reports nothing at all. Pin the floor.
if [[ "$base_prop" -lt 2 ]]; then
  bad "P8 baseline is degenerate — the unmutated tree reports only $base_prop propagating site(s);"
  printf '      a derived expectation of %s cannot distinguish a precise gate from a dead one.\n' "$((base_prop - 1))"
fi
want_prop=$((base_prop - 1))

d="$(mktree)"
( cd "$d" && eval "$swallow_mut" )
out="$( cd "$d" && bash "$GATE" 2>&1 )"
nswallow="$(printf '%s' "$out" | grep -c 'SWALLOWED durable-ack failure')"
nprop="$(printf '%s' "$out" | grep -c '— PROPAGATES')"
if [[ "$nswallow" == "1" && "$nprop" == "$want_prop" ]] \
   && printf '%s' "$out" | grep -q 'SWALLOWED durable-ack failure at apps/aberp/src/issue_invoice.rs'; then
  pass "P8 D3-C flags ONLY the mutated site (1 swallowed, $nprop propagate; baseline $base_prop, derived)"
else
  bad "P8 D3-C imprecise — expected 1 swallowed / $want_prop propagates (baseline $base_prop − 1), got $nswallow / $nprop"
  printf '%s\n' "$out" | sed 's/^/      /'
fi
rm -rf "$d"

# P11 — teeth for D3-A's per-file exactness (D-22 adversarial M7). Move ONE ack
# from a censused file that owns three boundaries to a censused file that owns
# four: the whole-tree total is unchanged, so D3-B stays GREEN and every file
# still has at least one call — the old `>= 1` D3-A was green on this. The
# twelve NAV-gated sites have no test cover at all, so an ack silently relocated
# off (say) the Attempt-before-call boundary is exactly the edit that must not
# pass. Assert the gate goes red AND that it is D3-A, not D3-B, that catches it.
move_mut="perl -0pi -e 's/    db\.durable_ack\(\)/    let _ack_moved_away = ();/' apps/aberp/src/submit_invoice.rs"
move_mut="$move_mut && printf 'fn _relocated(db: &Handle) -> anyhow::Result<()> { db.durable_ack()?; Ok(()) }\n' >> apps/aberp/src/retry_submission.rs"
d="$(mktree)"
( cd "$d" && eval "$move_mut" )
rc="$( cd "$d" && bash "$GATE" >"$d/out.txt" 2>&1; echo $? )"
if [[ "$rc" == "1" ]] \
   && grep -q 'census closed' "$d/out.txt" \
   && grep -q 'but the census owes it' "$d/out.txt"; then
  pass "P11 an ack MOVED between censused files is RED via D3-A while D3-B stays green"
else
  bad "P11 a relocated ack was not caught per-file — exit $rc (want 1, with D3-B still reporting 'census closed')"
  sed 's/^/      /' "$d/out.txt"
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
