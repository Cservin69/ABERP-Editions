# ADR-0098 v0.2.5 remediation — R4 (Fable-5 findings F + H)

**Branch:** `adr0098-remediation`, stacking on **R3** (`a09d8fe`).
**Batch:** FOURTH and LAST of the v0.2.5 remediation batch. Closes the batch;
next step is the batched 2-arm CI/Mac build-proof of R1+R2+R3+R4.
**Scope (this session, R4 only):** (F) the WriteGuard mutex-poison SPOF, and
(H) the cut-gate opener-scan blind spots. Both grep-verified at head `a09d8fe`.

## Finding F — WriteGuard mutex-poison is a process-wide SPOF (grep-verified)
`crates/aberp-db/src/lib.rs`: `write()`/`read()` (and the idle checkpoint) did
`self.inner.lock().map_err(|_| DbError::Poisoned)` — on a poisoned writer mutex
they returned `Poisoned` **forever**, with no `clear_poison`/`catch_unwind`. The
shared `Handle` (Gap 1a) made this a NEW single point of failure: one panic while
holding the `WriteGuard` (any of ~7 daemons or any handler) poisons the ONE
process-wide writer and bricks **every** write path until restart. (Pre-0098 a
panicking daemon hurt only itself.) The idle path additionally SWALLOWED the
poison silently (`let Ok(..) else { return }`).

### Fix — two complementary layers
**Layer 1 — poison policy in `aberp-db` (load-bearing, universal).** New
`Handle::lock_recovering()` routes every writer acquire (`write`, `read`,
`checkpoint_on_idle`) through recovery: on a poisoned mutex it calls
`Mutex::clear_poison()` (recover ONCE, not on every future acquire), reclaims the
guard via `PoisonError::into_inner()`, and runs `recover_from_poison`:
- **drop + reopen** the shared connection FRESH on the same live inode (a
  panicking holder may have left it mid-transaction); a reopen failure is a hard
  error;
- **post-poison integrity re-verify**: `Ledger::from_connection(try_clone)
  .verify_chain()` re-verifies the audit hash-chain **genesis→head**. A benign
  prior panic that left the DB CONSISTENT RESUMES; only a FAILED verify surfaces
  the new hard `DbError::PoisonRecoveryFailed` (real corruption — never served
  from a bad DB, never silently bricked);
- **loud log + audit event**: emits a `db.auto_recovered` audit row
  (`trigger="writer_poison_recovered"`) reusing the existing `EventKind` with a
  SCHEMA-VALID `DbAutoRecoveredPayload` (only its free-form `trigger` string
  carries a new value; the one variable is a `u64`, so the payload is
  hand-formatted — no `serde_json` dep, no decoder-shape risk). Best-effort: the
  mutex is already healed before the row is attempted.

**Layer 2 — daemon tick-guard in `apps/aberp` (containment + explicit boundary).**
New `apps/aberp/src/daemon_tick_guard.rs::guard_write_tick(tick, body)` wraps a
daemon's synchronous write-tick body (the `spawn_blocking` closure) in
`std::panic::catch_unwind(AssertUnwindSafe(..))`: a caught panic is sanitized
(CR/LF/NUL-stripped, bounded), logged loudly, and returned as an ordinary `Err`,
which the daemon loops already route into "log → next tick" (behaviour
preserved; the caught panic skips exactly that one tick). Applied to the
**email-relay drain daemon** — the ADR's named worst case (every 2 s,
unconditional, no gate) — at all 5 write-tick boundaries (claim / mark-sent /
mark-failed / requeue / startup-reconcile). The poisoning itself is NOT stopped
by the catch (the `WriteGuard` Drop already ran during unwind); Layer 1 is what
un-bricks the process, and the caught panic is AUDITED out-of-band by Layer 1's
`db.auto_recovered` row on the next `write()`. Write serialization is unchanged;
a caught panic never swallows the poison silently (must log+audit — it does both).

