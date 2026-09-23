#!/usr/bin/env bash
# Clippy, warnings denied (ADR-0007). Was inline in ci.yml only.
set -euo pipefail
ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
FEATURES="${ABERP_EDITION_FEATURES-"--features production"}"
echo "clippy gate ($FEATURES)"
# shellcheck disable=SC2086
cargo clippy --workspace --all-targets --locked $FEATURES -- -D warnings
