# FINDING (stop-the-line) — `ABERP_DB` escapes the ADR-0093 edition-isolation guard via a symlink

- **Date:** 2026-07-21
- **Found in:** S2 of the ADR-0100 Portable saw-off, as the pre-cut assertion
  required before `PROD_Portable_v1.0.0`.
- **Consequence at discovery:** `PROD_Portable_v1.0.0` was held.
- **Consequence now (updated 2026-07-21):** the gap was assigned to a parallel
  session porting `ensure_db_path_isolated` into `ABERP.git` and canonicalizing
  both sides. An owned residual is a different risk posture from an unowned
  defect, so the hold was lifted and **`PROD_Portable_v1.0.0` was cut at
  `234b598`, shipping with this gap open and documented** — see ADR-0100 §12.3
  for the exact containment behaviour the release carries.
- **Severity:** the guard this defeats is the one ADR-0093 and ADR-0100 §5 both
  name as the reason `ABERP_DB` is safe to expose at all.
- **Status:** OPEN, owned by the parallel canonicalization work. Not fixed here:
  this session was explicitly told not to fix it, to avoid colliding with that
  branch. Remediation options are in §5; option 1 is the recommendation.

## 1. What was asserted, and what is actually true

The S2 brief required proving that `ABERP_DB` *cannot redirect a Portable
build's data root outside `~/.aberp-portable/`*. Two separate things turned up:

| Claim | Result |
| --- | --- |
| `edition_data_dirname()` resolves to `.aberp-portable` | **holds** (compile-time `const`) |
| `ABERP_TENANT=prod` exits non-zero | **holds** (`guard_tenant_matches_build`, exit 1) |
| A *literal* path into `~/.aberp/` is refused | **holds** (exit 1, before any I/O) |
| `ABERP_DB` cannot redirect the root outside `~/.aberp-portable/` | **false — by design** (§2) |
| `ABERP_DB` cannot reach `~/.aberp/prod` | **FALSE — via a symlink** (§3) |

## 2. The guard is a foreign-root denylist, not a containment allowlist

`tenant_registry::ensure_db_path_isolated` (`apps/aberp/src/tenant_registry.rs:694`)
walks `path.components()` and errors only if some component's **name** equals
one of `build_profile::foreign_data_dirnames()` — for a Portable build,
`.aberp` and `.aberp-defense`.

It therefore permits any path that merely isn't *named* like a foreign root.
`./aberp.duckdb` and `/tmp/whatever/aberp.duckdb` are both accepted, and this is
**intentional and test-pinned** — `apps/aberp/tests/edition_db_isolation.rs:87-88`
asserts exactly those two are `is_ok()`. The default `--db` value is itself the
relative `./aberp.duckdb`.

So "the root cannot leave `~/.aberp-portable/`" was never the property on offer.
The property on offer is narrower: *a foreign edition's root cannot be opened*.
That narrower property is the one §3 breaks.

## 3. The defect: the component scan is lexical, so a symlink walks straight through

`ensure_db_path_isolated` does **no** `canonicalize()`, and neither does any
caller — `serve.rs:1078` passes `args.db` to the guard and then hands the *same
unresolved* `args.db` to `Connection::open` (`serve.rs:1486` and ~20 further
sites). The doc comment on `guard_db_matches_edition` (`serve.rs:280-287`)
promises refusal "no matter how the path arrived (`--db`, `ABERP_DB`, a
hand-edited launcher, a switch hint)" and says the path "**resolves** into a
FOREIGN edition's root". It never resolves anything.

### Reproduction (PORTABLE build, `cargo build --bin aberp`, default features)

```
tmp=$(mktemp -d); mkdir -p "$tmp/.aberp/prod"; ln -s "$tmp/.aberp" "$tmp/sneaky"
duckdb "$tmp/.aberp/prod/aberp.duckdb" -c "CREATE TABLE prod_secret(x INTEGER);"

# A) literal path — REFUSED
./target/debug/aberp snapshot now --db "$tmp/.aberp/prod/aberp.duckdb"
#   Error: ... is under the FROZEN prod DB root ~/.aberp/ — an editions build
#   must never read, snapshot, or restore the prod line.

# B) THE SAME FILE through the symlink — ACCEPTED
./target/debug/aberp snapshot now --db "$tmp/sneaky/prod/aberp.duckdb"
#   ...opens it, reads it, and takes a snapshot.
```

After (B):

```
$tmp/.aberp/prod/aberp.duckdb
$tmp/.aberp/prod/aberp.duckdb.wal      <-- the Portable build WROTE here
```

The Portable build **read the database inside the forbidden root and created a
WAL file in it** — the forbidden root was mutated, not merely read. It also
exported the contents to `~/Documents/ABERP-snapshots-portable/`. (Both probe
artifacts were removed afterwards; `~/Documents/ABERP-snapshots-portable/` did
not exist before the probe and does not exist now.)

The same bypass was confirmed on the `serve` boot path: with `--db` pointing
through the symlink, `guard_db_matches_edition` stayed silent and boot proceeded
past it through "resolving tenant id" into the keychain step. Boot only stopped
there because the probe ran under a synthetic `$HOME` with no macOS keychain —
an artifact of the probe sandbox, **not** a guard.

### Safety note on how this was tested

Every probe used a synthetic `$HOME` or a `mktemp -d` tree. The real
`~/.aberp/prod` (the live frozen HU prod store, present on this machine) was
never passed to the binary. The guard is a pure lexical scan that never reads
`$HOME`, so the synthetic tree exercises the identical code path.

## 4. Why this matters more than a normal path-validation bug

ADR-0100 §5 ("Correction to a prior finding: `ABERP_DB` is **not** vestigial")
argued — correctly — that `ABERP_DB` is a live, operator-reachable input to the
data root, and concluded that this is *fine* because
`guard_tenant_matches_build` and the foreign-root refusal "are load-bearing
against an actual attack surface". That conclusion rests on the foreign-root
refusal actually holding. For symlinked paths it does not.

The exposure is bounded in practice: reaching prod requires a symlink to already
exist that points into `~/.aberp/`, so this is not a bare `ABERP_DB=~/.aberp/prod`
one-liner. But "the binary physically cannot open another edition's DB"
(FOUNDATION §5, ADR-0093) is stated as an absolute, and it is not one.

## 5. Remediation options (not chosen here)

1. **Canonicalize before scanning.** In `ensure_db_path_isolated`, resolve the
   path (and its nearest existing ancestor, since the DB file often does not
   exist yet) before walking components. Closes the symlink vector; keeps the
   denylist shape and every existing test.
2. **Invert to an allowlist.** Require the path to be inside
   `$HOME/<edition_data_dirname()>/`. Genuinely delivers "the root cannot leave
   `~/.aberp-portable/`" — but it *breaks* the two intentional allowances pinned
   at `edition_db_isolation.rs:87-88` (`./aberp.duckdb`, `/tmp/...`), which the
   whole test suite and the default `--db` value depend on. Not a drop-in.
3. **Both:** allowlist for real deployments, escape hatch behind an explicit
   dev-only env var.

Option 1 is the smallest change that makes the documented promise true, and is
the recommendation. It is a change to a load-bearing security guard, so it wants
its own step with its own gates — which is why S2 did not take it.

A regression test asserting the *desired* behaviour is committed alongside this
document, `#[ignore]`d with a pointer here, so the fix has a ready-made proof and
the gap is discoverable from the test suite rather than only from this file.
