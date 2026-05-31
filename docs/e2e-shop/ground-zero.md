# e2e-shop — Ground-Zero Design

**Status:** Draft v0 — ground-zero design only, no code. Session 199 / PR-199.
**Base commit:** `b3c8df9` (S198 ADRs).
**Author session:** S199.
**Companion doc:** [`docs/research/aberp-erp-roadmap.md`](../research/aberp-erp-roadmap.md) (Stage 2 first module — pending; this doc is the kickoff).
**Author of vision:** Ervin Áben.

> Custom shop with landing page, then an ordering/quoting page where clients can upload the CAD, select features, and receive quotes based on CAM assessment, product/stock prices, manufacturing time, complexity, and transport distances. Lets start with the easiest. Landing page with WebGL using the codrops RainEffect.
>
> — Ervin, S198 prep

This doc is **the ground floor**. Every later phase gets its own design doc + ADR(s) + PR(s). Nothing here is committed beyond brand-direction recommendations Ervin can overturn with a sentence.

---

## 1. Vision & Stage

ABERP shipped its **Stage 1** scope (S172–S198): a single-tenant Hungarian e-invoicing + AP-side ledger with NAV-as-DR restore. That stack is now stable enough to be the *back office* for a customer-facing storefront.

**Stage 2** is the **e2e shop**: a public website where prospective customers can browse Áben's manufacturing offering, upload a CAD file, configure features, and receive a quote. The quote becomes an order, the order eventually flows into ABERP as an invoice.

This doc covers **the very first step**: a public-facing landing page on the chosen domain, with the codrops rain-effect background, that signals "we exist, here's what we do, leave your email." Everything downstream (quoting, CAM, payments, ABERP integration) is sketched at the architecture level but explicitly out of scope for the first PR strand.

The work cadence mirrors ABERP: one PR per session, one ADR per architectural choice, an end-to-end working artifact at every checkpoint.

---

## 2. Domain decision: `friboard.com` vs `aben.ch`

| | aben.ch | friboard.com |
|---|---|---|
| Owned | yes | yes |
| Current traffic | "no one uses it" (Ervin) | none — domain serves nothing |
| Existing brand association | **Áben Consulting — Investments & AML & Compliance & IT** (rain-effect template already live) | clean slate, no brand baggage |
| Template currently deployed | codrops rain-effect already wired (WebGL fallback string visible) | none |
| Risk of disrupting existing visitors | low (Ervin: nobody) but non-zero (search-engine cache, old contacts) | zero |
| Brand fit for CAD/manufacturing | poor — AML/compliance ≠ manufacturing; rebrand would confuse anyone who *did* land | excellent — "board" suffix hints at PCB, woodwork, sheet-metal, CAM workpieces |
| SEO history | likely indexed under consulting/compliance keywords | none — clean slate, no negative ranking baggage |
| Continuity story | "we pivoted" — bad signal to consulting customers if any remain | none needed |

### Recommendation: **friboard.com**

The brand fit is decisive. Keeping `aben.ch` as the consulting site (low maintenance, ignore it, let it 404 gracefully when the cert expires if Ervin ever decides) and putting the manufacturing storefront on `friboard.com` is the cleanest separation. A future Áben-group page (`aben.group` or similar holding-page) can link to both ventures if needed.

The rain-effect template that's already on `aben.ch` can be **copied verbatim into the new friboard codebase** and then re-themed — Ervin has the working integration as a reference, so we benefit from that prior work without inheriting the brand confusion.

**Decision deferred to Ervin** in §12 Q1, but everything below assumes friboard.com unless overruled.

---

## 3. Brand + positioning

**5-second visitor read** (the only test that matters for a landing page):

> "Friboard makes custom manufactured parts. Upload your CAD, get a quote, get it made."

That sentence has to fall out of the visual hierarchy in two seconds, the headline copy in three, and the CTA placement in five.

### Tagline candidates

| | EN | HU |
|---|---|---|
| **A.** "Your CAD. Your part. Made." | "A te terved. A te alkatrészed. Legyártva." | direct, confident, low-promise |
| **B.** "From file to finished part." | "A fájltól a kész alkatrészig." | journey-framed; works for non-CAD-fluent buyers too |
| **C.** "Upload. Quote. Manufacture." | "Töltsd fel. Árajánlat. Gyártás." | imperative triple; matches the actual app flow |

