#!/usr/bin/env bash
#
# upgrade_prod.sh — S200 / PR-200 — one-command, hülye-biztos prod upgrade.
#
# Replaces the manual `git fetch` + `git checkout PROD_vX.Y` + relaunch
# sequence that Ervin hit three times across PROD_v1.0 → v1.4 upgrades.
# The bare `git checkout` refuses on dirty working tree, and the recovery
# dance (operator `cp`s files to "fix" prior failures) keeps the tree
# dirty, perpetuating the failure. This script makes the whole sequence
# operator-error-proof:
#
#   ./run/upgrade_prod.sh PROD_v1.5
#
# What it does (in order):
#   1. Validates the version arg (`PROD_v<MAJOR>.<MINOR>` regex).
#   2. Refuses if run from the dev workspace (matches release.sh's
#      dev-sentinel). Opt-out: ABERP_ALLOW_DEV_WORKSPACE=1.
#   3. Verifies origin remote works + the release branch exists on it.
#   4. Verifies the tenant directory + seller.toml exist.
#   5. Refuses if the prod aberp-ui / aberp binary is still running
#      (operator must Ctrl-C the run_prod.sh terminal first — do NOT
#      try to swap a running binary).
#   6. Runs ./tools/snapshot-prod.sh <tenant> (the BEFORE-not-after
#      snapshot the runbook Step 9 warns about — see PROD_v1.1→v1.3
#      lessons in CUTOVER_RUNBOOK.md).
#   7. Full clean git switch:
#        git fetch
#        git reset --hard HEAD           # drop tracked mods
#        git clean -fd                    # drop untracked
#        git checkout -B <branch> origin/<branch>  # create/reset + switch
#        git reset --hard origin/<branch> # belt-and-suspenders
#        git branch -D <every other PROD_v*>  # prune stale local branches
#   8. Verifies clean state + HEAD matches origin/<branch>.
#   9. exec ./run/run_prod.sh — transfers control; one terminal,
#      continuous output.
#
# Forks chosen (per session-200 brief — "pick conservative + flag"):
#   - Default tenant is `prod` (matches snapshot-prod.sh + run_prod.sh).
#     Override with second positional arg: `./run/upgrade_prod.sh PROD_v1.5 dev`
#   - exec-into-run_prod.sh (not background-spawn) — single terminal.
#   - Strict dev-workspace refusal unless ABERP_ALLOW_DEV_WORKSPACE=1.
#   - Stale local PROD_v* branches are pruned (kept: target branch only).
#     Local `main` is left alone (operator may want it for git pull).
#
# What this script does NOT do:
#   - Does NOT create release branches (that's release.sh).
#   - Does NOT push anything (it's a pull-side / switch-side tool).
#   - Does NOT touch the macOS keychain (snapshot-prod.sh handles that).
#   - Does NOT delete local `main` (operator may want it for `git pull`
#     to inspect upstream).
#
# Exit codes:
#   0  success — exec'd into run_prod.sh (so this script's PID is gone)
#   2  arg / preflight failure (bad version, branch not on origin,
#      missing tenant, dev workspace, running binary, etc.)
#   3  snapshot-prod.sh failed
#   4  git switch step failed
#   5  HEAD / branch verify failed after switch
#
# Usage:
#   ./run/upgrade_prod.sh PROD_v1.5
#   ./run/upgrade_prod.sh PROD_v1.5 prod
#   ./run/upgrade_prod.sh --help

set -euo pipefail

# ---------- pure helper: which of THIS checkout's prod binaries run? --------
# S400 — emits "  <proc>: <pids...>\n" lines for every aberp-ui / aberp
# process whose command line is THIS checkout's RELEASE binary; empty output
# means none. Scoped to "$repo_root/target/release/..." on purpose: the old
# `pgrep -x <name>` matched the process NAME only, so a dev/test build from
# ANOTHER checkout (Ervin's ~/Documents/.../ABERP), or this checkout's own
# target/debug, tripped the refusal even though no prod binary was running
# (the S399 Class-A false positive). The `( |$)` anchor stops the `aberp`
# pattern from also swallowing `aberp-ui`'s command line, so the report is
# exact (one line per real process).
running_prod_pids() {
  local repo_root="$1" proc pids out=""
  for proc in aberp-ui aberp; do
    if pids="$(pgrep -f "${repo_root}/target/release/${proc}"'( |$)' 2>/dev/null)"; then
      if [[ -n "$pids" ]]; then
        out+="  ${proc}: $(echo "$pids" | tr '\n' ' ')\n"
      fi
    fi
  done
  printf '%b' "$out"
}

