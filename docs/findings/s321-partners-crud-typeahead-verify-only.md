# S321 / PR-21 — Partner CRUD + invoice-form typeahead — VERIFY-ONLY

**Verdict: SHIPPED. Re-cut REFUSED. No `PROD_v2.27.13` produced.**

Mission was to "close `project_aberp_partners`" per the spec:

> SPA needs partner CRUD + typeahead in invoice form so operators stop retyping buyer info.

A verify-first pass found the entire mission already in production, shipped
incrementally across PR-48α (session-68) → PR-98, hardened through PR-217/S220.
This is the third consecutive backlog item found already-shipped (cf.
S319/PR-19, S320/PR-20). Per HARD RULE #2 (verify-only sessions refuse the
re-cut), no version was cut.

---

## Verify-pass verdict per area

### 1. Backend `partners` table — ✅ SHIPPED

`PARTNERS_SCHEMA_SQL` in [partners.rs:523](apps/aberp/src/partners.rs) creates
the table with every field the brief asked for and more:

| Brief field | Shipped column |
|---|---|
| `id` | `id VARCHAR PK` |
| `tenant_id` | `tenant_id VARCHAR NOT NULL` |
| `kind` ENUM (`customer\|vendor\|both`) | `kind` with `CHECK (kind IN ('Customer','Supplier','Both'))` |
| `name` (required) | `display_name` + `legal_name` (both NOT NULL) |
| `tax_id` (adószám) | `tax_number` (+ `eu_vat_number`) |
| `address` | `address_street` / `address_postal_code` / `address_city` / `address_country` |
| `country` | `address_country` |
| `email` / `phone` | `contact_email` / `contact_phone` |
| `created_at` / `updated_at` | both `VARCHAR NOT NULL` |
| soft-delete `archived_at` | `deleted_at VARCHAR` (NULL ⇒ active) |

Plus `bank_account`, `customer_vat_status`, `issued_invoice_count`, and two
covering indexes (`partners_tenant_deleted_idx`, `partners_tenant_display_idx`).
Schema is installed at boot via `ensure_schema`, same posture as products.

> Note on the brief's `kind` vocab: shipped vocab is `Customer/Supplier/Both`,
> not the brief's `customer/vendor/both`. Same three-way semantics; the shipped
> naming is the established one. No change.

### 2. CRUD endpoints — ✅ SHIPPED

Five routes registered at [serve.rs:2754](apps/aberp/src/serve.rs) (PR-48α),
all `require_ready` + bearer-gated:

- `POST /api/partners` → `handle_create_partner` ([serve.rs:8245](apps/aberp/src/serve.rs)) — 201 Created
- `GET  /api/partners?search=<q>` → `handle_list_partners` ([serve.rs:8152](apps/aberp/src/serve.rs))
- `GET  /api/partners/:id` → `handle_get_partner` ([serve.rs:8209](apps/aberp/src/serve.rs))
- `PUT  /api/partners/:id` → `handle_update_partner` ([serve.rs:8294](apps/aberp/src/serve.rs))
- `DELETE /api/partners/:id` → `handle_delete_partner` ([serve.rs:8345](apps/aberp/src/serve.rs)) → soft-delete (sets `deleted_at`), 204 No Content

Typeahead search (`list_partners`, [partners.rs:853](apps/aberp/src/partners.rs))
is a case-insensitive prefix filter (`LOWER(display_name) LIKE 'needle%' OR
LOWER(legal_name) LIKE ...`) over **active** rows only (`deleted_at IS NULL`),
ordered `display_name ASC` — natural typeahead order. The brief's "max 20" cap
is enforced client-side (see §3); the dataset is operator-scale.

### 3. Typeahead in invoice form — ✅ SHIPPED

`PartnerTypeahead.svelte` ([lib/PartnerTypeahead.svelte](apps/aberp-ui/ui/src/lib/PartnerTypeahead.svelte))
is wired into BOTH invoice issuance surfaces:

- [IssueInvoice.svelte](apps/aberp-ui/ui/src/routes/IssueInvoice.svelte) (buyer combobox, PR-74/session-96)
- [ModificationInvoice.svelte](apps/aberp-ui/ui/src/routes/ModificationInvoice.svelte)
- plus [ExtNavPartnerPickerModal.svelte](apps/aberp-ui/ui/src/lib/ExtNavPartnerPickerModal.svelte) (PR-217/S220, restored-row linking)

