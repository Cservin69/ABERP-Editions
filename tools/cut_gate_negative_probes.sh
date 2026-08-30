#!/usr/bin/env bash
#
# cut_gate_negative_probes.sh — proves tools/cut_gate_db_isolation.sh has TEETH.
#
# For each invariant the cut-gate enforces, plant the corresponding VIOLATION
# in a throwaway COPY of the tree, run the gate against that copy, and assert
# it EXITS NON-ZERO with the matching CHECK's failure message. A green gate is
# only meaningful if it would have gone red on a real regression — this script
# is that proof, and it runs in CI alongside the gate (cut-gate.yml).
#
# The working tree is NEVER mutated; every probe operates on a fresh copy under
# a mktemp dir that is removed on exit.
#
# Exit 0 = every probe behaved (clean copy passes; each violation is caught).
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
GATE="tools/cut_gate_db_isolation.sh"
# Nest ALL of this run's scratch — the per-probe tree copies AND the gate's own
# mktemp temp files — under ONE run-unique dir, and free exactly that dir on
# exit. mktemp -d gives a unique name per invocation; the trap removes only this
# run's own dir, never a glob across cutgate-* . This makes a run UN-NUKEABLE by
# a concurrent probe run that blanket-purges "$TMPDIR"/cutgate-probes.* — a
# sibling project's wrapper did exactly that and wiped a validation run
# mid-flight. Our scratch now lives under "$TMPDIR"/cutgate-run.XXXXXX/…, which
# no cutgate-probes.* glob at the shared-$TMPDIR level can match; exporting
# TMPDIR makes the spawned gate inherit it, so ITS temps nest here too. chmod -R
# before rm: a copy can carry read-only dirs that rm alone cannot unlink (how
# earlier runs leaked husks into $TMPDIR).
RUN_TMP="$(mktemp -d "${TMPDIR:-/tmp}/cutgate-run.XXXXXX")"
export TMPDIR="$RUN_TMP"
trap 'chmod -R u+w "$RUN_TMP" 2>/dev/null; rm -rf "$RUN_TMP"' EXIT
WORK="$(mktemp -d "$TMPDIR/cutgate-probes.XXXXXX")"
pass=0; bad=0
i=0

# ── SHARDING — a PARTITION of the suite, never a reduction of it ─────────────
# This harness is O(probes x full-gate-run): every probe copies the whole tree
# and runs the WHOLE gate against it. At 77 probes that measured 70m10s on a
# GitHub runner — 94% of cut-gate.yml's 75-minute cap, with 4m50s to spare —
# while the IDENTICAL harness on IDENTICAL input took 54m38s two runs earlier.
# Ordinary runner variance was therefore already able to CANCEL a REQUIRED
# check (a false red on main, since cut-gate.yml also runs on push:main), and
# the next probe-adding ADR blew the cap outright. See SAW-OFF.md.
#
# The fix keeps every probe exactly as it is. Each shard runs a DISJOINT subset
# of the SAME probes: the same plant, against the same full fresh() tree copy,
# asserting the same signature against the same complete gate. Nothing is
# batched, cached, reordered, approximated or shared between probes, and no
# probe is dropped — cut-gate.yml runs PROBE_SHARD_TOTAL of these jobs in
# parallel and a fan-in job requires EVERY one of them green.
#
# Defaults are 1/1 — with no environment set this script behaves exactly as the
# un-sharded harness did, so a local `bash tools/cut_gate_negative_probes.sh`
# still runs the complete suite.
#
# The partition is round-robin on the probe ORDINAL (probe n -> shard
# ((n-1) % TOTAL) + 1), NOT a contiguous split: probes are grouped by CHECK in
# file order and their costs differ, so a contiguous split would pile all of
# CHECK 11 into one shard and simply move the cliff. Round-robin gives every
# shard probes from every CHECK family and near-equal cost.
PROBE_SHARD_TOTAL="${PROBE_SHARD_TOTAL:-1}"
PROBE_SHARD_INDEX="${PROBE_SHARD_INDEX:-1}"
# EXPECTED_PROBES is a FREEZE, in the same spirit as every other manifest this
# gate holds — it is the anti-silent-drop teeth for the sharding itself. The
# shard's expected workload is derived from THIS number rather than from
# anything the run accumulated, so a deleted probe, or a fresh() that loses its
# accounting site (which is how a probe stops being a probe), moves the count
# and goes RED instead of quietly testing less. Adding a probe is a deliberate
# one-line bump here.
EXPECTED_PROBES=77
if [[ ! "$PROBE_SHARD_TOTAL" =~ ^[1-9][0-9]*$ ]] || [[ ! "$PROBE_SHARD_INDEX" =~ ^[1-9][0-9]*$ ]] \
   || (( PROBE_SHARD_INDEX > PROBE_SHARD_TOTAL )); then
  echo "NEGATIVE-PROBES: ✗ FAILED — bad shard spec (1-based, index <= total): PROBE_SHARD_INDEX=$PROBE_SHARD_INDEX PROBE_SHARD_TOTAL=$PROBE_SHARD_TOTAL" >&2
  exit 1
fi
# The probe ordinal must survive `c="$(fresh)"`. fresh() runs inside a command
# substitution SUBSHELL, so a shell variable incremented there is lost in the
# parent — this file already carries that exact scar (see the fresh() note on
# the `i=$((i+1))` counter that never persisted and made every copy collide).
# A FILE survives the subshell; the harness is strictly serial, so a plain
# read-modify-write is sound here.
PROBE_CTR="$RUN_TMP/probe.ordinal"; printf '0' > "$PROBE_CTR"
skipped=0
probe_skipped() {  # true iff the CURRENT probe's ordinal belongs to another shard
  local n; n="$(cat "$PROBE_CTR")"
  (( (n - 1) % PROBE_SHARD_TOTAL != PROBE_SHARD_INDEX - 1 ))
}

fresh() {  # -> path to a fresh, clean copy of the tree (excludes .git)
  # Claim this probe's ordinal. Deliberately the FIRST thing fresh() does, and
  # deliberately unconditional: fresh() is called exactly once per probe, in
  # probe order, so the ordinal is exact. Note this function must print NOTHING
  # but the path — its stdout IS its return value.
  local n; n=$(( $(cat "$PROBE_CTR") + 1 )); printf '%s' "$n" > "$PROBE_CTR"
  # A probe belonging to another shard still gets a REAL copy. Handing back a
  # stub would be cheaper, but every plant below writes into this tree with
  # `printf >>`, `perl -0pi`, `grep -v`, and python3 heredocs that `assert` on an
  # anchor string — against a stub those fail loudly and fill the log of a
  # perfectly healthy shard with noise, and against a SHARED sink they would
  # accumulate and make each other's anchors drift. Fidelity is worth the copy:
  # the plant runs identically in every shard, only the gate run is skipped.
  # NOTE (ADR-0098 Session C): use mktemp -d for a UNIQUE dir per call. The
  # prior `i=$((i+1)); d="$WORK/copy.$i"` form incremented `i` inside this
  # function's command-substitution subshell (`c="$(fresh)"`), so the counter
  # never persisted in the parent — every copy collided on copy.1 and
  # ACCUMULATED each probe's planted violation. Harmless for the expect_fail
  # probes (the gate fails regardless), but it made any expect_pass probe after
  # the first plant spuriously fail. Unique dirs fix it for good.
  local d; d="$(mktemp -d "$WORK/copy.XXXXXX")"
  # Exclude the build/dependency dirs the gate NEVER reads. Proven safe: the
  # gate (cut_gate_db_isolation.sh) invokes no compiler/runtime and every scan
  # is rooted at source (run/ apps/ crates/ modules/ tools/ adr/ + named
  # markers); the set of *.rs it walks is byte-identical with or without
  # target/ (there is no *.rs under any target/ inside a scanned root). Dropping
  # target/ takes each copy from ~54G/213s to ~75M/1s — and every probe plants
  # its violation in source, so the excluded dirs cannot change any verdict.
  # target/ is anchored to the top level (./target) so a source dir that merely
  # shares the name could never be dropped; node_modules/.venv are absent today
  # but excluded to stay disk-safe if a future toolchain adds them.
  tar -C "$ROOT" --exclude=.git --exclude=./target --exclude=./node_modules --exclude=./.venv -cf - . | tar -C "$d" -xf -
  # Plant-detection marker (see assert_planted), kept OUTSIDE the copy so the
  # gate never sees an extra file.
  #
  # Every extracted mtime is first normalised to a fixed PAST instant, then the
  # marker is stamped fractionally later. A plant writes "now", which is
  # unambiguously newer than the marker on any filesystem granularity.
  #
  # The obvious `: > "${d}.marker"` (marker = "now") is RACY and was caught by
  # CI: `find -newer` is strictly-greater, so a fast one-liner plant
  # (`c="$(fresh)"; printf ... >> file`) lands in the SAME timestamp tick as the
  # marker and reads as "nothing planted" — 13 of 43 probes false-flagged as
  # HARNESS BUG on the ubuntu runner while all 43 passed on macOS, whose slower
  # shell and nanosecond APFS timestamps hid the race. Pinning both ends removes
  # the timing dependency entirely instead of narrowing it.
  # Costs ~55ms per probe on a clean 1097-file export.
  find "$d" -exec touch -t 199901010000 {} + 2>/dev/null
  touch -t 200001010000 "${d}.marker"
  printf '%s' "$d"
}
# Did the probe's plant actually modify the tree?
#
# WHY THIS EXISTS: a plant that silently does nothing makes its probe report
# "✗ ESCAPED", which reads as "the GATE is broken" when in truth the HARNESS
# never planted the violation — the gate correctly passed a pristine tree. That
# is worse than a plain failure: it is a red that sends you to audit the wrong
# code, and it means the probe provides ZERO real coverage while looking like it
# provides some. It bit CHECK 5 for exactly this reason (a GNU-only `sed -i` that
# is a parse error on BSD/macOS, so the mutation never landed).
#
# The check is mechanism-agnostic on purpose: it catches a no-op `sed`, a python
# plant whose anchor string drifted, and a `grep -v` filter that matched nothing,
# without the harness having to know which idiom a given probe used.
assert_planted() {  # $1 dir -> non-zero if nothing was modified since fresh()
  local d="$1" m="${1}.marker"
  [[ -e "$m" ]] || return 0   # no marker (dir not from fresh()) → don't block
  [[ -n "$(find "$d" -newer "$m" -print -quit 2>/dev/null)" ]]
}
gate_rc() {  # run the COPY's gate; echo exit code; stash output in $1/.out
  ( cd "$1" && bash "$GATE" ) >"$1/.out" 2>&1
  echo $?
}

