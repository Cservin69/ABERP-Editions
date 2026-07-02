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