Behaviour matches the brief item-for-item:
- **200ms debounce** — [PartnerTypeahead.svelte:64](apps/aberp-ui/ui/src/lib/PartnerTypeahead.svelte) `debounceMs = 200`
- **max-20 cap** — `matches = result.slice(0, maxRows)` ([line 100](apps/aberp-ui/ui/src/lib/PartnerTypeahead.svelte))
- **auto-fill on select** — `buyerFieldsFromPartner` ([partners.ts:83](apps/aberp-ui/ui/src/lib/partners.ts)) maps `legal_name`, `tax_number`, address fields, country into the buyer form
- **manual entry still works** — the typeahead writes into the existing buyer fields, which remain operator-editable (additive, not gated)
- **"create new partner"** — `PartnersList`/`PartnerForm` provide the create path; the picker modal links to it

### 4. Settings page — ✅ SHIPPED

Route `partners` ([router.ts:102](apps/aberp-ui/ui/src/lib/router.ts), PR-54/session-74)
renders [PartnersList.svelte](apps/aberp-ui/ui/src/routes/PartnersList.svelte)
(table + search + add + per-row edit/archive) with the
[PartnerForm.svelte](apps/aberp-ui/ui/src/routes/PartnerForm.svelte) modal.

### 5. adószám validation — ✅ SHIPPED

`validate_tax_number` ([partners.rs:263](apps/aberp/src/partners.rs), PR-98)
enforces the Hungarian `xxxxxxxx-y-zz` shape; `validate_partner_inputs`
([partners.rs:423](apps/aberp/src/partners.rs)) drives field-level errors and
the required/forbidden logic per `customer_vat_status` (Domestic requires it,
PrivatePerson forbids it). All in the app layer — no SQL CHECK/trigger, per
[[no-sql-specific]].

---

## The one apparent gap is a documented deliberate rejection

The brief asked for audit events `partner.created` / `partner.updated` /
`partner.archived`. These do **not** exist — and that is **by design**, not an
omission. [partners.rs:23](apps/aberp/src/partners.rs):

> Partner CRUD does NOT fire audit-ledger entries. The audit ledger
> (`aberp_audit_ledger`) is reserved for invoice lifecycle per ADR-0008 —
> extending the `EventKind` ladder would couple partner [CRUD to the invoice
> ledger].

Implementing the events would override a standing architectural decision
(ADR-0008). Per CLAUDE.md #7 (surface conflicts, don't blend) and #3 (surgical),
I did **not** add them. Flagged below as a conservative call.

---

## Flagged conservative calls

1. **Audit events NOT added** — deliberate ADR-0008 rejection (above). If
   operators later need a partner-mutation audit trail, that is an ADR-0008
   amendment / separate design decision, not a verify-session re-cut.
2. **No `GET /api/partners?archived=true` view** — the brief listed an
   archived-listing endpoint. `list_partners` returns active rows only; there is
   no UI/endpoint to browse soft-deleted partners. The core mission ("stop
   retyping buyer info") does not need it, and archived partners are still
   reachable by id. Left as-is; flag for a future cut if an "un-archive" UX is
   wanted.
3. **`LIMIT 20` is client-side, not SQL** — `list_partners` returns all matching
   active rows; the 20-cap is `slice(0, maxRows)` in the component. Fine at
   operator scale; if the partner table ever grows large, push the cap into SQL.
4. **`kind` vocab** is `Customer/Supplier/Both` (shipped) vs the brief's
   `customer/vendor/both` — same semantics, kept the established naming.

None of these change the SHIPPED verdict.

---

## Regression coverage (already in tree)

- **Rust unit** — 27 `fn …partner…` tests in [partners.rs](apps/aberp/src/partners.rs)
  (validation matrix, identity-field freeze post-issuance, serde round-trips,
  id prefix).
- **Rust route integration** — [serve_partners_route.rs](apps/aberp/tests/serve_partners_route.rs),
  49 partner references (full CRUD over HTTP).
- **Vitest** — [partners.test.ts](apps/aberp-ui/ui/src/lib/partners.test.ts),
  [buyer-combobox.test.ts](apps/aberp-ui/ui/src/lib/buyer-combobox.test.ts),
  [partner-list-persistence.test.ts](apps/aberp-ui/ui/src/lib/partner-list-persistence.test.ts).

The brief's four proposed test names
(`s321_partner_crud_create_returns_typeahead_result`, `…typeahead_q_prefix…`,
`…soft_delete_excludes…`, `…audit_emits…`) are functionally already covered by
the above (minus the audit one, which tests a deliberately-absent behaviour). No
new tests written — adding name-only duplicates of existing coverage would
violate CLAUDE.md #8 (read before you write).

---

## Gates

Doc-only branch — no code touched, so backend/vitest baselines are unchanged
(2107 cargo / 1079 vitest). No build or test re-run was required for a
documentation-only change.

## Branch

`session-321/pr-21-partners-crud-typeahead` (pushed). No tag, no version bump,
no Dispatch cut.
