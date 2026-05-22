# ADR-0035 — bundle verifier tool (`aberp-verify`) — separate-crate CLI binary that re-verifies a per-invoice export bundle from its own bytes alone, closes F38 at the operator-driven level, pins the two-root-element acceptance per ADR-0034 §10 at Reading A

- **Status:** Accepted
- **Date:** 2026-05-22
- **Deciders:** Ervin
- **Class:** Build-phase just-in-time ADR — first
  read-side-of-the-bundle PR after the operator-driven
  audit-evidence flow closed end-to-end at PR-21 (issue →
  submit → poll → drain → retry → recover → export). Closes
  finding F38 (bundle verifier tool, named-deferred since
  ADR-0029 §"Adversarial review" #4 and re-asserted by
  PR-18 / PR-20 / PR-21 handoffs). The verifier is the
  inspector-side artifact that re-asserts every claim the
  bundle reader (PR-16 / ADR-0029) makes — from its own
  bytes alone, without trusting the producing ABERP build.
  Load-bearing deltas: §1 (CLI verb shape + binary name),
  §2 (separate-crate boundary — the verifier does NOT live
  as an `aberp verify-bundle` subcommand for inspector-side
  trust posture reasons named in §"Surfaced conflict 1"),
  §3 (invariant list — every check the verifier runs), §4
  (two-root-element acceptance per ADR-0034 §10 — Reading A
  pinned), §5 (slice-aware chain verification shape — per-
  entry hash recomputation + consecutive-seq link checks +
  seq=1 genesis anchor; gap-spanning links delegated to the
  bundle reader's chain_verified manifest claim), §6 (no
  signing — F5 unchanged per ADR-0029 §4), §7 (operator-
  visible output shape + exit-code discipline), §8 (audit-
  ledger crate surface exposure — additive pub re-exports
  of `compute_entry_hash` + `genesis_hash`; F12 NOT FIRED),
  §9 (new workspace crate vs new bin under an existing
  crate), §10 (F45 / F49 / future-provenance interactions).
  Does **not** supersede ADR-0008, ADR-0009 §8, ADR-0028,
  ADR-0029, ADR-0030, ADR-0032, ADR-0033, or ADR-0034; all
  remain in force.
- **Related:**
  - **ADR-0008 §"Hash chain"** — the chain construction
    the verifier re-walks. ADR-0008 §"Storage" + §"Adversarial
    review" — the inspector-facing audit posture the verifier
    operationalises.
  - **ADR-0009 §8** — per-invoice export bundle contract:
    "verifiable by anyone holding the attestation public key."
    PR-22 closes the verifiability surface at the
    internal-hash-chain level; the attestation-public-key
    surface remains F5-deferred per §6 below.
  - **ADR-0021 §A12** — "the canonical encoding... lives in
    ONE place inside aberp-audit-ledger." PR-22 honours this
    by depending on aberp-audit-ledger for the canonical
    encoder rather than re-implementing it (§8 below).
  - **ADR-0029** — bundle reader. PR-22 consumes the bundle
    shape ADR-0029 §3 defined and re-asserts every claim
    ADR-0029 §3's manifest fields make.
  - **ADR-0029 §"Adversarial review" #4** — the named
    trigger for this ADR: "the verifier is its own non-
    trivial surface (it must re-implement the canonical-CBOR
    encoding, the SHA-256 chain verification, the manifest
    parse)." PR-22 re-uses the canonical encoder (per
    ADR-0021 §A12); it re-implements the manifest parse +
    the slice-aware chain verification.
  - **ADR-0030** — audit-ledger mirror file. PR-22 does NOT
    consult the mirror (mirror-vs-DB is the bundle reader's
    responsibility at production time; the verifier sees
    only what the bundle carries, which is the
    `mirror_file_status` manifest field).
  - **ADR-0034 §10** — F38 (bundle verifier) interaction
    contract: "Accept both root elements for the
    InvoiceSubmissionResponse kind (recommended — preserves
    the ADR-0034 §4 chain-walk-by-order posture)" OR
    "Use the preceding entry (Attempt vs CheckPerformed) to
    branch the expected root element." PR-22 commits to
    Reading A per §4 below.
  - **Session 21 handoff (PR-17) + session 22 (PR-18) +
    session 24 (PR-20) + session 25 (PR-21) re-assertions**
    — the named-deferred F38 trigger has been re-fired four
    times across the four operator-driven audit-evidence
    closure PRs. PR-22 lifts F38 at the operator-driven
    level.
