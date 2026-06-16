# ADR-0087 — Timestamp-anchored audit chain: session keys + NETLOCK qualified-timestamp anchoring.

- **Status:** Proposed
- **Date:** 2026-06-16
- **Deciders:** Ervin (decisions locked 2026-06-16 13:57 UTC — **Path A**: DÁP identity once per login + NETLOCK qualified-timestamp anchoring at login/heartbeat/logout, court-admissible under HU `Pp. § 325(1) f)` burden-shift; **not** a full per-session QES bracket).
- **Implements:** the court-admissibility goal of the Defense pivot. Consumes the operator identity ADR-0086 establishes; gives the existing SHA-256 hash-chained `audit-ledger` (ADR-0008) a *statutory* integrity presumption via eIDAS Art. 41(2) qualified timestamps.
- **Related:** ADR-0008 (audit ledger — `Entry` shape, `entry_hash`/`prev_hash` chain, `verify_chain`), ADR-0070 (`DigitalIdProvider` + the unused `Signed<T>`/`DigitalIdRef` wrapper this ADR finally populates), ADR-0086 (DÁP login — the session opener), ADR-0088 (unattended daemon writes — the service-session sibling), ADR-0081 (`aberp-verify` NAV-leakage gate), ADR-0082 (snapshot system — anchors must survive restore), the DÁP research findings (§3, §4), `[[trust-code-not-operator]]`, `[[no-sql-specific]]`, `[[hulye-biztos]]`.

## Context

The audit ledger today is a SHA-256 hash chain: each `Entry` carries `prev_hash` + a computed `entry_hash` over the canonical CBOR encoding (RFC 8949) of its other fields; `verify_chain` walks the links and recomputes each hash. This gives **tamper-evidence** — but, per the research (§4.7), a hash chain alone is *doctrinal* not *statutory*: it proves the record wasn't altered after entry, but carries no legally-presumed time or integrity. **Court weight comes from a qualified timestamp/seal over the chain head, not the hash chain** (eIDAS Art. 41(2) + HU `Pp. § 325(1) f)` + `§ 326`'s burden-shift: a qualified-anchored record is *deemed un-falsified until the contrary is proven*).

Path A's principle: **DÁP-attested-login → N session-key-signed events → qualified-timestamp anchors at login/heartbeat/logout.** A network round-trip per audit append is avoided; the legal binding comes from the endorsement bracket + periodic qualified timestamps.

Four facts from the codebase shaped the design (verified, not assumed):

1. **`entry_hash`'s preimage is a fixed-key canonical map.** `canonical.rs` encodes a fixed set of keys (incl. `idempotency_key`, emitted as CBOR `null` when `None`). **Adding any key to that map changes every entry's preimage** — every legacy entry's stored `entry_hash` would fail re-verification, and `chain_conformance` would break. Therefore the new signing fields **must NOT enter the `entry_hash` preimage.** (See Decision §"Why the signature is a separate layer.")
2. **`Signed<T> { payload, signer: Option<DigitalIdRef> }` and `DigitalIdRef` already exist** (ADR-0070) and are **unused** — built precisely so a future event opts in. This ADR is that future. But `DigitalIdRef` is identity metadata, not a cryptographic signature carrier; the actual `event_sig`/`session_pubkey` are new fields (see schema).
3. **`Actor.session_id` already exists** as a *per-process* identifier. The brief's `session_id` is a *per-login* grouping. These are **different concepts** — a naming collision that must be surfaced, not blended (CLAUDE.md rule 7). See Decision §"The `session_id` collision."
4. **Snapshots are logical `EXPORT DATABASE`** (ADR-0082), not file copies. New columns + the new anchors table are captured by `EXPORT DATABASE` automatically, but a *restore* must not orphan an open session — see Consequences.

## Decision

**Generate a software Ed25519 session key in memory at login; sign every audit entry of that session with it; persist three additive nullable fields (`session_id`, `session_pubkey`, `event_sig`) carried alongside `Entry` but excluded from the `entry_hash` preimage; and anchor the chain head with a NETLOCK RFC-3161 qualified timestamp at login, every 15 minutes during the session, and at logout — persisted in a new `audit_ledger_anchors` table. `verify_chain` extends to verify both the per-entry signatures and the anchor timestamps. Five new `auth.*`/`audit.*` EventKinds (count delta +5).**

### Session key — software Ed25519, in-memory only

