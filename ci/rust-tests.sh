#!/usr/bin/env bash
# The workspace test suite. Was inline in ci.yml only.
# The three ADR-0098/0105/0110 e2e steps in ci.yml are narrower runs of the
# same suite, surfaced for visibility; ci/gate-parity.sh allows them for that
# reason only.
set -euo pipefail
ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
FEATURES="${ABERP_EDITION_FEATURES-"--features production"}"
echo "rust-tests gate ($FEATURES)"
# shellcheck disable=SC2086
cargo test --workspace --locked $FEATURES
