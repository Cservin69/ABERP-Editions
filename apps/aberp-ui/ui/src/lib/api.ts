// Tauri command surface — the SPA's ONLY path to the backend.
//
// Per ADR-0021 §Part B, the wire protocol is HTTPS+JSON. The TLS
// termination + bearer-token attachment + fingerprint pinning all
// happen in Rust (see `apps/aberp-ui/src/commands.rs`). The SPA
// never sees the URL, the cert, or the token.
//
// Per ADR-0007 §"Tauri allow-list", the SPA is treated as
// semi-trusted. Every command here has a matching `#[tauri::command]`
// handler on the Rust side; the names MUST stay in sync. The Rust
// `tauri::generate_handler!` macro lists the four names in
// `lib.rs`'s `Builder::default()` chain.

import { invoke } from "@tauri-apps/api/core";

import { isDemoMode } from "./workshop-demo-mode";
import { getMockDashboard } from "./workshop-mock-data";

/** PR-44ε / session-53 — typed wire mirror for the `aberp_billing::Currency`
 * enum per ADR-0037 §3. Two variants today (HUF + EUR); third-currency
 * widening is named-deferred per ADR-0037 §5 (operator-signs-a-customer
 * trigger). Wire form is the `rename_all = "UPPERCASE"` ISO 4217 string
 * — matches `Currency::iso_code()` on the Rust side. Pinned by
 * `invoice_list_item_emits_currency` +
 * `invoice_detail_emits_currency_and_rate_metadata` on the Rust side;
 * TS reads the wire shape strictly via this typed union so a backend
 * drift surfaces at `npm run check`. */
export type Currency = "HUF" | "EUR";

/** PR-73 / ADR-0040 §addendum — denormalized per-invoice bank-account
 * snapshot mirror of `serve::BankAccountSnapshotResponse`. Carried on
 * BOTH the list-row and the detail wire shape so a single TS interface
 * drives both surfaces. `null` for pre-PR-73 / CLI-issued invoices
 * (which have NULL across the five `bank_account_*` DuckDB columns).
 * The `InvoiceDetail.svelte` "Pay to" sub-section renders the snapshot;
 * the list view does not. */
export interface BankAccountSnapshot {
  /** `bnk_<26-char>` deterministic id from the seller-banks schema. */
  id: string;
  /** ISO 4217 string matching the invoice's currency. */
  currency: Currency;
  /** Account number string verbatim (IBAN form for EUR, domestic for HUF). */
  account_number: string;
  /** Operator-typed bank name (e.g., `"Erste Bank"`). */
  bank_name: string;
  /** SWIFT/BIC code (8 or 11 chars). */
  swift_bic: string;
}

/** Single invoice row — shape mirrors `serve::InvoiceListItem`. */
export interface InvoiceListItem {
  invoice_id: string;
  sequence_number: number;
  fiscal_year: number;
  state: InvoiceState;
  /** Units depend on `currency` per PR-44ε / session-53: for
   * `currency === "HUF"` this is whole forints (HUF has no sub-unit;
   * the `Huf` newtype stores it as `i64`). For `currency === "EUR"`
   * this is EUR cents (the issuance-path posture per PR-44γ stores
   * EUR amounts in the underlying `i64` as cents and re-uses the
   * `Huf` wrapper at the per-line layer until PR-44δ+1 lifts
   * `LineItem` to a typed-EUR shape). `null` while billing still has
   * the invoice as a draft (no totals persisted yet); the backend
   * serialises this as `null` from `Option<i64>`. The list-row
   * formatter in `format.ts` reads `currency` to pick HUF-vs-EUR
   * display. */
  total_gross: number | null;
  /** PR-31 / session-35 — chain-link affordance for list rows
   * (session-30-named Option M). `true` iff this invoice is the
   * base of at least one InvoiceStornoIssued or
   * InvoiceModificationIssued chain entry. The list-row renderer
   * surfaces a small `↘` badge next to the state chip when this
   * is true; the badge is non-interactive (the row click already
   * opens the detail modal). Pinned by
   * `list_invoices_emits_has_chain_children` on the Rust side; TS
   * reads the wire shape strictly via this typed field so a
   * backend drift surfaces at `npm run check`. */
  has_chain_children: boolean;
  /** ADR-0049 §Screen render (session 156) — `true` iff this row IS a
   * storno (the chain child). The backend stores the storno's
   * `total_gross` positive (negation lives only in the NAV-XML / PDF
   * path); the list-row total formatter negates the displayed value
   * when this is true so the screen matches the buyer-facing PDF
   * (`-127 000 Ft`). Derived from the ledger, NOT a DB column. Pinned
   * by `invoice-list-storno-negation.test.ts`. */
  is_storno: boolean;
  /** PR-44ε / session-53 — currency on the list-row wire shape per
   * ADR-0037 §1.a + §3. The list-row formatter consumes this
   * field to pick the HUF-vs-EUR symbol + minor-unit interpretation
   * for `total_gross`; without it, an EUR invoice's cents would
   * render as forints (off by a factor of 100 + wrong symbol).
   * Pinned by `invoice_list_item_emits_currency` on the Rust side. */
  currency: Currency;
  /** PR-65 / session-86 — buyer label for the SPA's list-row Partner
   * column (Tier-1 UX lift). Best-effort read of the PR-47α / A174
   * side-stored `<ULID>.input.json`'s `customer.name` field on the
   * Rust side; `null` for CLI-issued invoices, pre-PR-47α SPA-issued
   * invoices, or any I/O failure. The SPA renders the value via
   * `buyerColumnDisplay` in `./invoice-list.ts`, which falls back to
   * a quiet em-dash placeholder on `null` rather than fabricating a
   * label. Pinned by `invoice_list_item_emits_buyer_name` on the
   * Rust side; TS reads the wire shape strictly via this typed
   * field so a backend drift surfaces at `npm run check`. */
  buyer_name: string | null;
  /** PR-70 / ADR-0039 §2 — operational payment-receipt summary for
   * the SPA's "Paid" chip + quick-action gating. `null` for unpaid
   * invoices; the SPA renders no Paid badge + shows the "💰 Pay"
   * quick action on the row when the state is `Finalized`. Pinned
   * by `invoice_list_item_emits_payment` on the Rust side (PR-70
   * pin set); TS reads the wire shape strictly via this typed field
   * so a backend drift surfaces at `npm run check`. */
  payment: PaymentRecordSummary | null;
  /** PR-73 / ADR-0040 §addendum — denormalized bank-account snapshot.
   * `null` for pre-PR-73 / CLI-issued invoices. The list view does
   * not render this field today; it rides on the same wire shape as
   * `InvoiceDetail.bank_account` so a single TS interface drives both. */
  bank_account: BankAccountSnapshot | null;
  /** PR-213 / S215 — closed-vocab row discriminator for the unified
   * invoices list per ADR-0058. `"Own"` rows come from the canonical
   * `invoice` table (full audit-ledger lifecycle, every write-back
   * affordance applies); `"ExtNav"` rows come from the
   * `restored_invoice` NAV-mirror table (S180 — invoices NAV says
   * were issued under our `supplierTaxNumber` but that we did NOT
   * issue through ABERP, e.g. Billingo or manual). The SPA reads
   * this strictly to (a) render the Kind column and (b) hide every
   * write-back action (Submit / Storno / Pay) on ExtNav rows — they
   * are read-only by construction (we observe them; we do not own
   * them). Pinned by `invoice_list_item_emits_row_kind` on the
   * Rust side. */
  row_kind: RowKind;
  /** PR-213 / S215 — raw NAV-emitted `<invoiceNumber>` for ExtNav
   * rows; `null` for Own rows. The SPA surfaces this on the ExtNav
   * row as the operator-readable identity (the digest does not
   * carry a buyer name AND the `sequence_number` slot is meaningless
   * for non-ABERP invoices). For Own rows the operator already has
   * the composed `${fiscal_year}-${sequence_number}` identifier;
   * this field stays `null` there. */
  source_nav_invoice_number: string | null;
  /** S242 / PR-236 — canonical ISO-8601 `YYYY-MM-DD` issue date
   * (Hungarian `Kelt` / "Issued"), surfaced on the Outgoing tab's
   * "Issued" column. `null` ONLY for Draft rows (S236 — pre-issuance
   * `invoice_draft` entries that have no issue date by construction);
   * Own non-drafts and ExtNav rows always carry a date. The SPA's
   * column renderer treats `null` as an empty cell, NOT an em-dash
   * placeholder, per the PR-236 brief. Pinned by
   * `invoice_list_item_emits_issue_date` on the Rust side. */
  issue_date: string | null;
}

/** PR-213 / S215 — closed-vocab row discriminator for the unified
 * invoices list per ADR-0058. Wire form is the PascalCase string
 * the Rust `RowKind` serialises to. */
export type RowKind = "Own" | "ExtNav";

/** Possible derived states from `InvoiceTrace::derive_state` on the
 * backend. Kept in lockstep with that `&'static str` ladder per
 * ADR-0036 §2 — eleven labels, lifecycle-ordered. A state the
 * backend invents without a matching union member here renders as
 * the raw string but does not break the table; the `labelMeta`
 * helper in `./labels.ts` falls back to a muted "?" pill so the
 * silent miss is visible per CLAUDE.md rule 12. */
export type InvoiceState =
  // S236 / PR-230b — pre-allocation Draft state for `invoice_draft`
  // rows. Surfaced by the unified list when `RowKind::Own` rows come
  // from the new third source. Renders with the Draft chip from
  // labels.ts; NAV submit + PDF affordances disabled.
  | "Draft"
  | "Unknown"
  | "Ready"
  | "Pending"
  | "PendingNavExists"
  | "Submitted"
  | "Recovered"
  | "Finalized"
  | "Rejected"
  | "Storno"
  | "Amended"
  | "Abandoned";

/** One audit-ledger entry — shape mirrors `serve::AuditEntryView`. */
export interface AuditEntryView {
  seq: number;
  kind: string;
  actor: string;
  occurred_at: string;
  /** PR-26 / session-30 — chain-link affordance for the detail
   * modal. Non-null for `InvoiceStornoIssued` /
   * `InvoiceModificationIssued` entries (the typed payload's
   * `base_invoice_id` field per ADR-0023 / ADR-0024); `null` for
   * every other kind. `InvoiceDetail.svelte` renders the field as
   * a clickable navigation to the base invoice when present.
   * Pinned by `audit_view_of_emits_chain_base_invoice_id` on the
   * Rust side; TS reads the wire shape strictly via this typed
   * field so a backend drift surfaces at `npm run check`. */
  chain_base_invoice_id: string | null;
  /** PR-27 / session-31 — full typed payload as raw JSON
   * (whatever `audit_payloads::*` serialised). Rendered by
   * `InvoiceDetail.svelte` under a per-row expansion toggle as
   * pretty-printed JSON; the operator inspects every typed payload
   * field (chain digests, idempotency keys, NAV-emitted
   * timestamps, ack-status strings) without dumping the whole
   * bundle. `unknown` keeps the TS type honest — the shape varies
   * per `EventKind` and the renderer treats it as opaque JSON. A
   * malformed payload (which would indicate direct DB tampering)
   * serialises as `null` from the backend; the renderer prints
   * `null` rather than crashing the view. Pinned by
   * `audit_view_of_emits_typed_payload` on the Rust side. */
  payload: unknown;
}

/** PR-32 / session-36 — chain-children list entry. One per storno
 * / modification invoice issued against a base. The detail-modal
 * renderer lists these in a section between the meta-grid and the
 * audit-trail table; each `invoice_id` is a clickable affordance
 * that reuses the same `onNavigate` callback as the audit-row
 * chain-link button (PR-26). Pinned by
 * `invoice_detail_emits_chain_children` on the Rust side. */
export interface ChainChildView {
  kind: ChainChildKind;
  invoice_id: string;
  /** PR-41 / session-45 — per-base chain index allocated at issuance
   * time (`InvoiceStornoIssuedPayload.modification_index` /
   * `InvoiceModificationIssuedPayload.modification_index` on the
   * Rust side). Shared name space across both kinds: the next
   * storno or modification against the same base receives
   * `max(modification_index) + 1` per
   * `next_modification_index_in_tx` in `issue_storno.rs` /
   * `issue_modification.rs`. Operator-meaningful as the per-row
   * answer to "which entry in this base's chain?"; the
   * detail-modal renderer surfaces it as a leading `#N` glyph on
   * each chain-children row. Pinned by
   * `invoice_detail_emits_chain_children` on the Rust side; TS
   * reads the wire shape strictly via this typed field so a
   * backend drift surfaces at `npm run check`. */
  modification_index: number;
}

/** PR-32 / session-36 — typed kind discriminator for chain-children
 * rows. PascalCase wire mirror of the two terminal `InvoiceState`
 * labels (`Storno` / `Amended`); the SPA's `labels.ts` carries the
 * same labels at the state-chip layer, so a chain-children row
 * renders with the same affordance the operator already
 * recognises from the list-row chip.
 *
 * PR-37 / session-41 — tightened via `Extract<InvoiceState, ...>` so
 * the PR-34 `labelMeta(kind)` dispatch's `ChainChildKind ⊆ InvoiceState`
 * invariant is pinned at the type level. If a future ADR drops or
 * renames one of the two terminal labels in `InvoiceState`, this
 * alias degenerates (to `"Amended"`, `"Storno"`, or `never`) and
 * every consumer fails `npm run check` per CLAUDE.md rule 12 (fail
 * loud) rather than silently dispatching to the muted "?" fallback.
 * The runtime shape is byte-identical pre/post PR-37 — the Extract
 * evaluates to the same `"Storno" | "Amended"` union today; only the
 * type-level dependency on `InvoiceState` is new. */
export type ChainChildKind = Extract<InvoiceState, "Storno" | "Amended">;

/** PR-33 / session-37 — typed wire mirror for the four NAV v3.0
 * `processingResult` values (Option Q). Mirrors `serve::AckStatus`
 * under serde's `rename_all = "UPPERCASE"` so the wire form is the
 * verbatim NAV literal. Two intermediate values
 * (`RECEIVED`, `PROCESSING`) and two terminal (`SAVED`, `ABORTED`)
 * per ADR-0009 §2; the deprecated pre-v3.0 `DONE` value is NOT
 * represented — the NAV-transport inbound parser rejects it and the
 * audit-ledger never persists it. Pinned by
 * `ack_status_wire_shape_pins_uppercase_strings` on the Rust side;
 * TS reads the wire shape strictly via the
 * `last_ack_status: AckStatus | null` field on `InvoiceDetail` so a
 * backend drift surfaces at `npm run check`. */
