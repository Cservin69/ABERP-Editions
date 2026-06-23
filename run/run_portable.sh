#!/usr/bin/env bash
#
# run_portable.sh — S435 — PORTABLE launcher (the NAV-off product line).
#
# The Portable sibling of run_prod.sh. Where run_prod.sh compiles
# `--features production` and forces tenant=prod (real NAV, real money,
# the loud red banner), this launcher does the opposite on purpose:
#
#   - Builds WITHOUT `--features production` (a dev-profile binary). The
#     compile-time gate in apps/aberp/src/build_profile.rs keeps that
#     binary physically incapable of touching the real NAV endpoint, and
#     `guard_tenant_matches_build` (serve.rs) ALLOWS non-prod tenants on
#     a non-production build — so the demo/international tenant boots
#     cleanly here while still being refused under run_prod.sh.
#   - Defaults to tenant=demo (override with ABERP_TENANT=<name> for a
#     real international tenant). The bundled demo tenant is NAV-off, so
#     the binary skips the keychain + §169 seller gate and boots straight
#     to Ready (a dashboard, not the first-run wizard).
#   - Shows a friendly green/yellow "no NAV, local-only" banner — there
#     is no real-money path to warn about.
#
# Hülye-biztos guards that still apply (mirrors run_prod.sh):
#   #1: ABERP_TENANT is exported (demo by default) and the per-tenant
#       DB/seller paths are derived from it — data isolation per tenant.
#   #2: the Frankenstein-build refusal accepts only an origin/PROD_Portable_v*
#       release tip (run_prod.sh accepts origin/PROD_v*). A dirty tree or a
#       HEAD that matches no published Portable release is refused.
#
# Usage:
#   ./run/run_portable.sh
#   ABERP_TENANT=acme ./run/run_portable.sh
#   ./run/run_portable.sh --help

set -euo pipefail

# ---------- self-syntax-check (mirrors run_prod.sh PR-55) -------------------
if ! bash -n "$0" 2>/dev/null; then
  echo "[fail] $0 failed 'bash -n' syntax check — refusing to run" >&2
  bash -n "$0"
  exit 2
fi

readonly REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# ---------- tenant resolution: demo by default, env-overridable ------------
# Portable lands in the bundled demo tenant unless the operator points it
# at a real international tenant (ABERP_TENANT=acme). Either way the binary
# is a dev-profile build, so guard_tenant_matches_build allows it — only
# tenant=prod would be refused (that is run_prod.sh's job).
readonly PORTABLE_TENANT="${ABERP_TENANT:-demo}"
readonly PORTABLE_HOME="${HOME}/.aberp-portable/${PORTABLE_TENANT}"
readonly PORTABLE_SELLER_TOML="${PORTABLE_HOME}/seller.toml"
readonly PORTABLE_DB="${PORTABLE_HOME}/aberp.duckdb"

# ---------- colour helpers (no-op when stderr is not a terminal) ------------
if [[ -t 2 && -z "${NO_COLOR:-}" ]]; then
  c_red=$'\033[1;31m'; c_yel=$'\033[1;33m'; c_grn=$'\033[1;32m'
  c_dim=$'\033[2m';    c_rst=$'\033[0m'
else
  c_red=""; c_yel=""; c_grn=""; c_dim=""; c_rst=""
fi

