#!/usr/bin/env bash
#
# release.sh — S169 / PR-169 — per-release-branch publisher.
#
# WHAT IT DOES (in order):
#   1. Refuses to run from the dev checkout (path-substring sentinel —
#      anything under `/Documents/Claude/Projects/` is considered dev).
#   2. Validates we're on `main` with a CLEAN working tree.
#   3. Validates the version arg matches `PROD_[<LINE>_]vMAJOR.MINOR` OR
#      `PROD_[<LINE>_]vMAJOR.MINOR.PATCH` — e.g. PROD_v1.0, PROD_v1.4.1,
#      PROD_Defense_v0.2.13, PROD_Portable_v1.0.0. Note the case +
#      separator: uppercase PROD, underscore, dot-separated numbers.
#      This is intentional — the release model uses branch names, not
#      tags, and the uppercase shape makes them visually distinct from
#      feature branches in `git branch -a`. The optional patch segment
#      was added in S201 / PR-201 once the versioning policy (ADR-0056)
#      named patch releases as load-bearing for bugfix-only bumps within
#      the 1.x invoicing strand. 4+ segments and pre-release suffixes
#      (e.g. `-rc1`) are rejected.
#
#      PRODUCT-LINE PREFIXES (ADR-0093 saw-off, ADR-0100 §4 blocker,
#      ADR-0056 amendment 2026-07-21): this repository (ABERP-Editions)
#      ships two product lines, and each has its own installer that
#      resolves a release from `origin/<version>` as a BRANCH:
#        - `PROD_Defense_v*`  → run/upgrade_defense.sh:83  VERSION_RE
#        - `PROD_Portable_v*` → run/upgrade_portable.sh:70 VERSION_RE
#      Before this change `VERSION_RE` here was `^PROD_v…$` only, so
#      this repo had NO script that could cut either line — the thirteen
#      `PROD_Defense_v*` branches on origin were all pushed BY HAND
#      (`git push origin <ref>:refs/heads/PROD_Defense_vX.Y.Z`), which is
#      how `PROD_Defense_v0.2.12` came to sit one commit AHEAD of `main`
#      and how the `v0.2.1` gap / `v0.2.0`==`v0.2.2` duplicate arose:
#      none of this script's preflights ran. The bare `PROD_v*` form is
#      retained for the frozen HU prod line's own clones.
#   4. Refuses if the release branch already exists on origin (suggests
#      the next minor as the conservative next pick, preserving the
#      product-line prefix the operator typed).
#   5. Pushes `origin main:PROD_[<LINE>_]vX.Y` — the branch IS the
#      release marker. No annotated tag, no local build, no codesign.
#      The operator clones from this branch on the target machine and
#      builds there.
#   6. Prints the `git clone --branch <name>` command and the matching
#      line-specific launcher the operator runs next.
#
# WHY THE MODEL CHANGED (S167 → S169):
#   S167's release.sh built locally + tagged locally + pushed a tag. That
#   model couples the dev machine to the build artifact, which the 2026-05-30
#   cutover proved fragile (an icons/ regression on the dev machine reached
#   prod silently). The new model decouples: dev publishes a release ref;
#   prod machine clones + builds. The build happens on the operator's prod
#   machine, with that machine's tooling, against a known git ref. Smaller
#   blast radius if dev tooling drifts.
#
# WHAT IT DELIBERATELY DOES NOT DO:
#   - Does NOT cargo fmt / clippy / build. Main HEAD is assumed ready —
#     those gates live in the dev workflow, not at publish time.
#   - Does NOT codesign or notarise (handled later on the prod machine).
#   - Does NOT push from a dev clone with uncommitted changes — the
#     `git status --porcelain` gate refuses dirty trees.
#   - Does NOT create a tag. The branch IS the release. If you need a
#     fixed pointer to a specific commit, the branch's HEAD is it.
#
# USAGE:
#   ./run/release.sh PROD_v1.0                 # 2-segment (major + minor)
#   ./run/release.sh PROD_v1.4.1               # 3-segment (+ patch)
#   ./run/release.sh PROD_Defense_v0.2.13      # Defense line
#   ./run/release.sh PROD_Portable_v1.0.0      # Portable line
#   ./run/release.sh --help
#
# FLAGS:
#   --help, -h         print this header and exit
#
# EXIT CODES:
#   0  release branch pushed to origin
#   2  arg / preflight failure (wrong branch, dirty tree, bad version,
#      branch exists, dev-sentinel match)
#   5  git push failed

set -euo pipefail

# ---------- self-syntax-check (mirrors run_desktop.sh PR-55) ----------------
if ! bash -n "$0" 2>/dev/null; then
  echo "[fail] $0 failed 'bash -n' syntax check — refusing to run" >&2
  bash -n "$0"
  exit 2
fi