expect_pass() {  # $1 dir  $2 label
  # Not this shard's probe: the plant above already ran (identically), we simply
  # do not spend a full gate run proving what another shard is proving.
  if probe_skipped; then skipped=$((skipped+1)); return 0; fi
  local rc; rc="$(gate_rc "$1")"
  if [[ "$rc" == "0" ]]; then
    printf '  ✓ %s\n' "$2"; pass=$((pass+1))
  else
    printf '  ✗ BROKEN: %s — clean copy should PASS but gate exit=%s\n' "$2" "$rc"
    sed 's/^/        /' "$1/.out"; bad=$((bad+1))
  fi
}
expect_fail() {  # $1 dir  $2 signature  $3 label
  # Not this shard's probe: the plant above already ran (identically), we simply
  # do not spend a full gate run proving what another shard is proving.
  if probe_skipped; then skipped=$((skipped+1)); return 0; fi
  # MUST run before gate_rc: gate_rc writes "$1/.out" inside the copy, which
  # would itself satisfy the -newer test and mask a no-op plant.
  if ! assert_planted "$1"; then
    printf '  ✗ HARNESS BUG: %s — the plant modified NOTHING, so this probe tests nothing.\n' "$3"
    printf '        (the gate is not implicated: it correctly passed a pristine tree)\n'
    bad=$((bad+1)); return
  fi
  local rc; rc="$(gate_rc "$1")"
  if [[ "$rc" != "0" ]] && grep -qF -- "$2" "$1/.out"; then
    printf '  ✓ caught: %s  (exit=%s; matched: "%s")\n' "$3" "$rc" "$2"; pass=$((pass+1))
  else
    printf '  ✗ ESCAPED: %s  (exit=%s; expected non-zero + "%s")\n' "$3" "$rc" "$2"
    sed 's/^/        /' "$1/.out"; bad=$((bad+1))
  fi
}

echo "negative probes for the ADR-0093/0002 DB-isolation cut-gate"
echo "root: $ROOT"
echo

echo "[sanity] a clean copy passes"
c="$(fresh)"; expect_pass "$c" "clean tree → CUT-GATE PASSED"

echo "[CHECK 1] planting run/run_prod.sh (prod launch surface)"
c="$(fresh)"; printf '#!/usr/bin/env bash\necho prod\n' > "$c/run/run_prod.sh"
expect_fail "$c" "must not carry the prod launcher" "prod launcher re-added"

echo "[CHECK 2] removing SAW-OFF.md (saw-off sentinel)"
c="$(fresh)"; rm -f "$c/SAW-OFF.md"
expect_fail "$c" "SAW-OFF.md missing" "saw-off sentinel removed"

echo "[CHECK 3] launcher resolving prod's DB root ~/.aberp/prod"
c="$(fresh)"; printf '\nDATA_DIR="${HOME}/.aberp/prod/${TENANT}"\n' >> "$c/run/run_portable.sh"
expect_fail "$c" "resolve prod's tenant/DB root" "launcher points back at ~/.aberp/prod"

echo "[CHECK 4] re-introducing the silent-truncate reconcile path"
c="$(fresh)"; printf '\n// regression: let _ = RecoveryAction::Truncated;\n' >> "$c/crates/audit-ledger/src/mirror.rs"
expect_fail "$c" "silent-truncate path" "RecoveryAction::Truncated re-introduced"

echo "[CHECK 5] in-place live-file rewrite (rename(2) -> in-place copy)"
c="$(fresh)"
# Replace the atomic rename swap with an in-place copy (the anti-pattern).
#
# Uses the python3 heredoc that every other in-place plant in this file uses.
# It was `sed -i 's#...#...#'`, which is GNU-only: BSD/macOS sed reads the next
# argument as the backup-file suffix, so it consumed the script, failed with
# "invalid command code f", and left the file UNTOUCHED. The gate then passed a
# pristine tree and this probe reported a bogus "ESCAPED" — red for the wrong
# reason, and zero real coverage on any Mac. CI (ubuntu, GNU sed) never saw it.
python3 - "$c/crates/aberp-snapshot/src/crash_safe.rs" <<'PYIN'
import sys
p = sys.argv[1]
old = "std::fs::rename(staged, target)"
new = "std::fs::copy(staged, target).map(|_| ())"
s = open(p).read()
# Fail loudly if the anchor ever drifts, rather than silently writing the file
# back unchanged and letting the probe report a bogus escape.
assert old in s, f"CHECK 5 probe anchor {old!r} not found in {p}"
open(p, "w").write(s.replace(old, new))
PYIN
expect_fail "$c" "no longer swaps via std::fs::rename" "checkpoint regressed to in-place rewrite"

echo "[CHECK 6] binary source resolving prod's bare snapshot store"
c="$(fresh)"
printf 'pub fn _probe() { let _ = default_store_dir("prod"); }\n' > "$c/apps/aberp/src/zz_probe_violation.rs"
expect_fail "$c" "calls prod-shaped default_store_dir" "binary reaches prod's bare snapshot store"

echo "[CHECK 7] rogue Defense launcher that crosses arms (production -> .aberp-portable)"
c="$(fresh)"
cat > "$c/run/run_defense_rogue.sh" <<'ROGUE'
#!/usr/bin/env bash
# A new "Defense" launcher that builds the production arm but binds the WRONG
# (Portable) root — the exact mismatch CHECK 3b cannot see.
readonly HOME_DIR="${HOME}/.aberp-portable/${TENANT}"
cargo build --release --features production --bin aberp
ROGUE
expect_fail "$c" "binds a non-defense root" "production-arm launcher pointed at .aberp-portable"

echo "[CHECK 8] a NEW storefront daemon spawned OUTSIDE the gate (the core anti-regression)"
c="$(fresh)"
cat >> "$c/apps/aberp/src/serve.rs" <<'ROGUE'

// ROGUE PROBE: a new storefront-reaching daemon spawned with NO edition gate.
fn _rogue_storefront_probe() {
    let rogue_handle = tokio::spawn(async move {});
    coordinator.register("storefront-sync", rogue_handle);
}
ROGUE
expect_fail "$c" "is NOT behind storefront_polling_allowed" "ungated storefront daemon spawn added"

echo "[CHECK 8] removing the serve.rs boot guard (storefront reach un-refused at boot)"
c="$(fresh)"; grep -v 'guard_storefront_reach_matches_edition();' "$c/apps/aberp/src/serve.rs" > "$c/apps/aberp/src/serve.rs.tmp" && mv "$c/apps/aberp/src/serve.rs.tmp" "$c/apps/aberp/src/serve.rs"
expect_fail "$c" "boot guard guard_storefront_reach_matches_edition missing or not wired" "boot guard calls removed"

echo "[CHECK 8] removing the storefront-reach predicate from build_profile.rs"
c="$(fresh)"; grep -v 'pub fn assert_storefront_reach_allowed' "$c/apps/aberp/src/build_profile.rs" > "$c/apps/aberp/src/build_profile.rs.tmp" && mv "$c/apps/aberp/src/build_profile.rs.tmp" "$c/apps/aberp/src/build_profile.rs"
expect_fail "$c" "missing the storefront-reach predicate" "assert_storefront_reach_allowed removed"

echo "[CHECK 8] un-gating an on-demand storefront handler (handle_test_quote_intake_connection)"
c="$(fresh)"
python3 - "$c/apps/aberp/src/serve.rs" <<'PYIN'
import sys
p=sys.argv[1]; L=open(p).read().split("\n")
# find the handler signature, strip storefront_polling_allowed from its next 50 lines
for i,l in enumerate(L):
    if "fn handle_test_quote_intake_connection" in l:
        for j in range(i, min(i+50, len(L))):
            if "storefront_polling_allowed" in L[j]:
                L[j] = "        // (gate removed by negative probe)"
        break
open(p,"w").write("\n".join(L))
PYIN
expect_fail "$c" "handler handle_test_quote_intake_connection does NOT gate on storefront_polling_allowed" "handler gate removed"

echo "[CHECK 9] editions upgrade re-defaulting the reserved prod tenant (bare tenant=\"prod\", which CHECK 3a misses)"
c="$(fresh)"; printf '\ntenant="prod"\n' >> "$c/run/upgrade_defense.sh"
expect_fail "$c" "defaults the reserved prod tenant" "editions upgrade re-defaulted tenant=prod (bare)"

echo "[CHECK 9] editions upgrade routing its snapshot at the BARE prod root ~/.aberp/ (no literal prod, which CHECK 3a misses)"
c="$(fresh)"; printf '\nSNAP_ROOT="${HOME}/.aberp/${TENANT}"\n' >> "$c/run/upgrade_defense.sh"
expect_fail "$c" "references the frozen prod data root" "editions upgrade pointed back at the bare ~/.aberp/"

echo "[CHECK 9] editions upgrade invoking snapshot-prod.sh WITHOUT ABERP_DATA_ROOT (would fall back to prod root)"
c="$(fresh)"
python3 - "$c/run/upgrade_defense.sh" <<'PYIN'
import sys
p=sys.argv[1]; s=open(p).read()
s=s.replace('ABERP_DATA_ROOT="${EDITION_DATA_ROOT}" "$SNAPSHOT_SCRIPT"', '"$SNAPSHOT_SCRIPT"')
open(p,"w").write(s)
PYIN
expect_fail "$c" "without ABERP_DATA_ROOT" "snapshot invocation lost its edition root"

