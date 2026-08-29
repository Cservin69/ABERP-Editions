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

# 10c-tryclone — the Handle's read() is the SANCTIONED single-instance read: a
#   duckdb try_clone of the ONE shared instance (coherent -- it replays the live
#   writer's WAL), NOT a separate read-only OS open. The F5 AccessMode::ReadOnly
#   instance proved to cause pervasive stale reads (a separate instance does not
#   replay the WAL); ADR-0098 C2 / v0.2.5 Option 1 (Ervin-approved) adopts
#   try_clone as the one coherent read seam. try_clone is PERMITTED (it is not a
#   separate Connection::open); a regression to a separate open_with_flags/
#   AccessMode read-only instance IN the Handle is a coherence RED BUILD (teeth:
#   cut_gate_negative_probes.sh "[CHECK 10 try_clone]").
if grep -q 'try_clone()' "$hb" && ! grep -qE 'open_with_flags\(|AccessMode::' "$hb"; then
  note "✓ Handle::read() is the sanctioned single-instance try_clone (coherent; no separate read-only open_with_flags/AccessMode)"
else
  flag10 "✗ Handle::read() must be a try_clone of the shared instance (ADR-0098 C2 Option 1) — a separate open_with_flags/AccessMode read-only instance is a coherence regression"
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
  # ── D-22 (ADR-0114) — the NAV money-submission CLI one-shots. They were
  #    FROZEN residuals (10i/10j/10k) rather than migrated seams, which is
  #    exactly how they kept the pre-D3 durability inversion alive: an
  #    independent `Connection::open` per tx, then `Ledger::open` +
  #    `sync_mirror`, so the audit MIRROR was explicitly `fsync`ed and the DB
  #    was not. Now Handle-routed like their already-migrated twins, so they
  #    are promoted from "frozen, may not grow" to "zero, ENFORCED here" —
  #    a re-added opener is a red build, not a silently tolerated count.
  apps/aberp/src/drain_submission_queue.rs
  apps/aberp/src/retry_submission.rs
  apps/aberp/src/submit_annulment.rs
  apps/aberp/src/poll_annulment_ack.rs
  apps/aberp/src/observe_receiver_confirmation.rs
  apps/aberp/src/recover_from_nav.rs
  apps/aberp/src/mark_abandoned.rs
  apps/aberp/src/request_technical_annulment.rs
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

# ── CHECK 10L — ADR-0098 R7: the BOOT RE-FORK class (an independent live-DB opener
#    + a rogue `sync_mirror` in the SAME runtime fn) cannot regrow, and the two write
#    seams migrated THIS session stay Handle-bound. The 415/416 fork was NOT caught by
#    10h–10k: the backfill opener WAS pragma-fenced (10j green) and frozen in the 10i
#    ledger (count green) — yet it still forked, because it was spawned at BOOT with a
#    bare `db_path`, opened a SEPARATE DuckDB instance, read a STALE ledger head,
#    re-assigned seq-415, and REWROTE THE MIRROR FROM ITS OWN VIEW. The pragma fence
#    cannot stop a stale-head seq collision or a rogue sync_mirror — only routing the
#    write through the ONE shared aberp_db::Handle does (its WriteGuard drop is the sole
#    sanctioned post-commit sync_mirror). 10L makes that class a RED BUILD:
#      • 10L-a (targeted): restore_from_nav_outgoing.rs::append_backfill_cycle_entry (the
#        boot re-fork) + incoming_invoices.rs::change_status must contain NO independent
#        opener, NO direct sync_mirror, and MUST route through `.write()`.
#      • 10L-b (freeze): the mirror-fork co-occurrence set (opener + sync_mirror in one
#        runtime fn; same scope/skip as 10i) is frozen in
#        tools/adr0098_r7_mirror_fork_sites.txt and may only SHRINK — a NEW or REGROWN
#        fork-capable site fails the build. Teeth: cut_gate_negative_probes.sh
#        "[CHECK 10L]" replants a raw opener+sync_mirror in a migrated seam.
#    ENFORCE_MIRROR_FORK=0 disables it for a deliberate, temporary local probe only.
echo "[CHECK 10L] ADR-0098 R7 — boot re-fork class (independent opener + rogue sync_mirror) frozen; migrated write seams stay Handle-bound (ENFORCED · D5)"
enforce10L="${ENFORCE_MIRROR_FORK:-1}"
flag10L() { note "$1"; if [[ "$enforce10L" == "1" ]]; then fail=1; else note "  (enforcement disabled — not failing)"; fi; }
mff_scan="tools/adr0098_r7_mirror_fork_scan.awk"
mff_manifest="tools/adr0098_r7_mirror_fork_sites.txt"
r7_c2_set=" apps/aberp/src/ap_sync.rs apps/aberp/src/poll_ack.rs apps/aberp/src/issue_invoice.rs apps/aberp/src/issue_storno.rs apps/aberp/src/issue_modification.rs apps/aberp/src/submit_invoice.rs apps/aberp/src/mark_invoice_paid.rs apps/aberp/src/drain_submission_queue.rs apps/aberp/src/retry_submission.rs apps/aberp/src/submit_annulment.rs apps/aberp/src/poll_annulment_ack.rs apps/aberp/src/observe_receiver_confirmation.rs apps/aberp/src/recover_from_nav.rs apps/aberp/src/mark_abandoned.rs apps/aberp/src/request_technical_annulment.rs "
if [[ ! -f "$mff_scan" || ! -f "$mff_manifest" ]]; then
  flag10L "✗ R7 mirror-fork scanner or frozen manifest missing: $mff_scan / $mff_manifest"
