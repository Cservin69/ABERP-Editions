#!/usr/bin/env bash
#
# run_defense_demo.sh — launch the SEEDED Defense demo tenant.
#
# The sibling of run_defense.sh, and deliberately NOT the same thing:
#
#   - It runs as the bundled `demo` tenant, never `defense`. The tenant is
#     hard-coded, not env-overridable — pointing a demo launcher at a real
#     tenant is exactly the accident this script exists to prevent.
#   - The `demo` registry row is NAV-off, so even though this is a
#     `--features production` (Defense edition) binary — which it must be, or
#     the Defense-only screens are compiled out — it physically cannot submit
#     an invoice to real NAV, and it skips the keychain + §169 seller gate at
#     boot instead of landing in the setup wizard.
#   - It runs `aberp demo-seed` first, which writes one coherent aerospace job
#     into the demo tenant's own database (idempotent: a second run is free).
#   - It does NOT enforce the Frankenstein-build refusal. A demo is given from
#     a branch, and this launcher cannot reach a real tenant's data, so the
#     release-tip check that guards run_defense.sh would only get in the way.
#     If you want the demo on the release cut, check the release branch out
#     first — the script does not care which commit it builds.
#
# Data root: ~/.aberp-defense/demo/  (edition-locked at compile time —
# ADR-0093 — so this binary cannot open ~/.aberp/ or ~/.aberp-portable/).
#
# Usage:
#   ./run/run_defense_demo.sh
#   ./run/run_defense_demo.sh --seed-only    # seed, print the summary, exit
#   ./run/run_defense_demo.sh --help

set -euo pipefail

if ! bash -n "$0" 2>/dev/null; then
  echo "[fail] $0 failed 'bash -n' syntax check — refusing to run" >&2
  bash -n "$0"
  exit 2
fi

readonly REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# ---------- the tenant is NOT negotiable -----------------------------------
# `aberp demo-seed` refuses any slug but `demo` anyway; this is the launcher
# half of the same guarantee, so a stray ABERP_TENANT in the operator's shell
# cannot silently retarget the demo at a real tenant.
readonly DEMO_TENANT="demo"
if [[ -n "${ABERP_TENANT:-}" && "${ABERP_TENANT}" != "$DEMO_TENANT" ]]; then
  echo "[fail] ABERP_TENANT=${ABERP_TENANT} is set, but this launcher only runs the" >&2
  echo "       bundled '${DEMO_TENANT}' tenant. Unset it, or use ./run/run_defense.sh." >&2
  exit 2
fi
readonly DEMO_HOME="${HOME}/.aberp-defense/${DEMO_TENANT}"
readonly DEMO_DB="${DEMO_HOME}/aberp.duckdb"

if [[ -t 2 && -z "${NO_COLOR:-}" ]]; then
  c_grn=$'\033[1;32m'; c_yel=$'\033[1;33m'; c_cyn=$'\033[1;36m'
  c_dim=$'\033[2m';    c_rst=$'\033[0m'
else
  c_grn=""; c_yel=""; c_cyn=""; c_dim=""; c_rst=""
fi

seed_only=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --help|-h) sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    --seed-only) seed_only=1; shift ;;
    *) echo "[fail] unknown flag: $1" >&2; exit 2 ;;
  esac
done

echo
echo "${c_cyn}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${c_rst}" >&2
echo "${c_cyn}  DEFENSE DEMO — seeded sample data, NAV OFF${c_rst}" >&2
echo "${c_yel}  VÉDELMI DEMÓ — mintaadatok, NAV KIKAPCSOLVA${c_rst}" >&2
echo "${c_cyn}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${c_rst}" >&2
echo "  tenant=${DEMO_TENANT}   (the bundled demo tenant — not a real one)" >&2
echo "  DB:    ${DEMO_DB}" >&2
echo "  Edition: Defense (--features production) — AVL, QC reporting," >&2
echo "           part marking and the shipment gates are all live." >&2
echo "  NAV:     OFF for this tenant. Invoices stay LOCAL ONLY." >&2
echo "${c_cyn}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${c_rst}" >&2
echo >&2

cd "$REPO_ROOT" || { echo "[fail] repo not at $REPO_ROOT" >&2; exit 2; }
mkdir -p "$DEMO_HOME"

# ---------- free a stale Vite (same defence-in-depth as run_defense.sh) ----
if lsof -ti :5173 >/dev/null 2>&1; then
  stale_pid="$(lsof -ti :5173)"
  echo "${c_yel}[stale]${c_rst} port 5173 in use by pid ${stale_pid} — killing." >&2
  kill "$stale_pid" 2>/dev/null || true
  sleep 1
fi

# ---------- the SPA bundle (embedded into the Tauri binary at compile) -----
if [[ "$seed_only" -eq 0 ]]; then
  readonly UI_DIR="${REPO_ROOT}/apps/aberp-ui/ui"
  echo "${c_dim}[ui] (cd apps/aberp-ui/ui && npm install --silent)${c_rst}" >&2
  (cd "$UI_DIR" && npm install --silent) \
    || { echo "[fail] npm install in $UI_DIR failed" >&2; exit 4; }
  echo "${c_dim}[ui] (cd apps/aberp-ui/ui && npm run build)${c_rst}" >&2
  (cd "$UI_DIR" && npm run build) \
    || { echo "[fail] npm run build in $UI_DIR failed" >&2; exit 4; }
  if [[ ! -s "${UI_DIR}/dist/index.html" ]]; then
    echo "[fail] SPA build did not produce ${UI_DIR}/dist/index.html" >&2
    exit 4
  fi
  echo "${c_grn}[ ok ]${c_rst} SPA built" >&2
fi

# ---------- build + seed ---------------------------------------------------
echo "${c_dim}[build] cargo build --release --features production --bin aberp${c_rst}" >&2
cargo build --release --features production --bin aberp

echo "${c_dim}[seed] aberp demo-seed --tenant ${DEMO_TENANT}${c_rst}" >&2
"${REPO_ROOT}/target/release/aberp" demo-seed --tenant "$DEMO_TENANT"

if [[ "$seed_only" -eq 1 ]]; then
  echo >&2
  echo "${c_grn}[done]${c_rst} seeded; --seed-only requested, not launching the app." >&2
  exit 0
fi

echo "${c_dim}[build] cargo build --release --features production --bin aberp-ui${c_rst}" >&2
cargo build --release --features production --bin aberp-ui

echo >&2
echo "${c_grn}[launch]${c_rst} starting ABERP in DEFENSE DEMO mode (tenant=${DEMO_TENANT})..." >&2
echo "${c_grn}[launch]${c_rst} (Ctrl-C in this terminal exits the app gracefully.)" >&2
echo >&2

ABERP_TENANT="$DEMO_TENANT" ABERP_DB="$DEMO_DB" \
  cargo run --release --features production --bin aberp-ui
