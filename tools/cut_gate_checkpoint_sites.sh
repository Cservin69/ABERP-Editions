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
# CHECK C-A — the FROZEN census of the whole rename-over-a-live-DB-path family
#             is closed in both directions. THE regression this gate exists for.
#             The first cut keyed on one function NAME and the PR #41
#             adversarial walked through it three ways (an aliased import, a
#             different public wrapper over the same rename, and the `pub`
#             rename primitive itself). See the census header for the shapes.
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
FAMILY_CENSUS="tools/adr0111_rename_family_sites.txt"
for c in "$CENSUS" "$FAMILY_CENSUS"; do
  [[ -f "$c" ]] || {
    note "✗ FAIL: census missing: $c"
    echo; echo "CUT-GATE: ✗ FAILED"; exit 1
  }
done

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
# A CALL of a sanctioned handle checkpoint entry point. BOTH spellings count:
# `checkpoint_now()` and the bounded `checkpoint_now_within(budget)` that clean
# shutdown uses (it must not park on the writer mutex and block process exit).
# Matching only the former would let a route silently disappear from the census
# by switching spelling — which is exactly what happened when the shutdown site
# was made bounded, and this check caught it.
now_calls() { strip_comments "$1" | grep -nE '\.checkpoint_now(_within)?[[:space:]]*\('; }

# ── the rename-over-a-live-DB-path FAMILY (CHECK C-A) ────────────────────────
#
# Every public entry point that ends up installing a different inode at a DB
# path: the four wrappers that reach `atomic_install`, `atomic_install` itself
# (it is `pub`), and `restore_into`, which does its own rename over the target.
# Keyed as a SET because the PR #41 adversarial defeated the single-name form.
FAMILY_SYMS="atomic_install durable_checkpoint live_durable_checkpoint provision_atomic resume_pending_install recover_or_refuse recover_or_refuse_with_audit restore_into"

# Code, with string literals AND comments removed — in that order.
#
# Strings first: stripping comments first would eat the tail of a line like
# `let u = "http://x";` at the `//`. Stripping strings first removes the whole
# literal, so the `//` inside it never reaches the comment pass.
#
# Removing string literals is what keeps `.context("ADR-0095 recover_or_refuse")`
# from counting as a touch — there are two of those in serve.rs, and counting
# them would make the census a transcript of log messages.
#
# KNOWN LIMITATION, deliberately left: this is line-based, so a family name
# inside a MULTI-LINE string (a `\`-continued `tracing::error!` message) still
# counts as a touch. That is the SAFE direction — the gate over-counts a mention
# rather than missing a call — and it cost exactly one reworded log line to
# discover. If you hit it: keep bare family symbol names out of multi-line log
# messages, or census the file. Do not "fix" it by loosening the stripper.
code_lines() { sed 's/"[^"]*"//g' "$1" | sed 's://.*::'; }

# TOUCHES of one family symbol in one file — not calls.
#
# A call matcher is exactly what the adversarial bypassed with
# `use ...live_durable_checkpoint as fold_live;`: the banned name appears only
# on the `use` line and the call is spelled `fold_live(..)`. So this counts every
# whole-word occurrence on a code line, which covers the call, the (aliased or
# plain) import, and a bare `let f = ...::atomic_install;` function pointer.
#
# The boundaries are hand-rolled rather than `\b` because BSD grep spells word
# boundaries differently from GNU, and because the prefix relations here are
# load-bearing: requiring a non-identifier on BOTH sides is what keeps
# `live_durable_checkpoint` from counting as `durable_checkpoint`,
# `recover_or_refuse_with_audit` from counting as `recover_or_refuse`, and the
# SANCTIONED `run_durable_checkpoint_locked` from counting as anything at all.
family_touches() { # file symbol -> count
  code_lines "$1" | grep -oE "(^|[^A-Za-z0-9_])$2([^A-Za-z0-9_]|\$)" | wc -l | tr -d ' '
}

