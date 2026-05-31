# ABERP production cutover runbook

**Last updated:** 2026-05-31 — Session 201 / PR-201 (versioning policy
landed in ADR-0056; `release.sh` validator extended to accept the
3-segment `PROD_vX.Y.Z` patch shape; runbook placeholder examples
switched to `PROD_vX.Y[.Z]`).
**Prior update:** 2026-05-31 — Session 200 / PR-200 (`upgrade_prod.sh`
one-command hülye-biztos upgrade + `run_prod.sh` Frankenstein-build
refusal — see Step 9 rewrite).
**Prior update:** 2026-05-31 — Session 198 / PR-198 (troubleshooting
entries for the PROD_v1.1→v1.3 cutover wrinkles + Appendix A inventory
extended per ADR-0055 to name `ap-artifacts/`, `ap_invoice`, and
`restored_invoice`).
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
  password, XML SIGN key, XML CHANGE/EXCHANGE key. Four secrets. The
  NAV credentials wizard (Step 4) will read them silently via the SPA.
- [ ] **SMTP working in test**: a recent test invoice email landed
  successfully from the dev tenant. The prod SMTP creds will be the
  same Zoho account; only the **host is different** — Zoho EU mailboxes
  use `smtppro.zoho.eu` (NOT `smtp.zoho.com` or `smtp.zoho.eu` — those
  are different hosts on Zoho's own infrastructure).
- [ ] **~15-minute uninterrupted window**: cutover itself is fast, but
  if a smoke invoice fails you want to debug while the binary is fresh
  in your head.
- [ ] **Audit ledger reviewed**: `git log --oneline -20` on `main`.
  Confirm the HEAD commit you're cutting over from is the one you
  expect.
- [ ] **Working tree clean** on the dev repo: `git status` shows
  nothing pending.
- [ ] **Coffee**.

---

## Step 1 — Publish the release branch (from the dev clone)

The S169 release model uses **per-release branches** on origin, not
tags. Dev publishes the ref; the prod machine clones from that ref and
builds locally. This decouples the dev tooling from the prod artifact —
an icons/ regression on dev cannot silently reach prod (the 2026-05-30
white-screen failure mode this PR exists to prevent).

```bash
# On the DEV machine, from the dev checkout (~/Documents/Claude/Projects/ABERP):
cd ~/Documents/Claude/Projects/ABERP
git status                         # must be clean
git checkout main
git pull --ff-only origin main     # match origin/main exactly
git push origin main               # if any local-only commits remain

# Publish the release branch:
./run/release.sh PROD_v1.0
```

`release.sh` will:

1. Refuse to run if invoked from inside `/Documents/Claude/Projects/` —
   the dev-workspace sentinel guards against running it from the wrong
   clone after cutover.
2. Refuse unless you're on `main` with a clean tree.
3. Validate the version matches `PROD_vMAJOR.MINOR[.PATCH]` (uppercase,
   underscore; 2-segment OR 3-segment per ADR-0056). 4+ segments and
   suffixes (e.g. `-rc1`) are rejected.
4. Refuse if `PROD_v1.0` already exists on origin.
5. `git push origin main:refs/heads/PROD_v1.0`.
6. Print the operator's `git clone --branch …` command for Step 2.

> **Bootstrap caveat for the very first cutover only:** the dev-sentinel
> is in the script to support the *post-cutover* steady state, where
> release.sh is run from the prod clone. For the first cutover, you
> are necessarily running from the dev clone — bypass the sentinel
> just this once by doing the push manually:
>
> ```bash
> git push origin main:refs/heads/PROD_v1.0
> ```
>
> After cutover, all future releases run release.sh from the prod
> clone (Step 9), and the sentinel does its job.

**HU:** A release.sh feltolja a `PROD_vX.Y` ágat az originra; a
következő lépésben az éles gépen klónozod le erről az ágról.

---

## Step 2 — Clone the prod repo on the prod machine

On the prod machine (or in a fresh directory outside the dev workspace):

```bash
cd ~
git clone --branch PROD_v1.0 <origin-url> ABERP-prod
cd ABERP-prod
```

This gives you a clean working tree pinned to the release ref. All
subsequent steps run from inside `ABERP-prod/`.

> **Why a clone (not a worktree, not a copy)?** A clone is the smallest,
> most explicit unit. It carries its own `.git`, can be pulled
> independently for future releases, and is impossible to confuse with
> the dev checkout. (Background: parallel dev sessions occasionally
> `reset --hard` the shared dev checkout — a fresh clone is immune.)

**HU:** Klónozd a `PROD_v1.0` ágról egy fejlesztői munkamappán kívüli
helyre. Minden következő lépés ebből a mappából fut.

---

## Step 2a — Pre-build sanity checks (CRITICAL)

Two fresh-clone gotchas can both surface as a white window with no logs.
Verify them BEFORE running `run_prod.sh` so the diagnosis time isn't lost
later.

**(i) SPA embedding — the real 2026-05-30 culprit.** `apps/aberp-ui/ui/dist/`
holds the built SPA. The directory is gitignored, so a fresh clone has
NO built SPA. `run_prod.sh` (S169-onward) runs `npm install && npm run build`
in `apps/aberp-ui/ui/` automatically before cargo, then sanity-checks
`dist/index.html` exists. The Tauri dep also has the `custom-protocol`
feature enabled (PR-169) so the release binary serves embedded assets via
the `tauri://localhost` scheme instead of falling back to `devUrl`. If
either condition is missing, the binary opens a window that tries to load
`http://localhost:5173` (Vite dev server), and you see a white screen
unless Vite is running in parallel.

You don't usually need to do anything for (i) — `run_prod.sh` handles it.
But you can confirm the binary embeds the SPA after a build:

```bash
strings target/release/aberp-ui | grep -c "svelte-"
# Expect a large number (hundreds+). Zero means the SPA isn't embedded —
# do NOT launch; re-run run_prod.sh from a clean tree.
```

**(ii) Tauri icons.** `apps/aberp-ui/icons/` must contain non-zero icon
files. Missing or zero-byte icons can also cause a silent WebView init
failure (NSImage init returns nil on bad icon data).

```bash
ls -l apps/aberp-ui/icons/
# Expect: 32x32.png, 128x128.png, 128x128_2x.png,
#         icon.png, icon.icns, icon.ico
```

The release branch ships placeholder icons. If any file is missing or
zero-byte, regenerate from the script:

```bash
python3 tools/generate-icons.py
```

> **Áben branding (deferred):** the icons in the repo today are a
> deliberately simple placeholder (dark navy + white "ABERP" wordmark).
> Real Áben branding will land in a follow-up PR when the logo file is
> available. To swap, drop a square PNG (≥1024×1024) at
> `tools/source-logo.png` and re-run `python3 tools/generate-icons.py`.

**HU:** A Tauri 2 hibásan vagy hiányosan megadott ikonok esetén csendben
üres fehér ablakot mutat (NSImage init nil, hibaüzenet a logban nincs).
A release-ág placeholder ikonokat tartalmaz; ellenőrizd, hogy léteznek
és nem 0 bájtosak, mielőtt buildelnél.

---

## Step 3 — Set up the prod seller config (via SPA wizard)

The prod tenant lives at `~/.aberp/prod/`. The launcher creates the
directory on first run; you populate `seller.toml` **through the SPA
seller wizard** (PR-51 / session 71). No hand-editing required.

```bash
./run/run_prod.sh
```

What `run_prod.sh` does on first launch (S169 — newly load-bearing):

0. `(cd apps/aberp-ui/ui && npm install --silent && npm run build)` —
   builds the SPA into `ui/dist/`. The Tauri compiler embeds these
   files into the release binary; without them you get a white window
   on launch (see Troubleshooting).
1. Sanity-checks `apps/aberp-ui/ui/dist/index.html` is non-zero. Aborts
   loud if not.
2. `cargo build --release --features production --bin aberp`
3. `cargo build --release --features production --bin aberp-ui` —
   the tauri dep has the `custom-protocol` feature always-on (PR-169,
   `apps/aberp-ui/Cargo.toml`), which registers the `tauri://localhost`
   URI scheme the WebView uses to serve the embedded SPA in release
   builds. Without that feature, the binary falls back to `devUrl`
   (Vite at :5173) — the 2026-05-30 failure mode.
4. `cargo run --release --features production --bin aberp-ui` launches
   the Tauri shell. The shell spawns `aberp serve` as a subprocess
   and the SPA loads from embedded assets.

What happens inside the binary as it boots:

1. Backend boots, detects `~/.aberp/prod/seller.toml` is absent (or
   identity-incomplete) → boot state `NeedsSellerConfig`.
2. SPA renders the seller wizard. Fill the form:
   - Legal name: `Áben Consulting KFT.`
   - Tax number: `24904362-2-41` (the S166 boot sanity check refuses
     any other value for prod — this is intentional)
   - EU VAT number: `HU24904362`
   - Address (country `HU`, postal code, city, street + house number)
   - Bank details (account number, IBAN, bank name, SWIFT/BIC)
3. SPA POSTs the form; backend atomically writes
   `~/.aberp/prod/seller.toml`, including the
   `[seller.numbering]` section that yields `ABERP/2026/0001`
   (annual reset).
4. SPA reloads; boot state transitions to the next missing-thing
   (likely `NeedsKeychainCreds`, see Step 4).

> **Why a wizard, not hand-edited TOML?** The wizard's atomic write
> preserves invariants the operator can't easily replicate (S148
> seller.toml write-path invariant — bank section and identity section
> must round-trip without clobbering each other). Hand-editing risks
> losing one section if you forget to re-append the other.