export type AckStatus = "RECEIVED" | "PROCESSING" | "SAVED" | "ABORTED";

/** PR-70 / ADR-0039 §2 — typed wire mirror of the four closed-vocab
 * payment methods on `serve::PaymentMethod` (Rust enum). PascalCase
 * variant identifiers verbatim per CLAUDE.md rule 7 (closed-vocab).
 * Drift between this union and the Rust enum surfaces at three layers:
 * the Rust-side `payment_method_wire_shape_pins_pascalcase_strings`
 * test pins each variant's JSON form; the SPA's
 * `paymentMethodLabel` dispatch covers every variant via TypeScript's
 * exhaustive-match check; the route's `deserialize` fails loud on
 * unrecognised wire strings. */
export type PaymentMethod = "BankTransfer" | "Cash" | "Card" | "Other";

/** PR-70 / ADR-0039 §2 — typed wire mirror of the operational payment
 * record carried on `InvoiceListItem.payment` /
 * `InvoiceDetail.payment`. Mirrors `serve::PaymentRecordSummary` on
 * the Rust side; pinned to PaymentRecord drift via the Rust-side
 * round-trip tests in `audit_payloads.rs`. */
export interface PaymentRecordSummary {
  /** Operator-supplied payment date in canonical YYYY-MM-DD form. */
  paid_at: string;
  /** Amount paid in the invoice's stored minor-unit form (whole
   * forints for HUF, EUR cents for EUR). Mirrors the
   * `InvoiceListItem.total_gross` shape — divide by 100 on the EUR
   * branch for display. */
  amount_minor: number;
  /** ISO-4217 currency code matching the invoice's currency. */
  currency: string;
  /** Closed-vocab payment method per [`PaymentMethod`]. */
  method: PaymentMethod;
  /** Optional free-form operator note (bank transaction id, cheque
   * number, etc.). `null` when the operator left the field blank. */
  reference: string | null;
}

/** The single-invoice detail — shape mirrors
 * `serve::InvoiceDetailResponse`. */
export interface InvoiceDetail {
  invoice_id: string;
  sequence_number: number;
  fiscal_year: number;
  state: InvoiceState;
  total_gross: number | null;
  /** ADR-0049 §Screen render (session 156) — `true` iff this invoice IS
   * a storno (the chain child). The detail modal negates the displayed
   * total when true, matching the buyer-facing PDF. Mirrors
   * `InvoiceListItem.is_storno`; derived from the ledger, NOT a DB
   * column. */
  is_storno: boolean;
  audit_entries: AuditEntryView[];
  /** PR-32 / session-36 — chain-children list (Option T). For an
   * invoice that is the BASE of at least one chain entry, this
   * array enumerates every storno / modification invoice issued
   * against it, in ledger-walk (i.e., issuance) order. Empty for
   * invoices with no chain children (NOT null — the backend
   * always emits a JSON array). The detail-modal renderer
   * conditionally renders the section only when the array is
   * non-empty. Pinned by `invoice_detail_emits_chain_children` on
   * the Rust side; TS reads the wire shape strictly so a backend
   * drift surfaces at `npm run check`. */
  chain_children: ChainChildView[];
  /** PR-33 / session-37 — latest NAV ack for this invoice (Option Q).
   * `null` when no `InvoiceAckStatus` audit entry has been written
   * yet (Draft / Pending lifecycle states) OR when a persisted
   * string fails to parse as one of the four NAV v3.0 values (the
   * audit-entries drill-down still surfaces the raw string via
   * `payload`, so no information is lost). The detail-modal
   * renderer surfaces the value as a meta-grid row next to State /
   * Total (gross). Pinned by `invoice_detail_emits_last_ack_status`
   * on the Rust side; TS reads the wire shape strictly via this
   * typed field so a backend drift surfaces at `npm run check`. */
  last_ack_status: AckStatus | null;
  /** PR-44ε / session-53 — currency on the detail wire shape per
   * ADR-0037 §1.a + §3. Same union as `InvoiceListItem.currency`.
   * The detail-modal renderer reads this field to pick the
   * HUF-vs-EUR `total_gross` formatter AND to gate the conditional
   * render of the four rate-metadata rows below. Pinned by
   * `invoice_detail_emits_currency_and_rate_metadata` on the Rust
   * side. */
  currency: Currency;
  /** PR-44ε / session-53 — MNB exchange rate per ADR-0037 §1.a +
   * §1.c (rate value) / C11 (precision). Decimal-as-string at
   * exactly 6 decimal places (`"405.230000"`); `null` iff
   * `currency === "HUF"`. The detail-modal renderer surfaces the
   * value as a meta-grid row only when non-null per the
   * conditional-render shape pinned by the SPA vitest. */
  exchange_rate: string | null;
  /** PR-44ε / session-53 — MNB source identifier per ADR-0037 §1.a
   * (printed-invoice field) + §2.a (literal `"MNB"`). `null` iff
   * `currency === "HUF"`. */
  exchange_rate_source: string | null;
  /** PR-44ε / session-53 — MNB rate publication date per ADR-0037
   * §1.a + §2.b (walk-back rule). ISO-8601 `YYYY-MM-DD`; `null`
   * iff `currency === "HUF"`. */
  exchange_rate_date: string | null;
  /** PR-44ε / session-53 — HUF-equivalent gross total per ADR-0037
   * §1.a + §1.c / C5. Whole forints (HUF has no sub-unit); `null`
   * iff `currency === "HUF"`. */
  huf_equivalent_total: number | null;
  /** PR-70 / ADR-0039 §2 — operational payment summary mirror of
   * [`InvoiceListItem.payment`]. Same wire shape on both list and
   * detail surfaces so one TS interface drives the SPA's chip
   * rendering. `null` for unpaid invoices. */
  payment: PaymentRecordSummary | null;
  /** PR-73 / ADR-0040 §addendum — denormalized bank-account snapshot.
   * `null` for pre-PR-73 / CLI-issued invoices. The
   * `InvoiceDetail.svelte` "Pay to" sub-section renders this when
   * non-null; the renderer falls back to "(no bank account on file)"
   * on `null`. */
  bank_account: BankAccountSnapshot | null;
  /** PR-82 — buyer-facing per-invoice global note ("Megjegyzés").
   * `null` when the operator did not annotate the invoice at
   * issuance. The detail modal renders this in a "Megjegyzés"
   * section so the operator previews what the buyer will see on
   * the printed PDF. NEVER on the NAV XML wire. */
  invoice_note: string | null;
  /** PR-82 — buyer-facing per-line notes. Empty array when no
   * line carries a note. Each entry is keyed by the original
   * line's zero-based `ordinal` and carries the line description
   * + the note text. The detail modal renders this beneath the
   * global note so the operator sees "Line 1 (Widget A): ...". */
  line_notes: LineNoteView[];
  /** PR-99 Item 5 — the three operator-meaningful invoice dates,
   * canonical YYYY-MM-DD strings. `null` for pre-PR-84 invoices that
   * never recorded the columns; the detail modal renders an em-dash
   * in that case. For new-issuance invoices all three are populated. */
  issue_date: string | null;
  payment_deadline: string | null;
  delivery_date: string | null;
}

/** PR-82 — one row in the detail-modal's per-line note list.
 * Mirrors `serve::LineNoteView`. */
export interface LineNoteView {
  ordinal: number;
  description: string;
  note: string;
}

/** `GET /health` response — `serve::HealthResponse`. */
export interface HealthResponse {
  ok: boolean;
  binary_hash: string;
  nav_xsd_version: string;
  /** S165 — `true` when the backend was compiled `--features
   * production`. Drives the Tenant-Settings invoice-number preview's
   * `TEST-` prefix (shown on dev/test builds, dropped on production). */
  is_production_build: boolean;
  /** S166 — `true` on a production build whose one-time first-launch
   * ceremony has not yet been acknowledged. While true, the SPA blocks
   * its main routes behind the `FirstProdLaunchModal`. Always `false` on
   * dev/test builds. */
  first_prod_launch_required: boolean;
}

export async function health(): Promise<HealthResponse> {
  return invoke<HealthResponse>("health");
}

/** `POST /health/acknowledge-first-prod-launch` response. */
export interface AcknowledgeFirstProdLaunchResponse {
  acknowledged_at: string;
}

/** S166 — record the operator's one-time consent to real fiscal
 * operation. Writes the touchfile + a permanent audit entry on the
 * backend; after it resolves, a fresh `health()` reports
 * `first_prod_launch_required: false`. */
export async function acknowledgeFirstProdLaunch(): Promise<AcknowledgeFirstProdLaunchResponse> {
  return invoke<AcknowledgeFirstProdLaunchResponse>("acknowledge_first_prod_launch");
}

export async function listInvoices(): Promise<InvoiceListItem[]> {
  return invoke<InvoiceListItem[]>("list_invoices");
}

/** S236 / PR-230b — pre-allocation invoice-draft wire shape. Mirrors
 * `apps/aberp/src/invoice_draft.rs::InvoiceDraft`. Surfaced in the
 * unified list as `state: "Draft"` and exposed standalone for the
 * detail / delete actions. */
export interface InvoiceDraftView {
  drf_id: string;
  tenant_id: string;
  partner_id: string;
  source_dispatch_id: string | null;
  source_wo_id: string | null;
  product_id: string;
  qty: string;
  notes: string | null;
  created_at: string;
}

export async function listInvoiceDrafts(): Promise<InvoiceDraftView[]> {
  return invoke<InvoiceDraftView[]>("list_invoice_drafts");
}

export async function getInvoiceDraft(drfId: string): Promise<InvoiceDraftView> {
  return invoke<InvoiceDraftView>("get_invoice_draft", { drfId });
}

export async function deleteInvoiceDraft(drfId: string): Promise<void> {
  return invoke<void>("delete_invoice_draft", { drfId });
}

export async function getInvoice(invoiceId: string): Promise<InvoiceDetail> {
  return invoke<InvoiceDetail>("get_invoice", { invoiceId });
}

export async function getAudit(invoiceId: string): Promise<AuditEntryView[]> {
  return invoke<AuditEntryView[]>("get_audit", { invoiceId });
}

/** PR-44ε.UI / session-58 — download the printed-invoice PDF as a
 * Blob suitable for triggering a browser save. The backend Tauri
 * command (`download_invoice_pdf`) wraps the loopback
 * `GET /invoices/<id>/pdf` route which streams `application/pdf`
 * bytes from `print_invoice::render_to_bytes`. The bytes cross the
 * Tauri boundary as a `Vec<u8>` (decoded SPA-side from Tauri's
 * default `Array<number>` shape into a `Uint8Array` for the
 * `Blob` constructor).
 *
 * Returns a `Blob` with MIME type `application/pdf`; the caller
 * (`InvoiceDetail.svelte`) uses `URL.createObjectURL` + a synthetic
 * `<a download>` click to surface the browser-native save dialog.
 * Errors propagate as the rejected promise per the existing
 * `getInvoice` / `getAudit` posture — the caller renders the
 * message inline. */
export async function downloadInvoicePdf(invoiceId: string): Promise<Blob> {
  const bytes = await invoke<number[]>("download_invoice_pdf", { invoiceId });
  return new Blob([new Uint8Array(bytes)], { type: "application/pdf" });
}

/** PR-44ζ / session-59 — wire request body for `POST /invoices/issue`.
 * Mirrors `serve::IssueInvoiceRequest` on the backend. The form-to-
 * body composer in `./issue-invoice.ts` turns the SPA form state into
 * this shape; pinned by `issue-invoice.test.ts`.
 *
 * PR-53 / session-73 — `supplier` removed from the wire shape. The
 * backend now reads seller identity from the per-tenant
 * `~/.aberp/<tenant>/seller.toml` (populated by the
 * SellerConfigWizard, PR-51). The Issue form no longer surfaces
 * seller inputs; the cross-cutting fix per Ervin's feedback that the
 * post-tenant-setup form was re-asking for already-saved values. */
/** PR-77 / session-101 — customer address sub-shape on the wire body.
 * Mirrors backend `issue_invoice::AddressJson` (camelCase via serde
 * rename). Required whenever the backend treats the buyer as a
 * Hungarian business (today: any well-formed tax number triggers the
 * DOMESTIC customerVatStatus path); preflight fires
 * `CustomerAddressMissing` when absent. The SPA's
 * `composeIssueInvoiceBody` reads `IssueInvoiceFormState`'s
 * customer-address fields and emits this block; the IssueInvoice form
 * itself populates the fields from the operator-selected partner via
 * `buyerFieldsFromPartner`. Country is `HU` for every supported buyer
 * today; closed-vocab country + non-Hungarian buyer support are
 * named-deferred. */
export interface CustomerAddressBody {
  countryCode: string;
  postalCode: string;
  city: string;
  street: string;
}

/** PR-97 / ADR-0048 — closed-vocab buyer-kind discriminator wire mirror.
 * Mirrors backend `nav_xml::CustomerVatStatus` (serde PascalCase). v1
 * ships `Domestic` + `PrivatePerson`; `Other` is named-deferred per
 * ADR-0048 §7 (the SPA disables the Külföldi radio option with a v2
 * hint, and the backend's preflight loud-fails an Other body with
 * `CustomerVatStatusOtherNotSupportedV1`). */
export type CustomerVatStatusBody = "Domestic" | "PrivatePerson" | "Other";

