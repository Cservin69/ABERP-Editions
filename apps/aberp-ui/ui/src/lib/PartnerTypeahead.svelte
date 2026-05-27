<script lang="ts">
  // PR-54 / session-74 — reusable partner-autocomplete input.
  //
  // Wired into IssueInvoice / ModificationInvoice forms' buyer
  // fieldset. Operator workflow: type 3+ characters of the buyer's
  // name → debounced 200ms → GET /api/partners?search=<query> → dropdown
  // of up to 8 matches → click (or keyboard-enter) → emit `select`
  // event → parent populates buyer fields from the chosen partner.
  //
  // Per the brief:
  //   - 3+ chars triggers a backend call.
  //   - 200ms debounce so an operator typing "BSCE" doesn't hit the
  //     backend four times.
  //   - Up to 8 visible matches in the dropdown.
  //   - "No match — use as one-off buyer" fallback at the bottom of
  //     the dropdown keeps the existing inline-edit posture (a buyer
  //     not in the partners list can still go on an invoice).
  //   - Keyboard nav: ArrowUp / ArrowDown / Enter / Escape.
  //
  // The component is dual-purpose:
  //   1. A search input that emits typed values upward (the parent
  //      keeps the existing text input for direct typing in fallback
  //      mode).
  //   2. A dropdown picker for selecting a saved partner.

  import { listPartners, type Partner } from "../lib/api";

  interface Props {
    /** Current text in the search box. Two-way bound so the parent
     * sees keystrokes (it uses them to populate the inline buyer name
     * field in fallback mode). */
    value: string;
    /** Invoked when the operator picks a partner from the dropdown.
     * The parent receives the full Partner and decides how to
     * populate its buyer fields (typically via
     * `buyerFieldsFromPartner`). */
    onSelect: (partner: Partner) => void;
    /** Invoked when the operator picks the "use as one-off buyer"
     * affordance. The parent leaves the typed `value` in place as the
     * buyer name and switches the form into manual-entry mode. */
    onUseAsOneOff?: () => void;
    /** Optional placeholder for the input. */
    placeholder?: string;
    /** Minimum prefix length before the backend is queried.
     * Defaults to 3 per the brief. */
    minChars?: number;
    /** Debounce delay in ms before the backend call fires. Defaults
     * to 200ms per the brief. */
    debounceMs?: number;
    /** Maximum rows in the dropdown. Defaults to 8 per the brief. */
    maxRows?: number;
    /** Optional input id (for label `for=` association). */
    inputId?: string;
    /** Optional aria-label when there's no associated visible label. */
    ariaLabel?: string;
  }

  let {
    value = $bindable(""),
    onSelect,
    onUseAsOneOff,
    placeholder = "Type 3+ characters of buyer name…",
    minChars = 3,
    debounceMs = 200,
    maxRows = 8,
    inputId,
    ariaLabel,
  }: Props = $props();

  let matches: Partner[] = $state([]);
  let dropdownOpen = $state(false);
  let highlight = $state(-1); // -1 = no row highlighted
  let queryError: string | null = $state(null);
  let inFlightSeq = 0;
  let debounceTimer: ReturnType<typeof setTimeout> | null = null;

  function clearDebounce() {
    if (debounceTimer !== null) {
      clearTimeout(debounceTimer);
      debounceTimer = null;
    }
  }

  function scheduleQuery(query: string) {
    clearDebounce();
    debounceTimer = setTimeout(() => {
      void runQuery(query);
    }, debounceMs);
  }

  async function runQuery(query: string) {
    const seq = ++inFlightSeq;
    queryError = null;
    try {
      const result = await listPartners(query);
      // Discard stale responses — a later keystroke's query may
      // resolve before an earlier slower one. Without this, the
      // dropdown would flash the wrong matches mid-typing.
      if (seq !== inFlightSeq) return;
      matches = result.slice(0, maxRows);
      highlight = matches.length > 0 ? 0 : -1;
    } catch (err: unknown) {
      if (seq !== inFlightSeq) return;
      matches = [];
      highlight = -1;
      queryError = err instanceof Error ? err.message : String(err);
    }
  }

  function onInput() {
    dropdownOpen = true;
    const trimmed = value.trim();
    if (trimmed.length < minChars) {
      matches = [];
      highlight = -1;
      queryError = null;
      clearDebounce();
      return;
    }
    scheduleQuery(trimmed);
  }

  function onFocus() {
    if (value.trim().length >= minChars) {
      dropdownOpen = true;
      if (matches.length > 0) {
        highlight = 0;
      }
    }
  }

  function onBlur() {
    // Delay closing so a mouse-down on a dropdown row still fires its
    // click handler before the dropdown disappears.
    setTimeout(() => {
      dropdownOpen = false;
    }, 150);
  }

  function onKeyDown(event: KeyboardEvent) {
    const rowsAvailable = matches.length > 0;
    const totalRows = matches.length + (canUseOneOff ? 1 : 0);
    switch (event.key) {
      case "ArrowDown":
        if (totalRows === 0) return;
        event.preventDefault();
        dropdownOpen = true;
        highlight = (highlight + 1) % totalRows;
        break;
      case "ArrowUp":
        if (totalRows === 0) return;
        event.preventDefault();
        dropdownOpen = true;
        highlight = (highlight - 1 + totalRows) % totalRows;
        break;
      case "Enter":
        if (!dropdownOpen || totalRows === 0) return;
        event.preventDefault();
        if (highlight >= 0 && highlight < matches.length) {
          pickPartner(matches[highlight]);
        } else if (canUseOneOff && highlight === matches.length) {
          pickOneOff();
        } else if (rowsAvailable) {
          pickPartner(matches[0]);
        }
        break;
      case "Escape":
        dropdownOpen = false;
        highlight = -1;
        break;
      default:
        // Other keys (alphanumeric) flow through to onInput.
        break;
    }
  }

  function pickPartner(partner: Partner) {
    dropdownOpen = false;
    highlight = -1;
    onSelect(partner);
  }

  function pickOneOff() {
    dropdownOpen = false;
    highlight = -1;
    if (onUseAsOneOff !== undefined) onUseAsOneOff();
  }

  const canUseOneOff = $derived(
    onUseAsOneOff !== undefined && value.trim().length >= minChars,
  );

  const showDropdown = $derived(
    dropdownOpen &&
      value.trim().length >= minChars &&
      (matches.length > 0 || canUseOneOff || queryError !== null),
  );
