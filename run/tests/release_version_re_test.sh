#!/usr/bin/env bash
#
# release_version_re_test.sh — S2 / ADR-0100 §4, ADR-0056 (amended 2026-07-21)
#
# Regression test for `run/release.sh`'s version validator and its
# next-version suggester.
#
# Bug (pre-S2): `release.sh:72` pinned
# `VERSION_RE='^PROD_v[0-9]+\.[0-9]+(\.[0-9]+)?$'`, which accepts neither
# `PROD_Defense_*` nor `PROD_Portable_*`. This repository (ABERP-Editions)
# ships ONLY those two product lines, so it had no script that could cut a
# release of either — while `run/upgrade_defense.sh:83` and
# `run/upgrade_portable.sh:70` both resolve a release from `origin/<version>`
# as a branch that only `release.sh` is supposed to create. The thirteen
# `PROD_Defense_v*` branches on origin were consequently pushed by hand, with
# none of release.sh's preflights running — which is how `PROD_Defense_v0.2.12`
# ended up one commit AHEAD of `main`.
#
# This test pins the contract three ways, all static and network-free:
#
#   1. ACCEPT: every shape the two installers can consume must validate,
#      including the bare `PROD_v*` form the frozen HU prod line still uses.
#   2. REJECT: the widening must stay a CLOSED alternation. A typo'd line
#      (`PROD_Portible_`), an arbitrary line (`PROD_Saas_`), the archived
#      `archive/aberp-git/` prefix (ADR-0100 Decision A), 4-segment versions
#      and `-rc` suffixes must all still die at arg validation.
#   3. SUGGESTER: the "branch already exists" hint must REPLAY the product-line
#      prefix. Suggesting `PROD_v0.2.13` to an operator who typed
#      `PROD_Defense_v0.2.12` would name a branch no installer accepts.
#
# Cross-check (4): release.sh's ACCEPT set must be exactly the union of the two
# installers' own VERSION_REs plus the bare prod form — the installers are the
# contract, so drift between them is the failure this catches.
#
# Exit 0 = all pass; non-zero = failure (CI / cut-gate citizen).

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd -P)"
RELEASE_SH="${REPO_ROOT}/run/release.sh"
UPGRADE_DEFENSE="${REPO_ROOT}/run/upgrade_defense.sh"
UPGRADE_PORTABLE="${REPO_ROOT}/run/upgrade_portable.sh"

fails=0
pass() { echo "[ ok ] $1"; }
fail() { echo "[FAIL] $1" >&2; fails=$((fails + 1)); }

for f in "$RELEASE_SH" "$UPGRADE_DEFENSE" "$UPGRADE_PORTABLE"; do
  [[ -r "$f" ]] || { fail "missing: $f"; echo "fails=$fails"; exit 1; }
done

# Extract the live regexes from the scripts rather than restating them here,
# so the test cannot silently drift out of agreement with the code.
extract_re() { sed -n "s/^readonly VERSION_RE='\(.*\)'$/\1/p" "$1" | head -1; }

RELEASE_RE="$(extract_re "$RELEASE_SH")"
DEFENSE_RE="$(extract_re "$UPGRADE_DEFENSE")"
PORTABLE_RE="$(extract_re "$UPGRADE_PORTABLE")"

[[ -n "$RELEASE_RE"  ]] && pass "release.sh VERSION_RE extracted: $RELEASE_RE" \
  || fail "could not extract VERSION_RE from release.sh"
[[ -n "$DEFENSE_RE"  ]] || fail "could not extract VERSION_RE from upgrade_defense.sh"
[[ -n "$PORTABLE_RE" ]] || fail "could not extract VERSION_RE from upgrade_portable.sh"

# ---------- 1. ACCEPT ------------------------------------------------------
ACCEPT=(
  PROD_v1.0 PROD_v1.4.1 PROD_v2.32.1
  PROD_Defense_v0.2.13 PROD_Defense_v1.0
  PROD_Portable_v1.0.0 PROD_Portable_v1.0
)
for v in "${ACCEPT[@]}"; do
  if [[ "$v" =~ $RELEASE_RE ]]; then pass "accept: $v"; else fail "accept: $v was REJECTED"; fi
done

