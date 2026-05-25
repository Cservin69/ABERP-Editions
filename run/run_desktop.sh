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
#   3. Optionally rebuilds the SPA front-end (auto-detect when stale,
#      or forced via --build-spa).
#   4. Runs `cargo build` for both binaries (aberp and aberp-ui).
#   5. Ad-hoc codesigns both Mach-O binaries on Darwin (so the macOS
#      keychain "Always Allow" ACL persists across rebuilds when the
#      binary content is unchanged).
#   6. Directly executes target/<profile>/aberp-ui (NOT `cargo run`) so
#      the codesigned bytes are exactly what runs — no cargo step
#      between codesign and exec to overwrite the signature.
#   7. Traps SIGINT / SIGTERM so Ctrl-C in *this terminal* sends SIGTERM
#      (not SIGKILL) to the child process, and waits for clean exit.
#   8. On exit (success OR failure), cd's back to your original directory
#      so the terminal you launched from isn't left stranded inside the repo.
#   9. Reports the exit code.
#
# Usage:
#   ./run_desktop.sh                   # debug build (fast compile, slower runtime)
#   ./run_desktop.sh --release         # release build (slower compile, faster runtime)
#   ./run_desktop.sh --tenant default  # which tenant the backend uses (default: test)
#   ./run_desktop.sh --db PATH         # DuckDB file path (default: ./aberp.duckdb)
#   ./run_desktop.sh --build-spa       # force-rebuild the SPA front-end (npm run build)
#   ./run_desktop.sh --no-build-spa    # skip the SPA staleness auto-detect (launch with existing dist)
#   ./run_desktop.sh --no-codesign     # skip the ad-hoc macOS codesign post-build step
#   ./run_desktop.sh -- --extra-arg    # everything after '--' is forwarded to the app
#
# Verified layout (per repo inspection 2026-05-24):
#   apps/aberp-ui/Cargo.toml           — Tauri Rust shell; [[bin]] name = "aberp-ui"
#   apps/aberp-ui/tauri.conf.json      — Tauri config
#   apps/aberp-ui/ui/package.json      — Svelte SPA front-end
#   apps/aberp/Cargo.toml              — CLI; [[bin]] name = "aberp"
#
# Config is via ENV VARS (the Tauri shell takes no CLI args):
#   ABERP_TENANT (default "test")    — which tenant's NAV creds + DB to use
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
# AD-HOC CODESIGN POST-BUILD STEP (PR-46β + PR-46γ):
#   After `cargo build` produces the binaries we run
#   `codesign --force --sign - target/<profile>/aberp` (and the same
#   for `aberp-ui`). The `--sign -` argument means an ad-hoc signature
#   (no Apple Developer ID required); its purpose is purely to give
#   the Mach-O binaries a stable code-signing identity for the macOS
#   keychain's "Always Allow" ACL.
#
#   ORDERING IS LOAD-BEARING (PR-46γ / session 66):
#     `cargo build` MUST run BEFORE `codesign`, and the launcher MUST
#     `exec` the resulting binary directly — NOT via `cargo run`.
#     PR-46β's first cut ran `codesign` before `cargo run`, but cargo's
#     internal recompile-when-stale step then overwrote the freshly-
#     codesigned binary with an unsigned one, invalidating the keychain
#     ACL on every Rust source change. The correct ordering is
#     `build → codesign → exec` so the codesigned bytes are exactly
#     what runs.
#
#   Without this step every cargo rebuild produces a fresh binary that
#   the keychain treats as a NEW client and re-prompts the operator for
#   each of the four NAV-credential entries + the session-token entry.
#   Five prompts × ~7s each blew the Tauri shell's previous 10s
#   HANDSHAKE_TIMEOUT (session-63 regression Ervin observed); the
#   post-PR-46β / PR-46γ flow:
#
#     - First launch after a Rust source change: cargo rebuilds → new
#       binary → new cdhash → keychain prompts ONCE per credential
#       (operator clicks "Always Allow"); subsequent launches against
#       the SAME unchanged binary cdhash boot in 1-2s with zero
#       prompts.
#     - First launch on a fresh clone: same — pay one round of prompts,
#       then back to the fast path.
#
#   The codesign step is a no-op on platforms other than Darwin and
#   when `--no-codesign` is passed (e.g. if another tool you use
#   verifies against a different signing identity).
#
# SPA STALENESS AUTO-DETECT (PR-46γ / session 66):
#   Before launch we check whether `apps/aberp-ui/ui/dist/index.html`
#   exists AND is newer than every `.svelte` / `.ts` / `.css` source
#   file under `apps/aberp-ui/ui/src/`. If dist is missing OR any
#   source file is newer, we run `npm run build` automatically. This
#   replaces the previous workflow where the operator had to remember
#   to pass `--build-spa` after every Svelte / TS edit (and forgetting
#   meant the desktop window mounted a stale UI silently — exactly the
#   class of bug CLAUDE.md rule 12 ("fail loud") prohibits).
#
#   The `--build-spa` flag is still honoured as a force-rebuild
#   override. The new `--no-build-spa` flag opts out of the auto-
#   detect (for cases where the operator wants to launch with
#   intentionally-stale dist for debugging).

