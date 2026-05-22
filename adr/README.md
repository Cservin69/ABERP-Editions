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

- [ADR-0009 — NAV invoice issuing](0009-nav-invoice-issuing.md) — *§1 extended by ADR-0022; §6 extended by ADR-0023 (storno), ADR-0024 (modify), ADR-0025 (technical annulment), ADR-0026 (submit-annulment wire flow), ADR-0027 (poll-annulment-ack wire flow), and ADR-0028 (observe-receiver-confirmation); §7 extended by ADR-0031 (offline submission queue); §8 extended by ADR-0029 (per-invoice export bundle) and ADR-0032 (Attempt-before-call posture + failed-submission audit trail); §5 extended by ADR-0032 (state-2 Pending precondition for retry-submission)*
- [ADR-0030 — audit-ledger mirror file `<db>.audit.log`](0030-audit-ledger-mirror-file.md) — *extends ADR-0008 §"Storage" (the mirror-file posture); lifts F10; cleared the session-6 fortnightly sharpening review-bar named in ADR-0029 §5*
- [ADR-0031 — offline submission queue](0031-offline-submission-queue.md) — *extends ADR-0009 §7 (the offline submission queue posture); closes ADR-0009 §7 at the infrastructure level; queue membership derived from the audit ledger (no side table); hard cap of 50 enforced at issue time; `drain-submission-queue` CLI processes the queue in FIFO; alert thresholds (5 pending or 30-minute oldest) surface as drain-time WARN; F40 (failed-attempt audit trail — closed by ADR-0032), F41 (submission-deadline gates), F42 (operator-tunable thresholds), F43 (bundle-redaction posture for `nav_xml_path` PII) deferred with named triggers*
- [ADR-0032 — Attempt-before-call posture and failed-submission audit trail](0032-attempt-before-call-failed-submissions.md) — *extends ADR-0009 §8 (`invoice.submission_attempt` Fires-before-the-response design intent) and ADR-0009 §5 (operator-unblock surface); closes F40 at the issuing-path level; adds `InvoiceSubmissionAttemptFailed` EventKind (F12 ritual tenth landing); splits `manage_invoice::call` into `build_request` + `send_built_request` (existing `call` retained as backward-compat wrapper); `submit-invoice` / `retry-submission` / `drain-submission-queue` shift to two-tx posture (TX1 = Attempt-before-call; TX2 = Response on success or AttemptFailed on failure); `retry-submission` accepts state-2 (Attempt-without-Response, transport-mid-flight loss) per the new `StuckStage::Pending`; `submission_queue::pending_from_ledger` excludes invoices with an Attempt entry from the drain's FIFO walk (third predicate clause); F44 (Layer-2 `queryInvoiceCheck`), F45 (automatic state-2 retry loop), F46 (operator-tunable attempt-failed alert thresholds) deferred with named triggers*
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
