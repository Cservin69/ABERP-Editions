# ADR-0115 — Outbound-only remote portal: poll transport, parked-nginx disguise, Phase-0 enrolment defence

- **Status:** Proposed
- **Date:** 2026-08-21
- **Deciders:** Ervin
- **Edition scope:** Editions/Defense only. The frozen Prod invoice tree and
  the Portable edition are untouched; `cargo tree -p aberp` contains no
  portal crate.

## Context

Ervin needs to read invoice data from a phone, away from the office, from a
Mac that runs the Defense edition. Nothing about that requirement is
unusual. Everything about the constraints is:

1. **The Mac must gain no inbound port.** It is the machine the business
   runs on. A listening socket on it is a new attack surface on the one host
   that cannot afford one.
2. **The host must not be discoverable.** Not "hard to guess" — *not
   there*. A scanner that sweeps the VPS's address range, or a bot that
   walks a wordlist against the domain, must come away with nothing that
   distinguishes this host from an empty one.
3. **A compromised relay must not become a compromised business.** The VPS
   is the exposed half. It is going to be the thing that gets rooted, and
   the design has to still hold when it is.
4. **Recovery is the Mac.** No email reset, no recovery code, no support
   path. Whatever the design is, the way back in is physical presence at the
   machine — because every remote recovery path is a door, and a door is
   what this is trying not to have.

This ADR is round 2. Round 1 (drafted against ADR-0113's number, which is
now earmarked for CAD B0 — see *Numbering* below) established the three
deployables and the security posture. Adversarial review of round 1 found
three things that needed deciding rather than patching, and this ADR is
where they are decided: the transport, the disguise, and the enrolment
defence.

### Numbering

This work was drafted as ADR-0113 and is filed as **0115**. `0113` is
earmarked for CAD B0 and `0114` for CAM Part D. Every in-tree reference has
been renumbered; nothing was published under 0113.

## Decision

### 0. The three deployables

Unchanged from round 1, restated because everything below refers to them:

| | Runs on | Is |
|---|---|---|
| **agent** | the Mac | the WebAuthn relying party, the credential store, the proxy to the local `aberp serve`, the alert sender |
| **relay + front** | a VPS | a blind parking lot with nothing at rest, and the public HTTPS surface |
| **shell** | the browser | one compiled-in HTML artifact, served only past the knock |

The load-bearing property is the asymmetry: **the relay decides nothing**.
It holds no credential, mints no session, verifies no signature, and has no
opinion about what is readable. `crates/aberp-portal-relay/Cargo.toml` is
the proof — there is no `p256`, no `x509-cert`, no keychain, no storage
crate, and no dependency on `aberp-portal-agent`.

### 1. Transport: the Mac polls. There is no tunnel. (§2.1)

**Decision — operator-mandated.** *"No existing tunnels, just a Mac
querying."* Leg B is a poll, not a held-open connection:

1. a browser request passes the knock at the front, and the relay **parks**
   it in a bounded in-memory queue;
2. the Mac long-polls `POST /agent/v3/poll` outbound over the
   mutually-pinned TLS and pulls the parked request;
3. the Mac runs the read locally and **posts** the answer to
   `POST /agent/v3/deliver`, again outbound.

Every leg is Mac-initiated. The relay never initiates anything, and there is
no third endpoint — if this surface ever grows a way for the relay to *ask*
the Mac for something, the "outbound only" claim stops being checkable by
reading one file.

**"Mac down → the host is simply not there", without a socket to close.**
The tunnel model got §5.3 for free: the socket closed, the knock token left
with it, and the host collapsed to the parked answer. A poll model has no
socket to close, so the property comes from a **presence lease** the Mac
renews by polling (`PRESENCE_TTL`, 75 s, deliberately more than two poll
cycles so an ordinary gap does not blink the portal out).

This is **strictly better** than the socket close, and that is worth being
explicit about because it looks like a downgrade. A socket close detects a
Mac that has *gone*. It does not detect a Mac that is **wedged** — powered
on, TCP established, answering nothing. Under the tunnel model that Mac kept
the portal advertised and every request hanging until timeout. Under the
lease it lapses like any other absence.

**Sessions are bound to a presence epoch.** Round 1 bound them to the tunnel
id, and a tunnel drop revoked them. With nothing held open there is no drop
to observe, so the agent mints an **epoch** and the relay echoes whether it
knew it. When the relay reports no live presence for an epoch it had already
acknowledged — it restarted, or the lease lapsed — the agent rotates and
revokes. That preserves §2.4's guarantee exactly: *a cookie that transited
relay memory dies no later than the relay's own memory of the Mac.*

