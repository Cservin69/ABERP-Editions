// S352 / PR-41 — pure orchestration for the Auto-Quoting detail panel's
// PDF View/Download buttons. The browser side-effects (blob-URL minting,
// opening a tab, triggering a download anchor, revoking the URL) are
// injected as `deps` so the flow is unit-testable without a DOM — same
// pure-function-helper posture as `pricing-material-edit.ts` and
// `format.ts::filenameForInvoice` (the codebase does NOT render Svelte
// components in vitest; the `{#if detail.pdf_available}` button-
// visibility gate is structural in the template).

/** Operator-facing download filename for a rendered quote PDF. Keyed on
 * the quote ref (never an fs path). Mirrors the backend's
 * `serve::pricing_job_pdf_filename` so the browser-saved file matches
 * the `Content-Disposition` regardless of which one the engine honours. */
export function quotePdfFilename(quoteId: string): string {
  return `quote-${quoteId}.pdf`;
}

/** View opens the PDF in a new tab; Download saves it via a synthetic
 * anchor. Both fetch the same Bearer-authenticated blob. */
export type QuotePdfAction = "view" | "download";

/** Injected browser seams. The Svelte component supplies the real
 * implementations (`api.downloadQuotePricingJobPdf`, `URL.createObjectURL`,
 * `window.open`, a synthetic `<a download>`, and a delayed
 * `URL.revokeObjectURL`); tests supply spies. */
export interface QuotePdfDeps {
  /** Bearer-authenticated fetch of the PDF bytes as a `Blob` (the
   *  Bearer is injected by the Tauri command seam, never in the URL). */
  download: (quoteId: string) => Promise<Blob>;
  createObjectURL: (blob: Blob) => string;
  /** Open the blob URL in a new tab (View). */
  openInNewTab: (url: string) => void;
  /** Trigger a browser download of the blob URL under `filename`. */
  triggerDownload: (url: string, filename: string) => void;
  /** Schedule revocation of the blob URL once the tab/save has consumed
   *  it (the component uses a delayed `URL.revokeObjectURL`). Always
   *  called so neither path leaks the object URL. */
  scheduleRevoke: (url: string) => void;
}

/** Run a View or Download against a rendered quote PDF. Rejects (so the
 * caller surfaces the message inline) if `download` rejects — e.g. a 404
 * `PdfNotRendered` / `PdfFileMissing` from the backend. The blob URL is
 * only minted after a successful fetch, so a failed fetch never leaks a
 * URL or opens a blank tab. */
export async function runQuotePdfAction(
  deps: QuotePdfDeps,
  quoteId: string,
  action: QuotePdfAction,
): Promise<void> {
  const blob = await deps.download(quoteId);
  const url = deps.createObjectURL(blob);
  if (action === "view") {
    deps.openInNewTab(url);
  } else {
    deps.triggerDownload(url, quotePdfFilename(quoteId));
  }
  deps.scheduleRevoke(url);
}
