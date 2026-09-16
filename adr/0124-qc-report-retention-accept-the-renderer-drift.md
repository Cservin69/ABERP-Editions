# ADR-0124 — QC report retention: accept the renderer drift, and say what is actually guaranteed (F2)

- **Status:** **Accepted** (2026-09-16) — Ervin's decision on ADR-0122 §F2:
  accept the drift, build no reproducibility machinery, correct the overclaim.
  One half of his stated reasoning did not survive verification and is recorded
  as an open gap rather than written in as fact (see §4).
- **Date:** 2026-09-16
- **Deciders:** Ervin (accept the drift, 2026-09-11). Verification + write-up
  2026-09-16.
- **Related:** ADR-0199 §D7 (the claim being corrected), ADR-0122 §D2 (the 2×2
  that already handles the drift correctly in code) and §F2 (where this was
  raised), ADR-0123 §D8 (the same defect class — documentation asserting a
  property the code does not have).

## 1. Context — what §D7 claims, and why it is too strong

ADR-0199 §D7 decided the QC report's bytes are not stored: the SHA-256 is
pinned into the hash-chained ledger at issuance and the document is re-rendered
on demand. The claim, verbatim:

> The bytes themselves are **not** stored: the report re-renders
> deterministically from the frozen `qc_report_lines`, and the chain proves the
> bytes anyone re-renders are the bytes that were issued.

`apps/aberp/src/qc_report.rs:397` repeats it:

> Anyone who wants the document re-renders it from the frozen rows, and the
> chain entry proves the bytes they get are the bytes that were issued.

**Both are unconditionally stated and conditionally true.** `aberp-qc-pdf` is a
pure renderer, so the bytes are reproducible **by that renderer version**. The
crate's own `Cargo.toml` instructs "bump it in the same commit as any change to
the rendered bytes", and the version prints into the page footer — so a layout
change necessarily changes every byte of every report ever issued. After such a
bump, the issued bytes of every prior report become **unreproducible by ABERP,
permanently**.

That is not hypothetical maintenance trivia; it is the expected consequence of
a normal change to a renderer that is expected to change.

## 2. Decision — accept the drift

**Ervin's call, recorded:** accept it. Build no reproducibility machinery — no
keeping superseded renderer versions compilable behind a feature, no storing
bytes after all. ADR-0122 §F2 listed those options; they are declined.

The reasoning that survives verification: **the thing that must be provable is
the DATA, and the data is provably intact independently of any renderer.** The
`qcr.report_issued` entry pins the disposition, the accountability counts
(required / measured / passed / failed / unaccounted), the traceability keys,
the drawing reference and the serial range into a hash-chained, append-only
ledger. An auditor asking "was this part conforming, and does the record say so
consistently" is answered from the chain. The PDF is a *rendering* of that
answer, not the answer.

So the honest position is: the **record** is durable and tamper-evident; the
**document** is reproducible only while its renderer exists.

## 3. What is corrected

§D7's sentence, and the code doc that repeats it, are corrected in place — not
silently rewritten but marked and dated, so a reader who remembers the old text
learns it was wrong rather than wondering whether they misremembered. That is
the same treatment ADR-0123 §D8 gave the false `EventKind` traceability
comments, and for the same reason: these are the sentences someone auditing the
retention posture reads.

The corrected claim, in both places:

- the bytes are reproducible **by the renderer version recorded on the issued
  entry** (`renderer_version`), and the chain proves a re-render by that
  version is the document that was issued;
- after a renderer bump the prior bytes are **not** reproducible, and ABERP
  says so at the point it matters rather than producing a document that fails
  its own hash check.

**No code behaviour changes here, because the code already behaves correctly.**
ADR-0122 §D2 built the 2×2 that handles exactly this: on a SHA mismatch, the
**same** `renderer_version` refuses the whole export (the frozen rows moved —
a tamper signal), while a **different** `renderer_version` omits that document
and names the omission in `manifest.qc_documents_omitted[].reason`. The export
already tells an auditor which document is missing and why. What was wrong was
only the documentation around it.

