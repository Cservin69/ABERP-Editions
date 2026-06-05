// S238 / PR-232 — Workshop demo-mode mock payload.
//
// Returns a synthetic `WorkshopDashboard` matching the real Tauri
// `get_workshop_dashboard` response shape, so `Workshop.svelte`
// renders it without knowing the source. The numbers are tuned to
// a "Q2 mid-day production" feel — not zeroed out, not overloaded,
// no red flares. Twelve to twenty recent-activity entries spread
// over the last 90 minutes give the timeline enough scroll to
// look alive without the operator having to invent a story.
//
// Timestamps are computed relative to a caller-supplied `now`
// (defaulting to `new Date()`), so the mock ages forward naturally
// while the page is open — `X perccel ezelőtt` stays believable
// across a multi-minute tour.
//
// Partner / WO / product names are deliberately neutral Hungarian-
// industry-sounding strings; the brief lists the canonical set.

import type {
  AdapterStatusSnapshot,
  EligibleDispatchRow,
  LowStockItemRow,
  PendingDispatchRow,
  PendingQaRow,
  RecentActivityEntry,
  TodayInvoiceRow,
  WorkOrderRow,
  WorkshopDashboard,
} from "./api";

const MIN_MS = 60 * 1000;

/** Build a mock `WorkshopDashboard`. The shape MUST match the real
 *  response — `workshop-mock-data.test.ts` pins this against the
 *  exported types so a backend widening is caught at vitest time
 *  rather than at render time. */
export function getMockDashboard(
  now: Date = new Date(),
): WorkshopDashboard {
  const todayDate = formatIsoDate(now);
  return {
    work_orders: {
      created: 4,
      released: 6,
      in_progress: 5,
      on_hold: 2,
      completed: 1,
      cancelled: 0,
    },
    low_stock_products: { count: 3 },
    qa: {
      pending: 6,
      passed: 4,
      failed: 0,
      reworking: 1,
      disposed: 0,
    },
    dispatch: {
      by_state: { drafted: 2, shipped: 5, cancelled: 0 },
      eligible_work_orders: 4,
      shipped_today: 3,
    },
    today: {
      date: todayDate,
      issued_count_huf: 6,
      issued_count_eur: 2,
      // 4,287,400 HUF gross — six small-to-medium invoices.
      gross_revenue_huf_minor: 428_740_000,
      // 12,450 EUR gross — two EUR invoices for the export side.
      gross_revenue_eur_minor: 1_245_000,
    },
    recent_activity: buildRecentActivity(now),
    adapters: buildAdapters(),
    snapshot_at_iso8601: now.toISOString(),
    // S246 / PR-239 — density rows. Counts/cadence per the brief
    // (5 WOs, 3 low-stock, 7 QA, 4 eligible + 2 pending dispatch,
    // 5 invoice rows + 8 total so the "+3 more" footer surfaces).
    work_order_rows: buildWorkOrderRows(now),
    low_stock_rows: buildLowStockRows(),
    pending_qa_rows: buildPendingQaRows(now),
    eligible_dispatch_rows: buildEligibleDispatchRows(now),
    pending_dispatch_rows: buildPendingDispatchRows(now),
    today_invoice_rows: buildTodayInvoiceRows(now),
    today_invoice_total: 8,
  };
}

// ── Recent activity ─────────────────────────────────────────────

/** Activity entries — 18 events spanning the last ~88 minutes.
 *  Order: newest first, matching the real payload's ORDER BY desc.
 *  EventKind strings mix `system.` prefixed (post-S177 convention)
 *  and bare names, so `fmtEventKind` is exercised in both modes. */
