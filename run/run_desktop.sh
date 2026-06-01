#!/usr/bin/env bash
#
# run_desktop.sh
#
# Launches the ABERP desktop app (Tauri 2 + Svelte) via `tauri dev`.
# This is the canonical Tauri 2 dev-loop shape: tauri-CLI spawns Vite
# (via tauri.conf.json's `beforeDevCommand`) AND runs the Rust shell,
# all in one process group, with hot-reload for the SPA.
#
# Why `tauri dev` (and NOT plain `cargo run`):
#   The Tauri webview loads the URL set by `tauri.conf.json.build.devUrl`
#   — by default `http://localhost:5173`. That URL is served by Vite,
#   which Tauri starts via `beforeDevCommand`. If the launcher invokes
#   `cargo run` (or `target/debug/aberp-ui` directly), tauri-CLI is
#   bypassed, `beforeDevCommand` never fires, Vite never starts, and
#   the webview opens to a blank page. Sessions 63+66 made this
#   regression by running the binary directly with the codesign-then-
#   exec pattern; PR-46δ restores the working pattern.
#
# Why we pre-build (PR-52 — and why we removed the explicit codesign step):
#   macOS keychain ACLs ("Always Allow") key off the running binary's CDHash.
#   The Apple Silicon linker (ld) automatically applies an ad-hoc Mach-O
#   signature on every link — that's the `flags=0x20002(adhoc,linker-signed)`
#   you see in `codesign -dvv` output. The linker-signed CDHash is a stable
#   function of the binary's code bytes: SAME bytes → SAME CDHash → ACL hits.
#
#   For the linker-signed CDHash to be stable across launcher invocations,
#   tauri-CLI's internal `cargo run --bin aberp-ui` (which fires on every
#   `tauri dev` launch) must be a TRUE NO-OP — no link, no copy, no byte
#   change. That requires two things in the pre-build:
#
#     (a) Flag-match: pre-build with the same flags tauri-CLI invokes
#         (`--no-default-features` injected by tauri-cli's `dev_options`).
#         Mismatched flags = different cargo fingerprint bucket = tauri
#         re-links from scratch on launch.
#
#     (b) Double-build: cargo's incremental linker re-runs once after the
#         first link, even with no source change, before fingerprints
#         "settle." Running `cargo build` twice in the pre-build absorbs
#         this re-link so tauri-CLI's `cargo run` is the third invocation —
#         which IS a true no-op.
#
#   With (a) + (b) in place, the linker-signed CDHash from the second pre-
#   build pass survives across all subsequent cargo invocations within and
#   between launcher runs. The operator clicks "Always Allow" once for each
#   keychain item on the first launch; subsequent launches match the ACL.
#
#   Sessions 63/66/67 each added an explicit `codesign --force --sign -`
#   pass thinking it would "stabilize" the ad-hoc identity. Empirically it
#   did the opposite: explicit codesign rewrites the binary's `__LINKEDIT`
#   segment and changes the file size, which trips cargo's fingerprint check
#   on the next invocation and triggers a re-link that undoes the signature.
#   PR-52 removes the codesign step entirely. The linker's native ad-hoc
#   signature is already stable.
#
#   Limitation: any Rust source change re-links the binary on the next run,
#   producing a new CDHash. The operator will see one "Always Allow" prompt
#   per keychain item the first time the new build runs, then it's stable
#   again until the next source change. This is unavoidable without a real
#   Apple-Developer-issued signing identity (which is out of scope here).
#
# What this script does:
#   1. Remembers your original working directory.
#   2. cd's into the ABERP repo root for the cargo build step.
#   3. Pre-builds `aberp` then `aberp-ui` in SEPARATE cargo invocations,
#      each one TWICE — the second pass settles cargo's linker
#      fingerprint so tauri-CLI's later `cargo run` is a true no-op
#      and the linker-signed ad-hoc CDHash stays stable across runs.
#   4. (No explicit codesign — see PR-52 note above. --no-codesign is
#      retained as an inert no-op for backward compat.)
#   5. Frees TCP port 5173 if a prior run left Vite stranded there.
#   6. cd's into `apps/aberp-ui/` and runs `./ui/node_modules/.bin/tauri dev`.
#      tauri-CLI then:
#        - executes `beforeDevCommand` from tauri.conf.json (which is
#          `{ "script": "npm run dev", "cwd": "ui" }`, i.e. cd into
#          `apps/aberp-ui/ui/` and run `npm run dev` → Vite serves
#          http://localhost:5173)
#        - runs `cargo run --no-default-features --bin aberp-ui` (a
#          true no-op rebuild since we pre-built twice with matching
#          flags; execs the linker-signed binary unmodified)
#        - the Tauri webview loads `devUrl` (http://localhost:5173)
#          and the SPA mounts with hot-reload enabled.
#   7. Puts the whole thing in one process group; Ctrl-C in this
#      terminal sends SIGTERM to the group so Vite, cargo, and the
#      aberp-ui binary all shut down gracefully. The SIGTERM lets
#      the aberp-ui drop handlers release the DuckDB write-lock.
#   8. Belt-and-suspenders: after the wait returns, force-kill any
#      stray PID on port 5173 (the failure mode SituationRoom's
#      run_desktop.sh guards against).
#   9. cd's back to the original cwd.
#
# Usage:
#   ./run_desktop.sh                   # debug profile
#   ./run_desktop.sh --release         # release profile (--release passed to tauri dev too)
#   ./run_desktop.sh --tenant default  # which tenant the backend uses (default: test)
#   ./run_desktop.sh --db PATH         # DuckDB file path (default: ./aberp.duckdb)
#   ./run_desktop.sh --no-codesign     # skip the ad-hoc macOS codesign post-build step
#   ./run_desktop.sh -- --extra-arg    # everything after '--' is forwarded to tauri dev
#
# Verified layout (per repo inspection 2026-05-25):
#   apps/aberp-ui/Cargo.toml           — Tauri Rust shell; [[bin]] name = "aberp-ui"
#   apps/aberp-ui/tauri.conf.json      — Tauri config (devUrl, beforeDevCommand, frontendDist)
#   apps/aberp-ui/ui/package.json      — Svelte SPA front-end (vite dev/build)
#   apps/aberp-ui/ui/node_modules/.bin/tauri — local tauri-CLI binary
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
#   wizard on first launch. When the backend handshake reports
#   `state=needs-setup`, the SPA renders a four-field wizard; submit writes
#   the four artifacts to the macOS keychain and the wizard hands off to the
#   normal invoice list. CLI fallback for automation:
#     cargo run --bin aberp -- setup-nav-credentials --tenant <id>
#
# SITUATIONROOM REFERENCE PATTERN:
#   The working-Tauri-2 launcher shape lives at
#     /Users/aben/Documents/Claude/Projects/SituationRoom/scripts/run_desktop.sh
#   Key invariants we copied:
#     - tauri-CLI runs from the dir containing tauri.conf.json's parent
#       (for SR: apps/desktop/, where npx finds tauri in node_modules/.bin/;
#        for ABERP: apps/aberp-ui/, where we call ./ui/node_modules/.bin/tauri
#        because tauri-CLI is installed in the SPA subdir's node_modules).
#     - beforeDevCommand in tauri.conf.json starts Vite; never bypass it.
#     - process-group SIGTERM + lsof :5173 belt-and-suspenders on exit.
#

