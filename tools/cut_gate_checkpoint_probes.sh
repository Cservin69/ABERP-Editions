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
# P13-P15b the three adversarial bypasses of the first, name-keyed C-A  -> RED
# P16/P17  census drift up/down on the LEGIT pre-Handle boot callers      -> RED
# P18      a census entry naming a symbol outside the family              -> RED
#
# P9–P11 matter as much as P1–P8: this gate bans a name that legitimately
# appears in doc comments all over the tree and is a PREFIX of the sanctioned
# wrapper's name. A matcher that cannot tell those apart gets switched off.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
GATE="tools/cut_gate_checkpoint_sites.sh"
CENSUS="tools/adr0111_checkpoint_sites.txt"
FAMILY_CENSUS="tools/adr0111_rename_family_sites.txt"
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
  cp "$ROOT/$GATE" "$d/tools/" && cp "$ROOT/$CENSUS" "$d/tools/" \
    && cp "$ROOT/$FAMILY_CENSUS" "$d/tools/"
  cp "$ROOT"/apps/aberp/src/*.rs "$d/apps/aberp/src/"
  cp "$ROOT"/crates/aberp-db/src/*.rs "$d/crates/aberp-db/src/"
  cp "$ROOT"/crates/aberp-snapshot/src/*.rs "$d/crates/aberp-snapshot/src/"
  printf '%s' "$d"
}

run_gate() { ( cd "$1" && bash "$GATE" >"$1/out.txt" 2>&1; echo $? ); }

# Fingerprint of everything the gate reads, so a mutation that silently failed
# to apply can be told apart from a gate that failed to catch it.
tree_sum() { find "$1" -name '*.rs' -o -name '*.txt' | sort | xargs cat 2>/dev/null | shasum | cut -d' ' -f1; }

probe() { # name expected_exit mutator [nochange]
  local name="$1" want="$2" mut="$3" nochange="${4:-}" d rc before after
  d="$(mktree)"
  before="$(tree_sum "$d")"
  ( cd "$d" && eval "$mut" )
  after="$(tree_sum "$d")"

  # A STALE MUTATION IS THE FAILURE MODE THAT LIES. Every one of these probes
  # is a `perl -0pi -e s/.../` anchored on a source shape; refactor the shape
  # and the substitution silently matches nothing, the tree is unmutated, the
  # gate correctly says PASSED — and the probe reports "the gate has no teeth"
  # (or, worse in an earlier form, quietly agreed). Five of these went stale at
  # once when `checkpoint_now` gained a return type and the fence moved into a
  # helper. So: prove the mutation LANDED before believing anything about the
  # gate's verdict.
  if [[ -z "$nochange" && "$before" == "$after" ]]; then
    bad "$name — PROBE BUG: the mutation changed nothing (its anchor no longer matches the source)."
    printf '      Fix the probe, not the gate: re-anchor the substitution on the current shape.\n'
    rm -rf "$d"; return
  fi

  rc="$(run_gate "$d")"
  if [[ "$rc" == "$want" ]]; then
    pass "$name (exit $rc as expected)"
  else
    bad "$name — ESCAPED: mutation applied, expected exit $want, got $rc"
    sed 's/^/      /' "$d/out.txt"
  fi
  rm -rf "$d"
}

probe "P0 unmutated tree stays GREEN" 0 "true" nochange

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
  "perl -0pi -e 's/(pub fn checkpoint_now\(&self\)[^{]*\{)/\$1\n        if !self.config.checkpoint_enabled { return false; }/' crates/aberp-db/src/lib.rs"

probe "P4 checkpoint_now stops taking the writer lock -> RED" 1 \
  "perl -0pi -e 's/(pub fn checkpoint_now\(&self\)[^{]*\{)(.*?)lock_recovering/\$1\$2self.inner.lock/s' crates/aberp-db/src/lib.rs"

probe "P5 a censused checkpoint route loses its call (count drift) -> RED" 1 \
  "perl -0pi -e 's/if db\.checkpoint_now\(\) \{/if false \{/' apps/aberp/src/snapshot.rs"

probe "P6 a new UNCENSUSED checkpoint_now() site -> RED" 1 \
  "printf 'fn sneaky(db: \&HandleArc) { db.checkpoint_now(); }\n' >> apps/aberp/src/products.rs"

probe "P7 the inode fence logs but FALLS THROUGH into the hooks -> RED" 1 \
  "perl -0pi -e 's/(self\.inner\.fence = None;)\n            return;\n/\$1\n/' crates/aberp-db/src/lib.rs"

probe "P8 the inode fence is removed entirely -> RED" 1 \
  "perl -0pi -e 's/        if handle\.live_file_swapped\(&self\.inner\) \{.*?\n        \}\n\n        \/\/ D3 B2/        \/\/ D3 B2/s' crates/aberp-db/src/lib.rs"

# ── P9–P11 — the non-triggers. A gate banning a name that appears in dozens of
# doc comments, is the primitive's own definition, and is a strict prefix of the
# SANCTIONED wrapper must not fire on any of them.
probe "P9 a doc-comment MENTION is not a call -> GREEN" 0 \
  "printf '/// See [\`durable_checkpoint\`] and live_durable_checkpoint for the swap protocol.\n' >> apps/aberp/src/products.rs"

probe "P10 a COMMENTED-OUT path-based call is not a call -> GREEN" 0 \
  "printf '    // aberp_snapshot::durable_checkpoint(\&db, \&tenant);\n' >> apps/aberp/src/products.rs"

probe "P11 the sanctioned run_durable_checkpoint_locked(..) call is not a violation -> GREEN" 0 \
  "printf 'fn f(s: \&S, i: \&mut Inner) { s.run_durable_checkpoint_locked(i); }\n' >> apps/aberp/src/products.rs"

# ── P13–P17 — THE THREE ADVERSARIAL BYPASSES of the first, name-keyed C-A.
# Each of these compiled clean and left the gate saying PASSED. They are the
# reason C-A now counts TOUCHES of the whole rename family instead of calls of
# one name. If any of these three ever goes green again, the gate is decorative.
probe "P13 BYPASS (a): aliased import of the banned name -> RED" 1 \
  "printf 'use aberp_snapshot::live_durable_checkpoint as fold_live;\nfn tick(p: \&Path, t: \&str) { let _ = fold_live(p, t); }\n' >> apps/aberp/src/live_checkpoint.rs"

probe "P14 BYPASS (b): a DIFFERENT public wrapper over the same rename -> RED" 1 \
  "printf 'fn tick(p: \&Path) { let _ = aberp_snapshot::provision_atomic(p, |_c| Ok(())); }\n' >> apps/aberp/src/live_checkpoint.rs"

probe "P14b BYPASS (b'): resume_pending_install in a daemon -> RED" 1 \
  "printf 'fn tick(p: \&Path) { let _ = aberp_snapshot::resume_pending_install(p); }\n' >> apps/aberp/src/snapshot.rs"

probe "P15 BYPASS (c): the pub rename primitive itself -> RED" 1 \
  "printf 'fn tick(s: \&Path, d: \&Path) { let _ = aberp_snapshot::atomic_install(s, d); }\n' >> apps/aberp/src/live_checkpoint.rs"

probe "P15b BYPASS (c'): a bare function-POINTER to the rename primitive -> RED" 1 \
  "printf 'fn tick() { let f = aberp_snapshot::atomic_install; let _ = f; }\n' >> apps/aberp/src/live_checkpoint.rs"

# ── P16/P17 — the census is closed in BOTH directions for the LEGIT boot path.
# The boot callers must stay green (P0 covers the unmutated tree), but a second
# copy of one, or the deletion of one, must both be red: an extra
# `recover_or_refuse_with_audit` is a new recovery site nobody argued for, and a
# deleted `resume_pending_install` silently drops the ADR-0098 R2 crash-resume.
probe "P16 an EXTRA copy of an already-censused boot symbol (count drift up) -> RED" 1 \
  "printf 'fn again(p: \&Path) { let _ = aberp_snapshot::recover_or_refuse_with_audit(p); }\n' >> apps/aberp/src/serve.rs"

probe "P17 a censused BOOT durability step DELETED (count drift down) -> RED" 1 \
  "perl -0pi -e 's/aberp_snapshot::resume_pending_install\(&args\.db\)/Ok::<_, ()>(0).map(|_| aberp_snapshot::ResumeAction::NoPendingInstall)/' apps/aberp/src/serve.rs"

# ── P18 — the family census must not be satisfiable by a typo'd symbol name.
# A census line naming something outside the family would silently govern
# nothing while looking like coverage.
probe "P18 a census entry naming a NON-family symbol -> RED" 1 \
  "printf 'apps/aberp/src/products.rs\tatomic_instal\t1\ttypo\n' >> tools/adr0111_rename_family_sites.txt"

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
