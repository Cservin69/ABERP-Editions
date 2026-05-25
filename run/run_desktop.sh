#!/usr/bin/env bash
#
# run_desktop.sh
#
# Launches the ABERP desktop app (Tauri + Svelte) and guarantees a graceful
# shutdown so DuckDB's exclusive write-lock is released cleanly on exit.
#
# Why this matters:
#   - DuckDB takes an exclusive file lock when opened in write mode.
#   - If the desktop process is killed with SIGKILL (or crashes), the lock
#     file (e.g. *.duckdb.wal or the OS-level fcntl lock) can persist or
#     leave the DB in a state where the next launch refuses to open it.
#   - Sending SIGTERM (the default behavior of Ctrl-C / Cmd-Q) lets the
#     Tauri shell + Rust app run their drop handlers, which closes the
#     DuckDB connection cleanly and releases the lock.
#
# What this script does:
#   1. Remembers your original working directory.
#   2. cd's into the ABERP repo root.
#   3. Launches the desktop app (cargo run with the right binary).
#   4. Traps SIGINT / SIGTERM so Ctrl-C in *this terminal* sends SIGTERM
#      (not SIGKILL) to the child process, and waits for clean exit.
#   5. On exit (success OR failure), cd's back to your original directory
#      so the terminal you launched from isn't left stranded inside the repo.
#   6. Reports the exit code.
#
# Usage:
#   ./run_desktop.sh                   # debug build (fast compile, slower runtime)
#   ./run_desktop.sh --release         # release build (slower compile, faster runtime)
#   ./run_desktop.sh --tenant default  # which tenant the backend uses (default: default)
#   ./run_desktop.sh --db PATH         # DuckDB file path (default: ./aberp.duckdb)
#   ./run_desktop.sh --build-spa       # rebuild the SPA front-end first (npm run build)
#   ./run_desktop.sh --no-codesign     # skip the ad-hoc macOS codesign pre-step (PR-46β)
#   ./run_desktop.sh -- --extra-arg    # everything after '--' is forwarded to the app
#
# Verified layout (per repo inspection 2026-05-24):
#   apps/aberp-ui/Cargo.toml           — Tauri Rust shell; [[bin]] name = "aberp-ui"
#   apps/aberp-ui/tauri.conf.json      — Tauri config
#   apps/aberp-ui/ui/package.json      — Svelte SPA front-end
#
# Config is via ENV VARS (the Tauri shell takes no CLI args):
#   ABERP_TENANT (default "default") — which tenant's NAV creds + DB to use
#   ABERP_DB     (default "./aberp.duckdb") — DuckDB file path
#   ABERP_BIN    (optional)          — path to the `aberp` CLI binary; auto-resolves
#                                      to a sibling next to the Tauri binary if unset
#
# FIRST-TIME SETUP:
#   As of PR-46α / session 62, NAV credentials are populated by the in-window
#   wizard on first launch (no terminal interaction required). When the
#   backend handshake reports `state=needs-setup`, the SPA renders a
#   four-field wizard; submit writes the four artifacts to the macOS
#   keychain and the wizard hands off to the normal invoice list.
#
#   The CLI fallback `cargo run --bin aberp -- setup-nav-credentials
#   --tenant <id>` is preserved for scripted / automation flows.
#
# AD-HOC CODESIGN PRE-STEP (PR-46β / session-63):
#   Before launch we run `codesign --force --sign - target/<profile>/aberp`
#   and the same for `aberp-ui`. The `--sign -` argument means an ad-hoc
#   signature (no Apple Developer ID required) — its purpose is purely to
#   give the Mach-O binary a stable code-signing identity across cargo
#   rebuilds so the macOS keychain's "Always Allow" ACL persists for that
#   identity. Without it, every `cargo run` produces a fresh binary that
#   the keychain treats as a NEW client and re-prompts the operator for
#   each of the four NAV-credential entries + the session-token entry.
#   Five prompts × ~7s each blew the Tauri shell's 10s HANDSHAKE_TIMEOUT
#   before this PR (a real regression Ervin saw at session-63 cold boot).
#
#   The codesign step is a no-op the first time the script runs on a
#   freshly-cloned tree (the target/ binaries don't exist yet) — cargo
#   then builds them and the next launch picks them up. The `|| true`
#   tail keeps that no-op silent. Pass --no-codesign to opt out entirely
#   (e.g. if another tool you use verifies against a different signing
#   identity).
#
# The Tauri binary expects the SPA's built assets to be present. If you've
# edited Svelte / TS source since the last build, pass --build-spa or run
# `cd apps/aberp-ui/ui && npm run build` once yourself before launching.

