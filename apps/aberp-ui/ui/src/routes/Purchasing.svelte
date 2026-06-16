<script lang="ts">
  // S440 (ADR-0068) — Purchasing module: purchase orders.
  //   1. Open #/purchase-orders. Page lists POs (filterable by state, vendor,
  //      date range).
  //   2. "+ New PO" → inline create form: vendor typeahead (AVL-gated on the
  //      backend), lines (product typeahead / free description, quantity, unit
  //      price, heat-lot-required), live totals. A Suspended/Revoked vendor is
  //      refused by the backend with a red banner.
  //   3. Click a row → detail panel: lines + receipt history + actions (Issue,
  //      Record receipt, Cancel, Close). The state machine + AVL re-check live
  //      in the backend ([[trust-code-not-operator]]); a 409 surfaces here.
  //   4. "Record receipt" → modal: vendor delivery note + per-line received
  //      quantities + per-line inspection pass/fail + per-line heat-lot when the
  //      line requires it. A failed inspection auto-creates an NCR (S439).

  import { onMount } from "svelte";
  import PartnerTypeahead from "../lib/PartnerTypeahead.svelte";
  import {
    listPurchaseOrders,
    createPurchaseOrder,
    getPurchaseOrder,
    transitionPurchaseOrder,
    receivePurchaseOrder,
    listProducts,
    type PurchaseOrder,
    type PoDetail,
    type PoState,
    type NewPoLineInput,
    type ReceiptLineInput,
    type Product,
  } from "../lib/api";
  import {
    PO_STATE_LABELS,
    allowedNextStates,
    poTotals,
    formatPoMoney,
    lineRemaining,
    avlChip,
    issueBlockedByPending,
    validateNewPo,
  } from "../lib/purchasing";

  const STATES: PoState[] = [
    "draft",
    "issued_to_vendor",
    "partially_received",
    "received",
    "closed",
    "cancelled",
  ];
  const ZERO_DECIMAL = new Set(["HUF", "JPY", "KRW"]);

  let rows: PurchaseOrder[] = $state([]);
  let loadState: "loading" | "loaded" | "error" = $state("loading");
  let loadError: string | null = $state(null);

  // Filters (server-side scan).
  let fState = $state("");
  let fVendor = $state("");
  let fFrom = $state("");
  let fTo = $state("");

  // Detail panel.
  let selected: PoDetail | null = $state(null);
  let detailError: string | null = $state(null);
  let approver = $state("");

  // Create form.
  let showCreate = $state(false);
  let cVendorId = $state("");
  let cVendorName = $state("");
  let cVendorSearch = $state("");
  let cCurrency = $state("HUF");
  let cVatRate = $state(27);
  let cExpected = $state("");
  let cNotes = $state("");
  type DraftLine = {
    product_id: string | null;
    description: string;
    quantity: number;
    unitPriceInput: string;
    expected_heat_lot_required: boolean;
  };
  let cLines: DraftLine[] = $state([blankLine()]);
  let createError: string | null = $state(null);
  let creating = $state(false);

  // Product search (lines).
  let productResults: Record<number, Product[]> = $state({});

  // Receive modal.
  let showReceive = $state(false);
  let rDeliveryNote = $state("");
  let rLines: Record<string, { qty: string; pass: boolean; notes: string; heat: string }> =
    $state({});
  let receiveError: string | null = $state(null);
  let receiving = $state(false);

  function blankLine(): DraftLine {
    return {
      product_id: null,
      description: "",
      quantity: 1,
      unitPriceInput: "",
      expected_heat_lot_required: false,
    };
  }

  function decimalsFor(cur: string): number {
    return ZERO_DECIMAL.has(cur.trim().toUpperCase()) ? 0 : 2;
  }

  /** Parse an operator-typed major-unit price into integer minor units for the
   * PO currency. Blank / invalid → 0 (the backend re-validates ≥ 0). */
  function priceToMinor(raw: string, cur: string): number {
    const t = raw.trim().replace(",", ".");
    if (!t) return 0;
    const n = Number(t);
    if (!Number.isFinite(n) || n < 0) return NaN;
    const mult = 10 ** decimalsFor(cur);
    return Math.round(n * mult);
  }

  function draftToInputLines(): NewPoLineInput[] {
    return cLines.map((l) => ({
      product_id: l.product_id,
      description: l.description,
      quantity: l.quantity,
      unit_price_minor: priceToMinor(l.unitPriceInput, cCurrency),
      expected_heat_lot_required: l.expected_heat_lot_required,
    }));
  }

  const liveTotals = $derived(
    poTotals(
      cLines.map((l) => ({
        quantity: Number(l.quantity) || 0,
        unit_price_minor: Number.isFinite(priceToMinor(l.unitPriceInput, cCurrency))
          ? priceToMinor(l.unitPriceInput, cCurrency)
          : 0,
      })),
      Number(cVatRate) || 0,
    ),
  );

  onMount(() => {
    void loadList();
  });

  async function loadList() {
    loadState = "loading";
    loadError = null;
    try {
      rows = await listPurchaseOrders({
        state: (fState as PoState) || undefined,
        vendorPartnerId: fVendor || undefined,
        from: fFrom || undefined,
        to: fTo || undefined,
      });
      loadState = "loaded";
    } catch (err: unknown) {
      loadState = "error";
      loadError = err instanceof Error ? err.message : String(err);
    }
  }

  function clearFilters() {
    fState = "";
    fVendor = "";
    fFrom = "";
    fTo = "";
    void loadList();
  }

  function onVendorSelect(p: { id: string; display_name: string }) {
    cVendorId = p.id;
    cVendorName = p.display_name;
    cVendorSearch = p.display_name;
  }

  async function searchProducts(i: number) {
    const needle = cLines[i].description.trim();
    if (needle.length < 2) {
      productResults = { ...productResults, [i]: [] };
      return;
    }
    try {
      const res = await listProducts(needle);
      productResults = { ...productResults, [i]: res.slice(0, 6) };
    } catch {
      productResults = { ...productResults, [i]: [] };
    }
  }

  function pickProduct(i: number, p: Product) {
    cLines[i].product_id = p.id;
    cLines[i].description = p.name;
    cLines[i].unitPriceInput = formatPriceInput(p.unit_price_minor, cCurrency);
    productResults = { ...productResults, [i]: [] };
  }

  function formatPriceInput(minor: number, cur: string): string {
    const d = decimalsFor(cur);
    if (d === 0) return String(minor);
    return (minor / 100).toFixed(2);
  }

  function addLine() {
    cLines = [...cLines, blankLine()];
  }
  function removeLine(i: number) {
    cLines = cLines.filter((_, idx) => idx !== i);
    if (cLines.length === 0) cLines = [blankLine()];
  }

  function resetCreate() {
    showCreate = false;
    cVendorId = "";
    cVendorName = "";
    cVendorSearch = "";
    cCurrency = "HUF";
    cVatRate = 27;
    cExpected = "";
    cNotes = "";
    cLines = [blankLine()];
    createError = null;
    productResults = {};
  }

  async function submitCreate() {
    createError = null;
    const input = {
      vendor_partner_id: cVendorId,
      currency: cCurrency.trim().toUpperCase(),
      vat_rate_pct: Number(cVatRate) || 0,
      lines: draftToInputLines(),
    };
    const problems = validateNewPo(input);
    if (problems.length > 0) {
      createError = problems.join(" ");
      return;
    }
    creating = true;
    try {
      const po = await createPurchaseOrder({
        ...input,
        expected_delivery_utc: cExpected || null,
        notes: cNotes,
      });
      resetCreate();
      await loadList();
      await openDetail(po.po_id);
    } catch (err: unknown) {
      createError = err instanceof Error ? err.message : String(err);
    } finally {
      creating = false;
    }
  }

  async function openDetail(poId: string) {
    detailError = null;
    approver = "";
    try {
      selected = await getPurchaseOrder(poId);
    } catch (err: unknown) {
      detailError = err instanceof Error ? err.message : String(err);
    }
  }
  function closeDetail() {
    selected = null;
    detailError = null;
  }

  async function doTransition(to: PoState) {
    if (!selected) return;
    detailError = null;
    if (to === "issued_to_vendor") {
      if (!approver.trim()) {
        detailError = "Adja meg a jóváhagyót. / Enter the approver to issue.";
        return;
      }
      if (issueBlockedByPending(selected.vendor_avl_status)) {
        detailError =
          "A beszállító AVL státusza Függőben; hagyja jóvá a beszállítót a kiadás előtt. / Vendor is Pending AVL approval; approve the vendor before issuing.";
        return;
      }
    }
    try {
      await transitionPurchaseOrder(
        selected.po_id,
        to,
        to === "issued_to_vendor" ? approver.trim() : undefined,
      );
      await openDetail(selected.po_id);
      await loadList();
    } catch (err: unknown) {
      detailError = err instanceof Error ? err.message : String(err);
    }
  }

  function openReceive() {
    if (!selected) return;
    receiveError = null;
    rDeliveryNote = "";
    const init: typeof rLines = {};
    for (const l of selected.lines) {
      init[l.pol_id] = {
        qty: String(lineRemaining(l)),
        pass: true,
        notes: "",
        heat: "",
      };
    }
    rLines = init;
    showReceive = true;
  }

  async function submitReceive() {
    if (!selected) return;
    receiveError = null;
    if (!rDeliveryNote.trim()) {
      receiveError = "A szállítólevél száma kötelező. / Delivery note number is required.";
      return;
    }
    const lines: ReceiptLineInput[] = [];
    for (const l of selected.lines) {
      const r = rLines[l.pol_id];
      const qty = Number(r.qty) || 0;
      if (qty <= 0) continue;
      if (l.expected_heat_lot_required && !r.heat.trim()) {
        receiveError = `„${l.description}”: ehhez az anyaghoz olvasztási/tételszám kell. / Line "${l.description}" requires a heat/lot.`;
        return;
      }
      lines.push({
        pol_id: l.pol_id,
        received_quantity: qty,
        inspection_pass: r.pass,
        inspection_notes: r.notes,
        heat_lot: r.heat.trim() || null,
      });
    }
    if (lines.length === 0) {
      receiveError = "Adjon meg legalább egy beérkezett tételt. / Record at least one received line.";
      return;
    }
    receiving = true;
    try {
      await receivePurchaseOrder(selected.po_id, {
        delivery_note_number: rDeliveryNote.trim(),
        lines,
      });
      showReceive = false;
      await openDetail(selected.po_id);
      await loadList();
    } catch (err: unknown) {
      receiveError = err instanceof Error ? err.message : String(err);
    } finally {
      receiving = false;
    }
  }

  function stateChipClass(s: PoState): string {
    if (s === "cancelled") return "chip chip--err";
    if (s === "received" || s === "closed") return "chip chip--ok";
    if (s === "partially_received") return "chip chip--warning";
    return "chip chip--neutral";
  }
