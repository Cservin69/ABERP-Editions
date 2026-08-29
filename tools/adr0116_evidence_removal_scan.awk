# ADR-0116 D2 — tenant-home removal scanner (cut-gate CHECK 11).
#
# Emits one record per filesystem-removal site in a Rust source file:
#
#   <file>:<fn>:<VERDICT>:<token>@L<n>
#
# VERDICTs:
#   GUARDED       the enclosing fn routes its removals through the ADR-0116 D2
#                 guard (`is_protected_evidence` / `guarded_remove` appear in
#                 the same fn body) AND no guard site is short-circuited dead.
#   DEAD_GUARD    the guard token is PRESENT but neutered — short-circuited by
#                 a boolean literal (`if false && is_protected_evidence(..)`,
#                 `if guard(..) || true`, …) or its result discarded
#                 (`let _ = is_protected_evidence(..)`). **Always a failure.**
#                 See "Why GUARDED is not token presence" below.
#   TENANT_HOME   the fn removes a path AND reaches a tenant-home path (it
#                 mentions a tenant-home token — `read_dir` included, because
#                 ENUMERATE-AND-UNLINK is the dangerous shape whatever the
#                 directory), with NO guard. **This is the
#                 class the gate freezes.** An unguarded unlink beside the live
#                 DB is permanent loss of the only record of a durability
#                 incident, and the ADR's own example — a "clean up the tenant
#                 home" helper meeting no guard — already existed in this tree
#                 (`recover::cleanup_siblings_with_infix`).
#   OTHER         a removal in a fn with no tenant-home reach — staging files,
#                 export dirs, temp scratch. Reported for corpus liveness only.
#
# ## Why a fn-scoped model, stated so nobody mistakes it for more
#
# The risk the ADR names is a helper that enumerates a tenant directory and
# unlinks by name or prefix. That shape puts the tenant-home token and the
# removal in the SAME fn body, which is what this can see. It is NOT a
# whole-program taint closure (CHECK 10N is that, for a different class), and
# it does not pretend to be. The gate carries a liveness floor so a scanner
# that stops classifying cannot read as "no violations".
#
# ## What is OUT OF MODEL, stated so a green is not read as more than it is
#
# The rev-3 adversarial planted nine mutations against this scanner. Seven are
# caught here or by the behavioural pins; **two walk past it, deliberately
# left**:
#
#   MF  an ALIASED import — `use std::fs::remove_file as rm;` … `rm(p)`.
#       Closing it needs a per-file alias table (capture the `use … as X;`
#       binding, then add X to the matcher), not a wider regex. A regex wide
#       enough to catch it without the binding would fire on every one-letter
#       call in the tree.
#   MH  destruction by TRUNCATION — `std::fs::write(p, b"")` or
#       `File::create(p)` over an existing artefact. This is a different verb,
#       not a different spelling of removal: adding `fs::write` / `File::create`
#       to a REMOVAL matcher would classify every legitimate write in every
#       tenant-home helper, flood the frozen manifest, and get the check
#       switched off — the failure mode the `self.remove_file()` probe below
#       exists to prevent.
#
# Neither is a hole in the CONTRACT, because the contract is carried in two
# layers and this scanner is only one of them. Both mutations are killed by
# `f7_prune_refuses_a_protected_directory_and_does_not_report_it_removed`, and
# the same neutering inside `guarded_remove` is killed by
# `ac6_guarded_remove_refuses_evidence_and_permits_a_live_transient`. Those two
# tests cover BOTH guarded functions in the tree. What a green CHECK 11 means
# is therefore precise: *no NEW removal site, spelled the way removals are
# spelled in this tree, reaches a tenant home unguarded* — not *no code can
# ever destroy evidence*. Filed in SAW-OFF.md rather than half-closed here.
#
# ## Why GUARDED is not token presence (the F7 / M1 finding)
#
# The first cut of CHECK 11d grepped `retention.rs` for the string
# `is_protected_evidence` — and the function's own DOC COMMENT names it, so
# neutering the real call left the gate green. That was closed by keying the
# check on THIS scanner's verdict instead, since the scanner strips comments
# and string literals.
#
# It was closed one level, not all the way. The verdict was still "the token
# appears somewhere in the fn body", which is presence, not reachability, and
# the adversarial mutation
#
#     if false && crate::evidence::is_protected_evidence(&rec.dir) {
#
# passed the whole gate with the guard dead. That is the ADR-0098 opener-scan
# char-literal class one level in: the first cut could be flipped by editing a
# COMMENT, this one by editing an OPERATOR.
#
# A scanner cannot decide reachability in general, and this one does not
# pretend to. What it CAN decide is whether a guard has been written in a form
# that is dead BY CONSTRUCTION — a constant-false short-circuit, a constant-true
# disjunction, or a discarded result — which is the shape a neutering edit
# actually takes. Those are now `DEAD_GUARD` and fail the build. The general
# question is answered where it belongs, by a behavioural test:
# `f7_prune_refuses_a_protected_directory_and_does_not_report_it_removed`.
#
# ## Structure detection is INDENT-based, not brace-counting
#
# The first cut counted braces and drifted: a `'{'` char literal or a raw
# string inside a 36k-line file desynchronised the depth, which silently ended
# the `#[cfg(test)] mod tests` block early and leaked two test fns into the
# TENANT_HOME class. That is the same failure mode as the ADR-0098 opener-scan
# char-literal bug already recorded in this repo — a scanner whose verdict can
# be flipped by editing an unrelated literal.
#
# `cargo fmt --all -- --check` is a REQUIRED CI step here, so rustfmt's layout
# is guaranteed: an item declared at indent N is closed by the first line whose
# stripped content is exactly `}` (or `};`) at indent N. Matching on that is
# both simpler and immune to literals.

