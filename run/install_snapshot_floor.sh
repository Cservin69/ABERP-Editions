#!/usr/bin/env bash
#
# install_snapshot_floor.sh — ADR-0116 D1.3, the OUT-OF-PROCESS snapshot floor.
#
# ## Why this exists
#
# The 4-hourly snapshot daemon lives inside `aberp serve`. When serve is not
# running, no snapshot is taken: there is no catch-up, no missed-tick
# detection, and nothing outside the process ever calls `take_snapshot`.
# Derived from the audit ledger (not from surviving directories, which were
# manually pruned on 2026-08-26):
#
#   * 79 snapshots in 71 days where a 4-hour cadence predicts 426 — the system
#     ran at **18.5 % of its configured cadence over its entire life**;
#   * largest gap **8 d 20 h 36 m**;
#   * the 2026-08-22 incident falls INSIDE a 6 d 6 h gap, so the snapshot
#     system was offering a rollback point five days stale at that moment.
#
# And those gaps are not "nothing happened" time: D-22 established that 15 CLI
# money-submission sites write to the DB with serve DOWN. The database changes
# during exactly the windows that produce no rollback points.
#
# **This is the only change in ADR-0116 that creates rollback points INSIDE a
# downtime gap.** The daemon's catch-up (D1.2) does not: it takes a snapshot at
# `restart + 60 s`, which in the 08-17 → 08-23 gap lands a rollback point AFTER
# the 08-22 incident, and a post-incident rollback point cannot roll back the
# incident.
#
# ## What it installs
#
# A macOS `launchd` user agent that runs `aberp snapshot now --if-stale-secs`
# once daily at 03:00 local. `--if-stale-secs` makes the floor and the
# in-process daemon idempotent against each other: whichever runs first
# satisfies the window and the other no-ops, so scheduling this cannot
# multiply the store's growth rate.
#
# Cadence chosen conservatively (ADR-0116 open decision #1): given a measured
# max gap of 8 d 20 h, even weekly would have helped; hourly would make
# serve-uptime nearly irrelevant but multiplies store growth (~1.8 MB/snapshot,
# so hourly ≈ 43 MB/day before retention). **Start daily, measure the resulting
# RPO for one month, then decide** — it is reversible in a plist.
#
# ## Two things the floor decides explicitly, because silence in either
# ## direction is wrong
#
#  1. `ABERP_SNAPSHOT_DISABLE` turns the in-process daemon off. The floor
#     HONOURS it — "disabled" must mean disabled, and a backup daemon that
#     ignores its own kill switch is worse than one that can be switched off.
#     `aberp snapshot now` logs LOUD at every scheduled invocation when it
#     no-ops for this reason, so a disable set for an unrelated reason cannot
#     silently remove the floor.
#
#  2. **The keychain / binary-identity constraint on Defense.** Prompt-freeness
#     depends on BINARY IDENTITY: a rebuild invalidates the ad-hoc-signed ACL
#     and blocks an unattended run on a prompt. An unattended scheduled
#     `aberp snapshot now` may therefore hit that prompt and the floor silently
#     never runs — which is indistinguishable from a floor that never existed.
#     THIS IS A VERIFICATION TASK, NOT A DESIGN QUESTION, and it gates whether
#     D1.3 delivers anything on Defense (ADR-0116 open decision #3).
#     `--verify-run` below is how you check it: it runs the exact command
#     launchd will run, unattended, and reports whether it produced a snapshot.
#
# ## Usage
#
#   run/install_snapshot_floor.sh --tenant defense [--dry-run]
#   run/install_snapshot_floor.sh --tenant defense --verify-run
#   run/install_snapshot_floor.sh --tenant defense --uninstall
#
set -euo pipefail

TENANT="default"
HOUR=3
MINUTE=0
DRY_RUN=0
UNINSTALL=0
VERIFY_RUN=0
BIN=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --tenant)      TENANT="$2"; shift 2;;
    --hour)        HOUR="$2"; shift 2;;
    --minute)      MINUTE="$2"; shift 2;;
    --bin)         BIN="$2"; shift 2;;
    --dry-run)     DRY_RUN=1; shift;;
    --uninstall)   UNINSTALL=1; shift;;
    --verify-run)  VERIFY_RUN=1; shift;;
    -h|--help)     sed -n '2,70p' "$0"; exit 0;;
    *) echo "unknown argument: $1" >&2; exit 2;;
  esac
done

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "This installer targets macOS launchd. On another platform, schedule the" >&2
  echo "equivalent command yourself (systemd timer / cron):" >&2
  echo "  aberp snapshot now --tenant $TENANT --db <live-db> --if-stale-secs 86400" >&2
  exit 2
fi

