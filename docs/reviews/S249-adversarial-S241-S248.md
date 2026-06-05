# S249 — Adversarial review of S241–S248 (PROD_v2.8.4 → v2.12.0)

**Reviewer:** Claude Opus 4.7 (1M ctx), session 249
**Reviewed range:** S241 (PR-235, cac5582) → S248 (PR-241, d72a9cd), plus ABERP-site S244 (PR-Q, b0836e4)
**Mode:** Doc-only. No code changes. Findings are inputs to a future sweep PR (S250).

## Method

Five parallel investigation agents read the actual merge commits, the changed files in full, and the surrounding context (callers, audit-ledger plumbing, repo-side helpers). I spot-verified the four most explosive 🔴 claims against the live tree before propagating them here. Anything I could not confirm by reading the code was demoted or dropped.

The review is intentionally hard. Per `[[pushback-as-method]]`, friction is the deliverable.

---

## 🔴 CRITICAL

### 🔴 Finding 1: S245 / S247 / S248 adapters are not wired into the running binary

**Where:** `apps/aberp/src/mes_boot.rs:34-44` and `:81-160` — the only MES-adapter boot path. `apps/aberp/src/serve.rs:1377-1400` — the only caller.

**What:** Grep across `apps/` returns **zero** matches for `ZebraLabelPrinterAdapter`, `MtconnectAdapter`, or `UrRtdeAdapter`. `boot_mes_adapters()` hard-codes `BarcodeScannerAdapter::new(cfg)` and reads only `ABERP_BARCODE_SCANNER_*` env vars. Three PRs (PR-238/PR-240/PR-241), three release cuts (v2.10.0/v2.11.0/v2.12.0), ~3,800 LOC of adapter + tests — and no production tenant can drive a Zebra printer, an MTConnect machine, or a UR cobot. The Workshop TV "live adapter health" tile from S240 only ever shows the barcode-scanner row.

**Why it matters:** Three "feature" releases shipped, version-tagged, and pushed to origin during the review window changed observable behaviour of exactly nothing. CLAUDE.md rule 12 ("fail loud") is inverted — the release notes loudly proclaim shipping, the binary silently lacks the feature. This is the single most consequential gap in the review.

**Fix sketch:** Extend `boot_mes_adapters` (or refactor into per-adapter `boot_zebra`, `boot_mtconnect`, `boot_urrtde`) with `ABERP_{ZEBRA,MTCONNECT,URRTDE}_ENABLED` env switches mirroring the barcode pattern. Register each spawned task into `ShutdownCoordinator` (see finding 11). Add a single smoke test per adapter proving the boot path actually constructs the type and the registry sees it. Issue a release-note correction — the v2.10/11/12 changelogs are misleading.

---

### 🔴 Finding 2: Two distinct session ULIDs minted inside one QA-decide transaction

**Where:** `apps/aberp/src/serve.rs:9322` (QA-decide `ledger_actor`) and `apps/aberp/src/serve.rs:9357` (auto-complete `ledger_actor`). Both call `Actor::from_local_cli(Ulid::new().to_string(), operator_login)`.

**What:** A single SPA click writes the `QaInspectionDecided` audit row with session ULID `A` and the `WorkOrderStateChanged + WoCompletion` audit rows with session ULID `B`, inside the same `tx.commit()`. Verified by direct read of the route handler.

**Why it matters:** A forensic walker reconciling "what did one operator do in one click" cannot join these by `actor.session_id` — the audit ledger lies about whether this was one action or two. This is exactly the kind of "audit lies" CLAUDE.md rule 12 names. Pre-existing audit invariants assume session_id is stable across a tx.

**Fix sketch:** Compute the `Actor` once at route entry, clone it into both contexts. Or factor the inner `WoWriteContext` to borrow `ledger_actor` from the outer `QaWriteContext`. Add a test that pulls every audit row for one decide call and asserts a single `actor.session_id` across them. Sweep the audit-ledger crate for any other "mint a fresh Ulid inside a tx" pattern.

---

### 🔴 Finding 3: `try_auto_complete_wo` silently skips OnHold WOs with no operator signal

**Where:** `crates/aberp-work-orders/src/repository.rs:1648` — `!matches!(current_state, WorkOrderState::InProgress)` guard. Zero test coverage in `tests/wo_auto_complete.rs` for the OnHold case.

**What:** The auto-complete hook only fires when the WO is `InProgress`. A WO that was Held mid-production (`OnHold`) but whose final op got QA-passed after the hold returns `Ok(None)` — `wo_auto_completed: None` in the route response. The operator gets zero hint that they must Resume + manually Complete. The entire point of S243 was to spare them that step.

**Why it matters:** Violates `[[trust-code-not-operator]]` for the exact lifecycle this PR was built to automate. The audit ledger shows "last QA passed" but the WO sits stuck in OnHold until somebody notices the queue isn't draining.

**Fix sketch:** Either widen the guard to `InProgress | OnHold` (and have the auto-complete first emit `WoAction::Resume` then `Complete` — requires an audit-ordering decision), or — safer — surface a structured warning in `DecideQaInspectionResponse` like `auto_complete_blocked: { reason: "wo_on_hold", wo_id }` so the SPA can toast a clear "Resume the WO to complete it." Add a test `auto_complete_on_held_wo_returns_warning_not_silent_noop`.

---

### 🔴 Finding 4: `decide_qa(Rework)` bypasses the routing-op state machine and skips its audit emit

**Where:** `crates/aberp-qa/src/repository.rs:439-456` — direct `UPDATE routings SET state='active', completed_at=NULL` on Rework.

