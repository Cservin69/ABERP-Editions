# ADR-0052 — Chain-verify cadence for bulk ingestion paths: accept per-insert cost, log progress, never amortize

**Status:** Accepted — S198 / PR-198 (2026-05-31). Pins the posture that
S186 and S191 implemented in code (cache the cross-cycle dedup set;
spawn_blocking the per-page batch; keep per-insert chain-verify).
**Author:** Ervin Áben (ABERP), session 198 brief — close the 💭 question
raised by the S172-S181 adversarial review.
**Supersedes / amends:** none — additive pin on a cadence question the audit-
ledger contract (ADR-0008) left open.
**Related:** ADR-0008 (audit ledger — `verify_chain` contract), ADR-0019
(relational storage strategy — DuckDB single-file constraints), ADR-0030
(audit-ledger mirror file — additional verify-cost surface), ADR-0034
(recover-from-NAV — the "operator-paced go-drink-coffee" precedent), the
NAV-as-DR restore wizard (S180), the AP auto-sync daemon (S178).

## Context

`aberp_audit_ledger::Ledger::verify_chain` walks the full chain from genesis
to head, recomputing per-entry hashes and asserting each entry's `prev_hash`
matches the prior entry's `current_hash`. The walk is `O(N)` in the number of
entries; called after each insert in a bulk-ingestion path it becomes
`O(N²)`.

Two paths exercise the worst-case shape:

1. **AP auto-sync first-cycle ingest (S178 / `ap_sync.rs`).** Boot-tick on a
   tenant with a 30-day backlog of supplier-side digests can ingest hundreds
   of incoming invoices in one cycle. Each ingest opens DuckDB, INSERTs the
   `ap_invoice` mirror row, appends one `IncomingInvoiceIngested` audit
   entry, drops the connection, reopens, calls `verify_chain`, syncs the
   audit-mirror file (ADR-0030). Sequential per-digest.
