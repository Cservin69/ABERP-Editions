# ADR-0088 — Unattended-write strategy: per-tenant service identity for daemon audit events.

- **Status:** Proposed
- **Date:** 2026-06-16
- **Deciders:** Ervin (decisions locked 2026-06-16 13:57 UTC — Path A; implement **Strategy A** (timestamp-only service key) first, document **Strategy B** (organizational QSeal) as the upgrade path).
- **Implements:** the unattended-write leg of the Path A audit chain. ADR-0087 signs and anchors *operator-session* events; this ADR covers events emitted by background daemons that run **outside any operator session**.
- **Related:** ADR-0087 (session keys + timestamp anchoring — the operator-session sibling whose machinery this reuses), ADR-0086 (operator login — the endorsement happens here), ADR-0082 (snapshot daemon — a chokepoint emitter), ADR-0070 (`DigitalIdProvider` + keychain-mirror pattern), the NAV-poll daemon (`project_nav_poll_daemon_s161`), the calibration WO-complete hook (S429), the retention pruner, AP-sync, the DÁP research (§3.2, §4.3), `[[trust-code-not-operator]]`, `[[no-sql-specific]]`, `[[hulye-biztos]]`.

## Context

Several ABERP subsystems write audit events with **no operator present**:

- the **snapshot daemon** (ADR-0082, every 4h),
- **NAV-poll** (S161, polls the tax authority),
- **AP-sync** (accounts-payable inbound),
- the **calibration WO-complete hook** (S429, fires post-commit),
- the **retention pruner**.

ADR-0087's session key is generated at *operator login* and lives only while an operator is signed in. A daemon firing at 03:00 has no operator and no session key — yet its event must still join the signed, timestamp-anchored chain, or it becomes an **unsigned hole** an auditor (or a court) reads as a gap. The research (§3.2, §4.3) frames the eIDAS-correct primitive for machine-emitted records as an **organizational Qualified Electronic Seal** (Art. 35(2): integrity + correct-origin presumption) — but a full QSeal per daemon event needs a remote-QSCD contract (NETLOCK Sign Enterprise) ABERP does not yet hold.

## Decision

**Open one long-lived "service session" per tenant at binary startup, signed by a per-tenant service key persisted in the macOS Keychain, endorsed once by the operator at login, anchored by NETLOCK qualified timestamps at the same cadence as operator sessions. Daemon events are signed by the service key and timestamp-anchored — NOT individually QSealed. Three new EventKinds (count delta +3). Full organizational QSeal (Strategy B) is documented as a deferred opt-in upgrade tier.**

### Strategy A (chosen) — timestamp-anchored service key