function buildRecentActivity(now: Date): RecentActivityEntry[] {
  const base = now.getTime();
  // [offsetMinutes, kind] — list is in chronological order
  // (oldest first) so editors can read it as a timeline; we reverse
  // at the end for newest-first output.
  const script: Array<[number, string]> = [
    [88, "system.work_order_created"],
    [82, "system.work_order_released"],
    [76, "system.work_order_released"],
    [71, "InvoiceDraftCreated"],
    [65, "system.qa_check_passed"],
    [60, "system.qa_check_passed"],
    [54, "system.dispatch_drafted"],
    [49, "system.work_order_in_progress"],
    [44, "InvoiceIssued"],
    [38, "system.qa_check_passed"],
    [33, "system.dispatch_shipped"],
    [28, "system.work_order_completed"],
    [24, "InvoiceDraftCreated"],
    [19, "system.qa_check_passed"],
    [15, "system.dispatch_shipped"],
    [11, "InvoiceIssued"],
    [6, "system.qa_check_reworking"],
    [2, "system.dispatch_shipped"],
  ];
  const out: RecentActivityEntry[] = [];
  // Reverse so newest is first. `seq` mirrors the audit-ledger
  // monotonic counter — seq is highest for the newest entry.
  for (let i = script.length - 1; i >= 0; i--) {
    const [offsetMin, kind] = script[i];
    const t = new Date(base - offsetMin * MIN_MS);
    out.push({
      id: `mock-${1000 + i}`,
      kind,
      at_iso8601: t.toISOString(),
      seq: 1000 + i,
    });
  }
  return out;
}

// ── Adapters ────────────────────────────────────────────────────

/** Three MES adapters — all `healthy` (S240 / PR-234 vocab —
 *  was `enabled` pre-live-registry). The barcode scanner is the one
 *  whose rotating "Last scan" messages feed the demo-mode
 *  Workshop.svelte ticker. The rotation itself lives in
 *  `Workshop.svelte` because it needs a Svelte effect; this list
 *  just supplies the static adapter metadata. */
function buildAdapters(): AdapterStatusSnapshot[] {
  return [
    {
      name: "barcode-scanner-01",
      status: "healthy",
      kind: "barcode-scanner",
      host: "192.168.42.21",
      port: 4001,
    },
    {
      name: "mes-printer-bay-A",
      status: "healthy",
      kind: "label_printer",
      host: "192.168.42.22",
      port: 9100,
    },
    {
      name: "scale-shipping-01",
      status: "healthy",
      kind: "weight_scale",
      host: "192.168.42.23",
      port: 4003,
    },
  ];
}

// ── Scan-message ticker (demo mode polish) ──────────────────────

/** Fake "incoming scan" messages cycled by the demo-mode Workshop
 *  page on a ~3-5s rotation. The values reference plausible WO and
 *  part numbers from the brief's neutral-Hungarian-industry name
 *  set; the seconds-ago suffix is rendered by the component so the
 *  list itself stays time-independent (and trivially pinnable). */
export const MOCK_SCAN_MESSAGES: readonly string[] = [
  "WO-2026-00428 — Manifold T4",
  "PART-MFLD-T4 ×12",
  "WO-2026-00431 — Tartó 240mm",
  "PART-BRKT-240 ×24",
  "WO-2026-00429 — Burkolat 12-A",
];

// ── Spotlight rotation list (tile highlight cycle) ──────────────

/** `data-testid` keys for the tiles the demo-mode spotlight rotates
 *  through. Keeping this list here (rather than inlining it in
 *  `Workshop.svelte`) means a layout refactor doesn't drift from
 *  the rotation script. */
export const MOCK_SPOTLIGHT_TILES: readonly string[] = [
  "tile-work-orders",
  "tile-qa",
  "tile-dispatch",
  "tile-adapters",
  "tile-low-stock",
  "tile-today",
  "tile-recent-activity",
];

// ── Density rows (S246 / PR-239) ────────────────────────────────
//
// Each tile gains a list of underlying items below the existing
// quick-focus number. The mock data here mirrors the shape the
// real backend payload carries (see `serve.rs::WorkshopDashboard`
// row structs) so the SPA renders identically against either source.

