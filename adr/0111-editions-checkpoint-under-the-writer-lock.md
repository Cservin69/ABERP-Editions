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

**P2a (done).** An **inode fence**: `Inner.fence` records `(st_dev, st_ino)` at every open (boot, `ensure_open`, the post-checkpoint reopen); `WriteGuard::drop` compares it first and, on a mismatch, skips **both** the by-path `fsync` and the mirror sync, parks `DbError::LiveFileSwapped` for `durable_ack` to claim, and drops the stranded connection so the next call reopens on the live file. In-process this is unreachable after P1 — it is the belt for an out-of-process swapper (a second `aberp` invocation, an operator restore, a backup tool), the one case no in-process discipline can prevent. It also closes the `durable_ack` by-path residual: before it, `durable_ack` returned `Ok(())` after a swap, certifying a write that went to an orphan.

**P2b (done).** `tools/cut_gate_checkpoint_sites.sh` + the frozen census `tools/adr0111_checkpoint_sites.txt`, wired into both CI workflows with a 13-probe teeth harness.

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
- **A daemon tick can now be blocked by a long write, and vice versa.** Both run on blocking threads, never on a runtime worker.
- **Clean shutdown now WAITS on the writer mutex** (the path-based call did not). Bounded in every non-pathological case — the lock is held for one transaction, or for one in-flight checkpoint, after which ours is a marker no-op. The unbounded case is a daemon permanently wedged inside a `WriteGuard`, i.e. a process whose every write path is already dead and which the 5 s drain has already failed to stop. Accepted, because *not* holding the lock here is exactly what corrupted the mirror. A checkpoint **error** still never wedges exit (S213) — it is logged and swallowed inside the handle.
- **The fence costs one `stat` per committed write.** It is silent when the identity cannot be read (non-unix, or the file is momentarily absent): a belt that fires on a stat race would be worse than the hazard.
- **`DbError` gains a variant** (`LiveFileSwapped`). Additive; the one external `match` on `DbError` has a fallback arm.
- **Portable carries the same defect** on its non-money audit paths (`ABERP.git`). A port is a **follow-up**, not this change.
