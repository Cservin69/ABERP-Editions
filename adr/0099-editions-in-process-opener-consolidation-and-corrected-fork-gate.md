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

So: **the lost commit is a pre-existing residual of the DEPLOYED binary, not a live code gap.** The fix for the loss itself already shipped in v0.4.0; what was missing is that it is *not deployed* — a deployment gap, which this ADR cannot close.

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

The check is **O(1) on the happy path by construction**: both stores are hash chains, so the mirror's head `entry_hash` commits to its entire prefix and the DB's row at that seq commits to its own — one row read proves the whole prefix. Neither a full scan nor a binary search is needed. The full scan runs *only* once a refusal is certain, to name the earliest divergent seq for the operator.

**`AppendError::MirrorDivergedFromDb { first_divergent_seq, mirror_max_seq, db_max_seq, preserved }`** is a distinct variant, and the distinction from `MirrorAheadOfDb` is load-bearing for recovery:

* **AHEAD** — the DB lost a *tail*; the mirror-only entries are the only copy, so recovery replays them into the DB and the DB catches up.
* **DIVERGED** — the DB lost entries *and re-used their seqs*, so both stores hold a row there. It must never be resolved by rebuilding the mirror from the DB.

Reusing `MirrorAheadOfDb` for the diverged case was rejected: the seq numbers genuinely are not ahead, only the content is, and lying about that in the operator message is how this got misdiagnosed the first time.

**Boot routes it to the sanctioned recovery path**, not a bespoke one — `attempt_db_auto_recovery(…, "mirror_diverged_from_db")`, so the `db.auto_recovered` row records which case it was. The safety of that routing is **not re-derived at the call site**: `replay_mirror_delta` already refuses with `SequenceConflict` the instant it is asked to replay a mirror entry onto a seq the staging DB holds, which is exactly the diverged case. A diverged mirror can therefore only reach `Recovered` if the replay was genuinely conflict-free; anything else falls through to preserve-and-surface.

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
- **`ensure_consistent_with_db` now blocks on the `flock` with no timeout**, including at boot. Deliberate: a timeout would have to choose between refusing to boot and proceeding unsynchronised, and the second is the failure this ADR exists to remove.
