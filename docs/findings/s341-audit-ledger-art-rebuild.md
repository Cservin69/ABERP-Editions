# S341 / PR-36 — audit-ledger ART rebuild (in-place index recovery)

**Date:** 2026-06-10 · **Branch:** `session-341/pr-36-audit-ledger-art-rebuild`
**Reported against:** PROD (the live ART crash Ervin sees on every audit-bearing
commit) · **Builds on:** S332 (`docs/findings/s332-duckdb-art-email-outbox.md`),
S335 (`docs/findings/s335-email-outbox-throttle.md`).

> **TL;DR.** S332/S335 throttled the *volume* hitting the corrupt DuckDB ART but
> never repaired the on-disk index image — so any audit-emitting commit (material
> CRUD, partner CRUD, catalogue-push, the shutdown row) still panics in
> `FixedSizeAllocator::New`, aborting the whole transaction. This PR ships a
> surgical, opt-in `aberp audit-rebuild` subcommand that **regenerates the ART
> from a verified-intact image of its own rows** — DROP + CREATE (identical
> schema, `UNIQUE(seq)`/`UNIQUE(id)` PRESERVED) + verbatim re-INSERT — gated by
> `verify_chain` before AND after. It is **NOT a schema relaxation**; the
> tamper-evident chain is preserved byte-for-byte. Phase 1 (CHECKPOINT/VACUUM)
> is the cheapest possible fix and is documented for the operator to try first.

---

## 1. The crash (verbatim, from S332 §1 / Ervin's console)

```
duckdb::FixedSizeBuffer::GetOffset → FixedSizeAllocator::New → Prefix::New →
ARTOperator::InsertIntoNode → ARTOperator::Insert →
ART::InsertKeys → ART::Insert → ART::Append → BoundIndex::Append →
DataTable::AppendToIndexes → LocalTableStorage::AppendToIndexes →
LocalStorage::Flush → LocalStorage::Commit → WriteToWAL → Commit → ...
This error signals an assertion failure within DuckDB.
Error code 1: Unknown error code kind=quote.email_outbox_fetched
```

The fault is in DuckDB's ART prefix-compression allocator. The worst case for it
is a **monotonic key with long shared prefixes** — exactly `seq` (strictly
increasing `BIGINT`). The crash fires on `AppendToIndexes` → `WriteToWAL` at
**commit time**, not at statement-execute time.

## 2. Emit sites that hit it — every one, via one shared INSERT

The corruption is **index-level, not call-site-level**: every audit producer in
the binary funnels through the single insert path

- `crates/audit-ledger/src/storage/mod.rs::append_in_tx`
  → `insert_entry_verbatim` → `tx.execute(schema::INSERT, …)`
  → DuckDB `AppendToIndexes` on the `UNIQUE(seq)` / `UNIQUE(id)` ARTs.

So the panic is reachable from **all** of these (non-exhaustive, by call count):