**Canary batches ride poll responses, at-least-once.** The relay cannot
push, so probe reports are queued and carried down on the next poll,
**held until acknowledged** by a later poll's `ack_canary_seq`. A poll
response lost to a dropped connection costs one redelivery, not lost
evidence. The agent discards a sequence it has already recorded, which is
what makes at-least-once safe to alert on.

**Every poll response carries a heartbeat, and that is the silence
detector.** The canary's weakest link is silence: a relay that has crashed,
been firewalled, or been taken over and told to drop canary batches produces
exactly the same observable as a quiet internet — nothing. A monotonic
sequence on every answer turns that into a *detectable* event at the Mac,
which is the side that owns the alert path. The design does not depend on
the heartbeat's contents being true — a hostile relay can lie about every
counter in it. It depends only on it **arriving**.

### 2. Disguise: be a parked nginx, byte for byte, per request class (§3.2)

Round 1 said "every unauthenticated request receives the same minimal 404".
Adversarial review surfaced a genuine conflict between two readings of that,
and this ADR picks one.

**Decision: mimic nginx fully. Do NOT return a uniform 404 to protocol
errors, because real nginx does not.**

The uniform-404 reading is simple and provably free of a path oracle. But it
is not actually uniform where it counts. It is uniform *across paths* —
which is the property the rule was written for — and wildly non-uniform
*across request classes*. A host that answers `404` to a malformed request
line, to `HTTP/9.9`, and to a TLS `ClientHello` sent at a cleartext port is
a host that is **not running nginx**, and says so to anyone who spends one
deliberately broken request. Scanners send those first.

So the rule is restated per class:

> Within a request class the answer is fixed and **path-independent**.
> Across classes the answer is **whatever nginx does**.

The anti-oracle property is untouched: nothing in the mimic reads the
target, and the class is chosen entirely from the shape of the request.

**We own the connection below the web framework.** This is the part that
made the decision real rather than aspirational. Running the front on
`axum`/`hyper` meant hyper answered anything that failed to parse — its own
`400`, its own header set, its own header **order**, no `Server:` line —
before a single line of our code ran. There is no hook for it; hyper's
connection-level error responses are not a `Service`. So
`crates/aberp-portal-relay/src/http1.rs` is a small, complete HTTP/1.1
server that parses, bounds, and writes every response byte itself. The
relay's dependency list got *shorter* as a result: `axum`, `axum-server` and
`tower` are gone.

**The bytes come from a real nginx, not from the RFCs.** Captured from
nginx 1.31.4 with `server_tokens off` into
`crates/aberp-portal-relay/tests/fixtures/nginx-goldens.txt`. Two findings
contradict what one would write from first principles, and both were live
defects in round 1:

- the 404 body is **146** bytes, not 150. Round 1's hand-written body omitted
  nginx's `<hr><center>nginx</center>` line and was 4 bytes short — a
  fingerprint on its own;
- `Connection` **echoes the client's intent**. nginx keeps an HTTP/1.1
  connection alive through a 404 and through a 405. Round 1 closed every
  un-knocked connection, which is distinguishable by opening one socket and
  sending two requests.

Pinned classes, all verified byte-for-byte:

| Class | Status | `Content-Length` | `Connection` |
|---|---|---|---|
| unknown path | `404 Not Found` | **146** | echoes intent |
| bad request line / missing or duplicate `Host` on 1.1 / bad header name / space in target / `OPTIONS *` / TLS at a cleartext port | `400 Bad Request` | **150** | close |
| method outside `GET`/`HEAD`/`POST` | `405 Not Allowed` | **150** | echoes intent |
| request line past 8 KiB | `414 Request-URI Too Large` | **170** | close |
| any version but 1.0/1.1, incl. cleartext `HTTP/2.0` | `505 HTTP Version Not Supported` | **180** | close |

Header order is **Server, Date, Content-Type, Content-Length, Connection**
and is as load-bearing as the values: an identical header set in a different
sequence identifies the server just as well as a version string would. The
bytes are written by hand rather than through a header map, because a map
has no stable order to promise and this one is a promise.

Also pinned, because each is a case a hand-written mimic gets wrong:
HTTP/1.0 without `Host` is **404, not 400** (`Host` is mandatory only on
1.1); `HTTP/0.9` gets the **bare 146-byte body**, no status line, no
headers; a chunked body is **drained** so a kept-alive socket stays aligned;
leading CRLFs and bare-LF line endings are tolerated; absolute-form targets
are 404; an impossible method byte is answered **immediately** rather than
waiting for a terminator, because a TLS `ClientHello` never sends one and a
server that sat silent there would be distinguishable by its silence; and a
socket that opens and closes is answered with **nothing at all**.

