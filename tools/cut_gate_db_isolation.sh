#!/usr/bin/env bash
#
# cut_gate_db_isolation.sh — ADR-0093 / ADR-0002 DB-isolation cut-gate.
#
# Enforces, at every product-line cut and on every CI run, that the
# sawed-off Portable+Defense editions tree CANNOT drift back into sharing
# prod's tree, launch surface, or database. This is the mechanical
# guardrail behind the cornerstone in FOUNDATION.md §2 ("Database-per-
# tenant. Each tenant owns its own physical store") and ADR-0002, applied
# at the product-line granularity by ADR-0093.
#
# Exit 0 = gate green. Non-zero = a saw-off invariant was violated.
#
# Checks tighten as the saw-off lands chunk by chunk (see SAW-OFF.md):
#   CHECK 1 (chunk 1, ENFORCED) — no prod launch surface in this tree.
#   CHECK 2 (chunk 1, ENFORCED) — saw-off markers present (SAW-OFF.md + ADR-0093).
#   CHECK 3 (chunk 2, ENFORCED) — each edition binds its OWN ~/.aberp-<ed>/
#           root at compile time; no launcher or source resolver reaches
#           prod's ~/.aberp/prod. Enforced by default;
#           ENFORCE_EDITION_DB_BINDING=0 disables it for a deliberate,
#           temporary local probe only.
#   CHECK 4 (chunk 3, ENFORCED) — the edition owns its OWN write/checkpoint
#           path: an edition-scoped, prod-refusing snapshot store; the
#           crash-safe durable checkpoint module (ADR-0082) wired into the
#           snapshot crate + clean shutdown; and reconcile safety (a mirror
#           AHEAD of the DB is preserved + refused, never silently
#           truncated). ENFORCE_CHUNK3_INVARIANTS=0 disables it for a
#           deliberate, temporary local probe only.
#   CHECK 5 (chunk 4, ENFORCED) — durable checkpoint is build-aside +
#           atomic rename (rename(2) + fsync of file & parent dir), never an
#           in-place rewrite of the live DB. ENFORCE_CHECKPOINT_ATOMIC=0
#           disables it for a deliberate, temporary local probe only.
#   CHECK 6 (chunk 4, ENFORCED) — no editions BINARY source resolves prod's
#           bare snapshot store ~/Documents/ABERP-snapshots/ (the
#           default_store_dir resolver or the bare component); editions use
#           ABERP-snapshots-<edition>. ENFORCE_SNAPSHOT_STORE_ISOLATION=0
#           disables it.
#   CHECK 7 (chunk 4, ENFORCED) — edition launchers bind a single MATCHING
#           root; arms don't cross (a --features production launcher binds
#           .aberp-defense, never the sibling/prod root).
#           ENFORCE_LAUNCHER_ARM_MATCH=0 disables it.
#   CHECK 8 (S2 storefront-isolation, ENFORCED) — storefront reach
#           (polling abenerp.com for customer CAD / pushing the catalogue) is
#           a COMPILE-TIME Defense-only capability: build_profile carries the
#           predicate + runtime backstop, serve.rs has the boot guard wired
#           into both config arms, and EVERY storefront daemon spawn + on-
#           demand handler sits behind storefront_polling_allowed(). A Portable
#           build physically cannot reach the storefront regardless of
#           [quote_intake] config / ABERP_QUOTE_INTAKE_* env.
#           ENFORCE_STOREFRONT_GATE=0 disables it for a deliberate local probe.
#
# Negative probes for the CHECKs live in tools/cut_gate_negative_probes.sh
# (each plants a violation in a throwaway copy and asserts the gate FAILS).

set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
fail=0
note() { printf '  %s\n' "$*"; }
echo "ADR-0093 DB-isolation cut-gate — root: $ROOT"

# ── CHECK 1 — no prod launch surface (ENFORCED) ──────────────────────────
echo "[CHECK 1] prod launch surface absent"
for f in run/run_prod.sh run/upgrade_prod.sh; do
  if [[ -e "$f" ]]; then
    note "✗ FAIL: $f exists — the editions tree must not carry the prod launcher."
    fail=1
  else
    note "✓ $f absent"
  fi
done

# ── CHECK 2 — saw-off markers present (ENFORCED) ─────────────────────────
echo "[CHECK 2] saw-off markers present"
[[ -f SAW-OFF.md ]] && note "✓ SAW-OFF.md present" || { note "✗ FAIL: SAW-OFF.md missing (editions-tree sentinel)."; fail=1; }
if ls adr/0093-*.md >/dev/null 2>&1; then note "✓ ADR-0093 present"; else note "✗ FAIL: adr/0093-*.md missing."; fail=1; fi

# ── CHECK 3 — edition DB binding (ENFORCED · chunk 2) ────────────────────
# The ADR-0093 build-locked binding has landed: each edition resolves its
# OWN ~/.aberp-<edition>/ root from a COMPILE-TIME constant
# (build_profile::EDITION) and physically refuses prod's or the sibling's
# root. This gate proves the binding stays in place, four ways.
echo "[CHECK 3] edition DB binding — own-root, no ~/.aberp/prod (ENFORCED)"
enforce="${ENFORCE_EDITION_DB_BINDING:-1}"
flag() {  # $1 = message; trips the gate iff enforcement is on
  note "$1"
  if [[ "$enforce" == "1" ]]; then fail=1; else note "  (enforcement disabled — not failing)"; fi
}

# 3a — no launcher resolves prod's tenant/DB root (ignore comment lines).
offenders="$(grep -rnE ':-prod\}|/\.aberp/prod' run/ 2>/dev/null | grep -vE '^[^:]+:[0-9]+:[[:space:]]*#' || true)"
if [[ -n "$offenders" ]]; then
  flag "✗ launcher(s) still resolve prod's tenant/DB root:"
  printf '%s\n' "$offenders" | sed 's/^/      /'
else
  note "✓ no launcher resolves ~/.aberp/prod"
fi

