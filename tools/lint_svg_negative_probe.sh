#!/usr/bin/env bash
# Prove tools/lint_svg_wellformed.sh has TEETH.
#
# A lint nobody has watched fail is decoration. This probe plants a deliberately
# malformed SVG that the REAL `git ls-files '*.svg'` glob will return (via an
# intent-to-add index entry), asserts the lint goes RED, then removes it and
# asserts the lint goes GREEN. It exercises the exact code path CI runs — not a
# side door.
#
# The fixture is NEVER committed: it is `git add -N`'d (intent-to-add, so
# ls-files sees it) and then reset + deleted, even on failure (trap).
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"
lint="tools/lint_svg_wellformed.sh"
probe="._svg_lint_probe_malformed.svg"

cleanup() {
  git reset -q -- "$probe" 2>/dev/null || true
  rm -f "$probe"
}
trap cleanup EXIT

# Malformed on PURPOSE: literal "--" inside an XML comment — the exact
# XML 1.0 §2.5 violation that shipped in PROD_Defense_v0.2.10.
printf '%s\n' \
  '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1 1">' \
  '  <!-- probe -- deliberately malformed comment -->' \
  '  <rect width="1" height="1"/>' \
  '</svg>' > "$probe"
git add -N -- "$probe"

echo "== probe 1/2: lint MUST go RED with a malformed tracked SVG present =="
if bash "$lint"; then
  echo "PROBE FAILED: lint passed while a malformed tracked SVG was present" >&2
  exit 1
fi
echo "  -> lint correctly went RED"

cleanup
trap - EXIT

echo "== probe 2/2: lint MUST go GREEN once the malformed SVG is removed =="
if ! bash "$lint"; then
  echo "PROBE FAILED: lint still RED after removing the malformed SVG" >&2
  exit 1
fi
echo "  -> lint correctly GREEN"

echo "svg-lint negative probe: PASS — the lint fails closed"
