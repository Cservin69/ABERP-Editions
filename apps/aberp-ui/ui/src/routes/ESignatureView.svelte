<script lang="ts">
  // D-15 (S355) — the 21 CFR Part 11 e-signature ceremony. The FIRST UI over
  // the signature layer (the personnel.signature_applied kind was live, firing
  // site + UI absent).
  //
  // An operator picks a record (a kind discriminator + id) and applies an
  // electronic signature under their registered digital identity. The backend
  // signs the canonical (kind, id) bytes via the DigitalIdProvider and records
  // the §11.50 manifestation landmark; the signer id + algorithm + timestamp
  // come from the signature, never this form. Mock identity today — the
  // ceremony + audit trail are edition-agnostic.

  import {
    applySignature,
    type SignatureCeremonyRecord,
  } from "../lib/api";

  // Common signable record kinds (free-text on the wire; these are the
  // demoable defaults). "other" reveals a free-text kind input.
  const KINDS: { value: string; label: string }[] = [
    { value: "invoice", label: "Invoice" },
    { value: "work_order", label: "Work order" },
    { value: "inspection", label: "Inspection report" },
    { value: "quote", label: "Quote" },
    { value: "other", label: "Other…" },
  ];

  let kindChoice = $state("work_order");
  let customKind = $state("");
  let recordId = $state("");
  let submitting = $state(false);
  let error = $state<string | null>(null);
  let result = $state<SignatureCeremonyRecord | null>(null);

  const effectiveKind = $derived(
    kindChoice === "other" ? customKind.trim() : kindChoice,
  );

  function fmt(ms: number): string {
    return new Date(ms).toLocaleString();
  }

  async function sign() {
    error = null;
    result = null;
    if (effectiveKind.length === 0) {
      error = "A record kind is required.";
      return;
    }
    if (recordId.trim().length === 0) {
      error = "A record id is required.";
      return;
    }
    submitting = true;
    try {
      result = await applySignature(effectiveKind, recordId.trim());
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      submitting = false;
    }
  }

  function reset() {
    result = null;
    error = null;
    recordId = "";
  }
</script>

<section class="esig">
  <header>
    <h1>✒️ Electronic signature</h1>
    <p class="lede">
      21 CFR Part 11 §11.50. Applying a signature records a durable,
      hash-chained landmark that this operator signed the named record — under
      their registered digital identity, with the provider's algorithm.
    </p>
  </header>

  {#if result}
    <div class="signed" role="status">
      <h2>Signature applied</h2>
      <dl>
        <dt>Signer</dt>
        <dd>
          {result.operator_display_name}
          <span class="muted">({result.operator_user_id})</span>
        </dd>
        <dt>Record</dt>
        <dd><code>{result.signed_record_kind} / {result.signed_record_id}</code></dd>
        <dt>Algorithm</dt>
        <dd><code>{result.signature_algorithm}</code></dd>
        <dt>Signed at</dt>
        <dd>{fmt(result.signed_at_ms)}</dd>
      </dl>
      <button type="button" onclick={reset}>Sign another record</button>
    </div>
  {:else}
    <form onsubmit={(e) => { e.preventDefault(); void sign(); }}>
      {#if error}
        <p class="error" role="alert">{error}</p>
      {/if}

      <label>
        <span>Record kind</span>
        <select bind:value={kindChoice}>
          {#each KINDS as k (k.value)}
            <option value={k.value}>{k.label}</option>
          {/each}
        </select>
      </label>

      {#if kindChoice === "other"}
        <label>
          <span>Custom kind</span>
          <input type="text" bind:value={customKind} placeholder="e.g. capa" />
        </label>
      {/if}

      <label>
        <span>Record id</span>
        <input
          type="text"
          bind:value={recordId}
          placeholder="e.g. wo_123"
        />
      </label>

      <button type="submit" disabled={submitting}>
        {submitting ? "Signing…" : "Apply signature"}
      </button>
    </form>
  {/if}
</section>

<style>
  .esig {
    max-width: 42rem;
    margin: 0 auto;
    padding: 1.5rem 1rem 3rem;
  }
  header h1 {
    margin: 0 0 0.25rem;
    font-size: 1.4rem;
  }
  .lede {
    margin: 0 0 1.5rem;
    color: var(--text-muted, #555);
    line-height: 1.45;
  }
  form {
    display: flex;
    flex-direction: column;
    gap: 1rem;
  }
  label {
    display: flex;
    flex-direction: column;
    gap: 0.35rem;
    font-weight: 600;
  }
  select,
  input[type="text"] {
    font: inherit;
    padding: 0.5rem 0.6rem;
    border: 1px solid var(--border, #c9c9c9);
    border-radius: 6px;
    background: var(--surface, #fff);
    color: inherit;
  }
  button {
    align-self: flex-start;
    font: inherit;
    font-weight: 600;
    padding: 0.55rem 1.1rem;
    border: 0;
    border-radius: 6px;
    background: var(--accent, #1a5cff);
    color: #fff;
    cursor: pointer;
  }
  button:disabled {
    opacity: 0.6;
    cursor: default;
  }
  .error {
    margin: 0;
    padding: 0.55rem 0.75rem;
    border-radius: 6px;
    background: #fde8e8;
    color: #a30000;
    border: 1px solid #f3b4b4;
  }
  .signed {
    border: 1px solid var(--border, #d5d5d5);
    border-radius: 8px;
    padding: 1rem 1.25rem 1.25rem;
    background: var(--surface, #fff);
  }
  .signed h2 {
    margin: 0 0 0.75rem;
    font-size: 1.15rem;
  }
  .signed dl {
    display: grid;
    grid-template-columns: max-content 1fr;
    gap: 0.4rem 1rem;
    margin: 0 0 1rem;
  }
  .signed dt {
    font-weight: 600;
    color: var(--text-muted, #555);
  }
  .signed dd {
    margin: 0;
  }
  .muted {
    color: var(--text-muted, #777);
  }
  code {
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  }
</style>