**What:** Rework mutates the routing-op row outside `transition_routing_op`, so no `RoutingOpStateChanged` ledger entry is written for the `Completed → Active` reverse flip. The forward `Active → Completed` carries the audit; the reverse only appears implicitly through `QaInspectionDecided{to_state: Reworking}`. Anyone reconstructing routing-op state from the audit ledger alone will be wrong.

**Why it matters:** This is pre-existing (S233/S238) but newly load-bearing in S243 — the auto-complete hook now relies on routing-op state implicitly via the QA gate. The day someone adds a "list routing-op transitions for this WO" query (an obvious next step), missing audits will misreport the cascade. Surfaces uncertainty CLAUDE.md rule 12 says to surface loud.

**Fix sketch:** Emit an explicit `RoutingOpStateChanged` audit on the Rework UPDATE — even if it duplicates info in the QA-decided payload, the ledger should be the canonical place to walk routing-op history. Or call back into `aberp_work_orders` to do the flip (requires breaking the dep-cycle the brief avoided). Add a test that walks the audit ledger after Rework and asserts the routing-op transition is recoverable.

---

### 🔴 Finding 5: Workshop SPA hard-crashes against a v2.10.0 backend during mid-upgrade skew

**Where:** `apps/aberp-ui/ui/src/routes/Workshop.svelte:460, 532, 619, 647, 766, 840, 860`.

**What:** Every new density block accesses `b.<rows>.length` and iterates the array directly. There are no `?? []` defaults, no `Array.isArray` guards, no optional chaining. A v2.10.1 SPA pointed at a v2.10.0 backend (operator restarted the Tauri shell before backend) receives a JSON response missing every new row array, and `{#if b.work_order_rows.length > 0}` throws `Cannot read properties of undefined (reading 'length')`. The entire Workshop page goes blank. The TS types in `api.ts:2853-2862` are non-optional, so the type system claims a contract the wire cannot honour mid-rollout. Verified.

**Why it matters:** ABERP's release model is per-branch (`[[aberp-release-model-s169]]`) and the wall-TV is the most likely place a mid-upgrade skew goes unnoticed. A blank wall-TV on the shop floor is the worst class of "completed successfully" (CLAUDE.md rule 12).

**Fix sketch:** Normalise the response shape at the API boundary in `lib/api.ts::getWorkshopDashboard` — default the six new arrays to `[]` and `today_invoice_total` to `0` after `invoke` returns. Matches the existing `parseNavUpstreamFault` "normalise at the boundary" pattern in `api.ts`. Cheaper diff than threading `?? []` through every access site. Mark the TS types as `Optional<T>` afterward so the compiler enforces handling at every future call site.

---

### 🔴 Finding 6: WO row ordering contradicts its own rendered "touched at" semantic

**Where:** `apps/aberp/src/serve.rs:11226-11248` (`build_work_order_rows`) reads from `list_work_orders(... LIMIT 5)` which orders by `created_at DESC, wo_id DESC` (`crates/aberp-work-orders/src/repository.rs:960`). The SPA renderer at `apps/aberp-ui/ui/src/routes/Workshop.svelte:469-475` displays `touched_at_iso8601` per row.

**What:** The five rows are sorted by `created_at` but timestamped (and labeled in HU relative time) by `touched_at`, a column-ladder fallback `cancelled_at → completed_at → started_at → released_at → created_at`. An idle WO created today but `on_hold` for hours will outrank a WO `released` 30 seconds ago. The brief's own justification on serve.rs:11220-11225 ("an idle WO reads 'released 12 órával ezelőtt' not 'created 6 napja'") is contradicted by the actual ordering — operator sees timestamps in disorder.

**Why it matters:** The "5 most-recently-touched WOs" semantic the brief promised is silently replaced by "5 most-recently-created WOs"; the operator's eye drifts to the wrong row. Displayed timestamp and sort key disagree, so the list looks unordered relative to its own timestamp column. This is exactly the kind of subtle correctness regression CLAUDE.md rule 12 names — the rows render, the page doesn't crash, but the semantics are silently wrong.

**Fix sketch:** Add `list_recent_work_orders_by_touched` to `aberp-work-orders` ordering by `COALESCE(cancelled_at, completed_at, started_at, released_at, created_at) DESC, wo_id DESC` and surface N rows server-side. Add a determinism pin: two WOs with identical touched_at must stay in `wo_id`-tiebreak order across calls.

---

### 🔴 Finding 7: `publicSiteUrl()` fail-opens to prod when env is missing — the Origin allowlist regresses with it

**Where:** ABERP-site `src/lib/server/public-url.ts:25-30`. Verified.

**What:** When `ABERP_SITE_PUBLIC_URL` is unset/empty/whitespace, `publicSiteUrl()` returns hardcoded `'https://abenerp.com'` — and that value becomes (a) the sole production Origin allowlist entry in `origin-check.ts:60`, (b) the host in every customer email link, (c) the canonical/og:url, (d) the sitemap pointer. A staging Lightsail box that loses its env var (`/etc/aberp-site.env` parse failure, container env-injection drop, operator forgot a line) starts advertising prod URLs AND accepting cross-origin POSTs whose `Origin: https://abenerp.com` — i.e. anyone who can submit a form from prod can drive the staging box's `/api/quote`.

**Why it matters:** Fail-open on env-missing is the canonical CSRF allowlist regression. PR-Q's commit message explicitly motivates the work by pointing at the "cross-environment links" footgun, then ships a default that re-introduces it on the security-critical path. The `public-url.ts` doc-comment even names "first-deploy moment work without explicit config" as a feature — directly conflating cosmetic and security defaults.

