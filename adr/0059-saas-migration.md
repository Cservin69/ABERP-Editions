# ADR-0059 — SaaS migration: ABERP from local Tauri desktop to `invoicing.abenerp.com`

- **Status:** Proposed — S223 / PR-219 (2026-06-02). Threat-model + design doc. **No infrastructure changes.** Gates Phase B through Phase G of the migration sequence enumerated in §7.
- **Date:** 2026-06-02
- **Deciders:** Ervin Áben
- **Supersedes:** none — additive cornerstone ADR. Extends ADR-0007 (Security baseline) for public-internet exposure; extends ADR-0016 (Cloud sync — stub) by collapsing "remote UI" into the SaaS deployment target.
- **Related:** ADR-0007 (security baseline + STRIDE/LINDDUN posture), ADR-0020 (NAV transport correction — pinned roots, no client cert), ADR-0019 (relational SoT — engine-agnostic), ADR-0021 (pre-code baseline — wire protocol), ADR-0030 (audit-ledger mirror file — DR surface), ADR-0052 (chain-verify cadence — load tolerance), ADR-0055 (operator-visible tenant-state inventory), ADR-0057 (quote-intake architecture — daemon spawn pattern), the project memory entries [[trust-code-not-operator]], [[hulye-biztos]], [[no-sql-specific]], [[no-smoke-test-in-prod]], [[origin-clean-topology]], [[aberp-nav-as-dr]].

## 1. Context

### Current topology (PROD_v2.1 — already shipped)

- **Tauri 2 desktop shell** wraps a Svelte SPA + spawns the `aberp serve` Rust backend as a subprocess. Tauri's role: window chrome + `beforeDevCommand`-style lifecycle + per-tenant DuckDB path resolution + first-launch confirmation modal.
- **Backend listens on `127.0.0.1:<port>` over self-signed TLS** with fingerprint pinning by the shell (ADR-0007 §Transport).
- **Auth surface**: session-token bearer minted at boot by the backend; the shell hands it to the SPA via Tauri command.
- **Storage**: per-tenant DuckDB at `~/.aberp/<tenant>/aberp.duckdb`; mirror at `<db>.audit.log` (ADR-0030); side-stores for input.json, NAV XML, AP XML, restored XML, ap-artifacts; `seller.toml` for the six preservation slots (identity / banks / smtp / numbering / branding / quote_intake — [[seller-toml-write-invariant]] and ADR-0055).
- **Secrets**: macOS Keychain — NAV technical-user login / password / `xmlSignKey` / `xmlChangeKey` per ADR-0020; SMTP password (cached at boot per [[smtp-secrets-cache-boot]]); quote-intake bearer (ADR-0057).
- **Threat model (ADR-0007)**: the operator is the only caller; physical access to Ervin's MacBook is the dominant attacker class; the wider internet is not a surface.

### Target topology (`invoicing.abenerp.com`)

Public-internet reachable single-tenant ABERP, MFA-protected, accessible from any browser, with the same NAV + email + audit-ledger guarantees that PROD_v2.1 ships today.

Ervin's framing (2026-06-02): *"invoicing.abenerp.com will be public facing probably with cheap MFA and hardened like a 650HDWR MONEL :) the upside is that I can reach it from anywhere not just from my laptop."*

The Monel 650 metaphor (a marine/chemical-industry nickel alloy chosen for extreme corrosion resistance) sets the security posture explicitly: defense-in-depth, not "good-enough SaaS." A successful breach lets an attacker submit fraudulent invoices to NAV under tax number `24904362-2-41` — the cost is regulatory, not just reputational.

### Why this is a separate concern from go-live

PROD_v2.1 has already cut. The invoicing strand is in production on Ervin's laptop, serving real money. SaaS migration is an architectural shift, not a launch milestone. The laptop deployment is the rollback target for every phase of this migration.

## 2. Decision drivers

| Driver | Constraint |
| --- | --- |
| **NAV compliance** | A fraudulent submission costs more than typical SaaS breaches. Bar is higher than usual. |
| **Cost** | Target ≤ €15–25/month OpEx steady-state. From €0 today. |
| **Operator complexity** | Ervin runs the stack solo. Runbook stays hülye-biztos ([[hulye-biztos]]); no operator-discipline gates ([[trust-code-not-operator]]). |
| **Reversibility** | The laptop deployment remains the rollback target. NAV-as-DR ([[aberp-nav-as-dr]]) remains the data-loss recovery surface. |
| **Single-operator scale today** | 1 MAU; ~10–50 invoices/month; ~5 GB total state. Sub-10 req/min steady-state. This drives many sizing choices toward "smallest reasonable" rather than scaling-anticipated. |
| **EU data residency** | Áben Consulting Kft. is a Hungarian entity. Customer PII + NAV submissions are EU-jurisdiction data. The compute region must be EU. |

## 3. Considered options

### 3a — Compute host

