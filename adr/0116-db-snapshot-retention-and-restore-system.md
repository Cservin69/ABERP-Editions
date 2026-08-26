# ADR-0116 (Editions) — Snapshot cadence, evidence retention, and a routine guarded restore

- **Status:** **Proposed — design only. No product code in this change. Adversarial review OWED.**
- **Date:** 2026-08-27
- **Deciders:** Ervin Áben (scope: close the recovery half of the durability programme — the write path was hardened by v0.6.0/0.6.1/D-22; recovery is still hand-run per incident). Investigation + design by Dispatch.
- **Base:** Editions `docs/dream-shop-workflow` @ `1c7c686`. Every file:line and every number below was reproduced in this session at that SHA.
- **Related:** **ADR-0082** (the snapshot system this ADR extends — `EXPORT DATABASE` logical snapshots, the `snap-<seq>-<ts>` store, the GFS retention math); **ADR-0087/S441** (timestamp-anchored audit chain — `audit_ledger_anchors`, live in code); **ADR-0095** (crash-safe durability + boot auto-recovery); **ADR-0098/0099** (the audit-fork lesson — one shared `Handle`, never a second writer); **ADR-0110 D3** (`durable_ack`); **ADR-0111** (checkpoint under the writer lock); `[[trust-code-not-operator]]`, `[[no-sql-specific]]`, `[[hulye-biztos]]`.

> **On the number.** `0116` is provisional. `adr/` holds files through `0105` plus `0111` and `0112`; `0110`, `0113`, `0114` and `0115` are referenced from `docs/` and `SAW-OFF.md` but have **no file in this tree** — they are in-flight provisional numbers (the D-22 money-CLI ADR is cited as `0114`, and the auto-probe / portal work holds others). `0116` is free in this tree today. **It must be reconciled at merge** against whatever the auto-probe and portal branches actually claimed; this ADR asserts no priority over them.

---

## Context

Four prod durability incidents in three months — **2026-06-22**, **2026-08-08**, **2026-08-10**, **2026-08-22** — across torn writes, absent checkpoints, and mirror-ahead-of-DB divergence. The **write** path has since been hardened substantially: `durable_ack` (ADR-0110 D3), checkpoint-under-the-writer-lock (ADR-0111), the inode fence, audit-fork detection (ADR-0098/0099), and the D-22 money-CLI durability closure.

The recovery path was assumed to be the remaining hole. **It is not a hole — it is a partially-wired system with four specific defects.** This ADR was commissioned to *design* periodic snapshots, retention, and a restore CLI. Grounding the design in the tree first (as instructed) established that **all three already exist and ship**. Designing them again would have produced a duplicate subsystem beside a working one.

What is actually owed is narrower and sharper: the cadence does not run when it matters, the restore path is the one file-install path in the tree with **no `fsync` at all**, recovery *evidence* is entirely ungoverned (1.1 GB and growing), and the snapshot validator does not check the anchors that give a restored chain its legal weight.

---

## Current state — what already ships (code-grounded)