# 3b — each edition launcher binds its OWN sibling root (positive proof).
check_own_root() {  # $1 launcher  $2 expected root dir
  if [[ ! -f "$1" ]]; then flag "✗ missing launcher: $1"; return; fi
  if grep -qF -- "$2" "$1"; then note "✓ $(basename "$1") binds $2"; else flag "✗ $(basename "$1") does not bind its own root ($2)"; fi
}
check_own_root run/run_defense.sh      ".aberp-defense"
check_own_root run/upgrade_defense.sh  ".aberp-defense"
check_own_root run/run_portable.sh     ".aberp-portable"
check_own_root run/upgrade_portable.sh ".aberp-portable"

# 3c — compile-time Edition→root binding present in the source of truth.
bp="apps/aberp/src/build_profile.rs"
if [[ -f "$bp" ]] && grep -q 'pub enum Edition' "$bp" && grep -q 'EDITION_DATA_DIRNAME' "$bp" \
   && grep -qF 'assert!(!matches!(EDITION, Edition::Prod))' "$bp"; then
  note "✓ compile-time Edition binding present (build_profile.rs)"
else
  flag "✗ build_profile.rs missing the compile-time Edition→root binding"
fi

# 3d — no Rust resolver reconstructs prod's base root ~/.aberp/ directly;
#      every per-tenant path must derive from build_profile::edition_data_dirname.
src_offenders="$(grep -rnE '\.join\("\.aberp"\)|format!\("\{home\}/\.aberp/' apps/aberp/src 2>/dev/null || true)"
if [[ -n "$src_offenders" ]]; then
  flag "✗ source still resolves prod's base root ~/.aberp/ directly:"
  printf '%s\n' "$src_offenders" | sed 's/^/      /'
else
  note "✓ no source resolver reconstructs ~/.aberp/ (all via edition_data_dirname)"
fi

# ── CHECK 4 — edition own write/checkpoint path (ENFORCED · chunk 3) ──────
# Chunk 3 landed the edition-scoped snapshot/restore + DuckDB write path:
#   (a) snapshots go to an edition-scoped, prod-refusing store;
#   (b) the deferred crash-safe durable-checkpoint fix (ADR-0082) lives in a
#       dedicated module wired into the snapshot crate and clean shutdown;
#   (c) boot reconcile refuses (never silently truncates) a mirror that is
#       AHEAD of the DB, preserving the recovery evidence first.
# This check proves all three stay in place.
echo "[CHECK 4] edition own write/checkpoint path — snapshot store, crash-safe checkpoint, reconcile safety (ENFORCED)"
enforce4="${ENFORCE_CHUNK3_INVARIANTS:-1}"
flag4() { note "$1"; if [[ "$enforce4" == "1" ]]; then fail=1; else note "  (enforcement disabled — not failing)"; fi; }
has() { grep -q -- "$2" "$1" 2>/dev/null; }

# 4a — crash-safe durable checkpoint module present + wired.
cs="crates/aberp-snapshot/src/crash_safe.rs"
if [[ -f "$cs" ]] && has "$cs" 'pub fn durable_checkpoint' && has "$cs" 'fn atomic_install' \
   && has "$cs" 'fn fsync_dir' \
   && has crates/aberp-snapshot/src/lib.rs 'mod crash_safe;' \
   && has crates/aberp-snapshot/src/lib.rs 'durable_checkpoint'; then
  note "✓ crash-safe durable checkpoint module present + exported (atomic rename + fsync file&dir)"
else
  flag4 "✗ crash-safe checkpoint module missing/unwired (crash_safe.rs + lib.rs mod/export)"
fi

# 4b — clean-shutdown durable checkpoint wired into serve.
if has apps/aberp/src/snapshot.rs 'fn checkpoint_on_clean_shutdown' \
   && has apps/aberp/src/serve.rs 'checkpoint_on_clean_shutdown('; then
  note "✓ clean-shutdown durable checkpoint wired (snapshot.rs + serve.rs)"
else
  flag4 "✗ clean-shutdown checkpoint not wired into serve"
fi

# 4c — snapshot store is edition-scoped + prod-refusing.
if has crates/aberp-snapshot/src/store.rs 'pub fn edition_store_dir' \
   && has crates/aberp-snapshot/src/take.rs 'pub fn ensure_not_prod_path' \
   && has apps/aberp/src/snapshot.rs 'edition_store_segment()' \
   && has apps/aberp/src/snapshot.rs 'ensure_not_prod_path'; then
  note "✓ snapshot store edition-scoped + prod-refusing (edition_store_dir + ensure_not_prod_path)"
else
  flag4 "✗ snapshot store not edition-scoped/prod-refusing"
fi

# 4d — the binary's store resolver no longer reaches prod's bare store.
if has apps/aberp/src/snapshot.rs 'default_store_dir'; then
  flag4 "✗ snapshot.rs still calls default_store_dir (prod-shaped store) — must use edition_store_dir"
else
  note "✓ binary store resolver uses only the edition-scoped store (no default_store_dir)"
fi

# 4e — reconcile safety: ahead mirror preserved + refused, never truncated.
mir="crates/audit-ledger/src/mirror.rs"
if grep -q 'RecoveryAction::Truncated' "$mir" 2>/dev/null; then
  flag4 "✗ mirror.rs still has the silent-truncate path (RecoveryAction::Truncated)"
elif has crates/audit-ledger/src/error.rs 'MirrorAheadOfDb' \
     && has "$mir" 'fn preserve_ahead_mirror' \
     && has apps/aberp/src/serve.rs 'MirrorAheadOfDb'; then
  note "✓ reconcile safety: ahead mirror preserved + refused (MirrorAheadOfDb), boot refuses; no auto-truncate"
else
  flag4 "✗ reconcile safety incomplete (need MirrorAheadOfDb + preserve_ahead_mirror + serve refuse, and NO Truncated)"
fi

