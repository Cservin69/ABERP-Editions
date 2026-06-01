#!/usr/bin/env bash
# land_pending_commits.sh
# Bundles all currently-unstaged work into one snapshot commit on top of HEAD.
# Mirrors the snapshot pattern used at session 35 (commit 262b4f2).
#
# Currently bundles (sessions 54..62 + PR-46α.1 fix):
#   PR-44γ.1   chain-currency-match
#   PR-44δ.1   retry/drain spot-check pin tests
#   PR-44ε.1   printed-invoice PDF render + CLI
#   PR-44ε.UI  SPA Download PDF button
#   PR-44ζ     SPA Issue invoice form
#   PR-44η     SPA Submit + Poll buttons
#   PR-45      offline-boot refactor
#   PR-46α     first-run NAV-credentials setup wizard
#   PR-46α.1   tracing-to-stderr + boot-step markers
#
# Usage:
#   ./run/land_pending_commits.sh           # interactive
#   ./run/land_pending_commits.sh --yes     # non-interactive

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEFAULT_MESSAGE='bundle: sessions 54..62 + PR-46a.1 fix

Bundles nine pending PRs into one snapshot commit. Per-PR commit messages
preserved at _handoffs/PR-44*-commit-message.txt + PR-45a + PR-46a + PR-46a-fix
for future archaeology.

  PR-44gamma.1  chain-currency-match (storno/modification inherit base currency + rate)
  PR-44delta.1  retry/drain spot-check pin tests (on-disk XML byte-verbatim posture)
  PR-44epsilon.1 printed-invoice PDF render + aberp print-invoice CLI
  PR-44epsilon.UI SPA Download PDF button in invoice detail modal
  PR-44zeta     SPA + New invoice form (first mutation route on serve.rs)
  PR-44eta      SPA Submit to NAV + Poll ack now buttons
  PR-45         offline-boot refactor (handshake + binary_hash background + boot-status UI)
  PR-46alpha    first-run NAV-credentials setup wizard (no CLI required for setup)
  PR-46alpha.1  route aberp tracing to stderr, add boot-step markers so SPA
                loading pane shows live progress and names keychain stalls'

auto_yes=0
custom_message=""
while [ $# -gt 0 ]; do
  case "$1" in
    --yes|-y)     auto_yes=1; shift ;;
    --message|-m) custom_message="$2"; shift 2 ;;
    --help|-h)
      sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *) echo "unknown flag: $1" >&2; exit 2 ;;
  esac
done

commit_message="${custom_message:-$DEFAULT_MESSAGE}"

cd "$REPO_ROOT" || { echo "repo not at $REPO_ROOT" >&2; exit 2; }

if ! git rev-parse --git-dir >/dev/null 2>&1; then
  echo "$REPO_ROOT is not a git repo" >&2
  exit 2
fi

if [ -f .git/index.lock ]; then
  echo "[preflight] removing stale .git/index.lock"
  rm -f .git/index.lock
fi

echo
echo "=== files about to be staged ==="
git status --short
echo

if [ -z "$(git status --porcelain)" ]; then
  echo "[done] working tree is clean — nothing to commit"
  exit 0
fi

if [ "$auto_yes" -eq 0 ]; then
  echo "=== commit message preview ==="
  printf '%s\n' "$commit_message" | head -20
  echo "..."
  echo
  read -r -p "Land as one bundled commit? [y/N] " reply
  case "$reply" in
    [yY]|[yY][eE][sS]) ;;
    *) echo "aborted"; exit 1 ;;
  esac
fi

echo "[stage]  git add -A"
git add -A

echo "[commit] git commit"
if ! git commit -m "$commit_message"; then
  echo "[fail] git commit refused — see output above" >&2
  exit 3
fi

new_sha="$(git rev-parse --short HEAD)"
echo
echo "[done] landed as commit $new_sha"
echo
echo "next:"
echo "  git log --oneline -1"
echo "  ./run/run_desktop.sh --build-spa     # relaunch (after session 62's launcher fix lands)"
echo "  git push origin main                  # when ready to publish"