## 4. ⚠️ The retention half of the stated reasoning does NOT hold

Ervin's decision rested on two legs: (a) the data is provably intact via the
ledger, and (b) **the original issued PDF is retained in the email archive** as
the archival copy.

**(a) is true and is the load-bearing half (§2). (b) is false today**, and it
is written here as a gap rather than into §D7 as fact. Four checks, re-run
2026-09-16:

1. **Phase 1c — auto-attach the QC report to the shipment e-mail — is OFF and
   was never built.** ADR-0199 Open Q8: *"✅ RESOLVED — default accepted: OFF.
   Phase 1 ships no automatic mailing of a compliance document. Not built."*
   That was Ervin's own accepted default.
2. **No mail path references QC at all.** `email_invoice.rs`,
   `email_relay*.rs` and `email_outbox_poll_daemon.rs` contain no `qc` /
   `qcr_` / `QcReport` occurrence. There is no shipment e-mail path of any kind.
3. **Even the invoice mail path retains nothing.** `email_invoice.rs` renders
   the PDF on the fly (`print_invoice::render_to_bytes`, `:321`) and hands it to
   lettre. Nothing is written to disk; any retained copy lives on the
   *recipient's* mail server, which is not an ABERP retention mechanism.
4. **The one place attachment bytes persist** is the storefront relay queue
   (`outbound_email_queue` → `~/.aberp/<tenant>/email-relay-attachments/…`), and
   its only enqueuers are a storefront relay route and `quote_refuse` — neither
   carries a QC report. It is also a loose directory with no enforced
   retention, which is the shape §D7 already rejected as "not an audit-grade
   record".

**Consequence, stated plainly:** there is today **no archival copy of an issued
QC report anywhere in ABERP**. After a renderer bump, a prior report's exact
bytes are gone — recoverable only from a copy someone outside the system kept.

This does not overturn §2. The data remains provable, which is what the
decision rests on. But the decision should be made knowing the fallback does
not exist, and anyone reading §D7 should not infer that it does.

**Options, if the archival copy is wanted** (each a build, none taken here):
turn Phase 1c on **and** make that path retain bytes; or write the issued PDF
to a retained artifact directory at issuance; or reopen §D7's no-store decision.
The first is the smallest and is the one Ervin's reasoning already assumed.

## 5. Consequences

- §D7 and `qc_report.rs` stop asserting unconditional reproducibility. The
  retention posture a reader takes away matches the one the code implements.
- The QC evidence export (ADR-0122) is unchanged: it already refuses on the
  tamper cell and omits-and-names on the renderer cell.
- **A renderer bump is now a decision with a stated cost.** Bumping
  `aberp-qc-pdf` renders every previously issued report unreproducible; §3 makes
  that visible at the point someone edits the renderer rather than discoverable
  afterwards.
- The gap in §4 is tracked in `docs/BACKLOG-designed-to-live.md` (F2) and stays
  Ervin's call.

## 6. Alternatives considered

- **Write (b) into §D7 as stated.** Rejected: it is false, and a retention
  claim that is false is worse than the overclaim being corrected — it would
  give an auditor a fallback that does not exist.
- **Keep superseded renderer versions compilable behind a feature.** Declined by
  Ervin (§2). It also only defers the problem: the set of versions to keep grows
  without bound, and each must stay buildable against a moving toolchain.
- **Store the bytes after all.** Declined; §D7's original reasoning against it
  (every checkpoint, mirror sync and snapshot carrying a derivable payload)
  still stands, and §2's data-integrity argument means the trade is not needed.
- **Say nothing and leave §D7 as it is.** Rejected on the ADR-0123 §D8
  precedent: the documentation is the artifact people audit, and an assertion
  is worse than an absence because it stops the reader looking.
