# ADR-0113 — `internal.abenerp.com`: an undiscoverable remote portal over an outbound-only relay (Phase 0: tunnel + health; Phase 1: read-only invoices)

- **Status:** **Proposed — design only.** No code exists; nothing in this ADR is
  implemented. The build is queued as backlog entry
  [D-17](../docs/BACKLOG-designed-to-live.md#d-17) and does **not** start until
  the current backlog is worked through. Open decisions for Ervin are collected
  in §9 and flagged `⚑` inline.
- **Date:** 2026-08-17
- **Deciders:** Ervin Áben (intent: a remote, authenticated, phone-usable door
  to the invoicing Mac that works whether or not the ABERP process is running;
  undiscoverable by subpage scans; "the most secure we currently imagine").
  Design drafted by Dispatch; conservative option recommended wherever a real
  product decision exists, each one flagged rather than silently taken.
- **Related:** ADR-0007 (security baseline), `docs/threat-model.md` (assets,
  actors, trust boundaries — gains three new boundaries when this builds),
  ADR-0047 (keychain-only secrets; the no-secrets-in-cloud posture this must
  not weaken), ADR-0088 (unattended service identity — the pattern the Mac
  agent's local credential follows), ADR-0082 (snapshot daemon — a DB-health
  signal the agent can surface), ADR-0058 (virtual union invoices list — the
  read surface Phase 1 renders), ADR-0016 / ADR-0059 (cloud-sync and SaaS
  posture: cloud components hold **no** business data at rest).

---

## 0. TL;DR

A new surface, `internal.abenerp.com`, reachable from any browser (primarily
Ervin's iPhone), that lands on the Mac running ABERP **without opening a single
inbound port on that Mac**. The Mac runs a small always-on **agent daemon**
(separate from the ABERP process) that dials **outbound** to a **relay** on a
VPS — the same shape as Dispatch's phone↔local-agent link: outbound dial to a
broker, zero listening sockets on the Mac, nothing for a port scan to find.
The relay brokers browser sessions through to that specific Mac.

Three properties carry the whole design:

| Property | How it is achieved |
|---|---|
| **Undiscoverable** | Separate subdomain never referenced by the storefront; wildcard TLS cert so Certificate Transparency logs never publish the label; every request that has not passed the pre-auth gate gets the host's uniform 404 — byte-identical whether the portal exists, the Mac is offline, or the path is garbage |
| **No inbound exposure** | The Mac only ever dials out (WSS + mTLS, both ends pinned). The frozen prod invoice box gains **zero** new listening ports; the storefront (`abenerp.com`, CloudFront→Lightsail, repo ABERP-site) is untouched |
| **Nothing worth stealing in the cloud** | The relay is a dumb authenticated pipe: no business data at rest, no WebAuthn credential store, no session issuance. The **agent on the Mac is the WebAuthn relying party** — auth state lives where the data already lives |

Auth is WebAuthn platform passkeys — Face ID on iPhone, Touch ID on the Mac —
one integration, the OS picks the modality, keys in the Secure Enclave, RP ID
bound to `internal.abenerp.com`. Recommended posture (⚑ §9.1): **passwordless,
passkey-only** — no password exists to phish, no fallback weaker than the
front door. Recovery is physical possession of the Mac (§4.5).

Phase 0 ships the tunnel, the gate, and an ABERP up/down health card — useful
on day one precisely because it works when ABERP is stopped. Phase 1 adds the
read-only invoice pages by proxying four **existing** `GET` routes of
`serve.rs` through an agent-enforced allowlist (§6). The agent refuses every
non-`GET` verb; Phase 1 structurally cannot write.

---

## 1. Security goals and threat model

### 1.1 Goals, ranked

1. **G1 — No new inbound attack surface on the invoicing Mac.** The box is
   frozen prod; its exposure must be additive-zero. This outranks everything,
   including availability of the portal itself.
2. **G2 — Unauthenticated observers cannot establish the portal exists.**
   "Undiscoverable by subpage scans": no link from the storefront, no CT-log
   leak, no fingerprintable response, no sitemap/robots mention, no directory
   listing, no distinguishing header or TLS artifact.
3. **G3 — Authentication is phishing-resistant and origin-bound.** WebAuthn's
   RP-ID binding means a credential registered to `internal.abenerp.com`
   cannot be replayed to a look-alike host even by a user who is fooled.
4. **G4 — A fully compromised relay/VPS yields no durable secrets and no
   standing access.** The relay stores nothing; what transits it in memory is
   bounded by §2.4 and closed by the H1 hardening (§7).
5. **G5 — Phase 1 is read-only by construction, not by convention.** The
   refusal of mutating verbs lives in the agent on the Mac — inside the trust
   boundary — not in cloud code an attacker could alter.

### 1.2 Attackers, and what defeats each

| Attacker | Capability assumed | What defeats them |
|---|---|---|
| **Internet scanner** (masscan, shodan, subdomain bruteforce) | Finds the VPS IP, resolves `internal.abenerp.com` (the label *will* be found — see ⚑ §9.2), probes every port and path | The Mac has no inbound ports at all (G1). The VPS answers every unauthenticated request — any path, any method, any SNI — with one uniform 404 (§3.2). Scanner's conclusion: parked host |
| **Someone who learns the subdomain** (shoulder-surf, DNS enumeration, leaked bookmark *name*) | Knows the exact hostname, browses to it | Same uniform 404 without the pre-auth knock (§3.3). With the knock but no passkey: a WebAuthn challenge that cannot be satisfied — no username field, no password form, nothing to guess or stuff |
| **Stolen iPhone / stolen Mac** | Physical device with an enrolled passkey | The passkey never releases without the biometric (Secure Enclave gates on Face ID / Touch ID); a device passcode alone does not exercise Face ID for a passkey assertion without presenting *some* user verification. Residual risk and revocation path in §4.6 |
| **Relay / VPS compromise** (hosting provider breach, stolen VPS credentials, malicious hoster insider) | Root on the relay box; can read its memory, alter its code | No credential store, no session issuance, no data at rest to take (G4). Cannot mint a session: WebAuthn verification happens on the agent. Can observe/tamper traffic transiting its memory → residual risk bounded in §2.4, closed by H1 (§7) |
| **Malicious insider / supply chain on the relay code** | Ships a hostile relay build | Same as above: the relay was never trusted with verification or storage, so a hostile relay degrades to the compromised-relay row |
| **Network MITM** (hostile Wi-Fi, rogue CA) | Intercepts either leg | Browser↔VPS: public TLS + HSTS. Agent↔relay: mTLS with **both** peers pinned — the agent refuses any relay cert but the pinned one, rogue-CA certs included (§2.3) |
| **Malicious authenticated user** | Ervin's own session, or a future second enrollee | Phase 1: the agent's `GET`-only allowlist (§6.3) — a valid session cannot mutate anything. Every proxied request is audit-logged on the Mac (§6.5) |

### 1.3 Explicit non-goals

- **Availability under VPS loss is not a goal.** Relay down → portal down →
  every probe gets the same 404 it always got. Fail-invisible, not fail-open.
- **Multi-operator / multi-tenant serving is not designed here.** One Mac, one
  operator, N enrolled devices. The seams that would widen this are noted
  (§7), not built.
- **Defending a compromised enrolled device is out of scope.** If the iPhone
  itself runs hostile code while Ervin authenticates, no transport design
  survives that; ABERP's own audit ledger is the after-the-fact control.

---

## 2. Transport architecture

### 2.1 The three legs

```
[iPhone/desktop browser]                    [VPS — cloud, untrusted-ish]              [Mac — trusted, frozen prod]
        │                                        │                                        │
        │  Leg A: HTTPS (public TLS,             │                                        │
        │  wildcard *.abenerp.com cert,          │                                        │
        │  HSTS, TLS 1.3)                        │                                        │
        ├───────────────────────────────────────►│ front + relay (one small binary)       │
        │                                        │  - pre-auth gate (§3.3)                │
        │                                        │  - serves static portal shell          │
        │                                        │  - forwards opaque frames              │
        │                                        │  - stores NOTHING                      │
        │                                        │◄───────────────────────────────────────┤
        │                                        │  Leg B: persistent outbound WSS,       │
        │                                        │  mTLS — agent's client cert pinned     │
        │                                        │  by relay, relay's cert pinned by      │
        │                                        │  agent. Dialed BY the Mac. Auto-       │
        │                                        │  reconnect with jittered backoff.      │
        │                                        │                                        │
        │                                        │                            [agent daemon (launchd)]
        │                                        │                                        │
        │                                        │                                        │  Leg C: localhost HTTP
        │                                        │                                        ├──────────► ABERP serve.rs
        │                                        │                                        │            (127.0.0.1 only,
        │                                        │                                        │            existing bearer)
```

- **Leg A** — browser to VPS. Ordinary public HTTPS. The *content* served is
  gated (§3); the transport is not distinguishable from any parked host.
- **Leg B** — the load-bearing leg, and the Dispatch pattern verbatim: the
  **Mac dials out**, holds one persistent WebSocket-over-TLS connection, and
  the relay multiplexes browser sessions down it as opaque framed streams.
  Nothing listens on the Mac. When the tunnel is down, the Mac is — from the
  internet's point of view — not there.
- **Leg C** — agent to the local ABERP process over loopback, exactly as the
  Tauri shell talks to it today (`Authorization: Bearer` per `serve.rs:19`).
  Exists only while proxying Phase-1 reads; the health probe also lives here.

### 2.2 The agent daemon (Mac side)

A new, small, separate binary — **not** part of the ABERP process — installed
as a launchd daemon with `KeepAlive`, so it runs from boot and survives ABERP
being stopped, crashed, or upgraded. That separation is what delivers the
"whether or not ABERP is running" requirement: the agent's liveness is the
portal's liveness; ABERP's liveness is merely a *status the agent reports*.

Agent responsibilities, complete list — anything not here is refused:

1. Maintain Leg B (dial, mTLS-verify the pinned relay, reconnect on drop).
2. Answer health queries (§5) from its own observations — no ABERP needed.
3. Act as the **WebAuthn relying party** (§4): store credential public keys,
   issue challenges, verify assertions, mint and validate portal sessions.
4. Proxy allowlisted `GET`s to `serve.rs` over loopback (§6), attaching the
   existing session bearer read from the macOS keychain (service per-tenant,
   account `session_token`, `serve.rs:160` / `serve.rs:3792`) — the same
   keychain-only posture as ADR-0047. The bearer never leaves the Mac.
5. Append an audit record for every proxied request and every auth ceremony
   (§6.5).

Its mTLS client key is generated on the Mac and lives in the macOS keychain
(pattern: ADR-0088's per-tenant service key — provisioned once, loud error on
corruption, never silently re-minted).

### 2.3 Mutual authentication of Leg B

- The **relay pins the agent**: exactly one client certificate (or a
  short allowlist for a future second Mac) is accepted; anything else is
  dropped *before* any application byte, indistinguishable from a closed
  service.
- The **agent pins the relay**: the relay's certificate (or its dedicated
  private issuing key) ships in the agent's config. The public WebPKI is not
  consulted on Leg B, so a mis-issued public cert for the relay's hostname
  buys an attacker nothing. This is deliberately *stricter* than the NAV leg
  posture (threat-model boundary 3, pinned issuing root): here we control
  both ends, so we pin the exact peer.

### 2.4 What the cloud may hold — and the honest residual

Rule, inherited from ADR-0047/ADR-0016 and kept absolute: **no business data
and no authentication material at rest on the VPS.** No database, no disk
spool, no request/response body logging, no WebAuthn credentials, no session
store, no keychain material. The relay's disk contents, stolen whole, are:
its own TLS keys, the pinned agent cert (public), and connection metadata
logs (⚑ §9.5 decides even those).

The honest residual: in Phase 0/1, Leg A's TLS terminates at the VPS, so
invoice payloads **transit the relay's memory in plaintext**. A live,
root-level compromise can read sessions while they happen — that is the
G4 residual. It is bounded (read-only data, no standing access gained, no
replay: WebAuthn challenges are single-use and sessions are tunnel-bound) and
it is **closed** by hardening H1 (§7): an inner end-to-end encryption layer
(browser↔agent, HPKE over the relayed frames) that demotes the relay to a
blind pipe. H1 is deliberately deferred — it requires the portal shell to do
in-browser crypto and complicates the first cut; the ADR records it as the
designed destination, not an afterthought. ⚑ §9.4 asks whether H1 should be
pulled into Phase 1.

### 2.5 Trust boundaries (delta to `docs/threat-model.md`)

Three new boundaries, to be appended to the threat model's numbered list when
this builds:

8. Browser ↔ VPS front — public TLS; everything past it is untrusted input
   until the agent verifies it.
9. VPS relay ↔ Mac agent — mTLS, both peers pinned; the relay is treated as
   honest-but-curious at best, hostile at worst; nothing crossing this
   boundary is trusted by the agent without its own verification.
10. Agent ↔ ABERP backend — loopback + existing bearer; the agent is a
    client of `serve.rs` with no privileged path; ABERP's own auth applies
    unchanged.

---

## 3. Undiscoverability

The requirement is stronger than "no links": an unauthenticated observer who
*suspects* the portal exists must not be able to confirm it.

### 3.1 No references, anywhere public

- The storefront repo (ABERP-site) never mentions the label — no link, no
  CSP/CORS entry, no redirect, no comment. Enforced by a grep in that repo's
  release checklist (`internal.` must not appear).
- No `sitemap.xml` or `robots.txt` entry on either host. The portal host
  serves `robots.txt` as — the uniform 404. (A `robots.txt` that answers 200
  with `Disallow: /` is itself a confession.)
- No directory listing anywhere; the front serves exactly the compiled-in
  shell to authenticated-gate-passers, 404 to everyone else.

### 3.2 No fingerprint

- **Certificate Transparency is the classic leak and is closed first:** the
  cert presented for `internal.abenerp.com` is the **wildcard**
  `*.abenerp.com`. CT logs then ever only show the wildcard — the label
  itself never enters any public log. A dedicated per-name cert would
  publish the hostname to every CT monitor within minutes of issuance;
  that is how "hidden" subdomains are actually found in practice.
- **Uniform 404:** every unauthenticated request — wrong path, right path,
  `HEAD`, `POST`, garbage SNI, direct IP — receives the same minimal 404:
  same status, same headers (a bare, common server line), same body bytes,
  no `Set-Cookie`, no cache-control oddities, no timing cliff (the gate
  check is a constant-time token compare). The identical response is also
  what the VPS's default vhost returns, so name-based probing and IP-based
  probing agree: nothing here.
- **No portal artifact pre-gate:** no favicon, no JS bundle, no manifest, no
  WebAuthn endpoint reachable — the app shell literally is not served, so
  there is no shell to fingerprint.
- **Mac side:** nothing to probe at all — no inbound ports (G1). The frozen
  prod box's scan profile is unchanged from today.

### 3.3 The pre-auth gate ("the knock")

WebAuthn itself must sit *behind* something, because answering a WebAuthn
challenge to strangers confirms the portal exists. Two candidate gates:

- **(a) mTLS client certificates on Leg A.** Strongest — the TLS handshake
  itself refuses strangers. But iOS Safari's client-cert UX (profile
  installation, per-site prompts) is poor, and a `CertificateRequest` in the
  handshake is itself a fingerprint unless sent only for exact-match SNI.
