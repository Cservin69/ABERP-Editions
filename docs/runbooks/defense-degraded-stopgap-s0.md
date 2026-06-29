# Defense degraded-but-stable stopgap (ADR-0098 Session S0 — the bridge)

**Status:** Bridge / risk-reduction. **Not tear-proof.** The real fix is
ADR-0098 **Session B** (one shared `aberp_db::Handle`, single-writer). Use
this only to run a *degraded* Defense for **manual invoicing / quoting**
until v0.2.5 lands.

This runbook documents the **S0** deliverable: the exact env flag-set that
quiesces the high-frequency concurrent DuckDB openers, now including the one
opener that previously had **no kill switch** — the email-relay drain daemon
(`ABERP_EMAIL_RELAY_DRAIN_DISABLED`, added in S0).

For the corruption-recovery procedure itself (the *first* step below), follow
the canonical `docs/runbooks/db-corruption-recovery-operator-runbook.md`.

## Step 1 — recover the torn DB first

```
aberp recover --db <path-to>/aberp.duckdb --tenant <tenant> --store <snapshot-store>
```

> **Caveat (ADR-0098 Gap 2a, flagged):** if a *fresh snapshot is ahead of the
> audit mirror*, `recover` currently **refuses** (`RefusedUnsafe`). That guard
> is fixed in **Session A**, not S0. If recovery is refusing for that reason,
> S0 cannot proceed until Session A lands — **A is the true unblock.**

## Step 2 — relaunch `serve` with the degraded-mode flag-set

Paste-ready (Defense; adjust the launch command to your environment):

```sh
# ADR-0098 S0 degraded-but-stable Defense stopgap.
# Quiesce every high-frequency concurrent DuckDB opener; keep snapshots ON.

# (1) NEW in S0 — silence the unconditional ~2s email-relay drain opener.
export ABERP_EMAIL_RELAY_DRAIN_DISABLED=1

# (2) No pricing-pipeline / quote-intake / catalogue-push daemons.
#     Set BOTH (env takes precedence over seller.toml; belt-and-braces):
unset  ABERP_QUOTE_INTAKE_ENABLED           # ensure NOT set to true/1
#     …and in <tenant>/seller.toml, under [quote_intake]:  enabled = false

# (3) No email-outbox poll daemon.
export ABERP_EMAIL_OUTBOX_POLL_DISABLED=1

# (4) No pdf-rerender daemon.
export ABERP_PDF_RERENDER_DISABLED=1

# (5) Leave the 4-h snapshot daemon ENABLED (recovery substrate) —
#     do NOT set ABERP_SNAPSHOT_DISABLE.
unset  ABERP_SNAPSHOT_DISABLE

# then launch Defense normally, e.g.  ./run/run_defense.sh   (or your serve cmd)
```

Accepted truthy form for every `*_DISABLED` flag: `1` or `true`
(case-insensitive, surrounding whitespace ignored). Anything else — including
`0`, `false`, empty — leaves the daemon **enabled**.

## What each lever does (all grep-verified)

| Lever | Effect | Spawn site |
|---|---|---|
| `ABERP_EMAIL_RELAY_DRAIN_DISABLED=1` *(NEW, S0)* | no email-relay drain daemon (the unconditional ~2s opener) | `serve.rs` email-relay block → `email_relay_daemon::is_disabled` |
| unset `ABERP_QUOTE_INTAKE_ENABLED` **and** `[quote_intake] enabled=false` | no pricing-pipeline / quote-intake / catalogue-push daemons | `serve.rs` (`ABERP_QUOTE_INTAKE_ENABLED` read) |
| `ABERP_EMAIL_OUTBOX_POLL_DISABLED=1` | no email-outbox poll daemon | `email_outbox_poll_daemon::is_disabled` |
| `ABERP_PDF_RERENDER_DISABLED=1` | no pdf-rerender daemon | `quote_pdf_rerender_daemon::is_disabled` |
| leave `ABERP_SNAPSHOT_DISABLE` **unset** | keep the 4-h snapshot daemon (low-frequency read; recovery substrate) | `snapshot` daemon |

## Residual risk — read this

With the full set applied, the **only** residual live-DB openers are the
**4-h snapshot daemon** (a low-frequency logical `EXPORT` read) and your own
**human-paced SPA writes**, which rarely overlap. This is **risk-reduced, not
tear-proof**: there is **no env-only configuration in v0.2.4** that makes the
serve process a single writer. Only **Session B** (the shared
`aberp_db::Handle`) removes the concurrent-separate-opens defect for good.
Until then, keep DB load low (one operator, manual cadence) and keep snapshots
on so recovery stays possible.

## Disabling the relay drain has one functional cost

While `ABERP_EMAIL_RELAY_DRAIN_DISABLED=1`, **queued outbound email is not
sent** — rows stay `Queued` and drain automatically on the next boot with the
flag unset. Re-enable (unset the flag, restart `serve`) once you no longer
need the degraded mode.
