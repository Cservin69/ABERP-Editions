#!/usr/bin/env bash
# Edition-isolation + crash-safe integration tests, by name (SAW-OFF.md
# chunk-3 handoff), so they run even if the workspace set is ever narrowed.
# The production feature exists only on aberp / aberp-ui, so the
# edition-agnostic crates run without it. Was a multi-line step in ci.yml only.
set -euo pipefail
ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
FEATURES="${ABERP_EDITION_FEATURES-"--features production"}"
echo "named-integration-tests gate ($FEATURES)"
# shellcheck disable=SC2086
cargo test -p aberp --test edition_db_isolation $FEATURES --locked
# shellcheck disable=SC2086
cargo test -p aberp --test edition_snapshot_isolation $FEATURES --locked
cargo test -p aberp-snapshot --test crash_safe_checkpoint_tests --locked
cargo test -p aberp-snapshot --test edition_isolation_tests --locked
cargo test -p aberp-audit-ledger ensure_consistent_refuses_and_preserves_when_mirror_ahead_of_db --locked