2. **NAV-as-DR restore (S180 / `restore_from_nav_outgoing.rs`).** A
   first-ever restore of a 1000-invoice year walks NAV's `queryInvoiceDigest
   OUTBOUND` by month, processes each digest the same per-row way:
   `Connection::open` + INSERT into the `restored_invoice` mirror + audit-
   append + `verify_chain` + mirror-sync.

For a tenant with 10 000 prior audit entries and 1 000 new digests to
ingest, the chain-verify cost is roughly:

- 1st new entry: walk 10 001 entries.
- 1 000th new entry: walk 11 000 entries.
- Total: ~10.5M entry-hashes computed across the cycle.

The session 182 adversarial review asked the architectural question
directly: should we amortize (one verify per page, or per cycle), or accept
the cost as an "operator-paced go drink coffee" operation and document the
wait?

## Decision

**Accept the per-insert cost. Do not amortize.** Three reasons:

1. **The chain-verify contract IS the value proposition.** ADR-0008 commits
   to a tamper-evident audit ledger; the per-insert verification is what
   makes "tamper-evident" detectable at the granularity the operator can
   act on. Amortizing to per-page means a tamper introduced mid-page is
   detected only at end-of-page — and the entire page must be rolled back
   together (the page's earlier inserts hash-chain into the tampered entry).
   Per-insert verification keeps the rollback unit single-row, which matches
   the per-digest commit boundary the ingestion paths already use.
2. **Steady-state is near-zero new digests per cycle.** AP auto-sync's
   30-minute cadence sees ~0–5 new digests per cycle after the initial
   backlog drain. NAV-as-DR is run once per tenant per year (the operator
   re-issues from NAV when the local DB is wiped — a recovery event, not a
   recurring one). Optimizing the per-insert verify-cost for a path that
   runs at >1 ingestion per cycle <1% of the time is the wrong altitude.
3. **The pre-load HashSet cache + spawn_blocking already address the
   discoverable pain.** S186 added `load_already_restored_cache` so the
   per-digest idempotency check is O(1) `HashSet::contains` instead of a
   full-ledger walk. S191 wrapped the per-page DuckDB batch in
   `tokio::task::spawn_blocking` so HTTP handlers stay responsive during a
   restore. Both changes attacked the discoverable operator-visible symptom
   ("the SPA is frozen during a restore") without touching the verify
   cadence — the per-insert verify is now `O(N)`-on-a-blocking-thread, not
   `O(N)`-on-the-tokio-runtime.

### What the operator sees

For bulk paths, the operator gets:

- A `tracing::info!` line at the start of each month-walk (`restore_from_nav_
  outgoing::run`) and at the start of each AP-sync cycle (`ap_sync::run_cycle`)
  naming the expected workload (digest count) and a one-line "this is the DR
  / first-ingest path; expect minutes for large workloads".
- The cycle-completion audit entry
  (`IncomingInvoiceSyncCycleCompleted` for AP sync; the per-digest entries
  themselves for restore) records the elapsed time so a future ops dashboard
  can surface "this cycle took K ms" without instrumenting the daemon further.
- No SPA progress bar — the cycles complete in seconds to a few minutes for
  realistic workloads, and a progress bar's complexity (server-sent events,
  polling) is itself a CLAUDE.md rule 2 violation against the steady-state
  shape.

### What is explicitly NOT in this ADR

- A per-page verify amortization. Considered + rejected: changes the
  rollback unit, violates the audit contract, and the cache + spawn_blocking
  changes already eliminated the discoverable operator pain.
- A `--skip-verify` operator flag. Considered + rejected: an operator-
  toggleable verify is a tamper-detection gap waiting for an operator to
  forget the flag. The verify is non-optional by design.
- A background "verify-cycle" daemon decoupled from insert. Considered +
  rejected: introduces a write/verify race window (an insert lands at T,
  verify-cycle runs at T+30s) during which tamper would be undetected.
  The per-insert posture is the strictest detectable cadence.

## Consequences

### Wins

- ADR-0008's tamper-evident contract holds at the strictest cadence
  available (per-insert).
- The S186 cache + S191 spawn_blocking work continues to be sufficient — no
  follow-on PR is required to address the verify-cost concern at the
  architecture level.
- Operator-paced bulk paths are explicitly named in the daemon / wizard
  doc-comments, so future maintainers don't perceive the slow first-cycle as
  a regression and try to "fix" it.

### Trade-offs

- A pathological tenant (millions of historical audit entries + thousands of
  new digests in one cycle) would see per-insert verify dominate the cycle.
  No tenant is anywhere near this scale; the trigger to revisit is named in
  §"When to revisit" below.
- The per-cycle wall-clock for a 1000-row first-restore is in the minutes,
  not seconds. This is documented at the wizard's confirmation step (the
  RESTORE-token gate already names it as a rare DR operation).

### When to revisit

- A single AP-sync cycle exceeds 60 seconds in steady-state (operator
  reports "the sync daemon is blocking my SPA"; alarm fires from
  `IncomingInvoiceSyncCycleCompleted.duration_ms`).
- A single NAV-as-DR restore for a calendar year exceeds 10 minutes wall-
  clock (operator reports "the wizard's spinner spun for ages").
- A tenant adopts a use case that runs bulk ingestion more than once per
  day (would change the steady-state from "rare" to "recurring", inverting
  reason 2 above).

The trigger to revisit is the operator-visible symptom, not a synthetic
benchmark. If the cadence becomes painful in practice, the next step is to
measure where the time goes (verify-walk vs DuckDB write vs mirror-file
fsync) — not to assume the verify is the culprit.

## Adversarial review

- *"Per-insert verify on a tampered entry detects at entry N, but the
  operator-visible feedback is 'the ingest failed' — the tamper notice is
  buried in logs."* True today. ADR-0008's tamper-evidence is the
  detectability guarantee; the operator-facing surface for tamper alerts is
  deferred to a future ops-dashboard PR (no current trigger). The
  in-the-meantime posture is: tamper trips the per-insert verify, halts
  the ingest cycle, and leaves a loud `tracing::error!` in stderr. The
  cycle-completion audit entry is NOT written on a verify failure (the
  early-return path skips it), so a missing cycle-completion entry is
  itself a tamper signal.
- *"The cache hydrated at cycle-start can go stale within a cycle."* True;
  the cache is mutated in place as new restores succeed within the cycle,
  but a concurrent writer (a manual `/sync-now` racing with the daemon)
  would still race on the audit-ledger append. The INGEST_SERIALIZER
  process-wide mutex (S186) defends against that race. The cache is per-
  cycle scratch; the audit-ledger walk on next cycle re-hydrates from
  authoritative state.
- *"Why not parallelize across digests within a cycle?"* DuckDB's single-
  writer constraint (ADR-0019) plus the chain-hash's serial-by-construction
  shape forbids it. Parallelism within a cycle would require a different
  storage backend or a per-tenant write-serialization layer that doesn't
  exist today. Per ADR-0019, we don't have that, and we don't want it.

## Alternatives considered

- **One verify per page (S180's natural batch boundary).** Rejected per
  reason 1 — changes the rollback unit, weakens the tamper-detectability
  granularity. The per-page batch is a natural boundary for spawn_blocking
  (S191) and for the cache-mutation cadence; making it the verify boundary
  too would conflate three independent concerns.
- **One verify per cycle.** Rejected per reason 1, more strongly — the
  cycle boundary is arbitrary (sync runs every 30 minutes; restore runs
  once per year). Tamper introduced mid-cycle would be undetected for the
  remainder of the cycle.
- **Hash-chain skip-list / O(log N) verify.** Considered as a future
  optimization but explicitly not built now (CLAUDE.md rule 13 — delete
  before optimize; the operator-visible symptom is the trigger, not a
  synthetic O(N²) worry). If the §"When to revisit" trigger fires, this
  is the recommended direction — but the change is to ADR-0008's contract,
  not to this ADR.

## Invariants pinned

- The cycle-completion audit entry's `duration_ms` field is the canonical
  observability handle. `IncomingInvoiceSyncCycleCompletedPayload.duration_ms`
  exists today (S178); the restore path emits per-digest audit entries
  whose `time_wall` deltas allow a cycle-duration reconstruction.
- The `INGEST_SERIALIZER: Mutex<()>` (S186 / `incoming_invoices.rs`) holds
  for the full critical section (find-or-insert + audit-append + chain-verify
  + mirror-sync). A future PR that drops the mutex MUST also drop the
  per-insert verify (and re-decide this ADR).
