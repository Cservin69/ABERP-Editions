# ADR-0098 v0.2.5 remediation — R2 (Fable-5 finding B)

**Branch:** `adr0098-remediation`, stacking on **R1** (`855426f`).
**Batch:** second of the v0.2.5 remediation batch; R3/R4 stack on this branch.
**Scope (this session, R2 only):** eliminate the in-place WAL fold hiding inside
`durable_checkpoint` (`crates/aberp-snapshot/src/crash_safe.rs`) via a **journaled
install-intent + boot-resume** protocol — an ordering / crash-safety fix, NOT a
cadence change and NOT a naive pragma.

## Root cause (finding B)
`durable_checkpoint`'s EXPORT connection was a plain `Connection::open(db_path)`
with **no** `disable_checkpoint_on_shutdown` pragma, so on drop DuckDB folds the
WAL **IN PLACE** on the live file — the exact `duckdb#23046` write locus, inside
the very primitive meant to prevent it. The sibling snapshot-EXPORT site
(`take.rs:208`) got the pragma at review **F6**; `crash_safe.rs` was the paired-
validator miss. The in-place fold was **LOAD-BEARING**: it truncated the WAL
before the rename so `atomic_install`'s WAL-delete was safe. A **naive** pragma
alone opens a **double-replay** window: a crash between the rename and the WAL
removal leaves a fresh main file beside a foreign WAL that the next boot replays
into the rebuilt file.