/** 5 mock WO rows spanning the WO state vocab — released, in_progress,
 *  on_hold, completed, and one created. Touched-at offsets span the
 *  last ~6 hours so the `fmtRelativeTime` formatter exercises both
 *  the minute and hour branches. Product names map to the brief's
 *  generic CNC-part vocab. */
function buildWorkOrderRows(now: Date): WorkOrderRow[] {
  const base = now.getTime();
  const HR = 60 * MIN_MS;
  return [
    {
      wo_id: "wo_mock_00428",
      wo_number: "WO-2026-00428",
      product_name: "Manifold T4",
      state: "in_progress",
      touched_at_iso8601: new Date(base - 18 * MIN_MS).toISOString(),
      qty_target: "12",
    },
    {
      wo_id: "wo_mock_00431",
      wo_number: "WO-2026-00431",
      product_name: "Tartó 240mm",
      state: "released",
      touched_at_iso8601: new Date(base - 1.4 * HR).toISOString(),
      qty_target: "24",
    },
    {
      wo_id: "wo_mock_00429",
      wo_number: "WO-2026-00429",
      product_name: "Burkolat 12-A",
      state: "on_hold",
      touched_at_iso8601: new Date(base - 2.7 * HR).toISOString(),
      qty_target: "6",
    },
    {
      wo_id: "wo_mock_00426",
      wo_number: "WO-2026-00426",
      product_name: "Manifold T4",
      state: "completed",
      touched_at_iso8601: new Date(base - 4.1 * HR).toISOString(),
      qty_target: "8",
    },
    {
      wo_id: "wo_mock_00432",
      wo_number: "WO-2026-00432",
      product_name: "Csapágy 80B",
      state: "created",
      touched_at_iso8601: new Date(base - 5.8 * HR).toISOString(),
      qty_target: "40",
    },
  ];
}

/** 3 mock below-min product rows with bin codes following the
 *  brief's `A-12-3`-style three-segment alpha-numeric grid. */
function buildLowStockRows(): LowStockItemRow[] {
  return [
    {
      product_id: "prod_mock_brkt_240",
      name: "Tartó 240mm",
      stock_qty: "4",
      min_stock: "20",
      bin_location: "A-12-3",
    },
    {
      product_id: "prod_mock_mfld_t4",
      name: "Manifold T4",
      stock_qty: "2",
      min_stock: "8",
      bin_location: "B-04-1",
    },
    {
      product_id: "prod_mock_hsg_12a",
      name: "Burkolat 12-A",
      stock_qty: "1",
      min_stock: "5",
      bin_location: "C-09-2",
    },
  ];
}

/** 7 mock Pending QA inspections — oldest first per ADR-0063 §8 so
 *  the list reads "longest waiting at the top." Operator-visible op
 *  names use HU manufacturing vocab (`Marás`, `Festés`, …). */
function buildPendingQaRows(now: Date): PendingQaRow[] {
  const base = now.getTime();
  const HR = 60 * MIN_MS;
  // [minutesAgo, woNumber, opName]
  const script: Array<[number, string, string]> = [
    [4.2 * 60, "WO-2026-00410", "Marás"],
    [3.6 * 60, "WO-2026-00412", "Esztergálás"],
    [2.8 * 60, "WO-2026-00415", "Festés"],
    [2.1 * 60, "WO-2026-00418", "Csiszolás"],
    [1.5 * 60, "WO-2026-00422", "Marás"],
    [38, "WO-2026-00425", "Festés"],
    [12, "WO-2026-00428", "Mérés"],
  ];
  return script.map(([minsAgo, woNumber, opName], i) => ({
    qa_id: `qa_mock_${1000 + i}`,
    wo_id: `wo_mock_${woNumber.slice(-5)}`,
    wo_number: woNumber,
    routing_op_id: `rop_mock_${1000 + i}`,
    op_name: opName,
    created_at_iso8601: new Date(base - minsAgo * MIN_MS).toISOString(),
  }));
}

/** 4 mock Eligible WO rows — Completed WOs without a dispatch row.
 *  Oldest first so the longest-waiting heads the list. */