else
  # 10L-b — the frozen mirror-fork set may only SHRINK; a new/regrown site is RED.
  r7_cur="$(mktemp "${TMPDIR:-/tmp}/mff_cur.XXXXXX")"
  r7_froz="$(mktemp "${TMPDIR:-/tmp}/mff_froz.XXXXXX")"
  while IFS= read -r f; do
    case " $r7_c2_set " in *" $f "*) continue;; esac
    case "$f" in crates/aberp-db/*|crates/aberp-snapshot/*) continue;; esac
    awk -f "$mff_scan" "$f" 2>/dev/null
  done < <(find apps/aberp/src modules crates -name '*.rs' | grep -vE '/tests/' | sort) | sort > "$r7_cur"
  # `|| true`: an EMPTY manifest is a legitimate state (D-22 emptied this one)
  # and `grep -v` exits 1 when it matches nothing. Without the guard,
  # `set -euo pipefail` KILLS the whole gate right here — 10i, 10j, 10k, 10M,
  # 10P and 10N never run, and the exit-1 looks like an ordinary failure. Same
  # guard 10M and 10N already carry on their own (already-empty) manifests.
  grep -vE '^#' "$mff_manifest" | sort > "$r7_froz" || true
  r7_grew="$(comm -13 "$r7_froz" "$r7_cur")"
  if [[ -n "$r7_grew" ]]; then
    flag10L "✗ a NEW/REGROWN independent-opener + sync_mirror (write-fork) site appeared — route it through the shared Handle (ADR-0098 R7 regression):"
    printf '%s\n' "$r7_grew" | sed 's/^/      /'
  fi
  r7_shrunk="$(comm -23 "$r7_froz" "$r7_cur")"
  if [[ -n "$r7_shrunk" ]]; then
    note "  (info) mirror-fork sites migrated off since freeze — refresh $mff_manifest to lock the smaller set:"
    printf '%s\n' "$r7_shrunk" | sed 's/^/      /'
  fi
  if [[ -z "$r7_grew" ]]; then note "✓ mirror-fork set has not grown ($(grep -vcE '^#' "$mff_manifest" || true) frozen fork-capable sites; D-22 emptied it — ANY site the scanner reports now is new)"; fi
  rm -f "$r7_cur" "$r7_froz"

  # 10L-a — the two seams migrated THIS session (ADR-0098 R7) must stay opener-free +
  #   sync_mirror-free (mirror now via the Handle WriteGuard drop) and .write()-routed.
  r7_extract="$(mktemp "${TMPDIR:-/tmp}/r7_extract_fn.XXXXXX.awk")"
  cat > "$r7_extract" <<'R7_FN_AWK'
BEGIN{ depth=0; inblk=0; instr=0; armed=0; bodydepth=-1;
       re="^[ \t]*(pub(\\([^)]*\\))?[ \t]+)?(async[ \t]+)?(unsafe[ \t]+)?fn[ \t]+" fn "[ (<]" }
{
  line=$0
  if (!armed && bodydepth<0 && line ~ re) armed=1
  printed=0
  code=""; L=length(line)
  for(i=1;i<=L;i++){
    c=substr(line,i,1); d=substr(line,i,2)
    if(inblk){ if(d=="*/"){inblk=0;i++} ; continue }
    if(instr){ if(c=="\\"){i++;continue} ; if(c=="\""){instr=0} ; continue }
    if(d=="//"){ break }
    if(d=="/*"){ inblk=1;i++;continue }
    if(c=="\""){ instr=1; continue }
    code=code c
    if(c=="{"){ depth++; if(armed && bodydepth<0) bodydepth=depth }
    else if(c=="}"){ if(armed && bodydepth>=0 && depth==bodydepth){ print code; printed=1; armed=0; bodydepth=-1 } depth-- }
  }
  if(armed && bodydepth>=0 && !printed) print code
}
R7_FN_AWK
  r7_check_seam() { # $1 file  $2 fnname
    local file="$1" fn="$2" body opn mir
    if [[ ! -f "$file" ]]; then flag10L "✗ R7 seam file missing: $file"; return; fi
    body="$(awk -v fn="$fn" -f "$r7_extract" "$file")"
    if [[ -z "$body" ]]; then flag10L "✗ R7 seam fn not found: $file::$fn (renamed? update CHECK 10L)"; return; fi
    opn="$(printf '%s\n' "$body" | grep -nE '(Connection::open(_with_flags)?|Ledger::open|DuckDbBillingStore::open|Database::open)\(|append_reopen[[:space:]]*\(' | grep -vE 'open_in_memory' || true)"
    if [[ -n "$opn" ]]; then
      flag10L "✗ $file::$fn REGREW an independent live-DB opener — the ADR-0098 R7 boot re-fork seam must stay on the shared Handle (db.write()):"
      printf '%s\n' "$opn" | sed 's/^/      /'
    fi
    # ROUND 6 — `sync_mirror` PREFIX, matching CHECK 10L-b's r7 awk. Round 5 added
    # the `sync_mirror_lockstep` spelling; a bare `sync_mirror(` token here would
    # have let a rogue opener paired with the new name walk through the seam check.
    mir="$(printf '%s\n' "$body" | grep -nE 'sync_mirror[A-Za-z0-9_]*[[:space:]]*\(' || true)"
    if [[ -n "$mir" ]]; then
      flag10L "✗ $file::$fn REGREW a direct sync_mirror — the post-commit mirror must come from the Handle WriteGuard drop, never a separate opener (ADR-0098 R7):"
      printf '%s\n' "$mir" | sed 's/^/      /'
    fi
    if ! printf '%s\n' "$body" | grep -qE '\.write\(\)'; then
      flag10L "✗ $file::$fn no longer routes through the shared Handle writer (.write()) — ADR-0098 R7 migration reverted?"
    fi
    if [[ -z "$opn" && -z "$mir" ]]; then note "✓ $(basename "$file")::$fn — Handle-bound (no independent opener, no rogue sync_mirror; mirror via WriteGuard drop)"; fi
  }
  r7_check_seam "apps/aberp/src/restore_from_nav_outgoing.rs" "append_backfill_cycle_entry"
  r7_check_seam "apps/aberp/src/incoming_invoices.rs" "change_status"
  rm -f "$r7_extract"
fi

# ── CHECK 10i — ADR-0098 Session C2: the FROZEN residual-opener ledger. Every
#    runtime independent opener NOT on the Handle is accounted for in
#    tools/adr0098_c2_frozen_residuals.txt (operator-paced ERP modules + CLI
#    one-shots + serve.rs request-handler audit reads). Each file's count is
#    FROZEN: it may not EXCEED its listed count, and no NEW opener-bearing file
#    may appear unlisted. This is what makes a deferred-to-v0.2.6 surface SAFE
#    (it cannot silently grow) and what keeps a green D5 from ever again meaning
#    "blind to most of the openers" (review F1-F4). Toolchain-free (awk).
echo "[CHECK 10i] ADR-0098 Session C2 (+ R4 finding H·a: scope now includes crates/) — frozen residual-opener ledger (operator/CLI/serve/crates cannot grow; ENFORCED · D5)"
manifest="tools/adr0098_c2_frozen_residuals.txt"
if [[ ! -f "$manifest" ]]; then
  flag10 "✗ frozen-residual manifest missing: $manifest"
elif [[ ! -f "$scan" ]]; then
  : # already flagged