</script>

<div class="typeahead">
  <input
    type="text"
    id={inputId}
    aria-label={ariaLabel}
    bind:value
    {placeholder}
    autocomplete="off"
    spellcheck="false"
    role="combobox"
    oninput={onInput}
    onfocus={onFocus}
    onblur={onBlur}
    onkeydown={onKeyDown}
    aria-autocomplete="list"
    aria-expanded={showDropdown}
    aria-controls="partner-typeahead-listbox"
  />

  {#if showDropdown}
    <ul
      id="partner-typeahead-listbox"
      class="dropdown"
      role="listbox"
    >
      {#if queryError !== null}
        <li class="error" aria-live="polite">{queryError}</li>
      {/if}
      {#each matches as match, i (match.id)}
        <li
          class="row"
          role="option"
          aria-selected={i === highlight}
          data-highlight={i === highlight}
          onmousedown={(e) => {
            // mousedown rather than click so the input's blur (which
            // fires before click) doesn't close the dropdown first.
            e.preventDefault();
            pickPartner(match);
          }}
        >
          <span class="row__name">{match.display_name}</span>
          <span class="row__hint">
            ({match.legal_name}, {match.tax_number})
          </span>
        </li>
      {/each}
      {#if canUseOneOff}
        <li
          class="row row--oneoff"
          role="option"
          aria-selected={highlight === matches.length}
          data-highlight={highlight === matches.length}
          onmousedown={(e) => {
            e.preventDefault();
            pickOneOff();
          }}
        >
          <span class="row__name">No match — use as one-off buyer</span>
          <span class="row__hint">
            Fill the buyer fields below manually.
          </span>
        </li>
      {/if}
    </ul>
  {/if}
</div>

<style>
  .typeahead {
    position: relative;
    display: flex;
    flex-direction: column;
  }

  .typeahead input {
    background: var(--color-surface-raised);
    color: var(--color-text-strong);
    border: 1px solid var(--color-surface-divider);
    padding: var(--space-1) var(--space-2);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
  }

  .typeahead input:focus-visible {
    outline: 1px solid var(--color-text-muted);
    outline-offset: 1px;
    border-color: var(--color-text-muted);
  }

  .dropdown {
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

  .row {
    padding: var(--space-2) var(--space-3);
    cursor: pointer;
    display: flex;
    flex-direction: column;
    gap: 2px;
    border-bottom: 1px solid var(--color-surface-divider);
  }

  .row:last-child {
    border-bottom: none;
  }

  .row[data-highlight="true"] {
    background: var(--color-surface-divider);
  }

  .row__name {
    color: var(--color-text-strong);
    font-size: var(--type-size-sm);
    font-weight: 500;
  }

  .row__hint {
    color: var(--color-text-muted);
    font-size: var(--type-size-xs);
    font-family: var(--type-family-mono);
  }

  .row--oneoff .row__name {
    color: var(--color-text-secondary);
    font-style: italic;
    font-weight: 400;
  }

  .error {
    padding: var(--space-2) var(--space-3);
    color: var(--color-signal-negative);
    font-size: var(--type-size-xs);
    font-family: var(--type-family-mono);
  }
</style>