set -uo pipefail   # NOTE: no -e — we want to handle child exit code, not abort on it

# ---------- self-syntax-check (PR-55) ---------------------------------------
# Catch a parse-error regression at startup so a typo doesn't manifest as a
# half-run launcher dying mid-flight. Cheap (~5 ms) and runs before any real
# work. NOTE: bash -n only catches syntax errors, not unbound-variable issues
# — those still fail at runtime, but the defensive array expansion below
# (${arr[@]+"${arr[@]}"}) prevents the macOS-bash-3.2 empty-array gotcha
# that PR-55 was opened to fix.
if ! bash -n "$0" 2>/dev/null; then
  echo "[fail] $0 failed 'bash -n' syntax check — refusing to run" >&2
  bash -n "$0"   # rerun without redirect so the operator sees the error
  exit 2
fi

# ---------- config (edit if your launch shape differs) -----------------------
readonly REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly DESKTOP_DIR="${REPO_ROOT}/apps/aberp-ui"
readonly TAURI_CLI_REL="./ui/node_modules/.bin/tauri"   # relative to DESKTOP_DIR
readonly TAURI_BIN_NAME="aberp-ui"
readonly ABERP_BIN_NAME="aberp"
readonly DEFAULT_TENANT="test"
readonly DEV_PORT="${ABERP_DEV_PORT:-5173}"
readonly SHUTDOWN_TIMEOUT_SECS=15

