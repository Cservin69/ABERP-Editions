# S341 / PR-36 — audit-ledger ART corruption: prod-capable fix (drop the UNIQUE-ART, transparent boot migration)

**Date:** 2026-06-10 · **Branch:** `session-341/pr-36b-audit-art-schema-fix`
**Reported against:** PROD (every audit-bearing commit panics in DuckDB's ART)
**Builds on:** S332 (`docs/findings/s332-duckdb-art-email-outbox.md`), S335
(`docs/findings/s335-email-outbox-throttle.md`).

> **TL;DR.** PROD's `audit_ledger` ART secondary index is corrupt: every
> audit-bearing commit (material CRUD, partner CRUD, catalogue-push, the
> shutdown row) panics in `FixedSizeAllocator::New → Prefix::New`. The root cause
> is an **open, unfixed DuckDB 1.5.x bug** (`duckdb/duckdb#23046`: ART
> UNIQUE/PK-constraint enforcement corrupts the heap on file-backed databases).
> We **avoid the corruption class entirely**: the `UNIQUE(seq)` / `UNIQUE(id)`
> constraints were the table's ONLY ART indexes, so we **drop them**. A
> **transparent boot migration** rebuilds existing prod files off the old schema
> automatically — no operator action. Integrity is unchanged: the tamper-evident
> hash chain (`verify_chain`) is the guarantee, and a new process-wide append
> serializer prevents in-process forks. **This replaces the earlier S341
> operator-run `aberp audit-rebuild` CLI, which Ervin rejected as a workaround
> rather than prod-capable code; that surface has been removed.**

---

## 1. The crash (verbatim, S332 §1)

```
duckdb::FixedSizeBuffer::GetOffset → FixedSizeAllocator::New → Prefix::New →
ARTOperator::InsertIntoNode → ARTOperator::Insert → ART::InsertKeys →
ART::Insert → ART::Append → BoundIndex::Append → DataTable::AppendToIndexes →
… → WriteToWAL → Commit
This error signals an assertion failure within DuckDB.
```

The fault is in DuckDB's ART (Adaptive Radix Tree) secondary-index code, on
insert/commit. Every audit producer funnels through one insert path
(`storage/mod.rs::append_in_tx` → `tx.execute(schema::INSERT)`), and the audit
row is written in the SAME transaction as the state change it describes
(ADR-0008 §Storage), so the panic aborts the whole commit — "save a material →
500 → nothing saved."

## 2. Option A — DuckDB version bump — investigated, NOT viable

We are on bundled DuckDB **1.5.2** (`duckdb-rs 1.10502.0`, workspace pin
`duckdb = "1"`). Latest is **1.5.3** (`duckdb-rs 1.10503.1`).

- **`duckdb/duckdb#23046`** — *"DuckDB 1.5.0's ART index constraint enforcement
  corrupts the heap on file-backed"* — is our exact bug class (ART
  UNIQUE/PK-constraint enforcement corrupting heap on **file-backed** DBs, crash
  in the allocator on insert, regression introduced in **1.5.0**). As of this
  investigation it is **OPEN — not fixed in any released version.**
- **DuckDB 1.5.3's changelog does not address it** (its only ART entry is an
  unrelated `ARTOperator::Delete` nested-leaf fix). Bumping 1.5.2 → 1.5.3 would
  not fix our crash.

→ No upstream fix exists to bump to. Option A is dead. (Downgrading below 1.5.0
predates the regression but would force a storage-format rollback on live prod
files — strictly riskier than removing the index.)

## 3. Option B — drop the UNIQUE-ART (chosen)

The `audit_ledger` table's ONLY ART secondary indexes are the inline
`UNIQUE(seq)` / `UNIQUE(id)` constraints (S332 §3: zero `CREATE INDEX`; the
inline UNIQUEs are the only ARTs). **Remove them → there is no secondary index
to corrupt → the `#23046` crash class cannot occur.**

### Why this does NOT weaken integrity

The `UNIQUE(seq)` looked like the cross-writer fork guard. It was not:

- **ABERP's own S186/PR-186 finding** (`apps/aberp/src/incoming_invoices.rs:54-77`)
  established, with a pinned test, that *"DuckDB's UNIQUE constraint does NOT
  fire across two `Connection::open` handles in the same process"*. So
  `UNIQUE(seq)` never prevented a concurrent in-process fork.
- An **S341 empirical probe** confirmed two concurrent `Connection::open`
  handles on the same file coexist with no exclusive lock — so the constraint
  was never the serializer either.

Integrity is enforced at two layers, neither of them the ART:

1. **Detection — the hash chain.** `verify_chain` walks `seq` order + chain
   links + per-entry hashes and loud-fails on the first duplicate / reorder /
   fork. This is the ADR-0008 tamper-evidence guarantee and is unchanged.
2. **Prevention (in-process) — a new `AUDIT_APPEND_LOCK`.** A process-wide
   `Mutex<()>` in `storage/mod.rs` serializes the whole
   open → read-head → insert → commit window, so two in-process writers cannot
   read the same committed head and both append `seq = head + 1`. Each write
   re-reads the head under the lock (NOT a cached `Mutex<u64>` counter — a
   cached counter would go stale across processes, the exact hazard S335
   documented for persistent connections). It also serializes the
   reopen-per-write `Connection::open`s, which sidesteps a separate
   DuckDB-internal concurrency assertion (`RLEScanState`) observed when many
   independent handles touch one file at once.

Cross-PROCESS writers (a CLI subcommand racing `aberp serve`) are outside the
in-process lock — backstopped by the hash chain's detection, exactly as before
(UNIQUE never covered them). A single serialized audit-writer actor across the
whole process tree remains the documented future hardening (S335 §3.4).

### The new write paths

- `Ledger::append` holds `AUDIT_APPEND_LOCK` across its tx + commit.
- `append_reopen(db_path, meta, kind, …)` — the serialized reopen-per-write
  helper the high-frequency daemons use (replaces hand-rolled
  `Connection::open` + `ensure_schema` + `append_in_tx` + `commit`). The
  email-outbox daemon's `write_audit` now routes through it.
- `append_in_tx` (cross-state txns, e.g. invoice issuance committing billing +
  audit together) is unchanged; those are file-lock-serialized across processes
  and chain-detected. Wiring them through the serializer is incremental future
  work.

## 4. Transparent boot migration (no operator action)

Existing prod files still carry the old `UNIQUE`-ART schema (and the corrupt
on-disk ART). `migrate_drop_unique_art_if_present(conn)`, called from
`ensure_schema` (which every boot / CLI / daemon write path invokes before
appending), runs the one-time recovery automatically:

1. **Detect** — `duckdb_constraints()` reports any `UNIQUE` on `audit_ledger`?
   (A metadata query — does not touch the corrupt ART insert path.) None → the
   post-migration steady state; return immediately (cheap no-op).
2. **Serialize** via `AUDIT_MIGRATION_LOCK` + re-check (another in-process
   caller may have just migrated).
3. **Dump** every row in `seq` order (SELECT is safe against the corrupt ART —
   the crash is on INSERT only, S332 §5) and **`verify_chain`** them: PROVE the
   rows are intact and only the index was corrupt. A broken chain ABORTS loud
   (`AppendError::Migration`) — that is data tampering, not index corruption,
   and a rebuild would faithfully re-index a tampered chain.
4. **Rebuild** in one transaction (manual `BEGIN`/`COMMIT` on the `&Connection`,
   `ROLLBACK` on error): `DROP TABLE` (discards the corrupt ART) → `CREATE TABLE`
   (new no-`UNIQUE` schema) → re-insert every row **verbatim** (same
   `seq`/`prev_hash`/`entry_hash` bytes).
5. **Re-`verify_chain`** the rebuilt table as the safety gate.

The rebuild preserves rows byte-for-byte, so the chain that verified going in
verifies coming out and **no audit row is added** — the migration leaves no
trace in the ledger content (provenance is a `tracing::warn!`). It is a pure
structural recovery, idempotent (second boot detects no `UNIQUE` → no-op), and
also repairs an already-corrupt ART (the `DROP` discards it).

## 5. What changed (files)