## Finding H — cut-gate opener-scan blind spots (grep-verified)
`tools/cut_gate_db_isolation.sh` + `tools/adr0098_opener_scan.awk`: (a) the scan
covered only `apps/aberp/src` + `modules` — a NEW separate opener in a crate
(`aberp-mes`/`-qa`/`-inventory`/`-work-orders`) was **invisible**; (b) the awk
matched the literal `Connection::open` token, so `use duckdb::Connection as C;
C::open(..)` **alias-evaded** it; (c) the freeze was by raw COUNT, so an
intra-file SWAP (drop 3 legit + add 3 bad = same count) stayed green.

### Fix
**(a) Scope → crates/.** CHECK 10i's scan root is now
`apps/aberp/src modules crates`, hard-excluding only the sanctioned seams
`crates/aberp-db/*` (the Handle's boot open, CHECK 10b/10c) and
`crates/aberp-snapshot/*` (boot/recover/snapshot/checkpoint primitives,
CHECK 5/10g). The extension SURFACED three previously-invisible crate openers,
now FROZEN in `tools/adr0098_c2_frozen_residuals.txt`:
`crates/audit-ledger/src/storage/mod.rs` (3 — the central `Ledger::open` +
`append_reopen` definitions), `crates/aberp-mes/src/ledger_writer.rs` (1 — a REAL
in-serve-process separate opener; FLAGGED v0.2.6 Handle-migration target),
`crates/aberp-inventory/src/bin/rebuild_stock_cache.rs` (1 — a CLI bin one-shot,
separate process). A new opener in a currently-clean crate (e.g. `aberp-qa`)
now trips 10i's "NEW unaccounted opener-bearing file".
**(b) Alias-evasion.** `adr0098_opener_scan.awk` now learns per-file aliases from
`use ... <Connection|Ledger|DuckDbBillingStore|Database> as <X>;` and matches
`<X>::open(_with_flags)?(`; `Database::open` added to the literal set. Still
cfg(test)-/comment-/string-aware and still excludes `open_in_memory` /
`from_connection` (incl. aliased `X::open_in_memory`).
**(c) Fingerprints — DONE (not deferred).** New CHECK **10k** freezes the SET of
per-opener fingerprints (`<file>|<fname>:<opener-text>`, line numbers dropped) in
`tools/adr0098_r4_opener_fingerprints.txt` (141 openers) across the same extended
scope. A count-preserving swap changes an opener's text/fn → set diverges → RED.
10i (count) is retained as the coarse backstop; 10k is the precise one.
**Scoping note:** CHECK 10j (pragma-presence, R3) keeps its `apps/aberp+modules`
scope — the crate residuals are frozen for COUNT (10i) + FINGERPRINT (10k) but
NOT pragma-enforced, to avoid forcing uncompilable pragma edits into business
crates this session. Residual fold-on-close risk for the `aberp-mes` ledger_writer
opener is FLAGGED (v0.2.6: migrate onto the Handle, which carries the pragma).

## Gate results (in-sandbox, honest)
- **cut-gate `cut_gate_db_isolation.sh`: PASSED** (exit 0). 10i now 141 frozen
  openers across 33 files (was 30; +3 crate residuals); 10j green; 10k green
  (fingerprint set matches baseline). No `✗`.
- **awk scanner:** no regression on the existing sweep; alias probe catches
  `C::open` / `DbAlias::open`, literal still caught, `open_in_memory` (incl.
  aliased) and cfg(test) correctly ignored.
- **Negative probes (teeth, verified directly — the full tar-copy suite is
  I/O-bound in-sandbox, same as R3):**
  - H·a: a planted `crates/aberp-qa` opener ⇒ RED ("NEW unaccounted opener-bearing
    file"). ✓
  - H·b: an aliased `use Connection as X; X::open` in `ap_sync` ⇒ RED
    ("Session-C2 regression", flagged the aliased line). ✓  and an aliased open
    inside `#[cfg(test)]` ⇒ GREEN (precision, no false-positive). ✓
  - H·c: a count-preserving opener swap ⇒ 10i COUNT stays green, 10k FINGERPRINT
    ⇒ RED ("opener fingerprint set DIVERGED"). ✓ (proves 10k catches what 10i
    cannot).
- **`rustc --test adr0098_r4_poison_and_scope_extract.rs`:** authored (9 tests /
  26 assertions) covering the poison-recover-and-reverify decision, the tick-guard
  invariant, and the scanner's new scope/alias rules. **The rustc RUN is
  CI/Mac-deferred** (no Rust toolchain in the saw-off sandbox — `rustc` absent,
  apt install is root-gated). The decision logic was independently validated via
  a Python mirror of the same 26 assertions: **26/26 green**. FLAGGED.

## CI/Mac-deferred (batch end-of-chain 2-arm build-proof)
- `rustfmt --check` — NO toolchain in-sandbox; produced rustfmt-clean code by
  construction (no trailing whitespace/tabs; brace/paren-balanced; long strings
  are un-splittable literals rustfmt leaves as-is). FLAGGED for the CI arm.
- Full `cargo build` / `cargo test` (the `duckdb` amalgamation does not compile in
  the sandbox — same gate as ADR-0095 chunk-3 / R3).
- The poison-recovery e2e: a panicked writer ⇒ the NEXT `write()` succeeds after
  clear_poison + integrity re-verify (and a `db.auto_recovered` row is emitted);
  and the email-relay tick-guard e2e: a tick panic is caught, the loop proceeds,
  the shared writer self-heals.
- `rustc --test` of the R4 extraction (logic pre-validated via the Python mirror).

## FLAGGED conservative calls
- **Poison-recovery audit reuses `EventKind::DbAutoRecovered`** rather than adding
  a new variant — adding an enum variant ripples into serialization/conformance
  arrays + the export-bundle classification, which is risky WITHOUT a local
  compiler. The payload is schema-valid (new `trigger` string only). If a
  dedicated `DbWriterPoisonRecovered` kind is preferred, it is a mechanical
  v0.2.6 follow-up. FLAGGED.
- **Recovery audit row records `binary_hash = 0`** (the Handle's existing
  placeholder-meta design; `sync_mirror` only reads `tenant_id`). The row is
  chain-consistent (verify recomputes over the stored zero); the attestation of
  *which* binary recovered is the only gap. FLAGGED.
- **Layer-2 tick-guard applied to email-relay only** (the ADR's named worst-case
  SPOF). The pricing daemon already has its `run_daemon_supervised` panic-catch
  (`quote.pricing_daemon_panicked`); the remaining daemons (email-outbox,
  catalogue-push, pdf-rerender, quote-intake, ap_sync) rely on Layer 1's
  universal poison-recovery meanwhile (no permanent-poison risk) and are
  recommended for mechanical `guard_write_tick` adoption in v0.2.6. FLAGGED.
- **CHECK 10j scope held at apps/aberp+modules** (not extended to crates/) — the
  surfaced crate openers (`aberp-mes` ledger_writer, `aberp-inventory` bin,
  `audit-ledger` append_reopen) are frozen for count+fingerprint but not
  pragma-enforced; the mes opener is an in-serve-process residual whose
  fold-on-close hardening is a v0.2.6 Handle-migration item. FLAGGED (residual
  risk noted in the manifest + here).
- **10k fingerprint brittleness (accepted):** a benign edit to an opener LINE
  itself requires refreshing `adr0098_r4_opener_fingerprints.txt`
  (`ENFORCE_OPENER_FINGERPRINTS=0` toggles it for a deliberate local probe). This
  is the precision/brittleness tradeoff the finding acknowledges; 10i's coarse
  count-freeze remains as the robust backstop.

This CLOSES the v0.2.5 remediation batch (R1+R2+R3+R4). **Do NOT start R5** — the
next step is the batched 2-arm CI/Mac build-proof.
