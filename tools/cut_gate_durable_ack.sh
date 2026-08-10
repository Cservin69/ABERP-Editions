#!/usr/bin/env bash
#
# cut_gate_durable_ack.sh — ADR-0110 D3 durable-ack cut-gate.
#
# ADR-0110 §7.9 states the complaint this whole document opens with: "One test
# is not a gate; a gate is a check that fails a cut." The D6b power-loss spec
# (apps/aberp/tests/adr0110_d6b_ondisk_durability.rs) proves the money path is
# durable TODAY. This is the other half — the check that keeps it durable.
#
# The regression it exists to catch is not exotic. It is a refactor that moves,
# reorders, or drops the `db.durable_ack()` line at an ack boundary. The D6b
# tier-2 test catches that for invoice issuance and mark-paid; it cannot catch
# it for modification, storno, or the AP status change, because those need NAV
# credentials / a NAV envelope / an ingested AP row and no unattended test may
# have them. This gate covers all five, statically, in seconds, with no
# toolchain — the same posture as the ADR-0099 opener-census gate.
#
# CHECK D3-A — every censused money-path ack file still calls `durable_ack()`.
# CHECK D3-B — the call-site count across apps/*/src EQUALS the census count,
#              so a new unregistered ack site is as red as a deleted one.
# CHECK D3-C — every call site PROPAGATES the failure (`?`), rather than
#              swallowing it. A/B only prove the call is present; they are
#              blind to the one-line rewrite
#                  if let Err(e) = db.durable_ack() { tracing::warn!(..) }
#              which is the exact CLAUDE.md rule-11 / ADR-0110 R3 downgrade the
#              ADR forbids by name, and which leaves the operator acked on a
#              write nobody could make durable.
#
# ENFORCE_DURABLE_ACK=0 disables enforcement (used by the probe harness to
# prove fail-closed). Exit 0 = gate green.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
fail=0
note() { printf '  %s\n' "$*"; }
echo "ADR-0110 D3 durable-ack cut-gate — root: $ROOT"

CENSUS="tools/adr0110_durable_ack_sites.txt"
[[ -f "$CENSUS" ]] || {
  note "✗ FAIL: census missing: $CENSUS"
  echo; echo "CUT-GATE: ✗ FAILED"; exit 1
}

enforce="${ENFORCE_DURABLE_ACK:-1}"
flag() {
  note "$1"
  if [[ "$enforce" == "1" ]]; then fail=1; else note "  (enforcement disabled — not failing)"; fi
}

# A CALL, not a mention. `.durable_ack()` with a leading dot and empty parens is
# the call form; the doc comments throughout the tree write it as
# `Handle::durable_ack` / [`Handle::durable_ack`], which this cannot match. Line
# comments are stripped anyway so a commented-out call does not count as one —
# that is the exact shape of the regression (someone comments it out "for now").
ack_calls() { grep -nE '^[^/]*\.durable_ack\(\)' "$1" 2>/dev/null; }

# ── CHECK D3-0 — matcher liveness (ALWAYS ENFORCED) ──────────────────────────
# "Zero hits ⇒ green" gates pass when the matcher is broken. These two controls
# make a silently-dead matcher fail the gate instead, and pin the comment
# exclusion in both directions.
echo "[CHECK D3-0] matcher liveness — it sees a real call and ignores a mention"
probe="$(mktemp)"; trap 'rm -f "$probe"' EXIT
printf '    db.durable_ack()\n        .context("x")?;\n' > "$probe"
[[ "$(ack_calls "$probe" | wc -l | tr -d ' ')" == "1" ]] \
  || { note "✗ FAIL: matcher missed a real .durable_ack() call"; fail=1; }
printf '/// see [`Handle::durable_ack`] for why\n    // db.durable_ack()\n' > "$probe"
[[ "$(ack_calls "$probe" | wc -l | tr -d ' ')" == "0" ]] \
  || { note "✗ FAIL: matcher counted a doc mention or commented-out call"; fail=1; }
# D3-C's own liveness. Its dangerous failure direction is "everything reads
# PROPAGATES" — silent green. A broken window/sed would do exactly that, so
# pin the swallowing form as NOT propagating before trusting any ✓ below.
propagates() { sed -n "1,4p" "$1" | sed 's://.*::' | grep -qE '\?[[:space:]]*;'; }
printf 'db.durable_ack()\n    .context("x")?;\n' > "$probe"
propagates "$probe" || { note "✗ FAIL: propagation check missed a real \`?\`"; fail=1; }
printf 'if let Err(e) = db.durable_ack() {\n    warn!("x");\n}\n' > "$probe"
propagates "$probe" && { note "✗ FAIL: propagation check called a swallowed error propagated"; fail=1; }
printf 'db.durable_ack()\n    .context("x");  // no question mark\n' > "$probe"
propagates "$probe" && { note "✗ FAIL: propagation check accepted a missing \`?\`"; fail=1; }
if [[ "$fail" -ne 0 ]]; then echo; echo "CUT-GATE: ✗ FAILED (matcher liveness)"; exit 1; fi
note "✓ matcher live"
echo

