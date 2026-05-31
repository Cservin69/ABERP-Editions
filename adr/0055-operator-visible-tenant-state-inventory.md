# ADR-0055 — Operator-visible tenant-state inventory MUST include every load-bearing artifact (runbook + snapshot-script contract)

**Status:** Accepted — S198 / PR-198 (2026-05-31). Pins the rule that
every PR adding a new tenant-state surface (table, side-store directory,
keychain entry, on-disk artifact) MUST extend the runbook's Appendix A
"File and keychain inventory" AND the `tools/snapshot-prod.sh`
docstring in the SAME PR.
**Author:** Ervin Áben (ABERP), session 198 brief — close the 💭 question
raised by the S172-S181 adversarial review.
**Supersedes / amends:** none — additive contract on the runbook +
snapshot-script ergonomics.
**Related:** ADR-0019 (relational storage strategy — per-tenant DuckDB
file), ADR-0030 (audit-ledger mirror file), the cutover runbook
(`docs/CUTOVER_RUNBOOK.md`), the prod-state snapshot script
(`tools/snapshot-prod.sh`), the S177 AP module (added `ap-artifacts/`
directory + `ap_invoice` table), the S180 NAV-as-DR wizard (added
`restored_invoice` table), the S197 AP XML fetch (extends `ap-artifacts/`
contents to one XML per ingested digest).

## Context

The session 182 adversarial review noted that S177 introduced
`~/.aberp/<tenant>/ap-artifacts/` (the side-store directory for incoming
NAV XML payloads) and the `ap_invoice` mirror table, S180 introduced the
`restored_invoice` table, and S197 extended `ap-artifacts/` with one XML
per ingested digest — but neither `docs/CUTOVER_RUNBOOK.md` Appendix A
"File and keychain inventory" nor `tools/snapshot-prod.sh`'s docstring
named any of them. The snapshot script captures them incidentally (the
`tar -C "${HOME}/.aberp" -czf "$SNAPSHOT_TGZ" "${TENANT}"` command sweeps
the entire tenant directory), and the DuckDB file captures the new tables
wholesale — so the artifacts are backed up correctly. But the operator-
visible inventory is silently incomplete: an operator reading the runbook
would not learn that `ap-artifacts/` exists or that the DB now carries
two new tables, and would not know to expect them in a snapshot.

The 💭 question framed it as "worth a one-line doc add". This ADR makes
the one-line doc add a CONTRACT, not a one-off — every future PR that
adds a tenant-state surface must extend both documents.

## Decision

**Two-part contract, enforced by review:**

1. **Runbook Appendix A coverage.** Any PR that adds a new on-disk path
   under `~/.aberp/<tenant>/`, a new DuckDB table, a new keychain entry,
   or a new side-store directory MUST add a row to the runbook's
   Appendix A "File and keychain inventory" in the same PR. The row
   names the path/table/entry, the owner (which subsystem creates it),
   and the lifetime (tenant-lifetime, invoice-lifetime, per-binary-build,
   etc.).
2. **`snapshot-prod.sh` docstring coverage.** The script's docstring
   `What it captures:` section MUST name every load-bearing artifact
   that lands under `~/.aberp/<tenant>/`, even when the artifact is
   captured incidentally by the wholesale `tar -czf` command. The
   docstring is the operator's mental model; "captured incidentally"
   means "not in the operator's mental model".

### What this PR (S198) implements

The contract above is asserted here; the existing gaps (S177's
`ap-artifacts/` + `ap_invoice`, S180's `restored_invoice`, S197's per-
digest XML files) are closed in the same PR — the runbook's Appendix A
gains four new rows, and the snapshot-script docstring gains two new
named artifacts. Future PRs inherit the contract from this ADR.

### Closed-vocabulary of artifact categories

For the rule to be enforceable in review, the categories are explicit:

| Category | Path / surface | Runbook row owner | Snapshot script note |
|---|---|---|---|
| Per-tenant config | `~/.aberp/<tenant>/seller.toml` | SPA seller wizard | required |
| Per-tenant DB | `~/.aberp/<tenant>/aberp.duckdb` | DuckDB ensure_schema | required |
| Per-tenant audit mirror | `~/.aberp/<tenant>/aberp.audit.log` | ADR-0030 | required |
| Per-tenant touchfile | `~/.aberp/<tenant>/.first-launch-acknowledged` | S166 ceremony | required |
| Per-tenant upgrade contract | `~/.aberp/<tenant>/.upgrade-snapshot.toml` | S171 / `snapshot-prod.sh` | required |
| Per-invoice side-store | `~/.aberp/<tenant>/invoices/<id>/` | PR-47α + S168 + S195 | required |
| AP per-digest side-store | `~/.aberp/<tenant>/ap-artifacts/` | S177 + S197 | required |
| Logo asset | `~/.aberp/<tenant>/logo.png` | PR-176 | optional (operator-supplied) |
| Loopback TLS cert | `~/.aberp/serve/<tenant>/` | serve at boot | regenerated as needed |
| Keychain entry | `aberp.<scope>.<tenant>` | various wizards | encrypted dump in `*-keychain.zip` |
| Mirror table | DuckDB table in `aberp.duckdb` | per-feature schema | captured via DB file |

