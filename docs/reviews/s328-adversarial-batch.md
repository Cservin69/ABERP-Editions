# S328 — Adversarial review of batch S312-S327

_Doc-only review. No code edited. Worktree `ABERP-pr28`, branch `session-328/pr-28-adversarial-review`._
_Reviewed against ABERP `PROD_v2.27.13` (`e88a7f0`) + storefront HEAD `c318850` (S323)._

## Verdict

**NOT pilot-ready as a customer-facing capability — the S318→S323→S325 stock-alert-banner arc is architecturally non-functional in production.** The plumbing (PDF band, queue, daemon, storefront relax, audit ritual) is individually well-built and well-tested, but the end-to-end path cannot fire: (a) the upstream snapshot column `stock_status_at_accept` has **no production writer**, so the trigger never arms; and even if it did, (b) the daemon's re-render lands **only** while the storefront quote is `quoted`, whereas the trigger is post-acceptance when the storefront quote is `approved`/terminal — the daemon then receives a `409` which it **misclassifies as success**, so non-delivery is silently recorded as `quote.pdf_rerendered`. The S325 memory claim "customer-facing stock-alert banner now LIVE end-to-end" is **false**. The CI de-gate + walkthrough are shippable with fixes. 4 🔴 / 9 🟡 / 8 🟢.

---

## 🔴 Must-fix (with file:line)

### 🔴1 — `stock_status_at_accept` has no production writer: the entire stock-alert arc is dead code in prod
The recompute that arms `stock_alert` requires a non-NULL snapshot:
`recompute_stock_alert(stock_status_at_accept.as_deref(), …)` — [quote_intake_query.rs:227-235](apps/aberp/src/quote_intake_query.rs:227). With a NULL snapshot it returns `false` (pinned by test `s271_unaccepted_quote_never_triggers`, [quote_intake_query.rs:871](apps/aberp/src/quote_intake_query.rs:871)).

Every write of `stock_status_at_accept` in the entire tree is inside a `#[cfg(test)] mod tests` block (the test module begins at [quote_intake_query.rs:566](apps/aberp/src/quote_intake_query.rs:566); writers at lines 746, 887, 915, 958, 989 are all below it). The DEAL saga writes only `material_grade`/`quantity` ([quote_deal.rs:668](apps/aberp/src/quote_deal.rs:668)), and the pricing pipeline never touches `quote_intake_log` at all (grep: only a doc comment). The EventKind doc even asserts the snapshot is "the value … at the moment of acceptance" ([event_kind.rs:1298](crates/audit-ledger/src/entry/event_kind.rs:1298)) — but nothing captures it at acceptance.

**Consequence:** in production `stock_status_at_accept` is always NULL → `stock_alert` never flips FALSE→TRUE → the queue is never fed → the S325 daemon drains an empty queue forever. S318 PDF band, S323 relax, and S325 producer are all dormant.

**Fix (sweep):** add the missing producer. When ABERP first observes a quote reach customer-acceptance (the storefront `approved` writeback / DEAL-eligibility projection), snapshot the live `quoting_materials.stock_status` for the row's `material_grade` into `quote_intake_log.stock_status_at_accept` in the same projection that already writes `material_grade`. Pseudocode at the acceptance projection site:
```
let snap = read_current_stock_status_by_grade(conn, tenant)?.get(grade);
UPDATE quote_intake_log SET stock_status_at_accept = :snap
  WHERE quote_id = :id AND tenant_id = :t AND stock_status_at_accept IS NULL;  -- write-once
```
Add a prod (non-test) integration test that drives the projection and asserts the column is populated.

### 🔴2 — Re-render delivery window (`quoted`) is mutually exclusive with the trigger window (post-acceptance / `approved` terminal)
The storefront overwrites the PDF + flips the flag **only** when `existing.status === 'quoted'` ([priced/+server.ts:194-265](../../../ABERP-site/src/routes/api/quotes/[id]/priced/+server.ts)). Customer acceptance sets status `approved`, which is terminal ([status/+server.ts:35](../../../ABERP-site/src/routes/api/quotes/[id]/status/+server.ts), comment line 24 "approved is only settable by the customer accept POST"). The ABERP trigger fires while `is_actionable = intake_state=='staged' && deal_issued_at IS NULL && picked_up_drf_id IS NULL` ([quote_intake_query.rs:222](apps/aberp/src/quote_intake_query.rs:222)) **and** the snapshot exists — i.e. after the customer has accepted (storefront `approved`/terminal) but before the operator DEALs.