function indent_of(line,   i, c, n) {
  n = 0
  for (i = 1; i <= length(line); i++) {
    c = substr(line, i, 1)
    if (c == " ") n++
    else if (c == "\t") n += 4
    else break
  }
  return n
}

function strip(line,   out, i, c, instr, inchar, prev) {
  out = ""; instr = 0; inchar = 0; prev = ""
  for (i = 1; i <= length(line); i++) {
    c = substr(line, i, 1)
    if (instr) {
      if (c == "\"" && prev != "\\") instr = 0
      prev = (c == "\\" && prev == "\\") ? "" : c
      continue
    }
    if (inchar) {
      if (c == "'" && prev != "\\") inchar = 0
      prev = (c == "\\" && prev == "\\") ? "" : c
      continue
    }
    if (c == "\"") { instr = 1; prev = c; continue }
    if (c == "/" && substr(line, i + 1, 1) == "/") break
    out = out c
    prev = c
  }
  return out
}

BEGIN {
  # Tokens that mean "this fn can reach a path inside a tenant home". Drawn
  # from the shapes that actually occur at removal sites in this tree.
  nth = split("db_path|mirror_path|tenant_home|tenant_dir|wal_sibling|marker_path|install_intent_path|.aberp|audit.log|CORRUPT|preserve_|evidence|sibling|live_db|.wal|read_dir|artefact", TH, "|")
  ng  = split("is_protected_evidence|guarded_remove", GUARD, "|")
  # The PREDICATE half of the guard set. `is_protected_evidence` RETURNS the
  # decision, so discarding its value neuters it; `guarded_remove` PERFORMS the
  # guarded action and returns a Result, so `let _ = guarded_remove(..)` is
  # idiomatic and completely safe — the removal still went through the guard.
  # `recover::cleanup_siblings_with_infix` spells it exactly that way today.
  # Conflating the two would fire on correct code, and a gate that reddens the
  # correct fix is a gate that gets switched off.
  npd = split("is_protected_evidence", PRED, "|")
  # F8 — files whose EVERY removal reaches a tenant home BY CONSTRUCTION,
  # whatever tokens the individual fn happens to mention.
  #
  # `evidence.rs::archive_then_remove` is the sanctioned release path: it
  # unlinks recovery evidence from a live tenant home by design, and the
  # token-based classifier called it OTHER — "a removal in a fn with no
  # tenant-home reach" — because it works through `artefact.path` and `dest`
  # and mentions none of the TH tokens. So the ONE function whose job is
  # unlinking evidence sat OUTSIDE the frozen may-only-shrink set, and a later
  # change that weakened it, or a new removal added inside it, would have kept
  # CHECK 11 green. These three files ARE the tenant-home surface; classifying
  # them by file removes the dependence on a fn happening to name the right
  # local.
  nfh = split("crates/aberp-snapshot/src/evidence.rs|crates/aberp-snapshot/src/recover.rs|crates/aberp-snapshot/src/crash_safe.rs", FH, "|")
}

function file_reaches_tenant_home(f,   i) {
  for (i = 1; i <= nfh; i++) if (index(f, FH[i]) > 0) return 1
  return 0
}

# Is this statement a guard call written in a form that is DEAD by
# construction? Evaluated over a whole statement, so a rustfmt line split
# inside an `if` condition cannot hide the operator.
function guard_is_dead(st,   i, has) {
  has = 0
  for (i = 1; i <= ng; i++) if (index(st, GUARD[i]) > 0) has = 1
  if (!has) return 0
  # `false &&` / `&& false` — a conjunction that can never be true.
  if (st ~ /(^|[^A-Za-z_0-9])false[ \t]*&&/) return 1
  if (st ~ /&&[ \t]*false([^A-Za-z_0-9]|$)/) return 1
  # `true ||` / `|| true` — a disjunction that is always true, so the guard's
  # answer never changes the branch.
  if (st ~ /(^|[^A-Za-z_0-9])true[ \t]*\|\|/) return 1
  if (st ~ /\|\|[ \t]*true([^A-Za-z_0-9]|$)/) return 1
  # The PREDICATE's answer thrown away: `let _ = is_protected_evidence(..)`.
  for (i = 1; i <= npd; i++)
    if (index(st, PRED[i]) > 0 && st ~ /let[ \t]+_[ \t]*=/) return 1
  return 0
}

FNR == 1 {
  file = FILENAME
  fn_name = ""; fn_indent = -1; body = ""; hits = ""; in_test_fn = 0
  test_indent = -1; pending_test = 0
  inblock = 0
  stmt = ""; dead_guard = 0
}

