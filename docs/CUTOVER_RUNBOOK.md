# ABERP production cutover runbook

**Last updated:** 2026-05-30 — Session 167 / PR-167.
**Audience:** Ervin (sole operator).
**Language:** EN-primary; HU clarifications inline where they help at the
machine.

Real-money invoicing for **Áben Consulting Kft.** (tax `24904362-2-41`)
begins Monday **2026-06-01**, starting at invoice number
`ABERP/2026/0001`. This runbook is the step-by-step for the manual
cutover the afternoon of 2026-05-30.

The runbook is **hülye-biztos** by design — every step that touches the
real tax authority or real money has a confirmation gate, and any guard
that fails refuses to proceed.

---

## Pre-flight checklist

Before you touch anything, confirm all of the below. If any line is not
true, **stop**, fix it, then restart the runbook.

- [ ] **NAV production technical-user creds collected**: login,
  password, XML SIGN key, XML CHANGE/EXCHANGE key. Four secrets. Have
  them ready to paste into a terminal (the `setup_nav_creds.sh` script
  reads them silently — no echo, no scrollback).
- [ ] **SMTP working in test**: a recent test invoice email landed
  successfully from the dev tenant. The prod SMTP creds will be the
  same Zoho account; only the **host is different** — Zoho EU mailboxes
  use `smtppro.zoho.eu` (NOT `smtp.zoho.com` or `smtp.zoho.eu` — those
  are different hosts on Zoho's own infrastructure).
- [ ] **~10-minute uninterrupted window**: cutover itself is fast, but
  if a smoke invoice fails you want to debug while the binary is fresh
  in your head.
- [ ] **Audit ledger reviewed**: `git log --oneline -20` on `main`.
  Confirm the HEAD commit you're cutting over from is the one you
  expect. Today's reference HEAD: `18f27ab` (S166 — boot sanity
  checks + first-launch confirm).
- [ ] **Working tree clean**: `git status` shows nothing pending.
- [ ] **Coffee**.

---

## Step 1 — Create the prod branch (one-time)

The prod branch is the long-lived release branch. Tagged builds get cut
from it. Created **once**, at cutover.

```bash
cd /Users/aben/Documents/Claude/Projects/ABERP
git fetch origin
git checkout -b prod 18f27ab     # or whatever main HEAD you cut over from
```

**HU:** A `prod` ág a hosszú életű kiadási ág. Egyszer hozod létre.

After this point, `main` continues to receive dev work. Ongoing prod
updates land on `prod` via fast-forward or cherry-pick (see Step 9).

> **Do NOT push the `prod` branch yet.** Push only after the smoke
> invoice (Step 7) lands cleanly.

---

## Step 2 — Build the prod binary

```bash
./run/release.sh prod-v0.1.0
```

The script:

1. Refuses to run unless you're on `main` (so re-run after Step 1 once
   you're on `prod`, OR build the very first release from `main` HEAD
   — see note below).
2. Validates the version matches `prod-vMAJOR.MINOR.PATCH`.
3. Refuses if `prod-v0.1.0` already exists locally or on origin.
4. Runs `cargo fmt --check` (strict).
5. Runs `cargo clippy --workspace` (advisory; prints, never blocks).
6. Builds **both** binaries (`aberp` + `aberp-ui`) with
   `--features production`.
7. Creates an **annotated** tag locally (NOT pushed).
8. Prints the binary paths.

> **Branch-vs-tag note:** `release.sh` currently checks for `main`.
> For the very first cutover, cut the release tag from `main` HEAD
> (which IS the `prod` ancestor — `prod` is just `main` HEAD at this
> moment). For subsequent releases off `prod`, edit `MAIN_BRANCH` in
> `release.sh` or temporarily check out `main` at the SHA you want to
> release from. This is intentional friction: cutting a prod tag from
> a non-mainline ref should be a deliberate, eye-balled action.

**HU:** A `release.sh` lefordítja az éles bináris kettőst és lokális
címkét készít. A címke MÉG NINCS feltöltve a távolira.

If clippy prints warnings, **read them**. The script does not block,
but a clippy warning on a release commit is something you want to
acknowledge before pushing.

Artifacts at the end of Step 2:

- `target/release/aberp` — the CLI (also spawned by aberp-ui at
  runtime for keychain reads + DB ops).
- `target/release/aberp-ui` — the Tauri shell. **This is the binary
  the operator launches.**
- Local annotated tag `prod-v0.1.0`.

---

## Step 3 — Set up the prod tenant directory

The prod tenant lives at `~/.aberp/prod/`. The launcher creates the
directory on first run; here we pre-populate the seller config so the
boot path doesn't drop you into the seller-config wizard.

```bash
mkdir -p ~/.aberp/prod
$EDITOR ~/.aberp/prod/seller.toml
```

Template (replace bracketed values; the `[seller.numbering]` section
gets you the `ABERP/2026/0001` shape — without it the default is
`INV-default/00001`):

```toml
# ABERP seller config — Áben Consulting Kft. — prod tenant.

[seller]
legal_name = "Áben Consulting KFT."
tax_number = "24904362-2-41"
eu_vat_number = "HU24904362"

[seller.address]
country_code = "HU"
postal_code = "[your prod postal code]"
city = "[your prod city]"
street = "[your prod street + house number]"

# Bank info — printed on the PDF footer.
bank_account_number = "[12345678-12345678-12345678]"
iban = "[HU12 1234 5678 1234 5678 1234 5678]"
bank_name = "[your bank name]"
swift_bic = "[BANKHUHB]"

# Invoice numbering — yields ABERP/2026/0001 with annual reset.
[seller.numbering]
segments = [
  { kind = "Literal", text = "ABERP/" },
  { kind = "Year", digits = 4 },
  { kind = "Literal", text = "/" },
  { kind = "Counter", pad_width = 4 },
]
reset_policy = "OnYearChange"
start_value = 1
```

**HU:** Az `[seller.numbering]` szekció adja az `ABERP/2026/0001`
számformátumot. Évente nullázódik (`reset_policy = "OnYearChange"`).

**Sanity check the tax number.** The S166 boot sanity check refuses to
start any prod binary whose `seller.toml` shows a `tax_number` other
than `24904362-2-41` (see `apps/aberp/src/serve.rs::sanity_check_environment`
case A). If you mistype it, the binary will refuse to start with a
bilingual fatal banner — you have to fix the toml and restart, no
silent miscarriage.

---

## Step 4 — Populate NAV credentials in the macOS keychain

ABERP reads NAV technical-user credentials from the keychain at
`service=aberp.nav.prod`, `account=nav_credentials_blob` (consolidated
blob layout per PR-57; one keychain item per tenant, four secrets
inside).

The canonical write path is the `setup_nav_creds.sh` script — it reads
each value silently (no terminal echo, no shell history, no temp file)
and pipes them straight into the `aberp setup-nav-credentials`
subcommand, which writes the keychain entry.

```bash
./run/setup_nav_creds.sh --tenant prod
```

The script will:

1. Show a "you are about to set up PRODUCTION NAV creds" warning and
   require you to type `I understand` literally.
2. Prompt for **Technical-user LOGIN** (read silently — `read -s`).
3. Prompt for **Technical-user PASSWORD**.
4. Prompt for **XML SIGN key**.
5. Prompt for **XML CHANGE (exchange) key**.
6. Pipe the four values into `cargo run --bin aberp -- setup-nav-credentials --tenant prod`.
7. Confirm the write and tell you to launch `./run/run_desktop.sh`.

> **Ignore step 7's `run_desktop.sh` suggestion** — that's the dev
> launcher. For prod, use `./run/run_prod.sh` (Step 6 here). The dev
> launcher *refuses* `--tenant prod` anyway (S165 guard); this is just
> a stale closing line in `setup_nav_creds.sh`.

**HU:** A `setup_nav_creds.sh` négy NAV-titkot olvas be némán és
beírja a macOS kulcstartóba. A `prod` bérlőhöz tartozó kulcstartó-elem:
`aberp.nav.prod` / `nav_credentials_blob`.

**Verify** the keychain write landed:

```bash
security find-generic-password -s "aberp.nav.prod" -a "nav_credentials_blob" -g
# (will prompt for your macOS login password to read the value; you
#  don't need to read it — just confirm the entry exists)
```