So a real alert always re-posts against an `approved` quote → storefront returns `409 terminal_or_committed` → never overwrites. The two windows never overlap. Even after fixing 🔴1, S325 cannot deliver.

**Fix (sweep):** decide the intended contract and make the windows meet. Either (a) extend the S323 relax to also accept the same-hash `stock_alert:true` overwrite for `approved` quotes (the geometry/pricing hash still guards identity; the overlay is orthogonal to terminality) — preferred; or (b) re-scope the ABERP trigger to fire while the storefront quote is still `quoted`. (a) is the smaller change and matches the EVE intent (alert the already-accepted customer). Add a storefront test: `approved` + same-hash + `stock_alert:true` → overwrite + `{rerendered:true}`.

### 🔴3 — Daemon classifies **all** `409` as success, recording non-delivery as `quote.pdf_rerendered`
[quote_pdf_rerender_daemon.rs:253-255](apps/aberp/src/quote_pdf_rerender_daemon.rs:253):
```rust
409 => RepostOutcome::Success { label: "already_flipped_409" },
```
But the storefront returns `409` for three distinct cases, only one of which is benign:
- `terminal_or_committed` — the 🔴2 path: **banner NOT delivered**.
- `already_priced_with_different_hash` — a genuine geometry/pricing conflict: **banner NOT delivered**, needs an operator.
- (`unexpected source state` — currently unreachable.)

Treating all three as `Success` drops the entry and emits `quote.pdf_rerendered` (success audit) when nothing was delivered. This is a CLAUDE.md #12 "fail-loud" violation: "completed successfully" while the customer PDF is unchanged. The test that "covers" this even seeds a misleading body (`status: 409, body: "already_priced_with_different_hash"` asserted as success — [daemon test line 1011-1031](apps/aberp/src/quote_pdf_rerender_daemon.rs:1011)).

**Fix (sweep):** parse the 409 body. `terminal_or_committed` → only treat as success once 🔴2 is resolved (until then it is a real non-delivery → emit `quote.pdf_rerender_failed` with `error_class:"terminal_window"`). `already_priced_with_different_hash` → `Permanent` failure + audit (operator must reconcile the hash). Add a `RepostOutcome::Conflict` arm rather than overloading `Success`.

### 🔴4 — No boot-time recovery of orphaned enqueues; a crash mid-cycle loses the whole drained batch permanently
`poll_once` does `let ids = deps.queue.drain();` ([daemon:405](apps/aberp/src/quote_pdf_rerender_daemon.rs:405)) — atomically removing **all** ids into a local `Vec`, then processing them sequentially with a 30s HTTP timeout each. A panic/SIGTERM/restart anywhere in that window loses every drained-but-unprocessed id. The in-memory queue starts **empty** at every boot ([serve.rs:1017](apps/aberp/src/serve.rs:1017)); there is **no** boot recovery scan. The read-side cannot re-detect because `stock_alert` is now sticky-TRUE ([quote_intake_query.rs:236-255](apps/aberp/src/quote_intake_query.rs:236)).

Critically, the durable signal already exists and is simply never replayed: `persist_alerts_and_enqueue_rerender` writes a `QuotePdfRerenderEnqueued` audit row in the same tx as the flip ([quote_intake_query.rs:376-389](apps/aberp/src/quote_intake_query.rs:376)). The module doc only admits the smaller "enqueued-but-undrained" window ([queue.rs:13-24](apps/aberp/src/quote_pdf_rerender_queue.rs:13)); the drained-batch window (up to N×30s) is larger and undocumented.

**Fix (sweep):** at boot, before spawning the daemon, scan the ledger for `QuotePdfRerenderEnqueued` rows lacking a later terminal (`QuotePdfRerendered`, or a `permanent` `QuotePdfRerenderFailed`) for the same `quote_id`, and re-enqueue them — the S307 outbox-recovery pattern. Pseudocode:
```
for qid in enqueued_without_terminal(conn, tenant)? { queue.enqueue(&qid); }
```

---

## 🟡 Should-fix (with file:line + recommended sweep action)