export interface IssueInvoiceRequest {
  customer: {
    /** PR-97 / ADR-0048 — closed-vocab buyer kind. Optional on the
     * wire so pre-PR-97 callers (CLI / fixtures) still type-check;
     * backend serde defaults to `"Domestic"` when absent, preserving
     * the pre-PR-97 implicit posture. */
    vatStatus?: CustomerVatStatusBody;
    /** PR-97 / ADR-0048 (Ervin override 1) — saved-partner id when
     * the operator picked a buyer via the typeahead. `null` (or
     * absent) for one-off buyers and CLI callers. When provided, the
     * backend increments `partners.issued_invoice_count` in the same
     * tx, which flips `has_issued_invoices` true and locks
     * `tax_number` + `customer_vat_status` in the PartnerForm. */
    partnerId?: string | null;
    /** PR-97 / ADR-0048 — empty string for `PrivatePerson` buyers
     * (the SPA's disabled-input emits `""` verbatim); well-formed
     * `xxxxxxxx-y-zz` for `Domestic`. Held as `string` (not
     * `string | null`) for wire-compat with pre-PR-97 fixtures. */
    taxNumber: string;
    name: string;
    /** PR-77 / session-101 — full customer address; required for any
     * Hungarian-business buyer (the DOMESTIC customerVatStatus branch).
     * PR-97 / ADR-0048 — optional under PrivatePerson (NAV wire layer
     * permits absence; printed-PDF rule lives at the render boundary).
     * Optional on the TS surface so pre-PR-77 callers still type-check;
     * the backend's preflight rejects an absent or partially-blank
     * address only when the buyer is Domestic. */
    address?: CustomerAddressBody;
  };
  lines: Array<{
    description: string;
    /** S157 — canonical dot-decimal quantity string (e.g. `"1.5"`). The
     * backend's `LineJson.quantity: Decimal` accepts this string (C11
     * Decimal-as-string wire convention, as `exchange_rate` uses). */
    quantity: string;
    unitPrice: number;
    vatRatePercent: number;
    /** PR-82 — buyer-facing per-line note ("Megjegyzés"). Optional;
     * the SPA emits `null` for unannotated lines so the backend
     * sees a clean "no note" signal. NEVER reaches the NAV
     * InvoiceData XML — recipient-facing only. */
    note?: string | null;
    /** S159 — the line's unit of measure, stamped from the picked
     * product (PR-100 picker). `null` for one-off freetext lines the
     * operator typed without picking a product; the backend's emit
     * falls back to `<unitOfMeasure>PIECE</...>`. A `Nav` token emits
     * that token; an `Own` label emits `OWN` + `<unitOfMeasureOwn>`.
     * Wire form is the Rust internally-tagged serde shape — see
     * [`ProductUnit`]. */
    unit?: ProductUnit | null;
  }>;
  currency: Currency;
  /** Optional series code; backend defaults to `"INV-default"` when
   * omitted. Kept opt-in so the SPA form does not have to expose a
   * series-picker on the first cut. */
  series?: string;
  /** PR-73 / ADR-0040 §addendum — operator-selected bank account id
   * (the `bnk_<26-char>` deterministic value from `listSellerBanks`).
   * `null` (or absent) → backend falls back to the per-currency
   * default. The SPA's bank picker defaults to the currency's
   * `is_default: true` entry and lets the operator switch to any
   * other entry for that currency. */
  bankAccountId?: string | null;
  /** PR-82 — buyer-facing per-invoice global note ("Megjegyzés").
   * Optional; `null` when the operator left the textarea blank. The
   * backend persists it on `invoice.invoice_note` and stamps it on
   * the audit payload; the printed PDF + SPA detail view render it
   * for buyer + operator preview. NEVER on the NAV XML wire. */
  invoiceNote?: string | null;
  /** PR-84 — operator-supplied payment deadline (Fizetési határidő),
   * canonical `YYYY-MM-DD`. Resolved absolute date from the form's
   * bidirectional offset/absolute pair. Optional on the wire so
   * pre-PR-84 callers keep type-checking; the backend defaults to the
   * server-stamped issue date when absent. */
  paymentDeadline?: string | null;
  /** PR-84 — operator-supplied delivery / fulfillment date
   * (Teljesítési dátum), canonical `YYYY-MM-DD`. REGULATORY: this is
   * what the NAV emit writes as `<invoiceDeliveryDate>`. Optional on
   * the wire for the same pre-PR-84 back-compat reason. */
  deliveryDate?: string | null;
  /** PR-84 — audit discriminant for the delivery-date choice's
   * comfort-zone classification. `null` for in-range (default operator
   * path, no audit flag); `"BeforeInvoiceDate"` /
   * `"AfterPaymentDeadline"` carry the operator's confirmed out-of-
   * range choice verbatim. The backend persists this on the
   * `InvoiceDraftCreated` audit payload so the tamper-evident
   * regulatory trail records every override. */
  deliveryDateOverride?: "BeforeInvoiceDate" | "AfterPaymentDeadline" | null;
  /** PR-92 / ADR-0047 — operator's per-invoice opt-out of the
   * default-on auto-send-to-buyer. The SPA's IssueInvoice form renders
   * a checkbox defaulted to `true` so silence-by-omission can never
   * suppress a send. Operator flips it `false` to suppress this
   * invoice's auto-send; the manual send button on InvoiceDetail
   * stays available either way. Optional on the wire; the backend
   * defaults to `true` when absent. */
  emailBuyerOnIssue?: boolean | null;
  /** PR-99 Item 4 Part B — operator's per-invoice opt-out of the
   * default-on auto-submit-to-NAV-on-issue. Mirrors the email toggle's
   * semantics: bound to a default-`true` checkbox on the IssueInvoice
   * form so silence-by-omission lands every invoice with NAV inside the
   * same operator session that issued it. When `true` AND issuance
   * succeeds, the backend fires the same path the manual `POST
   * /api/invoices/:id/submit` route hits (no body bypass; identical
   * audit-ledger footprint). When `false` the operator handles submit
   * manually from InvoiceDetail later. Optional on the wire; absent
   * defaults to `true`. */
  submitToNavOnIssue?: boolean | null;
  /** S160 / ADR-0050 — operator-selected payment method (Fizetési mód).
   * Closed-vocab NAV `paymentMethodType` mirror; the wire value is the
   * bare NAV token (`"TRANSFER"`, `"CASH"`, …). Optional on the wire so
   * pre-S160 callers / CLI still type-check; the backend's
   * `#[serde(default)]` resolves an absent value to `"TRANSFER"`,
   * preserving the pre-S160 hardcoded emit. See [`InvoicePaymentMethod`]. */
  paymentMethod?: InvoicePaymentMethod;
  /** PR-203 / S203 — operator-typed per-invoice email recipient override
   * ("Email-címzett(ek)"). Comma-separated address list (the canonical
   * `", "` shape the backend already emits for `partner.contact_email`).
   * `null` / absent → the send-path resolver falls back to the partner
   * master record's `contact_email`. Validated server-side at
   * `validate_issue_request`; a malformed value surfaces as 400 BEFORE
   * the issuance commits. Editing this field NEVER writes back to the
   * partner master — it is a one-off per-invoice override. */
  emailRecipientOverride?: string | null;
}

/** S160 / ADR-0050 — per-invoice payment method (Fizetési mód). Mirror
 * of the Rust `aberp_billing::PaymentMethod` closed-vocab enum, whose
 * `#[serde(rename_all = "SCREAMING_SNAKE_CASE")]` puts the bare NAV token
 * on the wire. Distinct from the operational mark-as-paid
 * [`PaymentMethod`] (PR-70 / ADR-0039) — that records HOW a payment was
 * received after issuance; THIS is the NAV `<paymentMethod>` snapshot on
 * the invoice itself. NAV's `paymentMethodType` is closed with NO
 * free-text companion, so `"OTHER"` (Egyéb) is the catch-all — there is
 * no `paymentMethodOwn`. The label table lives in `payment-method.ts`. */
export type InvoicePaymentMethod =
  | "TRANSFER"
  | "CASH"
  | "CARD"
  | "VOUCHER"
  | "OTHER";

/** PR-92 / ADR-0047 — wire shape for the per-invoice email send
 * outcome, surfaced on both the issue response (auto-send) and the
 * manual `POST /api/invoices/:id/email` response. */
export interface EmailRouteOutcome {
  /** Closed-vocab: `"succeeded"` | `"failed"`. */
  outcome: "succeeded" | "failed";
  /** Recipient address actually used (or attempted). */
  recipient: string;
  /** Closed-vocab error class on failure; absent on success. */
  error_class?:
    | "transport"
    | "tls"
    | "auth"
    | "recipient_rejected"
    | "compose"
    | "other";
  /** Operator-readable detail on failure; absent on success. */
  error_detail?: string;
  /** `true` for auto-send-after-issue; `false` for manual button. */
  auto: boolean;
  /** `true` iff the NAV XML rode alongside the PDF. */
  attached_xml: boolean;
}

/** PR-44ζ / session-59 — wire response body for `POST /invoices/issue`.
 * Mirrors `serve::IssueInvoiceResponse` on the backend. The SPA reads
 * `invoice_id` to open the detail modal at the just-issued invoice. */
export interface IssueInvoiceResponse {
  invoice_id: string;
  invoice_number: string;
  state: InvoiceState;
  /** PR-92 — outcome of the default-on auto-send. Present iff the
   * operator left the toggle on; absent when toggled off. */
  email?: EmailRouteOutcome;
}

/** PR-44ζ / session-59 — POST the SPA-composed request body to the
 * backend's `/invoices/issue` route via the matching Tauri command.
 * Errors propagate as the rejected promise; the caller renders the
 * message inline (no toast component per A157 precedent). */
export async function issueInvoice(
  body: IssueInvoiceRequest,
): Promise<IssueInvoiceResponse> {
  return invoke<IssueInvoiceResponse>("issue_invoice", { body });
}

/** PR-44η / session-60 — wire response body for
 * `POST /invoices/<id>/submit`. Mirrors `serve::SubmitInvoiceResponse`.
 * The SPA reads `transaction_id` to flash a success state and `state`
 * to flip the chip without an extra `getInvoice` roundtrip. */
export interface SubmitInvoiceResponse {
  invoice_id: string;
  transaction_id: string;
  state: InvoiceState;
  entries_verified: number;
}

/** PR-44η / session-60 — wire response body for
 * `POST /invoices/<id>/poll-ack`. Mirrors `serve::PollAckResponse`.
 * `state` reflects the terminal lifecycle label (`Finalized` /
 * `Rejected` on a clean terminus; `Submitted` when the loop hit a
 * stuck variant — the operator-visible reason is in `diagnostic`).
 * `attempts_made` lets the SPA render "after N attempts" verbatim. */
export interface PollAckResponse {
  invoice_id: string;
  state: InvoiceState;
  attempts_made: number;
  transaction_id: string;
  diagnostic: string | null;
  entries_verified: number;
}

/** PR-44η / session-60 — POST the SPA's "Submit to NAV" button to
 * the backend's `/invoices/<id>/submit` route via the matching Tauri
 * command. No body — the backend resolves the on-disk NAV XML +
 * supplier tax number from the audit ledger server-side. Errors
 * propagate as the rejected promise (including the typed 409 body
 * for precondition mismatch); the caller renders the message inline
 * (no toast component per A157). */
export async function submitInvoice(
  invoiceId: string,
): Promise<SubmitInvoiceResponse> {
  return invoke<SubmitInvoiceResponse>("submit_invoice_to_nav", {
    invoiceId,
  });
}

/** PR-59 / session-79 — one parsed `<technicalValidationMessages>` block
 * from NAV's `GeneralErrorResponse`. NAV emits one of these per validation
 * rule that fired against the request; a 400 typically carries 3-10. The
 * shape mirrors NAV's OSA 3.0 schema:
 *
 *   - `result_code` — `<validationResultCode>`: `"ERROR"` or `"WARN"`.
 *   - `error_code`  — `<validationErrorCode>`: machine-readable code.
 *   - `message`     — Hungarian-localized human description.
 *   - `tag`         — XPath / element name the rule fired on.
 *
 * Each field is nullable because NAV occasionally omits fields for
 * envelope-level rules. Mirrors `serve::TechnicalValidationBody`. */
export interface NavTechnicalValidation {
  result_code: string | null;
  error_code: string | null;
  message: string | null;
  tag: string | null;
}

/** PR-58 / session-78 — typed shape for the backend's
 * `nav_upstream_fault` JSON body (HTTP 502). Returned by
 * `POST /invoices/:id/submit` when NAV's `tokenExchange` rejects the
 * request at the HTTP layer (signature mismatch, IP not whitelisted,
 * expired technical-user password, etc.). The `fault_code` /
 * `fault_message` pair is NAV's parsed top-level diagnostic (Hungarian-
 * localized message when present); `raw_body_preview` is a prefix of
 * the verbatim response body as a fallback when parsing did not find a
 * typed pair. Mirrors `serve::NavUpstreamFaultBody`.
 *
 * PR-59 / session-79 — added `technical_validations`. For NAV's most
 * common 400 (`fault_code=INVALID_REQUEST`) the top-level wrapper is
 * generic; the per-rule diagnostic NAV actually emits lives inside the
 * `<technicalValidationMessages>` array. The SPA's invoice-detail
 * modal renders the list below the top-level fault headline. */
export interface NavUpstreamFault {
  error: "nav_upstream_fault";
  status: number;
  fault_code: string | null;
  fault_message: string | null;
  technical_validations: NavTechnicalValidation[];
  raw_body_preview: string;
}

/** PR-58 / session-78 — best-effort extract a [`NavUpstreamFault`] from
 * the error string the Tauri forwarder produces on a non-2xx response.
 * `forward_post` stringifies non-success responses as
 * `"backend returned <status> for <path>: <body>"`; this helper finds
 * the JSON tail, parses it, and returns the typed shape only if the
 * `error` discriminator is `"nav_upstream_fault"`. Returns `null` for
 * everything else (plain 4xx error_body, network failure, non-JSON
 * trailer) so the caller can fall through to its existing string-
 * rendering path. */
export function parseNavUpstreamFault(message: string): NavUpstreamFault | null {
  const brace = message.indexOf("{");
  if (brace < 0) return null;
  try {
    const parsed: unknown = JSON.parse(message.slice(brace));
    if (
      typeof parsed === "object" &&
      parsed !== null &&
      (parsed as { error?: unknown }).error === "nav_upstream_fault"
    ) {
      const fault = parsed as NavUpstreamFault;
      // PR-59 / session-79 — the backend sends an empty array for the
      // no-technical-validations case, but a pre-PR-59 backend (or a
      // future schema regression) might omit the field entirely.
      // Normalise to `[]` so consumers can iterate without null-checks.
      if (!Array.isArray(fault.technical_validations)) {
        fault.technical_validations = [];
      }
      return fault;
    }
  } catch {
    // Not JSON — caller renders the raw string.
  }
  return null;
}

/** PR-44η / session-60 — POST the SPA's "Poll ack now" button to the
 * backend's `/invoices/<id>/poll-ack` route via the matching Tauri
 * command. No body — the backend resolves the NAV transactionId
 * from the audit ledger server-side. Errors propagate as the
 * rejected promise; the caller renders the message inline. */
export async function pollAck(invoiceId: string): Promise<PollAckResponse> {
  return invoke<PollAckResponse>("poll_ack", { invoiceId });
}

