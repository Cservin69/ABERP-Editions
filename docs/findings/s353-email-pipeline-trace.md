# S353 — Customer quote-email pipeline trace

**Session:** S353 / PR-40 (`session-353/pr-40-email-pipeline`)
**Date:** 2026-06-11
**Trigger:** Ervin — "the customer-facing quote email definitely not worked." After a successful priced-writeback the customer should receive an email carrying the PDF (and the accept/DEAL link). It never landed.
**Verdict:** **Root cause A — the email is purely downstream of priced-writeback success. The writeback failed, so the email was never enqueued. The email path itself is sound. No code change. Doc-only.**

---

## 1. The pipeline, end to end

The customer "your quote is ready" email is **enqueued by the storefront, not by ABERP.** ABERP touches email only as a poll-and-send drainer. Three actors:

```
 ABERP pricing pipeline                STOREFRONT                         ABERP poll daemon
 (quote_pricing_pipeline.rs)           (ABERP-site)                       (email_outbox_poll_daemon.rs)
 ─────────────────────────             ──────────────                     ────────────────────────────
 price the job
   │
   │  POST /api/quotes/<id>/priced  ──▶ persist priced.pdf
   │  (THE WRITEBACK)                   set status = 'quoted'
   │                                    sendPricedReadyEmail(updated)
   │                                      └─ enqueueEmail(..,'priced_ready')
   │                                         → queued/<ulid>.json on disk
   │                                                  │
   │  ◀── { status:"quoted" } ─────────────────────  │
   ▼                                                  │
 PostingBack → Posted                                 │
 QuotePricingPosted audit row                         │
                                                      │
                                          GET /api/internal/email-queue  ◀── poll every 5s
                                          POST .../<id>/claim
                                          ◀──────────────────────────────  send via SMTP (lettre)
                                          POST .../<id>/sent | /failed
                                                      │
                                          quote.email_outbox_{fetched,claimed,sent,failed}
```

**The email is gated on the storefront's `/priced` handler executing.** That handler only runs if ABERP's writeback POST actually reaches the SvelteKit route and the price persists.

### Key citations

**ABERP — pricing pipeline (the writeback):**
- `apps/aberp/src/quote_pricing_pipeline.rs:974-1019` — priced-writeback success branch: `PostingBack → Posted`, emits `QuotePricingPosted`. **No email enqueue here.** ABERP's job ends at writeback.
- `apps/aberp/src/quote_pricing_pipeline.rs:1721-1795` — `classify_response_gate` (S347/S348): Content-Type gate before JSON parse → `WritebackOutcome::RoutingMisconfigured` when a `200 text/html` comes back (CloudFront S3 fallback).

**Storefront — where the email is actually enqueued:**
- `ABERP-site/src/routes/api/quotes/[id]/priced/+server.ts:341` — sets `status: 'quoted'` after persisting `priced.pdf`.
- `ABERP-site/src/routes/api/quotes/[id]/priced/+server.ts:360` — `await sendPricedReadyEmail(updated)`, fire-and-forget (a queue-write failure must NOT 500 the writeback; comment lines 355-358).
- `ABERP-site/src/lib/server/email.ts:462-509` — `sendPricedReadyEmail`: reads `priced.pdf` from disk, base64-attaches it, bilingual subject `"Ajánlat <shortId> — készen áll / Your quote is ready"`, CC operator, includes accept link + valid_until, then `enqueueEmail(req, 'priced_ready')`.
- `ABERP-site/src/lib/server/email-outbox.ts` — filesystem queue `queued/ → claimed/ → sent/|failed/` under `/home/aberp/data/email-outbox`.

**ABERP — the poll daemon that drains + sends:**
- `apps/aberp/src/serve.rs:1949-2005` — spawned at boot, registered `"email-outbox-poll"`, supervised.
- `apps/aberp/src/email_outbox_poll_daemon.rs:454-513` — `run_supervised` panic-catch (30s / 5min backoff).
- SMTP send via `lettre` (`send_via_smtp`, lines 952-1019), 30s timeout, reuses `[seller.smtp]` SPOC per `[[aberp-smtp-spoc]]`.

---

## 2. Why Ervin's quote produced no email

Quote `tjmc1wb5hbyydph` audit chain (from the screenshot):

```
Beérkezett → CAD-extract → Árazva → PDF kész → Visszaküldés (FAILED) → Hiba besorolva
                                                 └ writeback             └ failure classified
```

**The writeback (Visszaküldés) failed.** ABERP's `POST /api/quotes/<id>/priced` never landed a `2xx {status:"quoted"}` from the storefront SvelteKit handler — it hit the S347/S351 `routing_misconfigured` mode (operator `base_url` had a trailing `/` → `//api/...` → CloudFront served the S3 SPA fallback HTML instead of routing to the handler; see `[[project_aberp_s351_writeback_url_trailing_slash]]`).

Because the storefront `/priced` handler **never executed**, it never persisted the price, never set `status='quoted'`, and never called `sendPricedReadyEmail`. **No outbox entry was ever created.** The ABERP poll daemon therefore had nothing to drain — which is exactly why there is **no `quote.email_outbox_*` event in the chain.** The absence is correct, not a bug: there was nothing to send.

