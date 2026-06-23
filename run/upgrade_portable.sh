#!/usr/bin/env bash
#
# upgrade_portable.sh — S435 — one-command, hülye-biztos PORTABLE upgrade.
#
# The Portable sibling of upgrade_prod.sh. Same clean git-switch +
# launch-into-the-runner sequence, but for the NAV-off Portable line:
#
#   ./run/upgrade_portable.sh PROD_Portable_v0.1.0
#
# Differences from upgrade_prod.sh (everything else mirrors it):
#   - Version regex accepts ONLY PROD_Portable_v<MAJOR>.<MINOR>[.PATCH]
#     (Defense gets its own upgrade script when that line is cut).
#   - Default tenant is `demo`, not `prod`. Override with the second
#     positional arg: `./run/upgrade_portable.sh PROD_Portable_v0.1.0 acme`.
#   - The tenant dir + seller.toml are NOT required to pre-exist: the
#     bundled demo tenant is SEEDED on first boot, and seller.toml is
#     optional in NAV-off mode. On a genuinely fresh install the
#     pre-upgrade snapshot is skipped (there is nothing to roll back to).
#   - The running-binary refusal matches this checkout's DEV binary
#     (target/debug/...), since run_portable.sh launches a dev-profile
#     build — not the --features production release binary upgrade_prod.sh
#     guards against.
#   - exec's into ./run/run_portable.sh (not run_prod.sh).
#
# Exit codes:
#   0  success — exec'd into run_portable.sh
#   2  arg / preflight failure
#   3  snapshot-prod.sh failed
#   4  git switch step failed
#   5  HEAD / branch verify failed after switch
#
# Usage:
#   ./run/upgrade_portable.sh PROD_Portable_v0.1.0
#   ./run/upgrade_portable.sh PROD_Portable_v0.1.0 acme
#   ./run/upgrade_portable.sh --help

set -euo pipefail

# ---------- pure helper: which of THIS checkout's DEV binaries run? ----------
# Mirrors upgrade_prod.sh's running_prod_pids, but scoped to target/debug
# (run_portable.sh launches a dev-profile build). Emits "  <proc>: <pids>\n"
# per running aberp-ui / aberp process whose command line is THIS checkout's
# debug binary; empty output means none.
running_portable_pids() {
  local repo_root="$1" proc pids out=""
  for proc in aberp-ui aberp; do
    if pids="$(pgrep -f "${repo_root}/target/debug/${proc}"'( |$)' 2>/dev/null)"; then
      if [[ -n "$pids" ]]; then
        out+="  ${proc}: $(echo "$pids" | tr '\n' ' ')\n"
      fi
    fi
  done
  printf '%b' "$out"
}

# Test seam: sourcing with ABERP_UPGRADE_PORTABLE_LIB_ONLY=1 loads the pure
# helpers above without running the upgrade flow.
if [[ "${ABERP_UPGRADE_PORTABLE_LIB_ONLY:-0}" == "1" ]]; then
  return 0 2>/dev/null || exit 0
fi

# ---------- self-syntax-check (mirrors upgrade_prod.sh) ----------------------
if ! bash -n "$0" 2>/dev/null; then
  echo "[fail] $0 failed 'bash -n' syntax check — refusing to run" >&2
  bash -n "$0"
  exit 2
fi

# Portable-line tags only. Defense gets its own script when cut.
readonly VERSION_RE='^PROD_Portable_v[0-9]+\.[0-9]+(\.[0-9]+)?$'
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
  sed -n '2,38p' "$0" | sed 's/^# \{0,1\}//'
}

# ---------- arg parsing -----------------------------------------------------
version=""
tenant="demo"
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
  echo "usage: $(basename "$0") <PROD_Portable_vMAJOR.MINOR[.PATCH]> [tenant]" >&2
  echo "       $(basename "$0") --help" >&2
  exit 2
fi

if [[ ! "$version" =~ $VERSION_RE ]]; then
  die "version '$version' does not match $VERSION_RE — expected e.g. PROD_Portable_v0.1.0 or PROD_Portable_v0.1.1
HU: A '$version' nem felel meg a $VERSION_RE mintának — pl. PROD_Portable_v0.1.0"
fi

# Resolve script + repo paths.
script_path="$(cd "$(dirname "$0")" && pwd -P)"
readonly SCRIPT_PATH="$script_path"
readonly REPO_ROOT="$(cd "$SCRIPT_PATH/.." && pwd -P)"

