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

| | **Portable** | **Full (HU production)** |
|---|---|---|
| For | Anyone, anywhere — evaluating, or running outside Hungary | Hungarian manufacturing shops, especially defense / aerospace |
| Tax filing | **Off by default** — invoices stay local (LocalOnly) | Live NAV Online Számla 3.0 e-invoicing |
| First boot | Demo company pre-seeded — data to explore immediately | Your own seller profile + NAV credentials |
| Build | Dev profile — structurally cannot reach the live NAV endpoint | `--features production` — the real-money build |
| Install | `./run/upgrade_portable.sh` | `./run/upgrade_prod.sh` (see [runbook](docs/CUTOVER_RUNBOOK.md)) |

**Portable** is the path most newcomers want. It is the same application —
quoting, manufacturing, the audit ledger, all of it — with the Hungarian
NAV submission turned off per tenant. You can enter tax numbers for your
own country (they are stored as opaque strings for now; country-specific
tax modules are on the [roadmap](#roadmap)).

**Full (HU production)** adds live NAV Online Számla 3.0 invoicing plus the
defense/aerospace capabilities: approved-vendor screening, lot/heat
material traceability, and the production build that talks to the real NAV
endpoint. It is what Hungarian shops run for real money.

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

## What it does

Organized the way an operator actually works:

- **Quoting (CAD-aware).** Drop in an STL or STEP file → it extracts the
  geometry → estimates machining time → applies the margin profile for that
  customer type → shows a lead-time chip (green / yellow / red) → renders a
  customer-ready PDF. Quotes that would price below the margin floor are
  refused outright, not silently shipped.
- **Invoicing.** Hungarian shops file directly to **NAV Online Számla 3.0**
  (issue, credit-note/storno, modification, with XSD validation and status
  polling). Everyone else runs **LocalOnly** — full invoices, no tax-office
  submission.
- **Master data.** Partners, products, and machines, each with audited
  edits and an archive-don't-delete policy.
- **Approved Vendor List.** Vendor CRUD with screening and approval
  categories (ITAR, EAR99, Aerospace, Defense, Nuclear), plus a
  purchase-order eligibility gate so unscreened vendors can't slip through.
- **Material traceability.** Record heat-lot numbers and MTR (mill test
  report) URLs against inventory; for defense quotes the system refuses to
  start a work order until the heat lot is assigned — a chain-of-custody
  view shows the trail.
- **Audit ledger.** Every state change lands in a hash-chained, append-only
  ledger with an operator-visible screen (filter, sort, per-row hash check,
  whole-chain verdict). Sensitive payloads are redacted by default. A
  snapshot system and AES-256-GCM-encrypted CAD storage back it up.
- **Multi-tenant.** Run several companies from one install, switch between
  them, toggle NAV per tenant. A bundled demo tenant seeds fresh installs.

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

- **Current Portable stable: `PROD_Portable_v0.1.2`** — the edition the
  Quick Start above installs. Dev-profile build, NAV off, demo tenant
  seeded. `./run/upgrade_portable.sh PROD_Portable_v0.1.2`.
- **Current HU production stable: `PROD_v2.27.76`** — the full build with
  live NAV and the defense/aerospace capabilities, in production with real
  money and live filings flowing through it.
  `./run/upgrade_prod.sh PROD_v2.27.76` (see the
  [runbook](docs/CUTOVER_RUNBOOK.md)).

The test NAV path is the default for any build that does not pass
`--features production`; the production NAV endpoint is structurally
unreachable from a non-production build. That is exactly why Portable is
safe to hand to anyone.

---

## Full (HU production) install

The complete procedure — first-time prod branch, `seller.toml` template,
NAV + SMTP credentials, smoke-invoice checklist, rollback, and the ongoing
update workflow — lives in:

→ **[`docs/CUTOVER_RUNBOOK.md`](docs/CUTOVER_RUNBOOK.md)**

Short version, on the prod machine:

```bash
git clone --branch PROD_v2.27.76 https://github.com/Cservin69/ABERP.git ABERP-prod
cd ABERP-prod
./run/run_prod.sh   # builds with --features production, launches the shell
```

To upgrade an existing prod install, snapshot first (DuckDB storage
upgrades are one-way), then:

```bash
git fetch origin && git reset --hard origin/PROD_v2.27.76 && \
  ./run/upgrade_prod.sh PROD_v2.27.76
```

The versioning rules (when to bump patch vs minor vs major) are pinned in
[`adr/0056-versioning-policy.md`](adr/0056-versioning-policy.md).

---

## Roadmap

Honest about what isn't built yet:

- **Defense DÁP integration (HU)** — operator identity backed by the
  Hungarian government digital ID (DÁP) and qualified electronic
  signatures. The `aberp-digital-id` crate scaffolds a pluggable signer; a
  real provider is not yet wired.
- **Part UID / serial producers** — per-part serial-number generation for
  full unit-level traceability.
- **Full procurement** — vendor purchase orders and receiving. Accounts
  payable today mirrors supplier invoices and marks them; PO-matching and
  payment workflows are not built.
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
    threat-model.md
  crates/              ← audit-ledger, nav-transport, quote-engine, inventory,
                         work-orders, qa, dispatch, mes, compliance, digital-id, …
  modules/billing/     ← NAV invoice issuing (ADR-0009)
  apps/
    aberp/             ← the Rust backend (HTTPS+JSON localhost service)
    aberp-ui/          ← Tauri 2 shell + Svelte 5 SPA (ADR-0004)
  run/                 ← launcher scripts (run_portable / upgrade_portable /
                         run_prod / upgrade_prod / release)
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

Field-tested commands. Copy whichever recipe you need. For the Portable
edition, swap `upgrade_prod.sh` → `upgrade_portable.sh`, `run_prod.sh` →
`run_portable.sh`, and `<VERSION>` → a `PROD_Portable_v*` tag.

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
  refs/heads/main refs/heads/PROD_v2.27.76 \
  refs/tags/PROD_v2.27.76
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

For the **Full (HU production)** edition, after cloning and before the
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
</content>
</invoke>
