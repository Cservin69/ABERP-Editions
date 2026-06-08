# End-to-end auto-quote test path

Local-loopback walkthrough for testing the storefront → ABERP
auto-quote pipeline on one laptop.

## Primary path: one command

```sh
./run/dev-test.sh
```

That's it. The script does all of the following so the operator
doesn't have to:

1. Provisions the email-relay token in the macOS keychain if missing
   (idempotent — first run shows a keychain-access popup, subsequent
   runs reuse the existing entry).
2. Pins ABERP to a stable port (`ABERP_HTTPS_PORT=18443`) so the
   storefront's sister-service URL doesn't shift on every restart.
3. Sets `ABERP_SISTER_SERVICE_BASE_URL=http://localhost:5173` so the
   catalogue-push + quote-intake daemons hit the local storefront,
   overriding any prod URL left in `seller.toml` from a prior session.
4. Sets `ABERP_DEV_MODE=1` so the S289 fail-loud prod-URL warning
   fires if anything ever drifts.
5. Launches ABERP via `run/run_desktop.sh` in the background and
   waits for `~/.aberp/<tenant>/runtime.json` to appear.
6. Reads the discovery file, exports the matching env vars to the
   storefront process (`ABERP_INTERNAL_BASE_URL`,
   `ABERP_INTERNAL_TLS_FINGERPRINT`, `ABERP_INTERNAL_BEARER`,
   `ABERP_TENANT`, `NODE_TLS_REJECT_UNAUTHORIZED=0`).
7. `npm install` (idempotent) + `npm run dev` in the sister
   `ABERP-site` repo (or `$ABERP_SITE_DIR` if elsewhere).
8. Opens `http://localhost:5173/quote` in the default browser.
9. Ctrl-C → SIGTERM both processes → wait → SIGKILL stragglers →
   clean exit.

### Flags

| Flag                 | Default     | Purpose                                   |
|----------------------|-------------|-------------------------------------------|
| `--tenant <name>`    | `test`      | Tenant id (refuses `prod`).               |
| `--port <number>`    | `18443`     | ABERP loopback port (1024–65535).         |
| `--no-browser`       | (open)      | Skip the auto-open of `/quote`.           |
| `ABERP_SITE_DIR=...` | sibling dir | Storefront repo location override.        |

## What the operator no longer has to do

Pre-S291 the same setup required FIVE manual steps before every test
relaunch (the gap the `feedback_local_dev_test_path_gaps` memory
documents from the 2026-06-08 evening test):

1. `lsof -iTCP:LISTEN | grep aberp` to find ABERP's dynamic port.
2. Open the SPA → Maintenance → Quote Intake → flip the URL.
3. `openssl rand -hex 32` + `security add-generic-password ...` for
   the email-relay token.
4. Set five env vars on the storefront launch.
5. Restart ABERP whenever the storefront port reassigned.

Per CLAUDE.md rule 12 ([[trust-code-not-operator]]) safety belongs in
code, not in operator memory. `dev-test.sh` is the code that closes
that gap.

## Advanced: manual setup

If you need the underlying steps for a non-laptop environment (CI,
remote dev box, debugging the launcher itself), the equivalent shell
sequence is:

```sh
# 1. Mint + write keychain entry (only on first run).
TOKEN="$(openssl rand -hex 32)"
security add-generic-password \
    -s "aberp.email_relay.test" -a "email_relay_token" \
    -w "${TOKEN}" -U

# 2. Launch ABERP with the fixed port + URL + dev-mode pins.
export ABERP_TENANT=test
export ABERP_HTTPS_PORT=18443
export ABERP_SISTER_SERVICE_BASE_URL=http://localhost:5173
export ABERP_DEV_MODE=1
./run/run_desktop.sh --tenant test &
ABERP_PID=$!

# 3. Wait for the discovery file.
until [ -f ~/.aberp/test/runtime.json ]; do sleep 1; done

# 4. Read the discovery file + launch storefront.
export ABERP_INTERNAL_BASE_URL="$(jq -r .base_url ~/.aberp/test/runtime.json)"
export ABERP_INTERNAL_TLS_FINGERPRINT="$(jq -r .tls_fingerprint ~/.aberp/test/runtime.json)"
export ABERP_INTERNAL_BEARER="${TOKEN}"
export NODE_TLS_REJECT_UNAUTHORIZED=0
cd ../ABERP-site && npm install && npm run dev &
SITE_PID=$!

# 5. Cleanup on done.
trap 'kill $SITE_PID $ABERP_PID' EXIT
wait
```

The unified launcher does the same thing with idempotency, port-clash
detection, and graceful shutdown.

## Discovery file format

`~/.aberp/<tenant>/runtime.json` — written at boot, deleted on
graceful shutdown:

```json
{
  "base_url": "https://127.0.0.1:18443",
  "relay_token_keychain_service": "aberp.email_relay.test.email_relay_token",
  "started_at": "2026-06-08T20:00:00Z",
  "tenant": "test",
  "tls_fingerprint": "<64-hex>"
}
```

The bearer token is NOT in the discovery file — the file points at
the keychain entry by name. The storefront launcher reads the keychain
directly so the plaintext never lands on disk.

## Supported CAD formats

As of PR-273 (S292) the auto-quote pipeline accepts both `.stl` and
`.step`/`.stp` files end-to-end:

| Format    | Backend                                  | Notes                                          |
|-----------|------------------------------------------|------------------------------------------------|
| `.stl`    | `numpy-stl` (always installed)           | Triangle-soup; volume via signed-tetrahedra    |
| `.step`   | `cadquery-ocp` (optional `[step]` extra) | OCCT BREP; volume via `VolumeProperties_s`     |
| `.stp`    | same as `.step`                          | Same loader, alternate extension               |

The `[step]` extra is installed by `run/dev-test.sh` (and the
production installer). If the daemon is run in an environment without
cadquery-ocp the STEP path surfaces "STEP extraction not yet
implemented in this build" — the Rust-side FailureKind classifier
maps this to `Permanent` and the operator gets a clear SPA badge
asking them to run `pip install -e '.[step]'` in the
`python/aberp-cad-extract/.venv` before retry.

Multi-solid STEP files (assemblies) are rejected with a clear
"STEP file contains an assembly with N solids" error — same Permanent
verdict. Customers must trim assemblies to a single part at upload
time. IGES files remain unsupported.

## Non-goals

- **Cloudflare Tunnel**: out of scope here — that's a separate runbook
  for the cross-machine remote-test path. `dev-test.sh` is
  local-loopback only.
- **Production**: the launcher refuses `--tenant prod`. Use
  `run/run_prod.sh` for real launches.