Any future surface MUST land in one of these categories or extend the
table.

### Why a contract, not a one-off

The session 182 review found the gap because the artifacts were named in
the PR briefs but missed in the runbook. The same shape of miss could
recur on every future tenant-state surface (the next AP feature, the next
restore-from-X wizard, the next side-store directory). A one-off doc add
fixes the symptom; a contract pre-empts the recurrence.

The cost of the contract is small (one line added to two files per
qualifying PR; can be enforced by review checklist). The cost of NOT
having it is the failure mode the review surfaced: operator inventory
silently incomplete, snapshot script's mental model silently outdated,
and the disaster-recovery confidence calibration is wrong (operator
thinks "snapshot covered everything" when they have no list of what
"everything" includes).

## Consequences

### Wins

- The operator's mental model of "what state lives where" is the runbook's
  Appendix A; the model is complete by construction (every load-bearing
  artifact lands there in the same PR that introduces it).
- The snapshot script's docstring is the operator's mental model of "what
  the script captures"; the model is complete by construction (the
  docstring names every artifact).
- A future restore-from-snapshot operation can be planned against the
  runbook + docstring without needing to grep the code for side-store
  directories.

### Trade-offs

- Every PR that adds a tenant-state surface carries a small documentation
  burden. This is the cost of operator-visible completeness; it is paid
  by the PR author, not by the operator at recovery time.
- The closed-vocabulary table in §"Closed-vocabulary of artifact
  categories" needs maintenance — a category not yet listed (e.g., a
  future per-tenant search-index file) would need both a category row and
  an artifact row. Acceptable: category additions are rare enough that
  the cost is amortized.

### When to revisit

- The runbook adds a per-tenant search-index file, a per-tenant cache
  directory, or any artifact that does not fit the existing categories.
  At that point the category list above gains a row in an ADR amendment.
- The snapshot-script changes its capture strategy (e.g., adopts per-table
  DuckDB dumps instead of wholesale tar). The docstring contract still
  holds; the implementation of "what is captured" changes.

## Adversarial review

- *"What if the PR author forgets the Appendix A row?"* The contract is
  enforced by review (and by the existence of this ADR — a review pass
  citing the ADR is the enforcement surface). A future PR that adds a
  tenant-state surface without updating Appendix A trips ADR-0055 in
  the next adversarial review.
- *"What if Appendix A drifts from the code?"* The runbook is the
  operator's contract; drift between Appendix A and the code is itself
  a defect surfaced in the next adversarial review. The runbook is
  load-bearing and is treated as such.
- *"Why not auto-generate the inventory?"* Considered + rejected as
  premature: the artifacts are heterogeneous (DB tables, on-disk paths,
  keychain entries, side-stores) and live in different subsystems (DB
  schema, side-store conventions, keychain conventions). Auto-generation
  would need a manifest each subsystem writes to; the manifest itself
  is more surface than the runbook table. The runbook is the cheapest
  correct surface.
- *"Does this contract apply to dev tenants?"* Yes — the runbook's
  Appendix A is written against `~/.aberp/prod/` but the path structure
  is tenant-name-parameterized. The snapshot script defaults to `prod`
  but takes a tenant argument. Both apply uniformly to any tenant.

## Alternatives considered

- **One-off fix in S198, no contract.** Rejected per §"Why a contract,
  not a one-off" — same shape of miss would recur on the next feature.
- **Auto-generated inventory from a per-subsystem manifest.** Considered
  + rejected per §"Why not auto-generate the inventory?".
- **Inventory in CLAUDE.md instead of the runbook.** Rejected: CLAUDE.md
  is the AI-collaboration contract; the runbook is the operator's
  recovery contract. Different audiences; the runbook is the right
  surface for the operator-facing inventory.

## Invariants pinned

- The runbook's Appendix A "File and keychain inventory" is the single
  source of truth for "what tenant-state artifacts exist". A code path
  that creates a tenant-state artifact must have a corresponding row.
- The `tools/snapshot-prod.sh` docstring's `What it captures:` section
  is the single source of truth for "what the snapshot captures". A
  load-bearing artifact under `~/.aberp/<tenant>/` must be named there
  even if captured wholesale by the tar command.
- The closed-vocabulary category table in §"Closed-vocabulary of artifact
  categories" is the closed list of artifact shapes; an artifact that
  does not fit one of those categories is a trigger to amend this ADR.
