// ADR-0199 — pure-module helpers for the QC/AS9102 FAIR + Certificate-of-
// Conformance report screen. Display labels, the draft-body composer (with
// the CoC wire/storage token split), the disposition→shipment rule, the
// frozen-field awareness, tolerance/actual formatters, the client-side void
// validator, and the list filter all live here so vitest pins them without
// mounting a Svelte component (mirror of `inspection-plans.ts`).
//
// Pinned by `qc-reports.test.ts`.

import type {
  QcReport,
  QcReportLine,
  QcReportKind,
  QcReportKindInput,
  QcReportTemplate,
  QcReportState,
  QcDisposition,
  QcAccountability,
  DraftQcReportBody,
} from "./api";

// ── Report-kind wire ↔ draft-input token ────────────────────────────────
//
// The ONE asymmetry in the vocab: a Certificate of Conformance reads back
// (serde) as `"certificate_of_conformance"` but is DRAFTED with the storage
// token `"coc"`. Every other kind/token coincides. These two functions are
// the single crossing point; the rest of the UI never hard-codes the split.

/** Wire kind (as read back) → the token you POST to draft that same kind. */
export function draftTokenForKind(kind: QcReportKind): QcReportKindInput {
  return kind === "certificate_of_conformance" ? "coc" : kind;
}

/** The three kinds the operator can draft, as (input token, wire kind, label)
 * triples — the source of a draft-form dropdown. */
export const DRAFTABLE_KINDS: ReadonlyArray<{
  input: QcReportKindInput;
  wire: QcReportKind;
  label: string;
}> = [
  {
    input: "dimensional_inspection",
    wire: "dimensional_inspection",
    label: "Dimensional inspection",
  },
  {
    input: "as9102_fair",
    wire: "as9102_fair",
    label: "AS9102 First Article (FAIR)",
  },
  { input: "coc", wire: "certificate_of_conformance", label: "Certificate of Conformance" },
];

// ── Kind ↔ template compatibility ───────────────────────────────────────
//
// MIRRORS the backend `QcReportTemplate::permits(kind)`: not every template
// produces every kind, and the backend rejects an incompatible pair at draft
// (400 "template X does not produce a Y report"). The screen filters the
// template picker by the chosen kind to pre-empt that. AbenStandard covers
// the per-shipment pair (dimensional + CoC); As9102RevC covers everything; a
// FAIR is an AS9102 artefact and needs As9102RevC.
export function templatePermitsKind(
  template: QcReportTemplate,
  kind: QcReportKindInput,
): boolean {
  switch (template) {
    case "aben_standard":
      return kind === "dimensional_inspection" || kind === "coc";
    case "as9102_rev_c":
      return true;
    case "coc_only":
      return kind === "coc";
  }
}

/** The explicit templates that can produce this kind (excludes the
 * "customer default" option, which the caller adds separately). */
export function templatesForKind(kind: QcReportKindInput): QcReportTemplate[] {
  const all: QcReportTemplate[] = ["aben_standard", "as9102_rev_c", "coc_only"];
  return all.filter((t) => templatePermitsKind(t, kind));
}

// ── Display labels (English — the certificate surface is a US/aerospace
//    compliance artifact; the ERP chrome around it is bilingual elsewhere) ─

export function reportKindLabel(kind: QcReportKind): string {
  switch (kind) {
    case "dimensional_inspection":
      return "Dimensional inspection";
    case "certificate_of_conformance":
      return "Certificate of Conformance";
    case "as9102_fair":
      return "AS9102 FAIR";
  }
}

export function templateLabel(template: QcReportTemplate): string {
  switch (template) {
    case "aben_standard":
      return "ÁBEN standard";
    case "as9102_rev_c":
      return "AS9102 Rev C";
    case "coc_only":
      return "CoC only";
  }
}

export function stateLabel(state: QcReportState): string {
  switch (state) {
    case "drafted":
      return "Drafted";
    case "issued":
      return "Issued";
    case "superseded":
      return "Superseded";
    case "voided":
      return "Voided";
  }
}

/** A coarse tone token the screen maps to a chip colour. Kept as a string
 * (not a CSS value) so the component owns the palette. */
export type Tone = "neutral" | "positive" | "warning" | "danger";

export function stateTone(state: QcReportState): Tone {
  switch (state) {
    case "issued":
      return "positive";
    case "drafted":
      return "neutral";
    case "superseded":
      return "warning";
    case "voided":
      return "danger";
  }
}

export function dispositionLabel(d: QcDisposition): string {
  switch (d) {
    case "accept":
      return "Accept";
    case "accept_with_ncr":
      return "Accept (with NCR)";
    case "reject":
      return "Reject";
    case "incomplete":
      return "Incomplete";
  }
}

export function dispositionTone(d: QcDisposition): Tone {
  switch (d) {
    case "accept":
      return "positive";
    case "accept_with_ncr":
      return "warning";
    case "reject":
      return "danger";
    case "incomplete":
      return "warning";
  }
}

/** Whether this disposition permits shipment — MIRRORS the backend
 * `Disposition::permits_shipment()`. Advisory only: the server gate is
 * authoritative. `accept` and `accept_with_ncr` ship; `reject` and
 * `incomplete` do not. */
