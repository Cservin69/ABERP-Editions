# The Defense / aerospace customer demo walkthrough

**Audience — two of them, and they want opposite things.**

- **A parts customer** — an aerospace or defense buyer, a quality manager, and
  probably a supplier-development engineer. They came to find out whether you
  can make their part *and* prove it afterwards. They will believe a screen and
  a refusal; they will not believe a slide.
- **The EU grant board** — evaluators reading a capability claim. They came to
  find out whether the thing is real, whether the ask is honest, and whether the
  applicant knows the difference between what is running and what is drawn. For
  them the `/shop` page is the artefact, and its *honesty mechanism* is the
  argument, not the renders.

The two audiences share about half the material. §2 gives a run order for each.

**Purpose:** one continuous story — *a STEP file arrives, a price falls out of
it, a part is made under gates that are code, and the paperwork is a by-product*
— told against running software, where every screen is running software and not
a slide.

**Ground truth.** Every route, screen, command, field name, refusal and number in
this document was read out of `ABERP-Editions` at `origin/main` **`77664e9`**, and
the marked items were captured from software actually booted at that commit:
`target/debug/aberp` (built here), the SPA build, the real STEP extractor in
`python/aberp-cad-extract/.venv`, and the `ABERP-site` `/shop` page served from
`npm run dev`. Where a capability has **no screen**, this document says so in the
act itself and again in §4. **A demo script that points at a screen that is not
there is worse than no script.** §6 is the verification transcript.

**The release this describes.** The current Defense cut is the branch
**`PROD_Defense_v0.6.4` = `5bd846e`** (2026-08-30, the ADR-0116 snapshot system).
`origin/main` is three commits ahead of it and all three are CI-only
(`cad3468`, `8441d19`, `77664e9` — sharding the cut-gate probe harness). So
**main and the current release are the same product**, and everything below is
true of both. Release refs on this repo are **branches, not tags** — there is no
`PROD_Defense` tag on origin.

---

## 0 · The two one-paragraph stories

**To the parts customer:**

> *"Send us the STEP file. The geometry is read by code — bounding box, volume,
> surface area, and every drilled hole with its diameter, depth, axis and entry
> point — and the price is built from that by a pure function that writes down
> its own reasoning line by line, so you can take the number apart. How tight you
> need it moves the price, and it moves it through a published table of inspection
> minutes and scrap rates, not through a margin somebody adjusted by feel. When
> the part is made, it cannot start without a heat lot, it cannot ship without a
> unit-level UID on every piece, it cannot ship with an open non-conformance
> against it, and it cannot ship without an issued QC report — those four are
> transactions in code, not lines in a quality manual. The AS9102 Rev C first
> article, the dimensional report and the Certificate of Conformance render from
> the same frozen record, and the bytes we hand you are hashed into an
> append-only chain, so the document you hold can be proven to be the document we
> issued."*

**To the grant board:**

> *"The software half is running in production on a Defense-edition pilot: the
> quoting engine, the ERP, the hash-chained ledger, the NAV e-invoicing, the
> compliance gates. The manufacturing half — the mill-turn, the robot, the
> spindle probe, the dot-peen marker — is not built, and the public page says so
> on every single line, in a four-value vocabulary that cannot be read as more
> than it is. There are no photographs, no part counts, no lead times, no dates
> and no customer logos on that page, because there is nothing truthful to put
> there. What we are asking to fund is the half that is missing, and you can tell
> exactly which half that is by reading our own page."*

Everything below is those two paragraphs, checkable.

---

## 1 · Demo prep

### 1.1 · Which surfaces you will need

There are **four separate surfaces**, and they boot four different ways. Decide
before the meeting which of them you are opening; two of them are slow to build
the first time.

| # | Surface | What it is | How it boots |
|---|---|---|---|
| A | **The ABERP desktop app** | The operator SPA — every ERP screen | `./run/run_defense.sh` (Tauri shell, `--features production`) |
| B | **`aberp` CLI + the localhost API** | Snapshots, restore, evidence, QC reports, the extractor | `cargo build`, then subcommands / `curl` against `aberp serve` |
| C | **The STEP extractor** | The CAD half, standalone | `python/aberp-cad-extract/.venv/bin/python -m aberp_cad_extract <file>` |
| D | **`ABERP-site` `/shop`** | The grant-board page (separate repo) | `npm run dev` in `~/Documents/Claude/Projects/ABERP-site` |

**The SPA has no browser.** `apps/aberp-ui/ui/src/lib/api.ts` opens with *"Tauri
command surface — the SPA's ONLY path to the backend"*: every call is
`invoke()`, and TLS termination, the bearer token and the certificate pin all
live in the Rust shell. You **cannot** demo any ERP screen by pointing a browser
at `aberp serve`. Surface A is the desktop app or it is nothing.

### 1.2 · The day before, once

```bash
# 1 — the slow build. Cold, this is minutes (libduckdb-sys ships ~350 bundled
#     C++ translation units). Do not discover this in the room.
cargo build --release --features production --bin aberp
cargo build --release --features production --bin aberp-ui

# 2 — the CAD Python environment (~63 MB OCCT wheel; idempotent, sub-second
#     once it exists).
./run/provision_pipeline_venv.sh

# 3 — the SPA bundle. run_defense.sh does this too, but doing it now means
#     an npm failure is yesterday's problem.
cd apps/aberp-ui/ui && npm run build && cd -
```

**Then the three traps, all of which will stop the launch cold:**

1. **`run_defense.sh` refuses a "Frankenstein build".** It exits non-zero if the
   working tree has uncommitted changes, or if `HEAD` does not match an
   `origin/PROD_Defense_v*` (or `origin/PROD_v*`) branch tip
   (`run/run_defense.sh:130-178`). **For a demo, check out the release branch**:
   `git fetch origin && git checkout -B demo origin/PROD_Defense_v0.6.4`. The
   documented escape hatch is `ABERP_SKIP_GIT_CHECK=1`, which prints a yellow
   `[bypass]` line — if you use it in the room, somebody will read that line.
2. **The keychain will prompt after any rebuild.** The Defense build reads the
   session token, the NAV credential blob and the SMTP password from the macOS
   keychain, and the ad-hoc-signed binary's ACL is invalidated by a rebuild. The
   test bypass (`ABERP_KEYCHAIN_TEST_BYPASS`) is **compiled out of every
   `--features production` build** — it cannot save you here. Build once, launch
   once, click through the prompts, then leave the binary alone.
3. **Defense requires a tenant and a `seller.toml`.** Default tenant is `defense`
   (override with `ABERP_TENANT=`), data root is `~/.aberp-defense/<tenant>/`,
   seller profile at `~/.aberp-defense/<tenant>/seller.toml`. The reserved `prod`
   tenant is refused. See `docs/CUTOVER_RUNBOOK.md`.

> **Do not run `./run/upgrade_defense.sh` for a demo.** It is the real-money
> upgrade path: it forces a mandatory snapshot, resets the checkout hard, and
> launches a build that files invoices to the live NAV endpoint.

### 1.3 · The safe rehearsal build

For rehearsing the *shape* of the demo without touching the production edition,
build the default (Portable/dev) profile and boot the API directly:

```bash
cargo build -p aberp --bin aberp
ABERP_KEYCHAIN_TEST_BYPASS=1 ./target/debug/aberp serve \
  --db /tmp/demo/aberp.duckdb --tenant demo --port 5399
# then: curl -sk -H "Authorization: Bearer <token>" https://127.0.0.1:5399/health
```

**Know what this rehearsal cannot show you**, because it is the compile-time
edition split doing its job:

- `GET /api/qc-reports` returns **404** — the QC-report routes are mounted only
  when `qc_reporting_allowed()` is true, which is true only for
  `Edition::Defense`, which is `--features production`
  (`apps/aberp/src/serve.rs:5414`, `build_profile.rs:295-315`). *(Measured.)*
- The boot log says, in these words: **`email-outbox poll daemon NOT spawned —
  storefront reach is a Defense-only capability`**, and the same for the
  pdf-rerender daemon. *(Measured.)*
