# S339 / PR-24 — catalogue-push storefront auth contract

**Session:** S339 · **Branch:** `session-339/pr-24-catalogue-auth` ·
**Date:** 2026-06-10 · **Repo:** ABERP only (no storefront code change)

This is the **second** pilot blocker after S338's `GRADE_RE` fix. The
goal: make ABERP's `catalogue_push.rs` actually deliver the material
snapshot to the storefront so the `/quote` dropdown populates with real
grades.

---

## 1. Storefront's exact auth contract (verified cross-repo)

Read of `ABERP-site/src/hooks.server.ts`, `src/lib/server/auth.ts`, and
`src/routes/api/catalogue/materials/+server.ts`. The catalogue receiver
enforces a **dual gate**, in this order:

| # | Where | Check | Failure |
|---|-------|-------|---------|
| 1 | `hooks.server.ts` (global `handle`) | `X-CloudFront-Secret` header **string-equals** `CLOUDFRONT_SHARED_SECRET` env | `403 "forbidden: missing origin signature"` |
| 2 | `+server.ts` → `requireAdminAuth` | `Authorization: Bearer <ABERP_SITE_ADMIN_TOKEN>` | `401 {"message":"Unauthorized"}` |

**The "origin signature" is NOT an HMAC.** Despite the error string, the
check is a `timingSafeEqual` of a **static shared-secret header** —
there is no signing key, no canonical request string, no timestamp, no
replay window (`hooks.server.ts:11-16,70-76`). The brief's Part A
("HMAC-SHA256 of a canonical request string") **does not match reality**,
so no HMAC signer crate was built.

CloudFront injects `X-CloudFront-Secret` on origin requests, so traffic
that traverses CloudFront passes gate 1 automatically. But CloudFront
behaviours are **per-path** (see `docs/reviews/S249-...` finding 23 — the
distribution routes different paths to different origins/behaviours), so
a `PUT /api/catalogue/materials` that reaches the origin on a behaviour
that does **not** inject the header arrives bare → **403**.

Gate-2 token: the storefront uses **one** secret, `ABERP_SITE_ADMIN_TOKEN`,
for *every* admin route (catalogue, quotes, priced, status,
email-queue). There is no separate catalogue bearer.

---

## 2. What ABERP sent before S339 vs what storefront requires

| Header | Storefront requires | ABERP sent (pre-S339) | Delta |
|--------|---------------------|------------------------|-------|
| `Authorization: Bearer …` | gate 2 | ✅ from `StorefrontCredentialHandle` | none |
| `X-CloudFront-Secret` | gate 1 (when origin not behind CloudFront-inject) | ❌ never sent | **the gap** |
| `Content-Type: application/json` | (body parse) | ✅ | none |
| body `{materials:[…]}` | `validateSnapshotBody` | ✅ shape matches exactly | none |

Wire-shape confirmed identical both sides:
`{grade, display_name, stock_status, lead_time_default_days}`
(ABERP `quoting_materials::PublicMaterial` ↔ storefront
`CatalogueMaterial`). S338 already relaxed `GRADE_RE` to accept real
grades, so gate-after-auth (`400`) is closed.

### Why the prod symptom was `unexpected_status`, not `unauthorized`

The daemon maps **401 → `unauthorized`**, **any other non-2xx →
`unexpected_status`** (`catalogue_push.rs` `push_once`). The observed
prod outcome was `unexpected_status`, i.e. **NOT 401** — consistent with
a **403** (missing origin header) and/or the pre-S338 **400** (grade
reject). It is *not* consistent with the bearer being wrong.

### Bearer was never the gap — proven by the email-outbox daemon

`email_outbox_poll_daemon.rs` shares the **same**
`StorefrontCredentialHandle` and sends **only** `Authorization: Bearer …`
(no `X-CloudFront-Secret`), yet it successfully fetches from the
storefront in prod (S335 throttled its *successful* `EmailOutboxFetched`
emits). So the bearer is correct and gate-1 is satisfied **on the path
the email daemon uses**. The catalogue push uses the identical handle →
the bearer cannot be the catalogue gap; the only credible delta is the
per-path origin-header injection.

---

## 3. The fix (ABERP-side, surgical, additive, reversible)

1. **`storefront_origin_secret.rs`** (new) — optional origin shared
   secret in the OS keychain (`service aberp.storefront.<tenant>`,
   account `storefront_origin_secret`) with an env override
   `ABERP_STOREFRONT_ORIGIN_SECRET`. `resolve()` returns `Option`;
   missing entry → `None`, backend error → WARN + `None` (boot never
   aborts on a flaky keychain).
