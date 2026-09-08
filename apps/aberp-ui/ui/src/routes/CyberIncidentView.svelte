<script lang="ts">
  // D-09 (S362) — DFARS 252.204-7012 cyber-incident intake. The FIRST UI
  // over the incident layer (the EventKind + 72h deadline math were live;
  // firing site + operator surface were absent).
  //
  // The operator declares a detected incident; the backend re-validates
  // severity / detection source, computes the 72-hour DoD reporting deadline
  // (only when CDI or OCS is affected — the 252.204-7012(c) trigger), and
  // fires `incident.cyber_detected`. No PII / no controlled content at rest:
  // the scope is a summary, systems are identifiers, not their contents.

  import {
    recordCyberIncident,
    type CyberIncidentInput,
    type CyberIncidentRecord,
    type IncidentSeverity,
    type DetectionSource,
  } from "../lib/api";

  const SEVERITIES: { value: IncidentSeverity; label: string }[] = [
    { value: "informational", label: "Informational" },
    { value: "low", label: "Low" },
    { value: "medium", label: "Medium" },
    { value: "high", label: "High" },
    { value: "critical", label: "Critical" },
  ];
  const SOURCES: { value: DetectionSource; label: string }[] = [
    { value: "siem", label: "SIEM" },
    { value: "user_report", label: "User report" },
    { value: "vendor_notification", label: "Vendor notification" },
    { value: "audit", label: "Audit" },
    { value: "other", label: "Other" },
  ];

  let severity = $state<IncidentSeverity>("high");
  let detectionSource = $state<DetectionSource>("siem");
  let scopeDescription = $state("");
  let affectedSystemsRaw = $state("");
  let cdiAffected = $state(false);
  let cuiAffected = $state(false);
  let ocsAffected = $state(false);
  let exfiltrationSuspected = $state(false);
  let mitigationNotes = $state("");

  let submitting = $state(false);
  let error = $state<string | null>(null);
  let result = $state<CyberIncidentRecord | null>(null);

  // The DFARS clock starts iff CDI or OCS is affected — surface that
  // advisorily so the operator understands why a deadline will/won't appear.
  const deadlineWillArm = $derived(cdiAffected || ocsAffected);

  function fmt(ms: number): string {
    return new Date(ms).toLocaleString();
  }

  async function submit(event: SubmitEvent) {
    event.preventDefault();
    error = null;
    result = null;
    if (scopeDescription.trim().length === 0) {
      error = "Scope description is required.";
      return;
    }
    const affectedSystems = affectedSystemsRaw
      .split(",")
      .map((s) => s.trim())
      .filter((s) => s.length > 0);
    const input: CyberIncidentInput = {
      severity,
      scopeDescription: scopeDescription.trim(),
      detectionSource,
      cdiAffected,
      cuiAffected,
      ocsAffected,
      exfiltrationSuspected,
      affectedSystems,
      mitigationNotes: mitigationNotes.trim() || null,
    };
    submitting = true;
    try {
      result = await recordCyberIncident(input);
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      submitting = false;
    }
  }

  function reset() {
    result = null;
    error = null;
    scopeDescription = "";
    affectedSystemsRaw = "";
    cdiAffected = false;
    cuiAffected = false;
    ocsAffected = false;
    exfiltrationSuspected = false;
    mitigationNotes = "";
    severity = "high";
    detectionSource = "siem";
  }
</script>