**No HSTS, no CSP on parked responses.** Round 1 stamped HSTS "uniformly, on
every answer including the 404". That was exactly backwards: a parked nginx
sends five headers and no more, so a security header made the parked surface
unique in the only way that matters. Every security header — HSTS, CSP,
`Referrer-Policy`, `X-Content-Type-Options`, `X-Frame-Options`,
`Cache-Control` — is now on the authenticated shell and API **and nowhere
else**.

**Nothing escapes the trap.** Every path that ends in a parked response
feeds the canary a silent observation — the ordinary 404, a wrong knock, an
overloaded queue, a Mac that never answered, **and** the protocol-level
refusals that never become a parsed request. The response is byte-identical
either way, so the trap costs a prober nothing observable and misses
nothing.

### 3. Canary grace: the tripwire outranks every suppression (§3.4)

Round 1 suppressed a *recently-authorised source* whatever it asked for, so
the operator's own browser fetching `/favicon.ico` against the bare host
would not page anyone at 02:00. The bug: each knock renewed the window, so a
knocked source hammering the decoy produced tens of thousands of hits and
**zero alerts**. Whoever holds the knock token is exactly the population
worth watching once they start asking for things the portal does not have.

**Decision.** The tripwire is checked **first**, before anything can
suppress it. The recently-authorised exemption is narrowed from a blanket
per-source suppression to a fixed **path allowlist** — `/favicon.ico`,
`/manifest.json`, and a bounded `apple-touch-icon` family match — which is
the entire set of automatic requests a browser makes on its own. Everything
else a knocked source asks for classifies normally.

### 4. Enrolment defence (§4.3a, §4.3b)

Round 1 asked for `attestation: "none"` and reasoned that physical presence
at the Mac *was* the credential: only a human at the console can mint an
enrolment token, so whatever completes the ceremony must be Ervin's device.

**That reasoning has a gap, and the gap is the relay.** Until hardening H1
the enrolment ceremony crosses relay memory in plaintext (§2.4), so a
compromised relay can observe a live, console-minted, not-yet-spent token.
With `attestation: "none"` there was nothing downstream able to tell a
passkey generated in an iPhone's Secure Enclave from one generated by fifty
lines of software on the VPS. Both present a public key and say "trust me".

An attacker who wins that race does not get a session — they get an
**enrolled credential**, which is standing access that survives knock
rotation, relay redeploys, and the compromise being cleaned up. That is the
worst outcome in the threat model, and it was reachable.

**Decision — two independent Phase-0 controls, neither of which depends on
the relay being honest.**

**§4.3a — Require `attestation: "direct"` and verify Apple's anonymous
attestation chain.** Against the **pinned Apple WebAuthn Root CA**, vendored
at `crates/aberp-portal-agent/assets/` with its provenance and SHA-256 in a
README beside it, and re-asserted by a test so a substituted anchor fails
the build. Verified, in order: format is `apple` with an `x5c`; every
certificate is inside its validity window *including the anchor*; each is
signed by the next and the last by the **pinned root, never by the top of
the supplied chain**; the leaf carries Apple's nonce extension and the nonce
equals `SHA-256(authData ‖ clientDataHash)`; and the leaf's public key **is
this credential's key**. The last two are both needed — the nonce stops
replay of a chain captured from another ceremony, the key binding stops a
valid chain being pasted next to an attacker's key.

The objection that motivated `none` — "an identifying certificate on the
wire" — does not apply to what Apple actually sends. Its platform
attestation is *anonymous*: a per-credential certificate with no serial, no
account, no stable identifier. There is nothing to correlate.

**§4.3b — Console confirmation before anything is committed.** A ceremony
that passes every cryptographic check is **staged**, not stored, and no
session is minted. A short code is shown on the enrolling device, printed at
the daemon's console, and mailed; the credential is committed only when
someone at the Mac runs `aberp-portal-agent confirm --code <code>`.

The two controls answer different questions, which is why both are here.
Attestation proves the key lives in a Secure Enclave — it does **not** prove
it is *Ervin's* Secure Enclave, and an attacker with a stolen token and any
iPhone satisfies it. Console confirmation asks the one question no remote
attacker can answer: *is a human standing at the Mac who meant to do this?*