# ---------- 2. REJECT ------------------------------------------------------
REJECT=(
  PROD_Portible_v1.0.0                      # typo'd line must not slip through
  PROD_Saas_v1.0.0                          # closed alternation, not a wildcard
  PROD_defense_v1.0.0                       # case is load-bearing
  archive/aberp-git/PROD_Portable_v0.1.2    # ADR-0100 Decision A archive name
  PROD_Portable_v1.0.0.1                    # 4 segments
  PROD_Portable_v1.0.0-rc1                  # pre-release suffix
  PROD_v1                                   # 1 segment
  prod_v1.0                                 # lowercase
  PROD_Defense_1.0.0                        # missing the 'v'
  ""                                        # empty
)
for v in "${REJECT[@]}"; do
  if [[ "$v" =~ $RELEASE_RE ]]; then fail "reject: '$v' was ACCEPTED"; else pass "reject: '${v:-<empty>}'"; fi
done

# ---------- 3. SUGGESTER replays the product-line prefix -------------------
# Mirrors the extraction block in release.sh's "branch already exists" path.
suggest() {
  local version="$1" line major minor patch
  line="$(echo  "$version" | sed -E 's/^PROD_(Defense_|Portable_)?v[0-9]+\.[0-9]+(\.[0-9]+)?$/\1/')"
  major="$(echo "$version" | sed -E 's/^PROD_(Defense_|Portable_)?v([0-9]+)\.([0-9]+)(\.([0-9]+))?$/\2/')"
  minor="$(echo "$version" | sed -E 's/^PROD_(Defense_|Portable_)?v([0-9]+)\.([0-9]+)(\.([0-9]+))?$/\3/')"
  patch="$(echo "$version" | sed -E 's/^PROD_(Defense_|Portable_)?v([0-9]+)\.([0-9]+)(\.([0-9]+))?$/\5/')"
  if [[ -n "$patch" ]]; then echo "PROD_${line}v${major}.${minor}.$((patch + 1))"
  else echo "PROD_${line}v${major}.$((minor + 1))"; fi
}
check_suggest() {
  local got; got="$(suggest "$1")"
  if [[ "$got" == "$2" ]]; then pass "suggest $1 -> $got"
  else fail "suggest $1 -> got '$got', want '$2'"; fi
}
check_suggest PROD_Defense_v0.2.12  PROD_Defense_v0.2.13
check_suggest PROD_Defense_v0.2     PROD_Defense_v0.3
check_suggest PROD_Portable_v1.0.0  PROD_Portable_v1.0.1
check_suggest PROD_Portable_v1.0    PROD_Portable_v1.1
check_suggest PROD_v1.4.1           PROD_v1.4.2
check_suggest PROD_v1.4             PROD_v1.5

# Every suggestion must itself be a name release.sh would accept.
for v in PROD_Defense_v0.2.12 PROD_Portable_v1.0.0 PROD_v1.4.1 PROD_v1.4; do
  s="$(suggest "$v")"
  if [[ "$s" =~ $RELEASE_RE ]]; then pass "suggestion round-trips: $s"
  else fail "suggestion '$s' (from '$v') fails release.sh's own VERSION_RE"; fi
done

# ---------- 4. cross-check against the two installers ----------------------
# Anything release.sh cuts for a line, that line's installer must consume.
for v in PROD_Defense_v0.2.13 PROD_Defense_v1.0; do
  if [[ "$v" =~ $DEFENSE_RE ]]; then pass "upgrade_defense.sh consumes: $v"
  else fail "upgrade_defense.sh REJECTS a name release.sh cuts: $v"; fi
done
for v in PROD_Portable_v1.0.0 PROD_Portable_v1.0; do
  if [[ "$v" =~ $PORTABLE_RE ]]; then pass "upgrade_portable.sh consumes: $v"
  else fail "upgrade_portable.sh REJECTS a name release.sh cuts: $v"; fi
done
# ...and neither installer may consume the OTHER line's names.
if [[ "PROD_Portable_v1.0.0" =~ $DEFENSE_RE ]]; then
  fail "upgrade_defense.sh accepts a Portable name — lines not disjoint"
else pass "upgrade_defense.sh refuses Portable names"; fi
if [[ "PROD_Defense_v0.2.13" =~ $PORTABLE_RE ]]; then
  fail "upgrade_portable.sh accepts a Defense name — lines not disjoint"
else pass "upgrade_portable.sh refuses Defense names"; fi

# ---------- 5. release.sh still refuses to publish from the dev workspace --
# The dev-sentinel is untouched by S2; assert it is still present so a later
# edit cannot quietly drop the one guard that keeps a half-finished dev tree
# from becoming a release.
if grep -q 'DEV_SENTINEL_PATH_SUBSTR="/Documents/Claude/Projects/"' "$RELEASE_SH"; then
  pass "dev-sentinel intact in release.sh"
else
  fail "dev-sentinel missing or altered in release.sh"
fi

echo
if [[ $fails -eq 0 ]]; then echo "ALL PASS"; exit 0; else echo "$fails FAILURE(S)"; exit 1; fi