readonly MAIN_BRANCH="main"
# 2-segment (PROD_v1.5) and 3-segment (PROD_v1.4.1) accepted; 4+ segments
# and any suffix (pre-release / build metadata / -rc1) rejected. See
# ADR-0056 for the versioning policy that motivates the optional patch.
#
# The optional `Defense_` / `Portable_` product-line segment is pinned by
# ADR-0056 (amended 2026-07-21, ADR-0100 §4 blocker). It is a CLOSED
# alternation, not a wildcard: only the two lines this repository actually
# builds are cuttable, so a typo (`PROD_Portible_v1.0.0`) still dies at
# arg validation rather than publishing an uninstallable branch. These two
# alternatives mirror `upgrade_defense.sh:83` / `upgrade_portable.sh:70`
# exactly — the installer side is the contract this must satisfy.
readonly VERSION_RE='^PROD_(Defense_|Portable_)?v[0-9]+\.[0-9]+(\.[0-9]+)?$'
# Dev-sentinel: any checkout under this path subtree is the dev workspace.
# release.sh must be invoked from the OPERATOR's prod clone, not the dev
# clone. See header note (S169 model decouples publish from build).
readonly DEV_SENTINEL_PATH_SUBSTR="/Documents/Claude/Projects/"

# Resolve script location (not $PWD — the operator might `cd` elsewhere
# before invoking). pwd -P dereferences symlinks; we want the real
# physical path for the sentinel check.
script_path="$(cd "$(dirname "$0")" && pwd -P)"
readonly SCRIPT_PATH="$script_path"

# ---------- colour helpers (no-op when stdout is not a terminal) ------------
if [[ -t 1 && -z "${NO_COLOR:-}" ]]; then
  c_red=$'\033[1;31m'; c_yel=$'\033[1;33m'; c_grn=$'\033[1;32m'
  c_dim=$'\033[2m';    c_rst=$'\033[0m'
else
  c_red=""; c_yel=""; c_grn=""; c_dim=""; c_rst=""
fi

die()  { echo "${c_red}[fail]${c_rst} $*" >&2; exit "${2:-2}"; }
warn() { echo "${c_yel}[warn]${c_rst} $*" >&2; }
info() { echo "${c_dim}[info]${c_rst} $*"; }
ok()   { echo "${c_grn}[ ok ]${c_rst} $*"; }

print_help() {
  # Print the header block: every line after the shebang up to the first
  # non-comment line. Was a hardcoded `sed -n '2,60p'`, which silently
  # truncated --help the moment the header grew (it did, in the ADR-0056
  # product-line amendment). Deriving the range removes that trap.
  awk 'NR == 1 { next } !/^#/ { exit } { print }' "$0" | sed 's/^# \{0,1\}//'
}

# ---------- arg parsing -----------------------------------------------------
version=""
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
      if [[ -z "$version" ]]; then
        version="$1"
        shift
      else
        die "unexpected positional arg: $1 (version already set to $version)"
      fi
      ;;
  esac
done

if [[ -z "$version" ]]; then
  echo "usage: $(basename "$0") <PROD_vMAJOR.MINOR[.PATCH]>" >&2
  echo "       $(basename "$0") --help" >&2
  exit 2
fi

if [[ ! "$version" =~ $VERSION_RE ]]; then
  die "version '$version' does not match $VERSION_RE — expected e.g. PROD_v1.0, PROD_v1.4.1, PROD_Defense_v0.2.13 or PROD_Portable_v1.0.0
HU: A '$version' nem felel meg a $VERSION_RE mintának — pl. PROD_v1.0, PROD_v1.4.1, PROD_Defense_v0.2.13 vagy PROD_Portable_v1.0.0"
fi

# ---------- preflight: dev-sentinel ----------------------------------------
# Refuse to publish from the dev checkout. The new release model expects
# release.sh to be invoked from the OPERATOR's prod clone (which is a
# fresh clone of the previous release branch, sitting somewhere outside
# the dev workspace tree).
if [[ "$SCRIPT_PATH" == *"$DEV_SENTINEL_PATH_SUBSTR"* ]]; then
  die "release.sh is running from the DEV workspace
   path: $SCRIPT_PATH

   The S169 release model publishes from the operator's prod clone, not
   the dev clone. Steps:

   1. Make sure dev work has landed on main and pushed: git push origin main
   2. Clone (or pull) the prod working dir somewhere outside
      $DEV_SENTINEL_PATH_SUBSTR
   3. From THAT clone: ./run/release.sh $version

   HU: A release.sh-t a fejlesztői munkamappából futtatod. A kiadást
   az operátor prod-mappájából kell indítani, nem innen."
fi

# ---------- preflight: must be on main + clean tree ------------------------
cd "$SCRIPT_PATH/.." || die "could not cd to repo root"

current_branch="$(git rev-parse --abbrev-ref HEAD)"
if [[ "$current_branch" != "$MAIN_BRANCH" ]]; then
  die "must be on '$MAIN_BRANCH' to publish a release (currently on '$current_branch')
HU: A kiadáshoz '$MAIN_BRANCH' ágon kell lenned (jelenleg '$current_branch')."
fi

if [[ -n "$(git status --porcelain)" ]]; then
  die "working tree is dirty — commit / stash before publishing:
$(git status --short)
HU: A munkafa piszkos — commitold vagy stash-old a változtatásokat publikálás előtt."
fi