echo "[CHECK 9] snapshot-prod.sh hardcoding the prod root back (ABERP_DATA_ROOT override removed)"
c="$(fresh)"
python3 - "$c/tools/snapshot-prod.sh" <<'PYIN'
import sys
p=sys.argv[1]; s=open(p).read()
s=s.replace('readonly DATA_ROOT="${ABERP_DATA_ROOT:-${HOME}/.aberp}"', 'readonly DATA_ROOT="${HOME}/.aberp"')
open(p,"w").write(s)
PYIN
expect_fail "$c" "no longer honors ABERP_DATA_ROOT" "snapshot-prod.sh hardcoded the prod root"

echo "[CHECK 10] a NEW live-path Connection::open in a migrated daemon (the 17:02 separate-instance regression)"
c="$(fresh)"
python3 - "$c/apps/aberp/src/email_relay_daemon.rs" <<'PYIN'
import sys
p=sys.argv[1]; s=open(p).read()
needle="pub async fn run_drain_loop(deps: EmailRelayDaemonDeps, cancel: CancellationToken) {"
assert needle in s, "drain-loop anchor moved — probe is stale"
s=s.replace(needle, needle+'\n    let _stray = duckdb::Connection::open(&deps.db_path).expect("regression");', 1)
open(p,"w").write(s)
PYIN
expect_fail "$c" "live-path Connection::open OUTSIDE the Handle" "CHECK 10d — stray separate-instance open planted in the email-relay drain"

echo "[CHECK 10] the shared aberp_db::Handle crate removed (single-instance seam deleted)"
c="$(fresh)"; rm -f "$c/crates/aberp-db/src/lib.rs"
expect_fail "$c" "Handle missing or missing its write()/read()/open_runtime_connection" "CHECK 10a — aberp_db Handle crate deleted"

echo "[CHECK 10 try_clone] Handle read() regressed from try_clone to a SEPARATE read-only instance (AccessMode::ReadOnly / open_with_flags) — coherence regression"
c="$(fresh)"
# Re-introduce the removed F5 separate read-only opener inside the Handle -- the
# exact stale-read vector Option 1 eliminated. The gate's 10c-tryclone must red.
printf '\nfn _f5_regression_probe(p: &std::path::Path) -> Result<duckdb::Connection, duckdb::Error> {\n    let cfg = duckdb::Config::default().access_mode(duckdb::AccessMode::ReadOnly)?;\n    duckdb::Connection::open_with_flags(p, cfg)\n}\n' >> "$c/crates/aberp-db/src/lib.rs"
expect_fail "$c" "must be a try_clone of the shared instance" "CHECK 10 — Handle read() regressed to a separate read-only AccessMode/open_with_flags instance"

echo "[CHECK 10f] a NEW live-path Connection::open planted in a serve.rs REQUEST HANDLER (Session-C two-lock-regime regression)"
c="$(fresh)"
python3 - "$c/apps/aberp/src/serve.rs" <<'PYIN'
import sys
p=sys.argv[1]; s=open(p).read()
needle="    let partners = partners::list_partners(&conn, state.tenant.as_str(), search)?;"
assert needle in s, "list_partners_request anchor moved — probe is stale"
s=s.replace(needle, '    let _stray = duckdb::Connection::open(&*state.db_path).expect("CHECK10f regression");\n'+needle, 1)
open(p,"w").write(s)
PYIN
expect_fail "$c" "OUTSIDE the Handle (Session-C regression)" "CHECK 10f — stray separate-instance open planted in a serve.rs request handler"

echo "[CHECK 10f] a Connection::open added INSIDE a #[cfg(test)] block must NOT trip (cfg(test)-aware precision, no false-positive)"
c="$(fresh)"
python3 - "$c/apps/aberp/src/serve.rs" <<'PYIN'
import sys
p=sys.argv[1]; s=open(p).read()
needle='let conn = Connection::open(&db).expect("open demo db");'
assert needle in s, "cfg(test) anchor moved — probe is stale"
s=s.replace(needle, needle+'\n        let _t = duckdb::Connection::open(&db).expect("test-only stray must be ignored by the scan");', 1)
open(p,"w").write(s)
PYIN
expect_pass "$c" "CHECK 10f — Connection::open inside #[cfg(test)] is correctly IGNORED (scan is cfg(test)-aware, not blind)"

echo "[CHECK 10g] the snapshot-EXPORT SANCTIONED-RESIDUAL allow-list marker removed from take.rs"
c="$(fresh)"
python3 - "$c/crates/aberp-snapshot/src/take.rs" <<'PYIN'
import sys
p=sys.argv[1]; s=open(p).read()
needle="SANCTIONED RESIDUAL (gate allow-listed; FLAGGED)."
assert needle in s, "take.rs residual marker anchor moved — probe is stale"
s=s.replace(needle, "(allow-list marker removed by negative probe).", 1)
open(p,"w").write(s)
PYIN
expect_fail "$c" "snapshot EXPORT opener allow-list marker missing" "CHECK 10g — snapshot-EXPORT residual marker removed (undocumented opener)"

echo "[CHECK 10h] a runtime Ledger::open planted in a MIGRATED NAV daemon (ap_sync) — C2 audit-seam regression"
c="$(fresh)"; printf '\nfn _c2_probe_ledger_open() {\n    let _ = Ledger::open(std::path::Path::new("/x"), "t", "h");\n}\n' >> "$c/apps/aberp/src/ap_sync.rs"
expect_fail "$c" "(Session-C2 regression)" "CHECK 10h — Ledger::open re-added to ap_sync (the opener class C2 banned)"

echo "[CHECK 10h] a runtime DuckDbBillingStore::open planted in submit_invoice — C2 billing-seam regression"
c="$(fresh)"; printf '\nfn _c2_probe_billing_open() {\n    let _ = DuckDbBillingStore::open("/x");\n}\n' >> "$c/apps/aberp/src/submit_invoice.rs"
expect_fail "$c" "(Session-C2 regression)" "CHECK 10h — DuckDbBillingStore::open re-added to submit_invoice (the un-inventoried opener class F4)"

echo "[CHECK 10h] a Connection::open INSIDE a #[cfg(test)] block of a migrated file must NOT trip (cfg(test)-aware)"
c="$(fresh)"
python3 - "$c/apps/aberp/src/ap_sync.rs" <<'PYIN'
import sys
p=sys.argv[1]; s=open(p).read()
needle="let db_path = tmp.join(\"tenant.duckdb\");"
assert needle in s, "ap_sync cfg(test) anchor moved — probe is stale"
s=s.replace(needle, needle+"\n        let _t = Connection::open(&db_path).unwrap(); // test-only stray, scan must ignore", 1)
open(p,"w").write(s)
PYIN
expect_pass "$c" "CHECK 10h — Connection::open inside #[cfg(test)] of a migrated file is correctly IGNORED"

echo "[CHECK 10i] an operator-module residual GROWS its opener count (quality.rs +1) — frozen ledger must catch it"
c="$(fresh)"; printf '\nfn _c2_probe_grow() {\n    let _ = duckdb::Connection::open("/x");\n}\n' >> "$c/apps/aberp/src/quality.rs"
expect_fail "$c" "grew its residual openers" "CHECK 10i — operator-module residual opener count grew beyond its frozen baseline"

echo "[CHECK 10i] a BRAND-NEW opener-bearing file not on the frozen ledger — must be caught (no silent new opener)"
c="$(fresh)"; printf 'fn _c2_probe_new_opener() {\n    let _ = duckdb::Connection::open("/x");\n}\n' > "$c/apps/aberp/src/zz_c2_probe_opener.rs"
expect_fail "$c" "NEW unaccounted opener-bearing file" "CHECK 10i — a new unlisted runtime-opener file is rejected"

echo "[CHECK 10j] the no-in-place-fold pragma STRIPPED from a frozen residual Connection::open — must be caught (silent fold-on-close)"
c="$(fresh)"
python3 - "$c/apps/aberp/src/quote_calibration.rs" <<'PYIN'
import sys
p=sys.argv[1]; s=open(p).read()
block='    conn.execute_batch("PRAGMA disable_checkpoint_on_shutdown;")\n        .context("ADR-0098 R3 (finding C): disable implicit close-checkpoint on residual opener")?;\n'
assert block in s, "quote_calibration pragma anchor moved — probe is stale"
s=s.replace(block,'',1)
open(p,"w").write(s)
PYIN
expect_fail "$c" "residual Connection::open has NO disable_checkpoint_on_shutdown within" "CHECK 10j — pragma stripped from a frozen residual opener (silent close-checkpoint fold)"

echo "[CHECK 10j] the central audit-ledger Ledger::open pragma removed — its residual callers lose the guard"
c="$(fresh)"
python3 - "$c/crates/audit-ledger/src/storage/mod.rs" <<'PYIN'
import sys
p=sys.argv[1]; s=open(p).read()
needle='        conn.execute_batch("PRAGMA disable_checkpoint_on_shutdown;")?;\n'
assert needle in s, "audit-ledger Ledger::open pragma anchor moved — probe is stale"
s=s.replace(needle,'',1)
open(p,"w").write(s)
PYIN
expect_fail "$c" "missing disable_checkpoint_on_shutdown" "CHECK 10j — central audit-ledger Ledger::open pragma removed (its ~145 residual callers lose the guard)"

echo "[CHECK 10j] a pragma-less Connection::open INSIDE a #[cfg(test)] block must NOT trip 10j (cfg(test)-aware precision)"
c="$(fresh)"
python3 - "$c/apps/aberp/src/ap_sync.rs" <<'PYIN'
import sys
p=sys.argv[1]; s=open(p).read()
needle='let db_path = tmp.join("tenant.duckdb");'
assert needle in s, "ap_sync cfg(test) anchor moved — probe is stale"
s=s.replace(needle, needle+'\n        let _t = Connection::open(&db_path).unwrap(); // test-only, no pragma, 10j must ignore', 1)
open(p,"w").write(s)
PYIN
expect_pass "$c" "CHECK 10j — a pragma-less Connection::open inside #[cfg(test)] is correctly IGNORED (not a residual)"

