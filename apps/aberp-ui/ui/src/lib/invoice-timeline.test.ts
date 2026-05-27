// PR-67 / session-89 — pin tests for the SPA's audit-trail
// timeline mapper. One pin per modeled EventKind in the closed
// vocab plus the four `InvoiceAckStatus` payload forks, the
// `kind-default` fallback for an unmodelled kind, and a
// chronological-order preservation pin so a future refactor
// cannot silently sort the entries.
//
// CLAUDE.md rule 9: per-kind expectations on glyph + class +
// label mean a regression that collapses every entry to one
// kind (or returns a constant array) cannot pass every
// assertion vacuously.

import { describe, expect, it } from "vitest";

import type { AuditEntryView } from "./api";
import {
  timelineFromAuditEntries,
  type TimelineNode,
} from "./invoice-timeline";

/** Helper — build a minimal `AuditEntryView` for a single test. */
function entry(
  seq: number,
  kind: string,
  overrides: Partial<AuditEntryView> = {},
): AuditEntryView {
  return {
    seq,
    kind,
    actor: "cli",
    occurred_at: `2026-05-26T12:00:${String(seq).padStart(2, "0")}Z`,
    chain_base_invoice_id: null,
    payload: null,
    ...overrides,
  };
}

describe("timelineFromAuditEntries — per-kind glyph + class + label", () => {
  it("maps InvoiceDraftCreated to the 📝 issued node", () => {
    const [node] = timelineFromAuditEntries([entry(1, "InvoiceDraftCreated")]);
    expect(node.glyph).toBe("📝");
    expect(node.kind_class).toBe("kind-issued");
    expect(node.label_html_safe).toBe("Invoice issued");
  });

  it("maps InvoiceSubmissionAttempt to the ↗ submitted node", () => {
    const [node] = timelineFromAuditEntries([
      entry(2, "InvoiceSubmissionAttempt"),
    ]);
    expect(node.glyph).toBe("↗");
    expect(node.kind_class).toBe("kind-submitted");
    expect(node.label_html_safe).toBe("Submitted to NAV");
  });

  it("maps InvoiceStornoIssued to the ⊘ storno node", () => {
    const [node] = timelineFromAuditEntries([entry(3, "InvoiceStornoIssued")]);
    expect(node.glyph).toBe("⊘");
    expect(node.kind_class).toBe("kind-storno");
    expect(node.label_html_safe).toBe("Storno issued");
  });

  it("maps InvoiceModificationIssued to the ✎ modified node", () => {
    const [node] = timelineFromAuditEntries([
      entry(4, "InvoiceModificationIssued"),
    ]);
    expect(node.glyph).toBe("✎");
    expect(node.kind_class).toBe("kind-modified");
    expect(node.label_html_safe).toBe("Modification issued");
  });

  // PR-70 / ADR-0039 §2 — operational mark-as-paid event. Distinct
  // glyph from every regulatory-ladder kind so the operator sees at
  // a glance that this entry is OFF the NAV ladder (paid-vs-unpaid
  // is parallel operational metadata per ADR-0039 §3).
  it("maps InvoicePaymentRecorded to the 💰 paid node", () => {
    const [node] = timelineFromAuditEntries([
      entry(5, "InvoicePaymentRecorded"),
    ]);
    expect(node.glyph).toBe("💰");
    expect(node.kind_class).toBe("kind-paid");
    expect(node.label_html_safe).toBe("Payment recorded");
  });
});