**Fix sketch:** Split the two consumers. Emails / sitemap / og:url keep the cosmetic default. The Origin allowlist must NOT — either compute it from `event.url.origin` (post-adapter-node header reconstruction), or refuse-to-start when the env is unset in production (mirror the `ABERP_SITE_ADMIN_TOKEN` 503 pattern in `auth.ts`). At minimum, `origin-check.ts` should call a separate `publicSiteUrlStrict()` that throws when unset.

---

### 🔴 Finding 8: `dev` import baked at build time; "no Origin → allow" lets JSON POSTs sail past both layers

**Where:** ABERP-site `src/lib/server/origin-check.ts:1` (import from `$app/environment`), `:60` (`dev ? [...] : [...]`), `:65-66` (no-Origin carve-out). Verified.

**What:** Two compounding issues. (1) `dev` from `$app/environment` is resolved at **build time** by Vite. Any CI misconfig that runs `vite build --mode development` (or sets `MODE=development`) ships a production bundle with `dev === true` baked in — silently widening the allowlist to localhost. Invisible at runtime; no env-var inspection reveals it. (2) The "no Origin → allow" carve-out trusts SvelteKit's `csrf.checkOrigin` to catch the browser case "before us" — but `csrf.checkOrigin` only fires for `multipart/form-data`, `application/x-www-form-urlencoded`, and `text/plain` POSTs (per SvelteKit source). A `Content-Type: application/json` POST with no Origin header sails through both layers.

**Why it matters:** `/api/quote` accepts multipart so it's covered today by the framework layer, but the moment any state-changing endpoint accepts JSON (the `/admin/quotes/[id]/status` route is one PR away from this), the gap opens. Build-time `dev` flags for security boundaries are the wrong place — security should depend on runtime env, not bundler config.

**Fix sketch:** Read dev-vs-prod from runtime env (`NODE_ENV !== 'production'` via `$env/dynamic/private`), not the bundler flag. Treat missing Origin as REJECT for state-changing endpoints — extract server-to-server callers into an explicit `assertSameOriginOrBearer` helper that lets the bearer-authed callers through by token, not by Origin-absence.

---

### 🔴 Finding 9: `too_many_arguments` allowed workspace-wide AND scattered as 34 redundant call-site suppressions

**Where:** `Cargo.toml:373` (`too_many_arguments = "allow"`) plus 34 `#[allow(clippy::too_many_arguments)]` across `apps/aberp/src/{drain_pending_retries,poll_ack,issue_invoice,retry_submission,drain_submission_queue,reports,issue_storno,…}`. Three NEW suppressions added post-S241 in S247/S248 (`crates/aberp-mes/src/adapters/mtconnect.rs:391` — a fresh 10-arg `run_poll_loop`).

**What:** S241 silenced the lint globally for ~10 legacy hits, then S247 added a fresh 10-arg function with its own per-callsite suppression on top. Both layers now redundantly applied. The workspace allow means clippy will no longer warn when the new MTConnect function grows to 12 args. The lint exists precisely to signal API-design pressure on hot code (the AP/poll/retry path); silencing it workspace-wide makes that signal invisible. `drain_pending_retries.rs` has FOUR suppressions in one file — that's not legacy ossification, it's a module that keeps growing args.

**Why it matters:** This is the canonical "hide the warning, lose the signal" trap. The brief skip-list was meant as a temporary debt marker; making it a workspace `allow` converts it to permanent invisibility. The brief's question — "are they getting worse?" — is answered yes: S247 added one more, S248 added zero (good, but the trend is still up).

**Fix sketch:** Flip workspace `allow` to `warn` (or `deny` with explicit local `allow`) so each function carries a visible suppression and a tracking comment. Use those as a TODO list — `drain_pending_retries.rs` (introduce `RetryCtx`) and `mtconnect::run_poll_loop` (group `client + url + max_response_bytes + machine_id` into `PollerConfig`) are obvious first targets. Keep `type_complexity` / `result_large_err` allows since those have legitimate cascade costs.

---

### 🔴 Finding 10: 17 of 21 PROD release branches missing `docs/releases/PROD_vX.Y.Z.md`

**Where:** `docs/releases/` contains only `PROD_v2.0.md`, `PROD_v2.1.1.md`, `PROD_v2.5.1.md`, `PROD_v2.9.0.md`. Missing for: 2.1.2, 2.3.1, 2.3.2, 2.4.0, 2.5.0, 2.6.0, 2.7.0, 2.7.1, 2.8.0–2.8.5, **2.10.0, 2.10.1, 2.11.0, 2.12.0** (the four cuts in this review window).

**What:** Per `[[aberp-upgrade-workflow-s200]]` and `[[aberp-versioning-policy-s201]]`, every release branch is supposed to carry an operator-facing release-notes file. The four that exist prove the convention; the other 17 silently broke it. The upgrade script doesn't read the file, so nothing prevents the omission.

**Why it matters:** The operator runbook step "read `docs/releases/PROD_v<version>.md` before flipping the prod branch" cannot be followed. The session-memory entries are the de-facto release notes today — but those live in Claude's private memory, not in the repo. A different operator (or a Claude session without the memory file loaded) is blind to what changed.

**Fix sketch:** Backfill the four in-window files (2.10.0/2.10.1/2.11.0/2.12.0) first; one paragraph each is enough. Add a `release.sh` precheck that refuses to push a release branch when `docs/releases/PROD_v$VERSION.md` is missing (parallels the dev-sentinel from S169). The precheck matters more than the backfill — without it, the next ten releases drift again.

---

## 🟡 IMPORTANT

### 🟡 Finding 11: UR-RTDE handshake can mis-decode by swapping `robot_mode` and `safety_mode` slots

**Where:** `crates/aberp-mes/src/adapters/ur_rtde.rs:843-861` (`validate_output_types`) and `decode_data_payload`.

