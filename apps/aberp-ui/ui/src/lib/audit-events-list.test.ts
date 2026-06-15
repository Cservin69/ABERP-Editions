// S424 / session-424 — vitest pins for the Audit-events pure helpers.
// The load-bearing units: `parseSearch` (the search mini-syntax — the
// one real parser), `redactPayload` (Ervin #4 — redact-by-default +
// per-kind whitelist + show-raw), the sort/filter composition, the
// state-machine checklist mapping, and the operator "what did I do
// today" journey. CLAUDE.md rule 9 — every test pins INTENT (a constant-
// returning helper would fail these), not just shape.

import { describe, expect, it } from "vitest";

import {
  AUDIT_CHECKLISTS,
  EMPTY_AUDIT_FILTER,
  REDACTED,
  checklistsForKinds,
  compareEvents,
  domainOf,
  filterAuditEvents,
  isAuditFilterEmpty,
  kindLabel,
  parseSearch,
  prefixesForDomain,
  redactPayload,
  renderChecklist,
  sortAuditEvents,
  wouldRedact,
  type AuditDomain,
  type AuditEventRow,
} from "./audit-events-list";

function row(over: Partial<AuditEventRow> = {}): AuditEventRow {
  return {
    id: "aud_01HZ",
    seq: 1,
    kind: "invoice.payment_recorded",
    occurred_at: "2026-06-15T10:00:00Z",
    actor: "ervin",
    subject: "INV-1",
    summary: "Paid 100.00 EUR BankTransfer",
    hash_ok: true,
    has_payload: true,
    prev_hash_hex: "00",
    entry_hash_hex: "ab",
    ...over,
  };
}

const ids = (rows: AuditEventRow[]): number[] => rows.map((r) => r.seq);

// ── parseSearch ────────────────────────────────────────────────────────

describe("parseSearch", () => {
  it("empty input yields no facets", () => {
    expect(parseSearch("")).toEqual({ kinds: [] });
    expect(parseSearch("   ")).toEqual({ kinds: [] });
  });

  it("kind: token with a full as_str", () => {
    expect(parseSearch("kind:quote.operator_refused")).toEqual({
      kinds: ["quote.operator_refused"],
    });
  });

  it("kind: shorthand (bare suffix)", () => {
    expect(parseSearch("kind:operator_refused")).toEqual({
      kinds: ["operator_refused"],
    });
  });

  it("multiple kind: tokens accumulate", () => {
    const parsed = parseSearch("kind:storno_issued kind:modification_issued");
    expect(parsed.kinds).toEqual(["storno_issued", "modification_issued"]);
  });

  it("quote: / invoice: / subject: all set the subject facet", () => {
    expect(parseSearch("quote:8d83").subject).toBe("8d83");
    expect(parseSearch("invoice:INV-2026").subject).toBe("INV-2026");
    expect(parseSearch("subject:abc").subject).toBe("abc");
  });

  it("op: / operator: set the operator facet", () => {
    expect(parseSearch("op:ervin").operator).toBe("ervin");
    expect(parseSearch("operator:ervin").operator).toBe("ervin");
  });

  it("bare words collect into text", () => {
    expect(parseSearch("hello world").text).toBe("hello world");
  });

  it("mixed token salad parses every facet (AND across facets)", () => {
    const parsed = parseSearch("kind:payment_recorded op:ervin invoice:INV-7 paid");
    expect(parsed.kinds).toEqual(["payment_recorded"]);
    expect(parsed.operator).toBe("ervin");
    expect(parsed.subject).toBe("INV-7");
    expect(parsed.text).toBe("paid");
  });

  it("last subject: / op: wins on repeats", () => {
    expect(parseSearch("quote:a quote:b").subject).toBe("b");
    expect(parseSearch("op:a op:b").operator).toBe("b");
  });

  it("a bare colon-less word is text, not a facet", () => {
    expect(parseSearch("refused").text).toBe("refused");
    expect(parseSearch("refused").kinds).toEqual([]);
  });
});

// ── redactPayload (Ervin #4) ───────────────────────────────────────────