describe("timelineFromAuditEntries — InvoiceAckStatus forks per ack_status", () => {
  it("SAVED → ✓ kind-ack-saved", () => {
    const [node] = timelineFromAuditEntries([
      entry(1, "InvoiceAckStatus", {
        payload: { ack_status: "SAVED", transaction_id: "tx-1" },
      }),
    ]);
    expect(node.glyph).toBe("✓");
    expect(node.kind_class).toBe("kind-ack-saved");
    expect(node.label_html_safe).toBe("NAV ack: SAVED");
  });

  it("PROCESSING → ⏳ kind-ack-processing", () => {
    const [node] = timelineFromAuditEntries([
      entry(1, "InvoiceAckStatus", {
        payload: { ack_status: "PROCESSING" },
      }),
    ]);
    expect(node.glyph).toBe("⏳");
    expect(node.kind_class).toBe("kind-ack-processing");
    expect(node.label_html_safe).toBe("NAV ack: PROCESSING");
  });

  it("ABORTED → ⚠ kind-ack-aborted", () => {
    const [node] = timelineFromAuditEntries([
      entry(1, "InvoiceAckStatus", {
        payload: { ack_status: "ABORTED" },
      }),
    ]);
    expect(node.glyph).toBe("⚠");
    expect(node.kind_class).toBe("kind-ack-aborted");
    expect(node.label_html_safe).toBe("NAV ack: ABORTED");
  });

  it("RECEIVED → ⇣ kind-ack-received (matches labels.ts ACK_LABELS)", () => {
    // Brief explicitly named SAVED / PROCESSING / ABORTED; RECEIVED
    // reuses the `⇣` glyph from labels.ts ACK_LABELS so the four-way
    // typed fork over AckStatus is exhaustive. A future drop of this
    // arm would silently bucket RECEIVED with the muted-dot
    // fallback and lose the ack categorisation.
    const [node] = timelineFromAuditEntries([
      entry(1, "InvoiceAckStatus", {
        payload: { ack_status: "RECEIVED" },
      }),
    ]);
    expect(node.glyph).toBe("⇣");
    expect(node.kind_class).toBe("kind-ack-received");
    expect(node.label_html_safe).toBe("NAV ack: RECEIVED");
  });

  it("unmodelled ack literal → kind-default with the raw status named loud", () => {
    // Backend drift: if NAV invents a fifth processingResult value
    // (or the persisted payload carries a string the SPA does not
    // model), the operator must still see the literal — CLAUDE.md
    // rule 12 (fail loud) rules out silently bucketing it.
    const [node] = timelineFromAuditEntries([
      entry(1, "InvoiceAckStatus", {
        payload: { ack_status: "UNKNOWN_FUTURE_VALUE" },
      }),
    ]);
    expect(node.glyph).toBe("•");
    expect(node.kind_class).toBe("kind-default");
    expect(node.label_html_safe).toBe("NAV ack: UNKNOWN_FUTURE_VALUE");
  });

  it("malformed ack payload (missing field) → kind-default with bare 'NAV ack' label", () => {
    // Defence in depth — a payload shape that omits ack_status (e.g.
    // pre-PR-7-C audit row, or direct DB tampering) renders as the
    // bare "NAV ack" label rather than crashing. The muted-dot glyph
    // names the divergence.
    const [node] = timelineFromAuditEntries([
      entry(1, "InvoiceAckStatus", {
        payload: { transaction_id: "tx-no-status" },
      }),
    ]);
    expect(node.glyph).toBe("•");
    expect(node.kind_class).toBe("kind-default");
    expect(node.label_html_safe).toBe("NAV ack");
  });
});

describe("timelineFromAuditEntries — unmodelled EventKind fallback", () => {
  it("falls back to kind-default with the raw wire string as label", () => {
    // The closed-vocab switch covers the operator-meaningful set;
    // every other EventKind (InvoiceSequenceReserved,
    // InvoiceSubmissionResponse, the four annulment kinds,
    // InvoiceSubmissionAttemptFailed, InvoiceCheckPerformed, etc.)
    // falls through to the muted-dot fallback. The raw wire string
    // remains visible so the operator can drop into the
    // "Show raw table" toggle for details.
    const [node] = timelineFromAuditEntries([
      entry(7, "InvoiceCheckPerformed"),
    ]);
    expect(node.glyph).toBe("•");
    expect(node.kind_class).toBe("kind-default");
    expect(node.label_html_safe).toBe("InvoiceCheckPerformed");
  });

  it("future-invented EventKind surfaces verbatim, not silently dropped", () => {
    // A backend drift that emits an EventKind the SPA does not
    // model must NOT crash and must NOT silently disappear from
    // the timeline (rule 12). The fallback arm renders the raw
    // string so the operator can file a bug.
    const [node] = timelineFromAuditEntries([
      entry(9, "InvoiceFutureInvented"),
    ]);
    expect(node.glyph).toBe("•");
    expect(node.kind_class).toBe("kind-default");
    expect(node.label_html_safe).toBe("InvoiceFutureInvented");
  });
});

