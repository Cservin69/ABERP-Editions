#!/usr/bin/env bash
#
# release.sh — S167 / PR-167 — version-tagging release script.
#
# What it does (in order):
#   1. Validates we're on `main` with a CLEAN working tree.
#   2. Validates the version arg matches `prod-vMAJOR.MINOR.PATCH`.
#   3. Validates the tag doesn't already exist (locally OR on origin).
#   4. Runs `cargo fmt --check` (strict — refuses to release dirty fmt).
#   5. Runs `cargo clippy --workspace` (advisory — prints, never blocks).
#   6. Builds the release binaries with `--features production`:
#        - `aberp`       (CLI / serve / NAV submitter)
#        - `aberp-ui`    (Tauri shell — the operator launches this)
#   7. Creates an ANNOTATED tag locally (NOT pushed).
#   8. Prints binary paths + the reminder that the tag is local only.
#
# What it deliberately does NOT do:
#   - It does NOT push the tag. The cutover operator pushes it manually
#     after smoke-testing the build (CLAUDE.md rule 12 — never let a
#     tool make a remote-visible decision the operator hasn't seen).
#   - It does NOT codesign or notarise (out of scope — see
#     run_desktop.sh PR-52 note for the linker-adhoc rationale).
#   - It does NOT bundle a `.app`. The Tauri-produced binary in
#     target/release/aberp-ui IS the artifact for the cutover; the
#     operator can wrap it in a `.app` later if needed.
#
# Why annotated tags (not lightweight):
#   `git describe` and `git for-each-ref` treat annotated tags as
#   first-class refs with a tagger, date, and message. Lightweight
#   tags get filtered out of `git describe` by default. For an
#   audit trail of which-binary-shipped-when, annotated is correct.
#
# Usage:
#   ./run/release.sh prod-v0.1.0
#   ./run/release.sh prod-v0.1.0 --message "Cutover 2026-06-01 — first prod issuance"
#   ./run/release.sh --help
#
# Flags (after the version):
#   --message <text>   annotated-tag message (default: see DEFAULT_TAG_MSG below)
#   --skip-fmt         skip `cargo fmt --check` (NOT recommended)
#   --skip-clippy      skip `cargo clippy` advisory pass
#   --skip-build       skip the cargo build (use only when re-tagging an
#                      already-built tree; the build is what proves the
#                      tag is reproducible)
#
# Exit codes:
#   0  release artifacts + tag created
#   2  arg / preflight failure (wrong branch, dirty tree, bad version, tag exists)
#   3  fmt check failed
#   4  cargo build failed
#   5  git tag creation failed

set -euo pipefail

# ---------- self-syntax-check (mirrors run_desktop.sh PR-55) ----------------
if ! bash -n "$0" 2>/dev/null; then
  echo "[fail] $0 failed 'bash -n' syntax check — refusing to run" >&2
  bash -n "$0"
  exit 2
fi

readonly REPO_ROOT="/Users/aben/Documents/Claude/Projects/ABERP"
readonly MAIN_BRANCH="main"
readonly VERSION_RE='^prod-v[0-9]+\.[0-9]+\.[0-9]+$'

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
  sed -n '2,60p' "$0" | sed 's/^# \{0,1\}//'
}

# ---------- arg parsing -----------------------------------------------------
version=""
tag_message=""
skip_fmt=0
skip_clippy=0
skip_build=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --help|-h)        print_help; exit 0 ;;
    --message)        tag_message="$2"; shift 2 ;;
    --skip-fmt)       skip_fmt=1; shift ;;
    --skip-clippy)    skip_clippy=1; shift ;;
    --skip-build)     skip_build=1; shift ;;
    -*)               die "unknown flag: $1" ;;
    *)
      if [[ -z "$version" ]]; then
        version="$1"; shift
      else
        die "unexpected positional arg: $1 (version already set to $version)"
      fi
      ;;
  esac
done

if [[ -z "$version" ]]; then
  echo "usage: $(basename "$0") <prod-vMAJOR.MINOR.PATCH> [--message TEXT] [--skip-fmt] [--skip-clippy] [--skip-build]" >&2
  echo "       $(basename "$0") --help" >&2
  exit 2
fi

if [[ ! "$version" =~ $VERSION_RE ]]; then
  die "version '$version' does not match $VERSION_RE — expected e.g. prod-v0.1.0"
fi

readonly DEFAULT_TAG_MSG="Release ${version} — ABERP production build"
if [[ -z "$tag_message" ]]; then
  tag_message="$DEFAULT_TAG_MSG"
fi

cd "$REPO_ROOT" || die "repo not at $REPO_ROOT"

# ---------- preflight: branch + clean tree ----------------------------------
current_branch="$(git rev-parse --abbrev-ref HEAD)"
if [[ "$current_branch" != "$MAIN_BRANCH" ]]; then
  die "must be on '$MAIN_BRANCH' to release (currently on '$current_branch'). \
ff-merge your feature branch first, then re-run."
fi

if [[ -n "$(git status --porcelain)" ]]; then
  die "working tree is dirty — commit / stash before releasing:
$(git status --short)"
fi

