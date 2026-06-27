# ADR-0095 — Editions crash-safe durability: boot auto-recovery, atomic DB creation, and live-path durable checkpoints

- **Status:** Proposed (design pass — forensics + hardening; implementation is sequenced in the companion plan `docs/crash-safe-durability-and-boot-recovery-fix-plan.md`, not done here)
- **Date:** 2026-06-27
- **Deciders:** Ervin
- **Extends:** ADR-0082 (validated logical DuckDB snapshot system — the corruption-free recovery substrate this builds on), ADR-0093 chunk 3 (the `aberp-snapshot::crash_safe` durable-checkpoint primitives — this ADR *wires* them, it does not re-build them)
- **Grounds / related:** ADR-0008 (tamper-evident audit hash-chain ledger + its append-only JSONL mirror), ADR-0002 (database-per-tenant isolation), the **2026-06-22 prod corruption recovery record** (`~/.aberp/ABERP-prod-corruption-recovery-2026-06-22.md` — the *manual* procedure this ADR automates), `duckdb/duckdb#23046` (the torn-write / ART-checkpoint corruption family), `[[trust-code-not-operator]]`, `[[hulye-biztos]]`.

## Context

### Two failures, one root, reproduced on the fresh Defense edition (2026-06-27)

Defense's *first* launch crashed partway through creating
`~/.aberp-defense/defense/aberp.duckdb`. The second launch could not open it:

```
INTERNAL Error: Failed to load metadata pointer (id 0, idx 0, ptr 0)
  in CheckpointReader::LoadCheckpoint
```

After the operator moved the torn file aside and relaunched, boot built a
**fresh empty** DB but then **refused to boot** because the stale audit-ledger
mirror was ahead of it. Recovery required hand-clearing two sidecar files.
These are two surfaces of the same durability gap.

### Forensic root cause #1 — the torn checkpoint (confirmed signature)

Working on a **copy** of `aberp.duckdb.CORRUPT` (7,090,176 B; mtime
2026-06-27 18:56:35; never opened by an app binary), the DuckDB v1.5.3 file
carries two alternating database headers. DuckDB selects the header with the
**highest iteration** and dereferences its root metadata pointer:

| Header | Offset | iteration | `meta_block` | verdict |
|---|---|---|---|---|
| DBHdr0 | `0x1000` | 19 | `0x1` (valid) | standby |
| **DBHdr1** | `0x2000` | **20** | **`0x0`** | **active → selected** |

The active header's `meta_block` is **zero**. That zero *is* the `ptr 0` in
the error: `LoadCheckpoint` reads the root metadata pointer from the newest
header, gets `0`, and aborts — the file is unopenable. A healthy DB of the
same build (the post-recovery `aberp.duckdb`, 4,468,736 B) carries a valid
non-zero `meta_block` in its active header (iter 12). The main header (magic
`DUCK`, version 64, libduckdb `v1.5.3`, build `14eca11bd9`) is byte-identical
between the two files, so corruption is confined to the
metadata/database-header region — a checkpoint's header update became durable
while the metadata it points at did not (or the pointer field itself was torn
to zero).

This is the **`duckdb#23046` torn-write / ART-checkpoint family** — the exact
class named in ADR-0082 and in the 2026-06-22 prod incident (where blocks
93–95 / the in-DB ledger mirror + `ap_invoice` were zeroed). DuckDB folds its
WAL into the live `*.duckdb` **in place**; a crash mid-fold leaves a torn
file. Same family, same signature, new edition.

### Forensic root cause #2 — the crash-safety **wiring gap** (the central finding)

ADR-0093 chunk 3 built the *correct* durability primitives in
`crates/aberp-snapshot/src/crash_safe.rs`, and they are sound:

- `atomic_install` (`crash_safe.rs:141`) — drop staged WAL → **fsync staged**
  → **atomic `rename`** → drop stale target WAL → **fsync parent dir**. The
  textbook crash-safe commit: a crash is always on one side of the rename,
  never a torn middle. Exhaustively unit-tested on plain files.
- `write_marker` / `read_marker` / `checkpoint_is_current`
  (`crash_safe.rs:161`–`209`) — the `<db>.ckpt-ok` verified-good marker
  (SHA-256 + size), itself fsync'd.
- `durable_checkpoint` (`crash_safe.rs:222`) — `EXPORT DATABASE` (logical
  Parquet, corruption-free by construction per ADR-0082) → **validate**
  (import + smoke + hash-chain) → refuse if the live DB doesn't validate →
  `IMPORT`+`CHECKPOINT` into a **private staging** file → `atomic_install` →
  `write_marker`.

**The defect is not in these primitives. It is that nothing calls them on any
path that a crash actually traverses.** Grep of every caller:

- `durable_checkpoint` has exactly **one** live-runtime caller:
  `checkpoint_on_clean_shutdown` (`apps/aberp/src/snapshot.rs:104`), invoked
  **only** from `serve.rs:2978` — i.e. **clean shutdown only**.
- The periodic snapshot daemon (`run_supervised`, `snapshot.rs:475` →
  `run_cycle` → `take_and_emit`) writes **logical snapshots to the store**;
  it never rewrites or re-marks the **live** file and never calls
  `durable_checkpoint`.
- **Boot** opens the live DB directly and never durable-checkpoints, atomic-creates, or auto-recovers (below).

Therefore, during normal operation and during first launch, **the live DB is
left in whatever state DuckDB's default in-place checkpoint produces — the
vulnerable `duckdb#23046` path.** The crash-safe mechanism only engages at a
*clean* exit. Defense crashed on **first launch**, before any clean shutdown
had ever run, so chunk 3 protected nothing. The evidence confirms the
divergence: a `.ckpt-ok` written at 18:53:55 (size 4,468,736, sha `3c8926…`)
no longer matches the live file the app grew to before the 18:56:35 crash —
the marker and the live file drift precisely because the live file is never
continuously checkpointed-and-marked.

> **The central question, answered:** Nothing makes the *live* DuckDB
> writes/checkpoints crash-safe. The app relies on DuckDB's default
> (vulnerable) in-place checkpoint for all runtime writes, initial creation,
> and crashes; `durable_checkpoint` runs only at clean shutdown
> (`serve.rs:2978` → `snapshot.rs:104` → `crash_safe.rs:222`).

### Forensic root cause #3 — initial creation is **not** atomic

Boot creates the parent dir then opens DuckDB **directly at the final path**:
`std::fs::create_dir_all(parent)` (`serve.rs:1135`) → `DuckDbBillingStore::open(&args.db)`
(`serve.rs:1143`, the "ensuring billing schema" step) → a chain of
`Connection::open(&args.db)` + `ensure_schema` for every module
(`serve.rs:1160`–`1343`). DuckDB materialises the file in place. A crash
during this window leaves a **torn file at the live path** — exactly the
Defense first-launch outcome. The codebase already owns the right pattern —
`write_atomic` (tempfile + fsync + rename, 0600) in
`tenant_registry.rs:867`–`918` — but applies it only to TOML, **never to the
`.duckdb`**.

### Forensic root cause #4 — boot is monolithic fail-fast; the ahead-mirror refuse

After the operator moved the torn file aside, boot's mirror reconciliation
(`serve.rs:1381`, `recover_audit_mirror`) found the mirror (max seq **64**)
ahead of the fresh empty DB (max seq **0**). The chunk-3 P1 guard
(`crates/audit-ledger/src/mirror.rs:524`–`548`) correctly **preserved** the
ahead mirror to `<mirror>.ahead-<nanos>.bak` (`preserve_ahead_mirror`,
`mirror.rs:587`) and returned `MirrorAheadOfDb`; boot turned that into a fatal
`return Err` (`serve.rs:1399`–`1406`) and stopped. Three rapid relaunches each
re-preserved (three `.ahead-*.bak`, byte-identical) and re-refused. Recovery
required the operator to *also* hand-clear `aberp.duckdb.audit.log`.

The **preserve-and-refuse default is correct** — silently truncating the
mirror would destroy the only record of the lost commits (the 2026-06-22
record, §"Sequencing note", warns that a deferred top-up would let the first
boot truncate the JSONL and *fork the chain*). What is wrong is that **refuse
is the *only* outcome**: there is no automated path that does what a human did
on 2026-06-22 — rebuild from the last good snapshot and **replay** the ahead
mirror. The single supported recovery is therefore manual sidecar surgery.

### Boot-fragility: the SMTP hypothesis, corrected

