#!/usr/bin/env bash
# svelte-check over the SPA. package.json has had this script all along and
# NOTHING ran it — not the workflow, not a local gate. Green on 2026-09-23.
set -euo pipefail
ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
echo "spa-types gate"
cd apps/aberp-ui/ui
[ -d node_modules ] || npm ci --no-audit --no-fund
npm run check