**HU:** Az eladó-konfigurációt a SPA-ban beépített varázslóval töltsd
ki — ne kézzel szerkeszd a TOML-t. A backend atomikusan írja ki és
megőrzi az invariánsokat (S148).

### Step 3a — Drop the tenant logo (optional, PR-176)

Place a PNG at `~/.aberp/<tenant>/logo.png` to brand the printed
invoice header. Recommendations:

- PNG only for v1 (SVG / JPG flagged as a follow-up).
- ≤ 512×512 pixels — the renderer fits it inside a 50×50-point box
  top-left of the header with aspect ratio preserved, so any square-
  ish source works; oversized PNGs just waste decode bytes.
- Transparent background (RGBA) is fine — alpha is dropped to RGB
  against the page's implicit white background, so transparent edges
  display their RGB ink (no black halo).

Absent file → the header renders text-only (pre-PR-176 layout, all
other invoice content unchanged). Malformed PNG → loud render error
(re-export the file). No `seller.toml` edit, no DB row — convention
only.

```bash
cp ~/Downloads/our-logo.png ~/.aberp/prod/logo.png
# Verify by re-rendering an existing invoice's PDF; the logo appears
# top-left of the header above the silver under-rule.
```

---

## Step 4 — Populate NAV credentials (via SPA wizard)

