# ABERP

**A free desktop ERP for small manufacturing shops.** Clone it, run one
command, and in about five minutes you have a working system on your own
Mac — quoting, invoicing, partners, products, machines, an approved-vendor
list, material traceability, and a tamper-evident audit trail. No SaaS, no
account to create, no monthly bill, no Docker. It runs locally as a single
desktop app and your data never leaves your machine.

ABERP started as a tool for Hungarian shops filing invoices through the
NAV Online Számla system. It has since grown a **Portable** edition that
anyone, anywhere can use — with the Hungarian tax integration switched off
and a demo company pre-loaded so the very first launch already has data to
click around in. It is multi-tenant (run several companies side by side),
multi-currency, and every change you make lands in an append-only,
hash-chained ledger you can inspect and verify.

> **License — free for non-commercial use (PolyForm Noncommercial 1.0.0).**
> You may use, run, modify, and share ABERP for any non-commercial purpose
> at no cost. Commercial use needs a separate arrangement — see
> [License](#license) below and [`LICENSE`](LICENSE) for the full terms.
> (Note: PolyForm Noncommercial is *source-available* and free, but it is
> not an OSI-approved "open-source" license, because it restricts
> commercial use.)

---

## Two editions

| | **Portable** | **Defense (HU production)** |
|---|---|---|
| Latest | `PROD_Portable_v0.1.2` (2026-06-16) | `PROD_Defense_v0.2.1` (2026-06-16) |
| For | Anyone, anywhere — evaluating, or running outside Hungary | Hungarian manufacturing shops with NAV obligations + defense / aerospace compliance needs |
| Tax filing | **Off by default** — invoices stay local (LocalOnly) | Live NAV Online Számla 3.0 e-invoicing |
| First boot | Demo company pre-seeded — data to explore immediately | Your own seller profile + real NAV credentials |
| Build | Dev profile — structurally cannot reach the live NAV endpoint | `--features production` — the real-money build |
| Install | `./run/upgrade_portable.sh` | `./run/upgrade_defense.sh` |

**Portable** is the path most newcomers want. It is the same application —
quoting, manufacturing, the audit ledger, all of it — with the Hungarian
NAV submission turned off per tenant. You can enter tax numbers for your
own country (they are stored as opaque strings for now; country-specific
tax modules are on the [roadmap](#roadmap)).

**Defense (HU production)** adds live NAV Online Számla 3.0 invoicing plus
the defense/aerospace compliance stack: approved-vendor screening,
purchase orders gated on that AVL, lot/heat material traceability, per-unit
part UID marking, an NCR/CAPA quality workflow with shipment gates, QC
inspection plans, and the production build that talks to the real NAV
endpoint. It is what Hungarian shops with real NAV submission obligations
run for real money.

> **The legacy unified `PROD_v2.27.76` line is frozen.** Up to that tag,
> Portable and Defense shipped as one build. New work now lands on the two
> dedicated lines above — Portable for everyone, Defense for HU production
> — so each edition gets a launcher and an upgrade path scoped to it.
> Existing `PROD_v2.27.76` installs keep working; there is just no
> `PROD_v2.27.77`.

---

## Quick start — Portable

On a Mac, from a terminal:

```bash
git clone https://github.com/Cservin69/ABERP.git ABERP-Portable
cd ABERP-Portable
git fetch origin --tags
./run/upgrade_portable.sh PROD_Portable_v0.1.2
```

That last command does everything for you, in order:

1. Confirms the `PROD_Portable_v0.1.2` release exists on GitHub.
2. Snapshots any existing tenant data first (skipped on a fresh install —
   nothing to roll back to yet).
3. Resets your checkout cleanly to the release.
4. Provisions a small Python environment for the CAD geometry pipeline
   (so STL/STEP quoting works without you installing anything by hand).
5. Builds and launches the desktop app straight into the **demo** tenant.

The first window opens on a dashboard (not a setup wizard) with a sample
company already populated — partners, products, and machines to click
through. A friendly green **"PORTABLE BUILD — NO NAV — local-only"** banner
in the launch terminal confirms no invoices will be filed anywhere.

To run your own company instead of the demo, give the tenant a name:

```bash
ABERP_TENANT=acme ./run/run_portable.sh
```

> **macOS only, for now.** Shipped builds target macOS (the desktop shell
> and keychain integration need per-OS work). Linux and Windows are
> [roadmap](#roadmap) items — honestly not there yet.

### Prerequisites

The launcher needs these on your `PATH`; install them once if missing:

- **Rust** (stable channel) — `rust-toolchain.toml` pins the version, so
  `rustup` resolves it on first build.
- **Node.js 20+** with **npm**.
- **Python 3.11+** — only for the CAD geometry pipeline; quoting works
  without it, you just won't get geometry-driven machining estimates.

That's it. Build artifacts stay under `target/` and `apps/aberp-ui/ui/dist/`;
your runtime data lives under `~/.aberp/<tenant>/`.

---

## Quick start — Defense (HU production)

**For Hungarian operators with real NAV credentials and live NAV
submission obligations.** This builds with `--features production`, talks
to the real NAV Online Számla endpoint, and files invoices for real. Don't
run it unless that's what you want — Portable above is the safe sandbox.

```bash
git clone https://github.com/Cservin69/ABERP.git ABERP-Defense
cd ABERP-Defense
git fetch origin --tags
./run/upgrade_defense.sh PROD_Defense_v0.2.1
```

`upgrade_defense.sh` mirrors the Portable upgrade — confirm the release,
snapshot existing tenant data, reset cleanly, provision the CAD Python
environment, build, launch — but it is the real-money path: it **requires**
a tenant and seller profile, forces a mandatory snapshot (no skip), and
launches into the production build with the **"DEFENSE MODE: AVL + heat/lot
+ DÁP-ready"** banner. Set up your NAV + SMTP credentials first (see
[recipe 7](#7-set-up-nav-creds--smtp-on-a-fresh-box-új-gépen-alapbeállítás)
and the [runbook](docs/CUTOVER_RUNBOOK.md)).

---

## What it does

Organized the way an operator actually works. Tags mark where a feature
lives: **[both]** ships in Portable and Defense, **[Defense]** is part of
the HU-production compliance stack.

**Quote → price → win the job**

- **Quoting (CAD-aware)** *[both]*. Drop in an STL or STEP file → it
  extracts the geometry → estimates machining time → applies the margin
  profile for that customer type → shows a lead-time chip (green / yellow /
  red) → renders a customer-ready PDF. Quotes that would price below the
  margin floor are refused outright, not silently shipped.

**Procure → make → inspect → ship**

- **Approved Vendor List** *[Defense]*. Vendor CRUD with screening and
  approval categories (ITAR, EAR99, Aerospace, Defense, Nuclear), plus a
  purchase-order eligibility gate so unscreened vendors can't slip through.
- **Purchasing / purchase orders** *[Defense]*. Raise POs against the AVL
  (suspended or revoked vendors are blocked at create and issue); receiving
  a failed inspection auto-raises an NCR; defense lines require a heat lot
  captured at receipt.
- **Material traceability** *[Defense]*. Record heat-lot numbers and MTR
  (mill test report) URLs against inventory; for defense quotes the system
  refuses to start a work order until the heat lot is assigned — a
  chain-of-custody view shows the trail.
- **QC inspection plans** *[Defense]*. Record manual inspection results
  against a plan; the verdict math is calibration-stale-aware and grades by
  tolerance tier (1× / 2× the limit), auto-raising an NCR on the failing
  tier. The calibration-staleness window is per-tenant configurable.
- **Per-unit Part UID marking** *[Defense]*. Mint a per-unit UID and a
  DataMatrix payload for each part; the system **refuses to mark a defense
  shipment until every unit carries its UID**, with forward/reverse trace.
- **NCR / CAPA quality workflow** *[Defense]*. Non-conformance reports and
  corrective actions with a closed state machine; an open NCR **blocks the
  shipment**, and a Critical NCR escalates if not actioned within 24 hours.

**File the invoice**

- **Invoicing** *[both]*. Hungarian shops file directly to **NAV Online
  Számla 3.0** (issue, credit-note/storno, modification, with XSD
  validation and status polling). Everyone else runs **LocalOnly** — full
  invoices, no tax-office submission.

**Run the shop**

- **Master data** *[both]*. Partners, products, and machines, each with
  audited edits and an archive-don't-delete policy.
- **Multi-tenant + demo + NAV-off toggle** *[both]*. Run several companies
  from one install, switch between them, and flip NAV on or off per tenant.
  A bundled demo tenant seeds fresh installs so the first launch already
  has data to click through — this is what makes Portable boot straight
  into a populated dashboard.

**Prove what happened**

- **Audit ledger + audit screen** *[both]*. Every state change lands in a
  hash-chained, append-only ledger with an operator-visible screen (filter,
  sort, per-row hash check, whole-chain verdict). Sensitive payloads are
  redacted by default.
- **Snapshot system** *[both]*. Periodic, *validated* DuckDB snapshots
  (logical exports, smoke-tested on the way out) plus AES-256-GCM-encrypted
  CAD storage back the ledger up — a real rollback path, not a hopeful file
  copy.
- **Audit-chain DÁP / QES signing — coming soon** *[Defense]*. The
  scaffolding to anchor each ledger entry to a Hungarian government digital
  identity (DÁP eAzonosítás) and a NETLOCK qualified timestamp has landed
  on `main`, but is **not yet shippable**: the real DÁP and NETLOCK
  integrations are still pending (see [roadmap](#roadmap)).

---

## Capability status — how to read the next two sections

The house is wired for more sockets than currently have an appliance
plugged in. Both sections below list the whole wiring, each capability
carrying one of two statuses:

- **Live** — a real emitter or handler exists and is exercised end to end.
  This is a working control.
- **Designed — awaiting hardware/endpoint** — the code surface genuinely
  exists in this repo (a trait, a scaffold, a validated `EventKind`, a
  defined adapter), but it is not connected to the real device or external
  backend yet. The socket is in the wall; nothing is plugged into it.

**A Designed capability is not an operational one.** On an export-controlled
product that distinction is the whole point: a designed control described
as active is the dangerous overstatement. Nothing is listed unless its code
surface was verified in this repo — there are no aspirational entries.

Every Designed item has a matching entry in
[`docs/BACKLOG-designed-to-live.md`](docs/BACKLOG-designed-to-live.md),
which records the code surface that exists today and what is concretely
missing to drive it to Live.

A capability can also be **Live with expansion slots** — the base works and
is in use, but the seam it sits on has more capacity than is currently
drawn on. Those are tracked in the same backlog; the base stays Live.
MTConnect ([D-16](docs/BACKLOG-designed-to-live.md#d-16)) is the current
example.

---

## Audit — what is recorded, and what protects it

### The event surface

Every audited action becomes one entry with a typed JSON payload and a
namespaced `domain.event_name` kind. **187 kinds are defined**: **170 are
Live**, **17 are Designed** — defined, documented, round-trip validated,
and handled by the classifier and the verifier, but with no firing site
yet. Kind strings are stable identifiers — the audit screen, the export
bundle, and `aberp-verify` all key off them.

| Domain | What it records | Live |
|---|---|---|
| `quote.*` | The CAD → pricing → PDF pipeline: geometry extraction, pricing runs and classified failures, margin / lead-time overrides and below-floor refusals, operator accept / refuse, calibration samples and coefficient shifts, tunables (machine rates, gear processes, materials, tolerance multipliers, complexity rules), deal → sales order → work order, PDF re-render, stock alerts, storefront email-outbox claim / fetch / send / fail | 45 / 46 |
| `invoice.*` | The full NAV lifecycle: sequence reservation, draft created / deleted, staged, submission attempt / response / failure, ack polling, storno, modification, technical annulment (request → submit → ack → receiver confirmation), invoice check, payment recorded, emailed, marked abandoned, picked up from a quote, LocalOnly issuance | 22 / 22 |
| `mes.*` | Machine adapters (added / updated / removed / health transitions), adapter events off the wire, work orders and routing-op state, QA inspections, stock movements, dispatch created / shipped, machine master data | 16 / 16 |
| `system.*` | Daemon cycle outcomes and shutdown, incoming-invoice (AP) ingest and sync cycles, quote-intake poll attempts / failures / rows, restore-from-NAV runs and buyer backfill, ExtNav manual partner link, numbering-template changes, first-prod-launch acknowledgement, upgrade snapshot mismatch | 15 / 15 |
| `po.*` | Purchase orders: created, line added, issued, received / partially received, receipt recorded, incoming inspection failed, cancelled, closed | 9 / 9 |
| `tenant.*` | Tenant create / archive / restore / switch, NAV toggle, seller region + setup, demo seeding | 9 / 9 |
| `auth.*` | Audit-session lifecycle: operator and service sessions opened / closed / endorsed, crash-recovered sessions | 6 / 10 |
| `supplier.*` | AVL: vendor added, status changed, revoked, screening overdue, export screening recorded, PO blocked by vendor status | 6 / 7 |
| `qc.*` | Inspection recorded / passed / failed, auto-NCR raised, probe calibration-stale warning, probe ingestion failure | 6 / 6 |
| `ncr.*` | NCR created, state changed, escalated, closed, and work orders blocked by an open NCR | 5 / 5 |
| `capa.*` | CAPA created, approved, effectiveness reviewed, closed | 4 / 4 |
| `part.*` | Per-unit serial assigned, UID marked, traceability viewed, work order blocked for a missing UID | 4 / 4 |
| `snapshot.*` | Snapshot created, pruned, restored, validation failed | 4 / 4 |
| `material.*` | Heat lot assigned, MTR uploaded, traceability viewed, work order blocked for a missing heat lot | 4 / 5 |
| `export.*` | Export control / ITAR — see below | 3 / 3 |
| `email.*` | SMTP relay queued / sent / failed | 3 / 3 |
| `cad.*` | Encrypted-CAD key provisioning and every blob read, including legacy-plaintext reads | 3 / 3 |
| `audit.*` | Qualified-timestamp anchor taken / delayed | 2 / 2 |
| `inventory.*` | Material committed to a work order | 1 / 4 |
| `db.*` | Boot-time automatic ledger recovery | 1 / 1 |
| `partner.*` | Customer-type change (drives the margin profile) | 1 / 1 |
| `cui.*`, `personnel.*`, `incident.*` | — | 0 / 7 |

The table accounts for 186 kinds; the 187th is `test`, written only by the
chain-conformance suite.

### Designed — the 17 kinds awaiting a firing site

Each of these parses, round-trips through storage form, carries a
documented payload schema, and is handled exhaustively by the bundle
classifier and `aberp-verify` — but **no code path writes one today**. In
several cases the supporting types are built too, which is why they are
listed rather than hidden. Backlog IDs link to
[`docs/BACKLOG-designed-to-live.md`](docs/BACKLOG-designed-to-live.md).

| Kinds | Supporting code surface that exists today | Backlog |
|---|---|---|
| `cui.marking_applied`, `cui.access_event` | `aberp-compliance::cui` — `CuiMarking`, `CuiCategory`, `DisseminationControl`, `display_marking()`, `to_banner_str()` (32 CFR 2002 / DoD CUI Registry vocabulary) | [D-08](docs/BACKLOG-designed-to-live.md#d-08) |
| `personnel.id_registered`, `personnel.access_granted`, `personnel.access_denied`, `personnel.signature_applied` | The `DigitalIdProvider` trait and its two stub backends (see [D-07](docs/BACKLOG-designed-to-live.md#d-07)); no e-signature ceremony yet | [D-15](docs/BACKLOG-designed-to-live.md#d-15) |
| `incident.cyber_detected` | `aberp-compliance::incident` — `IncidentSeverity`, `DetectionSource`, and `dod_72h_report_due_at_ms()` computing the DFARS 252.204-7012 deadline | [D-09](docs/BACKLOG-designed-to-live.md#d-09) |
| `auth.dap_login_initiated` / `_completed` / `_failed` / `_fallback` | The `DapTransport` trait, a working `MockDapTransport`, and a live `POST /api/dap/mock-login` route driving it end to end | [D-05](docs/BACKLOG-designed-to-live.md#d-05) |
| `inventory.material_reserved` / `_released` / `_consumed`, `material.cert_attached` | The inventory tables and `inventory.material_committed`, which is Live; `aberp-compliance::lot_heat` validates the ids | [D-11](docs/BACKLOG-designed-to-live.md#d-11) |
| `supplier.dpas_priority_set` | `aberp-compliance::avl::DpasRating` (FAR 11.6 / 15 CFR 700), plus a live `dpas_rating` column on partners that already validates through it — the value is stored, the audit event is not written | [D-10](docs/BACKLOG-designed-to-live.md#d-10) |
| `quote.pricing_operator_accepted` | The out-of-band accept path (operator accepts by phone/email on the customer's behalf) with a documented writeback payload; the in-app `quote.operator_accepted` and `quote.priced_writeback_outcome` are Live | [D-12](docs/BACKLOG-designed-to-live.md#d-12) |

### Export control / ITAR *[Defense]*

Three kinds fire at the shipment boundary, all inside the single
`mark_shipped` transaction that flips the dispatch state — so an export
row cannot exist for a shipment that rolled back, and a shipment cannot
exist without its export rows:

- `export.classification_set` — the classification the injected
  `ExportControlProvider` returned for the commodity (`eccn`,
  `usml_category`, `jurisdiction`), stamped with the operator and a
  system clock.
- `export.access_check` — the consignee screening decision
  (`granted` / `restricted` / `denied` / `not_determined`) plus the
  `reason` and a `backend` field naming which provider answered. A
  denial rolls the ship back, so that row is appended by the route layer
  instead.
- `export.shipment_logged` — the physical export record: exporter,
  consignee, destination country, cited authorization.

The gate, the transaction atomicity, the recording, and the shipment
refusal on a blocking decision are all **Live**. The **screening backend
itself is Designed** ([D-01](docs/BACKLOG-designed-to-live.md#d-01)): the
`ExportControlProvider` trait defines `classify` and `screen_party`, and
the only implementation is `MockExportControlProvider`, selected at boot.

That distinction is enforced in the data, not just in this README. With no
denied-party list wired, the provider answers `not_determined` with
`backend: "mock"` — **never `granted`**. A `granted` row on an append-only
ledger would assert that a screen ran and cleared, and it could never be
corrected. Timestamps come from the system clock rather than the
operator-supplied ship date, so a back-dated shipment cannot claim its
screening ran in the past. Separately, `supplier.export_screened` records
AVL-side export screening of vendors, and is Live.

### What protects the chain

- **Hash chain.** `entry_hash[N] = SHA-256(canonical-CBOR(entry[N] with
  prev_hash = entry_hash[N-1]))`, anchored at a genesis hash derived from
  the tenant id. One canonical encoder, used by both the writer and the
  verifier.
- **Append-only, one writer.** All ledger writes go through the shared
  `aberp-db` handle (`Handle::with_ledger`), so appends across daemons and
  request handlers serialize instead of forking the chain.
- **Atomic with the business write.** `append_in_tx` takes a caller-owned
  transaction, so the state change and its audit entry commit or roll back
  together.
- **A JSONL mirror alongside the DuckDB table**, with a boot-time
  consistency check that replays or repairs a mirror/DB divergence and
  records the repair as `db.auto_recovered`.
- **Build-gated write-fork scanners.** The cut gate refuses a release
  whose sources grow a new ledger opener or a wrapper that appends outside
  the shared handle (`tools/adr0099_write_fork_scan.awk`,
  `tools/adr0105_wrapper_fork_scan.awk`), and
  `tools/cut_gate_negative_probes.sh` re-plants each defect to prove the
  scanner still catches it.
- **Session signing** *[Defense]*. With the per-tenant `dap_enabled`
  toggle on (**default off**), boot mints an ed25519 service session,
  recovers sessions left open by a crash, signs entries, and takes
  periodic timestamp anchors. That machinery is Live; the **timestamp
  authority behind it is Designed**
  ([D-06](docs/BACKLOG-designed-to-live.md#d-06)) — `NetlockTsa` exists
  and every method is `todo!`, so `MockTimestampAuthority` is what runs.
  Treat the `auth.*` and `audit.*` kinds as a working structural floor,
  **not a qualified signature**.

### Designed — compliance types built ahead of their surfaces

Two more sockets with no EventKind of their own yet:

- **MIL-STD-130N IUID** ([D-03](docs/BACKLOG-designed-to-live.md#d-03)).
  `aberp-compliance::uid` implements `IuidConstruct1` / `IuidConstruct2`,
  `validate_iac()`, and IRI rendering. Per-unit marking is Live today, but
  it mints a `dp-`-prefixed ULID and a DataMatrix payload — not a DoD
  IUID. Minting a real one needs an assigned enterprise identifier.
- **NIST SP 800-171 control tagging**
  ([D-04](docs/BACKLOG-designed-to-live.md#d-04)). All 110 DFARS
  252.204-7012 control identifiers exist as constants in
  `aberp-compliance::nist_800_171`, ready to tag audit events. Nothing
  consumes them yet.

### Reading and exporting it

- **Audit screen in the app** — `GET /api/audit-events` behind the SPA's
  Audit Events view. Server-side filters on date range, kind, domain
  prefix, subject (matched across own id *and* chain-base id, so a storno
  is found by the invoice it credits), operator, and free text; cursor
  pagination; storefront heartbeat kinds hidden by default. The detail
  view (`/api/audit-events/:seq`) returns the full typed payload plus a
  recomputed `hash_ok`, `prev_hash`, and `entry_hash`.
- **`aberp export-invoice-bundle`** — a single `.tar.zst` evidence archive
  for one invoice: the chain slice, the NAV request/response archive, and
  a manifest.
- **`aberp-verify`** — a separate binary that re-verifies an exported
  bundle from its bytes alone, without trusting the app that produced it,
  and reports every check it ran rather than stopping at the first
  failure.

---

## External connected workflows

Everything ABERP talks to over a wire. It has **no public inbound
surface** — the app itself is a loopback-only listener on the operator's
Mac, with no webhook and no tunnel — so every internet-facing integration
below is outbound: ABERP polls or pushes. The only sockets it accepts on
are shop-floor ones, bound on the local network for adapters that can
only push (the barcode scanner).

**NAV Online Számla 3.0** *[Defense]*

- **Outbound invoicing.** `tokenExchange` + `manageInvoice` for issue,
  storno, and modification, with NAV v3.0 XSD + invariant validation
  before the wire and `queryTransactionStatus` ack polling after it.
  `manageAnnulment` covers technical annulment, with its own ack poll and
  receiver-confirmation observation.
- **Offline queue and retry drains.** `drain-submission-queue` classifies
  invoices that were drafted but never submitted and files them FIFO,
  stopping on the first transport-layer error; `drain-pending-retries`
  drives invoices stuck between attempt and response back through the
  submission pipeline.
- **Inbound AP sync.** A background daemon polls `queryInvoiceDigest` +
  `queryInvoiceData` for invoices issued *against* you, ingests them as
  incoming-invoice rows, and tracks status changes. Tax IDs are redacted
  from the diagnostic previews it logs.
- **NAV as disaster recovery.** `restore-from-nav-outgoing` pages your own
  outgoing invoices back out of NAV to rebuild a lost local database:
  digest paging, a preview pass, a per-tenant restore lock, sequence-gap
  detection, and a checksum over the restored numbers. Restored rows land
  in a separate `restored_invoice` table surfaced as **ExtNav** rows — the
  NAV digest carries no buyer identity, so a background backfill and an
  operator-driven `system.extnav_partner_manual_link` attach the partner
  rather than the system guessing. `recover-from-nav` reconstructs a
  single invoice from `queryInvoiceData`.
- NAV credentials live in the macOS keychain (`./run/setup_nav_creds.sh`).
  A build without `--features production` structurally cannot reach the
  live endpoint.

**Storefront (abenerp.com)** *[Defense]*

Reach to the storefront is a **compile-time Defense-only** capability: in
a Portable build these daemons are never spawned, and a boot guard refuses
the reach. Local quoting is unaffected in both editions.

- **Quote intake (pull).** Polls the storefront for approved quotes and
  stages them in a purpose-built intake table together with a
  pre-prepared draft invoice. It deliberately never touches the `invoice`
  table — the operator picks a row up in the SPA and it goes through the
  normal issue pipeline, so the sequence burn, the audit chain, and the
  NAV submission stay operator-gated.
- **Catalogue push.** `PUT`s the public projection of the material
  catalogue to the storefront on a cadence and on every operator write, so
  the public quote form's dropdown is fed without the customer's browser
  ever reaching ABERP.
- **Email outbox (pull-then-send).** Polls the storefront's internal email
  queue, claims each entry, sends it through ABERP's own SMTP, then posts
  the sent/failed result back — the single-point-of-contact posture, where
  ABERP is the only thing holding mail credentials.

**SMTP** *[both]*

A local `lettre` transport over rustls, used for both invoice delivery and
the storefront outbox. Outbound mail is queued in a DuckDB table and
drained by a background daemon with a bounded retry budget; every
transition is audited (`email.relay_queued` / `_sent` / `_failed`).
Credentials are entered in Tenant Settings; the password lives in the OS
keychain. TLS is mandatory — the transport-security setting is a closed
vocabulary of `StartTls` or `Tls`, with no plaintext variant an operator
could type.

**Machine and shop-floor adapters — Live** *[Defense]*

Operators register adapters (host, port, device) in the app; the MES
manager builds and supervises them, and every event and health transition
is written to the ledger by a dedicated writer task. Five adapter families
are wired to real transports:

| Adapter | Transport | Notes |
|---|---|---|
| Barcode scanner | TCP listener, line-delimited UTF-8 | Bounded payload length and concurrent connections |
| Label printer (Zebra) | Raw TCP ZPL, TCP-connect health probe | Per-print connection, auto-reconnect |
| CNC (MTConnect) | HTTP `GET /{device}/current` polling | Bounded response size, classified transport errors |
| Robot (Universal Robots) | RTDE over TCP, version handshake | Reconnect with exponential backoff |
| Laser (Trumpf) | MTConnect agent / gateway | Backend is a code decision, not an operator field |

**MTConnect is the load-bearing seam, and it has room to grow**
([D-16](docs/BACKLOG-designed-to-live.md#d-16)). One open standard already
serves two machine families here — the CNC adapter and the Trumpf laser
share the same polling and parsing code — which is exactly why it was
chosen over N vendor SDKs. The base transport is Live and stays Live; what
is tracked as a development socket is the capacity beyond it. Today the
parser already extracts six data items per poll but only `Execution` drives
an event, and the adapter polls `/current` snapshots without touching
`/sample`, `/probe`, or `/assets`. Each of those is a concrete slot with
existing code behind it, itemised in the backlog.

### Designed — awaiting hardware or endpoint

These have a real code surface in this repo — a trait, an implementing
type, or a scaffold that compiles into the binary — but nothing is plugged
into them yet. None can be reached from operator configuration; the
unimplemented backends return an error or a documented `todo!` rather than
pretending to work. Each has a backlog entry in
[`docs/BACKLOG-designed-to-live.md`](docs/BACKLOG-designed-to-live.md).

| Capability | Code surface today | Missing to reach Live | Backlog |
|---|---|---|---|
| Denied-party / export screening backend | `ExportControlProvider` trait (`classify` + `screen_party`); `MockExportControlProvider` is the only impl, chosen at boot | A real screening service (e.g. the US Consolidated Screening List / BIS) behind the trait | [D-01](docs/BACKLOG-designed-to-live.md#d-01) |
| On-machine QC probe ingestion | `ProbeIngestionSource` trait + `ProbeCursor` / `RawProbeEvent`; a working `MockProbeSource`; `MtconnectProbeSource` and `RenishawCentralSource` both `todo!`. `qc.probe_ingestion_failed` is already a Live emitter | A DMG MORI MTConnect probe endpoint and a Renishaw Central deployment to read against | [D-02](docs/BACKLOG-designed-to-live.md#d-02) |
| Laser — OPC UA backend | `OpcUaLaserSource` implements the `TrumpfSource` trait; returns an error, never constructed by `build_adapter` | An OPC-UA client dependency plus an address-space capture from the target machine | [D-13](docs/BACKLOG-designed-to-live.md#d-13) |
| Laser — Oseon / TruTops Fab backend | `OseonLaserSource` implements `TrumpfSource`; same non-panicking posture | A licensed Oseon deployment to design against — this is where job-level linkage lives | [D-14](docs/BACKLOG-designed-to-live.md#d-14) |
| DÁP eAzonosítás operator login | `DapTransport` trait; `MockDapTransport` driven end to end by a live `POST /api/dap/mock-login`; the OIDC transport is `todo!` | szeusz.gov.hu relying-party credentials and spec access | [D-05](docs/BACKLOG-designed-to-live.md#d-05) |
| NETLOCK qualified timestamp | `NetlockTsa` compiles into the binary, every method `todo!`; `MockTimestampAuthority` runs in its place | NETLOCK account onboarding, then swapping the authority at the anchor site | [D-06](docs/BACKLOG-designed-to-live.md#d-06) |
| CAC / PIV operator identity | `DigitalIdProvider` trait with two selectable backends — `MockProvider` and `UsDodCacProvider`, a card-session stub that WARNs on construction and makes `current_operator()` genuinely fallible | A real card reader (PKCS#11) and DoD PKI chain validation instead of the stub's chain-membership check | [D-07](docs/BACKLOG-designed-to-live.md#d-07) |

QC inspection results are entered by hand today; the probe sources above
are what would feed them automatically.

---

## Why this is interesting

A few things under the hood that engineers tend to enjoy:

- **A hash-chained, immutable audit trail.** Every change is an
  append-only ledger entry chained to the one before it, so tampering is
  detectable from the bytes alone. `aberp-verify` re-checks an exported
  evidence bundle without trusting the running app.
- **One binary, no infrastructure.** A Rust backend with a Tauri 2 +
  Svelte 5 desktop shell, running in-process. No containers, no database
  server, no cloud — it launches like any other Mac app.
- **DuckDB for storage.** The embedded analytical database means
  finance-style aggregate queries (revenue, VAT, aging, cashflow) run
  against your live data without a separate warehouse.
- **Encrypted CAD at rest.** Uploaded CAD blobs are AES-256-GCM encrypted,
  with a read-audit trail and decrypt-to-temp handling for the extractor.
- **Corruption-recovery built in.** Periodic, *validated* DuckDB snapshots
  (logical exports, smoke-tested on the way out) give a real rollback path
  — not a hopeful file copy.

---

## Status

- **Current Portable stable: `PROD_Portable_v0.1.2`** (cut 2026-06-16) —
  the edition the Quick Start above installs. Dev-profile build, NAV off,
  demo tenant seeded. `./run/upgrade_portable.sh PROD_Portable_v0.1.2`.
- **Current Defense stable: `PROD_Defense_v0.2.1`** (cut 2026-06-16) — the
  HU-production build with live NAV plus the defense/aerospace compliance
  stack (AVL, purchasing, heat/lot, part UID, NCR/CAPA, QC inspection).
  `./run/upgrade_defense.sh PROD_Defense_v0.2.1`.
- **Legacy unified `PROD_v2.27.76` — frozen.** The last release before the
  Portable / Defense split. Still installable via
  `./run/upgrade_prod.sh PROD_v2.27.76` for existing operators (see the
  [runbook](docs/CUTOVER_RUNBOOK.md)), but no longer the path forward — new
  releases ship on the two lines above.

The test NAV path is the default for any build that does not pass
`--features production`; the production NAV endpoint is structurally
unreachable from a non-production build. That is exactly why Portable is
safe to hand to anyone.

---

## Defense (HU production) install

The complete procedure — first-time prod branch, `seller.toml` template,
NAV + SMTP credentials, smoke-invoice checklist, rollback, and the ongoing
update workflow — lives in:

→ **[`docs/CUTOVER_RUNBOOK.md`](docs/CUTOVER_RUNBOOK.md)**

Short version, on the prod machine:

```bash
git clone --branch PROD_Defense_v0.2.1 https://github.com/Cservin69/ABERP.git ABERP-Defense
cd ABERP-Defense
./run/run_defense.sh   # builds with --features production, launches the shell
```

To upgrade an existing Defense install, snapshot first (DuckDB storage
upgrades are one-way), then:

```bash
git fetch origin && git reset --hard origin/PROD_Defense_v0.2.1 && \
  ./run/upgrade_defense.sh PROD_Defense_v0.2.1
```

The versioning rules (when to bump patch vs minor vs major) are pinned in
[`adr/0056-versioning-policy.md`](adr/0056-versioning-policy.md).

---

## Roadmap

Honest about what isn't built yet. The **Designed — awaiting
hardware/endpoint** capabilities above each have a tracked entry in
[`docs/BACKLOG-designed-to-live.md`](docs/BACKLOG-designed-to-live.md),
which is the working list for driving them to Live; this section is the
wider view, including work with no code surface yet.

- **Real DÁP / QES audit-chain signing (HU)** — the structural floor has
  landed: traits for the DÁP transport and a timestamp authority, an
  ed25519 session key, three signature columns on the ledger, and a
  per-tenant `dap_enabled` toggle (default off). What is still mocked: the
  real **DÁP eAzonosítás** operator-identity flow and the **NETLOCK
  qualified-timestamp** integration. Until those are wired, the chain
  signs with mocks and is not shippable as a compliance feature.
- **On-machine probe ingestion (real machine)** — the QC inspection
  workflow ships today with manual result entry; the **DMG MORI** (MTConnect)
  and **Renishaw** probe sources that would feed inspection values
  automatically are designed and stubbed, not yet talking to real hardware.
- **International tax modules** — Portable currently stores foreign tax
  numbers as opaque strings. Country-specific tax/e-invoicing modules are
  future work.
- **Linux / Windows** — macOS only today.

---

## Contributing

The repo lives at **<https://github.com/Cservin69/ABERP>**. Bug reports and
PRs are welcome — open an issue with a minimal repro. This is a
single-maintainer project, so there is no SLA, and unsolicited large
rewrites are unlikely to land.

Be aware the bar for a green build is high — every change runs through:

- `cargo fmt` (no diffs) and `cargo clippy` (zero warnings)
- `cargo test --workspace` — the full Rust suite, including the real-Python
  CAD smoke tests
- `vitest` and `svelte-check` for the SPA

The non-negotiable working principles (think before coding, simplicity
first, surgical changes, fail loud, …) are in [`CLAUDE.md`](CLAUDE.md). PRs
that ignore them get sent back.

---

## Project structure

```
ABERP/
  README.md            ← you are here
  LICENSE              ← PolyForm Noncommercial 1.0.0
  FOUNDATION.md        ← architectural spine — every ADR must be consistent with it
  CLAUDE.md            ← project-wide working agreement
  Cargo.toml           ← workspace manifest, pinned deps
  adr/                 ← Architecture Decision Records, numbered + indexed
  docs/
    CUTOVER_RUNBOOK.md ← prod cutover + update workflow (the source of truth)
    BACKLOG-designed-to-live.md ← the Designed → Live capability backlog
    threat-model.md
  crates/              ← audit-ledger, nav-transport, quote-engine, inventory,
                         work-orders, qa, dispatch, mes, compliance, digital-id, …
  modules/billing/     ← NAV invoice issuing (ADR-0009)
  apps/
    aberp/             ← the Rust backend (HTTPS+JSON localhost service)
    aberp-ui/          ← Tauri 2 shell + Svelte 5 SPA (ADR-0004)
  run/                 ← launcher scripts (run_portable / upgrade_portable /
                         run_defense / upgrade_defense / run_prod /
                         upgrade_prod / release)
  tools/               ← operational scripts (snapshot, icons)
```

---

## License

ABERP is licensed under the **PolyForm Noncommercial License 1.0.0**. In
plain terms: free to use, run, modify, and share for any non-commercial
purpose; commercial use requires a separate arrangement with the
maintainer. The full text is in [`LICENSE`](LICENSE), and the canonical
terms are at <https://polyformproject.org/licenses/noncommercial/1.0.0>.

> *Required Notice: Copyright 2026 Ervin Aben*

---

## Credits & contact

Built in Hungary by Ervin Aben. Issues and pull requests:
**<https://github.com/Cservin69/ABERP>**.

> **Hungarian invoicing law is the operator's responsibility.** When NAV
> submission is on, ABERP files per the v3.0 spec — but the operator is the
> legally responsible party for the content of their invoices. ABERP is a
> tool; compliance is yours.

---

## Operator runbook — hülye-biztos cookbook

Field-tested commands, written against the legacy `run_prod.sh` /
`upgrade_prod.sh` launcher names with a `<VERSION>` placeholder. Swap for
your edition:

- **Portable** — `*_portable.sh` and a `PROD_Portable_v*` tag
  (`PROD_Portable_v0.1.2` is current).
- **Defense** — `*_defense.sh` and a `PROD_Defense_v*` tag
  (`PROD_Defense_v0.2.1` is current).

### 1. Upgrade to a new release (Frissítés új verzióra)

Kills running aberp, syncs to the release branch, snapshots, swaps the
binary, launches.

```bash
cd ~/ABERP && \
pgrep -f aberp | xargs -r kill 2>/dev/null; sleep 2; \
pgrep -f aberp | xargs -r kill -9 2>/dev/null; \
git fetch origin && git reset --hard origin/<VERSION> && \
./run/upgrade_prod.sh <VERSION>
```

### 2. Just relaunch (Újraindítás verzióváltás nélkül)

After a Ctrl-C or shutdown, when nothing changed and you want the app back up.

```bash
cd ~/ABERP && \
pgrep -f aberp | xargs -r kill 2>/dev/null; sleep 2; \
pgrep -f aberp | xargs -r kill -9 2>/dev/null; \
./run/run_prod.sh
```

### 3. Kill stuck aberp processes (Lefagyott aberp folyamatok kilövése)

When graceful shutdown didn't drain everything.

```bash
pgrep -f aberp | xargs -r kill 2>/dev/null; sleep 2; \
pgrep -f aberp | xargs -r kill -9 2>/dev/null
```

### 4. Emergency bypass — launch with a dirty tree (Vészhelyzeti megkerülés)

For dev workflows or when you've verified state by hand and know the git
check is a false positive. NEVER for casual prod use.

```bash
cd ~/ABERP && ABERP_SKIP_GIT_CHECK=1 ./run/run_prod.sh
```

### 5. Verify remote branch + tag SHAs before resetting (Távoli állapot ellenőrzése)

Sanity-check before any `git reset --hard origin/<VERSION>`.

```bash
git ls-remote https://github.com/Cservin69/ABERP.git \
  refs/heads/main refs/heads/PROD_Defense_v0.2.1 \
  refs/tags/PROD_Defense_v0.2.1
```

### 6. DuckDB snapshot / restore — the panic button (DuckDB pillanatkép)

Snapshots **just the tenant DuckDB** (binary-validated via
`PRAGMA verify_external_invariants`) to `~/Documents/ABERP-snapshots/` —
outside the repo and outside `~/.aberp/`. **Take one before every upgrade**,
especially across a one-way DuckDB storage bump. Best run with the app
stopped. `--db` defaults to `./aberp.duckdb`, so always pass the real path.

```bash
cd ~/ABERP
# Take a snapshot
cargo run -p aberp --release --bin aberp -- \
  snapshot --tenant prod --db ~/.aberp/prod/aberp.duckdb
# ... if an upgrade goes sideways, stop the app, then restore:
pgrep -f aberp | xargs -r kill -9 2>/dev/null
ls -lt ~/Documents/ABERP-snapshots/prod-*.duckdb | head -3
cargo run -p aberp --release --bin aberp -- restore-snapshot \
  --tenant prod --db ~/.aberp/prod/aberp.duckdb \
  --from ~/Documents/ABERP-snapshots/prod-TIMESTAMP.duckdb
```

`restore-snapshot` refuses while a server still holds the DB lock, and
refuses a backup that fails its own validity check — so it never clobbers a
working DB with a broken one.

### 7. Set up NAV creds + SMTP on a fresh box (Új gépen alapbeállítás)

For the **Defense (HU production)** edition, after cloning and before the
first prod launch. (Portable needs none of this — NAV is off.)

```bash
cd ~/ABERP && ./run/setup_nav_creds.sh
# Then in Tenant Settings → SMTP → enter the SMTP password
# Then in Tenant Settings → Quote Intake (if enabled) → bearer token
```

### Forensics

- Audit ledger: `~/.aberp/<tenant>/audit-ledger.duckdb` + JSONL mirror
- DuckDB: `~/.aberp/<tenant>/aberp.duckdb`
- Seller config: `~/.aberp/<tenant>/seller.toml`
- Snapshots: `~/Documents/ABERP-snapshots/` (DuckDB) and
  `~/aberp-snapshots/` (encrypted tenant tarballs)
- Logs (Tauri): `~/Library/Logs/aberp/`

---

## Branding (optional)

- **Printed invoice:** drop a PNG at `~/.aberp/<tenant>/logo.png` (≤ 512×512,
  aspect preserved, fit into a 50×50-point box top-left). A malformed PNG
  loud-fails the render rather than shipping a logo-less PDF silently.
- **App header:** drop a PNG at `apps/aberp-ui/ui/static/aberp-logo.png`
  *before* building; the topbar wordmark swaps from text to your image. The
  directory is gitignored, so your asset stays private.

Both are pure filesystem convention — no config knob, no DB column.
Absent file → text-only header.

---

## Further reading

1. [`FOUNDATION.md`](FOUNDATION.md) — the architectural spine.
2. [`adr/README.md`](adr/README.md) — how ADRs work; numbered, in order.
3. [`docs/CUTOVER_RUNBOOK.md`](docs/CUTOVER_RUNBOOK.md) — the prod cutover +
   update procedure.
4. [`docs/BACKLOG-designed-to-live.md`](docs/BACKLOG-designed-to-live.md) —
   every Designed capability, the code surface behind it, and what is
   missing to drive it to Live.
</content>
</invoke>
