#!/usr/bin/env bash
# The Defense workspace build, every target. Was inline in ci.yml only.
# --all-targets builds tests/benches/examples too, so compile errors surface
# here rather than later in test or clippy. The SPA bundle must exist first
# (Tauri's generate_context! reads ui/dist): run ci/spa-build.sh before this.
set -euo pipefail
ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
FEATURES="${ABERP_EDITION_FEATURES-"--features production"}"
echo "rust-build gate ($FEATURES)"
# shellcheck disable=SC2086
cargo build --workspace --locked --all-targets $FEATURES
