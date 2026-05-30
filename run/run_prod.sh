#!/usr/bin/env bash
set -euo pipefail

# S165 / prod-prep PR #1 — PRODUCTION launcher. Compiles the `aberp`
# backend with `--features production` (the COMPILE-TIME switch that
# flips the NAV endpoint to the real host, lifts the dev-build refusal
# gate, drops the `TEST-` invoice-number prefix, and arms the boot
# banner). The three hülye-biztos guards below make a wrong-environment
# launch structurally impossible.

# Hülye-biztos guard #1: must be tenant=prod
if [[ "${ABERP_TENANT:-}" != "prod" ]]; then
  export ABERP_TENANT=prod
fi

# Hülye-biztos guard #2: explicit prod paths
export ABERP_DB="${HOME}/.aberp/prod/aberp.duckdb"
mkdir -p "${HOME}/.aberp/prod"

# Hülye-biztos guard #3: compile + run with production feature
echo "Building prod binary (features=production)..."
cd "$(dirname "$0")/.."
cargo build --release --features production --bin aberp
cargo build --release --bin aberp-ui

echo "Launching ABERP in PRODUCTION mode..."
ABERP_TENANT=prod ABERP_DB="${HOME}/.aberp/prod/aberp.duckdb" \
  cargo run --release --features production --bin aberp-ui