else
  c2_set=" apps/aberp/src/ap_sync.rs apps/aberp/src/poll_ack.rs apps/aberp/src/issue_invoice.rs apps/aberp/src/issue_storno.rs apps/aberp/src/issue_modification.rs apps/aberp/src/submit_invoice.rs apps/aberp/src/mark_invoice_paid.rs apps/aberp/src/drain_submission_queue.rs apps/aberp/src/retry_submission.rs apps/aberp/src/submit_annulment.rs apps/aberp/src/poll_annulment_ack.rs apps/aberp/src/observe_receiver_confirmation.rs apps/aberp/src/recover_from_nav.rs apps/aberp/src/mark_abandoned.rs apps/aberp/src/request_technical_annulment.rs "
  resid_fail=0
  while IFS= read -r f; do
    case " $c2_set " in *" $f "*) continue;; esac
    case "$f" in crates/aberp-db/*|crates/aberp-snapshot/*) continue;; esac
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
  done < <(find apps/aberp/src modules crates -name '*.rs' | grep -vE '/tests/' | sort)
  if [[ "$resid_fail" == "0" ]]; then
    # `|| true` (x2): an ALL-COMMENT manifest is a legitimate terminal state —
    # it is what "every residual opener has been migrated onto the Handle"
    # looks like. `grep -v`/`grep -vc` exit 1 on zero matches, and under
    # `set -euo pipefail` a bare assignment from such a command substitution
    # (pipefail carries the failure out of the awk pipe) KILLS the whole gate
    # right here — a hard RED, mid-run, so CHECK 10j/10k/10M/10N/10P below
    # never execute at all. Same bug as the 10L-b manifest (D-22, line ~729)
    # and the 10k baseline read below; fixed in all three places together.
    ft="$( { grep -vE '^#' "$manifest" || true; } | awk '{s+=$1} END{print s+0}')"
    ff="$(grep -vcE '^#' "$manifest" || true)"
    note "✓ frozen residual ledger holds — no file exceeds its frozen count, no new unlisted opener ($ft frozen openers across $ff files; v0.2.6 migration target)"
  fi
fi

# ── CHECK 10j — ADR-0098 R3 (finding C): every FROZEN residual opener carries the
#    no-in-place-fold pragma. R3 closes the swap-orphan silent-write-loss vector two
#    ways: (Part 2) it MIGRATES the daemon-frequency ap_sync ingest seam
#    (incoming_invoices::ingest_incoming_invoice + submission_queue::count_pending)
#    onto the Handle; and (Part 1) it gives EVERY remaining residual runtime opener
#    `PRAGMA disable_checkpoint_on_shutdown` so a residual open→close can never fold
#    the shared WAL in place (duckdb#23046) while the Handle's instance is open.
#    10i freezes the COUNT of residual openers; 10j freezes their SAFETY — a frozen
#    opener that silently drops the guard is a RED BUILD. The central openers
#    (audit-ledger Ledger::open + DuckDbBillingStore::open) carry it once for all
#    their callers; every RAW residual Connection::open must carry it within a short
#    window after the open (cfg(test)/comment/string-aware via the shared scanner;
#    Ledger::open / DuckDbBillingStore::open openers are covered centrally, not
#    per-site). Teeth: cut_gate_negative_probes.sh "[CHECK 10j]" strips the pragma
#    from a frozen opener and asserts this goes red. ENFORCE_RESIDUAL_PRAGMA=0
#    disables it for a deliberate, temporary local probe only.
echo "[CHECK 10j] ADR-0098 R3 (finding C) + R6 (NEW-3): scope now includes crates/ — every frozen residual opener carries PRAGMA disable_checkpoint_on_shutdown (no silent fold-on-close; ENFORCED · D5)"
enforce10j="${ENFORCE_RESIDUAL_PRAGMA:-1}"
flag10j() { note "$1"; if [[ "$enforce10j" == "1" ]]; then fail=1; else note "  (enforcement disabled — not failing)"; fi; }
PRAGMA='disable_checkpoint_on_shutdown'
WINDOW=15
if [[ ! -f "$scan" || ! -f "$manifest" ]]; then
  flag10j "✗ opener scanner or frozen manifest missing (10h/10i already flagged)"
else
  # (a) the two CENTRAL openers carry the pragma once for all their callers.
  # R6 (NEW-3): storage/mod.rs now holds TWO openers (Ledger::open at
  # `Connection::open(path)` + the append_reopen path at `Connection::open(db_path)`,
  # the latter pragma-guarded in R6). A whole-file grep could no longer tell which
  # opener carried the pragma, so check the pragma sits within $WINDOW lines of
  # Ledger::open's OWN opener line specifically — a second pragma elsewhere in the
  # file can never again mask a Ledger::open removal.
  led="crates/audit-ledger/src/storage/mod.rs"
  led_open_ln="$(grep -nE 'Connection::open\(path\)' "$led" | head -1 | cut -d: -f1)"
  if [[ -n "$led_open_ln" ]] && sed -n "${led_open_ln},$((led_open_ln+WINDOW))p" "$led" | grep -q "$PRAGMA"; then
    note "✓ audit-ledger Ledger::open carries $PRAGMA (covers its ~145 residual callers)"
  else
    flag10j "✗ Ledger::open ($led) missing $PRAGMA — its residual callers would fold-on-close in place"
  fi
  bs="modules/billing/src/adapters/duckdb_store.rs"
  if grep -q "$PRAGMA" "$bs"; then
    note "✓ DuckDbBillingStore::open carries $PRAGMA (covers its residual callers)"
  else
    flag10j "✗ DuckDbBillingStore::open ($bs) missing $PRAGMA — its residual callers would fold-on-close in place"
  fi
  # (b) every RAW residual Connection::open must carry the pragma within WINDOW
  #     lines. Ledger::open / DuckDbBillingStore::open openers are covered by (a).
  pragma_fail=0
  while IFS= read -r f; do
    case " $c2_set " in *" $f "*) continue;; esac
    case "$f" in crates/aberp-db/*|crates/aberp-snapshot/*) continue;; esac
    frozen="$(awk -v p="$f" '$1!="#" && $2==p{print $1}' "$manifest")"
    [[ -z "$frozen" ]] && continue
    if [[ "$f" == "apps/aberp/src/serve.rs" ]]; then
      openers="$(awk -v allow="run,seed_demo_sample_data" -f "$scan" "$f" 2>/dev/null)"
    else
      openers="$(awk -f "$scan" "$f" 2>/dev/null)"
    fi
    while IFS= read -r rec; do
      [[ -z "$rec" ]] && continue
      case "$rec" in *Connection::open*) : ;; *) continue;; esac
      ln="${rec%%:*}"
      if ! sed -n "${ln},$((ln+WINDOW))p" "$f" | grep -q "$PRAGMA"; then
        flag10j "✗ $f:$ln — residual Connection::open has NO $PRAGMA within $WINDOW lines (silent fold-on-close risk)"
        pragma_fail=1
      fi
    done <<< "$openers"
  done < <(find apps/aberp/src modules crates -name '*.rs' | grep -vE '/tests/' | sort)
  if [[ "$pragma_fail" == "0" ]]; then
    note "✓ every frozen residual Connection::open carries $PRAGMA within $WINDOW lines (no silent close-checkpoint fold)"
  fi
fi

# ── CHECK 10k — ADR-0098 R4 (finding H·c): per-opener FINGERPRINT freeze. 10i freezes
#    the COUNT of residual openers per file; a raw count cannot see an intra-file SWAP
#    (drop 3 legit openers + add 3 different ones = same count, gate stays green). 10k
#    freezes the SET of per-opener fingerprints (<file>|<fname>:<opener-text>; line
#    numbers dropped so a benign line-shift doesn't churn it) across the SAME extended
#    scope as 10i (apps/aberp + modules + crates, minus the sanctioned aberp-db /
#    aberp-snapshot seams + the 7 C2-migrated files). Any add, removal, or content change
#    of an opener flips the set -> RED. 10i (count) stays as the coarse backstop; 10k is
#    the precise one. Teeth: cut_gate_negative_probes.sh "[CHECK 10k]" does a count-
#    preserving swap. ENFORCE_OPENER_FINGERPRINTS=0 disables it for a deliberate probe.
echo "[CHECK 10k] ADR-0098 R4 (finding H·c) — per-opener fingerprint freeze (intra-file swap cannot hide; ENFORCED · D5)"
enforce10k="${ENFORCE_OPENER_FINGERPRINTS:-1}"
flag10k() { note "$1"; if [[ "$enforce10k" == "1" ]]; then fail=1; else note "  (enforcement disabled — not failing)"; fi; }
fpfile="tools/adr0098_r4_opener_fingerprints.txt"
if [[ ! -f "$scan" || ! -f "$fpfile" ]]; then
  flag10k "✗ opener scanner or fingerprint manifest missing: $fpfile"
else
  cur="$(mktemp)"; froz="$(mktemp)"
  while IFS= read -r f; do
    case " $c2_set " in *" $f "*) continue;; esac
    case "$f" in crates/aberp-db/*|crates/aberp-snapshot/*) continue;; esac
    if [[ "$f" == "apps/aberp/src/serve.rs" ]]; then
      sigs="$(awk -v allow="run,seed_demo_sample_data" -f "$scan" "$f" 2>/dev/null | sed 's/^[0-9]*://')"
    else
      sigs="$(awk -f "$scan" "$f" 2>/dev/null | sed 's/^[0-9]*://')"
    fi
    [[ -z "$sigs" ]] && continue
    while IFS= read -r sig; do printf '%s|%s\n' "$f" "$sig"; done <<< "$sigs"
  done < <(find apps/aberp/src modules crates -name '*.rs' | grep -vE '/tests/' | sort) | sort > "$cur"
  # `|| true`: an ALL-COMMENT fingerprint manifest is a legitimate terminal
  # state (no residual openers left to fingerprint). `grep -v` exits 1 on zero
  # matches and `pipefail` carries that out of the pipeline, so under `set -e`
  # this line KILLED the gate mid-run and CHECK 10M/10N/10P never ran. An empty
  # `$froz` is the correct baseline: the diff below then reports every current
  # opener as an addition, which is exactly right.
  { grep -vE '^#' "$fpfile" || true; } | sort > "$froz"
  if diff -q "$froz" "$cur" >/dev/null 2>&1; then
    note "✓ opener fingerprint set matches the frozen baseline ($(grep -vcE '^#' "$fpfile") openers; no add/remove/swap)"
  else
    flag10k "✗ opener fingerprint set DIVERGED from $fpfile (an opener was added, removed, or content-swapped — count-preserving swaps are caught here):"
    # `|| true`: keep this DIAGNOSTIC pipe from aborting the whole gate under
    # `set -euo pipefail` (diff exits non-zero when the sets differ). Without it
    # the script died here on any 10k divergence, so later checks (CHECK 10M) and
    # the final summary never ran (ADR-0099).
    { diff "$froz" "$cur" | sed 's/^/      /' | head -40; } || true
  fi
  rm -f "$cur" "$froz"
fi

# ── CHECK 10M — ADR-0099: the CORRECTED fork model. The seq-369/416/428/515
#    forks were NOT the narrow "independent opener + rogue sync_mirror in one fn"
#    class CHECK 10L froze — the seq-515 fork was the periodic snapshot daemon's
#    `snapshot.created` and the quote-intake daemon EACH opening an INDEPENDENT
#    Ledger on the live DB and self-assigning the next seq off the same stale
#    head, in the ONE `serve` process. A rogue sync_mirror is NOT required; the
#    TRUE fork primitive is ANY independent audit opener + append on the live DB
#    outside the shared aberp_db::Handle. 10i merely FROZE the COUNT of such
#    openers (a frozen fork is still a fork); 10L required a sync_mirror
#    co-occurrence it did not have. 10M closes both gaps:
#      • 10M-a (targeted, ZERO): the in-process seams MIGRATED this session — the
#        snapshot daemon+HTTP audit path (snapshot.rs) and the serve.rs request
#        handlers — must contain NO write-fork (independent opener + append). Any
#        regrowth is a RED build (they must stay on db.write()+append_in_tx).
#      • 10M-b (freeze, may-only-shrink): the remaining write-fork set (the
#        separate-process CLI one-shots + the one deferred in-process daemon,
#        restore_from_nav_outgoing::process_digest) is frozen in
#        tools/adr0099_write_fork_residuals.txt and may only SHRINK — a NEW or
#        REGROWN write-fork fails the build. This drives the surface to zero
#        (the v0.2.9 completion) while making the corrected model enforceable now.
#    Detector: tools/adr0099_write_fork_scan.awk (comment/string/cfg(test)-aware).
#    Teeth: cut_gate_negative_probes.sh "[CHECK 10M]" plants a raw
#    Ledger::open+append daemon in a migrated seam and asserts this goes red.
#    ENFORCE_WRITE_FORK=0 disables it for a deliberate, temporary local probe only.
echo "[CHECK 10M] ADR-0099 — corrected fork model: migrated in-process seams have ZERO write-fork; residual set frozen may-only-shrink (ENFORCED · D5)"
enforce10M="${ENFORCE_WRITE_FORK:-1}"
flag10M() { note "$1"; if [[ "$enforce10M" == "1" ]]; then fail=1; else note "  (enforcement disabled — not failing)"; fi; }
wf_scan="tools/adr0099_write_fork_scan.awk"
wf_manifest="tools/adr0099_write_fork_residuals.txt"
# Allow-list: pre-serve boot openers + the sanctioned separate-process/primitive
# seams (append_reopen is the audit-ledger reopen primitive; emit_reopen_cli is
# snapshot.rs's separate-process CLI reopen — no Handle in that process;
# emit_tenant_reopen is tenant_registry.rs's sanctioned reopen for a DB with NO
# shared Handle in this process — a FOREIGN tenant's DB (create/demo-seed writes
# a different file than the booted Handle owns) or a PRE-Handle boot append
# (record_tenant_boot runs before open_tenant_handle) — neither can fork the
# serve writer, exactly as emit_reopen_cli).
WF_ALLOW="run,seed_demo_sample_data,record_upgrade_snapshot_mismatch_audit,emit_reopen_cli,append_reopen,emit_tenant_reopen"
if [[ ! -f "$wf_scan" || ! -f "$wf_manifest" ]]; then
  flag10M "✗ write-fork scanner or frozen manifest missing: $wf_scan / $wf_manifest"
else
  # 10M-a — the MIGRATED in-process seams must stay at ZERO write-fork.
  serve_wf="$(awk -v allow="run,seed_demo_sample_data,record_upgrade_snapshot_mismatch_audit" -f "$wf_scan" apps/aberp/src/serve.rs 2>/dev/null || true)"
  snap_wf="$(awk -v allow="emit_reopen_cli" -f "$wf_scan" apps/aberp/src/snapshot.rs 2>/dev/null || true)"
  if [[ -n "$serve_wf" ]]; then
    flag10M "✗ serve.rs REGREW an in-process write-fork (independent opener + append) — route it through the shared Handle (db.write()+append_in_tx), ADR-0099:"
    printf '%s\n' "$serve_wf" | sed 's/^/      /'
  else
    note "✓ serve.rs request handlers — no in-process write-fork (all audit appends on the shared Handle)"
  fi
  if [[ -n "$snap_wf" ]]; then
    flag10M "✗ snapshot.rs REGREW an in-process write-fork — the daemon+HTTP audit path must stay on the shared Handle (only emit_reopen_cli, the CLI reopen, is allow-listed), ADR-0099:"
    printf '%s\n' "$snap_wf" | sed 's/^/      /'
  else
    note "✓ snapshot.rs daemon+HTTP audit path — no in-process write-fork (seq-515 racer on the shared Handle)"
  fi

  # 10M-b — the frozen residual set may only SHRINK.
  wf_cur="$(mktemp "${TMPDIR:-/tmp}/wf_cur.XXXXXX")"
  wf_froz="$(mktemp "${TMPDIR:-/tmp}/wf_froz.XXXXXX")"
  while IFS= read -r f; do
    case "$f" in crates/aberp-db/*|crates/aberp-snapshot/*) continue;; esac
    awk -v allow="$WF_ALLOW" -f "$wf_scan" "$f" 2>/dev/null | while IFS=: read -r ln fn rest; do
      printf '%s:%s\n' "$f" "$fn"
    done
  done < <(find apps/aberp/src modules crates -name '*.rs' | grep -vE '/tests/' | sort) | sort -u > "$wf_cur"
  # `|| true`: the residual set is now EMPTY (ADR-0099 complete). With an
  # all-comment manifest the `grep -vE '^#'` matches nothing (exit 1) which,
  # under `set -euo pipefail`, would abort the gate at the SUCCESS state; the
  # guard keeps ZERO residuals a clean PASS. A regrown fork still trips 10M-b
  # below (it appears in wf_cur, absent from wf_froz -> wf_grew non-empty).
  grep -vE '^#' "$wf_manifest" | sed 's/[[:space:]]*#.*$//;s/[[:space:]]*$//' | grep -vE '^$' | sort -u > "$wf_froz" || true
  wf_grew="$(comm -13 "$wf_froz" "$wf_cur")"
  if [[ -n "$wf_grew" ]]; then
    flag10M "✗ a NEW/REGROWN write-fork (independent opener + append) appeared outside the frozen set — route it through the shared aberp_db::Handle (ADR-0099 regression):"
    printf '%s\n' "$wf_grew" | sed 's/^/      /'
  fi
  wf_shrunk="$(comm -23 "$wf_froz" "$wf_cur")"
  if [[ -n "$wf_shrunk" ]]; then
    note "  (info) write-fork sites migrated off since freeze — refresh $wf_manifest to lock the smaller set:"
    printf '%s\n' "$wf_shrunk" | sed 's/^/      /'
  fi
  if [[ -z "$wf_grew" ]]; then
    wf_n="$(grep -vcE '^#' "$wf_manifest" || true)"
    note "✓ frozen write-fork residual holds (${wf_n:-0} sites; ADR-0099 COMPLETE — every in-process aberp-serve audit write-fork is on the shared Handle; a NEW/REGROWN fork anywhere fails here)"
  fi
  rm -f "$wf_cur" "$wf_froz"
fi

# ── CHECK 10P — ADR-0099 R2: AUDIT-WRITER PROVENANCE. CHECK 10M/10N fire on
#    `independent opener AND audit-table append`. That predicate let the class
#    recur a FIFTH time (mirror-side, seq 2508, two duplicated quote-intake
#    heartbeats), because it has four blind spots:
#      B1 `Handle::read()` is not in their opener set, yet it hands back a
#         WRITABLE `Connection` holding neither the writer mutex nor
#         AUDIT_APPEND_LOCK — a second audit writer by any other name.
#      B2 a fn that appends on a `&Transaction`/`&mut Ledger`/`&mut Connection`
#         PARAMETER has no opener of its own, so it scores clean whatever its
#         caller opened (the qc_inspection split fork, found by hand).
#      B3 they are BAN-lists: a provenance they do not already know is silently
#         clean.
#      B4 they only count appends to the audit TABLE. The `<db>.audit.log`
#         MIRROR is the ledger's other half and its writers (`sync_mirror`,
#         `ensure_consistent_with_db`, `replay_mirror_delta`) were not in the
#         append set at all — which is exactly where the recurrence lived.
#         10M/10N also EXCLUDE crates/aberp-db and crates/aberp-snapshot from
#         their corpus, and the snapshot daemon's reconciler is in the second
#         of those.
#    10P inverts the predicate: it fires on the WRITE — table or mirror — and
#    demands each site PROVE its serialization domain (shared `.write()` guard,
#    `with_ledger`, an AUDIT_APPEND_LOCK-holding `Ledger` api, or a
#    caller-owned tx). A site it cannot classify is RED, so there is no
#    "not on the ban-list" escape. `TX_PARAM` sites are not trusted on faith:
#    the driver iterates the scanner to a FIXPOINT, re-running it with each
#    pass's caller-owned-tx fns treated as writes, so a fn that writes only by
#    handing its connection to a helper is classified by its OWN provenance.
#    Corpus is the WHOLE workspace, aberp-db and aberp-snapshot included.
#    Detector: tools/adr0099_audit_writer_scan.awk (comment/string/
#    cfg(test)-aware, STATEMENT-scoped so a multi-line `let mut conn = db\n
#    .write()?` is read as one statement — cf. ADR-0105 F1's line-scoped
#    laundering).
#    Teeth: cut_gate_negative_probes.sh "[CHECK 10P]" plants a daemon heartbeat
#    appending on a `db.read()` clone, an independent mirror writer, and a
#    split (helper-parameter) fork, and asserts each goes RED.
#    ENFORCE_AUDIT_WRITER=0 disables it for a deliberate, temporary local probe.
echo "[CHECK 10P] ADR-0099 R2 — audit-writer provenance: every table/mirror write proves its serialization domain; residual frozen (ENFORCED · D5)"
enforce10P="${ENFORCE_AUDIT_WRITER:-1}"
flag10P() { note "$1"; if [[ "$enforce10P" == "1" ]]; then fail=1; else note "  (enforcement disabled — not failing)"; fi; }
aw_scan="tools/adr0099_audit_writer_scan.awk"
aw_manifest="tools/adr0099_audit_writer_residuals.txt"
if [[ ! -f "$aw_scan" || ! -f "$aw_manifest" ]]; then
  flag10P "✗ audit-writer scanner or frozen residual missing: $aw_scan / $aw_manifest"
else
  aw_files="$(mktemp "${TMPDIR:-/tmp}/aw_files.XXXXXX")"
  find apps/aberp/src modules crates -name '*.rs' 2>/dev/null | grep -vE '/tests/' | sort > "$aw_files"

  # 10P-0 — MATCHER LIVENESS (always enforced). A gate whose scanner silently
  # stops matching reports "no violations" — the most dangerous green there is.
  # Three fixtures pin the three verdicts the gate actually rests on.
  aw_probe="$(mktemp "${TMPDIR:-/tmp}/aw_probe.XXXXXX")"
  printf 'fn h(db: &Db) {\n    let mut c = db\n        .read()\n        .unwrap();\n    let tx = c.transaction().unwrap();\n    append_in_tx(&tx, &m, k, p, a, None).unwrap();\n}\n' > "$aw_probe"
  awk -f "$aw_scan" "$aw_probe" | grep -q 'READ_CLONE' \
    || { flag10P "✗ HARNESS: scanner no longer verdicts a db.read()+append fn as READ_CLONE (blind spot B1 reopened)"; }
  printf 'fn h(db: &Db) {\n    let mut c = db\n        .write()\n        .unwrap();\n    let tx = c.transaction().unwrap();\n    append_in_tx(&tx, &m, k, p, a, None).unwrap();\n}\n' > "$aw_probe"
  awk -f "$aw_scan" "$aw_probe" | grep -q 'HANDLE_WRITE' \
    || { flag10P "✗ HARNESS: scanner no longer verdicts a db.write()+append fn as HANDLE_WRITE — it would cry wolf on the whole tree"; }
  printf '/// let mut c = Connection::open(p);\nfn h(tx: &Transaction) {\n    // Connection::open(p)\n    append_in_tx(tx, &m, k, p, a, None).unwrap();\n}\n' > "$aw_probe"
  awk -f "$aw_scan" "$aw_probe" | grep -q 'TX_PARAM' \
    || { flag10P "✗ HARNESS: scanner counted a doc/line comment as an opener, or lost caller-owned-tx detection"; }
  printf 'fn h(db: &Db) {\n    let mut l = Ledger::open(p, t, b).unwrap();\n    l.append(k, p, a, None).unwrap();\n}\n' > "$aw_probe"
  awk -f "$aw_scan" "$aw_probe" | grep -q 'INDEP_OPENER' \
    || { flag10P "✗ HARNESS: scanner no longer verdicts an independent Ledger::open+append as INDEP_OPENER"; }
  printf 'fn h(db: &Db) {\n    let mut l = Ledger::open(p, t, b).unwrap();\n    l.sync_mirror(&mp).unwrap();\n}\n' > "$aw_probe"
  awk -f "$aw_scan" "$aw_probe" | grep -q 'INDEP_OPENER' \
    || { flag10P "✗ HARNESS: scanner no longer treats a MIRROR write as a ledger write (blind spot B4 reopened)"; }
  # ROUND 6 — the mirror token must match EVERY spelling of the entry point, not
  # just the bare one. Round 5 split the per-commit path into `sync_mirror_lockstep`
  # and this scanner's narrow `sync_mirror(` token silently stopped matching it, so
  # aberp-db's WriteGuard::drop produced no record at all and 10P-2 offered its live
  # residual up for deletion. A rename must never be able to quiet the gate.
  printf 'fn h(db: &Db) {\n    let mut l = Ledger::open(p, t, b).unwrap();\n    l.sync_mirror_lockstep(&mp).unwrap();\n}\n' > "$aw_probe"
  awk -f "$aw_scan" "$aw_probe" | grep -q 'INDEP_OPENER' \
    || { flag10P "✗ HARNESS: scanner no longer matches the sync_mirror_lockstep spelling of the MIRROR write — the mirror token is name-keyed again (round 6; blind spot B4 reopened by rename)"; }
  rm -f "$aw_probe"

  # 10P-1 — iterate to a FIXPOINT over the caller-owned-tx set (blind spot B2).
  aw_out="$(mktemp "${TMPDIR:-/tmp}/aw_out.XXXXXX")"
  aw_taint=""
  for _pass in 1 2 3 4 5 6; do
    # ONE awk process over the corpus per pass. The scanner resets its
    # positional state at FNR==1 and attributes each record to the file the fn
    # was DECLARED in, so multi-file is byte-identical to per-file — and a
    # fixpoint over ~400 files stops costing ~400 process spawns per pass.
    #
    # PREFILTER (sound, not a sample): a file can only produce a record if it
    # contains a ledger-write token, or calls one of THIS PASS's tainted fns.
    # Recomputed every pass from `$aw_taint`, so growing the taint set grows the
    # file set — dropping the filter must not change the output, and CHECK 10P-0
    # + 10P-3 are what catch it if a future edit makes it lossy.
    : > "$aw_out"
    aw_re='append_in_tx|[.]append|append_reopen|sync_mirror|ensure_consistent_with_db|replay_mirror_delta'
    [[ -n "$aw_taint" ]] && aw_re="$aw_re|$(printf '%s' "$aw_taint" | tr ',' '|')"
    aw_hits="$(mktemp "${TMPDIR:-/tmp}/aw_hits.XXXXXX")"
    xargs grep -lE "$aw_re" < "$aw_files" > "$aw_hits" 2>/dev/null || true
    if [[ -s "$aw_hits" ]]; then
      xargs awk -v taint="$aw_taint" -f "$aw_scan" < "$aw_hits" >> "$aw_out" 2>/dev/null
    fi
    rm -f "$aw_hits"
    aw_next="$(grep ':TX_PARAM@' "$aw_out" | sed 's/^[^:]*:\([^:]*\):.*/\1/' | sort -u | paste -sd, -)"
    [[ "$aw_next" == "$aw_taint" ]] && break
    aw_taint="$aw_next"
  done
  note "  (taint fixpoint over ${#aw_taint} bytes of caller-owned-tx fn names)"

  # 10P-2 — every unclassifiable / non-shared writer must be on the frozen list.
  aw_cur="$(mktemp "${TMPDIR:-/tmp}/aw_cur.XXXXXX")"
  aw_froz="$(mktemp "${TMPDIR:-/tmp}/aw_froz.XXXXXX")"
  grep -E ':(READ_CLONE|INDEP_OPENER|UNCLASSIFIED)@' "$aw_out" \
    | sed -E 's/@L[0-9]+$//; s/:(READ_CLONE|INDEP_OPENER|UNCLASSIFIED)$//' \
    | sort -u > "$aw_cur" || true
  grep -vE '^#' "$aw_manifest" | sed 's/[[:space:]]*#.*$//;s/[[:space:]]*$//' \
    | grep -vE '^$' | sort -u > "$aw_froz" || true
  aw_grew="$(comm -13 "$aw_froz" "$aw_cur")"
  if [[ -n "$aw_grew" ]]; then
    flag10P "✗ a NON-SHARED audit writer appeared outside the frozen residual — route it through the shared aberp_db::Handle writer (ADR-0099 R2). Verdicts: READ_CLONE = appends on a db.read() clone; INDEP_OPENER = its own Connection/Ledger; UNCLASSIFIED = provenance not provable:"
    printf '%s\n' "$aw_grew" | sed 's/^/      /'
    grep -E ':(READ_CLONE|INDEP_OPENER|UNCLASSIFIED)@' "$aw_out" \
      | grep -Ff <(printf '%s\n' "$aw_grew") | sed 's/^/        /' || true
  fi
  aw_shrunk="$(comm -23 "$aw_froz" "$aw_cur")"
  if [[ -n "$aw_shrunk" ]]; then
    note "  (info) non-shared audit writers migrated off since freeze — refresh $aw_manifest to lock the smaller set:"
    printf '%s\n' "$aw_shrunk" | sed 's/^/      /'
  fi
  # 10P-3 — corpus liveness. A `find` that matches nothing, or a scanner that
  # emits nothing, makes 10P-2 vacuously green. The tree has ~50 shared-Handle
  # writers; require the classification to be non-trivial in the SAFE direction
  # too, so "no violations" can never mean "no scan".
  aw_ok="$(grep -cE ':(HANDLE_WRITE|WITH_LEDGER)@' "$aw_out" || true)"
  if [[ "${aw_ok:-0}" -lt 20 ]]; then
    flag10P "✗ HARNESS: only ${aw_ok:-0} shared-Handle audit writers classified across the workspace — the corpus or the scanner is broken, so a green 10P-2 means nothing"
  fi
  if [[ -z "$aw_grew" ]]; then
    aw_n="$(grep -vcE '^#|^$' "$aw_manifest" || true)"
    note "✓ audit-writer provenance holds (${aw_ok:-0} writes on the shared Handle; ${aw_n:-0} frozen sanctioned non-shared writers; a NEW one anywhere — table OR mirror, aberp-db and aberp-snapshot included — fails here)"
  fi
  rm -f "$aw_cur" "$aw_froz" "$aw_out" "$aw_files"
