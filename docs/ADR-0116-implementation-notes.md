# ADR-0116 — implementation notes, drift register, and what an adversarial should attack

- **Branch:** `feat/adr-0116-snapshot-system`, off `origin/main` @ `bae151d`.
- **Design source:** ADR-0116 **rev 2** (`docs/adr-db-snapshot-system` @ `afe4bdb`), which
  had already applied its own adversarial FIX-FIRST verdict (F1–F8, G6, G8).
- **Status:** implemented, all gates green, **not landed**. A v0.6.3 cut is staged on
  `fix/d22-money-cli-durability`; landing order is Ervin's call.

The ADR file itself is not on this branch — it lives on the docs branch. This file is
the in-tree record of what was built against it, and specifically of **where the
implementation deviates from the ADR's literal text and why**.

---

## What ships, mapped to the ADR

| ADR | Implemented as |
|---|---|
| **D1.1** grid cadence | `snapshot::sleep_to_next_grid_boundary` — pure, testable, cosmetic by the ADR's own measurement (0.27 s/tick drift) |
| **D1.2** catch-up / freshness | staleness check on **every** tick, not only the first, so a floor or trigger that already satisfied the window makes the daemon no-op |
| **D1.3** out-of-process floor | `run/install_snapshot_floor.sh` — launchd agent, daily 03:00, `--if-stale-secs 86400`, honours `ABERP_SNAPSHOT_DISABLE` and logs LOUD when it no-ops for it, `--verify-run` for open decision #3 |
| **D2** evidence guard | `crates/aberp-snapshot/src/evidence.rs`: `is_protected_evidence`, `guarded_remove`, case-insensitive, allow-list inverted, extra roots named; **cut-gate CHECK 11** + 8 negative probes |
| **D2** evidence lifecycle | `aberp evidence list|archive`, `plan_evidence_release` (pure), archive-then-remove with SHA-256 verification, new `evidence.archived` EventKind |
| **D3.1 / G2** crash-safe restore | `restore_into` deletes the target WAL **first**, then `crash_safe::atomic_install` |
| **D3.2** dry-run | `--dry-run` / `--verify-only` on both restore commands; exit code distinguishes proceed from refuse |
| **D3.3 / G6** stable identity | `SnapshotSelector` / `resolve_selector`, refuses on ambiguity, `<seq>@<created_at>#<sha8>` |
| **D3.4 / F4** in-place restore | `aberp restore --in-place` + `restore_in_place` — serve-stopped refusal, mandatory pre-restore snapshot, `.PRE-RESTORE-` unit, fresh marker |
| **D3.5 / F8** ledger routing | side path → live ledger (unchanged, pinned); `--in-place` → restored chain; both durably acked |
| **D4 / F7** anchors | `anchor_count` / `anchored_through_seq` / `meta_version`, `-1` / `None` sentinels, recorded never gating |
| **D5** triggers | `trigger_snapshot_if_stale` + boot-after-auto-recovery + clean-shutdown (bounded) |
| **G8** forensic retention | `RetentionPolicy::{keep_failed, keep_failed_days}` |

---

## Drift register — read this first

Six deviations. Each is a decision, not an oversight; each is argued.

### 1. `--accept-data-loss` added (D3.3)

The ADR says refuse when the live DB is ahead **"in a way the operator has not
acknowledged"** but names no acknowledgement flag. Implemented literally, every
backwards rollback would be banned — and going backwards past committed rows is what a
rollback **is**. The gate now makes it a decision rather than a ban. Without this the
operator's workaround is `cp`, which is the thing this ADR exists to eliminate.

### 2. The Defense anchor sanction fires on PARTIAL coverage, warns on ZERO (D4)

The ADR: *"`aberp restore --in-place` on Defense REFUSES without `--accept-unanchored`
when `anchored_through_seq < chain_len`."*

Taken literally, **every** Defense in-place restore refuses today: every
`audit_ledger_anchors.parquet` in both stores is exactly 300 bytes, i.e. zero anchor
rows everywhere. A flag that must always be passed is muscle-memory within a week and
stops being a decision — and it adds a step to the most stressful command in the
product, typed at 02:00 during an incident.