- **Source material:** ADR-0008 §"Hash chain" / §"Storage"
  / §"Adversarial review" + ADR-0009 §8 (the "verifiable
  by anyone" bar) + ADR-0029 §3 (the bundle shape) + ADR-
  0034 §10 (the two-root-element acceptance contract).

## Context

After PR-16 / ADR-0029 shipped the bundle writer and PR-17
/ ADR-0030 added the mirror-file second-source assertion,
the inspector-side question "how do I re-verify this bundle
without trusting the producing ABERP build?" became
operationally pressing. The PR-18 (drain) + PR-19 (attempt-
before-call) + PR-20 (Layer-2 queryInvoiceCheck) + PR-21
(recover-from-NAV chain reconstruction) PRs each completed
the operator-driven audit-evidence flow at a different
seam, and each handoff re-named F38 (bundle verifier) as
the next-PR candidate. By session 25's end the operator
has a complete end-to-end issue / check / drain / retry /
recover / export flow; the inspector-side close-out is the
last operator-visible artifact missing.

### What the verifier is for

An ABERP-produced bundle's manifest carries claims:

- `chain_verified: true` — at bundle-production time the
  FULL chain verified against the tenant genesis.
- `chain_verified_entries: N` — the FULL chain's entry
  count at bundle-production time.
- `entries_in_bundle: M` — the per-invoice slice's count.
- `mirror_file_present` + `mirror_file_status` — the
  mirror-vs-DB agreement state.
- per-entry `entry_hash` + `prev_hash` + ULID + RFC3339 +
  payload-bytes-via-base64 in `chain.jsonl`.
- per-NAV-bearing-entry verbatim XML bytes under
  `nav/<seq>_<kind>.xml`.

A NAV inspector reading the bundle today must EITHER
manually re-compute hashes (operator-unfriendly) OR install
a separate tool that re-asserts the claims. PR-22 ships
that tool. The verifier's job is structural: take the
bundle bytes, re-compute everything that can be re-
computed, surface every divergence loud per CLAUDE.md
rule 12.

### What the verifier is NOT for

- Not a CRYPTOGRAPHIC verifier. The bundle ships unsigned
  per ADR-0029 §4 (F5 deferred); the verifier asserts
  INTERNAL hash-chain integrity from the bundle's bytes
  alone. A future F5-lift PR adds detached-signature
  verification additively.
- Not a NAV-side cross-check. The verifier does NOT call
  NAV. The bundle is the universe of bytes it consults.
- Not a re-implementation of the canonical encoder per
  ADR-0021 §A12. The verifier depends on
  aberp-audit-ledger for the encoder + the chain
  primitives (genesis-hash + per-entry-hash); see §8.
- Not a writer of any audit entry. Read-only per the same
  posture every `export-*` and (now) `verify-*` verb uses.

### Prerequisite-gate state at PR-22 time

- **F38 trigger** has re-fired four times (PR-18 / PR-20 /
  PR-21 handoff texts; ADR-0029 §"Adversarial review" #4
  named it originally). It is now fired.
- **F5 (attestation signing key type)** trigger remains
  UNFIRED per ADR-0029 §4. The verifier ships without
  signature verification; a future F5-lift PR adds the
  detached-signature surface additively.
- **F12 four-edit ritual** does NOT fire. PR-22 introduces
  no new EventKind variant. The verifier reads existing
  kinds via `EventKind::from_storage_str`.

### Surfaced conflicts (CLAUDE.md rule 7)

Three ambiguities the build-phase will otherwise paper over:

1. **Crate boundary: separate crate vs subcommand of
   `aberp`.** Two readings:

   - **Reading A: separate crate `crates/aberp-verify`
     with its own binary `aberp-verify`** (this ADR's
     pick). Rationale — trust posture:
     - The inspector running the verifier should NOT need
       NAV credentials, billing DB connections, DuckDB,
       Tauri, keyring, rustls, axum, or any of the
       seventeen `aberp` subcommands' transitive deps. A
       separate crate cuts the surface to: `tar`, `zstd`,
       `serde_json`, `clap`, `anyhow`, `base64`, `hex`,
       `time`, `ulid`, plus `aberp-audit-ledger` for the
       canonical encoder.
     - The verifier is the WATCHER; `aberp` is the
       WATCHED. Co-locating them in the same crate
       conflates the trust boundary. An attacker who
       replaces `aberp` could plausibly also replace
       `aberp verify-bundle`; an attacker who replaces
       `aberp` cannot as easily replace `aberp-verify` if
       the inspector installs it from a different
       distribution channel.
     - The inspector's deployment posture is single-binary
       `aberp-verify` (no DB to connect, no keychain to
       unlock); separate crate matches the operational
       shape.

   - **Reading B: subcommand `aberp verify-bundle`.**
     Smaller diff (no new crate, no new bin target); reuses
     the existing CLI dispatcher. Rejected because:
     - It bundles the verifier with every NAV-side surface
       the operator binary has, multiplying the trust
       surface the inspector takes on.
     - The inspector may not have NAV credentials or a
       tenant DB; subcommand-shape forces the verifier to
       work in a "no NAV, no DB" mode that the rest of
       `aberp` doesn't share.
     - ADR-0029 §"Adversarial review" #4 explicitly named
       the separate-binary shape: "shipped as a separate
       CLI binary (`aberp-verify` or similar)."

   PR-22 commits to **Reading A**. The new crate is
   `crates/aberp-verify` with binary `aberp-verify`.

2. **Two-root-element acceptance for `InvoiceSubmissionResponse`
   entries (ADR-0034 §10).** Two readings:

   - **Reading A: accept both `<ManageInvoiceResponse>`
     AND `<QueryInvoiceDataResponse>` root elements for
     the `InvoiceSubmissionResponse` kind** (this ADR's
     pick). Rationale per ADR-0034 §10: preserves the
     chain-walk-by-order posture (the preceding entry's
     kind is the provenance marker, not the root
     element); future-proof against further provenance
     paths (F45 automatic loop, etc.).

   - **Reading B: branch on the preceding entry kind**
     (Attempt → expect `<ManageInvoiceResponse>`;
     CheckPerformed → expect `<QueryInvoiceDataResponse>`).
     More rigid; re-asserts the provenance distinction at
     verifier-side. Rejected because:
     - It introduces coupling between the verifier's
       per-entry check and the slice's ordering. A
       reorder bug (or a deliberate adversarial reorder)
       would surface as a root-element mismatch in
       addition to the chain-link break; double-fault is
       not informational gain.
     - F45's automatic-state-2-retry-loop (future surface)
       could produce additional provenance paths beyond
       the two PR-21 named; the branch-on-preceding rule
       would need extending in every such PR.
     - The chain-walk-by-order posture (ADR-0034 §4)
       already names the preceding entry as the
       provenance marker at the AUDIT-EVIDENCE level —
       the verifier-side branch would re-assert the same
       posture at the SYNTACTIC level, duplicating the
       semantic claim with no incremental defence value.

   PR-22 commits to **Reading A**. For
   `InvoiceSubmissionResponse` entries, both root
   elements are accepted; for every other XML-bearing
   kind, a single canonical root element is pinned (see
   §4 for the per-kind list).

3. **Slice chain verification rigor — what about
   gap-spanning links?** The bundle's `chain.jsonl` carries
   the per-invoice slice, NOT the full chain. A storno that
   burned seqs 13-24 for its own slice would leave the base
   invoice's bundle with seqs `…, 12, 25, 28, …` — non-
   contiguous. Two readings:

   - **Reading A: refuse to verify any bundle whose slice
     is non-contiguous.** Rejected because every chain-
     linked invoice's bundle (every BASE with a STORNO or
     MODIFY against it) would refuse — the operational
     shape is the wrong default.

   - **Reading B: verify what's verifiable from the slice,
     delegate gap-spanning to the manifest's
     `chain_verified` claim** (this ADR's pick). The
     verifier:
     - Re-computes `entry_hash` for every slice entry from
       its canonical CBOR encoding (catches per-entry
       tampering).
     - For consecutive-seq entries within the slice
       (seq[i+1] == seq[i]+1), checks the chain link
       (prev_hash[i+1] == entry_hash[i]).
     - For the seq=1 entry IF it appears in the slice,
       checks prev_hash against `genesis_hash(tenant)`.
     - For non-contiguous gaps (seq jumps from N to M>N+1),
       the verifier emits an OPERATOR-VISIBLE NOTE naming
       the gap and the manifest's `chain_verified` claim
       (the ABERP-build-time assertion that the FULL chain
       through entry N+1 to M-1 verified). The verifier
       does NOT fail the bundle on a gap; the gap is the
       expected per-invoice-slice shape per ADR-0029 §2.

   PR-22 commits to **Reading B**. The verifier surfaces
   what it can re-verify and surfaces what it must trust
   (the manifest's `chain_verified` claim). This is the
   honest posture per CLAUDE.md rule 12 — silent claim of
   complete verification when the slice fundamentally
   cannot carry the full chain would be worse than the
   loud delineation.

## Decision

### 1. CLI surface for `aberp-verify`

**Binary name:** `aberp-verify`.

**Rationale.** Distinct from `aberp` per §"Surfaced
conflict 1" Reading A. The `-verify` suffix names the
intent (read-only verification of an existing artifact);
mirrors the `cargo-deny` / `cargo-audit` external-tool
convention.

**Argument shape** (clap-flavoured) — TWO positional fields
(narrower than every other ABERP command):

| Flag | Type | Default | Purpose |
|---|---|---|---|
| `--bundle` | `PathBuf` | none (required) | Path to the `.tar.zst` bundle file to verify. |
| `--quiet` | `bool` | `false` | Suppress per-check OK lines; print only the summary + any failures. Default verbose so an inspector reading the output sees every check that ran. |

**What `aberp-verify` does NOT do.**

- **Does NOT call NAV.** No network access. No keychain
  consultation.
- **Does NOT open any DB.** The bundle is the universe of
  bytes consulted.
- **Does NOT write any file.** Output is stdout +
  stderr + exit code.
- **Does NOT take a `--tenant` or `--db` flag.** The
  bundle's manifest names the tenant; the verifier reads
  it from there.
- **Does NOT take an `--allow-unsigned` flag.** PR-22
  bundles are uniformly unsigned per ADR-0029 §4; a
  future F5-lift PR adds the flag when the signed-bundle
  surface lands.
- **Does NOT take a `--format json` flag.** Pre-emptive
  output-shape flag for shapes that do not yet exist is
  the CLAUDE.md rule 2 violation; a future PR may add
  alternative output shapes if operational pattern
  surfaces a need.

### 2. Separate-crate boundary (decides §"Surfaced conflict 1")

PR-22 lands a new workspace member `crates/aberp-verify`
with one binary target `aberp-verify`. The crate's
dependencies are deliberately narrow:

- `aberp-audit-ledger` (workspace member) — for the
  canonical CBOR encoder + chain primitives per ADR-0021
  §A12 (one place for the encoder). See §8 for the
  additive pub-re-export the audit-ledger crate gains.
- `tar`, `zstd` (workspace deps) — for the bundle's
  archive format per ADR-0029 §8.
- `serde_json`, `base64`, `hex`, `time`, `ulid` (workspace
  deps) — for the chain.jsonl parse + the entry
  reconstruction.
- `clap`, `anyhow`, `tracing`, `tracing-subscriber`
  (workspace deps) — standard CLI scaffolding.

**Notably NOT depended on:** `aberp` (the operator binary),
`aberp-billing`, `aberp-nav-transport`, `aberp-nav-xsd-validator`,
`tauri`, `tauri-build`, `axum`, `axum-server`, `rcgen`,
`rustls`, `rustls-pemfile`, `reqwest`, `keyring`,
`duckdb` (transitively from `aberp-audit-ledger` —
acceptable; the alternative is splitting audit-ledger into
a chain-primitives sub-crate, which is a substantial
refactor PR-22 deliberately does not undertake per
CLAUDE.md rule 3).

### 3. Invariant list — every check the verifier runs

The verifier walks the bundle bytes once and runs the
following checks. Each check produces either OK or FAIL
output; a single FAIL fails the bundle (exit code 1).

1. **Archive shape.** The file at `--bundle` decompresses
   via `zstd`; the resulting tar stream parses; every
   entry's path is under `bundle/`. Missing manifest or
   missing chain.jsonl → FAIL.
2. **Manifest version.** Parsed manifest's `version` field
   equals 1 (the only version PR-22 understands). Unknown
   version → FAIL with a forward-compatibility note ("a
   newer `aberp-verify` may understand this bundle").
3. **Manifest field set.** Every ADR-0029 §3 field is
   present (version, invoice_id, tenant_id, generated_at,
   binary_hash, nav_xsd_version, chain_verified,
   chain_verified_entries, entries_in_bundle, signed,
   signature_status, mirror_file_present, mirror_file_status).
4. **Manifest invariants.** `chain_verified == true`;
   `signed == false` AND `signature_status == "deferred-per-f5"`
   (PR-22's universe of bundles per ADR-0029 §4); the
   `mirror_file_status` is one of the three known values
   ("verified-agreement", "absent-pre-pr-17", or the
   deferred-per-f10 marker from pre-PR-17 bundles).
5. **chain.jsonl line count.** Equals manifest's
   `entries_in_bundle`. Mismatch → FAIL (silent omission
   territory per CLAUDE.md rule 12).
6. **Per-entry decode.** Every chain.jsonl line decodes as
   JSON; every field is present; `payload` base64-decodes;
   `kind` parses via `EventKind::from_storage_str`;
   hashes hex-decode to 32 bytes; `time_wall` parses
   RFC3339.
7. **Per-entry tenant pin.** Every entry's `tenant_id`
   equals manifest's `tenant_id`. A divergence would
   indicate cross-tenant contamination at bundle write
   time (which the bundle reader does not today protect
   against — the verifier surfaces it loud).
8. **Per-entry hash recomputation.** For each entry,
   reconstruct an `Entry` value, call
   `aberp_audit_ledger::compute_entry_hash(&entry)`,
   compare against the entry's claimed `entry_hash`.
   Divergence → FAIL with the seq named (catches per-
   entry tampering).
9. **Consecutive-seq chain links.** For each pair of
   slice entries where seq[i+1] == seq[i]+1: assert
   prev_hash[i+1] == entry_hash[i]. Divergence → FAIL.
10. **Genesis anchor (if applicable).** If the slice
    contains an entry with seq=1, assert its prev_hash
    equals `genesis_hash(tenant)`. Divergence → FAIL.
11. **Gap NOTE (informational).** For each non-
    consecutive-seq pair (seq[i+1] > seq[i]+1), emit an
    operator-visible NOTE naming the gap and the
    manifest's `chain_verified` claim per §"Surfaced
    conflict 3" Reading B.
12. **Bundle-membership pin.** Every slice entry's
    payload, when probed via the same any-id-field-
    equality posture ADR-0029 §2 uses, must match the
    manifest's `invoice_id`. Divergence → FAIL (catches
    the silent-omission failure mode CLAUDE.md rule 12
    names — an entry that doesn't reference the manifest's
    invoice id has no business being in this bundle).
13. **Per-NAV-bearing-entry XML pin.** For each entry kind
    that carries verbatim NAV bytes per §4: extract the
    payload's `request_xml` or `response_xml`; assert that
    bytes-equal an entry under `nav/<seq>_<kind>.xml` in
    the archive; assert that the root element matches the
    per-kind expected list per §4.
14. **Bundle-internal cross-totals.** Count of NAV-bearing
    entries in chain.jsonl equals count of `nav/*.xml`
    files in the archive (modulo `InvoiceSubmissionAttemptFailed`
    and `InvoiceCheckPerformed`'s optional response_xml —
    the verifier knows which kinds have optional XML and
    counts accordingly).

### 4. Two-root-element acceptance per ADR-0034 §10 (decides §"Surfaced conflict 2")

The per-kind expected root-element list:

| EventKind | Expected root element(s) | Optional? |
|---|---|---|
| `InvoiceSubmissionAttempt` | `<ManageInvoiceRequest>` | required |
| `InvoiceSubmissionResponse` | `<ManageInvoiceResponse>` OR `<QueryInvoiceDataResponse>` | required (one of two per ADR-0034 §10 Reading A) |
| `InvoiceAckStatus` | `<QueryTransactionStatusResponse>` | required |
| `InvoiceAnnulmentSubmissionAttempt` | `<ManageAnnulmentRequest>` | required |
| `InvoiceAnnulmentSubmissionResponse` | `<ManageAnnulmentResponse>` | required |
| `InvoiceAnnulmentAckStatus` | `<QueryTransactionStatusResponse>` | required |
| `InvoiceAnnulmentReceiverConfirmation` | `<QueryInvoiceDataResponse>` | required |
| `InvoiceSubmissionAttemptFailed` | any (payload's `response_xml` is `Option<Vec<u8>>`) | optional — when present, no root-element pin (failure-class bodies vary) |
| `InvoiceCheckPerformed` | request: `<QueryInvoiceCheckRequest>`; response: `<QueryInvoiceCheckResponse>` when present | response optional per ADR-0033 §2 |

**Root element detection.** Bytes are checked at the
prefix level — the first non-whitespace, non-XML-prolog
opening tag's name. Namespaces are matched at the local-
name level (an inspector running `aberp-verify` against
a bundle with namespace-prefixed elements like
`<ns0:ManageInvoiceResponse>` should accept; the suffix
match is the operational shape).

**Why no full XML parse.** Per CLAUDE.md rule 2 — minimum
code. A full XML parse for what reduces to "does the first
tag's local name match one of N expected strings" is the
exact "for future flexibility" trap rule 2 names. The
prefix check catches the divergence cases the verifier
cares about (wrong-kind bytes stored in payload, wholesale
XML replacement, root-element renaming) without pulling
quick-xml into the verifier's dep surface.

### 5. Slice-aware chain verification (decides §"Surfaced conflict 3")

The verifier walks the slice in seq order (which is the
order chain.jsonl was written by ADR-0029 §3). For each
adjacent pair:

- **Consecutive (seq[i+1] == seq[i]+1):** check
  prev_hash[i+1] == entry_hash[i]. Loud-fail on mismatch.
- **Non-consecutive (seq[i+1] > seq[i]+1):** emit NOTE
  naming the gap. The verifier cannot re-assert the chain
  link across the gap from the slice alone; this is
  delegated to the manifest's `chain_verified` claim.

**For the seq=1 entry (if present):** prev_hash must equal
`genesis_hash(tenant)` per ADR-0008 §"Entry shape".

**For seqs > 1 not preceded by a slice entry:** the
verifier CANNOT re-assert the chain link (the predecessor
entry is not in the bundle). Per §"Surfaced conflict 3"
Reading B, this is the expected per-invoice-slice shape;
the manifest's `chain_verified == true` claim is the
ABERP-build-time assertion of full-chain verification.

**Why this is honest.** A NAV inspector receiving a
bundle with a gap (typical: every chain-linked invoice's
bundle) wants the verifier to say "I verified what I
could; here is what I had to trust." The alternative —
silently claiming complete verification — would let a
forged bundle pass as if it were end-to-end re-verified.
CLAUDE.md rule 12 names this exact failure mode.

### 6. No signing — F5 unchanged

Per ADR-0029 §4, F5 remains DEFERRED. PR-22's verifier
does NOT verify any signature; it asserts that the
manifest's `signed == false` and `signature_status ==
"deferred-per-f5"` (catching a bundle that claims to be
signed but the verifier doesn't know how to verify). A
future F5-lift PR additively extends the verifier with a
`--public-key` flag and a detached-signature verification
pass; PR-22's surface is forward-compatible.

### 7. Operator-visible output shape

The verifier prints to stdout one line per check (unless
`--quiet`), then a summary block, then an exit code:

```
aberp-verify: <path>
  [OK]   archive shape: bundle/ root + manifest.json + chain.jsonl
  [OK]   manifest version: 1
  [OK]   manifest field set: 13/13
  [OK]   manifest signed=false (signing deferred per F5)
  [OK]   chain.jsonl entries: 12 (matches manifest.entries_in_bundle)
  [OK]   per-entry hash recomputation: 12/12
  [OK]   consecutive chain links: 8/8 (4 gaps NOTED below)
  [NOTE] seq gap 16 -> 23 (delegated to manifest.chain_verified=true)
  [NOTE] seq gap 25 -> 28 (delegated to manifest.chain_verified=true)
  [OK]   NAV-bearing XML files: 7/7 present and root-pinned
  [OK]   bundle membership: 12/12 entries reference invoice id inv_...

SUMMARY: bundle OK (12 entries verified from bundle bytes alone;
         full-chain claim trusted via manifest.chain_verified=true;
         this bundle is UNSIGNED — F5 deferred per ADR-0029 §4).
```

On failure, the FAIL line names the seq + the diagnostic;
the verifier continues to surface every check (operator
sees the full diagnostic picture, not just the first
failure):

```
  [FAIL] per-entry hash recomputation at seq=15: recomputed
         entry_hash 3f9a... does not match claimed entry_hash 1c2b...
         (entry has been tampered with after it was written)
```

Exit code: 0 on all-OK, 1 on any FAIL. NOTE lines do NOT
fail the bundle (gap NOTEs are expected per §"Surfaced
conflict 3" Reading B).

### 8. Audit-ledger crate surface exposure (F12 NOT FIRED)

PR-22 needs the canonical encoder + the chain primitives
from `aberp-audit-ledger`. The current surface exposes
`Ledger`, `Entry`, `EntryHash`, etc. but does NOT expose
`compute_entry_hash` or `genesis_hash` — both live in the
private `chain` module.

PR-22 adds two `pub use` re-exports in
`crates/audit-ledger/src/lib.rs`:

```rust
pub use chain::compute::compute_entry_hash;
pub use chain::genesis::genesis_hash;
```

**Additive only.** No function signature changes. No
behaviour changes. No new EventKind variants. The F12
four-edit ritual (variant + as_str + from_storage_str +
round-trip-test) does NOT fire. The existing internal
callers (`storage::append_in_tx` etc.) continue to use
the long-path imports; the public re-exports are
consumed only by `aberp-verify`.

**Why expose, not re-implement.** ADR-0021 §A12 names
the canonical encoder as "lives in ONE place inside
aberp-audit-ledger." Re-implementing it inside
aberp-verify would create a second copy that drifts
across future encoder changes — exactly the
"swallowed-twice" failure mode CLAUDE.md rule 7 names.

### 9. New crate `crates/aberp-verify` vs new bin under existing crate

PR-22 commits to a NEW workspace crate (not a new bin
under `apps/aberp/` or `crates/audit-ledger/`). Rationale:

- A bin under `apps/aberp/` would inherit `aberp`'s
  `[dependencies]` block — NAV transport, billing, axum,
  rustls, keyring, tauri-build et al. The verifier needs
  none of those.
- A bin under `crates/audit-ledger/` would mix a CLI
  binary into a library crate. The audit-ledger crate is
  deliberately library-only per ADR-0021's layering
  discipline; adding a bin would violate the crate's
  shape contract.
- A new workspace crate `crates/aberp-verify` keeps
  inspector-side deps narrow, signals the trust boundary
  cleanly, and matches what ADR-0029 §"Adversarial
  review" #4 named.

### 10. F45 / F49 / future-provenance interactions

**F45 (automatic state-2 retry loop, named-deferred).**
When F45 lifts, it may add NEW provenance paths for the
`InvoiceSubmissionResponse` kind beyond the two PR-21
named (manageInvoice POST → ManageInvoiceResponse;
recover-from-nav → QueryInvoiceDataResponse). Per §4
Reading A, additional root elements would extend the
`InvoiceSubmissionResponse` kind's expected list
additively — a future amendment ADR names each new root
element when its trigger fires. PR-22 does NOT
pre-emptively widen the list.

**F49 (Layer-2-aware mark-abandoned, named-deferred).**
F49 does not affect the verifier's invariant list (no
new EventKind, no new XML root element). Unchanged from
PR-22's perspective.

**Future audit-evidence kinds (post-PR-22).** A future
EventKind addition that carries verbatim XML must extend
§4's per-kind expected-root-element list AND the verifier's
NAV-bearing kind list. The F12 four-edit ritual continues
to apply for the variant addition itself; PR-22 adds an
implicit fifth-edit obligation on the verifier's per-kind
table. This is named in PR-22's source (a comment on the
per-kind table reminds the contributor to extend it).

## Open questions

Tracked against the next fortnightly adversarial review
and named external-check items in `docs/research/nav-and-
billingo.md`:

- **Verifier performance at hyperscale.** PR-22's verifier
  walks chain.jsonl entries one at a time and re-computes
  SHA-256 + canonical-CBOR for each. For a per-invoice
  slice bound at the hundreds-of-entries level (the
  ADR-0009 per-tenant volume bound), this is bounded; at
  the full-tenant-export level (a future PR per ADR-0008
  §"Export"), the linear walk may be a noticeable wall-
  clock cost. Not pre-emptively optimised per CLAUDE.md
  rule 2.
- **Verifier-side mirror cross-check.** PR-22 does NOT
  consult the mirror file (the verifier sees only the
  bundle bytes; the mirror lives outside the bundle).
  If a future operational pattern surfaces a need
  ("the bundle's mirror_file_status was generated by an
  ABERP I don't trust; can I cross-check?"), a future
  PR adds a `--mirror <path>` flag that consults the
  inspector-supplied mirror. Not pre-emptively here.
- **Verifier-side signed-bundle support.** F5 trigger
  has NOT fired. When it fires, the future F5-lift PR
  additively extends `aberp-verify` with a `--public-key`
  flag and a detached-signature verification pass. The
  PR-22 verifier's manifest-`signed=false` pin is the
  forward-compatibility marker; the future PR replaces
  the pin with a "signed and verified against
  --public-key" branch.
- **Two-root-element acceptance and future provenance
  paths.** §10 names the contract; if F45 (or any future
  PR) introduces a third root element for
  `InvoiceSubmissionResponse`, a future amendment ADR
  extends §4's expected-root-element list additively
  alongside that PR.

## Consequences

**What gets easier**

- A NAV inspector receiving an ABERP bundle can run ONE
  command (`aberp-verify --bundle <path>`) to re-verify
  every claim the bundle makes. The bundle's hash chain,
  per-entry integrity, payload-vs-nav-XML byte equality,
  bundle membership, and manifest invariants are all
  re-asserted from the bundle's own bytes — no DB, no
  network, no trust in the producing ABERP build.
- F38 closes at the operator-driven level. The trust
  boundary the bundle reader (PR-16) opened is now
  closed end-to-end with the inspector-side artifact.
- The trust boundary between WATCHER and WATCHED is
  structural: `aberp-verify` is a separate crate with
  narrow deps; an attacker compromising `aberp` cannot
  as easily compromise the verifier.
- The two-root-element acceptance for recovered Response
  entries (ADR-0034 §10) is pinned at Reading A; future
  provenance paths land additively without re-litigating
  the verifier's contract.
- The audit-ledger crate's canonical encoder remains the
  "one place" per ADR-0021 §A12 — the verifier reuses
  it via the additive `pub use` re-exports rather than
  re-implementing it.

**What gets harder**

- A NEW workspace crate to maintain. The crate is
  deliberately small (~600 LoC across src/ + tests/)
  and its surface is read-only; the maintenance cost is
  bounded.
- Two new `pub use` re-exports on `aberp-audit-ledger`'s
  public surface. The internal `chain` module's items
  become externally observable; a future change to
  `compute_entry_hash` or `genesis_hash` becomes a
  public-API change (semver implications under the
  workspace's `0.0.0` posture remain none today, but
  the surface area grows).
- The per-kind expected-root-element table (§4) becomes
  a maintenance burden parallel to the F12 ritual: every
  future EventKind addition that carries verbatim XML
  must extend the table. PR-22 names this in source
  comments; a future test pins the table's exhaustiveness.

**What we lock ourselves into**

- Binary name `aberp-verify` and arg names (`--bundle`,
  `--quiet`). Rename requires an amendment ADR.
- Two-root-element acceptance for `InvoiceSubmissionResponse`
  (the ADR-0034 §10 Reading A pick). Switching to Reading
  B (branch on preceding entry kind) would require a
  superseding ADR and a verifier-side rewrite.
- Slice-aware chain verification with gap NOTEs per
  §"Surfaced conflict 3" Reading B. Refusing gap-spanning
  bundles outright (Reading A) is a superseding-ADR
  amendment.
- The audit-ledger pub re-exports of `compute_entry_hash`
  + `genesis_hash`. Removing them is a breaking change
  to the verifier crate.
- No signing (F5 deferred). The verifier's `signed=false`
  pin assumes PR-22-era bundles; future F5-lifted bundles
  flip the pin AND require a new `--public-key` arg.
- Manifest version 1. Bundles produced by a future
  manifest version 2 are not verifiable by PR-22's
  verifier; a future PR updates the verifier alongside
  the writer.

## Adversarial review

A hostile NAV inspector + a hostile-engineer review,
alternating. ADR-README bar is three; FIVE surfaced
because the verifier's trust posture, gap-handling, and
two-root-element acceptance are load-bearing decisions
that span multiple prior ADRs.

1. **"The verifier ships in the same git repository as
   the bundle writer. An attacker who compromises the
   build process can produce both a forged bundle AND a
   verifier that 'verifies' it. The separate-crate
   posture is theater — same maintainer, same CI, same
   trust anchor."** Accepted, surfaced. The mitigation:
   - The separate-crate posture provides DEFENCE IN
     DEPTH, not a complete trust break. An attacker
     compromising the build process can indeed forge
     both — but the inspector's WORKFLOW differs: an
     inspector running an `aberp-verify` they obtained
     from a different distribution channel (vs.
     downloading from the same build) gets meaningful
     trust separation.
   - The verifier's narrow deps make it AUDITABLE in a
     way `aberp` itself is not. An inspector with a
     security review budget can read the entire
     verifier source (~600 LoC + 200 LoC tests + the
     audit-ledger crate's chain primitives) and verify
     the verifier itself. The full `aberp` binary
     (~20K LoC across the workspace today) is not
     similarly auditable.
   - The `binary_hash` manifest field is a load-bearing
     reproducibility marker per ADR-0008 §"Adversarial
     review" bullet 2; the inspector can cross-check
     against a known-good `aberp` binary hash. PR-22's
     verifier surfaces this hash in the output (not yet
     as a pinned assertion; a future PR adds
     `--expect-binary-hash <hex>` for the operator-
     supplied trust-anchor case).
   **Accepted with trust-anchor mitigation named.**
   Future PR adds the `--expect-binary-hash` flag when
   the operational pattern (an inspector receiving a
   bundle from a third-party ABERP deployment) surfaces.

2. **"The verifier 'accepts both root elements' for
   `InvoiceSubmissionResponse` per ADR-0034 §10
   Reading A. A future PR that adds a THIRD provenance
   path (e.g., F45's automatic state-2 retry loop) must
   extend the list — but PR-22 has no mechanism that
   forces the future contributor to do so. The verifier
   could silently accept the third root element OR
   silently reject it; either way the failure mode is
   wrong."** Accepted, surfaced. The mitigation:
   - PR-22's per-kind expected-root-element table (§4)
     is an inline data structure in the verifier source
     with a top-of-table doc comment naming the
     extension obligation: "When a future PR adds a new
     EventKind or extends an existing EventKind's
     provenance, this table extends." This is the same
     discipline ADR-0029 §2's BundleMembershipProbe
     uses; the failure mode is the same (silent
     omission) and the mitigation is the same (a hand-
     listed table with a contributor-facing comment).
   - A unit test pins the table against the current
     EventKind variant set: every NAV-bearing kind is
     listed; a future EventKind variant that the test
     doesn't yet cover causes the test to fail. (The
     test asserts exhaustiveness against
     `EventKind::from_storage_str`'s known kinds.)
   - The F12 ritual continues to apply to the variant
     addition itself; PR-22 adds an implicit fifth-edit
     obligation. The fifth edit is "extend the verifier's
     per-kind table" — failure to do so will surface in
     the exhaustiveness test.
   **Accepted with table + exhaustiveness-test
   mitigation.**

3. **"The gap-handling posture (§"Surfaced conflict 3"
   Reading B) emits a NOTE instead of failing the bundle
   for non-consecutive seqs. An attacker who replaces a
   chunk of the chain with adversarial entries could
   construct a bundle where the slice has 'gaps' that
   correspond to the deleted entries — the verifier
   would NOTE the gap and still report bundle OK."**
   Accepted, surfaced. The mitigation:
   - The manifest's `chain_verified == true` claim is
     the ABERP-build-time assertion that the FULL
     chain (including the gap-spanning entries)
     verified. If an attacker replaced bundle bytes
     with a different bundle, the attacker would need
     to also forge the manifest's `chain_verified`
     claim — which the verifier checks against the
     entries it CAN re-verify. A gap where the
     attacker deleted entries would still leave the
     surviving entries' per-entry hash recomputation
     intact (those are computed against the canonical
     encoding of the entry; the attacker cannot forge
     them without re-computing). A gap where the
     attacker REPLACED entries with adversarial ones
     would surface as a per-entry-hash failure on the
     adversarial entries themselves.
   - The honest acknowledgment: per-invoice-slice
     verification is FUNDAMENTALLY weaker than full-
     chain verification (the slice doesn't carry the
     gap entries; the verifier cannot re-verify
     across the gap). The named alternative is a
     full-tenant export bundle (future PR per
     ADR-0008 §"Export") which the verifier could
     re-verify end-to-end. PR-22's per-invoice-slice
     verifier is the operationally-useful subset; the
     full-tenant variant lands when the trigger fires.
   - A future PR may add `--strict-no-gaps` flag that
     fails on any non-consecutive seq; not pre-
     emptively here per CLAUDE.md rule 2 (the
     operational pattern would surface the need).
   **Accepted with full-chain-claim-trust + future-
   strict-flag-named mitigations.**

4. **"The verifier depends on `aberp-audit-ledger` for
   the canonical encoder per ADR-0021 §A12. But
   `aberp-audit-ledger` transitively depends on
   `duckdb` (the storage backend). The verifier
   inherits a substantial native-code dep
   (`duckdb-sys` vendors libduckdb) — for a tool
   whose entire job is reading bytes from a tar.zst.
   The 'narrow deps' claim of §2 is partially
   defeated."** Accepted, surfaced. The mitigation:
   - The duckdb transitive cost is real but bounded.
     PR-22 does NOT call into duckdb at runtime; the
     dep is paid only at compile time + binary size.
     A `cargo build -p aberp-verify` builds duckdb-
     sys as a side-effect but no runtime usage
     occurs.
   - The alternative — extracting a `aberp-audit-chain`
     sub-crate that contains only the canonical
     encoder + chain primitives (no duckdb) — is a
     substantial refactor of an existing crate. PR-22
     deliberately defers this per CLAUDE.md rule 3
     (surgical changes — extracting a sub-crate
     while landing the verifier crate would blend two
     surfaces). A future PR may extract the sub-crate
     if the duckdb-transitive cost becomes a real
     operational concern (binary size, build time,
     supply-chain audit).
   - The verifier's source-of-truth canonical encoder
     reuse honours ADR-0021 §A12 — that is the more
     important invariant.
   **Accepted with sub-crate-extraction-as-future-PR
   named.**

5. **"The verifier reads the manifest's `mirror_file_status`
   as one of three acceptable strings (`verified-agreement`,
   `absent-pre-pr-17`, or the legacy `deferred-per-f10`
   marker). The verifier does NOT independently re-check
   the mirror file (the mirror lives outside the bundle).
   An attacker who forged the bundle bytes could simply
   set `mirror_file_status: 'verified-agreement'` — the
   verifier wouldn't know the difference."** Accepted,
   surfaced. The mitigation:
   - The mirror file is BY DESIGN a second-source
     corroboration that lives OUTSIDE the bundle per
     ADR-0030. The verifier sees only the bundle; it
     cannot cross-check what isn't there.
   - A future `aberp-verify --mirror <path>` flag (per
     §"Open questions") adds the inspector-supplied
     mirror cross-check. Not pre-emptively here per
     CLAUDE.md rule 2 — the operational pattern hasn't
     surfaced the need.
   - The honest framing in the verifier's output: the
     `mirror_file_status: verified-agreement` line is
     ECHOED from the manifest, not RE-VERIFIED by the
     tool. An inspector reading the output sees the
     echo and can decide whether to cross-check
     manually.
   **Accepted with future-flag-named mitigation. The
   verifier's mirror-status line is documented as an
   ECHO, not a re-verification.**

## Alternatives considered

- **Reading B for crate boundary (`aberp verify-bundle`
  subcommand).** Rejected per §"Surfaced conflict 1" +
  §2. Trust-posture cost; transitive-dep cost.

- **Reading B for two-root-element acceptance (branch on
  preceding entry kind).** Rejected per §"Surfaced
  conflict 2" + §4. Couples the verifier to slice
  ordering; surfaces double-faults; doesn't extend
  cleanly to future provenance paths.

- **Reading A for gap handling (refuse non-consecutive
  slices).** Rejected per §"Surfaced conflict 3" + §5.
  Every chain-linked invoice's bundle would refuse —
  wrong default.

- **Re-implement the canonical encoder inside
  aberp-verify.** Rejected per §8 + ADR-0021 §A12.
  One-place discipline is load-bearing.

- **Lift F5 (attestation signing) in PR-22.** Rejected
  per §6. F5's trigger has not fired; the verifier's
  internal-hash-chain re-verification is meaningful on
  its own.

- **Verifier writes an audit entry naming itself.**
  Rejected — the verifier is read-only and has no DB
  to write to. The operator-visible output line is the
  canonical record per the same posture ADR-0029 §7
  uses for the bundle reader.

- **Verifier ships in a separate git repository.**
  Rejected for PR-22. The repo split is a real future
  consideration (per §"Adversarial review" #1's trust-
  separation concern) but spinning up a separate
  repository's CI / release / supply-chain processes
  is its own ADR-class decision. PR-22's separate-
  crate-same-repo posture is the smallest step that
  delivers the trust-posture value without the
  repo-split overhead.

- **Verifier accepts a `--db <path>` flag to cross-check
  against the producing ABERP's DB.** Rejected per §1's
  "What `aberp-verify` does NOT do." The verifier's
  job is bundle-bytes-only verification; cross-
  checking against the producing DB defeats the
  trust-separation posture and would require the
  inspector to have keychain / NAV credential access.

## Follow-on PRs unblocked by this decision

- **PR-22 — bundle verifier code.** Implements §1-§10
  above plus:
  - `crates/aberp-verify/Cargo.toml` (new workspace
    member, narrow deps).
  - `crates/aberp-verify/src/main.rs` (CLI entry).
  - `crates/aberp-verify/src/lib.rs` (public surface
    + `verify_bundle` orchestrator).
  - `crates/aberp-verify/src/bundle.rs` (tar.zst read
    + manifest parse + chain.jsonl parse).
  - `crates/aberp-verify/src/verify.rs` (the §3 invariant
    checks).
  - `crates/aberp-verify/src/report.rs` (operator-visible
    output composition).
  - `crates/aberp-verify/tests/` integration tests using
    aberp's own bundle writer as the test fixture.
  - `crates/audit-ledger/src/lib.rs` — two additive
    `pub use` re-exports (`compute_entry_hash`,
    `genesis_hash`).
  - Workspace `Cargo.toml` — add `crates/aberp-verify`
    to `[workspace] members`.

- **Future F5 lift PR.** Adds `--public-key` flag +
  detached-signature verification pass; flips the
  `signed=false` pin to a "signed and verified" branch.

- **Future `--expect-binary-hash` flag PR.** Per §"Adversarial
  review" #1; trigger: an operational case of an
  inspector receiving a bundle from a third-party
  ABERP deployment.

- **Future `--mirror <path>` flag PR.** Per §"Open
  questions" + §"Adversarial review" #5; trigger:
  operational pattern where the mirror cross-check
  needs to happen at the verifier rather than at
  bundle-write time.

- **Future `--strict-no-gaps` flag PR.** Per §"Adversarial
  review" #3; trigger: operational pattern where
  gap-spanning bundles should be refused (e.g., a
  full-tenant export verifier).

- **Future per-tenant full-export verifier PR.** Per
  ADR-0008 §"Export". Same `aberp-verify` binary
  (extends `--bundle` to accept either per-invoice
  or per-tenant shapes); same invariant list extended
  to walk the full chain end-to-end (no gaps in the
  full-tenant case).

- **Future sub-crate extraction PR.** Per §"Adversarial
  review" #4. Extract `aberp-audit-chain` from
  `aberp-audit-ledger` if duckdb-transitive cost
  becomes a real operational concern.