**Recommendation: C** for the hero, **B** for the meta-description / OG card. C reads like a product, B reads like a value-prop — both are useful in different surfaces. A is the runner-up if Ervin prefers a more declarative tone.

### Implicit promise

The landing page implicitly promises:

1. **CAD upload works** — drag, drop, done. No emailing files back and forth.
2. **Quote in minutes** *(eventually)* — for Phase 1 it's hours-to-a-day with operator-in-the-loop. The page should not over-promise auto-quote until it actually exists.
3. **Manufacturing on demand** — small batches, short lead times. Áben is not selling against Alibaba on volume; the positioning is fast, custom, EU-based.

### Tone

Industrial, precise, calm. Not "disruptive startup," not "boutique craftsmanship." Closer to **Bosch's professional tools page** than to **Etsy**. The rain effect carries the mood — atmospheric, technical, premium-without-shouting.

---

## 4. MVP scope

Three cuts, smallest first:

### Cut A — "Smallest" (recommended first ship)

- One page. Brand wordmark. Hero tagline. RainEffect WebGL background.
- One CTA: **email capture** ("Get notified when quoting opens").
- Bottom: 2-3 sentence "what we do" paragraph, contact email.
- That's it. No nav, no forms beyond the input.

**Why this cut.** It's small enough to ship in one session, validates the rain-effect adaptation, gets the domain into a "real" state, and starts collecting waitlist emails — which is the only useful metric before there's an actual quoting flow.

### Cut B — "Middle"

Everything in A, plus a **"Get a quote" CTA** that opens a placeholder form: name, email, "describe your project," file-attach disabled with a "coming soon" badge. Form submissions go to Ervin's email via SMTP (reuses ABERP's `[seller.smtp]` cache pattern — see [[project_smtp_secrets_cache_boot]]).

**Why not start here.** The placeholder form is more honest than a fake one, but the email capture in Cut A already covers the "let me know when this is real" use case. The form adds one more shipping risk without proportional value.

### Cut C — "Ambitious"

Everything in B, plus **actual CAD file upload** (`.step`, `.stp`, `.iges`, `.stl`, `.dxf`, `.dwg`), stored in object storage, with an email notification to Ervin containing a signed download link.

**Why not start here.** Real CAD upload introduces: GDPR retention policy, NDA copy in T&Cs, object-storage choice, file-type validation, virus scanning, signed-URL infrastructure. Each is a real decision. Phase 2 work, not Phase 1.

### Recommendation: **ship Cut A as PR-200 (next session)**.

Cut B becomes its own design doc when Phase 2 starts. Cut C becomes Phase 3.

---

## 5. Stack

| | SvelteKit | Next.js | Vanilla TS + Vite |
|---|---|---|---|
| Time-to-launch for Cut A | fastest — single `+page.svelte`, file-routing for future quote form | moderate — App Router config overhead | fast — but you'd rebuild routing later |
| Team continuity with ABERP | high — ABERP SPA is Svelte (`apps/aberp-ui/`); same idioms, same tooling | none — fork in mental model | partial |
| Future quote-engine integration | API routes + SSR for SEO + form actions for upload — native | same, plus larger ecosystem | DIY for everything |
| Hosting options | Vercel, Cloudflare, Netlify, self-hosted Node adapter | Vercel-first; others possible | any static + small backend |
| WebGL integration | irrelevant — works the same in any of them | same | same |
| Ecosystem size | smaller but sufficient | largest | n/a |
| Bundle size for landing page | smallest | largest by default | smallest |
| Risk of overkill for Cut A | low | medium | low |

### Recommendation: **SvelteKit**.

Continuity with ABERP is the deciding factor. Same components mental model (Svelte 5 runes), same Vite tooling, same `vitest` test runner ([[project_invoice_list_persistence_s175]] pattern carries over verbatim for any client-side persistence in the quote form). When the quote form needs to call ABERP's future `/api/orders/from-friboard` route, the fetch idioms are identical to the SPA's existing API calls.

Vanilla TS would also work for Cut A, but the moment Cut B lands you'd be reinventing routing, form actions, and SSR. Next.js's ecosystem advantage doesn't apply here — the page has one form and a WebGL canvas, not a CMS.

---

## 6. WebGL RainEffect adaptation

### License

**Codrops custom license.** The README states (verbatim phrasing confirmed via web fetch):

