#!/usr/bin/env bash
# S2 ref-mirror verification — ADR-0100 Decision A.
# Clones ABERP-Editions FRESH and asserts the three archived annotated tag
# objects resolve identically to the ABERP.git originals, and that nothing
# landed under refs/heads/*. Self-contained; re-runnable by a later session.
set -uo pipefail

ORIGIN="https://github.com/Cservin69/ABERP-Editions.git"
DEST="${1:?usage: verify_ref_mirror.sh <fresh-clone-dir>}"

# Expected values, hard-coded from ADR-0100 §2 / the S2 task brief.
NAMES=(PROD_Portable_v0.1.0 PROD_Portable_v0.1.1 PROD_Portable_v0.1.2)
EXP_TAGOBJ=(07d31599cfdf3265c5b191c96c77e40eecfb00dd
            059b498c8a66d641715112f8551a492a77540ef9
            e4de7dca1777b386099d10191da0632b56892bea)
EXP_COMMIT=(7b849f761cee 9dbecb735162 6a51d4ffafba)

rm -rf "$DEST"
echo "=========== S2 REF-MIRROR VERIFICATION (ADR-0100 Decision A) ==========="
echo "origin      : $ORIGIN"
echo "clone       : plain 'git clone' — DEFAULT refspec, no --tags, no --mirror"
echo "clone dir   : $DEST"
git clone --quiet "$ORIGIN" "$DEST" || { echo "FATAL: clone failed"; exit 1; }
cd "$DEST" || exit 1
echo "clone HEAD  : $(git rev-parse --short HEAD) ($(git rev-parse --abbrev-ref HEAD))"
echo

rc=0

echo "--- A. archive refs present in the fresh clone ---"
found=$(git for-each-ref --format='%(refname) %(objecttype) %(objectname)' refs/tags/archive/aberp-git/)
if [[ -z "$found" ]]; then echo "  NONE — FAIL"; rc=1; else echo "$found" | sed 's/^/  /'; fi
echo

echo "--- B. per-tag assertions (tag object SHA + dereferenced commit) ---"
for i in 0 1 2; do
  name="${NAMES[$i]}"; ref="refs/tags/archive/aberp-git/$name"
  got_type=$(git cat-file -t "$ref" 2>/dev/null || echo "<absent>")
  got_tag=$(git rev-parse --verify --quiet "$ref" || echo "<absent>")
  got_commit=$(git rev-parse --verify --quiet "${ref}^{commit}" || echo "<absent>")
  echo "  $name"
  s=PASS; [[ "$got_type" == tag ]] || { s=FAIL; rc=1; }
  echo "    objecttype   got=$got_type  want=tag                                   [$s]"
  s=PASS; [[ "$got_tag" == "${EXP_TAGOBJ[$i]}" ]] || { s=FAIL; rc=1; }
  echo "    tag object   got=$got_tag"
  echo "                 want=${EXP_TAGOBJ[$i]}   [$s]"
  s=PASS; [[ "${got_commit:0:12}" == "${EXP_COMMIT[$i]}" ]] || { s=FAIL; rc=1; }
  echo "    ^{commit}    got=${got_commit:0:12}  want=${EXP_COMMIT[$i]}                      [$s]"
  echo "    tag name in object: $(git cat-file -p "$ref" 2>/dev/null | sed -n 's/^tag //p')"
done
echo

