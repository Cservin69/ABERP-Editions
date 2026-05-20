# Threat model (living document)

**Methodology:** STRIDE for technical threats, LINDDUN for privacy threats.
**Cadence:** updated every two weeks during design phase; at every release
thereafter; and immediately on any incident.

**Status:** v0.1 skeleton — to be expanded at first adversarial review.

---

## Assets

| Asset                              | Sensitivity   | Notes |
|------------------------------------|---------------|-------|
| Tenant database files              | High          | Hold all customer/financial/inventory state |
| NAV submission receipts            | High          | Legal evidence; integrity is paramount |
| Tenant registry                    | High          | Holds connection info for every tenant DB |
| Audit ledger                       | High          | Tamper-evident; if it's editable, the whole system is |
| Session tokens                     | Medium-High   | Short-lived but capable while live |
| Operator OS keychain               | High          | Holds the root secret for at-rest encryption |
| CAD/CAM artifacts                  | Medium        | Customer-confidential; may be trade secret |
| Printer / robotics local network   | Medium        | Compromise → physical-world impact |
| Build provenance (binary hashes)   | Medium        | Required to defend "which binary signed this?" |

## Actors

| Actor                          | Capability                                              |
|--------------------------------|---------------------------------------------------------|
| External attacker (internet)   | Network reach to public endpoints (later, on cloud)     |
| External attacker (LAN)        | Reach to printers, robotics, the operator's workstation |
| Malicious tenant               | Authenticated, scoped to their own data                 |
| Compromised operator session   | Authenticated as the operator, capability-scoped        |
| Operator-as-threat-actor       | Fully authenticated; may try to backdate / hide / fake  |
| Compromised dependency         | Code-exec at the level of whatever uses the dependency  |
| Compromised LLM provider       | Can return adversarial outputs to LLM-using paths       |
| Insider (us)                   | Full source access; constrained by code review and audit|

## Trust boundaries (drawn from FOUNDATION.md §3)

1. UI process ↔ backend process (even local) — wire protocol with auth token.
2. Backend ↔ tenant database — only the backend reads/writes; storage adapter mediates.
3. Backend ↔ NAV — TLS with the NAV server-cert issuing root pinned in
   ABERP's trust store (OS trust store not consulted for NAV traffic),
   strict hostname verification. **No client X.509 / no mTLS** — NAV
   does not accept one. Client authentication is application-level
   inside the SOAP envelope per ADR-0009 §4: technical-user `login`,
   SHA-512 `passwordHash`, SHA3-512 `requestSignature` (with the
   per-invoice-index extension for `manageInvoice` / `manageAnnulment`),
   and an AES-128/ECB-decrypted `exchangeToken`. Replay protection
   comes from `requestId` + `requestTimestamp` being inputs to the
   signature. **Response integrity is TLS-only by decision**, with a
   retroactive-verification path provisioned: both the verbatim and
   parsed response body are committed to the audit ledger (ADR-0009
   §8), so a future signing-scheme disclosure by NAV unlocks offline
   re-verification of historical responses without an in-flight code
   change. The external fact "does NAV sign response bodies?" is
   tracked separately in ADR-0020 §Open Questions §1 against the
   Hungarian-dev research check; the ABERP-side posture is decided
   regardless. Authority: ADR-0020 §6 (editorially clarified
   2026-05-20 per F7), which partially supersedes ADR-0007's earlier
   mTLS-to-NAV claim.
4. Backend ↔ Billingo — TLS with pinned roots, API-key header from OS
   keychain. One-time read-path scope only (historical NAV invoice
   ingestion per ADR-0010); not on the issuance path. Deep posture
   detail belongs in ADR-0010.
5. Backend ↔ printer / robotics — local network, signed commands, ack required.
6. Tenant A backend process ↔ Tenant B backend process — none; separate processes.
7. Backend ↔ LLM provider (future) — TLS, inputs treated as untrusted, outputs as suggestions.

## Threats (STRIDE) — initial sketch

To be expanded at first adversarial review. The intent is that every entry
below grows into a row with: threat description, affected boundary,
likelihood, impact, mitigation (existing or required), and link to the ADR
that addresses it.

- **Spoofing** — forged session token; mitigation: token signing + short TTL + capability check.
- **Tampering** — audit ledger row edited post-hoc; mitigation: hash chain + external attestation.
- **Repudiation** — operator denies issuing an invoice; mitigation: ledger entry binds session + monotonic time + binary hash.
- **Information disclosure** — cross-tenant leak; mitigation: per-tenant DB + per-process isolation.
- **Denial of service** — local: unbounded resource use; cloud: rate limiting in ADR-0016.
- **Elevation of privilege** — capability bypass; mitigation: command-to-capability mapping is conformance-tested.

## Privacy (LINDDUN) — initial sketch

- **Linkability** — across tenants is structurally impossible (separate DBs); within a tenant is expected (it's an ERP).
- **Identifiability** — customer records are necessarily identifying; minimize fields, retention policy per ADR (TBD).
- **Non-repudiation (privacy sense)** — we want this for invoices, we do not want it leaking into customer-facing data unnecessarily.
- **Detectability** — ID schemes do not reveal volume (ADR-0005).
- **Disclosure of information** — same as STRIDE.
- **Unawareness** — what the operator does not know about their own data flows; documented in customer-facing notices.
- **Non-compliance** — Hungarian invoicing law, GDPR; tracked per ADR.

## Review log

| Date       | Reviewer | Findings filed as |
|------------|----------|-------------------|
| 2026-05-19 | Ervin    | Initial v0.1 — to be reviewed in two weeks |
| 2026-05-19 | Ervin    | Trust-boundary #3 corrected and split (NAV vs Billingo) per ADR-0020; response-body integrity flagged [OPEN] |
| 2026-05-19 | Ervin    | First full-spine adversarial review — see `docs/reviews/2026-05-19-pre-code-spine-review.md`; three blockers (F1–F3) and one tracked finding (F4) closed in ADR-0021 amendment same day; F5 + F6 deferred to build phase with named triggers; F7 (NAV response-body integrity) carried forward against external check |
| 2026-05-20 | Ervin    | Fortnightly adversarial review — see `docs/reviews/2026-05-20-fortnightly-review.md`; PR-6 closes the cross-crate transactional audit deviation; PR-6.1 closes F8/F9/F12; F7 closed editorially in ADR-0020 §6 (TLS-only with retroactive-verification path is the decided posture; the external fact about NAV's signing scheme is tracked separately and is no longer a blocking [OPEN]); F5 + F6 + F15 about to fire in PR-7-A |
