# S335 / PR-32 — Email-outbox idle-audit throttle (+ persistent-conn refusal)

**Date:** 2026-06-10 · **Branch:** `session-335/pr-32-email-outbox-throttle-persistent-conn`
**Builds on:** `session-332/pr-31-...` (`4cb1805`) · **Targets:** the live DuckDB
ART error-spam Ervin is seeing on PROD_v2.27.14. Bundles with S332 → eventual
PROD_v2.27.15.

> **TL;DR.** The brief mandated two fixes. **Fix 1 (throttle idle
> `EmailOutboxFetched` emits) shipped** — it removes ~98% of the audit-write
> volume that drives the ART crash, and is the primary, unambiguously-safe fix.
> **Fix 2 (convert the daemon to a persistent audit `Connection`) is REFUSED
> with empirical evidence** — a coherence probe proves a persistent connection
> would *fork the tamper-evident hash chain and silently lose rows* whenever any
> other daemon writes concurrently. That is strictly worse than the contained
> log-spam it set out to fix. The reopen-per-write pattern every ABERP daemon
> uses is the *coherence mechanism*, not an oversight; the brief's premise
> ("siblings hold persistent connections; borrow the pattern") is factually
> inverted. Throttle alone resolves the live symptom.

---

## 1. Verify-first — per-file confirmation of S332's diagnosis

| Brief item | File:line | Confirmed |
|---|---|---|
| Unconditional idle emit | `email_outbox_poll_daemon.rs:528` (pre-edit) | `emit_fetched_audit(deps, fetched_count, …)` fired on every cycle incl. `fetched_count == 0`. ✔ |
| Per-cycle reopen + ensure_schema | `email_outbox_poll_daemon.rs:1018-1042` (pre-edit) | `Connection::open` + `ensure_schema` + 1-row tx + commit + drop, per write. ✔ |
| S332 context | `docs/findings/s332-duckdb-art-email-outbox.md` | Read in full; §8.1/§8.2 are this PR's mandate. ✔ |
| UNIQUE(seq)/(id) inline, do not touch | `crates/audit-ledger/src/storage/schema.rs:36-37` | Inline UNIQUE constraints; **untouched**. ✔ |
| Sibling persistent-connection pattern | `quote_pdf_rerender_daemon.rs:719`, `quote_pricing_pipeline.rs:311-891` | **NO sibling holds a persistent connection — all reopen per write.** Brief premise inverted. ✘ |

## 2. Fix 1 — throttle idle `EmailOutboxFetched` (SHIPPED)

`poll_once` now gates the success-path emit:

```rust
if fetched_count > 0 {
    deps.status.stamp_fetched_emit(now_dt);
    emit_fetched_audit(deps, fetched_count, since.clone(), &cycle_at).await;   // real work
} else if deps.status.heartbeat_due_and_stamp(now_dt, HEARTBEAT_INTERVAL) {
    emit_fetched_audit(deps, 0, since.clone(), &cycle_at).await;               // liveness heartbeat
} else {
    tracing::debug!(/* idle cycle throttled */);                              // silent
}
```

- **Real batches** (`fetched_count > 0`) always emit — work observability unchanged.
- **Errored cycles** still always emit (S311 F13/F18 silent-401 defence) — and now
  stamp the heartbeat clock so an erroring daemon's rows count as liveness.
- **Idle cycles** emit at most one `EmailOutboxFetched{fetched_count:0}` per
  `HEARTBEAT_INTERVAL`, otherwise a `tracing::debug!` line.

**Heartbeat cadence — flagged conservative call.** `HEARTBEAT_INTERVAL = 5 min`
(const, not operator-overridable in v1). Idle footprint drops from ~17k rows/day
(every 5s) to ~288 rows/day — a **~98% cut** — while still proving "daemon alive
and idle" inside a human-noticeable window. The first idle cycle after boot emits
one row immediately (`heartbeat_due(None,…) == true`) so a freshly-booted idle
daemon is never dark. Cadence is a judgement, not a derived value; if a need
arises it should be plumbed through `EmailOutboxPollDaemonDeps`, not an env knob
(avoids a second hot-reload surface).

