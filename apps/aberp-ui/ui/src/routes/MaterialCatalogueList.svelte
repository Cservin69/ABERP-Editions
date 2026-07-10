<script lang="ts">
  // S266 / PR-255 — Settings → Material Catalogue page. The auto-quoting
  // strand's first tunable table (design doc §3 / §11): operator-managed
  // CRUD over `quoting_materials` with a sortable list, an Add/Edit modal,
  // and ConfirmActionModal delete ([[hulye-biztos]]). The list response
  // also carries the storefront catalogue-push status (the "last push" /
  // re-paste-bearer banner). Dark-theme per [[spa-dark-theme-default]];
  // modelled on AdaptersList.svelte.

  import { onMount } from "svelte";
  import {
    deleteQuotingMaterial,
    listQuotingMaterials,
    testCataloguePush,
    type CataloguePushStatus,
    type CataloguePushTestOutcome,
    type QuotingMaterial,
  } from "../lib/api";
  import {
    sortMaterials,
    stockStatusLabel,
    stockStatusTone,
    toggleSort,
    type SortKey,
    type SortState,
  } from "../lib/material-catalogue";
  import { isDemoMode } from "../lib/workshop-demo-mode";
  import ConfirmActionModal from "../lib/ConfirmActionModal.svelte";
  import MaterialCatalogueForm from "./MaterialCatalogueForm.svelte";

  type LoadState = "idle" | "loading" | "ready" | "error";

  let loadState = $state<LoadState>("idle");
  let errorMessage = $state<string | null>(null);
  let rows = $state<QuotingMaterial[]>([]);
  let pushStatus = $state<CataloguePushStatus | null>(null);

  let sort = $state<SortState>({ key: null, dir: "asc" });

  let formMode = $state<"add" | "edit" | null>(null);
  let formInitial = $state<QuotingMaterial | null>(null);

  let confirmDelete = $state<QuotingMaterial | null>(null);
  let deleting = $state(false);
  let deleteError = $state<string | null>(null);

  // S289 / PR-270 — "Test catalogue push" probe (brief D). Runs ONE
  // push using the current storefront credential snapshot and surfaces
  // the typed outcome inline.
  let testPushing = $state(false);
  let testPushOutcome = $state<CataloguePushTestOutcome | null>(null);
  let testPushError = $state<string | null>(null);

  const demo = isDemoMode();

  const sortedRows = $derived(sortMaterials(rows, sort));

  onMount(() => {
    void refresh();
  });

  async function refresh(): Promise<void> {
    loadState = "loading";
    errorMessage = null;
    try {
      const res = await listQuotingMaterials();
      rows = res.materials;
      pushStatus = res.push_status;
      loadState = "ready";
    } catch (e) {
      errorMessage = e instanceof Error ? e.message : String(e);
      loadState = "error";
    }
  }

  function onSort(key: SortKey): void {
    sort = toggleSort(sort, key);
  }

  function sortIndicator(key: SortKey): string {
    if (sort.key !== key) return "";
    return sort.dir === "asc" ? "▲" : "▼";
  }

  function openAdd(): void {
    if (demo) return;
    formInitial = null;
    formMode = "add";
  }

  function openEdit(row: QuotingMaterial): void {
    if (demo) return;
    formInitial = row;
    formMode = "edit";
  }

  function closeForm(): void {
    formMode = null;
    formInitial = null;
  }

  async function onFormSaved(): Promise<void> {
    closeForm();
    await refresh();
  }

  function askDelete(row: QuotingMaterial): void {
    if (demo) return;
    deleteError = null;
    confirmDelete = row;
  }

  async function onTestPush(): Promise<void> {
    testPushing = true;
    testPushError = null;
    testPushOutcome = null;
    try {
      testPushOutcome = await testCataloguePush();
    } catch (e) {
      testPushError = e instanceof Error ? e.message : String(e);
    } finally {
      testPushing = false;
    }
  }

  async function doDelete(): Promise<void> {
    if (confirmDelete === null) return;
    deleting = true;
    deleteError = null;
    try {
      await deleteQuotingMaterial(confirmDelete.grade);
      confirmDelete = null;
      await refresh();
    } catch (e) {
      deleteError = e instanceof Error ? e.message : String(e);
    } finally {
      deleting = false;
    }
  }

  function fmt2(n: number): string {
    return n.toFixed(2);
  }
</script>

