# ADR-0020 — NAV transport and credential posture correction

- **Status:** Accepted
- **Date:** 2026-05-19
- **Deciders:** Ervin
- **Partially supersedes:** ADR-0007 (only the NAV-specific clauses in
  §Transport and the matching threat-model trust-boundary entry; the
  rest of ADR-0007 remains in force unchanged)
- **Related:** ADR-0009 (NAV invoice issuing — §4 credentials,
  §6 transport behaviour), ADR-0010 (Billingo + historical NAV
  ingestion — inherits the corrected credential model)
- **Source material:** `docs/research/nav-and-billingo.md`

## Context

ADR-0007 §Transport states: *"mTLS where the counterparty supports it
(NAV does)."* The matching entry in `docs/threat-model.md` lists
trust boundary #3 as *"Backend ↔ NAV — TLS with mTLS, response
signature verification."* Both claims are factually wrong about
today's NAV interface.

Research compiled in `docs/research/nav-and-billingo.md` — drawing on
the NAV Online Számla v3.0 published interface specification, the
NAV technical-user setup documentation, and two independent shipping
open-source clients (`pzs/php-nav-online-szamla` in PHP,
`angro-kft/nav-online-szamla` in Node) — establishes the following
about the NAV submission interface that ABERP must implement:

1. NAV does not request or accept a client X.509 certificate. The TLS
   handshake at `api.onlineszamla.nav.gov.hu` is server-auth only.
2. Client authentication is **application-level inside the SOAP
   envelope**: a technical-user `login`, a SHA-512 `passwordHash` of
   that user's password, a SHA3-512 `requestSignature` computed over
   the request inputs (with the per-invoice-index extension required
   by `manageInvoice` / `manageAnnulment`), and an `exchangeToken`
   obtained from `tokenExchange` and AES-128/ECB-decrypted with the
   tenant's `xmlChangeKey`.
3. Whether NAV signs HTTP response bodies (independent of TLS) is not
   conclusively established in the consulted public sources. The
   research file flags this as **[OPEN]** pending external check by
   a Hungarian developer with shipped NAV experience.

Two ADRs already exist that touch this surface. ADR-0009 (NAV
invoice issuing) was filed in session 2 on the corrected
understanding — its §4 (Credentials) and §6 (Transport) match the
research above and include an explicit "Inherited wording correction"
section flagging that ADR-0007 needs follow-up. ADR-0007 itself has
not been touched, by intent: an Accepted ADR is changed only by
filing a superseding ADR (`adr/README.md`).

This ADR is the follow-up. It corrects only the NAV-specific
clauses. Everything else in ADR-0007 — authentication general
posture, authorization, secrets, at-rest encryption, the rest of
§Transport, supply chain, Tauri allow-list, logging, operator-as-
threat-actor controls, incident response — **remains in force
unchanged**. In particular, ADR-0007's broader principle *"mTLS
where the counterparty supports it"* is preserved as the general
posture for other counterparties (cloud sync per ADR-0016, robotics
endpoints per ADR-0013, any future external surface where the
counterparty does support client certs).

### Framing constraint (from the session-2 close)

Ervin stated, at the close of session 2: *"Security can be lifted
toward NAV only in one direction as we have to adapt to a legacy
regulator."* ABERP is not lowering its security posture. ABERP is
acknowledging that NAV does not expose an interface where mTLS or
client-side response-signature verification is on offer, and the
posture toward NAV is therefore upper-bounded by what NAV exposes.
This ADR makes that asymmetry explicit so a future contributor does
not read the absence as a preference.

## Decision

### 1. NAV transport posture (correction)

Standard HTTPS to NAV endpoints. Production:
`https://api.onlineszamla.nav.gov.hu/invoiceService/v3/`. Test:
`https://api-test.onlineszamla.nav.gov.hu/invoiceService/v3/`. The
NAV server-certificate issuing root is **pinned in ABERP's trust
store** for both endpoints; the OS trust store is not consulted for
NAV traffic. Strict hostname verification is enforced. Root rotation
is an operational event handled by an ABERP release that updates the
pinned set; the pin set is part of the binary, recorded in the
build provenance (ADR-0007).

**No client X.509 certificate is presented to NAV.** No mTLS.

### 2. NAV client-authentication posture (correction)

Authentication is application-level per ADR-0009 §4. The artifacts
the NAV adapter sends are:

- `user.login` — the technical-user identifier.
- `user.passwordHash` — SHA-512 of the technical-user password,
  computed per request.
