# ADR-0116 (Editions) — Snapshot cadence, evidence retention, and a routine guarded restore

- **Status:** **Proposed — design only. No product code in this change. Revision 2: applies the adversarial review's FIX-FIRST verdict (2026-08-27). A final design re-review follows.**
- **Date:** 2026-08-27 (rev 2)
- **Deciders:** Ervin Áben (scope: close the recovery half of the durability programme — the write path was hardened by v0.6.0/0.6.1/D-22; recovery is still hand-run per incident). Investigation + design by Dispatch. Adversarial design review by Dispatch (`docs/_adversarial-snapshot-adr-review.md`).
- **Base:** Editions `docs/dream-shop-workflow` @ `1c7c686`; revision 2 on `docs/adr-db-snapshot-system` @ `58f4364`. Every file:line and every number below was reproduced at those SHAs — the review re-checked all 27 citations and every one resolved.
- **Related:** **ADR-0082** (the snapshot system this ADR extends — `EXPORT DATABASE` logical snapshots, the `snap-<seq>-<ts>` store, the GFS retention math); **ADR-0087/S441** (timestamp-anchored audit chain — `audit_ledger_anchors`, live in code); **ADR-0095** (crash-safe durability + boot auto-recovery); **ADR-0098/0099** (the audit-fork lesson — one shared `Handle`, never a second writer); **ADR-0110 D3** (`durable_ack`); **ADR-0111** (checkpoint under the writer lock); `[[trust-code-not-operator]]`, `[[no-sql-specific]]`, `[[hulye-biztos]]`.

> **On the number.** `0116` is provisional. `adr/` holds `0001`–`0105` contiguously, plus `0111`, `0112` and this file; `0110`, `0113`, `0114` and `0115` are referenced from `docs/` and `SAW-OFF.md` but have **no file in this tree** — they are in-flight provisional numbers (the D-22 money-CLI ADR is cited as `0114`, and the auto-probe / portal work holds others). `0116` is free in this tree today. **It must be reconciled at merge** against whatever the auto-probe and portal branches actually claimed; this ADR asserts no priority over them.

---

## Revision 2 — what the adversarial review changed

The review's verdict was **FIX-FIRST**: the gap analysis was sound and every citation resolved, but eight design-level defects would have made an implementer build the wrong thing. All eight are applied here, plus one missed gap and the corrected measurements.

| # | Defect | Applied as |
|---|---|---|
| F1 | D3.1's "inherits the `install-intent` journal" was **false** | D3.1 rewritten: **delete the target WAL before the rename**, then `atomic_install`. Journal claim dropped. |
| F2 | AC-1 was a **vacuous** durability test (`rename` is page-cache atomic; a `kill -9` test passes with zero fsyncs) | AC-1 restated as a **mutation / fault-injection** assertion. |
| F3 | D2's globs missed **58 of 101** evidence artefacts; guard was on `prune`, which never touches the tenant home | D2's evidence guard redesigned: case-insensitive, live-file allow-list, shared `is_protected_evidence`, real locations, cut-gate check. |
| F4 | D3.4 silent on the **WAL** — its own AC-5 was unmet | D3.4 moves DB + `.wal` + `.ckpt-ok` as one unit; the mirror explicitly stays. |
| F5 | D3.4's mandatory pre-restore snapshot **contradicted** D3.5 | Resolved: D3.5 narrowed to *after the install*; the pre-restore snapshot's reconcile is stated and bounded. |
| F6 | Phasing **inverted** — D1.2 creates zero rollback points inside a gap | **D1.3 moved to Phase 1.** D1.2 redescribed as freshness, not RPO. |
| F7 | D4's `#[serde(default)]` → `0` relabels "not recorded" as "zero anchors" | `-1` sentinel on the in-tree `secondary_index_count` precedent; `Option<u64>`; `meta_version`. |
| F8 | D3.5 did not say **which ledger** | Stated per command. |
| §6.1 | **Missed gap:** a failed snapshot is pruned by the cycle that created it | New **G8**, closed by a D2 addition. |
| §1 | G6 was an open question | **Closed with positive proof** (manual deletion), and its `seq`-recycling consequence propagated into the CLI design. |
| §4 | G1 and G5 numbers | Re-derived from the ledger and re-measured on disk. |

Two review recommendations are adopted as decisions rather than left open: **open decision #5 is closed** (do *not* migrate the export connection — §7a of the review proved the seq-515 shape unreachable), and **open decision #2 is answered** (record-don't-fail everywhere, with the sanction moved to restore time).

---

## Context

Four prod durability incidents in three months — **2026-06-22**, **2026-08-08**, **2026-08-10**, **2026-08-22** — across torn writes, absent checkpoints, and mirror-ahead-of-DB divergence. The **write** path has since been hardened substantially: `durable_ack` (ADR-0110 D3), checkpoint-under-the-writer-lock (ADR-0111), the inode fence, audit-fork detection (ADR-0098/0099), and the D-22 money-CLI durability closure.

The recovery path was assumed to be the remaining hole. **It is not a hole — it is a partially-wired system with specific defects.** This ADR was commissioned to *design* periodic snapshots, retention, and a restore CLI. Grounding the design in the tree first (as instructed) established that **all three already exist and ship**. Designing them again would have produced a duplicate subsystem beside a working one.

What is actually owed is narrower and sharper: the cadence does not run when it matters (it ran at **18.5 %** of its configured rate over its whole life), the restore path is the one file-install path in the tree with **no `fsync` at all**, recovery *evidence* is entirely ungoverned (**~330 MB** in the tenant homes plus **~271 MB** outside them, including encrypted keychain dumps), the snapshot validator does not check the anchors that give a restored chain its legal weight, and **a snapshot that catches a defect is deleted by the same cycle that created it**.

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

- **The audit-fork lesson is honoured.** `SnapshotAudit::{Handle,Reopen}` (`snapshot.rs:288`) routes every in-process snapshot audit append through the ONE shared `aberp_db::Handle`; only the separate-process CLI reopens, where no `Handle` exists and a fork is therefore impossible. The seq-515 fork is closed at this seam. The review re-verified this for every change proposed here.
- **The checkpoint folded into the daemon cadence goes through the shared handle**, not by path (`live_checkpoint_logged`, ADR-0111) — the orphaned-inode defect is closed here.

---

## The gaps

### G1 — The cadence is bound to `aberp serve` uptime, so the real RPO is *days*, not 4 hours

`run_supervised` is spawned by `aberp serve`. When serve is not running, no snapshot is taken — there is no catch-up, no missed-tick detection, and nothing outside the process ever calls `take_snapshot`. (`tools/snapshot-prod.sh` is *not* an out-of-process floor: it is a physical `tar` of the tenant directory for pre-upgrade rollback, not a logical snapshot.)

**Derived from the audit ledger, not from the surviving store directories.** This matters: the store was manually pruned on 2026-08-26 (G6), so a directory-based measurement is measuring the wrong thing. The append-only mirror `~/.aberp/prod/aberp.duckdb.audit.log` holds the complete subsystem history — **79 `snapshot.created`, 15 `snapshot.pruned`, 2 `snapshot.validation_failed`, 2 `snapshot.restored`**.

