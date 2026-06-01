#!/usr/bin/env bash
#
# run_prod.sh — S165 (created) / S167 (hardened) — PRODUCTION launcher.
#
# Compiles the `aberp` backend AND the `aberp-ui` Tauri shell with
# `--features production` — the COMPILE-TIME switch that:
#   - flips the NAV endpoint to api.onlineszamla.nav.gov.hu
#   - lifts the dev-build refusal gate (assert_endpoint_allowed)
#   - drops the `TEST-` invoice-number prefix
#   - arms the loud red boot banner
#   - enforces the seller.toml identity sanity check (tax 24904362-2-41)
#
# The three hülye-biztos guards below make a wrong-environment launch
# structurally impossible:
#   #1: ABERP_TENANT is forced to `prod` (matches build_profile.rs).
#   #2: ABERP_DB is forced to ~/.aberp/prod/aberp.duckdb (per-tenant
#       data isolation — dev DBs are physically separate files).
#   #3: cargo build/run pass `--features production`. A binary built
#       without it physically cannot reach the prod NAV endpoint
#       (see apps/aberp/src/build_profile.rs).
#
# What's S167-new vs the original S165 one-liner:
#   - Pre-flight: warn (don't fail) if ~/.aberp/prod/seller.toml is
#     missing. The boot path drives a NeedsSellerConfig wizard either
#     way, but the operator should know what's coming.
#   - Pre-flight: warn (don't fail) on missing first-launch touchfile.
#     The SPA's first-prod-launch modal handles it, but a heads-up
#     is friendlier than a surprise.
#   - Banner: explicit "PRODUCTION BUILD — real NAV, real money,
#     tenant=prod" line before cargo starts. Bilingual EN+HU.
#
# Anything still NOT in scope here:
#   - codesigning / notarising the binary (linker-adhoc is what we use;
#     see run_desktop.sh PR-52 note for the rationale).
#   - tagging the release. That's release.sh's job.
#
# Usage:
#   ./run/run_prod.sh
#   ./run/run_prod.sh --help

set -euo pipefail

# ---------- self-syntax-check (mirrors run_desktop.sh PR-55) ----------------
if ! bash -n "$0" 2>/dev/null; then
  echo "[fail] $0 failed 'bash -n' syntax check — refusing to run" >&2
  bash -n "$0"
  exit 2
fi

readonly REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly PROD_TENANT="prod"
readonly PROD_HOME="${HOME}/.aberp/${PROD_TENANT}"
readonly PROD_SELLER_TOML="${PROD_HOME}/seller.toml"
readonly PROD_FIRST_LAUNCH_TOUCHFILE="${PROD_HOME}/.first-launch-acknowledged"
readonly PROD_DB="${PROD_HOME}/aberp.duckdb"

# Operator-facing constant — single source of truth so a future endpoint
# change updates this banner too. Mirrors what nav_endpoint_base_url()
# returns when IS_PRODUCTION_BUILD is true; if they diverge, that's a
# bug in this script, not in the binary (the binary IS truth).
readonly PROD_NAV_HOST="api.onlineszamla.nav.gov.hu"

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
    --help|-h) sed -n '2,42p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "${c_red}[fail]${c_rst} unknown flag: $1" >&2; exit 2 ;;
  esac
done

# ---------- hülye-biztos guard #1: tenant=prod ------------------------------
export ABERP_TENANT="$PROD_TENANT"

# ---------- hülye-biztos guard #2: explicit prod paths ----------------------
export ABERP_DB="$PROD_DB"
mkdir -p "$PROD_HOME"

# ---------- pre-flight: heads-up advisories (NOT fatal) ---------------------
# The Rust boot path (serve::sanity_check_environment in S166) is the
# authoritative gate. These warn-only checks just give the operator
# situational awareness so a wizard-prompt isn't a surprise.
if [[ ! -f "$PROD_SELLER_TOML" ]]; then
  echo "${c_yel}[heads-up]${c_rst} ${PROD_SELLER_TOML} is missing." >&2
  echo "           The first launch will route you through the seller-config wizard." >&2
  echo "${c_yel}[figyelem]${c_rst} A ${PROD_SELLER_TOML} fájl hiányzik." >&2
  echo "           Az első indításnál az eladó-beállító varázsló jelenik meg." >&2
