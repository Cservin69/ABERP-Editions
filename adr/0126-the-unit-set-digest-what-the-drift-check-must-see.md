# ADR-0126 — The unit-set digest: what the drift check must actually see

- **Status:** **Proposed** (2026-09-16) — held for Ervin's review before the
  fix lands. The defect is **reproduced**, not argued: two red tests on
  `feat/adr-0126-unit-set-digest` (`f2ab042`) drive the real gate and it
  returns `Pass` in both.
- **Date:** 2026-09-16
- **Deciders:** pending.
- **Related:** ADR-0199 round-4 residual 10 (this), ADR-0094 (no new
  `EventKind`; additive `Option` payload fields), ADR-0099 (one writer, one
  tx), ADR-0123 §D8 and ADR-0124 §3 (the same defect class — a comment
  asserting a property the code does not have), ADR-0122 §D3 (name the
  omission rather than hiding it), ADR-0124 (why rendered bytes must not move).

## 1. Context — a detector that cannot see the identity that decides coverage

`resolve_qc_report_gate` decides whether an issued QC report still covers the
units being shipped. Its whole comparison is one string
(`apps/aberp/src/serve.rs:18550`):

```rust
if current.serial_range != aberp_qa::serial_range_of(&units_now) { … UnitDrift }
```

and `serial_range_of` (`crates/aberp-qa/src/qc/reports.rs:733`) builds that
string from **`part_serial` alone**:

```rust
let mut serials: Vec<&str> = units.iter().map(|u| u.part_serial.as_str()).collect();
serials.sort_unstable();
Some(format!("{} … {} ({} units)", serials[0], serials[n - 1], n))
```

Two different unit sets therefore render the same key. **Both are reproduced
against the real gate, which returns `Pass`:**

| shape | what changes | why the key does not move |
|---|---|---|
| **A** | the **middle** serial | first, last and count are unchanged |
| **B** | every **`part_uid`**, serials untouched | the key never reads `part_uid` |

**B is the severe one.** `part_uid` is what `qc_inspections.linked_part_uid`
carries, what the per-serial measurement join keys on, and what the NCR belt
keys on. So it is the identity that decides coverage, and it is exactly the one
the comparison cannot see. In the reproduction the measurements that justified
`accept` remain keyed to the old uid, so after the rewrite the units being
shipped have **no measurements at all** — and the gate passes them.

### The comment is worse than the gap

`serve.rs:18532` states:

> "Equality here is therefore exactly `the marks today are the marks the report
> froze over`."

That is false, and it is the sentence someone auditing this posture reads. Same
class as ADR-0123 §D8's phantom `derive_from` chain and ADR-0124 §3's
unconditional reproducibility claim: an assertion stops the reader looking.

### "Out-of-band only" is the case it exists for, not a mitigation

`record_part_marks` writes the mark set once and refuses a second write, so
neither shape is reachable through the marking route. The code's own comment
calls this check "a refusal only an out-of-band write can reach, kept because
guessing there is worse than stopping." A check whose stated purpose is to
catch out-of-band writes, and which is blind to two of them, is not mitigated
by the fact that only an out-of-band write can trigger it.

## 2. Decision

### D1 — The drift key is a DIGEST of the set, not a rendering of it

Freeze a SHA-256 over the **complete** unit set and compare that. A rendering
is a lossy projection chosen for humans; a digest is chosen for equality. The
present bug is entirely the consequence of using one for the other.

### D2 — The encoding must be INJECTIVE, and this is the part to get right

Each unit contributes both fields; pairs are sorted by `(part_uid,
part_serial)`; **every field is length-prefixed**, not delimiter-joined:

```
for (uid, serial) in sorted_pairs:  write!("{}:{}:{}:{}", uid.len(), uid, serial.len(), serial)
```

A delimiter join is not injective — `("AB","C")` and `("A","BC")` collide under
`join(",")` unless serials are known never to contain the delimiter, and
nothing enforces that. Length-prefixing removes the assumption instead of
documenting it. **Pinned by a test that constructs exactly that collision.**

### D2b — The empty set gets a digest, not a NULL

A lot-only report enumerates no units. Its digest is the SHA-256 of the empty
encoding, **not** `NULL`. Otherwise "no digest because there are no units"
cannot be told from "no digest because this report predates ADR-0126", and D5's
whole distinction collapses.

### D3 — The frozen digest lives in TWO places, deliberately

- a `unit_set_sha256` column on `qc_reports`, written at freeze, which is what
  the gate reads;
- an additive `unit_set_sha256` field on the existing `qcr.report_issued`
  payload, which is what makes it tamper-evident.

Per ADR-0094 this is **no new `EventKind`** — an additive `Option` field with
`#[serde(default)]`, matching the four such fields the struct already carries.
`EventKind`'s doc comment **is** the schema contract, enforced by
`export_payload_field_names_match_the_eventkind_docs`, so the doc comment is
updated in the same commit or the gate reds.