**What:** `validate_output_types` zips the SETUP_OUTPUTS reply types with `EXPECTED_OUTPUT_TYPES` slot-by-slot. Slots 1 and 2 are both `INT32` (`robot_mode`, `safety_mode`). A controller (or a buggy / non-conformant RTDE responder) that returned the variables transposed would pass validation while `decode_data_payload` reads bytes in declared order — robot_mode bytes feed the safety_mode decoder and vice versa. The SETUP reply carries only types, not variable names, so the adapter cannot self-verify order from the ack alone.

**Why it matters:** Silent misclassification of robot/safety mode is exactly the failure mode CLAUDE.md rule 12 names. A `ProtectiveStop` could be logged as a benign mode change, undermining the e-stop audit trail. For a robot adapter, this is one of the most consequential silent-failure surfaces.

**Fix sketch:** Add a defensive sanity check on the first decoded frame: assert `robot_mode_code ∈ -1..=8` AND `safety_mode_code ∈ 1..=11`. If either is Unknown on frame 1, drop the connection and re-handshake, loud-failing per CLAUDE.md rule 12. Alternative: set up two recipes and infer order from differing recipe_id+types pairs. Cheapest first.

---

### 🟡 Finding 12: Reconnect backoff has no jitter — site-wide outage produces a thundering herd

**Where:** `crates/aberp-mes/src/adapters/ur_rtde.rs:961-968` (`next_backoff`) and the connect-loop body. Same shape will likely emerge in Zebra/MTConnect once they grow exp backoff (today both reconnect on a fixed interval).

**What:** `next_backoff` is deterministic doubling (500ms → 30s cap). A site-wide outage (shop-floor switch reboot) causes every UR adapter to start its 500ms backoff at the same moment and synchronise on the 30s cap. RTDE controllers refuse new connects during their own boot — synchronised retries can lock out the population for several cap-cycles.

**Why it matters:** As soon as a tenant has more than 3-4 cobots, the synchronised-retry pathology causes the cobot fleet to take longer to come back than they would have individually. Recovery time scales adversarially with adapter count.

**Fix sketch:** Decorrelated jitter, pulled into a shared `adapters/common::next_backoff_jittered(base, cap, last_delay)` helper. Seed RNG from `robot_id` so behaviour stays test-reproducible. Pulling the helper up doubles as a starting point for finding 13.

---

### 🟡 Finding 13: Three near-identical adapter scaffolds — duplication invites divergence drift

**Where:** `zebra.rs:212-403` vs `mtconnect.rs:228-376` vs `ur_rtde.rs:322-443`. Also `pick_free_port` / `cfg_for_test` repeated verbatim 3× in the test trees.

**What:** Each adapter re-implements: `Arc<Mutex<AdapterHealth>>` + `Mutex<Option<CancellationToken>>` + `Mutex<Option<JoinHandle<()>>>`, identical `start/stop/health/subscribe` impls, idempotency guard at start, take-cancel-then-await-handle in stop, even verbatim mutex-poison-panic strings. Plus duplicated helpers: `set_health`, classify-io-error patterns, the whole `last_state` baseline pattern.

**Why it matters:** Each future adapter author copies one of three subtly-different templates. The next round of "fix the start-idempotency check" lands in two of three and silently diverges (CLAUDE.md rules 7, 8, 11). At three concrete adapters the pattern is now load-bearing — the right time to extract is now, not after the fourth.

**Fix sketch:** Extract a `BackgroundAdapter` (or `LifecycleHarness`) helper in `adapters/mod.rs` owning the cancel + handle + health slot + start/stop/health impls. Each concrete adapter supplies one `async fn run(cancel, health_slot, sender)` closure. Pull `pick_free_port` to a `#[cfg(test)] mod test_support`. Per CLAUDE.md rule 13 ("delete before optimize"), the win here is delete-not-refactor.

---

### 🟡 Finding 14: PAUSE-on-shutdown's `stream.shutdown()` is unbounded

**Where:** `crates/aberp-mes/src/adapters/ur_rtde.rs:660-668` (cancel-arm of `run_stream_loop`).

**What:** On graceful shutdown the code wraps `write_frame(..PAUSE..)` in `tokio::time::timeout(pause_timeout, ...)` — good. Then unconditionally `stream.shutdown().await` — no timeout. If the controller's RX buffer is full / TCP window closed, `shutdown` can block indefinitely, defeating S213's graceful-shutdown deadline. Tauri window-close (S213) is expected to drain in tens of ms; a hung controller can hold the whole app from exiting.

**Why it matters:** User-visible "it won't quit" → SIGKILL becomes routine → `ShutdownCoordinator`'s entire reason for existing erodes. Same posture worth auditing in zebra/mtconnect; their connections are short-lived so risk is lower but the pattern is the same.

**Fix sketch:** Wrap `shutdown()` in the same `pause_timeout` (or a shared `graceful_close_budget`). On timeout, drop the stream — RST is acceptable; the controller already received PAUSE intent best-effort.

---

### 🟡 Finding 15: `AdapterRegistry` is not wired to `ShutdownCoordinator`

**Where:** `crates/aberp-mes/src/registry.rs:17-23` (struct doc) — no `ShutdownCoordinator`-aware constructor. Related to finding 1 — even when adapters get wired, the cancellation plumbing is missing.

**What:** Registry tells callers to "wrap in `Arc<Mutex<…>>` at the call site if shared." `start_all` / `stop_all` take `&self` and iterate sequentially, so a consumer mounting this behind an HTTP route AND a SIGTERM handler must externally serialise both. There's no automatic wiring such that the `ShutdownCoordinator` root token drains registered adapters.

