#!/usr/bin/env bash
#
# provision_pipeline_venv_step_test.sh — S422
#
# Regression test for the `[step]` / OCP provisioning contract.
#
# Bug (pre-S422, owned by run/upgrade_prod.sh): the embedded
# `provision_pipeline_venv` function installed the venv with
# `pip install -e .` (base only) and verified only `import aberp_cad_extract`.
# The pyproject mandates `pip install -e '.[step]'` (the ~63 MB OCP wheel) for
# prod/CI so STEP-format CAD uploads extract cleanly instead of falling through
# to the NotImplementedError stub. A prod box provisioned SOLELY by
# upgrade_prod.sh could therefore extract STL but FAIL every STEP upload — a
# customer-facing silent gap. The standalone run/provision_pipeline_venv.sh
# (S421) already carried the `[step]`/OCP contract; S422 brings the inline copy
# into parity.
#
# This test pins the contract two ways:
#
#   1. PARITY (static, network-free, deterministic): BOTH provisioners must
#      install the `[step]` extra AND verify `import ... OCP`, and neither may
#      carry the pre-S422 base-only install line. This assertion FAILS against
#      the pre-S422 upgrade_prod.sh and PASSES against the S422 fix — the
#      "fails on the old form, passes on the new" demonstration, with no
#      63 MB wheel download.
#
#   2. BEHAVIORAL (uses the gate-prereq venv): run the standalone provisioner
#      against this checkout and assert its venv python can
#      `import aberp_cad_extract, OCP`. The cut gate runs
#      `./run/provision_pipeline_venv.sh` first, so this is a sub-second no-op
#      that confirms the STEP backend is genuinely importable end-to-end.
#      Fails loud (rule 12) if OCP is missing — never a false GREEN.
#
# Exit 0 = all pass; non-zero = failure (CI / cut-gate citizen).

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd -P)"
UPGRADE_PROD="${REPO_ROOT}/run/upgrade_prod.sh"
STANDALONE="${REPO_ROOT}/run/provision_pipeline_venv.sh"

fails=0
pass() { echo "[ ok ] $1"; }
fail() { echo "[FAIL] $1" >&2; fails=$((fails + 1)); }

for f in "$UPGRADE_PROD" "$STANDALONE"; do
  if [[ ! -f "$f" ]]; then
    echo "[fail] required script not found: $f" >&2
    exit 1
  fi
done

# ---------- assertion 0: the standalone is operator-invocable ----------------
# [[hulye-biztos]]: its own header documents `./run/provision_pipeline_venv.sh`,
# which only works if the file is executable. Pre-S422 it was committed 100644.
if [[ -x "$STANDALONE" ]]; then
  pass "run/provision_pipeline_venv.sh is executable (./-invocable)"
else
  fail "run/provision_pipeline_venv.sh is NOT executable — ./-invocation fails"
fi

# ---------- assertion 1: parity — both install [step] + verify OCP -----------
# upgrade_prod.sh's inline copy MUST install with the [step] extra and verify
# OCP. The pre-S422 form was `pip install --quiet -e "$pkg_dir"` (base only)
# with an `import aberp_cad_extract`-only verify — both must be gone.
check_step_contract() {
  local label="$1" file="$2"

  if grep -Eq 'pip install [^\n]*-e "\$\{?pkg_dir\}?\[step\]"' "$file"; then
    pass "${label}: installs the [step] extra (pip install -e ...[step])"
  else
    fail "${label}: missing the [step] install — STEP backend (OCP) won't be present"
  fi

  if grep -q 'import aberp_cad_extract, OCP' "$file"; then
    pass "${label}: verify gate imports OCP (STEP backend), not just the base module"
  else
    fail "${label}: verify gate does not import OCP — a base-only venv passes silently"
  fi

  # The pre-S422 base-only install line must NOT survive anywhere in the file.
  # Discriminator: a closing quote IMMEDIATELY after `pkg_dir` (e.g.
  # `-e "$pkg_dir"`) means no `[step]` extra — the bug. The fixed form
  # `-e "${pkg_dir}[step]"` has `[step]` before the closing quote, so it
  # does not match.
  if grep -Eq 'pip install [^\n]*-e "\$\{?pkg_dir\}?"' "$file"; then
    fail "${label}: a base-only 'pip install -e <pkg_dir>' (no [step]) line still present"
  else
    pass "${label}: no base-only install line remains (pre-S422 form excised)"
  fi
}

check_step_contract "upgrade_prod.sh::provision_pipeline_venv" "$UPGRADE_PROD"
check_step_contract "provision_pipeline_venv.sh (standalone)" "$STANDALONE"

# ---------- assertion 2: behavioral — provisioned venv imports OCP -----------
# Skip only if python3 is entirely absent (no toolchain), mirroring S400's
# graceful skip-on-missing-toolchain. With python3 present we run the real
# provisioner and assert OCP is importable end-to-end.
if ! command -v python3 >/dev/null 2>&1; then
  echo "[skip] python3 not on PATH — skipping the behavioral provisioning check" >&2
else
  if "$STANDALONE" "$REPO_ROOT"; then
    pass "standalone provisioner exits 0 against this checkout"
  else
    fail "standalone provisioner exited non-zero — STEP backend could not be provisioned"
  fi

  venv_python="${REPO_ROOT}/python/aberp-cad-extract/.venv/bin/python"
  if [[ -x "$venv_python" ]] \
    && "$venv_python" -c "import aberp_cad_extract, OCP" >/dev/null 2>&1; then
    pass "provisioned venv imports BOTH aberp_cad_extract AND OCP (STEP backend live)"
  else
    fail "provisioned venv cannot import aberp_cad_extract + OCP — STEP uploads would fail in prod"
  fi
fi

# ---------- result ----------------------------------------------------------
echo
if [[ $fails -eq 0 ]]; then
  echo "[pass] all provision-venv [step]/OCP assertions passed"
  exit 0
fi
echo "[fail] ${fails} assertion(s) failed" >&2
exit 1