> "Integrate or build upon it for free in your personal or commercial projects. Don't republish, redistribute or sell 'as-is'."

**Translation:** integrating it into friboard.com (a commercial site) is explicitly allowed. We must not republish it as a standalone library or sell the effect itself. We're modifying it (theming, asset swap, integration into a Svelte component) so we satisfy the "build upon" clause naturally.

A short attribution comment in the source file ("Rain effect adapted from codrops/RainEffect, https://github.com/codrops/RainEffect") is good faith and costs nothing — recommended even though the license doesn't strictly demand it.

### Integration path

| | Verbatim copy | npm-package wrapper | Rewrite from scratch |
|---|---|---|---|
| Effort | small | n/a — no package exists | large |
| Maintenance | we own a 2015 codebase | n/a | we own it cleanly |
| Modernization | optional — port from Gulp to ESM | n/a | inherent |
| Risk | low — proven effect, just glue-code | n/a | shader bugs, time |

**Recommendation: verbatim copy of the rendering core, with a Svelte wrapper.**

Concretely:

1. Copy `src/` from `codrops/RainEffect` into `friboard-site/src/lib/rain-effect/`.
2. Delete the Gulp/build pipeline — Vite handles bundling now.
3. Drop the legacy module shape (browserify-era CommonJS) in favor of ES modules. This is mechanical — the effect itself is shader code + canvas plumbing, not framework-coupled.
4. Wrap the canvas init in `src/lib/RainCanvas.svelte` exposing props for `imageUrl`, `dropSize`, `intensity`.
5. Use it from `src/routes/+page.svelte` with the Áben/Friboard brand image as `imageUrl`.

### Background image source

Three options:

- **Áben-photographed workshop / part / machine** — most authentic, requires Ervin's camera time. Best for the "we actually make things" signal.
- **Generic industrial Unsplash/Pexels image** — fastest, but smells generic. Acceptable as a placeholder while real photography is commissioned.
- **Abstract / blueprint** — safe, but doesn't differentiate from competitors.

**Recommendation: option 1 (Áben-photographed)** — flagged as a §12 open question because it depends on what visual material Ervin already has.

The image gets darkened (~50% black overlay) so foreground copy reads at WCAG AA contrast over the busiest pixels of the photo. The rain droplets refract the darkened image — the original is still legible through the drops, the overlay just enforces text contrast.

### Performance / mobile fallback

- **WebGL feature-detect** before initializing. If absent, render a static darkened still image with the same composition. The `aben.ch` template already does this (the "Sorry, but your browser does not support WebGL!" string is the current fallback — we replace it with the static image, no error text).
- **Mobile**: WebGL is fine on iOS 14+/Android Chrome, but the effect is GPU-heavy. Use `matchMedia('(max-width: 640px)')` to drop to the static fallback on small screens — saves battery, no visual regression because mobile users scroll past the hero in 1-2s anyway.
- **Reduced-motion**: `prefers-reduced-motion: reduce` → static fallback regardless of device. Required for AA accessibility.
- **Frame budget**: target 60fps on a 2020 MacBook Air, accept 30fps on a 2019 mid-range Android. The original codrops demo hit those targets in 2015 hardware — we're well over the line.

---

## 7. Architecture sketch

### Repo location

| | Separate repo (`friboard-site`) | Monorepo addition under ABERP |
|---|---|---|
| Concern separation | clean | mixed (ERP + storefront in one tree) |
| Deploy independence | full | requires path-filter CI |
| Secret scope | per-repo | shared `.env` blast-radius |
| ABERP integration path | HTTPS API | could be direct DB / IPC (tempting but bad) |
| Backup posture | per-repo | one backup, both concerns |
| Reviewability | small PRs visible against a small repo | risk of friboard PRs touching ABERP files |

**Recommendation: separate repo `friboard-site`.**

The temptation in a monorepo is to import ABERP code directly ("just call this Rust function from the storefront") — which then creates a coupling that bites the moment Friboard scales differently from ABERP or wants to deploy to a different region. HTTPS at the boundary is a one-time cost that pays back forever.

The repo lives next to ABERP on Ervin's machine: `~/Documents/Claude/Projects/Friboard/`. Same CLAUDE.md discipline (13 principles), same auto-memory (separate index file), same session-numbered PRs starting at PR-1.

### ABERP integration (future)