fi

if [[ ! -f "$PROD_FIRST_LAUNCH_TOUCHFILE" ]]; then
  echo "${c_yel}[heads-up]${c_rst} First-launch ceremony has not been completed yet." >&2
  echo "           The SPA will block all main routes behind a confirmation modal." >&2
  echo "           You will need to type ${c_yel}ABERP${c_rst} (uppercase, exact) to proceed." >&2
  echo "${c_yel}[figyelem]${c_rst} Az első éles indítás ceremónia még nem zárult le." >&2
  echo "           A program egy megerősítő ablakot mutat — gépeld be: ${c_yel}ABERP${c_rst}" >&2
fi

# ---------- the loud banner -------------------------------------------------
# Operator-facing situational-awareness — the binary's own banner (in
# print_boot_banner, serve.rs:204) prints AFTER cargo finishes building.
# This one prints BEFORE, so the operator knows what they're about to
# launch without having to wait through the compile.
echo
echo "${c_red}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${c_rst}" >&2
echo "${c_red}  ⚠️  PRODUCTION BUILD — REAL NAV — REAL MONEY${c_rst}" >&2
echo "${c_yel}      ÉLES UZEM — VALÓDI NAV — VALÓDI PÉNZ${c_rst}" >&2
echo "${c_red}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${c_rst}" >&2
echo "${c_red}  tenant=${PROD_TENANT}${c_rst}" >&2
echo "${c_red}  NAV endpoint: https://${PROD_NAV_HOST}/invoiceService/v3/${c_rst}" >&2
echo "${c_red}  DB:           ${PROD_DB}${c_rst}" >&2
echo "${c_red}  seller.toml:  ${PROD_SELLER_TOML}${c_rst}" >&2
echo "${c_red}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${c_rst}" >&2
echo >&2

# ---------- pre-flight: Frankenstein-build refusal (S200 / PR-200) ----------
# Refuses to compile-and-launch from a tree the operator has hand-edited
# (uncommitted changes) OR a HEAD that doesn't match any origin/PROD_v*
# release branch. Both checks together prevent the failure mode where
# Ervin's recovery dance (cp'ing files to "fix" a prior cutover failure)
# leaves the tree dirty AND no longer pointed at a known release ref —
# the resulting binary is a one-off Frankenstein with no provenance.
#
# Opt-out: ABERP_SKIP_GIT_CHECK=1 (for dev workflows that intentionally
# launch from main or a feature branch — never for real prod).
if [[ "${ABERP_SKIP_GIT_CHECK:-0}" != "1" ]]; then
  if [[ -n "$(git -C "$REPO_ROOT" status --porcelain 2>/dev/null)" ]]; then
    echo "${c_red}❌ Working tree has uncommitted changes — refusing to launch a Frankenstein build.${c_rst}" >&2
    echo "${c_red}   A munkakönyvtár nincs tiszta — Frankenstein-bináris indítása megtagadva.${c_rst}" >&2
    echo >&2
    echo "   Recommended: ./run/upgrade_prod.sh <PROD_vX.Y>  (single-command clean upgrade)" >&2
    echo "   Manual:      git status, then git reset --hard origin/<your-branch> + git clean -fd" >&2
    echo "   Bypass:      ABERP_SKIP_GIT_CHECK=1 ./run/run_prod.sh  (dev workflows only)" >&2
    echo >&2
    exit 1
  fi

  # Confirm HEAD matches some origin/PROD_v* branch tip. A detached HEAD
  # or a feature branch reaching here means the operator launched
  # something other than a published release — refuse.
  local_head="$(git -C "$REPO_ROOT" rev-parse HEAD 2>/dev/null)"
  matched_branch=""
  while IFS= read -r remote_branch; do
    [[ -z "$remote_branch" ]] && continue
    remote_head="$(git -C "$REPO_ROOT" rev-parse "$remote_branch" 2>/dev/null || true)"
    if [[ -n "$remote_head" && "$local_head" == "$remote_head" ]]; then
      matched_branch="$remote_branch"
      break
    fi
  done < <(git -C "$REPO_ROOT" for-each-ref --format='%(refname:short)' refs/remotes/origin/PROD_v* 2>/dev/null || true)

  if [[ -z "$matched_branch" ]]; then
    echo "${c_red}❌ HEAD doesn't match any origin/PROD_v* branch — refusing to launch.${c_rst}" >&2
    echo "${c_red}   A HEAD nem egyezik egyik origin/PROD_v* branch-csel sem — indítás megtagadva.${c_rst}" >&2
    echo >&2
    echo "   HEAD is at: $(git -C "$REPO_ROOT" rev-parse --short HEAD 2>/dev/null || echo '?')" >&2
    echo >&2
    echo "   Recommended: ./run/upgrade_prod.sh <PROD_vX.Y>  (single-command clean upgrade)" >&2
    echo "   Bypass:      ABERP_SKIP_GIT_CHECK=1 ./run/run_prod.sh  (dev workflows only)" >&2
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
# Defense-in-depth: the S169 release binary embeds the SPA via
# tauri/custom-protocol and does NOT touch :5173 at runtime. But Ervin
# may have a leftover `npm run dev` from the 2026-05-30 workaround when
# the binary still needed Vite. A zombie Vite on :5173 is harmless to
# the new binary, but it's dangling state worth clearing before launch.
# lsof ships preinstalled on macOS.
if lsof -ti :5173 >/dev/null 2>&1; then
  stale_pid="$(lsof -ti :5173)"
  echo "${c_yel}[stale] port 5173 in use by pid ${stale_pid} (likely Vite from earlier workaround). Killing.${c_rst}" >&2
  echo "${c_yel}[ragadt] az 5173-as port pid=${stale_pid} kezeli (valószínűleg régi Vite). Kilövöm.${c_rst}" >&2
  kill "$stale_pid" 2>/dev/null || true
  sleep 1
  if lsof -ti :5173 >/dev/null 2>&1; then
    echo "${c_red}[fail] port 5173 still in use after kill — investigate manually: lsof -i :5173${c_rst}" >&2
    echo "${c_red}[hiba] az 5173-as port a kill után is foglalt — nézz utána kézzel: lsof -i :5173${c_rst}" >&2
    exit 1
  fi
  echo "${c_grn}[ ok ] port 5173 freed${c_rst}" >&2
