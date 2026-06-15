// S424 / session-424 — pure helpers for the cross-domain Audit-events
// screen (`AuditEvents.svelte`). Ports the S411 `pricing-jobs-list.ts`
// shape (sort + filter + a closed-vocab facet) and adds the three pieces
// the audit screen needs on top:
//
//   1. `parseSearch(raw)` — the search box's `kind:` / `quote:` / `op:`
//      mini-syntax. This is the highest-value unit (the one piece of
//      real parsing logic), pinned hardest in the test.
//   2. The state-machine CHECKLISTS — five per-domain ordered step lists
//      keyed by `EventKind::as_str()`; `checklistsForKinds` reconstructs
//      "what happened to subject X" as ✓ reached / ⏳ pending / ✗ failed
//      from the set of kinds present for that subject (design §5).
//   3. `redactPayload(value, kind, showRaw)` — client-side redaction.
//      Ervin 2026-06-15: REDACT BY DEFAULT for sensitive fields
//      (*credential* / *password* / *token* / *secret* / *hash*) + the
//      large NAV-XML blobs; the operator "show raw" toggle reveals them
//      but requires a per-use confirmation in the component
//      ([[trust-code-not-operator]] — defaults are code-driven).
//
// All filtering + sorting here is CLIENT-side over the already-fetched
// page (live search box); the chips + date + operator + subject facets
// also go to the server as query params (the index-free scan is there
// anyway). Pure, no Svelte deps; pinned by `audit-events-list.test.ts`.

import { applySortDir, localeCompareHu, type SortDir } from "./list-sort";

export type { SortDir };

// ── Wire row shape (mirrors `serve::AuditEventRow`) ────────────────────

/** One audit-events list row. `kind` is the `EventKind::as_str()` wire
 * string (domain-prefixed, e.g. `invoice.payment_recorded`) — the chips
 * + `domainOf` key on the prefix. `payload` is NOT here: the list omits
 * it (NAV XML blobs are MBs) and the row is expanded via a separate
 * `getAuditEvent(seq)` fetch. */
export interface AuditEventRow {
  id: string;
  seq: number;
  kind: string;
  occurred_at: string;
  actor: string;
  subject: string | null;
  summary: string;
  hash_ok: boolean;
  has_payload: boolean;
  prev_hash_hex: string;
  entry_hash_hex: string;
}

// ── Domains + filter chips ─────────────────────────────────────────────

/** Closed-vocab domain bucket. Maps a kind's `as_str` prefix to the chip
 * it lives under. The compliance bucket folds the seven defense prefixes
 * (personnel / material / part / export / cui / supplier / incident)
 * into one chip — they are rare and share an operator mental model. */
export type AuditDomain =
  | "invoice"
  | "quote"
  | "system"
  | "email"
  | "mes"
  | "inventory"
  | "compliance";

/** Resolve a kind's domain from its `as_str` prefix. Unknown / `test` /
 * `system.*` fold into `"system"` (the catch-all operational bucket). */
export function domainOf(kind: string): AuditDomain {
  const prefix = kind.includes(".") ? kind.slice(0, kind.indexOf(".")) : kind;
  switch (prefix) {
    case "invoice":
      return "invoice";
    case "quote":
      return "quote";
    case "email":
      return "email";
    case "mes":
      return "mes";
    case "inventory":
      return "inventory";
    case "personnel":
    case "material":
    case "part":
    case "export":
    case "cui":
    case "supplier":
    case "incident":
      return "compliance";
    case "system":
    default:
      return "system";
  }
}

/** The `as_str` prefixes that make up a domain bucket — the chip's
 * server-side `domains` filter (inverse of [`domainOf`]). The compliance
 * chip expands to its seven defense prefixes; the system chip folds in
 * the `test` kind. */
