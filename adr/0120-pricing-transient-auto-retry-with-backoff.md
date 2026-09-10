# ADR-0120 — Pricing pipeline: bounded auto-retry-with-backoff for Transient failures (D-20 A4)

- **Status:** **Accepted** (2026-09-10) — passed adversarial review (two sweeps;
  the reap↔retry bounce and the RFC3339 string-compare designed out, the audit
  payload made deserialize-safe, four properties confirmed). Safe to build, in
  the sub-slices below.
- **Date:** 2026-09-10
- **Deciders:** Design pass + adversarial review, 2026-09-10.
- **Related:** the D-PRICEQ head-of-line incident + its hardening (the pricing
  daemon `apps/aberp/src/quote_pricing_pipeline.rs`), D-20 A1 (the stale-job
  reaper's `last_attempt_at` stuck-vs-starved fix), D-20 A2/A3, S290/PR-271
  (`classify_failure` + `FailureKind`), ADR-0094 (the no-new-`EventKind`
  blast-radius clause), `[[defense-pricing-queue-head-of-line-wedge]]`,
  `[[trust-code-not-operator]]`.

## Context

`classify_failure` labels every terminal pricing failure `Transient`,
`Permanent` or `Unknown`, and its verdict is stamped on the job row
(`failure_kind`) and audited (`QuotePricingFailureClassified`). But **nothing
consumes the label on any scheduling path**: `next_actionable_job` excludes
every `Failed` row, so a `Transient` failure (a network blip on the storefront
writeback, a briefly-full disk under the PDF write, a transient audit-tx fault)
waits for an operator Retry click exactly as a `Permanent` one does. A quote can
sit dead overnight because a volume was unmounted for a minute — against the
never-lose-a-job ethos the rest of this daemon is built to. `UNKNOWN_AUTO_RETRY_CAP`
is a constant with no reader.

**Why this needs its own design.** The scheduler is the exact surface the
D-PRICEQ head-of-line incident came out of. An auto-retry that re-enqueues at
the FIFO head, or does not cap, or cannot tell "retried and failed the same way"
from "not yet retried", re-introduces the wedge in a new costume. Four questions
must be answered concretely: **(1)** where the row re-enters the queue; **(2)**
how the attempt counter + backoff persist across restarts; **(3)** how the audit
chain records attempt *n*; **(4)** how it interacts with the A1 stale-job reaper
(a row being auto-retried is *moving*, so it must not be reapable — and must not
auto-retry forever).

### The daemon as it stands

- `poll_once`: list+enqueue `received` quotes → **reaper** (A1, condemns stale
  *started* rows) → **advance loop** (`next_actionable_job_excluding`, strict
  FIFO `ORDER BY fetched_at ASC LIMIT 1`, `MAX_JOBS_PER_CYCLE` per cycle, skips
  erroring rows within the cycle) → record a completed-cycle mark.
- `next_actionable_job`: `state IN ('fetched','extracting','pricing','rendering','posting_back')`.
  `Failed` is excluded.
- A failure calls `emit_failure` → row `Failed`, `error_stage`/`error_reason`
  set, `failure_kind` stamped, `QuotePricingFailed` + `QuotePricingFailureClassified`
  audited.
- Operator retry (`retry_job`): `Failed → Fetched`, clears the error columns +
  `failure_kind`, `attempt_n = attempt_n + 1`; it **preserves `fetched_at`** (so
  a retried row jumps to the FIFO head — the very starvation source A1 named).
- The reaper condemns only *started* non-terminal rows (`extracting`…`posting_back`)
  that are stale AND were reached this live-cycle window (`last_attempt_at`).

## Decision

Auto-retry re-enqueues an eligible `Failed` row through a **new, bounded pass**
that is deliberately NOT the advance loop and NOT the operator-retry head-jump.

### Schema (additive, both editions)

Two nullable columns on `quote_pricing_jobs`:

- `auto_retry_count INTEGER` — how many times the daemon has auto-re-enqueued
  this row. `NULL`/absent ⇒ 0. Distinct from `attempt_n` (which counts operator
  retries too) so the auto-retry CAP is independent of operator action.
- `next_retry_at VARCHAR` (RFC3339) — the instant this `Failed` row becomes
  eligible for auto-retry. `NULL` ⇒ not scheduled (a `Permanent` failure, a row
  that has exhausted its budget, or any non-failed row). Persisting the schedule
  in the row is what makes it **survive a restart**: on boot the sweep simply
  finds every `Failed` row whose `next_retry_at` has passed.

### When a failure is scheduled — and why NOT in `emit_failure`

Scheduling is done by a **separate step on the advance-loop failure path only**,
`schedule_auto_retry_if_eligible(...)`, called right after `emit_failure` when
`advance_one_step` returns `Failed`. It is **deliberately not inside
`emit_failure`**, because `emit_failure` is the single failure chokepoint and
the **A1 stale-job reaper calls it too** (`emit_failure(.., "reaper", ..)`). A
reaper reap classifies `Unknown` (the `classify_failure` fallthrough), so if
scheduling lived in `emit_failure`, the reaper would auto-schedule the very row
it just condemned as stuck, and the **same cycle's sweep (which runs right after
the reaper) would re-enqueue it** — a reap↔retry bounce, up to the Unknown cap.
Keeping scheduling on the advance-loop path means only a genuine pipeline-stage
failure schedules; a reaper reap (and `enqueue_failed_no_cad`, and any other
`emit_failure` caller) never does.

