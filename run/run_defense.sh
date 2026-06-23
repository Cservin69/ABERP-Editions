#!/usr/bin/env bash
#
# run_defense.sh — S437 — DEFENSE launcher (the defense / aerospace line).
#
# The Defense sibling of run_prod.sh. It is a PRODUCTION-money build in
# every safety-relevant respect — real NAV, real money, the loud red
# banner, the compile-time `--features production` gate — so it MUST
# match run_prod.sh's guarantees one-for-one. The only differences are:
#
#   - Banner says DEFENSE and adds a defense-mode info line (AVL screening
#     + lot/heat traceability + DÁP-ready). The real-money / real-NAV
#     warning is identical in weight to run_prod.sh — this is not a softer
#     mode, it is prod-money-real with defense compliance bolted on.
#   - Default tenant is `defense` (OVERRIDABLE via ABERP_TENANT=<name>),
#     and the DB root is the edition-locked ~/.aberp-defense/<tenant>/ —
#     provably disjoint from prod's frozen ~/.aberp/ root (ADR-0093 §5). The
#     boot-time guard_tenant_matches_build is the authority: BOTH editions
#     refuse the frozen prod line's reserved `prod` tenant, and the data
#     root is compile-time edition-locked, so this build physically cannot
#     open prod's database whatever this launcher sets.
#   - The Frankenstein-build refusal accepts an origin/PROD_Defense_v* tip
#     OR an origin/PROD_v* tip: during the transition a Defense build can
#     still deploy a classic prod release tag.
#
# Same compile-time switch as run_prod.sh (`--features production`) which:
#   - flips the NAV endpoint to api.onlineszamla.nav.gov.hu
#   - lifts the dev-build refusal gate (assert_endpoint_allowed)
#   - drops the `TEST-` invoice-number prefix
#   - arms the loud red boot banner
#   - enforces the seller.toml identity sanity check
#
# DÁP (Digitális Állampolgárság Program) login is NOT wired yet. When it
# lands, the per-tenant `dap_login_enabled` flag would be surfaced here.
# Today the banner says "DÁP integration: pending".
#
# Usage:
#   ./run/run_defense.sh
#   ABERP_TENANT=defense-acme ./run/run_defense.sh
#   ./run/run_defense.sh --help

set -euo pipefail

# ---------- self-syntax-check (mirrors run_prod.sh PR-55) -------------------
if ! bash -n "$0" 2>/dev/null; then
  echo "[fail] $0 failed 'bash -n' syntax check — refusing to run" >&2
  bash -n "$0"
  exit 2
fi

readonly REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# ---------- tenant resolution: prod by default, env-overridable ------------
# Defaults to tenant=defense but lets a defense operator point at another
# tenant via ABERP_TENANT. The DB root is the edition-locked
# ~/.aberp-defense/<tenant>/ (ADR-0093). The binary refuses the reserved
# `prod` tenant at boot (guard_tenant_matches_build) and is compile-time
# bound to ~/.aberp-defense/ — it cannot open prod's frozen database.
readonly DEFENSE_TENANT="${ABERP_TENANT:-defense}"
readonly DEFENSE_HOME="${HOME}/.aberp-defense/${DEFENSE_TENANT}"
readonly DEFENSE_SELLER_TOML="${DEFENSE_HOME}/seller.toml"
readonly DEFENSE_FIRST_LAUNCH_TOUCHFILE="${DEFENSE_HOME}/.first-launch-acknowledged"
readonly DEFENSE_DB="${DEFENSE_HOME}/aberp.duckdb"

# Operator-facing constant — single source of truth, mirrors what
# nav_endpoint_base_url() returns when IS_PRODUCTION_BUILD is true. If they
# diverge that is a bug in this script, not the binary (the binary IS truth).
readonly DEFENSE_NAV_HOST="api.onlineszamla.nav.gov.hu"

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
    --help|-h) sed -n '2,40p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "${c_red}[fail]${c_rst} unknown flag: $1" >&2; exit 2 ;;
  esac
done

# ---------- hülye-biztos guard #1: tenant + explicit per-tenant paths -------
export ABERP_TENANT="$DEFENSE_TENANT"

# ---------- hülye-biztos guard #2: explicit per-tenant paths ----------------
export ABERP_DB="$DEFENSE_DB"
mkdir -p "$DEFENSE_HOME"

# ---------- pre-flight: heads-up advisories (NOT fatal) ---------------------
# The Rust boot path is the authoritative gate. These warn-only checks just
# give the operator situational awareness so a wizard-prompt isn't a surprise.
if [[ ! -f "$DEFENSE_SELLER_TOML" ]]; then
  echo "${c_yel}[heads-up]${c_rst} ${DEFENSE_SELLER_TOML} is missing." >&2
  echo "           The first launch will route you through the seller-config wizard." >&2
  echo "${c_yel}[figyelem]${c_rst} A ${DEFENSE_SELLER_TOML} fájl hiányzik." >&2
  echo "           Az első indításnál az eladó-beállító varázsló jelenik meg." >&2
fi

if [[ ! -f "$DEFENSE_FIRST_LAUNCH_TOUCHFILE" ]]; then
  echo "${c_yel}[heads-up]${c_rst} First-launch ceremony has not been completed yet." >&2
  echo "           The SPA will block all main routes behind a confirmation modal." >&2
  echo "           You will need to type ${c_yel}ABERP${c_rst} (uppercase, exact) to proceed." >&2
  echo "${c_yel}[figyelem]${c_rst} Az első éles indítás ceremónia még nem zárult le." >&2
  echo "           A program egy megerősítő ablakot mutat — gépeld be: ${c_yel}ABERP${c_rst}" >&2
fi

