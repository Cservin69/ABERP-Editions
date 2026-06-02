# Architecture Decision Records (ADRs)

ADRs capture decisions that are hard or costly to reverse. They are the only
place where architectural decisions live. If a decision is not in an ADR, it has
not been made, regardless of what the code does.

## Numbering

Four-digit, monotonic, never reused. `0001`, `0002`, ... A deleted decision is
**superseded**, not removed; its file stays.

## Status lifecycle

```
Proposed → Accepted → (Deprecated | Superseded by NNNN)
```

- **Proposed** — drafted, not yet adversarially reviewed. Not safe to build against.
- **Accepted** — has passed at least one adversarial review.
- **Deprecated** — no longer applies; replacement not needed.
- **Superseded by NNNN** — replaced by another ADR. The old one stays for history; the new one references it.

A ticket in a tracker is not enough to change an ADR. An ADR is changed only by:

1. Editing it in-place if status is still `Proposed`.
2. Filing a superseding ADR if status is `Accepted` or later.

## Standard ADR template

```markdown
# ADR-NNNN — <title>

- **Status:** Proposed | Accepted | Deprecated | Superseded by NNNN
- **Date:** YYYY-MM-DD
- **Deciders:** <names>
- **Supersedes:** (optional) ADR-NNNN

## Context

What problem are we solving? What constraints apply? What did we already rule out and why?

## Decision

The decision, stated as a single declarative paragraph or short list. No hedging.

## Consequences

What gets easier. What gets harder. What we lock ourselves into.

## Adversarial review

What would a hostile auditor / red team / future maintainer say about this?
Each ADR must have at least three such concerns answered or explicitly accepted.

## Alternatives considered

Other options, and the specific reason they lost. "Simpler" is not a reason on its own.

## Open questions

Things not decided here that this ADR depends on, with the ADR number that will resolve them.
```

## Adversarial review cadence

- **Design phase** (now): every two weeks, all `Proposed` ADRs.
- **Build phase**: every release, plus any ADR touched since the last review.
- **Incident-triggered**: any production incident triggers a review of the ADRs covering the affected surface.

## Index

### Spine (foundational — change at your peril)

- [ADR-0001 — Backend language: Rust](0001-backend-language-rust.md)
- [ADR-0002 — Tenant isolation: database-per-tenant](0002-tenant-isolation-db-per-tenant.md)
- ~~[ADR-0003 — Storage abstraction with DuckDB as first backend](0003-storage-abstraction-duckdb-first.md)~~ — **superseded by ADR-0019**
- [ADR-0004 — Frontend: Tauri + Svelte local, cloud reserved](0004-frontend-tauri-svelte.md)
- [ADR-0005 — Universal ID scheme: prefixed ULIDs](0005-id-scheme-ulid.md)
- [ADR-0006 — Module boundaries and contracts](0006-module-boundaries.md)
- [ADR-0007 — Security baseline and threat model](0007-security-baseline.md) — *partially superseded by ADR-0020 (NAV-specific clauses only)*
- [ADR-0008 — Tamper-evident audit ledger](0008-audit-ledger.md)
- [ADR-0019 — Storage strategy: one trait, relational SoT, search-first projections, no foreign keys](0019-storage-strategy-no-fks.md) — *replaces 0003 and 0018*
- [ADR-0020 — NAV transport and credential posture correction](0020-nav-transport-credential-correction.md) — *partially supersedes 0007 (NAV clauses only)*
- [ADR-0021 — Pre-code consolidated baseline (stack + wire protocol)](0021-pre-code-consolidated-baseline.md)

### Module-level (stubs — to be filled in)