- **Ed25519, software-generated** (`ed25519-dalek`) at login, held **only in process memory**, never written to disk and **never to the keychain**, zeroized on logout/drop.
- **Rationale (decided, flagged):** macOS Secure Enclave is **P-256-only** and needs an App-Store/Team-ID code-signing chain ABERP does not yet have. Software Ed25519 is simpler, portable, and adequate for the threat model: the session key's trust derives from the **login-endorsement bracket + qualified-timestamp anchor**, not from hardware provenance. If the operator's machine is compromised at the OS level, all signing keys on it are forfeit regardless of enclave residency — so the enclave buys little here.
- **Revisit Secure-Enclave-P256** when Defense ships on Linux/Windows (different HW story) or once ABERP gains code-signing infrastructure, *or* if the threat model is upgraded to "defend against malware forging audit signatures as the live operator" (the one case where a non-exportable HW key earns its P-256-only constraint). Noted, not chosen now.

### Audit-ledger schema additions (additive, nullable, [[no-sql-specific]])

Three fields land on `Entry` and the `audit_ledger` table, **all nullable** (no SQL `DEFAULT`, the DuckDB replay-clobber trap):

| Field | Type | Meaning |
|---|---|---|
| `session_id` | `Option<String>` (ULID) | groups all entries written under one login/service session |
| `session_pubkey` | `Option<String>` (hex) | the session's Ed25519 public key — **denormalised**, embedded on every entry for O(1) verification without a join to the anchors table |
| `event_sig` | `Option<String>` (hex) | Ed25519 signature, preimage below |

