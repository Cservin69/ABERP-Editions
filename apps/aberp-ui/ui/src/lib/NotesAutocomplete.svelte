<script lang="ts">
  // PR-172 — buyer-facing notes typeahead.
  //
  // Wraps a <textarea> with a startsWith-prefix dropdown sourced from
  // the operator's prior notes for the given scope. Three SPA call
  // sites:
  //   - per-line note on IssueInvoice (scope="line")
  //   - per-invoice note on IssueInvoice (scope="invoice")
  //   - storno reason on InvoiceDetail (scope="storno")
  //
  // History is fetched lazily on first focus (no pre-fetch on form
  // open — conservative on network and on the audit-ledger scan). The
  // dropdown filters client-side per `filterNotesByPrefix`; the
  // backend list is the deduped most-recent-N (default 50) which the
  // operator's typing narrows further.

  import { listNotesHistory, type NotesHistoryScope } from "./api";
  import { filterNotesByPrefix } from "./notes-autocomplete";

  interface Props {
    /** Textarea content, two-way bound to the parent's form field. */
    value: string;
    /** Which history bucket to consult — see NotesHistoryScope. */
    scope: NotesHistoryScope;
    /** Placeholder forwarded to the textarea. */
    placeholder?: string;
    /** Row count for the textarea. Mirrors the bare <textarea rows=…>
     * the call sites used before PR-172 so swapping in the component
     * does not change line-height. */
    rows?: number;
    /** Maxlength forwarded to the textarea (NAV note columns are
     * bounded to 4000 chars; line notes to 2000). */
    maxlength?: number;
    /** Optional data-testid on the underlying textarea so the
     * existing IssueInvoice / InvoiceDetail test selectors keep
     * working post-swap. */
    testid?: string;
    /** Maximum dropdown rows surfaced after filtering. Defaults to 10
     * — enough to be useful, small enough to fit on screen. */
    maxRows?: number;
    /** Optional id forwarded to the textarea for <label for=…>. */
    inputId?: string;
    /** Optional aria-label when there is no associated visible label. */
    ariaLabel?: string;
  }

  let {
    value = $bindable(""),
    scope,
    placeholder = "",
    rows = 3,
    maxlength,
    testid,
    maxRows = 10,
    inputId,
    ariaLabel,
  }: Props = $props();

  let history: string[] = $state([]);
  let historyLoaded = $state(false);
  let historyLoading = $state(false);
  let dropdownOpen = $state(false);
  let highlight = $state(-1);
  let textarea: HTMLTextAreaElement | undefined = $state();

  const suggestions = $derived(filterNotesByPrefix(history, value, maxRows));

  // Reset the highlight whenever the surfaced suggestions change so
  // the keyboard nav never points off the end of the list.
  $effect(() => {
    if (suggestions.length === 0) {
      highlight = -1;
    } else if (highlight >= suggestions.length) {
      highlight = 0;
    }
  });

  async function ensureHistoryLoaded() {
    if (historyLoaded || historyLoading) return;
    historyLoading = true;
    try {
      history = await listNotesHistory(scope);
      historyLoaded = true;
    } catch {
      // Silent-degrade: a failed history fetch must NOT block the
      // operator from typing. The textarea stays functional, just
      // without suggestions. We do not surface an error here — the
      // operator never asked to see history; it was an opt-in
      // affordance.
      history = [];
      historyLoaded = true;
    } finally {
      historyLoading = false;
    }
  }

  function onFocus() {
    void ensureHistoryLoaded();
    if (suggestions.length > 0) {
      dropdownOpen = true;
    }
  }

  function onInput() {
    void ensureHistoryLoaded();
    dropdownOpen = true;
  }

  function onBlur() {
    // Delay so a mousedown on a dropdown row still fires before the
    // dropdown disappears (mirrors PartnerTypeahead's posture).
    setTimeout(() => {
      dropdownOpen = false;
    }, 150);
  }

  function pickSuggestion(text: string) {
    value = text;
    dropdownOpen = false;
    highlight = -1;
    // Re-focus the textarea so the operator can keep typing after a
    // pick without grabbing the mouse again.
    textarea?.focus();
  }

  function onKeyDown(event: KeyboardEvent) {
    const total = suggestions.length;
    switch (event.key) {
      case "ArrowDown":
        if (total === 0) return;
        event.preventDefault();
        dropdownOpen = true;
        highlight = (Math.max(0, highlight) + 1) % total;
        break;
      case "ArrowUp":
        if (total === 0) return;
        event.preventDefault();
        dropdownOpen = true;
        highlight = (highlight <= 0 ? total : highlight) - 1;
        break;
      case "Enter":
        // Only intercept Enter when the operator has the dropdown
        // open AND has explicitly highlighted a row. Otherwise let
        // the native newline fire — these are multi-line buyer-
        // facing notes; swallowing Enter unconditionally would
        // surprise the operator typing a paragraph break.
        if (!dropdownOpen || highlight < 0 || highlight >= total) return;
        event.preventDefault();
        pickSuggestion(suggestions[highlight]);
        break;
      case "Escape":
        dropdownOpen = false;
        highlight = -1;
        break;
      default:
        break;
    }
  }

  const showDropdown = $derived(dropdownOpen && suggestions.length > 0);
</script>

<div class="notes-autocomplete">
  <textarea
    bind:this={textarea}
    bind:value
    id={inputId}
    aria-label={ariaLabel}
    {placeholder}
    {rows}
    maxlength={maxlength ?? undefined}
    data-testid={testid}
    autocomplete="off"
    role="combobox"
    aria-autocomplete="list"
    aria-expanded={showDropdown}
    aria-controls="notes-autocomplete-listbox"
    oninput={onInput}
    onfocus={onFocus}
    onblur={onBlur}
    onkeydown={onKeyDown}
  ></textarea>

  {#if showDropdown}
    <ul
      id="notes-autocomplete-listbox"
      class="dropdown"
      role="listbox"
      data-testid={testid ? `${testid}-dropdown` : undefined}
    >
      {#each suggestions as suggestion, i (suggestion)}
        <li
          class="row"
          role="option"
          aria-selected={i === highlight}
          data-highlight={i === highlight}
          data-testid={testid ? `${testid}-row-${i}` : undefined}
          onmousedown={(e) => {
            e.preventDefault();
            pickSuggestion(suggestion);
          }}
        >
          {suggestion}
        </li>
      {/each}
    </ul>
  {/if}
</div>

<style>
  .notes-autocomplete {
    position: relative;
    display: flex;
    flex-direction: column;
  }

  .notes-autocomplete textarea {
    background: var(--color-surface-raised);
    color: var(--color-text-strong);
    border: 1px solid var(--color-surface-divider);
    padding: var(--space-1) var(--space-2);
    font-family: var(--type-family-mono);
    font-size: var(--type-size-sm);
    width: 100%;
    box-sizing: border-box;
    resize: vertical;
  }

  .notes-autocomplete textarea:focus-visible {
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
    border-radius: var(--radius-sm);
    max-height: 280px;
    overflow-y: auto;
    box-shadow: 0 4px 12px rgba(0, 0, 0, 0.2);
  }

  .row {
    padding: var(--space-1) var(--space-2);
    cursor: pointer;
    color: var(--color-text-strong);
    font-size: var(--type-size-sm);
    font-family: var(--type-family-mono);
    border-bottom: 1px solid var(--color-surface-divider);
    white-space: pre-wrap;
    word-break: break-word;
  }

  .row:last-child {
    border-bottom: none;
  }

  .row[data-highlight="true"] {
    background: var(--color-surface-divider);
  }
</style>