/** PR-47α / session-64 — wire response body for
 * `POST /api/invoices/<id>/storno`. Mirrors `serve::StornoInvoiceResponse`.
 * `invoice_id` + `invoice_number` identify the NEW storno (the operator
 * already has the base in the modal); `state` is the BASE's new state
 * after this route — always `Storno` per ADR-0036 §3. */
export interface StornoInvoiceResponse {
  invoice_id: string;
  invoice_number: string;
  state: InvoiceState;
  modification_index: number;
  entries_verified: number;
}

/** PR-83 — wire request body for `POST /api/invoices/<id>/storno`.
 * Mirrors `serve::StornoInvoiceRequest`. The optional
 * `stornoReason` is the buyer-facing "Sztornó indoka / Storno reason"
 * the operator types into the inline storno confirm panel — it
 * persists onto the storno's own `invoice_note` column and renders on
 * the printed PDF / future email body. NEVER carried into the NAV
 * InvoiceData XML — recipient-facing only. */
export interface StornoInvoiceRequest {
  /** Operator-typed buyer-facing reason for the cancellation. `null`
   * when the operator did not type one — pre-PR-83 callers and the
   * "leave blank" case both wire as `null`. The backend trims +
   * normalises empty-after-trim to `null` as a single rule shared
   * with PR-82's `blankToNull` normalisation. */
  stornoReason: string | null;
}

/** PR-47α / session-64 — POST the SPA's "Cancel invoice (storno)"
 * button to the backend's `/api/invoices/<id>/storno` route via the
 * matching Tauri command. The backend resolves the operator's
 * original invoice JSON from the side-stored input.json file written
 * at issuance time per A174.
 *
 * PR-83 — the body now carries an optional buyer-facing storno
 * reason. Callers that pass an empty body OR `{ stornoReason: null }`
 * preserve the pre-PR-83 behaviour (no buyer note); a non-null
 * `stornoReason` lands on the storno's `invoice_note` column and
 * surfaces on the printed PDF.
 *
 * Errors propagate as the rejected promise (including the typed 409
 * body for precondition mismatch); the caller renders the message
 * inline (no toast component per A157). */
export async function cancelInvoiceStorno(
  invoiceId: string,
  body: StornoInvoiceRequest = { stornoReason: null },
): Promise<StornoInvoiceResponse> {
  return invoke<StornoInvoiceResponse>("cancel_invoice_storno", {
    invoiceId,
    body,
  });
}

/** PR-47β / session-65 — wire request body for
 * `POST /api/invoices/<id>/modification`. Mirrors
 * `serve::ModificationInvoiceRequest`. Shape is the
 * [`IssueInvoiceRequest`] fields plus an operator-supplied
 * `modificationDate` per ADR-0024 §1 (canonical `YYYY-MM-DD`; no
 * silent today-default). The `currency` MUST match the base invoice's
 * stored currency per ADR-0037 §4 invariant C6 — the SPA's form locks
 * the dropdown to the base's currency; the backend additionally
 * enforces a 400 if the body's currency differs (defence in depth
 * against a curl bypass). */
export interface ModificationInvoiceRequest {
  customer: {
    /** PR-97 / ADR-0048 — same `vatStatus` posture as the fresh
     * issuance path. The modification's customer block is full-replace
     * per ADR-0024 §4 and inherits the base invoice's buyer kind. */
    vatStatus?: CustomerVatStatusBody;
    taxNumber: string;
    name: string;
    /** PR-77 / session-101 — same address surface as
     * [`IssueInvoiceRequest.customer.address`]. The modification's
     * customer block is full-replace per ADR-0024 §4 and the address
     * field is required for any Hungarian-business buyer. */
    address?: CustomerAddressBody;
  };
  lines: Array<{
    description: string;
    /** S157 — canonical dot-decimal quantity string (e.g. `"1.5"`); see
     * [`IssueInvoiceRequest.lines`]. */
    quantity: string;
    unitPrice: number;
    vatRatePercent: number;
  }>;
  currency: Currency;
  /** ADR-0024 §1 — operator-supplied `YYYY-MM-DD`. Frozen onto the
   * `InvoiceModificationIssued` audit payload at issuance time. */
  modificationDate: string;
  /** Optional series code; backend defaults to `"INV-default"` when
   * omitted. Same posture as [`IssueInvoiceRequest.series`]. */
  series?: string;
  /** PR-203 / S203 — per-modification email recipient override. The
   * SPA's modification form pre-fills from the base's stored value (via
   * `GET /issuance-input`) but the operator can edit per-modification.
   * `null` / absent → resolver falls back to the partner master record.
   * Same wire shape as [`IssueInvoiceRequest.emailRecipientOverride`]. */
  emailRecipientOverride?: string | null;
}

/** PR-47β / session-65 — wire response body for
 * `POST /api/invoices/<id>/modification`. Mirrors
 * `serve::ModificationInvoiceResponse`. `invoice_id` + `invoice_number`
 * identify the NEW modification; `state` is the BASE's new state
 * after this route — always `Amended` per ADR-0036 §3. */
export interface ModificationInvoiceResponse {
  invoice_id: string;
  invoice_number: string;
  state: InvoiceState;
  modification_index: number;
  entries_verified: number;
}

/** PR-47β / session-65 — POST the SPA's "Amend invoice (modification)"
 * button to the backend's `/api/invoices/<id>/modification` route via
 * the matching Tauri command. Unlike storno, the body IS operator-
 * edited (full corrected invoice content + `modificationDate`).
 * Errors propagate as the rejected promise (including the typed 400
 * body for C6 currency mismatch and the typed 409 for precondition
 * mismatch); the caller renders the message inline per A157. */
export async function amendInvoiceModification(
  invoiceId: string,
  body: ModificationInvoiceRequest,
): Promise<ModificationInvoiceResponse> {
  return invoke<ModificationInvoiceResponse>("amend_invoice_modification", {
    invoiceId,
    body,
  });
}

/** PR-70 / ADR-0039 — wire request body for
 * `POST /api/invoices/<id>/mark-paid`. Mirrors
 * `serve::MarkPaidRequest` on the backend. `currency` MUST match the
 * invoice's stored currency per ADR-0039 §3; the SPA pre-locks the
 * form's currency display to the invoice's currency and the backend
 * additionally rejects with 400 on mismatch as defence-in-depth. */
export interface MarkPaidRequest {
  /** Operator-supplied payment date — canonical YYYY-MM-DD. Defaults
   * to today on the SPA form; the backend additionally validates the
   * string with `time::Date::parse` and rejects with 400 on malformed
   * input per CLAUDE.md rule 12. */
  paid_at: string;
  /** Amount paid in the invoice's stored minor-unit form. Defaults
   * to the invoice's `total_gross` on the SPA form; the operator
   * may override for partial-payment-recorded-as-full edge cases
   * (v1 records the operator-supplied amount verbatim — partial
   * payments as a typed lifecycle are out of scope per the session-92
   * brief). */
  amount_minor: number;
  /** Must equal the invoice's stored currency. */
  currency: Currency;
  /** Closed-vocab payment method. */
  method: PaymentMethod;
  /** Optional free-form operator reference (bank txn id, cheque
   * number, etc.). Empty / whitespace-only is normalised to `null`
   * server-side. */
  reference?: string | null;
}

/** PR-70 / ADR-0039 — wire response body for
 * `POST /api/invoices/<id>/mark-paid` on the success path. */
export interface MarkPaidResponse {
  invoice_id: string;
  /** The just-appended payment record echoed back so the SPA can
   * render the Paid chip + detail immediately without a follow-up
   * `getInvoice` round-trip. */
  payment: PaymentRecordSummary;
  entries_verified: number;
}

/** PR-70 / ADR-0039 — wire response body for the `409 Conflict`
 * already-paid arm. Carries the existing payment record verbatim
 * so the SPA can render "this invoice was already paid on X by Y"
 * inline rather than surfacing a generic conflict. The
 * `parseAlreadyPaidError` helper below extracts this shape from
 * the Tauri forwarder's stringified error message. */
export interface AlreadyPaidErrorBody {
  error: "already_paid";
  message: string;
  payment: PaymentRecordSummary;
}

/** PR-70 / ADR-0039 — best-effort extract an [`AlreadyPaidErrorBody`]
 * from the error string the Tauri forwarder produces on a non-2xx
 * response. Returns the typed body only if the `error` discriminator
 * is `"already_paid"`; returns `null` for everything else so the
 * caller can fall through to its generic-error-rendering path. Same
 * posture as `parseNavUpstreamFault` above. */
export function parseAlreadyPaidError(
  message: string,
): AlreadyPaidErrorBody | null {
  const brace = message.indexOf("{");
  if (brace < 0) return null;
  try {
    const parsed: unknown = JSON.parse(message.slice(brace));
    if (
      typeof parsed === "object" &&
      parsed !== null &&
      (parsed as { error?: unknown }).error === "already_paid"
    ) {
      return parsed as AlreadyPaidErrorBody;
    }
  } catch {
    // Not JSON — caller renders the raw string.
  }
  return null;
}

/** PR-70 / ADR-0039 — POST the SPA's "Mark as paid" button to the
 * backend's `/api/invoices/<id>/mark-paid` route via the matching
 * Tauri command. Errors propagate as the rejected promise (including
 * the typed 409 body for already-paid and the typed 400 for
 * currency-mismatch / invalid-date); the caller renders the message
 * inline per A157. */
export async function markInvoicePaid(
  invoiceId: string,
  body: MarkPaidRequest,
): Promise<MarkPaidResponse> {
  return invoke<MarkPaidResponse>("mark_invoice_paid", {
    invoiceId,
    body,
  });
}

/** PR-47β / session-65 — GET the operator's original
 * [`IssueInvoiceRequest`]-shaped body side-stored at issuance time
 * (per A174). The SPA's modification modal calls this on open to
 * pre-fill its form so the operator edits in place rather than
 * retyping the entire invoice. On 404 (CLI-issued invoice or
 * pre-PR-47α SPA-issued) the promise rejects with the backend's
 * loud-fail message; the caller catches and falls back to an empty
 * form with an explanatory banner. */
export async function getIssuanceInput(
  invoiceId: string,
): Promise<IssueInvoiceRequest> {
  return invoke<IssueInvoiceRequest>("get_issuance_input", { invoiceId });
}

/** PR-45a / session-61 — boot lifecycle status the Tauri shell
 * exposes so the SPA can render a loading / error pane instead of
 * sitting blank while `aberp serve` cold-boots. PR-46α / session-62
 * extended the union with `"needs-setup"` for the first-run
 * NAV-credentials wizard. Four states:
 *
 *   - `"starting"`: the backend subprocess is mid-spawn / mid-
 *     handshake. SPA renders the loading pane with recent log lines.
 *   - `"needs-setup"`: the backend's handshake reported an empty
 *     keychain. SPA renders the first-run setup wizard (four-field
 *     form → POST /api/setup-nav-credentials → flip to ready).
 *   - `"ready"`: the handshake parsed, the backend is reachable. SPA
 *     mounts its normal screens.
 *   - `"failed"`: boot errored out. `error` carries the message;
 *     SPA renders the error pane with a Retry button.
 *
 * Wire form is the lower-case string emitted by
 * `aberp-ui::commands::get_boot_status` on the Rust side; the SPA
 * reads it strictly via this typed union so a backend drift surfaces
 * at `npm run check`. */
export type BootStatus =
  | "starting"
  | "needs-setup"
  | "needs-seller-config"
  | "ready"
  | "failed";

/** PR-45a / session-61 — boot lifecycle snapshot, mirrors the Tauri
 * shell's `get_boot_status` JSON body. `error` is `null` unless
 * `status === "failed"`. `recent_logs` is the bounded ring buffer
 * of backend stderr lines (oldest first; capped at 20 entries on
 * the Rust side via `RECENT_LOGS_CAP`). */
export interface BootStatusResponse {
  status: BootStatus;
  error: string | null;
  recent_logs: string[];
}

/** PR-45a / session-61 — read the boot lifecycle snapshot. The SPA
 * polls this until `status !== "starting"`. */
export async function getBootStatus(): Promise<BootStatusResponse> {
  return invoke<BootStatusResponse>("get_boot_status");
}

/** PR-45a / session-61 — re-invoke `boot_backend` after a Failed
 * boot. The Retry button on the SPA's error pane calls this. The
 * command returns immediately; the SPA continues polling
 * `getBootStatus` and re-renders against the lifecycle that follows. */
export async function retryBoot(): Promise<void> {
  await invoke<void>("retry_boot");
}

/** PR-46α / session-62 — wire request body for the first-run setup
 * wizard. Mirrors the Rust-side
 * `serve::SetupNavCredentialsRequest` (snake_case JSON fields). The
 * SPA composer in `./setup-credentials.ts` builds this shape from
 * the four form fields. */
export interface SetupNavCredentialsRequest {
  technical_user_login: string;
  technical_user_password: string;
  xml_sign_key: string;
  xml_change_key: string;
}

/** PR-46α / session-62 — wire response body for the setup route on
 * the happy path. The Rust side returns `{ "state": "ready" }`; the
 * SPA reads this to confirm the keychain write landed before
 * re-rendering. */
export interface SetupNavCredentialsResponse {
  state: "ready";
}

/** PR-46α / session-62 — POST the SPA's first-run wizard form to the
 * backend's `/api/setup-nav-credentials` route via the matching
 * Tauri command. On success the backend has written all four
 * credential entries to the OS keychain AND flipped its boot state
 * to Ready (or to NeedsSellerConfig if seller.toml is still missing
 * — PR-51 / session-71 chained-wizard posture); the Tauri shell
 * mirrors that transition. Errors propagate as the rejected promise
 * (the typed 400 validation body surfaces verbatim so the SPA renders
 * the operator-actionable inline message per A157). */
export async function setupNavCredentials(
  body: SetupNavCredentialsRequest,
): Promise<SetupNavCredentialsResponse> {
  return invoke<SetupNavCredentialsResponse>("setup_nav_credentials", { body });
}

/** PR-51 / session-71 — wire request body for the seller-config
 * wizard. Mirror of the Rust-side `serve::SetupSellerInfoRequest`.
 * Address + optional bank as flat sub-objects so the SPA's form
 * state maps 1:1 to the wire shape with no per-field renaming. */
export interface SetupSellerInfoRequest {
  legal_name: string;
  tax_number: string;
  eu_vat_number: string | null;
  address: {
    country_code: string;
    postal_code: string;
    city: string;
    street: string;
  };
  bank: {
    account_number: string | null;
    iban: string | null;
    name: string | null;
    swift_bic: string | null;
  };
}

