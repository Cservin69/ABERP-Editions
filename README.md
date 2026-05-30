# ABERP

Modular multi-tenant ERP. Rust backend, Tauri + Svelte local UI, cloud UI later.
First production surface: NAV-compliant invoicing for a single tenant.
First real-world user: a CNC manufacturing company (inventory, logistics, CAD/CAM).

The order of operations is deliberate: foundation, then ADRs, then build. The
spine passed its first full-spine adversarial review at the close of session 4
(see `docs/reviews/`); the workspace scaffold — commit #1's first PR — landed
in session 5. Further ADRs land just-in-time when their named triggers fire
(see `adr/README.md` Deferred section).

## Layout

```
ABERP/
  README.md           ← you are here
  FOUNDATION.md       ← the architectural spine — every ADR must be consistent with it
  CLAUDE.md           ← project-wide working agreement
  LICENSE
  Cargo.toml          ← workspace manifest, pinned deps per ADR-0021
  Cargo.lock          ← committed pin set per ADR-0007 §Supply chain
  rust-toolchain.toml ← Rust 1.85.0 (MSRV floor) per ADR-0021
  adr/
    README.md         ← ADR index, numbering, status lifecycle, review cadence
    0001-*.md ... 0021-*.md
  docs/
    threat-model.md
    id-prefixes.md
    research/         ← raw research notes (NAV/Billingo, stack baseline)
    reviews/          ← adversarial review records
  crates/
    audit-ledger/     ← tamper-evident audit ledger (ADR-0008)
    nav-transport/    ← NAV TLS transport + credentials (ADR-0009 §4, ADR-0020)
    nav-xsd-validator/← <InvoiceData> v3.0 runtime invariant check (ADR-0022)
  modules/
    billing/          ← NAV invoice issuing (ADR-0009)
  apps/
    aberp/            ← the CLI binary
    aberp-ui/         ← Tauri 2 + Svelte 5 operator UI shell (ADR-0004)
```

## Reading order

1. `FOUNDATION.md` — the spine. Read this first.
2. `adr/README.md` — how ADRs work in this project.
3. The numbered ADRs — read in order; later ADRs assume earlier ones.

## Working principles (non-negotiable)

These come from the project's working agreement and apply to every change:

- **Think before coding.** State assumptions; don't guess.
- **Simplicity first.** Minimum code, no speculative abstractions.
- **Surgical changes.** Touch only what the task requires.
- **Goal-driven.** Define success criteria up front, loop until verified.
- **Use the model for judgment, not for routing or deterministic transforms.**
- **Surface conflicts, don't average them.** Two patterns? Pick one explicitly.
- **Read before you write.** No duplicate functions next to identical ones.
- **Tests verify intent, not just behavior.** A test that can't fail when business logic changes is wrong.
- **Match codebase conventions.** Don't fork patterns silently.
- **Fail loud.** "Completed successfully" with 14% silently skipped is the worst class of bug.

## Status

Build phase. Workspace scaffold landed in session 5; the supply-chain CI,
audit-ledger crate, billing module, and the NAV-XML-on-disk binary (commit
#1's success criterion) land across the rest of session 5's PRs. See
`adr/README.md` for the design ledger and `git log` for landed commits.

## Production cutover & releases

Dev work happens on `main`. Production releases live on the `prod` branch
and as annotated `prod-vMAJOR.MINOR.PATCH` tags.

The compile-time `production` Cargo feature is the load-bearing switch:

- **Without** `--features production` (every dev build) — NAV calls
  route to `api-test.onlineszamla.nav.gov.hu`, invoice numbers are
  prefixed `TEST-`, and the prod endpoint is structurally unreachable
  (`assert_endpoint_allowed`).
- **With** `--features production` — NAV calls route to the real
  `api.onlineszamla.nav.gov.hu`, the `TEST-` prefix is dropped, and
  boot-time sanity checks enforce that the binary is running as
  `tenant=prod` with the documented seller identity.

Release workflow:

```bash
./run/release.sh prod-v0.1.0   # validates main+clean, fmt, builds with
                               # --features production, tags locally
./run/run_prod.sh              # launches the prod binary (Tauri shell)
```

The full manual cutover procedure — first-time prod branch creation,
seller.toml template, NAV+SMTP credential setup, smoke-invoice
checklist, rollback, ongoing update workflow — is documented in
**[`docs/CUTOVER_RUNBOOK.md`](docs/CUTOVER_RUNBOOK.md)**.

> **The dev-DB-disposable rule reverses at prod cutover.** Once
> `~/.aberp/prod/aberp.duckdb` holds a single issued invoice, it is
> the legal record. Every schema change must be forward-compatible
> from that point onward; the audit ledger is already append-only
> (ADR-0008). Snapshot the DB before any upgrade — see the runbook
> Step 9.
