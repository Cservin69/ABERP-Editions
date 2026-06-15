# ABERP

A small-business ERP built around **Hungarian NAV Online Számla v3.0**
invoicing, now spanning the full shop-floor loop: auto-quoting →
manufacturing (work orders, QA, dispatch) → invoicing → accounts
payable → financial statistics. Rust backend, Tauri 2 + Svelte 5
desktop UI, append-only hash-chained audit ledger. Runs locally on the
operator's own machine; no SaaS dependency. Single-maintainer,
non-commercial, open-source.

> **License — PolyForm Noncommercial 1.0.0.** ABERP is free for
> non-commercial use. See [`LICENSE`](LICENSE) for the full terms. If
> you want to use it commercially, contact the maintainer.

> **Hungarian invoicing law is the operator's responsibility.** ABERP
> submits to NAV per the v3.0 spec, but the operator is the legally
> responsible party for the content of their invoices. ABERP is a tool;
> compliance is yours.

## Status

**Current stable: `PROD_v2.27.69`** (cut 2026-06-15). In production —
real money and live NAV submissions flow through it. The test path
remains the default for any build that does not pass
`--features production`; the production NAV endpoint is structurally
unreachable from a non-production build.

**What changed in `PROD_v2.27.69`:** operator audit screen —
`/api/audit-events` + AuditEvents SPA route with redaction defaults,
show-raw confirmation, hash-chain checklist viz. Upgrade straight to
`PROD_v2.27.69`.

**What's new since the previous milestone:** the full NAV credit-note
(storno) chain is now end-to-end NAV-accepted, closing a long-standing
storno blocker. This release lands an XSD-conformance sweep on storno
and modification XML (NAV rules F1–F4), atomic render-before-commit
invoice writes (the PDF + audit entry land in the same transaction or
not at all), a manual `aberp snapshot` / `restore-snapshot` panic-button
CLI, and a `queryInvoiceCheck` pre-flight that skips invoice numbers the
NAV test endpoint already claims from a prior cycle. Supporting fixes:
the storefront-writeback CSRF/Origin-header fix, a NAV submission
de-duplication gate (closes a double-submit TOCTOU race), and a
cross-process file lock at every NAV POST site.

