#!/usr/bin/env bash
# cargo-deny: advisories, licenses, bans, sources. ONE producer for both
# ci.yml and supply-chain-schedule.yml, which each carried their own copy.
#
# The four --deny lints are the drift-proofing (rationale: deny.toml and the
# ci.yml step comment). They are why this was RED on main on 2026-09-23 and
# nobody knew: RustSec withdrew eight advisories deny.toml still ignored, and
# the check lived only in workflows that do not run.
set -euo pipefail
ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
echo "supply-chain gate"
cargo deny check \
  --deny advisory-not-detected \
  --deny license-not-encountered \
  --deny license-exception-not-encountered \
  --deny unmatched-skip