When Phase 4 lands (approved quote → ABERP invoice), the contract between the two systems is one HTTPS endpoint:

```
POST https://aberp.local-or-prod/api/orders/from-friboard
  X-Friboard-Signature: hmac-sha256(body, shared_secret)
  Content-Type: application/json
  {
    "friboard_order_id": "FB-2026-0001",
    "customer": { ... ABERP partner shape ... },
    "lines": [ { product, qty, unit_price_huf, ... } ],
    "due_at": "...",
    "notes": "..."
  }
  → 201 { "aberp_invoice_id": "...", "aberp_invoice_number": "..." }
```

Signed with a shared HMAC (configured once, never rotated automatically — manual rotation only). ABERP creates a `DRAFT` invoice in its existing pipeline; Ervin reviews and issues it via the existing SPA. Friboard then receives a webhook back when the invoice is issued, and surfaces "your invoice is ready" to the customer.

**Important:** Friboard does **not** write directly to the ABERP DuckDB. The four-way `seller.toml` write invariant ([[project_seller_toml_write_invariant]]) and the audit-ledger discipline make ABERP very intolerant of side writes. HTTPS is the only contract.

This contract is sketched here, **not built**. It's a §12 placeholder for a future ADR (likely ADR-0056 or higher).

### Phase-1 deployment topology

```
  visitor browser
       │
       ▼  HTTPS
  friboard.com (Vercel edge)
       │
       └── form submit → email to Ervin (SMTP, no DB yet)
```

No database, no backend persistence, no auth in Cut A. Email-capture form posts to a SvelteKit form action which sends via SMTP. That's it.

---

## 8. Quote engine decomposition

This section is **future-facing**. None of it is in Cut A. It's here because Ervin's brief mentioned "CAM assessment and product/stock prices and manufacturing time, complexity and transport distances" — anchoring those variables early prevents later scope-creep surprises.

### Input variables to a quote

| Variable | Source | v1 (Phase 2) | v2 (Phase 3) | v3 (Phase 5) |
|---|---|---|---|---|
| **Material cost** | ABERP `product` table | operator looks it up | API call → ABERP `/api/products?material=...` | live API + alt-material suggestion |
| **Manufacturing time per feature** | CAM analysis of the CAD | operator estimates | feature-tier multiplier table (drill, mill, turn, weld) | full CAM toolpath simulation |
| **Complexity multiplier** | geometry analysis | operator-set tier (1/2/3) | rule-engine on geometry stats | ML model |
| **Transport distance** | customer postal code → workshop | operator estimates | OpenRouteService API | optimized multi-leg routing |
| **Stock check** | ABERP inventory | "we'll order it in" caveat | API call → ABERP `/api/products/:id/stock` (table not built yet) | live reserve-on-quote |
| **Currency conversion** | MNB rate | reuse existing ABERP MNB integration ([[project_mnb_endpoint_shift]], [[feedback_mnb_rates_walkback_on_404]]) | same | same |
| **Margin** | per-Áben policy | hardcoded operator markup | configurable tier | dynamic by demand/load |

### Phase split

- **Phase 2 quote engine = manual.** Customer submits CAD + intent → operator (Ervin) opens the file, fills in the variables in a back-office form, customer gets the quote by email. The "engine" is a spreadsheet-like form, not code.
- **Phase 3 quote engine = semi-automated.** The form pre-fills variables: material cost from ABERP API, transport distance from postal-code API, MNB FX from existing integration. Operator only adjusts complexity tier and manufacturing time. Approval still manual.
- **Phase 5 quote engine = automated.** CAM analysis library (likely Python or a CAM-as-service vendor — Xometry-style) returns feature counts and toolpath estimates. Auto-quote within a confidence band; operator review outside the band.

### Why this order

Phase 2 ships **business value with zero ML, zero CAM library, zero ABERP coupling**. It immediately validates whether anyone wants what Áben is selling — which is the highest-uncertainty assumption in the whole strand. If Phase 2 generates no real orders, building Phase 5 would be sunk cost.

Phase 3 only starts when Phase 2 has enough volume that operator time is a real bottleneck.

Phase 5 only starts when Phase 3's operator override rate is low (say, <20% of quotes get touched after pre-fill) — which means the deterministic rules are accurate and an ML model is worth the engineering investment.

---

## 9. Hosting + DNS