# ---------- the loud banner -------------------------------------------------
# Operator-facing situational-awareness. The binary's own banner prints
# AFTER cargo finishes building; this one prints BEFORE so the operator
# knows what they're launching without waiting through the compile.
echo
echo "${c_red}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${c_rst}" >&2
echo "${c_red}  ⚠️  DEFENSE BUILD — REAL NAV — REAL MONEY${c_rst}" >&2
echo "${c_red}      DEFENSE / AEROSPACE COMPLIANCE${c_rst}" >&2
echo "${c_yel}      VÉDELMI VÁLTOZAT — VALÓDI NAV — VALÓDI PÉNZ${c_rst}" >&2
echo "${c_yel}      VÉDELMI / REPÜLŐIPARI MEGFELELŐSÉG${c_rst}" >&2
echo "${c_red}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${c_rst}" >&2
echo "${c_red}  tenant=${DEFENSE_TENANT}${c_rst}" >&2
echo "${c_red}  NAV endpoint: https://${DEFENSE_NAV_HOST}/invoiceService/v3/${c_rst}" >&2
echo "${c_red}  DB:           ${DEFENSE_DB}${c_rst}" >&2
echo "${c_red}  seller.toml:  ${DEFENSE_SELLER_TOML}${c_rst}" >&2
echo "${c_yel}  DEFENSE MODE: AVL screening + lot/heat traceability + DÁP-ready${c_rst}" >&2
echo "${c_yel}  DÁP integration: pending${c_rst}" >&2
echo "${c_red}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${c_rst}" >&2
echo >&2

# ---------- pre-flight: Frankenstein-build refusal (S200 / PR-200) ----------
# Refuses to compile-and-launch from a hand-edited tree (uncommitted
# changes) OR a HEAD that doesn't match a published release tip. Defense
# accepts BOTH an origin/PROD_Defense_v* tip AND an origin/PROD_v* tip — a
# Defense build can deploy a classic prod release during the transition.
#
# Opt-out: ABERP_SKIP_GIT_CHECK=1 (dev workflows only — never for real prod).
if [[ "${ABERP_SKIP_GIT_CHECK:-0}" != "1" ]]; then
  if [[ -n "$(git -C "$REPO_ROOT" status --porcelain 2>/dev/null)" ]]; then
    echo "${c_red}❌ Working tree has uncommitted changes — refusing to launch a Frankenstein build.${c_rst}" >&2
    echo "${c_red}   A munkakönyvtár nincs tiszta — Frankenstein-bináris indítása megtagadva.${c_rst}" >&2
    echo >&2
    echo "   Recommended: ./run/upgrade_defense.sh <PROD_Defense_vX.Y>  (single-command clean upgrade)" >&2
    echo "   Manual:      git status, then git reset --hard origin/<your-branch> + git clean -fd" >&2
    echo "   Bypass:      ABERP_SKIP_GIT_CHECK=1 ./run/run_defense.sh  (dev workflows only)" >&2
    echo >&2
    exit 1
  fi

  # Confirm HEAD matches some origin/PROD_Defense_v* OR origin/PROD_v* tip.
  # Both globs are passed to for-each-ref; PROD_v* does NOT match
  # PROD_Portable_v* / PROD_Defense_v* (those start with PROD_P / PROD_D).
  local_head="$(git -C "$REPO_ROOT" rev-parse HEAD 2>/dev/null)"
  matched_branch=""
  while IFS= read -r remote_branch; do
    [[ -z "$remote_branch" ]] && continue
    remote_head="$(git -C "$REPO_ROOT" rev-parse "$remote_branch" 2>/dev/null || true)"
    if [[ -n "$remote_head" && "$local_head" == "$remote_head" ]]; then
      matched_branch="$remote_branch"
      break
    fi
  done < <(git -C "$REPO_ROOT" for-each-ref --format='%(refname:short)' \
             refs/remotes/origin/PROD_Defense_v* refs/remotes/origin/PROD_v* 2>/dev/null || true)

  if [[ -z "$matched_branch" ]]; then
    echo "${c_red}❌ HEAD doesn't match any origin/PROD_Defense_v* or origin/PROD_v* branch — refusing to launch.${c_rst}" >&2
    echo "${c_red}   A HEAD nem egyezik egyik origin/PROD_Defense_v* vagy origin/PROD_v* branch-csel sem — indítás megtagadva.${c_rst}" >&2
    echo >&2
    echo "   HEAD is at: $(git -C "$REPO_ROOT" rev-parse --short HEAD 2>/dev/null || echo '?')" >&2
    echo >&2
    echo "   Recommended: ./run/upgrade_defense.sh <PROD_Defense_vX.Y>  (single-command clean upgrade)" >&2
    echo "   Bypass:      ABERP_SKIP_GIT_CHECK=1 ./run/run_defense.sh  (dev workflows only)" >&2
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
# dangling state worth clearing before launch.
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
# Build BOTH binaries with --features production (same as run_prod.sh) so the
# compile-time gate covers both halves of the process group. A binary built
# without it physically cannot reach the prod NAV endpoint.
cd "$REPO_ROOT" || { echo "${c_red}[fail]${c_rst} repo not at $REPO_ROOT" >&2; exit 2; }

# Build the SPA into ui/dist BEFORE cargo build — tauri::generate_context!()
# embeds apps/aberp-ui/ui/dist at compile time, and ui/dist is gitignored.
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
echo "${c_grn}[launch]${c_rst} starting ABERP in DEFENSE mode (tenant=${DEFENSE_TENANT})..." >&2
echo "${c_grn}[launch]${c_rst} (Ctrl-C in this terminal exits the app gracefully.)" >&2
echo >&2

ABERP_TENANT="$DEFENSE_TENANT" ABERP_DB="$DEFENSE_DB" \
  cargo run --release --features production --bin aberp-ui
