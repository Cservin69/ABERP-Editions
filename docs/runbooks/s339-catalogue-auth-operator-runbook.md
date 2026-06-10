# Operator runbook — storefront catalogue-push 403 "missing origin signature"

**Audience:** Ervin / prod operator. **Time to resolve:** ~5 min.
**Verified against:** `ABERP-site/src/hooks.server.ts` @ commit on main
(S339). This is code-traced truth, not theory.

---

## The one fact that explains everything

The string **`forbidden: missing origin signature`** is emitted by
**exactly one** place in the whole storefront — `hooks.server.ts:70-75`:

```js
const expected = env.CLOUDFRONT_SHARED_SECRET;   // reads process.env at RUNTIME
if (expected && expected.length > 0) {           // ← guard
    const presented = request.headers.get('x-cloudfront-secret') ?? '';
    if (!safeEqual(presented, expected)) {
        return new Response('forbidden: missing origin signature', { status: 403 });
    }
}
return resolve(event);                            // ← unset/empty falls through here
```

- If `CLOUDFRONT_SHARED_SECRET` is **empty or unset** → the guard is
  falsy → the block is **skipped** → **no 403 is possible**.
- Therefore: **if any request returns that 403, the var IS set in the
  running node process.** Full stop. The code cannot lie about this.
- It is read via `$env/dynamic/private`, i.e. **`process.env` at
  runtime** (NOT build-time-inlined, NOT a file). One env var, one
  header (`x-cloudfront-secret`). No OIDC, no secrets file, no alternate
  name.

### "But `grep CLOUDFRONT_SHARED_SECRET /etc/aberp-site/env` returned nothing"

That only proves it isn't **in that file**. `process.env` can be fed by
any of: a systemd `Environment=` line, an `EnvironmentFile=` pointing
elsewhere, a systemd **drop-in** (`/etc/systemd/system/<unit>.d/*.conf`),
`systemctl set-environment`, a PM2 ecosystem file, or a launch wrapper.
**Do not trust config files. Read the process.**

---

## STEP 1 — read what the process actually sees (authoritative)

```sh
# find the storefront node PID (try the likely unit names)
PID=$(systemctl show -p MainPID --value aberp-site 2>/dev/null)
[ -z "$PID" ] || [ "$PID" = "0" ] && PID=$(pgrep -f 'node.*build/index.js' | head -1)
echo "storefront PID = $PID"

# THE definitive check — what the running process has in its env:
sudo tr '\0' '\n' < /proc/$PID/environ | grep CLOUDFRONT_SHARED_SECRET
```

**Branch on the result:**

- **Prints `CLOUDFRONT_SHARED_SECRET=<value>`** → the gate is ON. The
  `<value>` is the secret. Go to STEP 2.
- **Prints nothing** → the gate is genuinely OFF in this process, which
  means the 403 did **not** come from this code path. Re-curl and
  capture the *exact* body + which port/URL you hit:
  ```sh
  curl -sS -o /dev/stderr -w '\nHTTP %{http_code}\n' http://127.0.0.1:3000/api/catalogue/materials -X PUT
  ```
  If the body is literally `forbidden: missing origin signature`, the
  process you curled is NOT the one whose `/proc` you read (wrong PID —
  re-run STEP 1 against the right one). If the body is different, you're
  chasing a different 403 (e.g. nginx) — paste it.

---

## STEP 2 — pick the fix

The catalogue-push **daemon** reaches the storefront via the **public
CloudFront URL** (same path the working email-outbox daemon uses);
CloudFront injects `x-cloudfront-secret` on origin requests, so the
daemon normally passes the gate without ABERP doing anything. **Your
direct-to-:3000 curl bypassed CloudFront — that is why it 403'd, and it
does not reflect the daemon.** So first:

### Fix 0 (most likely — verify, don't change anything)

In ABERP: **Maintenance → Quote Intake → Base URL** must be the public
CloudFront host (`https://abenerp.com`), NOT a Lightsail IP / localhost.
Then watch the **Material catalogue** tile (S339): it now shows the live
push outcome. If it reads **`Pushed to storefront ✓`**, you are done —
the 403 was only the manual curl. (After the S338 grade fix + this, the
daemon's path should be clean.)

If the tile shows **`Push failing`** AND the daemon's Base URL is already
the CloudFront URL, then CloudFront is not injecting the header on the
`/api/catalogue/*` path → use Fix A.

### Fix A — give ABERP the secret so it carries the header itself

Take the `<value>` from STEP 1 and provision it into the **ABERP
machine's** keychain (ABERP runs on macOS; the value comes from the Linux
box):

```sh
# on the ABERP (Mac) machine, replace <TENANT> and <value>:
security add-generic-password \
  -s "aberp.storefront.<TENANT>" \
  -a "storefront_origin_secret" \
  -w "<value>" -U
# then restart ABERP; boot log will read: origin_secret_present=true
```

ABERP then sends `X-CloudFront-Secret: <value>` on every catalogue push
(S339), so even a direct-origin hit passes the gate. Reversible: delete
the keychain entry to revert.

### Fix B — turn the gate OFF (only if you don't want CloudFront origin protection)

Find where it's set (STEP 1 told you the process has it; locate the
source: `systemctl cat aberp-site` shows `Environment=` /
`EnvironmentFile=` / drop-ins), remove that line, then:

```sh
sudo systemctl daemon-reload
sudo systemctl restart aberp-site
# confirm it's now gone from the process:
sudo tr '\0' '\n' < /proc/$(systemctl show -p MainPID --value aberp-site)/environ | grep CLOUDFRONT_SHARED_SECRET || echo "GATE OFF"
```

With it empty, `hooks.server.ts` skips the check → no more 403 from any
path. **Trade-off:** the storefront origin then accepts unsigned direct
hits (the gate exists to prove traffic came through CloudFront). Prefer
Fix A unless you have another origin protection (e.g. Lightsail firewall
restricting the origin to CloudFront IPs).

---

## Decision summary (hülye-biztos)

| Symptom | Meaning | Action |
|---|---|---|
| `/proc/$PID/environ` shows the var | gate is ON | Fix 0 first; if daemon still fails on CloudFront URL → Fix A or B |
| `/proc/$PID/environ` empty, but curl still 403s that string | wrong PID read | re-run STEP 1 against the real node PID |
| ABERP tile says `Pushed to storefront ✓` | daemon already works via CloudFront | nothing — the curl 403 was a red herring |

The catalogue 403 is **not** an HMAC/signing problem — it's a single
static shared-secret header. The audit-ledger ART crash is a separate,
still-open issue and is unrelated to this.