After Step 3 saves, the backend re-evaluates boot state. If NAV
credentials are absent **and** the first-launch ceremony has not yet
been acknowledged, the SPA renders the NAV credentials wizard (S133
keychain prompt chaining).

Fill the four fields:

| Field                       | Source                            |
|-----------------------------|-----------------------------------|
| Technical-user login        | NAV invoice service registration  |
| Technical-user password     | NAV invoice service registration  |
| Software ID                 | NAV registered software ID        |
| Exchange (CHANGE) key       | NAV invoice service registration  |

Click **Save**. The SPA POSTs to the keychain setup route, which writes
the macOS keychain entry at:

- service: `aberp.nav.prod`
- account: `nav_credentials_blob` (consolidated blob per PR-57; four
  secrets inside)

The ACL persists across launches; you will not be re-prompted on next
boot.

**Verify** the keychain write landed (optional sanity check; the SPA
already confirmed):

```bash
security find-generic-password -s "aberp.nav.prod" -a "nav_credentials_blob" -g
# (prompts for your macOS login password; you don't need to read the
#  value — just confirm the entry exists.)
```

**HU:** A NAV technikai-felhasználói adatokat a SPA varázsló kéri be
és írja a macOS kulcstartóba. Nincs CLI-parancs erre — a varázsló az
egyetlen ajánlott út.

---

## Step 5 — Set up SMTP credentials (via SPA Tenant Settings)

SMTP is configured through **Tenant Settings → SMTP delivery** in the
SPA (PR-92, ADR-0047). The password is write-only in the UI — once
saved it is never re-displayed, only re-entered.

Navigate to: Tenant Settings → SMTP delivery → fill in:

| Field         | Value                                             |
|---------------|---------------------------------------------------|
| Host          | **`smtppro.zoho.eu`** (NOT `.com`, NOT plain `.eu`) |
| Port          | `465`                                              |
| Security      | TLS (implicit) — Zoho EU's `smtppro` listener on 465 |
| From address  | your Zoho mailbox address                          |
| Username      | your Zoho mailbox address (same as From)           |
| Password      | Zoho **app-specific password** (NOT your account password) |

Click **Test Connection** (PR-98) — the backend opens a TLS connection
to the host, runs AUTH, and sends a one-line test email to yourself.
**Do not proceed past this step until Test Connection succeeds.** A
failed Test Connection now means a failed smoke-invoice email in
Step 7.

On success, click **Save**. The backend writes the password to the
macOS keychain at:

- service: `aberp.smtp.prod`
- account: `smtp_password`

> **Zoho EU pitfall** — `smtp.zoho.com` is Zoho's US infra; `smtp.zoho.eu`
> exists but is for non-pro accounts. The Zoho **Workplace Pro** EU
> tenant uses `smtppro.zoho.eu` specifically. Authenticating with the
> wrong host will surface as TLS handshake or AUTH failures; the SPA's
> "Test Connection" button is the fast way to confirm before the first
> live email.

**HU:** SMTP-t a SPA Beállítások → SMTP-szállítás oldaláról állítod be.
A Zoho EU **Workplace Pro** host pontos neve: `smtppro.zoho.eu` (a
sima `smtp.zoho.eu` MÁS, és nem fog működni). A **Kapcsolat tesztelése**
gomb sikeres futása előtt NE LÉPJ TOVÁBB.

---

## Step 6 — First-launch ceremony

After Steps 3–5, the SPA renders the **first-prod-launch modal** (S166).
The touchfile `~/.aberp/prod/.first-launch-acknowledged` is absent on
first boot, so all main routes are blocked behind a confirmation modal.

You must type **`ABERP`** (uppercase, exact). On submit:

- The touchfile is written with an RFC3339 timestamp.
- A `FirstProdLaunchAcknowledged` entry is appended to the audit ledger.

This is one-time. Subsequent launches skip the modal.

What the binary checked at boot (S166 `sanity_check_environment` —
informational, in case you see the loud-fail banner):

- **A. Seller identity** — `seller.toml` must have
  `tax_number = "24904362-2-41"`. Mismatch = fatal. Missing file =
  deferred to the seller wizard (Step 3).
- **B. NAV credentials** — keychain entry must exist if the
  first-launch ceremony was previously acknowledged. On first boot this
  gate is permissive (the wizard populates).
- **C. SMTP** — missing `[seller.smtp]` is a **warning**, not fatal;
  configure via Step 5 after launch.

The loud-banner module also prints "PRODUCTION BUILD — REAL NAV — REAL
MONEY" in red/yellow on the launching terminal (bilingual EN+HU) before
the binary takes over.

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
- Email `Failed`: check the SMTP host. `smtppro.zoho.eu` is the right
  host for the EU pro tenant (Step 5).
- PDF wrong: re-check the seller wizard data (you can edit values via
  the SPA after first-launch; in extreme cases clear
  `~/.aberp/prod/seller.toml` to retrigger the wizard).
- White window with no logs: see **Troubleshooting** at the end of this
  runbook.

If the smoke invoice lands cleanly, **prod is live**. The S168 PDF fix
(re-source PRIVATE_PERSON buyer name/address from input.json) is
already in this release; you should not see address-empty regressions.

**HU:** A smoke invoice egy valódi (de általad kontrollált) NAV-beadás
+ e-mail küldés. Csak akkor lépj tovább a valódi szabályos
számlázásra, ha a teljes lánc lement.

---

## Step 8 — Rollback procedure

If something goes wrong after a release lands and you've already
issued one or more invoices, **do not panic and do not delete
anything**.