/** PR-51 / session-71 — wire response body for the seller-info setup
 * route on the happy path. Backend returns `{ "state": "ready" }`;
 * the Tauri shell reads it to flip its boot-state mirror. */
export interface SetupSellerInfoResponse {
  state: "ready";
}

/** PR-51 / session-71 — per-field error from the typed 400 body. The
 * `field` matches the wizard composer's camelCase form-field name so
 * the SPA can highlight the offending input without a lookup table. */
export interface SetupSellerInfoFieldError {
  field: string;
  message: string;
}

/** PR-51 / session-71 — typed 400 body. The SPA's wizard parses this
 * out of the rejected-promise message and renders a per-field inline
 * error for each entry. */
export interface SetupSellerInfoErrorBody {
  error: "validation_failed";
  fields: SetupSellerInfoFieldError[];
}

/** PR-51 / session-71 — POST the SellerConfigWizard form to the
 * backend's `/api/setup-seller-info` route via the matching Tauri
 * command. On success the backend has written
 * `~/.aberp/<tenant>/seller.toml` and flipped its boot state to
 * Ready. Errors (typed 400 validation, 500 atomic-write failure)
 * propagate as the rejected promise. */
export async function setupSellerInfo(
  body: SetupSellerInfoRequest,
): Promise<SetupSellerInfoResponse> {
  return invoke<SetupSellerInfoResponse>("setup_seller_info", { body });
}

/** PR-53 / session-73 — wire response body for the new read-side
 * counterpart `GET /api/seller-info`. Mirror of the request shape
 * `SetupSellerInfoRequest` minus the wrapping — used by the SPA's
 * Tenant Settings page to pre-fill the form with the current saved
 * values. */
export interface SellerInfoResponse {
  legal_name: string;
  tax_number: string;
  eu_vat_number: string | null;
  address: {
    country_code: string;
    postal_code: string;
    city: string;
    street: string;
  };
  bank: {
    account_number: string | null;
    iban: string | null;
    name: string | null;
    swift_bic: string | null;
  };
}

/** PR-53 / session-73 — fetch the saved seller-info for the Tenant
 * Settings page. The backend route requires the backend to be in
 * `Ready` (the wizard chain ensures it is by the time the SPA reaches
 * the settings screen); the promise rejects on 404 (file missing) and
 * 503 (boot state pre-Ready). */
export async function getSellerInfo(): Promise<SellerInfoResponse> {
  return invoke<SellerInfoResponse>("get_seller_info");
}

/** PR-53 / session-73 — per-item presence flags for the four NAV
 * credential artifacts. The `login_value` field carries the operator-
 * visible login string; the other three values are NEVER returned
 * (presence-bool only). The SPA's NAV Credentials settings page reads
 * this to render the four rows + the Rotate buttons. */
export interface NavCredentialsStatusResponse {
  login: boolean;
  password: boolean;
  sign_key: boolean;
  change_key: boolean;
  login_value: string | null;
}

/** PR-53 / session-73 — fetch the four NAV credential presence flags
 * + the login value for the Settings page. */
export async function getNavCredentialsStatus(): Promise<NavCredentialsStatusResponse> {
  return invoke<NavCredentialsStatusResponse>("get_nav_credentials_status");
}

/** PR-53 / session-73 — wire request body for the single-slot rotate
 * route. `item` is one of the four operator-readable slugs (`login`,
 * `password`, `sign_key`, `change_key`); `new_value` is the new
 * secret. The login slug also flows through the same route since the
 * operator may rotate it independently. */
export interface RotateNavCredentialRequest {
  item: "login" | "password" | "sign_key" | "change_key";
  new_value: string;
}

/** PR-53 / session-73 — typed response body for the single-slot
 * rotate route. `ok` is always `true` on the happy path (4xx / 5xx
 * propagate as rejected promises); `item` echoes the rotated slug. */
export interface RotateNavCredentialResponse {
  ok: true;
  item: string;
}

/** PR-53 / session-73 — POST a single-slot NAV-credential rotation to
 * the backend. */
export async function rotateNavCredential(
  body: RotateNavCredentialRequest,
): Promise<RotateNavCredentialResponse> {
  return invoke<RotateNavCredentialResponse>("rotate_nav_credential", { body });
}

/** PR-54 / session-74 — closed-vocab discriminator on a partner row.
 * PascalCase wire mirror of `aberp::partners::PartnerKind`. Pinned by
 * `partner_kind_serde_round_trip_pin` on the Rust side; the SPA's
 * TS-strict consumption catches a backend drift at `npm run check`. */
export type PartnerKind = "Customer" | "Supplier" | "Both";

/** PR-54 / session-74 — single partner row. Snake_case JSON shape
 * mirrors `aberp::partners::Partner`'s `#[derive(Serialize)]` (no
 * `rename_all` directive on the Rust struct). `eu_vat_number` and the
 * address / bank / contact fields are all nullable since the operator
 * may skip them at create time. `deleted_at` is non-null only for soft-
 * deleted rows; the list endpoint hides them by default per A182. */
export interface Partner {
  /** Prefixed-ULID `prt_<26-char-ULID>`. */
  id: string;
  display_name: string;
  legal_name: string;
  kind: PartnerKind;
  /** PR-97 / ADR-0048 — closed-vocab buyer-kind discriminator.
   * Pre-PR-97 rows backfill to `"Domestic"` via the migration's
   * `DEFAULT 'Domestic'`. Drives whether `tax_number` is required
   * (`Domestic`) or forbidden (`PrivatePerson`) at the partner-form
   * validation gate. `Other` is named in the closed vocab but
   * v1-deferred per ADR-0048 §7. */
  customer_vat_status: CustomerVatStatusBody;
  /** PR-97 / ADR-0048 — nullable for non-Domestic statuses. */
  tax_number: string | null;
  eu_vat_number: string | null;
  address_street: string | null;
  address_postal_code: string | null;
  address_city: string | null;
  address_country: string | null;
  bank_account: string | null;
  contact_email: string | null;
  contact_phone: string | null;
  created_at: string;
  updated_at: string;
  deleted_at: string | null;
}

/** PR-54 / session-74 — request-body shape for `POST /api/partners` +
 * `PUT /api/partners/:id`. Mirror of `aberp::partners::PartnerInputs`
 * — every optional field defaults to `null` on the wire so the
 * backend's `#[serde(default)]` accepts the body without rejecting
 * missing keys. `display_name`, `legal_name`, `kind`, and `tax_number`
 * are required (the backend's `validate_partner_inputs` enforces). */
export interface PartnerInputs {
  display_name: string;
  legal_name: string;
  kind: PartnerKind;
  /** PR-97 / ADR-0048 — closed-vocab buyer-kind discriminator. The
   * form's three-option radio binds to this field; backend serde
   * defaults to `"Domestic"` when absent for pre-PR-97 callers. */
  customer_vat_status: CustomerVatStatusBody;
  /** PR-97 / ADR-0048 — nullable for non-Domestic statuses. */
  tax_number: string | null;
  eu_vat_number: string | null;
  address_street: string | null;
  address_postal_code: string | null;
  address_city: string | null;
  address_country: string | null;
  bank_account: string | null;
  contact_email: string | null;
  contact_phone: string | null;
}

/** PR-54 / session-74 — typed 400 validation body per field. Same
 * envelope shape as `SetupSellerInfoFieldError` (A157 inline-error
 * rendering pattern). */
export interface PartnerFieldError {
  field: string;
  message: string;
}

/** PR-54 / session-74 — typed 400 body for partner create / update.
 * Same discriminant as the seller-info path; consumed by the
 * PartnerForm modal's catch arm to surface per-field inline errors. */
export interface PartnerValidationErrorBody {
  error: "validation_failed";
  fields: PartnerFieldError[];
}

/** PR-54 / session-74 — `GET /api/partners[?search=]`. Used both by
 * the PartnersList screen (no search → full list) and by the typeahead
 * (search prefix, debounced 200ms). The backend filters case-
 * insensitively on `display_name` OR `legal_name` per
 * `aberp::partners::list_partners`. */
export async function listPartners(search?: string): Promise<Partner[]> {
  const trimmed = search?.trim();
  const args = trimmed && trimmed.length > 0 ? { search: trimmed } : {};
  return invoke<Partner[]>("list_partners", args);
}

/** PR-54 / session-74 — `GET /api/partners/:id`. */
export async function getPartner(partnerId: string): Promise<Partner> {
  return invoke<Partner>("get_partner", { partnerId });
}

/** PR-54 / session-74 — `POST /api/partners`. */
export async function createPartner(body: PartnerInputs): Promise<Partner> {
  return invoke<Partner>("create_partner", { body });
}

/** PR-54 / session-74 — `PUT /api/partners/:id`. */
export async function updatePartner(
  partnerId: string,
  body: PartnerInputs,
): Promise<Partner> {
  return invoke<Partner>("update_partner", { partnerId, body });
}

/** PR-54 / session-74 — `DELETE /api/partners/:id`. Soft-delete; the
 * row stays in the DB for historical-invoice resolution per A182. */
export async function deletePartner(partnerId: string): Promise<void> {
  await invoke<void>("delete_partner", { partnerId });
}

// ── PR-172 — buyer-facing notes-history typeahead source ─────────────

/** PR-172 — closed-vocab discriminator for the notes-history scope.
 * Each scope feeds a distinct textarea: per-line notes, per-invoice
 * notes, and storno reason. Mirrors the Rust-side
 * `notes_history::NotesHistoryScope`. Wire form is the lowercase
 * kebab string; an unknown value would 400 backend-side. */
export type NotesHistoryScope = "line" | "invoice" | "storno";

/** PR-172 — `GET /api/notes-history?scope=...&limit=...`. Returns the
 * operator's most-recently-used distinct notes for the requested
 * scope, ordered newest-first. Empty array on a fresh tenant. The
 * SPA's NotesAutocomplete component filters the response client-side
 * by a startsWith prefix match on the textarea content. */
export async function listNotesHistory(
  scope: NotesHistoryScope,
  limit?: number,
): Promise<string[]> {
  return invoke<string[]>("list_notes_history", { scope, limit });
}

// ── PR-91 — products master-data CRUD ────────────────────────────────

/** PR-91 — closed-vocab mirror of NAV v3.0's `unitOfMeasureType` enum
 * (sans OWN, which is expressed at the outer [`ProductUnit`] level).
 * Tokens are SCREAMING_SNAKE_CASE on the wire so they agree with the
 * NAV XML body. Pinned by `nav_unit_serde_round_trip_pin` on the Rust
 * side; the SPA reads the wire shape strictly via this typed union so
 * a backend drift surfaces at `npm run check`. See ADR-0046. */
export type NavUnitOfMeasure =
  | "PIECE"
  | "KILOGRAM"
  | "TON"
  | "KWH"
  | "DAY"
  | "HOUR"
  | "MINUTE"
  | "MONTH"
  | "LITER"
  | "KILOMETER"
  | "CUBIC_METER"
  | "METER"
  | "LINEAR_METER"
  | "CARTON"
  | "PACK";

/** PR-91 — product's unit of measure: either one of NAV's enum tokens
 * or a free-text label that the future NAV emitter will render as
 * `OWN` + `<unitOfMeasureOwn>{label}</...>`. Wire form is the Rust
 * internally-tagged serde shape (`{"kind":"Nav","value":"PIECE"}` /
 * `{"kind":"Own","value":"liter@15C"}`).
 *
 * The canonical Own case is `liter@15C` — temperature-corrected litre
 * (fuel measure); NAV has plain LITER but no temperature-corrected
 * variant. See ADR-0046 for the closed-vocab + escape-hatch rationale. */
export type ProductUnit =
  | { kind: "Nav"; value: NavUnitOfMeasure }
  | { kind: "Own"; value: string };

/** PR-91 — single product row. Snake_case JSON mirrors
 * `aberp::products::Product`.
 *
 * S231 / PR-227 / ADR-0061 — the GET /api/products and
 * GET /api/products/:id responses now include the optional inventory
 * cache fields per ADR-0061 §6. They are OPTIONAL on the TS side so
 * the create/update responses (which do not carry inventory data
 * yet) continue to type-check; the products-list and product-detail
 * consumers always receive them populated from the backend. The
 * dashboard chip reads `is_low_stock` straight off the wire per
 * CLAUDE.md rule 5 (the SPA does not duplicate the rule logic). */
export interface Product {
  /** Prefixed-ULID `prd_<26-char-ULID>`. */
  id: string;
  name: string;
  unit: ProductUnit;
  currency: Currency;
  /** Unit price in the currency's minor units (HUF: whole forints,
   * EUR: cents) per ADR-0037. The SPA parses operator input via
   * PR-88's `parseAmountToMinor` rule (bare ints are WHOLE major
   * units; cents only when an explicit separator is typed). */
  unit_price_minor: number;
  created_at: string;
  updated_at: string;
  deleted_at: string | null;
  /** S231 / PR-227 — Decimal-as-string per ADR-0061 §1. */
  stock_qty?: string;
  /** S231 / PR-227 — Decimal-as-string per ADR-0061 §1. */
  min_stock?: string;
  /** S231 / PR-227 — free-text v1; multi-cell deferred. */
  bin_location?: string | null;
  /** S231 / PR-227 — RFC3339 UTC of the most recent movement. */
  last_movement_at?: string | null;
  /** S231 / PR-227 — derived `stock_qty < min_stock` per ADR-0061 §3. */
  is_low_stock?: boolean;
}

/** PR-91 — request body for `POST /api/products` + `PUT /api/products/:id`. */
export interface ProductInputs {
  name: string;
  unit: ProductUnit;
  currency: Currency;
  unit_price_minor: number;
}

/** PR-91 — `GET /api/products[?search=]`. Case-insensitive prefix
 * filter on `name`. */
export async function listProducts(search?: string): Promise<Product[]> {
  const trimmed = search?.trim();
  const args = trimmed && trimmed.length > 0 ? { search: trimmed } : {};
  return invoke<Product[]>("list_products", args);
}

/** PR-91 — `GET /api/products/:id`. */
export async function getProduct(productId: string): Promise<Product> {
  return invoke<Product>("get_product", { productId });
}

/** PR-91 — `POST /api/products`. */
export async function createProduct(body: ProductInputs): Promise<Product> {
  return invoke<Product>("create_product", { body });
}

/** PR-91 — `PUT /api/products/:id`. */
export async function updateProduct(
  productId: string,
  body: ProductInputs,
): Promise<Product> {
  return invoke<Product>("update_product", { productId, body });
}