- [ADR-0009 — NAV invoice issuing](0009-nav-invoice-issuing.md) — *§1 extended by ADR-0022; §6 extended by ADR-0023 (storno), ADR-0024 (modify), ADR-0025 (technical annulment), ADR-0026 (submit-annulment wire flow), ADR-0027 (poll-annulment-ack wire flow), and ADR-0028 (observe-receiver-confirmation); §7 extended by ADR-0031 (offline submission queue); §8 extended by ADR-0029 (per-invoice export bundle) and ADR-0032 (Attempt-before-call posture + failed-submission audit trail); §5 extended by ADR-0032 (state-2 Pending precondition for retry-submission), ADR-0033 (Layer-2 queryInvoiceCheck disambiguation for state-2 retries — closes the duplicate-submission residual ADR-0032 §"Adversarial review" #2 named-warned — first half of §5's Layer-2 idempotency intent), and ADR-0034 (recover-from-NAV chain reconstruction — second half of §5's Layer-2 idempotency intent; closes F48); §2 typestate-enum-to-UI-label mapping extended by ADR-0036 (`serve.rs::derive_state` mirrors `audit_query::stuck_precondition` at the UI label level; surfaces Storno + Amended + state-2 Pending + state-2 + Exists + recovered-Response as distinct `&'static str` labels on the loopback HTTPS API; closes F21 + F47 jointly)*
- [ADR-0030 — audit-ledger mirror file `<db>.audit.log`](0030-audit-ledger-mirror-file.md) — *extends ADR-0008 §"Storage" (the mirror-file posture); lifts F10; cleared the session-6 fortnightly sharpening review-bar named in ADR-0029 §5*
- [ADR-0031 — offline submission queue](0031-offline-submission-queue.md) — *extends ADR-0009 §7 (the offline submission queue posture); closes ADR-0009 §7 at the infrastructure level; queue membership derived from the audit ledger (no side table); hard cap of 50 enforced at issue time; `drain-submission-queue` CLI processes the queue in FIFO; alert thresholds (5 pending or 30-minute oldest) surface as drain-time WARN; F40 (failed-attempt audit trail — closed by ADR-0032), F41 (submission-deadline gates), F42 (operator-tunable thresholds), F43 (bundle-redaction posture for `nav_xml_path` PII) deferred with named triggers*
- [ADR-0032 — Attempt-before-call posture and failed-submission audit trail](0032-attempt-before-call-failed-submissions.md) — *extends ADR-0009 §8 (`invoice.submission_attempt` Fires-before-the-response design intent) and ADR-0009 §5 (operator-unblock surface); closes F40 at the issuing-path level; adds `InvoiceSubmissionAttemptFailed` EventKind (F12 ritual tenth landing); splits `manage_invoice::call` into `build_request` + `send_built_request` (existing `call` retained as backward-compat wrapper); `submit-invoice` / `retry-submission` / `drain-submission-queue` shift to two-tx posture (TX1 = Attempt-before-call; TX2 = Response on success or AttemptFailed on failure); `retry-submission` accepts state-2 (Attempt-without-Response, transport-mid-flight loss) per the new `StuckStage::Pending`; `submission_queue::pending_from_ledger` excludes invoices with an Attempt entry from the drain's FIFO walk (third predicate clause); F44 (Layer-2 `queryInvoiceCheck` — **closed by ADR-0033 at the state-2 disambiguation level**), F45 (automatic state-2 retry loop), F46 (operator-tunable attempt-failed alert thresholds) deferred with named triggers*
- [ADR-0033 — Layer-2 `queryInvoiceCheck` reconciliation](0033-layer-2-query-invoice-check.md) — *extends ADR-0009 §5 (Layer-2 idempotency design intent — first half: queryInvoiceCheck call + state-2 skip-on-exists); closes F44 at the state-2 disambiguation level; adds `InvoiceCheckPerformed` EventKind (F12 ritual eleventh landing); adds `queryInvoiceCheck` operation to `nav-transport` under `operations/query_invoice_check.rs` (build_request + send_built_request, no backward-compat `call` wrapper); `retry-submission`'s state-2 path shifts to a three-phase posture (TX0 = Layer-2 check; TX1+TX2 unchanged from ADR-0032 §1 when outcome is Absent; abort on Exists/Failure); `retry-submission`'s state-3 path unchanged; `drain-submission-queue` unchanged (the ADR-0032 §5 fourth-predicate clause stays); `submission_queue::classify_attempt_failure` extends with five new `QueryInvoiceCheck*` arms; `audit_query::stuck_precondition` UNCHANGED (Layer-2 entries are informational-only per §6); **F48 closed by ADR-0034** (chain-reconstruction `recover-from-nav` surface — ADR-0009 §5's second half), F49 (Layer-2-aware mark-abandoned), F50 (queryInvoiceCheck rate-limit cooldown) deferred with named triggers*
- [ADR-0034 — Recover-from-NAV chain reconstruction](0034-recover-from-nav.md) — *extends ADR-0009 §5 (Layer-2 idempotency design intent — second half: chain fetch via queryInvoiceData + local-state reconstruction); closes F48; adds `aberp recover-from-nav` operator command that reconstructs the missing local `InvoiceSubmissionResponse` from NAV's queryInvoiceData on a state-2 Pending invoice whose most-recent `InvoiceCheckPerformed.outcome` is `"exists"`; reuses existing `InvoiceSubmissionResponse` EventKind + payload (F12 ritual does NOT fire); adds one additive `parse_audit_data_transaction_id` helper to `query_invoice_data.rs` (preserves the ADR-0028 verbatim-bytes-first posture — `call` / `QueryInvoiceDataOutcome` unchanged); `audit_query::stuck_precondition` UNCHANGED (Layer-2 entries remain informational-only per ADR-0033 §6); reconstructs only `InvoiceSubmissionResponse` (NOT `InvoiceAckStatus` — operator runs `aberp poll-ack` next for authoritative ack status per CLAUDE.md rule 12); `retry-submission`'s state-2 + Exists summary gains a pointer at `recover-from-nav`; `mark-abandoned` unchanged; `drain-submission-queue` unchanged; **F38 (bundle verifier) interaction pinned in §10 — Reading A (accept both root elements for InvoiceSubmissionResponse) recommended; closed by ADR-0035 at Reading A**; F49 (Layer-2-aware mark-abandoned), F50 (queryInvoiceCheck rate-limit cooldown) remain deferred with their existing triggers*
- [ADR-0035 — Bundle verifier tool (`aberp-verify`)](0035-bundle-verifier-tool.md) — *closes F38 at the operator-driven level; new separate-crate CLI binary `aberp-verify` in `crates/aberp-verify` (NOT a subcommand of `aberp` — inspector-side trust posture per §"Surfaced conflict 1" Reading A); re-verifies a per-invoice export bundle from its own bytes alone (no DB, no network, no keychain); per-entry hash recomputation via `aberp-audit-ledger::compute_entry_hash` (additively re-exported alongside `genesis_hash` — F12 ritual does NOT fire); consecutive-seq chain links checked, gap-spanning links delegated to the manifest's `chain_verified` claim with operator-visible NOTE per §"Surfaced conflict 3" Reading B; pins ADR-0034 §10's two-root-element acceptance at Reading A (accept both `<ManageInvoiceResponse>` AND `<QueryInvoiceDataResponse>` for `InvoiceSubmissionResponse` entries); no signing (F5 unchanged per ADR-0029 §4); F45 (automatic state-2 retry loop — future root-element extensions extend §4's table additively), future `--expect-binary-hash` / `--mirror` / `--strict-no-gaps` / `--public-key` flags named with triggers; future `aberp-audit-chain` sub-crate extraction named as a future PR if duckdb-transitive cost surfaces as an operational concern*
- [ADR-0036 — `serve.rs::derive_state` mirrors `audit_query::stuck_precondition` at the UI label level](0036-derive-state-mirror-of-stuck-precondition.md) — *closes F21 + F47 jointly at the loopback-HTTPS-API label level; extends `apps/aberp/src/serve.rs::derive_state` from six labels to eleven (adds Pending, PendingNavExists, Recovered, Storno, Amended); the UI classifier becomes a verbatim mirror of `audit_query::stuck_precondition` for every state both can name (Pending ↔ Stuck(StuckStage::Pending), Submitted ↔ Stuck(StuckStage::AwaitingAck), Finalized ↔ NotStuck(AlreadyFinalized), Rejected ↔ NotStuck(AlreadyRejected), Abandoned ↔ NotStuck(AlreadyAbandoned)); surfaces four UI-only labels not in stuck_precondition (Storno + Amended via chain-link `base_invoice_id` detection; PendingNavExists + Recovered as sub-labels of Pending / Submitted per ADR-0033 §6 + ADR-0034 §4); no payload change, no new EventKind variant (F12 ritual NOT fired), no new CLI subcommand, no Svelte shell change (Svelte affordances deferred per CLAUDE.md rule 3); wire shape (`state: &'static str` on InvoiceListItem + InvoiceDetailResponse) preserved; parameterized expected-label table tests pin the mirror invariant against future refactor drift; recovered-Response detection mirrors ADR-0035 §4 / A91's prefix-match at Reading A*
- [ADR-0037 — EUR-denominated outgoing invoicing: compliance test surface pin](0037-eur-invoicing-compliance.md) — *fires ADR-0009 §1's named trigger ("first non-HUF customer signed") and extends it: the HUF-only command-boundary restriction is replaced with a `Currency` closed-vocab (initial set `{Huf, Eur}`; widening trigger inherited from ADR-0009 §1's posture); pins the **compliance test surface** (the C1-C10 invariants) before any code is written so PR-44α through PR-44ε (domain / mnb-rates / issuance / NAV-submission / SPA UI) build against a hard regulatory contract; §1 enumerates the printed-invoice + NAV Online Számla 3.0 wire-body field requirements for an EUR-denominated invoice (currency code, exchange rate, rate source name "MNB", rate date, HUF-equivalent gross total, HUF-denominated per-VAT-rate amounts) with `[NEEDS-LEGAL-CHECK]` placeholders on the precise Áfa tv. subsections (§80, §169 [NEEDS-LEGAL-CHECK]); §2 pins the MNB rate source (SOAP `MNBArfolyamServiceSoap` primary; date alignment = supply-fulfillment date per Áfa tv. §80(2) [NEEDS-LEGAL-CHECK] with non-publication-day walk-back; no fallback rate source — loud-fail on MNB unavailability); §3 pins the `Currency` closed-vocab posture (variant names match money types — `Huf`, `Eur`; ISO 4217 strings surface only via `iso_code` accessor); §4 enumerates eleven compliance invariants C1-C11 with owner-PR + test posture (C11 added at the 2026-05-23 legal cleanup: 6-decimal exchange-rate precision per NAV XSD + round-half-even HUF rounding per Áfa convention); §5 explicitly names the PR-44α-through-PR-44ε scope split + the named-deferred items (MNB JSON fallback, third-currency variants, print rendering, cross-currency chain children — all forbidden); §6 pins the test posture; no new EventKind variant (F12 ritual NOT fired); no code touched (doc-only PR + 2026-05-23 doc-only legal cleanup); ADR-0009 §1's HUF-only restriction is **extended** (not superseded) — the command-boundary refusal posture is preserved, the closed vocab is the new boundary; **2026-05-23 legal cleanup (session 50) resolved Áfa tv. §80(1)(g) (HUF equivalent required when invoice currency ≠ HUF) + §80(2) (rate of fulfillment date or D-1 if no rate) + NAV XSD field paths (`invoiceData/currencyCode`, `invoiceData/exchangeRate`, `invoiceSummary/summaryNormal/invoiceVatAmountHUF`) + rate precision (6 decimals) + HUF rounding mode (round-half-even; supersedes the pre-cleanup half-up pin); residual `[NEEDS-LEGAL-CHECK]` for §169 invoice-content-list subsection + §172 storno-currency subsection remains open***
- [ADR-0010 — Billingo + NAV invoice ingestion (read path)](0010-invoice-ingestion.md) — *Billingo migration Accepted; NAV historical read path deferred to build phase*
- [ADR-0011 — Inventory model](0011-inventory-model.md) — *stub*
- [ADR-0012 — QR / vignette labels and no-touch handling](0012-qr-labels-no-touch.md) — *stub*
- [ADR-0013 — Robotics handoff (label print + place)](0013-robotics-handoff.md) — *stub*
- [ADR-0014 — CAD/CAM artifact storage](0014-cad-cam-artifacts.md) — *stub*
- [ADR-0015 — Order + logistics state machine](0015-order-logistics-state.md) — *stub*
- [ADR-0016 — Cloud sync and remote UI](0016-cloud-sync.md) — *stub*

### Cross-cutting

- [ADR-0017 — ABERP design language](0017-design-language.md)
- ~~[ADR-0018 — Storage evolution toward search-first / document stores](0018-storage-evolution-search-first.md)~~ — **superseded by ADR-0019**
- [ADR-0051 — Base-issue year for NAV references reads `billing.invoice.issue_date.year()`](0051-base-issue-year-from-billing-db.md) — *S198 / PR-198 (2026-05-31). Documents retroactively the de facto posture PR-183 / S183 implemented: the operator-typed `issue_date.year()` is the source of truth for every render-call site that emits a NAV reference (issue / storno / modification / observe-receiver-confirmation / annulment), never the audit-ledger `Entry.time_wall.year()`. Closes the 💭 S173 question raised in `docs/REVIEW_S172_S181.md`; aligns the five call sites with ADR-0019's relational-SoT pin; deletes the walker's `latest_sequence_year` capture per CLAUDE.md rule 13. Pinned by `check_base_is_annullable_renders_year_from_base_issue_date_param` + `check_base_is_annullable_cross_year_cites_base_original_year`.*
- [ADR-0052 — Chain-verify cadence for bulk ingestion paths: accept per-insert cost](0052-chain-verify-cadence-bulk-paths.md) — *S198 / PR-198 (2026-05-31). Closes the 💭 S180+S178 chain-verify-cost question. Decision: keep per-insert `verify_chain` (strictest tamper-detection granularity) rather than amortizing per-page or per-cycle. S186's `load_already_restored_cache` (O(1) idempotency) + S191's `spawn_blocking` (HTTP responsiveness) already address the discoverable operator pain; no per-page amortization needed. Triggers to revisit: AP cycle >60s steady-state, restore >10 min wall-clock, or bulk ingestion becomes recurring (would invert the "rare event" assumption).*
- [ADR-0053 — NAV-as-DR RESTORE confirmation token: server-side enforcement](0053-restore-token-server-side-enforcement.md) — *S198 / PR-198 (2026-05-31). Documents retroactively the de facto posture PR-186 / S186 implemented: the literal `"RESTORE"` token is a REQUIRED serde-required field on `RestoreFromNavOutgoingRequest`, equality-checked against `RESTORE_CONFIRMATION_TOKEN: &str = "RESTORE"` before the year-validation gate fires. SPA ceremony preserved as operator-UX; backend gate is the security contract. Closes the 💭 S180 RESTORE question. Pinned by `restore_request_serde_requires_confirm_token` + `restore_confirm_token_literal_is_exact_match_uppercase_restore`. Future trigger to revisit: a second DR-style route lands (would generalize to a `DangerousRoute` middleware).*
- [ADR-0054 — AP `IncomingInvoiceStatusChanged` audit payload: stay minimal, join against `ap_invoice`](0054-ap-status-change-audit-payload-shape.md) — *S198 / PR-198 (2026-05-31). Pins the de facto S177 payload shape (`ap_invoice_id` + `idempotency_key` + `from_status` + `to_status` + optional `reason`). The dedup tuple `(supplier_tax_number, nav_invoice_number)` stays in the `ap_invoice` mirror row (immutable INSERT-once columns); future exports JOIN against the mirror by `ap_invoice_id`. Closes the 💭 AP-payload-shape question. Trigger to revisit: cross-tenant audit-only consolidation export OR audit-only AP-DR posture is named (neither exists today).*
- [ADR-0055 — Operator-visible tenant-state inventory MUST include every load-bearing artifact](0055-operator-visible-tenant-state-inventory.md) — *S198 / PR-198 (2026-05-31). Pins the rule that any future PR adding a new on-disk path under `~/.aberp/<tenant>/`, a new DuckDB table, a new keychain entry, or a new side-store directory MUST add a row to the runbook's Appendix A "File and keychain inventory" AND extend the `tools/snapshot-prod.sh` docstring's `What it captures:` section IN THE SAME PR. Closes the 💭 runbook-coverage question by promoting it from one-off doc fix to ongoing contract. Closed-vocabulary of artifact categories pinned in §"Closed-vocabulary of artifact categories"; gaps surfaced by the S172-S181 review (S177's `ap-artifacts/` + `ap_invoice`, S180's `restored_invoice`, S197's per-digest XML files) closed in the same PR.*
- [ADR-0056 — Versioning policy: PATCH / MINOR / MAJOR for the `PROD_vX.Y[.Z]` release branches](0056-versioning-policy.md) — *S201 / PR-201 (2026-05-31). Names the rules that decide which segment bumps on a release. 1.x is the Stage 1 invoicing strand; PATCH = bugfixes + small features that don't materially change invoicing UX; MINOR = major feature changes within invoicing scope before any Stage 2 module ships; MAJOR (2.0) = first new module from the Stage 2 ERP buildout per `docs/e2e-shop/ground-zero.md` and ADR-0021 §"Items deferred to build phase". Heuristic for patch-vs-minor: if release notes need a "what's new" section longer than 2 bullets, it's a minor. "Module" defined explicitly as the compound test (new routes AND new schema/side-store AND new audit kinds). Extends `run/release.sh`'s `VERSION_RE` to accept `^PROD_v[0-9]+\.[0-9]+(\.[0-9]+)?$` (2-segment OR 3-segment; 4+ segments + pre-release suffixes rejected). Existing PROD_v1.0..v1.4 branches are NOT retroactively renamed; policy is forward-looking from PROD_v1.4.1 onward. S177 (AP module) called out as the canonical "would have been a 2.0 trigger" precedent under this policy.*
- [ADR-0057 — Quote intake from sister storefront: operator-pull daemon + staging table + no auto-burn of the regulated `invoice` surface](0057-quote-intake-architecture.md) — *S212 / PR-211 (2026-05-31). The 2.0 cutover marker for ABERP per ADR-0056. Pins the architectural shape that the first Stage 2 module (S210 / PR-204 backend + S211b / PR-210 SPA) implemented: ABERP polls the storefront's `GET /api/quotes?status=approved`, stages each fresh approved quote in `quote_intake_log` (DuckDB) together with a pre-prepared `DraftInvoice` JSON, POSTs the status write-back back to the storefront, and lets the operator adopt via the normal `issue-invoice` pipeline. Three alternatives weighed (storefront→ABERP webhook, manual SPA fetch, polling); polling wins because ABERP is local-desktop (no inbound surface), the daemon enforces cadence (no operator-discipline failure), and it reuses the proven Stage 1 daemon shape (S161 NAV poll + S178 AP sync). Invariants: daemon NEVER writes to `invoice` (CLAUDE.md rule 2 + [[trust-code-not-operator]]); `quote_id` PRIMARY KEY makes re-fetch a no-op INSERT (idempotency at the schema layer); bearer token `Zeroizing`-wrapped end-to-end + SPA settings is write-only; spawn precedence env > toml+keychain > dormant; no hot-reload (restart-required banner); one `system.quote_intake_poll_completed` audit entry per poll cycle; `[quote_intake]` becomes the sixth seller.toml preservation slot. The 2.0 trigger fires: new routes (`/api/quote-intake/*`) + new schema (`quote_intake_log`) + new audit kind (`QuoteIntakePollCompleted`) — all three of the ADR-0056 module-test gates hold. Open questions named (not blocking): CAD/CAM cold archival, multi-operator token rotation, write-back retry exhaustion.*
- [ADR-0059 — SaaS migration: ABERP from local Tauri desktop to `invoicing.abenerp.com`](0059-saas-migration.md) — *S223 / PR-219 (2026-06-02). **Proposed.** Threat model + design doc that gates Phase B–G of the multi-PR migration to public-internet SaaS. No infrastructure changes in this PR. Pins the recommended stack: Lightsail $10 EU-Frankfurt (Hetzner CX22 flagged as ~50% cheaper alternative — pushback against the source brief's AWS-anchoring), Caddy + Cloudflare free tier at the edge, **WebAuthn primary with two-device enrollment + TOTP fallback** (pushback against Cognito — auditable-in-binary beats managed-opaque at 1 MAU), SSM Parameter Store SecureString for secrets, DuckDB on NVMe + hourly S3-IA snapshot, **dual-target build (`--features saas`) keeps the Tauri laptop binary as the rollback surface** (pushback against full Tauri removal), GH Actions OIDC + SSM Run Command for deploy, **step-up MFA 5-min freshness on NAV submit / storno / restore / recover** routes. STRIDE walked per surface for 8 trust boundaries; the NAV-submission path + operator-account compromise paths flagged as the two surfaces a third-party Hungarian-security-firm review should adversarially validate before Phase G cutover. Realistic ~€12–15/mo. Open questions for Ervin: AWS vs Hetzner, WebAuthn vs Cognito, dual-target vs full removal, Lightsail $5 vs $10, single-tenant only, third-party security review. Phases A–G named with gates; the laptop deployment remains the rollback target through 30 days of clean cloud operation. Confidence: high on stack picks, medium on STRIDE coverage (un-reviewed by external party).*

### Deferred (not yet filed — tracked so they don't fall through)

The remaining items below are **deferred to build phase per ADR-0021 §Items deferred to build phase**. Each is filed as a just-in-time ADR when the named trigger fires; soft assertion in advance is forbidden (CLAUDE.md rule 12).

- ADR — Backup, encryption-at-rest key management, and offsite key escrow. *Called out in ADR-0007. Trigger: first PR that writes the encrypted backup path.*
- ADR — Data retention and GDPR erasure workflow. *Called out in ADR-0002. Trigger: first PR that wires a `forget-tenant` or `erase-customer` workflow.*
- ADR — LLM use policy (which paths use models, which providers, supply chain). *Called out in ADR-0007. Trigger: first PR that adds an LLM-using code path.*
- ADR — Specific font family selection (Hungarian diacritic coverage). *Called out in ADR-0017. Trigger: first PR that produces a printed invoice.*
- ADR — Print rendering path (browser print vs Rust-side PDF). *Called out in ADR-0017. Trigger: same as font ADR; either fills in or is filed alongside.*
- ADR — NAV historical / reconciliation read path (`queryInvoiceData`, `queryInvoiceDigest`, `queryInvoiceChainDigest`, `queryTransactionList`). *Called out in ADR-0010 §Deferred. Trigger: first PR wiring a NAV-side reconciliation pass against migrated invoices, or the first NAV-audit operator view.*
- ADR — XSD runtime validation crate choice (libxml FFI vs hand-rolled invariant check vs pure-Rust validator). *Called out in ADR-0021. Trigger: first PR implementing schema-drift detection per ADR-0009 §1.*
- ADR — Attestation signing-key type for ADR-0008 external attestation checkpoints. *Surfaced in the first full-spine adversarial review (F5). Trigger: first PR that exercises attestation cadence (long-running process, integration test crossing the cadence threshold, or cloud attestation publishing per ADR-0016). Recommendation when filed: Ed25519.*
- ADR — OS-keychain Rust binding crate for ADR-0007 §Secrets. *Surfaced in the first full-spine adversarial review (F6). Trigger: first PR that loads keychain-bound material in production code. Likely pick: `keyring`.*
