# ADR-0111 (Defense) — Every durable checkpoint runs under the shared Handle's writer lock

- **Status:** **Proposed — implemented; adversarial review OWED (not yet run). Not merged.**
- **Date:** 2026-08-11
- **Deciders:** Ervin Áben (scope: fix the mirror-ahead-of-DB durability defect behind the recurring audit-chain fork incidents; fresh worktree off `main`; open a PR, do **not** merge — adversarial is owed and will be spawned separately). Investigation + implementation by Dispatch.
- **Base:** Editions `main` @ `07543e0`. Every file:line below was reproduced in this session at that SHA.
- **Related:** **ADR-0095 §3** (wired the crash-safe primitives into the daemon + post-write paths — and is where the two defective call sites were introduced); **ADR-0098 Gap 1a/1b** (the ONE shared `aberp_db::Handle`, and `run_durable_checkpoint_locked`, which has always been correct); **ADR-0098 R2** (the install-intent journal + WAL fence inside the primitive); **ADR-0105** (the single serialization domain this must not disturb); **ADR-0110 D3** (`durable_ack`, whose by-path `fsync` residual this also closes).

> **On the number.** The two trees' ADR sequences forked: Editions owns 0093–0105, Portable owns 0100–0110, and the middle collides. `0111` is free in **both**, and it continues the durability chain the Editions tree actually reads — 0095 → 0098 → 0105 → 0110 → 0111.

---

## 0. TL;DR

`aberp_snapshot::durable_checkpoint` commits by renaming a new inode over the live DB and deleting its WAL. That is sound **only** while the shared connection is quiesced. Two production callers ran it on a **path**, from `spawn_blocking`, holding no lock — so after every daemon tick the shared connection was stranded on an unlinked inode, later commits went to a file the kernel frees at exit, and the lockstep `sync_mirror` **durably mirrored them anyway**.

| | |
|---|---|
| **Symptom** | Mirror ahead of DB → boot refuses (`MirrorAheadOfDb`) or Defense's auto-heal **replays the delta** → audit-chain fork |
| **Blast radius** | Not shutdown-only. A long serve session silently loses **every** row committed after the first daemon tick |
| **Fix** | `Handle::checkpoint_now()` — writer mutex, quiesce, checkpoint, reopen. All three checkpoint sites routed through it |
| **Belt** | An inode fence in `WriteGuard::drop` that catches an **out-of-process** swapper and fails the money-path ack instead of certifying a lost write |
| **Gate** | `tools/cut_gate_checkpoint_sites.sh` — bans the path-based primitive outside its two owner files, holds the call-site census closed both ways, pins the lock/quiesce/reopen |

---

## 1. Context — the primitive, and the one caller that used it correctly

`atomic_install` (`crates/aberp-snapshot/src/crash_safe.rs:221-236`) is the crash-safe commit:

```rust
fsync_file(staged)?;
std::fs::rename(staged, target)...;          // a DIFFERENT inode now sits at the path
let target_wal = wal_sibling(target);
if target_wal.exists() { std::fs::remove_file(&target_wal)...; }   // and the old WAL is UNLINKED
fsync_dir(parent)?;
```

Both steps are correct, and both are lethal to any file descriptor opened before them. The primitive says so itself: the ADR-0098 R2 WAL fence inside `durable_checkpoint` is documented as *"Meaningful under the shared Handle's single-writer lock the runtime callers hold"* (`crash_safe.rs:529`).

Exactly one caller arranges that. `Handle::run_durable_checkpoint_locked` (`crates/aberp-db/src/lib.rs:804-848` at base) drops the shared connection so the checkpoint is the sole opener (`:812`), takes the validated checkpoint (`:818`), and **reopens on the freshly-installed inode** (`:839-847`). Its doc comment names the exact hazard: *"the `atomic_install` rename would **orphan** the shared connection on the old (now-unlinked) inode."*

## 2. Root cause — two callers that did not

ADR-0095 §3 wired the checkpoint into "the paths a crash traverses". Both wirings took a **path**, from a blocking task, with no lock:

| # | Site (at base `07543e0`) | Cadence |
|---|---|---|
| 1 | `apps/aberp/src/snapshot.rs:670` — `live_checkpoint_logged(&db, tenant.as_str())` inside the daemon's `spawn_blocking` | every snapshot tick (default 4 h) |
| 2 | `apps/aberp/src/live_checkpoint.rs:93-95` — `spawn_blocking(move || live_checkpoint_logged(&db, &tenant))` | ~60 s after any regulated write |
| 3 | `apps/aberp/src/serve.rs:3439` — `checkpoint_on_clean_shutdown(db_path, tenant)` | clean shutdown |

Site 3 is less damaging (the process is leaving) but is the same class, and it additionally re-creates the ADR-0098 Gap-1a two-instance hazard: the handle in `recovery_state.db` is **still open** there, so the primitive's own `Connection::open` is a second live opener.

The failure chain, for sites 1 and 2:

1. the rename installs a new inode at `db_path`; the handle's connection keeps its fd on the old one, now unlinked;
2. every subsequent `Handle::write()` commits into that orphan — it *appears* to succeed;
3. `WriteGuard::drop` runs `sync_mirror` **on that same connection**, so it sees those rows and durably appends them to `<db>.audit.log`;
4. the orphan is freed at process exit. The mirror keeps the rows; the DB never had them.

That is `MirrorAheadOfDb` — the direction `preserve_ahead_mirror` refuses at boot and Defense's `attempt_db_auto_recovery(mirror_ahead)` → `replay_mirror_delta` **resurrects** from. It is the recurring audit-chain fork, generated on a 60-second cadence.

`Handle::durable_ack` made it worse rather than better: `fsync_path` opens **by path** (`lib.rs:875`), so post-swap it flushed and journalled the brand-new inode — certifying bytes that had nothing to do with the commit being acked.

## 3. Decision

**P1.** Add `Handle::checkpoint_now()`: take the writer mutex via `lock_recovering` (the same poison-recovery path as `write`/`read`/`checkpoint_on_idle`) and call `run_durable_checkpoint_locked` **unconditionally**. Route all three sites through it. The `aberp_snapshot` primitive is **untouched**.

*Why unconditional.* It ignores both the D2 coalescing window and `HandleConfig::checkpoint_enabled`. Those gate the handle's own **automatic** cadence; this is an explicit caller demand, and each caller owns its own cadence and env kill-switch. Gating it would silently drop the daemon and shutdown checkpoints — ADR-0095 root cause #2, restored — and would make every test in `checkpoint_swap_orphan.rs` (which runs with `checkpoint_enabled = false`) vacuously green. CHECK C-B fails the gate if the gating ever comes back.

*Non-reentrancy.* `std::sync::Mutex` is not reentrant: `checkpoint_now` inside a live `WriteGuard` self-deadlocks. All three sites are outside any guard scope, and the census requires a new site to state where its guards end.

**P2a (done).** An **inode fence**: `Inner.fence` records `(st_dev, st_ino)` at every open (boot, `ensure_open`, the post-checkpoint reopen), and `Handle::live_file_swapped` compares it at all **three** points where a stale identity does damage:

| site | on a mismatch |
|---|---|
| `WriteGuard::drop` | skip **both** the by-path `fsync` and the mirror sync, park `DbError::LiveFileSwapped`, drop the stranded connection |
| `Handle::durable_ack`, unparked (`None`) arm | fail the ack instead of falling through to `fsync_data_paths` |
| `Handle::read` | drop and reopen, so a `try_clone` cannot keep serving the pre-swap file |

In-process this is unreachable after P1 — it is the belt for an out-of-process swapper (a second `aberp` invocation, a backup tool) and for the in-process operator restore below, which P1 does *not* cover.

The `durable_ack` and `read` arms were added after the PR #41 adversarial. The first cut fenced only the guard drop, which left two holes: `durable_ack`'s `None` arm (no parked outcome — a money path whose write was a no-op) fell through to `fsync_data_paths`, which opens **by path**, so after a swap it flushed and journalled the brand-new inode and returned `Ok(())`; and `read` handed out clones of the stranded connection forever, so an operator restore would leave the UI showing pre-restore rows indefinitely.