fi

# ── CHECK 10N — ADR-0105: the WRAPPER-HIDDEN write-fork. CHECK 10M above is a
#    per-FUNCTION scan: it fires only when the independent opener token and the
#    audit-append token appear in the SAME fn body. That model is structurally
#    blind to the most common real shape — the opener in one fn, the append one
#    (or N) calls away in a helper:
#
#        fn tick()  { let mut l = Ledger::open(..); write_event(&mut l, ..); }
#        fn write_event(l: &mut Ledger, ..) { l.append_signed(..) }
#
#    Neither fn trips 10M. This is not hypothetical: the pre-PR-33 aberp-mes
#    writer had exactly this shape (`write_mes_adapter_event`) and scanned CLEAN
#    while forking the chain in production, and on main @ 9723df3 BOTH DÁP audit
#    writers were invisible to 10M — one of them inside serve.rs, where 10M-a
#    demands a hard ZERO and was passing. The ADR-0099 manifest already recorded
#    a third instance of the same miss (qc_inspection::record_manual_inspection).
#
#    10N does NOT replace or relax 10M — 10M keeps its exact semantics and its
#    own frozen manifest. 10N adds the transitive teeth:
#      • detector: tools/adr0105_wrapper_fork_scan.awk — a whole-program taint
#        closure over fn DEFINITIONS (crossing crate boundaries, since the live
#        case is apps/aberp calling into crates/audit-ledger), seeded at the
#        append primitives, resolved same-file-first, stopping at any callee that
#        takes the shared Handle itself (that IS the serialization point), and
#        requiring the OPENED VALUE to actually reach the tainted callee.
#      • 10N-a: DIRECT records must stay at ZERO (they are 10M's own class; a
#        DIRECT hit here means 10M and 10N disagree — a harness fault).
#      • 10N-b: the TRANSITIVE/AMBIGUOUS set is frozen in
#        tools/adr0105_wrapper_fork_residuals.txt and may only SHRINK.
#      • a non-converged taint closure (exit 3) is a HARNESS FAULT, never a pass.
#    Teeth: cut_gate_negative_probes.sh "[CHECK 10N]" reintroduces the pre-PR-33
#    wrapper-hidden fork and asserts this goes red (and that 10M does NOT — the
#    probe pins the blind spot itself, so a future 10M that also catches it is a
#    visible, deliberate change rather than a silent one).
#    ENFORCE_WRAPPER_FORK=0 disables it for a deliberate, temporary local probe only.
echo "[CHECK 10N] ADR-0105 — wrapper-hidden write-fork: transitive taint closure, DIRECT==0 + frozen residual may-only-shrink (ENFORCED · D5)"
enforce10N="${ENFORCE_WRAPPER_FORK:-1}"
flag10N() { note "$1"; if [[ "$enforce10N" == "1" ]]; then fail=1; else note "  (enforcement disabled — not failing)"; fi; }
tf_scan="tools/adr0105_wrapper_fork_scan.awk"
tf_manifest="tools/adr0105_wrapper_fork_residuals.txt"
# 10N deliberately shares CHECK 10M's sanctioned-fn set: same fork model, same
# pre-serve boot openers and separate-process CLI seams. `:?` rather than a
# duplicated literal — a copy would drift, and if the checks are ever reordered
# so 10N runs first this aborts loudly instead of silently scanning with an
# EMPTY allow-list (which would flag every boot opener and look like a code
# regression rather than a harness fault).
: "${WF_ALLOW:?CHECK 10N needs WF_ALLOW, defined by CHECK 10M — check ordering}"
if [[ ! -f "$tf_scan" || ! -f "$tf_manifest" ]]; then
  flag10N "✗ wrapper-fork scanner or frozen manifest missing: $tf_scan / $tf_manifest"