2. **`catalogue_push.rs`** — `CataloguePushDeps.origin_secret:
   Option<Zeroizing<String>>`; when `Some`, `push_once` attaches
   `X-CloudFront-Secret`. When `None` (today's default) the request is
   **byte-for-byte the pre-S339 push** — no behaviour change unless the
   operator provisions the secret. The bearer is unchanged (shared
   handle).
3. **`serve.rs`** — boot resolves the secret once and sets it on
   `push_deps`; one-shot INFO names whether the header is in play.
4. **Maintenance dashboard** — the Material-catalogue tile now appends
   the live push status (`Pushed to storefront ✓` / `Push failing — see
   operator log ⚠` / `Push paused — re-paste bearer ⚠` / `Storefront not
   configured` / `Pending push`) derived from the daemon's recorded
   `CataloguePushStatus`. It previously showed only the grade count —
   green even while every push 403'd.

### Why optional, not mandatory (conservative call)

Making the header mandatory would **break** the currently-working
CloudFront path for any operator who hasn't provisioned the secret.
Optional means: zero regression, and the direct-origin 403 closes the
moment the operator provisions. Per `[[no-ask-user-question]]` this is
the most-reversible choice.

### Why the secret is NOT in `StorefrontCredentialHandle`

The origin secret is **deploy-infra** (a CloudFront↔origin shared
secret), not operator-SPA-editable, so it does not need the
hot-reloadable handle. Keeping it in `CataloguePushDeps` avoids rippling
the shared snapshot into the email daemon, the quote-intake crate, and
~16 AppState test constructors (the S289 ripple). Surgical per CLAUDE.md
#3.

---

## 4. ⚠ Operator provisioning required for the direct-origin path

If the prod push is 403-blocked (per-path CloudFront gap), the fix takes
effect **only once the operator provisions the secret** into ABERP. Two
options, in order of preference:

**Option A (preferred) — fix CloudFront so it injects the header on the
catalogue behaviour.** Then ABERP needs nothing and the secret never
leaves AWS. This is a storefront/AWS-infra change, explicitly **out of
scope** for this PR (see brief: "If we change the auth scheme on
storefront … that's a separate session").

**Option B — provision the secret into ABERP's keychain** so the push
carries it itself:

```sh
# value = the storefront's CLOUDFRONT_SHARED_SECRET (from AWS/Lightsail env)
security add-generic-password \
  -s "aberp.storefront.<TENANT_ID>" \
  -a "storefront_origin_secret" \
  -w "<CLOUDFRONT_SHARED_SECRET_VALUE>" -U
```

(or set `ABERP_STOREFRONT_ORIGIN_SECRET=<value>` for a dev-test run).
Then restart `aberp serve`; the boot INFO line will read
`origin_secret_present=true`.

**First check before either:** confirm the catalogue base URL is the
**public CloudFront URL** (`https://abenerp.com`), not a direct-Lightsail
or localhost URL — Maintenance → Quote Intake → Base URL. A
direct-origin base URL is the most likely root cause of the 403.

---

## 5. Tests

ABERP (`apps/aberp/tests/s339_catalogue_push_auth.rs`, hand-rolled mock
storefront):

- `s339_catalogue_push_signs_request_with_origin_signature` — header sent verbatim when provisioned.
- `s339_catalogue_push_omits_origin_header_when_unprovisioned` — additive/reversible (no header when `None`).
- `s339_catalogue_push_uses_storefront_credential_handle_bearer` — bearer sourced from the shared handle.
- `s339_catalogue_push_returns_success_against_test_storefront` — 2xx → `Ok` + `ok` audit row.
- `s339_catalogue_push_audit_records_http_400_when_storefront_rejects` — 400 → `UnexpectedStatus(400)` + `unexpected_status` audit row.

Plus unit pins (`catalogue_push.rs` header-name const; `storefront_origin_secret.rs`
service-name non-collision) and 7 vitest cases for `renderPushStatusSuffix`.

---

## 6. Flagged conservative calls

1. **No HMAC signer** — storefront uses a static shared header, not HMAC.
   The brief's Part A premise was incorrect.
2. **No storefront code/ADR change** — kept this an ABERP-only PR to
   avoid triggering the storefront deploy pipeline for a doc. **Follow-up
   recommended:** a short storefront ADR publicly documenting the
   dual-gate so future ABERP devs don't reverse-engineer it from a curl
   403.
3. **Origin secret optional + operator-provisioned** — not auto-enabled;
   requires the steps in §4. Flagged loudly because the fix is inert
   until provisioned (if the 403 is the live cause).
4. **No new auto-bearer** — reused the existing shared handle; did not
   mint a second catalogue token (the storefront has only one admin
   token anyway).

The audit-ledger ART crash is a **separate** open issue; S339 does not
touch it.