# ---------- arg parsing -----------------------------------------------------
while [[ $# -gt 0 ]]; do
  case "$1" in
    --help|-h) sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "${c_red}[fail]${c_rst} unknown flag: $1" >&2; exit 2 ;;
  esac
done

# ---------- hülye-biztos guard #1: tenant + explicit per-tenant paths -------
export ABERP_TENANT="$PORTABLE_TENANT"
export ABERP_DB="$PORTABLE_DB"
mkdir -p "$PORTABLE_HOME"

# ---------- pre-flight: heads-up advisory (NOT fatal) -----------------------
# In NAV-off mode seller.toml is optional — the binary boots Ready without
# it. A missing file is worth a friendly note, not the prod seller-wizard
# warning (there is no §169 gate to satisfy here).
if [[ ! -f "$PORTABLE_SELLER_TOML" ]]; then
  echo "${c_dim}[info]${c_rst} ${PORTABLE_SELLER_TOML} not present yet — optional in NAV-off mode." >&2
  echo "${c_dim}[info]${c_rst} A ${PORTABLE_SELLER_TOML} még nincs — NAV nélküli módban nem kötelező." >&2
fi

# ---------- the friendly banner ---------------------------------------------
echo
echo "${c_grn}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${c_rst}" >&2
echo "${c_grn}  🌍  PORTABLE BUILD — NO NAV — local-only operation${c_rst}" >&2
echo "${c_yel}      PORTABLE VÁLTOZAT — NAV NÉLKÜL — helyi működés${c_rst}" >&2
echo "${c_grn}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${c_rst}" >&2
echo "${c_grn}  tenant=${PORTABLE_TENANT}${c_rst}" >&2
echo "${c_grn}  NAV submission: disabled (per-tenant flag)${c_rst}" >&2
echo "${c_grn}  DB:           ${PORTABLE_DB}${c_rst}" >&2
echo "${c_grn}  seller.toml:  ${PORTABLE_SELLER_TOML}  (optional)${c_rst}" >&2
echo "${c_grn}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${c_rst}" >&2
echo >&2

# ---------- pre-flight: Frankenstein-build refusal --------------------------
# Mirror run_prod.sh's S200 guard, but anchored to the Portable release
# line: refuse a dirty tree or a HEAD that matches no published
# origin/PROD_Portable_v* tip. Opt-out: ABERP_SKIP_GIT_CHECK=1 (dev only).
if [[ "${ABERP_SKIP_GIT_CHECK:-0}" != "1" ]]; then
  if [[ -n "$(git -C "$REPO_ROOT" status --porcelain 2>/dev/null)" ]]; then
    echo "${c_red}❌ Working tree has uncommitted changes — refusing to launch a Frankenstein build.${c_rst}" >&2
    echo "${c_red}   A munkakönyvtár nincs tiszta — Frankenstein-bináris indítása megtagadva.${c_rst}" >&2
    echo >&2
    echo "   Recommended: ./run/upgrade_portable.sh <PROD_Portable_vX.Y>  (single-command clean upgrade)" >&2
    echo "   Bypass:      ABERP_SKIP_GIT_CHECK=1 ./run/run_portable.sh  (dev workflows only)" >&2
    echo >&2
    exit 1
  fi

  # Confirm HEAD matches some origin/PROD_Portable_v* branch tip.
  local_head="$(git -C "$REPO_ROOT" rev-parse HEAD 2>/dev/null)"
  matched_branch=""
  while IFS= read -r remote_branch; do
    [[ -z "$remote_branch" ]] && continue
    remote_head="$(git -C "$REPO_ROOT" rev-parse "$remote_branch" 2>/dev/null || true)"
    if [[ -n "$remote_head" && "$local_head" == "$remote_head" ]]; then
      matched_branch="$remote_branch"
      break
    fi
  done < <(git -C "$REPO_ROOT" for-each-ref --format='%(refname:short)' refs/remotes/origin/PROD_Portable_v* 2>/dev/null || true)

  if [[ -z "$matched_branch" ]]; then
    echo "${c_red}❌ HEAD doesn't match any origin/PROD_Portable_v* branch — refusing to launch.${c_rst}" >&2
    echo "${c_red}   A HEAD nem egyezik egyik origin/PROD_Portable_v* branch-csel sem — indítás megtagadva.${c_rst}" >&2
    echo >&2
    echo "   HEAD is at: $(git -C "$REPO_ROOT" rev-parse --short HEAD 2>/dev/null || echo '?')" >&2
    echo >&2
    echo "   Recommended: ./run/upgrade_portable.sh <PROD_Portable_vX.Y>  (single-command clean upgrade)" >&2
    echo "   Bypass:      ABERP_SKIP_GIT_CHECK=1 ./run/run_portable.sh  (dev workflows only)" >&2
    echo >&2
    exit 1
  fi
  echo "${c_grn}✓ HEAD matches ${matched_branch} — safe to launch.${c_rst}" >&2
  echo >&2
else
  echo "${c_yel}[bypass] ABERP_SKIP_GIT_CHECK=1 — skipping Frankenstein-build refusal.${c_rst}" >&2
  echo "${c_yel}[bypass] ABERP_SKIP_GIT_CHECK=1 — Frankenstein-ellenőrzés kikapcsolva.${c_rst}" >&2
  echo >&2
fi

# ---------- pre-flight: free port 5173 (stale-Vite cleanup) ----------------
# Same defense-in-depth as run_prod.sh: the release binary embeds the SPA
# and does not touch :5173 at runtime, but a leftover `npm run dev` is
# dangling state worth clearing.
if lsof -ti :5173 >/dev/null 2>&1; then
  stale_pid="$(lsof -ti :5173)"
  echo "${c_yel}[stale] port 5173 in use by pid ${stale_pid} (likely Vite). Killing.${c_rst}" >&2
  echo "${c_yel}[ragadt] az 5173-as port pid=${stale_pid} kezeli (valószínűleg Vite). Kilövöm.${c_rst}" >&2
  kill "$stale_pid" 2>/dev/null || true
  sleep 1
  if lsof -ti :5173 >/dev/null 2>&1; then
    echo "${c_red}[fail] port 5173 still in use after kill — investigate: lsof -i :5173${c_rst}" >&2
    echo "${c_red}[hiba] az 5173-as port a kill után is foglalt — nézz utána: lsof -i :5173${c_rst}" >&2
    exit 1
  fi
  echo "${c_grn}[ ok ] port 5173 freed${c_rst}" >&2
fi

# ---------- build the SPA + both binaries (DEV profile) --------------------
# Same SPA embed contract as run_prod.sh (tauri::generate_context!() embeds
# apps/aberp-ui/ui/dist at compile time, and ui/dist is gitignored), but
# the cargo invocations OMIT `--features production` — that omission is the
# whole point of the Portable line.
cd "$REPO_ROOT" || { echo "${c_red}[fail]${c_rst} repo not at $REPO_ROOT" >&2; exit 2; }

readonly UI_DIR="${REPO_ROOT}/apps/aberp-ui/ui"
readonly UI_DIST="${UI_DIR}/dist"

echo "${c_dim}[ui] (cd apps/aberp-ui/ui && npm install --silent)${c_rst}" >&2
(cd "$UI_DIR" && npm install --silent) \
  || { echo "${c_red}[fail]${c_rst} npm install in $UI_DIR failed" >&2; exit 4; }

echo "${c_dim}[ui] (cd apps/aberp-ui/ui && npm run build)${c_rst}" >&2
(cd "$UI_DIR" && npm run build) \
  || { echo "${c_red}[fail]${c_rst} npm run build in $UI_DIR failed" >&2; exit 4; }

if [[ ! -s "$UI_DIST/index.html" ]]; then
  echo "${c_red}[fail]${c_rst} SPA build did not produce $UI_DIST/index.html" >&2
  echo "${c_red}[hiba]${c_rst} A SPA build nem hozta létre a $UI_DIST/index.html fájlt." >&2
  exit 4
fi
echo "${c_grn}[ ok ]${c_rst} SPA built; $UI_DIST/index.html present" >&2

echo "${c_dim}[build] cargo build --bin aberp${c_rst}" >&2
cargo build --bin aberp

echo "${c_dim}[build] cargo build --bin aberp-ui${c_rst}" >&2
cargo build --bin aberp-ui

echo
echo "${c_grn}[launch]${c_rst} starting ABERP in PORTABLE mode (tenant=${PORTABLE_TENANT})..." >&2
echo "${c_grn}[launch]${c_rst} (Ctrl-C in this terminal exits the app gracefully.)" >&2
echo >&2

ABERP_TENANT="$PORTABLE_TENANT" ABERP_DB="$PORTABLE_DB" \
  cargo run --bin aberp-ui
