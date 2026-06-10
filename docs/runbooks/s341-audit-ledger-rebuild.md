# Operator runbook — audit-ledger ART crash recovery (`aberp audit-rebuild`)

**Audience:** Ervin / prod operator. **Time to resolve:** ~10 min.
**Verified against:** `apps/aberp/src/audit_rebuild.rs` @ S341/PR-36 on main.
This is code-traced truth, not theory.

---

## When to use this

Use this runbook when **every audit-bearing action fails** — saving a material,
editing a partner, catalogue-push, even a clean shutdown — and the journal shows
a DuckDB ART crash. Symptom Ervin sees locally: *"save a material → backend 500 →
nothing saved."* The audit row is written in the same transaction as the state
change, so when the audit insert panics it rolls back the whole commit.

## How to detect it

Grep the ABERP journal / console for the ART allocator signature:

```bash
journalctl -u aberp 2>/dev/null | grep -E "FixedSizeAllocator|Prefix::New|ARTOperator|assertion failure within DuckDB"
# or, if you run aberp in a terminal, look for:
#   FixedSizeAllocator::New → Prefix::New → ARTOperator::Insert → ... → WriteToWAL
#   This error signals an assertion failure within DuckDB.
#   Error code 1: Unknown error code kind=...
```

If you see that stack, the on-disk ART secondary index of the `audit_ledger`
table is corrupt. The rows themselves are intact; only the index image is bad
(S332 proved fresh DBs never reproduce it, even at 1M rows).

## Pre-flight (do this first, every time)