`schedule_auto_retry_if_eligible`, given the row's `failure_kind` +
`auto_retry_count`:

- `Permanent` ⇒ `next_retry_at = NULL`. Never auto-retried.
- `Transient` ⇒ if `auto_retry_count < MAX_TRANSIENT_AUTO_RETRIES` (5),
  `next_retry_at = now + backoff(auto_retry_count)`; else `NULL` (budget spent →
  operator only).
- `Unknown` ⇒ same, capped at `UNKNOWN_AUTO_RETRY_CAP` (3) — the constant that
  finally gets a reader.

`backoff(n)` is exponential with a ceiling: `min(BASE * 2^n, CEIL)` with
`BASE = 30s`, `CEIL = 15min`. Five Transient retries span ≈ 30s+1m+2m+4m+8m ≈
15m of wall clock — comfortably inside the reaper's `STALE_JOB_REAP_AFTER` (30m),
though (see below) a scheduled row is `Failed` and never a reaper candidate
anyway. A small deterministic jitter (±10%, seeded from the quote id) spreads a
thundering-herd of simultaneously-failed rows.

### The auto-retry sweep — where the row re-enters (Q1)

A new pass in `poll_once`, run **after the reaper and before the advance loop**,
bounded by `MAX_AUTO_RETRY_PER_CYCLE` (5):

- Select the `Failed` rows with `next_retry_at IS NOT NULL` (a non-null
  `next_retry_at` already means "scheduled AND under cap" — the cap was enforced
  when it was set, so the sweep needs no per-kind cap logic). **Parse and compare
  `next_retry_at` to `now` in Rust, not in SQL** — RFC3339 VARCHARs only sort
  lexicographically when their sub-second precision matches, the exact trap A1's
  reaper avoids by parsing `updated_at` in Rust; a `WHERE next_retry_at <= ?`
  string compare would fire early or late on a precision mismatch. Oldest
  `next_retry_at` first, bounded per cycle.