| Option | Monthly cost | Pros | Cons |
| --- | --- | --- | --- |
| **AWS Lightsail $5 (EU-Frankfurt)** | ~$5 | Same ecosystem as ABERP-site (GH Actions OIDC reuse), 2 GB RAM, predictable bill | 1 vCPU, AWS network egress meter, locked-in to AWS console |
| Lightsail $10 | ~$10 | 2 GB RAM headroom for DuckDB + SPA serving + Rust backend | More than Ervin's frame asked for |
| Fargate (no-server) | $20+ | No host to patch | Cost ceiling for a 1-MAU workload is silly |
| EC2 t4g.nano + EBS | $3 + EBS + LB | Granular tuning | Operator overhead — manual OS patching, no managed pricing |
| **Hetzner Cloud CX22 (EU-Falkenstein)** | ~€4.51 | 2 vCPU, 4 GB RAM, 40 GB NVMe SSD, EU-incorporated, ~50% cheaper than Lightsail for ~2× the resources | No AWS console integration; bring-your-own secrets + monitoring |
| Hetzner CX32 | ~€7.05 | 4 vCPU, 8 GB RAM, 80 GB SSD | Probably overspecced for a 1-MAU workload |

**Pushback against the source brief.** The source memory and the S223 brief both name "Lightsail $3.50" as the cheapest viable option. Hetzner Cloud CX22 (EU-jurisdiction, German-incorporated, EU-DPF certified) is roughly half the price of Lightsail $5 for ~2× the resources, with EU data residency that better aligns with Áben Consulting's customer-PII obligations. The AWS-anchoring assumption in the source memory is not load-bearing; it is implicit. **Recommendation in §4 takes Lightsail anyway**, because the ecosystem-reuse argument (GH Actions OIDC + IAM + CloudWatch) is real, but Ervin should see the alternative explicitly costed.

Lightsail $3.50 (1 GB RAM) is undersized for `aberp serve` + DuckDB working set + SPA asset cache + Caddy. Recommendation: Lightsail **$10** (2 GB RAM) — €4–5/mo headroom buys real RAM, the wrong place to economise.

### 3b — TLS termination + edge

| Option | Monthly cost | Pros | Cons |
| --- | --- | --- | --- |
| **Caddy on the instance, auto-Let's Encrypt** | $0 | 5 lines of Caddyfile, automatic 90-day renewal, HSTS + TLS 1.3 defaults, one moving part | Single point of failure (same instance); LE rate limits if misconfigured |
| CloudFront + ALB | $20+ | Caching + DDoS mitigation | Cost ceiling, complexity overhead — neither benefit is load-bearing at 1 MAU |
| CloudFront → direct origin | ~$1 | Caching, AWS WAF available | Origin still needs a TLS cert; double TLS |
| Cloudflare free tier in front of origin | $0 | DDoS mitigation, free WAF, hidden origin IP, basic rate limit | Cloudflare sees plaintext (privacy + trust-boundary expansion) |

**Recommendation: Caddy on the instance + Cloudflare free tier as proxied DNS.** Caddy handles cert lifecycle; Cloudflare hides the origin IP and provides L4 DDoS shielding for free. Cloudflare's view of plaintext is a trust-boundary expansion; mitigated by (a) Cloudflare's tenant is Ervin alone, (b) "Full (strict)" mode keeps origin TLS verified, (c) Cloudflare's plaintext window covers HTTPS bodies, not NAV traffic (NAV calls are outbound from the instance, bypass Cloudflare entirely).

### 3c — Auth + MFA

| Option | Monthly cost | Pros | Cons |
| --- | --- | --- | --- |
| AWS Cognito | $0 (under 50k MAU free tier) | Managed user pool, TOTP/SMS/email MFA out of the box | Lock-in to AWS; auth lives outside the ABERP code tree; one more service to audit; the JWT-validation library is the new attack surface |
| Self-hosted TOTP + session cookie | $0 | ~200 lines of Rust via `totp-rs` + `cookie` crates; lives in the same binary; auditable in one place | Shared-secret model (server-side TOTP seed in DuckDB) — a DB exfil hands the attacker the seed |
| **WebAuthn / passkeys** | $0 | Hardware-bound (Secure Enclave on iOS, TPM on macOS), phishing-resistant by spec, no shared secret to steal, free | Two-device enrollment ritual (laptop + iPhone) to avoid lockout if one device dies; `webauthn-rs` crate adds a dependency |
| Magic-link email + TOTP | $0 | Familiar UX | Email is the recovery channel — compromising the email account becomes the attack vector |

**Pushback against the source brief.** The brief calls Cognito "managed = less attack surface." That phrasing inverts the analysis: managed = **opaque** attack surface that ABERP cannot audit, plus a lock-in cost when (not if) the SaaS deployment eventually wants to move. For a 1-MAU workload, "managed" buys nothing — there is no user-pool admin work to delegate. WebAuthn keeps the auth surface inside the ABERP binary, auditable by Ervin (and Dispatch), with hardware-grade credentials.

**Recommendation: WebAuthn primary with passkey enrollment on two devices (MacBook + iPhone), TOTP as a fallback authenticator for the lockout-recovery scenario.** Self-hosted via `webauthn-rs`. Failed-attempt audit ledger entries per ADR-0007 §Operator-as-threat-actor and ADR-0008.

