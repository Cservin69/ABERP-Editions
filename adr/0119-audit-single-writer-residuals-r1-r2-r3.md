# ADR-0119 — Audit single-writer: closing the three residuals (D-21 / ADR-0099 §R2.8)

- **Status:** **Accepted** (2026-09-09) — passed adversarial review (two sweeps;
  three corrections folded in, three concerns closed, no fundamental hole). Safe
  to build against, in the R2 → R1 → R3 order, each sub-slice gated and landed
  independently.
- **Date:** 2026-09-09
- **Deciders:** Design pass + adversarial review, 2026-09-09.
- **Related:** ADR-0099 (§Deferred + §R2.8 — names these three residuals and
  flags the whole-DB flock for v0.2.9), ADR-0105 (`Handle::with_ledger`, the
  lock-domain unification these build on), ADR-0098 (one shared instance),
  ADR-0087 (the `append_in_tx_signed` chokepoint), `submission_lock.rs` (the
  fs2 flock pattern R1 clones), `[[trust-code-not-operator]]`,
  `[[no-sql-specific]]`.

## Context

The audit ledger is ABERP's tamper-evident spine, and a **forked `seq`** — two
rows both taking `seq = head + 1` — is the failure this whole line of work
exists to prevent (the seq-369/416/428/515 prod signature). ADR-0099 + ADR-0105
drove the **in-process** write-fork surface onto one shared `aberp_db::Handle`
and unified the two append-lock domains behind `Handle::with_ledger`. Cut-gate
**CHECK 10M / 10N / 10P** (`tools/adr0099_audit_writer_scan.awk` +
`tools/adr0099_audit_writer_residuals.txt`) classify every runtime audit-write
site and fail the build on any that cannot prove its serialization domain.

ADR-0099 §R2.8 names three honest residuals that the gate **classifies but does
not structurally prevent**. None is implicated in any incident to date; each is
a way the fork class could return. D-21 closes all three.

### The architecture as it stands

- **Two lock domains, one bridge.** `Handle::write` serializes handle-routed
  writers behind the **writer mutex**; `append_in_tx` (the ~100+ legacy callers)
  runs under it and takes no lock of its own. `Ledger::append` / `append_signed`
  take the audit-ledger's process-wide **`AUDIT_APPEND_LOCK`**. The two are
  DISJOINT. `Handle::with_ledger` is the only construct that holds BOTH (writer
  mutex + `AUDIT_APPEND_LOCK`, via a `try_clone` of the shared instance) — it is
  what closed the ADR-0105 fork.
- **`append_reopen` is dead.** Zero live callers; CHECK 10M fails any new one.
  It is not part of any residual — noted so the design does not spend effort on
  a corpse.
- **Cross-process writers** (the `aberp <subcommand>` one-shots) share no
  in-process lock with `serve`, by construction. The **mirror** half of that
  race is already closed — `sync_mirror_lockstep` takes a genuine cross-process
  `fs2` flock (ADR-0099 R3.5). The **table** half is backstopped by hash-chain
  detection alone.

### The three residuals

- **R1 — cross-process table-side fork is detection-only.** A CLI subcommand
  writing the audit table while `serve` holds the DB is outside every in-process
  lock. ADR-0099 §Deferred already prescribes the fix: a **whole-DB
  cross-process advisory lock** (fs2 flock, pattern `submission_lock.rs`) so a
  CLI **refuses** while `serve` holds it rather than racing it. Design content:
  what a refused CLI prints, and whether `serve` must publish liveness.
- **R2 — `serve.rs::run` is allow-listed wholesale.** A fork planted anywhere in
  the ~3,000-line boot fn passes 10M/10N/10P. The exemption is justified (it runs
  before `open_tenant_handle`, single-threaded, no daemons spawned) but it
  covers the whole of `run`, not the handful of boot steps that actually write
  audit. Fix: extract those steps into named, individually allow-listed fns.
- **R3 — `append_in_tx` takes no lock; the two domains stay distinct.** CHECK
  10P's `LEDGER_LOCKED` verdict is a *classification* — "this site holds the
  append lock end-to-end" — not a *proof* that a `Ledger`-domain writer cannot
  race a handle-domain one. `with_ledger` is still the only construct holding
  both.

