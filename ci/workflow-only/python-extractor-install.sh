#!/usr/bin/env bash
# CI-runner setup, not a check: installs aberp-cad-extract with [step,dev]
# (OCCT runtime + pytest) and exports an ABSOLUTE ABERP_TEST_PYTHON for the
# Rust wrapper tests and ci/python-extractor-tests.sh. Listed in ci/gate-parity.sh.
set -euo pipefail
ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
python -m pip install --upgrade pip
python -m pip install -e 'python/aberp-cad-extract[step,dev]'
py="$(python -c 'import sys; print(sys.executable)')"
if [ -n "${GITHUB_ENV:-}" ]; then
  echo "ABERP_TEST_PYTHON=$py" >> "$GITHUB_ENV"
else
  echo "ABERP_TEST_PYTHON=$py"
fi