# ── CHECK D3-A — every censused ack site still calls durable_ack ─────────────
echo "[CHECK D3-A] every censused money-path ack still calls durable_ack() (ENFORCED)"
census_files=()
while IFS= read -r line; do
  [[ -z "$line" || "$line" == \#* ]] && continue
  f="${line%%$'\t'*}"
  census_files+=("$f")
  if [[ ! -f "$f" ]]; then
    flag "✗ censused money-path file is GONE: $f — update $CENSUS if the ack moved"
    continue
  fi
  n="$(ack_calls "$f" | wc -l | tr -d ' ')"
  if [[ "$n" -lt 1 ]]; then
    flag "✗ ADR-0110 R1 AT RISK: $f no longer calls durable_ack() — an operator-visible"
    note "    money-path ack there is back to promising a write that only lives in an"
    note "    un-fsync'd WAL. That is the 2026-08-08 loss, restored."
  else
    note "✓ $f — $n call site(s)"
  fi
done < "$CENSUS"
echo

# ── CHECK D3-B — the census is CLOSED (no unregistered call sites) ───────────
echo "[CHECK D3-B] durable_ack() call sites across apps/*/src match the census (ENFORCED)"
expected="${#census_files[@]}"
actual=0
while IFS= read -r f; do
  n="$(ack_calls "$f" | wc -l | tr -d ' ')"
  actual=$((actual + n))
  if [[ "$n" -gt 0 ]]; then
    listed=0
    for c in "${census_files[@]}"; do [[ "$c" == "$f" ]] && listed=1; done
    [[ "$listed" -eq 1 ]] || flag "✗ UNCENSUSED durable_ack() call site: $f — add it to $CENSUS with its reason"
  fi
done < <(find apps/*/src -name '*.rs' | sort)

if [[ "$actual" -ne "$expected" ]]; then
  flag "✗ call-site count $actual != census count $expected"
  note "    A count that drifted either way is a real change to which acks are durable."
else
  note "✓ $actual call site(s) across $expected censused file(s) — census closed"
fi

echo

# ── CHECK D3-C — the failure must PROPAGATE, not be swallowed ────────────────
# A/B are satisfied by a call that exists. They cannot see the difference
# between `db.durable_ack().context(..)?` and
# `if let Err(e) = db.durable_ack() { warn!(..) }` — and the second is the
# 2026-08-08 posture with extra steps: the operator is acked, the write is not
# durable, and the only trace is a log line nobody reads.
#
# The `?` may not be on the call line: the house style is
#     db.durable_ack()
#         .context("...")?;
# so scan a small WINDOW forward from the call. The window is deliberately
# tight — a `?` five lines later belongs to a different statement.
echo "[CHECK D3-C] every durable_ack() call PROPAGATES its error (ENFORCED)"
WINDOW=3
for f in "${census_files[@]}"; do
  [[ -f "$f" ]] || continue
  while IFS= read -r rec; do
    [[ -z "$rec" ]] && continue
    ln="${rec%%:*}"
    # Lines [ln, ln+WINDOW], comments stripped, looking for a `?` terminator.
    if sed -n "${ln},$((ln + WINDOW))p" "$f" | sed 's://.*::' | grep -qE '\?[[:space:]]*;'; then
      note "✓ $f:$ln — PROPAGATES"
    else
      flag "✗ SWALLOWED durable-ack failure at $f:$ln — no \`?\` within $WINDOW line(s)."
      note "    ADR-0110 R3 / CLAUDE.md rule 11: a money-path write that could not be"
      note "    made durable must FAIL the ack, not log and continue. Acking a write"
      note "    nothing fsync'd is precisely the 2026-08-08 loss. Context:"
      sed -n "${ln},$((ln + WINDOW))p" "$f" | sed 's/^/        /'
    fi
  done < <(ack_calls "$f")
done

echo
if [[ "$fail" -ne 0 ]]; then echo "CUT-GATE: ✗ FAILED"; exit 1; fi
echo "CUT-GATE: ✓ PASSED"