Both, because they answer different questions. The column answers "did the
marks change since issuance" cheaply. The ledger entry answers "was the column
itself rewritten" — a tamper that edits the marks *and* the column is caught by
the hash chain, and a tamper that edits only the column is caught by the
comparison.

### D4 — `serial_range` does not change, at all

The renderer prints it twice (`aberp-qc-pdf/src/lib.rs:344`, `:548`), so it is
inside the hash-pinned bytes. Changing its value or format would move the
rendered bytes of **every** report and walk straight into ADR-0124's renderer
drift. The digest is stored and compared, **never rendered**. A test asserts
the rendered bytes are unchanged.

### D5 — A report with no digest is verified WEAKLY, and says so

Reports issued before this change have no digest, and it **cannot be
backfilled**: reconstructing the frozen set would mean reading the marks as
they are now, which is the very thing under suspicion. That is ADR-0123's
no-backfill posture — an honest absence beats a reconstructed guess in an
evidence trail.

So: digest present → compare digests. Digest absent → fall back to the range
comparison **and record that the strong check did not run**. This leaves legacy
reports exactly where they are today (no regression) and makes every report
issued from now on strongly checked. The weakness ages out as reports are
superseded, and until then it is visible rather than assumed.

**Rejected: blocking every pre-digest report.** It is the stricter reading, but
it strands already-issued evidence on a technicality about when it was issued,
and the exposure it removes is the one that existed before this ADR anyway.
Flagged for Ervin — if he wants the strict form, it is a one-line change.

### D6 — Correct the false comment, dated

`serve.rs:18532`'s claim is corrected in place and marked, on the ADR-0123 §D8
precedent, so a reader who remembers the old text learns it was wrong.

### D7 — Scope: the Defense QC gate only

One comparison site, one freeze site, one column, one payload field. No pricing
path, no renderer, no `EventKind`, and nothing in the Portable arm.

## 3. What this does NOT fix

- **Residual 8** (the report does not re-open on a LATE measurement) is a
  different mechanism and stays open.
- The digest proves the *set* is unchanged. It says nothing about whether the
  measurements behind it are still valid — that is residual 8's territory.
- A tamper with write access to marks, column and ledger is out of scope; the
  chain is what defends that, and it already does.

## 4. Adversarial surfaces — attack these before Accepted

1. **Injectivity.** D2 is the whole fix. Is the length-prefixed encoding
   genuinely collision-free for any UTF-8 serial, including empty strings and
   multi-byte content? `len()` is bytes, not chars — is that consistent on
   both sides?
2. **Ordering.** Sorting by `(part_uid, part_serial)` — can two units share a
   `part_uid`? If so the sort is still total, but is the set then meaningful?
3. **The doc-contract test.** Adding a payload field without updating the
   `EventKind` doc comment reds a gate. Is the doc format exact enough that a
   near-miss passes silently?
4. **Do rendered bytes really not move?** D4 asserts it; it must be measured on
   a real render, not assumed from the absence of a field in the template.
5. **D2b's empty digest.** Is a lot-only report's `units` slice genuinely
   empty at freeze, or is it `None` somewhere upstream and therefore never
   reaching the digest at all?
6. **D5's fallback.** Does the degraded path leave a *visible* trace, or only a
   comment? If an auditor cannot tell a strong check from a weak one, D5 is
   decoration.
7. **The NCR belt and the measurement join.** This ADR asserts they key on
   `part_uid`. Verify at both sites rather than trusting the residual's summary.

## 5. Consequences

- One evidence-integrity hole closes; two reproduced shapes start blocking.
- `qc_reports` gains a column and `qcr.report_issued` an additive field.
- Legacy reports stay exactly as strong as they are today, and are visibly so.
- Rendered bytes do not move, so no renderer bump and no ADR-0124 drift.

## 6. Alternatives considered

- **Compare the full unit list instead of a digest.** Same correctness, more
  storage, and it puts an unbounded list in a ledger payload. The digest is the
  same statement compressed.
- **Re-derive the enumeration from `qc_report_lines`.** The residual's own
  suggestion, and it does not work: those rows carry no per-unit entries when
  every characteristic is lot-level, which is exactly when the set is least
  visible elsewhere.
- **Fix `serial_range_of` to render more.** Tempting and wrong — it is printed
  into hash-pinned bytes (D4), so every rendered report would change.
- **Do nothing.** The check would keep claiming, in a comment, a property it
  does not have, on the Defense evidence path.
