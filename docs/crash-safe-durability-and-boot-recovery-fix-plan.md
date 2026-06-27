# Fix-session plan — editions crash-safe durability & boot auto-recovery (ADR-0095)

Companion to **ADR-0095**. Forensics + design only landed in this commit; the
code below is **sequenced, not implemented**. Scope is the **editions tree
only** — prod (`~/.aberp/prod`, tree `2d612811`) is frozen and untouched; the
crash-safe/snapshot crate already refuses prod paths (`ensure_not_prod_path`).

## Root-cause recap (what each session must close)

1. **Torn checkpoint** — corrupt active DB header (`iter=20`, `0x2000`) has
   `meta_block=0x0` → `Failed to load metadata pointer (id 0, idx 0, ptr 0)`;
   `duckdb#23046` family. Live writes use DuckDB's default in-place checkpoint.
2. **Wiring gap** — `durable_checkpoint` (`crash_safe.rs:222`) is sound but
   called only at clean shutdown (`serve.rs:2978` ← `snapshot.rs:104`). Not at
   boot, creation, periodically, or post-write.
3. **Non-atomic creation** — DB opened directly at the final path
   (`serve.rs:1143`), not temp+rename. (Atomic `write_atomic` exists for TOML
   only: `tenant_registry.rs:867`–`918`.)
4. **Fatal ahead-mirror + monolithic boot** — `MirrorAheadOfDb`
   (`mirror.rs:524`–`548`) → fatal `return Err` (`serve.rs:1399`–`1406`); no
   automated snapshot+replay recovery. SMTP already degrades (`serve.rs:472`–
   `480`); DB-durability failures do not.

## Sequencing rationale

**A → B → C.** B (boot wiring) depends on A's recovery engine + atomic-create
primitive. A is a pure-crate change, **fully locally verifiable** (incl.
plain-file crash-injection). B's integration/e2e needs the bundled libduckdb
1.5.3 build → **Mac-gated / CI**, mirroring chunk-3's existing gating. C is
docs only.

---

## Session A — `aberp-snapshot` recovery engine + atomic-create primitive (LOCAL)

**Where:** `crates/aberp-snapshot` (+ a thin replay helper in
`crates/audit-ledger`). No `apps/aberp` changes. Reuse `atomic_install`,
markers, `validate_export`, `durable_checkpoint` — do **not** re-build them.

**Scope**

- `recover_or_refuse(db_path, store_dir, mirror_path, tenant) -> RecoveryOutcome`
  implementing ADR-0095 §1: preserve evidence → pick latest valid snapshot →
  `IMPORT` to private staging → replay JSONL delta (`seq >
  snapshot.audit_count`, verbatim) → validate (chain genesis→head, heads match,
  `db_max_seq == mirror_max_seq`) → `atomic_install` + `write_marker`. Returns
  `Recovered{…}` / `RefusedNoSnapshot` / `RefusedUnsafe{reason}` — **never**
  truncates the mirror, **never** deletes the corrupt DB.
- `provision_atomic(db_path, init: FnOnce(&Path)->Result<()>)` implementing
  §2: build at `<db>.creating-<tag>.duckdb`, run `init` (all `ensure_schema` +
  genesis), `CHECKPOINT`, `atomic_install`, `write_marker`; `cleanup_stale`
  any orphan temp on entry.
- `live_durable_checkpoint(db_path, tenant)` thin wrapper for the daemon/
  post-write callers (no-op when `checkpoint_is_current`).

**Tests (all run locally; DuckDB-backed ones Mac-gated like chunk-3)**

- *Plain-file crash-injection (anywhere):* extend the existing
  `crash_safe` plain-file tests — kill between staging-write and rename in
  `provision_atomic` → **no file at the live path**; after rename → openable.
- *Torn-DB recovery (DuckDB, Mac-gated):* zero the active header `meta_block`
  of a built DB (reproduce the `id 0, idx 0, ptr 0` signature) → `recover_or_refuse`
  yields an **openable** DB == snapshot+replay; corrupt copy retained.