### 🟡1 — Re-render hard-codes `ToleranceRange::Standard`; latent content divergence beyond the banner
[daemon:377](apps/aberp/src/quote_pdf_rerender_daemon.rs:377) hard-codes `target_tolerance: ToleranceRange::Standard`, claiming it "mirrors" `advance_render`. But `advance_render` uses `self.config.default_tolerance` ([pipeline:677,712](apps/aberp/src/quote_pricing_pipeline.rs:677)). They match **only** because the single boot site happens to set `default_tolerance: Standard` ([serve.rs:1664](apps/aberp/src/serve.rs:1664)). `target_tolerance` is customer-visible: the thin-wall surcharge line renders iff `thin_wall_present && target_tolerance >= Tight` ([aberp-quote-pdf/src/lib.rs:338](crates/aberp-quote-pdf/src/lib.rs:338)). If `default_tolerance` is ever changed (the field exists precisely to be configurable), the re-render silently **drops** the thin-wall line, so the re-rendered PDF differs by more than the banner — contradicting the daemon's own promise.
**Fix:** persist the tolerance used at first render on `quote_pricing_jobs` and re-read it in `prepare_rerender`; do not hard-code.

### 🟡2 — Unbounded audit-row growth: no attempt cap, no per-entry backoff on transient failures
On any transient failure (storefront 5xx, transport error, DB-locked), `process_one` re-enqueues and emits one `quote.pdf_rerender_failed` audit row ([daemon:514-557, 465-476](apps/aberp/src/quote_pdf_rerender_daemon.rs:514)). With a 5s poll and a persistently-down storefront, that is **one ledger row every 5s per stuck quote, forever** — audit-ledger flooding. The supervisor backoff is panic-only ([daemon:677-684](apps/aberp/src/quote_pdf_rerender_daemon.rs:677)); transient failures have none, and there is no `attempt_n` cap (unlike the pricing pipeline).
**Fix:** add a per-entry attempt counter + exponential backoff; after K transient attempts, demote to a single `Permanent` audit + drop (operator-retry). Rate-limit identical consecutive failure audits.

### 🟡3 — `poll_once` is not cancellation-aware; shutdown mid-cycle loses the drained batch
`run_loop` awaits `poll_once(&deps, &reposter).await` with no `select!` against `cancel` ([daemon:710](apps/aberp/src/quote_pdf_rerender_daemon.rs:710)); cancellation is only honoured **between** cycles ([daemon:706-715](apps/aberp/src/quote_pdf_rerender_daemon.rs:706)). The module doc claims "Cancellation is honoured via the shared CancellationToken" ([daemon:57](apps/aberp/src/quote_pdf_rerender_daemon.rs:57)) — true only at cycle boundaries. Combined with 🔴4, a coordinated shutdown during a cycle loses the in-flight batch. No test exercises shutdown-during-poll.
**Fix:** check `cancel.is_cancelled()` between entries in the `for quote_id in ids` loop and re-enqueue the remainder on cancel; add a test.

### 🟡4 — S323 relax turns a previously-immutable priced PDF into a bearer-overwritable artifact; PDF bytes are never verified against the hash
Pre-S323, a same-hash re-post to a `quoted` quote was a strict no-op. Now any holder of the admin bearer can overwrite the stored customer PDF with **arbitrary** ≤5 MB bytes by posting the known `feature_graph_hash` + `stock_alert:true` ([priced/+server.ts:216-249](../../../ABERP-site/src/routes/api/quotes/[id]/priced/+server.ts)). The endpoint never checks that the PDF corresponds to the hash. Bearer is trusted, but artifact integrity regressed (defense-in-depth).
**Fix:** at minimum, scope the overwrite tightly to the stock-alert case (it already gates on `meta.stock_alert && prior.stock_alert !== true`, which is good); consider recording a digest of the original PDF and rejecting an overwrite whose size/shape is wildly different, or log a distinct audit event for byte-substitution. Document the widened write surface in the ADR.

