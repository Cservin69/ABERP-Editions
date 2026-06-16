# DÁP-backed login + court-admissible audit chain — Path A design (ADR-0086/0087/0088)

**Date:** 2026-06-16
**Status:** DESIGN — three Proposed ADRs, awaiting Ervin sign-off. No Rust/SPA code changed; implementation session(s) follow approval.
**Branch:** `session-adr-dap-path-a` (no origin push).
**Inputs:** decisions locked by Ervin 2026-06-16 13:57 UTC (Path A); research findings `docs/findings/dap-research-2026-06-16.md`.

## The one-paragraph version

A Defense operator logs in once with the DÁP app (~10s QR scan). That login mints an in-memory Ed25519 **session key** and a NETLOCK **qualified timestamp** binding the DÁP identity to that key. Every audit event the operator triggers is signed by the session key; the running chain head is re-stamped by NETLOCK every 15 minutes and at logout. Background daemons (snapshot, NAV-poll, calibration, retention) get their own per-tenant **service key**, endorsed by the operator at login and anchored on the same cadence — so there are no unsigned 03:00 holes. The hash chain proves *nothing was altered*; the qualified timestamp gives that an eIDAS Art. 41(2) **statutory integrity + time presumption**, which under HU `Pp. § 325(1) f)` + `§ 326` shifts the burden of proving forgery onto the challenger. That is the court-admissibility bar — reached **without** a per-event signing ceremony.

## How the three ADRs compose

```
   ADR-0086  DÁP eAzonosítás login  ──establishes──▶  operator identity (DÁP subject)
        │                                                      │
        │ opens a session                                      │ endorses
        ▼                                                      ▼
   ADR-0087  session key + timestamp anchors  ◀──signs/anchors──  operator events
        │  (login → N signed events → heartbeat/logout anchors)
        │  reuses the SAME machinery for ▼
        ▼
   ADR-0088  service session (per-tenant key)  ──signs/anchors──▶  daemon events
```

- **ADR-0086 (identity):** *who* — a real `DigitalIdProvider` backend (`DapProvider`) doing DÁP eAzonosítás over an OpenID4VP-style mobile flow, loopback callback (`127.0.0.1:0`, OS-firewall-friendly, desktop-app-correct), `DAP_ENV=sandbox|production`, and a code-gated `dap_bypass_allowed` emergency fallback so DÁP is not a shop-floor single point of failure. 4 new `auth.*` EventKinds.
- **ADR-0087 (integrity — the marquee):** *unaltered, and when* — software Ed25519 session key in memory; three additive nullable fields (`session_id`, `session_pubkey`, `event_sig`) carried alongside `Entry` but **deliberately outside the `entry_hash` preimage** (so legacy entries hash byte-identically); a new `audit_ledger_anchors` table holding NETLOCK RFC-3161 qualified timestamps at login / 15-min heartbeat / logout; TSA-outage queue-and-retry that **never blocks an audit write**; crash recovery; and an extended `verify_chain` (per-entry signatures + anchor timestamps, run as a progress-bar operator action). 5 new EventKinds.
- **ADR-0088 (unattended writes):** *daemons too* — a long-lived per-tenant service session with a keychain-persisted service key, endorsed once by the operator at login, anchored on the same cadence, reusing ADR-0087's machinery wholesale. Strategy A (timestamp-anchored service key) ships first; Strategy B (per-event organizational QSeal via NETLOCK Sign Enterprise) is the documented opt-in upgrade. 3 new EventKinds.

**EventKind budget:** 148 → **160** if all three implement (+4, +5, +3). Each ADR bumps `ALL_KINDS_COUNT` + both NAV-leakage `const _` drift assertions to its running total; every new kind is app-only JSON, never NAV XML (re-reviewed in both exhaustive arms per ADR-0081).

## The two design decisions that carry the most weight

1. **The signature is a layer *beside* the hash, not *inside* it.** `canonical.rs` builds `entry_hash`'s preimage from a fixed-key map; adding any key breaks every legacy entry's stored hash and the `chain_conformance` suite. So `event_sig` signs its own preimage (`prev_hash || kind || subject || payload_hash`) and `entry_hash` is untouched. The "strip the signature" attack is closed by a **session-membership rule** in `verify_chain` (every entry inside an anchored session must carry a valid sig under that session's pubkey), not by folding the sig into the hash. This is what keeps the change additive and legacy-safe.
2. **Qualified timestamp, not per-session QES.** Path A reaches the *integrity-presumption + burden-shift* bar via Art. 41(2) timestamps at ~750–900 HUF/day/operator and no signing ceremony beyond the DÁP scan. Per-session **personal QES** (research Candidate B — maximum non-repudiation, `Pp. § 326` on the operator's attestation itself) is deferred to an opt-in tier behind the same `DigitalIdProvider` trait, for contracts that contractually require it.

## Open questions for Ervin (carried from the ADRs)

1. **Heartbeat interval default** — 15 min (≈50 stamps/day/operator, as specified) or 30 min to roughly halve stamp cost? (ADR-0087; per-tenant configurable either way via `audit_anchor_heartbeat_seconds`.)
2. **`Actor.session_id` naming collision** — the pre-existing per-process `Actor.session_id` clashes with the new per-login `Entry.session_id`. Accept the temporary two-`session_id` ambiguity, or rename `Actor.session_id → Actor.process_id` in the implementation session? (ADR-0087.)
3. **Pre-login daemon writes on headless Defense boxes** — fire-with-`pending`-endorsement (recommended, never blocks a daemon) vs block daemons until the first operator login? (ADR-0088.)
4. **Service-key endorser policy** — any operator's login endorses the per-tenant service key (recommended; endorser `dap_subject` recorded), or only a designated service-responsible operator? (ADR-0088.)
5. **Qualified long-term preservation (eIDAS Art. 34)** — Defense retention is decades; the NETLOCK TSA/DÁP certs are 3-year. Is qualified preservation a near-term ADR or an accepted later slice? (ADR-0087 Consequences.)
6. **Strategy B timing** — confirm organizational QSeal stays deferred until a NETLOCK enterprise contract + a contract that demands per-event seals exist. (ADR-0088.)

## To-be-confirmed (needs szeusz.gov.hu RP access / NETLOCK onboarding)

- **DÁP protocol surface** — exact endpoints, scope/claim identifiers, credential format (mdoc vs SD-JWT-VC), redirect-URI rules, and whether KAÜ exposes a client-controllable OIDC `nonce` (for the optional cheap transitive key-binding) or is SAML-only. Quarantined behind ADR-0086's `DapTransport` seam — confirming it is a single-impl edit. The ADR assumes **OpenID4VP 1.0 + DCQL** (the 2026 DÁP direction).
- **NETLOCK Sign-Enterprise / qualified-TSA REST details** — RFC-3161 endpoint, auth, and the qualified-TSA certificate chain `verify_chain` checks against. Sourced from marketing copy in research; confirm at onboarding (no prior QTSP account — onboarding runs in parallel).
- **HU statute citations** — re-pull eIDAS Art. 25/35/41 verbatim from EUR-Lex CELEX 02014R0910-20241018, and confirm `Pp. § 325/326` numbering, before any of these appear in a customer-facing legal memo. ("Act CLXXII of 2024" from the original brief was **not** found and is treated as erroneous; controlling statutes are Eüsztv. 2015. CCXXII. and Pp. 2016. CXXX.)