set -uo pipefail   # NOTE: no -e — we want to handle child exit code, not abort on it

# ---------- config (edit if your launch shape differs) -----------------------
readonly REPO_ROOT="/Users/aben/Documents/Claude/Projects/ABERP"
readonly DESKTOP_DIR="${REPO_ROOT}/apps/aberp-ui"
readonly SPA_DIR="${REPO_ROOT}/apps/aberp-ui/ui"
readonly TAURI_BIN_NAME="aberp-ui"
readonly DEFAULT_TENANT="test"
readonly SHUTDOWN_TIMEOUT_SECS=15

# Where to find each launch shape. Picked in order; first hit wins.
candidate_launch_for_mode() {
  local mode="$1"   # debug | release
  if [[ "$mode" == "release" ]]; then
    echo "cargo run --release --bin ${TAURI_BIN_NAME} --manifest-path ${DESKTOP_DIR}/Cargo.toml"
  else
    echo "cargo run --bin ${TAURI_BIN_NAME} --manifest-path ${DESKTOP_DIR}/Cargo.toml"
  fi
}

# ---------- arg parsing ------------------------------------------------------
mode="debug"
# Honor env vars if already set; otherwise use script defaults.
tenant="${ABERP_TENANT:-$DEFAULT_TENANT}"
db_path="${ABERP_DB:-./aberp.duckdb}"
build_spa=0
codesign_enabled=1
extra_args=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --release)      mode="release"; shift ;;
    --debug)        mode="debug"; shift ;;
    --tenant)       tenant="$2"; shift 2 ;;
    --db)           db_path="$2"; shift 2 ;;
    --build-spa)    build_spa=1; shift ;;
    --no-codesign)  codesign_enabled=0; shift ;;
    --help|-h)
      sed -n '2,65p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    --)             shift; extra_args=("$@"); break ;;
    *)              extra_args+=("$1"); shift ;;
  esac
done

# ---------- preserve original cwd -------------------------------------------
readonly ORIGINAL_CWD="$(pwd)"
return_to_cwd() {
  local code=$?
  echo
  echo "[exit] returning to ${ORIGINAL_CWD}"
  cd "$ORIGINAL_CWD" 2>/dev/null || true
  echo "[exit] desktop exited with code ${code}"
  exit "$code"
}
trap return_to_cwd EXIT

# ---------- preflight --------------------------------------------------------
cd "$REPO_ROOT" || { echo "repo not at $REPO_ROOT" >&2; exit 2; }

if [[ ! -f "${DESKTOP_DIR}/Cargo.toml" ]]; then
  echo "[fail] no Cargo.toml at ${DESKTOP_DIR}" >&2
  echo "       edit DESKTOP_DIR at the top of $0 if your layout differs" >&2
  exit 2
fi

if [[ ! -f "${DESKTOP_DIR}/tauri.conf.json" ]]; then
  echo "[warn] no tauri.conf.json at ${DESKTOP_DIR} — is this really the Tauri shell dir?" >&2
fi

# Optional SPA rebuild (Svelte/Vite) before launching the Rust shell.
if [[ $build_spa -eq 1 ]]; then
  if [[ ! -f "${SPA_DIR}/package.json" ]]; then
    echo "[fail] --build-spa requested but no package.json at ${SPA_DIR}" >&2
    exit 2
  fi
  echo "[spa] building front-end at ${SPA_DIR} (npm run build)"
  ( cd "$SPA_DIR" && npm run build ) || { echo "[fail] SPA build failed" >&2; exit 3; }
  echo "[spa] front-end build done"
fi