### 🟡5 — Lapsed `valid_until` makes the banner permanently undeliverable for older accepted quotes
The daemon re-posts the **stored** `valid_until_iso` ([daemon:351,498](apps/aberp/src/quote_pdf_rerender_daemon.rs:351)). The storefront rejects `valid_until < today` with `400` ([priced/+server.ts:55-58](../../../ABERP-site/src/routes/api/quotes/[id]/priced/+server.ts)) → daemon classifies `Permanent` → drops + audits. A stock downgrade on an accepted-but-aged quote (original validity lapsed) can never deliver the banner.
**Fix:** decide policy — either skip re-render for expired quotes up front (no point alerting on a dead quote) and audit `skipped_expired`, or refresh `valid_until` on re-render. Don't let it surface as an opaque `http_4xx` permanent failure.

### 🟡6 — S315 "cured the hang at its root" overclaims; S317 still had to de-gate
S315/PR-15 commit message: "cure the vitest CI hang at its root (forks-pool teardown deadlock)" and sets `pool: 'threads'` ([vite.config.ts:31](../../../ABERP-site/vite.config.ts)). Yet S317/PR-17 (later) de-gates the test job anyway, with the workflow comment "Vitest CI exit-hang … is under investigation in a follow-up … Re-gate once the real exit-handle leak is found" ([deploy.yml:15-19, 38-43](../../../ABERP-site/.github/workflows/deploy.yml)). The root cause is by their own words **not** found; S315 changed the pool but the hang persists on 2-core runners. The two artifacts contradict.
**Fix:** correct the S315 framing in the memory/runbook to "mitigation, not root-cause"; keep the de-gate honest.

### 🟡7 — CI test de-gate (S317) is operational debt; pin explicit re-gate conditions
[deploy.yml:38-47](../../../ABERP-site/.github/workflows/deploy.yml): `test` job has `continue-on-error: true` and **no** `needs:` from `build`/`deploy` — a failing or hanging test never blocks a production deploy. Acknowledged in-file as TEMPORARY. This means a real test regression can ship to prod unnoticed during the de-gate window.
**Re-gate conditions (make explicit in the workflow + memory):** (1) vitest CI exit-hang root cause found AND fixed; (2) the standalone `test` job green ≥5 consecutive `main` runs; (3) then fold `Test` back into `build` (gating) and delete the separate job. Until then, add a step that posts the test job's red/green to the PR so a regression is at least visible.

### 🟡8 — Customer-facing PDF banner exposes internal "DEAL" jargon
Both banner constants say "… DEAL frissíti az árképzést" / "… DEAL will refresh pricing" ([aberp-quote-pdf/src/lib.rs:58-62](crates/aberp-quote-pdf/src/lib.rs:58)). "DEAL" is internal ABERP saga jargon; a customer reading the quote PDF has no idea what it means.
**Fix:** reword to customer language, e.g. "Stock status changed since this quote was issued — final pricing will be confirmed on order." (Note: this text is duplicated in the storefront `/q/[id]` web view — keep them in sync.)

### 🟡9 — Storefront re-render is two non-atomic writes; transient PDF/flag disagreement
`writePricedPdfAtomic(id, …)` then `writeQuoteAtomic(id, rerendered)` ([priced/+server.ts:221,249](../../../ABERP-site/src/routes/api/quotes/[id]/priced/+server.ts)) are independently atomic but not jointly. A crash between them leaves the PDF carrying the banner while metadata `stock_alert` stays false → the HTML banner at `/q/[id]` (reads metadata) and the PDF disagree until the daemon retries. Self-healing (next same-hash post re-enters the flip branch), but a window exists.
**Fix:** write metadata first (flag flip) then PDF, or note the ordering rationale; at minimum add a comment that the daemon's idempotent retry is the recovery path.

---

## 🟢 Acknowledged trade-offs