echo "[CHECK 10i/crates] R4 finding H·a — a NEW live-path Connection::open in a business crate (aberp-qa) that the pre-R4 scope (apps/aberp+modules only) could NOT see"
c="$(fresh)"
printf 'pub fn _r4_probe_new_crate_opener(p: &std::path::Path) -> Result<duckdb::Connection, duckdb::Error> {\n    duckdb::Connection::open(p)\n}\n' > "$c/crates/aberp-qa/src/zz_r4_probe_opener.rs"
expect_fail "$c" "NEW unaccounted opener-bearing file" "CHECK 10i/crates — a new separate opener in crates/ (invisible pre-R4) is now rejected"

echo "[CHECK 10h/alias] R4 finding H·b — an ALIASED live-DB open (use duckdb::Connection as X; X::open) in a migrated file (ap_sync) that the literal-token scan would MISS"
c="$(fresh)"
printf '\nuse duckdb::Connection as R4AliasConn;\nfn _r4_probe_alias_open(p: &std::path::Path) -> Result<R4AliasConn, duckdb::Error> {\n    R4AliasConn::open(p)\n}\n' >> "$c/apps/aberp/src/ap_sync.rs"
expect_fail "$c" "(Session-C2 regression)" "CHECK 10h/alias — an aliased Connection::open (alias-evasion) is caught, not invisible"

echo "[CHECK 10h/alias] R4 finding H·b — an aliased open INSIDE a #[cfg(test)] block must NOT trip (alias scan is cfg(test)-aware, no false-positive)"
c="$(fresh)"
printf '\n#[cfg(test)]\nmod r4_alias_test_probe {\n    use duckdb::Connection as TAlias;\n    fn t(p: &std::path::Path) { let _ = TAlias::open(p); }\n}\n' >> "$c/apps/aberp/src/ap_sync.rs"
expect_pass "$c" "CHECK 10h/alias — an aliased open inside #[cfg(test)] is correctly IGNORED (cfg(test)-aware alias scan)"

echo "[CHECK 10k] R4 finding H·c — a COUNT-PRESERVING opener swap (mutate a frozen opener line) — 10i count stays green, 10k fingerprint must catch it"
c="$(fresh)"
python3 - "$c/apps/aberp/src/tenant_registry.rs" <<'PYIN'
import sys
p=sys.argv[1]; s=open(p).read()
old="let mut ledger = Ledger::open(db_path, tenant, binary_hash)"
new="let mut ledger_swapped = Ledger::open(db_path, tenant, binary_hash)"
assert s.count(old)==1, "10k probe anchor not unique"
open(p,"w").write(s.replace(old,new,1))
PYIN
expect_fail "$c" "opener fingerprint set DIVERGED" "CHECK 10k — a count-preserving intra-file opener swap is caught by the fingerprint freeze"

# ADR-0104 re-anchored this probe. It used to strip the pragma from
# aberp-mes ledger_writer::write_one, but that opener is GONE — the MES
# writer moved onto the shared aberp_db::Handle, so aberp-mes owns no
# runtime opener at all. The probe now targets the inventory rebuild CLI,
# which is still a frozen crates/ residual, so the R6 crates-scope
# extension keeps its teeth.
echo "[CHECK 10j/crates] R6 (NEW-3) — pragma STRIPPED from a frozen CRATE residual opener (aberp-inventory rebuild_stock_cache) — the R6 crates-scope extension must catch it"
c="$(fresh)"
python3 - "$c/crates/aberp-inventory/src/bin/rebuild_stock_cache.rs" <<'PYIN'
import sys
p=sys.argv[1]; s=open(p).read()
block='    conn.execute_batch("PRAGMA disable_checkpoint_on_shutdown;")\n        .context("PRAGMA disable_checkpoint_on_shutdown on inventory rebuild residual opener (ADR-0098 R6)")?;\n'
assert block in s, "aberp-inventory rebuild_stock_cache pragma anchor moved -- probe is stale"
open(p,"w").write(s.replace(block,'',1))
PYIN
expect_fail "$c" "residual Connection::open has NO disable_checkpoint_on_shutdown within" "CHECK 10j/crates -- R6: pragma stripped from a crate residual opener (invisible pre-R6 crates-scope extension)"


echo "[CHECK 10L] a raw Connection/Ledger opener + rogue sync_mirror REPLANTED inside the migrated boot seam append_backfill_cycle_entry — the R7 re-fork must go red"
c="$(fresh)"
python3 - "$c/apps/aberp/src/restore_from_nav_outgoing.rs" <<'PYIN'
import sys
p=sys.argv[1]; s=open(p).read()
needle='    audit_ledger::ensure_schema(&guard)\n        .context("ensure audit-ledger schema for backfill cycle audit entry")?;'
assert needle in s, "append_backfill_cycle_entry anchor moved — probe is stale"
s=s.replace(needle, needle+'\n    let _r7 = Ledger::open(&inputs.db_path, inputs.tenant.clone(), inputs.binary_hash); let _m = _r7.map(|l| l.sync_mirror(std::path::Path::new("/x"))); // R7 negative probe', 1)
open(p,"w").write(s)
PYIN
expect_fail "$c" "ADR-0098 R7 regression" "CHECK 10L — opener+sync_mirror replanted in append_backfill_cycle_entry (the boot re-fork) is caught"

echo "[CHECK 10L] a raw opener + rogue sync_mirror REPLANTED inside the migrated invoicing seam change_status — the R7 re-fork must go red"
c="$(fresh)"
python3 - "$c/apps/aberp/src/incoming_invoices.rs" <<'PYIN'
import sys
p=sys.argv[1]; s=open(p).read()
needle='    ensure_schema(&guard).context("ensure ap_invoice schema (status change)")?;'
assert needle in s, "change_status anchor moved — probe is stale"
s=s.replace(needle, needle+'\n    let _r7 = Ledger::open(db_path, tenant.clone(), binary_hash); let _m = _r7.map(|l| l.sync_mirror(std::path::Path::new("/x"))); // R7 negative probe', 1)
open(p,"w").write(s)
PYIN
expect_fail "$c" "ADR-0098 R7 regression" "CHECK 10L — opener+sync_mirror replanted in change_status is caught"

echo "[CHECK 10L] ROUND 6 — the SAME re-fork spelled with round 5's new name, sync_mirror_lockstep. 10L-a's seam token was name-keyed (a bare sync_mirror( ) and this walked straight through it"
c="$(fresh)"
python3 - "$c/apps/aberp/src/incoming_invoices.rs" <<'PYIN'
import sys
p=sys.argv[1]; s=open(p).read()
needle='    ensure_schema(&guard).context("ensure ap_invoice schema (status change)")?;'
assert needle in s, "change_status anchor moved — probe is stale"
s=s.replace(needle, needle+'\n    let _r6 = Ledger::open(db_path, tenant.clone(), binary_hash); let _m = aberp_audit_ledger::sync_mirror_lockstep(std::path::Path::new("/x")); // round-6 negative probe', 1)
open(p,"w").write(s)
PYIN
expect_fail "$c" "REGREW a direct sync_mirror" "CHECK 10L-a — the sync_mirror_lockstep SPELLING of the re-fork is caught (the seam token matches the sync_mirror PREFIX, not one name)"

echo "[CHECK 10L] a BRAND-NEW runtime fn with opener + sync_mirror (a new fork-capable site) must grow the frozen mirror-fork set — red"
c="$(fresh)"
printf '\nfn _r7_new_fork_site() {\n    let _c = duckdb::Connection::open("/x");\n    let _m = _c.map(|c| c.sync_mirror(std::path::Path::new("/y")));\n}\n' >> "$c/apps/aberp/src/quality.rs"
expect_fail "$c" "write-fork) site appeared" "CHECK 10L — a new independent-opener + sync_mirror site is caught by the frozen-set freeze"

echo "[CHECK 10L] an opener + sync_mirror inside a #[cfg(test)] fn must NOT trip 10L (cfg(test)-aware precision, no false-positive)"
c="$(fresh)"
printf '\n#[cfg(test)]\nmod r7_cfgtest_probe {\n    fn t() {\n        let _c = Ledger::open("/x");\n        let _m = _c.map(|l| l.sync_mirror(std::path::Path::new("/y")));\n    }\n}\n' >> "$c/apps/aberp/src/quality.rs"
expect_pass "$c" "CHECK 10L — an opener+sync_mirror inside #[cfg(test)] is correctly IGNORED (10L is cfg(test)-aware)"

echo "[CHECK 10M] a raw Ledger::open + append REPLANTED in the migrated snapshot.rs daemon+HTTP path — the ADR-0099 write-fork must go red (10M-a)"
c="$(fresh)"
printf '\nfn _adr0099_probe_snapshot_refork(p: &std::path::Path, t: aberp_audit_ledger::TenantId, bh: aberp_audit_ledger::BinaryHash) {\n    let mut l = aberp_audit_ledger::Ledger::open(p, t, bh).unwrap();\n    let _ = l.append(aberp_audit_ledger::EventKind::Test, vec![], todo!(), None);\n}\n' >> "$c/apps/aberp/src/snapshot.rs"
expect_fail "$c" "snapshot.rs REGREW an in-process write-fork" "CHECK 10M-a — independent Ledger::open+append replanted in the snapshot daemon path (the seq-515 fork) is caught"