fi

# ---------- hülye-biztos guard #3: compile + run with production feature ---
# Build BOTH binaries: the Tauri shell (aberp-ui) is what the operator
# launches; it spawns the aberp CLI as a subprocess for serve/keychain
# reads. Both must be built with --features production so the compile-
# time gate covers both halves of the process group.
cd "$REPO_ROOT" || { echo "${c_red}[fail]${c_rst} repo not at $REPO_ROOT" >&2; exit 2; }

# S169 / PR-169 — build the SPA into ui/dist BEFORE cargo build.
# tauri::generate_context!() embeds frontendDist (= apps/aberp-ui/ui/dist)
# at compile time. ui/dist is gitignored, so a fresh prod clone has no
# built SPA; raw `cargo build` would link an aberp-ui binary with empty
# embedded assets, and the tauri:// scheme handler would 404 — at which
# point the WebView falls back to devUrl and the operator sees a blank
# window. The SPA build below + custom-protocol feature on the tauri
# dep (apps/aberp-ui/Cargo.toml) together make the release binary
# self-contained (no Vite needed).
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
  echo "        the Tauri binary will embed nothing and fall back to devUrl." >&2
  echo "${c_red}[hiba]${c_rst} A SPA build nem hozta létre a $UI_DIST/index.html fájlt;" >&2
  echo "        a Tauri bináris üresen embedelne, a WebView devUrl-re esne vissza." >&2
  exit 4
fi
echo "${c_grn}[ ok ]${c_rst} SPA built; $UI_DIST/index.html present" >&2

echo "${c_dim}[build] cargo build --release --features production --bin aberp${c_rst}" >&2
cargo build --release --features production --bin aberp

echo "${c_dim}[build] cargo build --release --features production --bin aberp-ui${c_rst}" >&2
cargo build --release --features production --bin aberp-ui

echo
echo "${c_grn}[launch]${c_rst} starting ABERP in PRODUCTION mode..." >&2
echo "${c_grn}[launch]${c_rst} (Ctrl-C in this terminal exits the app gracefully.)" >&2
echo >&2

ABERP_TENANT="$PROD_TENANT" ABERP_DB="$PROD_DB" \
  cargo run --release --features production --bin aberp-ui
