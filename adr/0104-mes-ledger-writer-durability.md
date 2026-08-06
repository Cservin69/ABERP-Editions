# ADR-0104 (Defense) — MES ledger-writer durability: drain on shutdown, retry failed appends, and stop forking the audit chain

- **Status:** **Accepted — implemented.** Written after measurement, because the premise handed to this session turned out to be wrong in its headline claim and the correction is the substance of the decision (§1).
- **Date:** 2026-08-06
- **Deciders:** Ervin Áben (set the scope: fix the MES adapter audit-durability defect; conservative option where ambiguous; no AskUserQuestion; do not merge). Investigation + implementation by Dispatch.
- **Base:** Editions `main` @ `e0ae99a` (PR #32 — the Trumpf laser seam). Every file:line below was read and every number below was measured in this session at that SHA, not inferred.
- **Related:** ADR-0060 (Stage 3 adapter framework — the broadcast lossiness contract, §1.2); S341 / `append_reopen` (the serialized reopen-per-write surface this adopts); ADR-0098 R6 + cut-gate CHECK 10j (the in-process-opener pragma guard); ADR-0008 §"Storage" (ledger entries ride the same tx as their state change).
- **Provenance:** the failing-first probe (`crates/aberp-mes/tests/ledger_contention_probe.rs`) was written by the Trumpf adversarial review and recovered from its worktree. It is committed here as a gate.

---

## 0. TL;DR

The MES ledger-writer had **three** ways to lose an audit row, none of them licensed by ADR-0060. It also had a fourth, worse property nobody had named: **it could fork the audit hash chain.**

| # | Path | Real? | Licensed by ADR-0060? | Fix |
|---|---|---|---|---|
| 1 | Cancellation abandoned the undrained broadcast backlog | **yes** | **no** | bounded drain (`DRAIN_BUDGET`) |
| 2 | A failed append was logged once at WARN and dropped | **yes**, latent | **no** | retry (`WRITE_ATTEMPTS`) + ERROR on exhaustion |
| 3 | Two writers bypassed `AUDIT_APPEND_LOCK` → **chain fork** | **yes** | **no** | route through `append_reopen` |
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

`write_one` hand-rolled its own `Connection::open` + `ensure_schema` + `conn.transaction()` + `write_mes_adapter_event` + `commit`.

`audit-ledger` already has a surface for exactly this, `append_reopen`, whose own doc comment reads:

> the safe replacement for the hand-rolled `Connection::open` + `ensure_schema` + `append_in_tx` + `commit` pattern the high-frequency daemons used […] plus the process-wide append lock so two in-process writers cannot read the same head and fork the chain now that the `UNIQUE(seq)` ART is gone.

`mes_manager` spawns **one writer per adapter**. A shop with a laser and a CNC has two, appending to one audit DB, holding **no lock** — the precise scenario `AUDIT_APPEND_LOCK` exists to prevent. `email_outbox_poll_daemon` was migrated to `append_reopen` under S341. The MES writer never was. It is the same recurring root cause the snapshot-daemon audit-fork fix hit: **a writer that never moved to the shared surface.**

This is not theoretical. With the fix reverted, the probe's chain verification fails:

```
audit chain must verify after concurrent adapter writes: Chain(OutOfOrder { expected: 3, found: 2 })
```

The audit chain — the tamper-evidence substrate — **forks under two concurrent adapters.** That is an integrity defect, strictly worse than the row loss that prompted the investigation, and it was found only because the wrong premise was checked properly rather than taken on trust.

---

## 3. Decision

Adopt **option 2 (bounded retry / no silent drop)**, plus the drain and the migration to `append_reopen`. **Reject option 1 (a single shared long-lived `Handle`)** — deliberately, and against the stated preference.

### 3.1 Why option 1 is rejected

The brief preferred one shared long-lived Handle, mirroring the snapshot-daemon fix. That instinct is right about the disease — the churn — and wrong about the cure *here*, because the audit ledger has a coherence requirement the snapshot path does not.

`append_reopen` reopens per write **on purpose**. Its doc calls the fresh open "the coherence mechanism every ABERP daemon relies on, S335": a new `Connection` reads the *current* on-disk committed head. Independent `Connection::open` calls in duckdb-rs get independent database instances with independent buffer managers, so a **long-lived** connection is not guaranteed to observe commits made through the other in-process writers that still use `append_reopen` — `email_outbox_poll_daemon`, quote-intake, AP sync.

A long-lived MES connection would therefore read a **stale chain head** and append on top of it: the exact fork of §2.3, reintroduced by the fix meant to cure it, and this time not fixable by a lock, because the staleness is in the snapshot rather than the interleaving. Killing the churn would mean migrating *every* in-process audit writer to one shared handle in one change — a far larger, riskier blast radius than this defect warrants, on a line that is in pilot.

**Conservative option, taken and flagged:** keep reopen-per-write; make it correct and lossless. The churn stays, and with it the ~60 ms write cost. That is a performance property, not a durability one, and §3.2 removes its ability to lose data. Consolidating the audit writers onto one handle remains the right long-term move and is **out of scope here** — flagged for its own workstream.

### 3.2 What was implemented

1. **`append_reopen` in `try_write_once`** (`ledger_writer.rs`) — replaces the hand-rolled `Connection::open` + `ensure_schema` + tx + commit. The kind / payload-bytes / idempotency-key mapping stays single-sourced in `audit::mes_adapter_event_parts`, which deliberately **opens nothing**. `write_mes_adapter_event` is kept unchanged for in-tx callers; the two are documented as *not* interchangeable (an adapter event has no sibling state change to ride, so the writer owns the whole window). The ADR-0098 R6 pragma guard now comes from inside `append_reopen` rather than being restated.

   > **Gate note.** The call deliberately lives in `ledger_writer.rs` and not in `audit.rs`. The opener scanner counts `append_reopen(` as an opener, and CHECK 10i fails on a *new* opener-bearing file. Keeping it in `ledger_writer.rs` holds that file at its frozen count of 1 and leaves `audit.rs` at 0, so **`tools/adr0098_c2_frozen_residuals.txt` needs no edit at all**. Verified by running `tools/adr0098_opener_scan.awk` on both files rather than by reading the manifest. CHECK 10k (per-opener fingerprints) does still change by exactly one line — the opener's text and enclosing function changed — and that re-freeze is the only governance edit here.

2. **`drain_backlog`** — on cancellation, drain the broadcast with `try_recv` until empty, bounded by `DRAIN_BUDGET` (30 s). Overrun logs at **ERROR**: it is the one remaining path on which a cancelled writer leaves events unwritten, and it is loud. The select arm was restructured to resolve to an `Option` first, because the `rx.recv()` branch holds a mutable borrow the drain also needs.
3. **`WRITE_ATTEMPTS` = 8** with an **exponential** backoff from 25 ms (≈3.2 s total). A flat 5×50 ms budget was tried first and measurably was not enough: under a sibling reopening the audit DB every 5 ms, single events still failed all their attempts. Exhaustion logs at **ERROR**, not WARN — an audit row that never lands is an integrity event, not an operational nuisance.
4. `tokio`'s `time` feature added to `aberp-mes` for the backoff.

`NoopAdapter`'s 16-slot default is left **unchanged**: nothing depends on it, and raising it would paper over §1.3 rather than record it. It is a fixture-shaped footgun on the crate's copy-paste reference adapter — flagged, not fixed here.

---

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
| Full fix | **5/5 pass**, 6 consecutive runs | intact |
| Fix reverted entirely | 3 red (26/40) | **forked** — `OutOfOrder { expected: 2, found: 1 }` |
| `append_reopen` kept, **drain disabled** | 3 red | **intact** |

The middle row proves the drain's teeth; the third row proves `append_reopen`'s, and that the two fixes are independent rather than one masking the other.

---

## 5. Consequences

- Adapter audit rows survive shutdown, transient write contention, and concurrent adapters. The audit chain no longer forks under a laser + CNC.
- Shutdown may now take up to `DRAIN_BUDGET` longer in the worst case. At ~60 ms/event a full 1024-deep backlog would exceed 30 s and log ERROR with a count — visible, bounded, and far better than the silent discard it replaces.
- Every MES append now serializes on `AUDIT_APPEND_LOCK` with every other in-process audit writer. Throughput per adapter is bounded by that lock; this is the price of not forking the chain, and §2.2's retry exists partly to absorb it.
- `Lagged` remains licensed and unchanged. At 1024 deep it takes a sustained overload to reach it, and it stays loud and counted when it does.

## 6. Follow-ups (not in this change)

1. **Consolidate all in-process audit writers onto one shared handle** (§3.1). The real cure for the churn; needs its own ADR and blast-radius analysis.
2. **`NoopAdapter::DEFAULT_CHANNEL_CAPACITY = 16`** contradicts ADR-0060's "size generously" on the crate's own reference adapter (§3.2).
3. **The `Lagged` counter ADR-0060 promised** ("emit a counter the future operations dashboard surfaces") is still only a log line, in both the steady-state and drain arms.
