# ADR-0040 — Multi-bank-account `seller.toml` schema + per-currency-defaults closed-vocab + load-only legacy migration (PR-71, the schema slice of the PR-A/B/C/D multi-bank initiative)

- **Status:** Accepted
- **Date:** 2026-05-26
- **Deciders:** Ervin
- **Class:** Build-phase just-in-time ADR — pins the schema
  contract for the multi-bank-account initiative BEFORE the UI /
  issue-path / render PRs build against it. Mirrors the ADR-0037
  posture (pin the regulatory contract before the EUR code) and
  the ADR-0035 posture (pin the verifier invariants before the
  verifier code).
- **Related:**
  - **ADR-0037 §3 — `Currency` closed vocab.** PR-71's
    `[[seller.banks]]` `currency` field reuses the existing
    `{Huf, Eur}` closed enum verbatim. A future third variant
    (`Chf`, `Usd`) lands as a one-line enum addition per the
    ADR-0037 widening trigger; the `seller_banks` parser inherits
    the new variant automatically via `parse_currency` and the
    typed `UnsupportedCurrency` arm catches typos at the boundary.
  - **ADR-0038 — invoice-preflight validation.** The
    operator-facing error posture (Hungarian + English message
    pair, names the file path) inherited verbatim here for the
    `SellerBanksError::operator_message` surface.
  - **ADR-0039 — operational metadata orthogonal to the regulatory
    ladder.** The same posture applies: bank-account selection is
    operator metadata, not part of the `derive_state` ladder. PR-C
    will stamp `bank_account_id` onto the issued invoice without
    extending the typestate.
  - **CLAUDE.md rule 7 (surface conflicts, don't average them)** +
    **rule 12 (fail loud).** The per-currency-defaults invariant
    enforces a single "this is THE default for HUF" pick rather
    than letting the loader silently fall through to "first wins".
  - **`project_aberp_tenant_management` — multi-bank-account
    requirement (Sn-80 scope, 2026-05-25).** Ervin: *"Tenant can
    have multiple bank account for one currency and for the
    other like two for huf and 3 for EUR and banks as well so
    that one must be a list of objects."* The session-93 brief
    decomposes that scope into PR-A (schema, THIS PR) / PR-B
    (UI) / PR-C (issue path) / PR-D (render).

## Context

### What changed

Pre-PR-71, `seller.toml` carried at most ONE bank block — either
flat-root keys (`bank_account_number = "..."`, `bank_name = "..."`,
`swift_bic = "..."`, the shape `samples/seller.toml.example` ships
today and `setup_seller_info::parse_seller_bank` reads) or a
`[seller.bank]` single section. The printed-invoice PDF footer and
the SPA's `GET /api/seller-info` route both consume that single
slot.

The reality at Áben Consulting KFT. (and any tenant doing
cross-border invoicing on the back of ADR-0037's EUR lift) is N
banks per currency: typically one or more HUF accounts AND one or
more EUR accounts. The session-80 operator scope-injection named
this explicitly. The current single-bank slot forces the operator
to manually edit `seller.toml` between issuing a HUF invoice and
an EUR invoice — a defect-prone manual ritual that violates the
"no terminal ever required" north star pinned in
`project_aberp_tenant_management`.

### Why a schema-only first PR

The session-93 brief decomposes the multi-bank initiative into
four PRs so each lands as a reviewable surgical change:

- **PR-A (PR-71) — schema.** Widens `seller.toml` to
  `[[seller.banks]]` array-of-tables + adds the typed read-side
  + validator + helper accessors. **THIS ADR.**
- **PR-B — UI.** Tenant Settings page Bank-accounts subsection +
  SetupWizard multi-row inputs. Persists the new form to disk.
- **PR-C — issue path.** Stamps `bank_account_id` onto the issued
  invoice; operator picks a non-default via a dropdown on the
  issue form.
- **PR-D — render.** NAV XML body's supplier-bank fields + printed-
  PDF footer both consume the per-invoice bank-account snapshot
  rather than "the current first one in seller.toml".

PR-A lands first because the rest build against its types. The
schema contract pinned here does not drift between PRs.

### Prerequisite-gate state at PR-71 time

- `apps/aberp/src/setup_seller_info.rs::SellerBank` carries a
  single-bank `{account_number, iban, name, swift_bic}` shape;
  `parse_seller_bank` is line-oriented and ignores `[section]`
  headers entirely.
- `apps/aberp/src/serve.rs::SellerInfoBank` (the wire shape for
  `GET /api/seller-info`) mirrors the same single-bank shape;
  the SPA's Tenant Settings page renders it as four `<input>`
  fields.
- `apps/aberp/src/print_invoice.rs::SellerToml` reads the same
  single-bank slot for the PDF footer render.
- `aberp_billing::Currency` is the closed `{Huf, Eur}` enum
  per ADR-0037 §3; serde rename UPPERCASE; `iso_code` accessor.

PR-71 leaves all three pre-existing surfaces UNCHANGED — they
continue to read the flat-root form via the existing parser. PR-B
swaps the SPA wire shape; PR-D swaps the PDF renderer. The
non-destructive load-only migration (§2 below) keeps every
already-deployed `seller.toml` working without operator
intervention until PR-B persists the new form.

### Surfaced conflicts (CLAUDE.md rule 7)

**Conflict 1 — default-marker semantics: explicit `default = true`
flag vs. "first entry wins" vs. "primary key on currency".**
**Reading A:** explicit `default = true` flag on each entry, with
a validator that requires exactly one per currency. **Reading B:**
"first declared entry for each currency is the default" implicit
rule. **Reading C:** force a separate top-level
`default_bank_huf = "<id>"` / `default_bank_eur = "<id>"` map.
**Decision below picks Reading A** — the explicit flag is
self-describing in the file (an operator opening `seller.toml`
sees which one is default without consulting the loader's
ordering rules); Reading B silently makes file-edit order
load-bearing, which a future operator re-ordering entries for
readability would break; Reading C duplicates state (now the
default lives in two places, the entry and the top-level map,
and they can drift). The validator's loud-fail on multiple- or
zero-defaults per currency is the forcing function that keeps
Reading A safe per CLAUDE.md rule 12.

**Conflict 2 — `currency` field: required vs. inferred at load.**
**Reading A:** required at the new-form `[[seller.banks]]` entry;
explicit value in the file; loader rejects entries without it.
**Reading B:** infer from the SWIFT/BIC country code on every
entry; the `currency` field becomes optional / redundant.
**Decision below picks Reading A** for new-form entries — explicit
beats inferred when the operator can type it once and the
inference is a heuristic (a Hungarian bank like Erste can hold
EUR accounts too, so SWIFT-country-code → currency is wrong in
the general case). **Reading B is preserved ONLY for the legacy
migration path** (§2 below) where no `currency` field exists in
the pre-PR-71 schema — there the SWIFT inference is the only
non-arbitrary fallback, and the loud `tracing::warn!` directs
the operator to confirm in the UI via PR-B's settings page.

**Conflict 3 — bank ID: stable across loads vs. random per load.**
**Reading A:** deterministic `bnk_<26-char>` derived from
`SHA-256(currency_iso || ":" || account_number)`. **Reading B:**
random ULID at first load + persistence of the assignment back
to the TOML. **Reading C:** no ID at all — PR-C stores a
denormalized snapshot (`bank_name + account_number + swift_bic`)
on the issued invoice.
**Decision below picks Reading A** — Reading B requires PR-71 to
WRITE to `seller.toml` on first load (breaks the non-destructive
property + creates a race between concurrent loads); Reading C
makes "rename my bank from `Erste Bank` to `Erste Bank Hungary`"
silently break the rename target on past invoices. Reading A's
determinism means every load produces the same id for the same
`(currency, account_number)` pair, the file is never mutated by
the loader, and PR-C's stamp is a stable reference that survives
operator edits to non-identifying fields (`bank_name`,
`swift_bic` rotation).

**Conflict 4 — "at least one entry per used currency" enforcement
timing: load vs. use.** **Reading A:** enforce at load — a
`seller.toml` with zero EUR entries rejects boot if the tenant
might issue EUR invoices. **Reading B:** enforce at use — load
accepts any non-empty configuration; PR-C's issue-path picker
loud-fails at issue time if no entry exists for the invoice's
currency.
**Decision below picks Reading B** — the load-time check would
need to know whether the tenant will ever issue EUR invoices,
which the loader cannot determine (a fresh tenant has zero
issued invoices). Reading B keeps the load contract minimal
("file parses, validators pass") and pushes the "no bank for
currency X" error to the surface that knows X (the issue path).
Reading A would also break the cold-boot flow for a brand-new
tenant whose first invoice happens to be HUF.

## Decision

### 1. Schema shape

The new canonical form (per CLAUDE.md rule 7, the ONE shape we
pick — Reading A on Conflict 1, Reading A on Conflict 2 for
new-form entries):

```toml
[[seller.banks]]
currency       = "HUF"
account_number = "12345678-12345678-12345678"
bank_name      = "Erste Bank"
swift_bic      = "GIBAHUHB"
default        = true

[[seller.banks]]
currency       = "EUR"
account_number = "HU12-3456-7890-1234-5678-9012-3456"
bank_name      = "Erste Bank"
swift_bic      = "GIBAHUHB"
default        = true
```

Required fields per entry (loud-fail at load via
`SellerBanksError::MissingField` keyed by zero-based entry
index): `currency`, `account_number`, `bank_name`, `swift_bic`.
`default` is bool; absent → `false`.

Field types (the typed `SellerBankEntry` shape):
- `id: String` — deterministic `bnk_<26-char>` per §3.
- `currency: Currency` — closed-vocab per ADR-0037 §3.
- `account_number: String` — verbatim; IBAN for EUR / domestic
  format for HUF.
- `bank_name: String` — operator-typed.
- `swift_bic: String` — 8 or 11 chars; positions 4-5
  (zero-indexed) are the ISO 3166-1 alpha-2 country code.
- `default: bool` — exactly one `true` per currency per §2 below.

### 2. Per-currency-defaults closed-vocab invariant + load-only legacy migration

**Per-currency-defaults invariant.** For each `Currency` value
that has at least one `[[seller.banks]]` entry, EXACTLY ONE entry
MUST carry `default = true`. The validator surfaces:
- `SellerBanksError::MultipleDefaults { currency, count }` when
  two or more entries for the same currency are marked default.
- `SellerBanksError::NoDefaultAmongEntries { currency }` when one
  or more entries exist for a currency but none is marked default.
- `SellerBanksError::UnsupportedCurrency { entry_index, value }`
  when an entry's `currency` value is outside the ADR-0037 closed
  vocab.
- `SellerBanksError::MissingField { entry_index, field }` when a
  required field is absent from an entry.

Each error variant carries a bilingual (Hungarian + English)
operator-visible message that names the file path + the offending
currency / field, per the ADR-0038 posture inherited here.

**Load-only legacy migration.** Two pre-PR-71 shapes are folded
transparently on load:

1. **Flat-root form** — `bank_account_number = "..."`,
   `bank_name = "..."`, `swift_bic = "..."` at the file root with
   no section header (the shape `samples/seller.toml.example`
   ships today).
2. **`[seller.bank]` single-section form** — a `[seller.bank]`
   heading followed by the same fields without a section
   wrapper.

Both fold to a single-element `[[seller.banks]]` array with:
- `default = true` (there is exactly one entry, so it IS the
  default per §2 invariant).
- `currency` inferred from the SWIFT/BIC country code (positions
  4-5 zero-indexed): `HU` → `Currency::Huf` with a mild
  `tracing::warn!` ("inferred HUF; persist via Tenant Settings to
  silence"); anything else → `Currency::Huf` (the conservative
  fallback) with a louder `tracing::warn!` ("the SWIFT/BIC
  country code is not HU; defaulted to HUF — open Tenant Settings
  → Bank accounts and confirm").

**Non-destructive migration.** The migration runs at LOAD ONLY.
The on-disk `seller.toml` is NOT rewritten. Persisting the
migrated form is an operator action via PR-B's UI write path;
PR-71 itself never touches the filesystem write surface. This
keeps PR-A a non-destructive schema lift — a tenant whose
`seller.toml` was last edited by hand keeps that file
byte-identical until they re-open Tenant Settings.

### 3. Stable bank IDs (deterministic from currency + account_number)

Each loaded entry is assigned `bnk_<26-char>` where the 26
characters are the Crockford-base32 (ULID) rendering of the first
16 bytes of:

```
SHA-256(currency_iso_code || ":" || account_number)
```

Per Conflict 3 Reading A above: deterministic across load cycles
(restarting the binary produces the same id), the file is never
rewritten by the loader, and the (currency, account_number) pair
is the natural unit of identity (two HUF accounts in the same
bank with the same account number are the same account; an EUR
account and a HUF account at the same bank with the same domestic-
form account number are NOT the same — the currency salts the
hash so they get distinct ids).

PR-C stamps the id onto the issued invoice; PR-D resolves it
back via `SellerBanks::bank_by_id` to populate the NAV body's
supplier-bank fields and the PDF footer.

### 4. Helper accessors on `SellerBanks`

The loaded collection wraps the entry vector and exposes the
three accessors PR-B / PR-C / PR-D will call. The accessors live
on the type (not the call site) so currency-defaulting logic
doesn't scatter:

- `entries(&self) -> &[SellerBankEntry]` — declaration order;
  PR-B's settings list reads this.
- `default_bank_for(&self, currency: Currency) -> Option<&SellerBankEntry>`
  — returns the unique `default = true` entry for `currency` (per
  §2 invariant the validator guarantees AT MOST one); returns
  `None` if no entries exist for `currency`.
- `banks_for_currency(&self, currency: Currency) -> Vec<&SellerBankEntry>`
  — all entries for the currency in declaration order; PR-B's
  dropdown + PR-C's per-invoice picker dropdown populate from
  this.
- `bank_by_id(&self, id: &str) -> Option<&SellerBankEntry>` — the
  reverse lookup PR-C uses when reading the stamped id off an
  issued invoice and PR-D uses when resolving the entry to emit
  on the NAV body + PDF footer.

### 5. Compliance invariants the PR-B/C/D code PRs MUST satisfy

| # | Invariant | Owner PR | Test posture |
|---|---|---|---|
| A1 | Schema parses the new `[[seller.banks]]` form + folds both legacy shapes (flat-root + `[seller.bank]`) on load without rewriting the file. | PR-71 (this PR) | Unit tests `migrates_legacy_flat_root_form` + `migrates_legacy_seller_bank_single_section` + integration test `migrates_legacy_flat_root_file_from_disk` (asserts file is byte-identical pre/post load). |
| A2 | Per-currency-defaults invariant: zero or multiple `default = true` for the same currency loud-fail at load. | PR-71 | Unit tests `rejects_multiple_defaults_for_same_currency` + `rejects_zero_defaults_among_entries_of_same_currency` + integration `multiple_defaults_in_file_loud_fails_via_disk_reader`. |
| A3 | Bank ids are deterministic across load cycles for the same `(currency, account_number)` pair. | PR-71 | Unit test `bank_id_is_deterministic_across_load_cycles` + integration `deterministic_id_pin`. |
| A4 | `default_bank_for(currency)` returns the unique default; returns `None` when no entries exist for that currency. | PR-71 | Unit tests `banks_for_currency_preserves_declaration_order` + `default_bank_for_returns_none_when_no_entries_for_currency`. |
| B1 | `currency` value outside the ADR-0037 closed vocab loud-fails at load. | PR-71 | Unit test `rejects_currency_outside_closed_vocab`. |
| B2 | Operator-facing error messages are bilingual (Hungarian + English) and name the file path. | PR-71 | Unit test `operator_message_is_bilingual_and_names_path`. |
| C1 | SetupWizard + Tenant Settings persist the new `[[seller.banks]]` form. The legacy shapes are not re-emitted. | PR-B (UI) | Future PR-B integration test pinning the on-disk shape after a wizard save. |
| C2 | Issue path stamps `bank_account_id` onto the issued invoice. Operator can pick a non-default via dropdown. | PR-C (issue path) | Future PR-C unit test pinning the stamp + dropdown contract. |
| C3 | NAV body's supplier-bank fields + PDF footer consume the per-invoice snapshot via `bank_by_id`, NOT "first entry in seller.toml". | PR-D (render) | Future PR-D golden-XML test + PDF-text-extraction test. |
| C4 | Issue path loud-fails when no `[[seller.banks]]` entry exists for the invoice's currency (the §Conflict 4 Reading B "enforce at use" rule). | PR-C | Future PR-C unit test on the picker — invoice in EUR + no EUR entry → loud-fail `NoBankForCurrency`. |

Invariants A1–A4 + B1–B2 are pinned by PR-71's test surface; C1–C4
are named here so PR-B/C/D's session briefs carry them as
concrete acceptance criteria.

### 6. Out-of-scope for PR-71

Deferred-items list with named PR-B/C/D ownership (per the
session-93 brief's scope discipline):

| Item | Owner |
|---|---|
| Tenant Settings page Bank-accounts list view + Add/Edit/Delete/Set-as-default buttons | PR-B (UI) |
| SetupWizard multi-row bank-account inputs | PR-B (UI) |
| HTTP routes `GET/POST/PUT/DELETE /api/seller-banks` | PR-B (UI) |
| Per-invoice `bank_account_id` stamping on the issued-invoice record (DuckDB column or denormalized snapshot — picked at PR-C time) | PR-C (issue path) |
| Issue-form bank-account dropdown defaulted to `default_bank_for(invoice.currency)` | PR-C (issue path) |
| Chain-child (storno / modification) bank-account picker — likely "inherit from base by default, allow override" | PR-C (issue path) |
| NAV `Online Számla` body's supplier-bank fields consume the per-invoice snapshot | PR-D (render) |
| Printed-PDF footer consumes the per-invoice snapshot | PR-D (render) |
| Swap the existing `setup_seller_info::SellerBank` / `serve.rs::SellerInfoBank` / `print_invoice.rs::SellerToml` to the new types | PR-B (the wire swap is part of the UI lift) + PR-D (the PDF swap) |
| Persist the migrated legacy form to disk on first load | NOT in scope; operator-driven via PR-B's UI write path per §2 non-destructive migration |
| Third currency variant on the `Currency` enum | Inherits ADR-0037 §3 widening trigger; not a PR-71 scope |

## Consequences

**What gets easier.**
- **Multiple banks per currency is now a typed first-class concept.**
  PR-C's issue-path picker no longer has to special-case "single
  bank from the file"; the picker reads
  `banks_for_currency(invoice.currency)` and surfaces the list.
- **Bank rename / SWIFT rotation is non-destructive to past invoices.**
  PR-C stamps `bnk_<id>` (deterministic over currency +
  account_number) onto the issued invoice; future renames of
  `bank_name` resolve cleanly via `bank_by_id` without
  invalidating the stamp.
- **The per-currency-defaults invariant is enforced at the load
  boundary.** A regression that introduces "first wins" silent
  defaulting would surface immediately as a loud-fail rather than
  as a wrong bank account on a future invoice.
- **Legacy `seller.toml` files keep working.** The non-destructive
  load-only migration means every already-deployed tenant
  continues to boot without operator intervention. The migration
  warn message is the only operator-visible change at PR-71 land
  time.
- **PR-B/C/D have a hard contract.** The schema, validator, and
  helper accessors are pinned here; PR-B/C/D build against them
  rather than guessing the shape mid-implementation.

**What gets harder.**
- **The per-currency-defaults invariant is now a hard load
  precondition.** A tenant whose operator manually edits
  `seller.toml` and accidentally marks two HUF entries
  `default = true` will see boot fail with a typed error. The
  loud-fail is the intended behaviour (silent first-wins would
  hide the misconfiguration); the bilingual error message names
  the file path + the currency so the fix is obvious.
- **The SWIFT-inference fallback is a heuristic.** A non-HU SWIFT
  on a legacy file defaults to HUF + a louder warn; the operator
  must open Tenant Settings (PR-B) and confirm. Until PR-B lands,
  the only correction path for a misinferred currency is manually
  upgrading `seller.toml` to the new `[[seller.banks]]` form. The
  warn message names the path explicitly.
- **Bank id determinism couples the id to the account_number
  string.** Operator edits to `account_number` (typo fix, format
  change) produce a NEW id, which orphans the stamp on any
  already-issued invoice. PR-C's stamp posture is the
  operator-twin pick that survives this — PR-C will store both
  the id AND a denormalized snapshot (per §6's named pick at
  PR-C time) so the printed invoice + NAV body remain correct
  for past invoices even after the id drifts. Naming this trade
  here so PR-C inherits the constraint.

**What we lock ourselves into.**
- **Reading A on §Conflict 1 (explicit `default = true`).**
  Switching to "first declared entry wins" silent defaulting is
  an ADR-amendment scope. Until then, the validator enforces
  exactly one per currency.
- **Reading A on §Conflict 2 (explicit `currency` required on
  new-form entries).** Making `currency` optional is an
  ADR-amendment scope. SWIFT inference applies ONLY to legacy
  entries during the migration window.
- **Reading A on §Conflict 3 (deterministic bank id).** Switching
  to random+persisted ids is an ADR-amendment scope. Until then,
  the loader is non-destructive (never writes the file).
- **Reading B on §Conflict 4 (enforce ≥1 entry per used currency
  at USE time, not load time).** Adding a load-time enforcement
  would require the loader to know which currencies the tenant
  uses, which is a separate piece of configuration — out of
  scope.

## Adversarial review

1. **"The SWIFT-inference fallback is wrong — a Hungarian bank
   can hold EUR accounts."** Accepted; the inference applies ONLY
   to the legacy migration path where no `currency` field exists.
   New-form entries REQUIRE explicit `currency`. The fallback
   defaults to HUF (the conservative pick — the legacy single-bank
   file's tenant was almost certainly HUF-only) AND emits a
   `tracing::warn!` directing the operator to confirm via PR-B's
   UI. The integration test
   `swift_inference_pins_country_code_position` pins both the
   happy-path (HU → HUF) and the fallback (non-HU → HUF + flag).
2. **"The bank id determinism breaks when an operator fixes a
   typo in `account_number`."** Accepted; named in the
   Consequences "What gets harder" section. PR-C's stamp posture
   stores both the id AND a denormalized snapshot so past
   invoices remain byte-identical at the PDF + NAV body even
   after the id drifts. Future ADR if the trade surfaces a
   pain point in operations.
3. **"Per-currency-defaults invariant is too strict — what if an
   operator deliberately wants two HUF defaults to indicate 'use
   either one'?"** Rejected. Two defaults means "no default" in
   practice (the issue-path picker would have to pick one, and
   any rule for picking is a hidden first-wins / random / hash
   default that violates CLAUDE.md rule 12). The operator who
   wants "two acceptable HUF accounts" gets two entries with
   exactly one marked default; PR-C's picker dropdown lets the
   operator override per invoice. The data shape supports the
   intent without the silent-default trap.
4. **"Non-destructive load-only migration leaves the file in a
   stale form indefinitely."** Defensible. Until the operator
   opens PR-B's Tenant Settings, the legacy form is read
   transparently on every load + warns once on each boot. The
   warn is structured (`tracing::warn!` with `swift_bic` field)
   so the operator can grep for it in launcher logs. PR-B's
   "Save bank account" action persists the new form and silences
   the warn permanently.
5. **"`seller.toml` is now load-bearing for the issue path; a
   load-time parse failure blocks issuing any invoice."**
   Accepted. The pre-PR-71 single-bank shape had the same
   property (a broken `bank_account_number` line broke the PDF
   footer). PR-71's typed parser + bilingual error message
   makes the diagnose-and-fix loop shorter, not longer.
6. **"You picked a custom line-oriented parser over the `toml`
   crate. That's fragile."** Defensible. The accepted grammar is
   constrained (a small fixed key set + section headers +
   `[[array.of.tables]]` headers + bool `default` + string
   values). The custom walker mirrors the existing
   `setup_seller_info::parse_seller_bank` /
   `parse_seller_identity` posture and avoids adding a direct
   `toml` crate dependency for what is read-only at PR-71 (PR-B
   may revisit at the write-path lift). The walker is covered by
   17 unit pins + 8 integration pins; a regression would surface
   immediately. If PR-B's write path makes the trade flip, the
   loader can be migrated to `toml` crate parsing without
   touching the public type / accessor surface.
7. **"`bnk_<26-char>` looks like a ULID but isn't — it has no
   time prefix."** Accepted. The format is "Crockford-base32 of
   a 128-bit value", which IS the ULID rendering format; the
   "no time prefix" property is intentional (determinism over
   load cycles is the load-bearing invariant per §3, and a time
   prefix would defeat it). Calling the format "ULID-shaped"
   rather than "a ULID" is a documentation precision the
   doc-comment carries explicitly.
8. **"You only enforce 'one default per currency' at load time;
   PR-B could persist a file that fails this and then the next
   boot fails."** Defensible — PR-B's write path will run the
   same validator before writing, so any wizard submit that
   violates the invariant rejects at the route. Naming the
   shared-validator dependency here so PR-B's session brief
   picks it up.

## Alternatives considered

- **A1 — Defer the schema lift; let PR-B introduce the new shape
  alongside the UI.** Rejected. PR-B's UI work is ~200-400 LoC of
  Svelte + route handlers; bundling the schema contract with it
  would make the contract subordinate to the UI rather than the
  other way around. The pinned-contract-first posture matches
  ADR-0037 (regulatory pin before EUR code) + ADR-0035 (verifier
  pin before verifier code) + ADR-0039 (operational-metadata pin
  before mark-paid code).
- **A2 — Random per-load bank IDs + auto-persist back to
  `seller.toml`.** Rejected per §Conflict 3 — the auto-persist
  breaks the non-destructive load-only migration property and
  creates a race between concurrent loads.
- **A3 — `Currency` as an open ISO 4217 string + accept whatever
  the operator types.** Rejected. ADR-0037 §3's closed-vocab
  posture applies here; the parser refuses anything outside
  `{HUF, EUR}` so a typo (`currency = "USD"`) surfaces
  immediately rather than at the issuance path.
- **A4 — Reach for the `toml` crate.** Rejected at PR-71 time
  per Adversarial-review item 6. The custom walker is the
  smaller surgical change; revisitable at PR-B's write-path
  lift.

## Open questions

1. **PR-B's write path should re-emit the migrated entry with
   the inferred currency, or leave the file in the legacy form
   until the operator explicitly saves a multi-bank
   configuration?** Default per §2 non-destructive migration:
   leave the file alone until an operator action triggers a
   write. PR-B's wizard "Save bank accounts" button is the
   trigger.
2. **Should `bank_account_id` on the issued-invoice record store
   the id alone, or the id + a denormalized snapshot
   (`bank_name`, `account_number`, `swift_bic` at issue time)?**
   PR-C decision. The operator-twin posture inherited from
   ADR-0036 / partner records suggests the snapshot is the safer
   pick (past invoices remain byte-identical even after the id
   drifts).
3. **Cross-tenant bank-account sharing.** Out of scope for the
   PR-A/B/C/D arc; each tenant's `seller.toml` is the boundary.
   A future ADR if the operator surfaces a "share a bank across
   tenants" need.
4. **Bank-account lifecycle (close / archive a bank without
   deleting past references).** PR-C question. The current shape
   has no "archived" flag; PR-C may add one if past-invoice
   stamps need to outlive the bank's active-use window.
5. **Validator's posture on a file with ZERO `[[seller.banks]]`
   entries.** Currently accepted as "no banks configured yet"
   (returns an empty `SellerBanks`). PR-C decides whether the
   issue path treats this as "no defaults available, picker is
   empty" vs. a separate boot-state. The PR-71 default is the
   permissive read; the issue-path picker is the surface that
   loud-fails on use.

## Follow-on PRs unblocked by this decision

- **PR-B (UI)** — Tenant Settings Bank-accounts subsection +
  SetupWizard multi-row inputs + `GET/POST/PUT/DELETE
  /api/seller-banks` routes. Consumes the typed `SellerBanks` +
  `SellerBankEntry` shapes pinned here.
- **PR-C (issue path)** — Bank-account picker on the issue form
  defaulted to `default_bank_for(invoice.currency)`; stamps
  `bank_account_id` (the `bnk_<26-char>` value) onto the issued
  invoice; loud-fails when no entry exists for the invoice's
  currency.
- **PR-D (render)** — NAV `Online Számla` body's supplier-bank
  fields + printed-PDF footer consume the per-invoice snapshot
  resolved via `SellerBanks::bank_by_id`.

The session-93 close handoff carries PR-B / PR-C / PR-D as the
named PR-71+ candidate space; subsequent sessions pick from that
list per the loop-window cadence.

---

## §addendum — PR-72 / session-94 (PR-B: write path + routes + SPA)

The PR-71 ADR §6 reserved "PR-B / PR-C / PR-D" as named follow-on
work. PR-72 lands the PR-B slice (write path + HTTP routes + SPA
surfaces). This §addendum extends the ADR's closed-vocab register
with the route-layer error enum + records the route shape so PR-C
+ PR-D do not redesign them.

### §addendum.1 — Route surface

Five routes over the per-tenant `~/.aberp/<tenant>/seller.toml`
`[[seller.banks]]` block, all Ready-gated + bearer-required:

| Method | Path                                       | Body                                                          | Success                                | Failure  |
|--------|--------------------------------------------|---------------------------------------------------------------|----------------------------------------|----------|
| GET    | `/api/seller/banks`                        | —                                                             | `200 {banks: SellerBankResponse[]}`    | 401 / 503 |
| POST   | `/api/seller/banks`                        | `SellerBankInputs` (incl. `set_as_default`)                   | `201 {banks: SellerBankResponse[]}`    | 400 / 401 / 503 |
| PUT    | `/api/seller/banks/:id`                    | `SellerBankInputs` (NO `set_as_default`; preserves prior flag)| `200 {banks: SellerBankResponse[]}`    | 400 / 404 / 401 |
| POST   | `/api/seller/banks/:id/set-default`        | —                                                             | `200 {banks: SellerBankResponse[]}`    | 404 / 401 |
| DELETE | `/api/seller/banks/:id`                    | —                                                             | `200 {banks: SellerBankResponse[]}`    | 404 / 409 / 401 |

Every mutation returns the full updated collection so the SPA
re-renders from one source of truth without a second GET. Set-
default is a separate route from PUT to keep the mental model
crisp (mutating the default is a separate intent from editing the
entry's other fields).

### §addendum.2 — `SellerBankRouteError` closed-vocab

```rust
pub enum SellerBankRouteError {
    Validation(Vec<SellerBankFieldError>),  // 400 — typed per-field
    NotFound,                                // 404
    Conflict { message: String },            // 409 — bilingual
    Other(anyhow::Error),                    // 500 — sanitised
}
```

Adding a new failure mode means adding a variant + a 4xx/5xx
mapping, NOT a free-text string. Mirrors the ADR-0038 preflight-
error shape + the PR-48α `PartnerRouteError`.

### §addendum.3 — Pre-write validation discipline

Every mutation route follows the same shape:

1. Read the current `SellerBanks` (legacy migration applies
   transparently per PR-71 §2).
2. Apply the operator's mutation to a `Vec<SellerBankEntry>` copy.
3. Run `SellerBanks::replace_entries(...)` — this re-runs the
   shared per-currency-default validator (`validate_per_currency_defaults`).
   The same validator is the read path's gate (per §3-A2).
4. `write_seller_banks_section(path, &banks)` does the POSIX-
   atomic merge that **preserves the identity block** + replaces
   only the `[[seller.banks]]` (or legacy flat-root / legacy
   `[seller.bank]`) bytes. Mirrors `setup_seller_info::write_atomic`'s
   tempfile + fsync + rename + 0600 pattern.

Validation runs BEFORE the atomic write — a broken file never
hits disk. PR-D inherits the same guarantee at the issue path.

### §addendum.4 — Set-default + auto-promote semantics

The route layer enforces two implicit-default rules that are not
in the read-path validator:

1. **First-entry auto-default**: an operator who creates the
   first entry for an unrepresented currency without ticking
   `set_as_default` gets it marked default anyway (the per-
   currency-default invariant requires at least one default
   per used currency; defaulting it silently is the simpler
   operator UX than rejecting the submission).
2. **Promote-next on delete-of-default**: deleting the marked
   default for a currency that still has other entries promotes
   the next remaining entry (declaration order) to default. The
   invariant remains satisfied without a second operator action.

### §addendum.5 — 409 Conflict rule

DELETE refuses (409) when the entry is the ONLY one for its
currency AND there are entries with a different currency. The
brief's exact wording. Rationale: a delete that would leave HUF
with zero entries + EUR with N entries would break PR-C's issue-
path picker for HUF invoices. The validator at load would surface
this implicitly (zero defaults among presence-zero), but
surfacing it pre-write gives the operator a clearer message + an
on-disk file that did NOT mutate.

The "delete every entry" case is allowed (zero HUF + zero EUR is
valid per §2 — PR-71's "empty bodies return empty collection"
pin). The conflict only fires for asymmetric-currency states.

### §addendum.6 — Deterministic id stability

`mint_entry(currency, account_number, ...)` derives the same
`bnk_<26-char>` id as the read-path parser for the same
`(currency, account_number)` pair (pinned by `mint_entry_id_matches_parser_id`).
PR-C's stamped references therefore survive a delete + re-add of
the same `(currency, account_number)` — the id is content-derived,
not row-generation-derived.

The PUT path re-mints the id over the new `(currency,
account_number)`; an operator's typo correction on either field
moves the stable id with the corrected content. A PUT that would
collide with a different existing entry's id refuses (validation
error keyed by `accountNumber`).

### §addendum.7 — SetupWizard chained writes

The wizard fires a two-phase write at "Save & continue":

1. POST `/api/setup-seller-info` with the legacy bank fields
   blank (the wizard's own form no longer carries them; the new
   multi-row block owns them).
2. Sequentially POST each multi-row bank entry to
   `/api/seller/banks`.

Phase 1 flips the boot state to Ready. If phase 2's per-row POST
fails mid-sequence, the operator lands in the normal app with
the partial bank state; they fix it via Tenant Settings → Bank
accounts. The inline-error renderer shows per-row 400 messages.

Rationale: the alternative (rollback identity on bank failure)
would require a multi-route transaction the backend does not
have. The operator-twin posture survives partial wizard failure
without losing identity work.

### §addendum.8 — Decisions deferred to PR-C / PR-D

Unchanged from PR-71 ADR §6. PR-72's route surface does NOT
preclude any of the open questions; in particular, the
`bank_account_id` stamped on issued invoices is opaque to the
routes, so PR-C's choice between "id alone" vs. "id +
denormalized snapshot" is unaffected.

---

## §addendum-C (PR-73 / session-95) — issue-path bank picker

PR-73 lands the issue-path slice of the multi-bank-account
initiative: an `IssueInvoiceRequest.bankAccountId: Option<String>`
wire field, a route resolver that maps `(bank_account_id | None)`
to a typed snapshot, two new `InvoicePreflightError` variants,
a DuckDB-column quintet for the per-invoice denormalized
snapshot, audit-payload extension, list + detail wire fields,
the SPA bank-picker UI, and chain inheritance.

### §addendum-C.1 — Denormalization decision (PR-71 §6.2 closed)

PR-71's §6.2 open question between "id alone" vs. "id +
denormalized snapshot" closes with **denormalized snapshot**
(per the session-95 brief's explicit operator-twin-survivor
mandate — if the operator later edits or deletes a bank
account, the historical invoice still renders the bank account
it was issued with).

Five new DuckDB columns on `invoice` (all VARCHAR, all nullable
per the DuckDB v1 ALTER TABLE constraint trap PR-44γ documented):

| Column                  | Source on typed `BankAccountSnapshot` |
|-------------------------|-----|
| `bank_account_id`       | `id` (the `bnk_<26-char>` deterministic value) |
| `bank_account_currency` | `currency.iso_code()` |
| `bank_account_number`   | `account_number` |
| `bank_account_bank_name`| `bank_name` |
| `bank_account_swift_bic`| `swift_bic` |

Read posture: NULL across all five → "no bank account on file"
(pre-PR-73 / CLI-issued rows). The
`InvoiceBankSnapshot::into_typed()` helper requires all five
populated; a partial state surfaces as `None` (defensive against
ledger tampering).

Migration: `MIGRATE_PR_73_SQL` adds the five columns with
`ALTER TABLE ADD COLUMN IF NOT EXISTS`. No `UPDATE` backfill —
fabricating a snapshot from current `seller.toml` for pre-PR-73
rows would corrupt the regulatory record (a different default
than the one in effect at issuance time). Pre-PR-73 rows stay
NULL; PR-D's render falls back to the legacy flat-root bank
slot for those rows.

### §addendum-C.2 — New preflight variants (ADR-0038 §4 widening)

Two additions to `InvoicePreflightError` per the closed-vocab
discipline ADR-0038 §4 names. Field path: `bankAccountId`
(closed-vocab camelCase) for both:

| Variant | Trigger | Operator action |
|---|---|---|
| `SellerBankMissingForCurrency { currency }` | `bank_account_id = None` AND `default_bank_for(currency)` returns `None` | Add a `[[seller.banks]]` entry for that currency in Tenant Settings |
| `SellerBankCurrencyMismatch { selected_id, selected_currency, invoice_currency }` | `bank_account_id = Some(id)` AND the looked-up entry's currency differs from the invoice's currency | Pick a different bank account |

Both carry bilingual Hungarian + English messages per ADR-0038
§3 + ADR-0040 §3-B2. Both surface in the same typed
`invoice_preflight_failed` 400 body the SPA's
`parseInvoicePreflightErrors` consumes; the SPA's inline-error
renderer targets the `bankAccountId` form input via the
existing `targetForFieldPath` router (extended with a new
`bankAccountId` arm).

### §addendum-C.3 — 404 case (defence in depth)

`bank_account_id = Some(id)` where the id does NOT exist in
the current `seller.toml` surfaces as `404 Not Found` (NOT a
preflight 400). The SPA's bank picker is populated from
`GET /api/seller/banks`, so the SPA never POSTs a stale id
under correct operation; the 404 fires only on a curl bypass
or a stale dropdown click (the operator opened the picker,
the operator deleted the entry in another tab, the operator
submitted). The SPA renders the typed 404 body's `error`
field; no inline routing.

### §addendum-C.4 — Chain inheritance rule

Storno + modification chain children **inherit** the base
invoice's bank-account snapshot verbatim. The chain-issuance
code reads the base's snapshot via
`load_invoice_bank_snapshot_in_tx` (sibling of the existing
`load_invoice_currency_metadata_in_tx`) and stamps it onto the
chain child's `AllocateArgs.bank_snapshot`.

Rationale: the regulatory record is "the bank account the base
invoice asked to be paid to"; re-resolving against the
operator's current `seller.toml` could surface a different
account if the operator rotated the per-currency default
between base issuance and chain issuance. Same posture as the
PR-44γ.1 rate-metadata inheritance.

A `None` snapshot (pre-PR-73 base) propagates as `None` — the
chain child has no snapshot either, matching the base's render.
The brief's "If you discover this is structurally tricky, note
in handoff" trigger did NOT fire: the existing chain-
inheritance posture from PR-44γ.1 generalised cleanly.

### §addendum-C.5 — Wire shapes (new fields)

`IssueInvoiceRequest` gains `bankAccountId: string | null`
(optional, defaults to `null`). The resolver normalises
empty-string to `null`.

`InvoiceListItem` + `InvoiceDetailResponse` gain `bank_account:
BankAccountSnapshot | null` (a new TS interface mirroring
`SellerBankResponse` minus the `is_default` flag — per-invoice
snapshots are immutable post-issuance).

`InvoiceDraftCreatedPayload` gains the five `bank_account_*`
fields per the additive-payload posture PR-18 + PR-44γ
established. Pre-PR-73 entries deserialise transparently with
all five `None` (per `#[serde(default)]`). The new
`with_bank_snapshot(...)` builder method stamps the snapshot
onto an existing payload — used by all three issue-paths
after their `from_invoice_with_*` constructor call.

### §addendum-C.6 — SPA bank-picker design

`IssueInvoice.svelte` lazy-loads `listSellerBanks()` on first
modal open. The picker dropdown filters by current
`form.currency`; an `$effect` re-defaults the selection
whenever currency changes:

- `defaultBankForCurrency` (derived) — the entry with
  `is_default: true` for the current currency, or `null`.
- On currency change: if a default exists, pre-populate
  `form.bankAccountId`; otherwise blank it.
- No-default-for-currency state renders an inline error +
  a link-to-Tenant-Settings affordance.

### §addendum-C.7 — Decisions deferred to PR-D

- **NAV XML body** consumes the per-invoice snapshot via the
  same `load_invoice_bank_snapshot_in_tx` helper. The fallback
  for `bank_snapshot = None` (pre-PR-73 / CLI-issued rows) is
  the legacy flat-root bank slot from `seller.toml`.
- **Printed-PDF render** same pattern.
- **Operator-side "edit snapshot on issued invoice"** stays out
  of scope: immutability post-issuance is the regulatory rule.