/** PR-91 — `DELETE /api/products/:id`. Soft-delete (mirrors
 * `deletePartner` per A182 — historical references stay resolvable). */
export async function deleteProduct(productId: string): Promise<void> {
  await invoke<void>("delete_product", { productId });
}

// ── S231 / PR-227 / ADR-0061 — Stage 3 Phase γ Inventory v1 ─────────

/** S231 — closed-vocab reason on `stock_movements`. Mirror of
 * `aberp_inventory::MovementReason` (snake_case storage strings per
 * ADR-0061 §2). */
export type StockMovementReason =
  | "receipt"
  | "bom_consumption"
  | "wo_completion"
  | "adjustment"
  | "dispatch"
  | "scrap";

/** S231 — closed-vocab ref-kind on `stock_movements`. Mirror of
 * `aberp_inventory::MovementRefKind`. */
export type StockMovementRefKind =
  | "work_order"
  | "qa_inspection"
  | "dispatch"
  | "invoice"
  | "manual";

/** S231 — one row from `stock_movements`. Snake_case mirror of
 * `aberp_inventory::StockMovement`. `qty_delta` is Decimal-as-string
 * per ADR-0061 §1 so JS clients do not lose precision. */
export interface StockMovement {
  movement_id: string;
  product_id: string;
  qty_delta: string;
  reason: StockMovementReason;
  ref_kind: StockMovementRefKind | null;
  ref_id: string | null;
  at_iso8601: string;
  operator: string;
  idempotency_key: string;
  notes: string | null;
}

/** S231 — POST /api/products/:id/stock-movements body. The SPA form
 * collects qty_delta + reason + notes and mints the idempotency_key
 * client-side per the F8 pattern. */
export interface CreateStockMovementInputs {
  qty_delta: string;
  reason: StockMovementReason;
  idempotency_key: string;
  notes?: string;
}

/** S231 — one row from GET /api/products/low-stock per ADR-0061 §3
 * virtual view. */
export interface LowStockRow {
  product_id: string;
  name: string;
  stock_qty: string;
  min_stock: string;
  bin_location: string | null;
}

/** S231 — `GET /api/products/:id/stock-movements?limit=&offset=`.
 * Pagination caps at 500 on the backend per
 * [[trust-code-not-operator]]; 100 is the default page size. */
export async function listStockMovements(
  productId: string,
  limit?: number,
  offset?: number,
): Promise<StockMovement[]> {
  return invoke<StockMovement[]>("list_stock_movements", {
    productId,
    limit,
    offset,
  });
}

/** S231 — `POST /api/products/:id/stock-movements`. The route layer
 * enforces the reason-sign matrix + the manual-form closed vocab
 * (Receipt / Adjustment / Scrap only) per ADR-0061 §6. A wrong-sign
 * POST surfaces as a 400 with a structured body. */
export async function createStockMovement(
  productId: string,
  body: CreateStockMovementInputs,
): Promise<StockMovement> {
  return invoke<StockMovement>("create_stock_movement", { productId, body });
}

/** S231 — `GET /api/products/low-stock`. ADR-0061 §3 virtual view —
 * products where the cached `stock_qty < min_stock`. Ordered by
 * deficit (most critical first). */
export async function listLowStockProducts(): Promise<LowStockRow[]> {
  return invoke<LowStockRow[]>("list_low_stock_products", {});
}

// ── S232 / PR-228 / ADR-0062 — Stage 3 Phase γ Work Orders v1 ────────

/** S232 — closed-vocab WO state per ADR-0062 §2. Snake_case storage
 * strings match `aberp_work_orders::WorkOrderState::as_str`. */
export type WorkOrderState =
  | "created"
  | "released"
  | "in_progress"
  | "completed"
  | "cancelled"
  | "on_hold";

/** S232 — closed-vocab WO action per ADR-0062 §3. The action a SPA
 * button (or future adapter event) submits to the transitions
 * endpoint. */
export type WoAction =
  | "release"
  | "start"
  | "complete"
  | "cancel"
  | "hold"
  | "resume";

/** S232 — closed-vocab per-operation state per ADR-0062 §2. */
export type RoutingOpState = "pending" | "active" | "completed" | "skipped";

/** S232 — one row from `work_orders`. Snake_case JSON mirror of
 * `aberp_work_orders::WorkOrder`. Decimal-as-string per
 * [[decimal-quantity-s157]]. */
export interface WorkOrder {
  wo_id: string;
  wo_number: string;
  product_id: string;
  qty_target: string;
  state: WorkOrderState;
  created_at: string;
  released_at: string | null;
  started_at: string | null;
  completed_at: string | null;
  cancelled_at: string | null;
  hold_reason: string | null;
  notes: string | null;
}

/** S232 — one routing operation. */
export interface RoutingOp {
  routing_op_id: string;
  wo_id: string;
  sequence: number;
  op_name: string;
  est_time_min: number | null;
  est_cost_huf: string | null;
  state: RoutingOpState;
  started_at: string | null;
  completed_at: string | null;
}

/** S232 — one BOM line. */
export interface BomLine {
  bom_line_id: string;
  product_id: string;
  component_id: string;
  qty_per_unit: string;
  created_at: string;
  retired_at: string | null;
}

/** S232 — POST /api/work-orders body. */
export interface CreateWorkOrderBody {
  wo_number: string;
  product_id: string;
  qty_target: string;
  notes?: string | null;
  routing_ops: CreateRoutingOpBody[];
  idempotency_key: string;
}

export interface CreateRoutingOpBody {
  op_name: string;
  est_time_min?: number | null;
  est_cost_huf?: string | null;
}

/** S232 — POST /api/work-orders response. */
export interface CreateWorkOrderResponse {
  work_order: WorkOrder;
  routing_ops: RoutingOp[];
}

/** S232 — GET /api/work-orders/:id response shape (WO + ops + active
 * BOM snapshot). */
export interface WorkOrderDetailResponse {
  work_order: WorkOrder;
  routing_ops: RoutingOp[];
  bom: BomLine[];
}

/** S232 — POST /api/work-orders/:id/transitions body. */
export interface TransitionWorkOrderBody {
  action: WoAction;
  reason?: string | null;
  idempotency_key: string;
}

/** S232 — POST /api/work-orders/:id/transitions response. */
export interface TransitionWorkOrderResponse {
  work_order: WorkOrder;
  warnings?: string[];
}

/** S232 — PUT BOM body: full-replace the active BOM rows. */
export interface PutProductBomBody {
  lines: PutProductBomLine[];
}

export interface PutProductBomLine {
  component_id: string;
  qty_per_unit: string;
}

/** S232 — `GET /api/work-orders[?state=&limit=&offset=]`. */
export async function listWorkOrders(
  stateFilter?: WorkOrderState | null,
  limit?: number,
  offset?: number,
): Promise<WorkOrder[]> {
  return invoke<WorkOrder[]>("list_work_orders", {
    stateFilter: stateFilter ?? null,
    limit: limit ?? null,
    offset: offset ?? null,
  });
}

/** S232 — `POST /api/work-orders`. */
export async function createWorkOrder(
  body: CreateWorkOrderBody,
): Promise<CreateWorkOrderResponse> {
  return invoke<CreateWorkOrderResponse>("create_work_order", { body });
}

/** S232 — `GET /api/work-orders/:id`. */
export async function getWorkOrder(
  woId: string,
): Promise<WorkOrderDetailResponse> {
  return invoke<WorkOrderDetailResponse>("get_work_order", { woId });
}

/** S232 — `POST /api/work-orders/:id/transitions`. */
export async function transitionWorkOrder(
  woId: string,
  body: TransitionWorkOrderBody,
): Promise<TransitionWorkOrderResponse> {
  return invoke<TransitionWorkOrderResponse>("transition_work_order", {
    woId,
    body,
  });
}

/** S232 — `GET /api/products/:id/bom`. */
export async function getProductBom(productId: string): Promise<BomLine[]> {
  return invoke<BomLine[]>("get_product_bom", { productId });
}

/** S232 — `POST /api/products/:id/bom`. Replace active BOM lines. */
export async function putProductBom(
  productId: string,
  body: PutProductBomBody,
): Promise<BomLine[]> {
  return invoke<BomLine[]>("put_product_bom", { productId, body });
}

// ── S233 / PR-229 / ADR-0063 — Stage 3 Phase γ QA queue v1 ──────────

/** S233 — closed-vocab inspection state per ADR-0063 §1. Snake_case
 * storage strings match `aberp_qa::QaState::as_str`. */
export type QaState =
  | "pending"
  | "passed"
  | "failed"
  | "reworking"
  | "disposed";

/** S233 — closed-vocab decision verb per ADR-0063 §3. */
export type QaDecision = "pass" | "fail" | "rework" | "dispose";

/** S233 — one row from `qa_inspections`. Snake_case JSON mirror of
 * `aberp_qa::QaInspection`. */
export interface QaInspection {
  qa_id: string;
  wo_id: string;
  routing_op_id: string;
  state: QaState;
  decided_at: string | null;
  decided_by: string | null;
  reason: string | null;
  measurement: string | null;
  source_event_id: string | null;
  created_at: string;
  superseded_by: string | null;
}

/** S233 — `POST /api/qa-inspections/:id/decisions` body. */
export interface DecideQaInspectionBody {
  decision: QaDecision;
  reason?: string | null;
  measurement?: string | null;
  idempotency_key: string;
}

/** S233 — `POST /api/qa-inspections/:id/decisions` response. */
export interface DecideQaInspectionResponse {
  inspection: QaInspection;
  superseded_qa_id: string | null;
  rework_flipped_routing_op_back_to_active: boolean;
  disposed_emitted_scrap_movement: boolean;
}

/** S233 — `GET /api/qa-inspections[?state=&limit=&offset=]`. */
export async function listQaInspections(
  stateFilter?: QaState | null,
  limit?: number,
  offset?: number,
): Promise<QaInspection[]> {
  return invoke<QaInspection[]>("list_qa_inspections", {
    stateFilter: stateFilter ?? null,
    limit: limit ?? null,
    offset: offset ?? null,
  });
}

/** S233 — `GET /api/qa-inspections/:id`. */
export async function getQaInspection(qaId: string): Promise<QaInspection> {
  return invoke<QaInspection>("get_qa_inspection", { qaId });
}

/** S233 — `POST /api/qa-inspections/:id/decisions`. */
export async function decideQaInspection(
  qaId: string,
  body: DecideQaInspectionBody,
): Promise<DecideQaInspectionResponse> {
  return invoke<DecideQaInspectionResponse>("decide_qa_inspection", {
    qaId,
    body,
  });
}

// ── S234 / PR-230 / ADR-0064 — Stage 3 Phase γ Dispatch board v1 ────

/** S234 — closed-vocab dispatch state per ADR-0064 §1. Snake_case
 * storage strings match `aberp_dispatch::DispatchState::as_str`. */
export type DispatchState = "drafted" | "shipped" | "cancelled";

/** S234 — closed-vocab carrier per ADR-0064 §1. Snake_case storage
 * strings match `aberp_dispatch::CarrierKind::as_str`. New carriers
 * go in by enum extension; free text is refused at the wire boundary
 * per ADR-0064 §"Invariants pinned" #8. */
export type CarrierKind =
  | "self_delivery"
  | "customer_pickup"
  | "magyar_posta"
  | "gls"
  | "dpd"
  | "foxpost"
  | "other";

/** S234 — one row from `dispatches`. Snake_case JSON mirror of
 * `aberp_dispatch::Dispatch`. */
export interface Dispatch {
  dsp_id: string;
  wo_id: string;
  partner_id: string;
  state: DispatchState;
  created_at: string;
  shipped_at: string | null;
  cancelled_at: string | null;
  carrier_kind: CarrierKind | null;
  tracking_number: string | null;
  spawned_invoice_id: string | null;
  notes: string | null;
}

/** S234 — one row from the eligible-WO read view per ADR-0064 §2. */
export interface EligibleWorkOrder {
  wo_id: string;
  wo_number: string;
  product_id: string;
  qty_target: string;
  completed_at: string;
}

/** S234 — `POST /api/dispatches` body. */
export interface CreateDispatchBody {
  wo_id: string;
  partner_id: string;
  notes?: string | null;
  idempotency_key: string;
}

/** S234 — `POST /api/dispatches/:id/ship` body. */
export interface MarkDispatchShippedBody {
  carrier_kind: CarrierKind;
  tracking_number?: string | null;
  shipped_at?: string | null;
  idempotency_key: string;
}

/** S234 — `POST /api/dispatches/:id/ship` response. */
export interface MarkDispatchShippedResponse {
  dispatch: Dispatch;
  /** `Some(invoice_id)` when the injected spawner produced a draft;
   * `null` for the v1 NoopInvoiceSpawner posture pending PR-230b. */
  spawned_invoice_id: string | null;
  stock_movement_id: string;
}

/** S234 — `GET /api/dispatches[?state=&limit=&offset=]`. */
export async function listDispatches(
  stateFilter?: DispatchState | null,
  limit?: number,
  offset?: number,
): Promise<Dispatch[]> {
  return invoke<Dispatch[]>("list_dispatches", {
    stateFilter: stateFilter ?? null,
    limit: limit ?? null,
    offset: offset ?? null,
  });
}

/** S234 — `GET /api/dispatches/:id`. */
export async function getDispatch(dspId: string): Promise<Dispatch> {
  return invoke<Dispatch>("get_dispatch", { dspId });
}

/** S234 — `GET /api/dispatches/eligible-work-orders[?limit=]`. */
export async function listEligibleWorkOrders(
  limit?: number,
): Promise<EligibleWorkOrder[]> {
  return invoke<EligibleWorkOrder[]>("list_eligible_work_orders", {
    limit: limit ?? null,
  });
}

/** S234 — `POST /api/dispatches`. */
export async function createDispatch(body: CreateDispatchBody): Promise<Dispatch> {
  return invoke<Dispatch>("create_dispatch", { body });
}

/** S234 — `POST /api/dispatches/:id/ship`. Atomic per ADR-0064 §4 +
 * §5 + invariant #1. */
export async function markDispatchShipped(
  dspId: string,
  body: MarkDispatchShippedBody,
): Promise<MarkDispatchShippedResponse> {
  return invoke<MarkDispatchShippedResponse>("mark_dispatch_shipped", {
    dspId,
    body,
  });
}

/** S234 — `POST /api/dispatches/:id/cancel`. Valid only from Drafted. */
export async function cancelDispatch(dspId: string): Promise<Dispatch> {
  return invoke<Dispatch>("cancel_dispatch", { dspId });
}

