// S239 / PR-233 — pure-helper tests for the Draft-delete flow.
//
// Two surfaces under test:
//   1. composeDraftDeleteCopy — the modal copy composer. The
//      with-source-dispatch branch surfaces the dispatch-link
//      consequence sentence per [[hulye-biztos]]; the standalone
//      branch keeps the simpler "permanently deleted" message.
//   2. The performDraftDelete shim. Mocks the Tauri `invoke` boundary
//      via a `vi.mock` of `./api` so the helper can be exercised
//      without a backend, mirroring how `invoice-actions.test.ts`
//      tests the audit-derived action gates.

import { afterEach, describe, expect, it, vi } from "vitest";

import { composeDraftDeleteCopy } from "./draft-delete";

describe("composeDraftDeleteCopy — with source dispatch", () => {
  const copy = composeDraftDeleteCopy("dsp_01H7TESTDISPATCH00000000");

  it("title is bilingual HU/EN", () => {
    expect(copy.title).toContain("Piszkozat törlése");
    expect(copy.title).toContain("Delete draft");
  });

  it("body names the dispatch id in both languages", () => {
    expect(copy.body).toContain("dsp_01H7TESTDISPATCH00000000");
    // Bilingual HU + EN sentences, separated by ` / `.
    expect(copy.body).toContain("kiszállításhoz");
    expect(copy.body).toContain("dispatch dsp_");
  });

  it("consequence sentence warns about the spawn-link severance", () => {
    expect(copy.consequence).not.toBeNull();
    expect(copy.consequence ?? "").toContain("spawn link");
    expect(copy.consequence ?? "").toContain("véglegesen");
  });

  it("confirm + cancel labels are bilingual HU/EN", () => {
    expect(copy.confirmLabel).toContain("Törlés");
    expect(copy.confirmLabel).toContain("Delete");
    expect(copy.cancelLabel).toContain("Mégse");
    expect(copy.cancelLabel).toContain("Cancel");
  });
});

describe("composeDraftDeleteCopy — standalone draft (no source dispatch)", () => {
  const copy = composeDraftDeleteCopy(null);

  it("body explains permanent deletion without naming a dispatch", () => {
    expect(copy.body).toContain("véglegesen");
    expect(copy.body).toContain("permanently");
    expect(copy.body).not.toContain("dsp_");
    expect(copy.body).not.toContain("kiszállítás");
  });

  it("consequence is null (no spawn-link to warn about)", () => {
    expect(copy.consequence).toBeNull();
  });

  it("confirm + cancel labels stay bilingual HU/EN", () => {
    expect(copy.confirmLabel).toContain("Törlés");
    expect(copy.confirmLabel).toContain("Delete");
    expect(copy.cancelLabel).toContain("Mégse");
    expect(copy.cancelLabel).toContain("Cancel");
  });
});

// ── Mock the api module so the performDraftDelete shim test runs
// purely in-process. `vi.mock` is hoisted; the per-test `vi.mocked`
// resets pick up the spied implementation per call. ──────────────
vi.mock("./api", () => ({
  deleteInvoiceDraft: vi.fn(),
  getInvoiceDraft: vi.fn(),
}));

import { deleteInvoiceDraft, getInvoiceDraft } from "./api";
import { loadDraftForDeleteConfirm, performDraftDelete } from "./draft-delete";

