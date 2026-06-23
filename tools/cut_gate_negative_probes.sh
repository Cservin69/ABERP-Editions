#!/usr/bin/env bash
#
# cut_gate_negative_probes.sh — proves tools/cut_gate_db_isolation.sh has TEETH.
#
# For each invariant the cut-gate enforces, plant the corresponding VIOLATION
# in a throwaway COPY of the tree, run the gate against that copy, and assert
# it EXITS NON-ZERO with the matching CHECK's failure message. A green gate is
# only meaningful if it would have gone red on a real regression — this script
# is that proof, and it runs in CI alongside the gate (cut-gate.yml).
#
# The working tree is NEVER mutated; every probe operates on a fresh copy under
# a mktemp dir that is removed on exit.
#
# Exit 0 = every probe behaved (clean copy passes; each violation is caught).
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
GATE="tools/cut_gate_db_isolation.sh"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/cutgate-probes.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT
pass=0; bad=0
i=0

fresh() {  # -> path to a fresh, clean copy of the tree (excludes .git)
  i=$((i+1))
  local d="$WORK/copy.$i"
  mkdir -p "$d"
  tar -C "$ROOT" --exclude=.git -cf - . | tar -C "$d" -xf -
  printf '%s' "$d"
}
gate_rc() {  # run the COPY's gate; echo exit code; stash output in $1/.out
  ( cd "$1" && bash "$GATE" ) >"$1/.out" 2>&1
  echo $?
}

expect_pass() {  # $1 dir  $2 label
  local rc; rc="$(gate_rc "$1")"
  if [[ "$rc" == "0" ]]; then
    printf '  ✓ %s\n' "$2"; pass=$((pass+1))
  else
    printf '  ✗ BROKEN: %s — clean copy should PASS but gate exit=%s\n' "$2" "$rc"
    sed 's/^/        /' "$1/.out"; bad=$((bad+1))
  fi
}
expect_fail() {  # $1 dir  $2 signature  $3 label
  local rc; rc="$(gate_rc "$1")"
  if [[ "$rc" != "0" ]] && grep -qF -- "$2" "$1/.out"; then
    printf '  ✓ caught: %s  (exit=%s; matched: "%s")\n' "$3" "$rc" "$2"; pass=$((pass+1))
  else
    printf '  ✗ ESCAPED: %s  (exit=%s; expected non-zero + "%s")\n' "$3" "$rc" "$2"
    sed 's/^/        /' "$1/.out"; bad=$((bad+1))
  fi
}

echo "negative probes for the ADR-0093/0002 DB-isolation cut-gate"
echo "root: $ROOT"
echo

echo "[sanity] a clean copy passes"
c="$(fresh)"; expect_pass "$c" "clean tree → CUT-GATE PASSED"

echo "[CHECK 1] planting run/run_prod.sh (prod launch surface)"
c="$(fresh)"; printf '#!/usr/bin/env bash\necho prod\n' > "$c/run/run_prod.sh"
expect_fail "$c" "must not carry the prod launcher" "prod launcher re-added"

echo "[CHECK 2] removing SAW-OFF.md (saw-off sentinel)"
c="$(fresh)"; rm -f "$c/SAW-OFF.md"
expect_fail "$c" "SAW-OFF.md missing" "saw-off sentinel removed"

echo "[CHECK 3] launcher resolving prod's DB root ~/.aberp/prod"
c="$(fresh)"; printf '\nDATA_DIR="${HOME}/.aberp/prod/${TENANT}"\n' >> "$c/run/run_portable.sh"
expect_fail "$c" "resolve prod's tenant/DB root" "launcher points back at ~/.aberp/prod"

echo "[CHECK 4] re-introducing the silent-truncate reconcile path"
c="$(fresh)"; printf '\n// regression: let _ = RecoveryAction::Truncated;\n' >> "$c/crates/audit-ledger/src/mirror.rs"
expect_fail "$c" "silent-truncate path" "RecoveryAction::Truncated re-introduced"

echo "[CHECK 5] in-place live-file rewrite (rename(2) -> in-place copy)"
c="$(fresh)"
# Replace the atomic rename swap with an in-place copy (the anti-pattern).
sed -i 's#std::fs::rename(staged, target)#std::fs::copy(staged, target).map(|_| ())#' \
    "$c/crates/aberp-snapshot/src/crash_safe.rs"
expect_fail "$c" "no longer swaps via std::fs::rename" "checkpoint regressed to in-place rewrite"

echo "[CHECK 6] binary source resolving prod's bare snapshot store"
c="$(fresh)"
printf 'pub fn _probe() { let _ = default_store_dir("prod"); }\n' > "$c/apps/aberp/src/zz_probe_violation.rs"
expect_fail "$c" "calls prod-shaped default_store_dir" "binary reaches prod's bare snapshot store"

echo "[CHECK 7] rogue Defense launcher that crosses arms (production -> .aberp-portable)"
c="$(fresh)"
cat > "$c/run/run_defense_rogue.sh" <<'ROGUE'
#!/usr/bin/env bash
# A new "Defense" launcher that builds the production arm but binds the WRONG
# (Portable) root — the exact mismatch CHECK 3b cannot see.
readonly HOME_DIR="${HOME}/.aberp-portable/${TENANT}"
cargo build --release --features production --bin aberp
ROGUE
expect_fail "$c" "binds a non-defense root" "production-arm launcher pointed at .aberp-portable"

echo
echo "probes passed: $pass   broken/escaped: $bad"
if [[ "$bad" -ne 0 ]]; then echo "NEGATIVE-PROBES: ✗ FAILED"; exit 1; fi
echo "NEGATIVE-PROBES: ✓ ALL CHECKS HAVE TEETH"
