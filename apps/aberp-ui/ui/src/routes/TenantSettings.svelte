<script lang="ts">
  // PR-53 / session-73 — Tenant Settings page. Reads the persisted
  // seller.toml via GET /api/seller-info, lets the operator edit any
  // field, POSTs the updated body back via the existing
  // POST /api/setup-seller-info route (the wizard's write surface
  // already handles overwrite semantics).
  //
  // Mirrors `SellerConfigWizard.svelte`'s field shape exactly — same
  // composer + validator from `seller-config.ts`. The difference is
  // operator UX: the wizard is one-shot first-run; this page is
  // view-then-edit with the saved values pre-filled and a brief
  // "Saved" indicator on success (no navigation away).
  //
  // PR-72 / session-94 — adds the "Bank accounts" subsection per the
  // multi-bank initiative (ADR-0040 §addendum). The legacy single-
  // slot bank fields in the right-hand column remain LIVE because the
  // existing PDF renderer + NAV body still consume them (PR-D
  // territory); the new subsection is additive and writes to the
  // `[[seller.banks]]` block via the dedicated /api/seller/banks
  // routes. PR-D will swap the legacy single-slot fields out.

  import { onMount } from "svelte";
  import {
    createSellerBank,
    deleteSellerBank,
    getSellerInfo,
    getSellerNumbering,
    getSmtpConfig,
    listSellerBanks,
    putSellerNumbering,
    putSmtpConfig,
    setDefaultSellerBank,
    setupSellerInfo,
    testSmtpConnection,
    updateSellerBank,
    type SellerBankResponse,
    type SmtpConfigGetResponse,
    type SmtpSecurity,
    type SmtpTestOutcome,
  } from "../lib/api";
  import {
    composeSellerConfigBody,
    DEFAULT_SELLER_CONFIG_FORM,
    parseSetupSellerInfoErrorBody,
    validateSellerConfig,
    type SellerConfigForm,
  } from "../lib/seller-config";
  import {
    composeSellerBankInputs,
    emptySellerBankForm,
    formFromSellerBank,
    groupSellerBanksByCurrency,
    parseSellerBankValidationError,
    validateSellerBankForm,
    type SellerBankFormState,
  } from "../lib/seller-banks";
  import {
    defaultTemplate,
    errorMessage as numberingErrorMessage,
    moveSegmentDown,
    moveSegmentUp,
    removeSegment,
    renderTemplateForBuild,
    validateTemplate,
    type NumberingSegment,
    type NumberingTemplate,
  } from "../lib/invoice-numbering";
  import { formFromSellerInfo } from "../lib/tenant-settings";

  // S165 — `isProductionBuild` comes from `GET /health` (threaded down
  // by App.svelte). Default `false` (dev/test) so the preview shows the
  // `TEST-` prefix until health resolves — the dev-safe default.
  let { isProductionBuild = false }: { isProductionBuild?: boolean } =
    $props();

  let form: SellerConfigForm = $state({ ...DEFAULT_SELLER_CONFIG_FORM });
  let loading = $state(true);
  let loadError: string | null = $state(null);
  let submitting = $state(false);
  let submitError: string | null = $state(null);
  let saved = $state(false);
  let fieldErrors: Record<string, string> = $state({});

  let validation = $derived(validateSellerConfig(form));

  // PR-72 — Bank-accounts subsection state.
  let banks: SellerBankResponse[] = $state([]);
  let banksLoading = $state(true);
  let banksLoadError: string | null = $state(null);
  let bankModalOpen = $state(false);
  let bankModalMode: "create" | "edit" = $state("create");
  let bankModalForm: SellerBankFormState = $state(emptySellerBankForm());
  let bankModalEditingId: string | null = $state(null);
  let bankModalEditingIsDefault = $state(false);
  let bankModalSubmitting = $state(false);
  let bankModalSubmitError: string | null = $state(null);
  let bankModalFieldErrors: Record<string, string> = $state({});
  let bankRowError: string | null = $state(null);

  let bankModalValidation = $derived(validateSellerBankForm(bankModalForm));
  let banksGrouped = $derived(groupSellerBanksByCurrency(banks));

  // PR-89 — Invoice numbering subsection state. The page loads the
  // current template via GET /api/seller/numbering on mount and the
  // operator builds against a local working copy; "Save" PUTs the
  // composed body back. Live preview renders against the current
  // calendar year + start_value so the operator sees exactly what
  // "next invoice will be" before saving.
  let numbering: NumberingTemplate = $state(defaultTemplate());
  let numberingLoading = $state(true);
  let numberingLoadError: string | null = $state(null);
  let numberingSubmitting = $state(false);
  let numberingSubmitError: string | null = $state(null);
  let numberingSaved = $state(false);

  // PR-92 / ADR-0047 — SMTP subsection state. Loaded on mount via
  // GET /api/smtp-config; the keychain password is NEVER carried
  // back to the SPA — the backend reports a `passwordSet` boolean.
  // The operator may type a NEW password to rotate (blank means
  // "leave existing keychain entry untouched").
  interface SmtpForm {
    host: string;
    port: number;
    fromAddress: string;
    fromDisplayName: string;
    username: string;
    security: SmtpSecurity;
    attachXml: boolean;
    password: string;
  }
  let smtp: SmtpForm = $state({
    host: "",
    port: 587,
    fromAddress: "",
    fromDisplayName: "",
    username: "",
    security: "StartTls",
    attachXml: false,
    password: "",
  });
  let smtpLoading = $state(true);
  let smtpLoadError: string | null = $state(null);
  let smtpPasswordSet = $state(false);
  let smtpSubmitting = $state(false);
  let smtpSubmitError: string | null = $state(null);
  let smtpSaved = $state(false);
  // PR-98 — SMTP "Test connection" state. The button runs the TLS
  // handshake + AUTH + NOOP via the backend without persisting
  // anything; outcome surfaces as an inline banner.
  let smtpTesting = $state(false);
  let smtpTestOutcome: SmtpTestOutcome | null = $state(null);
  let smtpTestError: string | null = $state(null);
  let numberingLiteralDraft = $state("");
  let numberingValidation = $derived(validateTemplate(numbering));
  let numberingPreview = $derived.by(() => {
    const err = validateTemplate(numbering);
    if (err !== null) return "—";
    const year = new Date().getFullYear();
    // S165 — preview the BUILD-rendered shape so it matches what the
    // backend emits: `TEST-…` on dev/test builds, unprefixed on prod.
    return renderTemplateForBuild(
      numbering,
      year,
      numbering.start_value,
      isProductionBuild,
    );
  });
  let numberingPreviewNextYear = $derived.by(() => {
    const err = validateTemplate(numbering);
    if (err !== null) return null;
    if (numbering.reset_policy !== "on_year_change") return null;
    return renderTemplateForBuild(
      numbering,
      new Date().getFullYear() + 1,
      numbering.start_value,
      isProductionBuild,
    );
  });

  onMount(() => {
    void loadSellerInfo();
    void loadBanks();
    void loadNumbering();
    void loadSmtpConfig();
  });

  async function loadSellerInfo() {
    loading = true;
    loadError = null;
    try {
      const response = await getSellerInfo();
      form = formFromSellerInfo(response);
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : String(err);
      loadError = message;
    } finally {
      loading = false;
    }
  }

  async function onSubmit(event: Event) {
    event.preventDefault();
    submitError = null;
    fieldErrors = {};
    saved = false;
    if (!validation.ok) {
      return;
    }
    submitting = true;
    try {
      const body = composeSellerConfigBody(form);
      await setupSellerInfo(body);
      saved = true;
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : String(err);
      const typed = parseSetupSellerInfoErrorBody(message);
      if (typed !== null) {
        const next: Record<string, string> = {};
        for (const f of typed.fields) {
          next[f.field] = f.message;
        }
        fieldErrors = next;
        submitError = "Some fields need attention — see the inline messages.";
      } else {
        submitError = message;
      }
    } finally {
      submitting = false;
    }
  }

  function fieldError(name: string, clientSide: string | null): string | null {
    if (fieldErrors[name] !== undefined) {
      return fieldErrors[name];
    }
    return clientSide;
  }

  // ── PR-72 / session-94 — bank-accounts subsection handlers ──────────

  async function loadBanks() {
    banksLoading = true;
    banksLoadError = null;
    try {
      const response = await listSellerBanks();
      banks = response.banks;
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : String(err);
      banksLoadError = message;
    } finally {
      banksLoading = false;
    }
  }

  function openAddBankModal() {
    bankModalMode = "create";
    bankModalForm = emptySellerBankForm();
    bankModalEditingId = null;
    bankModalEditingIsDefault = false;
    bankModalSubmitError = null;
    bankModalFieldErrors = {};
    bankModalOpen = true;
  }

  function openEditBankModal(bank: SellerBankResponse) {
    bankModalMode = "edit";
    bankModalForm = formFromSellerBank(bank);
    bankModalEditingId = bank.id;
    bankModalEditingIsDefault = bank.is_default;
    bankModalSubmitError = null;
    bankModalFieldErrors = {};
    bankModalOpen = true;
  }

  function closeBankModal() {
    bankModalOpen = false;
  }

  function bankFieldError(name: string, clientSide: string | null): string | null {
    if (bankModalFieldErrors[name] !== undefined) {
      return bankModalFieldErrors[name];
    }
    return clientSide;
  }

  async function onBankModalSubmit(event: Event) {
    event.preventDefault();
    bankModalSubmitError = null;
    bankModalFieldErrors = {};
    if (!bankModalValidation.ok) {
      return;
    }
    bankModalSubmitting = true;
    try {
      const body = composeSellerBankInputs(bankModalForm);
      const response =
        bankModalMode === "create"
          ? await createSellerBank(body)
          : await updateSellerBank(bankModalEditingId!, body);
      banks = response.banks;
      bankModalOpen = false;
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : String(err);
      const typed = parseSellerBankValidationError(message);
      if (typed !== null) {
        const next: Record<string, string> = {};
        for (const f of typed.fields) {
          next[f.field] = f.message;
        }
        bankModalFieldErrors = next;
        bankModalSubmitError = "Some fields need attention — see the inline messages.";
      } else {
        bankModalSubmitError = message;
      }
    } finally {
      bankModalSubmitting = false;
    }
  }

  async function onSetDefaultBank(bank: SellerBankResponse) {
    bankRowError = null;
    try {
      const response = await setDefaultSellerBank(bank.id);
      banks = response.banks;
    } catch (err: unknown) {
      bankRowError = err instanceof Error ? err.message : String(err);
    }
  }

  async function onDeleteBank(bank: SellerBankResponse) {
    bankRowError = null;
    const label = `${bank.currency} · ${bank.account_number}`;
    if (!confirm(`Delete bank account ${label}?`)) {
      return;
    }
    try {
      const response = await deleteSellerBank(bank.id);
      banks = response.banks;
    } catch (err: unknown) {
      bankRowError = err instanceof Error ? err.message : String(err);
    }
  }

  // ── PR-89 — Invoice numbering subsection handlers ───────────────────

  async function loadNumbering() {
    numberingLoading = true;
    numberingLoadError = null;
    try {
      numbering = await getSellerNumbering();
    } catch (err: unknown) {
      numberingLoadError = err instanceof Error ? err.message : String(err);
    } finally {
      numberingLoading = false;
    }
  }

  function addLiteralSegment() {
    const text = numberingLiteralDraft;
    if (text.length === 0) return;
    numbering = {
      ...numbering,
      segments: [...numbering.segments, { kind: "Literal", text }],
    };
    numberingLiteralDraft = "";
    numberingSaved = false;
  }

  function addYearSegment(digits: 2 | 4) {
    numbering = {
      ...numbering,
      segments: [...numbering.segments, { kind: "Year", digits }],
    };
    numberingSaved = false;
  }

  function addCounterSegment(padWidth: number) {
    const safePad = Math.max(1, Math.min(20, Math.floor(padWidth)));
    numbering = {
      ...numbering,
      segments: [...numbering.segments, { kind: "Counter", pad_width: safePad }],
    };
    numberingSaved = false;
  }

  function onSegmentUp(idx: number) {
    numbering = { ...numbering, segments: moveSegmentUp(numbering.segments, idx) };
    numberingSaved = false;
  }

  function onSegmentDown(idx: number) {
    numbering = { ...numbering, segments: moveSegmentDown(numbering.segments, idx) };
    numberingSaved = false;
  }

  function onSegmentRemove(idx: number) {
    numbering = { ...numbering, segments: removeSegment(numbering.segments, idx) };
    numberingSaved = false;
  }

  function onResetTemplateToDefault() {
    if (!confirm("Reset the invoice-numbering template to the default INV-default/NNNNN shape?")) {
      return;
    }
    numbering = defaultTemplate();
    numberingSaved = false;
  }

  function segmentLabel(seg: NumberingSegment): string {
    switch (seg.kind) {
      case "Literal":
        return `Literal "${seg.text}"`;
      case "Year":
        return `Year (${seg.digits} digits)`;
      case "Counter":
        return `Counter (pad ${seg.pad_width})`;
    }
  }

  // ── PR-92 / ADR-0047 — SMTP subsection handlers ─────────────────────

  async function loadSmtpConfig() {
    smtpLoading = true;
    smtpLoadError = null;
    try {
      const response: SmtpConfigGetResponse = await getSmtpConfig();
      smtpPasswordSet = response.passwordSet;
      if ("host" in response) {
        smtp = {
          host: response.host,
          port: response.port,
          fromAddress: response.fromAddress,
          fromDisplayName: response.fromDisplayName ?? "",
          username: response.username,
          security: response.security,
          attachXml: response.attachXml,
          password: "",
        };
      }
    } catch (err: unknown) {
      smtpLoadError = err instanceof Error ? err.message : String(err);
    } finally {
      smtpLoading = false;
    }
  }

  async function onTestSmtp() {
    smtpTestOutcome = null;
    smtpTestError = null;
    smtpTesting = true;
    try {
      const fromDisplayName =
        smtp.fromDisplayName.trim() === "" ? null : smtp.fromDisplayName.trim();
      // PR-98 — empty password means "use existing keychain entry" on
      // the test endpoint too, mirroring the PUT body semantics. The
      // operator can rotate AND test in one pass by typing the new
      // password and clicking Test before Save.
      const password = smtp.password.length > 0 ? smtp.password : null;
      smtpTestOutcome = await testSmtpConnection({
        host: smtp.host.trim(),
        port: smtp.port,
        fromAddress: smtp.fromAddress.trim(),
        fromDisplayName,
        username: smtp.username.trim(),
        security: smtp.security,
        attachXml: smtp.attachXml,
        password,
      });
    } catch (err: unknown) {
      smtpTestError = err instanceof Error ? err.message : String(err);
    } finally {
      smtpTesting = false;
    }
  }

  async function onSaveSmtp(event: Event) {
    event.preventDefault();
    smtpSaved = false;
    smtpSubmitError = null;
    smtpSubmitting = true;
    try {
      // Trim + normalise the optional display-name. Empty string ⇒
      // omit so the backend's `Option<String>` deserialiser sees the
      // clean "no display name" signal.
      const fromDisplayName =
        smtp.fromDisplayName.trim() === "" ? null : smtp.fromDisplayName.trim();
      // Password rotation: only send the field when the operator
      // typed something; blank means "leave the keychain entry
      // untouched". The form NEVER displays the existing password —
      // the backend's GET /api/smtp-config surfaces a
      // `passwordSet: bool` and the SPA renders an indicator.
      const password = smtp.password.length > 0 ? smtp.password : null;
      const response = await putSmtpConfig({
        host: smtp.host.trim(),
        port: smtp.port,
        fromAddress: smtp.fromAddress.trim(),
        fromDisplayName,
        username: smtp.username.trim(),
        security: smtp.security,
        attachXml: smtp.attachXml,
        password,
      });
      smtpPasswordSet = response.passwordSet;
      smtp = { ...smtp, password: "" };
      smtpSaved = true;
    } catch (err: unknown) {
      smtpSubmitError = err instanceof Error ? err.message : String(err);
    } finally {
      smtpSubmitting = false;
    }
  }

  async function onSaveNumbering(event: Event) {
    event.preventDefault();
    numberingSaved = false;
    numberingSubmitError = null;
    const validation = validateTemplate(numbering);
    if (validation !== null) {
      numberingSubmitError = numberingErrorMessage(validation);
      return;
    }
    numberingSubmitting = true;
    try {
      numbering = await putSellerNumbering(numbering);
      numberingSaved = true;
    } catch (err: unknown) {
      numberingSubmitError = err instanceof Error ? err.message : String(err);
    } finally {
      numberingSubmitting = false;
    }
  }