# ---------- arg parsing ------------------------------------------------------
mode="debug"
tenant="${ABERP_TENANT:-$DEFAULT_TENANT}"
db_path="${ABERP_DB:-./aberp.duckdb}"
codesign_enabled=1
extra_args=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --release)       mode="release"; shift ;;
    --debug)         mode="debug"; shift ;;
    --tenant)        tenant="$2"; shift 2 ;;
    --db)            db_path="$2"; shift 2 ;;
    --no-codesign)   codesign_enabled=0; shift ;;
    --help|-h)
      sed -n '2,99p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    --)              shift; extra_args=("$@"); break ;;
    *)               extra_args+=("$1"); shift ;;
  esac
done

# ---------- S165 hülye-biztos guard: this is the DEV launcher ----------------
# Symmetric counterpart to the Rust `guard_tenant_matches_build` in
# `apps/aberp/src/serve.rs`: run_desktop.sh builds the DEV (non-production)
# binary, so tenant=prod is structurally wrong here. Refuse loudly and
# steer the operator to run_prod.sh. (The compiled-feature half of the
# guard lives in the Rust binary — a dev build also exits(1) on
# tenant=prod even if this shell check is bypassed.)
if [[ "${tenant}" == "prod" ]]; then
  echo "[fail] run_desktop.sh is the DEV launcher and builds WITHOUT --features production;" >&2
  echo "       tenant=prod is refused. Use run/run_prod.sh for a real production launch," >&2
  echo "       or pick a non-prod tenant (e.g. --tenant test)." >&2
  exit 1
fi

# ---------- preserve original cwd -------------------------------------------
readonly ORIGINAL_CWD="$(pwd)"

# ---------- preflight --------------------------------------------------------
cd "$REPO_ROOT" || { echo "repo not at $REPO_ROOT" >&2; exit 2; }

if [[ ! -f "${DESKTOP_DIR}/Cargo.toml" ]]; then
  echo "[fail] no Cargo.toml at ${DESKTOP_DIR}" >&2
  exit 2
fi
if [[ ! -f "${DESKTOP_DIR}/tauri.conf.json" ]]; then
  echo "[fail] no tauri.conf.json at ${DESKTOP_DIR}" >&2
  exit 2
fi
if [[ ! -x "${DESKTOP_DIR}/${TAURI_CLI_REL}" ]]; then
  echo "[fail] tauri-CLI not found at ${DESKTOP_DIR}/${TAURI_CLI_REL}" >&2
  echo "       run \`cd ${DESKTOP_DIR}/ui && npm install\` first" >&2
  exit 2
fi

# Warn (but don't abort) if a stale DuckDB lock looks present from a prior crash.
readonly OPERATOR_DB_DEFAULT="$HOME/.aberp/serve/${tenant}/aberp.duckdb"
if [[ -f "${OPERATOR_DB_DEFAULT}.wal" ]] || [[ -f "${OPERATOR_DB_DEFAULT}.tmp" ]]; then
  echo "[warn] possible stale DuckDB lock companion files near ${OPERATOR_DB_DEFAULT}"
  echo "       (a .wal or .tmp file exists — usually fine, DuckDB will recover on open;"
  echo "       if launch fails with 'database is locked', stop here and inspect)"
fi

