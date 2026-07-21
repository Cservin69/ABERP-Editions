# S2 ref-mirror verification transcript — ADR-0100 Decision A

- **Date:** 2026-07-21
- **Purpose:** this is the hard precondition **S3's prune of the six Portable
  refs in `ABERP.git` depends on.** It exists so a later session can trust the
  archive without re-deriving it.
- **Reproduce:** `./tools/verify_ref_mirror.sh /tmp/verify-clone` (exit 0 = pass).

## Why a fresh clone

Pushing is not proof. `refs/archive/*` was rejected in ADR-0100 §3 precisely
because a ref can exist on the origin and still be invisible to every clone —
git's default refspec covers `refs/heads/*` and `refs/tags/*` only. The property
being asserted is therefore *"a plain `git clone` carries the archive"*, and the
only way to assert it is to make a plain clone and look.

The clone below was made with `git clone <url> <dir>` — default refspec, no
`--tags`, no `--mirror`, no custom refspec.

## What is asserted

1. All three archive refs are present in the clone, type `tag`.
2. Each tag-object SHA is **identical** to the `ABERP.git` original.
3. Each `^{commit}` is the ADR-0100 §2 branch tip.
4. `upgrade_portable.sh:205`'s `git ls-remote --exit-code --heads` returns **2**
   (absent) for the bare release names, the archive path, and the not-yet-cut
   `PROD_Portable_v1.0.0`.
5. `upgrade_portable.sh:126`'s `VERSION_RE` rejects the archive name.
6. **No `refs/heads/**/PROD_Portable_v*` exists on origin** — nothing was
   mirrored as a branch.

Expected values are hard-coded in the script from ADR-0100 §2, so the test
compares against the ADR rather than against whatever happens to be on origin.

## Transcript

