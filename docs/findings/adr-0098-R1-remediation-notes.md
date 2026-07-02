# ADR-0098 v0.2.5 remediation — R1 (Fable-5 findings D + E + G)

**Branch:** `adr0098-remediation` off `e144506` (main; try_clone merged).
**Batch:** first of the v0.2.5 remediation batch; R2/R3/R4 stack on this branch.
**Status:** WIP checkpoint (implementation follows in subsequent commits).

## Scope (this session, R1 only)
Make the audit-mirror **torn-tail** handling COHERENT across the two paths that
today take opposite stances, and close the **empty-mirror vacuous self-cert** hole.

- **D** — boot reconciler (`crates/audit-ledger/src/mirror.rs`
  `ensure_consistent_with_db`): the `MirrorCorrupt` arm and the equal-length
  head-hash-mismatch arm silently `rebuild_mirror_from_db` (`.truncate(true)`),
  destroying a possibly-intact prefix (entries the DB lost via a dropped WAL tail).
- **E** — recovery mirror-read (`crates/aberp-snapshot/src/recover.rs`
  `recover_or_refuse` → `aberp_audit_ledger::read_mirror_entries`) HARD-ERRORS on a
  torn tail and maps it to `RefusedUnsafe`, bricking auto-recovery on a single
  power event and demanding operator JSONL hand-surgery.
- **G** — an EMPTY mirror (`mirror_head == 0`) makes the ahead-snapshot overlap
  `[1..=0]` vacuously satisfied, so ANY internally-valid snapshot self-certifies
  and installs (a MISSING mirror refuses, but an EMPTY one recovers).

## Unified torn-tail policy (D + E — ONE code path)
Both the boot reconciler AND the recovery mirror-read route through a single
shared helper in `audit-ledger`:
1. On any parse failure, PRESERVE the original byte-for-byte to
   `<mirror>.corrupt-<nanos>.bak` FIRST (mirrors `preserve_ahead_mirror`).
2. If the corruption is EXACTLY one unterminated/partial FINAL line (a torn tail —
   "the append never durably happened") AND the trimmed prefix re-verifies →
   trim that one line, CONTINUE (loud log + audit event; boot then reconciles
   the trimmed head against the DB, so a still-ahead trimmed mirror hits the P0).
3. ANY deeper corruption (a break/gap/mismatch not at the final line) → preserve +
   REFUSE (boot: non-zero exit w/ operator-actionable message naming the preserved
   path; recovery: `RefusedUnsafe`). NEVER silent `rebuild_mirror_from_db`, NEVER
   `.truncate(true)` a mirror that may hold entries the DB lacks, NEVER tell the
   operator to hand-edit JSONL.
- Equal-length head-hash-mismatch (D): preserve + REFUSE (divergence at equal
  length on prod-class data = never auto-resolve).

## Empty-mirror genesis anchor (G)
An ahead snapshot may self-certify ONLY if `mirror_head >= 1` AND the overlap is
anchored at genesis (seq-1 hash matches). Empty/absent mirror → REFUSE.

## Preserved unchanged
The P0 mirror-AHEAD-of-DB preserve+refuse (`MirrorAheadOfDb` / `preserve_ahead_mirror`)
and every reconcile/recovery test not encoding the fixed arms above.

## Decision table (implemented)

| Mirror condition | Boot reconciler `ensure_consistent_with_db` | Recovery `recover_or_refuse` |
|---|---|---|
| Clean | proceed (extend / unchanged) | proceed |
| **Torn tail** (lone unterminated FINAL line; trimmed prefix re-verifies) | preserve `.corrupt-<nanos>.bak` → trim → **CONTINUE** (loud log + audit event) | preserve → trim → **PROCEED** on prefix |
| **Deep corruption** (break/gap/JSON/chain mismatch NOT at final line) | preserve → **REFUSE** `MirrorCorruptPreserved` (boot non-zero exit) | preserve → **`RefusedUnsafe`** |
| **Equal-length head-hash divergence** | preserve → **REFUSE** `MirrorCorruptPreserved` (never rebuild) | — (surfaces via overlap self-cert) |
| **Mirror AHEAD of DB** (P0 — UNCHANGED) | preserve `.ahead-<nanos>.bak` → **REFUSE** `MirrorAheadOfDb` | boot owns this P0 |
| **Empty mirror** (head=0) + ahead snapshot | — | **REFUSE** (no genesis anchor; no vacuous self-cert) |
| Missing mirror | (re)build from DB → `Created` | `RefusedUnsafe` |

Both the boot reconciler and the recovery mirror-read call the SAME shared helper
`read_mirror_under_tail_policy` (audit-ledger `mirror.rs`) → ONE torn-tail policy,
both sides. The ahead-of-DB P0 (`preserve_ahead_mirror`/`MirrorAheadOfDb`) is untouched.

## Local gate (in-sandbox, honest)
- `rustfmt --check` (edition 2021): CLEAN on all 4 changed files.
- `cut_gate_db_isolation.sh`: **PASSED** (79 ✓, exit 0) — incl. CHECK 3/4 reconcile
  safety (ahead-mirror preserve+refuse intact) and CHECK 10i frozen residual-opener
  ledger (139 openers / 31 files — NO growth; no new opener added).
- `rustc --test` faithful extraction (`tools/adr0098_r1_torn_tail_extract.rs`): **9/9 pass** —
  the six named behaviours (torn-tail-boot, deep-corrupt-boot, equal-length-mismatch,
  torn-tail-recovery, empty-mirror-recovery, P0 ahead-of-DB regression) + 3 pure tables.

## CI / Mac-deferred (cannot run in the DuckDB-less saw-off sandbox)
- Full `cargo build`/`cargo test` of audit-ledger + aberp-snapshot against real DuckDB
  (the new in-crate `#[cfg(test)]` tests: `ensure_consistent_trims_torn_tail_and_continues`,
  `..._refuses_and_preserves_on_head_hash_mismatch`, `..._on_deep_corruption`,
  `decide_tail_maps_the_four_cases`, `route_guard_refuses_ahead_against_empty_mirror`,
  `overlap_genesis_anchor_requires_seq_1`).
- Boot/CLI e2e (a DB-torn + mirror-torn-tail single power event → auto-recovers now,
  not bricks) — lands in the batch's end-of-chain CI 2-arm build-proof.

## FLAGGED conservative calls
- The torn-tail CONTINUE emits a structured `target:"audit_event"` tracing event, NOT a
  DB `db.audit` row: appending to the chain mid-reconcile (pre-serve, while validating it)
  would mutate the very chain being reconciled. A durable `db.auto_recovered`-class audit
  row belongs in the post-open serve wiring (alongside ADR-0095's `db.auto_recovered`) —
  CI/Mac-deferred.
- `first_overlap_disagreement` remains O(n²) — left as a flagged v0.2.6 perf item per scope
  (fine at current scale); not touched here.
- Line-ref reconciliation: finding D's "equal-length head-hash-mismatch arm ~:544-563" is a
  stale approximation — the real arm is `ensure_consistent_with_db` file lines ~648-668
  (`rebuild_mirror_from_db` at :658). Behaviour matched the finding exactly; fixed there.
