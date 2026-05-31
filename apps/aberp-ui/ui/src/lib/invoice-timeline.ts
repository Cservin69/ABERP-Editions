// PR-67 / session-89 — pure-module mapper from the SPA-side
// `AuditEntryView[]` wire shape to a renderer-friendly
// `TimelineNode[]` array. Drives the visual lifecycle timeline that
// replaces the dense audit-row table in `InvoiceDetail.svelte`.
//
// Closed-vocab posture per CLAUDE.md rule 11: the kind switch matches
// the operator-meaningful EventKind set the detail modal already
// surfaces (issuance, submission attempt, ack-status, storno,
// modification, payment recorded, and email-sent — the last forking on
// its `outcome` field, S163). Every other kind — `InvoiceSequenceReserved`,
// `InvoiceSubmissionResponse`, `InvoiceRetryRequested`,
// `InvoiceMarkedAbandoned`, the four annulment kinds,
// `InvoiceSubmissionAttemptFailed`, `InvoiceCheckPerformed` — falls
// through to the `kind-default` `•` glyph so a backend drift is
// visible per CLAUDE.md rule 12 rather than silently bucketing with
// a known kind. The "raw table" toggle in `InvoiceDetail.svelte`
// preserves access to every kind verbatim for power-user inspection.
//
// `InvoiceAckStatus` forks four ways by the typed payload's
// `ack_status` field (the four NAV v3.0 `processingResult` values per
// ADR-0009 §2 / `AckStatus` in `./api.ts`). The brief named glyphs
// for SAVED / PROCESSING / ABORTED; RECEIVED reuses the `⇣` glyph
// from `labels.ts` ACK_LABELS so the four-way fork is exhaustive
// over the typed union — an unmodelled ack literal (backend drift)
// falls to the same `kind-default` `•` as an unknown EventKind.
//
// No Svelte deps; pinned by `invoice-timeline.test.ts`.

import type { AuditEntryView } from "./api";

/** One rendered row in the timeline. The shape is what the Svelte
 * component consumes verbatim — the helper does all kind-dispatch
 * and string composition so the renderer stays presentational. */
export interface TimelineNode {
  /** Stable key for the Svelte `#each` block. Composed from the
   * audit-ledger `seq` (append-only primary key per ADR-0008 —
   * unique per ledger). String for `#each` key-shape ergonomics. */
  id: string;
  /** Operator-facing single-line label (e.g. "Invoice issued",
   * "NAV ack: SAVED"). The `_html_safe` suffix is a contract with
   * the renderer that this string is plain text and may be
   * dropped into `{label_html_safe}` without HTML escaping — the
   * helper composes it from typed wire fields, never operator
   * input, so it is HTML-safe by construction. Svelte's `{...}`
   * expression also escapes by default; the suffix names the
   * contract regardless. */
  label_html_safe: string;
  /** RFC3339 timestamp verbatim from `entry.occurred_at`. For the
   * `<time datetime=>` machine-readable attribute. */
  ts_iso: string;
  /** Operator-facing timestamp string. S195 — Hungarian-locale
   * relative-time when the entry is within the last week
   * ("2 órája", "tegnap", "3 napja"), and an absolute Hungarian
   * date+time ("2026. 05. 30. 14:32") for older entries. Computed
   * from `now` passed into [`timelineFromAuditEntries`] so the
   * pure-module contract stays testable. Pre-S195 this was identical
   * to `ts_iso` (raw RFC3339) and the timeline read as "what's a
   * Z-suffixed UTC string supposed to mean to an accountant?" */
  ts_display: string;
  /** Absolute Hungarian-locale timestamp string ("2026. 05. 30.
   * 14:32"), always rendered in Europe/Budapest. S195 — surfaces
   * via the `<time title=>` attribute so an operator can hover the
   * relative-time chip and read the exact instant without dropping
   * into the raw-table toggle. Identical to `ts_display` when the
   * entry is older than the 7-day relative-time window. */
  ts_absolute: string;
  /** Single glyph rendered inside the timeline rail badge. The
   * glyph is the colour-blind-safe categorical signal per
   * ADR-0017 §"Adversarial review #4". */
  glyph: string;
  /** CSS class the renderer applies to the badge for per-kind
   * border / colour styling. One of:
   *   - `kind-issued` (📝)
   *   - `kind-submitted` (↗)
   *   - `kind-ack-saved` (✓), `kind-ack-processing` (⏳),
   *     `kind-ack-aborted` (⚠), `kind-ack-received` (⇣)
   *   - `kind-storno` (⊘)
   *   - `kind-modified` (✎)
   *   - `kind-paid` (💰)
   *   - `kind-email-sent` (✉), `kind-email-failed` (⚠) — S163
   *   - `kind-default` (•) — fallback for unmodelled EventKinds */
  kind_class: string;
  /** Secondary lines rendered under the heading. Today: `actor`
   * plus an optional `Base: <invoice_id>` for chain entries.
   * Plain strings; the renderer drops them into a `<ul>` and
   * Svelte's expression escaping is the HTML-safe guarantee. */
  body_lines: string[];
}

