# Adversarial review — ADR-0116 DB snapshot system

**Branch** `feat/adr-0116-snapshot-system` @ `48ebc26` (off `origin/main` `bae151d`)
**Reviewed** 2026-08-29, in a detached worktree at `~/Documents/Claude/Projects/ABERP-snap-review-wt`
**Scope** re-verify the five self-reported fixes + the gate-that-fooled-itself by PROBE, then attack the core contract fresh.

---

## VERDICT: **BLOCKED**

One defect makes the branch's headline command a no-op across a restart, and it is
reproducible end-to-end through the shipped binary. Six further fix-firsts follow it.

> `aberp restore --in-place` rolls the live database back exactly as designed —
> and then **`aberp serve` refuses to boot**, because the restore leaves the audit
> mirror diverged from the restored chain and the boot path classifies that as a
> terminal incident. The only way out is the hand-reconciliation D3.4 exists to
> eliminate.

Everything else in the branch is strong. The crate layer, the evidence guard, the
preserve unit's ordering, the archive path, and the D3.1 WAL-first install all hold up
under attack. The gate is green and the supporting fixes are real. The blocker is a
seam *between* two correct subsystems that nobody walked across.

---

## 1. BLOCKED — `aberp serve` will not boot after an in-place restore

**Reproduced end to end through the built binary** (`adv_b5` + `adv_b6`).

```
live before rollback  = (6 invoices, 10 audit rows)
`aberp restore --in-place --snapshot 1 --confirm --accept-data-loss`   -> ok, "In-place restore complete."
live after rollback   = (2 invoices,  4 audit rows)          <- correct, the rollback worked

then, EXACTLY what serve.rs:1801 runs at every boot:
BOOT ROUTER: Err(MirrorDivergedFromDb {
    first_divergent_seq: 4, mirror_max_seq: 10, db_max_seq: 4,
    preserved: ".../aberp.duckdb.audit.log.diverged-<nanos>.bak" })
ahead=false diverged=true
```

`serve.rs:833 boot_mirror_route` maps `MirrorDivergedFromDb` to `RefuseFatal`, and the
branch it takes is:

```rust
BootMirrorRoute::RefuseFatal => {
    tracing::error!(target: "audit_event", event = "audit_mirror_boot_refused", …);
    return Err(anyhow::Error::new(e)).context(refusal_context);
}
```

**`aberp serve` does not start.** The flagship D3.4 command — sold as *"the guarded
in-place restore that replaces the hand-swap"* — leaves the product down, and the only
way out is to reconcile the mirror by hand: the precise per-incident manual step the
programme set out to eliminate. Nothing in the command's output says so.

### The mechanism, in the branch's own code

1. `take.rs::restore_in_place` step 3 deliberately leaves `.audit.log` at the live path:
   *"the mirror does NOT move. It is the durable record and stays at the live path."*
   After a backwards rollback it still holds the tail the operator discarded.