| Capability | Where | Status |
|---|---|---|
| Logical snapshot (`EXPORT DATABASE ... PARQUET`) | `crates/aberp-snapshot/src/take.rs:144` `take_snapshot` | **Live** |
| Validation (re-`IMPORT` in-memory + smoke + `verify_chain`) | `take.rs:60` `validate_export` | **Live** |
| Per-tenant store `~/Documents/ABERP-snapshots[-<edition>]/<tenant>/` | `store.rs:71,96` | **Live** (`prod`, `test`, `defense` populated) |
| GFS retention math (keep-last 24 + daily 30 + weekly 52) | `retention.rs:62` `plan_retention` | **Live**, pure, exhaustively unit-tested |
| Pruning | `retention.rs:158` `prune` | **Live** |
| Periodic daemon, 4 h cadence, 60 s boot delay | `apps/aberp/src/snapshot.rs:700` `run_supervised` | **Live** inside `aberp serve` |
| Restore (`IMPORT` → staging → rename) | `take.rs:397` `restore_into` | **Live** |
| Restore guard (`--confirm` + refuse under a live `~/.aberp/` home) | `take.rs:287` `ensure_restore_allowed` | **Live** |
| CLI `aberp snapshot {now,list,restore}` | `apps/aberp/src/snapshot.rs:572,611,640` | **Live** |
| HTTP `GET /api/snapshots`, `POST /api/snapshots/{now,restore}` | `serve.rs:4327-4329` | **Live** |
| Audit events `snapshot.{created,validation_failed,restored,pruned}` | `snapshot.rs:403,429,471,557` | **Live** |
| Boot auto-recovery + refuse-on-ambiguity | `recover.rs:189` `recover_or_refuse_with_audit` | **Live** |
| Ahead-snapshot self-certification (`topped_head != chain_len` → `Refuse`) | `recover.rs:283-390` | **Live** |
| `.CORRUPT-<tag>` evidence retention on recovery | `recover.rs:400,928` | **Live** |
| Timestamp anchoring (`audit_ledger_anchors`, `take_anchor`) | `crates/audit-ledger/src/session/mod.rs:185…366` | **Live** (ADR-0087 says *Proposed*; the status line lags the code) |

Two design properties are already correct and this ADR does **not** disturb them:

- **The audit-fork lesson is honoured.** `SnapshotAudit::{Handle,Reopen}` (`snapshot.rs:288`) routes every in-process snapshot audit append through the ONE shared `aberp_db::Handle`; only the separate-process CLI reopens, where no `Handle` exists and a fork is therefore impossible. The seq-515 fork is closed at this seam.
- **The checkpoint folded into the daemon cadence goes through the shared handle**, not by path (`live_checkpoint_logged`, ADR-0111) — the orphaned-inode defect is closed here.

---

## The gaps

### G1 — The cadence is bound to `aberp serve` uptime, so the real RPO is *days*, not 4 hours

`run_supervised` is spawned by `aberp serve`. When serve is not running, no snapshot is taken — there is no catch-up, no missed-tick detection, and nothing outside the process ever calls `take_snapshot`.

Measured in the live prod store (`~/Documents/ABERP-snapshots/prod/`, `meta.json` per snapshot):

```
seq 75  2026-08-15T13:27:51  valid  invoices=12  audit=8571
seq 76  2026-08-17T06:23:12  valid  invoices=12  audit=8579
seq 77  2026-08-17T10:23:13  valid  invoices=12  audit=8591
seq 78  2026-08-23T16:27:23  valid  invoices=12  audit=8604
seq 79  2026-08-26T10:19:21  valid  invoices=12  audit=8611
```

**5 snapshots across 10 d 20 h. A 4-hour cadence predicts 65.** The largest gap — `seq 77 → seq 78` — is **6 days 6 hours**, and **the 2026-08-22 incident falls inside it.** The snapshot system was, at that moment, offering a rollback point five days stale. The defense store shows the same shape (4 snapshots, 08-24 → 08-26).

This is the single highest-value gap: every other part of the system is sound, and it is protecting a window it is mostly absent from.

*(Related, minor: the loop sleeps `interval` **after** the cycle completes, so the cadence drifts by one cycle duration per tick rather than running on a fixed wall-clock grid.)*

### G2 — `restore_into` performs no `fsync` whatsoever

`crates/aberp-snapshot/src/take.rs:397` builds a staging file, `CHECKPOINT`s it, and `std::fs::rename`s it over the target:

```rust
let conn = Connection::open(&staging)?;
conn.execute_batch(&format!("IMPORT DATABASE {};", sql_quote(export_dir)))?;
conn.execute_batch("CHECKPOINT;")?;
// …
std::fs::rename(&staging, target).map_err(|e| SnapshotError::io(target, e))?;
```

`grep -c sync_all take.rs` → **0**.

Its sibling `crash_safe.rs:221` `atomic_install` does the full textbook recipe — and documents it: fsync the staging file, atomic rename, **fsync the parent directory** (`fsync_file` at `:182`, `fsync_dir` at `:192`, plus the `install-intent` journal at `:92` for a torn rename). `restore_into` uses none of it.

