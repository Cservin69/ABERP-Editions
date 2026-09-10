# Adversarial re-review — ADR-0116 DB snapshot system, rev 3

**Branch** `feat/adr-0116-snapshot-system` @ `252e8b5` (pushed; off `origin/main` `5020773`)
**Prior verdict** BLOCKED @ `48ebc26` — see `docs/_adversarial-adr-0116-snapshot-review.md`
**Reviewed** 2026-08-29, in the existing worktree `~/Documents/Claude/Projects/ABERP-snap-wt`
**Scope** re-verify every claimed fix BY PROBE, then attack fresh. Gate before the **v0.6.4** cut.

---

## VERDICT: **FIX-FIRST** (2 findings, ranked)

The blocker is **genuinely closed** — `aberp serve` boots after a backwards in-place
restore, the chain verifies, the rollback survives, nothing is filed as `.CORRUPT-`.
All seven fix-firsts are closed and their pins are non-vacuous under mutation. The full
Editions gate is green with nothing de-gated.

But the fix that closed the blocker **removed the only signal that detected an
interrupted restore**, and the result is worse than the thing it replaced:

> `^C` during `aberp restore --in-place` now leaves a database that `aberp serve`
> **boots silently, empty**. Before this revision the same interruption made boot
> **refuse, loudly**. Reproduced end to end through the shipped binary with a real
> SIGINT (**F1**), and the pre-revision behaviour reproduced alongside it for contrast.

Neither finding is data loss — the `.PRE-RESTORE-` unit survives intact in both — and
neither is on the happy path. Both are cheap to fix. Nothing else found in this pass
rises above a note.

---

## 1. FIX-FIRST — an interrupted in-place restore boots SILENTLY EMPTY

**Reproduced end to end through the shipped binary, with a real Ctrl-C** (`p20`).

```
live before                  (40 005 invoices, 8 audit rows)
`aberp restore --in-place --confirm --accept-data-loss`   … ^C  (SIGINT, exit status 2)

on disk after the interrupt:
  aberp.duckdb.PRE-RESTORE-20260829T170712Z
  aberp.duckdb.PRE-RESTORE-20260829T170712Z.audit.log
  aberp.duckdb.PRE-RESTORE-20260829T170712Z.wal
  (no live aberp.duckdb, no live mirror)

`aberp serve --boot-check`   ->  boot-check: PASSED
live after boot              ->  (0 invoices, 0 audit rows)      <- an EMPTY company
```

The only log lines are `INFO`:

```
boot: tenant DB absent — provisioning atomically (ADR-0095 §2)
audit_mirror_recovered action=created (mirror file was absent) entries_written=0 db_max_seq=0
```

### The mechanism

`restore_in_place` (`take.rs:883`) renames the live DB aside as step 2, then moves the
WAL and — **new on this revision** — the mirror. The live path then has *nothing* until
`restore_into` finishes: `validate_export` + `IMPORT DATABASE` + `CHECKPOINT` +
`fsync` + `atomic_install`. On the 40 k-row fixture that window is seconds; on a real
tenant it is longer. It is reachable by a power cut, an OOM kill, or — as reproduced —
an operator pressing Ctrl-C on a restore that is taking longer than they expected.

Nothing journals the intent. `crash_safe::write_install_intent` /
`resume_pending_install` exist and `serve.rs:1441` calls the resume at boot, but
`write_install_intent` still has exactly one non-test caller, inside
`durable_checkpoint` — the branch's own comment at `take.rs:637` says so. So boot reaches
`serve.rs:1480`:

```rust
if !args.db.exists() {
    tracing::info!(db = %args.db.display(), "boot: tenant DB absent — provisioning atomically (ADR-0095 §2)");
```

and provisions a fresh, empty tenant. `grep -n "PRE-RESTORE" apps/aberp/src/serve.rs`
returns nothing: the boot path has no idea a restore was in flight.

### Why this is a regression, not a pre-existing hazard

