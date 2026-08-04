# ADR-0007 — Security baseline and threat model

- **Status:** Accepted (partially superseded by ADR-0020 for the NAV
  transport and credential clauses in §Transport and trust-boundary #3;
  other clauses unchanged)
- **Date:** 2026-05-19
- **Deciders:** Ervin

## Context

ABERP handles tax-authority submissions, financial data, customer PII, and
eventually the CNC company's production-floor systems. The cost of getting
security wrong ranges from regulatory fines to operational shutdown to data
extortion. Security is therefore a property of the system, designed in, not a
phase to be bolted on before launch.

The user has stated: "I am paranoid about cybersecurity from day one." We
take this at face value and bake it into the cornerstone decisions.

## Decision

### Threat model

A separate, living document at `docs/threat-model.md` (created with this ADR)
enumerates assets, actors, trust boundaries, and threats using **STRIDE** for
the technical surface and **LINDDUN** for privacy. It is updated at every
adversarial review (every two weeks during design phase).

Key trust boundaries documented today:

1. **Tenant ↔ tenant** (foundation §5; never the same process).
2. **UI ↔ backend** (wire protocol with auth token).
3. **Backend ↔ NAV / Billingo** (mTLS where available, signature verification
   of responses, replay protection).
4. **Backend ↔ printer / robotics** (local network — assumed hostile beyond
   the device the operator can see; commands signed and acked).
5. **Operator ↔ system** (yes, the operator is a threat actor — they may try
   to backdate an invoice, hide a stock movement, or burn a sequence number;
   the audit ledger constrains this).

### Authentication

- **Token-based** even on local deployments. The Tauri shell obtains a token
  on launch from the backend by presenting OS-keychain-bound credentials.
- **No passwords stored** in ABERP. Local auth is keychain-bound. Cloud auth
  is OIDC against a chosen identity provider (decided in ADR-0016).
- **Session tokens** are short-lived (≤ 1 hour) and refreshable. Revocation
  is centralized per-tenant.

### Authorization

- **Capability-based** on commands. Each command type has a required capability;
  the caller's session token resolves to a set of capabilities at the start of
  the request.
- **No "admin == everything"** role. Even the tenant owner cannot mutate the
  audit ledger; that capability does not exist in the system.

### Secrets

- **OS keychain on desktop** (macOS Keychain, Windows Credential Manager, Linux
  Secret Service). No secrets in config files.
- **Managed secret store on cloud** (decided in ADR-0016).
- Secrets accessed at process start, held in memory zeroized on drop
  (`zeroize` crate).

### At-rest encryption

- The per-tenant DuckDB file lives in an **encrypted directory** whose key is
  derived from an OS-keychain-bound secret + a tenant-specific salt.
- On platforms with full-disk encryption (most modern OSes), we layer ours on
  top. We do not assume the OS-level encryption is sufficient.
- Backups are encrypted with a separate key escrowed by the tenant (detail in
  the backup ADR, not yet filed).

### Transport

- **All UI ↔ backend traffic** over TLS, even loopback. Self-signed cert on
  local, locked to a fingerprint the Tauri shell verifies.
- **All external traffic** (NAV, Billingo) over TLS with pinned roots. mTLS
  where the counterparty supports it (NAV does).

### Supply chain

- `cargo-deny` in CI: forbidden licenses, banned crates, advisory checks.
  Runs unconditionally on every push and PR — never gated to a matrix arm.
- ~~`cargo-audit` in CI: known-vuln check on every build.~~ **Retired
  2026-08-04.** `cargo-audit` reads `Cargo.lock` textually and is feature-blind;
  `Cargo.lock` records *optional* dependencies whether or not any feature
  activates them, so it reported CVE-class advisories against crates this
  workspace never compiles (RUSTSEC-2026-0235 / `rkyv`, an optional
  `rust_decimal` dep behind a feature nothing enables). The only ways to green
  it were to ignore a CVE-class advisory or to fork the crate — both worse than
  the finding. `cargo-deny` resolves features, so it is now the single advisory
  gate, hardened to compensate: `unmaintained`/`unsound` scoped to `"all"` (the
  defaults skip transitive deps) and the config-drift lints promoted to errors.
  Rationale and the reachability argument live at the top of `deny.toml`.