2. `run_restore_in_place` step 7 then appends `SnapshotRestored` **to the restored
   chain** (D3.5 / F8: *"the live DB IS the restored DB, so the row is simply the next
   seq on the restored chain"*). That row lands at a seq the mirror already holds with a
   *different* entry — here seq 4.
3. At boot `ensure_consistent_with_db` compares the two and reports
   `MirrorDivergedFromDb { first_divergent_seq: 4 }`, which is terminal by design:
   *"two different committed entries claim one seq, and choosing between them is a
   business question… boot refuses so an operator reconciles them."*

Step 2 is what makes the divergence **certain**, and step 2 is new on this branch.

### The other half of the fork, for completeness

If the restored chain is instead a clean **prefix** of the mirror — no divergent row, which
is what you get if the step-7 append is ever moved, fails, or is made conditional — the
route flips to `AutoRecover`, and `adv_b5` shows what the engine then does when called
directly:

```
recover_or_refuse -> Ok(Recovered { source_snapshot_seq: 2, snapshot_audit_count: 10,
                                    replayed_entries: 0, recovered_max_seq: 10,
                                    retained_corrupt_db: Some(".../aberp.duckdb.CORRUPT-<tag>") })
live after         = (6 invoices, 10 audit rows)    <- the rollback is silently UNDONE
```

It rebuilds from `source_snapshot_seq: 2` — the **mandatory pre-restore snapshot** that
step 2 of the restore itself just created, and which is by construction the newest valid
snapshot in the store, i.e. the exact state the operator rolled back *from* — and files
the restored database away as `.CORRUPT-<tag>`.

So the two available outcomes of a successful in-place rollback are **refuse to boot**
(shipped) or **silently revert the rollback** (one code change away). Neither is the
contract.

### The premise that D3.4 falsified

`boot_mirror_route`'s comment reads: *"a CLEAN ahead mirror is the fingerprint of a
torn-write / lost DB commit."* That was true before this branch. It is no longer: a
mirror that disagrees with the live DB is now **also** the fingerprint of a deliberate,
acknowledged rollback, and the two are indistinguishable on disk. The boot path has no
way to tell an incident from an operator's decision.

The CLI even predicts the collision without checking its outcome:

> *"If the restored database and the mirror disagree, the next boot's guarded recovery
> path owns that decision."*

That decision is: refuse to start.

### Why no existing test catches it

`adr0116_restore_journey_e2e.rs` and `ac9_in_place_restore_preserves_db_wal_and_marker…`
both stop at *"restore complete."* Neither walks the boot that follows — the one step an
operator cannot skip.

### Scope, stated honestly

- Affects the **backwards** in-place restore — exactly the case `--accept-data-loss`
  exists for. A restore that is not behind the mirror produces neither divergence nor an
  ahead mirror.
- The side-path `aberp snapshot restore --to` is unaffected (it never touches the live DB
  or its mirror).
- The underlying hazard **pre-exists the branch**: an operator hand-swapping a
  rolled-back file in leaves the same disagreeing mirror. What is new is that D3.4
  automates the procedure, guarantees the divergence via its own step-7 audit row, and
  ships as the sanctioned replacement — so an operator who follows the new documented
  path is guaranteed to hit it, with no warning and no remedy in the output.

### Minimal fix

The rollback is an explicit decision; the mirror's discarded tail must stop being an
input to the next boot. In preference order:

1. **Move the mirror into the PRE-RESTORE unit** — `<db>.PRE-RESTORE-<tag>.audit.log` —
   and write a fresh mirror matching the restored chain, inside the same operation, before
   step 7 appends. Symmetric with the DB/WAL/marker unit, keeps the discarded tail as
   protected evidence at its new name, and leaves boot with nothing to reconcile. This
   *contradicts* step 3's current rule, deliberately: "the mirror is the durable record"
   is right in general and wrong once the operator has acknowledged that its tail is not
   to be replayed.
2. If the mirror must stay put: land a **rollback marker** beside the DB that
   `boot_mirror_route` consults, so a post-rollback disagreement is classified as
   *expected* and reconciled to the restored chain rather than treated as an incident.

Either way, add the missing journey step — **restore → boot → assert the rollback
survived and serve starts** — to `adr0116_restore_journey_e2e.rs`. It is the assertion
the whole feature rests on, and it is the one step the current e2e stops short of.

---

## 2. FIX-FIRST, ranked

### F2 — the "re-verified" line after an in-place restore never reads the installed DB

`take.rs:870` — step 6, *"re-verify what is now on disk"* — calls
`validate_export(export_dir, tenant)`. That is the **identical call** to `pre` at
`take.rs:738`, a pure function of the export directory, with no reference to `db_path`.
`InPlaceRestoreReport.installed` is documented as *"Validation of the installed file,
re-run after the install"*, and `run_restore_in_place` prints it as:

```
  re-verified    invoices=2 audit_rows=3 chain=3
```

`adv_a1` proves the vacuity: `report.installed == validate_export(export_dir)` exactly,
and the same numbers still come back `ok` after the installed database is overwritten
with `b"not a duckdb file at all"`.

Partial mitigation: `emit_snapshot_event` afterwards opens the restored DB and appends,
so a totally unopenable install still errors the command. But that is an incidental
liveness check, not a verification, and it does not check row counts or the chain.

**Fix:** open `db_path` read-only and run the same smoke set (invoice count, audit
count, `verify_chain`) against it; compare to the export's numbers and fail loudly on a
mismatch. Roughly ten lines, reusing `ensure_serve_is_stopped`'s probe shape.

### F3 — a bare seq selector can silently resolve to a DIFFERENT snapshot

`store.rs::resolve_selector_in` tries the identity token as a **prefix** (form 2) before
the bare seq (form 3). `snapshot_identity` is `<seq>@<ts>#<sha8>`, so the typed string
`"2"` prefix-matches seq **24**'s identity.

- `adv_a2`: a store holding only seq 24. `resolve_selector(store, "2")` returns **seq
  24**. On `aberp restore --in-place --snapshot 2 --confirm` this overwrites the live
  database from a snapshot the operator never named. Seq 2 does not exist; the correct
  answer is `NotFound`.
- `adv_a2b`: the mirror image. A store holding seqs 2 **and** 24. Seq 2 is unique, but
  `--snapshot 2` is **refused as ambiguous**, because seq 24's identity also starts with
  `"2"`. The documented bare-seq form stops working as the store grows past seq 9.

**Fix:** try the exact bare seq before the identity prefix, and require the identity
form to contain `@` (or anchor it as `<seq>@`) before prefix-matching.

### F4 — the D3.3 data-loss gate is silently disarmed by an unreadable live audit table

`ensure_serve_is_stopped` records `-1` for "could not read" (`unwrap_or(-1)`), and
`build_preflight` coerces it with `.max(0) as u64`. `-1` therefore becomes a confident
zero in the refusal arithmetic. `adv_b1` output:

```
  live delta       EXACT (serve is stopped): the live DB holds 0 audit entries and -1 invoices; this snapshot carries 5 / 3
                   → nothing newer than the snapshot would be lost

  VERDICT: would PROCEED.
```

The `-1` leaks visibly for invoices while being silently swallowed for the audit count —
the one number the gate keys on. This is the same sentinel mistake the ADR is careful to
avoid for `anchor_count` (*"`-1` means not recorded, NEVER zero"*), made in the place
where it disarms the safety, and in exactly the scenario a restore exists for: a
database whose tables cannot be read.

**Fix:** carry `Option<i64>` (or keep `-1` and branch on it). A `None`/`-1` live head must
report `UNKNOWN` and refuse without `--accept-data-loss`, never print `EXACT … 0`.

### F5 — the gate compares against the RECORDED `meta.audit_count`, not the live re-validation

`build_preflight` re-runs validation LIVE and says why: *"never trust the recorded
verdict for a decision this destructive."* Two lines later the data-loss comparison uses
`record.meta.audit_count` — the recorded number — while `pf.live.audit_count` is sitting
right there.

`adv_b3`: inflating `meta.json`'s `audit_count` to `999999` (export bytes untouched, so
the live re-validation still reports the true 3) lets an **unacknowledged** rollback
proceed and discard 7 committed audit rows.