So the one path whose entire job is *producing a trustworthy database after a durability incident* is itself not crash-safe. A power loss during or just after `aberp snapshot restore` can leave the target absent, zero-length, or with the rename unpersisted — the exact failure class D3 closed everywhere else. **This is the most serious defect found, and it is also the cheapest to fix: route the install through the primitive that already exists ten files away.**

### G3 — Restore is neither previewable nor routine

- **No dry-run.** `grep -rn "dry_run" ` across `snapshot.rs`, `cli.rs`, and the crate → **0 hits**. An operator cannot ask "what would this restore, from when, with how many rows" before writing a database.
- **`--confirm` is the whole guard.** `ensure_restore_allowed` refuses a target under a live `~/.aberp/` home and otherwise requires `--confirm`. Sound as far as it goes, but it is a *destination* check, not a *decision* check: nothing compares the snapshot against the live DB, so the operator has no machine-checked answer to "am I about to roll back 5 days of invoices?"
- **In-place recovery remains hand-run.** By design the CLI refuses to write a live tenant home; the documented procedure is *restore to a side path → stop serve → swap the file in by hand*. That hand-swap is precisely the per-incident manual step this programme set out to eliminate, and it is unjournalled: a crash mid-swap is on the operator, not on the code. `[[trust-code-not-operator]]` is satisfied for the *guard* and violated for the *procedure*.

### G4 — Validation does not cover the anchors, so a restored chain's legal weight is unverified

`validate_export` (`take.rs:60`) gates on three things: `invoice` count (informational), `audit_ledger` count (hard), and `verify_chain` (hard). `EXPORT DATABASE` captures **all** tables, so `audit_ledger_anchors` rides along automatically — but nothing checks it. ADR-0087 explicitly requires that *"anchors must survive restore"*, and ADR-0087's own court-admissibility argument is that **weight comes from the qualified timestamp over the chain head, not from the hash chain**. A snapshot can therefore be marked `valid=true` on hash-chain grounds while carrying zero or truncated anchor coverage, and a DB restored from it silently loses the eIDAS Art. 41(2) presumption it is supposed to preserve.

### G5 — Recovery evidence is completely ungoverned: 1.1 GB, 34 artefacts, no policy

Evidence is retained by design and never deleted (`recover.rs:1107` — *"retained `.CORRUPT-` evidence is NEVER removed"*). Correct as a safety rule; absent as a lifecycle.

Critically, **evidence does not live in the snapshot store** — it is written as siblings of the live DB inside `~/.aberp*/<tenant>/`:

```
aberp.duckdb.CORRUPT-20260705T184449Z            7.9M
aberp.duckdb.CORRUPT-BACKUP-20260629T140040Z     7.9M
aberp.duckdb.audit.log.AHEAD-manual-20260810T172538Z
aberp.duckdb.audit.log.DEEPCORRUPT-manual-20260810T165806Z
…
34 artefacts, 1.1 GB total
```

`plan_retention` cannot see any of it: it operates on `SnapshotRecord`s, which `list_snapshots` builds only from `snap-*` directories containing a parseable `meta.json` (`store.rs:190`). Evidence has neither. So the protection the brief asked for (`*CORRUPT*`/`*RECOVERY*`/`*DEFORK*`/`*PRE-*` must never be pruned) is currently satisfied **by accident** — the pruner is structurally blind to those files rather than deliberately protective of them. That is a fragile guarantee: any future "clean up the tenant home" helper would meet no guard at all. Meanwhile the growth is unbounded and sits in the same filesystem as the live DB, where exhaustion is itself a durability event.

### G6 — Unexplained retention shortfall (open question, not a claim)

`keep_last` defaults to 24 and no `ABERP_SNAPSHOT_*` override appears anywhere under `run/` or `scripts/`. The prod store nonetheless holds **5** snapshots at seqs 75–79; seqs 56–74 are absent. `plan_retention` cannot produce that outcome — rule 2 keeps the 24 most recent *valid* snapshots unconditionally, and every surviving snapshot is `valid=true`.

The likely explanation is manual deletion (disk pressure) rather than a pruner defect, but **this was not confirmed in this session and must not be assumed.** Flagged for Ervin below.