# ── CHECK 5 — durable checkpoint = build-aside + atomic rename, never an
#    in-place rewrite of the live DB (ENFORCED · chunk 4) ──────────────────
# ADR-0082: the corruption being fixed is an IN-PLACE WAL-fold that tears the
# live *.duckdb. The fix MUST build a fresh, self-contained staging file and
# swap it over the live path with a single rename(2) (+ fsync of file AND
# parent dir). CHECK 4a only proves the symbols exist; this proves the COMMIT
# stays swap-based and can never regress to overwriting the live file in place.
echo "[CHECK 5] durable checkpoint = build-aside + atomic rename (no in-place live-file rewrite, ENFORCED)"
enforce5="${ENFORCE_CHECKPOINT_ATOMIC:-1}"
flag5() { note "$1"; if [[ "$enforce5" == "1" ]]; then fail=1; else note "  (enforcement disabled — not failing)"; fi; }
hasF() { grep -qF -- "$2" "$1" 2>/dev/null; }
cs="crates/aberp-snapshot/src/crash_safe.rs"
if [[ ! -f "$cs" ]]; then
  flag5 "✗ crash_safe.rs missing — the durable-checkpoint primitive is gone"
else
  if hasF "$cs" 'std::fs::rename(staged, target)'; then
    note "✓ atomic_install swaps via std::fs::rename(staged, target) (all-or-nothing)"
  else
    flag5 "✗ atomic_install no longer swaps via std::fs::rename(staged, target) — an in-place copy/overwrite would tear the live DB"
  fi
  if hasF "$cs" 'atomic_install(&staging, db_path)' && hasF "$cs" 'Connection::open(&staging)'; then
    note "✓ durable_checkpoint imports into a PRIVATE staging DB and installs it via atomic_install (never the live file)"
  else
    flag5 "✗ durable_checkpoint must build a private staging DB (Connection::open(&staging)) and commit via atomic_install(&staging, db_path)"
  fi
  if hasF "$cs" 'fsync_dir(parent)'; then
    note "✓ the rename is made durable (parent-directory fsync)"
  else
    flag5 "✗ atomic_install no longer fsyncs the parent dir after rename — the swap is not crash-durable"
  fi
fi

# ── CHECK 6 — no editions binary source resolves prod's bare snapshot store
#    ~/Documents/ABERP-snapshots/ (ENFORCED · chunk 4) ─────────────────────
# ADR-0093 §5: snapshots are edition-scoped (ABERP-snapshots-<edition>). The
# prod-shaped resolver default_store_dir() and the bare "ABERP-snapshots"
# component must never be reached from an editions BINARY. CHECK 4d guards
# snapshot.rs alone; this generalizes the ban to the whole binary so a NEW
# source file cannot regress, and bans the bare-path construction directly.
echo "[CHECK 6] no editions binary source resolves prod's bare snapshot store ~/Documents/ABERP-snapshots/ (ENFORCED)"
enforce6="${ENFORCE_SNAPSHOT_STORE_ISOLATION:-1}"
flag6() { note "$1"; if [[ "$enforce6" == "1" ]]; then fail=1; else note "  (enforcement disabled — not failing)"; fi; }
calls="$(grep -rnF 'default_store_dir(' apps/aberp/src 2>/dev/null || true)"
if [[ -n "$calls" ]]; then
  flag6 "✗ binary source calls prod-shaped default_store_dir() — must use edition_store_dir():"
  printf '%s\n' "$calls" | sed 's/^/      /'
else
  note "✓ no binary source calls default_store_dir() (edition_store_dir only)"
fi
bare="$(grep -rnF '.join("ABERP-snapshots")' apps/aberp/src 2>/dev/null || true)"
if [[ -n "$bare" ]]; then
  flag6 '✗ binary source builds the bare prod snapshot path .join("ABERP-snapshots") (edition form is ABERP-snapshots-<seg>):'
  printf '%s\n' "$bare" | sed 's/^/      /'
else
  note "✓ no binary source builds the bare ~/Documents/ABERP-snapshots/ path"
fi