## The fix — journaled install-intent (`durable_checkpoint` re-ordering)
0. capture a live-WAL **fence** baseline (presence + size) — Bug-2 belt;
   **i.** the EXPORT connection sets `PRAGMA disable_checkpoint_on_shutdown;`
   (exact string shared with `take.rs:208` + `aberp-db`'s `open_runtime_connection`)
   so its drop never folds the live WAL; validate the export (import + smoke +
   hash-chain), abort untouched if invalid; IMPORT + CHECKPOINT into a private
   staging `*.duckdb`;
  **ii.** fsync the staging file, then write an **fsync'd** `<db>.install-intent`
   journal (staging path + staging SHA-256 + target path) **BEFORE** the rename;
 **iii.** `atomic_install` renames staging → live (atomic);
  **iv.** …and deletes the now-stale target WAL (both inside `atomic_install`,
   which already fsyncs the parent dir);
   **v.** clear the journal, then write the verified-good marker.

## Boot-resume (`serve.rs` `run()`, BEFORE any DuckDB open — `resume_pending_install`)
- **(a)** staging present + matches the journaled SHA ⇒ **complete** the install
  (rename + stale-WAL delete) then clear the journal;
- **(b)** staging gone + live already matches the journaled identity ⇒ the rename
  happened ⇒ **delete the stale foreign WAL** + clear the journal — *this closes
  the double-replay window*;
- **(c)** neither reconciles ⇒ **preserve** evidence (`.install-intent.unreconciled-<tag>`)
  + **REFUSE** the boot (never guess; boot stays refused until an operator acts).
- No journal ⇒ `NoPendingInstall` (crash-after-staging-before-journal ⇒ the old DB
  and its own consistent WAL are left intact). Runs alongside R1's boot recovery.

## Bug-2 WAL-growth fence (defense-in-depth; the primary swap-orphan fix is R3)
Immediately before the swap, re-stat the live WAL; if it **grew / vanished /
shrank** since the EXPORT began ⇒ **ABORT** the checkpoint (evidence of a concurrent
unmigrated writer whose commits are absent from staging and would be destroyed by
the stale-WAL delete), leaving the live DB + WAL untouched. An empty WAL our own
read-only open may create (size 0 where there was none) is **not** a violation;
mtime is **not** compared — only a real size/presence delta counts, keeping the
fence free of self-perturbation false positives.

## Preserved unchanged
`checkpoint_is_current` fast-path + the `live_durable_checkpoint` debounce; no
publish-on-read / per-read checkpoint (banned). `atomic_install` is byte-identical
(CHECK 5 fixed-strings intact). `take.rs` is **untouched** — it already carries
the F6 pragma at `:208` and EXPORTs to the snapshot store (never an in-place fold);
no F6 / Gap-2b regression.

## Local gate (in-sandbox, honest)
- **`rustc --test` faithful extraction** (`tools/adr0098_r2_install_intent_extract.rs`):
  **8/8 pass** — pure decision table, WAL-fence table, and the crash points:
  after-staging-before-journal ⇒ NoPending / old-DB-intact; after-journal-before-
  rename ⇒ Complete; after-rename-before-WAL-clear ⇒ ClearStaleWal / **no double
  replay**; clean-completion ⇒ NoPending; unreconcilable ⇒ Refuse + preserve;
  fence-violation ⇒ abort.
- **`rustfmt --check`** (edition 2021): **CLEAN** on all four changed/new files.
- **`cut_gate_db_isolation.sh`**: **PASSED** (exit 0, 14 checks, zero ✗) — incl.
  CHECK 5 (build-aside + atomic rename, no in-place rewrite) and CHECK 10i frozen
  residual-opener ledger (139 openers / 31 files — **NO growth**). Independently
  proven: opener-scanner counts are EQUAL mine-vs-`855426f` (crash_safe 2=2,
  serve.rs 48=48) — the fix adds zero openers.
- **`cut_gate_negative_probes.sh`**: every reached probe ✓ caught, zero ✗; full
  completion is time-bounded by the 45s sandbox call cap (it re-runs the awk
  opener-scanner over the 1.4 MB `serve.rs` many times).

## CI / Mac-deferred (need the bundled-DuckDB build; land in the batch end-of-chain 2-arm build-proof)
- Full `cargo build`/`cargo test` of `aberp-snapshot` (the new in-crate
  `#[cfg(test)]` tests: `decide_resume_maps_the_four_cases`,
  `wal_fence_flags_growth_vanish_shrink_but_not_empty_appearance`,
  `install_intent_journal_roundtrips`, `resume_no_intent_is_a_noop`,
  `resume_completes_interrupted_rename_and_clears_stale_wal`,
  `resume_clears_stale_wal_when_rename_already_happened`,
  `resume_refuses_and_preserves_when_unreconcilable`) + `apps/aberp` against real DuckDB.
- The FULL crash-injection matrix on **real DuckDB**: `abort()` a child at each of
  the 5 protocol points → assert the live file is never torn and the next boot is
  clean with no foreign-WAL double-replay (extends `recover.rs:505`).
- The `serve.rs` boot-resume e2e (killed-mid-checkpoint child → next boot resumes
  → no double-replay).

## FLAGGED conservative calls
- **Fence lock scope:** the WAL-growth fence is defense-in-depth; the primary
  swap-orphan fix is **R3**. It is most meaningful under the shared Handle's
  single-writer lock the runtime callers hold, but `durable_checkpoint` opens its
  own EXPORT connection, so the fence is a best-effort stat comparison, not a
  lock-held invariant. FLAGGED.
- **Fence self-perturbation:** with the pragma set, our read-only EXPORT open
  should not touch the live WAL (the `take.rs` F6 precedent), but whether a
  large-WAL open can auto-checkpoint/fold is only provable on the real-DuckDB
  matrix (Mac-deferred). The fence is size/presence-only precisely so a benign
  open cannot spuriously abort; a spurious abort would only skip one checkpoint
  (live untouched — the safe direction).
- **Resume (a) leaves the marker:** case (a) completes the install but does NOT
  write the verified-good marker (matches the blueprint's "rename + delete stale
  WAL then clear journal"); the marker re-establishes on the next
  `live_durable_checkpoint` (`checkpoint_is_current` is false until then). Deliberate.
- **Resume wiring scope:** the boot-resume is wired into `serve.rs run()` (the
  serve boot path, before any DuckDB open). Other direct-open CLI subcommands
  (migrate/count/…) do not resume — they are separate processes / not the boot
  path, out of R2 scope; a shared pre-open guard is a candidate follow-up. FLAGGED.
- **Extraction fidelity:** the `rustc --test` extraction uses a std-only FNV-1a
  content hash + a 3-line journal as faithful analogues of the crate's SHA-256 +
  serde_json (the DECISION proven is byte-format-independent); the real serde_json
  journal + SHA-256 path is exercised by `cargo test` on the Mac/CI gate.
