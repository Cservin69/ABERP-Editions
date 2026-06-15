# ADR-0083 — CAD-blob encryption-at-rest + read-audit.

- **Status:** Accepted
- **Date:** 2026-06-16
- **Deciders:** Ervin (via S430 brief — last item of quoting batch-2, auto-mode).
- **Implements:** the promise made in **ADR-0066 §Decision** ("CAD blob encrypted at rest (AES-GCM keychain key, audit on every read — ADR-0007/0014)"). Partially discharges the deferred "encryption-at-rest key management" item called out in ADR-0007, scoped to the CAD-blob path only.
- **Related:** ADR-0007 (§Secrets — keychain-bound material), ADR-0067 (auto-quote pipeline architecture, the consumer), ADR-0081 (`aberp-verify` NAV-leakage coverage gate — the 3 new EventKinds re-review it), `[[trust-code-not-operator]]`, `[[hulye-biztos]]`, `[[no-sql-specific]]`.

## Context

The auto-quote pricing pipeline (`crate::quote_pricing_pipeline`) downloads a customer's CAD file (`.stl` / `.step`) from the storefront and lands it on the local filesystem under `artifact_dir/<quote_id>/<filename>`. Pre-S430 those bytes sat on disk **in the clear** — a customer's proprietary part geometry, readable by anyone (or any process) with filesystem access to the ABERP host. ADR-0066 promised AES-GCM encryption-at-rest with a keychain key and a read-audit; this ADR delivers exactly that slice.

Three facts from the codebase shaped the design (verified, not assumed, per `[[dont-invent-code-surfaces]]`):

1. **The only producer of an on-disk CAD blob is `enqueue_one`** (`quote_pricing_pipeline.rs`), a single `std::fs::write(&dest_path, &body)`.
2. **The only consumer of CAD *bytes* is `advance_extract`**, and it consumes a **file path**, not bytes: it hands `cad_local_path` to `aberp-cad-extract-wrapper::CadExtractor`, which spawns a **Python subprocess** that reads the file off disk itself. There is no in-Rust byte read to intercept.
3. **There is no authenticated operator CAD-download or customer CAD-download endpoint** (`serve.rs:18747` documents this explicitly — only `cad_filename` is surfaced, never the file). The storefront keeps its own copy of the upload; ABERP *pulls* from the storefront, it does not *serve* CAD back.

## Decision

**Encrypt new CAD writes with a per-tenant AES-256-GCM key minted into the OS keychain; decrypt on read to a short-lived temp file the Python extractor ingests; emit a debounced read-audit on every fetch. No SPA surface, no storefront change, no auto-migration of existing plaintext.** Lives entirely in one new module, `apps/aberp/src/cad_blob.rs`.

### Cipher + on-disk format

- **AES-256-GCM** (RustCrypto `aes-gcm`), authenticated encryption so a flipped ciphertext bit fails decryption loudly (tamper-evident) rather than yielding garbage geometry.
- On-disk layout: `MAGIC(8 = "ABRPCAD1") || nonce(12, random per blob) || ciphertext+tag`. A fresh 96-bit nonce is minted per write (never reused under one key).
- The magic header is the **plaintext discriminator**: a reader checks the first 8 bytes. Present → authenticated-decrypt. Absent → the blob predates S430; pass the bytes through unchanged.

### Key management (mirrors the NAV-credentials keychain pattern)

- One key per tenant: keychain service `aberp.cad.<tenant>`, item `cad_blob_key`, value = base64(32 random bytes). Mirrors `nav-transport::credentials::keychain`'s `aberp.nav.<tenant>` shape.
- **First boot** (`serve`): if the item is absent, mint a fresh AES-256 key from the OS CSPRNG, store it, and emit `CadBlobKeyProvisioned`. If present, load it. A **malformed** stored key (wrong length / bad base64) is a loud error, **not** a silent re-mint — re-minting would orphan every blob the old key encrypted (rule 12).
- A provisioning failure folds into the pipeline's existing graceful "construction failed → AMBER, boot survives" arm. We **never** fall back to writing CAD blobs in the clear.

### Read path = decrypt-to-temp (forced by fact #2)

Because the Python extractor reads a path, `advance_extract` decrypts the on-disk blob into a sibling temp file (`._decrypted_<ulid>_<filename>`, keeping the original extension so STL-vs-STEP dispatch still works), hands *that* path to the extractor, and **deletes it on drop** (`DecryptedTempFile`). The plaintext never outlives the extraction. A decrypt failure (tamper / wrong key) is recorded as a **Failed** job with stage `decrypt` — the SPA already renders Failed rows with a red error chip + reason, so the customer-visible tamper signal needs **zero new SPA surface**.

### Read-audit (`[[trust-code-not-operator]]`)

Every fetch emits `CadBlobRead` carrying `{ blob_id, requester, purpose }`. `purpose` is a closed vocabulary — `Preview` / `Reprice` / `CustomerDownload` / `SystemRevalidate` — so the audit string set is stable and no payload migration is needed when a future fetch site lands. Reads from the **same requester for the same blob within 60s** are debounced (in-memory, process-lifetime) to avoid spam from rapid re-renders / retry loops. The read fires automatically inside the pipeline; an operator cannot choose to skip it. A read of a legacy (no-magic) blob additionally emits `CadBlobLegacyPlaintextRead` so a future migration sweep can find them.

### Storefront integration — **passthrough, no new surface** (brief §4)

The storefront validates uploads by **magic-bytes only** and keeps its own copy; it has no deep CAD storage that ABERP reads. ABERP is the only party that persists the blob for pricing. Therefore the encryption lives **entirely in ABERP** and the storefront stays an unmodified passthrough — the less-new-surface option the brief asked us to pick. No `CAD_BLOB_KEY` is shared with the storefront.

## Consequences

- **Positive:** new CAD blobs are encrypted at rest with authenticated encryption; every read is audited and tamper is customer-visible. One module, one new dependency (`aes-gcm`), no schema change (`[[no-sql-specific]]` — invariants in code), no SPA/storefront change.
- **Deferred — legacy plaintext migration.** Existing plaintext blobs are **not** re-encrypted in this session (explicit brief scope). They read correctly (passthrough) and each read emits `CadBlobLegacyPlaintextRead`, which is the worklist a future one-shot migration sweep will drain. We only **stop accumulating new plaintext**.
- **Flagged — unwired read purposes.** Only the `Reprice` fetch site exists in code today (the extract path). `Preview` / `CustomerDownload` have **no live endpoint** (fact #3); the `ReadPurpose` enum + the `cad_blob` read chokepoint are ready for them, and wiring lands when those endpoints are built. The audit vocabulary is complete now so adding a site later needs no payload change.
- **Accepted limitation — transient plaintext temp file.** Decryption necessarily writes plaintext to disk for the duration of one extraction (the Python subprocess needs a path). It is deleted on drop. A future OCCT-in-Rust extractor could read bytes directly and remove even this transient.
- **Key escrow / backup** of `cad_blob_key` is out of scope here (the broader ADR-0007 key-escrow item). Losing the key orphans encrypted blobs; the storefront still holds the original uploads, so re-pull is the recovery path.
