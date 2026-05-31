# aberp-quote-intake

Sister-service quote-intake daemon — pulls **approved quotes** from an
ABERP-site storefront and stages them as **pending operator-pickup
rows** in a per-tenant `quote_intake_log` DuckDB table. S211 surfaces
the queue in the SPA; S212 ships ADR + the 2.0 release.

**S210 / PR-204** — first backend module of the **2.0 cutover** strand
(Stage 2 per ADR-0056 + [[aberp-versioning-policy]]).

## Why a separate crate

Auto-creating sequence-burned invoices from background polled data
would be irreversible and would couple a remote ABERP-site outage to
the regulated `invoice` table. This crate stages quotes in its own
table; the operator-clicked pickup (S211) routes them through the
normal `issue_invoice::run` allocator with all preflight checks.

## What one cycle does

1. `GET /api/quotes?status=approved` on the sister service.
2. For each fetched quote:
   - Skip if `quote_id` is already in `quote_intake_log`.
   - Map to a `PreparedDraft` JSON (one placeholder line at 0 HUF,
     today + 30d payment deadline, today's delivery date, HUF
     currency, suggested partner from `contact.{name,email,company}`,
     per-invoice notes from material/qty/notes + quote id).
   - Insert into `quote_intake_log` (raw payload + prepared draft).
   - `POST /api/quotes/<id>/status {"status":"invoiced", …}` —
     best-effort.
3. Retry NULL-writeback rows from PRIOR cycles (snapshot before the
   per-quote loop).
4. Emit one `system.quote_intake_poll_completed` audit entry when
   the cycle saw work (fetched > 0, failures, or error). Pure-zero
   no-op cycles are silent.

## Configuration (env vars)

| Var | Default | Description |
| --- | --- | --- |
| `ABERP_QUOTE_INTAKE_ENABLED` | `false` | `true` to spawn. |
| `ABERP_QUOTE_INTAKE_URL` | — | Base URL (no trailing slash). Required when enabled. |
| `ABERP_QUOTE_INTAKE_TOKEN` | — | Bearer token. Required when enabled. **Never logged.** |
| `ABERP_QUOTE_INTAKE_INTERVAL_SECS` | `60` | Cadence, clamped `[10, 3600]`. |

Refuse-to-start: `ENABLED=true` + missing/empty URL or TOKEN aborts
the `aberp serve` boot.

## Endpoints consumed (all bearer-authed)

- `GET  /api/quotes?status=approved`           — list (every cycle).
- `GET  /api/quotes/<id>`                       — single (reserved for a future SPA action; defined on transport for symmetry).
- `GET  /api/quotes/<id>/files/<filename>`      — **NOT consumed.** Files stay on ABERP-site.
- `POST /api/quotes/<id>/status`               — writeback.

## `quote_intake_log` schema

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
```

No CHECK constraints (per `[[no-sql-specific]]`); `quote_id` PRIMARY
KEY is the idempotency anchor.

## Local-dev test recipe (NOT for prod)

1. `npm run dev` ABERP-site somewhere — note its `ABERP_SITE_ADMIN_TOKEN`.
2. Export env in the shell that runs `aberp serve`:

   ```bash
   export ABERP_QUOTE_INTAKE_ENABLED=true
   export ABERP_QUOTE_INTAKE_URL=http://localhost:3000
   export ABERP_QUOTE_INTAKE_TOKEN=<same as ABERP-site>
   export ABERP_QUOTE_INTAKE_INTERVAL_SECS=30
   ```

3. Launch `aberp serve`. Watch for `spawning quote-intake daemon (S210 / PR-204)`.
4. Submit + approve a quote in ABERP-site's admin UI.
5. Within `INTERVAL_SECS`, daemon logs the cycle + `quote_intake_log` carries one row.

## Dev nuke recipe (NEVER on prod)

```sql
DROP TABLE quote_intake_log;
```

## What this crate DELIBERATELY does NOT do

- Touch the canonical `invoice` table (sequence burn stays
  operator-gated).
- Log the bearer token.
- Copy CAD files into ABERP (files stay on ABERP-site).
- Fan out per-quote `GET /api/quotes/<id>` (the list inlines what we
  need).
- Emit audit entries for no-op cycles.

## What lands in S211 / S212

- **S211:** SPA queue UI, tenant-settings UI for the config
  (env vars → keychain + DB), pickup action that opens
  `InvoiceCompose` modal pre-populated from `PreparedDraft`.
- **S212:** ADR, version bump to `PROD_v2.0.0`, release.
