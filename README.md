# ABERP

A small-business ERP focused on **Hungarian NAV Online Számla v3.0**
invoicing. Rust backend, Tauri 2 + Svelte 5 desktop UI, append-only
hash-chained audit ledger. Runs locally on the operator's own machine;
no SaaS dependency. Single-maintainer, non-commercial, open-source.

> **License — PolyForm Noncommercial 1.0.0.** ABERP is free for
> non-commercial use. See [`LICENSE`](LICENSE) for the full terms. If
> you want to use it commercially, contact the maintainer.

> **Hungarian invoicing law is the operator's responsibility.** ABERP
> submits to NAV per the v3.0 spec, but the operator is the legally
> responsible party for the content of their invoices. ABERP is a tool;
> compliance is yours.

## Status

Pre-release pilot. The first production cutover is imminent (PROD_v1.x).
Real money flows through the pilot; the test path is the default for any
build that does not pass `--features production`.

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
git clone --branch PROD_v1.0 <origin-url> ABERP-prod
cd ABERP-prod
./run/run_prod.sh   # builds with --features production, launches the shell
```

`./run/release.sh PROD_v1.0` is the dev-side script that publishes a
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

Always run `./tools/snapshot-prod.sh` before switching release branches.
It tarballs `~/.aberp/<tenant>/`, encrypts the keychain entries, AND
drops `~/.aberp/<tenant>/.upgrade-snapshot.toml` — a small contract
file the next boot of the new binary compares against the post-upgrade
`seller.toml`. The binary REFUSES to start if `[seller.smtp]` or
`[seller.numbering]` drifted, so you don't need to remember to verify
them manually.

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
    aberp-verify/      ← external-auditor evidence-bundle verifier
    aberp-quote-intake/ ← sister-service quote-poll daemon (S210 — Stage 2 entry)
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