**Fix:** one-token change — compare against `pf.live.audit_count`.

### F6 — the snapshot WRITE path has zero fsyncs

`take_snapshot_with` writes the parquet files (via `EXPORT DATABASE`), `schema.sql`,
`load.sql`, and `meta.json`, then `rename(partial → final)` — with **no `fsync` on any
of them and none on the store directory**. `grep -n 'sync_all' take.rs` returns a single
hit, and it is in the restore preserve step.

This is the exact shape the branch's own D3.1 rationale calls out as unacceptable:

> *"This was the ONE file-install path in the tree with **no `fsync` at all**
> (`grep -c sync_all take.rs` → 0), while its sibling `atomic_install` did the full
> durable recipe."*

The restore install was fixed. The **snapshot production path — the artefact the entire
feature exists to create — is now the one with no fsync at all.** After a power cut a
snapshot directory can be visible while its parquet bytes never reached the device, and
`meta.json` will still say `valid: true`.

Aggravator (`adv_a10`): `plan_retention` reads the **recorded** `meta.valid`, never the
export's current state. A torn snapshot therefore holds the never-prune-the-newest-valid
slot and, under a narrowed policy, evicts the last restorable one:

```
RETENTION EVICTED THE LAST RESTORABLE SNAPSHOT: plan kept [2] / pruned [1] (removed [1])
```

**Honest scope:** with the shipped defaults (`keep_last: 24`, 30 daily, 52 weekly) the
eviction is not immediate — a single torn newest snapshot displaces nothing for days.
The immediately-real half is that the store can silently hold unrestorable snapshots
after the one event most likely to make you need them, and nothing detects it.
`snapshot list --verify` can see it; nothing schedules it.

**Fix:** fsync each file in the partial dir + the partial dir itself before the finalize
rename, then fsync the store dir — the recipe `crash_safe::atomic_install` already
implements. Consider having the D1.3 floor run `list --verify` and emit a loud event on
a recorded-valid-but-live-invalid snapshot.