describe("redactPayload — default redaction", () => {
  it("redacts every sensitive-named field by substring", () => {
    const payload = {
      smtp_password: "hunter2",
      session_token: "tok_abc",
      api_credential: "c",
      client_secret: "s",
      binary_hash: "deadbeef",
      recipient: "a@b.hu",
    };
    const out = redactPayload(payload, "invoice.emailed_sent", false) as Record<string, unknown>;
    expect(out.smtp_password).toBe(REDACTED);
    expect(out.session_token).toBe(REDACTED);
    expect(out.api_credential).toBe(REDACTED);
    expect(out.client_secret).toBe(REDACTED);
    expect(out.binary_hash).toBe(REDACTED);
    // Non-sensitive fields pass through untouched.
    expect(out.recipient).toBe("a@b.hu");
  });

  it("redacts nested sensitive fields", () => {
    const payload = { meta: { nav_credential_hash: "x", ok: 1 } };
    const out = redactPayload(payload, "x", false) as { meta: Record<string, unknown> };
    expect(out.meta.nav_credential_hash).toBe(REDACTED);
    expect(out.meta.ok).toBe(1);
  });

  it("is case-insensitive on the field name", () => {
    const out = redactPayload({ SMTP_PASSWORD: "x" }, "k", false) as Record<string, unknown>;
    expect(out.SMTP_PASSWORD).toBe(REDACTED);
  });

  it("does not mutate the input", () => {
    const payload = { token: "secret" };
    redactPayload(payload, "k", false);
    expect(payload.token).toBe("secret");
  });
});

describe("redactPayload — per-kind whitelist override", () => {
  it("feature_graph_hash is shown for quote.pricing_posted (known-safe content hash)", () => {
    const payload = { feature_graph_hash: "abc123", binary_hash: "x" };
    const out = redactPayload(payload, "quote.pricing_posted", false) as Record<string, unknown>;
    // Whitelisted → visible.
    expect(out.feature_graph_hash).toBe("abc123");
    // Not whitelisted → still redacted.
    expect(out.binary_hash).toBe(REDACTED);
  });

  it("the same field IS redacted for a non-whitelisted kind", () => {
    const out = redactPayload({ feature_graph_hash: "abc" }, "other.kind", false) as Record<
      string,
      unknown
    >;
    expect(out.feature_graph_hash).toBe(REDACTED);
  });
});

describe("redactPayload — large-blob collapse + show-raw", () => {
  it("collapses a large byte array (NAV XML) to a placeholder", () => {
    const payload = { response_xml: Array.from({ length: 4096 }, () => 65), invoice_id: "INV-1" };
    const out = redactPayload(payload, "invoice.ack_status", false) as Record<string, unknown>;
    expect(typeof out.response_xml).toBe("string");
    expect(out.response_xml as string).toContain("4096");
    expect(out.invoice_id).toBe("INV-1");
  });

  it("collapses a very long string to a placeholder", () => {
    const payload = { request_xml: "x".repeat(5000) };
    const out = redactPayload(payload, "k", false) as Record<string, unknown>;
    expect(out.request_xml as string).toContain("5000");
  });

  it("show-raw returns the value unchanged (secrets + blobs)", () => {
    const payload = { token: "t", response_xml: Array.from({ length: 4096 }, () => 65) };
    const out = redactPayload(payload, "k", true);
    expect(out).toBe(payload); // identity — no clone, no masking
  });
});

describe("wouldRedact", () => {
  it("true when a sensitive field exists", () => {
    expect(wouldRedact({ password: "x" }, "k")).toBe(true);
  });
  it("true when a large blob exists", () => {
    expect(wouldRedact({ xml: Array.from({ length: 100 }, () => 1) }, "k")).toBe(true);
  });
  it("false for a clean small payload", () => {
    expect(wouldRedact({ invoice_id: "INV-1", amount: 100 }, "k")).toBe(false);
  });
  it("false when the only hash field is whitelisted", () => {
    expect(wouldRedact({ feature_graph_hash: "abc" }, "quote.pricing_posted")).toBe(false);
  });
});

// ── sort ───────────────────────────────────────────────────────────────

