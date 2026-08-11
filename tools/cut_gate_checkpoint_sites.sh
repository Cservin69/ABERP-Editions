#!/usr/bin/env bash
#
# cut_gate_checkpoint_sites.sh — ADR-0111 durable-checkpoint call-site cut-gate.
#
# The defect: `aberp_snapshot::durable_checkpoint` commits via `atomic_install`
# — `rename(staging → db_path)` plus an unlink of `<db>.wal`. Run on a PATH
# while the process-wide `aberp_db::Handle` is open, that strands the shared
# connection on the old, now-unlinked inode. Later commits land in that orphan
# (freed at exit) yet are still visible to the post-commit `sync_mirror` on the
# same connection, which durably mirrors them: MIRROR AHEAD OF DB, the direction
# boot refuses and Defense's auto-heal replays. Two production callers did
# exactly that (the snapshot daemon and the ADR-0095 §3 post-write debouncer).
#
# The fix is `Handle::checkpoint_now`, which takes the writer mutex and quiesces
# + reopens the shared connection around the swap. This gate keeps it that way.
#
# CHECK C-0 — matcher liveness (a "zero hits ⇒ green" gate is worthless if the
#             matcher is dead; pin that it sees a real call and ignores a doc
#             mention, a definition, and `run_durable_checkpoint_locked`).
# CHECK C-A — NO caller of the path-based `durable_checkpoint` /
#             `live_durable_checkpoint` outside the two allow-listed owners.
#             THE regression this gate exists for.
# CHECK C-B — the mechanism is intact: `checkpoint_now` takes the lock and
#             delegates, and `run_durable_checkpoint_locked` still QUIESCES
#             (`inner.conn = None`) and REOPENS (`open_runtime_connection`).
#             Without both, routing through the handle buys nothing.
# CHECK C-C — the `checkpoint_now()` call-site census is CLOSED both ways.
# CHECK C-D — the inode fence (the belt for an out-of-process swapper) is
#             present AND still skips the mirror sync on a mismatch.
#
# ENFORCE_CHECKPOINT_SITES=0 disables enforcement (used by a probe harness to
# prove the gate fails closed). Exit 0 = gate green.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
fail=0
note() { printf '  %s\n' "$*"; }
echo "ADR-0111 durable-checkpoint call-site cut-gate — root: $ROOT"

CENSUS="tools/adr0111_checkpoint_sites.txt"
[[ -f "$CENSUS" ]] || {
  note "✗ FAIL: census missing: $CENSUS"
  echo; echo "CUT-GATE: ✗ FAILED"; exit 1
}

enforce="${ENFORCE_CHECKPOINT_SITES:-1}"
flag() {
  note "$1"
  if [[ "$enforce" == "1" ]]; then fail=1; else note "  (enforcement disabled — not failing)"; fi
}

# ── matchers ─────────────────────────────────────────────────────────────────
#
# A CALL of the path-based primitive. Requirements, each learned from a way the
# naive form gets this wrong:
#   * `\(` immediately after the name, so the doc-comment spellings used all
#     over this tree (`[`durable_checkpoint`]`, "the durable_checkpoint
#     primitive") never count, and so `run_durable_checkpoint_locked(` — the
#     SANCTIONED wrapper, whose name contains the banned one — never counts;
#   * a non-identifier char before it, so `live_durable_checkpoint(` is not
#     matched twice and no longer-named helper matches by suffix;
#   * `fn ` excluded, so the definitions in aberp-snapshot are not calls;
#   * `//` comments stripped first, because commenting a call out is not a
#     call — and re-enabling one is the regression we want to see arrive.
strip_comments() { sed 's://.*::' "$1"; }
ckpt_calls() {
  strip_comments "$1" \
    | grep -nE '(^|[^A-Za-z0-9_])(live_)?durable_checkpoint[[:space:]]*\(' \
    | grep -vE '\bfn[[:space:]]+(live_)?durable_checkpoint'
}
# A CALL of the sanctioned handle method.
now_calls() { strip_comments "$1" | grep -nE '\.checkpoint_now[[:space:]]*\(\)'; }

# ── CHECK C-0 — matcher liveness (ALWAYS ENFORCED) ───────────────────────────
echo "[CHECK C-0] matcher liveness — real calls seen, mentions/definitions/wrappers ignored"
probe="$(mktemp)"; trap 'rm -f "$probe"' EXIT