export function prefixesForDomain(domain: AuditDomain): string[] {
  switch (domain) {
    case "invoice":
      return ["invoice"];
    case "quote":
      return ["quote"];
    case "email":
      return ["email"];
    case "mes":
      return ["mes"];
    case "inventory":
      return ["inventory"];
    case "compliance":
      return ["personnel", "material", "part", "export", "cui", "supplier", "incident"];
    case "system":
      return ["system", "test"];
  }
}

/** A filter chip. `domain === "all"` short-circuits the gate. The chip
 * label is bilingual (HU / EN) per the codebase convention. */
export interface AuditChip {
  domain: AuditDomain | "all";
  label: string;
}

/** The chip row across the top of the screen (design §4.5). */
export const AUDIT_CHIPS: AuditChip[] = [
  { domain: "all", label: "Mind / All" },
  { domain: "invoice", label: "Számla / Invoice" },
  { domain: "quote", label: "Ajánlat / Quote" },
  { domain: "system", label: "Rendszer / System" },
  { domain: "email", label: "E-mail / Email" },
  { domain: "mes", label: "Gyártás / MES" },
  { domain: "inventory", label: "Készlet / Inventory" },
  { domain: "compliance", label: "Megfelelés / Compliance" },
];

// ── Search box mini-syntax ─────────────────────────────────────────────

/** Parsed form of the search box. Special tokens pull out facets; bare
 * words collect into `text`. AND semantics ACROSS facets (kind AND
 * subject AND operator AND text); OR semantics WITHIN `kinds` (a row
 * passes if it matches ANY parsed kind token). */
export interface ParsedSearch {
  kinds: string[];
  subject?: string;
  operator?: string;
  text?: string;
}

/** Parse the search box: `kind:quote.operator_refused` (or shorthand
 * `kind:operator_refused`) → kind facet; `quote:8d83` / `invoice:INV-`
 * → subject substring; `op:ervin` → operator substring; bare words →
 * free-text needle. Tokens are whitespace-separated. The last `subject:`
 * / `op:` wins; `kind:` accumulates; everything else joins into `text`. */
export function parseSearch(raw: string): ParsedSearch {
  const out: ParsedSearch = { kinds: [] };
  const textWords: string[] = [];
  for (const token of raw.trim().split(/\s+/)) {
    if (token.length === 0) continue;
    const colon = token.indexOf(":");
    const prefix = colon > 0 ? token.slice(0, colon).toLowerCase() : "";
    const value = colon > 0 ? token.slice(colon + 1) : "";
    if (prefix === "kind" && value.length > 0) {
      out.kinds.push(value);
    } else if ((prefix === "quote" || prefix === "invoice" || prefix === "subject") && value.length > 0) {
      out.subject = value;
    } else if ((prefix === "op" || prefix === "operator") && value.length > 0) {
      out.operator = value;
    } else {
      textWords.push(token);
    }
  }
  if (textWords.length > 0) out.text = textWords.join(" ");
  return out;
}

// ── Filter ─────────────────────────────────────────────────────────────

/** The client-side filter spec. `domain === "all"` and an empty `search`
 * are both open gates. The "Clear" button on the empty-state resets to
 * [`EMPTY_AUDIT_FILTER`]. */
export interface AuditFilterSpec {
  domain: AuditDomain | "all";
  search: string;
}

export const EMPTY_AUDIT_FILTER: AuditFilterSpec = {
  domain: "all",
  search: "",
};

/** `true` iff no chip is engaged and no search needle is set. The
 * renderer surfaces the empty-state "Clear" button only when this is
 * `false` (CLAUDE.md rule 12 — no no-op affordance). */
export function isAuditFilterEmpty(spec: AuditFilterSpec): boolean {
  return spec.domain === "all" && spec.search.trim().length === 0;
}

function domainPasses(row: AuditEventRow, domain: AuditDomain | "all"): boolean {
  return domain === "all" || domainOf(row.kind) === domain;
}