ok "on $MAIN_BRANCH with clean working tree"

# ---------- preflight: tag must not exist (local OR remote) -----------------
if git rev-parse --verify --quiet "refs/tags/${version}" >/dev/null; then
  die "tag '${version}' already exists locally. Delete it (\`git tag -d ${version}\`) \
or pick a new version, then re-run."
fi

# Best-effort check against origin too — if no network or no origin, just skip.
if git ls-remote --exit-code --tags origin "${version}" >/dev/null 2>&1; then
  die "tag '${version}' already exists on origin. Pick a new version."
fi
ok "tag '${version}' is free locally and on origin"

# ---------- fmt gate (strict) -----------------------------------------------
if [[ $skip_fmt -eq 0 ]]; then
  info "cargo fmt --check ..."
  if ! cargo fmt --all -- --check; then
    die "cargo fmt --check failed — run \`cargo fmt --all\` and re-commit before releasing" 3
  fi
  ok "cargo fmt is clean"
else
  warn "--skip-fmt set; cargo fmt --check not run"
fi

# ---------- clippy (advisory) -----------------------------------------------
# Pragmatic: ABERP has not adopted a -D warnings baseline (CLAUDE.md
# rule 12 + S167 brief allowance: "if there's a known baseline, gate
# on no NEW lints pragmatically"). We print clippy output for the
# operator's eyes and continue.
if [[ $skip_clippy -eq 0 ]]; then
  info "cargo clippy --workspace ... (advisory; non-blocking)"
  if cargo clippy --workspace --all-targets --features production -- -W clippy::all; then
    ok "cargo clippy completed (no errors; any warnings printed above)"
  else
    warn "cargo clippy reported errors — review output above. NOT blocking the release; \
the operator decides whether to abort."
  fi
else
  warn "--skip-clippy set; cargo clippy not run"
fi

# ---------- the build -------------------------------------------------------
# We build BOTH binaries with --features production. The Tauri shell
# (aberp-ui) is what the operator launches; the CLI (aberp) is what the
# Tauri shell spawns for serve/issue/keychain reads. The `production`
# feature is wired on BOTH crates so a single flag enables both.
if [[ $skip_build -eq 0 ]]; then
  echo
  echo "${c_yel}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${c_rst}"
  echo "${c_yel}  Building PRODUCTION release binaries (this takes a while)${c_rst}"
  echo "${c_yel}  Éles release-fordítás (a build több percig is eltarthat)${c_rst}"
  echo "${c_yel}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${c_rst}"

  info "cargo build --release --features production --bin aberp"
  if ! cargo build --release --features production --bin aberp; then
    die "cargo build aberp (release, production) failed" 4
  fi

  info "cargo build --release --features production --bin aberp-ui"
  if ! cargo build --release --features production --bin aberp-ui; then
    die "cargo build aberp-ui (release, production) failed" 4
  fi

  ok "release binaries built"
else
  warn "--skip-build set; not rebuilding binaries (re-tagging existing tree)"
fi

# ---------- create the annotated tag ----------------------------------------
# -a creates an annotated tag; -m supplies the message inline. We pass
# `--cleanup=verbatim` so the message is kept exactly as supplied (git
# would otherwise strip our trailing comment lines).
info "git tag -a ${version} -m \"${tag_message}\""
if ! git tag -a --cleanup=verbatim -m "${tag_message}" "${version}"; then
  die "git tag creation failed" 5
fi

tag_sha="$(git rev-list -n1 "${version}")"
ok "annotated tag '${version}' created at ${tag_sha:0:12}"

# ---------- summary + reminders --------------------------------------------
bin_aberp="${REPO_ROOT}/target/release/aberp"
bin_ui="${REPO_ROOT}/target/release/aberp-ui"

echo
echo "${c_grn}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${c_rst}"
echo "${c_grn}  RELEASE ${version} — local artifacts ready${c_rst}"
echo "${c_grn}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${c_rst}"
echo
echo "  Tag:        ${version}"
echo "  Commit:     ${tag_sha}"
echo "  Message:    ${tag_message}"
if [[ $skip_build -eq 0 ]]; then
  echo "  aberp:      ${bin_aberp}"
  echo "  aberp-ui:   ${bin_ui}  ${c_dim}(launchable)${c_rst}"
fi
echo
echo "${c_yel}Reminders:${c_rst}"
echo "  1. The tag is ${c_yel}LOCAL ONLY${c_rst} — not yet pushed."
echo "     Push it manually after smoke-testing: ${c_dim}git push origin ${version}${c_rst}"
echo "  2. To launch the prod binary: ${c_dim}./run/run_prod.sh${c_rst}"
echo "  3. To roll back: check out the previous tag and rebuild."
echo "     See ${c_dim}docs/CUTOVER_RUNBOOK.md${c_rst} Step 8 for the full procedure."
echo
echo "${c_yel}Emlékeztető:${c_rst}"
echo "  A címke ${c_yel}csak LOKÁLIS${c_rst} — még nincs feltöltve a távolira."
echo "  Az indításhoz: ${c_dim}./run/run_prod.sh${c_rst}"
echo
