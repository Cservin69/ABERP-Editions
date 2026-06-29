// S267 / PR-256 — shared display helpers for the four quoting-tunables
// pages. Bilingual labels for the closed-vocab enums (FeatureType,
// SizeBucket, ToleranceRange) so each page renders the operator-
// readable name beside the durable wire form.
//
// Closed-vocab + deny-default: an unknown wire form returns the
// verbatim string. The Rust side already validated on write, so
// "unknown" should only appear when the SPA is older than the backend.

import type {
  FeatureType,
  GeneralClassDb,
  SizeBucket,
  ToleranceRange,
  ToleranceSpec,
} from "./api";

export function featureTypeLabel(t: FeatureType | string): string {
  switch (t) {
    case "pocket":
      return "Pocket / Zseb";
    case "hole":
      return "Hole / Furat";
    case "slot":
      return "Slot / Horony";
    case "thread":
      return "Thread / Menet";
    case "undercut_5axis":
      return "Undercut (5-axis) / Alávágás";
    case "thin_wall":
      return "Thin wall / Vékony fal";
    case "surface":
      return "Surface / Felület";
    case "engraving":
      return "Engraving / Gravírozás";
    default:
      return t;
  }
}

export function sizeBucketLabel(b: SizeBucket | string): string {
  switch (b) {
    case "XS":
      return "XS (< 10mm)";
    case "S":
      return "S (10–30mm)";
    case "M":
      return "M (30–80mm)";
    case "L":
      return "L (80–200mm)";
    case "XL":
      return "XL (≥ 200mm)";
    default:
      return b;
  }
}

export function toleranceRangeLabel(t: ToleranceRange | string): string {
  switch (t) {
    case "loose":
      return "Loose (±0.1mm+)";
    case "standard":
      return "Standard (±0.05mm)";
    case "tight":
      return "Tight (±0.02mm)";
    case "precision":
      return "Precision (±0.01mm)";
    case "ultra_precision":
      return "Ultra-precision (≤ ±0.005mm)";
    default:
      return t;
  }
}

/** Format a signed fractional adjustment (e.g. -0.05 → "−5.0%"). */
export function fmtPct(p: number): string {
  if (!Number.isFinite(p)) return "—";
  const sign = p > 0 ? "+" : p < 0 ? "−" : "";
  const abs = Math.abs(p) * 100;
  return `${sign}${abs.toFixed(1)}%`;
}

/** T5 / ADR-0097 Part 2 — bilingual label for an ISO 2768 general
 * (title-block) class. Falls back to the verbatim wire string for an unknown
 * value (a SPA older than the backend). */
export function generalClassLabel(c: GeneralClassDb | string): string {
  switch (c) {
    case "iso2768_fine":
      return "ISO 2768-f (fine / finom)";
    case "iso2768_medium":
      return "ISO 2768-m (medium / közepes)";
    case "iso2768_coarse":
      return "ISO 2768-c (coarse / durva)";
    case "iso2768_very_coarse":
      return "ISO 2768-v (very coarse / nagyon durva)";
    default:
      return c;
  }
}

/** T5 / ADR-0097 Part 2 — compact label for a per-job / per-feature
 * [`ToleranceSpec`] (the professional drawing taxonomy), for the editor's
 * read-mode summary. */
export function toleranceSpecLabel(spec: ToleranceSpec): string {
  switch (spec.kind) {
    case "unspecified":
      return "Nincs megadva / Unspecified";
    case "general_class":
      return generalClassLabel(spec.class);
    case "it_grade":
      return `IT${spec.grade}`;
    case "plus_minus":
      return `±${spec.value_mm} mm`;
    case "per_drawing":
      return "Rajz szerint / Per drawing (GD&T)";
  }
}