| | Vercel | Cloudflare Pages | Self-hosted on existing Áben infra |
|---|---|---|---|
| SvelteKit zero-config | yes | yes (with adapter) | DIY |
| Free tier | yes (within hobby limits) | yes (generous) | n/a — paid |
| EU regions | yes (Frankfurt edge) | yes | yes |
| GDPR data-residency | EU-only optional | EU-only optional | full control |
| Email-form action | edge function | Pages Functions | own server |
| DNS friction | NS-delegate to Vercel or A-record | similar | full control |
| Future serverless backend | clean | clean | DIY |

### Recommendation: **Vercel for Phase 1**.

Lowest friction, free until we exceed hobby limits, SvelteKit support is first-class. DNS: keep registrar (likely Gandi or similar — Ervin's call), point an `A` / `CNAME` record at Vercel per their wiring docs.

When the quoting backend grows beyond what edge functions handle (file upload, virus scan, CAM-service callouts), we revisit. Most likely answer at that point is a small **Hetzner VPS in Falkenstein** (EU, Áben is EU-based) running a Rust service that the SvelteKit edge calls. That's a Phase 3 decision, not now.

### DNS wiring sketch

- `friboard.com` → Vercel default (`A 76.76.21.21` or whatever Vercel's current edge IP is at deploy time — they document it).
- `www.friboard.com` → 301 to apex.
- SPF/DKIM/DMARC: configure for the SMTP sender domain so form-submit emails don't land in Ervin's spam. Likely Resend or Postmark for transactional sends; Vercel doesn't ship outbound SMTP.

---

## 10. Privacy + GDPR

Even a landing page incurs GDPR obligations the moment an EU visitor loads it.

### Minimum-viable compliance for Cut A

1. **Cookie consent.** If we only use strictly-necessary cookies (no analytics in Cut A), the consent banner is *not* required, but a short privacy notice is. Recommendation: skip analytics entirely in Cut A — defer to Phase 2 when there's something to measure.
2. **Email capture = personal data.** Need explicit consent (single checkbox, not pre-ticked) + a privacy policy linked at the form, naming controller (Áben Consulting / friboard operator), purpose ("notify you when quoting opens"), retention ("until you unsubscribe or 24 months, whichever sooner"), and data-subject rights.
3. **Unsubscribe link** in every notification email, one-click, no auth required.
4. **Hosting:** Vercel EU-only deploy (Vercel offers EU-region selection on Pro; Hobby tier may route through US — verify before launch; this is §12 Q3).

### Phase 2+ adds

1. **CAD file = trade-secret IP.** This is the privacy/GDPR area where Friboard's risk profile is unusual.
   - **Encryption at rest** on the object storage holding uploaded files (server-side encryption, dedicated key per customer if Phase 5).
   - **Retention policy:** delete uploaded CAD `N` days after quote-decision (accept/reject/timeout). Probably 30 days for active quotes, 7 days post-decision. NDA-style language in the T&Cs covering Áben's confidentiality obligation.
   - **Per-quote access log:** who at Áben opened the file, when. Defensive — protects Áben if a customer ever claims IP leak.
2. **Quote/order data** flows into ABERP. ABERP's existing GDPR posture (Hungarian e-invoicing has its own statutory retention) takes over.

### Recommendation

A barebones privacy policy + one consent checkbox is enough for Phase 1. Get a Hungarian-jurisdiction lawyer to review before Phase 2 lands (CAD uploads materially raise the stakes). The cost of getting GDPR wrong on a public site dwarfs the cost of one legal review.

---

## 11. Roadmap phases

Each phase = its own design doc + ADR(s) + one or more PRs. The doc/ADR/PR pattern matches ABERP S172–S198.

| Phase | Scope | Effort | Gating signal to start next phase |
|---|---|---|---|
| **1. Landing + email waitlist** | Cut A from §4. Static page, RainEffect, email-capture form action, SMTP send to Ervin. | 1-2 weeks (3-5 sessions) | At least 1 form submission, OR Ervin decides to proceed regardless. |
| **2. Quote form** | Cut B + Cut C. CAD upload (file → object storage), operator notification, operator manually quotes back via email. | 2-3 weeks | First paid manufacturing order. |
| **3. Semi-automated quote engine** | Variables wired (ABERP material/stock API, MNB FX, transport postal-code API). Operator approval still required. | 1-2 months | Operator override rate <20% on Phase 2 quotes. |
| **4. ABERP integration** | Approved quote → POST to ABERP `/api/orders/from-friboard` → ABERP draft invoice → operator issues via existing SPA. Webhook back to Friboard. | 2-4 weeks | Phase 3 stable; reproducible quote → invoice chain manually verified. |
| **5. Full CAM auto-quote** | CAM library or service for geometry analysis. ML or rule-engine for confidence band. Auto-quote within band, operator review outside. | Months — depends entirely on CAM choice. | Phase 4 volume justifying the engineering. |

### What is intentionally **not** on the roadmap

- **Customer login / account portal.** Quotes go by email until Phase 4 at the earliest. Accounts add auth, password reset, sessions, account-recovery support load — all overhead before there's a real need.
- **Payment processing.** Áben's existing bank-transfer-on-invoice flow continues. Adding Stripe before there's revenue is premature optimization.
- **Multi-tenant Friboard ("Friboard for other manufacturers").** Not a product strategy — Áben is the operator, not a SaaS vendor.

---

## 12. Open questions for Ervin

Listed for Ervin's review. None block Phase 1 start; default answers are noted where the cost of asking exceeds the cost of changing later.

1. **Domain confirmation.** Going with `friboard.com`? §2 recommends yes; if no, the doc reroutes to `aben.ch` with rebranding cost.
2. **Brand & visual assets.**
   a. Tagline preference — A, B, or C from §3 (default: C for hero, B for OG)?
   b. Do you have an Áben-photographed industrial image suitable for the rain-effect background? If not, are you OK with a Unsplash placeholder for v1?
   c. Wordmark / logotype — existing asset or do we need a typographic mark designed?
3. **Hosting & email.**
   a. Vercel EU-region deployment — Pro tier ($20/mo) for guaranteed EU residency, or Hobby tier accepting possible US routing for Phase 1?
   b. Transactional email sender — Resend, Postmark, or reuse ABERP's `[seller.smtp]` host? (Reusing SMTP keeps things simple but ties Friboard to Áben's SMTP credentials — minor coupling.)
4. **Repo location.** Separate `friboard-site/` next to ABERP, with its own CLAUDE.md + memory? §7 recommends yes.
5. **Privacy review timing.** OK to ship Phase 1 with a self-drafted minimal privacy policy and engage a HU lawyer before Phase 2 CAD uploads? Or do you want legal review *before* Phase 1?
6. **CAM long-term direction.** When Phase 5 enters scope, prefer building in-house (Python, geometry libraries, multi-year R&D) or buying via API (third-party CAM-as-service vendor)? Affects the Phase 4 ABERP-integration contract — buy-side likely streams more data through.
7. **Áben Group umbrella.** When friboard.com goes live, do you want a holding page at `aben.ch` (or new `aben.group`) linking Áben Consulting + Friboard, or keep them fully unlinked?

---

## Appendix A — Cross-references to existing ABERP work

These memories and docs already exist and inform Friboard design:

- [[aberp-erp-roadmap]] — overall Stage 2 framing (placeholder link; doc may be pending).
- [[project_smtp_secrets_cache_boot]] — if Friboard reuses ABERP SMTP, same boot-cache discipline applies.
- [[project_mnb_endpoint_shift]] + [[feedback_mnb_rates_walkback_on_404]] — MNB FX integration is reusable for Phase 3 quotes.
- [[project_seller_toml_write_invariant]] — explains why Friboard must talk to ABERP over HTTPS, never direct DB.
- [[project_invoice_list_persistence_s175]] + [[project_partner_product_list_persistence_s181]] — `localStorage` persistence pattern transfers to the quote form's "save as draft" later.
- ADR-0038 — preflight validator pattern, transferable to quote-form validation.
- ADR-0048 — PrivatePerson buyer modeling, transferable to non-tax-registered Friboard customers.

## Appendix B — What this doc explicitly defers

- No tech-stack decisions for Phase 3+ (queue, CAM service, background-job runner).
- No pricing of Áben's manufacturing services — that's Ervin's commercial call, not an engineering doc.
- No SEO/content strategy beyond "have a meta description."
- No analytics tooling — Phase 2+ concern.
- No customer-support channel design (chatbot, email-ticket system).
- No internationalization scope — Phase 1 is English + Hungarian by default; broader EU languages deferred.