</script>

<section class="page" aria-labelledby="page-title">
  <header class="page__head">
    <h2 id="page-title" class="page__title">Tenant settings</h2>
    <p class="page__lede">
      Seller identity persisted to <code>~/.aberp/&lt;tenant&gt;/seller.toml</code>.
      Edits land via the same atomic write the first-run wizard uses; the
      printed-invoice PDF + the NAV XML rebuild against the new values
      on the next invoice issued.
    </p>
  </header>

  {#if loading}
    <p class="page__muted">Loading current values…</p>
  {:else if loadError !== null}
    <div class="page__error" role="alert">
      <strong>Could not load seller info.</strong>
      <p class="page__error-detail">{loadError}</p>
    </div>
  {:else}
    <form onsubmit={onSubmit} class="page__form">
      <fieldset disabled={submitting} class="page__fieldset">
        <div class="page__columns">
          <section class="page__column">
            <h3 class="page__section">Identity</h3>

            <label class="field">
              <span class="field__label">Legal name</span>
              <input
                class="field__input"
                type="text"
                autocomplete="organization"
                bind:value={form.legalName}
                aria-invalid={fieldError("legalName", validation.legalName) !== null}
              />
              {#if fieldError("legalName", validation.legalName) !== null}
                <span class="field__error">{fieldError("legalName", validation.legalName)}</span>
              {/if}
            </label>

            <label class="field">
              <span class="field__label">
                Tax number (ADÓSZÁM)
                <span class="field__hint">format: <code>xxxxxxxx-y-zz</code></span>
              </span>
              <input
                class="field__input"
                type="text"
                autocomplete="off"
                spellcheck="false"
                bind:value={form.taxNumber}
                aria-invalid={fieldError("taxNumber", validation.taxNumber) !== null}
              />
              {#if fieldError("taxNumber", validation.taxNumber) !== null}
                <span class="field__error">{fieldError("taxNumber", validation.taxNumber)}</span>
              {/if}
            </label>

            <label class="field">
              <span class="field__label">
                EU VAT number
                <span class="field__hint">optional</span>
              </span>
              <input
                class="field__input"
                type="text"
                autocomplete="off"
                spellcheck="false"
                bind:value={form.euVatNumber}
              />
            </label>

            <h3 class="page__section">Address</h3>

            <label class="field">
              <span class="field__label">Country code</span>
              <input
                class="field__input"
                type="text"
                autocomplete="country"
                bind:value={form.addressCountryCode}
                aria-invalid={fieldError("addressCountryCode", validation.addressCountryCode) !== null}
              />
              {#if fieldError("addressCountryCode", validation.addressCountryCode) !== null}
                <span class="field__error">{fieldError("addressCountryCode", validation.addressCountryCode)}</span>
              {/if}
            </label>

            <label class="field">
              <span class="field__label">Postal code</span>
              <input
                class="field__input"
                type="text"
                autocomplete="postal-code"
                bind:value={form.addressPostalCode}
                aria-invalid={fieldError("addressPostalCode", validation.addressPostalCode) !== null}
              />
              {#if fieldError("addressPostalCode", validation.addressPostalCode) !== null}
                <span class="field__error">{fieldError("addressPostalCode", validation.addressPostalCode)}</span>
              {/if}
            </label>

            <label class="field">
              <span class="field__label">City</span>
              <input
                class="field__input"
                type="text"
                autocomplete="address-level2"
                bind:value={form.addressCity}
                aria-invalid={fieldError("addressCity", validation.addressCity) !== null}
              />
              {#if fieldError("addressCity", validation.addressCity) !== null}
                <span class="field__error">{fieldError("addressCity", validation.addressCity)}</span>
              {/if}
            </label>

            <label class="field">
              <span class="field__label">Street</span>
              <input
                class="field__input"
                type="text"
                autocomplete="street-address"
                bind:value={form.addressStreet}
                aria-invalid={fieldError("addressStreet", validation.addressStreet) !== null}
              />
              {#if fieldError("addressStreet", validation.addressStreet) !== null}
                <span class="field__error">{fieldError("addressStreet", validation.addressStreet)}</span>
              {/if}
            </label>
          </section>

          <section class="page__column">
            <h3 class="page__section">
              Bank info
              <span class="page__section-hint">printed-invoice footer</span>
            </h3>

            <label class="field">
              <span class="field__label">Bank account number</span>
              <input
                class="field__input"
                type="text"
                autocomplete="off"
                spellcheck="false"
                bind:value={form.bankAccountNumber}
              />
            </label>

            <label class="field">
              <span class="field__label">IBAN</span>
              <input
                class="field__input"
                type="text"
                autocomplete="off"
                spellcheck="false"
                bind:value={form.iban}
              />
            </label>

            <label class="field">
              <span class="field__label">Bank name</span>
              <input
                class="field__input"
                type="text"
                autocomplete="off"
                bind:value={form.bankName}
              />
            </label>

            <label class="field">
              <span class="field__label">SWIFT / BIC</span>
              <input
                class="field__input"
                type="text"
                autocomplete="off"
                spellcheck="false"
                bind:value={form.swiftBic}
              />
            </label>
          </section>
        </div>

        {#if submitError !== null}
          <div class="page__error" role="alert">
            <strong>Could not save seller info.</strong>
            <p class="page__error-detail">{submitError}</p>
          </div>
        {/if}

        {#if saved}
          <div class="page__saved" role="status">Saved.</div>
        {/if}

        <div class="page__actions">
          <button
            type="submit"
            class="page__submit"
            disabled={submitting || !validation.ok}
          >
            {submitting ? "Saving…" : "Save"}
          </button>
        </div>
      </fieldset>
    </form>

    <!-- PR-72 / session-94 — multi-bank-account subsection. Writes go
         through /api/seller/banks (atomic merge that preserves the
         identity block above). -->
    <section class="page__banks" aria-labelledby="banks-title" data-testid="seller-banks-section">
      <header class="page__banks-head">
        <h3 id="banks-title" class="page__section">
          Bank accounts
          <span class="page__section-hint">per-currency · canonical seller.toml block</span>
        </h3>
        <button
          type="button"
          class="page__bank-add"
          onclick={openAddBankModal}
          data-testid="seller-banks-add"
        >
          + Add bank account
        </button>
      </header>

      {#if banksLoading}
        <p class="page__muted">Loading bank accounts…</p>
      {:else if banksLoadError !== null}
        <div class="page__error" role="alert">
          <strong>Could not load bank accounts.</strong>
          <p class="page__error-detail">{banksLoadError}</p>
        </div>
      {:else}
        {#if bankRowError !== null}
          <div class="page__error" role="alert" data-testid="seller-banks-row-error">
            <strong>Action failed.</strong>
            <p class="page__error-detail">{bankRowError}</p>
          </div>
        {/if}

        {#if banks.length === 0}
          <p class="page__muted" data-testid="seller-banks-empty">
            No bank accounts saved yet. Use <strong>+ Add bank account</strong> to add one.
          </p>
        {:else}
          {#each banksGrouped as group (group.currency)}
            <div class="page__bank-group" data-testid="seller-banks-group-{group.currency}">
              <h4 class="page__bank-group-title">{group.currency}</h4>
              <ul class="page__bank-list">
                {#each group.banks as bank (bank.id)}
                  <li class="page__bank-row" data-testid="seller-banks-row-{bank.id}">
                    <div class="page__bank-row-main">
                      <div class="page__bank-row-account">
                        <span class="page__bank-currency-chip">{bank.currency}</span>
                        <span class="page__bank-account-number">{bank.account_number}</span>
                        {#if bank.is_default}
                          <span class="page__bank-default-badge">Default</span>
                        {/if}
                      </div>
                      <div class="page__bank-row-meta">
                        <span class="page__bank-name">{bank.bank_name}</span>
                        <span class="page__bank-swift">{bank.swift_bic}</span>
                      </div>
                    </div>
                    <div class="page__bank-row-actions">
                      <button
                        type="button"
                        class="page__bank-action"
                        onclick={() => openEditBankModal(bank)}
                      >Edit</button>
                      {#if !bank.is_default}
                        <button
                          type="button"
                          class="page__bank-action"
                          onclick={() => onSetDefaultBank(bank)}
                        >Set as default</button>
                      {/if}
                      <button
                        type="button"
                        class="page__bank-action page__bank-action--danger"
                        onclick={() => onDeleteBank(bank)}
                      >Delete</button>
                    </div>
                  </li>
                {/each}
              </ul>
            </div>
          {/each}
        {/if}
      {/if}
    </section>

    <!-- PR-89 — operator-configurable invoice numbering. Click-to-
         assemble segment chips with reorder + remove, live preview,
         NAV-charset validation. Writes go through /api/seller/numbering
         (atomic merge that preserves identity + bank sections above). -->
    <section
      class="page__numbering"
      aria-labelledby="numbering-title"
      data-testid="seller-numbering-section"
    >
      <header class="page__banks-head">
        <h3 id="numbering-title" class="page__section">
          Invoice numbering
          <span class="page__section-hint">számlasorszám sablon · NAV invoiceNumber</span>
        </h3>
      </header>

      {#if numberingLoading}
        <p class="page__muted">Loading numbering template…</p>
      {:else if numberingLoadError !== null}
        <div class="page__error" role="alert">
          <strong>Could not load numbering template.</strong>
          <p class="page__error-detail">{numberingLoadError}</p>
        </div>
      {:else}
        <p class="page__muted" style="margin-bottom: var(--space-3);">
          Assemble the next invoice number from segments. Hungarian §169 requires gap-free numbering;
          set the start value once as a setup/migration step (e.g. to continue from Billingo).
          After your first real invoice is issued, do NOT change the template — historical invoices
          would re-render under the new template.
        </p>

        <div class="numbering__preview" data-testid="seller-numbering-preview">
          <span class="numbering__preview-label">Next invoice will be:</span>
          <code class="numbering__preview-value">{numberingPreview}</code>
          {#if numberingPreviewNextYear !== null}
            <span class="numbering__preview-next-label">Next year (annual reset):</span>
            <code class="numbering__preview-value">{numberingPreviewNextYear}</code>
          {/if}
        </div>

        <ul class="numbering__segments" data-testid="seller-numbering-segments">
          {#each numbering.segments as seg, idx (idx + ":" + seg.kind + ":" + (seg.kind === "Literal" ? seg.text : seg.kind === "Year" ? seg.digits : seg.pad_width))}
            <li class="numbering__segment-row" data-testid="seller-numbering-segment-{idx}">
              <span class="numbering__segment-chip numbering__segment-chip--{seg.kind.toLowerCase()}">
                {segmentLabel(seg)}
              </span>
              <div class="numbering__segment-actions">
                <button
                  type="button"
                  class="numbering__seg-btn"
                  onclick={() => onSegmentUp(idx)}
                  disabled={idx === 0}
                  aria-label="Move up"
                >↑</button>
                <button
                  type="button"
                  class="numbering__seg-btn"
                  onclick={() => onSegmentDown(idx)}
                  disabled={idx === numbering.segments.length - 1}
                  aria-label="Move down"
                >↓</button>
                <button
                  type="button"
                  class="numbering__seg-btn numbering__seg-btn--remove"
                  onclick={() => onSegmentRemove(idx)}
                  aria-label="Remove"
                >×</button>
              </div>
            </li>
          {/each}
        </ul>

        <div class="numbering__builder">
          <div class="numbering__builder-row">
            <input
              type="text"
              class="field__input numbering__literal-input"
              placeholder="Literal text (e.g. ABERP-)"
              bind:value={numberingLiteralDraft}
              data-testid="seller-numbering-literal-input"
            />
            <button
              type="button"
              class="numbering__add-btn"
              onclick={addLiteralSegment}
              disabled={numberingLiteralDraft.length === 0}
              data-testid="seller-numbering-add-literal"
            >+ Literal</button>
            <button
              type="button"
              class="numbering__add-btn"
              onclick={() => addYearSegment(4)}
              data-testid="seller-numbering-add-year-4"
            >+ Year (4)</button>
            <button
              type="button"
              class="numbering__add-btn"
              onclick={() => addYearSegment(2)}
              data-testid="seller-numbering-add-year-2"
            >+ Year (2)</button>
            <button
              type="button"
              class="numbering__add-btn"
              onclick={() => addCounterSegment(6)}
              data-testid="seller-numbering-add-counter-6"
            >+ Counter (pad 6)</button>
            <button
              type="button"
              class="numbering__add-btn"
              onclick={() => addCounterSegment(4)}
              data-testid="seller-numbering-add-counter-4"
            >+ Counter (pad 4)</button>
          </div>
        </div>

        <form onsubmit={onSaveNumbering} class="page__form">
          <fieldset disabled={numberingSubmitting} class="page__fieldset">
            <div class="page__columns">
              <label class="field">
                <span class="field__label">Reset policy</span>
                <select
                  class="field__input"
                  bind:value={numbering.reset_policy}
                  data-testid="seller-numbering-reset-policy"
                >
                  <option value="never">Never (continuous)</option>
                  <option value="on_year_change">Reset on year change (HU default)</option>
                </select>
              </label>

              <label class="field">
                <span class="field__label">
                  Start value
                  <span class="field__hint">setup-only — locks after first invoice</span>
                </span>
                <input
                  type="number"
                  min="1"
                  class="field__input"
                  bind:value={numbering.start_value}
                  data-testid="seller-numbering-start-value"
                />
              </label>
            </div>

            {#if numberingValidation !== null}
              <div class="page__error" role="alert" data-testid="seller-numbering-validation-error">
                <strong>Template is not yet valid.</strong>
                <p class="page__error-detail">{numberingErrorMessage(numberingValidation)}</p>
              </div>
            {/if}

            {#if numberingSubmitError !== null}
              <div class="page__error" role="alert">
                <strong>Could not save numbering template.</strong>
                <p class="page__error-detail">{numberingSubmitError}</p>
              </div>
            {/if}

            {#if numberingSaved}
              <div class="page__saved" role="status" data-testid="seller-numbering-saved">Saved.</div>
            {/if}

            <div class="page__actions">
              <button
                type="button"
                class="modal__cancel"
                onclick={onResetTemplateToDefault}
                data-testid="seller-numbering-reset-default"
              >Reset to default</button>
              <button
                type="submit"
                class="page__submit"
                disabled={numberingSubmitting || numberingValidation !== null}
                data-testid="seller-numbering-save"
              >{numberingSubmitting ? "Saving…" : "Save"}</button>
            </div>
          </fieldset>
        </form>
      {/if}
    </section>

    <!-- PR-92 / ADR-0047 — SMTP delivery configuration. Non-secrets
         persist to seller.toml's [seller.smtp] section via the same
         atomic merge the other settings use; the password lives in the
         OS keychain and is NEVER round-tripped back to the SPA. -->
    <section
      class="page__smtp"
      aria-labelledby="smtp-title"
      data-testid="smtp-config-section"
    >
      <header class="page__banks-head">
        <h3 id="smtp-title" class="page__section">
          SMTP email delivery
          <span class="page__section-hint">számla küldés vevőnek · TLS-only</span>
        </h3>
      </header>

      {#if smtpLoading}
        <p class="page__muted">Loading SMTP config…</p>
      {:else if smtpLoadError !== null}
        <div class="page__error" role="alert">
          <strong>Could not load SMTP config.</strong>
          <p class="page__error-detail">{smtpLoadError}</p>
        </div>
      {:else}
        <p class="page__muted" style="margin-bottom: var(--space-3);">
          A számlákat ezekkel a beállításokkal küldjük el a vevőknek
          PDF-ben (és opcionálisan NAV XML-ben is). A jelszó a macOS
          kulcskarikán él, soha nem kerül lemezre vagy logba. Csak
          TLS — egyszerű (plaintext) SMTP nem konfigurálható. /
          Invoices are emailed to buyers as PDF (and optionally NAV
          XML) using these settings. The password lives in the macOS
          keychain — never on disk, never in logs. TLS-only — plaintext
          SMTP is not a configurable option.
        </p>

        <form onsubmit={onSaveSmtp} class="page__form" data-testid="smtp-config-form">
          <fieldset disabled={smtpSubmitting} class="page__fieldset">
            <div class="page__columns">
              <section class="page__column">
                <label class="field">
                  <span class="field__label">SMTP host</span>
                  <input
                    type="text"
                    class="field__input"
                    autocomplete="off"
                    spellcheck="false"
                    placeholder="smtp.example.com"
                    bind:value={smtp.host}
                    data-testid="smtp-host"
                    required
                  />
                </label>

                <label class="field">
                  <span class="field__label">Port</span>
                  <input
                    type="number"
                    class="field__input"
                    min="1"
                    max="65535"
                    bind:value={smtp.port}
                    data-testid="smtp-port"
                    required
                  />
                </label>

                <label class="field">
                  <span class="field__label">Security</span>
                  <select
                    class="field__input"
                    bind:value={smtp.security}
                    data-testid="smtp-security"
                  >
                    <option value="StartTls">STARTTLS (port 587)</option>
                    <option value="Tls">Implicit TLS (port 465)</option>
                  </select>
                </label>

                <label class="field field--checkbox">
                  <input
                    type="checkbox"
                    bind:checked={smtp.attachXml}
                    data-testid="smtp-attach-xml"
                  />
                  <span>
                    NAV XML csatolása PDF mellé / Attach NAV XML alongside PDF
                  </span>
                </label>
              </section>

              <section class="page__column">
                <label class="field">
                  <span class="field__label">From address</span>
                  <input
                    type="email"
                    class="field__input"
                    autocomplete="off"
                    spellcheck="false"
                    placeholder="noreply@example.com"
                    bind:value={smtp.fromAddress}
                    data-testid="smtp-from-address"
                    required
                  />
                </label>

                <label class="field">
                  <span class="field__label">From display name (optional)</span>
                  <input
                    type="text"
                    class="field__input"
                    placeholder="Áben Consulting KFT."
                    bind:value={smtp.fromDisplayName}
                    data-testid="smtp-from-display-name"
                  />
                </label>

                <label class="field">
                  <span class="field__label">SMTP username</span>
                  <input
                    type="text"
                    class="field__input"
                    autocomplete="off"
                    spellcheck="false"
                    placeholder="usually the same as From address"
                    bind:value={smtp.username}
                    data-testid="smtp-username"
                    required
                  />
                </label>

                <label class="field">
                  <span class="field__label">
                    SMTP password
                    {#if smtpPasswordSet}
                      <span class="field__hint" data-testid="smtp-password-set-indicator">
                        ✓ jelszó beállítva · password is set in the keychain
                      </span>
                    {:else}
                      <span class="field__hint" data-testid="smtp-password-not-set-indicator">
                        ⚠ még nincs beállítva · not yet set
                      </span>
                    {/if}
                  </span>
                  <input
                    type="password"
                    class="field__input"
                    autocomplete="new-password"
                    spellcheck="false"
                    placeholder={smtpPasswordSet
                      ? "leave blank to keep existing password"
                      : "enter SMTP password to save to keychain"}
                    bind:value={smtp.password}
                    data-testid="smtp-password"
                  />
                </label>
              </section>
            </div>

            {#if smtpSubmitError !== null}
              <div class="page__error" role="alert">
                <strong>Could not save SMTP config.</strong>
                <p class="page__error-detail">{smtpSubmitError}</p>
              </div>
            {/if}

            {#if smtpSaved}
              <div class="page__saved" role="status" data-testid="smtp-config-saved">Saved.</div>
            {/if}

            <!-- PR-98 — Test connection outcome banner. The test endpoint
                 NEVER persists anything; the outcome is shown inline and
                 does NOT toggle smtpSaved (only the Save button does). -->
            {#if smtpTestError !== null}
              <div class="page__error" role="alert" data-testid="smtp-test-error">
                <strong>Could not run the SMTP test.</strong>
                <p class="page__error-detail">{smtpTestError}</p>
              </div>
            {:else if smtpTestOutcome !== null}
              {#if smtpTestOutcome.outcome === "succeeded"}
                <div
                  class="page__saved"
                  role="status"
                  data-testid="smtp-test-success"
                >SMTP test OK — TLS handshake + AUTH + NOOP succeeded.</div>
              {:else}
                <div class="page__error" role="alert" data-testid="smtp-test-failure">
                  <strong>SMTP test failed ({smtpTestOutcome.error_class ?? "other"}).</strong>
                  {#if smtpTestOutcome.error_detail}
                    <p class="page__error-detail">{smtpTestOutcome.error_detail}</p>
                  {/if}
                </div>
              {/if}
            {/if}

            <div class="page__actions">
              <button
                type="button"
                class="page__secondary"
                disabled={smtpTesting || smtpSubmitting}
                onclick={onTestSmtp}
                data-testid="smtp-test-connection"
              >{smtpTesting ? "Testing…" : "Test connection"}</button>
              <button
                type="submit"
                class="page__submit"
                disabled={smtpSubmitting || smtpTesting}
                data-testid="smtp-config-save"
              >{smtpSubmitting ? "Saving…" : "Save"}</button>
            </div>
          </fieldset>
        </form>
      {/if}
    </section>

    {#if bankModalOpen}
      <div class="modal" role="dialog" aria-modal="true" aria-labelledby="bank-modal-title">
        <div class="modal__panel">
          <header class="modal__head">
            <h3 id="bank-modal-title" class="modal__title">
              {bankModalMode === "create" ? "Add bank account" : "Edit bank account"}
            </h3>
            <button
              type="button"
              class="modal__close"
              onclick={closeBankModal}
              aria-label="Close"
            >×</button>
          </header>
          <form onsubmit={onBankModalSubmit} class="modal__form" data-testid="seller-banks-modal-form">
            <fieldset disabled={bankModalSubmitting} class="modal__fieldset">
              <label class="field">
                <span class="field__label">Currency</span>
                <select
                  class="field__input"
                  bind:value={bankModalForm.currency}
                  data-testid="seller-banks-modal-currency"
                >
                  <option value="HUF">HUF</option>
                  <option value="EUR">EUR</option>
                </select>
              </label>

              <label class="field">
                <span class="field__label">Account number</span>
                <input
                  class="field__input"
                  type="text"
                  autocomplete="off"
                  spellcheck="false"
                  bind:value={bankModalForm.accountNumber}
                  data-testid="seller-banks-modal-account-number"
                  aria-invalid={bankFieldError("accountNumber", bankModalValidation.accountNumber) !== null}
                />
                {#if bankFieldError("accountNumber", bankModalValidation.accountNumber) !== null}
                  <span class="field__error">
                    {bankFieldError("accountNumber", bankModalValidation.accountNumber)}
                  </span>
                {/if}
              </label>

              <label class="field">
                <span class="field__label">Bank name</span>
                <input
                  class="field__input"
                  type="text"
                  autocomplete="off"
                  bind:value={bankModalForm.bankName}
                  aria-invalid={bankFieldError("bankName", bankModalValidation.bankName) !== null}
                />
                {#if bankFieldError("bankName", bankModalValidation.bankName) !== null}
                  <span class="field__error">
                    {bankFieldError("bankName", bankModalValidation.bankName)}
                  </span>
                {/if}
              </label>

              <label class="field">
                <span class="field__label">SWIFT / BIC</span>
                <input
                  class="field__input"
                  type="text"
                  autocomplete="off"
                  spellcheck="false"
                  bind:value={bankModalForm.swiftBic}
                  aria-invalid={bankFieldError("swiftBic", bankModalValidation.swiftBic) !== null}
                />
                {#if bankFieldError("swiftBic", bankModalValidation.swiftBic) !== null}
                  <span class="field__error">
                    {bankFieldError("swiftBic", bankModalValidation.swiftBic)}
                  </span>
                {/if}
              </label>

              {#if bankModalMode === "create" || !bankModalEditingIsDefault}
                <label class="field field--checkbox">
                  <input
                    type="checkbox"
                    bind:checked={bankModalForm.setAsDefault}
                    data-testid="seller-banks-modal-set-default"
                  />
                  <span>Set as default for {bankModalForm.currency}</span>
                </label>
              {/if}

              {#if bankModalSubmitError !== null}
                <div class="page__error" role="alert">
                  <strong>Could not save bank account.</strong>
                  <p class="page__error-detail">{bankModalSubmitError}</p>
                </div>
              {/if}

              <div class="modal__actions">
                <button type="button" class="modal__cancel" onclick={closeBankModal}>Cancel</button>
                <button
                  type="submit"
                  class="page__submit"
                  disabled={bankModalSubmitting || !bankModalValidation.ok}
                >
                  {bankModalSubmitting ? "Saving…" : "Save"}
                </button>
              </div>
            </fieldset>
          </form>
        </div>
      </div>
    {/if}
  {/if}
</section>

<style>
  .page {
    max-width: 960px;
    margin: 0 auto;
  }

  .page__head {
    margin-bottom: var(--space-4);
  }

  .page__title {
    margin: 0 0 var(--space-2) 0;
    font-size: var(--type-size-lg);
    font-weight: 600;
    color: var(--color-text-strong);
  }

  .page__lede {
    margin: 0;
    color: var(--color-text-secondary);
    font-size: var(--type-size-sm);
    line-height: 1.5;
  }

  .page__muted {
    color: var(--color-text-muted);
    font-size: var(--type-size-sm);
  }

  .page__form {
    display: contents;
  }

  .page__fieldset {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    border: 0;
    padding: 0;
    margin: 0;
  }

  .page__columns {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: var(--space-5);
  }

  @media (max-width: 720px) {
    .page__columns {
      grid-template-columns: 1fr;
    }
  }

  .page__column {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }

  .page__section {
    margin: var(--space-3) 0 0 0;
    font-size: var(--type-size-sm);
    font-weight: 600;
    color: var(--color-text-strong);
    border-bottom: 1px solid var(--color-surface-divider);
    padding-bottom: var(--space-1);
  }

  .page__section-hint {
    font-weight: 400;
    color: var(--color-text-muted);
    font-size: var(--type-size-xs);
    margin-left: var(--space-2);
  }

  .field {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }

  .field__label {
    font-size: var(--type-size-sm);
    color: var(--color-text-primary);
    font-weight: 500;
  }

  .field__hint {
    margin-left: var(--space-2);
    font-size: var(--type-size-xs);
    color: var(--color-text-muted);
    font-weight: 400;
  }

  .field__input {
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--color-surface-divider);
    border-radius: 4px;
    background: var(--color-surface-base, var(--color-surface-raised));
    color: var(--color-text-strong);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
  }

  .field__input[aria-invalid="true"] {
    border-color: var(--color-signal-negative);
  }

  .field__error {
    font-size: var(--type-size-xs);
    color: var(--color-signal-negative);
  }

  code {
    font-family: var(--type-family-mono);
    color: var(--color-text-strong);
  }

  .page__error {
    padding: var(--space-2) var(--space-3);
    border-left: 3px solid var(--color-signal-negative);
    background: var(--color-surface-raised);
    font-size: var(--type-size-sm);
  }

  .page__error-detail {
    margin: var(--space-1) 0 0 0;
    font-family: var(--type-family-mono);
    font-size: var(--type-size-xs);
    color: var(--color-text-secondary);
    white-space: pre-wrap;
    word-break: break-word;
  }

  .page__saved {
    padding: var(--space-2) var(--space-3);
    border-left: 3px solid var(--color-signal-positive);
    background: var(--color-surface-raised);
    color: var(--color-text-primary);
    font-size: var(--type-size-sm);
  }

  .page__actions {
    display: flex;
    justify-content: flex-end;
  }

  .page__submit {
    padding: var(--space-2) var(--space-5);
    background: var(--color-signal-positive, var(--color-text-strong));
    color: var(--color-surface-base, white);
    border: 0;
    border-radius: 4px;
    font-size: var(--type-size-sm);
    font-weight: 500;
    cursor: pointer;
  }

  .page__submit:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }

  /* PR-98 — secondary action button (Test connection). Quieter chrome
   * than .page__submit so the primary Save action stays visually
   * dominant; same disabled treatment. */
  .page__secondary {
    padding: var(--space-2) var(--space-5);
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    border: 1px solid var(--color-surface-divider);
    border-radius: 4px;
    font-size: var(--type-size-sm);
    cursor: pointer;
    margin-right: var(--space-2);
  }

  .page__secondary:hover:not(:disabled) {
    color: var(--color-text-strong);
  }

  .page__secondary:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }

  /* PR-72 / session-94 — bank-accounts subsection. */
  .page__banks {
    margin-top: var(--space-5);
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }

  .page__banks-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-3);
  }

  .page__bank-add {
    padding: var(--space-1) var(--space-3);
    background: var(--color-signal-positive, var(--color-text-strong));
    color: var(--color-surface-base, white);
    border: 0;
    border-radius: 4px;
    font-size: var(--type-size-sm);
    font-weight: 500;
    cursor: pointer;
  }

  .page__bank-group {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }

  .page__bank-group-title {
    margin: 0;
    font-size: var(--type-size-xs);
    font-weight: 600;
    color: var(--color-text-muted);
    text-transform: uppercase;
    letter-spacing: 0.05em;
  }

  .page__bank-list {
    list-style: none;
    padding: 0;
    margin: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }

  .page__bank-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-3);
    padding: var(--space-2) var(--space-3);
    background: var(--color-surface-raised);
    border: 1px solid var(--color-surface-divider);
    border-radius: 4px;
  }

  .page__bank-row-main {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    min-width: 0;
  }

  .page__bank-row-account {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
    color: var(--color-text-strong);
  }

  .page__bank-currency-chip {
    padding: 0 var(--space-1);
    background: var(--color-surface-divider);
    color: var(--color-text-secondary);
    border-radius: 2px;
    font-size: var(--type-size-xs);
    font-weight: 600;
    letter-spacing: 0.05em;
  }

  .page__bank-account-number {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .page__bank-default-badge {
    padding: 0 var(--space-1);
    background: var(--color-signal-positive, var(--color-text-strong));
    color: var(--color-surface-base, white);
    border-radius: 2px;
    font-size: var(--type-size-xs);
    font-weight: 500;
  }

  .page__bank-row-meta {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    font-size: var(--type-size-xs);
    color: var(--color-text-secondary);
  }

  .page__bank-swift {
    font-family: var(--type-family-mono);
  }

  .page__bank-row-actions {
    display: flex;
    gap: var(--space-1);
    flex-shrink: 0;
  }

  .page__bank-action {
    padding: var(--space-1) var(--space-2);
    background: transparent;
    color: var(--color-text-secondary);
    border: 1px solid var(--color-surface-divider);
    border-radius: 4px;
    font-size: var(--type-size-xs);
    cursor: pointer;
  }

  .page__bank-action:hover {
    background: var(--color-surface-divider);
    color: var(--color-text-strong);
  }

  .page__bank-action--danger {
    color: var(--color-signal-negative);
    border-color: var(--color-signal-negative);
  }

  .page__bank-action--danger:hover {
    background: var(--color-signal-negative);
    color: var(--color-surface-base, white);
  }

  /* PR-72 / session-94 — modal scaffolding for the add/edit form. */
  .modal {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.5);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 100;
  }

  .modal__panel {
    max-width: 480px;
    width: 90vw;
    max-height: 90vh;
    overflow-y: auto;
    background: var(--color-surface-raised);
    border-radius: 6px;
    padding: var(--space-4);
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }

  .modal__head {
    display: flex;
    align-items: center;
    justify-content: space-between;
  }

  .modal__title {
    margin: 0;
    font-size: var(--type-size-md);
    font-weight: 600;
    color: var(--color-text-strong);
  }

  .modal__close {
    background: transparent;
    border: 0;
    color: var(--color-text-secondary);
    font-size: var(--type-size-lg);
    cursor: pointer;
  }

  .modal__form {
    display: contents;
  }

  .modal__fieldset {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    border: 0;
    padding: 0;
    margin: 0;
  }

  .modal__actions {
    display: flex;
    justify-content: flex-end;
    gap: var(--space-2);
  }

  .modal__cancel {
    padding: var(--space-2) var(--space-3);
    background: transparent;
    color: var(--color-text-secondary);
    border: 1px solid var(--color-surface-divider);
    border-radius: 4px;
    cursor: pointer;
  }

  .field--checkbox {
    flex-direction: row;
    align-items: center;
    gap: var(--space-2);
  }
</style>