- **79 snapshots in 71 days** (2026-06-16 → 2026-08-26). A 4-hour cadence predicts **426**. The system ran at **18.5 % of its configured cadence over its entire life.**
- **Largest gap: 8 d 20 h 36 m** (seq 26 → 27, 2026-07-11 → 2026-07-20). Others: 6 d 6 h, 5 d 4 h, 4 d 3 h, 3 d 4 h, 3 d 1 h, 2 d 23 h, 2 d 17 h, 2 d 8 h…
- **The incident gap: 6 d 6 h 4 m** (seq 77 → 78, 2026-08-17T10:23:13Z → 2026-08-23T16:27:23Z) — **the 2026-08-22 incident falls inside it.** The snapshot system was, at that moment, offering a rollback point five days stale. (Seqs 77 and 78 are adjacent in the ledger, so nothing was deleted from that gap.)
- Clean 4-hourly runs *do* appear (seqs 39–45, 61–72), confirming the cadence works **when serve is up**. The failure is uptime, not the loop.

And the gaps are not "nothing happened" time: **D-22 established that 15 CLI money-submission sites write to the DB with `serve` down.** The database changes during exactly the windows that produce no rollback points.

This is the single highest-value gap: every other part of the system is sound, and it is protecting a window it is mostly absent from.

*(Related, and **minor** — say so: the loop sleeps `interval` **after** the cycle completes, so the cadence drifts. Measured drift on the clean runs is **≈ 0.27 s per tick** (`20:01:09.406 → 00:01:09.679 → 04:01:09.948`). D1.1 is nearly free and worth doing, but it is cosmetic and must not be counted as part of the risk reduction.)*

*(Also: `ABERP_SNAPSHOT_DISABLE` and `ABERP_SNAPSHOT_INTERVAL_SECS`/`_KEEP_LAST`/`_DAILY_DAYS`/`_WEEKLY_WEEKS` **do exist in code** (`snapshot.rs:54,249-251,269`). What is true is that no `ABERP_SNAPSHOT_*` value is **set** anywhere under `run/` or `tools/` — there is no `scripts/` directory. The defaults are therefore what prod runs.)*

### G2 — `restore_into` performs no `fsync` whatsoever

`crates/aberp-snapshot/src/take.rs:397` builds a staging file, `CHECKPOINT`s it, and `std::fs::rename`s it over the target:

```rust
let conn = Connection::open(&staging)?;
conn.execute_batch(&format!("IMPORT DATABASE {};", sql_quote(export_dir)))?;
conn.execute_batch("CHECKPOINT;")?;
// …
std::fs::rename(&staging, target).map_err(|e| SnapshotError::io(target, e))?;
```

`grep -c sync_all take.rs` → **0** (confirmed across the crate: `crash_safe.rs` has 3, every other file 0).

Its sibling `crash_safe.rs:221` `atomic_install` does the durable recipe: fsync the staging file (`fsync_file` at `:182`), atomic rename, drop the target's stale WAL, **fsync the parent directory** (`fsync_dir` at `:192`). *(The `install-intent` journal at `:92` belongs to `durable_checkpoint`, **not** to `atomic_install` — see D3.1. The earlier draft of this ADR conflated them.)* `restore_into` uses none of it.

So the one path whose entire job is *producing a trustworthy database after a durability incident* is itself not crash-safe. A power loss during or just after `aberp snapshot restore` can leave the target absent, zero-length, or with the rename unpersisted — the exact failure class D3 closed everywhere else.

**And this path has been used in anger twice.** The ledger holds both `snapshot.restored` events: seq 37 → `ABERP-incident-20260803/rebuild/aberp.duckdb` (2026-08-03) and seq 57 → `ABERP-recovery-20260808/rebuilt.duckdb` (2026-08-08) — both real incident recoveries, both run through a restore with zero fsyncs.

**This is the most serious defect found, and it is also the cheapest to fix.**

### G3 — Restore is neither previewable nor routine

- **No dry-run.** `grep -rn "dry_run\|dry-run"` across `snapshot.rs`, `cli.rs`, and the crate → **0 hits**. An operator cannot ask "what would this restore, from when, with how many rows" before writing a database.
- **`--confirm` is the whole guard.** `ensure_restore_allowed` refuses a target under a live `~/.aberp*` home, refuses the frozen prod line, and otherwise requires `--confirm`. Sound as far as it goes, but it is a *destination* check, not a *decision* check: nothing compares the snapshot against the live DB, so the operator has no machine-checked answer to "am I about to roll back 5 days of invoices?" *(The HTTP route calls the same guard — `serve.rs handle_snapshot_restore` — so that surface is **not** an unguarded second door.)*
- **In-place recovery remains hand-run.** By design the CLI refuses to write a live tenant home; the documented procedure is *restore to a side path → stop serve → swap the file in by hand*. That hand-swap is precisely the per-incident manual step this programme set out to eliminate, and it is unjournalled: a crash mid-swap is on the operator, not on the code. `[[trust-code-not-operator]]` is satisfied for the *guard* and violated for the *procedure*.

### G4 — Validation does not cover the anchors, so a restored chain's legal weight is unverified

`validate_export` (`take.rs:60`) gates on `IMPORT` success, `invoice` count (informational, `-1` tolerated), `audit_ledger` count (hard), and `verify_chain` (hard). `EXPORT DATABASE` captures **all** tables, so `audit_ledger_anchors` rides along automatically — `audit_ledger_anchors.parquet` is present in all 5 prod and all 4 defense snapshot directories — but **nothing reads it**. ADR-0087 explicitly requires that *"anchors must survive restore"*, and its court-admissibility argument is that **weight comes from the qualified timestamp over the chain head, not from the hash chain**. A snapshot can therefore be marked `valid=true` on hash-chain grounds while carrying zero or truncated anchor coverage, and a DB restored from it silently loses the eIDAS Art. 41(2) presumption it is supposed to preserve.

**A datum that decides D4's default:** every `audit_ledger_anchors.parquet` in both stores is **exactly 300 bytes** — byte-identical across two tenants and 11 days of growing audit counts. (For calibration the 12-row `invoice.parquet` beside it is 679 bytes.) That is strong indication of **zero anchor rows everywhere**, though the parquet footer was not decoded. See D4 for why this makes a hard gate actively dangerous rather than merely strict.

### G5 — Recovery evidence is completely ungoverned, and the protection is accidental

Evidence is retained by design and never deleted (`recover.rs:1107` — *"retained `.CORRUPT-` evidence is NEVER removed"*). Correct as a safety rule; absent as a lifecycle.

Critically, **evidence does not live in the snapshot store** — it is written as siblings of the live DB inside `~/.aberp*/<tenant>/`. **Re-measured 2026-08-27:**

```
~/.aberp             136 M   (includes the live prod DB)
~/.aberp-defense     194 M
~/.aberp-portable     24 K
~/.aberp-backups       0 B
                   -------
                    ~330 M   total, everything included
```

**101 evidence-shaped names**, not the 34 an earlier draft claimed. The two artefacts that draft quoted (`aberp.duckdb.CORRUPT-20260705T184449Z`, `aberp.duckdb.CORRUPT-BACKUP-20260629T140040Z`) are in **`~/.aberp-defense/defense/`**, not beside the prod DB the surrounding text implied.