The audit ledger is append-only (ADR-0008) and `ensure_schema` is
idempotent — rolling back to a previous prod build will not corrupt
any prior invoice's audit trail.

To roll back to a previous release branch:

```bash
# 1. Stop the running app (Ctrl-C in the run_prod.sh terminal).
# 2. Inside the prod clone, switch to the previous release branch.
cd ~/ABERP-prod
git fetch origin
git checkout PROD_v0.9      # whichever previous release-branch you want
# 3. Relaunch — run_prod.sh rebuilds with the new HEAD.
./run/run_prod.sh
```

The DuckDB file at `~/.aberp/prod/aberp.duckdb` is preserved across
binary versions; migrations are forward-only and run at boot
(`ensure_schema`). A rollback launches against the existing DB with
the previous code; if the previous code's schema is older, **do not
roll back across a destructive migration without first restoring a
DB snapshot from before the migration ran**.

> **Snapshot the full prod state before any cutover or upgrade.**
> Belt-and-suspenders backup (S170 / PR-170 — captures `~/.aberp/<tenant>/`
> AND the per-tenant macOS keychain entries to a password-protected zip):
> ```bash
> ./tools/snapshot-prod.sh
> ```
> Do this before Step 2 and before any future Step 9 upgrade.
> Snapshots land under `~/aberp-snapshots/`. The DuckDB-only one-liner
> below is still valid as a quick-and-dirty fallback when you only need
> a DB rollback target (no keychain, no seller.toml):
> ```bash
> cp ~/.aberp/prod/aberp.duckdb \
>    ~/.aberp/prod/aberp.duckdb.snapshot-$(date +%Y%m%d-%H%M%S)
> ```

**HU:** A rollback biztonságos, mert az audit ledger csak hozzáfűzhető,
és a séma-migrációk ütközésmentesek (`ensure_schema` idempotens). DB-
snapshotot mindig csinálj BÁRMILYEN frissítés előtt.

**If you need to invalidate an issued invoice**, that's NOT a rollback
— that's a stornó. Use the SPA's storno workflow (S156); don't try to
fix it by reverting code.

---

## Step 9 — Ongoing update workflow

Routine: dev work continues to land on `main`. When you want a fix or
feature to reach prod, **one command** does the whole upgrade on the
prod side. S200 / PR-200 introduced `./run/upgrade_prod.sh` after Ervin
hit the bare-`git checkout`-on-dirty-tree failure three times across
PROD_v1.0 → v1.4 cutovers; the script enforces the BEFORE-snapshot
ordering, refuses to swap a running binary, and does the full clean
git switch (`fetch` → `reset --hard` → `clean -fd` → `checkout -B`)
that the operator was doing piecewise (and getting wrong).

```bash
# 1. From the DEV clone: publish a new release branch.
cd ~/Documents/Claude/Projects/ABERP
git checkout main
git pull --ff-only origin main
./run/release.sh PROD_v1.5         # bump the minor

# 2. On the PROD clone: Ctrl-C the running prod app in its terminal
#    (do NOT skip this — upgrade_prod.sh refuses to swap a running
#    binary by design).

# 3. On the PROD clone: ONE command runs the whole upgrade.
cd ~/ABERP-prod
./run/upgrade_prod.sh PROD_v1.5
# (will prompt twice for the snapshot's encryption password — pick
#  one you can remember; you need it to restore from this snapshot.)

# 4. Smoke-test on a low-stakes path before bulk-issuing.
```

What `upgrade_prod.sh` does, in order (all failures loud-fail with
bilingual error + recovery hint):

1. Validates the version arg matches `PROD_v<MAJOR>.<MINOR>[.<PATCH>]`
   (2-segment OR 3-segment per ADR-0056).
2. Refuses if invoked from the dev workspace (path-substring sentinel,
   same as `release.sh`). Opt-out: `ABERP_ALLOW_DEV_WORKSPACE=1`.
3. Verifies `origin` works + the release branch exists on it
   (`git ls-remote --exit-code --heads origin <branch>`).
4. Verifies `~/.aberp/<tenant>/seller.toml` exists (refuses an upgrade
   against a tenant that was never set up — that's the cutover path,
   not the upgrade path).
5. Refuses if `aberp-ui` or `aberp` is still running. The operator
   must Ctrl-C the run_prod.sh terminal first; don't hot-swap a
   running binary.
6. **Snapshots BEFORE switching** — runs `./tools/snapshot-prod.sh
   <tenant>` (captures the full tenant dir + the per-tenant macOS
   keychain entries to a password-protected zip under
   `~/aberp-snapshots/`, plus the `[seller.smtp]` + `[seller.numbering]`
   contract at `~/.aberp/<tenant>/.upgrade-snapshot.toml` that S171's
   boot-time drift check reads). Operator can't forget the
   BEFORE-not-after ordering: the script does it.
7. Full clean git switch: `git fetch` → `git reset --hard HEAD`
   → `git clean -fd` → `git checkout -B <branch> origin/<branch>`
   → `git reset --hard origin/<branch>` (belt-and-suspenders).
8. Prunes stale local `PROD_v*` branches (everything except the
   target). Local `main` is left alone (operator may want it).
9. Verifies clean tree + `HEAD == origin/<branch>` before launching.
10. `exec ./run/run_prod.sh` — transfers control; one terminal, one
    continuous output stream. Ctrl-C in this terminal exits the
    relaunched app.