/** Internal — the (glyph, kind_class, label) tuple a switch arm
 * produces. Kept private so a future renderer cannot dispatch
 * directly on the EventKind and bypass the chronological-order
 * pin on the public mapper. */
interface KindMeta {
  glyph: string;
  kind_class: string;
  label: string;
}

/** Ack-status sub-table for `InvoiceAckStatus` entries. The typed
 * `AckStatus` union in `./api.ts` has four members; the brief
 * named three (SAVED / PROCESSING / ABORTED); RECEIVED reuses the
 * `⇣` from `labels.ts` ACK_LABELS so the renderer covers every
 * value the backend may emit without a fallback to the muted dot. */
const ACK_KIND_META: Record<string, { glyph: string; kind_class: string }> = {
  SAVED: { glyph: "✓", kind_class: "kind-ack-saved" },
  PROCESSING: { glyph: "⏳", kind_class: "kind-ack-processing" },
  ABORTED: { glyph: "⚠", kind_class: "kind-ack-aborted" },
  RECEIVED: { glyph: "⇣", kind_class: "kind-ack-received" },
};

/** Fallback for unmodelled EventKinds and unmodelled ack-status
 * literals. The `•` glyph + muted class is the visible "this is
 * a kind/status the SPA does not specifically model" signal per
 * CLAUDE.md rule 12 (fail loud, not silent). */
const DEFAULT_META: KindMeta = {
  glyph: "•",
  kind_class: "kind-default",
  label: "",
};

/** Read the `ack_status` string from a `InvoiceAckStatus` payload.
 * The wire shape is `{ invoice_id, transaction_id, ack_status,
 * response_xml }` per `audit_payloads::InvoiceAckStatusPayload`;
 * the SPA receives `payload: unknown` so we narrow defensively.
 * Returns the raw string (UPPERCASE per NAV v3.0) or `null` if
 * the field is missing / not a string. */
function readAckStatus(payload: unknown): string | null {
  if (typeof payload !== "object" || payload === null) return null;
  const ack = (payload as { ack_status?: unknown }).ack_status;
  return typeof ack === "string" ? ack : null;
}

/** S163 — read the `outcome` string off an `InvoiceEmailedSent`
 * payload. The wire shape is `{ invoice_id, recipient, subject,
 * outcome, error_class?, error_detail?, auto, attached_xml }` per
 * `audit_payloads::InvoiceEmailedSentPayload`; `outcome` is the
 * closed vocab `"succeeded" | "failed"`. Mirrors
 * `invoice-actions.ts::readEmailOutcome` (the codebase keeps a local
 * copy per module rather than cross-importing — same posture as
 * `readAckStatus`). Returns the raw string or `null` if missing /
 * not a string. */
function readEmailOutcome(payload: unknown): string | null {
  if (typeof payload !== "object" || payload === null) return null;
  const outcome = (payload as { outcome?: unknown }).outcome;
  return typeof outcome === "string" ? outcome : null;
}