## Decision

Three independent sub-slices, landable and gated separately, smallest-risk
first. **R2 → R1 → R3.**

### R2 — extract the pre-Handle boot sequence (smallest, do first)

`serve.rs::run`, before `open_tenant_handle`, performs a short, fixed sequence
of steps — some of which append audit (the boot mirror reconcile;
`record_upgrade_snapshot_mismatch_audit`; the snapshot recovery staging in
`aberp-snapshot/src/recover.rs`). These run single-threaded with no Handle and
no daemon, so they genuinely cannot fork — but the allow-list entry
`serve.rs:run` grants the whole 3,000-line function that exemption, so a fork
planted *anywhere* in `run` (including code added years later, after daemons
exist) passes all three gates.

**Fix (two load-bearing halves):**

1. Extract **every** pre-Handle audit-writing boot step out of `run`'s body into
   named free fns — e.g. `boot_reconcile_mirror`, `boot_record_upgrade_mismatch`,
   `boot_stage_recovery` — each doing exactly one step, and allow-list *those*
   names instead of `run`. `run` is then removed from the allow-list, so any
   audit write left in (or added to) `run`'s body becomes RED. Completeness is
   self-enforcing: if I miss one, removing `run` from the allow-list reds the
   build until it too is extracted.
2. **Anti-rot caller-set pin (not optional).** Extracting a fn only preserves the
   "provably before-Handle" property if that fn is *called only from `run`,
   before `open_tenant_handle`*. If a daemon later calls `boot_reconcile_mirror`,
   its exemption is false and the hole is back — the ADR-0116-rev3 failure mode
   (a fix that deletes its own detector). So the scanner gains a **caller-set
   assertion** (the CHECK 10N mechanism): each extracted boot fn must have
   exactly one caller, `run`, and the gate reds a second caller. The exemption is
   thereby bound to "the one pre-Handle boot call", not to the fn name.

Otherwise mechanical (no behaviour change); touches `serve.rs` + the residuals
file + the scanner. Gate: the full suite proves no behaviour change; one NEW test
plants a bare audit write in `run`'s body and asserts the scanner now reds it
(was green under the `run` exemption); a second adds a second caller of an
extracted boot fn and asserts the caller-set pin reds it.

### R1 — the whole-DB cross-process advisory lock

A new module `apps/aberp/src/db_lock.rs`, a direct structural sibling of
`submission_lock.rs`, exposing a **whole-DB** (not per-invoice) exclusive
advisory lock keyed on the tenant DB path:

- Lock file `.aberp-db.lock` next to the tenant `*.duckdb` (the cross-process
  rendezvous point — one DuckDB file per tenant, ADR-0002), tenant-sanitised
  exactly as `submission_lock.rs` sanitises.
- `aberp serve` acquires it **exclusive, blocking-with-timeout at boot** and
  holds the guard for its whole lifetime (stored in `AppState`, dropped at
  shutdown). Holding it IS the liveness signal — **no separate liveness file is
  needed**: `fs2`/flock releases on process death (clean exit, crash, or kill),
  so a CLI that finds the lock free knows no `serve` is live on this tenant.
  This is the answer to "whether serve must publish liveness": the lock is the
  liveness.
- Every **audit-writing** CLI one-shot (`drain_*`, `retry_*`, `submit_*`,
  `*_annulment`, `mark_abandoned`, …) calls `db_lock::try_acquire` FIRST. On
  `Ok(None)` (serve holds it) it **refuses** with a specific, actionable message
  (see below) and a non-zero exit; on `Ok(Some(guard))` it holds the guard for
  its whole run (which also excludes a concurrent second CLI and a `serve` boot
  — a bonus that closes CLI-vs-CLI table forks too). **Read-only** CLIs
  (`verify-chain`, `entries`, list/query subcommands) never append and are NOT
  gated — they may run against a live serve.

**The refusal message** (the design content). A refused CLI must not read as a
bug. It names the contention and the resolution:

> `aberp serve is running on tenant <t> (holds the database lock at <path>).`
> `This command writes the audit ledger; running it alongside serve could fork`
> `the tamper-evident chain, so it is refused. Quit the desktop app (or stop`
> `aberp serve) and re-run. Normal submission and ack happen in serve`
> `automatically; use this only for manual queue recovery while serve is down.`