- **(b) High-entropy knock token** — recommended (⚑ §9.3). The bookmark is
  `https://internal.abenerp.com/<128-bit-base32-token>`. A request bearing
  the token (path or later cookie) passes the gate: the shell and the
  WebAuthn ceremony become reachable. Anything else: uniform 404. The token
  is **not** an authenticator — it only decides *whether the door is even
  visible*; WebAuthn remains the lock. Compromise of the token alone
  degrades the attacker from "internet" to "someone who learns the
  subdomain" (§1.2 row 2) — they now face a passkey challenge they cannot
  answer. The token is rotatable at will by the agent (it is minted and
  verified there, like everything else).

Recommendation: **(b)** for Phase 0, with (a) recorded as an available
hardening for desktop-only use. Rationale: (b) is phone-friendly, invisible
in the TLS handshake, and its failure mode is explicitly non-catastrophic.

### 3.4 What undiscoverability does *not* claim

DNS is enumerable. `internal` is on every subdomain wordlist; the A record
will be found by anyone who tries (⚑ §9.2 offers a random label instead).
The design therefore never rests on the name staying secret — it rests on the
found host being **indistinguishable from a parked box** (§3.2) and on the
gate + WebAuthn behind it. Undiscoverability here means *unconfirmable*, not
*unresolvable*.