else
  tf_files="$(mktemp "${TMPDIR:-/tmp}/tf_files.XXXXXX")"
  tf_raw="$(mktemp "${TMPDIR:-/tmp}/tf_raw.XXXXXX")"
  tf_cur="$(mktemp "${TMPDIR:-/tmp}/tf_cur.XXXXXX")"
  tf_froz="$(mktemp "${TMPDIR:-/tmp}/tf_froz.XXXXXX")"
  # Same corpus as 10M-b: runtime sources only, minus the shared-Handle seams.
  find apps/aberp/src modules crates -name '*.rs' \
    | grep -vE '/tests/' \
    | grep -vE '^crates/(aberp-db|aberp-snapshot)/' \
    | sort > "$tf_files"
  # `xargs`-free: the file list is passed as arguments in one shot. `set -e` is
  # active, so capture the exit code explicitly to tell a non-converged closure
  # (3 = harness fault) apart from a clean scan that simply found records.
  tf_rc=0
  awk -v allow="$WF_ALLOW" -v levels=12 -f "$tf_scan" $(cat "$tf_files") > "$tf_raw" 2>/dev/null || tf_rc=$?
  if [[ "$tf_rc" -eq 3 ]]; then
    flag10N "✗ ADR-0105 taint closure did NOT converge — the scan is WEAKER than a complete one; raise -v levels (HARNESS FAULT, not a code regression)"
  elif [[ "$tf_rc" -ne 0 ]]; then
    flag10N "✗ ADR-0105 wrapper-fork scanner errored (exit $tf_rc) — treat as a harness fault, do not ignore"
  else
    # 10N-a — DIRECT is 10M's own class and must be empty here.
    tf_direct="$(grep ':DIRECT:' "$tf_raw" || true)"
    if [[ -n "$tf_direct" ]]; then
      # Worded so it is accurate whether or not 10M also fired: this check does
      # not (and cannot cheaply) verify 10M's verdict for the same site, so it
      # must not assert one. If 10M above IS silent for this site the scanners
      # genuinely disagree; if 10M also fired, this is simply the same real fork
      # reported twice.
      flag10N "✗ ADR-0105 reports a DIRECT (same-fn) write-fork. CHECK 10M covers this class — if 10M above is SILENT for the same site the two scanners disagree (HARNESS FAULT); if 10M also flagged it, fix the fork:"
      printf '%s\n' "$tf_direct" | sed 's/^/      /'
    else
      note "✓ no DIRECT write-fork (agrees with CHECK 10M)"
    fi
    # 10N-b — the frozen transitive/ambiguous set may only SHRINK.
    # `cut`, NOT `sed 's/:\(A\|B\):.*//'` — BSD sed (macOS, where this gate is
    # run by hand) has no BRE alternation, so that pattern silently matches
    # NOTHING and every record stays fully-qualified, never matching the frozen
    # key. Records are `<file>:<fn>:<CLASS>:opener@L<n>:via=<x>` and neither a
    # path nor a Rust fn name can contain `:`, so fields 1-2 are the stable key.
    cut -d: -f1,2 "$tf_raw" | sort -u > "$tf_cur"
    grep -vE '^#' "$tf_manifest" | sed 's/[[:space:]]*#.*$//;s/[[:space:]]*$//' | grep -vE '^$' | sort -u > "$tf_froz" || true
    tf_grew="$(comm -13 "$tf_froz" "$tf_cur")"
    if [[ -n "$tf_grew" ]]; then
      flag10N "✗ a NEW/REGROWN WRAPPER-HIDDEN write-fork appeared outside the frozen set — the opener's connection reaches an audit append through a helper; route it through the shared aberp_db::Handle (ADR-0105):"
      printf '%s\n' "$tf_grew" | sed 's/^/      /'
      printf '%s\n' "$tf_grew" | while IFS= read -r k; do
        grep -F "$k:" "$tf_raw" | sed 's/^/        via: /'
      done
    fi
    tf_shrunk="$(comm -23 "$tf_froz" "$tf_cur")"
    if [[ -n "$tf_shrunk" ]]; then
      note "  (info) wrapper-fork sites migrated off since freeze — refresh $tf_manifest to lock the smaller set:"
      printf '%s\n' "$tf_shrunk" | sed 's/^/      /'
    fi
    if [[ -z "$tf_grew" ]]; then
      tf_n="$(grep -vcE '^#' "$tf_manifest" || true)"
      note "✓ frozen wrapper-fork residual holds (${tf_n:-0} site(s); D-22 emptied it — every wrapper-hidden audit fork, in-process AND separate-process CLI, is on the shared Handle)"
    fi
  fi
  rm -f "$tf_files" "$tf_raw" "$tf_cur" "$tf_froz"