If `security find-generic-password` returns "The specified item could
not be found in the keychain", **stop** — re-run `setup_nav_creds.sh`.

---

## Step 5 — Set up SMTP credentials

There is **no CLI command for SMTP creds**. SMTP is configured through
the in-app SPA Settings page on first launch (see Step 6). The Settings
PUT route writes the keychain at `service=aberp.smtp.prod`,
`account=smtp_password`.

What you'll need at hand when the SPA prompts:

| Field         | Value                                             |
|---------------|---------------------------------------------------|
| Host          | **`smtppro.zoho.eu`** (NOT `.com`, NOT plain `.eu`) |
| Port          | `465`                                              |
| Security      | TLS (implicit) — Zoho EU's `smtppro` listener on 465 |
| From address  | your Zoho mailbox address                          |
| Username      | your Zoho mailbox address (same as From)           |
| Password      | Zoho **app-specific password** (NOT your account password) |

> **Zoho EU pitfall** — `smtp.zoho.com` is Zoho's US infra; `smtp.zoho.eu`
> exists but is for non-pro accounts. The Zoho **Workplace Pro** EU
> tenant uses `smtppro.zoho.eu` specifically. Authenticating with the
> wrong host will surface as TLS handshake or AUTH failures; the SPA's
> "Test Connection" button is the fast way to confirm before the first
> live email.

**HU:** SMTP-t a SPA Beállítások oldaláról állítod be — nincs CLI
parancs hozzá. A Zoho EU **Workplace Pro** host pontos neve:
`smtppro.zoho.eu` (a sima `smtp.zoho.eu` MÁS, és nem fog működni).

---

## Step 6 — First production launch

```bash
./run/run_prod.sh
```

The script:

1. Exports `ABERP_TENANT=prod` and `ABERP_DB=~/.aberp/prod/aberp.duckdb`.
2. Pre-flight warns (NOT fatal) if `~/.aberp/prod/seller.toml` is
   missing or the first-launch touchfile is absent.
3. Prints the loud red bilingual "PRODUCTION BUILD — REAL NAV — REAL
   MONEY" banner with the endpoint URL.
4. Compiles + runs `aberp` and `aberp-ui` with `--features production`.

What happens inside the binary as it boots:

- **S165 cross-stream guard** (`guard_tenant_matches_build`) — a prod
  binary refuses to start if `ABERP_TENANT != "prod"`. The launcher
  exports `prod`, so this passes.
- **S166 sanity check** (`sanity_check_environment`) — three gates:
  - **A. Seller identity** — `seller.toml` must have
    `tax_number = "24904362-2-41"`. Mismatch = fatal. Missing file =
    deferred to the wizard.
  - **B. NAV credentials** — keychain entry must exist if the
    first-launch ceremony was previously acknowledged. On the very
    first launch this gate is permissive (the wizard will populate).
  - **C. SMTP** — missing `[seller.smtp]` is a **warning**, not
    fatal; you can configure it via the SPA after launch (Step 5).
- **First-prod-launch modal** (S166) — because the touchfile
  `~/.aberp/prod/.first-launch-acknowledged` is absent on first boot,
  the SPA blocks all main routes behind a confirmation modal. You
  must type `ABERP` (uppercase, exact) to proceed. On submit, the
  touchfile is written with an RFC3339 timestamp and the
  `FirstProdLaunchAcknowledged` audit entry is appended.

If the SMTP settings wizard appears (or if you skipped Step 5 and just
go straight to the Settings page after the first-launch modal),
populate the values from Step 5 and click **Test Connection** before
saving.

**HU:** Az első éles indítás során egy megerősítő ablak jelenik meg —
gépeld be: `ABERP` (csupa nagybetű, pontosan). Ez egyszeri; a
megerősítést `~/.aberp/prod/.first-launch-acknowledged` rögzíti és az
audit ledgerbe is bekerül (`FirstProdLaunchAcknowledged`).

---

## Step 7 — Smoke invoice

The point of the smoke invoice is to prove the full prod path —
NAV submit, ack poll, PDF, email — works end-to-end before you sit
down to issue the real first invoice on Monday.

