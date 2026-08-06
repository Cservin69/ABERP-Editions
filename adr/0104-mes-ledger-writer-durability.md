# ADR-0104 (Defense) — MES ledger-writer durability: drain on shutdown, retry failed appends, and stop forking the audit chain

- **Status:** **Accepted — implemented.** Written after measurement, because the premise handed to this session turned out to be wrong in its headline claim and the correction is the substance of the decision (§1).
- **Date:** 2026-08-06
- **Deciders:** Ervin Áben (set the scope: fix the MES adapter audit-durability defect; conservative option where ambiguous; no AskUserQuestion; do not merge). Investigation + implementation by Dispatch.
- **Base:** Editions `main` @ `e0ae99a` (PR #32 — the Trumpf laser seam). Every file:line below was read and every number below was measured in this session at that SHA, not inferred.
- **Related:** **ADR-0098 Gap 1a** (the ONE shared `aberp_db::Handle` — the surface this migrates onto, and the reason `append_reopen` is legacy); ADR-0099 + cut-gate CHECK 10M (the write-fork model that forced the correction, §3.1); ADR-0060 (the broadcast lossiness contract, §1.2); S341 / `append_reopen` (the superseded reopen-per-write surface); ADR-0098 R6 + CHECK 10j (the in-process-opener pragma guard, which §4.1 confirms the hard way); ADR-0008 §"Storage".
- **Provenance:** the failing-first probe (`crates/aberp-mes/tests/ledger_contention_probe.rs`) was written by the Trumpf adversarial review and recovered from its worktree. It is committed here as a gate.

---

## 0. TL;DR

The MES ledger-writer had **three** ways to lose an audit row, none of them licensed by ADR-0060. It also had a fourth, worse property nobody had named: **it could fork the audit hash chain.**

| # | Path | Real? | Licensed by ADR-0060? | Fix |
|---|---|---|---|---|
| 1 | Cancellation abandoned the undrained broadcast backlog | **yes** | **no** | bounded drain (`DRAIN_BUDGET`) |
| 2 | A failed append was logged once at WARN and dropped | **yes**, latent | **no** | retry (`WRITE_ATTEMPTS`) + ERROR on exhaustion |
| 3 | Two writer instances off the same head → **chain fork** | **yes** | **no** | migrate onto the shared `Handle` |
| 4 | `RecvError::Lagged` overflow | yes | **YES — explicitly** | unchanged, by design |

**The headline claim handed to this session was wrong.** The measured loss (24/40, 43/50 rows) was path **4** — a licensed `Lagged` overflow of `NoopAdapter`'s **16-slot fixture channel** — and *not* the `write_one` "logged-and-dropped" path it was attributed to. That path never fired once. Every real adapter ships a **1024**-deep channel. The correction is load-bearing: it changes both what to fix and what to claim, and it is why the probes in this ADR pin the production channel depth (§3).

The defects that *are* real were found underneath that wrong premise, and #3 is the serious one.

---

## 1. What was claimed, what was measured, what is true

### 1.1 The claim

> `write_one` logs `"event lost"` on a failed write and NEVER retries. Production shape (a shop with a laser + a CNC) silently loses ~14% of its adapter audit trail. This is NOT covered by the ADR-0060 lossiness contract.

Two checkable assertions: *the loss is caused by failed writes*, and *ADR-0060 does not cover it*.

### 1.2 The measurement

The probe reproduced exactly on `e0ae99a` — 23/40 and 43/50, matching the adversarial's numbers. Then instrumentation of the actual code paths:

- A raw `eprintln!` on the `Ok(Err(e))` write-failure arm of `write_one` — **never fired, in any shape.** Not one write failed.
- A counter of events actually written, printed at task exit — **exactly equal to the rows landed** (24 written / 24 rows). The writer wrote everything it received. It simply never received the rest.
- A raw `eprintln!` on the `RecvError::Lagged(n)` arm — **fired**: `n=4, 4, 4, 1`.

> **Method note.** The `Lagged` diagnostic was first placed *inside* a `tracing::warn!` field expression and did not print, which briefly looked like exoneration. `tracing` does not evaluate field expressions when no subscriber is interested. A field-expression probe inside a logging macro proves nothing about whether that arm ran; only a plain statement does.

The arithmetic closes: 40 emitted − 24 received = 16 = `NoopAdapter`'s `DEFAULT_CHANNEL_CAPACITY`, exactly.

### 1.3 What is true

Assertion one is **false**. The loss was broadcast overflow, not failed writes.

Assertion two is **false for the measured numbers**. ADR-0060 §"broadcast lossiness on the ledger-writer path will lose audit entries" licenses precisely this drop, and the code already honours the mitigation it demanded (loud WARN with a count).

And the "~14% of a production audit trail" figure does not survive either. It is an artifact of the fixture:

| Adapter | `DEFAULT_CHANNEL_CAPACITY` |
|---|---|
| `barcode_scanner`, `mtconnect`, `zebra`, `ur_rtde`, `trumpf` | **1024** |
| `NoopAdapter` (reference impl, emits nothing in production) | **16** |

The probe drove `NoopAdapter`. Every adapter a shop actually runs has a channel **64× deeper**, which is ADR-0060's "size the receiver generously" mitigation, honoured. A 40-event burst does not overflow 1024.

**This does not clear the writer.** It relocates the defect. Re-running the same probes at the production depth of 1024 — so overflow is off the table and only writer durability is under test — they still go red, for three different and genuinely undocumented reasons.

---

## 2. The three real defects

### 2.1 Shutdown abandoned the backlog

`run_ledger_writer`'s `tokio::select!` cancel arm did `return` (`ledger_writer.rs:96-99` @ `e0ae99a`). Every event already sitting in the broadcast, received or not, went with it. No drain.

This mattered *because* the writer was slow: a pre-fix write cost ~60 ms (fresh `Connection::open` + `ensure_schema` + tx + commit, every event). Any burst arriving faster than ~16 events/second built a backlog as a matter of course, so this fired on ordinary shop-floor traffic and not only under stress. Cancellation is not an exotic path — it is every Ctrl-C in `run_prod.sh` and every Tauri window close.

ADR-0060 says nothing about shutdown. A `Lagged` drop is deliberate, counted and bounded; this was silent.

### 2.2 A failed append was logged and dropped

`write_one`'s failure arms logged `"event lost"` at WARN and continued. One transient DuckDB contention — the single-writer file lock lost to the snapshot daemon, a dashboard query, another adapter's writer — and the row was gone forever.

Latent, not measured: it never fired in these probes. It is fixed anyway, because a dropped audit row is unacceptable regardless of how rarely it happens, and because §2.3's fix *increases* lock contention on this exact path.

### 2.3 The chain fork — the serious one

`write_one` hand-rolled its own `Connection::open` + `ensure_schema` + `conn.transaction()` + `write_mes_adapter_event` + `commit`, per event.

`mes_manager` spawns **one writer per adapter**. A shop with a laser and a CNC therefore has two writers, each opening its **own** DuckDB instance on one audit DB, holding **no** cross-writer lock. Two instances read the same committed chain head, both self-assign the same next `seq`, and the tamper-evident hash chain forks — the seq-515 fork primitive ADR-0099 named.

Every other in-process audit writer had already been moved off this shape and onto the ONE shared `aberp_db::Handle` (ADR-0098 Gap 1a), whose writer mutex serializes appends so the head is always current. **The MES writer was the last one that never moved** — the same recurring root cause as the snapshot-daemon audit-fork fix.

This is not theoretical. With the fix reverted, the probe's chain verification fails:

```
audit chain must verify after concurrent adapter writes: Chain(OutOfOrder { expected: 3, found: 2 })
```

The audit chain — the tamper-evidence substrate — **forks under two concurrent adapters.** That is an integrity defect, strictly worse than the row loss that prompted the investigation, and it was found only because the wrong premise was checked properly rather than taken on trust.

---

## 3. Decision

**Take option 1: migrate the ledger-writer onto the ONE shared `aberp_db::Handle`.** Add the bounded retry and the shutdown drain on top, because the shared Handle fixes the fork and the churn but does not by itself make shutdown or a failed append lossless.

### 3.1 The route not taken, and why the first attempt was wrong

This ADR's first draft chose `append_reopen` (the S341 serialized reopen-per-write surface) and **rejected** the shared Handle, on the reasoning that a long-lived connection cannot see commits made through other in-process reopen-writers and would therefore read a stale chain head — reintroducing the very fork it was meant to cure.

That reasoning was wrong, and the codebase says so plainly. **ADR-0098 Gap 1a already superseded `append_reopen` for exactly this class.** `email_outbox_poll_daemon` carries the record in its own comment: the audit append now runs

> on the ONE shared instance under the writer mutex (which serializes appends, so the chain head is always current → correct next seq), replacing the separate-instance `append_reopen` + its `AUDIT_APPEND_LOCK`.

The stale-head objection only bites when there are *several* writer instances. The shared Handle answers it by construction: there is exactly one writer instance in the process, its mutex serializes every append, and so every reader of the chain head is current by definition. `append_reopen` is now the *legacy* surface — the scanner evidence below is decisive on that point.

Two independent checks confirmed the correction rather than my reading of it:

- `append_reopen` has **zero** live callers in the tree. Its apparent callers in `email_outbox_poll_daemon` are comments describing the migration away from it.
- **CHECK 10M treats any `append_reopen` caller as a write-fork** and fails the build. Routing MES through it turned the cut-gate red. That is the gate stating the architecture directly: this class belongs on the Handle.

Had the first draft been trusted, this change would have shipped an architecture the gate forbids. The lesson is the one this repo keeps relearning — **a writer that never moved to the shared Handle** — and MES was the last in-process audit writer still off it.

### 3.2 What was implemented

1. **`LedgerWriterDeps.db_path` → `db: aberp_db::HandleArc`.** `try_write_once` now does `db.write()` + `ensure_schema` + tx + `append_in_tx` + commit — byte-for-byte the `email_outbox_poll_daemon` shape. `aberp-mes` takes a dependency on `aberp-db` (no cycle: `aberp-db` depends only on `audit-ledger` and `aberp-snapshot`). `mes_boot::MesBootDeps` carries the handle instead of the path; both `serve.rs` construction sites already had one in scope.

   This kills the per-event open/close churn as a side effect: the probe suite's wall-clock fell from **16 s to 2.9 s**, because a write went from ~60 ms to **~23.75 ms**.

   > **Corrected.** An earlier draft of this ADR said "~1 ms". That was inferred from the suite's wall-clock, not measured per-write, and it was wrong by more than an order of magnitude. Independent measurement puts a write at **~23.75 ms**: `ensure_schema` 7-11 ms + tx/append/commit ~7 ms + `WriteGuard::drop` / `sync_mirror` 11-27 ms. The migration is still a large win over ~60 ms, but "~1 ms" overstated it badly and is corrected everywhere it appeared.

2. **`drain_backlog`** — on cancellation, drain the broadcast with `try_recv` until empty, bounded by `DRAIN_BUDGET` (30 s). Overrun logs at **ERROR**: it is the one remaining path on which a cancelled writer leaves events unwritten, and it is loud. The select arm resolves to an `Option` first, because the `rx.recv()` branch holds a mutable borrow the drain also needs.

3. **`WRITE_ATTEMPTS` = 8** with an **exponential** backoff from 25 ms (≈3.2 s total). A flat 5×50 ms budget was tried first and measurably was not enough under a sibling reopening the audit DB every 5 ms. Exhaustion logs at **ERROR**, not WARN — an audit row that never lands is an integrity event, not an operational nuisance.

4. `tokio`'s `time` feature added to `aberp-mes` for the backoff.

### 3.3 Effect on the frozen ledgers

`aberp-mes` now owns **zero** runtime openers, so every ADR-0098/0099 ledger **shrinks** — the direction they are allowed to move:

| Ledger | Before | After |
|---|---|---|
| CHECK 10i residual openers | 114 across 26 files | **113 across 25** |
| CHECK 10k opener fingerprints | 101 | **100** |
| CHECK 10M write-fork residual | 0 | **0** (unchanged) |

`crates/aberp-mes/src/ledger_writer.rs` is delisted from both manifests, joining the other Handle-migrated files. Its stale prose in the residual ledger — which still described the opener as a deferred v0.2.6 target — is corrected in place. Every count above came from running the scanners (`tools/adr0098_opener_scan.awk`, `tools/adr0099_write_fork_scan.awk`), not from reading the manifests.

> **Gate blind spot, reported not fixed.** The pre-fix code was a genuine write-fork (independent opener + append, no lock) and **CHECK 10M did not flag it**, because the append sat behind the `write_mes_adapter_event` wrapper rather than appearing as a bare `append_in_tx(` in the same function. The scanner matches append tokens lexically within one function body. Any fork hidden behind a one-line helper is invisible to it. Verified by running the scanner against the base revision: it returns nothing. See §6.

## 4. Teeth

`crates/aberp-mes/tests/ledger_contention_probe.rs`, at the production channel depth of 1024 (`PROD_CHANNEL_CAPACITY`), so no test can pass by accident of a licensed `Lagged` drop. Five tests: the two original shapes, a 4-adapter higher-concurrency burst, and the two single-event controls. The concurrency shapes assert **`verify_chain`** as well as row count — count alone cannot see a fork.

### 4.1 The probe's own reader was corrupting the ledger

With the fix in place the burst shape still lost 1–2 rows, intermittently, and instrumentation showed **no write ever failed and no retry ever ran**. The events were not lost by the writer at all.

The cause was the probe's polling reader. It opened the audit DB read-write every 5 ms (~400 opens per run) and dropped the connection **without** `PRAGMA disable_checkpoint_on_shutdown`, so its close could fold the shared WAL in place (duckdb#23046) and destroy a just-committed row. The reader was eating the writer's rows, and the assertion blamed the writer.

Adding the pragma to the poller — the same guard every opener in the tree carries — made the probe green 6 runs out of 6. This is worth recording twice over: it is an independent, accidental confirmation of exactly why ADR-0098 R6 exists, and it is a standing warning that a test harness which opens the audit DB unguarded will manufacture "durability defects" that are its own.

The probes' writer joins were also made assertive (`JOIN_TIMEOUT_SECS`, 120 s). They previously discarded the timeout result, so on a loaded machine the row count could be read while the writer was still draining — a spurious red that looked exactly like event loss.

### 4.2 Mutation results

Mutation-verified, isolating each half:

| Build | Rows | Chain |
|---|---|---|
| Full fix | **6/6 pass** | intact |
| **B** — shared Handle reverted to the per-event opener (drain + retry kept) | 3 red (39/40) | **forked** — `OutOfOrder { expected: 2, found: 1 }`, in BOTH concurrency shapes |
| **A** — shared Handle kept, **drain disabled** | 1 red (4-adapter burst) | intact |
| **C** — shared Handle + drain kept, **retry removed** | 1 red (0/1 rows) | intact |

Mutation **B** proves the Handle migration's teeth: with everything else in place, reverting just the writer instance forks the chain. Mutation **A** proves the drain's, and **C** the retry's — the three fixes are independent, none masking another.

**Mutation C was added late, and only because an adversarial found it missing.** The retry shipped in the first draft of this change with *zero* coverage: deleting it outright produced no red test at all. One of three advertised fixes was unverified. `a_failing_append_is_retried_rather_than_dropped` now closes that. It induces a real failure through the real write path rather than through an injected seam — the `audit_ledger` table is renamed aside and a VIEW put in its place, so `ensure_schema`'s `CREATE TABLE IF NOT EXISTS` hits a catalog-type conflict (and, if that ever no-ops, the follow-on `append_in_tx` still cannot INSERT into a view). Healing the schema at ~500 ms leaves several of the eight attempts in hand, so it is not a tight race. With the retry removed, the single attempt at t≈0 fails and the row is gone: **0/1**.

**Report on A:** the drain is now caught by the 4-adapter tight-burst shape *only*. On the shared Handle a write costs ~23.75 ms instead of ~60 ms, so the gentler two-writer and single-burst shapes no longer accumulate a backlog before cancellation and stay green without it. The drain still guards a real path — a fast writer can still be behind when a burst coincides with shutdown — but its coverage now rests on that one probe. Removing or weakening `four_concurrent_ledger_writers_lose_no_events` would silently un-gate the drain.

---

## 5. Consequences

- Adapter audit rows survive shutdown, transient write contention, and concurrent adapters.
- **The audit chain no longer forks *among MES writers on the shared Handle*.** That qualifier is load-bearing and this ADR should not be read as claiming more. The ledger still has **two disjoint lock domains** — `append_in_tx` takes no lock of its own, so a writer holding the `Handle` mutex and a writer holding `AUDIT_APPEND_LOCK` do not exclude each other — and a cross-domain fork remains reachable. That is **pre-existing**, not introduced here; its live trigger is `audit_dap_boot::heartbeat`, which is **default OFF**. Being on the Handle is what makes MES *eligible* for the eventual fix rather than a second forking domain of its own. Tracked as a separate follow-up (§6.1), together with the CHECK 10M blind spot.
- **`aberp-mes` now depends on `aberp-db`.** A hardware-adapter crate taking a DB-handle dependency is a real coupling; it is the price of the writer holding the shared instance rather than a path. The alternative — a trait seam with the Handle impl in `apps/aberp` — was considered and rejected as more machinery for the same result, and it would have left the probes testing a stand-in rather than the production surface.
- The per-event open/close churn is gone: a write went from ~60 ms to ~23.75 ms, and the probe suite from 16 s to 2.9 s. Backlogs are now far harder to build, which is also why the drain's test coverage narrowed (§4.2).
- Every MES append now serializes on the shared Handle's writer mutex with every other in-process audit writer. That is the same contention every migrated daemon already accepts, and it is what makes the chain single-writer.
- **`DRAIN_BUDGET` (30 s) is a backstop, not the budget the writer gets — and the overrun ERROR cannot be relied on in production.** An earlier draft implied otherwise; corrected here. `ShutdownCoordinator`'s `DEFAULT_SHUTDOWN_TIMEOUT` is **5 s**, shared as ONE deadline across **~13 registered daemons**, with MES registered mid-list — so MES sees a slice of 5 s, never 30 s. And on overrun the coordinator does `tokio::time::timeout(remaining, daemon.handle)`, which **drops** the `JoinHandle`: dropping a tokio `JoinHandle` *detaches* the task rather than aborting it, so the writer drains on into an exiting process and the loud `UNWRITTEN` error is not guaranteed to be emitted.

  The 30 s is deliberately left above the coordinator's window: it exists to stop an unbounded drain, and clamping it to 5 s would only make the writer give up early in tests and in embeddings that do grant more time. **This is not a live loss path** — MTConnect/Trumpf poll on a 5 s cadence and emit on state-change, so real backlogs sit far below what a 5 s slice drains — but the sizing is acknowledged rather than left implied.
- `Lagged` remains licensed and unchanged. At 1024 deep it takes a sustained overload to reach it, and it stays loud and counted when it does.

## 6. Follow-ups (not in this change)

1. **The cross-lock-domain chain fork.** The ledger has two disjoint lock domains — `append_in_tx` takes no lock of its own, so a writer holding the `Handle` mutex and one holding `AUDIT_APPEND_LOCK` do not exclude each other. A fork across them is still reachable. **Pre-existing**, not introduced here; live trigger is `audit_dap_boot::heartbeat`, default OFF. This is why §5 qualifies the fork claim to "among MES writers on the shared Handle." Being on the Handle makes MES eligible for the eventual fix instead of being a second forking domain. Owned by a separate follow-up alongside item 2.
2. **CHECK 10M's wrapper blind spot** (§3.3). The pre-fix MES write-fork — an independent opener plus an append, the exact primitive ADR-0099 bans — was invisible to the scanner because the append sat behind `write_mes_adapter_event` instead of appearing as a bare `append_in_tx(` in the same function body. Verified against the base revision: the scanner returns nothing. Any fork hidden behind a one-line helper is unguarded today. This is the highest-value follow-up here: the gate reads as green while blind, which is precisely the failure mode ADR-0098's F1-F4 review set out to end.
3. **A test harness that opens the audit DB unguarded manufactures false durability defects** (§4.1). The probe's own poller cost 1-2 rows a run until it carried `PRAGMA disable_checkpoint_on_shutdown`. Worth a lint or a shared test helper, since the next probe author will hit it too.
4. **`NoopAdapter::DEFAULT_CHANNEL_CAPACITY = 16`** contradicts ADR-0060's "size generously" on the crate's own reference adapter — the fixture footgun that produced the wrong premise in §1.
5. **The `Lagged` counter ADR-0060 promised** ("emit a counter the future operations dashboard surfaces") is still only a log line, in both the steady-state and drain arms.