**Why it matters:** Once finding 1 is fixed and adapters actually run in prod, a SIGTERM mid-stream will leak the connect loops + half-closed sockets unless the integrator manually wires `stop_all` into the shutdown handler. The pattern should be safe-by-default, not safe-if-you-remember.

**Fix sketch:** Add `AdapterRegistry::stop_on_cancel(CancellationToken)` that spawns a watcher firing `stop_all` on cancel. Have the consumer pass the `ShutdownCoordinator`'s token at registration time. Document the wiring contract in the registry's doc-header. Pin with a test using mock controllers that asserts cancellation-driven stop fires within a bounded deadline.

---

### 🟡 Finding 16: `health_snapshot` re-locks every adapter's health mutex per dashboard tick

**Where:** `crates/aberp-mes/src/registry.rs:105-119` (`health_snapshot`).

**What:** Walks `self.names()`, then for each name fetches the Arc, then calls `adapter.health()` which locks per-adapter `Mutex<AdapterHealth>`. For N adapters that's O(N) lock acquisitions per snapshot. The registry sits behind an external `RwLock<AdapterRegistry>` read-lock for the entire iteration, blocking concurrent `register`/`start`/`stop`. On a 100-machine plant polling at 1Hz that's 100 mutex round-trips per request.

**Why it matters:** Today this is fine (single-digit adapters), but the framework is sold for 100+ adapter plants. The brief specifically asked about scaling here. Zero tests pin snapshot latency, so the regression would not surface until prod (CLAUDE.md rule 9 — tests verify intent).

**Fix sketch:** Switch each adapter's health storage from `Arc<Mutex<AdapterHealth>>` to `arc_swap::ArcSwap<AdapterHealth>` — reads become a single atomic load, `health_snapshot` becomes lock-free. One-line per-adapter change; the lock disappears.

---

### 🟡 Finding 17: `wo_auto_complete.rs` tests don't pin that the hook itself decides to no-op

**Where:** `crates/aberp-work-orders/tests/wo_auto_complete.rs:300-304` — `decide_and_maybe_auto_complete` only calls `try_auto_complete_wo` when QA outcome is `Passed`.

**What:** All `None`-returning test cases (Fail, no-regress, Rework) short-circuit BEFORE the hook. The hook's own internal guards (gate-not-satisfied, WO not InProgress, WO not found, WO Cancelled) are not tested through the route-shaped path. The only direct hook-call test is the idempotency case (line 476) for already-Completed. If a refactor inverted the route-layer `matches!(…, Passed)` guard to always call the hook, every existing test would still pass.

**Why it matters:** Tests-verify-intent (CLAUDE.md rule 9). 715 lines of test coverage looks thorough; in practice it's not exercising the hook's guard semantics. All 12 tests can pass while the hook returns a constant `Ok(None)` for the InProgress case.

**Fix sketch:** Add direct-call tests for each internal guard: (a) WO in `Cancelled` returns None; (b) WO in `OnHold` returns None (see finding 3 for what it SHOULD do); (c) gate-not-satisfied returns None; (d) unknown WO id returns None. Drive them through the hook directly, not through the QA-state guard.

---

### 🟡 Finding 18: Cancelled WO between tx-start and auto-complete check — QA decide silently passes against terminal WO

**Where:** `crates/aberp-qa/src/repository.rs::decide_qa` (reads `qa_inspections` but not `work_orders.state`) + `try_auto_complete_wo` (silently returns `Ok(None)` for non-InProgress states).

**What:** Scenario: SPA-A tab Cancels a WO at 12:00:00 (commits). At 12:00:01 SPA-B tab's QA-decide tx starts (snapshot reads the now-Cancelled WO but its inspections are still live). `decide_qa` Passes the last inspection successfully. The auto-complete hook sees `Cancelled`, no-ops. The audit ledger now contains `QaInspectionDecided{Passed}` against a Cancelled WO with no warning anywhere.

**Why it matters:** "Pass an inspection on a Cancelled WO" is structurally meaningless. It should be refused at decide_qa, not silently accepted with the auto-complete soft-failing. CLAUDE.md rule 12 (fail loud). Today the route conflates "gate not satisfied" and "WO not eligible" into one Ok(None) — separate them.

**Fix sketch:** Add a WO-state gate inside `decide_qa` that refuses Pass/Fail/Rework on a `Cancelled` WO (Dispose stays legal — scrap is the natural outcome). Surface a warning on `DecideQaInspectionResponse` when the auto-complete hook skipped for a terminal-WO reason.

---

### 🟡 Finding 19: Auto-complete hook hardcodes `source_event_id: None` — adapter-driven causality is dropped

**Where:** `crates/aberp-work-orders/src/repository.rs:1665` — `source_event_id: None` in the `TransitionInputs` for the auto-complete call.

**What:** Even when the originating QA decide carried a `source_event_id` (today the route refuses it from the SPA, but adapter-fed decide paths exist per ADR-0063 §3 — see the test at line 578 setting `source_event_id: Some("evt_adapter")`), the `WorkOrderStateChanged` audit emitted by the auto-complete is event-orphaned. The forward link "adapter event → QA pass → WO completion" breaks at the second arrow.

**Why it matters:** ADR-0062 invariant 7 names `source_event_id` "load-bearing" precisely so adapter-driven cascades stay reconstructible. The hook silently drops it. Dormant today (no adapter-decide route), but the next adapter-fed QA-decide lands in prod with a half-traceable cascade.