/** Does the row match the parsed search? Facets AND together; the `kinds`
 * facet matches if the row's kind CONTAINS any parsed token (case-
 * insensitive substring — handles both the full `as_str` and the bare
 * suffix shorthand). */
function searchPasses(row: AuditEventRow, parsed: ParsedSearch): boolean {
  if (parsed.kinds.length > 0) {
    const k = row.kind.toLowerCase();
    if (!parsed.kinds.some((tok) => k.includes(tok.toLowerCase()))) return false;
  }
  if (parsed.subject) {
    const subj = (row.subject ?? "").toLowerCase();
    if (!subj.includes(parsed.subject.toLowerCase())) return false;
  }
  if (parsed.operator) {
    if (!row.actor.toLowerCase().includes(parsed.operator.toLowerCase())) return false;
  }
  if (parsed.text) {
    const needle = parsed.text.toLowerCase();
    const haystack = [
      row.kind,
      kindLabel(row.kind),
      row.actor,
      row.summary,
      row.subject ?? "",
      row.id,
    ]
      .join(" ")
      .toLowerCase();
    if (!haystack.includes(needle)) return false;
  }
  return true;
}

/** Domain-facet + search filter. Returns a NEW array. */
export function filterAuditEvents<R extends AuditEventRow>(
  rows: R[],
  spec: AuditFilterSpec,
): R[] {
  const parsed = parseSearch(spec.search);
  return rows.filter((row) => domainPasses(row, spec.domain) && searchPasses(row, parsed));
}

// ── Sort ───────────────────────────────────────────────────────────────

/** Sortable columns. `seq` is the stable cursor + default; `occurred_at`
 * mirrors it in practice (append-time order); `kind` / `actor` group the
 * log by domain / operator. */
export type AuditSortKey = "seq" | "occurred_at" | "kind" | "actor";

/** Per-column comparator. Ties fall to `seq` ascending (unique →
 * deterministic render order across refreshes, CLAUDE.md rule 12). */
export function compareEvents(
  a: AuditEventRow,
  b: AuditEventRow,
  key: AuditSortKey,
  dir: SortDir,
): number {
  let cmp: number;
  switch (key) {
    case "seq":
      cmp = a.seq - b.seq;
      break;
    case "occurred_at":
      // RFC3339 from the backend is fixed-offset (Z) → lex compare ==
      // chrono compare. No `new Date()` (would re-interpret per tz).
      cmp =
        a.occurred_at < b.occurred_at ? -1 : a.occurred_at > b.occurred_at ? 1 : 0;
      break;
    case "kind":
      cmp = localeCompareHu(a.kind, b.kind);
      break;
    case "actor":
      cmp = localeCompareHu(a.actor, b.actor);
      break;
  }
  if (cmp !== 0) return applySortDir(cmp, dir);
  return a.seq - b.seq; // dir-invariant tiebreak
}

/** Sort by `key` + `dir`. Returns a NEW array (does not mutate input). */
export function sortAuditEvents<R extends AuditEventRow>(
  rows: R[],
  key: AuditSortKey,
  dir: SortDir,
): R[] {
  return rows.slice().sort((a, b) => compareEvents(a, b, key, dir));
}

// ── Kind → label vocabulary ────────────────────────────────────────────

/** Bilingual labels for the high-traffic kinds. Keyed on the `as_str`
 * wire string. Everything not listed falls through to [`humanizeKind`]
 * (the raw suffix, title-cased) so a NEW backend kind is still readable
 * — fail-soft to the kind name, never a blank cell (CLAUDE.md rule 12). */
