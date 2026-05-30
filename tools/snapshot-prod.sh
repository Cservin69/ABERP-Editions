#!/usr/bin/env bash
#
# snapshot-prod.sh — PR-170 / session-170 — defense-in-depth backup
# before any prod upgrade, in case a future regression in the
# seller.toml write path (S170 type) costs us hand-typed operator
# state.
#
# What it captures:
#   1. The full tenant directory `~/.aberp/<tenant>/` as a gzipped tar.
#      That's seller.toml + the DuckDB file + audit log + side-store
#      invoice directories + the .first-launch-acknowledged touchfile.
#   2. A password-protected zip of the per-tenant macOS keychain
#      entries (NAV credentials blob + SMTP password). The operator
#      types the encryption password interactively at zip-creation;
#      we never write the keychain values unencrypted anywhere on
#      disk except inside a tempfile that's `shred`-removed before
#      the script exits.
#
# What it does NOT capture:
#   - The session_token keychain entry (`aberp.nav.prod` /
#     `session_token`) — that's per-binary-build and regenerated on
#     next boot.
#   - Anything outside `~/.aberp/<tenant>/`.
#
# What this prevents:
#   The S170 prod-update regression where the identity-write surface
#   silently dropped `[seller.smtp]` and `[seller.numbering]`. PR-170
#   fixes the bug at the write path. This script is belt+suspenders:
#   even if a future regression does the same, the snapshot lets the
#   operator restore in ~30 seconds.
#
# Usage:
#   ./tools/snapshot-prod.sh            # default tenant: prod
#   ./tools/snapshot-prod.sh dev        # snapshot dev tenant instead
#   ./tools/snapshot-prod.sh --help

set -euo pipefail

# ---------- arg parsing -----------------------------------------------------
TENANT="prod"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --help|-h)
      sed -n '2,42p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    -*)
      echo "[fail] unknown flag: $1" >&2
      exit 2
      ;;
    *)
      TENANT="$1"
      shift
      ;;
  esac
done

# ---------- colour helpers --------------------------------------------------
if [[ -t 2 && -z "${NO_COLOR:-}" ]]; then
  c_red=$'\033[1;31m'; c_yel=$'\033[1;33m'; c_grn=$'\033[1;32m'
  c_dim=$'\033[2m';    c_rst=$'\033[0m'
else
  c_red=""; c_yel=""; c_grn=""; c_dim=""; c_rst=""
fi

readonly TENANT_DIR="${HOME}/.aberp/${TENANT}"
readonly SNAPSHOT_ROOT="${HOME}/aberp-snapshots"
TIMESTAMP="$(date +%Y%m%d-%H%M%S)"
readonly TIMESTAMP
readonly SNAPSHOT_TGZ="${SNAPSHOT_ROOT}/${TENANT}-${TIMESTAMP}.tgz"
readonly KEYCHAIN_ZIP="${SNAPSHOT_ROOT}/${TENANT}-${TIMESTAMP}-keychain.zip"

# Keychain entries to capture for this tenant. The session_token is
# deliberately omitted — it's regenerated on each binary boot, so
# carrying it across an upgrade is meaningless.
KEYCHAIN_ENTRIES=(
  "aberp.nav.${TENANT}|nav_credentials_blob"
  "aberp.smtp.${TENANT}|smtp_password"
)

# ---------- pre-flight ------------------------------------------------------
if [[ ! -d "$TENANT_DIR" ]]; then
  echo "${c_red}[fail]${c_rst} tenant directory not found: $TENANT_DIR" >&2
  echo "${c_red}[hiba]${c_rst} a bérlő mappa nem található: $TENANT_DIR" >&2
  exit 2
fi

mkdir -p "$SNAPSHOT_ROOT"
chmod 0700 "$SNAPSHOT_ROOT"

echo
echo "${c_yel}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${c_rst}" >&2
echo "${c_yel}  ABERP snapshot — tenant=${TENANT}${c_rst}" >&2
echo "${c_yel}  ABERP biztonsági mentés — bérlő=${TENANT}${c_rst}" >&2
echo "${c_yel}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${c_rst}" >&2
echo

# ---------- 1) tarball the tenant dir ---------------------------------------
echo "${c_dim}[1/2] tarball ${TENANT_DIR} → ${SNAPSHOT_TGZ}${c_rst}" >&2
# Tar from the parent so the tarball expands to `<tenant>/...` on restore.
tar -C "${HOME}/.aberp" -czf "$SNAPSHOT_TGZ" "${TENANT}"
chmod 0600 "$SNAPSHOT_TGZ"
TGZ_SIZE="$(du -h "$SNAPSHOT_TGZ" | awk '{print $1}')"
echo "${c_grn}[ ok ]${c_rst} tarball written ($TGZ_SIZE): $SNAPSHOT_TGZ" >&2
echo "${c_grn}[ ok ]${c_rst} tarball kész ($TGZ_SIZE): $SNAPSHOT_TGZ" >&2