- The snapshot store is `~/Documents/ABERP-snapshots-**portable**/<tenant>`, not
  `-defense`.

### 1.4 · What data and fixtures exist

**There is no seeded Defense demo tenant.** The bundled demo company that makes
Portable boot into a populated dashboard is a *Portable* convenience; a fresh
Defense tenant is empty. Booted cold, `GET /api/workshop/dashboard` returns every
counter at zero. **Budget an hour to hand-build a demo tenant**, or use the two
theatre modes below.

What *does* exist in the tree, ready to point at:

| Fixture | Where | What it is good for |
|---|---|---|
| **~60 STEP fixtures** | `python/aberp-cad-extract/aberp_cad_extract/tests/fixtures/` | The whole CAD act, live, in a terminal. `plate_4_through_holes.step`, `cross_drilled_shaft.step`, `stepped_bore.step`, `countersunk_bore_120.step`, `assembly_two_solids.step` are the five worth knowing. |
| **A real customer part** | `quote-artifacts/aeb2771d-…/pump_adapter_v16.step` (321 KB) | Looks great on screen — **but see the trap below.** |
| **Workshop demo mode** | `apps/aberp-ui/ui/src/lib/workshop-mock-data.ts` | A fully populated wall-TV with no data entry at all. See Act 4. |
| **The `/shop` renders** | `ABERP-site/static/shop/*.jpg` (5 images, 448 + 768 px) | The grant-board act. |