# Warn (but don't abort) if a stale DuckDB lock looks present from a prior crash.
# DuckDB's lock is typically a `.wal` companion in the same dir as the .duckdb file.
# We can't reliably know the DB path without launching the app, so just check the
# common operator-default location.
readonly OPERATOR_DB_DEFAULT="$HOME/.aberp/serve/${tenant}/aberp.duckdb"
if [[ -f "${OPERATOR_DB_DEFAULT}.wal" ]] || [[ -f "${OPERATOR_DB_DEFAULT}.tmp" ]]; then
  echo "[warn] possible stale DuckDB lock companion files near ${OPERATOR_DB_DEFAULT}"
  echo "       (a .wal or .tmp file exists — usually fine, DuckDB will recover on open;"
  echo "       if launch fails with 'database is locked', stop here and inspect)"
fi

# ---------- ad-hoc codesign (macOS keychain ACL stability) ------------------
# See the AD-HOC CODESIGN PRE-STEP block at the top of this file for the
# full WHY. Short version: a stable ad-hoc identity means the macOS
# keychain's "Always Allow" ACL persists across cargo rebuilds, so the
# five-prompt cold-boot cycle Ervin saw at session-63 doesn't recur on
# subsequent launches.
if [[ "$(uname -s)" == "Darwin" && $codesign_enabled -eq 1 ]]; then
  if [[ "$mode" == "release" ]]; then
    cs_dir="${REPO_ROOT}/target/release"
  else
    cs_dir="${REPO_ROOT}/target/debug"
  fi
  for cs_bin in aberp aberp-ui; do
    if [[ -f "${cs_dir}/${cs_bin}" ]]; then
      codesign --force --sign - "${cs_dir}/${cs_bin}" 2>/dev/null \
        && echo "[codesign] ad-hoc signed ${cs_dir}/${cs_bin}" \
        || echo "[codesign] could not sign ${cs_dir}/${cs_bin} (continuing — keychain may re-prompt)"
    fi
  done
  unset cs_dir cs_bin
elif [[ $codesign_enabled -eq 0 ]]; then
  echo "[codesign] skipped (--no-codesign)"
fi

# ---------- launch ----------------------------------------------------------
# The Tauri shell reads config from env vars, NOT CLI args. Export them.
export ABERP_TENANT="$tenant"
export ABERP_DB="$db_path"

launch_cmd="$(candidate_launch_for_mode "$mode")"
echo "[launch] mode=${mode}"
echo "[launch] ABERP_TENANT=${tenant} ABERP_DB=${db_path}"
echo "[launch] ${launch_cmd} ${extra_args[*]:-}"
echo "[launch] (Ctrl-C in this terminal sends SIGTERM to the app — graceful shutdown)"
echo "[launch] First-run NAV-credentials setup is now in the SPA itself; no"
echo "[launch] terminal step is required. (CLI fallback for automation:"
echo "[launch]  cargo run --bin aberp -- setup-nav-credentials --tenant ${tenant})"
echo

# Launch in background so we control the signal handling
# shellcheck disable=SC2086
$launch_cmd ${extra_args[@]:+"${extra_args[@]}"} &
child_pid=$!

# Forward Ctrl-C / SIGTERM to the child as SIGTERM (not SIGKILL).
# Then wait up to SHUTDOWN_TIMEOUT_SECS for the child to exit cleanly.
graceful_stop() {
  echo
  echo "[shutdown] forwarding SIGTERM to PID ${child_pid} (graceful close — DuckDB lock will release)"
  kill -TERM "$child_pid" 2>/dev/null || true

  # Wait up to SHUTDOWN_TIMEOUT_SECS for the process to actually exit
  local waited=0
  while kill -0 "$child_pid" 2>/dev/null; do
    if [[ $waited -ge $SHUTDOWN_TIMEOUT_SECS ]]; then
      echo "[shutdown] timeout after ${SHUTDOWN_TIMEOUT_SECS}s; escalating to SIGKILL"
      echo "[shutdown] WARNING: DuckDB lock may be left stale — next launch may need recovery"
      kill -KILL "$child_pid" 2>/dev/null || true
      break
    fi
    sleep 1
    waited=$((waited + 1))
  done
}
trap 'graceful_stop' INT TERM

# Block until the child exits (either naturally or via our signal handler).
# `wait` returns the child's exit code; we propagate it via return_to_cwd().
wait "$child_pid"