fi

# ── CHECK 11 — ADR-0116 D2: the RECOVERY-EVIDENCE guard.
#
#    Recovery evidence (*CORRUPT*, *RECOVERY*, *DEFORK*, *PRE-*, healed-*,
#    INDEXDESYNC*, the 22 lowercase corrupt-<nanos>.bak, …) lives as SIBLINGS
#    of the live DB inside a tenant home, NOT in the snapshot store. So
#    `plan_retention` — which operates on records built from `snap-*` dirs with
#    a parseable meta.json — is structurally blind to it, and the "never prune
#    evidence" guarantee was satisfied BY ACCIDENT rather than by a rule.
#
#    ADR-0116 D2.3 states the consequence precisely: `prune` only ever touches
#    the snapshot store, so **a refusal inside `prune` alone protects nothing**.
#    The load-bearing half is that every TENANT-HOME helper consults the same
#    predicate, and this check is what enforces it. The hazard is concrete, not
#    theoretical: `recover::cleanup_siblings_with_infix` already enumerated a
#    tenant home and unlinked by prefix with no guard whatsoever.
#
#    11a — the shared guard must EXIST and be reachable: `is_protected_evidence`
#          + `guarded_remove` exported from aberp-snapshot, matching
#          case-INSENSITIVELY (58 of 101 real artefacts escaped a case-sensitive
#          match; this bug class was closed once already in this repo's edition
#          DB-guard), and inverted to a live-file ALLOW-LIST (14 escaped even
#          case-insensitively, including all 9 healed-*.bak and the 24 MB
#          INDEXDESYNC-BACKUP — neither matches any deny-list family in any
#          case).
#    11b — the TENANT_HOME removal-site set may only SHRINK. A NEW unguarded
#          removal that reaches a tenant-home path fails here.
#    11c — corpus liveness: a scanner that classifies nothing makes 11b
#          vacuously green, so a floor is required in the SAFE direction too.
#    11d — `retention::prune` must CONSULT the guard (the scanner's verdict).
#    11e — **no guard may be DEAD.** F7: 11d's verdict was still token
#          PRESENCE in the fn body, so the mutation
#          `if false && …is_protected_evidence(…)` left the gate GREEN with the
#          guard inert. The scanner now verdicts a guard that is dead BY
#          CONSTRUCTION as DEAD_GUARD; this arm fails on any.
#    11f — MATCHER LIVENESS (the 10P-0 pattern). A scanner that silently stops
#          recognising the two known escapes reports "no violations", which is
#          the most dangerous green there is. Four fixtures pin the verdicts
#          this check actually rests on, including both mutations the
#          adversarial review found escaping (M1, M5).
#    Teeth: cut_gate_negative_probes.sh "[CHECK 11]".
#    ENFORCE_EVIDENCE_GUARD=0 disables it for a deliberate, temporary local probe.
echo "[CHECK 11] ADR-0116 D2 — recovery-evidence guard present + tenant-home removal sites frozen (ENFORCED · D5)"
enforce11="${ENFORCE_EVIDENCE_GUARD:-1}"
flag11() { note "$1"; if [[ "$enforce11" == "1" ]]; then fail=1; else note "  (enforcement disabled — not failing)"; fi; }
ev_src="crates/aberp-snapshot/src/evidence.rs"
ev_lib="crates/aberp-snapshot/src/lib.rs"
ev_scan="tools/adr0116_evidence_removal_scan.awk"
ev_manifest="tools/adr0116_tenant_home_removal_sites.txt"