`event_sig` signs the preimage `prev_hash || kind.as_str() || subject || payload_hash`, where `subject` is the deterministic S424 subject extractor (`audit_summary::subject_of`) and `payload_hash = SHA-256(payload)`. (The brief's `subject` is honoured literally; `payload_hash` already commits to the full payload, so `subject` is a stable, fast-to-recompute redundancy aiding human-readable verification reports.) Legacy + unsigned entries leave all three `None`. New `audit_ledger_anchors` table (additive):

```
audit_ledger_anchors(
  anchor_id        TEXT  PRIMARY KEY,   -- ULID
  session_id       TEXT  NOT NULL,
  tenant_id        TEXT  NOT NULL,
  anchor_kind      TEXT  NOT NULL,      -- 'login' | 'heartbeat' | 'logout' | 'crash_recovery'
  chain_head_hash  TEXT  NOT NULL,      -- the entry_hash this anchor commits to
  anchored_payload TEXT  NOT NULL,      -- the exact bytes timestamped (see cadence)
  tsa_token        BLOB,                -- RFC-3161 TimeStampToken; NULL while queued/pending
  tsa_status       TEXT  NOT NULL,      -- 'pending' | 'anchored' | 'failed'
  opened_at_utc    TEXT  NOT NULL,
  closed_at_utc    TEXT                 -- NULL = open session (the crash-recovery signal)
)
```

Chain invariants live in Rust `verify_chain`, **not** in CHECK constraints (`[[no-sql-specific]]`).

#### Why the signature is a separate layer (not folded into `entry_hash`)

Per fact #1, the `entry_hash` preimage is a fixed-key map; adding `event_sig` to it breaks every legacy entry. So **`entry_hash` is unchanged** — it remains the existing tamper-evidence layer over the semantic fields. `event_sig` is an **independent** integrity layer: it signs a preimage that itself includes `prev_hash` (chaining it to the link structure). This is also why the brief's `event_sig` signs `prev_hash || kind || subject || payload_hash` rather than the full canonical entry — the two layers are deliberately decoupled.

The one attack this opens — *strip the signature without breaking the hash chain* — is closed by **session membership**: an anchor records the session's `session_pubkey` and time bounds; `verify_chain` requires that **every entry whose `session_id` falls in an anchored session carries a valid `event_sig` under that session's pubkey.** A stripped (`None`) or foreign-key signature inside a signed session range is a verification failure. The membership rule, not the hash preimage, is what makes signatures non-strippable. (Rejected alternative: versioning the canonical encoding to a v2 that includes the new keys — heavier, needs a per-entry version discriminator, and gains nothing over the membership rule.)

#### The `session_id` collision (surfaced, not blended)

`Actor.session_id` already exists as a **per-process** id (fact #3). The new `Entry.session_id` is a **per-login/per-service** grouping — distinct lifetime (one process can host several logins; a service session spans the whole process). They are **not** merged. The new field is named `session_id` (per the brief + the anchors table FK). **Recommendation (out of scope here, flagged for Ervin):** a later cleanup renames `Actor.session_id` → `Actor.process_id` to kill the collision; this ADR does not touch `Actor` (rule 3, surgical). **Open question for Ervin** — accept the temporary two-`session_id` ambiguity, or rename `Actor.session_id` in the implementation session?

### NETLOCK qualified-timestamp anchor cadence

A NETLOCK RFC-3161 qualified TSA (≈15–18 HUF/stamp) timestamps a defined payload at three (+one recovery) moments. The `tsa_token` is the eIDAS Art. 41(2) anchor:

- **LOGIN** — timestamp over `operator_dap_subject || tenant || session_id || session_pubkey || login_at_utc`. Persisted as an `audit_ledger_anchors` row (`anchor_kind='login'`, `closed_at_utc=NULL`). This binds the operator identity (ADR-0086) to the session key — the binding DÁP itself cannot mint (research §3.1).
- **HEARTBEAT — every 15 minutes** during an active session — timestamp over `session_id || current_chain_head_hash || heartbeat_at_utc`. Each is a Certificate-Transparency-style signed checkpoint: an abrupt termination still leaves a recent qualified-anchored head.
- **LOGOUT / graceful shutdown** — timestamp over `session_id || final_chain_head_hash || logout_at_utc`; sets `closed_at_utc`.

The heartbeat interval is **configurable per tenant** via `audit_anchor_heartbeat_seconds` (default 900). **Cost:** a 12-hour shift ≈ login + ~48 heartbeats + logout ≈ **50 stamps ≈ 750–900 HUF/day/operator** — locked-acceptable.

### Heartbeat failure (NETLOCK unreachable) — never block audit writes

The local hash chain + per-entry signatures continue regardless of TSA reachability. On a failed timestamp request:

- Queue the request locally (the anchor row persists with `tsa_status='pending'`, `tsa_token=NULL`, `anchored_payload` = the exact bytes to stamp). **Retry every minute.** Backfill `tsa_token` when NETLOCK is reachable.
- Each unavailability emits **`audit.timestamp_anchor_delayed`**. Reaching **X consecutive failures (default 10 ≈ 10 minutes)** escalates to an operator-visible banner ("Qualified timestamping unavailable — audit integrity preserved locally, anchors will backfill").
- **The audit write path NEVER blocks on the TSA.** A pending anchor is an audit-noted, self-healing gap — not a chain break, not a write stall.

### Crash recovery (no logout anchor)

On boot, scan `audit_ledger_anchors WHERE closed_at_utc IS NULL` (an open session with no clean logout). For each:

1. Emit **`auth.session_crash_recovered`**, signed by the **new** boot session's key, chaining to the recovered chain's terminal entry.
2. Take a NETLOCK timestamp over the recovered chain's terminal `entry_hash` → a `crash_recovery` anchor row.
3. Mark the old session `closed_at_utc = <recovery time>`, `anchor_kind='crash_recovery'`.

**Legal interpretation:** chain integrity is preserved via the last heartbeat anchor + the recovery timestamp; the missing logout is an **audit-noted irregularity, not a chain break** (research §3.4 — ABERP already has the synthetic-state precedent, the NAV-off `"nav-disabled"` Ready state).

### Chain verification extends

`verify_chain` keeps its existing two checks (per-entry `entry_hash` recompute + `prev_hash` link) and **adds**:

- **(a) per-entry signature:** for every entry with a non-null `session_id`, verify `event_sig` against that session's `session_pubkey` (membership rule above — missing/foreign sig inside a signed session ⇒ fail).
- **(b) anchor verification:** for every `audit_ledger_anchors` row, verify the RFC-3161 `tsa_token` against NETLOCK's qualified **TSA certificate** and confirm its `messageImprint` matches the stored `anchored_payload`/`chain_head_hash`. A `pending` anchor verifies as "integrity-local, qualified-anchor-pending" — not a failure, a flagged state.

For large ledgers, "Verify chain" is an **operator action with a progress bar**, run on a **background thread**, reporting cumulative progress (mirrors ADR-0082's restore-wizard ergonomics). The verdict distinguishes *chain intact + fully anchored* / *chain intact + anchors pending* / *tamper at seq N*.

### New EventKinds (5; count delta +5)

| Variant (`as_str`) | When |
|---|---|
| `auth.session_opened` | session key generated, login anchor taken |
| `auth.session_closed` | clean logout, logout anchor taken |
| `auth.session_crash_recovered` | boot found an unclosed session |
| `audit.timestamp_anchor_taken` | a login/heartbeat/logout/recovery anchor succeeded |
| `audit.timestamp_anchor_delayed` | a TSA request failed and was queued |

Full F12 ritual for each; both NAV-leakage arms classify all five **app-only, never NAV XML**; the `ALL_KINDS_COUNT` pin and both `const _` drift assertions bump to this ADR's running total (see ADR-0086 §5 for the combined-delta sequencing).

## Threat model & assumptions

- **In scope:** detecting any post-hoc alteration of audit records (hash chain + signatures); proving *when* the chain head existed with statutory force (qualified timestamp, Art. 41(2)); binding *which operator* opened the session (DÁP identity in the login anchor); surviving crashes without a chain break.
- **Out of scope / accepted:** an operator's machine compromised at OS level can forge signatures *for the live session* (in-memory key) — the same exposure any client-side signer has; mitigated only by upgrading to Secure-Enclave/HW keys later. The qualified timestamp does **not** prove the *entered content is true* (GIGO, research §4.7) — only that it wasn't altered after entry. The legal bar targeted is an **internal court-admissible audit trail** (integrity presumption + burden-shift), **not** per-session personal-QES non-repudiation (research Candidate B, deferred).
- **TO-BE-CONFIRMED:** NETLOCK Sign-Enterprise/TSA REST details came from marketing copy, not a developer-portal read; confirm the RFC-3161 endpoint, auth, and qualified-TSA cert chain at onboarding. NETLOCK account onboarding runs in parallel (no prior QTSP account).

## Acceptance criteria

1. A session generates an in-memory Ed25519 key at login, signs every entry's `event_sig`, and zeroizes the key at logout/drop (proven by a test asserting the key material is gone post-close).
2. `entry_hash` for legacy entries is **byte-identical** before and after the schema change (a fixture-replay test pins it); `chain_conformance` stays green; the three new fields are `Option`, absent from the `entry_hash` preimage.
3. `verify_chain` fails when (a) any signed-session entry has a missing/foreign `event_sig`, (b) an anchor's `tsa_token` doesn't match its `chain_head_hash`; and reports `pending` anchors as a distinct non-failure state.
4. Login/heartbeat/logout each write an `audit_ledger_anchors` row with the specified payload; the 15-min heartbeat fires on `audit_anchor_heartbeat_seconds` (default 900); a forced TSA outage queues the anchor (`pending`), retries every 60s, fires `audit.timestamp_anchor_delayed`, and backfills `tsa_token` on recovery — **without ever blocking an audit write**.
5. A simulated crash (kill mid-session) is detected on next boot: `auth.session_crash_recovered` fires, a `crash_recovery` anchor is taken, the old session is closed.
6. Five EventKinds pass the F12 ritual + both NAV arms; the count pins reflect the running total.
7. A snapshot taken mid-session and restored does not corrupt verification (open-session anchors restore as open; the restoring boot treats them via crash-recovery — see Consequences).

## Consequences

- **Positive:** the hash chain gains a *statutory* integrity + time presumption (Art. 41(2) + `Pp. § 326` burden-shift) at ~750–900 HUF/day/operator, with **no per-login QES ceremony** beyond the ~10s DÁP scan; legacy entries are untouched (signature is a separate layer); the unused `Signed<T>` foundation from ADR-0070 finally earns its place; verification is a real operator action, not a faith statement.
- **Snapshot/restore interaction (ADR-0082):** `EXPORT DATABASE` captures the new columns + anchors table automatically. A restore of a snapshot taken **mid-session** lands an open-session anchor (`closed_at_utc=NULL`); the restoring binary's crash-recovery scan closes it and re-anchors — correct by construction, no special-casing. Flagged so the implementation session adds a restore-mid-session test.
- **Deferred:** Secure-Enclave/HW session keys (revisit triggers above); per-session personal QES (Candidate B, opt-in tier); **qualified long-term preservation** (eIDAS Art. 34 / NETLOCK qualified preservation) so anchors stay verifiable past the TSA cert's validity — Defense retention is measured in decades, the cert is not; this is a real future ADR, called out now (research §6 Q8).
- **Open questions for Ervin:** (1) heartbeat default — 15 min as specified, or 30 min to halve stamp cost? (2) the `Actor.session_id` rename (above). (3) Is qualified long-term preservation (Art. 34) in the near-term roadmap, or accepted as a later slice?