describe("timelineFromAuditEntries — body lines", () => {
  it("always includes the actor", () => {
    const [node] = timelineFromAuditEntries([
      entry(1, "InvoiceDraftCreated", { actor: "spa" }),
    ]);
    expect(node.body_lines).toContain("actor: spa");
  });

  it("appends a chain base id line for InvoiceStornoIssued chain entries", () => {
    // The audit-row table renders chain_base_invoice_id as a
    // clickable affordance; the timeline body line preserves the
    // information without the click (operator can flip to the raw
    // table for the navigation). A future PR may surface a
    // chain-navigation affordance on the timeline directly; the
    // pure module already carries the field so no wire-shape
    // change is needed.
    const [node] = timelineFromAuditEntries([
      entry(1, "InvoiceStornoIssued", {
        chain_base_invoice_id: "inv_BASE_ULID_01",
      }),
    ]);
    expect(node.body_lines).toEqual([
      "actor: cli",
      "→ Base: inv_BASE_ULID_01",
    ]);
  });

  it("omits the chain base line when chain_base_invoice_id is null", () => {
    const [node] = timelineFromAuditEntries([
      entry(1, "InvoiceDraftCreated"),
    ]);
    expect(node.body_lines).toEqual(["actor: cli"]);
  });
});

describe("timelineFromAuditEntries — node shape", () => {
  it("carries seq as the id and the occurred_at ISO string verbatim", () => {
    // The id is the Svelte #each key. The occurred_at string lands
    // on both ts_iso (for <time datetime=>) and ts_display (the
    // operator-facing string) verbatim per the helper's stated
    // simplicity-first posture — a future locale-aware formatter
    // can extend ts_display additively without touching call sites.
    const [node] = timelineFromAuditEntries([
      entry(42, "InvoiceDraftCreated", {
        occurred_at: "2026-01-15T09:30:00Z",
      }),
    ]);
    expect(node.id).toBe("42");
    expect(node.ts_iso).toBe("2026-01-15T09:30:00Z");
    expect(node.ts_display).toBe("2026-01-15T09:30:00Z");
  });

  it("returns an empty array for empty input", () => {
    // The Svelte renderer branches on `nodes.length === 0` to show
    // the empty-state copy; the mapper's contract is "in:0 → out:0".
    expect(timelineFromAuditEntries([])).toEqual([]);
  });
});

describe("timelineFromAuditEntries — chronological order preserved", () => {
  it("returns nodes in input order (the backend emits in seq order)", () => {
    // get_audit_for_invoice walks the ledger in append-only seq
    // order; the timeline renders top-down as chronological. A
    // sort or reverse would invert the operator's mental model of
    // "what happened first". Pinned with a multi-kind input so a
    // sort by kind-string (instead of preserving seq order) would
    // also fail.
    const entries: AuditEntryView[] = [
      entry(1, "InvoiceDraftCreated"),
      entry(2, "InvoiceSubmissionAttempt"),
      entry(3, "InvoiceAckStatus", { payload: { ack_status: "SAVED" } }),
      entry(4, "InvoiceStornoIssued", {
        chain_base_invoice_id: "inv_BASE",
      }),
    ];
    const nodes = timelineFromAuditEntries(entries);
    expect(nodes.map((n: TimelineNode) => n.id)).toEqual([
      "1",
      "2",
      "3",
      "4",
    ]);
    expect(nodes.map((n: TimelineNode) => n.label_html_safe)).toEqual([
      "Invoice issued",
      "Submitted to NAV",
      "NAV ack: SAVED",
      "Storno issued",
    ]);
  });
});