1. Pick a small, internal-target invoice (e.g. a HUF 1 line to a partner
   you control). It will be a **real** NAV submission and a **real**
   email — pick the recipient accordingly.
2. Issue it from the SPA.
3. Watch the invoice-detail page:
   - The number should be **`ABERP/2026/0001`** (no `TEST-` prefix —
     prod builds drop it).
   - NAV submit transitions to `RECEIVED` → `PROCESSING` → `DONE`
     (the S161 poll daemon handles the transitions automatically).
   - Email delivery status reaches `Sent`.
4. Open the PDF — verify the seller block (legal name, tax, address,
   bank) renders from your Step-3 `seller.toml`.

**If anything fails:**

- NAV submit `ABORTED` with a business rule violation: read the ack
  XML. `customerAddress` is a classic gotcha for PRIVATE_PERSON buyers
  (see `[[reference_nav_gotchas]]` memory).
- Email `Failed`: check the `[seller.smtp]` host. `smtppro.zoho.eu` is
  the right host for the EU pro tenant (Step 5).
- PDF wrong: re-check `~/.aberp/prod/seller.toml`.

If the smoke invoice lands cleanly:

```bash
git push origin prod-v0.1.0   # push the tag (Step 2 left it local)
git push origin prod          # push the prod branch
```

**HU:** A smoke invoice egy valódi (de általad kontrollált) NAV-beadás
+ e-mail küldés. Csak akkor folytasd, ha a teljes lánc lement.

---

## Step 8 — Rollback procedure

If something goes wrong after a release lands and you've already
issued one or more invoices, **do not panic and do not delete
anything**.

The audit ledger is append-only (ADR-0008) and `ensure_schema` is
idempotent — rolling back to a previous prod build will not corrupt
any prior invoice's audit trail.

To roll back to a previous tagged release:

```bash
# 1. Stop the running app (Ctrl-C in the run_prod.sh terminal).
# 2. Check out the previous tag.
git checkout prod-v0.0.x      # whichever previous tag you want
# 3. Rebuild with the production feature.
cargo build --release --features production --bin aberp
cargo build --release --features production --bin aberp-ui
# 4. Relaunch.
./run/run_prod.sh
```

The DuckDB file at `~/.aberp/prod/aberp.duckdb` is preserved across
binary versions; migrations are forward-only and run at boot
(`ensure_schema`). A rollback launches against the existing DB with
the previous code; if the previous code's schema is older, **do not
roll back across a destructive migration without first restoring a
DB snapshot from before the migration ran**.

> **Snapshot the DB before any cutover or upgrade.**
> Standard belt-and-suspenders backup:
> ```bash
> cp ~/.aberp/prod/aberp.duckdb \
>    ~/.aberp/prod/aberp.duckdb.snapshot-$(date +%Y%m%d-%H%M%S)
> ```
> Do this before Step 2 and before any future Step 9 upgrade. DuckDB
> is a single file; the snapshot IS the rollback target.

**HU:** A rollback biztonságos, mert az audit ledger csak hozzáfűzhető,
és a séma-migrációk ütközésmentesek (`ensure_schema` idempotens). DB-
snapshotot mindig csinálj BÁRMILYEN frissítés előtt.

**If you need to invalidate an issued invoice**, that's NOT a rollback
— that's a stornó. Use the SPA's storno workflow (S156); don't try to
fix it by reverting code.

---

## Step 9 — Ongoing update workflow

Routine: dev work continues to land on `main`. When you want a fix or
feature to reach prod:

```bash
# 1. (Optional but recommended) DB snapshot first.
cp ~/.aberp/prod/aberp.duckdb \
   ~/.aberp/prod/aberp.duckdb.snapshot-$(date +%Y%m%d-%H%M%S)

# 2. Update prod. Two flavours:

#    (a) Fast-forward the whole of main into prod (typical):
git checkout prod
git merge --ff-only main      # refuse if non-FF — surface conflicts
                              # before they reach prod

#    (b) Cherry-pick a specific fix only (for partial updates):
git checkout prod
git cherry-pick <sha>

# 3. Cut a new tag + build.
./run/release.sh prod-v0.X.Y

# 4. Stop the running app, then relaunch:
./run/run_prod.sh

# 5. After smoke-confirming the new build:
git push origin prod
git push origin prod-v0.X.Y
```

