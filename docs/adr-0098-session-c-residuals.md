# ADR-0098 Session C — migration status & flagged residuals

Companion to the fix plan (`docs/daemon-durability-and-recovery-guard-fix-plan.md`,
§"Session C") and ADR-0098. Records exactly what Session C migrated, what it
deliberately left as a flagged residual, and the recipe to finish — for the
adversarial review and the compiler-available follow-up pass.

Behaviour-preserving throughout: **only DB-access routing changed.** No business
logic, schema, or audit-chain semantics were altered. The in-sandbox proof is
the cut-gate (CHECK 10f/10g) + negative probes + a Rust-aware structural sweep;
`cargo build --workspace` (both arms) + clippy + the DuckDB e2e are
**CI/Mac-deferred** to the chain's single end-of-chain 2-arm build-proof (this
sandbox has no Rust toolchain — same constraint as Session B).

## Done in Session C (gated)

- **serve.rs HTTP request handlers — every direct `Connection::open` migrated
  onto the shared `aberp_db::Handle`** (134 call sites): reads -> `db.read()`,
  writes -> `db.write()` (incl. `append_in_tx` audit writes); classified so no
  write is mis-routed as a read (which would bypass the post-commit durability
  hook -> Gap 2b) and no read mis-routed as a write (spurious checkpoint). This
  closes the **`Connection::open` half** of B's two-lock-regime window for the
  on-demand request path: post-C, **0** request-handler `Connection::open`
  remain in serve.rs (15 boot-create opens in `run`/`seed_demo_sample_data`
  stay, allow-listed; sequential, pre-serve-loop, before the Handle exists).
- **`write_shutdown_audit_entry`** additionally dropped its explicit
  `Ledger::open` + `sync_mirror` (a 2nd live opener) — the Handle's post-commit
  hook now syncs the mirror on guard drop.
- **Snapshot-EXPORT opener decision** (the opener B left out): kept as the one
  **sanctioned, allow-listed residual** (`crates/aberp-snapshot/src/take.rs`).
- **D5 gate finalized:** `cut_gate_db_isolation.sh` CHECK 10f (serve.rs,
  cfg(test)-aware + boot allow-list) and 10g (snapshot-EXPORT residual marker),
  with negative probes proving teeth (stray-in-handler trips; stray-in-test does
  NOT trip; marker-removal trips). Toolchain-free (bash/awk).

## Flagged residual #1 — handler-module internal openers (NOT migrated)

Serve request handlers call these module fns passing `db_path`; the fns
`Connection::open` internally. Same hazard class as the serve.rs opens, but
migrating them changes **public fn signatures** (`db_path: &Path` ->
`db: &aberp_db::HandleArc`) which ripples into each module's `#[cfg(test)]`
callers — un-verifiable without the deferred compiler, so flagged rather than
rewritten blind (a botched audit/business path in a defence ERP is worse than a
documented residual — ADR-0098's own ethos).

`Connection::open` (32): quality.rs 10, purchasing.rs 6, incoming_invoices.rs 6,
qc_inspection.rs 2, reports.rs 2, avl_vendors.rs 1, mes_manager.rs 1,
email_invoice.rs 1, quote_calibration.rs 1, ap_sync.rs 1, mark_invoice_paid.rs 1.

**Recipe (per fn):** change `db_path: &Path` -> `db: &aberp_db::HandleArc`;
internal `Connection::open(db_path)` -> `db.read()` (read) / `db.write()`
(write/append); update serve callers to pass `&state.db`; update `#[cfg(test)]`
callers with `aberp_db::Handle::open_default(&path, tenant)` OR a B-style
`#[cfg(test)]` path-shim (cf. B's `quote_pdf_rerender_daemon`). Then add each
file to gate CHECK 10d's `db_daemons` array (or a new 10h list).

## Flagged residual #2 — the `Ledger::open` audit seam (B->C->next)

`Ledger::open(path, ...)` internally `Connection::open`s. It is the audit-ledger
seam B explicitly flagged (the `AUDIT_APPEND_LOCK` regime; see `aberp-db`
lib.rs). The task's D5 hard-ban list is `Connection::open`/`open_with_flags`/
`append_reopen` and deliberately **omits `Ledger::open`**, so it is out of the
gate's hard scope and tracked here.

`Ledger::open` runtime sites: serve.rs 31; modules ~13 (incoming_invoices 3,
ap_sync 2, mark_invoice_paid 2, quality/purchasing/reports/avl_vendors/
email_invoice/quote_calibration 1 each).

**Recipe:** reads -> `Ledger::from_connection(db.read()?, tenant, hash)` (no
re-open — S375); audit appends -> `db.write()` + `append_in_tx(&tx, ...)` so the
post-commit hook fires (durable checkpoint + lockstep mirror), then drop the
`Ledger::open`/`sync_mirror`. `from_connection` already exists in audit-ledger.

## Flagged residual #3 — dual-use CLI+serve modules

`submit_invoice.rs` (3), `poll_ack.rs` (2), `print_invoice.rs` (2) are reached
from BOTH the serve process AND one-shot `aberp <cmd>` CLI subcommands. The
serve-reachable opens need the Handle; the CLI one-shot path has none. Resolve
by constructing a Handle in the CLI `run()` (`Handle::open_default(&args.db,
tenant)`) so all paths route uniformly — flagged for the compiler-available pass.

## Out of scope (correctly)

- **CLI-only one-shot subcommands** (11 modules with zero serve refs:
  observe_receiver_confirmation, poll_annulment_ack, request_technical_annulment,
  drain_pending_retries, recover_from_nav, drain_submission_queue, mark_abandoned,
  restore_from_nav_outgoing, retry_submission, submit_annulment,
  restore_from_nav_extract) — separate processes, not the in-serve concurrency
  window. Cross-process safety is a different mitigation, not this ADR.
- **MES boot/heartbeat** — not tenant-DB quote paths (per the plan).

## The one sanctioned, allow-listed residual

`crates/aberp-snapshot/src/take.rs` — the 4-h snapshot daemon's logical
read-only `EXPORT DATABASE`. Read-only table scan (+ a mirror-reconcile read);
never writes the live file nor touches the ART/checkpoint metadata locus of
`duckdb#23046`. Lowest-frequency opener in the process. Routing it through a
Handle quiesce-around-EXPORT would add a new public API and hold the writer
mutex for the entire multi-second EXPORT each cycle (an availability regression)
and change the durability core un-compilably here — the **less** conservative
option, FLAGGED as the path to full closure. Gate CHECK 10g asserts its
SANCTIONED-RESIDUAL marker so it cannot silently grow.
