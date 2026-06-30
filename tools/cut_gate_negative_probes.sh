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
WORK="$(mktemp -d "${TMPDIR:-/tmp}/cutgate-probes.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT
pass=0; bad=0
i=0

fresh() {  # -> path to a fresh, clean copy of the tree (excludes .git)
  # NOTE (ADR-0098 Session C): use mktemp -d for a UNIQUE dir per call. The
  # prior `i=$((i+1)); d="$WORK/copy.$i"` form incremented `i` inside this
  # function's command-substitution subshell (`c="$(fresh)"`), so the counter
  # never persisted in the parent — every copy collided on copy.1 and
  # ACCUMULATED each probe's planted violation. Harmless for the expect_fail
  # probes (the gate fails regardless), but it made any expect_pass probe after
  # the first plant spuriously fail. Unique dirs fix it for good.
  local d; d="$(mktemp -d "$WORK/copy.XXXXXX")"
  tar -C "$ROOT" --exclude=.git -cf - . | tar -C "$d" -xf -
  printf '%s' "$d"
}
gate_rc() {  # run the COPY's gate; echo exit code; stash output in $1/.out
  ( cd "$1" && bash "$GATE" ) >"$1/.out" 2>&1
  echo $?
}

expect_pass() {  # $1 dir  $2 label
  local rc; rc="$(gate_rc "$1")"
  if [[ "$rc" == "0" ]]; then
    printf '  ✓ %s\n' "$2"; pass=$((pass+1))
  else
    printf '  ✗ BROKEN: %s — clean copy should PASS but gate exit=%s\n' "$2" "$rc"
    sed 's/^/        /' "$1/.out"; bad=$((bad+1))
  fi
}
expect_fail() {  # $1 dir  $2 signature  $3 label
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
sed -i 's#std::fs::rename(staged, target)#std::fs::copy(staged, target).map(|_| ())#' \
    "$c/crates/aberp-snapshot/src/crash_safe.rs"
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

echo
echo "probes passed: $pass   broken/escaped: $bad"
if [[ "$bad" -ne 0 ]]; then echo "NEGATIVE-PROBES: ✗ FAILED"; exit 1; fi
echo "NEGATIVE-PROBES: ✓ ALL CHECKS HAVE TEETH"