const KIND_LABELS: Record<string, string> = {
  "invoice.sequence_reserved": "Sorszám foglalva / Sequence reserved",
  "invoice.draft_created": "Számla kiállítva / Invoice issued",
  "invoice.submission_attempt": "NAV beküldés / Submitted to NAV",
  "invoice.submission_response": "NAV válasz / NAV response",
  "invoice.ack_status": "NAV nyugta / NAV ack",
  "invoice.retry_requested": "Újraküldés kérve / Retry requested",
  "invoice.marked_abandoned": "Feladva / Marked abandoned",
  "invoice.storno_issued": "Sztornó / Storno issued",
  "invoice.modification_issued": "Módosítás / Modification issued",
  "invoice.check_performed": "NAV ellenőrzés / NAV check",
  "invoice.payment_recorded": "Fizetés rögzítve / Payment recorded",
  "invoice.emailed_sent": "E-mail elküldve / Email sent",
  "quote.pricing_fetched": "Ajánlat beérkezett / Quote fetched",
  "quote.pricing_extracted": "CAD elemzés / CAD extracted",
  "quote.pricing_priced": "Árazva / Priced",
  "quote.pricing_rendered": "PDF kész / PDF rendered",
  "quote.pricing_posted": "Ajánlat kész / Quote posted",
  "quote.pricing_failed": "Árazás sikertelen / Pricing failed",
  "quote.operator_accepted": "Operátor elfogadta / Operator accepted",
  "quote.operator_refused": "Operátor elutasította / Operator refused",
  "quote.deal_issued": "DEAL kiállítva / DEAL issued",
  "system.numbering_template_changed": "Számozás módosítva / Numbering changed",
  "system.quote_intake_poll_attempted": "Storefront lekérdezés / Storefront poll",
  "email.relay_queued": "Levél sorba állítva / Email queued",
  "email.relay_sent": "Levél elküldve / Email relayed",
  "email.relay_failed": "Levél sikertelen / Email failed",
};

/** Title-case the kind's suffix for the fallback label. */
function humanizeKind(kind: string): string {
  const suffix = kind.includes(".") ? kind.slice(kind.indexOf(".") + 1) : kind;
  const words = suffix.replace(/_/g, " ").trim();
  if (words.length === 0) return kind;
  return words.charAt(0).toUpperCase() + words.slice(1);
}

/** Operator-facing bilingual label for a kind's `as_str`. */
export function kindLabel(kind: string): string {
  return KIND_LABELS[kind] ?? humanizeKind(kind);
}

// ── State-machine checklists (design §5) ───────────────────────────────

/** One step in a domain's canonical path. `kinds` are the `as_str`
 * values that satisfy it (ANY present → reached); an `optional` step is
 * never the failure point. */
export interface ChecklistStep {
  label: string;
  kinds: string[];
  optional?: boolean;
}

/** A per-domain canonical path. `failureKinds` mark the chain as stalled
 * — when one is present, the first not-yet-reached step renders ✗. */
export interface ChecklistDef {
  id: string;
  label: string;
  steps: ChecklistStep[];
  failureKinds: string[];
}

export type StepStatus = "reached" | "pending" | "failed";

export interface RenderedStep {
  label: string;
  status: StepStatus;
}

export interface RenderedChecklist {
  id: string;
  label: string;
  steps: RenderedStep[];
}

/** The five worked domains from design §5. Pure data so the renderer
 * stays presentational and the mapping is unit-tested. */