describe("sortAuditEvents", () => {
  const a = row({ seq: 1, occurred_at: "2026-06-15T08:00:00Z", kind: "quote.pricing_posted", actor: "anna" });
  const b = row({ seq: 2, occurred_at: "2026-06-15T12:00:00Z", kind: "invoice.draft_created", actor: "bela" });
  const c = row({ seq: 3, occurred_at: "2026-06-15T10:00:00Z", kind: "email.relay_sent", actor: "csaba" });

  it("seq desc is newest-first (the default view)", () => {
    expect(ids(sortAuditEvents([a, b, c], "seq", "desc"))).toEqual([3, 2, 1]);
  });
  it("seq asc is oldest-first", () => {
    expect(ids(sortAuditEvents([a, b, c], "seq", "asc"))).toEqual([1, 2, 3]);
  });
  it("occurred_at orders chronologically", () => {
    expect(ids(sortAuditEvents([a, b, c], "occurred_at", "asc"))).toEqual([1, 3, 2]);
  });
  it("kind sorts by the wire string", () => {
    // email < invoice < quote → seq 3, 2, 1
    expect(ids(sortAuditEvents([a, b, c], "kind", "asc"))).toEqual([3, 2, 1]);
  });
  it("actor sorts alphabetically", () => {
    expect(ids(sortAuditEvents([a, b, c], "actor", "asc"))).toEqual([1, 2, 3]);
  });
  it("ties break on seq ascending regardless of dir (deterministic)", () => {
    const x = row({ seq: 5, actor: "same" });
    const y = row({ seq: 9, actor: "same" });
    expect(ids(sortAuditEvents([y, x], "actor", "asc"))).toEqual([5, 9]);
    expect(ids(sortAuditEvents([y, x], "actor", "desc"))).toEqual([5, 9]);
  });
  it("does not mutate the input array", () => {
    const input = [b, a, c];
    sortAuditEvents(input, "seq", "asc");
    expect(ids(input)).toEqual([2, 1, 3]);
  });
});

describe("compareEvents direct", () => {
  it("returns 0-equivalent (seq tiebreak) only when same seq", () => {
    const same = row({ seq: 7 });
    expect(compareEvents(same, same, "kind", "asc")).toBe(0);
  });
});

// ── filter + domain ────────────────────────────────────────────────────

describe("domainOf + prefixesForDomain", () => {
  it("maps prefixes to buckets", () => {
    expect(domainOf("invoice.payment_recorded")).toBe("invoice");
    expect(domainOf("quote.operator_refused")).toBe("quote");
    expect(domainOf("email.relay_sent")).toBe("email");
    expect(domainOf("mes.work_order_created")).toBe("mes");
    expect(domainOf("inventory.material_reserved")).toBe("inventory");
    expect(domainOf("system.numbering_template_changed")).toBe("system");
    expect(domainOf("test")).toBe("system");
  });

  it("folds the seven defense prefixes into compliance", () => {
    for (const p of ["personnel", "material", "part", "export", "cui", "supplier", "incident"]) {
      expect(domainOf(`${p}.something`)).toBe("compliance");
    }
  });

  it("prefixesForDomain is the inverse of domainOf for every bucket", () => {
    const domains: AuditDomain[] = [
      "invoice",
      "quote",
      "email",
      "mes",
      "inventory",
      "compliance",
      "system",
    ];
    for (const d of domains) {
      for (const prefix of prefixesForDomain(d)) {
        expect(domainOf(`${prefix}.x`)).toBe(d);
      }
    }
  });
});

describe("filterAuditEvents", () => {
  const dump = [
    row({ seq: 1, kind: "invoice.payment_recorded", actor: "ervin", subject: "INV-1", summary: "Paid 100" }),
    row({ seq: 2, kind: "quote.operator_refused", actor: "ervin", subject: "qpj_7", summary: "Operator refused · no stock" }),
    row({ seq: 3, kind: "email.relay_sent", actor: "daemon", subject: null, summary: "" }),
    row({ seq: 4, kind: "quote.pricing_posted", actor: "daemon", subject: "qpj_8", summary: "Quote priced" }),
  ];

  it("domain:all + empty search passes everything", () => {
    expect(ids(filterAuditEvents(dump, EMPTY_AUDIT_FILTER))).toEqual([1, 2, 3, 4]);
  });

  it("domain facet narrows to one bucket", () => {
    expect(ids(filterAuditEvents(dump, { domain: "quote", search: "" }))).toEqual([2, 4]);
  });

  it("kind: token matches by substring (full or shorthand)", () => {
    expect(ids(filterAuditEvents(dump, { domain: "all", search: "kind:operator_refused" }))).toEqual([2]);
    expect(ids(filterAuditEvents(dump, { domain: "all", search: "kind:quote." }))).toEqual([2, 4]);
  });

  it("op: token matches the actor", () => {
    expect(ids(filterAuditEvents(dump, { domain: "all", search: "op:daemon" }))).toEqual([3, 4]);
  });

  it("subject token matches the subject", () => {
    expect(ids(filterAuditEvents(dump, { domain: "all", search: "quote:qpj_7" }))).toEqual([2]);
  });

  it("bare text searches kind + label + actor + summary + subject", () => {
    expect(ids(filterAuditEvents(dump, { domain: "all", search: "stock" }))).toEqual([2]);
  });

  it("facets AND together (domain + op + text)", () => {
    expect(
      ids(filterAuditEvents(dump, { domain: "quote", search: "op:daemon priced" })),
    ).toEqual([4]);
  });
});

