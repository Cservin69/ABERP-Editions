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
#   CHECK 3 (chunk 2, ENFORCED) — each edition binds its OWN ~/.aberp-<ed>/
#           root at compile time; no launcher or source resolver reaches
#           prod's ~/.aberp/prod. Enforced by default;
#           ENFORCE_EDITION_DB_BINDING=0 disables it for a deliberate,
#           temporary local probe only.

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

# ── CHECK 3 — edition DB binding (ENFORCED · chunk 2) ────────────────────
# The ADR-0093 build-locked binding has landed: each edition resolves its
# OWN ~/.aberp-<edition>/ root from a COMPILE-TIME constant
# (build_profile::EDITION) and physically refuses prod's or the sibling's
# root. This gate proves the binding stays in place, four ways.
echo "[CHECK 3] edition DB binding — own-root, no ~/.aberp/prod (ENFORCED)"
enforce="${ENFORCE_EDITION_DB_BINDING:-1}"
flag() {  # $1 = message; trips the gate iff enforcement is on
  note "$1"
  if [[ "$enforce" == "1" ]]; then fail=1; else note "  (enforcement disabled — not failing)"; fi
}

# 3a — no launcher resolves prod's tenant/DB root (ignore comment lines).
offenders="$(grep -rnE ':-prod\}|/\.aberp/prod' run/ 2>/dev/null | grep -vE '^[^:]+:[0-9]+:[[:space:]]*#' || true)"
if [[ -n "$offenders" ]]; then
  flag "✗ launcher(s) still resolve prod's tenant/DB root:"
  printf '%s\n' "$offenders" | sed 's/^/      /'
else
  note "✓ no launcher resolves ~/.aberp/prod"
fi

# 3b — each edition launcher binds its OWN sibling root (positive proof).
check_own_root() {  # $1 launcher  $2 expected root dir
  if [[ ! -f "$1" ]]; then flag "✗ missing launcher: $1"; return; fi
  if grep -qF -- "$2" "$1"; then note "✓ $(basename "$1") binds $2"; else flag "✗ $(basename "$1") does not bind its own root ($2)"; fi
}
check_own_root run/run_defense.sh      ".aberp-defense"
check_own_root run/upgrade_defense.sh  ".aberp-defense"
check_own_root run/run_portable.sh     ".aberp-portable"
check_own_root run/upgrade_portable.sh ".aberp-portable"

# 3c — compile-time Edition→root binding present in the source of truth.
bp="apps/aberp/src/build_profile.rs"
if [[ -f "$bp" ]] && grep -q 'pub enum Edition' "$bp" && grep -q 'EDITION_DATA_DIRNAME' "$bp" \
   && grep -qF 'assert!(!matches!(EDITION, Edition::Prod))' "$bp"; then
  note "✓ compile-time Edition binding present (build_profile.rs)"
else
  flag "✗ build_profile.rs missing the compile-time Edition→root binding"
fi

# 3d — no Rust resolver reconstructs prod's base root ~/.aberp/ directly;
#      every per-tenant path must derive from build_profile::edition_data_dirname.
src_offenders="$(grep -rnE '\.join\("\.aberp"\)|format!\("\{home\}/\.aberp/' apps/aberp/src 2>/dev/null || true)"
if [[ -n "$src_offenders" ]]; then
  flag "✗ source still resolves prod's base root ~/.aberp/ directly:"
  printf '%s\n' "$src_offenders" | sed 's/^/      /'
else
  note "✓ no source resolver reconstructs ~/.aberp/ (all via edition_data_dirname)"
fi

echo
if [[ "$fail" -ne 0 ]]; then echo "CUT-GATE: ✗ FAILED"; exit 1; fi
echo "CUT-GATE: ✓ PASSED"