// ── S233 / PR-229 Part A — per-routing-op Complete cascade ──────────

/** S233 — closed-vocab per-op action; v1 just `complete`. */
export type RoutingOpAction = "complete";

/** S233 — `POST /api/work-orders/:wo_id/routing-ops/:op_id/transitions`
 * body. */
export interface TransitionRoutingOpBody {
  action: RoutingOpAction;
  idempotency_key: string;
}

/** S233 — `POST .../routing-ops/:op_id/transitions` response. */
export interface TransitionRoutingOpResponse {
  routing_op: RoutingOp;
  next_op_activated: RoutingOp | null;
  /** The auto-created Pending QA inspection id (ADR-0063 §2). */
  qa_inspection_id: string;
}

/** S233 — `POST /api/work-orders/:wo_id/routing-ops/:op_id/transitions`. */
export async function transitionRoutingOp(
  woId: string,
  opId: string,
  body: TransitionRoutingOpBody,
): Promise<TransitionRoutingOpResponse> {
  return invoke<TransitionRoutingOpResponse>("transition_routing_op", {
    woId,
    opId,
    body,
  });
}

// ── PR-72 / session-94 — multi-bank-account routes (PR-B) ─────────────

/** PR-72 / session-94 — closed-vocab currency on a bank-account row.
 * Mirror of the Rust-side ADR-0037 `Currency` enum. Pinned by the
 * Rust round-trip pins on `Currency::iso_code`; the SPA's TS-strict
 * consumption catches a backend drift at `npm run check`. */
export type SellerBankCurrency = "HUF" | "EUR";

/** PR-72 / session-94 — one bank-account row. Snake_case JSON mirrors
 * the Rust-side `serve::SellerBankResponse`. `id` is the deterministic
 * `bnk_<26-char-ULID>` derived over `(currency, account_number)`. */
export interface SellerBankResponse {
  id: string;
  currency: SellerBankCurrency;
  account_number: string;
  bank_name: string;
  swift_bic: string;
  is_default: boolean;
}

/** PR-72 / session-94 — list / mutation response shape. Always carries
 * the full updated collection so the SPA re-renders the list view
 * from one source of truth after every mutation (one round-trip, not
 * two). */
export interface SellerBanksListResponse {
  banks: SellerBankResponse[];
}

/** PR-72 / session-94 — request body for create + update. Snake_case
 * to match the Rust-side `serve::SellerBankInputs`. `set_as_default`
 * is only meaningful on the POST + PUT paths; the dedicated
 * set-default route has no body. */
export interface SellerBankInputs {
  currency: SellerBankCurrency;
  account_number: string;
  bank_name: string;
  swift_bic: string;
  set_as_default: boolean;
}

/** PR-72 / session-94 — per-field error from the typed 400 body.
 * Field names are camelCase to match the form input names in
 * TenantSettings + SellerConfigWizard's bank-row composer. */
export interface SellerBankFieldError {
  field: string;
  message: string;
}

/** PR-72 / session-94 — typed 400 body. Discriminant matches the
 * setup-seller-info + partners routes so the existing parser
 * pattern can be reused for the bank-account form. */
export interface SellerBankValidationErrorBody {
  error: "validation_failed";
  fields: SellerBankFieldError[];
}

/** PR-72 / session-94 — `GET /api/seller/banks`. The TenantSettings
 * "Bank accounts" subsection calls this on open. */
export async function listSellerBanks(): Promise<SellerBanksListResponse> {
  return invoke<SellerBanksListResponse>("list_seller_banks");
}

/** PR-72 / session-94 — `POST /api/seller/banks`. The "Add bank
 * account" modal POSTs the composed inputs body here. */
export async function createSellerBank(
  body: SellerBankInputs,
): Promise<SellerBanksListResponse> {
  return invoke<SellerBanksListResponse>("create_seller_bank", { body });
}

/** PR-72 / session-94 — `PUT /api/seller/banks/:id`. The "Edit"
 * affordance PUTs here. `set_as_default` MUST be `false` on this path
 * — the route preserves the existing flag and ignores the input
 * value; the dedicated set-default route owns the flip intent. */
export async function updateSellerBank(
  bankId: string,
  body: SellerBankInputs,
): Promise<SellerBanksListResponse> {
  return invoke<SellerBanksListResponse>("update_seller_bank", {
    bankId,
    body,
  });
}

/** PR-72 / session-94 — `POST /api/seller/banks/:id/set-default`.
 * Flips the marked default to this entry for its currency; demotes
 * the previous default in the same write. */
export async function setDefaultSellerBank(
  bankId: string,
): Promise<SellerBanksListResponse> {
  return invoke<SellerBanksListResponse>("set_default_seller_bank", { bankId });
}

/** PR-72 / session-94 — `DELETE /api/seller/banks/:id`. Returns the
 * updated collection on success. Surfaces 409 Conflict if the delete
 * would leave the currency unrepresented while other currencies still
 * have entries (see the brief's explicit refusal rule). */
export async function deleteSellerBank(
  bankId: string,
): Promise<SellerBanksListResponse> {
  return invoke<SellerBanksListResponse>("delete_seller_bank", { bankId });
}

import type { NumberingTemplate } from "./invoice-numbering";

/** PR-89 — `GET /api/seller/numbering`. Returns the operator-
 * configured invoice-number template (or the default INV-default/NNNNN
 * shape when no `[seller.numbering]` section is present in
 * seller.toml). */
export async function getSellerNumbering(): Promise<NumberingTemplate> {
  return invoke<NumberingTemplate>("get_seller_numbering");
}

/** PR-89 — `PUT /api/seller/numbering`. The Invoice numbering builder
 * PUTs the operator-assembled template here. Backend validates
 * (closed-vocab on kinds + reset policy, NAV-charset on Literal
 * segments, exactly-one-counter) and atomically replaces the
 * `[seller.numbering]` section of seller.toml. Returns the validated
 * (canonical) template on success; 422 on validation failure. */
export async function putSellerNumbering(
  body: NumberingTemplate,
): Promise<NumberingTemplate> {
  return invoke<NumberingTemplate>("put_seller_numbering", { body });
}

// ── PR-92 / ADR-0047 — SMTP email delivery ─────────────────────────

/** PR-92 / ADR-0047 — closed-vocab SMTP transport security. NO
 * plaintext variant — TLS is mandatory; the backend rejects any
 * other token. */
export type SmtpSecurity = "StartTls" | "Tls";

/** PR-92 — wire shape of GET /api/smtp-config when no
 * `[seller.smtp]` is configured. The SPA renders an empty form. */
export interface SmtpConfigGetEmpty {
  configured: false;
  passwordSet: boolean;
}

/** PR-92 — wire shape of GET /api/smtp-config when SMTP is
 * configured. NEVER carries the password — the backend reports a
 * `passwordSet` boolean instead. */
export interface SmtpConfigGetPopulated {
  configured?: true;
  host: string;
  port: number;
  fromAddress: string;
  fromDisplayName?: string | null;
  username: string;
  security: SmtpSecurity;
  attachXml: boolean;
  passwordSet: boolean;
}

export type SmtpConfigGetResponse =
  | SmtpConfigGetEmpty
  | SmtpConfigGetPopulated;

/** PR-92 — wire body for PUT /api/smtp-config. `password` is
 * optional: `null` / absent leaves the existing keychain entry
 * untouched (so the operator can rotate non-secret fields without
 * re-typing the password). */
export interface SmtpConfigPutBody {
  host: string;
  port: number;
  fromAddress: string;
  fromDisplayName?: string | null;
  username: string;
  security: SmtpSecurity;
  attachXml: boolean;
  password?: string | null;
}

/** PR-92 — fetch the current SMTP config + keychain password status. */
export async function getSmtpConfig(): Promise<SmtpConfigGetResponse> {
  return invoke<SmtpConfigGetResponse>("get_smtp_config");
}

/** PR-92 — write the SMTP config (merge-not-replace on seller.toml)
 * + optionally rotate the password in the keychain. */
export async function putSmtpConfig(
  body: SmtpConfigPutBody,
): Promise<SmtpConfigGetPopulated> {
  return invoke<SmtpConfigGetPopulated>("put_smtp_config", { body });
}

/** PR-92 — operator-clicked manual send button on InvoiceDetail.
 * Returns the same EmailRouteOutcome shape the auto-send-after-issue
 * surfaces, so a single TS interface drives both renderers. */
export async function emailInvoiceToBuyer(
  invoiceId: string,
): Promise<EmailRouteOutcome> {
  return invoke<EmailRouteOutcome>("email_invoice_to_buyer", { invoiceId });
}

/** PR-98 — outcome of the SMTP test-connection probe. Mirrors
 * `serve::SmtpTestOutcome` on the backend. Shape mirrors
 * [`EmailRouteOutcome`] so the same banner-rendering helper can be
 * reused on both surfaces. */
export interface SmtpTestOutcome {
  /** Closed-vocab: `"succeeded"` | `"failed"`. */
  outcome: "succeeded" | "failed";
  /** Closed-vocab error class on failure; absent on success. */
  error_class?:
    | "transport"
    | "tls"
    | "auth"
    | "recipient_rejected"
    | "compose"
    | "other";
  /** Operator-readable detail on failure; absent on success. */
  error_detail?: string;
}

/** PR-98 — TenantSettings "Test connection" button. POSTs the same
 * `SmtpConfigPutBody` shape as `putSmtpConfig` but the backend runs
 * the TLS handshake + AUTH + NOOP without sending mail or persisting
 * anything. Leaving `password` empty / null tests against the
 * existing keychain entry. */
export async function testSmtpConnection(
  body: SmtpConfigPutBody,
): Promise<SmtpTestOutcome> {
  return invoke<SmtpTestOutcome>("test_smtp_connection", { body });
}

// ── PR-179 / session-179 — AP module SPA surface ────────────────────
//
// Mirrors `apps/aberp/src/incoming_invoices.rs::IncomingInvoice` 1:1.
// Currency rides as `string` not `Currency` because the backend stores
// the raw column verbatim — the IncomingInvoiceList component coerces
// it to the closed-vocab union at render time (so a future supplier
// invoice in GBP renders muted rather than crashing the table).
export interface IncomingInvoice {
  id: string;
  supplier_tax_number: string;
  supplier_name: string;
  supplier_address: string | null;
  nav_invoice_number: string;
  issue_date: string;
  delivery_date: string | null;
  payment_deadline: string | null;
  total_net_minor: number;
  total_vat_minor: number;
  total_gross_minor: number;
  currency: string;
  /** Closed-vocab string: `"Outstanding"` | `"Paid"` | `"Irrelevant"`.
   * A backend that drifts to a fourth label surfaces as the muted
   * "Unknown" chip per CLAUDE.md rule 12 — visible miss, not a crash. */
  local_status: string;
  irrelevant_reason: string | null;
  nav_xml_path: string | null;
  created_at: string;
  updated_at: string;
}

/** Mirrors `serve::MarkIncomingStatusResponse`. */
export interface MarkIncomingStatusResponse {
  id: string;
  from_status: string;
  to_status: string;
  reason: string | null;
  entries_verified: number;
}

/** Mirrors `serve::SyncIncomingNowResponse`. */
export interface SyncIncomingNowResponse {
  /** `"ok"` on a clean cycle; `"error"` on loud-fail. */
  status: "ok" | "error";
  ingested_count: number;
  skipped_count: number;
  pages_walked: number;
  elapsed_ms: number;
  date_from: string;
  date_to: string;
  /** `null` on success; verbatim NAV diagnostic on failure. */
  error: string | null;
}

export async function listIncomingInvoices(): Promise<IncomingInvoice[]> {
  return invoke<IncomingInvoice[]>("list_incoming_invoices");
}

export async function markIncomingPaid(
  incomingId: string,
): Promise<MarkIncomingStatusResponse> {
  return invoke<MarkIncomingStatusResponse>("mark_incoming_paid", { incomingId });
}

export async function markIncomingOutstanding(
  incomingId: string,
): Promise<MarkIncomingStatusResponse> {
  return invoke<MarkIncomingStatusResponse>("mark_incoming_outstanding", {
    incomingId,
  });
}

export async function markIncomingIrrelevant(
  incomingId: string,
  reason: string,
): Promise<MarkIncomingStatusResponse> {
  return invoke<MarkIncomingStatusResponse>("mark_incoming_irrelevant", {
    incomingId,
    body: { reason },
  });
}

export async function syncIncomingInvoicesNow(): Promise<SyncIncomingNowResponse> {
  return invoke<SyncIncomingNowResponse>("sync_incoming_invoices_now");
}

/** S197 / PR-197 — fetch the raw NAV InvoiceData XML bytes for one
 * AP-side row whose `nav_xml_path` is populated. The backend serves
 * `application/xml`; the Tauri command relays the bytes as
 * `Vec<u8>` and we re-wrap them in a `Blob` for the browser-native
 * save dialog (same posture as `downloadInvoicePdf`). Errors (404
 * when `nav_xml_path` is still NULL, 500 on disk read failure)
 * propagate as the rejected promise per the existing AP-action
 * surface. */
export async function downloadIncomingXml(incomingId: string): Promise<Blob> {
  const bytes = await invoke<number[]>("download_incoming_xml", {
    incomingId,
  });
  return new Blob([new Uint8Array(bytes)], { type: "application/xml" });
}

// ── S180 / PR-180 — NAV-as-DR restore wizard ────────────────────────

/** Mirror of `restore_from_nav_outgoing::RestoreSummary`. The wizard
 * renders every field; a backend rename surfaces at `npm run check`. */
export interface RestoreSummary {
  year: number;
  restored: number;
  skipped: number;
  errored: number;
  pages_walked: number;
  elapsed_ms: number;
}

/** Mirror of `restore_from_nav_outgoing::RestoredInvoice` — one
 * row in the local `restored_invoice` table. */
export interface RestoredInvoice {
  id: string;
  source_nav_invoice_number: string;
  source_nav_transaction_id: string | null;
  issue_date: string;
  total_net_minor: number;
  total_vat_minor: number;
  total_gross_minor: number;
  currency: Currency;
  restore_year: number;
  created_at: string;
}

/** S186 / PR-186 — the backend now REQUIRES a `confirm_token` field
 * equal to the literal `"RESTORE"`. The wizard already gates submit
 * on `isRestoreConfirmed`, so by the time this call fires the
 * operator has typed the token; we forward it verbatim so the
 * backend's equality check (mirroring `isRestoreConfirmed`) passes.
 * Missing or mismatched → 400 from the backend. */
