# S263 — Pre-go-live adversarial review of S250–S262 (PROD_v2.13.0 → v2.17.0)

**Reviewer:** Claude Opus 4.8 (1M ctx), session 263
**Reviewed range:** S250 (PR-242, `eab3c74`) → S262 (PR-251, `58361a1`), the full batch since the S249 review baseline (`df3a14b`).
**Mode:** Doc-only. **No code, no test changes.** Findings are inputs to the S264 sweep PR.
**Gate context:** Third and final adversarial pass before real production invoicing for Áben Consulting (~2026-06-10). Per `[[pushback-as-method]]`, friction is the deliverable — an honest 7 🔴 beats a polite 2.

## Method

Seven parallel investigation agents read the actual merge commits, the full current source of every touched file, and the surrounding callers/plumbing. The two most explosive 🔴 claims were re-verified by hand against the live worktree tree before propagating:

- **Identity-write drops `[[mes.adapters]]`** — confirmed: `setup_seller_info.rs` re-appends 5 preserved sections (banks/smtp/numbering/branding/quote_intake) and has **zero** references to `mes_adapters`.
- **AP-sync daemon ungated by the restore lock** — confirmed: `restore_lock_block` is called only at `serve.rs:4753` (issue) and `serve.rs:10794` (manual sync-now); the AP-sync daemon (`ap_sync.rs:232`) and bootstrap (`ap_sync.rs:1106`) call `run_one_cycle` with no lock check, while the boot warning (`serve.rs:824`) promises the opposite.

Anything not confirmable by reading the code was demoted or dropped. ABERP-site PRs (S251 PR-R, S253 PR-S, S254 CloudFront) live in a separate repo and are **not reviewable from this worktree** — see the closing note.

---

## Executive summary

| Severity | Count |
|----------|-------|
| 🔴 Critical | **7** |
| 🟡 Medium   | **13** |
| 🟢 Verified-clean / good | **20** |

### Single most important go-live answer — the prod-NAV flag is CLEAN 🟢

The compile-time `production` Cargo gate (ADR-0038 preflight + `build_profile.rs` endpoint/`TEST-` prefix) was **not touched, not flipped, and not made harder to lift** by anything in S250–S262. `build_profile.rs` and `numbering.rs` have zero commits in the range; `production = []` / `default = []` are unchanged in both `Cargo.toml`s; the one new NAV-touching path (S261 restore wizard) routes through `build_profile::nav_endpoint()` + `assert_endpoint_allowed()` and does **not** hardcode `NavEndpoint::Production`. **Go-live is not blocked on the flag — it remains a safe one-line deliberate lift.** (Detail: cross-cutting Concern A.)

### Top 3 go-live blockers (plain English)

1. **The NAV-as-DR "safety net" is not trustworthy under failure (S261).** Three independent holes compound: (a) the *abandon* button deletes a lock with **no liveness check**, so a second operator can clear a *still-running* restore; (b) the AP-sync **daemon** and boot bootstrap ignore the lock entirely, even though the boot banner tells the operator they're blocked; (c) a **partial** restore (a failed month or a per-row insert error) still produces a checksum and audit landmark that *validate* — the durable record cannot distinguish a clean restore from one that silently dropped a whole month. For a disaster-recovery tool this is the worst possible failure class.

2. **Quote → draft pickup has a documented guard that does not exist (S255).** The code repeatedly claims an audit-ledger "F8 pin" prevents double-pickup; there is no UNIQUE on `idempotency_key` and no in-transaction CAS. Two operators (or one operator on two machines) picking up the same customer-submitted quote can mint **two orphan drafts**. Gap-free numbering itself is safe (numbers allocate at issue, not pickup), but the false-safety comment is exactly the rule-12 inversion the codebase warns against.

3. **Saving company identity silently wipes all MES adapter config (S257) — an exact repeat of the S170 regression.** The identity writer preserves 5 seller.toml sections but never re-appends `[[mes.adapters]]` (the 7th preservation slot). Configure adapters, later edit anything in Tenant Settings → Company identity, and every adapter vanishes on next boot. Three-line fix, five existing precedents in the same file, fully testable.

### Carryover note — half the S249 critical charter is still open

The S250/S252 "S249 critical sweep" PRs **genuinely fixed** F1, F2, F5 (and the F11/F14/F17/F18/F19 mediums). But **F3, F4, and F6 were never fixed** — and F3/F4 are now *pinned with code comments that reject the S249 recommendation*. A reader of "S249 critical sweep" would reasonably assume those are closed; they are not. F4 (missing `RoutingOpStateChanged` audit on Rework) is the most consequential, because F19 made the QA gate load-bearing for the auto-complete cascade that walks exactly that state.

---

## 🔴 CRITICAL

### 🔴 Finding 1 — NAV-as-DR `abandon` deletes the lock with no liveness guard; can clear a *running* restore

**Where:** `apps/aberp/src/serve.rs:11269-11301` (`handle_restore_lock_abandon`) → `crates/.../restore_from_nav_outgoing.rs:392` (`release_restore_lock_at`). Lock row schema `restore_from_nav_outgoing.rs:266-271`.