# ── CHECK 7 — edition launchers bind a single MATCHING root; arms don't
#    cross (ENFORCED · chunk 4) ────────────────────────────────────────────
# CHECK 3b proves each named launcher binds ITS OWN root; it does NOT prove a
# launcher avoids the SIBLING's root, nor that a launcher that ACTUALLY builds
# the production (Defense) arm binds .aberp-defense. This catches a
# mismatched/rogue launcher (e.g. a new Defense launcher that boots
# `--features production` but points at .aberp-portable or prod). Comment lines
# are ignored — only real bindings / build invocations count.
echo "[CHECK 7] edition launchers bind a single matching root — arms don't cross (ENFORCED)"
enforce7="${ENFORCE_LAUNCHER_ARM_MATCH:-1}"
flag7() { note "$1"; if [[ "$enforce7" == "1" ]]; then fail=1; else note "  (enforcement disabled — not failing)"; fi; }
ncgrep() { grep -nE "$1" "$2" 2>/dev/null | grep -vE '^[0-9]+:[[:space:]]*#' || true; }
sibling_check() {  # $1 launcher  $2 own-regex  $3 sibling-regex  $4 own-label  $5 sibling-label
  if [[ ! -f "$1" ]]; then flag7 "✗ missing launcher: $1"; return; fi
  [[ -z "$(ncgrep "$2" "$1")" ]] && flag7 "✗ $(basename "$1") does not bind its own root ($4)"
  local cross; cross="$(ncgrep "$3" "$1")"
  if [[ -n "$cross" ]]; then
    flag7 "✗ $(basename "$1") binds the SIBLING root ($5) — arms crossed:"
    printf '%s\n' "$cross" | sed 's/^/      /'
  else
    note "✓ $(basename "$1") binds only $4"
  fi
}
sibling_check run/run_defense.sh      '\.aberp-defense'  '\.aberp-portable' ".aberp-defense"  ".aberp-portable"
sibling_check run/upgrade_defense.sh  '\.aberp-defense'  '\.aberp-portable' ".aberp-defense"  ".aberp-portable"
sibling_check run/run_portable.sh     '\.aberp-portable' '\.aberp-defense'  ".aberp-portable" ".aberp-defense"
sibling_check run/upgrade_portable.sh '\.aberp-portable' '\.aberp-defense'  ".aberp-portable" ".aberp-defense"
for f in run/*.sh; do
  [[ -f "$f" ]] || continue
  if [[ -n "$(ncgrep 'cargo (build|run).*--features production.*--bin aberp' "$f")" ]]; then
    [[ -z "$(ncgrep '\.aberp-defense' "$f")" ]] && flag7 "✗ $(basename "$f") builds the production (Defense) arm but never binds .aberp-defense"
    wrong="$(ncgrep '\.aberp-portable|/\.aberp/prod' "$f")"
    if [[ -n "$wrong" ]]; then
      flag7 "✗ $(basename "$f") builds the production (Defense) arm but binds a non-defense root:"
      printf '%s\n' "$wrong" | sed 's/^/      /'
    fi
  fi
done

# ── CHECK 8 — storefront reach is a COMPILE-TIME Defense-only capability
#    (ENFORCED · ADR-0093 storefront isolation) ─────────────────────────────
# The quote-intake / pricing pipeline polls the customer storefront
# (abenerp.com) for uploaded CAD and pushes the catalogue / priced PDFs back.
# That REACH pulls real customer data, so — like the prod-NAV endpoint and the
# edition DB root — it is bound to the edition at COMPILE time, not merely
# config-gated: ONLY the Defense build may reach the storefront; a Portable
# (demo) build has the capability compiled out and physically cannot poll/push
# regardless of [quote_intake] config or ABERP_QUOTE_INTAKE_* env. This check
# proves every storefront-reaching spawn/handler sits behind the gate.
echo "[CHECK 8] storefront reach gated to Defense edition (ADR-0093)"
enforce8="${ENFORCE_STOREFRONT_GATE:-1}"
flag8() { note "$1"; if [[ "$enforce8" == "1" ]]; then fail=1; else note "  (enforcement disabled — not failing)"; fi; }
bp="apps/aberp/src/build_profile.rs"
sv="apps/aberp/src/serve.rs"
probe="tools/storefront_gate_decision_probe.rs"
# window-search helpers
back_has() { local f="$1" ln="$2" win="$3" needle="$4" start; start=$(( ln - win )); (( start < 1 )) && start=1; sed -n "${start},${ln}p" "$f" | grep -qF "$needle"; }
fwd_has()  { local f="$1" ln="$2" win="$3" needle="$4" end;   end=$(( ln + win )); sed -n "${ln},${end}p" "$f" | grep -qF "$needle"; }

# 8a — compile-time predicate + runtime backstop + single-source DECISION rule.
if [[ -f "$bp" ]] \
   && grep -q 'pub const fn storefront_polling_allowed_for(edition: Edition) -> bool' "$bp" \
   && grep -q 'pub const fn storefront_polling_allowed() -> bool' "$bp" \
   && grep -q 'pub fn assert_storefront_reach_allowed' "$bp" \
   && grep -qF 'matches!(edition, Edition::Defense)' "$bp"; then
  note "✓ build_profile.rs: storefront_polling_allowed[_for] + assert_storefront_reach_allowed (Defense-only)"
else
  flag8 "✗ build_profile.rs missing the storefront-reach predicate / backstop / decision rule"
fi

# 8a' — the standalone both-arms proof carries the SAME decision rule (no drift).
if [[ -f "$probe" ]] && grep -qF 'matches!(edition, Edition::Defense)' "$probe"; then
  note "✓ both-arms decision probe present + carries build_profile's exact rule (drift-proof)"
else
  flag8 "✗ tools/storefront_gate_decision_probe.rs missing or drifted from build_profile's decision rule"
fi

# 8b — serve.rs boot guard present AND wired (definition + >=1 call site).
if grep -q 'fn guard_storefront_reach_matches_edition' "$sv" \
   && [[ "$(grep -c 'guard_storefront_reach_matches_edition' "$sv")" -ge 3 ]]; then
  note "✓ serve.rs boot guard guard_storefront_reach_matches_edition present + called (resolved+malformed arms)"
else
  flag8 "✗ serve.rs boot guard guard_storefront_reach_matches_edition missing or not wired into both config arms"
fi

# 8c — every KNOWN storefront DAEMON spawn sits behind the gate: each
#      coordinator.register("<tag>") has storefront_polling_allowed within the
#      preceding 20 lines (per-spawn debug_assert backstop / boot guard).
for tag in quote-intake catalogue-push quote-pricing-pipeline email-outbox-poll pdf-rerender; do
  rln="$(grep -nE "coordinator\.register\([[:space:]]*\"$tag\"|^[[:space:]]*\"$tag\",[[:space:]]*$" "$sv" | head -1 | cut -d: -f1)"
  if [[ -z "$rln" ]]; then
    flag8 "✗ storefront daemon '$tag' has no coordinator.register — surface moved/renamed? gate may be stale"
  elif back_has "$sv" "$rln" 20 "storefront_polling_allowed"; then
    note "✓ '$tag' daemon spawn (L$rln) behind storefront_polling_allowed"
  else
    flag8 "✗ '$tag' daemon spawn (L$rln) is NOT behind storefront_polling_allowed — ungated storefront reach"
  fi
done

# 8d — every on-demand storefront HTTP surface refuses in non-Defense: the
#      gate token appears inside each handler body.
for h in handle_test_catalogue_push handle_put_quote_intake_config handle_test_quote_intake_connection post_operator_accept; do
  hln="$(grep -nE "fn $h\b" "$sv" | head -1 | cut -d: -f1)"
  if [[ -z "$hln" ]]; then
    flag8 "✗ storefront handler $h not found — surface moved/renamed? gate may be stale"
  elif fwd_has "$sv" "$hln" 50 "storefront_polling_allowed"; then
    note "✓ handler $h gates on storefront_polling_allowed (refuses in non-Defense)"
  else
    flag8 "✗ handler $h does NOT gate on storefront_polling_allowed"
  fi
done

# 8e — ANTI-REGRESSION: ANY coordinator.register("<…storefront-keyword…>")
#      — even a NEW tag — must be gated. Keyed STRICTLY off coordinator.register
#      (not axum .route paths): for each register call, read the tag from the
#      same line or, for the multiline form, the next non-blank line; if the tag
#      names a storefront surface it must have storefront_polling_allowed within
#      the preceding 20 lines. Catches a brand-new ungated storefront daemon.
sfkey='storefront|catalogue|quote-intake|quote-pricing|pdf-rerender|email-outbox'
while IFS=: read -r ln _; do
  [[ -z "$ln" ]] && continue
  same="$(sed -n "${ln}p" "$sv")"
  tag="$(printf '%s' "$same" | sed -nE 's/.*coordinator\.register\([[:space:]]*"([^"]+)".*/\1/p')"
  if [[ -z "$tag" ]]; then
    # multiline form: tag is the first quoted string on the next 1-2 lines
    tag="$(sed -n "$((ln+1)),$((ln+2))p" "$sv" | sed -nE 's/^[[:space:]]*"([^"]+)",?[[:space:]]*$/\1/p' | head -1)"
  fi
  [[ -z "$tag" ]] && continue
  if printf '%s' "$tag" | grep -qE "$sfkey"; then
    if back_has "$sv" "$ln" 20 "storefront_polling_allowed"; then
      : # gated
    else
      flag8 "✗ storefront-ish daemon register '$tag' at L$ln is NOT behind storefront_polling_allowed"
    fi
  fi
done < <(grep -nE 'coordinator\.register\(' "$sv" 2>/dev/null || true)

# ── CHECK 9 — editions UPGRADE + pre-upgrade snapshot never default to, accept,
#    or target the frozen prod line (ENFORCED · prod-touch fix 2026-06-27) ──────
# CHECK 3a bans the `:-prod}` default form and any literal `/.aberp/prod` in
# run/. This closes the two gaps behind the live 2026-06-27 incident: (1) a BARE
# `tenant="prod"` default (no `:-prod}` syntax), and (2) an editions upgrade
# routing its pre-upgrade snapshot at the BARE prod root `~/.aberp/` (no literal
# "prod"). It also proves snapshot-prod.sh stays parameterizable (so editions
# can root it at their own tree) and that each editions upgrade passes its
# edition root to the snapshot. ENFORCE_EDITIONS_UPGRADE_PROD_REFUSAL=0 disables
# it for a deliberate, temporary local probe only.
echo "[CHECK 9] editions upgrade+snapshot never default/accept/target the frozen prod line (ENFORCED)"
enforce9="${ENFORCE_EDITIONS_UPGRADE_PROD_REFUSAL:-1}"
flag9() { note "$1"; if [[ "$enforce9" == "1" ]]; then fail=1; else note "  (enforcement disabled — not failing)"; fi; }
ci_ncgrep() { grep -inE "$1" "$2" 2>/dev/null | grep -vE '^[0-9]+:[[:space:]]*#' || true; }

# 9a — snapshot-prod.sh stays parameterizable: honors ABERP_DATA_ROOT and falls
#      back to the prod root ONLY when unset (prod's flow unchanged), so editions
#      can root it at their own tree. A regression that hardcodes ~/.aberp/ trips this.
snap="tools/snapshot-prod.sh"
if [[ -f "$snap" ]] \
   && grep -qE 'DATA_ROOT="\$\{ABERP_DATA_ROOT:-\$\{HOME\}/\.aberp\}"' "$snap" \
   && grep -qE 'TENANT_DIR="\$\{DATA_ROOT\}/\$\{TENANT\}"' "$snap" \
   && grep -qE 'tar -C "\$\{DATA_ROOT\}"' "$snap"; then
  note "✓ snapshot-prod.sh honors ABERP_DATA_ROOT (editions root it at their own tree; prod default unchanged)"
else
  flag9 "✗ snapshot-prod.sh no longer honors ABERP_DATA_ROOT — editions cannot root the snapshot off the frozen prod line"
fi

# 9b/9c/9d — per editions upgrade script.
check_editions_upgrade() {  # $1 script  $2 edition-root (.aberp-<ed>)
  local f="$1" root="$2" base; base="$(basename "$f")"
  if [[ ! -f "$f" ]]; then flag9 "✗ missing editions upgrade script: $f"; return; fi

  # 9b — never DEFAULT the reserved prod tenant. Assignment-anchored so the
  #      fail-fast guard's prose ("'prod' is the reserved tenant") never self-trips.
  local q; q=$'^[[:space:]]*(readonly[[:space:]]+)?tenant=[\'\"]?prod([\'\"]|[[:space:]]|$)'
  local bad_default
  bad_default="$(ci_ncgrep "$q" "$f")"
  bad_default+="$(ci_ncgrep '^[[:space:]]*(readonly[[:space:]]+)?tenant=.*:-[[:space:]]*prod[[:space:]]*}' "$f")"
  if [[ -n "$bad_default" ]]; then
    flag9 "✗ $base defaults the reserved prod tenant — editions must default to a non-prod tenant:"
    printf '%s\n' "$bad_default" | sed 's/^/      /'
  else
    note "✓ $base does not default the reserved prod tenant"
  fi

  # 9c — never reference the BARE frozen prod data root ~/.aberp/ (only the
  #      edition's own ~/.aberp-<ed>/). \.aberp/ matches the prod root but NOT
  #      .aberp-defense/ / .aberp-portable/ (the hyphen breaks the match).
  local bad_root
  bad_root="$(ci_ncgrep '\.aberp/' "$f")"
  if [[ -n "$bad_root" ]]; then
    flag9 "✗ $base references the frozen prod data root ~/.aberp/ — editions must use only $root:"
    printf '%s\n' "$bad_root" | sed 's/^/      /'
  else
    note "✓ $base references only its own edition root ($root), never the bare ~/.aberp/"
  fi

  # 9d — the pre-upgrade snapshot is rooted at the edition tree: the script passes
  #      ABERP_DATA_ROOT to snapshot-prod.sh so it can never fall back to the prod
  #      default. (CHECK 3b/CHECK 7 already prove the value is THIS edition's root.)
  if [[ -n "$(ci_ncgrep 'snapshot-prod\.sh|SNAPSHOT_SCRIPT' "$f")" ]]; then
    if [[ -n "$(ci_ncgrep 'ABERP_DATA_ROOT=.*("\$SNAPSHOT_SCRIPT"|snapshot-prod\.sh)' "$f")" ]]; then
      note "✓ $base roots its pre-upgrade snapshot at its own edition tree (ABERP_DATA_ROOT → snapshot-prod.sh)"
    else
      flag9 "✗ $base invokes snapshot-prod.sh without ABERP_DATA_ROOT — the snapshot would fall back to the frozen prod root ~/.aberp/"
    fi
  fi
}
check_editions_upgrade run/upgrade_defense.sh  ".aberp-defense"
check_editions_upgrade run/upgrade_portable.sh ".aberp-portable"

# ── CHECK 10 — ADR-0098 Session B: daemons route DuckDB through the ONE shared
#    aberp_db::Handle; no NEW separate-instance live open (ENFORCED · D5) ───────
# The 2026-06-29 17:02 re-tear came from many subsystems each Connection::open-
# ing the single-file tenant DB concurrently (N checkpoint actors racing one
# file = duckdb#23046). Session B collapses ALL runtime DB access onto one
# shared instance (crates/aberp-db Handle: one serialized writer + try_clone
# reads + a post-commit lockstep sync_mirror + debounced live_durable_checkpoint).
# This gate (D5) fails if a migrated daemon regrows a live-path Connection::open
# / open_with_flags OUTSIDE the Handle — so the class that caused the incident is
# a RED BUILD, not a latent corruption. The Handle's own single boot open is the
# ONLY allow-listed live open; #[cfg(test)] / in-memory opens are allow-listed.
# Session C (CHECK 10f/10g below) extends the ban to the serve.rs request
# handlers + records the one allow-listed snapshot-EXPORT residual.
# ENFORCE_SHARED_DB_HANDLE=0 disables it for a deliberate, temporary local probe.
echo "[CHECK 10] ADR-0098 Session B — daemons share the one aberp_db::Handle; no new live open (ENFORCED · D5)"
enforce10="${ENFORCE_SHARED_DB_HANDLE:-1}"
flag10() { note "$1"; if [[ "$enforce10" == "1" ]]; then fail=1; else note "  (enforcement disabled — not failing)"; fi; }

# 10a — the shared Handle crate exists + exports the single-instance API.
hb="crates/aberp-db/src/lib.rs"
if [[ -f "$hb" ]] && grep -q 'pub struct Handle' "$hb" && grep -q 'pub fn write(' "$hb" \
   && grep -q 'pub fn read(' "$hb" && grep -q 'fn open_runtime_connection' "$hb"; then
  note "✓ aberp_db::Handle present with write()/read()/open_runtime_connection (single instance)"
else
  flag10 "✗ crates/aberp-db Handle missing or missing its write()/read()/open_runtime_connection API"
fi

# 10b — the post-commit hook REUSES the ADR-0095 primitives (no reinvented
#       durability): lockstep sync_mirror + debounced live_durable_checkpoint.
if grep -q 'sync_mirror' "$hb" && grep -q 'live_durable_checkpoint' "$hb"; then
  note "✓ Handle post-commit hook reuses sync_mirror + live_durable_checkpoint (no new primitive)"
else
  flag10 "✗ Handle does not reuse the ADR-0095 sync_mirror + live_durable_checkpoint primitives"
fi

# 10c — the Handle owns the ONLY allow-listed live open (open_runtime_connection).
if grep -q 'Connection::open(db_path)' "$hb"; then
  note "✓ the single allow-listed live open is the Handle's open_runtime_connection"
else
  flag10 "✗ the Handle's single live open (open_runtime_connection -> Connection::open(db_path)) is missing"
fi

# 10d — NO live-path Connection::open / open_with_flags in the migrated Session-B
#       daemon files OUTSIDE #[cfg(test)]. Scan only the runtime portion (lines
#       before the first #[cfg(test)]; tests live at the bottom) and ban any
#       Connection::open*/open_with_flags that is not open_in_memory.
db_daemons=(
  apps/aberp/src/quote_pricing_pipeline.rs
  apps/aberp/src/email_relay_daemon.rs
  apps/aberp/src/catalogue_push.rs
  apps/aberp/src/quote_pdf_rerender_daemon.rs
  apps/aberp/src/email_outbox_poll_daemon.rs
  crates/aberp-quote-intake/src/service.rs
)
for f in "${db_daemons[@]}"; do
  if [[ ! -f "$f" ]]; then flag10 "✗ migrated daemon file missing: $f"; continue; fi
  cut="$(grep -nE '^[[:space:]]*#\[cfg\(test\)\]' "$f" | head -1 | cut -d: -f1)"
  if [[ -z "$cut" ]]; then cut=$(( $(wc -l < "$f") + 1 )); fi
  runtime_open="$(awk -v c="$cut" 'NR<c' "$f" | grep -nE 'Connection::open(_with_flags)?\(' | grep -v 'open_in_memory' || true)"
  if [[ -n "$runtime_open" ]]; then
    flag10 "✗ $f has a live-path Connection::open OUTSIDE the Handle (Session-B regression):"
    printf '%s\n' "$runtime_open" | sed 's/^/      /'
  else
    note "✓ $(basename "$f") — no runtime Connection::open (routes through aberp_db::Handle)"
  fi
done

# 10e — POSITIVE proof the daemons actually call the shared handle (migration
#       present, not reverted): each migrated file calls the handle's write()/read().
for f in "${db_daemons[@]}"; do
  [[ -f "$f" ]] || continue
  if grep -qE '\.(write|read)\(\)' "$f"; then
    note "✓ $(basename "$f") routes through the handle (.write()/.read() present)"
  else
    flag10 "✗ $(basename "$f") no longer calls the shared handle (.write()/.read()) — migration reverted?"
  fi
done

# ── CHECK 10f — ADR-0098 Session C: the on-demand HTTP REQUEST HANDLERS in
#    serve.rs route DuckDB through the shared Handle too — closing the
#    two-lock-regime window B flagged (daemons on the Handle, request handlers
#    still Connection::open-ing per request). serve.rs INTERLEAVES #[cfg(test)]
#    modules with runtime code, so the daemon files' single-#[cfg(test)]-cut
#    heuristic (10d) does NOT apply; this uses a cfg(test)-aware brace scan
#    (toolchain-free awk, validated against the Session-C enumeration) and
#    allow-lists ONLY the boot-create region (run / seed_demo_sample_data —
#    sequential, pre-serve-loop, before the Handle exists). Any OTHER runtime
#    Connection::open / open_with_flags / append_reopen in serve.rs is the
#    Session-C regression this fails on. ENFORCE_SHARED_DB_HANDLE=0 disables.
echo "[CHECK 10f] ADR-0098 Session C — serve.rs request handlers share the one aberp_db::Handle; no new live open (ENFORCED · D5)"
sv="apps/aberp/src/serve.rs"
if [[ ! -f "$sv" ]]; then
  flag10 "✗ serve.rs missing: $sv"
else
  scan_awk="$(mktemp "${TMPDIR:-/tmp}/serve_open_scan.XXXXXX.awk")"
  cat > "$scan_awk" <<'SERVE_SCAN_AWK'
# cfg(test)+boot-aware live-opener scanner (toolchain-free; bash/awk only).
# Prints "LINE:text" for every Connection::open*/append_reopen in RUNTIME code
# (outside #[cfg(test)]) whose enclosing fn is NOT on the boot allow-list.
# Allow-listed boot fns passed via -v allow="fn1,fn2,...".
BEGIN{ depth=0; tdepth=-1; pending=0; inblk=0; instr=0; n_allow=split(allow,A,",") }
function is_allowed(name,   k){ for(k=1;k<=n_allow;k++) if(A[k]==name) return 1; return 0 }
{
  line=$0
  # fn-name tracking (decls are never inside strings/comments at col<=~8)
  if (match(line,/^[ \t]*(pub(\([^)]*\))?[ \t]+)?(async[ \t]+)?(unsafe[ \t]+)?fn[ \t]+[A-Za-z0-9_]+/)) {
    fn=substr(line,RSTART,RLENGTH); sub(/.*fn[ \t]+/,"",fn); fname=fn
  }
  st=line; sub(/^[ \t]+/,"",st)
  if (st ~ /^#\[cfg\(/ && st ~ /test/ && st !~ /not\(test\)/) pending=1
  was_in=(tdepth>=0)
  L=length(line)
  for(i=1;i<=L;i++){
    c=substr(line,i,1); d=substr(line,i,2)
    if(inblk){ if(d=="*/"){inblk=0;i++} ; continue }
    if(instr){ if(c=="\\"){i++;continue} ; if(c=="\""){instr=0} ; continue }
    if(d=="//"){ break }            # line comment: ignore rest
    if(d=="/*"){ inblk=1;i++;continue }
    if(c=="\""){ instr=1; continue }
    if(c=="'"){                      # char literal or lifetime: skip 'x' or '\x'
       if(substr(line,i,3) ~ /^'\\.'/){ i+=2; }       # '\n'
       else if(substr(line,i+2,1)=="'"){ i+=2 }       # 'x'
       continue
    }
    if(c=="{"){ depth++; if(pending && tdepth<0){ tdepth=depth; pending=0 } }
    else if(c=="}"){ if(tdepth==depth) tdepth=-1; depth-- }
  }
  now_in=(tdepth>=0)
  intest = was_in || now_in
  if (!intest) {
    if (line ~ /Connection::open(_with_flags)?\(/ && line !~ /open_in_memory/) {
      if (!is_allowed(fname)) { t=line; sub(/^[ \t]+/,"",t); printf "%d:%s:%s\n",NR,fname,substr(t,1,70) }
    }
    else if (line ~ /append_reopen[ \t]*\(/) {
      if (!is_allowed(fname)) { t=line; sub(/^[ \t]+/,"",t); printf "%d:%s:%s\n",NR,fname,substr(t,1,70) }
    }
  }
}
SERVE_SCAN_AWK
  # Boot allow-list: the sequential pre-serve-loop create/provision/seed region.
  serve_strays="$(awk -v allow="run,seed_demo_sample_data" -f "$scan_awk" "$sv" || true)"
  rm -f "$scan_awk"
  if [[ -n "$serve_strays" ]]; then
    flag10 "✗ serve.rs has a live-path Connection::open/open_with_flags/append_reopen OUTSIDE the Handle (Session-C regression):"
    printf '%s\n' "$serve_strays" | sed 's/^/      /'
  else
    note "✓ serve.rs — no runtime request-handler Connection::open (routes through aberp_db::Handle; boot-create allow-listed)"
  fi
  if grep -qE 'state(_for_task)?\.db\.(read|write)\(\)' "$sv"; then
    note "✓ serve.rs routes request handlers through state.db.read()/.write()"
  else
    flag10 "✗ serve.rs no longer calls state.db.read()/.write() — Session-C migration reverted?"
  fi
fi

# ── CHECK 10g — ADR-0098 Session C: the SOLE sanctioned non-Handle live opener
#    is the 4-h snapshot daemon's logical read-only EXPORT
#    (crates/aberp-snapshot/src/take.rs). It must carry its SANCTIONED RESIDUAL
#    marker so this allow-list entry is self-documenting and cannot silently
#    grow into an undocumented separate opener.
echo "[CHECK 10g] ADR-0098 Session C — snapshot EXPORT opener is the sole allow-listed residual (documented)"
tk="crates/aberp-snapshot/src/take.rs"
if [[ -f "$tk" ]] && grep -q 'SANCTIONED RESIDUAL (gate allow-listed' "$tk"; then
  note "✓ snapshot EXPORT opener documented as the sole allow-listed residual (take.rs)"
else
  flag10 "✗ snapshot EXPORT opener allow-list marker missing in take.rs (undocumented residual — see ADR-0098 Session C)"
fi

# ── CHECK 10h — ADR-0098 Session C2: the two NAV daemons (ap_sync + poll_ack) and
#    the invoicing-mutation seam (issue/storno/modification/submit/mark-paid) route
#    DuckDB through the shared Handle — NO runtime independent opener outside it.
#    C's 10d/10f banned ONLY the Connection::open family and never scanned these
#    files at all; review F1/F3/F4 showed the NAV daemons + the whole invoicing
#    surface stayed live AND a whole opener class (Ledger::open / DuckDbBillingStore::
#    open) was invisible — D5 was green-while-blind. 10h closes both: it scans the
#    seven migrated files with the FULL ban set (Connection::open*/Ledger::open/
#    DuckDbBillingStore::open/append_reopen), comment/string/cfg(test)-aware
#    (tools/adr0098_opener_scan.awk; open_in_memory & from_connection are the
#    sanctioned shared-instance seams and excluded).
echo "[CHECK 10h] ADR-0098 Session C2 — NAV daemons + invoicing seam on the Handle (bans Connection::open/Ledger::open/DuckDbBillingStore::open; ENFORCED · D5)"
scan="tools/adr0098_opener_scan.awk"
[[ -f "$scan" ]] || flag10 "✗ opener scanner missing: $scan"
c2_files=(
  apps/aberp/src/ap_sync.rs
  apps/aberp/src/poll_ack.rs
  apps/aberp/src/issue_invoice.rs
  apps/aberp/src/issue_storno.rs
  apps/aberp/src/issue_modification.rs
  apps/aberp/src/submit_invoice.rs
  apps/aberp/src/mark_invoice_paid.rs
)
for f in "${c2_files[@]}"; do
  if [[ ! -f "$f" ]]; then flag10 "✗ C2 migrated file missing: $f"; continue; fi
  strays="$(awk -f "$scan" "$f" 2>/dev/null || true)"
  if [[ -n "$strays" ]]; then
    flag10 "✗ $f has a runtime independent live-DB opener OUTSIDE the Handle (Session-C2 regression):"
    printf '%s\n' "$strays" | sed 's/^/      /'
  else
    note "✓ $(basename "$f") — no runtime Connection::open/Ledger::open/DuckDbBillingStore::open (routes through aberp_db::Handle)"
  fi
  if grep -qE '\.(read|write)\(\)' "$f"; then
    note "  ✓ $(basename "$f") routes through the handle (.read()/.write() present)"
  else
    flag10 "✗ $(basename "$f") no longer calls the shared handle (.read()/.write()) — C2 migration reverted?"
  fi
done

# ── CHECK 10i — ADR-0098 Session C2: the FROZEN residual-opener ledger. Every
#    runtime independent opener NOT on the Handle is accounted for in
#    tools/adr0098_c2_frozen_residuals.txt (operator-paced ERP modules + CLI
#    one-shots + serve.rs request-handler audit reads). Each file's count is
#    FROZEN: it may not EXCEED its listed count, and no NEW opener-bearing file
#    may appear unlisted. This is what makes a deferred-to-v0.2.6 surface SAFE
#    (it cannot silently grow) and what keeps a green D5 from ever again meaning
#    "blind to most of the openers" (review F1-F4). Toolchain-free (awk).
echo "[CHECK 10i] ADR-0098 Session C2 — frozen residual-opener ledger (operator/CLI/serve cannot grow; ENFORCED · D5)"
manifest="tools/adr0098_c2_frozen_residuals.txt"
if [[ ! -f "$manifest" ]]; then
  flag10 "✗ frozen-residual manifest missing: $manifest"
elif [[ ! -f "$scan" ]]; then
  : # already flagged
else
  c2_set=" apps/aberp/src/ap_sync.rs apps/aberp/src/poll_ack.rs apps/aberp/src/issue_invoice.rs apps/aberp/src/issue_storno.rs apps/aberp/src/issue_modification.rs apps/aberp/src/submit_invoice.rs apps/aberp/src/mark_invoice_paid.rs "
  resid_fail=0
  while IFS= read -r f; do
    case " $c2_set " in *" $f "*) continue;; esac
    case "$f" in crates/aberp-db/*|crates/aberp-snapshot/src/take.rs) continue;; esac
    if [[ "$f" == "apps/aberp/src/serve.rs" ]]; then
      actual="$(awk -v allow="run,seed_demo_sample_data" -f "$scan" "$f" 2>/dev/null | wc -l | tr -d ' ')"
    else
      actual="$(awk -f "$scan" "$f" 2>/dev/null | wc -l | tr -d ' ')"
    fi
    [[ "${actual:-0}" -eq 0 ]] && continue
    frozen="$(awk -v p="$f" '$1!="#" && $2==p{print $1}' "$manifest")"
    if [[ -z "$frozen" ]]; then
      flag10 "✗ NEW unaccounted opener-bearing file $f ($actual runtime opener(s)) — migrate it onto the Handle or add a tracked-residual line to $manifest"
      resid_fail=1
    elif [[ "$actual" -gt "$frozen" ]]; then
      flag10 "✗ $f grew its residual openers ($actual > frozen $frozen) — the deferred surface may not grow; migrate the new opener onto the Handle"
      resid_fail=1
    fi
  done < <(find apps/aberp/src modules -name '*.rs' | grep -vE '/tests/' | sort)
  if [[ "$resid_fail" == "0" ]]; then
    ft="$(grep -vE '^#' "$manifest" | awk '{s+=$1} END{print s}')"
    ff="$(grep -vcE '^#' "$manifest")"
    note "✓ frozen residual ledger holds — no file exceeds its frozen count, no new unlisted opener ($ft frozen openers across $ff files; v0.2.6 migration target)"
  fi
fi

echo
if [[ "$fail" -ne 0 ]]; then echo "CUT-GATE: ✗ FAILED"; exit 1; fi
echo "CUT-GATE: ✓ PASSED"