So: `anchor_count > 0 && coverage < chain_len` (**anchoring is running and this snapshot
is genuinely short**) refuses; `anchor_count == 0` (**the rollout has not happened** — a
fact about the system, not this snapshot) proceeds with a LOUD warning naming exactly
what the restored DB cannot prove; not-recorded (pre-D4) proceeds with a warning, since
refusing would make every pre-D4 snapshot unrestorable. ADR-0116 Phase 3 already says
the Defense refusal *"waits on real anchor coverage existing"*.

### 3. Evidence is archived VERBATIM, not compressed (D2)

The ADR says "(compressed)". Compression would be a new supply-chain dependency
(ADR-0007, and `cargo-deny` is the single gate since 2026-08-04) for a Phase-2
convenience — and verbatim bytes plus a re-checkable SHA-256 are strictly stronger for
forensic evidence than a re-encoded copy whose integrity depends on a decoder.

### 4a. Order WITHIN the preserved unit (found by self-review, not by a test)

The first cut moved the WAL and marker aside first and the DB second. A failed DB
rename would then have left the **live** database in place **without its WAL** — stripped
of every un-checkpointed commit, which is F4's failure caused by the preserve step
itself. Every `Handle` commit is WAL-only until a checkpoint (ADR-0098 R5), so that is
the most recent rows, not a narrow window. The DB now moves first (the point of no
return) and a failed WAL move rolls it back; the marker is best-effort because step 5
regenerates it. Pinned by `preserve_moves_the_db_first_and_rolls_back_if_the_wal_move_fails`,
which is mutation-checked: restoring the old order turns it red.

### 4. The preserved WAL is named `<db>.PRE-RESTORE-<tag>.wal`, not `<db>.wal.PRE-RESTORE-<tag>`

The ADR does not specify the naming, and the on-disk `.CORRUPT-` convention would
suggest the second form. **The second form is wrong**: DuckDB finds a WAL by appending
`.wal` to the FULL database filename, so it pairs with nothing and the preserved unit
would open WITHOUT its un-checkpointed commits — F4's defect wearing a different mask,
and AC-9 unsatisfiable again. The first form follows the in-tree ADR-0099 R3 precedent
(`recover::preserve_corrupt_db` copies to `wal_sibling(&dest)` and states the same
reason). **Found by the AC-9 test, not by review.**

### 5. The side-path dry-run reports a LOWER BOUND, not an exact delta (D3.2)

The ADR asks the dry-run for "the delta against the live DB: rows and invoices that
exist now and would not exist after". Reading that exactly requires opening the live
database — and `aberp serve` may be running, which is the ADR-0098 two-instance hazard
this tree spent three sessions closing.

Resolution: **exact where it is safe, honest where it is not.** `restore --in-place`
already refuses unless serve is stopped, so it holds the file exclusively and reads
EXACT counts on that same probe connection (no new opener). The side-path command falls
back to the audit mirror and **labels the result a lower bound**, because
`Ledger::append` commits without syncing the mirror — so the 15 CLI money-submission
sites (D-22) leave the mirror lagging in exactly the serve-down windows an operator
restores in. A report that presented that as "nothing would be lost" would be worse than
no report.

### 6. The clean-shutdown snapshot is BOUNDED (D5)

30 s, then the process exits anyway and logs LOUD. A logical EXPORT is unbounded in
principle, and S213 / CLAUDE.md #12 forbid anything that can wedge process exit. Giving
up costs a `*.partial` directory, which is inert: `list_snapshots` and `next_seq` both
ignore `*.partial`.

**Narrowing, stated:** D5's "boot after an unclean shutdown" trigger is implemented only
for the *successful auto-recovery* case, not for a general unclean-shutdown detector.
There is no non-noisy signal for the latter — `checkpoint_is_current` is false after any
write since the last checkpoint, not only after an unclean stop.

---

## One new sanctioned opener, registered deliberately

`snapshot::ensure_serve_is_stopped` opens the live DB exclusively as the serve-liveness
probe. The cut-gate caught it (CHECK 10i count, CHECK 10k fingerprint) and it is now in
both frozen manifests with its rationale.

