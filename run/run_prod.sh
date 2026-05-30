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

readonly REPO_ROOT="/Users/aben/Documents/Claude/Projects/ABERP"
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

# ---------- hülye-biztos guard #3: compile + run with production feature ---
# Build BOTH binaries: the Tauri shell (aberp-ui) is what the operator
# launches; it spawns the aberp CLI as a subprocess for serve/keychain
# reads. Both must be built with --features production so the compile-
# time gate covers both halves of the process group.
cd "$REPO_ROOT" || { echo "${c_red}[fail]${c_rst} repo not at $REPO_ROOT" >&2; exit 2; }

echo "${c_dim}[build] cargo build --release --features production --bin aberp${c_rst}" >&2
cargo build --release --features production --bin aberp

echo "${c_dim}[build] cargo build --release --features production --bin aberp-ui${c_rst}" >&2
cargo build --release --bin aberp-ui

echo
echo "${c_grn}[launch]${c_rst} starting ABERP in PRODUCTION mode..." >&2
echo "${c_grn}[launch]${c_rst} (Ctrl-C in this terminal exits the app gracefully.)" >&2
echo >&2

ABERP_TENANT="$PROD_TENANT" ABERP_DB="$PROD_DB" \
  cargo run --release --features production --bin aberp-ui
