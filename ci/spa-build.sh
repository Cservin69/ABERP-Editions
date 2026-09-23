#!/usr/bin/env bash
# The SPA bundle (apps/aberp-ui/ui/dist). Was a multi-line step in ci.yml only.
# Also the producer of the dist that ci/rust-build.sh needs.
set -euo pipefail
ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
echo "spa-build gate"
cd apps/aberp-ui/ui
npm ci --no-audit --no-fund
npm run build