I'd want a security review to validate the WebAuthn parameter choices (resident-key, user-verification required, attestation `direct`/`none`) before commit. Flagging confidence: **high on the choice, medium on the parameter set** — `webauthn-rs` has sane defaults but the Hungarian regulatory threat model deserves a second look.

### 3d — Secret management

| Option | Monthly cost | Pros | Cons |
| --- | --- | --- | --- |
| AWS Secrets Manager | $0.40 / secret / mo (~$2/mo for 5) | Managed rotation, IAM-scoped, audit log | Cost for what is a single-instance load-once-at-boot pattern |
| **AWS SSM Parameter Store SecureString** | $0 (free tier covers our request rate) | KMS-backed encryption (same algorithm as Secrets Manager), simpler boto/SDK call | No automatic rotation — operator runs rotation manually |
| HashiCorp Vault | $5–20+ (self-hosted overhead) | Best-in-class | Overkill for 5 secrets |
| sops + age + git | $0 | Secrets-in-repo with age decryption at boot | Secrets in git = blast radius if the repo is exposed, even encrypted |

**Recommendation: SSM Parameter Store SecureString.** Free at our scale; same KMS as Secrets Manager; the IAM role attached to the instance reads parameters at boot via `aws ssm get-parameter --with-decryption`. Five secrets: NAV `login`, NAV `password`, NAV `xmlSignKey`, NAV `xmlChangeKey`, SMTP password. Optional sixth: quote-intake bearer per ADR-0057.

If the §3a decision flips to Hetzner: **sops + age** with the age private key in `/etc/aberp/age.key` (root-readable, `0400`), encrypted secrets file alongside `seller.toml`. Same security posture as SSM but without AWS dependency.

### 3e — Database

| Option | Monthly cost | Pros | Cons |
| --- | --- | --- | --- |
| **DuckDB on instance NVMe + nightly S3 snapshot** | ~$0.50 (S3 storage) | Zero code change; matches today's posture | Single-writer; restore is whole-file replay |
| Postgres on RDS db.t4g.micro | ~$15 + storage | Proper backups, PITR | Schema migration cost; couples to AWS RDS; bigger attack surface |
| Postgres on the instance | $0 | Same shape as RDS without lock-in | Couples DB lifecycle to compute lifecycle |
| SQLite on instance | $0 | Drop-in for DuckDB-on-file with smaller working set | Less powerful queries (ABERP's analytical paths use DuckDB's columnar advantages); migration cost |

**Recommendation: DuckDB on instance NVMe + hourly snapshot to S3-IA + audit-ledger mirror file (already a property per ADR-0030).** Engine-agnostic per [[no-sql-specific]]; if the cloud experiment surfaces a DuckDB-specific operational issue, we swap to SQLite/Postgres without invariant rewrites (ADR-0019).

S3 (or Hetzner Storage Box, ~€3.20/mo for 1 TB) versioned bucket + 30-day retention. Encrypt with KMS (or sops/age). NAV-as-DR remains the **secondary** recovery surface for current-year invoice data (per [[aberp-nav-as-dr]] and ADR-0034 `recover-from-nav`).

### 3f — SPA serving

| Option | Monthly cost | Pros | Cons |
| --- | --- | --- | --- |
| **Same backend serves SPA assets** | $0 | Single TLS cert, single origin, matches today's loopback pattern | Asset cache headers must be configured (Caddy default OK) |
| S3 + CloudFront for static | ~$1 | Asset caching at edge | Two origins, two deploy pipelines, CORS overhead |

**Recommendation: same backend serves SPA.** Cloudflare already caches static `/_app/immutable/*` paths for free; CloudFront doesn't earn its keep for 1 MAU.

### 3g — Tauri removal

| Option | Pros | Cons |
| --- | --- | --- |
| Full removal — web SPA + backend service only | Simplest; one binary; one deploy pipeline | Loses laptop offline mode; loses first-launch confirmation modal as a security gate; loses `tools/snapshot-prod.sh` rooting at `~/.aberp/` |
| **Dual-target: Tauri shell stays for laptop mode, web SaaS is a second deployment target** | Reversibility preserved; NAV-as-DR offline-recovery surface keeps a Tauri build; both targets compile from the same `aberp serve` core | Build matrix gains a `--features saas` profile; CI compiles twice |
| Tauri-only via Tailscale/wireguard tunnel | Reuses every laptop invariant | Defeats "reach from anywhere any browser" — Tailscale on iPhone is awkward and Tailscale is the new auth surface |

**Pushback against the source brief.** The brief lists "full Tauri removal" as the recommended path. That is wrong-shape. Tauri's loopback fingerprint pinning + first-launch confirmation modal + snapshot-aware boot check (PR-171) are load-bearing local-mode invariants. The cloud target adds an alternative deployment, it does not subtract laptop mode. The laptop deployment is also the **rollback target** for every phase of this migration; deleting it deletes the rollback.

**Recommendation: dual-target.** Cargo feature `saas` flips the backend to web-deployment mode (no Tauri-bound paths, SSM-instead-of-Keychain secret loader, public-TLS-instead-of-loopback transport). Same code tree; same audit-ledger; same NAV adapter; same SPA. The laptop binary stays the same as PROD_v2.1.