set -uo pipefail   # NOTE: no -e — we want to handle child exit code, not abort on it

# ---------- config (edit if your launch shape differs) -----------------------
readonly REPO_ROOT="/Users/aben/Documents/Claude/Projects/ABERP"
readonly DESKTOP_DIR="${REPO_ROOT}/apps/aberp-ui"
readonly SPA_DIR="${REPO_ROOT}/apps/aberp-ui/ui"
readonly TAURI_BIN_NAME="aberp-ui"
readonly ABERP_BIN_NAME="aberp"
readonly DEFAULT_TENANT="test"
readonly SHUTDOWN_TIMEOUT_SECS=15

# ---------- arg parsing ------------------------------------------------------
mode="debug"
# Honor env vars if already set; otherwise use script defaults.
tenant="${ABERP_TENANT:-$DEFAULT_TENANT}"
db_path="${ABERP_DB:-./aberp.duckdb}"
build_spa=0           # 1 = force rebuild (--build-spa)
no_build_spa=0        # 1 = skip auto-detect (--no-build-spa)
codesign_enabled=1
extra_args=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --release)       mode="release"; shift ;;
    --debug)         mode="debug"; shift ;;
    --tenant)        tenant="$2"; shift 2 ;;
    --db)            db_path="$2"; shift 2 ;;
    --build-spa)     build_spa=1; shift ;;
    --no-build-spa)  no_build_spa=1; shift ;;
    --no-codesign)   codesign_enabled=0; shift ;;
    --help|-h)
      sed -n '2,117p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    --)              shift; extra_args=("$@"); break ;;
    *)               extra_args+=("$1"); shift ;;
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

# ---------- SPA build (force, auto-detect, or skip) -------------------------
# Decision matrix:
#   --build-spa     → force rebuild (overrides everything)
#   --no-build-spa  → skip auto-detect entirely (operator wants stale dist)
#   neither flag    → auto-detect: rebuild iff dist/index.html is missing
#                     OR any .svelte/.ts/.css under src/ is newer than it
#
# Why this matters: pre-PR-46γ the operator had to remember `--build-spa`
# after every Svelte/TS edit. Forgetting silently mounted a stale UI —
# the "completes successfully with X% silently skipped" failure class
# CLAUDE.md rule 12 prohibits. The auto-detect makes the default safe.
if [[ ! -f "${SPA_DIR}/package.json" ]]; then
  echo "[warn] no package.json at ${SPA_DIR} — skipping SPA build/auto-detect"
elif [[ $build_spa -eq 1 ]]; then
  echo "[spa] forced rebuild requested (--build-spa)"
  echo "[spa] building front-end at ${SPA_DIR} (npm run build)"
  ( cd "$SPA_DIR" && npm run build ) || { echo "[fail] SPA build failed" >&2; exit 3; }
  echo "[spa] front-end build done"
elif [[ $no_build_spa -eq 1 ]]; then
  echo "[spa] auto-detect skipped (--no-build-spa); launching with existing dist as-is"
