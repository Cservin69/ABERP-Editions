# Fix-session plan — editions daemon-path durability & recovery-guard coherence (ADR-0098)

Companion build plan for **ADR-0098**. Editions tree only (`Cservin69/ABERP-Editions`, `main` head `9c35ebb` = `PROD_Defense_v0.2.4`); frozen prod is never touched. The chain ends at **`PROD_Defense_v0.2.5`**. Two gaps, sequenced so the **operator gets a stable degraded mode first**, then the locally-verifiable guard fix, then the large CI-gated single-instance migration that removes the residual risk.

## Root-cause recap (what each session must close)

- **Gap 1a — concurrent separate-instance opens (the 17:02 re-tear).** The serve process hosts many subsystems (pricing/intake/catalogue/email-relay/outbox/rerender daemons + every request handler) that each `duckdb::Connection::open` the same single-file DB read-write, concurrently. DuckDB single-file is single-writer; N separate instances = N checkpoint actors racing one file = `duckdb#23046` torn metadata, reproducibly within minutes (email-relay ticks every 2 s). **Fix: one shared `aberp_db::Handle` instance; nothing else opens the live path at runtime.**
- **Gap 1b — no durable checkpoint on the daemon write path.** `quote_pricing_pipeline.rs` calls `durable_checkpoint`/`live_checkpoint` **zero** times; daemon writes rely on DuckDB's implicit close-checkpoint. **Fix: a post-commit hook on the handle → debounced `live_durable_checkpoint`; never the implicit checkpoint.**
- **Gap 2a — recovery guard refuses a fresh ahead snapshot.** `recover_or_refuse` (`recover.rs:177–188`) returns `RefusedUnsafe` when `snapshot_audit_count > mirror_max_seq`; `build_and_validate` anchors head-of-truth on the mirror. **Fix: recover from a self-certified ahead snapshot, top the mirror up to it; fall back to newest valid snapshot ≤ mirror head; refuse only if neither works.**
- **Gap 2b — the mirror lags the daemon writes, so a snapshot outruns it.** The daemon never calls `sync_mirror`; `take_snapshot` `EXPORT`s the live DB without fsyncing the mirror first. **Fix: lockstep `sync_mirror` on the handle's post-commit hook + pre-`EXPORT` mirror fsync in `take_snapshot`.**

## Sequencing rationale