describe("isAuditFilterEmpty", () => {
  it("true only for the empty filter", () => {
    expect(isAuditFilterEmpty(EMPTY_AUDIT_FILTER)).toBe(true);
    expect(isAuditFilterEmpty({ domain: "quote", search: "" })).toBe(false);
    expect(isAuditFilterEmpty({ domain: "all", search: "x" })).toBe(false);
  });
});

// ── kindLabel ──────────────────────────────────────────────────────────

describe("kindLabel", () => {
  it("returns the curated bilingual label for known kinds", () => {
    expect(kindLabel("invoice.payment_recorded")).toContain("Payment recorded");
  });
  it("falls back to a humanised suffix for unknown kinds", () => {
    // Title-cased suffix, underscores → spaces. Never a blank cell.
    expect(kindLabel("supplier.dpas_priority_set")).toBe("Dpas priority set");
  });
  it("handles a prefix-less kind", () => {
    expect(kindLabel("test")).toBe("Test");
  });
});

// ── state-machine checklists ───────────────────────────────────────────

describe("renderChecklist", () => {
  const invoiceDef = AUDIT_CHECKLISTS.find((d) => d.id === "invoice-issuance")!;

  it("marks reached steps ✓ and the rest pending when no failure", () => {
    const present = new Set([
      "invoice.sequence_reserved",
      "invoice.draft_created",
      "invoice.submission_attempt",
    ]);
    const rendered = renderChecklist(present, invoiceDef);
    expect(rendered.steps.slice(0, 3).map((s) => s.status)).toEqual([
      "reached",
      "reached",
      "reached",
    ]);
    expect(rendered.steps[3].status).toBe("pending");
    expect(rendered.steps.every((s) => s.status !== "failed")).toBe(true);
  });

  it("marks the first non-optional unreached step ✗ when a failure kind is present", () => {
    const present = new Set([
      "invoice.sequence_reserved",
      "invoice.draft_created",
      "invoice.submission_attempt",
      "invoice.submission_attempt_failed", // failure
    ]);
    const rendered = renderChecklist(present, invoiceDef);
    // reached: reserved, draft, attempt; the next non-optional step
    // (submission_response) is the failure point.
    expect(rendered.steps[3].label).toContain("Response");
    expect(rendered.steps[3].status).toBe("failed");
    // Only ONE step is marked failed.
    expect(rendered.steps.filter((s) => s.status === "failed")).toHaveLength(1);
  });

  it("a fully-walked happy path is all reached (optional steps included)", () => {
    const present = new Set([
      "invoice.sequence_reserved",
      "invoice.draft_created",
      "invoice.submission_attempt",
      "invoice.submission_response",
      "invoice.ack_status",
      "invoice.payment_recorded",
      "invoice.emailed_sent",
    ]);
    const rendered = renderChecklist(present, invoiceDef);
    expect(rendered.steps.every((s) => s.status === "reached")).toBe(true);
  });
});

describe("checklistsForKinds", () => {
  it("returns only checklists with ≥1 reached step (an invoice subject does not show the quote pipeline)", () => {
    const present = new Set(["invoice.draft_created", "invoice.payment_recorded"]);
    const cls = checklistsForKinds(present);
    const idsOut = cls.map((c) => c.id);
    expect(idsOut).toContain("invoice-issuance");
    expect(idsOut).not.toContain("quote-pipeline");
  });

  it("surfaces the quote pipeline for a quote subject", () => {
    const present = new Set(["quote.pricing_fetched", "quote.pricing_priced", "quote.pricing_failed"]);
    const cls = checklistsForKinds(present);
    expect(cls.map((c) => c.id)).toContain("quote-pipeline");
    const pipeline = cls.find((c) => c.id === "quote-pipeline")!;
    expect(pipeline.steps.some((s) => s.status === "failed")).toBe(true);
  });

  it("returns nothing for an unrelated kind set", () => {
    expect(checklistsForKinds(new Set(["personnel.access_granted"]))).toEqual([]);
  });
});

