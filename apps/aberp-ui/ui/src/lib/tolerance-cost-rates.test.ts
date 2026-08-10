// T5 / ADR-0097 Part 2 — vitest pin for the tolerance cost-rate helpers.
// Guards the form composer (incl. the zero-contribution seed), the band-list
// label coverage, and the zero-contribution detector, so a backend/engine
// drift surfaces here rather than in a live screen.

import { describe, expect, it } from "vitest";

import {
  composeToleranceCostRateInputs,
  emptyToleranceCostRateForm,
  formFromToleranceCostRate,
  isZeroContribution,
  TOLERANCE_BANDS,
  isSeedDefault,
  toleranceCostRateStatus,
  toleranceCostRateStatusLabel,
} from "./tolerance-cost-rates";
import { TOLERANCE_RANGES, type ToleranceCostRate } from "./api";

describe("emptyToleranceCostRateForm / composeToleranceCostRateInputs", () => {
  it("composes the zero-contribution seed (no money moves until tuned)", () => {
    const body = composeToleranceCostRateInputs(emptyToleranceCostRateForm());
    expect(body).toEqual({
      tolerance_class: "standard",
      finish_passes_add: 0,
      inproc_inspection_min: 0,
      cmm_min_per_critical_feature: 0,
      rework_scrap_pct: 0,
      feed_slowdown_factor: 1.0,
      grinding_escalation: false,
      notes: null,
    });
  });

  it("yields NaN for an unparseable numeric (backend rejects with a field error)", () => {
    const form = { ...emptyToleranceCostRateForm(), inprocInspectionMin: "abc" };
    const body = composeToleranceCostRateInputs(form);
    expect(Number.isNaN(body.inproc_inspection_min)).toBe(true);
  });
});

describe("formFromToleranceCostRate", () => {
  it("round-trips a fetched row through the form back to the wire body", () => {
    const rate: ToleranceCostRate = {
      id: "qtcr_01",
      tolerance_class: "precision",
      finish_passes_add: 1,
      inproc_inspection_min: 2.5,
      cmm_min_per_critical_feature: 4,
      rework_scrap_pct: 0.03,
      feed_slowdown_factor: 1.25,
      grinding_escalation: false,
      notes: "tuned",
      updated_at: "2026-06-29T00:00:00Z",
      updated_by_actor: "op",
    };
    const body = composeToleranceCostRateInputs(formFromToleranceCostRate(rate));
    expect(body).toEqual({
      tolerance_class: "precision",
      finish_passes_add: 1,
      inproc_inspection_min: 2.5,
      cmm_min_per_critical_feature: 4,
      rework_scrap_pct: 0.03,
      feed_slowdown_factor: 1.25,
      grinding_escalation: false,
      notes: "tuned",
    });
  });
});

describe("TOLERANCE_BANDS", () => {
  it("covers every ToleranceRange band with a friendly label", () => {
    expect(TOLERANCE_BANDS.map((b) => b.value)).toEqual([...TOLERANCE_RANGES]);
    for (const b of TOLERANCE_BANDS) {
      expect(b.label.trim().length).toBeGreaterThan(0);
      expect(b.label).not.toBe(b.value);
    }
  });
});

describe("isZeroContribution", () => {
  const base: ToleranceCostRate = {
    id: "qtcr_x",
    tolerance_class: "tight",
    finish_passes_add: 0,
    inproc_inspection_min: 0,
    cmm_min_per_critical_feature: 0,
    rework_scrap_pct: 0,
    feed_slowdown_factor: 1,
    grinding_escalation: false,
    notes: null,
    updated_at: "",
    updated_by_actor: "",
  };
  it("true for the all-zero seed, false once any driver is tuned", () => {
    expect(isZeroContribution(base)).toBe(true);
    expect(isZeroContribution({ ...base, cmm_min_per_critical_feature: 5 })).toBe(
      false,
    );
    expect(isZeroContribution({ ...base, feed_slowdown_factor: 1.2 })).toBe(false);
    expect(isZeroContribution({ ...base, grinding_escalation: true })).toBe(false);
  });
});

describe("seed-default badging", () => {
  const SEED =
    "SEED — default EU/DE machine-shop rates, NOT your shop's measured values. " +
    "Tune to your shop. / ALAPÉRTÉK — EU/DE gépipari átlag, hangolja a saját műhelyére.";
  const base: ToleranceCostRate = {
    id: "qtcr_y",
    tolerance_class: "precision",
    finish_passes_add: 0.5,
    inproc_inspection_min: 1,
    cmm_min_per_critical_feature: 2,
    rework_scrap_pct: 0.05,
    feed_slowdown_factor: 1.25,
    grinding_escalation: false,
    notes: SEED,
    updated_at: "",
    updated_by_actor: "boot",
  };

  it("keys on the write actor, NOT on the notes stamp (N2)", () => {
    expect(isSeedDefault(base)).toBe(true);
    // The notes stamp is NOT the signal: the edit form pre-fills it, so a row
    // an operator has written carries SEED_NOTE verbatim and must still read
    // as theirs.
    expect(isSeedDefault({ ...base, updated_by_actor: "ervin" })).toBe(false);
    expect(
      isSeedDefault({ ...base, notes: null, updated_by_actor: "ervin" }),
    ).toBe(false);
    // ...and clearing the note on a row nobody has written does not fake
    // ownership.
    expect(isSeedDefault({ ...base, notes: null })).toBe(true);
    expect(isSeedDefault({ ...base, notes: `${SEED} (checked 2026-09)` })).toBe(
      true,
    );
  });

  it("a seeded non-zero band is NOT reported as tuned", () => {
    expect(toleranceCostRateStatus(base)).toBe("seed");
    expect(toleranceCostRateStatusLabel(base)).toBe(
      "seed default — tune to your shop",
    );
  });

  it("an operator-owned row reads as tuned, and a zero row as dormant", () => {
    expect(
      toleranceCostRateStatus({
        ...base,
        notes: "measured on cell 3",
        updated_by_actor: "ervin",
      }),
    ).toBe("tuned");
    expect(
      toleranceCostRateStatus({
        ...base,
        finish_passes_add: 0,
        inproc_inspection_min: 0,
        cmm_min_per_critical_feature: 0,
        rework_scrap_pct: 0,
        feed_slowdown_factor: 1,
      }),
    ).toBe("dormant");
  });
});