The window itself pre-dates rev 3 — the DB rename was already there. What changed is the
**outcome**, and it changed in the dangerous direction. `p13` runs the identical
interruption with the mirror left at the live path (rev 2's rule):

```
rev 2 shape (mirror stays):
  ERROR audit_mirror_AHEAD_of_db — REFUSING to auto-truncate; preserved the ahead mirror
        mirror_max_seq=9 db_max_seq=0 entries_ahead=9
  ERROR REFUSING to boot — ahead mirror could not be safely auto-recovered
  Error: audit-ledger mirror is AHEAD of the DB at boot (possible lost DB commit …)
  `aberp serve --boot-check` -> ok=false
```

**The left-behind mirror was what caught this.** Moving it into the preserved unit closed
the blocker and, in the same stroke, deleted the detector. The implementation notes even
state the property that makes it silent, as a feature:

> *"an ABSENT mirror is the ONE disagreement the boot path resolves safely by itself
> (`RecoveryAction::Created`), so a failure there degrades to 'the next boot writes it',
> never to 'the next boot refuses'."*

That is correct for a *completed* restore and exactly wrong for an *interrupted* one, and
on disk the two are indistinguishable — which is the same sentence, one level down, as
the rev-2 finding about a diverged mirror being indistinguishable from a rollback.

### Scope, stated honestly

- **No data is lost.** The `.PRE-RESTORE-` unit is intact, complete (DB + WAL + mirror)
  and protected evidence. Everything needed to recover is on disk.
- It is a crash/interrupt window, not the happy path. A completed restore is fine (`p1`).
- `p14` shows the neighbouring window (crash between the DB rename and the WAL move)
  behaves the same way, and that the orphan `.wal` left at the live path is correctly
  consumed by the provision — the WAL half is clean.
- The failure is **silent**, which is what makes it worse than the refusal it replaced.
  An operator who Ctrl-Cs a slow restore, sees `boot-check: PASSED`, and starts serve is
  looking at an empty company with no error anywhere.

### Minimal fix

`serve.rs`'s provisioning branch must not fire while a `.PRE-RESTORE-*` unit is sitting
beside a missing database. Roughly:

```rust
if !args.db.exists() {
    // ADR-0116 D3.4 — a PRE-RESTORE unit beside a MISSING live DB is an
    // interrupted in-place restore, never a first launch. Refuse and name it.
    if let Some(unit) = find_pre_restore_sibling(&args.db) { bail!(…, unit.display()) }
    …
}
```

This needs no journal and no second source of truth — the marker *is* the preserved unit,
which cannot be lost independently of the thing it describes (the objection rev 3 raised
against the rollback-marker design). It is precise in both directions: after a *successful*
restore the live DB exists, so the branch is not taken; a genuine first launch has no
`.PRE-RESTORE-` sibling.

Then add the journey step that is missing for the same reason the boot step was missing
last round: **restore → interrupt → boot → assert the operator is told**, not `PASSED` on
an empty database.

---

## 2. FIX-FIRST — the F4 refusal names a flag that cannot work

`p17`, `p18`. The D3.3 gate refuses an unreadable live head with (verbatim):

> *"A database whose tables cannot be read is exactly the case a restore exists for … **Pass
> `--accept-data-loss` to proceed anyway**, having accepted that the amount of committed
> audit history discarded is unmeasurable."*

Pass it, and the command refuses again, further down, for a different reason:

```
WARN  ADR-0116 D3.3 — --accept-data-loss was passed: this restore will DISCARD committed
      audit entries  live_head=UNKNOWN … discarded=UNKNOWN
Error: ABORTING the in-place restore: the mandatory pre-restore snapshot of the CURRENT
      database failed validation (audit_ledger unreadable in snapshot: …)
```

`snapshot.rs:1775` bails whenever `!pre_snapshot.meta.valid`, and a database whose
`audit_ledger` is unreadable — or whose chain is broken — cannot produce a valid snapshot
by construction. `p18` confirms the realistic incident shape: a live DB with a **tampered
audit chain** (`hash-chain verification failed: tamper detected at seq=5`) is refused by
`restore --in-place` under **every** flag combination, including
`--accept-data-loss --accept-unanchored`.

The abort itself is defensible and deliberate — *"a database that cannot be snapshotted
cannot be safely replaced"*. The defect is the **operator contract**: the pre-flight
promises a flag that provably cannot succeed in that state, and neither message names the
command that does work.

**One does work.** `p19` — with the store passed:

```
`aberp snapshot restore <id> --to <side> --confirm`   -> ok, side counts (2, 3)
`aberp recover --db … --tenant … --store …`          -> RECOVERED, rebuilt from snapshot seq 1
live after                                            -> (2, 4);  boot-check PASSED
```

So the operator is not stranded — but they will only find that after typing a flag the
product told them to type, watching it fail, and reading a second refusal that also does
not mention `aberp recover`. At 02:00, on the path this whole programme exists to make
non-manual.

Aggravator, minor: each attempt writes a new **retained invalid** snapshot to the store
(`p18` produced seq 2 then seq 3). Correct by G8, but retrying flags accumulates junk and
burns seqs.

**Minimal fix, no behaviour change:**
1. In `build_preflight`, when the live head is UNKNOWN *because the DB is unreadable*,
   stop offering `--accept-data-loss` as the way forward and name `aberp recover` instead.
2. Add `aberp recover --db … --store …` to the step-2 abort message at `snapshot.rs:1776`.

---

## 3. Re-verification of every claimed fix — by probe, not assertion

| # | Claim | Verdict | Evidence |
|---|---|---|---|
| **BLOCKER** | serve would not boot after a backwards in-place restore | **CLOSED** | `p1`: rollback `(6,11) → (2,4)`, `boot-check: PASSED`, chain verifies from genesis (len 4), rollback survives the boot, **no `.CORRUPT-`**, preserved mirror present as `…PRE-RESTORE-<tag>.audit.log`. `p2`: a **forward** restore needs no data-loss flag, lands, and boots (chain 12). |
| **F2** | `re-verified` never read the installed DB | **CLOSED, pin non-vacuous** | `p10`: `validate_installed_db` tracks the FILE (3→5 invoices after mutating the DB, not the export) and rejects garbage bytes. **Mutation**: reverting the call site to `validate_export(export_dir, …)` turns `f2_in_place_restore_fails_loudly_when_the_installed_db_does_not_match` RED. |
| **F3** | `--snapshot 2` resolved to seq 24 | **CLOSED** | `p6`: store holding only seq 24 → `resolve_selector("2")` = `NotFound`; `"24"` resolves; the full identity resolves; an identity naming the **wrong `source_db_sha256`** does not resolve. The bare-integer form correctly refuses to fall through to the substring form. |
| **F4** | `-1` live head became a confident `0` | **CLOSED in the gate** | `p7`: unreadable `audit_ledger` → *"could NOT be read, so how much this restore would discard is UNKNOWN — not zero"*, refuses, never prints `EXACT … 0`. `p17`: with the flag it warns `live_head=UNKNOWN … discarded=UNKNOWN`. **But see §2** — the acknowledged path is unreachable. |
| **F5** | the gate compared the recorded `meta.audit_count` | **CLOSED** | `p7`: `meta.audit_count` inflated to `999999`, export untouched → the restore still refuses on the live number and the live DB is untouched. |
| **F6** | the snapshot WRITE path had zero fsyncs | **CLOSED, pin non-vacuous** | `take.rs:476-481` — `fsync_export_dir(partial)` (every file, then the dir) → `rename` → `fsync_dir(store)`; `meta.json` is written into `partial_dir` before the pass, so it is covered; `fsync_file` uses `sync_all` (which is `F_FULLFSYNC` on macOS). **Mutation**: deleting the `fsync_export_dir` line turns `f6_…fsyncs_before_it_publishes_the_rename` RED. |
| **F7** | CHECK 11d satisfiable by a DEAD guard | **CLOSED — by the TEST, as designed** | See §4. The scanner arm catches the shape it models; the behavioural test carries reachability and is load-bearing under four distinct neuterings. |
| **F8** | the scanner could not see `archive_then_remove`; a bare-import removal escaped | **CLOSED** | `archive_then_remove` and `cleanup_stale` now classify `TENANT_HOME` (by-file rule) and are in the frozen manifest. **Mutation MG**: a sweeper spelled `use std::fs::remove_file;` … `remove_file(p)` → `CUT-GATE: ✗ FAILED`, naming the site. |
| **self-reported** | `anchor_count` read off operator-writable `meta.json` | **CLOSED, and it says so out loud** | `p8`: `meta.json` edited to `anchor_count: 99, anchored_through_seq: 99`; the pre-flight prints `anchors  0 rows, NONE verified  (LIVE re-validation; meta.json RECORDS 99 rows … they DISAGREE, and the live number is the one that gates)`. |

### Anchor-sanction bypass hunt (the brief's item 3) — nothing further found

- `is_defense` is `build_profile::EDITION`, a **compile-time** constant — not reachable
  from any file, env var or flag.
- `anchor_verdict`, the warning, the stdout line and the audit-row field all read
  `anchor_verdict_live(&pf.live)` / `describe_anchors_live(&pf.live)`. Every `meta.*`
  read left in `snapshot.rs` is display-only, or `record.meta.valid`, which only ever
  *adds* a refusal (`!live.ok` is checked independently, so flipping it to `true` cannot
  admit a snapshot the live re-validation rejects).
- `--accept-data-loss` / `--accept-unanchored` have no non-flag path: both are read
  straight off `args`, and `p7`/`p18` confirm the refusals hold with the flags absent.
- **The remaining limit, stated:** the export directory itself has **no integrity binding**
  — `meta.json` records `source_db_sha256` of the *live source DB*, not a hash over the
  export. An operator who edits the anchors parquet can downgrade `ShortCoverage` (refuse)
  to `NoAnchorsAtAll` (warn), or forge `FullCoverage`. This is not a regression and not a
  new hole — it is the offline-unverifiability the branch already declares for RFC-3161
  (Phase 3) — but it means "`meta.json` is evidence, not authority; the export is the
  authority" rests on an authority nothing signs. Worth a sentence in the ADR.

---

## 4. CHECK 11 / F7-F8 — fresh mutations, planted and run

Every mutation below was planted in a throwaway `tar` copy and run against the real
`tools/cut_gate_db_isolation.sh`; each plant was diffed against the base first, so a
no-op plant cannot read as a pass.

| Mutation | Gate | Behavioural test |
|---|---|---|
| **MA** `if false && is_protected_evidence(…)` (their M1) | **RED** — `DEAD_GUARD`, 11d + 11e both fire | `f7` RED |
| **MB** `if !true && is_protected_evidence(…)` | GREEN | **`f7` RED** |
| **MC** `let never = false; if never && …` | GREEN | **`f7` RED** |
| **MD** `if !is_protected_evidence(…)` (sense inverted) | GREEN | **`f7` RED** |
| **ME** `let _ignored = is_protected_evidence(…); if false {` | GREEN | **`f7` RED** |
| **MI** the same neutering inside `guarded_remove` itself | GREEN | **`ac6` RED** |
| **MG** sweeper via `use std::fs::remove_file;` (their M5) | **RED**, names the site | — |
| **MF** sweeper via `use std::fs::remove_file as rm;` | GREEN | none |
| **MH** destroy-in-place: `std::fs::write(p, b"")` over a tenant home | GREEN | none |
| **correct fix** `let _ = guarded_remove(&p)` alongside a removal | **GREEN** (11f pins it, `expect_pass` probe pins it) | — |

**The F7 design holds.** The scanner models three shapes; four further neuterings walk
past it — but every one of them is killed by
`f7_prune_refuses_a_protected_directory_and_does_not_report_it_removed`, and neutering the
other guarded function is killed by `ac6_guarded_remove_refuses_evidence_and_permits_a_live_transient`.
Those two tests cover **both** GUARDED functions in the tree
(`retention::prune`, `evidence::guarded_remove` — the scanner reports exactly 3 GUARDED
sites across 2 functions), which answers the implementation notes' own challenge #10.
The load-bearing pin is genuinely load-bearing: it is RED under four independent
mutations, not one.

The `let _ = guarded_remove(..)` idiom stays **green**, verified two ways — the 11f
in-gate fixture and the `expect_pass` negative probe. A gate that reddened the correct fix
would get switched off; it does not.

**Two spelling gaps remain (note, not fix-first).** `MF` (an *aliased* import) and `MH`
(destruction by truncation rather than unlink) are invisible to the scanner. Nothing
in-tree spells either way; both are the same "gate bans ONE spelling" class the branch
just widened for — it widened one spelling short. Cheapest close: extend the `use`-alias
capture and add `File::create` / `fs::write` with an empty payload to the matcher, or say
explicitly in the awk header that truncation is out of model.

**One harness asymmetry.** `expect_fail` calls `assert_planted`; `expect_pass` does not.
A no-op plant on a non-trigger probe would silently degrade to the sanity check. The one
`expect_pass` CHECK-11 probe appends with `printf >>` so it cannot no-op in practice — but
the guard is one line and the asymmetry is exactly the BSD-sed class already on record here.

---

## 5. Core contract — attacked, and what held

| Attack | Result |
|---|---|
| backwards in-place restore → boot → parity + chain from genesis | **clean** (`p1`) — counts, chain, rollback survives, no `.CORRUPT-` |
| forward in-place restore → boot | **clean** (`p2`) — needs no data-loss flag, lands, boots, chain 12 |
| the mirror move FAILS mid-preserve (destination blocked by a non-empty dir) | **clean** (`p4`) — `Err(IsADirectory)`, DB rolled back **byte-identical**, live mirror byte-identical, boot passes. The rollback leaves no `.PRE-RESTORE-` DB or WAL behind (the only `PRE-RESTORE`-named path left is the probe's own blocking directory). The mirror is moved *after* the DB and WAL, so a failure there rolls all three back |
| is the moved mirror protected, paired, never orphaned? | **clean** (`p5`) — `is_protected_evidence` true, `guarded_remove` refuses it, named exactly `mirror_path_for(preserved_db)` so it pairs with the preserved DB by the same rule as the WAL; the fresh live mirror carries the restored chain (5 entries) |
| can a restore still silently auto-revert a rollback and file the restored DB as `.CORRUPT-`? | **closed** (`p1`, `p13`) — with the mirror moved, the `MirrorAheadOfDb → AutoRecover` route is unreachable after a restore; `p13` shows it firing *only* when the mirror is left behind |
| `(seq, created_at, source_db_sha256)` selector picking a mismatched pair | **clean** (`p6`) — a wrong-sha identity does not resolve; a bare integer is a seq and only a seq |
| a `*.partial` export presented as restorable | **clean** (`p9`) — invisible to `list_snapshots` and to all selector forms |
| retention evicting protected evidence / emptying the store | **clean** (`p11`, `f7`) — `keep_last:1` pruned [1,2,3], kept [4], the planted evidence directory survived; seq derivation gave 5, no duplicate |
| `--dry-run` writes something (rev 3 added a live re-validation + an in-memory IMPORT) | **clean** (`p16`) — live DB, WAL, mirror and the whole sibling set byte-identical after a dry-run |
| durable-ack / fsync reaching disk | **clean** — `f6` pin non-vacuous; ADR-0110 D3 fault-injection e2e green; `durable_ack` gate + its 10 probes green |
| **the step below `--boot-check`'s cut** — the ADR asks whether anything there can break | **clean** (`p15`) — `open_tenant_handle` (shared `Handle` + eager tenant schemas, a `?` at `serve.rs:2029`) opens fine on a restored DB. Note the notes call everything below the cut *"about talking to the outside world"*; this step is not, so the framing understates the cut point by one call. The empirical answer is still clean |

**One inaccurate operator-facing claim.** The completion message ends: *"a FRESH mirror was
written from the restored chain, so the next `aberp serve` boot has nothing to reconcile."*
It always has one thing to reconcile: the CLI appends `SnapshotRestored` **after**
`restore_in_place` wrote the mirror, so the mirror is one entry behind on every run. `p1`:
`action=extended (mirror was behind DB) mirror_max_seq=3 db_max_seq=4 entries_added=1`.
`Extended` is the safe direction and nothing is wrong — but the sentence is false on every
successful restore, and "a call-site comment that is false" is a class already on this
branch's record (PR #41). Either reword it or write the mirror after step 7.

---

## 6. Full gate — GREEN, nothing de-gated

Run in `~/Documents/Claude/Projects/ABERP-snap-wt` @ `252e8b5`, `--features production`,
`--locked`, `ABERP_TEST_PYTHON` pointing at the provisioned `aberp-cad-extract` venv.

| Gate | Result |
|---|---|
| `cargo fmt --all -- --check` | **clean** |
| `cargo build --workspace --locked --all-targets` | **clean** |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | **clean** |
| `cargo test --workspace --locked --no-fail-fast` | **205 test binaries, 3 460 tests, 0 failures** |
| ADR-0098 Handle e2e / ADR-0105 lock-domain e2e / ADR-0110 durable-ack fault injection | **PASSED / PASSED / PASSED** |
| Edition-isolation + crash-safe named suites | **PASSED** |
| `ADR-0093 DB-isolation cut-gate` (CHECK 1–11, all ENFORCED) | **PASSED** — 29 removal sites classified, 3 guarded, 9 frozen tenant-home sites |
| `ADR-0111 checkpoint-site cut-gate` | **PASSED** |
| `ADR-0110 D3 durable-ack gate` + probes | **PASSED** — census 6, all PROPAGATE; *"the gate has teeth"* |
| `cut_gate_negative_probes.sh` (all 77, incl. the 17 CHECK-11 arms) | **PASSED — `probes passed: 77   broken/escaped: 0`**, `NEGATIVE-PROBES: ✓ ALL CHECKS HAVE TEETH` |
| `cargo deny check` (+4 drift lints) | **advisories ok, bans ok, licenses ok, sources ok** |

**The two `aberp_cad_extract` reds did not occur** — both `step_extract_smoke` and
`hole_mining_failure` ran and passed with the venv provisioned, confirming they are
environmental and not code.

**The `serve_numbering_route::put_preserves_identity_and_bank_sections` flake is
pre-existing and out of scope**, verified independently rather than taken on assertion:
`unique_tmpdir()` keys its scratch directory on `pid` + `SystemTime` nanos with no
per-test counter, its file's last commit is `b0cdb2e` (2026-05-30, S165), and
`git log origin/main..252e8b5 --name-only` shows this branch touches no numbering file.
It **passed** in this run.

No enforcement flag was disabled, no check skipped, no `continue-on-error`.

---

## 7. What this review did not cover

- **Genuine block-device crash injection.** F1 is reproduced with a real SIGINT, which is
  the reachable trigger; a torn write mid-`EXPORT DATABASE` is still asserted at code
  level (F6's primitive) rather than by tearing a device write. Same honest scope the
  branch states for AC-1.
- **A real `aberp serve`.** Every boot assertion goes through `serve --boot-check`, plus
  `open_tenant_handle` called directly (`p15`) to cover the one DB-side step below the
  cut. Spawning a real serve reads the operator's actual OS keychain on a Defense build
  and can block on an ACL prompt after a rebuild; it was deliberately not done.
- **Environment note, unrelated to the branch.** The live v0.6.3 Defense stack
  (`run_defense.sh` 99015 / `aberp-ui` 99493 / `aberp serve` 99497) was running when this
  review began and is **no longer running**. It went down **cleanly at 18:21:08** and was
  not stopped by this review: `~/.aberp-defense/defense/` has **no `.wal`**, and its
  `aberp.duckdb.ckpt-ok` marker (`created_at_unix: 1788020468`, `byte_size: 8925184`)
  matches the database file exactly — the fingerprint of a durable checkpoint plus a clean
  close, not a kill or a crash. The only signal this session sent was a `SIGINT` to its own
  `cargo test` child in `p20`, 46 minutes later, addressed by the pid returned from
  `Child::id()`; no pattern kill was ever used. Every probe ran with `HOME` redirected into
  a scratch directory, so the `.aberp-defense` path each one touched was
  `/var/folders/.../aberp-advrev3-*/.aberp-defense/…`, never the real tenant home. **Ervin
  will want to restart it** — flagged here because a v0.6.4 cut assumes prod is up.

- **The prod line.** F10 from the previous round (the prod-shaped store reading as wholly
  protected) is unchanged and still latent. Nothing under `~/.aberp-defense`,
  `~/Documents/ABERP-snapshots*`, or any `*CORRUPT*` / `*RECOVERY*` / `*PRE-*` / `*.wal`
  artefact was read or written, and the live v0.6.3 Defense backend (pid 99497) was never
  signalled or disturbed.
- **F9 / F10 / F11 and the three notes** the branch consciously deferred are unchanged and
  were not re-attacked; the deferral reasoning (lower severity, no data-loss path on this
  edition, keep the blocker diff narrow) still reads as right.

---

## 8. What the probes were

`apps/aberp/tests/zz_adv_rev3.rs` — `p1`…`p20`, driving the built `aberp` binary with
`HOME` redirected into a scratch dir. Written as an **untracked** file in the worktree and
**deleted afterwards**; the branch was not modified, committed or pushed, and
`git status --porcelain` is empty. Gate mutations MA–MI were planted in throwaway `tar`
copies under the session scratchpad. Test-level mutations (MB/MC/MD/ME/MI, the F2 call
site, the F6 fsync) were applied to the worktree one at a time and reverted with
`git checkout --` immediately after each run.

`p1`, `p2`, `p4`, `p5`, `p6`, `p9`, `p10`, `p11`, `p12`, `p15`, `p16`, `p19` pass — each
one a contract holding. `p3`, `p13`, `p14`, `p20` are the F1 evidence. `p7`, `p8` confirm
F4/F5 and the anchor fix. `p17`, `p18` are the F2-of-this-round evidence.

---

## 9. The bar for a clean verdict

Both findings are small, local, and message- or guard-shaped. Fix them and this lands:

1. refuse to provision when a `.PRE-RESTORE-` unit sits beside a missing live DB, and pin
   it with a **restore → interrupt → boot** journey step;
2. stop offering `--accept-data-loss` where it provably cannot succeed, and name
   `aberp recover` in both refusals.

Everything else on this branch — the crate layer, the evidence guard, the preserve unit's
ordering and rollback, the D3.1 WAL-first install, the F6 write-path durability, the
selector, the two live-re-validation fixes, and the layered scanner-plus-test design
behind CHECK 11 — held up under everything this pass could throw at it.