/** S163 — read the operator-readable `error_detail` (already secret-
 * scrubbed backend-side per `EmailSendError::scrubbed_detail`) off a
 * FAILED `InvoiceEmailedSent` payload, prefixed by its closed-vocab
 * `error_class` when present. Returns `null` when neither field is a
 * string (e.g. a success entry), so the caller simply omits the
 * detail line. */
function readEmailFailureDetail(payload: unknown): string | null {
  if (typeof payload !== "object" || payload === null) return null;
  const p = payload as { error_class?: unknown; error_detail?: unknown };
  const cls = typeof p.error_class === "string" ? p.error_class : null;
  const detail = typeof p.error_detail === "string" ? p.error_detail : null;
  if (cls === null && detail === null) return null;
  return `${cls ?? "?"}: ${detail ?? "(no detail)"}`;
}

/** PR-76 — one entry from the `technical_validation_messages` array
 * the backend grafts onto an `InvoiceAckStatus` payload (see
 * `apps/aberp/src/serve.rs::audit_view_of`). Field names match the
 * NAV v3.0 OSA element names verbatim, snake-cased — same shape
 * `TechnicalValidationBody` already uses for the upstream-fault
 * wire surface (PR-59). All four fields are `Option<string>`
 * because NAV occasionally omits `tag` or `result_code` for terse
 * WARN-class entries. */
interface TechnicalValidationMessage {
  result_code: string | null;
  error_code: string | null;
  message: string | null;
  tag: string | null;
}

/** PR-76 — read the `technical_validation_messages` array off an
 * `InvoiceAckStatus` payload, narrowing defensively. The backend
 * always grafts a (possibly empty) array onto the payload; a missing
 * field, a non-array value, or a non-object element falls back to
 * an empty list so the timeline simply omits the messages section
 * rather than crashing. */
function readTechnicalValidationMessages(
  payload: unknown,
): TechnicalValidationMessage[] {
  if (typeof payload !== "object" || payload === null) return [];
  const raw = (payload as { technical_validation_messages?: unknown })
    .technical_validation_messages;
  if (!Array.isArray(raw)) return [];
  const out: TechnicalValidationMessage[] = [];
  for (const m of raw) {
    if (typeof m !== "object" || m === null) continue;
    const r = m as Record<string, unknown>;
    out.push({
      result_code: typeof r.result_code === "string" ? r.result_code : null,
      error_code: typeof r.error_code === "string" ? r.error_code : null,
      message: typeof r.message === "string" ? r.message : null,
      tag: typeof r.tag === "string" ? r.tag : null,
    });
  }
  return out;
}

/** PR-76 — format one validation message as a single-line operator-
 * facing body line. Shape: `"<ERROR|WARN> <ERROR_CODE>: <message>"`,
 * with the message bilingual when NAV provides both (NAV-test
 * currently returns English; HU output would round-trip the same
 * way). Missing fields render as `?` rather than being silently
 * dropped, so a wire-shape regression that omits one of them
 * surfaces visibly per CLAUDE.md rule 12. */
function formatValidationMessage(m: TechnicalValidationMessage): string {
  const result = m.result_code ?? "?";
  const code = m.error_code ?? "?";
  const body = m.message ?? "(no message)";
  return `${result} ${code}: ${body}`;
}