printf 'aberp_snapshot::durable_checkpoint(db_path, tenant)?;\n' > "$probe"
[[ "$(ckpt_calls "$probe" | wc -l | tr -d ' ')" == "1" ]] \
  || { note "✗ FAIL: matcher missed a real path-based durable_checkpoint call"; fail=1; }
printf 'let r = aberp_snapshot::live_durable_checkpoint(&db, &tenant);\n' > "$probe"
[[ "$(ckpt_calls "$probe" | wc -l | tr -d ' ')" == "1" ]] \
  || { note "✗ FAIL: matcher missed a real live_durable_checkpoint call"; fail=1; }
# The three shapes that must NOT count.
printf '/// see [`durable_checkpoint`] and the live_durable_checkpoint wrapper\n' > "$probe"
[[ "$(ckpt_calls "$probe" | wc -l | tr -d ' ')" == "0" ]] \
  || { note "✗ FAIL: matcher counted a doc mention"; fail=1; }
printf 'pub fn durable_checkpoint(db_path: &Path, tenant: &str) -> Result<X> {\n' > "$probe"
[[ "$(ckpt_calls "$probe" | wc -l | tr -d ' ')" == "0" ]] \
  || { note "✗ FAIL: matcher counted the primitive's own definition as a call"; fail=1; }
printf '        self.run_durable_checkpoint_locked(&mut inner);\n' > "$probe"
[[ "$(ckpt_calls "$probe" | wc -l | tr -d ' ')" == "0" ]] \
  || { note "✗ FAIL: matcher counted the SANCTIONED run_durable_checkpoint_locked wrapper"; fail=1; }
printf '    // aberp_snapshot::durable_checkpoint(&db, &tenant);\n' > "$probe"
[[ "$(ckpt_calls "$probe" | wc -l | tr -d ' ')" == "0" ]] \
  || { note "✗ FAIL: matcher counted a commented-out call"; fail=1; }
# checkpoint_now matcher, both directions.
printf '            db.checkpoint_now();\n' > "$probe"
[[ "$(now_calls "$probe" | wc -l | tr -d ' ')" == "1" ]] \
  || { note "✗ FAIL: matcher missed a real .checkpoint_now() call"; fail=1; }
printf '/// [`aberp_db::Handle::checkpoint_now`] takes the writer mutex\n' > "$probe"
[[ "$(now_calls "$probe" | wc -l | tr -d ' ')" == "0" ]] \
  || { note "✗ FAIL: matcher counted a checkpoint_now doc mention"; fail=1; }

if [[ "$fail" -ne 0 ]]; then echo; echo "CUT-GATE: ✗ FAILED (matcher liveness)"; exit 1; fi
note "✓ matchers live"
echo

# ── CHECK C-A — the path-based primitive has no callers outside its owners ────
# Scoped to production sources (`*/src/**`). Tests are deliberately OUT of
# scope: `aberp-snapshot`'s own suite calls the primitive by design, and
# `aberp-db`'s ADR-0111 suite calls it to SIMULATE the out-of-process swapper
# the inode fence exists to catch. Banning it there would delete the proof.
echo "[CHECK C-A] no path-based durable_checkpoint caller outside its owners (ENFORCED)"
owner_re='^crates/aberp-snapshot/|^crates/aberp-db/src/lib\.rs$'
offenders=0
while IFS= read -r f; do
  [[ "$f" =~ $owner_re ]] && continue
  hits="$(ckpt_calls "$f")"
  [[ -z "$hits" ]] && continue
  offenders=$((offenders + 1))
  flag "✗ UN-SANCTIONED path-based checkpoint caller: $f"
  while IFS= read -r h; do note "    $f:${h}"; done <<< "$hits"
  note "    This renames a new inode over the live DB and unlinks its WAL while the"
  note "    shared Handle is open: the connection is stranded on the old inode, later"
  note "    commits go to a file the kernel frees at exit, and the lockstep sync_mirror"
  note "    durably mirrors them anyway — MIRROR AHEAD OF DB (the audit-chain fork)."
  note "    Use aberp_db::Handle::checkpoint_now() instead (ADR-0111), and add the site"
  note "    to $CENSUS."