- `user.taxNumber` — the tenant's tax number (first 8 digits).
- `user.requestSignature` — SHA3-512 over the documented input
  string for the called operation; for `manageInvoice` and
  `manageAnnulment` the per-invoice-index extension specified by NAV
  is appended before hashing.
- `exchangeToken` — obtained from `tokenExchange`, returned by NAV
  as a base64-encoded AES-128/ECB ciphertext, decrypted client-side
  with the tenant's `xmlChangeKey`, then included in the next
  modifying request.

These are the **only** client-authentication artifacts NAV accepts.
Any code path that proposes presenting a client certificate to NAV
is rejecting the protocol and must fail review.

### 3. Credential storage scope for NAV (correction)

The OS keychain holds, per tenant, exactly four NAV-related items:

1. `nav.technical_user.login`
2. `nav.technical_user.password` (plaintext at rest; hashed per
   request, never logged, zeroized on drop per ADR-0007)
3. `nav.xml_sign_key` (used to derive the `requestSignature`)
4. `nav.xml_change_key` (used to AES-128/ECB-decrypt the
   `exchangeToken`)

**No NAV client certificate is held**, because NAV does not accept
one. Wording in ADR-0007 that implies "an mTLS cert in the keychain
for NAV" is what is superseded here. Keychain scope for *other*
counterparties (Billingo API key per ADR-0010; cloud sync
credentials per ADR-0016; robotics signing keys per ADR-0013) is
unaffected by this ADR.

### 4. Threat-model trust-boundary entry (correction and split)

`docs/threat-model.md` currently lists, as boundary #3:
*"Backend ↔ NAV — TLS with mTLS, response signature verification."*
That single entry combines a wrong claim about NAV with what is
actually a separate boundary (Billingo). This ADR directs the
following edit to `docs/threat-model.md`:

- **Backend ↔ NAV** becomes its own entry: TLS with the NAV issuing
  root pinned in ABERP's trust store; application-level credential
  authentication (technical-user, SHA-512 `passwordHash`, SHA3-512
  `requestSignature`, AES-128/ECB-decrypted `exchangeToken`);
  replay protection through `requestId` + `requestTimestamp` inputs
  to the signature; **response integrity is TLS-only by decision**,
  with a retroactive-verification path provisioned via ADR-0009 §8's
  verbatim-store (see §6 below). The external fact "does NAV sign
  response bodies?" is tracked in §Open Questions §1.
- **Backend ↔ Billingo** becomes its own entry: TLS with pinned
  roots; an API-key header for authentication; scope limited to the
  one-time migration read path per ADR-0010. Detail belongs in
  ADR-0010, not here.

The other threat-model entries (UI ↔ backend, backend ↔ tenant DB,
backend ↔ printer/robotics, tenant ↔ tenant, backend ↔ LLM
provider) are unaffected.

### 5. ADR-0007's general "mTLS where supported" principle is preserved

ADR-0007 §Transport's wider clause — *"mTLS where the counterparty
supports it"* — **remains in force** for every counterparty other
than NAV. Cloud sync (ADR-0016), robotics endpoints (ADR-0013), and
any future external surface where the counterparty offers client-
certificate authentication continue to be governed by that
principle. ADR-0020 corrects only the parenthetical NAV-specific
factual claim; it does not strip mTLS as a general posture from the
security baseline. (Surgical-changes principle — CLAUDE.md rule 3 —
applied to ADRs.)

### 6. Response-body integrity: TLS-only today, retroactive verification path provisioned

**Decision.** ABERP's response-body integrity posture toward NAV is
**TLS-only**: the response body is trusted at the transport level
only (server-cert chain rooted at the pinned NAV issuing root per
§1), with no application-layer signature verification of the body
itself.

**Mitigations actually shipped.** Both the verbatim response body
and the parsed body are committed to the audit ledger per ADR-0009
§8. This provisions a **retroactive-verification path**: if NAV
publishes its response-body signing scheme later, every historical
response in the ledger can be re-verified offline against the
disclosed scheme without changing the in-flight code path. The
parsed body is additionally constrained by the pinned NAV v3.0 XSD
set (ADR-0009 §1), which bounds what a tampered response can cause
downstream.

**What is and is not pending.** *Not* pending: the ABERP-side
decision. It is made (TLS-only + verbatim store). *Pending*: an
external fact — whether NAV signs response bodies independent of
TLS, and if so, the algorithm, the verification-key location, and
the wire encoding. This is tracked in Open Questions §1 below
against the Hungarian-dev external check.