readonly TENANT="$tenant"
readonly TENANT_DIR="${HOME}/.aberp-portable/${TENANT}"

echo
echo "${c_grn}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${c_rst}" >&2
echo "${c_grn}  ABERP portable upgrade — ${version} (tenant=${TENANT})${c_rst}" >&2
echo "${c_grn}  ABERP portable frissítés — ${version} (bérlő=${TENANT})${c_rst}" >&2
echo "${c_grn}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${c_rst}" >&2
echo

# ---------- preflight 1: dev-workspace sentinel -----------------------------
# As with upgrade_prod.sh: refuse to `git reset --hard` against the dev tree.
if [[ "$SCRIPT_PATH" == *"$DEV_SENTINEL_PATH_SUBSTR"* ]]; then
  if [[ "${ABERP_ALLOW_DEV_WORKSPACE:-0}" == "1" ]]; then
    warn "running from dev workspace (${SCRIPT_PATH}) — ABERP_ALLOW_DEV_WORKSPACE=1, proceeding."
    warn "fejlesztői munkamappából fut (${SCRIPT_PATH}) — engedélyezve."
  else
    die "upgrade_portable.sh is running from the DEV workspace
   path: ${SCRIPT_PATH}

   upgrade_portable.sh is the operator's upgrade tool — it must run from
   the Portable clone (e.g. ~/ABERP-Portable), not from the dev tree.
   Running it here would 'git reset --hard origin/${version}' against your
   dev work and wipe in-progress changes.

   If you really meant this (e.g. testing the script):
     ABERP_ALLOW_DEV_WORKSPACE=1 $0 ${version} ${TENANT}

HU: Az upgrade_portable.sh fejlesztői munkamappából fut. A Portable
   mappából (pl. ~/ABERP-Portable) indítsd. Tesztelésre:
   ABERP_ALLOW_DEV_WORKSPACE=1."
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

# ---------- preflight 3: tenant dir (optional — seeded on first boot) -------
# Unlike upgrade_prod.sh, the Portable tenant dir + seller.toml are NOT
# required: the bundled demo tenant is seeded on first boot and seller.toml
# is optional in NAV-off mode. A present dir gets a pre-upgrade snapshot
# below; a missing one is a genuinely fresh install (snapshot skipped).
tenant_dir_exists=0
if [[ -d "$TENANT_DIR" ]]; then
  tenant_dir_exists=1
  ok "tenant directory present: $TENANT_DIR"
else
  info "tenant directory $TENANT_DIR not present — fresh install; will be seeded on first boot"
fi

# ---------- preflight 4: refuse if the portable binary is still running -----
# Match this checkout's DEV binary (run_portable.sh launches a dev build).
running_pids="$(running_portable_pids "$REPO_ROOT")"

if [[ -n "$running_pids" ]]; then
  die "the portable app is still running:
$running_pids
   Stop it FIRST (Ctrl-C in the run_portable.sh terminal where it's running),
   then re-run this script. Do not try to swap a running binary.

HU: A portable app még fut. Ctrl-C-vel állítsd le abban a terminálban,
   ahol a run_portable.sh elindította, majd indítsd újra ezt a scriptet."
fi
ok "no aberp-ui / aberp process running — safe to swap"

# ---------- step 1: snapshot BEFORE switching (only if there's state) -------
# A snapshot is a rollback handle for an EXISTING install. On a fresh
# Portable install there is nothing to capture, so skip it. When the tenant
# dir exists, take the same BEFORE-not-after snapshot upgrade_prod.sh does.
if [[ "$tenant_dir_exists" -eq 1 ]]; then
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
else
  info "skipping pre-upgrade snapshot (fresh install — no existing tenant state to roll back to)"
fi

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

info "git reset --hard origin/${version} (belt-and-suspenders) ..."
git reset --hard "origin/${version}" || die "git reset --hard origin/${version} failed" 4

ok "switched to ${version}"

# Prune stale local PROD_Portable_v* branches (everything except the target).
# Local `main` is intentionally left alone.
echo
info "pruning stale local PROD_Portable_v* branches ..."
pruned_count=0
while IFS= read -r stale_branch; do
  [[ -z "$stale_branch" ]] && continue
  if [[ "$stale_branch" != "$version" ]]; then
    if git branch -D "$stale_branch" 2>/dev/null; then
      info "  pruned local ${stale_branch}"
      pruned_count=$((pruned_count + 1))
    fi
  fi
done < <(git for-each-ref --format='%(refname:short)' refs/heads/PROD_Portable_v* 2>/dev/null || true)

if [[ $pruned_count -eq 0 ]]; then
  info "  (no stale PROD_Portable_v* branches to prune)"
else
  ok "pruned ${pruned_count} stale PROD_Portable_v* branch(es)"
fi

# ---------- step 3: verify clean state --------------------------------------
echo
if [[ -n "$(git status --porcelain)" ]]; then
  die "working tree is NOT clean after switch — something is very wrong:
$(git status --short)
HU: A munkafa nem tiszta a váltás után — valami baj van." 5
fi

# `git branch --show-current` is the only form that returns the unprefixed
# branch name when a tag of the same name exists (release.sh pushes both).
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
# Identical contract to upgrade_prod.sh: idempotent `.[step]`/OCP venv so the
# pricing pipeline (STL + STEP) works without operator fiddling. Failure is
# logged, not fatal.
provision_pipeline_venv() {
  local venv_dir="${REPO_ROOT}/python/aberp-cad-extract/.venv"
  local venv_python="${venv_dir}/bin/python"
  local pkg_dir="${REPO_ROOT}/python/aberp-cad-extract"

  if [[ ! -d "$pkg_dir" ]]; then
    warn "auto-quoting Python package missing at ${pkg_dir} — skipping venv provisioning"
    return 0
  fi

  if [[ -x "$venv_python" ]] \
    && "$venv_python" -c "import aberp_cad_extract, OCP" >/dev/null 2>&1; then
    info "pipeline venv OK at ${venv_dir} (module + OCP) — no-op"
    return 0
  fi

  echo
  info "provisioning auto-quoting Python venv at ${venv_dir} ..."

  if [[ -d "$venv_dir" ]] && [[ ! -x "$venv_python" ]]; then
    warn "removing stale venv directory: ${venv_dir}"
    rm -rf "$venv_dir" || {
      warn "could not remove stale venv — skipping provisioning (operator can retry by hand)"
      return 0
    }
  fi

  if ! command -v python3 >/dev/null 2>&1; then
    warn "python3 not found on PATH — cannot provision the pipeline venv"
    warn "Install Python 3.11+ then re-run this script."
    return 0
  fi

  if ! python3 -m venv "$venv_dir" >/dev/null 2>&1; then
    warn "python3 -m venv failed — pipeline daemon will surface as 'dormant' in the SPA"
    return 0
  fi

  if ! "$venv_python" -m pip install --quiet --upgrade pip >/dev/null 2>&1; then
    warn "pip --upgrade failed — continuing with the venv's bundled pip"
  fi

  if ! "$venv_python" -m pip install --quiet -e "${pkg_dir}[step]"; then
    warn "pip install -e ${pkg_dir}[step] failed — pipeline daemon will be dormant"
    return 0
  fi

  if "$venv_python" -c "import aberp_cad_extract, OCP" >/dev/null 2>&1; then
    ok "pipeline venv provisioned at ${venv_dir} (module + OCP)"
  else
    warn "venv created but aberp_cad_extract / OCP still not importable — investigate by hand"
  fi
}

provision_pipeline_venv

# ---------- step 4: exec into run_portable.sh -------------------------------
echo
echo "${c_grn}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${c_rst}" >&2
echo "${c_grn}  UPGRADE STATE READY — launching run_portable.sh${c_rst}" >&2
echo "${c_grn}  FRISSÍTÉS KÉSZ — run_portable.sh indítása${c_rst}" >&2
echo "${c_grn}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${c_rst}" >&2
echo

readonly RUN_PORTABLE_SCRIPT="${REPO_ROOT}/run/run_portable.sh"
if [[ ! -x "$RUN_PORTABLE_SCRIPT" ]]; then
  die "run_portable.sh not found / not executable: $RUN_PORTABLE_SCRIPT"
fi

# exec replaces this process; the resolved tenant is passed via env so
# run_portable.sh lands in the same tenant we just snapshotted/verified.
exec env ABERP_TENANT="$TENANT" "$RUN_PORTABLE_SCRIPT"