When the binary boots after the swap, `run_prod.sh` also re-checks
the working tree is clean and HEAD matches an `origin/PROD_v*` branch
(S200 / PR-200 Frankenstein-build refusal). If the operator somehow
got into a state where the tree drifted between `upgrade_prod.sh`
finishing and `run_prod.sh` starting, the launcher refuses. Opt-out:
`ABERP_SKIP_GIT_CHECK=1` (dev workflows only).

### Manual escape hatch

If `upgrade_prod.sh` itself errors and you need to force the upgrade
manually (very first cutover from a pre-S200 clone; script not yet
present on disk; or a corrupted `tools/snapshot-prod.sh` you need to
work around), here's the equivalent raw sequence:

```bash
# 1. Snapshot first — BEFORE, not after.
./tools/snapshot-prod.sh prod

# 2. Stop the running app (Ctrl-C in the run_prod.sh terminal).

# 3. Full clean switch.
cd ~/ABERP-prod
git fetch origin
git reset --hard HEAD                            # drop tracked mods
git clean -fd                                    # drop untracked
git checkout -B PROD_v1.5 origin/PROD_v1.5       # create/reset + switch
git reset --hard origin/PROD_v1.5                # belt-and-suspenders

# 4. Verify.
git status              # must be clean
git rev-parse HEAD      # must match origin/PROD_v1.5

# 5. Launch.
./run/run_prod.sh
```

The `upgrade_prod.sh` script automates this sequence and adds the
checks (running-binary, branch-exists, dev-sentinel) — prefer the
script whenever it's available.

### Restoring from a snapshot

If the new release loses operator state (or anything else goes wrong
that warrants a full rollback to pre-upgrade), the snapshot artifacts
land under `~/aberp-snapshots/`:

- `<tenant>-<timestamp>.tgz` — the tenant directory.
- `<tenant>-<timestamp>-keychain.zip` — encrypted keychain dump.

```bash
# 1. Stop the running app (Ctrl-C in the run_prod.sh terminal).
# 2. Restore the tenant directory in-place. The tarball expands to
#    `prod/` under ~/.aberp/, so cd into the parent first.
#    NOTE: this OVERWRITES the current ~/.aberp/prod/ directory.
cd ~/.aberp
tar -xzf ~/aberp-snapshots/prod-20260601-143022.tgz   # pick the right ts

# 3. Restore keychain entries from the encrypted zip. The zip contains
#    one JSON file; unzip with the password you set at snapshot time.
cd /tmp
unzip ~/aberp-snapshots/prod-20260601-143022-keychain.zip
# Then for each entry in the JSON, re-import:
#   security add-generic-password -U -s <service> -a <account> -w <password>
# (The `-U` flag updates an existing entry instead of failing on duplicate.)
# Don't leave the unzipped JSON on disk — `shred -uz` it after you're done.

# 4. Relaunch with the prior release branch (Step 8 procedure).
cd ~/ABERP-prod
git checkout PROD_v1.0    # whichever release matched the snapshot
./run/run_prod.sh
```

**Schema migrations** are automatic — `ensure_schema` runs at boot
and applies any new migrations forward. The DB write-lock is released
when the old binary exits (the run_prod.sh process group sends SIGTERM
on Ctrl-C, drop handlers release the lock). If the relaunch fails with
"database is locked", check no stray aberp/aberp-ui process is running:
`pgrep -f aberp`.

**HU:** A `main` ág a fejlesztés gerince; minden éles frissítéshez egy
új `PROD_vX.Y` ágat hozunk létre (release.sh push-olja). Az éles gépen
csak `git fetch && git checkout PROD_vX.Y && ./run/run_prod.sh` — a
többi automatikus.

---

## Troubleshooting

### Blank white window on launch (no logs, no error)

**Symptom:** `./run/run_prod.sh` finishes the cargo build, launches
aberp-ui, but the window renders fully white. No errors in stdout,
stderr, or the Console.app system log.

**Most likely cause (S169 — was the 2026-05-30 cutover root cause):**
the release binary is loading the SPA from `http://localhost:5173`
(Vite dev server) instead of from embedded assets. Two prerequisites
must BOTH be true for the embed path to work:

1. `apps/aberp-ui/Cargo.toml` has `custom-protocol` in the tauri dep's
   feature list. PR-169 added it (always-on); if your branch predates
   that commit, the binary cannot serve embedded assets regardless of
   `frontendDist`.
2. `apps/aberp-ui/ui/dist/index.html` exists at the time `cargo build`
   runs. `dist/` is gitignored — fresh clones have nothing here.
   PR-169 `run_prod.sh` runs `npm install && npm run build` before
   cargo to guarantee this.

**Quick diagnosis:**

```bash
# Is anything listening on the Vite port? In prod you want NO.
lsof -i :5173

# Does the built binary contain the SPA?
strings target/release/aberp-ui | grep -c "svelte-"
#   >100 → SPA is embedded. White screen is NOT this cause.
#   0    → SPA missing from binary. Re-run with the fix below.
```

**Fix:**

```bash
# Force a fully clean rebuild.
cargo clean -p aberp-ui
rm -rf apps/aberp-ui/ui/dist apps/aberp-ui/ui/node_modules
./run/run_prod.sh
```

If you confirm SPA strings are >100 AND :5173 is empty AND the window
is still white, the secondary cause is **missing or malformed Tauri
icons** under `apps/aberp-ui/icons/`. macOS `NSImage` init returns nil
on bad icon data; the WebView never reaches `loadURL`.

```bash
ls -l apps/aberp-ui/icons/
# Any zero-byte file → regenerate.
python3 tools/generate-icons.py
# Then rebuild + relaunch:
./run/run_prod.sh
```