This changes the posture `submission_lock.rs` was built on — that operators run
drain/retry CLIs *while serve is live*. Be precise about what `serve` does and
does not do (verified against `serve.rs`), because the honest version is narrower
than "serve does that work anyway":

- `serve` DOES handle the **normal** submission path in-process: auto-submit on
  issue, and `run_nav_poll_daemon` polling NAV for acks. For those, a manual CLI
  while serve is up was always redundant.
- `serve` does NOT run `drain_submission_queue` / `drain_pending_retries` /
  `retry_submission` — those are **break-glass queue-recovery** tools with no
  serve-side loop. R1 makes them **break-glass-when-serve-is-down**: to drain a
  stuck queue by hand, the operator quits the app first. This is a real
  operational-model change, owned by the refusal message, and it is the posture
  ADR-0099 §Deferred already prescribed when it flagged this lock ("so a CLI
  refuses while serve holds the DB"). Whether `serve` should *also* grow an
  in-process drain/retry loop so the break-glass path is rarely needed is
  **Open Q4** — a separate enhancement, not a blocker for closing the fork.

The per-invoice `submission_lock` stays as the in-process + CLI-vs-CLI backstop
for the serve-down window; it is not removed (defence in depth), it is simply no
longer the *only* guard.

**Coordination with the tracked S386 guard.** `serve.rs:9457` tracks a
"flock/pidfile single-instance guard (S386)" that was never built. R1's
whole-DB exclusive lock IS a single-`serve`-instance guard as a side effect
(two serves cannot both hold it), so it **subsumes S386** — we build one lock,
not two overlapping ones. It is independent of the DB-row restore lock
(`serve.rs:21191`), which coordinates a different thing (mid-restore recovery)
and stays as is.

`serve`'s own boot acquire is the mirror image: it takes the lock
blocking-with-timeout, and on timeout (a CLI is mid-run) **refuses to boot**
loud, naming the lock path and the likely CLI — never boots unsynchronised. A
short CLI one-shot clears in well under the timeout; a hung one is an operator
`lsof`, exactly as `MirrorLockTimeout` is.

Gate: a test acquires the lock (simulating serve), then asserts an audit-writing
CLI entry point returns the typed `DbLocked` refusal (not a race, not a silent
skip); a second asserts a read-only path is unaffected; a third asserts the lock
auto-frees when the holder drops (process-death-release proxy).

### R3 — collapse to one serialization domain (the "large" one, reframed)

ADR-0099 §R2.8 frames R3's fix as making the chokepoint acquire one lock, with a
public/`_locked` split and a **commit-spanning** hold — an API change at ~100+
`append_in_tx` call sites. **We propose a smaller, equally sound fix** and put
the maximal one as the considered alternative, because the evidence changed the
cheapest safe closure:

- Every `append_in_tx` caller already runs under the **writer mutex** (they are
  reached through `Handle::write`), so Domain A is already single-writer,
  head-read-through-commit, with no change.
- `append_reopen` (the other `AUDIT_APPEND_LOCK` path) is **dead**.
- Therefore the ONLY way the two domains can still race in-process is a **direct
  `Ledger::append` caller that does not go through `with_ledger`** — a Domain-B
  writer holding `AUDIT_APPEND_LOCK` but NOT the writer mutex, concurrent with a
  Domain-A writer.

**Fix:** leave every in-process Domain-B writer holding the **writer mutex**, so
there is one in-process serialization domain. Each direct in-process
`Ledger::append` caller migrates to the path that matches what it does — and the
distinction matters:

- **Business + audit atomic** (the caller writes a business row and the audit
  entry in one unit — `issue_storno`, `issue_modification`,
  `export_invoice_bundle`, `submission_queue`, the poll/observe daemons):
  route through **`Handle::write` + `append_in_tx`** (Domain A). `with_ledger` is
  the WRONG target here — it hands back an audit-only `Ledger` clone on a
  *separate* try_clone, which would split the business write and its audit into
  two connections/txs and break their atomicity. The business+audit sites belong
  on the writer-mutex tx path, full stop.
- **Audit-only session ops** (`open_service_session`, `heartbeat`,
  `recover_crashed_sessions` — the `&mut Ledger` api that genuinely cannot borrow
  the handle's connection): route through **`with_ledger`** (holds both locks).

Either way the in-process caller ends up under the writer mutex, so no
in-process Domain-B writer runs unguarded. Each migration preserves the caller's
exact append variant — an unsigned `Ledger::append` becomes `append_in_tx`, a
signed one becomes `append_in_tx_signed` with the same `SessionContext` — so the
audit row (including its `event_sig`/`session_id` columns) is byte-identical;
the verified migration sites (`issue_storno`, `issue_modification`, …) use the
unsigned `Actor::from_local_cli` form, so `append_in_tx` is the target there.

Then **strengthen CHECK 10P**: for the **in-process** surface, turn the
classification into a standing proof — a direct `Ledger::append` /
`Ledger::from_connection(..).append` that is not inside `with_ledger` and not on
the frozen residuals list reds the build. Be honest about the limit: the text
scanner has no call graph, so it cannot itself *compute* "in-process vs
CLI-only". The **CLI one-shots** (which have no Handle and legitimately use
`Ledger::append`) therefore stay **frozen-list-sanctioned** — but they are no
longer bare: R1's whole-DB flock is what now guards them cross-process, so the
frozen list is backed by a real exclusion, not just a rationale. Net: the
**in-process** cross-domain race becomes structurally impossible and gate-proven;
the **CLI** surface moves from detection-only to flock-guarded. That is the whole
of the residual, closed from both sides.

This is bounded by the number of *direct `Ledger::append`* in-process sites (a
handful of files), not the ~100+ already-mutex-guarded `append_in_tx` sites — an
order of magnitude smaller than §R2.8's framing, and it *removes* the racing path
rather than adding a lock the ~100 sites must remember to take. Each migrated
file is its own bounded sub-slice, golden-guarded for byte-identical audit
output, committed and gated independently.

**If a business+audit caller cannot be put on the writer-mutex tx path** (e.g.
it interleaves a `Ledger` session op it cannot restructure), that caller alone
stays a sanctioned residual and the per-holdout FALLBACK is the §R2.8
commit-spanning lock — an `append_in_tx_locked` / `append_in_tx` split whose
public form takes a unified append lock across the caller's commit. It is never
applied to the ~100 already-guarded sites. Any holdout is re-surfaced (Open Q3),
not silently widened.

## Consequences

The three ways the audit-fork class could return are closed: a CLI can no longer
race `serve` on the audit table (R1), a fork can no longer hide in the boot fn's
exemption (R2), and the last in-process cross-domain race is removed at the
source with the gate upgraded to prove it (R3). The tamper-evident spine — a
product selling point — gains a structural, gate-enforced single-writer
guarantee rather than a classified-safe one.

**Costs locked in.** Audit-writing CLIs no longer run alongside a live `serve`;
this is a deliberate posture change (the refusal message owns it) that operators
must learn — mitigated because `serve`'s own daemons already do that work. R1's
lock adds one flock acquire to `serve` boot and each audit CLI; the boot
acquire is bounded-with-timeout and fails loud (never proceeds unsynchronised).
R3's migration changes the connection provenance of a few audit writers; each is
golden-guarded for byte-identical audit output.

## Adversarial review

One adversarial pass run against the draft (2026-09-09). It did not sink the
design but corrected three things now folded into the Decision above; recorded
here so the corrections are not re-litigated:

1. **"R1's claim that `serve` already does the drain/retry work is false."**
   FOUND: `serve.rs` spawns the nav-poll ack daemon and the email-relay drain,
   but has NO `drain_submission_queue` / `drain_pending_retries` /
   `retry_submission` loop — those are break-glass CLIs. The draft's "serve does
   that anyway" would have mis-sold a real operational-model change. FIXED: R1
   now states precisely what serve does (auto-submit + ack) and does not (queue
   recovery), owns the change in the refusal message, and files serve-side
   auto-drain as Open Q4. The refuse posture itself stands — it is what ADR-0099
   §Deferred already prescribed — but honestly, not on a false premise.
2. **"R3 migrating business+audit callers to `with_ledger` would BREAK their
   atomicity."** FOUND: `with_ledger` hands back an audit-only `Ledger` on a
   *separate* try_clone; a caller that writes a business row + its audit entry in
   one unit would be split across two connections. FIXED: R3 now routes
   business+audit atomic callers to `Handle::write` + `append_in_tx` (Domain A,
   one tx), and reserves `with_ledger` for the audit-only `&mut Ledger` session
   ops. Wrong target in the draft; corrected before any build.
3. **"R3's 'proof not classification' overclaims — the scanner has no call graph,
   so it cannot prove the CLI sites are cross-process."** FIXED: R3 now scopes the
   proof to the IN-PROCESS surface (structurally empty, gate-enforced) and is
   explicit that the CLI Domain-B surface stays frozen-list-sanctioned — but now
   backed by R1's flock rather than a bare rationale. The claim is exactly as
   strong as it can honestly be.

Concerns that survived without a change:

4. **"R1 exclusive whole-DB lock for serve's lifetime — does it break a
   legitimate reader?"** No. Read-only CLIs never append and are not gated
   (`verify-chain` etc. run against a live serve). A second `serve` on the same
   tenant was already out of scope and is now *closed* as a bonus (two serves
   cannot both hold the exclusive lock).
5. **"Process-death-release of flock across kill -9 / OOM?"** Reliable: the
   kernel releases flock on fd close, and all fds close on process death by any
   signal — the same guarantee `submission_lock.rs` and `sync_mirror_lockstep`
   already depend on. The one residual is a *hung* (not dead) serve; the CLI's
   refusal names the lock path for an `lsof`, as `MirrorLockTimeout` does.
6. **"R3 rests on a grep (append_reopen dead, append_in_tx all mutex-guarded) —
   grep rots."** The ADR does not rest on the grep: the strengthened CHECK 10P
   is the standing proof — a future direct in-process `Ledger::append` or a
   revived `append_reopen` caller reds the build. The grep motivated the plan;
   the gate enforces it.

## Alternatives considered

- **R3 as the §R2.8 commit-spanning lock at all ~100+ sites.** Rejected as the
  primary plan: it adds a lock every caller must remember to take and hold
  across commit (a discipline that rots), where domain-collapse *removes* the
  racing path entirely and is an order of magnitude smaller. Kept as the
  per-holdout fallback only.
- **R1 as a shared/exclusive scheme (serve holds shared, CLI holds shared, both
  coexist).** Rejected: a shared lock does not exclude two audit-table writers,
  so it does not close the fork — the whole point is exclusion.
- **R1 via a PID/liveness file instead of a held flock.** Rejected: a liveness
  file is a TOCTOU (serve dies between the CLI's read and its write) and needs
  stale-file reaping; a held flock is self-cleaning on death and race-free.
- **Leave the residuals as classified-safe (status quo).** Rejected: D-21's
  premise is that a classification is not a proof, and the fork class recurred
  four times. The spine deserves a structural guarantee.

## Open questions

- **Q1 — R1 refusal ergonomics.** Should an audit-writing CLI offer a
  `--force-when-serve-down`-style override, or is the plain refusal enough? Lean
  plain refusal (an override re-opens the hazard); revisit if operators hit a
  real serve-down-but-lock-held case.
- **Q2 — R2 anti-rot invariant.** The extracted boot fns must be provably called
  only from `run` pre-Handle. What does the scanner assert to keep that true (a
  caller-set pin, like CHECK 10N's), so the extraction cannot become a new
  wholesale exemption?
- **Q3 — R3 holdouts.** Which direct `Ledger::append` callers, if any, cannot
  route through the writer-mutex tx path (Domain A) or `with_ledger`, and
  therefore force the commit-spanning-lock fallback for themselves? Enumerated
  during the R3 sub-slices; any holdout is re-surfaced rather than silently
  widening the change.
- **Q4 — should `serve` grow an in-process drain/retry loop?** R1 makes the
  drain/retry CLIs break-glass-when-serve-down. A serve-side loop (like the
  existing nav-poll / email-relay daemons) would make the break-glass path rare,
  softening the operational-model change. A separate enhancement, deliberately
  NOT bundled into the fork closure; flagged for Ervin's call on priority.