**crate `aberp-audit-ledger`**
- `storage/schema.rs` — `CREATE_TABLE` drops `UNIQUE (seq)` / `UNIQUE (id)`
  (CHECK constraints kept; they are row-level, not ART).
- `storage/mod.rs` — `AUDIT_APPEND_LOCK` + `AUDIT_MIGRATION_LOCK`;
  `Ledger::append` locked; new `append_reopen`; `ensure_schema` runs
  `migrate_drop_unique_art_if_present`; private `rebuild_table` +
  `insert_entry_verbatim` (on `&Connection`, via `Transaction: Deref<Connection>`);
  `read_all_entries`, `audit_ledger_has_unique_constraints`.
- `error.rs` — new `AppendError::Migration(String)`.
- `lib.rs` — export `append_reopen`.

**binary `aberp`**
- `email_outbox_poll_daemon.rs::write_audit` → `append_reopen` (serialized);
  dropped now-unused `Connection` / `append_in_tx` imports.
- **Removed the rejected operator-run CLI:** `src/audit_rebuild.rs`,
  `cli::AuditRebuild`(+Args), `main.rs` wiring, `tests/s341_audit_ledger_art_rebuild.rs`,
  `docs/runbooks/s341-audit-ledger-rebuild.md`, the `AuditLedgerRebuilt`
  EventKind + payload + their two downstream exhaustiveness arms. No prod row
  ever carried `audit.ledger_rebuilt` (the CLI never ran on prod), so removing
  the variant is safe.

## 6. Tests

- `crates/audit-ledger/src/storage/mod.rs` (`migration_tests`):
  `fresh_schema_has_no_unique_constraints`,
  `migration_detects_and_rebuilds_off_unique_schema`, `migration_is_idempotent`,
  `ensure_schema_runs_the_migration`, `migration_aborts_on_tampered_chain`
  (refuses + leaves the table untouched), `migration_on_empty_legacy_table_is_safe`.
- `crates/audit-ledger/tests/s341_concurrent_append.rs` —
  `s341_concurrent_appends_stay_dense_and_verify`: 16 threads contend on the
  append lock through serialized reopen-per-write; result is dense+monotonic
  `seq` and a verifying chain (no fork). Without the lock, concurrent
  reopen-per-write forks and this fails.
- All existing audit suites green: S307 full-cycle, S311 stale-recovery, S332
  no-crash, S335 throttle + **coherence** (`s335_persistent_connection_forks…`
  still holds — it never depended on UNIQUE), chain conformance.

## 7. Gates & honest limitations

- `cargo fmt` clean; `cargo clippy --workspace --all-targets -D warnings` clean;
  `cargo test --workspace` green (cad-extract-wrapper cold-OCCT 15s smoke flake
  → green warm, unrelated); release build green; SPA build + vitest 1086 green.
- **No live-prod verification of the migration** (no copy of the prod DB
  available to this session — S332 §9's request stands). The migration logic is
  proven in-memory; the on-disk-ART repair path is the same `DROP`+rebuild that
  cannot re-enter the corruption class (no ART on the rebuilt table). On the
  first upgraded boot the migration runs automatically; if the operator can
  quiesce other writers during that first boot it removes even the narrow
  cross-process first-migration race (otherwise that race is loud + retryable +
  chain-backstopped).
- **`append_in_tx` cross-state callers** (invoice issuance, etc.) are not yet
  routed through `AUDIT_APPEND_LOCK` — they remain file-lock-serialized +
  chain-detected (their prior safety level). Full single-writer coverage is
  documented future hardening, orthogonal to the corruption fix.
- **Deviation from the brief's literal "Mutex<u64> next-seq counter":** a cached
  counter goes stale across processes (serve vs CLI) and would *reintroduce* the
  S335 fork hazard. We use a write-serializer that re-reads the committed head
  each time instead — strictly safer; flagged here per the conservative-choice
  rule.

## 8. Operator action

None for the fix itself — recovery is automatic at the first boot of the
upgraded binary. Deploy with `./run/upgrade_prod.sh PROD_v2.27.18`, restart
ABERP, and confirm a material save succeeds (the boot log shows
`migrated audit_ledger off the legacy UNIQUE-ART schema` once).
