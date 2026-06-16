<script lang="ts">
  // S439 — Quality module: NCR + CAPA workflow.
  //   1. Open #/quality-ncrs. Page lists NCRs (filterable by state, severity,
  //      date range, part UID).
  //   2. "+ New NCR" → inline create form (severity, category, description,
  //      affected part-UIDs / WOs / heat-lots, photos).
  //   3. Click a row → detail panel: transition timeline + linked CAPAs +
  //      state-transition buttons (only the allowed next states) + CAPA create
  //      / approve / review / close. Closing the NCR needs an approved +
  //      effectiveness-Verified CAPA (the backend enforces it; a 409 surfaces
  //      here as a banner).

  import { onMount } from "svelte";
  import {
    listNcrs,
    createNcr,
    getNcr,
    transitionNcr,
    createCapa,
    approveCapa,
    reviewCapa,
    closeCapa,
    type Ncr,
    type NcrDetail,
    type NcrSeverity,
    type NcrCategory,
    type NcrState,
    type CapaVerdict,
    type PhotoUpload,
  } from "../lib/api";
  import {
    SEVERITY_LABELS,
    CATEGORY_LABELS,
    STATE_LABELS,
    VERDICT_LABELS,
    allowedNextStates,
    capaPermitsClose,
    validateNcrDescription,
    splitList,
  } from "../lib/ncr";

  const SEVERITIES: NcrSeverity[] = ["critical", "major", "minor"];
  const CATEGORIES: NcrCategory[] = [
    "material",
    "workmanship",
    "documentation",
    "equipment_failure",
    "operator_error",
    "supplier_issue",
    "other",
  ];
  const STATES: NcrState[] = [
    "open",
    "contained",
    "under_investigation",
    "correction_applied",
    "closed",
    "escalated",
  ];
  const REVIEW_VERDICTS: CapaVerdict[] = ["verified", "not_effective"];

  let rows: Ncr[] = $state([]);
  let loadState: "loading" | "loaded" | "error" = $state("loading");
  let loadError: string | null = $state(null);

  // Filters (server-side scan).
  let fState = $state("");
  let fSeverity = $state("");
  let fFrom = $state("");
  let fTo = $state("");
  let fPartUid = $state("");

  // Detail panel.
  let selected: NcrDetail | null = $state(null);
  let detailError: string | null = $state(null);

  // Create form.
  let showCreate = $state(false);
  let cSeverity: NcrSeverity = $state("major");
  let cCategory: NcrCategory = $state("workmanship");
  let cDescription = $state("");
  let cPartUids = $state("");
  let cWoIds = $state("");
  let cHeatLots = $state("");
  let cPhotos: PhotoUpload[] = $state([]);
  let createError: string | null = $state(null);
  let creating = $state(false);

  // Transition note + CAPA form inputs (detail panel).
  let transitionNote = $state("");
  let capCorrective = $state("");
  let capPreventive = $state("");
  let capResponsible = $state("");
  let capTargetDate = $state("");

  // Per-CAPA review inputs keyed by capa id.
  let reviewVerdict: Record<string, CapaVerdict> = $state({});
  let reviewComment: Record<string, string> = $state({});

  onMount(() => {
    void loadList();
  });

  async function loadList() {
    loadState = "loading";
    loadError = null;
    try {
      const resp = await listNcrs({
        stateFilter: fState || undefined,
        severity: fSeverity || undefined,
        from: fFrom || undefined,
        to: fTo || undefined,
        partUid: fPartUid || undefined,
      });
      rows = resp.ncrs;
      loadState = "loaded";
    } catch (err: unknown) {
      loadState = "error";
      loadError = err instanceof Error ? err.message : String(err);
    }
  }

  function clearFilters() {
    fState = "";
    fSeverity = "";
    fFrom = "";
    fTo = "";
    fPartUid = "";
    void loadList();
  }

  async function openDetail(ncrId: string) {
    detailError = null;
    try {
      selected = await getNcr(ncrId);
      resetDetailInputs();
    } catch (err: unknown) {
      detailError = err instanceof Error ? err.message : String(err);
    }
  }

  function resetDetailInputs() {
    transitionNote = "";
    capCorrective = "";
    capPreventive = "";
    capResponsible = "";
    capTargetDate = "";
  }

  async function refreshDetail() {
    if (selected) {
      try {
        selected = await getNcr(selected.ncr_id);
      } catch (err: unknown) {
        detailError = err instanceof Error ? err.message : String(err);
      }
    }
  }

  function closeDetail() {
    selected = null;
    detailError = null;
  }

  // ── Create NCR ──────────────────────────────────────────────────

  function onPhotoChange(event: Event) {
    const input = event.currentTarget as HTMLInputElement;
    const files = Array.from(input.files ?? []);
    const next: PhotoUpload[] = [];
    let pending = files.length;
    if (pending === 0) {
      cPhotos = [];
      return;
    }
    for (const f of files) {
      const reader = new FileReader();
      reader.onload = () => {
        const result = String(reader.result ?? "");
        // Strip the `data:<mime>;base64,` prefix → raw base64.
        const comma = result.indexOf(",");
        const b64 = comma >= 0 ? result.slice(comma + 1) : result;
        next.push({ filename: f.name, data_base64: b64 });
        pending -= 1;
        if (pending === 0) {
          cPhotos = next;
        }
      };
      reader.readAsDataURL(f);
    }
  }

  async function submitCreate() {
    createError = null;
    const descErr = validateNcrDescription(cDescription);
    if (descErr) {
      createError = descErr;
      return;
    }
    creating = true;
    try {
      await createNcr({
        severity: cSeverity,
        category: cCategory,
        description: cDescription.trim(),
        affected_part_uids: splitList(cPartUids),
        affected_wo_ids: splitList(cWoIds),
        affected_heat_lots: splitList(cHeatLots),
        photos: cPhotos,
      });
      // Reset + close + reload.
      cDescription = "";
      cPartUids = "";
      cWoIds = "";
      cHeatLots = "";
      cPhotos = [];
      cSeverity = "major";
      cCategory = "workmanship";
      showCreate = false;
      await loadList();
    } catch (err: unknown) {
      createError = err instanceof Error ? err.message : String(err);
    } finally {
      creating = false;
    }
  }

  // ── Transitions + CAPA actions ──────────────────────────────────

  async function doTransition(to: NcrState) {
    if (!selected) return;
    detailError = null;
    try {
      await transitionNcr(selected.ncr_id, { to_state: to, note: transitionNote });
      transitionNote = "";
      await refreshDetail();
      await loadList();
    } catch (err: unknown) {
      detailError = err instanceof Error ? err.message : String(err);
    }
  }

  async function submitCapa() {
    if (!selected) return;
    detailError = null;
    try {
      await createCapa(selected.ncr_id, {
        corrective_action_text: capCorrective.trim(),
        preventive_action_text: capPreventive.trim(),
        responsible_operator: capResponsible.trim(),
        target_close_date: capTargetDate.trim(),
      });
      capCorrective = "";
      capPreventive = "";
      capResponsible = "";
      capTargetDate = "";
      await refreshDetail();
    } catch (err: unknown) {
      detailError = err instanceof Error ? err.message : String(err);
    }
  }

  async function doApprove(capaId: string) {
    detailError = null;
    try {
      await approveCapa(capaId);
      await refreshDetail();
    } catch (err: unknown) {
      detailError = err instanceof Error ? err.message : String(err);
    }
  }

  async function doReview(capaId: string) {
    detailError = null;
    const verdict = reviewVerdict[capaId] ?? "verified";
    const comment = reviewComment[capaId] ?? "";
    try {
      await reviewCapa(capaId, { verdict, comment });
      await refreshDetail();
    } catch (err: unknown) {
      detailError = err instanceof Error ? err.message : String(err);
    }
  }

  async function doCloseCapa(capaId: string) {
    detailError = null;
    try {
      await closeCapa(capaId);
      await refreshDetail();
    } catch (err: unknown) {
      detailError = err instanceof Error ? err.message : String(err);
    }
  }

  function severityChipClass(s: NcrSeverity): string {
    if (s === "critical") return "chip chip--err";
    if (s === "major") return "chip chip--warning";
    return "chip chip--neutral";
  }
  function stateChipClass(s: NcrState): string {
    if (s === "escalated") return "chip chip--err";
    if (s === "closed") return "chip chip--ok";
    return "chip chip--neutral";
  }