1. **F12 audit ritual is complete** for all 3 new kinds — enum ([event_kind.rs:1828-1855](crates/audit-ledger/src/entry/event_kind.rs:1828)), `as_str` (1954-56), `from_storage_str` (2066-68), round-trip list (2170-72), and dedicated test `s325_pdf_rerender_kinds_distinct_and_round_trip` (4040). 🟢.
2. **Enqueue seam is correctly atomic** — flip + `QuoteStockAlertTriggered` + `QuotePdfRerenderEnqueued` in one tx, enqueue strictly **after** commit, idempotency keys on both audit rows, lost-race drops the tx ([quote_intake_query.rs:344-390](apps/aberp/src/quote_intake_query.rs:344)). Clean.
3. **Queue is idempotent + poison-safe** — `HashSet` dedups, poisoned lock degrades to "already there"/empty rather than panicking the request path ([queue.rs:57-72](apps/aberp/src/quote_pdf_rerender_queue.rs:57)).
4. **PDF banner is hardcoded bilingual constants** — no attacker-controlled text in the stock-alert branch ([lib.rs:247-254](crates/aberp-quote-pdf/src/lib.rs:247)); injection N/A. (Customer name/email flow is pre-existing, unchanged.)
5. **Supervisor** — panic-catch + 30s/5min burst backoff mirrors S286/S307 ([daemon:639-689](apps/aberp/src/quote_pdf_rerender_daemon.rs:639)). Sound.
6. **`scrub_for_audit` reused** on all error details ([daemon:460,485,521,…](apps/aberp/src/quote_pdf_rerender_daemon.rs:460)); bearer lives only in the `Authorization` header (not in URLs/bodies), so it cannot leak into audit/log payloads.
7. **Storefront `/priced` input validation is strong** — UUID regex, `blake3:` hash regex, CRLF/NUL header-injection rejection on version fields, content-type + 5 MB/6 MB size caps, past-date rejection ([priced/+server.ts:13-178](../../../ABERP-site/src/routes/api/quotes/[id]/priced/+server.ts)). `requireAdminAuth` gates the endpoint.
8. **No CRLF injection in the re-render log line** — `quote.priced_pdf_rerendered` logs `JSON.stringify` of UUID-validated `id` + regex-validated hash ([priced/+server.ts:255-263](../../../ABERP-site/src/routes/api/quotes/[id]/priced/+server.ts)); ABERP audit payloads are `serde_json`-serialized. Safe.

---

## Test-coverage gaps to close in S329 sweep

1. **No prod test that `stock_status_at_accept` is ever written** — the gap that hid 🔴1. Add an integration test driving the acceptance projection and asserting the column populates, then driving the recompute to a real FALSE→TRUE flip.
2. **No end-to-end "approved quote" re-post test** — every daemon test seeds a `Posted` job and a `FakeReposter`; none drives the real storefront state machine. A test that posts to an `approved` quote and asserts delivery (post-🔴2 fix) would have caught 🔴2/🔴3.
3. **409 classification test asserts the wrong thing** — [daemon:1011](apps/aberp/src/quote_pdf_rerender_daemon.rs:1011) pins `already_priced_with_different_hash` → success. After 🔴3, flip this to assert `Permanent`/`Conflict`.
4. **No crash/restart recovery test** for the queue (🔴4) — add one that enqueues, simulates a drop (drained Vec discarded), restarts with an empty queue, and asserts boot recovery re-enqueues from the ledger.
5. **No shutdown-during-poll test** (🟡3).
6. **Storefront `priced.spec.ts`** — does it cover an operator re-POSTing the same hash with `stock_alert:true` from a stale client, and a true→false same-hash post (must stay sticky-noop)? Verify the sticky semantics at [priced/+server.ts:216](../../../ABERP-site/src/routes/api/quotes/[id]/priced/+server.ts) are pinned.
7. **No test asserting the re-render PDF differs from the first render by the banner *only*** (🟡1) — render both, diff text, assert exactly the banner lines added and nothing else removed.

---

## Walkthrough-specific findings

Full adversarial read of `docs/runbooks/option-d-pilot-walkthrough.md` (V2, 918 lines). The doc is unusually thorough and most paths are code-verified, but **NOT safe to hand a non-author operator as-is** — two issues are 🔴-severity for an operator:

1. **🔴 (doc) Stale version pin — following it literally DOWNGRADES prod.** Lines 61-62, 74, 88, 101 target ABERP `PROD_v2.27.11` / storefront `9b5611d` (S313). Actual prod is `PROD_v2.27.13` / `c318850` (S323). `./run/upgrade_prod.sh PROD_v2.27.11` (line 88) rolls prod **back** two releases; the expected SHA `HEAD=9b5611d0a1b2` (line 101) never matches `origin`, so the verification check (line 320) loops the operator forever. **Fix:** bump to current release, or replace literal versions/SHAs with "latest `PROD_v*` tag" / "newest green SHA".
2. **🔴 (doc) Destructive upgrade with no caveat.** Lines 86-93 describe the upgrade as "verifies the tree is clean" — but `upgrade_prod.sh` runs `git reset --hard` + `git clean -fd` + prunes other `PROD_v*` branches. Any uncommitted/untracked file is silently destroyed; the wording actively misleads (it *forces* clean, doesn't verify-and-abort). **Fix:** add a red caveat + correct the WHY text.
3. **🟡 `pkill -f run_prod.sh` doesn't stop what the guard checks.** Line 120's failure advice kills the wrapper by command line, but the upgrade guard uses `pgrep -x aberp-ui`/`aberp` (binary name), and `run_prod.sh` `exec`s into the binary — so `pkill` may kill nothing while the binary keeps running, looping the operator. **Fix:** `pkill -x aberp-ui; pkill -x aberp`.
4. **🟡 References a non-existent `~/.aberp/prod/env.sh`.** Line 841 sends the operator to a file that does not exist and is never sourced. **Fix:** drop it; the disable var is per-invocation, not persisted.
5. **🟡 Rollback toggle described as needing `0`/`false`.** Line 846; `is_disabled()` only treats `1`/`true` as disable, and the inline env is one-shot (vanishes on relaunch from a new terminal) — never stated. **Fix:** "the var lives only for the single `run_prod.sh` invocation; relaunch with no prefix to re-enable."
6. **🟡 Prints the live admin bearer to the terminal with no scrollback caveat.** Lines 170-172, 452-459 dump the token (which also gates priced-writeback/catalogue) to stdout on a likely screen-shared morning-of session. **Fix:** caveat + suggest a non-printing `shasum`/`diff` comparison.
7. **🟢-ish minor:** "See also" (line 916) cites `bin/lightsail-deploy.sh` while the body + `deploy.yml:193` invoke the on-box `/home/aberp/lightsail-deploy.sh`; two names side-by-side can confuse someone locating the script on the box.

(No automated test for the runbook is intrinsic to docs; the value is keeping its commands matched to live code — items 1-3 are exactly that drift.)

---

## ABERP-side findings (PROD_v2.27.12 + PROD_v2.27.13)

Covered above: 🔴1 (no snapshot writer), 🔴2 (state-window exclusivity), 🔴3 (409-as-success), 🔴4 (no boot recovery / drained-batch loss); 🟡1 (hard-coded tolerance), 🟡2 (audit flooding / no attempt cap), 🟡3 (poll_once not cancellation-aware), 🟡8 (PDF "DEAL" jargon); 🟢 1-6. The `build_priced_multipart` `stock_alert` param flows correctly daemon→multipart→POST (verified: [daemon:191-199](apps/aberp/src/quote_pdf_rerender_daemon.rs:191) → [pipeline:1608-1631](apps/aberp/src/quote_pricing_pipeline.rs:1608), pinned by `priced_multipart_carries_stock_alert_true_for_rerender`). The `advance_render` first-render `stock_alert: false` literal ([pipeline:720](apps/aberp/src/quote_pricing_pipeline.rs:720)) is correct (pre-acceptance). The 21-site `AppState` integration initializes the queue once per construction (boot site [serve.rs:1017](apps/aberp/src/serve.rs:1017), not per-request) — no boot race on the queue itself.

## Storefront-side findings (PR-13..PR-17, PR-23)

Covered above: 🔴2/🔴3 root cause lives in [priced/+server.ts](../../../ABERP-site/src/routes/api/quotes/[id]/priced/+server.ts) (status-gated overwrite); 🟡4 (widened write surface / hash not verified vs PDF), 🟡5 (lapsed valid_until → 400), 🟡6 (S315 overclaim), 🟡7 (S317 de-gate debt + re-gate conditions), 🟡9 (two non-atomic writes); 🟢7-8 (strong validation, no CRLF). PR-13/PR-16 walkthrough findings in the dedicated section. PR-14 (`timeout-minutes` backstop) and PR-15 (`pool: 'threads'`) are sound in isolation — the concern is only that PR-15's "root cause" framing didn't hold (🟡6).

---

_End S328 review. 4 🔴 / 9 🟡 / 8 🟢. Headline: the customer-facing stock-alert banner arc is non-functional end-to-end in production (🔴1 + 🔴2), and the daemon's 409-as-success (🔴3) masks that as success in the audit trail._
