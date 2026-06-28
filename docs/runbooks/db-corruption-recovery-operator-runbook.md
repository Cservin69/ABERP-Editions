# Operator runbook — DuckDB corruption / torn-write / ahead-mirror recovery (editions)

**Audience:** Ervin / edition operator (Defense, Portable). **Time to resolve:** usually **0 min — the app fixes itself on boot**; ~2 min if you have to run the one manual command.
**Applies to:** the **editions** tree only (`~/.aberp-defense/…`, `~/.aberp-portable/…`). **Not prod** — prod has its own procedure (see [ADR-0096](../../adr/0096-prod-backport-of-editions-durability-hardening.md)).
**Verified against:** ADR-0095 as shipped in Sessions A + B —
`crates/aberp-snapshot/src/recover.rs`, `apps/aberp/src/serve.rs`,
`apps/aberp/src/snapshot.rs`, `apps/aberp/src/cli.rs`, `apps/aberp/src/audit_payloads.rs`,
`apps/aberp/tests/boot_crash_recovery_e2e.rs`. This is code-traced truth, not theory.

---

## The one thing to know

If the database is torn or out of sync after a crash, **`aberp serve` now repairs
it automatically the next time it boots**, with **zero manual steps and no lost
audit entry**. It does this by rebuilding from the last good snapshot and replaying
the audit-ledger. If — and only if — it **cannot prove the repair is safe**, it
stops and preserves everything, and you run **one** command:

```sh
aberp recover --db <path-to-aberp.duckdb> --tenant <tenant>
```

**You never hand-edit, move, or delete the sidecar files again.** The old
"clear `aberp.duckdb.audit.log` and the `.bak` by hand" surgery is retired and
must not be performed — doing so can destroy the only record of committed work.

---

## What the failure looks like

There are two surfaces of the **same** root cause (a checkpoint that was made
durable while the data it points at was not — DuckDB's in-place checkpoint,
`duckdb/duckdb#23046`, the torn-write / ART-checkpoint family). A crash, power
loss, or kill mid-write (especially mid first-launch creation) is what triggers it.

### Surface 1 — torn / unopenable database ("root cause #1")

The live file will not open. The signature error is:

```
INTERNAL Error: Failed to load metadata pointer (id 0, idx 0, ptr 0)
  in CheckpointReader::LoadCheckpoint
```

Plain meaning: the newest database header points at metadata block `0` (a hole),
so DuckDB aborts. The bytes that hold your data may be fine — it is the pointer
that is torn. **Do not** try to "repair" the file; it is preserved as evidence and
rebuilt from a snapshot.

### Surface 2 — "audit-ledger mirror is AHEAD of the DB" ("root cause #4")

Boot logs an error containing **`audit-ledger mirror is AHEAD of the DB`**. This
means the append-only ledger mirror (`<db>.audit.log`) records commits that the
database file no longer contains — typically because the live DB was lost and a
fresh/empty one took its place. The mirror is the proof those commits happened, so
the app **replays** them rather than throwing them away.

---

## What the app now does AUTOMATICALLY on boot

On every `aberp serve` boot, before the schema step, the app open-checks the DB and
reconciles the mirror. If it sees **either** surface above, it runs one guarded,
reversible engine (`aberp_snapshot::recover_or_refuse`, ADR-0095 §1). The steps,
in order:

1. **Preserve evidence — never destroy it.** The existing live file is **copied**
   (not moved) aside to `<db>.CORRUPT-<tag>`. The ahead mirror, if that was the
   trigger, was already copied to `<db>.audit.log.ahead-<nanos>.bak`. The mirror
   itself is **read, never truncated**.
2. **Find the latest VALID snapshot** in the edition-scoped snapshot store
   (ADR-0082 `meta.valid == true`). If there is none → it **refuses** (see below).
3. **Rebuild aside.** It `IMPORT`s that snapshot's logical export (corruption-free
   by construction) into a **private** staging file `<db>.recover-<tag>.duckdb` —
   never the live path.
4. **Replay the ledger delta.** Every mirror entry with `seq` greater than the
   snapshot's audit head is inserted verbatim, in order, into the staging DB.
5. **Validate the rebuild.** The hash-chain must verify genesis→head; the rebuilt
   head sequence must equal the mirror head; the rebuilt head `entry_hash` must
   match the mirror head. **Any** check fails → discard staging, **refuse**.
6. **Install atomically.** The validated staging file is `rename(2)`-swapped over
   the live path and a verified-good marker (`<db>.ckpt-ok`) is written.