<section class="mat-page" data-testid="material-catalogue-section">
  <header class="mat-page__head">
    <div>
      <h2 class="mat-page__title">
        Anyagkatalógus / Material catalogue
        <span class="mat-page__hint">
          Az automatikus árazás anyagtáblája — a nyilvános mezők a
          webáruházba kerülnek / pricing material table; public fields push
          to the storefront
        </span>
      </h2>
    </div>
    <div class="mat-page__actions">
      <button
        type="button"
        class="mat-page__refresh"
        disabled={loadState === "loading"}
        onclick={() => void refresh()}
        data-testid="material-refresh"
      >
        {loadState === "loading" ? "Frissítés…" : "Frissítés / Refresh"}
      </button>
      <button
        type="button"
        class="mat-page__refresh"
        disabled={testPushing}
        onclick={() => void onTestPush()}
        data-testid="material-test-push"
        title="Try one push against the storefront using the current Quote Intake Base URL + bearer."
      >
        {testPushing ? "Tesztelés…" : "Teszt push / Test push"}
      </button>
      <button
        type="button"
        class="mat-page__add"
        disabled={demo}
        onclick={openAdd}
        data-testid="material-add-btn"
        title={demo ? "Demo mode — changes disabled" : "Add material"}
      >
        + Anyag / Material
      </button>
    </div>
  </header>

  {#if demo}
    <div class="mat-page__demo" role="status">
      Demo mód — a katalógus módosítása letiltva. / Demo mode — catalogue
      changes disabled.
    </div>
  {/if}

  {#if pushStatus !== null}
    {#if pushStatus.paused}
      <div class="mat-page__error" role="alert" data-testid="material-push-paused">
        <strong>A webáruház-feltöltés szünetel (401) / Storefront push paused (401).</strong>
        <p class="mat-page__error-detail">
          Illeszd be újra a bearer tokent a Beállítások → Quote Intake alatt,
          majd indítsd újra az ABERP-et. / Re-paste the bearer token in
          Settings → Quote Intake and restart ABERP to resume.
        </p>
      </div>
    {:else}
      <div class="mat-page__push" role="status" data-testid="material-push-status">
        {#if !pushStatus.running}
          Webáruház-feltöltés inaktív (nincs konfigurált webáruház). /
          Storefront push dormant (no storefront configured).
        {:else if pushStatus.last_attempt_at}
          Utolsó feltöltés / Last push: <strong>{pushStatus.last_outcome}</strong>
          {#if pushStatus.last_pushed_count !== null}
            · {pushStatus.last_pushed_count} tétel / rows
          {/if}
          · {pushStatus.last_attempt_at}
          {#if pushStatus.last_detail}
            · {pushStatus.last_detail}
          {/if}
        {:else}
          Webáruház-feltöltés aktív, még nem futott. / Storefront push
          running, no attempt yet.
        {/if}
      </div>
    {/if}
  {/if}

  {#if testPushError !== null}
    <div class="mat-page__error" role="alert" data-testid="material-test-push-error">
      <strong>A teszt push nem futott le / Test push could not run.</strong>
      <p class="mat-page__error-detail">{testPushError}</p>
    </div>
  {:else if testPushOutcome !== null}
    {#if testPushOutcome.outcome === "succeeded"}
      <div class="mat-page__push" role="status" data-testid="material-test-push-success">
        Teszt push sikeres / Test push succeeded
        {#if testPushOutcome.pushed_count !== undefined}
          · {testPushOutcome.pushed_count} tétel / rows
        {/if}
      </div>
    {:else}
      <div class="mat-page__error" role="alert" data-testid="material-test-push-failure">
        <strong>Teszt push sikertelen ({testPushOutcome.error_class ?? "other"}) / Test push failed.</strong>
        {#if testPushOutcome.error_detail}
          <p class="mat-page__error-detail">{testPushOutcome.error_detail}</p>
        {/if}
      </div>
    {/if}
  {/if}

  {#if deleteError !== null}
    <div class="mat-page__error" role="alert" data-testid="material-delete-error">
      <strong>A törlés nem sikerült / Delete failed.</strong>
      <p class="mat-page__error-detail">{deleteError}</p>
    </div>
  {/if}

  {#if loadState === "loading" && rows.length === 0}
    <p class="mat-page__muted">Betöltés… / Loading materials…</p>
  {:else if loadState === "error"}
    <div class="mat-page__error" role="alert">
      <strong>Nem sikerült lekérni a katalógust / Failed to load catalogue.</strong>
      <p class="mat-page__error-detail">{errorMessage}</p>
    </div>
  {:else if rows.length === 0}
    <div class="mat-page__empty" data-testid="material-empty">
      <p>
        Nincs még anyag a katalógusban. Adj hozzá egyet a „+ Anyag" gombbal.
      </p>
      <p>No materials in the catalogue yet. Add one with "+ Material".</p>
    </div>
  {:else}
    <table class="mat-table" data-testid="material-table">
      <thead>
        <tr>
          {#each [["grade", "Grade"], ["display_name", "Megnevezés / Name"], ["density_g_cm3", "Sűrűség / Density"], ["cost_per_kg_eur", "€/kg"], ["machining_difficulty", "Nehézség / Diff."], ["carbide_life_multiplier", "Karbid / Carbide"], ["stock_status", "Készlet / Stock"], ["lead_time_default_days", "Szállítás / Lead"], ["quote_multiplier", "Szorzó / Mult."], ["updated_at", "Módosítva / Updated"]] as [key, label] (key)}
            <th
              scope="col"
              aria-sort={sort.key === key
                ? sort.dir === "asc"
                  ? "ascending"
                  : "descending"
                : "none"}
            >
              <button
                type="button"
                class="sort-header"
                onclick={() => onSort(key as SortKey)}
                data-testid={`material-sort-${key}`}
              >
                <span>{label}</span>
                <span class="sort-indicator" aria-hidden="true"
                  >{sortIndicator(key as SortKey)}</span
                >
              </button>
            </th>
          {/each}
          <th scope="col">Művelet / Action</th>
        </tr>
      </thead>
      <tbody>
        {#each sortedRows as row (row.grade)}
          <tr data-testid="material-row" data-grade={row.grade}>
            <td class="mat-table__grade">{row.grade}</td>
            <td>{row.display_name}</td>
            <td class="num">{fmt2(row.density_g_cm3)}</td>
            <td class="num">{fmt2(row.cost_per_kg_eur)}</td>
            <td class="num">{row.machining_difficulty}</td>
            <td class="num">{row.carbide_life_multiplier}</td>
            <td>
              <span
                class="mat-chip mat-chip--{stockStatusTone(row.stock_status)}"
                data-status={row.stock_status}
              >
                {stockStatusLabel(row.stock_status)}
              </span>
            </td>
            <td class="num">{row.lead_time_default_days}</td>
            <td class="num">{row.quote_multiplier}</td>
            <td class="mat-table__updated" title={row.updated_by_actor}>
              {row.updated_at}
            </td>
            <td>
              <div class="mat-row__actions">
                <button
                  type="button"
                  class="mat-row__edit"
                  disabled={demo}
                  onclick={() => openEdit(row)}
                  data-testid="material-row-edit-btn"
                >
                  Szerkesztés / Edit
                </button>
                <button
                  type="button"
                  class="mat-row__delete"
                  disabled={demo}
                  onclick={() => askDelete(row)}
                  data-testid="material-row-delete-btn"
                >
                  Törlés / Delete
                </button>
              </div>
            </td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}
</section>

{#if formMode !== null}
  <MaterialCatalogueForm
    mode={formMode}
    initial={formInitial}
    onSaved={() => void onFormSaved()}
    onCancel={closeForm}
  />
{/if}

{#if confirmDelete !== null}
  <ConfirmActionModal
    title="Anyag törlése? / Delete material?"
    body={`A(z) "${confirmDelete.grade}" anyagminőség törlődik a katalógusból. / The "${confirmDelete.grade}" grade will be removed from the catalogue.`}
    consequence="A törlés után a következő feltöltéskor eltűnik a webáruház anyaglistájából. / After deletion it disappears from the storefront material list on the next push."
    confirmLabel="Törlés / Delete"
    cancelLabel="Mégse / Cancel"
    busy={deleting}
    onConfirm={() => void doDelete()}
    onCancel={() => (confirmDelete = null)}
  />
{/if}

<style>
  .mat-page {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    padding: var(--space-4) 0;
  }

  .mat-page__head {
    display: flex;
    justify-content: space-between;
    align-items: flex-end;
    gap: var(--space-3);
    flex-wrap: wrap;
  }

  .mat-page__title {
    font-size: var(--type-size-lg);
    font-weight: 600;
    margin: 0;
    color: var(--color-text-strong);
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }

  .mat-page__hint {
    font-size: var(--type-size-sm);
    font-weight: 400;
    color: var(--color-text-muted);
  }

  .mat-page__actions {
    display: flex;
    gap: var(--space-2);
  }

  .mat-page__refresh,
  .mat-page__add {
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    border-radius: var(--radius-sm);
    cursor: pointer;
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    transition: color var(--motion-fade-in);
  }

  .mat-page__add {
    color: var(--color-text-strong);
    border-color: var(--color-signal-positive);
  }

  .mat-page__refresh:hover:not(:disabled),
  .mat-page__add:hover:not(:disabled) {
    color: var(--color-signal-positive);
  }

  .mat-page__refresh:disabled,
  .mat-page__add:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }

  .mat-page__demo {
    padding: var(--space-2) var(--space-3);
    border: 1px dashed var(--color-signal-warning);
    background: var(--color-surface-raised);
    color: var(--color-signal-warning);
    border-radius: var(--radius-sm);
    font-size: var(--type-size-sm);
  }

  .mat-page__push {
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-raised);
    color: var(--color-text-muted);
    border-radius: var(--radius-sm);
    font-size: var(--type-size-sm);
  }

  .mat-page__push strong {
    color: var(--color-text-secondary);
  }

  .mat-page__muted {
    color: var(--color-text-muted);
    font-style: italic;
  }

  .mat-page__error {
    padding: var(--space-3);
    background: var(--color-surface-sunken);
    border: 1px solid var(--color-signal-negative);
    border-radius: var(--radius-sm);
    color: var(--color-text-primary);
  }

  .mat-page__error strong {
    color: var(--color-signal-negative);
  }

  .mat-page__error-detail {
    margin-top: var(--space-1);
    font-size: var(--type-size-sm);
    color: var(--color-text-muted);
  }

  .mat-page__empty {
    padding: var(--space-4);
    background: var(--color-surface-raised);
    border: 1px dashed var(--color-surface-divider);
    border-radius: var(--radius-sm);
    color: var(--color-text-secondary);
  }

  .mat-page__empty p + p {
    margin-top: var(--space-2);
  }

  .mat-table {
    width: 100%;
    border-collapse: collapse;
    font-size: var(--type-size-sm);
    background: var(--color-surface-sunken);
  }

  .mat-table th,
  .mat-table td {
    padding: var(--space-2) var(--space-3);
    text-align: left;
    border-bottom: 1px solid var(--color-surface-divider);
    vertical-align: top;
  }

  .mat-table td {
    color: var(--color-text-primary);
  }

  .mat-table td.num {
    text-align: right;
    font-family: var(--type-family-mono);
    color: var(--color-text-secondary);
  }

  .mat-table th {
    background: var(--color-surface-raised);
    padding: 0;
  }

  .sort-header {
    width: 100%;
    display: flex;
    gap: var(--space-1);
    align-items: center;
    padding: var(--space-2) var(--space-3);
    background: transparent;
    border: none;
    cursor: pointer;
    color: var(--color-text-muted);
    font-size: var(--type-size-xs);
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    font-family: var(--type-family-body);
  }

  .sort-header:hover {
    color: var(--color-text-strong);
  }

  .sort-indicator {
    color: var(--color-signal-positive);
  }

  .mat-table tbody tr:hover {
    background: var(--color-surface-raised);
  }

  .mat-table__grade {
    font-weight: 600;
    color: var(--color-text-strong);
    font-family: var(--type-family-mono);
  }

  .mat-table__updated {
    font-family: var(--type-family-mono);
    font-size: var(--type-size-xs);
    color: var(--color-text-muted);
  }

  .mat-chip {
    display: inline-block;
    padding: 2px 8px;
    border-radius: var(--radius-pill);
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    font-size: var(--type-size-xs);
    font-weight: 500;
    white-space: nowrap;
  }

  .mat-chip--positive {
    color: var(--color-signal-positive);
    border-color: var(--color-signal-positive);
  }

  .mat-chip--warning {
    color: var(--color-signal-warning);
    border-color: var(--color-signal-warning);
  }

  .mat-chip--neutral {
    color: var(--color-text-secondary);
  }

  .mat-chip--muted {
    color: var(--color-text-muted);
  }

  .mat-row__actions {
    display: flex;
    gap: var(--space-2);
    flex-wrap: wrap;
  }

  .mat-row__edit,
  .mat-row__delete {
    padding: var(--space-1) var(--space-2);
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    border-radius: var(--radius-sm);
    cursor: pointer;
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    transition: color var(--motion-fade-in);
  }

  .mat-row__edit:hover:not(:disabled) {
    color: var(--color-text-strong);
    border-color: var(--color-text-strong);
  }

  .mat-row__delete:hover:not(:disabled) {
    color: var(--color-signal-negative);
    border-color: var(--color-signal-negative);
  }

  .mat-row__edit:disabled,
  .mat-row__delete:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }
</style>