<section class="cyber">
  <header>
    <h1>🛡️ Cyber-incident reporting</h1>
    <p class="lede">
      DFARS 252.204-7012. Declaring an incident records it to the audit ledger
      and, when Controlled Defense Information (CDI) or operationally critical
      support (OCS) is affected, starts the 72-hour DoD reporting clock.
    </p>
  </header>

  {#if result}
    <div class="recorded" role="status">
      <h2>Incident recorded</h2>
      <dl>
        <dt>Reference</dt>
        <dd><code>{result.incident_id}</code></dd>
        <dt>Severity</dt>
        <dd>{result.severity}</dd>
        <dt>Detected</dt>
        <dd>{fmt(result.detected_at_ms)}</dd>
        <dt>DoD 72-hour report due</dt>
        <dd>
          {#if result.dod_72h_report_due_at_ms !== null}
            <strong class="due">{fmt(result.dod_72h_report_due_at_ms)}</strong>
          {:else}
            <span class="muted">— (no CDI/OCS impact; no DFARS deadline)</span>
          {/if}
        </dd>
      </dl>
      <button type="button" onclick={reset}>Declare another</button>
    </div>
  {:else}
    <form onsubmit={submit}>
      {#if error}
        <p class="error" role="alert">{error}</p>
      {/if}

      <label>
        <span>Severity</span>
        <select bind:value={severity}>
          {#each SEVERITIES as s (s.value)}
            <option value={s.value}>{s.label}</option>
          {/each}
        </select>
      </label>

      <label>
        <span>Detection source</span>
        <select bind:value={detectionSource}>
          {#each SOURCES as s (s.value)}
            <option value={s.value}>{s.label}</option>
          {/each}
        </select>
      </label>

      <label>
        <span>Scope description <em>(summary only — no raw log dumps)</em></span>
        <textarea
          bind:value={scopeDescription}
          rows="3"
          placeholder="e.g. Anomalous outbound traffic from CAD workstation segment"
        ></textarea>
      </label>

      <label>
        <span>Affected systems <em>(identifiers, comma-separated)</em></span>
        <input
          type="text"
          bind:value={affectedSystemsRaw}
          placeholder="cad-ws-04, file-srv-02"
        />
      </label>

      <fieldset>
        <legend>Impact</legend>
        <label class="check">
          <input type="checkbox" bind:checked={cdiAffected} />
          <span>CDI affected <em>(Controlled Defense Information)</em></span>
        </label>
        <label class="check">
          <input type="checkbox" bind:checked={ocsAffected} />
          <span>OCS affected <em>(operationally critical support)</em></span>
        </label>
        <label class="check">
          <input type="checkbox" bind:checked={cuiAffected} />
          <span>CUI affected <em>(32 CFR Part 2002)</em></span>
        </label>
        <label class="check">
          <input type="checkbox" bind:checked={exfiltrationSuspected} />
          <span>Exfiltration suspected</span>
        </label>
      </fieldset>

      <p class="clock" class:armed={deadlineWillArm}>
        {#if deadlineWillArm}
          The 72-hour DoD reporting clock will start at the recorded detection
          time.
        {:else}
          No CDI/OCS impact selected — no DFARS reporting deadline will be set.
        {/if}
      </p>

      <label>
        <span>Mitigation notes <em>(optional)</em></span>
        <textarea
          bind:value={mitigationNotes}
          rows="2"
          placeholder="e.g. Segment isolated; credentials rotated."
        ></textarea>
      </label>

      <button type="submit" disabled={submitting}>
        {submitting ? "Recording…" : "Declare incident"}
      </button>
    </form>
  {/if}
</section>

<style>
  .cyber {
    max-width: 46rem;
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
  label em {
    font-weight: 400;
    color: var(--text-muted, #666);
    font-style: normal;
  }
  select,
  input[type="text"],
  textarea {
    font: inherit;
    padding: 0.5rem 0.6rem;
    border: 1px solid var(--border, #c9c9c9);
    border-radius: 6px;
    background: var(--surface, #fff);
    color: inherit;
  }
  textarea {
    resize: vertical;
  }
  fieldset {
    border: 1px solid var(--border, #d5d5d5);
    border-radius: 8px;
    padding: 0.75rem 1rem 1rem;
  }
  legend {
    padding: 0 0.4rem;
    font-weight: 600;
  }
  label.check {
    flex-direction: row;
    align-items: center;
    gap: 0.5rem;
    font-weight: 500;
    margin-top: 0.4rem;
  }
  label.check input {
    width: auto;
  }
  .clock {
    margin: 0;
    padding: 0.6rem 0.75rem;
    border-radius: 6px;
    background: var(--surface-muted, #f2f2f2);
    color: var(--text-muted, #555);
    font-size: 0.9rem;
  }
  .clock.armed {
    background: #fff4e5;
    color: #9a4a00;
    border: 1px solid #f0c48a;
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
  .recorded {
    border: 1px solid var(--border, #d5d5d5);
    border-radius: 8px;
    padding: 1rem 1.25rem 1.25rem;
    background: var(--surface, #fff);
  }
  .recorded h2 {
    margin: 0 0 0.75rem;
    font-size: 1.15rem;
  }
  .recorded dl {
    display: grid;
    grid-template-columns: max-content 1fr;
    gap: 0.4rem 1rem;
    margin: 0 0 1rem;
  }
  .recorded dt {
    font-weight: 600;
    color: var(--text-muted, #555);
  }
  .recorded dd {
    margin: 0;
  }
  .due {
    color: #9a4a00;
  }
  .muted {
    color: var(--text-muted, #777);
  }
  code {
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  }
</style>
