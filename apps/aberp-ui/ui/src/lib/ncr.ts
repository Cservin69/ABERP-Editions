// S439 — pure client-side helpers for the Quality (NCR + CAPA) module.
//
// Label maps, the NCR state-transition graph (MIRRORS the backend's
// authoritative `aberp::quality::allowed_transition` — the POST route
// re-validates and is the source of truth), the CAPA close-gate predicate, and
// the description validator. These give the operator instant feedback before the
// round-trip.
//
// Pinned by `ncr.test.ts`.

import type { Capa, NcrSeverity, NcrCategory, NcrState, CapaVerdict } from "./api";

/** Max NCR description length the backend accepts. */
const DESCRIPTION_MAX = 4000;

/** Bilingual (HU / EN) severity labels. */
export const SEVERITY_LABELS: Record<NcrSeverity, string> = {
  critical: "Kritikus / Critical",
  major: "Súlyos / Major",
  minor: "Kisebb / Minor",
};

/** Bilingual category labels. */
export const CATEGORY_LABELS: Record<NcrCategory, string> = {
  material: "Anyag / Material",
  workmanship: "Munkavégzés / Workmanship",
  documentation: "Dokumentáció / Documentation",
  equipment_failure: "Berendezéshiba / Equipment failure",
  operator_error: "Kezelői hiba / Operator error",
  supplier_issue: "Beszállítói probléma / Supplier issue",
  other: "Egyéb / Other",
};

/** Bilingual NCR state labels. */
export const STATE_LABELS: Record<NcrState, string> = {
  open: "Nyitott / Open",
  contained: "Elszigetelve / Contained",
  under_investigation: "Vizsgálat alatt / Under investigation",
  correction_applied: "Javítás alkalmazva / Correction applied",
  closed: "Lezárva / Closed",
  escalated: "Eszkalálva / Escalated",
};

/** Bilingual CAPA effectiveness verdict labels. */
export const VERDICT_LABELS: Record<CapaVerdict, string> = {
  verified: "Igazolt / Verified",
  not_effective: "Nem hatékony / Not effective",
  pending: "Függőben / Pending",
};

/** S439 — the allowed next states from a given NCR state. MIRRORS
 * `aberp::quality::allowed_transition` exactly. `closed` is terminal (empty). */
export function allowedNextStates(state: NcrState): NcrState[] {
  switch (state) {
    case "open":
      return ["contained", "under_investigation", "escalated"];
    case "contained":
      return ["under_investigation", "escalated"];
    case "under_investigation":
      return ["correction_applied", "escalated"];
    case "correction_applied":
      return ["closed", "escalated"];
    case "escalated":
      return ["under_investigation", "correction_applied", "closed"];
    case "closed":
      return [];
  }
}

/** S439 — whether a CAPA permits closing its parent NCR. MIRRORS
 * `aberp::quality::Capa::permits_ncr_close`: approved AND effectiveness-Verified.
 * The backend re-checks this at the close route (409 otherwise). */
export function capaPermitsClose(c: Capa): boolean {
  return c.approved_at_utc != null && c.effectiveness_verdict === "verified";
}

/** S439 — validate an operator-typed NCR description. Mirrors
 * `aberp::quality::validate_description`. Returns an error message string
 * (HU / EN), or `null` when acceptable. */
export function validateNcrDescription(s: string): string | null {
  const v = s.trim();
  if (v.length === 0) {
    return "A leírás nem lehet üres / description must not be blank";
  }
  if (v.length > DESCRIPTION_MAX) {
    return `Legfeljebb ${DESCRIPTION_MAX} karakter / At most ${DESCRIPTION_MAX} characters`;
  }
  return null;
}

/** Split a comma/newline-separated operator textarea into a trimmed,
 * non-empty array (used for the affected part-UID / WO / heat-lot lists). */
export function splitList(s: string): string[] {
  return s
    .split(/[\n,]/)
    .map((x) => x.trim())
    .filter((x) => x.length > 0);
}
