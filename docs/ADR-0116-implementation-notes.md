# ADR-0116 — implementation notes, drift register, and what an adversarial should attack

- **Branch:** `feat/adr-0116-snapshot-system`, off `origin/main` @ `bae151d`.
- **Design source:** ADR-0116 **rev 2** (`docs/adr-db-snapshot-system` @ `afe4bdb`), which
  had already applied its own adversarial FIX-FIRST verdict (F1–F8, G6, G8).
- **Status:** implemented, all gates green, **not landed**. A v0.6.3 cut is staged on
  `fix/d22-money-cli-durability`; landing order is Ervin's call.
- **Revision 3 (2026-08-29)** applies the second adversarial verdict
  (`docs/_adversarial-adr-0116-snapshot-review.md`, **BLOCKED**): the boot-after-restore
  blocker, seven ranked fix-firsts, the CHECK 11 liveness hardening, and Ervin's KEEP
  rulings on both drifts with the adversarial's conditions attached. See
  **"Revision 3"** below.

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
| **D3.4 / F4** in-place restore | `aberp restore --in-place` + `restore_in_place` — serve-stopped refusal, mandatory pre-restore snapshot, `.PRE-RESTORE-` unit (DB + WAL + marker + **mirror**, rev 3), fresh marker, **fresh mirror for the restored chain**, **post-install verification of the file on disk** |
| **D3.5 / F8** ledger routing | side path → live ledger (unchanged, pinned); `--in-place` → restored chain; both durably acked |
| **D4 / F7** anchors | `anchor_count` / `anchored_through_seq` / `meta_version`, `-1` / `None` sentinels, recorded never gating |
| **D5** triggers | `trigger_snapshot_if_stale` + boot-after-auto-recovery + clean-shutdown (bounded) |
| **G8** forensic retention | `RetentionPolicy::{keep_failed, keep_failed_days}` |

---

## Drift register — read this first

Six deviations. Each is a decision, not an oversight; each is argued. **Both drifts
flagged for review (1 and 2) were ruled KEEP by Ervin on 2026-08-29, with the
adversarial's conditions applied — see Revision 3.**

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
regenerates it. Rev 3 puts the **mirror after the WAL** on the same rule — the last
renameable artefact, rolled back with the rest on failure. Pinned by
`preserve_moves_the_db_first_and_rolls_back_if_the_wal_move_fails`, which is
mutation-checked: restoring the old order turns it red, and so does deleting the mirror
move.

### 4b. The mirror moves INTO the preserved unit (rev 3 — this REVERSES rev 2)

Rev 2's D3.4 step 3 read *"the `.audit.log` mirror does NOT move. It is the durable
record and stays at the live path."* That rule is right in general and **wrong once the
operator has acknowledged that the mirror's tail is not to be replayed** — and leaving
it in place made the command's headline case a no-op across a restart. It is the
BLOCKER; the full mechanism is in Revision 3 below.

