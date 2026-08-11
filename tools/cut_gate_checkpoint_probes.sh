#!/usr/bin/env bash
#
# cut_gate_checkpoint_probes.sh — teeth for the ADR-0111 checkpoint-site gate.
#
# A negative probe that cannot fail is decoration. Each probe plants ONE
# regression into a throwaway copy of the tree and asserts the gate goes RED,
# then pins the non-triggers that would otherwise make the gate cry wolf.
#
# The probe set is chosen from the ways this specific defect comes BACK — not
# from the ways source can be edited. Each of P1–P7 is a plausible one-line
# refactor that restores mirror-ahead-of-DB in whole or in part.
#
# P0  the unmutated tree                                            -> GREEN
# P1  a daemon calls the path-based primitive again (the original)  -> RED
# P2  checkpoint_now stops QUIESCING (keeps the lock + the reopen)   -> RED
# P3  checkpoint_now gated on checkpoint_enabled                     -> RED
# P4  checkpoint_now stops taking the writer lock                    -> RED
# P5  a censused checkpoint route loses its call (count drift)       -> RED
# P6  a new UNCENSUSED checkpoint_now() site appears                 -> RED
# P7  the inode fence logs but falls through into the hooks          -> RED
# P8  the fence is removed entirely                                  -> RED
# P9  a doc MENTION of durable_checkpoint alone                      -> GREEN (not a call)
# P10 a COMMENTED-OUT path-based call                                -> GREEN (not a call)
# P11 the sanctioned run_durable_checkpoint_locked(..) wrapper call  -> GREEN (name collision)
# P12 a de-gated run on a broken tree                                -> GREEN, loudly
#
# P9–P11 matter as much as P1–P8: this gate bans a name that legitimately
# appears in doc comments all over the tree and is a PREFIX of the sanctioned
# wrapper's name. A matcher that cannot tell those apart gets switched off.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
GATE="tools/cut_gate_checkpoint_sites.sh"
CENSUS="tools/adr0111_checkpoint_sites.txt"
fail=0
pass() { printf '  ✓ %s\n' "$*"; }
bad()  { printf '  ✗ %s\n' "$*"; fail=1; }

echo "ADR-0111 checkpoint-site gate — negative probes"

