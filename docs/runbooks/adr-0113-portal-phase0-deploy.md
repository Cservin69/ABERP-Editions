# ADR-0113 Phase 0 — portal deploy runbook

**Status: built, not deployed.** The three components exist and pass
their gates on loopback. Nothing has been provisioned: there is no VPS,
no DNS record, no wildcard certificate, and no launchd daemon. This
runbook is the list of things that must be true before the portal is
reachable, in the order they have to happen.

The design is ADR-0113 (branch `docs/adr-0113-internal-portal`). Read
it first; this file assumes it.

---

## 0. What was built

| Component | Crate | Runs on |
|---|---|---|
| Outbound agent + WebAuthn relying party | `crates/aberp-portal-agent` | the invoicing Mac |
| Relay + front (blind pipe, uniform 404) | `crates/aberp-portal-relay` | the VPS |
| Shared wire types + mutual pinning | `crates/aberp-portal-core` | both |
| Portal shell (one HTML file) | `crates/aberp-portal-relay/assets/shell.html` | served by the front |
| Canary trap (probe detection) | `…-relay/src/canary.rs` | the VPS |
| Canary trap (probe log + alert) | `…-agent/src/{canary,alert}.rs` | the Mac |

The Mac opens **no inbound port**. There is no `TcpListener` anywhere
in `aberp-portal-agent`; the only socket it creates is the outbound dial
to the relay.

---

## 1. Prerequisites Ervin must provide

These are the blockers. Nothing below §2 can start without them.

1. **A relay VPS** — smallest instance at a reputable host, with a
   public IPv4 (and IPv6 if available). Per ADR-0113 §9.6 this is a
   *separate* box, not the Lightsail storefront host: blending the
   crown-jewel door into the public web host couples blast radii for the
   price of one small instance.
2. **The hostname.** A random-but-memorable multi-word label under
   `abenerp.com`, chosen at deploy time. **It must not be written into
   this repository** — `crates/aberp-portal-agent/tests/no_committed_hostname.rs`
   fails CI if it is. It reaches the agent through `PORTAL_HOST` and
   reaches nothing else at all.
3. **A DNS A/AAAA record** for that label, pointing at the VPS. Nothing
   else in the zone changes.
4. **A wildcard `*.abenerp.com` certificate** — specifically *not* a
   per-name certificate. This is the control that keeps the label out of
   every Certificate Transparency log; a per-name cert publishes the
   hostname to every CT monitor within minutes of issuance, which is how
   "hidden" subdomains are actually found.
5. **Ten minutes at the Mac** for the first enrolment (§4.3), enrolling
   **two** passkeys: the iPhone and the Mac's own Touch ID. The second
   one is the recovery path — there is no other.

---

## 2. Certificates and pins

Three independent identities. None of them is a public CA problem
except the front's.

| Identity | Who holds it | Who pins it |
|---|---|---|
| Front (Leg A) | relay | browsers, via the public WebPKI — the wildcard |
| Relay (Leg B) | relay | the agent, by leaf SHA-256 |
| Agent (Leg B) | Mac | the relay, by leaf SHA-256 |

Leg B never consults the public WebPKI. Generate its two identities as
long-lived self-signed certificates and exchange their fingerprints:

```sh
# The SHA-256 each side pins is over the leaf DER.
openssl x509 -in relay-leg-b.pem -outform DER | shasum -a 256
openssl x509 -in agent.pem       -outform DER | shasum -a 256
```

The agent's private key belongs in the macOS keychain
(`PORTAL_AGENT_KEY_KEYCHAIN_SERVICE`, account `mtls_client_key`);
`PORTAL_AGENT_KEY_PEM` is the dev/test escape hatch and should not be
set in production.

**Rotation**: replacing either Leg-B identity means updating one
fingerprint on the other side and restarting. Replacing the front
certificate is an ordinary wildcard renewal. Replacing the *relay VPS
entirely* needs only a DNS change and a new pin in the agent's config —
no user-facing recovery at all, because the relay holds no auth state.

---

## 3. The relay

```sh
aberp-portal-relay \
  --front-addr        0.0.0.0:443 \
  --front-cert-pem    /etc/portal/wildcard-fullchain.pem \
  --front-key-pem     /etc/portal/wildcard-key.pem \
  --agent-addr        0.0.0.0:8443 \
  --agent-leg-cert-pem /etc/portal/relay-leg-b.pem \
  --agent-leg-key-pem  /etc/portal/relay-leg-b-key.pem \
  --pin-agent         <64-hex SHA-256 of the agent leaf>
```

Notes that matter:

- `--pin-agent` is required and repeatable. An unpinned Leg B is refused
  at startup, not warned about.
- `--front-plaintext` exists for the loopback end-to-end test and is
  **refused** on any non-loopback address.
- The relay has no hostname argument. It answers whatever `Host`
  arrives; the wildcard covers the label; the RP ID lives on the Mac.
- Logging is metadata-only by construction (Ervin's §9.5 decision):
  connection timestamps and peer addresses, never paths, tokens or
  bodies. Set a short journald rotation to match.

Firewall: 443 and the agent port inbound; nothing else. The agent port
answers only a pinned client certificate, and drops everything else
inside the TLS handshake — before any application byte.

---

## 4. The agent (launchd, on the Mac)

Environment:

| Variable | Meaning |
|---|---|
| `PORTAL_HOST` | the portal hostname — **the deploy-time secret** |
| `PORTAL_RELAY_ADDR` | `<relay host>:8443` |
| `PORTAL_RELAY_SERVER_NAME` | TLS name on Leg B (defaults to the host part) |
| `PORTAL_RELAY_CERT_SHA256` | the relay's pinned leaf fingerprint |
| `PORTAL_AGENT_CERT_PEM` | the agent's own certificate |
| `PORTAL_AGENT_KEY_KEYCHAIN_SERVICE` | keychain service holding its key |
| `ABERP_PORTAL_STATE_DIR` | defaults to `~/.aberp-defense/portal-agent/` |
| `ABERP_TENANT` | which tenant's `runtime.json` and keychain bearer to use |
| `PORTAL_TRIPWIRE_PATH` | the canary's decoy path (defaults to a compiled-in value) |
| `PORTAL_ALERT_SINK` | `smtp` (default), `file:<path>`, or `off` |
| `PORTAL_ALERT_TO` | canary alert recipient (defaults to the SPOC's own `from_address`) |

Install as a launchd **daemon** with `KeepAlive` so it starts at boot
and survives ABERP being stopped, crashed or upgraded. That separation
is the whole point: the agent's liveness is the portal's liveness, and
ABERP's liveness is merely a status it reports.

First run:

```sh
aberp-portal-agent knock        # the bookmark URL — treat it as the bookmark
aberp-portal-agent enrol --label iPhone   # 10-minute, single-use URL
aberp-portal-agent enrol --label Mac      # do the second one the same day
aberp-portal-agent credentials  # confirm two passkeys
```

Enrolment is **only** available from this console. There is no remote
enrolment endpoint and no password anywhere in the system, so the
recovery story is physical access to the Mac — which is why the second
passkey on day one is not optional.

Day-to-day:

```sh
aberp-portal-agent rotate-knock       # invalidates the old bookmark
aberp-portal-agent revoke --id <id>   # lost phone
aberp-portal-agent revoke --all       # panic button (also drops sessions)
```

The audit log is `<state dir>/audit.log`, append-only JSONL, metadata
only — every proxied request and every auth event, refusals as loudly as
successes.

---

## 4a. The canary trap

The host has no legitimate unauthenticated traffic — it is never
linked, never crawled, never referenced. So **every request that fails
the knock is a probe**, and each one trips a silent canary.

The prober sees nothing. The response is the byte-identical uniform
404, produced by the same code path in the same shape of time, whether
they brushed the host or hit the decoy. That is not a nicety: a trap
that could be detected would be exactly the fingerprint §3.2 forbids,
and the prober would learn more from finding it than you learn from it
firing.

**Where the alert comes from.** The probe is seen on the VPS; the mail
is sent from the **Mac**. §2.4 forbids authentication material at rest
on the relay and ADR-0047 puts the SMTP password in the keychain, so a
relay that could send mail would need a credential a relay compromise
would hand over. Batches ride the tunnel that already exists, in the
direction it already runs.

**Severity.**

| Class | What it means | Cadence |
|---|---|---|
| `suppressed` | the source passed the knock minutes ago — your own browser fetching `/favicon.ico` off the bare host | logged, never mailed |
| `low` | background noise: reached the IP, did not name the host, asked for nothing meaningful | at most hourly, a digest |
| `high` | the decoy was hit, the hostname was used, a knock-shaped token was guessed, or an API-shaped path was requested | at most every 5 minutes |

`high` is the one that matters: it means somebody knows something they
should not. If it was not you, rotate the knock token
(`aberp-portal-agent rotate-knock`). Passkeys are unaffected — no
authentication can succeed without your Face ID or Touch ID.

**The decoy.** One path, referenced by nothing: not the shell, not a
redirect, not a `robots.txt` (there is none). Any hit is unambiguous.
It defaults to a compiled-in value and is overridden with
`PORTAL_TRIPWIRE_PATH`, which the agent publishes to the relay in the
tunnel handshake — so rotating it needs no relay redeploy and leaves no
value in the repository.

**Coalescing.** Two ceilings, protecting different things. The front
batches probes into 30-second windows with a 60-second floor between
`high` batches (protects the tunnel); the agent rate-limits alerts and
folds the held-back counts into the next one (protects your attention).
A `/16` sweep is a handful of mails saying "1,400 probes from 62
sources", not 1,400 mails.

**Storage.** The relay keeps nothing on disk — a bounded in-memory
window and metadata-only journald lines, per Ervin's §9.5 decision. The
Mac keeps `<state dir>/canary.log`, append-only JSONL, metadata only,
rotated at 1 MiB with one generation retained.

**Testing it after deploy.** Request the decoy path from a phone on
mobile data (not your home IP, which the grace window will suppress).
You should get the ordinary 404 and an alert within a minute.

---

## 5. Verifying the deployment

1. **From anywhere, without the knock**: every path and every method
   must return an identical 404. Byte-diff two of them.
2. **Certificate Transparency**: search the CT logs for the label. It
   must not appear — only the wildcard.
3. **From the Mac**: `nmap` it from another machine. The scan profile
   must be unchanged from before the portal existed.
4. **Stop `aberp serve`**: the portal must still load and must say
   "ABERP: down", and the invoice reads must be refused with 503.
   *This is the feature*, not a degraded mode.
5. **Stop the agent**: the whole host must go back to answering the
   uniform 404, including from Ervin's own bookmark.

---

## 6. What is deliberately still open

- **Invoice payloads transit relay memory in plaintext.** Leg A's TLS
  terminates at the VPS, so a live root-level compromise of the relay
  can read a session while it is happening. It cannot mint a session,
  enrol a passkey, widen the allowlist, or recover anything afterwards.
  Closing this is hardening **H1** — browser↔agent HPKE — which Ervin
  scheduled for Phase 2.
- **The knock token, not client certificates**, gates the browser leg.
  mTLS on Leg A (**H3**) remains available as a desktop-only hardening.
- **The upstream bearer is all-routes.** The agent's allowlist confines
  what can be *asked*; hardening **H2** (a read-only-scoped bearer minted
  by `serve.rs`) is what would confine what can be *held*.
- **Responses are buffered, not streamed**, through the relay, capped at
  8 MiB. Bounded and transient — nothing is spooled to disk — but ADR-0113
  §7 wants streaming for large PDFs.
- **No QR rendering** of the enrolment URL yet; it prints as text.
- **The canary records no TLS SNI and no client fingerprint.**
  `ProbeSample::sni` is always `None`: `axum-server`'s rustls acceptor
  does not surface the handshake's SNI or a JA3-style fingerprint to the
  handler, and recovering them means running a custom acceptor and
  threading per-connection state into the request extensions — real
  regression surface on the one listener that must never behave
  distinguishably. The `Host` header covers most of the signal, with the
  caveat that it is client-controlled where SNI is observed. **Phase 2.**
- **The SMTP SPOC is single in configuration and policy, not in code.**
  The agent reads the same `[seller.smtp]` section, the same
  `aberp.smtp.<tenant>` keychain entry and the same closed-vocab
  `security` field, and builds the transport with the same TLS-mandatory
  posture — but it is a second call site, because the agent cannot link
  `apps/aberp` without dragging DuckDB, NAV and Tauri into a daemon that
  must run when they are stopped. Unifying it means extracting
  `smtp_config.rs` and the transport builder into a shared crate, which
  edits the frozen invoice application; that was not taken unilaterally
  inside a portal build. Until it is, both sites carry the same
  source-scanning pin against a plaintext fallback. **Should-fix.**
- **The grace window is IP-based.** A source that passed the knock is
  treated as the operator for five minutes, so an attacker sharing your
  egress IP inside that window is suppressed too. The alternative —
  alerting at HIGH on every legitimate portal visit, because browsers
  fetch `/favicon.ico` off the bare host — trains you to ignore the
  alert, which is worse. **Accepted.**
- **A hostile relay can suppress canaries.** It can drop any frame,
  including these. Inherent to the relay being untrusted, and not
  something a mailer on the VPS would fix — that would only add a
  credential for the same attacker to steal.
- **The relay's backlog is bounded and volatile.** While the tunnel is
  down, batches wait in memory (32 of them) and flush on reconnect; a
  relay restart loses them. Correct for a box that must hold nothing,
  but it means a scan during an outage may be seen only in part.
