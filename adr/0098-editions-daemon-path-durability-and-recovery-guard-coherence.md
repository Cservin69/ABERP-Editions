# ADR-0098 — Editions daemon-path durability & recovery-guard coherence: crash-safe quote/intake daemon writes, and recovery from a snapshot that is ahead of the mirror

- **Status:** Proposed (design pass; conservative calls flagged for Ervin's confirmation/veto — see *Decisions surfaced* D1–D8. Implementation is sequenced in the companion plan `docs/daemon-durability-and-recovery-guard-fix-plan.md`, **not done here**.)
- **Status update — v0.2.5 finalization (2026-07-02):** implemented over Sessions B/C/C2 and finalized with **Option 1 — `Handle::read()` = `try_clone` of the shared instance** (coherent reads, per the 1a design in D1). The Session-C2 F5 separate read-only instance (`read_returns_readonly`) is REMOVED — it caused pervasive post-checkpoint stale reads. Gap-2b (DB-level write-on-read fail-loud) is accepted for v0.2.5 and deferred to v0.2.6. See *§ v0.2.5 finalization* below and `docs/adr-0098-v0.2.6-scope.md`.
- **Live evidence (2026-06-29 17:02, folded in):** the DB recovered at 16:58 (seq 109, validated, booted clean, mirror reconciled `Unchanged`) **re-tore at runtime ~4 minutes later under live daemon load** — a **reproducible** repro, not an intermittent one. This sharpened Gap 1's root cause from "the daemon write path skips the durable checkpoint" to the deeper "**multiple subsystems concurrently open the single-file DuckDB read-write in one process**"; the design below targets *that*. See §"Gap 1" and the §"Interim stopgap".
- **Date:** 2026-06-29
- **Deciders:** Ervin
- **Extends:** ADR-0095 (the editions crash-safe durability hardening — it *wired* the chunk-3 primitives into the paths a crash takes, but only the **boot / clean-shutdown / snapshot-daemon** paths; this ADR extends the same wiring to the **quote/intake daemon write path** it never reached, and makes `recover_or_refuse` coherent for the **snapshot-ahead-of-mirror** case ADR-0095 §1 did not consider), ADR-0097 (introduced the quote pricing/intake daemon whose ~19 raw `duckdb::Connection::open` sites are the uncovered surface)
- **Grounds / related:** ADR-0082 (validated logical DuckDB snapshots — the corruption-free recovery substrate), ADR-0008 + ADR-0030 (the tamper-evident audit hash-chain ledger and its append-only JSONL mirror + the `sync_mirror` lockstep contract this design leans on), ADR-0093 / `SAW-OFF.md` (the product-line saw-off: editions tree vs the byte-for-byte frozen prod), ADR-0096 (the prod-backport deferral — this ADR **expands its backport surface** with a flag, **out of scope** here), `duckdb/duckdb#23046` (the torn-write / ART-checkpoint corruption family), the **2026-06-29 Defense DB corruption + recovery-refusal incident**, `[[trust-code-not-operator]]`, `[[hulye-biztos]]`.
- **Scope guard:** Authored in the **ABERP-Editions** tree (`Cservin69/ABERP-Editions`, the Defense + Portable line; bundle `main` head **`9c35ebb`** = the `PROD_Defense_v0.2.4` head). Frozen prod (`Cservin69/ABERP`, `PROD_v2.27.76`, tree `2d612811`, roots `~/.aberp/prod` and `~/ABERP`) is **never** touched — per ADR-0093 and Ervin's permanent freeze. This ADR is a **design pass only**: no engine/wiring code is changed this session; the sequenced build is the companion plan named above. The sequenced fix chain ends at the next editions baseline, **`PROD_Defense_v0.2.5`**.

> **Source-availability note (honesty over convenience).** The companion forensic write-up `defense-duckdb-corruption-forensic-2026-06-29.md` was authored to a prior session's *outputs* scratch, which is cleared between sessions; it is **not present** in this run. Rather than cite a document I cannot read, every file:line below was re-derived directly from the code at bundle head `9c35ebb` (the authoritative source the forensic itself points at). Likewise, **`CLAUDE.md` is not present** at the editions tree root (only `SAW-OFF.md`, `FOUNDATION.md`, `README.md`); the house guard-rails are sourced from those and from the `[[trust-code-not-operator]]` / `[[hulye-biztos]]` tokens carried through ADR-0008/0030/0082/0093/0095. Both absences are surfaced as findings (D6), not silently worked around.

## Context

### One incident, two surfaces, the same root as ADR-0095 — on a path ADR-0095 never covered

On 2026-06-29 the Defense edition suffered (1) a torn DuckDB live file and (2) a boot auto-recovery that **refused** to recover from a perfectly good, fresh snapshot. Then — the **live evidence that reframed this ADR** — after the operator's 16:58 recovery booted **clean** (seq 109, validated, mirror reconciled `Unchanged`, Ready), the live DB **re-tore at runtime ~4 minutes later (17:02)** under the daemon workload, with a reproducible signature: every subsystem doing its **own** fresh open failed (`email-relay drain`, `quote-intake notifications`, pricing-pipeline `open DB for next-job` / `open DB for enqueue-fail` ×7 fetched CAD jobs) with `INTERNAL Error: Failed to load metadata pointer (id 0, idx 0, ptr 0)` via `duckdb_open → SingleFileCheckpointReader::LoadFromStorage → MetadataManager::FromDiskPointerInternal → null`, while the **main `serve` process kept running on its already-open handle** (`pricing-pipeline cycle complete fetched=7 enqueued=0` — fetched but could not write). The on-disk checkpoint is torn; only *fresh* opens see it.

This is *not* a regression of ADR-0095 — it is ADR-0095's **central finding recurring on a new write path**, plus a **deeper root the 17:02 re-tear exposed**. ADR-0095 proved the crash-safe primitives in `crates/aberp-snapshot/src/crash_safe.rs` (`atomic_install:141`, markers `:161`, `durable_checkpoint:222`) are sound but protect **nothing on a path that does not call them**, and it wired them into boot creation, boot auto-recover, clean shutdown (`serve.rs:2978`), and the periodic **snapshot daemon**. It did **not** reach the **quote pricing / intake daemon** (ADR-0097, a separate write subsystem with its own raw opens). But the re-tear shows the problem is broader than one un-wired daemon: **the running serve process hosts *many* subsystems that each independently `Connection::open` the same single-file DuckDB read-write, concurrently** — and DuckDB single-file storage is single-writer, so a checkpoint by one open while others hold/open the file tears the on-disk metadata (`duckdb#23046`). Two gaps, one deepened root: **runtime DB access is a free-for-all of concurrent separate opens that bypass both the single-writer discipline and the durable checkpoint, while the daemon writes also bypass the lockstep mirror append.**

### Gap 1 — concurrent separate-instance opens of a single-writer file (the deepened root) + no durable checkpoint on the daemon path

**The reproduction (named case).** Recover at 16:58 → clean boot, seq 109, mirror `Unchanged`, Ready → **re-tear at 17:02 (~4 min)** under daemon load → every fresh `Connection::open` fails with `LoadCheckpoint … ptr 0` while the main serve handle survives. Reproducible.

**The mechanism, confirmed against the code.** The running `serve` process is a **single process hosting many independent DuckDB openers**, each calling its **own** `duckdb::Connection::open(&db_path)` on the **same** `~/.aberp-defense/defense/aberp.duckdb`, in **read-write** mode (no `AccessMode::ReadOnly` anywhere → DuckDB's default is RW), each of which can run an **in-place checkpoint on close**. There is **no shared, long-lived DB handle** — `AppState` (`serve.rs:3805`) holds daemon handles (`email_outbox_daemon`, `catalogue_push`, `quote_pdf_rerender_queue`, …) but **no** DB `Connection`; both the request handlers and the daemons open per-operation (serve.rs alone has 154 `Connection::open` sites). Concurrently in one process at steady state:

| Opener (all `tokio::spawn` in the serve process) | Cadence / trigger | Spawn gate |
|---|---|---|
| pricing-pipeline daemon (`run_daemon_supervised`, `serve.rs:2730`) | per job through enqueue/extract/price/render/post stages | storefront reach enabled |
| quote-intake daemon (`run_daemon_forever`, `serve.rs:2425`) | poll cadence (~60 s) | storefront reach enabled |
| catalogue-push daemon (`serve.rs:2464`) | per cycle | storefront reach enabled |
| **email-relay drain (`serve.rs:2866` → `email_relay_daemon.rs:75`)** | **every 2 s** (`DRAIN_TICK_SECS=2`), opens DB **every tick** to claim a row (`:135`) | **unconditional — no gate, no kill switch** |
| email-outbox poll daemon (`serve.rs:2982`) | poll cadence | storefront reach + `ABERP_EMAIL_OUTBOX_POLL_DISABLED` |
| pdf-rerender daemon (`serve.rs:3064`) | on stock-alert flip | storefront reach + `ABERP_PDF_RERENDER_DISABLED` |
| snapshot daemon (`serve.rs:3147`) | every 4 h (`EXPORT`, read) | `ABERP_SNAPSHOT_DISABLE` |
| NAV poll / ap-sync daemons (`serve.rs:2012/2074`) | poll cadence | tenant `nav_enabled` |
| every serve HTTP request handler | per request | — |

DuckDB single-file storage is **single-writer**; multiple concurrent *separate instances* (each `Connection::open` is a distinct `Database`, not a shared one) writing/checkpointing the same file is exactly the `duckdb#23046` torn-metadata path. The `take.rs:172` comment that "DuckDB shares one instance per process, so no cross-process lock conflict" is the **assumption the 17:02 re-tear disproves**: separate `Connection::open` calls do **not** share an instance, so N openers = N checkpoint actors racing one file. At a 2-second email-relay tick plus pricing/intake/outbox traffic, the collision window is hit within minutes — hence "reproducible, not intermittent."

**The daemon-path durable-checkpoint gap (the ADR-0095 surface) sits on top of this.** Even setting concurrency aside, `apps/aberp/src/quote_pricing_pipeline.rs` carries **19** `duckdb::Connection::open*` sites (grep at `9c35ebb`); its **runtime write** sites — the ones a crash traverses during pricing — are:

| Site (`quote_pricing_pipeline.rs`) | Stage / purpose |
|---|---|
| `:671` | enqueue a pricing job (the primary write entry) |
| `:798` | enqueue-fail record |
| `:885` | poll-outcome **audit** write |
| `:927` | claim next actionable job |
| `:964` | extract stage |
| `:1136` | price stage |
| `:1641` | render stage |
| `:1774` | post stage (read) |
| `:1817` | post-finish stage |
| `:2230` | daemon-panic **audit** write |
| `:4213` | python-resolved **audit** write (`emit_python_resolved_audit`) |
| `:4273` | index-migrated **audit** write |

(The remaining matches are helper signatures taking `&mut duckdb::Connection` at `:2915/:2968/:3038/:3330`, and `#[cfg(test)]` reopen/in-memory helpers at `:5030/:5304/:6206/:6342/:6379/:6500/:6510`.) Each runtime site runs inside a `spawn_blocking` closure that opens a **fresh raw connection**, runs a transaction, commits, and **drops the connection** — at which point DuckDB performs its implicit **in-place close-checkpoint**, folding the WAL into the live `*.duckdb` in place. That is precisely the `duckdb#23046` torn-write / ART-checkpoint path: a crash mid-fold leaves a torn file (alternating DB headers, the newest with a zeroed `meta_block` → `LoadCheckpoint … ptr 0` on the next open), the exact signature ADR-0095 documented for the 2026-06-27 Defense first-launch crash.

The decisive evidence is a grep: `live_durable_checkpoint`, `durable_checkpoint`, and `live_checkpoint` appear **zero** times in `quote_pricing_pipeline.rs`. ADR-0095 §3's "wire the existing mechanism where it isn't" reached the snapshot daemon and the post-*regulated-invoice*-write debouncer, but the **quote daemon's** ~12 runtime writes still rely on DuckDB's default (vulnerable) close-checkpoint. Note the pricing daemon's *own* stages are serial *within* its loop (`run_daemon_supervised:2081` → `run_daemon_forever:2016`) — but that intra-daemon serialization is irrelevant to the 17:02 re-tear, because the tear comes from the daemon's opens racing the **other** subsystems' opens (email-relay every 2 s, intake, outbox, the snapshot daemon, request handlers). So Gap 1 has **two layers**: the **process-wide concurrency layer** (N separate RW instances on one single-writer file — the dominant cause of the re-tear) and the **per-write durability layer** (no validated checkpoint on the daemon path). A fix that addresses only the second (debounce a durable checkpoint) leaves the first wide open: any *other* concurrent opener's close-checkpoint still tears the file. **Both layers must close.**

### Gap 2 — the recovery guard refuses exactly when a fresh valid snapshot exists

When boot tried to auto-recover the torn DB, `recover_or_refuse` (`crates/aberp-snapshot/src/recover.rs:126`) found the latest **valid** snapshot was **ahead** of the audit-ledger mirror — snapshot head `109`, mirror head `106` — and **refused**:

```rust
// recover.rs:177–188
if snapshot_audit_count > mirror_max_seq {
    return Ok(RecoveryOutcome::RefusedUnsafe {
        reason: format!(
            "latest valid snapshot (audit_count={snapshot_audit_count}) is AHEAD of the \
             mirror head (seq={mirror_max_seq}); snapshot and mirror disagree — refusing"),
        retained_corrupt_db,
    });
}
```

This guard treats *snapshot ahead of mirror* as "they disagree, don't guess." But a snapshot ahead of the mirror is **not** disagreement — it is exactly the state you want to recover **from**: a logical `EXPORT DATABASE` (ADR-0082, corruption-free by construction) that captured DB state the lagging mirror had not yet caught up to. The whole rebuild path reinforces the wrong anchor: `build_and_validate` (`recover.rs:261–333`) makes the **mirror** the head of truth — it requires `chain_len == mirror_max_seq` (`:310`) and the rebuilt head `entry_hash == mirror_head_hash` (`:321`) — so even structurally it can only succeed when `mirror ≥ snapshot`. **Gap 2(a):** the engine cannot recover from an ahead snapshot even though that snapshot is the freshest valid truth available.

**Gap 2(b) — why the snapshot was ahead in the first place.** The mirror lagged because the quote daemon never appends its audit writes to the mirror. `sync_mirror` (`crates/audit-ledger/src/mirror.rs:376`) is the lockstep append-and-fsync that keeps `<db>.audit.log` tracking committed DB state — and grep confirms the daemon calls it **zero** times (no `sync_mirror` / `ensure_consistent_with_db` in `quote_pricing_pipeline.rs`). So the daemon's audit rows (poll-outcome `:885`, python-resolved `:4213`, index-migrated `:4273`, daemon-panic `:2230`) land in the **DB only**. Then the periodic snapshot daemon's `take_snapshot` (`crates/aberp-snapshot/src/take.rs:142`) does `EXPORT DATABASE` straight off the **live DB** (`take.rs:172–178`) and records `meta.audit_count` from that export — so the snapshot captures the daemon entries the mirror lacks. Result: snapshot `audit_count` (109) overtakes `mirror_max_seq` (106). The lag is **structural, not a race**: nothing ever fsynced the mirror up to the DB on the daemon path, and nothing fsyncs it before a snapshot.

### How the two gaps composed into the incident

A daemon stage write tore the live DB (Gap 1). Boot detected the torn open and invoked the ADR-0095 auto-recover engine. The engine found a valid snapshot — but ahead of the structurally-lagging mirror (Gap 2b) — and hit the `snapshot > mirror` guard (Gap 2a), returning `RefusedUnsafe`. The safe fallback (preserve-and-surface) held — **no data was destroyed, evidence was retained** — but recovery did not complete automatically, which is the `[[trust-code-not-operator]]` gap ADR-0095 exists to close. Both gaps are the same omission seen from two sides: **the daemon write path was never given the crash-safe checkpoint (Gap 1) nor the lockstep mirror append (Gap 2b), and the guard was never taught the snapshot-ahead case the missing lockstep made reachable (Gap 2a).**

### Prod carries the same latent gap — and it stays frozen (ADR-0096)

Frozen prod runs DuckDB's same default in-place checkpoint, so it carries the **same latent torn-write vulnerability**; and the recovery guard lives in the **shared** `aberp-snapshot` crate, so the `snapshot > mirror` refusal is latent there too **if** prod ever ran this engine. But prod does **not** carry ADR-0095's wiring (editions-only, `ensure_not_prod_path` refuses prod paths), is **invoicing-only** with **no quote daemon**, and is frozen. Critically, **the 2026-06-29 incident is an *editions* incident; it does NOT trip ADR-0096's trigger #2**, which is a *prod* corruption recurrence. The only prod-facing action here is a **backport-surface flag** (below): when a future trigger reopens ADR-0096, its "port, don't rewrite" list must grow to include this ADR's daemon-path wiring and guard-coherence fix. That flag is recorded and **explicitly out of scope** for this session.

## Decision

Close both gaps by **collapsing all runtime DB access onto one shared instance + extending ADR-0095's wiring to that one write path**, and **making the recovery guard coherent** — reusing the existing primitives (`live_durable_checkpoint`, `durable_checkpoint`, `atomic_install`, the verified-good markers, `sync_mirror`, the ADR-0082 export/import), inventing the **minimum** new surface (one shared DB handle, one new guard branch). No new durability primitive is invented; no pricing behavior changes.

Gap 1 has two layers (above), so the fix has two parts: **(1a) eliminate the concurrent separate opens** — the dominant cause of the 17:02 re-tear — and **(1b) make the one remaining write path durably checkpoint** through the validated crash-safe primitive. 1a is primary; 1b without 1a does not stop the tear.

#### 1a — One process-wide DuckDB access path (eliminate concurrent separate instances)

**Design (conservative call D1).** Open the live tenant DB **once** at boot into a single shared instance owned by `AppState`, and route **every** runtime DB access in the serve process — all daemons *and* all request handlers — through that one instance, so there is exactly **one** `Database`, **one** checkpoint actor, and DuckDB's own MVCC/locking (not N racing OS-level opens) mediates concurrency. Concretely:

- Add a shared handle to `AppState`, e.g. `db: Arc<aberp_db::Handle>` where `Handle` owns the single `duckdb::Connection` opened at boot and hands out **cloned connections to the same instance** (`Connection::try_clone`, which shares the `Database`) for read work, and serializes **writes** behind the handle (a `Mutex`/writer-actor so writes never interleave a checkpoint). This is the seam the codebase assumed existed (`take.rs:172`) but never built.
- **Replace every runtime `Connection::open(&db_path)` on the live tenant path** with `state.db.read()` / `state.db.write()` — across `quote_pricing_pipeline.rs` (the ~12 runtime sites), `email_relay_daemon.rs` (`:135/:158/:187/:216/:239/:418`), `email_outbox_poll_daemon.rs`, `quote_pdf_rerender_daemon.rs`, `catalogue_push.rs`, the quote-intake daemon, and the serve request handlers. The only code that still calls `Connection::open` on the live path is the boot path that *creates* the shared handle (and `provision_atomic`/`recover_or_refuse`, which run before serve is up).
- **Snapshots stay on a logical read.** `take_snapshot`'s `EXPORT DATABASE` can run on a cloned read connection of the shared instance (no second OS open), keeping ADR-0082's "EXPORT never touches the ART/checkpoint structure" property while removing it as a concurrent *separate* opener.
- **Coverage is enforced (D5):** a CI grep/lint gate fails the build on any new `Connection::open*` against the live tenant path **outside** `aberp_db::Handle` (test/in-memory and the boot-create site are allow-listed). The class that caused 2026-06-29 becomes a red build, not a latent corruption.

This is a larger change than "wire a checkpoint" — it touches every live-DB open in `apps/aberp` (CI-gated, Mac-gated e2e), and is the bulk of v0.2.5. That cost is the honest price of the defect: the 17:02 re-tear proves piecemeal per-daemon hardening is insufficient while *any* other subsystem opens the file independently.

#### 1b — Durable checkpoint + lockstep mirror on the one write path

With all writes funneled through `aberp_db::Handle`, give the handle a **post-commit hook** that:

1. **Lockstep mirror append** — `sync_mirror(&conn, &meta, mirror_path)` on the just-committed connection, so the mirror tracks the DB continuously (this *also* closes Gap 2b at the source). `sync_mirror` already fsyncs (`mirror.rs:485–486`).
2. **Debounced durable checkpoint** — `live_durable_checkpoint(db_path, tenant)` (`recover.rs:393`), the ADR-0095 §3 wrapper: a cheap no-op when `checkpoint_is_current` (`crash_safe.rs:200`), otherwise one validated `durable_checkpoint`. The handle **disables DuckDB's implicit checkpoint-on-close** for runtime connections (it never closes the shared instance mid-run) and checkpoints **only** via this validated path.

**Cheap by construction (ADR-0095 adversarial #4):** coalesce ≤ 1 durable checkpoint/min + one at loop-idle (queue drained), off the request path, on the existing blocking thread; `checkpoint_is_current` makes quiescent ticks free. The 4-h store-snapshot cadence is unchanged.

**Additive (no behavior change to prices):** the handle wraps the *existing* transactions verbatim; mirror append + checkpoint are **post-commit side effects**. Pricing inputs, outputs, schema, and audit payloads are untouched — a price computed before and after is byte-identical.

> **Veto path for D1.** If a full single-instance migration is judged too large for one cycle, the fallback is the smaller **`PricingWriter` seam** (route only the pricing daemon's ~12 writes through one serialized writer with the 1b hook) **plus the §"Interim stopgap" daemon-quiesce flags and the one-line email-relay-drain gate** to remove the *other* concurrent openers. This is weaker — it leaves the serve request handlers opening per-request — but it is shippable faster as a bridge. The recommendation is the full 1a migration; the bridge is explicitly second-best.

### Part 2 — Gap 2: recovery-guard coherence

**(a) Recover from a self-certified-valid snapshot when the mirror is merely behind it.** Replace the hard `snapshot_audit_count > mirror_max_seq → RefusedUnsafe` (`recover.rs:177–188`) with a three-way decision, and make the rebuild's head-of-truth `max(snapshot_head, mirror_head)` instead of always the mirror:

```
recover_or_refuse(db, store, mirror, tenant):
  preserve evidence (copy live → <db>.CORRUPT-<tag>)            # unchanged, recover.rs:138
  snap   = latest VALID snapshot (ADR-0082 meta.valid)          # unchanged, recover.rs:146
          none → RefusedNoSnapshot                              # unchanged
  read mirror (never mutate); missing/corrupt → RefusedUnsafe   # unchanged, recover.rs:161

  if snap.audit_count <= mirror_head:        # snapshot is a PREFIX of the mirror
      → existing path: IMPORT snap, REPLAY mirror delta (seq>snap.audit_count),
        validate head==mirror_head (recover.rs:261–333), install.        # unchanged

  else (snap.audit_count > mirror_head):     # AHEAD snapshot — the new branch
      IMPORT snap into private staging
      SELF-CERTIFY: staging chain verifies genesis→snap.audit_count       # ADR-0008
                    AND mirror is a consistent PREFIX of the snapshot
                    (entry_hash agrees on the overlap [1 .. mirror_head])
      if self-certifies:
          rebuild = staging (head = snap.audit_count)
          TOP UP the mirror to the snapshot head: append snapshot entries
            (mirror_head .. snap.audit_count] verbatim, fsync             # never truncate
          install rebuild (atomic_install + write_marker)
          → Recovered  (mirror & DB reconcile to Unchanged; no fork)
      else:   # ahead snapshot cannot self-certify, or overlap disagrees
          fall back to newest VALID snapshot whose audit_count <= mirror_head
            and take the prefix path above
          if none exists → RefusedUnsafe (preserve-and-surface)           # safe fallback
```

The ahead-snapshot rebuild **tops up** the lagging mirror to the snapshot head (append + fsync, the same append `sync_mirror` does) so the system reconciles to `Unchanged` with **no chain fork** — the mirror is **extended, never truncated**. Self-certification has **two** gates (D4): the snapshot's own hash-chain must verify genesis→head (it is an ADR-0082 export, so this is the same check `validate_export`/`Ledger::verify_chain` already perform), **and** the mirror must agree with the snapshot over their overlap. If either fails we do **not** guess — we fall back to the newest valid snapshot **≤ mirror head** (the original prefix path), and only if *that* is also impossible do we return `RefusedUnsafe`. `build_and_validate` is generalized so the validated head is `max(snapshot_head, mirror_head)` and the head-hash check compares against whichever source is the head.

**(b) Mirror lockstep + pre-snapshot fsync — make the refusal *unreachable* going forward.** Prevention, so 2(a) is only ever a fallback for legacy snapshots:

1. **Lockstep on the daemon path** — the same post-commit `sync_mirror` from Part 1 (1b, on the shared `aberp_db::Handle`): the mirror tracks the DB continuously, so a snapshot can no longer find audit rows the mirror lacks.
2. **fsync the mirror before each snapshot** — `take_snapshot` (`take.rs:142`) must, *before* `EXPORT DATABASE`, reconcile and fsync the mirror to the live DB (`ensure_consistent_with_db(&conn, mirror_path)` / `sync_mirror`, both of which fsync). Then a snapshot can **never** get ahead of the mirror — `snapshot.audit_count ≤ mirror_head` by construction, so the prefix path always applies.

**P0 safety preserved.** The mirror-**ahead-of-DB** preserve-and-refuse (`ensure_consistent_with_db`, `mirror.rs:625–645`, `preserve_ahead_mirror:689`) is **untouched**: we never silently truncate an ahead mirror, we always preserve evidence (`<mirror>.ahead-<nanos>.bak`, and `<db>.CORRUPT-<tag>`), and every recovery remains audited (`db.auto_recovered`) and reversible. The new branch only changes the **snapshot-ahead-of-mirror** decision from "refuse" to "recover-then-extend-the-mirror," which never destroys data.

## Decisions surfaced (conservative calls — Ervin can veto)

- **D1 — Gap 1 shape: one process-wide DuckDB instance/handle (1a) that ALL daemons + request handlers share, not just a pricing writer.** *Call: full single-instance migration.* The 17:02 re-tear proves any *other* concurrent opener tears the file, so the seam must cover every live-DB open in the serve process. *Veto →* the smaller `PricingWriter` bridge + the stopgap flags + the email-relay-drain gate (§"Interim stopgap"); faster to ship but leaves request handlers opening per-request (weaker).
- **D2 — Checkpoint cadence: coalesce ≤ 1 `durable_checkpoint`/min + one at loop-idle; the shared handle disables DuckDB's implicit checkpoint-on-close and checkpoints only via the validated path; keep the 4-h store-snapshot cadence.** *Call: ≤1/min + idle.* `checkpoint_is_current` makes quiescent periods free. *Veto →* tune the window or drop the idle checkpoint.
- **D3 — Ahead-snapshot recovery tops up the mirror to the snapshot head (append + fsync) so it reconciles to `Unchanged`.** *Call: top-up, never truncate.* *Alternative (rejected):* rebuild from snapshot but leave the mirror behind and let the next `sync_mirror` Extend it — leaves a transient ahead-DB window and a second reconcile.
- **D4 — Self-certification bar for the ahead snapshot = chain-verifies **AND** mirror-overlap-agrees.** *Call: both gates.* *Veto →* chain-only (weaker: could accept a snapshot that forks the mirror's recorded history; not recommended).
- **D5 — Add a CI grep/lint gate** that fails on any new `Connection::open*` against the live tenant path **anywhere in `apps/aberp`** outside `aberp_db::Handle` (test/in-memory + the boot-create site allow-listed). *Call: yes* — the class is "easy to forget one," so make forgetting a red build.
- **D6 — Missing-source findings surfaced, not papered over.** The 2026-06-29 forensic markdown and `CLAUDE.md` are absent this session (outputs cleared; no `CLAUDE.md` at the editions root). All evidence was re-derived from code at `9c35ebb`; guard-rails sourced from `SAW-OFF.md`/`FOUNDATION.md`. Flag: if a canonical `CLAUDE.md` is expected at the editions root, it is missing and should be restored.
- **D7 — Prod backport: flag only, stays deferred.** Append the daemon-path wiring + guard-coherence fix to ADR-0096's "port, don't rewrite" surface, but do **not** backport: 2026-06-29 is an *editions* incident and does not satisfy ADR-0096 trigger #2 (a *prod* recurrence). **Out of scope** here.
- **D8 — Interim stopgap is partial, and the email-relay-drain gate should ship as a one-line hotfix.** The daemon-quiesce flags (§"Interim stopgap") remove the high-frequency storefront openers but **cannot** make the process single-writer, because email-relay-drain is spawned unconditionally with a 2 s tick and no kill switch. *Call:* ship the flags now as risk-reduction **and** land a one-line `ABERP_EMAIL_RELAY_DRAIN_DISABLED` gate (mirroring the outbox/rerender switches) as a fast bridge hotfix ahead of the full 1a migration. *Plainly: there is no env-only configuration in v0.2.4 that guarantees no re-tear.*

## Interim stopgap (pre-v0.2.5) — partial, grep-verified, with the limit stated plainly

To run a degraded-but-more-stable Defense for **manual invoicing/quoting** before v0.2.5 ships: first recover the torn DB (`aberp recover --db <path> --tenant <t> --store <store>` — note the Gap 2(a) refusal can bite if a fresh snapshot is ahead of the mirror; that is exactly what this ADR fixes), then relaunch `serve` with the high-frequency storefront openers quiesced:

| Lever (grep-verified at `9c35ebb`) | Effect | Site |
|---|---|---|
| unset `ABERP_QUOTE_INTAKE_ENABLED` **and** `[quote_intake] enabled = false` in `seller.toml` (both — precedence env > toml) | no pricing-pipeline, quote-intake, **or** catalogue-push daemon spawns | `serve.rs:2238–2310` |
| `ABERP_EMAIL_OUTBOX_POLL_DISABLED=1` | no email-outbox poll daemon | `serve.rs:2982` |
| `ABERP_PDF_RERENDER_DISABLED=1` | no pdf-rerender daemon | `serve.rs:3064` |
| leave `ABERP_SNAPSHOT_DISABLE` **unset** | keep the 4-h snapshot daemon (recovery substrate; low-frequency read-open) | `serve.rs:3147` |

**This is risk-reduction, not a guarantee — stated plainly because the main process alone can still tear.** The **email-relay drain daemon has no kill switch**, is spawned unconditionally (`serve.rs:2834`), and opens the live DB **every 2 s** (`email_relay_daemon.rs:51,135`) — so even fully quiesced, the serve process keeps doing concurrent separate opens (email-relay every 2 s + the snapshot daemon + your own SPA writes) and can re-tear. There is **no env-only configuration in v0.2.4** that makes the process single-writer. The only genuinely-safe bridge short of the full 1a migration is a **one-line code gate** on the email-relay-drain spawn (`ABERP_EMAIL_RELAY_DRAIN_DISABLED`, mirroring the outbox/rerender switches) — a trivial hotfix, **not** an env flag that exists today (D8). With that gate added and the table above applied, the only residual openers are the 4-h snapshot daemon and human-paced SPA writes, which rarely overlap.

## Reproduction & regression test (the load-bearing proof)

The fix is not accepted until it **reproduces 2026-06-29 and then prevents it**:

1. **Concurrency re-tear repro (the named 17:02 case), local-ish + Mac-gated.** Drive ≥ 2 independent writers (a pricing-style enqueue loop + an email-relay-style 2 s claim loop) opening the **same** single-file DuckDB via separate `Connection::open` instances under load; assert the on-disk file tears (a fresh open fails `LoadCheckpoint … ptr 0`) on the **pre-fix** code, and **never tears** once both writers go through the shared `aberp_db::Handle`. This is the test that would have caught the gap.
2. **Daemon-write crash-injection (mid-write kill → recoverable).** Re-exec a child that performs a daemon write through the handle and `abort()`s mid-checkpoint; assert the live path is never torn (the validated `durable_checkpoint`/`atomic_install` commit is all-or-nothing) and the next boot opens clean — the plain-file analogue already exists at `recover.rs:505`; extend it to the handle's checkpoint path.
3. **Snapshot-ahead recovery (Gap 2a).** Construct snapshot head 109 > mirror head 106 with a self-consistent overlap; assert `recover_or_refuse` **Recovers** (rebuilds from the snapshot, tops the mirror up to 109, reconciles `Unchanged`) instead of `RefusedUnsafe`; and assert it still **refuses** when the overlap disagrees or the snapshot chain fails to verify.
4. **Lockstep + pre-snapshot fsync (Gap 2b).** Assert that after a daemon write through the handle the mirror head == DB head (no lag), and that `take_snapshot` fsyncs the mirror before `EXPORT` so `snapshot.audit_count ≤ mirror_head` always holds.

## v0.2.5 finalization (2026-07-02) — read() coherence: Option 1 (try_clone), Gap-2b deferred

The 1a design (D1) specifies that `Handle::read()` hands out a `Connection::try_clone`
of the one shared instance. Session C2 briefly deviated: to make a write *mis-routed*
through `read()` fail loud at the DB layer, `read()` returned a SEPARATE read-only
instance (`AccessMode::ReadOnly`, `read_returns_readonly = true` — review F5 / Gap-2b).

**That deviation was proven wrong.** A separate DuckDB instance reads only the durably
CHECKPOINTED live file; it does **not** replay the live writer's WAL. So any mutation
committed since the last debounced (≤ 1/min) durable checkpoint was invisible to
`read()` — the `avl_vendors_route` `revoke→list` stale read, and why four group-A route
modules (quoting_machines, margin_profiles, pricing_job_material, stock_movements) went
red. A round-13 "publish-before-read" (force a durable checkpoint before the read-only
open) papered over it at the cost of a checkpoint on the read path. The discriminating
e2e proved the clean fix: `read_returns_readonly = false` (try_clone of the shared
instance) → **avl 7/7 green**, coherent everywhere.

**Ervin approved Option 1 (2026-07-02).** v0.2.5 adopts `try_clone` as the SOLE read path:

- **Removed:** `HandleConfig::read_returns_readonly`, the `AccessMode::ReadOnly` opener
  (`open_runtime_connection_readonly`), and the publish-before-read
  (`publish_committed_for_readonly`). A `try_clone` reads the shared instance's WAL
  directly — no publish needed.
- **Kept:** the class-1 write-on-read → `write()` routing; the tolerant `ensure_schema`
  + eager `ensure_all_tenant_schemas`; `connection_is_read_only` (now inert on the read
  clone, still guarding the idempotent DDL). Read call sites are unchanged (`handle.read()`).
- **Idiomatic + no tear regression:** one instance / many connections is the DuckDB
  model; a `try_clone` is NOT a second OS opener, so it is not a `duckdb#23046` tear
  vector — consistent with the single-Handle design and the D5 cut-gate, which now
  positively recognizes the Handle-internal `try_clone` (CHECK 10c-tryclone) while STILL
  banning every separate `Connection::open` / `open_with_flags` / `Ledger::open` /
  `DuckDbBillingStore::open` / `append_reopen` at call sites (10d/10f/10h) and keeping the
  frozen residual-opener ledger (10i) intact.

**Gap-2b acceptance.** The cost of Option 1 is losing the *DB-level* write-on-read
fail-loud: a `try_clone` is read-WRITE, so a write mis-routed through `read()` would
succeed silently (bypassing the post-commit durability hook) instead of being rejected.
This is **narrow** (all real writes already flow through `write()`; the class-1 audit
routed the known write-on-read sites) and is **accepted for v0.2.5**, then closed in
v0.2.6 by a **compile-time** read-guard — a *stronger* guarantee than F5's runtime one,
with NO separate instance. The single-WRITER durability invariant is unchanged.

**Deferred to v0.2.6** (see `docs/adr-0098-v0.2.6-scope.md`): (a) the compile-time
write-on-read guard; (b) the residual `Ledger::open` / separate-audit-opener migration
(~180 runtime sites; the operator ERP modules frozen in cut-gate CHECK 10i — 139 openers
across 31 files — which may not grow).

## Consequences

**Easier / safer.** The serve process stops being a free-for-all of concurrent separate opens: one shared DuckDB instance means one checkpoint actor, so the `duckdb#23046` race that re-tore the DB at 17:02 cannot occur. Every write ends in a validated crash-safe checkpoint and a lockstep-fsync'd mirror, so a mid-write kill is recoverable and a snapshot can never outrun the mirror. The recovery engine now recovers from the *freshest valid* truth (an ahead snapshot) instead of refusing it, closing the `[[trust-code-not-operator]]` gap the refusal reopened. The mirror becomes a continuously-current second source of truth on *all* write paths.

**Harder / locked in.** Every live-DB access in `apps/aberp` must go through the shared `aberp_db::Handle` — a real refactor (serve handlers + all daemons), enforced by the CI gate but a standing discipline. Process-wide write serialization is a throughput ceiling (acceptable for a single-operator CNC-shop ERP; revisit only if it bites). The recovery guard gains a branch that must itself be exhaustively crash-tested (a buggy ahead-snapshot recovery is worse than a refuse — ADR-0095 adversarial #1 still applies). We deepen the commitment to the snapshot store **and** the JSONL mirror as twin recovery sources. `take_snapshot` gains a pre-export mirror fsync, a small bounded cost on the snapshot cadence. Until v0.2.5 lands, Defense runs on the partial stopgap (D8) with its stated residual risk.

## Adversarial review

1. *"You're changing the guard to recover from an ahead snapshot — isn't that the 'guess when sources disagree' you condemned?"* No. We recover only when the snapshot **self-certifies** (its own ADR-0008 chain verifies genesis→head) **and** the mirror is a consistent **prefix** of it (overlap hashes agree). That is not disagreement — it is a fresh valid export plus a lagging-but-consistent mirror. If the overlap disagrees or the chain fails to verify, we fall back and ultimately refuse. The bar is *raised*, not lowered.
2. *"Topping up the mirror — isn't that mutating the audit record during recovery?"* It is **append-only extension** of the mirror to entries that already exist, validated, inside the corruption-free snapshot — the same operation `sync_mirror`/`append_db_entries_after` perform in normal operation. We never truncate, never rewrite an existing line; evidence (`.CORRUPT-`, `.ahead-*.bak`) is always retained and the recovery is audited.
3. *"Could the daemon checkpoint stall pricing or thrash disk?"* It is debounced (≤1/min + idle), off the request path, on the daemon's existing blocking thread, and a no-op via `checkpoint_is_current` when nothing changed — the exact posture ADR-0095 adversarial #4 validated for the regulated-write debouncer.
4. *"Could any of this touch prod?"* No. Every snapshot/recover/checkpoint entrypoint calls `ensure_not_prod_path` (`recover.rs:133/356/395`, `take.rs`); editions binaries bind non-prod roots (ADR-0093); the quote daemon is editions-only. Prod is untouched and explicitly out of scope (ADR-0096).
5. *"Why a single process-wide instance instead of just adding a debounced checkpoint to the pricing daemon?"* Because the 17:02 re-tear proves a per-daemon checkpoint is **insufficient**: the tear came from the pricing daemon's opens racing the **other** subsystems' opens (email-relay every 2 s, intake, request handlers). Even a perfect durable checkpoint on one daemon does nothing about a *different* opener's close-checkpoint tearing the file. Only collapsing all openers onto one shared instance removes the race; the debounced checkpoint then protects that single writer.
6. *"Isn't a full single-instance migration too big / risky?"* It is big (every live-DB open in `apps/aberp`), which is why it is CI-gated and Mac-gated e2e, sequenced as the bulk of v0.2.5, with the smaller `PricingWriter` + stopgap-flags + email-relay-gate bridge as the explicit veto path (D1/D8). But "big" is the honest size of the defect: the alternative — keep N independent openers and hope they don't collide — is what produced a *reproducible* corruption. A single shared instance is also the configuration the code already *assumed* it had (`take.rs:172`).

## Alternatives considered

- **Per-site checkpoint + mirror calls (no writer).** Rejected as the primary design (offered as the D1 veto path): correct but fragile across 12 sites; it is how this gap was born. Acceptable only with the CI gate as a hard backstop.
- **Make the snapshot daemon the only mirror updater (drop daemon-path lockstep).** Rejected: it leaves a window where the DB is ahead of the mirror between snapshots — exactly the lag that produced the refusal; lockstep at the write removes the window at the source.
- **Auto-truncate the ahead mirror to the snapshot, or the snapshot to the mirror.** Rejected — destroys a record / discards fresh valid state and risks a chain fork (ADR-0082/ADR-0095 reconcile safety; the 2026-06-22 "Sequencing note"). We extend, never truncate.
- **Fix DuckDB's in-place checkpoint.** Out of our control (`duckdb#23046`); libduckdb is pinned for storage compatibility. We harden around it, as ADR-0095 did.
- **Backport to prod now as part of this fix.** Rejected — out of scope; spends the saw-off invariant for a bounded, monitored risk on a frozen, quote-daemon-free line. Recorded as the D7 flag against ADR-0096 instead.

## Open questions

- Exact debounce window and whether the loop-idle checkpoint should also fire on supervisor re-spawn (`run_daemon_supervised:2081`). Proposed: ≤1/min + idle + on graceful loop exit.
- Should an ahead-snapshot auto-recovery raise a persistent UI banner until acknowledged (parallel to ADR-0095's open question on `db.auto_recovered`)? Leaning yes — an automatic recovery from an ahead snapshot should never be invisible.
- Whether `take_snapshot`'s pre-export reconcile should *hard-fail* the snapshot on a mirror-ahead-of-DB condition (vs today's preserve-and-surface from `ensure_consistent_with_db`). Leaning: surface, don't fail the snapshot — the snapshot is still valid evidence.