# ---------- pre-build (settle cargo's linker fingerprint) -------------------
# PR-52 — closes the macOS-keychain "Always Allow" prompt-storm that sessions
# 63/66/67 each half-closed.
#
# Empirically-verified mechanism (May 2026, macOS 15, cargo 1.88):
#
#   (a) Apple Silicon LD auto-applies an ad-hoc Mach-O signature on every link
#       (flag `linker-signed,adhoc`). The signature's CDHash is what the macOS
#       keychain ACL ("Always Allow") binds to. CDHash is a function of the
#       binary's code-segment bytes, so any change to the linked bytes = new
#       CDHash = re-prompt.
#
#   (b) tauri-CLI's `dev` mode invokes `cargo run --no-default-features --bin
#       aberp-ui` internally (verified against tauri-cli's rust.rs
#       `dev_options`: it unconditionally injects `--no-default-features` if
#       not already in args). If our pre-build uses a different flag set, the
#       artifacts go to a different cargo fingerprint bucket and tauri-CLI
#       re-links from scratch on launch.
#
#   (c) Even with matching flags, the FIRST `cargo build` after a fresh clean
#       (or after any source change) and the SECOND cargo invocation produce
#       DIFFERENT linked bytes. The first link emits to deps/; the second
#       re-runs the linker once more, then fingerprints settle. The THIRD and
#       subsequent invocations are pure no-ops — same bytes, same CDHash.
#       So the pre-build must run cargo TWICE to settle before tauri-CLI's
#       `cargo run` (the third invocation) sees an up-to-date binary.
#
#   (d) aberp and aberp-ui must be built in SEPARATE cargo invocations. A
#       single `cargo build --bin aberp --bin aberp-ui` selects both packages
#       and unifies features across the dependency graph (resolver v2),
#       producing different aberp-ui bytes than tauri-CLI's narrower
#       `cargo run --bin aberp-ui` build will.
#
# Why we no longer run `codesign --sign -` explicitly:
#   Prior sessions added an explicit `codesign --force --sign -` pass to
#   "stabilize" the ad-hoc identity. Empirically that step DESTABILIZES it:
#   codesign rewrites the binary's `__LINKEDIT` segment, changing the file
#   size. Cargo's fingerprint check on the NEXT invocation sees the file is
#   "dirty" and re-links over our signature. The linker-signed adhoc CDHash
#   that LD applies natively is stable on its own once the double-build
#   settles — verified by hashing CDHash across four consecutive launcher
#   runs (all four matched). The codesign step is therefore dead weight that
#   caused the symptom it was meant to prevent. (CLAUDE.md rule 13: delete
#   before optimize.)
profile_flag=()
if [[ "$mode" == "release" ]]; then
  bin_dir="${REPO_ROOT}/target/release"
  profile_flag=(--release)
else
  bin_dir="${REPO_ROOT}/target/debug"
fi

# aberp: CLI binary, also the keychain-reading subprocess that aberp-ui spawns
# at runtime. Two passes settle the linker fingerprint; subsequent cargo
# invocations (within this launcher run and across runs without source
# changes) are true no-ops, so the linker-signed CDHash stays stable.
#
# NOTE on ${profile_flag[@]+"${profile_flag[@]}"}: macOS ships bash 3.2, where
# expanding an empty array under `set -u` raises "unbound variable" even if
# the array was initialized with `arr=()`. The `${arr[@]+...}` parameter-
# expansion form yields nothing when the array is empty and the quoted
# expansion when it's not — safe under bash 3.2 AND modern bash. (PR-55.)
aberp_build_cmd=(cargo build -p "${ABERP_BIN_NAME}" --bin "${ABERP_BIN_NAME}" ${profile_flag[@]+"${profile_flag[@]}"})
echo "[build] ${aberp_build_cmd[*]}  (pass 1/2)"
"${aberp_build_cmd[@]}" || { echo "[fail] cargo build (aberp pass 1) failed" >&2; exit 4; }
echo "[build] ${aberp_build_cmd[*]}  (pass 2/2 — settle linker fingerprint)"
"${aberp_build_cmd[@]}" || { echo "[fail] cargo build (aberp pass 2) failed" >&2; exit 4; }

# aberp-ui: same pattern, but with --no-default-features matching tauri-CLI's
# `cargo run` invocation so tauri-CLI later sees the build as up-to-date and
# its internal cargo step is a true no-op.
ui_build_cmd=(cargo build -p "${TAURI_BIN_NAME}" --bin "${TAURI_BIN_NAME}" --no-default-features ${profile_flag[@]+"${profile_flag[@]}"})
echo "[build] ${ui_build_cmd[*]}  (pass 1/2)"
"${ui_build_cmd[@]}" || { echo "[fail] cargo build (aberp-ui pass 1) failed" >&2; exit 4; }
echo "[build] ${ui_build_cmd[*]}  (pass 2/2 — settle linker fingerprint)"
"${ui_build_cmd[@]}" || { echo "[fail] cargo build (aberp-ui pass 2) failed" >&2; exit 4; }

# --no-codesign retained for backward-compat with operators who scripted around
# it; the launcher no longer calls codesign at all so the flag is now inert.
if [[ $codesign_enabled -eq 0 ]]; then
  echo "[codesign] flag accepted; explicit codesign was removed in PR-52 (linker adhoc is stable)"
fi