`plan_retention` cannot see any of it: it operates on `SnapshotRecord`s, which `list_snapshots` builds only from `snap-*` directories containing a parseable `meta.json` (`store.rs:190`). Evidence has neither. So the protection the brief asked for (`*CORRUPT*`/`*RECOVERY*`/`*DEFORK*`/`*PRE-*` must never be pruned) is currently satisfied **by accident** — the pruner is structurally blind to those files rather than deliberately protective of them. That is a fragile guarantee: any future "clean up the tenant home" helper would meet no guard at all.

**And the guard as first drafted would not have held.** Tested against the real on-disk names:

- **Case-sensitively, as first written: 58 of 101 escape.** Dominant families: 22 × `aberp.duckdb.audit.log.corrupt-<nanos>.bak` (lowercase), 9 × `healed-*.bak`, `INDEXDESYNC-BACKUP`, `pre-recovery-*`, `_evidence-*`, `_recovery-*`, `SPURIOUS-POST-DEDUP-*`, `AHEAD-manual-*`.
- **Case-insensitively, 14 still escape** — independently re-confirmed here: **all 9 `healed-*.bak` (69 MB in `~/.aberp/prod/` alone — the only surviving copies of pre-heal mirror state)**, `aberp.duckdb.INDEXDESYNC-BACKUP-20260803` **(24 MB)** and its `.wal` — the sole physical DB backup from the 2026-08-03 index-desync incident — `_evidence-20260627/`, and 2 × `SPURIOUS-POST-*`. `healed-` and `INDEXDESYNC-` match none of the five patterns in any case.

This is a **repeat of a previously-closed bug class in this repo** (the edition DB-guard escape that needed both walks made case-insensitive). Shipping it would re-open that class in the one place where the failure mode is permanent data loss.

**A further ~271 MB sits outside the tenant homes entirely:**

- **`~/aberp-snapshots/` — 214 MB.** Written by `tools/snapshot-prod.sh`: full tenant tarballs plus **`*-keychain.zip`** (encrypted NAV credentials + SMTP password — the most sensitive artefacts the system produces) plus a stray `ABERP-recovery-evidence-20260811/`. **Note the trap: this is `$HOME/aberp-snapshots`, a different directory from `$HOME/Documents/ABERP-snapshots`.**
- **`~/Documents/ABERP-recovery-20260808/` — 57 MB**, the target of the seq-57 restore recorded in the ledger.

Meanwhile the growth is unbounded and sits in the same filesystem as the live DB, where exhaustion is itself a durability event.

### G6 — **RESOLVED: the retention math is trustworthy. The shortfall was a manual Finder deletion.**

This was the open question gating everything else. It is now closed with **positive evidence**, not an absence of evidence — and it is *not* a retention bug. There is no "retention defect" anywhere in this ADR's scope.

The prod store holds **5** snapshots at seqs 75–79 while `keep_last` is 24; seqs 56–74 are absent. Four independent lines of evidence:

1. **The pruner's own record exonerates it.** All 15 `snapshot.pruned` payloads name **exactly one seq** each — `[24],[24],[1],[2],[3],[4],[5],[6],[7],[29],[30],[51],[52],[54],[55]`. **Seqs 56–74 appear in no prune event, ever.** The last cycle before the shortfall (2026-08-26T10:19:21Z) reports `retained_count: 25`. The store held 25 snapshots at that instant; today it holds 5.
2. **The deletion window is pinned to a 34-minute Finder session.** Last prune cycle 2026-08-26T10:19:21Z = **12:19 local (CEST)**; `~/Documents/ABERP-snapshots/prod/` directory mtime **Aug 26 12:52** — 33 minutes later, and a directory mtime only moves when an entry is added or removed. `.DS_Store` files written **Aug 26 12:52–12:53** in `ABERP-snapshots/`, `ABERP-snapshots/prod/`, `ABERP-snapshots/test/`, `ABERP-snapshots-defense/` and `.../defense/` — a Finder browse across all four store directories in that same window. Twenty snapshots vanished between 12:19 and 12:52 local, with no audit event, while Finder was walking those exact directories.
3. **The two apparently-impossible prune events are fully explained, and prove the pruner correct.** `pruned_seqs:[24], retained:23` fires twice *before seq 24 was ever created*. The two `validation_failed` payloads resolve it: two snapshots took seq 24, both failed `validate_export` with `"out of order: expected seq=7995, found seq=7994"`, and `plan_retention` pruned each in the same cycle that made it — correct under its documented rule that invalid snapshots have no retention value. The third seq-24 snapshot, 2026-07-09T14:24, validated and survived. *(That same correctness is also **G8** below: the pruner destroyed the only artefact of a live audit-chain fork, twice.)*
4. **The code has no other deletion authority.** `prune` (`retention.rs:158`) only removes `rec.dir` for records `list_snapshots` produced, and `list_snapshots` (`store.rs:190`) only admits `snap-<seq>-<ts>` directories with a parseable `meta.json`. It never reads a tenant home. Cross-checked against the **frozen prod tree** that actually wrote this store: `~/ABERP/crates/aberp-snapshot/src/retention.rs` is **byte-identical** to Editions'.

> **Ruling: the retention math is trustworthy. Phase 1 is not blocked on G6.**

**But G6 leaves one consequence that is load-bearing for the CLI design — `seq` is NOT a stable identity.** `next_seq` = `max(surviving dirs) + 1`, so **a pruned seq is recycled**: seq 24 names three different snapshots in prod's ledger. `SnapshotMeta.seq`'s doc says *"unique within the store"*, which is true only instantaneously. **Every seq-addressed surface must therefore change** — see D3.2/D3.3. (The store *sort* is unaffected: a new snapshot always takes `max+1`, so within-store seq order still tracks creation order.)

### G7 — No snapshot is taken at the moments that most warrant one

There is no snapshot trigger on clean shutdown, on boot-after-unclean-shutdown, before a restore, or before a schema migration. The clean-shutdown path takes a *checkpoint* (`checkpoint_on_clean_shutdown`) — which makes the live file crash-safe but produces **no rollback point**. The pre-restore case is the sharpest: restoring is the single most destructive operation the system offers, and it does not first preserve what it is about to overwrite.

### G8 — **A failed snapshot is destroyed by the cycle that created it** (found by the adversarial review)

`run_cycle` (`snapshot.rs:483`) calls `take_and_emit` then, in the same cycle, `retention_and_emit`. A snapshot that fails `validate_export` is written to disk and marked `valid=false` — and `plan_retention` prunes it **milliseconds later**, because every keep rule considers only `valid` snapshots (`retention.rs:57` — *"Invalid snapshots (failed validation) have no retention value and are pruned"*).

**Prod has already lost real forensic evidence to this, twice.** On 2026-07-08 a snapshot caught a live audit-chain defect — `"out of order: expected seq=7995, found seq=7994"` — and the system deleted the only artefact of it in the same cycle. It happened again on 2026-07-09. All that survives is the error string in the audit payload, and because the seq was recycled, even the identifier is ambiguous.

An invalid snapshot is the single highest-value forensic artefact the subsystem produces: **a complete logical export taken at the instant a defect was detected.** G5 governs `.CORRUPT-` evidence carefully; this hole is the mirror image of it, inside the snapshot store, running automatically on a timer with no operator involved. Closed by D2 below.

---

## Decision

Five changes, all additive, all app-layer, no SQL-specific mechanism (`[[no-sql-specific]]`: no triggers, no engine-side jobs — the cadence, the policy, and the guards are Rust).

### D1 — Make the cadence real: an out-of-process floor first, then grid + catch-up

