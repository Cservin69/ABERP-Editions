# S319 / PR-19 — Workshop demo-mode + layout reorg: VERIFY-ONLY (already shipped)

**Verdict: SHIPPED.** The entire scope of this brief (the original S238 work —
hidden operator-gated demo-mode toggle on `#/workshop` plus the Recent-Activity
right-rail layout reorganization) shipped in **`f72d365` — PR-232, tag
`PROD_v2.8.1`** and was extended by three later sessions. No code was written
this session; the cut was refused per CLAUDE.md HARD RULE #2 (verify before
implementing).

## How verified

Per-file verify pass against the brief's five toggle requirements + layout reorg:

| # | Requirement | Status | Evidence |
|---|-------------|--------|----------|
| 1 | `isDemoMode()` / `setDemoMode()` persisted toggle | ✅ SHIPPED | [workshop-demo-mode.ts:62-73](../../apps/aberp-ui/ui/src/lib/workshop-demo-mode.ts) — `localStorage` key `aberp:workshop:demo-mode`, closed-vocab `{on,off}` discard fallback. Brief floated `Ctrl+Shift+D`; ship uses a **5-tap-within-2s gesture on the page H2** (`createTapDetector`, [lines 102-134](../../apps/aberp-ui/ui/src/lib/workshop-demo-mode.ts)) — a more tour-friendly, operator-only handle. |
| 2 | Reload-survives | ✅ SHIPPED | `loadDemoMode` reads `localStorage` at component init: `let demoMode = $state(isDemoMode())` ([Workshop.svelte:109](../../apps/aberp-ui/ui/src/routes/Workshop.svelte)). |
| 3 | Mock payload `getMockDashboard()` | ✅ SHIPPED | [workshop-mock-data.ts:37](../../apps/aberp-ui/ui/src/lib/workshop-mock-data.ts) — full `WorkshopDashboard`-shaped payload. Content matches the brief's guidelines: WOs mixed across all 6 states (18 total), **3** low-stock rows, **6** QA pending (mostly passed), 4 eligible + 2 pending dispatch, today 4,287,400 HUF + 12,450 EUR, **all adapters render Running** (one `unhealthy` in raw payload, suppressed in demo mode to prove the override), canonical partner/product/WO-number sets, **18 recent-activity entries** scripted over the last ~88 min. |
| 4 | Fetcher branches on `isDemoMode()` | ✅ SHIPPED | [api.ts:3815-3817](../../apps/aberp-ui/ui/src/lib/api.ts) — `getWorkshopDashboard()` returns `normalizeWorkshopDashboard(await getMockDashboard())` when `isDemoMode()`. |
| 5 | Subtle operator-only indicator | ✅ SHIPPED | `.ws-head__demo-dot` — 6px corner dot, `opacity: 0.3`, `pointer-events:none` ([Workshop.svelte:1064-1077](../../apps/aberp-ui/ui/src/routes/Workshop.svelte)). |

### Layout reorganization

Shipped grid (`.ws-grid` [Workshop.svelte:1101-1104](../../apps/aberp-ui/ui/src/routes/Workshop.svelte)) matches the brief's target **verbatim**:

```
grid-template-columns: 1fr 1fr 1fr minmax(280px, 1fr);
grid-template-areas:
  "wo       wo       wo      recent"
  "qa       dispatch dispatch recent"
  "adapters lowstock today    recent";
```

Recent Activity is the full-height right rail (spans all 3 rows); Low Stock +
Today moved to the bottom row alongside Adapters. **Before** (PR-231/S235, the
predecessor) was a flat tile flow; the `grid-template-areas` rewrite is the S238
reorg.

Mobile collapse present at three breakpoints (`recent` drops to a full-width
bottom tile):
- `≤1280px`: 3-col, `recent recent recent` bottom row
- `≤960px`: 2-col stack
- `≤720px`: single-column stack of every tile in source order
([Workshop.svelte:1670-1711](../../apps/aberp-ui/ui/src/routes/Workshop.svelte)).

### Demo-mode polish (brief's "Polish" section)

- **Number transitions**: `getMockDashboard(now)` is timestamp-relative; recent
  activity ages forward as the page stays open.
- **Auto-scroll/cycle**: `.ws-activity--demo` class + `$effect` theater timers
  ([Workshop.svelte:223-227, 953](../../apps/aberp-ui/ui/src/routes/Workshop.svelte)).
- **Scan-message ticker**: `MOCK_SCAN_MESSAGES` cycled every ~3.5s on the
  barcode-scanner adapter tile ([Workshop.svelte:776-785](../../apps/aberp-ui/ui/src/routes/Workshop.svelte)).
- **Spotlight rotation**: `MOCK_SPOTLIGHT_TILES` cycles tile highlight
  ([Workshop.svelte:188-190, 368-369](../../apps/aberp-ui/ui/src/routes/Workshop.svelte)).
- **Adapter suppression**: every adapter forced `healthy` in demo mode
  ([Workshop.svelte:135](../../apps/aberp-ui/ui/src/routes/Workshop.svelte)).

## Later-session extensions (still in scope, still green)

- **S240 / PR-234** (`e45ee47`) — adapter tile reads live registry.
- **S246 / PR-239** (`cb3f505`) — density rows: `work_order_rows`,
  `low_stock_rows`, `pending_qa_rows`, dispatch rows, `today_invoice_rows`
  + "+N more" footers; mock data extended to match.
- **S256 / PR-245** (`0d73590`) — quote-arrival glyph in recent activity.
- **S258 / PR-247** (`3cc04e4`) — wall-TV adapter-health alerts; demo-mode
  suppression of the `unhealthy` adapter exercised end-to-end.

## Gates (confirming shipped state is sound)

- `npm run build` ✅ — 256 modules, built clean.
- `npx vitest run` ✅ — **1079/1079** passing (57 files), matches the standing
  baseline. Demo-mode + mock-data tests
  (`workshop-demo-mode.test.ts`, `workshop-mock-data.test.ts`,
  `workshop-format.test.ts`, `workshop-dashboard-safe-degrade.test.ts`) all pass.
- Cargo gates not run: frontend-only verdict, zero Rust touched, zero files
  changed — no diff to gate.

## Conservative calls

- **Refused the cut** rather than re-implementing. Brief HARD RULE #2 anticipated
  this exact outcome ("S238 was specced for this scope; may be fully shipped").
  It is fully shipped — re-implementing would have churned working, polished,
  thrice-extended code for nothing.
- **No `git status` mutation**: the only artifacts this session are this report +
  the MEMORY.md index pointer + memory topic file. No source under
  `apps/aberp-ui/` was touched.
- The brief's `Ctrl+Shift+D` handler and the `getMockDashboard()` (sync) /
  per-call jitter wording differ cosmetically from the ship (5-tap gesture;
  timestamp-relative aging). The shipped behavior **satisfies the intent** of
  every requirement — flagged here rather than "corrected," because changing a
  shipped operator gesture would be a regression risk with no benefit.

## Outcome

No version cut. No `PROD_v2.27.13`. Dispatch should **not** spawn a utility cut
for this brief — the scope is already in production as `PROD_v2.8.1` and later.
Branch `session-319/pr-19-workshop-verify-only` carries this report only.