The mirror is now `<db>.PRE-RESTORE-<tag>.audit.log`, moved as part of the same unit and
protected by the D2 evidence guard at its new name, and a fresh mirror is written from
the restored chain inside the same operation.

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
3. **The inversion is scoped to IMMEDIATE children of a tenant directory**, and depth is
   governed by the family predicate alone (which matches on ancestor components too, so an
   evidence directory's contents stay protected). A first cut applied it at any depth, which
   made every file inside `ap-artifacts/`, `ncr-photos/`, `email-relay-attachments/` and
   `issued/` "evidence" — freezing, among others, the incoming-invoice ingest's rollback
   cleanup of an orphaned artifact. Both halves are pinned; check the boundary.
4. **`LIVE_TENANT_NAMES` is a maintained list.** A new legitimate live filename nobody
   adds becomes "protected evidence" — the safe direction, but it accumulates, and a
   cleanup helper that starts refusing is a guard people switch off.
5. **`LIVE_TRANSIENT_INFIXES` is the un-protecting half** and deserves the harder look:
   the family predicate runs first, so an evidence-shaped name cannot be un-protected by
   a transient infix — but verify that ordering has not been inverted by a later edit.
6. **The pre-restore snapshot extends the mirror**, and the e2e asserts append-only on
   the PRESERVED copy plus agreement between the FRESH mirror and the restored DB
   (rev 3 — the mirror now moves into the unit). Is there a mirror mutation the restore
   could make that this would not catch?
7. **Drift #2 (the anchor sanction)** is the most consequential deviation and is the one
   to argue with directly if you disagree.
8. **The D1.2 skip and the ADR-0095 §3 live checkpoint share a loop but must not
   share a condition.** The first cut of the skip `continue`d past
   `live_checkpoint_logged`, silently un-wiring the live-file durable checkpoint on
   every skipped tick — and precisely in the configuration this ADR sets up, where a
   scheduled floor satisfies the staleness window most ticks. Fixed, and called out
   here because it is the kind of coupling that reads as correct.
9. **(rev 3) `--boot-check` is not a boot.** It runs `serve::run`'s DB-side
   preconditions and returns before the registry self-heal, the TLS cert and the
   listener — and it skips the session token and NAV credentials so it never touches the
   OS keychain. The claim it supports is exactly "the restored database does not stop
   serve from starting", nothing wider. Attack the gap: is there anything BELOW the
   return that a restore can break?
10. **(rev 3) The DEAD_GUARD detector recognises SHAPES, not reachability.** It catches a
   constant-false conjunction, a constant-true disjunction and a discarded predicate
   result, evaluated over a statement. It does not catch, say, a guard behind a runtime
   flag that is always false, or an early `return` above it. That is why
   `f7_prune_refuses_a_protected_directory_and_does_not_report_it_removed` exists — but
   it pins ONE function. Which other guard site would survive being neutered?
11. **(rev 3) `journey_backwards_in_place_restore_leaves_a_database_serve_can_boot`
   asserts a healthy reconcile action.** `Extended` and `Unchanged` both pass. Is there a
   restore outcome that produces one of those and is still wrong?
12. **`store_is_stale` uses the newest snapshot valid-or-not.** A DB that fails validation
   every cycle therefore suppresses retries for a full interval. Deliberate (avoids a
   snapshot storm on a broken DB), but it means a broken tenant produces one failed
   forensic snapshot per interval rather than a burst — check that is what you want.

---

## The probes that escaped, and what they say about the check

**Read this section together with Revision 3's "CHECK 11 can no longer be fooled".
The same check was flipped TWICE, by two different things that are not the guard —
first a comment, then an operator — and each time the gate was green, the code was
correct, and the check was worthless.**

### Rev 2's escape: a comment

CHECK 11's first cut asserted "prune consults the guard" with a bare
`grep -q is_protected_evidence crates/aberp-snapshot/src/retention.rs`. The negative
probe that neuters the real call **ESCAPED**: the function's own DOC COMMENT names
`is_protected_evidence`, so the grep still matched and the gate stayed green.

That is the flip-by-editing-a-comment class already on record in this repo (the ADR-0098
opener-scan char-literal bug) — reproduced by me, in a brand-new check, while writing the
check whose whole purpose is to not be fooled. It is the strongest argument in this
branch for why negative probes are non-optional: the gate was green, the code was
correct, and the check was worthless.

Now asserted via the SCANNER's `GUARDED` verdict, which strips comments and strings
before matching. Verified: with the call neutered, `grep -c` still returns 1 and the gate
goes RED.

### Rev 3's escape: an operator

The scanner verdict closed the comment escape and stopped one level short. `GUARDED` was
still **token presence in the fn body**, so

```rust
if false && crate::evidence::is_protected_evidence(&rec.dir) {
```

passed the whole gate with the guard dead. Fixed by teaching the scanner the shapes a
neutering edit actually takes (`DEAD_GUARD`) and by pinning the answer where reachability
actually belongs — a behavioural test. See Revision 3.

**The lesson both escapes share:** each fix was assertion-level (grep → scanner verdict →
liveness), and each time the *next* level down was still assertion-level. The check only
stopped being foolable when a test asserted the BEHAVIOUR rather than the source. A
scanner arm and a behavioural test are not alternatives here; the scanner catches the
shape cheaply across the whole corpus, and the test is what makes a green mean something.

## Revision 3 — the second adversarial verdict, applied

Source: `docs/_adversarial-adr-0116-snapshot-review.md` (branch @ `48ebc26`), verdict
**BLOCKED**. Everything below is fixed on this branch, each with a test that is RED
before its fix.

### The BLOCKER — `aberp serve` would not boot after a backwards in-place restore

`restore --in-place --accept-data-loss` rolled the live database back exactly as
designed, and then the next boot **refused to start**:

```
BOOT ROUTER: Err(MirrorDivergedFromDb { first_divergent_seq: 4, mirror_max_seq: 11, db_max_seq: 4 })
             -> BootMirrorRoute::RefuseFatal -> `aberp serve` does not start
```

Three correct subsystems, one seam nobody walked across:

1. the preserve step deliberately left `.audit.log` at the live path, so after a
   backwards rollback it still held the tail the operator discarded;
2. step 7 appended `SnapshotRestored` **to the restored chain** — landing at a seq the
   mirror already held with a *different* entry;
3. `ensure_consistent_with_db` reports that as `MirrorDivergedFromDb`, which is terminal
   **by design** and correctly so: two committed entries claiming one seq is a business
   question, not an automatic one.

Step 2 made the divergence certain, and step 2 was new on this branch. The only way out
was the hand-reconciliation D3.4 exists to eliminate, and nothing in the command's
output said so.

**The premise that changed.** `boot_mirror_route`'s comment says *"a CLEAN ahead mirror
is the fingerprint of a torn-write / lost DB commit."* Since D3.4 a mirror that
disagrees with the DB is **also** the fingerprint of a deliberate, acknowledged
rollback, and the two are indistinguishable on disk.

**The fix, and why this shape.** The rollback is an explicit decision, so the mirror's
discarded tail stops being an input to the next boot: it moves into the `.PRE-RESTORE-`
unit as `<db>.PRE-RESTORE-<tag>.audit.log` (protected evidence at its new name, paired
with the preserved DB by the same rule the WAL follows), and a fresh mirror is written
from the restored chain inside the same operation. Boot is then left with nothing to
reconcile.

The alternative — a rollback marker beside the DB that `boot_mirror_route` consults — was
rejected: it makes the boot decision depend on a second source of truth that can be
lost, stale, or left behind, and it teaches the boot path to *ignore* a divergence,
which is the one thing ADR-0099 R3 spent a session proving it must never do.

The mirror rebuild is **best-effort by design**: an ABSENT mirror is the one
disagreement the boot path resolves safely by itself (`RecoveryAction::Created`), so a
failure there degrades to "the next boot writes it", never to "the next boot refuses".

**The other half of the fork, closed too.** If the restored chain is instead a clean
PREFIX of a left-behind mirror, boot routes to `AutoRecover`, rebuilds from the
mandatory pre-restore snapshot — which is by construction the state the operator rolled
back *from* — and silently UNDOES the rollback while filing the restored database away
as `.CORRUPT-`. Refuse-to-boot and silently-revert were the only two outcomes available.
The journey test asserts against both.

**The gate.** `journey_backwards_in_place_restore_leaves_a_database_serve_can_boot`
(`apps/aberp/tests/adr0116_restore_journey_e2e.rs`) walks snapshot → diverge → boot-check
→ acknowledged backwards rollback → **boot** → chain verifies from genesis → the
rollback survived. It asserts twice over: through the shipped binary, and directly
against `ensure_consistent_with_db` — the exact call `serve.rs` makes at every boot — so
the gate does not rest on one flag's wiring.

**`aberp serve --boot-check`, and what it does not cover.** Boot has two halves. The
first (open the tenant DB, ensure the audit-ledger schema, reconcile the mirror, route
the result) is the half a restore can break and the half that decides whether serve
starts at all. The second (session token, NAV credentials, TLS cert, listener) is about
talking to the outside world and is independent of what the restore put on disk.
`--boot-check` runs the first half through the real `serve::run` and exits.

It exists because the alternative for a gate is spawning a real `aberp serve`, which
reads the operator's **actual OS keychain** — the test bypass is `#[cfg]`-compiled out
of every `--features production` build — and can block on an ACL prompt after any
rebuild. A hermetic test could not otherwise walk the one step an operator cannot skip.
Stated plainly: `--boot-check` does **not** prove the listener binds or that TLS is
valid.

### The seven fix-firsts

| # | Finding | Fix | Test (RED before) |
|---|---|---|---|
| **F2** | the `re-verified` line never read the installed DB — step 6 called `validate_export(export_dir)`, the identical pure function that produced `pre`, and still reported `ok` after the installed file was overwritten with garbage | new `validate_installed_db(db_path, tenant)`: opens the real file, same smoke set, and a mismatch against the snapshot's counts is a hard `InstalledVerifyFailed` naming the `.PRE-RESTORE-` unit | `f2_installed_verification_reads_the_file_on_disk_not_the_export`, `f2_in_place_restore_fails_loudly_when_the_installed_db_does_not_match` (which pins the CALL SITE, comments stripped) |
| **F3** | `--snapshot 2` resolved to seq **24** (identity prefix tried before the bare seq), and `--snapshot 2` was refused as ambiguous when seq 2 was unique | exact bare seq first; the identity form must contain `@` before prefix-matching; a numeric selector is a seq and only a seq (it no longer falls through to the substring form, where the same defect lived one door down) | `f3_a_bare_seq_that_does_not_exist_resolves_to_nothing_not_to_a_prefix_match`, `f3_a_unique_bare_seq_still_resolves_when_a_higher_seq_shares_its_digits`, `f3_a_recycled_bare_seq_still_refuses_rather_than_guessing` |
| **F4** | an unreadable live `audit_ledger` recorded `-1`, which `.max(0) as u64` turned into a confident **0** — the D3.3 gate silently disarmed in exactly the scenario a restore is for | `LiveCounts` carries `Option<i64>`; an unknown head reports `UNKNOWN`, never `EXACT … 0`, and REFUSES without `--accept-data-loss`. The mirror's LOWER bound must not paper over it either | `journey_an_unreadable_live_audit_table_is_unknown_and_refuses` |
| **F5** | the gate compared against the recorded `meta.audit_count` two lines after saying *"never trust the recorded verdict for a decision this destructive"* | compare against `pf.live.audit_count`, the number the live re-validation just produced. `meta.json` is evidence, not authority | `journey_a_tampered_meta_json_cannot_disarm_the_data_loss_gate` |
| **F6** | the snapshot WRITE path — the artefact the whole feature exists to create — had **zero fsyncs**, the exact shape D3.1 called unacceptable on the restore path | `fsync_export_dir` (every file, then the partial dir) before the finalize rename, then `fsync_dir(store_dir)` after it | `f6_the_snapshot_write_path_fsyncs_before_it_publishes_the_rename` |
| **F7** | CHECK 11d asserted the scanner's `GUARDED` verdict, which was still **token presence**, so `if false && …is_protected_evidence(…)` left the guard dead and the gate GREEN | scanner verdicts `DEAD_GUARD` for a guard dead by construction (constant-false conjunction, constant-true disjunction, discarded result), evaluated over a STATEMENT so a rustfmt split cannot hide the operator; new CHECK 11e fails on any; plus the behavioural test a scanner cannot substitute for | `f7_prune_refuses_a_protected_directory_and_does_not_report_it_removed` (kills M1), + 3 negative probes |
| **F8** | the scanner could not see `archive_then_remove` — the ONE fn whose job is unlinking evidence — because it works through `artefact.path` and `dest` and mentions no tenant-home token; and a removal spelled through a direct import escaped the `fs::remove_(` regex entirely | three files are tenant-home-reaching BY FILE (`evidence.rs`, `recover.rs`, `crash_safe.rs`); removal matcher widened to the bare spelling, excluding method calls and `fn` definitions; `archive_then_remove` + `cleanup_stale` added to the frozen manifest with their reasons | 2 negative probes (M5, un-freezing `archive_then_remove`) |

### CHECK 11 can no longer be fooled into a silent green

The F7 story is the second time this check was flipped by editing something that is not
the guard: rev 2's first cut by editing a **comment**, rev 3's by editing an
**operator**. Both times the gate was green, the code was correct, and the check was
worthless. So the hardening is not only the new detection — it is **11f, matcher
liveness**, on the 10P-0 pattern:

- a live guard must verdict `GUARDED` (or 11d could never pass honestly);
- `if false && is_protected_evidence(..)` must verdict `DEAD_GUARD`, on one line **and**
  split across lines;
- a bare `remove_file(p)` after `use std::fs::remove_file;` must verdict `TENANT_HOME`;
- and the widened matcher must NOT fire on `self.remove_file()`,
  `guarded_remove_file()`, or a `fn remove_file` definition — a gate that cries wolf
  gets switched off, which is its own silent green.

Seven new negative probes plant each mutation in a real tree copy and assert the gate
goes RED, including two that neuter the *scanner* to prove the liveness fixtures have
teeth. Harness cost: 69 → 76 probes.

### The two drift rulings (Ervin, 2026-08-29)

**(a) `--accept-data-loss` — KEEP.** Rolling backwards past committed rows *is* what a
rollback is, and a banned operation's workaround is `cp`, which is the thing this
programme exists to eliminate. Conditions applied:

- F4 and F5 fixed, so the gate is not absent exactly when it matters;
- an UNKNOWN live head reports `UNKNOWN` and refuses without the flag — it never prints
  `EXACT … 0`;
- the `WARN ADR-0116 D3.3 — --accept-data-loss was passed: this restore will DISCARD
  committed audit entries` line fires on use, on the UNKNOWN path too;
- and the discarded count now lands on **stdout** in the completion message and on the
  **restored chain** (`SnapshotRestoredPayload::discarded_audit_rows`), not only in a log
  line and shell history.

**(b) The Defense anchor sanction — KEEP warn-on-zero.** With zero anchor rows
everywhere the flag is never required, so it cannot become muscle memory; under the
ADR's literal reading every Defense restore refuses today and a flag that must always be
typed stops carrying information within a week. Condition applied: the anchor verdict is
recorded in the `SnapshotRestored` audit payload
(`anchor_verdict` + `anchor_coverage`), so **the restored chain itself carries what
coverage it was restored under**. That is the fact a court would ask about, and it
previously existed only in a stderr warning nobody keeps. Recorded on the side-path
restore too, since that command also produces a restored database.

Both payload fields are `#[serde(default)]`; a pre-rev-3 row reads back as
`"not-recorded"`, never as `"no anchors"`.

### One finding rev 3 added on its own — F5's class, in the anchor sanction

Found while applying Ervin's KEEP ruling on drift (b), not named by the review.

The Defense anchor sanction read `meta.anchor_count` / `meta.anchored_through_seq`
straight off `meta.json` — which is F5's exact shape one function over. `meta.json` is a
plain file beside the export with no integrity binding to it, so editing `anchor_count`
to `0` there downgrades a Defense `ShortCoverage` **refusal** to a warning. A one-line
bypass, in a file an operator can write, of the sanction that had just been ruled KEEP.

Fixed the same way F5 was: the verdict, the warning, the stdout line and the audit-row
field all come from the LIVE re-validation (`anchor_verdict_live` / `describe_anchors_live`),
including on the side-path restore, which pays one extra in-memory `IMPORT` for a number
that goes into a row a court may read. The pre-flight prints **both** when they disagree,
because a snapshot that misdescribes itself is a finding, not a detail to reconcile
silently. Pinned by
`journey_the_anchor_verdict_comes_from_the_live_revalidation_not_meta_json`.

### One more sanctioned non-shared audit writer

`take::rebuild_mirror_for_restored_db` writes the fresh mirror, so CHECK 10P caught it
and it is registered in `tools/adr0099_audit_writer_residuals.txt`. It is the strictest
provenance in that file, not the weakest: it is reachable only from `restore_in_place`,
whose caller refuses to proceed unless DuckDB's own **exclusive file lock** proves serve
is stopped — and re-asserts it at the commit point.

### Not fixed in rev 3, and why

F9 (a seq collision inside one store leaks a condemned snapshot), F10 (the prod-shaped
store `~/Documents/ABERP-snapshots/` reads as wholly protected — latent, bites on the
port back to the prod line), F11 (the D1.2 skipped-tick checkpoint is unpinned) and the
three notes (`diverged-*` missing from `EVIDENCE_FRAGMENTS`, `guarded_remove` not being a
live-file guard, retention reading the recorded `meta.valid`) are all **lower severity by
the review's own ranking** and none is a data-loss path on this edition. They are left
for the next pass rather than bundled into a blocker fix, deliberately: this branch goes
back to adversarial before the v0.6.3 cut, and a wide diff is harder to attack.

---

## Gate results

Recorded in the branch's final commit message. Both required checks
(`ADR-0093 DB-isolation cut-gate`, `defense · build + lint + test`) plus fmt,
`clippy -D warnings`, the full workspace test, and all four probe harnesses.
No de-gating, no `continue-on-error`, no skipped probe.

### Rev 3 measured results (2026-08-29, worktree `~/Documents/Claude/Projects/ABERP-snap-wt`, `--features production`)

| Gate | Result |
|---|---|
| `ADR-0093 DB-isolation cut-gate` (CHECK 1-11, all ENFORCED) | **PASSED** — 29 removal sites classified, 3 guarded, 9 frozen tenant-home sites |
| `cut_gate_negative_probes.sh` | **PASSED — `probes passed: 77   broken/escaped: 0`** (69 -> 77; 17 of them CHECK 11) |
| `ADR-0111 checkpoint-site cut-gate` | **PASSED** |
| `ADR-0110 D3 durable-ack gate` | **PASSED** |
| `cargo fmt --all -- --check` | clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| `cargo build --workspace --all-targets` | clean |
| `cargo test --workspace --all-targets --no-fail-fast` | 180 test binaries green; 3 red — the 2 known environmental `aberp-cad-extract-wrapper` reds and the pre-existing flake below |
| `cargo deny check` | advisories ok, bans ok, licenses ok, sources ok |

No enforcement flag was disabled and no check was skipped.

### Rev 3 note — a test-isolation flake that is NOT ours

`apps/aberp/tests/serve_numbering_route.rs::put_preserves_identity_and_bank_sections`
can go red in a full parallel run. `unique_tmpdir()` in that file keys its scratch
directory on `pid` + `SystemTime` nanos, and two tests in the SAME binary running on two
threads can read the same coarse nanos value, land in the same directory, and race on one
`seller.toml`. It passes when run alone. Nothing on this branch touches that file, that
route, or the numbering module — its last commit is `b0cdb2e` (S165). Recorded here so
the next full-suite run does not re-derive it, alongside the known environmental
`aberp-cad-extract-wrapper` reds (`step_extract_smoke`, `hole_mining_failure`).
