// S432 — pure client-side validators for the heat-lot assignment form.
// These MIRROR the backend's authoritative checks (the POST route
// re-validates and is the source of truth); they exist only to give the
// operator instant inline feedback before the round-trip.
//
// Pinned by `heat-lot.test.ts`.

/** Max heat-lot length the backend accepts. */
const HEAT_LOT_MAX = 32;

/** Allowed heat-lot characters: ASCII alphanumerics + `-`. */
const HEAT_LOT_RE = /^[A-Za-z0-9-]+$/;

/** S432 — validate a heat-lot number. Returns an error message string
 * (HU/EN) when invalid, or `null` when the value is acceptable. Mirrors
 * the backend: non-empty, ≤32 chars, `[A-Za-z0-9-]` only. */
export function validateHeatLot(s: string): string | null {
  const v = s.trim();
  if (v.length === 0) {
    return "Kötelező mező / Heat lot is required";
  }
  if (v.length > HEAT_LOT_MAX) {
    return `Legfeljebb ${HEAT_LOT_MAX} karakter / At most ${HEAT_LOT_MAX} characters`;
  }
  if (!HEAT_LOT_RE.test(v)) {
    return "Csak betű, szám és kötőjel / Only letters, digits and '-'";
  }
  return null;
}

/** S432 — validate the optional mill-test-report URL. Empty is OK (no
 * MTR yet); a non-empty value must start with `file://`. Mirrors the
 * backend. Returns an error message string, or `null` when acceptable. */
export function validateMtrUrl(s: string): string | null {
  const v = s.trim();
  if (v.length === 0) {
    return null;
  }
  if (!v.startsWith("file://")) {
    return "file:// útvonal kell / must start with file://";
  }
  return null;
}
