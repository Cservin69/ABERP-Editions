# Adversarial re-review — ADR-0116 snapshot system, rev 5 (pre-cut, v0.6.4)

**Target** `feat/adr-0116-snapshot-system` @ `c1e5770` (pushed, unmerged)
**Scope** targeted confirmation of the rev-4 fix round (F1 + F2), no-new-regression, the
stray `788ee85`, and the full Editions gate. **Not** a fresh full teardown — the branch has
already been through BLOCKED → fix → FIX-FIRST → fix.
**Method** a run-unique detached worktree at `~/Documents/Claude/Projects/ABERP-adv-rev5-…`,
never `/tmp`, never `~/.aberp-defense`, no signal ever sent to anything but this review's own
spawned children (by PID, never by pattern). The real Defense home was checked before and
after and is byte-identical. Nothing on the branch was modified, committed or pushed.

> **Note, added at close of review (2026-08-30 ~01:2x).** While this review's gate was
> running, a concurrent session pushed `6aba45e` — *"fix(snapshot): ADR-0116 rev 5 — the
> bare-relative --db blind spot + the dead-end refusal"* (5 files, +728/-43) — moving the
> branch tip off `c1e5770`. That commit claims to close exactly the two findings below.
> **It was not reviewed here.** Everything in this document is measured against `c1e5770`,
> which is the commit the brief named. The verdict stands for `c1e5770`; `6aba45e` needs its
> own confirmation pass before the cut — in particular that the bare-relative pin drives the
> CLI from inside the tenant home, and that the reworded refusal is pinned on substance
> rather than on whole-output `contains`, which is the vacuity this branch already paid for
> once (see §3, F2).

---

## VERDICT: **FIX-FIRST** — 2 findings, both message/one-line, ranked

The two rev-4 fixes are **real, and non-vacuously pinned** — F1 refuses a genuine `^C`-
interrupted restore through the shipped binary, F2's hint is the whole pasteable command and
both its pins redden when `recover_hint` is gutted. The full gate is green with nothing
de-gated. Neither finding below loses data or touches the shipped boot route.

But the guard added to close F1 has **one reachable blind spot** and its refusal **names two
recovery routes that cannot run in the state it fires in** — F2's own defect class, in the
message the same commit introduced. Both are small; both are on the 02:00 path this whole
programme exists for. On this branch's own bar (F2 was a FIX-FIRST for exactly this), they
are fix-first, not ship-and-file.

| # | Finding | Cost |
|---|---|---|
| 1 | a `--db` with no directory component blinds the detector, and one such boot **latches** the blindness permanently | ~4 lines + 1 test case |
| 2 | the F1 refusal names `aberp restore --in-place` and `aberp recover`; **neither can run** in an interrupted state, and the partial form of the third route boots an EMPTY company green | message only + 1 pin |

`788ee85`: **reword** (or squash into `c1e5770`). Docs-only, confirmed; nothing depends on it;
its numbers independently reproduced. Its *subject line* is the problem.

---

## 1. FIX-FIRST — a bare-relative `--db` blinds the detector, permanently

`crates/aberp-snapshot/src/take.rs:759`

```rust
let Some(parent) = db_path.parent().filter(|p| !p.as_os_str().is_empty()) else {
    return Vec::new();
};
```

`Path::new("aberp.duckdb").parent()` is `Some("")`. The filter drops it, the function returns
empty, and `serve.rs:1505` reads that as *"no interrupted restore here"*.

**Probed, not argued** (release binary, isolated `HOME`, tenant-home-shaped path):

| probe | invocation | result |
|---|---|---|
| P6 | `cd <home> && aberp serve --db aberp.duckdb --boot-check`, one `.PRE-RESTORE-` unit beside it | **exit 0 — provisioned a fresh empty DB, `boot-check: PASSED`** |
| P6b | same home, `--db ./aberp.duckdb` (the clap default *shape*) | exit 1 — refuses correctly |

**The latch is the serious half.** After P6 the tenant home holds `aberp.duckdb` (empty) *and*
the intact unit. I then re-ran boot with the **correct absolute path**:

```
boot-check: PASSED — the tenant database at …/p6/.aberp-defense/defense/aberp.duckdb opened…
home: aberp.duckdb  aberp.duckdb.PRE-RESTORE-20260829T170712Z  aberp.duckdb.audit.log  aberp.duckdb.ckpt-ok
```

Green. Because `args.db.exists()` is now true, the `if !args.db.exists()` branch is never
entered again and the detector is never consulted again — **for the life of that tenant home**.
One mistyped `--db` disarms the F1 guard for good, and the very next `run_defense.sh` boot
reports PASSED over an empty company. That is F1's harm, exactly, arrived at through the new
guard.

