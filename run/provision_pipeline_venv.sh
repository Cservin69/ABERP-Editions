#!/usr/bin/env bash
#
# provision_pipeline_venv.sh — S421 — stand-alone, gate-invocable provisioner
# for the auto-quoting Python venv (`aberp_cad_extract`).
#
# WHY THIS EXISTS (separate from upgrade_prod.sh):
#   `upgrade_prod.sh` provisions the same venv (its `provision_pipeline_venv`
#   function, S282), but it `exec`s into `run_prod.sh` at the end — you cannot
#   run it just to materialize a venv for a CI/cut gate. The cut gate runs
#   `cargo test` in an ISOLATED worktree that has NO venv (the canonical
#   `python/aberp-cad-extract/.venv` is gitignored, so each worktree starts
#   empty). Without a venv there, the CAD-smoke tests
#   (`aberp-cad-extract-wrapper`) fall through to a system `python3` that
#   lacks the module and fail loud with an ImportError — a false RED for a
#   tree that is actually green. The cut session runs THIS script in the gate
#   worktree first; the test harness (`tests/common/mod.rs::test_python_bin`)
#   then auto-discovers the venv with `ABERP_TEST_PYTHON` UNSET — honoring
#   [[trust-code-not-operator]] (no env var to remember).
#
#   It is a deliberate, cross-referenced near-duplicate of upgrade_prod.sh's
#   inline copy. Kept separate rather than refactoring upgrade_prod.sh because
#   that script's `exec`-into-prod flow must not be perturbed by a docs/test
#   sweep. If the two ever need to converge, extract a shared helper THEN.
#
# Idempotent: if the venv already imports the module it is a sub-second no-op.
# Fail-loud: exits non-zero if the module is not importable at the end, so a
# gate that calls it knows provisioning failed rather than silently RED'ing
# later in `cargo test`.
#
# Usage:
#   ./run/provision_pipeline_venv.sh            # repo root = this script's ../
#   ./run/provision_pipeline_venv.sh <repo_root>

set -euo pipefail

repo_root="${1:-"$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"}"
pkg_dir="${repo_root}/python/aberp-cad-extract"
venv_dir="${pkg_dir}/.venv"
venv_python="${venv_dir}/bin/python"

if [[ ! -d "$pkg_dir" ]]; then
  echo "ERROR: auto-quoting Python package missing at ${pkg_dir}" >&2
  exit 2
fi

# Already good → no-op. Require BOTH the base module AND the OCP STEP
# backend (the `[step]` extra), since the gate's STEP smoke test needs OCP;
# a base-only venv must fall through and get `.[step]` added.
if [[ -x "$venv_python" ]] \
  && "$venv_python" -c "import aberp_cad_extract, OCP" >/dev/null 2>&1; then
  echo "pipeline venv OK at ${venv_dir} (module + OCP) — no-op"
  exit 0
fi

if ! command -v python3 >/dev/null 2>&1; then
  echo "ERROR: python3 not found on PATH — cannot provision the venv" >&2
  exit 3
fi

# A half-built venv (e.g. a broken symlink after a python upgrade) — recreate.
if [[ -d "$venv_dir" ]] && [[ ! -x "$venv_python" ]]; then
  echo "removing stale venv directory: ${venv_dir}" >&2
  rm -rf "$venv_dir"
fi

echo "provisioning auto-quoting Python venv at ${venv_dir} ..."
python3 -m venv "$venv_dir"
"$venv_python" -m pip install --quiet --upgrade pip >/dev/null 2>&1 || \
  echo "pip --upgrade failed — continuing with bundled pip" >&2
# Install WITH the `[step]` extra (the ~63 MB OCP wheel): the pyproject says
# "Production installs (and CI) MUST install with `.[step]`" so STEP
# submissions extract cleanly instead of the NotImplementedError stub. Both
# CAD-smoke tests then pass — without it `step_extract_smoke` REDs on a
# missing OCP backend. ADR-0112 Part A made the pipeline STEP-only, so this
# is no longer "one of two smoke lanes needs OCP": OCP is required for the
# extractor to do anything at all.
"$venv_python" -m pip install --quiet -e "${pkg_dir}[step]"

# Verify — fail loud if the module OR the OCP STEP backend isn't importable.
if "$venv_python" -c "import aberp_cad_extract, OCP" >/dev/null 2>&1; then
  echo "pipeline venv provisioned at ${venv_dir} (module + OCP)"
  exit 0
fi
echo "ERROR: venv created but aberp_cad_extract / OCP still not importable" >&2
exit 4