else
  # Auto-detect: is dist stale relative to src/?
  dist_html="${SPA_DIR}/dist/index.html"
  stale=0
  reason=""
  if [[ ! -f "$dist_html" ]]; then
    stale=1
    reason="dist/index.html missing"
  else
    # `find -newer` returns files in src/ newer than the dist marker.
    # -print -quit stops at the first hit (we only need to know IF any exist).
    newer_hit="$(find "${SPA_DIR}/src" -type f \
                   \( -name '*.svelte' -o -name '*.ts' -o -name '*.css' \) \
                   -newer "$dist_html" -print -quit 2>/dev/null)"
    if [[ -n "$newer_hit" ]]; then
      stale=1
      reason="source newer than dist (e.g. ${newer_hit#${SPA_DIR}/})"
    fi
  fi
  if [[ $stale -eq 1 ]]; then
    echo "[spa] dist is stale, rebuilding… (reason: ${reason})"
    ( cd "$SPA_DIR" && npm run build ) || { echo "[fail] SPA build failed" >&2; exit 3; }
    echo "[spa] front-end build done"
  else
    echo "[spa] dist is fresh, skipping rebuild"
  fi
  unset dist_html stale reason newer_hit
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

# ---------- cargo build (must run BEFORE codesign) --------------------------
# We build both bins explicitly (instead of `cargo run`) so the codesign
# step in the next block runs on bytes that won't be overwritten by a
# cargo recompile between codesign and exec. See "AD-HOC CODESIGN
# POST-BUILD STEP" at the top of this file for the full WHY.
if [[ "$mode" == "release" ]]; then
  bin_dir="${REPO_ROOT}/target/release"
  build_cmd=(cargo build --release --bin "${ABERP_BIN_NAME}" --bin "${TAURI_BIN_NAME}")
else
  bin_dir="${REPO_ROOT}/target/debug"
  build_cmd=(cargo build --bin "${ABERP_BIN_NAME}" --bin "${TAURI_BIN_NAME}")
fi
echo "[build] ${build_cmd[*]}"
"${build_cmd[@]}" || { echo "[fail] cargo build failed" >&2; exit 4; }

binary_path="${bin_dir}/${TAURI_BIN_NAME}"
if [[ ! -f "$binary_path" ]]; then
  echo "[fail] cargo build reported success but ${binary_path} is missing" >&2
  exit 4
fi

# ---------- ad-hoc codesign (macOS keychain ACL stability) ------------------
# See the AD-HOC CODESIGN POST-BUILD STEP block at the top of this file
# for the full WHY. Short version: a stable ad-hoc identity means the
# macOS keychain's "Always Allow" ACL persists across launches when the
# binary content is unchanged, so the five-prompt cold-boot cycle Ervin
# saw at session-63 doesn't recur on subsequent launches.
#
# This MUST run AFTER cargo build (so the bytes we sign are the bytes
# that will actually execute) and BEFORE the launch line below.
if [[ "$(uname -s)" == "Darwin" && $codesign_enabled -eq 1 ]]; then
  for cs_bin in "$ABERP_BIN_NAME" "$TAURI_BIN_NAME"; do
    if [[ -f "${bin_dir}/${cs_bin}" ]]; then
      codesign --force --sign - "${bin_dir}/${cs_bin}" 2>/dev/null \
        && echo "[codesign] ad-hoc signed ${bin_dir}/${cs_bin}" \
        || echo "[codesign] could not sign ${bin_dir}/${cs_bin} (continuing — keychain may re-prompt)"
    fi
  done
  unset cs_bin
elif [[ $codesign_enabled -eq 0 ]]; then
  echo "[codesign] skipped (--no-codesign)"
fi

# ---------- launch ----------------------------------------------------------
# The Tauri shell reads config from env vars, NOT CLI args. Export them.
export ABERP_TENANT="$tenant"
export ABERP_DB="$db_path"

echo "[launch] mode=${mode}"
echo "[launch] ABERP_TENANT=${tenant} ABERP_DB=${db_path}"
echo "[launch] ${binary_path} ${extra_args[*]:-}"
echo "[launch] (Ctrl-C in this terminal sends SIGTERM to the app — graceful shutdown)"
echo "[launch] First-run NAV-credentials setup is in the SPA itself; no"
echo "[launch] terminal step is required. (CLI fallback for automation:"
echo "[launch]  cargo run --bin aberp -- setup-nav-credentials --tenant ${tenant})"
echo

# Launch the codesigned binary DIRECTLY (not via `cargo run`) so the
# bytes that execute are the exact bytes we just signed. Run in
# background so we control the signal handling.
"$binary_path" ${extra_args[@]:+"${extra_args[@]}"} &
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