</script>

<section class="page" aria-labelledby="page-title">
  <header class="page__head">
    <div class="page__head-row">
      <h2 id="page-title" class="page__title">Minőség — NCR-ek / Quality — NCRs</h2>
      <button type="button" class="page__primary" onclick={() => (showCreate = !showCreate)}>
        {showCreate ? "Mégse / Cancel" : "+ Új NCR / New NCR"}
      </button>
    </div>
    <p class="page__lede">
      Nem-megfelelőségi jelentések (NCR) és helyesbítő/megelőző intézkedések
      (CAPA). / Non-conformance reports and corrective/preventive actions. An
      open or contained NCR on a defense part blocks its shipment until resolved.
    </p>
  </header>

  {#if showCreate}
    <form class="create" onsubmit={(e) => { e.preventDefault(); void submitCreate(); }}>
      <div class="create__row">
        <label class="field">
          <span class="field-label">Súlyosság / Severity</span>
          <select bind:value={cSeverity}>
            {#each SEVERITIES as s (s)}
              <option value={s}>{SEVERITY_LABELS[s]}</option>
            {/each}
          </select>
        </label>
        <label class="field">
          <span class="field-label">Kategória / Category</span>
          <select bind:value={cCategory}>
            {#each CATEGORIES as c (c)}
              <option value={c}>{CATEGORY_LABELS[c]}</option>
            {/each}
          </select>
        </label>
      </div>
      <label class="field">
        <span class="field-label">Leírás / Description</span>
        <textarea bind:value={cDescription} rows="3" placeholder="Mi nem felelt meg? / What failed?"></textarea>
      </label>
      <div class="create__row">
        <label class="field">
          <span class="field-label">Érintett part UID-k / Part UIDs</span>
          <textarea bind:value={cPartUids} rows="2" placeholder="dp-…, dp-… (vesszővel / comma or newline)"></textarea>
        </label>
        <label class="field">
          <span class="field-label">Munkalapok / Work orders</span>
          <textarea bind:value={cWoIds} rows="2" placeholder="wo-…"></textarea>
        </label>
        <label class="field">
          <span class="field-label">Olvasztási tételek / Heat lots</span>
          <textarea bind:value={cHeatLots} rows="2" placeholder="HEAT-…"></textarea>
        </label>
      </div>
      <label class="field">
        <span class="field-label">Fotók / Photos</span>
        <input type="file" accept="image/*" multiple onchange={onPhotoChange} />
        {#if cPhotos.length > 0}
          <span class="page__muted">{cPhotos.length} fotó kiválasztva / photo(s) selected</span>
        {/if}
      </label>
      {#if createError}
        <p class="confirm__error" role="alert">{createError}</p>
      {/if}
      <div class="create__buttons">
        <button type="button" class="quiet-button" onclick={() => (showCreate = false)}>Mégse / Cancel</button>
        <button type="submit" class="page__primary" disabled={creating}>
          {creating ? "Mentés… / Saving…" : "NCR létrehozása / Create NCR"}
        </button>
      </div>
    </form>
  {/if}

  <div class="page__toolbar">
    <label class="filter">
      <span class="filter-label">State</span>
      <select bind:value={fState} onchange={() => void loadList()}>
        <option value="">All</option>
        {#each STATES as s (s)}
          <option value={s}>{STATE_LABELS[s]}</option>
        {/each}
      </select>
    </label>
    <label class="filter">
      <span class="filter-label">Severity</span>
      <select bind:value={fSeverity} onchange={() => void loadList()}>
        <option value="">All</option>
        {#each SEVERITIES as s (s)}
          <option value={s}>{SEVERITY_LABELS[s]}</option>
        {/each}
      </select>
    </label>
    <label class="filter">
      <span class="filter-label">From</span>
      <input type="date" bind:value={fFrom} onchange={() => void loadList()} />
    </label>
    <label class="filter">
      <span class="filter-label">To</span>
      <input type="date" bind:value={fTo} onchange={() => void loadList()} />
    </label>
    <label class="filter">
      <span class="filter-label">Part UID</span>
      <input type="search" bind:value={fPartUid} placeholder="dp-…" oninput={() => void loadList()} />
    </label>
    <button type="button" class="quiet-button" onclick={clearFilters}>Clear</button>
  </div>

  {#if loadState === "loading"}
    <p class="page__muted">Loading…</p>
  {:else if loadState === "error"}
    <div class="page__error" role="alert">
      <strong>Could not load NCRs.</strong>
      <p class="page__error-detail">{loadError}</p>
    </div>
  {:else if rows.length === 0}
    <div class="page__empty">
      <p>Nincs NCR. / No NCRs yet.</p>
    </div>
  {:else}
    <table class="ncrs-table">
      <thead>
        <tr>
          <th scope="col">NCR</th>
          <th scope="col">Severity</th>
          <th scope="col">Category</th>
          <th scope="col">State</th>
          <th scope="col">Discovered</th>
          <th scope="col">By</th>
          <th scope="col">Parts</th>
        </tr>
      </thead>
      <tbody>
        {#each rows as n (n.ncr_id)}
          <tr
            class={selected?.ncr_id === n.ncr_id ? "is-selected" : ""}
            class:is-escalated={n.state === "escalated"}
            onclick={() => void openDetail(n.ncr_id)}
          >
            <td class="mono">{n.ncr_id.slice(0, 12)}…</td>
            <td><span class={severityChipClass(n.severity)}>{SEVERITY_LABELS[n.severity]}</span></td>
            <td>{CATEGORY_LABELS[n.category]}</td>
            <td><span class={stateChipClass(n.state)}>{STATE_LABELS[n.state]}</span></td>
            <td class="mono">{n.discovered_at_utc.slice(0, 10)}</td>
            <td class="mono">{n.discovered_by_operator}</td>
            <td class="mono">{n.affected_part_uids.length}</td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}
</section>

{#if selected}
  {@const ncr = selected}
  <section class="detail" aria-label="NCR detail">
    <div class="detail__head">
      <h3 class="detail__title">
        <span class="mono">{ncr.ncr_id}</span>
        <span class={stateChipClass(ncr.state)}>{STATE_LABELS[ncr.state]}</span>
      </h3>
      <button type="button" class="quiet-button" onclick={closeDetail}>Bezárás / Close</button>
    </div>

    {#if detailError}
      <p class="confirm__error" role="alert">{detailError}</p>
    {/if}

    <dl class="detail__grid">
      <dt>Severity</dt><dd>{SEVERITY_LABELS[ncr.severity]}</dd>
      <dt>Category</dt><dd>{CATEGORY_LABELS[ncr.category]}</dd>
      <dt>Discovered</dt><dd class="mono">{ncr.discovered_at_utc} — {ncr.discovered_by_operator}</dd>
      <dt>Description</dt><dd>{ncr.description}</dd>
      <dt>Part UIDs</dt><dd class="mono">{ncr.affected_part_uids.join(", ") || "—"}</dd>
      <dt>Work orders</dt><dd class="mono">{ncr.affected_wo_ids.join(", ") || "—"}</dd>
      <dt>Heat lots</dt><dd class="mono">{ncr.affected_heat_lots.join(", ") || "—"}</dd>
      <dt>Photos</dt><dd class="mono">{ncr.photos.join(", ") || "—"}</dd>
      {#if ncr.closed_at_utc}
        <dt>Closed</dt><dd class="mono">{ncr.closed_at_utc} — {ncr.closed_by_operator}</dd>
      {/if}
    </dl>

    <!-- State transitions -->
    {#if allowedNextStates(ncr.state).length > 0}
      <div class="detail__section">
        <h4 class="detail__subtitle">Állapotváltás / Transition</h4>
        <textarea bind:value={transitionNote} rows="2" placeholder="Megjegyzés / Note (optional)"></textarea>
        <div class="row-actions">
          {#each allowedNextStates(ncr.state) as to (to)}
            <button
              type="button"
              class={to === "escalated" ? "quiet-button danger" : "quiet-button"}
              onclick={() => void doTransition(to)}
            >
              → {STATE_LABELS[to]}
            </button>
          {/each}
        </div>
      </div>
    {/if}

    <!-- Transition timeline -->
    <div class="detail__section">
      <h4 class="detail__subtitle">Idővonal / Timeline</h4>
      <ol class="timeline">
        {#each ncr.transitions as t (t.seq)}
          <li>
            <span class="mono">{t.at_utc}</span>
            — {t.from_state || "∅"} → <strong>{t.to_state}</strong>
            <span class="page__muted">({t.operator}){t.note ? ` · ${t.note}` : ""}</span>
          </li>
        {/each}
      </ol>
    </div>

    <!-- Linked CAPAs -->
    <div class="detail__section">
      <h4 class="detail__subtitle">CAPA-k / CAPAs</h4>
      {#if ncr.capas.length === 0}
        <p class="page__muted">Nincs CAPA. / No CAPAs yet.</p>
      {:else}
        {#each ncr.capas as c (c.capa_id)}
          <div class="capa">
            <div class="capa__head">
              <span class="mono">{c.capa_id.slice(0, 14)}…</span>
              <span class="chip {c.effectiveness_verdict === 'verified' ? 'chip--ok' : c.effectiveness_verdict === 'not_effective' ? 'chip--err' : 'chip--neutral'}">
                {VERDICT_LABELS[c.effectiveness_verdict]}
              </span>
              {#if capaPermitsClose(c)}
                <span class="chip chip--ok" title="Permits NCR close">✓ permits close</span>
              {/if}
            </div>
            <p class="capa__text"><strong>Corrective:</strong> {c.corrective_action_text}</p>
            <p class="capa__text"><strong>Preventive:</strong> {c.preventive_action_text}</p>
            <p class="page__muted">
              Responsible: {c.responsible_operator || "—"} · Target: {c.target_close_date || "—"}
              {#if c.approved_at_utc}· Approved: {c.approved_by_operator}{/if}
              {#if c.actual_close_date}· Closed: {c.actual_close_date}{/if}
            </p>
            <div class="row-actions">
              {#if !c.approved_at_utc}
                <button type="button" class="quiet-button" onclick={() => void doApprove(c.capa_id)}>Approve</button>
              {/if}
              <label class="filter">
                <span class="visually-hidden">Verdict</span>
                <select bind:value={reviewVerdict[c.capa_id]}>
                  {#each REVIEW_VERDICTS as v (v)}
                    <option value={v}>{VERDICT_LABELS[v]}</option>
                  {/each}
                </select>
              </label>
              <input
                type="text"
                class="reason-input"
                placeholder="Review comment…"
                bind:value={reviewComment[c.capa_id]}
              />
              <button type="button" class="quiet-button" onclick={() => void doReview(c.capa_id)}>Review</button>
              {#if !c.actual_close_date}
                <button type="button" class="quiet-button" onclick={() => void doCloseCapa(c.capa_id)}>Close CAPA</button>
              {/if}
            </div>
          </div>
        {/each}
      {/if}

      <!-- Create CAPA -->
      <form class="capa-form" onsubmit={(e) => { e.preventDefault(); void submitCapa(); }}>
        <h4 class="detail__subtitle">+ Új CAPA / New CAPA</h4>
        <textarea bind:value={capCorrective} rows="2" placeholder="Corrective action…"></textarea>
        <textarea bind:value={capPreventive} rows="2" placeholder="Preventive action…"></textarea>
        <div class="create__row">
          <input type="text" class="reason-input" bind:value={capResponsible} placeholder="Responsible operator" />
          <input type="date" class="reason-input" bind:value={capTargetDate} />
          <button type="submit" class="quiet-button">Create CAPA</button>
        </div>
      </form>
    </div>
  </section>
{/if}

<style>
  .page {
    max-width: 1200px;
    margin: 0 auto;
  }
  .page__head {
    margin-bottom: var(--space-4);
  }
  .page__head-row {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: var(--space-3);
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
  .page__toolbar {
    margin-bottom: var(--space-3);
    display: flex;
    align-items: center;
    gap: var(--space-3);
    flex-wrap: wrap;
  }
  .page__muted {
    color: var(--color-text-muted);
    font-size: var(--type-size-sm);
  }
  .page__empty {
    padding: var(--space-5);
    border: 1px dashed var(--color-surface-divider);
    background: var(--color-surface-raised);
    text-align: center;
    color: var(--color-text-secondary);
  }
  .page__primary {
    padding: var(--space-2) var(--space-4);
    background: var(--color-signal-positive, var(--color-text-strong));
    color: var(--color-surface-base, white);
    border: 0;
    border-radius: 4px;
    font-size: var(--type-size-sm);
    font-weight: 500;
    cursor: pointer;
  }
  .page__primary:hover:not(:disabled) {
    opacity: 0.9;
  }
  .page__primary:disabled {
    opacity: 0.5;
    cursor: not-allowed;
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
  .ncrs-table {
    width: 100%;
    border-collapse: collapse;
    font-size: var(--type-size-sm);
  }
  .ncrs-table th,
  .ncrs-table td {
    text-align: left;
    padding: var(--space-2) var(--space-3);
    border-bottom: 1px solid var(--color-surface-divider);
    color: var(--color-text-primary);
    vertical-align: top;
  }
  .ncrs-table th {
    color: var(--color-text-secondary);
    font-weight: 500;
    background: var(--color-surface-raised);
    text-transform: uppercase;
    letter-spacing: 0.06em;
    font-size: var(--type-size-xs);
  }
  .ncrs-table tbody tr {
    cursor: pointer;
  }
  .ncrs-table tbody tr:hover {
    background: var(--color-surface-raised);
  }
  .ncrs-table tbody tr.is-selected {
    background: var(--color-surface-raised);
  }
  .ncrs-table tbody tr.is-escalated td:first-child {
    border-left: 3px solid var(--color-signal-negative);
  }
  td.mono,
  .mono {
    font-family: var(--type-family-mono);
    color: var(--color-text-strong);
  }
  .filter {
    display: inline-flex;
    align-items: center;
    gap: var(--space-2);
    font-size: var(--type-size-sm);
    color: var(--color-text-secondary);
  }
  .filter-label {
    text-transform: uppercase;
    font-size: var(--type-size-xs);
    letter-spacing: 0.06em;
  }
  .filter select,
  .filter input {
    background: var(--color-surface-raised);
    color: var(--color-text-strong);
    border: 1px solid var(--color-surface-divider);
    padding: var(--space-1) var(--space-2);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
  }
  .quiet-button {
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    border: 1px solid var(--color-surface-divider);
    padding: var(--space-1) var(--space-3);
    font-family: var(--type-family-body);
    font-size: var(--type-size-sm);
    cursor: pointer;
    border-radius: 4px;
  }
  .quiet-button:hover {
    color: var(--color-text-strong);
  }
  .quiet-button.danger {
    color: var(--color-signal-negative);
    border-color: var(--color-signal-negative);
  }
  .row-actions {
    display: flex;
    gap: var(--space-2);
    align-items: center;
    flex-wrap: wrap;
    margin-top: var(--space-2);
  }
  .chip {
    display: inline-block;
    padding: 0 var(--space-2);
    border-radius: 12px;
    border: 1px solid var(--color-surface-divider);
    font-size: var(--type-size-xs);
    font-weight: 500;
  }
  .chip--ok {
    color: var(--color-signal-positive, var(--color-text-strong));
    border-color: var(--color-signal-positive, var(--color-text-strong));
  }
  .chip--err {
    color: var(--color-signal-negative);
    border-color: var(--color-signal-negative);
  }
  .chip--warning {
    color: var(--color-signal-warning);
    border-color: var(--color-signal-warning);
  }
  .chip--neutral {
    color: var(--color-text-muted);
  }
  /* Create form */
  .create {
    margin-bottom: var(--space-4);
    padding: var(--space-3);
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-raised);
    border-radius: 4px;
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }
  .create__row {
    display: flex;
    gap: var(--space-3);
    flex-wrap: wrap;
  }
  .create__buttons {
    display: flex;
    gap: var(--space-2);
    justify-content: flex-end;
  }
  .field {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    flex: 1;
    min-width: 180px;
  }
  .field-label {
    text-transform: uppercase;
    font-size: var(--type-size-xs);
    letter-spacing: 0.06em;
    color: var(--color-text-secondary);
  }
  .field select,
  .field textarea,
  .field input {
    background: var(--color-surface-base, var(--color-surface-raised));
    color: var(--color-text-strong);
    border: 1px solid var(--color-surface-divider);
    padding: var(--space-2);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
    border-radius: 4px;
  }
  /* Detail panel */
  .detail {
    max-width: 1200px;
    margin: var(--space-4) auto 0;
    padding: var(--space-4);
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-raised);
    border-radius: 4px;
  }
  .detail__head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-3);
    margin-bottom: var(--space-3);
  }
  .detail__title {
    margin: 0;
    font-size: var(--type-size-md);
    display: flex;
    gap: var(--space-2);
    align-items: center;
  }
  .detail__grid {
    display: grid;
    grid-template-columns: max-content 1fr;
    gap: var(--space-1) var(--space-3);
    margin: 0 0 var(--space-3) 0;
    font-size: var(--type-size-sm);
  }
  .detail__grid dt {
    color: var(--color-text-secondary);
    text-transform: uppercase;
    font-size: var(--type-size-xs);
    letter-spacing: 0.06em;
  }
  .detail__grid dd {
    margin: 0;
    color: var(--color-text-primary);
    word-break: break-word;
  }
  .detail__section {
    margin-top: var(--space-4);
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  .detail__subtitle {
    margin: 0;
    font-size: var(--type-size-sm);
    color: var(--color-text-secondary);
    text-transform: uppercase;
    letter-spacing: 0.06em;
  }
  .detail textarea,
  .reason-input {
    background: var(--color-surface-base, var(--color-surface-raised));
    color: var(--color-text-strong);
    border: 1px solid var(--color-surface-divider);
    padding: var(--space-2);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
    border-radius: 4px;
  }
  .timeline {
    margin: 0;
    padding-left: var(--space-4);
    font-size: var(--type-size-sm);
    color: var(--color-text-primary);
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }
  .capa {
    padding: var(--space-3);
    border: 1px solid var(--color-surface-divider);
    border-radius: 4px;
    margin-bottom: var(--space-2);
  }
  .capa__head {
    display: flex;
    gap: var(--space-2);
    align-items: center;
    margin-bottom: var(--space-2);
  }
  .capa__text {
    margin: var(--space-1) 0;
    font-size: var(--type-size-sm);
    color: var(--color-text-primary);
  }
  .capa-form {
    margin-top: var(--space-3);
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    padding-top: var(--space-3);
    border-top: 1px solid var(--color-surface-divider);
  }
  .confirm__error {
    margin: 0;
    font-size: var(--type-size-xs);
    color: var(--color-signal-negative);
    font-family: var(--type-family-mono);
    word-break: break-word;
  }
  .visually-hidden {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip: rect(0 0 0 0);
    white-space: nowrap;
  }
</style>