export const AUDIT_CHECKLISTS: ChecklistDef[] = [
  {
    id: "invoice-issuance",
    label: "Számla → NAV → fizetve / Invoice → NAV → paid",
    steps: [
      { label: "Sorszám foglalva / Sequence reserved", kinds: ["invoice.sequence_reserved"] },
      { label: "Számla kiállítva / Draft created", kinds: ["invoice.draft_created"] },
      { label: "NAV beküldés / Submitted", kinds: ["invoice.submission_attempt"] },
      { label: "NAV válasz / Response", kinds: ["invoice.submission_response"] },
      { label: "NAV nyugta / Acknowledged", kinds: ["invoice.ack_status"] },
      { label: "Fizetve / Paid", kinds: ["invoice.payment_recorded"], optional: true },
      { label: "E-mail elküldve / Emailed", kinds: ["invoice.emailed_sent"], optional: true },
    ],
    failureKinds: ["invoice.submission_attempt_failed", "invoice.marked_abandoned"],
  },
  {
    id: "storno-modification",
    label: "Sztornó / módosítás / Storno / modification",
    steps: [
      {
        label: "Sztornó / módosítás kiállítva / Storno / mod issued",
        kinds: ["invoice.storno_issued", "invoice.modification_issued"],
      },
      { label: "NAV beküldés / Submitted", kinds: ["invoice.submission_attempt"] },
      { label: "NAV nyugta / Acknowledged", kinds: ["invoice.ack_status"] },
    ],
    failureKinds: ["invoice.submission_attempt_failed", "invoice.marked_abandoned"],
  },
  {
    id: "annulment",
    label: "Technikai érvénytelenítés / Technical annulment",
    steps: [
      {
        label: "Kérve / Requested",
        kinds: ["invoice.technical_annulment_requested"],
      },
      {
        label: "Beküldés / Submission",
        kinds: ["invoice.annulment_submission_attempt"],
      },
      {
        label: "Válasz / Response",
        kinds: ["invoice.annulment_submission_response"],
      },
      { label: "Nyugta / Ack", kinds: ["invoice.annulment_ack_status"] },
      {
        label: "Befogadó visszaigazolás / Receiver confirmation",
        kinds: ["invoice.annulment_receiver_confirmation"],
        optional: true,
      },
    ],
    failureKinds: [],
  },
  {
    id: "quote-pipeline",
    label: "Auto-árazás / Auto-quote pipeline",
    steps: [
      { label: "Beérkezett / Fetched", kinds: ["quote.pricing_fetched"] },
      { label: "CAD elemzés / Extracted", kinds: ["quote.pricing_extracted"] },
      { label: "Árazva / Priced", kinds: ["quote.pricing_priced"] },
      { label: "PDF / Rendered", kinds: ["quote.pricing_rendered"] },
      { label: "Kész / Posted", kinds: ["quote.pricing_posted"] },
      {
        label: "Operátor döntés / Operator decision",
        kinds: ["quote.operator_accepted", "quote.operator_refused"],
        optional: true,
      },
    ],
    failureKinds: ["quote.pricing_failed", "quote.pricing_daemon_panicked"],
  },
  {
    id: "quote-to-production",
    label: "Ajánlat → DEAL → gyártás / Quote → DEAL → production",
    steps: [
      { label: "DEAL kiállítva / DEAL issued", kinds: ["quote.deal_issued"] },
      { label: "Vevői rendelés / Sales order", kinds: ["quote.sales_order_created"] },
      { label: "Munkalap / Work order", kinds: ["quote.work_order_created", "mes.work_order_created"] },
      { label: "QA / Inspection", kinds: ["mes.qa_inspection_created"], optional: true },
      { label: "QA döntés / QA decided", kinds: ["mes.qa_inspection_decided"], optional: true },
      { label: "Kiszállítás / Dispatch", kinds: ["mes.dispatch_created"], optional: true },
      { label: "Elküldve / Shipped", kinds: ["mes.dispatch_shipped"], optional: true },
    ],
    failureKinds: [],
  },
];

/** Render one checklist against the set of kinds present for a subject.
 * A step is `reached` if any of its kinds is present; if a failure kind
 * is present, the first non-optional unreached step renders `failed`;
 * the rest are `pending`. */
export function renderChecklist(
  presentKinds: Set<string>,
  def: ChecklistDef,
): RenderedChecklist {
  const hasFailure = def.failureKinds.some((k) => presentKinds.has(k));
  let failureAssigned = false;
  const steps: RenderedStep[] = def.steps.map((step) => {
    const reached = step.kinds.some((k) => presentKinds.has(k));
    if (reached) return { label: step.label, status: "reached" as StepStatus };
    if (hasFailure && !failureAssigned && step.optional !== true) {
      failureAssigned = true;
      return { label: step.label, status: "failed" as StepStatus };
    }
    return { label: step.label, status: "pending" as StepStatus };
  });
  return { id: def.id, label: def.label, steps };
}