- For each due row: `state = Fetched`, clear
  `error_stage`/`error_reason`/`failure_kind`, `next_retry_at = NULL`,
  `auto_retry_count += 1`, and **`fetched_at = now`** — so the row re-enters at
  the **BACK** of the FIFO, never the head. This is the crux of not re-wedging: a
  serially-failing row keeps going to the back, so it can never hold the head
  against fresh work (unlike the operator retry, which preserves `fetched_at` on
  the operator's explicit "do this next" intent).
- Audit each re-enqueue (Q3, below).

Running the sweep before the advance loop means a re-enqueued row is in the
advance loop's actionable set the same cycle — but it enters at the BACK, so it
is only *processed* when it reaches the head behind older work; the sweep buys
eligibility, not a queue-jump. All of this is one DB write per re-enqueued row
under the shared Handle writer (ADR-0099), through `db.write()` + `append_in_tx`.

Because a re-enqueue resets the row to `Fetched`, the pipeline re-runs from the
start (extract → price → render → post) — the same restart the operator retry
does, and correct: extract/price/render are deterministic over the CAD, and the
writeback is idempotent on `feature_graph_hash`. A `posting_back`-stage
Transient failure therefore re-does the earlier stages' work; that waste is
bounded (≤5 retries) and buys the simplicity of one re-enqueue state. Resuming
from the failed stage is a possible optimisation, deliberately out of scope.

### Attempt counter + backoff persist across restart (Q2)

Both `auto_retry_count` and `next_retry_at` are columns, so a restart mid-backoff
loses nothing: the sweep re-derives eligibility from the row on the next cycle.
There is no in-memory retry state to reconstruct (contrast the reaper's cycle-mark
window, which is deliberately in-memory because it measures *this run's* liveness;
the retry schedule is durable because it measures the *row's* future).

### Audit of attempt n (Q3)

Each auto-retry re-enqueue emits **one** audit row. Per the ADR-0094
blast-radius clause we do **not** add a `QuotePricingAutoRetried` `EventKind`;
we reuse `QuotePricingFetched` (the row genuinely re-enters the `Fetched` state).
Crucially the payload carries the **full** `QuotePricingFetchedPayload` fields
(re-derived from the row: quote_id, tenant_id, customer_email, material_grade,
quantity, cad_filename, cad_local_path, idempotency_key, fetched_at) so a reader
deserializing the event as that struct never trips on a missing field —
`QuotePricingFetchedPayload` is not `deny_unknown_fields`, so the append also
carries the extra fields `{ auto_retry: true, auto_retry_count, failure_kind }`
and `actor: "daemon-auto-retry"` (distinct from the storefront-poll enqueue's
`"system"` and the operator retry's actor). Built as a `serde_json::json!` value,
not the bare struct, so the extras ride along. The failure that *preceded* the
retry is already its own `QuotePricingFailed` + `QuotePricingFailureClassified`
pair, so the chain reads: failed(k) → fetched{auto_retry, k→k+1} → failed(k+1) →
… → exhausted — a complete history an operator can read.

### Reaper interaction — moving ≠ reapable, and never forever (Q4)

No conflict, by construction:

- **A reaper reap never schedules an auto-retry.** Scheduling is on the
  advance-loop failure path, not in `emit_failure`, so the row the reaper
  condemns as stuck stays `Failed` with `next_retry_at = NULL` and does NOT
  bounce back through the sweep. The reaper's verdict is final (operator Retry
  only), as its backstop role intends. (Were scheduling in `emit_failure`, the
  reaper's `Unknown`-classified reap would auto-schedule and the same cycle's
  sweep would re-enqueue it — the bounce this ADR avoids.)
- A row **waiting out its backoff** is `Failed`. The reaper's candidate set is
  `started_non_terminal_jobs` (`extracting`…`posting_back`) — `Failed` is not in
  it. So a scheduled-but-not-yet-retried row is never reaped.
- A row **just auto-retried** is `Fetched`. Also not in the reaper's *started*
  candidate set (a `Fetched` row is queue-wait, not stuck — the exact case A1's
  `started_non_terminal_jobs` doc excludes). Once the advance loop picks it up
  and it enters `extracting`, it becomes a reaper candidate with a *fresh*
  `last_attempt_at` (A1's pickup stamp), so it is reaped only if it then genuinely
  sticks — which is correct.
- **Never forever:** the per-kind cap (`auto_retry_count < cap`) terminates the
  loop. A row that exhausts its budget stays `Failed` with `next_retry_at = NULL`
  until an operator Retry (which resets the budget). So auto-retry has a hard
  ceiling, and beyond it the honest operator-only fallback returns.

### Operator retry resets the budget

`retry_job` additionally sets `auto_retry_count = 0` and `next_retry_at = NULL`:
an operator's deliberate Retry is a fresh decision that restarts the auto-retry
budget (and, as today, jumps to the head on the operator's intent). Nothing else
about `retry_job` changes.

## Consequences

A `Transient`/`Unknown` pricing failure now self-heals — a network blip or a
minute-long disk-full resolves without an operator, up to a bounded number of
backed-off attempts, then falls back to the operator Retry it has today. The
`FailureKind` label and `UNKNOWN_AUTO_RETRY_CAP` finally drive behaviour. The
re-enqueue-at-the-back rule keeps the D-PRICEQ head-of-line property intact: no
retried row can hold the queue head against fresh work.

**Costs / what is locked in.** Two additive columns. A `Transient` row now
occupies the queue longer (it re-enters up to 5×) — bounded and at the back, so
it competes fairly. A genuinely-broken-but-Transient-classified failure (a
mis-classification) will retry 5× before resting Failed — wasted cycles, but
bounded, audited, and self-terminating; the fix for a systematic
mis-classification is `classify_failure`, not the retry budget.

## Adversarial review

One adversarial pass run against the draft (2026-09-10). It found one real
defect, now designed out, plus confirmations:

1. **FOUND — the reap↔retry bounce.** The draft scheduled inside `emit_failure`.
   But the A1 reaper calls `emit_failure(.., "reaper", ..)`, and a reaper reason
   classifies `Unknown` (verified: `classify_failure`'s fallthrough is
   `Unknown`), so the reaper would auto-schedule the row it just condemned, and
   the **same cycle's sweep** (which runs right after the reaper) would
   re-enqueue it — a bounce up to the Unknown cap, wasting cycles on a genuinely
   stuck row and undermining the reaper's backstop verdict. FIXED: scheduling
   moved to a separate `schedule_auto_retry_if_eligible` on the **advance-loop
   failure path only**; `emit_failure` (and thus the reaper) never schedules.
2. **FOUND — the RFC3339 sub-second string-compare trap.** A SQL
   `WHERE next_retry_at <= now` compares VARCHARs lexicographically, which A1's
   reaper deliberately avoids for `updated_at` because RFC3339 strings only sort
   chronologically when their sub-second precision matches. FIXED: the sweep
   parses `next_retry_at` in Rust and compares instants, exactly as the reaper
   does.
3. **CONFIRMED — no hot loop.** `next_retry_at` gates re-entry and `backoff(0)`
   is 30s, so the fastest a row can re-fail-and-re-enter is once per 30s, not
   once per cadence.
4. **CONFIRMED — operator Retry vs the sweep is race-free.** Both run under the
   shared Handle writer (serialised). An operator retry clears `next_retry_at`
   (the sweep then skips it); a sweep re-enqueue clears it too and moves to
   `Fetched`. No lost update.
5. **CONFIRMED — a restart storm is bounded.** `MAX_AUTO_RETRY_PER_CYCLE` (5)
   drains a backlog of due rows over cycles, and the jitter spreads their next
   schedule. The queue does not thrash.
6. **CONFIRMED — the reaper and sweep touch disjoint state in a fixed order**
   (reaper on *started* rows, then sweep `Failed → Fetched`), so neither
   preempts the other, and a row only becomes reapable once it re-enters a
   *started* state and genuinely sticks there (fresh `last_attempt_at`).

Second sweep:

7. **FOUND — reusing `QuotePricingFetched` could break a strict deserializer.**
   If the auto-retry emitted a `QuotePricingFetched` event with a payload missing
   the struct's required fields, a reader deserializing as
   `QuotePricingFetchedPayload` would fail. FIXED: the auto-retry payload carries
   the FULL set of `QuotePricingFetchedPayload` fields (re-derived from the row)
   plus the extras; the struct is not `deny_unknown_fields`, so the extras are
   tolerated and the required fields are all present.
8. **CONFIRMED — a re-enqueued row whose CAD blob vanished self-limits.** It
   re-runs from `extract`, which fails `"cad file missing"` → `Permanent`
   (verified token), so it does not auto-retry again — it rests `Failed` for the
   operator, not a loop.
9. **CONFIRMED — additive & backward-compatible.** `auto_retry_count` NULL ⇒ 0
   (`COALESCE`), `next_retry_at` NULL ⇒ unscheduled; legacy rows read correctly.

## Alternatives considered

- **Include `Failed`+`Transient`+due rows directly in `next_actionable_job`
  (no separate sweep, no state change).** Rejected: it entangles the retry
  policy with the strict-FIFO advance query (the D-PRICEQ incident surface), and
  a `Failed` row surfacing in the advance loop would need the loop to understand
  retry eligibility + backoff — exactly the scheduler complexity the separate,
  auditable sweep keeps out of the hot path.
- **Re-enqueue at the FIFO head (reuse the operator-retry path).** Rejected
  outright: it is the head-of-line wedge in a new costume — a serially-failing
  Transient row would hold the head every backoff interval.
- **A new `QuotePricingAutoRetried` EventKind.** Rejected per ADR-0094: a new
  variant in a ~190-variant non-`#[non_exhaustive]` enum matched across crates
  the sandbox cannot compile-verify. Reusing `QuotePricingFetched` with a
  self-describing payload is a pure relabel if a dedicated kind is ever wanted.
- **Bump `attempt_n` instead of a separate `auto_retry_count`.** Rejected: it
  would make an operator retry count against the auto-retry cap (and vice
  versa), so a row an operator retried 5× could never auto-retry, and the cap
  would not mean "auto-retries". Two counters keep the two budgets honest.

## Open questions

- **Q1 — the constants.** `MAX_TRANSIENT_AUTO_RETRIES = 5`, `BASE = 30s`,
  `CEIL = 15min` are proposed, not measured. They are chosen to fit inside the
  reaper's 30-minute window with margin; real storefront-blip durations could
  tune them. Constants, easily changed; no schema impact.
- **Q2 — should a Transient failure at the writeback stage, where NAV has
  already accepted the invoice, auto-retry?** The pricing pipeline is a QUOTE
  path (no NAV filing), so this is moot here; noted so a future reader does not
  assume the money-CLI semantics apply.
- **Q3 — surfacing auto-retry state in the operator panel.** The SPA could show
  "auto-retrying (attempt 2/5, next in 4m)". Additive UI, out of scope for the
  scheduler slice; a follow-on.