# 11a — the guard exists, is exported, is case-insensitive, and is an
#       allow-list rather than a deny-list.
if [[ ! -f "$ev_src" ]]; then
  flag11 "✗ ADR-0116 D2 evidence guard missing: $ev_src"
else
  for sym in "pub fn is_protected_evidence" "pub fn guarded_remove" "LIVE_TENANT_NAMES" "EVIDENCE_FRAGMENTS"; do
    grep -q "$sym" "$ev_src" || flag11 "✗ $ev_src is missing '$sym' — the shared ADR-0116 D2 guard is incomplete"
  done
  grep -q "is_protected_evidence" "$ev_lib" \
    || flag11 "✗ is_protected_evidence is not re-exported from $ev_lib — tenant-home helpers in other crates cannot call it"
  # Case-insensitivity is the non-negotiable part (F3).
  grep -q "to_ascii_lowercase" "$ev_src" \
    || flag11 "✗ $ev_src does not lowercase before matching — 58 of 101 real evidence artefacts escape a case-SENSITIVE guard (ADR-0116 F3)"
  # The allow-list inversion: the primary predicate must be "not live => evidence".
  grep -q '!name_is_live' "$ev_src" \
    || flag11 "✗ $ev_src has lost the allow-list INVERSION (!name_is_live). A deny-list of named families missed 14 artefacts even case-insensitively, including every healed-*.bak and the sole 2026-08-03 INDEXDESYNC backup (ADR-0116 D2.2)"
fi

# 11b/11c — the frozen TENANT_HOME set.
if [[ ! -f "$ev_scan" || ! -f "$ev_manifest" ]]; then
  flag11 "✗ evidence removal scanner or frozen manifest missing: $ev_scan / $ev_manifest"