/** Pick + render the checklists relevant to a subject from the set of
 * kinds observed for it. A checklist is relevant when ≥1 of its steps is
 * reached (so an invoice subject does not show the quote pipeline). */
export function checklistsForKinds(presentKinds: Set<string>): RenderedChecklist[] {
  return AUDIT_CHECKLISTS.map((def) => renderChecklist(presentKinds, def)).filter((c) =>
    c.steps.some((s) => s.status === "reached"),
  );
}

// ── Payload redaction (Ervin 2026-06-15 decision #4) ───────────────────

/** Substrings that mark a field name as sensitive. A field whose name
 * (case-insensitive) contains ANY of these is redacted by default. */
export const SENSITIVE_SUBSTRINGS = ["credential", "password", "token", "secret", "hash"];

/** Per-kind whitelist of field names that MATCH a sensitive substring
 * but are known-safe (content-addressing hashes — not secrets). The
 * override mechanism [[trust-code-not-operator]] keeps the default
 * deny-by-substring while letting a verified safe field through. */
export const REDACTION_WHITELIST: Record<string, string[]> = {
  "quote.pricing_posted": ["feature_graph_hash"],
  "quote.pricing_fetched": ["feature_graph_hash"],
};

/** The placeholder a redacted field renders as. */
export const REDACTED = "‹redacted›";

const LARGE_STRING = 200;
const LARGE_ARRAY = 64;

function isSensitiveKey(key: string): boolean {
  const lower = key.toLowerCase();
  return SENSITIVE_SUBSTRINGS.some((s) => lower.includes(s));
}

function redactValue(value: unknown, whitelist: Set<string>): unknown {
  if (Array.isArray(value)) {
    // Large arrays are the serialised NAV XML byte vectors (request_xml /
    // response_xml) — tens of KB. Collapse behind the show-raw toggle.
    if (value.length > LARGE_ARRAY) {
      return `‹${value.length} elem — “raw” a megtekintéshez / items — show raw to view›`;
    }
    return value.map((v) => redactValue(v, whitelist));
  }
  if (value !== null && typeof value === "object") {
    return redactObject(value as Record<string, unknown>, whitelist);
  }
  if (typeof value === "string" && value.length > LARGE_STRING) {
    return `‹${value.length} karakter — “raw” a megtekintéshez / chars — show raw to view›`;
  }
  return value;
}

function redactObject(
  obj: Record<string, unknown>,
  whitelist: Set<string>,
): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(obj)) {
    if (isSensitiveKey(k) && !whitelist.has(k)) {
      out[k] = REDACTED;
    } else {
      out[k] = redactValue(v, whitelist);
    }
  }
  return out;
}

/** Redact a payload for display. `showRaw === true` returns the value
 * unchanged (the component gates this behind a per-use confirmation —
 * Ervin decision #4). Otherwise every sensitive-named field (not in the
 * per-kind whitelist) becomes [`REDACTED`] and every large blob collapses
 * to a placeholder. Returns a NEW value; never mutates the input. */
export function redactPayload(value: unknown, kind: string, showRaw: boolean): unknown {
  if (showRaw) return value;
  const whitelist = new Set(REDACTION_WHITELIST[kind] ?? []);
  return redactValue(value, whitelist);
}

/** `true` iff redaction would actually hide something — drives whether
 * the component shows the sensitivity warning + the show-raw toggle
 * (CLAUDE.md rule 12 — no no-op affordance on a payload with nothing to
 * hide). */
export function wouldRedact(value: unknown, kind: string): boolean {
  return JSON.stringify(redactPayload(value, kind, false)) !== JSON.stringify(value);
}