/** Map one audit entry to its `(glyph, kind_class, label)` tuple. */
function classify(entry: AuditEntryView): KindMeta {
  switch (entry.kind) {
    case "InvoiceDraftCreated":
      return { glyph: "📝", kind_class: "kind-issued", label: "Invoice issued" };
    case "InvoiceSubmissionAttempt":
      return {
        glyph: "↗",
        kind_class: "kind-submitted",
        label: "Submitted to NAV",
      };
    case "InvoiceAckStatus": {
      const ack = readAckStatus(entry.payload);
      const variant = ack !== null ? ACK_KIND_META[ack] : undefined;
      if (variant !== undefined && ack !== null) {
        return {
          glyph: variant.glyph,
          kind_class: variant.kind_class,
          label: `NAV ack: ${ack}`,
        };
      }
      // Unknown ack literal — surface the raw string so a backend
      // drift is operator-visible per CLAUDE.md rule 12.
      return {
        glyph: DEFAULT_META.glyph,
        kind_class: DEFAULT_META.kind_class,
        label: ack !== null ? `NAV ack: ${ack}` : "NAV ack",
      };
    }
    case "InvoiceStornoIssued":
      return { glyph: "⊘", kind_class: "kind-storno", label: "Storno issued" };
    case "InvoiceModificationIssued":
      return {
        glyph: "✎",
        kind_class: "kind-modified",
        label: "Modification issued",
      };
    case "InvoicePaymentRecorded":
      // PR-70 / ADR-0039 §2 — operational mark-as-paid event.
      // Distinct glyph from the regulatory-ladder kinds so the
      // operator sees at a glance that this entry is OFF the NAV
      // ladder (paid-vs-unpaid is parallel operational metadata
      // per ADR-0039 §3).
      return {
        glyph: "💰",
        kind_class: "kind-paid",
        label: "Payment recorded",
      };
    case "InvoiceEmailedSent": {
      // S163 — the email-send event carries a closed-vocab `outcome`
      // ("succeeded" | "failed") on its payload (PR-92/93). Pre-S163
      // this kind fell through to the muted `•` default and rendered
      // the raw "InvoiceEmailedSent" string IDENTICALLY for both
      // outcomes — so a FAILED send read as "sent" on the timeline
      // while the action-bar tooltip (PR-99 `emailButtonState`)
      // correctly said it failed. The audit row's `outcome` field was
      // truthful all along; only this display label lied. Fork on the
      // outcome so the narrative matches the data (CLAUDE.md rule 12).
      const outcome = readEmailOutcome(entry.payload);
      if (outcome === "succeeded") {
        return { glyph: "✉", kind_class: "kind-email-sent", label: "Email sent" };
      }
      if (outcome === "failed") {
        return {
          glyph: "⚠",
          kind_class: "kind-email-failed",
          label: "Email send failed",
        };
      }
      // Unknown / missing outcome — surface the raw kind on the muted
      // dot so a backend wire-shape drift stays operator-visible
      // rather than masquerading as a successful send.
      return {
        glyph: DEFAULT_META.glyph,
        kind_class: DEFAULT_META.kind_class,
        label: entry.kind,
      };
    }
    default:
      // Unmodelled EventKind — render the raw wire string so the
      // operator can still see WHICH entry it is. The muted `•`
      // glyph names the SPA-did-not-model state per rule 12.
      return {
        glyph: DEFAULT_META.glyph,
        kind_class: DEFAULT_META.kind_class,
        label: entry.kind,
      };
  }
}

/** Build the body lines for one entry — actor on every node plus
 * an optional `→ Base: <id>` for chain entries. The base id is
 * surfaced inline so the timeline preserves the chain context the
 * audit-row table renders as a clickable affordance; the
 * "Show raw table" toggle in `InvoiceDetail.svelte` keeps the
 * clickable navigation available for power users.
 *
 * PR-76 — for `InvoiceAckStatus` entries, every parsed
 * `technicalValidationMessages` from the NAV ack body appears as a
 * separate line below the actor. This is the "operator sees WHY
 * without digging into logs" surface for ABORTED acks; on
 * SAVED / PROCESSING / RECEIVED the backend's parsed list is empty
 * and no extra lines render. */
function bodyLines(entry: AuditEntryView): string[] {
  const lines = [`actor: ${entry.actor}`];
  if (entry.chain_base_invoice_id !== null) {
    lines.push(`→ Base: ${entry.chain_base_invoice_id}`);
  }
  if (entry.kind === "InvoiceAckStatus") {
    for (const m of readTechnicalValidationMessages(entry.payload)) {
      lines.push(formatValidationMessage(m));
    }
  }
  // S163 — for a FAILED email send, surface the scrubbed error class +
  // detail as a body line (same "operator sees WHY without digging into
  // logs" posture as the ack-status validation messages above). Success
  // entries return null here and add no extra line.
  if (entry.kind === "InvoiceEmailedSent") {
    const failure = readEmailFailureDetail(entry.payload);
    if (failure !== null) lines.push(failure);
  }
  return lines;
}