### 3h — Deploy pipeline

| Option | Monthly cost | Pros | Cons |
| --- | --- | --- | --- |
| **GH Actions OIDC → AWS IAM role → SSM Run Command → systemd restart** | $0 | Same pattern as ABERP-site PR-G; no SSH keys in CI; clean audit trail | Tied to AWS — needs alternative if §3a goes Hetzner |
| Manual ssh + systemctl | $0 | Trivial | Operator discipline anti-pattern per [[trust-code-not-operator]] |
| Container registry + cron pull | $0 | Pull-based, instance-driven | Extra layer for a 1-instance deployment |

**Recommendation: GH Actions OIDC + SSM Run Command.** Mirrors ABERP-site's deploy pattern. If §3a goes Hetzner: GH Actions + age-signed artifact upload + instance pulls on webhook.

### 3i — MFA enforcement scope

| Option | Pros | Cons |
| --- | --- | --- |
| MFA at login, session cookie afterward | Standard SaaS UX | A session-cookie theft hands an attacker the full NAV-submit capability |
| MFA per sensitive action | Paranoid mode — every NAV submit is an explicit assertion | UX pain; operator habituates and clicks through; phishing surface widens |
| **Step-up MFA: login + re-assert for NAV submit / storno / restore / recover** | Balance — high-value actions get a fresh challenge; routine reads are session-cookied | Implementation cost: routes tagged `requires_fresh_mfa(within: 5min)`; one extra integration test surface |

**Recommendation: step-up MFA on the four irreversible-NAV-side actions** — `issue-invoice` (when the submit-now toggle is on), `submit-invoice` retry, `storno`, `restore-from-nav-outgoing` (RESTORE token per ADR-0053), `recover-from-nav`. Freshness window: 5 minutes. Session cookie covers everything else.

## 4. Decision outcome (recommended stack)

Defense-in-depth without paying for it; one operator's posture against the public internet.

| Layer | Pick | Reasoning |
| --- | --- | --- |
| Compute | **Lightsail $10 (EU-Frankfurt)** | 2 GB RAM headroom; same ecosystem as ABERP-site for OIDC reuse. Hetzner CX22 flagged as ~50% cheaper alternative if Ervin decides AWS lock-in is the worse trade. |
| Edge | **Caddy on the instance + Cloudflare free tier proxied DNS** | Auto-LE certs; free L4 DDoS + WAF; hides origin IP. |
| Auth | **WebAuthn primary (2-device enrollment) + TOTP fallback** | Hardware-grade, phishing-resistant, $0, auditable in the ABERP binary. |
| Secrets | **AWS SSM Parameter Store SecureString** | Free; KMS-backed; IAM-scoped. |
| Database | **DuckDB on NVMe + hourly S3-IA snapshot + audit-ledger mirror** | Zero code change; engine-agnostic preserved; matches [[no-sql-specific]]. |
| SPA | **Same backend serves assets** | One origin, one cert. |
| Tauri | **Dual-target — laptop binary stays, web is a second profile (`--features saas`)** | Reversibility + NAV-as-DR offline-recovery preserved. |
| Deploy | **GH Actions OIDC → SSM Run Command → systemd** | Mirrors ABERP-site; no SSH keys in CI. |
| MFA scope | **Step-up MFA on NAV submit / storno / restore / recover (5-min freshness)** | High-value actions re-assert; routine reads session-cookied. |
| Domain | **`invoicing.abenerp.com` via Route 53 ($0.50/mo)** | Per source memory. |

**Realistic monthly bill: ~€12–15.** Lightsail $10 + Route 53 $0.50 + S3-IA ~$1 + SSM $0 + Cloudflare $0 + Cognito $0 (not used) + minor egress.

## 5. Threat model — STRIDE per surface

The bar set by the Monel-650 metaphor demands surface-by-surface enumeration. The table below is best-effort; I'd want a third-party security review (engaging a Hungarian security firm familiar with NAV compliance) before Phase G cutover. Confidence notes are explicit.

### 5.1 Public TLS edge (Caddy + Cloudflare)

- **Spoofing**: TLS misconfig — Caddy enforces TLS 1.3 + strong cipher suites by default; HSTS preload-eligible. Mitigated.
- **Tampering**: cert exfil — cert lives on instance disk, accessible only to root + the caddy user; never in git; auto-rotated every ~60 days by Let's Encrypt. Residual: full instance compromise = cert exfil; accepted.
- **Repudiation**: edge logs accessible — Cloudflare access logs (free tier) + Caddy access log on instance + audit-ledger append-only. Three independent surfaces.
- **Information disclosure**: route enumeration — unauthenticated routes return identical JSON shape (`{ "error": "unauthenticated" }`, 401, same body length). Mitigated.
- **DoS**: L4/L7 DDoS — Cloudflare free tier covers L4; L7 sophisticated DDoS is the residual risk. For a 1-MAU target, the attack-cost vs payoff makes L7 unlikely; accepted.
- **Elevation**: TLS downgrade — HSTS preload + TLS 1.3-only forbids downgrade. Mitigated.

### 5.2 Auth endpoints (login, WebAuthn challenge, TOTP fallback, session refresh)