echo "[CHECK 10M] a raw Ledger::open + append REPLANTED in a serve.rs request handler — the write-fork must go red (10M-a)"
c="$(fresh)"
printf '\nfn _adr0099_probe_serve_refork(p: &std::path::Path, t: aberp_audit_ledger::TenantId, bh: aberp_audit_ledger::BinaryHash) {\n    let mut l = Ledger::open(p, t, bh).unwrap();\n    let _ = l.append(EventKind::Test, vec![], todo!(), None);\n}\n' >> "$c/apps/aberp/src/serve.rs"
expect_fail "$c" "serve.rs REGREW an in-process write-fork" "CHECK 10M-a — independent Ledger::open+append replanted in a serve.rs request handler is caught"

echo "[CHECK 10M] a BRAND-NEW write-fork fn (independent opener + append) outside the frozen residual set — must grow the set → red (10M-b)"
c="$(fresh)"
printf 'fn _adr0099_probe_new_fork(p: &std::path::Path, t: aberp_audit_ledger::TenantId, bh: aberp_audit_ledger::BinaryHash) {\n    let mut l = aberp_audit_ledger::Ledger::open(p, t, bh).unwrap();\n    let _ = l.append(aberp_audit_ledger::EventKind::Test, vec![], todo!(), None);\n}\n' > "$c/apps/aberp/src/zz_adr0099_probe_fork.rs"
expect_fail "$c" "a NEW/REGROWN write-fork" "CHECK 10M-b — a new independent-opener + append site outside the frozen set is caught"

echo "[CHECK 10M] a write-fork inside a #[cfg(test)] fn must NOT trip 10M (cfg(test)-aware precision, no false-positive)"
c="$(fresh)"
printf '\n#[cfg(test)]\nmod adr0099_test_probe {\n    fn t(p: &std::path::Path, tn: aberp_audit_ledger::TenantId, bh: aberp_audit_ledger::BinaryHash) {\n        let mut l = aberp_audit_ledger::Ledger::open(p, tn, bh).unwrap();\n        let _ = l.append(aberp_audit_ledger::EventKind::Test, vec![], todo!(), None);\n    }\n}\n' >> "$c/apps/aberp/src/serve.rs"
expect_pass "$c" "CHECK 10M — a Ledger::open+append inside #[cfg(test)] is correctly IGNORED (10M is cfg(test)-aware)"


# ── CHECK 10N — ADR-0105 wrapper-hidden write-fork ────────────────────────────
# THE probe for this ADR: the exact pre-PR-33 aberp-mes shape — an independent
# opener in one fn, the audit append hidden one call away in a helper. CHECK 10M
# cannot see it (that is the whole point); CHECK 10N must.

echo "[CHECK 10N] the pre-PR-33 WRAPPER-HIDDEN fork (opener here, append one call away) — 10N must go red"
c="$(fresh)"
printf '\nfn _adr0105_probe_wrapper_fork(p: &std::path::Path, t: aberp_audit_ledger::TenantId, bh: aberp_audit_ledger::BinaryHash) {\n    let mut l = aberp_audit_ledger::Ledger::open(p, t, bh).unwrap();\n    _adr0105_probe_hidden_append(&mut l);\n}\nfn _adr0105_probe_hidden_append(l: &mut aberp_audit_ledger::Ledger) {\n    let _ = l.append(aberp_audit_ledger::EventKind::Test, vec![], todo!(), None);\n}\n' > "$c/apps/aberp/src/zz_adr0105_probe_wrapper.rs"
expect_fail "$c" "a NEW/REGROWN WRAPPER-HIDDEN write-fork" "CHECK 10N-b — an opener whose append hides ONE level down in a helper is caught"

echo "[CHECK 10N] the same fork hidden TWO wrapper levels down — the taint closure must still reach it"
c="$(fresh)"
printf '\nfn _adr0105_probe_deep_fork(p: &std::path::Path, t: aberp_audit_ledger::TenantId, bh: aberp_audit_ledger::BinaryHash) {\n    let mut l = aberp_audit_ledger::Ledger::open(p, t, bh).unwrap();\n    _adr0105_probe_mid(&mut l);\n}\nfn _adr0105_probe_mid(l: &mut aberp_audit_ledger::Ledger) {\n    _adr0105_probe_deep_append(l);\n}\nfn _adr0105_probe_deep_append(l: &mut aberp_audit_ledger::Ledger) {\n    let _ = l.append(aberp_audit_ledger::EventKind::Test, vec![], todo!(), None);\n}\n' > "$c/apps/aberp/src/zz_adr0105_probe_deep.rs"
expect_fail "$c" "a NEW/REGROWN WRAPPER-HIDDEN write-fork" "CHECK 10N-b — N-level (2-deep) wrapper indirection is caught by the taint closure"

# This one pins the BLIND SPOT itself. If a future change teaches CHECK 10M to
# see through wrappers, this probe flips to a HARNESS BUG report — which is
# correct: the blind spot 10N exists to cover would have closed, and that should
# be a deliberate, visible decision rather than a silent overlap.
echo "[CHECK 10N] the wrapper-hidden fork planted in snapshot.rs — 10M-a (ZERO-tolerance) does NOT see it; only 10N does"
c="$(fresh)"
printf '\nfn _adr0105_probe_blindspot(p: &std::path::Path, t: aberp_audit_ledger::TenantId, bh: aberp_audit_ledger::BinaryHash) {\n    let mut l = aberp_audit_ledger::Ledger::open(p, t, bh).unwrap();\n    _adr0105_probe_blindspot_append(&mut l);\n}\nfn _adr0105_probe_blindspot_append(l: &mut aberp_audit_ledger::Ledger) {\n    let _ = l.append(aberp_audit_ledger::EventKind::Test, vec![], todo!(), None);\n}\n' >> "$c/apps/aberp/src/snapshot.rs"
# Not this shard's probe (see SHARDING above) — the plant ran, the gate run did not.
if probe_skipped; then skipped=$((skipped+1));
elif ! assert_planted "$c"; then
  printf '  ✗ HARNESS BUG: CHECK 10N blind-spot probe — the plant modified NOTHING.\n'; bad=$((bad+1))
else
  rc="$(gate_rc "$c")"
  m10m="$(grep -c 'snapshot.rs REGREW an in-process write-fork' "$c/.out" || true)"
  m10n="$(grep -c 'WRAPPER-HIDDEN write-fork' "$c/.out" || true)"
  if [[ "$rc" != "0" && "$m10m" == "0" && "$m10n" != "0" ]]; then
    printf '  ✓ caught: CHECK 10N — wrapper-hidden fork in snapshot.rs caught by 10N while 10M-a stays blind (the documented gap)  (exit=%s)\n' "$rc"; pass=$((pass+1))
  elif [[ "$rc" != "0" && "$m10m" != "0" ]]; then
    printf '  ✗ HARNESS BUG: CHECK 10M-a now ALSO catches the wrapper-hidden fork — the ADR-0105 blind-spot premise no longer holds.\n'
    printf '        Re-verify whether CHECK 10N is still needed, then update this probe deliberately.\n'; bad=$((bad+1))
  else
    printf '  ✗ ESCAPED: CHECK 10N — wrapper-hidden fork in snapshot.rs was NOT caught (exit=%s)\n' "$rc"
    sed 's/^/        /' "$c/.out"; bad=$((bad+1))
  fi
fi

echo "[CHECK 10N] a wrapper-hidden fork inside #[cfg(test)] must NOT trip 10N (cfg(test)-aware precision, no false-positive)"
c="$(fresh)"
printf '\n#[cfg(test)]\nmod adr0105_test_probe {\n    fn t(p: &std::path::Path, tn: aberp_audit_ledger::TenantId, bh: aberp_audit_ledger::BinaryHash) {\n        let mut l = aberp_audit_ledger::Ledger::open(p, tn, bh).unwrap();\n        hidden(&mut l);\n    }\n    fn hidden(l: &mut aberp_audit_ledger::Ledger) {\n        let _ = l.append(aberp_audit_ledger::EventKind::Test, vec![], todo!(), None);\n    }\n}\n' >> "$c/apps/aberp/src/serve.rs"
expect_pass "$c" "CHECK 10N — a wrapper-hidden fork inside #[cfg(test)] is correctly IGNORED (10N is cfg(test)-aware)"

# Precision probe for the Handle barrier. NOTE the assertion shape: this canNOT
# be an `expect_pass`. Any plant that introduces a new opener necessarily trips
# CHECK 10i ("NEW unaccounted opener-bearing file") and CHECK 10k (the
# per-opener fingerprint freeze), so the gate is *expected* to fail here — just
# not for a 10N reason. The claim under test is therefore "10N stayed silent",
# which is an ABSENCE assertion, not a gate-exit assertion.
echo "[CHECK 10N] an opener whose helper routes through the shared Handle must NOT trip 10N (Handle barrier, no false-positive)"
c="$(fresh)"
printf '\nfn _adr0105_probe_handle_routed(p: &std::path::Path, t: aberp_audit_ledger::TenantId, bh: aberp_audit_ledger::BinaryHash, db: &aberp_db::HandleArc) {\n    let mut l = aberp_audit_ledger::Ledger::open(p, t, bh).unwrap();\n    _adr0105_probe_handle_append(db, &mut l);\n}\nfn _adr0105_probe_handle_append(db: &aberp_db::HandleArc, _l: &mut aberp_audit_ledger::Ledger) {\n    let mut g = db.write().unwrap();\n    let tx = g.transaction().unwrap();\n    let _ = aberp_audit_ledger::append_in_tx(&tx, todo!(), aberp_audit_ledger::EventKind::Test, vec![], todo!(), None);\n}\n' > "$c/apps/aberp/src/zz_adr0105_probe_handle.rs"
# Not this shard's probe (see SHARDING above) — the plant ran, the gate run did not.
if probe_skipped; then skipped=$((skipped+1));
elif ! assert_planted "$c"; then
  printf '  ✗ HARNESS BUG: CHECK 10N Handle-barrier probe — the plant modified NOTHING.\n'; bad=$((bad+1))