function buildEligibleDispatchRows(now: Date): EligibleDispatchRow[] {
  const base = now.getTime();
  const HR = 60 * MIN_MS;
  const script: Array<[number, string, string, string]> = [
    [7.8 * HR, "WO-2026-00401", "Tartó 240mm", "16"],
    [5.4 * HR, "WO-2026-00405", "Manifold T4", "10"],
    [3.2 * HR, "WO-2026-00409", "Csapágy 80B", "24"],
    [1.6 * HR, "WO-2026-00417", "Burkolat 12-A", "6"],
  ];
  return script.map(([msAgo, woNumber, productName, qty]) => ({
    wo_id: `wo_mock_${woNumber.slice(-5)}`,
    wo_number: woNumber,
    product_name: productName,
    qty_target: qty,
    completed_at_iso8601: new Date(base - msAgo).toISOString(),
  }));
}

/** 2 mock Drafted dispatches with neutral-Hungarian-industry partner
 *  names from the brief's canonical fake set. Newest first. */
function buildPendingDispatchRows(now: Date): PendingDispatchRow[] {
  const base = now.getTime();
  const HR = 60 * MIN_MS;
  return [
    {
      dsp_id: "dsp_mock_0007",
      wo_id: "wo_mock_00420",
      wo_number: "WO-2026-00420",
      partner_name: "Acme Manufacturing Kft.",
      created_at_iso8601: new Date(base - 0.5 * HR).toISOString(),
    },
    {
      dsp_id: "dsp_mock_0006",
      wo_id: "wo_mock_00414",
      wo_number: "WO-2026-00414",
      partner_name: "Stellar Industries Zrt.",
      created_at_iso8601: new Date(base - 2.4 * HR).toISOString(),
    },
  ];
}

/** 5 mock issued-today invoices spanning HUF + EUR, with HU+EU
 *  partner names. The parent payload's `today_invoice_total` is 8
 *  so the SPA renders "+3 more" below the 5 surfaced rows. */
function buildTodayInvoiceRows(now: Date): TodayInvoiceRow[] {
  const today = formatIsoDate(now);
  return [
    {
      invoice_id: "inv_mock_2026_0428",
      sequence_number: 428,
      fiscal_year: 2026,
      currency: "HUF",
      total_gross_minor: 87_500_000,
      buyer_name: "Acme Manufacturing Kft.",
      issue_date: today,
    },
    {
      invoice_id: "inv_mock_2026_0427",
      sequence_number: 427,
      fiscal_year: 2026,
      currency: "HUF",
      total_gross_minor: 142_300_000,
      buyer_name: "Northstar Co.",
      issue_date: today,
    },
    {
      invoice_id: "inv_mock_2026_0426",
      sequence_number: 426,
      fiscal_year: 2026,
      currency: "EUR",
      total_gross_minor: 685_000,
      buyer_name: "Stellar Industries GmbH",
      issue_date: today,
    },
    {
      invoice_id: "inv_mock_2026_0425",
      sequence_number: 425,
      fiscal_year: 2026,
      currency: "HUF",
      total_gross_minor: 56_840_000,
      buyer_name: "Magyar Gépgyár Zrt.",
      issue_date: today,
    },
    {
      invoice_id: "inv_mock_2026_0424",
      sequence_number: 424,
      fiscal_year: 2026,
      currency: "EUR",
      total_gross_minor: 560_000,
      buyer_name: "Stellar Industries GmbH",
      issue_date: today,
    },
  ];
}

// ── Helpers ─────────────────────────────────────────────────────

/** YYYY-MM-DD, Budapest-local-ish (uses the host's local date).
 *  Matches the real `TodayPanel.date` shape (a bare ISO date, not
 *  a full timestamp). */
function formatIsoDate(d: Date): string {
  const y = d.getFullYear();
  const m = String(d.getMonth() + 1).padStart(2, "0");
  const day = String(d.getDate()).padStart(2, "0");
  return `${y}-${m}-${day}`;
}