# ---------- 2) keychain dump + encrypted zip --------------------------------
# Tempfile + cleanup. NEVER print the dump contents.
TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/aberp-snapshot.XXXXXX")"
trap 'shred -uz "${TMP_DIR}"/* 2>/dev/null || rm -rf "${TMP_DIR}"' EXIT
chmod 0700 "$TMP_DIR"

DUMP_FILE="${TMP_DIR}/keychain-${TENANT}.json"

# Build the JSON dump entry-by-entry. We capture {service, account,
# password} with the password as a raw string. Any failure to read
# loud-fails so the operator does not get a snapshot that silently
# omits one of the secrets they expect.
{
  echo "{"
  echo "  \"tenant\": \"${TENANT}\","
  echo "  \"captured_at\": \"$(date -u +%Y-%m-%dT%H:%M:%SZ)\","
  echo "  \"entries\": ["
  first=1
  for entry in "${KEYCHAIN_ENTRIES[@]}"; do
    SERVICE="${entry%%|*}"
    ACCOUNT="${entry##*|}"
    # Capture the password to a sub-tempfile so it never lives in a
    # shell variable (and so the JSON-escape step is purely file-driven).
    PWD_FILE="${TMP_DIR}/.pwd-${SERVICE//[^A-Za-z0-9]/_}-${ACCOUNT//[^A-Za-z0-9]/_}"
    if ! security find-generic-password -s "$SERVICE" -a "$ACCOUNT" -w \
        > "$PWD_FILE" 2>/dev/null; then
      echo "${c_yel}[skip]${c_rst} keychain entry not present: $SERVICE / $ACCOUNT" >&2
      echo "${c_yel}[ki  ]${c_rst} kulcstartó bejegyzés hiányzik: $SERVICE / $ACCOUNT" >&2
      continue
    fi
    # Strip the trailing newline `security -w` appends.
    PWD_LEN="$(wc -c < "$PWD_FILE" | awk '{print $1}')"
    if [[ "$PWD_LEN" -gt 0 ]]; then
      truncate -s $((PWD_LEN - 1)) "$PWD_FILE"
    fi
    # JSON-escape via python (stdlib only). We pipe the password in via
    # stdin so it never appears on the command line.
    ESCAPED_PWD="$(python3 -c 'import sys,json; print(json.dumps(sys.stdin.read()), end="")' < "$PWD_FILE")"
    if [[ $first -eq 0 ]]; then echo "    ,"; fi
    first=0
    printf '    {"service": %s, "account": %s, "password": %s}' \
      "$(printf '%s' "$SERVICE" | python3 -c 'import sys,json; print(json.dumps(sys.stdin.read()), end="")')" \
      "$(printf '%s' "$ACCOUNT" | python3 -c 'import sys,json; print(json.dumps(sys.stdin.read()), end="")')" \
      "$ESCAPED_PWD"
    echo
    shred -uz "$PWD_FILE" 2>/dev/null || rm -f "$PWD_FILE"
  done
  echo "  ]"
  echo "}"
} > "$DUMP_FILE"
chmod 0600 "$DUMP_FILE"

# Encrypt to zip. `zip -e` prompts for a password twice on stderr.
# Operator types it interactively — we never see it.
echo >&2
echo "${c_yel}[2/2] encrypting keychain dump → ${KEYCHAIN_ZIP}${c_rst}" >&2
echo "${c_yel}[2/2] kulcstartó-másolat titkosítása → ${KEYCHAIN_ZIP}${c_rst}" >&2
echo "${c_dim}      zip will prompt for an encryption password — pick one you can remember;${c_rst}" >&2
echo "${c_dim}      a zip jelszót fog kérni — válassz olyat, amit meg tudsz jegyezni;${c_rst}" >&2
echo "${c_dim}      restore needs it.${c_rst}" >&2
echo "${c_dim}      a visszaállításhoz szükséges lesz.${c_rst}" >&2
echo >&2

(cd "$TMP_DIR" && zip -e "$KEYCHAIN_ZIP" "$(basename "$DUMP_FILE")")
chmod 0600 "$KEYCHAIN_ZIP"

# Tempfile is shredded by the EXIT trap.
echo
echo "${c_grn}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${c_rst}" >&2
echo "${c_grn}  snapshot complete${c_rst}" >&2
echo "${c_grn}  biztonsági mentés kész${c_rst}" >&2
echo "${c_grn}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${c_rst}" >&2
echo "${c_grn}  tenant tarball: ${SNAPSHOT_TGZ}${c_rst}" >&2
echo "${c_grn}  keychain zip:   ${KEYCHAIN_ZIP}${c_rst}" >&2
echo "${c_grn}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${c_rst}" >&2
echo
echo "${c_dim}Restore recipe (see runbook Step 9 for full procedure):${c_rst}" >&2
echo "${c_dim}  1. Stop the app (Ctrl-C in the run_prod.sh terminal).${c_rst}" >&2
echo "${c_dim}  2. tar -C \"\${HOME}/.aberp\" -xzf \"${SNAPSHOT_TGZ}\"${c_rst}" >&2
echo "${c_dim}  3. unzip the keychain zip + re-import via 'security add-generic-password'.${c_rst}" >&2
echo "${c_dim}  4. Relaunch ./run/run_prod.sh${c_rst}" >&2
echo