# Test seam: sourcing with ABERP_UPGRADE_PROD_LIB_ONLY=1 loads the pure
# helpers above without running the upgrade flow. Used by
# run/tests/upgrade_prod_running_check_test.sh.
if [[ "${ABERP_UPGRADE_PROD_LIB_ONLY:-0}" == "1" ]]; then
  return 0 2>/dev/null || exit 0
fi

# ---------- self-syntax-check (mirrors run_prod.sh / release.sh) ------------
if ! bash -n "$0" 2>/dev/null; then
  echo "[fail] $0 failed 'bash -n' syntax check — refusing to run" >&2
  bash -n "$0"
  exit 2
fi

readonly VERSION_RE='^PROD_v[0-9]+\.[0-9]+(\.[0-9]+)?$'
readonly DEV_SENTINEL_PATH_SUBSTR="/Documents/Claude/Projects/"

# ---------- colour helpers (no-op when stdout is not a terminal) ------------
if [[ -t 1 && -z "${NO_COLOR:-}" ]]; then
  c_red=$'\033[1;31m'; c_yel=$'\033[1;33m'; c_grn=$'\033[1;32m'
  c_dim=$'\033[2m';    c_rst=$'\033[0m'
else
  c_red=""; c_yel=""; c_grn=""; c_dim=""; c_rst=""
fi

die() {
  echo "${c_red}[fail]${c_rst} $1" >&2
  exit "${2:-2}"
}
warn() { echo "${c_yel}[warn]${c_rst} $*" >&2; }
info() { echo "${c_dim}[info]${c_rst} $*" >&2; }
ok()   { echo "${c_grn}[ ok ]${c_rst} $*" >&2; }

print_help() {
  sed -n '2,55p' "$0" | sed 's/^# \{0,1\}//'
}

# ---------- arg parsing -----------------------------------------------------
version=""
tenant="prod"
positional=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --help|-h)
      print_help
      exit 0
      ;;
    -*)
      die "unknown flag: $1"
      ;;
    *)
      if [[ $positional -eq 0 ]]; then
        version="$1"
      elif [[ $positional -eq 1 ]]; then
        tenant="$1"
      else
        die "unexpected positional arg: $1"
      fi
      positional=$((positional + 1))
      shift
      ;;
  esac
done

if [[ -z "$version" ]]; then
  echo "usage: $(basename "$0") <PROD_vMAJOR.MINOR[.PATCH]> [tenant]" >&2
  echo "       $(basename "$0") --help" >&2
  exit 2
fi

if [[ ! "$version" =~ $VERSION_RE ]]; then
  die "version '$version' does not match $VERSION_RE — expected e.g. PROD_v1.5 or PROD_v1.5.1
HU: A '$version' nem felel meg a $VERSION_RE mintának — pl. PROD_v1.5 vagy PROD_v1.5.1"
fi

# Resolve script + repo paths.
script_path="$(cd "$(dirname "$0")" && pwd -P)"
readonly SCRIPT_PATH="$script_path"
readonly REPO_ROOT="$(cd "$SCRIPT_PATH/.." && pwd -P)"

readonly TENANT="$tenant"
readonly TENANT_DIR="${HOME}/.aberp/${TENANT}"
readonly TENANT_SELLER_TOML="${TENANT_DIR}/seller.toml"

echo
echo "${c_yel}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${c_rst}" >&2
echo "${c_yel}  ABERP prod upgrade — ${version} (tenant=${TENANT})${c_rst}" >&2
echo "${c_yel}  ABERP éles frissítés — ${version} (bérlő=${TENANT})${c_rst}" >&2
echo "${c_yel}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${c_rst}" >&2
echo

# ---------- preflight 1: dev-workspace sentinel -----------------------------
# upgrade_prod.sh is the OPERATOR's upgrade tool. Running it from the dev
# clone would `git reset --hard origin/PROD_vX.Y` against the dev tree —
# i.e. wipe in-progress dev work. Refuse by default; opt-out for testing.
if [[ "$SCRIPT_PATH" == *"$DEV_SENTINEL_PATH_SUBSTR"* ]]; then
  if [[ "${ABERP_ALLOW_DEV_WORKSPACE:-0}" == "1" ]]; then
    warn "running from dev workspace (${SCRIPT_PATH}) — ABERP_ALLOW_DEV_WORKSPACE=1, proceeding."
    warn "fejlesztői munkamappából fut (${SCRIPT_PATH}) — engedélyezve."
  else
    die "upgrade_prod.sh is running from the DEV workspace
   path: ${SCRIPT_PATH}

   upgrade_prod.sh is the operator's upgrade tool — it must run from
   the PROD clone (e.g. ~/ABERP-prod), not from the dev tree. Running
   it here would 'git reset --hard origin/${version}' against your dev
   work and wipe in-progress changes.

   If you really meant this (e.g. testing the script):
     ABERP_ALLOW_DEV_WORKSPACE=1 $0 ${version} ${TENANT}