done < <(find apps/*/src crates/*/src -name '*.rs' 2>/dev/null | sort)
[[ "$offenders" -eq 0 ]] && note "✓ the only callers are aberp-snapshot (owner) and the Handle's run_durable_checkpoint_locked"
echo

# ── CHECK C-B — the mechanism itself is intact ───────────────────────────────
# C-A only proves nobody calls the primitive directly. It is fully satisfied by
# a `checkpoint_now` that quietly stopped quiescing — which is the ORIGINAL
# defect wearing the fix's name. Pin the three load-bearing lines.
echo "[CHECK C-B] Handle::checkpoint_now takes the lock; the checkpoint quiesces AND reopens (ENFORCED)"
hb="crates/aberp-db/src/lib.rs"
if [[ ! -f "$hb" ]]; then
  flag "✗ $hb missing — the shared Handle is gone"
else
  # `checkpoint_now` exists, and its body reaches the locked runner. Read the
  # 40 lines after the signature: long enough for the real body, short enough
  # that a neighbouring method's call cannot satisfy it.
  ln="$(grep -nE 'pub fn checkpoint_now[[:space:]]*\(&self\)' "$hb" | head -1 | cut -d: -f1)"
  if [[ -z "$ln" ]]; then
    flag "✗ Handle::checkpoint_now is GONE — the three app checkpoint sites have nothing sanctioned to call"
  else
    body="$(sed -n "${ln},$((ln + 40))p" "$hb" | sed 's://.*::')"
    if printf '%s' "$body" | grep -q 'lock_recovering'; then
      note "✓ checkpoint_now acquires the writer mutex (lock_recovering)"
    else
      flag "✗ checkpoint_now no longer acquires the writer mutex — the swap would race live writers again"
    fi
    if printf '%s' "$body" | grep -q 'run_durable_checkpoint_locked'; then
      note "✓ checkpoint_now delegates to run_durable_checkpoint_locked"
    else
      flag "✗ checkpoint_now no longer delegates to run_durable_checkpoint_locked"
    fi
    # Deliberately UNCONDITIONAL: gating it on the D2 debounce window or on
    # `checkpoint_enabled` would silently drop the daemon/shutdown checkpoints
    # and re-open ADR-0095 root cause #2 (nothing folds the live file on a path
    # a crash traverses) — while every test in checkpoint_swap_orphan.rs, which
    # runs with checkpoint_enabled = false, would go vacuously green.
    if printf '%s' "$body" | grep -qE 'should_checkpoint_now|config\.checkpoint_enabled'; then
      flag "✗ checkpoint_now is now GATED (debounce window or checkpoint_enabled) — it is an"
      note "    explicit caller demand and must be unconditional; gating it makes the ADR-0111"
      note "    tests vacuous AND drops the daemon/shutdown checkpoints"
    else
      note "✓ checkpoint_now is unconditional (not gated by the D2 window or checkpoint_enabled)"
    fi
  fi

  rl="$(grep -nE 'fn run_durable_checkpoint_locked' "$hb" | head -1 | cut -d: -f1)"
  if [[ -z "$rl" ]]; then
    flag "✗ run_durable_checkpoint_locked is GONE — nothing quiesces the connection around the swap"
  else
    rbody="$(sed -n "${rl},$((rl + 60))p" "$hb" | sed 's://.*::')"
    if printf '%s' "$rbody" | grep -qE 'inner\.conn[[:space:]]*=[[:space:]]*None'; then
      note "✓ the checkpoint QUIESCES the shared connection (inner.conn = None) before the swap"
    else
      flag "✗ the checkpoint no longer drops the shared connection before the swap — atomic_install"
      note "    would orphan it on the old inode. This IS the ADR-0111 defect."
    fi
    if printf '%s' "$rbody" | grep -q 'open_runtime_connection'; then
      note "✓ the checkpoint REOPENS on the freshly-installed inode"
    else
      flag "✗ the checkpoint no longer reopens after the swap — every later write would be lost"
    fi
  fi
fi
echo

