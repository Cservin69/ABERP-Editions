#!/usr/bin/env bash
# The SPA vitest suite (88 files, 1587 tests on 2026-09-23). Like spa-types,
# it was in package.json and run by nothing.
set -euo pipefail
ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
echo "spa-tests gate"
cd apps/aberp-ui/ui
[ -d node_modules ] || npm ci --no-audit --no-fund
npm test
