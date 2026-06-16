// S438 — pure client-side validators for the part-UID marking form.
// These MIRROR the backend's authoritative checks (the POST route
// re-validates and is the source of truth); they exist only to give the
// operator instant inline feedback before the round-trip.
//
// The operator NEVER types a part UID — the server mints the `dp-<ULID>`.
// The only operator input is the optional serial (blank → auto-derived).
//
// Pinned by `part-uid.test.ts`.

/** Max serial length the backend accepts. */
const SERIAL_MAX = 64;

/** True if `v` contains any C0 control char (0x00–0x1F) or DEL (0x7F). */
function hasControlChar(v: string): boolean {
  for (let i = 0; i < v.length; i++) {
    const c = v.charCodeAt(i);
    if (c < 0x20 || c === 0x7f) {
      return true;
    }
  }
  return false;
}

/** S438 — validate an OPTIONAL operator-typed serial. Empty is OK (the
 * server auto-derives `<wo_id>-<index>`). A non-empty serial must be ≤64
 * chars, carry no `|` (the DataMatrix delimiter), and no control chars.
 * Mirrors `aberp::part_marking::validate_serial`. Returns an error message
 * string (HU/EN), or `null` when acceptable. */
export function validateSerial(s: string): string | null {
  const v = s.trim();
  if (v.length === 0) {
    return null; // blank → server auto-derives
  }
  if (v.length > SERIAL_MAX) {
    return `Legfeljebb ${SERIAL_MAX} karakter / At most ${SERIAL_MAX} characters`;
  }
  if (v.includes("|")) {
    return "A '|' nem engedélyezett / '|' is not allowed (DataMatrix delimiter)";
  }
  if (hasControlChar(v)) {
    return "Vezérlőkarakter nem engedélyezett / No control characters";
  }
  return null;
}

/** S438 — validate a part UID has the `dp-` + 26-char Crockford-base32 ULID
 * shape. Used only to render scanned/returned UIDs defensively; mirrors
 * `aberp::part_marking::validate_part_uid`. Returns an error message string,
 * or `null` when acceptable. */
export function validatePartUid(s: string): string | null {
  const v = s.trim();
  if (!v.startsWith("dp-")) {
    return "A part UID 'dp-' előtaggal kezdődik / part UID must start with 'dp-'";
  }
  const body = v.slice(3);
  if (body.length !== 26) {
    return "A ULID-törzs 26 karakter / ULID body must be 26 chars";
  }
  if (!/^[0-9ABCDEFGHJKMNPQRSTVWXYZ]+$/.test(body)) {
    return "Érvénytelen ULID-törzs / invalid ULID body";
  }
  return null;
}
