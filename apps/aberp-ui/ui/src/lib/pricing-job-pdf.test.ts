// S352 / PR-41 — unit tests for the detail-panel PDF View/Download
// orchestration. Pure-function tests with injected browser seams; no
// DOM, no @testing-library/svelte (matches the repo convention — the
// button-visibility `{#if detail.pdf_available}` gate is structural).

import { describe, expect, it, vi } from "vitest";

import {
  quotePdfFilename,
  runQuotePdfAction,
  type QuotePdfDeps,
} from "./pricing-job-pdf";

const REF = "550e8400-e29b-41d4-a716-446655440000";

function makeDeps(
  overrides: Partial<QuotePdfDeps> = {},
): QuotePdfDeps & {
  download: ReturnType<typeof vi.fn>;
  createObjectURL: ReturnType<typeof vi.fn>;
  openInNewTab: ReturnType<typeof vi.fn>;
  triggerDownload: ReturnType<typeof vi.fn>;
  scheduleRevoke: ReturnType<typeof vi.fn>;
} {
  const blob = new Blob([new Uint8Array([1, 2, 3])], {
    type: "application/pdf",
  });
  return {
    download: vi.fn(async () => blob),
    createObjectURL: vi.fn(() => "blob:fake-url"),
    openInNewTab: vi.fn(),
    triggerDownload: vi.fn(),
    scheduleRevoke: vi.fn(),
    ...overrides,
  } as never;
}

describe("quotePdfFilename", () => {
  it("carries the quote ref verbatim", () => {
    expect(quotePdfFilename(REF)).toBe(`quote-${REF}.pdf`);
  });

  it("interpolates the ref (not a constant)", () => {
    expect(quotePdfFilename("00000000-0000-0000-0000-000000000001")).toBe(
      "quote-00000000-0000-0000-0000-000000000001.pdf",
    );
  });
});

describe("runQuotePdfAction — view", () => {
  it("fetches the blob, mints a URL, opens a new tab, and schedules revoke", async () => {
    const deps = makeDeps();
    await runQuotePdfAction(deps, REF, "view");

    expect(deps.download).toHaveBeenCalledWith(REF);
    expect(deps.createObjectURL).toHaveBeenCalledTimes(1);
    expect(deps.openInNewTab).toHaveBeenCalledWith("blob:fake-url");
    expect(deps.scheduleRevoke).toHaveBeenCalledWith("blob:fake-url");
    // View must NOT trigger a download anchor.
    expect(deps.triggerDownload).not.toHaveBeenCalled();
  });
});

describe("runQuotePdfAction — download", () => {
  it("triggers a download under the ref-based filename, not a new tab", async () => {
    const deps = makeDeps();
    await runQuotePdfAction(deps, REF, "download");

    expect(deps.download).toHaveBeenCalledWith(REF);
    expect(deps.triggerDownload).toHaveBeenCalledWith(
      "blob:fake-url",
      `quote-${REF}.pdf`,
    );
    expect(deps.scheduleRevoke).toHaveBeenCalledWith("blob:fake-url");
    expect(deps.openInNewTab).not.toHaveBeenCalled();
  });
});

describe("runQuotePdfAction — backend error", () => {
  it("propagates the rejection and never mints a URL or opens a tab", async () => {
    const deps = makeDeps({
      download: vi.fn(async () => {
        throw new Error(
          'backend returned 404 Not Found for /api/quote-pricing-jobs/x/pdf: {"error":"PdfNotRendered"}',
        );
      }),
    });

    await expect(runQuotePdfAction(deps, REF, "view")).rejects.toThrow(
      /PdfNotRendered/,
    );
    // A failed fetch must not leak a blob URL or open a blank tab.
    expect(deps.createObjectURL).not.toHaveBeenCalled();
    expect(deps.openInNewTab).not.toHaveBeenCalled();
    expect(deps.triggerDownload).not.toHaveBeenCalled();
    expect(deps.scheduleRevoke).not.toHaveBeenCalled();
  });
});