export function permitsShipment(d: QcDisposition): boolean {
  return d === "accept" || d === "accept_with_ncr";
}

export function accountabilityLabel(a: QcAccountability): string {
  switch (a) {
    case "measured":
      return "Measured";
    case "not_measured":
      return "Not measured";
    case "not_applicable":
      return "N/A";
  }
}

// ── Report-line formatters ──────────────────────────────────────────────

/** Nominal ± tolerance band for a line, e.g. `"12.500 +0.050 / -0.020 mm"`.
 * Returns `"—"` when the line carries no nominal (a note / material
 * characteristic). Pure; the units suffix is appended only when present. */
export function toleranceLabel(
  line: Pick<QcReportLine, "nominal_value" | "upper_tol" | "lower_tol" | "units">,
): string {
  if (line.nominal_value === null) return "—";
  const u = line.units ? ` ${line.units}` : "";
  const up = line.upper_tol === null ? "" : ` +${line.upper_tol}`;
  const lo = line.lower_tol === null ? "" : ` / -${Math.abs(line.lower_tol)}`;
  return `${line.nominal_value}${up}${lo}${u}`.trim();
}

/** The measured actual for a line, or an explicit blank for an unmeasured
 * (accountability) row — never a fabricated zero. */
export function actualLabel(
  line: Pick<QcReportLine, "actual_value" | "accountability" | "units">,
): string {
  if (line.accountability === "not_measured") return "— (not measured)";
  if (line.accountability === "not_applicable") return "N/A";
  if (line.actual_value === null) return "—";
  const u = line.units ? ` ${line.units}` : "";
  return `${line.actual_value}${u}`;
}

// ── Draft composer ──────────────────────────────────────────────────────

/** Operator-typed draft-form state. `kind` is the INPUT token (so a CoC is
 * `"coc"`); `template` empty ⇒ resolve from the customer default. */
export interface DraftFormState {
  woId: string;
  kind: QcReportKindInput;
  template: "" | QcReportTemplate;
  notes: string;
}

export function emptyDraftForm(woId: string): DraftFormState {
  return { woId, kind: "dimensional_inspection", template: "", notes: "" };
}

/** Fold the draft form into the wire body. Omits `template` when blank (so
 * the backend resolves the partner default) and omits empty notes. Trims. */
export function composeDraftBody(form: DraftFormState): DraftQcReportBody {
  const body: DraftQcReportBody = {
    wo_id: form.woId.trim(),
    report_kind: form.kind,
  };
  if (form.template !== "") body.template = form.template;
  const notes = form.notes.trim();
  if (notes.length > 0) body.notes = notes;
  return body;
}

/** Client-side draft validation (the backend re-checks authoritatively).
 * A work order is required. */
export function validateDraftForm(form: DraftFormState): Record<string, string> {
  const errors: Record<string, string> = {};
  if (form.woId.trim().length === 0) {
    errors.wo_id = "A work order is required.";
  }
  return errors;
}

/** Void-form validation: a reason is required (the backend rejects a blank
 * reason with 400). */
export function validateVoidReason(reason: string): Record<string, string> {
  const errors: Record<string, string> = {};
  if (reason.trim().length === 0) {
    errors.reason = "A reason is required to void a report.";
  }
  return errors;
}

// ── Lifecycle predicates (drive which actions the detail pane shows) ─────

export function canIssue(report: Pick<QcReport, "state">): boolean {
  return report.state === "drafted";
}

/** A drafted or issued report can be voided; a voided/superseded one cannot
 * be voided again. */
export function canVoid(report: Pick<QcReport, "state">): boolean {
  return report.state === "drafted" || report.state === "issued";
}

/** Only a current (drafted or issued) report renders a PDF; a voided /
 * superseded one has no document (the backend returns 409). */
export function canRenderPdf(report: Pick<QcReport, "state">): boolean {
  return report.state === "drafted" || report.state === "issued";
}

// ── List filter ─────────────────────────────────────────────────────────

/** Case-insensitive substring search over report number, kind label, and
 * work-order id. Empty needle returns the list unchanged. */
export function filterReports(rows: QcReport[], needle: string): QcReport[] {
  const q = needle.trim().toLowerCase();
  if (q.length === 0) return rows;
  return rows.filter(
    (r) =>
      r.report_number.toLowerCase().includes(q) ||
      reportKindLabel(r.report_kind).toLowerCase().includes(q) ||
      r.wo_id.toLowerCase().includes(q),
  );
}

// ── Error message extraction ────────────────────────────────────────────

/** Pull a human-readable message out of a Tauri-wrapped backend error.
 * The QC routes answer failures as a JSON body carrying `error`/`message`;
 * this peels the JSON out of the wrapper string and returns the best
 * message, falling back to the raw string. Never throws. */
export function qcErrorMessage(raw: string): string {
  const start = raw.indexOf("{");
  const end = raw.lastIndexOf("}");
  if (start >= 0 && end > start) {
    try {
      const obj = JSON.parse(raw.slice(start, end + 1)) as Record<string, unknown>;
      const msg = obj.message ?? obj.error;
      if (typeof msg === "string" && msg.trim().length > 0) return msg;
    } catch {
      /* fall through to the raw string */
    }
  }
  return raw.trim();
}