else
  # 11f — MATCHER LIVENESS, before anything is derived from the scanner.
  ev_probe="$(mktemp "${TMPDIR:-/tmp}/ev_probe.XXXXXX")"
  printf 'fn c(rec: &R) {\n    if crate::evidence::is_protected_evidence(&rec.dir) {\n        return;\n    }\n    std::fs::remove_dir_all(&rec.dir).ok();\n}\n' > "$ev_probe"
  awk -f "$ev_scan" "$ev_probe" | grep -q ':GUARDED:' \
    || flag11 "✗ HARNESS: the scanner no longer verdicts a live guard as GUARDED — every guarded site would read as unguarded and 11d could never pass honestly"
  # M1 — the guard is PRESENT and dead. This is the mutation that escaped the
  # whole gate before F7; it must be RED now, on one line and split across
  # lines (rustfmt decides which).
  printf 'fn c(rec: &R) {\n    if false && crate::evidence::is_protected_evidence(&rec.dir) {\n        return;\n    }\n    std::fs::remove_dir_all(&rec.dir).ok();\n}\n' > "$ev_probe"
  awk -f "$ev_scan" "$ev_probe" | grep -q ':DEAD_GUARD:' \
    || flag11 "✗ HARNESS: the scanner no longer detects a SHORT-CIRCUITED guard (mutation M1: \`if false && is_protected_evidence(..)\`). GUARDED would be back to token presence, and the guard could be neutered by editing an operator — the ADR-0098 opener-scan class one level in"
  printf 'fn c(rec: &R) {\n    if false\n        && crate::evidence::is_protected_evidence(&rec.dir)\n    {\n        return;\n    }\n    std::fs::remove_dir_all(&rec.dir).ok();\n}\n' > "$ev_probe"
  awk -f "$ev_scan" "$ev_probe" | grep -q ':DEAD_GUARD:' \
    || flag11 "✗ HARNESS: the scanner detects a short-circuited guard only when it fits on ONE line — a rustfmt split hides it. The statement buffer in the scanner has regressed"
  # M5 — the removal spelled through a direct import. Nothing in-tree uses
  # this spelling, which is precisely why it must stay pinned: a gate that
  # bans ONE spelling is the class already on record here from PR #41.
  printf 'use std::fs::remove_file;\nfn sweep(tenant_home: &Path) {\n    for e in std::fs::read_dir(tenant_home).unwrap().flatten() {\n        remove_file(e.path()).ok();\n    }\n}\n' > "$ev_probe"
  awk -f "$ev_scan" "$ev_probe" | grep -q ':TENANT_HOME:' \
    || flag11 "✗ HARNESS: the scanner no longer sees a removal spelled through a direct import (\`use std::fs::remove_file;\` … \`remove_file(p)\`, mutation M5). A tenant-home sweeper written that way passes the whole gate"
  # …and DEAD_GUARD must NOT fire on `let _ = guarded_remove(..)`, which is
  # idiomatic and completely safe: `guarded_remove` PERFORMS the guarded
  # action and returns a Result, unlike `is_protected_evidence`, whose
  # returned bool IS the decision. `recover::cleanup_siblings_with_infix`
  # spells it that way today, and a gate that reddens correct code is a gate
  # that gets switched off.
  printf 'fn c(db_path: &Path) {\n    for e in std::fs::read_dir(db_path.parent().unwrap()).unwrap().flatten() {\n        let _ = aberp_snapshot::guarded_remove(&e.path());\n        let _ = std::fs::remove_file(e.path());\n    }\n}\n' > "$ev_probe"
  awk -f "$ev_scan" "$ev_probe" | grep -q ':GUARDED:' \
    || flag11 "✗ HARNESS: the scanner verdicts \`let _ = guarded_remove(..)\` as anything but GUARDED — the discarded-result rule must apply to the PREDICATE (is_protected_evidence), never to the guarded ACTION"
  # …and it must NOT cry wolf on a method call or a fn DEFINITION, or the
  # widened matcher would flood the frozen manifest and get switched off.
  printf 'fn q(x: &X) {\n    x.remove_file();\n    guarded_remove_file(x);\n}\nfn remove_file(p: &Path) {}\n' > "$ev_probe"
  if awk -f "$ev_scan" "$ev_probe" | grep -q ':'; then
    flag11 "✗ HARNESS: the widened removal matcher fires on \`self.remove_file()\`, \`guarded_remove_file()\` or a \`fn remove_file\` DEFINITION — it would classify half the tree and the check would be abandoned"
  fi
  rm -f "$ev_probe"

  ev_files="$(mktemp "${TMPDIR:-/tmp}/ev_files.XXXXXX")"
  ev_raw="$(mktemp "${TMPDIR:-/tmp}/ev_raw.XXXXXX")"
  ev_cur="$(mktemp "${TMPDIR:-/tmp}/ev_cur.XXXXXX")"
  ev_froz="$(mktemp "${TMPDIR:-/tmp}/ev_froz.XXXXXX")"
  find apps/aberp/src modules crates -name '*.rs' | grep -vE '/tests/' | sort > "$ev_files"
  ev_rc=0
  awk -f "$ev_scan" $(cat "$ev_files") > "$ev_raw" 2>/dev/null || ev_rc=$?
  if [[ "$ev_rc" -ne 0 ]]; then
    flag11 "✗ ADR-0116 evidence removal scanner errored (exit $ev_rc) — treat as a HARNESS FAULT, never as a clean tree"
  else
    # `cut`, NOT a sed alternation: BSD sed (macOS, where this gate is also run
    # by hand) has no BRE alternation, so an `s/:\(A\|B\):.*//` pattern matches
    # NOTHING and every record stays fully-qualified, never matching the frozen
    # key — a silent green. Records are <file>:<fn>:<VERDICT>:<tok>@L<n> and
    # neither a path nor a Rust fn name can contain `:`, so fields 1-2 are the
    # stable key.
    grep ':TENANT_HOME:' "$ev_raw" | cut -d: -f1,2 | sort -u > "$ev_cur" || true
    grep -vE '^#' "$ev_manifest" | sed 's/[[:space:]]*#.*$//;s/[[:space:]]*$//' \
      | grep -vE '^$' | sort -u > "$ev_froz" || true
    ev_grew="$(comm -13 "$ev_froz" "$ev_cur")"
    if [[ -n "$ev_grew" ]]; then
      flag11 "✗ a NEW UNGUARDED tenant-home removal appeared. Route it through aberp_snapshot::guarded_remove (ADR-0116 D2) — an unlink beside the live DB destroys the ONLY record of a durability incident, permanently:"
      printf '%s\n' "$ev_grew" | sed 's/^/      /'
      printf '%s\n' "$ev_grew" | while IFS= read -r k; do
        grep -F "$k:" "$ev_raw" | sed 's/^/        at: /'
      done
    fi
    ev_shrunk="$(comm -23 "$ev_froz" "$ev_cur")"
    if [[ -n "$ev_shrunk" ]]; then
      note "  (info) tenant-home removal sites guarded/removed since freeze — refresh $ev_manifest to lock the smaller set:"
      printf '%s\n' "$ev_shrunk" | sed 's/^/      /'
    fi
    # 11d — `retention::prune` must actually CALL the guard (D2.3's belt half).
    #
    # Asserted via the SCANNER's verdict, never with a bare grep. The first cut
    # grepped retention.rs for the string `is_protected_evidence`, and its own
    # DOC COMMENT names the function — so neutering the real call left the gate
    # green and the negative probe ESCAPED. That is the flip-by-editing-a-
    # comment class already on record in this repo (the ADR-0098 opener-scan
    # char-literal bug), reproduced here in a new check. The scanner strips
    # comments and strings before matching, so its GUARDED verdict means the
    # call is real code.
    if ! grep -q '^crates/aberp-snapshot/src/retention.rs:prune:GUARDED:' "$ev_raw"; then
      flag11 "✗ retention::prune does not CONSULT is_protected_evidence (scanner verdict, comment-aware, LIVENESS-aware) — the pruner's blindness to evidence must be a DELIBERATE refusal, not a structural accident (ADR-0116 D2.3)"
    fi
    # 11e — no guard anywhere may be dead by construction (ADR-0116 F7 / M1).
    ev_dead="$(grep ':DEAD_GUARD:' "$ev_raw" || true)"
    if [[ -n "$ev_dead" ]]; then
      flag11 "✗ an ADR-0116 D2 guard is PRESENT but DEAD — short-circuited by a boolean literal, or its result discarded. A guard that cannot refuse is worse than no guard: its presence is what stops anyone looking. Fix the guard, never this check:"
      printf '%s\n' "$ev_dead" | sed 's/^/      /'
    fi
    # 11c — liveness. A scanner that emits nothing makes 11b vacuously green.
    # The tree has ~30 removal sites across the three classes; require the
    # classification to be non-trivial so "no violations" cannot mean "no scan".
    ev_total="$(grep -c ':\(GUARDED\|DEAD_GUARD\|TENANT_HOME\|OTHER\):' "$ev_raw" || true)"
    ev_guarded="$(grep -c ':GUARDED:' "$ev_raw" || true)"
    if [[ "${ev_total:-0}" -lt 15 ]]; then
      flag11 "✗ HARNESS: only ${ev_total:-0} removal sites classified across the workspace — the corpus or the scanner is broken, so a green CHECK 11b means nothing"
    fi
    if [[ "${ev_guarded:-0}" -lt 1 ]]; then
      flag11 "✗ HARNESS: the scanner classified ZERO removals as GUARDED — it can no longer RECOGNISE the guard, so every guarded site would read as unguarded (or, worse, a future guarded site would read as safe for the wrong reason)"
    fi
    if [[ -z "$ev_grew" ]]; then
      ev_n="$(grep -vcE '^#|^$' "$ev_manifest" || true)"
      note "✓ evidence guard holds (${ev_total:-0} removal sites classified, ${ev_guarded:-0} guarded; ${ev_n:-0} frozen tenant-home sites, all removing live-allow-listed artefacts)"
    fi
  fi
  rm -f "$ev_files" "$ev_raw" "$ev_cur" "$ev_froz"
fi

echo
if [[ "$fail" -ne 0 ]]; then echo "CUT-GATE: ✗ FAILED"; exit 1; fi
echo "CUT-GATE: ✓ PASSED"