- **Spoofing**: brute force — per-IP + per-username rate limit (5 attempts / 5 min, then 15-min lockout); ADR-0008 audit-ledger entry per failed attempt; alert on >10 failures/hr (out-of-band via SNS or operator-visible audit timeline).
- **Tampering**: session cookie theft via XSS — strict CSP (`default-src 'self'; script-src 'self'`); HttpOnly + Secure + SameSite=Strict cookie; SPA's existing XSS-resilience reviewed in Phase B.
- **Repudiation**: failed logins — audit-ledger entries with source IP + user agent + outcome.
- **Information disclosure**: timing oracle on username existence — constant-time DB lookup; same error message + same response time (±50ms) for "user doesn't exist" vs "MFA failed."
- **DoS**: TOTP code replay — `used_totp_codes` table with 90-second TTL; WebAuthn challenge is single-use by spec.
- **Elevation**: passkey replay — WebAuthn challenge nonce per assertion; client-side counter sanity check. Residual: device compromise = game over; accepted as the worst-case threat that step-up MFA cannot solve.

### 5.3 Authenticated API

- **Spoofing**: session fixation — session ID rotated on login + on every MFA step-up.
- **Tampering**: CSRF — SameSite=Strict cookie + double-submit token on state-changing routes (issue / storno / settings).
- **Repudiation**: every state change generates an ADR-0008 audit entry; chain-verify cadence per ADR-0052.
- **Information disclosure**: verbose errors — `tracing` filters at `info`; no PII in logs (ADR-0007 §Logging); response bodies sanitise SQL errors.
- **DoS**: per-session rate limit (100 req/min default) + Cloudflare L7 rate limit at edge.
- **Elevation**: capability bypass — ADR-0007 capability-mapped routes; CI conformance test asserts every route declares a capability.

### 5.4 NAV submission path (server signs with prod tech-user creds)

This is the worst-case attack surface — a successful compromise lets the attacker submit fraudulent invoices that become real ÁFA liability under tax 24904362-2-41.

- **Spoofing**: NAV cred exfil — SSM SecureString at rest (KMS-encrypted); `Zeroizing<String>` in memory; never logged. Loaded once at boot.
- **Tampering**: replay of signed NAV request — NAV server-side `requestId` dedup window + ABERP's own two-layer idempotency (ADR-0009 §5).
- **Repudiation**: ADR-0032 attempt-before-call writes `InvoiceSubmissionAttempt` BEFORE the network call; ADR-0034 `recover-from-nav` reconstructs from NAV if local response is lost.
- **Information disclosure**: NAV creds never in logs ([[no-smoke-test-in-prod]]).
- **DoS**: NAV API rate limit handled by ABERP's offline submission queue (ADR-0031).
- **Elevation**: fraudulent submission via compromised session — step-up MFA freshness check (5-min window) blocks every irreversible NAV path. Residual: a fully compromised authenticated session that triggers step-up MFA from a fully compromised device. Mitigated by passkey-on-second-device requirement.

### 5.5 SMTP send path

- **Spoofing**: SMTP relay abuse — only authenticated workflows can trigger; per-tenant rate limit (max 50 emails / hour, audit-logged).
- **Tampering / Information disclosure**: SMTP creds — SSM SecureString; in-memory cached at boot per [[smtp-secrets-cache-boot]]; never logged.
- **Elevation**: sender spoofing — DKIM + SPF on the sender domain (`abenerp.com`) is the operator's responsibility outside ABERP; document in Phase F runbook.

### 5.6 DuckDB on disk + snapshots

- **Tampering**: file tamper — EBS encrypted (or Hetzner volume-level encrypted); file permissions root-only; audit-ledger hash chain (ADR-0008) + per-insert verify (ADR-0052) detects tamper at the application layer.
- **Repudiation**: snapshot retention — S3-IA versioned bucket; 30-day retention; access audit-logged via CloudTrail.
- **Information disclosure**: snapshot leak — S3 bucket private + KMS encrypted + IAM-scoped read access. Cloudflare has no role here.
- **DoS / data loss**: hourly snapshot + audit-ledger mirror file (ADR-0030) + NAV-as-DR recovery surface ([[aberp-nav-as-dr]]).

### 5.7 Audit-ledger integrity

- **Tampering**: hash chain (ADR-0008) + mirror file (ADR-0030) + per-insert verify (ADR-0052) — three layers. Tamper requires breaking SHA-256 AND modifying both the DB AND the mirror file AND every subsequent entry's hash.
- Future enhancement: signed attestation checkpoints (deferred ADR per `adr/README.md` deferred section; Ed25519 recommended) — would tag periodic ledger heads with a key not held on the same instance.

### 5.8 Operator account compromise