**What:** The lock row carries `{tenant_id, acquired_at, operator, year}` only — no PID, no process-instance id, no running/crashed flag. The *same* row is held during a live in-flight restore (acquired `serve.rs:11084`, released `:11117` after `run()` returns). The abandon handler unconditionally `DELETE`s it after the RESTORE-token check; it cannot tell a crashed lock from one a restore is actively using. Operator B clicking *Abandon* while operator A's restore is mid-walk frees the lock, after which a second restore can start concurrently (and Finding 4's stale in-memory idempotency cache then collides on the UNIQUE, laundered as `errored`).

**Why it matters:** The PR's stated thesis is `[[trust-code-not-operator]]` ("physically impossible parallel restore"). "Only abandon a *crashed* restore" is precisely the operator-trust pattern the lock was built to eliminate. This is a 🔴 by the convention's own default.

**Fix:** Make the lock distinguish live from crashed. Stamp the row with the current process boot-id/PID at acquire; on abandon, refuse (409) when that id matches the running process *and* an in-memory "restore active" flag is set — only allow abandon when the holder is provably a prior (crashed) process. Or add a `heartbeat_at` the running task bumps every N seconds and refuse abandon when it's fresh.

---

### 🔴 Finding 2 — AP-sync daemon + boot bootstrap are UNGATED by the restore lock; the boot warning is actively false

**Where:** `apps/aberp/src/ap_sync.rs:232` (daemon loop calls `run_one_cycle`), `ap_sync.rs:1106` / `:1169` (`run_bootstrap_year_once` → `run_one_cycle_for_window`). The only gated paths are `serve.rs:4753` (issue) and `serve.rs:10794` (manual sync-now). Boot warning text: `serve.rs:824` / `:831-833`.

**What:** `restore_lock_block` guards exactly two route handlers. The AP-sync *daemon* runs `run_one_cycle` every cadence with no lock check, and the bootstrap-year sweep runs at boot — precisely the crash-recovery boot where the lock is most likely held. The gate's own justification (`serve.rs:10790-10793`) is "a parallel NAV walk against the same tenant DB is exactly the contention the restore lock exists to prevent" — that walk happens unguarded from the daemon. The boot banner explicitly promises "Issue + AP-sync are blocked until the operator abandons the restore"; for the daemon (the dominant AP-sync path) this is a lie.

**Why it matters:** Parallel NAV walks against one tenant exhaust the per-tenant NAV rate limit and contend DuckDB writes during the exact window the restore needs them quiet. And a false operator-facing promise is a rule-12 fail-loud inversion. (No cross-table corruption — AP writes `ap_invoice`, restore writes `restored_invoice` — so harm is contention + false promise, not row collision. Still 🔴: a DR tool that lies about what it has blocked cannot be trusted under the one condition it exists for.)

**Fix:** Add the `restore_lock` read to the daemon loop body (skip + log the cycle when held) and to `run_bootstrap_year_once`. Thread a lock-check into `CycleInputs`/`build_inputs`, or short-circuit inside `run_one_cycle` when a lock row exists for the tenant. Then the boot warning becomes truthful.

---

### 🔴 Finding 3 — Partial NAV-as-DR restore produces a *validating* checksum + an audit landmark that cannot record the partial

**Where:** `restore_from_nav_outgoing.rs:1682` (all digest numbers collected before processing), `:1155` (checksum over the full set), `:1727-1734` (per-row insert error → `er += 1`, `warn!`, **continue**), `:1640-1658` (a month's digest-fetch failure → `errored += 1`, returns the month's *partial* outcome, outer loop continues). Audit payload `audit_payloads.rs:94-110`.

**What:** Two divergences make the checksum a false witness. (1) Per-row insert errors stay *in* the checksum: `month_numbers.extend(...)` captures every digest number before the blocking insert closure runs, so an errored insert leaves the number in `all_numbers` — the checksum says "NAV held N" and `invoice_count = N` while `restored_invoice` is missing those rows. (2) A mid-year month-fetch failure truncates the *other* way — that month's numbers are never fetched, absent from the checksum, yet the run returns `Ok(summary)` with only `errored += 1`. Critically, `RestoreFromNavRunPayload` carries `{year, invoice_count, partner_count, product_count, checksum, ts}` with **no `restored`/`skipped`/`errored` fields** — the durable landmark literally cannot distinguish a clean restore from one that dropped a month. Its own docstring concedes `invoice_count` is "the cardinality of the set the checksum is computed over — NOT the rows written."

**Why it matters:** "Completed successfully" with rows silently skipped is the worst class of bug (CLAUDE.md rule 12), and here it is baked into the *disaster-recovery* tool's permanent audit record. An auditor recomputing the checksum against a full NAV dump gets a different hash with no indication which is authoritative.

**Fix:** (a) Add `restored`, `skipped`, `errored` to `RestoreFromNavRunPayload` so the landmark records the actual write outcome. (b) Compute the checksum over the *successfully restored-or-already-present* set, or emit two checksums (`nav_set_checksum` vs `local_state_checksum`) so a mismatch is detectable. (c) When `errored > 0`, surface a distinct non-clean outcome on the SPA summary, not a buried count.

