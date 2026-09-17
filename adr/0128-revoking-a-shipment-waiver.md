# ADR-0128 — Revoking a shipment waiver

- **Status:** **Proposed** (2026-09-17) — held on
  `feat/adr-0127-stale-report-nudge`, not merged. Three stated decisions
  (§D2, §D3, §D4) are Ervin's to veto at review.
- **Related:** ADR-0199 residual 18 (this), ADR-0090 round-7 (the waiver),
  ADR-0094 (no new `EventKind` — and why this is the exception), ADR-0127
  (the sibling work on the same belt).

## 1. Context — the only irreversible disarm in the quality gates

`grant_ncr_shipment_waiver` writes a row into `ncr_shipment_waivers`, and
`waived_for` is a bare existence check:

```rust
waivers.iter().any(|w| w.ncr_id == ncr_id && w.work_order_id == wo_id)
```

There is no state column, no expiry, and no revoke. So a waiver granted **in
error** disarms `open_ncr_ids_blocking_wo` for that `(ncr, work order)` pair
**permanently**.

The residual notes the remedy is "corrected through the NCR it names". That
works when the defect turns out not to be real — close the NCR and it stops
blocking anyway. It does **not** work in the case that matters: a real,
unresolved defect whose waiver was signed by mistake, or on the wrong work
order. The NCR must stay open, and nothing can make it block again.

Everything else in this module is reversible or append-corrected: an NCR
transitions, a CAPA is reviewed, a report is superseded. This is the one act
that cannot be taken back, and it is the act that releases parts.

## 2. Decision

### D1 — Revocation is a new APPEND, never a mutation

A second table, `ncr_shipment_waiver_revocations`, naming the `waiver_id` it
withdraws. `ncr_shipment_waivers` stays append-only and untouched, so the
record of what was signed, by whom and why survives exactly as before — a
revoked waiver is not an erased one.

### D2 — STATED DECISION: revocation is TERMINAL, not "latest wins"

Once a waiver is revoked it never disarms the belt again, whatever the
timestamps say. Re-permitting the shipment requires a **new waiver**, which is
a fresh deliberate act with its own sign-off and its own ledger entry.

The alternative — order the two by time and let the later win — invites exactly
the failure a safety gate must not have: a clock skew, a replayed row or a
back-dated grant silently re-arms a release nobody signed for today. Terminal
revocation fails toward **refusing to ship**, which is visible and correctable;
latest-wins fails toward shipping, which is neither.

### D3 — REVISED: one transaction on the shared Handle, so there is no residue

**The first draft of this decision was wrong, and the cut-gate caught it.**

It mirrored `grant_ncr_shipment_waiver`'s own `Connection::open(db_path)` and
then reasoned carefully about which order to write in:

| | row lands, ledger fails | ledger lands, row fails |
|---|---|---|
| **grant** (release) | unaudited release — worst | entry for a release that never happened |
| **revoke** (tighten) | gate blocks, unaudited — recoverable | gate still releases while the chain says it was revoked — worst |

…and concluded row-first. CHECK 10i and CHECK 10k refused the build:

```
✗ quality.rs grew its residual openers (12 > frozen 11) — the deferred surface
  may not grow; migrate the new opener onto the Handle
✗ opener fingerprint set DIVERGED
  > quality.rs|revoke_ncr_shipment_waiver:let conn = Connection::open(db_path)
```

The gate was right twice. The ordering question **only exists because the two
writes are on different connections.** On the shared `aberp_db::Handle` they
are one transaction: the revocation row and its ledger entry land together or
not at all, and there is no residue to order. That is ADR-0099's rule, and it
makes this writer strictly stronger than the grant it was copied from — the
grant keeps its ledger-first ordering because its opener is in the frozen set
and migrating it is a separate change with its own blast radius.

The table above is kept because it is still the right way to reason about a
writer that *cannot* be atomic. It simply does not apply to this one.

### D4 — STATED DECISION: this earns a new `EventKind`

ADR-0094's default is to avoid new kinds and add `Option` payload fields
instead, because a new kind has blast radius (`ALL_KINDS_COUNT` is pinned, and
every exporter sees it). That guidance is for when an additive field would say
the same thing. Here it would not: folding a revocation into
`ncr.shipment_waiver_granted` would make a withdrawal indistinguishable from a
sign-off in the hash chain, on the one event whose whole purpose is
accountability for releasing parts.

`ncr.shipment_waiver_revoked`, with the count bumped deliberately in the same
commit — the same one-line-bump discipline `EXPECTED_PROBES` already has.

### D5 — A revoke names a waiver, and refuses the rest

Revoking requires an existing, not-already-revoked `waiver_id`, and a reason
validated the same way the grant's is. A double revoke is refused rather than
appended twice, so the table cannot accumulate rows that mean nothing.

## 3. Adversarial surfaces

1. **Does terminal revocation strand a legitimate re-release?** It should not —
   a new waiver is available — but if the route or the SPA makes a second grant
   hard, D2 becomes an operational trap rather than a safety property.
2. **`waived_for`'s new argument.** Every caller must pass revocations; one
   that keeps the old two-argument form silently keeps the old behaviour.
3. **Row-first (D3) under a failed append.** The gate blocks and the chain has
   no entry. Is that discoverable, or does it look like a mystery refusal?
4. **The count bump.** `ALL_KINDS_COUNT` is asserted somewhere; a bump that
   misses one site reds a gate rather than silently passing — verify which.

## 4. Consequences

- The quality module gains its first way to take back a release.
- One new table, one new `EventKind`, one new route.
- A waiver's record is never destroyed; it is superseded by a second fact.
