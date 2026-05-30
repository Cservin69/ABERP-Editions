// PR-179 / session-179 — pure helpers for the IncomingInvoiceList
// status-action interactions. Lifts the "irrelevant requires a
// non-empty reason" rule out of the Svelte component so a vitest pin
// can pin it without rendering DOM.

/** Trim a free-text reason and decide whether it satisfies the
 * backend's `ReasonRequiredForIrrelevant` invariant. The backend
 * loud-fails an empty / whitespace-only reason with HTTP 400; the SPA
 * gates the modal's submit button on the same predicate so the
 * operator never sees the round-trip error. */
export function isIrrelevantReasonValid(reason: string): boolean {
  return reason.trim().length > 0;
}

/** Toast / banner copy shown after a successful sync-now cycle. The
 * backend echoes counts; the SPA chooses the right HU/EN sentence
 * based on the ingested count. Returns the bilingual line the UI
 * renders verbatim. */
export function syncCompletedToast(ingested: number): {
  hu: string;
  en: string;
} {
  if (ingested === 0) {
    return {
      hu: "Szinkronizálás kész — nincs új bejövő számla.",
      en: "Sync complete — no new incoming invoices.",
    };
  }
  if (ingested === 1) {
    return {
      hu: "Szinkronizálás kész — 1 új bejövő számla.",
      en: "Sync complete — 1 new incoming invoice.",
    };
  }
  return {
    hu: `Szinkronizálás kész — ${ingested} új bejövő számla.`,
    en: `Sync complete — ${ingested} new incoming invoices.`,
  };
}