| File | Audit producers |
|---|---|
| `apps/aberp/src/serve.rs` | material CRUD routes, partner CRUD routes, catalogue-push, payment, the `/health` first-prod-launch row, … (17 append sites) |
| `apps/aberp/src/material_inventory.rs` | inventory ledger (Ervin's "save a material → 500" symptom) |
| `apps/aberp/src/shutdown.rs` | `DaemonShutdownCompleted` (the shutdown row crashes too) |
| `apps/aberp/src/email_outbox_poll_daemon.rs::write_audit` | `EmailOutboxFetched` — the **highest-frequency** producer; the `kind=` token in the crash message is `quote.email_outbox_fetched` only because that producer fires most often, NOT because it is the cause |
| `submit_annulment.rs`, `recover_from_nav.rs`, `incoming_invoices.rs`, `quote_pickup.rs`, `email_relay_daemon.rs`, `email_invoice.rs`, `quote_intake_query.rs` | their respective lifecycle events |

Because the audit row is written in the **same transaction** as the state change
it describes (ADR-0008 §Storage), the ART panic rolls back the *whole* commit —
hence "save a material → backend 500 → nothing saved."

## 3. Phase 1 — CHECKPOINT / VACUUM (try first, ships zero code)

The cheapest possible fix is DuckDB rebuilding the ART internally. **If it works,
no rebuild is needed.** Operator steps (also in the runbook):

```sql
-- with ABERP stopped, against ~/.aberp/<tenant>/aberp.duckdb:
CHECKPOINT;   -- folds the WAL into the main file, re-serialising the ART
VACUUM;       -- reclaims + re-analyses
```

then restart ABERP and try saving a material.

**Result (assumption, pending operator confirmation):** S332 §6 already showed the
crash is in the on-disk ART *image* and is **not reproducible on fresh DBs even
at 1M rows**. A `CHECKPOINT` re-serialises the *same corrupt allocator state*
through the very `WriteToWAL` path in the crash stack, so it is **expected to
re-trigger the panic rather than clear it**. We have NOT been able to run it
against the live prod file from this session (no copy of the prod DB is
available here — S332 §9 requested one). The runbook therefore lists
CHECKPOINT/VACUUM as step 0 ("try the free fix first"), and `aberp audit-rebuild`
as the durable fix if it does not clear the crash. If CHECKPOINT *does* clear it,
S341 ships only the diagnostic + runbook and the rebuild subcommand stays unused.

## 4. Why dropping `UNIQUE(seq)` is unsafe — S332/S335 evidence

The brief's tempting "just drop the offending index" is **structurally
inapplicable and unsafe**, per the prior sessions:

- **No droppable index exists** (S332 §3). `audit_ledger` has zero
  `CREATE INDEX` statements; its only ART indexes are the inline `UNIQUE(seq)` /
  `UNIQUE(id)` constraints, which DuckDB does not list in `duckdb_indexes()` and
  gives no user-addressable name. `DROP INDEX` has nothing to target.
- **`UNIQUE(seq)` is the hash-chain fork guard** (S332 §4). `append_in_tx` reads
  the chain head inside the tx, computes `seq = head + 1`, and relies on
  `UNIQUE(seq)` to reject a racing second writer that read the same head. It is
  the cross-producer fork-prevention for the tamper-evident ledger.
- **Removing it forks the chain — proven** (S335 §3). A coherence probe (now
  `tests/s335_email_outbox_audit_write_coherence.rs`) demonstrated that without
  the cross-handle `UNIQUE(seq)` guard, two writers reuse a `seq`, silently
  dropping a row on the crown-jewel ledger. Detection-at-`verify_chain` is a
  strict downgrade from prevention.

So the fix must **preserve** `UNIQUE(seq)`/`UNIQUE(id)`. S332 §8.3 named the
correct durable path verbatim: *"a tested table rebuild that PRESERVES
`UNIQUE(seq)`/`UNIQUE(id)` (regenerating the ART from a clean image to recover a
corrupted on-disk index), never a constraint drop."* That is exactly what this
PR ships.

## 5. Phase 2 — what `aberp audit-rebuild` does

New out-of-serve-loop subcommand (`apps/aberp/src/audit_rebuild.rs`):

1. **Refuse if a serve process holds the DB** (`lsof -F pc`; DuckDB's
   single-writer lock is the backstop). A rebuild while serve is live would race
   the live audit-write path.
2. **Dump** every row in `seq` order via a **read-only** connection (the read
   path is safe against the corrupt ART — the crash is on INSERT, not SELECT).
3. **`verify_chain` BEFORE** — prove the rows are intact and only the index is
   corrupt. A broken chain means the *data* is suspect → **ABORT** (a rebuild
   would faithfully re-index a tampered chain, masking the tamper).
4. **Backup** `aberp.duckdb` → `aberp.duckdb.pre-rebuild-<unix_ts>.bak`
   (+ `.wal` sidecar if present) unless `--no-backup`.
5. **One transaction:** `DROP TABLE audit_ledger` (destroys the corrupt ART) →
   `CREATE TABLE` with the **identical** `schema::CREATE_TABLE` DDL
   (`UNIQUE(seq)`/`UNIQUE(id)` preserved) → re-`INSERT` every row **verbatim**
   (same `seq`/`prev_hash`/`entry_hash`/`payload` bytes — `insert_entry_verbatim`)
   → append ONE `AuditLedgerRebuilt` marker as the last row. DuckDB executes DDL
   transactionally, so it all lands at `COMMIT` or none does.
6. **`COMMIT`, `VACUUM`.**
7. **`verify_chain` AFTER** (operator-facing integrity gate) + a rows/seq sanity
   check. A post-verify failure loud-fails and points at the backup. Then
   re-sync the ADR-0030 mirror to its new head.

`--dry-run` does 1–3 plus a **non-destructive ART-health probe** (copies the DB
to a throwaway file and attempts one append against the *copy*, so it reports
"ART healthy" vs "ART corrupt" without touching the real ledger) and prints the
plan. The dry-run is a guaranteed no-op: it opens read-only only, so the DB file
mtime is untouched (pinned by `s341_rebuild_dry_run_is_no_op`).

### The integrity invariant
`verify_chain` runs before (rows intact?) AND after (rebuild preserved the
chain?). Either failure aborts/loud-fails. **A rebuild that loses chain
verification is never shipped** — the same invariant the S335 probe proved a
naive "drop the constraint" fix would break. Pinned by
`s341_rebuild_preserves_hash_chain` (THE critical regression test).

## 6. The `AuditLedgerRebuilt` audit event

New `EventKind::AuditLedgerRebuilt` → `audit.ledger_rebuilt` (a new `audit.`
prefix family — a meta-event about the ledger itself, never swept by the
per-invoice `invoice.*` export glob). Payload
(`AuditLedgerRebuiltPayload`): `rows_before`, `rows_after`, `seq_max_before`,
`seq_max_after`, `chain_verified`, `took_ms`. It is the **last row of the rebuild
transaction**, so the rebuilt ledger carries permanent, hash-chained proof the
rebuild happened. `chain_verified` records the pre-rebuild verify that gated the
operation (a persisted `false` is impossible — the rebuild aborts before the
marker otherwise); the post-commit re-verify is the operator-facing gate.

The F12 four-edit ritual fired once (variant + `as_str` + `from_storage_str` +
`round_trip_for_every_variant`).

## 7. Tests

`apps/aberp/tests/s341_audit_ledger_art_rebuild.rs`:

| Test | Pins |
|---|---|
| `s341_rebuild_preserves_row_count` | N originals preserved verbatim (same ids, same order) + exactly one marker → `rows_after == N + 1` |
| `s341_rebuild_preserves_seq_order` | `seq` is dense + monotonic `1..=N+1` after rebuild |
| `s341_rebuild_preserves_hash_chain` | **THE critical one** — `verify_chain` passes after, independently re-checked |
| `s341_rebuild_dry_run_is_no_op` | DB mtime + size unchanged; no marker row; no probe/backup files leaked; probes `Healthy` |
| `s341_rebuild_refuses_if_serve_alive` | end-to-end `lsof` guard fires on a held handle (defers to the unit guard test where `lsof` is absent) |
| `s341_rebuild_marker_payload_records_counts` | marker payload counts are correct |

Plus module unit tests in `audit_rebuild.rs`: `holders_from_lsof_fpc` parsing,
`guard_from_lsof_output` refuse/allow logic, `append_ext`, `resolve_db_path`.

## 8. Honest limitations / conservative calls

1. **Could not run Phase 1 against the live prod file** (none available to this
   session). The CHECKPOINT verdict in §3 is an *expectation* grounded in S332's
   on-disk-image diagnosis, flagged as such. The runbook still has the operator
   try it first (free, non-destructive, stops here if it works).
2. **The ART-health probe is best-effort.** In a release build the DuckDB ART
   `InternalException` is a catchable `Err` (S332 §5); the probe relies on that.
   It runs against a throwaway copy, so a worst-case `abort()` in a debug build
   cannot harm the real ledger — but in that case the probe would crash the
   `audit-rebuild --dry-run` process rather than report `Corrupt`. The real
   (non-dry-run) rebuild does NOT depend on the probe — the operator invokes it
   because they observed the crash (runbook detection step).
3. **The real rebuild always rebuilds** (after backup) — it does not skip on a
   "looks healthy" probe. Re-running on an already-healthy DB is safe (it yields
   a healthy DB with a verified chain + one more marker). "Idempotent" here means
   "safe to re-run, converges to healthy", not "byte-identical no-op". The
   `--dry-run` path provides the "no rebuild needed" advisory.
4. **WAL sidecar on restore.** The backup copies `<db>.wal` to `<bak>.wal` when
   present; the runbook documents the rename-on-restore step. With ABERP stopped
   gracefully first (runbook pre-flight), the WAL is normally already
   checkpointed and absent.

## 9. Operator action

See `docs/runbooks/s341-audit-ledger-rebuild.md`. Short form, ABERP stopped:

```
aberp audit-rebuild --tenant prod --dry-run     # inspect + probe, no mutation
aberp audit-rebuild --tenant prod               # backup + rebuild + verify
```
