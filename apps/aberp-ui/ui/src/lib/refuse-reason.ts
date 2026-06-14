// S403 — operator REFUSE-with-reason: the reason-field validation
// contract. Pure so the modal's enable/disable + inline error is
// testable without a Svelte renderer (mirrors `deal-gate-state.ts`).
//
// The server (`validate_refuse_reason` in serve.rs) is the source of
// truth per [[trust-code-not-operator]]; this mirror keeps the two
// citing the SAME floor so the SPA never lets a reason through that the
// server would 400.

/** Minimum reason length (chars, after trim). Must match the backend
 * `quote_refuse::REASON_MIN_CHARS`. */
export const REFUSE_REASON_MIN_CHARS = 5;

/** Upper bound — matches the backend `REFUSE_REASON_MAX_CHARS` (the
 * storefront `notes` cap). */
export const REFUSE_REASON_MAX_CHARS = 2000;

/** True if the string contains any control char (code < 0x20, or DEL
 * 0x7f). Rejected so the reason stays a single line — safe both as the
 * storefront `notes` (header-injection gate) and the e-mail subject
 * line. A char-code scan avoids a control-char regex literal. */
function hasControlChar(s: string): boolean {
  for (let i = 0; i < s.length; i++) {
    const code = s.charCodeAt(i);
    if (code < 0x20 || code === 0x7f) return true;
  }
  return false;
}

/** Validate the operator reason. Returns `null` when valid, or a short
 * bilingual operator-readable error when not. */
export function validateRefuseReason(raw: string): string | null {
  const trimmed = raw.trim();
  if (trimmed.length < REFUSE_REASON_MIN_CHARS) {
    return `Az indok legalább ${REFUSE_REASON_MIN_CHARS} karakter legyen. / Reason must be at least ${REFUSE_REASON_MIN_CHARS} characters.`;
  }
  if (trimmed.length > REFUSE_REASON_MAX_CHARS) {
    return `Túl hosszú (max ${REFUSE_REASON_MAX_CHARS}). / Too long (max ${REFUSE_REASON_MAX_CHARS}).`;
  }
  if (hasControlChar(trimmed)) {
    return "Az indok egysoros legyen (nincs sortörés). / Reason must be a single line (no line breaks).";
  }
  return null;
}