> **Trap — the stored quote artifact is encrypted.** `quote-artifacts/…/*.step`
> is **AES-256-GCM at rest** (ADR-0083; magic header `ABRPCAD1`,
> `apps/aberp/src/cad_blob.rs:70`). Feeding it straight to the extractor returns
> `{"error": {"stage": "input", "message": "STEP file could not be parsed (OCCT
> ReadFile status=3)"}}`. *(Measured — I hit this.)* That refusal is a *feature*
> worth mentioning out loud to a defense customer ("your geometry is encrypted on
> our disk and every read of it is audited"), but do not plan the act around
> extracting that file. **Use the test fixtures for the live extraction.**

### 1.5 · Booting the grant-board page

`ABERP-site` **refuses to boot without its environment**. Run `npm run dev` bare
and `/shop` renders a plain-text `service unavailable: storefront boot checks
failed` followed by five numbered diagnostics. *(Measured.)* The working
incantation:

```bash
cd ~/Documents/Claude/Projects/ABERP-site
BODY_SIZE_LIMIT=52428800 \
ABERP_SITE_OPERATOR_EMAIL=<an address> \
ABERP_SITE_EMAIL_OUTBOX_DIR=<a writable dir> \
ABERP_SITE_CATALOGUE_DIR=<a writable dir> \
ABERP_SITE_QUOTE_DIR=<a writable dir> \
npm run dev -- --port 5199
# → http://localhost:5199/shop
```

Verified rendering at `9875c11`: the nav (`HOME` · `SHOP`), the hero **"ONE FILE
IN. ONE CERTIFIED PART OUT."**, the cinematic cell render with its
`ILLUSTRATION ONLY` caption, the four-line status strip, and the marquee whose
last token is `THE MACHINE SHOP — NOT BUILT YET`.

---

## 2 · Two run orders

### 2.1 · The parts customer — about 35 minutes

| # | Act | Surface | Minutes |
|---|---|---|---|
| 1 | STEP in, geometry out | **C** terminal | 5 |
| 2 | The price you can take apart | **A** screen (`#/invoices` → Pricing tab) | 6 |
| 3 | Tolerance moves the price — and how | **A** screen (Maintenance → Quoting) | 4 |
| 4 | Buy from the right people, start from the right bar | **A** screens (Approved vendors, POs, Inventory balances) | 5 |
| 5 | The shop floor | **A** screen (Workshop TV, demo mode) | 3 |
| 6 | The four refusals | **A** screens (Work orders, NCRs, Dispatch) | 6 |
| 7 | The paperwork | **B** API — **say the words "no screen yet"** | 4 |
| 8 | Prove it later | **A** screen (Activity log) + **B** `aberp-verify` | 2 |

Acts 1 and 7 are the two that decide this meeting, and act 7 is the one with no
screen. Plan for that: have the `curl` and the rendered PDF **already on the
second monitor** before you start talking.

### 2.2 · The EU grant board — about 15 minutes

| # | Act | Surface | Minutes |
|---|---|---|---|
| 10 | The `/shop` page, read top to bottom | **D** browser | 7 |
| 1 | STEP in, geometry out — the one live proof | **C** terminal | 3 |
| 8 | The ledger, and `aberp-verify` | **A** + **B** | 3 |
| — | The `Designed → Live` backlog as the work plan | `docs/BACKLOG-designed-to-live.md` | 2 |

**Do not give the grant board the ERP tour.** Their question is not "does it
work", it is "is the claim honest and is the gap real". The `/shop` page's
four-value vocabulary and the backlog file answer exactly that question, and the
backlog is more persuasive than any screen because it is a list of things you
are saying you *cannot* do yet.

---

## 3 · The acts

### Act 1 · A STEP file goes in, and real geometry comes out

**Surface C — a terminal. This is live, and it is fast.**

```bash
./python/aberp-cad-extract/.venv/bin/python -m aberp_cad_extract \
  python/aberp-cad-extract/aberp_cad_extract/tests/fixtures/plate_4_through_holes.step \
  --material-grade 1.4301
```

**What the room sees** — this exact output, in under a second *(captured)*:

```json
{"_schema_version": 6, "bounding_box_mm": [100.0, 60.0, 12.0],
 "volume_mm3": 69587.25684204299, "surface_area_mm2": 16644.24771931907,
 "material_grade": "1.4301", "features": [], "requires_5_axis": false,
 "thin_wall_present": false,
 "located_holes": [
   {"diameter_mm": 8.0, "depth_mm": 12.0, "axis_unit": [0.0, 0.0, 1.0],
    "entry_point_mm": [20.0, 20.0, 0.0], "end_condition": "through", "flat_bottom": false},
   … three more at (20,40), (80,20), (80,40) …]}
```

Then the one that makes an engineer sit up — a Ø8 cross-drill through a Ø30 bar:

```
cross_drilled_shaft.step → 1 hole, Ø8.0, depth 22.360679774997898,
                           axis [1,0,0], entry (−11.1803, 10.0, 30.0), through
```

That depth is **2·√125**, exactly. The bore's axis leaves the material at
±11.1803 while the trim curve on the outside diameter reaches ±13.7477 — the
miner measures to the **bar's surface**, not to the exit curve's extreme, so it
does not invent 22.96 % of drilling that nobody does. That is a real geometric
correctness result, and it is pinned by
`test_n1_cross_drilled_shaft_measures_to_the_round_od`.

And the refusal, which is worth showing on purpose:

```
assembly_two_solids.step →
  {"error": {"stage": "input", "message":
   "STEP file contains an assembly with 2 solids; only single-part STEP is supported in v1"}}
```

**Why it matters.** Nobody in this market quotes off a mesh any more, and STL was
deliberately retired: ADR-0112 Part A made this pipeline **STEP-only**, because a
triangle soup has no topology and you cannot mine a hole out of it. `located_holes`
is B-rep mining — the real axis, the real depth, the real entry point, and a
through/blind/flat-bottom end condition. Those are **postable coordinates**, which
is the difference between a quoting toy and the front half of a CAM pipeline.

**The honest half — say this before they ask.** Two things in that output:

- **`features: []` is always empty.** The STEP extractor never constructs a
  `Feature` (`extractors/step.py:263` passes `features or []` and nothing ever
  supplies one), so the engine's feature-driven machining time is structurally
  0.0 (`engine.rs:761-769`). **Machining time today is driven by removed volume,
  surface area and the complexity rules — not by a recognised feature list.**
- **`located_holes` is computed and *unconsumed*.** No quote path and no toolpath
  reads it. That is deliberate: [D-19](../BACKLOG-designed-to-live.md#d-19) is a
  **hard gate** holding drilling cycle-time pricing (ADR-0112 slice C) until five
  named geometry defects are closed — an undercut spherical cavity that reads up
  to 87 % short, a Ø16 bore that silently returns *zero* holes, a boss-overhang
  band, a 118° drill-point apex admitted as a cap, and a breakout hole that reads
  blind. Four of the five **under-quote**. The gate exists so a future pricing
  slice cannot consume the field before they are fixed.

That last paragraph is the strongest thing you will say all day to an aerospace
buyer. You have a computed feature, you know exactly how it is wrong, you have
written the wrongness down, and you have wired a gate so nobody can price off it
by accident.

---

### Act 2 · A price you can take apart

**Surface A — the desktop app. `#/invoices`, the fourth tab, "Pricing".**

The Invoices screen carries four tabs — Outgoing, Incoming, **Quotes** (the
storefront intake queue) and **Pricing** (the auto-quoting pipeline). Pricing is
where a CAD job lives: `PricingJobsList` → `PricingJobDetail`.

**What the audience sees.** A job row per quote, its pricing state, and on the
detail: the material, the stock form, the gear operations, the resolved tolerance,
the margin and lead-time overrides, a **"Render PDF"** action
(`/api/quote-pricing-jobs/:quote_id/pdf`, rendered by the `aberp-quote-pdf`
crate) and a **per-job audit view** (`…/audit`).

The thing to actually put on the projector is the **reasoning log**. The engine is
a pure function that narrates itself, line by line, in the shape of:

```
[feature 0] …/…/count=… (size=…mm) → rule#… base=… * count=… * mult=… = … min
[tolerance] governing band=precision (resolved target=standard, 3 critical-feature callout(s)) → rate row matched
[tolerance] inspection = (1.0000 in-proc + 2.0000 CMM) min/feat * 3 feat = 9.0000 min * rate=… EUR/min = … EUR
[tolerance] finishing  = finish_passes_add=0.5000 * base_finish_min=… * feed_slowdown=1.2500 = … min → … EUR
[tolerance] scrap/rework = rework_scrap_pct=0.0500 * (material=… + machining=…) = … EUR
[gate] margin/total = 0.3142 >= min_margin floor 0.2000 — OK
```

**Why it matters.** Every aerospace buyer has been handed a one-number quote and
been told the breakdown is commercially sensitive. This one hands over the
arithmetic. And the last line is the good one: if the computed margin falls under
the floor, the engine returns `QuoteError::MarginFloorViolation` and there is **no
quote** — it does not silently ship a below-floor number
(`engine.rs:1218-1231`). A supplier that refuses its own bad quotes is a supplier
that will still be there in year three.

---

### Act 3 · Tolerance moves the price, through a published table

**Surface A — Maintenance area → Quoting → "Tolerance cost rates".**

Five rows, one per band, every field editable, every seeded row visibly stamped as
a seed. The numbers as shipped (`quoting_tolerance_cost_rates.rs`, `SEEDS`):

| Band | extra finish passes | in-proc gauging min/feat | CMM min/feat | scrap % | feed factor | grinding |
|---|---|---|---|---|---|---|
| `loose` | 0 | 0 | 0 | 0 % | 1.00 | no |
| `standard` | 0 | 0 | 0 | 0 % | 1.00 | no |
| `tight` | 0 | 0.5 | 1.0 | 2 % | 1.00 | no |
| `precision` | 0.5 | 1.0 | 2.0 | 5 % | 1.25 | no |
| `ultra_precision` | 0.5 | 2.0 | 4.0 | 12 % | 1.50 | **yes** |

**Why it matters.** These are researched EU/DE machine-shop defaults with their
provenance written into the source (a published aerospace-aluminium case where
relaxing twenty ±0.01 mm dimensions cut scrap from 12 % to 2 %; a metrology guide
putting a simple part's CMM run at 15–30 min; the IT-grade cost ladder used as a
*ceiling check* — measured, the seed moves a real quote at most 1.19× at `tight`,
1.63× at `precision`, 3.13× at `ultra`, all comfortably inside the published
multipliers). Every value sits at the **low end** of its range on purpose: a seed
that under-states is corrected by the operator's first review; one that
over-states silently loses work.

And `loose`/`standard` are **exactly zero**, so a part with no tolerance signal
prices byte-identically to before the feature existed. That is the sentence that
stops a buyer worrying you bolted a surcharge onto everything.

**⚠️ The honest half — do not say "every quote carries an inspection cost."**
It does not, and your own public page already says so. The inspection term is
`(inproc_inspection_min + cmm_min_per_critical_feature) × n_critical_features`
(`engine.rs:1598-1602`). With **no critical-feature callouts, that term is zero**,
and at `loose`/`standard` the whole tolerance block contributes zero. The
storefront form asks for a tolerance scheme, a *"critical features?"* checkbox and
a free-text note — which routes to **operator review**, not to an automatic
per-feature callout. The `/shop` page's own chain step 01 is marked
**`DESIGNED`**, with the text: *"Specified. The quoting engine that would carry
the line is live; the line is not in it."* Say that version. It is better than the
overclaim and it is already public.

**The right sentence:** *"Tightening the tolerance moves the price through
inspection minutes, scrap rate, feed rate and — at the tightest band — a
grinding escalation. What is not automatic yet is a per-quote inspection line on
every part regardless of tolerance; that is designed and it is not in the engine."*

---

### Act 4 · The shop floor, on a wall

**Surface A — `#/workshop`, the Műhely / Workshop wall-TV.**

One endpoint (`GET /api/workshop/dashboard`) returns the whole bundle and the SPA
re-polls on a ~10 s cadence: work-order state buckets, low-stock count, the QA
queue, the dispatch panel, today's invoice headline, a live recent-activity rail
off the audit ledger, and an **MES adapter status tile** with per-adapter health
dots, a red pulse on a degraded adapter and an audible alert chime driven off
recorded health transitions.

**Demo mode — this is the single highest-leverage thing in this document.** Five
taps on the page's `<h2>` (`#ws-page-title`) within 2 seconds flips a
`localStorage` flag and the dashboard short-circuits to mock data
(`workshop-demo-mode.ts`, threshold 5, window 2000 ms). It survives a reload, it
never exposes real numbers, and it adds three kinetic effects that make the wall
look like a shop that is running: the activity stream auto-scrolls, a spotlight
rotates across the seven tiles, and a scan-message ticker cycles on the
barcode-scanner row (`WO-2026-00428 — Manifold T4`, `PART-MFLD-T4 ×12`,
`WO-2026-00431 — Tartó 240mm`, …). Real mode stays deliberately animation-free.

The mock shop: 18 work orders across six states, 3 low-stock products, 6 pending
QA, 5 shipped + 2 drafted dispatches, 6 HUF + 2 EUR invoices today, and four
adapters. Note that demo mode **forces every adapter to render healthy** — one
adapter in the raw mock is `unhealthy` specifically so the suppression is
exercised, and so a tour wall never flares red mid-sentence.

**The five real adapter families** (`crates/aberp-mes/src/adapter_config.rs`),
all wired to real transports:

| Kind | Transport |
|---|---|
| `barcode-scanner` | TCP listener, line-delimited UTF-8, bounded payload + connections |
| `label-printer` | Raw TCP ZPL to a Zebra, TCP-connect health probe |
| `cnc-machine` | **MTConnect** — HTTP `GET /{device}/current` polling |
| `robot` | **Universal Robots RTDE** over TCP, version handshake, backoff reconnect |
| `laser-cutter` | Trumpf, through the `TrumpfSource` seam; v1 backend is MTConnect |

**Why it matters — the vertical-integration argument.** One open standard already
serves two machine families here: the CNC adapter and the Trumpf laser share the
same polling and parsing code. That was chosen over N vendor SDKs deliberately —
you consume DMG MORI's IoTconnector *MTConnect output*, not its proprietary API,
so a vendor swap is mechanical rather than a rewrite. `MockExportControlProvider`,
`MockDapTransport`, `MockTimestampAuthority`, `MockProbeSource`, `TrumpfSource`,
`ProbeIngestionSource`, `DigitalIdProvider` — the whole outside world sits behind
`Send + Sync` traits chosen at the boot boundary. That is the answer to "what
happens when we change machines".

**The honest half.** Three things:

- **"DMG MORI NTX" and "FANUC" appear only in ADRs, never in shipped code.** The
  CNC adapter is a generic MTConnect poller. Say *"MTConnect, which is what the
  NTX's IoTconnector speaks"* — not *"we have a DMG MORI integration"*.
- **MTConnect is a base transport with room in it**
  ([D-16](../BACKLOG-designed-to-live.md#d-16)): the parser extracts six data
  items per poll but only `Execution` drives an event, and the adapter reads
  `/current` snapshots without touching `/sample`, `/probe` or `/assets`.
- **Nothing has ever been plugged in.** No machine, no robot, no scanner on a real
  floor. The adapters work; there is no cell.

---

### Act 5 · The paperwork: AS9102, the CoC, and the dimensional report

**Surface B — the API. ⚠️ THERE IS NO SCREEN. Say this out loud.**

This is the capability an aerospace buyer came for, and it is the one with no
operator surface at this commit. `qc-report` appears in **zero** files under
`apps/aberp-ui/ui/src/` *(verified: `grep -rIn` over the whole SPA source returns
no matches)*. ADR-0199's Phase-1 plan listed "Routes + SPA surface"; the routes
landed and the SPA surface did not, and the ADR's own "deltas from the design
pass" section does not claim otherwise.

**What exists, and it is a lot.** Eight routes, mounted **only** in a Defense
build (`serve.rs:5414-5433`, behind `qc_reporting_allowed()`):

```
POST   /api/qc-reports                     # draft: resolve traceability, freeze the record
GET    /api/qc-reports/:id
POST   /api/qc-reports/:id/issue           # render, SHA-256 the bytes, pin the hash into the chain
GET    /api/qc-reports/:id/pdf             # re-render; header x-aberp-qc-sha-matches-issued
POST   /api/qc-reports/:id/void
GET    /api/dispatches/:id/qc-reports
GET/PUT /api/partners/:id/qc-report-template
```

**Three document shapes, one frozen record** (`QcReportKind`,
`crates/aberp-qa/src/qc/vocab.rs:29-45`):

- **`dimensional_inspection`** — the per-shipment document. One row **per
  characteristic per serialised unit**: balloon number, characteristic name and
  type, nominal, tolerance −/+, **actual measured**, deviation, units, method,
  verdict. Plus the traceability block, the overall disposition and a signature
  block.
- **`coc`** — the Certificate of Conformance. One page, no characteristic table:
  the conformance statement, part number + drawing revision, quantity, serial
  range, work order, heat/lot + mill-cert reference, applicable specs, the QC
  report number it certifies against, disposition, signature block.
- **`as9102_fair`** — the **AS9102 Rev C** First Article Inspection Report, Forms
  1 / 2 / 3. Form 3's characteristic-accountability model is a strict superset of
  the per-shipment report, which is why the per-shipment report is a *projection*
  of it and not a second data model. Rev C was named explicitly, and Forms 1/2/3
  shipped in Phase 1 rather than being deferred.

Layout is resolved per customer through a closed-vocab `QcReportTemplate` —
`partners.qc_report_template` → `AbenStandard` — so a prime's own form is a
rendering change, never a schema change.

**The one detail that earns real credibility with a quality manager.** The issued
SHA-256 is **not printed on the page it hashes**, and neither is the dispatch id.
Because the gate requires the report to be ISSUED *before* the shipment may
proceed, `qc_reports.dsp_id` is written by `mark_shipped` strictly *after* the
hash is taken. A first cut printed the dispatch in the identity block — which
would have made **every correctly shipped report report itself as tampered** on
its first download, turning the tamper signal into noise on exactly the documents
that matter. Four post-issuance fields are normalised by one function,
`canonical_for_render`, that both issuance and re-render go through so they
provably agree. The hash lives in the chain, on the API response, and in the
`x-aberp-qc-sha-matches-issued` response header — which reports `draft`, not
`false`, when there is no pin to compare against.

**What to actually do in the room.** Have a rendered PDF of each of the three
shapes open on a second monitor **before the meeting**, and drive one
`POST …/issue` + one `GET …/pdf` from a terminal so they watch the hash pin
happen. Then say: *"The operator screen for this is the next thing we build; the
record, the arithmetic, the three documents and the shipment gate are done."*

**The honest half.**

- **`mill_cert_id`, `machine_id` and `program_id` snapshot as `NULL` on the manual
  path** and print as blanks, not guesses. The machine and NC-program identity
  arrive on a probe event, which is Phase 2.
- **Actuals are typed in by hand.** `submit_measured_characteristics` and the
  three `RawProbeEvent` fields — the Phase-2 seam — were deliberately **not
  built**, because shipping an interface with no consumer is speculative.
- **The probe sources are stubs.** `MtconnectProbeSource` and
  `RenishawCentralSource` are both `todo!()`
  (`crates/aberp-qa/src/qc/probe.rs:116,146`); `MockProbeSource` is the only
  working implementation, and it is used in tests. **"Every part auto-probed,
  no-touch" is the vision, not the product** ([D-02](../BACKLOG-designed-to-live.md#d-02)).
- **`qty_reported` is the marked-unit count, not the work order's target.** With
  no marked units the report degrades to a single lot-level document and records
  `1`. It states what it accounted for.
- The `qc/` directory in the evidence bundle is **read-side only** — `aberp-verify`
  accepts and re-hashes `qc/` entries; nothing writes one yet.

---

### Act 6 · Four refusals that are code, not policy

**Surface A — screens, and this act is the best one in the deck because every
step ends in a visible refusal.**

**1 — No heat lot, no start.** `#/inventory-balances` → assign a heat lot and an
MTR reference against a material. Then try to start a defense work order without
one: the route refuses, and writes `material.wo_blocked_no_heat_lot`
(`serve.rs:17368,17518`).

**2 — No UID, no ship.** `#/work-orders` → **Mark parts**
(`POST /api/work-orders/:id/mark-parts`). Each unit gets a `dp-`-prefixed ULID and
a DataMatrix payload string; an operator serial is optional and auto-derives to
`<wo_id>-<index>`. Then try to ship before every unit is marked: **409**, naming
the work order, `qty_target` and `marked_count`, plus one `part.wo_blocked_no_uid`
ledger row (`serve.rs:17644`).

**3 — Open non-conformance, no ship.** `#/quality-ncrs` → raise an NCR against the
work order, leave it open, try to ship: **409** naming the blocking NCR ids, plus
`ncr.wo_blocked_by_open_ncr` (`serve.rs:17799`). A Critical NCR escalates if it is
not actioned within 24 hours. There is an explicit, audited waiver route
(`POST /api/ncrs/:id/shipment-waiver`) — show it, because "we have an override and
it is recorded" is a better story than "we have no override".

**4 — No issued QC report, no ship.** The third gate
(`enforce_qc_report_gate_for_shipment`, `serve.rs:18408`): **409** naming what is
missing, plus `qcr.report_shipment_blocked`. Sharp edge worth quoting: if a
report's `created_at` is unparseable the gate **refuses the whole shipment with a
500** rather than sorting the bad row to the bottom — because if that row is the
`reject`, sorting it down ships the part. *"Failing loud turns it into something
an operator must escalate rather than a release nobody notices."*

**And the fifth thing, which is not a gate but a transaction.** Three export rows
— `export.classification_set`, `export.access_check`, `export.shipment_logged` —
fire **inside the single `mark_shipped` transaction** that flips the dispatch
state. So an export record cannot exist for a shipment that rolled back, and a
shipment cannot exist without its export rows.

**The honest half, and this one is a selling point.** The denied-party screening
backend is **not wired** ([D-01](../BACKLOG-designed-to-live.md#d-01)):
`ExportControlProvider` defines `classify` and `screen_party`, and
`MockExportControlProvider` is the only implementation. With no denied-party list
behind it, the provider answers **`not_determined`** with `backend: "mock"` —
**never `granted`**. That is enforced in the data, not just in a README: a
`granted` row on an append-only ledger would assert that a screen ran and cleared,
and it could never be corrected. Timestamps come from the system clock rather than
the operator's ship date, so a back-dated shipment cannot claim its screening ran
in the past. *"We built the gate, the transaction and the recording. We have not
bought the screening list, and rather than fake it the system records the absence
of a determination."*

Approved vendors (`#/avl-vendors`) and purchase orders (`#/purchase-orders`) are
the front half of the same story: vendor CRUD with screening and approval
categories — the closed vocabulary is six values: `general`, `itar`, `ear99`,
`aerospace`, `defense`, `nuclear` — a PO-eligibility gate that blocks suspended or revoked vendors at
create *and* issue, defense lines that require a heat lot at receipt, and a failed
incoming inspection that auto-raises an NCR.

---

### Act 7 · Identity, and the compliance vocabulary

**Mixed — one live API route, one screen-less crate. Keep this act short and be
blunt; it is the weakest part of the story and pretending otherwise is expensive.**

What is live: **`POST /api/dap/mock-login`** returns, verbatim *(measured)*:

```json
{"attested_at_utc":"2026-06-17T00:00:00Z","display_name":"Mock DÁP Operator",
 "mock":true,"subject":"hu-mock-citizen-0001"}
```

`"mock": true` is on the wire. That is the whole posture in one field.

With the per-tenant `dap_enabled` toggle on (**default off**), boot mints an
ed25519 service session, recovers sessions a crash left open, signs entries and
takes periodic timestamp anchors. **That machinery is live. The timestamp
authority behind it is not** — `NetlockTsa` compiles in with every method
`todo!()`, and `MockTimestampAuthority` is what runs
([D-06](../BACKLOG-designed-to-live.md#d-06)). Treat `auth.*` and `audit.*` as a
working **structural floor, not a qualified signature.**

`crates/aberp-compliance` holds the defense vocabulary as validated types with no
operator surface: `avl` (DPAS `DO`/`DX` × 15 CFR 700 program symbols), `cui`
(32 CFR 2002 markings, including the `SP-` Specified prefix, rendered from typed
values so a free-text banner can never reach a deliverable), `export_control`,
`incident` (which computes the DFARS 252.204-7012 **72-hour** reporting deadline
the moment a detection is stamped), `lot_heat`, `nist_800_171` (all 110 DFARS
control identifiers as constants), and `uid` (MIL-STD-130N `IuidConstruct1` /
`IuidConstruct2`, `validate_iac()`, IRI rendering).

**Say the hard sentence.** Per-unit marking is live today — but it mints a
`dp-`-prefixed ULID and a DataMatrix payload, **not a DoD IUID**. Minting a real
one needs an assigned enterprise identifier, which is a registration, not a
sprint ([D-03](../BACKLOG-designed-to-live.md#d-03)). Same for CAC/PIV: the
`DigitalIdProvider` trait has two backends, and `UsDodCacProvider` is a card-session
stub that WARNs on construction and makes `current_operator()` genuinely fallible
([D-07](../BACKLOG-designed-to-live.md#d-07)).

**Why the act still earns its four minutes.** The point is not the mocks. The
point is that on an export-controlled product, **a designed control described as
active is the dangerous overstatement** — and this system is built so the
overstatement is structurally hard to make: the mock refuses to say `granted`,
the DÁP response says `mock: true`, the timestamp authority panics rather than
pretending, and the README carries a Live-vs-Designed status on every capability
with a backlog entry behind each Designed one.

---

### Act 8 · Durability — the part nobody asks about until it is too late

**Surface A — `#/snapshots`. Plus surface B for the guarded restore.**

The screen lists managed snapshots newest-first with sequence, UTC timestamp,
size, validation status and age; it has a **Snapshot now** button and a **guarded
restore** wizard. Live, on a cold-booted tenant *(measured)*: the daemon interval
is **14400 s (4 h)**, the store is
`~/Documents/ABERP-snapshots-<edition>/<tenant>`, and the first snapshot appeared
within a minute of boot — `seq 1`, 66.9 KiB, `valid: true`, `invoice_count: 0`,
`audit_count: 1`, `chain_len: 1`.

These are **logical** DuckDB exports (`EXPORT`/`IMPORT DATABASE`), smoke-tested on
the way out. A snapshot that fails validation is **kept as forensic evidence**,
not deleted.

The CLI is the operator surface for everything the screen does not do:

```
aberp snapshot now | list | restore | prune [--dry-run]
aberp recover                  # guarded, reversible crash recovery — the ONE manual path
aberp restore                  # the guarded IN-PLACE restore (ADR-0116 D3.4)
aberp evidence list | archive  # recovery-evidence inventory + archive-then-remove
```

Two things worth reading aloud from `aberp restore --help`. First, what it
replaces: *"the documented hand-swap — restore to a side path, stop serve, swap
the file in by hand — is the per-incident manual step the durability programme set
out to eliminate, and it is unjournalled: a crash mid-swap is on the operator, not
on the code."* It now refuses unless serve is stopped, snapshots the current DB
**first**, moves it aside as a `.PRE-RESTORE-<tag>` unit, installs crash-safely,
re-verifies, and records the restore on the restored chain with a durable ack.
Second, `aberp evidence archive` is **archive-then-remove**: copy to
`~/Documents/ABERP-evidence/<tenant>/<incident-tag>/`, verify the copy by SHA-256,
and only then unlink — so *"pruned" never means "gone"*, and the periodic daemon
never runs either evidence command.

**Why it matters.** This shipped as `PROD_Defense_v0.6.4` and it is in production
now. The reason it exists is that this system lost invoices to an unclean restart
once, and the answer was not a bigger warning in the runbook. It was: automatic
validated snapshots, a boot-time mirror/DB reconciliation that repairs a
divergence and records the repair as `db.auto_recovered`, a durable-ack contract
on every money-moving write, an in-place restore that journals the swap, and a
retention policy that cannot silently eat the evidence of an incident.

---

### Act 9 · Prove it afterwards, without trusting us

**Surface A — `#/audit-events`. Plus surface B — `aberp-verify`.**

The Activity log is the whole ledger, filterable server-side by date range, kind,
domain prefix, subject (matched across own id *and* chain-base id, so a storno is
found by the invoice it credits), operator and free text, with cursor pagination.
The detail view returns the full typed payload plus a **recomputed `hash_ok`**,
`prev_hash` and `entry_hash`.

The chain: `entry_hash[N] = SHA-256(canonical-CBOR(entry[N] with prev_hash =
entry_hash[N-1]))`, anchored at a genesis hash derived from the tenant id, one
canonical encoder used by both writer and verifier. All appends funnel through the
shared `aberp-db` handle (`Handle::with_ledger`), so daemons and request handlers
serialise instead of forking the chain; `append_in_tx` takes a **caller-owned
transaction**, so the state change and its audit entry commit or roll back
together. A JSONL mirror sits alongside the DuckDB table with a boot-time
consistency check.

Then the closer: `aberp export-invoice-bundle` produces one `.tar.zst` — chain
slice, NAV request/response archive, manifest — and **`aberp-verify` re-verifies
it from its bytes alone**, in a separate crate that deliberately does not inherit
the NAV, billing, DuckDB-write or Tauri dependencies, and reports every check it
ran rather than stopping at the first failure.

**And the gate that keeps it true.** The release cut refuses a build whose sources
grow a new ledger opener or a wrapper that appends outside the shared handle
(`tools/adr0099_write_fork_scan.awk`, `tools/adr0105_wrapper_fork_scan.awk`), and
`tools/cut_gate_negative_probes.sh` **re-plants each defect on every run to prove
the scanner still catches it**. A tamper-evidence property that is only asserted
in prose decays; this one is re-tested against a deliberately reintroduced bug
before every release.

**Number to quote carefully:** the audit vocabulary is **197 `EventKind`s** at
`77664e9`, pinned by `all_kinds_count_is_pinned` and by two matching drift
assertions in `aberp-verify` and `export_invoice_bundle` — so a new kind forces a
re-review of the NAV-leakage gate. **The README's "187 kinds / 170 Live / 17
Designed" is stale.** Say 197, or say "just under two hundred", and do not read
the README's breakdown aloud.

---

### Act 10 · `/shop` — the grant-board act

**Surface D — a browser at `http://localhost:5199/shop` (or the deployed site).**

Read the page top to bottom with the evaluators. It is one argument in four bands.

**The hero.** *"ONE FILE IN. ONE CERTIFIED PART OUT. Nobody in between. That is
the cell we are building — and it is not built. What is built is the software
underneath it."* Under the cinematic render, in its own caption: **ILLUSTRATION
ONLY — A drawing of the cell we are building. Not a photograph, not our floor —
Áben owns no machine tool.** Then a four-line status strip: Online quoting *Live*
· ABERP ERP *Live · Defense pilot* · NAV invoicing *Live* · **The machine shop
*Not built***.

**Band 01 — six things working today**, five marked `LIVE` and one marked `IN
SOFTWARE`: *"Heat and lot traceability, part-UID records, NCR/CAPA and the
shipment gates … all run in ABERP. No real part has ever passed through them —
there is no cell yet to make one."*

**Band 03 — the chain, eleven links, each carrying its own state and a *where it
stands* line that is never softened.** This is the band to dwell on:

| # | Link | State | Where it stands |
|---|---|---|---|
| 01 | Quote with the inspection already in it | `DESIGNED` | the engine is live; the line is not in it |
| 02 | No heat lot, no start | `IN SOFTWARE` | runs today; no steel has passed through it |
| 03 | One key for the whole life of the part | `DESIGNED` | the code that mints it at release does not exist |
| 04 | The robot loads it, the robot unloads it | `TO BUY` | ABERP can read a robot's telemetry, it cannot command one |
| 05 | One clamping, turning and milling | `TO BUY` | a purchase decision, not an asset |
| 06 | Probed on the machine, every part | `DESIGNED` | the ingest path is built; the source that would feed it is stubbed |
| 07 | Peened, then read back | `DESIGNED` | no marker connected, no part ever marked |
| 08 | Every scan is a genealogy event | `DESIGNED` | the scanner adapter works; nothing turns a scan into a part event |
| 09 | The paperwork writes itself | `IN SOFTWARE` | AS9102 Rev C + CoC generate; they cannot be filled from a real probe |
| 10 | Ship — or refuse to | `IN SOFTWARE` | the gates run; nothing has been shipped through them |
| 11 | One key joins the whole file | `IN SOFTWARE` | the ledger is live; the manufacturing events do not exist yet |

**Band 04 — what we are not claiming.** Certification: *"None. No ISO 9001, no
AS9100, no NADCAP — and no application filed for any of them."* Parts delivered:
*"Zero."* Machines owned: *"None."* Lead times: *"Unpublished."* Dates: *"None
anywhere on this page."* Photographs: *"None."*

**Why it matters, for the grant framing.** The page's honesty is a **mechanism**,
not a tone, and the mechanism is written into the source file as a contract the
next editor must not break: every image is a render and carries its caption; every
capability carries one of exactly four states (`live` / `software` / `designed` /
`buy`) with the vocabulary deliberately narrow *"so nothing can be read as more
than it is"*; nothing is in the past tense unless it happened; and there are no
numbers on the page the company cannot stand behind. The file's own rule for the
next person: ***"If you are unsure which state something is in, it goes in the
weaker one."***

For an evaluator, that is the whole case: an applicant who marks its own gaps in
a public vocabulary, whose engineering backlog
(`docs/BACKLOG-designed-to-live.md`, D-01 … D-22) maps one-to-one onto the page's
`designed` and `buy` entries, and whose ask is precisely the `buy` column.

**⚠️ Two things to correct before you brief anyone on this page.**

1. **The images are not C2PA-signed.** There is no C2PA, Content Credentials or
   provenance-manifest implementation anywhere in `ABERP-site`
   *(verified by grep)*. The honesty mechanism is the visible `ILLUSTRATION ONLY`
   caption, the four-state vocabulary and the no-numbers rule — which is a
   *stronger* argument for a human reader anyway, because they can check it
   themselves. **Do not tell a grant board the renders are cryptographically
   signed.** If C2PA signing is wanted it is real, unstarted work.
2. **The page cites a north-star document that is not on `main`.** Its header
   comment names `ABERP-Editions docs/dream-shop-workflow.md` as the source the
   public page must not drift optimistic against. That file **does not exist on
   `origin/main`** — it lives on the unmerged branch `docs/adr-db-snapshot-system`
   at `1c7c686`. The page is fine; the citation currently points at nothing a
   reader could be handed.

---

## 4 · What is demoable on screen, and what is not — the honest table

**Read this section before you promise a screen.** The SPA's complete route list
at `77664e9` (`apps/aberp-ui/ui/src/lib/router.ts`, the `AppRoute` union) is:

`invoices` · `invoices-new` · `statistics` · `tenant` · `nav-credentials` ·
`partners` · `products` · `machines` · `inspection-plans` · `margin-profiles` ·
`avl-vendors` · `work-orders` · `qa` · `dispatch` · `workshop` · `maintenance` ·
`restore-from-nav` · `adapters` · `material-catalogue` ·
`quoting-complexity-rules` · `quoting-tolerance-multipliers` ·
`quoting-parameters` · `quoting-stock-adjustments` · `quoting-machine-rates` ·
`quoting-gear-processes` · `quoting-tolerance-cost-rates` · `inventory-balances` ·
`email-relay-queue` · `audit-events` · `snapshots` · `calibration` ·
`material-traceability` · `quality-ncrs` · `purchase-orders` · `tenants`.

**That is all of them.** Quotes and the CAD pricing pipeline are **tabs** inside
`#/invoices`, not routes of their own. There are **151** `/api/*` routes behind
them.

| # | Capability | On screen? | What exists | What is missing |
|---|---|---|---|---|
| 1 | **QC report / AS9102 FAIR / CoC** | ❌ **No screen at all** | Eight Defense-only routes, three render shapes (`aberp-qc-pdf`), the frozen record + pure accountability core, the SHA-256 issuance pin, the shipment gate, six `qcr.*` kinds | **The largest gap.** `qc-report` appears in **zero** SPA files. No draft screen, no per-characteristic chip row, no "characteristics accounted for: 11 / 14" counter, no preview, no issue button — all of which ADR-0199 §D10 specifies |
| 2 | **`located_holes` / CAD geometry** | ⚠️ **Terminal only** | The extractor runs, emits FeatureGraph v6 with real hole axes/depths/entry points, refuses assemblies loudly | Nothing renders a hole. No viewer, no overlay, and by design **no pricing consumer** — [D-19](../BACKLOG-designed-to-live.md#d-19) gates that until five geometry defects close |
| 3 | **CAD → quote pipeline** | ✅ **Yes** | `#/invoices` → Pricing tab: job list, job detail, material/stock-form/gear-ops/tolerance edits, margin + lead-time overrides, PDF render, per-job audit | The reasoning log is the best thing in it and it is behind a click — it is not the headline of the detail screen |
| 4 | **Tolerance cost rates** | ✅ **Yes, fully** | `#/quoting-tolerance-cost-rates`, all five bands, every field editable, seeds visibly stamped | — |
| 5 | **Workshop wall-TV + MES adapters** | ✅ **Yes, and it has a demo mode** | One bundled endpoint, 10 s poll, seven tiles, health dots, alert chime; 5-tap demo mode with mock data and three kinetic effects | Real mode on a fresh Defense tenant is **all zeros**. The demo needs either a hand-built tenant or the demo-mode gesture |
| 6 | **The four shipment/start gates** | ✅ **Yes** | Heat-lot gate on WO start; part-UID, open-NCR and QC-report gates on dispatch ship — each a 409 with a named reason and one audit row | Nothing shows the *four gates as a set*; you demo them one screen at a time |
| 7 | **AVL + purchasing** | ✅ **Yes** | `#/avl-vendors` (CRUD, screening, categories, status), `#/purchase-orders` (create, issue, receive, auto-NCR on failed inspection) | The **screening backend** is a mock that can only answer `not_determined` |
| 8 | **Heat/lot + part-UID traceability** | ⚠️ **Mostly** | `#/inventory-balances` assigns heat lot + MTR; `#/work-orders` → Mark parts mints UIDs; `#/material-traceability` is a chain-of-custody lookup | Part-UID *lookup* is API-only (`GET /api/part-traceability?part_uid=…`) — there is no `part-uid` route in the SPA |
| 9 | **Snapshots + restore** | ✅ **Yes** | `#/snapshots`: list, snapshot-now, guarded restore wizard | `aberp recover`, `aberp restore` (in-place) and `aberp evidence list/archive` are **CLI-only** — the incident paths are the ones without a screen |
| 10 | **Audit ledger** | ✅ **Yes, fully** | `#/audit-events` with server-side filters, cursor pagination, per-row recomputed `hash_ok`, typed payload detail | `aberp-verify` and `export-invoice-bundle` are CLI — correctly, since the point is not trusting the app |
| 11 | **Digital identity (DÁP / CAC)** | ⚠️ **One route, no screen** | `POST /api/dap/mock-login` works end to end and answers `"mock": true` | No sign-in screen; `dap_enabled` defaults **off**; the OIDC transport, the NETLOCK TSA and the PKCS#11 card reader are all `todo!()` |
| 12 | **Probe ingestion ("MC Connect")** | ❌ **No screen, no transport** | `ProbeIngestionSource` + `ProbeCursor` + `RawProbeEvent`; a working `MockProbeSource`; `qc.probe_ingestion_failed` is a live emitter | `MtconnectProbeSource` and `RenishawCentralSource` are `todo!()`. QC actuals are typed by hand |
| 13 | **CUI / incident / NIST-800-171 / IUID** | ❌ **No screen, no firing site** | Validated types in `aberp-compliance`; the DFARS 72-hour clock computes; all 110 control ids are constants; IUID constructs 1 & 2 build and validate | Nothing writes a `cui.*`, `personnel.*` or `incident.*` event. Marking mints a `dp-` ULID, not a DoD IUID |
| 14 | **`/shop` vision page** | ✅ **Yes, fully** | A finished, self-honest SvelteKit page with renders, the four-state vocabulary and the not-claiming band | Separate repo, needs five env vars to boot, and the renders are **not** C2PA-signed |

### 4.1 · What would make each red row demoable — most demo bought per unit of work

1. **A QC-report screen (#1) — by a wide margin the highest-value item.** Every
   route, every vocabulary, all three render shapes and the accountability
   arithmetic already exist and are already mutation-tested. The work is SPA-side
   only: a report list on the Dispatch and Work Order detail, the
   green/amber/red/grey chip per characteristic, the *"characteristics accounted
   for: 11 / 14"* counter that stays red until complete, a Generate button that is
   disabled with a **specific reason**, and a preview before issuance — exactly
   what ADR-0199 §D10 already specifies, so there is no design left to do.
   **Until this lands, the single capability an aerospace prime came to see is
   demoed from a terminal** — which is backwards, because it is the item aimed at
   the audience least likely to read one.
2. **A seeded Defense demo tenant (#5, #6, #8) — small, and it multiplies four
   other rows.** Portable already ships a demo seed that makes first boot land on
   a populated dashboard. A Defense equivalent — a handful of partners with
   `customer_type` defense, two work orders, a heat lot, marked units, one open
   NCR and one drafted dispatch — turns the workshop wall, all four gates and both
   traceability screens from *"let me type for ten minutes"* into *"look at
   this"*. This is the cheapest red→green on the list.
3. **A part-UID lookup screen (#8) — small.** `GET /api/part-traceability` already
   returns forward trace (UID → WO → heat lot → quote → customer) and reverse
   trace (customer → every UID shipped). One route, one search box, two tables.
   It is also the screen a customer will ask to see the moment you say the words
   *"we can walk any unit back to the bar it came out of"*.
4. **The reasoning log promoted on the pricing detail (#3) — very small.** It is
   the single most persuasive artefact the quoting engine produces and it is
   currently one click away and unlabelled.
5. **A DÁP sign-in screen (#11) — small, but hold it.** `dap-signin.ts` exists in
   the SPA lib and the mock route works end to end. It is deliberately ranked low:
   a sign-in screen driven by a mock identity provider is the one piece of theatre
   in this deck that could be *mistaken* for a real control, and the whole
   argument of this demo is that the system does not do that. Build it when the
   OIDC transport lands, not before.
6. **`located_holes` visualised (#2) — medium, and correctly last.** A 2-D
   hole-pattern overlay would be striking, and it would also be the first thing
   that tempts somebody to price off the field before
   [D-19](../BACKLOG-designed-to-live.md#d-19)'s five defects are closed. If it is
   built, it must carry the "not used for pricing" label in the component itself.

### 4.2 · Demo hygiene, noticed while verifying

- **The README's audit numbers are stale.** It says *"187 kinds are defined: 170
  are Live, 17 are Designed"*; the code pins **197**. A prospect who counts will
  not, but a grant evaluator reading the README next to the code might.
- **The README's Status section names the wrong release.** It still says *"Current
  Defense stable: `PROD_Defense_v0.2.1` (cut 2026-06-16)"* and the Quick Start
  clones that branch. The current cut is **`PROD_Defense_v0.6.4`** (2026-08-30) —
  four minor versions and the entire snapshot system later. **Do not run the
  README's Defense quick-start command in front of anyone.**
- **`aberp serve --help` describes a product from two years ago.** It says
  *"Routes are read-only over the billing DB + audit ledger. Mutations remain on
  the CLI subcommands"*. There are 151 API routes and a great many of them are
  `POST`/`PUT`. Cosmetic, but it is help text an engineer in the room may run.
- **`aberp --version` prints `aberp 0.0.0`.** The workspace version is not the
  release identity; the real identity is the `binary_hash` on `GET /health`
  (a SHA-256 of the running binary, computed in the background at boot — it took
  ~24 s in my run, so it is briefly absent right after launch). If someone asks
  "what version is this", answer with the branch name and the health endpoint's
  hash, not with `--version`.
- **The workshop demo mock lists an adapter kind that does not exist.** Its four
  adapters include `scale-shipping-01` with `kind: "weight_scale"`, which is not
  one of the five values in `AdapterKind` (`barcode-scanner`, `label-printer`,
  `cnc-machine`, `robot`, `laser-cutter`). Nobody will notice; but if you are
  showing demo mode to an integrator, do not let them ask you about the scale.
- **Demo mode is scoped to the wall, and `#/adapters` knows it.** The flag
  short-circuits `getWorkshopDashboard()` only, so navigating to `#/adapters`
  during a demo-mode tour shows the **real** (on a fresh tenant, empty) registry.
  That screen at least defends itself: with the flag on it still renders but
  refuses every mutation, disables Add/Edit/Delete and shows a
  `Demo mode — adapter changes disabled` banner. Useful to know, but it means the
  wall's four adapters and the Adapters screen's zero adapters are both on
  screen if you navigate. **Stay on the wall.**

---

## 5 · The sentences to be careful with

The whole argument of this demo is that nothing in it is theatre. These are the
places where a careless sentence costs more than the point it makes.

| Do not say | Say instead |
|---|---|
| *"Every quote carries an inspection cost."* | *"Tolerance moves the price through inspection minutes, scrap, feed rate and a grinding escalation at the tightest band. A per-quote inspection line on every part regardless of tolerance is designed and is not in the engine — our own public page marks it `DESIGNED`."* |
| *"We extract the features from your CAD."* | *"We extract the solid — bounding box, volume, surface area — and every drilled hole with its axis, depth and entry point. The feature list is empty by construction today; machining time comes from removed volume, surface area and the complexity rules."* |
| *"We price off the hole geometry."* | *"We compute it and we deliberately do not price off it yet — there are five named geometry defects, four of which would under-quote, and a gate that stops a pricing slice consuming the field until they are closed."* |
| *"Every part is auto-probed on the machine."* | *"The ingest path, the verdict arithmetic and the automatic non-conformance are built. The probe source is stubbed and there is no machine. Actuals are entered by hand today."* |
| *"We have a DMG MORI integration."* | *"We speak MTConnect, which is what the NTX's IoTconnector emits — and the same code already serves two machine families, which is why we chose the standard over the vendor SDK."* |
| *"We screen your consignees against the denied-party list."* | *"The gate, the transaction and the recording are live. The screening list is not bought, so the provider answers `not_determined` and it is **structurally incapable** of answering `granted`. We would rather record the absence of a determination than manufacture one."* |
| *"The audit chain is qualified-signed."* | *"The chain is hash-linked and the session signing machinery is live. The timestamp authority is a mock — `NetlockTsa` is `todo!()`. Treat it as a structural floor, not a qualified signature."* |
| *"Every part carries a MIL-STD-130N IUID."* | *"Every unit carries a minted UID and a DataMatrix payload, and no defense shipment can leave without one. It is not a DoD IUID yet — that needs an assigned enterprise identifier."* |
| *"Here's the QC report screen."* | Do not open the app. *"The record, the three documents and the shipment gate are done; the operator screen is the next thing we build — here is the API and here is the rendered PDF."* |
| *"The renders are C2PA-signed."* | *"Every image is captioned `Illustration only` in the page itself, every capability carries one of four states, and there are no numbers on the page we cannot stand behind. You can check all three yourself, which is the point."* |
| *"We're AS9100 / ISO 9001 aligned."* | Read the page's own line: *"None. No ISO 9001, no AS9100, no NADCAP — and no application filed for any of them."* |

And the frame to keep for the whole meeting, because it is the actual product:

> *"The house is wired for more sockets than currently have an appliance plugged
> in — and on an export-controlled product, a designed control described as an
> operational one is the dangerous overstatement. So every capability carries one
> of two statuses, nothing is listed unless its code surface was verified in the
> repo, and every Designed item has a backlog entry saying exactly what is missing.
> You have just watched me mark our own gaps six times."*

---

## 6 · Appendix — the verification transcript

Everything asserted above was read at `origin/main` **`77664e9`**. The
non-obvious claims were captured from software actually booted at that commit,
not from comments:

| Claim | How it was checked |
|---|---|
| The `aberp` binary builds at this commit | `cargo build -p aberp --bin aberp` → exit 0 |
| **The Defense arm compiles** | `cargo check -p aberp --features production --bin aberp` → `Finished dev profile … in 5m 09s`, exit 0 |
| The SPA builds | `npm run build` in `apps/aberp-ui/ui` → `✓ built in 30.71s`, `dist/` produced |
| **QC-report routes are Defense-only** | Booted the dev build: `GET /api/qc-reports` → **404**, while `/api/work-orders`, `/api/dispatches`, `/api/avl-vendors`, `/api/ncrs`, `/api/snapshots`, `/api/adapters`, `/api/audit-events` all → 200 |
| Storefront reach is compile-gated | Boot log, verbatim: `email-outbox poll daemon NOT spawned — storefront reach is a Defense-only capability` (and the same for the pdf-rerender daemon) |
| Health / binary identity | `GET /health` → `{"ok":true,"binary_hash":"f598f0b3…","nav_xsd_version":"3.0","is_production_build":false,"first_prod_launch_required":false}`; the hash took 23891 ms to compute at boot |
| DÁP is a mock, on the wire | `POST /api/dap/mock-login` → `{"attested_at_utc":"2026-06-17T00:00:00Z","display_name":"Mock DÁP Operator","mock":true,"subject":"hu-mock-citizen-0001"}` |
| Snapshot daemon cadence, store and first snapshot | `GET /api/snapshots` on a cold tenant → `store_dir` `~/Documents/ABERP-snapshots-portable/demo`, `interval_secs 14400`, `seq 1`, `66.9 KiB`, `valid true`, `chain_len 1`, taken ~60 s after boot |
| A fresh tenant is empty | `GET /api/workshop/dashboard` → every work-order, QA and dispatch counter `0` |
| **The extractor runs, and `features[]` is empty** | `stepped_bore.step` → `_schema_version 6`, `features: []`, two located holes (Ø6 through, Ø14 blind flat-bottom) |
| Four holes, exact coordinates | `plate_4_through_holes.step` → 4 × Ø8 through at (20,20), (20,40), (80,20), (80,40) |
| The cross-drill depth is measured to the bar, not the exit curve | `cross_drilled_shaft.step` → 1 hole, Ø8, depth `22.360679774997898` = 2·√125 |
| Assemblies are refused loudly | `assembly_two_solids.step` → `"STEP file contains an assembly with 2 solids; only single-part STEP is supported in v1"` |
| **Stored CAD is encrypted at rest** | `quote-artifacts/…/pump_adapter_v16.step` begins with magic `ABRPCAD1`; extracting it → `"STEP file could not be parsed (OCCT ReadFile status=3)"` |
| The SPA has no QC-report surface | `grep -rIn 'qc-report\|qcReport\|qc_report' apps/aberp-ui/ui/src/` → **no matches** |
| The SPA can only be reached through Tauri | `apps/aberp-ui/ui/src/lib/api.ts:1` — *"Tauri command surface — the SPA's ONLY path to the backend"* |
| The route surface | 35 `AppRoute` variants in `router.ts`; **151** distinct `/api/*` route strings in `serve.rs` |
| The audit vocabulary is 197 | `all_kinds_count_is_pinned` asserts `197`; counting `EventKind::` entries in `ALL_KINDS` → 197 |
| AVL approval categories | `ApprovalCategory` in `crates/aberp-compliance/src/avl/mod.rs:322-337` — six variants: `general`, `itar`, `ear99`, `aerospace`, `defense`, `nuclear` |
| Demo mode's blast radius | `api.ts:5371` short-circuits only `getWorkshopDashboard()`; `AdaptersList.svelte` separately reads the flag and disables every mutation behind a banner |
| The probe sources are stubs | `crates/aberp-qa/src/qc/probe.rs:116,146` — two `todo!()`; no `impl ProbeIngestionSource` outside that file |
| The current release | `git ls-remote --heads origin 'PROD_Defense_v*'` → newest `PROD_Defense_v0.6.4` = `5bd846e`; `git log 5bd846e..origin/main` → three CI-only commits. **No `PROD_Defense` tag exists on origin** |
| **`/shop` refuses to boot bare** | `npm run dev` with no env → `service unavailable: storefront boot checks failed` + five numbered diagnostics (F19/F8/F15/F-CAT/F-QUOTE) |
| `/shop` renders | With the five env vars set: hero, `ILLUSTRATION ONLY` caption, four-line status strip, the 11-link chain with per-link states, the not-claiming band — all captured from `localhost:5199/shop` at `ABERP-site` `9875c11` |
| **No C2PA anywhere** | `grep -rIn 'c2pa\|C2PA\|Content Credentials\|provenance'` over `ABERP-site/src`, `static`, `docs`, `tools` → three unrelated hits on quote-acceptance provenance, zero image provenance |
| The north-star doc is not on main | `docs/dream-shop-workflow.md` does not exist at `origin/main`; it is on the unmerged branch `docs/adr-db-snapshot-system` at `1c7c686` |

**What I could not verify, and why.**

- **The Defense build has not been booted here.** `--features production`
  type-checks clean, but launching it reads the operator's real macOS keychain,
  requires a `seller.toml`, and targets the live NAV endpoint — so the Defense-only
  behaviour (the QC-report routes answering 200, the storefront daemons spawning,
  `~/.aberp-defense` as the data root) is verified by the compile-time gate and by
  the dev build's *refusals*, not by a Defense boot.
- **No screen was clicked.** The SPA compiles and its route table and component
  wiring were read; the Tauri desktop shell was not launched, because that is the
  production path. Screen behaviour is asserted from source, not from pixels.
- **The three PDF shapes were not rendered.** `aberp-qc-pdf` compiles under
  `--features production`; no report was drafted, issued or rendered, because
  doing so needs a Defense binary and a tenant with inspection data.
- **`aberp-verify` was not run against a bundle.** No invoice exists in the scratch
  tenant to export.