1. **Stop ABERP.** The rebuild refuses to run while a serve process holds the DB
   (and DuckDB's single-writer lock is the backstop), so it must be down.
   ```bash
   sudo systemctl stop aberp        # or however prod is started
   ```
2. **Confirm nothing holds the DB:**
   ```bash
   lsof -- ~/.aberp/prod/aberp.duckdb     # should print nothing
   ```
3. The `aberp` binary takes its own timestamped backup before rebuilding, but a
   manual copy never hurts:
   ```bash
   cp ~/.aberp/prod/aberp.duckdb ~/.aberp/prod/aberp.duckdb.manual-bak
   ```

## Step 0 — try the free fix first (CHECKPOINT / VACUUM)

The cheapest possible recovery is DuckDB rebuilding the ART internally. It costs
nothing to try and ships zero code. With ABERP stopped:

```bash
duckdb ~/.aberp/prod/aberp.duckdb <<'SQL'
CHECKPOINT;
VACUUM;
SQL
```

Restart ABERP and try saving a material.

- **If the crash is gone** → you are done. Stop here. (Please tell the dev so the
  S341 findings doc can record that CHECKPOINT cleared it.)
- **If the crash persists** → CHECKPOINT re-serialised the same corrupt allocator
  state through the very `WriteToWAL` path in the crash stack (expected — see
  `docs/findings/s341-audit-ledger-art-rebuild.md` §3). Stop ABERP again and go
  to Step 1.

## Step 1 — dry-run (inspect, no mutation)

```bash
aberp audit-rebuild --tenant prod --dry-run
```

This opens the DB **read-only**, dumps + verifies the hash chain, and runs a
**non-destructive ART probe** against a *throwaway copy* of the DB. It prints
something like:

```
audit-rebuild DRY-RUN (~/.aberp/prod/aberp.duckdb):
  rows: 41234  seq_max: 41234  chain_verified: true
  ART CORRUPT — rebuild recommended (probe error: ...)
  Plan: DROP + CREATE (UNIQUE(seq)/UNIQUE(id) preserved) + reinsert 41234 rows verbatim + 1 AuditLedgerRebuilt marker.
  Re-run WITHOUT --dry-run to execute (a .pre-rebuild-<ts>.bak backup is taken first).
```

- **`chain_verified: true`** is the green light — the rows are intact and only
  the index is corrupt, which is exactly what a rebuild fixes.
- **`chain_verified: false`** → **STOP. Do not rebuild.** The ledger *data* is
  suspect, not just the index; a rebuild would faithfully re-index a tampered
  chain and mask the problem. Preserve the file and escalate to the dev.
- **`ART healthy — NO rebuild needed`** → the index is fine; the crash you saw
  is something else. Stop and re-diagnose.

The dry-run does not touch the DB (it opens read-only) — the file is byte- and
mtime-identical afterward.

## Step 2 — rebuild (the real thing)

```bash
aberp audit-rebuild --tenant prod
```

This:
1. Takes a backup `~/.aberp/prod/aberp.duckdb.pre-rebuild-<unix_ts>.bak`
   (+ `.wal` sidecar if one exists).
2. In one transaction: `DROP TABLE audit_ledger` (destroys the corrupt ART) →
   `CREATE TABLE` with the **identical** schema (`UNIQUE(seq)`/`UNIQUE(id)`
   **preserved** — never relaxed) → re-inserts every row **verbatim** → appends
   one `audit.ledger_rebuilt` marker as the last row → `COMMIT` → `VACUUM`.
3. Re-verifies the chain. **If the post-rebuild verify fails it loud-fails and
   names the backup to restore** — that must never happen, but if it does, see
   "If something goes wrong" below.

Expected output:

```
audit-rebuild OK (~/.aberp/prod/aberp.duckdb):
  rows: 41234 -> 41235  seq_max: 41234 -> 41235
  chain verified before: true, after: true
  took: 1843 ms
  backup: ~/.aberp/prod/aberp.duckdb.pre-rebuild-1718000000.bak
  UNIQUE(seq)/UNIQUE(id) preserved; AuditLedgerRebuilt marker appended as the last row.
  Restart ABERP and confirm a material save now succeeds.
```

`rows: N -> N+1` is correct — the `+1` is the `audit.ledger_rebuilt` marker that
records the rebuild in the chain itself.

### Flags
- `--db <path>` — override the DB location (default
  `~/.aberp/<tenant>/aberp.duckdb`).
- `--no-backup` — skip the backup. **Dangerous**; the backup is your only undo.
  Don't use it unless you have your own copy.

## Post-flight

1. **Restart ABERP:**
   ```bash
   sudo systemctl start aberp
   ```
2. **Verify the fix:** save a material in the UI (or POST a partner). It should
   now succeed — the audit insert no longer hits the corrupt ART.
3. **Confirm the marker landed:**
   ```bash
   duckdb ~/.aberp/prod/aberp.duckdb \
     "SELECT seq, kind FROM audit_ledger ORDER BY seq DESC LIMIT 1;"
   # → the top row's kind should be 'audit.ledger_rebuilt'
   ```
4. Once you have confirmed several days of clean operation, the
   `.pre-rebuild-<ts>.bak` (and any `.manual-bak`) can be deleted.

## If something goes wrong

The rebuild runs in a single transaction — any error before `COMMIT` rolls back
and leaves the original file untouched. If the **post-commit** verify ever fails
(it names the backup path in the error), restore it:

```bash
sudo systemctl stop aberp
cp ~/.aberp/prod/aberp.duckdb.pre-rebuild-<ts>.bak ~/.aberp/prod/aberp.duckdb
# if a sidecar was backed up, restore it under the DB's own .wal name:
[ -f ~/.aberp/prod/aberp.duckdb.pre-rebuild-<ts>.bak.wal ] && \
  cp ~/.aberp/prod/aberp.duckdb.pre-rebuild-<ts>.bak.wal ~/.aberp/prod/aberp.duckdb.wal
```

then escalate to the dev with the full `audit-rebuild` output.

## Background

- Why this can't just "drop the bad index": `UNIQUE(seq)` is the hash-chain fork
  guard; dropping it silently forks the tamper-evident chain (S335 proved this
  with a coherence probe). The rebuild **preserves** the constraints and
  regenerates the index from a clean image — see
  `docs/findings/s341-audit-ledger-art-rebuild.md`.
- The crash signature, all the audit producers it affects, and the design are in
  that findings doc and in `docs/findings/s332-duckdb-art-email-outbox.md`.
