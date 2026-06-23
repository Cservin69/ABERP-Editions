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
#   CHECK 4 (chunk 3, ENFORCED) — the edition owns its OWN write/checkpoint
#           path: an edition-scoped, prod-refusing snapshot store; the
#           crash-safe durable checkpoint module (ADR-0082) wired into the
#           snapshot crate + clean shutdown; and reconcile safety (a mirror
#           AHEAD of the DB is preserved + refused, never silently
#           truncated). ENFORCE_CHUNK3_INVARIANTS=0 disables it for a
#           deliberate, temporary local probe only.

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

# ── CHECK 4 — edition own write/checkpoint path (ENFORCED · chunk 3) ──────
# Chunk 3 landed the edition-scoped snapshot/restore + DuckDB write path:
#   (a) snapshots go to an edition-scoped, prod-refusing store;
#   (b) the deferred crash-safe durable-checkpoint fix (ADR-0082) lives in a
#       dedicated module wired into the snapshot crate and clean shutdown;
#   (c) boot reconcile refuses (never silently truncates) a mirror that is
#       AHEAD of the DB, preserving the recovery evidence first.
# This check proves all three stay in place.
echo "[CHECK 4] edition own write/checkpoint path — snapshot store, crash-safe checkpoint, reconcile safety (ENFORCED)"
enforce4="${ENFORCE_CHUNK3_INVARIANTS:-1}"
flag4() { note "$1"; if [[ "$enforce4" == "1" ]]; then fail=1; else note "  (enforcement disabled — not failing)"; fi; }
has() { grep -q -- "$2" "$1" 2>/dev/null; }

# 4a — crash-safe durable checkpoint module present + wired.
cs="crates/aberp-snapshot/src/crash_safe.rs"
if [[ -f "$cs" ]] && has "$cs" 'pub fn durable_checkpoint' && has "$cs" 'fn atomic_install' \
   && has "$cs" 'fn fsync_dir' \
   && has crates/aberp-snapshot/src/lib.rs 'mod crash_safe;' \
   && has crates/aberp-snapshot/src/lib.rs 'durable_checkpoint'; then
  note "✓ crash-safe durable checkpoint module present + exported (atomic rename + fsync file&dir)"
else
  flag4 "✗ crash-safe checkpoint module missing/unwired (crash_safe.rs + lib.rs mod/export)"
fi

# 4b — clean-shutdown durable checkpoint wired into serve.
if has apps/aberp/src/snapshot.rs 'fn checkpoint_on_clean_shutdown' \
   && has apps/aberp/src/serve.rs 'checkpoint_on_clean_shutdown('; then
  note "✓ clean-shutdown durable checkpoint wired (snapshot.rs + serve.rs)"
else
  flag4 "✗ clean-shutdown checkpoint not wired into serve"
fi

# 4c — snapshot store is edition-scoped + prod-refusing.
if has crates/aberp-snapshot/src/store.rs 'pub fn edition_store_dir' \
   && has crates/aberp-snapshot/src/take.rs 'pub fn ensure_not_prod_path' \
   && has apps/aberp/src/snapshot.rs 'edition_store_segment()' \
   && has apps/aberp/src/snapshot.rs 'ensure_not_prod_path'; then
  note "✓ snapshot store edition-scoped + prod-refusing (edition_store_dir + ensure_not_prod_path)"
else
  flag4 "✗ snapshot store not edition-scoped/prod-refusing"
fi

# 4d — the binary's store resolver no longer reaches prod's bare store.
if has apps/aberp/src/snapshot.rs 'default_store_dir'; then
  flag4 "✗ snapshot.rs still calls default_store_dir (prod-shaped store) — must use edition_store_dir"
else
  note "✓ binary store resolver uses only the edition-scoped store (no default_store_dir)"
fi

# 4e — reconcile safety: ahead mirror preserved + refused, never truncated.
mir="crates/audit-ledger/src/mirror.rs"
if grep -q 'RecoveryAction::Truncated' "$mir" 2>/dev/null; then
  flag4 "✗ mirror.rs still has the silent-truncate path (RecoveryAction::Truncated)"
elif has crates/audit-ledger/src/error.rs 'MirrorAheadOfDb' \
     && has "$mir" 'fn preserve_ahead_mirror' \
     && has apps/aberp/src/serve.rs 'MirrorAheadOfDb'; then
  note "✓ reconcile safety: ahead mirror preserved + refused (MirrorAheadOfDb), boot refuses; no auto-truncate"
else
  flag4 "✗ reconcile safety incomplete (need MirrorAheadOfDb + preserve_ahead_mirror + serve refuse, and NO Truncated)"
fi

echo
if [[ "$fail" -ne 0 ]]; then echo "CUT-GATE: ✗ FAILED"; exit 1; fi
echo "CUT-GATE: ✓ PASSED"