**Every enrolment attempt alerts, and the alert is deliberately not
rate-limited.** Enrolment is the only operation in the design that grants
standing access. A legitimate one produces a mail Ervin was expecting; an
illegitimate one produces a mail nobody was expecting, next to a console
prompt nobody typed.

**Corrected claim.** Three places in round 1 said a relay compromise "cannot
enrol a credential". That was **overstated**. What was true: the relay
cannot *mint* an enrolment token. What did not follow: that it could not
*use* one. The honest statement is now: *a relay compromise cannot enrol a
credential, and the reason is §4.3a and §4.3b — not the token being secret
from it.* All three sites are corrected in place, naming the correction
rather than quietly editing it.

### 5. `enrolment_open` is no longer published (§4.2)

`GET /api/session` is reachable by anyone holding the knock and nothing
else. Publishing `enrolment_open` meant an attacker with a stolen knock
could poll a few times a minute and learn the exact 10-minute window in
which a registration ceremony is accepted at all — turning a window nobody
can see into a scheduled opportunity. It cost nothing to publish, because
the shell enters enrolment from a URL fragment the console printed.

### 6. The nine remaining hardening items

| # | Fix |
|---|---|
| 1 | A failed alert send no longer clears the rate-limit stamp. Clearing it meant a broken SMTP path turned a scan into an unbounded retry loop against an already-unhealthy network. The counts are deferred; nothing is lost. |
| 2 | Pending canary batches are retried on **every** aggregator tick, not only on ticks that also produce a new batch. Previously, a scan that stopped the moment the Mac went down left its own evidence stranded indefinitely — the quiet case is the one worth getting right. |
| 4 | The per-window source set is bounded (`MAX_SOURCES`, 1024) and `distinct_sources` saturates. It was fed one entry per probe from the open internet: a memory-growth primitive costing one spoofable packet per entry. Precision is lost exactly where it has no value — "1024 sources" and "90,000 sources" both read as *a distributed sweep*, and `total` still carries the true volume. |
| 5 | `RESERVED_INVOICE_SEGMENTS` is compared case-insensitively. A case-sensitive check quietly reintroduced the "the other end would have refused it" reliance that §6.3 exists to avoid. |
| 6 | CSP (`frame-ancestors 'none'`), `Referrer-Policy: no-referrer`, `X-Content-Type-Options`, `X-Frame-Options` and HSTS on the shell and API **only**. `no-referrer` is load-bearing: the knock token is in the path, so without it any outbound link hands the gate to a third party. |
| 8 | The session cookie is scoped `Path=/<knock>/`, stamped at the relay because it is the only party that knows the prefix (the knock is stripped before Leg B). At `Path=/` the browser offered the session to every un-knocked path on the host. |
| 9 | The challenge table gains a **per-source** cap, and a full table **evicts the heaviest source** rather than refusing the caller. The global-only cap was a denial-of-service against the operator: anyone with the knock could fill it in a fraction of a second, and the next caller refused would be Ervin. |
| 10 | Every relay-supplied string is re-sanitised **on the Mac** before it can reach an audit line. `peer` in particular is attacker-influenced metadata a hostile relay could decorate with newlines to forge log entries. |

## Consequences

**Easier.** The relay is smaller and holds less: no framework, no frame
codec, no held socket, three fewer dependencies. "Mac down or wedged → the
host is not there" is now one TTL rather than an emergent property of socket
lifetimes. The disguise is testable — and *tested* — against the real thing.

**Harder.** We own an HTTP/1.1 parser. That is a real, permanent maintenance
obligation and the honest cost of the disguise decision; it is bounded by
being deliberately incomplete (no HTTP/2, no upgrades, no `100-continue`, no
trailers, no compression) and by the differential test.

Latency gains a floor: a request parked just after a poll returns empty
waits for the next one. `MAX_POLL_WAIT` is 25 s, so the worst case is a
noticeable pause on a cold portal. Accepted — this is a read-only invoice
view, not an interactive application.

**Locked in.** The poll protocol is `PROTOCOL_VERSION = 3` with no
compatibility shim: both ends ship as one unit and a skew must fail loudly.
Enrolment now requires Apple hardware, which means **no non-Apple
authenticator can ever enrol** without a further ADR — a real constraint,
accepted because the platform is already Apple-only.

## Adversarial review