export async function restoreFromNavOutgoing(
  year: number,
  confirmToken: string,
): Promise<RestoreSummary> {
  return invoke<RestoreSummary>("restore_from_nav_outgoing", {
    body: { year, confirm_token: confirmToken },
  });
}

export async function listRestoredInvoices(): Promise<RestoredInvoice[]> {
  return invoke<RestoredInvoice[]>("list_restored_invoices");
}

/** S220 / PR-217 — `POST /api/restored-invoices/:id/partner`.
 *
 * Operator-paced manual partner link on a restored ExtNav row. Per
 * [[aberp-extnav-partner-nav-gap]] NAV does not expose buyer info for
 * invoices submitted via other software, so the backfill cannot
 * populate `customer_name` for those rows; the SPA partner-picker
 * modal calls this to annotate from the operator's own records.
 *
 * Pass `partnerId === null` to clear an existing link (the row falls
 * back to NULL `customer_name` and renders as the em-dash again).
 *
 * Returns the post-write denormalized snapshot so the caller can
 * refresh the row inline without a second list-restored round trip.
 */
export interface RestoredPartnerSnapshot {
  partner_id: string | null;
  customer_name: string | null;
  customer_tax_number: string | null;
  customer_vat_status: string | null;
}

export async function setRestoredPartner(
  restoredId: string,
  partnerId: string | null,
): Promise<RestoredPartnerSnapshot> {
  return invoke<RestoredPartnerSnapshot>("set_restored_partner", {
    restoredId,
    body: { partner_id: partnerId },
  });
}

// ── S211 / PR-210 — Quote intake config + queue ─────────────────────

/** Most-recent `QuoteIntakePollCompleted` audit summary, surfaced by the
 * Tenant Settings status panel. `null` when the daemon has never
 * emitted (a pure-zero cycle is silent per crate `should_emit`). */
export interface QuoteIntakeLastPoll {
  at: string;
  trigger: string;
  fetched_count: number;
  created_count: number;
  skipped_duplicate_count: number;
  writeback_retried_count: number;
  writeback_failed_count: number;
  failed_count: number;
  elapsed_ms: number;
  error?: string | null;
}

/** `GET /api/quote-intake/config` shape. The bearer token NEVER round-
 * trips through the SPA — `has_token` is the only signal the operator
 * sees about its presence. `env_override_active` is `true` when env
 * vars are providing the live daemon config; the SPA shows the form
 * read-only in that state because a save would silently lose to the
 * env var on the next boot. */
export interface QuoteIntakeConfigResponse {
  enabled: boolean;
  base_url: string | null;
  poll_interval_secs: number;
  has_token: boolean;
  env_override_active: boolean;
  last_poll?: QuoteIntakeLastPoll | null;
}

/** PUT body — `token` is optional; an empty / absent value leaves the
 * existing keychain entry untouched. Same posture as the SMTP password
 * input on the SMTP settings card. */
export interface QuoteIntakeConfigPutBody {
  enabled: boolean;
  base_url: string | null;
  poll_interval_secs: number | null;
  token: string | null;
}

export interface QuoteIntakeTestOutcome {
  outcome: "succeeded" | "failed";
  error_class?: string | null;
  error_detail?: string | null;
}

export async function getQuoteIntakeConfig(): Promise<QuoteIntakeConfigResponse> {
  return invoke<QuoteIntakeConfigResponse>("get_quote_intake_config");
}

export async function putQuoteIntakeConfig(
  body: QuoteIntakeConfigPutBody,
): Promise<QuoteIntakeConfigResponse> {
  return invoke<QuoteIntakeConfigResponse>("put_quote_intake_config", { body });
}

export async function testQuoteIntakeConnection(body: {
  base_url: string;
  token: string | null;
}): Promise<QuoteIntakeTestOutcome> {
  return invoke<QuoteIntakeTestOutcome>("test_quote_intake_connection", {
    body,
  });
}

/** Single row of the operator Quotes tab — mirrors the backend
 * `QuoteIntakeRow`. Lossy summary fields (contact / material /
 * quantity / notes) may be `null` if the daemon-side raw payload
 * didn't carry them. */
export interface QuoteIntakeRow {
  quote_id: string;
  invoice_id: string;
  received_at: string;
  intake_at: string;
  status_writeback_at: string | null;
  contact_name: string | null;
  contact_email: string | null;
  contact_company: string | null;
  material: string | null;
  quantity: string | null;
  notes: string | null;
}

export async function listQuoteIntake(): Promise<QuoteIntakeRow[]> {
  return invoke<QuoteIntakeRow[]>("list_quote_intake");
}

// ── S225 / PR-221 — Financial statistics dashboard ──────────────────

/** One per-currency aggregate (HUF cents-equivalent, EUR cents).
 * Mirrors `aberp::reports::AmountAggregate`. Minor-unit semantics
 * inherit the closed-vocab Currency rule: HUF = whole forints,
 * EUR = cents. */
export interface AmountAggregate {
  gross_minor: number;
  net_minor: number;
  vat_minor: number;
  count: number;
}

/** Parallel HUF + EUR aggregates. The dashboard renders the two side-
 * by-side; either may be zero. */
export interface CurrencyAggregate {
  huf: AmountAggregate;
  eur: AmountAggregate;
}

/** Parallel HUF + EUR amounts (no count/net/vat split). */
export interface CurrencyPair {
  huf_minor: number;
  eur_minor: number;
}

/** AR / AP aging buckets in days-overdue. `current` = not yet due. */
export interface AgingPanel {
  current: AmountAggregate;
  days_1_30: AmountAggregate;
  days_31_60: AmountAggregate;
  days_61_90: AmountAggregate;
  days_90_plus: AmountAggregate;
}

/** Days Sales Outstanding (DSO) — avg(paid - issued). `null` when
 * sample size is zero. */
export interface DsoPanel {
  huf_days: number | null;
  eur_days: number | null;
  huf_sample_size: number;
  eur_sample_size: number;
}

/** Forward cashflow projection — gross of not-yet-overdue receivables
 * due within the next N days. */
export interface CashflowPanel {
  next_30: CurrencyPair;
  next_60: CurrencyPair;
  next_90: CurrencyPair;
}

export interface VatRateBreakdownEntry {
  rate_basis_points: number;
  currency: string;
  net_minor: number;
  vat_minor: number;
}

export interface TopEntry {
  label: string;
  currency: string;
  gross_minor: number;
  count: number;
}

export interface HygienePanel {
  outgoing_rejected_count: number;
  outgoing_abandoned_count: number;
  outgoing_pending_count: number;
  restored_no_partner_count: number;
  outstanding_past_deadline_count: number;
  payable_past_deadline_count: number;
  storno_chain_count: number;
  modification_chain_count: number;
}

export interface DeltaSet {
  period_label: string;
  revenue: CurrencyAggregate;
  expenses: CurrencyAggregate;
  revenue_pct_huf: number | null;
  revenue_pct_eur: number | null;
  expenses_pct_huf: number | null;
  expenses_pct_eur: number | null;
}

export interface PeriodDeltas {
  mom: DeltaSet | null;
  yoy: DeltaSet | null;
}

export interface AnnualRunningPanel {
  year: number;
  revenue: CurrencyAggregate;
}

export interface PeriodMeta {
  kind: string;
  label: string;
  from: string | null;
  to: string | null;
  date_basis: string;
  today: string;
}

export interface FinancialReport {
  period: PeriodMeta;
  revenue: CurrencyAggregate;
  expenses: CurrencyAggregate;
  gross_profit: CurrencyPair;
  vat_collected: CurrencyAggregate;
  vat_paid: CurrencyAggregate;
  vat_to_pay: CurrencyPair;
  receivables: CurrencyAggregate;
  payables: CurrencyAggregate;
  receivables_aging: AgingPanel;
  payables_aging: AgingPanel;
  dso_days: DsoPanel;
  cashflow_forward: CashflowPanel;
  vat_breakdown_outgoing: VatRateBreakdownEntry[];
  top_customers: TopEntry[];
  top_vendors: TopEntry[];
  hygiene: HygienePanel;
  deltas: PeriodDeltas;
  annual_running: AnnualRunningPanel;
  deferred_notes: string[];
}

/** Fetch the financial-statistics snapshot for the given period +
 * date basis. Both args optional — backend defaults to current month +
 * `teljesites` (delivery-date) basis when either is empty. */
export async function getFinancialReport(
  period?: string,
  dateBasis?: string,
): Promise<FinancialReport> {
  return invoke<FinancialReport>("get_financial_report", {
    period: period ?? null,
    dateBasis: dateBasis ?? null,
  });
}

// ── S235 / PR-231 — Workshop / Műhely operator dashboard ───────────────
//
// One endpoint returns every count tile in one bundle. The SPA polls
// it every ~10s; each tile is a slice of the response. See
// `Workshop.svelte` for the render.

export interface WorkOrderStateCounts {
  created: number;
  released: number;
  in_progress: number;
  completed: number;
  on_hold: number;
  cancelled: number;
}

export interface QaStateCounts {
  pending: number;
  passed: number;
  failed: number;
  reworking: number;
  disposed: number;
}

export interface DispatchStateCounts {
  drafted: number;
  shipped: number;
  cancelled: number;
}

export interface DispatchPanel {
  by_state: DispatchStateCounts;
  eligible_work_orders: number;
  shipped_today: number;
}

export interface LowStockCount {
  count: number;
}

export interface TodayPanel {
  date: string;
  issued_count_huf: number;
  issued_count_eur: number;
  gross_revenue_huf_minor: number;
  gross_revenue_eur_minor: number;
}

export interface RecentActivityEntry {
  id: string;
  kind: string;
  at_iso8601: string;
  seq: number;
}

/** Closed-vocab adapter health, mirrored from the Rust
 *  `AdapterHealth` enum via `serve::adapter_health_status`. Drives
 *  `adapterDotClass` + chip rendering in `Workshop.svelte`. A future
 *  variant (e.g. `"maintenance"`) needs widening here AND in the
 *  Rust mapper at the same time — the helper signature catches drift
 *  at TS compile time. */
export type AdapterStatus =
  | "healthy"
  | "degraded"
  | "unhealthy"
  | "starting"
  | "stopped";

export interface AdapterStatusSnapshot {
  name: string;
  status: AdapterStatus;
  kind: string;
  /** Empty string when the adapter declines a TCP endpoint (e.g. a
   *  polled HTTP client). The SPA suppresses the `host:port` line in
   *  that case. */
  host: string;
  /** 0 when the adapter declines a TCP endpoint. */
  port: number;
}

// S246 / PR-239 — wall-TV density rows. Each tile that previously
// surfaced only an aggregate count now also lists the underlying
// items. Field naming on the parent payload uses the `_rows` suffix
// where the count-shaped field already owned the plural slot
// (`work_orders`, `low_stock_products`); on tiles with no naming
// collision (`pending_qa`, `eligible_dispatch`, `pending_dispatch`,
// `today_invoice`) the suffix is dropped from the brief's draft
// names — every parent-level field reads `_rows` to keep the wire
// shape consistent. Mirrors `serve.rs::WorkshopDashboard`.
export interface WorkOrderRow {
  wo_id: string;
  wo_number: string;
  product_name: string;
  /** snake_case `WorkOrderState` — the SPA's existing chip mapping
   * matches on these strings. */
  state:
    | "created"
    | "released"
    | "in_progress"
    | "completed"
    | "cancelled"
    | "on_hold";
  /** RFC3339 — newest non-NULL transition timestamp; `created_at`
   *  floor. Render via `fmtRelativeTime`. */
  touched_at_iso8601: string;
  /** Decimal-as-string. */
  qty_target: string;
}

export interface LowStockItemRow {
  product_id: string;
  name: string;
  stock_qty: string;
  min_stock: string;
  bin_location: string;
}

export interface PendingQaRow {
  qa_id: string;
  wo_id: string;
  wo_number: string;
  routing_op_id: string;
  op_name: string;
  created_at_iso8601: string;
}

export interface EligibleDispatchRow {
  wo_id: string;
  wo_number: string;
  product_name: string;
  qty_target: string;
  completed_at_iso8601: string;
}

export interface PendingDispatchRow {
  dsp_id: string;
  wo_id: string;
  wo_number: string;
  partner_name: string;
  created_at_iso8601: string;
}

export interface TodayInvoiceRow {
  invoice_id: string;
  sequence_number: number;
  fiscal_year: number;
  currency: "HUF" | "EUR";
  /** Same `total_gross` semantics as the InvoiceList row — null when
   *  the row has no lines yet. */
  total_gross_minor: number | null;
  buyer_name: string;
  issue_date: string;
}

export interface WorkshopDashboard {
  work_orders: WorkOrderStateCounts;
  low_stock_products: LowStockCount;
  qa: QaStateCounts;
  dispatch: DispatchPanel;
  today: TodayPanel;
  recent_activity: RecentActivityEntry[];
  adapters: AdapterStatusSnapshot[];
  snapshot_at_iso8601: string;
  // S246 / PR-239 — density rows below each aggregate tile.
  work_order_rows: WorkOrderRow[];
  low_stock_rows: LowStockItemRow[];
  pending_qa_rows: PendingQaRow[];
  eligible_dispatch_rows: EligibleDispatchRow[];
  pending_dispatch_rows: PendingDispatchRow[];
  today_invoice_rows: TodayInvoiceRow[];
  /** Untruncated count of issued-today invoices. The SPA renders a
   *  "+N more" footer when this exceeds `today_invoice_rows.length`. */
  today_invoice_total: number;
}

/** Fetch the Workshop dashboard bundle. No params — the whole tile
 * grid renders off one response. The SPA polls this on a 10s default
 * cadence (overridable via the `VITE_WORKSHOP_POLL_MS` env var, read
 * inside `Workshop.svelte`).
 *
 * S238 / PR-232 — when the Workshop demo-mode flag is on (5-tap
 * gesture on the page H2 sets it via `workshop-demo-mode`), short-
 * circuit to mock data instead of the real Tauri command. The
 * component never sees the branch; tile rendering is identical.
 * The check is per-call so a mid-poll toggle takes effect at the
 * next 10s tick. Both modules are statically imported because
 * `Workshop.svelte` consumes them too (scan/spotlight constants)
 * — they would land in the same chunk regardless. */
export async function getWorkshopDashboard(): Promise<WorkshopDashboard> {
  if (isDemoMode()) {
    return getMockDashboard();
  }
  return invoke<WorkshopDashboard>("get_workshop_dashboard");
}