**Unchanged:** audit event-schema, wire format, the `EmailOutboxFetched`
EventKind itself, the 5s poll cadence, the `audit_ledger` schema. No new EventKind
(per CLAUDE.md #13). The heartbeat reuses the existing empty-entries payload shape.

## 3. Fix 2 — persistent audit connection (REFUSED, with evidence)

### The probe
A throwaway coherence probe (deleted post-investigation; permanently re-encoded as
`tests/s335_email_outbox_audit_write_coherence.rs`) ran the exact scenario the
persistent-connection fix would create:

```
A wrote, A sees head seq = 1
B opened (separate conn), B sees head seq = 1, B wrote seq=2, B closed
after B closed, persistent A sees head seq = 1     ← STALE (did not see B's row)
A wrote again, A now computes seq = 2              ← REUSED B's seq = FORK
final rows = 2, distinct seqs = 2                  ← one of 3 writes LOST
VERDICT: INCOHERENT/FORK — persistent connection is UNSAFE
```

### Why
DuckDB `Connection::open` creates an **independent `Database` instance with no
shared buffer cache across handles** — documented at
`apps/aberp/src/incoming_invoices.rs:54-74` ("UNIQUE constraint does NOT fire
across two `Connection::open` handles … concurrent writers produce *tamper
detected at seq=1*"). A persistent connection held for the daemon's lifetime
never re-reads another daemon's committed rows, so `append_in_tx`'s
read-head-inside-tx computes a stale `seq`, the `UNIQUE(seq)` guard can't fire
across instances, and the tamper-evident chain forks — silently dropping a row on
the **crown-jewel ledger**. There is **no process-wide audit-write serializer**
across daemons (only `INGEST_SERIALIZER`, scoped to incoming-invoice ingest), so
this is a live hazard the moment two writers overlap — which a persistent
email-outbox connection guarantees (its window is always open).

### Why reopen-per-write is correct
Every ABERP daemon reopens per write *because* each fresh `Connection::open` reads
current on-disk state and computes the correct next `seq`. It trades the O(n²)
checkpoint cost for **cross-handle coherence on the tamper-evident ledger** — the
right trade. After Fix 1 the email-outbox daemon writes only on real work + a
5-min heartbeat (~288/day), matching the write profile of the sibling daemons
prod already tolerates; the O(n²) hot-loop condition is gone because the *every-5s
write* is gone.

`write_audit` keeps the reopen pattern, now with a comment block citing the probe
so a future contributor can't silently "optimize" it into a fork.

### The genuinely-safe durable path for Fix 2 (out of this PR's scope)
A persistent connection is only safe if **all** audit writers share **one**
serialized connection (a single dedicated audit-writer actor/task that owns the
sole `Connection` and other producers message into). That is a cross-cutting
architectural change touching every daemon + the invoice-issuance path — too broad
for a live hotfix (CLAUDE.md #3). Recommended as a dedicated follow-up session.
The throttle removes the urgency: idle ART pressure is already cut ~98%.

## 4. Tests (all green)

| Test | Pins |
|---|---|
| `email_outbox_poll_daemon::tests::s335_email_outbox_heartbeat_emits_at_cadence` | pure `heartbeat_due`: None→due, <interval→throttled, ≥interval→due, clock-skew→safe |
| `…::s335_heartbeat_due_and_stamp_fires_once_then_throttles` | handle stamps once/interval; `stamp_fetched_emit` resets the clock |
| `s335_email_outbox_idle_cycle_does_not_emit_fetched_audit` | 10 idle cycles → **1** fetched row (pre-S335: 10) |
| `s335_email_outbox_non_idle_cycle_does_emit_fetched_audit` | 2-entry cycle → 1 fetched row, `fetched_count == 2` |
| `s335_email_outbox_errored_cycle_emits_fetched_audit_with_errored_classification` | 401 cycle → 1 fetched row, `error_class == "auth_failed"` |
| `s335_email_outbox_idle_cycles_collapse_audit_writes` | 200 idle cycles → **1** write (perf delta) |
| `s335_reopen_per_write_interleaved_stays_coherent` | shipped pattern: interleaved reopen-writes → dense seqs, `verify_chain` clean |
| `s335_persistent_connection_forks_chain_documented_hazard` | refused pattern: persistent+transient interleave → row lost / seq collision |

**Perf delta proven:** `S335 PERF: 200 idle cycles → 1 audit write(s) in ~109ms`
(pre-S335 would be 200 writes, each an O(ledger) ART checkpoint).

No existing email-outbox test regressed (`s307_…full_cycle`, `s311_…stale_recovery`,
`s332_…no_crash` all green).

## 5. Conservative calls flagged

1. **Fix 2 refused** — persistent connection forks the chain (probe + Part-B test).
   Shipped the safe subset (throttle) only; proposed the serialized-writer follow-up.
2. **Heartbeat cadence = 5 min**, const, not operator-overridable in v1.
3. **No schema / no EventKind / no wire change** — emit-frequency only.