**"You built an HTTP parser. That is where the next vulnerability is."**
Fair, and the reason the surface is deliberately small: no HTTP/2, no
upgrades, no `100-continue`, no trailers, no compression. Everything is
bounded before it is read — request line 8 KiB, header block 32 KiB, body 64
KiB on the front and 8 MiB on the agent leg, chunked decoding bounded by the
same constant. Twenty-four request classes are diffed against a live nginx.
The alternative was not "no parser" — it was hyper's parser answering
protocol probes with hyper's fingerprint, which is a certainty rather than a
risk.

**"The disguise is security by obscurity."** It is, and it is not the
security. The knock is a 256-bit token compared in constant time; the
credential is a hardware passkey; the read surface is a four-route
allowlist enforced on the Mac. The disguise is there so those controls are
never *reached* by the population that sweeps address ranges. Obscurity as
the outer layer of several is not the failure mode the phrase names.

**"A poll is worse than a tunnel: the relay now holds a queue."** It holds a
**bounded** queue — 64 parked requests, 256 canary batches — in memory,
transiently, and it held response bodies in memory under the tunnel model
too. What it no longer holds is a socket, which is what let a wedged Mac
keep the portal advertised. Overload answers the ordinary parked 404, so
load is not an oracle either.

**"Console confirmation is theatre — Ervin will type the code without
looking."** Probably, most of the time. It still works, because its value is
not the *comparison* but the **requirement that a human be at the Mac at
all**. An attacker who silently completes a ceremony gets a staged
credential nobody confirms, and an alert Ervin was not expecting. The code
exists so two enrolments cannot be confused for one another, not as a
cryptographic check — it is 32 bits and is not doing cryptographic work.

**"Requiring Apple attestation locks you out if Apple changes the
hierarchy."** True. The anchor expires in 2045 and the chain check fails
closed. If Apple rotates, enrolment breaks until the vendored anchor is
updated — which is a diff, reviewed, in this repository. That is the
intended failure direction: an enrolment that cannot be verified must not
proceed.

**"`known_epoch` on the first poll is always false — does that not rotate
forever?"** It did, and that was a live bug found in this round: the first
poll of any epoch necessarily reports `false`, and treating it as "the relay
forgot me" rotated on every poll, clearing the parked queue each time so no
browser request was ever answered. The agent now tracks whether the epoch
was ever *acknowledged*; only a relay that forgets an acknowledged epoch
triggers a rotation. Pinned by two tests.

**"The Mac's clock now matters."** It does — certificate validity is checked
against wall-clock. A badly wrong clock fails enrolment closed, which is the
right direction; the alternative is accepting an expired chain.

## Alternatives considered

**Keep the framed tunnel.** Lost to an operator decision, and independently
to the wedged-Mac case a socket close does not cover.

**Uniform 404 for every request class.** Lost because it is a fingerprint:
no real server behaves that way, and the protocol-error classes are the
first thing a scanner probes.

**Intercept hyper's error responses.** Not possible — they are not a
`Service`. Investigated and rejected on that basis, not on preference.

**`reqwest` for the agent's poll client.** *Chosen*, not rejected:
`use_preconfigured_tls` hands it the exact pinned rustls config, so the
public WebPKI is never consulted, and it is the pattern `nav-transport` and
`upstream.rs` already use. Hand-rolling a second HTTP client to match the
hand-rolled server would have doubled the parser surface for nothing.

**Attestation `indirect` instead of `direct`.** `indirect` permits the
platform to substitute an anonymisation CA, which would defeat the chain
check. There is nothing to anonymise — Apple's platform attestation is
already anonymous.

**Rate-limit the enrolment alert.** Rejected. It is the one alert that must
never be coalesced away.

## Open questions

- **H1 — browser↔agent inner encryption (HPKE).** Phase 2, per Ervin's §9.4
  decision. Until it lands, Leg A's TLS terminates at the VPS and payloads
  transit relay memory in plaintext; a live root-level compromise can read a
  session while it happens. §4.3a and §4.3b remove that compromise's ability
  to *enrol*; they do not remove its ability to *watch*. Its own ADR.
- **H2 — read-only-scoped upstream bearer.** §6.4's liability stands until
  `serve.rs` grows a scoped token.
- **H3 — client certificates on the browser leg.** Available for
  desktop-only use; §9.3 chose the knock for Phase 0.
- **TLS-level canary signal (SNI, client fingerprint).** Needs a custom TLS
  acceptor. Phase 2 rather than half-done.
- **QR rendering of the enrolment URL.** §4.3 asks for it; the URL is
  printed as text. A QR encoder is a dependency `deny.toml`'s
  `unmaintained = "all"` scope would have to be argued past — worth doing,
  not worth doing silently.