# A throwaway copy of just what the gate reads: the gate, the census, and the
# production sources it scans. Never the whole tree — `target/` alone is tens of
# gigabytes and copying it once wedged an unrelated co-running gate.
mktree() {
  local d; d="$(mktemp -d "${TMPDIR:-/tmp}/adr0111-probes.XXXXXX")"
  mkdir -p "$d/tools" "$d/apps/aberp/src" "$d/crates/aberp-db/src" "$d/crates/aberp-snapshot/src"
  cp "$ROOT/$GATE" "$d/tools/" && cp "$ROOT/$CENSUS" "$d/tools/"
  cp "$ROOT"/apps/aberp/src/*.rs "$d/apps/aberp/src/"
  cp "$ROOT"/crates/aberp-db/src/*.rs "$d/crates/aberp-db/src/"
  cp "$ROOT"/crates/aberp-snapshot/src/*.rs "$d/crates/aberp-snapshot/src/"
  printf '%s' "$d"
}

run_gate() { ( cd "$1" && bash "$GATE" >"$1/out.txt" 2>&1; echo $? ); }

probe() { # name expected_exit mutator
  local name="$1" want="$2" mut="$3" d rc
  d="$(mktree)"
  ( cd "$d" && eval "$mut" )
  rc="$(run_gate "$d")"
  if [[ "$rc" == "$want" ]]; then
    pass "$name (exit $rc as expected)"
  else
    bad "$name — expected exit $want, got $rc"
    sed 's/^/      /' "$d/out.txt"
  fi
  rm -rf "$d"
}

probe "P0 unmutated tree stays GREEN" 0 "true"

# ── P1 — THE regression. Exactly what snapshot.rs and live_checkpoint.rs did.
probe "P1 a daemon calls the path-based primitive again -> RED" 1 \
  "printf 'fn tick(p: \&Path, t: \&str) { let _ = aberp_snapshot::live_durable_checkpoint(p, t); }\n' >> apps/aberp/src/live_checkpoint.rs"

# ── P2 — the subtle one. Every call site still routes through the handle and
# the gate's C-A/C-C both stay green; only the quiesce is gone, which is the
# whole mechanism. Without CHECK C-B this is a green gate over the live defect.
# Anchored on the run_durable_checkpoint_locked body so it cannot accidentally
# delete the identically-spelled line in recover_from_poison instead — an
# earlier cut of this probe did exactly that and reported a false ESCAPE.
probe "P2 checkpoint_now stops QUIESCING the shared connection -> RED" 1 \
  "perl -0pi -e 's/(fn run_durable_checkpoint_locked.*?)\n        inner\.conn = None;\n/\$1\n/s' crates/aberp-db/src/lib.rs"

probe "P3 checkpoint_now gated on checkpoint_enabled -> RED" 1 \
  "perl -0pi -e 's/(pub fn checkpoint_now\(&self\) \{)/\$1\n        if !self.config.checkpoint_enabled { return; }/' crates/aberp-db/src/lib.rs"

probe "P4 checkpoint_now stops taking the writer lock -> RED" 1 \
  "perl -0pi -e 's/(pub fn checkpoint_now\(&self\) \{)(.*?)lock_recovering/\$1\$2self.inner.lock/s' crates/aberp-db/src/lib.rs"

probe "P5 a censused checkpoint route loses its call (count drift) -> RED" 1 \
  "perl -0pi -e 's/^pub fn live_checkpoint_logged\(db: &HandleArc\) \{\n    db\.checkpoint_now\(\);\n\}/pub fn live_checkpoint_logged(db: \&HandleArc) { let _ = db; }/m' apps/aberp/src/snapshot.rs"

probe "P6 a new UNCENSUSED checkpoint_now() site -> RED" 1 \
  "printf 'fn sneaky(db: \&HandleArc) { db.checkpoint_now(); }\n' >> apps/aberp/src/products.rs"

probe "P7 the inode fence logs but FALLS THROUGH into the hooks -> RED" 1 \
  "perl -0pi -e 's/^                self\.inner\.fence = None;\n                return;\n/                self.inner.fence = None;\n/m' crates/aberp-db/src/lib.rs"

probe "P8 the inode fence is removed entirely -> RED" 1 \
  "perl -0pi -e 's/        let live_now = file_id\(&handle\.db_path\);\n.*?\n        \}\n\n        \/\/ D3 B2/        \/\/ D3 B2/s' crates/aberp-db/src/lib.rs"

# ── P9–P11 — the non-triggers. A gate banning a name that appears in dozens of
# doc comments, is the primitive's own definition, and is a strict prefix of the
# SANCTIONED wrapper must not fire on any of them.
probe "P9 a doc-comment MENTION is not a call -> GREEN" 0 \
  "printf '/// See [\`durable_checkpoint\`] and live_durable_checkpoint for the swap protocol.\n' >> apps/aberp/src/products.rs"

probe "P10 a COMMENTED-OUT path-based call is not a call -> GREEN" 0 \
  "printf '    // aberp_snapshot::durable_checkpoint(\&db, \&tenant);\n' >> apps/aberp/src/products.rs"

probe "P11 the sanctioned run_durable_checkpoint_locked(..) call is not a violation -> GREEN" 0 \
  "printf 'fn f(s: \&S, i: \&mut Inner) { s.run_durable_checkpoint_locked(i); }\n' >> apps/aberp/src/products.rs"

# ── P12 — a de-gated run must still ANNOUNCE itself, so "green" can never be
# read as "clean" when enforcement was switched off.
d="$(mktree)"
( cd "$d" && printf 'fn tick(p: &Path, t: &str) { let _ = aberp_snapshot::durable_checkpoint(p, t); }\n' >> apps/aberp/src/live_checkpoint.rs )
rc="$( cd "$d" && ENFORCE_CHECKPOINT_SITES=0 bash "$GATE" >"$d/out.txt" 2>&1; echo $? )"
if [[ "$rc" == "0" ]] && grep -q 'enforcement disabled' "$d/out.txt"; then
  pass "P12 de-gated run on a broken tree is GREEN and says so"
else
  bad "P12 de-gated run did not announce itself (exit $rc)"
  sed 's/^/      /' "$d/out.txt"
fi
rm -rf "$d"

echo
if [[ "$fail" -ne 0 ]]; then echo "PROBES: ✗ FAILED (the gate does not have the teeth it claims)"; exit 1; fi
echo "PROBES: ✓ PASSED"