ok "on $MAIN_BRANCH with clean working tree"

# ---------- preflight: branch must not already exist on origin -------------
info "git fetch origin (refresh remote refs) ..."
if ! git fetch --quiet origin; then
  warn "git fetch failed — proceeding with possibly-stale remote refs"
fi

existing_ref="$(git ls-remote --heads origin "$version" 2>/dev/null | awk '{print $1}')"
if [[ -n "$existing_ref" ]]; then
  # Suggest the next bump in the SAME shape the operator typed:
  # - 2-segment input → suggest next minor (PROD_v1.5 → PROD_v1.6).
  # - 3-segment input → suggest next patch (PROD_v1.4.1 → PROD_v1.4.2).
  # The shape choice is intentional per ADR-0056: a 2-segment bump
  # signals a minor release; a 3-segment bump signals a patch release.
  # The suggester preserves the operator's intent rather than forcing
  # a shape switch.
  # The product-line prefix is captured and REPLAYED into the suggestion:
  # suggesting `PROD_v0.2.13` to an operator who typed
  # `PROD_Defense_v0.2.12` would send them to a name no installer accepts.
  line="$(echo "$version" | sed -E 's/^PROD_(Defense_|Portable_)?v[0-9]+\.[0-9]+(\.[0-9]+)?$/\1/')"
  major="$(echo "$version" | sed -E 's/^PROD_(Defense_|Portable_)?v([0-9]+)\.([0-9]+)(\.([0-9]+))?$/\2/')"
  minor="$(echo "$version" | sed -E 's/^PROD_(Defense_|Portable_)?v([0-9]+)\.([0-9]+)(\.([0-9]+))?$/\3/')"
  patch="$(echo "$version" | sed -E 's/^PROD_(Defense_|Portable_)?v([0-9]+)\.([0-9]+)(\.([0-9]+))?$/\5/')"
  if [[ -n "$patch" ]]; then
    next_version="PROD_${line}v${major}.${minor}.$((patch + 1))"
    shape_label="next patch"
  else
    next_version="PROD_${line}v${major}.$((minor + 1))"
    shape_label="next minor"
  fi
  die "release branch '$version' already exists on origin at ${existing_ref:0:12}.
   Pick a new version — $shape_label would be: $next_version
HU: A '$version' release-ág már létezik a távolin. Válassz új verziót, pl.: $next_version"
fi
ok "release branch '$version' is free on origin"

# ---------- the push --------------------------------------------------------
main_sha="$(git rev-parse "$MAIN_BRANCH")"

echo
echo "${c_yel}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${c_rst}"
echo "${c_yel}  Publishing release branch ${version}${c_rst}"
echo "${c_yel}  Kiadási ág publikálása: ${version}${c_rst}"
echo "${c_yel}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${c_rst}"
echo "  main HEAD: ${main_sha}"
echo "  ref:       refs/heads/${version}"
echo

info "git push origin ${MAIN_BRANCH}:refs/heads/${version}"
if ! git push origin "${MAIN_BRANCH}:refs/heads/${version}"; then
  die "git push failed — network down, or no write permission to refs/heads/${version}" 5
fi

ok "pushed origin/${version} → ${main_sha:0:12}"

# ---------- summary + operator next-step -----------------------------------
origin_url="$(git remote get-url origin 2>/dev/null || echo '<origin-url>')"

echo
echo "${c_grn}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${c_rst}"
echo "${c_grn}  RELEASE ${version} PUBLISHED${c_rst}"
echo "${c_grn}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${c_rst}"
echo
echo "  Branch:   ${version}"
echo "  Commit:   ${main_sha}"
echo "  Origin:   ${origin_url}"
echo
# Line-specific launcher + installer. Cutting a Defense/Portable release
# and then printing `run_prod.sh` would hand the operator the frozen HU
# prod line's launcher — a different data root and, for Defense, a
# different NAV posture. Derive it from the version instead of hardcoding.
case "$version" in
  PROD_Defense_v*)  launcher="./run/run_defense.sh";  upgrader="./run/upgrade_defense.sh ${version}"  ;;
  PROD_Portable_v*) launcher="./run/run_portable.sh"; upgrader="./run/upgrade_portable.sh ${version}" ;;
  *)                launcher="./run/run_prod.sh";     upgrader="./run/upgrade_prod.sh ${version}"     ;;
esac

echo "${c_yel}Next on the target machine:${c_rst}"
echo "  ${c_dim}git clone --branch ${version} ${origin_url} <target-dir>${c_rst}"
echo "  ${c_dim}cd <target-dir>${c_rst}"
echo "  ${c_dim}${launcher}${c_rst}"
echo
echo "  ${c_dim}# ...or, upgrading an EXISTING install in place:${c_rst}"
echo "  ${c_dim}${upgrader}${c_rst}"
echo
echo "${c_yel}Következő lépés a cél gépen:${c_rst}"
echo "  Klónozd a $version ágról és futtasd: ${launcher}"
echo "  Meglévő telepítés frissítése: ${upgrader}"
echo

exit 0
