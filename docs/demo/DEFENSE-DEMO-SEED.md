# The Defense demo seed

**What it is.** One command that writes a coherent aerospace job into the
bundled `demo` tenant, so the Defense screens have something to show.

```bash
./run/run_defense_demo.sh
```

That builds the Defense edition (`--features production`), runs
`aberp demo-seed --tenant demo`, and launches the desktop shell against the
`demo` tenant. `--seed-only` stops after the seed. Re-running is free: the seed
short-circuits on a tenant that already has data.

To seed without the launcher (e.g. against an already-built binary):

```bash
cargo build --release --features production --bin aberp
./target/release/aberp demo-seed --tenant demo
```

---

## Why it exists

The bundled seed that shipped with S434 writes three partners and two products.
That is a **Portable** convenience — it makes a NAV-off international install
look used. It says nothing about the Defense line, so a fresh Defense tenant
booted with every workshop wall-TV counter reading zero and the pricing, PO,
AVL, QA, work-order, inventory and NCR screens empty.

This seed writes the **narrative** instead of a table dump. One part flows
CAD → quote → PO → make → inspect → ship-gate, and each screen in the demo is a
different window onto that same job. The ids line up on purpose: the heat lot on
the stock row is the heat lot stamped into the part UIDs, which is the heat lot
the traceability report resolves, whose grade names the quote that spawned the
work order.

---

## The story it writes

**Customer:** Meridian Aerostructures Kft. (customer type *Aerospace*).
**Parts:** `LG-BRKT-4412` landing-gear bracket (Ti-6Al-4V) and `HYD-MAN-2207`
hydraulic manifold (7075-T651).

| Act | What lands |
|---|---|
| Master data | 1 customer + 3 suppliers, 2 finished goods + 2 raw-stock items, BOMs tying them together, 3 machines |
| AVL | 3 vendors: one **Approved** in date, one **Conditional** whose re-screening window has **lapsed**, one **Suspended** |
| Procurement | 2 POs. The titanium one is issued and received clean → **Received**. The aluminium one's incoming inspection **fails** → **PartiallyReceived** + an auto-raised NCR |
| Material | 3 inventory balances; two carry a heat lot and a Mill Test Report **file that exists on disk** |
| Quoting | 2 quotes priced by the **real engine** off the tenant's own catalogues, with a **real rendered PDF**; 1 permanently-failed job |
| Inspection plans | 6 QC characteristics with AS9102 identity (balloon number, designator, type, method, sheet/zone, accountability) |
| Shop floor | 3 work orders: two Completed (all ops done, all QA passed), one In progress with a live **Pending** QA row |
| Marking | 18 part UIDs — one per unit of both completed batches — each carrying the heat lot in its data-matrix payload |
| QC | Dimensional measurements against the plans. Batch 1 conforms; batch 2's trunnion centre distance measures **out of band**, and the failing verdict raises the second NCR |
| Dispatch | 2 drafted dispatches + 2 invoice drafts. One is clean and ships; one is **ship-blocked** by the open NCR against its unit UIDs |

Nothing in the quoting act is a made-up number. The totals, the reasoning log,
the margin verdict and the lead time are whatever the shipped engine computes
for those parts against the tenant's own catalogues; the PDF is the same
renderer the pricing daemon uses.

---

## Guards

The seed writes into exactly one place and refuses everything else.

1. **Slug** — `demo-seed` accepts only the slug `demo`. An exact match, not a
   prefix: `demo-defense` is refused too. The DB path is *derived* from the slug
   through the edition-locked resolver, so no launcher string can point it at a
   real tenant, and the compile-time edition binding means it cannot reach
   `~/.aberp/` or `~/.aberp-portable/` at all.
2. **Emptiness** — a tenant that already has partners is left untouched; the
   command reports `already_seeded` and exits 0.
3. **NAV off** — the `demo` registry row is written by the existing
   `TenantRegistry::add_demo`, which is NAV-**off**. So even though this is a
   `--features production` Defense binary, the demo tenant cannot submit an
   invoice to real NAV — and boot skips the keychain + §169 seller gate, landing
   on `Ready` instead of the setup wizard.
4. **The launcher** refuses to run if `ABERP_TENANT` is set to anything else.

The seed lives in `apps/aberp/src/demo_seed.rs` and is reachable only through
`aberp demo-seed`. It writes no config file, no universe document, and nothing
a cut gate inducts over.

---

## What it deliberately leaves empty

* **No issued invoice.** Issuing one needs the whole pipeline (seller.toml
  identity, MNB rates, gap-free numbering, NAV XML render) and burns a real
  sequence number. The demo's Today tile therefore reads zero until the operator
  issues one on stage from a seeded draft — which is the authentic
  demonstration, and the same call the S434 seed made.
* **No STEP geometry.** The seed ships no CAD file; the `.step` path each quote
  points at is a clearly-labelled placeholder, and the FeatureGraph the pipeline
  consumed is recorded directly. Drop a real STEP file over that path to re-run
  the extractor.
* **No MES adapters.** Adapter health is runtime state from live endpoints, not
  something a seed can write. The wall-TV adapter tiles show whatever is
  actually configured.
* **No QC reports issued.** The ADR-0199 report layer is Defense-only and its
  drafting/issuing is an operator action; the seed lays down the inspection
  plans and the measurements a report would be built from.
