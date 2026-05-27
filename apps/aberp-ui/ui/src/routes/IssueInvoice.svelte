<script lang="ts">
  // PR-44ζ / session-59 — Issue-invoice modal.
  //
  // The first MUTATION-route surface on the SPA (every prior screen
  // is read-only per ADR-0021 §Part B). Wraps the new
  // `POST /invoices/issue` route behind a native <dialog> modal,
  // matching `InvoiceDetail.svelte`'s posture per A157 (no toast
  // component; inline error rendering).
  //
  // Form fields (per the session-59 brief, surgical subset):
  //   - Supplier (name, taxNumber, address: country/postal/city/street)
  //   - Customer (name, taxNumber)
  //   - Currency (HUF / EUR dropdown)
  //   - Lines (1+) with description, quantity, unitPrice, vatRatePercent
  //
  // Deliberately NOT on the form (per CLAUDE.md rule 3, surgical):
  //   - Fulfillment date / payment due date / payment method — the
  //     backend's `nav_xml.rs` hardcodes these to issue_date and
  //     "TRANSFER" today; exposing form fields the backend silently
  //     ignores would violate rule 12. Future PR-44ζ.1 widens the
  //     input schema + render to support them.
  //   - Series picker — backend defaults to "INV-default"; the
  //     wire-shape supports overriding via `series?: string` but
  //     the form does not expose it on the first cut.
  //   - Customer address — backend's CustomerJson does not carry an
  //     address field today; widening that surface is named-deferred.
  //
  // On submit:
  //   1. Compose the wire body via `composeIssueInvoiceBody(form)`.
  //   2. POST via `issueInvoice(body)` Tauri command.
  //   3. On success, invoke `onIssued(invoice_id)` so the parent
  //      navigates the detail modal open on the just-issued invoice.
  //   4. On failure, render the error string inline (no toast).

  import {
    issueInvoice,
    listPartners,
    listSellerBanks,
    type Currency,
    type Partner,
    type SellerBankResponse,
  } from "../lib/api";
  import {
    composeIssueInvoiceBody,
    emptyForm,
    emptyLine,
    parseInvoicePreflightErrors,
    parseMissingSellerConfigError,
    targetForFieldPath,
    type InvoicePreflightErrorBody,
    type InvoicePreflightErrorItem,
    type IssueInvoiceFormState,
    type MissingSellerConfigError,
  } from "../lib/issue-invoice";
  import { buyerFieldsFromPartner } from "../lib/partners";
  import { buyerComboboxState } from "../lib/buyer-combobox";

  interface Props {
    /** Whether the modal is open. The parent toggles by reassigning
     * this boolean prop; `null`-vs-string would mirror the detail
     * modal's posture but the issue modal has no per-invocation
     * payload to carry, so a boolean is the simpler shape. */
    open: boolean;
    /** Invoked with the freshly-issued invoice id when the backend
     * returns 200. The parent uses this to navigate the detail
     * modal open at the just-issued invoice. */
    onIssued: (invoiceId: string) => void;
    /** Invoked when the operator closes the modal (ESC / backdrop /
     * Cancel button) without issuing. */
    onClose: () => void;
  }

  let { open, onIssued, onClose }: Props = $props();

  let dialogEl: HTMLDialogElement | null = $state(null);
  let form: IssueInvoiceFormState = $state(emptyForm());
  let submitState: "idle" | "submitting" | "error" = $state("idle");
  let submitError: string | null = $state(null);
  /** PR-74 / session-96 — saved-partners cache for the buyer combobox.
   * Lazy-loaded on first dialog open via `loadPartners()`; the
   * combobox filters this client-side rather than per-keystroke fetch.
   * `partnersLoaded` flips to `true` after the first successful load
   * so subsequent opens reuse the cached list (the partners list is
   * small in practice — operator-scale, not data-warehouse-scale). */
  let savedPartners: Partner[] = $state([]);
  let partnersLoaded = $state(false);
  /** PR-74 — combobox dropdown lifecycle. Open on focus + 3+ char
   * needle; closed by Escape, blur, or partner selection. The
   * `buyerComboboxState` helper computes `matches` from the cached
   * list + current `form.customerName` value. */
  let buyerDropdownOpen = $state(false);
  let buyerHighlight = $state(-1); // -1 = no row highlighted
  /** PR-50 / session-70 — when the backend's `400` body carries the
   * `missing_seller_config` discriminant, hold the typed shape so the
   * template can render the operator-actionable `config_path` +
   * `sample_path` hints instead of just the raw message. `null` for
   * every other error class (network, 500, plain 400). */
  let missingSellerConfig: MissingSellerConfigError | null = $state(null);
  /** PR-69 / session-91 — when the backend's `400` body carries the
   * `invoice_preflight_failed` discriminant, hold the typed body so
   * the template can render each error inline at its `field_path`
   * input. `null` for every other error class. */
  let preflightErrors: InvoicePreflightErrorBody | null = $state(null);

  /** PR-69 / session-91 — derived per-field error lookup keyed by the
   * field_path's routed target. Each input pulls its inline-error
   * payload from here by calling `customerErrorFor` /
   * `lineErrorFor`. Errors with an un-routable field_path fall through
   * to the general error block at the top of the form (so a
   * forward-compat variant never silently drops). */
  let customerErrors: { name: InvoicePreflightErrorItem | null; taxNumber: InvoicePreflightErrorItem | null } =
    $state({ name: null, taxNumber: null });
  let linesContainerError: InvoicePreflightErrorItem | null = $state(null);
  let lineErrors: Record<number, Partial<Record<"description" | "quantity" | "unitPrice" | "vatRatePercent", InvoicePreflightErrorItem>>> =
    $state({});
  let unroutedPreflightErrors: InvoicePreflightErrorItem[] = $state([]);

  // PR-73 / ADR-0040 §addendum — bank-account picker state.
  // `sellerBanks` is the full list from `GET /api/seller/banks`
  // (populated on first open via `loadSellerBanks()`); the picker
  // filters by current `form.currency`. `bankPickerError` is the
  // inline error item for the `bankAccountId` field path.
  let sellerBanks: SellerBankResponse[] = $state([]);
  let sellerBanksLoaded = $state(false);
  /** PR-74 — load-error state for the bank list. Distinct from
   * `sellerBanksLoaded = false` (which still means "loading"); this
   * surfaces when `listSellerBanks()` rejects so the renderer can
   * show an actionable hint instead of an indefinite loading
   * spinner. Closes the PR-73 footgun where a backend error left the
   * form unresponsive (CLAUDE.md rule 12 — fail loud). */
  let sellerBanksLoadError: string | null = $state(null);
  let bankPickerError: InvoicePreflightErrorItem | null = $state(null);
  /** Banks for the form's current currency, in declaration order. */
  let banksForCurrency = $derived(
    sellerBanks.filter((b) => b.currency === form.currency),
  );
  /** Default bank for the current currency (the entry with
   * `is_default: true`), or `null` if no entry exists for that
   * currency. Drives the auto-pre-population on currency change. */
  let defaultBankForCurrency = $derived(
    banksForCurrency.find((b) => b.is_default) ?? null,
  );

  async function loadSellerBanks() {
    try {
      const response = await listSellerBanks();
      sellerBanks = response.banks;
      sellerBanksLoaded = true;
      sellerBanksLoadError = null;
      // After load, pre-populate the picker with the default for the
      // current currency if the operator hasn't already chosen one.
      if (form.bankAccountId === null && defaultBankForCurrency !== null) {
        form = { ...form, bankAccountId: defaultBankForCurrency.id };
      }
    } catch (err: unknown) {
      // PR-74 — surface the failure instead of looping back to
      // "Loading…" indefinitely (the PR-73 footgun). The renderer
      // shows a "Could not load bank accounts" affordance with a
      // Retry button so the operator can recover without closing
      // the dialog.
      sellerBanks = [];
      sellerBanksLoaded = true;
      sellerBanksLoadError = err instanceof Error ? err.message : String(err);
    }
  }

  /** PR-74 / session-96 — lazy load the saved-partners list on first
   * dialog open. Mirrors `loadSellerBanks`'s posture: one fetch per
   * SPA session, cached for subsequent opens. The combobox filters
   * client-side from `savedPartners`. */
  async function loadPartners() {
    try {
      const response = await listPartners();
      savedPartners = response;
      partnersLoaded = true;
    } catch (_err) {
      // Non-fatal: a partners-fetch failure still lets the operator
      // type a free-text buyer name. The dropdown simply never
      // surfaces matches (operator types-through to one-off).
      savedPartners = [];
      partnersLoaded = true;
    }
  }

  /** PR-74 — derived combobox state from the cached partners +
   * current input value. The renderer reads `matches` +
   * `shouldShowDropdown` to render the dropdown below the input. */
  let comboboxView = $derived(
    buyerComboboxState({
      needle: form.customerName,
      savedPartners,
    }),
  );

  // When the currency changes, re-default the picker to that
  // currency's `is_default` entry. If no entry exists for the new
  // currency, blank the selection — the no-default-for-currency
  // affordance below renders the link-to-Tenant-Settings hint.
  $effect(() => {
    // Touch form.currency so the effect re-runs on change (svelte 5
    // tracks the reads inside the effect closure — referencing the
    // field here is enough; no local binding required).
    form.currency;
    if (!sellerBanksLoaded) return;
    const fresh = defaultBankForCurrency;
    if (fresh) {
      // Only re-default when the operator hasn't picked an entry for
      // this currency (or picked one whose currency changed away).
      const current = sellerBanks.find((b) => b.id === form.bankAccountId);
      if (current === undefined || current.currency !== form.currency) {
        form = { ...form, bankAccountId: fresh.id };
      }
    } else {
      // No entry for this currency — blank the picker; the inline
      // affordance below renders the missing-bank hint.
      form = { ...form, bankAccountId: null };
    }
  });

  function routePreflightErrors(body: InvoicePreflightErrorBody) {
    customerErrors = { name: null, taxNumber: null };
    linesContainerError = null;
    lineErrors = {};
    unroutedPreflightErrors = [];
    bankPickerError = null;
    for (const item of body.errors) {
      const target = targetForFieldPath(item.field_path);
      if (!target) {
        unroutedPreflightErrors.push(item);
        continue;
      }
      switch (target.kind) {
        case "customer":
          customerErrors[target.field] = item;
          break;
        case "lines":
          linesContainerError = item;
          break;
        case "bankAccountId":
          bankPickerError = item;
          break;
        case "line": {
          const bucket = lineErrors[target.lineIndex] ?? {};
          bucket[target.field] = item;
          lineErrors[target.lineIndex] = bucket;
          break;
        }
      }
    }
  }

  function clearPreflightErrors() {
    preflightErrors = null;
    customerErrors = { name: null, taxNumber: null };
    linesContainerError = null;
    lineErrors = {};
    unroutedPreflightErrors = [];
    bankPickerError = null;
  }

  function totalPreflightErrorCount(): number {
    return preflightErrors?.errors.length ?? 0;
  }

  // Sync the native <dialog>'s open state with the `open` prop.
  // `showModal()` and `close()` are imperative; the prop is the
  // declarative source of truth.
  $effect(() => {
    if (!dialogEl) return;
    if (open) {
      if (!dialogEl.open) dialogEl.showModal();
      // PR-73 — lazy-load seller banks on first open (subsequent opens
      // reuse the cached list). The list is small (≤ ~6 entries per
      // tenant in practice) so a periodic re-fetch is unnecessary.
      if (!sellerBanksLoaded) {
        void loadSellerBanks();
      }
      // PR-74 — lazy-load partners on first open so the buyer
      // combobox has its candidate list ready by the time the
      // operator focuses the input.
      if (!partnersLoaded) {
        void loadPartners();
      }
    } else {
      if (dialogEl.open) dialogEl.close();
    }
  });

  function resetForm() {
    form = emptyForm();
    submitState = "idle";
    submitError = null;
    missingSellerConfig = null;
    clearPreflightErrors();
    buyerDropdownOpen = false;
    buyerHighlight = -1;
  }

  /** PR-74 — operator clicked / Enter-selected a saved partner row.
   * Auto-fills the customer fields from the partner and closes the
   * dropdown. `legal_name` is the regulatory-compliant string NAV
   * expects on the printed invoice (per `buyerFieldsFromPartner`);
   * the input now displays that value rather than `display_name`
   * because the input IS the wire-bound `form.customerName`. */
  function pickPartner(partner: Partner) {
    const fields = buyerFieldsFromPartner(partner);
    form = {
      ...form,
      customerName: fields.customerName,
      customerTaxNumber: fields.customerTaxNumber,
    };
    buyerDropdownOpen = false;
    buyerHighlight = -1;
  }

  function onBuyerInput() {
    buyerDropdownOpen = true;
    buyerHighlight = comboboxView.matches.length > 0 ? 0 : -1;
  }

  function onBuyerFocus() {
    if (comboboxView.shouldShowDropdown) {
      buyerDropdownOpen = true;
      if (comboboxView.matches.length > 0 && buyerHighlight < 0) {
        buyerHighlight = 0;
      }
    }
  }

  function onBuyerBlur() {
    // Delay closing so a mousedown on a dropdown row still fires its
    // handler before the dropdown disappears (mirror of the PR-54
    // PartnerTypeahead posture).
    setTimeout(() => {
      buyerDropdownOpen = false;
    }, 150);
  }

  function onBuyerKeyDown(event: KeyboardEvent) {
    const rows = comboboxView.matches.length;
    switch (event.key) {
      case "ArrowDown":
        if (rows === 0) return;
        event.preventDefault();
        buyerDropdownOpen = true;
        buyerHighlight = (buyerHighlight + 1) % rows;
        break;
      case "ArrowUp":
        if (rows === 0) return;
        event.preventDefault();
        buyerDropdownOpen = true;
        buyerHighlight = (buyerHighlight - 1 + rows) % rows;
        break;
      case "Enter":
        // Only intercept Enter when the dropdown has a highlighted
        // selectable row — otherwise let the form-default Enter
        // behaviour through (and the operator can submit a
        // one-off-named invoice).
        if (
          buyerDropdownOpen &&
          buyerHighlight >= 0 &&
          buyerHighlight < rows
        ) {
          event.preventDefault();
          pickPartner(comboboxView.matches[buyerHighlight]);
        }
        break;
      case "Escape":
        if (buyerDropdownOpen) {
          event.preventDefault();
          buyerDropdownOpen = false;
          buyerHighlight = -1;
        }
        break;
      default:
        break;
    }
  }

  function addLine() {
    form = { ...form, lines: [...form.lines, emptyLine()] };
  }

  function removeLine(index: number) {
    // Refuse to delete the last line — the form must always have
    // one line for the backend's `at least one line item is required`
    // pre-validation to pass. The button is disabled in the markup
    // when `form.lines.length === 1` so the operator gets a visual
    // cue too.
    if (form.lines.length <= 1) return;
    form = {
      ...form,
      lines: form.lines.filter((_, i) => i !== index),
    };
  }

  async function handleSubmit(event: Event) {
    event.preventDefault();
    submitState = "submitting";
    submitError = null;
    missingSellerConfig = null;
    clearPreflightErrors();
    try {
      const body = composeIssueInvoiceBody(form);
      const response = await issueInvoice(body);
      submitState = "idle";
      // Reset the form so the next opening starts fresh; the parent
      // owns the open/close lifecycle.
      resetForm();
      onIssued(response.invoice_id);
    } catch (err: unknown) {
      submitState = "error";
      const raw = err instanceof Error ? err.message : String(err);
      // PR-69 / session-91 — try the preflight 400 shape FIRST (it's
      // the most common operator-correctable failure). PR-50's
      // `missing_seller_config` is a sibling 400 with a different
      // discriminant; we try both, then fall back to the raw string.
      const preflight = parseInvoicePreflightErrors(raw);
      if (preflight) {
        preflightErrors = preflight;
        routePreflightErrors(preflight);
      } else {
        // PR-50 / session-70 — when the backend's 400 body carries the
        // typed `missing_seller_config` discriminant, populate the
        // structured state so the template renders the
        // config_path + sample_path hints. Otherwise fall back to
        // displaying the raw error string verbatim.
        missingSellerConfig = parseMissingSellerConfigError(raw);
      }
      submitError = raw;
    }
  }

  function handleDialogClose() {
    // Reset form on close so an aborted issuance does not leak
    // operator-typed values into the next opening — the same
    // posture InvoiceDetail.svelte uses for its per-invocation
    // state (download error, expanded payloads).
    resetForm();
    onClose();
  }

  function handleDialogClick(event: MouseEvent) {
    if (event.target === dialogEl) {
      dialogEl?.close();
    }
  }

  // Currency dropdown options. The backend's `Currency` Deserialize
  // accepts exactly two strings per `rename_all = "UPPERCASE"`;
  // mirror that closed vocab here so a future widening (ADR-0037 §5)
  // surfaces at both ends.
  const CURRENCY_OPTIONS: Currency[] = ["HUF", "EUR"];