Ordered by **RPO value**, which is the opposite of the order of cost — and the opposite of this ADR's first draft:

1. **D1.3 (Phase 1) — an out-of-process floor, so the RPO does not depend on serve being up at all.** `aberp snapshot now` is already a complete, fork-safe, separate-process entrypoint (`SnapshotAudit::Reopen`). Schedule it — on macOS a `launchd` `StartCalendarInterval` agent. **This is the only change that creates rollback points *inside* a downtime gap.** In the 6 d 6 h gap containing the 08-22 incident a daily floor would have produced ~6 rollback points, at least four of them predating the incident. **Conservative default chosen: once daily at 03:00 local**, purely as a floor under the 4-hour in-process cadence, not a replacement for it.

   Two things the floor must decide explicitly, because silence in either direction is wrong:
   - **`ABERP_SNAPSHOT_DISABLE` (`snapshot.rs:54`)** turns the in-process daemon off. **Decision: the floor HONOURS it** — "disabled" must mean disabled, and a backup daemon that ignores its own kill switch is worse than one that can be switched off. It must log LOUD at every scheduled invocation when it no-ops for this reason, so a disable set for an unrelated reason cannot silently remove the floor.
   - **The keychain / binary-identity constraint.** An unattended scheduled `aberp snapshot now` on Defense may hit the ad-hoc-signing ACL prompt that blocks unattended boots after a rebuild, in which case the floor silently never runs. **Verify before relying on it** — and make the failure loud (a floor that no-ops silently is indistinguishable from one that never existed, which is exactly the condition G1 measures).

2. **D1.2 (Phase 1) — catch-up on start and on every tick.** Before sleeping, compare `now` against the newest snapshot's `created_at` in the store; if the store is staler than `interval`, take one immediately. **This is a *freshness-at-restart* improvement, not an RPO improvement** — and the first draft of this ADR had that backwards. Trace it through the incident gap: D1.2 takes a snapshot at `restart + 60 s`, which in the 08-17 → 08-23 gap lands a rollback point on **2026-08-23**, *after* the 08-22 incident. A post-incident rollback point cannot roll back the incident. D1.2 creates **zero** rollback points inside a gap; its whole benefit is bounding staleness to ≤ 4 h once serve is back. Worth having, cheap, correctly Phase 1 — but it is not the RPO fix and must not be sold as one.

3. **D1.1 (Phase 1) — anchor the loop to a wall-clock grid.** Sleep to the next `interval` boundary rather than `interval` *after* the cycle. **Measured drift is ≈ 0.27 s per tick**, so this is cosmetic; it is in Phase 1 only because it is nearly free.

D1.2 and D1.3 are idempotent by construction: whichever runs first satisfies the window and the other no-ops.

**Explicitly not chosen: write-count-based triggering.** It is the more precise signal — RPO measured in transactions rather than hours — but it requires a counter on the shared `Handle`'s write path, which is the most safety-critical, most recently-hardened code in the tree (ADR-0110/0111). Adding a side-effecting hook there to serve a *backup* cadence is a poor risk trade while the time-based gaps are this large. Revisit once D1 lands and the measured RPO is hours rather than days.

The daemon keeps its current correctness properties unchanged: `spawn_blocking`, audit through the shared `Handle`, logged-but-survives on every error, `EXPORT` non-blocking to serve (a logical read, not a writer-lock holder).

**Routing check (re-verified by the review): no second writer is introduced.** D1.1/D1.2 are edits inside `run_supervised`, which already appends via `SnapshotAudit::Handle` (`snapshot.rs:288`) through the one shared `aberp_db::Handle`. D1.3 is a separate process using `SnapshotAudit::Reopen`, where no `Handle` exists and a fork is impossible by construction.

**On the second opener — open decision #5 is now CLOSED: do not migrate it.** `take_snapshot` deliberately opens its own short-lived `Connection` for the `EXPORT` (`take.rs:195`), because the alternative holds the single writer mutex for the entire multi-second export every cycle. That reasoning holds. The review traced every branch of `ensure_consistent_with_db` (`crates/audit-ledger/src/mirror.rs:786`) and established that **it never inserts into `audit_ledger`**: it extends the *mirror file* from the DB, rebuilds an absent mirror, or — when the mirror is ahead — preserves and **refuses**. It never tops the DB up from the mirror. Combined with `PRAGMA disable_checkpoint_on_shutdown` at `take.rs:208`, the connection cannot fold the WAL in place on drop either. **The seq-515 fork shape is genuinely unreachable here, and no follow-up ADR is owed.**