# Production sources the family census governs. `crates/aberp-snapshot/**` is
# the owner (it defines and tests these) and is the ONLY exemption — note that
# `crates/aberp-db/src/lib.rs` is NOT exempt here: it is censused, so a new
# `atomic_install` inside the Handle would still have to be argued for.
family_scope() {
  find apps/*/src crates/*/src modules/*/src -name '*.rs' 2>/dev/null \
    | grep -v '^crates/aberp-snapshot/' | sort
}

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
printf '    match db.checkpoint_now_within(BUDGET) {\n' > "$probe"
[[ "$(now_calls "$probe" | wc -l | tr -d ' ')" == "1" ]] \
  || { note "✗ FAIL: matcher missed the BOUNDED .checkpoint_now_within() call"; fail=1; }
printf '/// [`aberp_db::Handle::checkpoint_now`] takes the writer mutex\n' > "$probe"
[[ "$(now_calls "$probe" | wc -l | tr -d ' ')" == "0" ]] \
  || { note "✗ FAIL: matcher counted a checkpoint_now doc mention"; fail=1; }

# ── family matcher liveness. This is the one that was DEFEATED once, so its
# controls are the three adversarial bypasses plus the shapes that must stay
# silent (the prefix relations, and string literals).
ft() { printf '%s' "$2" > "$probe"; family_touches "$probe" "$1"; }

[[ "$(ft live_durable_checkpoint 'use aberp_snapshot::live_durable_checkpoint as fold_live;')" == "1" ]] \
  || { note "✗ FAIL: family matcher missed an ALIASED import (adversarial bypass (a))"; fail=1; }
[[ "$(ft provision_atomic '    aberp_snapshot::provision_atomic(&db, |c| Ok(()))?;')" == "1" ]] \
  || { note "✗ FAIL: family matcher missed a provision_atomic call (bypass (b))"; fail=1; }
[[ "$(ft atomic_install '    aberp_snapshot::atomic_install(&staged, db_path)?;')" == "1" ]] \
  || { note "✗ FAIL: family matcher missed an atomic_install call (bypass (c))"; fail=1; }
[[ "$(ft atomic_install '    let f = aberp_snapshot::atomic_install;')" == "1" ]] \
  || { note "✗ FAIL: family matcher missed a bare function-pointer reference"; fail=1; }
# Must stay SILENT — the prefix relations. If any of these ever counts, the
# census counts become noise and the gate gets switched off.
[[ "$(ft durable_checkpoint '    aberp_snapshot::live_durable_checkpoint(&db, &t);')" == "0" ]] \
  || { note "✗ FAIL: family matcher counted live_durable_checkpoint AS durable_checkpoint"; fail=1; }
[[ "$(ft durable_checkpoint '        self.run_durable_checkpoint_locked(&mut inner);')" == "0" ]] \
  || { note "✗ FAIL: family matcher counted the SANCTIONED run_durable_checkpoint_locked"; fail=1; }
[[ "$(ft recover_or_refuse '    aberp_snapshot::recover_or_refuse_with_audit(&db)?;')" == "0" ]] \
  || { note "✗ FAIL: family matcher counted recover_or_refuse_with_audit AS recover_or_refuse"; fail=1; }
[[ "$(ft recover_or_refuse '    .context("ADR-0095 recover_or_refuse")?;')" == "0" ]] \
  || { note "✗ FAIL: family matcher counted a name inside a STRING LITERAL"; fail=1; }
[[ "$(ft atomic_install '/// see [`atomic_install`] for the swap protocol')" == "0" ]] \
  || { note "✗ FAIL: family matcher counted a doc-comment mention"; fail=1; }
# Strings are stripped BEFORE comments — pin that ordering, or a URL in a
# string literal would truncate the line at its `//` and hide a real touch.
[[ "$(ft atomic_install 'let u = "http://x"; aberp_snapshot::atomic_install(&s, &t);')" == "1" ]] \
  || { note "✗ FAIL: a string containing // hid a real touch (strip order regressed)"; fail=1; }

if [[ "$fail" -ne 0 ]]; then echo; echo "CUT-GATE: ✗ FAILED (matcher liveness)"; exit 1; fi
note "✓ matchers live"
echo

# ── CHECK C-A — the rename-family census is CLOSED in both directions ────────
# Scoped to production sources (`*/src/**`). Tests are deliberately OUT of
# scope: `aberp-snapshot`'s own suite calls these by design, and `aberp-db`'s
# ADR-0111 suite calls `durable_checkpoint` to SIMULATE the out-of-process
# swapper the inode fence exists to catch. Banning it there would delete the
# proof.
echo "[CHECK C-A] the rename-over-a-live-DB-path family census is closed (ENFORCED)"

# Expected counts from the census, keyed "<file>|<symbol>".
declare -a exp_keys=() exp_vals=()
while IFS= read -r line; do
  [[ -z "$line" || "$line" == \#* ]] && continue
  IFS=$'\t' read -r cf cs cn _ <<< "$line"
  if [[ -z "$cf" || -z "$cs" || -z "$cn" ]]; then
    flag "✗ malformed census line in $FAMILY_CENSUS: $line"
    continue
  fi
  if ! printf '%s' "$FAMILY_SYMS" | grep -qw -- "$cs"; then
    flag "✗ census names a symbol that is NOT in the family: $cs ($FAMILY_CENSUS)"
    note "    Either it was renamed upstream (update FAMILY_SYMS in this gate) or the"
    note "    entry is a typo that silently governs nothing."
  fi
  if [[ ! -f "$cf" ]]; then
    flag "✗ censused file is GONE: $cf — update $FAMILY_CENSUS if the call moved"
  fi
  exp_keys+=("$cf|$cs"); exp_vals+=("$cn")
done < "$FAMILY_CENSUS"

expected_of() { # key -> count, or empty
  local i
  for i in "${!exp_keys[@]}"; do
    [[ "${exp_keys[$i]}" == "$1" ]] && { printf '%s' "${exp_vals[$i]}"; return; }
  done
}

# Walk the ACTUAL tree. Pre-filter to files that mention any family name at all
# so the per-symbol pass runs over a handful of files, not the whole tree.
family_re="$(printf '%s' "$FAMILY_SYMS" | tr ' ' '|')"
seen_keys=()
while IFS= read -r f; do
  grep -qE "($family_re)" "$f" 2>/dev/null || continue
  for s in $FAMILY_SYMS; do
    n="$(family_touches "$f" "$s")"
    [[ "$n" -eq 0 ]] && continue
    key="$f|$s"
    seen_keys+=("$key")
    want="$(expected_of "$key")"
    if [[ -z "$want" ]]; then
      flag "✗ UNCENSUSED rename-family touch: $f touches \`$s\` ($n×)"
      note "    Every symbol in this family installs a DIFFERENT INODE at a DB path."
      note "    Done while the shared aberp_db::Handle is open, that strands its"
      note "    connection on the old inode: later commits go to a file the kernel frees"
      note "    at exit, and the lockstep sync_mirror durably mirrors them anyway —"
      note "    MIRROR AHEAD OF DB, the audit-chain fork."
      note "    Runtime checkpoints must use aberp_db::Handle::checkpoint_now() (ADR-0111)."
      note "    A touch is counted for a CALL, a \`use\` (aliased or not) and a bare"
      note "    function-pointer reference — all three were adversarial bypasses."
      note "    If the touch really is sound, add it to $FAMILY_CENSUS with its reason."
    elif [[ "$n" -ne "$want" ]]; then
      flag "✗ rename-family touch count DRIFTED: $f \`$s\` — census says $want, found $n"
      note "    One MORE touch is a new site that must be argued for; one FEWER means a"
      note "    boot-durability step was deleted. Both are reviewable edits of the census."
    else
      note "✓ $f — \`$s\` ×$n (censused)"
    fi
  done
done < <(family_scope)

# The other direction: a censused entry whose touches vanished entirely (the
# loop above only sees files that still touch something).
for i in "${!exp_keys[@]}"; do
  k="${exp_keys[$i]}"
  found=0
  for sk in "${seen_keys[@]:-}"; do [[ "$sk" == "$k" ]] && found=1; done
  if [[ "$found" -eq 0 ]]; then
    flag "✗ censused rename-family site is GONE: ${k%|*} no longer touches \`${k#*|}\`"
    note "    If that step was intentionally removed, remove its census line and say why"
    note "    in the PR — several of these are load-bearing BOOT durability steps."
  fi
done
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
    if printf '%s' "$dbody" | grep -q 'live_file_swapped' && printf '%s' "$dbody" | grep -q 'fence'; then
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

  # The guard-drop fence alone leaves a hole, and the PR #41 adversarial found
  # it: `durable_ack`'s `None` arm (no parked outcome — a money path whose write
  # was a no-op) falls through to `fsync_data_paths`, which opens BY PATH. After
  # a swap that flushes the brand-new inode and returns Ok(()) — an ack over a
  # file that has nothing to do with this handle's writes.
  al="$(grep -nE 'pub fn durable_ack' "$hb" | head -1 | cut -d: -f1)"
  if [[ -z "$al" ]]; then
    flag "✗ Handle::durable_ack is GONE — ADR-0110 D3's money-path claim has no implementation"
  else
    abody="$(sed -n "${al},$((al + 40))p" "$hb" | sed 's://.*::')"
    if printf '%s' "$abody" | grep -q 'live_file_swapped' \
       && printf '%s' "$abody" | grep -q 'LiveFileSwapped'; then
      note "✓ durable_ack's unparked (None) arm is fenced too — no by-path ack over a swapped file"
    else
      flag "✗ durable_ack no longer checks the inode fence before its by-path fsync fallback."
      note "    That arm opens fsync_data_paths BY PATH, so on a swapped file it certifies the"
      note "    NEW inode and returns Ok(()) — the operator is acked for a write that went to an"
      note "    orphan (ADR-0110 R3 / CLAUDE.md rule 11)."
    fi
  fi
fi

echo
if [[ "$fail" -ne 0 ]]; then echo "CUT-GATE: ✗ FAILED"; exit 1; fi
echo "CUT-GATE: ✓ PASSED"