7. **Record + continue.** A `db.auto_recovered` audit entry is written (actor
   `system:auto-recover`) and boot continues normally.

The two boot triggers are recorded as `trigger = "torn_open"` (surface 1) and
`trigger = "mirror_ahead"` (surface 2). A manual run records `trigger = "manual_cli"`.

**If recovery is NOT provably safe** (no valid snapshot, mirror missing/unreadable,
snapshot ahead of the mirror, chain won't verify, or heads disagree) the app does
**not** guess. It keeps the old, safe behaviour: **preserve everything and stop**,
and tells you to run `aberp recover`. Refusing is the last resort, not the only
outcome — but it is always available.

> First-launch note: a brand-new DB is now created **aside and swapped**
> (`provision_atomic`, ADR-0095 §2): the schema is built in
> `<db>.creating-<tag>.duckdb`, checkpointed, then atomically installed. A crash
> during first launch therefore leaves only a disposable temp (cleaned on the next
> boot) — **never a torn file at the live path**. This is what makes the
> 2026-06-27 Defense first-launch failure impossible to reach going forward.

---

## The evidence and sidecar files (leave them where they are)

| File | What it is | What to do |
|---|---|---|
| `<db>.duckdb` | the live database | nothing — the app manages it |
| `<db>.duckdb.CORRUPT-<tag>` | a **copy** of the torn DB, kept for forensics | keep; never delete until you've confirmed recovery |
| `<db>.duckdb.audit.log` | the append-only audit-ledger **mirror** — the second source of truth | **never** truncate or hand-edit |
| `<db>.duckdb.audit.log.ahead-<nanos>.bak` | a preserved copy of an ahead mirror | keep; it is evidence, not garbage |
| `<db>.duckdb.ckpt-ok` | verified-good marker (SHA-256 + size of the last good file) | leave it; it makes the next checkpoint a cheap no-op |
| `<db>.duckdb.creating-<tag>.duckdb` | disposable first-creation temp | ignore — auto-cleaned next boot |
| `<db>.duckdb.recover-<tag>.duckdb` | disposable recovery-staging temp | ignore — auto-cleaned next boot |

The default live DB path is `~/.aberp-<edition>/<tenant>/aberp.duckdb`
(e.g. `~/.aberp-defense/defense/aberp.duckdb`). The `.CORRUPT-`, `.recover-`, and
`.creating-` temps are distinct infixes, so cleanup of one never touches the
retained `.CORRUPT-` evidence.

---

## The ONE supported manual path — `aberp recover`

Use this only if boot **refused** (it told you to), or to recover a DB out of band.
It runs the **identical** guarded engine the boot path runs.

```sh
# Defense example (adjust edition/tenant/paths to your install):
aberp recover \
  --db     ~/.aberp-defense/defense/aberp.duckdb \
  --tenant defense
# --store <dir> is optional; it defaults to the edition-scoped per-tenant snapshot store.
```

Flags (from `apps/aberp/src/cli.rs`): `--db` (default `./aberp.duckdb`), `--tenant`
(default `default`), `--store` (optional; **refused if it points at the frozen prod
line**).

### What success prints

```
aberp recover — ADR-0095 §1 guarded, reversible recovery
  db:     …/aberp.duckdb
  store:  …/ABERP-snapshots-<edition>/<tenant>
  mirror: …/aberp.duckdb.audit.log
RECOVERED
  rebuilt from snapshot seq <S> (audit head <H>)
  replayed <N> mirror entries; recovered head seq <M>
  corrupt DB retained at …/aberp.duckdb.CORRUPT-<tag>
```

Exit code `0`. The DB is openable, every committed audit entry is intact, and a
`db.auto_recovered` entry is in the ledger. Done — no further steps.

### What a refusal prints

```
REFUSED: no VALID snapshot in …/ABERP-snapshots-<edition>/<tenant> to rebuild from
  — nothing was changed; the corrupt DB and any ahead mirror are preserved for investigation
```

or

```
REFUSED (unsafe): <reason> — nothing was changed; the corrupt DB and any ahead
  mirror are preserved for investigation
```

Exit code is **non-zero** (so a script can detect it). **Nothing was changed.**
A refusal means the engine could not *prove* the repair safe. Do **not** hand-fix.
Instead: confirm a valid snapshot exists (`aberp snapshot list --tenant <t>`),
confirm `<db>.audit.log` is present and readable, and check that the snapshot is not
*ahead* of the mirror. The corrupt DB and the ahead `.bak` are preserved for you to
investigate or hand to engineering.

---

## How to read the boot log

These are the exact lines `aberp serve` emits (tracing). Match on the quoted text.

**Recovery starting (either surface):**

```
ADR-0095 §1 — attempting guarded snapshot+replay auto-recovery   (fields: db, store, trigger)
```

`trigger=torn_open` means the live file would not open; `trigger=mirror_ahead`
means the mirror was ahead of the DB.

**Recovery succeeded:**

```
ADR-0095 §1 — auto-recovery SUCCEEDED; live DB rebuilt + verified-good marker written
   (fields: source_snapshot_seq, snapshot_audit_count, replayed_entries, recovered_max_seq, retained_corrupt_db)
```

For the ahead-mirror surface you will also see:

```
ADR-0095 §1 — auto-recovery reconciled the ahead mirror with the DB at boot
```

After this line boot continues normally — **recovery is invisible to users except
for the log line and the `db.auto_recovered` audit entry.**

**Recovery refused → boot stops (you must act):**

- Torn-open surface — the boot error context reads:
  *"billing DB at … could not be opened and auto-recovery refused (<reason>); the
  corrupt DB and any ahead mirror were preserved — investigate, then run `aberp
  recover`."*
- Ahead-mirror surface — you will see:
  `REFUSING to boot — ahead mirror could not be safely auto-recovered`.

In both cases nothing was changed; follow the `aberp recover` section above.

**Healthy steady-state (not an error):** during normal running you may see

```
live-path crash-safe durable checkpoint installed (ADR-0095 §3)
```

That is the app keeping a recent verified-good copy of the live file so a crash has
less to recover. `…checkpoint skipped — a verified-good checkpoint already covers
the DB` is the cheap no-op and is also normal.

---

## Two worked examples

### A. 2026-06-22 — prod corruption (the manual procedure that is now code)

A torn write on prod zeroed in-DB blocks (the audit-ledger mirror table + part of
`ap_invoice`). Recovery was done **by hand**: restore the last good snapshot and
**replay** the append-only ledger so no committed commit was lost, keeping the
corrupt file as evidence (reversible). That exact sequence — *preserve → restore
snapshot → replay ledger → validate → install* — is what `recover_or_refuse` now
performs automatically on the editions tree. (Prod itself was recovered manually
and remains on the manual procedure; see ADR-0096.)

### B. 2026-06-27 — Defense first-launch (the two surfaces, back to back)

Defense's **first** launch crashed partway through creating
`~/.aberp-defense/defense/aberp.duckdb`. The second launch hit **surface 1**:
`Failed to load metadata pointer (id 0, idx 0, ptr 0)`. The operator moved the torn
file aside; boot then built a fresh empty DB and hit **surface 2**: the mirror
(head 64) was ahead of the empty DB (head 0), and boot **refused**, requiring
hand-clearing two sidecar files. Under ADR-0095 as shipped, **both** are handled on
boot with no operator action: atomic creation prevents the torn first-launch file
in the first place (§2); if a torn file still appears it is auto-recovered (§1,
`torn_open`); and the ahead mirror is **replayed, not truncated** (§1,
`mirror_ahead`), so the chain does not fork and nothing is lost. The end-to-end
proof of both is `apps/aberp/tests/boot_crash_recovery_e2e.rs`
(`recover_cli_auto_recovers_torn_db_with_no_lost_committed_entry` and
`recover_cli_replays_ahead_mirror_with_no_fork_or_loss`).

---

## What changed vs the old runbook

- **Retired:** hand-clearing `aberp.duckdb.audit.log` / the `.ahead-*.bak`, and any
  "move the torn file aside and relaunch" loop. These can destroy committed-work
  evidence and must not be performed.
- **New default:** recovery is automatic on boot and **reversible** (the corrupt DB
  and ahead mirror are always retained).
- **Single manual entrypoint:** `aberp recover` — same engine, guarded and audited.

## Prod is out of scope here

This runbook covers the **editions** tree only. The `aberp-snapshot` engine refuses
any prod path (`ensure_not_prod_path`), and `--store` pointed at the frozen prod
line is rejected. Prod (`PROD_v2.27.76`) is **not** hardened by ADR-0095 and is
recovered by the manual snapshot + ledger procedure. The decision and its trigger
criteria are recorded in [ADR-0096](../../adr/0096-prod-backport-of-editions-durability-hardening.md).
