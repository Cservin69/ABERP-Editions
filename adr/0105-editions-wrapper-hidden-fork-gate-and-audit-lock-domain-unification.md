# ADR-0105 (Defense) — The wrapper-hidden write-fork gate, and one serialization domain for every audit append

- **Status:** **Proposed — implemented; adversarially reviewed (PR #34, verdict *merge-after-fixes*).** The review's verdict on `Handle::with_ledger` (§2) was CLEAN — it survived panic-poison, deadlock/lock-inversion, production-posture chain integrity, and its own mutation check. Every finding was against the **scanner** (§5.0): **F1 is closed here**; **F2 and F3 remain OPEN** as a scoped gate-hardening follow-up. Not merged.
- **Date:** 2026-08-07
- **Deciders:** Ervin Áben (scope: close the two durability follow-ups the PR #33 adversarial flagged as their own workstream; conservative option where ambiguous; no AskUserQuestion; open a PR, do **not** merge). Investigation + implementation by Dispatch.
- **Base:** Editions `main` @ `9723df3` (PR #33 — the MES ledger-writer durability fix). Every file:line and every result below was reproduced in this session at that SHA.
- **Related:** **ADR-0098 Gap 1a** (the ONE shared `aberp_db::Handle`); **ADR-0099** + cut-gate **CHECK 10M** (the write-fork model this extends); **ADR-0104** / PR #33 (moved the MES writer onto the Handle — the last *in-domain* fork); **ADR-0087 / ADR-0088** (the DÁP/QES session + anchor chain, whose boot path is the live trigger here); S335 §3.4 (the standing cross-PROCESS limitation, unchanged).

---

## 0. TL;DR

Two gaps, one root cause and one loaded gun — plus three gate bypasses the adversarial review then found in the scanner itself (§5.0), one of which is closed here.

| # | Gap | Was it real? | Status |
|---|---|---|---|
| 1 | **CHECK 10M is blind to wrapper-hidden audit forks.** It only fires when the opener token and the append token sit in the *same* function body. | **Yes — and it was hiding live forks, including one in `serve.rs` where 10M-a demands a hard ZERO and was passing.** | New **CHECK 10N** (transitive taint closure) + 5 negative probes |
| 2 | **`append_in_tx` takes no lock; the Handle mutex and `AUDIT_APPEND_LOCK` are disjoint domains.** One writer in each forks the chain. | **Yes**, reproduced: `Chain(OutOfOrder { expected: 2, found: 1 })`. Pre-existing, **not** a PR #33 regression. Live trigger `dap_enabled`, default **off** — latent, not firing. | New `Handle::with_ledger`; both DÁP writers migrated |

**Post-review addendum.** The `with_ledger` fix was found clean. The review instead broke the *gate*: `from_connection` line-laundering (**F1**, closed here — it let a DIRECT fork pass the FULL gate in `snapshot.rs`, a zero-tolerance file), the bare-name allow-list (**F2**, open), and `RwLock::write()` poisoning the 10N barrier (**F3**, open). See §5.0.

The two gaps turned out to be **the same two call sites**: `serve.rs::spawn_dap_audit_chain` and `audit_dap_boot::run_heartbeat_supervised` each hold an independent `Ledger::open` whose append is hidden inside a session helper. Gap 1 is precisely *why* Gap 2 was never caught by the gate.

---

## 1. Gap 1 — CHECK 10M's blind spot

### 1.1 The model and its hole

`tools/adr0099_write_fork_scan.awk` reports a runtime fn containing **both** an independent live-DB opener **and** an audit append. That is a per-function, syntactic model. It cannot see:

```rust
fn tick()  { let mut l = Ledger::open(..); write_event(&mut l, ..); }   // opener, no append token
fn write_event(l: &mut Ledger, ..) { l.append_signed(..) }              // append token, no opener
```

Neither fn trips it. This is not a hypothetical shape — it is the **default** shape once anyone factors an append into a helper.

### 1.2 Reproduction (three independent instances, all at `9723df3`)

1. **Historical.** The pre-PR-33 aberp-mes writer hid its append in `write_mes_adapter_event`. Reintroducing exactly that shape on this tree: CHECK 10M output is **empty**; CHECK 10N reports `try_write_once:TRANSITIVE:via=write_mes_adapter_event`.
2. **Already documented.** The ADR-0099 manifest itself records a second miss: *"qc_inspection::record_manual_inspection — a SPLIT write-fork … that the per-fn scanner did not flag."* It was found by hand, not by the gate.
3. **Live, and worse than expected.** Both DÁP audit writers were invisible to 10M. One of them, `serve.rs::spawn_dap_audit_chain`, sits in the file 10M-a holds at **ZERO tolerance** — and 10M-a was reporting `✓ serve.rs request handlers — no in-process write-fork`.

Instance 3 is the finding that matters: a ZERO-tolerance check was green over a real violation.

### 1.3 Decision — add CHECK 10N; do not touch CHECK 10M

CHECK 10M keeps its exact semantics and its own frozen manifest. **CHECK 10N** is additive, backed by `tools/adr0105_wrapper_fork_scan.awk`: a whole-program **taint closure over function definitions**, crossing crate boundaries (the live case is `apps/aberp` calling into `crates/audit-ledger`).

Four design properties are load-bearing, each forced by a false result observed while building it:

| Property | Why — the failure it fixes |
|---|---|
| Taint is per **definition**, resolved **same-file first**, then by corpus-unique name | A first draft keyed taint on the bare *name*, unioning the callees of all ~200 fns named `new`. One tainted `new` poisoned the corpus and every opener reported `via=open` — because `Connection::open(` itself yields the callee token `open`. |
| A callee that takes the **shared Handle** stops taint | Post-ADR-0099, migrated seams keep an independent `Connection::open` for the **business** row while the audit append goes through `db.write()`. Without this barrier all 101 migrated seams report as forks. |
| `.append(true)` / `.append(&mut ..)` are excluded from both the append test **and** the callee list | `mirror::sync_mirror` opens the sidecar with `OpenOptions::append(true)`. That token made `sync_mirror` a "tainted appender" and flagged every route that syncs the mirror. |
| The opened value must **actually reach** the tainted callee | The dominant benign shape is a post-commit `Ledger::open(..).sync_mirror(..)` in a fn whose append runs elsewhere. `pickup_quote_as_draft_request` passes its opened ledger to a **read** helper while appending on the Handle guard. |

Unresolvable calls are reported as `AMBIGUOUS` and frozen, not guessed and not swallowed. A non-converged closure exits 3 and is treated as a **harness fault**, never a pass.

Result on `main`: **6 → 1** record, and the one remaining is a sanctioned separate-process CLI one-shot.

---

## 2. Gap 2 — two disjoint serialization domains

### 2.1 The hazard

| path | lock held |
|---|---|
| `Handle::write()` + `append_in_tx` | the handle's writer mutex (`append_in_tx` takes **none**) |
| `Ledger::append` / `append_signed` / `append_reopen` | audit-ledger's `AUDIT_APPEND_LOCK` |

Neither excludes the other. Both read the head, both take `seq = head + 1`; the `UNIQUE(seq)` ART is gone, so both commit and the chain forks.

**Reproduced** (`crates/aberp-db/tests/audit_lock_domain_e2e.rs`), one writer per domain on ONE instance:

```
audit chain FORKED across the two lock domains: Chain(OutOfOrder { expected: 2, found: 1 })
```

The racing arm deliberately uses `Handle::read()` (a `try_clone` of the **same** instance) so the only variable is the lock domain — a separate `Connection::open` would drag in the Gap-1a two-instance tear and confound the result.

### 2.2 Decision — `Handle::with_ledger`, not a lock inside `append_in_tx`

Making `append_in_tx` take `AUDIT_APPEND_LOCK` was rejected: `Ledger::append` and `append_reopen` **already hold that lock** and then call `append_in_tx`. `std::sync::Mutex` is not reentrant, so that change self-deadlocks on the two most-used audit paths. It would require restructuring the lock to exactly one level across ~200 call sites — high blast radius for a latent bug.

`Handle::with_ledger(binary_hash, |ledger| …)` instead:

1. takes the writer mutex, and **holds it across the whole closure**, then
2. hands the closure a `Ledger` built from a **`try_clone` of the shared instance** — so there is still exactly one `Database` / one checkpoint actor (Gap 1a), and the head read inside is coherent.

Both properties are necessary. A clone alone does not help — the domains still interleave. Inside the closure `Ledger::append` still takes `AUDIT_APPEND_LOCK`, so this path holds **both** locks and excludes writers in either domain. Lock order is always handle-mutex → `AUDIT_APPEND_LOCK`; inversion is impossible because `aberp-audit-ledger` does not depend on `aberp-db` and so can never call back in while holding its lock.

This also leaves the ADR-0087/0088 session API (`&mut Ledger`) untouched.

### 2.3 A test that looked mutation-verified and was not

The first guard was the race + `verify_chain`. It passed. Then the mutex was mutated out of `with_ledger` — **and it still passed.** The race is probabilistic: a green run is equally explained by "the mutex works" and by "the interleaving didn't happen this time".

The second attempt asserted `with_ledger` *blocks while a handle writer holds the guard*. It also passed under mutation — `Handle::read()` takes the same mutex briefly to `try_clone`, so the mutated build blocks too.

The third attempt measured the right direction but included the guard's drop hooks (mirror sync + checkpoint) in the timing, which alone exceed the threshold; it passed under mutation for the wrong reason.

The guard that actually works parks **inside** `with_ledger`'s closure, times **only** the `write()` acquisition from another thread, and runs with the checkpoint disabled: **417 ns / 542 ns vs 600 ms** under mutation. Recorded here because "mutation-verified" was claimed prematurely three times, and only the fourth framing earned it.

**Then CI made the same point from the other side.** The retried race that asserted the hazard is *still demonstrable* (`unfixed_ledger_writer_still_forks`) passed on an 8-core dev box and **failed on the 2-core CI runner** — the unguarded race never interleaved in 6 attempts. A scheduler-dependent test is worthless as a guard in **either** direction, so the hazard proof was made deterministic as well:
[`two_unserialized_appenders_fork_the_chain`] uses no threads and no timing — two appenders with no shared serialization point, transactions ordered by hand, both reading the same head and both taking `seq = head + 1`. It forks on any core count. If it ever stops forking, its failure message says the PREMISE changed (DuckDB rejecting the second commit, or a `UNIQUE(seq)` returning) and this ADR must be revisited rather than the assertion relaxed. The thread race survives only as corroboration that the fixed path tolerates real contention.

---

## 3. The audit-append call-site census

Every audit-append call site in runtime code (`apps/`, `modules/`, `crates/`, excluding `tests/` and `#[cfg(test)]`), classified:

| Class | Count | Meaning |
|---|---|---|
| **Handle-routed** | **89** in the gate's scanned corpus; **101** over the full runtime corpus (see §3.1) | The appending fn takes `db.write()` itself. Serialized by the writer mutex. Safe. |
| **CLI one-shot (separate process)** | 1 in-scope (`drain_submission_queue::drive_one_invoice`) + the allow-listed `emit_reopen_cli`, `emit_tenant_reopen`, `run`, `seed_demo_sample_data`, `record_upgrade_snapshot_mismatch_audit` | No `serve` process, so no shared Handle to route through, so it cannot race a handle-routed writer. |
| **Unguarded in-process** | **2 — both CLOSED here** | `serve.rs::spawn_dap_audit_chain`, `audit_dap_boot::run_heartbeat_supervised`. |
| **In-tx appenders** (take a caller-supplied `&Transaction`) | the bulk of the ~200 `append_in_tx` sites | Inherit the caller's serialization; classified by their caller, which is Handle-routed or CLI. |

`drain_submission_queue` is reached **only** via `main.rs` → `cli::Command::DrainSubmissionQueue` → `drain_submission_queue::run`; it has zero references in `serve.rs`. It is frozen in `tools/adr0105_wrapper_fork_residuals.txt` rather than allow-listed, because the allow-list matches on fn *name* and an in-process fn later named `drive_one_invoice` must still fail the build.

### 3.1 Reconciling the Handle-routed count (89 / 101)

Three different numbers for this figure were in circulation — 89 (this ADR), 86 (the residual manifest) and 90 (the PR #34 review). Re-measured at this commit, with the command stated so it is reproducible rather than asserted:

```
FILES=(... find apps/aberp/src modules crates -name '*.rs' | grep -vE '/tests/' \
        [ | grep -vE '^crates/(aberp-db|aberp-snapshot)/' for the gate corpus ] ... )
awk -v allow="$WF_ALLOW" -v levels=12 -v show_barriers=1 \
    -f tools/adr0105_wrapper_fork_scan.awk "${FILES[@]}" 2>&1 >/dev/null | grep -c BARRIER
```

* **89** — the gate's corpus (excludes `crates/aberp-db` and `crates/aberp-snapshot`, the shared-Handle seams themselves). 89 records, 89 unique `file:fn`, 84 distinct fn names, across 30 files.
* **101** — the full runtime corpus, i.e. the same set plus those two crates.
* **86** — **stale**. It predates two scanner corrections in this PR (excluding `#[cfg(test)]` definitions from the resolution index, and keeping `OpenOptions::append(true)` out of the callee list), both of which changed the taint set. The manifest has been corrected to 89.
* **90** — **not reproducible here.** No corpus variant tried (gate corpus, either crate excluded singly, or including `/tests/`) yields 90; the closest are 89, 100 and 115. Recorded as unreconciled rather than silently rounded to the number that suited.

**Not closed, and deliberately so:** cross-PROCESS races (a CLI subcommand run while `serve` is up) are outside *any* in-process lock. That is the standing S335 §3.4 limitation, backstopped by the hash chain's detection. This ADR does not change it.

---

## 4. Consequences

**Easier.** A wrapper-hidden fork now fails the build with the wrapper named (`via=<callee>`). The DÁP chain is no longer a second DuckDB instance on the live file, so enabling `dap_enabled` no longer arms both a fork hazard and a Gap-1a tear. `Handle::with_ledger` gives any future `Ledger`-shaped audit work one correct way to run.

**Harder.** CHECK 10N adds a second scanner to keep honest; its precision rules (Handle barrier, escape attribution) are heuristics over syntax, and a sufficiently unusual shape can still evade them — see §5. The heartbeat now blocks on the writer mutex once per interval (default 900 s), which is a real if negligible contention change.

**CI budget.** Both workflow timeouts were raised on measured evidence (cut-gate 15 → 30 after a 14 m 32 s run against a 15-min cap; ci 60 → 90 after PR #33 ran 59 min against a 60-min cap and PR #29 was cancelled at exactly 60). The negative-probe harness costs ~17.5 s per probe and runs in BOTH workflows, so every probe added is paid twice. Speeding up `fresh()` is now load-bearing, not cosmetic.

**Locked in.** `HeartbeatDeps` carries a `HandleArc`, not a `db_path` — the DÁP boot path can no longer be constructed without a Handle. That is intentional.

---

## 5. Adversarial review

### 5.0 Known gate bypasses — F1, F2, F3

The PR #34 adversarial review found three ways to plant a **real in-process audit write-fork** and keep the gate green. All three are recorded here in full, because an earlier draft of this section listed only strictly weaker gaps and would have left a reader believing the gate was tighter than it is. Repro for all three: `tools/adv_pr34_gate_bypass_repro.sh` (from the review's worktree).

| # | Bypass | Severity, as MEASURED | Status |
|---|---|---|---|
| **F1** | **`from_connection` line-laundering.** Every opener scanner skipped any line containing the substring `from_connection`. The exclusion was LINE-scoped, so `Ledger::from_connection(Connection::open(p)?, ..)` hid a genuinely independent opener *as an argument on that same line*. | **Total bypass** — but not everywhere. Planted in `serve.rs` the gate still went red, because CHECK 10 (the serve.rs-specific live-path scan) never carried the clause. Planted in `quality.rs`, `crates/aberp-qa/`, or **`snapshot.rs` — which CHECK 10M-a holds at a hard ZERO** — the pre-fix gate passed **in full** (exit 0). | **CLOSED** by this ADR. Clause removed from 6 sites; proven a NO-OP on the clean tree (122 records byte-identical). Permanent probe `[ADR-0105 F1]` plants in `snapshot.rs` and asserts the 10M-a signature. |
| **F2** | **Allow-list matches a bare fn NAME.** The sanctioned list contains `run`, so *any* of the ~21 runtime fns named `run` — including `serve.rs::run`, the long-running process entry point, which is emphatically **not** a separate-process CLI one-shot — may open and append freely. | **Total bypass** in `serve.rs`, a DIRECT same-fn fork, gate green. Verified. | **OPEN — scoped follow-up.** Fix is to key the allow-list on `file:fn`, not bare name. Deliberately not attempted here: it touches CHECK 10M's allow-list, which this PR committed to leaving byte-identical. |
| **F3** | **`RwLock::write()` poisons the 10N taint barrier.** 10N stops taint at any callee containing `.write()` with empty parens, intended as "takes the shared Handle". `RwLock::write()` and tokio's `.write().await` match identically, so adding ONE unrelated lock line to an append wrapper silently clears the entire caller chain. | Defeats **CHECK 10N specifically** (the wrapper class this ADR exists for). The review's control case proves 10N catches the same fork without the lock line. | **OPEN — scoped follow-up.** Fix is to freeze the barrier set using the existing `-v show_barriers=1` machinery, so a NEW barrier must be reviewed rather than silently trusted. |

F2 and F3 are queued as a separate gate-hardening change at Ervin's direction; they are **not** fixed in this PR.

### 5.1 Other objections

**"Your scanner is syntactic. Name a fork it still misses."** Beyond F2/F3 above: a wrapper whose name is defined in >1 file where the same-file rule picks wrong — reported `AMBIGUOUS` rather than silently dropped, but only if *some* definition of that name is tainted; a wrapper reached through a **function pointer** (no callee name at the call site at all); and an opened connection stored in a struct field and appended from elsewhere (no escape attribution). CHECK 10N narrows the hole; it does not prove absence. The chain's own `verify_chain` remains the only *detection* backstop, unchanged.

**Correction — trait objects are NOT a blind spot.** An earlier draft of this section claimed a wrapper reached "through a trait object" was missed. That is wrong, and it understated the scanner. Callee extraction is receiver-agnostic: `s.zz_unique_sink_name(&mut l)` on a `&dyn AuditSink` yields the callee token `zz_unique_sink_name`, which resolves like any other name. Verified by planting exactly that shape — 10N reports `zz_dyn_caller:TRANSITIVE:opener@L9:via=zz_unique_sink_name`. Dynamic dispatch is caught whenever the method name is same-file- or corpus-unique; when several types implement the same method name it degrades to `AMBIGUOUS`, which is still **reported**, not dropped.

**"You disabled the checkpoint in the exclusion test — are you testing production?"** No, and deliberately: with the checkpoint on, the assertion passed under mutation purely on checkpoint time. The test isolates lock-wait. Production posture is covered by the existing `handle_concurrency_e2e.rs` suite and by the end-to-end `verify_chain` race in the same file, which runs with defaults.

**"The `.write()` barrier token would also match `RwLock::write()`."** It does — this is **F3**, and it is worse than the earlier draft of this ADR admitted. That draft argued the token "is only ever consulted on a definition that already reaches an append, so a lock-only `.write()` is inert", and then leaned on "CHECK 10M still covers the same-fn case independently". **That mitigation does not apply.** The whole point of the wrapper class is that the opener and the append are in *different* functions, which is exactly what 10M cannot see — so when F3 blinds 10N, nothing else covers it. One unrelated `RwLock` line in an append wrapper clears the entire caller chain, and the review demonstrated it against a control that 10N otherwise catches.

The barrier list is emitted on demand (`-v show_barriers=1`) and was read as a set — every entry is a recognisable ADR-0099-migrated audit sink (`*::append_event`, `write_*_audit`, `emit_*_audit`, `ledger_writer::try_write_once`), with no `RwLock`-shaped outlier **today**. It was not line-by-line audited across all 89, so this is a reviewed shape, not a proof, and nothing stops the next one. Freezing that set is the F3 follow-up.

**"You changed frozen baselines — is that a weakening?"** Two, both shrinks, both forced by removing openers: `adr0098_r4_opener_fingerprints.txt` loses the 2 migrated `Ledger::open` lines (100 → 98), and `adr0098_c2_frozen_residuals.txt` tightens `serve.rs` 30 → 29 and drops `audit_dap_boot.rs` entirely. Dropping the file's line is strictly *stronger*: a re-added opener there now fails as "NEW unaccounted opener-bearing file". No count was raised.

**"CHECK 10N-b freezes a set — so a future fork can just be added to the manifest."** Same objection as 10M-b, same answer: the manifest is may-only-shrink and is reviewed in the diff. The blind-spot probe additionally fails loudly if CHECK 10M ever *starts* catching this class, so the two checks cannot silently collapse into one.

---

## 6. Alternatives considered

**Make `append_in_tx` take `AUDIT_APPEND_LOCK`.** Rejected — non-reentrant self-deadlock via `Ledger::append` / `append_reopen`, which already hold it (§2.2).

**Hold the Handle guard around the *existing* `Ledger::open`.** Would serialize the domains but leaves a second `Database` instance on the live file — trading a fork hazard for the ADR-0098 Gap-1a tear.

**Refactor the session API to take `&Transaction`.** The clean long-term shape, but it touches every ADR-0087/0088 entry point and their anchor/signature tests for a latent, default-off bug. Deferred.

**Teach CHECK 10M itself to see through wrappers.** Rejected for this change: 10M is a load-bearing ZERO-tolerance gate on two files, and rewriting its model in the same PR that fixes a live fork would make a regression in it indistinguishable from the fix. Additive is auditable.

---

## 7. Open questions

- **Cross-process audit serialization** (a CLI subcommand appending while `serve` runs) remains open — S335 §3.4's "single serialized audit-writer actor across the process tree". Not addressed here.
- **`drain_submission_queue::drive_one_invoice`** stays a frozen residual. Migrating the CLI one-shots onto a per-process Handle is the natural follow-on and would take CHECK 10N-b to zero.
- **`append_reopen` is legacy** (zero live callers, per ADR-0104). Deleting it would shrink the append surface and the scanner's seed set; it stays only because CHECK 10M's negative probe is anchored on the token.
