# ADR-0096 — Prod backport of the editions crash-safe durability hardening: deferred, with explicit trigger criteria

- **Status:** Accepted (decision in force today: do **not** backport now; criteria below trigger a reopen. Adversarial review included per the lifecycle in `adr/README.md`.)
- **Date:** 2026-06-27
- **Deciders:** Ervin
- **Extends:** ADR-0095 (the editions crash-safe durability hardening this decision is about whether to backport), ADR-0093 (the saw-off that froze prod and created the editions tree)
- **Grounds / related:** ADR-0082 (validated logical snapshot system — prod's recovery substrate), ADR-0008 (audit hash-chain ledger + append-only JSONL mirror — prod's second source of recovery truth), the **2026-06-22 prod corruption recovery record** (the manual procedure prod still relies on), `SAW-OFF.md` (the prod baseline + the byte-for-byte untouched invariant), `duckdb/duckdb#23046` (the torn-write / ART-checkpoint family), `[[trust-code-not-operator]]`.

## Context

ADR-0095 (implemented in Sessions A + B) makes the **editions** tree (Defense,
Portable) crash-safe: atomic DB creation, boot safe-open + guarded auto-recovery,
live-path durable checkpoints, and a single supported `aberp recover` command. It
lands in the editions tree only; the `aberp-snapshot` crate refuses any prod path
(`ensure_not_prod_path`), so the change is mechanically prod-safe.

Prod is the frozen, unified legacy line:

| Ref | Value |
|---|---|
| Prod branch / tag | `PROD_v2.27.76` |
| Prod commit SHA | `f7519b4077fa9af4f3c7949e58aa29f4268ff9e9` |
| Prod **tree-hash** (content identity) | `2d612811dd487a50f33476c484d1768cc8e99a51` |
| Source `main` fork point | `2bd2adff51737e3eb9729dbc325db0a16bf238e4` |

Prod runs DuckDB with the **same default in-place checkpoint**, so it carries the
**same latent torn-write vulnerability** (`duckdb#23046`) that ADR-0095 hardens the
editions tree against. The 2026-06-22 incident was prod hitting exactly this class
(zeroed in-DB blocks: the ledger mirror table + part of `ap_invoice`); it was
recovered **by hand** using the snapshot + ledger-replay method that ADR-0095 now
automates for editions.

The question this ADR settles: **do we backport the ADR-0095 hardening to prod now?**

## Decision

**No — do not backport to prod now.** Prod stays **frozen** at `PROD_v2.27.76`,
byte-for-byte untouched, running "invoicing only," and protected by the **existing,
proven** recovery posture: ADR-0082 validated snapshots + the ADR-0008 append-only
audit-ledger mirror, recovered via the **manual** 2026-06-22 procedure (preserve →
restore snapshot → replay ledger → validate → install; reversible, evidence
retained).

The ADR-0095 hardening remains **editions-only**. The mechanical guarantee that it
cannot touch prod (`ensure_not_prod_path`, and `--store`/`--to` refusing any prod
path) stays in force as defense in depth.

This decision is **revisited only** when one of the trigger criteria below is met.
A backport, if it happens, is **its own separate, gated change** — a new ADR, a
dedicated change-window, and full re-validation — never a side effect of other work.

### Why deferring is acceptable (the case for "no")

- **Prod is frozen and its write surface is small.** "Invoicing only," no new
  feature work; fewer writes means fewer checkpoints, the narrow window where the
  torn-write defect can bite.
- **The existing recovery posture is proven.** 2026-06-22 demonstrated that the
  manual snapshot + ledger-replay path recovers prod with **no lost committed
  entry** and is fully reversible. ADR-0095 automates that same procedure; it does
  not invent a safer one. Prod already has the substrate (snapshots + mirror).
