# ADR-0127 — Evidence drift: the gate must re-read the measurements

- **Status:** **Proposed** (2026-09-17) — defect **reproduced** (`c826d32`), not
  argued. Held for Ervin's review; three calls below are stated decisions he
  can veto at review (§D1c, §D4, §D5).
- **Related:** ADR-0199 residual 8 (this), ADR-0099 (one writer, one
  transaction), ADR-0126 (the sibling residual, and the same lesson), ADR-0122
  §D2 / ADR-0124 (name what is missing rather than implying it).

## 1. Context — the residual is filed as a documentation problem and is not one

ADR-0199 residual 8 says a measurement recorded after issuance does not re-open
the gate, and concludes the only remaining harm is that *"the DOCUMENT is stale,
and an operator has to notice"*. That conclusion rests on one sentence: *"the
failure a late measurement records now spawns an NCR the belt sees"*.

**The net is real and it is not atomic.** `record_manual_inspection`
(`apps/aberp/src/qc_inspection.rs`) uses three transactions:

```
1. tx  → record_inspection(...) ; tx.commit()      ← the failing measurement lands
2. create_ncr(...) on its own connection           ← second commit
3. tx2 → link_auto_ncr(...) ; tx2.commit()         ← third commit
```

A crash or any error between (1) and (2) leaves a **committed failing
measurement with no NCR**. The belt reads NCRs, so it sees nothing. The QC
report gate never re-reads `qc_inspections`. The shipment releases on a report
that still says `accept`.

Reproduced in `a_late_failure_with_no_ncr_must_not_release_the_shipment`: the
test does not simulate a crash, it constructs the row the crash leaves behind.

This is the ADR-0099 violation the AVL firing sites already taught: commit, then
do the consequence in a second transaction. The atomic template in this codebase
is `mark_shipped`'s caller-owned `Transaction`.

### A second, latent hole found while measuring

The auto-NCR decision is made **twice**, by two predicates that must agree:

- `severity_for(verdict)` (`qc_inspection.rs`) — the live operator path;
- `RecordedInspection::auto_ncr_recommended` (`aberp-qa`) — the crate's answer.

They agree today (Minor/Major/Critical → NCR; Pass/CalibrationStale → none) and
**nothing pins that they must**. A new `Verdict` variant defaults to whatever
each `match` happens to do, and the operator path is the live one.

## 2. Decision

### D1 — The gate RE-DERIVES today's verdict and blocks when it no longer permits shipment

The gate reads today's plans, today's marks and today's `qc_inspections`, runs
the **same pure functions the freeze ran** — `build_report_lines` → `summarise`
→ `compute_disposition` — and blocks when the result does not
`permits_shipment()`.

New reason: `QcReportBlockReason::EvidenceDrift`.

**Why re-derive rather than look for "late failures".** The obvious rule is
"block if a failing measurement postdates the report". It is wrong twice over:

- **It is backdatable.** `qc_inspections` has **no recorded-at column** — only
  `measured_at_utc`, which the caller supplies. A rule keyed on it is evaded by
  passing an earlier timestamp.
- **It is too crude.** A failure later corrected by a passing re-measurement
  would block forever, when the evidence today is fine.

Re-derivation is time-free and says exactly the right thing: *does the evidence
that exists now still support the verdict this report is releasing on?*

### D1b — This is not the re-derivation §D7 forbids

ADR-0199 §D7 forbids re-deriving the **document**, because the hash pin makes
the issued bytes the record. This re-derives a **gate decision** and renders
nothing. The frozen report, its lines and its bytes are untouched; what is
recomputed is only the answer to "may this ship", which was always a live
question.

### D1c — STATED DECISION: `open_ncr_against_reported_part = false` in the re-derivation

`compute_disposition` takes that flag and returns `AcceptWithNcr` when set. The
gate passes **false**, so the re-derivation answers on **measurement evidence
alone** and the NCR belt keeps sole ownership of the NCR question. The two gates
stay separable, and neither can mask the other. Consequence: the re-derived
verdict is only ever `Accept`, `Reject` or `Incomplete`.