- *Ahead-mirror recovery (DuckDB, Mac-gated):* fresh empty DB + valid 64-entry
  mirror → rebuild from snapshot + **replay** → `db_max_seq==64`, chain
  reconciles `Unchanged`, mirror **not** truncated, `.ahead-*.bak` retained.
- *Unsafe/refuse:* no snapshot → `RefusedNoSnapshot`; broken mirror chain or
  head mismatch → `RefusedUnsafe`; assert **zero** mutation of live inputs.
- *Idempotent:* re-running recovery on an already-recovered DB is a no-op.

**Done when:** `cargo test -p aberp-snapshot` (+ `-p audit-ledger`) green
locally; Mac-gated DuckDB e2e green in CI; no `apps/aberp` diff.

---

## Session B — serve/boot wiring (INTEGRATION; e2e Mac-gated / CI)

**Where:** `apps/aberp/src/serve.rs`, `apps/aberp/src/snapshot.rs`,
`apps/aberp/src/audit_payloads.rs` (new `db.auto_recovered` payload), CLI.

**Scope**

- **Boot safe-open + auto-recover (§1):** before the schema chain
  (`serve.rs:1143`), attempt the live open; on torn-open **or** when
  `recover_audit_mirror` (`serve.rs:1381`) returns `MirrorAheadOfDb`, call
  A's `recover_or_refuse`. On `Recovered`, emit `snapshot.restored` +
  `db.auto_recovered` and continue; on `Refused*`, keep **today's**
  preserve-and-surface (`serve.rs:1399`–`1406` becomes the fallback, not the
  default). Demote the fatal path; do not delete the P1 guard.
- **Atomic creation (§2):** replace the first-create `DuckDbBillingStore::open(&args.db)`
  (`serve.rs:1135`–`1143`) with `provision_atomic` when the DB is absent; the
  rest of boot then opens the present, good file.
- **Live checkpoints (§3):** add `live_durable_checkpoint` to the daemon cycle
  (`run_supervised`, `snapshot.rs:475`) and a **debounced** post-issue hook in
  the invoice issue/storno/modification paths.
- **Boot robustness (§4):** add a unit table asserting every optional-config
  read degrades (extend `sanity_check_environment` tests); confirm no boot
  step `?`/`exit`s on SMTP/email/NAV-inbound/quote-intake (close the
  `SmtpSecurity::parse` residual at `smtp_config.rs:58`–`60` by asserting the
  `matches!` degrade pattern at every reader). Keep `exit(1)` only for
  prod-identity (`serve.rs:1031`) and creds-vanished sanity.
- **CLI:** `aberp recover [--db …]` / `serve --recover` — the single supported
  manual entrypoint (guarded, audited), replacing sidecar surgery.

**Tests**

- *Boot e2e (Mac-gated, CI):* (i) kill mid-`provision_atomic` → next boot
  opens clean; (ii) boot against a torn defense-class file → auto-recovers,
  corrupt retained, `db.auto_recovered` in the ledger; (iii) boot with fresh
  DB + ahead mirror → auto-replays, boots clean, mirror preserved, **zero**
  manual steps.
- *Unit (local):* optional-config degrade table; post-issue checkpoint
  debounce; `db.auto_recovered` payload round-trip.

**Done when:** the three Defense reproductions boot clean with no operator
action; full `cargo test` + clippy green; Mac-gated boot e2e green in CI.

---

## Session C — runbook + prod decision (LOCAL, docs only)

- Rewrite the corruption runbook around the single supported path (auto on
  boot / `aberp recover`); retire the hand-clear-the-sidecar instructions.
- Record the **prod backport** as a separate, explicit decision (out of scope
  now): prod stays frozen, protected by snapshot+ledger recovery; backporting
  §1–§3 to a future `PROD_vX` is its own ADR + change-window.

---

## Cross-cutting crash-injection acceptance test (the load-bearing proof)

A single named test that **kills the process mid-write** (mid-creation and
mid-checkpoint) and proves the next boot yields a **recoverable/openable** DB
with **no data loss and no manual step** — the property the whole ADR exists
to guarantee. Plain-file layer runs locally; the DuckDB/boot layer is
Mac-gated/CI (bundled libduckdb 1.5.3), consistent with chunk-3's SAW-OFF
gating.
