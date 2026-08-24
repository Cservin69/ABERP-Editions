# ADR-0099 — Editions in-process audit-opener consolidation onto the shared Handle, and the corrected write-fork gate

- **Status:** Accepted, **amended by R2 (2026-08-24)** — see §R2 at the end. A fifth incident (seq **2508**) was initially read as a recurrence of this ADR's fork class. It was not: it was a **lost DB commit** that the mirror reconciler then masked. R2 records the real mechanism with evidence, the fix, and what the fork gate does and does not cover. Original status: The in-process **write-fork** surface for the always-on daemon racer + the serve request handlers is migrated onto the one shared `aberp_db::Handle`; the gate's fork model is corrected (CHECK 10L → CHECK 10M); the remaining in-process residual (`process_digest`, the DÁP heartbeat) and the separate-process CLI writers are **frozen may-only-shrink** and tracked for **v0.2.9**. This ADR does not cut a release (v0.2.8 is cut by Ervin).
- **Date:** 2026-07-06
- **Deciders:** Ervin
- **Extends:** ADR-0098 (the shared `aberp_db::Handle` — one process-wide DuckDB instance; this ADR routes the audit-opener seams ADR-0098 C2/R7 did not reach onto it, and **corrects the gate's fork model**), ADR-0095/0082 (crash-safe durability substrate), ADR-0008/0030 (the tamper-evident audit hash-chain ledger + its lockstep JSONL mirror).
- **Grounds / related:** `duckdb/duckdb#23046` (torn-write / ART family), the recurring **audit-ledger seq fork** (seq **369 → 416 → 428 → 515**), `[[trust-code-not-operator]]`, `[[hulye-biztos]]`.
- **Scope guard:** Authored in the **ABERP-Editions** tree (`Cservin69/ABERP-Editions`), branched off `main = 1a56872`. Frozen prod (`Cservin69/ABERP`) is **never** touched. `CLAUDE.md` is not present at the editions root (only `SAW-OFF.md`/`FOUNDATION.md`/`README.md`); house rules sourced from those + the guard tokens.

## Context — four forks, four different stray openers, one primitive

The audit-ledger `seq` has forked **four times** (seq 369 → 416 → 428 → 515), each time a **different** stray non-Handle opener, each "fixed" one-at-a-time. The one-at-a-time approach does not converge because every fix targeted the *specific* opener, not the *class*.

**Confirmed root cause of seq-515 (read-only forensic on the live Defense DB).** Two INDEPENDENT openers each self-assigned seq 515 off head 514: the **snapshot daemon** (`apps/aberp/src/snapshot.rs::open_ledger` → `Ledger::open` on the live DB, emitting `snapshot.created`) racing the **quote-intake daemon**. Both are the **same `serve` process**, both bypassed the shared `aberp_db::Handle`. Neither ran a rogue `sync_mirror`: `snapshot.rs` appends through `Ledger`, whose mirror write is the sanctioned `WriteGuard` drop, not a raw `sync_mirror`.

**Why v0.2.7's CHECK 10L missed it.** 10L froze only the narrow *"independent opener **+ a rogue `sync_mirror`** in the same runtime fn"* class. `snapshot.rs` has no rogue `sync_mirror`, so 10L never saw it. And CHECK 10i merely **froze the count** of such openers — a frozen fork is still a fork. **The true fork primitive is broader and simpler: ANY independent `Ledger::open(...)`/`Connection::open` + append on the live DB, inside the `serve` process, outside the shared Handle.** A rogue `sync_mirror` is not required. 10L's fork model is too narrow; this ADR replaces it with the correct one (CHECK 10M).

## Decision

### 1 — Route the in-process audit **write** seams onto the one shared Handle

Every migrated seam now appends through `st.db.write()` → `ensure_schema` → `conn.transaction()` → `append_in_tx` → `commit`; the `WriteGuard` drop runs the lockstep `sync_mirror`, so no separate opener and no separate `sync_mirror` remain. Migrated this session:

- **The seq-515 racer — the snapshot daemon (`snapshot.rs`).** `open_ledger` is replaced by a `SnapshotAudit` sink: `Handle(&HandleArc)` for the **in-process** callers (the periodic daemon `run_supervised` **and** the operator-UI HTTP `snapshot now`/`restore` endpoints in `serve.rs`), `Reopen` for the **separate-process CLI** (`aberp snapshot now/restore` — no Handle in that process). `take_and_emit` / `retention_and_emit` / `restore_and_emit` / `run_cycle` thread the sink; `SnapshotDaemonDeps` carries the `Handle`. The sole surviving `Ledger::open` is `emit_reopen_cli` (the CLI reopen — a different process, cannot fork the serve writer).
- **The serve.rs request handlers (priority 2):** `emit_invoice_local_only`, the work-order gates (`enforce_heat_lot_gate_for_start`, `enforce_part_uid_gate_for_shipment`, `enforce_open_ncr_gate_for_shipment`), `handle_material_traceability`, `handle_part_traceability`, `record_restore_from_nav_run_audit`, `record_first_prod_launch_audit`, `record_numbering_change_audit`. Also `set_restored_partner_request`: its post-commit `Ledger::open + sync_mirror` (a redundant **second** opener) is deleted — the `WriteGuard` drop already syncs the mirror.

Read-only independent openers (`verify_chain`/`entries`/`recent`, e.g. `list_invoices`, `handle_quote_intake_notifications`) are **not** seq-fork primitives (they never append a seq) and are out of scope for this gate; they are a lower-severity read-coherence cleanup tracked for v0.2.9 (they already have a coherent `db.read()` alternative).

### 2 — Correct the gate's fork model (CHECK 10L → CHECK 10M)

`tools/cut_gate_db_isolation.sh` **CHECK 10M** (new; 10L retained) enforces the **true** primitive via `tools/adr0099_write_fork_scan.awk` (comment/string/`cfg(test)`-aware): a runtime fn that contains an **independent opener** (`Connection::open`/`Ledger::open`/`DuckDbBillingStore::open`/`append_reopen`) **and** an **append** (`.append`/`append_in_tx`/`append_reopen`) is a write-fork.

- **10M-a (targeted, ZERO):** the migrated in-process seams — `serve.rs` request handlers and the `snapshot.rs` daemon+HTTP path — must contain **zero** write-fork (allow-list: pre-serve boot `run`/`seed_demo_sample_data`/`record_upgrade_snapshot_mismatch_audit`; the CLI `emit_reopen_cli`). Any regrowth is a **RED build**.
- **10M-b (freeze, may-only-shrink):** the remaining write-fork set is frozen in `tools/adr0099_write_fork_residuals.txt`; a NEW/REGROWN site fails the build. This drives the surface to zero without silently tolerating growth (the same discipline as 10i/10k/10L-b).

**Teeth** (`tools/cut_gate_negative_probes.sh` "[CHECK 10M]"): replanting a raw `Ledger::open+append` in the snapshot path → RED; in a serve handler → RED; a brand-new write-fork file → RED; the same inside `#[cfg(test)]` → correctly ignored.

### 3 — Regression test

`crates/aberp-db/tests/adr0099_snapshot_quote_intake_seq_fork.rs` (real DuckDB, runs on the Mac/CI gate):
- **RED half** — two independent openers (the snapshot daemon + quote-intake) read the same head and both append the next seq → a **duplicate seq** (the fork reproduced deterministically; `UNIQUE(seq)` is gone per duckdb#23046, so nothing stops it).
- **GREEN half** — the same interleaved burst routed through one `Handle` → a **dense, fork-free** chain and **DB == mirror**.

## Deferred (frozen may-only-shrink, tracked for v0.2.9)

These are **honestly not done** this session and are held by CHECK 10M-b so they cannot grow:

- **`restore_from_nav_outgoing.rs::process_digest`** — the nav restore/backfill daemon writes the `restored_invoice` row + `InvoiceRestoredFromNav` audit through its own `Connection::open`. Migrating it needs the `Handle` threaded through its `Ctx` struct + constructors (a deeper change). **In-process residual — MUST reach zero.**
- **The DÁP heartbeat** (`serve.rs::spawn_dap_audit_chain` / `audit_dap_boot.rs::run_heartbeat_supervised`) — opt-in (`dap_enabled` default false). Its `heartbeat()` takes qualified-timestamp anchors + signed appends via a `&mut Ledger`; routing it through the Handle's `WriteGuard` needs a `Ledger`-over-shared-writer adapter (a larger design). The scanner does not flag it (its opener and append are cross-fn), so it is listed here explicitly, not in the manifest.
- **Separate-process CLI one-shots** (`avl_vendors`, `email_invoice`, `material_inventory`, `mes_manager`, `part_marking`, `purchasing`, `quality`, `quote_calibration`, `quoting_machines`, `tenant_registry`, and the `run()` subcommands `drain_*`/`retry_*`/`*_annulment`/etc.) — a **different process** from `serve`, so they cannot share the in-process Handle. Some of their append fns are *also* reachable from serve routes; a full fix needs the `SnapshotAudit`-style dual sink (Handle in-process, reopen in the CLI) **and** a whole-DB cross-process advisory lock (fs2 flock, pattern `submission_lock.rs`) so a CLI refuses while `serve` holds the DB. Flagged as a tracked v0.2.9 follow-up (the flock is non-trivial; forcing it here would block the in-process sweep).

## Consequences

- The **specific recurring seq-515 race** (snapshot daemon vs quote-intake) is closed: both are on the shared serialized writer. The high-frequency serve request-handler write-forks are closed. The gate now fails on the **true** fork primitive, not the narrow 10L subset, and cannot silently grow.
- This is a **partial** in-process consolidation, not the complete zero-opener end state. CHECK 10M-b holds the deferred surface at may-only-shrink; v0.2.9 must migrate `process_digest` + the DÁP heartbeat to reach in-process zero and add the cross-process CLI flock.
- Single-writer throughput ceiling (already accepted in ADR-0098) now also covers the migrated snapshot/HTTP/request-handler audit appends — acceptable for a single-operator CNC-shop ERP.


---
# R2 (2026-08-24) — the fifth recurrence was a LOST DB COMMIT, and the reconciler masked it

- **Status:** Accepted (implemented). Branch `fix/audit-fork-class-0099` off `main = 6182c6e` (the v0.6.0 tip). No merge, no cut — this becomes **v0.6.1** when Ervin lands it.
- **Trigger:** a Defense **prod boot refusal**, ~2026-08-22.

## R2.1 — What the evidence actually shows

Four consecutive 60-second intake-poll heartbeats. Two (19:19:44, 19:20:44) were durable in the `<db>.audit.log` **mirror** and **absent from the DB**; the next two (19:21:44, 19:22:44) sat in the **DB at the same seqs**, 2508/2509.

**That is not two writers racing.** A concurrent-writer duplicate puts the *same* entry — same `entry_hash`, same timestamp — twice in *one* store. Two *different* entries at the same seq, split across the two stores, is the DB losing already-committed appends: the chain head fell back to 2507, so the later pair legitimately took the freed seqs. **The seq re-use is the consequence of the loss, not evidence of a fork.** The intake daemon writes through `db.write()` + `append_in_tx` and is a correct shared-Handle writer throughout; it was the victim, not the second writer.

An earlier pass of this ADR blamed a TOCTOU duplicate in the mirror reconciler. That defect is real and is fixed here, but it is **not** what happened: it would have produced the same entry twice in the mirror. The correction is recorded rather than quietly dropped, because the wrong diagnosis is itself part of the story (§R2.3).

## R2.2 — Mechanism: why the DB lost a committed append

Pre-ADR-0110-D3, `WriteGuard::drop` was:

```rust
sync_mirror(conn, …)     // mirror.rs ends in flush() + sync_all() → DURABLE
debouncer.note_write();  // the DB's only durability is the DEBOUNCED checkpoint
```

The **mirror** is `fsync`ed on every commit; the **DB** is not flushed at all. The ordering invariant D3 later introduced — *the data must be durable before the record that points to it* — is exactly inverted, so an unclean stop loses the WAL tail while the mirror keeps the rows. Two consecutive heartbeats is precisely the size of such a tail.

Verified against the release branches with `git merge-base --is-ancestor`:

| | releases |
|---|---|
| **no** D3 unconditional `fsync_data_paths` (`a28df91`) | v0.1.0 … **v0.3.0** |
| has D3 | v0.4.0 … v0.6.0 |
| **no** ADR-0111 inode fence (`09d7273`) | v0.1.0 … **v0.4.0** |
| has ADR-0111 | v0.6.0 |

v0.2.11's checkpoint is path-based (`live_durable_checkpoint(&self.db_path, …)`) with no fence, so a checkpoint taken outside the Handle also strands the shared connection on an unlinked inode — commits land in the orphan while the lockstep `sync_mirror`, reading that same connection, durably mirrors them. Either sub-mechanism yields the same signature.

**Is it preventable, and is it already prevented?** Both, on this tree — and the claim is measured, not asserted. `daemon_heartbeats_are_power_loss_durable` drives six real heartbeats, copies only the files the write path *certified* durable (its `fsynced_paths` journal plus the two pre-D3 constants), boots that copy, and demands all six back. It passes on `main`, and the WAL is in the journal. Restore the pre-D3 drop and it fails with the incident's exact shape: **mirror 6, DB 1.**

So: **on the serve/daemon path**, the lost commit is a pre-existing residual of the DEPLOYED binary, not a live code gap. The fix for the loss itself already shipped in v0.4.0 (ADR-0110 D3 `durable_ack` / ADR-0111's inode fence); what was missing is that it is *not deployed* — a deployment gap, which this ADR cannot close.

**That qualifier is load-bearing and the unqualified sentence was wrong.** The CLI **money**-submission paths on THIS tree still carry the pre-D3 inversion: `retry_submission.rs`, `drain_submission_queue.rs` and the rest of the D-22 sites do `Ledger::open` → `append` → `sync_mirror` with no `durable_ack`, no `fsync_data_paths`, and `PRAGMA disable_checkpoint_on_shutdown` set — the mirror explicitly `fsync`ed, the DB explicitly not folded. That is a **live code gap on NAV money paths**, not a deployment gap, and it is live at `main` right now. It is out of scope here only because folding a money-path change into a daemon-heartbeat fix would have made both unreviewable; it is tracked as **[D-22](../docs/BACKLOG-designed-to-live.md#d-22)** with the sites enumerated, the target shape named (`submit_invoice`), and the census extension specified — an elevated fix awaiting scheduling, not a backlog shrug.

One live hardening is added anyway. ADR-0110's existing power-loss spec drives a **money** path, so a change that made the flush conditional on money paths would leave it green while daemon heartbeats silently lost durability again. The new test pins the **daemon** path specifically.

## R2.3 — Why nobody saw it: the reconciler masked the diagnosis

Measured on the current tree, one branch at a time:

| state | verdict before R2 |
|---|---|
| **A** — right after the loss: mirror 1..5, DB 1..3 | `MirrorAheadOfDb{5,3}` ✅ correct |
| **B** — seqs re-used, counts equal, interiors differ | `MirrorCorruptPreserved{"…equal length…"}` ✅ caught, ❌ mislabelled |
| **C** — the DB has moved past the mirror | `Extended{entries_added:2}` ❌ **silently grafts**, then `"hash-chain break at seq 6"` |

The masking step is **`Extended`**. It appended DB rows after `mirror_max_seq` **without ever comparing the shared prefix**, which:

* destroyed the length asymmetry `MirrorAheadOfDb` (state A) keys on;
* destroyed the head-hash equality the equal-length branch (state B) keys on;
* left the mirror's own `prev_hash` link check to fail one seq later, surfacing as **"corrupt mirror"**.

So the counts did not "catch up" on their own — the reconciler **made** them equal. And the operator was pointed at the wrong subsystem: a lost DB commit reported as mirror corruption. That misdirection is why the incident read as a fork. `sync_mirror` has always made exactly this comparison before appending (its step 4); the reconciler simply did not, and the two paths disagreed.

There are exactly two routes into the masked state: `Extended` grafting (automatic, every boot and every pre-snapshot), and a manual mirror rebuild.

## R2.4 — The undocumented 2026-08-20 rebuild

`aberp.duckdb.audit.log.PRE-MIRROR-REBUILD-20260820T070244Z` is **not written by any code in this tree**. The code's own preserved copies are `.corrupt-<nanos>.bak` and `.ahead-<nanos>.bak` — nanos, not ISO-8601Z. The single `%Y%m%dT%H%M%S` in the repo is an *operator instruction string* that `upgrade_snapshot.rs` prints for a hand-run `mv`. It was a manual rebuild in house style, and the missing `_RECOVERY-*.md` is consistent with that.

It matters mechanically: moving the mirror aside makes the next boot take the `Created` branch and rebuild from the DB — **silently discarding the only surviving record of whatever the DB had already lost, and resetting the mirror to the DB's lossy state.** If that procedure is in a runbook it needs a warning; if it is not, it was improvised. Either way it is the one manoeuvre that turns this class fully invisible, and nothing in the code can stop it.

## R2.5 — Fix

**Detection — prove the shared prefix, then act.** `ensure_consistent_with_db` now verifies the mirror's prefix against the DB before `Extended` appends anything, and reports interior divergence as its own class.

The check is **O(1) on the happy path by construction**: both stores are hash chains, so the mirror's head `entry_hash` commits to its entire prefix and the DB's row at that seq commits to its own. Neither a full scan nor a binary search is needed. The full scan runs *only* once a refusal is certain, to name the earliest divergent seq for the operator.

**Round 4 correction — one row read is not enough, and "proves the whole prefix" was false as written.** A hash chain commits to its *history*, not to its own continued existence in the table. `DELETE FROM audit_ledger WHERE seq = 3` rewrites nothing — every surviving row's `entry_hash` is untouched, head included — so an interior HOLE passed the head compare and reconciled to `Unchanged`. Measured on a 5-entry ledger with seq 3 deleted: `Ok(Unchanged)`, while the mirror still held the row. That is a lost committed audit entry reported as healthy: precisely the class this ADR exists for, slipping through the check written to catch it. The proof is now head hash **AND** cardinality (`COUNT(*) over [1..=head.seq] == head.seq`) — still O(1), two reads instead of one. `an_interior_row_the_db_lost_is_not_reported_as_agreement` pins it.

**`AppendError::MirrorDivergedFromDb { first_divergent_seq, mirror_max_seq, db_max_seq, preserved }`** is a distinct variant, and the distinction from `MirrorAheadOfDb` is load-bearing for recovery:

* **AHEAD** — the DB lost a *tail*; the mirror-only entries are the only copy, so recovery replays them into the DB and the DB catches up.
* **DIVERGED** — the DB lost entries *and re-used their seqs*, so both stores hold a row there. It must never be resolved by rebuilding the mirror from the DB.

Reusing `MirrorAheadOfDb` for the diverged case was rejected: the seq numbers genuinely are not ahead, only the content is, and lying about that in the operator message is how this got misdiagnosed the first time.

~~**Boot routes it to the sanctioned recovery path**, not a bespoke one — `attempt_db_auto_recovery(…, "mirror_diverged_from_db")` … `replay_mirror_delta` already refuses with `SequenceConflict` …~~

**WITHDRAWN — the safety it rested on does not exist, and the route silently discarded committed audit rows. Superseded by [§R3](#r3-2026-08-24--a-divergence-is-terminal-the-r2-recovery-route-was-lossy).** Struck through rather than deleted, because "the call site does not re-derive the safety, a lower layer already guarantees it" is a specific and repeatable way to be wrong, and the reasoning is worth keeping visible next to what it cost.

**Single-writer routing** (carried from the earlier pass, still correct, now correctly *scoped* as hardening rather than as the root cause):

* `ensure_consistent_with_db` holds the mirror's exclusive `flock` across its whole decide→act window, closing a real TOCTOU in which a lockstep append landing between the head sample and the act was re-appended verbatim. Helpers became `*_locked` variants — `flock` is per-fd, so re-locking would self-deadlock. `rebuild_mirror_from_db_locked` truncates *under* the lock instead of `truncate(true)` before it. Lock order is always handle-mutex → mirror-flock.
* The pre-EXPORT reconcile is hoisted off `take_snapshot`'s export connection onto the shared Handle (`MirrorReconcile` owner + `aberp::snapshot::reconcile_mirror_for`). That connection is a separate DuckDB instance which never replays the shared writer's WAL, so it read `db_max_seq` stale-low and fired spurious `MirrorAheadOfDb`. The residual's rationale — *"READ-ONLY … never writes the live file"* — was true and beside the point: nobody asked whether it was read-only w.r.t. the **mirror**. A residual's rationale must name the invariant it is read-only *against*.
* `serve::sync_audit_mirror_best_effort` (11 call sites) and the stock-movement route's inline copy are deleted. Redundant since ADR-0098, and since D3 actively wrong: when the guard's data flush fails it deliberately SKIPS the mirror sync so the mirror stays *behind* (benign); these put it back *ahead*.

## R2.6 — The gate

CHECK 10M/10N fire on *independent opener AND audit-**table** append*. Four blind spots: `Handle::read()` is not in their opener set though it returns a writable `Connection` (**B1**); a fn appending on a `&Transaction`/`&mut Ledger`/`&mut Connection` **parameter** has no opener of its own (**B2**); they are ban-lists, so an unknown provenance is silently clean (**B3**); and the **mirror** was not in the append set at all, with `aberp-db` and `aberp-snapshot` excluded from the corpus (**B4**). CHECK 10L does cover `opener + .sync_mirror`, but it is a may-only-shrink *freeze*, so the two `serve.rs` sites were **listed and green** — *a frozen fork is still a fork*, the same lesson §2 learned about 10i and did not carry over to the mirror.

**CHECK 10P** inverts the predicate: it fires on the **write** — table or mirror — and every runtime site must *prove* its serialization domain. Unclassifiable is RED, so there is no not-on-the-ban-list escape. `TX_PARAM` is not trusted on faith: the driver iterates the scanner to a fixpoint, so a fn that writes only by handing its connection to a helper is classified by its own provenance. Corpus is the whole workspace. 97 shared-Handle writes; 22 frozen sanctioned non-shared writers.

Honest scope: **10P would not have caught this incident.** It closes the second-writer class; the incident was a durability loss. It is kept because that class is real, recurred four times, and the blind spots are demonstrable — not because it explains seq 2508.

Three scanner properties are load-bearing, each arrived at by getting it wrong first: statement-scoped rather than line-scoped (ADR-0105 F1 found a real opener laundered past a line-scoped exclusion); provenance tracked per binding and propagated only along *connection-shaped* derivations (a `mut`-based heuristic is wrong in both directions, and without the derivation bound `.read()` — which the Handle shares with `RwLock::read()` — reddens the shutdown writer); and taint propagating only through calls that actually **hand over a connection** (unbounded bare-name taint walks into every daemon supervisor, and a gate that cries wolf gets switched off).

## R2.7 — What is pinned, and what a green run does not prove

`crates/aberp-db/tests/adr0099r2_lost_commit_divergence.rs`

| test | pins |
|---|---|
| `daemon_heartbeats_are_power_loss_durable` | the loss is prevented — derivation-based, so deleting the `fsync` drops the WAL from the journal and it goes red |
| `state_a_…_reported_as_ahead_not_as_divergence` | AHEAD stays AHEAD (recovery replays) |
| `state_b_reused_seqs_…_naming_the_seq` | equal counts + differing content ⇒ `MirrorDivergedFromDb{seq:4}` |
| `state_c_extended_refuses_instead_of_grafting` | the masking step, and that the mirror is left byte-identical |
| `the_refusal_is_idempotent_…` | evidence cannot decay across reconciles — the pre-R2 non-idempotence *was* the bug |
| `an_agreeing_mirror_still_extends_and_settles` | the happy path is untouched |

Mutations, each killing exactly its own test and nothing else: pre-R2 `Extended` grafting → state C + idempotence red; equal-length branch reporting generic corruption → state B red; pre-D3 drop → the durability pin red.

`adr0099r2_mirror_reconcile_race.rs` and `adr0099r2_mirror_reconcile_owner.rs` pin the single-writer hardening. Their concurrency test is corroboration only: per `audit_lock_domain_e2e.rs`, a race proves a hazard exists, never that one is gone.

## R2.8 — Honest residuals

- **The deployment gap is the live risk.** Every release ≤ v0.3.0 can lose a committed audit append. This ADR cannot close that; deploying ≥ v0.4.0 does. Nothing here should be read as "prod is now safe".
- **15 CLI `Ledger::open` → `append` → `sync_mirror` sites** (`drain_submission_queue`, `retry_submission`, `submit_annulment`, `poll_annulment_ack`, `observe_receiver_confirmation`, `recover_from_nav`, `mark_abandoned`, `request_technical_annulment`) `sync_all` the mirror with no DB flush at all — the same inversion, cross-process, on NAV money paths. Deliberately **out of scope** here (this incident was a serve-path daemon); tracked as backlog **D-22**.
- **`fsync_data_paths` skips the WAL when `wal_path.exists()` is false** and still returns `Ok`. Not reachable in the measured runs (the journal always carried the WAL), but it is a silent-skip on a durability path and is flagged rather than redesigned.
- **`serve.rs::run` is allow-listed by 10M, 10N and 10P alike** — a fork planted in the boot fn passes all three. Genuinely pre-Handle, but a real hole.
- **`append_in_tx` still takes no lock**, so the two serialization domains remain distinct; `with_ledger` is still the only construct holding both. 10P's `LEDGER_LOCKED` is a *classification*, not a proof. ADR-0105's residual, unchanged.
- ~~**`ensure_consistent_with_db` now blocks on the `flock` with no timeout**, including at boot. Deliberate: a timeout would have to choose between refusing to boot and proceeding unsynchronised, and the second is the failure this ADR exists to remove.~~ **Closed in §R3.5** — a false dilemma. The bound fails *loud* and never proceeds unsynchronised, so the TOCTOU stays closed and a stuck peer can no longer wedge every serve DB write.

# R3 (2026-08-24) — a divergence is TERMINAL; the R2 recovery route was lossy

- **Status:** Accepted (implemented). Same branch `fix/audit-fork-class-0099`, on top of R2. No merge, no cut.
- **Trigger:** the R2 adversarial review.

R2 got the *detection* right and the *handling* wrong, and the handling was a **regression against `main`**. Having correctly named the seq-2508 shape (`MirrorDivergedFromDb`), it routed that shape into `attempt_db_auto_recovery`. Measured on the fork shape: boot logs `Recovered`, **four committed DB audit rows cease to exist**, and serve continues on a WARN. On `main` the same shape returned `MirrorCorruptPreserved`, boot was fatal, and a human looked at it. Making the diagnosis precise must not make the handling lossy.

## R3.1 — Why the `SequenceConflict` safety was never real

R2's justification was that the call site need not re-derive safety because `replay_mirror_delta` refuses a colliding replay with `SequenceConflict`. That guard is unreachable **twice over**:

1. **It is never asked to collide.** `replay_mirror_delta(conn, mirror, after_seq)` replays only `seq > after_seq`, and `after_seq` is the imported snapshot's audit head — into a *staging* DB that by construction holds exactly `[1..=after_seq]`. Every replayed seq is above every occupied seq. There is no collision to refuse.
2. **A collision would not be caught anyway.** `audit_ledger` has **no `UNIQUE(seq)`** — S341 dropped that inline ART index (duckdb#23046 / S332), as `AppendError::SequenceConflict`'s own doc comment states. A duplicate-seq INSERT still reports `rows_changed = 1` and returns `Ok`. The variant survives as a defensive row-count catch, not as a uniqueness guarantee.

And the loss underneath is **structural, not incidental**. Recovery rebuilds from a SNAPSHOT and replays the MIRROR's delta. The DB's own entries at the re-used seqs exist in *neither* input, so a "successful" recovery reconciles the two stores by deleting them. No conflict guard at any layer could have changed that: the rows are not overwritten, they are simply never rebuilt.

The general lesson, and the reason §R2.5 is struck through rather than deleted: *"the call site does not re-derive the safety, a lower layer already guarantees it"* is only as good as having read the lower layer. Here the lower layer's own doc comment said the opposite.

## R3.2 — Divergence is terminal

`MirrorDivergedFromDb` now returns the preserve-and-refuse path: boot-fatal, both copies retained (the mirror preserved to `<mirror>.diverged-<nanos>.bak`, the DB untouched).

This is not conservatism, it is arithmetic. Two *different* committed entries claim one seq. Which of them really happened is a question about the business's own history, and the answer is not derivable from either store. An operator reconciles them; code that picks silently is code that deletes audit rows.

The boot routing is now a value — `boot_mirror_route(&AppendError) -> BootMirrorRoute` — extracted out of `serve::run`. The defect R2 shipped was a *routing* defect, and a routing defect buried in a 30k-line `run` is one no test can reach. It is fail-closed: `AutoRecover` is reachable from exactly one variant and every other reconcile failure, known or not, is `RefuseFatal`.

The refusal message names `aberp recover` explicitly **as something not to run**. It is the CLI entry point to the same lossy engine, and an operator staring at a boot refusal that says "recover with `aberp recover`" — which the `MirrorDivergedFromDb` error text itself used to say — would have completed the loss by hand.

## R3.3 — The AHEAD branch had the same hole

R2 proved the shared prefix only on the BEHIND branch. A mirror that was ahead on **count** and *also* disagreed over the shared prefix therefore reported plain `MirrorAheadOfDb` — the one condition boot auto-recovers — and recovery discarded the DB's divergent rows exactly as above.

Divergence is a property of the **shared prefix** `[1..=min(mirror_max, db_max)]`, not of which store is longer, so the proof is hoisted above the length branch and decided once. The prefix *slice* is load-bearing in the other direction: comparing the whole mirror would read an ahead mirror's legitimately DB-absent tail as a divergence at `db_max_seq + 1`, turning every honest lost-tail — and every intentional dev DB-nuke — into a boot-fatal refusal, i.e. removing the one condition this system can safely fix by itself. `state_a_mirror_ahead_is_reported_as_ahead_not_as_divergence` is the pin for that direction.

The equal-length arm is now unreachable (at equal length the shared prefix *is* the whole mirror). It is kept as a fail-closed backstop: if the head hashes ever disagree while the prefix comparison reports agreement, that is two contradictory reads of the same rows, and the one thing we must not do is fall through to `Unchanged`.

The preserved evidence for a divergence now gets **its own infix**, `<mirror>.diverged-<nanos>.bak`. R2 reused `.ahead-`, and in the behind- and equal-length shapes the seq numbers are precisely *not* ahead — mislabelling that is how the incident got misdiagnosed in the first place (§R2.1). `.corrupt-` is worse: it points the operator at the mirror, when the mirror is the intact party and the DB is the one that lost entries.

**The in-crate `…_when_mirror_ahead_of_db` fixture was itself a divergence, and nothing had noticed.** It built the mirror from one DB and then compared it against a *separate* DB holding its own seq-1 entry, so the two disagreed at seq 1 — a shape CI has been running by name (`ci.yml`) as the canonical clean-AHEAD case. R2's BEHIND-only proof never looked at it. The fixture now loses a genuine tail (`DELETE FROM audit_ledger WHERE seq >= 2` on the same DB), and the diverged variant is pinned separately as `ensure_consistent_reports_an_ahead_but_diverged_mirror_as_divergence`.

`RecoveryAction::Rebuilt` is **deleted**, not re-documented. It had no producer, and both remaining rebuild call sites return `Created`; the fn's doc decision tree still promised a "full rebuild from the DB" on equal-length divergence — the exact manoeuvre §R2.4 identifies as the one that makes this incident class invisible. A variant no code can produce is an invitation to write the arm back.

## R3.4 — The evidence copy was destroying evidence

`preserve_corrupt_db` copied only `<db>.duckdb`. Every `Handle` commit is **WAL-only** — the main file's bytes do not change until a checkpoint (ADR-0098 R5, measured on duckdb 1.5.3) — so a live DB's most recent committed rows can live *entirely* in `<db>.wal`. Step 4 of `atomic_install` then deliberately unlinks the target's stale `.wal`.

So on the `aberp recover` CLI path the sequence was: copy the main file (missing the recent rows), rebuild, rename over the live path, unlink the live WAL. The rows were **destroyed**, and the operator was told they had been preserved. A preserve step that loses evidence is worse than none.

The WAL is now copied to `<dest>.wal` before anything unlinks anything, so the retained pair opens as a real database and shows the rows as committed. A missing WAL is normal (a freshly checkpointed DB has none) and is not an error; a WAL that exists and cannot be copied *is* one.

## R3.5 — The cross-process lock is bounded

`aberp::snapshot::reconcile_mirror_for` holds `aberp_db`'s single writer mutex across `ensure_consistent_with_db`, which blocks on a **cross-process** `flock`. Untimed, any stuck peer — a hung `aberp` CLI, a crashed-but-not-reaped process still owning the fd — froze *every DB write in the serve process*, indefinitely, with no diagnostic.

§R2.8 defended the untimed wait as a choice between refusing to boot and proceeding unsynchronised. That is a false dilemma: the bound **fails loud** (`AppendError::MirrorLockTimeout`, naming the path and telling the operator to `lsof` it) and never proceeds unsynchronised, so R2's TOCTOU stays closed. `fs2` exposes no timed acquire, so the bound is a `try_lock` spin at 50 ms — ~200 syscalls over a 10 s wait.

**Round 4 — the first pass bounded the wrong lock, and this section asserted a result that was empirically false.** `ensure_consistent_with_db` was bounded; `sync_mirror` was not. That is the one on the **per-commit hot path**, called from `WriteGuard::drop` *while the writer mutex is still held*, so the wedge this section claimed to have removed was fully intact. The adversarial measured it: one ordinary `Handle::write()` + guard drop took **30.03 s** against a peer holding the mirror lock for 30 s. Bounding only the reconciler also inverted the asymmetry — under contention the `sync_mirror` holder always won and the *booting* process took the fatal timeout.

Both takers now go through one `lock_exclusive_bounded` helper, with **different budgets, chosen by consequence rather than by caller**:

| taker | budget | why |
|---|---|---|
| `ensure_consistent_with_db` | 10 s | runs at boot / pre-snapshot; failure is FATAL, so waiting is worth it |
| `sync_mirror` (lockstep) | 2 s | runs on every commit; failure is BENIGN — the caller logs and continues, the mirror stays BEHIND, and the next write or the pre-snapshot reconcile catches it up. A behind mirror is the safe direction (ADR-0110 D3; the dangerous one is AHEAD) |

Not zero-wait on the hot path: brief contention with the in-process reconciler is routine and a bare `try_lock` would skip spuriously. `a_stuck_peer_cannot_wedge_the_per_commit_write_path` pins it; restoring the untimed `lock_exclusive()` makes a single commit take the peer's full lifetime and the test fails saying so.

## R3.6 — What is pinned

`crates/aberp-db/tests/adr0099r3_diverged_is_terminal.rs`, plus two unit tests in `serve.rs` and two in `aberp-snapshot::recover`.

| test | pins | mutation that kills it (and nothing else) |
|---|---|---|
| `serve::only_a_clean_mirror_ahead_is_routed_to_auto_recovery` | the routing: AHEAD → recover, everything else → refuse | route `MirrorDivergedFromDb` back to `AutoRecover` (the R2 behaviour) |
| `serve::the_divergence_refusal_warns_against_the_lossy_recovery_cli` | the message says *not* to run `aberp recover` | — |
| `the_fork_shape_refuses_and_preserves_without_touching_either_store` | the 2510 fork shape refuses; mirror byte-identical, every DB row in place | — |
| `recovery_on_a_diverged_mirror_discards_the_dbs_own_rows` | **the measurement**: the real engine reports `Recovered` while seqs 4,5,6,7 vanish | — **not a regression pin**: it calls the engine directly and passes with the routing fix reverted. Documentation, not coverage |
| `an_ahead_but_diverged_mirror_refuses_instead_of_reporting_a_recoverable_ahead` | R3.3 | prove the prefix only when `mirror_max < db_max` |
| `a_clean_ahead_mirror_still_recovers_with_zero_row_loss` | a clean AHEAD stays AHEAD **and** recovers losing nothing | drop the prefix *slice* (also reddens R2's `state_a`) |
| `a_stuck_peer_cannot_wedge_the_reconciler_forever` | R3.5 | restore `lock_exclusive()`; the peer holds for 25 s so the mutation fails the assertion rather than hanging the suite |
| `recover::preserve_corrupt_db_copies_the_wal_so_the_evidence_survives_atomic_install` | R3.4, through a real `atomic_install` | skip the WAL copy |
| `recover::preserve_corrupt_db_without_a_wal_is_not_an_error` | the no-WAL case is not fabricated into a failure | — |
| `a_stuck_peer_cannot_wedge_the_per_commit_write_path` | **round 4**: a commit completes while a peer holds the mirror lock | restore `sync_mirror`'s untimed `lock_exclusive()` — the commit takes the peer's full lifetime |
| `an_interior_row_the_db_lost_is_not_reported_as_agreement` | **round 4**: a hole behind the head is a divergence, not agreement | drop the cardinality half of the prefix proof |
| `repeated_refusals_do_not_grow_the_evidence_without_bound` | **round 4**: four refusals on an unchanged mirror ⇒ ONE evidence copy | make the preserve copy unconditional again |

`only_a_clean_mirror_ahead_is_routed_to_auto_recovery` pins the **classifier**, not the boot path: re-inlining R2's routing into `run`'s `match` would leave it green. Extracting the decision is what made it testable at all — it lived inside a 30k-line fn — but the extraction is a convention `run` must keep honouring, and nothing enforces that. Listed in §R3.7.

`recovery_on_a_diverged_mirror_discards_the_dbs_own_rows` is a **characterisation** test of the recovery engine, not a specification of it. Dropping rows that are in neither the snapshot nor the mirror is inherent to what a snapshot+mirror rebuild *is* — which is precisely why nothing may route a divergence into it. If a later change teaches the engine to detect divergence and refuse, that test goes red: delete it and pin the stronger property (recovery never returns `Recovered` while losing a row) in its place.

## R3.7 — Honest residuals

- **The recovery engine itself is still willing to run on a diverged pair.** Boot classifies fail-closed and the refusal message warns off the CLI, but `aberp recover --db … --tenant …` typed by hand on a diverged pair will still rebuild and drop rows. (An earlier draft of this bullet said R3 "closes every route into it". It does not — see the next two bullets. The categorical phrasing was exactly the kind of unverified summary §R3.1 is about.)
- **The `torn_open` arm is a second unclassified route.** `serve.rs`'s torn-DB branch calls `attempt_db_auto_recovery` with no mirror classification at all. It is only reachable when the DB will not open, so a divergence *cannot* be detected there and the loss is inherent to the situation — but it is a route, and it was unlisted.
- **The routing extraction is a convention, not an invariant.** `boot_mirror_route` is pinned as a function; nothing pins that `run` calls it. Making that structural (or gating it) is the real fix.
- **The WAL evidence copy is not atomic against a live writer.** Main file and WAL are two `fs::copy` calls; a checkpoint landing between them yields old-main + folded-WAL. The enclosing operation is already unsafe against a live writer, so this window alone is not worth closing — but the doc no longer claims otherwise.
- **`restore_into` deletes the target WAL with no evidence copy at all.** By §R3.4's own reasoning that destroys every committed row since the last checkpoint. Deliberate — a restore is a requested overwrite — but it is the one other place the §R3.4 sweep did not visit. Defence-in-depth would be a divergence check inside `recover_or_refuse`. Not done here: it belongs with the engine, it needs its own refusal semantics, and R3 is a scoped fix to a regression this branch introduced.
- **The 10 s lock bound is a judgement, not a measurement.** Every legitimate holder is orders of magnitude below it (`sync_mirror` = one append + one `fsync`; the reconciler = one bounded compare), and the worst plausible case — a cross-process `sync_mirror` doing a first-time backfill of a very large ledger — is still JSONL writes plus one `fsync`. If a real holder ever does exceed it, the failure is a loud, actionable, retryable boot refusal naming the path to `lsof`, not a silent or data-losing one. That asymmetry is why the bound is set where it is; if a legitimate >10 s holder is ever observed, raise the constant rather than removing the bound.
- **§R2.8's residuals otherwise stand**, including the one that matters most: the deployment gap. Every release ≤ v0.3.0 can lose a committed audit append, and nothing in this branch changes that.
- **The CLI money-path inversion (D-22) is a live code gap on this tree**, not a deployment gap — see the correction in §R2.2. Out of scope here by scheduling, not by triage.