else
  rc="$(gate_rc "$c")"
  if grep -q 'WRAPPER-HIDDEN write-fork' "$c/.out"; then
    printf '  ✗ ESCAPED: CHECK 10N FALSE-POSITIVE — an append that takes the shared Handle itself was reported as a wrapper-hidden fork; the barrier rule is broken (every ADR-0099-migrated seam would fail).\n'
    grep 'WRAPPER-HIDDEN' -A 3 "$c/.out" | sed 's/^/        /'; bad=$((bad+1))
  elif grep -q 'NEW unaccounted opener-bearing file' "$c/.out"; then
    printf '  ✓ CHECK 10N — an append that takes the shared Handle itself is correctly IGNORED (10N silent; the gate still fails on the CHECK 10i/10k opener freeze, as designed)  (exit=%s)\n' "$rc"; pass=$((pass+1))
  else
    printf '  ✗ HARNESS BUG: CHECK 10N Handle-barrier probe — neither the 10N signature NOR the expected CHECK 10i opener-freeze failure appeared; the probe is no longer exercising what it claims (exit=%s).\n' "$rc"
    sed 's/^/        /' "$c/.out"; bad=$((bad+1))
  fi
fi


# ── ADR-0105 F1 — the `from_connection` LAUNDERING channel ────────────────────
# Found by the PR #34 adversarial. EVERY opener scanner in the tree used to skip
# any line containing the substring `from_connection`. The exclusion was
# LINE-scoped, not call-scoped, so a genuinely independent `Connection::open`
# hidden as an ARGUMENT on that same line became invisible to 10i / 10j / 10k /
# 10L / 10M / 10N at once:
#
#     let mut l = Ledger::from_connection(Connection::open(p)?, tid(), bh());
#     l.append(..);
#
# That is a DIRECT, same-fn write-fork. Severity was measured rather than
# assumed: planted in serve.rs the pre-fix gate still went red, because CHECK 10
# (the serve.rs-specific live-path opener scan) never carried the clause. But
# planted ANYWHERE ELSE — quality.rs, crates/aberp-qa, and snapshot.rs, which
# CHECK 10M-a holds at a hard ZERO — the pre-fix gate passed **in full**.
#
# So the probe plants in **snapshot.rs**: a zero-tolerance file where the bypass
# was total, not one that another check happened to cover. The exclusion was a
# proven no-op for its stated purpose (`Ledger::from_connection(` matches none
# of the opener regexes, so it never suppressed a real record) and was removed;
# this probe is what keeps it removed. It asserts the 10M-a zero-tolerance
# signature specifically, so a partial re-introduction that leaves only the
# opener freezes firing cannot pass unnoticed.
echo "[ADR-0105 F1] a Connection::open LAUNDERED through Ledger::from_connection on one line — must be caught, not skipped"
c="$(fresh)"
printf '\npub fn _adr0105_f1_laundered_fork(p: &std::path::Path) -> anyhow::Result<()> {\n    let mut l = Ledger::from_connection(Connection::open(p)?, tid(), bh());\n    l.append(EventKind::Test, Vec::new(), actor(), None)?;\n    Ok(())\n}\n' >> "$c/apps/aberp/src/snapshot.rs"
# Not this shard's probe (see SHARDING above) — the plant ran, the gate run did not.
if probe_skipped; then skipped=$((skipped+1));
elif ! assert_planted "$c"; then
  printf '  ✗ HARNESS BUG: ADR-0105 F1 probe — the plant modified NOTHING.\n'; bad=$((bad+1))
else
  rc="$(gate_rc "$c")"
  m_10m="$(grep -c 'snapshot.rs REGREW an in-process write-fork' "$c/.out" || true)"
  m_open="$(grep -c 'grew its residual openers\|opener fingerprint set DIVERGED' "$c/.out" || true)"
  if [[ "$rc" != "0" && "$m_10m" != "0" && "$m_open" != "0" ]]; then
    printf '  ✓ caught: ADR-0105 F1 — from_connection line-laundering trips 10M-a (zero-tolerance) AND the opener freeze (exit=%s)\n' "$rc"; pass=$((pass+1))
  elif [[ "$rc" == "0" ]]; then
    printf '  ✗ ESCAPED: ADR-0105 F1 — a DIRECT write-fork laundered through from_connection passed the WHOLE gate. The line-scoped exclusion is back; remove it from every opener scanner (see ADR-0105 §5 F1).\n'
    bad=$((bad+1))
  else
    printf '  ✗ ESCAPED (partial): ADR-0105 F1 — the gate failed but NOT on 10M-a (zero-tolerance=%s opener-freeze=%s). A subset of the scanners still skips the laundered line.\n' "$m_10m" "$m_open"
    sed 's/^/        /' "$c/.out"; bad=$((bad+1))
  fi
fi

# ── CHECK 10P — ADR-0099 R2 audit-writer provenance ──────────────────────────
# These probe the FOUR blind spots that let the class recur a fifth time. Two of
# them also assert that 10M/10N stay SILENT, because a probe that only proves
# "something went red" would not distinguish 10P from the checks it supplements.

echo "[CHECK 10P] B1 — a daemon heartbeat appending on a db.read() CLONE (no writer mutex, no AUDIT_APPEND_LOCK): 10P must go red where 10M/10N are structurally blind"
c="$(fresh)"
printf 'fn _adr0099r2_probe_read_clone_appender(db: &aberp_db::HandleArc) {\n    let mut conn = db.read().unwrap();\n    let tx = conn.transaction().unwrap();\n    let _ = aberp_audit_ledger::append_in_tx(&tx, todo!(), todo!(), vec![], todo!(), None);\n    tx.commit().unwrap();\n}\n' > "$c/apps/aberp/src/zz_adr0099r2_probe_read.rs"
# Not this shard's probe (see SHARDING above) — the plant ran, the gate run did not.
if probe_skipped; then skipped=$((skipped+1));
elif ! assert_planted "$c"; then
  printf '  ✗ HARNESS BUG: CHECK 10P B1 probe — the plant modified NOTHING.\n'; bad=$((bad+1))
else
  rc="$(gate_rc "$c")"
  p10p="$(grep -c 'NON-SHARED audit writer appeared outside the frozen residual' "$c/.out" || true)"
  p10mn="$(grep -c 'NEW/REGROWN write-fork\|WRAPPER-HIDDEN write-fork\|REGREW an in-process write-fork' "$c/.out" || true)"
  if [[ "$rc" != "0" && "$p10p" != "0" && "$p10mn" == "0" ]]; then
    printf '  ✓ caught: CHECK 10P — the read-clone appender is caught by 10P alone; 10M/10N stay blind (their opener set has no .read(), which is blind spot B1)  (exit=%s)\n' "$rc"; pass=$((pass+1))
  elif [[ "$rc" != "0" && "$p10p" != "0" ]]; then
    printf '  ✗ HARNESS BUG: CHECK 10P B1 — 10M/10N now ALSO catch the read-clone appender, so the B1 premise no longer holds. Re-verify the overlap and update this probe deliberately.\n'; bad=$((bad+1))
  else
    printf '  ✗ ESCAPED: CHECK 10P B1 — a second audit writer on a db.read() clone passed the gate (exit=%s). This is the seq-fork primitive with a different name.\n' "$rc"
    sed 's/^/        /' "$c/.out"; bad=$((bad+1))
  fi
fi

echo "[CHECK 10P] B4 — a non-shared MIRROR writer (Connection::open + ensure_consistent_with_db): 10L cannot see it (its append token is .sync_mirror only). NOT the seq-2508 mechanism — that was a lost DB commit (ADR-0099 R2.2); this is the second-writer class 10P exists for"
c="$(fresh)"
printf 'fn _adr0099r2_probe_mirror_writer(p: &std::path::Path) {\n    let conn = duckdb::Connection::open(p).unwrap();\n    let mp = aberp_audit_ledger::mirror_path_for(p);\n    let _ = aberp_audit_ledger::ensure_consistent_with_db(&conn, &mp);\n}\n' > "$c/apps/aberp/src/zz_adr0099r2_probe_mirror.rs"
# Not this shard's probe (see SHARDING above) — the plant ran, the gate run did not.
if probe_skipped; then skipped=$((skipped+1));
elif ! assert_planted "$c"; then
  printf '  ✗ HARNESS BUG: CHECK 10P B4 probe — the plant modified NOTHING.\n'; bad=$((bad+1))
else
  rc="$(gate_rc "$c")"
  p10p="$(grep -c 'NON-SHARED audit writer appeared outside the frozen residual' "$c/.out" || true)"
  p10l="$(grep -c 'write-fork) site appeared\|ADR-0098 R7 regression' "$c/.out" || true)"
  if [[ "$rc" != "0" && "$p10p" != "0" && "$p10l" == "0" ]]; then
    printf '  ✓ caught: CHECK 10P — the reconcile-shaped mirror writer is caught by 10P alone; CHECK 10L stays blind (blind spot B4)  (exit=%s)\n' "$rc"; pass=$((pass+1))
  elif [[ "$rc" != "0" && "$p10p" != "0" ]]; then
    printf '  ✗ HARNESS BUG: CHECK 10P B4 — CHECK 10L now ALSO catches the reconcile-shaped mirror writer. Re-verify the overlap and update this probe deliberately.\n'; bad=$((bad+1))
  else
    printf '  ✗ ESCAPED: CHECK 10P B4 — a mirror writer on its own connection passed the gate (exit=%s). The mirror is half the ledger; a forked mirror refuses the next boot.\n' "$rc"
    sed 's/^/        /' "$c/.out"; bad=$((bad+1))
  fi
fi

echo "[CHECK 10P] B4/scope — the same mirror writer planted inside crates/aberp-snapshot, which 10i/10k/10L all EXCLUDE from their corpus (where the real defect lived)"
c="$(fresh)"
printf 'pub fn _adr0099r2_probe_snapshot_scope(p: &std::path::Path) {\n    let conn = duckdb::Connection::open(p).unwrap();\n    let mp = aberp_audit_ledger::mirror_path_for(p);\n    let _ = aberp_audit_ledger::ensure_consistent_with_db(&conn, &mp);\n}\n' > "$c/crates/aberp-snapshot/src/zz_adr0099r2_probe_scope.rs"
expect_fail "$c" "NON-SHARED audit writer appeared outside the frozen residual" "CHECK 10P — a non-shared ledger writer inside crates/aberp-snapshot (outside every other check's corpus) is caught"