**Reachability, stated honestly.** No shipped route hits it: clap's default is `./aberp.duckdb`
(safe), `run_defense.sh`/`run_portable.sh` export absolute paths, and `apps/aberp-ui`'s
`ABERP_DB` fallback is `"./aberp.duckdb"` (safe). It needs a hand-typed bare filename — from an
operator standing in the tenant home at 02:00, which is precisely who this guard is for.
`ensure_db_path_isolated` does **not** stop it: it is a deny-list for foreign edition roots, not
an allow-list, and a bare relative name passes it (verified — P6 got all the way to
provisioning).

**Second site, same idiom, same file:** `take.rs:1010`, the parent-dir `sync_all` that makes the
preserved unit durable before the install overwrites the live path, is behind the identical
`.filter(|p| !p.as_os_str().is_empty())`. A bare-relative `restore --in-place` silently skips
that fsync.

### Minimal fix

```rust
// take.rs:759 — a `--db` with no directory component ("aberp.duckdb") has a
// parent of `""`. Returning empty there made the detector blind on exactly
// that spelling, and one boot LATCHES the blindness: the DB then exists, so
// this branch is never reached again.
let parent = match db_path.parent() {
    Some(p) if !p.as_os_str().is_empty() => p,
    _ => Path::new("."),
};
```

Same at `take.rs:1010`. Pin it in the journey suite, not only the crate test — the crate test
cannot express it without a `cwd` change, and the CLI is where it bites:
`cd <tenant home> && aberp serve --db aberp.duckdb --boot-check` must refuse.

---

## 2. FIX-FIRST — the F1 refusal names three routes; two cannot run, and the third has a silent-empty partial form

The refusal (`serve.rs:1509-1536`) tells the operator:

1. *"move the unit and its siblings back onto `<db>` (stripping the `.PRE-RESTORE-<tag>` suffix)"*
2. *"or re-run `aberp restore --in-place` to complete the restore that was interrupted"*
3. *"`aberp recover --db <db> --tenant <tenant>` rebuilds from the snapshot store if the unit itself will not open"*

I produced a **genuine** interrupted state — real `SIGINT` (`wait_status(2)`) delivered to the
shipped `restore --in-place` inside the preserve→install window, 40 005 invoices live — and ran
all of them.

| route | as the message spells it | result |
|---|---|---|
| **3** | `aberp recover --db … --tenant …` | **`REFUSED: no VALID snapshot in <DEFAULT store>`** — no `--store`, so it resolves the wrong store |
| **3′** | same **with** the correct `--store` | **`REFUSED (unsafe): audit-ledger mirror at <db>.audit.log is missing or unreadable`** |
| **2** | `aberp restore --in-place … --confirm --accept-data-loss` | **`Error: ADR-0116 D3.4 step 2 — mandatory pre-restore snapshot of the live database`** |
| **1-partial** | move the **unit only** back onto `<db>` | boot **`ok=true`**, **`counts=(0, 0)`** — an EMPTY company, `boot-check: PASSED` |
| **1-full** | move **all four** files back | boot ok, **`counts=(40005, 11)`** — fully recovered ✅ |

Three things follow.

**(a) Route 3 is structurally impossible, and rev 2 is why.** `recover_or_refuse_with_audit`
requires a readable mirror at `mirror_path_for(live_db)`. Rev 2 moved the mirror *into* the
unit to close the boot-after-rollback blocker. So in the one state this refusal fires in, the
live mirror is by construction absent and `aberp recover` refuses. The message offers it as the
fallback for *"if the unit itself will not open"* — the case in which it is least able to help.

**(b) Route 3 also drops `--store`.** `recover_hint` exists *because* "a hint that silently
pointed at the default one would rebuild from the wrong snapshots" (its own doc comment).
`serve.rs` does not call `recover_hint`; it hand-rolls a two-flag string. Probe A returned
`no VALID snapshot in …/Documents/ABERP-snapshots-defense/defense` — the default store, not the
one the restore was running against. This is F3's finding and F2's fix, both bypassed at the
one call site added after them.