# ---------- free port 5173 if a prior run left it stranded ------------------
# SituationRoom's run_desktop.sh documents this failure mode: a second
# Ctrl-C while Rust is still compiling can detach Vite and leave it
# owning :5173. We pre-flight free the port so this run isn't blocked.
if command -v lsof >/dev/null 2>&1; then
  if lsof -tiTCP:"$DEV_PORT" -sTCP:LISTEN >/dev/null 2>&1; then
    held_by="$(lsof -tiTCP:"$DEV_PORT" -sTCP:LISTEN | tr '\n' ' ')"
    echo "[port] :${DEV_PORT} held by pids ${held_by} (stale Vite from prior run); freeing"
    # shellcheck disable=SC2086
    kill -TERM $held_by 2>/dev/null || true
    sleep 1
    # shellcheck disable=SC2086
    kill -KILL $held_by 2>/dev/null || true
    unset held_by
  fi
fi

# ---------- cleanup hook (group SIGTERM + port-5173 belt-and-suspenders) ----
# Whatever path we exit through (graceful, signal, error), kill the whole
# process group and double-check the dev port is free. Pattern copied from
# SituationRoom/scripts/run_desktop.sh — the working Tauri 2 reference.
cleanup() {
  local rc=$?
  trap - EXIT INT TERM HUP   # avoid recursive cleanup
  echo
  echo "[shutdown] forwarding SIGTERM to process group (rc=${rc})"

  # 1. Polite SIGTERM to the whole group. -$$ targets pgid == our pid.
  if kill -0 -- "-$$" 2>/dev/null; then
    kill -TERM -- "-$$" 2>/dev/null || true
  fi

  # 2. Give children up to SHUTDOWN_TIMEOUT_SECS to exit gracefully so
  #    the aberp-ui drop handlers can release the DuckDB write-lock.
  local waited=0
  while pgrep -g "$$" >/dev/null 2>&1; do
    if [[ $waited -ge $SHUTDOWN_TIMEOUT_SECS ]]; then
      echo "[shutdown] timeout after ${SHUTDOWN_TIMEOUT_SECS}s; escalating to SIGKILL"
      echo "[shutdown] WARNING: DuckDB lock may be left stale — next launch may need recovery"
      kill -KILL -- "-$$" 2>/dev/null || true
      break
    fi
    sleep 1
    waited=$((waited + 1))
  done

  # 3. Belt-and-suspenders: if anything is still on :5173, kill it.
  if command -v lsof >/dev/null 2>&1; then
    local stragglers
    stragglers="$(lsof -tiTCP:"$DEV_PORT" -sTCP:LISTEN 2>/dev/null || true)"
    if [[ -n "$stragglers" ]]; then
      echo "[shutdown] :${DEV_PORT} still held by ${stragglers} — killing"
      # shellcheck disable=SC2086
      kill -TERM $stragglers 2>/dev/null || true
      sleep 1
      # shellcheck disable=SC2086
      kill -KILL $stragglers 2>/dev/null || true
    fi
  fi

  echo "[shutdown] done."
  cd "$ORIGINAL_CWD" 2>/dev/null || true
  echo "[exit] returning to ${ORIGINAL_CWD}"
  echo "[exit] desktop exited with code ${rc}"
  exit "$rc"
}
trap cleanup EXIT INT TERM HUP

# ---------- launch via tauri-CLI --------------------------------------------
# Export the env vars the Tauri shell reads.
export ABERP_TENANT="$tenant"
export ABERP_DB="$db_path"

tauri_args=(dev)
if [[ "$mode" == "release" ]]; then
  tauri_args+=(--release)
fi
if [[ ${#extra_args[@]} -gt 0 ]]; then
  tauri_args+=(-- "${extra_args[@]}")
fi

echo "[launch] mode=${mode}"
echo "[launch] ABERP_TENANT=${tenant} ABERP_DB=${db_path}"
echo "[launch] cd ${DESKTOP_DIR} && ${TAURI_CLI_REL} ${tauri_args[*]}"
echo "[launch] tauri-CLI will run beforeDevCommand (vite at :${DEV_PORT}) + cargo run."
echo "[launch] (Ctrl-C in this terminal sends SIGTERM to the group — graceful shutdown.)"
echo "[launch] First-run NAV-credentials setup is in the SPA itself; no terminal step needed."
echo

cd "$DESKTOP_DIR" || { echo "[fail] cd ${DESKTOP_DIR} failed" >&2; exit 2; }
"$TAURI_CLI_REL" "${tauri_args[@]}" &
tauri_pid=$!

# `wait` returns when the child exits OR when a signal arrives. Either way
# the EXIT trap above does the cleanup (group kill + :5173 sweep + cd back).
wait "$tauri_pid"
