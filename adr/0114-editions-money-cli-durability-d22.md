# ADR-0114 (Defense) — D-22: the NAV money-submission CLI paths join the D3 durable contract

- **Status:** **Proposed — implemented; adversarial round 1 RUN and its required fixes applied (r2). Not merged.**
- **Round 2 (2026-08-27):** the durability work itself was found correct and fully tested (21 write-windows ↔ 21 acks, exact; the false-success path closed; the 13-of-15 decision structurally safe) and is untouched. Every r2 change is in the GATE tooling or in this document: the rotted P8 constant is now derived (§4), the `set -e` empty-manifest bug is guarded at all three sites rather than the one D-22 tripped (§4), CHECK D3-A is a per-file equality closing the cross-file ack relocation (§7.1), and §3/§7.3/§7.6–§7.9 correct or add residuals the round surfaced. No check was loosened.
- **Date:** 2026-08-26
- **Deciders:** Ervin Áben (scope: close D-22, the genuinely-live durability gap on the NAV money paths; fresh worktree off `main`; open a PR, do **not** merge — an adversarial re-attack and a v0.6.3 cut follow). Implementation by Dispatch.
- **Base:** Editions `main` @ `bae151d` (QC/FAIR Phase 1 + the v0.6.1 audit-fork durability work). Every file:line below was reproduced in this session at that SHA.
- **Related:** **ADR-0098 Gap 1a/1b + C2** (the ONE shared `aberp_db::Handle`; the seven files this extends the C2 set from); **ADR-0099 §R2.2** (which *named* this gap and deliberately deferred it); **ADR-0105** (the wrapper-hidden-fork gate whose frozen residual this empties); **ADR-0110 D3** (`fsync_data_paths` on `WriteGuard::drop`, `durable_ack`, and the census this extends); **ADR-0111** (the inode fence the acks below rely on).

> **On the number.** `0113` and `0115` are both claimed by *unmerged* branches (the auto-probe pricing ADR and the internal-portal ADR); the highest ADR on `origin/main` is `0112`. `0114` is free on `main` and free in both of those branches. Whoever merges second must not renumber this one silently — the string `ADR-0114` appears in the source comments, the two frozen manifests, the gate script and the test file.

---

## 0. TL;DR

ADR-0110 D3 closed the durability inversion on the **serve/daemon** path in v0.4.x. The **CLI money-submission** paths were never on that mechanism. Eight commands did `Ledger::open → append → sync_mirror` on their own `Connection` with `PRAGMA disable_checkpoint_on_shutdown` set — so the audit **mirror** was explicitly `fsync`ed while the DB row it mirrors was left in an un-`fsync`'d WAL that the connection's close deliberately did not fold. Three more were on the Handle but never CLAIMED the flush outcome, so a failed flush printed `submitted invoice …` anyway.

| | |
|---|---|
| **Symptom** | A crash or power loss after a NAV submission lands, but before the local record is durable, loses the record of a **filed ÁFA invoice**. The mirror keeps it → mirror-ahead → Defense's boot auto-heal replays a delta the DB never had |
| **Blast radius** | Every NAV money CLI: `submit-invoice`, `retry-submission`, `drain-submission-queue`, `drain-pending-retries`, `poll-ack`, `poll-annulment-ack`, `submit-annulment`, `observe-receiver-confirmation`, `recover-from-nav`, `mark-abandoned`, `request-technical-annulment` |
| **Fix** | All eight un-routed commands go on the shared `aberp_db::Handle`; every money-path ack boundary in all eleven calls `db.durable_ack()?` **before** the operator is told it worked |
| **Gate** | `tools/cut_gate_durable_ack.sh` census 5 → 26 sites; the eight files promoted from CHECK 10i's *frozen, may-not-grow* ledger into CHECK 10h's *zero, ENFORCED* set; two frozen fork manifests emptied |
| **Spec** | `apps/aberp/tests/d22_money_cli_power_loss_durability.rs` — power loss + fault injection on the real path, three mutations run and killed |

---

## 1. Context — what ADR-0099 §R2.2 said, and what it deferred

ADR-0099 §R2 established that the seq-2508 incident was a **lost DB commit**, not a writer fork: pre-D3, `WriteGuard::drop` `fsync`ed the audit MIRROR on every commit and never flushed the DB — the durability ordering exactly inverted, so an unclean stop keeps the mirror line and loses the DB row.