</script>

<dialog
  bind:this={dialogEl}
  class="issue"
  onclose={handleDialogClose}
  onclick={handleDialogClick}
  aria-label="Issue new invoice"
>
  <form class="issue-frame" onsubmit={handleSubmit}>
    <header class="issue-head">
      <h2>New invoice</h2>
      <button
        type="button"
        class="quiet-button"
        onclick={() => dialogEl?.close()}
        aria-label="Cancel issuance"
      >
        Cancel
      </button>
    </header>

    {#if submitState === "error" && submitError}
      {#if preflightErrors}
        <div class="error error-typed" role="alert" data-testid="preflight-summary">
          <p class="error-summary">
            {preflightErrors.errors.length} validation
            {preflightErrors.errors.length === 1 ? "issue" : "issues"} —
            fix the highlighted fields below and re-submit.
          </p>
          {#each unroutedPreflightErrors as item (item.field_path + item.kind)}
            <p class="error-path">
              {item.field_path}: {item.message_hu}
            </p>
            <p class="error-hint">{item.message_en}</p>
          {/each}
        </div>
      {:else if missingSellerConfig}
        <div class="error error-typed" role="alert">
          <p class="error-summary">{missingSellerConfig.message}</p>
          <p class="error-hint">
            Per-tenant config home (PR-51 will route this through the
            wizard):
          </p>
          <p class="error-path">{missingSellerConfig.config_path}</p>
          <p class="error-hint">Template to copy from:</p>
          <p class="error-path">{missingSellerConfig.sample_path}</p>
        </div>
      {:else}
        <p class="error" role="alert">{submitError}</p>
      {/if}
    {/if}

    <fieldset>
      <legend>Buyer</legend>
      <!-- PR-74 / session-96 — single-input buyer combobox. Replaces
           the PR-54 two-input posture (a separate `Search saved
           partners` typeahead above a `Name (auto-filled)` input)
           per operator feedback that the two-field UX was awkward
           and the typeahead's discoverability suffered. The input is
           wire-bound to `form.customerName`; the dropdown surfaces
           saved-partner matches as the operator types. Clicking a
           row auto-fills the ADÓSZÁM field below; typing past 3+
           chars without selecting flows through as a one-off
           buyer-name on submit. -->
      <label class="buyer-combobox">
        <span>Buyer name</span>
        <input
          type="text"
          bind:value={form.customerName}
          required
          role="combobox"
          autocomplete="off"
          spellcheck="false"
          placeholder="Type to search saved partners or enter a one-off name…"
          oninput={onBuyerInput}
          onfocus={onBuyerFocus}
          onblur={onBuyerBlur}
          onkeydown={onBuyerKeyDown}
          aria-autocomplete="list"
          aria-expanded={buyerDropdownOpen && comboboxView.shouldShowDropdown}
          aria-controls="buyer-combobox-listbox"
          class:input-invalid={customerErrors.name !== null}
          aria-invalid={customerErrors.name !== null}
          data-testid="customer-name-input"
        />
        {#if buyerDropdownOpen && comboboxView.shouldShowDropdown}
          <ul
            id="buyer-combobox-listbox"
            class="buyer-dropdown"
            role="listbox"
            data-testid="buyer-combobox-dropdown"
          >
            {#if comboboxView.matches.length === 0}
              <li class="buyer-dropdown__hint" aria-live="polite" data-testid="buyer-combobox-no-match">
                No saved partner matches — typed name will be used as-is.
              </li>
            {/if}
            {#each comboboxView.matches as match, i (match.id)}
              <li
                class="buyer-dropdown__row"
                role="option"
                aria-selected={i === buyerHighlight}
                data-highlight={i === buyerHighlight}
                data-testid={`buyer-combobox-row-${i}`}
                onmousedown={(e) => {
                  // mousedown rather than click so the input's blur
                  // (which fires before click) doesn't close the
                  // dropdown first.
                  e.preventDefault();
                  pickPartner(match);
                }}
              >
                <span class="buyer-dropdown__name">{match.display_name}</span>
                <span class="buyer-dropdown__hint-meta">
                  ({match.legal_name}, {match.tax_number})
                </span>
              </li>
            {/each}
          </ul>
        {/if}
        {#if customerErrors.name}
          <p class="inline-error" data-testid="customer-name-error" data-kind={customerErrors.name.kind}>
            <span class="inline-error-hu">{customerErrors.name.message_hu}</span>
            <span class="inline-error-en">{customerErrors.name.message_en}</span>
          </p>
        {/if}
      </label>
      <label>
        <span>ADÓSZÁM</span>
        <input
          type="text"
          bind:value={form.customerTaxNumber}
          required
          placeholder="87654321-2-13"
          class:input-invalid={customerErrors.taxNumber !== null}
          aria-invalid={customerErrors.taxNumber !== null}
          data-testid="customer-tax-input"
        />
        {#if customerErrors.taxNumber}
          <p class="inline-error" data-testid="customer-tax-error" data-kind={customerErrors.taxNumber.kind}>
            <span class="inline-error-hu">{customerErrors.taxNumber.message_hu}</span>
            <span class="inline-error-en">{customerErrors.taxNumber.message_en}</span>
          </p>
        {/if}
      </label>
    </fieldset>

    <fieldset>
      <legend>Currency</legend>
      <label>
        <span>Currency</span>
        <select bind:value={form.currency}>
          {#each CURRENCY_OPTIONS as option (option)}
            <option value={option}>{option}</option>
          {/each}
        </select>
      </label>
      {#if form.currency === "EUR"}
        <p class="hint">
          The MNB exchange rate will be fetched at issuance per
          ADR-0037 §2.b (with D-1 walk-back).
        </p>
      {/if}
    </fieldset>

    <!-- PR-73 / ADR-0040 §addendum — bank-account picker. Filtered
         to the form's currency; defaults to the currency's
         `is_default` entry. The no-default-for-currency state
         renders the "navigate to Tenant Settings" affordance per
         the brief. -->
    <fieldset>
      <legend>Bank account</legend>
      {#if !sellerBanksLoaded}
        <p class="hint" data-testid="bank-picker-loading">Loading bank accounts…</p>
      {:else if sellerBanksLoadError !== null}
        <!-- PR-74 — explicit load-error affordance so a backend
             failure does not leave the form stuck on "Loading…".
             The Retry button re-issues `loadSellerBanks()` so the
             operator can recover without closing the dialog. -->
        <p class="inline-error" data-testid="bank-picker-load-error">
          <span class="inline-error-hu">
            A bankszámlák betöltése sikertelen.
          </span>
          <span class="inline-error-en">
            Could not load bank accounts: {sellerBanksLoadError}
          </span>
        </p>
        <button
          type="button"
          class="quiet-button"
          onclick={() => {
            sellerBanksLoaded = false;
            sellerBanksLoadError = null;
            void loadSellerBanks();
          }}
          data-testid="bank-picker-retry"
        >
          Retry
        </button>
      {:else if banksForCurrency.length === 0}
        <p class="inline-error" data-testid="bank-picker-empty-for-currency">
          <span class="inline-error-hu">
            Nincs konfigurált bankszámla a(z) {form.currency} pénznemhez.
          </span>
          <span class="inline-error-en">
            No bank account configured for {form.currency}.
          </span>
        </p>
        <p class="hint">
          Add one in
          <a href="#/settings" data-testid="bank-picker-settings-link">
            Tenant Settings → Bank accounts
          </a>
          and re-open this form.
        </p>
      {:else}
        <label>
          <span>Pay to</span>
          <select
            bind:value={form.bankAccountId}
            class:input-invalid={bankPickerError !== null}
            aria-invalid={bankPickerError !== null}
            data-testid="bank-picker-select"
          >
            {#each banksForCurrency as bank (bank.id)}
              <option value={bank.id}>
                {bank.bank_name} — {bank.account_number}
                {bank.is_default ? "(default)" : ""}
              </option>
            {/each}
          </select>
        </label>
        {#if bankPickerError}
          <p class="inline-error" data-testid="bank-picker-error" data-kind={bankPickerError.kind}>
            <span class="inline-error-hu">{bankPickerError.message_hu}</span>
            <span class="inline-error-en">{bankPickerError.message_en}</span>
          </p>
        {/if}
      {/if}
    </fieldset>

    <fieldset>
      <legend>Line items</legend>
      {#if linesContainerError}
        <p class="inline-error" data-testid="lines-container-error" data-kind={linesContainerError.kind}>
          <span class="inline-error-hu">{linesContainerError.message_hu}</span>
          <span class="inline-error-en">{linesContainerError.message_en}</span>
        </p>
      {/if}
      {#each form.lines as line, index (index)}
        <div class="line">
          <label class="wide">
            <span>Description</span>
            <input
              type="text"
              bind:value={line.description}
              required
              class:input-invalid={lineErrors[index]?.description !== undefined}
              aria-invalid={lineErrors[index]?.description !== undefined}
              data-testid={`line-${index}-description-input`}
            />
          </label>
          <label class="narrow">
            <span>Qty</span>
            <input
              type="number"
              min="1"
              step="1"
              bind:value={line.quantity}
              required
              class:input-invalid={lineErrors[index]?.quantity !== undefined}
              aria-invalid={lineErrors[index]?.quantity !== undefined}
              data-testid={`line-${index}-quantity-input`}
            />
          </label>
          <label class="narrow">
            <span>Unit price</span>
            <input
              type="number"
              min="0"
              step="1"
              bind:value={line.unitPriceMinor}
              required
              class:input-invalid={lineErrors[index]?.unitPrice !== undefined}
              aria-invalid={lineErrors[index]?.unitPrice !== undefined}
              data-testid={`line-${index}-unit-price-input`}
            />
          </label>
          <label class="narrow">
            <span>VAT %</span>
            <input
              type="number"
              min="0"
              max="100"
              step="1"
              bind:value={line.vatRatePercent}
              required
              class:input-invalid={lineErrors[index]?.vatRatePercent !== undefined}
              aria-invalid={lineErrors[index]?.vatRatePercent !== undefined}
              data-testid={`line-${index}-vat-input`}
            />
          </label>
          <button
            type="button"
            class="quiet-button line-remove"
            onclick={() => removeLine(index)}
            disabled={form.lines.length <= 1}
            aria-label={`Remove line ${index + 1}`}
            title={form.lines.length <= 1
              ? "At least one line is required"
              : `Remove line ${index + 1}`}
          >
            ✕
          </button>
        </div>
        {#if lineErrors[index]}
          <div class="line-errors" data-testid={`line-${index}-errors`}>
            {#each Object.entries(lineErrors[index]) as [field, item] (field)}
              <p class="inline-error" data-testid={`line-${index}-${field}-error`} data-kind={item.kind}>
                <span class="inline-error-hu">{item.message_hu}</span>
                <span class="inline-error-en">{item.message_en}</span>
              </p>
            {/each}
          </div>
        {/if}
      {/each}
      <button type="button" class="quiet-button" onclick={addLine}>
        + Add line
      </button>
    </fieldset>

    <footer class="issue-foot">
      <button
        type="submit"
        class="quiet-button primary"
        disabled={submitState === "submitting"}
      >
        {#if submitState === "submitting"}
          <span aria-hidden="true">…</span> Issuing
        {:else if totalPreflightErrorCount() > 0}
          Issue invoice
          <span class="preflight-badge" data-testid="submit-preflight-badge">
            ({totalPreflightErrorCount()} {totalPreflightErrorCount() === 1 ? "issue" : "issues"})
          </span>
        {:else}
          Issue invoice
        {/if}
      </button>
    </footer>
  </form>
</dialog>

<style>
  dialog.issue {
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-base);
    color: var(--color-text-primary);
    padding: 0;
    max-width: 90vw;
    max-height: 90vh;
    width: 720px;
    overflow: hidden;
  }

  dialog.issue::backdrop {
    background: rgba(0, 0, 0, 0.5);
  }

  .issue-frame {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    max-height: 90vh;
    overflow: auto;
    padding: var(--space-4) var(--space-5);
    animation: aberp-fade-in var(--motion-fade-in) both;
  }

  .issue-head {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
  }

  h2 {
    margin: 0;
    font-size: var(--type-size-lg);
    font-weight: 500;
    color: var(--color-text-strong);
  }

  fieldset {
    border: 1px solid var(--color-surface-divider);
    padding: var(--space-3);
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }

  legend {
    text-transform: uppercase;
    font-size: var(--type-size-xs);
    letter-spacing: 0.06em;
    color: var(--color-text-secondary);
    padding: 0 var(--space-2);
  }

  label {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    font-size: var(--type-size-sm);
    color: var(--color-text-secondary);
    flex: 1 1 auto;
  }

  label.narrow {
    flex: 0 0 8ch;
  }

  label.wide {
    flex: 2 1 auto;
  }

  .line {
    display: flex;
    gap: var(--space-2);
    align-items: flex-end;
  }

  input[type="text"],
  input[type="number"],
  select {
    background: var(--color-surface-raised);
    color: var(--color-text-strong);
    border: 1px solid var(--color-surface-divider);
    padding: var(--space-1) var(--space-2);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
  }

  input:focus-visible,
  select:focus-visible {
    outline: 1px solid var(--color-text-muted);
    outline-offset: 1px;
    border-color: var(--color-text-muted);
  }

  .quiet-button {
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    border: 1px solid var(--color-surface-divider);
    padding: var(--space-1) var(--space-3);
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    cursor: pointer;
    transition: color var(--motion-fade-in);
  }

  .quiet-button:hover:not(:disabled) {
    color: var(--color-text-strong);
  }

  .quiet-button:disabled {
    cursor: not-allowed;
    opacity: 0.5;
  }

  /* `.primary` is a quiet emphasis — the dense ADR-0017 aesthetic
   * keeps the chrome quiet; the primary button is just slightly
   * stronger than `.quiet-button` to mark "this is the action". */
  .quiet-button.primary {
    color: var(--color-text-strong);
    border-color: var(--color-text-muted);
  }

  .line-remove {
    flex: 0 0 auto;
    align-self: flex-end;
  }

  .issue-foot {
    display: flex;
    justify-content: flex-end;
    gap: var(--space-2);
  }

  .hint {
    margin: 0;
    color: var(--color-text-muted);
    font-size: var(--type-size-xs);
    font-style: italic;
  }

  .error {
    color: var(--color-signal-negative);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
    margin: 0;
    white-space: pre-wrap;
    word-break: break-word;
  }

  /* PR-50 / session-70 — typed `missing_seller_config` block. Same
   * negative-signal colour as the plain inline error, with extra
   * structure so the config_path + sample_path hints render as
   * monospaced "you can copy this" lines. */
  .error-typed {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }

  .error-summary {
    color: var(--color-signal-negative);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
    margin: 0;
    white-space: pre-wrap;
    word-break: break-word;
  }

  .error-hint {
    color: var(--color-text-secondary);
    font-family: var(--type-family-body);
    font-size: var(--type-size-xs);
    margin: 0;
  }

  .error-path {
    color: var(--color-text-strong);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
    margin: 0;
    word-break: break-all;
  }

  /* PR-69 / session-91 — pre-issuance preflight inline-error surface
   * per ADR-0038. `.input-invalid` paints the offending input's
   * border in the negative-signal colour so the operator can scan
   * the form for highlighted fields; `.inline-error` renders the
   * Hungarian + English messages beneath the input. */
  input.input-invalid {
    border-color: var(--color-signal-negative);
    outline-color: var(--color-signal-negative);
  }

  .inline-error {
    display: flex;
    flex-direction: column;
    gap: 2px;
    margin: var(--space-1) 0 0 0;
    font-family: var(--type-family-mono);
    font-size: var(--type-size-xs);
  }

  .inline-error-hu {
    color: var(--color-signal-negative);
  }

  .inline-error-en {
    color: var(--color-text-muted);
    font-style: italic;
  }

  .line-errors {
    margin-bottom: var(--space-2);
  }

  /* PR-69 / session-91 — Submit-button count badge surfaces the
   * unresolved-count when preflight rejected the request. */
  .preflight-badge {
    color: var(--color-signal-negative);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-xs);
    margin-left: var(--space-1);
  }

  /* PR-74 / session-96 — buyer combobox dropdown styles. Mirror of
   * the PR-54 PartnerTypeahead dropdown posture (absolute-positioned
   * below the input, raised surface, divider rows) so the operator
   * sees a familiar affordance — only difference is the parent label
   * is `position: relative` instead of a wrapping <div>. */
  label.buyer-combobox {
    position: relative;
  }

  .buyer-dropdown {
    position: absolute;
    top: 100%;
    left: 0;
    right: 0;
    z-index: 10;
    list-style: none;
    margin: 0;
    padding: 0;
    background: var(--color-surface-raised);
    border: 1px solid var(--color-surface-divider);
    border-radius: 4px;
    max-height: 320px;
    overflow-y: auto;
    box-shadow: 0 4px 12px rgba(0, 0, 0, 0.2);
  }

  .buyer-dropdown__row {
    padding: var(--space-2) var(--space-3);
    cursor: pointer;
    display: flex;
    flex-direction: column;
    gap: 2px;
    border-bottom: 1px solid var(--color-surface-divider);
  }

  .buyer-dropdown__row:last-child {
    border-bottom: none;
  }

  .buyer-dropdown__row[data-highlight="true"] {
    background: var(--color-surface-divider);
  }

  .buyer-dropdown__name {
    color: var(--color-text-strong);
    font-size: var(--type-size-sm);
    font-weight: 500;
  }

  .buyer-dropdown__hint-meta {
    color: var(--color-text-muted);
    font-size: var(--type-size-xs);
    font-family: var(--type-family-mono);
  }

  .buyer-dropdown__hint {
    padding: var(--space-2) var(--space-3);
    color: var(--color-text-muted);
    font-size: var(--type-size-xs);
    font-style: italic;
  }
</style>