---

### 🔴 Finding 4 — Quote → draft pickup: the documented "ledger F8 gate" does not exist; concurrent double-pickup mints two orphan drafts

**Where:** `apps/aberp/src/quote_pickup.rs:287` (idempotency read **outside** the tx), `:301` (short-circuit on stale read), `:342` (tx begins here), `:393` (writeback). The claimed backstop: `crates/audit-ledger/src/storage/mod.rs:239-301` (`append_in_tx` — bare INSERT, no idempotency-key dedup) and `schema.rs:34-37` (no UNIQUE on `idempotency_key`). False-safety comments at `quote_pickup.rs:270`, `:313-315`, `:357-364`, `invoice_draft.rs:249`.

**What:** `read_for_pickup` reads `picked_up_drf_id` before any transaction; the route opens a fresh `Connection::open` per request with no app-level write mutex. Two concurrent pickups both read `NULL`. The code claims the audit ledger's "F8 pin" prevents the double-emit, but that guard is fictional — `idempotency_key` is forensic metadata with no UNIQUE and no SELECT-on-conflict. The *only* incidental protection is DuckDB MVCC conflicting the two `quote_intake_log` UPDATEs **iff the transactions overlap in time**. In the interleaving where A commits before B *begins* its tx (but after B already read `NULL`), B mints a second draft, B's UPDATE wins last-writer (no conflict, A's tx closed), and the result is **two orphan drafts + two `InvoicePickedUpFromQuote` audit rows**, one silently overwritten in the log column.

**Why it matters:** Gap-free numbering itself is safe (🟢 Finding G1 — numbers allocate at issue, not pickup). But a codebase that *documents a protective mechanism it does not implement* is the rule-12 worst case: false-safety + real gap. The single-operator SPA button-disable (`QuotesList.svelte:347`) masks it, which is exactly why it would ship undetected until two operators hit the same Lightsail-sourced quote.

**Fix:** Move the idempotency read INSIDE the transaction and make the write a compare-and-swap: `UPDATE quote_intake_log SET picked_up_drf_id = ? WHERE quote_id = ? AND tenant_id = ? AND picked_up_drf_id IS NULL`, assert `rows_updated == 1` before commit; on 0, another pickup won — roll back and return the winner's draft. Separately, **delete the false "ledger F8 gate" comments** in `quote_pickup.rs` / `invoice_draft.rs`.

---

### 🔴 Finding 5 — Saving company identity silently wipes `[[mes.adapters]]` — exact repeat of the S170 regression

**Where:** `apps/aberp/src/setup_seller_info.rs:274-296` (write path). Verified: `grep -c mes_adapter setup_seller_info.rs` → **0**.

**What:** `setup_seller_info::write` re-renders seller.toml from scratch (`render_seller_toml`) and explicitly re-appends each *known* preserved section: banks (`:276`), smtp (`:279`), numbering (`:282`), branding (`:285`), quote_intake (`:291`). It does **not** read or re-append `[[mes.adapters]]` (S257's 7th preservation slot). An operator configures adapters via Settings → Adapters, later edits anything in Tenant Settings → Company identity, and the identity rebuild drops the section — every MES adapter silently vanishes on next boot (`boot_from_toml` reads zero).

**Why it matters:** This is the identical failure class Ervin caught in S170 (lost smtp+numbering on a prod identity edit), the lesson that produced four of the five preservation precedents sitting in this very function. The other four section-replace writers (smtp/numbering/branding/quote_intake) all preserve `[[mes.adapters]]` via preserve-everything-else line-walkers — only the from-scratch identity rebuild drops it.

**Fix:** Mirror the existing pattern — `let preserved_mes = mes_adapters_config::read_mes_adapters(path)?;` then `append_section(&mut body, &mes_adapters_config::to_toml_section(&preserved_mes));` after the quote_intake append. Add `identity_save_preserves_existing_mes_adapters` alongside the existing five preservation tests. See `[[seller-toml-write-invariant]]`.

---

### 🔴 Finding 6 — S249-F4 NOT FIXED: `decide_qa(Rework)` still bypasses the routing-op state machine and emits no audit

**Where:** `crates/aberp-qa/src/repository.rs:469-475` — raw `UPDATE routings SET state='active', completed_at=NULL` outside `transition_routing_op`. Lines `462-468` are a **new comment that affirmatively declines** to emit `RoutingOpStateChanged` ("We do NOT emit … Cleaner than double-emitting").

**What:** The `Completed → Active` reverse flip on Rework is still invisible to anyone reconstructing routing-op state from the audit ledger — exactly S249-F4. The S252 sweep not only left it unfixed but pinned the decision in-code. This is now *more* load-bearing than at S249 because PR-243's F19 made the QA gate the source of truth for the auto-complete cascade that depends on routing-op state.

**Why it matters:** The forward `Active → Completed` carries an audit; the reverse only appears implicitly inside `QaInspectionDecided{to_state: Reworking}`. The day someone adds the obvious "list routing-op transitions for this WO" query, the cascade is misreported. Audit-ledger holes on a now-load-bearing path are 🔴.

**Fix:** Emit an explicit `RoutingOpStateChanged{Completed→Active, source=qa_rework, qa_id, routing_op_id}` inside the Rework branch alongside the UPDATE. The "double-emitting" objection is wrong: the QA row records the *decision*; the ledger needs the routing-op *state transition* as a first-class walkable row.

---

### 🔴 Finding 7 — S249-F3 NOT FIXED: `try_auto_complete_wo` still silently skips OnHold WOs, now test-pinned as intended

**Where:** `crates/aberp-work-orders/src/repository.rs:1658` — unchanged `if !matches!(current_state, WorkOrderState::InProgress) { return Ok(None); }`. The S252 sweep added `tests/wo_auto_complete.rs:917-932` (`try_auto_complete_returns_none_when_wo_on_hold`) which now **pins the silent no-op as correct**, with a comment deferring S249-F3.

**What:** An OnHold WO whose final op is QA-passed returns `Ok(None)` with zero operator signal — no `auto_complete_blocked` field was added to `DecideQaInspectionResponse`. The operator gets no hint they must Resume + manually Complete; the WO sits in OnHold until someone notices the queue isn't draining. The sweep converted an open critical into a test-enforced invariant without fixing it.

**Why it matters:** Violates `[[trust-code-not-operator]]` for the exact lifecycle S243 was built to automate, and the test now actively defends the gap. 🔴.

**Fix:** Surface a structured warning — add `wo_auto_complete_skipped: Option<{ reason: "wo_on_hold" | "wo_cancelled", wo_id }>` to `DecideQaInspectionResponse`, populated when `try_auto_complete_wo` returns `Ok(None)` for a non-InProgress, non-completed state, so the SPA can toast "Resume the WO to complete it." (Widening the guard to auto-Resume is heavier and needs the audit-ordering decision — surfacing the warning is the safe minimum.)

---

## 🟡 MEDIUM

### 🟡 Finding 8 — Adapter health baseline is never seeded from the ledger at boot; the "durable trail" has restart-sized holes

**Where:** `apps/aberp/src/serve.rs:939` (`adapter_health_baseline: HashMap::new()`, empty at boot), `:11633-11649` (`diff_adapter_health` treats first-sight as a silent seed). Doc at `serve.rs:2042`.

**What:** S258's premise is that `AdapterHealthTransitioned` is the durable trail so reloads recover from the ledger. The SPA *chime* state does recover. But the backend emission **baseline** initializes empty and is never seeded from the ledger (`grep adapter_health_baseline` → 1 init + 1 poll use + 2 test inits, no ledger walk). Consequences: an adapter that stays `unhealthy` across a restart records no continued-fault entry (correct for chime, but a *gap* in the trail); a fault that *clears* during downtime (`unhealthy`→`healthy`) never records the recovery transition; a fault that *occurs* during downtime is never captured. The ledger is an in-session transition log, not a faithful health history.

**Why it matters:** Demoted from 🔴 because there is no data loss and the chime behavior is correct — but the doc claims a durable trail it doesn't fully deliver, and the brief asked directly "does the boot path replay it?" The answer is no.

**Fix:** Either seed `adapter_health_baseline` at boot from the most-recent `AdapterHealthTransitioned` per `adapter_id` (one ledger walk in `run()`), so continued-fault / downtime-recovery produce correct transitions on first post-boot poll; or, if the gap is intentional, change the `serve.rs:2042` doc to state plainly that downtime transitions are invisible by design.

### 🟡 Finding 9 — Adapter `add`/`update` leak a live adapter on persist/audit failure (no rollback) + duplicate-endpoint bypass

**Where:** `apps/aberp/src/mes_manager.rs:219-225` (`add`), same shape `:262-276` (`update`). `refuse_duplicate_endpoint` checks TOML, not the live registry (`:504`).

**What:** `add` does `start_and_register` → `persist` → `audit`. If `persist` or `audit` errors, the started/registered adapter is never stopped or deregistered — it runs as a ghost absent from TOML, dying on next restart. Worse: the operator's retry isn't refused by `refuse_duplicate_endpoint` (the ghost isn't in TOML), a new `adapter_id` is minted, `register` succeeds on the different key, and **two live adapters bind one host:port**.

**Fix:** On `persist`/`audit` failure, call `stop_and_deregister(&entry.adapter_id)` before returning the error.

### 🟡 Finding 10 — Runtime `add`/`update` drop the spawned task handles; not registered with the shutdown coordinator

**Where:** `mes_manager.rs:219` (`start_and_register(...).await?` return value discarded) vs `boot_from_toml:319-328` (collects + returns handles).

**What:** Boot-spawned adapter tasks are awaited on shutdown; runtime-added ones are not (handles dropped). Child-token parentage still *cancels* them, but shutdown doesn't *block* on teardown — a barcode listener's port may not release and a mid-write ledger append may be cut. Asymmetric with the boot path.

**Fix:** Thread runtime-spawned handles into the shutdown coordinator (AdapterManager holds a clone of the handle-sink), or document the runtime-add path relies solely on token cancellation.

### 🟡 Finding 11 — Restore storno / modify write paths are ungated by the restore lock

**Where:** Gated: `handle_issue_invoice` (`serve.rs:4753`). Ungated: `handle_storno_invoice` (`serve.rs:5959`), `handle_modification_invoice` (`serve.rs:~6320`). Quote-pickup (`serve.rs:14680`) is draft-only, lower severity.

**What:** Storno and modify both issue real NAV invoices and run the same post-issue NAV-submit tail (`run_chain_post_issue_tail`), generating exactly the parallel-NAV-walk contention the issue gate exists for. An operator can issue a correction during a restore even though the plain issue button is blocked — inconsistent and contended. (🟡 not 🔴: writes to canonical `invoice`, not `restored_invoice`, so no cross-table collision.)

**Fix:** Add `restore_lock_block(&state)` to `handle_storno_invoice` and `handle_modification_invoice`. Decide explicitly on quote-pickup and document either way.

### 🟡 Finding 12 — Restore idempotency is a stale in-memory cache; a concurrent dup is laundered into the silent-partial `errored` bucket

**Where:** `restore_from_nav_outgoing.rs:1120` (cache loaded once per run), `:1992` (`already_restored_cache.contains` — the only gate), `:2054` (INSERT; UNIQUE violation → `Err`), `:1727-1734` (that `Err` → `er += 1`, warn, continue). UNIQUE at `:202`.

**What:** The idempotency check is `HashSet::contains` against a run-start snapshot, not a live DB check. Once Finding 1's abandon-during-run permits two concurrent restores, both hold stale caches, both attempt the same INSERT; the `restored_invoice` UNIQUE is the only thing preventing a duplicate row — and when it fires, the loser isn't classified as a benign conflict, it's counted as a generic `errored` row feeding straight into Finding 3's silent-partial problem.

**Fix:** Primarily close Finding 1. Secondarily, match the DuckDB constraint-violation error and classify it as `Skipped`, not `errored`, so a benign idempotency collision isn't laundered into the corruption bucket.

### 🟡 Finding 13 — Finance currency-split EUR segment is storno-asymmetric vs its own neighbor figure

**Where:** `apps/aberp/src/reports.rs:1348-1356`. `eur_native_minor` is storno-adjusted (storno-self rows netted in `aggregate_outgoing`); `eur_as_huf_minor` = `query_eur_huf_equivalent` (`:844`) is a raw `SUM(huf_equivalent_total) WHERE currency='EUR'` with **no storno awareness**.

**What:** With any EUR storno in the window, the stacked-bar EUR segment (HUF-converted) overstates EUR relative to the native EUR figure printed beside it — the two numbers on one tile won't reconcile. The window filter is consistent (both use `build_date_where`) and the snapshot rate is correctly per-invoice; only the storno netting diverges.

**Fix:** Subtract the snapshot-HUF equivalent of storno-self EUR rows in `query_eur_huf_equivalent` (thread the storno-self set the aggregate already walks), or render an explicit "storno not netted" marker on the EUR segment. A financial tile whose two numbers don't reconcile must fail loud, not carry only a backend doc-comment.

### 🟡 Finding 14 — Finance AR aging click-through eligibility uses a different axis than the dashboard's counted-set

**Where:** SPA gate `apps/aberp-ui/ui/src/routes/InvoiceList.svelte:177-200` (`agingMatches` keys on `row.state ∈ {Submitted, Recovered, Finalized}` + `payment != null`). Backend gate `reports.rs:1149-1205` / `:543-560` (`CountedKind::Counted` = `last_ack_status == "SAVED"` OR `has_submission_response`, + `payment_paid_at.is_some()`).

**What:** The SPA's `InvoiceState` enum is a derived lifecycle label; the backend's "counted" is raw NAV-ack presence — different classification axes. The code comment at `:189-197` openly admits "the list count can diverge slightly from the dashboard bucket count." That is exactly the fail-loud regression the shared `aging.ts` module was meant to prevent — except `aging.ts` only unifies the bucket *boundary* math, not the *eligibility* predicate. The operator can click "31–60 nap = 3" and land on a list of 2. (Boundaries themselves are consistent across all four surfaces — 🟢 G8.) Secondary 🟢 nit: `agingMatches` re-derives `todayIsoLocal()` independently of the report's `req.today`, so a drill-through across local midnight shifts boundary rows by one bucket.

**Fix:** Surface an `ar_eligible` boolean per row on the wire, computed from the same ack-status trace the backend uses; or make the click-through banner disclose "approximate — may differ from dashboard count" so the divergence is loud.

### 🟡 Finding 15 — Quote arrival 90s catch-up grace is a hard-coded wall-clock guess vs a configurable cadence

**Where:** `serve.rs:14326` (`QUOTE_INTAKE_CATCHUP_GRACE_SECS = 90`), `:14349` (`boot + 90` boundary), `:14947` (`<= boundary → continue`).

**What:** The fixed 90s ("30s boot delay plus first cycle") is measured against a *configurable* poll cadence and a first cycle that can legitimately exceed 90s (large downtime backlog, slow storefront, high `poll_interval_secs`). Failure modes: a long first cycle's catch-up rows land after boot+90s and wrongly toast as "live"; conversely a genuinely-live arrival within 90s is demoted to badge-only. The `<=` boundary is correct for "cycle-2-onward"; no data loss (badge is DB-backed).

**Fix:** Derive the boundary from the first completed cycle's finish time (a watermark set when cycle 1's `QuoteIntakePollAttempted` lands), or at minimum scale the grace from the configured `poll_interval_secs`.

### 🟡 Finding 16 — Quote-pickup shim test mocks the function under test (mock-on-mock)

**Where:** `apps/aberp-ui/ui/src/lib/quote-pickup.test.ts:67-92` — `vi.mock` replaces `pickupQuoteAsDraft` with `vi.fn()`, then asserts the resolved value equals what was just stubbed.

**What:** This pins mock-on-mock — it cannot fail if the real shim's `quote_id` forwarding, URL construction, or body parsing breaks (CLAUDE.md rule 9). The sibling `pickupActionVariant` and S256 arrival-notification tests DO pin real pure logic — those are good.

**Fix:** Test the real shim against a stubbed `fetch`/`invoke` (assert the POST URL `/api/quotes/:quote_id/pickup-as-draft` and that the JSON parses into `PickupQuoteOutcome`), or delete the test — its current form adds coverage-count without coverage.

### 🟡 Finding 17 — No concurrent-pickup test; the "idempotent" test only covers the post-commit path

**Where:** `quote_pickup.rs:561` (`pickup_is_idempotent_returns_existing_drf_id`).

**What:** Every backend pickup test is single-threaded sequential; the "idempotent" test exercises only the post-commit path (second call sees the committed `picked_up_drf_id`). The concurrent-uncommitted race (Finding 4) is untested, so the green "idempotent" test gives false confidence that concurrency is handled.

**Fix:** Add a test that interleaves two pickups before either commits and asserts exactly one draft + one audit row (will fail until Finding 4's CAS lands — which is the point).

### 🟡 Finding 18 — S249-F6 NOT FIXED: WO dashboard row ordering still contradicts the displayed `touched_at`

**Where:** `serve.rs:12094-12116` (`build_work_order_rows` calls `list_work_orders(...LIMIT)`, ordered `created_at DESC, wo_id DESC`) while rendering `wo_touched_at(&wo)` per row (`:12105`, ladder `:12123-12144`). Self-describing doc at `:12088` now even says "5 most-recently-**created**."

**What:** Sort key (created) and displayed column (touched) disagree on the wall-TV; neither sweep touched it. Carryover S249 🔴, demoted to 🟡 here because it's display ordering, not data integrity — but it remains a visible inconsistency on the shop-floor surface.

**Fix:** Add `list_recent_work_orders_by_touched` ordering by `COALESCE(cancelled_at, completed_at, started_at, released_at, created_at) DESC, wo_id DESC`; point `build_work_order_rows` at it; pin a tiebreak determinism test.

### 🟡 Finding 19 — Operator release notes absent for the entire window (v2.10.0 → v2.17.0)

**Where:** `docs/releases/` holds only `PROD_v2.0 / 2.1.1 / 2.5.1 / 2.9.0`.

**What:** Everything from v2.10.0 through the go-live candidate v2.17.0 ships with no operator-facing release note, and no `release.sh` precheck enforces one. Carryover S249-F10, still open. For a go-live cut this is the difference between an operator knowing what changed and not.

**Fix:** Backfill the window's release notes (terse is fine) and add a `release.sh` precheck that refuses a cut without a matching `docs/releases/PROD_vX.Y.Z.md`.

### 🟡 Finding 20 — `too_many_arguments` still `allow`ed workspace-wide

**Where:** `Cargo.toml:373` — `too_many_arguments = "allow"`.

**What:** Carryover S249-F9. A blanket allow suppresses a real signal (the restore/AP-sync cycle-input plumbing this review touched is exactly where arg-count creep hides). Hygiene, not correctness — 🟡.

**Fix:** Flip to `warn`, fix or `#[allow]` the handful of legitimate sites individually.

---

## 🟢 VERIFIED-CLEAN / GOOD

These were investigated adversarially and held up — several answer the brief's specific worries directly.

- **G1 — Gap-free numbering preserved on quote pickup.** Pickup creates a `drf_<ULID>` draft (`quote_pickup.rs:343` → `invoice_draft.rs:275`, emits `InvoiceStaged`); the gap-free sequence allocator is untouched until issue time. No gap, no duplicate from pickup. (Brief worry #1 — clean.)
- **G2 — Restore lock *acquisition* is atomic, NOT a TOCTOU race.** Single `INSERT ... ON CONFLICT (tenant_id) DO NOTHING` on a `tenant_id` PK (`restore_from_nav_outgoing.rs:320-331`); the conflict resolution *is* the check. A hard-kill in the "acquisition window" the brief worried about is impossible — there is no separate check-then-write. (Brief worry #3 — the acquisition is clean; the *abandon* and *gating* are the holes — Findings 1/2.)
- **G3 — Finance CSV export DOES honor the aging bucket filter.** `exportCsv()` maps `visibleRows` = `filterInvoices(rows, filter).filter(agingMatches)` (`InvoiceList.svelte:730-732, :771`); a deep-link into 90+ then Export writes only the bucketed rows. AP/Incoming list has no CSV export, so no AP-side mismatch surface. (Brief worry #4 — clean.)
- **G4 — Aging bucket boundaries consistent across all four surfaces.** Backend `reports.rs:1250-1263` and SPA `aging.ts:93-101` use identical thresholds; `parseAgingBucket` discards unknown values (closed-vocab, pinned); `panelField` is an exhaustive switch with no `_ =>`. Day-30/31 land in the right buckets. (Brief worry #9 — clean.)
- **G5 — PDF storno/modify/re-render all go through the single `render_invoice` path.** Only one render path exists (`print_invoice::render_to_bytes` → `lib.rs:324`), reached from the print route and email only; storno/modify just write their own NAV XML and re-parse through it, so both the column clamp and EUR NBSP land on every surface. Negative-total storno renders `-€\u{00A0}…` correctly. (Brief worry #5 — clean.)
- **G6 — PDF AFM width table is panic-safe and Adobe-accurate.** `width_proxy_byte` has a `_ => b'o'` catch-all; all 256 input bytes resolve to `0x20..=0x7E` (max index 94, table len 95) — no zero-width/overflow path. All Hungarian letters resolve (`ő/ű` pre-folded to `ö/ü` → base). Spot-checked against canonical Core-14 values. (Brief worry #5 charset — clean.)
- **G7 — EUR NBSP fires only for EUR; HUF untouched.** `format_eur_cents` (`format.rs:67`) inserts the NBSP; `format_huf_forints` is unchanged. Applied uniformly to every EUR amount via `format::money`. (Brief worry #5 — clean.)
- **G8 — "Tisztelt Partner" greeting covers EVERY email surface.** `email_invoice.rs:525-527` `compose_body_plain` is the only greeting composer in the codebase; issue/storno/modify/restore-DR auto-email all funnel through `send_invoice_email` → `compose_body_plain`. No per-surface fork. (Brief worry #6 — clean.)
- **G9 — Finance top-N + custom range validated on both layers.** Backend `clamp(1, 50)` (`serve.rs:11443`); SPA clamps + rejects inverted ranges (`StatisticsPage.svelte:104-124`). Negative `top_n` is a serde 4xx, not a silent 0.
- **G10 — Prod-NAV flag CLEAN and one-PR-liftable.** (Cross-cutting Concern A — see executive summary. The single most important go-live answer.)
- **G11 — No new DuckDB-only DDL in the range.** The only new table (`restore_lock`) is CHECK-free; the one new `ON CONFLICT` is standard-SQL-portable. (Brief worry #10 — clean. Note: the `[[no-sql-specific]]` premise doesn't match this codebase — `docs/external-outgoing-mirror-design.md:153-161` documents the actual convention is CHECK-as-defence-in-depth, all pre-dating this range.)
- **G12 — EventKind exhaustiveness: all integrity consumers fail-closed.** `as_str` exhaustive (no `_ =>`); `from_str`/`mirror.rs` fail-closed on unknown; the NAV-bundle reader (`verify.rs:692-889`) and writer (`export_invoice_bundle.rs:493-768`) enumerate every new kind with no wildcard — a future variant is a compile error. The wildcards that exist are semantically-correct chain-link filters. (Brief worry #9 — clean.)
- **G13 — New quote EventKinds exhaustively matched.** `QuoteIntakePoll{Attempted,Failed}`, `QuoteIntakeRowAdded`, `InvoicePickedUpFromQuote` handled at every consuming match with no silent swallow.
- **G14 — Adapter closed-vocab exhaustive.** `AdapterKind` / `AdapterHealth` / the 3 CRUD EventKinds all exhaustively matched; SPA `adapter-format.ts` graceful-degrades unknowns (render raw + warn) rather than crashing. F12 ritual followed.
- **G15 — S259 `common.rs` dedup is behavior-preserving.** Read all three adapter diffs: each old `start`/`stop`/`health` was byte-for-byte the take→cancel→await→Stopped pattern now in `AdapterLifecycle`; zebra's sync probe order, mtconnect's out-of-band Unhealthy flip, ur_rtde's `last_state` reset all preserved. No backoff/probe drift.
- **G16 — Mid-backoff adapter delete cancels cleanly (no orphan).** `stop_and_deregister` cancels the child token then awaits `adapter.stop()`; ur_rtde's `sleep_with_cancel` unblocks the backoff immediately.
- **G17 — env→TOML adapter migration is idempotent.** `migrate_env_adapters_to_toml` skips any `adapter_id` already in TOML; tested for idempotency + hand-added preservation. (Only the *identity* writer drops the section — Finding 5.)
- **G18 — S249 critical sweep genuinely fixed F1/F2/F5.** Adapters wired + registry-visible (`mes_manager.rs:366-402`, smoke test `adapter_config.rs:329`); single session ULID per QA-decide tx (`serve.rs:9676`, real integration test asserts set-len-1); Workshop SPA safe-degrades at the API boundary (`api.ts:3171-3190`, real before/after tests). All three verified at source with tests that genuinely fail on regression.
- **G19 — S249 medium sweep genuinely fixed F11/F14/F17/F18/F19.** RTDE first-frame sanity (`ur_rtde.rs:725`), bounded shutdown (`:653`), QA-on-cancelled guard (`repository.rs:360`), source_event_id threaded into the auto-complete cascade with a real two-row test (`wo_auto_complete.rs:735`). Verified.
- **G20 — PDF `doc.compress()` is encoding-only.** FlateDecode on the content stream; rendered output byte-identical after inflate. Tests correctly assert at the `layout()` op level. (One 🟢-level visual nit: right-aligned EUR amounts shift ~2–5pt left because `text_right_in` counts the NBSP at the 0.55em proxy vs its real 0.278em advance — safe direction, away from the margin. **visual-validation-pending** — eyeball one EUR invoice's totals alignment live.)

---

## Out-of-repo (not reviewable from this worktree)

The ABERP-site PRs — **S251 PR-R** (fail-closed `publicSiteUrl`), **S253 PR-S** (`/quote` form action fix), **S254** (CloudFront walkthrough) — live in the separate ABERP-site repo and cannot be read from the ABERP worktree. They are **not verified by this review**. S249's F7/F8 (CSRF / fail-open on that site) were explicitly deferred to a session in that repo and remain unconfirmed-closed. **Recommendation:** a dedicated ABERP-site review pass before go-live if the storefront quote intake is live on day one. Flagging so these are not assumed closed.

---

## Sweep PR brief — for S264 (verbatim-usable)

**Goal:** Clear the 7 🔴 before go-live; the 🟡s are fast-follow. No version bump on this review doc (Dispatch merges it to main). The sweep itself is a normal PR cut.

**Order by blast radius — do the DR holes first (the safety net must be trustworthy before go-live):**

1. **🔴 F1+F2+F3 (NAV-as-DR, one coherent PR):**
   - F2: add `restore_lock` read to the AP-sync daemon loop (`ap_sync.rs:232`) and `run_bootstrap_year_once` (`ap_sync.rs:1106`) — skip + log when held. Then the `serve.rs:824` boot warning is true.
   - F1: stamp the lock row with process boot-id/PID at acquire (`restore_from_nav_outgoing.rs` acquire); refuse abandon (409) when the holder is the running process + restore-active flag set.
   - F3: add `restored`/`skipped`/`errored` to `RestoreFromNavRunPayload` (`audit_payloads.rs:94-110`); compute the checksum over the successfully-restored set (or emit `nav_set_checksum` + `local_state_checksum`); surface `errored > 0` as a non-clean SPA outcome.
   - Tests: abandon-during-live-run is refused; daemon skips while lock held; a forced per-row insert error yields `errored > 0` in the audit landmark and a checksum that does NOT match the clean-run hash.

2. **🔴 F4 (quote pickup CAS):** move the idempotency read inside the tx; convert the writeback to `UPDATE … WHERE picked_up_drf_id IS NULL` + assert `rows_updated == 1`; on 0, roll back and return the winner. Delete the false "ledger F8 gate" comments in `quote_pickup.rs` / `invoice_draft.rs`. Add the concurrent-uncommitted test (Finding 17).

3. **🔴 F5 (identity preserves mes.adapters):** in `setup_seller_info::write`, read + re-append `[[mes.adapters]]` after the quote_intake append (mirror the 5 existing precedents). Add `identity_save_preserves_existing_mes_adapters`.

4. **🔴 F6 (Rework routing-op audit):** emit `RoutingOpStateChanged{Completed→Active, source=qa_rework}` in `aberp-qa/src/repository.rs:469-475`; delete the "we do NOT emit" comment; add an audit-walk test that recovers the reverse transition.

5. **🔴 F7 (auto-complete OnHold warning):** add `wo_auto_complete_skipped: Option<{reason, wo_id}>` to `DecideQaInspectionResponse`, populate on the non-InProgress `Ok(None)`; SPA toasts "Resume the WO to complete it." Re-point the F3-pinning test (`wo_auto_complete.rs:917`) to assert the warning instead of the silent no-op.

**Fast-follow 🟡 (same or next PR):** F8 (seed health baseline from ledger at boot, or fix the doc), F9 (adapter add/update rollback on persist fail), F11 (gate storno/modify on the restore lock), F13 (storno-net the EUR-as-HUF figure), F14 (AR click-through eligibility axis), F19 (backfill v2.10–2.17 release notes + `release.sh` precheck).

**Do NOT touch:** the prod-NAV flag (G10 — it lifts in its own deliberate reviewed PR per `[[aberp-golive]]`; leave it TEST-default).

---

*End of S263 review. 7 🔴 / 13 🟡 / 20 🟢. The prod flag is clean; the DR safety net and the quote/identity write paths are where go-live risk concentrates.*
