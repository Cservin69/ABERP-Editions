#!/usr/bin/env bash
# The load-bearing durability e2e, run by name before the workspace suite so
# their verdict stands on its own (rationale per test: the ci.yml comments).
#   ADR-0098  shared DuckDB Handle, read-only open beside the live writer
#   ADR-0105  audit append lock domains cannot interleave — single-threaded:
#             the guard measures lock WAIT, and parallel tests add noise
#   ADR-0110  durable_ack actually reaches the filesystem (main file + WAL)
# aberp-db is edition-agnostic: no production feature. These were three
# inline `cargo test -p` steps in ci.yml, run nowhere else with these flags.
set -euo pipefail
ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
echo "durability-e2e gate"
cargo test -p aberp-db --test handle_concurrency_e2e --locked -- --nocapture
cargo test -p aberp-db --test audit_lock_domain_e2e --locked -- --nocapture --test-threads=1
cargo test -p aberp-db --test durable_ack_fault_injection --locked -- --nocapture