**Fix sketch:** Plumb `outcome.inspection.source_event_id` (or the decide's input `source_event_id`) into the `try_auto_complete_wo` callsite and forward into the inner `transition_work_order` call. Add a property pin: adapter-driven QA-decide → auto-complete carries the same `source_event_id` across both audit rows.

---

### 🟡 Finding 20: `today_invoice_total` walks the full invoices table on every 10s wall-TV poll

**Where:** `apps/aberp/src/serve.rs:11409-11436` (`build_today_invoice_rows`).

**What:** Calls `list_invoices(state)` — the same reader powering the Outgoing tab — and filters in Rust to "today" rows. `list_invoices` is unbounded: every poll pulls every Own+ExtNav row the tenant has accumulated since day one, just to discard 99% and surface 5. As the ledger grows past a few thousand rows, every wall-TV poll becomes O(N) work + Vec allocation.

**Why it matters:** Wall-TV is the highest-frequency poll surface in the SPA (10s, 24h, multi-tab possible). By mid-year this becomes the single most expensive endpoint per request, for the most trivial "5 rows + a count" use case. The `[[aberp-workshop-tv-density-s246]]` memory is explicit about row vecs being capped — the implicit precondition that the filter is also bounded was violated.

**Fix sketch:** Add `count_invoices_issued_on(date)` + `list_invoices_issued_on(date, LIMIT)` on the invoice list reader that push the date filter into SQL `WHERE issue_date = ?`. Two cheap indexed queries; SPA wire shape unchanged.

---

### 🟡 Finding 21: Spotlight glow animation is clipped on every rotation

**Where:** `apps/aberp-ui/ui/src/routes/Workshop.svelte:1528-1553`.

**What:** CSS `animation: ws-spotlight-pulse 8s ease-in-out` runs when `data-spotlight="true"` is set. `spotlightTimer` advances `spotlightIdx` every 8s (`DEMO_SPOTLIGHT_TICK_MS`) — same value as the animation duration. The prior tile's fade-out (reserved for the last 25% of the timeline, ~2s) is hard-cut mid-fade on every rotation.

**Why it matters:** This IS the polish — it's what prospects see on the 65" demo wall. A clipped fade-out reads as "glitchy" not "polished." Picking the same value for tick and animation suggests "I copied the number" — choosing distinct values surfaces intent.

**Fix sketch:** Set `DEMO_SPOTLIGHT_TICK_MS` to at least animation duration + a small buffer (e.g. 9_000ms), or compress the keyframes so fade-out completes by 90% and align the durations explicitly.

---

### 🟡 Finding 22: H2 tap-to-toggle has no keyboard equivalent — operator-discipline-as-accessibility

**Where:** `apps/aberp-ui/ui/src/routes/Workshop.svelte:353-360`.

**What:** The H2 carries `onclick={() => tapDetector.tap()}` with both `a11y_click_events_have_key_events` and `a11y_no_noninteractive_element_interactions` lints suppressed. Rationale: "keyboard activation would expose the affordance." Result: a heading that mutates page state on a 5-mouse-tap gesture but offers no keyboard equivalent. Assistive-tech operators cannot reach demo mode at all.

**Why it matters:** Other operator surfaces (DispatchList, QaList) maintain "every action keyboard-reachable." This is a `[[trust-code-not-operator]]` adjacent: relying on operator-discipline + lack-of-discovery rather than separating the operator path from the guest-hiding goal.

**Fix sketch:** Move the gesture to a focusable but visually-blank affordance (an invisible button absolutely positioned in the H2 corner) bound to both `onclick` and a `Ctrl+Shift+D` keyboard shortcut. Operator gets keyboard reach; guest still has no visible cue. The "hide from guests" goal is separable from "remove keyboard path."

---

### 🟡 Finding 23: CloudFront cache-behavior table still routes `/robots.txt` and `/sitemap.xml` to S3

**Where:** ABERP-site `docs/aws/cloudfront-behaviors.md:22-23` and `docs/deploy.md:213`.

**What:** PR-Q deleted `static/robots.txt` + `static/sitemap.xml` and replaced them with prerendered SvelteKit endpoints at `src/routes/{robots.txt,sitemap.xml}/+server.ts` (marked `prerender = true`, served by `adapter-node` from `build/prerendered/`). But the CloudFront behaviors doc still pins both paths to the **S3** origin. Post-cutover, `/robots.txt` and `/sitemap.xml` return 404 from S3 via CloudFront.

**Why it matters:** Quiet outage — won't trip any smoke test because the dev box serves them locally fine. Memory file `[[s244]]` already flagged this; verifying the docs were not updated as part of the cutover means the flag is still live. SEO discovery breaks until somebody notices.

**Fix sketch:** Update the behavior tables to route `/robots.txt` and `/sitemap.xml` to the Lightsail origin alongside `/quote*`, `/api/*`, `/admin*`. Add a deploy-time smoke test that `curl`s both URLs and asserts 200 + correct content-type. Add a release-note line so the operator flips the behavior before deploying.

---

### 🟡 Finding 24: Origin allowlist accepts exactly one host — `www.` vs apex breaks every browser POST

**Where:** ABERP-site `src/lib/server/origin-check.ts:59-60`.

**What:** `expected` is exactly one string. If deploy sets `ABERP_SITE_PUBLIC_URL=https://abenerp.com` but the visitor lands on `https://www.abenerp.com` (both names are on the same CloudFront distribution per the deploy doc), browser sends `Origin: https://www.abenerp.com`, allowlist contains only `https://abenerp.com`, every form POST 403s with `origin_mismatch`. There is also no validation that the env value parses as a URL or starts with `https://`. An operator who pastes `abenerp.com` (no scheme), `https://abenerp.com/` (trailing slash), or `http://...` silently produces a broken allowlist.

**Why it matters:** Same drift PR-Q claims to harden against — just moved one env var to the side. Deploy doc explicitly says the `www.→apex` redirect is "deferred" while both names stay on the distribution, so `www.` traffic is a live concern, not hypothetical.

**Fix sketch:** Accept comma-separated `ABERP_SITE_PUBLIC_URL` (split + trim + drop empties + reject any entry that doesn't `new URL()` parse to an `https:` origin). Or auto-derive `[apex, "https://www." + apex]` when the env value's host has no `www.` prefix. Validate at boot — malformed entries throw 503-at-startup, not silently produce a 403-everything allowlist.

---

### 🟡 Finding 25: `/api/quote` buffers 50 MB into RAM before content-sniff rejects it; no rate limit

**Where:** ABERP-site `src/routes/api/quote/+server.ts:69-166`.

**What:** Origin check is first (good). Then `request.formData()` consumes the full 50 MB upload into memory, then `validateCadFile` runs on every file (also in memory). No early Content-Length pre-check, no per-IP rate limit (deploy doc line 402 confirms "Defer"). Attacker submits 10 × 5 MB `/dev/urandom` with `.step` extensions: same RAM cost as a real submission, fails validation, no audit trail of abuse. Lightsail Nano = 512 MB RAM total.

**Why it matters:** PR-Q's Origin check moves the goalposts but doesn't address the underlying "single-instance 512 MB box" reality. N concurrent uploads can OOM the box without ever passing CAD validation.

**Fix sketch:** Pre-check Content-Length before `request.formData()` (reject early if > 50 MB). Add per-IP token-bucket rate limit in front of the handler — in-memory is fine for the single-instance deploy, mirroring `email.ts`'s pattern.

---

### 🟡 Finding 26: `doc_lazy_continuation` + `doc_overindented_list_items` workspace allows may mask future clippy doc-lint consolidations

**Where:** `Cargo.toml:369-370`. Scope: `[workspace.lints.clippy]` — inherited via `[lints] workspace = true` in all 15 member manifests.

**What:** The two allows are narrow today (named lints), but nightly clippy occasionally rewrites diagnostic classes — `doc_markdown` once subsumed `doc_lazy_continuation`. A future clippy could fold doc-parsing into a broader `doc_*` class that this allow silently catches, missing legit doc-correctness signals (broken intra-doc links, malformed code-fences). The single `#[allow(missing_docs)]` in `apps/aberp/src/submit_invoice.rs:266` shows the project DOES care about doc lints in some places.

**Why it matters:** Workspace-level lint allows are cheap to add and invisible to remove — exactly the kind of debt that compounds silently.

**Fix sketch:** Add a `# clippy-1.95-baseline` comment to the two allows naming the clippy version they were observed in. On each `rust-version` bump, run `cargo clippy --workspace -- -W clippy::all -W clippy::pedantic | grep -i doc_` and re-evaluate scope.

---

### 🟡 Finding 27: Flat `EventKind` enum with prefix-by-storage-string-convention — collision pressure mounts

**Where:** `crates/audit-ledger/src/entry/event_kind.rs:235` (variant list) and `:1054-1107` (storage-string map). 40+ variants, three "namespaces" (`invoice.*`, `system.*`, `mes.*`) enforced purely via `as_str`.

**What:** Rust enum is flat — `DispatchCreated`, `WorkOrderCreated`, etc. No compile-time check that storage prefix matches Rust grouping. A future contributor naming `EventKind::Shipped` for outbound-invoice-shipped would compile fine even though the audit storage string says `mes.shipped`. The three-edit invariant (variant + as_str + from_storage_str) is enforced only by a round-trip unit test. `DispatchCreated` from S233 already conflicts ambiguously with any future SMS/email/notification dispatch.

**Why it matters:** Stage 3 strand keeps adding events — ~10 new variants in 3 releases. Audit-ledger names ARE schema; rename drift is data-schema drift. The variant count is still manageable today, which makes this the right window to nest, not after the next 20.

**Fix sketch:** Move to nested enums: `EventKind::Mes(MesEventKind)`, `EventKind::Invoice(InvoiceEventKind)`, `EventKind::System(SystemEventKind)`. The `as_str`/`from_storage_str` round-trip stays — but the Rust namespace and the storage namespace are now isomorphic by construction. Storage strings stay byte-identical; no audit-ledger rows need rewriting. File an ADR; defer landing to the next hygiene window.

---

### 🟡 Finding 28: `vec_init_then_push` rewrite in `reports.rs` lost the per-line git-blame trail

**Where:** `apps/aberp/src/reports.rs:1346-1352` (the `deferred_notes` `vec![...]` literal).

**What:** Before S241: each `deferred_notes.push("...")` was its own statement, blameable to the PR that added it, surgically removable. After: a single `vec![...]` literal. A future PR removing one item (because the feature finally lands) requires comma-management and can't be a one-line revert. `git blame` now names S241 for every entry.

**Why it matters:** Minor, but real. Deferred-notes are operator-visible — the kind of thing removed in a hurry on the morning of a tax-deadline release. The clippy lint suggested a style improvement the project's per-line evolution pattern doesn't benefit from.

**Fix sketch:** Either add per-line `// PR-XXX` markers re-stating origin sprint (cheap, makes blame survivable), or revert this hand-fix with `#[allow(clippy::vec_init_then_push)]` + one-line rationale. Not urgent; flag for the next time someone touches `compute_financial_report`.

---

## 🟢 NICE-TO-HAVE

### 🟢 Finding 29: `MtconnectAdapter` Content-Length pre-check fails open on chunked responses

**Where:** `crates/aberp-mes/src/adapters/mtconnect.rs:456-469`.

**What:** Content-Length is checked only when present. Chunked transfer encoding or HTTP/2 responses have no Content-Length header, so the adapter falls through and reads the full body before bounds-checking via `resp.bytes().await`. The post-read check is the actual guard.

**Why it matters:** Defence-in-depth that doesn't defend in the chunked case. Minor since reqwest is bounded by connection timeout. Real but narrow.

**Fix sketch:** Use `resp.bytes_stream()` with a running-byte-count check that aborts the stream once `max_response_bytes` is exceeded.

---

### 🟢 Finding 30: Event `type_tag` namespace is convention, not enforced

**Where:** `crates/aberp-mes/src/events.rs:148-158`.

**What:** The seven `CanonicalEvent` `type_tag`s (`part_moved`, `machine_state_changed`, `robot_state_changed`, etc.) are bare snake_case with no namespace prefix. They ride inside the `mes.adapter_event` EventKind, so storage-level collision is avoided by EventKind segregation. But future `mes.work_order_*` payloads with their own `"type"` discriminators have no enforcement against re-using `"type": "machine_state_changed"`.

**Why it matters:** A SQL `json_extract(payload, '$.type')` join across `mes.*` rows becomes a silent wrong-display bug if tags collide. Cheap to harden while vocab is small. Related to finding 27 but a different layer.

**Fix sketch:** Prefix the canonical-event type tags (`adapter.machine_state_changed`, `adapter.robot_state_changed`, …). Or add a unit test in `audit-ledger` that greps every EventKind's payload-emitting crate for `"type"` discriminator strings and asserts no overlap.

---

### 🟢 Finding 31: `wo_touched_at` clones each timestamp string on every call

**Where:** `apps/aberp/src/serve.rs:11255-11277`.

**What:** Function clones every fallthrough timestamp string per row. Cap-of-5 makes it trivial today, but it's new code per CLAUDE.md rule 13 — delete-first thinking on fresh helpers.

**Why it matters:** Cosmetic perf; row cap makes it a non-issue.

**Fix sketch:** Return `&str` borrowed from the `WorkOrder` reference; let the caller `.to_string()` once into the `WorkOrderRow` field.

---

### 🟢 Finding 32: Workspace-lints opt-in actually propagated to ALL 15 member crates (held)

**Where:** Confirmed grep across every `crates/*/Cargo.toml`, `modules/*/Cargo.toml`, `apps/*/Cargo.toml` — `[lints] workspace = true` present in 15/15. New S245/S247/S248 work inherits via `crates/aberp-mes/Cargo.toml:87`.

**What:** Subsequent PRs after S241 modified `crates/aberp-mes/Cargo.toml` to add `reqwest` + `quick-xml` deps; the `[lints] workspace = true` line survived intact.

**Why it matters:** This is the kind of thing that quietly breaks first — a contributor adds a `[dependencies]` table at the bottom and the `[lints]` table no longer parses, or a new sub-crate forgets the four lines. Worth noting it held across four post-S241 PRs.

---

## Summary tally

**10 🔴 / 18 🟡 / 4 🟢 = 32 findings.**

### Severity recap

**🔴 Critical (must fix before S250 ships or before more adapters land):**
- 1: Three new adapters not actually wired into the binary
- 2: Two ULIDs per QA-decide tx breaks audit session correlation
- 3: OnHold WOs silently skipped by auto-complete
- 4: Rework skips routing-op audit emit
- 5: Workshop SPA hard-crashes on backend version skew
- 6: WO row order contradicts the displayed timestamp semantic
- 7: ABERP-site `publicSiteUrl()` fail-open on missing env
- 8: ABERP-site `dev` baked at build time + no-Origin carve-out
- 9: `too_many_arguments` allowed workspace-wide silences API-design signal
- 10: 17 of 21 PROD releases missing operator-facing notes

**🟡 Important (should fix soon):** 11–28, covering adapter-framework duplication, RTDE protocol robustness, registry scaling, test-pinning gaps in S243, performance regressions on hot poll paths, ABERP-site security hardening, and workspace-lint posture.

**🟢 Nice-to-have:** 29–32.

### Cross-cutting themes

- **S243 audit gaps** (findings 2, 4, 19): three independent paths where the audit-ledger silently drops causality (ULID drift, Rework state-change, source_event_id). Convergence suggests the audit-ledger crate needs a single chokepoint for "create child ledger context from parent" instead of each route hand-rolling it.
- **S246 boundary discipline** (findings 5, 6, 20): the Workshop dashboard route + SPA pair has three independent failure modes from skipping normalisation at the API boundary. The fix shape is the same in all three places — normalise at `lib/api.ts::getWorkshopDashboard`.
- **Adapter framework not done** (findings 1, 11–16): the three adapters are scaffolded, tested at unit level, and unreachable from the binary. Until finding 1 lands, the other adapter findings are dormant — but the duplication and the missing ShutdownCoordinator wiring will compound once they go live. Recommended sequencing: land finding 13 (extract shared scaffold) BEFORE finding 1 (boot wiring) so the wiring lands against the deduplicated shape.
- **Security defaults regress to "open"** (findings 7, 8, 9, 26): four independent surfaces where the safer default would have been "fail closed / warn loudly" and instead chose "fail open / silence." Cumulatively this is a posture problem more than a code problem; consider an ADR on "fail-closed defaults" before the next batch of features lands.

### Recommended S250 sweep ordering

1. Finding 1 + Finding 13 together (extract scaffold, then wire to binary).
2. Findings 2, 4, 19 together (audit-ledger session/causality sweep).
3. Finding 5 (cheap, prevents the worst-class regression).
4. Finding 7 + Finding 8 together (ABERP-site CSRF posture).
5. Finding 6 (WO ordering fix).
6. Finding 10 (release-notes backfill + precheck).
7. Findings 3, 17, 18 together (S243 test + state-machine sweep).
8. Remainder triaged into v2.13.x / v2.14.x.