**Schema migrations** are automatic — `ensure_schema` runs at boot
and applies any new migrations forward. The DB write-lock is released
when the old binary exits (the run_prod.sh process group sends SIGTERM
on Ctrl-C, drop handlers release the lock). If the relaunch fails with
"database is locked", check no stray aberp/aberp-ui process is running:
`pgrep -f aberp`.

**HU:** A `main` ág a fejlesztés gerince; a `prod` ágra fast-forward-
mergeléssel vagy cherry-pick-kel kerülnek a változtatások. A séma-
migrációk automatikusan futnak induláskor.

---

## Appendix A — File and keychain inventory

What lives where after a successful cutover:

| Location | Owner | Lifetime |
|----------|-------|----------|
| `~/.aberp/prod/seller.toml` | seller-config wizard / your editor | tenant-lifetime |
| `~/.aberp/prod/aberp.duckdb` | DuckDB | tenant-lifetime |
| `~/.aberp/prod/.first-launch-acknowledged` | first-launch ceremony | tenant-lifetime |
| `~/.aberp/serve/prod/` | TLS cert + key for the loopback listener | regenerated as needed |
| `~/.aberp/prod/invoices/<id>/` | side-store per-invoice artifacts (input.json, nav_xml, PDF) | invoice-lifetime |
| macOS keychain: `aberp.nav.prod` / `nav_credentials_blob` | `setup_nav_creds.sh` → CLI | tenant-lifetime |
| macOS keychain: `aberp.smtp.prod` / `smtp_password` | SPA Settings PUT | tenant-lifetime |
| macOS keychain: `aberp.nav.prod` / `session_token` | serve at boot | per-binary-build |

**Backups:** the DuckDB file IS the database. Snapshot it before any
upgrade (Step 8/9 instructions). Side-store directories
(`~/.aberp/prod/invoices/<id>/`) are also load-bearing — `input.json`
and `nav_xml` are referenced by audit replay and the PDF print path.
A backup strategy that covers `~/.aberp/prod/` entirely is the
simplest correct posture.

---

## Appendix B — Why the dev-DB-disposable rule reverses at prod cutover

During dev, `main` may include destructive migrations or schema
rewrites against a dev tenant's DuckDB. The dev DB is disposable —
delete and re-issue.

From the prod cutover onwards, **the prod DB is the legal record of
issued invoices**. Hungarian tax law requires retention of issued
invoices for years. The prod DB is no longer disposable; every
schema change must be forward-compatible, and the audit ledger is
append-only (ADR-0008).

This is the single most important behavioural shift at cutover, and
the reason the runbook bangs the snapshot-the-DB drum at every step.

---

## Appendix C — Quick reference card

| Need to... | Command |
|------------|---------|
| Build + tag a prod release | `./run/release.sh prod-vX.Y.Z` |
| Launch the prod app | `./run/run_prod.sh` |
| Set up / rotate NAV creds | `./run/setup_nav_creds.sh --tenant prod` |
| Set up / rotate SMTP creds | SPA → Settings → SMTP → Test Connection |
| Verify NAV creds are in keychain | `security find-generic-password -s "aberp.nav.prod" -a "nav_credentials_blob"` |
| Verify SMTP creds are in keychain | `security find-generic-password -s "aberp.smtp.prod" -a "smtp_password"` |
| Snapshot the DB | `cp ~/.aberp/prod/aberp.duckdb ~/.aberp/prod/aberp.duckdb.snapshot-$(date +%Y%m%d-%H%M%S)` |
| See recent audit entries | (via the SPA's audit timeline on the invoice detail page) |
| Roll back to previous tag | `git checkout prod-vX.Y.Z-prev && cargo build --release --features production --bin aberp-ui && ./run/run_prod.sh` |
| Re-trigger first-launch modal | `rm ~/.aberp/prod/.first-launch-acknowledged && ./run/run_prod.sh` |

---

*This runbook is the single source of truth for the prod cutover.*
*If you find a step that doesn't match reality, fix the runbook — it*
*is the artifact that survives across sessions.*
