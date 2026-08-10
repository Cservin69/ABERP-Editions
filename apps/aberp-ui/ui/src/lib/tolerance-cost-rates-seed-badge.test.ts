// N2 regression pin, imported from the PR #38 adversarial review: a row an
// operator has actually tuned must stop reading as a seed default, even though
// the form re-submits SEED_NOTE verbatim when they only edit the numbers.
import { describe, it, expect } from "vitest";
import {
  formFromToleranceCostRate,
  composeToleranceCostRateInputs,
  toleranceCostRateStatus,
  isSeedDefault,
} from "./tolerance-cost-rates";
import type { ToleranceCostRate } from "./api";

const SEED =
  "SEED — default EU/DE machine-shop rates, NOT your shop's measured values. " +
  "Tune to your shop. / ALAPÉRTÉK — EU/DE gépipari átlag, hangolja a saját műhelyére.";

const seeded: ToleranceCostRate = {
  id: "qtcr_x",
  tolerance_class: "precision",
  finish_passes_add: 0.5,
  inproc_inspection_min: 1,
  cmm_min_per_critical_feature: 2,
  rework_scrap_pct: 0.05,
  feed_slowdown_factor: 1.25,
  grinding_escalation: false,
  notes: SEED,
  updated_at: "2026-08-10T00:00:00Z",
  updated_by_actor: "boot",
};

describe("seed badge after a real operator edit", () => {
  it("an operator who tunes the NUMBERS through the form keeps the seed note verbatim", () => {
    // The operator opens the row, changes the scrap figure to their measured
    // one, and saves. They never touch the notes box.
    const form = formFromToleranceCostRate(seeded);
    form.reworkScrapPct = "0.07";
    const wire = composeToleranceCostRateInputs(form);

    // This is exactly what the backend persists.
    const persisted: ToleranceCostRate = {
      ...seeded,
      rework_scrap_pct: wire.rework_scrap_pct,
      notes: wire.notes,
      updated_by_actor: "ervin",
    };

    expect(persisted.updated_by_actor).toBe("ervin");
    expect(isSeedDefault(persisted)).toBe(false);
    expect(toleranceCostRateStatus(persisted)).toBe("tuned");
  });
});