echo "[CHECK 10P] ROUND 6 — a db.read() clone whose MIRROR write uses round 5's new name, sync_mirror_lockstep. 10P's B4 token was name-keyed, so this second mirror writer produced NO record at all"
c="$(fresh)"
printf 'fn _adr0099r6_probe_lockstep_read_clone(db: &aberp_db::HandleArc, p: &std::path::Path) {\n    let conn = db.read().unwrap();\n    let mp = aberp_audit_ledger::mirror_path_for(p);\n    let _ = aberp_audit_ledger::sync_mirror_lockstep(&conn, todo!(), &mp);\n}\n' > "$c/apps/aberp/src/zz_adr0099r6_probe_lockstep.rs"
expect_fail "$c" "NON-SHARED audit writer appeared outside the frozen residual" "CHECK 10P — the sync_mirror_lockstep SPELLING of a mirror write is a ledger write (B4 stays closed across a rename)"

echo "[CHECK 10P] B2 — a SPLIT fork: the independent opener here, the append one call away behind a &mut Connection parameter (the qc_inspection shape). The taint fixpoint must classify the OPENER's fn."
c="$(fresh)"
printf 'fn _adr0099r2_probe_split_opener(p: &std::path::Path) {\n    let mut conn = duckdb::Connection::open(p).unwrap();\n    _adr0099r2_probe_split_append(&mut conn);\n}\nfn _adr0099r2_probe_split_append(conn: &mut duckdb::Connection) {\n    let tx = conn.transaction().unwrap();\n    let _ = aberp_audit_ledger::append_in_tx(&tx, todo!(), todo!(), vec![], todo!(), None);\n    tx.commit().unwrap();\n}\n' > "$c/apps/aberp/src/zz_adr0099r2_probe_split.rs"
expect_fail "$c" "NON-SHARED audit writer appeared outside the frozen residual" "CHECK 10P — a split (helper-parameter) fork is classified at the opener via the taint fixpoint"

echo "[CHECK 10P] the exact writer R2 removed — serve.rs's post-tx Ledger::open + sync_mirror best-effort helper — replanted must go red"
c="$(fresh)"
printf '\nfn _adr0099r2_probe_best_effort_mirror(db_path: &std::path::Path, tenant: aberp_audit_ledger::TenantId, binary_hash: aberp_audit_ledger::BinaryHash) {\n    let mirror_path = aberp_audit_ledger::mirror_path_for(db_path);\n    if let Ok(ledger) = Ledger::open(db_path, tenant, binary_hash) {\n        let _ = ledger.sync_mirror(&mirror_path);\n    }\n}\n' >> "$c/apps/aberp/src/serve.rs"
expect_fail "$c" "NON-SHARED audit writer appeared outside the frozen residual" "CHECK 10P — the removed serve.rs second mirror writer, replanted, is caught (an IMMUTABLE Ledger binding still writes the mirror)"

echo "[CHECK 10P] NON-TRIGGER — a db.read() clone used ONLY for verify_chain must stay GREEN (this is the shape ~7 money paths use; a gate that reddens it would be switched off)"
c="$(fresh)"
printf '\nfn _adr0099r2_probe_verify_only(db: &aberp_db::HandleArc, t: aberp_audit_ledger::TenantId, bh: aberp_audit_ledger::BinaryHash) {\n    let mut conn = db.write().unwrap();\n    let tx = conn.transaction().unwrap();\n    let _ = aberp_audit_ledger::append_in_tx(&tx, todo!(), todo!(), vec![], todo!(), None);\n    tx.commit().unwrap();\n    drop(conn);\n    let verify_conn = db.read().unwrap();\n    let ledger = Ledger::from_connection(verify_conn, t, bh);\n    let _ = ledger.verify_chain();\n}\n' >> "$c/apps/aberp/src/serve.rs"
expect_pass "$c" "CHECK 10P — write-then-verify-on-a-read-clone is correctly GREEN (provenance travels only along connection derivations, so a read clone that never writes is not a fork)"

echo "[CHECK 10P] NON-TRIGGER — a read-clone appender inside #[cfg(test)] must NOT trip 10P (cfg(test)-aware precision)"
c="$(fresh)"
printf '\n#[cfg(test)]\nmod adr0099r2_test_probe {\n    fn t(db: &aberp_db::HandleArc) {\n        let mut conn = db.read().unwrap();\n        let tx = conn.transaction().unwrap();\n        let _ = aberp_audit_ledger::append_in_tx(&tx, todo!(), todo!(), vec![], todo!(), None);\n    }\n}\n' >> "$c/apps/aberp/src/serve.rs"
expect_pass "$c" "CHECK 10P — a read-clone appender inside #[cfg(test)] is correctly IGNORED"

echo "[CHECK 10P] HARNESS — deleting the scanner must NOT read as \"no violations\"; the gate must say so"
c="$(fresh)"
rm -f "$c/tools/adr0099_audit_writer_scan.awk"
expect_fail "$c" "audit-writer scanner or frozen residual missing" "CHECK 10P — a deleted scanner is RED, not vacuously green"

echo "[CHECK 10P] HARNESS — breaking the scanner's shared-Handle verdict must be RED (corpus liveness), not a silent green"
c="$(fresh)"
# Neuter only the HANDLE_WRITE recognition. Every real writer then falls to
# UNCLASSIFIED, which must trip 10P-2; and the 10P-3 corpus-liveness floor is
# what makes the OPPOSITE mutation (a scanner that emits nothing at all) red too.
perl -0pi -e 's/if \(stmt ~ \/\\\.write\[ \\t\]\*\\\(\[ \\t\]\*\\\)\/\)      src="W"/if (0) src="W"/' "$c/tools/adr0099_audit_writer_scan.awk"
expect_fail "$c" "NON-SHARED audit writer appeared outside the frozen residual" "CHECK 10P — a scanner that stops recognising the shared writer goes RED instead of reporting a clean tree"


# ── ADR-0116 D2 — the recovery-evidence guard (CHECK 11) ────────────────
#
# The class these probes pin is permanent data loss: an unlink beside the live
# DB destroys the ONLY record of a durability incident. Prod holds ~330 MB of
# such artefacts in the tenant homes and ~271 MB outside them, and before this
# guard the pruner's protection of them was a structural accident.

echo "[CHECK 11] the ADR-0116 hazard, exactly: a new 'clean up the tenant home' helper that enumerates the home and unlinks by prefix, with no guard"
c="$(fresh)"
printf '\npub fn _adr0116_probe_tidy_tenant_home(db_path: &std::path::Path) {\n    let parent = db_path.parent().unwrap();\n    for entry in std::fs::read_dir(parent).unwrap().flatten() {\n        let _ = std::fs::remove_file(entry.path());\n    }\n}\n' >> "$c/apps/aberp/src/snapshot.rs"
expect_fail "$c" "NEW UNGUARDED tenant-home removal appeared" "CHECK 11 — an unguarded tenant-home sweeper is caught (this is recover::cleanup_siblings_with_infix's exact shape, which shipped unguarded)"

echo "[CHECK 11] the same helper INSIDE crates/aberp-snapshot, where the evidence actually lives and where 10i/10k/10L all exclude the corpus"
c="$(fresh)"
printf 'pub fn _adr0116_probe_scope(db_path: &std::path::Path) {\n    let wal = db_path.with_extension("wal");\n    let _ = std::fs::remove_file(&wal);\n}\n' > "$c/crates/aberp-snapshot/src/zz_adr0116_probe_scope.rs"
expect_fail "$c" "NEW UNGUARDED tenant-home removal appeared" "CHECK 11 — a new unguarded removal inside aberp-snapshot (outside every opener check's corpus) is caught"

echo "[CHECK 11] NON-TRIGGER — the SAME helper routed through guarded_remove must be GREEN (a gate that reddens the correct fix would be switched off)"
c="$(fresh)"
printf '\npub fn _adr0116_probe_guarded(db_path: &std::path::Path) {\n    let parent = db_path.parent().unwrap();\n    for entry in std::fs::read_dir(parent).unwrap().flatten() {\n        let _ = aberp_snapshot::guarded_remove(&entry.path());\n        let _ = std::fs::remove_file(entry.path());\n    }\n}\n' >> "$c/apps/aberp/src/snapshot.rs"
expect_pass "$c" "CHECK 11 — a tenant-home removal that consults the guard is correctly GREEN"

echo "[CHECK 11] deleting the shared guard must be RED (it is the predicate every tenant-home helper calls)"
c="$(fresh)"
rm -f "$c/crates/aberp-snapshot/src/evidence.rs"
expect_fail "$c" "ADR-0116 D2 evidence guard missing" "CHECK 11 — a deleted evidence.rs is RED, not vacuously green"

echo "[CHECK 11] F3's REAL defect: making the guard case-SENSITIVE again. 58 of 101 on-disk artefacts escape that, and this bug class was already closed once in this repo's edition DB-guard"
c="$(fresh)"
perl -0pi -e 's/to_ascii_lowercase/to_ascii_uppercase_NOT/g' "$c/crates/aberp-snapshot/src/evidence.rs"
expect_fail "$c" "does not lowercase before matching" "CHECK 11 — a case-SENSITIVE evidence guard is RED"

echo "[CHECK 11] F3's other half: reverting the allow-list INVERSION to a deny-list. 14 artefacts escape even case-insensitively — every healed-*.bak and the sole 2026-08-03 INDEXDESYNC backup"
c="$(fresh)"
perl -0pi -e 's/return !name_is_live\(name\);/return false;/' "$c/crates/aberp-snapshot/src/evidence.rs"
expect_fail "$c" "has lost the allow-list INVERSION" "CHECK 11 — a deny-list-only guard is RED"