### F7 — CHECK 11d is still satisfiable by a **dead** guard

The self-reported fix is real as far as it goes: 11d now asserts the *scanner's* verdict
(`^crates/aberp-snapshot/src/retention.rs:prune:GUARDED:`) instead of grepping
`retention.rs`, and the scanner strips comments and string literals, so the doc-comment
escape is closed and their probe passes.

But the scanner's `GUARDED` verdict is still **token presence in the function body**, not
liveness. Mutation **M1**, planted in a fresh copy:

```rust
if false && crate::evidence::is_protected_evidence(&rec.dir) {
```

→ `CUT-GATE: ✓ PASSED`, with the guard dead.

This is the ADR-0098 opener-scan class one level in: the first cut could be flipped by
editing a comment; this one can be flipped by editing an operator. And there is **no
test** pinning `prune`'s refusal anywhere in the tree — `adv_a7` is the first.

**Fix:** add the missing unit test (`prune` must not unlink a protected directory and
must not report it as removed). A test is the right instrument here; a scanner cannot
answer liveness.

### F8 — the scanner cannot see the one function whose job is unlinking evidence

Running `tools/adr0116_evidence_removal_scan.awk` over the corpus:

```
crates/aberp-snapshot/src/evidence.rs:archive_then_remove:OTHER:remove_file@L746
crates/aberp-snapshot/src/evidence.rs:archive_then_remove:OTHER:remove_file@L761
```

`archive_then_remove` is the sanctioned release path — it unlinks recovery evidence from
a live tenant home by design — and the classifier calls it **OTHER**: *"a removal in a fn
with no tenant-home reach."* Its body carries none of the `TH` tokens (`db_path`,
`tenant_home`, `.aberp`, `read_dir`, …); it works through `artefact.path` and `dest`.

Consequence: the site is outside `tools/adr0116_tenant_home_removal_sites.txt` and
outside CHECK 11b's may-only-shrink freeze. The function is careful today (SHA-verified
copy read back from disk, `fsync` before unlink, credential material and directories
refused — all confirmed by reading). But if a later change weakens it, or adds a removal
inside it, **CHECK 11 stays green**.

Related, latent (mutation **M5**): a removal spelled through a direct import
(`use std::fs::remove_file;` … `remove_file(p)`) escapes the `fs::remove_(` regex
entirely. A tenant-home sweeper written that way passes the whole gate. Nothing in-tree
uses that spelling today — this is the "gate bans ONE spelling" class already on record
here from PR #41.

**Fix:** add the artefact-path token family (`artefact`, `dest`, `path`) or classify
`aberp-snapshot`'s `evidence.rs`/`recover.rs`/`crash_safe.rs` as tenant-home-reaching by
file, and list `archive_then_remove` in the frozen manifest with its reason. Widen the
removal regex to catch the bare spelling.

---

## 3. Lower severity / notes

**F9 — a seq collision inside one store leaks a condemned snapshot** (`adv_a9`).
`prune` resolves a condemned seq with `records.iter().find(|r| r.meta.seq == seq)`, so
with two same-seq records only the **first** is ever unlinked while the report claims
both were removed (`plan condemned [7,7]`, `prune reported [7,7] removed`, one directory
still on disk). Reachable: the in-process daemon and the D1.3 launchd floor both derive
`next_seq` by scanning, with no lock; `--if-stale-secs` is a TOCTOU check, not mutual
exclusion. Consequence is a store leak and a false report, not data loss.
Fix: key `RetentionPlan` on the directory, not the seq.