Gap 2 (recovery + lag) lives almost entirely in the **pure crates** (`aberp-snapshot`, `audit-ledger`) and is **locally verifiable** (plain-file + in-memory unit tests run in the saw-off sandbox; the DuckDB-backed e2e is Mac-gated). Gap 1 lives in **`apps/aberp`** and is **CI-gated / Mac-gated e2e** (the bundled libduckdb amalgamation cannot build in the sandbox — same gate as ADR-0095's chunk-3 and Session B). So: ship the **bridge** immediately to stop the bleeding, do the **locally-verifiable Gap 2** next (high value, independent of the big migration), then the **large Gap 1a/1b migration** that actually removes the concurrency defect, then finish + cut the release. Sessions S0 and A are parallelizable; B depends on A's mirror/checkpoint contract; C depends on B.

| Session | Scope | Where | Verifiable | Ships |
|---|---|---|---|---|
| **S0 — bridge** | stopgap flags (ops) + 1-line `ABERP_EMAIL_RELAY_DRAIN_DISABLED` gate | `apps/aberp` (trivial) | env-read unit; ops | hotfix on `main` |
| **A — guard coherence** | Gap 2a recover branch + Gap 2b pre-snapshot fsync | `crates/aberp-snapshot`, `crates/audit-ledger` | **LOCAL** (plain-file + in-memory); e2e Mac-gated | `v0.2.5-rc` |
| **B — single instance** | Gap 1a `aberp_db::Handle` + migrate daemons + 1b hook | `apps/aberp`, new `crates/aberp-db` | CI + Mac-gated e2e | `v0.2.5-rc` |
| **C — finish + gate** | migrate serve request handlers + CI grep/lint gate (D5) | `apps/aberp` | CI + Mac-gated e2e | `v0.2.5` |
| **D — docs/release** | runbook, ADR status, ADR-0096 backport flag, cut tag | docs | LOCAL | `PROD_Defense_v0.2.5` |

## Session S0 — the bridge (ship first, stop the bleeding)

**Goal:** a stable-enough degraded Defense for manual invoicing/quoting *today*, with the residual risk stated.

1. **Ops (no code):** recover the torn DB (`aberp recover …`), then relaunch `serve` with the §"Interim stopgap" levers: unset `ABERP_QUOTE_INTAKE_ENABLED` **and** `[quote_intake] enabled=false`; `ABERP_EMAIL_OUTBOX_POLL_DISABLED=1`; `ABERP_PDF_RERENDER_DISABLED=1`; leave `ABERP_SNAPSHOT_DISABLE` unset.
2. **1-line code gate (the only piece that needs a build):** add `ABERP_EMAIL_RELAY_DRAIN_DISABLED` to the email-relay-drain spawn (`serve.rs:2834`), mirroring `email_outbox_poll_daemon::is_disabled` / `quote_pdf_rerender_daemon::is_disabled`. This is the only way to silence the unconditional 2 s opener without the full migration.

**Verifiable:** an env-read unit (`is_disabled()` true/false) — runs anywhere. **Acceptance:** with the gate set + the flags applied, the only residual live-DB openers are the 4-h snapshot daemon and human-paced SPA writes; document that this is **risk-reduced, not tear-proof** (only Session B closes it). **Caveat to flag to Ervin:** if `aberp recover` itself hits the Gap 2a refusal (a fresh snapshot ahead of the mirror), S0 cannot proceed until **Session A** lands — so A is the true unblock if recovery is currently refusing.

## Session A — recovery-guard coherence + pre-snapshot fsync (LOCAL, pure crates)

**Goal:** make `recover_or_refuse` recover from a self-certified ahead snapshot, and make a snapshot unable to outrun the mirror.

**`crates/aberp-snapshot/src/recover.rs`:**

- Replace the hard `snapshot_audit_count > mirror_max_seq → RefusedUnsafe` (`:177–188`) with the three-way branch (ADR-0098 §Part 2a): `≤ mirror_head` → existing prefix path; `> mirror_head` → IMPORT to staging, **self-certify** (staging chain verifies genesis→`snap.audit_count` **and** mirror agrees on the overlap `[1..mirror_head]`), then rebuild-from-snapshot + **top up the mirror** to the snapshot head (append verbatim + fsync, reusing `append_db_entries_after`/`sync_mirror`); else fall back to newest valid snapshot `≤ mirror_head`; else `RefusedUnsafe`.
- Generalize `build_and_validate` (`:261–333`) so the validated head is `max(snapshot_head, mirror_head)` and the head-hash check compares against whichever source is the head (today it hard-asserts `chain_len == mirror_max_seq` at `:310` and the mirror head hash at `:321`).
- Preserve P0: evidence retention (`preserve_corrupt_db`), `ensure_not_prod_path`, and the mirror-ahead-of-DB preserve-and-refuse stay untouched.

**`crates/aberp-snapshot/src/take.rs`:** before `EXPORT DATABASE` (`:172`), reconcile + fsync the mirror to the live DB (`ensure_consistent_with_db` / `sync_mirror`) so `snapshot.audit_count ≤ mirror_head` by construction. Surface (don't fail the snapshot) on mirror-ahead-of-DB.

**Tests (LOCAL where possible):**

- `recover_or_refuse_recovers_from_ahead_snapshot_and_tops_up_mirror` — snap head 109 > mirror 106, consistent overlap → `Recovered{recovered_max_seq:109, replayed/​topped_up:3}`, mirror reconciles `Unchanged`, no fork. *(Mac-gated: needs DuckDB IMPORT.)*
- `recover_or_refuse_refuses_ahead_snapshot_when_overlap_disagrees` and `…_when_snapshot_chain_unverifiable` → falls back to newest valid `≤ mirror_head`, or `RefusedUnsafe`. *(Mac-gated.)*
- `ahead_snapshot_topup_is_append_only_never_truncates` — assert existing mirror lines are byte-identical after top-up. *(Plain-file portion runs anywhere.)*
- `take_snapshot_fsyncs_mirror_before_export_so_snapshot_never_ahead` — after a write, `meta.audit_count ≤ mirror_head`. *(Mac-gated.)*

## Session B — one shared DuckDB instance + durable-checkpoint hook (CI-gated `apps/aberp`)

**Goal:** eliminate concurrent separate opens (Gap 1a) and durably checkpoint the one write path (Gap 1b).

- **New `crates/aberp-db` `Handle`:** owns the single `Connection` opened at boot; `read()` hands out a `try_clone` of the same instance, `write()` serializes writes behind a `Mutex`/writer-actor; a **post-commit hook** runs `sync_mirror` (lockstep) + debounced `live_durable_checkpoint` (`recover.rs:393`), and the handle never lets DuckDB's implicit checkpoint-on-close fire on a runtime connection. `ensure_not_prod_path` on construction.
- **Wire into `AppState`** (`serve.rs:3805`): `db: Arc<aberp_db::Handle>`, created at boot after `provision_atomic`/`recover_or_refuse`.
- **Migrate the daemon openers** to `state.db`: `quote_pricing_pipeline.rs` (~12 runtime sites incl. `:671` enqueue), `email_relay_daemon.rs` (`:135/:158/:187/:216/:239/:418`), `email_outbox_poll_daemon.rs`, `quote_pdf_rerender_daemon.rs`, `catalogue_push.rs`, the quote-intake daemon. `take_snapshot`'s `EXPORT` runs on a cloned read connection.
- **Debounce** per ADR-0098 D2: ≤1 `durable_checkpoint`/min + one at loop-idle; `checkpoint_is_current` no-ops quiescent ticks.

**Tests (the load-bearing proof; Mac-gated e2e):**

- **`concurrent_separate_opens_tear_the_file_but_shared_handle_never_does`** — the named 17:02 repro: ≥2 independent writers (a pricing-style enqueue loop + an email-relay-style 2 s claim loop) on the same single-file DB. Assert **pre-fix** (separate `Connection::open`) → a fresh open fails `LoadCheckpoint … ptr 0`; **post-fix** (both via `aberp_db::Handle`) → never tears across N iterations. This is the test that would have caught 2026-06-29.
- **`daemon_write_killed_mid_checkpoint_is_recoverable`** — re-exec a child that does a daemon write through the handle and `abort()`s mid-checkpoint; assert the live path is never torn and the next boot opens clean. Extends the existing plain-file crash-injection unit (`recover.rs:505`) to the handle's checkpoint path.
- **`daemon_write_appends_to_mirror_in_lockstep`** — after a handle write, mirror head == DB head (no lag) — closes Gap 2b at the source.

## Session C — finish the migration + the CI guard gate (CI-gated `apps/aberp`)

- Migrate the **serve request handlers** (the remaining live-path `Connection::open` sites in `serve.rs`) onto `state.db`.
- Land the **CI grep/lint gate (D5):** fail the build on any new `Connection::open*` against the live tenant path anywhere in `apps/aberp` outside `aberp_db::Handle` (test/in-memory + the boot-create site allow-listed).
- Keep `ABERP_EMAIL_RELAY_DRAIN_DISABLED` (S0) as an ops escape hatch, now redundant for safety.

**Acceptance:** the S0 stopgap flags become *optional* — Defense is tear-safe with all daemons running.

## Session D — docs + release (LOCAL)

- Operator runbook: the stopgap (S0) and the new ahead-snapshot recovery behavior; update `docs/runbooks/db-corruption-recovery-operator-runbook.md`.
- Flip ADR-0098 Status → Accepted/Implemented; record the **ADR-0096 backport-surface flag** (the daemon-path single-instance + guard-coherence fix is added to ADR-0096's "port, don't rewrite" list; **still deferred**, editions-only, out of scope until an ADR-0096 trigger fires).
- Cut **`PROD_Defense_v0.2.5`**.

## Cross-cutting acceptance gate (the one test that must flip)

`concurrent_separate_opens_tear_the_file_but_shared_handle_never_does` (Session B) is the load-bearing proof: it must **fail on `9c35ebb`** (reproducing the 17:02 re-tear) and **pass after Session B**. No `v0.2.5` without it green, plus the Gap 2a ahead-snapshot recovery test and the daemon-write crash-injection test. This mirrors ADR-0095's "a buggy auto-recover is worse than a refuse" — the recovery branch and the shared handle are themselves crash-tested before they are trusted.

## Prod (out of scope — flag only)

Prod carries the same latent in-place-checkpoint vulnerability and the shared-crate guard refusal, but is frozen, invoicing-only, with **no quote daemon**, and the 2026-06-29 incident is **editions**, not a prod recurrence (ADR-0096 trigger #2 untripped). No prod change here; the backport surface is flagged against ADR-0096 for when a trigger fires.