> **DuckDB 1.5.3 storage upgrade.** This release bumps the embedded
> DuckDB to 1.5.3. The on-disk storage format upgrade from 1.5.2 → 1.5.3
> is **one-way** — once the new binary opens a tenant DB it cannot be
> reopened by an older build. **Snapshot before upgrading** (see
> [Updating an existing prod install](#updating-an-existing-prod-install)).

### Upgrade an existing install to `PROD_v2.27.69`

```bash
cd ~/ABERP
# 1. Snapshot first — the 1.5.2 → 1.5.3 storage upgrade is one-way.
#    --db must point at the real tenant DB; the flag defaults to ./aberp.duckdb.
cargo run -p aberp --release --bin aberp -- \
  snapshot --tenant prod --db ~/.aberp/prod/aberp.duckdb
# 2. Switch to the release and launch.
git fetch origin && git reset --hard origin/PROD_v2.27.50 && \
  ./run/upgrade_prod.sh PROD_v2.27.50
```

`upgrade_prod.sh` also takes its own pre-swap tenant snapshot, but taking
your own with `aberp snapshot` first (binary-validated via
`PRAGMA verify_external_invariants`) is the belt-and-braces move before a
one-way storage migration. If an upgrade goes sideways, restore with
`aberp restore-snapshot` (refuses while a server holds the DB lock).

## Modules & capabilities

What a fresh install actually gives you, as shipped in `PROD_v2.27.50`:

- **NAV invoicing core** — issue, credit-note (storno), and modification
  invoices against NAV Online Számla v3.0, with runtime `<InvoiceData>`
  XSD invariant checking, technical annulment, async status polling, and
  a NAV-as-disaster-recovery restore wizard. MNB exchange-rate lookup for
  foreign-currency invoices.
- **Auto-quoting pipeline** — pulls approved quotes from a sister
  storefront, runs CAD feature extraction (a sandboxed Python extractor
  behind a Rust wrapper), prices them with a pure-function quote engine,
  renders an indicative quote PDF, and writes the price back. Pricing-jobs
  list + per-job detail panel in the SPA; operator material-grade override
  and accept-on-behalf; a maintainable material catalogue with complexity,
  tolerance, parameter, and stock-adjustment tunables.
- **Manufacturing (shop floor)** — append-only inventory ledger, work
  orders with 1-level BOM and linear routings (consume-on-release,
  produce-on-complete), a QA inspection queue, and a dispatch board that
  spawns the Stage-1 invoice draft on ship. A wall-TV "workshop" density
  dashboard (with a self-contained demo mode for tours).
- **MES adapters** — an adapter framework plus barcode-scanner, Zebra,
  MTConnect, and UR-RTDE adapters, each wired into the binary and gated by
  `ABERP_<TYPE>_ENABLED` env flags (single-instance per type). Configured
  from the Adapters screen; live adapters surface on the workshop dashboard.
- **Accounts payable (incoming invoices)** — mirrors supplier invoices
  from NAV (`queryInvoiceDigest INBOUND`) on a sync daemon, with a
  three-state operator workflow (Outstanding / Paid / Irrelevant). v1 is
  mirror + mark; approval/payment/PO-matching are not built yet.
- **Financial statistics** — a read-only finance dashboard aggregating
  outgoing invoices, AP, and NAV-mirror rows into revenue / VAT / AR-AP /
  aging / cashflow with period and date-basis selectors.
- **Audit & evidence** — every state change lands in a tamper-evident,
  hash-chained append-only ledger; `aberp-verify` re-verifies a
  per-invoice export bundle from bytes alone; the `aberp snapshot` /
  `restore-snapshot` CLI is the DB-rollback panic button.
- **Email** — quote and invoice email out via an SMTP outbox with a
  storefront email-relay queue.

> **Defense / compliance scaffolding is foundation-only.** The
> `aberp-compliance` crate carries trait definitions, mock backends, and
> reserved audit event-kinds (export control, CUI marking, material
> traceability, AVL/DPAS, NIST 800-171), but no operator-facing
> compliance workflow fires them. `aberp-digital-id` boots a mock signer
> only (a real provider such as DoD CAC is scaffolded, not wired). Do not
> treat any of this as an available feature.

## Prerequisites

- **Rust toolchain** — stable channel (currently 1.88+). `rust-toolchain.toml`
  pins the channel, so `rustup` resolves the right version on first build.
- **Node.js 20+** with **npm** — package-lock.json is the lockfile; do
  not switch to pnpm/yarn without converting it.
- **macOS** — shipped binaries target macOS only at this stage. Linux
  and Windows are not currently supported (the Tauri shell and the
  keychain integration would need per-OS work).
- **`iconutil`** — preinstalled on macOS; required for icon generation.

No system-wide installs beyond those. Build artifacts land under
`target/` and `apps/aberp-ui/ui/dist/`; runtime data lives under
`~/.aberp/<tenant>/`.

## Dev quickstart

From a fresh clone on macOS:

```bash
git clone <this-repo-url> ABERP
cd ABERP

# 1. Build the Rust workspace (downloads + compiles deps; one-time).
cargo build

# 2. Build the Svelte SPA bundle (Tauri's webview loads this in dev too).
cd apps/aberp-ui/ui
npm install
npm run build
cd -

# 3. Launch the desktop app (Tauri 2 dev loop: tauri-CLI spawns Vite
#    AND the Rust shell in one process group, hot-reload enabled).
./run/run_desktop.sh
```

The dev build talks to the NAV **test** endpoint
(`api-test.onlineszamla.nav.gov.hu`); invoice numbers are prefixed
`TEST-`. The production endpoint is structurally unreachable from a
non-production build.

Local data — seller profile, NAV credentials, SMTP password, DuckDB,
issued invoices, audit ledger — lives under `~/.aberp/<tenant>/`
(default tenant: `dev`).

## Production install

Full procedure with the first-time prod branch creation,
seller.toml template, NAV + SMTP credential setup, smoke-invoice
checklist, rollback, and ongoing update workflow:

→ **[`docs/CUTOVER_RUNBOOK.md`](docs/CUTOVER_RUNBOOK.md)**

Short version: each production release is a branch on origin named
`PROD_vMAJOR.MINOR` or `PROD_vMAJOR.MINOR.PATCH`. On the prod machine:

```bash
git clone --branch PROD_v2.27.50 <origin-url> ABERP-prod
cd ABERP-prod
./run/run_prod.sh   # builds with --features production, launches the shell
```

`./run/release.sh PROD_v2.27.50` is the dev-side script that publishes a
release branch from `main`.

The patch-vs-minor-vs-major rules (when to bump which segment, what
counts as a "module" for the 2.0 trigger) are pinned in
[`adr/0056-versioning-policy.md`](adr/0056-versioning-policy.md).

## Branding the printed invoice (optional)

Drop your logo at `~/.aberp/<tenant>/logo.png` to brand the printed
invoice header. PNG only for v1; ≤ 512×512 recommended; the renderer
preserves the aspect ratio and fits the image inside a 50×50-point box
top-left of the header (no operator config). Absent file → text-only
header, same as pre-PR-176. A malformed PNG loud-fails the render so
the operator sees the broken state rather than shipping a logo-less
PDF silently.

No `seller.toml` knob, no UI upload yet, no DB column — pure
filesystem convention. Re-export a different logo at the same path to
switch.

## Branding the SPA header (optional)

Drop your logo at `apps/aberp-ui/ui/static/aberp-logo.png` *before*
running `vite build` (or `cargo build --release --features production
--bin aberp-ui`, which embeds the built SPA). Vite serves the file at
`/aberp-logo.png`; the topbar wordmark swaps from the text "ABERP" to
the image automatically. Sized at `height: 32px; width: auto` —
~200×144 (the original mark) renders at ~44×32. Absent file → text-only
wordmark, same as pre-PR-188.

Convention only. The directory is tracked via a `.gitignore` that
ignores everything but itself; the operator's branding asset is private
and never lands in git. To override on a per-build basis, copy your
file in and rebuild.

## Updating an existing prod install

→ **[`docs/CUTOVER_RUNBOOK.md` § Step 9](docs/CUTOVER_RUNBOOK.md)**

The canonical one-liner is in the [Status](#upgrade-an-existing-install-to-prod_v22750)
section above:
`git fetch origin && git reset --hard origin/<VERSION> && ./run/upgrade_prod.sh <VERSION>`.

Two layers of safety net apply before any branch switch:

1. **Snapshot the tenant DB first.** Run
   `aberp snapshot --tenant prod --db ~/.aberp/prod/aberp.duckdb`
   (writes a binary-validated copy to `~/Documents/ABERP-snapshots/`,
   outside the repo and outside `~/.aberp/`; `--db` defaults to
   `./aberp.duckdb`, so pass the real path). This is the rollback path
   for the **one-way** DuckDB 1.5.x storage upgrades — once a newer build
   opens the DB, an older build can't. Restore with
   `aberp restore-snapshot` if needed.
2. **The seller-config drift guard.** `./tools/snapshot-prod.sh` (run by
   `upgrade_prod.sh`) tarballs `~/.aberp/<tenant>/`, encrypts the keychain
   entries, AND drops `~/.aberp/<tenant>/.upgrade-snapshot.toml` — a small
   contract file the next boot of the new binary compares against the
   post-upgrade `seller.toml`. The binary REFUSES to start if
   `[seller.smtp]` or `[seller.numbering]` drifted, so you don't need to
   remember to verify them manually.

## Project structure

```
ABERP/
  README.md            ← you are here
  LICENSE              ← PolyForm Noncommercial 1.0.0
  FOUNDATION.md        ← architectural spine — every ADR must be consistent with it
  CLAUDE.md            ← project-wide working agreement
  Cargo.toml           ← workspace manifest, pinned deps
  rust-toolchain.toml  ← channel = stable
  adr/                 ← Architecture Decision Records, numbered + indexed
  docs/
    CUTOVER_RUNBOOK.md ← prod cutover + update workflow (the source of truth)
    threat-model.md
    research/          ← raw research notes (NAV / Billingo / stack baseline)
    reviews/           ← adversarial review records
  crates/
    audit-ledger/      ← tamper-evident append-only ledger (ADR-0008)
    nav-transport/     ← NAV TLS transport + credentials (ADR-0009 §4, ADR-0020)
    nav-xsd-validator/ ← <InvoiceData> v3.0 runtime invariant check (ADR-0022)
    invoice-pdf/       ← printed-invoice PDF renderer
    mnb-rates/         ← MNB exchange-rate fetcher (foreign-currency invoices)
    aberp-verify/      ← external-auditor evidence-bundle verifier
    aberp-quote-intake/ ← sister-storefront quote-poll daemon (S210 — Stage 2 entry)
    aberp-quote-engine/ ← pure-function quote scoring (auto-quoting)
    aberp-quote-pdf/   ← indicative-quote PDF renderer
    aberp-cad-extract-wrapper/ ← Rust shim around the sandboxed Python CAD extractor
    aberp-inventory/   ← append-only stock-movement ledger + balance cache
    aberp-work-orders/ ← work orders + 1-level BOM + routings
    aberp-qa/          ← QA inspection queue
    aberp-dispatch/    ← dispatch board (ships goods, spawns invoice draft)
    aberp-mes/         ← MES adapter framework (barcode / Zebra / MTConnect / UR-RTDE)
    aberp-digital-id/  ← pluggable audit-entry signature provider (foundation)
    aberp-compliance/  ← export/CUI/traceability scaffolding — foundation only, dormant
  modules/
    billing/           ← NAV invoice issuing (ADR-0009)
  apps/
    aberp/             ← the Rust backend (HTTPS+JSON localhost service)
    aberp-ui/          ← Tauri 2 shell + Svelte 5 SPA (ADR-0004)
  run/                 ← launcher scripts (dev / prod / release)
  tools/               ← operational scripts (snapshot-prod.sh, icons)
```

## Contributing

This is a single-maintainer project; there is no formal support
guarantee, SLA, or roadmap for external feature requests. If you
spot a bug — open an issue on GitHub with a minimal repro. PRs are
welcome but unsolicited large rewrites are unlikely to land.

The working agreement in [`CLAUDE.md`](CLAUDE.md) describes the
non-negotiable principles that apply to every change (think before
coding, simplicity first, surgical changes, fail loud, etc.). PRs
that ignore those principles will be sent back.

## Further reading

1. [`FOUNDATION.md`](FOUNDATION.md) — the architectural spine.
2. [`adr/README.md`](adr/README.md) — how ADRs work; numbered ADRs in
   order, later ones assume earlier ones.
3. [`docs/CUTOVER_RUNBOOK.md`](docs/CUTOVER_RUNBOOK.md) — the prod
   cutover + update procedure.

## Operator runbook — hülye-biztos cookbook

Field-tested commands. Copy whichever recipe you need. Replace `<VERSION>` with the release name (e.g., `PROD_v2.27.50`, `PROD_v2.27.49`).

### 1. Upgrade prod to a new release (Frissítés új verzióra)

The canonical "go from current to `<VERSION>`" command. Kills running aberp, syncs to release branch, snapshots, swaps binary, launches.

```bash
cd ~/ABERP && \
pgrep -f aberp | xargs -r kill 2>/dev/null; sleep 2; \
pgrep -f aberp | xargs -r kill -9 2>/dev/null; \
git fetch origin && git reset --hard origin/<VERSION> && \
./run/upgrade_prod.sh <VERSION>
```

### 2. Just relaunch (Újraindítás verzióváltás nélkül)

After a Ctrl-C or shutdown, when nothing changed and you want prod back up.

```bash
cd ~/ABERP && \
pgrep -f aberp | xargs -r kill 2>/dev/null; sleep 2; \
pgrep -f aberp | xargs -r kill -9 2>/dev/null; \
./run/run_prod.sh
```

### 3. Kill stuck aberp processes (Lefagyott aberp folyamatok kilövése)

When graceful shutdown didn't drain everything (rare post-PR-209 / S213).

```bash
pgrep -f aberp | xargs -r kill 2>/dev/null; sleep 2; \
pgrep -f aberp | xargs -r kill -9 2>/dev/null
```

### 4. Emergency bypass — launch with dirty tree (Vészhelyzeti megkerülés)

For dev workflows or when you've verified state by hand and know the git check is a false positive. NEVER for casual prod use.

```bash
cd ~/ABERP && ABERP_SKIP_GIT_CHECK=1 ./run/run_prod.sh
```

### 5. Verify remote branch + tag SHAs before resetting (Távoli állapot ellenőrzése)

Sanity-check before any `git reset --hard origin/<VERSION>`.

```bash
git ls-remote https://github.com/Cservin69/ABERP.git \
  refs/heads/main refs/heads/PROD_v2.27.50 \
  refs/tags/PROD_v2.27.50
```

### 6. Restore tenant from snapshot (Visszaállítás biztonsági mentésből)

If an upgrade went sideways. The snapshot was taken at the start of every `upgrade_prod.sh` run; tarball + keychain-zip live in `~/aberp-snapshots/`.

```bash
# Stop the app first
pgrep -f aberp | xargs -r kill -9 2>/dev/null
# Pick the snapshot to restore
ls -lt ~/aberp-snapshots/prod-*.tgz | head -3
# Replace TIMESTAMP with the chosen file
tar -C "$HOME/.aberp" -xzf "$HOME/aberp-snapshots/prod-TIMESTAMP.tgz"
unzip "$HOME/aberp-snapshots/prod-TIMESTAMP-keychain.zip" -d /tmp/
# Re-import keychain entries
for line in $(jq -r '.[] | @base64' /tmp/keychain-prod.json); do echo "$line" | base64 -d | jq -r '"security add-generic-password -s \"" + .service + "\" -a \"" + .account + "\" -w \"" + .password + "\""'; done
# (paste each printed command back into the shell)
# Relaunch
cd ~/ABERP && ./run/run_prod.sh
```

### 6b. DuckDB snapshot / restore — the panic button (DuckDB pillanatkép)

Distinct from recipe 6. This is the S393 CLI that snapshots **just the
tenant DuckDB** (binary-validated via `PRAGMA verify_external_invariants`)
to `~/Documents/ABERP-snapshots/` — outside the repo and outside
`~/.aberp/`, so a tenant wipe or restore never touches it. **Take one
before every upgrade**, especially across a one-way DuckDB storage bump
(e.g. 1.5.2 → 1.5.3). Best run with `aberp serve` stopped.

`--db` defaults to `./aberp.duckdb` on both subcommands, so always pass
the real tenant DB path (`~/.aberp/prod/aberp.duckdb`) explicitly.

```bash
cd ~/ABERP
# Take a snapshot (writes ~/Documents/ABERP-snapshots/prod-<UTC-ts>.duckdb)
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
refuses a snapshot that fails its own validity check — so it never
clobbers a working DB with a broken backup.

### 7. Wipe leftover worktrees in DEV that poison prod check (Dev worktree takarítás)

Pre-PR-C only relevant. Post-PR-C, run_prod.sh uses its own checkout's path and dev's worktrees don't affect prod. Still useful for dev cleanup.

```bash
cd ~/Documents/Claude/Projects/ABERP && \
git worktree list && \
git worktree list --porcelain | grep '^worktree' | awk '{print $2}' | grep -v "^$(pwd)$" | xargs -r -I{} git worktree remove --force {} 2>/dev/null; \
git worktree prune && \
rm -rf .claude && git status
```

### 8. Verify a release binary's provenance (Build provenance ellenőrzés)

Confirms the binary was built from the same audit-ledger state it claims.

```bash
cargo run -p aberp-verify -- --tenant prod
```

### 9. Setup NAV creds + SMTP password on a fresh box (Új gépen alapbeállítás)

After cloning the repo on a new machine and before the first prod launch.

```bash
cd ~/ABERP && ./run/setup_nav_creds.sh
# Then in Tenant Settings → SMTP → enter the SMTP password
# Then in Tenant Settings → Quote Intake (if enabled) → bearer token
```

### Forensics

- Snapshot tarballs: `~/aberp-snapshots/prod-*.tgz` (encrypted keychain dump beside each)
- Audit ledger: `~/.aberp/prod/audit-ledger.duckdb` + mirror at `~/.aberp/prod/audit-ledger.jsonl`
- DuckDB: `~/.aberp/prod/aberp.duckdb`
- Seller config: `~/.aberp/prod/seller.toml`
- Logs (Tauri): `~/Library/Logs/aberp/`
