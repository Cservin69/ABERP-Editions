# ADR-0125 prototypes — measured, NOT production code

Nothing here is imported by the package. These are the exact scripts behind
ADR-0125 §2's table, kept so the numbers can be re-run rather than believed.

- `buried.py` — the inside-material predicate (projection + oriented normal).
- `patch_buried3.py` — **M5, the proposal**: the level-aware pairwise buried
  veto with the adjacency gate, installed by monkeypatching `_rim_winner`.
- `sweep_the_boss_family.py` — the 108-part sweep. Baseline 10 wrong; M5 0.
- `M1-reemergence-FALSIFIED.patch` — drop un-re-emerged marches. 10 -> 11.
- `M2-carrier-reconstruction-FALSIFIED.py` — reconstruct the consumed division
  with `GeomAPI_IntSS`. 10 -> 10; the circle encircles the axis and blocks
  every ray.

Run from `python/aberp-cad-extract` with an interpreter that has OCP + pytest.

## Round 1 additions (2026-09-16)

- `patch_buried3.py` — updated: the adjacency gate is now **mouth-scoped**
  (A1). Behaviourally identical to the whole-solid gate on the whole corpus.
- `adversarial_parts.py` — `filleted_boss()`, the shape the corpus lacks (A6).
- `sweep_filleted_boss.py` — run `base` / `m5`. 6/6 correct either way.
- `check_never_shallower.py` — the direction-of-error check (A2):
  10 deeper, 98 unchanged, 0 shallower.
