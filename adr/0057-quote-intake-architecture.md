# ADR-0057 — Quote intake from sister storefront: operator-pull daemon + staging table + no auto-burn of the regulated `invoice` surface

**Status:** Accepted — S212 / PR-211 (2026-05-31). The 2.0 cutover marker
for ABERP per ADR-0056. Pins the architectural shape that the first
Stage 2 module (S210 / PR-204 backend + S211b / PR-210 SPA) implemented:
ABERP polls a sister storefront's `/api/quotes?status=approved`, stages
each fresh approved quote in a purpose-built `quote_intake_log` DuckDB
table together with a pre-prepared `DraftInvoice` JSON, and lets the
operator adopt it through the normal `issue-invoice` pipeline. The
canonical `invoice` table is never touched by the daemon.
**Author:** Ervin Áben (ABERP), session 212 brief — 2.0 cutover.
**Supersedes / amends:** none — additive architectural ADR. Names the
shape that S210 + S211b shipped; pins the contract that future Stage 2
sister-service integrations must follow.
**Related:** ADR-0009 (NAV invoice issuing — §2 sequence burn, §3 audit
chain), ADR-0019 (relational SoT — `invoice` table is the regulated
surface), ADR-0021 §"Items deferred to build phase" (Stage 2 catalogue),
ADR-0056 (versioning policy — names the 2.0 trigger), ADR-0055 (tenant-
state inventory contract — `quote_intake_log` row + seller.toml
preservation slot land in the runbook in the same PR), the e2e-shop
Stage 2 design (`docs/e2e-shop/ground-zero.md`), and the Stage 1 daemon
patterns this ADR reuses (S161 NAV poll daemon, S178 AP sync daemon,
S184 storno post-issue tail).

## Context