/** S195 — Intl formatters cached at module load. The relative
 * formatter uses `numeric: 'auto'` so 0/-1/+1 days render as
 * "ma" / "tegnap" / "holnap" rather than the numeric "0 napja"
 * / "1 napja" / "1 nap múlva" forms — closer to how a Hungarian
 * accountant reads the timeline aloud. The absolute formatter
 * pins `Europe/Budapest` so the operator-facing string is
 * deterministic regardless of the browser's host timezone
 * (operators work in HU local time; printed invoices are stamped
 * in HU local time; the timeline must agree). */
const HU_RELATIVE = new Intl.RelativeTimeFormat("hu", { numeric: "auto" });
const HU_DATE_TIME = new Intl.DateTimeFormat("hu-HU", {
  timeZone: "Europe/Budapest",
  year: "numeric",
  month: "2-digit",
  day: "2-digit",
  hour: "2-digit",
  minute: "2-digit",
  hour12: false,
});

/** S195 — split the absolute and relative-time formatting out of
 * the per-entry mapper so the per-kind classify() can stay focused
 * on glyph/class/label and a future change to the relative-time
 * thresholds is one-line. Exported for direct unit-test coverage.
 *
 * Cutover thresholds: within the last 45 seconds reads as "épp
 * most" (Intl has no "just-now" unit), then minute/hour/day picks
 * the coarsest unit whose magnitude is ≥ 1, then beyond ~7 days
 * the relative form stops being useful and the display falls
 * through to the absolute date+time. A malformed `occurred_at`
 * (rejected by `Date.parse`) round-trips the raw string unchanged
 * so a wire-shape regression is visible per CLAUDE.md rule 12. */
export function formatHungarianTimestamp(
  occurredAt: string,
  now: Date,
): { display: string; absolute: string } {
  const ts = new Date(occurredAt);
  if (Number.isNaN(ts.getTime())) {
    return { display: occurredAt, absolute: occurredAt };
  }
  const absolute = HU_DATE_TIME.format(ts);
  const diffMs = ts.getTime() - now.getTime();
  const diffSec = Math.round(diffMs / 1000);
  const absSec = Math.abs(diffSec);

  if (absSec < 45) {
    return { display: "épp most", absolute };
  }
  if (absSec < 60 * 60) {
    const mins = Math.round(diffSec / 60);
    return { display: HU_RELATIVE.format(mins, "minute"), absolute };
  }
  if (absSec < 60 * 60 * 24) {
    const hours = Math.round(diffSec / 3600);
    return { display: HU_RELATIVE.format(hours, "hour"), absolute };
  }
  if (absSec < 60 * 60 * 24 * 7) {
    const days = Math.round(diffSec / 86400);
    return { display: HU_RELATIVE.format(days, "day"), absolute };
  }
  // Beyond a week the relative form (`12 napja`, `3 hete`) reads
  // less crisply than the absolute date — fall through.
  return { display: absolute, absolute };
}

/** Map an ordered `AuditEntryView[]` to `TimelineNode[]`. Order is
 * preserved verbatim — the backend's `get_audit_for_invoice`
 * walker emits entries in append-only `seq` order, and the
 * timeline renderer reads top-to-bottom as chronological. Pinned
 * by `preserves chronological order` in
 * `invoice-timeline.test.ts`.
 *
 * S195 — the optional `now` second argument anchors the
 * relative-time computation. Production callers (`InvoiceDetail.svelte`)
 * pass `new Date()`; unit tests pass a fixed instant so the
 * `ts_display` strings are deterministic. */
export function timelineFromAuditEntries(
  entries: AuditEntryView[],
  now: Date = new Date(),
): TimelineNode[] {
  return entries.map((entry) => {
    const meta = classify(entry);
    const ts = formatHungarianTimestamp(entry.occurred_at, now);
    return {
      id: String(entry.seq),
      label_html_safe: meta.label,
      ts_iso: entry.occurred_at,
      ts_display: ts.display,
      ts_absolute: ts.absolute,
      glyph: meta.glyph,
      kind_class: meta.kind_class,
      body_lines: bodyLines(entry),
    };
  });
}