**P2b (done).** `tools/cut_gate_checkpoint_sites.sh` + two frozen censuses (`tools/adr0111_checkpoint_sites.txt`, `tools/adr0111_rename_family_sites.txt`), wired into both CI workflows with a 21-probe teeth harness.

The first cut of CHECK C-A keyed on one function *name* and the adversarial walked through it three ways, each compiling clean while the gate said PASSED: an aliased import (`use ...live_durable_checkpoint as fold_live`), a different public wrapper over the same rename (`provision_atomic` / `resume_pending_install`), and the `pub` rename primitive itself (`atomic_install`). So C-A now governs the **whole rename-over-a-DB-path family** and counts **touches** (a call, a `use` aliased or not, a bare function-pointer reference) rather than calls of one spelling.

The complication is that three of those symbols are *legitimately* called from the **pre-Handle boot sequence** — `resume_pending_install`, `provision_atomic`, `recover_or_refuse_with_audit`, all before `open_tenant_handle` at `serve.rs:1871`, where there is no shared connection to orphan. Banning the names globally would red the boot path. So the census pins **(file, symbol, count)**: those three are listed with the reason they are sound, and a *new* symbol or *one more* touch of a listed one is red. Censusing the file instead would have handed 30k-line `serve.rs` a blanket pass.

### Two premises corrected before deciding

Both were considered and are **wrong**; recording them so they are not re-proposed.

- **"Extend ADR-0110 D3 `durable_ack` to the daemon paths."** No effect. Daemon and system audit writes already go through the Handle (`db_handle.write()` / `state.db.write()`), and `durable_ack` does no I/O on the normal path — it claims an outcome the guard already produced. Worse, its `fsync_path` opens by-path, so post-swap it certifies the *new* inode while the live connection is on the orphan. The fence fixes that; extending the ack would not.
- **"Just force a checkpoint at shutdown."** Masks one stop and leaves the mid-run loss — which is the majority of it. And opening the DB fresh while the Handle is open on it is precisely the two-instance hazard ADR-0098 exists to prevent.

## 4. Proof

`crates/aberp-db/tests/checkpoint_swap_orphan.rs` — deterministic, milliseconds, no threads, no 60 s window.

The crux (`checkpoint_between_writes_loses_no_rows_and_never_puts_the_mirror_ahead`) writes row A, checkpoints, writes row B, drops the handle, and asserts a **fresh** `Connection::open` sees both, and that mirror == DB. Against a `checkpoint_now` stubbed with the pre-fix daemon body:

```
ROW B LOST. … A fresh open sees only ["{\"probe\":\"A-before-checkpoint\"}"]
MIRROR AHEAD OF DB: mirror has 2 entries (head seq 2) but the live DB has 1.
```

Green after P1. `checkpoint_actually_swaps_the_live_inode` is the **anti-vacuity control** and must be read as part of the crux: `durable_checkpoint` ABORTS on a WAL-fence trip and `live_durable_checkpoint` no-ops when a marker already covers the file, so "no rows lost" could otherwise pass for a reason unrelated to the fix. `an_out_of_process_swap_fails_the_ack_and_never_advances_the_mirror` pins the fence, and goes red when the fence branch is mutated out.

## 5. Consequences and accepted risks

- **The snapshot cycle now serializes against writes.** `checkpoint_now` holds the writer mutex across the primitive's EXPORT + IMPORT. This is **not a new class**: the handle's own debounced checkpoint (`WriteGuard::drop`) and `checkpoint_on_idle` already did exactly this. It widens an existing throughput ceiling that ADR-0098 accepted for a single-operator CNC-shop ERP. No deadlock is possible: `aberp-snapshot` does not depend on `aberp-db`, so nothing inside the checkpoint can call back into the handle, and the only other lock in play (`AUDIT_APPEND_LOCK`) is taken strictly *after* the handle mutex by `with_ledger` and never before it — `checkpoint_now` never takes it at all, so no inversion exists.

