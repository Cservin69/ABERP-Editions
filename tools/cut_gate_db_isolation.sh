#!/usr/bin/env bash
#
# cut_gate_db_isolation.sh — ADR-0093 / ADR-0002 DB-isolation cut-gate.
#
# Enforces, at every product-line cut and on every CI run, that the
# sawed-off Portable+Defense editions tree CANNOT drift back into sharing
# prod's tree, launch surface, or database. This is the mechanical
# guardrail behind the cornerstone in FOUNDATION.md §2 ("Database-per-
# tenant. Each tenant owns its own physical store") and ADR-0002, applied
# at the product-line granularity by ADR-0093.
#
# Exit 0 = gate green. Non-zero = a saw-off invariant was violated.
#
# Checks tighten as the saw-off lands chunk by chunk (see SAW-OFF.md):
#   CHECK 1 (chunk 1, ENFORCED) — no prod launch surface in this tree.
#   CHECK 2 (chunk 1, ENFORCED) — saw-off markers present (SAW-OFF.md + ADR-0093).
#   CHECK 3 (chunk 2, currently PENDING/WARN) — no edition resolves prod's
#           DB root (~/.aberp/prod); each edition binds its OWN root.
#           Flip to ENFORCED by exporting ENFORCE_EDITION_DB_BINDING=1
#           once chunk 2 repoints the launchers + boot binding.

set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
fail=0
note() { printf '  %s\n' "$*"; }
echo "ADR-0093 DB-isolation cut-gate — root: $ROOT"

# ── CHECK 1 — no prod launch surface (ENFORCED) ──────────────────────────
echo "[CHECK 1] prod launch surface absent"
for f in run/run_prod.sh run/upgrade_prod.sh; do
  if [[ -e "$f" ]]; then
    note "✗ FAIL: $f exists — the editions tree must not carry the prod launcher."
    fail=1
  else
    note "✓ $f absent"
  fi
done

# ── CHECK 2 — saw-off markers present (ENFORCED) ─────────────────────────
echo "[CHECK 2] saw-off markers present"
[[ -f SAW-OFF.md ]] && note "✓ SAW-OFF.md present" || { note "✗ FAIL: SAW-OFF.md missing (editions-tree sentinel)."; fail=1; }
if ls adr/0093-*.md >/dev/null 2>&1; then note "✓ ADR-0093 present"; else note "✗ FAIL: adr/0093-*.md missing."; fail=1; fi

# ── CHECK 3 — no edition resolves prod's DB root (PENDING → chunk 2) ──────
echo "[CHECK 3] edition DB binding (no ~/.aberp/prod default)"
offenders="$(grep -rnE ':-prod\}|/\.aberp/prod' run/ 2>/dev/null || true)"
if [[ -n "$offenders" ]]; then
  note "launchers still able to resolve prod's tenant/DB root:"
  printf '%s\n' "$offenders" | sed 's/^/      /'
  if [[ "${ENFORCE_EDITION_DB_BINDING:-0}" == "1" ]]; then
    note "✗ FAIL: ENFORCE_EDITION_DB_BINDING=1 and a prod-DB binding remains."
    fail=1
  else
    note "⚠ PENDING (chunk 2): repoint run_defense.sh / run_portable.sh to OWN roots"
    note "  (~/.aberp-defense, ~/.aberp-portable); then set ENFORCE_EDITION_DB_BINDING=1."
  fi
else
  note "✓ no launcher resolves ~/.aberp/prod"
fi

echo
if [[ "$fail" -ne 0 ]]; then echo "CUT-GATE: ✗ FAILED"; exit 1; fi
echo "CUT-GATE: ✓ PASSED"