# ── CHECK C-C — the checkpoint_now() census is CLOSED ────────────────────────
echo "[CHECK C-C] checkpoint_now() call sites across apps/*/src match the census (ENFORCED)"
census_files=()
expected=0
while IFS= read -r line; do
  [[ -z "$line" || "$line" == \#* ]] && continue
  f="${line%%$'\t'*}"
  expected=$((expected + 1))
  listed=0
  for c in "${census_files[@]:-}"; do [[ "$c" == "$f" ]] && listed=1; done
  [[ "$listed" -eq 1 ]] || census_files+=("$f")
  if [[ ! -f "$f" ]]; then
    flag "✗ censused checkpoint site file is GONE: $f — update $CENSUS if the route moved"
  fi
done < "$CENSUS"

for f in "${census_files[@]}"; do
  [[ -f "$f" ]] || continue
  n="$(now_calls "$f" | wc -l | tr -d ' ')"
  if [[ "$n" -lt 1 ]]; then
    flag "✗ $f no longer calls checkpoint_now() — either a checkpoint route was deleted (the"
    note "    live file stops being folded on a path a crash traverses, ADR-0095 root #2) or it"
    note "    went back to the path-based primitive; C-A covers the second, this covers the first."
  else
    note "✓ $f — $n checkpoint_now() call site(s)"
  fi
done

actual=0
while IFS= read -r f; do
  n="$(now_calls "$f" | wc -l | tr -d ' ')"
  actual=$((actual + n))
  if [[ "$n" -gt 0 ]]; then
    listed=0
    for c in "${census_files[@]}"; do [[ "$c" == "$f" ]] && listed=1; done
    [[ "$listed" -eq 1 ]] || flag "✗ UNCENSUSED checkpoint_now() call site: $f — add it to $CENSUS, and say where its WriteGuards end (the writer mutex is NOT reentrant)"
  fi
done < <(find apps/*/src -name '*.rs' | sort)

if [[ "$actual" -ne "$expected" ]]; then
  flag "✗ checkpoint_now() call-site count $actual != census count $expected"
  note "    A count that drifted either way changes which paths fold the live DB."
else
  note "✓ $actual call site(s) across ${#census_files[@]} censused file(s) — census closed"
fi
echo

# ── CHECK C-D — the inode fence (belt) is present and load-bearing ───────────
# C-A/C-B are in-process discipline. Neither can see an out-of-process swapper
# (a second `aberp` invocation, an operator restore, a backup tool). The fence
# is what catches that, and its VALUE is the skip: on a mismatch it must not run
# the mirror sync, because that is exactly how the mirror gets ahead.
echo "[CHECK C-D] the WriteGuard inode fence is present and still skips the mirror sync (ENFORCED)"
if [[ -f "$hb" ]]; then
  dl="$(grep -nE 'fn drop\(&mut self\)' "$hb" | head -1 | cut -d: -f1)"
  if [[ -z "$dl" ]]; then
    flag "✗ WriteGuard::drop not found — the post-commit hook is gone"
  else
    dbody="$(sed -n "${dl},$((dl + 50))p" "$hb" | sed 's://.*::')"
    if printf '%s' "$dbody" | grep -q 'file_id' && printf '%s' "$dbody" | grep -q 'fence'; then
      note "✓ the guard compares the live file's identity against the one it opened on"
    else
      flag "✗ the ADR-0111 inode fence is gone from WriteGuard::drop — an out-of-process swap"
      note "    would again be fsync'd BY PATH (certifying the wrong inode) and mirrored from"
      note "    the orphaned connection (mirror ahead of DB)."
    fi
    if printf '%s' "$dbody" | grep -q 'LiveFileSwapped'; then
      note "✓ a detected swap parks a HARD ack failure (DbError::LiveFileSwapped)"
    else
      flag "✗ a detected swap no longer fails the money-path ack — the operator would be told a"
      note "    write succeeded that went to a file the kernel frees at exit (CLAUDE.md rule 11)."
    fi
    # The fence must SHORT-CIRCUIT. A fence that logs and falls through into the
    # hooks is a log line, not a guard.
    if printf '%s' "$dbody" | grep -qE '^[[:space:]]*return;[[:space:]]*$'; then
      note "✓ the fence short-circuits (returns) before the fsync + mirror sync"
    else
      flag "✗ the fence no longer returns early — it must SKIP fsync_data_paths and sync_mirror,"
      note "    not merely log; falling through is the mirror-ahead path with a warning attached."
    fi
  fi
fi

echo
if [[ "$fail" -ne 0 ]]; then echo "CUT-GATE: ✗ FAILED"; exit 1; fi
echo "CUT-GATE: ✓ PASSED"