- **Pinned dependency versions** (`Cargo.lock` checked in, `--locked` builds).
- **License allow-list**: MIT, Apache-2.0, BSD-3-Clause, MPL-2.0. Anything
  else requires a documented exception.
- **No `unsafe`** in business code without a `// SAFETY:` comment and a
  reviewer sign-off recorded in the PR.
- **Build provenance**: reproducible builds; the binary hash is recorded
  alongside the audit ledger so "the binary that signed this invoice" is
  identifiable.

### Tauri allow-list

- Tauri commands exposed to the Svelte side are explicitly enumerated. No
  `fs::all` or `shell::all` permissions. The frontend is treated as
  semi-trusted — display only.

### Logging and PII

- Structured logging (`tracing` crate, JSON in production).
- Log lines never include PII or invoice content beyond IDs and event types.
- Log retention is bounded; long-term evidence lives in the audit ledger
  (ADR-0008), not in the log file.

### Operator-as-threat-actor controls

- Backdating: invoice timestamps are server-clock-only; the operator cannot
  set them. Audit ledger records the wall clock + a monotonic time.
- Sequence-number burning without invoice: prevented by the transactional
  allocator (ADR-0009 will detail).
- Deletion: business entities are never hard-deleted. "Delete" produces an
  audit entry and a tombstone. Hard delete is a separate, capability-gated
  workflow used only for legal erasure.

### Incident response

- Every adversarial review produces a finding log even if no findings are
  filed (an explicit "none" is itself a finding).
- A known-incident playbook lives next to the threat model and is exercised
  at least quarterly.

## Consequences

- We spend time on hardening that does not produce visible features.
- Some friction for the operator (token expiry, no quick deletes).
- Defensible posture toward NAV audit: every decision has a stated rationale,
  every state change has an audit entry, every binary has a hash.

## Adversarial review

- *"OS keychain on Linux is uneven."* — Acknowledged. Linux deployments use
  the Secret Service API; we document the dependency. A future ADR may add
  a fallback (e.g., a passphrase-protected file) if real-world Linux
  deployments demand it.
- *"Loopback TLS is overkill."* — It is the cost of treating local and cloud
  topologies identically. Cheap to do, hard to add later.
- *"Capability lists drift from reality."* — Conformance test: every command
  declares its required capability; CI fails if a command exists without one.
- *"Zeroize is best-effort and the OS can swap before zeroize fires."* — True.
  We minimize secret lifetime in memory; we accept that defense-in-depth here
  has limits and document them in the threat model.
- *"You did not mention rate limiting."* — Right. Rate limiting is added when
  ABERP exposes a cloud surface (ADR-0016). On local, the operator is the
  only caller; rate limiting is theatre.
- *"What about the LLM components?"* — The system's product surface uses
  language models only for **judgment calls** (classification, extraction,
  drafting). Never for routing, retries, status-code handling, or
  deterministic transforms (working agreement, rule 5). Any LLM-using path
  declares its inputs as untrusted and its outputs as suggestions, never as
  authoritative state.

## Alternatives considered

- **"Security in v2"** — refused; reversing this is the kind of decision
  the project instructions specifically warn against.
- **No threat model document, security as folklore** — refused; an audit
  needs evidence, not a culture.
- **Bring your own auth (OIDC only, no local mode)** — refused for local;
  the desktop binary must work in a NAV inspector's office with no internet.

## Open questions

- Choice of OIDC provider(s) supported on cloud — ADR-0016.
- Backup encryption key escrow — separate ADR.
- LLM provider supply chain (which models, who hosts) — separate ADR before
  any model-using path ships.