```
=========== S2 REF-MIRROR VERIFICATION (ADR-0100 Decision A) ===========
origin      : https://github.com/Cservin69/ABERP-Editions.git
clone       : plain 'git clone' — DEFAULT refspec, no --tags, no --mirror
clone dir   : /private/tmp/claude-501/-Users-aben-Documents-Claude-Projects-ABERP-Editions/43257c74-d162-4fb5-b2bc-d9eb0b6a4d7f/scratchpad/verify-clone
clone HEAD  : 7520ed2 (main)

--- A. archive refs present in the fresh clone ---
  refs/tags/archive/aberp-git/PROD_Portable_v0.1.0 tag 07d31599cfdf3265c5b191c96c77e40eecfb00dd
  refs/tags/archive/aberp-git/PROD_Portable_v0.1.1 tag 059b498c8a66d641715112f8551a492a77540ef9
  refs/tags/archive/aberp-git/PROD_Portable_v0.1.2 tag e4de7dca1777b386099d10191da0632b56892bea

--- B. per-tag assertions (tag object SHA + dereferenced commit) ---
  PROD_Portable_v0.1.0
    objecttype   got=tag  want=tag                                   [PASS]
    tag object   got=07d31599cfdf3265c5b191c96c77e40eecfb00dd
                 want=07d31599cfdf3265c5b191c96c77e40eecfb00dd   [PASS]
    ^{commit}    got=7b849f761cee  want=7b849f761cee                      [PASS]
    tag name in object: PROD_Portable_v0.1.0
  PROD_Portable_v0.1.1
    objecttype   got=tag  want=tag                                   [PASS]
    tag object   got=059b498c8a66d641715112f8551a492a77540ef9
                 want=059b498c8a66d641715112f8551a492a77540ef9   [PASS]
    ^{commit}    got=9dbecb735162  want=9dbecb735162                      [PASS]
    tag name in object: PROD_Portable_v0.1.1
  PROD_Portable_v0.1.2
    objecttype   got=tag  want=tag                                   [PASS]
    tag object   got=e4de7dca1777b386099d10191da0632b56892bea
                 want=e4de7dca1777b386099d10191da0632b56892bea   [PASS]
    ^{commit}    got=6a51d4ffafba  want=6a51d4ffafba                      [PASS]
    tag name in object: PROD_Portable_v0.1.2

--- C. the archive is NOT installable: upgrade_portable.sh:205 gate ---
  ls-remote --exit-code --heads origin 'PROD_Portable_v0.1.0' -> exit 2 (absent, gate refuses)
  ls-remote --exit-code --heads origin 'PROD_Portable_v0.1.2' -> exit 2 (absent, gate refuses)
  ls-remote --exit-code --heads origin 'archive/aberp-git/PROD_Portable_v0.1.2' -> exit 2 (absent, gate refuses)
  ls-remote --exit-code --heads origin 'PROD_Portable_v1.0.0' -> exit 2 (absent, gate refuses)

--- D. VERSION_RE gate (upgrade_portable.sh:126) rejects the archive name ---
  'archive/aberp-git/PROD_Portable_v0.1.2' rejected by VERSION_RE
  'PROD_Portable_v0.1.2' MATCHES VERSION_RE

--- E. no Portable RELEASE BRANCH anywhere on origin ---
  (none) — PASS
  all origin heads, for the record:
    refs/heads/PROD_Defense_v0.1.0
    refs/heads/PROD_Defense_v0.2.0
    refs/heads/PROD_Defense_v0.2.10
    refs/heads/PROD_Defense_v0.2.11
    refs/heads/PROD_Defense_v0.2.12
    refs/heads/PROD_Defense_v0.2.2
    refs/heads/PROD_Defense_v0.2.3
    refs/heads/PROD_Defense_v0.2.4
    refs/heads/PROD_Defense_v0.2.5
    refs/heads/PROD_Defense_v0.2.6
    refs/heads/PROD_Defense_v0.2.7
    refs/heads/PROD_Defense_v0.2.8
    refs/heads/PROD_Defense_v0.2.9
    refs/heads/main
    refs/heads/worktree-adr-portable-sawoff

--- F. archived tag objects, full content (tagger metadata preserved) ---
  === PROD_Portable_v0.1.0 ===
    object 7b849f761cee9f90a0de03ec6e667517c31819f3
    type commit
    tag PROD_Portable_v0.1.0
    tagger Ervin Aben <PASTE_NOREPLY_EMAIL_HERE> 1781601750 +0200
    
    PROD_Portable_v0.1.0 — first cut of the ABERP Portable line
    
    First release of the Portable product line, for international (non-Hungarian)
    operators who run ABERP without the Hungarian NAV fiscal integration.
    
    Contains:
    - S433: multi-tenant CRUD + tenant switcher + bundled demo tenant
    - S434: per-tenant NAV-off toggle (skip keychain/§169 gate at boot, NAV
      daemons skip-at-spawn, LocalOnly invoices, region-aware seller tax)
    
    Off main f7519b4 (= PROD_v2.27.76). Includes upgrade_prod.sh regex widening
    so `./run/upgrade_prod.sh PROD_Portable_v0.1.0` is accepted.
  === PROD_Portable_v0.1.1 ===
    object 9dbecb735162317cf0ca73d2cbf2f8568959d17a
    type commit
    tag PROD_Portable_v0.1.1
    tagger Ervin Aben <PASTE_NOREPLY_EMAIL_HERE> 1781606099 +0200
    
    PROD_Portable_v0.1.1 — Portable launcher pair (run_portable.sh + upgrade_portable.sh) + demo boot e2e
  === PROD_Portable_v0.1.2 ===
    object 6a51d4ffafba03b123f7693f8b7fc27f8e9fce4a
    type commit
    tag PROD_Portable_v0.1.2
    tagger Ervin Aben <PASTE_NOREPLY_EMAIL_HERE> 1781610122 +0200
    
    PROD_Portable_v0.1.2: exec-bit fix on run scripts (run_portable.sh, upgrade_portable.sh, dev-test.sh, upgrade_prod_running_check_test.sh); 0 content change

=======================================================================
RESULT: ALL ASSERTIONS PASS — S3 prune precondition SATISFIED
```

## Result

**All assertions pass (exit 0). S3's prune precondition is satisfied.**

The three annotated tag objects `07d3159` / `059b498` / `e4de7dc` — the only
genuinely GC-eligible objects in the six refs, per ADR-0100 §2 — now exist in
`ABERP-Editions` with identical SHAs and survive a plain clone. Deleting the six
refs in `ABERP.git` orphans nothing.

### Footnote on an earlier false failure

A first draft of the verifier grepped origin's heads for the case-insensitive
substring `portable`, which matches the ADR work branch
`worktree-adr-portable-sawoff` and produced a spurious check-E failure. The
committed script matches release-shaped names
(`refs/heads/(.*/)?PROD_Portable_v`) and additionally prints every origin head
verbatim, so the reader can confirm the negative themselves rather than trusting
a grep.