Why an opener rather than something lighter: DuckDB's own exclusive lock is the ground
truth, and every alternative is wrong in the **dangerous** direction. A liveness
touchfile is stale after a crash, so it answers "serve is stopped" while serve is up —
and an in-place swap under a live writer strands the shared connection on an unlinked
inode, which is the ADR-0111 orphan this whole programme closed. It opens, sets
`PRAGMA disable_checkpoint_on_shutdown`, reads two counts, and drops. It appends
nothing, so CHECK 10M/10N/10P see no fork.

---

## Where the tests are honest about their limits

**AC-1 is a mutation assertion, not a crash test.** The ADR's original AC-1 —
"kill the process between import and rename, assert old-or-new" — is **vacuous**:
`rename(2)` is atomic in the page cache, so that test passed with zero fsyncs and would
pass again after the fix. It tests rename atomicity (never in doubt) instead of the
fsyncs (the entire point). The implemented AC-1 asserts (a) `restore_into` commits
through `crash_safe::atomic_install` and not a bare rename, and (b) that primitive still
carries `fsync_file` and `fsync_dir` — deleting either turns the named test red.

**Genuine crash injection needs a filesystem-level harness this project does not have.**
Said here rather than papered over.

---

## What an adversarial should attack

1. **CHECK 11's classification model is fn-scoped**, not a taint closure. A helper that
   takes a tenant-home path as a parameter and unlinks it, with no tenant-home token in
   its own body, classifies `OTHER` and passes. The token set has already been widened
   once for exactly this: self-review found `seller_toml_backup::prune_old_backups`
   enumerating the tenant home and unlinking by prefix — the ADR's own hazard shape, a
   SECOND instance of it in the tree — and the scanner did not see it, because that fn
   mentions none of the DB-shaped path names. `read_dir` is now a token, on the
   principle that ENUMERATE-AND-UNLINK is the dangerous shape whatever the directory,
   and the site now routes through `guarded_remove`. **Assume there is a third.**
2. **`.bak` was a substring fragment and matched `.backup-`**, so the guard silently
   froze the seller-config backup rotation — a guard that breaks a working rotation is
   a guard someone switches off. It is now suffix-anchored, and the `.backup-`
   allow-list entry is pinned from both sides (the seller backup must be removable;
   `CORRUPT-BACKUP` / `INDEXDESYNC-BACKUP` must not be). **The allow-list entries are
   the risky half of an allow-list design — audit each one for what it un-protects.**
3. **`LIVE_TENANT_NAMES` is a maintained list.** A new legitimate live filename nobody
   adds becomes "protected evidence" — the safe direction, but it accumulates, and a
   cleanup helper that starts refusing is a guard people switch off.
4. **`LIVE_TRANSIENT_INFIXES` is the un-protecting half** and deserves the harder look:
   the family predicate runs first, so an evidence-shaped name cannot be un-protected by
   a transient infix — but verify that ordering has not been inverted by a later edit.
5. **The pre-restore snapshot extends the mirror**, and the e2e now asserts only
   append-only + not-moved. Is there a mirror mutation the restore could make that this
   would not catch?
6. **Drift #2 (the anchor sanction)** is the most consequential deviation and is the one
   to argue with directly if you disagree.
7. **The D1.2 skip and the ADR-0095 §3 live checkpoint share a loop but must not
   share a condition.** The first cut of the skip `continue`d past
   `live_checkpoint_logged`, silently un-wiring the live-file durable checkpoint on
   every skipped tick — and precisely in the configuration this ADR sets up, where a
   scheduled floor satisfies the staleness window most ticks. Fixed, and called out
   here because it is the kind of coupling that reads as correct.
8. **`store_is_stale` uses the newest snapshot valid-or-not.** A DB that fails validation
   every cycle therefore suppresses retries for a full interval. Deliberate (avoids a
   snapshot storm on a broken DB), but it means a broken tenant produces one failed
   forensic snapshot per interval rather than a burst — check that is what you want.

---

## Gate results

Recorded in the branch's final commit message. Both required checks
(`ADR-0093 DB-isolation cut-gate`, `defense · build + lint + test`) plus fmt,
`clippy -D warnings`, the full workspace test, and all four probe harnesses.
No de-gating, no `continue-on-error`, no skipped probe.