**But the accurate framing is sharper than "a mirror reconcile," and the ADR now states it as such:** the same call can **trim the live audit mirror in place** (the torn-tail branch preserves the original and truncates the file) and **mint evidence artefacts** (`preserve_ahead_mirror` → the `.ahead-<nanos>.bak` / `AHEAD-*` files found on disk). So the snapshot daemon performs **audit-mirror recovery surgery on a 4-hour timer**, inside `serve`, outside the boot recovery path that is supposed to own it, on a best-effort/log-and-continue basis. That is not a write to the ledger and it is not the fork shape — but it is not "read-only" either, and the comment at `take.rs` that says so should be corrected to match. **This has consequences for D3.4 (see F5's resolution) and for D5 (who owns the mirror at boot).**

### D2 — Three retention domains: snapshots (keep the math), failed snapshots (new), evidence (new, redesigned)

**Snapshots.** `plan_retention` is correct, pure, well-tested, and its floors are right (newest-valid is sacred; never drop to zero). **Keep it as-is.** G6 confirms it with positive evidence.

**Failed snapshots (new — closes G8).** `plan_retention` gains one rule: **keep the N most recent `valid=false` snapshots** (N = 3, or all within 30 days, whichever is the larger set), and **never prune a `validation_failed` snapshot that is the only artefact of its defect**. The pure function already has the `valid` flag in hand, so this is cheap. Failed snapshots kept under this rule are **also protected evidence** for the purposes of the guard below, and `aberp snapshot list` must show them distinctly — a rollback store whose newest entries are all invalid is an incident, not an inventory. *(Rationale: the system has already destroyed the only byte-for-byte export of a forked chain twice, on a timer, with no operator involved. An invalid snapshot has no **restore** value; it has the highest **forensic** value of anything the subsystem produces. Those are different questions and the current code answers only the first.)*

**Evidence (new — redesigned; the first draft's guard would not have held).** Four changes from that draft, all of them because the guard as written protected the wrong files in the wrong building:

1. **Match case-insensitively.** Non-negotiable: 58 of 101 real artefacts escape a case-sensitive match, and this exact bug class was closed once already in this repo's edition DB-guard.
2. **Invert the rule — protect by allow-list, not deny-list.** Under a tenant home, **anything that is not a known-live filename is protected evidence.** The live set is enumerable and stable (`aberp.duckdb`, `aberp.duckdb.wal`, `aberp.duckdb.audit.log`, `aberp.duckdb.ckpt-ok`, `seller.toml`, `logo.png`, …); the evidence set is neither, as those 58 misses demonstrate. The named families — `*CORRUPT*`, `*RECOVERY*`, `*DEFORK*`, `*PRE-*`, `*.ahead-*`, `*healed-*`, `*INDEXDESYNC*`, `*DEDUP*`, `*SPURIOUS*`, `*_evidence*`, `*EVIDENCE*`, `*.bak` — remain as a **belt-and-braces second predicate**, not as the primary one. **A `*CORRUPT*` / `*RECOVERY*` / `*DEFORK*` / `*PRE-*` / `healed-*` / `INDEXDESYNC*` artefact is NEVER pruned, in any case, by any caller.**
3. **Put the guard where the risk is.** Ship a shared **`is_protected_evidence(path) -> bool`** in `aberp-snapshot`. `retention::prune` calls it — but `prune` only ever touches the snapshot store, where no evidence lives, so **a refusal inside `prune` alone protects nothing**. Every tenant-home helper must call it too, and a **cut-gate check** must fail any `remove_file` / `remove_dir_all` under a tenant home that does not go through it. *(A guard that refuses must log LOUD, per house rule #12 — silent protection is how a stale plan becomes invisible.)*
4. **Name the real locations.** The policy's scope is `~/.aberp*/<tenant>/` **plus `~/aberp-snapshots/` (214 MB, including the `*-keychain.zip` encrypted credential dumps — and note it is *not* `~/Documents/ABERP-snapshots/`) plus `~/Documents/ABERP-recovery-*/` (57 MB)**. Governing one third of the footprint while claiming to govern all of it is worse than governing none, because it reads as done. The keychain dumps are additionally **never archived to a less-protected location** — for them, release means delete-in-place or nothing.

The tiered policy itself, applied by an explicit operator command and **never** by the periodic daemon (evidence deletion is not a background activity):

- **Never auto-delete** anything younger than **90 days**, or belonging to the **3 most recent distinct incidents**, whichever is the larger set.
- **Never delete the only artefact of an incident.** **Incident grouping is `(mtime window, tenant)` with the filename tag as a secondary key** — *not* the tag alone. Two mutually incompatible tag formats coexist on disk: ISO (`aberp.duckdb.CORRUPT-20260705T184449Z` and its `.wal` sibling group correctly) and nanosecond-epoch (`aberp.duckdb.audit.log.corrupt-1783315209649645000.bak` shares a tag string with *nothing*, including the ISO-tagged `.CORRUPT-` file from the same incident). Under tag-only keying every nanosecond-tagged artefact is a singleton incident, permanently protected by this very rule — safe, but the policy would never release the 22 `corrupt-*.bak` + 9 `healed-*.bak` that are most of the growth, i.e. **technically correct and operationally inert.** Normalise nanosecond tags to ISO at archive time. **Safe default, stated explicitly: ungroupable ⇒ protected.**
- **Release is archive-then-remove**: evidence leaving the live tenant home is first written to `~/Documents/ABERP-evidence/<tenant>/<incident-tag>/` (compressed) and only then unlinked, so "pruned" never means "gone".

**Dropped from the first draft:** *"never delete anything referenced by an unresolved `db.auto_recovered` audit event."* It was the strongest of the floors and the one most likely to be quietly dropped in implementation, **because it needs a *resolution* concept that does not exist in the tree.** Rather than lean on a rule nothing can evaluate, it is removed. If a resolution marker is later defined, reinstate it as a fifth floor. Three independent floors plus `release ≠ delete` remain.

**Conservative default:** nothing is deleted without an explicit command, and with a 90-day floor today's evidence reduces to roughly the 2026-06/07 cohort being *archivable* and the whole August set retained.

### D3 — Make restore crash-safe, previewable, and routine

**D3.1 — Delete the target's stale WAL *before* the rename, then install via `atomic_install`.**

The first draft said `restore_into` should route through `crash_safe::atomic_install`, "inheriting fsync-file → rename → fsync-dir **and the `install-intent` journal**." The first three are true. **The journal claim was false and is withdrawn:** `write_install_intent` has exactly one non-test caller — `crash_safe.rs:604`, inside `durable_checkpoint` — and `atomic_install` never writes it. Nor would it help here: `resume_pending_install` is called from exactly one place (`serve.rs:1372`), keyed on the **live** `args.db`, so nothing would ever resume an intent left beside a side-path restore target.

That matters because a journal-free `atomic_install` leaves the last window open:

```
atomic_install:  fsync(staged)
                 rename(staged → target)     ← new file is now visible
                 remove target's stale WAL   ← crash HERE leaves new file + OLD WAL
                 fsync(parent dir)
```

`restore_into`'s own comment says a surviving old WAL *"would corrupt it on next open"*. `durable_checkpoint` survives that window via its journal and `resume_pending_install` → `ClearedStaleWal`; `atomic_install` alone does not.

**The correct fix is simpler than the one proposed, not more complex.** A restore is destroying the target *by definition*, so there is no reason to preserve its WAL past the point of no return — unlike `durable_checkpoint`, where the target is the live DB and must survive an aborted swap. **Delete `<target>.wal` first, then call `atomic_install`.** Step 4 becomes a no-op and the window disappears with no journal and no resume path.

An implementer who reads "route through `atomic_install`" and stops will leave that window open **and believe this ADR told them it was closed.** It did not. This closes G2 outright and is a small change to one function.

**D3.2 — `--dry-run`.** Prints what would happen and writes nothing: the chosen snapshot, its age and byte size, its validation verdict re-run live, its `invoice_count` / `audit_count` / `chain_len` / anchor coverage, the resolved target path, whether that target exists, and — the part that matters — **the delta against the live DB**: rows and invoices that exist now and would not exist after. Exit code distinguishes *would proceed* from *would refuse*.

**D3.3 — Verify-before-restore, refuse on ambiguity, and address snapshots by a STABLE id.**

Before writing, and beyond today's `validate_export`: re-verify the chain end-to-end, verify anchor coverage (D4), and confirm the snapshot's genesis matches the target tenant's. **Refuse — never guess —** when the selector matches more than one snapshot, when the snapshot's tenant differs from `--tenant`, when the snapshot is `valid=false`, or when the live DB is *ahead* of the snapshot in a way the operator has not acknowledged. This mirrors `recover_or_refuse`'s posture, already proven in the boot path.

**Critical, from G6: the selector must NOT be a bare `seq`.** `seq` is recycled after a prune (`next_seq` = `max(surviving) + 1`); seq 24 names three different snapshots in prod's ledger, two of which are the `validation_failed` pair. A `seq`-addressed restore CLI is therefore ambiguous by construction, and the ambiguity is worst exactly where it hurts most — around failed and pruned snapshots. **Every snapshot-addressing surface — the `<selector>`, `--dry-run`'s report, the audit payloads, and `list` output — uses `(seq, created_at, source_db_sha256)` as the identity, and refuses a selector that resolves to more than one.** `SnapshotMeta.seq`'s doc comment (*"unique within the store"*) must be corrected to *"unique instantaneously; recycled after a prune"*.

**D3.4 — A guarded in-place restore, replacing the hand-swap.** `aberp restore --in-place` performs the sequence the operator performs today, in code and journalled:

1. Refuse unless serve is stopped (fail on a live lock, do not race it).
2. **Snapshot the current DB first** — closing G7's sharpest case: the thing about to be overwritten is preserved before it is overwritten. *(See the F5 resolution below for what this snapshot is allowed to touch.)*
3. **Move the current DB aside as `.PRE-RESTORE-<tag>` evidence — as ONE UNIT: `aberp.duckdb` + `aberp.duckdb.wal` + `aberp.duckdb.ckpt-ok`.** The first draft said only "move the current DB aside", which would have been a real defect: a DB moved without its WAL is **stripped of its un-checkpointed commits** — not a recoverable original, so **AC-5 would have been unsatisfiable** — *and* the orphaned `aberp.duckdb.wal` would stay at the live path and pair with the freshly restored file, which is **the exact corruption vector `restore_into`'s own comment warns about**, reintroduced by the command written to eliminate the hand-swap. Prod's live DB carries a WAL right now; this is not hypothetical.
4. **The `.audit.log` mirror does NOT move.** It is the durable record and stays at the live path. Stated explicitly because an implementer moving "the tenant's DB artefacts" would naturally take it too.
5. `restore_into` via D3.1 (WAL deleted first, then `atomic_install`).
6. **Write a fresh `.ckpt-ok` marker for the installed file** (or delete the stale one). `restore_into` today installs a new file beside a marker describing the **old** one; `checkpoint_is_current` then returns false on the SHA mismatch, so the next debounced checkpoint runs — benign, but the restored file's provenance record would otherwise be a lie.
7. Re-verify the installed file, then emit `snapshot.restored` (see D3.5).

Any failure leaves the `.PRE-RESTORE-` unit and the original path recoverable.

**D3.5 — Do not poison the audit index; and say which ledger, per command.**

The 2026-08-03 heal-path lesson: **never pre-seed a seq, never write into a restored file out-of-band.** The restored DB's chain is whatever the snapshot certified; the restore event is the next entry on it, not an edit to it.

**Which ledger (F8) — this differs per command, and a single global rule would regress shipped behaviour:**
- **`aberp snapshot restore` (side path):** the row lands in the **live** DB's ledger, *not* the freshly-restored side-DB. This is what the code already does deliberately (`snapshot.rs:549-551` — *"so the operator's main timeline shows that a restore happened"*), and prod's two restore events confirm it (prod ledger seqs 8153 and 8408, both targets side paths). **Correct; do not change it.** The first draft, read as a global instruction, would have regressed this.
- **`aberp restore --in-place`:** the live DB *is* the restored DB, so the two collapse and the row is the next seq on the restored chain.

**The restore row must be durably acked.** `emit_reopen_cli` does `Ledger::open` → `append` → return; `Ledger::open` (`storage/mod.rs:148`) sets `PRAGMA disable_checkpoint_on_shutdown` and nothing else, and `append` commits a transaction **without `durable_ack` and without syncing the mirror**. So post-D3.1 the restored *file* is durable while the row recording the restore is not — a power cut moments later leaves a **silently-restored database**. On the D-22 precedent, the restore event gets `durable_ack`.

**F5 resolution — the mirror.** D3.4's mandatory pre-restore snapshot calls `take_snapshot`, which unconditionally runs `ensure_consistent_with_db` on the live DB (`take.rs:221`) — and that call can **trim the live mirror in place** and mint a `.bak`. The first draft's D3.5 forbade exactly that pre-resolution, so the text told the implementer to do both. **Resolved in favour of keeping the pre-restore snapshot and narrowing D3.5:**

> **D3.5's rule is scoped to *after* the install: the restore must not reconcile the mirror once the new file is in place.** If the mirror disagrees with the restored DB, that is `recover_or_refuse`'s decision at next boot, and the restore leaves it that way.
>
> The pre-restore snapshot at step 2 runs **before** anything is overwritten, on the DB that is about to be replaced, and its reconcile is the same one the 4-hourly daemon has been performing all along (D1's framing) — so it introduces no new behaviour, only a new occasion. It is nonetheless **logged as a distinct pre-restore reconcile** so that a mirror trim in that window is attributable, and any outcome other than `Clean`/`Extended` **aborts the restore** rather than proceeding: a mirror that is ahead or deeply corrupt at the moment of an in-place restore is a refuse-and-escalate condition, not a step to log past.

*(The alternative — a `take_snapshot` variant with reconciliation suppressed — was considered and rejected for Phase 2: it would lose the Gap-2b `audit_count <= mirror_head` guarantee at exactly the moment the operator most needs it, and it adds a second code path through the most incident-prone function in the crate.)*

### D4 — Anchors participate in validation and travel with the snapshot — recorded, never gating

Extend `ValidationReport` and `SnapshotMeta` with anchor coverage. `validate_export` reads `audit_ledger_anchors` from the re-imported in-memory DB and verifies each anchor against the chain head it claims to cover. **The verification goes through `audit-ledger`'s existing API, not raw SQL in `aberp-snapshot`** — the table is table-shaped, not engine-shaped, and keeping the query behind the crate that owns the schema is what keeps `[[no-sql-specific]]` / engine-swappability true here.

**Conservative default: a missing or short anchor set does NOT fail validation** — it is recorded and surfaced. The data makes this stronger than a preference. Every `audit_ledger_anchors.parquet` in both stores is exactly 300 bytes, consistent with **zero anchor rows everywhere**. A hard gate would therefore mark **every existing snapshot invalid** — and `plan_retention` prunes invalid snapshots, so a hard gate would not merely fail validation, **it would delete the entire rollback store on the next cycle**, save the last-resort floor. That is a durability regression in service of a legal property. *(G8's new "keep the N most recent invalid" rule would blunt but not prevent it.)*

**Open decision #2 is now ANSWERED — record-don't-fail on Defense too, with the sanction moved to restore time.** Defense's premise is court-admissibility, so the instinct to hard-gate there is right in spirit and wrong in placement: a validation gate punishes the *snapshot*, destroying rollback points. Put the sanction where the legal claim is actually made — **`aberp restore --in-place` on Defense REFUSES without `--accept-unanchored` when `anchored_through_seq < chain_len`.** Same protection, no loss of rollback capability. Restore's `--dry-run` and pre-flight report anchor coverage prominently either way, so the operator always sees what a restored DB will and will not be able to prove.

**F7 — the "not recorded" sentinel, not zero.** The fields are:

```rust
/// Count of anchor rows in the snapshot. `-1` for snapshots taken before
/// this field existed, or when the table was unreadable: "not recorded",
/// NEVER "zero anchors".
#[serde(default = "anchor_count_unrecorded")]  // -> -1
pub anchor_count: i64,
/// Highest `audit_ledger` seq covered by a VERIFIED anchor.
/// `None` == not recorded (a `u64` cannot carry the sentinel at all).
#[serde(default)]
pub anchored_through_seq: Option<u64>,
```

The first draft used bare `#[serde(default)]`, which for `i64`/`u64` yields **`0`** — so every existing snapshot would read back "0 anchors", indistinguishable from one *verified* to carry none. For a field whose only purpose is telling a restoring operator what a database can prove in court, defaulting to the worst-case-looking value while meaning "unknown" is exactly backwards. **The precedent is already in the tree:** the frozen prod `crates/aberp-snapshot/src/lib.rs:151-162` carries `secondary_index_count` with a `-1` sentinel doc'd as *"not recorded, never zero indexes"*, added after the 2026-08-03 prod incident. Follow it exactly.

**Add a `meta_version` field.** Prod's live `meta.json` files already carry `secondary_index_count`, a field **Editions' `SnapshotMeta` does not have** — the format has already drifted between the two lines with no version marker. Cross-parsing works today only because `serde` ignores unknown fields and because the stores are disjoint (ADR-0093). D4 is the moment to fix that.

`meta.json` is `serde`-parsed per directory and older files must keep parsing, so all new fields are defaulted. The ADR-0087 fact-1 discipline applies by analogy: **nothing in this change may enter the `entry_hash` preimage.**

### D5 — Snapshot at the moments that warrant one

Add triggers, all reusing the existing `run_cycle`: **before any restore** (D3.4, non-optional), **on clean shutdown** (alongside the existing checkpoint, skipped if a snapshot is already within `interval`), and **on boot after an unclean shutdown or a successful auto-recovery** (so the recovered state is itself a rollback point). Each is subject to the same staleness check as D1.2, so none of them can produce a snapshot storm.

**Who owns the mirror at boot — state it or they race.** Every one of these triggers multiplies how often `take_snapshot`'s `ensure_consistent_with_db` runs (D1's "mirror recovery surgery on a timer"), and the boot trigger runs it **at boot, where `recover_or_refuse_with_audit` (`recover.rs:189`) is the designated owner of exactly that decision.** **Decision: `recover_or_refuse` owns the mirror at boot, unconditionally.** The boot-after-unclean-shutdown snapshot trigger fires **only after** `recover_or_refuse` has completed and returned a non-`Refuse` outcome, and never before it. A snapshot must never be the thing that first touches a mirror at boot.

---

## CLI surface

Existing commands keep their shape and their flags; additions are marked **new**. Note that `<selector>` resolves against `(seq, created_at, source_db_sha256)` and refuses on ambiguity (D3.3, G6) — a bare `seq` is not a stable identity.

```
aberp snapshot now      [--db P] [--tenant T] [--store P]
                        [--if-stale <dur>]                        # new: no-op if a snapshot is newer than <dur>

aberp snapshot list     [--tenant T] [--store P]
                        [--json]                                  # new: machine-readable, for the launchd floor
                        [--verify]                                # new: re-run validation live, don't trust meta.json
                                                                  #      shows invalid/retained-forensic snapshots distinctly

aberp snapshot restore  <selector> --to P --confirm [--tenant T] [--db P] [--store P]
                        [--dry-run]                               # new: D3.2, writes nothing
                        [--verify-only]                           # new: run the pre-flight, report, exit

aberp restore --in-place --tenant T --snapshot <selector> --confirm    # new: D3.4, replaces the hand-swap
                        [--accept-unanchored]                     # new: D4, required on Defense when anchors are short

aberp snapshot prune    [--tenant T] [--store P] [--dry-run]       # new: retention on demand, not only via the daemon

aberp evidence list     [--tenant T]                               # new: D2, the ~600 MB nobody can currently see
aberp evidence archive  --older-than <dur> [--tenant T] [--dry-run] [--confirm]   # new: archive-then-remove
```

`--dry-run` and `--confirm` are mutually exclusive everywhere they both appear. Every mutating command emits its audit event through the routing that already exists (`SnapshotAudit::{Handle,Reopen}`), durably acked per D3.5; `aberp evidence archive` needs a new `EventKind` (**+1 to the kind count — reconcile against the 195 in the QC-report work before merge**).

---

## Phasing

**Phase 1 — works today, no hardware, no endpoint, no schema change.**
**D3.1** (delete target WAL → `atomic_install` in `restore_into`), **D1.3** (the out-of-process floor), D1.1 + D1.2 (grid + catch-up), D2's `is_protected_evidence` + the cut-gate check + the failed-snapshot retention rule (G8), `--dry-run` / `--verify-only` / `--json` / `--if-stale`, and the D3.3 stable-id selector.

**D1.3 is in Phase 1, and the first draft's reason for deferring it was backwards.** It was placed in Phase 4 on the rationale that *"D1.2's catch-up already recovers most of its value"* — it does not: D1.2 creates **zero** rollback points inside a downtime gap, and D1.3 is the only proposed change that creates any. D1.3 remains a host-level artefact (`launchd`) rather than a code change, and the keychain/binary-identity constraint on Defense is a real risk — **but that is the honest reason to sequence it carefully, not a reason to call it low-value.** If it must slip, it slips on the keychain constraint and on nothing else.

**D3.1 should not wait for the rest** — it is a few lines against a primitive that already exists, and it is the difference between a restore that survives a power cut and one that does not.

**Phase 2 — still no hardware.** D3.4 (`--in-place`, including the WAL/`.ckpt-ok` unit move and the F5 pre-restore reconcile bounding), D3.5's durable-ack and per-command ledger routing, D5 (the triggers, with `recover_or_refuse` owning the boot mirror), D2's evidence commands and archive store, `aberp snapshot prune`.

**Phase 3 — depends on the anchoring rollout.** D4. The mechanism is live in `audit-ledger`, but the coverage question ("is a TSA reachable, and how much of the historical chain is anchored") is operational. D4's *recording* half (fields, sentinel, `meta_version`) can land in Phase 2; the Defense restore-time refusal waits on real anchor coverage existing.

Nothing here depends on a machine, an endpoint, or a NAV/TSA connection except D4's Defense refusal.

**One operational addition:** `snapshot list --verify` should be part of the daily floor's job, not only an operator command. `restore_into` re-runs `validate_export` and refuses with `RestoreFromInvalid` — correct, but it means a store whose snapshots have bit-rotted is **unrestorable with no warning until the incident**. D3.2 and `list --verify` fix the *visibility* half; running it on the floor's schedule fixes the *proactive* half.

---

## Open decisions — flagged for Ervin

Three of the original six are now closed by the adversarial review and are recorded above rather than left open: **#1 (G6 — retention math trustworthy, manual deletion proven)**, **#2 (D4 — record-don't-fail everywhere, sanction at restore)**, **#5 (the export connection — do not migrate; the seq-515 shape is unreachable)**. What remains genuinely open:

1. **Is the daily 03:00 out-of-process floor the right cadence?** Chosen conservatively. Given a measured max gap of 8 d 20 h, even weekly would have helped; hourly would make serve-uptime nearly irrelevant but multiplies store growth (~1.8 MB/snapshot today, so hourly ≈ 43 MB/day before retention). **Recommendation: start daily, measure the resulting RPO for one month, then decide** — this is reversible in a plist.
2. **90-day evidence floor and the archive location.** `~/Documents/ABERP-evidence/` mirrors the snapshot store's "outside the repo, outside `~/.aberp/`" property. Four incidents in ~10 weeks and the oldest artefacts dating to 2026-06-27 suggest 90 days is comfortably long enough. **Separate call needed on the `*-keychain.zip` dumps in `~/aberp-snapshots/`** — encrypted NAV credentials and an SMTP password should arguably never be archived to a second location at all, only deleted in place. D2 assumes that stricter reading; confirm.
3. **Does the Defense `launchd` floor actually run unattended?** The ad-hoc-signing ACL prompt that blocks unattended boots after a rebuild may block it silently. This is a *verification task*, not a design question, but it gates whether D1.3 delivers anything on Defense.
4. **The ADR number.** `0116` is provisional — reconcile against the auto-probe and portal branches' provisional numbers at merge, along with the `EventKind` count delta (+1, against the 195 in the QC-report work).

---

## Consequences

**Positive.** Recovery stops being per-incident hand-surgery: `--in-place` journals the swap that is currently manual, `--dry-run` lets an operator see a restore before committing to it, and the restore install inherits the same durability guarantee as every other file install in the tree. The RPO improves from *days* (measured: 18.5 % of cadence, max gap 8 d 20 h) toward *hours* — and, because D1.3 is now first, it improves **inside** downtime gaps rather than only at restart. Recovery evidence becomes visible and governed without becoming deletable, under a guard that actually matches the filenames on disk. The subsystem stops destroying its own forensic evidence on a timer.

**Negative / accepted.** More CLI surface to keep coherent. Snapshot storage grows if the floor cadence is raised, and the G8 rule retains some invalid snapshots that today are deleted immediately. D4 adds fields to `meta.json` — additive and defaulted, but a format change nonetheless (and one that *acknowledges* an existing undocumented drift rather than creating it). The write-count trigger is deferred, so RPO stays time-bounded rather than transaction-bounded; a burst of invoices immediately before a failure can still fall inside one interval. The evidence allow-list must be maintained: a new legitimate live filename that nobody adds to it will be treated as protected evidence — which is the safe direction, but it will accumulate.

**Not addressed here.** The Portable port of ADR-0111 and the HTTP restore route remain owed from PR #41 and are out of scope. The `take.rs` "read-only" comment should be corrected to match what the connection actually does (D1), but the connection itself is deliberately not migrated.

---

## House rules — explicit verdicts

| Rule | Verdict |
|---|---|
| **Shared `Handle` only / no second writer** | ✅ **Holds.** D1.1/D1.2/D5 stay inside `run_supervised` (`SnapshotAudit::Handle`). D1.3/D3.4 are separate processes (`SnapshotAudit::Reopen`), where no `Handle` exists and a fork is impossible by construction. The `take.rs:195` export connection writes *files*, never `audit_ledger` rows — every branch of `ensure_consistent_with_db` traced (D1). |
| **No SQL-specific mechanism** | ✅ **Holds.** Cadence, retention, guards, and evidence policy are all Rust. `EXPORT`/`IMPORT`/`CHECKPOINT` are pre-existing. No triggers, no engine-side jobs. |
| **Engine-swappable** | ✅ **Holds, with D4's constraint made explicit:** D4's anchor verification goes through `audit-ledger`'s existing API, not raw SQL in `aberp-snapshot`. The anchor table is table-shaped, not engine-shaped. |
| **Recovery-evidence protection** | ✅ **Now satisfied.** The first draft **failed** this (58 of 101 artefacts escaped; 14 escaped even case-insensitively; the guard sat on `prune`, which never touches a tenant home). D2 now matches case-insensitively, inverts to a live-file allow-list, ships a shared `is_protected_evidence` that tenant-home helpers must call, adds a cut-gate check, names the real locations including the ~271 MB outside `~/.aberp*`, and never prunes a `*CORRUPT*` / `*RECOVERY*` / `*DEFORK*` / `*PRE-*` / `healed-*` / `INDEXDESYNC*` artefact. G8 extends the same protection to failed snapshots. |
| **`[[trust-code-not-operator]]`** | ✅ The guard satisfies it; the hand-swap procedure violates it; D3.4 is the closure. |
| **`[[fail-loud]]` / house rule #12** | ✅ A pruner that refuses logs LOUD; the D1.3 floor logs LOUD when it no-ops under `ABERP_SNAPSHOT_DISABLE`; a pre-restore mirror outcome other than `Clean`/`Extended` aborts rather than logging past. |

---

## Acceptance criteria

1. **(F2 — replaces the vacuous original.)** The restore install path is durable, asserted by **mutation**, not by process-kill: **deleting `fsync_file` or `fsync_dir` from the restore install path turns a named test red**, and a code-level assertion pins that the install goes through `crash_safe::atomic_install`. *The original AC — "kill the process between import and rename, assert the target is old-or-new, never torn" — is **vacuous**: `rename(2)` is atomic in the page cache, so `kill -9` / `panic!` / `abort()` cannot lose it. That assertion passes today with **zero** fsyncs and would pass again after the fix, testing rename atomicity (never in doubt) instead of the fsyncs (the entire point). This is the same vacuity trap that already burned this project twice — the debounce-shadow power-loss test and the `durable_ack` deletion no test could see. If genuine crash injection needs a filesystem-level harness this project does not have, say so in the test's doc comment; do not substitute a test a reviewer will tick while the defect is still present.*
2. **(D3.1, the window F1 found.)** A test asserts the target's `.wal` is unlinked **before** the rename: with a stale `<target>.wal` present and the process failed immediately after the rename, the installed file has **no** WAL beside it.
3. With the daemon started against a store whose newest snapshot is older than `interval`, a snapshot is taken within `BOOT_DELAY_SECS` — not at the next interval boundary.
4. **(D1.3.)** With `aberp serve` never started, the scheduled floor produces a snapshot; and with `ABERP_SNAPSHOT_DISABLE=1` set it produces none **and logs LOUD**.
5. `aberp snapshot restore --dry-run` writes nothing (asserted by mtime + directory listing over the target and the store) and exits non-zero when the pre-flight would refuse.
6. **(D2/F3.)** `is_protected_evidence` returns true for **every one of the 101 evidence-shaped names observed on this machine**, fixtured verbatim — including all 9 `healed-*.bak`, `INDEXDESYNC-BACKUP-20260803` + its `.wal`, the 22 lowercase `corrupt-<nanos>.bak`, `_evidence-20260627/`, and `SPURIOUS-POST-*` — and false for the live set (`aberp.duckdb`, `.wal`, `.audit.log`, `.ckpt-ok`, `seller.toml`, `logo.png`). Any caller that removes a path under a tenant home without consulting it fails the cut-gate check.
7. **(G8.)** A cycle whose snapshot fails `validate_export` **leaves that snapshot on disk**; a test drives `run_cycle` to a validation failure and asserts the directory still exists after `retention_and_emit`, and that `list` reports it distinctly.
8. **(D3.3/G6.)** A store containing two snapshots that have held the same `seq` (constructible: create, prune, create) causes a bare-`seq` selector to **refuse as ambiguous**, and a `(seq, created_at, sha256)` selector to resolve exactly one.
9. **(D3.4/F4.)** `aberp restore --in-place` leaves a `.PRE-RESTORE-<tag>` artefact **including the `.wal` and `.ckpt-ok`** and a recoverable original on every injected failure path; the live-path `.audit.log` mirror is **unmoved**; and no orphan `.wal` remains beside the restored file. The recoverability assertion opens the preserved unit and reads back a row that was in the WAL and not in the DB file.
10. **(D3.5/F8.)** `aberp snapshot restore` to a side path writes `snapshot.restored` to the **live** ledger (unchanged behaviour, pinned by test so a future refactor cannot silently move it); `aberp restore --in-place` writes it as the next seq on the restored chain, asserted by `verify_chain` over the restored file with no pre-seeded seq and no mirror write **after** the install. Both are `durable_ack`ed.
11. **(D4/F7.)** `validate_export` reports `anchor_count` / `anchored_through_seq`; a snapshot with zero anchors still validates; a `meta.json` written before these fields existed reads back `anchor_count == -1` and `anchored_through_seq == None` — **never `0`/`Some(0)`** — asserted against a fixture of a real pre-D4 `meta.json`.
12. **(D5.)** At boot, `recover_or_refuse_with_audit` completes before any snapshot trigger fires; a test asserts no `take_snapshot` call precedes it.