Ervin attributed the first-launch crash to missing SMTP config. **The code
does not support that:** `sanity_check_environment` treats a missing
`[seller.smtp]` as a **warning, not fatal** (`serve.rs:472`–`480`); the boot
SMTP-password read is explicitly non-fatal and skips silently when SMTP is
absent (`serve.rs:970`–`983`); and the `smtp_configured` input is computed as
`matches!(read_smtp_config(p), Ok(Some(_)))` (`serve.rs:1011`–`1013`), so even
a *malformed* SMTP section degrades to "not configured" → warning. Missing
SMTP already degrades. The genuine fragility is different and is what actually
took boot down: **boot is a monolithic fail-fast sequence** where a single
subsystem error aborts everything — via `?` on the live-DB-open/`ensure_schema`
chain (`serve.rs:1143`–`1343`; root cause #1's surface), via `return Err` on
the ahead-mirror (`serve.rs:1399`; root cause #4), and via
`std::process::exit(1)` on sanity-fatal (`serve.rs:1031`) and
upgrade-snapshot-mismatch (`serve.rs:1105`). The fix the feedback asks for —
"degrade on missing optional config" — is already true for SMTP; the missing
half is that **DB-durability failures are fatal instead of auto-recovered**,
plus a standing invariant that *no* optional subsystem may hard-abort boot
(one residual risk: `SmtpSecurity::parse` loud-fails on an unknown `security`
token, `smtp_config.rs:58`–`60` — safe today only because the boot reader uses
the `matches!` degrade pattern, not `?`).

### Prod is out of scope

Frozen prod (`~/.aberp/prod`, repo HEAD `2bd2adff`, source tree `2d612811`)
carries the **same latent vulnerability** (default in-place checkpoint) but is
**not touched here**. It is protected only by the snapshot + ledger recovery
procedure (the automated, reversible 2026-06-22 method). This fix lands in the
**editions tree only**; the snapshot/crash-safe crate already refuses to act
on a prod path (`ensure_not_prod_path` — `snapshot.rs:86`, `:209`, `:140`), so
the change is mechanically prod-safe. **Any prod backport is a separate, later
decision.**

## Decision

Wire the chunk-3 primitives that already exist into the paths a crash
actually takes, and add one automated recovery engine. Four parts; **no new
durability primitive is invented** — `atomic_install`, the markers, the
logical export/import, and `durable_checkpoint` are reused as-is.

### 1. Boot safe-open + auto-recover (covers BOTH torn-open and ahead-mirror)

On boot, before the fail-fast schema chain, run a guarded recovery decision
on the tenant DB. Trigger if **either**: (a) opening the live DB fails
(torn — root cause #1), **or** (b) mirror reconciliation reports
`MirrorAheadOfDb` (root cause #4). When recovery is **safe**, perform the
automated, reversible 2026-06-22 procedure instead of aborting:

```
recover_or_refuse(db_path, store_dir, mirror_path, tenant):
  1. PRESERVE evidence, never destroy it:
       - torn DB      → copy to <db>.CORRUPT-<ts> (retain)
       - ahead mirror → already preserved as <mirror>.ahead-<nanos>.bak (chunk 3)
  2. Locate latest VALID snapshot in store_dir (ADR-0082 meta.json valid=true).
       none found → REFUSE (fall through to today's preserve-and-surface).
  3. Build a fresh staging DB from it: IMPORT DATABASE (corruption-free by
     construction) into <db>.recover-<tag>.duckdb (private; never the live path).
  4. REPLAY the append-only audit-ledger JSONL delta into the rebuilt DB:
       entries with seq > snapshot.audit_count, inserted byte-faithfully
       (insert_entry_verbatim — the 12 canonical columns), in seq order.
  5. VALIDATE the rebuilt DB: schema migrates clean; hash-chain verifies
     genesis→head (ADR-0008); rebuilt db_max_seq == mirror_max_seq and the
     head entry_hash matches the mirror head.
       any check fails → discard staging, REFUSE (preserve-and-surface).
  6. COMMIT atomically: atomic_install(staging → db_path)  [reuse crash_safe.rs:141]
     then write_marker(db_path).                            [reuse crash_safe.rs:161]
  7. Emit audit: snapshot.restored + new db.auto_recovered (counts, source snap,
     replayed range, retained backup paths). Continue boot.
```

**Guard rails (safety is the default):** recover **only** when a valid
snapshot exists **and** the mirror's hash-chain validates **and** the mirror
is a consistent extension of the snapshot (`snapshot.audit_count ≤
mirror_max_seq`, chains agree on the overlap). Otherwise do **not** guess —
fall back to the existing **preserve-and-refuse** (the chunk-3 P1 guard stays
as the safe fallback, demoted from "only outcome" to "last resort"). The ahead
mirror is **replayed, never truncated**; after a successful recovery the mirror
and DB reconcile to `Unchanged`, so no chain fork. Both the corrupt DB and the
`.ahead-*.bak` are retained → fully reversible, same posture as the 2026-06-22
record's retained backup. **One** supported entrypoint: automatic on boot when
safe; otherwise a single documented command (`aberp recover` / `serve
--recover`). **No manual sidecar surgery, ever.**

### 2. Atomic initial creation

When the tenant DB does not yet exist, create it **aside and swap**: provision
a fresh DB at `<db>.creating-<tag>.duckdb` (run every `ensure_schema` + the
genesis audit row + `CHECKPOINT` there), then `atomic_install` it onto the
final path and `write_marker`. A crash mid-creation leaves only a disposable
temp file (cleaned next boot) — **never a torn file at the live path**. This
alone prevents the Defense first-launch failure; the §1 auto-recover covers
the residual case where a torn file still appears.

### 3. Live-path durable checkpoints (wire the existing mechanism where it isn't)

Keep the clean-shutdown checkpoint (`serve.rs:2978`). **Add** the same
`durable_checkpoint` call on the paths a crash traverses:

- **Periodically against the live file** — fold a live-file
  `durable_checkpoint` into the snapshot daemon cadence (`run_supervised`,
  `snapshot.rs:475`), so a recent verified-good live file exists even with no
  clean shutdown.
- **Post-meaningful-write** — after a regulated write (invoice issue/storno/
  modification), schedule a **debounced** `durable_checkpoint`, bounding the
  exposure window after important commits.
- **At boot after recovery/creation** — §1 and §2 already end in
  `write_marker`, so the next boot needs no in-place `LoadCheckpoint` replay.

Exactly what chunk 3 built and where it must now be **called**:
`durable_checkpoint`/`atomic_install`/markers (`crash_safe.rs:141`/`161`/`222`)
exist and are correct; today they fire only at `serve.rs:2978`. They must
**also** fire at boot recovery (§1), at atomic creation (§2), periodically, and
post-write (§3).

### 4. Boot robustness

Preserve the current SMTP/NAV degrade posture (`serve.rs:472`–`480`,
`970`–`983`) and **extend the principle**: (a) reclassify DB-durability
failures from *fatal* to *auto-recover-then-continue* via §1; (b) pin the
invariant that **no optional subsystem may hard-abort boot** — every
optional-config read degrades + logs + surfaces in the UI (banner), never `?`
or `exit` at boot. Fatal is reserved for true safety stops (prod-identity
mismatch, NAV creds vanished after a completed first launch); those keep
`exit(1)`.

## Consequences

**Easier:** first launch and crash recovery become automatic and reversible;
the recurring `duckdb#23046` incident stops being an unbounded-downtime,
hand-surgery event; the 2026-06-22 runbook becomes code that runs on boot; a
known-good live file + marker always exists.

**Harder / locked in:** boot gains a recovery branch that must itself be
crash-safe and exhaustively tested (a buggy auto-recover is worse than a
refuse); we commit to the snapshot store + append-only JSONL mirror as the
twin sources of recovery truth (both must remain present and validated); the
post-write checkpoint adds bounded I/O that must be debounced so it never
stalls issuing.

## Adversarial review

1. *"Auto-recovery silently rewrites the operator's DB — how is that not the
   silent-truncation you just condemned?"* It never destroys: corrupt DB and
   ahead mirror are retained; recovery is **append-only replay** of the
   preserved mirror onto a snapshot, validated to reconcile with no fork; it
   runs **only** when snapshot+mirror prove consistent, else it refuses.
   Every recovery is audited (`db.auto_recovered`) and reversible.
2. *"What if the snapshot is also corrupt, or the mirror chain is broken?"*
   Then a guard fails (no valid snapshot / chain won't verify / heads
   disagree) and we fall back to today's preserve-and-surface. Auto-recover is
   strictly additive over the current safe default; it never lowers the bar.
3. *"Could this touch prod?"* No. Editions builds bind to non-prod roots
   (ADR-0093) and the crash-safe/snapshot crate refuses prod paths
   (`ensure_not_prod_path`). Prod is explicitly out of scope and unchanged.
4. *"Post-write + periodic checkpoints could stall the app or thrash disk."*
   Debounced, off the request path, on the existing blocking-snapshot thread;
   `checkpoint_is_current` makes a no-op cheap when nothing changed.

## Alternatives considered

- **Byte-copy backup / restore.** Rejected by ADR-0082: a copy of a torn file
  is still torn. We reuse the logical export/import substrate instead.
- **Fix DuckDB's in-place checkpoint.** Out of our control (`duckdb#23046`);
  the build is pinned to libduckdb 1.5.3 for storage compatibility. We harden
  *around* it.
- **Auto-truncate the ahead mirror to match the DB.** Rejected — destroys the
  record of lost commits and forks the chain (2026-06-22 §"Sequencing note").
- **Leave recovery manual (status quo).** Rejected — `[[trust-code-not-operator]]`:
  a safety property that depends on operator sidecar surgery is not a safety
  property.

## Open questions

- Debounce window + cadence for the post-write/periodic live checkpoints
  (proposed: coalesce ≤1/min post-write; keep the 4-h store snapshot cadence).
- Should `db.auto_recovered` raise a persistent UI banner until the operator
  acknowledges (so an automatic recovery is never invisible)? Leaning yes.
- Bound on replayable mirror delta before recovery prefers a fresher snapshot
  over a long replay (cosmetic; replay is fast and verified).