Stage 2 of the ABERP roadmap (per ADR-0021 §"Items deferred to build
phase" and `docs/e2e-shop/ground-zero.md`) is the ERP build-out: an
external customer-facing storefront takes quote requests from end
customers; an approval workflow happens on the storefront; *approved*
quotes need to flow into ABERP as invoices the operator issues against
the customer.

Two facts make the architecture non-obvious:

1. **ABERP is a local desktop app** (Tauri shell + loopback HTTPS
   backend, per ADR-0004 + ADR-0021 §Part B). It has no public IP and
   no inbound webhook surface. Whatever pattern connects the storefront
   to ABERP MUST work from ABERP's side, against the storefront's
   public HTTP surface.
2. **The `invoice` table is the regulated surface** (ADR-0009 §2-§3).
   Every row is a sequence-burned, audit-chained, NAV-bound record.
   Auto-creating sequence-burned rows from background HTTP polling
   would couple a remote-service outage to the regulated invoice
   surface — a NAV-side rejection on a poll-spawned row would have to
   be unwound across the sequence + the audit chain + the NAV ack
   storage. Stage 1 deliberately avoids this coupling (CLAUDE.md
   rule 2 + the [[trust-code-not-operator]] memory: the operator
   clicks Issue, code does not).

The S210 brief named the design constraint: "the daemon stages
quotes — it MUST NOT touch the `invoice` table". S211b's SPA brief
extended the constraint to the operator UX: a "Quotes / Ajánlatok"
operational tab lists pending intakes; pickup is a future operator
click (not a daemon side-effect). This ADR documents the architectural
posture those two PRs established and pins it as the contract for
future sister-service integrations under Stage 2.

## Decision

The architecture is **operator-pull from ABERP to the storefront**,
realised as a four-part pipeline:

### 1. Polling — `aberp-quote-intake` crate, daemon spawned at `aberp serve` boot

A new crate `crates/aberp-quote-intake/` owns:

- The `QuoteIntakeConfig` (env-driven OR `[quote_intake]` section in
  `seller.toml` + bearer token in the OS keychain).
- The `QuoteIntakeService` that owns the `reqwest` HTTPS client, the
  audit-ledger handle, and the per-tenant DuckDB path.
- The `run_daemon_forever()` loop: poll → ingest → write-back → audit →
  sleep `poll_interval` → repeat.

The daemon is spawned from `apps/aberp/src/serve.rs` at boot. Spawn
precedence is **env > toml+keychain > dormant**:

- If `ABERP_QUOTE_INTAKE_ENABLED=true` is in the environment, the
  daemon starts with env-derived config.
- Else, if `[quote_intake] enabled = true` is in `seller.toml` AND a
  keychain bearer token exists for the tenant, the daemon starts with
  toml + keychain config.
- Else, the daemon stays dormant. A single `tracing::info!` line names
  the dormancy at boot.

**Refuse-to-start on misconfiguration.** If `ENABLED=true` is set but
the URL or token is missing/empty/non-http(s), `aberp serve` aborts the
boot rather than spawn a daemon that polls the wrong URL forever. Per
[[trust-code-not-operator]].

**No hot-reload.** A toml/keychain edit takes effect on the next
`aberp serve` boot. The SPA settings card surfaces a restart-required
banner. (S211b conservative choice — hot-reload requires multiplexing
the daemon's lifetime with the config watcher; out of scope for the
2.0 cutover.)

### 2. Endpoint contract — `GET /api/quotes?status=approved` + `POST /api/quotes/:id/status`

The storefront publishes two endpoints under its operator-facing API:

- `GET /api/quotes?status=approved` — returns a JSON array of approved
  quotes the storefront has not yet been told ABERP picked up. Each
  quote carries an opaque storefront ID, the contact info (name +
  email + optional tax_number / billing address), the line items (with
  description / quantity / unit / unit price / VAT bracket), and an
  optional reference to a CAD/CAM bundle (file pointer; not downloaded
  by ABERP in v1).
- `POST /api/quotes/:id/status` with `{ "status": "invoiced", "note":
  "<free-form note carrying the ABERP draft invoice ULID>" }` — the
  ABERP-side write-back after the staging row lands.

Both calls carry a `Bearer <token>` from the configured secret. The
token is `Zeroizing`-wrapped end-to-end (loaded into a `Zeroizing<String>`
from env or keychain; never logged).

### 3. Staging — `quote_intake_log` DuckDB table + pre-prepared `DraftInvoice` JSON

A new per-tenant table `quote_intake_log` (schema in
`crates/aberp-quote-intake/src/log_table.rs`):

```sql
CREATE TABLE IF NOT EXISTS quote_intake_log (
    quote_id              VARCHAR NOT NULL PRIMARY KEY,
    tenant_id             VARCHAR NOT NULL,
    invoice_id            VARCHAR NOT NULL,
    received_at           VARCHAR NOT NULL,
    intake_at             VARCHAR NOT NULL,
    status_writeback_at   VARCHAR,
    raw_payload           VARCHAR NOT NULL,
    prepared_draft        VARCHAR NOT NULL
);
CREATE INDEX IF NOT EXISTS quote_intake_log_pending_writeback_idx
    ON quote_intake_log (tenant_id, status_writeback_at);
```

Per fetched approved quote that is NOT already in the table:

1. Mint an `InvoiceId` ULID locally (`invoice_id` — prefixed `inv_`).
2. Build a `PreparedDraft` JSON the operator-side issue pipeline will
   adopt verbatim (line items, partner identity, currency, payment
   method default).
3. INSERT one row into `quote_intake_log` with `raw_payload` =
   verbatim JSON from the storefront (audit-grade) and `prepared_draft`
   = the JSON the SPA renders + the issue route consumes on pickup.
4. POST the status write-back to the storefront with the minted
   `invoice_id` in the `note` field (operator cross-reference + the
   storefront's own audit trail).
5. If the write-back succeeds, UPDATE `status_writeback_at` on the
   same row; if it fails, leave NULL and retry on the next poll cycle.

The `quote_id` PRIMARY KEY is the cross-cycle idempotency contract:
a storefront that re-returns an already-picked-up quote (write-back
lost in transit, storefront reset, etc.) is a no-op INSERT — the row
already exists, the operator already saw it.

**The `invoice` table is NOT written by the daemon.** Adoption happens
when the operator clicks pickup in the SPA, which routes through the
existing `issue-invoice` pipeline — sequence burn, audit chain, NAV
submission stay operator-gated. The `prepared_draft` JSON is the
operator's pre-filled form; they can accept or edit before clicking
Issue.

### 4. Audit — `system.quote_intake_poll_completed` per cycle (system-prefixed)

One audit-ledger entry per poll cycle, `EventKind::QuoteIntakePollCompleted`
(serialised as `system.quote_intake_poll_completed`, mirroring the AP
sync's `system.incoming_invoice_sync_cycle_completed` per ADR-0008 F12
ritual + the S178 precedent). Payload summarises: fetched count, new
intake count, write-backs attempted / succeeded / retried / failed,
duration. No PII (the verbatim `raw_payload` rides in the
`quote_intake_log` row, not the audit ledger).

### 5. SPA surface — Quotes / Ajánlatok operational tab + Tenant Settings card

`apps/aberp-ui/ui/src/routes/QuotesList.svelte` mounts as the third
operational tab on `#/invoices` (Outgoing / Incoming / Quotes per
S179 segmented-control extension). Lists pending intakes with the
prepared-draft preview. Future: a "Pickup" button per row that POSTs
the prepared-draft JSON to the existing issue pipeline (named-deferred
to a follow-up PR).

`apps/aberp-ui/ui/src/routes/TenantSettings.svelte` gains a Quote
Intake card (bilingual HU/EN): enable toggle, base URL, poll interval,
write-only bearer token (writes to keychain, NEVER reads back), Test
Connection button (issues a one-shot GET `/api/quotes?status=approved`
with the typed/keychain token, surfaces the HTTP status). A
restart-required banner reminds the operator changes take effect on
the next boot.

## Rationale — three alternatives considered, polling wins

### Alternative A: Storefront → ABERP webhook on approval

**Rejected.** ABERP is local-desktop; no inbound public surface. A
webhook would require a tunnel (ngrok-style) or a cloud-relay queue
(per ADR-0016 cloud-sync — explicitly deferred). Either way, the
inbound path is a new attack surface (auth, replay defense, rate
limit, DoS hardening) that the operator-pull pattern avoids entirely.
The storefront does not need to know where ABERP runs; ABERP knows
where the storefront is.

### Alternative B: Operator clicks "Fetch quotes" in the SPA

**Rejected.** Reproducible operator discipline failure — quotes pile
up on the storefront, the operator forgets to click for two days,
customers complain. A daemon enforces the cadence; the operator owns
*adoption* (pickup), not *fetch*. This matches the Stage 1
[[trust-code-not-operator]] posture: structural guarantees beat
operator habit. A manual fetch button stays as a future complement
(SPA-side ergonomics), not the load-bearing path.

### Alternative C: Polling — what we shipped

**Accepted.** Reuses the daemon pattern proven in Stage 1 (S161 NAV
poll daemon, S178 AP sync daemon — both indefinite tokio::spawn loops
with backoff + audit-cycle entries). The 60s default poll interval
gives ~30s mean approval-to-staging latency, configurable to 10s..3600s
per operator preference. Backpressure is implicit: a slow storefront
extends the cycle, not the daemon's invariants; a fast operator can
adopt staged intakes while the next poll is mid-flight.

## Consequences

### Wins

- **Single source of truth per concept.** Quotes live on the
  storefront (with their CAD/CAM artifacts, customer back-and-forth,
  approval history); invoices live in ABERP (with NAV ack chains,
  sequence burns, regulated 8-year retention). The intake daemon is
  the named bridge; neither system pretends to own the other's data.
- **Daemon-pattern reuse.** S161 (NAV poll) + S178 (AP sync) +
  the new quote-intake daemon all share the same shape (tokio::spawn
  at boot, audit-cycle entry per loop, refuse-to-start on bad config,
  graceful dormancy). A future Stage 2 daemon (Ordering, Inventory
  sync, etc.) drops into the same template — `apps/aberp/src/serve.rs`
  is already the spawn site.
- **Regulated surface untouched.** The `invoice` table behaves
  identically whether the daemon ran or not. An operator who never
  enables quote intake sees zero behavioral change. An operator who
  enables it sees pending intakes in the SPA tab but the same Issue
  button drives the same path — the only branch is "pre-filled
  draft from staging row" vs "blank draft". The audit chain,
  sequence burn, NAV ack are bit-for-bit identical.
- **Idempotency at the schema layer.** `quote_id` PRIMARY KEY makes
  re-fetch a no-op INSERT. The storefront can lose the write-back
  ack, double-publish the approved quote, restart its DB — none of
  that creates duplicate staging rows on the ABERP side.
- **Release decoupling.** The storefront can iterate independently
  of ABERP as long as the JSON contract on the two endpoints stays
  stable. ABERP can cut a release (PROD_vN.M.P) without coordinating
  with the storefront's deploy cycle.

### Trade-offs

- **Eventual consistency on the approval→staging path.** The
  60s default poll interval is the latency between storefront-side
  approval and ABERP-side staging. For a small-volume artisan shop
  (the ABERP target operator profile) this is fine; if a higher-volume
  Stage 2 deployment surfaces, the interval clamps down to 10s
  (`MIN_POLL_INTERVAL_SECS`). Sub-10s polling would warrant a webhook
  re-evaluation.
- **Bearer token is a single shared secret.** The token authenticates
  the daemon to the storefront on every poll. A compromised token
  reveals the entire approved-quotes list. Mitigations: keychain
  storage (not on-disk), `Zeroizing` in-memory, write-only SPA
  surface (operator can rotate the token but never read it back).
  Multi-operator deployments will need OAuth or per-operator tokens
  (deferred — out of scope for the 2.0 cutover; named in §"Open
  questions" below).
- **Storefront contract is a private API, not a standard.** The two
  endpoints (`GET /api/quotes?status=approved`, `POST
  /api/quotes/:id/status`) are ABERP-and-storefront-specific. A
  third sister-service integration would need its own equivalent or
  the contract would need to be generalised. The conservative
  default: each Stage 2 sister service gets its own crate
  (`aberp-<service>-intake`) until two consumers exist and a
  generalisation earns its keep (CLAUDE.md rule 13).
- **The `prepared_draft` JSON is a snapshot, not a live view.** If
  the storefront's price changes after staging but before the
  operator adopts, the `prepared_draft` carries the at-staging price.
  This is intentional (the operator decides what gets invoiced; a
  late price change is a re-approval on the storefront), but
  it is a footgun if the operator adopts a long-stale draft. Future
  mitigation: a "staged N days ago" warning in the SPA tab (named
  as future ergonomic polish).

### When to revisit

- A second sister-service integration (Ordering full lifecycle,
  external Inventory sync, third-party catalogue) lands. At that
  point the per-service-crate pattern is checked: is there a
  load-bearing shared shape (HTTP client + audit cycle + staging
  table + write-back), and should it become an `aberp-intake-runtime`
  trait? If yes, the per-service crates become thin payload/mapping
  adapters over the shared runtime. If no (each service is genuinely
  different), the per-crate pattern stays.
- The daemon's poll cadence becomes operator-visible pain (either
  too slow under load, or wasting CPU when idle). The clamp window
  `[MIN_POLL_INTERVAL_SECS, MAX_POLL_INTERVAL_SECS]` extends, or
  the daemon gains a long-poll mode if the storefront grows one.
- Multi-operator deployments surface (today: one operator per
  tenant). The bearer-token-as-shared-secret model gives way to
  OAuth-style per-operator scopes; the keychain entry becomes
  per-operator instead of per-tenant.
- The CAD/CAM artifact pointer in the quote payload needs to land
  in ABERP for cold archival (today: stays on the storefront, ABERP
  cross-refs by ID). This is a Phase 4 e2e-shop concern per
  `docs/e2e-shop/ground-zero.md`; not in scope for 2.0.

## Adversarial review

- *"A daemon that auto-creates invoices is exactly the coupling
  CLAUDE.md rule 2 warns about."* The daemon does NOT auto-create
  invoices. It stages rows in `quote_intake_log` and pre-prepares a
  `DraftInvoice` JSON. The operator's pickup click drives the
  existing `issue-invoice` pipeline — sequence burn, audit chain,
  NAV submission stay operator-gated. The §3 "no auto-burn" rule
  is the load-bearing constraint, not a footnote.

- *"What if the storefront returns a malformed quote — does the
  daemon crash?"* The daemon's `process_one_quote` swallows per-
  quote errors as `summary.failed += 1` and continues. The audit
  cycle entry surfaces the failure count. The operator sees the
  count in the audit timeline. A persistent failure (same quote
  failing every cycle) is operator-visible via the audit ledger
  and the staging-row absence; future ergonomic polish: a
  "rejected by mapping" surface in the SPA tab. The daemon never
  panics on payload data.

- *"The bearer token sits in macOS Keychain forever — what if the
  operator forgets to rotate?"* Rotation is operator-driven via
  the SPA Test Connection + write-only token field. The token
  has no expiry on the ABERP side; the storefront enforces TTL
  (out of scope for this ADR). If a deployment requires
  mandatory rotation, the storefront can refuse the token at
  any poll cycle, the daemon surfaces the 401 in the audit
  payload, and the operator opens Tenant Settings to write the
  new token.

- *"Why a per-cycle audit entry and not per-quote?"* Per-quote
  audit entries would flood the ledger (a fetched-but-already-
  staged quote on every cycle = O(active-approved-count) entries
  per minute). The per-cycle entry's payload carries the
  per-quote counters (fetched / new / write-back outcomes); the
  `raw_payload` lives in the staging row for forensic depth. The
  ledger stays at the audit-grade granularity it pins for every
  other system-prefixed event (sync cycles, poll cycles).

- *"What happens if `aberp serve` boots before the storefront is
  up?"* The first poll cycle's HTTP call fails; the daemon logs
  the error, the audit entry records `fetched = 0`, the daemon
  sleeps the interval and tries again. There is no boot-time
  health check on the storefront URL — a transient storefront
  outage is not an ABERP boot blocker. The operator sees the
  failure in the audit timeline (and the SPA tab stays empty);
  this is louder than a silent retry-loop, but it does not block
  the rest of `aberp serve`.

- *"The `Zeroizing` token still lives in HTTP request headers,
  which `reqwest` may log under debug tracing."* Confirmed —
  `reqwest`'s `debug` features can log headers. The ABERP build
  posture is `tracing` filters at `info` for the daemon module,
  no header logging on the request path. Per ADR-0007's secrets
  posture, the explicit guarantee is "not logged in the
  ABERP-emitted log lines"; transitive dependency logging at
  debug is a runtime ops choice.

## Alternatives considered

(See §"Rationale — three alternatives considered" above for the
detailed treatment of: Webhook from storefront, Manual operator
fetch, and Polling.)

Additional alternatives that did not survive the first cut:

- **DB-to-DB replication (storefront writes to ABERP's DuckDB
  directly).** Rejected — couples the storefront's schema to
  ABERP's tenant-isolation invariant (ADR-0002), and the
  storefront would need write access to a desktop-local DB.
  Operationally untenable.
- **File drop (storefront writes a JSON file to a shared
  directory, ABERP watches the directory).** Rejected — same
  inbound-surface problem as the webhook (the storefront would
  need to mount a directory on the ABERP machine). Also loses
  the audit-grade write-back path (the storefront has no
  feedback channel; ABERP-side adoption is silent on the
  storefront side).
- **Email-driven (storefront emails approved quotes; ABERP
  parses inbox).** Rejected — the email-parse layer is a
  whole own subsystem (auth, MIME, dedup, deliverability),
  and email is not a reliable transport for structured data.
  Stage 1's email path is one-way out (ADR for email send-side;
  no inbound parsing).

## Open questions

These do not block the 2.0 cutover; each is named so it does not
fall through.

- **CAD/CAM artifact cold archival.** Today the quote payload
  carries a pointer to the CAD/CAM bundle on the storefront; the
  daemon does not download it. When the storefront's retention
  policy ages out a quote's CAD bundle, the ABERP-side invoice
  loses cross-reference depth. Phase 4 of `docs/e2e-shop/ground-
  zero.md` is the named integration point; the trigger to file an
  ADR is the first PR that downloads CAD bundles into the ABERP
  tenant.
- **Multi-operator token rotation.** Per §"Trade-offs". Trigger:
  first deployment with more than one operator per tenant. The
  ADR amends or supersedes this one at the §"Auth" sub-clause.
- **Status write-back retry exhaustion.** Today a failed write-
  back retries indefinitely (the row's `status_writeback_at`
  stays NULL, the next cycle reattempts). A retry-failure
  alert threshold would land if it surfaces in operator pain
  (recurring `writeback_failed > 0` cycle entries). Out of
  scope for 2.0.

## Invariants pinned

- The daemon NEVER writes to the `invoice` table. The
  `prepared_draft` JSON in `quote_intake_log` is the operator's
  pre-filled form; the existing `issue-invoice` pipeline (sequence
  burn + audit chain + NAV submission) stays the only path that
  touches the regulated `invoice` row.
- `quote_id` is the PRIMARY KEY of `quote_intake_log`. Re-fetch of
  an already-staged quote is a no-op INSERT (idempotency at the
  schema layer, not the application layer).
- The bearer token is `Zeroizing`-wrapped on the read path (env
  load, keychain load, transport hand-off). The SPA settings card
  is write-only (never reads the token back from keychain).
- The daemon spawns at `aberp serve` boot with precedence env >
  toml+keychain > dormant. A misconfigured ENABLED=true (missing
  URL or token) refuses to start; it does NOT spawn a daemon
  pointing at a wrong URL.
- The daemon does NOT hot-reload; toml/keychain edits take effect
  on the next `aberp serve` boot. The SPA settings card surfaces
  a restart-required banner.
- One `system.quote_intake_poll_completed` audit entry per poll
  cycle. Per-quote forensic depth lives in the staging row's
  `raw_payload`, not the ledger.
- The `[quote_intake]` section in `seller.toml` is the sixth slot
  of the seller.toml preservation invariant (identity / banks /
  smtp / numbering / branding / quote_intake). Per ADR-0055 and
  the memory [[seller-toml-write-invariant]].
- The Quotes / Ajánlatok operational tab is mounted as the third
  segmented-control tab under `#/invoices` (alongside Outgoing
  + Incoming per S179). The tab is always visible; an empty
  staging table renders the empty-state copy.
- The 2.0 trigger fires per ADR-0056: new module = new routes
  (`/api/quote-intake/*`) + new schema (`quote_intake_log`) + new
  audit kind (`QuoteIntakePollCompleted`). All three hold.