// ── operator journey ("what did I do today") ───────────────────────────

describe("operator journey — find what I did today", () => {
  it("filter to my operator + kind, sort newest, collapses a dump to my rows", () => {
    const dump = [
      row({ seq: 10, kind: "quote.operator_refused", actor: "ervin", occurred_at: "2026-06-15T09:00:00Z", summary: "Operator refused · no stock" }),
      row({ seq: 11, kind: "invoice.payment_recorded", actor: "daemon", occurred_at: "2026-06-15T09:30:00Z" }),
      row({ seq: 12, kind: "quote.operator_refused", actor: "ervin", occurred_at: "2026-06-15T11:00:00Z", summary: "Operator refused · capacity" }),
      row({ seq: 13, kind: "email.relay_sent", actor: "daemon", occurred_at: "2026-06-15T11:30:00Z" }),
    ];
    // "kind:operator_refused op:ervin" — my refusals.
    const visible = sortAuditEvents(
      filterAuditEvents(dump, { domain: "all", search: "kind:operator_refused op:ervin" }),
      "occurred_at",
      "desc",
    );
    expect(ids(visible)).toEqual([12, 10]); // newest first, only my refusals
  });
});

// ── End-to-end operator journey (helper composition) ───────────────────
//
// This codebase has no DOM / component-render harness — screens are
// covered by pure-helper journey tests (cf. `pricing-jobs-list.test.ts`).
// This walks the EXACT units `AuditEvents.svelte` + `AuditEventDetail`
// wire together for the task's scenario: open the screen, filter to
// `kind:QuoteOperatorRefused`, find the row, "expand" it (the detail's
// payload), see it REDACTED by default, "confirm show raw", see the full
// payload, then "close". The component glue (invoke / DOM) is thin; the
// load-bearing logic is these helpers.
describe("e2e operator journey — refused quote: filter → find → expand → reveal → close", () => {
  it("walks the full flow through the real helpers", () => {
    // A mixed page as the screen would load it.
    const page = [
      row({ seq: 40, kind: "invoice.payment_recorded", actor: "daemon", subject: "INV-9" }),
      row({
        seq: 41,
        kind: "quote.operator_refused",
        actor: "ervin",
        subject: "qpj_42",
        summary: "Operator refused · no stock",
      }),
      row({ seq: 42, kind: "email.relay_sent", actor: "daemon", subject: null }),
    ];

    // 1. Operator types `kind:QuoteOperatorRefused` shorthand. The search
    //    is case-insensitive substring over the wire kind.
    const visible = filterAuditEvents(page, {
      domain: "all",
      search: "kind:operator_refused",
    });
    expect(visible).toHaveLength(1);
    const found = visible[0];
    expect(found.seq).toBe(41);
    expect(kindLabel(found.kind)).toContain("refused");

    // 2. Expand → the detail fetches the full entry (here, the payload a
    //    QuoteOperatorRefused entry carries, plus a sensitive field the
    //    redaction must hide by default).
    const fullPayload = {
      quote_id: "qpj_42",
      reason: "no stock",
      customer_email_present: true,
      session_token: "tok_supersecret",
    };

    // 3. Default render → sensitive field redacted.
    const redacted = redactPayload(fullPayload, found.kind, false) as Record<string, unknown>;
    expect(redacted.reason).toBe("no stock");
    expect(redacted.session_token).toBe(REDACTED);
    expect(wouldRedact(fullPayload, found.kind)).toBe(true); // → show-raw affordance is offered

    // 4. Operator clicks "show raw" → confirms → full payload revealed.
    const raw = redactPayload(fullPayload, found.kind, true) as Record<string, unknown>;
    expect(raw.session_token).toBe("tok_supersecret");
    expect(raw).toEqual(fullPayload);

    // 5. The subject's checklist reconstructs the quote pipeline state.
    const checklists = checklistsForKinds(new Set([found.kind]));
    expect(checklists.map((c) => c.id)).toContain("quote-pipeline");

    // 6. Close → (component sets selectedSeq = null; nothing to assert at
    //    the helper level beyond the flow above completing cleanly).
  });
});