LABEL="ch.aben.aberp.snapshot-floor.${TENANT}"
PLIST="$HOME/Library/LaunchAgents/${LABEL}.plist"
LOGDIR="$HOME/Library/Logs/aberp"

if [[ "$UNINSTALL" == "1" ]]; then
  launchctl bootout "gui/$(id -u)/${LABEL}" 2>/dev/null || true
  rm -f "$PLIST"
  echo "Snapshot floor uninstalled: $LABEL"
  echo "NOTE: the RPO now depends entirely on \`aberp serve\` uptime again."
  exit 0
fi

# Resolve the binary. An absolute path is required in the plist: launchd runs
# with a minimal PATH and a relative name would simply never start — the
# silent-no-op failure mode this whole script exists to avoid.
if [[ -z "$BIN" ]]; then
  BIN="$(command -v aberp || true)"
fi
if [[ -z "$BIN" || ! -x "$BIN" ]]; then
  echo "✗ Could not resolve an executable \`aberp\`. Pass --bin /absolute/path/to/aberp." >&2
  echo "  launchd runs with a minimal PATH; a floor that cannot start is a floor" >&2
  echo "  that does not exist, and it would fail SILENTLY." >&2
  exit 1
fi
BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"

# The edition-scoped live DB. Never `~/.aberp/` — that is the FROZEN prod line
# (ADR-0093), and the binary refuses it anyway.
DB="$HOME/.aberp-defense/${TENANT}/aberp.duckdb"
if [[ ! -f "$DB" ]]; then
  echo "! No live DB at $DB — the floor will be installed, but verify the path." >&2
fi

# 86400 = the floor's own cadence. Running daily with a one-day staleness
# window means: take a snapshot unless something already did today.
CMD_ARGS=(snapshot now --tenant "$TENANT" --db "$DB" --if-stale-secs 86400)

if [[ "$VERIFY_RUN" == "1" ]]; then
  echo "Running the EXACT command launchd will run, unattended:"
  echo "  $BIN ${CMD_ARGS[*]}"
  echo
  echo "ADR-0116 open decision #3 — if this hangs or prompts, the Defense"
  echo "ad-hoc-signing ACL is blocking unattended keychain access and the floor"
  echo "will silently never run. That is the one thing that would make D1.3"
  echo "deliver nothing on Defense."
  echo
  before="$("$BIN" snapshot list --tenant "$TENANT" --json 2>/dev/null | grep -c '"seq"' || true)"
  "$BIN" "${CMD_ARGS[@]}"
  after="$("$BIN" snapshot list --tenant "$TENANT" --json 2>/dev/null | grep -c '"seq"' || true)"
  echo
  echo "snapshots before=$before after=$after"
  if [[ "$after" -gt "$before" ]]; then
    echo "✓ the floor produced a snapshot unattended."
  else
    echo "! no new snapshot — this is EXPECTED if one was already taken inside the"
    echo "  staleness window, and a PROBLEM otherwise. Check the output above."
  fi
  exit 0
fi

mkdir -p "$(dirname "$PLIST")" "$LOGDIR"
PLIST_BODY=$(cat <<PL
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>${LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>${BIN}</string>
$(printf '    <string>%s</string>\n' "${CMD_ARGS[@]}")
  </array>
  <key>StartCalendarInterval</key>
  <dict>
    <key>Hour</key><integer>${HOUR}</integer>
    <key>Minute</key><integer>${MINUTE}</integer>
  </dict>
  <!-- Run the missed occurrence when the machine was asleep at 03:00. A
       laptop that is closed overnight is the common case, and a floor that
       skips it is a floor that runs on the days it was least needed. -->
  <key>RunAtLoad</key><false/>
  <key>StandardOutPath</key><string>${LOGDIR}/snapshot-floor.log</string>
  <key>StandardErrorPath</key><string>${LOGDIR}/snapshot-floor.log</string>
</dict>
</plist>
PL
)

if [[ "$DRY_RUN" == "1" ]]; then
  echo "Would write $PLIST:"; echo; echo "$PLIST_BODY"; exit 0
fi

printf '%s\n' "$PLIST_BODY" > "$PLIST"
launchctl bootout "gui/$(id -u)/${LABEL}" 2>/dev/null || true
launchctl bootstrap "gui/$(id -u)" "$PLIST"
echo "✓ Snapshot floor installed: $LABEL (daily at $(printf '%02d:%02d' "$HOUR" "$MINUTE") local)"
echo "  command: $BIN ${CMD_ARGS[*]}"
echo "  log:     $LOGDIR/snapshot-floor.log"
echo
echo "NEXT: run with --verify-run to confirm it actually works unattended on"
echo "Defense (ADR-0116 open decision #3 — the keychain/binary-identity ACL)."