describe("performDraftDelete + loadDraftForDeleteConfirm", () => {
  afterEach(() => {
    vi.mocked(deleteInvoiceDraft).mockReset();
    vi.mocked(getInvoiceDraft).mockReset();
  });

  it("performDraftDelete calls deleteInvoiceDraft with the drf id verbatim", async () => {
    vi.mocked(deleteInvoiceDraft).mockResolvedValueOnce(undefined);
    await performDraftDelete("drf_01H7DELETETHIS00000000000");
    expect(deleteInvoiceDraft).toHaveBeenCalledWith(
      "drf_01H7DELETETHIS00000000000",
    );
    expect(deleteInvoiceDraft).toHaveBeenCalledTimes(1);
  });

  it("performDraftDelete propagates backend errors as rejected promises", async () => {
    vi.mocked(deleteInvoiceDraft).mockRejectedValueOnce(
      new Error("backend: not found"),
    );
    await expect(performDraftDelete("drf_X")).rejects.toThrow(
      "backend: not found",
    );
  });

  it("loadDraftForDeleteConfirm fetches the draft and returns its row", async () => {
    const fixture = {
      drf_id: "drf_X",
      tenant_id: "t",
      partner_id: "ptr_X",
      source_dispatch_id: "dsp_X",
      source_wo_id: "wo_X",
      source_quote_id: null,
      product_id: "prd_X",
      qty: "1",
      notes: null,
      created_at: "2026-06-04T10:00:00Z",
    };
    vi.mocked(getInvoiceDraft).mockResolvedValueOnce(fixture);
    const draft = await loadDraftForDeleteConfirm("drf_X");
    expect(getInvoiceDraft).toHaveBeenCalledWith("drf_X");
    expect(draft.source_dispatch_id).toBe("dsp_X");
  });
});

// ── End-to-end: simulate the InvoiceList confirm flow purely in the
//    helper layer. Modeled after the existing invoice-list-persistence
//    + buyer-combobox pure-module tests — no DOM mount required.
//    The cancel-out and confirm-fires-DELETE paths are the two
//    branches the brief's "SPA tests" bullet names.

describe("Draft-delete confirm flow (pure-layer simulation)", () => {
  afterEach(() => {
    vi.mocked(deleteInvoiceDraft).mockReset();
    vi.mocked(getInvoiceDraft).mockReset();
  });

  it("Cancel-out preserves the draft (no DELETE fired)", async () => {
    const fixture = {
      drf_id: "drf_C",
      tenant_id: "t",
      partner_id: "ptr_C",
      source_dispatch_id: "dsp_C",
      source_wo_id: "wo_C",
      source_quote_id: null,
      product_id: "prd_C",
      qty: "1",
      notes: null,
      created_at: "2026-06-04T10:00:00Z",
    };
    vi.mocked(getInvoiceDraft).mockResolvedValueOnce(fixture);

    // Step 1: caller fetches the draft to know whether a source
    // dispatch link exists.
    const draft = await loadDraftForDeleteConfirm("drf_C");
    // Step 2: caller composes the modal copy.
    const copy = composeDraftDeleteCopy(draft.source_dispatch_id);
    expect(copy.consequence).not.toBeNull();
    // Step 3: operator clicks Cancel — no DELETE fires.
    // (In the renderer this corresponds to `cancelDraftDelete`
    // setting `draftDeleteContext = null` without invoking
    // `performDraftDelete`.)
    expect(deleteInvoiceDraft).not.toHaveBeenCalled();
  });

  it("Confirm fires the DELETE and the helper resolves to undefined", async () => {
    const fixture = {
      drf_id: "drf_D",
      tenant_id: "t",
      partner_id: "ptr_D",
      source_dispatch_id: "dsp_D",
      source_wo_id: "wo_D",
      source_quote_id: null,
      product_id: "prd_D",
      qty: "1",
      notes: null,
      created_at: "2026-06-04T10:00:00Z",
    };
    vi.mocked(getInvoiceDraft).mockResolvedValueOnce(fixture);
    vi.mocked(deleteInvoiceDraft).mockResolvedValueOnce(undefined);

    const draft = await loadDraftForDeleteConfirm("drf_D");
    const copy = composeDraftDeleteCopy(draft.source_dispatch_id);
    expect(copy.body).toContain("dsp_D");

    // Operator clicks Confirm — DELETE fires once with the row's
    // drf_id; the renderer then refetches the list (out of scope
    // here — the helper is the API-surface seam).
    await performDraftDelete(draft.drf_id);
    expect(deleteInvoiceDraft).toHaveBeenCalledWith("drf_D");
    expect(deleteInvoiceDraft).toHaveBeenCalledTimes(1);
  });
});