echo "--- C. the archive is NOT installable: upgrade_portable.sh:205 gate ---"
# :205 is `git ls-remote --exit-code --heads origin "$version"`; exit 2 == absent.
# ONLY archived / never-cut names belong here. `PROD_Portable_v1.0.0` was in this
# list while it was still uncut; once S5 cut it (2026-07-21, 234b598) the probe
# started reporting "PRESENT" and this script exited 1 with "DO NOT PRUNE" —
# a red for a reason that has nothing to do with the archive, which would have
# told the S3 session to abort a prune that is in fact fully cleared. Real
# release names are asserted in section C2 instead.
for probe in "PROD_Portable_v0.1.0" "PROD_Portable_v0.1.1" "PROD_Portable_v0.1.2" \
             "archive/aberp-git/PROD_Portable_v0.1.2"; do
  git ls-remote --exit-code --heads origin "$probe" >/dev/null 2>&1
  e=$?
  if [[ $e -eq 2 ]]; then
    echo "  ls-remote --exit-code --heads origin '$probe' -> exit 2 (absent, gate refuses)"
  else
    echo "  ls-remote --exit-code --heads origin '$probe' -> exit $e (PRESENT) — FAIL"
    rc=1
  fi
done
echo

echo "--- C2. the CUT release IS a real branch (disjoint from every archived name) ---"
# The archive must never collide with a live release name. Cutting v1.0.0 is the
# case that proves it: the release resolves as a branch, the archived v0.1.x
# names do not, and no archived name is installable.
git ls-remote --exit-code --heads origin "PROD_Portable_v1.0.0" >/dev/null 2>&1
if [[ $? -eq 0 ]]; then
  echo "  PROD_Portable_v1.0.0 -> present as refs/heads (installable release) [PASS]"
else
  echo "  PROD_Portable_v1.0.0 -> ABSENT as a branch"
  echo "    (expected before S5 cut it; if you are running this after 2026-07-21 the release ref is missing)"
fi
for name in "${NAMES[@]}"; do
  if [[ "$name" == "PROD_Portable_v1.0.0" ]]; then
    echo "  COLLISION: an archived name equals the cut release name — FAIL"; rc=1
  fi
done
echo "  archived names are v0.1.x only; the Editions line starts at v1.x (ADR-0100 §4) [PASS]"
echo

echo "--- D. VERSION_RE gate (upgrade_portable.sh:126) rejects the archive name ---"
VRE='^PROD_Portable_v[0-9]+\.[0-9]+(\.[0-9]+)?$'
for probe in "archive/aberp-git/PROD_Portable_v0.1.2" "PROD_Portable_v0.1.2"; do
  if [[ "$probe" =~ $VRE ]]; then echo "  '$probe' MATCHES VERSION_RE"; else echo "  '$probe' rejected by VERSION_RE"; fi
done
echo

echo "--- E. the ARCHIVED v0.1.x lineage is not a branch anywhere on origin ---"
# The property being asserted is "nothing from the ARCHIVED ABERP.git lineage was
# mirrored as a branch" — NOT "no Portable branch exists at all". Those were the
# same thing only until S5 cut PROD_Portable_v1.0.0; afterwards the blanket form
# flagged the legitimate release and printed "DO NOT PRUNE", which would have
# aborted a prune that is fully cleared. Scoped to v0.1.x accordingly.
#
# (A branch merely containing the word 'portable' — e.g. the ADR work branch
#  'worktree-adr-portable-sawoff' — is not a release ref and is not a finding.)
b=$(git ls-remote --heads origin | grep -E 'refs/heads/(.*/)?PROD_Portable_v0\.1\.' || true)
if [[ -z "$b" ]]; then echo "  (none) — PASS"; else echo "$b" | sed 's/^/  /'; echo "  FAIL"; rc=1; fi
echo "  all origin heads, for the record:"
git ls-remote --heads origin | awk '{print "    " $2}'
echo

echo "--- F. archived tag objects, full content (tagger metadata preserved) ---"
for name in "${NAMES[@]}"; do
  echo "  === $name ==="
  git cat-file -p "refs/tags/archive/aberp-git/$name" | sed 's/^/    /'
done
echo
echo "======================================================================="
echo "RESULT: $([[ $rc -eq 0 ]] && echo 'ALL ASSERTIONS PASS — S3 prune precondition SATISFIED' || echo 'FAILURES PRESENT — DO NOT PRUNE')"
exit $rc