- **Phishing**: passkey is phishing-resistant by WebAuthn spec; the origin-bound assertion cannot be replayed against a phishing origin. (TOTP fallback IS phishable — flagged for the security review.)
- **Device theft**: passkey requires biometric/PIN; FileVault on Ervin's MacBook; iPhone passcode + Face ID.
- **Cleartext password reuse**: no password (passkey-only primary flow).
- **AWS account takeover**: separate IAM user for Ervin's console (MFA-enforced via Cognito-style admin login); deploy pipeline uses OIDC (no long-lived AWS keys in GitHub). Residual: AWS account itself is compromised; accepted as out-of-scope for this ADR (AWS Root MFA + hardware key recommended).
- **Supply chain (npm/cargo)**: `cargo audit` + `cargo deny` in CI per ADR-0007 §Supply chain; npm side covered by the `pnpm-lock.yaml` commit + Snyk/Dependabot review cadence (Phase F runbook item).
- **MFA bypass**: enrollment requires existing MFA + a recovery code printed at first setup, stored in a physical safe. Documented in Phase B's runbook.

**Confidence note.** §5.4 (NAV submission path) and §5.8 (operator account) are the two surfaces I'd specifically want a security review to validate. The rest is conservatively-mapped from STRIDE primers; the two named surfaces have load-bearing financial consequences and merit a third-party adversarial look before Phase G cutover.

## 6. Consequences

### Wins

- **Reach-from-anywhere**: the stated goal.
- **Automatic backups**: hourly snapshots replace "Ervin remembers" backup discipline.
- **HTTPS + MFA hardening**: a meaningful security uplift vs loopback-only token auth.
- **Same audit-ledger invariants** survive — ADR-0008 hash chain, ADR-0030 mirror file, ADR-0052 per-insert verify. The cloud database is the same DuckDB on a different filesystem.
- **NAV-as-DR remains primary recovery surface** for current-year invoice data ([[aberp-nav-as-dr]]).
- **Cargo `production` feature flag** + TEST- prefix + tenant guards (PR-165) still apply — same code, different host.

### Trade-offs

- **Operator UX shifts**: `ssh aberp@... systemctl restart aberp` replaces `cmd-Q` + relaunch. Mitigated by `./run/ssh_restart.sh` one-liner per [[hulye-biztos]].
- **Internet outage = ABERP outage** — laptop binary stays as the fallback per dual-target (§3g).
- **CloudWatch logs** are a different surface — operator runbook changes; structured logging via `tracing` makes search reasonable.
- **~€12–15/month OpEx** — from €0 today. Worth budgeting.
- **New attack surface**: public TLS edge + AWS account itself. Documented in §5; partly mitigated by AWS root MFA hygiene + OIDC for CI.

### What gets locked in

- **AWS dependency** if §3a stays Lightsail. SSM, Route 53, S3 all bind to AWS. Migration to Hetzner is a Phase-G-redux.
- **WebAuthn dependency**: `webauthn-rs` becomes a load-bearing crate. Acceptable; the crate is mature and widely audited.
- **Cloudflare DNS**: domain NS records point at Cloudflare. Mitigated by keeping the DNS zone exportable; migration to another provider is hours, not days.

## 7. Sequence (phases A–G with gates)

Each phase is a multi-session effort. Each ships with a clean rollback to the laptop deployment.

### Phase A — ADR + threat model (this PR, PR-219, S223)

- Filed: this ADR + index entry.
- **Gate to advance to Phase B**: Ervin acknowledges the recommended stack; Open Questions §8 are resolved (at least the binding ones).

### Phase B — Auth layer

- WebAuthn enrollment route + assertion route + session cookie minting.
- Step-up MFA freshness check on NAV-submit / storno / restore / recover routes.
- Audit-ledger entries for `LoginAttempted`, `LoginSucceeded`, `LoginFailed`, `MfaStepUpRequired`, `MfaStepUpCompleted` (five new `EventKind` variants — ritual fires).
- TOTP fallback enrollment + recovery code printout (single use, printable, store in safe).
- **Gate**: passkey enroll/assert + step-up flow demoed against a local `--features saas` build; one-clickable per [[hulye-biztos]]; the laptop-mode binary is unaffected (the new routes refuse to fire under `--features !saas`).

### Phase C — Secrets migration

- SSM Parameter Store SecureString loader behind the same `SecretStore` trait that Keychain implements today.
- `--features saas` enables `SsmSecretStore`; `--features !saas` keeps `KeychainSecretStore`.
- Boot-time load + zeroize on drop preserved.
- **Gate**: same `aberp serve` runs successfully against SSM in a dev account; secrets never logged; failure-to-load aborts boot loud per [[trust-code-not-operator]].

### Phase D — Storage + backup