### G7 — No snapshot is taken at the moments that most warrant one

There is no snapshot trigger on clean shutdown, on boot-after-unclean-shutdown, before a restore, or before a schema migration. The clean-shutdown path takes a *checkpoint* (`checkpoint_on_clean_shutdown`) — which makes the live file crash-safe but produces **no rollback point**. The pre-restore case is the sharpest: restoring is the single most destructive operation the system offers, and it does not first preserve what it is about to overwrite.

---

## Decision

Five changes, all additive, all app-layer, no SQL-specific mechanism (`[[no-sql-specific]]`: no triggers, no engine-side jobs — the cadence, the policy, and the guards are Rust).

### D1 — Make the cadence real: wall-clock grid + staleness catch-up + an out-of-process floor

Three parts, in increasing order of what they cost:

1. **Anchor the loop to a wall-clock grid.** Sleep to the next `interval` boundary rather than `interval` *after* the cycle, removing per-tick drift.
2. **Catch-up on start and on every tick.** Before sleeping, compare `now` against the newest snapshot's `created_at` in the store. If the store is staler than `interval`, take one immediately. This alone converts "serve restarted after 6 days down" from *no snapshot until +4 h* into *a snapshot at +60 s*, and it is the cheapest meaningful RPO improvement available.
3. **An out-of-process floor, so the RPO does not depend on serve being up at all.** `aberp snapshot now` is already a complete, fork-safe, separate-process entrypoint (`SnapshotAudit::Reopen`). Schedule it — on macOS a `launchd` `StartCalendarInterval` agent; the daemon's own catch-up check makes the two idempotent (whichever runs first satisfies the window and the other no-ops). **Conservative default chosen: once daily at 03:00 local**, purely as a floor under the 4-hour in-process cadence, not a replacement for it.

**Explicitly not chosen: write-count-based triggering.** It is the more precise signal — RPO measured in transactions rather than hours — but it requires a counter on the shared `Handle`'s write path, which is the most safety-critical, most recently-hardened code in the tree (ADR-0110/0111). Adding a side-effecting hook there to serve a *backup* cadence is a poor risk trade while the time-based gaps are this large. Revisit once D1.1–D1.3 land and the measured RPO is hours rather than days.

The daemon keeps its current correctness properties unchanged: `spawn_blocking`, audit through the shared `Handle`, logged-but-survives on every error, `EXPORT` non-blocking to serve (a logical read, not a writer-lock holder).

**On the second opener.** `take_snapshot` deliberately opens its own short-lived `Connection` for the `EXPORT` (`take.rs:195`), with a documented rationale: the alternative holds the single writer mutex for the entire multi-second export every cycle. That reasoning holds for the read, and this ADR does not disturb it. **But it is worth stating precisely, because the same connection also *writes*:** at `take.rs:210-225` it runs the ADR-0098 Gap 2b mirror reconcile + fsync. That is a write on a non-`Handle` connection, i.e. exactly the shape the audit-fork lesson forbids — mitigated today only by it being a mirror reconcile rather than a ledger append, and by its errors being surfaced-not-fatal. **Flagged for the adversarial review** rather than changed here: it may be sound, but "the export connection is read-only" is stated in the comment and is not literally true, and that gap between comment and code is the exact shape of the last three fork incidents.

### D2 — Two retention domains: snapshots (keep the existing math) and evidence (new)