echo "[CHECK 11] removing prune's consultation of the guard (D2.3's belt half) must be RED"
c="$(fresh)"
perl -0pi -e 's/crate::evidence::is_protected_evidence\(&rec\.dir\)/false/' "$c/crates/aberp-snapshot/src/retention.rs"
expect_fail "$c" "retention::prune does not CONSULT is_protected_evidence" "CHECK 11 — a pruner that no longer consults the guard is RED (its blindness must be a deliberate refusal, not an accident). NOTE the signature is the SCANNER-verdict wording: the first cut of CHECK 11 asserted this with a bare grep, which its own DOC COMMENT satisfied, and this probe ESCAPED"

echo "[CHECK 11] F7/M1 — the guard left in place but SHORT-CIRCUITED DEAD. This mutation passed the WHOLE gate before rev 2"
c="$(fresh)"
perl -0pi -e 's/if crate::evidence::is_protected_evidence\(&rec\.dir\)/if false && crate::evidence::is_protected_evidence(&rec.dir)/' "$c/crates/aberp-snapshot/src/retention.rs"
expect_fail "$c" "guard is PRESENT but DEAD" "CHECK 11 — a guard neutered with \`false &&\` is RED. The previous cut asserted the SCANNER's verdict rather than a bare grep, which closed the flip-by-editing-a-COMMENT escape — but the verdict was still token PRESENCE in the fn body, so the guard could still be flipped by editing an OPERATOR. That is the ADR-0098 opener-scan char-literal class one level in"

echo "[CHECK 11] F7/M1 variant — the same mutation as rustfmt would split it across lines"
c="$(fresh)"
perl -0pi -e 's/if crate::evidence::is_protected_evidence\(&rec\.dir\) \{/if false\n            && crate::evidence::is_protected_evidence(&rec.dir)\n        {/' "$c/crates/aberp-snapshot/src/retention.rs"
expect_fail "$c" "guard is PRESENT but DEAD" "CHECK 11 — a short-circuited guard is caught whether or not it fits on one line (the scanner evaluates a STATEMENT, not a line)"

echo "[CHECK 11] F7 — the guard called for its side effects only: \`let _ = is_protected_evidence(..)\`"
c="$(fresh)"
perl -0pi -e 's/if crate::evidence::is_protected_evidence\(&rec\.dir\) \{/let _ = crate::evidence::is_protected_evidence(&rec.dir);\n        if false {/' "$c/crates/aberp-snapshot/src/retention.rs"
expect_fail "$c" "guard is PRESENT but DEAD" "CHECK 11 — a guard whose ANSWER is discarded is RED"

echo "[CHECK 11] F8/M5 — an unguarded tenant-home sweeper spelled through a DIRECT IMPORT (\`use std::fs::remove_file;\`). Nothing in-tree uses this spelling, which is exactly why it must be pinned"
c="$(fresh)"
printf 'use std::fs::remove_file;\n\npub fn _adr0116_probe_bare_spelling(tenant_home: &std::path::Path) {\n    for entry in std::fs::read_dir(tenant_home).unwrap().flatten() {\n        let _ = remove_file(entry.path());\n    }\n}\n' > "$c/crates/aberp-snapshot/src/zz_adr0116_probe_bare.rs"
expect_fail "$c" "NEW UNGUARDED tenant-home removal appeared" "CHECK 11 — the removal matcher is not keyed to ONE spelling. A gate that bans a single spelling is the class already on record here from PR #41"

echo "[CHECK 11] F8 — dropping \`archive_then_remove\` out of the frozen set must be RED (it is the one fn whose JOB is unlinking evidence, and the token-based classifier called it OTHER)"
c="$(fresh)"
perl -0pi -e 's{^crates/aberp-snapshot/src/evidence\.rs:archive_then_remove\n}{}m' "$c/tools/adr0116_tenant_home_removal_sites.txt"
expect_fail "$c" "NEW UNGUARDED tenant-home removal appeared" "CHECK 11 — the sanctioned evidence-release path is INSIDE the may-only-shrink freeze, so a change that weakens it is visible"

echo "[CHECK 11] HARNESS — DEAD_GUARD must not fire on the PREDICATE rule applied to the guarded ACTION (a gate that reddens correct code gets switched off)"
c="$(fresh)"
perl -0pi -e 's/npd = split\("is_protected_evidence", PRED, "\|"\)/npd = split("is_protected_evidence|guarded_remove", PRED, "|")/' "$c/tools/adr0116_evidence_removal_scan.awk"
expect_fail "$c" "must apply to the PREDICATE" "CHECK 11 — widening the discarded-result rule to \`guarded_remove\` is RED. \`let _ = guarded_remove(..)\` is idiomatic and safe (recover::cleanup_siblings_with_infix spells it that way); only the PREDICATE's discarded answer neuters a guard"

echo "[CHECK 11] HARNESS — a scanner that stops detecting a DEAD guard must be RED, not a silent green"
c="$(fresh)"
perl -0pi -e 's/if \(st ~ \/\(\^\|\[\^A-Za-z_0-9\]\)false\[ \\t\]\*&&\/\) return 1/return 0/' "$c/tools/adr0116_evidence_removal_scan.awk"
expect_fail "$c" "no longer detects a SHORT-CIRCUITED guard" "CHECK 11 — the DEAD_GUARD matcher has its own liveness fixture, so it cannot quietly stop matching (the 10P-0 pattern)"

echo "[CHECK 11] HARNESS — a scanner that stops seeing the bare removal spelling must be RED"
c="$(fresh)"
perl -0pi -e 's/\[\^A-Za-z_0-9\.\]\)remove_/])fs_NEVER_MATCHES_remove_/' "$c/tools/adr0116_evidence_removal_scan.awk"
expect_fail "$c" "no longer sees a removal spelled through a direct import" "CHECK 11 — the widened removal matcher has its own liveness fixture"

echo "[CHECK 11] HARNESS — a scanner that stops recognising the guard must be RED (liveness), not a silent green"
c="$(fresh)"
perl -0pi -e 's/ng  = split\("is_protected_evidence\|guarded_remove", GUARD, "\|"\)/ng = 0/' "$c/tools/adr0116_evidence_removal_scan.awk"
expect_fail "$c" "classified ZERO removals as GUARDED" "CHECK 11 — a scanner blind to the guard goes RED instead of reporting a clean tree"

echo "[CHECK 11] HARNESS — deleting the scanner must NOT read as \"no violations\""
c="$(fresh)"
rm -f "$c/tools/adr0116_evidence_removal_scan.awk"
expect_fail "$c" "evidence removal scanner or frozen manifest missing" "CHECK 11 — a deleted scanner is RED, not vacuously green"

echo
# ── shard accounting — the teeth of the sharding itself ──────────────────────
# Sharding is only sound if the shards PARTITION the suite: every probe in
# exactly one shard, none in zero. The two ways that silently breaks are an
# accounting site that was never made shard-aware (its probe then runs in EVERY
# shard — wasteful but harmless) and a probe skipped by all of them (ZERO
# coverage, and the suite still prints green). Both are coverage bugs wearing a
# performance-fix costume, which is exactly what this file exists to refuse.
#
# So the expected workload is recomputed from the FROZEN EXPECTED_PROBES rather
# than from anything this run accumulated, and compared three ways.
ran=$((pass+bad))
total="$(cat "$PROBE_CTR")"
expect_ran=0
for ((n=1; n<=EXPECTED_PROBES; n++)); do
  (( (n - 1) % PROBE_SHARD_TOTAL == PROBE_SHARD_INDEX - 1 )) && expect_ran=$((expect_ran+1))
done
echo "shard $PROBE_SHARD_INDEX/$PROBE_SHARD_TOTAL — probes run here: $ran   left to other shards: $skipped   suite total: $total (frozen: $EXPECTED_PROBES)"
echo "probes passed: $pass   broken/escaped: $bad"

if [[ "$total" -ne "$EXPECTED_PROBES" ]]; then
  echo "NEGATIVE-PROBES: ✗ FAILED — the suite ran $total probes but EXPECTED_PROBES=$EXPECTED_PROBES."
  echo "  A probe was added or removed, or a fresh() lost its accounting site. The shard partition is"
  echo "  derived from this frozen count, so it must move DELIBERATELY: bump EXPECTED_PROBES and re-check"
  echo "  cut-gate.yml's shard matrix. A drifting count is how a shard silently stops covering a probe."
  exit 1
fi
if [[ "$((ran + skipped))" -ne "$EXPECTED_PROBES" ]]; then
  echo "NEGATIVE-PROBES: ✗ FAILED — $ran run + $skipped skipped = $((ran + skipped)), not $EXPECTED_PROBES."
  echo "  Every probe must be either run here or explicitly left to another shard; one that is neither is"
  echo "  a probe that reports nothing anywhere."
  exit 1
fi
if [[ "$ran" -ne "$expect_ran" ]]; then
  echo "NEGATIVE-PROBES: ✗ FAILED — shard $PROBE_SHARD_INDEX/$PROBE_SHARD_TOTAL accounted for $ran probes but its"
  echo "  round-robin share of $EXPECTED_PROBES is exactly $expect_ran. Either an accounting site is not"
  echo "  shard-aware (its probe runs in every shard) or a probe is being skipped by all of them (zero"
  echo "  coverage). Both change what the suite covers; neither is a timing problem."
  exit 1
fi

if [[ "$bad" -ne 0 ]]; then echo "NEGATIVE-PROBES: ✗ FAILED"; exit 1; fi
if (( PROBE_SHARD_TOTAL > 1 )); then
  echo "NEGATIVE-PROBES: ✓ SHARD $PROBE_SHARD_INDEX/$PROBE_SHARD_TOTAL HAS TEETH ($ran probes; the suite is green only when every shard is)"
else
  echo "NEGATIVE-PROBES: ✓ ALL CHECKS HAVE TEETH"
fi
