# PROD_v2.0 — ABERP 2.0: Stage 2 begins

**Cutover date:** 2026-05-31 (session 212 / PR-211).
**Release branch:** `origin/PROD_v2.0`.
**Predecessor:** `PROD_v1.4` (last Stage-1-only invoicing release).
**Cutover SHA recorded in:** `docs/releases/PROD_v2.0.md` (this file)
and the annotated `PROD_v2.0` tag.

## Headline

**ABERP 2.0 is the first Stage 2 release.** The cutover marker fires
per ADR-0056's versioning policy: the quote-intake module (S210 / PR-204
backend + S211b / PR-210 SPA + S212 / PR-211 ADR + cutover) is a new
module by ADR-0056's compound test — new routes, new schema, new audit
kind. From here on, the ABERP version string carries the Stage signal:
1.x = invoicing-only, 2.x = invoicing + Stage 2 module surface area.

The 1.x invoicing strand is bit-for-bit preserved. Operators upgrading
from PROD_v1.4 (or any 1.x release) get every Stage 1 feature unchanged,
plus an opt-in quote-intake daemon that stays dormant unless explicitly
enabled.

## Breaking changes

**NONE.** Every Stage 1 invoicing path is preserved. The quote-intake
daemon defaults to disabled; an operator who never opens Tenant
Settings → Quote Intake or sets `ABERP_QUOTE_INTAKE_ENABLED=true`
sees zero behavioral change. The DuckDB schema gains one additive
table (`quote_intake_log`) and the audit ledger gains one new
EventKind variant (`QuoteIntakePollCompleted`); no existing column,
table, or audit kind is altered.

## New features (the only two bullets that matter)

- **Quote intake from sister storefront.** A polling daemon
  (`aberp-quote-intake` crate, S210 / PR-204) ingests approved quotes
  from the storefront's `GET /api/quotes?status=approved` endpoint,
  stages each one in the per-tenant `quote_intake_log` DuckDB table
  with a pre-prepared `DraftInvoice` JSON, and POSTs the status
  write-back to the storefront. The canonical `invoice` table is
  NEVER written by the daemon — operator pickup routes through the
  existing `issue-invoice` pipeline (sequence burn, audit chain,
  NAV submission stay operator-gated).
- **SPA surface for the intake queue + configuration.** A "Quotes /
  Ajánlatok" operational tab (S211b / PR-210) mounts as the third
  segmented-control tab under `#/invoices` (alongside Outgoing +
  Incoming). A Tenant Settings → Quote Intake card lets the operator
  enable the daemon, set the storefront URL + poll interval, and
  store a write-only bearer token in the macOS keychain. A Test
  Connection button issues a one-shot probe before the daemon ever
  starts.

## Architecture pin

The full architectural shape is in [ADR-0057 — Quote intake from
sister storefront](../../adr/0057-quote-intake-architecture.md):
operator-pull daemon (not webhook), `quote_intake_log` staging table
(not auto-burn of `invoice`), `Zeroizing` bearer tokens end-to-end,
spawn precedence env > toml+keychain > dormant, no hot-reload
(restart-required banner). Three alternatives weighed (webhook,
manual fetch, polling); polling wins because ABERP is local-desktop
(no inbound HTTP surface) and the daemon enforces cadence (no
operator-discipline failure).

## Upgrade path from PROD_v1.4 (or any 1.x release)

```bash
# On the prod machine, with the ABERP repo checked out under ~/ABERP-prod:
cd ~/ABERP-prod
git fetch origin
./run/upgrade_prod.sh PROD_v2.0
```

The `upgrade_prod.sh` one-command flow (S200) does:

1. Pre-flight `snapshot-prod.sh` (per the runbook + ADR-0055
   tenant-state inventory contract). Captures the tenant DuckDB, the
   `nav-artifacts/` + `ap-artifacts/` side-stores, `seller.toml` (all
   six preservation slots, including the new `[quote_intake]` section
   if the operator has set it), and the keychain entries.
2. Clean `git switch PROD_v2.0` (refuses on dirty trees).
3. Exec `run_prod.sh`, which:
   - Refuses to start unless HEAD is `origin/PROD_v2.0` (or
     `ABERP_SKIP_GIT_CHECK=1` opt-out, intended for dev only).
   - Boots `aberp serve` against the production NAV endpoint.
   - The quote-intake daemon stays dormant on the first boot UNLESS
     the operator has previously enabled it via Tenant Settings OR
     `ABERP_QUOTE_INTAKE_ENABLED=true` is in the environment. Default
     posture = dormant; operator decides.