If `tools/generate-icons.py` itself errors with "PIL not installed":
`pip3 install --break-system-packages Pillow` and retry.

### SMTP "Test Connection" times out

Usually wrong host. `smtppro.zoho.eu` (NOT `.com`, NOT plain `.eu`).
See Step 5.

If you confirmed the host is right and it still times out: check the
firewall — port 465 outbound must be open. macOS Application Firewall
(System Settings → Network → Firewall) is fine by default; corporate
VPNs sometimes block 465 specifically.

### NAV submit stuck at `RECEIVED` for >5 minutes

The S161 poll daemon is meant to escalate from 1/2/4/8/16/30/60s to
steady 60s. If `RECEIVED` does not transition to `PROCESSING`:

- Check the NAV status page: <https://onlineszamla.nav.gov.hu/>
- The audit ledger entry shows the request URL and timestamps —
  compare against the daemon's expected next-poll time.
- `pgrep -f aberp` should show the running binary; if it's not there,
  the daemon died — relaunch the app and the boot-recovery path
  (S161) will resume polling.

### "Working tree dirty" from release.sh after a clean pull

Usually `target/` or `node_modules/` getting touched by an editor's
sidecar process. `git status --short` will name the culprit; the
fixes are usually either:

- `cargo clean` followed by a fresh build (target/), OR
- `rm -rf node_modules apps/aberp-ui/ui/node_modules && (cd apps/aberp-ui/ui && npm install)`.

If `git status --short` shows changes you didn't make and don't
recognise, **stop and investigate** — do not blindly `git checkout .`,
that destroys in-progress dev work from parallel sessions (see memory
`feedback_aberp_shared_checkout_concurrent_branch_hopping`).

### `PROD_vX.Y already exists on origin` error

Pick the next minor. The script suggests it in the error message.

### Forgot to snapshot before upgrade

**Symptom:** you've already run `git checkout PROD_vX.Y` and
`./run/run_prod.sh` against the new release before snapshotting.

**Diagnosis:** the snapshot script captures CURRENT state. If you run
it now, the snapshot is post-upgrade — it cannot serve as a rollback
handle for the upgrade itself (the bug you'd want to roll back from is
already in the snapshot). The S171 `.upgrade-snapshot.toml` contract
defense (boot-time SMTP + numbering drift check) ran at the new
binary's first boot and either accepted the post-upgrade state or
refused to start; the boot acceptance is the strongest signal that
seller.toml is intact.

**What to do:**

1. Run `./tools/snapshot-prod.sh` NOW anyway. It captures the current
   (post-upgrade) state, which is the best available rollback target
   for the NEXT upgrade. The current upgrade does not have one.
2. Verify the SPA renders correctly + a smoke invoice round-trips
   end-to-end (Step 7 procedure). If something is wrong, you have no
   automated rollback handle for the upgrade — surface the
   regression, fix it forward in a `PROD_vX.Y+1`, never `git reset
   --hard` against a release branch you've already booted against
   (which would orphan whatever the new binary wrote post-boot).
3. Internalize the **BEFORE, not after** rule (Step 9). The runbook
   was updated 2026-05-31 to make the ordering louder; if the wording
   still didn't bite, surface the gap and we'll sharpen it again.

**Why the bite is sharp:** Hungarian tax law requires retention of
issued invoices for years. The prod DB is no longer disposable; an
upgrade-introduced regression that corrupts a write path between the
old binary and the new one is the worst-class incident. The snapshot
is the only rollback handle that survives a binary swap.

### Numbering literal drift between an issue and its chain ops

**Symptom:** you issued invoice N, and then edited the numbering
section of `seller.toml` (or worse — re-typed it via the seller wizard)
before issuing a storno or modification against N. The chain op fails
with NAV reference mismatch or local-validation mismatch.

**Diagnosis:** the chain op (storno / modification / annulment)
renders the base invoice number from the **current** seller.toml
template, then compares against the **base's stored** number. If the
template literal drifted (`ABERP/` → `ÁBEN/` say), the renders no
longer agree and the op refuses.

PR-184 + PR-190 defended the most common failure modes — the storno
chain re-reads the base's on-disk NAV XML for `<originalInvoiceNumber>`
instead of re-deriving from the template, and ADR-0051 / PR-198 pins
that the base year reads from the billing DB row, not the audit-ledger
wall clock. But the seller.toml template literal itself remains
operator-editable; a literal change between issue and chain ops will
still surface as a refusal at the boundary that does the comparison.

**What NOT to do:** do not edit `seller.toml`'s `[seller.numbering]`
section between an invoice issuance and any chain operation against
that invoice. The literal is part of the invoice's identity — once
issued, the literal is frozen for that invoice's chain forever.

**What to do if you already did:** revert the `seller.toml`
`[seller.numbering]` section to the literal in effect at the base's
issue time. The base invoice's NAV XML on disk records the literal it
was issued with (`<invoiceNumber>` field); use that as the source of
truth. Then re-attempt the chain op.

**Future invariants:** a "freeze the numbering template once an
invoice has been issued under it" enforcement is named-deferred; the
trigger is a recurring operator report of this failure mode. Today's
posture is "operator knows not to do this".

### macOS Dock shows stale app icon after binary swap

**Symptom:** you swapped to a new release branch that ships new
`apps/aberp-ui/icons/` assets, but the Dock still shows the old icon
even after relaunching the app.

**Diagnosis:** macOS aggressively caches Dock icons. The new binary
loads the new icons internally (the window's title-bar icon should
already show the new asset), but the Dock's cached representation
lags until the Dock process restarts.