§R2.2 then says, in as many words, that its own "a pre-existing residual of the DEPLOYED binary, not a live code gap" qualifier is **load-bearing** and true of the serve/daemon path **only**:

> The CLI **money**-submission paths on THIS tree still carry the pre-D3 inversion […] That is a **live code gap on NAV money paths**, not a deployment gap, and it is live at `main` right now. It is out of scope here only because folding a money-path change into a daemon-heartbeat fix would have made both unreviewable.

That deferral was Ervin's call and it was the right one. This ADR is the deferred half.

---

## 2. The two shapes that were open

### 2a. Eight commands never flushed anything

```rust
let ledger = Ledger::open(db_path, …);   // independent connection, no Handle
ledger.append(…);                        // commit
ledger.sync_mirror(&mirror_path);        // mirror → sync_all()  DURABLE
```

No Handle, no `durable_ack`, no `fsync_data_paths` — and `Ledger::open` sets `PRAGMA disable_checkpoint_on_shutdown`, so the connection's close deliberately folded nothing either. The mirror was explicitly made durable; the DB's durability was left to whatever DuckDB does with its WAL on commit, which is precisely the assumption D3 exists to stop relying on.

Reproduced at `bae151d` in: `drain_submission_queue` (×3), `retry_submission` (×4), `submit_annulment`, `poll_annulment_ack`, `observe_receiver_confirmation`, `recover_from_nav`, `mark_abandoned`, `request_technical_annulment`.

### 2b. Three commands flushed but discarded the result

`submit_invoice`, `poll_ack` and `drain_pending_retries` were already Handle-routed (ADR-0098 C2), so the `WriteGuard` drop DID run `fsync_data_paths`. But nothing claimed the parked outcome. The `Inner::last_ack` `Result` sat unread, the guard's failure path logged a `tracing::error!`, and the command printed `submitted invoice … -> NAV transactionId …` regardless.

That is the **exact** downgrade CHECK D3-C forbids by name —

```rust
if let Err(e) = db.durable_ack() { tracing::warn!(..) }
```

— reached by omission rather than by a `warn!`. D3-C could not see it, because D3-C only inspects sites the census lists, and these were not in the census. This shape is why the census had to grow rather than merely be re-derived.

---

## 3. Decision

**Route every NAV money-submission CLI path through the same shared `aberp_db::Handle` the serve/daemon path uses, and CLAIM the flush at every ack boundary.**

No new durability primitive. The mechanism is entirely ADR-0110's, unchanged:

* the write happens inside a tight `db.write()` window;
* the `WriteGuard`'s drop runs `fsync_data_paths` (main file → WAL → tenant directory) **first**, and **skips** the lockstep `sync_mirror` if that flush failed, so the mirror can never end up ahead of the DB (ADR-0110 B2);
* the money path then calls `db.durable_ack()?`, which takes the parked `Result` and propagates it.

**The ack sits at the boundary of the WRITE it belongs to, not once per command.** An Attempt-before-call row must be durable BEFORE the wire send — a power loss mid-flight otherwise leaves NAV holding a submission ABERP has no record of attempting — and the TX2 row must be durable before the operator is told the filing landed. That is why several files carry three or four census lines.

That rule is the DESIGN, and it is satisfied on every `manageInvoice` path. It is **not** universally satisfied in this tree: `submit_annulment::run` has no Attempt-before-call row at all — its first and only write happens *after* `manageAnnulment` returns — so the sentence above describes what D-22 enforces where a pre-call row exists, not an invariant the whole NAV surface holds. §7.6 records that open path rather than letting the general claim paper over it.

**The writer mutex is never held across a NAV wire send.** Each transaction takes its own window and releases it; the `!Send` `WriteGuard` never crosses an `.await`. This is the shape `submit_invoice` and `drain_pending_retries` already had, copied rather than invented.