**Snapshots.** `plan_retention` is correct, pure, well-tested, and its floors are right (newest-valid is sacred; never drop to zero). **Keep it as-is.** The only change is to make its protection *deliberate* rather than incidental: `prune` gains an explicit refusal — a directory whose name matches `*CORRUPT*`, `*RECOVERY*`, `*DEFORK*`, `*PRE-*`, or `*.ahead-*` is never removed, even if a caller hands it a plan naming it. Today that safety rests on such directories failing to parse as snapshots. Make it a guard, not a side effect. *(A pruner that refuses a directory it was told to remove must log LOUD, per house rule #12 — silent protection is how a stale plan becomes invisible.)*

**Evidence (new).** A tiered policy over `~/.aberp*/<tenant>/` artefacts, applied by an explicit operator command and **never** by the periodic daemon (evidence deletion is not a background activity):

- **Never auto-delete** anything younger than **90 days**, or belonging to the **3 most recent distinct incidents**, whichever is the larger set.
- **Never delete the only artefact of an incident.** An incident is keyed by its timestamp tag; `.CORRUPT-<tag>` and any `.ahead-<nanos>.bak` / `AHEAD-*` / `DEEPCORRUPT-*` sharing that incident are one unit, kept or released together — a de-fork is unreconstructable from half its evidence.
- **Never delete anything referenced by an unresolved `db.auto_recovered` audit event**, so the ledger, not a filename, decides what is still live evidence.
- **Release is archive-then-remove**: evidence leaving the live tenant home is first written to `~/Documents/ABERP-evidence/<tenant>/<incident-tag>/` (compressed) and only then unlinked, so "pruned" never means "gone".

**Conservative default chosen:** with a 90-day floor, today's 34 artefacts / 1.1 GB reduce to roughly the 2026-06/07 cohort being archivable and the August set retained. Nothing is deleted without an explicit command.

### D3 — Make restore crash-safe, previewable, and routine

**D3.1 — Route the install through `atomic_install`.** `restore_into` stops calling `std::fs::rename` directly and instead uses `crash_safe::atomic_install`, inheriting fsync-file → rename → fsync-dir and the `install-intent` journal. This is a small change to one function and it closes G2 outright. The stale-WAL deletion must move inside the same journalled sequence (a WAL surviving a restored file is itself a corruption vector — `restore_into`'s existing comment says so).

**D3.2 — `--dry-run`.** Prints what would happen and writes nothing: chosen snapshot (seq, age, byte size), its validation verdict re-run live, its `invoice_count` / `audit_count` / `chain_len` / anchor coverage, the resolved target path, whether that target exists, and — the part that matters — **the delta against the live DB**: rows and invoices that exist now and would not exist after. Exit code distinguishes *would proceed* from *would refuse*.

**D3.3 — Verify-before-restore, and refuse on ambiguity.** Before writing, and beyond today's `validate_export`: re-verify the chain end-to-end, verify anchor coverage (D4), and confirm the snapshot's genesis matches the target tenant's. **Refuse — never guess —** when the selector matches more than one snapshot, when the snapshot's tenant differs from `--tenant`, when the snapshot is `valid=false`, or when the live DB is *ahead* of the snapshot in a way the operator has not acknowledged. This mirrors `recover_or_refuse`'s posture, which is the right one and is already proven in the boot path.

**D3.4 — A guarded in-place restore, replacing the hand-swap.** `aberp restore --in-place` performs the sequence the operator performs today, in code and journalled: refuse unless serve is stopped (fail on a live lock, do not race it) → **snapshot the current DB first** (closing G7's sharpest case — the thing about to be overwritten is preserved before it is overwritten) → move the current DB aside as `.PRE-RESTORE-<tag>` evidence → `restore_into` via `atomic_install` → re-verify the installed file → emit `snapshot.restored`. Any failure leaves the `.PRE-RESTORE-` artefact and the original path recoverable.

**D3.5 — Do not poison the audit index.** The 2026-08-03 heal-path lesson: a restore writes its `snapshot.restored` row *after* the new DB is installed and *through the same append path as every other event* — never by pre-seeding a seq, never by writing into the restored file out-of-band, never by reconciling the mirror to match a restored head. The restored DB's chain is whatever the snapshot certified; the restore event is the next entry on it, not an edit to it. If the mirror disagrees with the restored DB, that is `recover_or_refuse`'s decision at next boot, and the restore must leave it that way rather than pre-resolving it.

### D4 — Anchors participate in validation and travel with the snapshot

Extend `ValidationReport` and `SnapshotMeta` with `anchor_count: i64` and `anchored_through_seq: u64` (the highest `audit_ledger` seq covered by a verified anchor). `validate_export` reads `audit_ledger_anchors` from the re-imported in-memory DB and verifies each anchor against the chain head it claims to cover.

**Conservative default chosen: a missing or short anchor set does NOT fail validation** — it is recorded and surfaced. Anchoring is a live but young capability (ADR-0087 is still marked *Proposed*), the historical snapshots in the store predate it entirely, and failing validation on it would immediately invalidate every existing rollback point — turning a legal-weight gap into an availability incident. Restore's `--dry-run` and its pre-flight report the anchor coverage prominently so the operator sees what a restored DB will and will not be able to prove. **Flagged: Ervin may want this to be a hard gate for Defense specifically**, where court-admissibility is the point of the edition.

`SnapshotMeta` gains fields additively; `meta.json` is `serde`-parsed per directory and older files must keep parsing, so the new fields are `#[serde(default)]`. Note the ADR-0087 fact-1 discipline applies here by analogy: nothing in this change may enter the `entry_hash` preimage.

### D5 — Snapshot at the moments that warrant one

Add triggers, all reusing the existing `run_cycle`: **before any restore** (D3.4, non-optional), **on clean shutdown** (alongside the existing checkpoint, skipped if a snapshot is already within `interval`), and **on boot after an unclean shutdown or a successful auto-recovery** (so the recovered state is itself a rollback point). Each is subject to the same staleness check as D1.2, so none of them can produce a snapshot storm.

---

## CLI surface

Existing commands keep their shape and their flags; additions are marked **new**.

```
aberp snapshot now      [--db P] [--tenant T] [--store P]
                        [--if-stale <dur>]                        # new: no-op if a snapshot is newer than <dur>

aberp snapshot list     [--tenant T] [--store P]
                        [--json]                                  # new: machine-readable, for the launchd floor
                        [--verify]                                # new: re-run validation live, don't trust meta.json

aberp snapshot restore  <selector> --to P --confirm [--tenant T] [--db P] [--store P]
                        [--dry-run]                               # new: D3.2, writes nothing
                        [--verify-only]                           # new: run the pre-flight, report, exit

aberp restore --in-place --tenant T --snapshot <selector> --confirm    # new: D3.4, replaces the hand-swap

aberp snapshot prune    [--tenant T] [--store P] [--dry-run]       # new: retention on demand, not only via the daemon

aberp evidence list     [--tenant T]                               # new: D2, the 1.1 GB nobody can currently see
aberp evidence archive  --older-than <dur> [--tenant T] [--dry-run] [--confirm]   # new: archive-then-remove
```

`--dry-run` and `--confirm` are mutually exclusive everywhere they both appear. Every mutating command emits its audit event through the routing that already exists (`SnapshotAudit::{Handle,Reopen}`); `aberp evidence archive` needs a new `EventKind` (**+1 to the kind count — reconcile against the 195 in the QC-report work before merge**).

---

## Phasing

**Phase 1 — works today, no hardware, no endpoint, no schema change.** D3.1 (`atomic_install` in `restore_into`), D1.1 + D1.2 (grid + catch-up), D2's explicit prune refusal, `--dry-run` / `--verify-only` / `--json` / `--if-stale`. This is the bulk of the risk reduction and touches one crate plus the CLI. **D3.1 should not wait for the rest** — it is a few lines against a primitive that already exists, and it is the difference between a restore that survives a power cut and one that does not.

**Phase 2 — still no hardware.** D3.4 (`--in-place`), D5 (the triggers), D2's evidence commands and archive store, `aberp snapshot prune`.

**Phase 3 — depends on the anchoring rollout.** D4. The mechanism is live in `audit-ledger`, but the coverage question ("is a TSA reachable, and how much of the historical chain is anchored") is an operational one; D4's *recording* half can land in Phase 2, and only the *gating* decision waits.

**Phase 4 — the out-of-process floor (D1.3).** Deliberately last, not because it is hard but because it is a host-level artefact (`launchd`) rather than a code change, and because D1.2's catch-up already recovers most of its value. It also needs the keychain/binary-identity constraint checked: an unattended scheduled `aberp snapshot now` on Defense may hit the ad-hoc-signing ACL prompt that blocks unattended boots after a rebuild, in which case the floor silently never runs — **verify before relying on it.**

Nothing here depends on a machine, an endpoint, or a NAV/TSA connection except D4's gating decision.

---

## Open decisions — flagged for Ervin

1. **G6: why does prod hold 5 snapshots when `keep_last` is 24?** Seqs 56–74 are gone and `plan_retention` cannot have removed them. Manual deletion under disk pressure is the likely answer, but if it is *not*, there is a pruning defect this ADR has not diagnosed. **This should be settled before Phase 1 ships**, because everything else assumes the retention math is trustworthy.
2. **Should D4 hard-gate on Defense?** Recommended default is record-don't-fail everywhere (above). Defense is the edition whose entire premise is court-admissibility, so a *restored* Defense DB with no anchor coverage is arguably not fit for purpose. Counter-argument: it would invalidate every pre-anchoring snapshot at once.
3. **Is the daily 03:00 out-of-process floor the right cadence?** Chosen conservatively. Given a measured 6-day gap, even weekly would have helped; hourly would make serve-uptime irrelevant but multiplies store growth (~1.8 MB/snapshot today, so hourly ≈ 43 MB/day before retention).
4. **90-day evidence floor and the archive location.** `~/Documents/ABERP-evidence/` mirrors the snapshot store's "outside the repo, outside `~/.aberp/`" property. Confirm 90 days is long enough for the incident cadence actually seen (four incidents in ~10 weeks suggests it is comfortably so).
5. **The `take.rs` export connection also writes** (the Gap 2b mirror reconcile at `:210`). Flagged above; needs the adversarial review's eye specifically, since it is the one place in the current design where a non-`Handle` connection mutates state.
6. **The ADR number.** `0116` is provisional — reconcile against the auto-probe and portal branches' provisional numbers at merge, along with the `EventKind` count delta.

---

## Consequences

**Positive.** Recovery stops being per-incident hand-surgery: `--in-place` journals the swap that is currently manual, `--dry-run` lets an operator see a restore before committing to it, and the restore install inherits the same durability guarantee as every other file install in the tree. The RPO improves from *days* (measured) toward *hours* (the configured cadence). The 1.1 GB of evidence becomes visible and governed without becoming deletable.

**Negative / accepted.** More CLI surface to keep coherent. Snapshot storage grows if the floor cadence is raised. D4 adds fields to `meta.json` — additive and `serde(default)`, but a format change nonetheless. The write-count trigger is deferred, so RPO stays time-bounded rather than transaction-bounded; a burst of invoices immediately before a failure can still fall inside one interval.

**Not addressed here.** The Portable port of ADR-0111 and the HTTP restore route remain owed from PR #41 and are out of scope. Whether the second export connection should be migrated onto the shared `Handle` is raised (open decision 5) but deliberately not decided — that is a durability-core change and wants its own ADR if it is taken.

## Acceptance criteria

1. `restore_into` performs fsync-file → rename → fsync-dir via `atomic_install`; a crash-injection test kills the process between import and rename and asserts the target is either the old file or the complete new one, never torn.
2. With the daemon started against a store whose newest snapshot is older than `interval`, a snapshot is taken within `BOOT_DELAY_SECS` — not at the next interval boundary.
3. `aberp snapshot restore --dry-run` writes nothing (asserted by mtime + directory listing over the target and the store) and exits non-zero when the pre-flight would refuse.
4. `prune` refuses, and logs LOUD, when handed a plan naming a `*CORRUPT*` / `*RECOVERY*` / `*DEFORK*` / `*PRE-*` / `*.ahead-*` directory.
5. `aberp restore --in-place` leaves a `.PRE-RESTORE-<tag>` artefact and a recoverable original on every injected failure path.
6. A restored DB's `snapshot.restored` row is the next seq on the restored chain — asserted by `verify_chain` over the restored file, with no pre-seeded seq and no mirror write during the restore.
7. `validate_export` reports `anchor_count` / `anchored_through_seq`; a snapshot with zero anchors still validates (per D4's default) and the value is surfaced by `list --verify` and by restore's pre-flight.
