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
  /** PR-44ε / session-53 — currency on the list-row wire shape per
   * ADR-0037 §1.a + §3. The list-row formatter consumes this
   * field to pick the HUF-vs-EUR symbol + minor-unit interpretation
   * for `total_gross`; without it, an EUR invoice's cents would
   * render as forints (off by a factor of 100 + wrong symbol).
   * Pinned by `invoice_list_item_emits_currency` on the Rust side. */
  currency: Currency;
}

/** Possible derived states from `InvoiceTrace::derive_state` on the
 * backend. Kept in lockstep with that `&'static str` ladder per
 * ADR-0036 §2 — eleven labels, lifecycle-ordered. A state the
 * backend invents without a matching union member here renders as
 * the raw string but does not break the table; the `labelMeta`
 * helper in `./labels.ts` falls back to a muted "?" pill so the
 * silent miss is visible per CLAUDE.md rule 12. */
export type InvoiceState =
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

/** The single-invoice detail — shape mirrors
 * `serve::InvoiceDetailResponse`. */
export interface InvoiceDetail {
  invoice_id: string;
  sequence_number: number;
  fiscal_year: number;
  state: InvoiceState;
  total_gross: number | null;
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
}

/** `GET /health` response — `serve::HealthResponse`. */
export interface HealthResponse {
  ok: boolean;
  binary_hash: string;
  nav_xsd_version: string;
}

export async function health(): Promise<HealthResponse> {
  return invoke<HealthResponse>("health");
}

export async function listInvoices(): Promise<InvoiceListItem[]> {
  return invoke<InvoiceListItem[]>("list_invoices");
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
export interface IssueInvoiceRequest {
  customer: {
    taxNumber: string;
    name: string;
  };
  lines: Array<{
    description: string;
    quantity: number;
    unitPrice: number;
    vatRatePercent: number;
  }>;
  currency: Currency;
  /** Optional series code; backend defaults to `"INV-default"` when
   * omitted. Kept opt-in so the SPA form does not have to expose a
   * series-picker on the first cut. */
  series?: string;
}

/** PR-44ζ / session-59 — wire response body for `POST /invoices/issue`.
 * Mirrors `serve::IssueInvoiceResponse` on the backend. The SPA reads
 * `invoice_id` to open the detail modal at the just-issued invoice. */
export interface IssueInvoiceResponse {
  invoice_id: string;
  invoice_number: string;
  state: InvoiceState;
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

/** PR-58 / session-78 — typed shape for the backend's
 * `nav_upstream_fault` JSON body (HTTP 502). Returned by
 * `POST /invoices/:id/submit` when NAV's `tokenExchange` rejects the
 * request at the HTTP layer (signature mismatch, IP not whitelisted,
 * expired technical-user password, etc.). The `fault_code` /
 * `fault_message` pair is NAV's parsed diagnostic (Hungarian-localized
 * message when present); `raw_body_preview` is the first 500 chars of
 * the verbatim response body as a fallback when parsing did not find
 * a typed pair. Mirrors `serve::NavUpstreamFaultBody`. */
export interface NavUpstreamFault {
  error: "nav_upstream_fault";
  status: number;
  fault_code: string | null;
  fault_message: string | null;
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
      return parsed as NavUpstreamFault;
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

/** PR-47α / session-64 — POST the SPA's "Cancel invoice (storno)"
 * button to the backend's `/api/invoices/<id>/storno` route via the
 * matching Tauri command. No body — the backend resolves the
 * operator's original invoice JSON from the side-stored input.json
 * file written at issuance time per A174. Errors propagate as the
 * rejected promise (including the typed 409 body for precondition
 * mismatch); the caller renders the message inline (no toast
 * component per A157). */
export async function cancelInvoiceStorno(
  invoiceId: string,
): Promise<StornoInvoiceResponse> {
  return invoke<StornoInvoiceResponse>("cancel_invoice_storno", {
    invoiceId,
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
    taxNumber: string;
    name: string;
  };
  lines: Array<{
    description: string;
    quantity: number;
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
  tax_number: string;
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
  tax_number: string;
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