---

## 4. Authentication — WebAuthn platform passkeys, verified on the Mac

### 4.1 Why this exact shape

Face ID on iPhone and Touch ID on the Mac are the same integration: WebAuthn
with a **platform authenticator**. The browser and OS pick the modality; the
private key sits in the Secure Enclave; the biometric gates its use locally
(the biometric itself never crosses the wire). RP ID = `internal.abenerp.com`
— an assertion is cryptographically bound to that origin, so a pixel-perfect
phishing clone on another host receives assertions that verify against
nothing (G3).

### 4.2 The relying party lives on the Mac

The agent — not the VPS — stores credential public keys, issues challenges,
verifies assertions, and mints sessions. The front merely relays ceremony
messages as opaque frames. Consequences, all load-bearing:

- A relay compromise cannot mint a session, cannot enroll a credential,
  cannot read the credential store (G4).
- Auth state lives on the same machine as the data it protects — one place
  to back up, one place to steal, and that place is the already-defended Mac.
- When the Mac is unreachable, *authentication itself* is unavailable, and
  the front's honest answer is the uniform 404 — the portal simply is not
  there (§5.3), which is precisely the undiscoverability posture.

Credential public keys + metadata live in the agent's own small store on the
Mac (not in an ABERP tenant DB — the agent must work with ABERP stopped, and
ADR-0002's tenant isolation is not for infrastructure state).

### 4.3 Ceremonies

- **Registration (enrolment):** disabled remotely, always. Enrolment runs
  only via a one-time, 10-minute, single-use URL minted **at the Mac's own
  console** (agent CLI prints it as a QR code; scan with the phone →
  Face ID → passkey created). Physical presence at the Mac is the enrolment
  credential. Day-one enrolment registers **two** passkeys: the iPhone and
  the Mac's own Touch ID — two independent authenticators from the start.
- **Authentication:** knock (§3.3) → shell loads → WebAuthn `get()` with an
  agent-issued single-use challenge (nonce, 60 s TTL) → assertion verified
  by the agent (origin, RP ID hash, signature, **and sign-count regression
  check** where the platform provides it) → session minted.
- **User verification required:** `userVerification: required` on every
  ceremony — presence alone (a tap) is not enough; the biometric/passcode
  gate must fire.

### 4.4 Sessions

Agent-minted, short-lived, scoped tokens: 15-minute idle timeout, 8-hour
absolute cap, bound to the front connection that carried the ceremony
(a stolen cookie replayed through a new connection fails), delivered as
`Secure; HttpOnly; SameSite=Strict`, revocable at the agent (single `revoke
--all` CLI). No refresh tokens — a lapsed session is one Face ID glance away
from a new one; long-lived credentials in the browser buy convenience the
threat model cannot pay for.

### 4.5 Recovery — "the Mac is the recovery"

Passkey postures die on recovery design, so it is decided here, not later:

- Lost iPhone → sign in with the Mac's Touch ID passkey (enrolled day one),
  revoke the phone's credential, enrol the replacement phone via §4.3.
- All passkeys lost → **physical access to the Mac** re-runs enrolment
  (§4.3). There is no remote recovery path *by design*: any remote fallback
  (email link, recovery code typed into the portal) would become the
  weakest door on the crown-jewel surface. iCloud Keychain passkey sync is
  additionally in play on the Apple platform (a replacement iPhone restores
  its synced passkey) — treated as a convenience, never as the designed
  path.
- The relay/VPS holds no auth state, so VPS loss requires only re-pointing
  DNS and re-pinning a new relay cert in the agent — no user-facing
  recovery at all.

### 4.6 Stolen-device residual

A thief with the iPhone and the ability to satisfy its biometric/passcode
gate *is* Ervin as far as any authenticator design is concerned. Controls:
sessions expire (§4.4), the credential is revocable from the Mac in one
command, and Phase 1's blast radius is read-only invoice data (§6). This is
the accepted residual of biometric convenience and is stated rather than
hidden.

### 4.7 ⚑ Recommended posture (open decision §9.1)

**Passwordless, passkey-only.** No password exists on this surface — primary
*and* only. The alternative (password + passkey-2FA, or passkey-primary with
password fallback) re-introduces a phishable, keyloggable, reusable secret
onto exactly the surface built to have none, and every "fallback" is an
attacker's preferred entrance. The recovery story (§4.5) is what makes
passkey-only viable; it is designed to that standard.

---

## 5. "Running or not running" — the health surface

### 5.1 What the agent observes

The agent computes health locally, needing nothing from the cloud:

| Signal | Source | ABERP needed? |
|---|---|---|
| ABERP process up | `GET http://127.0.0.1:<port>/health` (`serve.rs:4271` — deliberately the one unauthenticated route) with a short timeout; process-table check as the tiebreaker between "down" and "hung" | no |
| DB reachable / tenant open | the `/health` payload (backend answering implies its DB handle is live) | yes, for the positive claim |
| Last-known-good | agent-kept timestamp of the last healthy probe | no |
| Agent itself up | implicit — the tunnel being answerable *is* the agent's liveness | no |

Poll cadence ~10 s, cached; a browser session never triggers a probe storm.

### 5.2 What the portal renders

Post-auth, always: a status card — **ABERP: up / down**, since-when,
last-known-good, agent uptime. When up, the invoice navigation (Phase 1)
is offered. When down, it is not rendered *and* the agent refuses proxy
requests server-side (the UI hiding a button is never the enforcement —
`[[trust-code-not-operator]]` applied to browsers).

### 5.3 The layered-liveness table

| State | What Ervin sees | What a probe sees |
|---|---|---|
| Mac up, ABERP up | full portal | uniform 404 |
| Mac up, ABERP down | portal + "ABERP down since …" — the raison d'être of the separate agent | uniform 404 |
| Mac down / tunnel down | nothing — front answers 404 even to a knocked, enrolled user (⚑ §9.5 may soften this to a minimal "unreachable" page post-knock) | uniform 404 |
| VPS down | nothing loads | nothing loads — same as any dead host |

---

## 6. Phase 1 — the read-only invoice path

### 6.1 Data path

`browser → front (Leg A) → relay frame (Leg B) → agent allowlist check →
loopback GET with keychain bearer (Leg C) → serve.rs handler → back the same
way.` No cloud persistence at any hop (§2.4); the response is rendered by the
portal shell and exists otherwise only in transit.

### 6.2 The proxied surfaces — existing routes only, none invented

Phase 1 proxies exactly these **existing** `serve.rs` read handlers:

| Route (verbatim) | Handler | Anchor | Portal use |
|---|---|---|---|
| `GET /health` | `handle_health` | `serve.rs:4271` | §5 probe (agent-internal, not exposed as a page) |
| `GET /invoices` | `handle_list_invoices` | `serve.rs:4280` | the invoice list page (ADR-0058's virtual union list) |
| `GET /invoices/:id` | `handle_get_invoice` | `serve.rs:4282` | the invoice detail page |
| `GET /invoices/:id/pdf` | `handle_get_invoice_pdf` | `serve.rs:4283` | tap-through to the printed PDF |

Candidates deliberately **excluded** from Phase 1 (they exist and would slot
into the same allowlist later, each needing its own justification):
`GET /api/incoming-invoices` (`serve.rs:4753`), `GET /api/restored-invoices`
(`serve.rs:4824`), the reports family. Phase 1 stays minimal: the outgoing
invoice list, one invoice, its PDF.

### 6.3 Read-only, enforced at the agent

The allowlist is compiled into the agent as **(method, exact route shape)**
pairs — the four rows above, `GET` only. Everything else is refused *at the
agent, on the Mac, inside the trust boundary*: any non-`GET` verb, any
unlisted path, any query-string smuggling of a different route. Defense in
depth behind it: `serve.rs` still demands its bearer and still runs its own
handler logic — the agent's allowlist is an *additional* gate in front of an
already-authenticated API, not a replacement for it.

### 6.4 The bearer-scope liability, named

The keychain `session_token` the agent attaches (§2.2) is today an
all-routes bearer — `serve.rs` has one token for everything (`serve.rs:19`).
The agent's allowlist confines what can be *asked through the portal*, but
an attacker who fully owns the **agent process** holds a full-capability
token. Accepted for Phase 1 (agent compromise on the Mac ≈ Mac compromise,
which already forfeits the box), with the clean fix queued as hardening H2
(§7): a second, read-only-scoped bearer minted by `serve.rs`, so the agent
never holds write capability at all.

### 6.5 Audit

Every proxied request and every auth event (knock accepted, ceremony
started/verified/failed, session minted/expired/revoked, allowlist refusal)
is appended by the agent to a local, append-only log on the Mac, following
ADR-0088's unattended-identity pattern for how a non-ABERP daemon writes
attributable records. Wiring these into the ABERP audit ledger proper as
`portal.*` EventKinds is a build-time decision for the D-17 implementation —
the design constraint fixed here is only: **on the Mac, append-only, no
bodies logged, and refusals are logged as loudly as successes.**

---

## 7. Phased roadmap, failure modes, liabilities

**Phase 0 — tunnel + gate + health.** Agent daemon (launchd), relay+front on
the VPS, wildcard cert, uniform-404 posture, knock, WebAuthn enrolment +
auth, status card. *No ABERP data crosses the wire at all in Phase 0.*
- Failure modes: tunnel flap (jittered reconnect; meanwhile the portal is
  invisibly down — G1-consistent); launchd misconfig leaving the agent dead
  (detectable only by Ervin noticing the portal 404s — accepted, the
  Phase-0 health card cannot report its own absence); knock token leaked
  via bookmark sync to a shared browser (rotate at the agent; residual is
  §1.2 row 2).
- Liabilities: one more always-on daemon on the frozen prod Mac (small,
  separate, no inbound — but real); a VPS to patch and pay for.

**Phase 1 — read-only invoices.** The §6 allowlist, list/detail/PDF pages.
- Failure modes: ABERP down mid-session (agent returns a typed "backend
  down" the shell renders — never a hung proxy); large PDF over a phone
  link (stream, don't buffer whole on the VPS — the no-at-rest rule also
  forbids spooling); serve.rs route drift breaking the allowlist (the
  allowlist is exact — drift fails *closed*, portal shows an error, nothing
  silently widens).
- Liabilities: §2.4's plaintext-in-relay-memory residual now covers real
  invoice data (until H1); §6.4's bearer scope.

**Hardenings queued, in order:**
- **H1 — end-to-end inner encryption** (browser↔agent HPKE over the relayed
  frames): demotes the VPS to a blind pipe; closes §2.4. ⚑ §9.4.
- **H2 — read-only scoped bearer** in `serve.rs`: closes §6.4.
- **H3 — mTLS on Leg A** for desktop sessions (§3.3a) on top of the knock.
- **Growth seams, explicitly not designed here:** more read surfaces
  (incoming invoices, reports, workshop dashboard), a second enrolled
  operator, a second Mac behind the same relay (the pinning model §2.3
  already leaves room for an allowlist), and — far later, behind its own
  ADR — any mutating action, which would demand step-up re-auth per action
  and a rethink of §6.3's whole posture. **Nothing in Phase 0/1 pre-commits
  to writes.**

---

## 8. Fit with existing infrastructure — additive only

- **The storefront is untouched.** `abenerp.com` (SSR, CloudFront→Lightsail,
  repo ABERP-site) gains no config, no link, no header, no shared cert
  handling beyond the wildcard it already could use. The one obligation is
  *negative*: the release-checklist grep (§3.1).
- **The frozen prod Mac gains zero inbound exposure** (G1). One new outbound
  WSS connection and one new local daemon; every existing posture — keychain
  secrets (ADR-0047), bearer-gated serve.rs, tenant isolation (ADR-0002),
  audit chain (ADR-0087/0088) — is consumed as-is, never modified.
- **The no-cloud-secrets line holds.** The SMTP-SPOC/keychain posture's rule
  — secrets and business data live on the Mac, the cloud sees transient
  traffic at most — is inherited unchanged; §2.4 is its restatement for
  this surface, and H1 is its completion.
- **`docs/threat-model.md`** gains boundaries 8–10 (§2.5) and a
  "portal/relay/agent" asset row when D-17 builds — flagged here so the
  threat model's own cadence rule ("updated at every release") catches it.
- **DNS**: one new A/AAAA record for `internal.abenerp.com` → the VPS.
  Nothing else in the zone changes.

---

## 9. Open decisions for Ervin — ⚑ flagged, defaults recommended

1. **Auth posture** (§4.7). **Recommended: passwordless passkey-only**, no
   password on the surface at all; recovery = the Mac (§4.5). Alternative:
   password + passkey second factor. Decide before Phase 0 enrolment code.
2. **The label itself** (§3.4). `internal.abenerp.com` is wordlist-guessable;
   a random label (`<12-random-chars>.abenerp.com`) would not even resolve
   for an enumerator. **Recommended: keep `internal` for memorability** —
   the design already assumes the name is found (§3.2 does the real work) —
   but this is a free hardening if Ervin will tolerate a bookmark-only name.
3. **Pre-auth gate mechanism** (§3.3). **Recommended: high-entropy knock
   token (b)**, mTLS (a) as later desktop hardening H3.
4. **H1 timing** (§2.4, §7). **Recommended: Phase 2**, immediately after the
   first working invoice page — accepting the §2.4 residual for the interim.
   Pull into Phase 1 if the residual is unacceptable even briefly.
5. **Relay observability** (§2.4, §5.3). How much may the VPS log
   (connection timestamps/IPs only vs nothing), and does a knocked, enrolled
   user get a minimal "Mac unreachable" page instead of the 404 when the
   tunnel is down? **Recommended: metadata-only logs with short rotation;
   keep the pure 404** (unreachable = invisible, no exceptions).
6. **VPS placement.** **Recommended: a separate minimal VPS** (smallest
   instance anywhere reputable; the relay is tiny), *not* the Lightsail
   storefront box — blending the crown-jewel door into the public web host
   couples blast radii for the cost of one small instance. Alternative:
   reuse Lightsail and accept the coupling.

## 10. Prerequisites Ervin must provide before D-17 can start

1. A relay host: the VPS (or the Lightsail decision, §9.6) with a public IP.
2. DNS: the `internal.abenerp.com` A/AAAA record (or §9.2's random label).
3. TLS: issuance of the **wildcard** `*.abenerp.com` certificate for the
   front (§3.2) — specifically *not* a per-name cert.
4. §9 decisions 1–3 (posture, label, gate); 4–6 can trail into the build.
5. Ten minutes at the Mac for first enrolment (§4.3) when Phase 0 lands.

---

## Consequences

**Easier:** invoice state visible from anywhere on a Face ID glance; ABERP
process health visible remotely *especially when it is down*; a hardened,
already-authenticated seam through which every future remote capability can
grow without ever opening a port on the Mac; the relay/agent split means
cloud components stay commodity and disposable.

**Harder:** three new deployables (agent, relay+front, portal shell) to
build, sign, and keep patched; a VPS to operate; enrolment and recovery are
physical-presence affairs by design — no "forgot password" convenience,
ever; the uniform-404 posture makes remote debugging of the portal itself
deliberately awkward (you cannot probe your own front without the knock).

**Locked in:** outbound-only from the Mac (any future feature needing inbound
is out — that is the point); WebAuthn RP ID bound to the chosen hostname
(renaming the surface later orphans enrolled passkeys — enrolment is cheap,
but it is a real re-enrolment); verification-on-the-Mac (a future
multi-portal cloud with its own IdP is a different architecture and a
superseding ADR); read-only-at-the-agent as the Phase-1 invariant every
later write feature must explicitly argue its way past.

## Adversarial review

None yet — **Proposed, design-only**. First review should attack, in order:
the §3.2 indistinguishability claim (byte-diff the gate's 404 against the
default vhost under every method/SNI/timing probe); the §4.3 enrolment path
(can a one-time URL leak survive its 10-minute window?); the §2.4 residual
(is "honest-but-curious VPS" the right ceiling before H1?); and §6.3's
allowlist (route-shape matching vs axum's actual path normalization —
smuggling via encoding is the classic hole).