**Named amendment trigger.** A follow-up amendment to this ADR
adds the verification step when *either* of the following fires,
whichever first:

1. The external check (Hungarian-dev review of the
   `docs/research/nav-and-billingo.md` posture) confirms NAV's
   signing scheme and provides the verification key location.
2. NAV publishes the signing details in its interface
   specification.

Until one of those fires: **TLS-only**, decided, said out loud,
not papered over. Soft-asserting verification we cannot perform
is refused at the bar of CLAUDE.md rule 12 (fail loud) and the
session-2 minority-report lesson.

> **Editorial amendment, 2026-05-20.** This section was originally
> titled "Response-body integrity is [OPEN], not soft-asserted"
> with the body asserting the same TLS-only posture. The
> fortnightly review on 2026-05-20 (`docs/reviews/2026-05-20-
> fortnightly-review.md`, finding F7) surfaced that the `[OPEN]`
> label in the section title implied an undecided ABERP posture
> while the body already stated the decision. The title and body
> were rewritten editorially to be decision-shaped; no direction
> change. The external-fact tracking lives in §Open Questions §1.

### 7. Forward stance (one paragraph, principled)

If NAV's published interface specification ever introduces client-
certificate authentication, signed response bodies, or any other
transport- or message-layer integrity control, ABERP adopts it
immediately on the next release that implements the new spec patch
level. The current absence of those controls toward NAV is NAV's
choice as the legacy regulator, not ABERP's preference. The
posture toward NAV is upper-bounded by what NAV exposes; the
direction of movement is fixed.

## Consequences

**What gets easier**

- ADR-0009 §4 and the corrected ADR-0007 wording (via this ADR) now
  agree. A future contributor reading ADR-0007 together with
  ADR-0020 walks away with the correct credential model on the
  first read.
- ADR-0010 (Billingo + historical NAV ingestion) can be filed
  against a clean credential model and threat-model entry without
  inheriting the original error.
- The four-keychain-artifact scope for NAV is fixed and explicit,
  so the secrets-loading code can be specified, tested, and
  reviewed against a closed set.

**What gets harder**

- Anyone reading ADR-0007 in isolation will see a Status line that
  flags this partial supersede, will need to follow the link to
  ADR-0020 for the NAV-specific posture, and only then has the
  complete picture. We accept this two-document read as the cost of
  not rewriting ADR-0007's prose.
- The response-body integrity [OPEN] is now a documented gap in the
  threat model rather than an unstated assumption. That is the
  intended trade — surfaced loud, not averaged out — but it does
  add a tracked item that must close before the cloud surface
  expands and before any non-NAV regulator interface adopts a
  signed-body protocol that ABERP would otherwise model on this
  one.

**What we lock ourselves into**

- The pinned-issuing-root posture for NAV. If NAV rotates its
  issuing CA, ABERP ships an updated release rather than relying on
  OS trust-store updates. This is the deliberate choice — root
  rotation is rare; OS-trust-store compromise is the threat we are
  pricing in.
- The four keychain artifacts, named as in §3. Any future addition
  (e.g., a signing key NAV adopts later) is an ADR amendment, not
  an ad-hoc keychain-schema change.

## Adversarial review

The bar for an ADR this small is three concerns answered or
accepted. The three load-bearing critiques follow.

- *"Pinning the NAV issuing root in ABERP's trust store means a NAV
  CA rotation breaks production until an ABERP release ships. That
  is operational fragility you have chosen on purpose."* — Yes,
  intentionally. NAV root rotations are infrequent and announced
  in advance. The alternative — relying on the OS trust store —
  takes on the risk that any CA in that store could be coerced or
  compromised into issuing for `api.onlineszamla.nav.gov.hu`. ABERP
  is a tax-submission path; the failure mode of "wrong CA accepted"
  is materially worse than the failure mode of "release lag during
  root rotation." The pin set is part of the binary and therefore
  part of the build provenance ADR-0007 already requires; the
  rotation playbook lives next to the incident-response playbook
  (ADR-0007 §Incident response). Accepted.