**Fix:**

```bash
killall Dock
# Dock restarts immediately; the new icon appears on next app launch.
```

This is cosmetic; no functional impact. Skip the fix if you don't
care which icon the Dock shows.

### Cargo lock held by a stale process

**Symptom:** `cargo build` reports `error: failed to acquire packages
file lock: would block` or `error: waiting for file lock on package
cache`, and waits indefinitely.

**Diagnosis:** another `cargo` process is holding the
`~/.cargo/registry/.cargo-lock` or the project's `target/` lock. Most
common cause: a previous `run_prod.sh` invocation that was
Ctrl-C'd at the wrong moment and left a zombie `cargo` worker, or a
parallel session running cargo against the same workspace.

**Fix:**

```bash
ps aux | grep -E '(cargo|rustc)' | grep -v grep
# Identify the stale PID(s) — usually the oldest cargo process with
# no terminal attached. Kill it:
kill <PID>
# If it doesn't die in 5 seconds, escalate:
kill -9 <PID>
# Then retry the build.
```

If the lock is in the project's `target/` (not the global registry),
`rm target/.rustc_info.json` is sometimes enough; if not, `cargo
clean` and rebuild.

### Shell scripts fail with "Permission denied"

**Symptom:** `./run/run_prod.sh` or `./tools/snapshot-prod.sh` errors
with `zsh: permission denied: ./run/run_prod.sh` (or equivalent
bash error). The script is present and readable; it just won't
execute.

