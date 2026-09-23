#!/usr/bin/env bash
# The Python extractor suite (python/aberp-cad-extract). Was inline in ci.yml
# only. Needs the package installed with [step,dev]: on CI that is
# ci/workflow-only/python-extractor-install.sh (which sets ABERP_TEST_PYTHON);
# locally it is ABERP_TEST_PYTHON, else the repo's .venv, else python3.
#
# The `import OCP` line is a GUARD: test_holes.py opens with
# pytest.importorskip("OCP"), so without a working OCCT runtime most of the
# suite SKIPS and pytest still exits 0. Asserting the import first makes that
# silent hole a loud failure.
set -euo pipefail
ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
PKG="python/aberp-cad-extract"
if [ -n "${ABERP_TEST_PYTHON:-}" ]; then PY="$ABERP_TEST_PYTHON"
elif [ -x "$PKG/.venv/bin/python" ]; then PY="$ROOT/$PKG/.venv/bin/python"
else PY="python3"; fi
echo "python-extractor-tests gate ($PY)"
cd "$PKG"
"$PY" -c "import OCP"
if ! "$PY" -m pytest --version >/dev/null 2>&1; then
  echo "  ✗ pytest is not installed for $PY" >&2
  echo "    run/provision_pipeline_venv.sh installs [step] only; this gate needs [step,dev]:" >&2
  echo "    $PY -m pip install -e '$PKG[step,dev]'" >&2
  exit 1
fi
"$PY" -m pytest