### D2 — The auto-NCR rides the measurement's own transaction (defence in depth)

Fix D1 catches the bad state; D2 stops it existing. The auto-NCR is created and
linked **inside the same transaction** that records the inspection, per
ADR-0099. Either the failing measurement and its NCR both land, or neither does.

Both are built. D1 alone would leave a window that quietly depends on a detector
noticing afterwards; D2 alone would leave every database that has *already* been
through the window releasing bad parts.

### D3 — The two predicates get pinned across EVERY variant

A test enumerates every `Verdict` variant and asserts
`severity_for(v).is_some() == auto_ncr_recommended_for(v)`. Adding a variant
without deciding both sides fails the build rather than silently diverging.

### D4 — STATED DECISION: a later failing measurement blocks OVER a waiver

Today a waived NCR plus a stale `accept` report ships. Under D1 it does not: the
re-derived verdict is `Reject`, and the gate blocks until a fresh report is
issued.

**This is a deliberate behaviour change with an operational cost.** An operator
who has waived an NCR and expects to ship must now issue a new QC report first.
The argument for it: on a no-bad-part-ships gate, the accept must be
re-affirmed **with the new data visible**. A waiver granted against the evidence
that existed yesterday is not consent to ship against evidence recorded since.
The remedy is the one ADR-0199 already documents — supersede the report — and
this makes the system ask for it instead of hoping someone notices.

Recorded here as a decision, prominently, so it can be vetoed at review rather
than discovered in the field.

### D5 — STATED DECISION: scope is the whole work order

The re-derivation uses every inspection on the WO, not only those for
characteristics the report enumerated. A failing measurement against a
characteristic the report never covered is exactly the case least likely to be
noticed, and the least defensible to ship over.

### D6 — Ordering: the specific reasons keep priority

`EvidenceDrift` is evaluated **after** `UnitDrift` and `PlanDrift`, so a part
that drifted for a nameable reason still gets that name. `EvidenceDrift` is what
remains when the scope is intact and the evidence is not.

## 3. Adversarial surfaces — attack before Accepted

1. **False blocks are the whole risk.** The re-derivation must agree with the
   freeze on every part that has not changed. `unaccounted > 0` →
   `Incomplete` → block; is there a benign shape where today's lines are
   unaccounted but the frozen report was legitimately `accept`? Lot-level
   characteristics and optional plans are where to look.
2. **Does it double-report?** `PlanDrift` and `UnitDrift` already cover promoted
   plans and changed mark sets. If `EvidenceDrift` also fires there, D6's
   ordering must genuinely give the specific reason.
3. **`calibration_stale`.** `compute_disposition` returns `Incomplete` on any
   stale count. A measurement that was fresh at freeze can age. Does the
   re-derivation therefore block a part purely because time passed? **If so,
   that is a false block and D1 needs a carve-out.**
4. **Cost.** Re-deriving on every shipment reads all plans, marks and
   inspections for the WO. Bounded, but measure it.
5. **D2's atomicity.** `create_ncr` currently opens its own connection. Moving
   it inside the caller's transaction must not reintroduce the reentrancy
   deadlock ADR-0099 warns about (the lock domains are disjoint).
6. **D4's blast radius.** How many existing flows waive-then-ship? If that is a
   routine path, the cost is larger than stated.

## 4. Consequences

- A committed failing measurement can no longer release a shipment, whether or
  not the NCR mechanism fired.
- The bad state stops being created (D2), and the databases that already contain
  it are caught anyway (D1).
- One new block reason, one new test-only predicate pin, one behaviour change
  (D4) that needs Ervin's sign-off.

## 5. Alternatives considered

- **Nudge only**, as the residual proposes: a banner saying the report may be
  stale. Rejected — it puts a safety property behind an operator noticing, and
  the reproduction shows the shipment releasing with nobody told.
- **Block on any failing measurement postdating issuance.** Rejected: §D1,
  backdatable and too crude.
- **D2 alone.** Rejected: leaves already-damaged databases releasing.
- **D1 alone.** Rejected: leaves a window whose only defence is a detector.