**Diagnosis:** the executable bit got dropped. Common cause: a `chmod
-R` against the repo for unrelated reasons (e.g., "fix permissions
after restoring from a non-Unix backup"), or a `git` config that
strips the executable bit. The S169 `release.sh` and S170
`snapshot-prod.sh` ship with mode `0755`; any operation that
re-applies a restrictive umask resets them.

**Fix:**

```bash
chmod +x run/*.sh tools/*.sh
# Verify:
ls -l run/*.sh tools/*.sh
# Expect the leftmost field to show -rwxr-xr-x (or similar with x bits).
```

Then retry the script. If the bits keep getting stripped, check `git
config core.fileMode` — `true` means git respects the file mode bit;
`false` ignores it (and a `git checkout` could appear to restore
without restoring the bit).

---

## Step 10 — (Optional, opt-in) Enable the quote-intake daemon (S210 / PR-204)

The quote-intake daemon polls a sister storefront (ABERP-site) for
**approved quotes** and stages them as pending operator-pickup rows
in `quote_intake_log`. Disabled by default — set
`ABERP_QUOTE_INTAKE_ENABLED=true` to opt in.

**S210 ships the backend infrastructure only.** The operator-facing
SPA queue + tenant-settings UI land in S211; the version bump to
`PROD_v2.0.0` lands in S212. Until then this step is OPTIONAL and
operator-pickup of staged rows is via a future SPA route.

### When to enable

- You run a sister storefront (`ABERP-site`) and want approved
  quotes to flow into ABERP as pre-prepared invoice drafts.
- You're ready to operate the queue by hand pending S211.

### Configuration

```bash
# In the prod shell that launches `aberp serve` (e.g. `run_prod.sh`'s env):
export ABERP_QUOTE_INTAKE_ENABLED=true
export ABERP_QUOTE_INTAKE_URL=https://aberp-site.example.com   # NO trailing slash
export ABERP_QUOTE_INTAKE_TOKEN=<matches ABERP-site's ABERP_SITE_ADMIN_TOKEN>
# Optional — default 60s; clamped to [10, 3600]:
export ABERP_QUOTE_INTAKE_INTERVAL_SECS=300
```

**Refuse-to-start contract.** Setting `ENABLED=true` without `URL`
AND `TOKEN` causes the next `aberp serve` boot to abort loud. That's
intentional per the trust posture (`[[trust-code-not-operator]]` in
the auto-memory) — better than silently polling a wrong URL.

### What it writes

- One row per fresh quote into the per-tenant `quote_intake_log`
  DuckDB table (`quote_id` PRIMARY KEY → idempotent on the same
  quote).
- One `system.quote_intake_poll_completed` audit-ledger entry per
  CYCLE that saw work (fetched > 0, or any failure). No-op cycles
  are silent in the audit chain; the daemon's `info!` log line still
  reports them.

### What it does NOT do

- Does NOT touch the canonical `invoice` table. Sequence-burn stays
  operator-gated.
- Does NOT log the bearer token.
- Does NOT copy CAD files into ABERP. Files stay on ABERP-site.

### Verifying it's running

After enabling + relaunching `aberp serve`, look for one of these
two log lines in the boot output:

- `spawning quote-intake daemon (S210 / PR-204) cadence_secs=…`
  — daemon is up.
- `quote-intake daemon disabled (ABERP_QUOTE_INTAKE_ENABLED not 'true'); skipping spawn`
  — daemon is dormant.

Once running, every cycle emits a single `info!` log line:
`quote-intake cycle complete fetched=… created=… skipped=… …`.

### Disabling

Unset (or set to anything other than `true`) the
`ABERP_QUOTE_INTAKE_ENABLED` env and relaunch. The DuckDB table
stays put; you can drop it manually if no longer needed.

### Dev-only reset recipe (NOT for prod)

Per the `[[dev-db-disposable]]` memory the dev DB is disposable. To
re-process every approved quote in a dev environment:

```sql
DROP TABLE quote_intake_log;
```

The next cycle re-ingests everything; writeback is idempotent on
the ABERP-site side (`writeQuoteAtomic` always overwrites + appends
to `status_history`).

**Never run this on a prod tenant.** The prod `quote_intake_log` is
the audit trail of which quotes the daemon ingested and when; the
operator-pickup SPA (S211) reads it. Dropping it loses the
operator's pickup queue.

---

## Appendix A — File and keychain inventory

What lives where after a successful cutover:

| Location | Owner | Lifetime |
|----------|-------|----------|
| `~/.aberp/prod/seller.toml` | SPA seller wizard (PR-51) | tenant-lifetime |
| `~/.aberp/prod/aberp.duckdb` | DuckDB | tenant-lifetime |
| `~/.aberp/prod/aberp.audit.log` | audit-ledger mirror file (ADR-0030) | tenant-lifetime |
| `~/.aberp/prod/.first-launch-acknowledged` | first-launch ceremony (S166) | tenant-lifetime |
| `~/.aberp/prod/.upgrade-snapshot.toml` | `snapshot-prod.sh` (S171 — pre-upgrade SMTP + numbering contract) | per-upgrade |
| `~/.aberp/prod/logo.png` | operator-supplied tenant logo (PR-176) | tenant-lifetime, optional |
| `~/.aberp/prod/invoices/<id>/` | side-store per-invoice artifacts (input.json, nav_xml, PDF) | invoice-lifetime |
| `~/.aberp/prod/ap-artifacts/<apinv-id>.xml` | AP incoming-invoice NAV XML side-store (S177 + S197) | invoice-lifetime |
| `~/.aberp/serve/prod/` | TLS cert + key for the loopback listener | regenerated as needed |
| DuckDB table: `invoice` (+ related billing rows) | issue / storno / modification (ADR-0019) | tenant-lifetime |
| DuckDB table: `ap_invoice` | AP module mirror (S177); status changes audit-logged via `IncomingInvoiceStatusChanged` (ADR-0054) | tenant-lifetime |
| DuckDB table: `restored_invoice` | NAV-as-DR restore mirror (S180); segregated from canonical `invoice` table per ADR-0019 no-FK / named-table convention | tenant-lifetime |
| DuckDB table: `quote_intake_log` | Sister-service quote-intake staging (S210 / PR-204); operator pickup surface lands in S211 | tenant-lifetime |
| macOS keychain: `aberp.nav.prod` / `nav_credentials_blob` | SPA NAV creds wizard (S133) | tenant-lifetime |
| macOS keychain: `aberp.smtp.prod` / `smtp_password` | SPA Tenant Settings → SMTP PUT (PR-92) | tenant-lifetime |
| macOS keychain: `aberp.nav.prod` / `session_token` | serve at boot | per-binary-build |

> **Inventory contract (ADR-0055):** any future PR that adds a new
> path under `~/.aberp/<tenant>/`, a new DuckDB table, a new keychain
> entry, or a new side-store directory MUST add a row to the table
> above AND extend `tools/snapshot-prod.sh`'s docstring `What it
> captures:` section in the same PR. The inventory is the operator's
> mental model; a missing row is a silent recovery gap.

**Backups:** the DuckDB file IS the database (carries `invoice` +
`ap_invoice` + `restored_invoice` and every related table). Snapshot
it before any upgrade (Step 8/9 instructions). Side-store directories
(`~/.aberp/prod/invoices/<id>/` and `~/.aberp/prod/ap-artifacts/`) are
also load-bearing — `input.json` and the on-disk NAV XML files are
referenced by audit replay, the PDF print path, and the AP SPA's per-
row "XML" download button (S197). A backup strategy that covers
`~/.aberp/prod/` entirely is the simplest correct posture; the
`tools/snapshot-prod.sh` script implements it.

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
| Publish a release branch (from dev) | `./run/release.sh PROD_vX.Y[.Z]` |
| Clone a release on the prod machine | `git clone --branch PROD_vX.Y[.Z] <origin-url> ABERP-prod` |
| Upgrade an existing prod install to a new release | `./run/upgrade_prod.sh PROD_vX.Y[.Z]` |
| Launch the prod app | `./run/run_prod.sh` |
| Set up / rotate NAV creds | SPA NAV credentials wizard (boot route, S133) |
| Set up / rotate SMTP creds | SPA → Tenant Settings → SMTP → Test Connection → Save |
| Regenerate placeholder icons | `python3 tools/generate-icons.py` |
| Verify NAV creds are in keychain | `security find-generic-password -s "aberp.nav.prod" -a "nav_credentials_blob"` |
| Verify SMTP creds are in keychain | `security find-generic-password -s "aberp.smtp.prod" -a "smtp_password"` |
| Snapshot the DB + seller.toml + keychain (preferred) | `./tools/snapshot-prod.sh` |
| Snapshot the DB only (DuckDB single-file fallback) | `cp ~/.aberp/prod/aberp.duckdb ~/.aberp/prod/aberp.duckdb.snapshot-$(date +%Y%m%d-%H%M%S)` |
| See recent audit entries | (via the SPA's audit timeline on the invoice detail page) |
| Roll back to previous release | `cd ABERP-prod && git fetch && git checkout PROD_vX.Y-prev && ./run/run_prod.sh` |
| Re-trigger first-launch modal | `rm ~/.aberp/prod/.first-launch-acknowledged && ./run/run_prod.sh` |

---

*This runbook is the single source of truth for the prod cutover.*
*If you find a step that doesn't match reality, fix the runbook — it*
*is the artifact that survives across sessions.*