- DuckDB lives at a configurable path (`ABERP_DB_PATH`, falls back to today's `~/.aberp/<tenant>/aberp.duckdb`).
- Hourly snapshot to S3-IA via instance-scheduled `cron` (preferred over Lambda — simpler, lives next to the DB).
- Snapshot bucket versioned + 30-day retention.
- Restore script: `tools/restore-from-s3-snapshot.sh <timestamp>` — pulls the snapshot, halts `aberp`, swaps the DB file, restarts.
- **Gate**: a restore-from-snapshot exercises the audit-ledger chain-verify (ADR-0052); the operator can name an arbitrary snapshot in the last 30 days and the restore completes in <2 min.

### Phase E — Tauri detachment (dual-target)

- New Cargo feature `saas` on the workspace.
- Tauri-bound code paths (`tauri::command`, shell-managed lifecycle, fingerprint cert handoff) compile-out under `--features saas`.
- SPA continues to load from `/index.html` whether served by Tauri or by the cloud backend's static-asset handler.
- **Gate**: `cargo build --features saas` produces a binary with no Tauri symbols; `cargo build` (laptop mode) is unchanged from PROD_v2.1.

### Phase F — Cloud deploy pipeline

- Lightsail $10 instance provisioned in `eu-central-1` via Terraform (or plain `aws` CLI in a script — Terraform may be overkill for one instance).
- Caddyfile + systemd unit committed in `infra/saas/`.
- GH Actions workflow: PR build → on-tag deploy → SSM Run Command → systemd restart.
- Cloudflare DNS + proxied A record for `invoicing.abenerp.com`.
- **Gate**: a fresh push to `main` deploys to a staging subdomain (`invoicing-staging.abenerp.com`); a tag push deploys to `invoicing.abenerp.com`; both behind WebAuthn; both with no NAV creds wired (uses NAV test endpoint per `cargo --features !production`).

### Phase G — Cutover

- Ervin enrolls passkey on MacBook + iPhone in the staging environment first.
- Snapshot taken of laptop's PROD_v2.1 state.
- Snapshot restored to cloud; `cargo --features "production saas"` build deployed.
- Visual smoke (NOT NAV-touching per [[no-smoke-test-in-prod]]) — login + list outgoing + list incoming + view a PDF.
- Laptop binary preserved as the rollback target until 30 days of clean cloud operation pass.
- **Gate**: first real invoice issued on the cloud deployment succeeds end-to-end (NAV submit + ack + email + audit); the first real submission IS the validation (per [[no-smoke-test-in-prod]]).

## 8. Open questions

These do not block PR-219 filing. Ervin must decide each before the corresponding phase opens.

1. **AWS Lightsail vs Hetzner CX22** (§3a) — recommendation Lightsail for ecosystem-reuse, but Hetzner is ~50% cheaper at ~2× the resources and EU-jurisdiction. Trigger: Phase F start.
2. **WebAuthn vs Cognito** (§3c) — recommendation WebAuthn for auditable-in-binary + hardware-grade. Trigger: Phase B start.
3. **Full Tauri removal vs dual-target** (§3g) — recommendation dual-target to preserve laptop rollback + NAV-as-DR offline-recovery. Trigger: Phase E start.
4. **Lightsail $5 vs $10** (§3a) — recommendation $10 for RAM headroom. Trigger: Phase F start.
5. **Single-tenant SaaS today vs multi-tenant eventually** — recommendation single-tenant; multi-tenant is an order-of-magnitude shift in posture (per-tenant DB → row-tenanted, ADR-0002 supersede). Out of scope for this ADR.
6. **Stage 2 (Ordering / Inventory) integration** — same backend or separate service? Recommendation: same backend until ADR-0021 §"Items deferred to build phase" surfaces a concrete Stage 2 PR; the quote-intake daemon (ADR-0057) shows the same-backend pattern works. Out of scope here.
7. **Third-party security review before Phase G** — strongly recommended; cost likely €1–3k for a 1-week engagement with a Hungarian firm familiar with NAV. Confidence-flagged in §5.

## 9. Adversarial review

- *"A single instance is a single point of failure — you're trading 'laptop offline' for 'instance offline.'"* True. Mitigations: Cloudflare cache holds the SPA assets through a brief origin outage; the laptop deployment remains as the manual fallback; uptime target is "good enough for a sole-trader CNC shop," not three-nines. A two-instance HA setup is a 4× cost step that doesn't earn its keep at 1 MAU.

- *"WebAuthn enrollment lockout is real — if both Ervin's devices die, ABERP is unrecoverable."* Mitigated by: (a) two-device enrollment from day one, (b) printed TOTP recovery code stored in a physical safe, (c) the laptop deployment remains as the data-recovery path (DuckDB snapshot + audit-ledger mirror + NAV-as-DR for current year). Residual: full simultaneous device loss + safe inaccessibility + cloud-snapshot-only path = recoverable via SSM SecureString reset + new enrollment; documented in Phase B's runbook.

- *"Cloudflare in front of plaintext bodies expands the trust boundary."* True. Mitigations: (a) NAV submissions bypass Cloudflare entirely (outbound from instance), (b) PII in SPA payloads is no more sensitive than what Cloudflare already terminates for millions of small SaaS, (c) Cloudflare's free-tier privacy posture is contractual + EU-DPF certified, (d) origin TLS verified end-to-end via "Full (strict)" mode. Residual: Cloudflare itself as a state-actor target; accepted at 1-MAU threat level.

- *"Step-up MFA at 5-min freshness is too short for productivity / too long for security."* Picked window is conventional (banks use 5–15 min for re-auth). Configurable via a `seller.toml` setting if operator pain surfaces; default tight for the Monel posture.

- *"You haven't named DDoS-defense costs explicitly."* Cloudflare free tier is the L3/L4 defense. L7 sophisticated DDoS would require Cloudflare Pro (~$25/mo); flagged as a Phase-F upgrade option if observed pain warrants. For a 1-MAU obscure-Hungarian-SaaS attack target, the probability is low; accepted.

- *"Why no client-side IP allow-list as a primary defense layer?"* Considered; rejected. Ervin invoicing from a customer's office or an airport breaks the allow-list pattern. WebAuthn + step-up MFA + Cloudflare WAF is the substitute. IP allow-list flagged as a future-additive hardening if Ervin commits to a stable office network.

- *"NAV requires HUF-bank-account-style trust — should the cloud instance be Hungarian-jurisdiction specifically?"* Frankfurt (EU-Central) is EU-jurisdiction; NAV's compliance requirements bind the data controller (Áben Consulting Kft.) not the data processor. Hetzner Falkenstein is similarly EU. No NAV-specific jurisdiction requirement names a specific country for SaaS hosting; flagged for the security-review-firm to validate against current NAV/AEOI/GDPR positions.

## 10. Alternatives considered

(See §3 for the full sub-decision treatments.) Cornerstone alternatives that did not survive the first cut:

- **Tailscale-only laptop reachability** — defeats "any browser from anywhere"; introduces Tailscale as the auth surface.
- **Stay on laptop forever; mDNS + dynamic DNS for remote reach** — fragile, no MFA, no defensible posture.
- **Hand off to a managed SaaS hoster (e.g., a Hungarian PaaS provider)** — surfaces a third-party-trust boundary larger than AWS+Cloudflare combined; rejected.

## 11. Invariants pinned (load-bearing for Phase B onward)

- The laptop deployment (PROD_v2.1 binary) remains a supported deployment target until at least 30 days of clean cloud operation pass. The cloud target is **additive**, not a replacement. Per §3g pushback.
- WebAuthn enrollment requires two devices from day one. Single-device enrollment refused at the API layer.
- Step-up MFA freshness window is enforced server-side at the route layer. Client-side disclosure of remaining freshness is OK; client-side bypass is impossible.
- Secrets are loaded once at boot, held in `Zeroizing` wrappers, never logged, never returned by any HTTP route. SSM secret loader implements the same `SecretStore` trait as `KeychainSecretStore` per [[no-sql-specific]]-spirit (engine-agnostic).
- The audit-ledger (ADR-0008) + mirror file (ADR-0030) + per-insert chain-verify (ADR-0052) survive the migration unchanged. The hash chain is the durable record across deployment targets.
- The NAV submission path is `--features saas`-orthogonal: same XML build, same SHA3-512 request signature, same `xmlSignKey` + `xmlChangeKey` artifacts (ADR-0020), same `attempt-before-call` posture (ADR-0032). Cloud vs laptop is a config change, not a NAV-protocol change.
- `seller.toml`'s six preservation slots ([[seller-toml-write-invariant]] + ADR-0057) are loaded from S3-IA on `--features saas` instances; the same four-way write invariant holds.
- Cargo `production` feature flag (PR-165) composes with `saas` — `cargo build --features "production saas"` is the cloud-prod build; `cargo build --features production` is the laptop-prod build; both go to NAV-prod endpoints. NAV-test endpoints are addressed by the absence of `production`.
- Origin push topology per [[origin-clean-topology]] — this branch (`s223/pr-219-saas-adr`) is local-only. The merge utility is the only writer to origin.
- The `--no-smoke-test` posture ([[no-smoke-test-in-prod]]) tightens, not relaxes, in SaaS: the first real cloud-prod NAV submission is the validation.

## 12. Confidence + verification posture

- **High confidence**: §3a–§3i recommendations + §4 stack + §6 consequences. These are conventional SaaS infra decisions at a 1-MAU scale; the only nuance is the dual-target + Monel-hardening framing.
- **Medium confidence**: §5 threat model. STRIDE coverage is best-effort; the table is conservative but un-reviewed by an external party.
- **Verification work to commission before Phase G**: a third-party security review by a Hungarian firm familiar with NAV compliance + WebAuthn parameter choices + AWS IAM scope-of-blast-radius. Budget €1–3k.

## 13. Items NOT in scope for this ADR

Per [[no-sql-specific]] + simplicity-first, these are explicitly deferred:

- **Multi-tenant SaaS** — single-tenant only at Phase G. Multi-tenant supersedes ADR-0002 and is a separate cornerstone ADR when triggered.
- **Stage 2 module integration (Ordering / Inventory)** — deferred to ADR-0021 §"Items deferred"; not affected by this migration.
- **CAD/CAM cold archival** (ADR-0057 §Open questions) — orthogonal.
- **Multi-operator token rotation** (ADR-0057) — orthogonal.
- **Hetzner CX22 deep-dive** — flagged in §3a as a viable alternative; trigger to file a follow-on ADR is Ervin's decision to flip §3a, which would supersede this ADR's §3a / §4 / §3h rows.
- **Concrete Terraform / Caddyfile / systemd unit text** — Phase F deliverable, not Phase A.
- **Backup encryption key escrow** — already a deferred ADR in `adr/README.md` § Deferred; trigger fires in Phase D.
- **LLM-use policy** — already a deferred ADR; no LLM paths in this migration.

These will each become their own ADR when triggered, per the README's just-in-time pattern.