</script>

<section class="page" aria-labelledby="page-title">
  <header class="page__head">
    <div class="page__head-row">
      <h2 id="page-title" class="page__title">Beszerzés — Megrendelések / Purchasing — POs</h2>
      <button type="button" class="page__primary" onclick={() => (showCreate = !showCreate)}>
        + Új PO / New PO
      </button>
    </div>
    <p class="page__lede">
      Megrendelés beszállítónak. A felfüggesztett/visszavont AVL-beszállítót a rendszer elutasítja. /
      Purchase orders to vendors. A Suspended/Revoked AVL vendor is refused at create.
    </p>
  </header>

  {#if showCreate}
    <form class="create" onsubmit={(e) => { e.preventDefault(); void submitCreate(); }}>
      <div class="create__row">
        <label class="field">
          <span class="field-label">Beszállító / Vendor</span>
          <PartnerTypeahead
            bind:value={cVendorSearch}
            onSelect={onVendorSelect}
            placeholder="Beszállító neve (3+ betű) / Vendor name (3+ chars)…"
            ariaLabel="Vendor"
          />
          {#if cVendorId}
            <span class="page__muted mono">{cVendorName} ({cVendorId.slice(0, 12)}…)</span>
          {/if}
        </label>
        <label class="field field--narrow">
          <span class="field-label">Pénznem / Currency</span>
          <input bind:value={cCurrency} maxlength="3" style="text-transform:uppercase" />
        </label>
        <label class="field field--narrow">
          <span class="field-label">ÁFA % / VAT %</span>
          <input type="number" bind:value={cVatRate} min="0" max="100" />
        </label>
        <label class="field field--narrow">
          <span class="field-label">Várható szállítás / Expected</span>
          <input type="date" bind:value={cExpected} />
        </label>
      </div>

      <div class="lines">
        <table class="lines-table">
          <thead>
            <tr>
              <th scope="col">Leírás / Description</th>
              <th scope="col">Menny. / Qty</th>
              <th scope="col">Egységár / Unit price</th>
              <th scope="col" title="Heat/lot required">Heat/lot</th>
              <th scope="col">Sor / Line</th>
              <th scope="col"></th>
            </tr>
          </thead>
          <tbody>
            {#each cLines as line, i (i)}
              <tr>
                <td class="lines-table__desc">
                  <input
                    bind:value={line.description}
                    placeholder="Cikk vagy szabad szöveg / product or free text"
                    oninput={() => { line.product_id = null; void searchProducts(i); }}
                  />
                  {#if productResults[i] && productResults[i].length > 0}
                    <ul class="product-dd">
                      {#each productResults[i] as p (p.id)}
                        <li>
                          <button type="button" onclick={() => pickProduct(i, p)}>
                            {p.name}
                            <span class="page__muted mono">
                              {formatPoMoney(p.unit_price_minor, p.currency)}
                            </span>
                          </button>
                        </li>
                      {/each}
                    </ul>
                  {/if}
                </td>
                <td><input type="number" min="1" bind:value={line.quantity} class="num" /></td>
                <td><input bind:value={line.unitPriceInput} placeholder="0" class="num" /></td>
                <td class="center">
                  <input type="checkbox" bind:checked={line.expected_heat_lot_required} aria-label="Heat/lot required" />
                </td>
                <td class="mono">
                  {formatPoMoney(
                    (Number(line.quantity) || 0) *
                      (Number.isFinite(priceToMinor(line.unitPriceInput, cCurrency))
                        ? priceToMinor(line.unitPriceInput, cCurrency)
                        : 0),
                    cCurrency,
                  )}
                </td>
                <td>
                  <button type="button" class="quiet-button danger" onclick={() => removeLine(i)}>×</button>
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
        <button type="button" class="quiet-button" onclick={addLine}>+ Sor / Add line</button>
      </div>

      <label class="field">
        <span class="field-label">Megjegyzés / Notes</span>
        <textarea bind:value={cNotes} rows="2"></textarea>
      </label>

      <div class="totals">
        <span>Nettó / Subtotal: <strong class="mono">{formatPoMoney(liveTotals.subtotalMinor, cCurrency)}</strong></span>
        <span>ÁFA / VAT: <strong class="mono">{formatPoMoney(liveTotals.vatMinor, cCurrency)}</strong></span>
        <span>Összesen / Total: <strong class="mono">{formatPoMoney(liveTotals.totalMinor, cCurrency)}</strong></span>
      </div>

      {#if createError}
        <p class="confirm__error" role="alert">{createError}</p>
      {/if}
      <div class="create__buttons">
        <button type="button" class="quiet-button" onclick={resetCreate}>Mégse / Cancel</button>
        <button type="submit" class="page__primary" disabled={creating}>
          {creating ? "Mentés… / Saving…" : "PO létrehozása / Create PO"}
        </button>
      </div>
    </form>
  {/if}

  <div class="page__toolbar">
    <label class="filter">
      <span class="filter-label">State</span>
      <select bind:value={fState} onchange={loadList}>
        <option value="">Mind / All</option>
        {#each STATES as s}<option value={s}>{PO_STATE_LABELS[s]}</option>{/each}
      </select>
    </label>
    <label class="filter">
      <span class="filter-label">Vendor id</span>
      <input bind:value={fVendor} onchange={loadList} placeholder="partner id" />
    </label>
    <label class="filter">
      <span class="filter-label">From</span>
      <input type="date" bind:value={fFrom} onchange={loadList} />
    </label>
    <label class="filter">
      <span class="filter-label">To</span>
      <input type="date" bind:value={fTo} onchange={loadList} />
    </label>
    <button type="button" class="quiet-button" onclick={clearFilters}>Clear</button>
  </div>

  {#if loadState === "loading"}
    <p class="page__muted">Loading…</p>
  {:else if loadState === "error"}
    <div class="page__error" role="alert">
      <strong>Could not load purchase orders.</strong>
      <p class="page__error-detail">{loadError}</p>
    </div>
  {:else if rows.length === 0}
    <div class="page__empty"><p>Nincs megrendelés. / No purchase orders yet.</p></div>
  {:else}
    <table class="pos-table">
      <thead>
        <tr>
          <th scope="col">PO</th>
          <th scope="col">Vendor</th>
          <th scope="col">State</th>
          <th scope="col">AVL</th>
          <th scope="col">Total</th>
          <th scope="col">Created</th>
        </tr>
      </thead>
      <tbody>
        {#each rows as p (p.po_id)}
          {@const chip = avlChip(p.vendor_avl_status)}
          <tr
            class={selected?.po_id === p.po_id ? "is-selected" : ""}
            onclick={() => void openDetail(p.po_id)}
          >
            <td class="mono">{p.po_number}</td>
            <td class="mono">{p.vendor_partner_id.slice(0, 14)}…</td>
            <td><span class={stateChipClass(p.state)}>{PO_STATE_LABELS[p.state]}</span></td>
            <td>
              {#if chip}<span class="chip chip--{chip.tone === 'yellow' ? 'warning' : chip.tone === 'green' ? 'ok' : chip.tone === 'red' ? 'err' : 'neutral'}">{chip.label}</span>{:else}<span class="page__muted">—</span>{/if}
            </td>
            <td class="mono">{formatPoMoney(p.total_minor, p.currency)}</td>
            <td class="mono">{p.created_at_utc.slice(0, 10)}</td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}
</section>

{#if selected}
  {@const po = selected}
  {@const chip = avlChip(po.vendor_avl_status)}
  <section class="detail" aria-label="PO detail">
    <div class="detail__head">
      <h3 class="detail__title">
        <span class="mono">{po.po_number}</span>
        <span class={stateChipClass(po.state)}>{PO_STATE_LABELS[po.state]}</span>
        {#if chip}<span class="chip chip--{chip.tone === 'yellow' ? 'warning' : chip.tone === 'green' ? 'ok' : chip.tone === 'red' ? 'err' : 'neutral'}">{chip.label}</span>{/if}
      </h3>
      <button type="button" class="quiet-button" onclick={closeDetail}>Bezárás / Close</button>
    </div>

    {#if detailError}<p class="confirm__error" role="alert">{detailError}</p>{/if}

    <dl class="detail__grid">
      <dt>Vendor</dt><dd class="mono">{po.vendor_partner_id}</dd>
      <dt>Currency</dt><dd>{po.currency}</dd>
      <dt>Subtotal</dt><dd class="mono">{formatPoMoney(po.subtotal_minor, po.currency)}</dd>
      <dt>VAT ({po.vat_rate_pct}%)</dt><dd class="mono">{formatPoMoney(po.vat_minor, po.currency)}</dd>
      <dt>Total</dt><dd class="mono">{formatPoMoney(po.total_minor, po.currency)}</dd>
      <dt>Requested by</dt><dd class="mono">{po.requested_by_operator}</dd>
      {#if po.issued_at_utc}
        <dt>Issued</dt><dd class="mono">{po.issued_at_utc} — {po.approved_by_operator}</dd>
      {/if}
      {#if po.expected_delivery_utc}
        <dt>Expected</dt><dd class="mono">{po.expected_delivery_utc}</dd>
      {/if}
      {#if po.notes}<dt>Notes</dt><dd>{po.notes}</dd>{/if}
    </dl>

    <h4 class="detail__subtitle">Tételek / Lines</h4>
    <table class="lines-table">
      <thead>
        <tr><th>Description</th><th>Qty</th><th>Unit</th><th>Line</th><th>Received</th><th>Heat/lot</th></tr>
      </thead>
      <tbody>
        {#each po.lines as l (l.pol_id)}
          <tr>
            <td>{l.description}</td>
            <td class="mono">{l.quantity}</td>
            <td class="mono">{formatPoMoney(l.unit_price_minor, l.currency)}</td>
            <td class="mono">{formatPoMoney(l.line_total_minor, l.currency)}</td>
            <td class="mono">{l.received_quantity}/{l.quantity}</td>
            <td class="center">{l.expected_heat_lot_required ? "⚑" : "—"}</td>
          </tr>
        {/each}
      </tbody>
    </table>

    {#if po.receipts.length > 0}
      <h4 class="detail__subtitle">Beérkezések / Receipts</h4>
      <table class="lines-table">
        <thead>
          <tr><th>When</th><th>Note</th><th>Qty</th><th>Inspection</th><th>Heat/lot</th><th>NCR</th></tr>
        </thead>
        <tbody>
          {#each po.receipts as r (r.por_id)}
            <tr>
              <td class="mono">{r.received_at_utc.slice(0, 16)}</td>
              <td class="mono">{r.delivery_note_number}</td>
              <td class="mono">{r.received_quantity}</td>
              <td>
                {#if r.inspection_pass}<span class="chip chip--ok">PASS</span>
                {:else}<span class="chip chip--err">FAIL</span>{/if}
              </td>
              <td class="mono">{r.heat_lot_assigned ?? "—"}</td>
              <td class="mono">{r.ncr_id ? r.ncr_id.slice(0, 12) + "…" : "—"}</td>
            </tr>
          {/each}
        </tbody>
      </table>
    {/if}

    <div class="detail__actions">
      {#if po.state === "issued_to_vendor" || po.state === "partially_received"}
        <button type="button" class="page__primary" onclick={openReceive}>Beérkezés rögzítése / Record receipt</button>
      {/if}
      {#each allowedNextStates(po.state) as next}
        {#if next === "issued_to_vendor"}
          <span class="issue-group">
            <input bind:value={approver} placeholder="Jóváhagyó / Approver" aria-label="Approver" />
            <button type="button" class="page__primary" onclick={() => void doTransition("issued_to_vendor")}>Kiadás / Issue</button>
          </span>
        {:else if next === "cancelled"}
          <button type="button" class="quiet-button danger" onclick={() => void doTransition("cancelled")}>Visszavonás / Cancel</button>
        {:else if next === "closed"}
          <button type="button" class="page__primary" onclick={() => void doTransition("closed")}>Lezárás / Close</button>
        {/if}
      {/each}
    </div>
  </section>
{/if}

{#if showReceive && selected}
  {@const po = selected}
  <div class="modal-backdrop" role="dialog" aria-modal="true" aria-label="Record receipt">
    <div class="modal">
      <h3 class="modal__title">Beérkezés rögzítése — {po.po_number} / Record receipt</h3>
      <label class="field">
        <span class="field-label">Szállítólevél száma / Delivery note number</span>
        <input bind:value={rDeliveryNote} placeholder="DN-…" />
      </label>
      <table class="lines-table">
        <thead>
          <tr><th>Line</th><th>Remaining</th><th>Receive</th><th>Inspection</th><th>Notes</th><th>Heat/lot</th></tr>
        </thead>
        <tbody>
          {#each po.lines as l (l.pol_id)}
            <tr>
              <td>{l.description}</td>
              <td class="mono">{lineRemaining(l)}</td>
              <td><input type="number" min="0" bind:value={rLines[l.pol_id].qty} class="num" /></td>
              <td>
                <label class="inline">
                  <input type="checkbox" bind:checked={rLines[l.pol_id].pass} />
                  {rLines[l.pol_id].pass ? "PASS" : "FAIL → NCR"}
                </label>
              </td>
              <td><input bind:value={rLines[l.pol_id].notes} placeholder="megjegyzés / notes" /></td>
              <td>
                <input
                  bind:value={rLines[l.pol_id].heat}
                  placeholder={l.expected_heat_lot_required ? "kötelező / required" : "opcionális"}
                  class:required={l.expected_heat_lot_required}
                />
              </td>
            </tr>
          {/each}
        </tbody>
      </table>
      {#if receiveError}<p class="confirm__error" role="alert">{receiveError}</p>{/if}
      <div class="create__buttons">
        <button type="button" class="quiet-button" onclick={() => (showReceive = false)}>Mégse / Cancel</button>
        <button type="button" class="page__primary" onclick={() => void submitReceive()} disabled={receiving}>
          {receiving ? "Mentés… / Saving…" : "Rögzítés / Record"}
        </button>
      </div>
    </div>
  </div>
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
    padding: var(--space-3);
    border-left: 3px solid var(--color-signal-negative);
    background: var(--color-surface-raised);
  }
  .page__error-detail {
    margin: var(--space-1) 0 0 0;
    font-size: var(--type-size-sm);
    color: var(--color-text-secondary);
  }
  .pos-table,
  .lines-table {
    width: 100%;
    border-collapse: collapse;
    font-size: var(--type-size-sm);
  }
  .pos-table th,
  .pos-table td,
  .lines-table th,
  .lines-table td {
    text-align: left;
    padding: var(--space-2) var(--space-3);
    border-bottom: 1px solid var(--color-surface-divider);
    color: var(--color-text-primary);
  }
  .pos-table th,
  .lines-table th {
    color: var(--color-text-secondary);
    font-weight: 500;
    background: var(--color-surface-raised);
  }
  .pos-table tbody tr {
    cursor: pointer;
  }
  .pos-table tbody tr:hover,
  .pos-table tbody tr.is-selected {
    background: var(--color-surface-raised);
  }
  td.mono,
  .mono {
    font-family: var(--font-mono, monospace);
    color: var(--color-text-strong);
  }
  .center {
    text-align: center;
  }
  .filter {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    font-size: var(--type-size-sm);
    color: var(--color-text-secondary);
  }
  .filter-label {
    font-size: var(--type-size-xs, 0.75rem);
    text-transform: uppercase;
    letter-spacing: 0.04em;
  }
  .filter select,
  .filter input,
  .num {
    background: var(--color-surface-raised);
    color: var(--color-text-strong);
    border: 1px solid var(--color-surface-divider);
    border-radius: 4px;
    padding: var(--space-1) var(--space-2);
  }
  .num {
    width: 6rem;
  }
  .quiet-button {
    background: var(--color-surface-raised);
    color: var(--color-text-secondary);
    border: 1px solid var(--color-surface-divider);
    border-radius: 4px;
    padding: var(--space-1) var(--space-3);
    cursor: pointer;
  }
  .quiet-button:hover {
    color: var(--color-text-strong);
  }
  .quiet-button.danger {
    color: var(--color-signal-negative);
    border-color: var(--color-signal-negative);
  }
  .chip {
    display: inline-block;
    padding: 0.1rem 0.5rem;
    border-radius: 999px;
    font-size: var(--type-size-xs, 0.75rem);
    border: 1px solid var(--color-surface-divider);
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
  .create {
    margin-bottom: var(--space-4);
    padding: var(--space-4);
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-raised);
    border-radius: 6px;
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }
  .create__row {
    display: flex;
    gap: var(--space-3);
    flex-wrap: wrap;
    align-items: flex-end;
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
    flex: 1 1 12rem;
  }
  .field--narrow {
    flex: 0 0 8rem;
  }
  .field-label {
    font-size: var(--type-size-xs, 0.75rem);
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--color-text-secondary);
  }
  .field input,
  .field textarea {
    background: var(--color-surface-base, var(--color-surface-raised));
    color: var(--color-text-strong);
    border: 1px solid var(--color-surface-divider);
    border-radius: 4px;
    padding: var(--space-2);
  }
  .lines {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    align-items: flex-start;
  }
  .lines-table__desc {
    position: relative;
    min-width: 16rem;
  }
  .lines-table__desc input {
    width: 100%;
    background: var(--color-surface-base, var(--color-surface-raised));
    color: var(--color-text-strong);
    border: 1px solid var(--color-surface-divider);
    border-radius: 4px;
    padding: var(--space-1) var(--space-2);
  }
  .product-dd {
    position: absolute;
    z-index: 10;
    left: 0;
    right: 0;
    margin: 2px 0 0 0;
    padding: 0;
    list-style: none;
    background: var(--color-surface-raised);
    border: 1px solid var(--color-surface-divider);
    border-radius: 4px;
    max-height: 12rem;
    overflow-y: auto;
  }
  .product-dd button {
    display: flex;
    justify-content: space-between;
    gap: var(--space-2);
    width: 100%;
    text-align: left;
    background: none;
    border: 0;
    color: var(--color-text-primary);
    padding: var(--space-1) var(--space-2);
    cursor: pointer;
  }
  .product-dd button:hover {
    background: var(--color-surface-base);
  }
  .totals {
    display: flex;
    gap: var(--space-4);
    flex-wrap: wrap;
    font-size: var(--type-size-sm);
    color: var(--color-text-secondary);
  }
  .detail {
    max-width: 1200px;
    margin: var(--space-4) auto 0 auto;
    padding: var(--space-4);
    border: 1px solid var(--color-surface-divider);
    background: var(--color-surface-raised);
    border-radius: 6px;
  }
  .detail__head {
    display: flex;
    justify-content: space-between;
    align-items: center;
    margin-bottom: var(--space-3);
  }
  .detail__title {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    margin: 0;
    font-size: var(--type-size-md, 1rem);
  }
  .detail__subtitle {
    margin: var(--space-4) 0 var(--space-2) 0;
    font-size: var(--type-size-sm);
    color: var(--color-text-secondary);
  }
  .detail__grid {
    display: grid;
    grid-template-columns: max-content 1fr;
    gap: var(--space-1) var(--space-3);
    margin: 0;
  }
  .detail__grid dt {
    color: var(--color-text-secondary);
    font-size: var(--type-size-sm);
  }
  .detail__grid dd {
    margin: 0;
    color: var(--color-text-primary);
  }
  .detail__actions {
    display: flex;
    gap: var(--space-2);
    flex-wrap: wrap;
    margin-top: var(--space-4);
    align-items: center;
  }
  .issue-group {
    display: inline-flex;
    gap: var(--space-2);
    align-items: center;
  }
  .issue-group input {
    background: var(--color-surface-base, var(--color-surface-raised));
    color: var(--color-text-strong);
    border: 1px solid var(--color-surface-divider);
    border-radius: 4px;
    padding: var(--space-1) var(--space-2);
  }
  .confirm__error {
    color: var(--color-signal-negative);
    font-size: var(--type-size-sm);
    margin: var(--space-2) 0 0 0;
  }
  .inline {
    display: inline-flex;
    gap: var(--space-1);
    align-items: center;
    font-size: var(--type-size-sm);
  }
  .required {
    border-color: var(--color-signal-warning) !important;
  }
  .modal-backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.5);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 100;
  }
  .modal {
    background: var(--color-surface-base, var(--color-surface-raised));
    border: 1px solid var(--color-surface-divider);
    border-radius: 8px;
    padding: var(--space-4);
    max-width: 900px;
    width: 90%;
    max-height: 90vh;
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }
  .modal__title {
    margin: 0;
    font-size: var(--type-size-md, 1rem);
    color: var(--color-text-strong);
  }
  .modal input {
    background: var(--color-surface-raised);
    color: var(--color-text-strong);
    border: 1px solid var(--color-surface-divider);
    border-radius: 4px;
    padding: var(--space-1) var(--space-2);
  }
</style>
