#!/usr/bin/env bash
# Formatting. Was an inline step in ci.yml only; see ci/gate-parity.sh.
set -euo pipefail
ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
echo "rust-formatting gate"
cargo fmt --all -- --check