{
  raw = $0
  line = strip(raw)
  ind = indent_of(raw)
  trimmed = line; gsub(/^[ \t]+|[ \t]+$/, "", trimmed)

  # /* */ block comments.
  if (inblock) {
    if (index(line, "*/") > 0) { line = substr(line, index(line, "*/") + 2) }
    else next
    inblock = 0
  }
  while (index(line, "/*") > 0) {
    pre = substr(line, 1, index(line, "/*") - 1)
    rest = substr(line, index(line, "/*") + 2)
    if (index(rest, "*/") > 0) { line = pre substr(rest, index(rest, "*/") + 2) }
    else { line = pre; inblock = 1; break }
  }

  # ── cfg(test) scope, closed by indentation ──────────────────────────
  if (test_indent >= 0 && ind <= test_indent && trimmed ~ /^\}[;,]?$/) {
    test_indent = -1
  }
  if (line ~ /#\[cfg\(test\)\]/) { pending_test = 1; pending_indent = ind; next }
  if (pending_test) {
    # The attributed item opens here. Everything until its closing brace at
    # the same indent is test code.
    if (test_indent < 0) test_indent = pending_indent
    pending_test = 0
  }
  # A `mod tests`/`mod *_tests` block with no attribute is still test code.
  if (test_indent < 0 && trimmed ~ /^(pub[ \t]+)?mod[ \t]+[A-Za-z_0-9]*tests?[ \t]*\{/) {
    test_indent = ind
  }

  # ── fn scope, closed by indentation ─────────────────────────────────
  if (fn_name != "" && ind <= fn_indent && trimmed ~ /^\}[;,]?$/) {
    emit()
    fn_name = ""; fn_indent = -1
  }
  if (line ~ /(^|[^A-Za-z_0-9])fn[ \t]+[A-Za-z_][A-Za-z_0-9]*[ \t]*[(<]/) {
    if (fn_name != "") emit()
    m = line
    sub(/.*[^A-Za-z_0-9]fn[ \t]+/, "", m); sub(/^[ \t]*fn[ \t]+/, "", m)
    sub(/[ \t]*[(<].*$/, "", m)
    fn_name = m
    fn_indent = ind
    body = ""; hits = ""; stmt = ""; dead_guard = 0
    in_test_fn = (test_indent >= 0)
  }

  if (fn_name != "") {
    body = body " " line
    # Statement buffer for the DEAD_GUARD test. Reset at a statement/block
    # boundary, so a guard call split across lines by rustfmt is still
    # evaluated together with the operator that neuters it.
    stmt = stmt " " line
    if (line ~ /[;{}]/) {
      if (guard_is_dead(stmt)) dead_guard = 1
      stmt = ""
    }
    # F8 (mutation M5) — the removal must be recognised in EVERY spelling, not
    # only `fs::remove_*`. A direct import (`use std::fs::remove_file;` then a
    # bare `remove_file(p)`) escaped the old `fs::remove_(` regex entirely, so
    # a tenant-home sweeper written that way passed the whole gate. Nothing
    # in-tree spells it that way today — which is exactly when to close it;
    # this is the "gate bans ONE spelling" class already on record here from
    # PR #41. `[^A-Za-z_0-9.]` keeps `self.remove_file(` and
    # `guarded_remove_file(` out, and the `fn` guard keeps DEFINITIONS out.
    if (line ~ /(^|[^A-Za-z_0-9.])remove_(file|dir_all|dir)[ \t]*\(/ \
        && line !~ /(^|[^A-Za-z_0-9])fn[ \t]+remove_/) {
      tok = "remove_file"
      if (line ~ /remove_dir_all/) tok = "remove_dir_all"
      else if (line ~ /remove_dir[ \t]*\(/) tok = "remove_dir"
      hits = hits " " tok "@L" FNR
    }
  }
}

END { if (fn_name != "") emit() }

function emit(   i, guarded, reaches, verdict, n, parts) {
  # A trailing statement with no terminator (the last line of a fn body) still
  # has to be tested before the verdict is taken.
  if (stmt != "" && guard_is_dead(stmt)) dead_guard = 1
  stmt = ""
  if (hits == "" || in_test_fn) { body = ""; hits = ""; dead_guard = 0; return }
  guarded = 0
  for (i = 1; i <= ng; i++) if (index(body, GUARD[i]) > 0) guarded = 1
  reaches = file_reaches_tenant_home(file)
  if (!reaches) for (i = 1; i <= nth; i++) if (index(body, TH[i]) > 0) reaches = 1
  # DEAD_GUARD outranks everything: a guard written dead by construction is
  # worse than no guard, because its presence is what stops anyone looking.
  verdict = dead_guard ? "DEAD_GUARD" : (guarded ? "GUARDED" : (reaches ? "TENANT_HOME" : "OTHER"))
  n = split(hits, parts, " ")
  for (i = 1; i <= n; i++) {
    if (parts[i] == "") continue
    print file ":" fn_name ":" verdict ":" parts[i]
  }
  body = ""; hits = ""; dead_guard = 0
}