This is textbook **Root cause A**: the email is downstream of writeback success, and the writeback never succeeded.

---

## 3. The fix already shipped — S351

`[[project_aberp_s351_writeback_url_trailing_slash]]` (the trailing-slash hotfix) introduced a pure `resolved_writeback_url(base, quote_id, suffix)` that trims the operator `base_url` before formatting, applied at all three storefront writeback sites (priced / status / rerender). Once that is live in prod and Ervin retries, the writeback should reach the real SvelteKit handler, persist the price, and the storefront will enqueue the email — which the already-running poll daemon will then send.

**No email-side code is broken. Nothing to fix in this session.**

---

## 4. Audit-event coverage — already covered; phantom events deliberately NOT added

The brief asked to add `quote.email_enqueued` / `quote.email_sent` / `quote.email_failed` if missing. **Investigation shows the coverage already exists, and the missing one would be a phantom. No EventKinds were added.** Reasoning:

| Proposed kind        | Status | Reason |
|----------------------|--------|--------|
| `quote.email_sent`   | **Already exists** as `quote.email_outbox_sent` (`event_kind.rs:1862/2022`) — emitted by the poll daemon on SMTP success. |
| `quote.email_failed` | **Already exists** as `quote.email_outbox_failed` (`event_kind.rs:1879/2023`) — emitted on SMTP retry-exhaustion. |
| `quote.email_enqueued` | **Would be a phantom on ABERP.** The enqueue happens entirely on the storefront's filesystem queue (`email-outbox.ts`); ABERP has no enqueue site and never sees the event. An ABERP EventKind with no producer is exactly the "phantom code surface" the brief forbids. The storefront's own enqueue is observable storefront-side (entry `audit_id`, `submitter:'priced_ready'`). |

The full ABERP-side email lifecycle is already audited by the `quote.email_outbox_*` family (CLAUDE.md #13 — don't add a kind that shouldn't exist):

- `quote.email_outbox_fetched` (`event_kind.rs:1826`) — poll cycle (S335-throttled when idle)
- `quote.email_outbox_claimed` (`event_kind.rs:1845`)
- `quote.email_outbox_sent` (`event_kind.rs:1862`)
- `quote.email_outbox_failed` (`event_kind.rs:1879`)

These give complete next-time-debuggability on the only side ABERP touches email. Adding `email_enqueued` would require either inventing an ABERP enqueue (architecture change, out of scope) or emitting an event with no producer (forbidden). **Conservative call: add nothing; flag the existing coverage.** If end-to-end enqueue→send correlation is ever wanted, the right move is to surface the storefront entry's `audit_id` into the daemon's `claimed`/`sent` payloads — a future enhancement, not a new EventKind.

---

## 5. Recommendation for Ervin

1. **Upgrade prod to the build carrying the S351 trailing-slash fix**, then re-run a quote end to end.
2. On a clean run, the audit chain should extend past `Visszaküldés` to `Posted`, and within ~5s you should see `quote.email_outbox_fetched` → `…_claimed` → `…_sent`. The customer (CC operator) gets the PDF email.
3. If `…_sent` does **not** appear after a successful writeback, the failure has moved into the daemon/SMTP leg — check `[seller.smtp]` creds and `quote.email_outbox_failed` payloads (`error_class` / scrubbed detail).

### Monitoring caveat — the idempotency edge (worth watching)

The storefront `/priced` handler short-circuits when the quote is **already** `'quoted'` (`+server.ts:291` → returns `{status:'quoted', idempotent:true}` at line 314) and in that branch it **does not** re-call `sendPricedReadyEmail`. Ervin's case is safe because the failed writeback (CloudFront S3 fallback) never reached the handler, so the storefront status was never set to `'quoted'` — the retry will take the normal persist+enqueue path.

But a sharper failure could dead-end the email: if the storefront handler ever **commits** the price (`status='quoted'`) yet ABERP reads the response as a failure (e.g. body truncated, or HTML injected *after* commit), then ABERP's operator-Retry would hit line 314's idempotent early-return and **skip the email enqueue permanently.** This is not Ervin's situation (his request never reached the handler) and is out of scope here, but it is the one real way the email path can silently swallow a send. Flagging for a future storefront hardening: move the `sendPricedReadyEmail` call above / outside the idempotency guard, or enqueue on the idempotent branch too.

---

## Summary

- **Root cause: A.** Email is downstream of priced-writeback success; the writeback failed (`routing_misconfigured`, trailing-slash), so the storefront never enqueued. No email-side defect.
- **No code change.** Fix already shipped as S351; verify after Ervin's next quote on the upgraded prod build.
- **No EventKinds added.** `sent`/`failed` already exist as `quote.email_outbox_*`; `email_enqueued` would be a phantom (storefront-owned enqueue). Conservative call, flagged.
- **One flagged future risk:** storefront `/priced` idempotency guard skips the email on the already-`quoted` branch — harden storefront-side if a commit-then-fail race ever appears.