- **Service key:** a per-tenant Ed25519 keypair persisted in the OS keychain at service `aberp.audit_service.<tenant>`, item `service_session_key` — mirroring the existing `aberp.nav.<tenant>` and `aberp.cad.<tenant>` keychain patterns (ADR-0070, ADR-0083). Unlike the operator session key (in-memory, ephemeral — ADR-0087), the service key **persists** because the service session spans process restarts and has no human to regenerate it per boot. Provisioned on first boot from the OS CSPRNG; a malformed stored key is a **loud error, not a silent re-mint** (re-minting would orphan the endorsement linking it to an operator — same posture as the CAD key in ADR-0083).
- **Service identity attestation (the endorsement):** at **operator login** (ADR-0086), a one-time **`ServiceSessionEndorsed`** record is written and anchored *alongside* the normal login anchor. It states: *operator `<dap_subject>` (DÁP-attested) endorses service key `<service_pubkey>` for tenant `<tenant>` as of `<utc>`.* This is what gives daemon events a human root of trust: a court reading a 03:00 snapshot event walks `event → service_pubkey → ServiceSessionEndorsed → DÁP-attested operator`. The endorsement is itself a timestamp-anchored, operator-session-key-signed entry — so it inherits the full ADR-0087 legal weight.
- **Daemon event signing:** every daemon-emitted entry is signed with the service key into the same `event_sig`/`session_pubkey` fields ADR-0087 defined (`session_pubkey` = the service pubkey; `session_id` = the service session's ULID). **No new schema** — the service session reuses ADR-0087's columns and `audit_ledger_anchors` table entirely.
- **Anchoring:** the service session takes login/heartbeat/logout anchors exactly as an operator session — login at startup, heartbeat every `audit_anchor_heartbeat_seconds`, logout at shutdown. NETLOCK timestamps; same cost model (~50 stamps/day per running service session). The heartbeat/queue/retry/never-block machinery is **literally the same code** as ADR-0087; the service session is just another `session_id` flowing through it.

### Strategy B (deferred, documented) — organizational QSeal

Every daemon event gets a real **eIDAS Qualified Electronic Seal** via NETLOCK Sign Enterprise's remote QSCD (server-side seal key, re-authorized via a Signature Activation Module). Art. 35(2) gives each sealed event an *individual* integrity + correct-origin presumption — strictly stronger than Strategy A's "service key + timestamp anchor" bracket. **Cost:** per-seal pricing + a NETLOCK enterprise contract + onboarding. **Recommendation:** offer B as an **opt-in upgrade tier** once ABERP holds a NETLOCK enterprise relationship and a specific defense/aerospace contract demands per-record QSeal on machine writes. The `DigitalIdProvider` trait + the `session_pubkey`/`event_sig` seam already abstract the signer, so swapping the service-key signer for a QSeal signer is a behind-the-trait change, not a schema migration.

**Why A first:** A delivers a continuous, court-anchored chain for daemon writes at timestamp cost (~tens of HUF/day) with no new vendor contract, using machinery ADR-0087 already builds. B's marginal legal gain (per-record vs per-bracket presumption for *machine* events, where no human "signs" anyway) does not justify blocking the first cut on a QTSP enterprise contract. The endorsement bracket gives daemon events a real human root of trust today; B upgrades the *interior* presumption later if a contract demands it.

### Service-session lifecycle

- **Opens** at binary startup: provision/load the service key, open a service session (`session_id` ULID), write **`ServiceSessionOpened`**, take a `login` anchor. This happens **before** any daemon can fire — a daemon event can never precede its session open.
- **Endorsed** at the first operator login of the run: **`ServiceSessionEndorsed`** links the service key to the DÁP-attested operator (above). **Open question for Ervin (flagged):** if daemons fire *before* any operator logs in (e.g. a headless restart of a Defense box that boots straight into NAV-poll), those events are service-key-signed + timestamp-anchored but **not yet operator-endorsed** until someone logs in. They carry `endorsement_state = "pending"` in the anchor so the gap is visible; the endorsement, when it lands, retroactively roots them (the chain already binds them by hash + timestamp). Decided: **fire anyway, mark pending-endorsement** — never block a daemon on the absence of an operator (`[[trust-code-not-operator]]`: a snapshot must run at 03:00 whether or not anyone is logged in). Ervin to confirm this is the desired posture for headless Defense deployments.
- **Heartbeat / crash recovery:** identical to ADR-0087 — heartbeat anchors on cadence; an unclosed service session (crash) is detected on next boot and gets a `crash_recovery` anchor + `auth.session_crash_recovered` (reused from ADR-0087, no new kind).
- **Closes** at graceful shutdown: **`ServiceSessionClosed`**, `logout` anchor, `closed_at_utc` set.

### New EventKinds (3; count delta +3)

| Variant (`as_str`) | When | Payload sketch |
|---|---|---|
| `auth.service_session_opened` | binary startup, service key loaded, login anchor taken | `{ tenant, service_pubkey, session_id, opened_at_utc }` |
| `auth.service_session_endorsed` | first operator login endorses the service key | `{ tenant, service_pubkey, operator_dap_subject, endorsed_at_utc }` |
| `auth.service_session_closed` | graceful shutdown | `{ tenant, session_id, closed_at_utc }` |

All `auth.*`, app-only JSON (never NAV XML), full F12 ritual, both NAV-leakage arms classify them app-only, count pins bump to the running total. Crash recovery reuses ADR-0087's `auth.session_crash_recovered` (no fourth kind).

## Acceptance criteria

1. A service session opens at startup **before** any daemon fires; `auth.service_session_opened` is the first entry of the run when no operator is yet present (proven by an ordering test).
2. Daemon events (snapshot, NAV-poll, calibration hook, retention pruner, AP-sync) are signed with the service key into `event_sig`/`session_pubkey` and pass ADR-0087's membership rule in `verify_chain`.
3. The first operator login fires `auth.service_session_endorsed` linking the service pubkey to the DÁP-attested operator; a chain walk from a daemon event reaches the operator via the endorsement.
4. Daemon events fired before any login carry `endorsement_state = "pending"` and are **not blocked**; a later login roots them without rewriting them.
5. The service key is keychain-persisted (`aberp.audit_service.<tenant>`); a malformed stored key is a loud error, never a silent re-mint.
6. The service session heartbeats, queues-on-TSA-outage, and crash-recovers using ADR-0087's machinery (no duplicated logic); three EventKinds pass F12 + both NAV arms; count pins reflect the running total.

## Consequences

- **Positive:** daemon writes join the same signed, timestamp-anchored chain as operator writes — no unsigned 03:00 holes — at timestamp cost and with no new vendor contract; the endorsement bracket gives machine events a DÁP-rooted human anchor; all of ADR-0087's heartbeat/recovery/verify machinery is reused, not reimplemented.
- **Accepted limitation:** a daemon event before the first login is timestamp-anchored but only *pending*-endorsed until an operator signs in. For a headless Defense box that may run for hours unattended, the human root of trust is retroactive. This is strictly better than today's unsigned daemon writes, and the `pending` state makes the gap auditable rather than invisible (CLAUDE.md rule 12).
- **Deferred — Strategy B (organizational QSeal):** documented as the opt-in upgrade once a NETLOCK enterprise relationship + customer demand exist. The signer seam makes it a behind-the-trait swap.
- **Open questions for Ervin:** (1) the pre-login daemon posture (fire-with-pending-endorsement vs block until first login) — recommended: fire-with-pending. (2) On a multi-operator box, does *any* operator's login endorse the service key, or only a designated "service-responsible" operator (research §7)? Recommended: any operator endorses, but the endorsing `dap_subject` is recorded — a named-responsible-operator policy can be layered later in tenant config.
