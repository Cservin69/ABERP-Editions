#!/usr/bin/env bash
# Strict XML well-formedness lint for every TRACKED *.svg in the repo.
#
# Why this exists: PROD_Defense_v0.2.10 shipped apps/aberp-ui/ui/static/
# favicon.svg and brand-mark.svg with a literal "--" inside an XML comment
# (XML 1.0 §2.5 forbids "--" within a comment). Browsers are lenient and render
# such files fine, so the defect was LATENT — but strict parsers (expat,
# rsvg-convert, resvg) refuse them outright. One favicon rasteriser away from
# live. This lint fails CI on any malformed tracked SVG so it can't recur.
#
# Coverage is `git ls-files '*.svg'` ON PURPOSE: it picks up ANY .svg added
# anywhere in the tree, automatically. Do NOT hand-scope this to the directories
# we happen to remember today — a new asset elsewhere would then slip past, and
# that exact blind-spot is the class of bug we've been closing all week.
#
# The parser is Python's expat (via xml.dom.minidom) — the same class of strict
# parser that refused the shipped files — so "green here" means "a strict SVG
# consumer will accept every tracked asset".
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

svgs=()
while IFS= read -r f; do
  [ -n "$f" ] && svgs+=("$f")
done < <(git ls-files '*.svg')

if [ "${#svgs[@]}" -eq 0 ]; then
  echo "svg-lint: no tracked *.svg files — nothing to check"
  exit 0
fi

fail=0
for f in "${svgs[@]}"; do
  if err=$(python3 - "$f" <<'PY' 2>&1
import sys, xml.dom.minidom as minidom
try:
    minidom.parse(sys.argv[1])
except Exception as e:
    sys.exit(f"{type(e).__name__}: {e}")
PY
  ); then
    echo "svg-lint: ok        $f"
  else
    echo "svg-lint: MALFORMED $f"
    printf '%s\n' "$err" | sed 's/^/    /'
    fail=1
  fi
done

if [ "$fail" -ne 0 ]; then
  echo "svg-lint: FAIL — at least one tracked SVG is not well-formed XML" >&2
  exit 1
fi

echo "svg-lint: PASS — ${#svgs[@]} tracked SVG(s) are well-formed"
