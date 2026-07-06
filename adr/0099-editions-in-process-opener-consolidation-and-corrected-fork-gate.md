# ADR-0099 — Editions in-process audit-opener consolidation onto the shared Handle, and the corrected write-fork gate

- **Status:** Accepted (implemented this session). The in-process **write-fork** surface for the always-on daemon racer + the serve request handlers is migrated onto the one shared `aberp_db::Handle`; the gate's fork model is corrected (CHECK 10L → CHECK 10M); the remaining in-process residual (`process_digest`, the DÁP heartbeat) and the separate-process CLI writers are **frozen may-only-shrink** and tracked for **v0.2.9**. This ADR does not cut a release (v0.2.8 is cut by Ervin).
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