### Post-upgrade verification

- The audit timeline should show one `system.quote_intake_poll_completed`
  entry per minute (default `poll_interval = 60s`) ONLY IF the operator
  enabled the daemon. If dormant, no entries appear; the audit ledger
  for outgoing invoices is unchanged.
- The SPA `#/invoices` page should show three tabs: Outgoing, Incoming
  (AP), Quotes / Ajánlatok. The Quotes tab renders an empty-state copy
  until the daemon stages a row OR the operator manually triggers
  intake (manual trigger named-deferred to a follow-up PR).
- `seller.toml` is preserved on every identity-write. Touch a Tenant
  Settings field (e.g. re-save identity), then confirm `[quote_intake]`
  is still present in `~/.aberp/<tenant>/seller.toml` (sixth
  preservation slot per ADR-0057 §"Invariants pinned" + the
  [[seller-toml-write-invariant]] memory).

### Rollback

If a regression surfaces:

```bash
cd ~/ABERP-prod
./run/upgrade_prod.sh PROD_v1.4
```

The 1.4 binary does not understand the `quote_intake_log` table; it
ignores it. The audit ledger's `QuoteIntakePollCompleted` entries
written under 2.0 are preserved (the ledger is append-only per
ADR-0008); the 1.4 binary deserialises them as
`EventKind::Unknown(...)` and surfaces them in the timeline under a
neutral label. No data is lost.

## What did NOT change

- The Stage 1 invoicing pipeline (issue → NAV submit → poll → email)
  is bit-identical to PROD_v1.4.
- The NAV endpoint posture (test for dev builds, production behind
  the `production` Cargo feature per S165) is unchanged.
- The AP module (S177 / S178 / S179 / S197 / S203) is unchanged.
- The NAV-as-DR restore wizard (S180 / S196) is unchanged.
- The `seller.toml` preservation invariant grows from five slots
  (identity / banks / smtp / numbering / branding) to six (adds
  `[quote_intake]`). All five prior slots stay preserved.

## Roadmap pointers

- Stage 2 continues with Ordering (full lifecycle) and Inventory sync,
  per `docs/e2e-shop/ground-zero.md`. Each future Stage 2 module is a
  candidate for a 2.x MINOR bump if it stays scoped to extending the
  Stage 2 surface (per ADR-0056 §"Heuristic"), or a 3.0 trigger if it
  introduces a brand-new operational concept (e.g. CAD/CAM artifact
  store with its own lifecycle).
- The CAD/CAM artifact cold-archival question is named-deferred per
  ADR-0057 §"Open questions" — Phase 4 of the e2e-shop integration is
  the trigger.
- Multi-operator token rotation (today: one bearer per tenant) is
  named-deferred per ADR-0057 §"Trade-offs" — the trigger is the
  first multi-operator deployment.

## Operator quick-reference

| Surface | Where |
|---|---|
| Enable / disable the daemon | Tenant Settings → Quote Intake card |
| Override at the environment | `ABERP_QUOTE_INTAKE_ENABLED=true` |
| Storefront URL | Tenant Settings → Quote Intake → Base URL |
| Bearer token (write-only) | Tenant Settings → Quote Intake → Token |
| Pending intake queue | `#/invoices` → Quotes / Ajánlatok tab |
| Per-cycle audit entry | Audit ledger, kind `system.quote_intake_poll_completed` |
| Staging table | DuckDB `quote_intake_log` (per-tenant) |
| Architectural reference | `adr/0057-quote-intake-architecture.md` |
| Backend crate | `crates/aberp-quote-intake/` |
| Daemon spawn site | `apps/aberp/src/serve.rs` (boot block) |

## Version posture

On-disk version constants (`Cargo.toml` workspace `version`,
`apps/aberp-ui/tauri.conf.json` `version`, `apps/aberp-ui/ui/package.json`
`version`) remain `"0.0.0"` — the established Stage 1 posture. The
release identity is carried by the **branch + tag** (`PROD_v2.0`), not
by the source-level version string. This matches the posture every
prior `PROD_v1.x` cutover shipped under. The `softwareMainVersion`
field NAV receives in the wire payload (`env!("CARGO_PKG_VERSION")`)
likewise stays `"0.0.0"` — NAV-side acceptance is by `softwareId`
match (`ABERP-000000000001`), not by version string.
