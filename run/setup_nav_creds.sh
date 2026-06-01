#!/usr/bin/env bash
#
# setup_nav_creds.sh
#
# Safely populate the macOS keychain with NAV technical-user credentials
# for `aberp` without ever writing them to disk.
#
# Security properties:
#   - Each value is read via `read -s` (no echo on the terminal — no shoulder
#     surfing, no terminal scrollback exposure).
#   - Reads use /dev/tty explicitly so the prompts go to the terminal even
#     while stdout is connected to the cargo subprocess's pipe.
#   - Values are passed to the `aberp setup-nav-credentials` subcommand via
#     stdin (a kernel-managed pipe buffer, ~64 KB, never persists to disk).
#   - Variables are `unset` after the pipe completes (bash doesn't zero
#     memory, but the binding goes away — a `ps eww` won't show them).
#   - The cargo subprocess's argv contains only the subcommand name +
#     `--tenant <name>`. The actual credentials never appear on a command
#     line, in $HISTFILE, in $ENV, or in any file.
#   - Once written, credentials live in the macOS keychain. The keychain
#     enforces per-process ACLs; only `aberp` (registered as an authorized
#     accessor at write time) and explicit user approval (Keychain Access
#     prompt) can read them later.
#
# What this script does NOT protect against (be honest about the limits):
#   - A compromised macOS user account: anything that runs as you can ask
#     the keychain for access. The keychain prompt mitigates this for GUI
#     access, but background processes you've installed are inherently
#     trusted.
#   - Memory inspection of the bash or cargo process by root during the
#     few-millisecond window the values are in memory.
#   - A trojaned `aberp` binary. If the cargo workspace is compromised,
#     the binary itself could exfiltrate the values it receives.
#   - Shoulder surfing while you're physically typing. `read -s` blocks
#     echo, but anyone watching your fingers still sees the characters
#     you press. If that's in scope, use a password manager that supports
#     copy-to-clipboard with auto-clear, and paste each value blind.
#
# Usage:
#   ./setup_nav_creds.sh                # writes to tenant "default"
#   ./setup_nav_creds.sh --tenant test  # writes to tenant "test"
#   ./setup_nav_creds.sh --tenant prod  # writes to tenant "prod" (use later)
#
# After this script: launch `./run/run_desktop.sh` (with the matching tenant).

set -uo pipefail

readonly REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tenant="default"

# ---------- arg parsing ------------------------------------------------------
while [[ $# -gt 0 ]]; do
  case "$1" in
    --tenant) tenant="$2"; shift 2 ;;
    --help|-h)
      sed -n '2,45p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *) echo "unknown flag: $1" >&2; exit 2 ;;
  esac
done

cd "$REPO_ROOT" || { echo "repo not at $REPO_ROOT" >&2; exit 2; }

# ---------- guardrails -------------------------------------------------------
if [[ "$tenant" == "prod" || "$tenant" == "production" ]]; then
  echo
  echo "================================================================"
  echo " You are about to set up PRODUCTION NAV credentials."
  echo " Production submissions are real tax filings — irrevocable."
  echo " ABERP currently routes only to NAV test endpoint regardless of"
  echo " tenant (per ADR-0037 boilerplate). Production cutover is gated"
  echo " by a future ADR (~ADR-0038). Continuing this script will store"
  echo " prod creds in the keychain but they will NOT be used yet."
  echo "================================================================"
  read -r -p "Type 'I understand' to continue: " ack < /dev/tty
  if [[ "$ack" != "I understand" ]]; then
    echo "aborted"
    exit 1
  fi
fi

# ---------- read each value without echo -------------------------------------
# Open the tty as fd 3 (read) and fd 4 (write) so prompts and reads work
# even when this script's stdout is later piped into the cargo subprocess.
exec 3</dev/tty 4>/dev/tty

echo "[setup] Tenant: $tenant" >&4
echo "[setup] Each prompt below reads silently — your input will NOT echo." >&4
echo "[setup] Just type / paste each value and press Enter." >&4
echo >&4

prompt_secret() {
  local var_name="$1"
  local label="$2"
  local value
  read -r -s -p "  $label: " value <&3
  echo >&4
  if [[ -z "$value" ]]; then
    echo "[fail] empty value for $label — aborting (no keychain write attempted)" >&4
    exec 3<&- 4>&-
    exit 2
  fi
  printf -v "$var_name" '%s' "$value"
}

prompt_secret LOGIN      "Technical-user LOGIN     "
prompt_secret PASSWORD   "Technical-user PASSWORD  "
prompt_secret SIGN_KEY   "XML SIGN key             "
prompt_secret CHANGE_KEY "XML CHANGE (exchange) key"

echo >&4
echo "[setup] All four values captured in memory. Writing to keychain..." >&4
echo >&4

exec 3<&- 4>&-

# ---------- pipe into the CLI ------------------------------------------------
# The CLI reads four lines from stdin in this order. We send them with no
# trailing newlines beyond what's needed, and no file ever exists on disk.
printf '%s\n%s\n%s\n%s\n' "$LOGIN" "$PASSWORD" "$SIGN_KEY" "$CHANGE_KEY" \
  | cargo run --bin aberp -- setup-nav-credentials --tenant "$tenant"
cli_exit=$?

# ---------- wipe ------------------------------------------------------------
# bash doesn't zero memory on unset, but the binding is removed.
unset LOGIN PASSWORD SIGN_KEY CHANGE_KEY

if [[ $cli_exit -eq 0 ]]; then
  echo
  echo "[done] keychain populated for tenant=$tenant."
  echo "[done] Next: ./run/run_desktop.sh --tenant $tenant"
else
  echo
  echo "[fail] setup-nav-credentials exited with code $cli_exit" >&2
  echo "       check the cargo output above for the specific error" >&2
  exit "$cli_exit"
fi