HU: Az upgrade_prod.sh fejlesztői munkamappából fut. Az operátori
   prod-mappából (pl. ~/ABERP-prod) indítsd, különben felülírja a
   dev-tree-t. Tesztelésre: ABERP_ALLOW_DEV_WORKSPACE=1."
  fi
fi

# ---------- preflight 2: git remote + branch exists on origin ---------------
cd "$REPO_ROOT" || die "could not cd to repo root: $REPO_ROOT"

if ! git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  die "not inside a git work tree (cwd: $REPO_ROOT)"
fi

if ! origin_url="$(git remote get-url origin 2>/dev/null)" || [[ -z "$origin_url" ]]; then
  die "no 'origin' remote configured — \`git remote add origin <url>\` first"
fi
info "origin: $origin_url"

info "git ls-remote --heads origin $version ..."
if ! git ls-remote --exit-code --heads origin "$version" >/dev/null 2>&1; then
  die "release branch '$version' does not exist on origin
   Did you run ./run/release.sh on the dev side first?
HU: A '$version' release-ág nem létezik az originon. Futott a release.sh a dev oldalon?"
fi
ok "release branch '$version' exists on origin"

# ---------- preflight 3: tenant dir + seller.toml present -------------------
if [[ ! -d "$TENANT_DIR" ]]; then
  die "tenant directory missing: $TENANT_DIR
   This script upgrades an EXISTING prod install. For a first cutover,
   follow CUTOVER_RUNBOOK.md Steps 1-7.
HU: A bérlő mappa hiányzik: $TENANT_DIR. Új telepítéshez kövesd a CUTOVER_RUNBOOK.md 1-7. lépéseit."
fi

if [[ ! -f "$TENANT_SELLER_TOML" ]]; then
  die "seller.toml missing: $TENANT_SELLER_TOML
   Operator state is incomplete — see runbook Step 3 (seller wizard).
HU: A seller.toml hiányzik: $TENANT_SELLER_TOML. Lásd a runbook 3. lépést."
fi
ok "tenant directory + seller.toml present"

# ---------- preflight 4: refuse if the prod binary is still running --------
# Don't try to hot-swap a running binary. We match this checkout's RELEASE
# binary by command line (`pgrep -f "$REPO_ROOT/target/release/<proc>"`),
# NOT the bare process name — see running_prod_pids() above. The old
# `pgrep -x <name>` matched any checkout's aberp-ui/aberp, so a dev/test
# session elsewhere produced a false "prod is running" refusal (S399/S400).
running_pids="$(running_prod_pids "$REPO_ROOT")"

if [[ -n "$running_pids" ]]; then
  die "the prod app is still running:
$running_pids
   Stop it FIRST (Ctrl-C in the run_prod.sh terminal where it's running),
   then re-run this script. Do not try to swap a running binary.

HU: Az éles app még fut. Ctrl-C-vel állítsd le abban a terminálban,
   ahol a run_prod.sh elindította, majd indítsd újra ezt a scriptet."
fi
ok "no aberp-ui / aberp process running — safe to swap"

# ---------- step 1: snapshot BEFORE switching -------------------------------
# This is the BEFORE-not-after snapshot the runbook PROD_v1.1→v1.3
# postmortem made bold. A snapshot taken AFTER `git checkout PROD_vX.Y`
# captures post-upgrade state and is useless as a rollback handle.
readonly SNAPSHOT_SCRIPT="${REPO_ROOT}/tools/snapshot-prod.sh"
if [[ ! -x "$SNAPSHOT_SCRIPT" ]]; then
  die "snapshot script not found / not executable: $SNAPSHOT_SCRIPT
HU: A snapshot-prod.sh hiányzik vagy nem futtatható: $SNAPSHOT_SCRIPT"
fi
echo
info "running ${SNAPSHOT_SCRIPT} ${TENANT} (zip will prompt for an encryption password) ..."
if ! "$SNAPSHOT_SCRIPT" "$TENANT"; then
  die "snapshot-prod.sh failed — refusing to switch branches without a recovery handle
HU: A snapshot futás nem sikerült — visszaállítási kézifogantyú nélkül nem váltok ágat." 3
fi
ok "pre-upgrade snapshot complete"

# ---------- step 2: full clean git switch -----------------------------------
echo
info "git fetch origin ..."
git fetch origin || die "git fetch failed — check network / origin access" 4

info "git reset --hard HEAD (drop tracked local modifications) ..."
git reset --hard HEAD || die "git reset --hard HEAD failed" 4

info "git clean -fd (drop untracked files + directories) ..."
git clean -fd || die "git clean -fd failed" 4

info "git checkout -B ${version} origin/${version} (create/reset + switch) ..."
git checkout -B "${version}" "origin/${version}" || die "git checkout -B ${version} failed" 4

# Belt-and-suspenders: ensure the local branch is exactly at origin.
# `checkout -B` should have done this, but a corrupt index could leave
# tracked mods carried forward. Explicit re-anchor.
info "git reset --hard origin/${version} (belt-and-suspenders) ..."
git reset --hard "origin/${version}" || die "git reset --hard origin/${version} failed" 4

ok "switched to ${version}"

# Prune stale local PROD_v* branches (everything except the target).
# Local `main` and feature branches are intentionally left alone — the
# operator may want `main` for `git pull` from dev.
echo
info "pruning stale local PROD_v* branches ..."
pruned_count=0
while IFS= read -r stale_branch; do
  [[ -z "$stale_branch" ]] && continue
  if [[ "$stale_branch" != "$version" ]]; then
    if git branch -D "$stale_branch" 2>/dev/null; then
      info "  pruned local ${stale_branch}"
      pruned_count=$((pruned_count + 1))
    fi
  fi
done < <(git for-each-ref --format='%(refname:short)' refs/heads/PROD_v* 2>/dev/null || true)

if [[ $pruned_count -eq 0 ]]; then
  info "  (no stale PROD_v* branches to prune)"
else
  ok "pruned ${pruned_count} stale PROD_v* branch(es)"
fi

# ---------- step 3: verify clean state --------------------------------------
echo
if [[ -n "$(git status --porcelain)" ]]; then
  die "working tree is NOT clean after switch — something is very wrong:
$(git status --short)
HU: A munkafa nem tiszta a váltás után — valami baj van." 5
fi

# NB: `git rev-parse --abbrev-ref HEAD` AND `git symbolic-ref --short HEAD`
# BOTH emit `heads/<name>` when a tag of the same name exists (release.sh
# pushes both `refs/heads/PROD_vX.Y` AND `refs/tags/PROD_vX.Y` since S212,
# so this collision is the steady state for every release branch).
# `git branch --show-current` (git 2.22+) is the only form that returns the
# unprefixed branch name in that ambiguous state. See S214 / PR-212.
current_branch="$(git branch --show-current 2>/dev/null)" || current_branch="UNKNOWN"
if [[ -z "$current_branch" ]]; then
  current_branch="UNKNOWN"
fi
if [[ "$current_branch" != "$version" ]]; then
  die "current branch is '$current_branch' but expected '$version'" 5
fi

local_head="$(git rev-parse HEAD)"
remote_head="$(git rev-parse "origin/${version}")"
if [[ "$local_head" != "$remote_head" ]]; then
  die "local HEAD ($local_head) does not match origin/${version} ($remote_head)" 5
fi
ok "verified: on ${version}, clean tree, HEAD=${local_head:0:12} matches origin"

# ---------- step 3b: provision the auto-quoting Python venv ------------------
# S282 / PR-267 — honor [[trust-code-not-operator]]: the operator should
# never need to remember to set `ABERP_QUOTE_PIPELINE_PYTHON` or run
# `python -m venv` by hand to enable the pricing pipeline. This step is
# idempotent — if the venv already exists AND can import BOTH
# `aberp_cad_extract` (STL) AND `OCP` (the STEP backend), it's a 100ms
# no-op. First time on a fresh checkout it creates the venv + installs the
# local package WITH the `[step]` extra. Failure here is logged but does
# NOT block the upgrade (operator can retry); the backend resolver will
# surface the missing venv as a RED card on the Pricing tab.
#
# S422 — this inline copy previously installed `pip install -e .` (base
# only) and verified only `import aberp_cad_extract`. A prod box provisioned
# solely by this script could extract STL but FAILED every STEP upload
# silently. It now mirrors run/provision_pipeline_venv.sh's `[step]`/OCP
# contract; the two are a deliberate, cross-referenced near-duplicate (see
# that file's header for why they are kept separate). Re-running this on a
# pre-S422 box detects the missing OCP and re-installs with `[step]`.
provision_pipeline_venv() {
  local venv_dir="${REPO_ROOT}/python/aberp-cad-extract/.venv"
  local venv_python="${venv_dir}/bin/python"
  local pkg_dir="${REPO_ROOT}/python/aberp-cad-extract"

  if [[ ! -d "$pkg_dir" ]]; then
    warn "auto-quoting Python package missing at ${pkg_dir} — skipping venv provisioning"
    warn "auto-árazás Python csomag hiányzik — kihagyás"
    return 0
  fi

  # No-op only if BOTH the base module AND the OCP STEP backend import.
  # A base-only venv (pre-S422, or built without `[step]`) must fall through
  # and get `.[step]` added — otherwise STEP uploads silently fail.
  if [[ -x "$venv_python" ]] \
    && "$venv_python" -c "import aberp_cad_extract, OCP" >/dev/null 2>&1; then
    info "pipeline venv OK at ${venv_dir} (module + OCP) — no-op"
    return 0
  fi

  echo
  info "provisioning auto-quoting Python venv at ${venv_dir} ..."
  info "auto-árazás Python venv telepítése a ${venv_dir} útvonalon ..."

  # The user may have a half-built venv (e.g. python upgrade left behind
  # a broken symlink). Recreate from scratch — idempotent + cheap.
  if [[ -d "$venv_dir" ]] && [[ ! -x "$venv_python" ]]; then
    warn "removing stale venv directory: ${venv_dir}"
    rm -rf "$venv_dir" || {
      warn "could not remove stale venv — skipping provisioning (operator can retry by hand)"
      return 0
    }
  fi

  if ! command -v python3 >/dev/null 2>&1; then
    warn "python3 not found on PATH — cannot provision the pipeline venv"
    warn "python3 nincs a PATH-on — a venv nem telepíthető"
    warn "Install Python 3.11+ then re-run this script."
    return 0
  fi

  if ! python3 -m venv "$venv_dir" >/dev/null 2>&1; then
    warn "python3 -m venv failed — pipeline daemon will surface as 'dormant' in the SPA"
    warn "python3 -m venv hibára futott — a SPA Pricing fülön RED kártyával jelzi"
    return 0
  fi

  if ! "$venv_python" -m pip install --quiet --upgrade pip >/dev/null 2>&1; then
    warn "pip --upgrade failed — continuing with the venv's bundled pip"
  fi

  # Install WITH the `[step]` extra (the ~63 MB OCP wheel): the pyproject
  # mandates "Production installs (and CI) MUST install with `.[step]`" so
  # STEP submissions extract cleanly instead of hitting the
  # NotImplementedError stub path. Without it STEP uploads fail in prod.
  if ! "$venv_python" -m pip install --quiet -e "${pkg_dir}[step]"; then
    warn "pip install -e ${pkg_dir}[step] failed — pipeline daemon will be dormant"
    warn "pip install -e hibára futott — a daemon szünetel"
    return 0
  fi

  if "$venv_python" -c "import aberp_cad_extract, OCP" >/dev/null 2>&1; then
    ok "pipeline venv provisioned at ${venv_dir} (module + OCP)"
  else
    warn "venv created but aberp_cad_extract / OCP still not importable — investigate by hand"
  fi
}

provision_pipeline_venv

# ---------- step 4: exec into run_prod.sh -----------------------------------
echo
echo "${c_grn}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${c_rst}" >&2
echo "${c_grn}  UPGRADE STATE READY — launching run_prod.sh${c_rst}" >&2
echo "${c_grn}  FRISSÍTÉS KÉSZ — run_prod.sh indítása${c_rst}" >&2
echo "${c_grn}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${c_rst}" >&2
echo

readonly RUN_PROD_SCRIPT="${REPO_ROOT}/run/run_prod.sh"
if [[ ! -x "$RUN_PROD_SCRIPT" ]]; then
  die "run_prod.sh not found / not executable: $RUN_PROD_SCRIPT"
fi

# exec replaces this process — operator sees one continuous output
# stream and Ctrl-C in the launching terminal exits the app cleanly.
exec "$RUN_PROD_SCRIPT"
