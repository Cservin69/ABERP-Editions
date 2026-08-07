# ADR-0105 (Defense) — The wrapper-hidden write-fork gate, and one serialization domain for every audit append

- **Status:** **Proposed — implemented, not yet adversarially reviewed.**
- **Date:** 2026-08-07
- **Deciders:** Ervin Áben (scope: close the two durability follow-ups the PR #33 adversarial flagged as their own workstream; conservative option where ambiguous; no AskUserQuestion; open a PR, do **not** merge). Investigation + implementation by Dispatch.
- **Base:** Editions `main` @ `9723df3` (PR #33 — the MES ledger-writer durability fix). Every file:line and every result below was reproduced in this session at that SHA.
- **Related:** **ADR-0098 Gap 1a** (the ONE shared `aberp_db::Handle`); **ADR-0099** + cut-gate **CHECK 10M** (the write-fork model this extends); **ADR-0104** / PR #33 (moved the MES writer onto the Handle — the last *in-domain* fork); **ADR-0087 / ADR-0088** (the DÁP/QES session + anchor chain, whose boot path is the live trigger here); S335 §3.4 (the standing cross-PROCESS limitation, unchanged).

---

## 0. TL;DR

Two gaps, one root cause and one loaded gun.

| # | Gap | Was it real? | Status |
|---|---|---|---|
| 1 | **CHECK 10M is blind to wrapper-hidden audit forks.** It only fires when the opener token and the append token sit in the *same* function body. | **Yes — and it was hiding live forks, including one in `serve.rs` where 10M-a demands a hard ZERO and was passing.** | New **CHECK 10N** (transitive taint closure) + 5 negative probes |
| 2 | **`append_in_tx` takes no lock; the Handle mutex and `AUDIT_APPEND_LOCK` are disjoint domains.** One writer in each forks the chain. | **Yes**, reproduced: `Chain(OutOfOrder { expected: 2, found: 1 })`. Pre-existing, **not** a PR #33 regression. Live trigger `dap_enabled`, default **off** — latent, not firing. | New `Handle::with_ledger`; both DÁP writers migrated |

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

The guard that actually works parks **inside** `with_ledger`'s closure, times **only** the `write()` acquisition from another thread, and runs with the checkpoint disabled: **417 ns vs 600 ms** under mutation. Recorded here because "mutation-verified" was claimed prematurely three times, and only the fourth framing earned it.

---

## 3. The audit-append call-site census

Every audit-append call site in runtime code (`apps/`, `modules/`, `crates/`, excluding `tests/` and `#[cfg(test)]`), classified:

| Class | Count | Meaning |
|---|---|---|
| **Handle-routed** | 101 | The appending fn takes `db.write()` itself. Serialized by the writer mutex. Safe. |
| **CLI one-shot (separate process)** | 1 in-scope (`drain_submission_queue::drive_one_invoice`) + the allow-listed `emit_reopen_cli`, `emit_tenant_reopen`, `run`, `seed_demo_sample_data`, `record_upgrade_snapshot_mismatch_audit` | No `serve` process, so no shared Handle to route through, so it cannot race a handle-routed writer. |
| **Unguarded in-process** | **2 — both CLOSED here** | `serve.rs::spawn_dap_audit_chain`, `audit_dap_boot::run_heartbeat_supervised`. |
| **In-tx appenders** (take a caller-supplied `&Transaction`) | the bulk of the ~200 `append_in_tx` sites | Inherit the caller's serialization; classified by their caller, which is Handle-routed or CLI. |

`drain_submission_queue` is reached **only** via `main.rs` → `cli::Command::DrainSubmissionQueue` → `drain_submission_queue::run`; it has zero references in `serve.rs`. It is frozen in `tools/adr0105_wrapper_fork_residuals.txt` rather than allow-listed, because the allow-list matches on fn *name* and an in-process fn later named `drive_one_invoice` must still fail the build.

**Not closed, and deliberately so:** cross-PROCESS races (a CLI subcommand run while `serve` is up) are outside *any* in-process lock. That is the standing S335 §3.4 limitation, backstopped by the hash chain's detection. This ADR does not change it.

---

## 4. Consequences

**Easier.** A wrapper-hidden fork now fails the build with the wrapper named (`via=<callee>`). The DÁP chain is no longer a second DuckDB instance on the live file, so enabling `dap_enabled` no longer arms both a fork hazard and a Gap-1a tear. `Handle::with_ledger` gives any future `Ledger`-shaped audit work one correct way to run.

**Harder.** CHECK 10N adds a second scanner to keep honest; its precision rules (Handle barrier, escape attribution) are heuristics over syntax, and a sufficiently unusual shape can still evade them — see §5. The heartbeat now blocks on the writer mutex once per interval (default 900 s), which is a real if negligible contention change.

**Locked in.** `HeartbeatDeps` carries a `HandleArc`, not a `db_path` — the DÁP boot path can no longer be constructed without a Handle. That is intentional.

---

## 5. Adversarial review

**"Your scanner is syntactic. Name a fork it still misses."** Several. A wrapper reached through a trait object or a function pointer (no resolvable callee name). A wrapper whose name is defined in >1 file where the same-file rule picks wrong — reported `AMBIGUOUS` rather than silently dropped, but only if *some* definition of that name is tainted. An opened connection stored in a struct field and appended from elsewhere (no escape attribution). CHECK 10N narrows the hole; it does not prove absence. The chain's own `verify_chain` remains the only *detection* backstop, unchanged.

**"You disabled the checkpoint in the exclusion test — are you testing production?"** No, and deliberately: with the checkpoint on, the assertion passed under mutation purely on checkpoint time. The test isolates lock-wait. Production posture is covered by the existing `handle_concurrency_e2e.rs` suite and by the end-to-end `verify_chain` race in the same file, which runs with defaults.

**"The `.write()` barrier token would also match `RwLock::write()`."** It would. It is only ever consulted on a definition that already reaches an append, so a lock-only `.write()` in a non-appending fn is inert. A fn that both takes an `RwLock` write guard and appends via an independent opener would be mis-cleared; none exists today (all 101 barrier sites were listed and reviewed), and CHECK 10M still covers the same-fn case independently.

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
