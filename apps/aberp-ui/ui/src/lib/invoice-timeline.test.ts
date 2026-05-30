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

// S163 — InvoiceEmailedSent forks on its closed-vocab `outcome`
// ("succeeded" | "failed"). Pre-S163 this kind fell through to the
// muted `•` default and rendered the raw "InvoiceEmailedSent" string
// IDENTICALLY for both outcomes, so a failed send read as "sent" on
// the timeline. These pins lock the truthful per-outcome labels.
describe("timelineFromAuditEntries — InvoiceEmailedSent forks per outcome", () => {
  it("outcome=succeeded → ✉ kind-email-sent 'Email sent'", () => {
    const [node] = timelineFromAuditEntries([
      entry(6, "InvoiceEmailedSent", {
        payload: { outcome: "succeeded", recipient: "a@b.hu" },
      }),
    ]);
    expect(node.glyph).toBe("✉");
    expect(node.kind_class).toBe("kind-email-sent");
    expect(node.label_html_safe).toBe("Email sent");
    // No failure detail line on a success entry.
    expect(node.body_lines).toEqual(["actor: cli"]);
  });

  it("outcome=failed → ⚠ kind-email-failed 'Email send failed' + detail line", () => {
    const [node] = timelineFromAuditEntries([
      entry(7, "InvoiceEmailedSent", {
        payload: {
          outcome: "failed",
          recipient: "a@b.hu",
          error_class: "transport",
          error_detail: "connection refused",
        },
      }),
    ]);
    expect(node.glyph).toBe("⚠");
    expect(node.kind_class).toBe("kind-email-failed");
    expect(node.label_html_safe).toBe("Email send failed");
    // The scrubbed class + detail surface as a body line (same posture
    // as InvoiceAckStatus validation messages).
    expect(node.body_lines).toContain("transport: connection refused");
  });

  it("failed with missing error fields still renders the failure label (no crash)", () => {
    const [node] = timelineFromAuditEntries([
      entry(8, "InvoiceEmailedSent", { payload: { outcome: "failed" } }),
    ]);
    expect(node.label_html_safe).toBe("Email send failed");
    // No detail fields → no extra body line beyond the actor.
    expect(node.body_lines).toEqual(["actor: cli"]);
  });

  it("missing/unknown outcome → muted default with the raw kind named loud", () => {
    const [node] = timelineFromAuditEntries([
      entry(9, "InvoiceEmailedSent", { payload: { outcome: "weird" } }),
    ]);
    expect(node.glyph).toBe("•");
    expect(node.kind_class).toBe("kind-default");
    // Surfaced raw rather than masquerading as a successful send.
    expect(node.label_html_safe).toBe("InvoiceEmailedSent");
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

describe("timelineFromAuditEntries — technicalValidationMessages on ack-status", () => {
  // PR-76 — the backend extracts NAV's `<technicalValidationMessages>`
  // array from the verbatim ack body and grafts it onto the payload
  // before the SPA sees it (`apps/aberp/src/serve.rs::audit_view_of`).
  // The timeline mapper renders each message as a body line under
  // the ack node so an operator staring at "Rejected" sees WHY
  // without dropping into the raw-table toggle for the response_xml
  // bytes. Pinned against the actual invoice-17 rejection shape.
  it("renders ABORTED ack with the parsed validation message body lines", () => {
    const [node] = timelineFromAuditEntries([
      entry(1, "InvoiceAckStatus", {
        actor: "spa",
        payload: {
          ack_status: "ABORTED",
          transaction_id: "5E9J5P1AQVE90N2I",
          technical_validation_messages: [
            {
              result_code: "ERROR",
              error_code: "SCHEMA_VIOLATION",
              message:
                "XML contains on line: [5] and column: [16] error: [...completenessIndicator is expected.]",
              tag: null,
            },
            {
              result_code: "ERROR",
              error_code: "SCHEMA_VIOLATION",
              message: "Xml validation failed",
              tag: null,
            },
          ],
        },
      }),
    ]);
    expect(node.kind_class).toBe("kind-ack-aborted");
    expect(node.label_html_safe).toBe("NAV ack: ABORTED");
    expect(node.body_lines).toEqual([
      "actor: spa",
      "ERROR SCHEMA_VIOLATION: XML contains on line: [5] and column: [16] error: [...completenessIndicator is expected.]",
      "ERROR SCHEMA_VIOLATION: Xml validation failed",
    ]);
  });

  it("renders SAVED ack with no extra lines when the array is empty", () => {
    // SAVED is the happy path — NAV does not emit any technicalValidationMessages
    // and the backend grafts an empty array onto the payload. The timeline
    // must NOT render any extra lines below the actor, or the ack node
    // would always have a vestigial blank section.
    const [node] = timelineFromAuditEntries([
      entry(1, "InvoiceAckStatus", {
        payload: {
          ack_status: "SAVED",
          technical_validation_messages: [],
        },
      }),
    ]);
    expect(node.body_lines).toEqual(["actor: cli"]);
  });

  it("renders ABORTED ack without the field as a single actor line (defence in depth)", () => {
    // Old payload shape (pre-PR-76 ack entries already on disk) does NOT
    // carry the technical_validation_messages field. The mapper must
    // degrade gracefully — show the kind_class + label per the ack_status
    // and skip the messages section, NOT crash.
    const [node] = timelineFromAuditEntries([
      entry(1, "InvoiceAckStatus", {
        payload: { ack_status: "ABORTED" },
      }),
    ]);
    expect(node.kind_class).toBe("kind-ack-aborted");
    expect(node.body_lines).toEqual(["actor: cli"]);
  });

  it("renders a message with a missing field using ?-placeholders, not silent drop", () => {
    // CLAUDE.md rule 12 — if NAV (or a future wire-shape regression) omits
    // a required-shaped field on a single message, the operator still sees
    // the entry instead of having it silently disappear. The `?` markers
    // make the omission visible.
    const [node] = timelineFromAuditEntries([
      entry(1, "InvoiceAckStatus", {
        payload: {
          ack_status: "ABORTED",
          technical_validation_messages: [
            { result_code: null, error_code: null, message: null, tag: null },
          ],
        },
      }),
    ]);
    expect(node.body_lines).toEqual(["actor: cli", "? ?: (no message)"]);
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
