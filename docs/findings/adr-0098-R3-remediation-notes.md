# ADR-0098 v0.2.5 remediation — R3 (Fable-5 finding C)

**Branch:** `adr0098-remediation`, stacking on **R2** (`3a66698`).
**Batch:** third of the v0.2.5 remediation batch; R4 stacks on this branch.
**Scope (this session, R3 only):** close the **swap-orphan silent-write-loss**
vector — residual separate openers × the runtime checkpoint file-swap (R2's
`atomic_install` rename). Two parts, both landed here.

## Finding C (grep-verified at head 3a66698)
A residual, non-Handle `Connection::open` of the live tenant DB can (i) be
**orphaned** by the durable-checkpoint rename — a write on the about-to-be-
renamed inode vanishes; and (ii) **fold the shared WAL in place** on close
(duckdb#23046) while the Handle's instance is open. The audit's HIGHEST-priority
residual is **daemon-frequency**, not operator-paced: `ap_sync` →
`incoming_invoices::ingest_incoming_invoice` (its own `Connection::open` +
`Ledger::open`) at the ~2 s bootstrap-year backfill cadence, plus
`submission_queue`.

## Part 1 — no-in-place-fold pragma on EVERY residual runtime opener
`PRAGMA disable_checkpoint_on_shutdown;` (exact string reused verbatim from
`crates/aberp-snapshot/src/take.rs:208` / `aberp-db::open_runtime_connection`)
added to:
- **Central openers** (one change each; all their callers inherit it):
  `audit_ledger::Ledger::open` (`crates/audit-ledger/src/storage/mod.rs`) and
  `DuckDbBillingStore::open` (`modules/billing/src/adapters/duckdb_store.rs`).
  Each now sets the pragma on the connection it returns/holds.
- **Every RAW module-level runtime `Connection::open`** in the frozen-residual
  ledger — **51** sites across 22 files, one pragma line each, error-idiom
  matched per site (anyhow `.context` / custom `.map_err`). No API change; the
  only behaviour change is suppressing the on-close in-place fold.

This is mechanical + additive; the residual surface stays frozen for the full
v0.2.6 migration but can no longer silently fold-on-close.

## Part 2 — migrate the ap_sync ingest seam through the Handle
Routed onto the shared `AppState.db` Handle, EXACTLY the C2 pattern
(`db.write()` for the write + `append_in_tx`; `db.read()` + `from_connection`
for reads; WriteGuard drop runs the lockstep `sync_mirror`):
- `incoming_invoices::ingest_incoming_invoice` — signature `&Path` → `&HandleArc`;
  `Connection::open(:481)` → `db.write()`, `Ledger::open(:650)` mirror-sync →
  WriteGuard post-commit hook + `db.read()`+`from_connection` chain verify.
  Runtime callers threaded the Handle: `ap_sync` ingest loop + the serve.rs
  manual route. Tests route through a `#[cfg(test)]` Handle shim; the 4-thread
  concurrency test shares one Arc-cloned Handle.
- `submission_queue::count_pending` — `&Path` → `&HandleArc` via
  `db.read()`+`from_connection`; its three `issue-*` callers pass the Handle.

These openers come OFF the frozen ledger (now Handle-routed, gate-clean):
`incoming_invoices` 9→7, `submission_queue` 1→0.

## Part 3 — honest gate
- `tools/adr0098_c2_frozen_residuals.txt`: **139/31 → 136/30** openers/files; the
  delta (−3 openers, −1 file) equals exactly what Part 2 migrated (ingest 2 +
  count_pending 1). `(a-residual)` note updated to record the migration.
- **cut-gate CHECK 10j** (new): asserts every remaining frozen residual opener
  carries `disable_checkpoint_on_shutdown` — central `Ledger::open` +
  `DuckDbBillingStore::open` once, each raw `Connection::open` within 15 lines
  (cfg(test)/comment/string-aware via the shared scanner). 10i freezes the COUNT;
  10j freezes the SAFETY. Negative probes (teeth): pragma stripped from a frozen
  opener ⇒ RED; central `Ledger::open` pragma removed ⇒ RED; pragma-less
  `#[cfg(test)]` open ⇒ ignored (green).

## Gate results (in-sandbox, honest)
- `rustfmt --check`: clean across all 29 changed files.
- cut-gate `cut_gate_db_isolation.sh`: **PASSED** (10i 136/30; 10j green).
- negative probes: pre-existing probes pass; the three new 10j probes proven to
  have teeth (verified directly; the full tar-copy suite is I/O-bound in-sandbox).
- `rustc --test tools/adr0098_r3_pragma_presence_extract.rs`: **7/7 green** — the
  CHECK 10j scanner rule + the swap-orphan/ingest-routing invariant.

## FLAGGED conservative calls
- **Scope held to the finding's named 2 s residual.** Finding C names
  `ingest_incoming_invoice` (`:481`+`:650`) + `submission_queue` as THE
  daemon-frequency openers; those are migrated. The S197 XML-backfill reads/writes
  `incoming_invoices::get_nav_xml_path` / `set_nav_xml_path` are also ap_sync-
  reached but are conditional backfill, NOT the named 2 s ingest seam. They stay
  as **pragma-guarded** frozen residuals (Part 1 covers them; they cannot
  fold-on-close) and are recommended for the same Handle treatment in v0.2.6.
  FLAGGED (deliberately not expanded beyond the finding's enumeration).
- **Raw-site count is 51, not the task's ~32 estimate.** The frozen ledger's
  raw runtime `Connection::open` sites total 51 (the rest of the 136 residual
  openers are `Ledger::open`/`DuckDbBillingStore::open`, covered centrally). All
  51 carry the per-site pragma; the honest count is reported here and in 10j.
- **Test-Handle coexistence.** The `#[cfg(test)]` shim opens a real Handle over
  the fixture path while other test lines still path-open the same file — the
  same residual-coexists-with-Handle premise the ADR is built on. Runtime
  behaviour is exercised by `cargo test` on the Mac/CI gate.
- **CI/Mac-deferred:** full `cargo build`/`cargo test` + the swap-orphan-races-
  checkpoint e2e (a residual write racing `durable_checkpoint`, now pragma-guarded
  + ap_sync migrated) run at the batch end-of-chain 2-arm build-proof. In-sandbox
  we did rustfmt `--check`, the cut-gate CHECK pass + teeth, and the `rustc --test`
  extraction; a full crate compile was not run (no workspace toolchain + a full
  target dir exceeds sandbox scratch). FLAGGED.

**Do NOT start R4.**