- **The checkpoint costs O(whole DB), not O(WAL) — and that cost is now paid under the writer mutex.** The primitive is a logical `EXPORT DATABASE` + `IMPORT DATABASE`, so it rewrites *everything*, however small the pending WAL. Measured on this branch (M-series dev box, debug build, audit rows only; one dirty write before each checkpoint so none is a marker no-op):

  | audit rows | live file | `checkpoint_now` |
  |---|---|---|
  | 200 | 0.5 MB | 0.29 s |
  | 2 000 | 0.8 MB | 0.39 s |
  | 20 000 | 2.9 MB | 1.42 s |

  Writes park for that whole window. At Defense's pilot scale (a single operator, a DB in the low MB) a ~1 s stall once per debounce window is not something an operator can perceive, and the daemon tick is off the request path entirely. It does **not** stay negligible: the cost tracks total DB size, so a tree that grows to hundreds of MB would be stalling writers for seconds every time the post-write debouncer fires. The lever when that day comes is the *cadence* (`min_checkpoint_interval`, the debounce window), not removing the lock — the lock is the fix. A release-build re-measure at real data volume is the trigger to revisit.

  (The PR #41 adversarial measured the same shape with different absolute numbers on its own box — 0.42 s at 200 rows, ~16 s at 20 k. The numbers above are the ones reproducible from this branch; the conclusion, that cost scales with DB size rather than WAL size, is the same either way and is what matters here.)
- **A daemon tick can now be blocked by a long write, and vice versa.** Both run on blocking threads, never on a runtime worker.
- **Clean shutdown waits on the writer mutex, but only for 2 s.** The path-based call did not wait at all. An earlier draft of this section — and the call-site comment — claimed the drain guaranteed no `WriteGuard` was alive by then. That is **false**, and the PR #41 adversarial disproved it: `ShutdownCoordinator::shutdown` races each daemon's `JoinHandle` against a shared 5 s budget with `tokio::time::timeout` and, on expiry, **drops** the handle — which *detaches* the tokio task rather than aborting it. A straggler is therefore still running and can still be mid-guard, so an unbounded wait could block process exit indefinitely (the S213 bug). `Handle::checkpoint_now_within` caps it and logs a miss loudly; giving up leaves the live file to the next boot's recovery path, exactly as after any unclean stop.
- **The fence costs one `stat` per committed write, per ack, and per `read()`.** `read()` is the hot path (once per request handler), so this is the one to watch; a `stat` on a warm dentry is ~1 µs against a DuckDB query, and the alternative is serving pre-restore rows forever. It is silent when the identity cannot be read (non-unix, or the file momentarily absent): a belt that fires on a stat race would be worse than the hazard.
- **`DbError` gains a variant** (`LiveFileSwapped`). Additive; the one external `match` on `DbError` has a fallback arm.

## 6. Follow-up (NOT closed by this ADR)

1. **The in-process operator restore.** `POST /api/snapshots/restore` → `snapshot_restore_request` → `restore_and_emit` → `aberp_snapshot::restore_into`, which builds `<target>.restoring` and renames it over `target`. Pointed at the live DB — the intended use — that orphans the shared connection exactly as the daemon checkpoint used to, **while `state.db` is open**. Found while enumerating the rename family for CHECK C-A; **pre-existing**, not introduced here, and censused with this reasoning in `tools/adr0111_rename_family_sites.txt`.

   No longer silent: the inode fence detects it on the next commit, skips the mirror sync so the mirror cannot run ahead, fails the money-path ack, and reopens — and the `read` arm stops the UI serving pre-restore rows. The residual is the in-flight write: lost, and now loudly reported. Closing it properly means quiescing the Handle across a restore, which is a design question this PR does not answer — a restore replaces the whole DB **including the audit chain**, so the reopened handle's ledger head, the JSONL mirror and `LedgerMeta` all need reconciling.

2. **Portable (`ABERP.git`) carries the same defect** on its non-money audit paths. The port is its own change.