**(c) Route 1's partial form is the F1 harm itself.** The unit path is the **only** path the
message prints; the siblings are named generically (*"with its .wal, .audit.log and .ckpt-ok
siblings"*). An operator who moves the file that was named gets a database whose every
un-checkpointed commit is still in the orphaned `.wal` — the tree's own F4 comment says *"Every
`Handle` commit is WAL-only until a checkpoint"* — and boot then prints **PASSED** over
`(0, 0)`. Verified twice, on two independent SIGINT-produced states.

None of this loses data: the unit is intact and protected evidence, and the full move-back
recovers everything (40 005 / 11). The defect is the operator contract — which is the entire
content of F2, in the message F2's own commit added.

### Minimal fix — message only, plus one pin

```
To recover, move the WHOLE unit back — all four files. Moving only the database
leaves its un-checkpointed commits behind in the orphaned .wal and the next boot
comes up as an EMPTY company:

    mv <unit>             <db>
    mv <unit>.wal         <db>.wal
    mv <unit>.audit.log   <db>.audit.log
    mv <unit>.ckpt-ok     <db>.ckpt-ok

then run `aberp serve` again.

Neither `aberp restore --in-place` nor `aberp recover` can run before that move:
the first needs a live database to take its mandatory pre-restore snapshot of,
the second needs the live .audit.log mirror — and both are inside the unit until
you move it back.
```

The message already holds `unit`, so the four concrete paths cost nothing. If a hint is kept,
route it through `recover_hint` so it carries `--store`. Pin the substance (the four
destinations present; `restore --in-place` / bare `aberp recover` not offered as if they worked)
in `journey_an_interrupted_in_place_restore_refuses_to_boot_empty`.

---

## 3. Re-verification of the rev-4 fixes — by probe, not assertion

### F1 — the silent-empty-boot regression: **CLOSED** (with finding 1 above)

Independently reproduced end to end. A real `SIGINT` to the shipped
`aberp restore --in-place`, fired the instant the live path went missing:

```
PROBE1 before: invoices/audit = (300000, 6)
PROBE1 pre-restore live:        (300005, 10)
PROBE1 sigint_fired=true restore_status=ExitStatus(unix_wait_status(2))
PROBE1 disk after ^C: db_exists=false units=[…/aberp.duckdb.PRE-RESTORE-20260829T221010Z]
PROBE1 home: [ …PRE-RESTORE-…Z.audit.log, …PRE-RESTORE-…Z, …PRE-RESTORE-…Z.wal ]
PROBE1 boot ok=false            <-- REFUSED
```

The refusal names the unit, says `INTERRUPTED`, does not print `boot-check: PASSED`, and
provisions nothing. Before rev 4 this state booted green and empty.

**Attempts to defeat the detector — all held** (release binary, one tenant home per case):

| planted beside a missing `aberp.duckdb` | must | got |
|---|---|---|
| nothing (genuine first launch) | provision | ✅ exit 0, DB created |
| `.PRE-DEDUP-…`, `.audit.log.PRE-RECONCILE-…`, `.CORRUPT-BACKUP-…`, `.PRE-DEFORK-….bak`, `healed-….bak` | provision | ✅ exit 0 — no false refusal from another incident family |
| `other.duckdb.PRE-RESTORE-…` (different db, same home) | provision | ✅ exit 0 |
| `aberp.duckdb.PRE-RESTORE-<tag>` | refuse | ✅ exit 1 |
| two units | refuse, note both | ✅ exit 1, `NOTE: 2 PRE-RESTORE units are present` |
| **partial** unit — only `.wal` | (fail-safe) | ✅ exit 1 |
| **partial** unit — only `.audit.log` | (fail-safe) | ✅ exit 1 |
| **bare-relative `--db`** | refuse | ❌ **exit 0, provisioned** — finding 1 |

*Minor, not ranked:* on a partial unit the refusal still prints *"the previous database is
INTACT at `<unit>`"* naming a file that does not exist. Refusing is the right call; the sentence
is not true in that case. One `if unit.exists()` would fix it.

**Mutations — the pins are load-bearing** (each plant diff-checked; a no-op plant is reported,
never counted):

| mutation | `f1_the_pre_restore_detector…` | `journey_an_interrupted…` | `journey_a_clean_first_boot…` |
|---|---|---|---|
| detector returns `Vec::new()` (under-match) | **RED** | **RED** | GREEN ✅ (control) |
| prefix drops the db-name (over-match) | **RED** | — | — |
| `serve.rs` refusal made unreachable | — | **RED** | — |

This confirms the rev-4 notes' claim of three RED mutations, and adds the control the notes did
not state: the clean-first-boot pin stays GREEN under the under-match mutation, so the two pins
are independent rather than one behaviour asserted twice.

### F2 — the dead-end refusal: **CLOSED, and the pins are NOT vacuous**

- `recover_hint` carries `--db`, `--tenant` **and** `--store`; `aberp recover --help` confirms
  all three flags exist, so the string is genuinely pasteable, not merely plausible.
- **No guard weakened.** The diff is message text plus one helper: `refusals.push(…)` still
  pushes, `anyhow::bail!` still bails. Probed: the damaged-DB case still refuses, still says
  `UNKNOWN`, and the live DB is untouched afterwards.
- **The hint is effective, not just runnable** — I ran the exact pasted command on both fixtures:

| fixture | hinted command | after |
|---|---|---|
| `DROP TABLE audit_ledger` | `ok=true` | `(30, 6)`, and the DB then **boots** (`boot-check` ok) |
| tampered chain (`UPDATE … WHERE seq = 3`) | `ok=true` | `(30, 7)` |

- **Mutation:** gutting `recover_hint` to return the bare string `"aberp recover"` turns **both**
  `journey_the_damaged_db_refusal_names_recover_not_an_impossible_flag` and
  `journey_the_pre_restore_snapshot_abort_names_recover` **RED**. The re-pin in `cd80cf5` did the
  job; the vacuity the rev-4 notes self-report is genuinely gone.

### No new regression from the fix round

The whole code delta since the last-reviewed `252e8b5` is **three source files** (`serve.rs`
+57, `snapshot.rs` +63, `take.rs` +80), tests, docs, and `tools/adr0116_evidence_removal_scan.awk`
+29 — the last of which is **comment-only** (every added line begins `#`). CHECK 11 classifies
**29 removal sites / 3 guarded / 9 frozen tenant-home sites**, identical to rev 3; the scanner
change moves no verdict, as the notes claim.

| earlier finding | status |
|---|---|
| BLOCKER — serve would not boot after a backwards in-place restore | holds (journey suite green; full 4-file move-back probe recovered `(40005, 11)`, chain intact) |
| F2 `re-verified` read the export not the file · F3 selector · F4 `-1`→`0` · F5 recorded count · F6 fsyncs · F7 dead guard · F8 scanner blind spots | all pins green in the workspace run; CHECK 11a-f green; the F7 behavioural pin `f7_prune_refuses_a_protected_directory_and_does_not_report_it_removed` passes |
| retention never deletes protected evidence | green (CHECK 11 + `f7…` + `ac6_guarded_remove_refuses_evidence_and_permits_a_live_transient`) |

`SAW-OFF.md` +50 is an honest deferral of the two out-of-model removal spellings (MF aliased
import, MH truncation) with acceptance criteria that explicitly forbid closing them by widening
the regex. No enforcement flag disabled, no check skipped, no `continue-on-error`.

---

## 4. `788ee85` — confirmed docs-only; **reword** (or squash)

```
788ee85  Cservin69  Sat Aug 29 22:45:00 2026 +0200
fix(snapshot): ADR-0116 rev 4 — the interrupted-restore silent-empty boot + the dead-end refusal
 docs/ADR-0116-implementation-notes.md | 29 +++++++++++++++++++++++++++++
 1 file changed, 29 insertions(+)
```

- **Touches exactly one file**, `docs/ADR-0116-implementation-notes.md`. No code, no test, no
  gate script, no manifest.
- Its content is one hunk at line ~620: the **"Rev 4 measured results"** gate table.
- **Nothing at `c1e5770` depends on it.** `c1e5770`'s two hunks land at lines ~522 and ~556 —
  inside the F1/F2 sections written by `08dcc9c`, both *above* `788ee85`'s hunk.
- **Its numbers are no longer unvouched.** I reproduced them on this machine, independently:

| its claim | my run |
|---|---|
| 205 test binaries, 3 465 tests, 0 failures | **205 binaries, 3 465 passed, 0 failed, 2 ignored** ✅ |
| the two `aberp_cad_extract` reds do not occur with the venv | ✅ `step_extract_smoke`, `hole_mining_failure` both green |
| `serve_numbering_route` passed | ✅ green |
| CHECK 11: 29 sites / 3 guarded / 9 frozen | ✅ identical |
| `cargo deny`: advisories/bans/licenses/sources ok | ✅ identical |

**The problem is the subject line, not the content.** A docs-only commit carrying
`fix(snapshot): … the interrupted-restore silent-empty boot + the dead-end refusal` — verbatim
the subject of `08dcc9c`, the commit that actually contains the fix — puts the same fix in the
log twice. For a money-real release that is a real cost: `git log --oneline` reads as two fixes,
release notes generated from subjects double-count it, and a `git bisect` narrowing to `788ee85`
lands on a commit that changes no behaviour at all.

**Recommendation: reword to `docs(adr): ADR-0116 rev 4 — record the measured rev-4 gate run`, or
squash it into `c1e5770`** (same file, same intent, adjacent in time). **Do not drop it** — the
gate table is the release evidence, and it is now corroborated.

---

## 5. Full Editions gate — GREEN, nothing de-gated

Worktree `ABERP-adv-rev5-…` @ `c1e5770`, `--features production`, `--locked`,
`ABERP_TEST_PYTHON` absolute into a purpose-built venv
(`pip install -e 'python/aberp-cad-extract[step]'`).

| gate | result |
|---|---|
| `cargo fmt --all -- --check` | clean |
| `cargo build --workspace --locked --all-targets --features production` | clean |
| `cargo build --release --locked --features production -p aberp` | clean |
| `cargo clippy --workspace --all-targets --locked --features production -- -D warnings` | clean |
| `cargo test --workspace --locked --features production --no-fail-fast` | **205 binaries · 3 465 passed · 0 failed · 2 ignored** |
| ADR-0098 Handle e2e · ADR-0105 lock-domain e2e · ADR-0110 durable-ack fault injection | 0 · 0 · 0 |
| edition-isolation (`aberp` ×2, `aberp-snapshot` ×2), crash-safe checkpoint, mirror-ahead | all 0 |
| ADR-0093 DB-isolation cut-gate (CHECK 1-11, all ENFORCED) | **`CUT-GATE: ✓ PASSED`** |
| ADR-0111 checkpoint-site cut-gate | **`CUT-GATE: ✓ PASSED`** |
| SVG well-formedness lint + its negative probe | PASS / PASS |
| ADR-0111 checkpoint-site negative probes (`cut_gate_checkpoint_probes.sh`) | **`PROBES: ✓ PASSED`** |
| `cut_gate_negative_probes.sh` (77) | **`probes passed: 77   broken/escaped: 0`** · `NEGATIVE-PROBES: ✓ ALL CHECKS HAVE TEETH` |
| `cargo deny check --deny advisory-not-detected --deny license-not-encountered --deny license-exception-not-encountered --deny unmatched-skip` | **advisories ok, bans ok, licenses ok, sources ok** |

`serve_numbering_route::put_preserves_identity_and_bank_sections` — the rev-3 note's
pre-existing, out-of-scope flake — **passed** here.

No enforcement flag was disabled, no check skipped, no `continue-on-error`, no probe dropped.

---

## 6. What this review did NOT cover

- No fresh teardown of the parts rev 2/rev 3 already closed — those were re-verified by the gate
  and by the pins, not re-attacked from scratch.
- **Genuine power-loss** crash injection is still out of reach; the SIGINT probes here are the
  closest reachable analogue and are honest about that.
- The export directory still has **no integrity binding** (rev 3 §3's stated residual, unchanged).
  `meta.json` is evidence, not authority.
- The `--store` / `--tenant` matrix on `recover` was probed only for the two damaged-DB fixtures
  and the interrupted-restore state.

## 7. The bar for MERGE-READY

Both findings closed, each RED before its fix:

1. `find_pre_restore_units` and the `restore_in_place` parent fsync fall back to `Path::new(".")`,
   with a journey pin driving `--db aberp.duckdb` from inside the tenant home.
2. The F1 refusal prints the four concrete move destinations and stops offering
   `restore --in-place` / a store-less `aberp recover` as if they worked in that state, with the
   substance pinned in `journey_an_interrupted_in_place_restore_refuses_to_boot_empty`.

Then re-run the gate and cut **v0.6.4**. Nothing here needs a redesign; both are localised, and
the rest of the branch is in good shape.

## 8. What the probes were

All against the **release** binary built from `c1e5770` unless noted, each with an isolated
`HOME` and a tenant-home-shaped `--db`; the SIGINT probes ran as throwaway untracked test files
in the review worktree (`apps/aberp/tests/zz_adv_rev5_probe.rs`), removed before the gate run —
the worktree was `git status --porcelain`-clean for every gate step and for every mutation
revert.

- **P1** real `SIGINT` inside `restore --in-place`'s preserve→install window (300 005 invoices), then boot.
- **P2-P7** the eight detector discriminations in §3's table, one tenant home each.
- **P4** the three recovery routes the F1 refusal names, on a real SIGINT state (40 005 invoices).
- **P5** the full four-file move-back on a second, independent SIGINT state.
- **F2** the two damaged-DB fixtures, plus running the pasted `aberp recover` and booting after it.
- **MU1-MU4** four mutations, each plant diff-verified, each reverted; six test runs, all RED
  where expected, one control GREEN.