**The explicit `Ledger::open` + `sync_mirror` tails are removed, not kept alongside.** They were a SECOND live opener of the tenant DB (the duckdb#23046 replay locus) and they were the half that made the mirror durable ahead of the DB. The `WriteGuard` drop's lockstep `sync_mirror_lockstep` covers the mirror.

**Post-commit `verify_chain` reuses a shared READ clone** (`Ledger::from_connection`), never a re-open — the "reuse, never re-open" discipline `submit_invoice::verify_chain_and_sync_reusing_conn` and `drain_pending_retries::verify_chain_reusing_read` already carry.

---

## 4. What the gates now hold

| gate | before | after |
|---|---|---|
| `cut_gate_durable_ack.sh` census | 5 sites | **26** sites (D3-A/B/C hold all of them; **A is a per-file equality** and B a whole-tree equality, so a deleted ack, an unregistered new one, and an ack MOVED between two censused files are all equally red) |
| CHECK 10h — C2 set, **zero openers ENFORCED** | 7 files | **15** files |
| CHECK 10i — frozen residual ledger | 110 openers / 24 files | **75 / 16** |
| CHECK 10k — opener fingerprints | 97 | **62** |
| CHECK 10L-b — mirror-fork sites | 12 frozen | **0** |
| CHECK 10N — wrapper-hidden fork residual | 1 frozen | **0** |
| CHECK 10P — sanctioned non-shared audit writers | 22 frozen | **13** |

Every one of those is a strict tightening: the eight files moved from "frozen, may not grow" to "zero, and a re-added opener is a red build."

Three things in that table need justifying rather than just reporting.

**The eight files were promoted into the C2 set, not merely delisted.** A file that reaches zero openers is simply *skipped* by 10i, so leaving it listed with a stale count would have been green and blind. Adding it to `c2_files` makes CHECK 10h assert the zero and assert the file still calls `.read()`/`.write()`.

**CHECK 10L-b's manifest is now EMPTY, and three of the removals were not mine.** Nine of the twelve entries were the D-22 commands. The other three (`email_invoice::record_email_audit_entry`, `restore_from_nav_outgoing::process_digest`, `serve::set_restored_partner_request`) had already migrated off before this session — the gate reported them under its own `(info) migrated off since freeze` line at `bae151d`. Three phantom entries in a may-only-shrink manifest are three sites that could have regrown a fork unnoticed, so they were removed rather than carried. `git diff` shows them; they are called out here so the reviewer is not surprised by a change outside D-22's stated scope.

**Emptying a may-only-shrink manifest exposed a latent gate bug — in three places, not one.** `grep -v '^#'` (and `grep -vc`) exit 1 on an all-comment file. Under `set -euo pipefail` an unguarded such command in a pipeline or a bare assignment **kills the gate script mid-run**, so every check after it never executes and the script never prints its summary.

Three sites carried the bug. CHECK 10L-b's pipeline is the one D-22 tripped, because D-22 is what emptied its manifest. The other two were found by the D-22 adversarial and are fixed in the same change: CHECK 10i's frozen-residual-ledger tally (`ft`/`ff`, reached only in the fully-migrated terminal state where the manifest is all comments *and* the scanner finds no residual openers) and CHECK 10k's fingerprint-baseline read (`grep -vE '^#' "$fpfile" | sort > "$froz"`, reached on **any** run against an all-comment fingerprint manifest). Reproduced against the pre-fix script: with an all-comment `tools/adr0098_r4_opener_fingerprints.txt` the gate died immediately after printing the `[CHECK 10k]` header — 17 check headers, no 10M/10P/10N, no `CUT-GATE:` line. With the fix it runs all 20 and reports an ordinary red. CHECK 10M and 10N already carried the guard (their manifests were already empty).

**Two things the first draft of this section got wrong, corrected here.** The failure is a **hard RED — `exit 1`, with the output truncated mid-check** — not a silent pass and not a green gate; a reader who took "silent loss of six checks" to mean *undetected* would have the risk backwards. And the bug was never *live* on `main`: CHECK 10L-b's manifest held **12 entries** there, so the guarded branch was never reached and **no historical cut-gate green was blind**. The exposure is prospective — it arms the moment a manifest reaches its all-comment terminal state, which is exactly the state these may-only-shrink ledgers are designed to converge to. That is why all three are guarded now rather than only the one that fired.

**The durable-ack probe harness carried a rotted constant.** `cut_gate_durable_ack_probes.sh`'s P8 asserts D3-C's *precision* — that the P7 mutation flags the one site it downgrades and clears every other. It hard-coded `4` propagating sites from the five-site census. D-22 grew the census to 26, so P8 went red (`1 / 61`, on the un-deduped count) against a gate whose property still held exactly. The constant is now **derived**: the harness runs the gate on the unmutated tree, counts its D3-C verdict lines, and expects that minus one — with a floor assertion, since a derived expectation can go vacuous the way a literal cannot (a dead matcher reporting nothing would satisfy "baseline − 1" if the baseline were 1).

---

## 5. Deliberately NOT changed: the two `aberp-snapshot::recover` sites

The D-22 backlog entry counts fifteen sites and the last two are in `crates/aberp-snapshot/src/recover.rs`. They stay as they are, and this is a decision rather than an oversight:

* **`build_and_validate` §5d** (`recover.rs:809`) tops the lagging mirror up to an ahead-snapshot's head — and its result is an **input to the self-certification gate**: `if topped_head != chain_len { return Refuse }`. It decides Recover-vs-Refuse, so it structurally cannot move after the install. Reordering it means redesigning ADR-0098 D3's ahead-snapshot certification.
* **`append_staged_audit_row`** (`recover.rs:631`) appends the `db.auto_recovered` breadcrumb into the **private staging file**, which is then folded (`fold_staging_wal`) and committed by `atomic_install` — `fsync_file` → `rename` → `fsync_dir`, a durable commit. The append is explicitly best-effort: "a failed append is logged, never fatal (the recovery is still durable)."

Neither writes a money row, both run at boot before any `Handle` exists, and the DB catches up to the mirror within the same boot. The residual is the window between the mirror top-up and the install; a crash there re-enters the same recovery on the next boot. Named here, and in `tools/adr0110_durable_ack_sites.txt`, rather than quietly dropped from the count.

---

## 6. The spec — `apps/aberp/tests/d22_money_cli_power_loss_durability.rs`

`mark_abandoned` is the one D-22 path an unattended test can drive end-to-end: a terminal money-path decision (the invoice's sequence is burned for good) with **no NAV call**. Its library core was split out of the CLI wrapper — `mark_abandoned_from_inputs`, on the `submit_invoice::submit_from_inputs` / `poll_ack::poll_ack_from_inputs` precedent — so the test can own the `Handle`.

Three tests, and **three mutations were run, not reasoned about**:

| mutation | what it restores | killed by |
|---|---|---|
| **M1** — `db.durable_ack()?` → `if let Err(e) = … { warn!() }` | the ADR-0110 R3 downgrade | the fault-injection test ONLY |
| **M2** — `db.write()` → independent pragma-fenced `Connection::open` + `Ledger::open` + `sync_mirror`, ack kept | the pre-D-22 opener | the power-loss test |
| **M3** — both together | the pre-D-22 posture verbatim | all three |

### 6a. The design took a wrong turn first, and the wrong version was green

The obvious design is the one `adr0110_d3_power_loss_durability.rs` uses: burn a warm-up write to enter the D2 debounce shadow, then measure write #2. Written that way, **M3 passed**.

The reason is worth recording. `fsync_data_paths` journals `<db>.wal` on the FIRST guard drop, and `power_loss_durable_set` then copies that file **whole**. Once the WAL is in the set, any later bytes in it ride along, `fsync`'d or not. The warm-up write put the WAL in the set; the measured write inherited it for free; a full revert to the pre-D-22 posture came back green.

The fix is to make the measured write the **only** write on the handle — which is also the production shape, since a CLI one-shot opens a `Handle`, makes its money write and exits. The `InvoiceSubmissionAttempt` precondition row is seeded through a plain connection and CHECKPOINTed before the handle exists, so the durability journal starts empty and the durable set starts at exactly `{DB, mirror}`. `d22_the_durable_set_is_empty_until_the_money_path_fsyncs` pins that baseline so the vacuity cannot come back silently.

Under M3 the read-back then reports the loss verbatim:

```text
D-22 / ADR-0110 R1 VIOLATED on the mark-abandoned money path …
Durable set: [("aberp.duckdb", 4468736), ("aberp.duckdb.audit.log", 2306)]
```

No WAL. The abandonment is gone from the DB and the mirror still carries it — mirror-ahead, the direction Defense's boot auto-heal resurrects from.

### 6b. Two results that are not what a reader would assume

* **M1 leaves the bytes durable.** The `fsync` happens in `WriteGuard::drop`, not in `durable_ack` — ADR-0110's B2 reorder put it there. Deleting the ack costs the operator the FAILURE REPORT, not the data. A power-loss spec structurally cannot see that; the fault-injection test (unlink the DB path so the flush's fresh `File::open` sees `ENOENT`, then require an `Err` back from the money path) is what does.
* **M2 keeps the row in the durable set**, because `durable_ack` on a handle with no parked outcome falls through to a direct `fsync_data_paths`. What M2 loses is **coherence**: the row went to a second DuckDB instance, so the shared handle's own `verify_chain` reports a short chain and the power-loss test catches it there. Durability and single-instance coherence are two guarantees; M2 is the mutation that separates them.

### 6c. The NAV-gated siblings

The other ten commands cannot be driven unattended — each needs the OS keychain and a live NAV wire call *between* its writes. They carry the identical `{ db.write() … } db.durable_ack()?` pair on the same `Handle`, and their coverage is the static census gate. That is the same division of labour ADR-0110 §"CHECK D3-A/B/C" already documents for modification, storno and the AP status change: "This gate covers all five, statically, in seconds, with no toolchain."

---

## 7. Honest residuals

1. **The census counts a file's sites; it still cannot tell them APART.** *Half of this was closed in review.* D3-A used to check `n >= 1` per file, which — now that five files carry three or four lines each — let an ack be **moved between two censused files** with both the per-file `>= 1` and D3-B's whole-tree total left intact. D3-A is now a **per-file equality** (`n == the number of census lines naming that file`), and probe P11 plants exactly that relocation and asserts the gate goes red via D3-A while D3-B still reports `census closed`. Since the twelve NAV-gated sites have no test cover at all, that per-file count is their only cover against a relocated ack.

   What remains open is a genuine *intra-file* swap: deleting one of `retry_submission.rs`'s four acks and adding a fourth somewhere else **in the same file** still passes, because the count is unchanged and nothing anchors a line to its function. Closing that means reading the census's third column (`<path>\t<function>\t<told>` — the format already carries it) and matching each call site to its enclosing `fn`. Not done here; the shape is a per-file count, not a per-boundary one.
2. **The two `aberp-snapshot::recover` sites** — §5.
3. **Cross-process races are not merely unchanged — for eight commands the exposure got WIDER, and that is a deliberate trade.** A CLI subcommand run while `aberp serve` holds the DB is outside every in-process lock (the standing S335 §3.4 limitation), and D-22 does not fix that; the neighbouring fix is D-21 R1's whole-DB advisory lock. But D-22 does change the *shape* of the race for the eight commands it moved off a raw `Connection`. A `Handle` opened with `open_default` has `checkpoint_enabled: true`, and `CheckpointDebouncer::new` starts with `last_checkpoint: None` — documented as "the very first post-write checkpoint is never blocked by the interval". A CLI one-shot therefore runs a **validated durable checkpoint on its first money write**, and that checkpoint commits by `atomic_install`: a new inode renamed over the live DB path. Pre-D-22 these eight commands wrote through a plain connection and could not do that.

   So: run one of them while `aberp serve` is up, and serve's shared connection is left on the swapped-away inode. ADR-0111's fence catches this **in-process** — it is read under the writer lock and re-armed by `Handle::checkpoint_now` — but a swap performed by a *different process* is invisible to serve until serve's own next ack or write trips the fence, at which point it reports `LIVE DB FILE SWAPPED under the open handle`. That is a loud failure, not a silent loss, and it is the same exposure the three already-Handle-routed commands (`submit_invoice`, `poll_ack`, `drain_pending_retries`) have carried since ADR-0098 C2. It is accepted on the same grounds: concurrent `serve` + money-CLI is already outside the supported single-writer discipline, and the alternative — leaving eight NAV filing paths writing through an un-`fsync`ed second instance — is the 2026-08-08 loss shape. Recorded rather than folded into "unchanged", because the sentence this item used to open with was not true.
4. **One extra `F_FULLFSYNC` per additional ack.** A command that acks three times pays three flushes instead of none. On the money paths that is the intended trade; `retry_submission::perform_layer_2_check` acks TX0 once and its two early-return arms deliberately do NOT re-claim, to avoid a second redundant flush on the same committed row.
5. **`Ledger::from_connection` does not `ensure_schema`; `Ledger::open` did.** Every pre-write read that used to open a `Ledger` by path implicitly created the `audit_ledger` table if it was missing; the shared read clone does not. On a tenant DB with **no** audit table these commands now fail with a SQL error instead of walking an empty chain to a clean `NeverSubmitted` refusal. That state is unreachable for all eleven — each requires a prior issuance/submission chain to have anything to act on — and it is the posture the three already-migrated files (`submit_invoice`, `poll_ack`, `drain_pending_retries`) have carried since ADR-0098 C2, because `Handle::open` does not ensure the audit schema either. Matched deliberately rather than diverged from; flagged because it is a real, if unreachable, difference from the pre-D-22 behaviour.
6. **OPEN — `submit-annulment` has no Attempt-before-call row, and no annulment recovery exists** (adversarial round-2 finding **D-1**; the label is that review's finding id, *not* a `docs/BACKLOG-designed-to-live.md` id — `D-01` there is denied-party screening. Promoting this into that file's D-NN namespace is a separate call, since its rows are 1:1 with the README's "Designed — awaiting hardware/endpoint" table). Every `manageInvoice` path writes a durable Attempt row before it touches the wire, precisely so a mid-flight power loss leaves a local trace of a submission NAV may be holding. `submit_annulment::run` does not: its first and only write is `write_annulment_submission_audit_entries`, which appends `InvoiceAnnulmentSubmissionAttempt` **and** the response entry in one transaction *after* `call_nav` returns. A crash in the window between NAV accepting the `manageAnnulment` call and that transaction committing therefore loses the entire record — and unlike the invoice side there is no way back: `recover-from-nav` reconstructs a chain from `queryInvoiceData` and has no annulment counterpart, so nothing can re-derive the lost row from NAV. The annulment is filed at NAV and absent locally, permanently.

   D-22 does not change this and deliberately does not pretend to: it makes the *existing* write durable at the boundary it has. Closing it is a two-part change — split the Attempt row into its own pre-call transaction with its own `db.durable_ack()?` (the `submit_invoice` TX1 shape, plus a census line), and add an annulment `recover-from-nav` arm (a `queryTransactionStatus` on the annulment transaction id). Filed as a backlog item, not done here, because both halves are NAV-protocol work outside D-22's stated scope. §3's "must be durable BEFORE the wire send" is the rule this path does not yet satisfy.

7. **OPEN — the ack's ORDERING relative to the wire send is enforced by code review only** (adversarial round-2 finding **D-3**; same caveat about the id namespace as §7.6). CHECK D3-A/B/C hold that every censused money path *has* an ack, that the set is closed both ways, and that the failure propagates. None of them knows where the ack sits. Mutation **M6** — take a correct `db.durable_ack()?` and move it to *after* the NAV call it was supposed to precede — passes the gate green: the call is present, censused, and propagating. Ordering is exactly the property §3 rests on ("an Attempt-before-call row must be durable BEFORE the wire send"), and it is the one property the static gate cannot see. A check that could see it would need the census's third column plus the position of the `call_nav`/`operations::` await within the same function — the same anchoring §7.1 wants. Until then, ordering is a review obligation, and this ADR is where a reviewer is told so.

8. **The mutation teeth are uneven across the three tests, and two of them are weaker than they look.** §6a and §6b give the mechanics; the residual is worth stating flatly. **M1** (the ack downgraded to a `warn!`) is killed by the fault-injection test *alone* — the power-loss spec structurally cannot see it, because ADR-0110 B2 put the `fsync` in `WriteGuard::drop` and M1 only costs the operator the failure report. **M3** (the full pre-D-22 revert) was **green** under the first, obvious test design, and only bites because the measured write is now the handle's *only* write; `d22_the_durable_set_is_empty_until_the_money_path_fsyncs` is the pin that keeps that vacuity from returning silently, and it is load-bearing rather than decorative. And all three mutations were run against `mark_abandoned` — the one D-22 path an unattended test can drive. The other ten carry the identical pair by inspection and by the static census (§6c); no mutation has been run against them.

9. **A power-loss test is vacuous per-site by construction.** Following from §8: deleting `durable_ack` at any *individual* site is invisible to a power-loss spec, since the `fsync` happens in the guard drop. Only fault injection distinguishes "the bytes are durable" from "the operator was told the truth about whether they are". Any future D-22-shaped work that adds an ack boundary and proves it with a power-loss test alone has proved the wrong half.