**F10 — latent: the prod-shaped snapshot store reads as wholly protected** (`adv_a8`).
`PHYSICAL_BACKUP_COMPONENT = "aberp-snapshots"` is compared as a whole path component, so
it cannot distinguish `~/aberp-snapshots/` (which the doc comment says it means) from
`~/Documents/ABERP-snapshots/` (prod's managed store). Under the latter every ordinary
`snap-*` directory returns `is_protected_evidence == true`, so `prune` would refuse the
entire store and retention would become a silent no-op. **Editions is safe** — its store
is `ABERP-snapshots-defense` — and `ensure_not_prod_path` blocks the prod store here.
This bites on the port back to the prod line. Fix: match the full parent path shape, not
a bare component name.

**F11 — the D1.2 skipped-tick checkpoint is unpinned.** The fix is correct in code
(`run_supervised`'s skip branch calls `live_checkpoint_logged` before napping), but
nothing pins it: no test names it and no gate arm covers it. Re-introducing a `continue`
above that line silently un-wires ADR-0095 §3 in precisely the configuration ADR-0116
sets up (a floor satisfying the staleness window on most ticks). Cheapest pin: a gate
assertion that `live_checkpoint_logged` appears in **both** arms of the loop.

**Note — `diverged-*` is missing from the evidence family list.** ADR-0099 R3's
divergence preserve writes `aberp.duckdb.audit.log.diverged-<nanos>.bak` (observed in
`adv_b6`), and `EVIDENCE_FRAGMENTS` has no `diverged` token — the artefact is protected
only by the `.bak` suffix and, under a tenant home, by the allow-list inversion. Both
hold today, so this is not a live hole; but `diverged` belongs beside `ahead` and
`healed-` in the list, which is the predicate that applies *everywhere*, including
outside a tenant home. One-line change.

**Note — `guarded_remove` is not a live-file guard.** Under a tenant home
`is_protected_evidence` returns `!name_is_live(name)`, so `guarded_remove` would happily
unlink `aberp.duckdb` itself. Not reachable today (both callers pass prefix-filtered
transients), and correct by its charter — but the name reads like a safety net it is not.

**Checked and clean — attacks that found nothing.** Worth recording so the next reviewer
does not re-spend the time:

| Attack | Result |
|---|---|
| `--dry-run` claims "NOTHING is written" but takes a read-write live open | **clean** — `adv_b4`: live DB and WAL byte-identical after a dry-run |
| side-path restore strips a live WAL | **clean** — `adv_a3`: live DB + WAL byte-identical; no orphan WAL beside the target |
| PRE-RESTORE unit loses the un-checkpointed commits | **clean** — `adv_a4`: preserved unit opens and carries the full pre-rollback counts |
| restore parity + chain from genesis | **clean** — `adv_a4`: restored DB's invoice and audit counts match the snapshot exactly and `verify_chain` returns the full length. Byte parity is deliberately *not* the contract — the restore is a logical `IMPORT`, and the branch's own e2e asserts the rebuilt file differs from the pre-loss bytes |
| a `*.partial` export presented as restorable | **clean** — `adv_a5`: invisible to `list_snapshots` and to all four selector forms |
| a bit-rotted snapshot installed anyway | **clean** — `adv_a6`: `RestoreFromInvalid`, live DB byte-identical |
| `prune` unlinks a protected directory | **clean** — `adv_a7`: refused, newest valid never pruned |
| `archive_then_remove` can lose evidence | **clean** by reading — copy read back from disk and SHA-compared, `fsync` before unlink, credential material and directories refused |

---

## 4. Re-verification of the five self-reported fixes — by probe, not assertion

| # | Claim | Verdict | Evidence |
|---|---|---|---|
| 1 | the preserve step could strip the **live** DB of its WAL | **CLOSED** | `adv_a3` (side path: live DB+WAL byte-identical), `adv_a4` (preserved unit openable, carries the WAL-only rows). DB-moves-first + rollback-on-WAL-failure read at `take.rs:790-830`; their own `preserve_moves_the_db_first_and_rolls_back_if_the_wal_move_fails` is a real mutation pin. |
| 2 | a second unguarded sweeper (`seller_toml_backup`) | **CLOSED and GATED** | Mutation **M7** reverts it to `fs::remove_file` → `CUT-GATE: ✗ FAILED`, naming `apps/aberp/src/seller_toml_backup.rs:prune_old_backups:TENANT_HOME:remove_file@L151`. Mutation **M6** (same revert on `recover::cleanup_siblings_with_infix`) also RED. |
| 3 | `.bak` matching `.backup-`; inversion applying at any depth | **CLOSED** | `.bak` moved to `EVIDENCE_SUFFIXES` (suffix-anchored); `path_is_under_tenant_home` requires `comps.len() == i + 3`, so the inversion is siblings-of-the-live-DB only. Their `seller_toml_backup_is_removable_but_backup_shaped_evidence_is_not` and `the_inversion_is_scoped_to_siblings_of_the_live_db` cover both directions; `adv_a7` confirms a family-token directory is still refused inside the store. |
| 4 | D1.2 skip bypassing the ADR-0095 §3 live checkpoint | **CLOSED in code, UNPINNED** | `run_supervised`'s skip branch calls `live_checkpoint_logged` before the nap. No test, no gate arm — see **F11**. |
| 5 | **CHECK 11 fooled by its own doc comment** | **PARTIALLY closed** | The check now keys on the scanner verdict, the scanner strips `//`, `///` and `/* */`, and their probe (`s/crate::evidence::is_protected_evidence\(&rec\.dir\)/false/`) is caught. But mutation **M1** (`if false && …is_protected_evidence(…)`) leaves the gate GREEN with the guard dead, and no test pins the behaviour — see **F7**. |

### The nine CHECK 11 negative probes

All nine are structurally sound: `expect_fail` requires a **non-zero exit AND** a
`grep -F` match on that check's own message, and `assert_planted` catches a plant that
modified nothing (the BSD-sed class). The set covers the sweeper shape in both crates,
the guarded non-trigger, a deleted `evidence.rs`, the case-sensitivity revert, the
allow-list inversion revert, `prune`'s consultation, a scanner blind to the guard, and a
deleted scanner. **All nine were run and all nine caught their mutation**
(`probes passed: 69   broken/escaped: 0` for the whole harness, 0 `HARNESS BUG`, 0
`ESCAPED`) — including the one that matters most here, the `prune`-consultation probe,
which now matches on the scanner-verdict wording rather than the bare grep that its own
doc comment used to satisfy.

Two gaps in the set, both now filed: nothing probes a **dead** guard (F7) and nothing
probes an **unrecognised removal spelling** (F8).

---

## 5. Full gate — all green, nothing de-gated

Run in a detached worktree at `48ebc26`, `--features production`, `--locked`.

| Gate | Result |
|---|---|
| `ADR-0093 DB-isolation cut-gate` (CHECK 1–11, all ENFORCED) | **PASSED** — CHECK 11: 29 removal sites classified, 3 guarded, 7 frozen tenant-home sites |
| `cut_gate_negative_probes.sh` (incl. the 9 new CHECK 11 probes) | **PASSED — `probes passed: 69   broken/escaped: 0`** |
| `ADR-0111 checkpoint-site cut-gate` + its probes | **PASSED** / **PASSED** |
| `ADR-0110 D3 durable-ack gate` + its probes | **PASSED** / **PASSED** (census 6, all PROPAGATE) |
| `cargo fmt --all -- --check` | **clean** |
| `cargo build --workspace --all-targets` | **clean** |
| `cargo clippy --workspace --all-targets -- -D warnings` | **clean** |
| `cargo test --workspace --no-fail-fast` | 203 test binaries green; 2 red — `aberp-cad-extract-wrapper::step_extract_smoke` and `::hole_mining_failure`, both the known environmental Python-package reds, unrelated to this branch |
| `cargo deny check` (advisories, licenses, bans, sources, +4 drift lints) | **advisories ok, bans ok, licenses ok, sources ok** |

No enforcement flag was disabled and no check was skipped.

---

## 6. The two drift decisions — recommendations for Ervin

### (a) `--accept-data-loss` for a backwards rollback — **KEEP**, with two conditions

The acknowledgement gate is **real and un-bypassable by omission**. `adv_b2` proves the
full loop through the binary: without the flag the restore refuses and the refusal names
the flag; with it the restore proceeds and emits

```
WARN ADR-0116 D3.3 — --accept-data-loss was passed: this restore will DISCARD committed
     audit entries  live_head=10 snapshot_audit=3 discarded=7
```

It does not normalise data loss. The alternative — implementing the ADR literally and
banning every backwards rollback — is worse: rolling backwards past committed rows *is*
what a rollback is, and a banned operation's workaround is `cp`, which is the thing this
programme exists to eliminate.

Conditions before it can be trusted:

1. **Fix F4 and F5.** As shipped the gate is silently off when the live audit table
   cannot be read (the corrupt-DB case it is *for*), and defeatable by a stale or
   tampered `meta.json`. An acknowledgement gate that is absent exactly when it matters
   is worse than none, because its presence is what stops anyone looking.
2. **Print the discarded count on stdout**, in the completion message, not only as a
   `tracing::warn!`. Today the only durable record of "I threw away 7 committed audit
   entries" is a log line and shell history; nothing lands on the restored chain.

And note the sequencing: none of this matters until **F1** is fixed, because today the
rollback the operator acknowledged does not survive the next boot anyway.

### (b) The Defense anchor sanction — **KEEP the drift**, with one addition

The concern in the brief is that warn-on-zero makes `--accept-unanchored` muscle memory.
**The risk runs the other way.** Under the current shape, with zero anchor rows
everywhere, the flag is *never required* — so it cannot become reflex. Under the ADR's
literal reading (`anchored_through_seq < chain_len` ⇒ refuse) **every** Defense in-place
restore refuses today, and a flag that must always be typed is muscle memory inside a
week — pasted into the runbook, never read again, and by the time anchoring rolls out it
has stopped carrying information. That is the failure mode the drift avoids, and
ADR-0116 Phase 3 already says the Defense refusal *"waits on real anchor coverage
existing."*

The three-way split is the right shape: refuse when anchoring **is** running and this
snapshot is short (a real gap); warn loudly when it has not rolled out at all (a fact
about the system, not the snapshot); warn on pre-D4 not-recorded (refusing would make
every pre-D4 snapshot unrestorable). The `-1` / `Some(0)` / `None` sentinel discipline
behind it is careful and correct — and it is exactly the discipline **F4** breaks two
functions away.

The one weakness worth closing: on the `NoAnchorsAtAll` path the operator gets a
`tracing::warn!` on stderr and **nothing else**. No acknowledgement, and no record on the
restored chain. Recommend:

- surface the zero-anchor statement in the stdout completion message too, and
- record the anchor verdict in the `SnapshotRestored` audit payload, so the restored
  database itself carries *"this chain had no timestamp coverage at restore time."*

That is the fact a court would ask about, and right now it exists only in a log line
nobody keeps.

---

## 7. What the probes were

All probe sources are in the review worktree and are **not** on the branch:

- `crates/aberp-snapshot/tests/zz_adversarial_adr0116.rs` — `adv_a1`…`adv_a10`
- `apps/aberp/tests/zz_adversarial_journey.rs` — `adv_b1`…`adv_b6` (drive the real
  built `aberp` binary with `HOME` redirected into a scratch dir)
- gate mutations **M1**, **M5**, **M6**, **M7** — planted in throwaway `tar` copies and
  run against `tools/cut_gate_db_isolation.sh`

Results: `adv_a1`, `a3`, `a4`, `a5`, `a6`, `a7`, `b2`, `b4` pass (contracts hold, and
`a1` passing *is* the F2 finding). `adv_a2`, `a2b`, `a8`, `a9`, `a10`, `b1`, `b3`, `b5`
fail — each one a finding above. `adv_b6` is an informational probe that panics on
purpose to print the boot router's verdict. `M1` and `M5` escape the gate; `M6` and `M7`
are correctly caught.

**One correction made during the review, recorded because it changes the finding:**
`adv_b5` calls `recover_or_refuse` directly and therefore skips serve's routing
decision. `adv_b6` was written to close that gap and showed the shipped path is
`MirrorDivergedFromDb` → `RefuseFatal` (refuse to boot), not the `AutoRecover` →
silent-undo that `adv_b5` alone would have suggested. §1 reports both, with the shipped
one first.

Nothing outside the review worktree and the session scratchpad was touched: no
`~/.aberp-defense`, no `~/Documents/ABERP-snapshots*`, no `*CORRUPT*`/`*RECOVERY*`/
`*PRE-*`/`*.wal` artefact, no runbook. The branch was not modified, committed, or pushed.

---

## 8. What this review did not cover

- **Genuine crash injection.** No power-cut or block-device fault harness exists here, so
  F6 is asserted at code level (the durable primitive is absent from the path) plus the
  deterministic consequence (`adv_a10`), not by tearing a real write. This is the same
  honest scope the branch itself states for AC-1, and it is why F6 is ranked on the
  missing-primitive argument rather than on a reproduction.
- **Concurrency between the daemon and the D1.3 launchd floor.** F9's seq collision is
  demonstrated on constructed records, not raced in vivo; the TOCTOU in `next_seq` is
  read from the code.
- **RFC-3161 token verification.** Out of scope by the branch's own admission
  (ADR-0116 Phase 3); `anchor_coverage` only proves an anchor points at a head the
  snapshot really has, and says so.
- **The prod line.** F10 is reasoned and unit-pinned against the path shape; nothing in
  `~/.aberp`, `~/Documents/ABERP-snapshots*`, or any `*CORRUPT*`/`*RECOVERY*`/`*PRE-*`
  artefact was read or written.