- **Touching a frozen line is itself a risk.** A backport edits prod's code and
  boot path. A buggy auto-recover is worse than a refuse (ADR-0095 adversarial #1).
  Porting onto prod's own build/libduckdb requires its own full crash-injection
  validation; doing that without a driver is how you turn a latent risk into an
  active one.
- **It breaks the saw-off invariant.** ADR-0093 / `SAW-OFF.md` make
  "prod byte-for-byte untouched" a load-bearing guarantee (verified by
  `PROD_v2.27.76^{tree} == 2d612811…`). Any prod change deliberately spends that
  invariant; there must be a reason worth spending it on.

### Why it is not free (the residual risk we are accepting)

State it plainly: **prod can still suffer a torn-write corruption**, and when it
does, recovery is **manual** — higher MTTR, operator-dependent, and exactly the
`[[trust-code-not-operator]]` gap ADR-0095 closes for editions. The exposure is:

- A recurrence means **unbounded downtime + hand-surgery** until an operator runs
  the procedure, versus automatic, near-zero-touch recovery on editions.
- The probability is **bounded but not zero** — bounded by prod's frozen,
  low-write, invoicing-only posture; not zero because the defect is in DuckDB's
  in-place checkpoint, which every prod write still uses.
- It depends on the snapshot store + mirror remaining present and current on prod.
  **Mitigation we require while deferred:** keep prod's periodic snapshots running
  and verify the `<db>.audit.log` mirror is intact, so the manual recovery inputs
  always exist. (No code change — operational hygiene only.)

We judge this acceptable **for a frozen line** specifically because the downside is
*recoverable* (manual, but proven) rather than *data-loss*, and because the cost of
the alternative — unfreezing prod to ship code — is higher than the bounded risk.

## Trigger criteria — when this decision is reopened

Any **one** of the following flips this ADR from "deferred" to "file the backport
ADR":

1. **Prod un-freeze.** Prod resumes feature work, or a successor `PROD_vX` line is
   cut. A live line should not ship on a known latent corruption bug.
2. **A prod corruption recurrence.** A second torn-write / `LoadCheckpoint` /
   ahead-mirror incident on prod after 2026-06-22. One incident was tolerable on a
   frozen line; a recurrence changes the expected-cost calculation.
3. **Prod write-surface expansion.** Prod's writes materially grow beyond invoicing
   (wider/heavier checkpointing → larger torn-write window).
4. **Manual recovery proves insufficient.** Any case where the manual procedure
   loses a committed entry, cannot complete, or its MTTR is judged unacceptable for
   prod's role.

Until a trigger fires, the `aberp recover` engine and ADR-0095 wiring remain
**editions-only**.

## How a backport would be done (so the deferral is concrete, not vague)

A future backport is a scoped, gated change, roughly:

1. **Its own ADR**, superseding/extending this one, recording the trigger that
   fired and the scope.
2. **A dedicated change-window** in which the "prod untouched" invariant is
   *explicitly and temporarily lifted* for prod only — recorded, time-boxed, and
   re-frozen at a new `PROD_vX` baseline afterward (with `SAW-OFF.md`'s prod
   baseline table updated to the new tree-hash).
3. **Port, don't rewrite:** carry over `aberp-snapshot::{recover_or_refuse,
   provision_atomic, live_durable_checkpoint}` and the serve boot wiring
   (`attempt_db_auto_recovery`, the atomic-create and mirror-reconcile call sites,
   `aberp recover`), adapting `ensure_not_prod_path` for the now-in-scope prod root.
4. **Re-validate against prod's build:** prod's pinned libduckdb and binary must
   pass the full crash-injection acceptance gate (`boot_crash_recovery_e2e` plus
   the plain-file crash-injection unit), not just editions'.
5. **Migrate forward only:** existing prod installs upgrade via the standard
   `upgrade_prod.sh` path; recovery is additive over their existing snapshots +
   mirror.

## Consequences

**Easier / safer now:** prod stays provably untouched; the saw-off guarantee holds;
no risk is introduced into a frozen line; engineering attention stays on the active
editions tree.

**Harder / carried as debt:** prod keeps a known latent corruption risk with manual
recovery; we depend on prod's snapshots + mirror staying healthy as the recovery
inputs; if a trigger fires, the backport must re-pay ADR-0095's full validation cost
against prod's build before it can ship. This ADR is the standing record of that
accepted risk so it is never silently forgotten.

## Adversarial review

1. *"You proved the editions risk was serious enough to fix in three sessions, then
   left the identical bug in prod — the line that actually issues real invoices.
   Isn't that backwards?"* The risk is identical; the **expected cost** is not.
   Editions includes brand-new first launches (the 2026-06-27 Defense failure
   happened with zero prior clean shutdown, so nothing protected it) and ongoing
   development churn. Prod is frozen, invoicing-only, and already survived its one
   incident via the manual path with no data loss. Fixing the high-churn line first
   and recording prod as accepted, monitored risk is the defensible ordering — and
   criterion #2 means a *recurrence* immediately reopens it.
2. *"'Manual recovery is fine' contradicts `[[trust-code-not-operator]]`, which
   ADR-0095 invokes to reject manual recovery."* Correct, and we acknowledge it:
   for an **active** line we would not accept operator-dependent recovery. The
   narrow claim here is that for a **frozen, low-write** line, a proven reversible
   manual procedure is an acceptable *interim* posture — not a permanent one. The
   trigger criteria exist precisely so this exception cannot quietly become forever.
3. *"Does deferring leave any way for the editions change to touch prod by
   accident?"* No. `ensure_not_prod_path` rejects prod paths on every
   snapshot/restore/recover entrypoint, `--store`/`--to` refuse the prod line, and
   ADR-0093's edition binding refuses prod's DB root at boot. Deferral changes
   nothing about that; it is purely a decision *not to add* prod-side hardening.
4. *"If a backport is so well-specified, why not just do it now?"* Because doing it
   now spends the "prod untouched" invariant and incurs prod-build re-validation for
   a risk that is currently bounded and monitored. The specification exists so that
   when a trigger makes the cost worth paying, the work is ready — not so that we
   pay it pre-emptively.

## Alternatives considered

- **Backport now (harden prod immediately).** Rejected for now: spends the saw-off
  invariant and adds change-risk to a frozen line for a bounded, monitored risk; the
  manual recovery already works. Reconsidered the moment any trigger fires.
- **Never backport; rely on manual recovery permanently.** Rejected: that would make
  the `[[trust-code-not-operator]]` exception permanent and ignore the case where
  prod un-freezes or a recurrence raises the expected cost. The trigger criteria
  forbid "never."
- **Unfreeze prod to take the fix as part of broader modernization.** Out of scope
  here; that is exactly trigger #1 and would carry its own ADR.
- **Backport only the live-path checkpoints (§3), not the boot auto-recovery
  (§1–§2).** Rejected as a half-measure: §3 narrows the window but the boot
  auto-recovery is the part that removes the manual step; splitting them adds prod
  change-risk while leaving the operator-dependent gap open.

## Open questions

- Should prod's snapshot-store + mirror health be put under an explicit periodic
  check (alert if a recent valid snapshot or the mirror is missing), so the manual
  recovery inputs are guaranteed while deferred? (Leaning yes — it is operational,
  not a prod code change, so it does not spend the untouched invariant.)
- On a future un-freeze, do we backport onto `PROD_v2.27.76` in place or only onto
  the successor `PROD_vX`? (Leaning successor-only, to keep the frozen tag immutable.)