- *"§6 admits you do not verify NAV response bodies independent of
  TLS. A regulator-side compromise of the response path is therefore
  undetectable. You have written that down — that does not make it
  safe."* — Correct. The decision here is to refuse the
  alternative, which would be to assert response-body verification
  in the ADR while the verification code path does not exist and
  the signing details are not in hand. The session-2 minority
  report made exactly this error — recommending "retain signature
  verification on NAV responses" despite the [OPEN] status — and
  this ADR refuses to repeat it. The mitigations available today
  are: (a) the verbatim response body and the parsed body are both
  written to the audit ledger per ADR-0009 §8, so a later
  integrity reconstruction is possible; (b) the response is acted
  upon only after parse-validation against the pinned NAV v3.0 XSD
  set (ADR-0009 §1), which constrains what a tampered response can
  cause downstream; (c) the [OPEN] item is tracked against an
  external check and against the next adversarial-review cadence.
  Accepted with the gap surfaced.

- *"You partially supersede ADR-0007 instead of rewriting its
  §Transport prose. A future contributor reading 0007 in isolation
  will write an mTLS-to-NAV adapter in good faith before they
  notice the supersede note."* — Two protections close this. First,
  ADR-0007's Status line is the very first marginal note in the
  file (after this ADR lands), so anyone reading the file at all
  sees the partial-supersede flag before they read §Transport.
  Second, ADR-0009 §4 (Credentials) is the controlling ADR for the
  NAV adapter, and a contributor implementing the NAV adapter
  cannot do so without reading ADR-0009 — at which point the
  forward link from ADR-0009 §"Inherited wording correction" to
  ADR-0020 takes them to the corrected posture. The cost of the
  two-document read is small; the cost of rewriting ADR-0007's
  prose under the partial-supersede pattern would be a precedent
  this project should not set (Accepted ADRs change only via
  superseding ADRs — `adr/README.md`). Accepted.

## Alternatives considered

- **Full supersede of ADR-0007 with a rewritten security baseline.**
  Rejected. ADR-0007's content is overwhelmingly correct and
  Accepted. A full supersede would invite re-litigation of
  unrelated clauses (authentication, authorization, secrets
  baseline, supply chain, Tauri allow-list) for which there is no
  decision change. The partial-supersede pattern modelled on
  ADR-0019 is the right fit when the wrong content is narrow and
  the rest stands.

- **In-place edit of ADR-0007's §Transport prose with a "corrected
  on 2026-05-19" note.** Rejected. Per `adr/README.md`: "A ticket
  in a tracker is not enough to change an ADR. An ADR is changed
  only by editing it in-place if status is still Proposed, or by
  filing a superseding ADR if status is Accepted or later."
  ADR-0007 is Accepted. Editing its prose without filing a
  superseding ADR breaks the rule the project depends on.

- **Defer the correction until ADR-0010 (Billingo + historical NAV
  ingestion) is filed and roll both into one ADR.** Rejected.
  ADR-0010 inherits the credential model and the threat-model entry
  from this correction. Filing 0010 against the *current* ADR-0007
  would propagate the same conflict into the read path. Cheaper to
  fix once, in a small ADR, before the next module-level ADR
  lands.

- **Assert signed-response-body verification "for completeness,"
  with a TODO to implement.** Refused at the bar of CLAUDE.md
  rule 12 (fail loud) and the session-2 lesson on averaging
  patterns. An ADR is a decision record. Asserting a control that
  does not exist and is not yet decidable is the soft-assertion
  failure mode that produced the original mTLS error.

## Open questions

These are tracked against the next adversarial-review cadence and
against the external-check items in
`docs/research/nav-and-billingo.md`.

- **NAV response-body signing — does it exist?** External-fact
  tracking item, *not* an open ABERP-side decision. §6 above
  documents ABERP's posture (TLS-only with retroactive-verification
  path) which holds regardless of how this resolves. If yes, the
  algorithm, the location of the verification key, and the wire
  encoding. Resolution path: Hungarian-dev external check on the
  research file. If positive, an amendment to this ADR adds the
  verification step and ADR-0009 §8's verbatim-store path absorbs
  it. Status check: open at the 2026-05-20 fortnightly review (F7);
  no progress in the two-week window.
- **`exchangeToken` lifetime.** Pinned by NAV server-side
  behaviour. ABERP currently treats the token as single-use per
  modifying request, per the consulted clients. Confirmation of
  the actual TTL would let ADR-0009's token-refresh logic narrow
  its retry behaviour.
- **NAV `requestId` server-side dedup window.** ABERP's two-layer
  idempotency (ADR-0009 §5) currently assumes a conservative
  window. Confirmed value lets the local layer relax.
- **Future ADR-0007 rewrite cadence.** This ADR partially
  supersedes 0007. If the count of partial supersedes against 0007
  reaches three, ADR-0007 is rewritten in full at that point
  rather than accreting marginal notes. Tracking item; not a
  current action.
